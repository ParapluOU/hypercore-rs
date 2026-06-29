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
- **A convergence/fuzz sim needs a *seeded* PRNG, not the platform RNG.** Use a tiny inline
  deterministic generator (SplitMix64 is ~5 lines, no deps) so a failing case reproduces forever — it
  *is* the repro, which is what upstream's "format a failing DAG to a JS file" machinery exists to
  provide. Drive delivery-order variety with a **randomized-Kahn** topological sort (pick a random
  causally-ready node each step); every output is a valid causal delivery order.
- **Convergence (a pure function of the node set) holds under arbitrary partitions; conservative
  *finality* does not.** `order()`/`finalized()` are pure functions of the DAG, so any delivery order
  agrees — assert that everywhere. But the conservative `finalized()` (comparable-to-every-node) can
  legitimately *shrink* when a late concurrent node strands a previously-finalized node, so assert
  *monotonic, never-reordering* growth only under **cooperative** generation (each node references all
  current tails ⇒ a total order, no stranding). Asserting strict monotonicity on a partitioned DAG
  would be testing a property the conservative form is honestly allowed to violate (the deferred
  fork/merge gap, ADR-0015/0016), not a real bug.
- **Keep the sim L1.** "Application state" in a domain-agnostic convergence test is just an
  order-sensitive checksum of the emitted `NodeId`s (a rolling FNV fold) — equal iff the orders are
  equal. No payload, no domain type; it stands in for "replicas folded the same ops to the same state".
- **Multi-leaf (range) Merkle proof soundness = path nodes and sibling nodes never mix roles.** Recompute
  *every* on-path node from the block data; treat the proof's supplied nodes purely as **off-path
  siblings** (look them up by index, and prefer a recomputed node when both exist) so a forged node can
  never impersonate a leaf's ancestor; and **force every recomputed leaf up to a genuine root index** —
  a missing sibling is a rejection, not a silent skip. The trap to avoid: if a recomputed leaf is allowed
  to *not* connect to a root, a prover can hand you the real roots plus bogus data that is never checked
  against them, and verification passes. Generate the proof with the *same* traversal the verifier uses
  (a depth-by-depth climb over a frontier set) so both sides agree on the boundary node set by construction.
- **A touched root's proof-supplied hash is dead weight.** A range/inclusion proof substitutes every root
  the range reaches with a node recomputed from the data, so tampering that root's supplied copy has no
  effect (by design — its integrity comes from the leaves). Tamper-rejection tests must therefore mutate an
  **untouched** root (or a block, or a boundary node) — mutating a substituted root tests nothing.
- **Atomic multi-step commit over a fallible store: fallible writes first, mutate state last.** To make an
  N-block commit all-or-nothing, write every block to storage *before* touching the in-memory source of
  truth (the Merkle tree + signed head), and roll back the writes already made if any fails. Then a partial
  storage failure can never advance the log's logical state. Test it with a **fault-injecting store**
  wrapper that fails the `put` at a chosen key — assert length/head/reads are untouched, no orphan blocks
  remain, *and* a fault-free retry recovers. A happy-path-only test never exercises the rollback branch.
- **Fork detection is two L1 primitives, not a replication event.** Upstream surfaces a forking writer
  via a `'conflict'` event mid-replication, but the *provable* fact is content-blind and needs no
  network: (1) two signed heads of **equal length but different root** from one author are a fork on
  their own (the head at a length is a deterministic pure function of the first `length` blocks — two
  roots ⇒ two histories), so a proof-free `conflicting_heads` is the first-notice detector; (2) for
  forks across **different** lengths (truncate-and-rewrite), pin the disagreement to a **shared block
  index** — two inclusion proofs at the same index, both signed by the author, committing different
  bytes (reuse `verify_block` per side). Soundness is just leaf collision-resistance (different bytes ⇒
  different committed leaf) — the assumption the Merkle scheme already makes. Gotcha when writing the
  tamper tests: a block that *is* a root has an **empty** sibling list (e.g. block 4 in a 5-block log =
  leaf 8 = a root), so to exercise a "tampered sibling" case pick a fork index whose inclusion proof is
  interior (diverge at index 1 of a ≥4-block log), not the last block.

