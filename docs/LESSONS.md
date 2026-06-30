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
- **Atomic-commit fault coverage needs the *boundaries* and the rollback's *own* failure, not one
  mid-batch fault** (audit follow-up). A single "fault the 2nd of 3 blocks" test leaves three branches
  unexercised: (1) a **first-block** fault aborts before any write, so the rollback list is empty and
  the `for w in &written` loop must correctly no-op (storage stays *pristine*, not just logically
  unchanged); (2) a **last-block** fault must roll back *all* prior writes (no orphans); (3) the
  rollback `delete` can *itself* fail — and because it is swallowed (`let _ = store.delete(..)`), the
  commit must still surface the **original `put`** error (the root cause, not the secondary delete
  error) and keep the log's *logical* state atomic (length/head/reads untouched) even though one
  **unreachable orphan** physically remains, to be overwritten by the next commit. Two test gotchas:
  the orphan is the **codec-encoded** block (varint length prefix — assert against `codec.encode(v)`,
  not the raw payload `v`), and asserting the recovered head equals a freshly-built prefix head
  (ed25519 is deterministic) proves the whole fault→rollback→recover path lands on the canonical state.
  This mirrors ADR-0024's truncate stance: physical reclamation is best-effort; the logical state is
  the invariant.
- **A "strictly higher fork" reorg gate is unreachable from a `None` head — and the mid-reorg case is
  the one worth testing** (audit follow-up). `Replica::verify_reorg` follows only a fork strictly
  higher than the one it currently trusts, so with no verified head there is no current fork to
  compare against and *every* reorg is refused — before any signature/proof check. Two situations
  reach `head == None`: a **fresh empty replica** (so a reorg can never bootstrap a replica — that is
  `add_block` against a head; even an `ancestors == 0` from-scratch reorg is refused), and a replica
  **mid-reorg** (it dropped the divergent suffix, keeping the shared prefix, so the tree is non-empty
  but `head == None` until the suffix is refetched). The mid-reorg case is the valuable one: a second,
  even-higher-fork reorg arriving now is refused, the replica is left untouched, and it must still be
  able to finish its *original* pending refetch. Test gotcha: capture the in-flight suffix
  blocks/proofs from the source **before** mutating it onto the higher fork — once the source moves to
  fork N+1 it no longer holds the fork-N tail, so the original refetch can't be completed from it.
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
- **Value-check a recursive DP with an *independent* oracle derived from the definition, not a
  copy of the algorithm** (audit follow-up). Production `quorum_degree` is a single bottom-up pass
  over a topological order carrying a per-indexer "best degree" from each node's *strict* deps plus
  a hardcoded author self-vote. To genuinely cross-check the *value* (convergence/monotonicity
  fuzzing never touches it), reimplement the `DESIGN.md` recursion a *different* way: a **fixpoint
  relaxation** over **inclusive** causal closures (build your own reachability from the test's edge
  list — don't reuse the linearizer's `sees`/`deps`), letting the author's self-vote be **emergent**
  (a node is in its own closure, so it counts at exactly the levels its current degree already
  reaches) instead of re-hardcoding the `+1`. Two routes to the same number ⇒ an off-by-one in the
  level indexing or the self-vote diverges. Validate the oracle against the `DESIGN.md` worked
  examples *first* (so it's trustworthy), then assert production == oracle node-for-node over seeded
  random DAGs × indexer-set sizes × several delivery orders (the degree is a pure function of the
  node set). Gate non-vacuity (degrees 0/1/≥2 all occur, a double quorum forms) so the fuzz isn't
  hollow. Keep indexer sets ≥ 2: a lone indexer ⇒ majority 1 ⇒ the production degree loop (author
  self-vote alone always ≥ majority) never terminates — a latent degenerate to avoid, not exercise.
- **A sparse bitfield is an *unbounded, infinite-zero* field — separate the data structure from
  persistence/replication, and watch the asymmetric `find` semantics.** Upstream's `bitfield.js` bundles
  three concerns; only the **data structure** is L1 (get/set/`set_range`/`count`/`find_first`/`find_last`)
  — `open`/`flush`/`BitInterlude` is storage persistence and `*want` is replication wire framing, both
  out of scope. Implement it sparse + paged (a `BTreeMap<page, [u64; words]>`; a single bit at index 8e15
  must cost one page, not a petabyte), and treat a **missing page as an all-`false` page you never
  materialize on clear** (mirror upstream's `if (!p && val)`: setting `false` allocates nothing, so
  "clear a range on a page that doesn't exist" is a no-op, not a throw). Three semantic traps the upstream
  tests pin: (1) `count(start, length, val)`'s second arg is a **length, not an end** (`count(3, 18, ..)`
  is over `[3, 21)`); (2) because the field is infinite zeros beyond the last set bit, `find_first(false,
  ..)` **always** returns a hit (the search terminates at the first missing or partial page — present
  fully-set pages are finite), while `find_first(true, ..)` and both `find_last` directions return
  "absent" (`None`/`-1`); (3) keep the page granularity (`2^15`) so page/segment **boundary** offsets
  (`2^15±1`, `2^21±1`, last-bit-in-page) behave like upstream even though the internal layout is
  clean-room (no segment/`BigSparseArray` layer). Verify `count`/`find_*` against a brute-force scan over a
  sparse mix and seed the "random set/get" test (SplitMix64 + a reference `HashSet`) so it reproduces.
