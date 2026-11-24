//! Linux clipboard access and change notifications.

pub mod monitor;

use std::{env, ffi::OsStr, io, process::Stdio, time::Duration};

use lyteboard_protocol::peer::Media;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::timeout,
};
use zeroize::Zeroizing;

const TOOL_TIMEOUT: Duration = Duration::from_millis(1500);
const DIAGNOSTIC_LIMIT: u64 = 1024;
const TARGETS_LIMIT: usize = 4096;

#[derive(Debug, Error)]
pub enum Error {
    #[error("No graphical clipboard session. Run from your Wayland or X11 session.")]
    NoSession,
    #[error("Missing {tool}. Install the {package} package and retry.")]
    Missing {
        tool: &'static str,
        package: &'static str,
    },
    #[error("Clipboard access failed. {0}")]
    Access(&'static str),
    #[error("Clipboard access timed out. {0}")]
    Timeout(&'static str),
    #[error("Clipboard has no text.")]
    Empty,
    #[error("Clipboard content exceeds the {0}-byte transfer limit.")]
    TooLarge(usize),
    #[error("Clipboard has no supported text or image.")]
    NoSupportedContent,
    #[error("Clipboard text must be UTF-8 without NUL bytes.")]
    InvalidText,
    #[error("Clipboard monitoring is unavailable. {0}")]
    Monitor(&'static str),
}

#[derive(Clone, Copy)]
pub struct Clipboard(Backend);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Wayland,
    X11,
}

impl Clipboard {
    pub fn for_session() -> Result<Self, Error> {
        let session = env::var_os("XDG_SESSION_TYPE");
        let wayland = env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let x11 = env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
        select_backend(session.as_deref(), wayland, x11)
            .map(Self)
            .ok_or(Error::NoSession)
    }

    pub async fn read_content(self) -> Result<(Media, Zeroizing<Vec<u8>>), Error> {
        self.read_selection(false)
            .await?
            .ok_or(Error::NoSupportedContent)
    }

    async fn read_selection(
        self,
        unattended: bool,
    ) -> Result<Option<(Media, Zeroizing<Vec<u8>>)>, Error> {
        let (targets_tool, targets_args) = self.0.targets();
        let targets = self
            .read_bytes(targets_tool, targets_args, TARGETS_LIMIT)
            .await?;
        let targets = std::str::from_utf8(&targets).map_err(|_| self.0.access())?;
        if unattended
            && targets
                .lines()
                .any(|target| monitor::is_sensitive(target.trim()))
        {
            return Ok(None);
        }
        let media = select_media(targets).ok_or(Error::NoSupportedContent)?;
        let (tool, args) = self.0.reader(media);
        let bytes = self.read_bytes(tool, &args, media.limit()).await?;
        if media == Media::Text {
            let text = std::str::from_utf8(&bytes).map_err(|_| Error::InvalidText)?;
            validate_text(text, media.limit())?;
        } else if bytes.is_empty() {
            return Err(Error::NoSupportedContent);
        }
        if unattended {
            let current = self
                .read_bytes(targets_tool, targets_args, TARGETS_LIMIT)
                .await?;
            if current.as_slice() != targets.as_bytes() {
                return Ok(None);
            }
        }
        Ok(Some((media, bytes)))
    }

    async fn read_bytes(
        self,
        tool: &'static str,
        args: &[&str],
        limit: usize,
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        let mut child = self
            .0
            .spawn(tool, args, Stdio::null(), Stdio::piped(), Stdio::piped())?;
        let stdout = child.stdout.take().expect("clipboard reader pipes stdout");
        let stderr = child.stderr.take().expect("clipboard reader pipes stderr");
        timeout(TOOL_TIMEOUT, async {
            let read_text = async {
                let mut text = Zeroizing::new(Vec::with_capacity(limit.saturating_add(1)));
                stdout
                    .take(limit.saturating_add(1) as u64)
                    .read_to_end(&mut text)
                    .await
                    .map_err(|_| self.0.access())?;
                if text.len() > limit {
                    return Err(Error::TooLarge(limit));
                }
                Ok(text)
            };
            let read_diagnostic = async {
                let mut diagnostic = Vec::new();
                stderr
                    .take(DIAGNOSTIC_LIMIT)
                    .read_to_end(&mut diagnostic)
                    .await
                    .map_err(|_| self.0.access())?;
                Ok(diagnostic)
            };
            let (text, diagnostic) = tokio::try_join!(read_text, read_diagnostic)?;
            let status = child.wait().await.map_err(|_| self.0.access())?;
            if !status.success() {
                return Err(self.0.read_error(&diagnostic));
            }
            Ok(text)
        })
        .await
        .map_err(|_| Error::Timeout(self.0.hint()))?
    }

    pub async fn write_text(self, text: &str, limit: usize) -> Result<(), Error> {
        validate_text(text, limit)?;
        self.write_content(Media::Text, text.as_bytes()).await
    }

    pub async fn write_content(self, media: Media, bytes: &[u8]) -> Result<(), Error> {
        if media == Media::Text {
            validate_text(
                std::str::from_utf8(bytes).map_err(|_| Error::InvalidText)?,
                media.limit(),
            )?;
        } else if bytes.is_empty() {
            return Err(Error::NoSupportedContent);
        } else if bytes.len() > media.limit() {
            return Err(Error::TooLarge(media.limit()));
        }
        let (tool, args) = self.0.writer(media);
        // The owner can fork to serve later clipboard reads. Never hold its stdout or stderr open.
        let mut child = self
            .0
            .spawn(tool, &args, Stdio::piped(), Stdio::null(), Stdio::null())?;
        let mut stdin = child.stdin.take().expect("clipboard writer pipes stdin");
        timeout(TOOL_TIMEOUT, async {
            stdin.write_all(bytes).await.map_err(|_| self.0.access())?;
            drop(stdin);
            let status = child.wait().await.map_err(|_| self.0.access())?;
            if status.success() {
                Ok(())
            } else {
                Err(self.0.access())
            }
        })
        .await
        .map_err(|_| Error::Timeout(self.0.hint()))?
    }

    /// Check that the writer is installed before enabling unattended application.
    pub async fn check_writer(self) -> Result<(), Error> {
        let (tool, _) = self.0.writer(Media::Text);
        let args = if self.0 == Backend::Wayland {
            &["--version"]
        } else {
            &["-version"]
        };
        let mut child = self
            .0
            .spawn(tool, args, Stdio::null(), Stdio::null(), Stdio::null())?;
        let status = timeout(TOOL_TIMEOUT, child.wait())
            .await
            .map_err(|_| Error::Timeout(self.0.hint()))?
            .map_err(|_| self.0.access())?;
        if status.success() {
            Ok(())
        } else {
            Err(self.0.access())
        }
    }
}

fn select_backend(session: Option<&OsStr>, wayland: bool, x11: bool) -> Option<Backend> {
    match session.and_then(OsStr::to_str) {
        Some("wayland") => wayland.then_some(Backend::Wayland),
        Some("x11") => x11.then_some(Backend::X11),
        _ if wayland => Some(Backend::Wayland),
        _ if x11 => Some(Backend::X11),
        _ => None,
    }
}

impl Backend {
    fn targets(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Wayland => ("wl-paste", &["--list-types"]),
            Self::X11 => (
                "xclip",
                &["-selection", "clipboard", "-out", "-target", "TARGETS"],
            ),
        }
    }

    fn reader(self, media: Media) -> (&'static str, Vec<&'static str>) {
        match self {
            Self::Wayland => (
                "wl-paste",
                vec![
                    "--no-newline",
                    "--type",
                    if media == Media::Text {
                        "text"
                    } else {
                        media.mime()
                    },
                ],
            ),
            Self::X11 => (
                "xclip",
                vec![
                    "-selection",
                    "clipboard",
                    "-out",
                    "-target",
                    if media == Media::Text {
                        "UTF8_STRING"
                    } else {
                        media.mime()
                    },
                ],
            ),
        }
    }

    fn writer(self, media: Media) -> (&'static str, Vec<&'static str>) {
        match self {
            Self::Wayland => ("wl-copy", vec!["--type", media.mime(), "--sensitive"]),
            Self::X11 => (
                "xclip",
                vec![
                    "-selection",
                    "clipboard",
                    "-in",
                    "-target",
                    if media == Media::Text {
                        "UTF8_STRING"
                    } else {
                        media.mime()
                    },
                    "-silent",
                ],
            ),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Wayland => {
                "Use wl-clipboard 2.3 or newer in an unlocked Wayland session with clipboard access."
            }
            Self::X11 => "Check DISPLAY and XAUTHORITY in your X11 session.",
        }
    }

    fn access(self) -> Error {
        Error::Access(self.hint())
    }

    fn spawn(
        self,
        tool: &'static str,
        args: &[&str],
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Child, Error> {
        Command::new(tool)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    let package = match self {
                        Self::Wayland => "wl-clipboard",
                        Self::X11 => "xclip",
                    };
                    Error::Missing { tool, package }
                } else {
                    self.access()
                }
            })
    }

    fn read_error(self, diagnostic: &[u8]) -> Error {
        let diagnostic = String::from_utf8_lossy(diagnostic);
        let empty = diagnostic.lines().any(|line| match self {
            Self::Wayland => matches!(
                line,
                "Nothing is copied"
                    | "No selection"
                    | "No suitable type of content copied"
                    | "Clipboard content is not available as requested type \"text\""
            ),
            Self::X11 => matches!(
                line,
                "Error: target UTF8_STRING not available"
                    | "Error: target STRING not available"
                    | "Error: target TARGETS not available"
            ),
        });
        if empty { Error::Empty } else { self.access() }
    }
}

