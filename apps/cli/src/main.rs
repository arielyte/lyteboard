use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[cfg(target_os = "linux")]
mod clipboard;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    Network {
        #[command(subcommand)]
        command: network::NetworkCommand,
    },
    /// Pair with another device.
    #[cfg(target_os = "linux")]
    Pair {
        #[command(subcommand)]
        command: network::PairCommand,
    },
    /// Manage paired devices.
    #[cfg(target_os = "linux")]
    Devices {
        #[command(subcommand)]
        command: network::DeviceCommand,
    },
    /// Send text and wait for an authenticated receipt.
    #[cfg(target_os = "linux")]
    Send {
        #[arg(value_parser = network::parse_key)]
        device: [u8; 32],
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        text: Option<String>,
        /// Read UTF-8 text from standard input, up to 1 MiB.
        #[arg(long)]
        stdin: bool,
    },
    /// Send or apply text from the Linux clipboard.
    #[cfg(target_os = "linux")]
    Clipboard {
        #[command(subcommand)]
        command: clipboard::ClipboardCommand,
    },
    /// Show recent received test messages.
    #[cfg(target_os = "linux")]
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
