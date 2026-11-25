use std::{sync::atomic::Ordering, time::Duration};

use ksni::{Icon, MenuItem, OfflineReason, ToolTip, Tray, TrayMethods, menu::StandardItem};
use lyteboard_core::ipc::{
    Client, ClientError,
    network::{Command, Reply},
};
use tauri::{AppHandle, Manager};
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::{
    desktop,
    lifecycle::{self, Desktop},
};

#[derive(Default, PartialEq, Eq)]
struct Snapshot {
    status: String,
    daemon_running: bool,
    paused: Option<bool>,
    can_send: bool,
    last_action: Option<String>,
}

struct DesktopTray {
    app: AppHandle,
    info: Snapshot,
    pixels: Vec<u8>,
}

impl Tray for DesktopTray {
    fn id(&self) -> String {
        "io.github.arielyte.lyteboard".into()
    }

    fn title(&self) -> String {
        "Lyteboard".into()
    }

    fn icon_name(&self) -> String {
        "io.github.arielyte.lyteboard".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        if self.pixels.is_empty() {
            return Vec::new();
        }
        vec![Icon {
            width: 32,
            height: 32,
            data: self.pixels.clone(),
        }]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.title(),
            description: self.info.status.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        lifecycle::show(&self.app);
    }

    fn watcher_online(&self) {
        lifecycle::tray_status(&self.app, true);
    }

    fn watcher_offline(&self, _reason: OfflineReason) -> bool {
        lifecycle::tray_status(&self.app, false);
        true
    }

