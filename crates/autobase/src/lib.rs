//! `autobase` — multi-writer causal linearizer.
//!
//! Combines multiple `hypercore`s (one per writer) into a single deterministic,
//! eventually-consistent order. Each node carries a *clock*: a set of causal
//! references to other writers' heads. The linearization is a topological order
//! of that DAG, tie-broken deterministically among concurrent nodes by **lowest
//! writer key first, then lowest seq** — never by a timestamp or any self-reported
//! scalar (that is what makes forged "append times" a non-attack), and never by
//! inspecting a payload (this layer is domain-agnostic, L1).
//!
//! On top of the linearizer (causal order + deterministic tiebreak) this layer
//! adds **indexer quorum** and a **finalized prefix**: by counting how many
//! indexers reference a node — and, recursively, how many reference *that*
//! quorum — we determine when a node is permanently confirmed and can no longer
//! be reordered (DESIGN.md "Consistency"). Votes are counted by causal
//! reachability only — never a timestamp, never a payload peek.
//!
//! ## Clean-room divergence
//!
//! Upstream (`reference/js/autobase/lib/topolist.js`) keeps an *incremental*
//! sorted tip and shuffles each arriving node into place (`moveDown`/`moveUp`),
//! tracking `undo`/`shared` so a streaming view can be patched cheaply. We instead
//! recompute the whole order with a **priority Kahn topological sort** — at each
//! step emit the causally-ready node with the smallest [`NodeId`]. This is simpler,
//! is *manifestly* independent of arrival order (so determinism is obvious), and
//! reproduces the canonical linearizations in `reference/js/autobase/DESIGN.md`.
//! See ADR-0014.

use std::collections::{BTreeMap, BTreeSet};

/// A writer's stable identity: the 32-byte author public key
/// (`identity::PublicKey::to_bytes()`, equivalently an Iroh `NodeId`).
///
/// The linearizer treats it purely as an opaque, totally-ordered label — it never
/// verifies crypto, decodes a payload, or reads a clock. Byte-lexicographic order
/// on the key *is* the tiebreak ("lowest key wins").
pub type WriterKey = [u8; 32];

/// Address of one node in the causal DAG: writer [`WriterKey`] plus `seq`, the
/// 0-based position in that writer's append-only log.
///
/// `Ord` compares `key` first, then `seq` — exactly the deterministic tiebreak the
/// linearizer applies to concurrent nodes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId {
    pub key: WriterKey,
    pub seq: u64,
}

impl NodeId {
    pub fn new(key: WriterKey, seq: u64) -> Self {
        Self { key, seq }
    }
}

/// Why a node could not be added. Each variant is a violation of **causal
/// delivery** — the L1 guarantee that a node arrives only after everything it
/// references. With it upheld, the DAG is always acyclic and causally closed, so
/// [`Linearizer::order`] is always well-defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// A node already exists at this id (writers are append-only; ids are unique).
    Duplicate(NodeId),
    /// `seq` skips ahead of the writer's log — its predecessor must arrive first.
    Gap {
        writer: WriterKey,
        expected: u64,
        got: u64,
    },
    /// A referenced causal head has not been delivered yet.
    MissingHead(NodeId),
}

/// A growing causal DAG of nodes from many writers, linearizable on demand.
///
/// Add nodes with [`add`](Self::add) (respecting causal delivery); call
/// [`order`](Self::order) for the current deterministic linearization. `order` is a
/// pure function of the node set, so two replicas holding the same DAG produce the
/// same order regardless of arrival order — the convergence property.
#[derive(Default)]
pub struct Linearizer {
    /// node -> its direct causal dependencies (referenced heads ∪ same-writer predecessor).
    deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// node -> the nodes that directly depend on it (reverse edges, for Kahn).
    dependents: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// next expected `seq` per writer — enforces append-only, gap-free delivery.
    next_seq: BTreeMap<WriterKey, u64>,
    /// The **indexers**: writers whose references count as votes toward quorums
    /// (DESIGN.md "Indexing Writer"). `None` ⇒ quorum disabled (pure ordering); a
    /// node from a writer outside this set is still ordered but never votes
    /// (a non-indexing writer).
    indexers: Option<BTreeSet<WriterKey>>,
}

