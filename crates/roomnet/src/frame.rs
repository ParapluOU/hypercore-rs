//! The frame a client and its server (or two direct peers) exchange over one
//! message-oriented connection — a WebSocket ([`ws`](crate::ws)) or a data channel:
//! `[room: 32][from: 32][body]`, where the body is a sync message
//! ([`wire`](crate::wire)), [presence](crate::presence) or a [signal](crate::signal),
//! told apart by its first tag.
//!
//! `from` is the sender's declared id: it only routes `Want`/`Have` replies; trust
//! comes from the signed blocks inside, not from this hint.

use crate::presence::{self, Presence};
use crate::room::PeerId;
use crate::server::RoomId;
use crate::signal::{self, Signal};
use crate::sync::SyncMessage;
use crate::transport::Inbound;
use crate::wire;

/// A parsed frame.
#[derive(Clone, Debug)]
pub enum Frame {
    /// Sync for the room's log.
    Sync(Inbound),
    /// Presence beside it.
    Presence { room: RoomId, from: PeerId, presence: Presence },
    /// A signal beside it.
    Signal { room: RoomId, from: PeerId, signal: Signal },
}

fn frame(room: &RoomId, from: &PeerId, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(64 + body.len());
    frame.extend_from_slice(room);
    frame.extend_from_slice(from);
    frame.extend_from_slice(body);
    frame
}

/// A sync frame.
pub fn sync_frame(room: &RoomId, from: &PeerId, msg: &SyncMessage) -> Vec<u8> {
    frame(room, from, &wire::encode(msg))
}

/// A presence frame.
pub fn presence_frame(room: &RoomId, from: &PeerId, p: &Presence) -> Vec<u8> {
    frame(room, from, &presence::encode(p))
}

/// A signal frame.
pub fn signal_frame(room: &RoomId, from: &PeerId, s: &Signal) -> Vec<u8> {
    frame(room, from, &signal::encode(s))
}

/// Parse a frame. `None` if it is too short or its body does not decode (a malformed
/// frame is dropped, never panics).
pub fn parse(buf: &[u8]) -> Option<Frame> {
    if buf.len() < 64 {
        return None;
    }
    let mut room: RoomId = [0u8; 32];
    room.copy_from_slice(&buf[0..32]);
    let mut from: PeerId = [0u8; 32];
    from.copy_from_slice(&buf[32..64]);
    let body = &buf[64..];
    if presence::is_presence(body) {
        return Some(Frame::Presence { room, from, presence: presence::decode(body).ok()? });
    }
    if signal::is_signal(body) {
        return Some(Frame::Signal { room, from, signal: signal::decode(body).ok()? });
    }
    let msg = wire::decode(body).ok()?;
    Some(Frame::Sync(Inbound { room, from, msg }))
}
