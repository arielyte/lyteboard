//! Shared Lyteboard application and platform integration code.

pub mod bulk;
#[cfg(target_os = "linux")]
pub mod clipboard;
pub mod ipc;
#[cfg(target_os = "windows")]
pub use lyteboard_windows as windows;
#[cfg(target_os = "windows")]
#[path = "clipboard_unsupported.rs"]
pub mod clipboard;
pub mod peer;

/// Build version shared by the workspace applications.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
