//! Port of `reference/js/autobase/test/topolist.js` — the **ordering** behaviour.
//!
//! `topolist.js` exercises upstream's incremental linearizer (`lib/topolist.js`). Its
//! in-scope, L1-relevant assertion is **stable ordering**: the linearization is a pure
//! function of the node set — the *same* DAG, delivered in *any* causally-valid order,
//! yields the *same* order (the `topolist - stable ordering` / `topolist - fuzz` /
//! `topolist - optimistic N` tests all assert exactly this invariance).
//!
//! Out of scope (deferred, not ported here):
//! - the **streaming-view bookkeeping** (`undo` / `shared` / `mark` / `flush` / `indexed`):
//!   upstream tracks how much of a *previously emitted* prefix a new node invalidates so a
//!   live view can be patched cheaply. We recompute `order()` from scratch each call
//!   (ADR-0014), so there is no patch to track — this is a streaming optimization, not the
//!   ordering definition.
//! - **optimistic** nodes (a separate writer-admission feature; `optimistic.js` row `[ ]`).
//!
//! ## What this file proves
//!
//! ADR-0014 reimplements upstream's *incremental insertion sort* as a **priority-Kahn**
//! topological sort and claims the two produce the *same* order for causally-closed,
//! non-optimistic DAGs. This file turns that claim into an asserting test, host-safely (no
//! `node`, no container — the JS oracle gate #4 stays env-blocked):
//!
//! 1. a **faithful, test-only re-statement** of upstream's non-optimistic
//!    `lib/topolist.js` insertion sort (`topolist_oracle`), and
//! 2. a cross-check that it equals our [`Linearizer::order`] on the canonical
//!    `DESIGN.md` DAGs, the explicit `topolist - stable ordering` example, and a battery
//!    of seeded random fork/merge DAGs — each also asserting the oracle is itself
//!    delivery-order independent (upstream's `stable ordering` / `fuzz` property).
//!
//! The oracle is a behavioural mirror of upstream's algorithm used *only* to validate our
//! independent implementation; it is not the production path. It complements — does not
//! replace — the upstream-JS oracle (gate #4, ADR-0008), which runs the actual reference
//! code in a sandbox.

use std::collections::{BTreeMap, BTreeSet};

use autobase::{Linearizer, NodeId, WriterKey};

// ----------------------------------------------------------------------------------------
// In-Rust topolist oracle: a faithful re-statement of upstream's *non-optimistic* insertion
// sort (`reference/js/autobase/lib/topolist.js`: `add` → `moveDown` + `moveNonOptimisticUp`,
// `cmp` / `cmpUnlinked`, `links`). Used purely as a test oracle (ADR-0027).
// ----------------------------------------------------------------------------------------

/// `direct[a]` = a's **direct** causal dependencies: its referenced cross-writer heads plus
/// its implicit same-writer predecessor `(key, seq-1)` — exactly the union upstream's
/// `links(a, b)` recognizes (`b ∈ a.dependencies`, i.e. `b.dependents.has(a)`, **or** `b` is
/// a's `length-1` predecessor on the same writer) and exactly what `Linearizer::add`
/// reconstructs internally.
fn direct_deps(steps: &[(NodeId, Vec<NodeId>)]) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    let mut direct = BTreeMap::new();
    for (node, cross) in steps {
        let mut d: BTreeSet<NodeId> = cross.iter().copied().collect();
        if node.seq > 0 {
            d.insert(NodeId::new(node.key, node.seq - 1));
        }
        direct.insert(*node, d);
    }
    direct
}

/// `links(a, b)` — does `a` directly depend on `b`? (Upstream: `b.dependents.has(a) ||
/// b is a's same-writer predecessor`; both fold into `direct[a]`.)
fn links(direct: &BTreeMap<NodeId, BTreeSet<NodeId>>, a: &NodeId, b: &NodeId) -> bool {
    direct.get(a).is_some_and(|d| d.contains(b))
}

/// Upstream `cmp(a, b)`: if `b` directly depends on `a`, `a` must come first (`Less`);
/// otherwise compare unlinked by writer key then seq (`cmpUnlinked`).
fn cmp(direct: &BTreeMap<NodeId, BTreeSet<NodeId>>, a: &NodeId, b: &NodeId) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if links(direct, b, a) {
        return Ordering::Less;
    }
    a.key.cmp(&b.key).then(a.seq.cmp(&b.seq))
}

