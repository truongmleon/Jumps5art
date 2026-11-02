//! Command-line tools for local server bookkeeping and storage observations.
//!
//! For example, `jumpstart-server init --metadata-root <new-folder> --development`
//! creates scratch metadata. `jumpstart-server inspect --metadata-root <folder>`
//! reports its verified ID and format. `snapshot create` copies that bookkeeping
//! through SQLite into a separate verified bundle; `snapshot verify` checks the
//! bundle without needing the original folder. These snapshots contain no library
//! files, Worlds data, or secrets and cannot restore a server on their own.
//! `storage inspect <folder>` observes filesystem capacity, for example before
//! planning a large import. That point-in-time report checks neither disk health
//! nor whether Jumpstart may safely write there.
//! The program accepts these arguments, delegates file/database work to the storage
//! library, and prints a report or exits with an error.
//!
//! This gives developers a direct way to exercise the storage foundation while the
//! server is being built. Despite the binary's name, its current commands do not
//! run a network server or offer music, photo, or other library features.

use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_storage::{
    create_snapshot, initialize, inspect, inspect_capacity, migrate, verify_snapshot,
};

/// Parsed options with `command: Command` selecting a local storage operation.
/// No network listener is configured or started by this development tool.
#[derive(Parser)]
#[command(
    version,
    about = "Jumpstart local storage development tools; no network listener"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Local metadata commands: `Init` creates a development root and `Inspect`
/// verifies an offline root. `Snapshot` selects a nested create/verify command.
/// `Storage` observes directory capacity separately from metadata inspection.
/// Paths are owned `PathBuf` values and output selection uses a `bool`.
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
    /// Create or verify bootstrap metadata snapshots without media or Worlds data.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
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
    /// Create a local bootstrap metadata snapshot; this is not a full server backup.
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

/// Execute a parsed local storage operation and print its scoped report.
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
/// snapshot. Verification, metadata inspection, and read-only capacity observation
/// do not require that acknowledgment. Capacity observations are point-in-time
/// measurements, not disk-health checks or permission to write.
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
                return Err("metadata migration is a development preview; use --development to acknowledge the schema upgrade and its recovery copy of bootstrap metadata".into());
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
                    return Err("snapshot creation is a development preview; use --development to acknowledge that this copies bootstrap metadata only, not media, Worlds, secrets, or a restorable server backup".into());
                }
                (
                    serde_json::to_value(create_snapshot(&metadata_root, &output)?)?,
                    json,
                    "Verified local bootstrap metadata snapshot (not a full backup):",
                )
            }
            SnapshotCommand::Verify {
                snapshot_root,
                json,
            } => (
                serde_json::to_value(verify_snapshot(&snapshot_root)?)?,
                json,
                "Verified local bootstrap metadata snapshot (not a full backup):",
            ),
        },
        Command::Storage { command } => match command {
            StorageCommand::Inspect { path, json } => (
                serde_json::to_value(inspect_capacity(&path)?)?,
                json,
                "Storage capacity observation (not disk health or write approval):",
            ),
        },
    };
    print_report(&report, title, json)
}

/// Write a metadata, snapshot, or capacity report to stdout and flush the output.
///
/// # Arguments
///
/// * `report` — borrowed `&serde_json::Value` containing the serialized report.
/// * `title` — `&str` heading describing the report's scope in text mode.
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
            print_fields(&mut output, fields, 2)?;
        }
    }
    output.flush()?;
    Ok(())
}

/// Print nested report objects as indented labels for people reading the terminal.
///
/// For example, capacity thresholds appear under `assessment` on separate lines
/// instead of being squeezed into a JSON object inside the text report.
///
/// # Arguments
///
/// * `output` — mutable borrowed writer receiving the report's text fields.
/// * `fields` — borrowed JSON object produced from a typed local storage report.
/// * `indent` — `usize` number of leading spaces for fields at this object level.
///
/// # Returns
///
/// `std::io::Result<()>` after writing this object and its nested objects.
///
/// # Errors
///
/// Propagates write failures; output may already contain part of the report.
fn print_fields(
    output: &mut impl Write,
    fields: &serde_json::Map<String, serde_json::Value>,
    indent: usize,
) -> std::io::Result<()> {
    for (name, value) in fields {
        write!(output, "{blank:indent$}{name}:", blank = "")?;
        match value {
            serde_json::Value::Object(nested) => {
                writeln!(output)?;
                print_fields(output, nested, indent + 2)?;
            }
            serde_json::Value::String(text) => writeln!(output, " {text}")?,
            other => writeln!(output, " {other}")?,
        }
    }
    Ok(())
}
