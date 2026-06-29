//! Audit follow-up — cross-check `Linearizer::quorum_degree` **values** against an
//! independent reference computation over random DAGs.
//!
//! The existing quorum tests pin production `quorum_degree` against a handful of
//! hand-worked `DESIGN.md` examples, and the convergence sim only fuzzes that the
//! *finalized prefix* converges / stays monotone — never the **degree value** itself
//! over random graphs. This file closes that gap (the last `DEFINITION_OF_DONE.md`
//! audit follow-up).
//!
//! The reference oracle here is a deliberately *different* algorithm from production:
//! production (`crates/autobase/src/lib.rs::quorum_degree`) computes the degree in a
//! single bottom-up pass over a topological order, carrying a per-indexer "best degree"
//! from each node's **strict** dependencies plus a hardcoded author self-vote. The oracle
//! instead computes the degree of every node by a **fixpoint relaxation** straight from
//! the recursive `DESIGN.md` definition over **inclusive** causal closures, with the
//! author's self-vote *emerging* from the node's own current degree rather than being
//! special-cased. Two independent routes to the same number ⇒ a meaningful cross-check:
//! an off-by-one in either the level indexing or the self-vote would make them diverge.
//!
//! Definition being computed (`reference/js/autobase/DESIGN.md` "Quorums"): a **vote** is
//! a reference from an indexer to a node (≥1 of that indexer's nodes causally sees it).
//! `target` has a degree-1 (single) quorum once a majority of indexers vote for it; a
//! degree-`k` quorum once a majority of indexers each have a node that itself witnesses a
//! degree-`(k-1)` quorum over `target` within its own causal closure. `quorum_degree(target)`
//! is the highest degree any node witnesses. Votes are read from the DAG shape alone — never
//! a payload, never a timestamp.

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

/// Writer `i`'s opaque key: distinct and byte-lexicographically ordered, so the first few
/// (the indexers) and the tiebreak are predictable. `i` stays well under 255.
fn wkey(i: usize) -> WriterKey {
    [(i as u8) + 1; 32]
}

/// `NodeId` for writer `i`'s `seq`-th node.
fn node(i: usize, seq: u64) -> NodeId {
    NodeId::new(wkey(i), seq)
}

/// A generated DAG: nodes in creation order, each paired with the **cross-writer** heads it
/// referenced. The same-writer predecessor is implicit (`Linearizer::add` re-adds it).
type Steps = Vec<(NodeId, Vec<NodeId>)>;

/// **Partitioned** generator (the upstream `createDag` model): each new node references a
/// *random subset* of the current cross-writer tails — so writers fork, run concurrently,
/// and later merge, producing the rich mix of quorum degrees we want to value-check.
/// `ref_num/ref_den` is the per-tail reference probability.
fn gen_partitioned(
    n_writers: usize,
    n_nodes: usize,
    ref_num: u32,
    ref_den: u32,
    rng: &mut Rng,
) -> Steps {
    let writers: Vec<WriterKey> = (0..n_writers).map(wkey).collect();
    let mut next_seq = vec![0u64; n_writers];
    let mut tails: BTreeSet<NodeId> = BTreeSet::new();
    let mut steps: Steps = Vec::with_capacity(n_nodes);

    while steps.len() < n_nodes {
        let wi = rng.below(n_writers);
        let key = writers[wi];
        let seq = next_seq[wi];
        let n = NodeId::new(key, seq);

        let mut cross = Vec::new();
        for t in tails.iter() {
            if t.key != key && rng.chance(ref_num, ref_den) {
                cross.push(*t);
            }
        }

        next_seq[wi] = seq + 1;

        for t in &cross {
            if rng.chance(ref_num, ref_den) {
                tails.remove(t);
            }
        }
        if seq > 0 && rng.chance(ref_num, ref_den) {
            tails.remove(&NodeId::new(key, seq - 1));
        }
        tails.insert(n);

        steps.push((n, cross));
    }

    steps
}

/// Full dependency map: explicit cross heads ∪ implicit same-writer predecessor — exactly
/// what `Linearizer::add` reconstructs internally. Built independently here so the oracle
/// never reaches into the linearizer's private state.
fn deps_of(steps: &Steps) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    let mut deps = BTreeMap::new();
    for (n, cross) in steps {
        let mut d: BTreeSet<NodeId> = cross.iter().copied().collect();
        if n.seq > 0 {
            d.insert(NodeId::new(n.key, n.seq - 1));
        }
        deps.insert(*n, d);
    }
    deps
}

/// Inclusive causal closure of every node: `anc[v]` = `{v}` ∪ ⋃ over `v`'s deps' closures.
/// Computed in creation order, which is a valid topological order (the generator only ever
/// references already-created nodes), so each dependency is closed before its dependent.
/// This is our own reachability — independent of the linearizer's `sees`.
fn closures(steps: &Steps) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    let deps = deps_of(steps);
    let mut anc: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    for (n, _) in steps {
        let mut set = BTreeSet::new();
        set.insert(*n);
        for d in &deps[n] {
            let prior = anc
                .get(d)
                .expect("dependency closed before dependent (creation order is topological)");
            for u in prior {
                set.insert(*u);
            }
        }
        anc.insert(*n, set);
    }
    anc
}

