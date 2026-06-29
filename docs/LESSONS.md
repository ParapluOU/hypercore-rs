# Lessons

Reusable gotchas discovered while porting. Append as you learn. Keep them general — **no private or
personal data** (this repo is public; use repo-relative paths).

- **We have upstream tests to port.** Unlike a from-C++ render port with only output-equivalence,
  here the upstream JS test suites are behavioural specs. Porting a test often clarifies the API we
  should expose before writing the implementation — port the test first, then make it pass.
- **Order by causality, never by a self-reported clock.** Autobase orders by the reference DAG +
  deterministic tiebreak + quorum, not by timestamps — that is what makes forged "append times" a
  non-attack. If our linearizer ever reads a wall-clock or a self-reported scalar to decide order,
  that is a bug. (See `reference/js/autobase/DESIGN.md`.)
- **Keep `T` out of L1.** If ordering/verification code needs to look inside a payload, domain
  semantics have leaked into the transport — stop and rethink the boundary.
- **Never run the JS reference on the host.** npm supply-chain exploits are common; `reference/js/*`
  and its dependency tree are untrusted. Read the JS to port it; only *execute* it inside
  `scripts/node-sandbox.sh` (container, install-scripts disabled).
- **Port the algorithm from `DESIGN.md`, not the optimized data structure.** Upstream's `topolist.js`
  is an incremental insertion-sort with `undo`/`shared` patch-tracking — machinery for *streaming
  view updates*, not the ordering definition. The definition is in `DESIGN.md`: topological order,
  tie-broken by lowest writer key then seq. A plain priority-Kahn (emit the smallest causally-ready
  `NodeId`) reproduces every canonical `DESIGN.md` linearization and makes determinism obvious
  (pure function of the node set). Reach for the clever incremental structure only if a benchmark
  demands it.
- **Most upstream `autobase` test assertions are about the *confirmed* prefix, not raw order.**
  `linearizer.js`/`dags.js` mostly assert `getIndexedViewLength` / `view.get` — those depend on
  indexer **quorum**, a separate capability. The pure-ordering behaviour they exercise is best
  pinned by the explicit `a<b<c` DAGs in `DESIGN.md`; defer the view-length assertions to the quorum
  iteration rather than dragging the whole base/replicate/confirm harness into an L1 ordering test.
- **Derive the tiebreak into the key type.** Making `NodeId` `Ord` as `(key, seq)` means a
  `BTreeSet` frontier *is* the "lowest key wins" tiebreak — no separate comparator, and arrival
  order can't leak in.
- **Quorum degree is a clean bottom-up recursion; don't port the incremental machine.** Upstream's
  `consensus.js` streams confirmation with vector clocks; the *definition* in `DESIGN.md` is a simple
  recursion — a node has degree `k` once a majority of indexers reference a degree-`(k-1)` quorum
  over it. One topological pass carrying "best degree per indexer in this node's causal closure"
  reproduces every worked `DESIGN.md` example exactly. The one subtlety: **a node's own author
  vouches every level up to that node's own degree**, so when counting voters for level `k-1` add the
  author's `+1` — it is sound because you only test level `k-1` after the degree is already confirmed
  `≥ k-1`. Verify the recursion against the `DESIGN.md` "1'/2'/3' quorum" chain by hand before trusting it.
- **Finalize conservatively, then prove stability as a property.** A double quorum alone is *not*
  safe to finalize in the presence of a competing fork (the `DESIGN.md` caveat) — and the fully
  general rule is the whole consensus algorithm. The honest single-iteration move: finalize only the
  snapshot/no-active-fork prefix (double quorum **and** comparable-to-every-node), which is provably
  safe (refusing to commit is always safe) and still confirms the common chain case. Assert
  finality-*stability* directly (a finalized prefix only ever extends under cooperative growth) rather
  than claiming the full fork/merge rule. Defer the 2-degree-lead caveat to the iteration that has the
  JS oracle to check against.
