//! Gate #3 — generic **convergence simulation**.
//!
//! Modeled on `reference/js/autobase/test/fuzz/` (clean-room, not a port): generate
//! random causal DAGs with N writers under seeded randomness (so failures reproduce),
//! then assert the four `autobase` properties from `docs/DEFINITION_OF_DONE.md` over them:
//!
//! - **determinism / convergence** — replicas that have seen the same node set produce
//!   the same [`Linearizer::order`], *regardless of the delivery order* (we replay each
//!   DAG through several distinct causally-valid topological orders and compare);
//! - **causal-respect** — no node is emitted before one of its dependencies;
//! - **state-equality** — a generic content-agnostic fold over the order (a rolling
//!   checksum — *no domain types*, L1) is identical across replicas;
//! - **finality-stability** — under cooperative growth the finalized prefix only ever
//!   extends, never reorders, and a long run finalizes a non-empty prefix.
//!
//! Everything here is generic: writers are opaque 32-byte keys, payloads do not exist,
//! and "state" is just a checksum of the emitted order. Nothing inspects a payload.

use std::collections::{BTreeMap, BTreeSet};

use autobase::{Linearizer, NodeId, WriterKey};

/// Deterministic, dependency-free PRNG (SplitMix64). Seeded so every generated DAG and
/// every delivery permutation reproduces exactly — a failing seed is a permanent repro.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// `true` with probability `num/den` — integer ratios keep it float-free & exact.
    fn chance(&mut self, num: u32, den: u32) -> bool {
        (self.next_u64() % den as u64) < num as u64
    }
}

/// Writer `i`'s opaque key: distinct and byte-lexicographically ordered, so indexers
/// (the first few) and the tiebreak are predictable. `i` stays well under 255.
fn wkey(i: usize) -> WriterKey {
    [(i as u8) + 1; 32]
}

/// A generated DAG: nodes in creation order, each paired with the **cross-writer** heads
/// it referenced. The same-writer predecessor is implicit (`Linearizer::add` re-adds it),
/// so it is never listed here.
struct Dag {
    steps: Vec<(NodeId, Vec<NodeId>)>,
}

impl Dag {
    /// Full dependency map (cross heads ∪ implicit same-writer predecessor) — what
    /// `Linearizer::add` reconstructs internally, used here to drive topo replay and the
    /// causal-respect check without reaching into the linearizer's private state.
    fn deps(&self) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
        let mut deps = BTreeMap::new();
        for (node, cross) in &self.steps {
            let mut d: BTreeSet<NodeId> = cross.iter().copied().collect();
            if node.seq > 0 {
                d.insert(NodeId::new(node.key, node.seq - 1));
            }
            deps.insert(*node, d);
        }
        deps
    }

    /// Lookup of each node's cross-writer heads, for replay through an arbitrary order.
    fn cross_by_node(&self) -> BTreeMap<NodeId, Vec<NodeId>> {
        self.steps.iter().cloned().collect()
    }

    /// The creation order — itself a valid causal delivery order (every reference points
    /// at an already-created node).
    fn creation_order(&self) -> Vec<NodeId> {
        self.steps.iter().map(|(n, _)| *n).collect()
    }
}

/// **Partitioned** generator (the `createDag` model): each new node references a *random
/// subset* of the current cross-writer tails — so writers fork, run concurrently, and
/// later merge. `ref_num/ref_den` is the per-tail reference probability (lower ⇒ more
/// partitions / more reordering). Referenced tails and the superseded predecessor are
/// retired with the same probability, leaving some old heads live to be re-merged.
fn gen_partitioned(n_writers: usize, n_nodes: usize, ref_num: u32, ref_den: u32, rng: &mut Rng) -> Dag {
    let writers: Vec<WriterKey> = (0..n_writers).map(wkey).collect();
    let mut next_seq = vec![0u64; n_writers];
    let mut tails: BTreeSet<NodeId> = BTreeSet::new();
    let mut steps = Vec::with_capacity(n_nodes);

    while steps.len() < n_nodes {
        let wi = rng.below(n_writers);
        let key = writers[wi];
        let seq = next_seq[wi];
        let node = NodeId::new(key, seq);

        // Reference a random subset of *other* writers' live tails (same-writer chaining
        // is the implicit predecessor, never an explicit cross head).
        let mut cross = Vec::new();
        for t in tails.iter() {
            if t.key != key && rng.chance(ref_num, ref_den) {
                cross.push(*t);
            }
        }

        next_seq[wi] = seq + 1;

        // Retire some referenced tails (merged in) and, usually, the superseded
        // predecessor — leaving the rest live, so stale heads can spawn later forks.
        for t in &cross {
            if rng.chance(ref_num, ref_den) {
                tails.remove(t);
            }
        }
        if seq > 0 && rng.chance(ref_num, ref_den) {
            tails.remove(&NodeId::new(key, seq - 1));
        }
        tails.insert(node);

        steps.push((node, cross));
    }

    Dag { steps }
}

