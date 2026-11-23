#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use lyteboard_core::ipc::{Client, ClientError, Status};

#[tauri::command]
async fn daemon_status() -> Result<Option<Status>, String> {
    let client = Client::from_env().map_err(|error| error.to_string())?;
    match client.status().await {
        Ok(status) => Ok(Some(status)),
        Err(ClientError::Offline) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn main() -> tauri::Result<()> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![daemon_status])
        .run(tauri::generate_context!())
}
