# roomnet — pluggable room-replication layer

`roomnet` (crate `crates/roomnet`) turns the L1 substrate (hypercore + autobase +
storage) into a **configurable room-replication service**: a *room* is one
autobase-linearized multiwriter log with a rolling materialized projection, and a
*server* owns many rooms and replicates remote ones on demand. It is designed so a
consumer (the Parture node) collapses its bespoke networking to thin wiring, and so
the **same room code runs in the browser**.

The spec is derived from the Parture node's actual needs (its hand-rolled
`services/node/src/services/network/` layer): head advertisement, block transfer,
whole-room fetch on join, per-room gossip topics, an in-memory room manager, and a
WebSocket edit fanout.

## Two tiers

- **`Room` (Tier 1, shared, sans-IO).** A pure state machine — no async, no I/O, no
  transport — so it compiles to `wasm32` and runs identically on node and browser.
  It owns one local writer [`Hypercore`] + a verify-only [`Replica`] per remote
  writer, drives the autobase `Linearizer`, and maintains two projections:
  - `snapshot_finalized()` — folds the autobase **finalized** order (authoritative;
    the node API serves it and Lane 3 persists it).
  - `snapshot_live()` — folds finalized-state + the unconfirmed tail (optimistic;
    what a client renders immediately).
- **`RoomServer` (Tier 2, node).** Owns many rooms keyed by `RoomId`; `host` opens a
  local (original) room, `join_remote` opens a replica room and pulls it from a peer
  (IPFS-like on-demand replication), `evict_stale` GCs cold replicas.

## Three pluggable seams

| Seam | Trait | node | browser |
|---|---|---|---|
| Transport | `Transport` / `IrohTransport` | Iroh QUIC | WS + WebRTC/Trystero |
| Storage (per writer) | `StoreFactory` → `storage::Store` | file-WAL | in-mem |
| Projection sink | `ProjectionSink` | throttled → TerminusDB | render callback |

The domain fold itself is the `Projection` trait (`apply` / `snapshot` /
`reset_to`) — deterministic, portable, the one place domain logic lives.

## Three lanes per verified ingest

1. **immediate** — fan the verified block out to clients (`Fanout::Clients`, Lane 1).
2. **immediate** — persist the block to the sync `Store` (Lane 2 durability; the node
   mirrors it eagerly to a schemaless `sys:JSON` TerminusDB db).
3. **deferred** — on autobase finality, `poll_finalized()` yields **one `Finalized`
   per finalized mutation** (never coalesced); a throttled driver writes each version
   to the schema'd TerminusDB db (Lane 3).

## Log content vs. wire

- **`Entry { heads, payload }`** — the *content* of one log entry (a hypercore block):
  `heads` are the autobase causal deps, `payload` the opaque domain op. L1 never
  inspects it.
- **`SyncMessage`** — the *wire* protocol: `Head` (advert), `Have`/`Want`
  (anti-entropy), `Block` (a self-verifying block carrying its signed head + Merkle
  proof; `bytes` is an `Entry`). Encoded by `wire::{encode,decode}` (no serde).

## Iroh transport (feature `iroh`)

`IrohTransport::bind(&IrohConfig)` binds a QUIC endpoint and ships `[RoomId | wire]`
frames; `run_server` is the tokio driver tying a `RoomServer` to it (pump inbound →
rooms → dial outbound; drain finalized → sink; service `Command`s). A node's identity
seed yields both its autobase `WriterKey` and its iroh `EndpointId` (both raw
ed25519), so peers are dialed directly by writer key.

`IrohConfig` is fully configurable with defaults: `alpn` (default
`b"roomnet/sync/0"`), `seed`, `bind_port`, `bootstrap`, `max_message_bytes`.

## Mapping to `services/node`

| node today | roomnet |
|---|---|
| `RoomManager` DashMaps | `RoomServer` room map |
| `clone_remote()` | `RoomServer::join_remote` |
| ALPN `parture/room/state/1` | `Want`/`Have` + `snapshot_finalized()`/`logs()` |
| gossip edit JSON | `SyncMessage::{Head,Block}` (Merkle-verified) |
| `broadcast::Sender<RoomMessage>` | Lane 1 (`Fanout::Clients`) |
| `save_edits` / `get_room` | `Room::local_append` / snapshot+logs queries |

## Honest scope / follow-ons

- **Cross-writer resume-from-checkpoint** is not yet implemented: a `Room` derives its
  DAG/projection in memory; on restart it re-syncs from peers. Persisting the finalized
  checkpoint `(state, finalized_len)` to skip re-fold needs a system/view-core
  addition (upstream has one; we do not yet).
- **Relay of a replicated writer's blocks** is deferred: a room serves `Want` only for
  its *local* writer (it holds proofs for it). Relaying a replica's blocks needs
  `Replica::{block,proof}` accessors in L1 — a small, planned addition.
- **Multi-room over one endpoint** works (frames are `RoomId`-tagged); peer discovery
  beyond `bootstrap` (a DHT/gossip discovery layer) is future.
- The **TerminusDB adapters** (WAL `SyncFile`, `sys:JSON` mirror, throttled schema
  sink) and the `lib/oplog` → `SongProjection` migration live in the monorepo, not
  here (the public submodule stays dependency-free of Parture internals).
