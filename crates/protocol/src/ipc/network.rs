//! Bounded same-user LAN controls. Debug deliberately omits payloads.

use std::{fmt, net::SocketAddr};

use serde::{Deserialize, Serialize};

use crate::peer::{Media, NoiseKey};

pub const MAX_TEST_TEXT_BYTES: usize = 512;
pub const DEVICES_PER_PAGE: usize = 4;
pub const MAX_RECIPIENTS: usize = 32;
pub const HISTORY_PER_PAGE: usize = 16;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Status,
    Listen {
        address: SocketAddr,
    },
    Discover {
        offset: usize,
    },
    SetDiscovery {
        enabled: bool,
    },
    SetAddress {
        peer: NoiseKey,
        address: SocketAddr,
    },
    Invite,
    Accept {
        qr: String,
    },
    PairStatus,
    Confirm {
        code: String,
    },
    CancelPair,
    Devices {
        offset: usize,
    },
    Rename {
        peer: NoiseKey,
        name: String,
    },
    Remove {
        peer: NoiseKey,
    },
    Send {
        peer: NoiseKey,
        text: String,
    },
    Delivery {
        id: u64,
    },
    BulkUpload {
        peers: Vec<String>,
        media: Media,
        size: u32,
    },
    BulkDownload {
        offset: usize,
    },
    Received {
        offset: usize,
    },
    ClipboardAutoStatus,
    SetClipboardAuto {
        enabled: bool,
    },
    History(HistoryCommand),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Ok,
    Error(String),
    Status {
        listener: SocketAddr,
        sessions: usize,
        discovery: DiscoveryState,
    },
    Discovered {
        entries: Vec<Discovered>,
        more: bool,
    },
    Invite(String),
    Pair(PairStatus),
    Devices {
        entries: Vec<Device>,
        more: bool,
    },
    Queued(u64),
    BulkReady,
    BulkQueued(Vec<QueueOutcome>),
    BulkItem {
        media: Media,
        size: u32,
    },
    NoBulkItem,
    Delivery(Delivery),
    Received(Option<Message>),
    ClipboardAuto {
        enabled: bool,
        last_error: Option<String>,
    },
    HistoryStatus(Option<HistoryLimits>),
    HistoryPage {
        entries: Vec<HistoryEntry>,
        more: bool,
    },
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryCommand {
    Status,
    List { before: Option<i64> },
    Restore { id: i64 },
    Resend { id: i64, peers: Vec<String> },
    Delete { id: i64 },
    Clear,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HistoryLimits {
    pub entries: u32,
    pub mib: u32,
    pub days: u32,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub id: i64,
    pub timestamp: i64,
    pub media: Media,
    pub direction: HistoryDirection,
    pub peer: String,
    pub size: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    Sent,
    Received,
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NetworkCommand([redacted])")
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NetworkReply([redacted])")
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueOutcome {
    Queued(u64),
    Error(String),
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PairStatus {
    Idle,
    Connecting,
    Compare { code: String },
    Approving,
    Paired,
    Failed(String),
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Delivery {
    Pending,
    Delivered,
    Failed(String),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub key: NoiseKey,
    pub name: String,
    pub address: Option<SocketAddr>,
    pub state: ConnectionState,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryState {
    Off,
    Loopback,
    Running,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Discovered {
    pub key: NoiseKey,
    pub address: SocketAddr,
    pub paired: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub peer: NoiseKey,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{MAX_FRAME_BYTES, Request, Response, decode, encode};

    #[test]
    fn network_payloads_fit_existing_frames_and_debug_is_redacted() {
        let text = "\u{1}".repeat(MAX_TEST_TEXT_BYTES);
        let request = Request::Network(Command::Send {
            peer: [255; 32],
            text: text.clone(),
        });
        let bytes = encode(&request).unwrap();
        assert!(bytes.len() <= MAX_FRAME_BYTES);
        assert_eq!(decode::<Request>(&bytes).unwrap(), request);
        let received = Response::Network(Reply::Received(Some(Message {
            peer: [255; 32],
            text,
        })));
        assert!(encode(&received).unwrap().len() <= MAX_FRAME_BYTES);
        let page = Response::Network(Reply::Devices {
            entries: (0..DEVICES_PER_PAGE)
                .map(|_| Device {
                    key: [255; 32],
                    name: "\\".repeat(64),
                    address: Some("[2001:db8::1]:65535".parse().unwrap()),
                    state: ConnectionState::Connecting,
                })
                .collect(),
            more: true,
        });
        assert!(encode(&page).unwrap().len() <= MAX_FRAME_BYTES);
        let discovered = Response::Network(Reply::Discovered {
            entries: (0..DEVICES_PER_PAGE)
                .map(|_| Discovered {
                    key: [255; 32],
                    address: "[2001:db8::1]:65535".parse().unwrap(),
                    paired: true,
                })
                .collect(),
            more: true,
        });
        assert!(encode(&discovered).unwrap().len() <= MAX_FRAME_BYTES);
        let auto = Request::Network(Command::SetClipboardAuto { enabled: true });
        let auto_frame = encode(&auto).unwrap();
        assert!(auto_frame.len() < MAX_FRAME_BYTES);
        assert_eq!(decode::<Request>(&auto_frame).unwrap(), auto);
        let upload = Request::Network(Command::BulkUpload {
            peers: vec!["ff".repeat(32); MAX_RECIPIENTS],
            media: Media::Webp,
            size: crate::peer::MAX_IMAGE_BYTES as u32,
        });
        assert!(encode(&upload).unwrap().len() <= MAX_FRAME_BYTES);
        let report = Response::Network(Reply::BulkQueued(
            (0..MAX_RECIPIENTS)
                .map(|_| QueueOutcome::Error(char::from(92).to_string().repeat(48)))
                .collect(),
        ));
        assert!(encode(&report).unwrap().len() <= MAX_FRAME_BYTES);
        let qr = Request::Network(Command::Accept {
            qr: "private-qr".into(),
        });
        let code = Response::Network(Reply::Pair(PairStatus::Compare {
            code: "private-code".into(),
        }));
        assert_eq!(
            format!("{qr:?} {code:?}"),
            "Network(NetworkCommand([redacted])) Network(NetworkReply([redacted]))"
        );
    }

    #[test]
    fn history_metadata_and_max_recipient_request_fit_control_frames() {
        let page = Response::Network(Reply::HistoryPage {
            entries: (0..HISTORY_PER_PAGE)
                .map(|_| HistoryEntry {
                    id: i64::MAX,
                    timestamp: i64::MAX,
                    media: Media::Text,
                    direction: HistoryDirection::Received,
                    peer: "ff".repeat(32),
                    size: u32::MAX,
                })
                .collect(),
            more: true,
        });
        let resend = Request::Network(Command::History(HistoryCommand::Resend {
            peers: vec!["ff".repeat(32); MAX_RECIPIENTS],
            id: i64::MAX,
        }));
        let encoded = encode(&page).unwrap();
        assert!(encoded.len() <= MAX_FRAME_BYTES);
        assert_eq!(decode::<Response>(&encoded).unwrap(), page);
        let encoded = encode(&resend).unwrap();
        assert!(encoded.len() <= MAX_FRAME_BYTES);
        assert_eq!(decode::<Request>(&encoded).unwrap(), resend);
    }
}
