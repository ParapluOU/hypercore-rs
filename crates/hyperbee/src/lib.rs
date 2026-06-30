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
}
