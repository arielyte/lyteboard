//! Request and response bodies for the local daemon protocol.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use thiserror::Error;

pub mod network;

pub const VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 4096;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Ping,
    GetStatus,
    GetVersion,
    Shutdown,
    Network(network::Command),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Response {
    Pong,
    Status(Status),
    Version(String),
    ShuttingDown,
    Network(network::Reply),
    Error(ErrorCode),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Status {
    pub running: bool,
    pub version: String,
    pub pid: u32,
    pub uptime_seconds: u64,
    pub protocol_version: u16,
    /// Total same-user connections accepted since this daemon started.
    pub connections_accepted: u64,
}

#[derive(Debug, Clone, Copy, Error, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    #[error("invalid IPC request")]
    InvalidRequest,
    #[error("unsupported IPC protocol version")]
    UnsupportedVersion,
    #[error("IPC frame exceeds the size limit")]
    FrameTooLarge,
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("invalid IPC message")]
    Invalid(#[from] serde_json::Error),
    #[error("unsupported IPC protocol version {0}")]
    UnsupportedVersion(u16),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u16,
    body: T,
}

pub fn encode<T: Serialize>(body: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&Envelope {
        version: VERSION,
        body,
    })
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    let envelope: Envelope<&RawValue> = serde_json::from_slice(bytes)?;
    if envelope.version != VERSION {
        return Err(DecodeError::UnsupportedVersion(envelope.version));
    }

    Ok(serde_json::from_str(envelope.body.get())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_round_trips() {
        let bytes = encode(&Request::Ping).unwrap();
        assert_eq!(decode::<Request>(&bytes).unwrap(), Request::Ping);
    }

    #[test]
    fn future_version_is_rejected_before_decoding_operation() {
        assert!(matches!(
            decode::<Request>(br#"{"version":65535,"body":"future_operation"}"#),
            Err(DecodeError::UnsupportedVersion(65535))
        ));
    }

    #[test]
    fn invalid_envelopes_and_requests_are_rejected() {
        for invalid in [
            r#"{"version":1,"body":"send_clipboard"}"#,
            r#"{"version":1,"body":"ping","uid":1000}"#,
            r#"{"version":1,"version":1,"body":"ping"}"#,
            r#"{"version":1,"body":"ping"}null"#,
        ] {
            assert!(matches!(
                decode::<Request>(invalid.as_bytes()),
                Err(DecodeError::Invalid(_))
            ));
        }
    }
}
