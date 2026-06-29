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
}
