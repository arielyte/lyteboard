use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use gtk::prelude::WidgetExt;
use lyteboard_core::ipc::{Client, ClientError, lifecycle::StartOutcome};
use serde::Serialize;
use tauri::{App, AppHandle, Emitter, Manager, State, WindowEvent};
use tokio::sync::{Notify, Semaphore};

use crate::{
    desktop,
    desktop_preferences::{Preferences, Store},
    instance::Instance,
    tray,
};

pub(crate) const CHANGED: &str = "desktop-changed";
pub(crate) const NOTICE: &str = "desktop-notice";

#[derive(Clone, Serialize)]
pub(crate) struct Notice {
    pub message: String,
    pub error: bool,
}

pub(crate) struct Desktop {
    store: Mutex<Result<Store, String>>,
    pub tray_available: AtomicBool,
    hidden: AtomicBool,
    pub recipients: Mutex<Vec<String>>,
    pub wake_tray: Notify,
    pub last_notice: Mutex<Option<Notice>>,
    daemon_action: Semaphore,
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            store: Mutex::new(Store::open()),
            tray_available: false.into(),
            hidden: false.into(),
            recipients: Mutex::new(Vec::new()),
            wake_tray: Notify::new(),
            last_notice: Mutex::new(None),
            daemon_action: Semaphore::const_new(1),
        }
    }

    fn preferences(&self) -> Preferences {
        self.store
            .lock()
            .expect("desktop preferences")
            .as_ref()
            .map_or(Preferences::default(), |store| store.choices)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopConfiguration {
    preferences: Preferences,
    tray_available: bool,
    starts_at_login: bool,
    error: Option<String>,
    last_notice: Option<Notice>,
}

fn configuration(state: &Desktop) -> DesktopConfiguration {
    let store = state.store.lock().expect("desktop preferences");
    let (preferences, starts_at_login, error) = match store.as_ref() {
        Ok(store) => match store.starts_at_login() {
            Ok(enabled) => (store.choices, enabled, None),
            Err(error) => (store.choices, false, Some(error)),
        },
        Err(error) => (Preferences::default(), false, Some(error.clone())),
    };
    DesktopConfiguration {
        preferences,
        starts_at_login,
        error,
        tray_available: state.tray_available.load(Ordering::Relaxed),
        last_notice: state.last_notice.lock().expect("desktop notice").clone(),
    }
}

#[tauri::command]
pub(crate) fn desktop_configuration(state: State<'_, Desktop>) -> DesktopConfiguration {
    configuration(&state)
}

#[tauri::command]
pub(crate) fn set_desktop_preferences(
    state: State<'_, Desktop>,
    preferences: Preferences,
) -> Result<DesktopConfiguration, String> {
    {
        let mut store = state.store.lock().expect("desktop preferences");
        store
            .as_mut()
            .map_err(|error| error.clone())?
            .save(preferences)?;
    }
    Ok(configuration(&state))
}

#[tauri::command]
pub(crate) fn set_login_startup(
    state: State<'_, Desktop>,
    enabled: bool,
) -> Result<DesktopConfiguration, String> {
    {
        let store = state.store.lock().expect("desktop preferences");
        store.as_ref().map_err(Clone::clone)?.set_login(enabled)?;
    }
    Ok(configuration(&state))
}

pub(crate) fn changed(app: &AppHandle) {
    app.state::<Desktop>().wake_tray.notify_one();
    let _ = app.emit_to("main", CHANGED, ());
}

pub(crate) fn notify(app: &AppHandle, message: String, error: bool) {
    let notice = Notice { message, error };
    *app.state::<Desktop>()
        .last_notice
        .lock()
        .expect("desktop notice") = Some(notice.clone());
    let _ = app.emit_to("main", NOTICE, notice);
    changed(app);
}

#[tauri::command]
pub(crate) async fn start_daemon(app: AppHandle) -> Result<(), String> {
    let state = app.state::<Desktop>();
    let _action = state
        .daemon_action
        .try_acquire()
        .map_err(|_| "A daemon action is already in progress.")?;
    let result = Client::from_env()
        .map_err(|error| error.to_string())?
        .start_daemon()
        .await
        .map_err(|error| error.to_string())?;
    notify(
        &app,
        match result {
            StartOutcome::AlreadyRunning => "Daemon already running.",
            StartOutcome::Started => "Daemon started.",
        }
        .into(),
        false,
    );
    Ok(())
}

#[tauri::command]
pub(crate) async fn stop_daemon(app: AppHandle) -> Result<(), String> {
    let state = app.state::<Desktop>();
    let _action = state
        .daemon_action
        .try_acquire()
        .map_err(|_| "A daemon action is already in progress.")?;
    match Client::from_env()
        .map_err(|error| error.to_string())?
        .shutdown()
        .await
    {
        Ok(()) | Err(ClientError::Offline) => (),
        Err(error) => return Err(error.to_string()),
    }
    notify(&app, "Daemon stopped.".into(), false);
    Ok(())
}

#[tauri::command]
pub(crate) fn select_tray_recipients(
    state: State<'_, Desktop>,
    ids: Vec<String>,
) -> Result<(), String> {
    let ids = if ids.is_empty() {
        ids
    } else {
        desktop::recipients(ids)?
    };
    *state.recipients.lock().expect("tray recipients") = ids;
    state.wake_tray.notify_one();
    Ok(())
}

#[tauri::command]
pub(crate) fn minimize_to_tray(app: AppHandle) -> Result<(), String> {
    let state = app.state::<Desktop>();
    if !state.preferences().minimize_to_tray {
        return Err("Enable minimize to tray in Settings first.".into());
    }
    hide(&app)
}

fn hide(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<Desktop>();
    if !state.tray_available.load(Ordering::Relaxed) {
        return Err("No system tray is available.".into());
    }
    let window = app
        .get_webview_window("main")
        .ok_or("Lyteboard window is unavailable.")?;
    state.hidden.store(true, Ordering::Relaxed);
    if let Err(error) = window.hide() {
        state.hidden.store(false, Ordering::Relaxed);
        return Err(error.to_string());
    }
    Ok(())
}

pub(crate) fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        app.state::<Desktop>()
            .hidden
            .store(false, Ordering::Relaxed);
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        changed(app);
    }
}

