//! The desktop bridge exposes individual operations on the existing local daemon.

use std::{fmt::Write as _, net::SocketAddr};

use lyteboard_core::ipc::{
    Client, ClientError, Status,
    network::{Command, ConnectionState, DEVICES_PER_PAGE, DiscoveryState, PairStatus, Reply},
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
    error: Option<String>,
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

#[cfg(target_os = "linux")]
async fn send_on_linux(recipients: Vec<String>) -> Result<Vec<DeliveryView>> {
    use std::{collections::BTreeSet, time::Duration};

    use lyteboard_core::{
        clipboard::Clipboard,
        ipc::network::{Delivery, MAX_RECIPIENTS, QueueOutcome},
    };
    use tokio::{
        sync::Semaphore,
        time::{Instant, sleep, timeout_at},
    };

    static SENDING: Semaphore = Semaphore::const_new(1);
    let _permit = SENDING
        .try_acquire()
        .map_err(|_| "Clipboard sending is already in progress.")?;
    let mut unique = BTreeSet::new();
    if recipients.is_empty() || recipients.len() > MAX_RECIPIENTS {
        return Err(format!("Select one to {MAX_RECIPIENTS} paired devices."));
    }
    let recipients = recipients
        .into_iter()
        .map(|id| {
            let key = pin(&id)?;
            if !unique.insert(key) {
                return Err("Select each device only once.".into());
            }
            Ok(hex::encode(key))
        })
        .collect::<Result<Vec<_>>>()?;

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
                match timeout_at(deadline, request(&client, Command::Delivery { id })).await {
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
