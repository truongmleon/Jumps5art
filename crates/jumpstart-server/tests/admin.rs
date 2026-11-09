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
