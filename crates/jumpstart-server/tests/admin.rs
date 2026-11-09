//! Exercises the real Linux admin socket and its metadata ownership boundary.
//!
//! Requests go through the built binary so the tests cover kernel credentials,
//! the CLI wire exchange, and shutdown cleanup together.

#![cfg(target_os = "linux")]

use std::{
    fs,
    io::Write,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, geteuid, kill_process};
use serde_json::Value;

/// Keep a test daemon from surviving a panic or assertion failure.
struct TestDaemon(Child);

impl Drop for TestDaemon {
    /// Stop and reap the child if it is still running.
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a process command for the CLI compiled by Cargo.
fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jumpstart-server"))
}

/// Run a single JSON status query at the test's private socket.
fn status(socket: &Path) -> Output {
    command()
        .args(["status", "--json", "--socket"])
        .arg(socket)
        .output()
        .unwrap()
}

/// Create and verify a real private listener, then wait for a successful exchange.
fn start(root: &Path, socket: &Path) -> TestDaemon {
    let mut daemon = TestDaemon(
        command()
            .args(["serve", "--metadata-root"])
            .arg(root)
            .arg("--socket")
            .arg(socket)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    loop {
        if socket.exists() && status(socket).status.success() {
            return daemon;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited before readiness"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon startup timeout"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Check the Linux process owns the root, tolerates malformed peers, and releases
/// both the maintenance lock and socket on normal shutdown.
#[test]
fn private_daemon_owns_metadata_until_signal_shutdown() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    let baseline = jumpstart_storage::initialize(&root).unwrap();
    let mut daemon = start(&root, &socket);
    let entry = fs::symlink_metadata(&socket).unwrap();
    assert_eq!(entry.mode() & 0o777, 0o600);
    assert!(matches!(
        jumpstart_storage::inspect(&root),
        Err(jumpstart_storage::StorageError::InUse)
    ));

    let mut malformed = UnixStream::connect(&socket).unwrap();
    malformed.write_all(&u32::MAX.to_be_bytes()).unwrap();
    drop(malformed);
    let output = status(&socket);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "metadata_verified");
    assert_eq!(
        report["metadata"]["server_id"],
        serde_json::to_value(baseline.server_id).unwrap()
    );

    let pid = Pid::from_raw(i32::try_from(daemon.0.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    assert!(daemon.0.wait().unwrap().success());
    assert!(!socket.exists());
    assert_eq!(jumpstart_storage::inspect(&root).unwrap(), baseline);
}

/// Keep absent metadata absent while exposing recovery evidence to the operator.
#[test]
fn recovery_mode_never_creates_the_missing_root() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let missing = parent.path().join("missing");
    let socket = parent.path().join("admin.sock");
    let _daemon = start(&missing, &socket);
    let output = status(&socket);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "recovery_required");
    assert!(!missing.exists());
}

/// Collect the identity of the opened candidate and flag data without adopting it.
#[test]
fn volume_preflight_reports_mount_and_preserves_existing_files() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    let candidate = parent.path().join("candidate");
    fs::create_dir(&candidate).unwrap();
    jumpstart_storage::initialize(&root).unwrap();
    let _daemon = start(&root, &socket);
    let sentinel = candidate.join("existing.txt");
    fs::write(&sentinel, b"do not adopt").unwrap();
    let output = command()
        .args(["storage", "register"])
        .arg(&candidate)
        .args(["--plan", "--role", "primary", "--json", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scope"], "registration_preflight");
    assert!(report["candidate"]["mount_id"].is_string());
    assert_eq!(report["candidate"]["empty"], false);
    assert!(
        !report["candidate"]["blocking_reasons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(&sentinel).unwrap(), b"do not adopt");
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 1);

    let link = parent.path().join("linked-candidate");
    std::os::unix::fs::symlink(&candidate, &link).unwrap();
    let output = command()
        .args(["storage", "register"])
        .arg(&link)
        .args(["--plan", "--role", "backup", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("without following links"));
}

/// Check that live health measures the leased root and refuses to reuse startup
/// evidence after a replacement directory appears at the same path.
#[test]
fn live_health_distinguishes_a_replaced_root_from_the_verified_metadata() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    jumpstart_storage::initialize(&root).unwrap();
    jumpstart_storage::enqueue_snapshot_job(&root, &parent.path().join("snapshot")).unwrap();
    let _daemon = start(&root, &socket);

    let output = command()
        .args(["health", "--json", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scope"], "live_local_admin");
    assert_eq!(report["metadata"]["status"], "verified_at_startup");
    assert_eq!(report["capacity"]["evidence"]["scope"], "capacity_only");
    assert_eq!(report["snapshot_jobs"]["by_state"]["queued"], "1");
    assert_eq!(report["disk_hardware"]["status"], "unknown");
    assert_eq!(report["registered_volumes"]["status"], "observed");
    assert_eq!(
        report["registered_volumes"]["evidence"]["volumes"],
        serde_json::json!([])
    );
    assert_eq!(report["registered_volumes"]["evidence"]["pending"], "0");

    fs::rename(&root, parent.path().join("detached")).unwrap();
    fs::create_dir(&root).unwrap();
    let output = command()
        .args(["health", "--json", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["metadata"]["status"], "unavailable");
    assert_eq!(report["snapshot_jobs"]["status"], "unknown");
    assert_eq!(report["registered_volumes"]["status"], "unknown");
    assert_eq!(report["capacity"]["evidence"]["scope"], "capacity_only");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

/// Invoke the separate Worlds binary against the test daemon's protected socket.
fn worlds(socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_worlds"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .unwrap()
}

/// A fresh daemon reports an absent managed world and an empty recovery index.
#[test]
fn worlds_cli_reads_fresh_catalog_without_touching_world_files() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    jumpstart_storage::initialize(&root).unwrap();
    let _daemon = start(&root, &socket);
    let output = worlds(&socket, &["status", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["lifecycle"], "absent");
    assert_eq!(report["players_can_connect"], false);
    assert_eq!(report["evidence"]["registered_primaries"], "0");
    let output = worlds(&socket, &["backup", "list", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(index["index"]["points"].as_array().unwrap().is_empty());
    assert_eq!(index["index"]["truncated"], false);
    assert!(!root.join("worlds").exists());
}

/// Stored points do not prove the Java lifecycle, and listing honors its bound.
#[test]
fn worlds_cli_distinguishes_indexed_points_from_unassessed_runtime() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    jumpstart_storage::initialize(&root).unwrap();
    let database = rusqlite::Connection::open(root.join("database/jumpstart.sqlite")).unwrap();
    database.execute_batch(
        "INSERT INTO volumes VALUES (zeroblob(16),'primary',zeroblob(16),'fixture','unknown',NULL);
         INSERT INTO worlds_backup_catalog VALUES (zeroblob(16),zeroblob(16),1,'b3',zeroblob(32),1000,1001);
         INSERT INTO worlds_backup_catalog VALUES (randomblob(16),zeroblob(16),1,'b3',zeroblob(32),2000,2001);"
    ).unwrap();
    database.close().unwrap();
    let _daemon = start(&root, &socket);

    let output = worlds(&socket, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(4));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["lifecycle"].is_null());
    assert!(report["players_can_connect"].is_null());
    assert_eq!(report["reason"], "runtime_unassessed");
    assert_eq!(report["evidence"]["indexed_recovery_points"], "2");

    let output = worlds(&socket, &["backup", "list", "--limit", "1", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["scope"], "worlds_recovery_index");
    let points = report["index"]["points"].as_array().unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0]["captured_at_ms"], 2000);
    assert_eq!(points[0]["manifest_hash"], format!("b3:{}", "0".repeat(64)));
    assert_eq!(report["index"]["truncated"], true);
}

/// Exercise the command, framed request, durable fence, and daemon restart.
#[test]
fn maintenance_survives_restart_and_refuses_storage_apply_until_left() {
    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    jumpstart_storage::initialize(&root).unwrap();
    let mut daemon = start(&root, &socket);

    for mode in ["enter", "enter"] {
        let output = command()
            .args(["maintenance", mode, "--json", "--socket"])
            .arg(&socket)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["maintenance"], true);
    }
    let pid = Pid::from_raw(i32::try_from(daemon.0.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    assert!(daemon.0.wait().unwrap().success());
    let _restarted = start(&root, &socket);
    let report: Value = serde_json::from_slice(&status(&socket).stdout).unwrap();
    assert_eq!(report["maintenance"], true);

    let output = command()
        .args([
            "storage",
            "apply",
            "53e133a2-ae22-45a4-bafa-9cc4af6b6fa9",
            "--acknowledge-unverified-encryption",
            "--socket",
        ])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("maintenance is enabled"));

    let output = command()
        .args(["maintenance", "leave", "--json", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&status(&socket).stdout).unwrap();
    assert_eq!(report["maintenance"], false);

    let output = command()
        .args([
            "storage",
            "apply",
            "53e133a2-ae22-45a4-bafa-9cc4af6b6fa9",
            "--acknowledge-unverified-encryption",
            "--socket",
        ])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown or expired"));
}

/// Exercise the CLI and daemon against a persisted device approval fixture.
#[test]
fn device_revocation_remains_in_the_catalog_after_restart() {
    use jumpstart_domain::{ClientInstanceId, DeviceId, Owner};
    use jumpstart_storage::{AdminMetadata, DeviceRegistration};

    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    jumpstart_storage::initialize(&root).unwrap();
    let id = DeviceId::new();
    let key = [11; 32];
    let instance_id = ClientInstanceId::new();
    let grant = || DeviceRegistration {
        device_id: id,
        public_key: key,
        name: "Library laptop".into(),
        platform: "linux".into(),
        app: Owner::Mass,
        instance_id,
        signed_receipt: b"signed receipt fixture".to_vec(),
    };
    let mut lease = AdminMetadata::open(&root).unwrap();
    lease.register_device(grant()).unwrap();
    assert!(lease.device_receipt(&key).unwrap().is_some());
    drop(lease);
    let mut daemon = start(&root, &socket);

    let revoke = || {
        command()
            .args(["devices", "revoke", &id.to_string(), "--json", "--socket"])
            .arg(&socket)
            .output()
            .unwrap()
    };
    let output = revoke();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let revoked: Value = serde_json::from_slice(&output.stdout).unwrap();
    let repeated = revoke();
    assert!(repeated.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&repeated.stdout).unwrap(),
        revoked
    );
    assert!(revoked["device"]["revoked_at_ms"].is_string());

    let pid = Pid::from_raw(i32::try_from(daemon.0.id()).unwrap()).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    assert!(daemon.0.wait().unwrap().success());
    let mut lease = AdminMetadata::open(&root).unwrap();
    assert!(lease.device_key_known(&key).unwrap());
    assert!(lease.device_receipt(&key).unwrap().is_none());
    assert!(lease.register_device(grant()).is_err());
    drop(lease);

    let _restarted = start(&root, &socket);
    let output = command()
        .args(["devices", "list", "--json", "--socket"])
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["page"]["devices"][0], revoked["device"]);
}

/// The protected socket exposes the real empty catalog and refuses an unbound primary.
#[test]
fn mass_socket_lists_the_catalog_and_rejects_an_unregistered_destination() {
    use jumpstart_server::mass_client::MassClient;
    use jumpstart_storage::mass::MassDestination;
    use uuid::Uuid;

    if geteuid().is_root() {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("metadata");
    let socket = parent.path().join("admin.sock");
    jumpstart_storage::initialize(&root).unwrap();
    let _daemon = start(&root, &socket);
    let client = MassClient::new(socket);
    assert!(client.list(None, None).unwrap().items.is_empty());
    assert!(client.primaries().unwrap().is_empty());
    let error = client
        .import(
            MassDestination {
                request_id: Uuid::new_v4(),
                primary_id: Uuid::new_v4(),
                parent: None,
                name: "file.txt".into(),
            },
            b"cannot reach storage",
        )
        .unwrap_err();
    assert!(error.to_string().contains("registered primary"));
    assert!(client.list(None, None).unwrap().items.is_empty());
}
