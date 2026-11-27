//! Durable peer profiles and platform-protected credentials.

use lyteboard_protocol::peer::{NoiseKey, RouteId, validate_label, validate_lan_address};
use std::{collections::HashSet, fs::File, io, net::SocketAddr, path::Path};
#[cfg(any(target_os = "linux", target_os = "android"))]
mod unix;
#[cfg(target_os = "android")]
use rustix::{
    fs::{Mode, OFlags, mkdirat, openat},
    io::Errno,
};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use unix::*;
#[cfg(target_os = "android")]
use unix::{READ_FLAGS, check_private_dir};
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
use secret_service::{EncryptionType, blocking::SecretService};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::path::{Component, PathBuf};
use thiserror::Error;
#[cfg(target_os = "windows")]
pub use windows::*;
use x25519_dalek::{PublicKey, StaticSecret};
#[cfg(target_os = "linux")]
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

/// Open a durable LAN endpoint, creating its identity on first use. Call on a blocking thread.
#[cfg(target_os = "linux")]
pub fn open_endpoint(path: &Path) -> Result<Endpoint, StorageError> {
    let profile = Profile::open(path)?;
    let namespace = profile.namespace.clone();
    profile.load(&mut ServiceStore {
        namespace: &namespace,
    })
}

/// Open a profile under the application's Android no-backup directory.
#[cfg(target_os = "android")]
pub fn open_android_endpoint(
    directory: &Path,
    store: &mut impl CredentialStore,
) -> Result<Endpoint, StorageError> {
    if !directory.is_absolute() {
        return Err(StorageError::UnsafePath);
    }
    // Android app sandboxes cannot open every ancestor for reading. The parent
    // is supplied by Context.getNoBackupFilesDir inside this application.
    let parent = File::open(directory)?;
    let name = "peer-profile";
    match mkdirat(&parent, name, Mode::RWXU) {
        Ok(()) => parent.sync_all()?,
        Err(Errno::EXIST) => (),
        Err(error) => return Err(error.into()),
    }
    let dir = File::from(openat(
        &parent,
        name,
        READ_FLAGS | OFlags::DIRECTORY,
        Mode::empty(),
    )?);
    check_private_dir(&dir)?;
    let lock = lock_named(&dir, "profile.lock")?;
    Profile { dir, _lock: lock }.load(store)
}

struct Profile {
    dir: PrivateDir,
    #[cfg(target_os = "linux")]
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
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn open(path: &Path) -> Result<Self, StorageError> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(StorageError::UnsafePath);
        }
        let path: PathBuf = path.components().collect();
        #[cfg(target_os = "linux")]
        let namespace = path.to_str().ok_or(StorageError::UnsafePath)?.to_owned();
        let dir = open_private_dir(&path)?;
        let lock = lock_named(&dir, "profile.lock")?;
        Ok(Self {
            dir,
            #[cfg(target_os = "linux")]
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

    #[cfg(all(test, target_os = "linux"))]
    fn write(&self, name: &str, bytes: &[u8], limit: usize) -> Result<(), StorageError> {
        unix::write_private(&self.dir, name, bytes, limit)
    }
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

#[cfg(target_os = "linux")]
struct ServiceStore<'a> {
    namespace: &'a str,
}

#[cfg(target_os = "linux")]
impl ServiceStore<'_> {
    fn attributes(&self) -> HashMap<&str, &str> {
        HashMap::from([
            ("application", "lyteboard"),
            ("profile", self.namespace),
            ("schema", "identity-v1"),
        ])
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn credential_error(error: secret_service::Error) -> CredentialError {
    match error {
        secret_service::Error::Locked => CredentialError::Locked,
        secret_service::Error::Crypto(_) => CredentialError::Invalid,
        _ => CredentialError::Unavailable,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
