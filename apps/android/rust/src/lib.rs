//! Typed entry points for the Android application.

uniffi::setup_scaffolding!();

#[derive(uniffi::Record)]
pub struct CoreInfo {
    pub core_version: String,
    pub protocol_version: u8,
}

#[uniffi::export]
pub fn core_info() -> CoreInfo {
    CoreInfo {
        core_version: lyteboard_core::VERSION.into(),
        protocol_version: lyteboard_protocol::peer::VERSION,
    }
}
