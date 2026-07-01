//! Per-room and per-server configuration.

use core::time::Duration;

use autobase::WriterKey;
use identity::SecretKey;

/// A room's provenance: is it hosted here, or a replica pulled from a peer?
///
/// The node's [`RoomServer`](crate::RoomServer) treats the two differently: an
/// [`Original`](Self::Original) room is never auto-deleted, while a
/// [`Replicated`](Self::Replicated) room is an IPFS-like cache entry that may be
/// evicted after an inactivity window. The sans-IO core only *stores* the policy
/// (and bumps a logical [`last_activity`](crate::Room::last_activity) marker on
/// each ingest); the driver enforces the actual wall-clock timer — which is why
/// `Duration` here is pure data and the core stays wasm-clean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Hosted locally (the homeserver of record for this room). Never auto-deleted.
    Original,
    /// Replicated from a peer on demand. Evicted after `delete_on_stale_after` of
    /// inactivity when set; `None` means "keep until explicitly dropped".
    Replicated { delete_on_stale_after: Option<Duration> },
}

/// Configuration for a single [`Room`](crate::Room).
pub struct RoomConfig {
    /// This node's writer identity — its single local writer core in this room.
    pub identity: SecretKey,
    /// The indexer set (quorum voters). **Passed in**; dynamic reconfiguration is
    /// an external concern (out of scope here). Empty ⇒ pure ordering, no quorum.
    pub indexers: Vec<WriterKey>,
    /// Provenance + stale-GC policy.
    pub origin: Origin,
}

impl RoomConfig {
    /// An original (locally-hosted) room with the given identity and indexer set.
    pub fn original(identity: SecretKey, indexers: Vec<WriterKey>) -> Self {
        Self { identity, indexers, origin: Origin::Original }
    }

    /// A replicated (pulled-from-peer) room, optionally GC'd after inactivity.
    pub fn replicated(
        identity: SecretKey,
        indexers: Vec<WriterKey>,
        delete_on_stale_after: Option<Duration>,
    ) -> Self {
        Self { identity, indexers, origin: Origin::Replicated { delete_on_stale_after } }
    }
}

/// Configuration for a [`RoomServer`](crate::RoomServer) — the node's world-manager.
pub struct ServerConfig {
    /// The node's identity **seed**, reused as its writer in every room it joins.
    /// The server mints a deterministic [`SecretKey`] per room via
    /// [`SecretKey::from_seed`] (identity keys are not `Clone`).
    pub identity_seed: [u8; 32],
    /// The default indexer set applied to rooms this server hosts/joins.
    pub indexers: Vec<WriterKey>,
    /// Idle window after which *replicated* rooms are eligible for eviction.
    pub replica_stale_after: Option<Duration>,
}
