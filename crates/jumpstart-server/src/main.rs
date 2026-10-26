//! Command-line program for trying out server bookkeeping storage locally.
//!
//! For example, `jumpstart-server init --metadata-root <new-folder> --development`
//! creates scratch metadata. `jumpstart-server inspect --metadata-root <folder>`
//! reports its verified ID and format. `snapshot create` copies that bookkeeping
//! through SQLite into a separate verified bundle; `snapshot verify` checks the
//! bundle without needing the original folder. These snapshots contain no library
//! files, Worlds data, or secrets and cannot restore a server on their own.
//! The program accepts these arguments, delegates file/database work to the storage
//! library, and prints a report or exits with an error.
//!
//! This gives developers a direct way to exercise the storage foundation while the
//! server is being built. Despite the binary's name, its current commands do not
//! run a network server or offer music, photo, or other library features.

use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_storage::{create_snapshot, initialize, inspect, verify_snapshot};

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
    /// Create or verify bootstrap metadata snapshots without media or Worlds data.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
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
/// Refuses initialization or snapshot creation without the development
/// acknowledgment before invoking storage. Propagates storage, serialization, or
/// output errors; an output failure does not undo an already-published root or
/// snapshot. Verification and inspection do not require that acknowledgment.
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
    };
    print_report(&report, title, json)
}

/// Write a verified metadata or snapshot report to stdout and flush the output.
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