/// Insert each node of `add_order` into a growing `tip` with upstream's non-optimistic
/// insertion sort. `moveDown` slides the node toward the front until it sits just after a
/// node it directly depends on (its causal floor); `moveNonOptimisticUp` then slides it back
/// toward the end past every strictly-smaller node, landing it at the sorted insertion point.
fn topolist_oracle(direct: &BTreeMap<NodeId, BTreeSet<NodeId>>, add_order: &[NodeId]) -> Vec<NodeId> {
    use std::cmp::Ordering;
    let mut list: Vec<NodeId> = Vec::with_capacity(add_order.len());

    for &node in add_order {
        list.push(node);
        let mut idx = list.len() - 1;

        // moveDown: stop at the first preceding node `node` directly depends on.
        while idx > 0 {
            let prev = list[idx - 1];
            if links(direct, &node, &prev) {
                break;
            }
            list.swap(idx - 1, idx);
            idx -= 1;
        }

        // moveNonOptimisticUp: advance while the next node should sort before `node`.
        while idx + 1 < list.len() {
            let next = list[idx + 1];
            if cmp(direct, &node, &next) != Ordering::Greater {
                break;
            }
            list.swap(idx, idx + 1);
            idx += 1;
        }
    }

    list
}

// ----------------------------------------------------------------------------------------
// Driving the real linearizer + a seeded fork/merge DAG generator (mirrors convergence.rs).
// ----------------------------------------------------------------------------------------

/// Replay a causally-valid delivery order into a fresh linearizer and return its `order()`.
fn linearizer_order(steps: &BTreeMap<NodeId, Vec<NodeId>>, add_order: &[NodeId]) -> Vec<NodeId> {
    let mut lin = Linearizer::new();
    for node in add_order {
        let cross = steps.get(node).map(|v| v.as_slice()).unwrap_or(&[]);
        lin.add(*node, cross).expect("a causal delivery order must add cleanly");
    }
    lin.order()
}

/// Deterministic, dependency-free PRNG (SplitMix64) — a failing seed reproduces forever.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    fn chance(&mut self, num: u32, den: u32) -> bool {
        (self.next_u64() % den as u64) < num as u64
    }
}

fn wkey(i: usize) -> WriterKey {
    [(i as u8) + 1; 32]
}

/// Partitioned fork/merge generator (the upstream `createDag` model): each node references a
/// random subset of the current cross-writer tails, so writers fork, run concurrently, and
/// later merge. Returns nodes in creation order with their cross-writer heads.
fn gen_dag(n_writers: usize, n_nodes: usize, num: u32, den: u32, rng: &mut Rng) -> Vec<(NodeId, Vec<NodeId>)> {
    let writers: Vec<WriterKey> = (0..n_writers).map(wkey).collect();
    let mut next_seq = vec![0u64; n_writers];
    let mut tails: BTreeSet<NodeId> = BTreeSet::new();
    let mut steps = Vec::with_capacity(n_nodes);

    while steps.len() < n_nodes {
        let wi = rng.below(n_writers);
        let key = writers[wi];
        let seq = next_seq[wi];
        let node = NodeId::new(key, seq);

        let mut cross = Vec::new();
        for t in tails.iter() {
            if t.key != key && rng.chance(num, den) {
                cross.push(*t);
            }
        }
        next_seq[wi] = seq + 1;
        for t in &cross {
            if rng.chance(num, den) {
                tails.remove(t);
            }
        }
        if seq > 0 && rng.chance(num, den) {
            tails.remove(&NodeId::new(key, seq - 1));
        }
        tails.insert(node);
        steps.push((node, cross));
    }

    steps
}

