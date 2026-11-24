//! Linux peer profiles and Secret Service credentials.
//! Key material is stored in the user's Secret Service.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, TryLockError},
    io::{self, Read, Write},
    net::SocketAddr,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use lyteboard_protocol::peer::{NoiseKey, RouteId, validate_label, validate_lan_address};
use rustix::{
    fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat},
    io::Errno,
    process::geteuid,
};
use secret_service::{EncryptionType, blocking::SecretService};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::{
    CredentialError, CredentialStore, Credentials, DeviceIdentity, Endpoint, State, TrustedDevice,
    trust::{MAX_ACTIVE, MAX_RECORDS, Registry, Status},
};

const VERSION: u16 = 1;
const IDENTITY_LIMIT: usize = 512;
const TRUST_LIMIT: usize = 128 * 1024;
const IDENTITY_FILE: &str = "identity.json";
const TRUST_FILE: &str = "trusted-devices.json";
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
    #[error("the trusted-device file is missing from an existing profile")]
    MissingTrust,
    #[error("profile sync failed after replacement; reopen the profile before use")]
    WriteUncertain,
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

/// Open a durable LAN endpoint, creating its identity on first use. Call on a blocking thread.
pub fn open_endpoint(path: &Path) -> Result<Endpoint, StorageError> {
    let profile = Profile::open(path)?;
    let namespace = profile.namespace.clone();
    profile.load(&mut ServiceStore {
        namespace: &namespace,
    })
}

struct Profile {
    dir: File,
    namespace: String,
    _lock: File,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
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
    fn open(path: &Path) -> Result<Self, StorageError> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(StorageError::UnsafePath);
        }
        let path: PathBuf = path.components().collect();
        let namespace = path.to_str().ok_or(StorageError::UnsafePath)?.to_owned();
        let dir = open_private_dir(&path)?;
        let lock = lock_named(&dir, "profile.lock")?;
        Ok(Self {
            dir,
            namespace,
            _lock: lock,
        })
    }

    fn load(self, store: &mut impl CredentialStore) -> Result<Endpoint, StorageError> {
        let anchor: Option<Owner> = self.read_json(IDENTITY_FILE, IDENTITY_LIMIT)?;
        let snapshot: Option<TrustSnapshot> = self.read_json(TRUST_FILE, TRUST_LIMIT)?;
        if anchor
            .as_ref()
            .is_some_and(|owner| owner.version != VERSION)
        {
            return Err(StorageError::Invalid);
        }
        if anchor.is_some() && snapshot.is_none() {
            return Err(StorageError::MissingTrust);
        }
        let identity = match store.load()? {
            Some(credentials) => DeviceIdentity::from_credentials(credentials)?,
            None if anchor.is_some() || snapshot.is_some() => {
                return Err(CredentialError::Missing.into());
            }
            None => {
                let credentials = Credentials::generate()?;
                store.store(&credentials)?;
                DeviceIdentity::from_credentials(credentials)?
            }
        };
        let owner = Owner::from_identity(&identity);
        if anchor.as_ref().is_some_and(|anchor| anchor != &owner) {
            return Err(StorageError::WrongIdentity);
        }
        let file = TrustFile {
            profile: self,
            owner,
        };
        let records = if let Some(snapshot) = snapshot {
            snapshot.into_records(&file.owner)?
        } else {
            file.save(&[])?;
            Vec::new()
        };
        // Write trust first. A crash here leaves a recoverable snapshot tied to the keyring item.
        if anchor.is_none() {
            file.profile
                .write_json(IDENTITY_FILE, &file.owner, IDENTITY_LIMIT)?;
        }

        let mut endpoint = Endpoint::new_lan(identity);
        endpoint.state = std::sync::Mutex::new(State {
            trust: Registry::persistent(file, records),
            ..State::default()
        });
        Ok(endpoint)
    }

    fn read_json<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Option<T>, StorageError> {
        read_private_json(&self.dir, name, limit)
    }

    fn write_json(
        &self,
        name: &str,
        value: &impl Serialize,
        limit: usize,
    ) -> Result<(), StorageError> {
        write_private_json(&self.dir, name, value, limit)
    }

    #[cfg(test)]
    fn write(&self, name: &str, bytes: &[u8], limit: usize) -> Result<(), StorageError> {
        write_private(&self.dir, name, bytes, limit)
    }
}

/// Read a bounded JSON file from an already checked private directory.
pub fn read_private_json<T: serde::de::DeserializeOwned>(
    dir: &File,
    name: &str,
    limit: usize,
) -> Result<Option<T>, StorageError> {
    read_private(dir, name, limit)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| StorageError::Invalid))
        .transpose()
}

