use std::process::ExitCode;

use clap::Parser;

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod history;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod network;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod preferences;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod server;

#[derive(Parser)]
#[command(name = "lyteboardd", version = lyteboard_core::VERSION, about = "Lyteboard local daemon")]
struct Daemon {}

fn main() -> ExitCode {
    Daemon::parse();

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Err(error) = server::run() {
        eprintln!("lyteboardd: {error}");
        return ExitCode::FAILURE;
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    return ExitCode::SUCCESS;

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        eprintln!("lyteboardd: the daemon currently runs on Linux");
        ExitCode::FAILURE
    }
}
