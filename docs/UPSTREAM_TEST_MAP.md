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
| `core.js` | [~] | `hypercore` core append/get/verify + proof-based replication ported; **verified length-extension replication** (`Replica::verify_upgrade` — a longer signed head accepted only as an append-only extension of the replica's own roots before fetching, ADR-0021); **"append and truncate" ported** — `truncate` rewinds to a prefix, bumps a signed `fork` counter, shrinks `byte_length`, and records `lastTruncation {from,to}` cleared by the next append (ADR-0024); signed-length fast-forward / wire framing still networking (out of scope) |
| `batch.js` | [~] | `hypercore` batch append — stage-without-touching / single-head commit / stale-base reject ported (ADR-0018); multi-session interactions out of scope (sessions excluded) |
| `atomic.js` | [~] | `hypercore` atomic append — all-or-nothing commit (storage-fault rollback leaves tree+head untouched) ported (ADR-0018); `atom.flush()` storage-overlay + truncate/append events deferred |
| `merkle-tree.js` | [~] | `merkle` (roots/proof/verify/determinism/tamper, contiguous-range proofs (ADR-0017), length-extension `upgrade` proofs (data-free consistency/anti-fork-across-lengths, ADR-0020), **and byte-offset seek** — tree-accelerated `seek` (== linear scan, the "basic tree seeks" test) + standalone data-free `SeekProof` locating a byte→block against the signed root, ADR-0022 — ported; **local `truncate`** — rewind to a prefix, node-for-node identical to a fresh prefix so the root is the prefix root, + `byte_length` (ADR-0024); `upgrade.additionalNodes` + bundled-wire seek `padding` + reorg-by-proof still later) |
| `merkle-tree-recovery.js` | [~] | `merkle` node recovery — L1 behaviour ported (ADR-0023): repair mode (`is_intact`/`missing_nodes`/`try_root_hash`, `try_append` refuses while corrupt), a data-free `NodeProof` authenticating any tree node against the signed root (`node_proof` = `generateRemoteProofForTreeNode`), and atomic `recover_node` (= `recoverFromRemoteProof`, mangled proof leaves storage untouched); replication-driven repair (range-request auto-repair, peer requests, `repairing`/`repaired`/`repair-failed` events) is networking/sessions (out of scope); reorg later |
| `encodings.js` | [~] | `codec` (varint/framing/tagged/tolerance concepts ported; upstream-specific encodings are clean-room) |
| `conflicts.js` | [~] | `hypercore` fork detection — L1 behaviour ported (ADR-0019): `conflicting_heads` (same-fork, same-length, different-root signed heads) + per-index `ForkProof::verify`, **refined for the signed `fork` counter (ADR-0024): equivocation = a *same-fork* contradiction, while a fork-bumped truncate-and-rewrite is a legitimate reorg and is not flagged**; the replication-time `'conflict'` event + session teardown are networking/sessions (out of scope, return with Iroh) |
| `bitfield.js`, `bit-interlude.js`, `mark-bitfield.js`, `mark-n-sweep.js` | [ ] | `storage`/sparse bitfield (local) |
| `clear.js`, `purge.js` | [ ] | `hypercore` clear/truncate — the **truncate** primitive is ported (ADR-0024, tracked on `core.js`); `clear` (sparse range-clear) + `purge`/physical storage reclamation still todo |
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
| `linearizer.js` | [~] | causal order + tiebreak ported; **quorum degree (single/double) + double-quorum finalized prefix now ported** (DESIGN.md worked examples); `getIndexedViewLength`/`view.get` still need view materialization (apply layer); fork/merge confirm deferred (ADR-0015) ★★ |
| `dags.js` | [~] | ordering + causal-respect + determinism ported; **quorum-degree confirmation ported**; confirmed-view-*length* assertions still need view materialization ★★ |
| `basic.js` | [ ] | `autobase` basics ★ |
| `core.js` | [ ] | `autobase` core |
| `anchors.js` | [ ] | `autobase` anchoring ★ |
| `apply.js` | [ ] | `autobase` apply/view ★ |
| `fork.js` | [ ] | `autobase` fork handling ★ |
| `topolist.js` | [ ] | topological list ★ |
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
