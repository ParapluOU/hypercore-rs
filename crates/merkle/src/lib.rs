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
}
