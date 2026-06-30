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
use storage::Store;

/// A node splits once it would hold this many keys (upstream `MAX_CHILDREN`).
const MAX_CHILDREN: usize = 9;

/// Minimum keys a **non-root** node may hold; below this it borrows from a sibling
/// or merges (upstream `MIN_KEYS = (MAX_CHILDREN - 1) / 2`). A fresh split yields
/// exactly `MIN_KEYS` keys per half, so `[MIN_KEYS, MAX_CHILDREN - 1]` is the legal
/// fill range for a non-root node.
const MIN_KEYS: usize = (MAX_CHILDREN - 1) / 2;

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

/// Result of deleting from a subtree: either nothing was present, or the subtree
/// was rewritten to `seq` and possibly fell below [`MIN_KEYS`] (so the parent must
/// rebalance it).
enum Del {
    NotFound,
    Down { seq: u64, underflow: bool },
}

/// An ordered key/value store: a copy-on-write B-tree over a `hypercore`.
pub struct Hyperbee<S> {
    core: Hypercore<Node, NodeCodec, S>,
}

/// A read-only view of a [`Hyperbee`] as of a past version (upstream `checkout`).
/// Created by [`Hyperbee::checkout`]; offers [`get`](Checkout::get) and
/// [`range`](Checkout::range) against the tree as it was at that version. Because
/// the tree is copy-on-write, every historic root block is still present, so a
/// checkout is just a read anchored at that root — no copy.
pub struct Checkout<'a, S> {
    bee: &'a Hyperbee<S>,
    /// Root block seq of the checked-out version (`None` for the empty version 0).
    root: Option<u64>,
    version: u64,
}


mod btree;

#[cfg(test)]
mod tests;
