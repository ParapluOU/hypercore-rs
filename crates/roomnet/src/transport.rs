//! The async transport + projection-sink seams (driver-level, not the core).

use crate::room::{Outbound, PeerId};
use crate::sync::SyncMessage;

/// The transport seam a driver wires under a [`Room`](crate::Room).
///
/// The room core is **sans-IO**: it consumes inbound `(PeerId, SyncMessage)` and
/// emits [`Outbound`]s. A driver moves those over a real network — natively Iroh
/// (gossip + streams), in the browser WebSockets + WebRTC/Trystero. This trait is
/// the documented boundary; the concrete Iroh impl is a follow-on behind the
/// `iroh` feature. Tests drive rooms synchronously without it (see `tests/`).
pub trait Transport {
    type Error: core::fmt::Debug;

    /// Deliver an outbound message according to its [`Fanout`](crate::Fanout).
    fn send(&mut self, out: Outbound) -> Result<(), Self::Error>;

    /// Drain inbound messages received since the last poll.
    fn recv(&mut self) -> Vec<(PeerId, SyncMessage)>;
}

/// The Lane-3 projection sink a driver wires under a [`Room`](crate::Room).
///
/// The core emits a finalized mutation per [`poll_finalized`](crate::Room::poll_finalized);
/// a driver feeds each to the sink. On the node this is a **throttled** writer to
/// the schema'd TerminusDB (one write per finalized version — not coalesced —
/// paced by a token bucket); in the browser it is a render callback. Kept out of
/// the sans-IO core so the core has no clock/IO dependency.
pub trait ProjectionSink<State> {
    type Error: core::fmt::Debug;

    /// Persist finalized `state` at monotonically increasing `version`.
    fn write(&mut self, version: u64, state: &State) -> Result<(), Self::Error>;
}
