# Architecture Decision Records

Terse, append-only. Record **every divergence from upstream** here.

## ADR-0001 — Clean-room, not a verbatim port
**Context:** Upstream is JS; we want a Rust substrate shaped for our consumer, not interop.
**Decision:** Reimplement behaviour; we are **not** wire-/disk-/JS-compatible. Cherry-pick ideas.
**Consequence:** No output-equivalence oracle; correctness comes from property tests + ported
upstream behaviour tests + the algorithmic oracle (ADR-0008).

## ADR-0002 — Typed log, generic over blob content
**Decision:** `Hypercore<T, C: Codec<T>>`; `T` is erased to bytes at the storage/proof boundary.
Ordering and verification stay content-blind.
**Consequence:** Ergonomic win over opaque buffers; the codec must be versioned & tolerant
(immutable history + content addressing make the wire format a permanent ABI).

## ADR-0003 — Networking deferred to Iroh
**Decision:** Do not port hyperswarm / HyperDHT / Noise / `hypercore-protocol-rs`. Build cores
against storage/transport abstractions; layer Iroh (core / blobs / gossip) underneath later.

## ADR-0004 — Monorepo Cargo workspace
**Decision:** One workspace; shared internals (`codec`, `merkle`, `identity`, `storage`) as their
own crates rather than copy-pasted, unlike upstream's per-package repos.

## ADR-0005 — WASM-first + browser storage
**Decision:** `hypercore`, `autobase`, and `storage` must build for `wasm32-unknown-unknown` and
run in a browser host; `storage` ships a `localStorage`/IndexedDB backend so logs persist locally.

## ADR-0006 — Loop log / lessons / decisions live in-repo
**Decision:** Keep the porting log, lessons, and ADRs under `docs/` (committed), not in a private
machine-local memory, so any instance/contributor sees full history.
**Consequence:** They are public → ADR-0010 applies.

## ADR-0007 — Convergence test is generic-only
**Decision:** The collaborative-editing test uses a generic toy document in this repo. Any
domain-specific (e.g. real editing-session) integration test lives in the **consuming** app, not
here — it would couple L1 to a domain and risk leaking.

## ADR-0008 — Upstream-JS algorithmic-equivalence oracle
**Decision:** Cross-check our linearizer against `reference/js/autobase` on random DAGs (via node):
same DAG ⇒ identical linearization order.
**Consequence:** Recovers a deterministic, upstream-anchored oracle without wire/disk compat.

## ADR-0009 — Port relevant upstream test suites
**Decision:** Translate the in-scope upstream tests into Rust tests as behavioural specs; track in
`docs/UPSTREAM_TEST_MAP.md`. Exclude networking / wire / disk-interop / sessions / encryption.

## ADR-0010 — Public repo: no private or personal data
**Decision:** Never commit absolute disk paths, machine/user names, emails, tokens/secrets,
internal hostnames, or consumer-project internals — in code, tests, docs, or the porting log. Use
repo-relative paths; sanitize tool output before committing.

## ADR-0011 — Run JS reference only in a sandbox/container
**Context:** The JS oracle (ADR-0008) executes `reference/js/autobase` via node, pulling an
untrusted npm dependency tree (supply-chain exploits are common).
**Decision:** Never run `npm`/`node` against the reference on the host. Route it through
`scripts/node-sandbox.sh` — a container runtime (Apple `container` or docker), with npm install
scripts disabled and optional no-network at runtime. Reading/porting JS to Rust is host-safe;
*executing* it is not.
**Consequence:** The `oracle` gate and any reference-JS execution go through the wrapper; a host with
no container runtime cannot run them, by design.

## ADR-0012 — Single-writer loop; no code-editing subagents
**Context:** Parallel editing agents within one iteration would race on files and the porting log.
**Decision:** Each iteration has exactly one writer — the iteration agent. It may spawn read-only
exploration subagents (e.g. `Explore`) for searching, but must not spawn code-editing subagents or
delegate edits.
**Consequence:** Enforced by the driver prompt (`scripts/iterate.sh`) and CLAUDE.md; subagent use in
the loop is read-only.

## ADR-0013 — Scoped permission allowlist for the driver (no blanket bypass)
**Context:** Headless driver agents must not block on approval prompts, but
`--permission-mode bypassPermissions` disables *all* gates — an autonomous loop running arbitrary
shell, which is unsafe (and was correctly flagged).
**Decision:** Run with `--permission-mode acceptEdits` plus a scoped `.claude/settings.json`
allowlist: allow `cargo` / `rustup` / `rustc` / `just` / `wasm-pack` / `scripts/node-sandbox.sh` and
non-push `git`; **deny** `git push`, `rm`, `curl`/`wget`, and host `node`/`npm`/`npx` (node only via
the sandbox wrapper — reinforces ADR-0011). Subagents are not allowlisted → strictly single-writer.
**Consequence:** Autonomous iterations are constrained to build / test / commit. They cannot push,
delete, exfiltrate over the network, or run untrusted node on the host.

## ADR-0014 — Linearizer is a priority-Kahn topo sort, not upstream's incremental tip
**Context:** Upstream (`reference/js/autobase/lib/topolist.js`) keeps an incremental sorted "tip"
and shuffles each arriving node into place (`moveDown`/`moveUp`), tracking `undo`/`shared` so a
streaming view can be patched cheaply. That bookkeeping is an optimization for live updates, not the
ordering definition.
**Decision:** Reimplement the *behaviour* with a **priority Kahn topological sort**: recompute the
order each call, at every step emitting the causally-ready node with the smallest `NodeId`
(`(writer_key, seq)`, lowest-key-first — the documented "lowest key wins" tiebreak). Enforce causal
delivery on `add` (no duplicate / no seq gap / no dangling head) so the DAG is always acyclic and
causally closed.
**Consequence:** Determinism is *manifest* — `order()` is a pure function of the node set,
independent of arrival order — and it reproduces the canonical linearizations in
`reference/js/autobase/DESIGN.md` (incl. the recursive `[a0, c0, a1, b0, b1, c1, b2]` example). We
do **not** port the `undo`/`shared` reorder-tracking (a streaming optimization) nor the
consensus/quorum confirmation (next capability; the upstream `linearizer.js`/`dags.js` assertions on
*indexed* view length depend on it). Equivalence is at the linearization level for causally-closed
DAGs.

