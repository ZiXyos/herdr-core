//! Fresh, transport-neutral client for classic Herdr's stable endpoint generation 1.
//!
//! This crate consumes server-rendered cells. It does not translate those cells into
//! modern Core PTY streams and never installs or upgrades software on the remote host.

mod framing;
mod protocol;
mod session;

pub use framing::{FrameReader, FrameWriter, frame_bytes, write_frame};
pub use protocol::*;
pub use session::{ClassicEndpoint, ClassicEndpointHandle, EndpointEvent, EndpointResponse, MAX_REQUEST_PARAMS_BYTES};

use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassicError {
    pub kind: &'static str,
    pub message: String,
}

impl ClassicError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new("classic_protocol", message)
    }

    pub fn stale(message: impl Into<String>) -> Self {
        Self::new("classic_stale_state", message)
    }
}

impl Display for ClassicError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ClassicError {}

impl From<std::io::Error> for ClassicError {
    fn from(error: std::io::Error) -> Self {
        Self::new("classic_transport", error.to_string())
    }
}
