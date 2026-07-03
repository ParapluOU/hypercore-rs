//! The `RoomServer` — the node's world-manager (Tier 2).
//!
//! Owns *many* [`Room`]s keyed by [`RoomId`], hosts local rooms, and replicates
//! remote rooms **on demand** (IPFS-like): a client reaching for a room the node
//! does not hold makes it [`join_remote`](RoomServer::join_remote) — open a
//! replica room and pull it from the origin. A browser client never uses this; it
//! holds a single [`Room`] per session directly.
//!
//! This type is deliberately runtime-agnostic (no tokio): it owns and routes
//! rooms. The real Iroh event loop that pumps [`Outbound`]s over the network is
//! the driver (feature `iroh`, a follow-on); tests drive it synchronously.

use std::collections::BTreeMap;

use autobase::WriterKey;
use identity::SecretKey;

use crate::config::{Origin, RoomConfig, ServerConfig};
use crate::projection::Projection;
use crate::room::{Error, Room, StoreErr};
use crate::store::StoreFactory;

/// A room's stable identifier (the node derives it from the room's topic).
pub type RoomId = [u8; 32];

/// Builds the byte-store [`StoreFactory`] for one room. A **durable** factory
/// MUST give each room its own location: every room a node hosts is signed by the
/// same per-node writer identity (so peers can dial it — see [`mint_identity`]),
/// so a flat store keyed only by writer would make all rooms collide on one
/// `hex(writer)` file. Return e.g. a `DiskStoreFactory` rooted at a per-`RoomId`
/// directory; an ephemeral factory (`MemStoreFactory`) can ignore the id.
///
/// [`mint_identity`]: RoomServer::mint_identity
pub type StoreProvider<F> = Box<dyn Fn(RoomId) -> F + Send>;

/// A node's collection of rooms.
pub struct RoomServer<F: StoreFactory, P: Projection> {
    identity_seed: [u8; 32],
    indexers: Vec<WriterKey>,
    replica_stale_after: Option<core::time::Duration>,
    rooms: BTreeMap<RoomId, Room<F, P>>,
    projection: P,
    new_store: StoreProvider<F>,
}

impl<F, P> RoomServer<F, P>
where
    F: StoreFactory,
    P: Projection + Clone,
{
    /// Create an empty server. `projection` is the template cloned into each room;
    /// `new_store` builds each room's [`StoreFactory`] — for durable storage it
    /// must isolate rooms (see [`StoreProvider`]).
    pub fn open(
        cfg: ServerConfig,
        projection: P,
        new_store: impl Fn(RoomId) -> F + Send + 'static,
    ) -> Self {
        Self {
            identity_seed: cfg.identity_seed,
            indexers: cfg.indexers,
            replica_stale_after: cfg.replica_stale_after,
            rooms: BTreeMap::new(),
            projection,
            new_store: Box::new(new_store),
        }
    }

    /// Host a room locally (creating it if absent). Never auto-deleted. Fallible
    /// because opening the room may resume it from a durable store.
    pub fn host(&mut self, room: RoomId) -> Result<&mut Room<F, P>, Error<StoreErr<F>>> {
        if !self.rooms.contains_key(&room) {
            let cfg = RoomConfig::original(self.mint_identity(), self.indexers.clone());
            let r = Room::open(cfg, (self.new_store)(room), self.projection.clone())?;
            self.rooms.insert(room, r);
        }
        Ok(self.rooms.get_mut(&room).expect("just inserted"))
    }

    /// Replicate a remote room on demand (creating a replica room if absent). The
    /// driver connects to the origin; the room pulls via adverts + `Want`s.
    /// Eligible for stale eviction after the configured idle window.
    pub fn join_remote(&mut self, room: RoomId) -> Result<&mut Room<F, P>, Error<StoreErr<F>>> {
        let indexers = self.indexers.clone();
        self.join_remote_with_indexers(room, indexers)
    }

    /// Like [`join_remote`](Self::join_remote) but with an explicit indexer set —
    /// use the **origin's** indexers, not this node's own, so a replica agrees with
    /// the host on who finalises (otherwise each node self-finalises a divergent
    /// order). The origin's writer key is a natural indexer.
    pub fn join_remote_with_indexers(
        &mut self,
        room: RoomId,
        indexers: Vec<WriterKey>,
    ) -> Result<&mut Room<F, P>, Error<StoreErr<F>>> {
        if !self.rooms.contains_key(&room) {
            let cfg =
                RoomConfig::replicated(self.mint_identity(), indexers, self.replica_stale_after);
            let r = Room::open(cfg, (self.new_store)(room), self.projection.clone())?;
            self.rooms.insert(room, r);
        }
        Ok(self.rooms.get_mut(&room).expect("just inserted"))
    }

    pub fn get(&self, room: RoomId) -> Option<&Room<F, P>> {
        self.rooms.get(&room)
    }

    pub fn get_mut(&mut self, room: RoomId) -> Option<&mut Room<F, P>> {
        self.rooms.get_mut(&room)
    }

    pub fn rooms(&self) -> impl Iterator<Item = (&RoomId, &Room<F, P>)> {
        self.rooms.iter()
    }

    /// Evict replicated rooms judged stale by `is_stale` (the driver supplies the
    /// wall-clock policy). Original rooms are never evicted. Returns the dropped ids.
    pub fn evict_stale(&mut self, is_stale: impl Fn(&Room<F, P>) -> bool) -> Vec<RoomId> {
        let stale: Vec<RoomId> = self
            .rooms
            .iter()
            .filter(|(_, r)| matches!(r.origin(), Origin::Replicated { .. }) && is_stale(r))
            .map(|(id, _)| *id)
            .collect();
        for id in &stale {
            self.rooms.remove(id);
        }
        stale
    }

    fn mint_identity(&self) -> SecretKey {
        SecretKey::from_seed(&self.identity_seed)
    }
}
