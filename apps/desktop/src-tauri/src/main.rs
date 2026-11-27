#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod desktop;
#[cfg_attr(target_os = "windows", path = "desktop_preferences_windows.rs")]
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod desktop_preferences;
#[cfg_attr(target_os = "windows", path = "instance_windows.rs")]
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod instance;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod lifecycle;
#[cfg(target_os = "linux")]
mod tray;

#[cfg(any(target_os = "linux", target_os = "windows"))]
use desktop::*;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use lifecycle::*;

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Match the installed desktop entry on Wayland without registering a global
    // GTK application singleton: development runtime profiles stay independent.
    #[cfg(target_os = "linux")]
    gtk::glib::set_prgname(Some("io.github.arielyte.lyteboard"));
    let instance = match instance::Instance::claim() {
        Ok(Some(instance)) => Some(instance),
        Ok(None) => return Ok(()),
        #[cfg(target_os = "linux")]
        Err(error)
            if error
                .downcast_ref::<lyteboard_core::ipc::local::EndpointError>()
                .is_some() =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    tauri::Builder::default()
        .setup(move |app| {
            lifecycle::setup(app, instance);
            Ok(())
        })
        .on_window_event(lifecycle::window_event)
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
            set_clipboard_pause,
            set_clipboard_application,
            set_clipboard_recipient,
            history_configuration,
            configure_history,
            history_page,
            history_preview,
            restore_history,
            latest_received,
            copy_received,
            resend_history,
            delete_history,
            clear_history,
            desktop_configuration,
            set_desktop_preferences,
            set_login_startup,
            start_daemon,
            stop_daemon,
            select_tray_recipients,
            minimize_to_tray,
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("The Lyteboard desktop currently runs on Linux.");
}
