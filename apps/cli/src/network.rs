use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    net::SocketAddr,
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};

use clap::{Subcommand, ValueEnum};
use lyteboard_core::ipc::{
    Client,
    network::{
        Command as Request, ConnectionState, DEVICES_PER_PAGE, Delivery, DiscoveryState,
        MAX_TEST_TEXT_BYTES, PairStatus, Reply,
    },
};
use tokio::time::{sleep, timeout};
use zeroize::Zeroizing;

use crate::{Command, linux::CommandError};

#[derive(Subcommand)]
pub enum NetworkCommand {
    /// Show the listener address and active peer sessions.
    Status,
    /// Set the listener IP and port for this daemon run.
    Listen { address: SocketAddr },
    /// List unverified connection hints found by mDNS.
    Discover,
    /// Enable or disable mDNS for this daemon run.
    Discovery { mode: DiscoveryMode },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum DiscoveryMode {
    On,
    Off,
}

#[derive(Subcommand)]
pub enum PairCommand {
    /// Create a single-use invitation. Pass --qr to render it in the terminal.
    Invite {
        #[arg(long)]
        qr: bool,
    },
    /// Read an invitation from standard input and start pairing.
    Accept,
    /// Show the pairing state and the code to compare.
    Status,
    /// Read the verified comparison code from standard input and approve locally.
    Confirm,
    /// Cancel the current pairing or invitation.
    Cancel,
}

#[derive(Subcommand)]
pub enum DeviceCommand {
    /// List paired devices and their identifiers.
    List,
    /// Change a paired device's local display name.
    Rename {
        #[arg(value_parser = parse_key)]
        device: [u8; 32],
        name: String,
    },
    /// Save an IP:port fallback for an already paired device.
    Address {
        #[arg(value_parser = parse_key)]
        device: [u8; 32],
        address: SocketAddr,
    },
    /// Revoke a paired device and close its sessions.
    Remove {
        #[arg(value_parser = parse_key)]
        device: [u8; 32],
    },
}

pub fn parse_key(value: &str) -> Result<[u8; 32], String> {
    let mut key = [0; 32];
    hex::decode_to_slice(value, &mut key)
        .map_err(|_| "Expected the 64-character device ID from `devices list`.")?;
    Ok(key)
}

pub async fn run(client: &Client, command: Command) -> Result<(), CommandError> {
    match command {
        Command::Network { command } => {
            let reply = match command {
                NetworkCommand::Status => request(client, Request::Status).await?,
                NetworkCommand::Listen { address } => {
                    request(client, Request::Listen { address }).await?
                }
                NetworkCommand::Discovery { mode } => {
                    request(
                        client,
                        Request::SetDiscovery {
                            enabled: matches!(mode, DiscoveryMode::On),
                        },
                    )
                    .await?
                }
                NetworkCommand::Discover => return list_discovered(client).await,
            };
            let Reply::Status {
                listener,
                sessions,
                discovery,
            } = reply
            else {
                return Err(unexpected());
            };
            let discovery = match discovery {
                DiscoveryState::Off => "off",
                DiscoveryState::Loopback => "paused on loopback",
                DiscoveryState::Running => "on",
                DiscoveryState::Unavailable => "unavailable",
            };
            println!("Listening on {listener} ({sessions} active sessions); discovery {discovery}");
        }
        Command::Pair { command } => match command {
            PairCommand::Invite { qr } => {
                let Reply::Invite(invite) = request(client, Request::Invite).await? else {
                    return Err(unexpected());
                };
                let invite = Zeroizing::new(invite);
                if qr {
                    print_qr(&invite)?;
                } else {
                    println!("{invite}", invite = invite.as_str());
                }
            }
            PairCommand::Accept => {
                let qr = read_secret("Paste the invitation:", 512)?;
                expect_ok(request(client, Request::Accept { qr: qr.to_string() }).await?)?;
                println!("Pairing started. Run `pair status` on both devices.");
            }
            PairCommand::Status => {
                let Reply::Pair(status) = request(client, Request::PairStatus).await? else {
                    return Err(unexpected());
                };
                match status {
                    PairStatus::Idle => println!("No pairing in progress"),
                    PairStatus::Connecting => println!("Connecting"),
                    PairStatus::Compare { code } => println!("Verification code: {code}"),
                    PairStatus::Approving => println!("Local approval recorded. Waiting for peer"),
                    PairStatus::Paired => println!("Paired"),
                    PairStatus::Failed(error) => return Err(CommandError::Network(error)),
                }
            }
            PairCommand::Confirm => {
                let code = read_secret(
                    "After comparing both displays, enter the verification code:",
                    19,
                )?;
                expect_ok(
                    request(
                        client,
                        Request::Confirm {
                            code: code.to_string(),
                        },
                    )
                    .await?,
                )?;
                println!("Approval recorded. Run `pair status` to check the result.");
            }
            PairCommand::Cancel => {
                expect_ok(request(client, Request::CancelPair).await?)?;
                println!("Pairing canceled");
            }
        },
        Command::Devices { command } => match command {
            DeviceCommand::List => list_devices(client).await?,
            DeviceCommand::Rename { device: peer, name } => {
                expect_ok(request(client, Request::Rename { peer, name }).await?)?;
                println!("Device renamed");
            }
            DeviceCommand::Address {
                device: peer,
                address,
            } => {
                expect_ok(request(client, Request::SetAddress { peer, address }).await?)?;
                println!("Peer address saved");
            }
            DeviceCommand::Remove { device: peer } => {
                expect_ok(request(client, Request::Remove { peer }).await?)?;
                println!("Device removed");
            }
        },
        Command::Send { device: peer, text } => send_text(client, peer, text).await?,
        Command::Received { limit } => {
            for offset in 0..usize::from(limit) {
                let Reply::Received(message) =
                    request(client, Request::Received { offset }).await?
                else {
                    return Err(unexpected());
                };
                let Some(message) = message else {
                    if offset == 0 {
                        println!("No received messages");
                    }
                    break;
                };
                println!(
                    "{}\t{}",
                    hex::encode(message.peer),
                    message.text.escape_debug()
                );
            }
        }
        _ => return Err(unexpected()),
    }
    Ok(())
}

async fn list_devices(client: &Client) -> Result<(), CommandError> {
    let mut offset = 0;
    loop {
        let Reply::Devices { entries, more } = request(client, Request::Devices { offset }).await?
        else {
            return Err(unexpected());
        };
        if offset == 0 && entries.is_empty() {
            println!("No paired devices");
        }
        for device in entries {
            let address = device
                .address
                .map(|address| address.to_string())
                .unwrap_or_else(|| "-".into());
            let state = match device.state {
                ConnectionState::Offline => "offline",
                ConnectionState::Connecting => "connecting",
                ConnectionState::Online => "online",
            };
            println!(
                "{}\t{}\t{address}\t{state}",
                hex::encode(device.key),
                device.name
            );
        }
        if !more {
            return Ok(());
        }
        offset += DEVICES_PER_PAGE;
    }
}

async fn list_discovered(client: &Client) -> Result<(), CommandError> {
    let mut offset = 0;
    loop {
        let Reply::Discovered { entries, more } =
            request(client, Request::Discover { offset }).await?
        else {
            return Err(unexpected());
        };
        if offset == 0 && entries.is_empty() {
            println!("No discovered devices");
        }
        for device in entries {
            let identity = if device.paired {
                "paired key, unverified address"
            } else {
                "unverified"
            };
            println!(
                "{}\t{}\t{identity}",
                hex::encode(device.key),
                device.address
            );
        }
        if !more {
            return Ok(());
        }
        offset += DEVICES_PER_PAGE;
    }
}

async fn send_text(client: &Client, peer: [u8; 32], text: String) -> Result<(), CommandError> {
    if text.is_empty() || text.len() > MAX_TEST_TEXT_BYTES || text.contains('\0') {
        return Err(CommandError::Network(format!(
            "Text must be 1–{MAX_TEST_TEXT_BYTES} UTF-8 bytes without NUL."
        )));
    }
    let Reply::Queued(id) = request(client, Request::Send { peer, text }).await? else {
        return Err(unexpected());
    };
    timeout(Duration::from_secs(30), async {
        loop {
            match request(client, Request::Delivery { id }).await? {
                Reply::Delivery(Delivery::Pending) => sleep(Duration::from_millis(100)).await,
                Reply::Delivery(Delivery::Delivered) => {
                    println!("Delivered");
                    return Ok(());
                }
                Reply::Delivery(Delivery::Failed(error)) => {
                    return Err(CommandError::Network(error));
                }
                _ => return Err(unexpected()),
            }
        }
    })
    .await
    .map_err(|_| CommandError::Network("Delivery was not confirmed in time.".into()))?
}

async fn request(client: &Client, command: Request) -> Result<Reply, CommandError> {
    match client.network(command).await? {
        Reply::Error(error) => Err(CommandError::Network(error)),
        reply => Ok(reply),
    }
}

fn expect_ok(reply: Reply) -> Result<(), CommandError> {
    if matches!(reply, Reply::Ok) {
        Ok(())
    } else {
        Err(unexpected())
    }
}

fn unexpected() -> CommandError {
    CommandError::Network("Unexpected response from the LAN worker.".into())
}

fn read_secret(prompt: &str, limit: usize) -> Result<Zeroizing<String>, CommandError> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        eprintln!("{prompt}");
    }
    let mut value = Zeroizing::new(String::new());
    stdin
        .lock()
        .take((limit + 2) as u64)
        .read_line(&mut value)
        .map_err(CommandError::Terminal)?;
    let length = value.trim_end_matches(['\r', '\n']).len();
    value.truncate(length);
    if length == 0 || length > limit {
        return Err(CommandError::Network("Invalid input length.".into()));
    }
    Ok(value)
}

fn print_qr(invite: &str) -> Result<(), CommandError> {
    let mut child = ProcessCommand::new("qrencode")
        .args(["-t", "ANSIUTF8", "-o", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                CommandError::Network("Install qrencode to display a QR, or run `pair invite` for the pasteable invitation.".into())
            } else {
                CommandError::Terminal(error)
            }
        })?;
    let write = child
        .stdin
        .take()
        .expect("qrencode stdin is piped")
        .write_all(invite.as_bytes());
    let status = child.wait().map_err(CommandError::Terminal)?;
    write.map_err(CommandError::Terminal)?;
    if !status.success() {
        return Err(CommandError::Network(
            "Could not render the QR invitation.".into(),
        ));
    }
    Ok(())
}
