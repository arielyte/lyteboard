//! Per-user local named pipes. All traffic follows the shared bounded IPC protocol.
use super::ClientError;
pub use super::framing::{FrameError, read_bulk, receive, send, write_bulk};
pub(super) use super::transport::{download, request, upload};
use crate::{
    peer::storage::{StorageError, lock_named},
    windows::{self, PrivateDir},
};
use sha2::{Digest, Sha256};
use std::{fs::File, io, path::PathBuf};
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeServer},
    sync::Mutex,
    time::{Duration, Instant, sleep},
};
pub type LocalStream = NamedPipeServer;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
pub const BULK_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("a Lyteboard instance already owns {0}")]
    AlreadyRunning(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}
pub struct Endpoint {
    directory: PathBuf,
}
impl Endpoint {
    pub fn from_env() -> Result<Self, EndpointError> {
        // Optional absolute override isolates development/test instances.
        Self::new(
            std::env::var_os("LYTEBOARD_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or(windows::app_data()?.join("runtime")),
        )
    }
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, EndpointError> {
        let directory = path.into();
        if !directory.is_absolute() {
            return Err(io::Error::other("runtime directory must be absolute").into());
        }
        Ok(Self { directory })
    }
    pub fn private_directory(&self) -> Result<PathBuf, EndpointError> {
        PrivateDir::open(&self.directory)?;
        Ok(self.directory.clone())
    }
    fn private(&self) -> Result<PrivateDir, EndpointError> {
        Ok(PrivateDir::open(&self.directory)?)
    }
    pub fn pipe_name(&self) -> Result<String, EndpointError> {
        let directory = self.private()?;
        // canonicalize normalizes path spelling/case through the filesystem, avoiding duplicate names.
        let path = std::fs::canonicalize(directory.path())?;
        let hash = Sha256::digest(path.to_string_lossy().to_lowercase().as_bytes());
        Ok(format!(
            r"\\.\pipe\lyteboard-{}-{}",
            windows::user_sid()?,
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ))
    }
    pub(in crate::ipc) fn lock_launcher(&self) -> Result<File, EndpointError> {
        self.lock("launcher.lock")
    }
    fn lock(&self, name: &str) -> Result<File, EndpointError> {
        let dir = self.private()?;
        match lock_named(&dir, name) {
            Ok(f) => Ok(f),
            Err(StorageError::InUse) => {
                Err(EndpointError::AlreadyRunning(self.directory.join(name)))
            }
            Err(e) => Err(e.into()),
        }
    }
}
struct Listener {
    next: NamedPipeServer,
}
pub struct ServerSocket {
    listener: Mutex<Listener>,
    name: String,
    _directory: PrivateDir,
    _lock: File,
}
impl ServerSocket {
    pub async fn bind(endpoint: &Endpoint) -> Result<Self, EndpointError> {
        let directory = endpoint.private()?;
        let lock = endpoint.lock("daemon.lock")?;
        let name = endpoint.pipe_name()?;
        let next = windows::pipe_server(&name, true)?;
        Ok(Self {
            listener: Mutex::new(Listener { next }),
            name,
            _directory: directory,
            _lock: lock,
        })
    }
    pub async fn accept(&self) -> io::Result<LocalStream> {
        let mut listener = self.listener.lock().await;
        listener.next.connect().await?;
        // Keep a listening instance alive continuously; never relinquish the pipe name.
        let next = windows::pipe_server(&self.name, false)?;
        Ok(std::mem::replace(&mut listener.next, next))
    }
}
pub fn authenticate(stream: &LocalStream) -> io::Result<()> {
    windows::validate_pipe_peer(stream, true)
}
pub(super) async fn connect(
    endpoint: &Endpoint,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, ClientError> {
    let name = endpoint.pipe_name()?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        // SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION prevents server impersonation.
        match ClientOptions::new()
            .security_qos_flags(0x00100000 | 0x00010000)
            .open(&name)
        {
            Ok(client) => {
                windows::validate_pipe_peer(&client, false).map_err(ClientError::Authentication)?;
                return Ok(client);
            }
            Err(e) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                sleep(Duration::from_millis(20)).await
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ClientError::Offline),
            Err(e) => return Err(ClientError::Connect(e)),
        }
    }
}
