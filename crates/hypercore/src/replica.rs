use std::marker::PhantomData;

use codec::Codec;
use identity::PublicKey;
use merkle::{Hash, MerkleTree, Proof, UpgradeProof};
use storage::Store;

use crate::*;
use crate::head_message;

impl ForkProof {
    /// Verify this is a genuine equivocation by `public`: both sides must be
    /// signed by `public` **at the same fork counter**, prove their block at
    /// `index`, and commit **different** bytes there. Returns `false` for anything
    /// else — a forged side, a cross-fork (legitimate-reorg) divergence, a
    /// consistent pair (same bytes), a tampered proof, or a mismatched index claim.
    pub fn verify(&self, public: &PublicKey) -> bool {
        self.head_a.fork == self.head_b.fork
            && verify_block(public, &self.head_a, self.index, &self.data_a, &self.proof_a)
            && verify_block(public, &self.head_b, self.index, &self.data_b, &self.proof_b)
            && self.data_a != self.data_b
    }
}

/// A verify-only replica of a [`Hypercore`]. It holds no secret key; it accepts
/// blocks accompanied by a proof against a signed head, verifies each, and

impl<T, C: Codec<T>, S: Store> Replica<T, C, S> {
    pub fn new(public: PublicKey, codec: C, store: S) -> Self {
        Self {
            public,
            codec,
            store,
            tree: MerkleTree::new(),
            head: None,
            _t: PhantomData,
        }
    }

