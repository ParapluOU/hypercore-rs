# Upstream test map

Every **relevant** upstream test becomes a passing Rust test against our clean-room API. Porting a
test may mean adapting it (our API differs) — we keep the *behaviour under test*, not the JS.

Relevance filter (what we **exclude**, because it is out of scope):
- networking / replication / wire protocol
- disk-format & JS interop (we are not compatible)
- sessions / preload / mutex / timeouts (session & runtime plumbing)
- encryption (deferred)
- tracing / timers / benches

Status: `[ ]` todo · `[~]` partial · `[x]` ported & green · `—` out of scope (with reason).

## `reference/js/hypercore/test`

| File | Status | Maps to |
|------|--------|---------|
| `basic.js` | [~] | `hypercore` append/get + signed-head verify ported; sparse/session bits later |
| `core.js` | [~] | `hypercore` core append/get/verify + proof-based replication ported; **verified length-extension replication** (`Replica::verify_upgrade` — a longer signed head accepted only as an append-only extension of the replica's own roots before fetching, ADR-0021); **"append and truncate" ported** — `truncate` rewinds to a prefix, bumps a signed `fork` counter, shrinks `byte_length`, and records `lastTruncation {from,to}` cleared by the next append (ADR-0024); **secure replica-level reorg ported** — `Replica::verify_reorg`/`reorg` follow a *strictly higher*-fork truncate-and-rewrite, authenticating the claimed shared-prefix length by re-anchoring an `UpgradeProof` on the replica's own roots at that prefix (`MerkleTree::prefix_roots`), then drop the divergent suffix and refetch byte-identically — pure-truncation, from-scratch (ancestors 0), over-claimed-ancestor and forked-old-block cases (ADR-0026); the `want`/`update` proof-narrowing exchange that *discovers* the ancestor + signed-length fast-forward / wire framing still networking (out of scope) |
| `batch.js` | [~] | `hypercore` batch append — stage-without-touching / single-head commit / stale-base reject ported (ADR-0018); multi-session interactions out of scope (sessions excluded) |
| `atomic.js` | [~] | `hypercore` atomic append — all-or-nothing commit (storage-fault rollback leaves tree+head untouched) ported (ADR-0018); `atom.flush()` storage-overlay + truncate/append events deferred |
| `merkle-tree.js` | [~] | `merkle` (roots/proof/verify/determinism/tamper, contiguous-range proofs (ADR-0017), length-extension `upgrade` proofs (data-free consistency/anti-fork-across-lengths, ADR-0020), **and byte-offset seek** — tree-accelerated `seek` (== linear scan, the "basic tree seeks" test) + standalone data-free `SeekProof` locating a byte→block against the signed root, ADR-0022 — ported; **local `truncate`** — rewind to a prefix, node-for-node identical to a fresh prefix so the root is the prefix root, + `byte_length` (ADR-0024); **"lowest common ancestor" reorg ported** — `lowest_common_ancestor` (content-blind shared-prefix length via a monotone binary search over prefix root hashes) + `reorg` (keep the LCA prefix, adopt the other tree's divergent suffix, byte-identical), the small/bigger-gap, remote-shorter, simple-fork and long-fork cases (ADR-0025); the secure replica-level reorg gate (signed head + fork counter) is now ported at the hypercore layer (`Replica::reorg`, ADR-0026, tracked on `core.js`); the `want`/`update` proof-narrowing that *discovers* the ancestor is networking (deferred); `upgrade.additionalNodes` + bundled-wire seek `padding` still later) |
| `merkle-tree-recovery.js` | [~] | `merkle` node recovery — L1 behaviour ported (ADR-0023): repair mode (`is_intact`/`missing_nodes`/`try_root_hash`, `try_append` refuses while corrupt), a data-free `NodeProof` authenticating any tree node against the signed root (`node_proof` = `generateRemoteProofForTreeNode`), and atomic `recover_node` (= `recoverFromRemoteProof`, mangled proof leaves storage untouched); replication-driven repair (range-request auto-repair, peer requests, `repairing`/`repaired`/`repair-failed` events) is networking/sessions (out of scope); reorg later |
| `encodings.js` | [~] | `codec` (varint/framing/tagged/tolerance concepts ported; upstream-specific encodings are clean-room) |
| `conflicts.js` | [~] | `hypercore` fork detection — L1 behaviour ported (ADR-0019): `conflicting_heads` (same-fork, same-length, different-root signed heads) + per-index `ForkProof::verify`, **refined for the signed `fork` counter (ADR-0024): equivocation = a *same-fork* contradiction, while a fork-bumped truncate-and-rewrite is a legitimate reorg and is not flagged**; the replication-time `'conflict'` event + session teardown are networking/sessions (out of scope, return with Iroh) |
| `bitfield.js`, `bit-interlude.js`, `mark-bitfield.js`, `mark-n-sweep.js` | [~] | `storage::Bitfield` — the clean-room **sparse bitfield data structure** ported (`bitfield.js`: get/set/`set_range`/`count(start,length,val)`/`find_first`/`find_last` over an unbounded sparse paged presence map; semantics match upstream — `count`'s 2nd arg is a length, `find_first(false,..)` always finds, a missing page is all-`false` and never materialized on clear; ADR-0030). Persistence (`open`/`flush`/`BitInterlude`, `bit-interlude.js`) is storage-backend/disk-format (out of scope, returns with the IndexedDB/native `storage` work); `want` chunking is replication wire framing (networking, out of scope); GC marking (`mark-bitfield.js`/`mark-n-sweep.js`) not yet built. Now has its first L1 consumer: `Hypercore`'s presence map (`has`/`contiguous_length`/`clear`, ADR-0031, tracked on `clear.js`) |
| `clear.js`, `purge.js` | [~] | `hypercore` clear — **`clear` ported** (ADR-0031): a `storage::Bitfield` presence map in `Hypercore` (`append`/`commit` set bits, `truncate` clears the tail), `has(index)` / `contiguous_length()` ≡ upstream `has`/`contiguousLength`, `get`/`block` return `None` for an absent block (no-wait read), and `clear(start, end)` drops the present blocks in the range (best-effort byte delete + bit clear) returning the count cleared (`0` ≡ upstream `null`). Clear **leaves the Merkle tree / signed head untouched**, so a cleared block stays authenticated and re-verifiable from a holder (the `clear`, `incorrect clear`, `clear blocks with diff option`, `no side effect from clearing unknown nodes` cases, + an interior-range hole). The replication **re-download** that refills a cleared block (`clear + replication`/`gossip`) is networking (deferred); `purge` (whole-core deletion) is storage-backend + session lifecycle (out of scope); the `diff`/byte-API return shape is commented out upstream |
| `move-to.js`, `snapshots.js`, `streams.js` | [ ] | `hypercore` seek/snapshot/stream — `move-to.js`'s `truncate(1)` primitive is ported (ADR-0024); move-to/snapshot/stream proper still todo |
| `manifest.js` | [ ] | `identity`/signing config |
| `user-data.js`, `groups.js`, `push.js` | [ ] | low priority; classify when reached |
| `replicate.js`, `fully-remote-proof.js`, `wants.js`, `remote-length.js`, `remote-bitfield.js`, `extension.js`, `timeouts.js` | — | networking / replication |
| `encryption.js` | — | encryption deferred |
| `sessions.js`, `preload.js`, `mutex.js`, `compat.js` | — | session plumbing / upstream-version compat |
| `all.js`, `helpers/`, `fixtures/`, `bench/` | — | runner / support |

## `reference/js/autobase/test` (the heart)

| File | Status | Maps to |
|------|--------|---------|
| `linearizer.js` | [~] | causal order + tiebreak ported; **quorum degree (single/double) + double-quorum finalized prefix ported** (DESIGN.md worked examples), and the **quorum-degree *value* is now cross-checked over random DAGs** by an independent fixpoint oracle (`crates/autobase/tests/quorum.rs`, audit follow-up — not just the DESIGN.md examples and finalized-prefix convergence); **view materialization ported** (ADR-0028, `crates/autobase/tests/view.rs`): `view`/`view.length`/`view.get(i)`+`null`-past-end ≡ `view()`/`view_len()`/`view_get()`, `getIndexedViewLength` ≡ `indexed_view_len()` — the fork-free `linearizer - simple` chain asserts the exact upstream numbers (`view.length 6`, `getIndexedViewLength 4`, the per-index `view.get` sequence) since our conservative double-quorum confirmation equals upstream's confirmed length on a fork-free chain, plus the cross-replica `getIndexedViewLength(a)==(b)==(c)` convergence as a property over every DAG; the fork-case confirmed lengths (`compete`/`count ordering`) need the deferred fork/merge consensus (ADR-0015) and the per-replica *partial* view needs the apply/view layer ★★ |
| `dags.js` | [~] | ordering + causal-respect + determinism ported; **quorum-degree confirmation ported**; **view materialization ported** (ADR-0028) — `dags - simple 3` is the same fork-free chain as `linearizer - simple` (view + `getIndexedViewLength 4`); `dags - simple 2` (`n==2`, a unanimous single quorum upstream confirms but our conservative double-quorum form does not) + the fork cases' confirmed-view-*length* still need the deferred fork/merge consensus (ADR-0015) ★★ |
| `basic.js` | [ ] | `autobase` basics ★ |
| `core.js` | [ ] | `autobase` core |
| `anchors.js` | [ ] | `autobase` anchoring ★ |
| `apply.js` | [ ] | `autobase` apply/view ★ |
| `fork.js` | [ ] | `autobase` fork handling ★ |
| `topolist.js` | [~] | **stable-ordering behaviour ported** (`crates/autobase/tests/topolist.rs`, ADR-0027): a host-safe in-Rust re-statement of upstream's *non-optimistic* insertion sort (`moveDown`/`moveNonOptimisticUp`/`cmp`/`links`) cross-checks that our priority-Kahn `order()` (ADR-0014) agrees node-for-node with it on the canonical `DESIGN.md` DAGs, the explicit `stable ordering` example, and 200 seeded random fork/merge DAGs × several delivery orders — both compute the lex-minimal linear extension under (key, seq). Streaming-view bookkeeping (`undo`/`shared`/`mark`/`flush`/`indexed`) is a live-view patch optimization, not the ordering definition (we recompute each call) — deferred; **optimistic** nodes deferred (`optimistic.js`). Complements the env-blocked JS oracle (gate #4) ★ |
| `updates.js` | [ ] | update propagation |
| `autoack.js` | [ ] | quorum acknowledgement |
| `repair.js`, `snapshots.js` | [ ] | reorder repair / snapshots |
| `fast-forward.js`, `optimistic.js` | [ ] | signed-length fast-forward / optimistic blocks (later) |
| `fuzz/` | [x] | **convergence sim (gate #3)** ported clean-room as `crates/autobase/tests/convergence.rs`: seeded random DAG generator (`createDag` model) + delivery-order convergence + cooperative-growth finality-stability (the `rollBack` confirmed-prefix-stability idea). Deadlock/JS-formatting harness out of scope (ADR-0016) |
| `messages.js`, `node-buffer.js`, `encoding/` | — | wire encoding (→ our `codec`, not ported as-is) |
| `encryption.js` | — | encryption deferred |
| `suspend.js`, `timer.js`, `trace.js` | — | session/runtime/tracing |
| `all.js`, `helpers/`, `fixtures/`, `reference/`, `replay/` | — | runner / support |

## `reference/js/hyperbee/test` (only if we build `hyperbee`)

`basic.js`, `batches.js`, `ranges.js`, `sub.js`, `cas.js`, `checkout.js`, `diff.js`, `history.js`,
`watch.js`, `cache.js` → `[ ]` deferred until the `hyperbee` decision. `extension.js` → networking.

## `reference/rust/datrs-hypercore` (already Rust — direct reference)

| File | Status | Note |
|------|--------|------|
| `tests/core.rs` | [ ] | core behaviour, reusable patterns |
| `tests/model.rs` | [ ] | model-based testing — good template |
| `src/**` unit tests (`core`, `oplog/header`, `bitfield/*`) | [ ] | component-level reference |
| `tests/js_interop.rs`, `tests/js/`, `tests/common/` | — | JS disk-format interop (out of scope) |
