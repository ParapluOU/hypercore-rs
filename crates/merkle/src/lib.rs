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
}