fn select_media(targets: &str) -> Option<Media> {
    for media in [Media::Png, Media::Jpeg, Media::Webp] {
        if targets.lines().any(|target| target.trim() == media.mime()) {
            return Some(media);
        }
    }
    targets
        .lines()
        .any(|target| {
            let target = target.trim();
            matches!(target, "UTF8_STRING" | "STRING" | "TEXT")
                || target
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.eq_ignore_ascii_case("text/plain"))
        })
        .then_some(Media::Text)
}

fn validate_text(text: &str, limit: usize) -> Result<(), Error> {
    if text.is_empty() {
        Err(Error::Empty)
    } else if text.len() > limit {
        Err(Error::TooLarge(limit))
    } else if text.contains('\0') {
        Err(Error::InvalidText)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_sessions_adapter_without_falling_back_from_wayland_to_xwayland() {
        assert!(select_backend(Some(OsStr::new("wayland")), true, true) == Some(Backend::Wayland));
        assert!(select_backend(Some(OsStr::new("wayland")), false, true).is_none());
        assert!(select_backend(Some(OsStr::new("x11")), true, true) == Some(Backend::X11));
        assert!(select_backend(None, false, true) == Some(Backend::X11));
        assert!(select_backend(None, false, false).is_none());
    }

    #[test]
    fn validates_unicode_in_bytes_and_distinguishes_empty_from_unavailable() {
        assert!(validate_text("שלום\n✓", 12).is_ok());
        assert!(matches!(validate_text("é", 1), Err(Error::TooLarge(1))));
        assert!(matches!(validate_text("", 512), Err(Error::Empty)));
        assert!(matches!(
            validate_text("a\0b", 512),
            Err(Error::InvalidText)
        ));
        assert!(matches!(
            Backend::Wayland.read_error(b"Nothing is copied\n"),
            Error::Empty
        ));
        assert!(matches!(
            Backend::X11.read_error(b"Error: target UTF8_STRING not available\n"),
            Error::Empty
        ));
        let error = Backend::Wayland
            .read_error(b"Failed to connect to a Wayland server\nprivate clipboard contents");
        assert!(matches!(error, Error::Access(_)));
        assert!(!error.to_string().contains("private clipboard contents"));
    }
}
