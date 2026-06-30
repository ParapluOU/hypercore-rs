use hypercore::{Error as HcError, Hypercore};
use identity::SecretKey;
use storage::Store;

use crate::*;
use crate::{Del, Ins, Node, NodeCodec, MAX_CHILDREN, MIN_KEYS};

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

    pub(crate) fn node(&self, seq: u64) -> Result<Node, Error<S>> {
        self.core.get(seq)?.ok_or(HcError::Corrupt)
    }

    pub(crate) fn root_seq(&self) -> Option<u64> {
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
