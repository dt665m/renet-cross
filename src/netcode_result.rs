use std::net::SocketAddr;

use renet::ClientId;
use renetcode::ServerResult;

#[derive(Debug)]
pub(crate) enum OwnedServerResult {
    None,
    PacketToSend {
        addr: SocketAddr,
        payload: Vec<u8>,
    },
    Payload {
        client_id: ClientId,
        payload: Vec<u8>,
    },
    ClientConnected {
        client_id: ClientId,
        addr: SocketAddr,
        payload: Vec<u8>,
    },
    ClientDisconnected {
        client_id: ClientId,
        addr: SocketAddr,
        payload: Option<Vec<u8>>,
    },
}

pub(crate) fn to_owned_server_result(server_result: ServerResult<'_, '_>) -> OwnedServerResult {
    match server_result {
        ServerResult::None => OwnedServerResult::None,
        ServerResult::PacketToSend { addr, payload } => OwnedServerResult::PacketToSend {
            addr,
            payload: payload.to_vec(),
        },
        ServerResult::Payload { client_id, payload } => OwnedServerResult::Payload {
            client_id,
            payload: payload.to_vec(),
        },
        ServerResult::ClientConnected {
            client_id,
            user_data: _,
            addr,
            payload,
        } => OwnedServerResult::ClientConnected {
            client_id,
            addr,
            payload: payload.to_vec(),
        },
        ServerResult::ClientDisconnected {
            client_id,
            addr,
            payload,
        } => OwnedServerResult::ClientDisconnected {
            client_id,
            addr,
            payload: payload.map(|value| value.to_vec()),
        },
    }
}
