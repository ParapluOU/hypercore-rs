//! Port of the **view materialization** behaviour in `reference/js/autobase/test/linearizer.js`
//! and `dags.js` — the `view` / `view.get(i)` / `getIndexedViewLength` assertions.
//!
//! Upstream linearizes the multi-writer DAG and *applies* each node to materialize a `view`
//! (a hypercore the consumer reads). The tests assert three things about it:
//!   - `view.length` — the total materialized length (every linearized node contributes);
//!   - `view.get(i)` — the entry at position `i` (and `null` past the end);
//!   - `getIndexedViewLength` — how much of the view is **confirmed** (the indexed prefix
//!     that can never reorder), and that every replica agrees on it.
//!
//! At L1 the apply step is domain logic we deliberately don't have (the layer is
//! content-blind), so the materialization is the identity one: each node contributes one
//! entry — its own [`NodeId`]. The "view" is then [`Linearizer::view`] (≡ `order`) and the
//! "indexed view" is [`Linearizer::indexed_view`] (≡ `finalized`). See ADR-0028.
//!
//! ## What is and isn't ported here
//!
//! - **Ported, asserting the exact upstream numbers:** `linearizer - simple` / `dags - simple 3`
//!   — the fork-free `c-b-a-c-b-a` indexer chain, where `view.length == 6`,
//!   `getIndexedViewLength == 6`'s confirmed prefix is `4`, the per-index `view.get` sequence,
//!   and `view.get(6) == null`. For a fork-free chain our conservative double-quorum
//!   confirmation (ADR-0015) equals upstream's confirmed length exactly.
//! - **Ported as a property (holds for *every* DAG):** all replicas agree on the view and on
//!   the indexed view length regardless of causally-valid delivery order (the
//!   `getIndexedViewLength(a) == getIndexedViewLength(b) == getIndexedViewLength(c)` family),
//!   and the indexed view is always a prefix of the view.
//! - **Deferred (not asserted here):** the indexed-length *values* for cases where upstream
//!   confirms earlier than our conservative form — a unanimous single quorum (`dags - simple 2`,
//!   `n == 2`) and confirmation across a resolved fork/merge (`linearizer - compete` /
//!   `count ordering`). Those need the deferred fork/merge consensus (ADR-0015), as does the
//!   per-replica *partial* view (each base seeing a different node subset before full sync).
//!   Cross-replica view convergence — which *does* hold for those DAGs — is the property test
//!   above and in `convergence.rs`.

use std::collections::{BTreeMap, BTreeSet};

use autobase::{Linearizer, NodeId, WriterKey};

// Writers a=1 < b=2 < c=3 byte-lexicographically (matches DESIGN.md / linearizer.js a<b<c).
fn n(key: u8, seq: u64) -> NodeId {
    NodeId::new([key; 32], seq)
}
const A: u8 = 1;
const B: u8 = 2;
const C: u8 = 3;

fn indexed(keys: &[u8]) -> Linearizer {
    Linearizer::with_indexers(keys.iter().map(|&k| [k; 32] as WriterKey))
}

/// The `linearizer - simple` / `dags - simple 3` DAG: `c-b-a-c-b-a`, each append made after a
/// full sync so it causally sees everything before it (a total causal order). Returned in
/// creation order with each node's cross-writer heads (same-writer predecessors are implicit).
fn simple_chain() -> Vec<(NodeId, Vec<NodeId>)> {
    vec![
        (n(C, 0), vec![]),
        (n(B, 0), vec![n(C, 0)]),
        (n(A, 0), vec![n(B, 0)]),
        (n(C, 1), vec![n(A, 0)]), // + implicit c0
        (n(B, 1), vec![n(C, 1)]), // + implicit b0
        (n(A, 1), vec![n(B, 1)]), // + implicit a0
    ]
}

fn build(indexers: &[u8], steps: &[(NodeId, Vec<NodeId>)], add_order: &[usize]) -> Linearizer {
    let mut lin = indexed(indexers);
    for &i in add_order {
        let (node, heads) = &steps[i];
        lin.add(*node, heads).expect("causal delivery order must add cleanly");
    }
    lin
}

/// `direct[a]` = a's cross-writer heads ∪ same-writer predecessor — the edges Kahn walks.
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

/// Every causally-valid permutation of `steps` (by index) — for the small DAGs here we just
/// enumerate a few hand-picked valid delivery orders that exercise reordering of concurrent
/// nodes.
fn assert_view_prefix_invariant(lin: &Linearizer) {
    let view = lin.view();
    let indexed = lin.indexed_view();
    assert!(view.starts_with(&indexed), "indexed view must be a prefix of the view");
    assert_eq!(lin.view_len(), view.len(), "view_len == |view|");
    assert_eq!(lin.indexed_view_len(), indexed.len(), "indexed_view_len == |indexed_view|");
    // view_get agrees with view() index-for-index, and is None exactly past the end.
    for (i, node) in view.iter().enumerate() {
        assert_eq!(lin.view_get(i), Some(*node), "view_get({i})");
    }
    assert_eq!(lin.view_get(view.len()), None, "view_get past the end is None");
}

// ----------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------

