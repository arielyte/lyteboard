//! Daemon-owned, profile-scoped preferences shared by the LAN and history workers.

use std::{
    collections::BTreeSet,
    fs::File,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use lyteboard_core::peer::storage::{
    StorageError, lock_named, open_private_dir, read_private_json, write_private_json,
};
use lyteboard_protocol::{
    ipc::network::{HistoryLimits, MAX_RECIPIENTS},
    peer::NoiseKey,
};
use serde::{Deserialize, Serialize};

const NAME: &str = "preferences.json";
const MAX_BYTES: usize = 8192;
const UNAVAILABLE: &str =
    "Saved preferences are unavailable. Check the private profile and restart the daemon.";

#[derive(Clone, Copy, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct HistorySettings {
    pub enabled: bool,
    pub limits: HistoryLimits,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Values {
    pub listen: Option<SocketAddr>,
    pub discovery: Option<bool>,
    pub send: bool,
    pub apply: bool,
    pub recipients: BTreeSet<NoiseKey>,
    pub history: Option<HistorySettings>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    values: Values,
}

impl Values {
    fn valid(&self) -> bool {
        self.recipients.len() <= MAX_RECIPIENTS
            && (!self.send || !self.recipients.is_empty())
            && self.history.is_none_or(|history| history.limits.is_valid())
    }
}

struct Directory {
    dir: File,
    _lock: File,
}

struct Store {
    directory: Option<Directory>,
    values: Values,
}

impl Store {
    fn open(path: &Path) -> Result<Self, StorageError> {
        let dir = open_private_dir(path)?;
        let lock = lock_named(&dir, "preferences.lock")?;
        let saved = read_private_json::<Snapshot>(&dir, NAME, MAX_BYTES)?;
        let values = match saved {
            Some(saved) if saved.version == 1 && saved.values.valid() => saved.values,
            Some(_) => return Err(StorageError::Invalid),
            None => Values::default(),
        };
        Ok(Self {
            directory: Some(Directory { dir, _lock: lock }),
            values,
        })
    }
}

#[derive(Clone)]
pub(super) struct Preferences {
    path: Option<PathBuf>,
    store: Arc<Mutex<Result<Store, &'static str>>>,
}

impl Preferences {
    pub fn load(path: Option<PathBuf>) -> Self {
        let store = match path.as_deref().map(Path::symlink_metadata) {
            Some(Ok(_)) => {
                Store::open(path.as_deref().expect("profile path")).map_err(|_| UNAVAILABLE)
            }
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => Err(UNAVAILABLE),
            _ => Ok(Store {
                directory: None,
                values: Values::default(),
            }),
        };
        Self {
            path,
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn values(&self) -> Result<Values, String> {
        self.store
            .lock()
            .expect("preferences store")
            .as_ref()
            .map(|store| store.values.clone())
            .map_err(|error| (*error).into())
    }

    pub fn update(&self, change: impl FnOnce(&mut Values)) -> Result<(), String> {
        let mut state = self.store.lock().expect("preferences store");
        let store = state.as_mut().map_err(|error| *error)?;
        if store.directory.is_none() {
            let path = self.path.as_deref().ok_or(UNAVAILABLE)?;
            *store = Store::open(path).map_err(|_| UNAVAILABLE)?;
        }
        let mut values = store.values.clone();
        change(&mut values);
        if !values.valid() {
            return Err("Invalid preference values.".into());
        }
        let dir = &store.directory.as_ref().expect("checked directory").dir;
        write_private_json(
            dir,
            NAME,
            &Snapshot {
                version: 1,
                values: values.clone(),
            },
            MAX_BYTES,
        )
        .map_err(|_| "Could not save preferences. Check the private profile and disk space.")?;
        store.values = values;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn defaults_and_updates_survive_restart_in_private_storage() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let settings = Preferences::load(Some(profile.clone()));
        let initial = settings.values().unwrap();
        assert!(!initial.send && !initial.apply && initial.history.is_none());
        assert!(!profile.exists());
        settings
            .update(|values| {
                values.send = true;
                values.recipients.insert([42; 32]);
                values.history = Some(HistorySettings {
                    enabled: true,
                    limits: HistoryLimits::default(),
                });
            })
            .unwrap();
        drop(settings);
        let file = profile.join(NAME);
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let saved = Preferences::load(Some(profile.clone()));
        let values = saved.values().unwrap();
        assert!(
            values.send && values.recipients.contains(&[42; 32]) && values.history.unwrap().enabled
        );
        assert!(saved.update(|values| values.recipients.clear()).is_err());
        drop(saved);
        std::fs::write(file, b"invalid profile").unwrap();
        assert!(Preferences::load(Some(profile)).values().is_err());
    }
}
