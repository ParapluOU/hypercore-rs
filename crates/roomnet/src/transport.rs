//! The transport + projection-sink seams (driver-level, not the core).

use crate::room::{Outbound, PeerId};
use crate::server::RoomId;
use crate::sync::SyncMessage;

/// One decoded inbound frame: which room, which peer, which message.
///
/// Lives here (not in the `iroh`-gated `native` module) so the sans-IO
/// [`Driver`](crate::Driver) and any [`Transport`] can name it in every build.
#[derive(Clone, Debug)]
pub struct Inbound {
    pub room: RoomId,
    pub from: PeerId,
    pub msg: SyncMessage,
}

/// The transport seam a driver wires under a [`Room`](crate::Room).
///
/// The room core is **sans-IO**: it consumes inbound `(PeerId, SyncMessage)` and
/// emits [`Outbound`]s. A driver moves those over a real network — natively Iroh
/// (gossip + streams), in the browser WebSockets + WebRTC/Trystero. This trait is
/// the per-room dispatch boundary: [`dispatch`](Self::dispatch) ships one outbound
/// frame for a room, [`drain_inbound`](Self::drain_inbound) collects the frames
/// received since the last call. It mirrors exactly how the native
/// [`IrohTransport`](crate::IrohTransport) already works, so the sans-IO
/// [`Driver`](crate::Driver) drives both native and browser transports unchanged.
pub trait Transport {
    type Error: core::fmt::Debug;

    /// Send one outbound frame for `room`.
    fn dispatch(&mut self, room: RoomId, out: Outbound) -> Result<(), Self::Error>;

    /// Drain all inbound frames received since the last call.
    fn drain_inbound(&mut self) -> Vec<Inbound>;
}

/// Fans one room's traffic across MULTIPLE underlying transports — e.g. a trustless
/// client that aggregates the same room's hypercores from several homeservers into
/// one autobase. [`dispatch`](Transport::dispatch) broadcasts to every inner
/// transport; [`drain_inbound`](Transport::drain_inbound) merges the frames drained
/// from all of them.
///
/// This is the "one room, many connections" building block: wrap N transports and
/// hand the composite to [`Driver::new`](crate::Driver::new), and the driver drives
/// exactly one [`Room`](crate::Room) fed by every connection at once.
///
/// ```text
/// // A browser client replicating one room from several homeservers, folded into
/// // one local autobase. (Native `IrohTransport`s shown; any `Transport` works —
/// // in the browser these would be WebSocket/WebRTC transports.)
/// let mut fanout = CompositeTransport::new(vec![]);
/// for server in homeservers {
///     let t = IrohTransport::bind(&config_for(server)).await?;
///     fanout.push(t);
/// }
/// let mut driver = Driver::new(room_server, fanout, /* announce_every_ticks */ 40);
/// driver.join_remote(room_id)?;
/// loop {
///     for (room, deltas) in driver.tick() {
///         // sink `deltas` (render, persist, …)
///     }
/// }
/// ```
pub struct CompositeTransport<T: Transport> {
    inner: Vec<T>,
}

impl<T: Transport> CompositeTransport<T> {
    /// Wrap a set of transports as one room-fanout transport.
    pub fn new(inner: Vec<T>) -> Self {
        Self { inner }
    }

    /// Add another underlying transport to the fanout.
    pub fn push(&mut self, t: T) {
        self.inner.push(t);
    }

    /// Access the underlying transports (e.g. to record peers on a specific one).
    pub fn transports_mut(&mut self) -> &mut [T] {
        &mut self.inner
    }
}

impl<T: Transport> Transport for CompositeTransport<T> {
    type Error = T::Error;

    /// Broadcast the outbound frame to every inner transport. Attempts all of them
    /// (so a single failing connection does not stop the others) and returns the
    /// first error encountered, if any.
    fn dispatch(&mut self, room: RoomId, out: Outbound) -> Result<(), Self::Error> {
        let mut first_err = None;
        for t in &mut self.inner {
            if let Err(e) = t.dispatch(room, out.clone()) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Merge the inbound frames drained from every inner transport.
    fn drain_inbound(&mut self) -> Vec<Inbound> {
        let mut all = Vec::new();
        for t in &mut self.inner {
            all.extend(t.drain_inbound());
        }
        all
    }
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
