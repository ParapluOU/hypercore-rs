# Feature matrix — this port vs. upstream Holepunch JS

How `hypercore-rs` compares, capability by capability, to the upstream JS
(`reference/js/{hypercore,hyperbee,autobase}`). We are **clean-room and not
wire-/disk-compatible** (ADR-0001); this tracks *behavioural* capability, not API
shape.

**Legend:** ✅ at parity · ◑ partial / L1-only · ✗ in-scope gap (not yet ported) ·
⛔ deliberately out of scope (networking → Iroh, encryption, domain `apply` → the
consumer's L2, JS session/event machinery).

## hypercore

| Capability | upstream | port | |
|---|---|---|---|
| Create / open | `new`, `ready`, `key`, `id` | `new`, `with_prologue`, `key`, `public_key` | ✅ |
| Append / truncate / clear | `append`, `truncate`, `clear` | `append`, `truncate`, `clear` | ✅ |
| Atomic batch | `append([...])` | `batch`/`stage`/`commit`/`batch_get` | ✅ |
| Read | `get`, `has`, `length`, `byteLength`, `contiguousLength` | `get`, `block`, `has`, `len`, `byte_length`, `contiguous_length` | ✅ |
| Streams | `createReadStream`, `createByteStream` | `read_stream`, `byte_stream` | ✅ |
| `createWriteStream` | ✔ | — | ✗ |
| Seek | `seek` | exposed via `byte_stream`; `merkle::seek` | ◑ |
| Proofs / verify | `proof`, `applyProof`, `treeHash`, `signable`, `missingNodes` | `proof`, `upgrade_proof`, `verify_block`/`_head`/`_upgrade`/`_reorg`, `root_hash` | ✅ |
| Fork detection | fork counter + proofs | `conflicting_heads`, `ForkProof` | ✅ |
| Snapshots | `snapshot` | `snapshot`, `signed_length` | ✅ |
| Multisig / manifest | `manifest`, multisig | `ManifestCore`, `verify_manifest_block` | ✅ |
| Move-to / key rotation | `lib/move-to` | `prologue_at`/`with_prologue`/`copy_prologue`/`verify_prologue` | ✅ |
| Persistence | storage/disk | `persist`/`open` (over `Store`, incl. OPFS) | ✅ |
| `purge` / mark-&-sweep GC / `compact` | ✔ | sparse `clear` only | ✗ |
| `setUserData` / `getUserData` | ✔ | — | ✗ |
| Sessions | `session`, `transferSession`, `commit` | `snapshot` covers read-isolation | ◑ |
| Replication **transport** | `replicate`, `peers`, `download`, `update` | — | ⛔ |
| Replication **logic** | proof apply / upgrade / reorg | `Replica::add_block`/`verify_upgrade`/`reorg` | ✅ |
| Encryption | `setEncryptionKey`, … | — | ⛔ |

## hyperbee

| Capability | upstream | port | |
|---|---|---|---|
| put / get / del | `put`, `get`, `del` | `put`, `get`, `del` | ✅ |
| Range | `createRangeIterator` (lazy), `peek` | `range` (eager `Vec`; gt/gte/lt/lte/reverse/limit) | ◑ |
| Compare-and-swap | `put`/`del` `{cas}` | — | ✗ |
| `getBySeq` / `peek` | ✔ | — | ✗ |
| Batch | `batch` | — | ✗ |
| Sub-databases | `sub(prefix)` | — | ✗ |
| Checkout / snapshot / version | `checkout(v)`, `snapshot`, `version` | `version` | ✗ (COW substrate exists) |
| Diff / history | `createDiffStream`, `createHistoryStream` | — | ✗ |
| Watch | `watch`, `getAndWatch` | — | ⛔ (needs the live/networking layer) |
| Header / detection | `getHeader`, `isHyperbee` | — | ⛔ (no header — ADR-0030) |
| Replication | `replicate` | via hypercore | ⛔ |

## autobase

| Capability | upstream | port | |
|---|---|---|---|
| Causal linearization | `heads`, vector-clock order | `order`, `sees`, `clock`, `tails` | ✅ |
| Indexer quorum / consensus | `consensus`, `system.indexers`, `ack` | `quorum_degree`, the faithful `consensus.js` machine | ✅ |
| View confirmation / finality | `indexedLength` | `finalized`/`indexed_view`/`confirmed_view` | ✅ |
| Materialized view | `view` (via `apply`) | `view`/`view_get` (identity; the fold is L2) | ◑ |
| Indexer set | dynamic, in-log | `with_indexers` (passed in) | ◑ (external by design) |
| Writer / indexer reconfiguration | `addWriter`/`removeWriter` | — | ✗ (deferred) |
| Append / local writer | `append`, `setLocal` | DAG-level `add(node, heads)` | ◑ |
| `apply`/`open`/`update` handlers | ✔ | — | ⛔ (domain logic = L2) |
| Acks | explicit `ack()` nodes | emergent (recompute-from-scratch) | ◑ (same result) |
| Fast-forward / repair | `forceFastForward` | — | ✗ |
| Replication / wakeup | `replicate`, `hintWakeup` | — | ⛔ |
| Encryption / events / sessions | ✔ | — | ⛔ |

## Summary

- **At parity (the L1 substrate, the deliverable):** hypercore write/read/proofs/
  snapshots/multisig/move-to/persistence + replication *logic*; hyperbee ordered-KV;
  autobase ordering + consensus + finality.
- **In-scope gaps (the backlog):** hyperbee batch / sub-databases / checkout /
  diff-history; hypercore write-stream / purge-GC / setUserData; autobase dynamic
  reconfiguration; lazy streaming iterators (a couple of eager `Vec`s).
- **Deliberately out (not gaps):** networking/wire/replication-transport (→ Iroh),
  encryption, the domain `apply` fold (→ the consumer's L2), watch/live (needs the
  networking layer), JS session/event machinery, the hyperbee header block.
