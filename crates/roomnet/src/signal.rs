//! Signals: what two [direct](crate::presence::Presence::Here) peers in a room say to
//! each other, through their servers, to connect directly (an offer and an answer,
//! say).
//!
//! Like presence, signals are **ephemeral**: never in a room's log, never persisted,
//! never confirmed, and opaque to roomnet. A server routes a [`Signal::Peer`] to the one
//! peer it is addressed to, in the same room only, and only between two peers that are
//! both direct; another node's peer is reached through the node that relays its
//! presence. A server tells its clients how they connect directly with
//! [`Signal::Config`] (its operator's choice, opaque here: which servers help two
//! peers find a path, say).

use codec::varint;

use crate::presence::{rd_bytes, rd_u64, wr_bytes, MAX_PEER};
use crate::wire::WireError;

/// The largest signal a peer sends.
pub const MAX_DATA: usize = 64 * 1024;

/// Wire tags, distinct from every sync and presence tag.
const T_PEER: u64 = 0x12;
const T_CONFIG: u64 = 0x13;

/// One signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// From peer `from` to peer `to` in a room: `data`.
    Peer { from: Vec<u8>, to: Vec<u8>, data: Vec<u8> },
    /// From the server to its clients: how they connect directly.
    Config { data: Vec<u8> },
}

impl Signal {
    /// Whether the signal is within the size limits.
    pub fn fits(&self) -> bool {
        match self {
            Self::Peer { from, to, data } => {
                !from.is_empty() && from.len() <= MAX_PEER && !to.is_empty() && to.len() <= MAX_PEER && from != to && data.len() <= MAX_DATA
            }
            Self::Config { data } => data.len() <= MAX_DATA,
        }
    }
}

/// Whether a frame body (after the room and sender) is a signal.
pub fn is_signal(body: &[u8]) -> bool {
    let mut b = body;
    matches!(varint::read(&mut b), Ok(T_PEER | T_CONFIG))
}

/// Encode one signal.
pub fn encode(s: &Signal) -> Vec<u8> {
    let mut out = Vec::new();
    match s {
        Signal::Peer { from, to, data } => {
            varint::write(&mut out, T_PEER);
            wr_bytes(&mut out, from);
            wr_bytes(&mut out, to);
            wr_bytes(&mut out, data);
        }
        Signal::Config { data } => {
            varint::write(&mut out, T_CONFIG);
            wr_bytes(&mut out, data);
        }
    }
    out
}

/// Decode one signal (the whole buffer).
pub fn decode(mut b: &[u8]) -> Result<Signal, WireError> {
    let s = match rd_u64(&mut b)? {
        T_PEER => {
            let from = rd_bytes(&mut b, MAX_PEER)?;
            let to = rd_bytes(&mut b, MAX_PEER)?;
            let data = rd_bytes(&mut b, MAX_DATA)?;
            Signal::Peer { from, to, data }
        }
        T_CONFIG => Signal::Config { data: rd_bytes(&mut b, MAX_DATA)? },
        other => return Err(WireError::BadTag(other)),
    };
    if !b.is_empty() || !s.fits() {
        return Err(WireError::Eof);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_round_trip_and_are_told_from_sync_and_presence() {
        let peer = Signal::Peer { from: b"a".to_vec(), to: b"b".to_vec(), data: b"offer".to_vec() };
        let config = Signal::Config { data: b"{}".to_vec() };
        assert_eq!(decode(&encode(&peer)).unwrap(), peer);
        assert_eq!(decode(&encode(&config)).unwrap(), config);
        assert!(is_signal(&encode(&peer)) && is_signal(&encode(&config)));
        let here = crate::presence::Presence::Here { peer: b"a".to_vec(), state: vec![], direct: true };
        assert!(!is_signal(&crate::presence::encode(&here)), "presence is not a signal");
        let sync = crate::wire::encode(&crate::SyncMessage::Have { writer: [1; 32], length: 2 });
        assert!(!is_signal(&sync), "sync is not a signal");
        let to_itself = Signal::Peer { from: b"a".to_vec(), to: b"a".to_vec(), data: vec![] };
        assert!(decode(&encode(&to_itself)).is_err(), "a peer does not signal itself");
        assert!(decode(&encode(&peer)[..5]).is_err(), "a truncated signal is refused");
    }
}
