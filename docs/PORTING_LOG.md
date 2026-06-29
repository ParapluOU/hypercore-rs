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

---

## 2026-06-29 — Iteration 16: `merkle` node recovery (`merkle-tree-recovery.js`)

**Did**
- Added **tree-node recovery + repair mode** to `crates/merkle` — the L1 behaviour behind upstream
  `merkle-tree-recovery.js` (networking/storage/sessions stripped): a tree whose stored nodes are the
  source of truth can lose one and securely recover it from a peer that holds only the signed root.
  - `remove_node(index)` (corruption injector, the analogue of upstream `deleteTreeNode`) +
    `has_node`; `missing_nodes()`/`is_intact()` derive **repair mode** (every node implied by the
    length — `block_range(i).end <= len` — must be present); `try_roots()`/`try_root_hash()` are the
    panic-free counterparts that return `None` over a gap; `try_append` **refuses while in repair
    mode** (extending a corrupt tree could bake in an inconsistent root).
  - `node_proof(index) -> Option<NodeProof>` (analogue of `generateRemoteProofForTreeNode`) — an
    authenticated proof of **any** tree node (leaf / interior / root), not just a leaf-from-data:
    the node + its sibling path to the containing root + all roots. `NodeProof::verify(expected_root)
    -> Option<Node>` climbs the node to its root via `parent_hash` (binds hash **and** size),
    substitutes the recomputed root, checks `tree_hash == expected_root`, and returns the
    authenticated node. `recover_node(&proof, expected_root)` is **atomic**: verify first, store only
    on success, leave the tree untouched on any tamper.
- 6 asserting tests (merkle 24→30): a corrupt tree (all roots deleted) still reports its length and
  refuses a root hash ("can still ready" / "still has length"); a deleted **root** recovers from a
  remote proof and `try_root_hash` is restored exactly ("fix via fully remote proof"); a deleted
  **interior sub-root** recovers (root hash unaffected by the gap; node provable again, equal to the
  original) ("fix … sub root"); **atomicity** — mangled size / mangled hash / tampered sibling /
  dropped sibling / wrong expected-root each rejected with the node left missing, then the honest
  proof recovers cleanly ("atomically updates storage"); appends **refused in repair mode** and
  resume after recovery ("fail appends … when in repair mode"); and a round-trip over every stored
  node of sizes 1..=16 (prove → delete a copy → recover → intact). `just verify` green (82 native
  tests + wasm build of `hypercore`/`autobase`/`storage`).

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0023: node recovery is **storage robustness + a standalone, data-free `NodeProof`** verified
  against the trusted signed root (the arbitrary-node generalization of `Proof`), plus a derived
  repair-mode that refuses appends — not upstream's replication-driven repair (`_repairMode`,
  `recoverTreeNodeFromPeers`, `repairing`/`repaired`/`repair-failed` events, range-request auto-repair).
  Those are networking/sessions (out of scope; return with Iroh). `merkle-tree-recovery.js` moves
  `[ ]`→`[~]`. We still **defer** reorg/`additionalNodes` (`merkle-tree-recovery.js`'s sibling concern
  remains, plus `merkle-tree.js`'s `additionalNodes`).

**Lessons** (moved to `docs/LESSONS.md`)
- Tree-node recovery is just an inclusion proof that **starts from an arbitrary node** (not a leaf
  recomputed from data): authenticate the node by climbing it to the trusted signed root and require
  `tree_hash == expected_root`, then it is safe to store. "Repair mode" is derivable, not a flag — a
  node is implied by the length iff its whole block range is within `[0, len)`, so the missing set
  (and the append guard) fall out of the length. Recovery must be **verify-then-store** so a mangled
  proof leaves storage untouched (atomic). The corrupt *source* cannot prove the node it lost
  (`node_proof` needs the node present) — proofs flow from a healthy holder to the gap.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not started — needs an XPC service outside the loop's allowlist — and the image
  pull needs network; see iters 11–15). When a container runtime is *started*, build `tools/oracle/`
  driving the reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2); `merkle` reorg/`additionalNodes` (the last
  `merkle-tree.js`/`merkle-tree-recovery.js` pieces); `autobase` `topolist.js` ordering; and the
  view/apply layer (`apply.js`/`anchors.js`) the `linearizer.js`/`dags.js` view-length assertions need.

---

## 2026-06-29 — Iteration 17: `hypercore` truncate + signed `fork` counter

**Did**
- Added **truncate** — the local "rewind to a prefix" capability behind upstream `core.js`'s
  "core - append and truncate" test (and `move-to.js`'s `truncate(1)`), with no networking:
  - `merkle`: `MerkleTree::truncate(new_len)` discards every block — and derived node — at index
    `>= new_len` (`retain` nodes whose whole block range lies in `[0, new_len)`), leaving a tree
    **node-for-node identical** to a fresh tree of the first `new_len` blocks, so `root_hash()` equals
    the prefix's root (the head at a length is a pure function of the first `length` blocks — the same
    property fork detection rests on). `byte_length()` = sum of the (authenticated) root subtree sizes.
  - `hypercore`: a signed **`fork` counter** now binds into the head message
    (`head_message(fork, length, root)`) and `SignedHead`. `Hypercore::truncate(new_len) ->
    Option<Truncation>` rewinds the tree, **increments `fork` by one**, re-signs, and records
    `last_truncation { from, to }`; the next `append`/`commit` clears it. Accessors `fork()`,
    `byte_length()`, `last_truncation()`. A private `resign()` consolidates the three signing sites
    (append / commit / truncate).
- **Sharpened fork detection for the fork counter (extends ADR-0019).** An *equivocation* is now two
  contradictory histories at the **same** fork: `conflicting_heads` requires `a.fork == b.fork` (in
  addition to equal length / different root), and `ForkProof::verify` requires `head_a.fork ==
  head_b.fork`. A divergence across **different** forks is a legitimate author reorg (truncate bumps
  the counter; readers follow the highest fork) and is no longer flagged.
- 7 asserting tests (merkle 30→33, hypercore 22→26):
  - merkle: truncate == fresh-prefix for every `(new_len < n)` over sizes 1..=20 (root hash, root
    nodes, `byte_length`, intactness, surviving-block proofs all match a fresh prefix); `byte_length`
    tracks the live prefix (incl. truncate-to-0 == fresh empty); no-op truncate (`new_len >= len`) +
    clean re-append after truncate (reused indices overwritten).
  - hypercore: the `core.js` fork/length/byteLength/`lastTruncation` progression (fork 0→7 over seven
    truncations, append clearing `lastTruncation`, no-op truncates); truncated head == fresh-prefix
    head up to the fork counter; a replica replicates a truncated-and-rewritten source byte-identically
    (fork carried through the head); and **reorg-with-bumped-fork is not equivocation** (cross-fork
    same-length heads not flagged, cross-fork `ForkProof` refused) vs a same-fork rewrite which **is** a
    provable fork.
- `just verify` green: 89 native tests + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0024: truncate is a **pure in-memory rewind to a prefix + a signed fork counter**, not upstream's
  storage-batch truncate (`MerkleTreeBatch.truncate` + reorg-hint persistence). The fork counter is
  signed into the head so a deliberate reorg is distinguishable from an equivocation; this refines fork
  detection (equivocation = same-fork contradiction). We **defer** physical storage reclamation
  (blocks `>= new_len` go unreachable and are overwritten on re-append; `clear`/`purge` are separate),
  reorg-by-proof, and `additionalNodes`.

**Lessons** (moved to `docs/LESSONS.md`)
- Truncate is just "rewind to a prefix": keep every tree node whose block range is `< new_len` and the
  result is byte-for-byte the prefix tree (so the root is the prefix's root — no recomputation), because
  the first `new_len` blocks were never touched. The signed **fork counter** is what makes truncation a
  first-class, non-equivocating operation: bind it into the head and an equivocation becomes a
  *same-fork* contradiction (a higher fork is a legitimate reorg, which readers follow). Watch the
  framing: `byte_length` is the **encoded** (stored) prefix size the tree commits to, not raw payload
  length, so assert it against a freshly-built prefix rather than hardcoded byte counts.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not started — needs an XPC service outside the loop's allowlist — and the image
  pull needs network; see iters 11–16). When a container runtime is *started*, build `tools/oracle/`
  driving the reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2); `merkle` reorg-by-proof/`additionalNodes` (the last
  `merkle-tree.js`/`merkle-tree-recovery.js` pieces); `autobase` `topolist.js` ordering; and the
  view/apply layer (`apply.js`/`anchors.js`) the `linearizer.js`/`dags.js` view-length assertions need.

---

## 2026-06-29 — Iteration 18: `merkle` reorg / lowest common ancestor

**Did**
- Added **reorg / lowest-common-ancestor** to `crates/merkle` — the L1 behaviour behind upstream
  `merkle-tree.js`'s five "lowest common ancestor" tests (themselves host-safe: a local
  `reorg(clone, core)` over two in-memory trees, no swarm), the content-following counterpart of
  iter 17's `truncate`:
  - `MerkleTree::lowest_common_ancestor(&other) -> u64` — the **content-blind** divergence finder. Two
    trees agree on blocks `[0, a)` iff their (private) `prefix_root_hash(a)` are equal — the head at a
    length is a pure function of the first `length` blocks (the property truncate/fork-detection rest
    on) — and prefix agreement is **monotone**, so the LCA is a **binary search** over `0..=min(len)`
    comparing only authenticated prefix root hashes. Never peeks at payloads; requires both trees
    intact (a gap reads conservatively as disagreement).
  - `MerkleTree::reorg(&other) -> u64` — keeps the shared LCA prefix (`truncate`s to it: the surviving
    nodes already equal `other`'s prefix, so it's **preserved, not re-derived**) and adopts `other`'s
    nodes for the divergent suffix, leaving the tree **byte-identical** to `other`. Returns the
    `ancestors` length. **Fork-agnostic** — it reorganizes tree nodes; *which* `other` to follow (the
    signed head + fork counter) is the hypercore layer's job.
