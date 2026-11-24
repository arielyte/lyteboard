//! Unwired Linux profile draft retained for future persistence work.
//! Key material is stored in the user's Secret Service.

use std::{
    collections::HashMap,
    fs::{File, TryLockError},
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use lyteboard_protocol::peer::RouteId;
use rustix::{
    fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat},
    io::Errno,
    process::geteuid,
};
use secret_service::{EncryptionType, blocking::SecretService};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use super::{CredentialError, CredentialStore, Credentials, DeviceIdentity};

const VERSION: u16 = 1;
const IDENTITY_LIMIT: usize = 512;
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const LOCK_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CREATE)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Error)]
pub enum StorageError {
    #[error(
        "the data directory must be an absolute UTF-8 path with private ownership and permissions"
    )]
    UnsafePath,
    #[error("this data directory is already in use")]
    InUse,
    #[error("could not access the private data directory: {0}")]
    Io(#[from] io::Error),
    #[error("the stored profile format is invalid")]
    Invalid,
    #[error("the stored profile belongs to another device identity")]
    WrongIdentity,
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

impl From<Errno> for StorageError {
    fn from(error: Errno) -> Self {
        Self::Io(error.into())
    }
}

#[derive(Clone)]
pub struct Profile(Arc<ProfileInner>);

struct ProfileInner {
    dir: File,
    namespace: String,
    identity_lock: Mutex<()>,
    _lock: File,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    version: u16,
    route: RouteId,
    noise: [u8; 32],
}

impl Owner {
    fn from_identity(identity: &DeviceIdentity) -> Self {
        Self {
            version: VERSION,
            route: identity.route_id(),
            noise: identity.noise_key(),
        }
    }
}

impl Profile {
    /// Lock one data directory for exclusive use by a daemon.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(StorageError::UnsafePath);
        }
        let path: PathBuf = path.components().collect();
        let namespace = path.to_str().ok_or(StorageError::UnsafePath)?.to_owned();
        let dir = open_private_dir(&path)?;
        let lock = lock_named(&dir, "profile.lock")?;
        Ok(Self(Arc::new(ProfileInner {
            dir,
            namespace,
            identity_lock: Mutex::new(()),
            _lock: lock,
        })))
    }

    /// Load or explicitly provision this profile's identity. Call on a blocking thread.
    pub fn identity(&self) -> Result<DeviceIdentity, StorageError> {
        let _guard = self
            .0
            .identity_lock
            .lock()
            .map_err(|_| StorageError::Invalid)?;
        self.load_identity(&mut ServiceStore {
            namespace: &self.0.namespace,
        })
    }

    fn load_identity(
        &self,
        store: &mut impl CredentialStore,
    ) -> Result<DeviceIdentity, StorageError> {
        let anchor = self
            .read("identity.json", IDENTITY_LIMIT)?
            .map(|bytes| serde_json::from_slice::<Owner>(&bytes).map_err(|_| StorageError::Invalid))
            .transpose()?;
        let identity = match store.load()? {
            Some(credentials) => DeviceIdentity::from_credentials(credentials)?,
            None if anchor.is_some() => return Err(CredentialError::Missing.into()),
            None => {
                let credentials = Credentials::generate()?;
                store.store(&credentials)?;
                DeviceIdentity::from_credentials(credentials)?
            }
        };
        let actual = Owner::from_identity(&identity);
        match anchor {
            Some(anchor) if anchor.version != VERSION => return Err(StorageError::Invalid),
            Some(anchor) if anchor != actual => return Err(StorageError::WrongIdentity),
            Some(_) => (),
            None => self.write(
                "identity.json",
                &serde_json::to_vec(&actual).map_err(|_| StorageError::Invalid)?,
                IDENTITY_LIMIT,
            )?,
        }
        Ok(identity)
    }

    fn read(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>, StorageError> {
        let fd = match openat(&self.0.dir, name, READ_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file = File::from(fd);
        check_private_file(&file)?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(StorageError::Invalid);
        }
        Ok(Some(bytes))
    }

    fn write(&self, name: &str, bytes: &[u8], limit: usize) -> Result<(), StorageError> {
        if bytes.len() > limit {
            return Err(StorageError::Invalid);
        }
        let mut suffix = [0; 16];
        getrandom::fill(&mut suffix).map_err(|_| CredentialError::EntropyUnavailable)?;
        let temporary = format!(".{:032x}.tmp", u128::from_ne_bytes(suffix));
        let fd = openat(
            &self.0.dir,
            temporary.as_str(),
            LOCK_FLAGS | OFlags::EXCL,
            PRIVATE_FILE,
        )?;
        let mut file = File::from(fd);
        let result = (|| {
            check_private_file(&file)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            renameat(&self.0.dir, temporary.as_str(), &self.0.dir, name)?;
            self.0.dir.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&self.0.dir, temporary.as_str(), AtFlags::empty());
        }
        result
    }
}

