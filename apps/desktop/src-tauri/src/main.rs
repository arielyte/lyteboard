#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod desktop;

use desktop::*;

fn main() -> tauri::Result<()> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            daemon_status,
            devices_snapshot,
            discover_devices,
            set_listener,
            set_discovery,
            rename_device,
            set_device_address,
            remove_device,
            send_clipboard,
            pair_invite,
            pair_import,
            pair_status,
            pair_confirm,
            pair_cancel,
            clipboard_preferences,
            set_clipboard_sending,
            set_clipboard_application,
            set_clipboard_recipient,
            history_configuration,
            configure_history,
            history_page,
            history_preview,
            restore_history,
            resend_history,
            delete_history,
            clear_history,
        ])
        .run(tauri::generate_context!())
}
