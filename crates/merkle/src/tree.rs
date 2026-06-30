use std::collections::{BTreeMap, BTreeSet};

use crate::*;
use crate::{leaf_hash, parent_hash, tree_hash};

impl MerkleTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of appended blocks.
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Append a block; returns its block number. Rolls completed parents up.
    pub fn append(&mut self, data: &[u8]) -> u64 {
        let block = self.length;
        let mut cur = block * 2;
        self.nodes.insert(
            cur,
            Node {
                index: cur,
                hash: leaf_hash(data),
                size: data.len() as u64,
            },
        );

        // Climb while a *left* sibling is already present (we add left-to-right,
        // so only completed parents roll up).
        loop {
            let sib = flat::sibling(cur);
            if sib > cur {
                break; // right sibling not present yet
            }
            let left = match self.nodes.get(&sib) {
                Some(n) => *n,
                None => break,
            };
            let right = self.nodes[&cur];
            let p = flat::parent(cur);
            self.nodes.insert(
                p,
                Node {
                    index: p,
                    hash: parent_hash(&left, &right),
                    size: left.size + right.size,
                },
            );
            cur = p;
        }

        self.length += 1;
        block
    }

    /// Rewind the tree to its first `new_len` blocks, discarding every block —
    /// and every derived node — at index `>= new_len`. Returns `true` if the
    /// tree changed (`false`, a no-op, when `new_len >= len()`). The local
    /// "rewind to a prefix" primitive behind hypercore truncate.
    ///
    /// Because the first `new_len` blocks are untouched, the result is
    /// **node-for-node identical** to a fresh tree built from just those blocks:
    /// a node is kept exactly when its whole block range lies within
    /// `[0, new_len)`, so every retained node covers only unchanged blocks and
    /// has the hash the prefix would produce, and the kept set is precisely the
    /// completed-subtree set a fresh prefix builds. Hence
    /// [`root_hash`](MerkleTree::root_hash) equals the prefix's root hash (the
    /// head at a length is a pure function of the first `length` blocks — the
    /// same property fork detection rests on).
    pub fn truncate(&mut self, new_len: u64) -> bool {
        if new_len >= self.length {
            return false;
        }
        // Keep only nodes fully within the surviving prefix; what remains is the
        // fully-built tree of the first `new_len` blocks.
        self.nodes.retain(|&i, _| flat::block_range(i).1 <= new_len);
        self.length = new_len;
        true
    }

    /// Total byte size of every live block — the sum of the (authenticated) root
    /// subtree sizes. Shrinks under [`truncate`](MerkleTree::truncate); `0` when
    /// empty.
    pub fn byte_length(&self) -> u64 {
        self.roots().iter().map(|r| r.size).sum()
    }

    /// Serialize the tree (its `length` + all retained nodes) for persistence, so
    /// a hypercore can be reconstituted from storage — including a **sparse** core
    /// whose block bytes were cleared, whose tree nodes are the only thing that
    /// still authenticates the absent blocks. Layout: `[length u64][node_count u64]`
    /// then, per node, `[index u64][size u64][hash 32B]`, all little-endian. Nodes
    /// are emitted in flat-index order (the [`BTreeMap`] iteration order). Not
    /// disk-compatible with upstream (ADR-0001).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.nodes.len() * 48);
        out.extend_from_slice(&self.length.to_le_bytes());
        out.extend_from_slice(&(self.nodes.len() as u64).to_le_bytes());
        for node in self.nodes.values() {
            out.extend_from_slice(&node.index.to_le_bytes());
            out.extend_from_slice(&node.size.to_le_bytes());
            out.extend_from_slice(&node.hash);
        }
        out
    }

    /// Reconstruct a tree from [`serialize`](Self::serialize) output. Returns
    /// `None` on a malformed buffer (too short, a node count that overruns, or
    /// trailing bytes). Round-trips exactly: the restored tree has the same
    /// `root_hash`, proofs, and `byte_length` as the original.
    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        let length = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let count = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
        let mut nodes = BTreeMap::new();
        let mut off = 16usize;
        for _ in 0..count {
            if off + 48 > bytes.len() {
                return None;
            }
            let index = u64::from_le_bytes(bytes[off..off + 8].try_into().ok()?);
            let size = u64::from_le_bytes(bytes[off + 8..off + 16].try_into().ok()?);
            let mut hash: Hash = [0u8; 32];
            hash.copy_from_slice(&bytes[off + 16..off + 48]);
            nodes.insert(index, Node { index, hash, size });
            off += 48;
        }
        if off != bytes.len() {
            return None; // trailing garbage
        }
        Some(MerkleTree { nodes, length })
    }

    /// The root nodes the tree *would* have if truncated to its first `len`
    /// blocks, or `None` if any node that prefix needs is missing. `len == 0`
    /// yields the empty root set; `len > len()` yields `None`.
    ///
    /// This is the authenticated anchor a holder folds an [`UpgradeProof`] onto
    /// when following a reorg (the hypercore layer's `Replica::reorg`): because
    /// the head at a length is a pure function of the first `length` blocks,
    /// these roots are **identical** in any two trees that share the `[0, len)`
    /// prefix — so a verifier can re-anchor a length-extension proof on a proper
    /// prefix of its own trusted history, not just on its full current head.
    pub fn prefix_roots(&self, len: u64) -> Option<Vec<Node>> {
        if len > self.length {
            return None;
        }
        flat::full_roots(len * 2)
            .into_iter()
            .map(|i| self.nodes.get(&i).copied())
            .collect()
    }

    /// The root hash the tree *would* have if truncated to its first `len`
    /// blocks, or `None` if any node that prefix needs is missing (repair mode).
    /// `len == 0` yields the empty-tree hash. Used by
    /// [`lowest_common_ancestor`](MerkleTree::lowest_common_ancestor): because
    /// the head at a length is a pure function of the first `length` blocks, two
    /// trees agree on blocks `[0, len)` iff this hash is equal in both.
    pub fn prefix_root_hash(&self, len: u64) -> Option<Hash> {
        Some(tree_hash(&self.prefix_roots(len)?))
    }

    /// The **lowest common ancestor** of `self` and `other`: the length `a` of
    /// the longest block prefix `[0, a)` on which the two trees agree
    /// block-for-block (`0 <= a <= min(self.len(), other.len())`). This is the
    /// content-blind divergence finder behind upstream `merkle-tree.js`'s
    /// "lowest common ancestor" tests and the basis of a reorg.
    ///
    /// It compares only authenticated [`prefix_root_hash`](MerkleTree::prefix_root_hash)
    /// values, never payload bytes. Prefix agreement is **monotone** — agreeing
    /// on `[0, a)` implies agreeing on every shorter prefix — so the LCA is found
    /// by binary search over `0..=min(len)`. Both trees must be intact (no missing
    /// nodes), as a reorg's inputs are; a gap conservatively reads as disagreement.
    pub fn lowest_common_ancestor(&self, other: &MerkleTree) -> u64 {
        let agree = |a: u64| {
            a == 0
                || matches!(
                    (self.prefix_root_hash(a), other.prefix_root_hash(a)),
                    (Some(x), Some(y)) if x == y
                )
        };
        // Largest `a` in `0..=max` with `agree(a)`; `agree(0)` is always true.
        let max = self.length.min(other.length);
        let (mut lo, mut hi) = (0u64, max);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if agree(mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    /// Reorganize `self` to follow `other`: keep the shared
    /// [`lowest_common_ancestor`](MerkleTree::lowest_common_ancestor) prefix and
    /// adopt `other`'s divergent suffix, leaving `self` byte-identical to `other`.
    /// Returns the ancestor length kept. The local-tree mechanism behind upstream
    /// `merkle-tree.js`'s reorg (`MerkleTree.reorg` + `ReorgBatch`), and the
    /// content-following counterpart of [`truncate`](MerkleTree::truncate): where
    /// truncate is the author rewinding its own log, reorg is a holder following
    /// the author onto a rewritten history (readers follow the highest fork).
    ///
    /// The shared prefix is never re-derived — `self` truncates to the ancestor
    /// (the surviving nodes already equal `other`'s prefix, since the head at a
    /// length is a pure function of the first `length` blocks) and then takes on
    /// `other`'s nodes for the rest. Fork-agnostic: it reorganizes tree nodes, so
    /// authenticating *which* `other` to follow (the signed head + fork counter)
    /// belongs to the hypercore layer.
    pub fn reorg(&mut self, other: &MerkleTree) -> u64 {
        let ancestors = self.lowest_common_ancestor(other);
        self.truncate(ancestors); // keep the shared prefix (no-op if a == len)
        for (&idx, &node) in &other.nodes {
            self.nodes.insert(idx, node); // adopt the divergent suffix
        }
        self.length = other.length;
        ancestors
    }

    /// The current root nodes (one per complete power-of-two subtree).
    pub fn roots(&self) -> Vec<Node> {
        if self.length == 0 {
            return Vec::new();
        }
        flat::full_roots(self.length * 2)
            .into_iter()
            .map(|i| self.nodes[&i])
            .collect()
    }

    /// The signable/comparable hash over all roots.
    pub fn root_hash(&self) -> Hash {
        tree_hash(&self.roots())
    }

    /// An inclusion proof for `block`, or `None` if out of range.
    pub fn proof(&self, block: u64) -> Option<Proof> {
        if block >= self.length {
            return None;
        }
        let roots = self.roots();
        let root_set: BTreeSet<u64> = roots.iter().map(|n| n.index).collect();

        let mut cur = block * 2;
        let mut siblings = Vec::new();
        while !root_set.contains(&cur) {
            siblings.push(self.nodes[&flat::sibling(cur)]);
            cur = flat::parent(cur);
        }
        Some(Proof {
            block,
            leaf_size: self.nodes[&(block * 2)].size,
            siblings,
            roots,
        })
    }

    /// A proof that the contiguous block range `[start, end)` (end exclusive)
    /// belongs to this signed tree, or `None` if the range is empty or runs past
    /// the end. The multi-block generalization of [`MerkleTree::proof`].
    ///
    /// The proof carries only the **off-range boundary** sibling nodes needed to
    /// roll the range's leaves up to the roots — every on-range node is recomputed
    /// by the verifier from the block data, so a boundary node can never sit on a
    /// leaf's path to its root.
    pub fn range_proof(&self, start: u64, end: u64) -> Option<RangeProof> {
        if start >= end || end > self.length {
            return None;
        }
        let roots = self.roots();
        let root_set: BTreeSet<u64> = roots.iter().map(|n| n.index).collect();
        let leaf_sizes = (start..end).map(|b| self.nodes[&(b * 2)].size).collect();

        // Climb depth-by-depth from the in-range leaves. At each level, two
        // in-range/derived siblings pair into a parent for free; an off-range
        // sibling must be supplied from the full tree. Identical traversal to
        // `RangeProof::verify`, so generator and verifier agree on the boundary set.
        let mut active: BTreeSet<u64> = (start..end).map(|b| b * 2).collect();
        let mut boundary = Vec::new();
        while !active.iter().all(|i| root_set.contains(i)) {
            let d = active
                .iter()
                .filter(|i| !root_set.contains(i))
                .map(|&i| flat::depth(i))
                .min()
                .expect("a non-root node exists whenever the loop body runs");
            let level: Vec<u64> = active
                .iter()
                .copied()
                .filter(|&i| flat::depth(i) == d && !root_set.contains(&i))
                .collect();
            for cur in level {
                if !active.contains(&cur) {
                    continue; // already consumed as a sibling earlier this level
                }
                let sib = flat::sibling(cur);
                if active.contains(&sib) {
                    active.remove(&sib); // pair two in-range/derived nodes
                } else {
                    boundary.push(self.nodes[&sib]); // off-range sibling: supply it
                }
                active.remove(&cur);
                active.insert(flat::parent(cur));
            }
        }
        Some(RangeProof {
            start,
            end,
            leaf_sizes,
            nodes: boundary,
            roots,
        })
    }

    /// A length-extension (consistency) proof that the signed tree at length
    /// `new` is a genuine **append-only extension** of the tree at length `old`
    /// — i.e. the first `old` blocks were not rewritten (the cross-length
    /// anti-fork check). Returns `None` unless `1 <= old < new <= len`.
    ///
    /// The proof carries **no block data**: it supplies only the fully-new
    /// subtree nodes (covering blocks `>= old`) needed to fold the verifier's
    /// trusted *old roots* up into the *new roots*. A verifier holding the old
    /// prefix recomputes the new roots from its own (trusted) old roots plus
    /// these nodes and checks them against the new signed head's hash — so a
    /// rewrite of any old block makes the fold diverge and the proof fail.
    ///
    /// Composes with [`MerkleTree::range_proof`]: the upgrade proof confirms the
    /// extension is honest, then a range proof over `[old, new)` verifies the
    /// new blocks themselves against the same new head hash.
    pub fn upgrade_proof(&self, old: u64, new: u64) -> Option<UpgradeProof> {
        if old == 0 || old >= new || new > self.length {
            return None;
        }
        let old_root_set: BTreeSet<u64> = flat::full_roots(old * 2).into_iter().collect();
        // Walk down from each new root, stopping at old roots (the verifier has
        // them) and emitting the largest fully-new subtrees (it needs them).
        let mut out: BTreeMap<u64, Node> = BTreeMap::new();
        for r in flat::full_roots(new * 2) {
            self.collect_upgrade(r, old, &old_root_set, &mut out);
        }
        Some(UpgradeProof {
            old_len: old,
            new_len: new,
            nodes: out.into_values().collect(),
        })
    }

    fn collect_upgrade(
        &self,
        index: u64,
        old: u64,
        old_root_set: &BTreeSet<u64>,
        out: &mut BTreeMap<u64, Node>,
    ) {
        let (first, end) = flat::block_range(index);
        if end <= old {
            // Fully within the trusted old prefix. The recursion only ever
            // reaches an *old root* here (straddle-splitting stops exactly at the
            // old-root boundary), so the verifier already has it — supply nothing.
            if !old_root_set.contains(&index) {
                if let Some((l, r)) = flat::children(index) {
                    self.collect_upgrade(l, old, old_root_set, out);
                    self.collect_upgrade(r, old, old_root_set, out);
                }
            }
            return;
        }
        if first >= old {
            // Largest fully-new subtree: the verifier can't derive it, so supply it.
            out.insert(index, self.nodes[&index]);
            return;
        }
        // Straddles the old/new boundary (≥ 2 blocks ⇒ a parent): split.
        let (l, r) = flat::children(index).expect("a straddling node is not a leaf");
        self.collect_upgrade(l, old, old_root_set, out);
        self.collect_upgrade(r, old, old_root_set, out);
    }

    /// Map a byte offset to the block it falls in: returns `(block, offset)`
    /// where `offset` is the offset *within* that block. A byte offset that lands
    /// exactly on a block boundary belongs to the block it starts. For `bytes`
    /// past the end of the log, returns `(len(), bytes - total_byte_length)`
    /// (mirroring the upstream linear seek).
    ///
    /// O(log n): instead of summing every leaf, it descends the flat tree using
    /// each subtree's committed byte `size` to skip whole subtrees — yet it agrees
    /// with the linear scan for every offset (the upstream "basic tree seeks"
    /// property). This is byte-addressed random access; it never inspects payload
    /// contents, only the authenticated byte sizes.
    pub fn seek(&self, bytes: u64) -> (u64, u64) {
        let mut remaining = bytes;
        for root in self.roots() {
            if root.size > remaining {
                return self.seek_descend(root.index, remaining);
            }
            remaining -= root.size;
        }
        (self.length, remaining)
    }

    /// Descend the subtree rooted at `index` (whose committed `size > bytes`) to
    /// the leaf containing byte `bytes`, returning `(block, offset_within_block)`.
    fn seek_descend(&self, mut index: u64, mut bytes: u64) -> (u64, u64) {
        loop {
            match flat::children(index) {
                None => return (index / 2, bytes),
                Some((l, r)) => {
                    let left = self.nodes[&l];
                    if left.size > bytes {
                        index = l;
                    } else {
                        bytes -= left.size;
                        index = r;
                    }
                }
            }
        }
    }

    /// A proof that byte offset `bytes` falls in a particular block, verifiable
    /// against the signed root without any block data. Returns `None` if `bytes`
    /// is at or past the end of the log (there is no block to locate).
    ///
    /// Structurally it is the target block's inclusion path (siblings + roots)
    /// plus the leaf node itself; [`SeekProof::verify`] recomputes the
    /// left-cumulative byte size from the authenticated left-sibling and
    /// left-root sizes, so an untrusted holder cannot lie about where a byte
    /// offset lands. The byte-offset analogue of [`MerkleTree::proof`].
    pub fn seek_proof(&self, bytes: u64) -> Option<SeekProof> {
        let (block, _offset) = self.seek(bytes);
        if block >= self.length {
            return None; // past the end (also covers the empty tree)
        }
        let roots = self.roots();
        let root_set: BTreeSet<u64> = roots.iter().map(|n| n.index).collect();
        let mut cur = block * 2;
        let mut siblings = Vec::new();
        while !root_set.contains(&cur) {
            siblings.push(self.nodes[&flat::sibling(cur)]);
            cur = flat::parent(cur);
        }
        Some(SeekProof {
            bytes,
            leaf: self.nodes[&(block * 2)],
            siblings,
            roots,
        })
    }

    /// Whether the node at flat-tree `index` is currently stored.
    pub fn has_node(&self, index: u64) -> bool {
        self.nodes.contains_key(&index)
    }

    /// Remove a stored tree node — a corruption / partial-state injector, the
    /// clean-room analogue of upstream `deleteTreeNode`. Returns whether a node
    /// was present at `index`. After removal the tree may be in **repair mode**
    /// (see [`MerkleTree::is_intact`]); a missing node is restorable from a remote
    /// [`NodeProof`] via [`MerkleTree::recover_node`].
    pub fn remove_node(&mut self, index: u64) -> bool {
        self.nodes.remove(&index).is_some()
    }

    /// The flat-tree indices of every node a fully-built tree of this length
    /// *would* store but which is currently **missing** (deleted, or never
    /// fetched). Empty iff the tree is intact. A node at `index` is implied by the
    /// length exactly when its whole block range lies within `[0, len)`.
    pub fn missing_nodes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        if self.length == 0 {
            return out;
        }
        // Every implied node has index < 2*len (its block range end <= len), so
        // this bound is complete.
        for i in 0..(2 * self.length) {
            let (_, end) = flat::block_range(i);
            if end <= self.length && !self.nodes.contains_key(&i) {
                out.push(i);
            }
        }
        out
    }

    /// Whether every tree node implied by the current length is present. A tree
    /// that is **not** intact is in *repair mode*: it still reports its
    /// [`len`](MerkleTree::len) and serves the nodes it holds, but it refuses to
    /// be extended ([`try_append`](MerkleTree::try_append)) and may be unable to
    /// produce a root hash until the missing nodes are recovered.
    pub fn is_intact(&self) -> bool {
        self.missing_nodes().is_empty()
    }

    /// The root nodes, or `None` if any root is currently missing (repair mode).
    /// Unlike [`roots`](MerkleTree::roots) this never panics on a gap.
    pub fn try_roots(&self) -> Option<Vec<Node>> {
        if self.length == 0 {
            return Some(Vec::new());
        }
        flat::full_roots(self.length * 2)
            .into_iter()
            .map(|i| self.nodes.get(&i).copied())
            .collect()
    }

    /// The signable root hash, or `None` if a root is missing (repair mode). The
    /// graceful counterpart of [`root_hash`](MerkleTree::root_hash).
    pub fn try_root_hash(&self) -> Option<Hash> {
        Some(tree_hash(&self.try_roots()?))
    }

    /// Append a block, **refusing while the tree is in repair mode** (missing
    /// nodes). Extending a corrupt tree could silently bake an inconsistent root
    /// into the log, so recovery must complete first (ports
    /// `merkle-tree-recovery.js`'s "fail appends … when in repair mode"). Returns
    /// the new block number on success.
    pub fn try_append(&mut self, data: &[u8]) -> Result<u64, InRepairMode> {
        if !self.is_intact() {
            return Err(InRepairMode);
        }
        Ok(self.append(data))
    }

    /// An authenticated proof of the tree node at flat-tree `index` (leaf,
    /// interior node, or root) against the current signed root — the clean-room
    /// analogue of upstream `generateRemoteProofForTreeNode`. A peer that trusts
    /// the signed root but is missing this node can verify and re-store it with
    /// [`MerkleTree::recover_node`].
    ///
    /// Returns `None` if `index` is not a complete subtree within the tree, or if
    /// the node, any sibling on its path, or any root is missing locally (a
    /// corrupt *source* cannot prove the node it lost — it must receive a proof).
    pub fn node_proof(&self, index: u64) -> Option<NodeProof> {
        let (_, end) = flat::block_range(index);
        if self.length == 0 || end > self.length {
            return None;
        }
        let node = *self.nodes.get(&index)?;
        let roots = self.try_roots()?;
        let root_set: BTreeSet<u64> = roots.iter().map(|n| n.index).collect();
        let mut cur = index;
        let mut siblings = Vec::new();
        while !root_set.contains(&cur) {
            siblings.push(*self.nodes.get(&flat::sibling(cur))?);
            cur = flat::parent(cur);
        }
        Some(NodeProof { node, siblings, roots })
    }

    /// Recover a missing tree node from a remote [`NodeProof`], verified against
    /// the trusted `expected_root` (the signed head). On success stores the
    /// authenticated node and returns `true`; on any tampering / inconsistency it
    /// returns `false` and leaves the tree **unchanged** — the recovery is atomic
    /// (ports `merkle-tree-recovery.js`'s "atomically updates storage": a mangled
    /// proof fails and the node stays missing).
    pub fn recover_node(&mut self, proof: &NodeProof, expected_root: &Hash) -> bool {
        match proof.verify(expected_root) {
            Some(node) => {
                self.nodes.insert(node.index, node);
                true
            }
            None => false,
        }
    }
}
