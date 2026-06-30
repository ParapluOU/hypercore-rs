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

use std::collections::BTreeMap;

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


/// Flat-tree index arithmetic. Leaves at even indices (block `k` -> `2k`), parents at odd.
/// See the mafintosh `flat-tree` algorithm.
pub mod flat;

mod tree;

mod proof;
pub use proof::{InRepairMode, NodeProof, Proof, RangeProof, SeekProof, UpgradeProof};

#[cfg(test)]
mod tests;
