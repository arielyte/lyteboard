use std::{collections::HashSet, time::Duration};

use clap::{Subcommand, ValueEnum};
use lyteboard_core::{
    clipboard::Clipboard,
    ipc::{
        Client,
        network::{Command as Request, Delivery, MAX_RECIPIENTS, QueueOutcome, Reply},
    },
};
use tokio::time::{Instant, sleep, timeout_at};

use crate::{
    linux::CommandError,
    network::{parse_key, request, unexpected},
};

#[derive(Subcommand)]
pub enum ClipboardCommand {
    /// Send the current clipboard text or image to one or more paired devices.
    Send {
        #[arg(required = true, value_parser = parse_key)]
        devices: Vec<[u8; 32]>,
    },
    /// Put a received message on the local clipboard, newest first.
    Copy {
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
        index: u8,
    },
    /// Check or change automatic application of newly received text.
    Auto {
        #[arg(value_enum, default_value = "status")]
        mode: AutoMode,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum AutoMode {
    On,
    Off,
    Status,
}

pub async fn run(client: &Client, command: ClipboardCommand) -> Result<(), CommandError> {
    match command {
        ClipboardCommand::Send { devices } => send_clipboard(client, devices).await,
        ClipboardCommand::Copy { index } => {
            let clipboard = Clipboard::for_session().map_err(clipboard_error)?;
            let (media, bytes) = client
                .download_bulk(usize::from(index - 1))
                .await?
                .ok_or_else(|| {
                    CommandError::Network("No received content at that index.".into())
                })?;
            clipboard
                .write_content(media, &bytes)
                .await
                .map_err(clipboard_error)?;
            println!(
                "Copied received {}",
                if media == lyteboard_core::bulk::Media::Text {
                    "text"
                } else {
                    "image"
                }
            );
            Ok(())
        }
        ClipboardCommand::Auto { mode } => {
            let reply = request(
                client,
                match mode {
                    AutoMode::On => Request::SetClipboardAuto { enabled: true },
                    AutoMode::Off => Request::SetClipboardAuto { enabled: false },
                    AutoMode::Status => Request::ClipboardAutoStatus,
                },
            )
            .await?;
            let Reply::ClipboardAuto {
                enabled,
                last_error,
            } = reply
            else {
                return Err(unexpected());
            };
            println!(
                "Automatic clipboard application {}",
                if enabled { "on" } else { "off" }
            );
            if let Some(error) = last_error {
                eprintln!("Last automatic write failed. {error}");
            }
            Ok(())
        }
    }
}

async fn send_clipboard(client: &Client, mut peers: Vec<[u8; 32]>) -> Result<(), CommandError> {
    let mut seen = HashSet::new();
    peers.retain(|peer| seen.insert(*peer));
    if peers.len() > MAX_RECIPIENTS {
        return Err(CommandError::Network(
            "Select at most 32 paired devices.".into(),
        ));
    }

    let clipboard = Clipboard::for_session().map_err(clipboard_error)?;
    let (media, bytes) = clipboard.read_content().await.map_err(clipboard_error)?;
    let total = peers.len();
    let keys = peers.iter().map(hex::encode).collect();
    let outcomes = client.upload_bulk(keys, media, &bytes).await?;
    if outcomes.len() != total {
        return Err(unexpected());
    }
    let mut delivered = 0;
    let mut pending = Vec::new();
    for (peer, outcome) in peers.into_iter().zip(outcomes) {
        match outcome {
            QueueOutcome::Queued(id) => pending.push((peer, id)),
            QueueOutcome::Error(error) => eprintln!("{}: {error}", hex::encode(peer)),
        }
    }
    drop(bytes);

    let deadline = Instant::now() + Duration::from_secs(75);
    while !pending.is_empty() && Instant::now() < deadline {
        let mut next = Vec::new();
        for (peer, id) in pending {
            match timeout_at(deadline, request(client, Request::Delivery { id })).await {
                Ok(Ok(Reply::Delivery(Delivery::Pending))) => next.push((peer, id)),
                Ok(Ok(Reply::Delivery(Delivery::Delivered))) => delivered += 1,
                Ok(Ok(Reply::Delivery(Delivery::Failed(error)))) => {
                    eprintln!("{}: {error}", hex::encode(peer))
                }
                Ok(Err(error)) => eprintln!("{}: {error}", hex::encode(peer)),
                Ok(Ok(_)) => eprintln!("{}: {}", hex::encode(peer), unexpected()),
                Err(_) => next.push((peer, id)),
            }
        }
        pending = next;
        if !pending.is_empty() {
            sleep(Duration::from_millis(100)).await;
        }
    }

    for (peer, _) in pending {
        eprintln!("{}: delivery was not confirmed in time.", hex::encode(peer));
    }
    println!("Clipboard delivered to {delivered}/{total} devices");
    if delivered == total {
        Ok(())
    } else {
        Err(CommandError::Network(
            "Some clipboard deliveries failed.".into(),
        ))
    }
}

fn clipboard_error(error: lyteboard_core::clipboard::Error) -> CommandError {
    CommandError::Network(error.to_string())
}
