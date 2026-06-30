//! `hyperbee` — an ordered key/value B-tree over a `hypercore`.
//!
//! Keys are kept in sorted (bytewise) order in a B-tree whose nodes are stored as
//! blocks in an append-only [`Hypercore`]. The tree is **copy-on-write**: a `put`
//! never mutates a block — it rewrites the leaf→root path as new blocks and the
//! **new root is the latest block** (no separate root pointer; latest wins).
//!
//! ## Clean-room divergences from upstream (ADR-0030)
//!
//! Behaviourally faithful, simpler on the wire (we are not format-compatible —
//! ADR-0001):
//! - **One block = one B-tree node.** Upstream packs the whole rewritten path into
//!   a single block's `YoloIndex` (a block-count optimization) and addresses nodes
//!   by `(seq, offset)`. We give each node its own block, so a child pointer is
//!   just a `seq` (`u64`). A `put` therefore appends one block per path node.
//! - **Inline key+value.** Upstream stores the key/value in the entry block and
//!   references them from nodes by `seq`; we store them inline in the node. Simpler;
//!   the trade-off is a key's bytes are re-encoded when its node is rewritten.
//! - **No header block** (no `isHyperbee`/protocol metadata) — `version()` is just
//!   the block count; an empty tree is `version 0`.
//! - **Split threshold matches upstream** (`MAX_CHILDREN = 9`): a node splits once
//!   it would hold 9 keys.
//!
//! v1 scope: `put` / `get` / `range` (asc + desc, gt/gte/lt/lte/limit). Deferred:
//! `del` + rebalance, sub-databases, the header, diff/history/watch.

use codec::{varint, Codec};
use hypercore::{Error as HcError, Hypercore};
use identity::SecretKey;
use storage::Store;

/// A node splits once it would hold this many keys (upstream `MAX_CHILDREN`).
const MAX_CHILDREN: usize = 9;

/// Minimum keys a **non-root** node may hold; below this it borrows from a sibling
/// or merges (upstream `MIN_KEYS = (MAX_CHILDREN - 1) / 2`). A fresh split yields
/// exactly `MIN_KEYS` keys per half, so `[MIN_KEYS, MAX_CHILDREN - 1]` is the legal
/// fill range for a non-root node.
const MIN_KEYS: usize = (MAX_CHILDREN - 1) / 2;

/// A B-tree node: sorted `entries` and, for an internal node, `children` block
/// seqs. A leaf has empty `children`; an internal node has
/// `children.len() == entries.len() + 1`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Node {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    children: Vec<u64>,
}

impl Node {
    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// Length-prefixed encoding of a [`Node`] (varints + length-delimited bytes).
#[derive(Clone, Copy, Default)]
struct NodeCodec;

impl Codec<Node> for NodeCodec {
    fn encode_into(&self, node: &Node, out: &mut Vec<u8>) {
        varint::write(out, node.entries.len() as u64);
        for (k, v) in &node.entries {
            varint::write(out, k.len() as u64);
            out.extend_from_slice(k);
            varint::write(out, v.len() as u64);
            out.extend_from_slice(v);
        }
        varint::write(out, node.children.len() as u64);
        for c in &node.children {
            varint::write(out, *c);
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<Node, codec::Error> {
        let mut b = bytes;
        let take = |b: &mut &[u8], n: usize| -> Result<Vec<u8>, codec::Error> {
            if b.len() < n {
                return Err(codec::Error::Eof);
            }
            let (head, rest) = b.split_at(n);
            *b = rest;
            Ok(head.to_vec())
        };

        let ne = varint::read(&mut b)? as usize;
        let mut entries = Vec::with_capacity(ne);
        for _ in 0..ne {
            let kl = varint::read(&mut b)? as usize;
            let k = take(&mut b, kl)?;
            let vl = varint::read(&mut b)? as usize;
            let v = take(&mut b, vl)?;
            entries.push((k, v));
        }
        let nc = varint::read(&mut b)? as usize;
        let mut children = Vec::with_capacity(nc);
        for _ in 0..nc {
            children.push(varint::read(&mut b)?);
        }
        Ok(Node { entries, children })
    }
}

/// Errors from a [`Hyperbee`] — the underlying [`Hypercore`]'s error.
pub type Error<S> = HcError<<S as Store>::Error>;

/// Range bounds for [`Hyperbee::range`]. Unset bounds are open. Bytewise order.
#[derive(Clone, Debug, Default)]
pub struct Range {
    pub gt: Option<Vec<u8>>,
    pub gte: Option<Vec<u8>>,
    pub lt: Option<Vec<u8>>,
    pub lte: Option<Vec<u8>>,
    pub reverse: bool,
    pub limit: Option<usize>,
}

/// Result of inserting into a subtree: either the rewritten node's new seq, or a
/// split that pushes a median entry (and a new right sibling) up to the parent.
enum Ins {
    Down(u64),
    Split { left: u64, median: (Vec<u8>, Vec<u8>), right: u64 },
}

/// Result of deleting from a subtree: either nothing was present, or the subtree
/// was rewritten to `seq` and possibly fell below [`MIN_KEYS`] (so the parent must
/// rebalance it).
enum Del {
    NotFound,
    Down { seq: u64, underflow: bool },
}

/// An ordered key/value store: a copy-on-write B-tree over a `hypercore`.
pub struct Hyperbee<S> {
    core: Hypercore<Node, NodeCodec, S>,
}

impl<S: Store> Hyperbee<S> {
    /// Create an empty tree written by `author`, stored in `store`.
    pub fn new(author: SecretKey, store: S) -> Self {
        Self {
            core: Hypercore::new(author, NodeCodec, store),
        }
    }

    /// The version = number of blocks appended (0 for an empty tree).
    pub fn version(&self) -> u64 {
        self.core.len()
    }

    pub fn is_empty(&self) -> bool {
        self.core.len() == 0
    }

    fn node(&self, seq: u64) -> Result<Node, Error<S>> {
        self.core.get(seq)?.ok_or(HcError::Corrupt)
    }

    fn root_seq(&self) -> Option<u64> {
        self.core.len().checked_sub(1)
    }

    /// The value for `key`, or `None`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
        let mut seq = match self.root_seq() {
            Some(s) => s,
            None => return Ok(None),
        };
        loop {
            let node = self.node(seq)?;
            match node.entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                Ok(i) => return Ok(Some(node.entries[i].1.clone())),
                Err(i) => {
                    if node.is_leaf() {
                        return Ok(None);
                    }
                    seq = node.children[i];
                }
            }
        }
    }

