use std::process::ExitCode;

use clap::Parser;

#[cfg(target_os = "linux")]
mod history;
#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
mod preferences;
#[cfg(target_os = "linux")]
mod server;

#[derive(Parser)]
#[command(name = "lyteboardd", version = lyteboard_core::VERSION, about = "Lyteboard local daemon")]
struct Daemon {}

fn main() -> ExitCode {
    Daemon::parse();

    #[cfg(target_os = "linux")]
    if let Err(error) = server::run() {
        eprintln!("lyteboardd: {error}");
        return ExitCode::FAILURE;
    }

    #[cfg(target_os = "linux")]
    return ExitCode::SUCCESS;

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("lyteboardd: the daemon currently runs on Linux");
        ExitCode::FAILURE
    }
}