/// Reference quorum degree of `target` — the **independent** fixpoint computation.
///
/// Relaxes a per-node degree upward until stable, straight from the `DESIGN.md` recursion:
/// a node `v` (that sees `target`) witnesses degree `d` once, within `v`'s inclusive causal
/// closure, a majority of indexers each have a node that sees `target` with degree ≥ `d-1`.
/// `v` itself is in its own closure, so the author's self-vote is emergent (it counts at
/// exactly the levels `v`'s own degree already reaches) rather than a hardcoded `+1`. The
/// answer is the maximum degree any node witnesses.
fn ref_quorum_degree(
    anc: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    indexers: &BTreeSet<WriterKey>,
    target: &NodeId,
) -> usize {
    if !anc.contains_key(target) || indexers.is_empty() {
        return 0;
    }
    let m = indexers.len() / 2 + 1;

    // Nodes that causally see `target` — its voters live here.
    let seen: BTreeSet<NodeId> = anc
        .iter()
        .filter(|(_, a)| a.contains(target))
        .map(|(v, _)| *v)
        .collect();

    // How many *distinct indexers* have a node `u` in `v`'s inclusive closure that sees
    // `target` and has degree ≥ `level` in the current snapshot. `u == v` counts — exactly
    // the author's self-vote, emergent from `v`'s own degree.
    let count_at_level = |v: &NodeId, level: usize, deg: &BTreeMap<NodeId, usize>| -> usize {
        let mut writers: BTreeSet<WriterKey> = BTreeSet::new();
        for u in &anc[v] {
            if seen.contains(u) && indexers.contains(&u.key) && *deg.get(u).unwrap_or(&0) >= level {
                writers.insert(u.key);
            }
        }
        writers.len()
    };

    let cap = seen.len() + 8;
    let mut deg: BTreeMap<NodeId, usize> = seen.iter().map(|v| (*v, 0usize)).collect();
    loop {
        let snapshot = deg.clone();
        let mut changed = false;
        for v in &seen {
            // deg_v = max d with count_at_level(v, d-1) ≥ m: keep raising while a majority
            // vouch the current level (level d ⇒ degree d+1).
            let mut d = 0usize;
            while count_at_level(v, d, &snapshot) >= m {
                d += 1;
                assert!(d <= cap, "degree did not converge (cap {cap}) for {v:?}");
            }
            if d != snapshot[v] {
                changed = true;
            }
            deg.insert(*v, d);
        }
        if !changed {
            break;
        }
    }

    deg.values().copied().max().unwrap_or(0)
}