- **A Merkle length-extension proof is a data-free consistency proof — and its soundness is "supplied
  nodes must be fully new".** To prove tree-at-`new` is an append-only extension of tree-at-`old` (the
  cross-length anti-fork check), you don't need block data: supply only the **fully-new** subtree nodes
  (every covered block `>= old`) and have the verifier fold them into *its own trusted old roots* to
  rebuild the new roots, then check `tree_hash(new_roots) == new_head_hash`. The whole guarantee hinges
  on the verifier **rejecting any supplied node that isn't fully new** (a straddling or fully-old node).
  If you let the prover supply a straddling node (e.g. a new *root* that also covers old blocks) directly,
  it bypasses the fold from the trusted old prefix and a rewritten-old-block fork sails through — the
  verifier never re-derives that root from old material. Generate the proof with the *same* descent the
  verifier climbs (walk down from each new root, stop at old roots, emit the largest fully-new subtrees),
  exactly as with range proofs, so the two agree on the node set by construction. Require `old >= 1`: an
  `old = 0` "upgrade" has no trusted anchor and proves nothing. Test the anti-fork arm explicitly — a
  verifier holding the *honest* prefix must **reject** a forked longer head even though that proof is
  internally consistent against the *forked* old roots.
- **An inclusion proof ties a block to *a* head, not to the replica's history — a longer head needs a
  separate consistency gate.** When a replica fetches a longer head's blocks, each block's inclusion proof
  verifies against *that head's* root — so a forking writer's self-consistent longer head has every block
  verify, and the replica would silently adopt a forked history it had already contradicted. Tie the new
  head back to trusted state **before downloading**: fold a data-free `UpgradeProof`'s fully-new nodes into
  the replica's *own* roots and require it to rebuild the new head's root (`Replica::verify_upgrade`). It's
  cheap (no block data) and catches the fork before a single new block is fetched — the cross-length
  analogue of `conflicting_heads`/`ForkProof`. The empty replica (length 0) has no anchor, so it has no
  upgrade gate; it replicates from scratch against the head directly.
