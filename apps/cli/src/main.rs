use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod clipboard;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod history;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod network;

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
    /// Inspect or configure direct LAN networking.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Network {
        #[command(subcommand)]
        command: network::NetworkCommand,
    },
    /// Pair with another device.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Pair {
        #[command(subcommand)]
        command: network::PairCommand,
    },
    /// Manage paired devices.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Devices {
        #[command(subcommand)]
        command: network::DeviceCommand,
    },
    /// Send text and wait for an authenticated receipt.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Send {
        #[arg(value_parser = network::parse_key)]
        device: [u8; 32],
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        text: Option<String>,
        /// Read UTF-8 text from standard input, up to 1 MiB.
        #[arg(long)]
        stdin: bool,
    },
    /// Share or apply Linux clipboard text and images.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Clipboard {
        #[command(subcommand)]
        command: clipboard::ClipboardCommand,
    },
    /// Inspect or reuse opt-in local clipboard history.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    History {
        #[command(subcommand)]
        command: history::HistoryCommand,
    },
    /// Show recent received test messages.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Received {
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=16))]
        limit: u8,
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
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let result = linux::run(command);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
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
