//! The desktop bridge exposes individual operations on the existing local daemon.

use std::{fmt::Write as _, net::SocketAddr};

use lyteboard_core::ipc::{
    Client, ClientError, Status,
    network::{
        ClipboardPreferences, ClipboardSyncCommand, Command, ConnectionState, DEVICES_PER_PAGE,
        Delivery, DiscoveryState, HistoryCommand, HistoryConfiguration, HistoryEntry,
        HistoryLimits, MAX_HISTORY_QUERY_BYTES, MAX_RECIPIENTS, PairStatus, QueueOutcome, Reply,
    },
};
use lyteboard_protocol::peer::{LanInvite, MAX_LABEL_BYTES, NoiseKey, validate_label};
use qrcode::{Color, QrCode};
use serde::Serialize;
use zeroize::{Zeroize, Zeroizing};

type Result<T> = std::result::Result<T, String>;

fn client() -> Result<Client> {
    Client::from_env().map_err(|error| error.to_string())
}

async fn request(client: &Client, command: Command) -> Result<Reply> {
    match client
        .network(command)
        .await
        .map_err(|error| error.to_string())?
    {
        Reply::Error(error) => Err(error),
        reply => Ok(reply),
    }
}

fn unexpected() -> String {
    "Unexpected daemon response. Check that the GUI and daemon are from the same build.".into()
}

async fn mutate(command: Command) -> Result<()> {
    match request(&client()?, command).await? {
        Reply::Ok => Ok(()),
        _ => Err(unexpected()),
    }
}

fn pin(id: &str) -> Result<NoiseKey> {
    let mut key = [0; 32];
    hex::decode_to_slice(id, &mut key).map_err(|_| "Invalid device ID.")?;
    Ok(key)
}

fn address(value: &str) -> Result<SocketAddr> {
    value.trim().parse().map_err(|_| {
        "Enter an IP address and port, such as 192.168.1.10:47851 or [fd00::10]:47851.".into()
    })
}

#[tauri::command]
pub(crate) async fn daemon_status() -> Result<Option<Status>> {
    match client()?.status().await {
        Ok(status) => Ok(Some(status)),
        Err(ClientError::Offline) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Serialize)]
pub(crate) struct ListenerView {
    address: String,
    discovery: DiscoveryState,
    loopback: bool,
}

#[derive(Serialize)]
pub(crate) struct DeviceView {
    id: String,
    name: String,
    address: Option<String>,
    state: ConnectionState,
}

#[derive(Serialize)]
pub(crate) struct DevicesView {
    listener: ListenerView,
    devices: Vec<DeviceView>,
}

fn listener(reply: Reply) -> Result<ListenerView> {
    let Reply::Status {
        listener,
        discovery,
        ..
    } = reply
    else {
        return Err(unexpected());
    };
    Ok(ListenerView {
        address: listener.to_string(),
        discovery,
        loopback: listener.ip().is_loopback(),
    })
}

#[tauri::command]
pub(crate) async fn devices_snapshot() -> Result<DevicesView> {
    let client = client()?;
    let listener = listener(request(&client, Command::Status).await?)?;
    let mut devices = Vec::new();
    for offset in (0..128).step_by(DEVICES_PER_PAGE) {
        let Reply::Devices { entries, more } =
            request(&client, Command::Devices { offset }).await?
        else {
            return Err(unexpected());
        };
        devices.extend(entries.into_iter().map(|entry| DeviceView {
            id: hex::encode(entry.key),
            name: entry.name,
            address: entry.address.map(|address| address.to_string()),
            state: entry.state,
        }));
        if !more {
            return Ok(DevicesView { listener, devices });
        }
    }
    Err(unexpected())
}

#[derive(Serialize)]
pub(crate) struct DiscoveryEntry {
    id: String,
    address: String,
    paired: bool,
}

#[tauri::command]
pub(crate) async fn discover_devices() -> Result<Vec<DiscoveryEntry>> {
    let client = client()?;
    let mut found = Vec::new();
    for offset in (0..512).step_by(DEVICES_PER_PAGE) {
        let Reply::Discovered { entries, more } =
            request(&client, Command::Discover { offset }).await?
        else {
            return Err(unexpected());
        };
        found.extend(entries.into_iter().map(|entry| DiscoveryEntry {
            id: hex::encode(entry.key),
            address: entry.address.to_string(),
            paired: entry.paired,
        }));
        if !more {
            return Ok(found);
        }
    }
    Err(unexpected())
}