- **A byte-offset seek proof is an inclusion proof read for *sizes*, not data.** To prove "byte `X` lands
  in block `B`" against the signed root, you don't need the block's bytes — you need its *position* and the
  *cumulative byte size* of everything to its left, both already authenticated by the tree. Ship the target
  leaf node + its inclusion siblings + the roots (the same shape as a block proof, minus the data). The
  verifier climbs the leaf to its root with `parent_hash` (which binds each child's hash **and** size) and
  checks `tree_hash == expected_root`, which authenticates every size on the path; then the block's
  left-cumulative size is just the sum of the **left** siblings' sizes met while climbing plus the sizes of
  the roots left of the containing root, and the offset is in the block iff `cumulative <= X < cumulative +
  leaf.size`. Because the blocks' byte intervals are disjoint and contiguous, exactly one block brackets `X`,
  so a forged-block proof fails the bracket (its authenticated sizes won't contain `X`) — no separate
  "is this the right block" check needed. Tamper tests: a tampered `bytes` field that stays *within the
  proven block* is still a correct proof for that offset (not an attack) — to test rejection, move `bytes`
  into a *different* block's interval. And the local tree-accelerated seek (descend by subtree `size`) must be
  asserted equal to a naive linear scan for **every** offset — that equivalence is the whole point of the
  O(log n) descent. Keep `padding`/framing out of L1: it's application byte-layout, not log structure.
- **Tree-node recovery is an inclusion proof that starts from an *arbitrary node*, not a leaf.** Upstream
  surfaces a tree with deleted nodes via `_repairMode` + replication-driven repair (peer requests,
  `repairing`/`repaired` events), but the *provable* L1 fact is content-blind and needs no network: a peer
  that trusts the signed root can recover any missing node from a `NodeProof` — the node itself + its sibling
  path to the containing root + the roots — by climbing it to the root with `parent_hash` (binds hash **and**
  size) and requiring `tree_hash(roots) == expected_root`. It's the arbitrary-node generalization of the
  block `Proof` (which starts from a leaf recomputed from data); soundness is the same leaf/parent
  collision-resistance. Two gotchas: (1) **repair mode is derivable, not a flag** — a node is implied by the
  length iff its whole block range is within `[0, len)`, so `missing_nodes`/`is_intact`/the append guard all
  fall out of the length with no stored bit to keep in sync; make `root_hash`/`roots` panic-free
  counterparts (`try_*` → `None`) so a gap degrades gracefully instead of indexing into a missing key. (2)
  **recovery must be verify-then-store** so a mangled proof leaves storage untouched (atomic) — and the
  corrupt *source* can't prove the node it lost (`node_proof` needs the node present), so proofs always flow
  from a healthy holder into the gap.
- **The minimal-dependency JS oracle is the bare `topolist.js`.** Upstream's `Linearizer` drags in the whole
  writer/core/consensus/clock object graph (and `batch.js`/`dags.js` drive the *full* native stack —
  sodium, hypercore, corestore). But the actual *ordering* producer (ADR-0014) is `lib/topolist.js`, which
  needs only `b4a.compare` + `nanoassert` and synthetic node objects (`writer.core.key`, `length`,
  `dependencies`/`dependents`, `index`; `optimistic`/`yielded` false) — no clock/consensus. Inject the two
  trivial deps via Node's `Module._compile` over the reference source (no npm, no network), feed nodes to
  `Topolist.add` in causal order, and compare `.tip` to our `order()`. Precondition the JS reference can't
  remove: a **started** container runtime (`container system start`) — the sandbox only does `container run`.
- **Truncate is "rewind to a prefix": keep nodes fully inside `[0, new_len)` and the root is the
  prefix's root for free.** A flat-tree truncation needs no recomputation — `retain` only the nodes
  whose whole block range is `< new_len` and what remains is byte-for-byte the tree a fresh prefix
  builds (the surviving blocks were never touched, so every kept hash already matches). So
  `root_hash()` after truncate == the fresh prefix's root, which is the same "head at a length is a pure
  function of the first `length` blocks" property fork detection rests on. The signed **fork counter**
  is what makes truncate a first-class, non-equivocating op: bind it into the head message and an
  *equivocation* becomes a **same-fork** contradiction — two heads at different forks are a legitimate
  reorg the writer performed (readers follow the highest fork), so `conflicting_heads`/`ForkProof` must
  require equal forks. Two framing gotchas: (1) `byte_length` is the **encoded** (stored) prefix size
  the tree commits to, not raw payload length (the codec adds a varint length prefix), so assert it
  against a freshly-built prefix core, not hardcoded byte counts; (2) don't eagerly delete the truncated
  blocks from storage — they're unreachable (`get`/`block` gate on the length) and overwritten on
  re-append, so a pure in-memory tree+head mutation keeps truncate atomic and infallible (physical
  reclamation is a separate `clear`/`purge` capability).
- **The lowest common ancestor of two trees is a binary search over prefix root hashes — no payload
  peek, no node-by-node descent.** Upstream's `ReorgBatch` narrows the divergence top-down (find the
  topmost differing root, then descend) because the wire protocol forces it to fetch nodes one round at
  a time. With both full trees in memory you don't need that: two trees agree on blocks `[0, a)` **iff**
  their root-hash-at-length-`a` are equal (the head at a length is a pure function of the first `length`
  blocks — the same property truncate and fork-detection rest on), and prefix agreement is **monotone**
  (agree at `a` ⟹ agree at every `a' < a`), so the LCA is just the largest `a ≤ min(len)` where the
  prefix root hashes match — a clean binary search comparing only authenticated hashes. `reorg` then =
  `truncate(lca)` (keep the shared prefix — it's preserved, not re-derived, since the surviving nodes
  already equal the other tree's prefix) + adopt the other's suffix nodes, ending byte-identical. Keep
  it **fork-agnostic** (it reorganizes tree nodes); *which* tree to follow — the signed head + fork
  counter — is the hypercore layer's job, the cross-fork analogue of wiring `UpgradeProof` into
  `Replica::verify_upgrade`. Test gotcha: reorg always makes the local follow the remote (up **or** down
  to the remote's length), so the post-reorg target is always the remote — don't branch on "which is
  longer".
- **Upstream's topolist insertion sort and our priority-Kahn both compute the *lex-minimal linear
  extension* — so they're equal, and you can prove it host-safely with an in-Rust oracle.** `topolist.js`'s
  real assertion is **stable ordering**: same DAG ⇒ same order regardless of arrival order. Upstream gets
  there with an incremental insertion sort (`moveDown` slides a new node to its causal floor — the spot
  right after the last node it directly depends on — then `moveNonOptimisticUp` slides it back past every
  *strictly smaller* node, where "smaller" is causal-first then (writer key, seq)). We get there with a
  priority-Kahn that emits the smallest causally-ready node. Both land on the **unique** lexicographically-
  minimal linear extension under (key, seq), so they agree node-for-node — even though the per-pair
  comparison "causal-or-(key,seq)" is *not* transitive (a causal edge can cross the key tiebreak, making a
  ≺-cycle), because "always take the minimum *available* node" sidesteps the non-transitivity. You don't
  need the container/JS oracle to check this: transliterate the *non-optimistic* insertion sort into a
  test-only Rust oracle (its `links(a,b)` is just "b ∈ a's direct deps", where direct deps = explicit heads
  ∪ same-writer predecessor — the same union `Linearizer::add` builds and upstream's `links` recognizes) and
  assert it equals `order()` over the `DESIGN.md` DAGs + seeded random fork/merge DAGs × several delivery
  orders. This is a host-safe *complement* to the env-blocked gate #4, not a replacement (gate #4 runs the
  actual reference JS). Keep `undo`/`shared`/`flush` (live-view patch tracking) and optimistic nodes out —
  they're not the ordering definition.
- **A replica follows a reorg by re-anchoring the upgrade proof on a *proper prefix* of its own
  history — and the claimed ancestor authenticates itself.** The same-fork length-extension gate
  (`verify_upgrade`) folds the peer's new nodes onto the replica's *entire* current head; a reorg is
  the cross-fork case (the author rewound + rewrote under a bumped `fork`), so the shared part is only
  a prefix `[0, ancestors)`. Reuse the exact same data-free `UpgradeProof`, but anchor it on the
  replica's roots *at `ancestors`* (`prefix_roots`) instead of its full roots — because the head at a
  length is a pure function of the first `length` blocks, those roots equal the source's iff the
  prefix is genuinely shared. Consequences that fall out for free: (1) the `ancestors` value needs no
  separate trust — an **over-claim** (larger than the true LCA) names a prefix the new history doesn't
  share, so the fold misses `new_head.root` and is rejected; an **under-claim** is a real shorter
  shared prefix, accepted, costing only extra refetch — so the binary-search LCA is purely an
  efficiency optimization, not a security boundary. (2) Gate the *fork*: follow only a **strictly
  higher** fork (a same/lower fork is a stale head or an equivocation — an attack, not a history to
  adopt). (3) Two degenerate anchors need no proof: `ancestors == new_head.length` is a pure
  truncation (the new head *is* your prefix — compare `prefix_root_hash`), and `ancestors == 0` has no
  prefix to anchor (an upgrade proof needs `old >= 1`), so adopt the signed higher-fork head from
  scratch and let `add_block` re-verify every block. After verifying, `truncate` to `ancestors` and
  refetch the suffix with the existing `add_block` — byte-identical to the source's rewritten history.
- **At L1 the autobase "view" is the linearization itself and the "indexed view" is the finalized
  prefix — the apply step is the domain logic you deliberately don't have.** Upstream materializes a
  `view` by *applying* each linearized node (where the consumer's domain logic runs, possibly emitting a
  batch of entries per node); the tests then assert `view.length` / `view.get(i)` / `getIndexedViewLength`.
  Since L1 is content-blind there is nothing to apply, so the domain-agnostic fold is the **identity** one
  — one node, one entry (its `NodeId`) — making `view() ≡ order()`, `view_len()` = node count,
  `view_get(i)` the i-th ordered node (`None` past the end ≡ `view.get(i,{wait:false}) == null`),
  `indexed_view() ≡ finalized()`, and `indexed_view_len() ≡ getIndexedViewLength`. The consumer replays
  `view()` through *its* apply to build the typed view; only the ordering/confirmation lives at L1.
  Porting gotcha — **only assert the upstream `getIndexedViewLength` number where your confirmation rule
  actually matches it**: for a fork-free indexer chain our conservative double-quorum prefix (ADR-0015)
  *is* upstream's confirmed length (e.g. the `c-b-a-c-b-a` chain → view 6, indexed 4), so assert it; but
  upstream confirms *earlier* in two deferred cases — a **unanimous single quorum** (`n` indexers, all `n`
  voting: safe because a single quorum is the whole set, but our rule still demands a double quorum, so
  `dags - simple 2`'s `getIndexedViewLength == 1` would fail against our `0`) and across a **resolved
  fork/merge** — so do *not* hardcode those numbers; assert instead the always-true properties (view +
  indexed-length **converge** across every causally-valid delivery order — the `(a)==(b)==(c)` family — and
  indexed view ⊑ view). Also: a forced chain (each step has exactly one causally-ready node) has only
  *one* valid delivery order, so a "converges across delivery orders" test over it is vacuous — use a
  genuinely forked DAG (concurrent tails) to exercise reordering.
- **A proof verifier must bind structural position, not just the root hash** (audit, after iter 21).
  `parent_hash` binds child hash+size but NOT index, so `tree_hash == root` authenticates *content*,
  not *position*. Two real consequences found: (1) **a seek leaf must be even** — `SeekProof::verify`
  trusted a prover-supplied node, so the root / an interior node (odd index) authenticated and its
  aggregate subtree size bracketed any offset → a bogus `index/2` block accepted (a genuine soundness
  bug; upstream `ByteSeeker` guards `(index & 1) === 0`, our reimpl dropped it); (2) **a sibling must be
  the actual sibling** — `NodeProof::verify` checked `sib.index == flat::sibling(..)`, `Proof`/`SeekProof`
  did not. Rule: when a verifier trusts a prover-supplied index, guard it structurally. Every seek test
  used honest (even-index) proofs, so the hole was invisible to "green" — **write the *forged* proof,
  not just the honest one.**
- **An inclusion proof binds a block to *one specific head's root*, so the replica's cross-head
  rejection is invisible to positive-path tests** (audit follow-up). `Replica::add_block` checks the
  head signature under its *own* configured key and `proof.verify(data, &head.root)` — but a clean
  replicate→verify test always passes a block under the head it was generated against, so neither the
  root-binding branch nor the wrong-author branch is ever exercised. To pin them, present an *honest*
  proof under a **different same-author head**: a fork at the **same length** is the purest (only the
  root differs, so the proof's other root node can't fold to it), and a **longer honest head** catches
  the different-length case (test both directions). Separately, a replica keyed to A must refuse a
  wholly-valid log signed by B — that is the head-signature gate, not the proof. Assert nothing is
  stored on each rejection, and keep `index == replica.len()` so the in-order guard passes and the test
  actually reaches the binding branch (not a trivial ordering reject).
