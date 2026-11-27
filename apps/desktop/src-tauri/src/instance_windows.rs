//! Per-user GUI instance lock. Activation of an existing window is deferred.
use lyteboard_core::{
    ipc::local::Endpoint,
    peer::storage::{StorageError, lock_named, open_private_dir},
    windows::PrivateDir,
};
pub(crate) struct Instance {
    _lock: std::fs::File,
    _directory: PrivateDir,
}
impl Instance {
    pub fn claim() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let directory = open_private_dir(&Endpoint::from_env()?.private_directory()?)?;
        match lock_named(&directory, "desktop.lock") {
            Ok(lock) => Ok(Some(Self {
                _lock: lock,
                _directory: directory,
            })),
            Err(StorageError::InUse) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn activated(&self) -> std::io::Result<bool> {
        std::future::pending().await
    }
}