    fn menu_about_to_show(&mut self) {
        self.app.state::<Desktop>().wake_tray.notify_one();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let action = |label: &str, enabled, activate| {
            StandardItem {
                label: label.into(),
                enabled,
                activate,
                ..Default::default()
            }
            .into()
        };
        let mut entries = vec![
            StandardItem {
                label: self.info.status.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            action(
                "Open Lyteboard",
                true,
                Box::new(|tray: &mut Self| lifecycle::show(&tray.app)),
            ),
            action(
                "Send clipboard to selected devices",
                self.info.can_send,
                Box::new(|tray: &mut Self| tray.run_action(send_selected)),
            ),
            action(
                if self.info.paused == Some(true) {
                    "Resume automatic synchronization"
                } else {
                    "Pause automatic synchronization"
                },
                self.info.paused.is_some(),
                Box::new(|tray: &mut Self| tray.run_action(toggle_sync)),
            ),
        ];
        if let Some(last_action) = &self.info.last_action {
            entries.push(
                StandardItem {
                    label: last_action.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        entries.extend([
            MenuItem::Separator,
            action(
                "Quit GUI",
                true,
                Box::new(|tray: &mut Self| tray.app.exit(0)),
            ),
            action(
                "Stop daemon and quit",
                self.info.daemon_running,
                Box::new(|tray: &mut Self| tray.run_action(stop_and_quit)),
            ),
        ]);
        entries
    }
}

impl DesktopTray {
    fn run_action<F>(&self, action: impl FnOnce(AppHandle) -> F + Send + 'static)
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = action(app.clone()).await {
                lifecycle::notify(&app, error, true);
            }
        });
    }
}

async fn send_selected(app: AppHandle) -> Result<(), String> {
    let recipients = app
        .state::<Desktop>()
        .recipients
        .lock()
        .expect("tray recipients")
        .clone();
    if recipients.is_empty() {
        return Err("Select at least one device in Lyteboard first.".into());
    }
    let replies = desktop::send_clipboard(recipients).await?;
    let sent = replies
        .iter()
        .filter(|delivery| delivery.error.is_none())
        .count();
    let total = replies.len();
    let mut status = format!("Clipboard delivered to {sent} of {total} devices.");
    if let Some(reason) = replies
        .iter()
        .find_map(|delivery| delivery.error.as_deref())
    {
        status.push(' ');
        status.push_str(reason);
    }
    lifecycle::notify(&app, status, sent != total);
    Ok(())
}

async fn toggle_sync(app: AppHandle) -> Result<(), String> {
    let current = desktop::clipboard_preferences().await?;
    let status = desktop::set_clipboard_pause(!current.paused).await?;
    if let Some(error) = status.send_error.or(status.apply_error) {
        return Err(error);
    }
    lifecycle::notify(
        &app,
        if status.paused {
            "Automatic synchronization paused."
        } else {
            "Automatic synchronization resumed."
        }
        .into(),
        false,
    );
    Ok(())
}

async fn stop_and_quit(app: AppHandle) -> Result<(), String> {
    lifecycle::stop_daemon(app.clone()).await?;
    app.exit(0);
    Ok(())
}

async fn snapshot(app: &AppHandle) -> Snapshot {
    let state = app.state::<Desktop>();
    let mut snapshot = Snapshot {
        status: "Daemon unavailable".into(),
        last_action: state
            .last_notice
            .lock()
            .expect("desktop notice")
            .as_ref()
            .map(|notice| notice.message.chars().take(100).collect()),
        ..Default::default()
    };
    let Ok(client) = Client::from_env() else {
        return snapshot;
    };
    match client.status().await {
        Ok(_) => {
            snapshot.status = "Daemon running".into();
            snapshot.daemon_running = true;
            let (network, clipboard) = tokio::join!(
                client.network(Command::Status),
                desktop::clipboard_preferences()
            );
            if let Ok(Reply::Status { sessions, .. }) = network {
                snapshot.status = format!("Daemon running · {sessions} connected");
            }
            snapshot.paused = clipboard.ok().map(|prefs| prefs.paused);
            snapshot.can_send = !state.recipients.lock().expect("tray recipients").is_empty();
        }
        Err(ClientError::Offline) => snapshot.status = "Daemon offline".into(),
        Err(_) => (),
    }
    snapshot
}

pub(crate) async fn run(app: AppHandle) {
    let pixels = icon_pixels(&app);
    let mut handle: Option<ksni::Handle<DesktopTray>> = None;
    let mut timer = interval(Duration::from_secs(15));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let state = app.state::<Desktop>();
    let mut last_probe = None::<tokio::time::Instant>;
    loop {
        tokio::select! {
            _ = timer.tick() => (),
            _ = state.wake_tray.notified() => (),
        }
        if handle.as_ref().is_none_or(ksni::Handle::is_closed) {
            lifecycle::tray_status(&app, false);
            if last_probe.is_some_and(|last| last.elapsed() < Duration::from_secs(15)) {
                continue;
            }
            last_probe = Some(tokio::time::Instant::now());
            let tray = DesktopTray {
                app: app.clone(),
                info: Snapshot {
                    status: "Checking daemon…".into(),
                    ..Default::default()
                },
                pixels: pixels.clone(),
            };
            handle = timeout(Duration::from_secs(3), tray.spawn())
                .await
                .ok()
                .and_then(Result::ok);
            lifecycle::tray_status(&app, handle.is_some());
        }
        if let Some(handle) = &handle
            && state.tray_available.load(Ordering::Relaxed)
        {
            let info = snapshot(&app).await;
            let _ = handle
                .update(move |tray| {
                    tray.info = info;
                })
                .await;
        }
    }
}

fn icon_pixels(app: &AppHandle) -> Vec<u8> {
    let Some(icon) = app
        .default_window_icon()
        .filter(|icon| icon.width() > 0 && icon.height() > 0)
    else {
        return Vec::new();
    };
    let mut result = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let i = ((y * icon.height() / 32) * icon.width() + x * icon.width() / 32) as usize * 4;
            let [r, g, b, a] = icon.rgba()[i..i + 4] else {
                unreachable!()
            };
            result.extend([a, r, g, b]);
        }
    }
    result
}