- 5 asserting tests (merkle 33→38): the upstream prefix-gap cases (remote=10/local=8 ⇒ LCA 8;
  remote=20/local=1 ⇒ 1; remote=5/local=10 ⇒ 5) with a byte-identical follow each way; simple fork
  (share 5, diverge at block 5 ⇒ LCA 5); long fork (diverge at 5, each appends 100 ⇒ LCA 5, full
  adopt); a **property** test asserting LCA == `k` for every shared-prefix length `k` over sizes
  1..=16 (incl. identical trees ⇒ LCA = full length, reorg a no-op) and **symmetry** of the LCA; and
  reorg **preserves the shared prefix** (truncating the reorged tree back to `ancestors` reproduces
  the very prefix root that existed before the reorg). A shared `assert_followed` helper checks
  length / roots / root hash / byte_length / intactness and that every adopted block proves.
- `just verify` green: 94 native tests + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0025: reorg is a **local LCA (binary search over prefix root hashes) + adopt-suffix** on the
  tree, not upstream's `ReorgBatch` `want`/`update` multi-round node-request narrowing (that top-down
  root descent is forced by the wire protocol; with both full trees in memory the binary search
  computes the same `ancestors` in one shot). We **defer** the *secure replica-level* reorg —
  authenticating which `other` to follow via the signed head + fork counter, plus the proof-narrowing
  exchange — to the hypercore layer; it is networking-driven (ADR-0003), the cross-fork analogue of how
  iter 14 (ADR-0021) wired `UpgradeProof` into `Replica::verify_upgrade`. `additionalNodes` adds no L1
  capability our standalone `upgrade_proof(old, any_new)` lacks (ADR-0020). `merkle-tree.js` stays
  `[~]` (LCA/reorg added; bundled-wire seek + `additionalNodes` remain).
