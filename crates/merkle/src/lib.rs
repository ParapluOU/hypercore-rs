//! `merkle` — flat-tree Merkle over BLAKE3.
//!
//! A content-blind, append-only Merkle tree: blocks of bytes are appended, each
//! producing a leaf; parents and roots are derived via the [flat-tree] index
//! scheme. Hashing is **domain-separated and length-bound** (distinct prefix
//! bytes for leaf / parent / tree, and each node binds its byte size) to prevent
//! second-preimage and length-extension confusion.
//!
//! This is clean-room: we do **not** match upstream Hypercore's byte layout
//! (which uses BLAKE2b). We keep the *structure* (flat-tree, multi-root,
//! inclusion proofs) and choose our own sound hashing.
//!
//! [flat-tree]: https://github.com/mafintosh/flat-tree

use std::collections::{BTreeMap, BTreeSet};

/// Domain-separation prefixes.
const LEAF: u8 = 0x00;
const PARENT: u8 = 0x01;
const TREE: u8 = 0x02;

/// A 32-byte BLAKE3 digest.
pub type Hash = [u8; 32];

/// A tree node: its flat-tree `index`, its `hash`, and the total byte `size` of
/// the block range it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub index: u64,
    pub hash: Hash,
    pub size: u64,
}

/// Flat-tree index arithmetic. Leaves live at even indices (block `k` → `2k`);
/// parents at odd indices. See the mafintosh `flat-tree` algorithm.
pub mod flat {
    /// Depth of a node (leaves = 0).
    pub fn depth(i: u64) -> u32 {
        (i + 1).trailing_zeros()
    }

    /// Horizontal offset of a node within its depth.
    pub fn offset(i: u64) -> u64 {
        if i & 1 == 0 {
            i / 2
        } else {
            let d = depth(i);
            (((i + 1) >> d) - 1) / 2
        }
    }

    /// Node index from a (depth, offset) pair.
    pub fn index(depth: u32, offset: u64) -> u64 {
        (1 + 2 * offset) * (1u64 << depth) - 1
    }

    /// Parent of node `i`.
    pub fn parent(i: u64) -> u64 {
        let d = depth(i);
        index(d + 1, offset(i) >> 1)
    }

    /// Sibling of node `i`.
    pub fn sibling(i: u64) -> u64 {
        let d = depth(i);
        index(d, offset(i) ^ 1)
    }

    /// The (left, right) children of a parent node, or `None` for a leaf.
    pub fn children(i: u64) -> Option<(u64, u64)> {
        if i & 1 == 0 {
            return None;
        }
        let d = depth(i);
        let off = offset(i) * 2;
        Some((index(d - 1, off), index(d - 1, off + 1)))
    }

    /// The half-open block range `[first, end)` that node `i` covers. A leaf
    /// (`i == 2k`) covers exactly block `k`; a node at depth `d` covers `2^d`
    /// contiguous blocks.
    pub fn block_range(i: u64) -> (u64, u64) {
        let d = depth(i);
        let count = 1u64 << d;
        let first = offset(i) * count;
        (first, first + count)
    }

    /// Root indices covering a fully-rooted tree of `idx` (= `2 * block_count`)
    /// tree-index units. Returns one index per complete power-of-two subtree.
    pub fn full_roots(idx: u64) -> Vec<u64> {
        assert!(idx & 1 == 0, "full_roots requires an even index");
        let mut result = Vec::new();
        let mut index = idx / 2;
        let mut offset = 0u64;
        let mut factor = 1u64;
        while index > 0 {
            while factor * 2 <= index {
                factor *= 2;
            }
            result.push(offset * 2 + factor - 1);
            offset += factor;
            index -= factor;
            factor = 1;
        }
        result
    }
}

fn leaf_hash(data: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF]);
    h.update(&(data.len() as u64).to_le_bytes());
    h.update(data);
    *h.finalize().as_bytes()
}

fn parent_hash(left: &Node, right: &Node) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[PARENT]);
    h.update(&left.hash);
    h.update(&left.size.to_le_bytes());
    h.update(&right.hash);
    h.update(&right.size.to_le_bytes());
    *h.finalize().as_bytes()
}

fn tree_hash(roots: &[Node]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[TREE]);
    for r in roots {
        h.update(&r.hash);
        h.update(&r.index.to_le_bytes());
        h.update(&r.size.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// An append-only flat-tree Merkle tree.
#[derive(Clone, Debug, Default)]
pub struct MerkleTree {
    nodes: BTreeMap<u64, Node>,
    length: u64,
}

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

/// Returned when a mutation is refused because the tree is in repair mode
/// (missing tree nodes). See [`MerkleTree::try_append`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InRepairMode;

impl std::fmt::Display for InRepairMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("merkle tree is in repair mode (missing tree nodes)")
    }
}

impl std::error::Error for InRepairMode {}

/// An authenticated proof of a single tree node (leaf, interior, or root)
/// against the signed root: the node itself, the sibling nodes from it up to its
/// containing root, and every root. Used to **recover** a missing node from an
/// untrusted peer — the climb to the trusted root authenticates the node's hash
/// and size. The arbitrary-node generalization of [`Proof`] (which always starts
/// from a leaf recomputed from block data).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeProof {
    /// The node being proven (its `index` may be any complete subtree).
    pub node: Node,
    /// Sibling nodes, bottom-up, from `node` to its containing root.
    pub siblings: Vec<Node>,
    /// All roots of the tree at proof time.
    pub roots: Vec<Node>,
}

impl NodeProof {
    /// Verify against the trusted `expected_root`; on success return the
    /// authenticated node (safe to store), else `None`.
    ///
    /// Soundness: the proven node climbs to its containing root via
    /// [`parent_hash`] (which binds each child's hash **and** size), the
    /// recomputed root is substituted into the roots, and [`tree_hash`] must equal
    /// `expected_root`. Tampering with the node, any sibling, or any root makes
    /// the recomputed hash diverge (collision-resistance), so a peer cannot foist
    /// a wrong node — the same assumption every other proof here rests on. A
    /// dropped sibling leaves the climb short of any root, so the substitution
    /// fails and this returns `None`.
    pub fn verify(&self, expected_root: &Hash) -> Option<Node> {
        let mut node = self.node;
        for sib in &self.siblings {
            if sib.index != flat::sibling(node.index) {
                return None; // not the path node's actual sibling
            }
            let p = flat::parent(node.index);
            let (left, right) = if sib.index < node.index {
                (*sib, node)
            } else {
                (node, *sib)
            };
            node = Node {
                index: p,
                hash: parent_hash(&left, &right),
                size: left.size + right.size,
            };
        }
        let mut roots = self.roots.clone();
        match roots.iter_mut().find(|r| r.index == node.index) {
            Some(slot) => *slot = node,
            None => return None,
        }
        if &tree_hash(&roots) != expected_root {
            return None;
        }
        Some(self.node)
    }
}

/// A self-contained inclusion proof: the sibling hashes from a block's leaf up
/// to its containing root, plus every root of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    pub block: u64,
    pub leaf_size: u64,
    /// Sibling nodes, bottom-up, to the containing root.
    pub siblings: Vec<Node>,
    /// All roots of the tree at proof time.
    pub roots: Vec<Node>,
}

impl Proof {
    /// Verify this proof for `data` against an expected tree `root` hash.
    ///
    /// Tampering with the block, any sibling, or any root makes the recomputed
    /// tree hash diverge from `expected_root`, so this returns `false`.
    pub fn verify(&self, data: &[u8], expected_root: &Hash) -> bool {
        if data.len() as u64 != self.leaf_size {
            return false;
        }
        // Recompute the leaf, then climb using the supplied siblings.
        let mut node = Node {
            index: self.block * 2,
            hash: leaf_hash(data),
            size: data.len() as u64,
        };
        for sib in &self.siblings {
            if sib.index != flat::sibling(node.index) {
                return false; // not the path node's actual sibling (defense-in-depth)
            }
            let p = flat::parent(node.index);
            let (left, right) = if sib.index < node.index {
                (*sib, node)
            } else {
                (node, *sib)
            };
            node = Node {
                index: p,
                hash: parent_hash(&left, &right),
                size: left.size + right.size,
            };
        }
        // The recomputed node must be one of the roots. Substitute it (do not
        // trust the proof's copy), then the whole-tree hash must match.
        let mut roots = self.roots.clone();
        match roots.iter_mut().find(|r| r.index == node.index) {
            Some(slot) => *slot = node,
            None => return false,
        }
        &tree_hash(&roots) == expected_root
    }
}

/// A self-contained proof that a byte offset falls in a particular block: the
/// target leaf node plus its inclusion path (siblings to the containing root)
/// and every root. Carries **no block data** — a seek locates a block, it does
/// not reveal its contents. The byte-offset analogue of [`Proof`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeekProof {
    /// The byte offset this proof locates.
    pub bytes: u64,
    /// The target leaf node (its `index` is `block * 2`).
    pub leaf: Node,
    /// Sibling nodes, bottom-up, to the containing root.
    pub siblings: Vec<Node>,
    /// All roots of the tree at proof time.
    pub roots: Vec<Node>,
}

