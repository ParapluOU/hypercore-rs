use std::collections::BTreeSet;

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
            pending: None,
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
        // During a batch, blocks at/beyond the batch base are staged, not yet in the
        // core — read those back from the staging batch.
        if let Some(b) = self.pending.as_ref() {
            if seq >= b.base() {
                return self.core.batch_get(b, seq)?.ok_or(HcError::Corrupt);
            }
        }
        self.core.get(seq)?.ok_or(HcError::Corrupt)
    }

    pub(crate) fn root_seq(&self) -> Option<u64> {
        // The root is the latest block — a staged one mid-batch, else the core's.
        if let Some(b) = self.pending.as_ref() {
            let staged = b.staged() as u64;
            if staged > 0 {
                return Some(b.base() + staged - 1);
            }
        }
        self.core.len().checked_sub(1)
    }

    /// Append `node`, returning its block seq — **staged** into the in-progress batch
    /// if one is open (so the whole batch lands under one head), else appended
    /// directly. All copy-on-write writes go through here.
    fn append_node(&mut self, node: &Node) -> Result<u64, Error<S>> {
        if let Some(mut b) = self.pending.take() {
            self.core.stage(&mut b, node);
            let seq = b.base() + b.staged() as u64 - 1;
            self.pending = Some(b);
            Ok(seq)
        } else {
            self.core.append(node)
        }
    }

    /// The value for `key`, or `None`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
        match self.root_seq() {
            Some(r) => self.get_from(r, key),
            None => Ok(None),
        }
    }

    /// Descend the subtree rooted at `seq` looking up `key`.
    fn get_from(&self, mut seq: u64, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
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

    /// The root block seq for a hyperbee `version` (a value from [`version`](Self::version)
    /// or a prior op — i.e. an op boundary, where the latest block *is* the root).
    /// `version 0` ⇒ empty; clamped to the current version.
    fn root_at(&self, version: u64) -> Option<u64> {
        version.min(self.version()).checked_sub(1)
    }

    /// [`get`](Self::get) as of a past `version` (upstream `checkout(v).get`).
    pub fn get_at(&self, version: u64, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
        match self.root_at(version) {
            Some(r) => self.get_from(r, key),
            None => Ok(None),
        }
    }

    /// Put `key=value` **only if** its current value equals `expected` (`None` = the
    /// key is currently absent) — a value-equality compare-and-swap (upstream
    /// `put(k, v, { cas })`). Returns whether it applied.
    pub fn put_cas(
        &mut self,
        key: &[u8],
        value: &[u8],
        expected: Option<&[u8]>,
    ) -> Result<bool, Error<S>> {
        if self.get(key)?.as_deref() != expected {
            return Ok(false);
        }
        self.put(key, value)?;
        Ok(true)
    }

    /// Delete `key` **only if** its current value equals `expected` — a
    /// compare-and-swap delete. Returns whether it applied.
    pub fn del_cas(&mut self, key: &[u8], expected: Option<&[u8]>) -> Result<bool, Error<S>> {
        if self.get(key)?.as_deref() != expected {
            return Ok(false);
        }
        self.del(key)
    }

    /// Insert or overwrite `key`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error<S>> {
        let root = match self.root_seq() {
            None => {
                let leaf = Node {
                    entries: vec![(key.to_vec(), value.to_vec())],
                    children: vec![],
                };
                self.append_node(&leaf)?;
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
            self.append_node(&new_root)?;
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
                Ok(Ins::Down(self.append_node(&node)?))
            }
            Err(i) => {
                if node.is_leaf() {
                    node.entries.insert(i, (key.to_vec(), value.to_vec()));
                    self.finish(node)
                } else {
                    match self.insert(node.children[i], key, value)? {
                        Ins::Down(child) => {
                            node.children[i] = child;
                            Ok(Ins::Down(self.append_node(&node)?))
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
            return Ok(Ins::Down(self.append_node(&node)?));
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
        let left = self.append_node(&node)?;
        let right = self.append_node(&right)?;
        Ok(Ins::Split { left, median, right })
    }

    /// All entries in key order within `bounds` (honouring reverse + limit).
    pub fn range(&self, bounds: &Range) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error<S>> {
        self.range_from(self.root_seq(), bounds)
    }

    /// [`range`](Self::range) as of a past `version` (upstream `checkout(v)` range).
    pub fn range_at(
        &self,
        version: u64,
        bounds: &Range,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error<S>> {
        self.range_from(self.root_at(version), bounds)
    }

    /// The first entry within `bounds` (in iteration order), or `None` — upstream
    /// `peek`. Honours `reverse` (so reverse-peek yields the last entry).
    pub fn peek(&self, bounds: &Range) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error<S>> {
        Ok(self
            .range_from(self.root_seq(), &Range { limit: Some(1), ..bounds.clone() })?
            .into_iter()
            .next())
    }

    /// All entries under `root` (the current root if `None`), filtered and ordered by
    /// `bounds` (bytewise).
    fn range_from(
        &self,
        root: Option<u64>,
        bounds: &Range,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error<S>> {
        let mut out = Vec::new();
        if let Some(r) = root {
            self.collect(r, &mut out)?;
        }
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

    /// A read-only view of the tree as of a past `version` (upstream `checkout`). The
    /// `version` is a value from [`version`](Self::version) or a prior op.
    pub fn checkout(&self, version: u64) -> Checkout<'_, S> {
        let version = version.min(self.version());
        Checkout { bee: self, root: version.checked_sub(1), version }
    }

    /// A **sub-database** namespaced under `prefix` (upstream `sub`). Reads/writes
    /// through the returned [`Sub`] are transparently prefixed, so independent
    /// sub-databases coexist in one B-tree.
    pub fn sub(&mut self, prefix: &[u8]) -> Sub<'_, S> {
        Sub { bee: self, prefix: prefix.to_vec() }
    }

    /// The **diff** between two versions (upstream `createDiffStream`): every key whose
    /// value changed going from `old` to `new`, as `(key, old_value, new_value)` in key
    /// order. `old_value == None` ⇒ the key was added; `new_value == None` ⇒ it was
    /// deleted; both `Some` (and unequal) ⇒ changed. Unchanged keys are omitted.
    pub fn diff(
        &self,
        old: u64,
        new: u64,
    ) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>, Error<S>> {
        // Both sides come back sorted by key, so a linear merge finds the differences.
        let a = self.range_at(old, &Range::default())?;
        let b = self.range_at(new, &Range::default())?;
        let mut out = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < a.len() || j < b.len() {
            match (a.get(i), b.get(j)) {
                (Some((ka, va)), Some((kb, vb))) => {
                    if ka < kb {
                        out.push((ka.clone(), Some(va.clone()), None));
                        i += 1;
                    } else if ka > kb {
                        out.push((kb.clone(), None, Some(vb.clone())));
                        j += 1;
                    } else {
                        if va != vb {
                            out.push((ka.clone(), Some(va.clone()), Some(vb.clone())));
                        }
                        i += 1;
                        j += 1;
                    }
                }
                (Some((ka, va)), None) => {
                    out.push((ka.clone(), Some(va.clone()), None));
                    i += 1;
                }
                (None, Some((kb, vb))) => {
                    out.push((kb.clone(), None, Some(vb.clone())));
                    j += 1;
                }
                (None, None) => unreachable!(),
            }
        }
        Ok(out)
    }

    /// The **history** of operations (upstream `createHistoryStream`), oldest first, as
    /// `(key, value)` — `Some(v)` is a put, `None` a delete.
    ///
    /// Reconstructed without per-op records: in this copy-on-write layout each op's
    /// **root block is referenced by no other block** (roots are never pointed at), so
    /// the unreferenced blocks, in append order, are exactly the op boundaries — and
    /// diffing consecutive root-versions yields each op's single change. Caveats: a
    /// *redundant* put (same value) leaves no diff and is omitted; a no-op delete
    /// appended nothing and never appears. Eager (`O(versions × tree)`), not streaming.
    pub fn history(&self) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>, Error<S>> {
        let len = self.version();
        if len == 0 {
            return Ok(Vec::new());
        }
        // A block referenced as some node's child is an interior node, not an op root.
        let mut referenced = BTreeSet::new();
        for seq in 0..len {
            for &c in &self.node(seq)?.children {
                referenced.insert(c);
            }
        }
        // The op-boundary roots, in append (op) order.
        let roots: Vec<u64> = (0..len).filter(|s| !referenced.contains(s)).collect();
        let mut out = Vec::new();
        let mut prev = 0u64;
        for &r in &roots {
            let v = r + 1;
            for (key, _old, new) in self.diff(prev, v)? {
                out.push((key, new));
            }
            prev = v;
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
                    self.append_node(&child)?;
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
                    Ok(Del::Down { seq: self.append_node(&node)?, underflow: under })
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
        Ok(Del::Down { seq: self.append_node(&node)?, underflow: under })
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
                node.children[i - 1] = self.append_node(&left)?;
                node.children[i] = self.append_node(&child)?;
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
                node.children[i] = self.append_node(&child)?;
                node.children[i + 1] = self.append_node(&right)?;
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
        let merged = self.append_node(&left)?;
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
            Ok((min, self.append_node(&node)?, under))
        } else {
            let (min, new_child, child_under) = self.delete_min(node.children[0])?;
            node.children[0] = new_child;
            if child_under {
                self.rebalance(&mut node, 0)?;
            }
            let under = node.entries.len() < MIN_KEYS;
            Ok((min, self.append_node(&node)?, under))
        }
    }

    /// Remove and return the largest entry of the subtree at `seq` (COW). Returns
    /// `(entry, new_seq, underflow)`.
    fn delete_max(&mut self, seq: u64) -> Result<((Vec<u8>, Vec<u8>), u64, bool), Error<S>> {
        let mut node = self.node(seq)?;
        if node.is_leaf() {
            let max = node.entries.pop().expect("non-empty leaf");
            let under = node.entries.len() < MIN_KEYS;
            Ok((max, self.append_node(&node)?, under))
        } else {
            let last = node.children.len() - 1;
            let (max, new_child, child_under) = self.delete_max(node.children[last])?;
            node.children[last] = new_child;
            if child_under {
                self.rebalance(&mut node, last)?;
            }
            let under = node.entries.len() < MIN_KEYS;
            Ok((max, self.append_node(&node)?, under))
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

impl<S: Store> Checkout<'_, S> {
    /// The version this view is anchored at.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The value for `key` as of this version, or `None`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
        match self.root {
            Some(r) => self.bee.get_from(r, key),
            None => Ok(None),
        }
    }

    /// All entries within `bounds` as of this version (bytewise order).
    pub fn range(&self, bounds: &Range) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error<S>> {
        self.bee.range_from(self.root, bounds)
    }
}

impl<S: Store> Sub<'_, S> {
    /// Put `key=value` in this sub-database (stored as `prefix ++ key`).
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error<S>> {
        let k = [self.prefix.as_slice(), key].concat();
        self.bee.put(&k, value)
    }

    /// The value for `key` in this sub-database, or `None`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
        let k = [self.prefix.as_slice(), key].concat();
        self.bee.get(&k)
    }

    /// Delete `key` from this sub-database; returns whether it was present.
    pub fn del(&mut self, key: &[u8]) -> Result<bool, Error<S>> {
        let k = [self.prefix.as_slice(), key].concat();
        self.bee.del(&k)
    }

    /// Entries within `bounds`, scoped to this sub-database — `bounds` are relative to
    /// the sub-database's keys, and result keys have `prefix` stripped.
    pub fn range(&self, bounds: &Range) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error<S>> {
        let p = self.prefix.as_slice();
        let pre = |b: &[u8]| [p, b].concat();
        let lower_given = bounds.gt.is_some() || bounds.gte.is_some();
        let upper_given = bounds.lt.is_some() || bounds.lte.is_some();
        // The namespace is `[prefix, prefix_successor)`; the user's bounds (prefixed)
        // tighten it. With no lower/upper bound, default to the namespace edge.
        let full = Range {
            gt: bounds.gt.as_deref().map(pre),
            gte: bounds
                .gte
                .as_deref()
                .map(pre)
                .or_else(|| if lower_given { None } else { Some(p.to_vec()) }),
            lt: bounds
                .lt
                .as_deref()
                .map(pre)
                .or_else(|| if upper_given { None } else { prefix_successor(p) }),
            lte: bounds.lte.as_deref().map(pre),
            reverse: bounds.reverse,
            limit: bounds.limit,
        };
        let n = self.prefix.len();
        Ok(self
            .bee
            .range(&full)?
            .into_iter()
            .map(|(k, v)| (k[n..].to_vec(), v))
            .collect())
    }

    /// The parent tree's current version.
    pub fn version(&self) -> u64 {
        self.bee.version()
    }
}

/// The smallest key strictly greater than every key beginning with `prefix` — i.e.
/// the exclusive upper bound of the prefix's namespace. `None` if `prefix` is empty
/// or all `0xff` (the namespace runs to the end of keyspace).
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.last_mut() {
        if *last < 0xff {
            *last += 1;
            return Some(p);
        }
        p.pop();
    }
    None
}

impl<S: Store> Hyperbee<S> {
    /// Begin an **atomic batch** (upstream `batch`): `put`/`del` on the returned
    /// [`BeeBatch`] stage their copy-on-write writes; [`commit`](BeeBatch::commit)
    /// lands them all under one signed head. Dropping without committing discards
    /// them (the tree is unchanged).
    pub fn batch(&mut self) -> BeeBatch<'_, S> {
        self.pending = Some(self.core.batch());
        BeeBatch { bee: self }
    }
}

impl<S: Store> BeeBatch<'_, S> {
    /// Stage a put into the batch.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error<S>> {
        self.bee.put(key, value)
    }

    /// Stage a delete; returns whether the key was present (in the batched view).
    pub fn del(&mut self, key: &[u8]) -> Result<bool, Error<S>> {
        self.bee.del(key)
    }

    /// The value for `key`, reflecting the batch's staged writes so far.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S>> {
        self.bee.get(key)
    }

    /// Apply all staged writes atomically, under a single head.
    pub fn commit(self) -> Result<(), Error<S>> {
        if let Some(b) = self.bee.pending.take() {
            self.bee.core.commit(b)?;
        }
        Ok(())
    }
}

impl<S> Drop for BeeBatch<'_, S> {
    fn drop(&mut self) {
        // An uncommitted batch discards its staged writes — nothing reached the core.
        self.bee.pending = None;
    }
}