#[tauri::command]
pub(crate) async fn set_listener(endpoint: String) -> Result<ListenerView> {
    listener(
        request(
            &client()?,
            Command::Listen {
                address: address(&endpoint)?,
            },
        )
        .await?,
    )
}

#[tauri::command]
pub(crate) async fn set_discovery(enabled: bool) -> Result<ListenerView> {
    listener(request(&client()?, Command::SetDiscovery { enabled }).await?)
}

#[tauri::command]
pub(crate) async fn rename_device(id: String, name: String) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || validate_label(name).is_err() {
        return Err(format!(
            "Enter a device name of up to {MAX_LABEL_BYTES} UTF-8 bytes without control characters."
        ));
    }
    mutate(Command::Rename {
        peer: pin(&id)?,
        name: name.into(),
    })
    .await
}

#[tauri::command]
pub(crate) async fn set_device_address(id: String, endpoint: String) -> Result<()> {
    mutate(Command::SetAddress {
        peer: pin(&id)?,
        address: address(&endpoint)?,
    })
    .await
}

#[tauri::command]
pub(crate) async fn remove_device(id: String) -> Result<()> {
    mutate(Command::Remove { peer: pin(&id)? }).await
}

#[derive(Serialize)]
pub(crate) struct DeliveryView {
    id: String,
    pub(crate) error: Option<String>,
}

#[tauri::command]
pub(crate) async fn send_clipboard(recipients: Vec<String>) -> Result<Vec<DeliveryView>> {
    #[cfg(target_os = "linux")]
    return send_on_linux(recipients).await;

    #[cfg(not(target_os = "linux"))]
    {
        let _ = recipients;
        Err("Clipboard sharing is currently supported on Linux.".into())
    }
}

static SENDING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(crate) fn recipients(ids: Vec<String>) -> Result<Vec<String>> {
    use std::collections::BTreeSet;
    let recipients = ids;
    let mut unique = BTreeSet::new();
    if recipients.is_empty() || recipients.len() > MAX_RECIPIENTS {
        return Err(format!("Select one to {MAX_RECIPIENTS} paired devices."));
    }
    recipients
        .into_iter()
        .map(|id| {
            let key = pin(&id)?;
            if !unique.insert(key) {
                return Err("Select each device only once.".into());
            }
            Ok(hex::encode(key))
        })
        .collect()
}

#[cfg(target_os = "linux")]
async fn send_on_linux(ids: Vec<String>) -> Result<Vec<DeliveryView>> {
    use lyteboard_core::clipboard::Clipboard;
    let _permit = SENDING
        .try_acquire()
        .map_err(|_| "Clipboard sending is already in progress.")?;
    let recipients = recipients(ids)?;
    let client = client()?;
    client.ping().await.map_err(|error| error.to_string())?;
    let (media, bytes) = Clipboard::for_session()
        .map_err(|error| error.to_string())?
        .read_content()
        .await
        .map_err(|error| error.to_string())?;
    let outcomes = client
        .upload_bulk(recipients.clone(), media, &bytes)
        .await
        .map_err(|error| error.to_string())?;
    drop(bytes);
    delivery_results(&client, recipients, outcomes).await
}

