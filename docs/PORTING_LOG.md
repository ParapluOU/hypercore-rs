# Porting log

Append-only. Newest at the bottom. One entry per iteration: **what / decisions / lessons / next**.
Repo-relative paths only — no private or personal data (this repo is public).

---

## 2026-06-29 — Iteration 0: scaffold + loop

**Did**
- Created the Cargo workspace and empty crates (`codec`, `merkle`, `identity`, `storage`,
  `hypercore`, `autobase`, `hyperbee`) — doc comments only, no data types. `cargo check` green.
- Vendored upstream as read-only submodules under `reference/` (datrs Rust port; Holepunch JS
  `hypercore`/`autobase`/`hyperbee`).
- Established the loop: `CLAUDE.md`, `docs/DEFINITION_OF_DONE.md`, `docs/UPSTREAM_TEST_MAP.md`,
  `docs/DECISIONS.md`, `docs/LESSONS.md`, this log, and `Justfile`.

**Decisions** (see `docs/DECISIONS.md`)
- Clean-room (not verbatim); networking deferred to Iroh; monorepo; WASM-first; in-repo logs;
  generic-only convergence sim; include the JS algorithmic-equivalence oracle; port relevant
  upstream tests; never commit private/personal data.

**Lessons**
- We have something verovio did not: upstream **test suites** to port as behavioural specs —
  enumerated in `docs/UPSTREAM_TEST_MAP.md`. This is the closest thing to a deterministic oracle.

**Next**
- First red item: `merkle` (tree + inclusion/range proofs + tamper-rejection), porting
  `reference/js/hypercore/test/merkle-tree.js`. Then `codec` round-trip, then the `autobase`
  linearizer against `reference/js/autobase/test/linearizer.js` + `dags.js`.

---

## 2026-06-29 — Iteration 1: `merkle`

**Did**
- Implemented `crates/merkle`: flat-tree index math (depth/offset/index/parent/sibling/children/
  full_roots), `append` with parent roll-up, multi-root `roots()`/`root_hash()`, inclusion
  `proof()` and `Proof::verify()`. BLAKE3, domain-separated + length-bound (leaf `0x00` / parent
  `0x01` / tree `0x02`; every node binds its byte size).
- 5 asserting tests: roots shape, proof round-trip over sizes 1..=33, proof-carries-siblings,
  determinism, tamper-rejection (bad data / same-length bytes / sibling / root / expected-root).
- `just verify` green: workspace tests + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions**
- Clean-room hashing: BLAKE3 with explicit domain prefixes + length binding; not byte-compatible
  with upstream BLAKE2b (per ADR-0001/0002).

**Lessons**
- Port the flat-tree algorithm first — it's small and self-contained, and proofs fall out of it.
- The reference-spec agent's diagram was muddled (claimed root 15 for 4 blocks); the canonical
  mafintosh flat-tree gives root index 3. Trust the algorithm; verify shapes by hand.

**Next**
- `merkle` range proofs (multi-block) to fully close the merkle DoD item.
- `codec`: round-trip + versioned/tolerant decode (port hypercore `encodings.js`).

---

## 2026-06-29 — Iteration 2: `codec`

**Did**
- Implemented `crates/codec` (dependency-free): LEB128 `varint`, version `frame`/`unframe`,
  length-skippable `write_tagged`/`read_tagged`, and a `Codec<T>` trait (separate from `T`) with
  built-in `U64` and `Bytes` codecs.
- 8 asserting tests: varint round-trip + truncation-EOF, built-in round-trip, trailing-byte
  tolerance, version framing, unknown-variant skip, and schema evolution both directions (old bytes
  under a newer schema; newer bytes tolerated by an older reader). `just verify` green.

**Decisions**
- `Codec<T>` is a separate encoder type (matches `Hypercore<T, C: Codec<T>>`), not a self-encoding
  trait — one type can carry different wire formats; storage/ordering stay content-blind.
- Dependency-free (no serde): deterministic bytes for content-addressing, trivially wasm-safe.

**Lessons**
- "Tolerant" = explicit lengths + ignore-trailing + default-on-EOF for newer fields. The
  length-delimited tagged frame is what lets a reader skip unknown variants without losing the stream.

**Next**
- `identity`: ed25519 keygen / sign / verify + forgery-rejection (maps onto an Iroh `NodeId`).

---

## 2026-06-29 — Iteration 3: `identity`

**Did**
- Implemented `crates/identity` on `ed25519-dalek` v2: `SecretKey::from_seed` (deterministic,
  RNG-free → wasm-safe), `sign`, `PublicKey::verify` (author id; maps to an Iroh `NodeId`), and
  byte round-trips for keys/sigs.
- 4 asserting tests: sign→verify, forgery-rejection (wrong msg / wrong key / tampered sig),
  determinism, public-key byte round-trip. `just verify` green — incl. the wasm build of
  `hypercore` pulling ed25519/curve25519 for `wasm32`.

