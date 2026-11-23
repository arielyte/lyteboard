use std::{
    env,
    fs::{self, DirBuilder, File, Metadata, Permissions, TryLockError},
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use rustix::{
    fs::{Mode, OFlags},
    process::geteuid,
};
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    time::timeout,
};

use super::REQUEST_TIMEOUT;

const APP_DIR: &str = "lyteboard";
const SOCKET: &str = "daemon.sock";
const LOCK: &str = "daemon.lock";

#[derive(Debug, Error)]
pub enum EndpointError {
    #[error("XDG_RUNTIME_DIR is unset. Use a private runtime directory owned by your user")]
    MissingRuntimeDir,
    #[error("XDG_RUNTIME_DIR must be an absolute path without parent components: {0}")]
    InvalidRuntimeDir(PathBuf),
    #[error("unsafe IPC path {path}: expected {expected}")]
    UnsafePath {
        path: PathBuf,
        expected: &'static str,
    },
    #[error("a Lyteboard daemon or listener already owns {0}")]
    AlreadyRunning(PathBuf),
    #[error("could not probe existing socket {0} before the timeout")]
    ProbeTimeout(PathBuf),
    #[error("could not {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub struct Endpoint {
    runtime_dir: PathBuf,
}

impl Endpoint {
    pub fn from_env() -> Result<Self, EndpointError> {
        let path = env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .ok_or(EndpointError::MissingRuntimeDir)?;
        Self::new(path)
    }

    pub fn new(path: impl Into<PathBuf>) -> Result<Self, EndpointError> {
        let runtime_dir = path.into();
        if !runtime_dir.is_absolute()
            || runtime_dir
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(EndpointError::InvalidRuntimeDir(runtime_dir));
        }

        // Strip a trailing slash or dot so symlink_metadata cannot follow the final component.
        Ok(Self {
            runtime_dir: runtime_dir.components().collect(),
        })
    }

    fn app_dir(&self) -> Result<PathBuf, EndpointError> {
        let metadata = fs::symlink_metadata(&self.runtime_dir)
            .map_err(|error| path_error("inspect runtime directory", &self.runtime_dir, error))?;
        validate_directory(&self.runtime_dir, &metadata)?;
        Ok(self.runtime_dir.join(APP_DIR))
    }

    pub(super) fn existing_socket(&self) -> Result<Option<PathBuf>, EndpointError> {
        let dir = self.app_dir()?;
        let Some(metadata) = metadata_if_exists(&dir)? else {
            return Ok(None);
        };
        validate_directory(&dir, &metadata)?;

        let path = dir.join(SOCKET);
        let Some(metadata) = metadata_if_exists(&path)? else {
            return Ok(None);
        };
        validate_socket(&path, &metadata)?;
        Ok(Some(path))
    }
}

pub struct ServerSocket {
    listener: UnixListener,
    _socket: OwnedSocket,
    // Keep the lock until the listener is closed and its socket path has been cleaned up.
    _lock: File,
}

impl ServerSocket {
    pub async fn bind(endpoint: &Endpoint) -> Result<Self, EndpointError> {
        let dir = endpoint.app_dir()?;
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(path_error("create IPC directory", &dir, error)),
        }
        let metadata = fs::symlink_metadata(&dir)
            .map_err(|error| path_error("inspect IPC directory", &dir, error))?;
        validate_directory(&dir, &metadata)?;
        let lock = lock_server(&dir.join(LOCK))?;

        let path = dir.join(SOCKET);
        remove_stale_socket(&path).await?;
        let listener = UnixListener::bind(&path)
            .map_err(|error| path_error("bind IPC socket", &path, error))?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| path_error("inspect bound socket", &path, error))?;
        validate_socket(&path, &metadata)?;
        let socket = OwnedSocket {
            path,
            identity: identity(&metadata),
        };
        fs::set_permissions(&socket.path, Permissions::from_mode(0o600))
            .map_err(|error| path_error("set IPC socket permissions", &socket.path, error))?;

        Ok(Self {
            listener,
            _socket: socket,
            _lock: lock,
        })
    }

    pub async fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        Ok(stream)
    }
}

fn lock_server(path: &Path) -> Result<File, EndpointError> {
    let flags =
        OFlags::CREATE | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let file: File = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR)
        .map_err(|error| path_error("open daemon lock", path, error.into()))?
        .into();
    let metadata = file
        .metadata()
        .map_err(|error| path_error("inspect daemon lock", path, error))?;
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(unsafe_path(
            path,
            "a regular file owned by your user with mode 0600",
        ));
    }

    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(EndpointError::AlreadyRunning(path.to_owned())),
        Err(TryLockError::Error(error)) => Err(path_error("lock daemon", path, error)),
    }
}

async fn remove_stale_socket(path: &Path) -> Result<(), EndpointError> {
    let Some(metadata) = metadata_if_exists(path)? else {
        return Ok(());
    };
    validate_socket(path, &metadata)?;
    match timeout(REQUEST_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
            if !unlink_owned_socket(path, identity(&metadata))
                .map_err(|error| path_error("remove stale socket", path, error))?
            {
                return Err(unsafe_path(
                    path,
                    "the same socket inspected during startup",
                ));
            }
            Ok(())
        }
        Ok(Err(error)) => Err(path_error("probe existing socket", path, error)),
        Ok(Ok(_)) => Err(EndpointError::AlreadyRunning(path.to_owned())),
        Err(_) => Err(EndpointError::ProbeTimeout(path.to_owned())),
    }
}

fn validate_directory(path: &Path, metadata: &Metadata) -> Result<(), EndpointError> {
    if !metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(unsafe_path(
            path,
            "a directory owned by your user with mode 0700",
        ));
    }

    Ok(())
}

fn validate_socket(path: &Path, metadata: &Metadata) -> Result<(), EndpointError> {
    if !metadata.file_type().is_socket() || metadata.uid() != geteuid().as_raw() {
        return Err(unsafe_path(path, "a Unix socket owned by your user"));
    }

    Ok(())
}

fn metadata_if_exists(path: &Path) -> Result<Option<Metadata>, EndpointError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(path_error("inspect IPC path", path, error)),
    }
}

fn unsafe_path(path: &Path, expected: &'static str) -> EndpointError {
    EndpointError::UnsafePath {
        path: path.to_owned(),
        expected,
    }
}

fn path_error(operation: &'static str, path: &Path, source: io::Error) -> EndpointError {
    EndpointError::Io {
        operation,
        path: path.to_owned(),
        source,
    }
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn unlink_owned_socket(path: &Path, expected_identity: (u64, u64)) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if validate_socket(path, &metadata).is_ok()
                && identity(&metadata) == expected_identity =>
        {
            fs::remove_file(path)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

struct OwnedSocket {
    path: PathBuf,
    identity: (u64, u64),
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if let Err(error) = unlink_owned_socket(&self.path, self.identity) {
            eprintln!(
                "could not clean up Lyteboard socket {}: {error}",
                self.path.display()
            );
        }
    }
}
