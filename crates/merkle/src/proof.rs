use std::collections::{BTreeMap, BTreeSet};

use crate::*;
use crate::{leaf_hash, parent_hash, tree_hash};

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
