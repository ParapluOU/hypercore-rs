use super::*;
use identity::SecretKey;
use storage::MemoryStore;

fn bee() -> Hyperbee<MemoryStore> {
    Hyperbee::new(SecretKey::from_seed(&[7; 32]), MemoryStore::new())
}

#[test]
fn put_get_roundtrip_and_overwrite() {
    let mut b = bee();
    assert!(b.is_empty());
    assert_eq!(b.get(b"missing").unwrap(), None);

    for (k, v) in [(&b"foo"[..], &b"1"[..]), (b"bar", b"2"), (b"baz", b"3")] {
        b.put(k, v).unwrap();
    }
    assert_eq!(b.version(), 3, "copy-on-write: each put appends a fresh rewritten leaf block; latest is the root");
    assert_eq!(b.get(b"foo").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(b.get(b"bar").unwrap().as_deref(), Some(&b"2"[..]));
    assert_eq!(b.get(b"nope").unwrap(), None);

    // overwrite (LWW)
    b.put(b"foo", b"updated").unwrap();
    assert_eq!(b.get(b"foo").unwrap().as_deref(), Some(&b"updated"[..]));
}

#[test]
fn in_order_after_unsorted_inserts_and_splits() {
    let mut b = bee();
    // 20 keys inserted in reverse → forces multi-level splits
    let key = |i: u32| format!("{i:02}").into_bytes();
    for i in (0..20u32).rev() {
        b.put(&key(i), &key(i)).unwrap();
    }
    let all = b.range(&Range::default()).unwrap();
    let keys: Vec<Vec<u8>> = all.into_iter().map(|(k, _)| k).collect();
    let expect: Vec<Vec<u8>> = (0..20).map(key).collect();
    assert_eq!(keys, expect, "B-tree yields sorted order across splits");
    // every key still retrievable through the multi-level tree
    for i in 0..20 {
        assert_eq!(b.get(&key(i)).unwrap(), Some(key(i)));
    }
    assert!(b.version() > 1, "splits appended multiple blocks");
}

// Upstream basic.js exhaustive range oracle: for sizes 1..=25, every combination
// of {gt|gte} x {lt|lte} x {reverse} over every pair of anchor keys must equal a
// sorted-reference slice. This also exercises splitting + multi-level descent.
#[test]
fn exhaustive_range_matches_reference() {
    let key = |i: u32| format!("{i:02}").into_bytes();
    for n in 1..=25u32 {
        let mut b = bee();
        for i in (0..n).rev() {
            b.put(&key(i), &key(i)).unwrap();
        }
        let all: Vec<Vec<u8>> = (0..n).map(key).collect(); // already sorted

        for a in 0..n {
            for c in 0..n {
                for &use_gt in &[false, true] {
                    for &use_lt in &[false, true] {
                        for &reverse in &[false, true] {
                            let lo = key(a);
                            let hi = key(c);
                            let bounds = Range {
                                gt: use_gt.then(|| lo.clone()),
                                gte: (!use_gt).then(|| lo.clone()),
                                lt: use_lt.then(|| hi.clone()),
                                lte: (!use_lt).then(|| hi.clone()),
                                reverse,
                                limit: None,
                            };
                            let mut expect: Vec<Vec<u8>> = all
                                .iter()
                                .filter(|k| {
                                    let k = k.as_slice();
                                    (if use_gt { k > &lo[..] } else { k >= &lo[..] })
                                        && (if use_lt { k < &hi[..] } else { k <= &hi[..] })
                                })
                                .cloned()
                                .collect();
                            if reverse {
                                expect.reverse();
                            }
                            let got: Vec<Vec<u8>> = b
                                .range(&bounds)
                                .unwrap()
                                .into_iter()
                                .map(|(k, _)| k)
                                .collect();
                            assert_eq!(
                                got, expect,
                                "n={n} a={a} c={c} gt={use_gt} lt={use_lt} rev={reverse}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn range_limit_and_open_bounds() {
    let mut b = bee();
    let key = |i: u32| format!("{i:02}").into_bytes();
    for i in 0..10u32 {
        b.put(&key(i), &key(i)).unwrap();
    }
    let first3 = b
        .range(&Range {
            limit: Some(3),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        first3.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        (0..3).map(key).collect::<Vec<_>>()
    );
    // descending, open bounds
    let desc = b
        .range(&Range {
            reverse: true,
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        desc.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        vec![key(9), key(8)]
    );
}

// ---- delete -------------------------------------------------------------

/// Walk the tree asserting every B-tree invariant, and return its keys in
/// in-order (which must therefore be strictly sorted): per-node sorted entries;
/// non-root nodes hold `[MIN_KEYS, MAX_CHILDREN-1]` keys; internal nodes have
/// `entries+1` children; all leaves sit at the same depth.
fn invariants(b: &Hyperbee<MemoryStore>) -> Vec<Vec<u8>> {
    fn walk(
        b: &Hyperbee<MemoryStore>,
        seq: u64,
        depth: usize,
        is_root: bool,
        leaf_depth: &mut Option<usize>,
        out: &mut Vec<Vec<u8>>,
    ) {
        let node = b.node(seq).unwrap();
        for w in node.entries.windows(2) {
            assert!(w[0].0 < w[1].0, "node entries must be strictly sorted");
        }
        assert!(node.entries.len() <= MAX_CHILDREN - 1, "overfull node");
        if !is_root {
            assert!(
                node.entries.len() >= MIN_KEYS,
                "non-root underflow: {} keys",
                node.entries.len()
            );
        }
        if node.is_leaf() {
            match leaf_depth {
                None => *leaf_depth = Some(depth),
                Some(d) => assert_eq!(*d, depth, "all leaves must be at the same depth"),
            }
            out.extend(node.entries.iter().map(|(k, _)| k.clone()));
        } else {
            assert_eq!(
                node.children.len(),
                node.entries.len() + 1,
                "internal node child count"
            );
            for i in 0..node.entries.len() {
                walk(b, node.children[i], depth + 1, false, leaf_depth, out);
                out.push(node.entries[i].0.clone());
            }
            walk(b, node.children[node.entries.len()], depth + 1, false, leaf_depth, out);
        }
    }
    let mut out = Vec::new();
    let mut leaf_depth = None;
    if let Some(root) = b.root_seq() {
        walk(b, root, 0, true, &mut leaf_depth, &mut out);
    }
    for w in out.windows(2) {
        assert!(w[0] < w[1], "in-order traversal must be strictly sorted");
    }
    out
}

fn all_pairs(b: &Hyperbee<MemoryStore>) -> Vec<(Vec<u8>, Vec<u8>)> {
    b.range(&Range::default()).unwrap()
}

#[test]
fn del_basic_present_absent_and_idempotent() {
    let mut b = bee();
    assert!(!b.del(b"x").unwrap(), "delete on an empty tree is a no-op");
    for (k, v) in [(&b"a"[..], &b"1"[..]), (b"b", b"2"), (b"c", b"3")] {
        b.put(k, v).unwrap();
    }
    let v0 = b.version();
    assert!(!b.del(b"zzz").unwrap(), "absent key → false");
    assert_eq!(b.version(), v0, "a 404 delete appends nothing");

    assert!(b.del(b"b").unwrap(), "present key → true");
    assert_eq!(b.get(b"b").unwrap(), None);
    assert_eq!(b.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(b.get(b"c").unwrap().as_deref(), Some(&b"3"[..]));
    assert!(!b.del(b"b").unwrap(), "deleting it again → false");

    assert!(b.del(b"a").unwrap());
    assert!(b.del(b"c").unwrap());
    assert_eq!(all_pairs(&b), vec![], "tree drained to empty");
    // an emptied tree is still usable
    b.put(b"d", b"4").unwrap();
    assert_eq!(b.get(b"d").unwrap().as_deref(), Some(&b"4"[..]));
}

#[test]
fn del_drains_a_multi_level_tree_keeping_invariants() {
    let key = |i: u32| format!("k{i:03}").into_bytes();
    let mut b = bee();
    // 80 keys → a genuine multi-level tree (forces internal-node deletes,
    // borrows, merges and a root shrink as it drains).
    for i in 0..80u32 {
        b.put(&key(i), &key(i)).unwrap();
    }
    invariants(&b);

    // Delete in a scrambled order; check every invariant after each delete.
    let mut order: Vec<u32> = (0..80).collect();
    let mut s = 0x1234_5678u64;
    for i in (1..order.len()).rev() {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        order.swap(i, (s >> 33) as usize % (i + 1));
    }

    let mut remaining: std::collections::BTreeSet<u32> = (0..80).collect();
    for &i in &order {
        assert!(b.del(&key(i)).unwrap(), "delete {i} present");
        remaining.remove(&i);
        let keys = invariants(&b);
        let expect: Vec<Vec<u8>> = remaining.iter().cloned().map(key).collect();
        assert_eq!(keys, expect, "tree keys after deleting {i}");
        assert_eq!(b.get(&key(i)).unwrap(), None);
    }
    assert_eq!(all_pairs(&b), vec![]);
}

#[test]
fn del_randomized_against_btreemap_oracle() {
    use std::collections::BTreeMap;
    let key = |i: u64| format!("k{i:03}").into_bytes();
    let mut b = bee();
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    const SPACE: u64 = 90; // big enough for a 3-level tree
    let mut s = 0xC0FFEEu64;
    let mut next = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        s >> 17
    };

    for op in 0..1200u64 {
        let k = key(next() % SPACE);
        if next() % 2 == 0 {
            // put with a value that varies, so overwrites are observable
            let v = format!("v{op}").into_bytes();
            b.put(&k, &v).unwrap();
            oracle.insert(k, v);
        } else {
            let had = oracle.remove(&k).is_some();
            assert_eq!(b.del(&k).unwrap(), had, "del return matches oracle (op {op})");
        }
        // Full structural + content equivalence after every op.
        let keys = invariants(&b);
        let expect_keys: Vec<Vec<u8>> = oracle.keys().cloned().collect();
        assert_eq!(keys, expect_keys, "key set diverged at op {op}");
        let expect_pairs: Vec<(Vec<u8>, Vec<u8>)> =
            oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(all_pairs(&b), expect_pairs, "values diverged at op {op}");
    }
    // Sanity: the run actually built a tall tree at some point and exercised
    // the rebalancing paths (not just a single leaf).
    assert!(b.version() > 50, "the randomized run did substantial work");
}

#[test]
fn del_then_get_and_range_stay_consistent_under_reinsert() {
    let key = |i: u32| format!("k{i:03}").into_bytes();
    let mut b = bee();
    for i in 0..40u32 {
        b.put(&key(i), &key(i)).unwrap();
    }
    // delete every even key
    for i in (0..40u32).step_by(2) {
        assert!(b.del(&key(i)).unwrap());
    }
    invariants(&b);
    let odds: Vec<Vec<u8>> = (1..40u32).step_by(2).map(key).collect();
    assert_eq!(
        all_pairs(&b).into_iter().map(|(k, _)| k).collect::<Vec<_>>(),
        odds
    );
    // re-insert the evens; tree must heal back to the full set
    for i in (0..40u32).step_by(2) {
        b.put(&key(i), &key(i)).unwrap();
    }
    invariants(&b);
    let all: Vec<Vec<u8>> = (0..40u32).map(key).collect();
    assert_eq!(
        all_pairs(&b).into_iter().map(|(k, _)| k).collect::<Vec<_>>(),
        all
    );
}

// ---- checkout / get_at / range_at / peek / cas (gap-fill) -----------------

#[test]
fn checkout_reads_a_historic_version() {
    let mut b = bee();
    b.put(b"a", b"1").unwrap();
    b.put(b"b", b"2").unwrap();
    let v = b.version(); // an op-boundary version
    b.put(b"c", b"3").unwrap();
    b.put(b"a", b"updated").unwrap();

    // current sees all + the overwrite
    assert_eq!(b.get(b"a").unwrap().as_deref(), Some(&b"updated"[..]));
    assert_eq!(b.get(b"c").unwrap().as_deref(), Some(&b"3"[..]));

    // the checkout sees the tree as it was: a=1, b=2, no c
    let co = b.checkout(v);
    assert_eq!(co.version(), v);
    assert_eq!(co.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(co.get(b"b").unwrap().as_deref(), Some(&b"2"[..]));
    assert_eq!(co.get(b"c").unwrap(), None);
    let keys: Vec<Vec<u8>> = co.range(&Range::default()).unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);

    // get_at / range_at convenience, and version 0 = empty
    assert_eq!(b.get_at(v, b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(b.get_at(v, b"c").unwrap(), None);
    assert_eq!(b.range_at(v, &Range::default()).unwrap().len(), 2);
    assert_eq!(b.checkout(0).get(b"a").unwrap(), None);
}

#[test]
fn peek_returns_first_or_last_within_bounds() {
    let key = |i: u32| format!("k{i:02}").into_bytes();
    let mut b = bee();
    for i in (0..10u32).rev() {
        b.put(&key(i), &key(i)).unwrap();
    }
    assert_eq!(b.peek(&Range::default()).unwrap(), Some((key(0), key(0))));
    assert_eq!(
        b.peek(&Range { reverse: true, ..Default::default() }).unwrap(),
        Some((key(9), key(9)))
    );
    assert_eq!(
        b.peek(&Range { gte: Some(key(5)), ..Default::default() }).unwrap(),
        Some((key(5), key(5)))
    );
    assert_eq!(bee().peek(&Range::default()).unwrap(), None);
}

#[test]
fn cas_put_and_del() {
    let mut b = bee();
    // expected-absent applies once, then fails (key now present)
    assert!(b.put_cas(b"k", b"v1", None).unwrap());
    assert!(!b.put_cas(b"k", b"v2", None).unwrap());
    assert_eq!(b.get(b"k").unwrap().as_deref(), Some(&b"v1"[..]));
    // matching value swaps; mismatching value is a no-op
    assert!(b.put_cas(b"k", b"v2", Some(b"v1")).unwrap());
    assert!(!b.put_cas(b"k", b"v3", Some(b"WRONG")).unwrap());
    assert_eq!(b.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
    // del only on a value match
    assert!(!b.del_cas(b"k", Some(b"WRONG")).unwrap());
    assert!(b.get(b"k").unwrap().is_some());
    assert!(b.del_cas(b"k", Some(b"v2")).unwrap());
    assert_eq!(b.get(b"k").unwrap(), None);
}
