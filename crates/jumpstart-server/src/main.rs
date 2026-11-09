//! Command-line program for trying out server bookkeeping storage locally.
//!
//! For example, `jumpstart-server init --metadata-root <new-folder> --development`
//! creates scratch metadata. `jumpstart-server inspect --metadata-root <folder>`
//! reports its verified ID and format. `snapshot create` copies that bookkeeping
//! through SQLite into a separate verified bundle; `snapshot verify` checks the
//! bundle without needing the original folder. These snapshots contain no library
//! files, Worlds data, or secrets. `snapshot restore` recovers metadata into a
//! new development root and assigns it a fresh authority and synchronization epoch.
//! `migrate` upgrades an older bookkeeping schema after saving a verified copy.
//! Its journal lets the same command finish an interrupted upgrade instead of
//! replacing a server's identity or silently starting again with an empty database.
//! `jobs` retains a snapshot request and its outcome in SQLite, so a later command
//! can inspect what happened after the original command exits. Running one job
//! performs the existing snapshot operation synchronously; recovery records an
//! interrupted run as failed rather than assuming that an output file means success.
//! `storage inspect <folder>` observes filesystem capacity without granting write
//! approval or diagnosing disk health.
//! `health --metadata-root <folder>` reports both offline database verification
//! and filesystem capacity evidence, including what could not be checked.
//! `jobs changes` returns bounded original projections after a saved epoch/position.
//! `jobs run-next` executes one oldest queued snapshot without selecting an ID.
//! The program accepts these arguments, delegates file/database work to the storage
//! library, and prints a report or exits with an error.
//!
//! This gives developers a direct way to exercise the storage foundation while the
//! server is being built. Despite the binary's name, its current commands do not
//! run a network server or offer music, photo, or other library features.

use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_domain::{BlobId, Counter, JobId, JobState, PositiveCounter, SyncEpoch};
use jumpstart_storage::{
    SnapshotJob, SnapshotJobChangePage, cancel_job, create_snapshot, enqueue_snapshot_job,
    initialize, inspect, inspect_capacity, inspect_job, job_history, list_jobs, list_jobs_in_state,
    migrate, pull_job_changes, recover_jobs, restore_metadata_snapshot, run_next_snapshot_job,
    run_snapshot_job, verify_snapshot,
};

mod admin;
mod health;
mod update;

use jumpstart_server::print_report;

