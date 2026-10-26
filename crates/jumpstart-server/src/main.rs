//! Command-line program for trying out server bookkeeping storage locally.
//!
//! For example, `jumpstart-server init --metadata-root <new-folder> --development`
//! creates scratch metadata. `jumpstart-server inspect --metadata-root <folder>`
//! reports its verified ID and format.
//! The program accepts these arguments, delegates file/database work to the storage
//! library, and prints a report or exits with an error.
//!
//! This gives developers a direct way to exercise the storage foundation while the
//! server is being built. Despite the binary's name, its current commands do not
//! run a network server or offer music, photo, or other library features.

use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_storage::{MetadataReport, initialize, inspect};

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
/// verifies an offline root. Each owns a `PathBuf` and a `bool` for JSON output;
/// `Init` additionally carries the development acknowledgment as a `bool`.
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
/// Refuses initialization without the development acknowledgment and propagates
/// storage, serialization, or output errors. Output failure does not undo storage
/// changes already completed by initialization.
fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let (report, json) = match cli.command {
        Command::Init {
            metadata_root,
            development,
            json,
        } => {
            if !development {
                return Err("initialization is a development preview; use --development with a new scratch path (production storage qualification is not implemented)".into());
            }
            (initialize(&metadata_root)?, json)
        }
        Command::Inspect {
            metadata_root,
            json,
        } => (inspect(&metadata_root)?, json),
    };
    print_report(&report, json)
}

/// Write a verified metadata report to stdout and flush the output.
///
/// # Arguments
///
/// * `report` — borrowed `&MetadataReport` to serialize and display.
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
fn print_report(report: &MetadataReport, json: bool) -> Result<(), Box<dyn Error>> {
    let value = serde_json::to_value(report)?;
    let mut output = std::io::stdout().lock();
    if json {
        writeln!(output, "{}", serde_json::to_string_pretty(&value)?)?;
    } else {
        writeln!(output, "Verified offline development metadata:")?;
        if let serde_json::Value::Object(fields) = value {
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
