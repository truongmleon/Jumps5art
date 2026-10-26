use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_storage::{MetadataReport, initialize, inspect};

#[derive(Parser)]
#[command(
    version,
    about = "Jumpstart local metadata development tools; no network listener"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

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

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "error: {error}");
            ExitCode::FAILURE
        }
    }
}

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