fn open_private_dir(path: &Path) -> Result<File, StorageError> {
    let uid = geteuid().as_raw();
    let mut dir = File::open("/")?;
    for part in path.components().skip(1) {
        let parent = dir.metadata()?;
        if (parent.uid() != uid && parent.uid() != 0)
            || (parent.mode() & 0o022 != 0 && parent.mode() & 0o1000 == 0)
        {
            return Err(StorageError::UnsafePath);
        }
        let Component::Normal(name) = part else {
            return Err(StorageError::UnsafePath);
        };
        let flags = READ_FLAGS | OFlags::DIRECTORY;
        let next = match openat(&dir, name, flags, Mode::empty()) {
            Err(Errno::NOENT) => {
                match mkdirat(&dir, name, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => (),
                    Err(error) => return Err(error.into()),
                }
                openat(&dir, name, flags, Mode::empty())?
            }
            other => other?,
        };
        dir = File::from(next);
    }
    let metadata = dir.metadata()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(StorageError::UnsafePath);
    }
    Ok(dir)
}

fn lock_named(dir: &File, name: &str) -> Result<File, StorageError> {
    let lock = File::from(openat(dir, name, LOCK_FLAGS, PRIVATE_FILE)?);
    check_private_file(&lock)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(StorageError::InUse),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn check_private_file(file: &File) -> Result<(), StorageError> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(StorageError::UnsafePath);
    }
    Ok(())
}

struct ServiceStore<'a> {
    namespace: &'a str,
}

impl ServiceStore<'_> {
    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("application", "lyteboard"),
            ("profile", self.namespace),
            ("schema", "identity-v1"),
        ])
    }
}

impl CredentialStore for ServiceStore<'_> {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        let service = SecretService::connect(EncryptionType::Dh).map_err(credential_error)?;
        let results = service
            .search_items(self.attributes())
            .map_err(credential_error)?;
        if !results.locked.is_empty() {
            return Err(CredentialError::Locked);
        }
        let item = match results.unlocked.as_slice() {
            [] => return Ok(None),
            [item] => item,
            _ => return Err(CredentialError::Invalid),
        };
        let session = service.get_collection_by_alias("session").ok();
        let item_path = item.item_path.as_str();
        if [
            Some("/org/freedesktop/secrets/collection/session"),
            session.as_ref().map(|entry| entry.collection_path.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|prefix| {
            item_path
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            return Err(CredentialError::Invalid);
        }
        let secret = Zeroizing::new(item.get_secret().map_err(credential_error)?);
        let bytes: &[u8; 64] = secret
            .as_slice()
            .try_into()
            .map_err(|_| CredentialError::Invalid)?;
        let (noise, route) = bytes.split_at(32);
        Ok(Some(Credentials::from_seeds(
            noise.try_into().map_err(|_| CredentialError::Invalid)?,
            route.try_into().map_err(|_| CredentialError::Invalid)?,
        )))
    }

    fn store(&mut self, credentials: &Credentials) -> Result<(), CredentialError> {
        let service = SecretService::connect(EncryptionType::Dh).map_err(credential_error)?;
        let found = service
            .search_items(self.attributes())
            .map_err(credential_error)?;
        if !found.locked.is_empty() {
            return Err(CredentialError::Locked);
        }
        if !found.unlocked.is_empty() {
            return Err(CredentialError::Invalid);
        }
        let collection = service.get_default_collection().map_err(credential_error)?;
        let path = collection.collection_path.as_str();
        if path == "/org/freedesktop/secrets/collection/session"
            || service
                .get_collection_by_alias("session")
                .is_ok_and(|entry| entry.collection_path == collection.collection_path)
        {
            return Err(CredentialError::Unavailable);
        }
        collection.ensure_unlocked().map_err(credential_error)?;
        let mut bytes = Zeroizing::new([0u8; 64]);
        credentials.with_seeds(|noise, route| {
            bytes[..32].copy_from_slice(noise);
            bytes[32..].copy_from_slice(route);
        });
        collection
            .create_item(
                "Lyteboard device identity",
                self.attributes(),
                &bytes[..],
                false,
                "application/octet-stream",
            )
            .map_err(credential_error)?;
        Ok(())
    }
}

fn credential_error(error: secret_service::Error) -> CredentialError {
    match error {
        secret_service::Error::Locked => CredentialError::Locked,
        secret_service::Error::Crypto(_) => CredentialError::Invalid,
        _ => CredentialError::Unavailable,
    }
}