- **The LCA binary search is sound even when corruption violates its monotonicity precondition**
  (audit follow-up). `lowest_common_ancestor` assumes prefix agreement is monotone (true for two
  *intact* trees), but a missing node makes `agree(a)` false where the prefix it needs is gone — which
  can be non-monotone. It stays sound regardless because the search keeps the invariant **`agree(lo)`
  is always true**, and `agree(a)` is true only when *both* trees produce equal prefix-root-hashes at
  `a` (a present, collision-resistant match) — so the returned length is always a length where the two
  trees genuinely share `[0, a)`. A gap therefore only ever **shrinks** the LCA to a real, shorter
  ancestor; it can never over-claim. Consequences for `reorg`, which adopts `other`'s entire node set:
  an **intact `other` heals a gapped `self`** (the full set overwrites the gap → intact, byte-identical
  follow), but a **corrupt `other` is copied verbatim** (its gaps land in `self`), so intact-other is
  the precondition for a clean follow — not a guard to add, just a contract to pin. And **byte seek
  skips zero-size blocks for free**: `seek`/`seek_proof` use the same `>` comparison as a linear scan,
  so an empty block's empty byte interval is never a seek target and an all-empty tree has no locatable
  byte — test it explicitly, because varied-size fixtures (`(i % 5) + 1`) never hit size 0.
