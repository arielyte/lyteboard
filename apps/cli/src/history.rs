use clap::Subcommand;
use lyteboard_core::{
    clipboard::Clipboard,
    ipc::{
        Client,
        network::{Command, HistoryCommand as Request, HistoryDirection, HistoryLimits, Reply},
    },
};

use crate::{
    clipboard::{clipboard_error, report_deliveries, selection},
    linux::CommandError,
    network::{parse_key, request, unexpected},
};

#[derive(Subcommand)]
pub enum HistoryCommand {
    /// Show whether persistent history is enabled and its retention limits.
    Status,
    /// Enable persistent clipboard history.
    Enable,
    /// Disable history and keep the saved database.
    Disable,
    /// Set the maximum entries, MiB, and days to retain.
    Retention {
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10000))]
        entries: u32,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=1024))]
        mib: u32,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=3650))]
        days: u32,
    },
    /// List entry metadata from newest to oldest, one page at a time.
    List {
        /// Continue with entries older than this ID.
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        before: Option<i64>,
        /// Find literal text in saved text items.
        #[arg(long)]
        search: Option<String>,
    },
    /// Copy a saved entry into the local clipboard.
    Restore {
        #[arg(value_parser = clap::value_parser!(i64).range(1..))]
        id: i64,
    },
    /// Send a saved entry to one or more currently trusted devices.
    Resend {
        #[arg(value_parser = clap::value_parser!(i64).range(1..))]
        id: i64,
        #[arg(required = true, value_parser = parse_key)]
        devices: Vec<[u8; 32]>,
    },
    /// Delete one entry.
    Delete {
        #[arg(value_parser = clap::value_parser!(i64).range(1..))]
        id: i64,
    },
    /// Delete all history entries.
    Clear {
        /// Confirm deletion of all saved entries.
        #[arg(long)]
        yes: bool,
    },
}

pub async fn run(client: &Client, command: HistoryCommand) -> Result<(), CommandError> {
    match command {
        HistoryCommand::Status => match request(client, Command::History(Request::Status)).await? {
            Reply::HistoryStatus(Some(limits)) => println!(
                "History on ({} entries, {} MiB, {} days)",
                limits.entries, limits.mib, limits.days
            ),
            Reply::HistoryStatus(None) => println!("History off"),
            _ => return Err(unexpected()),
        },
        HistoryCommand::Enable | HistoryCommand::Disable | HistoryCommand::Retention { .. } => {
            let Reply::HistoryConfiguration(current) =
                request(client, Command::History(Request::Configuration)).await?
            else {
                return Err(unexpected());
            };
            let (enabled, limits) = match command {
                HistoryCommand::Enable => (true, current.limits),
                HistoryCommand::Disable => (false, current.limits),
                HistoryCommand::Retention { entries, mib, days } => {
                    (current.enabled, HistoryLimits { entries, mib, days })
                }
                _ => unreachable!(),
            };
            let Reply::HistoryConfiguration(saved) = request(
                client,
                Command::History(Request::Configure { enabled, limits }),
            )
            .await?
            else {
                return Err(unexpected());
            };
            if let Some(error) = saved.error {
                return Err(CommandError::Network(error));
            }
            println!(
                "History {} ({} entries, {} MiB, {} days)",
                if saved.enabled { "on" } else { "off" },
                saved.limits.entries,
                saved.limits.mib,
                saved.limits.days
            );
        }
        HistoryCommand::List { before, search } => {
            let Reply::HistoryPage { entries, more } = request(
                client,
                Command::History(match search {
                    Some(query) => Request::Search { before, query },
                    None => Request::List { before },
                }),
            )
            .await?
            else {
                return Err(unexpected());
            };
            if entries.is_empty() {
                println!("No history entries");
            } else {
                println!("ID\tUNIX TIME\tDIRECTION\tCONTENT TYPE\tBYTES\tDEVICE");
                for entry in &entries {
                    let direction = match entry.direction {
                        HistoryDirection::Sent => "sent",
                        HistoryDirection::Received => "received",
                    };
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        entry.id,
                        entry.timestamp,
                        direction,
                        entry.media.mime(),
                        entry.size,
                        entry.peer
                    );
                }
                if more && let Some(last) = entries.last() {
                    println!("Next page: lyteboard history list --before {}", last.id);
                }
            }
        }
        HistoryCommand::Restore { id } => {
            let (media, bytes) = client.download_history(id).await?.ok_or_else(|| {
                CommandError::Network("History entry was not found or has expired.".into())
            })?;
            let clipboard = Clipboard::for_session().map_err(clipboard_error)?;
            clipboard
                .write_content(media, &bytes)
                .await
                .map_err(clipboard_error)?;
            println!("Restored history entry {id}");
        }
        HistoryCommand::Resend { id, devices } => {
            let devices = selection(devices)?;
            let peers = devices.iter().map(hex::encode).collect();
            let Reply::BulkQueued(outcomes) =
                request(client, Command::History(Request::Resend { id, peers })).await?
            else {
                return Err(unexpected());
            };
            report_deliveries(client, devices, outcomes).await?;
        }
        HistoryCommand::Delete { id } => {
            change(client, Request::Delete { id }).await?;
            println!("Deleted history entry {id}");
        }
        HistoryCommand::Clear { yes } => {
            if !yes {
                return Err(CommandError::Network(
                    "Use history clear --yes to confirm.".into(),
                ));
            }
            change(client, Request::Clear).await?;
            println!("Cleared history");
        }
    }
    Ok(())
}

async fn change(client: &Client, command: Request) -> Result<(), CommandError> {
    match request(client, Command::History(command)).await? {
        Reply::Ok => Ok(()),
        _ => Err(unexpected()),
    }
}