/// Atomically replace a bounded JSON file inside an already checked private directory.
pub fn write_private_json(
    dir: &File,
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

fn read_private(dir: &File, name: &str, limit: usize) -> Result<Option<Vec<u8>>, StorageError> {
    let fd = match openat(dir, name, READ_FLAGS, Mode::empty()) {
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

fn write_private(dir: &File, name: &str, bytes: &[u8], limit: usize) -> Result<(), StorageError> {
    if bytes.len() > limit {
        return Err(StorageError::Invalid);
    }
    let mut suffix = [0; 16];
    getrandom::fill(&mut suffix).map_err(|_| CredentialError::EntropyUnavailable)?;
    let temporary = format!(".{:032x}.tmp", u128::from_ne_bytes(suffix));
    let fd = openat(
        dir,
        temporary.as_str(),
        LOCK_FLAGS | OFlags::EXCL,
        PRIVATE_FILE,
    )?;
    let mut file = File::from(fd);
    let result = (|| {
        check_private_file(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        renameat(dir, temporary.as_str(), dir, name)?;
        dir.sync_all().map_err(|_| StorageError::WriteUncertain)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(dir, temporary.as_str(), AtFlags::empty());
    }
    result
}

pub(super) struct TrustFile {
    profile: Profile,
    owner: Owner,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustSnapshot {
    version: u16,
    owner: Owner,
    devices: Vec<StoredDevice>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDevice {
    noise_key: NoiseKey,
    label: String,
    last_known_address: Option<SocketAddr>,
    revoked: bool,
}

impl TrustSnapshot {
    fn into_records(self, owner: &Owner) -> Result<Vec<(TrustedDevice, Status)>, StorageError> {
        if self.version != VERSION
            || self.owner.version != VERSION
            || self.devices.len() > MAX_RECORDS
            || self.devices.iter().filter(|device| !device.revoked).count() > MAX_ACTIVE
        {
            return Err(StorageError::Invalid);
        }
        if self.owner != *owner {
            return Err(StorageError::WrongIdentity);
        }
        let mut seen = HashSet::new();
        // Any fixed clamped scalar detects non-contributory public keys.
        let validation_scalar = StaticSecret::from([1; 32]);
        self.devices
            .into_iter()
            .map(|device| {
                if device.noise_key == owner.noise
                    || !seen.insert(device.noise_key)
                    || validate_label(&device.label).is_err()
                    || device
                        .last_known_address
                        .is_some_and(|address| validate_lan_address(address).is_err())
                    || !validation_scalar
                        .diffie_hellman(&PublicKey::from(device.noise_key))
                        .was_contributory()
                {
                    return Err(StorageError::Invalid);
                }
                Ok((
                    TrustedDevice {
                        route: device.noise_key,
                        noise_key: device.noise_key,
                        label: device.label,
                        last_known_address: device.last_known_address,
                    },
                    if device.revoked {
                        Status::Revoked
                    } else {
                        Status::Active
                    },
                ))
            })
            .collect()
    }
}

impl TrustFile {
    pub(super) fn save(&self, records: &[(TrustedDevice, Status)]) -> Result<(), StorageError> {
        let devices = records
            .iter()
            .filter(|(_, status)| !matches!(status, Status::Pending(_)))
            .map(|(device, status)| {
                if device.route != device.noise_key {
                    return Err(StorageError::Invalid);
                }
                Ok(StoredDevice {
                    noise_key: device.noise_key,
                    label: device.label.clone(),
                    last_known_address: device.last_known_address,
                    revoked: *status == Status::Revoked,
                })
            })
            .collect::<Result<_, _>>()?;
        self.profile.write_json(
            TRUST_FILE,
            &TrustSnapshot {
                version: VERSION,
                owner: self.owner.clone(),
                devices,
            },
            TRUST_LIMIT,
        )
    }
}

/// Open or create a private absolute directory without following symlinks.
pub fn open_private_dir(path: &Path) -> Result<File, StorageError> {
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(StorageError::UnsafePath);
    }
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
                    Ok(()) => dir.sync_all()?,
                    Err(Errno::EXIST) => (),
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

/// Exclusively lock a private file inside a previously checked directory.
pub fn lock_named(dir: &File, name: &str) -> Result<File, StorageError> {
    let lock = File::from(openat(dir, name, LOCK_FLAGS, PRIVATE_FILE)?);
    check_private_file(&lock)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(StorageError::InUse),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Require a private regular file owned by the current effective user.
pub fn check_private_file(file: &File) -> Result<(), StorageError> {
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
        if is_session_path(&service, item.item_path.as_str())? {
            return Err(CredentialError::Invalid);
        }
        item.ensure_unlocked().map_err(credential_error)?;
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
        if is_session_path(&service, collection.collection_path.as_str())? {
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

fn is_session_path<'a>(
    service: &'a SecretService<'a>,
    path: &str,
) -> Result<bool, CredentialError> {
    let belongs_to = |collection: &str| {
        path.strip_prefix(collection)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('/'))
    };
    if belongs_to("/org/freedesktop/secrets/collection/session") {
        return Ok(true);
    }
    match service.get_collection_by_alias("session") {
        Ok(collection) => Ok(belongs_to(collection.collection_path.as_str())),
        Err(secret_service::Error::NoResult) => Ok(false),
        Err(error) => Err(credential_error(error)),
    }
}

fn credential_error(error: secret_service::Error) -> CredentialError {
    match error {
        secret_service::Error::Locked => CredentialError::Locked,
        secret_service::Error::Crypto(_) => CredentialError::Invalid,
        _ => CredentialError::Unavailable,
    }
}

#[cfg(test)]
mod tests;