/// `linearizer - simple` (also `dags - simple 3`): the fork-free `c-b-a-c-b-a` indexer chain.
/// The materialized view is the full linearization `[c0,b0,a0,c1,b1,a1]` (length 6) and the
/// **indexed** (confirmed) view length is 4 — `[c0,b0,a0,c1]` are double-quorum'd, `b1,a1` are
/// not yet. (Upstream: `view.length == 6`, `getIndexedViewLength == 4`.)
#[test]
fn simple_chain_view_and_indexed_length() {
    let steps = simple_chain();
    let lin = build(&[A, B, C], &steps, &[0, 1, 2, 3, 4, 5]);

    let expected_view = vec![n(C, 0), n(B, 0), n(A, 0), n(C, 1), n(B, 1), n(A, 1)];
    assert_eq!(lin.view(), expected_view, "full materialized view");
    assert_eq!(lin.view_len(), 6, "view.length");

    // view.get(i) per upstream's per-index assertions.
    assert_eq!(lin.view_get(0), Some(n(C, 0)));
    assert_eq!(lin.view_get(1), Some(n(B, 0)));
    assert_eq!(lin.view_get(2), Some(n(A, 0)));
    assert_eq!(lin.view_get(3), Some(n(C, 1)));
    assert_eq!(lin.view_get(4), Some(n(B, 1)));
    assert_eq!(lin.view_get(5), Some(n(A, 1)));
    assert_eq!(lin.view_get(6), None, "view.get(6, {{wait:false}}) == null");

    // getIndexedViewLength == 4: the double-quorum'd prefix [c0,b0,a0,c1].
    assert_eq!(lin.indexed_view_len(), 4, "getIndexedViewLength");
    assert_eq!(lin.indexed_view(), vec![n(C, 0), n(B, 0), n(A, 0), n(C, 1)]);

    // One tail (the chain root c0), as upstream asserts `linearizer.tails.size == 1`.
    assert_eq!(lin.tails(), [n(C, 0)].into_iter().collect());

    assert_view_prefix_invariant(&lin);
}

/// The recursive `DESIGN.md` DAG (forks: `a0` and `c0` are concurrent tails). The materialized
/// view is the canonical `[a0,c0,a1,b0,b1,c1,b2]`, and across several causally-valid delivery
/// orders the view, the per-index entries, and the indexed view length all converge and the
/// indexed view stays a prefix. We assert convergence + the prefix invariant (which hold for
/// *any* DAG) but **not** a specific confirmed length: a forked DAG's confirmed depth depends
/// on the deferred fork/merge consensus (ADR-0015), so pinning the number is out of scope.
#[test]
fn recursive_dag_view_converges() {
    // a0 c0 | b0={a0,c0} | a1={c0}(+a0) | c1={b0}(+c0) | b1={a1}(+b0) | b2={c1}(+b1)
    let steps = vec![
        (n(A, 0), vec![]),
        (n(C, 0), vec![]),
        (n(B, 0), vec![n(A, 0), n(C, 0)]),
        (n(A, 1), vec![n(C, 0)]),
        (n(C, 1), vec![n(B, 0)]),
        (n(B, 1), vec![n(A, 1)]),
        (n(B, 2), vec![n(C, 1)]),
    ];
    let expected_view = vec![
        n(A, 0),
        n(C, 0),
        n(A, 1),
        n(B, 0),
        n(B, 1),
        n(C, 1),
        n(B, 2),
    ];

    // Three causally-valid delivery orders (same set as the lib determinism test).
    let add_orders: [Vec<usize>; 3] = [
        vec![0, 1, 2, 3, 4, 5, 6],
        vec![1, 0, 3, 2, 4, 5, 6],
        vec![1, 0, 2, 4, 3, 5, 6],
    ];

    let direct = direct_deps(&steps);
    let mut indexed_lens = Vec::new();
    for p in &add_orders {
        let lin = build(&[A, B, C], &steps, p);
        assert_eq!(lin.view(), expected_view, "view is the canonical linearization");
        // causal-respect: every dep precedes its dependent in the materialized view.
        let pos: BTreeMap<NodeId, usize> =
            lin.view().iter().enumerate().map(|(i, x)| (*x, i)).collect();
        for (node, ds) in &direct {
            for d in ds {
                assert!(pos[d] < pos[node], "view respects causality: {d:?} before {node:?}");
            }
        }
        assert_view_prefix_invariant(&lin);
        indexed_lens.push(lin.indexed_view_len());
    }
    // All delivery orders agree on the confirmed length (whatever it is) — convergence.
    assert!(
        indexed_lens.windows(2).all(|w| w[0] == w[1]),
        "indexed view length converges across delivery orders: {indexed_lens:?}"
    );
}

/// A non-indexing writer's nodes still appear in the materialized view (they are ordered),
/// but never push the indexed view forward (they cast no vote) — the view/indexed split is
/// orthogonal to who is an indexer.
#[test]
fn non_indexer_nodes_are_in_view_but_do_not_index() {
    // Only a & b index; c is a non-indexing writer whose node is still linearized.
    let mut lin = indexed(&[A, B]);
    lin.add(n(A, 0), &[]).unwrap();
    lin.add(n(C, 0), &[n(A, 0)]).unwrap(); // non-indexer c references a0

    assert!(lin.view().contains(&n(C, 0)), "non-indexer node is in the view");
    assert_eq!(lin.view_len(), 2);
    // a0 has only its own indexer vote so far ⇒ nothing confirmed.
    assert_eq!(lin.indexed_view_len(), 0, "non-indexer reference does not index");
    assert_view_prefix_invariant(&lin);
}