async fn delivery_results(
    client: &Client,
    recipients: Vec<String>,
    outcomes: Vec<QueueOutcome>,
) -> Result<Vec<DeliveryView>> {
    use std::time::Duration;
    use tokio::time::{Instant, sleep, timeout_at};

    if outcomes.len() != recipients.len() {
        return Err(unexpected());
    }

    let deadline = Instant::now() + Duration::from_secs(75);
    let mut results = Vec::with_capacity(recipients.len());
    let mut pending = Vec::new();
    for (index, (id, outcome)) in recipients.into_iter().zip(outcomes).enumerate() {
        let error = match outcome {
            QueueOutcome::Queued(receipt) => {
                pending.push((index, receipt));
                Some("Delivery was not confirmed before the deadline.".into())
            }
            QueueOutcome::Error(error) => Some(error),
        };
        results.push(DeliveryView { id, error });
    }
    while !pending.is_empty() && Instant::now() < deadline {
        let mut waiting = Vec::new();
        for (index, id) in pending {
            results[index].error =
                match timeout_at(deadline, request(client, Command::Delivery { id })).await {
                    Ok(Ok(Reply::Delivery(Delivery::Delivered))) => None,
                    Ok(Ok(Reply::Delivery(Delivery::Pending))) => {
                        waiting.push((index, id));
                        continue;
                    }
                    Ok(Ok(Reply::Delivery(Delivery::Failed(error))) | Err(error)) => Some(error),
                    Ok(Ok(_)) => Some(unexpected()),
                    Err(_) => continue,
                };
        }
        pending = waiting;
        if !pending.is_empty() {
            sleep(Duration::from_millis(250)).await;
        }
    }
    Ok(results)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Invitation {
    text: String,
    expires_at: u64,
    size: usize,
    modules: String,
}

impl Drop for Invitation {
    fn drop(&mut self) {
        self.text.zeroize();
        self.modules.zeroize();
    }
}

#[tauri::command]
pub(crate) async fn pair_invite() -> Result<Invitation> {
    let Reply::Invite(text) = request(&client()?, Command::Invite).await? else {
        return Err(unexpected());
    };
    let text = Zeroizing::new(text);
    let invite = LanInvite::decode_qr(&text).map_err(|_| unexpected())?;
    let qr = QrCode::new(text.as_bytes()).map_err(|_| "Unable to render this invitation.")?;
    let width = qr.width();
    let mut modules = String::new();
    for (i, color) in qr.to_colors().into_iter().enumerate() {
        if color == Color::Dark {
            write!(&mut modules, "M{} {}h1v1h-1z", i % width + 4, i / width + 4)
                .expect("write SVG coordinates into a string");
        }
    }
    Ok(Invitation {
        text: text.to_string(),
        expires_at: invite.expires_at,
        size: width + 8,
        modules,
    })
}

#[tauri::command]
pub(crate) async fn pair_import(invitation: String) -> Result<()> {
    let invitation = Zeroizing::new(invitation);
    let qr = invitation.trim();
    if qr.len() > 512 || LanInvite::decode_qr(qr).is_err() {
        return Err("Enter a valid Lyteboard LAN invitation.".into());
    }
    mutate(Command::Accept { qr: qr.into() }).await
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PairView {
    Idle,
    Connecting,
    Compare { code: String },
    Approving,
    Paired,
    Failed { error: String },
}

#[tauri::command]
pub(crate) async fn pair_status() -> Result<PairView> {
    let Reply::Pair(status) = request(&client()?, Command::PairStatus).await? else {
        return Err(unexpected());
    };
    Ok(match status {
        PairStatus::Idle => PairView::Idle,
        PairStatus::Connecting => PairView::Connecting,
        PairStatus::Compare { code } => PairView::Compare { code },
        PairStatus::Approving => PairView::Approving,
        PairStatus::Paired => PairView::Paired,
        PairStatus::Failed(error) => PairView::Failed { error },
    })
}

#[tauri::command]
pub(crate) async fn pair_confirm(code: String) -> Result<()> {
    let code = Zeroizing::new(code);
    let code = code.trim();
    if code.is_empty()
        || code.len() > 19
        || !code
            .bytes()
            .all(|byte| byte == b'-' || byte.is_ascii_hexdigit())
    {
        return Err("Enter the verification code shown on both devices.".into());
    }
    mutate(Command::Confirm { code: code.into() }).await
}

#[tauri::command]
pub(crate) async fn pair_cancel() -> Result<()> {
    mutate(Command::CancelPair).await
}

#[tauri::command]
pub(crate) async fn clipboard_preferences() -> Result<ClipboardPreferences> {
    let Reply::ClipboardPreferences(settings) =
        request(&client()?, Command::ClipboardPreferences).await?
    else {
        return Err(unexpected());
    };
    Ok(settings)
}

#[tauri::command]
pub(crate) async fn set_clipboard_pause(paused: bool) -> Result<ClipboardPreferences> {
    let Reply::ClipboardPreferences(settings) =
        request(&client()?, Command::SetClipboardPaused { paused }).await?
    else {
        return Err(unexpected());
    };
    Ok(settings)
}

#[tauri::command]
pub(crate) async fn set_clipboard_sending(enabled: bool) -> Result<ClipboardPreferences> {
    request(
        &client()?,
        Command::ClipboardSync(ClipboardSyncCommand::Set { enabled }),
    )
    .await?;
    clipboard_preferences().await
}

#[tauri::command]
pub(crate) async fn set_clipboard_application(enabled: bool) -> Result<ClipboardPreferences> {
    request(&client()?, Command::SetClipboardAuto { enabled }).await?;
    clipboard_preferences().await
}

#[tauri::command]
pub(crate) async fn set_clipboard_recipient(
    id: String,
    enabled: bool,
) -> Result<ClipboardPreferences> {
    request(
        &client()?,
        Command::ClipboardSync(ClipboardSyncCommand::Recipient {
            peer: pin(&id)?,
            enabled,
        }),
    )
    .await?;
    clipboard_preferences().await
}

#[tauri::command]
pub(crate) async fn history_configuration() -> Result<HistoryConfiguration> {
    history_config(HistoryCommand::Configuration).await
}

#[tauri::command]
pub(crate) async fn configure_history(
    enabled: bool,
    limits: HistoryLimits,
) -> Result<HistoryConfiguration> {
    if !limits.is_valid() {
        return Err("Enter valid history retention limits.".into());
    }
    history_config(HistoryCommand::Configure { enabled, limits }).await
}

async fn history_config(command: HistoryCommand) -> Result<HistoryConfiguration> {
    let Reply::HistoryConfiguration(value) = request(&client()?, Command::History(command)).await?
    else {
        return Err(unexpected());
    };
    Ok(value)
}

#[derive(Serialize)]
pub(crate) struct HistoryPageView {
    entries: Vec<HistoryEntry>,
    more: bool,
}

#[tauri::command]
pub(crate) async fn history_page(before: Option<i64>, query: String) -> Result<HistoryPageView> {
    let query = query.trim();
    if query.len() > MAX_HISTORY_QUERY_BYTES || query.contains('\0') {
        return Err("Search must be at most 256 UTF-8 bytes.".into());
    }
    let command = if query.is_empty() {
        HistoryCommand::List { before }
    } else {
        HistoryCommand::Search {
            before,
            query: query.into(),
        }
    };
    let Reply::HistoryPage { entries, more } =
        request(&client()?, Command::History(command)).await?
    else {
        return Err(unexpected());
    };
    Ok(HistoryPageView { entries, more })
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PreviewView {
    Text { text: String },
    Image { data: String },
}

#[tauri::command]
pub(crate) async fn history_preview(id: i64) -> Result<PreviewView> {
    #[cfg(target_os = "linux")]
    {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use lyteboard_protocol::peer::Media;
        static PREVIEWS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let _permit = PREVIEWS
            .try_acquire()
            .map_err(|_| "Another preview is loading.")?;
        let (media, bytes) = client()?
            .download_history_preview(id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("History entry was not found.")?;
        match media {
            Media::Text => Ok(PreviewView::Text {
                text: std::str::from_utf8(&bytes)
                    .map_err(|_| "History preview contains invalid text.")?
                    .into(),
            }),
            Media::Png => Ok(PreviewView::Image {
                data: STANDARD.encode(&*bytes),
            }),
            _ => Err(unexpected()),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        Err("Clipboard history is currently supported on Linux.".into())
    }
}

#[tauri::command]
pub(crate) async fn restore_history(id: i64) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let clipboard = lyteboard_core::clipboard::Clipboard::for_session()
            .map_err(|error| error.to_string())?;
        let (media, bytes) = client()?
            .download_history(id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("History entry was not found.")?;
        return clipboard
            .write_content(media, &bytes)
            .await
            .map_err(|error| error.to_string());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        Err("Clipboard history is currently supported on Linux.".into())
    }
}

#[tauri::command]
pub(crate) async fn resend_history(id: i64, devices: Vec<String>) -> Result<Vec<DeliveryView>> {
    let _permit = SENDING
        .try_acquire()
        .map_err(|_| "Clipboard sending is already in progress.")?;
    let devices = recipients(devices)?;
    let client = client()?;
    let Reply::BulkQueued(outcomes) = request(
        &client,
        Command::History(HistoryCommand::Resend {
            id,
            peers: devices.clone(),
        }),
    )
    .await?
    else {
        return Err(unexpected());
    };
    delivery_results(&client, devices, outcomes).await
}

#[tauri::command]
pub(crate) async fn delete_history(id: i64) -> Result<()> {
    mutate(Command::History(HistoryCommand::Delete { id })).await
}

#[tauri::command]
pub(crate) async fn clear_history(confirmed: bool) -> Result<()> {
    if !confirmed {
        return Err("Confirm that you want to clear all history.".into());
    }
    mutate(Command::History(HistoryCommand::Clear)).await
}
