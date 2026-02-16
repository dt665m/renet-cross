use std::io;

use renet::ClientId;
use renetcode::{NetcodeError, TokenGenerationError};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Netcode(#[from] NetcodeError),
    #[error("renet disconnect: {0}")]
    Renet(renet::DisconnectReason),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Rtc(#[from] str0m::RtcError),
    #[error("data channel is not open for client {client_id}")]
    DataChannelNotOpen { client_id: ClientId },
    #[error("data channel backpressure for client {client_id}")]
    DataChannelBackpressure { client_id: ClientId },
    #[error("no registered str0m peer for client {client_id}")]
    MissingPeer { client_id: ClientId },
}

impl From<TokenGenerationError> for TransportError {
    fn from(inner: TokenGenerationError) -> Self {
        Self::Netcode(NetcodeError::TokenGenerationError(inner))
    }
}

impl From<renet::DisconnectReason> for TransportError {
    fn from(inner: renet::DisconnectReason) -> Self {
        Self::Renet(inner)
    }
}
