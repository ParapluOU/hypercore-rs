use codec::Codec;
use merkle::{Hash, Proof};

use crate::*;

impl<T, C: Codec<T>> Snapshot<T, C> {
    /// The snapshotted length — fixed for the life of the snapshot.
    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// The fork counter at snapshot time.
    pub fn fork(&self) -> u64 {
        self.fork
    }

    /// The signed head captured at snapshot time (`None` for an empty core).
    pub fn head(&self) -> Option<&SignedHead> {
        self.head.as_ref()
    }

    /// The Merkle root hash of the snapshotted prefix.
    pub fn root_hash(&self) -> Hash {
        self.tree.root_hash()
    }

    /// The raw (codec-encoded) bytes of block `index` as captured, or `None` past
    /// the snapshot's length or for a block that was absent at snapshot time. This
    /// is the unit the snapshot's [`proof`](Self::proof) authenticates.
    pub fn block(&self, index: u64) -> Option<&[u8]> {
        if index >= self.length {
            return None;
        }
        self.blocks.get(&index).map(Vec::as_slice)
    }

    /// Decode the value at `index`, or `None` past the snapshot's length (a no-wait
    /// read — upstream's out-of-range `get` throws `SNAPSHOT_NOT_AVAILABLE`; at L1
    /// we report absence as `None`, consistent with [`Hypercore::get`]).
    pub fn get(&self, index: u64) -> Result<Option<T>, codec::Error> {
        match self.block(index) {
            Some(bytes) => Ok(Some(self.codec.decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// A Merkle inclusion proof for `index` against the snapshot's captured head —
    /// so a snapshot block is independently verifiable ([`verify_block`]) even
    /// after the original has forked away.
    pub fn proof(&self, index: u64) -> Option<Proof> {
        self.tree.proof(index)
    }

    /// How much of this snapshot is still backed by `core`'s **current** signed
    /// head: the length of the longest prefix the snapshot and the live core still
    /// share. It equals the snapshot's length while the core's history still
    /// contains the snapshotted prefix, and drops when the core truncates below the
    /// snapshot (or rewrites a block within it) — upstream's `snapshot.signedLength`.
    ///
    /// Computed content-blind from the two trees' shared prefix
    /// ([`MerkleTree::lowest_common_ancestor`]), so it never inspects a payload and
    /// never exceeds [`length`](Self::length).
    pub fn signed_length<S: Store>(&self, core: &Hypercore<T, C, S>) -> u64 {
        self.tree.lowest_common_ancestor(&core.tree)
    }
}
