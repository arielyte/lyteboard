use lyteboard_core::{
    peer::storage::{open_private_dir, read_private_json, write_private_json},
    windows::PrivateDir,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Preferences {
    pub start_daemon: bool,
    pub close_to_tray: bool,
    pub minimize_to_tray: bool,
}
pub(crate) struct Store {
    directory: PrivateDir,
    pub choices: Preferences,
}
impl Store {
    pub fn open() -> Result<Self, String> {
        let directory = open_private_dir(
            &lyteboard_core::windows::app_data()
                .map_err(|e| e.to_string())?
                .join("settings"),
        )
        .map_err(|e| e.to_string())?;
        let choices = read_private_json(&directory, "desktop.json", 1024)
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        Ok(Self { directory, choices })
    }
    pub fn save(&mut self, choices: Preferences) -> Result<(), String> {
        if choices.close_to_tray || choices.minimize_to_tray {
            return Err("Windows system tray integration is not implemented yet.".into());
        }
        write_private_json(&self.directory, "desktop.json", &choices, 1024)
            .map_err(|e| e.to_string())?;
        self.choices = choices;
        Ok(())
    }
    pub fn starts_at_login(&self) -> Result<bool, String> {
        Ok(false)
    }
    pub fn set_login(&self, _: bool) -> Result<(), String> {
        Err("Windows login startup is not implemented yet.".into())
    }
}
