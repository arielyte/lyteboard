//! Shared Lyteboard application and platform integration code.

pub mod bulk;
#[cfg(target_os = "linux")]
pub mod clipboard;
pub mod ipc;
pub mod peer;

/// Build version shared by the workspace applications.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
