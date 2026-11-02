//! Command-line program for trying out server bookkeeping storage locally.
//!
//! For example, `jumpstart-server init --metadata-root <new-folder> --development`
//! creates scratch metadata. `jumpstart-server inspect --metadata-root <folder>`
//! reports its verified ID and format. `snapshot create` copies that bookkeeping
//! through SQLite into a separate verified bundle; `snapshot verify` checks the
//! bundle without needing the original folder. These snapshots contain no library
//! files, Worlds data, or secrets and cannot restore a server on their own.
//! `migrate` upgrades an older bookkeeping schema after saving a verified copy.
//! Its journal lets the same command finish an interrupted upgrade instead of
//! replacing a server's identity or silently starting again with an empty database.
//! `jobs` retains a snapshot request and its outcome in SQLite, so a later command
//! can inspect what happened after the original command exits. Running one job
//! performs the existing snapshot operation synchronously; recovery records an
//! interrupted run as failed rather than assuming that an output file means success.
//! `storage inspect <folder>` observes filesystem capacity without granting write
//! approval or diagnosing disk health.
//! The program accepts these arguments, delegates file/database work to the storage
//! library, and prints a report or exits with an error.
//!
//! This gives developers a direct way to exercise the storage foundation while the
//! server is being built. Despite the binary's name, its current commands do not
//! run a network server or offer music, photo, or other library features.

use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_domain::{JobId, JobState, PositiveCounter};
use jumpstart_storage::{
    cancel_job, create_snapshot, enqueue_snapshot_job, initialize, inspect, inspect_capacity,
    inspect_job, list_jobs, migrate, recover_jobs, run_snapshot_job, verify_snapshot,
};

/// Parsed options with `command: Command` selecting one local metadata operation.
/// No network listener is configured or started by this development tool.
#[derive(Parser)]
#[command(
    version,
    about = "Jumpstart local metadata development tools; no network listener"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Local metadata commands: `Init` creates a development root and `Inspect`
