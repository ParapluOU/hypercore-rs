//! The sans-IO orchestration core — the reusable driver shared by every runtime.
//!
//! A [`Driver`] owns a [`RoomServer`] and a [`Transport`] and turns the room
//! state machines' `on_inbound → outbound` cycle into a single synchronous
//! [`tick`](Driver::tick). It has **no async, no clock, no I/O**: it does not sleep,
//! and it hands finalized deltas back to the caller instead of writing them, so the
//! async sink lives in the runtime wrapper. The tokio/Iroh node
//! ([`run_server`](crate::run_server)) and a future wasm/WebRTC browser both build
//! on this one driver — the only difference is the [`Transport`] they pass and how
//! they schedule [`tick`](Driver::tick) + sink the returned deltas.

use autobase::WriterKey;

use crate::projection::Projection;
use crate::room::{Error, Finalized, Outbound, StoreErr};
use crate::server::{RoomId, RoomServer};
use crate::store::StoreFactory;
use crate::transport::{Inbound, Transport};

/// The reusable, runtime-agnostic room-driving core. See the module docs.
pub struct Driver<F: StoreFactory, P: Projection, T: Transport> {
    server: RoomServer<F, P>,
    transport: T,
    announce_every_ticks: u64,
    tick: u64,
}

impl<F, P, T> Driver<F, P, T>
where
    F: StoreFactory,
    // `Clone` is required by `RoomServer`'s own `host`/`join_remote` API (the
    // projection template is cloned into each room); it is not an extra demand.
    P: Projection + Clone,
    T: Transport,
{
    /// Wrap a [`RoomServer`] and a [`Transport`], re-advertising every room's head
    /// every `announce_every_ticks` ticks (0 disables periodic re-announce).
    pub fn new(server: RoomServer<F, P>, transport: T, announce_every_ticks: u64) -> Self {
        Self { server, transport, announce_every_ticks, tick: 0 }
    }

    /// The owned room server (read-only).
    pub fn server(&self) -> &RoomServer<F, P> {
        &self.server
    }

    /// The owned room server (mutable) — e.g. to read a snapshot or evict rooms.
    pub fn server_mut(&mut self) -> &mut RoomServer<F, P> {
        &mut self.server
    }

    /// The owned transport (mutable) — e.g. to record peers on the concrete impl.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Host a room locally and advertise it.
    pub fn host(&mut self, room: RoomId) -> Result<(), Error<StoreErr<F>>> {
        self.server.host(room)?;
        self.announce(room);
        Ok(())
    }

    /// Replicate a remote room on demand and advertise it.
    pub fn join_remote(&mut self, room: RoomId) -> Result<(), Error<StoreErr<F>>> {
        self.server.join_remote(room)?;
        self.announce(room);
        Ok(())
    }

    /// Replicate a remote room with an explicit indexer set (use the origin's
    /// indexers so a replica agrees with the host on finality) and advertise it.
    pub fn join_remote_with_indexers(
        &mut self,
        room: RoomId,
        indexers: Vec<WriterKey>,
    ) -> Result<(), Error<StoreErr<F>>> {
        self.server.join_remote_with_indexers(room, indexers)?;
        self.announce(room);
        Ok(())
    }

    /// Author a local op into `room`: [`local_append`](crate::Room::local_append)
    /// then dispatch the outbounds. A no-op if the room is not held (matching the
    /// existing `run_server` semantics — hosting is an explicit prior step).
    pub fn append(&mut self, room: RoomId, op: &[u8]) -> Result<(), Error<StoreErr<F>>> {
        let outs = match self.server.get_mut(room) {
            Some(r) => r.local_append(op)?,
            None => return Ok(()),
        };
        for o in outs {
            self.dispatch(room, o);
        }
        Ok(())
    }

    /// ONE sans-IO tick. In order:
    ///  (a) drain transport inbound → each `room.on_inbound` → dispatch outbounds,
    ///  (b) per [`Original`](crate::Origin::Original) room: `room.checkpoint()` → dispatch,
    ///  (c) every `announce_every_ticks`: announce every room → dispatch,
    ///  (d) per room: `room.poll_finalized()` → collect.
    ///
    /// Returns the newly-finalized deltas per room for the CALLER to sink (the driver
    /// is sink-agnostic + synchronous, so async sinks live in the runtime wrapper).
    pub fn tick(&mut self) -> Vec<(RoomId, Vec<Finalized>)> {
        // (a) Inbound pump. `drain_inbound` returns owned frames, so the transport
        //     borrow ends before we route them.
        for Inbound { room, from, msg } in self.transport.drain_inbound() {
            let outs = match self.server.get_mut(room) {
                Some(r) => match r.on_inbound(from, msg) {
                    Ok(o) => o,
                    // A room-level error must not abort the whole tick; drop it and
                    // carry on (no `log` dep in the sans-IO build; wasm panics abort
                    // the module, so we never unwrap here).
                    Err(_e) => continue,
                },
                None => continue,
            };
            for o in outs {
                self.dispatch(room, o);
            }
        }

        // (b) Indexer checkpoints on hosted rooms with a pending peer op.
        let host_ids: Vec<RoomId> = self
            .server
            .rooms()
            .filter(|(_, r)| r.needs_checkpoint())
            .map(|(id, _)| *id)
            .collect();
        for id in host_ids {
            let outs = match self.server.get_mut(id) {
                Some(r) => match r.checkpoint() {
                    Ok(o) => o,
                    Err(_e) => continue,
                },
                None => continue,
            };
            for o in outs {
                self.dispatch(id, o);
            }
        }

        // (c) Periodic head re-advertise so peers that connected after an edit (or
        //     whose sync stalled) converge.
        if self.announce_every_ticks != 0 && self.tick % self.announce_every_ticks == 0 {
            let ids: Vec<RoomId> = self.server.rooms().map(|(id, _)| *id).collect();
            for id in ids {
                self.announce(id);
            }
        }
        self.tick = self.tick.wrapping_add(1);

        // (d) Drain newly-finalized deltas per room for the caller to sink.
        let ids: Vec<RoomId> = self.server.rooms().map(|(id, _)| *id).collect();
        let mut finalized = Vec::new();
        for id in ids {
            if let Some(r) = self.server.get_mut(id) {
                let deltas = r.poll_finalized();
                if !deltas.is_empty() {
                    finalized.push((id, deltas));
                }
            }
        }
        finalized
    }

    /// Ship one outbound frame for `room`, dropping (never panicking on) a transport
    /// error — the room re-advertises on the next announce tick.
    fn dispatch(&mut self, room: RoomId, out: Outbound) {
        if let Err(_e) = self.transport.dispatch(room, out) {
            // Transport errors are non-fatal to the room state machine. No `log`
            // dependency in the default (wasm-clean) build, so we drop the error
            // here rather than surface or panic on it.
        }
    }

    /// Advertise a room's head to peers (gossip).
    fn announce(&mut self, room: RoomId) {
        let outs = match self.server.get(room) {
            Some(r) => r.announce(),
            None => return,
        };
        for o in outs {
            self.dispatch(room, o);
        }
    }
}
