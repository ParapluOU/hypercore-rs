//! Presence: who is in a room now and what they show (a cursor, a name).
//!
//! Presence is **ephemeral**. It never enters a room's log, is never persisted, and
//! is never confirmed: a [`Presence`] message only replaces what one peer showed
//! before. Roomnet carries it beside the sync protocol — over the WebSocket between a
//! client and its homeserver (see [`ws`](crate::ws)) — and treats its state as opaque
//! bytes; what a peer shows is the application's business.
//!
//! A peer is named by a short key of the client's own choosing (a random id per open
//! page, say), not by a writer key: one identity may be present several times.

use codec::varint;

use crate::wire::WireError;

/// The longest peer key a presence message carries.
pub const MAX_PEER: usize = 64;
/// The largest state a peer shows.
pub const MAX_STATE: usize = 64 * 1024;

/// Wire tags. Distinct from every [`SyncMessage`](crate::SyncMessage) tag, so a frame
/// body tells by its first varint whether it is presence or sync.
const T_HERE: u64 = 0x10;
const T_GONE: u64 = 0x11;

/// One peer's presence in a room, as it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// `peer` is in the room, showing `state` (replacing what it showed before).
    Here { peer: Vec<u8>, state: Vec<u8> },
    /// `peer` left the room.
    Gone { peer: Vec<u8> },
}

impl Presence {
    /// The peer the message is about.
    pub fn peer(&self) -> &[u8] {
        match self {
            Self::Here { peer, .. } | Self::Gone { peer } => peer,
        }
    }

    /// Whether the message is within the size limits ([`MAX_PEER`], [`MAX_STATE`]).
    pub fn fits(&self) -> bool {
        match self {
            Self::Here { peer, state } => !peer.is_empty() && peer.len() <= MAX_PEER && state.len() <= MAX_STATE,
            Self::Gone { peer } => !peer.is_empty() && peer.len() <= MAX_PEER,
        }
    }
}

/// Whether a frame body (after the room and sender) is presence rather than sync.
pub fn is_presence(body: &[u8]) -> bool {
    let mut b = body;
    matches!(varint::read(&mut b), Ok(T_HERE | T_GONE))
}

/// Encode one presence message.
pub fn encode(p: &Presence) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, p);
    out
}

/// Decode one presence message (the whole buffer).
pub fn decode(mut b: &[u8]) -> Result<Presence, WireError> {
    let p = read(&mut b)?;
    if !b.is_empty() || !p.fits() {
        return Err(WireError::Eof);
    }
    Ok(p)
}

/// Encode several presence messages (a node's list of who is in a room).
pub fn encode_all(ps: &[Presence]) -> Vec<u8> {
    let mut out = Vec::new();
    varint::write(&mut out, ps.len() as u64);
    for p in ps {
        write(&mut out, p);
    }
    out
}

/// Decode what [`encode_all`] wrote.
pub fn decode_all(mut b: &[u8]) -> Result<Vec<Presence>, WireError> {
    let n = rd_u64(&mut b)? as usize;
    let mut v = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let p = read(&mut b)?;
        if !p.fits() {
            return Err(WireError::Eof);
        }
        v.push(p);
    }
    if !b.is_empty() {
        return Err(WireError::Eof);
    }
    Ok(v)
}

fn write(out: &mut Vec<u8>, p: &Presence) {
    match p {
        Presence::Here { peer, state } => {
            varint::write(out, T_HERE);
            wr_bytes(out, peer);
            wr_bytes(out, state);
        }
        Presence::Gone { peer } => {
            varint::write(out, T_GONE);
            wr_bytes(out, peer);
        }
    }
}

fn read(b: &mut &[u8]) -> Result<Presence, WireError> {
    match rd_u64(b)? {
        T_HERE => {
            let peer = rd_bytes(b, MAX_PEER)?;
            let state = rd_bytes(b, MAX_STATE)?;
            Ok(Presence::Here { peer, state })
        }
        T_GONE => Ok(Presence::Gone { peer: rd_bytes(b, MAX_PEER)? }),
        other => Err(WireError::BadTag(other)),
    }
}

fn wr_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    varint::write(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn rd_bytes(b: &mut &[u8], max: usize) -> Result<Vec<u8>, WireError> {
    let n = rd_u64(b)? as usize;
    if n > max || b.len() < n {
        return Err(WireError::Eof);
    }
    let (head, tail) = b.split_at(n);
    *b = tail;
    Ok(head.to_vec())
}

fn rd_u64(b: &mut &[u8]) -> Result<u64, WireError> {
    varint::read(b).map_err(|_| WireError::Varint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_round_trips_and_is_told_from_sync() {
        let here = Presence::Here { peer: b"tab-1".to_vec(), state: br#"{"cursor":1}"#.to_vec() };
        let gone = Presence::Gone { peer: b"tab-2".to_vec() };
        assert_eq!(decode(&encode(&here)).unwrap(), here);
        assert_eq!(decode(&encode(&gone)).unwrap(), gone);
        assert_eq!(decode_all(&encode_all(&[here.clone(), gone.clone()])).unwrap(), vec![here.clone(), gone]);
        assert!(is_presence(&encode(&here)));
        let sync = crate::wire::encode(&crate::SyncMessage::Have { writer: [1; 32], length: 2 });
        assert!(!is_presence(&sync), "a sync message is not presence");
        assert!(decode(&encode(&here)[..4]).is_err(), "a truncated message is refused");
        let big = Presence::Here { peer: vec![1; MAX_PEER + 1], state: vec![] };
        assert!(decode(&encode(&big)).is_err(), "an oversized peer key is refused");
    }
}
