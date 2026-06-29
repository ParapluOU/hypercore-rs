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
| `basic.js` | [ ] | `hypercore` append/get |
| `core.js` | [ ] | `hypercore` core behaviour |
| `batch.js` | [ ] | `hypercore` batch append |
| `atomic.js` | [ ] | `hypercore` atomic append |
| `merkle-tree.js` | [ ] | `merkle` |
| `merkle-tree-recovery.js` | [ ] | `merkle` recovery |
| `encodings.js` | [ ] | `codec` |
| `conflicts.js` | [ ] | `hypercore`/`merkle` fork detection |
| `bitfield.js`, `bit-interlude.js`, `mark-bitfield.js`, `mark-n-sweep.js` | [ ] | `storage`/sparse bitfield (local) |
| `clear.js`, `purge.js` | [ ] | `hypercore` clear/truncate |
| `move-to.js`, `snapshots.js`, `streams.js` | [ ] | `hypercore` seek/snapshot/stream |
| `manifest.js` | [ ] | `identity`/signing config |
| `user-data.js`, `groups.js`, `push.js` | [ ] | low priority; classify when reached |
| `replicate.js`, `fully-remote-proof.js`, `wants.js`, `remote-length.js`, `remote-bitfield.js`, `extension.js`, `timeouts.js` | — | networking / replication |
| `encryption.js` | — | encryption deferred |
| `sessions.js`, `preload.js`, `mutex.js`, `compat.js` | — | session plumbing / upstream-version compat |
| `all.js`, `helpers/`, `fixtures/`, `bench/` | — | runner / support |

## `reference/js/autobase/test` (the heart)

| File | Status | Maps to |
|------|--------|---------|
| `linearizer.js` | [ ] | `autobase` linearizer ★★ |
| `dags.js` | [ ] | `autobase` DAG ordering ★★ |
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
| `fuzz/` | [ ] | **model for our convergence sim (gate #3)** |
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
