//! Activate the existing GUI for this private runtime profile.

use lyteboard_core::{
    ipc::linux::Endpoint,
    peer::storage::{StorageError, lock_named, open_private_dir},
};
use std::{
    fs::{self, File},
    io,
    os::unix::{fs::FileTypeExt, net::UnixDatagram},
    path::PathBuf,
    thread,
    time::Duration,
};

pub(crate) struct Instance {
    socket: UnixDatagram,
    path: PathBuf,
    _lock: File,
}

impl Instance {
    pub fn claim() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let path = Endpoint::from_env()?.private_directory()?;
        let directory = open_private_dir(&path)?;
        let path = path.join("desktop.sock");
        let lock = match lock_named(&directory, "desktop.lock") {
            Ok(lock) => lock,
            Err(StorageError::InUse) => {
                let sender = UnixDatagram::unbound()?;
                sender.set_nonblocking(true)?;
                // The first GUI can briefly hold the lock before it binds its socket.
                for _ in 0..20 {
                    if sender.send_to(b"open", &path).is_ok() {
                        return Ok(None);
                    }
                    thread::sleep(Duration::from_millis(30));
                }
                return Err(io::Error::other(
                    "Another Lyteboard window is starting. Try again shortly.",
                )
                .into());
            }
            Err(error) => return Err(error.into()),
        };
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => {
                return Err(
                    io::Error::other("Unexpected file at the Lyteboard desktop socket").into(),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let socket = UnixDatagram::bind(&path)?;
        socket.set_nonblocking(true)?;
        Ok(Some(Self {
            socket,
            path,
            _lock: lock,
        }))
    }

    pub async fn activated(&self) -> io::Result<bool> {
        let socket = tokio::net::UnixDatagram::from_std(self.socket.try_clone()?)?;
        let mut message = [0; 5];
        let size = socket.recv(&mut message).await?;
        Ok(&message[..size] == b"open")
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
