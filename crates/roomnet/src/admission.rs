//! The admission seam: who may connect to a server, and what they may do in a room.
//!
//! A room's log needs no admission to stay sound — every block is signed and
//! verified, and an [`Authority`](crate::Authority) can gate which entries take
//! effect. Admission answers the other question, which signatures cannot: who may
//! *reach* a server at all, who may read a room through it, and whose blocks it will
//! take in.
//!
//! roomnet does not say how a client proves who it is. The application reads what
//! the client presented when it connected — a path, a query, headers such as a
//! cookie or a bearer token — and returns a [`Grant`]: opaque bytes it will be handed
//! back when the connection frames to a room. A server bound without an
//! [`Admission`] admits everyone, as before.

use crate::server::RoomId;

/// What a client presented when it opened its connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectRequest {
    /// The request target: path and query, as sent (e.g. `/rooms?token=…`).
    pub target: String,
    /// The request headers, in order; names lower-cased. A header whose value is not
    /// valid UTF-8 is left out.
    pub headers: Vec<(String, String)>,
}

impl ConnectRequest {
    /// The first value of header `name` (lower-case), if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// What the application remembers about an admitted connection — opaque to roomnet.
pub type Grant = Vec<u8>;

/// What a connection may do in one room.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Nothing: its frames for the room are dropped and it hears nothing of the room.
    None,
    /// Follow the room: fetch its log, hear its gossip, take part in presence and
    /// signals. Blocks and head adverts it sends are dropped.
    Read,
    /// Everything `Read` allows, and its blocks and head adverts are taken in.
    Write,
}

/// Decides who a server admits. Asked off the driver's thread, hence `Send + Sync`.
pub trait Admission: Send + Sync + 'static {
    /// A client is opening a connection. `None` refuses it (the handshake is answered
    /// `403 Forbidden`); `Some` admits it and is remembered for the connection's life.
    fn connect(&self, request: &ConnectRequest) -> Option<Grant>;

    /// What the connection holding `grant` may do in `room`. Asked once per
    /// connection and room, the first time the connection frames to it; a change
    /// takes effect when the client reconnects.
    fn access(&self, grant: &Grant, room: &RoomId) -> Access;
}
