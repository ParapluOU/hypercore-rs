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
