//! Shared Lyteboard application and platform integration code.

pub mod ipc;

/// Build version shared by the workspace applications.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
