use std::io;

use lyteboard_core::ipc::{
    Client, ClientError,
    lifecycle::{StartError, StartOutcome},
};
use thiserror::Error;

use super::{Command, DaemonCommand};

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("{0}")]
    Network(String),
    #[error("terminal I/O failed: {0}")]
    Terminal(#[source] io::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Start(#[from] StartError),
}

#[tokio::main(flavor = "current_thread")]
pub async fn run(command: Command) -> Result<(), CommandError> {
    let client = Client::from_env()?;
    match command {
        Command::Status => {
            let status = client.status().await?;
            println!(
                "Daemon running (PID {}, version {}, uptime {}s, IPC v{}, {} connections accepted)",
                status.pid,
                status.version,
                status.uptime_seconds,
                status.protocol_version,
                status.connections_accepted,
            );
        }
        Command::Ping => {
            client.ping().await?;
            println!("Pong");
        }
        Command::Version => println!("{}", client.version().await?),
        Command::Daemon {
            command: DaemonCommand::Start,
        } => match client.start_daemon().await? {
            StartOutcome::AlreadyRunning => println!("Daemon already running"),
            StartOutcome::Started => println!("Daemon started"),
        },
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => {
            client.shutdown().await?;
            println!("Daemon stopped");
        }
        Command::Clipboard { command } => super::clipboard::run(&client, command).await?,
        Command::History { command } => super::history::run(&client, command).await?,
        command => super::network::run(&client, command).await?,
    }
    Ok(())
}