/// **Cooperative** generator: each new node references *every* current cross-writer tail
/// and retires all of them, so it causally sees the entire history so far. The DAG is a
/// total order (no stranded forks), which is exactly the regime in which the conservative
/// finalized prefix is guaranteed monotone — the setting for finality-stability.
fn gen_cooperative(n_writers: usize, n_nodes: usize, rng: &mut Rng) -> Dag {
    let writers: Vec<WriterKey> = (0..n_writers).map(wkey).collect();
    let mut next_seq = vec![0u64; n_writers];
    let mut tails: BTreeSet<NodeId> = BTreeSet::new();
    let mut steps = Vec::with_capacity(n_nodes);

    while steps.len() < n_nodes {
        let wi = rng.below(n_writers);
        let key = writers[wi];
        let seq = next_seq[wi];
        let node = NodeId::new(key, seq);

        let cross: Vec<NodeId> = tails.iter().filter(|t| t.key != key).copied().collect();

        next_seq[wi] = seq + 1;
        for t in &cross {
            tails.remove(t);
        }
        if seq > 0 {
            tails.remove(&NodeId::new(key, seq - 1));
        }
        tails.insert(node);

        steps.push((node, cross));
    }

    Dag { steps }
}

/// A uniformly-random topological order of `deps` (randomized Kahn): at each step pick a
/// random causally-ready node. Every result is a valid causal delivery order, so replaying
/// it must reproduce the same linearization.
fn random_topo(deps: &BTreeMap<NodeId, BTreeSet<NodeId>>, rng: &mut Rng) -> Vec<NodeId> {
    let mut children: BTreeMap<NodeId, Vec<NodeId>> = deps.keys().map(|k| (*k, Vec::new())).collect();
    let mut indeg: BTreeMap<NodeId, usize> = BTreeMap::new();
    for (n, d) in deps {
        indeg.insert(*n, d.len());
        for dep in d {
            children.get_mut(dep).expect("dependency is a known node").push(*n);
        }
    }

    let mut frontier: Vec<NodeId> = indeg
        .iter()
        .filter(|(_, c)| **c == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut out = Vec::with_capacity(deps.len());

    while !frontier.is_empty() {
        let i = rng.below(frontier.len());
        let node = frontier.swap_remove(i);
        out.push(node);
        if let Some(cs) = children.get(&node) {
            for c in cs {
                let e = indeg.get_mut(c).expect("known node");
                *e -= 1;
                if *e == 0 {
                    frontier.push(*c);
                }
            }
        }
    }

    out
}

/// Replay a delivery order into a fresh linearizer with the given indexer set.
fn deliver(
    cross_by_node: &BTreeMap<NodeId, Vec<NodeId>>,
    order: &[NodeId],
    indexers: &[WriterKey],
) -> Linearizer {
    let mut lin = Linearizer::with_indexers(indexers.iter().copied());
    for node in order {
        let cross = cross_by_node.get(node).map(|v| v.as_slice()).unwrap_or(&[]);
        lin.add(*node, cross).expect("a causal delivery order must add cleanly");
    }
    lin
}

/// Generic, content-agnostic "application state": an order-sensitive rolling checksum
/// (FNV-1a over each node's bytes). Equal iff the orders are equal — the L1 stand-in for
/// "replicas folded the same operations and reached the same state". No domain types.
fn fold_state(order: &[NodeId]) -> u64 {
    let mut acc = 0xcbf2_9ce4_8422_2325u64;
    for n in order {
        for b in n.key {
            acc = (acc ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        acc = (acc ^ n.seq).wrapping_mul(0x0100_0000_01b3);
    }
    acc
}

/// Every dependency precedes its dependent, and the order lists every node exactly once.
fn assert_causal(deps: &BTreeMap<NodeId, BTreeSet<NodeId>>, order: &[NodeId]) {
    let pos: BTreeMap<NodeId, usize> = order.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    assert_eq!(pos.len(), deps.len(), "order must list every node exactly once");
    for (node, ds) in deps {
        for d in ds {
            assert!(pos[d] < pos[node], "causal violation: {d:?} must precede {node:?}");
        }
    }
}

/// Convergence + causal-respect + state-equality over random **partitioned** DAGs: the
/// same node set delivered in many distinct causally-valid orders always yields the same
/// `order()`, the same folded state, and the same `finalized()`. Some seeds are dense
/// enough to actually finalize a prefix, proving the finalized-convergence path is
/// exercised (not vacuously empty everywhere).
#[test]
fn order_state_and_finalized_converge_across_delivery_orders() {
    let n_writers = 5;
    let n_indexers = 3;
    let n_nodes = 30;
    let replicas = 4;
    let indexers: Vec<WriterKey> = (0..n_indexers).map(wkey).collect();

    let mut total_finalized = 0usize;

    for seed in 0..16u64 {
        let mut rng = Rng::new(0x5EED_0000_0000_0001 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));

        // Alternate sparse (≈0.6, heavy partitioning/reordering) and dense (≈0.85,
        // funnels → finalizes) so both regimes are covered.
        let (rn, rd) = if seed % 2 == 0 { (3, 5) } else { (17, 20) };
        let dag = gen_partitioned(n_writers, n_nodes, rn, rd, &mut rng);
        let deps = dag.deps();
        let cross_by_node = dag.cross_by_node();

        // Reference replica: creation order.
        let r0 = deliver(&cross_by_node, &dag.creation_order(), &indexers);
        let order0 = r0.order();
        let state0 = fold_state(&order0);
        let fin0 = r0.finalized();

        assert_eq!(order0.len(), n_nodes, "every node is ordered (seed {seed})");
        assert_causal(&deps, &order0);
        assert!(order0.starts_with(&fin0), "finalized ⊑ order (seed {seed})");
        total_finalized += fin0.len();

        // Each random causal delivery order must reproduce all three exactly.
        for rep in 0..replicas {
            let topo = random_topo(&deps, &mut rng);
            assert_eq!(topo.len(), n_nodes, "topo covers every node (seed {seed} rep {rep})");
            assert_causal(&deps, &topo); // the replay order is itself causal...

            let r = deliver(&cross_by_node, &topo, &indexers);
            assert_eq!(r.order(), order0, "order converges (seed {seed} rep {rep})");
            assert_eq!(fold_state(&r.order()), state0, "state converges (seed {seed} rep {rep})");
            assert_eq!(r.finalized(), fin0, "finalized converges (seed {seed} rep {rep})");
        }
    }

    assert!(
        total_finalized > 0,
        "no seed finalized anything — the finality path was never exercised; make the dense regime denser"
    );
}

/// Finality-stability over random **cooperative** DAGs: as nodes arrive in creation order
/// the finalized prefix only ever extends — never drops or reorders a node — and stays a
/// prefix of `order()` at every step. A long cooperative run finalizes a non-empty prefix.
#[test]
fn finalized_prefix_is_monotone_under_cooperative_growth() {
    let n_writers = 5;
    let n_indexers = 3;
    let n_nodes = 40;
    let indexers: Vec<WriterKey> = (0..n_indexers).map(wkey).collect();

    let mut ever_finalized = false;

    for seed in 0..8u64 {
        let mut rng = Rng::new(0xC007_0000_0000_0001 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let dag = gen_cooperative(n_writers, n_nodes, &mut rng);
        let cross_by_node = dag.cross_by_node();

        // Deliver one node at a time in creation order; the finalized prefix must grow
        // monotonically and never desynchronize from the current order.
        let mut lin = Linearizer::with_indexers(indexers.iter().copied());
        let mut prev: Vec<NodeId> = Vec::new();
        for node in dag.creation_order() {
            let cross = cross_by_node.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
            lin.add(node, cross).expect("cooperative delivery adds cleanly");

            let cur = lin.finalized();
            assert!(
                cur.starts_with(&prev),
                "finalized must extend, never reorder (seed {seed}): {prev:?} -> {cur:?}"
            );
            assert!(
                lin.order().starts_with(&cur),
                "finalized ⊑ order at every step (seed {seed})"
            );
            prev = cur;
        }

        if !prev.is_empty() {
            ever_finalized = true;
        }
    }

    assert!(
        ever_finalized,
        "cooperative growth never finalized anything — quorum/finality is not being reached"
    );
}
