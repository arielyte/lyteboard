use super::*;
pub use lyteboard_windows::{PrivateDir, check_private_file};
use std::io::Read;
use zeroize::Zeroizing;

pub fn open_private_dir(path: &Path) -> Result<PrivateDir, StorageError> {
    Ok(PrivateDir::open(path)?)
}
pub fn lock_named(dir: &PrivateDir, name: &str) -> Result<File, StorageError> {
    let file = dir.open_file(name, true)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(StorageError::InUse),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}
pub fn read_private_json<T: serde::de::DeserializeOwned>(
    dir: &PrivateDir,
    name: &str,
    limit: usize,
) -> Result<Option<T>, StorageError> {
    read_private(dir, name, limit)?
        .map(|v| serde_json::from_slice(&v).map_err(|_| StorageError::Invalid))
        .transpose()
}
pub fn write_private_json(
    dir: &PrivateDir,
    name: &str,
    value: &impl Serialize,
    limit: usize,
) -> Result<(), StorageError> {
    write_private(
        dir,
        name,
        &serde_json::to_vec(value).map_err(|_| StorageError::Invalid)?,
        limit,
    )
}
fn read_private(
    dir: &PrivateDir,
    name: &str,
    limit: usize,
) -> Result<Option<Vec<u8>>, StorageError> {
    let file = match dir.open_file(name, false) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(StorageError::Invalid);
    }
    Ok(Some(bytes))
}
fn write_private(
    dir: &PrivateDir,
    name: &str,
    bytes: &[u8],
    limit: usize,
) -> Result<(), StorageError> {
    if bytes.len() > limit {
        return Err(StorageError::Invalid);
    }
    Ok(dir.replace(name, bytes)?)
}

pub fn open_endpoint(path: &Path) -> Result<Endpoint, StorageError> {
    let profile = Profile::open(path)?;
    // Separate handles keep the protected directory stable for credential operations.
    let mut store = DpapiStore(open_private_dir(path)?);
    profile.load(&mut store)
}
struct DpapiStore(PrivateDir);
impl CredentialStore for DpapiStore {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        let Some(blob) = read_private(&self.0, "identity.dpapi", 16384)
            .map_err(|_| CredentialError::Unavailable)?
        else {
            return Ok(None);
        };
        let bytes =
            lyteboard_windows::protect(&blob, true).map_err(|_| CredentialError::Unavailable)?;
        if bytes.len() != 64 {
            return Err(CredentialError::Invalid);
        }
        Ok(Some(Credentials::from_seeds(
            bytes[..32].try_into().unwrap(),
            bytes[32..].try_into().unwrap(),
        )))
    }
    fn store(&mut self, credentials: &Credentials) -> Result<(), CredentialError> {
        let mut bytes = Zeroizing::new([0u8; 64]);
        credentials.with_seeds(|noise, route| {
            bytes[..32].copy_from_slice(noise);
            bytes[32..].copy_from_slice(route);
        });
        let blob = lyteboard_windows::protect(bytes.as_slice(), false)
            .map_err(|_| CredentialError::Unavailable)?;
        // Never replace a credential that another profile opening may have created.
        // Profile::open already holds the exclusive profile lock.
        if self.load()?.is_some() {
            return Err(CredentialError::Invalid);
        }
        self.0
            .replace("identity.dpapi", &blob)
            .map_err(|_| CredentialError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dpapi_identity_is_stable_locked_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("profile");
        let first = open_endpoint(&path).unwrap();
        let key = first.noise_key();
        assert!(matches!(open_endpoint(&path), Err(StorageError::InUse)));
        drop(first);
        let second = open_endpoint(&path).unwrap();
        assert_eq!(second.noise_key(), key);
        drop(second);
        let encrypted = std::fs::read(path.join("identity.dpapi")).unwrap();
        assert_ne!(encrypted.len(), 64);
        std::fs::write(path.join("identity.dpapi"), b"corrupt DPAPI data").unwrap();
        assert!(open_endpoint(&path).is_err());
        assert_eq!(
            std::fs::read(path.join("identity.dpapi")).unwrap(),
            b"corrupt DPAPI data"
        );
        std::fs::remove_file(path.join("identity.dpapi")).unwrap();
        assert!(matches!(
            open_endpoint(&path),
            Err(StorageError::Credential(CredentialError::Missing))
        ));
        assert!(!path.join("identity.dpapi").exists());
    }
}