    /// Insert or overwrite `key`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error<S>> {
        let root = match self.root_seq() {
            None => {
                let leaf = Node {
                    entries: vec![(key.to_vec(), value.to_vec())],
                    children: vec![],
                };
                self.core.append(&leaf)?;
                return Ok(());
            }
            Some(s) => s,
        };

        if let Ins::Split { left, median, right } = self.insert(root, key, value)? {
            // Root split: a fresh root holds the median and the two halves. It is
            // appended last, so it becomes the new latest-block root.
            let new_root = Node {
                entries: vec![median],
                children: vec![left, right],
            };
            self.core.append(&new_root)?;
        }
        // (the `Down` case already appended the rewritten root last)
        Ok(())
    }

    fn insert(&mut self, seq: u64, key: &[u8], value: &[u8]) -> Result<Ins, Error<S>> {
        let mut node = self.node(seq)?;
        match node.entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            // Key present at this node — overwrite the value (LWW), rewrite the node.
            Ok(i) => {
                node.entries[i].1 = value.to_vec();
                Ok(Ins::Down(self.core.append(&node)?))
            }
            Err(i) => {
                if node.is_leaf() {
                    node.entries.insert(i, (key.to_vec(), value.to_vec()));
                    self.finish(node)
                } else {
                    match self.insert(node.children[i], key, value)? {
                        Ins::Down(child) => {
                            node.children[i] = child;
                            Ok(Ins::Down(self.core.append(&node)?))
                        }
                        Ins::Split { left, median, right } => {
                            node.children[i] = left;
                            node.entries.insert(i, median);
                            node.children.insert(i + 1, right);
                            self.finish(node)
                        }
                    }
                }
            }
        }
    }

    /// Append `node`, splitting first if it now holds `MAX_CHILDREN` keys.
    fn finish(&mut self, mut node: Node) -> Result<Ins, Error<S>> {
        if node.entries.len() < MAX_CHILDREN {
            return Ok(Ins::Down(self.core.append(&node)?));
        }
        // Split: median moves up, right half becomes a new sibling.
        let mid = node.entries.len() / 2;
        let right_entries = node.entries.split_off(mid + 1);
        let median = node.entries.pop().expect("non-empty");
        let mut right = Node {
            entries: right_entries,
            children: Vec::new(),
        };
        if !node.is_leaf() {
            right.children = node.children.split_off(mid + 1);
        }
        let left = self.core.append(&node)?;
        let right = self.core.append(&right)?;
        Ok(Ins::Split { left, median, right })
    }

    /// All entries in key order within `bounds` (honouring reverse + limit).
    pub fn range(&self, bounds: &Range) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error<S>> {
        let mut out = Vec::new();
        if let Some(root) = self.root_seq() {
            self.collect(root, &mut out)?;
        }
        // Filter by bounds (bytewise).
        out.retain(|(k, _)| {
            let k = k.as_slice();
            bounds.gt.as_deref().map_or(true, |g| k > g)
                && bounds.gte.as_deref().map_or(true, |g| k >= g)
                && bounds.lt.as_deref().map_or(true, |l| k < l)
                && bounds.lte.as_deref().map_or(true, |l| k <= l)
        });
        if bounds.reverse {
            out.reverse();
        }
        if let Some(limit) = bounds.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// In-order traversal (yields entries in sorted key order).
    fn collect(&self, seq: u64, out: &mut Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), Error<S>> {
        let node = self.node(seq)?;
        if node.is_leaf() {
            out.extend(node.entries.iter().cloned());
        } else {
            for i in 0..node.entries.len() {
                self.collect(node.children[i], out)?;
                out.push(node.entries[i].clone());
            }
            self.collect(node.children[node.entries.len()], out)?;
        }
        Ok(())
    }

    /// Delete `key`. Returns `true` if it was present (and removed), `false` if it
    /// was absent (the tree is then untouched — no block appended). Copy-on-write:
    /// the root-to-leaf path plus any siblings touched by rebalancing are rewritten
    /// and appended, the new root last; an internal key is replaced by its in-order
    /// neighbour from a leaf, then nodes that fall below [`MIN_KEYS`] borrow from a
    /// sibling or merge, mirroring upstream `del`/`rebalance`.
    pub fn del(&mut self, key: &[u8]) -> Result<bool, Error<S>> {
        let root = match self.root_seq() {
            None => return Ok(false),
            Some(s) => s,
        };
        match self.delete(root, key)? {
            Del::NotFound => Ok(false),
            Del::Down { seq, .. } => {
                // Tree shrinks a level if the rewritten root is now an empty internal
                // node (a merge consumed its last key): its sole child becomes the new
                // root. Re-append that child so it is the latest block.
                let node = self.node(seq)?;
                if node.entries.is_empty() && !node.is_leaf() {
                    let child = self.node(node.children[0])?;
                    self.core.append(&child)?;
                }
                Ok(true)
            }
        }
    }

    /// Delete `key` from the subtree at `seq`, rewriting it copy-on-write. Returns
    /// [`Del::NotFound`] (nothing appended) or [`Del::Down`] with the rewritten
    /// subtree's new seq and whether it fell below [`MIN_KEYS`].
    fn delete(&mut self, seq: u64, key: &[u8]) -> Result<Del, Error<S>> {
        let mut node = self.node(seq)?;
        match node.entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            Ok(i) => {
                if node.is_leaf() {
                    node.entries.remove(i);
                    let under = node.entries.len() < MIN_KEYS;
                    Ok(Del::Down { seq: self.core.append(&node)?, underflow: under })
                } else {
                    // Replace the separator with an in-order neighbour pulled from a
                    // boundary leaf — from whichever side's boundary leaf has more
                    // keys (upstream `setKeyToNearestLeaf` via `leafSize`).
                    let left_child = node.children[i];
                    let right_child = node.children[i + 1];
                    let ls = self.boundary_leaf_len(left_child, false)?;
                    let rs = self.boundary_leaf_len(right_child, true)?;
                    if ls < rs {
                        let (succ, new_right, under) = self.delete_min(right_child)?;
                        node.entries[i] = succ;
                        node.children[i + 1] = new_right;
                        self.fix_child(node, i + 1, under)
                    } else {
                        let (pred, new_left, under) = self.delete_max(left_child)?;
                        node.entries[i] = pred;
                        node.children[i] = new_left;
                        self.fix_child(node, i, under)
                    }
                }
            }
            Err(i) => {
                if node.is_leaf() {
                    Ok(Del::NotFound)
                } else {
                    match self.delete(node.children[i], key)? {
                        Del::NotFound => Ok(Del::NotFound),
                        Del::Down { seq: child, underflow } => {
                            node.children[i] = child;
                            self.fix_child(node, i, underflow)
                        }
                    }
                }
            }
        }
    }

    /// After child `i` of `node` was rewritten (`child_under` = it underflowed),
    /// optionally rebalance it, then append `node` and report its own fill.
    fn fix_child(&mut self, mut node: Node, i: usize, child_under: bool) -> Result<Del, Error<S>> {
        if child_under {
            self.rebalance(&mut node, i)?;
        }
        let under = node.entries.len() < MIN_KEYS;
        Ok(Del::Down { seq: self.core.append(&node)?, underflow: under })
    }

    /// Restore [`MIN_KEYS`] for the under-full child `node.children[i]` by borrowing
    /// from a sibling (whichever has `> MIN_KEYS`) or, failing that, merging with one.
    /// Updates `node`'s entries/children (and may itself drop `node` below `MIN_KEYS`,
    /// which the caller reports upward). All touched nodes are appended COW.
    fn rebalance(&mut self, node: &mut Node, i: usize) -> Result<(), Error<S>> {
        // Borrow from the left sibling (rotate right through the separator at i-1).
        if i > 0 {
            let mut left = self.node(node.children[i - 1])?;
            if left.entries.len() > MIN_KEYS {
                let mut child = self.node(node.children[i])?;
                child.entries.insert(0, node.entries[i - 1].clone());
                if !left.is_leaf() {
                    let moved = left.children.pop().expect("internal has children");
                    child.children.insert(0, moved);
                }
                node.entries[i - 1] = left.entries.pop().expect("left has spare keys");
                node.children[i - 1] = self.core.append(&left)?;
                node.children[i] = self.core.append(&child)?;
                return Ok(());
            }
        }
        // Borrow from the right sibling (rotate left through the separator at i).
        if i + 1 < node.children.len() {
            let mut right = self.node(node.children[i + 1])?;
            if right.entries.len() > MIN_KEYS {
                let mut child = self.node(node.children[i])?;
                child.entries.push(node.entries[i].clone());
                if !right.is_leaf() {
                    let moved = right.children.remove(0);
                    child.children.push(moved);
                }
                node.entries[i] = right.entries.remove(0);
                node.children[i] = self.core.append(&child)?;
                node.children[i + 1] = self.core.append(&right)?;
                return Ok(());
            }
        }
        // No spare sibling — merge the child with one. `left.keys += [sep] + right.keys`,
        // `left.children += right.children`; the separator and right pointer leave `node`.
        let (li, ri) = if i > 0 { (i - 1, i) } else { (i, i + 1) };
        let mut left = self.node(node.children[li])?;
        let right = self.node(node.children[ri])?;
        left.entries.push(node.entries[li].clone());
        left.entries.extend(right.entries.iter().cloned());
        left.children.extend(right.children.iter().cloned());
        let merged = self.core.append(&left)?;
        node.entries.remove(li);
        node.children.remove(ri);
        node.children[li] = merged;
        Ok(())
    }

    /// Remove and return the smallest entry of the subtree at `seq` (COW). Returns
    /// `(entry, new_seq, underflow)`.
    fn delete_min(&mut self, seq: u64) -> Result<((Vec<u8>, Vec<u8>), u64, bool), Error<S>> {
        let mut node = self.node(seq)?;
        if node.is_leaf() {
            let min = node.entries.remove(0);
            let under = node.entries.len() < MIN_KEYS;
            Ok((min, self.core.append(&node)?, under))
        } else {
            let (min, new_child, child_under) = self.delete_min(node.children[0])?;
            node.children[0] = new_child;
            if child_under {
                self.rebalance(&mut node, 0)?;
            }
            let under = node.entries.len() < MIN_KEYS;
            Ok((min, self.core.append(&node)?, under))
        }
    }

    /// Remove and return the largest entry of the subtree at `seq` (COW). Returns
    /// `(entry, new_seq, underflow)`.
    fn delete_max(&mut self, seq: u64) -> Result<((Vec<u8>, Vec<u8>), u64, bool), Error<S>> {
        let mut node = self.node(seq)?;
        if node.is_leaf() {
            let max = node.entries.pop().expect("non-empty leaf");
            let under = node.entries.len() < MIN_KEYS;
            Ok((max, self.core.append(&node)?, under))
        } else {
            let last = node.children.len() - 1;
            let (max, new_child, child_under) = self.delete_max(node.children[last])?;
            node.children[last] = new_child;
            if child_under {
                self.rebalance(&mut node, last)?;
            }
            let under = node.entries.len() < MIN_KEYS;
            Ok((max, self.core.append(&node)?, under))
        }
    }

    /// Number of keys in the boundary leaf of the subtree at `seq` — its leftmost
    /// leaf if `leftmost`, else its rightmost (upstream `leafSize`).
    fn boundary_leaf_len(&self, seq: u64, leftmost: bool) -> Result<usize, Error<S>> {
        let mut node = self.node(seq)?;
        while !node.is_leaf() {
            let c = if leftmost {
                node.children[0]
            } else {
                *node.children.last().expect("internal has children")
            };
            node = self.node(c)?;
        }
        Ok(node.entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
