//! Polls the disks under registered volumes without blocking admin requests.
//!
//! The service reads a bounded smartctl report from a checked device node.
//! A probe can report warnings or unavailable evidence. Its result is a
//! timestamped hardware observation, separate from filesystem and data checks.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use jumpstart_storage::{AdminMetadata, HardwareDevice, RegisteredHardwareTargets, StorageError};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command, sync::watch, task::JoinHandle, time::timeout};

pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_OUTPUT: u64 = 128 * 1024;
const DEVICE_LIMIT: usize = 32;

/// The most recent complete poll, aged using the daemon's monotonic clock.
struct Sample {
    recorded_at: Instant,
    report: Value,
}

/// Owns at most one polling task and keeps its last complete observation.
pub(super) struct Monitor {
    sample: watch::Sender<Option<Sample>>,
    task: Option<JoinHandle<()>>,
}

impl Monitor {
    /// Create an empty monitor for one administration daemon.
    pub(super) fn new() -> Self {
        Self {
            sample: watch::channel(None).0,
            task: None,
        }
    }

    /// Start a bounded background poll from the current metadata lease.
    pub(super) fn refresh(&mut self, metadata: &Result<AdminMetadata, StorageError>) {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let targets = metadata
            .as_ref()
            .map_err(ToString::to_string)
            .and_then(|lease| {
                lease
                    .registered_hardware_targets()
                    .map_err(|error| error.to_string())
            });
        let sample = self.sample.clone();
        self.task = Some(tokio::spawn(async move {
            let report = match targets {
                Ok(targets) => collect(targets).await,
                Err(detail) => json!({"status": "unknown", "detail": detail}),
            };
            sample.send_replace(Some(Sample {
                recorded_at: Instant::now(),
                report,
            }));
        }));
    }

    /// Return the cached report with its age or an explicit pending/unknown state.
    pub(super) fn report(&self) -> Value {
        let sample = self.sample.borrow();
        match sample.as_ref() {
            Some(sample) => {
                let mut report = sample.report.clone();
                report["observation_age_ms"] =
                    json!(sample.recorded_at.elapsed().as_millis().to_string());
                report
            }
            None => json!({"status": "unknown", "detail": "hardware poll has not completed"}),
        }
    }
}

impl Drop for Monitor {
    /// Cancel an in-progress probe when the daemon shuts down.
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Probe each distinct checked disk once and associate its evidence with volumes.
async fn collect(targets: RegisteredHardwareTargets) -> Value {
    let binary = smartctl_binary();
    let mut observations: BTreeMap<HardwareDevice, Value> = BTreeMap::new();
    let mut volumes = Vec::new();
    let mut completed = false;
    for volume in targets.volumes {
        let result = match volume.devices {
            Ok(devices) => {
                let mut evidence = Vec::new();
                let omitted_devices = devices.len().saturating_sub(DEVICE_LIMIT);
                for device in devices.into_iter().take(DEVICE_LIMIT) {
                    let status = if let Some(prior) = observations.get(&device) {
                        prior.clone()
                    } else if observations.len() >= DEVICE_LIMIT {
                        unknown("disk was omitted by the diagnostic device limit")
                    } else {
                        let value = match binary.as_deref() {
                            Some(path) => probe(path, &device).await,
                            None => unknown("a trusted system smartctl executable is unavailable"),
                        };
                        observations.insert(device.clone(), value.clone());
                        value
                    };
                    completed |= status["status"] != "unknown";
                    evidence.push(json!({"device": device.node(), "evidence": status}));
                }
                json!({"devices": evidence, "omitted_devices": omitted_devices.to_string()})
            }
            Err(error) => unknown(&error.to_string()),
        };
        volumes.push(json!({"volume_id": volume.volume_id, "observation": result}));
    }
    json!({
        "status": if completed { "observed" } else { "unknown" },
        "omitted_volumes": targets.omitted_volumes.to_string(),
        "volumes": volumes,
    })
}

/// Select a fixed system executable with root ownership and safe file permissions.
fn smartctl_binary() -> Option<PathBuf> {
    ["/usr/sbin/smartctl", "/usr/bin/smartctl"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| {
            fs::symlink_metadata(path)
                .is_ok_and(|entry| entry.is_file() && entry.uid() == 0 && entry.mode() & 0o022 == 0)
        })
}

/// Collect SMART/NVMe telemetry from one device with no self-test or configuration writes.
async fn probe(binary: &Path, device: &HardwareDevice) -> Value {
    if !device.still_current() {
        return unknown("the disk changed before the hardware poll");
    }
    let outcome = timeout(PROBE_TIMEOUT, read_probe(binary, device.node())).await;
    if !device.still_current() {
        return unknown("the disk changed during the hardware poll");
    }
    match outcome {
        Ok(Ok((code, bytes))) => parse_report(code, &bytes),
        Ok(Err(reason)) => unknown(reason),
        Err(_) => unknown("smartctl exceeded the hardware poll deadline"),
    }
}

/// Read at most the report limit and ensure dropped or oversized probes are stopped.
async fn read_probe(binary: &Path, device: &Path) -> Result<(i32, Vec<u8>), &'static str> {
    let mut child = Command::new(binary)
        .args(["-a", "-j=v", "--"])
        .arg(device)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "smartctl could not start")?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or("smartctl has no output pipe")?
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "smartctl output could not be read")?;
    if bytes.len() as u64 > MAX_OUTPUT {
        return Err("smartctl exceeded the report size limit");
    }
    let code = child
        .wait()
        .await
        .map_err(|_| "smartctl did not finish")?
        .code()
        .ok_or("smartctl was terminated")?;
    Ok((code, bytes))
}