impl SeekProof {
    /// Verify against the trusted `expected_root` and, if valid, return the
    /// authenticated `(block, offset_within_block)` that byte `bytes` lands in.
    ///
    /// Soundness: the leaf climbs to its containing root via [`parent_hash`]
    /// (which binds each child's hash **and** byte size), and the recomputed root
    /// is substituted into the roots before checking [`tree_hash`] against
    /// `expected_root` — so every size used below is authenticated. The
    /// left-cumulative byte size of the target block is the sum of the sizes of
    /// the **left** siblings met while climbing (each the full subtree preceding
    /// the target) plus the sizes of the roots to the left of the containing
    /// root. The byte offset is in this block iff
    /// `cumulative <= bytes < cumulative + leaf.size`; since the blocks' byte
    /// intervals are disjoint and contiguous, exactly one block satisfies it, so
    /// a prover cannot pass off a different block (its authenticated sizes will
    /// not bracket `bytes`). Any tampering breaks the climb or the bracket and
    /// yields `None`.
    pub fn verify(&self, expected_root: &Hash) -> Option<(u64, u64)> {
        // A seek target must be a real block leaf (even flat-tree index). Without
        // this, an interior/root node (odd index) authenticates against the root and
        // its aggregate subtree size brackets any offset, so a prover could pass it
        // off and get a bogus `index / 2` block accepted. Upstream's `ByteSeeker`
        // descends to an even index and guards `(index & 1) === 0`; we restore it.
        if self.leaf.index & 1 != 0 {
            return None;
        }
        // Climb the leaf to its containing root, accumulating the byte sizes of
        // every left sibling (each precedes the target block in its entirety).
        let mut node = self.leaf;
        let mut left_sum: u64 = 0;
        for sib in &self.siblings {
            if sib.index != flat::sibling(node.index) {
                return None; // not the path node's actual sibling (defense-in-depth)
            }
            let p = flat::parent(node.index);
            let (left, right) = if sib.index < node.index {
                left_sum += sib.size;
                (*sib, node)
            } else {
                (node, *sib)
            };
            node = Node {
                index: p,
                hash: parent_hash(&left, &right),
                size: left.size + right.size,
            };
        }

        // Add the sizes of the roots strictly to the left of the containing root.
        // (Root indices increase left-to-right, so a smaller index is to the left.)
        let mut root_left_sum: u64 = 0;
        let mut found = false;
        for r in &self.roots {
            if r.index == node.index {
                found = true;
            } else if r.index < node.index {
                root_left_sum += r.size;
            }
        }
        if !found {
            return None; // the climb did not end on a known root (e.g. dropped sibling)
        }
        let cumulative = root_left_sum + left_sum;

        // Substitute the recomputed root (do not trust the proof's copy); the
        // whole-tree hash must match, which authenticates every size used above.
        let mut roots = self.roots.clone();
        match roots.iter_mut().find(|r| r.index == node.index) {
            Some(slot) => *slot = node,
            None => return None,
        }
        if &tree_hash(&roots) != expected_root {
            return None;
        }

        // The byte offset must land within this (authenticated) block.
        if self.bytes < cumulative {
            return None;
        }
        let offset = self.bytes - cumulative;
        if offset >= self.leaf.size {
            return None;
        }
        Some((self.leaf.index / 2, offset))
    }
}

/// A self-contained proof for a contiguous block range `[start, end)`: the
/// off-range boundary nodes needed to roll the range's leaves up to the roots,
/// plus every root of the tree. The multi-block generalization of [`Proof`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeProof {
    pub start: u64,
    pub end: u64,
    /// Byte size of each block in `[start, end)`, in order.
    pub leaf_sizes: Vec<u64>,
    /// Off-range boundary sibling nodes (any depth), in climb order.
    pub nodes: Vec<Node>,
    /// All roots of the tree at proof time.
    pub roots: Vec<Node>,
}

impl RangeProof {
    /// Verify this proof for `blocks` (the data for `[start, end)`, in order)
    /// against an expected tree `root` hash.
    ///
    /// Soundness: every on-range node is recomputed from `blocks` and only ever
    /// used as the *path* node; the proof's `nodes` are consulted strictly as
    /// off-path siblings (by index, preferring a recomputed node when present), so
    /// a forged boundary node cannot impersonate a leaf's ancestor. Each recomputed
    /// leaf is force-climbed to a genuine root index (a missing sibling is a
    /// rejection), then the recomputed roots are substituted and the whole-tree
    /// hash must equal `expected_root`. Tampering with any block, boundary node, or
    /// root makes the recomputed tree hash diverge, so this returns `false`.
    pub fn verify<B: AsRef<[u8]>>(&self, blocks: &[B], expected_root: &Hash) -> bool {
        if self.start >= self.end {
            return false;
        }
        let n = (self.end - self.start) as usize;
        if blocks.len() != n || self.leaf_sizes.len() != n {
            return false;
        }

        // Recompute every in-range leaf from the supplied data; these are the only
        // nodes we trust as path nodes.
        let mut have: BTreeMap<u64, Node> = BTreeMap::new();
        for (k, b) in blocks.iter().enumerate() {
            let data = b.as_ref();
            if data.len() as u64 != self.leaf_sizes[k] {
                return false;
            }
            let idx = (self.start + k as u64) * 2;
            have.insert(
                idx,
                Node {
                    index: idx,
                    hash: leaf_hash(data),
                    size: data.len() as u64,
                },
            );
        }

        let sibling_table: BTreeMap<u64, Node> =
            self.nodes.iter().map(|n| (n.index, *n)).collect();
        let root_set: BTreeSet<u64> = self.roots.iter().map(|n| n.index).collect();

        // Same depth-by-depth climb as `MerkleTree::range_proof`: a path node is
        // always a recomputed/derived node; siblings come from `have` first, else
        // from the (untrusted) boundary table.
        let mut active: BTreeSet<u64> = (self.start..self.end).map(|b| b * 2).collect();
        let mut guard = 0u32;
        while !active.iter().all(|i| root_set.contains(i)) {
            guard += 1;
            if guard > 256 {
                return false; // malformed proof: not converging to the roots
            }
            let d = match active
                .iter()
                .filter(|i| !root_set.contains(i))
                .map(|&i| flat::depth(i))
                .min()
            {
                Some(d) => d,
                None => break,
            };
            let level: Vec<u64> = active
                .iter()
                .copied()
                .filter(|&i| flat::depth(i) == d && !root_set.contains(&i))
                .collect();
            for cur in level {
                if !active.contains(&cur) {
                    continue;
                }
                let cur_node = match have.get(&cur) {
                    Some(n) => *n,
                    None => return false,
                };
                let sib_idx = flat::sibling(cur);
                let sib_node = match have
                    .get(&sib_idx)
                    .copied()
                    .or_else(|| sibling_table.get(&sib_idx).copied())
                {
                    Some(n) => n,
                    None => return false, // insufficient proof
                };
                let p = flat::parent(cur);
                let (left, right) = if sib_idx < cur {
                    (sib_node, cur_node)
                } else {
                    (cur_node, sib_node)
                };
                have.insert(
                    p,
                    Node {
                        index: p,
                        hash: parent_hash(&left, &right),
                        size: left.size + right.size,
                    },
                );
                active.remove(&cur);
                active.remove(&sib_idx); // no-op if the sibling was an off-range boundary node
                active.insert(p);
            }
        }

        // Every surviving frontier node is now a root index, recomputed from the
        // block data. Substitute them in; untouched roots stay as given (all bound
        // by `tree_hash` against `expected_root`).
        let mut roots = self.roots.clone();
        for idx in active.iter() {
            let recomputed = match have.get(idx) {
                Some(n) => *n,
                None => return false,
            };
            match roots.iter_mut().find(|r| r.index == *idx) {
                Some(slot) => *slot = recomputed,
                None => return false,
            }
        }
        &tree_hash(&roots) == expected_root
    }
}

/// A length-extension (consistency) proof: the fully-new subtree nodes needed to
/// fold a verifier's trusted *old roots* (at `old_len`) up into the *new roots*
/// (at `new_len`), proving the new tree is an append-only extension of the old.
/// Carries no block data. The multi-length analogue of fork detection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeProof {
    pub old_len: u64,
    pub new_len: u64,
    /// Fully-new subtree nodes (every node covers only blocks `>= old_len`),
    /// sorted by index. Combined with the trusted old roots they climb to the
    /// new roots.
    pub nodes: Vec<Node>,
}

