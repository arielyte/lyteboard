//! Explicit unavailable adapter for the Windows foundation milestone.
use lyteboard_protocol::peer::Media;
use zeroize::Zeroizing;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Windows clipboard integration is not implemented yet.")]
    Unsupported,
    #[error("Clipboard has no text.")]
    Empty,
    #[error("Clipboard has no supported content.")]
    NoSupportedContent,
}
#[derive(Clone, Copy)]
pub struct Clipboard;
impl Clipboard {
    pub fn for_session() -> Result<Self, Error> {
        Err(Error::Unsupported)
    }
    pub async fn check_writer(self) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    pub async fn read_content(self) -> Result<(Media, Zeroizing<Vec<u8>>), Error> {
        Err(Error::Unsupported)
    }
    pub async fn write_content(self, _: Media, _: &[u8]) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}
pub mod monitor {
    use super::*;
    pub struct Monitor;
    pub struct Change {
        pub at: std::time::Instant,
    }
    impl Monitor {
        pub async fn start(_: Clipboard) -> Result<Self, Error> {
            Err(Error::Unsupported)
        }
        pub async fn changed(&mut self) -> Result<Change, Error> {
            Err(Error::Unsupported)
        }
        pub fn newer(&self) -> bool {
            false
        }
        pub async fn read(&self, _: &Change) -> Result<Option<(Media, Zeroizing<Vec<u8>>)>, Error> {
            Err(Error::Unsupported)
        }
    }
}