- The **JS oracle (gate #4) is still environment-blocked** (iters 11–17): the Apple `container`
  binary is on PATH but `container system status`/`system start` is outside the loop's scoped
  allowlist (needs approval the headless driver can't give), and the image pull needs network — so a
  green oracle run isn't reachable here. Likewise the **wasm runtime gate (#2)** needs headless Chrome.
  Both stay deferred; I picked the next self-contained, natively-testable red item instead.

**Lessons** (moved to `docs/LESSONS.md`)
- The LCA of two trees is a **binary search over prefix root hashes** — no payload peek, no node-by-node
  descent (that's a wire-protocol artifact). `agree(a)` = equal root-hash-at-length-`a`, monotone, so the
  LCA is the largest `a ≤ min(len)` with agreement. `reorg` = `truncate(lca)` (keep the preserved prefix)
  + adopt the other's suffix, ending byte-identical; keep it fork-agnostic. Test gotcha: reorg always
  makes the local follow the remote (up **or** down to the remote's length), so the follow target is
  always the remote — don't branch on "which is longer" (I hit exactly that and fixed it).

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not startable under the loop's allowlist + image pull needs network; see
  iters 11–17). When a container runtime is *started*, build `tools/oracle/` driving the reference
  `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the **secure replica-level reorg** (hypercore): a `Replica` follows the source's
  truncate-and-rewrite (higher fork) — verify the new signed head against trusted state, then
  reorg-to-LCA + re-fetch the suffix (the cross-fork analogue of `verify_upgrade`, closing the iter 17
  truncate loop). Then the wasm runtime / IndexedDB gate (#2); `merkle` reorg-by-proof/`additionalNodes`;
  `autobase` `topolist.js` ordering; and the view/apply layer (`apply.js`/`anchors.js`) the
  `linearizer.js`/`dags.js` view-length assertions need.

---

## 2026-06-29 — Iteration 19: `hypercore` secure replica-level reorg

**Did**
- Closed the **secure replica-level reorg** ADR-0025 deferred to the hypercore layer (the cross-fork
  analogue of iter 14's `verify_upgrade`, ADR-0021), wiring iter 18's local LCA/reorg into a
  verify-only `Replica` that follows the author's truncate-and-rewrite (`core.js`):
  - `merkle`: exposed `MerkleTree::prefix_roots(len)` (the authenticated anchor — the roots the tree
    *would* have at a prefix length; identical in any two trees sharing that prefix) and made
    `prefix_root_hash` public; the latter now delegates to the former.
  - `hypercore`: `Replica::verify_reorg(new_head, ancestors, proof)` (pure) + `Replica::reorg(..)`
    (verify-then-truncate). A reorg is followed only at a **strictly higher `fork`** (a same/lower
    fork is a stale head or an equivocation — never a history to adopt) signed by the author. The
    claimed shared-prefix length `ancestors` is **authenticated, not trusted**, by re-anchoring the
    data-free `UpgradeProof` on the replica's own roots *at `ancestors`* (`prefix_roots`): the fold
    reaches `new_head.root` only if `[0, ancestors)` is genuinely shared. Three anchor cases:
    `ancestors == new_head.length` (pure truncation — the new head *is* our prefix, no proof);
    `ancestors == 0` (no prefix — adopt the signed head from scratch, every block re-verified on
    refetch); else the proof bridges `ancestors -> new_head.length`. `reorg` then `truncate`s to
    `ancestors` and the caller refetches the suffix with the existing `add_block`, ending
    **byte-identical** to the rewritten history.
- 4 asserting tests (hypercore 26→30): follow a fork-bumped rewrite ([a,b,c,d,e] ⇒ [a,b,c,X,Y]) —
  verify, drop the suffix to len 3, refetch [3,5), byte-identical; **pure truncation** (ancestors ==
  new length, no proof, completes immediately); **from scratch** (ancestors 0, no shared prefix,
  full re-replication); and a **rejection battery** — an *over-claimed* ancestor (4 when the true
  divergence is 3) on an honest head, a forking writer that rewrote an *old* block (b→Z) under a
  bumped fork claiming to share [0,5), and a *same-fork* divergence (equivocation) refused at any
  ancestor — with the replica left untouched at its honest fork-0 head throughout.
- `just verify` green: 98 native tests + wasm build of `hypercore`/`autobase`/`storage`. (Also added
  the stray empty `verify.log` to `.gitignore`.)

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0026: the secure reorg is an **L1 gate that re-anchors the `UpgradeProof` on the shared prefix's
  roots**, not upstream's `want`/`update` proof-narrowing wire exchange. `ancestors` authenticates
  itself (over-claim ⇒ the fold misses the new root ⇒ rejected; under-claim ⇒ a real shorter shared
  prefix, safe, only extra refetch), so the maximal-ancestor `lowest_common_ancestor` binary search
  (ADR-0025) is a pure efficiency concern, not a security boundary. We **defer** the wire exchange
  that *discovers* `ancestors` and delivers the suffix proofs in a live system (networking, ADR-0003)
  — the test supplies the construction-known divergence point and the source produces the proofs.
  `core.js` advances; ADR-0025's deferred replica-level reorg is now done.

**Lessons** (moved to `docs/LESSONS.md`)
- A replica follows a reorg by re-anchoring the *same* upgrade proof on a **proper prefix** of its own
  history (`prefix_roots` at `ancestors`), not its full head — and the claimed ancestor then
  authenticates itself (over-claim rejected, under-claim safe), so the LCA search is purely
  efficiency. Gate the fork (strictly higher only); two degenerate anchors (`ancestors == new length`
  pure truncation, `ancestors == 0` from scratch) need no proof; then `truncate` + refetch via
  `add_block`.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not startable under the loop's scoped allowlist + image pull needs network; see
  iters 11–18). When a container runtime is *started*, build `tools/oracle/` driving the reference
  `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2, needs headless Chrome); `merkle`
  reorg-by-proof/`additionalNodes` (the last `merkle-tree.js` pieces); `autobase` `topolist.js`
  ordering; and the view/apply layer (`apply.js`/`anchors.js`) the `linearizer.js`/`dags.js`
  view-length assertions need.

---

## 2026-06-29 — Iteration 20: `autobase` `topolist.js` ordering (in-Rust equivalence oracle)

**Did**
- Ported the **ordering / stable-ordering behaviour** of `reference/js/autobase/test/topolist.js`
  as `crates/autobase/tests/topolist.rs`, and turned ADR-0014's long-standing claim — that our
  **priority-Kahn** `order()` reproduces upstream's incremental insertion sort — into a host-safe
  asserting cross-check (the JS oracle gate #4 stays env-blocked, iters 11–19; this needs no `node`,
  no container):
  - A **faithful, test-only re-statement** of upstream's *non-optimistic* `lib/topolist.js`
    insertion sort (`topolist_oracle`): push each node, `moveDown` to its causal floor (just after
    the last node it directly depends on), then `moveNonOptimisticUp` past every strictly-smaller
    node — with `cmp`/`cmpUnlinked`/`links` over `direct[a]` = explicit cross-heads ∪ same-writer
    predecessor (the exact union upstream's `links` recognizes and `Linearizer::add` builds). It is
    a behavioural mirror used *only* as an oracle, never the production path.
  - Cross-checks that the oracle equals `Linearizer::order()` on (a) the explicit
    `topolist - stable ordering` example `[a0, b0, c0, c1]` (where `c1` follows `c0` purely by
    same-writer sequencing, listing *no* explicit deps) across its causally-valid add orders;
    (b) the three canonical `DESIGN.md` DAGs (linear chain / branch tiebreak / recursive
    `[a0, c0, a1, b0, b1, c1, b2]`), all three vs the hand-derived expected order *and* vs the
    oracle; and (c) **200 seeded random fork/merge DAGs × several randomized-Kahn delivery orders**,
    each asserting the oracle is itself delivery-order independent (upstream's `stable ordering` /
    `fuzz` property) and equal to `order()`, plus per-edge causal-respect and a non-vacuity guard
    (≥1 seed actually reorders creation order).
- 3 asserting tests (autobase 16→19 across files: 14 unit + 2 convergence + 3 topolist).
  `just verify` green: **101 native tests** + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0027: port `topolist.js`'s ordering behaviour via an in-Rust, test-only re-statement of the
  non-optimistic insertion sort, asserting it equals our priority-Kahn `order()` (both compute the
  unique **lex-minimal linear extension under (key, seq)**). It **complements, does not replace** the
  upstream-JS oracle (gate #4) — that runs the *actual* reference code in a sandbox and stays the
  deferred cross-language check. We **defer** the streaming-view bookkeeping (`undo`/`shared`/`mark`/
  `flush`/`indexed` — a live-view patch optimization, not the ordering definition) and **optimistic**
  nodes (`optimistic.js`). `topolist.js` moves `[ ]`→`[~]`.

**Lessons** (moved to `docs/LESSONS.md`)
- Upstream's topolist insertion sort and our priority-Kahn both compute the *lex-minimal linear
  extension* under (key, seq) — which is unique, so they're equal node-for-node, even though the
  per-pair "causal-or-(key,seq)" comparison is non-transitive (a causal edge can cross the key
  tiebreak into a ≺-cycle; "always take the minimum *available* node" sidesteps it). You can prove
  this host-safely by transliterating only the *non-optimistic* insertion sort into a test oracle
  (its `links(a,b)` is just "b ∈ a's direct deps") and asserting equivalence over the `DESIGN.md`
  DAGs + seeded random DAGs × delivery orders — a complement to, not a replacement for, gate #4.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not startable under the loop's scoped allowlist + image pull needs network;
  see iters 11–19). When a container runtime is *started*, build `tools/oracle/` driving the
  reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()` (now additionally cross-validated by the
  iter-20 in-Rust oracle, so a green run should agree by construction).
- Then the wasm runtime / IndexedDB gate (#2, needs headless Chrome); the **view/apply layer**
  (`apply.js`/`anchors.js`) that the `linearizer.js`/`dags.js` `getIndexedViewLength`/`view.get`
  view-length assertions need; and `merkle` reorg-by-proof/`additionalNodes` (the last
  `merkle-tree.js` pieces — note ADR-0025: `additionalNodes` adds no L1 capability our standalone
  proofs lack).

---

## 2026-06-29 — Iteration 21: `autobase` view materialization (`linearizer.js`/`dags.js`)

**Did**
- Ported the **view materialization** behaviour of `reference/js/autobase/test/linearizer.js` +
  `dags.js` — the `view` / `view.get(i)` / `getIndexedViewLength` assertions — as thin accessors
  over the existing linearizer plus `crates/autobase/tests/view.rs`. Upstream linearizes the DAG
  and then *applies* each node to materialize a `view` (the apply step is where domain logic lives,
  possibly batching entries per node); at L1 there is nothing to apply (content-blind), so the
  domain-agnostic fold is the **identity** one — one node, one entry (its `NodeId`):
  - `Linearizer::view()` ≡ `order()` (the materialized view); `view_len()` ≡ `view.length`
    (node count); `view_get(i)` ≡ `view.get(i, {wait:false})` (`None` past the end);
    `indexed_view()` ≡ `finalized()`; `indexed_view_len()` ≡ `getIndexedViewLength`
    (`getIndexedInfo().views[].length`). A consuming app replays `view()` through *its* apply to
    build the typed view; only the ordering/confirmation is L1.
- 3 asserting tests (autobase 19→22 across files: 14 unit + 2 convergence + 3 topolist + 3 view):
  - `simple_chain_view_and_indexed_length` — the fork-free `c-b-a-c-b-a` indexer chain
    (`linearizer - simple` / `dags - simple 3`): the full view `[c0,b0,a0,c1,b1,a1]`,
    `view_len == 6`, the per-index `view_get` sequence, `view_get(6) == None`, the single tail, and
    **`indexed_view_len == 4`** — the double-quorum'd `[c0,b0,a0,c1]` prefix, matching upstream's
    `getIndexedViewLength` exactly (for a fork-free chain our conservative double-quorum
    confirmation = upstream's confirmed length; hand-verified the quorum recursion gives degrees
    c0/b0/a0 = 3, c1 = 2, b1 = 1, a1 = 0 ⇒ prefix length 4).
  - `recursive_dag_view_converges` — the recursive `DESIGN.md` DAG (forks: a0/c0 concurrent
    tails): the view is the canonical `[a0,c0,a1,b0,b1,c1,b2]`, and across three causally-valid
    delivery orders the view, every `view_get`, and the **indexed view length converge** (the
    `getIndexedViewLength(a)==(b)==(c)` family) with the indexed view always a prefix — asserting
    the always-true convergence/prefix properties, *not* a specific fork-case confirmed number.
  - `non_indexer_nodes_are_in_view_but_do_not_index` — a non-indexing writer's node is in the view
    but never advances the indexed view (the view/indexed split is orthogonal to indexer status).
- `just verify` green: **104 native tests** + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0028: view materialization is the **identity fold** at L1 (one node = one entry, its
  `NodeId`) — `view`/`view_get`/`indexed_view*` are thin accessors over `order()`/`finalized()`.
  Assert the **exact upstream numbers only where our conservative confirmation matches** (the
  fork-free chain → view 6 / indexed 4), and assert cross-replica view + indexed-length
  **convergence** as a property over every DAG. **Defer** the indexed-length *values* where upstream
  confirms earlier — a **unanimous single quorum** (`dags - simple 2`, `n == 2`) and confirmation
  across a **resolved fork/merge** (`compete`/`count ordering`), both the deferred fork/merge
  consensus (ADR-0015) — and the **per-replica partial view** (apply/view layer). `linearizer.js` /
  `dags.js` stay `[~]`. No typed payload / per-node batch (domain concerns, ADR-0002/0007).

**Lessons** (moved to `docs/LESSONS.md`)
- At L1 the autobase "view" is the linearization itself and the "indexed view" is the finalized
  prefix — the apply step is the domain logic you deliberately don't have, so the fold is identity.
  Only assert the upstream `getIndexedViewLength` *number* where your confirmation rule matches it
  (fork-free chain ✓; unanimous single quorum and resolved-fork cases confirm earlier upstream than
  our conservative double-quorum form — assert convergence/prefix properties there, not the number).
  A forced chain has only one valid delivery order, so a "converges across delivery orders" test
  over it is vacuous — use a genuinely forked DAG (concurrent tails) to exercise reordering.

**Next**
- **JS algorithmic-equivalence oracle** (gate #4, ADR-0008) — still environment-blocked (Apple
  `container` service not startable under the loop's scoped allowlist + image pull needs network;
  see iters 11–20). When a container runtime is *started*, build `tools/oracle/` driving the
  reference `lib/topolist.js` (deps injected via `Module._compile`, network-free) through
  `scripts/node-sandbox.sh`; compare order vs our `order()`.
- Then the wasm runtime / IndexedDB gate (#2, needs headless Chrome); the **deferred fork/merge
  consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation across a resolved fork/merge —
  which would let `finalized()`/`indexed_view_len()` match upstream's earlier-confirming cases
  (`dags - simple 2`, `linearizer - compete`/`count ordering`) and the **apply/view layer**
  (`apply.js`/`anchors.js`) needed for the per-replica partial views; and `merkle`
  reorg-by-proof/`additionalNodes` (the last `merkle-tree.js` pieces).

---

## 2026-06-29 — Audit round (after iteration 21)

**Did**
- Audited `autobase` (read directly), `merkle` + `hypercore` (independent adversarial reviewers;
  every headline finding re-verified against the code before acting).
- **Found + fixed a real soundness bug:** `SeekProof::verify` accepted a non-leaf (odd-index) node as
  the seek target — a prover could authenticate the root/an interior node and get a bogus `index/2`
  block accepted. Fix: reject odd `leaf.index` (matches upstream `ByteSeeker`'s `(index & 1) === 0`).
  **Not present upstream** — introduced by our clean-room reimpl.
- Added the missing `sib.index == flat::sibling(..)` structural guard to `Proof`/`SeekProof` (present
  in `NodeProof`). 2 regression tests; `merkle` 38 → 40 tests; `just verify` green. ADR-0029.

**Findings (queued in DEFINITION_OF_DONE as audit follow-ups)**
- Strong positive-path but under-tested negative-path across crates: `hypercore` replica
  cross-head/wrong-key rejection, atomic first/last-block + `delete`-failure, reorg head-`None`;
  `merkle` reorg/LCA adversarial; `autobase` quorum-degree value not oracle-checked.
- Overall quality high (pervasive non-vacuity guards, honest in-code deferrals); the audit's value
  was the one real exploit + the structural-binding asymmetry.

**Next**
- The queued audit follow-ups, then resume feature iterations (the gate-#4 JS oracle when a container
  runtime is available; wasm/IndexedDB gate; deferred fork/merge consensus; `hyperbee`).

---

## 2026-06-29 — Iteration 22: `hypercore` replica negative-path binding (audit follow-up)

**Did**
- Closed the next **audit follow-up** (DEFINITION_OF_DONE, after iter 21): the `Replica::add_block`
  negative paths that the positive-path replica tests never exercised — **cross-head root binding**
  and **wrong author key**. The behaviour already holds (`verify_block` checks the head signature
  under `self.public` and `proof.verify(data, &head.root)`); this iteration writes the *forged* proof
  that pins it (ADR-0029's lesson). 2 asserting tests (hypercore 30→32):
  - `add_block_binds_proof_to_the_specific_head` — an inclusion proof carries the root nodes of the
    head it was generated against, so an honest block+proof bound to one head is **rejected under a
    different same-author head**: (a) a **fork at the same length** (`[a,b,c,d,e]` vs `[a,b,c,d,X]`,
    same author, equal length, different root) — block 0's proof under head_a is refused under head_f
    because the proof's other root (block 4 = 'e') can't fold to head_f's root (built from 'X'); and
    (b) a **longer honest head** (length 7), both directions (length-5 proof under the length-7 head
    and vice versa). Each rejection stores nothing (`len() == 0`, `verified_head().is_none()`), with a
    positive control that the proof *is* accepted under its own head. Each cross-head case has
    `index == replica.len() == 0`, so the in-order guard passes and the test genuinely reaches the
    `verify_block` root-binding branch.
  - `add_block_rejects_wrong_author` — a replica keyed to author **A** refuses an internally-honest
    log signed by author **B**: B's blocks verify under B's key but *not* under A's (sanity-asserted
    both ways), and `add_block` against A's replica stores nothing. The head-signature check, not the
    proof, is the gate here.
- `just verify` green: **108 native tests** (autobase 22 across files + codec 8 + hypercore 32 +
  identity 4 + merkle 40 + storage 2) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions**
- **No new ADR** — no divergence from upstream: this is a test-only iteration closing a negative-path
  *coverage* gap on already-decided behaviour (the proof→head root binding and the replica's
  author-key gate). The governing decision is ADR-0029 (a verifier binds structural position / the
  exact head, and the test must *forge* the proof, not just present the honest one). No source change.

**Lessons** (moved to `docs/LESSONS.md`)
- An inclusion proof binds a block to *one specific head's root* (it carries that tree's root nodes),
  so the replica's cross-head rejection is only visible if you present an honest proof under a
  *different same-author head* — a fork at the same length (purest: only the root differs) or a longer
  honest head (both directions). And a replica keyed to A must refuse a wholly-valid log from B (the
  head-signature gate, not the proof). Positive-path replication tests pass straight through both
  branches; write the forged/mismatched proof to exercise them, and assert nothing is stored.

**Next**
- The remaining **audit follow-ups** (DEFINITION_OF_DONE), in order: `hypercore` atomic append —
  first/last-block fault injection + `delete`-failure handling (only a mid-batch `put` fault is
  exercised today); `hypercore` `verify_reorg` head-`None` branch; `merkle` reorg / LCA adversarial
  (corrupt `other`, gapped `self`, monotonicity-precondition violation; seek zero-size block);
  `autobase` quorum-degree *value* cross-checked against an independent computation over random DAGs.
- Then resume feature iterations: the gate-#4 JS oracle (still env-blocked — container service not
  startable under the loop's allowlist + image pull needs network; iters 11–21); the wasm runtime /
  IndexedDB gate (#2, needs headless Chrome); the deferred fork/merge consensus (ADR-0015); `hyperbee`.

---

## 2026-06-29 — Iteration 23: `hypercore` atomic-commit fault coverage (audit follow-up)

**Did**
- Closed the next **audit follow-up** (DEFINITION_OF_DONE, after iter 21): the `Hypercore::commit`
  atomicity paths the single iter-11 test (`failed_commit_is_atomic`) never exercised — a fault on
  the **first** vs the **last** staged block, and a **`delete` failure during rollback**. The
  behaviour already holds (writes-first / mutate-tree+head-last / roll back on any `put` error,
  ADR-0018); this iteration is test-only — it pins the boundary and the swallowed-rollback-delete
  branch. Extended the test-only `FaultyStore` with a `fail_delete_at` injector and added a `head_of`
  helper (the canonical fresh-prefix head a recover path must land on; ed25519 is deterministic so
  head equality is exact). 3 asserting tests (hypercore 32→35):
  - `commit_fault_on_first_staged_block_is_atomic` — `put` fails at the first staged index, so the
    commit aborts before any write succeeds: `written` is empty, there is nothing to roll back, and
    storage is left **pristine** (len 3, no key at 3), not merely logically unchanged. The `for w in
    &written` rollback loop is correctly a no-op.
  - `commit_fault_on_last_staged_block_rolls_back_all` — `put` fails at the last staged index, so both
    earlier successful writes (3, 4) are deleted: storage is back to len 3 with **no orphans**, head
    and reads untouched.
  - `commit_rollback_tolerates_delete_failure` — `put` fails at the last index **and** the rollback
    `delete` of index 3 also fails. The commit still returns the original *`put`* error (the secondary
    delete error is swallowed by `let _ = store.delete(..)`), and the log's **logical** state stays
    atomic (len 3, head unchanged, `get(3) == None`) even though one **unreachable orphan** — the
    *encoded* block `[1, b'd']` (codec varint length prefix) — physically survives at storage index 3
    (`store.len() == 4`). A later fault-free commit overwrites the orphan and lands byte-identically on
    the canonical six-block head with no stray keys (`store.len() == 6`).
  Each test asserts the recovered head equals `head_of(seed, ["a".."f"])`, so the whole
  fault→rollback→recover path provably lands on the canonical state.
- `just verify` green: **111 native tests** (autobase 22 across files + codec 8 + hypercore 35 +
  identity 4 + merkle 40 + storage 2) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions**
- **No new ADR** — no divergence from upstream: a test-only iteration closing a negative-path
  *coverage* gap on already-decided behaviour (ADR-0018's all-or-nothing commit). The governing
  decision is unchanged; no source change beyond the test-only `FaultyStore` injector. The design
  point the delete-failure test documents — a swallowed rollback `delete` reports the *root-cause*
  `put` error and preserves *logical* atomicity while leaving an unreachable, later-overwritten orphan
  — is the same "physical reclamation is a separate concern; logical state is the invariant" stance as
  ADR-0024's truncate (no eager block deletion).

**Lessons** (moved to `docs/LESSONS.md`)
- Atomic-commit fault coverage needs the **boundaries and the rollback's own failure**, not just one
  mid-batch fault: a first-block fault leaves `written` empty (the rollback loop must no-op), a
  last-block fault must delete *all* prior writes, and a `delete` that itself fails must still report
  the original `put` error and keep *logical* state atomic — physical cleanup is best-effort, so an
  unreachable orphan (the *encoded* block, length-gated out of reads) can remain and is overwritten on
  the next commit. Assert the recovered head equals a freshly-built prefix head (ed25519 determinism)
  to prove the path lands on the canonical state; assert against the **encoded** orphan bytes, not the
  raw payload (the codec adds a varint length prefix).

**Next**
- The remaining **audit follow-ups** (DEFINITION_OF_DONE), in order: `hypercore` `verify_reorg`
  head-`None` branch (untested); `merkle` reorg / LCA adversarial (corrupt `other`, gapped `self`,
  monotonicity-precondition violation; seek zero-size block); `autobase` quorum-degree *value*
  cross-checked against an independent computation over random DAGs.
- Then resume feature iterations: the gate-#4 JS oracle (still env-blocked — container service not
  startable under the loop's allowlist + image pull needs network; iters 11–21); the wasm runtime /
  IndexedDB gate (#2, needs headless Chrome); the deferred fork/merge consensus (ADR-0015); `hyperbee`.

---

## 2026-06-29 — Iteration 24: `hypercore` `verify_reorg` head-`None` coverage (audit follow-up)

**Did**
- Closed the next **audit follow-up** (DEFINITION_OF_DONE, after iter 21): the `Replica::verify_reorg`
  **head-`None` branch** that the positive reorg tests never reached. A reorg adopts a *strictly
  higher* `fork` than the one we currently trust, so a replica with no verified head has no current
  fork to gate "strictly higher" against and must refuse any reorg, untouched. The behaviour already
  holds (`verify_reorg` returns `false` on `self.head == None` before checking anything else,
  ADR-0026); this iteration is test-only — it pins both ways the branch is reached. 1 asserting test
  (hypercore 35→36):
  - `verify_reorg_requires_a_trusted_head`:
    - **(a) fresh empty replica** (len 0, no head): a higher-fork head is refused even at
      `ancestors == 0` (the from-scratch anchor) — a replica with nothing trusted can't know it is
      moving to a higher fork, so from-scratch replication is `add_block` against a head, *not*
      `reorg`; a valid `ancestors == 1` upgrade-proof offer is likewise refused; nothing is stored.
    - **(b) mid-reorg replica**: after following one reorg (`reorg(head_r, 3, ..)` drops the divergent
      suffix), the replica holds the shared `[0,3)` prefix but `head == None` while the suffix refetch
      is pending. A *second*, even-higher-fork (fork 2) reorg arriving now is refused via the same
      `None` branch — and crucially the replica is **untouched** (len 3, head still `None`) and can
      still finish its **original** pending refetch to `head_r`, ending byte-identical (`get(3)=="X"`,
      `get(4)=="Y"`, root == `head_r.root`). The suffix blocks/proofs are captured from the source
      *before* it is mutated into fork 2 (the fork-2 history no longer holds the fork-1 tail).
- `just verify` green: **112 native tests** (autobase 22 across files + codec 8 + hypercore 36 +
  identity 4 + merkle 40 + storage 2) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions**
- **No new ADR** — no divergence from upstream: a test-only iteration closing a negative-path
  *coverage* gap on already-decided behaviour. The governing decision is ADR-0026 (a reorg is followed
  only from a trusted head at a strictly higher fork; with no head there is no fork to compare, so the
  reorg is refused). No source change.

**Lessons** (moved to `docs/LESSONS.md`)
- A reorg gate keyed on "strictly higher fork than what I trust" is unreachable from a `None` head, so
  the empty-replica and mid-reorg cases both refuse — and the mid-reorg case is the valuable one: a
  replica that dropped its suffix (`head == None`, tree non-empty) must finish its current refetch
  before it can adopt anything new. Capture the in-flight suffix blocks/proofs *before* mutating the
  source onto a higher fork, or the original refetch can no longer be completed from it.

**Next**
- The remaining **audit follow-ups** (DEFINITION_OF_DONE), in order: `merkle` reorg / LCA adversarial
  (corrupt `other`, gapped `self`, monotonicity-precondition violation; seek zero-size block);
  `autobase` quorum-degree *value* cross-checked against an independent computation over random DAGs.
- Then resume feature iterations: the gate-#4 JS oracle (still env-blocked — container service not
  startable under the loop's allowlist + image pull needs network; iters 11–21); the wasm runtime /
  IndexedDB gate (#2, needs headless Chrome); the deferred fork/merge consensus (ADR-0015); `hyperbee`.

---

## 2026-06-29 — Iteration 25: `merkle` reorg / LCA adversarial + seek zero-size (audit follow-up)

**Did**
- Closed the next **audit follow-up** (DEFINITION_OF_DONE, after iter 21): the `merkle`
  reorg / `lowest_common_ancestor` adversarial paths (corrupt `other`, gapped `self`,
  monotonicity-precondition violation) and `seek` over **zero-size blocks** — none exercised today
  (`varied_tree` sizes are always `1..=5`; the LCA/reorg tests all use intact trees). I audited the
  code first and found **no bug**: the LCA binary search is sound under corruption (its invariant —
  `agree(lo)` always true, and `agree(a)` true only when *both* trees produce equal prefix-root-hashes
  at `a` — means a gap can only shrink the result, never over-claim), `reorg` faithfully adopts
  `other`'s node set (intact-other is a documented precondition), and `seek` uses the same `>`
  comparison as a linear scan (so empty blocks are skipped as targets). So this is **test-only**,
  pinning the already-correct behaviour. 4 asserting tests (merkle 40→44):
  - `lca_conservative_under_corruption` — for two trees sharing `[0,6)` of length 8 (intact LCA 6):
    removing `other`'s node 9 (length-6 prefix root of `[4,6)`) shrinks the LCA to a genuine shorter
    shared prefix; removing node 8 (block-4 leaf) makes `agree` **non-monotone** (false at length 5,
    true at length 6) yet the search still returns a length where the prefixes genuinely match; and a
    gap in `self` (symmetric) is equally conservative. Every case asserts `lca <= intact` and that
    `self`/`other` prefix-root-hashes are equal *at the returned length* (a real ancestor, never forged).
  - `lca_intact_agreement_is_monotone` — the binary-search precondition: for two intact trees the
    `agree` vector is monotone (no agreement reappears after the first disagreement), the boundary sits
    exactly at the divergence (`agree[6] && !agree[7]`), and the search is exact (LCA 6).
  - `reorg_precondition_on_intact_other` — `reorg` adopts every node `other` holds: following a
    **corrupt** `other` (suffix node 12 removed) copies the gap verbatim, leaving `self` non-intact
    (intact-other is the precondition); conversely an **intact** `other` **heals** a gapped `self`
    (shared-region node 3 removed) by overwriting the gap with the complete node set — `self` ends
    intact, byte-identical, and every block proves.
  - `seek_handles_zero_size_blocks` — a tree with leading/interior/consecutive/trailing empty blocks
    (`sizes [0,2,0,0,3,1,0]`): `seek` == linear scan for every byte offset, the located block is never
    an empty one (no byte lands in an empty interval), seek proofs authenticate the same mapping, the
    byte at `total` is past the end (no proof), and an **all-empty** tree has no locatable byte.
- `just verify` green: **116 native tests** (autobase 22 across files + codec 8 + hypercore 36 +
  identity 4 + merkle 44 + storage 2) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions**
- **No new ADR** — no divergence from upstream: a test-only iteration closing an adversarial-coverage
  gap on already-decided behaviour. The governing decisions are unchanged — ADR-0025 (local LCA =
  binary search over prefix root hashes; both inputs intact) and ADR-0022 (byte seek). No source change.
  The honest result the corruption tests pin is itself the soundness property: the LCA never
  over-claims because the search only ever returns a length where *both* prefixes are present and
  equal — so a corrupt input degrades to a conservative (shorter, still-genuine) ancestor, not a wrong one.

**Lessons** (moved to `docs/LESSONS.md`)
- The LCA binary search is sound even when its monotonicity precondition is violated by corruption: it
  keeps the invariant that the returned length always satisfies `agree` (both prefix-root-hashes
  present and equal), so a gap can only *shrink* the LCA, never over-claim — a real shorter ancestor,
  never a forged one. `reorg` then heals a gapped `self` from an intact `other` (it overwrites with the
  full node set) but copies a corrupt `other`'s gaps verbatim (intact-other is the precondition). And
  byte seek skips zero-size blocks for free (the `>` comparison matches a linear scan), so an empty
  block is never a seek target and an all-empty tree has no locatable byte — test it explicitly, since
  varied-size fixtures never hit size 0.

**Next**
- The last **audit follow-up** (DEFINITION_OF_DONE): `autobase` quorum-degree *value* cross-checked
  against an independent computation over random DAGs (today only convergence + monotonicity are
  fuzzed, not the degree value).
- Then resume feature iterations: the gate-#4 JS oracle (still env-blocked — container service not
  startable under the loop's allowlist + image pull needs network; iters 11–21); the wasm runtime /
  IndexedDB gate (#2, needs headless Chrome); the deferred fork/merge consensus (ADR-0015); `hyperbee`.

---

## 2026-06-29 — Iteration 26: `autobase` quorum-degree value cross-check (audit follow-up)

**Did**
- Closed the **last audit follow-up** (DEFINITION_OF_DONE, after iter 21): the `autobase`
  quorum-degree *value* was only ever pinned against a handful of hand-worked `DESIGN.md`
  examples, and the convergence sim fuzzes only the *finalized prefix* (convergence +
  monotonicity) — never the **degree value** itself over random graphs. Added
  `crates/autobase/tests/quorum.rs`, a value cross-check against an **independent reference
  oracle**:
  - The oracle is a deliberately *different* algorithm from production. Production
    (`Linearizer::quorum_degree`) is a single bottom-up pass over a topological order
    carrying a per-indexer "best degree" from each node's **strict** dependencies plus a
    hardcoded author self-vote (ADR-0015). The oracle computes every node's degree by a
    **fixpoint relaxation** straight from the `DESIGN.md` recursion over **inclusive** causal
    closures (its own reachability, built from the test's own edge list — never the
    linearizer's private `deps`/`sees`), with the author's self-vote *emergent* (a node is in
    its own closure, so it counts at exactly the levels its current degree already reaches)
    rather than special-cased. Two structurally different routes to the same number ⇒ an
    off-by-one in either the level indexing or the self-vote would diverge.
  - It is **first validated against the `DESIGN.md` worked examples** (chain `a0-b0-c0-a1` ⇒
    3/2/1/0; higher quorum `c0-b0-c1` ⇒ 2/1; competing single quorums ⇒ 1/1) so that using it
    as a cross-check is trustworthy, each example also asserting production == oracle.
  - It is then asserted **equal to `quorum_degree(target)` node-for-node** over seeded random
    partitioned DAGs (the upstream `createDag` model, reusing the convergence sim's SplitMix64
    PRNG) × 3 indexer-set sizes (majorities 2/3/3, incl. a strict subset of writers so
    non-indexing writers are present) × creation order + several randomized-Kahn delivery
    orders (the degree is a pure function of the node set, so all replicas must agree).
    Non-vacuity guards assert degrees 0, 1, and ≥2 all occur and a double quorum forms — so
    the cross-check isn't hollow.
- 2 asserting tests (autobase 22→24 across files: 14 unit + 2 convergence + 2 **quorum** +
  3 topolist + 3 view). `just verify` green: **118 native tests** (autobase 24 + codec 8 +
  hypercore 36 + identity 4 + merkle 44 + storage 2) + wasm build of
  `hypercore`/`autobase`/`storage`.

**Decisions**
- **No new ADR** — no divergence from upstream: a test-only iteration closing a coverage gap
  on already-decided behaviour (ADR-0015's recompute-from-scratch quorum *degree*). The
  governing decision is unchanged; no source change. The oracle is an in-test reference
  computation (like the iter-20 in-Rust `topolist` oracle, ADR-0027), validating the existing
  algorithm host-safely — it complements, does not replace, the env-blocked JS oracle (gate #4).

**Lessons** (moved to `docs/LESSONS.md`)
- To value-check a recursive DP like quorum-degree, write an **independent** oracle from the
  *definition*, not a copy of the algorithm: a fixpoint relaxation over inclusive causal
  closures reaches the same number as production's single-pass topological DP by a different
  route, so an off-by-one in the level indexing or the self-vote is caught. Let the author's
  self-vote be **emergent** (a node is in its own closure) rather than re-hardcoding the
  production `+1`. Validate the oracle against the `DESIGN.md` worked examples *first*, then
  cross-check; and gate non-vacuity (degrees 0/1/≥2 all occur, a double quorum forms) so the
  fuzz isn't hollow. Keep indexer sets ≥ 2 (a lone indexer ⇒ majority 1 ⇒ the production
  degree loop never terminates — a latent degenerate, out of scope here).

**Next**
- All DEFINITION_OF_DONE audit follow-ups are now ticked. Resume feature iterations, all of
  which remain environment-blocked or are larger deferred work:
  - the gate-#4 **JS oracle** (ADR-0008) — still env-blocked (Apple `container` service not
    startable under the loop's scoped allowlist + image pull needs network; iters 11–21);
  - the **wasm runtime / IndexedDB gate (#2)** — needs headless Chrome (`storage` IndexedDB
    backend still `[ ]`);
  - the **deferred fork/merge consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation
    across a resolved fork/merge, which would let `finalized()`/`indexed_view_len()` match
    upstream's earlier-confirming cases (`dags - simple 2`, `linearizer - compete`/`count
    ordering`) and needs the apply/view layer (`apply.js`/`anchors.js`);
  - `merkle` reorg-by-proof / `additionalNodes` (the last `merkle-tree.js` pieces); `hyperbee`.

---

## 2026-06-30 — Iteration 27: `storage` sparse bitfield (`bitfield.js`)

**Did**
- Picked the next natively-testable, in-scope red item. The deferred **fork/merge consensus**
  (ADR-0015, the `[~]` autobase row) is the other non-env-blocked candidate, but LESSONS.md
  explicitly says to defer the 2-degree-lead caveat *to the iteration that has the JS oracle to check
  against* — and gate #4 is still env-blocked — so attempting that subtle clean-room algorithm now,
  with no oracle, would go against the project's own recorded guidance. Instead ported the
  foundational **sparse bitfield** (`reference/js/hypercore/test/bitfield.js`), the local presence map
  `clear`/`purge`/sparse cores will build on.
- Added `crates/storage/src/bitfield.rs` — `storage::Bitfield`, a clean-room (ADR-0001), dependency-
  free, wasm-safe **unbounded sparse bitfield**: `get` / `set` / `set_range(start,end,val)` (end
  exclusive) / `count(start,length,val)` (2nd arg is a **length**, matching upstream) / `find_first` /
  `find_last` (+ `first/last_set/unset`). Storage is a `BTreeMap<page, Box<[u64;512]>>` at upstream's
  `2^15`-bit page granularity (so page/segment **boundary** offsets behave like upstream) but with no
  segment/`BigSparseArray` layer; a **missing page is an all-`false` page** and is never materialized
  just to clear bits (mirrors upstream's `if (!p && val)`). Word-level masks/popcount/`trailing_zeros`/
  `leading_zeros` do the per-page work; `find_first`/`find_last` walk pages via `BTreeMap` ranges.
- 15 asserting tests (storage 2→17): every `bitfield.js` case ported — set/get incl. huge sparse
  index (`8e15`, no panic); `count` (set 8 / unset 10 over the worked example; the length-not-end
  semantics); `find_first`/`find_last` over all-zeroes and all-ones (`set_range(0, 2^24, true)`) at
  page (`2^15±1`, `2^16±1`) and segment (`2^21±1`, `2^22±1`) boundaries; `last_unset` around a page
  boundary (32766/32769); `set_range` on a page boundary; **clear a range on a not-yet-existing page
  allocates nothing**; last-bits-in-segment → `find_first`; `set_range` over multiple pages — plus a
  deterministic (seeded SplitMix64 + reference `HashSet`) random set/get and two brute-force
  cross-checks (`count` and `find_*` vs a naive scan over a sparse mix).
- `just verify` green: **133 native tests** (autobase 24 across files + codec 8 + hypercore 36 +
  identity 4 + merkle 44 + **storage 17**) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0030: implement only the **L1 data structure** of `bitfield.js`, clean-room (own paged layout,
  not byte/disk/wire compatible). **Defer** persistence (`open`/`flush`/`BitInterlude`, `bit-interlude.js`
  — storage backend & disk format, returns with the IndexedDB/native `storage` work) and `want` chunking
  (replication wire framing, networking — ADR-0003) as out of scope per the relevance filter; GC marking
  (`mark-bitfield.js`/`mark-n-sweep.js`) is not yet built. The bitfield row stays `[~]` (structure ported;
  persistence/want/mark deferred).

**Lessons** (moved to `docs/LESSONS.md`)
- A sparse bitfield is an *unbounded, infinite-zero* field; separate the data structure from
  persistence/replication. Store it sparse + paged (one page per touched region, not per index), treat a
  missing page as all-`false` never materialized on clear, and mind the asymmetric `find` semantics:
  `count`'s 2nd arg is a **length**, `find_first(false,..)` **always** finds (infinite-zero tail) while
  `find_first(true,..)`/`find_last(..)` can be absent. Keep the page granularity so boundary offsets
  match upstream; cross-check `count`/`find_*` against a brute-force scan and seed the random test.

**Next**
- All feature iterations from here remain environment-blocked or are larger deferred work:
  - the gate-#4 **JS oracle** (ADR-0008) — still env-blocked (Apple `container` service not startable
    under the loop's scoped allowlist + image pull needs network; iters 11–21);
  - the **wasm runtime / IndexedDB gate (#2)** — needs headless Chrome; the `storage` IndexedDB backend
    `[ ]` would also wire the bitfield's deferred `open`/`flush` persistence;
  - the **deferred fork/merge consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation across a
    resolved fork/merge (LESSONS.md: best done once the JS oracle can cross-check it), which would let
    `finalized()`/`indexed_view_len()` match upstream's earlier-confirming cases; needs the apply/view
    layer (`apply.js`/`anchors.js`);
  - `hypercore` **`clear`** (sparse range-clear over the new `Bitfield`) + `purge`; `merkle`
    reorg-by-proof / `additionalNodes`; `hyperbee`.

---

## 2026-06-30 — Iteration 28: `hypercore` sparse `clear` (presence map over the bitfield)

**Did**
- Picked the natively-testable, in-scope red item iter 27 set up: `hypercore` **`clear`**
  (`reference/js/hypercore/test/clear.js`), built directly on iter 27's `storage::Bitfield`. (The
  other non-env-blocked candidate — the deferred fork/merge consensus, ADR-0015 — LESSONS.md says to
  defer until the JS oracle, gate #4, can cross-check it; that is still env-blocked, so attempting it
  now would defy the project's own recorded guidance.)
- Wired a `storage::Bitfield` **presence map** into `Hypercore` and ported the L1 behaviour of `clear`:
  - `append`/`commit` set the new blocks' presence bits (commit only *after* its writes succeed — so a
    rolled-back failed commit leaves presence untouched, consistent with ADR-0018); `truncate` clears
    the discarded tail's bits.
  - `has(index)` (≡ upstream `has`) = within length **and** bit set; `contiguous_length()` (≡
    `contiguousLength`) = the first absent index capped at the length; `get`/`block` now read **`None`**
    for an absent block (a no-wait read — at L1 there is no peer to wait on), reserving `Error::Corrupt`
    for the genuine "bit set but bytes missing" inconsistency.
  - `clear(start, end)` drops the present blocks in the range — clearing the bit (logical absence) then
    best-effort-deleting the bytes — returning the **count cleared** (`0` ≡ upstream's `null`/no-op). It
    **never touches the Merkle tree**, so the signed head and every block proof are unchanged: a cleared
    block stays authenticated and re-verifiable from a holder.
- 6 asserting tests (hypercore 36→42; workspace 133→139): clear a middle block (presence/`has`/
  `contiguous_length`/`get`-`None`/tree-untouched — upstream `clear`); single-block clear in a 129-block
  log (upstream `incorrect clear`); out-of-range clear is a 0-count no-op (upstream `clear blocks with
  diff option`'s `null`); idempotent/no-side-effect clearing of unknown/absent blocks (upstream `no
  side effect from clearing unknown nodes`); an interior-range clear leaving a hole (small-scale
  analogue of `clear - large cores`); and a cleared block staying authenticated + re-verifiable from a
  holder against the unchanged head (the L1 form of `clear + replication`, no wire). `just verify`
  green: **139 native tests** (autobase 24 + codec 8 + hypercore 42 + identity 4 + merkle 44 +
  storage 17) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0031: `clear` is **presence reclamation over the bitfield**, leaving the Merkle tree / signed head
  untouched (block presence and log structure are two separate axes) — not upstream's storage-stream /
  session layer. `has`/`contiguous_length`/`clear` are the L1 surface; `get`/`block` read `None` for an
  absent block. We **defer** the replication re-download that *refills* a cleared block (networking,
  ADR-0003), `purge` (whole-core deletion = storage backend + session lifecycle, out of scope),
  physical-reclamation guarantees (best-effort: a failed delete still marks the block absent — same
  "logical state is the invariant" stance as ADR-0024), and the commented-out `diff`/byte-API return
  shape. `clear.js` moves `[ ]`→`[~]`; `purge.js` stays `[ ]`. `Replica` is left as-is (implicitly
  fully-present; sparse-replica presence is networking).

**Lessons** (moved to `docs/LESSONS.md`)
- `clear` is *presence* reclamation, not truncation — keep block-presence and log-structure as two
  separate axes. The tree is untouched, so a cleared block stays authenticated and re-fetchable (prove
  it host-safely by verifying a holder-supplied block against the unchanged head). Set presence on
  `append`/`commit` (commit *after* writes succeed) and clear it on `truncate`; `has` = within-length
  ∧ bit-set, `contiguous_length` = first-absent capped at length, `get`/`block` = `None` for absent
  (no-wait), `Corrupt` reserved for bit-set-but-bytes-gone (the bitfield, not storage, owns presence).
  Clearing absent/out-of-range blocks is a no-op; physical reclamation is best-effort.

**Next**
- All remaining feature iterations are environment-blocked or larger deferred work:
  - the gate-#4 **JS oracle** (ADR-0008) — still env-blocked (Apple `container` service not startable
    under the loop's scoped allowlist + image pull needs network; iters 11–21);
  - the **wasm runtime / IndexedDB gate (#2)** — needs headless Chrome; the `storage` IndexedDB backend
    `[ ]` would also wire the bitfield's deferred `open`/`flush` persistence (and persist this presence
    map);
  - the **deferred fork/merge consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation across a
    resolved fork/merge (LESSONS.md: best done once the JS oracle can cross-check it), which would let
    `finalized()`/`indexed_view_len()` match upstream's earlier-confirming cases; needs the apply/view
    layer (`apply.js`/`anchors.js`);
  - the replication re-download that refills a cleared block + `purge` (both deferred this iter);
    `merkle` reorg-by-proof / `additionalNodes`; `hyperbee`.

---

## 2026-06-30 — Iteration 29: `hypercore` snapshots (`snapshots.js`)

**Did**
- Picked the next natively-testable, in-scope red item: `hypercore` **snapshots**
  (`reference/js/hypercore/test/snapshots.js`). (The other non-env-blocked candidate — the deferred
  fork/merge consensus, ADR-0015 — LESSONS.md says to defer until the JS oracle, gate #4, can
  cross-check it; gate #4 is still env-blocked, so attempting it now would defy the project's own
  recorded guidance.)
- Added a **self-contained, by-value** `Snapshot<T, C>` + `Hypercore::snapshot()`. A snapshot owns an
  immutable copy of the present blocks `[0, len)` (encoded bytes), the `MerkleTree` at that length, the
  captured `SignedHead`, and a clone of the codec — so it observes the log exactly as it was when taken
  and is **immune to any later mutation of the core** (append / truncate / truncate-and-rewrite). Its
  surface: `length()`/`fork()`/`is_empty()`/`head()`/`root_hash()` (all fixed), `block(i)` (raw encoded
  bytes, `None` past the length or for an absent block), `get(i)` (decode; `None` past the end — our L1
  form of upstream's out-of-range `SNAPSHOT_NOT_AVAILABLE`), `proof(i)` (inclusion proof against the
  captured head, so a snapshot block is independently authenticated), and `signed_length(&core)` ≡
  upstream `snapshot.signedLength` computed content-blind as the shared-prefix LCA
  (`MerkleTree::lowest_common_ancestor`) of the snapshot's tree and the core's *current* tree.
- The two built-in codecs (`codec::U64`/`Bytes`) gained `#[derive(Clone, Copy, PartialEq, Eq, Default)]`
  so a snapshot can own a codec to decode with — a Rust-ergonomics detail (zero-sized config types), not
  a behavioural change.
- 4 asserting tests (hypercore 42→46; workspace 139→143):
  - `snapshot_is_immune_to_truncate_and_rewrite` — the headline `snapshots.js` case: snapshot at len 3,
    then the core appends Block3, `truncate(3)`, `truncate(2)`, re-appends "new Block2"; the snapshot's
    `length` stays 3 and `get(2)` stays "block2" throughout, `signed_length` goes 3→3→3→2→2 (drops once
    the core truncates below 3, and stays at 2 after the divergent re-append), and a read over the
    snapshot yields the three snapshotted blocks (L1 `createReadStream`).
  - `snapshot_block_is_independently_authenticated` — after a fork (truncate-and-rewrite, fork 0→1) of
    the core beneath it, every captured block still verifies against the snapshot's own head with the
    snapshot's own proof, the snapshot keeps the old block 2, and `signed_length` collapses to the
    shared 2-block prefix (host-safe form of "snapshots are consistent").
  - `empty_and_static_snapshots` — an empty snapshot stays empty (no head); a snapshot at len 2 stays
    static as the core grows; in-range `get` decodes, out-of-range `get`/`block` are `None`;
    `signed_length` is 0 / 2 respectively ("snapshots wait for ready" minus async-ready/persistence).
  - `snapshot_is_independent_of_clear` — clearing the core's presence map afterwards doesn't affect the
    by-value snapshot (it still has every block); clear leaves the tree untouched, so the prefix is
    still fully shared (`signed_length == len`).
- `just verify` green: **143 native tests** (autobase 24 across files + codec 8 + hypercore 46 +
  identity 4 + merkle 44 + storage 17) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0032: a snapshot is a **self-contained by-value point-in-time view**, not upstream's shared-storage
  copy-on-write disk model. We copy the snapshotted prefix's bytes (identical observable behaviour,
  simplest L1 way to survive a truncate-and-rewrite); `signed_length` is the content-blind shared-prefix
  LCA against the live core; out-of-range `get` is `None` (our `SNAPSHOT_NOT_AVAILABLE` analogue). We
  **defer** `signedLength`-over-replication, implicit per-call snapshotting during a live download,
  `createReadStream` (`streams.js`), and the session/atom cases (sessions/networking, out of scope).
  `snapshots.js` moves `[ ]`→`[~]`. The codec `Clone, Copy` derives are an ergonomics detail, not a
  divergence.

**Lessons** (moved to `docs/LESSONS.md`)
- A snapshot that must survive truncate-and-rewrite is a **by-value** copy (own the prefix's bytes +
  tree + head + codec), not a shared-storage view — then nothing the core does later can change what it
  reports. Three consequences fall out: out-of-range `get` is `None` (no-wait), a captured block is
  independently authenticated against the captured head, and `signedLength` is just the content-blind
  shared-prefix LCA of the snapshot's tree and the core's current tree (never exceeds the snapshot
  length; drops on truncate-below / in-prefix rewrite). Zero-sized config codecs only need
  `derive(Clone, Copy)` to be owned for decoding.

**Next**
- All remaining feature iterations are environment-blocked or larger deferred work:
  - the gate-#4 **JS oracle** (ADR-0008) — still env-blocked (Apple `container` service not startable
    under the loop's scoped allowlist + image pull needs network; iters 11–21);
  - the **wasm runtime / IndexedDB gate (#2)** — needs headless Chrome; the `storage` IndexedDB backend
    `[ ]` would also wire the bitfield's deferred `open`/`flush` persistence (and persist the presence
    map / snapshots);
  - the **deferred fork/merge consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation across a
    resolved fork/merge (LESSONS.md: best done once the JS oracle can cross-check it), which would let
    `finalized()`/`indexed_view_len()` match upstream's earlier-confirming cases; needs the apply/view
    layer (`apply.js`/`anchors.js`);
  - more natively-testable rows: `hypercore` `move-to.js` / `streams.js` (the rest of the
    snapshot/seek/stream family); `merkle` reorg-by-proof / `additionalNodes`; `manifest.js`
    (signing config); the replication re-download that refills a cleared block + `purge`; `hyperbee`.

---

## 2026-06-30 — Iteration 30: `hypercore` read / byte streams (`streams.js`)

**Did**
- Picked the next natively-testable, in-scope red item: `hypercore` **streams**
  (`reference/js/hypercore/test/streams.js`), reusing the existing `get`/`block`/`seek`. (The other
  non-env-blocked candidate — the deferred fork/merge consensus, ADR-0015 — LESSONS.md says to defer
  until the JS oracle, gate #4, can cross-check it; gate #4 is still env-blocked, so attempting it now
  would defy the project's own recorded guidance.)
- Added two synchronous **iterator**-based streams to `Hypercore` (+ their option structs):
  - `read_stream(ReadStreamOptions { start, end, reverse, live })` → `ReadStream`, yielding
    `Result<T, Error>` for each **present** block in `[start, end)`, forward or `reverse`. `end` defaults
    to and is clamped to `len()`. It is **no-wait** (like `get`): an absent block — never downloaded or
    dropped by `clear` — is *skipped*, not waited on (no peer at L1). `live` is accepted but **ignored**
    (no async tail), so upstream's "live should be ignored" ports directly.
  - `byte_stream(ByteStreamOptions { byte_offset, byte_length })` → `ByteStream`, yielding
    `Result<Vec<u8>, Error>` of whole **encoded** blocks covering `[byte_offset, byte_offset+byte_length)`:
    `seek(byte_offset)` locates the start block, whole blocks are emitted until the byte budget is consumed
    (`byte_length` defaults to the rest of the log), and an empty-payload block is still emitted while the
    budget is non-zero. Offsets address the **encoded** byte layout the tree authenticates (the `padding`
    divergence, ADR-0022) — a consumer subtracts its own framing.
  - `createWriteStream` is buffered `append` (no new L1 behaviour), so it gets no type — covered by
    append/batch.
- 5 asserting tests (hypercore 46→51; workspace 143→148): `read_stream_basic_and_range` (whole log /
  `start` / `start`+`end` / reverse / empty range / out-of-range `end` clamped — upstream "basic read
  stream" + "read stream with start / end" + "basic write+read stream"); `read_stream_end_ignores_live`
  (upstream "read stream with end and live (live should be ignored)"); `read_stream_skips_cleared_holes`
  (the no-wait consequence — a `clear`ed hole is skipped in both directions); `byte_stream_basic_and_ranges`
  (whole log + the byteOffset/byteLength cases + past-the-end yields nothing — offsets derived from the
  encoded `block(i).len()`); `byte_stream_yields_empty_payload_blocks` (upstream "basic byte stream w/
  empty buffers" — every block in range emitted, incl. empty payloads). `just verify` green: **148 native
  tests** (autobase 24 across files + codec 8 + hypercore 51 + identity 4 + merkle 44 + storage 17) + wasm
  build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0033: read/byte streams are **no-wait L1 iterators**, not async backpressured duplex streams. Byte
  addressing is over the **encoded** layout the tree authenticates (padding-free, ADR-0022), not upstream's
  value-byte layout. `live` is accepted-but-ignored; absent blocks are skipped. We **defer**: `live`
  tailing (async/networking), duplex backpressure (Node runtime), sub-block byte slicing of a non-boundary
  offset + `padding`, the `createWriteStream` object (= buffered `append`), and the snapshot/session-bound
  stream variants (sessions/networking). `streams.js` moves `[ ]`→`[~]`; `move-to.js` (the move-to op)
  remains.

**Lessons** (moved to `docs/LESSONS.md`)
- A read/byte stream at L1 is a **no-wait iterator over what you locally have**, not an async tail: the
  read stream yields decoded blocks over `[start,end)` (forward/reverse, `end` clamped to `len`); the byte
  stream `seek`s the start block then emits whole encoded blocks until the byte budget is spent (empty-
  payload blocks still emitted while budget > 0). `live` has nothing to tail ⇒ accept-and-ignore it (the
  "live should be ignored" case passes by construction); absent (`clear`ed) blocks are skipped (matching
  `get`'s `None`). Keep byte offsets over the **encoded** layout (ADR-0022) and derive test offsets from
  `block(i).len()`. `createWriteStream` is just buffered `append` — no new L1 behaviour.

**Next**
- All remaining feature iterations are environment-blocked or larger deferred work:
  - the gate-#4 **JS oracle** (ADR-0008) — still env-blocked (Apple `container` service not startable
    under the loop's scoped allowlist + image pull needs network; iters 11–21);
  - the **wasm runtime / IndexedDB gate (#2)** — needs headless Chrome; the `storage` IndexedDB backend
    `[ ]` would also wire the bitfield's deferred `open`/`flush` persistence (and persist the presence
    map / snapshots);
  - the **deferred fork/merge consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation across a
    resolved fork/merge (LESSONS.md: best done once the JS oracle can cross-check it), which would let
    `finalized()`/`indexed_view_len()` match upstream's earlier-confirming cases; needs the apply/view
    layer (`apply.js`/`anchors.js`);
  - more natively-testable rows: `hypercore` `move-to.js` (the move-to operation + write-stream object);
    `merkle` reorg-by-proof / `additionalNodes`; `manifest.js` (signing config); the replication
    re-download that refills a cleared block + `purge`; `hyperbee`.

---

## 2026-06-30 — Iteration 31: `hypercore` prologue migration / move-to (`move-to.js`)

**Did**
- Picked the next natively-testable, in-scope red item from iter 30's "Next": `hypercore` **move-to**
  (`reference/js/hypercore/test/move-to.js`). (The other non-env-blocked candidate — the deferred
  fork/merge consensus, ADR-0015 — LESSONS.md says to defer until the JS oracle, gate #4, can cross-check
  it; gate #4 is still env-blocked, so attempting it now would defy the project's own recorded guidance.
  `manifest.js` is the full multi-signer `Verifier`/`multisig` feature — much larger; its one prologue-
  relevant piece is exactly what move-to needs, ported here. `additionalNodes` was already noted (ADR-0025)
  to add no L1 capability our standalone `upgrade_proof` lacks.)
- Distilled the L1 behaviour-under-test — **migrate a log's history onto a new identity** — into a
  content-addressed prologue primitive (no manifest/multisig/session wrapping):
  - `Prologue { length, hash }` — a commitment naming a prefix by its **Merkle hash**.
    `Hypercore::prologue_at(length)` mints one from a source (`{ length, prefix_root_hash(length) }` ≡
    upstream's `{ length, core.state.hash() }`); `with_prologue(author, codec, store, prologue)` creates a
    fresh core under a *new* key bound to it; `prologue()` accessor.
  - `copy_prologue(source)` ≡ upstream `copyPrologue`: it **content-checks before copying**
    (`source.prefix_root_hash(length) == hash`, so a non-matching source leaves the target untouched),
    copies the committed prefix in **by value** (ADR-0032's snapshot stance), rebuilds an identical prefix
    tree, marks each block present, and **re-signs the prefix under the new key** (a head at `length`,
    fork 0). Returns the migrated length; errors `NoPrologue` (no commitment to satisfy) / `PrologueMismatch`
    (non-empty target, too-short source, missing prefix block, or a content mismatch).
  - `verify_prologue()` is the maintained invariant (`prefix_root_hash(length) == hash`), and the prologue
    length is a **`truncate` floor** (a prologue-bound core refuses to rewind into the committed prefix).
- 4 asserting tests (hypercore 51→55; workspace 148→152):
  - `move_to_basic_preserves_prefix_under_new_key` — the "move - basic" headline: [1,2,3] migrated onto a
    new key, prefix byte-identical (raw bytes + decoded values), the migrated head signed by the **new** key
    and **not** the source's, then append('4') on top; every block (prefix + new) authenticates against the
    new head.
  - `copy_prologue_is_content_addressed_and_rejects_mismatch` — a **cross-author** source with the same
    prefix content backs the prologue (content-addressed, not identity-bound); a diverging source / a
    too-short source / a core with no prologue / a non-empty target are each rejected leaving the core
    untouched.
  - `move_to_after_truncate_with_surviving_snapshot` — the "move - snapshots" case: the source is
    truncate-and-rewritten ([hello,world,again] → [hello,break], fork 1) then migrated; a snapshot taken
    *before* the rewrite still returns its own three blocks (by-value immunity, iter 29 — `moveTo` on a
    snapshot is a no-op at L1).
  - `prologue_is_a_truncate_floor` — truncate above / exactly at the floor is allowed; below it is refused,
    keeping `verify_prologue` an invariant.
- `just verify` green: **152 native tests** (autobase 24 across files + codec 8 + hypercore 55 + identity 4 +
  merkle 44 + storage 17) + wasm build of `hypercore`/`autobase`/`storage`.

**Decisions** (see `docs/DECISIONS.md`)
- ADR-0034: prologue migration is a **content-addressed prefix commitment + a by-value copy re-signed under
  a new key**, not upstream's manifest-embedded multisig prologue / shared-storage `copyPrologue` / session-
  level `moveTo`. We **diverge** by copying by value (vs shared storage) and re-signing the prefix under the
  new key (vs manifest self-authorization) — observably identical. We **defer**: the full multi-signer
  manifest + `Verifier`/`multisig` and the manifest-hash-into-key identity binding (`manifest.js`, still
  `[ ]`), and the session-level `moveTo`/`migrate` re-homing + `createWriteStream` object (sessions/
  networking, out of scope). `move-to.js` moves `[ ]`→`[~]`.

**Lessons** (moved to `docs/LESSONS.md`)
- A log migration is a content-addressed prefix commitment + a by-value copy under a new key — strip the
  manifest/multisig/session wrapping. The `Prologue { length, hash }` names the prefix by its Merkle hash
  (content-addressed: any holder of the same prefix content backs it, regardless of author — the "head at a
  length is a pure function of the first `length` blocks" property again); `copy_prologue` content-checks
  before copying (verify-then-mutate), copies by value (ADR-0032), and re-signs the prefix under the new key
  (observably identical to upstream's manifest self-authorization, keeping `verify_head`/`verify_block`
  uniform). Two consequences: the prologue length is a `truncate` floor (keeping `verify_prologue` an
  invariant), and the snapshot-`moveTo` case is a no-op at L1 (a by-value snapshot is already immune to the
  source's mutation).

**Next**
- All remaining feature iterations are environment-blocked or larger deferred work:
  - the gate-#4 **JS oracle** (ADR-0008) — still env-blocked (Apple `container` service not startable under
    the loop's scoped allowlist + image pull needs network; iters 11–21);
  - the **wasm runtime / IndexedDB gate (#2)** — needs headless Chrome; the `storage` IndexedDB backend `[ ]`
    would also wire the bitfield's deferred `open`/`flush` persistence (and persist the presence map /
    snapshots);
  - the **deferred fork/merge consensus** (ADR-0015) — the 2-degree-lead caveat + confirmation across a
    resolved fork/merge (LESSONS.md: best done once the JS oracle can cross-check it), which would let
    `finalized()`/`indexed_view_len()` match upstream's earlier-confirming cases; needs the apply/view layer
    (`apply.js`/`anchors.js`);
  - more natively-testable rows: `manifest.js` (the full multi-signer `Verifier`/`multisig` config, of which
    the prologue piece is now done); `merkle` reorg-by-proof / `additionalNodes`; the replication re-download
    that refills a cleared block + `purge`; `hyperbee`.