/// verifies an offline root. `Snapshot` selects a nested create/verify command.
/// `Migrate` upgrades existing bookkeeping while retaining a pre-upgrade snapshot.
/// `Jobs` administers persisted requests for the existing snapshot operation.
/// Root paths are owned `PathBuf` values and output selection uses a `bool`.
#[derive(Subcommand)]
enum Command {
    /// Initialize a new scratch metadata root. Existing paths are never adopted.
    Init {
        #[arg(long)]
        metadata_root: PathBuf,
        /// Acknowledge the development preview; production storage is not qualified.
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect an offline, checkpointed root without changing its contents.
    Inspect {
        #[arg(long)]
        metadata_root: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Upgrade offline metadata, retaining a verified pre-upgrade snapshot.
    Migrate {
        /// Existing offline metadata root to upgrade, preserving its server identity.
        #[arg(long)]
        metadata_root: PathBuf,
        /// New directory outside the source for its pre-upgrade snapshot.
        /// Retrying an interrupted upgrade must supply the same path.
        /// A clean, current root leaves this path untouched.
        #[arg(long)]
        snapshot_output: PathBuf,
        /// Acknowledge the development preview and limited metadata-only recovery copy.
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create or verify metadata snapshots without media or Worlds data.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Enqueue, run, inspect, cancel, or recover local metadata snapshot jobs.
    Jobs {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Observe existing-directory storage capacity without approving writes.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
}

/// Snapshot operations: `Create` owns source/output paths and an acknowledgment;
/// `Verify` owns a bundle path. Both carry a `bool` selecting JSON output.
#[derive(Subcommand)]
enum SnapshotCommand {
    /// Create a local metadata snapshot; this is not a full server backup.
    Create {
        /// Existing offline, checkpointed metadata root to read without changing.
        #[arg(long)]
        metadata_root: PathBuf,
        /// New snapshot directory outside the source; its parent must already exist.
        #[arg(long)]
        output: PathBuf,
        /// Acknowledge the development preview and its limited metadata-only scope.
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Verify an existing snapshot without changing it or requiring its source.
    Verify {
        #[arg(long)]
        snapshot_root: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// A parsed request for the local snapshot queue, with each action's needed data.
///
/// The variants keep a snapshot destination with enqueue requests, a checked
/// `JobId` with requests about one job, and an expected `PositiveCounter` revision
/// with cancellation. This lets execution reject an outdated cancellation rather
/// than treating every command as an arbitrary status edit. The enum routes
/// requests; the storage library owns persistence and snapshot verification.
#[derive(Subcommand)]
enum JobCommand {
    /// Save a queued snapshot request without creating its output yet.
    EnqueueSnapshot {
        #[arg(long)]
        metadata_root: PathBuf,
        /// New snapshot directory outside the source; its parent must exist.
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run one queued snapshot synchronously and retain its verified outcome.
    Run {
        #[arg(long)]
        metadata_root: PathBuf,
        #[arg(long)]
        job_id: JobId,
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read a bounded list of persisted snapshot jobs without changing them.
    List {
        #[arg(long)]
        metadata_root: PathBuf,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
        #[arg(long)]
        json: bool,
    },
    /// Read the saved status and outcome of one snapshot job.
    Inspect {
        #[arg(long)]
        metadata_root: PathBuf,
        #[arg(long)]
        job_id: JobId,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a still-queued request using the revision shown by inspection.
    Cancel {
        #[arg(long)]
        metadata_root: PathBuf,
        #[arg(long)]
        job_id: JobId,
        #[arg(long)]
        expected_revision: PositiveCounter,
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Fence interrupted active jobs and retain their artifacts for inspection.
    Recover {
        #[arg(long)]
        metadata_root: PathBuf,
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
}

/// Read-only filesystem observations, separate from verifying a metadata root.
///
/// `Inspect` owns an existing directory's positional `PathBuf` and a `bool`
/// selecting JSON output. Its capacity observation checks neither disk health
/// nor encryption, registered identity, reservations, or write authorization.
#[derive(Subcommand)]
enum StorageCommand {
    /// Observe point-in-time capacity; this is not disk health or write approval.
    Inspect {
        /// Existing directory to observe; missing paths, files, and final symlinks fail.
        path: PathBuf,
        /// Print the capacity-only report as JSON, with byte counts as decimal strings.
        #[arg(long)]
        json: bool,
    },
}

/// Parse process arguments, execute the command, and report failures to stderr.
///
/// # Returns
///
/// `ExitCode::SUCCESS` after a successful operation and report, or
/// `ExitCode::FAILURE` after an execution error. Clap handles argument errors and
/// help/version requests during parsing before execution begins.
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Execute a parsed metadata operation and print its verified report.
///
/// # Arguments
///
/// * `cli` — owned `Cli` selecting the operation and its arguments.
///
/// # Returns
///
/// `Result<(), Box<dyn Error>>`, with `Ok(())` after the report is flushed.
///
/// # Errors
///
/// Refuses initialization, migration, or snapshot creation without the development
/// acknowledgment before invoking storage. Propagates storage, serialization, or
/// output errors; an output failure does not undo an already-published root or
/// snapshot. A migration error may leave its journal for a retry with the same
/// paths; this command never deletes it as a workaround. Job writes apply the same
/// acknowledgment in `run_jobs`. Verification, inspection, and job reads need none.
fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let (report, json, title) = match cli.command {
        Command::Init {
            metadata_root,
            development,
            json,
        } => {
            if !development {
                return Err("initialization is a development preview; use --development with a new scratch path (production storage qualification is not implemented)".into());
            }
            (
                serde_json::to_value(initialize(&metadata_root)?)?,
                json,
                "Verified offline development metadata:",
            )
        }
        Command::Inspect {
            metadata_root,
            json,
        } => (
            serde_json::to_value(inspect(&metadata_root)?)?,
            json,
            "Verified offline development metadata:",
        ),
        Command::Migrate {
            metadata_root,
            snapshot_output,
            development,
            json,
        } => {
            if !development {
                return Err("metadata migration is a development preview; use --development to acknowledge the schema upgrade and its recovery copy of local metadata".into());
            }
            (
                serde_json::to_value(migrate(&metadata_root, &snapshot_output)?)?,
                json,
                "Verified offline development metadata migration:",
            )
        }
        Command::Snapshot { command } => match command {
            SnapshotCommand::Create {
                metadata_root,
                output,
                development,
                json,
            } => {
                if !development {
                    return Err("snapshot creation is a development preview; use --development to acknowledge that this copies local metadata only, not media, Worlds, secrets, or a restorable server backup".into());
                }
                (
                    serde_json::to_value(create_snapshot(&metadata_root, &output)?)?,
                    json,
                    "Verified local metadata snapshot (not a full backup):",
                )
            }
            SnapshotCommand::Verify {
                snapshot_root,
                json,
            } => (
                serde_json::to_value(verify_snapshot(&snapshot_root)?)?,
                json,
                "Verified local metadata snapshot (not a full backup):",
            ),
        },
        Command::Storage { command } => match command {
            StorageCommand::Inspect { path, json } => (
                serde_json::to_value(inspect_capacity(&path)?)?,
                json,
                "Storage capacity observation (not disk health or write approval):",
            ),
        },
        Command::Jobs { command } => return run_jobs(command),
    };
    print_report(&report, title, json)
}

/// Execute one persisted snapshot-job request and print its saved result.
///
/// # Arguments
///
/// * `command` — owned `JobCommand` selecting the action and its checked arguments.
///
/// # Returns
///
/// `Result<(), Box<dyn Error>>` after flushing the report. List and recovery use
/// a `jobs` array inside an object so text and JSON modes both show their results.
///
/// # Errors
///
/// Requires acknowledgment before any storage call for writes. Propagates storage,
/// serialization, and output failures. A run whose persisted outcome is `Failed`
/// prints that record and then returns an error, giving scripts a nonzero exit.
fn run_jobs(command: JobCommand) -> Result<(), Box<dyn Error>> {
    let (report, json, title) = match command {
        JobCommand::EnqueueSnapshot {
            metadata_root,
            output,
            development,
            json,
        } => {
            acknowledge_job_write(development)?;
            (
                serde_json::to_value(enqueue_snapshot_job(&metadata_root, &output)?)?,
                json,
                "Persisted queued snapshot job:",
            )
        }
        JobCommand::Run {
            metadata_root,
            job_id,
            development,
            json,
        } => {
            acknowledge_job_write(development)?;
            let job = run_snapshot_job(&metadata_root, job_id)?;
            let failed = job.state == JobState::Failed;
            let value = serde_json::to_value(&job)?;
            print_report(&value, "Persisted snapshot job outcome:", json)?;
            if failed {
                return Err(format!(
                    "snapshot job {} failed: {}",
                    job.job_id,
                    job.error.as_deref().unwrap_or("inspect the saved outcome")
                )
                .into());
            }
            return Ok(());
        }
        JobCommand::List {
            metadata_root,
            limit,
            json,
        } => (
            serde_json::json!({"jobs": list_jobs(&metadata_root, limit)?}),
            json,
            "Persisted local snapshot jobs:",
        ),
        JobCommand::Inspect {
            metadata_root,
            job_id,
            json,
        } => (
            serde_json::to_value(inspect_job(&metadata_root, job_id)?)?,
            json,
            "Persisted local snapshot job:",
        ),
        JobCommand::Cancel {
            metadata_root,
            job_id,
            expected_revision,
            development,
            json,
        } => {
            acknowledge_job_write(development)?;
            (
                serde_json::to_value(cancel_job(&metadata_root, job_id, expected_revision)?)?,
                json,
                "Persisted canceled snapshot job:",
            )
        }
        JobCommand::Recover {
            metadata_root,
            development,
            json,
        } => {
            acknowledge_job_write(development)?;
            (
                serde_json::json!({"jobs": recover_jobs(&metadata_root)?}),
                json,
                "Recovered interrupted local snapshot jobs:",
            )
        }
    };
    print_report(&report, title, json)
}

/// Check the explicit acknowledgment before a job command can write to storage.
///
/// `development` is the parsed `bool` flag. Returns `Result<(), Box<dyn Error>>`;
/// false returns an explanatory error before opening a root or creating output.
fn acknowledge_job_write(development: bool) -> Result<(), Box<dyn Error>> {
    if !development {
        return Err("snapshot job writes are a development preview; use --development to acknowledge local persistence and offline execution".into());
    }
    Ok(())
}

/// Write a metadata, migration, snapshot, or persisted-job report and flush output.
///
/// # Arguments
///
/// * `report` — borrowed `&serde_json::Value` containing the serialized report.
/// * `title` — `&str` heading describing the verified report's scope in text mode.
/// * `json` — `bool` selecting pretty JSON when true, or labeled text when false.
///
/// # Returns
///
/// `Result<(), Box<dyn Error>>`, with `Ok(())` after a successful flush.
///
/// # Errors
///
/// Propagates serialization and stdout write/flush errors. Output may already
/// contain part of the report when an I/O error occurs.
fn print_report(report: &serde_json::Value, title: &str, json: bool) -> Result<(), Box<dyn Error>> {
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(output, "{}", serde_json::to_string_pretty(report)?)?;
    } else {
        writeln!(output, "{title}")?;
        if let serde_json::Value::Object(fields) = report {
            for (name, value) in fields {
                match value {
                    serde_json::Value::String(text) => writeln!(output, "  {name}: {text}")?,
                    other => writeln!(output, "  {name}: {other}")?,
                }
            }
        }
    }
    output.flush()?;
    Ok(())
}
