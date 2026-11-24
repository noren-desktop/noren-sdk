use std::fmt;
use std::io;

/// Unified error type returned by `noren-sdk` operations.
#[derive(Debug)]
pub enum SdkError {
    Io(io::Error),
    Security(String),
    Serialization(String),
    Protocol(String),
    PeerNotConnected(String),
    CommandTimeout { target: String, name: String },
    CommandRejected(String),
    Ui(String),
}

impl fmt::Display for SdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Security(s) => write!(f, "Security error: {s}"),
            Self::Serialization(s) => write!(f, "Serialization error: {s}"),
            Self::Protocol(s) => write!(f, "Protocol error: {s}"),
            Self::PeerNotConnected(m) => write!(f, "Peer '{m}' is not connected"),
            Self::CommandTimeout { target, name } => {
                write!(f, "Command '{name}' to '{target}' timed out")
            }
            Self::CommandRejected(s) => write!(f, "Command rejected: {s}"),
            Self::Ui(s) => write!(f, "UI error: {s}"),
        }
    }
}

impl std::error::Error for SdkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SdkError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for SdkError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}