- **`clear` is *presence* reclamation, not truncation — keep block-presence and log-structure as two
  separate axes.** Upstream's `clear(start, end)` drops a block range's *bytes* without shortening the
  log: the Merkle tree (length, root, every node) is **untouched**, only a presence bitfield + the
  stored bytes change. That separation is the whole point — a cleared block is still *authenticated*
  (the signed head and its inclusion proof are unaffected), so it stays re-downloadable from a holder;
  you can prove this host-safely (no wire) by verifying a holder-supplied block against the cleared
  core's unchanged head. Wiring it in: set presence bits on `append`/`commit` (commit only *after* its
  writes succeed, so a rolled-back failed commit leaves presence untouched too) and clear the tail's
  bits on `truncate`; then `has(i)` = within-length **and** bit-set, `contiguous_length` = the first
  absent index capped at the length (`find_first(false, 0)` is always `Some` — the infinite-zero tail —
  so an all-present log returns `len`), and `get`/`block` read **`None`** for an absent block (a no-wait
  read; at L1 there is no peer to wait on). Reserve the `Corrupt` error for the *genuine* inconsistency
  — the presence bit says "here" but the bytes are gone — so the bitfield, not storage, is the source
  of truth for presence (narrowing `get`'s old "missing ⇒ Corrupt" to "bit-set-but-missing ⇒ Corrupt"
  breaks no existing test, since every appended block is present). Clearing absent/out-of-range blocks
  is a harmless no-op (never touch a block you don't hold), and physical reclamation is best-effort (a
  failed `delete` still marks the block absent — same "logical state is the invariant" stance as
  truncate's orphans). `purge` (whole-core deletion) is a separate storage/session concern, not this.
- **A snapshot that must survive truncate-and-rewrite is a *by-value* copy, not a shared-storage
  view.** Upstream keeps a snapshot reading the old bytes after a rewrite via disk-layer
  copy-on-write / fork-namespacing — disk plumbing we don't have. At L1 the simplest faithful
  reimplementation is to **own a copy** of the snapshotted prefix (encoded block bytes + the
  `MerkleTree` at that length + the captured `SignedHead` + a clone of the codec): then nothing the
  core does later (append / truncate / re-append different content over the same indices) can change
  what the snapshot reports, which is exactly the observable contract. Three things fall out: (1)
  `get` past the snapshot's fixed length is `None` (our L1 form of upstream's out-of-range
  `SNAPSHOT_NOT_AVAILABLE` throw — same no-wait stance as the core's `get`); (2) a captured block is
  **independently authenticated** — its inclusion proof against the *captured* head still verifies even
  after the core forks away, because the snapshot carries its own tree + signed head; (3) the snapshot's
  `signedLength` (how much of it the *live* core still backs) is just the **content-blind shared-prefix
  LCA** of the snapshot's tree and the core's current tree (`lowest_common_ancestor`) — it never
  exceeds the snapshot length and drops the moment the core truncates below or rewrites a block within
  the snapshotted prefix, reproducing every upstream `signedLength` assertion without inspecting a
  payload. To own a codec for decoding, zero-sized config codecs (`U64`/`Bytes`) just need
  `derive(Clone, Copy)` — an ergonomics detail, not a behavioural change. The by-value copy is a
  documented divergence (ADR-0032); a consumer needing zero-copy snapshots layers it on the storage
  backend.
- **A read/byte stream is a no-wait iterator over what you locally have, not an async tail.** Upstream's
  `createReadStream`/`createByteStream` are async, backpressured, optionally `live` (tail for new blocks),
  and the byte stream addresses the *value* byte layout. At L1 the behaviour-under-test collapses to a
  synchronous `Iterator`: the read stream yields decoded blocks over `[start, end)` (forward or reverse,
  `end` clamped to `len`), and the byte stream `seek`s to the start block then emits whole **encoded**
  blocks until the byte budget is spent (an empty-payload block is still emitted while budget > 0 — that's
  the "decode blocks that don't contribute to byte length" case; with framing an "empty" payload is a
  1-byte block, but the property "every block in range is emitted" still holds). Two consequences fall out
  for free: (1) `live` has nothing to tail (no peer), so **accept-and-ignore** it — upstream's "live should
  be ignored" then passes by construction; (2) absent blocks (`clear`ed / never downloaded) are *skipped*
  (no-wait, matching `get`'s `None`), so a stream over a holey core yields the present blocks in order.
  Keep byte offsets over the **encoded** layout the tree authenticates (not the payload — the `padding`
  divergence, ADR-0022); derive the test's offsets/lengths from `block(i).len()`, not hardcoded payload
  sizes. `createWriteStream` is just buffered `append` — no new L1 behaviour, don't build a type for it.
- **A log migration ("move-to") is a *content-addressed prefix commitment* + a by-value copy under a new
  key — strip the manifest/multisig/session wrapping.** Upstream re-homes a log onto a new core whose
  *manifest* carries a `prologue { length, hash }`, copies the prefix in (`copyPrologue`), and `moveTo`s a
  session/snapshot onto it (with a `migrate` event); the prologue is *self-authorizing* (the manifest hash
  is the authority, no per-head signature over the copied prefix) and is one field of the full multi-signer
  `Verifier`/`multisig` manifest (`manifest.js`). The L1 essence is much smaller: a `Prologue { length,
  hash }` names a prefix by its **Merkle hash** (so it is content-addressed — *any* log holding the same
  prefix content backs it, regardless of author — the same "head at a length is a pure function of the first
  `length` blocks" property truncate/fork-detection/LCA all rest on). `copy_prologue` then **content-checks
  before copying** (`source.prefix_root_hash(length) == hash`, so a non-matching source leaves the target
  untouched — verify-then-mutate, like every proof verifier here), copies the prefix **by value** (ADR-0032's
  snapshot stance — own the bytes, no shared storage), and **re-signs the prefix under the new key** (a head
  at `length`) rather than reproducing manifest self-authorization — observably identical, since the new
  key's first real append already signs a head ⊇ the prefix, so signing at `length` just does it one step
  early, and it keeps `verify_head`/`verify_block`/proofs uniform. Two consequences fall out: the prologue
  length is a **`truncate` floor** (a committed prefix can never be rewound, keeping `verify_prologue` an
  invariant), and the snapshot-`moveTo` case is a **no-op at L1** (a by-value snapshot is already immune to
  any mutation of the source, so a snapshot taken before the migration keeps returning its own blocks —
  nothing to re-home). Defer the multi-signer manifest + manifest-into-key identity (`manifest.js`) and the
  session-level `moveTo`/`migrate` (sessions/networking).
- **A multi-signer manifest is a *content-addressed quorum policy*: the signing rule hashed into the
  identity. Port the quorum primitive; defer the wire/compat/patch wrapping.** Upstream's `Verifier` drags
  in compact-encoding wire format, v0 compat signers, `allowPatch` cross-length patch signing, and
  `linked`/`userData` fields; the L1 essence is small. A `Manifest { quorum, signers: [{public_key,
  namespace}], prologue }` whose `hash()` (domain-separated, length-bound, over quorum + ordered signers +
  prologue) **is** the would-be log key — so the policy is self-authorizing (who may sign can't change
  without changing the identity), and a single-signer manifest's hash is exactly a plain core's key
  (`single(pk).hash() ≡ Hypercore.key(pk)`). To authorize a head a signer signs a `signable` that **binds
  the manifest hash** (the modern `ctx = manifestHash` path — so a signature is valid only under that exact
  policy; the per-signer namespace folds into the hash, not the signing context, in v1). `verify`
  short-circuits a `Prologue` prefix on content alone (the manifest-level form of the hypercore prologue),
  else the **multisig quorum** rule: ≥ quorum signatures, each a *distinct* in-range signer, each valid.
  Four gotchas: (1) the distinctness check (a `seen` set / `tried` array) is essential — the same signer
  twice is *not* two signatures (a real attack the `multi signer` test pins as `secondBadSignature`); (2)
  reject by *count of distinct-valid >= quorum*, not "any valid" (`thirdBadSignature` = one valid sig under
  quorum 2 must fail); (3) a non-ed25519 signer is **structurally impossible** if `Signer` only holds an
  ed25519 key — upstream's runtime "unsupported curve" throw becomes a type-system guarantee, so the
  asserting analogue is the *config* validation (`quorum == 0` / `quorum > signers` / no signers rejected
  at construction); (4) requiring **all supplied** signatures valid (vs upstream checking only the first
  `quorum`) is behaviourally identical for a distinct-valid quorum set and strictly safer (a garbage extra
  proof can't ride along). The full multisig **wire format** (`assemble`/`inflate`), the v0 **compat**
  path, `allowPatch` patch signing, and wiring the verifier into `Hypercore` (replacing the single-key
  signed head) are the deferred wrapping — none change the quorum primitive.
- **Wiring a new authority into a large, well-tested core: add a focused parallel type, don't refactor the
  hot core in place under a single green gate.** Changing how a head is *signed* (e.g. binding a manifest
  hash so a quorum can authorize it) changes the head's signed bytes, which cascades into every
  verify/replicate call site and their tests — in a 3000-line core with ~60 tests, many of them textually
  identical (`verify_block(&pk, &head, i, &enc, &proof)`), that is ~60 brittle edits with no behavioural gain
  for the single-signer case. The lower-risk, equally-faithful move is a self-contained sibling type
  (`ManifestCore`/`ManifestReplica`) that wires the new authority end-to-end (key = `manifest.hash()`; head =
  ≥ quorum distinct valid sigs; verify-only replica keyed by the *public* manifest), with the single-signer
  manifest as the special case so `key()` still equals the plain core's identity. The capability lands green;
  *unifying* the sibling into the original (retiring its single-key path) becomes a deferred mechanical step,
  not a prerequisite. Two design notes fall out: (1) a core collects a partial signature from each **local**
  signer secret it holds, so a core holding `< quorum` secrets honestly produces an *unauthorized* head it
  cannot ratify alone — that is the quorum gate, and it's the test worth writing; (2) sort the collected
  partial sigs by signer index so two cores with the same signer set produce **byte-identical** heads
  (determinism), and keep the head fork-free when the focused type has no `truncate` (don't import a
  fork-counter you don't yet use).
- **OPFS is the sync, persistent browser store — and it is worker-only.** `FileSystemSyncAccessHandle`
  read/write/getSize/truncate/flush are synchronous (so they fit a sync `Store`), but
  `createSyncAccessHandle()` is unavailable on the main thread — browser tests must
  `wasm_bindgen_test_configure!(run_in_dedicated_worker)`. *Acquiring* the handle (getDirectory →
  getFileHandle → createSyncAccessHandle) is async; the I/O is sync. Needs `--cfg web_sys_unstable_apis`.
- **`wasm-pack test` pins the *latest* chromedriver and ignores `CHROMEDRIVER`.** If the installed Chrome
  lags (e.g. Chrome 149 vs chromedriver 150), session creation fails with `http 404` / `SIGKILL` — looks
  like a sandbox problem, is actually a **major-version mismatch**. Fix: fetch the chromedriver matching the
  installed Chrome (Chrome-for-Testing `known-good-versions-with-downloads.json`), de-quarantine it, and run
  `cargo test --target wasm32-unknown-unknown` directly with
  `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=<wasm-bindgen-test-runner whose version matches the
  wasm-bindgen used to build>` + `CHROMEDRIVER=<that chromedriver>` (the runner honors `CHROMEDRIVER`;
  a mismatched runner gives a "schema version" error). A `webdriver.json` with `--no-sandbox` /
  `--disable-dev-shm-usage` helps in restricted environments.
- **Abstract browser-only I/O behind a trait so the real logic stays natively testable.** OPFS handles
  only work in a worker in a browser, but a log-structured store's complexity (record framing, replay,
  compaction, partial-tail recovery) is target-agnostic. Put it behind a `SyncFile` trait and test
  `LogStore<MemFile>` with plain `cargo test`; the browser run then only confirms the thin OPFS `SyncFile`
  impl. Push platform code to the edges.
