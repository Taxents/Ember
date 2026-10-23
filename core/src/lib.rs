//! Music core: audio engine, YouTube Music client and player logic. No UI dependencies.

pub mod audio;
pub mod net;
pub mod paths;
pub mod player;
pub mod yt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub enum Error {
    /// Transport or HTTP failure.
    Net(String),
    /// YouTube refused or returned something we can't use (e.g. not playable).
    Unavailable(String),
    /// Malformed or unsupported media.
    Media(String),
    /// Audio device failure.
    Device(String),
    /// Work was cancelled (e.g. a stream was dropped while a read was waiting).
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Net(s) => write!(f, "network: {s}"),
            Error::Unavailable(s) => write!(f, "unavailable: {s}"),
            Error::Media(s) => write!(f, "media: {s}"),
            Error::Device(s) => write!(f, "audio device: {s}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::other(e)
    }
}
