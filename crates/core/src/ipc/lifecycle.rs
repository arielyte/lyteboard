//! Start the installed sibling daemon without a shell.

use std::{
    env, io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    process::Command,
    time::{Instant, sleep, timeout_at},
};

use super::{Client, ClientError, local::EndpointError};

const START_TIMEOUT: Duration = Duration::from_secs(4);
const RETRY: Duration = Duration::from_millis(50);

#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    AlreadyRunning,
    Started,
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Runtime(#[from] EndpointError),
    #[error("Could not locate the Lyteboard executable: {0}")]
    Executable(#[source] io::Error),
    #[error(
        "Could not start {path}: {source}. Install lyteboardd alongside the Lyteboard executable."
    )]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Could not check lyteboardd: {0}")]
    Wait(#[source] io::Error),
    #[error("Lyteboardd exited during startup ({0}). Run lyteboardd in a terminal for details.")]
    EarlyExit(ExitStatus),
    #[error("Lyteboardd did not become ready. Run lyteboardd in a terminal for details.")]
    Timeout,
}

impl Client {
    pub async fn start_daemon(&self) -> Result<StartOutcome, StartError> {
        let deadline = Instant::now() + START_TIMEOUT;
        let _launch = loop {
            if self.daemon_is_ready(deadline).await? {
                return Ok(StartOutcome::AlreadyRunning);
            }
            match self.endpoint.lock_launcher() {
                Ok(lock) => break lock,
                Err(EndpointError::AlreadyRunning(_)) if Instant::now() < deadline => {
                    sleep(RETRY).await
                }
                Err(error) => return Err(error.into()),
            }
        };
        if self.daemon_is_ready(deadline).await? {
            return Ok(StartOutcome::AlreadyRunning);
        }

        #[cfg(target_os = "windows")]
        let daemon_name = "lyteboardd.exe";
        #[cfg(target_os = "linux")]
        let daemon_name = "lyteboardd";
        let path = env::current_exe()
            .map_err(StartError::Executable)?
            .with_file_name(daemon_name);
        let mut command = Command::new(&path);
        #[cfg(target_os = "linux")]
        command.process_group(0);
        #[cfg(target_os = "windows")]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| StartError::Spawn { path, source })?;
        let mut exited = None;
        let result = loop {
            match self.daemon_is_ready(deadline).await {
                Ok(true) => break Ok(StartOutcome::Started),
                Err(StartError::Timeout) => {
                    break Err(exited.map_or(StartError::Timeout, StartError::EarlyExit));
                }
                Err(error) => break Err(error),
                Ok(false) => (),
            }
            if exited.is_none() {
                match child.try_wait() {
                    Ok(status) => exited = status,
                    Err(error) => break Err(StartError::Wait(error)),
                }
            }
            sleep(RETRY).await;
        };
        if result.is_ok() {
            // Reap the child if it stops while the launcher is still running.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        } else {
            let _ = child.kill().await;
        }
        result
    }

    async fn daemon_is_ready(&self, deadline: Instant) -> Result<bool, StartError> {
        match timeout_at(deadline, self.ping()).await {
            Ok(Ok(())) => Ok(true),
            Ok(Err(ClientError::Offline)) if Instant::now() < deadline => Ok(false),
            Ok(Err(ClientError::Offline)) | Err(_) => Err(StartError::Timeout),
            Ok(Err(error)) => Err(error.into()),
        }
    }
}
