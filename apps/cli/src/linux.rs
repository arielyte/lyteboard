use std::{
    env, io,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command as ProcessCommand, ExitStatus, Stdio},
    time::Duration,
};

use lyteboard_core::ipc::{Client, ClientError};
use thiserror::Error;
use tokio::time::{sleep, timeout};

use super::{Command, DaemonCommand};

const START_TIMEOUT: Duration = Duration::from_secs(4);
const START_RETRY: Duration = Duration::from_millis(30);

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("{0}")]
    Network(String),
    #[error("terminal I/O failed: {0}")]
    Terminal(#[source] io::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("could not find the lyteboard executable: {0}")]
    Executable(#[source] io::Error),
    #[error("could not start {path}: {source}. Install lyteboardd alongside lyteboard")]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not check the launched daemon process: {0}")]
    Wait(#[source] io::Error),
    #[error("lyteboardd exited during startup ({0}). Run it in the foreground for details")]
    EarlyExit(ExitStatus),
    #[error("lyteboardd did not become ready in time. Run it in the foreground for details")]
    StartupTimeout,
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
        } => start(&client).await?,
        Command::Daemon {
            command: DaemonCommand::Stop,
        } => {
            client.shutdown().await?;
            println!("Daemon stopped");
        }
        command => super::network::run(&client, command).await?,
    }
    Ok(())
}

async fn start(client: &Client) -> Result<(), CommandError> {
    match client.ping().await {
        Ok(()) => {
            println!("Daemon already running");
            return Ok(());
        }
        Err(ClientError::Offline) => (),
        Err(error) => return Err(error.into()),
    }

    let path = env::current_exe()
        .map_err(CommandError::Executable)?
        .with_file_name("lyteboardd");
    let mut child = ProcessCommand::new(&path)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| CommandError::Spawn { path, source })?;

    let mut exit_status = None;
    let result = timeout(START_TIMEOUT, async {
        loop {
            match client.ping().await {
                Ok(()) => return Ok(()),
                Err(ClientError::Offline) => (),
                Err(error) => return Err(error.into()),
            }
            if exit_status.is_none() {
                // Another launch may hold the lock and still be publishing its socket.
                exit_status = child.try_wait().map_err(CommandError::Wait)?;
            }
            sleep(START_RETRY).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(exit_status.map_or(CommandError::StartupTimeout, CommandError::EarlyExit))
    });

    if result.is_ok() {
        println!("Daemon started");
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