impl Linearizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes currently in the DAG.
    pub fn len(&self) -> usize {
        self.deps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }

    /// Whether a node at `id` has been delivered.
    pub fn contains(&self, id: &NodeId) -> bool {
        self.deps.contains_key(id)
    }

    /// Add `node`, referencing `heads` (the other writers' heads it saw). The
    /// same-writer predecessor `(key, seq - 1)` is added as a dependency
    /// automatically, so callers need only list cross-writer references.
    ///
    /// Enforces causal delivery: rejects a duplicate, a seq gap, or a head that has
    /// not been delivered. On success the DAG stays acyclic and causally closed.
    pub fn add(&mut self, node: NodeId, heads: &[NodeId]) -> Result<(), AddError> {
        if self.deps.contains_key(&node) {
            return Err(AddError::Duplicate(node));
        }

        let expected = self.next_seq.get(&node.key).copied().unwrap_or(0);
        if node.seq != expected {
            return Err(AddError::Gap {
                writer: node.key,
                expected,
                got: node.seq,
            });
        }

        // Every referenced head must already be delivered (the predecessor is
        // guaranteed present by the gap check above, so it is not re-checked here).
        for h in heads {
            if !self.deps.contains_key(h) {
                return Err(AddError::MissingHead(*h));
            }
        }

        let mut dep_set: BTreeSet<NodeId> = heads.iter().copied().collect();
        if node.seq > 0 {
            dep_set.insert(NodeId::new(node.key, node.seq - 1));
        }
        // A node never depends on itself; causal delivery already rules this out,
        // but be explicit so a self-referencing head can't smuggle in a cycle.
        dep_set.remove(&node);

        for d in &dep_set {
            self.dependents.entry(*d).or_default().insert(node);
        }
        self.deps.insert(node, dep_set);
        self.dependents.entry(node).or_default();
        self.next_seq.insert(node.key, node.seq + 1);

        Ok(())
    }

    /// The current deterministic linearization: a topological order of the DAG,
    /// emitting at each step the causally-ready node with the smallest [`NodeId`].
    ///
    /// Causality is always respected (a node never precedes a dependency) and the
    /// result depends only on the node set, never on arrival order.
    pub fn order(&self) -> Vec<NodeId> {
        // Remaining unsatisfied dependencies per node.
        let mut indegree: BTreeMap<NodeId, usize> =
            self.deps.iter().map(|(n, d)| (*n, d.len())).collect();

        // Ready frontier: causally-ready nodes, ordered by NodeId (key then seq).
        let mut frontier: BTreeSet<NodeId> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(n, _)| *n)
            .collect();

        let mut out = Vec::with_capacity(self.deps.len());
        while let Some(node) = frontier.pop_first() {
            out.push(node);
            if let Some(children) = self.dependents.get(&node) {
                for child in children {
                    let d = indegree
                        .get_mut(child)
                        .expect("dependent must be a known node");
                    *d -= 1;
                    if *d == 0 {
                        frontier.insert(*child);
                    }
                }
            }
        }

        out
    }

    /// Tails: nodes with no dependencies (the roots of the DAG). Mirrors upstream's
    /// `linearizer.tails` — useful for the quorum layer and as a structural check.
    pub fn tails(&self) -> BTreeSet<NodeId> {
        self.deps
            .iter()
            .filter(|(_, d)| d.is_empty())
            .map(|(n, _)| *n)
            .collect()
    }

    /// Designate the **indexers** — the writers whose references count as votes
    /// toward quorums (DESIGN.md "Indexing Writer"). Without this, [`finalized`]
    /// is always empty and [`quorum_degree`] is always 0: the linearizer still
    /// produces a deterministic order, but no prefix is ever *confirmed*.
    ///
    /// [`finalized`]: Self::finalized
    /// [`quorum_degree`]: Self::quorum_degree
    pub fn with_indexers(indexers: impl IntoIterator<Item = WriterKey>) -> Self {
        Self {
            indexers: Some(indexers.into_iter().collect()),
            ..Self::default()
        }
    }

    /// Majority of the indexer set (`⌊n/2⌋ + 1`), or `None` when quorum is disabled.
    fn majority(&self) -> Option<usize> {
        self.indexers.as_ref().map(|ix| ix.len() / 2 + 1)
    }

    /// Does node `a` causally see node `b` — i.e. is `b` in `a`'s causal history
    /// (inclusive of `a == b`)? This is the graph-reachability equivalent of
    /// upstream's `clock.includes`: votes and quorums are read from the DAG shape
    /// alone, never from a payload or a clock value a writer reports about itself.
    pub fn sees(&self, a: &NodeId, b: &NodeId) -> bool {
        if a == b {
            return true;
        }
        if !self.deps.contains_key(a) || !self.deps.contains_key(b) {
            return false;
        }
        let mut stack = vec![*a];
        let mut visited: BTreeSet<NodeId> = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if let Some(ds) = self.deps.get(&node) {
                for d in ds {
                    if d == b {
                        return true;
                    }
                    if visited.insert(*d) {
                        stack.push(*d);
                    }
                }
            }
        }
        false
    }

    /// The **quorum degree** achieved over `target` (DESIGN.md "Quorums").
    ///
    /// A **vote** is a reference from an indexer to a node (at most one per
    /// indexer, counted by causal reachability). Degree 1 — a *single quorum* —
    /// means a majority of indexers reference `target`. Degree 2 — a *double
    /// quorum* — means a majority of indexers each reference a degree-1 quorum
    /// over `target`; and so on, recursively. The result is the highest degree
    /// any node in the DAG witnesses over `target`.
    ///
    /// Returns 0 when quorum is disabled, the target is unknown, or not even a
    /// single quorum has formed.
    ///
    /// Computed by a single bottom-up pass over a topological order: for every
    /// node `v` we track, per indexer `w`, the best degree reached by any node of
    /// `w` in `v`'s causal closure (`bestdeg`). `v` then witnesses degree `k` over
    /// `target` once a majority of indexers vouch the previous level — `v`'s own
    /// author vouching every level up to `v`'s degree. This recomputes from
    /// scratch rather than maintaining upstream's incremental `Consensus` machine
    /// (ADR-0015), so determinism is manifest.
    pub fn quorum_degree(&self, target: &NodeId) -> usize {
        let Some(m) = self.majority() else {
            return 0;
        };
        let indexers = self.indexers.as_ref().expect("majority ⇒ indexers set");
        if !self.deps.contains_key(target) {
            return 0;
        }

        // bestdeg[v][w] = max degree-over-target reached by any node of indexer
        // `w` within `v`'s causal closure (only nodes that see `target`).
        let mut bestdeg: BTreeMap<NodeId, BTreeMap<WriterKey, i64>> = BTreeMap::new();
        // seesx[v] = whether `v` causally sees `target`.
        let mut seesx: BTreeMap<NodeId, bool> = BTreeMap::new();
        let mut max_degree: i64 = 0;

        // order() is a valid topological order: every dependency precedes its node.
        for v in self.order() {
            let mut pre: BTreeMap<WriterKey, i64> = BTreeMap::new();
            let mut sees_target = v == *target;
            if let Some(ds) = self.deps.get(&v) {
                for d in ds {
                    if seesx.get(d).copied().unwrap_or(false) {
                        sees_target = true;
                    }
                    if let Some(bd) = bestdeg.get(d) {
                        for (w, deg) in bd {
                            let e = pre.entry(*w).or_insert(*deg);
                            if *deg > *e {
                                *e = *deg;
                            }
                        }
                    }
                }
            }
            seesx.insert(v, sees_target);

            let author = v.key;
            let author_is_indexer = indexers.contains(&author);
            let mut deg_v: i64 = 0;
            if sees_target {
                // `v`'s author vouches every level up to `v`'s own degree, so it
                // contributes +1 at the level under test (the loop only reaches
                // level k-1 after confirming the degree is already ≥ k-1).
                let self_vote = if author_is_indexer { 1 } else { 0 };
                let mut d = 1i64;
                loop {
                    let level = d - 1;
                    let mut count = 0usize;
                    for w in indexers.iter() {
                        if *w == author {
                            continue;
                        }
                        if pre.get(w).map(|pd| *pd >= level).unwrap_or(false) {
                            count += 1;
                        }
                    }
                    if count + self_vote >= m {
                        deg_v = d;
                        d += 1;
                    } else {
                        break;
                    }
                }
            }

            // Record this node's own vote (only an indexer that sees the target).
            let mut bd = pre;
            if sees_target && author_is_indexer {
                let e = bd.entry(author).or_insert(deg_v);
                if deg_v > *e {
                    *e = deg_v;
                }
            }
            bestdeg.insert(v, bd);

            if sees_target && deg_v > max_degree {
                max_degree = deg_v;
            }
        }

        max_degree.max(0) as usize
    }

    /// The **finalized prefix**: the maximal prefix of [`order`](Self::order)
    /// whose every node has reached a **double quorum** ([`quorum_degree`] ≥ 2)
    /// *and* is causally comparable to every other node in the DAG (no unresolved
    /// concurrent fork around it). Once a node enters this prefix, growing the DAG
    /// cooperatively never reorders it — the **finality-stability** property.
    ///
    /// This is the snapshot / no-active-fork form of finalization: it deliberately
    /// refuses to commit *either* arm of an unresolved fork until a confirmed merge
    /// makes the contested nodes comparable. The fork/merge competition rule and
    /// the 2-degree-lead caveat (DESIGN.md "Tails, Forks and Merges"; upstream
    /// `consensus.js` merge handling) are deferred — see ADR-0015.
    ///
    /// [`quorum_degree`]: Self::quorum_degree
    pub fn finalized(&self) -> Vec<NodeId> {
        if self.majority().is_none() {
            return Vec::new();
        }
        let order = self.order();
        let mut out = Vec::new();
        for node in &order {
            let comparable = order
                .iter()
                .all(|other| other == node || self.sees(node, other) || self.sees(other, node));
            if comparable && self.quorum_degree(node) >= 2 {
                out.push(*node);
            } else {
                break;
            }
        }
        out
    }

    // ----------------------------------------------------------------------------------
    // View materialization (upstream autobase's `view`).
    //
    // Upstream linearizes the DAG and then *applies* each node to materialize a `view`
    // (a hypercore the consumer reads): `view.length` is the total materialized length,
    // `view.get(i)` reads entry `i`, and `getIndexedViewLength` (DESIGN.md "Indexing")
    // reports how much of that view is **confirmed** — the indexed prefix that can never
    // reorder. The apply step is where *domain* logic lives, so at L1 there is nothing to
    // apply: this layer is content-blind. The domain-agnostic materialization is therefore
    // the identity one — **each node contributes exactly one entry, its own [`NodeId`]** —
    // so the "view" is just the linearization and the "indexed view" is the finalized
    // prefix. A consuming application replays `view()` through its own apply function to
    // build the real, typed view; the ordering/confirmation it relies on lives here.
    // See ADR-0028.
    // ----------------------------------------------------------------------------------

    /// The **materialized view**: the linearization rendered as an indexable sequence of
    /// entries (≡ upstream `view`). At L1 each node is one entry (its [`NodeId`]), so this
    /// equals [`order`](Self::order); a domain consumer folds these through its own apply
    /// function to produce a typed view.
    pub fn view(&self) -> Vec<NodeId> {
        self.order()
    }

    /// Length of the materialized [`view`](Self::view) (≡ upstream `view.length`). One node
    /// = one entry at L1, so this is the node count.
    pub fn view_len(&self) -> usize {
        self.len()
    }

    /// Entry `i` of the materialized [`view`](Self::view), or `None` past the end
    /// (≡ upstream `view.get(i, { wait: false })` returning `null` beyond `view.length`).
    pub fn view_get(&self, i: usize) -> Option<NodeId> {
        self.order().into_iter().nth(i)
    }

    /// The **indexed view**: the confirmed prefix of the [`view`](Self::view) that can
    /// never reorder — the [`finalized`](Self::finalized) prefix, which is always a prefix
    /// of [`view`](Self::view).
    pub fn indexed_view(&self) -> Vec<NodeId> {
        self.finalized()
    }

    /// Length of the [`indexed_view`](Self::indexed_view) (≡ upstream
    /// `getIndexedViewLength` — `getIndexedInfo().views[].length`). Always
    /// `<= view_len()`.
    ///
    /// This is our **conservative** confirmation depth: a double-quorum, no-active-fork
    /// prefix (ADR-0015). For a fork-free indexer chain it equals upstream's confirmed
    /// length exactly; the cases where upstream confirms *earlier* — a unanimous single
    /// quorum (`n` indexers, all `n` voting, e.g. `dags.js` "simple 2") and confirmation
    /// across a resolved fork/merge — are the deferred fork/merge consensus work and are
    /// not yet matched (ADR-0015, ADR-0028).
    pub fn indexed_view_len(&self) -> usize {
        self.finalized().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer key built so that `wk(1) < wk(2) < wk(3)` byte-lexicographically —
    /// i.e. writers a < b < c, matching `reference/js/autobase/DESIGN.md`.
    fn wk(tag: u8) -> WriterKey {
        [tag; 32]
    }

    fn n(key: u8, seq: u64) -> NodeId {
        NodeId::new(wk(key), seq)
    }

    // Writers a=1, b=2, c=3.
    const A: u8 = 1;
    const B: u8 = 2;
    const C: u8 = 3;

    fn add(lin: &mut Linearizer, node: NodeId, heads: &[NodeId]) {
        lin.add(node, heads).expect("causal add must succeed");
    }

    /// Every dependency must appear before its dependent in `order`.
    fn assert_causal(lin: &Linearizer, order: &[NodeId]) {
        let pos: BTreeMap<NodeId, usize> =
            order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        assert_eq!(pos.len(), lin.len(), "order must list every node exactly once");
        for (node, deps) in &lin.deps {
            for dep in deps {
                assert!(
                    pos[dep] < pos[node],
                    "causal violation: {dep:?} must precede {node:?}"
                );
            }
        }
    }

    // DESIGN.md: `a0 - b0 - c0 - a1 - b1` linearises to itself.
    #[test]
    fn linear_chain() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);
        add(&mut lin, n(B, 1), &[n(A, 1)]);

        assert_eq!(
            lin.order(),
            vec![n(A, 0), n(B, 0), n(C, 0), n(A, 1), n(B, 1)]
        );
        assert_causal(&lin, &lin.order());
    }

    // DESIGN.md branch:
    //   a0 - c0 - a1
    //      /
    //   b0
    // c0 sees both tails {a0, b0}; with a < b the tails order [a0, b0].
    #[test]
    fn branch_tiebreak_by_key() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[]);
        add(&mut lin, n(C, 0), &[n(A, 0), n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        assert_eq!(lin.order(), vec![n(A, 0), n(B, 0), n(C, 0), n(A, 1)]);
        assert_causal(&lin, &lin.order());
    }

    // DESIGN.md recursive example (a < b < c):
    //
    //   a0   c0
    //    | X |
    //   b0   a1
    //    | \ |
    //   c1   b1
    //    | /
    //   b2
    //
    // canonical linearisation: [a0, c0, a1, b0, b1, c1, b2]
    fn recursive_dag() -> Vec<(NodeId, Vec<NodeId>)> {
        vec![
            (n(A, 0), vec![]),
            (n(C, 0), vec![]),
            (n(B, 0), vec![n(A, 0), n(C, 0)]),
            (n(A, 1), vec![n(C, 0)]), // + implicit a0
            (n(C, 1), vec![n(B, 0)]), // + implicit c0
            (n(B, 1), vec![n(A, 1)]), // + implicit b0
            (n(B, 2), vec![n(C, 1)]), // + implicit b1
        ]
    }

    #[test]
    fn recursive_dag_matches_design() {
        let mut lin = Linearizer::new();
        for (node, heads) in recursive_dag() {
            add(&mut lin, node, &heads);
        }

        assert_eq!(
            lin.order(),
            vec![
                n(A, 0),
                n(C, 0),
                n(A, 1),
                n(B, 0),
                n(B, 1),
                n(C, 1),
                n(B, 2)
            ]
        );
        assert_causal(&lin, &lin.order());
        assert_eq!(lin.tails(), [n(A, 0), n(C, 0)].into_iter().collect());
    }

    // Determinism: same DAG ⇒ same order, regardless of (causally-valid) arrival
    // order. This is the "replicas seeing the same set agree" property.
    #[test]
    fn deterministic_regardless_of_arrival_order() {
        let dag = recursive_dag();
        let by_index = |idxs: &[usize]| {
            let mut lin = Linearizer::new();
            for &i in idxs {
                let (node, heads) = &dag[i];
                add(&mut lin, *node, heads);
            }
            lin.order()
        };

        // Three distinct topologically-valid insertion orders.
        let canonical = by_index(&[0, 1, 2, 3, 4, 5, 6]);
        let perm_b = by_index(&[1, 0, 3, 2, 4, 5, 6]); // c0, a0, a1, b0, c1, b1, b2
        let perm_c = by_index(&[1, 0, 2, 4, 3, 5, 6]); // c0, a0, b0, c1, a1, b1, b2

        assert_eq!(canonical, perm_b);
        assert_eq!(canonical, perm_c);
    }

    #[test]
    fn rejects_causal_delivery_violations() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);

        // Duplicate id.
        assert_eq!(lin.add(n(A, 0), &[]), Err(AddError::Duplicate(n(A, 0))));

        // Seq gap: a2 before a1.
        assert_eq!(
            lin.add(n(A, 2), &[]),
            Err(AddError::Gap {
                writer: wk(A),
                expected: 1,
                got: 2,
            })
        );

        // Reference to an undelivered head.
        assert_eq!(
            lin.add(n(B, 0), &[n(C, 0)]),
            Err(AddError::MissingHead(n(C, 0)))
        );

        // The valid follow-up still works, and nothing partial was committed.
        add(&mut lin, n(A, 1), &[]);
        assert_eq!(lin.len(), 2);
        assert_eq!(lin.order(), vec![n(A, 0), n(A, 1)]);
    }

    #[test]
    fn empty_orders_to_nothing() {
        let lin = Linearizer::new();
        assert!(lin.order().is_empty());
        assert!(lin.tails().is_empty());
    }

    // A fourth writer key for the majority-threshold test (d, beyond a<b<c).
    const D: u8 = 4;

    fn indexed(keys: &[u8]) -> Linearizer {
        Linearizer::with_indexers(keys.iter().map(|&k| wk(k)))
    }

    // DESIGN.md "Quorums": in the chain `a0 - b0 - c0 - a1`, a0 accumulates a
    // single quorum at b0, a double quorum at c0, and a triple quorum at a1.
    #[test]
    fn quorum_degrees_match_design_chain() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        assert_eq!(lin.quorum_degree(&n(A, 0)), 3, "a1 triple-quorums a0");
        assert_eq!(lin.quorum_degree(&n(B, 0)), 2, "a1 double-quorums b0");
        assert_eq!(lin.quorum_degree(&n(C, 0)), 1, "a1 single-quorums c0");
        assert_eq!(lin.quorum_degree(&n(A, 1)), 0, "a1 itself unconfirmed");
    }

    // DESIGN.md "Higher Quorums": `c0 - b0 - c1` lifts c0 to a double quorum
    // because writers b & c reference the single quorum that formed at b0.
    #[test]
    fn double_quorum_design_higher_example() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(C, 0), &[]);
        add(&mut lin, n(B, 0), &[n(C, 0)]);
        add(&mut lin, n(C, 1), &[n(B, 0)]);

        assert_eq!(lin.quorum_degree(&n(C, 0)), 2, "c1 double-quorums c0");
        assert_eq!(lin.quorum_degree(&n(B, 0)), 1, "b0 single-quorumed by c1");
    }

    // DESIGN.md "Condition for Consistency": two nodes can each reach a *single*
    // quorum yet conflict — so a single quorum must never finalize. Here c1 sees
    // {a0, c0} and b0 sees {c0}; a0 is voted by {a, c}, c0 by {b, c}. Both reach a
    // single quorum, but c is in both — writers b and c hold conflicting views.
    #[test]
    fn single_quorum_does_not_finalize() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(C, 0), &[]);
        add(&mut lin, n(C, 1), &[n(A, 0)]); // c1: cross-ref a0, implicit pred c0
        add(&mut lin, n(B, 0), &[n(C, 0)]);

        assert_eq!(lin.quorum_degree(&n(A, 0)), 1);
        assert_eq!(lin.quorum_degree(&n(C, 0)), 1);
        assert!(
            lin.finalized().is_empty(),
            "competing single quorums must not finalize"
        );
    }

    // The finalized prefix is the double-quorum'd head of a chain, and it is
    // always a genuine prefix of the deterministic order.
    #[test]
    fn finalized_is_a_double_quorum_prefix() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        let f = lin.finalized();
        assert_eq!(f, vec![n(A, 0), n(B, 0)], "only a0,b0 are double-quorum'd");
        assert!(lin.order().starts_with(&f), "finalized ⊑ order");
    }

    // Finality-stability: as the DAG grows cooperatively (each append references
    // the head), the finalized prefix only ever extends — no node already
    // finalized is dropped or reordered.
    #[test]
    fn finalized_prefix_only_grows() {
        // round-robin chain a0,b0,c0,a1,b1,c1,a2,b2,c2
        let writers = [A, B, C];
        let steps: Vec<NodeId> = (0..9)
            .map(|i| n(writers[i % 3], (i / 3) as u64))
            .collect();

        let mut lin = indexed(&[A, B, C]);
        let mut prev: Vec<NodeId> = Vec::new();
        let mut last = String::new();
        for (i, node) in steps.iter().enumerate() {
            let heads = if i == 0 { vec![] } else { vec![steps[i - 1]] };
            add(&mut lin, *node, &heads);

            let cur = lin.finalized();
            assert!(cur.starts_with(&prev), "finalized must extend, never reorder");
            assert!(lin.order().starts_with(&cur), "finalized ⊑ order at every step");
            prev = cur;
            last = format!("{} nodes finalized", prev.len());
        }
        assert!(!prev.is_empty(), "a long chain must finalize something ({last})");
    }

    // Majority is ⌊n/2⌋+1: with four indexers it takes three distinct votes to
    // form even a single quorum.
    #[test]
    fn majority_threshold_scales_with_indexer_count() {
        let mut lin = indexed(&[A, B, C, D]); // majority = 3
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]); // a0 referenced by {a,b} = 2 < 3
        assert_eq!(lin.quorum_degree(&n(A, 0)), 0, "two of four is not a majority");

        add(&mut lin, n(C, 0), &[n(B, 0)]); // now {a,b,c} = 3 ≥ 3
        assert_eq!(lin.quorum_degree(&n(A, 0)), 1, "three of four is a single quorum");
    }

    // Without an indexer set the linearizer still orders, but confirms nothing.
    #[test]
    fn no_indexers_means_no_finalization() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);

        assert_eq!(lin.quorum_degree(&n(A, 0)), 0);
        assert!(lin.finalized().is_empty());
        // ordering is unaffected
        assert_eq!(lin.order(), vec![n(A, 0), n(B, 0), n(C, 0)]);
    }

    // A non-indexing writer is ordered but never casts a vote: its references do
    // not move a target toward quorum.
    #[test]
    fn non_indexer_references_do_not_vote() {
        // Only a & b are indexers; c is a non-indexing writer.
        let mut lin = indexed(&[A, B]); // majority = 2
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(C, 0), &[n(A, 0)]); // c (non-indexer) references a0

        // a0 has only its own indexer vote {a}; c's reference doesn't count.
        assert_eq!(lin.quorum_degree(&n(A, 0)), 0, "non-indexer vote ignored");

        add(&mut lin, n(B, 0), &[n(C, 0)]); // b (indexer) now references a0
        assert_eq!(lin.quorum_degree(&n(A, 0)), 1, "{{a,b}} is the quorum");
        // c is still part of the linearization.
        assert!(lin.order().contains(&n(C, 0)));
    }
}