/// A uniformly-random topological order (randomized Kahn): pick a random causally-ready node
/// each step. Every result is a valid causal delivery order, so replaying it must reproduce
/// the same — pure-function-of-the-node-set — quorum degrees.
fn random_topo(deps: &BTreeMap<NodeId, BTreeSet<NodeId>>, rng: &mut Rng) -> Vec<NodeId> {
    let mut children: BTreeMap<NodeId, Vec<NodeId>> =
        deps.keys().map(|k| (*k, Vec::new())).collect();
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
        let n = frontier.swap_remove(i);
        out.push(n);
        if let Some(cs) = children.get(&n) {
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
    for n in order {
        let cross = cross_by_node.get(n).map(|v| v.as_slice()).unwrap_or(&[]);
        lin.add(*n, cross).expect("a causal delivery order must add cleanly");
    }
    lin
}

/// First validate the **oracle itself** against the hand-worked `DESIGN.md` quorum examples
/// — so that using it as a cross-check is trustworthy — and confirm the production linearizer
/// agrees with the oracle on each.
#[test]
fn reference_oracle_reproduces_design_examples() {
    // Writers a < b < c (wkey(0) < wkey(1) < wkey(2)).
    let (a, b, c) = (0usize, 1, 2);
    let idx: BTreeSet<WriterKey> = [a, b, c].iter().map(|&i| wkey(i)).collect();
    let idx_vec: Vec<WriterKey> = [a, b, c].iter().map(|&i| wkey(i)).collect();

    let check = |steps: &Steps, target: NodeId, expected: usize| {
        let anc = closures(steps);
        assert_eq!(
            ref_quorum_degree(&anc, &idx, &target),
            expected,
            "oracle disagrees with DESIGN.md for {target:?}"
        );
        // The production linearizer must agree with the oracle, node-for-node.
        let cross: BTreeMap<NodeId, Vec<NodeId>> = steps.iter().cloned().collect();
        let order: Vec<NodeId> = steps.iter().map(|(n, _)| *n).collect();
        let lin = deliver(&cross, &order, &idx_vec);
        assert_eq!(
            lin.quorum_degree(&target),
            expected,
            "production disagrees with DESIGN.md for {target:?}"
        );
    };

    // DESIGN "Quorums": chain a0 - b0 - c0 - a1 ⇒ degrees 3, 2, 1, 0 over a0/b0/c0/a1.
    let chain: Steps = vec![
        (node(a, 0), vec![]),
        (node(b, 0), vec![node(a, 0)]),
        (node(c, 0), vec![node(b, 0)]),
        (node(a, 1), vec![node(c, 0)]),
    ];
    check(&chain, node(a, 0), 3);
    check(&chain, node(b, 0), 2);
    check(&chain, node(c, 0), 1);
    check(&chain, node(a, 1), 0);

    // DESIGN "Higher Quorums": c0 - b0 - c1 ⇒ c0 double-quorum (2), b0 single (1).
    let higher: Steps = vec![
        (node(c, 0), vec![]),
        (node(b, 0), vec![node(c, 0)]),
        (node(c, 1), vec![node(b, 0)]),
    ];
    check(&higher, node(c, 0), 2);
    check(&higher, node(b, 0), 1);

    // DESIGN "Condition for Consistency": competing single quorums — a0 and c0 each reach 1
    // but conflict (writer c is in both), so neither is enough to finalize.
    let compete: Steps = vec![
        (node(a, 0), vec![]),
        (node(c, 0), vec![]),
        (node(c, 1), vec![node(a, 0)]), // implicit predecessor c0
        (node(b, 0), vec![node(c, 0)]),
    ];
    check(&compete, node(a, 0), 1);
    check(&compete, node(c, 0), 1);
}

/// The main cross-check: over many seeded random partitioned DAGs and several indexer-set
/// sizes, `Linearizer::quorum_degree(target)` must equal the independent fixpoint oracle for
/// **every** node — and, because the degree is a pure function of the node set, equal it under
/// several distinct causally-valid delivery orders too. Non-vacuity guards assert the corpus
/// actually exercises degrees 0, 1, and ≥2 (else the cross-check would be hollow).
#[test]
fn quorum_degree_matches_independent_oracle_over_random_dags() {
    // (writers, indexers): majorities 2, 3, 3 — exercises odd/even indexer counts and a
    // strict-subset-of-writers indexer set (non-indexing writers present).
    let configs = [(5usize, 3usize), (6, 4), (5, 5)];
    let n_nodes = 30;
    let replicas = 3;

    let mut seen_deg0 = false;
    let mut seen_deg1 = false;
    let mut seen_deg2 = false;
    let mut max_deg = 0usize;
    let mut comparisons = 0usize;

    for &(n_writers, n_indexers) in &configs {
        let indexers_vec: Vec<WriterKey> = (0..n_indexers).map(wkey).collect();
        let indexers: BTreeSet<WriterKey> = indexers_vec.iter().copied().collect();

        for seed in 0..24u64 {
            let mut rng = Rng::new(
                0x91A0_0000_0000_0001
                    ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    ^ ((n_writers as u64) << 40)
                    ^ ((n_indexers as u64) << 48),
            );
            // Alternate sparse (heavy partitioning) and dense (funnels → higher quorums).
            let (rn, rd) = if seed % 2 == 0 { (3, 5) } else { (17, 20) };
            let steps = gen_partitioned(n_writers, n_nodes, rn, rd, &mut rng);

            let cross_by_node: BTreeMap<NodeId, Vec<NodeId>> = steps.iter().cloned().collect();
            let deps = deps_of(&steps);
            let anc = closures(&steps);

            // Production replicas: creation order plus several random causal delivery orders.
            let creation: Vec<NodeId> = steps.iter().map(|(n, _)| *n).collect();
            let mut reps = vec![deliver(&cross_by_node, &creation, &indexers_vec)];
            for _ in 0..replicas {
                let topo = random_topo(&deps, &mut rng);
                assert_eq!(topo.len(), n_nodes, "topo covers every node (seed {seed})");
                reps.push(deliver(&cross_by_node, &topo, &indexers_vec));
            }

            for (n, _) in &steps {
                let expected = ref_quorum_degree(&anc, &indexers, n);
                for (ri, r) in reps.iter().enumerate() {
                    assert_eq!(
                        r.quorum_degree(n),
                        expected,
                        "quorum_degree mismatch for {n:?} \
                         (writers {n_writers}, indexers {n_indexers}, seed {seed}, replica {ri})"
                    );
                }
                comparisons += 1;
                max_deg = max_deg.max(expected);
                match expected {
                    0 => seen_deg0 = true,
                    1 => seen_deg1 = true,
                    _ => seen_deg2 = true,
                }
            }
        }
    }

    assert!(comparisons > 0, "no comparisons ran");
    assert!(
        seen_deg0 && seen_deg1 && seen_deg2,
        "non-vacuity: degrees 0, 1, and ≥2 must all appear across the corpus \
         (0={seen_deg0} 1={seen_deg1} ≥2={seen_deg2})"
    );
    assert!(
        max_deg >= 2,
        "expected at least a double quorum to form somewhere (max observed {max_deg})"
    );
}