**Decisions**
- Keys derive from a 32-byte seed; no `rand`/`getrandom`/OsRng in the build path → wasm builds
  cleanly and tests stay deterministic. The host supplies entropy for real keys.

**Lessons**
- ed25519-dalek v2 + curve25519-dalek build for `wasm32-unknown-unknown` out of the box, *as long
  as* you avoid the `rand_core`/OsRng path (use `from_bytes`/`from_seed`).

**Next**
- `storage`: byte-storage trait + in-memory backend (random-access read / write / len / truncate).

---

## 2026-06-29 — Iteration 4: `storage`

**Did**
- Implemented `crates/storage`: a `u64`-keyed `Store` trait (put / get / delete / len / contains),
  a `MemoryStore` backend (BTreeMap, `Error = Infallible`), and a reusable `contract::run<S: Store>`
  that any backend must pass. 2 asserting tests; `just verify` green.

**Decisions**
- `Store` is a synchronous `u64`-keyed KV (blocks and tree nodes addressed by index).
- The browser backend is **deferred** and intentionally reordered after `hypercore`: IndexedDB is
  *async*, so it needs either an async backend trait or a synchronous `localStorage` backend, and it
  can only be runtime-tested in a browser (a `verify-full` gate). `hypercore` is the centerpiece and
  fully testable natively, so it goes next to keep every iteration green-by-real-test.

**Lessons**
- A shared `contract::run` is the concrete enforcement of "same contract upheld by every backend" —
  the memory test and the future browser test call the exact same assertions.

**Next**
- `hypercore`: the typed, signed, append-only log — `append`/`get`/`verify` over `codec` + `merkle`
  + `identity` + `storage` (port `basic.js` / `core.js` behaviours). Then proof-based replication.

---

## 2026-06-29 — Iteration 5: `hypercore`

**Did**
- Implemented `Hypercore<T, C: Codec<T>, S: Store>`: `append` (encode → store → merkle → sign the
  new head), `get` (decode), `block` (raw encoded bytes), `proof`, `verify_head`, and a free
  `verify_block(public, head, index, enc, proof)` so any holder of the author's public key can
  confirm a block belongs to the log. 5 asserting tests; `just verify` green.