/// Extract selected telemetry while preserving unknown and warning outcomes.
fn parse_report(code: i32, bytes: &[u8]) -> Value {
    let Ok(report) = serde_json::from_slice::<Value>(bytes) else {
        return unknown("smartctl returned invalid JSON");
    };
    if code & 0x07 != 0
        || report
            .pointer("/smartctl/exit_status")
            .and_then(Value::as_i64)
            != Some(i64::from(code))
    {
        return unknown("the device refused SMART access or returned incomplete evidence");
    }
    let passed = report
        .pointer("/smart_status/passed")
        .and_then(Value::as_bool);
    let state = if passed == Some(false) || code & 0xf8 != 0 {
        "warning"
    } else if passed == Some(true) {
        "pass"
    } else {
        return unknown("the device did not report a supported SMART status");
    };
    let nvme = &report["nvme_smart_health_information_log"];
    let temperature = report
        .pointer("/temperature/current")
        .and_then(Value::as_i64)
        .or_else(|| nvme["temperature"].as_i64());
    json!({
        "status": state,
        "temperature_celsius": temperature,
        "reallocated_sectors": ata_attribute(&report, 5),
        "pending_sectors": ata_attribute(&report, 197),
        "uncorrectable_sectors": ata_attribute(&report, 198),
        "media_errors": exact_count(nvme, "media_errors"),
        "unsafe_shutdowns": exact_count(nvme, "unsafe_shutdowns"),
        "smartctl_status_bits": code,
    })
}

/// Read one standard ATA attribute's raw count, when the device supplies it.
fn ata_attribute(report: &Value, id: u64) -> Option<String> {
    let entries = report.pointer("/ata_smart_attributes/table")?.as_array()?;
    let attribute = entries
        .iter()
        .find(|attribute| attribute["id"].as_u64() == Some(id))?;
    exact_count(&attribute["raw"], "value")
}

/// Preserve even NVMe counters too large for a native JSON integer.
fn exact_count(report: &Value, field: &str) -> Option<String> {
    if let Some(value) = report[format!("{field}_s")].as_str()
        && !value.is_empty()
        && value.len() <= 39
        && value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Some(value.to_owned());
    }
    report[field].as_u64().map(|value| value.to_string())
}

/// Return an unavailable hardware observation without implying good health.
fn unknown(detail: &str) -> Value {
    json!({"status": "unknown", "detail": detail})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid diagnostic failure is a warning. Permission failures stay unknown.
    #[test]
    fn warning_and_exact_counter_survive_smartctl_exit_status() {
        let bytes = br#"{
            "smartctl":{"exit_status":8},
            "smart_status":{"passed":false},
            "temperature":{"current":42},
            "nvme_smart_health_information_log":{"media_errors_s":"18446744073709551618"}
        }"#;
        let report = parse_report(8, bytes);
        assert_eq!(report["status"], "warning");
        assert_eq!(report["temperature_celsius"], 42);
        assert_eq!(report["media_errors"], "18446744073709551618");
        assert_eq!(parse_report(2, bytes)["status"], "unknown");
        assert_eq!(parse_report(8, b"{")["status"], "unknown");
    }
}
