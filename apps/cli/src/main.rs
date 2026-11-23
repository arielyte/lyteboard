use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[cfg(target_os = "linux")]
mod linux;

#[derive(Parser)]
#[command(name = "lyteboard", version = lyteboard_core::VERSION, about = "Lyteboard command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the daemon's current status.
    Status,
    /// Check that the daemon responds.
    Ping,
    /// Print the running daemon's version.
    Version,
    /// Start or stop the local daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start a detached daemon unless one is already running.
    Start,
    /// Ask the daemon to shut down and release its IPC socket.
    Stop,
}

fn main() -> ExitCode {
    let Cli { command } = Cli::parse();
    #[cfg(target_os = "linux")]
    let result = linux::run(command);
    #[cfg(not(target_os = "linux"))]
    let result = {
        let _ = command;
        Err::<(), _>(lyteboard_core::ipc::ClientError::UnsupportedPlatform)
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lyteboard: {error}");
            ExitCode::FAILURE
        }
    }
}
