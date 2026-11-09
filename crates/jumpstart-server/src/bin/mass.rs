//! Starts Mass's development file browser or native mount.
//!
//! The helper connects to the Linux daemon as its service user. It never opens
//! the server database or the primary's managed object paths.

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use jumpstart_server::SOCKET_PATH;

#[cfg(target_os = "linux")]
#[path = "mass/mount.rs"]
mod mount;
#[cfg(target_os = "linux")]
#[path = "mass/web.rs"]
mod web;

/// The top-level entry point for Mass on the service host.
#[derive(Parser)]
#[command(version, about = "Mass file browser and virtual drive")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Frontends to the same protected Mass catalog.
#[derive(Subcommand)]
enum Command {
    /// Mount the same catalog as a read-only Linux filesystem.
    Mount {
        directory: PathBuf,
        #[arg(long, default_value = SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Start the private browser interface and print its session URL.
    Web {
        #[arg(long, default_value = SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Parse the selected frontend and report startup failures to stderr.
fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Web { socket } => start_web(socket),
        Command::Mount { directory, socket } => start_mount(directory, socket),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Start the local browser bridge on Linux.
fn start_web(socket: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    return web::run(socket);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        Err("the Mass browser preview requires the Linux daemon on this device".into())
    }
}

/// Mount the native Linux adapter or explain the unsupported platform.
fn start_mount(directory: PathBuf, socket: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    return mount::run(&directory, socket);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (directory, socket);
        Err("this Mass preview provides native mounts on Linux".into())
    }
}
