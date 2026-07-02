//! `roomnet` — a pluggable, sans-IO room-replication layer over the L1 substrate.
//!
//! # Two tiers
//!
//! - **[`Room`] (Tier 1, shared, sans-IO).** One room = one autobase-linearized
//!   multiwriter log (a [`hypercore`] per writer) + a rolling finalized/live
//!   [`Projection`] + the block-sync state machine. No async, no I/O, no transport
//!   → it compiles to `wasm32` and runs identically on the node and in the browser.
//! - **[`RoomServer`] (Tier 2, node).** Owns many rooms and replicates remote ones
//!   on demand (IPFS-like). A browser client skips this and holds one [`Room`] per
//!   session.
//!
//! # Three pluggable seams
//!
//! - **[`Transport`]** — how [`Outbound`]s move (native: Iroh; browser: WS/Trystero).
//! - **[`StoreFactory`]/[`storage::Store`]** — where each writer's blocks live.
//! - **[`ProjectionSink`]** — where finalized projection versions are persisted.
//!
//! # Three lanes per verified ingest
//!
//! 1. **immediate** — fan the block out to clients ([`Fanout::Clients`]).
//! 2. **immediate** — durably store the block (the sync [`storage::Store`]).
//! 3. **deferred** — on autobase finality, fold the [`Projection`] and surface each
//!    finalized mutation via [`Room::poll_finalized`] (Lane 3, throttled by the driver).

pub mod config;
pub mod entry;
pub mod projection;
pub mod room;
pub mod server;
pub mod store;
pub mod sync;
pub mod testkit;
pub mod transport;
pub mod wire;

/// The real Iroh transport + tokio driver (native). Behind `iroh` so the default
/// build stays wasm-clean.
#[cfg(feature = "iroh")]
pub mod native;

pub use autobase::{NodeId, WriterKey};
pub use config::{Origin, RoomConfig, ServerConfig};
pub use entry::{Entry, EntryCodec};
pub use projection::Projection;
pub use room::{Error, Fanout, Finalized, Outbound, PeerId, Room, StoreErr};
pub use server::{RoomId, RoomServer};
pub use store::{MemStoreFactory, StoreFactory};
#[cfg(unix)]
pub use store::DiskStoreFactory;
pub use sync::SyncMessage;
pub use transport::{ProjectionSink, Transport};

#[cfg(feature = "iroh")]
pub use native::{run_server, Command, DriverOpts, Inbound, IrohConfig, IrohTransport};

#[cfg(test)]
mod tests;