/// A uniformly-random topological (causal) delivery order of `direct` via randomized Kahn.
fn random_topo(direct: &BTreeMap<NodeId, BTreeSet<NodeId>>, rng: &mut Rng) -> Vec<NodeId> {
    let mut children: BTreeMap<NodeId, Vec<NodeId>> = direct.keys().map(|k| (*k, Vec::new())).collect();
    let mut indeg: BTreeMap<NodeId, usize> = BTreeMap::new();
    for (n, d) in direct {
        indeg.insert(*n, d.len());
        for dep in d {
            children.get_mut(dep).expect("dependency is a known node").push(*n);
        }
    }
    let mut frontier: Vec<NodeId> = indeg.iter().filter(|(_, c)| **c == 0).map(|(n, _)| *n).collect();
    let mut out = Vec::with_capacity(direct.len());
    while !frontier.is_empty() {
        let i = rng.below(frontier.len());
        let node = frontier.swap_remove(i);
        out.push(node);
        for c in children.get(&node).into_iter().flatten() {
            let e = indeg.get_mut(c).expect("known node");
            *e -= 1;
            if *e == 0 {
                frontier.push(*c);
            }
        }
    }
    out
}

/// Every dependency precedes its dependent, and the order lists every node exactly once.
fn assert_causal(direct: &BTreeMap<NodeId, BTreeSet<NodeId>>, order: &[NodeId]) {
    let pos: BTreeMap<NodeId, usize> = order.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    assert_eq!(pos.len(), direct.len(), "order lists every node exactly once");
    for (node, ds) in direct {
        for d in ds {
            assert!(pos[d] < pos[node], "causal violation: {d:?} must precede {node:?}");
        }
    }
}

// ----------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------

// Writers a=1 < b=2 < c=3 byte-lexicographically (matches DESIGN.md / topolist.js a<b<c).
fn n(key: u8, seq: u64) -> NodeId {
    NodeId::new([key; 32], seq)
}
const A: u8 = 1;
const B: u8 = 2;
const C: u8 = 3;

/// `topolist - stable ordering`: a0 ← {b0, c0}; c1 follows c0 only by same-writer
/// sequencing (it lists *no* explicit deps). Whatever causally-valid order the four nodes
/// arrive in, both our `order()` and the topolist oracle yield `[a0, b0, c0, c1]`.
#[test]
fn stable_ordering_matches_upstream_example() {
    let steps = vec![
        (n(A, 0), vec![]),
        (n(B, 0), vec![n(A, 0)]),
        (n(C, 0), vec![n(A, 0)]),
        (n(C, 1), vec![]), // predecessor c0 implicit
    ];
    let steps_map: BTreeMap<NodeId, Vec<NodeId>> = steps.iter().cloned().collect();
    let direct = direct_deps(&steps);
    let expected = vec![n(A, 0), n(B, 0), n(C, 0), n(C, 1)];

    // The three causally-valid add orders upstream replays (c1 always after its predecessor
    // c0; b0 after a0), plus creation order — all converge to the same linearization.
    let add_orders = [
        vec![n(A, 0), n(C, 0), n(C, 1), n(B, 0)],
        vec![n(A, 0), n(C, 0), n(B, 0), n(C, 1)],
        vec![n(A, 0), n(B, 0), n(C, 0), n(C, 1)],
    ];
    for add_order in &add_orders {
        assert_eq!(linearizer_order(&steps_map, add_order), expected, "order() example");
        assert_eq!(topolist_oracle(&direct, add_order), expected, "oracle example");
    }
}