impl UpgradeProof {
    /// Verify that this proof extends the trusted `old_roots` (the verifier's own
    /// roots at `old_len`) to a tree whose root hash is `new_root_hash` (the new
    /// signed head). Returns `false` on any tampering or inconsistency.
    ///
    /// Soundness / anti-fork: the verifier seeds its frontier with **its own**
    /// trusted old roots and folds in the proof's nodes, which are accepted only
    /// if **fully new** (every covered block `>= old_len`) — so prover-supplied
    /// data can never sit on, or stand in for, an old block. The new roots are
    /// therefore recomputed from the trusted old prefix; if the prover rewrote any
    /// old block, the recomputed new roots diverge from `new_root_hash` and this
    /// returns `false`.
    pub fn verify(&self, old_roots: &[Node], new_root_hash: &Hash) -> bool {
        if self.old_len == 0 || self.old_len >= self.new_len {
            return false;
        }
        // The caller's old roots must match the shape claimed by the proof, so a
        // mismatched old state can't be silently accepted.
        let expected_old = flat::full_roots(self.old_len * 2);
        if old_roots.len() != expected_old.len()
            || old_roots.iter().zip(&expected_old).any(|(r, &i)| r.index != i)
        {
            return false;
        }

        // Seed the frontier with the trusted old roots, then fold in the proof's
        // nodes — each accepted only if it is a *fully-new* subtree and lies
        // within `[0, new)`. (Rejecting straddling/old nodes is what forces the
        // new roots to be rebuilt from the trusted old prefix.)
        let mut known: BTreeMap<u64, Node> = old_roots.iter().map(|n| (n.index, *n)).collect();
        for n in &self.nodes {
            let (first, end) = flat::block_range(n.index);
            if first < self.old_len || end > self.new_len {
                return false; // not a fully-new in-range subtree
            }
            if known.insert(n.index, *n).is_some() {
                return false; // duplicate / collides with an old root
            }
        }

        // Climb: combine any two known siblings into their parent until stable.
        let mut changed = true;
        let mut guard = 0u32;
        while changed {
            changed = false;
            guard += 1;
            if guard > 4096 {
                return false; // malformed: not converging
            }
            for c in known.keys().copied().collect::<Vec<_>>() {
                let sib = flat::sibling(c);
                let p = flat::parent(c);
                if !known.contains_key(&sib) || known.contains_key(&p) {
                    continue;
                }
                let (cn, sn) = (known[&c], known[&sib]);
                let (left, right) = if sib < c { (sn, cn) } else { (cn, sn) };
                known.insert(
                    p,
                    Node {
                        index: p,
                        hash: parent_hash(&left, &right),
                        size: left.size + right.size,
                    },
                );
                changed = true;
            }
        }

        // Every new root must now be derived; their tree hash must match the head.
        let mut new_roots = Vec::new();
        for idx in flat::full_roots(self.new_len * 2) {
            match known.get(&idx) {
                Some(n) => new_roots.push(*n),
                None => return false,
            }
        }
        &tree_hash(&new_roots) == new_root_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(n: usize) -> (MerkleTree, Vec<Vec<u8>>) {
        let mut t = MerkleTree::new();
        let mut blocks = Vec::new();
        for i in 0..n {
            let b = format!("block-{i}").into_bytes();
            t.append(&b);
            blocks.push(b);
        }
        (t, blocks)
    }

    fn tree_from(blocks: &[Vec<u8>]) -> MerkleTree {
        let mut t = MerkleTree::new();
        for b in blocks {
            t.append(b);
        }
        t
    }

    // flat-tree shape — ports "get roots" structure from merkle-tree.js.
    #[test]
    fn roots_shape() {
        assert_eq!(flat::full_roots(2), vec![0]); // 1 block  -> 1 root
        assert_eq!(flat::full_roots(8), vec![3]); // 4 blocks -> 1 root
        assert_eq!(flat::full_roots(10), vec![3, 8]); // 5 blocks -> 2 roots
        assert_eq!(flat::full_roots(14), vec![3, 9, 12]); // 7 blocks -> 3 roots
    }

    // Every block in a range of tree sizes proves & verifies — ports
    // "proof only block" + "verify proof".
    #[test]
    fn proof_roundtrip_all_sizes() {
        for n in 1..=33usize {
            let (t, blocks) = tree(n);
            let root = t.root_hash();
            for b in 0..n as u64 {
                let proof = t.proof(b).expect("proof exists");
                assert!(
                    proof.verify(&blocks[b as usize], &root),
                    "honest proof must verify (n={n}, block={b})"
                );
            }
            assert!(t.proof(n as u64).is_none(), "out-of-range proof is None");
        }
    }

    // A non-edge block's proof carries sibling + sub-root hashes (not empty).
    #[test]
    fn proof_carries_siblings() {
        let (t, _) = tree(8);
        let proof = t.proof(3).unwrap();
        assert!(!proof.siblings.is_empty(), "interior block needs siblings");
    }

    // Determinism — ports "tree hash determinism".
    #[test]
    fn determinism() {
        let (a, _) = tree(6);
        let (b, _) = tree(6);
        assert_eq!(a.root_hash(), b.root_hash(), "same blocks => same root");

        let mut c = MerkleTree::new();
        for i in 0..6 {
            c.append(format!("other-{i}").as_bytes());
        }
        assert_ne!(a.root_hash(), c.root_hash(), "different blocks => different root");
    }

    // Tamper-rejection — the property the DoD requires.
    #[test]
    fn rejects_tampering() {
        let (t, blocks) = tree(7);
        let root = t.root_hash();
        let proof = t.proof(4).unwrap();

        // honest baseline
        assert!(proof.verify(&blocks[4], &root));

        // wrong data
        assert!(!proof.verify(b"forged-block", &root));
        // right length, wrong bytes
        let mut same_len = blocks[4].clone();
        same_len[0] ^= 0xff;
        assert!(!proof.verify(&same_len, &root));

        // tampered sibling
        let mut bad = proof.clone();
        bad.siblings[0].hash[0] ^= 0xff;
        assert!(!bad.verify(&blocks[4], &root));

        // tampered root entry
        let mut bad_root = proof.clone();
        bad_root.roots[0].hash[0] ^= 0xff;
        assert!(!bad_root.verify(&blocks[4], &root));

        // honest proof, wrong expected root
        let mut wrong = root;
        wrong[0] ^= 0xff;
        assert!(!proof.verify(&blocks[4], &wrong));
    }

    // Every contiguous sub-range of a range of tree sizes proves & verifies —
    // the multi-block generalization of `proof_roundtrip_all_sizes`.
    #[test]
    fn range_proof_roundtrip_all_sizes() {
        for n in 1..=20usize {
            let (t, blocks) = tree(n);
            let root = t.root_hash();
            for start in 0..n as u64 {
                for end in (start + 1)..=n as u64 {
                    let rp = t
                        .range_proof(start, end)
                        .expect("in-range proof exists");
                    let span: &[Vec<u8>] = &blocks[start as usize..end as usize];
                    assert!(
                        rp.verify(span, &root),
                        "honest range proof must verify (n={n}, [{start},{end}))"
                    );
                }
            }
        }
    }

    // The whole-tree range recomputes every root: the strongest check.
    #[test]
    fn range_proof_full_tree() {
        for n in 1..=17usize {
            let (t, blocks) = tree(n);
            let rp = t.range_proof(0, n as u64).unwrap();
            assert!(rp.verify(&blocks, &t.root_hash()));
            // A full-tree range needs no off-range boundary nodes.
            assert!(rp.nodes.is_empty(), "full-tree range needs no boundary nodes (n={n})");
        }
    }

    // A range spanning multiple roots carries boundary nodes and still verifies.
    #[test]
    fn range_proof_spans_multiple_roots() {
        let (t, blocks) = tree(7); // 3 roots: indices 3, 9, 12
        let root = t.root_hash();
        let rp = t.range_proof(2, 5).unwrap(); // blocks 2,3,4 -> leaves 4,6,8
        assert!(!rp.nodes.is_empty(), "interior range needs boundary nodes");
        assert!(rp.verify(&blocks[2..5], &root));
    }

    // A single-block range carries exactly the same boundary set as the
    // single-block inclusion proof's siblings.
    #[test]
    fn range_proof_single_block_matches_inclusion() {
        let (t, blocks) = tree(13);
        let root = t.root_hash();
        for b in 0..13u64 {
            let rp = t.range_proof(b, b + 1).unwrap();
            let p = t.proof(b).unwrap();
            let mut rp_idx: Vec<u64> = rp.nodes.iter().map(|n| n.index).collect();
            let mut p_idx: Vec<u64> = p.siblings.iter().map(|n| n.index).collect();
            rp_idx.sort_unstable();
            p_idx.sort_unstable();
            assert_eq!(rp_idx, p_idx, "single-block range == inclusion siblings (b={b})");
            assert!(rp.verify(std::slice::from_ref(&blocks[b as usize]), &root));
        }
    }

    // Out-of-range / empty ranges produce no proof.
    #[test]
    fn range_proof_out_of_range() {
        let (t, _) = tree(8);
        assert!(t.range_proof(0, 9).is_none(), "end past length");
        assert!(t.range_proof(8, 9).is_none(), "start past length");
        assert!(t.range_proof(3, 3).is_none(), "empty range");
        assert!(t.range_proof(5, 2).is_none(), "inverted range");
    }

    // Tamper-rejection across the whole span — the DoD property for range proofs.
    #[test]
    fn range_proof_rejects_tampering() {
        let (t, blocks) = tree(11);
        let root = t.root_hash();
        let rp = t.range_proof(3, 8).unwrap(); // blocks 3..8
        let span: Vec<Vec<u8>> = blocks[3..8].to_vec();

        // honest baseline
        assert!(rp.verify(&span, &root));

        // a single mutated block anywhere in the span is caught
        for i in 0..span.len() {
            let mut bad = span.clone();
            bad[i][0] ^= 0xff;
            assert!(!rp.verify(&bad, &root), "mutated block {i} must reject");
        }

        // reordering two blocks (positions matter — leaves are positional)
        let mut swapped = span.clone();
        swapped.swap(0, 1);
        assert!(!rp.verify(&swapped, &root), "reordered span must reject");

        // wrong block count
        assert!(!rp.verify(&span[..span.len() - 1], &root), "short span rejects");

        // tampered boundary node
        let mut bad_node = rp.clone();
        bad_node.nodes[0].hash[0] ^= 0xff;
        assert!(!bad_node.verify(&span, &root), "tampered boundary node rejects");

        // tampered *untouched* root entry. Range [3,8) lives under root 7 only, so
        // the other roots (17, 20) are passed through unchanged and bound by
        // tree_hash — tampering one must reject. (A *touched* root is recomputed
        // from the block data and would be substituted over, by design.)
        let mut bad_root = rp.clone();
        let last = bad_root.roots.len() - 1;
        assert!(last > 0, "range must leave some roots untouched");
        bad_root.roots[last].hash[0] ^= 0xff;
        assert!(!bad_root.verify(&span, &root), "tampered untouched root rejects");

        // honest proof, wrong expected root
        let mut wrong = root;
        wrong[0] ^= 0xff;
        assert!(!rp.verify(&span, &wrong), "wrong expected root rejects");

        // dropping a needed boundary node makes the proof insufficient
        let mut missing = rp.clone();
        missing.nodes.pop();
        assert!(!missing.verify(&span, &root), "missing boundary node rejects");
    }

    // Every (old < new) length pair produces an upgrade proof that an honest
    // verifier (holding the genuine old roots) accepts — the length-extension
    // round-trip across the whole shape space.
    #[test]
    fn upgrade_proof_roundtrip_all_sizes() {
        for new in 1..=20u64 {
            let (t, blocks) = tree(new as usize);
            let new_root = t.root_hash();
            for old in 1..new {
                let old_roots = tree_from(&blocks[..old as usize]).roots();
                let up = t
                    .upgrade_proof(old, new)
                    .expect("1 <= old < new <= len has a proof");
                assert!(
                    up.verify(&old_roots, &new_root),
                    "honest upgrade must verify (old={old}, new={new})"
                );
            }
        }
    }

    // Extend by exactly one block — the smallest, most common upgrade.
    #[test]
    fn upgrade_proof_single_step() {
        for new in 2..=18u64 {
            let (t, blocks) = tree(new as usize);
            let old = new - 1;
            let old_roots = tree_from(&blocks[..old as usize]).roots();
            let up = t.upgrade_proof(old, new).unwrap();
            assert!(up.verify(&old_roots, &t.root_hash()), "single-step upgrade (new={new})");
        }
    }

    // The proof carries only *fully-new* subtree nodes (every covered block is
    // `>= old`); it never ships old data. This is what the anti-fork soundness
    // argument rests on.
    #[test]
    fn upgrade_proof_supplies_only_fully_new_nodes() {
        for new in 1..=20u64 {
            let (t, _) = tree(new as usize);
            for old in 1..new {
                let up = t.upgrade_proof(old, new).unwrap();
                for n in &up.nodes {
                    let (first, end) = flat::block_range(n.index);
                    assert!(
                        first >= old,
                        "supplied node {} covers old data [{first},{end}) (old={old}, new={new})",
                        n.index
                    );
                    assert!(end <= new, "supplied node must stay within the new tree");
                }
            }
        }
    }

    // Anti-fork across lengths: a verifier holding the *honest* prefix rejects a
    // longer head that rewrote an old block — even though the proof is internally
    // well-formed for the forked tree.
    #[test]
    fn upgrade_proof_detects_old_rewrite() {
        let (honest, blocks) = tree(8);
        let old = 5u64;
        let honest_old_roots = tree_from(&blocks[..old as usize]).roots();

        // Sanity: the honest extension verifies under the honest old roots.
        let honest_up = honest.upgrade_proof(old, 8).unwrap();
        assert!(honest_up.verify(&honest_old_roots, &honest.root_hash()));

        // Fork: identical except block 2 (which is < old) is rewritten.
        let mut forked_blocks = blocks.clone();
        forked_blocks[2] = b"rewritten".to_vec();
        let forked = tree_from(&forked_blocks);
        assert_ne!(forked.root_hash(), honest.root_hash());

        let forked_up = forked.upgrade_proof(old, 8).unwrap();
        // The forked proof is self-consistent against the *forked* old roots...
        let forked_old_roots = tree_from(&forked_blocks[..old as usize]).roots();
        assert!(forked_up.verify(&forked_old_roots, &forked.root_hash()));
        // ...but a verifier trusting the honest prefix must reject it.
        assert!(
            !forked_up.verify(&honest_old_roots, &forked.root_hash()),
            "honest old prefix must reject a forked extension"
        );
    }

    // Tamper-rejection across every input the verifier trusts.
    #[test]
    fn upgrade_proof_rejects_tampering() {
        let (t, blocks) = tree(13);
        let new_root = t.root_hash();
        let old = 6u64;
        let old_roots = tree_from(&blocks[..old as usize]).roots();
        let up = t.upgrade_proof(old, 13).unwrap();
        assert!(!up.nodes.is_empty(), "this upgrade needs new nodes");
        assert!(up.verify(&old_roots, &new_root)); // honest baseline

        // tampered supplied node
        let mut bad_node = up.clone();
        bad_node.nodes[0].hash[0] ^= 0xff;
        assert!(!bad_node.verify(&old_roots, &new_root), "tampered new node rejects");

        // wrong expected new head
        let mut wrong = new_root;
        wrong[0] ^= 0xff;
        assert!(!up.verify(&old_roots, &wrong), "wrong new head rejects");

        // dropping a needed node makes the proof insufficient
        let mut missing = up.clone();
        missing.nodes.pop();
        assert!(!missing.verify(&old_roots, &new_root), "missing node rejects");

        // tampered old root (the verifier's own trusted state, mutated)
        let mut bad_old = old_roots.clone();
        bad_old[0].hash[0] ^= 0xff;
        assert!(!up.verify(&bad_old, &new_root), "tampered old root rejects");

        // old roots of the wrong length (shape mismatch with the proof's old_len)
        let wrong_len_old = tree_from(&blocks[..(old as usize - 1)]).roots();
        assert!(!up.verify(&wrong_len_old, &new_root), "mismatched old shape rejects");

        // injecting a fully-old node (a fork attempt: stand in for the verifier's
        // own old data) is refused — supplied nodes must be fully new. Leaf 0 =
        // block 0, which lies in the trusted old prefix.
        let mut injected = up.clone();
        injected.nodes.insert(
            0,
            Node { index: 0, hash: leaf_hash(&blocks[0]), size: blocks[0].len() as u64 },
        );
        assert!(!injected.verify(&old_roots, &new_root), "injected old-region node rejects");
    }

    // Out-of-range / degenerate requests produce no proof.
    #[test]
    fn upgrade_proof_out_of_range() {
        let (t, _) = tree(8);
        assert!(t.upgrade_proof(0, 8).is_none(), "old=0 has no anchor");
        assert!(t.upgrade_proof(8, 8).is_none(), "old==new is not an extension");
        assert!(t.upgrade_proof(5, 3).is_none(), "old>new inverted");
        assert!(t.upgrade_proof(3, 9).is_none(), "new past length");
    }

    // The upgrade proof and a range proof compose: confirm the extension is an
    // honest append, then verify the new blocks themselves against the same head.
    #[test]
    fn upgrade_proof_composes_with_range_proof() {
        let (t, blocks) = tree(14);
        let new_root = t.root_hash();
        let old = 5u64;
        let old_roots = tree_from(&blocks[..old as usize]).roots();

        // 1. append-only / anti-fork across lengths (no data)
        assert!(t.upgrade_proof(old, 14).unwrap().verify(&old_roots, &new_root));
        // 2. the new blocks [old, 14) verify against the same (now trusted) head
        let rp = t.range_proof(old, 14).unwrap();
        assert!(rp.verify(&blocks[old as usize..], &new_root));
    }

    // A tree with varied (cycling 1..=5) block sizes so byte seeks are non-trivial.
    fn varied_tree(n: usize) -> (MerkleTree, Vec<Vec<u8>>) {
        let mut t = MerkleTree::new();
        let mut blocks = Vec::new();
        for i in 0..n {
            let b = vec![b'a' + (i % 26) as u8; (i % 5) + 1];
            t.append(&b);
            blocks.push(b);
        }
        (t, blocks)
    }

    // The naive linear reference for a byte seek (sum leaf sizes left-to-right).
    fn linear_seek(blocks: &[Vec<u8>], bytes: u64) -> (u64, u64) {
        let mut remaining = bytes;
        for (i, b) in blocks.iter().enumerate() {
            if b.len() as u64 > remaining {
                return (i as u64, remaining);
            }
            remaining -= b.len() as u64;
        }
        (blocks.len() as u64, remaining)
    }

    // The tree-accelerated seek agrees with the linear reference for every byte
    // offset — ports `merkle-tree.js` "basic tree seeks". Also checks past-the-end.
    #[test]
    fn seek_matches_linear_all_sizes() {
        for n in 1..=20usize {
            let (t, blocks) = varied_tree(n);
            let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
            for bytes in 0..total {
                assert_eq!(
                    t.seek(bytes),
                    linear_seek(&blocks, bytes),
                    "tree seek must match linear seek (n={n}, bytes={bytes})"
                );
            }
            // Past-the-end: exactly at total lands on (len, 0); beyond carries over.
            assert_eq!(t.seek(total), (n as u64, 0));
            assert_eq!(t.seek(total + 3), (n as u64, 3));
        }
    }

    // Every in-range byte offset has a seek proof that verifies against the signed
    // root and returns the same `(block, offset)` as the local seek.
    #[test]
    fn seek_proof_roundtrip_all_sizes() {
        for n in 1..=20usize {
            let (t, blocks) = varied_tree(n);
            let root = t.root_hash();
            let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
            for bytes in 0..total {
                let sp = t.seek_proof(bytes).expect("in-range seek has a proof");
                assert_eq!(
                    sp.verify(&root),
                    Some(t.seek(bytes)),
                    "seek proof must authenticate the local seek (n={n}, bytes={bytes})"
                );
            }
        }
    }

    // Hand-checked block boundaries: a byte exactly on a block start belongs to
    // that block at offset 0; the byte before it is the last byte of the previous.
    #[test]
    fn seek_proof_pins_block_at_boundaries() {
        // sizes 1,2,3,4,5 -> cumulative starts 0,1,3,6,10, total 15
        let (t, _) = varied_tree(5);
        let root = t.root_hash();
        let starts = [0u64, 1, 3, 6, 10];
        for (block, &start) in starts.iter().enumerate() {
            let size = (block % 5) as u64 + 1;
            // first byte of the block
            assert_eq!(t.seek_proof(start).unwrap().verify(&root), Some((block as u64, 0)));
            // last byte of the block
            let last = start + size - 1;
            assert_eq!(
                t.seek_proof(last).unwrap().verify(&root),
                Some((block as u64, size - 1))
            );
        }
    }

    // A seek at or past the end of the log has no block to locate.
    #[test]
    fn seek_proof_past_end_is_none() {
        let (t, blocks) = varied_tree(7);
        let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
        assert!(t.seek_proof(total).is_none(), "byte == total is past the last block");
        assert!(t.seek_proof(total + 5).is_none(), "byte past total is unlocatable");
        assert!(MerkleTree::new().seek_proof(0).is_none(), "empty tree has no blocks");
    }

    // Tamper-rejection across every input the verifier trusts.
    #[test]
    fn seek_proof_rejects_tampering() {
        let (t, blocks) = varied_tree(11);
        let root = t.root_hash();
        // byte offset inside block 4 (an interior block, so the proof has siblings
        // and the climb crosses at least one root boundary).
        let cum4: u64 = blocks[..4].iter().map(|b| b.len() as u64).sum();
        let bytes = cum4 + 0; // first byte of block 4
        let sp = t.seek_proof(bytes).unwrap();
        assert!(!sp.siblings.is_empty(), "interior block needs siblings");
        assert_eq!(sp.verify(&root), Some((4, 0))); // honest baseline

        // tampered leaf hash
        let mut bad = sp.clone();
        bad.leaf.hash[0] ^= 0xff;
        assert!(bad.verify(&root).is_none(), "tampered leaf hash rejects");

        // tampered leaf size (would shift the bracket; the climb also diverges)
        let mut bad = sp.clone();
        bad.leaf.size += 1;
        assert!(bad.verify(&root).is_none(), "tampered leaf size rejects");

        // tampered sibling
        let mut bad = sp.clone();
        bad.siblings[0].hash[0] ^= 0xff;
        assert!(bad.verify(&root).is_none(), "tampered sibling rejects");

        // tampered untouched root entry (the containing root is substituted over,
        // so mutate a different one — bound by tree_hash).
        let root_indices: Vec<u64> = sp.roots.iter().map(|n| n.index).collect();
        let leaf_root = {
            // find which root the leaf climbs to, to mutate a *different* one
            let mut node = sp.leaf.index;
            while !root_indices.contains(&node) {
                node = flat::parent(node);
            }
            node
        };
        let other = sp.roots.iter().position(|r| r.index != leaf_root);
        assert!(other.is_some(), "an 11-block tree has multiple roots");
        let mut bad = sp.clone();
        bad.roots[other.unwrap()].hash[0] ^= 0xff;
        assert!(bad.verify(&root).is_none(), "tampered untouched root rejects");

        // wrong expected root
        let mut wrong = root;
        wrong[0] ^= 0xff;
        assert!(sp.verify(&wrong).is_none(), "wrong expected root rejects");

        // dropped sibling -> climb cannot reach a real root
        let mut bad = sp.clone();
        bad.siblings.pop();
        assert!(bad.verify(&root).is_none(), "dropped sibling rejects");

        // tampered `bytes` to a value in a *different* block: the proof genuinely
        // proves block 4's interval, which no longer brackets the byte -> None.
        let mut bad = sp.clone();
        bad.bytes = cum4 + (blocks[4].len() as u64); // first byte of block 5
        assert!(bad.verify(&root).is_none(), "bytes outside the proven block rejects");
    }

    // A power-of-two tree has a single root; seeks/proofs still hold.
    #[test]
    fn seek_proof_single_root() {
        let (t, blocks) = varied_tree(8); // one root (index 7)
        assert_eq!(t.roots().len(), 1, "8 blocks => single root");
        let root = t.root_hash();
        let total: u64 = blocks.iter().map(|b| b.len() as u64).sum();
        for bytes in 0..total {
            assert_eq!(t.seek_proof(bytes).unwrap().verify(&root), Some(t.seek(bytes)));
        }
    }

    // recovery: a tree with its tree nodes deleted still reports its length and
    // does not panic — ports `merkle-tree-recovery.js` "core can still ready" +
    // "still has length".
    #[test]
    fn recovery_corrupt_tree_keeps_length() {
        let (mut t, _) = tree(30); // 4 roots
        let roots: Vec<u64> = t.roots().iter().map(|n| n.index).collect();
        assert!(roots.len() > 1, "30 blocks has multiple roots");

        for r in &roots {
            assert!(t.remove_node(*r), "deleting a present root");
        }

        // Length survives; no panic querying it.
        assert_eq!(t.len(), 30);
        assert!(!t.is_intact(), "missing roots => repair mode");
        assert_eq!(t.try_root_hash(), None, "cannot build a root hash with roots gone");

        // The missing set is exactly the deleted roots — nothing else was touched.
        let mut missing = t.missing_nodes();
        missing.sort_unstable();
        let mut expect = roots.clone();
        expect.sort_unstable();
        assert_eq!(missing, expect);
    }

    // recovery: a deleted *root* is restored from a remote proof verified against
    // the signed root — ports "fix via fully remote proof".
    #[test]
    fn recovery_root_via_remote_proof() {
        let (healthy, _) = tree(30);
        let root_hash = healthy.root_hash();
        let root_index = healthy.roots()[0].index; // first root, covers [0,16)
        let proof = healthy.node_proof(root_index).expect("healthy can prove its root");

        let mut corrupt = healthy.clone();
        assert!(corrupt.remove_node(root_index));
        assert_eq!(corrupt.len(), 30, "length survives corruption");
        assert!(!corrupt.is_intact());
        assert_eq!(corrupt.try_root_hash(), None, "cannot create tree hash with a root gone");
        assert!(corrupt.node_proof(root_index).is_none(), "corrupt source cannot prove the lost node");

        assert!(corrupt.recover_node(&proof, &root_hash), "honest remote proof recovers the node");
        assert!(corrupt.has_node(root_index));
        assert!(corrupt.is_intact(), "recovered tree is whole again");
        assert_eq!(corrupt.try_root_hash(), Some(root_hash), "root hash reconstructed exactly");
    }

    // recovery: a deleted *interior sub-root* is restored from a remote proof; the
    // (still-present) root hash is unaffected by the gap, but the node itself is
    // gone until recovered — ports "fix via fully remote proof" for a sub root.
    #[test]
    fn recovery_subroot_via_remote_proof() {
        let (healthy, _) = tree(64); // single root 63
        let root_hash = healthy.root_hash();
        let subroot = 15u64; // covers blocks [0,16): root 63 -> 31 -> 15
        assert!(healthy.has_node(subroot));
        let proof = healthy.node_proof(subroot).expect("prove the sub-root");
        let original = proof.node;

        let mut corrupt = healthy.clone();
        assert!(corrupt.remove_node(subroot));
        assert!(!corrupt.is_intact(), "a missing sub-root is repair mode");
        // A sub-root gap does not prevent the still-present root hash...
        assert_eq!(corrupt.try_root_hash(), Some(root_hash));
        // ...but the node itself is gone and cannot be re-proven locally.
        assert!(corrupt.node_proof(subroot).is_none());

        assert!(corrupt.recover_node(&proof, &root_hash));
        assert!(corrupt.is_intact());
        // The recovered node is exactly the original (hash + size reconstructed),
        // and it is provable again against the signed root.
        let reproof = corrupt.node_proof(subroot).expect("provable again");
        assert_eq!(reproof.node, original);
        assert_eq!(reproof.verify(&root_hash), Some(original));
    }

    // recovery security/atomicity: a mangled remote proof is rejected and the tree
    // is left unchanged (node stays missing) — ports "atomically updates storage".
    #[test]
    fn recovery_rejects_tampered_proof_atomically() {
        let (healthy, _) = tree(64); // single root 63
        let root_hash = healthy.root_hash();
        let target = 15u64; // interior node: its proof carries siblings [47, 95]
        let proof = healthy.node_proof(target).unwrap();
        assert!(!proof.siblings.is_empty(), "interior node needs siblings");

        let assert_untouched = |c: &MerkleTree| {
            assert!(!c.has_node(target), "tampered recovery must not store the node");
            assert!(!c.is_intact(), "still in repair mode");
        };

        // mangled node size (upstream mangles the proven node's size)
        let mut bad = proof.clone();
        bad.node.size += 1;
        let mut corrupt = healthy.clone();
        corrupt.remove_node(target);
        assert!(!corrupt.recover_node(&bad, &root_hash), "mangled size rejected");
        assert_untouched(&corrupt);

        // mangled node hash
        let mut bad = proof.clone();
        bad.node.hash[0] ^= 0xff;
        let mut corrupt = healthy.clone();
        corrupt.remove_node(target);
        assert!(!corrupt.recover_node(&bad, &root_hash), "mangled hash rejected");
        assert_untouched(&corrupt);

        // tampered sibling
        let mut bad = proof.clone();
        bad.siblings[0].hash[0] ^= 0xff;
        let mut corrupt = healthy.clone();
        corrupt.remove_node(target);
        assert!(!corrupt.recover_node(&bad, &root_hash), "tampered sibling rejected");
        assert_untouched(&corrupt);

        // dropped sibling -> climb cannot reach a real root
        let mut bad = proof.clone();
        bad.siblings.pop();
        let mut corrupt = healthy.clone();
        corrupt.remove_node(target);
        assert!(!corrupt.recover_node(&bad, &root_hash), "dropped sibling rejected");
        assert_untouched(&corrupt);

        // honest proof, wrong expected root
        let mut wrong = root_hash;
        wrong[0] ^= 0xff;
        let mut corrupt = healthy.clone();
        corrupt.remove_node(target);
        assert!(!corrupt.recover_node(&proof, &wrong), "wrong expected root rejected");
        assert_untouched(&corrupt);

        // finally, the honest proof recovers cleanly after the failed attempts
        assert!(corrupt.recover_node(&proof, &root_hash));
        assert!(corrupt.is_intact());
    }

    // recovery: appends are refused while in repair mode, and resume after the
    // missing node is recovered — ports "fail appends … when in repair mode".
    #[test]
    fn recovery_append_refused_in_repair_mode() {
        let (mut t, _) = tree(30);
        let root_hash = t.root_hash();
        let root_index = t.roots()[0].index;
        let proof = t.node_proof(root_index).unwrap(); // capture while healthy

        assert!(t.remove_node(root_index));
        assert!(!t.is_intact());
        assert_eq!(t.try_append(b"nope"), Err(InRepairMode), "cannot extend in repair mode");
        assert_eq!(t.len(), 30, "the refused append did not change the length");

        // Recover, then appending works again and the tree grows.
        assert!(t.recover_node(&proof, &root_hash));
        assert!(t.is_intact());
        assert_eq!(t.try_append(b"now ok").expect("append after recovery"), 30);
        assert_eq!(t.len(), 31);
    }

    // recovery round-trip: every stored node (leaf, interior, root) over a range
    // of tree sizes proves & verifies against the signed root, and recovers a copy
    // that had exactly that node deleted back to intact.
    #[test]
    fn node_proof_roundtrip_all_nodes() {
        for n in 1..=16u64 {
            let (t, _) = tree(n as usize);
            let root = t.root_hash();
            for i in 0..(2 * n) {
                let (_, end) = flat::block_range(i);
                if end > n {
                    continue; // not a complete subtree of this tree
                }
                let proof = t.node_proof(i).expect("every stored node is provable");
                assert_eq!(proof.verify(&root), Some(proof.node), "node proof must verify (n={n}, i={i})");

                let mut corrupt = t.clone();
                assert!(corrupt.remove_node(i), "node {i} was present");
                assert!(!corrupt.is_intact(), "deleting node {i} => repair mode (n={n})");
                assert!(corrupt.recover_node(&proof, &root), "honest proof recovers node {i} (n={n})");
                assert!(corrupt.is_intact(), "recovered tree intact (n={n}, i={i})");
            }
        }
    }

    // truncate: rewinding to `new_len` leaves a tree node-for-node identical to
    // a fresh tree of the first `new_len` blocks — for every (new_len < n) over a
    // range of sizes. The root hash, node set, byte length, and proofs all match.
    #[test]
    fn truncate_equals_fresh_prefix_all_sizes() {
        for n in 1..=20u64 {
            for new_len in 0..n {
                let (mut t, blocks) = tree(n as usize);
                assert!(t.truncate(new_len), "truncate {n}->{new_len} changes the tree");
                assert_eq!(t.len(), new_len);

                let fresh = tree_from(&blocks[..new_len as usize]);
                assert_eq!(t.len(), fresh.len());
                assert_eq!(t.root_hash(), fresh.root_hash(), "root == prefix root ({n}->{new_len})");
                assert_eq!(t.byte_length(), fresh.byte_length(), "byte_length == prefix");
                assert_eq!(t.roots(), fresh.roots(), "root nodes identical");
                // The node maps coincide exactly (no stale nodes left behind).
                let live: Vec<u64> = t.missing_nodes();
                assert!(live.is_empty(), "truncated tree is intact ({n}->{new_len})");
                // Every surviving block still proves against the truncated root.
                for b in 0..new_len {
                    let p = t.proof(b).expect("surviving block proves");
                    assert!(p.verify(&blocks[b as usize], &t.root_hash()), "block {b} proves");
                }
                // A block past the new length is gone.
                assert!(t.proof(new_len).is_none(), "truncated block has no proof");
            }
        }
    }

    // truncate byte_length tracks the live prefix byte size exactly.
    #[test]
    fn truncate_byte_length() {
        let mut t = MerkleTree::new();
        for b in [&b"hello"[..], b"world", b"fo", b"ooo"] {
            t.append(b);
        }
        assert_eq!(t.byte_length(), 15); // 5+5+2+3
        assert!(t.truncate(3));
        assert_eq!(t.byte_length(), 12); // 5+5+2
        assert!(t.truncate(2));
        assert_eq!(t.byte_length(), 10); // 5+5
        assert!(t.truncate(0));
        assert_eq!(t.byte_length(), 0);
        assert!(t.is_empty());
        assert_eq!(t.root_hash(), MerkleTree::new().root_hash(), "empty == fresh empty");
    }

    // truncate is a no-op (returns false, no change) when new_len >= len, and a
    // truncated tree can be appended to again, re-deriving the discarded indices.
    #[test]
    fn truncate_noop_and_reappend() {
        let (mut t, _) = tree(5);
        let root5 = t.root_hash();
        assert!(!t.truncate(5), "truncate to current length is a no-op");
        assert!(!t.truncate(9), "truncate beyond length is a no-op");
        assert_eq!(t.root_hash(), root5, "no-op truncate left the tree unchanged");

        assert!(t.truncate(3));
        // Re-append two blocks; the result equals a fresh 5-block tree of the new
        // content (the reused indices are overwritten cleanly).
        t.append(b"new-3");
        t.append(b"new-4");
        let mut fresh = tree_from(&[b"block-0".to_vec(), b"block-1".to_vec(), b"block-2".to_vec()]);
        fresh.append(b"new-3");
        fresh.append(b"new-4");
        assert_eq!(t.root_hash(), fresh.root_hash(), "re-append after truncate is clean");
        assert!(t.is_intact());
    }

    // After a reorg, `local` must be byte-identical to `remote`: same length,
    // same roots, same root hash, intact, and every block proves.
    fn assert_followed(local: &MerkleTree, remote: &MerkleTree, blocks: &[Vec<u8>]) {
        assert_eq!(local.len(), remote.len(), "reorg adopts remote's length");
        assert_eq!(local.roots(), remote.roots(), "reorg adopts remote's roots");
        assert_eq!(local.root_hash(), remote.root_hash(), "byte-identical after reorg");
        assert_eq!(local.byte_length(), remote.byte_length(), "byte_length follows remote");
        assert!(local.is_intact(), "reorged tree is intact");
        let root = remote.root_hash();
        for b in 0..remote.len() {
            let p = local.proof(b).expect("every adopted block proves");
            assert!(p.verify(&blocks[b as usize], &root), "block {b} proves after reorg");
        }
    }

    // Two trees built from identical content where one is a strict prefix of the
    // other: LCA is the shorter length, and the shorter reorgs up to the longer
    // (and vice versa) byte-identically. Ports merkle-tree.js "lowest common
    // ancestor - small gap / bigger gap / remote is shorter than local".
    #[test]
    fn lca_prefix_gaps() {
        for &(remote_n, local_n, expect) in &[(10u64, 8u64, 8u64), (20, 1, 1), (5, 10, 5)] {
            let (remote, rblocks) = tree(remote_n as usize);
            let (mut local, _) = tree(local_n as usize);
            assert_eq!(
                local.lowest_common_ancestor(&remote),
                expect,
                "LCA(remote={remote_n}, local={local_n})"
            );
            // Reorg always makes `local` follow `remote` (up or down to its length).
            let ancestors = local.reorg(&remote);
            assert_eq!(ancestors, expect, "reorg returns the LCA");
            assert_followed(&local, &remote, &rblocks);
        }
    }

    // Both trees share a prefix then diverge at one block. LCA is the shared
    // length; the local follows the remote onto its fork. Ports merkle-tree.js
    // "lowest common ancestor - simple fork".
    #[test]
    fn lca_simple_fork() {
        let shared: Vec<Vec<u8>> = (0..5).map(|i| format!("block-{i}").into_bytes()).collect();
        let mut remote = tree_from(&shared);
        remote.append(b"fork #1");
        let mut local = tree_from(&shared);
        local.append(b"fork #2");

        assert_eq!(local.lowest_common_ancestor(&remote), 5, "diverge at block 5");
        let mut rblocks = shared.clone();
        rblocks.push(b"fork #1".to_vec());

        let ancestors = local.reorg(&remote);
        assert_eq!(ancestors, 5);
        assert_followed(&local, &remote, &rblocks);
    }

    // Diverge at block 5, then each side appends 100 more blocks (a long fork).
    // LCA is still the shared prefix; the local fully adopts the remote's fork.
    // Ports merkle-tree.js "lowest common ancestor - long fork".
    #[test]
    fn lca_long_fork() {
        let shared: Vec<Vec<u8>> = (0..5).map(|i| format!("block-{i}").into_bytes()).collect();
        let mut rblocks = shared.clone();
        rblocks.push(b"fork #1".to_vec());
        let mut lblocks = shared.clone();
        lblocks.push(b"fork #2".to_vec());
        for i in 0..100u64 {
            rblocks.push(format!("r#{i}").into_bytes());
            lblocks.push(format!("l#{i}").into_bytes());
        }
        let remote = tree_from(&rblocks);
        let mut local = tree_from(&lblocks);

        assert_eq!(local.lowest_common_ancestor(&remote), 5, "LCA is the shared prefix");
        let ancestors = local.reorg(&remote);
        assert_eq!(ancestors, 5);
        assert_followed(&local, &remote, &rblocks);
    }

    // Property: for every shared-prefix length `k` and every divergence shape,
    // the LCA is exactly `k`. Covers prefix-only (no divergence ⇒ LCA = min len),
    // divergence at `k`, and identical trees (LCA = full length, reorg is a no-op).
    #[test]
    fn lca_all_divergence_points() {
        for total in 1..=16u64 {
            for k in 0..=total {
                // Two trees agreeing on `[0, k)`, then differing from block `k`.
                let mut ablocks: Vec<Vec<u8>> = Vec::new();
                let mut bblocks: Vec<Vec<u8>> = Vec::new();
                for i in 0..total {
                    let shared = format!("s-{i}").into_bytes();
                    if i < k {
                        ablocks.push(shared.clone());
                        bblocks.push(shared);
                    } else {
                        ablocks.push(format!("a-{i}").into_bytes());
                        bblocks.push(format!("b-{i}").into_bytes());
                    }
                }
                let a = tree_from(&ablocks);
                let mut b = tree_from(&bblocks);
                // When k == total the trees are identical ⇒ LCA = total.
                assert_eq!(b.lowest_common_ancestor(&a), k, "LCA(total={total}, k={k})");
                assert_eq!(a.lowest_common_ancestor(&b), k, "LCA is symmetric");

                let was_noop = b.root_hash() == a.root_hash();
                b.reorg(&a);
                assert_followed(&b, &a, &ablocks);
                if was_noop {
                    // Identical trees: reorg changes nothing.
                    assert_eq!(b.len(), total);
                }
            }
        }
    }

    // Reorg keeps the shared prefix rather than rebuilding it: the surviving
    // prefix nodes are exactly the ones the common ancestor already held (same
    // hashes), so a block in `[0, ancestors)` proves under the *pre-reorg* root
    // too — the prefix was never rewritten.
    #[test]
    fn reorg_preserves_shared_prefix() {
        let shared: Vec<Vec<u8>> = (0..6).map(|i| format!("block-{i}").into_bytes()).collect();
        let mut remote = tree_from(&shared);
        remote.append(b"R");
        let mut local = tree_from(&shared);
        local.append(b"L");

        // The shared prefix's root hash before the reorg.
        let prefix_root = {
            let mut p = local.clone();
            p.truncate(6);
            p.root_hash()
        };
        let ancestors = local.reorg(&remote);
        assert_eq!(ancestors, 6);
        // After the reorg, truncating back to the ancestor reproduces the very
        // same prefix root — the common prefix was preserved, not re-derived.
        let mut back = local.clone();
        back.truncate(ancestors);
        assert_eq!(back.root_hash(), prefix_root, "shared prefix preserved across reorg");
    }

    // --- audit regression tests (post-iteration-21) ---

    // P0 soundness: a seek target must be a real block leaf. Passing the root node
    // (odd index) authenticates against the real root and its aggregate subtree size
    // brackets any offset; without the evenness guard, `verify` returned a bogus
    // `index / 2` block. (Upstream's ByteSeeker guards `(index & 1) === 0`.)
    #[test]
    fn seek_rejects_non_leaf_target() {
        let (t, _) = tree(4);
        let root = t.root_hash();
        let root_node = t.roots()[0]; // index 3 (odd) for 4 blocks
        assert_eq!(root_node.index & 1, 1, "the 4-block root is an interior (odd) node");

        let forged = SeekProof {
            bytes: 0,
            leaf: root_node,
            siblings: vec![],
            roots: t.roots(),
        };
        assert!(
            forged.verify(&root).is_none(),
            "an interior node must not be accepted as a seek leaf"
        );
    }

    // P1 defense-in-depth: a proof sibling must be the path node's actual sibling.
    // `parent_hash` binds child hash+size but NOT index, so a falsified same-side
    // sibling index leaves the climb hash unchanged — only the structural guard
    // rejects it.
    #[test]
    fn proof_rejects_falsified_sibling_index() {
        let (t, blocks) = tree(4);
        let root = t.root_hash();
        let mut proof = t.proof(0).unwrap();
        assert!(proof.verify(&blocks[0], &root), "honest proof verifies");

        // Real sibling of leaf 0 is index 2; forge the index to another same-side leaf.
        proof.siblings[0].index = 6;
        assert!(
            !proof.verify(&blocks[0], &root),
            "a sibling at the wrong index must be rejected structurally"
        );
    }

    // --- audit follow-up: reorg / LCA adversarial + seek zero-size (iter 25) ---

    // Two length-`len` trees sharing blocks `[0, share)` then diverging.
    fn forked_pair(share: u64, len: u64) -> (MerkleTree, MerkleTree, Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let mut ablocks: Vec<Vec<u8>> = Vec::new();
        let mut bblocks: Vec<Vec<u8>> = Vec::new();
        for i in 0..len {
            if i < share {
                let s = format!("s-{i}").into_bytes();
                ablocks.push(s.clone());
                bblocks.push(s);
            } else {
                ablocks.push(format!("a-{i}").into_bytes());
                bblocks.push(format!("b-{i}").into_bytes());
            }
        }
        (tree_from(&ablocks), tree_from(&bblocks), ablocks, bblocks)
    }

    // `lowest_common_ancestor` is content-blind and depends on both trees being
    // intact; a missing node reads conservatively as disagreement. The invariant
    // the binary search keeps — `agree(lo)` is always true, and `agree(a)` is true
    // only when *both* trees produce equal prefix-root-hashes at `a` — means a
    // corrupt input can only *shrink* the LCA, never over-claim. Whatever it
    // returns is a genuine shared prefix (a real ancestor), with no panic.
    #[test]
    fn lca_conservative_under_corruption() {
        // self = a (intact); other = b. They genuinely share [0, 6) of length 8.
        let (a, b, _ablocks, _bblocks) = forked_pair(6, 8);
        let intact = a.lowest_common_ancestor(&b);
        assert_eq!(intact, 6, "intact LCA is the true shared prefix");

        // --- corrupt `other`: remove node 9 (root of blocks [4,6)), which the
        // length-6 prefix needs. The gap reads as disagreement, so the LCA falls
        // back to a genuine shorter shared prefix — never larger than the intact LCA.
        let mut b_corrupt = b.clone();
        assert!(b_corrupt.remove_node(9));
        assert_eq!(b_corrupt.prefix_root_hash(6), None, "length-6 prefix now unavailable");
        let lca = a.lowest_common_ancestor(&b_corrupt);
        assert!(lca <= intact, "corruption can only shrink the LCA, never grow it");
        assert!(b_corrupt.prefix_root_hash(lca).is_some(), "returned LCA is computable");
        assert_eq!(
            a.prefix_root_hash(lca),
            b_corrupt.prefix_root_hash(lca),
            "the returned LCA is a genuine shared prefix, not a forged one"
        );

        // --- monotonicity-precondition violation: removing node 8 (block-4 leaf)
        // makes the `agree` predicate FALSE at length 5 (node 8 gone) yet TRUE at
        // length 6 (nodes 3, 9 present, content shared) — non-monotone. The binary
        // search must still land on a length where the prefixes genuinely match.
        let mut b_holey = b.clone();
        assert!(b_holey.remove_node(8));
        assert_eq!(b_holey.prefix_root_hash(5), None, "length-5 prefix unavailable (node 8 gone)");
        assert_eq!(
            b_holey.prefix_root_hash(6),
            a.prefix_root_hash(6),
            "yet length-6 prefix is intact and matches — agreement is non-monotone"
        );
        let lca = a.lowest_common_ancestor(&b_holey);
        assert!(lca <= intact && lca > 0, "still a conservative, non-empty ancestor");
        assert_eq!(
            a.prefix_root_hash(lca),
            b_holey.prefix_root_hash(lca),
            "non-monotone agreement still yields a genuine ancestor"
        );

        // --- gapped `self`: corruption is symmetric and equally conservative.
        let mut a_corrupt = a.clone();
        assert!(a_corrupt.remove_node(9));
        let lca = a_corrupt.lowest_common_ancestor(&b);
        assert!(lca <= intact, "a gap in self also only shrinks the LCA");
        assert_eq!(
            a_corrupt.prefix_root_hash(lca),
            b.prefix_root_hash(lca),
            "gapped self still returns a genuine shared prefix"
        );
    }

    // The precondition the LCA binary search relies on: for two INTACT trees,
    // prefix agreement is monotone — agreeing on `[0, a)` implies agreeing on every
    // shorter prefix — so the search is exact (no over- or under-shoot).
    #[test]
    fn lca_intact_agreement_is_monotone() {
        let (a, b, _, _) = forked_pair(6, 9);
        let max = a.len().min(b.len());
        let agree: Vec<bool> = (0..=max)
            .map(|k| a.prefix_root_hash(k) == b.prefix_root_hash(k))
            .collect();
        // No agreement reappears after the first disagreement.
        if let Some(f) = agree.iter().position(|&x| !x) {
            assert!(agree[f..].iter().all(|&x| !x), "intact agreement is monotone");
        }
        // Diverging at block 6, [0,6) is shared so length-6 prefix agrees; length 7
        // (which covers block 6) does not.
        assert!(agree[6] && !agree[7], "boundary is exactly at the divergence");
        assert_eq!(a.lowest_common_ancestor(&b), 6, "binary search is exact for intact inputs");
    }

    // `reorg` adopts every node `other` holds, so an intact `other` is the
    // precondition for a clean follow: following a CORRUPT `other` faithfully
    // copies its gaps (self ends non-intact), while an intact `other` HEALS a
    // gapped `self` by overwriting the gap with the complete node set.
    #[test]
    fn reorg_precondition_on_intact_other() {
        // Corrupt `other`: removing a suffix node (block-6 leaf = index 12) is
        // copied into `self`, leaving it in repair mode.
        let (a, mut b, _ablocks, _bblocks) = forked_pair(4, 8);
        let mut a_corrupt = a.clone();
        assert!(a_corrupt.remove_node(12));
        let _ = b.reorg(&a_corrupt);
        assert_eq!(b.len(), 8, "reorg adopts other's length");
        assert!(!b.has_node(12), "the gap in other is copied verbatim");
        assert!(!b.is_intact(), "reorg copies other's corruption — intact-other is required");

        // Intact `other` heals a gapped `self`: remove a shared-region node
        // (node 3 = root of [0,4)) from self, then follow the intact other. Adopting
        // other's full node set overwrites the gap, so self ends intact + identical.
        let (a2, b2, ablocks2, _) = forked_pair(4, 8);
        let mut b2_holey = b2.clone();
        assert!(b2_holey.remove_node(3));
        assert!(!b2_holey.is_intact(), "self starts gapped");
        b2_holey.reorg(&a2);
        assert!(b2_holey.is_intact(), "intact other heals self's gap");
        assert_eq!(b2_holey.root_hash(), a2.root_hash(), "byte-identical follow");
        let root = a2.root_hash();
        for blk in 0..a2.len() {
            let p = b2_holey.proof(blk).expect("every adopted block proves");
            assert!(p.verify(&ablocks2[blk as usize], &root), "block {blk} proves after reorg");
        }
    }

    // Zero-size (empty) blocks are legitimate L1 payloads. A zero-size block
    // occupies an empty byte interval, so no byte offset lands in it — the seek
    // skips it to the next non-empty block — and the tree seek still agrees with a
    // linear scan, with seek proofs authenticating the same mapping.
    #[test]
    fn seek_handles_zero_size_blocks() {
        // Leading, interior, consecutive, and trailing empties.
        let sizes = [0usize, 2, 0, 0, 3, 1, 0];
        let mut t = MerkleTree::new();
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        for (i, &s) in sizes.iter().enumerate() {
            let b = vec![b'a' + i as u8; s];
            t.append(&b);
            blocks.push(b);
        }
        let total: u64 = sizes.iter().map(|&s| s as u64).sum();
        assert!(total > 0, "the tree has some bytes despite the empties");
        let root = t.root_hash();

        for bytes in 0..total {
            let located = t.seek(bytes);
            assert_eq!(located, linear_seek(&blocks, bytes), "tree seek == linear (bytes={bytes})");
            let (block, _off) = located;
            assert!(
                block < t.len() && !blocks[block as usize].is_empty(),
                "a byte never resolves to an empty block (bytes={bytes})"
            );
            let sp = t.seek_proof(bytes).expect("in-range byte has a seek proof");
            assert_eq!(
                sp.verify(&root),
                Some(located),
                "seek proof authenticates the located block (bytes={bytes})"
            );
        }
        // At/past the end there is no block to locate.
        assert_eq!(t.seek(total), (t.len(), 0), "byte == total is past the last block");
        assert!(t.seek_proof(total).is_none());

        // An all-empty tree has zero bytes: every offset is past the (zero) end.
        let mut empties = MerkleTree::new();
        for _ in 0..4 {
            empties.append(b"");
        }
        assert_eq!(empties.seek(0), (4, 0), "all-empty tree: byte 0 is past the end");
        assert!(empties.seek_proof(0).is_none(), "no block to locate in an all-empty tree");
    }
}