/// Parsed options with `command: Command` selecting one local metadata operation.
/// No network listener is configured or started by this development tool.
#[derive(Parser)]
#[command(version, about = "Jumpstart local metadata and Linux administration")]
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
    /// Run the Linux daemon with its protected local admin socket.
    Serve {
        /// Existing metadata root to verify before allowing administration.
        #[arg(long)]
        metadata_root: PathBuf,
        /// Unix socket in a pre-existing private runtime directory.
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        /// Opt in to paired, read-only Mass on one concrete application UDP address.
        #[arg(long)]
        mass_listen: Option<std::net::SocketAddr>,
    },
    /// Ask the running Linux daemon about its metadata state.
    Status {
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
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
    /// Report live local health, or check an offline root with --metadata-root.
    Health {
        /// Existing root to diagnose offline instead of contacting the daemon.
        #[arg(long, conflicts_with = "socket")]
        metadata_root: Option<PathBuf>,
        /// Override the local daemon socket for a live health query.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Print the component report as JSON, including failed checks.
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
    /// Create, verify, or restore metadata snapshots without media or Worlds data.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Enqueue, run, inspect, cancel, or recover local metadata snapshot jobs.
    Jobs {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Observe storage or register an approved volume through the daemon.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Open or review a temporary LAN pairing session.
    Pair {
        #[command(subcommand)]
        command: PairCommand,
    },
    /// List and revoke approved device installations on the local server.
    Devices {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Persist or clear the daemon's local mutation fence.
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },
    /// Verify a signed server release before applying an update.
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
}

/// Commands for a standalone metadata snapshot and its isolated recovery path.
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
    /// Recover a verified metadata snapshot into a new development root.
    Restore {
        /// Existing standalone bundle produced by snapshot create or a snapshot job.
        #[arg(long)]
        snapshot_root: PathBuf,
        /// New directory outside the source and any existing installation.
        #[arg(long)]
        output: PathBuf,
        /// Database digest reported by snapshot verify.
        #[arg(long)]
        expected_hash: BlobId,
        /// Acknowledge metadata-only recovery and fresh server identity.
        #[arg(long)]
        development: bool,
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
/// Change pages carry a `SyncEpoch` and `Counter` so a position from another
/// synchronization history cannot be mistaken for a continuation of this one.
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
    /// Run queued work or return its recorded completion without copying again.
    /// A recorded completion does not verify the artifact's current health.
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
    /// Run at most one oldest queued request; an empty queue succeeds without work.
    RunNext {
        #[arg(long)]
        metadata_root: PathBuf,
        /// Acknowledge local execution, even when the checked queue is empty.
        #[arg(long)]
        development: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read a bounded list of persisted snapshot jobs without changing them.
    List {
        #[arg(long)]
        metadata_root: PathBuf,
        /// Select this recorded state before applying the result limit.
        #[arg(long, value_parser = parse_job_state)]
        state: Option<JobState>,
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
    /// Read a bounded page of saved job changes after an existing history position.
    Changes {
        #[arg(long)]
        metadata_root: PathBuf,
        /// Synchronization history from metadata inspection or a verified baseline.
        #[arg(long)]
        epoch: SyncEpoch,
        /// Last change position already held; the returned page starts after it.
        #[arg(long)]
        after: Counter,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=500))]
        limit: u16,
        #[arg(long)]
        json: bool,
    },
    /// Show saved revisions in order without executing or changing the request.
    History {
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

/// Parse the local snapshot runner's closed state vocabulary for list filtering.
///
/// `value` is a command-line `&str`. Returns `Result<JobState, String>` for queued,
/// running, verifying, completed, failed, or canceled. Other scheduler states and
/// misspellings are argument errors before the metadata path is accessed.
fn parse_job_state(value: &str) -> Result<JobState, String> {
    match value {
        "queued" => Ok(JobState::Queued),
        "running" => Ok(JobState::Running),
        "verifying" => Ok(JobState::Verifying),
        "completed" => Ok(JobState::Completed),
        "failed" => Ok(JobState::Failed),
        "canceled" => Ok(JobState::Canceled),
        _ => Err(
            "state must be queued, running, verifying, completed, failed, or canceled".to_owned(),
        ),
    }
}

/// Observe capacity and plan or apply Linux volume registrations.
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
    /// Inspect a registration candidate through the running Linux daemon.
    Register {
        /// Existing absolute directory to observe.
        path: PathBuf,
        #[arg(long, value_parser = ["primary", "backup"])]
        role: String,
        /// Request preflight evidence and a plan ID if the candidate is eligible.
        #[arg(long)]
        plan: bool,
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Apply or retry a previously approved Linux volume registration.
    Apply {
        /// Plan ID returned by storage register --plan.
        plan_id: uuid::Uuid,
        /// Confirm that the daemon could not verify this filesystem's encryption.
        #[arg(long, required = true)]
        acknowledge_unverified_encryption: bool,
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// Actions for the separate LAN invitation and its explicit local decision.
#[derive(Subcommand)]
enum PairCommand {
    /// Open one short-lived invitation on a selected physical LAN interface.
    Create {
        #[arg(long)]
        interface: String,
        #[arg(long)]
        listen: std::net::SocketAddrV4,
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Display the exact request and six words to compare with the client.
    Pending {
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Approve the exact request and transcript after comparing its six words.
    Approve(PairDecisionArgs),
    /// Reject the exact pending request and consume its invitation.
    Reject(PairDecisionArgs),
}

/// Selects the pending request the operator has actually reviewed.
#[derive(clap::Args)]
struct PairDecisionArgs {
    request_id: uuid::Uuid,
    #[arg(long)]
    confirm_transcript: String,
    #[arg(long, default_value = admin::SOCKET_PATH)]
    socket: PathBuf,
    #[arg(long)]
    json: bool,
}

/// Operator actions on the daemon's persisted device authority.
#[derive(Subcommand)]
enum DeviceCommand {
    /// List active and revoked installations in ID order.
    List {
        #[arg(long)]
        after: Option<jumpstart_domain::DeviceId>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove authorization for an exact device ID.
    Revoke {
        device_id: jumpstart_domain::DeviceId,
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// Commands that toggle the local daemon's durable maintenance state.
#[derive(Subcommand)]
enum MaintenanceCommand {
    /// Prevent new local daemon mutations.
    Enter {
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Re-enable otherwise permitted local daemon mutations.
    Leave {
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

/// Read-only operations for a signed release and its compatibility evidence.
#[derive(Subcommand)]
enum UpdateCommand {
    /// Verify a signed release directory against the running daemon's storage.
    Plan {
        #[arg(long)]
        release_root: PathBuf,
        #[arg(long, default_value = admin::SOCKET_PATH)]
        socket: PathBuf,
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
        Command::Serve {
            metadata_root,
            socket,
            mass_listen,
        } => return admin::serve(&metadata_root, &socket, mass_listen),
        Command::Status { socket, json } => (
            admin::status(&socket)?,
            json,
            "Live local administration status:",
        ),
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
        Command::Health {
            metadata_root,
            socket,
            json,
        } => match metadata_root {
            Some(root) => return health::run(&root, json),
            None => {
                let socket = socket.unwrap_or_else(|| PathBuf::from(admin::SOCKET_PATH));
                let report = admin::health(&socket)?;
                print_report(&report, "Live local administration health:", json)?;
                if report["metadata"]["status"] != "verified_at_startup"
                    || report["capacity"]["status"] == "unknown"
                {
                    return Err("one or more live checks could not complete; see the report".into());
                }
                return Ok(());
            }
        },
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
            SnapshotCommand::Restore {
                snapshot_root,
                output,
                expected_hash,
                development,
                json,
            } => {
                if !development {
                    return Err("metadata recovery is a development preview; use --development to acknowledge a new authority and metadata-only recovery".into());
                }
                (
                    serde_json::to_value(restore_metadata_snapshot(
                        &snapshot_root,
                        &output,
                        expected_hash,
                    )?)?,
                    json,
                    "Restored metadata with a new authority:",
                )
            }
        },
        Command::Storage { command } => match command {
            StorageCommand::Inspect { path, json } => (
                serde_json::to_value(inspect_capacity(&path)?)?,
                json,
                "Storage capacity observation (not disk health or write approval):",
            ),
            StorageCommand::Register {
                path,
                role,
                plan,
                socket,
                json,
            } => {
                if !plan {
                    return Err(
                        "use --plan to review the candidate before running storage apply".into(),
                    );
                }
                (
                    admin::volume_preflight(&socket, &path, &role)?,
                    json,
                    "Read-only volume registration preflight:",
                )
            }
            StorageCommand::Apply {
                plan_id,
                acknowledge_unverified_encryption,
                socket,
                json,
            } => (
                admin::apply_volume(&socket, plan_id, acknowledge_unverified_encryption)?,
                json,
                "Registered storage volume:",
            ),
        },
        Command::Pair { command } => {
            let (action, socket, json) = match command {
                PairCommand::Create {
                    interface,
                    listen,
                    socket,
                    json,
                } => (
                    admin::PairAction::Create {
                        interface,
                        address: listen,
                    },
                    socket,
                    json,
                ),
                PairCommand::Pending { socket, json } => (admin::PairAction::Pending, socket, json),
                PairCommand::Approve(args) => pair_decision(args, true),
                PairCommand::Reject(args) => pair_decision(args, false),
            };
            (admin::pair(&socket, action)?, json, "Local device pairing:")
        }
        Command::Devices { command } => match command {
            DeviceCommand::List {
                after,
                limit,
                socket,
                json,
            } => (
                admin::list_devices(&socket, after, limit)?,
                json,
                "Device installations:",
            ),
            DeviceCommand::Revoke {
                device_id,
                socket,
                json,
            } => (
                admin::revoke_device(&socket, device_id)?,
                json,
                "Persisted device revocation:",
            ),
        },
        Command::Maintenance { command } => {
            let (enabled, socket, json) = match command {
                MaintenanceCommand::Enter { socket, json } => (true, socket, json),
                MaintenanceCommand::Leave { socket, json } => (false, socket, json),
            };
            (
                admin::maintenance(&socket, enabled)?,
                json,
                "Local daemon maintenance state:",
            )
        }
        Command::Jobs { command } => return run_jobs(command),
        Command::Update { command } => match command {
            UpdateCommand::Plan {
                release_root,
                socket,
                json,
            } => (
                update::plan(&release_root, &socket)?,
                json,
                "Verified signed update preflight:",
            ),
        },
    };
    print_report(&report, title, json)
}

/// Convert the reviewed CLI request into a typed approval or refusal message.
fn pair_decision(args: PairDecisionArgs, approve: bool) -> (admin::PairAction, PathBuf, bool) {
    let action = admin::PairAction::Decide {
        request_id: args.request_id,
        transcript: args.confirm_transcript,
        approve,
    };
    (action, args.socket, args.json)
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
            let value = serde_json::to_value(&job)?;
            print_report(&value, "Persisted snapshot job outcome:", json)?;
            return job_result_status(&job);
        }
        JobCommand::RunNext {
            metadata_root,
            development,
            json,
        } => {
            acknowledge_job_write(development)?;
            let job = run_next_snapshot_job(&metadata_root)?;
            print_report(
                &serde_json::json!({"job": &job}),
                "Persisted next snapshot job outcome:",
                json,
            )?;
            if let Some(job) = &job {
                job_result_status(job)?;
            }
            return Ok(());
        }
        JobCommand::List {
            metadata_root,
            state,
            limit,
            json,
        } => {
            let jobs = match state {
                Some(state) => list_jobs_in_state(&metadata_root, state, limit)?,
                None => list_jobs(&metadata_root, limit)?,
            };
            let report = match state {
                Some(state) => serde_json::json!({"jobs": jobs, "state": state}),
                None => serde_json::json!({"jobs": jobs}),
            };
            (report, json, "Persisted local snapshot jobs:")
        }
        JobCommand::Inspect {
            metadata_root,
            job_id,
            json,
        } => (
            serde_json::to_value(inspect_job(&metadata_root, job_id)?)?,
            json,
            "Persisted local snapshot job:",
        ),
        JobCommand::History {
            metadata_root,
            job_id,
            json,
        } => {
            let history = job_history(&metadata_root, job_id)?;
            return print_job_history(job_id, &history, json);
        }
        JobCommand::Changes {
            metadata_root,
            epoch,
            after,
            limit,
            json,
        } => {
            let page = pull_job_changes(&metadata_root, epoch, after, limit)?;
            return print_job_changes(&page, json);
        }
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

/// Give the CLI a nonzero outcome for a saved failure after displaying its record.
///
/// `job: &SnapshotJob` is the runner's saved outcome. Returns
/// `Result<(), Box<dyn Error>>`: `Err` carries the job ID and retained explanation
/// for Failed work; `Ok(())` preserves the runner's other returned outcome.
/// The caller prints the record first so scripts can inspect a failed attempt.
fn job_result_status(job: &SnapshotJob) -> Result<(), Box<dyn Error>> {
    if job.state == JobState::Failed {
        return Err(format!(
            "snapshot job {} failed: {}",
            job.job_id,
            job.error.as_deref().unwrap_or("inspect the saved outcome")
        )
        .into());
    }
    Ok(())
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

/// Display the stages behind a job's current row, rather than rerunning its work.
///
/// `job_id: JobId` identifies the request, `history: &[SnapshotJob]` holds checked
/// revisions, and `json: bool` selects full projections instead of a concise text
/// timeline. Returns `Result<(), Box<dyn Error>>` after flushing stdout; storage
/// is already read-only, and serialization/write errors are propagated.
fn print_job_history(
    job_id: JobId,
    history: &[SnapshotJob],
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        return print_report(
            &serde_json::json!({"job_id": job_id, "history": history}),
            "Persisted snapshot job history:",
            true,
        );
    }
    let mut output = std::io::stdout().lock();
    writeln!(output, "Snapshot job {job_id} history:")?;
    writeln!(output, "  revision  state         worker token")?;
    for job in history {
        let state = serde_json::to_value(job.state)?;
        let state = state.as_str().ok_or("invalid serialized job state")?;
        writeln!(
            output,
            "  {:>8}  {state:<12} {}",
            job.revision, job.attempt_generation
        )?;
        if let Some(error) = &job.error {
            writeln!(output, "    error: {}", serde_json::to_string(error)?)?;
        }
    }
    output.flush()?;
    Ok(())
}

/// Display a change page as compact JSON or a concise saved-state timeline.
///
/// `page: &SnapshotJobChangePage` contains checked historical projections and
/// cursor bounds; `json: bool` selects the full response. Returns
/// `Result<(), Box<dyn Error>>` after flushing stdout. Compact JSON preserves the
/// backend's 1-MiB JSON-body bound; its final newline is outside that body.
/// Serialization and write failures propagate and can leave partial output.
fn print_job_changes(page: &SnapshotJobChangePage, json: bool) -> Result<(), Box<dyn Error>> {
    let mut output = std::io::stdout().lock();
    if json {
        serde_json::to_writer(&mut output, page)?;
        writeln!(output)?;
    } else {
        writeln!(output, "Persisted snapshot-job changes:")?;
        writeln!(output, "  server_id: {}", page.server_id)?;
        writeln!(output, "  sync_epoch: {}", page.sync_epoch)?;
        writeln!(output, "  after: {}", page.after)?;
        writeln!(output, "  next: {}", page.next)?;
        writeln!(output, "  high_water: {}", page.high_water)?;
        writeln!(output, "  minimum: {}", page.minimum)?;
        writeln!(output, "  batches: {}", page.batches.len())?;
        for batch in &page.batches {
            let state = serde_json::to_value(batch.job.state)?;
            let state = state.as_str().ok_or("invalid serialized job state")?;
            writeln!(
                output,
                "  {}: job {}, revision {}, {state}",
                batch.seq, batch.job.job_id, batch.job.revision
            )?;
        }
    }
    output.flush()?;
    Ok(())
}
