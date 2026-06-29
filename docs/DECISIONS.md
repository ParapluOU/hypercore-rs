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