## ADR-0015 — Quorum is a recompute-from-scratch degree; finality is the conservative snapshot form
**Context:** Upstream confirmation lives in `reference/js/autobase/lib/consensus.js` — an
*incremental* `Consensus` machine over vector clocks (`confirms`/`shift`/`_isConfirmed`/
`_isConfirmableAt`, plus merge bookkeeping) that streams the indexed view as nodes arrive. The
*definition* of a quorum, though, is in `DESIGN.md` ("Quorums"): a **vote** is a reference from an
indexer to a node; a node has a degree-1 quorum once a majority of indexers reference it, and the
degree increases each time a majority reference the lower-degree quorum.
**Decision:** Reimplement the *definition*, not the machine. `quorum_degree(target)` is a single
bottom-up pass over a topological order (`order()`): for each node we carry, per indexer, the best
degree any of that indexer's nodes reached over the target within its causal closure, and a node
witnesses degree `k` once a majority vouch level `k-1` (its own author vouching every level up to its
degree). Votes are read purely from causal reachability (`sees`, the graph equivalent of
`clock.includes`) — never a timestamp or a payload. `finalized()` returns the conservative
**snapshot / no-active-fork** prefix: the maximal prefix of `order()` whose nodes have a **double
quorum** (degree ≥ 2) *and* are causally comparable to every other node (no unresolved concurrent
fork around them).
**Consequence:** Determinism is manifest (a pure function of the DAG ⇒ replicas seeing the same set
agree) and the recursive degree reproduces every worked `DESIGN.md` example (the `a0` 1'/2'/3'
quorum chain; the `c0-b0-c1` higher quorum; the conflicting single-quorum pair that must *not*
finalize). We **defer** two things, each its own iteration: (a) the fork/merge competition rule and
the **2-degree-lead caveat** (`DESIGN.md` "Tails, Forks and Merges // todo"; `consensus.js` merge
handling) — `finalized()` refuses to commit either arm of an unresolved fork until a confirmed merge
makes the contested nodes comparable, which is safe but conservative (it may confirm later than
upstream, never earlier/wrongly); and (b) view materialization, so the upstream
`getIndexedViewLength`/`view.get` assertions in `linearizer.js`/`dags.js` stay `[~]`. Finality is
validated as a *property* (a finalized prefix never reorders under cooperative growth), to be
strengthened against arbitrary partitions by the convergence sim (gate #3) and the JS oracle
(gate #4).

## ADR-0016 — Convergence sim: clean-room generator; monotonicity scoped to cooperative growth
**Context:** Gate #3 (`docs/DEFINITION_OF_DONE.md`) is modeled on `reference/js/autobase/test/fuzz/`.
Upstream's `fuzz/helpers.js` generates random DAGs (`createDag`: each node references a random subset
of current tails) and runs `rollBack` — incrementally feeding nodes, randomly deleting head nodes, and
re-deriving — asserting the *confirmed/indexed* prefix never disagrees across runs. It then formats any
failure as a runnable JS repro. It executes `Linearizer` via node and uses `Math.random()`.
**Decision:** Reimplement the *behaviour*, not the harness, as `crates/autobase/tests/convergence.rs`,
host-safe and dependency-free:
- our own **seeded** SplitMix64 PRNG (reproducible; no `rand`/`getrandom`, no `Math.random`), so a
  failing seed is a permanent repro — replacing the upstream "format a JS repro file" machinery;
- two generators: **partitioned** (the `createDag` subset-reference model — forks/merges/reordering)
  and **cooperative** (each node references *all* current tails ⇒ a total order);
- assert the gate's four properties directly: deliver each DAG through several **randomized-Kahn**
  topological orders and check `order()`, a generic content-agnostic **state fold** (rolling FNV
  checksum — no domain types), and `finalized()` are identical across delivery orders (determinism /
  convergence / state-equality), plus per-edge causal-respect.
- **Finality-stability is asserted only on the cooperative generator.** The conservative `finalized()`
  (ADR-0015) requires each finalized node be *comparable to every other node*; under arbitrary
  partitions a **late-arriving concurrent node can strand a previously-finalized node** (it loses
  comparability), so `finalized()` may legitimately *shrink* there. That is the very fork/merge gap
  ADR-0015 defers — not a regression — so on partitioned DAGs we assert only convergence (a pure
  function of the node set always agrees), and we assert *monotonic, never-reordering* growth only
  under cooperative delivery, where no stranding can occur.
**Consequence:** Gate #3 is green and host-safe. We do **not** port `rollBack`'s node-deletion
re-derivation or the deadlock/JS-repro formatting (a test-runner concern). Strengthening `finalized()`
to advance monotonically *through* resolved partitions is the deferred fork/merge work (ADR-0015),
to be cross-checked by the JS oracle (gate #4, ADR-0008).

## ADR-0017 — Merkle: one contiguous-range inclusion proof, not upstream's block+upgrade+seek triplet
**Context:** Upstream hypercore (`reference/js/hypercore/test/merkle-tree.js`) structures proofs as three
composable parts — a **block** proof (`block.nodes`), an **upgrade** proof that extends a verifier from an
old length to a new one (`upgrade.nodes` + `upgrade.additionalNodes`), and a **seek** proof (byte offset →
block). The DoD (`docs/DEFINITION_OF_DONE.md` row #1) asks for "tree + inclusion/**range** proofs".
**Decision:** Implement a single **contiguous-range inclusion proof**: `RangeProof { start, end, leaf_sizes,
nodes, roots }`, the multi-block generalization of our existing single-block `Proof`. `nodes` carries only
the **off-range boundary** siblings (any depth). Generator and verifier run the *same* deterministic
**depth-by-depth climb** over a frontier `BTreeSet` — at each level two in-range/derived siblings pair into
a parent for free, an off-range sibling is supplied — so they agree on the boundary set by construction.
Verification keeps **path nodes and sibling nodes in strictly separate roles**: every on-range node is
recomputed from the block data and is the only thing that can sit on a leaf's path; the proof's `nodes` are
consulted purely as off-path siblings, by index, preferring a recomputed node when one exists. Every
recomputed leaf is force-climbed to a genuine root index (a missing sibling ⇒ rejection), the recomputed
roots are substituted, and `tree_hash` must equal the trusted `expected_root`.
**Consequence:** The DoD merkle box is `[x]` (tree + inclusion + range + tamper-rejection). We **defer**
length-extension (`upgrade`/`additionalNodes`), byte-offset **seek**, and **reorg/recovery** — they remain
tracked on the `merkle-tree.js` / `merkle-tree-recovery.js` rows in `docs/UPSTREAM_TEST_MAP.md` (still
`[~]`/`[ ]`). A single-block range carries exactly the single-block proof's sibling set, so the two APIs
coincide on `end = start + 1`. Soundness rests on recompute-the-path + force-climb-to-a-real-root, so a
prover cannot pair real roots with disconnected forged data.

## ADR-0018 — Batch / atomic append is a staged buffer + atomic commit, not a session/atom overlay
**Context:** Upstream hypercore (`reference/js/hypercore/test/batch.js`, `atomic.js`) builds batching on
**sessions**: `core.session({ name })` returns a batch session you append to (its `length` grows while
`core.length` stays), `core.commit(session)` flushes it atomically (and returns `null` if the core moved
underneath); atomicity is a storage-layer **`atom`** (`storage.createAtom()` + `atom.flush()`) shared
across sessions, emitting `append`/`truncate` events. Sessions and the storage-overlay/`atom` machinery
are out of scope per the relevance filter (`docs/UPSTREAM_TEST_MAP.md`: "sessions / preload / mutex").
**Decision:** Reimplement the **L1 behaviour-under-test**, not the session/atom layer. A `Batch<T>` is a
staged buffer that records the log length it was opened against (`base`) and holds encoded blocks;
`Hypercore::stage` buffers without touching the log; `batch_get` reads through the batch (committed region
from the log, staged region from the buffer); `commit` applies all staged blocks under a **single** signed
head. Commit is **all-or-nothing**: storage writes happen first and roll back on failure, and the Merkle
tree + signed head (the log's source of truth) are mutated only after every write succeeds — so a partial
failure never advances the log. Commit returns `Ok(None)` (log unchanged) on a **stale base** (the log
advanced past `base`), mirroring upstream's "`commit` returns `null` when the core moved".
**Consequence:** Commit-equivalence holds — a committed batch yields a head identical to N single appends
(ed25519 is deterministic), so batching is invisible to verifiers/replicas. We **defer** upstream's
multi-session interactions, `byteLength`, `truncate`/`append` events, and the `atom.flush()` storage
overlay; `batch.js`/`atomic.js` stay `[~]`. Atomic rollback is honestly tested with a fault-injecting
store (`FaultyStore`), not just the happy path.

## ADR-0019 — Fork detection is a self-contained L1 capability, not a replication-time event
**Context:** Upstream hypercore (`reference/js/hypercore/test/conflicts.js`) surfaces a forking writer
as a `'conflict'` event emitted **during replication**: peer `b` replicates from both a writer `a`
(`[a,b,c,d,e]`) and a second core `c` sharing `a`'s keypair (`[a,b,c,d,f,e]`); when the two signed
trees disagree at a length, `b`/`c` emit `conflict` and sessions close. That test is replication- and
session-driven (and is itself `test.skip`ed upstream for a lifecycle flake), so the *mechanism* —
swarm, streams, `'conflict'` events, session teardown — is out of scope per the relevance filter
(networking / sessions). The *behaviour under test* — proving a single writer signed two incompatible
logs — is a pure L1 property and very much in scope for a "secure append-only log substrate".
**Decision:** Reimplement the **detection behaviour** as two self-contained, content-blind primitives
over our existing signed head + Merkle inclusion proof + identity, with no networking and no events:
- `conflicting_heads(public, a, b)` — proof-free: two heads of **equal length but different root**,
  each verifying under the author's key, are a fork (the head at a given length is a deterministic
  pure function of the first `length` blocks, so two roots at one length ⇒ two histories). This is how
  a verifier first *notices* a fork — two contradictory heads at one length.
- `ForkProof { index, head_a/b, data_a/b, proof_a/b }` with `verify(public)` — pins the disagreement
  to a **shared block index**: both sides must be signed by `public` and prove their block at `index`
  (reusing `verify_block`), and the two blocks must differ. Works across heads of **different**
  lengths (truncate-and-rewrite forks), where `conflicting_heads` deliberately abstains. Soundness
  rests on leaf collision-resistance (different bytes ⇒ a different committed leaf) — the same
  assumption the whole Merkle scheme already rests on.
**Consequence:** Fork detection is green and host-safe under `just verify` (no swarm, no Chrome). We
**defer** the replication-time plumbing that produces these inputs in a live system (peer streams, the
`'conflict'` event, session close) — it returns with networking (Iroh, ADR-0003). `conflicts.js` stays
`[~]`: the L1 detection behaviour is ported; the replication/session mechanism is out of scope.
Different-length *honest* extensions and different-author logs are correctly **not** flagged.

## ADR-0020 — Merkle upgrade proof is a data-free consistency proof, not upstream's block+upgrade bundle
**Context:** Upstream hypercore (`reference/js/hypercore/test/merkle-tree.js`, "proof with upgrade*")
bundles a length-extension into the *same* proof object as the block/seek proof:
`upgrade: { start, length }` yields `upgrade.nodes` (to reach the requested length) **plus**
`upgrade.additionalNodes` (to reach the tree's *actual* larger length), and the bundling lets the block
proof's leaf double as an upgrade node (e.g. block 1's leaf 2 is omitted from `upgrade.nodes`). This is
an optimization for the replication wire, where one round-trip carries block + seek + upgrade together.
**Decision:** Implement the length-extension as a **standalone, data-free consistency proof**
`MerkleTree::upgrade_proof(old, new) -> UpgradeProof { old_len, new_len, nodes }`, the cross-length
analogue of `conflicting_heads` (ADR-0019). It proves the signed tree at length `new` is an
**append-only extension** of the tree at length `old` (the first `old` blocks were not rewritten) by
supplying only the **fully-new** subtree nodes (every covered block `>= old`) needed to fold the
verifier's *own trusted old roots* up into the new roots; `verify(old_roots, new_root_hash)` rejects any
supplied node that is not fully-new, so prover data can never sit on or stand in for an old block, and
the new roots are necessarily rebuilt from the trusted old prefix. Generator and verifier share one
descent/climb (like `range_proof`, ADR-0017): the generator walks down from each new root, stopping at
old roots and emitting the largest fully-new subtrees; the verifier seeds its frontier with the old
roots, folds the supplied nodes, climbs sibling pairs, and checks `tree_hash(new_roots) == new_root_hash`.
**Consequence:** Length-extension / anti-fork-across-lengths is ported and composes cleanly with
`range_proof` (confirm the honest append, then verify the new blocks). We **keep proofs separate** (no
block+seek+upgrade bundle) and require `1 <= old < new <= len` (an `old = 0` "upgrade" has no trusted
anchor, so it is meaningless and refused). We still **defer** upstream's `additionalNodes` (proving past
the requested length), byte-offset **seek**, and **reorg/recovery** — `merkle-tree.js` stays `[~]` and
`merkle-tree-recovery.js` `[ ]`. Soundness rests on the same leaf/parent collision-resistance the scheme
already assumes.

## ADR-0021 — Replica verifies a longer head against its own roots before fetching (upgrade gate)
**Context:** Upstream hypercore applies a longer remote head inside the replication protocol's proof
exchange: a peer sends a proof carrying `upgrade.nodes`/`additionalNodes`, the receiver verifies and
applies it (`core.verify` / `_verifyAndApplyUpgrade`), advancing its local signed length, then fetches
the blocks. The upgrade is part of the wire proof object (ADR-0020: upstream bundles block+seek+upgrade).
**Decision:** Wire the standalone, data-free `UpgradeProof` (ADR-0020) into `Replica` as a pure
**pre-fetch gate** `verify_upgrade(new_head, proof)`. It accepts a longer signed head only if (a) the
author signed it, (b) the proof bridges *exactly* from the replica's current verified length to the new
head's length (`old_len == len()`, `new_len == new_head.length > len()`), and (c) folding the proof's
fully-new nodes into the replica's **own** trusted roots reconstructs `new_head.root`. It does **not**
mutate the replica; the new blocks `[old, new)` are then fetched with the existing `add_block` against the
verified head. `Hypercore::upgrade_proof(old, new)` exposes the generator on the source side.
**Consequence:** A replica can no longer be lured onto a forked history by a self-consistent longer head —
the fork is caught **before any new block is downloaded** (anti-fork at the replication level; the
cross-length analogue of `conflicting_heads`/`ForkProof`, ADR-0019). We keep proofs separate (no bundled
block+upgrade object) and the gate purely verifying. We **defer** signed-length fast-forward,
`additionalNodes` (proving past the requested length), and the wire framing that delivers these inputs in a
live system — they return with networking (ADR-0003) and the `fast-forward.js` row. The empty-replica case
(`old = 0`) has no trusted anchor, so it has no upgrade gate and replicates from scratch against the head
directly. `core.js` moves toward verified incremental replication.

## ADR-0022 — Byte-offset seek is a tree-accelerated locator + a standalone, data-free seek proof
**Context:** Upstream hypercore (`reference/js/hypercore/lib/merkle-tree.js`) supports **byte seeks** two ways:
a local `ByteSeeker` that descends the flat tree by subtree byte `size` to map a byte offset to a block in
O(log n) (the `merkle-tree.js` "basic tree seeks" test asserts it equals a linear scan), and a **seek proof**
(`seekProof`/`verifyTree`'s seek branch) that an untrusted peer sends so a verifier can confirm the mapping
against the signed root. Upstream **bundles** the seek nodes into the same wire proof object as block/upgrade
(`proof.seek.nodes`, with the block leaf sometimes doubling as a seek node), and threads a `padding` parameter
(per-block framing overhead subtracted from each node's size).
**Decision:** Implement the L1 behaviour as two pieces, kept **separate** (consistent with ADR-0017/0020):
- `MerkleTree::seek(bytes) -> (block, offset)` — the tree-accelerated locator (descend by subtree `size`),
  agreeing with the linear scan for every offset; past-the-end returns `(len, bytes - total)` like upstream's
  linear seek. No `padding` (that is application framing — it would leak domain/byte-layout assumptions into
  L1; a consumer subtracts its own framing before seeking).
- `MerkleTree::seek_proof(bytes) -> Option<SeekProof>` + `SeekProof::verify(expected_root) -> Option<(block,
  offset)>` — a **standalone, data-free** proof. It is the target block's inclusion path (siblings + roots +
  the leaf node) plus the byte offset; `verify` climbs the leaf to its root via `parent_hash` (which binds each
  child's hash **and** size), substitutes the recomputed root, checks `tree_hash == expected_root`, then derives
  the block's left-cumulative byte size from the **authenticated** left-sibling and left-root sizes and accepts
  iff `cumulative <= bytes < cumulative + leaf.size`. Carries no block data (a seek locates, it does not reveal).
**Consequence:** Byte-addressed random access + a verifiable byte→block locator are ported and compose with the
existing inclusion `Proof` (fetch the located block's data with a separate `proof`). We **keep proofs separate**
(no block+seek+upgrade bundle) and **defer** `padding`, the bundled-wire seek (where the block leaf doubles as a
seek node), and reorg/recovery — `merkle-tree.js` stays `[~]`, `merkle-tree-recovery.js` `[ ]`. Soundness rests
on the same hash/size binding (`parent_hash` over child sizes, `tree_hash` over root sizes) the scheme already
assumes, plus the disjoint-contiguous-interval argument: exactly one block's authenticated byte interval brackets
`bytes`, so a prover cannot pass off a different block.

## ADR-0023 — Node recovery is storage robustness + a data-free `NodeProof`, not replication-driven repair
**Context:** Upstream hypercore (`reference/js/hypercore/test/merkle-tree-recovery.js`) handles a tree with
**deleted tree nodes** (e.g. a root or sub-root removed from disk): the core still `ready()`s and keeps its
`length`, enters `_repairMode` (refusing appends/truncates: "Cannot commit while repair mode is on"), and the
missing node is restored either from a **fully-remote proof** (`generateRemoteProofForTreeNode` →
`recoverFromRemoteProof`) or by **replication** (a range request that auto-includes the roots, peer requests,
`repairing`/`repaired`/`repair-failed` events). A mangled proof must fail and leave storage untouched
("atomically updates storage"). The replication machinery (swarm, peer streams, the repair events,
range-request auto-repair) is networking/sessions — out of scope per the relevance filter.
**Decision:** Reimplement the **L1 behaviour-under-test** as storage robustness + a standalone, data-free
proof, with no networking and no events:
- The tree's nodes are the source of truth (our `BTreeMap`). `remove_node(index)` is the corruption injector
  (analogue of `deleteTreeNode`); `missing_nodes()`/`is_intact()` **derive repair mode** from the length — a
  node at `index` is implied iff its whole block range is within `[0, len)` — so no stale flag is needed.
  `try_roots()`/`try_root_hash()` are panic-free over a gap (return `None`), and `try_append` refuses while
  not intact (extending a corrupt tree could bake an inconsistent root into the log).
- `NodeProof { node, siblings, roots }` (from `node_proof(index)`, the `generateRemoteProofForTreeNode`
  analogue) authenticates an **arbitrary** tree node — leaf, interior, or root — by climbing it to its
  containing root via `parent_hash` (binding hash **and** size), substituting the recomputed root, and
  requiring `tree_hash(roots) == expected_root` (the trusted signed head); it returns the authenticated
  `Node`. It is the arbitrary-node generalization of `Proof` (which always starts from a leaf recomputed
  from block data). `recover_node(&proof, expected_root)` is **verify-then-store**: a tamper (mangled
  node/sibling, dropped sibling, wrong root) is rejected and the tree is left **unchanged** (atomic).
**Consequence:** Local recovery from a remote proof + repair-mode robustness are ported and host-safe under
`just verify`. `merkle-tree-recovery.js` moves `[ ]`→`[~]`. We **defer** the replication-driven repair
(range-request auto-repair, peer requests, the `repairing`/`repaired`/`repair-failed` events) — it returns
with networking (ADR-0003) — and still defer reorg/`additionalNodes`. Soundness rests on the same leaf/parent
collision-resistance the scheme already assumes (a peer cannot foist a wrong node without colliding the
trusted root). A corrupt *source* cannot prove the node it lost (`node_proof` needs the node present), so
proofs necessarily flow from a healthy holder to the gap.

## ADR-0024 — Truncate is a pure rewind-to-a-prefix + a signed fork counter; equivocation is same-fork
**Context:** Upstream hypercore (`reference/js/hypercore/test/core.js` "core - append and truncate";
`move-to.js`) truncates a log via `MerkleTreeBatch.truncate(length, fork)` against the storage layer:
it pops roots past the new length, sets a caller-supplied **`fork`** counter, recomputes `byteLength`,
flips `upgraded`, persists a reorg hint, and tracks `lastTruncation { from, to }`. The signed head/
manifest binds the fork, so a reader follows the highest fork and a deliberate truncate-and-rewrite is
distinguishable from a malicious one. The storage-batch / atom / reorg-hint machinery is sessions/
storage plumbing (out of scope per the relevance filter).
**Decision:** Reimplement the **L1 behaviour-under-test**, not the storage-batch layer:
- `MerkleTree::truncate(new_len)` is a pure in-memory rewind — `retain` only nodes whose whole block
  range lies in `[0, new_len)`. Because the surviving blocks were never touched, the kept node set
  (and every hash) is exactly a fresh tree of the first `new_len` blocks, so `root_hash()` equals the
  prefix's root with **no recomputation**. `byte_length()` is the sum of the (authenticated) root
  subtree sizes.
- A **`fork` counter** binds into the head message (`head_message(fork, length, root)`) and
  `SignedHead`. `Hypercore::truncate(new_len)` rewinds the tree, **increments `fork`** (we auto-bump by
  one rather than taking an explicit fork like upstream — the loop's writer is the sole authority), re-
  signs, and records `last_truncation`; `append`/`commit` clear it.
- The fork counter refines fork detection (extends ADR-0019): an **equivocation** is two contradictory
  histories at the **same** fork, so `conflicting_heads` and `ForkProof::verify` now require equal
  `fork`. A divergence across **different** forks is a legitimate author reorg (readers follow the
  highest fork) and is not flagged.
**Consequence:** Truncate is atomic and infallible (pure in-memory mutation of the tree + head, the
log's source of truth). We **defer** physical storage reclamation — blocks at `>= new_len` become
logically unreachable (`get`/`block` gate on the length) and are overwritten when those indices are
re-appended; reclaiming them eagerly is a separate capability (upstream `clear.js`/`purge.js`). We also
defer reorg-by-proof (accepting a remote truncation/reorg over the wire) and `additionalNodes`. The
`fork` field changes the head's signed-message format (a clean-room divergence; we are not
wire-compatible anyway, ADR-0001) — every signing/verifying site routes through `head_message`, so the
change is centralized. `core.js` advances (truncate behaviour ported); `conflicts.js` gains the
same-fork equivocation refinement.

## ADR-0025 — Reorg is a local LCA + adopt-suffix on the tree; the secure replica gate is deferred
**Context:** Upstream hypercore (`reference/js/hypercore/lib/merkle-tree.js`'s `ReorgBatch` +
`MerkleTree.reorg`/`_updateDiffRoot`/`_update`, exercised by `merkle-tree.js`'s "lowest common
ancestor" tests) reorganizes a local tree onto a peer's divergent/rewritten history: it verifies the
peer's signed **upgrade proof**, finds the topmost differing **root** (the `diff`), then narrows the
divergence down to a block via a **multi-round `want`/`update` node-request protocol** (each round
fetches a `hash` proof for a specific index), yielding `ancestors` = the shared-prefix length, and
adopts the peer's roots from there. The narrowing rounds are replication (peer requests over the wire);
the *behaviour under test* in `merkle-tree.js` is itself host-safe (a local `reorg(clone, core)` helper
over two in-memory trees, no swarm).
**Decision:** Reimplement the **L1 behaviour-under-test**, not the request protocol:
- `MerkleTree::lowest_common_ancestor(&other)` is the content-blind divergence finder. Because the head
  at a length is a pure function of the first `length` blocks (the property truncate/fork-detection rest
  on), two trees agree on `[0, a)` iff their `prefix_root_hash(a)` are equal; prefix agreement is
  **monotone**, so the LCA is a **binary search** over `0..=min(len)` comparing only authenticated prefix
  root hashes — never payload bytes (unlike upstream's incremental top-down root descent, which the
  multi-round wire protocol forces; with both full trees in memory the binary search computes the same
  `ancestors` in one shot). Both trees must be intact; a gap reads conservatively as disagreement.
- `MerkleTree::reorg(&other)` keeps the shared LCA prefix (it `truncate`s to it — the surviving nodes
  already equal `other`'s prefix, so the prefix is preserved, not re-derived) and adopts `other`'s nodes
  for the divergent suffix, leaving the tree byte-identical to `other`. It is **fork-agnostic** (it
  reorganizes tree nodes), the content-following counterpart of `truncate` (ADR-0024): truncate is the
  author rewinding its own log; reorg is a holder following the author onto a rewritten history (readers
  follow the highest fork).
**Consequence:** The `merkle-tree.js` "lowest common ancestor" tests (small/bigger gap, remote shorter,
simple fork, long fork) are ported and host-safe under `just verify`. We **defer** the *secure
replica-level* reorg — authenticating *which* `other` to follow via the signed head + fork counter, and
the `want`/`update` proof-narrowing exchange that delivers a peer's divergent nodes — to the hypercore
layer; it is networking-driven (peer requests/events, ADR-0003), the cross-fork analogue of how iter 14
(ADR-0021) wired the data-free `UpgradeProof` into `Replica::verify_upgrade` for the length-extension
case. We also still defer `additionalNodes` (which, with standalone proofs per ADR-0020, adds no L1
capability our `upgrade_proof(old, any_new)` lacks). `merkle-tree.js` stays `[~]` (LCA/reorg added; the
bundled-wire seek + `additionalNodes` remain). Soundness rests on the same prefix-root collision-
resistance the scheme already assumes.

## ADR-0026 — Secure replica-level reorg re-anchors the upgrade proof on the shared prefix's roots
**Context:** ADR-0025 implemented the *local-tree* reorg (`MerkleTree::lowest_common_ancestor` +
`reorg`, both trees in memory) and explicitly **deferred the secure replica-level reorg** — a
verify-only `Replica` (no peer's full tree, only a signed head) following the source's
truncate-and-rewrite — as the cross-fork analogue of how iter 14 (ADR-0021) wired the data-free
`UpgradeProof` into `Replica::verify_upgrade` for the same-fork length-extension case. Upstream
drives this inside the replication protocol's `want`/`update` proof-narrowing exchange, which is
networking (ADR-0003).
**Decision:** Reimplement the **L1 gate**, not the wire exchange. `Replica::verify_reorg(new_head,
ancestors, proof)` (pure) + `Replica::reorg(..)` (verify-then-truncate). A reorg is followed only at
a **strictly higher `fork`** than the replica's current head (a same/lower fork is a stale head or an
*equivocation* — ADR-0019/0024 — never a history to adopt) and only if the author signed `new_head`.
The shared-prefix length `ancestors` is **authenticated, not trusted**, by *re-anchoring the same
`UpgradeProof` (ADR-0020) on the replica's own roots at `ancestors`* — exposed as
`MerkleTree::prefix_roots` (and `prefix_root_hash`, now public): because the head at a length is a
pure function of the first `length` blocks, those roots equal the source's roots there **iff** the
prefix is genuinely shared, so the fold reaches `new_head.root` only for a real ancestor. Three
cases: `ancestors == new_head.length` (pure truncation — the new head *is* our prefix, no proof);
`ancestors == 0` (no prefix to anchor — adopt the signed higher-fork head from scratch, every block
re-verified on refetch); otherwise the proof bridges `ancestors -> new_head.length`. `reorg` then
`truncate`s to `ancestors` (preserving the shared prefix, ADR-0024) and the caller refetches the
suffix with the existing `add_block`.
**Consequence:** Closes the iter 17 truncate loop — a replica now follows the author's reorg
end-to-end and ends **byte-identical** to the rewritten history, while a forking writer's rewrite of
*old* history is rejected (the honest prefix can't fold to the forked root) before any divergent
block is fetched. `ancestors` over-claiming is rejected (a too-large prefix the new history doesn't
share); under-claiming is safe (a genuine shorter shared prefix — only extra refetch), so finding the
**maximal** ancestor (the `lowest_common_ancestor` binary search, ADR-0025) stays a pure efficiency
concern. We still **defer** the `want`/`update` proof-narrowing wire exchange that *discovers*
`ancestors` and delivers the suffix proofs in a live system (networking, ADR-0003) — here the test
supplies `ancestors` (the construction-known divergence point) and the source produces the proofs.
`core.js` advances (secure reorg-follow ported); the deferred replica-level reorg of ADR-0025 is now
done. Soundness rests on the same leaf/prefix-root collision-resistance the scheme already assumes.

## ADR-0027 — `topolist.js` ordering: an in-Rust oracle validates priority-Kahn ≡ insertion sort
**Context:** `reference/js/autobase/test/topolist.js` exercises upstream's incremental linearizer
(`lib/topolist.js`). Its in-scope, L1-relevant assertion is **stable ordering** — the linearization
is a pure function of the node set: the same DAG, delivered in any causally-valid order, yields the
same order (the `stable ordering`, `fuzz`, and `optimistic N` tests all assert this invariance). ADR-0014
already reimplemented the *ordering* as a **priority-Kahn** topological sort (emit the smallest
causally-ready `NodeId`) instead of upstream's incremental insertion sort (`moveDown`/
`moveNonOptimisticUp` with `undo`/`shared` patch-tracking), claiming the two produce the *same* order
for causally-closed, non-optimistic DAGs — but that equivalence was never turned into an asserting test
(the JS oracle, gate #4, is environment-blocked: iters 11–19).
**Decision:** Port the ordering behaviour and turn ADR-0014's equivalence claim into a host-safe,
in-Rust asserting test (`crates/autobase/tests/topolist.rs`), with **no `node` and no container**:
- a **faithful, test-only re-statement** of upstream's *non-optimistic* `lib/topolist.js` insertion
  sort (`topolist_oracle`: `add` → `moveDown` to the causal floor, then `moveNonOptimisticUp` past
  strictly-smaller nodes; `cmp`/`cmpUnlinked`/`links` over `direct[a]` = explicit heads ∪ same-writer
  predecessor — exactly the union upstream's `links` recognizes). It is a behavioural mirror used
  *only* as a test oracle, **not** the production path.
- a cross-check that the oracle equals our `Linearizer::order()` on the canonical `DESIGN.md` DAGs, the
  explicit `topolist - stable ordering` example (`[a0, b0, c0, c1]`, where `c1` follows `c0` purely by
  same-writer sequencing), and a battery of **seeded random fork/merge DAGs** (200 seeds × several
  randomized-Kahn delivery orders) — each also asserting the oracle is itself **delivery-order
  independent** (upstream's `stable ordering`/`fuzz` property).
  Both topolist's non-optimistic insertion sort and our priority-Kahn compute the **lexicographically-
  minimal linear extension under (writer key, seq)**, which is unique — hence equal.
**Consequence:** `topolist.js` moves `[ ]`→`[~]`: the ordering/stable-ordering behaviour is ported and
ADR-0014's priority-Kahn≡insertion-sort claim is now an asserting cross-check. We **defer** the
**streaming-view bookkeeping** (`undo`/`shared`/`mark`/`flush`/`indexed` — a live-view patch optimization,
not the ordering definition; we recompute `order()` each call) and **optimistic** nodes (a separate
writer-admission feature; `optimistic.js` row `[ ]`). This in-Rust oracle **complements, does not
replace,** the upstream-JS algorithmic-equivalence oracle (gate #4, ADR-0008): gate #4 runs the *actual*
reference code in a sandbox and remains the deferred cross-language check; this validates the same
equivalence at the algorithm level, host-safely, today.

## ADR-0028 — View materialization is the identity fold at L1; indexed view = the finalized prefix
**Context:** Upstream autobase (`reference/js/autobase/test/linearizer.js`, `dags.js`) linearizes the
multi-writer DAG and then **applies** each node to materialize a `view` (a hypercore the consumer
reads). The tests assert `view.length` (total materialized length), `view.get(i)` (entry `i`, `null`
past the end), and `getIndexedViewLength` (`getIndexedInfo().views[].length` — how much of the view is
**confirmed**, the indexed prefix that can never reorder), plus that every replica agrees on these. The
apply step is where *domain* logic lives (`apply.js`), and a node may emit a **batch** of view entries.
**Decision:** The apply step is domain logic we deliberately do not have (L1 is content-blind), so the
domain-agnostic materialization is the **identity fold**: each node contributes exactly one entry — its
own `NodeId`. Expose it as thin accessors over the existing linearizer: `view()` ≡ `order()`,
`view_len()` ≡ `view.length` (= node count, one entry per node), `view_get(i)` ≡
`view.get(i, {wait:false})` (`None` past the end), `indexed_view()` ≡ `finalized()`, and
`indexed_view_len()` ≡ `getIndexedViewLength`. A consuming application replays `view()` through its own
apply function to build the real, typed view; the ordering/confirmation it relies on is what lives here.
**Consequence:** The `view` / `view.get` / `getIndexedViewLength` behaviour is ported as
`crates/autobase/tests/view.rs`. We assert the **exact upstream numbers only where our conservative
confirmation matches** — the fork-free `c-b-a-c-b-a` indexer chain (`linearizer - simple` /
`dags - simple 3`): `view_len == 6`, the per-index `view_get` sequence, `view_get(6) == None`, and
`indexed_view_len == 4` (the double-quorum'd `[c0,b0,a0,c1]` prefix), because for a fork-free chain our
double-quorum finalization (ADR-0015) equals upstream's confirmed length exactly. We additionally assert,
as a **property holding for every DAG**, that the view and the indexed view length converge across all
causally-valid delivery orders (the `getIndexedViewLength(a)==(b)==(c)` family) and that the indexed view
is always a prefix of the view. We **defer** the indexed-length *values* where upstream confirms earlier
than our conservative form — a **unanimous single quorum** (`dags - simple 2`, `n == 2`: a single quorum
*is* all `n` indexers, so it is safe, but our rule still requires a double quorum) and confirmation across
a **resolved fork/merge** (`linearizer - compete` / `count ordering`) — both the deferred fork/merge
consensus (ADR-0015) — and the **per-replica partial view** (each base seeing a different node subset
before full sync; cross-replica view *convergence* still holds and is the property test above). We do
**not** add a typed payload or a per-node batch (domain concerns, ADR-0002/0007). `linearizer.js` /
`dags.js` stay `[~]`: view materialization + the fork-free indexed length + cross-replica convergence
ported; the fork/merge confirmed lengths and the apply/view layer (`apply.js`/`anchors.js`) remain.

## ADR-0029 — Proof verifiers bind structural position, not just the root hash
**Context:** `parent_hash` binds each child's hash + size but **not** its flat-tree index, so the final
`tree_hash == root` check authenticates *content*, not *position*. An audit (after iteration 21) found
`SeekProof::verify` trusted a prover-supplied `leaf`: an interior/root node (odd index) authenticates
against the real root and its aggregate subtree size brackets any byte offset, so a prover could get a
bogus `index/2` block accepted — **a real soundness bug**. Upstream's `ByteSeeker` guards `(index & 1)
=== 0`; our clean-room reimpl dropped it. `Proof`/`SeekProof` also omitted the `sib.index ==
flat::sibling(..)` guard that `NodeProof::verify` has.
**Decision:** Every proof verifier enforces structural position explicitly — a seek leaf must be even
(a real block), and every supplied sibling must be the path node's actual flat-tree sibling. Position
must never rest solely on collision-resistance.
**Consequence:** `SeekProof::verify` rejects odd-index leaves; `Proof`/`SeekProof` gained the
sibling-index guard (now consistent with `NodeProof`); regression tests pin both. A deeper fix —
binding child indices into `parent_hash` — would make these structural by construction but rewrites
every hash (a permanent ABI change), so it is deferred as a decision, not a quick fix.

## ADR-0030 — Sparse bitfield is the pure L1 data structure; persistence & replication chunking deferred
**Context:** Upstream hypercore (`reference/js/hypercore/lib/bitfield.js`, exercised by
`test/bitfield.js`) tracks which blocks a holder has as a paged bitfield. Its surface mixes three
concerns: (1) the **data structure** — `get`/`set`/`setRange`/`count`/`findFirst`/`findLast`
(+`first/lastSet/Unset`) over an unbounded, sparse field; (2) **persistence** — `static open(storage,
length)` rehydrates pages from a storage stream and `flush(tx)` writes dirty pages through a storage
transaction (with `BitInterlude` staging a batch of bit changes, `bit-interlude.js`); (3)
**replication framing** — `*want(start, length)` chunks 32-bit-aligned segments to send to a peer.
Internally upstream uses a `BigSparseArray` of fixed pages grouped into segments, with `findFirst`/
`findLast` recursing page→segment→field.
**Decision:** Reimplement only the **L1 data structure** (`crates/storage/src/bitfield.rs`,
`storage::Bitfield`), clean-room (ADR-0001) — not byte/disk/wire compatible. We keep the page
granularity (`2^15` bits) so the page/segment **boundary behaviours** the tests pin line up, but store
pages in a plain `BTreeMap<u64, Box<[u64; 512]>>` (no segment layer / `BigSparseArray`); a **missing
page is semantically an all-`false` page** and is never materialized just to clear bits in it (mirrors
upstream's `if (!p && val)`). Query semantics match upstream exactly: `count(start, length, val)` takes
a **length** (not an end); `find_first(false, ..)` always returns `Some` (infinite-zero tail) while
`find_first(true, ..)`/`find_last(..)` return `None`/`Option` when absent (upstream's `-1`).
**Consequence:** The pure-structure behaviour of `bitfield.js` is ported and host-safe under
`just verify`; it is the local presence map `clear`/`purge`/sparse cores will build on. We **defer** as
out of scope per the relevance filter: persistence (`open`/`flush`/`BitInterlude` — storage backend &
disk format, returns with the `storage` IndexedDB/native work) and `want` (replication wire framing,
networking, ADR-0003). The bitfield row (`bitfield.js` + `bit-interlude.js`/`mark-bitfield.js`/
`mark-n-sweep.js`) stays `[~]`: the core structure is ported; staged-tx changes and GC marking
(`mark-*`/`*-sweep`) are not yet built.

## ADR-0031 — `clear` is presence reclamation over the bitfield; the tree is untouched, purge/redownload deferred
**Context:** Upstream hypercore (`reference/js/hypercore/test/clear.js`) drops the locally-stored bytes
for a block range without shortening the log: `core.clear(start, end)` clears the blocks' data and
their bits in the presence bitfield, so `core.has(i)` becomes false and `core.get(i, {wait:false})`
returns `null`, while `contiguousLength` shrinks to the first hole and the **Merkle tree / signed
length are unchanged** — the blocks remain authenticated and re-downloadable from a peer (the
`clear + replication` tests). `purge.js` is the orthogonal "delete the whole core + close all sessions"
operation (file removal + session lifecycle). Most of upstream's `clear` surface is storage/disk
(block streams, storage transactions) and replication (re-download); the *behaviour under test* — the
presence/length separation — is pure L1.
**Decision:** Wire iter 27's `storage::Bitfield` (ADR-0030) into `Hypercore` as a local **presence
map**, and reimplement the **L1 behaviour-under-test** of `clear`, not the storage-stream / session
layer. `append`/`commit` set the new blocks' presence bits (commit only *after* its writes succeed, so
a rolled-back failed commit leaves presence untouched — consistent with ADR-0018); `truncate` clears
the discarded tail's bits. `has(index)` = within length **and** the bit is set; `get`/`block` return
`None` for an absent block (a no-wait read — at L1 there is no peer to wait on) and reserve
[`Error::Corrupt`] for a genuine "bit set but bytes missing" inconsistency; `contiguous_length()` is
the first absent index capped at the length. `clear(start, end)` clears the present blocks in the
range — clearing the bit (logical absence) then best-effort-deleting the bytes — and returns the count
cleared (`0` == upstream's `null`/no-op). It **never touches the Merkle tree**, so the signed head and
every block proof are unaffected: a cleared block is still authenticated and re-verifiable from a
holder (demonstrated host-safely without the wire by verifying a holder-supplied block against the
unchanged head). Clearing absent or out-of-range blocks is a harmless no-op (upstream "no side effect
from clearing unknown nodes").
**Consequence:** The L1 presence/length separation of `clear.js` is ported and host-safe under
`just verify` (`has`/`contiguous_length`/`get`-returns-`None`/the tree-untouched invariant). `Replica`
is left as-is (it builds `[0, len)` strictly in order and is implicitly fully-present; sparse-replica
presence is a networking concern). We **defer**: the replication re-download that *refills* a cleared
block (networking, ADR-0003); `purge` (whole-core deletion is storage-backend + session lifecycle, out
of scope per the relevance filter); physical storage reclamation guarantees (clearing is best-effort —
a failed `delete` still marks the block absent, leaving an orphan, the same "logical state is the
invariant" stance as ADR-0024's truncate and iter 23's rollback orphan); and the `diff`/byte-API return
shape (commented out upstream). `clear.js` moves `[ ]`→`[~]`; `purge.js` stays `[ ]`. The `bitfield.js`
row gains its first L1 *consumer* (`Hypercore` presence), tightening the deferred persistence story.

## ADR-0032 — Snapshot is a self-contained by-value point-in-time view; signed length is the shared-prefix LCA
**Context:** Upstream hypercore (`reference/js/hypercore/test/snapshots.js`) exposes `core.snapshot()` —
a read-only view pinned at a length. Its headline behaviour ("snapshot does not change when original
gets modified"): the snapshot's `length` is fixed; `snap.get(i)` keeps returning the block as it was at
snapshot time even after the core appends, **truncates below the snapshot, and re-appends different
content** over those indices; and `snap.signedLength` reflects how much of the snapshot the *current*
core still backs (it drops to 2 once the core truncates below 3, and stays there after a divergent
re-append). Upstream snapshots **share storage** with the core and rely on copy-on-write / fork
namespacing at the disk layer to keep the old bytes readable after a rewrite; they also carry
session/replication behaviour (`signedLength` over the wire, `createReadStream`, implicit-snapshot gets,
`SNAPSHOT_NOT_AVAILABLE`, atomized sessions) that is storage-plumbing / sessions / networking — out of
scope per the relevance filter.
**Decision:** Reimplement the **L1 behaviour-under-test** as a **self-contained, by-value** snapshot,
not the shared-storage / copy-on-write disk model. `Hypercore::snapshot()` returns a `Snapshot<T, C>`
owning an immutable copy of the present blocks `[0, len)` (encoded bytes), the `MerkleTree` at that
length, the captured `SignedHead`, and a clone of the codec — so the snapshot observes the log exactly
as it was when taken and is immune to any later mutation of the original (the simplest way to guarantee
"survives truncate-and-rewrite" without disk-layer COW). `length()`/`fork()`/`head()`/`root_hash()` are
fixed; `block(i)`/`get(i)` read the captured bytes (`None` past the length — our L1 form of upstream's
out-of-range `SNAPSHOT_NOT_AVAILABLE`, consistent with `Hypercore::get`'s no-wait `None`); `proof(i)`
authenticates a captured block against the captured head (a snapshot is independently verifiable even
after the core forks away). `signed_length(&core)` is the content-blind shared-prefix length
([`MerkleTree::lowest_common_ancestor`] of the snapshot's tree and the core's current tree), which
reproduces every `signedLength` assertion exactly (it never exceeds the snapshot length and drops the
moment the core truncates below or rewrites a block within the snapshotted prefix). The two built-in
codecs (`U64`/`Bytes`) gain `#[derive(Clone, Copy, …)]` so the snapshot can own a codec to decode with —
a Rust-ergonomics detail (zero-sized config types), not a behavioural divergence.
**Consequence:** The headline `snapshots.js` behaviour is ported and host-safe under `just verify`
(static length; survives append/truncate/re-append; `signed_length` LCA; independent authentication; the
empty/static-length and out-of-range cases). `move-to.js`/`snapshots.js`/`streams.js` splits:
`snapshots.js` moves `[ ]`→`[~]`. We **diverge** from upstream's shared-storage COW (we copy bytes by
value — identical observable behaviour, simpler at L1; a consuming app that needs zero-copy snapshots can
layer it on the storage backend) and **defer**: `signedLength` propagation over replication, implicit
per-call snapshotting during a live download, `createReadStream`/streams (`streams.js`), and the
session/atom cases (sessions/networking, out of scope). Soundness of the independent-authentication
property rests on the same head-signature + inclusion-proof guarantees the core already provides.

## ADR-0033 — Read/byte streams are no-wait L1 iterators; byte addressing is encoded-byte, padding-free
**Context:** Upstream hypercore (`reference/js/hypercore/test/streams.js`) exposes
`createReadStream({ start, end, live, reverse, snapshot })` — an async iterator over **decoded** blocks —
and `createByteStream({ byteOffset, byteLength })` — an async iterator over **raw block buffers** covering
a byte range, located by the byte-seek — plus `createWriteStream` (a writable that appends). `live` tails
the log for newly-appended blocks; both are Node duplex/readable streams with backpressure; and the byte
stream's offsets are over the raw **value** byte layout (upstream stores values un-framed). The
async-runtime/duplex-stream machinery is session/runtime plumbing, `live` tailing is networking, and
value-byte addressing needs the per-block framing `padding` we deliberately omitted (ADR-0022).
**Decision:** Reimplement the **L1 behaviour-under-test** as synchronous Rust **iterators**:
- `read_stream(ReadStreamOptions { start, end, reverse, live })` → `ReadStream`, yielding
  `Result<T, Error>` for each **present** block in `[start, end)`, forward or `reverse`. `end` defaults to
  and is clamped to `len()`. It is **no-wait** (consistent with `get`): an absent block — never downloaded
  or dropped by `clear` — is *skipped*, not waited on (there is no peer at L1). `live` is accepted but
  **ignored** (no async tail to keep open), so upstream's "live should be ignored" case ports directly
  (set `live: true`, the stream still stops at `end`).
- `byte_stream(ByteStreamOptions { byte_offset, byte_length })` → `ByteStream`, yielding
  `Result<Vec<u8>, Error>` of whole **encoded** blocks covering `[byte_offset, byte_offset+byte_length)`:
  `seek(byte_offset)` locates the start block, whole blocks are emitted until the byte budget is consumed
  (`byte_length` defaults to the rest of the log), and an empty-payload block is still emitted while the
  budget is non-zero (upstream's "decode previous blocks even though they don't contribute to byte
  length"). Offsets address the **encoded** byte layout the tree authenticates, not the decoded payload —
  the `padding` divergence (ADR-0022): a consumer subtracts its own framing before seeking; a non-boundary
  offset emits the whole block it lands in.
- `createWriteStream` is a buffered `append` of the same blocks — no new L1 behaviour — so it is covered by
  `append`/batch and is **not** given its own type.
**Consequence:** `streams.js` moves `[ ]`→`[~]`: read/byte stream iteration (start/end/reverse, the
byteOffset/byteLength cases, empty-payload blocks, and the L1 form of "live ignored") is ported and
host-safe under `just verify`, reusing the existing `get`/`block`/`seek`. We **diverge** on byte
addressing (encoded bytes vs upstream's value bytes + `padding` — identical observable behaviour for a
framing-aware consumer) and **defer**: `live` tailing (async/networking), duplex-stream backpressure (Node
runtime), sub-block byte slicing of a non-boundary offset + `padding`, and the snapshot/session-bound
stream variants (`{ snapshot }` / atom — sessions/networking, out of scope). `move-to.js` (the move-to
operation) and the write-stream object remain. Soundness is unchanged — streams are read-only views over
already-authenticated blocks.

## ADR-0034 — Prologue migration is a content-addressed prefix commitment + by-value copy under a new key
**Context:** Upstream hypercore (`reference/js/hypercore/test/move-to.js`) migrates a log's history onto a
**new core** under a fresh keypair: the new core is created with a manifest whose `prologue` is `{ length,
hash }` (a commitment to a prefix of the old log), `core2.copyPrologue(core.state)` copies that prefix's
blocks/tree in, and `session.moveTo(core2, len)` / `snapshot.moveTo(...)` re-homes the writer (or snapshot)
onto it, emitting a `migrate` event; appends then continue under the new key. The prologue is one field of
the full **manifest** (`manifest.js` — multi-signer `quorum`/`signers`/`namespace`, the `Verifier`/
`multisig` machinery), which is hashed into the core's key, so the prefix is *self-authorizing* (the manifest
commitment is the authority — no per-head signature by the new key over the copied prefix). `moveTo`/`migrate`
are session-level operations.
**Decision:** Reimplement the **L1 behaviour-under-test** — migrate a log's prefix onto a new identity — as
a standalone, content-addressed primitive, not the manifest/multisig/session layer:
- A [`Prologue`] is just `{ length, hash }` — a commitment naming a prefix by its **Merkle hash**, carried
  on the core (not embedded in a multi-signer manifest). `Hypercore::prologue_at(length)` mints one from a
  source (`{ length, prefix_root_hash(length) }` ≡ upstream's `{ length: core.length, hash: core.state.hash() }`);
  `with_prologue(author, codec, store, prologue)` creates a fresh core bound to it under a *new* key.
- `copy_prologue(source)` adopts the committed prefix **by value** (the same divergence as the by-value
  snapshot, ADR-0032): it content-checks `source.prefix_root_hash(length) == hash` *before* copying (so a
  non-matching source leaves the core untouched), copies the prefix blocks in, rebuilds an identical prefix
  tree, marks them present, and **re-signs the prefix under the new key** (a head at `length`, fork 0).
  Re-signing — rather than upstream's manifest-self-authorization — keeps `verify_head`/`verify_block`/
  proofs uniform; it is observably equivalent (the new key's first real append already signs a head ⊇ the
  prefix, so signing at `length` just does it one step early). Because the commitment is content-addressed,
  `source` need **not** share the new key — any log holding the same prefix content backs it.
- `verify_prologue()` is the maintained invariant (`prefix_root_hash(length) == hash`), and the prologue
  length is a **`truncate` floor** (a prologue-bound core refuses to rewind into the committed prefix). The
  snapshot `moveTo` case reduces to a no-op at L1 — our by-value snapshot (ADR-0032) is already immune to any
  mutation of the source, so a snapshot taken before the migration keeps returning its own captured blocks.
**Consequence:** The `move-to.js` headline (migrate a prefix onto a new identity; the truncate-and-rewrite +
surviving-snapshot case) is ported and host-safe under `just verify`. `move-to.js` moves `[ ]`→`[~]`. We
**diverge** by copying the prefix by value (vs upstream's shared-storage `copyPrologue`) and by re-signing it
under the new key (vs manifest self-authorization) — identical observable behaviour. We **defer**: the full
multi-signer **manifest** + `Verifier`/`multisig` and the manifest-hash-into-key identity binding
(`manifest.js`, still `[ ]`); the session-level `moveTo`/`migrate` re-homing and the `createWriteStream`
object (sessions, out of scope); and the value-byte/`padding` concerns unchanged from ADR-0022. Soundness
rests on the same prefix-root collision-resistance the scheme already assumes (a forged prefix can't match
the committed hash) plus the new key's head signature.

## ADR-0035 — Multi-signer manifest verifier is an L1 quorum primitive in `identity`; wiring + multisig wire format deferred
**Context:** Upstream hypercore (`reference/js/hypercore/lib/{verifier,multisig,caps}.js`, exercised by
`test/manifest.js`) makes a log's authority a **manifest**: `{ version, hash, allowPatch, quorum,
signers: [{ publicKey, namespace, signature: 'ed25519' }], prologue, linked, userData }`. The manifest is
hashed (`hash(MANIFEST_cap || encode(manifest))`) into the core's **key**, so the signing policy is
self-authorizing — who may sign cannot change without changing the identity. `Verifier.fromManifest`
builds signers; `verify(batch, signature)` either short-circuits a **prologue** prefix (accept iff
`batch.length === prologue.length && batch.hash() === prologue.hash`, no signature) or runs the
**multisig** rule (`_verifyMulti`): inflate the signature into proofs, require `proofs.length >= quorum`,
each proof a *distinct* in-range signer, each signer's ed25519 signature valid over `batch.signable(ctx)`
where `ctx = manifestHash` (v1). A single-signer manifest's hash equals `Hypercore.key(publicKey)` (the
content-addressed identity of a plain one-author core). The surrounding machinery — the compact-encoding
wire format, v0 **compat** signers (`ctx = namespace`, legacy `signableCompat`), `allowPatch` cross-length
patch signing (`generateUpgrade`/`partialSignature` over the replication proof), `linked`/`userData`
manifest fields, and the session-level `multisig - append`/`patches` (which drive `core.replicate` /
`download`) — is wire format / disk compat / sessions / networking, out of scope per the relevance filter.
**Decision:** Reimplement the **L1 behaviour-under-test** — a content-addressed multi-signer quorum
policy — as a standalone primitive in `crates/identity` (`Manifest` + `Signer` + `PartialSig` +
`Prologue`), clean-room (ADR-0001), not the wire/compat/patch layer:
- A [`Signer`] is an ed25519 [`PublicKey`] + a 32-byte `namespace`; a [`Manifest`] commits to `quorum` +
  ordered signers (+ an optional [`Prologue`]). [`Manifest::hash`] is the **content-addressed identity**
  (domain-separated, length-bound blake3 over quorum/signers/prologue) — the would-be log key;
  [`Manifest::single`] is the one-author identity (`single(pk).hash()` is that core's key).
- To authorize a head `(length, tree_hash)` a signer signs [`Manifest::signable`] — domain-tagged bytes
  binding the **manifest hash** (the modern `ctx = manifestHash` path) alongside length + root (mirrors
  `caps.treeSignable`). [`Manifest::sign`] finds the matching declared signer or returns `None`
  ("public key is not a declared signer").
- [`Manifest::verify`] short-circuits a [`Prologue`] prefix on content alone (the manifest-level form of
  ADR-0034's hypercore prologue), else the **multisig quorum** rule: at least `quorum` signatures, each
  from a distinct in-range signer, each valid — any out-of-range/repeated signer or invalid supplied
  signature rejects the whole multisig. We require **all supplied** signatures valid+distinct (upstream
  only checks the first `quorum`); behaviourally identical for a distinct-valid quorum set, and strictly
  safer (a garbage extra proof can't ride along). Invalid configs (`quorum == 0`, `quorum > signers`,
  no signers) are rejected at construction (upstream `createManifest` throwing); a **non-ed25519** signer
  is *structurally* impossible (a [`Signer`] only holds an ed25519 key), so upstream's "unsupported curve"
  rejection is enforced by the type system.
**Consequence:** The `create verifier - *` family (static signer, single signer, multi signer, defaults /
content-addressed key, multisig distinctness/quorum, validation) is ported and host-safe under
`just verify`. `manifest.js` moves `[ ]`→`[~]`. We **diverge**: clean-room manifest hash (not upstream's
compact-encoding + blake2b), all-supplied-valid (vs first-`quorum`), and the namespace folds into the
manifest hash rather than being the v0 signing context (we implement only the modern v1 `ctx =
manifestHash` path). We **defer**: wiring the verifier into `Hypercore` (replacing the single-key
`SignedHead` with a manifest — the manifest-hash-into-key identity binding); the **compat (v0)** signer
path, `allowPatch` cross-length patch signing, and the `linked`/`userData` manifest fields; the multisig
**wire format** (`assemble`/`inflate` compact-encoding); and the session-level `moveTo`/`multisig -
append`/`patches` (sessions/networking). Soundness rests on ed25519 unforgeability + manifest-hash
collision-resistance (a signature is bound to the exact policy via the manifest hash in its signable).

## ADR-0036 — Manifest-authorized core is a focused new type (`ManifestCore`), not an in-place `Hypercore` refactor
**Context:** ADR-0035 deferred *wiring* the iter-32 `identity::Manifest` verifier into a hypercore-style
core — "replacing the single-key `SignedHead` with a `Manifest`" (the manifest-hash-into-key identity
binding + a quorum-authorized head). The single-key `Hypercore` is a 3057-line module with ~60 tests; an
in-place replacement changes the head's *signed bytes* (a head must bind the **manifest hash**, so a
single-author head signed over `head_message(fork,length,root)` no longer verifies under a manifest), which
cascades into ~60 call sites (`verify_block`/`conflicting_heads`/`ForkProof::verify`/`Replica`/`Snapshot` +
their tests, many of which are textually identical `verify_block(&pk, &head, i, &enc, &proof)` lines).
Doing that under a single green gate, in one single-writer step, is high-risk churn with no behavioural
gain for the single-signer case.
**Decision:** Deliver the **manifest-authorized core capability** as a focused, self-contained new type in
`crates/hypercore` (`crates/hypercore/src/manifest_core.rs`), leaving the single-key `Hypercore` untouched:
- [`ManifestCore<T, C, S>`] — an append-only log governed by a [`Manifest`] quorum. Its identity is the
  content-addressed [`Manifest::hash`] ([`key`]), each head `(length, root)` is signed by every locally-held
  declared signer (collected in signer-index order — deterministic), and [`verify_head`] passes iff those
  reach the manifest's quorum. A head here is at a single (implicit fork-0) history, so it carries **no fork
  field** (the fork counter + `truncate`/batch/snapshot/streams stay on the single-key `Hypercore`).
- [`verify_manifest_block`] — the multi-signer analogue of `verify_block`: a head meets the manifest quorum
  *and* the proof places `data` at `index` under the head's root.
- [`ManifestReplica<T, C, S>`] — verify-only, holding only the **public** [`Manifest`] (the policy); rebuilds
  a byte-identical log, trusting no sender.
`Manifest::single(pk)` makes `ManifestCore::key()` equal a plain one-author core's identity, so the
single-signer case is the special case; a true multi-signer (e.g. 2-of-3) core that holds < quorum secrets
produces an *unauthorized* head it cannot ratify alone.
**Consequence:** The manifest-hash-into-key binding + quorum-authorized head + manifest-keyed verify-only
replication (the L1 of `manifest.js`'s `multisig - append` shape, minus sessions/networking) are ported and
host-safe under `just verify`. `manifest.js` stays `[~]` (advances further). We **diverge** by adding a
parallel core type rather than retrofitting `Hypercore` in place; **unifying** the two (reframing `Hypercore`
as `ManifestCore` with `Manifest::single`, retiring the single-key `SignedHead`) is the remaining mechanical
follow-up, deferred. We still **defer** (from ADR-0035) the compat (v0) signer path, `allowPatch`
cross-length patch signing, the multisig wire format, and the session-level `moveTo`/`migrate`. The fork
counter is not yet on `ManifestCore` (no `truncate` here), so cross-fork equivocation lives only on the
single-key core until unification. Soundness rests on the same ed25519 + manifest-hash collision-resistance
ADR-0035 already assumes, plus the Merkle leaf binding `verify_block` rests on.

## ADR-0037 — `hyperbee` is a one-block-per-node, inline-KV B-tree (v1)
**Context:** Upstream `hyperbee` packs the rewritten leaf→root path into a single block's `YoloIndex`
(addressing nodes by `(seq, offset)`) and stores keys/values in the entry block, referenced from nodes by
`seq`. That is a block-count/storage optimization, not the ordered-KV semantics.
**Decision:** Reimplement the *behaviour* — a copy-on-write B-tree over our `hypercore` — with a simpler
format: **one block = one node** (a child pointer is just a `seq`); **key+value inline** in the node; **no
header block** (`version()` = block count, empty = 0); split at `MAX_CHILDREN = 9` (matches upstream). The
new root is always the latest block. v1 = `put`/`get`/`range` (asc+desc, gt/gte/lt/lte, limit).
**Consequence:** Faithful to the ordered-KV behaviour — the upstream `basic.js` exhaustive range oracle
(sizes 1..25 × {gt|gte}×{lt|lte}×reverse) passes and exercises multi-level splits. Trade-offs: a `put`
appends one block per path node (vs upstream's one), and a key's bytes are re-encoded when its node is
rewritten. **`del` + rebalance now implemented** (2026-06-30): `MIN_KEYS = (MAX_CHILDREN-1)/2 = 4`; a key
in an internal node is replaced by its in-order neighbour pulled from whichever boundary leaf has more
keys (upstream `setKeyToNearestLeaf`/`leafSize`), then nodes below `MIN_KEYS` borrow from a sibling with
`> MIN_KEYS` or merge, bottom-up, shrinking the root when it empties — a recursive COW analogue of
upstream's stack-based `rebalance`. A 404 delete appends nothing. **Deferred:** sub-databases, the
header/`isHyperbee` detection, and diff/history/watch.

## ADR-0038 — Browser persistence is OPFS sync-access-handle (verified in Chrome)
**Context:** Local-first means the browser is the writer and must persist its hypercores locally.
IndexedDB has **no synchronous API**, but our `Store` trait is synchronous. OPFS's
`FileSystemSyncAccessHandle` exposes synchronous read/write/getSize/truncate/flush, fitting the trait
with no async plumbing — the same primitive SQLite-WASM uses.
**Decision:** Ship `storage::opfs::OpfsStore` behind the `opfs` feature + `--cfg web_sys_unstable_apis`:
an async `open()` acquires the sync handle; the sync `Store` ops mirror an in-memory map to a single OPFS
file (re-serialize + rewrite per mutation — O(n), simple; a log-structured layout is a follow-up).
Worker-only (sync handles are unavailable on the main thread; tests use `run_in_dedicated_worker`).
**Consequence:** Verified end-to-end in **real headless Chrome** (`wasm-pack test`, dedicated worker):
put/get/overwrite/delete + **persistence across close+reopen**. Closes gate #2. `localStorage` (too small)
and async-IndexedDB (a trait-wide async refactor) were rejected. Deferred: a log-structured/compacting
layout; per-key files.

## ADR-0039 — Log-structured OPFS via a `SyncFile` abstraction
**Context:** The v1 OPFS store (ADR-0038) mirrored an in-memory map to one file and rewrote the *whole*
file on every mutation — O(n) write amplification.
**Decision:** Introduce a `SyncFile` trait (`size`/`read_at`/`write_at`/`truncate`/`flush`) and a
`LogStore<F: SyncFile>`: each mutation **appends** a `[key][kind][len][value]` record (O(1) amortized);
an in-memory index maps `key → (offset, len)`; a delete appends a tombstone; `open` replays the file to
rebuild the index (dropping a partial trailing record); `compact()` rewrites with only the live records
once dead bytes exceed half the file. OPFS becomes one `SyncFile` impl (`OpfsFile`, offset read/write via
the sync access handle); `OpfsStore = LogStore<OpfsFile>`.
**Consequence:** The log-structured KV + compaction logic is **tested natively** against an in-memory
`MemFile` (contract, persistence-across-reopen, compaction-reclaims-space, partial-tail-dropped) — no
browser needed — and the OPFS binding is re-verified in real headless Chrome. Supersedes ADR-0038's v1
whole-file rewrite. Deferred: compaction tuning; wiring OPFS persistence into bitfield/snapshots.

## ADR-0040 — (Speculative; NOT adopted, no work planned) Succinct-proof readiness
**Status:** Idea only, recorded so we don't foreclose it. Nothing here is built or scheduled.
**Observation:** A single-writer core is *already* light-verifiable — the signed head signs the Merkle
root, so "block N exists with this content-hash, authored by key K" is one signature check plus an
O(log n) inclusion path. What is **not** succinct is a multiwriter **linearized view**: trusting it today
means pulling every input log and replaying the deterministic fold (linearize → apply). That replay is a
natural candidate for **succinct verifiable computation** — a recursive SNARK / folding scheme (the fold
is a uniform repeated step, which suits IVC/folding à la Nova), or a zkVM run over the *existing*
deterministic Rust fold (minimal new code, high proving cost). The view could then be verified from
`{head, proof, writer public keys}` without replay.
**Guardrails (cheap, worth honouring now):**
- Keep the linearization/apply a **deterministic pure function** of the signed inputs (already a goal) —
  this is the precondition any such proof needs.
- Keep the **hash and signature behind traits**, so a future native-circuit path could swap to
  SNARK-friendly primitives without a rewrite (the current BLAKE3 + ed25519 are deliberately *not*
  circuit-friendly). No swap now.
**Non-goal:** privacy/ZK of op contents — attribution is meant to be public; the property of interest is
succinctness (the "S" in SNARK), not zero-knowledge.

## ADR-0041 — Reconstitute a core from storage: `persist`/`open` over reserved keys
**Context:** `Hypercore` already wrote block bytes to the `Store` (`store.put(index, bytes)`), but its
authenticated state — the Merkle tree, presence bitfield, signed head, fork, prologue — lived **only in
memory**, and there was no `open`. So a browser (OPFS) writer's bytes survived a reload but the core
itself could not be reconstituted: the local-first persistence story had no payoff.
**Decision:** Add `Hypercore::persist(&mut self)` and `Hypercore::open(author, codec, store)`.
- Serialization lives with each type (encapsulating its private layout): `merkle::MerkleTree::serialize`/
  `deserialize` (`[length][count]` + `[index][size][hash]` per node) and `storage::Bitfield::serialize`/
  `deserialize` (live non-zero pages only — an all-zero page ≡ absent, ADR-0030). `hypercore` adds a small
  metadata codec for `fork` + the optional `SignedHead` + optional `Prologue`.
- The single flat `u64→bytes` `Store` is shared with block keys (`0..length`), so metadata goes at three
  **reserved top keys**: `KEY_META = u64::MAX`, `KEY_TREE = MAX-1`, `KEY_PRESENCE = MAX-2`. A collision
  needs ~1.8e19 blocks — impossible in practice. (Upstream uses separate per-section files; this is the
  clean-room single-store equivalent.)
- The **secret key is never persisted** — it's the caller's keyring, passed back to `open`. `open`
  reconstructs the core and then runs `verify_head()`: the persisted head must be self-consistent with the
  persisted tree **and** signed by `author`'s key — so a wrong key, a mismatched tree/head, or tampered
  metadata fails with `Error::Corrupt`; an unpersisted store yields `Error::NotPersisted`.
**Consequence:** A core round-trips through its `Store` (incl. a **sparse** core: cleared blocks stay
authenticated — the persisted tree still produces a verifying proof though the bytes are gone). Native
tests cover full round-trip, sparse round-trip, wrong-key, unpersisted, and tampered-metadata. Deferred:
incremental/dirty-tracked persistence (today `persist` rewrites all three blobs); auto-persist hooks;
`ManifestCore` persistence.

## ADR-0042 — Faithful `consensus.js` port (staged); conservative `finalized()` stays the baseline
**Context:** ADR-0015 deferred the fork/merge competition + 2-degree-lead caveat, leaving `finalized()`
as the conservative snapshot / no-active-fork prefix. Revisiting it surfaced two facts. (1) The
conservative rule is not merely a stopgap — it is **safe and correct for a complete DAG**: when two fork
arms each reach a double quorum (DESIGN.md "Consistent Ordering" `a0`/`b0`, both verified degree 2), the
correct behaviour is to wait for the merge before locking, exactly what we do; the famous "writer `a`
locks `a0` with just `a1`" is a *local, pre-sync* view. (2) A naive "2-degree-lead" refinement is wrong
both ways: a plain degree ≥ 2 rule is **unsafe** (an unseen lower-keyed contender with its own double
quorum reorders an already-finalized node via the deterministic tiebreak), and "lead every incomparable
node by 2" is **too conservative** (fails the `a0` example). The genuinely-correct rule is upstream's
incremental confirmation machine, and DESIGN.md's "Tails, Forks and Merges" is itself `// todo`.
**Decision:** Port `consensus.js` **faithfully** — its *behaviour*, reimplemented over our DAG (clean-room
per CLAUDE.md, not the stateful BufferMap/Clock machine) — in committed green **stages**, keeping the
existing safe `finalized()` as the baseline until the new machine passes the worked DESIGN examples, the
existing quorum/finality tests, and the convergence sim (gate #3). Stages: **(1) vector clocks** over the
DAG — `Clock` + `Linearizer::clock` (DONE); (2) DAG predicates `_strictlyNewer` / `_acks` /
`_indexerTails` / `_isMerge`; (3) `confirms` / `_isConfirmed` / `_isConfirmableAt` / `_ackedAt`; (4) the
`shift` / `_yieldNext` driver yielding the confirmed prefix, then swap `finalized()`/`indexed_view` onto
it behind the same safety tests.
**Consequence:** Each stage is independently testable; safety is never regressed — the conservative prefix
stays live until the precise machine is proven at least as safe and strictly-or-equally as eager. Stage 1's
clock layer is the substrate every later predicate reads.

## ADR-0043 — Swapping `finalized()` onto the consensus machine is blocked on two pieces
**Context:** The staged `consensus.js` port (ADR-0042) is complete through `consensus.shift`:
`Linearizer::confirmed_prefix` drives it from scratch, reproduces upstream's confirmation — including
committing a merge-resolved fork arm the conservative `finalized()` defers — and converges across delivery
orders. Validating it against the convergence sim's 5-writer / 3-indexer **partitioned** DAGs surfaced two
gaps that block making it the live finalization.
**Findings.** (1) **`consensus.shift` confirms only *indexer* nodes.** Upstream weaves the non-indexer
nodes into each indexed batch via `linearizer.js` `_yield` → `Topolist.add`; that is not yet ported, so
`confirmed_prefix` omits a confirmed indexer node's non-indexer dependencies — it is causally closed only
*among indexer nodes*, not the full indexed view. (All-indexer unit DAGs hid this; the sim caught it.)
(2) **The consensus yield order ≠ our `order()` tiebreak.** `order()` is an independent priority-Kahn with
a lowest-writer-key tiebreak (ADR-0014, chosen for *manifest* determinism); the consensus machine yields an
arm once its quorum forms, picking a different concurrent node first. Both are valid causal linearizations,
but the confirmed set is therefore **not a contiguous prefix of `order()`** — so `finalized() =
confirmed_prefix()` would violate the `order ⊑ finalized` contract the sim enforces.
**Decision:** Keep `finalized()`/`indexed_view` as the **order-aligned conservative prefix** (safe, an
`order()` prefix) for now; ship `confirmed_prefix` as a **validated standalone** precise-confirmation API
documenting both gaps. The swap requires (a) porting `_yield` (non-indexer interleaving) and (b)
**reconciling `order()` with the consensus order** — a foundational call against ADR-0014: either align
`order()`'s confirmed prefix to the consensus/topolist order (so the indexed view is a true prefix), or
define the indexed view independently of the key-tiebreak `order()`. Deferred pending that decision.
**Consequence:** No safety regression — the conservative prefix stays live. The consensus *core* is ported,
proven, and converges; only the view assembly (`_yield`) + the ordering reconciliation remain.

## ADR-0044 — Consensus swap done: `finalized()`/`order()` are now the precise machine
**Context:** ADR-0043's two blockers, resolved. (1) **`_yield` ported** as `confirmed_view` — each `shift`
batch expanded to its newly-covered causal closure (non-indexer nodes included) and emitted in key-tiebreak
topo order. (2) **Order reconciliation:** the discovery that upstream's `Topolist.cmpUnlinked` uses the
*same* lowest-key tiebreak as our `order()` made alignment clean — `order()` = `confirmed_view()` ++
`topo_key_order(remaining)`, so `finalized()` (= `confirmed_view()`) is a true prefix by construction.
**Decision:** Swap the live finalization onto the faithful machine: `finalized()` / `indexed_view` =
`confirmed_view()`; `order()` puts the confirmed view first; `quorum_degree` now uses a private
`plain_order()` (consensus-agnostic topo) to stay independent. The old conservative rule is kept as a
**test oracle** (`conservative_finalized`) asserting the precise machine is never *less* eager.
**Validation:** full suite + convergence sim green — `order ⊑ finalized` by construction, finalized
**converges** across delivery orders (partitioned DAGs), and is **monotone** under cooperative growth.
**Honest scope (the remaining gap):** the from-scratch `confirmed_view` is convergent always and
order-stable for **cooperative** growth (the regime ADR-0016 already scoped, and the federated-homeserver
norm). It does **not** guarantee order-stability under *adversarial* partitions — where a lower-key node
concurrently confirmed *later* could re-sort into an earlier batch — because we did not port upstream's
incremental **flush-permanence** (`Topolist` `undo`/`mark`, which freezes a once-published indexed prefix).
The *set* of confirmed nodes is always monotone (consensus guarantees it); only intra-prefix *order*
under adversarial partition is unguaranteed. Porting flush-permanence (so a published checkpoint never
reorders even adversarially) is the one remaining refinement — deferred; not needed for the cooperative
federated regime. (Trade vs. the old conservative rule: it was unconditionally order-stable but far less
eager — it only committed fork-free prefixes; the precise machine confirms fork-merges.)

## ADR-0045 — `roomnet`: scope the Iroh room layer in as a separate crate

**Status:** adopted. Amends ADR-0003 ("networking out of scope, deferred to Iroh"): the Iroh
layer is now **in scope**, but confined to a dedicated crate (`crates/roomnet`) so the L1 crates
(hypercore/autobase/storage/merkle/identity/codec) stay pure, transport-free, and `wasm32`-clean.

**What:** `roomnet` is a pluggable room-replication layer. See `docs/ROOMNET_SPEC.md`. A **`Room`**
(Tier 1) is a sans-IO state machine: one local writer `Hypercore` + a `Replica` per remote writer,
driving the autobase `Linearizer`, maintaining a rolling **finalized** projection (authoritative →
Lane 3 sink) and a **live** projection (optimistic → render). A **`RoomServer`** (Tier 2, native)
owns many rooms and replicates remote ones on demand. Three pluggable seams — `Transport`,
`StoreFactory`/`Store`, `ProjectionSink` — plus the portable `Projection` fold (the one place domain
logic lives). Wire protocol `SyncMessage` (`Head`/`Have`/`Want`/`Block`) carries self-verifying,
Merkle-proofed blocks; `wire::{encode,decode}` serializes it with the `codec` varints (no serde on
the L1 types). `Entry { heads, payload }` is the log-entry *content* (opaque to L1).

**Iroh (feature `iroh`, default-off):** `IrohTransport` binds a QUIC endpoint (ALPN + net params
configurable via `IrohConfig`, all defaulted) and ships `[RoomId | wire]` frames; `run_server` is
the tokio driver. A node's ed25519 seed is simultaneously its autobase `WriterKey` and its iroh
`EndpointId`, so peers dial by writer key. Kept behind a feature flag so the default build stays
wasm-clean and dependency-light.

**iroh version:** `iroh = "1"` (1.0.1). iroh 1.x pins its own crypto tree (`ed25519-dalek
=3.0.0-rc.0`), which resolves against the released `ed25519`/`signature`/`pkcs8` — so **no manual
crypto pins are needed** (an earlier 0.97 cut required RC pins for `ed25519-dalek 3.0.0-pre.1`; the
1.0 bump removed them, and the 0.97→1.0 transport API was source-compatible — no code changes).
**MSRV:** the `iroh` feature needs **rustc ≥ 1.91** (iroh 1.x MSRV); the default (wasm-clean) build
has no such requirement, so the standard `just verify` gate stays on the project nightly.

**Scope held out (follow-ons):** cross-writer resume-from-checkpoint (needs a persisted system/view
core); relay of a replicated writer's blocks (needs `Replica::{block,proof}` in L1); discovery
beyond `bootstrap`; the monorepo-side TerminusDB adapters and the `lib/oplog` → `SongProjection`
migration (kept out of the public submodule). Tests: 11 native (Room convergence/finality/in-order
recovery, RoomServer on-demand replication, stale-GC, wire round-trip) + wasm build of the `Room`
core + a compiling iroh transport & `chat_room` example.

## ADR-0046 — Durable resume: Replica persist/open + on-disk store + Room replay

**Status:** adopted. A roomnet `Room` derived its DAG + projections in memory and, on restart, kept
only the local writer's own signed history — remote writers had to be **re-synced from peers**. This
makes a room resume its full state from **local storage alone**.

**What:**
- **L1 `Replica::persist`/`open`** (`crates/hypercore/src/replica.rs`), mirroring `Hypercore`'s:
  `persist` writes the Merkle tree + the (optional) verified head under the reserved
  `KEY_TREE`/`KEY_META` keys (block bytes are already written by `add_block`; a replica never clears,
  so there is no presence map). `open` deserializes, reconstructs, and re-verifies a present head
  against the writer's public key (`Corrupt` on mismatch, `NotPersisted` if absent).
- **Native on-disk store** (`crates/storage`): `StdFile` — a `SyncFile` over `std::fs` (Unix
  positional `pread`/`pwrite`, `fsync` on flush). `LogStore<StdFile>` is therefore a real, crash-safe
  on-disk `Store` (log-structured append + replay + compaction already existed). This closes the
  "hypercore data is in-mem-only" gap: a node's logs are written to disk.
- **roomnet**: `StoreFactory` gains `known_writers()` and a fallible `open` (`type OpenError`);
  `DiskStoreFactory` stores each writer's log as `hex(writer)` under a per-room directory (the real
  persistence path); `MemStoreFactory` stays ephemeral. `Room` persists the local `Hypercore` after
  each append and each remote `Replica` after each accepted block. `Room::open` became fallible and
  **resumes**: reopen the local core (`Hypercore::open`, fallback `new`) + every `known_writer` as a
  `Replica::open`, **replay** all entries into the linearizer (per-writer in order; cross-writer
  causal deps buffer via `pending`), one `advance()` rebuilds the projections, and the finalized
  delta queue is cleared (those versions were persisted before the restart — not re-emitted to Lane 3).
  Because `order()`/`finalized()` are pure functions of the DAG, the reopened room is identical.

**Validation:** `hypercore` — Replica persist→open round-trip (+ wrong-key ⇒ `Corrupt`, unpersisted
⇒ `NotPersisted`); `storage` — `LogStore<StdFile>` upholds the Store contract and persists across a
reopen on a real file; `roomnet` — two-writer and single-writer rooms resume from a `DiskStoreFactory`
directory with **zero network**, matching the pre-restart `order`/`finalized`/`live`/`finalized_len`
and emitting no finalized deltas on resume.

**Scope held out (next increment):** resume-by-replay requires every finalized entry's bytes to be
present — a room that `sweep`s (GC's live block bytes) needs a persisted projection **checkpoint**
`(state, finalized_len)` instead of replay. Persist is per-op (a crash-safe WAL); batched/debounced
persist is a later optimization. `RoomServer` still constructs in-mem (`Default`) factories — wiring
it to per-room disk directories rides with the `services/node` cutover (still out of scope).
