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
use crate::room::Room;
use crate::store::StoreFactory;

/// A room's stable identifier (the node derives it from the room's topic).
pub type RoomId = [u8; 32];

/// A node's collection of rooms.
pub struct RoomServer<F: StoreFactory, P: Projection> {
    identity_seed: [u8; 32],
    indexers: Vec<WriterKey>,
    replica_stale_after: Option<core::time::Duration>,
    rooms: BTreeMap<RoomId, Room<F, P>>,
    projection: P,
}

impl<F, P> RoomServer<F, P>
where
    F: StoreFactory + Default,
    P: Projection + Clone,
{
    /// Create an empty server. `projection` is the template cloned into each room.
    pub fn open(cfg: ServerConfig, projection: P) -> Self {
        Self {
            identity_seed: cfg.identity_seed,
            indexers: cfg.indexers,
            replica_stale_after: cfg.replica_stale_after,
            rooms: BTreeMap::new(),
            projection,
        }
    }

    /// Host a room locally (creating it if absent). Never auto-deleted.
    pub fn host(&mut self, room: RoomId) -> &mut Room<F, P> {
        if !self.rooms.contains_key(&room) {
            let cfg = RoomConfig::original(self.mint_identity(), self.indexers.clone());
            let r = Room::open(cfg, F::default(), self.projection.clone());
            self.rooms.insert(room, r);
        }
        self.rooms.get_mut(&room).expect("just inserted")
    }

    /// Replicate a remote room on demand (creating a replica room if absent). The
    /// driver connects to the origin; the room pulls via adverts + `Want`s.
    /// Eligible for stale eviction after the configured idle window.
    pub fn join_remote(&mut self, room: RoomId) -> &mut Room<F, P> {
        if !self.rooms.contains_key(&room) {
            let cfg = RoomConfig::replicated(
                self.mint_identity(),
                self.indexers.clone(),
                self.replica_stale_after,
            );
            let r = Room::open(cfg, F::default(), self.projection.clone());
            self.rooms.insert(room, r);
        }
        self.rooms.get_mut(&room).expect("just inserted")
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
