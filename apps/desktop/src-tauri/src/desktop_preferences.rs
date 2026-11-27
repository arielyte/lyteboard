use std::{
    env,
    fs::{self, File},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use lyteboard_core::peer::storage::{open_private_dir, read_private_json, write_private_json};
use serde::{Deserialize, Serialize};

const PREFS: &str = "desktop.json";
const LIMIT: usize = 1024;
const AUTOSTART: &str = "io.github.arielyte.lyteboard.desktop";
const MARKER: &str = "X-Lyteboard-Managed=true\n";

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Preferences {
    pub start_daemon: bool,
    pub close_to_tray: bool,
    pub minimize_to_tray: bool,
}

pub(crate) struct Store {
    directory: lyteboard_core::peer::storage::PrivateDir,
    autostart: PathBuf,
    pub choices: Preferences,
}

impl Store {
    pub fn open() -> Result<Self, String> {
        let root = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or("Set XDG_CONFIG_HOME to use desktop preferences.")?;
        Self::in_config(root).map_err(|_| "Could not read desktop preferences. Check the private Lyteboard configuration directory.".into())
    }

    fn in_config(root: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let directory = open_private_dir(&root.join("lyteboard"))?;
        let choices = read_private_json(&directory, PREFS, LIMIT)?.unwrap_or_default();
        Ok(Self {
            directory,
            autostart: root.join("autostart").join(AUTOSTART),
            choices,
        })
    }

    pub fn save(&mut self, choices: Preferences) -> Result<(), String> {
        write_private_json(&self.directory, PREFS, &choices, LIMIT).map_err(
            |_| "Could not save desktop preferences. Check directory permissions and free space.",
        )?;
        self.choices = choices;
        Ok(())
    }

    pub fn starts_at_login(&self) -> Result<bool, String> {
        managed_entry(&self.autostart)
            .map_err(|_| "The Lyteboard login launcher exists but could not be verified. Check the autostart folder.".into())
    }

    pub fn set_login(&self, enabled: bool) -> Result<(), String> {
        let binary = env::current_exe()
            .map_err(|_| "Could not locate the installed Lyteboard executable.")?;
        self.write_login(enabled, &binary)
            .map_err(
                |_| "Could not update the Lyteboard login launcher. Check the autostart folder.",
            )
            .map_err(Into::into)
    }

    fn write_login(&self, enabled: bool, binary: &Path) -> io::Result<()> {
        let parent = self.autostart.parent().expect("autostart parent exists");
        let existing = managed_entry(&self.autostart)?;
        if !enabled {
            if existing {
                fs::remove_file(&self.autostart)?;
                File::open(parent)?.sync_all()?;
            }
            return Ok(());
        }
        fs::create_dir_all(parent)?;
        if !fs::symlink_metadata(parent)?.is_dir() {
            return Err(io::Error::other("autostart directory is unsafe"));
        }
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        write!(
            pending,
            "[Desktop Entry]\nType=Application\nName=Lyteboard\nExec={}\nIcon=io.github.arielyte.lyteboard\nTerminal=false\n{}",
            desktop_exec(binary)?,
            MARKER
        )?;
        pending.as_file().sync_all()?;
        pending.persist(&self.autostart)?;
        File::open(parent)?.sync_all()
    }
}

fn managed_entry(path: &Path) -> io::Result<bool> {
    let parent = path.parent().expect("autostart parent exists");
    match fs::symlink_metadata(parent) {
        Ok(meta) if meta.is_dir() => (),
        Ok(_) => return Err(io::Error::other("autostart directory is unsafe")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.is_file()
                && meta.len() <= 4096
                && fs::read_to_string(path)?.ends_with(MARKER) =>
        {
            Ok(true)
        }
        Ok(_) => Err(io::Error::other("unrecognized login launcher")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn desktop_exec(path: &Path) -> io::Result<String> {
    let path_text = path
        .to_str()
        .filter(|text| !text.chars().any(char::is_control))
        .filter(|_| {
            path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
        })
        .ok_or_else(|| io::Error::other("unsupported executable path"))?;
    let mut argument = String::from("\"");
    for ch in path_text.chars() {
        match ch {
            '\\' | '"' | '`' | '$' => {
                argument.push('\\');
                argument.push(ch);
            }
            '%' => argument.push_str("%%"),
            _ => argument.push(ch),
        }
    }
    argument.push('"');
    Ok(argument.replace('\\', "\\\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_settings_and_managed_login_entry() {
        let config = tempfile::tempdir().unwrap();
        let mut store = Store::in_config(config.path().to_owned()).unwrap();
        assert!(
            !store.choices.start_daemon
                && !store.choices.close_to_tray
                && !store.choices.minimize_to_tray
        );
        assert!(!store.starts_at_login().unwrap());
        store
            .save(Preferences {
                start_daemon: true,
                close_to_tray: true,
                minimize_to_tray: true,
            })
            .unwrap();
        let store = Store::in_config(config.path().to_owned()).unwrap();
        assert!(
            store.choices.start_daemon
                && store.choices.close_to_tray
                && store.choices.minimize_to_tray
        );
        store
            .write_login(true, Path::new("/opt/test dir/%lyteboard"))
            .unwrap();
        assert!(store.starts_at_login().unwrap());
        assert!(
            fs::read_to_string(&store.autostart)
                .unwrap()
                .contains("Exec=\"/opt/test dir/%%lyteboard\"")
        );
        store
            .write_login(false, Path::new("/usr/bin/lyteboard-desktop"))
            .unwrap();
        assert!(!store.starts_at_login().unwrap());
        fs::write(&store.autostart, "unrelated").unwrap();
        assert!(
            store
                .write_login(false, Path::new("/usr/bin/lyteboard-desktop"))
                .is_err()
        );
        assert_eq!(fs::read_to_string(&store.autostart).unwrap(), "unrelated");
    }
}