/// The canonical `DESIGN.md` DAGs (linear chain, branch tiebreak, the recursive example):
/// our priority-Kahn `order()`, the topolist oracle, and the hand-derived expected order all
/// agree. This pins ADR-0014's "priority-Kahn reproduces the canonical linearizations".
#[test]
fn priority_kahn_matches_topolist_on_design_dags() {
    let cases: Vec<(Vec<(NodeId, Vec<NodeId>)>, Vec<NodeId>)> = vec![
        // a0 - b0 - c0 - a1 - b1 linearizes to itself.
        (
            vec![
                (n(A, 0), vec![]),
                (n(B, 0), vec![n(A, 0)]),
                (n(C, 0), vec![n(B, 0)]),
                (n(A, 1), vec![n(C, 0)]),
                (n(B, 1), vec![n(A, 1)]),
            ],
            vec![n(A, 0), n(B, 0), n(C, 0), n(A, 1), n(B, 1)],
        ),
        // branch: c0 sees {a0, b0}; a<b ⇒ tails order [a0, b0].
        (
            vec![
                (n(A, 0), vec![]),
                (n(B, 0), vec![]),
                (n(C, 0), vec![n(A, 0), n(B, 0)]),
                (n(A, 1), vec![n(C, 0)]),
            ],
            vec![n(A, 0), n(B, 0), n(C, 0), n(A, 1)],
        ),
        // DESIGN.md recursive example ⇒ [a0, c0, a1, b0, b1, c1, b2].
        (
            vec![
                (n(A, 0), vec![]),
                (n(C, 0), vec![]),
                (n(B, 0), vec![n(A, 0), n(C, 0)]),
                (n(A, 1), vec![n(C, 0)]),
                (n(C, 1), vec![n(B, 0)]),
                (n(B, 1), vec![n(A, 1)]),
                (n(B, 2), vec![n(C, 1)]),
            ],
            vec![n(A, 0), n(C, 0), n(A, 1), n(B, 0), n(B, 1), n(C, 1), n(B, 2)],
        ),
    ];

    for (steps, expected) in cases {
        let steps_map: BTreeMap<NodeId, Vec<NodeId>> = steps.iter().cloned().collect();
        let direct = direct_deps(&steps);
        let creation: Vec<NodeId> = steps.iter().map(|(node, _)| *node).collect();

        let got = linearizer_order(&steps_map, &creation);
        assert_eq!(got, expected, "order() vs hand-derived");
        assert_eq!(topolist_oracle(&direct, &creation), expected, "oracle vs hand-derived");
        assert_eq!(got, topolist_oracle(&direct, &creation), "order() vs oracle");
    }
}

/// Over a battery of seeded random fork/merge DAGs: the topolist oracle is itself
/// **delivery-order independent** (upstream's `stable ordering` / `fuzz` property), and it
/// agrees **node-for-node** with our priority-Kahn `order()` — the host-safe, in-Rust
/// analogue of the algorithmic-equivalence oracle for the non-optimistic case (ADR-0027).
#[test]
fn priority_kahn_matches_topolist_over_random_dags() {
    let n_writers = 5;
    let n_nodes = 30;
    let deliveries = 4;

    let mut nontrivial = 0usize; // seeds whose order actually reorders creation order

    for seed in 0..200u64 {
        let mut rng = Rng::new(0x70F0_0000_0000_0001 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        // Alternate sparse (more partitioning) and dense (more merging) reference regimes.
        let (num, den) = if seed % 2 == 0 { (3, 5) } else { (17, 20) };
        let steps = gen_dag(n_writers, n_nodes, num, den, &mut rng);
        let steps_map: BTreeMap<NodeId, Vec<NodeId>> = steps.iter().cloned().collect();
        let direct = direct_deps(&steps);
        let creation: Vec<NodeId> = steps.iter().map(|(node, _)| *node).collect();

        // Reference: priority-Kahn order() (insertion-order independent by construction).
        let order = linearizer_order(&steps_map, &creation);
        assert_eq!(order.len(), n_nodes, "every node ordered (seed {seed})");
        assert_causal(&direct, &order);
        if order != creation {
            nontrivial += 1;
        }

        // Oracle over creation order must equal order().
        assert_eq!(
            topolist_oracle(&direct, &creation),
            order,
            "topolist oracle vs priority-Kahn (seed {seed}, creation order)"
        );

        // Oracle over several random causal delivery orders: stable (== itself) and == order().
        for d in 0..deliveries {
            let delivery = random_topo(&direct, &mut rng);
            assert_causal(&direct, &delivery); // the replay order is itself causal
            let oracle = topolist_oracle(&direct, &delivery);
            assert_eq!(
                oracle, order,
                "topolist oracle is delivery-order independent and == order() (seed {seed}, delivery {d})"
            );
            // and our linearizer reproduces it under the same delivery order too
            assert_eq!(
                linearizer_order(&steps_map, &delivery),
                order,
                "order() is delivery-order independent (seed {seed}, delivery {d})"
            );
        }
    }

    assert!(
        nontrivial > 0,
        "no seed reordered creation order — the equivalence check was vacuous; make the DAGs forkier"
    );
}
