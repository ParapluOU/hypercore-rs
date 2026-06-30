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
| Seek | `seek` | `seek`, `byte_stream` | ✅ |
| Proofs / verify | `proof`, `applyProof`, `treeHash`, `signable`, `missingNodes` | `proof`, `upgrade_proof`, `verify_block`/`_head`/`_upgrade`/`_reorg`, `root_hash` | ✅ |
| Fork detection | fork counter + proofs | `conflicting_heads`, `ForkProof` | ✅ |
| Snapshots | `snapshot` | `snapshot`, `signed_length` | ✅ |
| Multisig / manifest | `manifest`, multisig | `ManifestCore`, `verify_manifest_block` | ✅ |
| Move-to / key rotation | `lib/move-to` | `prologue_at`/`with_prologue`/`copy_prologue`/`verify_prologue` | ✅ |
| Persistence | storage/disk | `persist`/`open` (over `Store`, incl. OPFS) | ✅ |
| `purge` | ✔ | `purge` | ✅ |
| mark-&-sweep GC / `compact` | ✔ | sparse `clear` only | ✗ |
| `setUserData` / `getUserData` | ✔ | `set_user_data` / `get_user_data` | ✅ |
| Sessions | `session`, `transferSession`, `commit` | `snapshot` covers read-isolation | ◑ |
| Replication **transport** | `replicate`, `peers`, `download`, `update` | — | ⛔ |
| Replication **logic** | proof apply / upgrade / reorg | `Replica::add_block`/`verify_upgrade`/`reorg` | ✅ |
| Encryption | `setEncryptionKey`, … | — | ⛔ |

## hyperbee

| Capability | upstream | port | |
|---|---|---|---|
| put / get / del | `put`, `get`, `del` | `put`, `get`, `del` | ✅ |
| Range | `createRangeIterator` (lazy) | `range` (eager) + **`iter` → `RangeIter`** (lazy, on-demand, early-stop); gt/gte/lt/lte/reverse/limit | ✅ |
| Compare-and-swap | `put`/`del` `{cas}` | `put_cas` / `del_cas` | ✅ |
| Peek | `peek` | `peek` | ✅ |
| Batch | `batch` | `batch` → `BeeBatch` (stage / commit / drop-rollback) | ✅ |
| Sub-databases | `sub(prefix)` | `sub` → `Sub` | ✅ |
| Checkout / version | `checkout(v)`, `version` | `checkout` → `Checkout`, `get_at`/`range_at`, `version` | ✅ |
| Diff | `createDiffStream` | `diff(old, new)` | ✅ |
| History | `createHistoryStream` | `history()` (op roots = unreferenced blocks, then diff consecutive) | ✅ |
| `getBySeq` | ✔ | — | ⛔ (entry-block addressing — N/A to our format) |
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

- **At parity (the L1 substrate, the deliverable):** hypercore write/read/seek/proofs/
  snapshots/multisig/move-to/persistence/user-data/purge + replication *logic*; hyperbee
  ordered-KV with checkout, sub-databases, atomic batch, diff, peek and CAS; autobase
  ordering + consensus + finality.
- **Remaining in-scope gaps:** hypercore write-stream object and mark-&-sweep GC /
  `compact`; autobase dynamic indexer reconfiguration. (`diff`/`history` stay eager —
  they are inherently whole-set operations, not streaming.)
- **Deliberately out (not gaps):** networking/wire/replication-transport (→ Iroh),
  encryption, the domain `apply` fold (→ the consumer's L2), watch/live (needs the
  networking layer), JS session/event machinery, the hyperbee header block, and
  `getBySeq` (entry-block addressing our format does not use).