**Decisions**
- The Merkle tree commits to the **codec-encoded** bytes (what's stored). Verifiers check encoded
  bytes against the signed head, then decode — decode is strictly post-verification.
- The head signs `(length, root)` under a domain tag; ed25519 determinism makes the whole log
  reproducible for a fixed author + appends.

**Lessons**
- Real bug caught by the gate: feeding the *raw value* (not the encoded block) to `verify_block`
  fails, because the tree hashes encoded blocks. The proven unit is the encoded block. Fixed within
  the iteration by adding `block()` and verifying encoded bytes — exactly the "else, fix" path.

**Next**
- `hypercore` replication: a verify-only `Replica` that, given the signed head + per-block proofs,
  accepts blocks and ends **byte-identical** to the source (the DoD replication property).
- Then the `autobase` linearizer (causal DAG order + deterministic tiebreak).

---

## 2026-06-29 — Iteration 6: `hypercore` replication

**Did**
- Added `Replica<T, C, S>` (verify-only, holds no secret key): `add_block` verifies each block
  against the signed head + Merkle proof, appends strictly in index order, and **rejects** bad or
  out-of-order blocks without storing them. A fully-replicated replica is byte-identical to the
  source — same root, same decoded values. 2 new tests (7 total in `hypercore`); `just verify` green.

**Decisions**
- Blocks apply in strict index order, each verified against the final signed head's root; the
  verified head is recorded once length + root match. The sender is never trusted.

**Lessons**
- "Byte-identical replication" falls out for free once both sides hash the same encoded blocks in
  the same order — the (proof, signed-head) pair is all the replica needs.

**Next**
- `autobase`: the causal linearizer — DAG order + deterministic tiebreak (port `linearizer.js` +
  `dags.js` basics). Then quorum/finality and the generic convergence sim.

---

## 2026-06-29 — Iteration 7: `autobase` linearizer (causal order + tiebreak)

**Did**
- Implemented `crates/autobase`: a `Linearizer` over a causal DAG of `NodeId { key, seq }`. `add`
  enforces causal delivery (rejects duplicate / seq-gap / dangling head) and auto-adds the
  same-writer predecessor as a dependency; `order()` returns the deterministic linearization via a
  **priority-Kahn topological sort** — emit the smallest causally-ready `NodeId` each step. Lowest
  writer key first, then seq ("lowest key wins"); never a timestamp, never a payload peek.
- 6 asserting tests: the three canonical DESIGN.md DAGs (linear chain; branch tiebreak; the recursive
  `[a0, c0, a1, b0, b1, c1, b2]`), determinism across three causally-valid arrival orders,
  causal-respect over every edge, causal-delivery rejection (duplicate/gap/missing-head with no
  partial commit), and empty. `just verify` green incl. the wasm build of `autobase`.
- Trimmed `autobase`'s `hypercore`/`identity` deps: the linearizer is pure L1 ordering over opaque
  writer keys (they return with quorum/view materialization).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0014: priority-Kahn instead of upstream's incremental `topolist` tip. We port the *ordering*
  from `DESIGN.md`, not the `undo`/`shared` streaming-patch bookkeeping; quorum confirmation is the
  next capability, so the `linearizer.js`/`dags.js` *indexed-view-length* assertions stay deferred
  (those rows are now `[~]`).

**Lessons**
- The definition of the order lives in `DESIGN.md`; `topolist.js` is an optimized view-patcher.
- Folding the tiebreak into `NodeId`'s `Ord` makes a `BTreeSet` frontier the tiebreak itself —
  determinism becomes structural, immune to arrival order. (Both moved to `docs/LESSONS.md`.)

**Next**
- `autobase` quorum / finality-stability: count distinct-writer votes over a node (causal closure),
  define the double-quorum confirmation rule from `DESIGN.md`, and assert a quorum-finalized prefix
  never reorders. Then the generic convergence sim (gate #3) and the JS oracle (gate #4) on top of
  `order()`.

---

## 2026-06-29 — Iteration 8: `autobase` quorum + finalized prefix

**Did**
- Added **indexer quorum** to the `Linearizer`. `with_indexers(..)` designates the voting writers;
  `sees(a, b)` is causal reachability (the graph equivalent of upstream `clock.includes`).
  `quorum_degree(target)` implements the `DESIGN.md` recursion — a node has a degree-1 (single)
  quorum once a majority of indexers reference it, degree-2 (double) once a majority reference *that*
  quorum, etc. — via one bottom-up pass over the topo order carrying best-degree-per-indexer.
  `finalized()` returns the conservative **snapshot/no-active-fork** prefix: the maximal prefix of
  `order()` whose nodes have a double quorum **and** are comparable to every other node.
- 8 new asserting tests (14 total in `autobase`): quorum degrees match the `DESIGN.md` 1'/2'/3'
  quorum chain; the `c0-b0-c1` higher-quorum example; the conflicting single-quorum pair that must
  **not** finalize; finalized = double-quorum prefix and is always a prefix of `order()`;
  **finality-stability** (the finalized prefix only ever extends under cooperative growth, never
  reorders); majority scales with indexer count (3-of-4); no-indexers ⇒ no finalization but ordering
  intact; a non-indexing writer is ordered but never votes. `just verify` green (40 workspace tests +
  wasm build of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0015: quorum is a recompute-from-scratch *degree* (the `DESIGN.md` definition), not upstream's
  incremental `consensus.js` vector-clock machine — determinism stays manifest. Finalization is the
  conservative snapshot form (double quorum + comparable-to-all); the fork/merge competition rule and
  the **2-degree-lead caveat** (`DESIGN.md` "Tails, Forks and Merges // todo") plus view
  materialization (`getIndexedViewLength`) are deferred. Hence the quorum DoD box and the
  `linearizer.js`/`dags.js` rows stay `[~]`.

**Lessons** (moved to `docs/LESSONS.md`)
- The quorum *degree* is a clean bottom-up recursion; the one subtlety is the author's self-vote at
  every level up to its own degree (sound because you only test level `k-1` after the degree is
  confirmed `≥ k-1`). Verify against the `DESIGN.md` 1'/2'/3' chain by hand.
- A double quorum alone is **not** safe to finalize past a competing fork (the `DESIGN.md` caveat),
  and the general rule is the whole consensus algorithm — so finalize only the snapshot/no-active-fork
  prefix (always safe) and assert finality-*stability* as a property; defer the lead caveat to the
  oracle iteration.

**Next**
- Strengthen quorum for **forks/merges**: confirm a merge over competing tails and the 2-degree-lead
  caveat (`consensus.js` `_isConfirmed`/`_isConfirmableAt`), so `finalized()` advances through
  resolved forks, not just chains.
- Then the generic **convergence sim** (gate #3): N writers, seeded random causal visibility; assert
  convergence + finalized-prefix-never-reorders. Then the **JS oracle** (gate #4) on `order()` +
  `quorum_degree`.

---

## 2026-06-29 — Iteration 9: convergence simulation (gate #3)

**Did**
- Closed **integration gate #3**: `crates/autobase/tests/convergence.rs`, a clean-room reimplementation
  of `reference/js/autobase/test/fuzz/` — host-safe and dependency-free (own seeded **SplitMix64** PRNG;
  no `rand`/`getrandom`, no `Math.random`). Two random-DAG generators: **partitioned** (the upstream
  `createDag` subset-of-tails model — forks/merges/reordering) and **cooperative** (each node references
  *all* current tails ⇒ a total order). Delivery-order variety comes from a **randomized-Kahn** topo sort.
- Two asserting tests: (1) over 16 partitioned seeds (mixed sparse/dense reference density), replaying
  the same node set through 4 distinct causally-valid delivery orders yields identical `order()`, an
  identical generic **state fold** (rolling FNV checksum over `NodeId`s — no domain types), and identical
  `finalized()`, plus per-edge causal-respect; a non-vacuity guard asserts the dense seeds actually
  finalize *something*. (2) over 8 cooperative seeds, incremental creation-order delivery keeps the
  finalized prefix **monotone** (`starts_with` the previous) and ⊑ `order()` at every step, and a long
  run finalizes a non-empty prefix. `just verify` green (42 native tests + wasm build).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0016: reimplement the fuzz *behaviour*, not the harness — seeded PRNG **is** the repro (replacing
  upstream's "format a failing DAG to a JS file"); skip `rollBack`'s node-deletion re-derivation and the
  deadlock/JS-formatting plumbing (test-runner concerns). Crucially, **finality monotonicity is asserted
  only under cooperative growth**: the conservative `finalized()` (ADR-0015, comparable-to-every-node) can
  legitimately *shrink* under arbitrary partitions when a late concurrent node strands a previously-
  finalized node — that is the deferred fork/merge gap, not a bug — so partitioned DAGs assert only
  convergence (a pure function of the node set always agrees).

**Lessons** (moved to `docs/LESSONS.md`)
- A fuzz/convergence sim needs a *seeded* PRNG (a 5-line SplitMix64, no deps) so a failure reproduces
  forever; drive delivery variety with randomized-Kahn (every output is a valid causal order).
- Convergence is a pure function of the node set ⇒ holds under arbitrary partitions; the conservative
  *finality* prefix is monotone only under cooperative growth — assert each where it actually holds.
- L1 "application state" in a domain-agnostic sim is just an order-sensitive checksum of the `NodeId`s.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008): feed the same random DAGs (reuse this sim's
  seeded generator) to `reference/js/autobase`'s linearizer **inside `scripts/node-sandbox.sh`** and to
  ours; assert identical `order()` (and ideally `quorum_degree`). This is also the cross-check that lets
  us safely strengthen `finalized()` for **forks/merges** + the 2-degree-lead caveat (ADR-0015).
- Then the remaining wasm runtime / IndexedDB gate (#2) and the per-file upstream test rows.

---

## 2026-06-29 — Iteration 10: `merkle` range proofs

**Did**
- Closed the **#1 DoD property row** (`merkle` — tree + inclusion/**range** proofs + tamper-rejection):
  added `MerkleTree::range_proof(start, end)` and `RangeProof::verify(blocks, expected_root)` — the
  contiguous multi-block generalization of the existing single-block `proof`/`Proof::verify`. A range
  proof carries only the **off-range boundary** sibling nodes (any depth) needed to roll the range's
  leaves up to the roots, plus all roots. Generator and verifier share one **depth-by-depth climb** over
  a frontier `BTreeSet`: at each level two in-range/derived siblings pair into a parent for free, an
  off-range sibling is supplied; the verifier recomputes every on-range node from the block data and
  consults the (untrusted) boundary table *strictly* as off-path siblings (preferring a recomputed node
  by index), so a forged boundary node can never impersonate a leaf's ancestor. Every recomputed leaf is
  force-climbed to a genuine root index (a missing sibling ⇒ rejection), the recomputed roots are
  substituted, and `tree_hash` must equal `expected_root`.
- 6 asserting tests (merkle 5→11): every contiguous sub-range over sizes 1..=20 round-trips; full-tree
  range recomputes every root and needs **zero** boundary nodes; a range spanning multiple roots needs
  boundary nodes and still verifies; a single-block range carries exactly the inclusion proof's sibling
  set; out-of-range/empty/inverted ⇒ `None`; tamper-rejection across the span (any mutated block,
  reordered blocks, wrong block count, tampered boundary node, tampered **untouched** root, wrong expected
  root, dropped boundary node). `just verify` green (48 native tests + wasm build).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0017: implement one **contiguous-range inclusion proof** (off-path-only boundary nodes via a
  deterministic depth-climb) rather than upstream hypercore's block + **upgrade** + **seek** proof triplet
  (`block.nodes` / `upgrade.nodes` / `additionalNodes`). The DoD asks for *range* proofs; length-extension
  (upgrade), byte-offset seek, and reorg/recovery stay deferred and continue to be tracked on the
  `merkle-tree.js` / `merkle-tree-recovery.js` upstream rows.

**Lessons** (moved to `docs/LESSONS.md`)
- Multi-leaf Merkle proof soundness = keep path nodes and sibling nodes in separate roles: recompute
  every on-path node, treat supplied nodes purely as off-path siblings, and force every recomputed leaf
  up to a real root (missing sibling ⇒ reject). Otherwise a prover can hand you the real roots plus bogus
  data that never gets connected to them.
- A *touched* (substituted) root's proof-supplied hash is irrelevant — it is overwritten by the recomputed
  node — so a tamper-rejection test must mutate an **untouched** root (or a block / boundary node).

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) on the linearizer, via `scripts/node-sandbox.sh`
  (a container runtime is available as Apple `container`; the reference linearizer pulls `b4a`/`nanoassert`
  + local `clock`/`consensus`/`topolist`, so the harness reconstructs `add`/`order` against it).
- Then the wasm runtime / IndexedDB gate (#2) and the per-file upstream test rows (incl. merkle
  seek/upgrade/reorg).

---

## 2026-06-29 — Iteration 11: `hypercore` batch + atomic append

**Did**
- Added **batch / atomic append** to `crates/hypercore` (upstream `batch.js` / `atomic.js` essence,
  L1): a `Batch<T>` opened with `Hypercore::batch()` records the log length it was opened against
  (`base`); `stage(&mut batch, &value)` encodes and buffers a block **without touching the log**;
  `batch_get(&batch, i)` reads *through* the batch (committed region from the log, staged region from
  the buffer); `commit(batch)` applies every staged block under a **single** signed head. Commit is
  **all-or-nothing**: blocks are written to storage first and, on any storage failure, the partial
  writes are rolled back and the Merkle tree + signed head are left untouched (the log never advances
  on a failed commit). Commit returns `Ok(None)` (log unchanged) on a **stale base** — the log
  advanced past `base` since the batch opened — and an empty batch is a successful no-op.
- 7 asserting tests (hypercore 7→14): staging leaves the log + head untouched while the batch reads
  both regions and reports `length()` = base+staged; **commit-equivalence** (a committed batch yields
  a head — root/length/signature — *identical* to N single appends under the same author); a committed
  batch is invisible to verifiers (every block proves against the one head; a `Replica` rebuilds it
  byte-identically); **stale-base rejection** (direct append during an open batch ⇒ commit refused,
  log unchanged); empty-batch no-op; dropped-batch leaves the log unchanged; and **commit atomicity**
  via a `FaultyStore` that injects a `put` failure mid-batch — commit errors, the partial write is
  rolled back (no orphan blocks), length/head/reads are intact, and a later fault-free commit recovers
  cleanly. `just verify` green (55 native tests + wasm build of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0018: model a batch as a **staged encoded buffer + atomic commit with stale-base rejection**,
  not upstream's session/`atom`/storage-overlay machinery (sessions are out of scope per the relevance
  filter). We port the L1 behaviour-under-test — stage-without-touching / single-head commit /
  all-or-nothing / stale-base reject — and defer upstream's multi-session interactions, `byteLength`,
  truncate/append events, and the `atom.flush()` storage-overlay model.
- The **JS oracle (gate #4) is environment-blocked this iteration**, so I picked the next self-contained
  red item instead: the Apple `container` runtime is installed but its system service is **not started**
  (`container system start` needs an XPC service that is outside the iteration's scoped allowlist), and
  the image pull needs network — so a green oracle run isn't reachable under the loop's permissions here.
  Separately, an order-equivalence oracle could legitimately come back **red** (our priority-Kahn
  `order()` vs upstream's incremental `topolist.js` insertion-sort), which is precisely the divergence it
  exists to surface and can't be resolved without it. The oracle stays the top "Next".

**Lessons** (moved to `docs/LESSONS.md`)
- Atomic multi-step commit over a fallible byte store: do the **fallible writes first** (rolling back
  on failure), and only mutate the in-memory source of truth (Merkle tree + signed head) **after** every
  write has succeeded — so a partial failure can never advance the log's logical state.
- Test atomicity with a **fault-injecting store** wrapper (fail the `put` at a chosen key); assert the
  logical state (length/head/reads) is untouched *and* no orphan blocks remain, then that a fault-free
  retry recovers — a happy-path-only test would never exercise the rollback.
- The minimal-dependency path to the **JS oracle** is upstream's bare `lib/topolist.js` (the actual
  ordering producer, ADR-0014): it needs only `b4a.compare`/`nanoassert` and synthetic node objects
  (`writer.core.key`, `length`, `dependencies`/`dependents`, `index`) — *no* clock/consensus/writer
  graph and none of the heavy native deps. Inject the two trivial deps via `Module._compile` over the
  reference source (no npm, no network) and drive `Topolist.add` in causal order, comparing `.tip` to
  our `order()`. Precondition: a **started** container runtime (`container system start`).

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) once a container runtime is *started*:
  build `tools/oracle/` driving the reference `lib/topolist.js` (deps injected via `Module._compile`,
  network-free) through `scripts/node-sandbox.sh`, feed it the convergence sim's seeded DAGs, and assert
  identical order vs our `order()` (a `--ignored`, `oracle`-feature test, run by `just oracle`). This is
  also the cross-check that lets us safely strengthen `finalized()` for forks/merges (ADR-0015).
- Then the wasm runtime / IndexedDB gate (#2), and more upstream rows (`conflicts.js` fork detection;
  merkle seek/upgrade/reorg).

---

## 2026-06-29 — Iteration 12: `hypercore` fork detection (`conflicts.js`)

**Did**
- Added **fork detection** to `crates/hypercore` — the L1 behaviour behind upstream `conflicts.js`,
  with no networking and no events (two self-contained, content-blind primitives over the existing
  signed head + Merkle inclusion proof + identity):
  - `conflicting_heads(public, a, b)` — **proof-free**: two heads of *equal length but different
    root*, each verifying under the author's key, are a fork (the head at a length is a deterministic
    pure function of the first `length` blocks ⇒ two roots at one length ⇒ two histories). How a
    verifier first *notices* a fork.
  - `ForkProof { index, head_a/b, data_a/b, proof_a/b }` + `verify(public)` — pins the disagreement to
    a **shared block index**: both sides must be signed by `public` and prove their block at `index`
    (reuses `verify_block`), and the two blocks must differ. Works across **different-length** heads
    (truncate-and-rewrite forks), where `conflicting_heads` abstains.
- 5 asserting tests (hypercore 14→19): a forking writer (same author, `[a,b,c,d,e]` vs `[a,b,c,d,f]`)
  is caught by both detectors at the divergence; an honest length-7 extension of a length-5 log is
  **not** a fork (different lengths ⇒ `conflicting_heads` abstains; shared blocks agree ⇒ no
  `ForkProof`); identical logs don't conflict; `ForkProof` tamper-rejection (wrong author key, tampered
  data / proof sibling / signed head root, mismatched index claim — diverging at index 1 of a 4-block
  log so the proof carries interior siblings); two *different* authors disagreeing are **not** a fork
  under either key. `just verify` green (60 native tests + wasm build of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0019: fork detection is a self-contained L1 capability (signed-head conflict + per-index
  `ForkProof`), not upstream's replication-time `'conflict'` event. The replication mechanism (peer
  streams, the event, session teardown) is out of scope (networking/sessions; returns with Iroh,
  ADR-0003) and upstream's own `conflicts.js` is `test.skip`ed for a session-lifecycle flake — so
  `conflicts.js` stays `[~]`: detection behaviour ported, mechanism deferred. Soundness rests only on
  leaf collision-resistance, which the Merkle scheme already assumes.

**Lessons** (moved to `docs/LESSONS.md`)
- Fork detection = two L1 primitives (same-length/different-root proof-free detector; per-index
  inclusion-proof fork proof), not a replication event — soundness is just leaf collision-resistance.
- Tamper-test gotcha: a block that *is* a root has an **empty** sibling list (block 4 of a 5-block log
  = leaf 8 = a root), so to exercise a "tampered sibling" case diverge at an interior index (index 1 of
  a ≥4-block log), not the last block.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) once a container runtime is *started*
  (still environment-blocked: Apple `container` service isn't started — needs an XPC service outside
  the loop's allowlist — and the image pull needs network; see iter 11). Build `tools/oracle/` driving
  the reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2), and more upstream rows: merkle **seek/upgrade/reorg**
  (`merkle-tree.js`/`merkle-tree-recovery.js`), `autobase` `topolist.js` ordering, and the view/apply
  layer (`apply.js`/`anchors.js`) that the `linearizer.js`/`dags.js` view-length assertions need.

---

## 2026-06-29 — Iteration 13: `merkle` length-extension (`upgrade`) proofs

**Did**
- Added the **length-extension / consistency proof** to `crates/merkle` — the L1 behaviour behind
  upstream `merkle-tree.js`'s "proof with upgrade*" cases, and the cross-length analogue of iter 12's
  fork detection (ADR-0019):
  - `MerkleTree::upgrade_proof(old, new) -> UpgradeProof { old_len, new_len, nodes }` — a **data-free**
    proof that the signed tree at length `new` is a genuine **append-only extension** of the tree at
    length `old` (the first `old` blocks weren't rewritten). It carries only the **fully-new** subtree
    nodes (every covered block `>= old`) needed to fold the verifier's trusted old roots up into the new
    roots. Generated by walking down from each new root, stopping at old roots (the verifier has them)
    and emitting the largest fully-new subtrees (it needs them).
  - `UpgradeProof::verify(old_roots, new_root_hash)` — seeds its frontier with the verifier's **own**
    trusted old roots, folds in the supplied nodes **only if fully-new and in-range** (rejecting any
    straddling / fully-old node — this is the anti-fork hinge), climbs sibling pairs to the new roots,
    and checks `tree_hash(new_roots) == new_root_hash`. Requires `1 <= old < new <= len` (`old = 0` has
    no trusted anchor ⇒ refused).
- 7 asserting tests (merkle 11→18): every `(old < new)` pair in `1..=20` round-trips; single-step
  (`new-1 → new`) extension; **supplied nodes are always fully-new** (the soundness invariant);
  **anti-fork** — a verifier holding the *honest* prefix rejects a forked longer head (block rewritten
  at index 2 < old) even though that proof is self-consistent against the *forked* old roots;
  tamper-rejection (tampered new node / wrong new head / dropped node / tampered old root / wrong-length
  old roots / an **injected fully-old node** = fork attempt); out-of-range (`old=0`, `old==new`,
  inverted, `new>len`); and **composition with `range_proof`** (confirm the honest append, then verify
  the new blocks `[old,new)` against the same head). `just verify` green (67 native tests + wasm build
  of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0020: the upgrade proof is a **standalone, data-free consistency proof**, not upstream's bundled
  block+seek+upgrade object (`upgrade.nodes` + `additionalNodes`, with the block leaf doubling as an
  upgrade node). We keep proofs separate (composes with `range_proof`); soundness comes from the verifier
  rejecting any non-fully-new supplied node so the new roots are necessarily rebuilt from the trusted old
  prefix. We **defer** `additionalNodes` (proving past the requested length), byte-offset **seek**, and
  **reorg/recovery**; `merkle-tree.js` stays `[~]`, `merkle-tree-recovery.js` `[ ]`.

**Lessons** (moved to `docs/LESSONS.md`)
- A Merkle length-extension proof needs no block data; its whole soundness is "**supplied nodes must be
  fully new**" — let a straddling node (e.g. a new root that also spans old blocks) be supplied directly
  and a rewritten-old-block fork bypasses the fold from the trusted old prefix. Generate with the same
  descent the verifier climbs (so the node sets agree by construction), require `old >= 1`, and test the
  anti-fork arm explicitly (the honest prefix must reject a forked longer head).

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not started — needs an XPC service outside the loop's allowlist — and the image
  pull needs network; see iters 11–12). When a container runtime is *started*, build `tools/oracle/`
  driving the reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2); and more upstream rows: wire the merkle `upgrade_proof`
  into `hypercore`/`Replica` (accept a longer signed head as a verified extension before fetching the new
  blocks); merkle **seek** + `merkle-tree-recovery.js` reorg; `autobase` `topolist.js` ordering; and the
  view/apply layer (`apply.js`/`anchors.js`) the `linearizer.js`/`dags.js` view-length assertions need.

---

## 2026-06-29 — Iteration 14: `hypercore` verified length-extension replication

**Did**
- Wired iter 13's data-free merkle `upgrade_proof` (ADR-0020) into `crates/hypercore` as the
  replication anti-fork gate behind upstream `core.js`'s "apply a longer remote head" flow (L1, no
  networking):
  - `Replica::verify_upgrade(new_head, proof)` — the gate a replica applies **before** fetching a
    longer head's blocks. Accepts a longer signed head only if (a) the author signed it, (b) the proof
    bridges *exactly* from the replica's current verified length to the new head's length
    (`old_len == len()`, `new_len == new_head.length > len()`), and (c) folding the proof's fully-new
    nodes into the replica's **own** trusted roots reconstructs `new_head.root`. Pure check — no
    mutation; the new blocks `[old, new)` are then fetched with the existing `add_block`.
  - `Hypercore::upgrade_proof(old, new)` — exposes the merkle generator on the source side (mirrors
    `proof`).
  The point: an inclusion proof ties a block to *the head it came with*, so a forking writer's
  self-consistent longer head would have every block verify and the replica would silently adopt a
  forked history it had already contradicted — `verify_upgrade` ties the new head back to trusted state
  first, so the fork is caught before a single new block is downloaded.
- 3 asserting tests (hypercore 19→22): a replica replicates a length-5 log, accepts a verified
  extension to length 9 (proof supplies new subtree nodes; no block data), fetches only `[5,9)`, and
  ends **byte-identical** at the new signed head; a forking writer (same author, block 2 rewritten,
  extended to 9) is **rejected** by `verify_upgrade` against the honest prefix even though its head is
  validly self-signed, and the replica is left untouched at length 5; and a malformed/tampered battery
  (tampered new-head root ⇒ bad signature; tampered proof node; wrong `old_len` ≠ replica length; proof
  `new_len` ≠ head length; a length-7 head signed by a *different* author). `just verify` green
  (70 native tests + wasm build of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0021: the upgrade proof is wired as a **standalone pre-fetch gate** verifying a longer head
  against the replica's *own* roots, not upstream's bundled block+upgrade wire object applied inside
  the replication protocol. We keep proofs separate (composes with `add_block`), the gate purely
  verifying, and **defer** signed-length fast-forward / `additionalNodes` / wire framing (networking,
  ADR-0003; `fast-forward.js` row). The empty-replica case (`old = 0`) has no anchor ⇒ no upgrade gate,
  replicating from scratch. `core.js` advances toward verified incremental replication.

**Lessons** (moved to `docs/LESSONS.md`)
- An inclusion proof ties a block to *a* head, not to the replica's history, so a longer head needs a
  separate consistency gate: fold a data-free `UpgradeProof` into the replica's own roots and require it
  to rebuild the new head's root, *before* downloading — the cross-length analogue of
  `conflicting_heads`/`ForkProof`. The empty replica (length 0) has no anchor, so it replicates from
  scratch against the head directly.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not started — needs an XPC service outside the loop's allowlist — and the image
  pull needs network; see iters 11–13). When a container runtime is *started*, build `tools/oracle/`
  driving the reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2); merkle **seek** + `merkle-tree-recovery.js` reorg;
  `autobase` `topolist.js` ordering; and the view/apply layer (`apply.js`/`anchors.js`) the
  `linearizer.js`/`dags.js` view-length assertions need.

---

## 2026-06-29 — Iteration 15: `merkle` byte-offset seek + verifiable seek proof

**Did**
- Added **byte-offset seek** to `crates/merkle` — the L1 behaviour behind upstream `merkle-tree.js`'s
  "basic tree seeks" + seek-proof cases, the byte-offset analogue of inclusion proofs:
  - `MerkleTree::seek(bytes) -> (block, offset)` — a **tree-accelerated** locator that descends the flat
    tree by each subtree's committed byte `size` (O(log n)), mapping a byte offset to the block it lands in
    and the offset within. A byte exactly on a block boundary belongs to the block it starts; past-the-end
    returns `(len, bytes - total)` (mirroring the upstream linear seek). Never inspects payloads — only the
    authenticated byte sizes.
  - `MerkleTree::seek_proof(bytes) -> Option<SeekProof>` + `SeekProof::verify(expected_root) -> Option<(block,
    offset)>` — a **standalone, data-free** proof: the target block's inclusion path (siblings + roots) plus
    the leaf node and the byte offset. `verify` climbs the leaf to its containing root via `parent_hash`
    (binds each child's hash **and** size), substitutes the recomputed root, checks `tree_hash ==
    expected_root` (authenticating every size on the path), then derives the block's left-cumulative byte
    size from the **left** siblings met while climbing + the roots left of the containing root, and accepts
    iff `cumulative <= bytes < cumulative + leaf.size`. Returns the authenticated `(block, offset)`; `None`
    past the end (also the empty tree).
- 6 asserting tests (merkle 18→24): the tree seek equals a naive linear scan for **every** byte offset over
  sizes 1..=20 (varied per-block sizes) incl. past-the-end (the "basic tree seeks" property); every in-range
  offset's seek proof verifies and returns the same `(block, offset)` as the local seek; hand-checked block
  boundaries (first/last byte of each block in a 5-block tree); seek-proof past-the-end / empty-tree ⇒ `None`;
  tamper-rejection (tampered leaf hash/size, tampered sibling, tampered **untouched** root, wrong expected
  root, dropped sibling, and `bytes` moved into a *different* block's interval); single-root (power-of-two)
  sanity. `just verify` green (76 native tests + wasm build of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0022: byte seek is a tree-accelerated locator + a **standalone, data-free** seek proof, kept separate
  from block/upgrade proofs (consistent with ADR-0017/0020), not upstream's bundled `proof.seek.nodes` wire
  object (where the block leaf sometimes doubles as a seek node). We **omit `padding`** (per-block framing
  overhead is application byte-layout — it would leak domain assumptions into L1; a consumer subtracts its own
  framing before seeking). Soundness rests on the existing hash/size binding (`parent_hash` over child sizes,
  `tree_hash` over root sizes) plus the disjoint-contiguous-interval argument: exactly one block's
  authenticated byte interval brackets `bytes`. We **defer** the bundled-wire seek, `additionalNodes`, and
  reorg/recovery — `merkle-tree.js` stays `[~]`, `merkle-tree-recovery.js` `[ ]`.

**Lessons** (moved to `docs/LESSONS.md`)
- A byte-offset seek proof is an inclusion proof read for *sizes*, not data: ship the leaf + siblings + roots,
  authenticate every size via the hash climb to the trusted root, then the left-cumulative size is just the
  sum of left-sibling + left-root sizes and the offset is in-block iff it brackets `bytes`. Disjoint contiguous
  intervals ⇒ exactly one block brackets `bytes`, so no separate "right block?" check is needed. A tampered
  `bytes` *within* the proven block is still a correct proof (not an attack) — test rejection by moving `bytes`
  into a different block's interval. Assert the O(log n) tree seek equals a naive linear scan for **every**
  offset (that equivalence is the point); keep `padding`/framing out of L1.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple `container`
  service not started — needs an XPC service outside the loop's allowlist — and the image pull needs network;
  see iters 11–14). When a container runtime is *started*, build `tools/oracle/` driving the reference
  `lib/topolist.js` (deps injected via `Module._compile`, network-free) through `scripts/node-sandbox.sh`;
  compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2); merkle **recovery/reorg** (`merkle-tree-recovery.js`) +
  `upgrade.additionalNodes`; `autobase` `topolist.js` ordering; and the view/apply layer
  (`apply.js`/`anchors.js`) the `linearizer.js`/`dags.js` view-length assertions need.
