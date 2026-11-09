//! Read-only Worlds CLI for a protected local Jumpstart daemon.
//!
//! Status reports the available catalog evidence. The backup list shows saved
//! recovery index rows. The CLI sends commands to the daemon without opening
//! SQLite, touching the world directory, or spawning Java.

use std::{error::Error, io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_server::SOCKET_PATH;

/// One Worlds command and the protected local socket it should use.
#[derive(Parser)]
#[command(version, about = "Inspect Worlds through the local Jumpstart daemon")]
struct Cli {
    /// Protected socket exposed by the running Jumpstart daemon.
    #[arg(long, global = true, default_value = SOCKET_PATH)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// Read-only commands available before world mutation services are configured.
#[derive(Subcommand)]
enum Command {
    /// Read the catalog's evidence about the managed Worlds server.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Inspect recorded Worlds recovery points.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
}

/// Commands about the independent Worlds recovery catalog's SQLite index.
#[derive(Subcommand)]
enum BackupCommand {
    /// List indexed recovery points in the captured catalog.
    List {
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=i64::from(jumpstart_storage::MAX_WORLDS_INDEX_ROWS)))]
        limit: u16,
        #[arg(long)]
        json: bool,
    },
}

/// Run one command and use the Worlds exit code for unavailable resources.
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "error: {error}");
            ExitCode::from(4)
        }
    }
}

/// Dispatch the chosen command and render its scoped daemon reply.
///
/// Returns an error if the daemon or evidence is unavailable. A status reply
/// with an unassessed lifecycle is still printed before returning that error.
fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        use jumpstart_server::{
            local_admin::{Command as Request, call},
            print_report,
        };

        match cli.command {
            Command::Status { json } => {
                let report = call(&cli.socket, Request::WorldsStatus)?;
                print_report(&report, "Worlds catalog status:", json)?;
                if report["lifecycle"].is_null() {
                    return Err("live Worlds state is unavailable; see the catalog report".into());
                }
            }
            Command::Backup {
                command: BackupCommand::List { limit, json },
            } => {
                let report = call(&cli.socket, Request::WorldsBackupList { limit })?;
                print_report(&report, "Indexed Worlds recovery points:", json)?;
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cli;
        Err("Worlds requires a Linux Jumpstart daemon".into())
    }
}
