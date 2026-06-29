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
//! This iteration implements the **linearizer**: causal order + deterministic
//! tiebreak. Indexer-quorum finalization (which *prefix* is permanently confirmed)
//! is the next capability and is intentionally absent here.
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
}