    pub fn len(&self) -> u64 {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn root_hash(&self) -> Hash {
        self.tree.root_hash()
    }

    /// The signed head we have fully replicated up to (if any).
    pub fn verified_head(&self) -> Option<&SignedHead> {
        self.head.as_ref()
    }

    /// Verify the next block (`index` must equal the current length) against
    /// `head`, then append it. Returns whether it was accepted; a rejected block
    /// is **not** stored.
    pub fn add_block(
        &mut self,
        head: &SignedHead,
        index: u64,
        enc: &[u8],
        proof: &Proof,
    ) -> Result<bool, Error<S::Error>> {
        if index != self.tree.len() {
            return Ok(false); // must apply in order
        }
        if !verify_block(&self.public, head, index, enc, proof) {
            return Ok(false);
        }
        self.store.put(index, enc).map_err(Error::Storage)?;
        self.tree.append(enc);
        if self.tree.len() == head.length && self.tree.root_hash() == head.root {
            self.head = Some(head.clone());
        }
        Ok(true)
    }

    /// Verify that `new_head` is a genuine **append-only extension** of this
    /// replica's current verified state, using a length-extension
    /// [`UpgradeProof`] — the gate a replica applies *before* fetching a longer
    /// head's blocks.
    ///
    /// [`Self::add_block`] verifies each block against the head it came with, but
    /// an inclusion proof only ties a block to *that* head's root. A writer that
    /// forked/rewrote old history produces a self-consistent longer head whose
    /// blocks all verify against its own (forked) root — so without this check a
    /// replica could be lured onto a forked history that contradicts what it
    /// already verified. `verify_upgrade` ties the longer head back to what we
    /// already trust: it folds the proof's fully-new nodes into our **own** roots
    /// and must rebuild `new_head.root`. A forked/rewritten prefix fails the fold.
    ///
    /// Returns `true` only if the author signed `new_head`, the proof bridges
    /// exactly from our current length (`old_len == len()`) to the new head
    /// (`new_len == new_head.length > len()`), and the fold from our trusted roots
    /// reconstructs `new_head.root`. It does **not** mutate the replica — apply
    /// the new blocks with [`Self::add_block`] (against `new_head`) afterward.
    pub fn verify_upgrade(&self, new_head: &SignedHead, proof: &UpgradeProof) -> bool {
        proof.old_len == self.tree.len()
            && proof.new_len == new_head.length
            && new_head.length > self.tree.len()
            && self
                .public
                .verify(&head_message(new_head.fork, new_head.length, &new_head.root), &new_head.sig)
            && proof.verify(&self.tree.roots(), &new_head.root)
    }

    /// Verify that `new_head` is a legitimate **reorg** this replica should
    /// follow: a *higher-fork* signed head whose history shares this replica's
    /// `[0, ancestors)` prefix and append-only-extends it. The cross-fork
    /// analogue of [`Self::verify_upgrade`] — pure (no mutation).
    ///
    /// Where [`Self::verify_upgrade`] handles a **same-fork** extension anchored
    /// on the replica's *entire* current head, a reorg is the author rewriting
    /// history under a bumped `fork` counter ([`Hypercore::truncate`]): readers
    /// follow the highest fork, so the new head shares only a **proper prefix**
    /// `[0, ancestors)` (the [lowest common ancestor]) and diverges after it. The
    /// gate re-anchors the same data-free [`UpgradeProof`] on the replica's own
    /// roots *at `ancestors`* (`tree.prefix_roots`): those roots are identical to
    /// the source's roots at that length **iff** the prefix is genuinely shared,
    /// so the fold reaches `new_head.root` only for a real shared ancestor.
    ///
    /// Returns `true` only if: we already trust a head; `new_head.fork` is
    /// **strictly greater** than ours (a same/lower fork is a stale head or an
    /// *equivocation* — an attack, see [`conflicting_heads`] — never a history to
    /// adopt); the author signed `new_head`; `ancestors <= len()` and
    /// `<= new_head.length`; and the prefix is authenticated:
    /// - `ancestors == new_head.length` — a **pure truncation**: the new head *is*
    ///   our prefix at `ancestors` (`prefix_root_hash` must equal `new_head.root`);
    ///   no `proof` needed.
    /// - `ancestors == 0` — **no shared prefix**: nothing to anchor (an upgrade
    ///   proof needs `old >= 1`), so the signed higher-fork head is adopted from
    ///   scratch and every refetched block is verified against it by
    ///   [`Self::add_block`]; no `proof` needed.
    /// - otherwise — `proof` must bridge exactly `ancestors -> new_head.length`
    ///   and fold our trusted prefix roots up to `new_head.root`.
    ///
    /// Soundness note on `ancestors`: the value is *authenticated*, not trusted —
    /// an **over-claim** (a larger `ancestors` than the true ancestor) names a
    /// prefix the replica holds but the new history does not, so the fold can't
    /// reach `new_head.root` and is rejected. An **under-claim** (smaller) is a
    /// genuine shorter shared prefix and is accepted; it only costs extra refetch
    /// (the maximal ancestor is the [`MerkleTree::lowest_common_ancestor`] binary
    /// search — a separate, efficiency concern). Either way the replica ends
    /// byte-identical to `new_head`.
    ///
    /// [lowest common ancestor]: merkle::MerkleTree::lowest_common_ancestor
    pub fn verify_reorg(
        &self,
        new_head: &SignedHead,
        ancestors: u64,
        proof: Option<&UpgradeProof>,
    ) -> bool {
        let cur = match &self.head {
            Some(h) => h,
            None => return false, // nothing trusted to reorg away from
        };
        if new_head.fork <= cur.fork {
            return false; // only a strictly higher fork is a reorg to follow
        }
        if !self
            .public
            .verify(&head_message(new_head.fork, new_head.length, &new_head.root), &new_head.sig)
        {
            return false;
        }
        if ancestors > self.tree.len() || ancestors > new_head.length {
            return false;
        }
        // Our own roots at `ancestors` are the trusted anchor — equal to the
        // source's roots there iff [0, ancestors) is genuinely shared.
        let anchor = match self.tree.prefix_roots(ancestors) {
            Some(r) => r,
            None => return false, // missing prefix nodes (not intact)
        };
        if ancestors == new_head.length {
            // Pure truncation: the new head must be exactly our prefix.
            self.tree.prefix_root_hash(ancestors) == Some(new_head.root)
        } else if ancestors == 0 {
            // No prefix to anchor: adopt the signed higher-fork head from scratch
            // (blocks are verified against it on refetch).
            true
        } else {
            match proof {
                Some(p) => {
                    p.old_len == ancestors
                        && p.new_len == new_head.length
                        && p.verify(&anchor, &new_head.root)
                }
                None => false,
            }
        }
    }

    /// Follow a reorg: verify `new_head` is a legitimate higher-fork rewrite that
    /// shares this replica's `[0, ancestors)` prefix (via [`Self::verify_reorg`]),
    /// then drop the divergent suffix, keeping that prefix. Returns `false` and
    /// leaves the replica **untouched** if verification fails.
    ///
    /// On success the replica is at length `ancestors`; fetch the new suffix
    /// `[ancestors, new_head.length)` with [`Self::add_block`] (against
    /// `new_head`) to finish. The shared prefix is preserved, not re-derived (the
    /// surviving nodes already equal the new history's prefix). If
    /// `ancestors == new_head.length` (a pure truncation) there is no suffix and
    /// `new_head` becomes the verified head immediately.
    pub fn reorg(&mut self, new_head: &SignedHead, ancestors: u64, proof: Option<&UpgradeProof>) -> bool {
        if !self.verify_reorg(new_head, ancestors, proof) {
            return false;
        }
        self.tree.truncate(ancestors); // keep the shared prefix (no-op if == len)
        if self.tree.len() == new_head.length && self.tree.root_hash() == new_head.root {
            self.head = Some(new_head.clone()); // pure truncation: reorg complete
        } else {
            self.head = None; // suffix refetch pending — no fully-verified head yet
        }
        true
    }

    /// Decode the value at `index`, or `None`.
    pub fn get(&self, index: u64) -> Result<Option<T>, Error<S::Error>> {
        if index >= self.tree.len() {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(index)
            .map_err(Error::Storage)?
            .ok_or(Error::Corrupt)?;
        Ok(Some(self.codec.decode(&bytes).map_err(Error::Codec)?))
    }
}