pub(crate) fn tray_status(app: &AppHandle, available: bool) {
    let state = app.state::<Desktop>();
    if state.tray_available.swap(available, Ordering::Relaxed) != available {
        if !available && state.hidden.load(Ordering::Relaxed) {
            show(app);
        } else {
            changed(app);
        }
    }
}

pub(crate) fn window_event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() == "main"
        && let WindowEvent::CloseRequested { api, .. } = event
        && window.state::<Desktop>().preferences().close_to_tray
        && hide(window.app_handle()).is_ok()
    {
        api.prevent_close();
    }
}

pub(crate) fn setup(app: &mut App, instance: Option<Instance>) {
    app.manage(Desktop::new());
    let handle = app.handle().clone();
    if let Some(window) = app.get_webview_window("main")
        && let Ok(gtk_window) = window.gtk_window()
    {
        let handle = handle.clone();
        gtk_window.connect_window_state_event(move |_, event| {
            use gtk::gdk::WindowState;
            if event.changed_mask().contains(WindowState::ICONIFIED)
                && event.new_window_state().contains(WindowState::ICONIFIED)
                && handle.state::<Desktop>().preferences().minimize_to_tray
            {
                let _ = hide(&handle);
            }
            gtk::glib::Propagation::Proceed
        });
    }
    if let Some(instance) = instance {
        let handle = handle.clone();
        tauri::async_runtime::spawn(async move {
            while let Ok(activate) = instance.activated().await {
                if activate {
                    show(&handle);
                }
            }
        });
    }
    if app.state::<Desktop>().preferences().start_daemon {
        let handle = handle.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = start_daemon(handle.clone()).await {
                notify(&handle, error, true);
            }
        });
    }
    tauri::async_runtime::spawn(tray::run(handle));
}
