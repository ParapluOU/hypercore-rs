//! `hypercore` — typed, signed, append-only log.
//!
//! A single-writer log generic over a typed payload `T` (encoded via a
//! [`codec::Codec`]). Each appended value is encoded to bytes, stored by index
//! (via [`storage::Store`]), and folded into a [`merkle::MerkleTree`]; the writer
//! then signs the new tree head (`length`, `root`) with its [`identity`] key.
//!
//! That signed head + a per-block Merkle proof let *any* verifier — holding only
//! the author's public key — confirm that a block belongs to this log at a given
//! index, without trusting the sender. Ordering and verification never inspect
//! `T`: it is opaque bytes below the codec.

use std::marker::PhantomData;

use codec::Codec;
use identity::{PublicKey, SecretKey, Sig};
use merkle::{Hash, MerkleTree, Proof, UpgradeProof};
use storage::Store;

/// Domain tag for the head-signable message (separates it from any other thing
/// the author might sign).
const HEAD_DOMAIN: u8 = 0xC0;

fn head_message(fork: u64, length: u64, root: &Hash) -> Vec<u8> {
    let mut m = Vec::with_capacity(1 + 8 + 8 + 32);
    m.push(HEAD_DOMAIN);
    m.extend_from_slice(&fork.to_le_bytes());
    m.extend_from_slice(&length.to_le_bytes());
    m.extend_from_slice(root);
    m
}

/// The author's signature over the current tree head.
///
/// The signed message binds a **`fork` counter** alongside the length and root.
/// The writer bumps `fork` whenever it deliberately rewinds and rewrites history
/// ([`Hypercore::truncate`]); a reader follows the highest fork. This makes a
/// legitimate reorg by the author distinguishable from an *equivocation* — two
/// contradictory histories signed at the **same** fork (see
/// [`conflicting_heads`] / [`ForkProof`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHead {
    pub fork: u64,
    pub length: u64,
    pub root: Hash,
    pub sig: Sig,
}

/// Records the most recent truncation: the log shrank from `from` blocks to `to`
/// blocks (`to < from`). Reset to `None` by the next append/commit — it reflects
/// only the immediately preceding operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncation {
    pub from: u64,
    pub to: u64,
}

/// Errors from a [`Hypercore`], parameterised over the backend's error type.
#[derive(Debug, PartialEq, Eq)]
pub enum Error<SE> {
    Storage(SE),
    Codec(codec::Error),
    /// A stored block was missing where the tree says one exists.
    Corrupt,
}

/// A staged, atomic multi-block append.
///
/// Open one with [`Hypercore::batch`], stage values into it with
/// [`Hypercore::stage`] (the log is **not** touched — staged blocks are only
/// visible through [`Hypercore::batch_get`]), then apply them all at once with
/// [`Hypercore::commit`]: every staged block lands under a **single** signed
/// head, identical to having appended them one by one. Dropping a batch without
/// committing leaves the log unchanged. A batch records the log length it was
/// opened against (`base`); if the log advances past that base before commit,
/// the commit is rejected (stale base) and the batch must be rebuilt.
pub struct Batch<T> {
    base: u64,
    encoded: Vec<Vec<u8>>,
    _t: PhantomData<fn() -> T>,
}

impl<T> Batch<T> {
    /// The log length this batch was opened against.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Number of blocks staged so far.
    pub fn staged(&self) -> usize {
        self.encoded.len()
    }

    /// The batch's logical length (`base` + staged blocks).
    pub fn length(&self) -> u64 {
        self.base + self.encoded.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.encoded.is_empty()
    }
}

/// A typed, signed, append-only log.
pub struct Hypercore<T, C, S> {
    author: SecretKey,
    public: PublicKey,
    codec: C,
    store: S,
    tree: MerkleTree,
    head: Option<SignedHead>,
    fork: u64,
    last_truncation: Option<Truncation>,
    _t: PhantomData<fn() -> T>,
}

impl<T, C: Codec<T>, S: Store> Hypercore<T, C, S> {
    /// Create a fresh, empty log written by `author`.
    pub fn new(author: SecretKey, codec: C, store: S) -> Self {
        let public = author.public();
        Self {
            author,
            public,
            codec,
            store,
            tree: MerkleTree::new(),
            head: None,
            fork: 0,
            last_truncation: None,
            _t: PhantomData,
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    pub fn len(&self) -> u64 {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn head(&self) -> Option<&SignedHead> {
        self.head.as_ref()
    }

    /// The current fork counter (`0` for a log that was never truncated). It
    /// increments by one on each [`truncate`](Self::truncate) and is signed into
    /// every head.
    pub fn fork(&self) -> u64 {
        self.fork
    }

    /// Total byte size of the live blocks (sum of the Merkle root subtree sizes).
    pub fn byte_length(&self) -> u64 {
        self.tree.byte_length()
    }

    /// The truncation performed by the immediately preceding operation, or `None`
    /// if the last operation was an append/commit (which clears it).
    pub fn last_truncation(&self) -> Option<Truncation> {
        self.last_truncation
    }

    /// Re-sign the current tree head under the current `fork`.
    fn resign(&mut self) {
        let length = self.tree.len();
        let root = self.tree.root_hash();
        let sig = self.author.sign(&head_message(self.fork, length, &root));
        self.head = Some(SignedHead { fork: self.fork, length, root, sig });
    }

    /// Append a value; returns its block index. Append-only: indices only grow.
    pub fn append(&mut self, value: &T) -> Result<u64, Error<S::Error>> {
        let bytes = self.codec.encode(value);
        let index = self.tree.len();
        self.store.put(index, &bytes).map_err(Error::Storage)?;
        self.tree.append(&bytes);
        self.last_truncation = None;
        self.resign();
        Ok(index)
    }

    /// Rewind the log to its first `new_len` blocks, discarding every block at
    /// index `>= new_len`, then re-sign a new head under an **incremented `fork`
    /// counter**. Returns the [`Truncation`] performed, or `None` if
    /// `new_len >= len()` (nothing to truncate). Ports hypercore `core.js`'s
    /// "append and truncate" behaviour (length / byteLength / fork progression).
    ///
    /// The new tree is node-for-node the prefix tree, so the new `root` is exactly
    /// the prefix's root — but the bumped `fork` (signed into the head) marks this
    /// as a *deliberate* reorg by the author, so a later truncate-and-rewrite is
    /// not mistaken for an equivocation (which is a fork at the **same** counter;
    /// see [`conflicting_heads`] / [`ForkProof`]).
    ///
    /// Storage is not eagerly reclaimed: blocks at `>= new_len` become logically
    /// unreachable ([`get`](Self::get)/[`block`](Self::block) gate on the length)
    /// and are overwritten when those indices are re-appended. The logical
    /// truncation (tree + head) is a pure in-memory mutation, so it is atomic and
    /// infallible; physical reclamation is a separate concern (upstream
    /// `clear.js`/`purge.js`).
    pub fn truncate(&mut self, new_len: u64) -> Option<Truncation> {
        let from = self.tree.len();
        if !self.tree.truncate(new_len) {
            return None; // new_len >= len: nothing to do
        }
        self.fork += 1;
        let t = Truncation { from, to: new_len };
        self.last_truncation = Some(t);
        self.resign();
        Some(t)
    }

    /// Open an empty [`Batch`] based on the current log length.
    pub fn batch(&self) -> Batch<T> {
        Batch {
            base: self.tree.len(),
            encoded: Vec::new(),
            _t: PhantomData,
        }
    }

    /// Encode and stage `value` into `batch`. The log is untouched; the value is
    /// only visible through [`Self::batch_get`] until [`Self::commit`].
    pub fn stage(&self, batch: &mut Batch<T>, value: &T) {
        batch.encoded.push(self.codec.encode(value));
    }

    /// Read block `index` as seen *through* `batch`: indices below the batch's
    /// base come from the committed log, indices in the staged range from the
    /// batch itself. `None` past the batch's end.
    pub fn batch_get(&self, batch: &Batch<T>, index: u64) -> Result<Option<T>, Error<S::Error>> {
        if index < batch.base {
            return self.get(index);
        }
        match batch.encoded.get((index - batch.base) as usize) {
            Some(enc) => Ok(Some(self.codec.decode(enc).map_err(Error::Codec)?)),
            None => Ok(None),
        }
    }

    /// Atomically apply every staged block under a **single** signed head.
    ///
    /// All-or-nothing: blocks are written to storage first and, on any storage
    /// failure, the partial writes are rolled back and the Merkle tree + signed
    /// head are left **untouched** (the log's logical state never advances on a
    /// failed commit). Returns the new length on success.
    ///
    /// Returns `Ok(None)` — leaving the log unchanged — if the log advanced past
    /// the batch's base since it was opened (a *stale base*): the batch was built
    /// against a head that no longer exists and must be rebuilt. An empty batch
    /// is a successful no-op.
    pub fn commit(&mut self, batch: Batch<T>) -> Result<Option<u64>, Error<S::Error>> {
        if batch.base != self.tree.len() {
            return Ok(None); // stale base: the log moved under the batch
        }
        if batch.encoded.is_empty() {
            return Ok(Some(self.tree.len())); // empty batch: nothing to do
        }

        // Write every staged block first; this is the only fallible step. On
        // failure, undo the writes already made so the tree + head — the log's
        // source of truth — are never advanced on a partial batch.
        let start = self.tree.len();
        let mut written: Vec<u64> = Vec::with_capacity(batch.encoded.len());
        for (i, enc) in batch.encoded.iter().enumerate() {
            let idx = start + i as u64;
            if let Err(e) = self.store.put(idx, enc) {
                for w in &written {
                    let _ = self.store.delete(*w);
                }
                return Err(Error::Storage(e));
            }
            written.push(idx);
        }

        // All blocks stored — now fold them into the tree and sign once.
        for enc in &batch.encoded {
            self.tree.append(enc);
        }
        self.last_truncation = None;
        self.resign();
        Ok(Some(self.tree.len()))
    }

    /// Decode the value at `index`, or `None` if out of range.
    pub fn get(&self, index: u64) -> Result<Option<T>, Error<S::Error>> {
        if index >= self.tree.len() {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(index)
            .map_err(Error::Storage)?
            .ok_or(Error::Corrupt)?;
        let value = self.codec.decode(&bytes).map_err(Error::Codec)?;
        Ok(Some(value))
    }

    /// The raw stored (codec-encoded) bytes of block `index` — i.e. exactly what
    /// the Merkle tree committed to. This is the unit a verifier checks a proof
    /// against (it decodes only *after* verifying).
    pub fn block(&self, index: u64) -> Result<Option<Vec<u8>>, Error<S::Error>> {
        if index >= self.tree.len() {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(index)
            .map_err(Error::Storage)?
            .ok_or(Error::Corrupt)?;
        Ok(Some(bytes))
    }

    /// A Merkle inclusion proof for `index` (pair with [`Self::head`] to make it
    /// independently verifiable).
    pub fn proof(&self, index: u64) -> Option<Proof> {
        self.tree.proof(index)
    }

    /// A length-extension (consistency) proof bridging length `old` to `new` for
    /// this log. Pair it with the *new* [`Self::head`]: a replica that has
    /// already verified up to `old` can confirm the longer head is an honest
    /// append-only extension (the first `old` blocks weren't rewritten) **before**
    /// fetching the new blocks (see [`Replica::verify_upgrade`]). `None` unless
    /// `1 <= old < new <= len`.
    pub fn upgrade_proof(&self, old: u64, new: u64) -> Option<UpgradeProof> {
        self.tree.upgrade_proof(old, new)
    }

    /// Internal-consistency + signature check of our own head.
    pub fn verify_head(&self) -> bool {
        match &self.head {
            None => self.tree.is_empty(),
            Some(h) => {
                h.fork == self.fork
                    && h.length == self.tree.len()
                    && h.root == self.tree.root_hash()
                    && self.public.verify(&head_message(h.fork, h.length, &h.root), &h.sig)
            }
        }
    }
}

/// Verify, from a signed head alone, that `data` is the block at `index` in the
/// log owned by `public`. This is what a replica/verifier uses — it needs only
/// the public key, the signed head, and the block's proof.
pub fn verify_block(public: &PublicKey, head: &SignedHead, index: u64, data: &[u8], proof: &Proof) -> bool {
    public.verify(&head_message(head.fork, head.length, &head.root), &head.sig)
        && proof.block == index
        && proof.verify(data, &head.root)
}

/// Whether two signed heads from the **same author** are a proven *equivocation*
/// (a fork at one fork counter).
///
/// At a fixed `(fork, length)` the head's root is a deterministic pure function
/// of the first `length` blocks. So two heads of **equal fork and equal length
/// but different root**, each carrying the author's signature, are non-repudiable
/// evidence that the author signed two incompatible logs at the same counter —
/// an equivocation. This is the proof-free detector: it needs only the two
/// heads, and it is how a verifier first *notices* a fork — two contradictory
/// heads at one length (upstream's replication-time `'conflict'` at a length;
/// ADR-0019).
///
/// **Different forks are not flagged.** When the author deliberately rewinds and
/// rewrites it bumps the `fork` counter ([`Hypercore::truncate`]); a reader
/// follows the highest fork, so two heads at different forks are a legitimate
/// reorg, not equivocation. Heads of **different lengths** are likewise not
/// flagged here — an honest log of length `L2 > L1` legitimately extends the
/// length-`L1` head — so a same-fork divergence across different lengths must
/// instead be pinned to a shared block index with a [`ForkProof`].
pub fn conflicting_heads(public: &PublicKey, a: &SignedHead, b: &SignedHead) -> bool {
    a.fork == b.fork
        && a.length == b.length
        && a.root != b.root
        && public.verify(&head_message(a.fork, a.length, &a.root), &a.sig)
        && public.verify(&head_message(b.fork, b.length, &b.root), &b.sig)
}

/// Non-repudiable evidence that one author committed **two different blocks at
/// the same index** — a fork proven at a specific block index.
///
/// Each side pairs a signed head with an inclusion proof and the block bytes it
/// commits at `index`. If both sides are signed by the author **at the same fork
/// counter** and prove their block at `index`, and the two blocks differ, the
/// author signed two incompatible histories at one counter (under leaf
/// collision-resistance, different bytes ⇒ a different committed leaf) — an
/// equivocation. Unlike [`conflicting_heads`], this works across heads of
/// **different lengths** (e.g. an equivocation that also truncated one side): it
/// pins the disagreement to one shared index rather than the whole-tree root.
///
/// A divergence across **different** forks is *not* a fork: that is a legitimate
/// reorg by the author ([`Hypercore::truncate`] bumps the counter), which is why
/// `verify` requires both heads to carry the same `fork`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkProof {
    /// The block index at which the two logs disagree.
    pub index: u64,
    pub head_a: SignedHead,
    pub data_a: Vec<u8>,
    pub proof_a: Proof,
    pub head_b: SignedHead,
    pub data_b: Vec<u8>,
    pub proof_b: Proof,
}

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
/// rebuilds an **identical** log — never trusting the sender.
pub struct Replica<T, C, S> {
    public: PublicKey,
    codec: C,
    store: S,
    tree: MerkleTree,
    head: Option<SignedHead>,
    _t: PhantomData<fn() -> T>,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{Bytes, U64};
    use storage::MemoryStore;

    fn author(seed: u8) -> SecretKey {
        SecretKey::from_seed(&[seed; 32])
    }

    #[test]
    fn append_get_roundtrip() {
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(1), Bytes, MemoryStore::new());
        assert!(core.is_empty());
        for i in 0..10u8 {
            let idx = core.append(&vec![i, i + 1, i + 2]).unwrap();
            assert_eq!(idx, i as u64);
        }
        assert_eq!(core.len(), 10);
        for i in 0..10u8 {
            assert_eq!(core.get(i as u64).unwrap(), Some(vec![i, i + 1, i + 2]));
        }
        assert_eq!(core.get(10).unwrap(), None);
    }

    #[test]
    fn head_is_signed_and_consistent() {
        let mut core = Hypercore::<u64, _, _>::new(author(2), U64, MemoryStore::new());
        assert!(core.verify_head()); // empty core
        core.append(&7).unwrap();
        core.append(&8).unwrap();
        assert!(core.verify_head());
        let head = core.head().unwrap();
        assert_eq!(head.length, 2);
    }

    #[test]
    fn blocks_verify_against_signed_head() {
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(3), Bytes, MemoryStore::new());
        let blocks: Vec<Vec<u8>> = (0..7).map(|i| format!("b{i}").into_bytes()).collect();
        for b in &blocks {
            core.append(b).unwrap();
        }
        let head = core.head().unwrap().clone();
        let pk = core.public_key();

        for i in 0..blocks.len() as u64 {
            // The verifier checks the *encoded* (stored) bytes, then decodes.
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(verify_block(&pk, &head, i, &enc, &proof), "honest block verifies");

            // tampered data
            assert!(!verify_block(&pk, &head, i, b"forged-encoded-bytes", &proof));
            // wrong author key
            assert!(!verify_block(&author(99).public(), &head, i, &enc, &proof));
            // wrong index claim (proof.block != claimed index)
            let wrong = (i + 1) % blocks.len() as u64;
            assert!(!verify_block(&pk, &head, wrong, &enc, &proof));
        }
    }

    #[test]
    fn forged_head_does_not_verify_under_real_key() {
        // Author A's head must not verify under author B's key.
        let mut a = Hypercore::<u64, _, _>::new(author(4), U64, MemoryStore::new());
        a.append(&1).unwrap();
        let head = a.head().unwrap();
        let b_pub = author(5).public();
        assert!(!b_pub.verify(&head_message(head.fork, head.length, &head.root), &head.sig));
    }

    #[test]
    fn deterministic_log() {
        // Same author + same appends => identical signed head (ed25519 is deterministic).
        let build = || {
            let mut c = Hypercore::<u64, _, _>::new(author(6), U64, MemoryStore::new());
            c.append(&100).unwrap();
            c.append(&200).unwrap();
            c.head().unwrap().clone()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn replica_ends_byte_identical() {
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(10), Bytes, MemoryStore::new());
        let data: Vec<Vec<u8>> = (0..9).map(|i| format!("blk-{i}").into_bytes()).collect();
        for d in &data {
            src.append(d).unwrap();
        }
        let head = src.head().unwrap().clone();

        let mut rep = Replica::<Vec<u8>, _, _>::new(src.public_key(), Bytes, MemoryStore::new());
        for i in 0..data.len() as u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap(), "verified block accepted");
        }
        assert_eq!(rep.len(), src.len());
        assert_eq!(rep.root_hash(), head.root, "replica root == source signed root");
        assert!(rep.verified_head().is_some());
        for i in 0..data.len() as u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap(), "decoded values match");
        }
    }

    #[test]
    fn replica_rejects_bad_and_out_of_order() {
        let mut src = Hypercore::<u64, _, _>::new(author(11), U64, MemoryStore::new());
        for v in [1u64, 2, 3] {
            src.append(&v).unwrap();
        }
        let head = src.head().unwrap().clone();
        let mut rep = Replica::<u64, _, _>::new(src.public_key(), U64, MemoryStore::new());

        let p1 = src.proof(1).unwrap();
        let e1 = src.block(1).unwrap().unwrap();
        // out of order: index 1 before 0
        assert!(!rep.add_block(&head, 1, &e1, &p1).unwrap());

        // index 0 with tampered bytes
        let p0 = src.proof(0).unwrap();
        assert!(!rep.add_block(&head, 0, b"garbage", &p0).unwrap());
        assert_eq!(rep.len(), 0, "nothing stored on rejection");

        // honest 0 then 1
        let e0 = src.block(0).unwrap().unwrap();
        assert!(rep.add_block(&head, 0, &e0, &p0).unwrap());
        assert!(rep.add_block(&head, 1, &e1, &p1).unwrap());
        assert_eq!(rep.len(), 2);
    }

    // ---- verified length-extension replication (merkle upgrade proof, ADR-0020) ----

    #[test]
    fn replica_upgrades_to_longer_head() {
        // A replica fully replicates a length-5 log, then accepts a *verified*
        // append-only extension to length 9 and fetches only the new blocks.
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(30), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            src.append(&blk(s)).unwrap();
        }
        let head5 = src.head().unwrap().clone();
        let pk = src.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.verified_head(), Some(&head5));

        // The source extends the log.
        for s in ["f", "g", "h", "i"] {
            src.append(&blk(s)).unwrap();
        }
        let head9 = src.head().unwrap().clone();

        // Before fetching, the replica verifies the longer head is an honest
        // extension of what it already trusts — no block data needed.
        let up = src.upgrade_proof(5, 9).unwrap();
        assert!(!up.nodes.is_empty(), "extension supplies new subtree nodes");
        assert!(rep.verify_upgrade(&head9, &up), "honest extension accepted");

        // Then it fetches only the new blocks [5, 9) against the new head and
        // ends byte-identical to the source at length 9.
        for i in 5..9u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head9, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 9);
        assert_eq!(rep.root_hash(), head9.root, "replica root == new signed root");
        assert_eq!(rep.verified_head(), Some(&head9));
        for i in 0..9u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap(), "decoded values match");
        }
    }

    #[test]
    fn replica_rejects_forked_upgrade() {
        // A replica trusting the honest length-5 prefix must reject a longer head
        // from a forking writer (same author) that rewrote an old block: the
        // upgrade proof's new nodes can't fold into the honest roots to reach the
        // forked root. This is the anti-fork guarantee at the replication level.
        let mut honest = Hypercore::<Vec<u8>, _, _>::new(author(31), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            honest.append(&blk(s)).unwrap();
        }
        let head5 = honest.head().unwrap().clone();
        let pk = honest.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = honest.block(i).unwrap().unwrap();
            let proof = honest.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }

        // Forking writer: same author seed, but block 2 ('c' -> 'Z') is rewritten,
        // then the log is extended to length 9.
        let mut forked = Hypercore::<Vec<u8>, _, _>::new(author(31), Bytes, MemoryStore::new());
        for s in ["a", "b", "Z", "d", "e", "f", "g", "h", "i"] {
            forked.append(&blk(s)).unwrap();
        }
        let forked_head9 = forked.head().unwrap().clone();
        let forked_up = forked.upgrade_proof(5, 9).unwrap();

        // The forked head *is* signed by the same author (signature alone passes)...
        assert!(pk.verify(
            &head_message(forked_head9.fork, forked_head9.length, &forked_head9.root),
            &forked_head9.sig
        ));
        // ...but the replica's honest roots can't fold the forked extension up to
        // the forked root, so the upgrade is refused and the replica is untouched.
        assert!(
            !rep.verify_upgrade(&forked_head9, &forked_up),
            "forked extension rejected against the honest prefix"
        );
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.verified_head(), Some(&head5));
    }

    #[test]
    fn verify_upgrade_rejects_malformed_or_tampered() {
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(32), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e", "f", "g"] {
            src.append(&blk(s)).unwrap();
        }
        let pk = src.public_key();

        // The replica replicates only the first 4 blocks (under the length-4 head).
        let mut early = Hypercore::<Vec<u8>, _, _>::new(author(32), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d"] {
            early.append(&blk(s)).unwrap();
        }
        let head4 = early.head().unwrap().clone();
        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..4u64 {
            let enc = early.block(i).unwrap().unwrap();
            let proof = early.proof(i).unwrap();
            assert!(rep.add_block(&head4, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 4);

        let head7 = src.head().unwrap().clone();
        let up = src.upgrade_proof(4, 7).unwrap();
        assert!(!up.nodes.is_empty());
        assert!(rep.verify_upgrade(&head7, &up), "honest baseline accepted");

        // Tampered new-head root: the head signature no longer verifies.
        let mut bad_head = head7.clone();
        bad_head.root[0] ^= 0xff;
        assert!(!rep.verify_upgrade(&bad_head, &up));

        // Tampered proof node: the fold no longer reaches the new root.
        let mut bad_up = up.clone();
        bad_up.nodes[0].hash[0] ^= 0xff;
        assert!(!rep.verify_upgrade(&head7, &bad_up));

        // Proof bridging from the wrong old length (not the replica's length).
        let up_wrong_old = src.upgrade_proof(3, 7).unwrap();
        assert!(!rep.verify_upgrade(&head7, &up_wrong_old), "old_len must equal replica length");

        // Proof whose new_len disagrees with the head's length.
        let up_wrong_new = src.upgrade_proof(4, 6).unwrap();
        assert!(!rep.verify_upgrade(&head7, &up_wrong_new), "new_len must equal head length");

        // A length-7 head signed by a *different* author is refused.
        let other_head = {
            let mut o = Hypercore::<Vec<u8>, _, _>::new(author(33), Bytes, MemoryStore::new());
            for s in ["a", "b", "c", "d", "e", "f", "g"] {
                o.append(&blk(s)).unwrap();
            }
            o.head().unwrap().clone()
        };
        assert!(!rep.verify_upgrade(&other_head, &up), "head signed by another author refused");
    }

    // ---- batch / atomic append (upstream `batch.js` / `atomic.js`) ----

    fn blk(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn batch_stages_without_touching_log() {
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(20), Bytes, MemoryStore::new());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();

        let mut b = core.batch();
        core.stage(&mut b, &blk("de"));
        core.stage(&mut b, &blk("fg"));

        // The log itself is untouched while the batch is open.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);

        // The batch presents a length of 5 and reads both committed and staged.
        assert_eq!(b.base(), 3);
        assert_eq!(b.staged(), 2);
        assert_eq!(b.length(), 5);
        assert_eq!(core.batch_get(&b, 0).unwrap(), Some(blk("a"))); // committed
        assert_eq!(core.batch_get(&b, 2).unwrap(), Some(blk("c"))); // committed
        assert_eq!(core.batch_get(&b, 3).unwrap(), Some(blk("de"))); // staged
        assert_eq!(core.batch_get(&b, 4).unwrap(), Some(blk("fg"))); // staged
        assert_eq!(core.batch_get(&b, 5).unwrap(), None); // past the batch

        // Committing advances the log to the batch length.
        assert_eq!(core.commit(b).unwrap(), Some(5));
        assert_eq!(core.len(), 5);
        assert_eq!(core.get(3).unwrap(), Some(blk("de")));
        assert_eq!(core.get(4).unwrap(), Some(blk("fg")));
    }

    #[test]
    fn commit_equals_sequential_appends() {
        // Same author + same blocks: one committed batch == N single appends,
        // down to the signed head (root, length, signature).
        let all = ["a", "b", "c", "d", "e"];

        let mut seq = Hypercore::<Vec<u8>, _, _>::new(author(21), Bytes, MemoryStore::new());
        for s in all {
            seq.append(&blk(s)).unwrap();
        }

        let mut bat = Hypercore::<Vec<u8>, _, _>::new(author(21), Bytes, MemoryStore::new());
        for s in &all[..3] {
            bat.append(&blk(s)).unwrap();
        }
        let mut b = bat.batch();
        for s in &all[3..] {
            bat.stage(&mut b, &blk(s));
        }
        assert_eq!(bat.commit(b).unwrap(), Some(5));

        assert_eq!(bat.head().unwrap(), seq.head().unwrap(), "single head identical");
        for i in 0..5 {
            assert_eq!(bat.get(i).unwrap(), seq.get(i).unwrap());
        }
    }

    #[test]
    fn committed_batch_blocks_verify_and_replicate() {
        // A batch is invisible to verifiers: every block proves against the one
        // signed head, and a replica rebuilds the core byte-identically.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(22), Bytes, MemoryStore::new());
        core.append(&blk("g0")).unwrap();
        let mut b = core.batch();
        for s in ["g1", "g2", "g3"] {
            core.stage(&mut b, &blk(s));
        }
        core.commit(b).unwrap();

        let head = core.head().unwrap().clone();
        let pk = core.public_key();
        for i in 0..core.len() {
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(verify_block(&pk, &head, i, &enc, &proof));
        }

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..core.len() {
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.root_hash(), head.root);
        assert_eq!(rep.len(), core.len());
    }

    #[test]
    fn stale_base_batch_is_rejected() {
        // Open a batch, then append to the log directly: the batch's base is now
        // stale, so commit is refused and the direct append stands.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(23), Bytes, MemoryStore::new());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let mut b = core.batch(); // base = 3
        core.stage(&mut b, &blk("from-batch"));

        core.append(&blk("from-core")).unwrap(); // log now length 4
        let head_after_core = core.head().unwrap().clone();

        assert_eq!(core.commit(b).unwrap(), None, "stale-base batch rejected");
        assert_eq!(core.len(), 4, "log unchanged by the rejected commit");
        assert_eq!(core.get(3).unwrap(), Some(blk("from-core")));
        assert_eq!(core.head().unwrap(), &head_after_core);
    }

    #[test]
    fn empty_batch_is_noop() {
        let mut core = Hypercore::<u64, _, _>::new(author(24), U64, MemoryStore::new());
        core.append(&1).unwrap();
        let head_before = core.head().unwrap().clone();
        let b = core.batch();
        assert!(b.is_empty());
        assert_eq!(core.commit(b).unwrap(), Some(1));
        assert_eq!(core.len(), 1);
        assert_eq!(core.head().unwrap(), &head_before);
    }

    #[test]
    fn dropped_batch_leaves_log_unchanged() {
        let mut core = Hypercore::<u64, _, _>::new(author(25), U64, MemoryStore::new());
        core.append(&10).unwrap();
        let head_before = core.head().unwrap().clone();
        {
            let mut b = core.batch();
            core.stage(&mut b, &20);
            core.stage(&mut b, &30);
            // b is dropped here without commit
        }
        assert_eq!(core.len(), 1);
        assert_eq!(core.head().unwrap(), &head_before);
    }

    /// A store that injects a failure on the `put` at a chosen key, to prove
    /// commit atomicity. Otherwise an in-memory map.
    #[derive(Default)]
    struct FaultyStore {
        inner: MemoryStore,
        fail_at: Option<u64>,
    }
    impl Store for FaultyStore {
        type Error = &'static str;
        fn put(&mut self, key: u64, value: &[u8]) -> Result<(), &'static str> {
            if self.fail_at == Some(key) {
                return Err("injected put failure");
            }
            self.inner.put(key, value).unwrap();
            Ok(())
        }
        fn get(&self, key: u64) -> Result<Option<Vec<u8>>, &'static str> {
            Ok(self.inner.get(key).unwrap())
        }
        fn delete(&mut self, key: u64) -> Result<(), &'static str> {
            self.inner.delete(key).unwrap();
            Ok(())
        }
        fn len(&self) -> Result<u64, &'static str> {
            Ok(self.inner.len().unwrap())
        }
    }

    #[test]
    fn failed_commit_is_atomic() {
        // Append a, b, c cleanly, then arm a storage failure at index 4 (the 2nd
        // staged block of a 3-block batch). The commit must fail, roll back its
        // partial write at index 3, and leave the log's logical state untouched.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(26), Bytes, FaultyStore::default());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();
        core.store.fail_at = Some(4);

        let mut b = core.batch(); // base = 3, blocks at 3,4,5
        for s in ["d", "e", "f"] {
            core.stage(&mut b, &blk(s));
        }
        assert_eq!(core.commit(b), Err(Error::Storage("injected put failure")));

        // Logical state unchanged: length, head, and reads all intact; the
        // rolled-back partial write at index 3 is gone.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);
        assert_eq!(core.store.get(3).unwrap(), None, "partial write rolled back");
        assert_eq!(core.store.len().unwrap(), 3, "no orphan blocks left behind");

        // Recovery: clear the fault and the batch commits cleanly to the right state.
        core.store.fail_at = None;
        let mut b2 = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b2, &blk(s));
        }
        assert_eq!(core.commit(b2).unwrap(), Some(6));
        assert_eq!(core.get(5).unwrap(), Some(blk("f")));
    }

    // ---- fork detection (upstream `conflicts.js`, L1) ----

    type ByteCore = Hypercore<Vec<u8>, Bytes, MemoryStore>;

    fn core_with(seed: u8, blocks: &[&str]) -> ByteCore {
        let mut c = Hypercore::<Vec<u8>, _, _>::new(author(seed), Bytes, MemoryStore::new());
        for b in blocks {
            c.append(&blk(b)).unwrap();
        }
        c
    }

    /// Assemble a [`ForkProof`] at `index` from two cores (each supplies its own
    /// signed head, block bytes, and inclusion proof at that index).
    fn fork_proof_at(index: u64, a: &ByteCore, b: &ByteCore) -> ForkProof {
        ForkProof {
            index,
            head_a: a.head().unwrap().clone(),
            data_a: a.block(index).unwrap().unwrap(),
            proof_a: a.proof(index).unwrap(),
            head_b: b.head().unwrap().clone(),
            data_b: b.block(index).unwrap().unwrap(),
            proof_b: b.proof(index).unwrap(),
        }
    }

    #[test]
    fn forking_writer_is_detected() {
        // Same author (seed 40), two logs sharing the prefix [a,b,c,d] but
        // diverging at index 4 — mirrors conflicts.js (a=[..e], c=[..f]).
        let a = core_with(40, &["a", "b", "c", "d", "e"]);
        let c = core_with(40, &["a", "b", "c", "d", "f"]);
        let pk = a.public_key();
        assert_eq!(pk, c.public_key(), "same seed => same author key");

        // Both heads are length 5 with different roots: a proof-free fork.
        let ha = a.head().unwrap();
        let hc = c.head().unwrap();
        assert_eq!(ha.length, hc.length);
        assert_ne!(ha.root, hc.root);
        assert!(conflicting_heads(&pk, ha, hc), "same-length conflicting heads = fork");

        // And the per-index fork proof at the divergence (block 4: 'e' vs 'f').
        let fork = fork_proof_at(4, &a, &c);
        assert!(fork.verify(&pk), "per-index fork proof verifies");
    }

    #[test]
    fn honest_extension_is_not_a_fork() {
        // A length-5 log and an honest length-7 continuation by the same author:
        // shared blocks agree, so neither detector flags a fork.
        let short = core_with(41, &["a", "b", "c", "d", "e"]);
        let long = core_with(41, &["a", "b", "c", "d", "e", "f", "g"]);
        let pk = short.public_key();

        // Different lengths => conflicting_heads never flags (it judges equal lengths only).
        assert!(!conflicting_heads(&pk, short.head().unwrap(), long.head().unwrap()));

        // A "fork proof" over any shared index has identical data on both sides => not a fork.
        for i in 0..5u64 {
            let not_fork = fork_proof_at(i, &short, &long);
            assert_eq!(not_fork.data_a, not_fork.data_b, "shared block agrees at {i}");
            assert!(!not_fork.verify(&pk), "consistent block is not a fork (i={i})");
        }
    }

    #[test]
    fn identical_logs_do_not_conflict() {
        // Same author, same appends => identical deterministic heads => no conflict.
        let a = core_with(42, &["x", "y", "z"]);
        let b = core_with(42, &["x", "y", "z"]);
        let pk = a.public_key();
        assert_eq!(a.head().unwrap(), b.head().unwrap());
        assert!(!conflicting_heads(&pk, a.head().unwrap(), b.head().unwrap()));
    }

    #[test]
    fn fork_proof_rejects_forgery() {
        // Diverge at index 1 (block 'b' vs 'Z') in a 4-block log, so the block-1
        // inclusion proof carries interior siblings to tamper with.
        let a = core_with(43, &["a", "b", "c", "d"]);
        let c = core_with(43, &["a", "Z", "c", "d"]);
        let pk = a.public_key();
        let good = fork_proof_at(1, &a, &c);
        assert!(good.verify(&pk));
        assert!(!good.proof_a.siblings.is_empty(), "block 1 proof has siblings");

        // Wrong author key: neither head is signed by it.
        assert!(!good.verify(&author(99).public()));

        // Tampered data on one side: its proof no longer matches the head root.
        let mut bad_data = good.clone();
        bad_data.data_a = blk("zzz");
        assert!(!bad_data.verify(&pk));

        // Tampered proof sibling on one side.
        let mut bad_proof = good.clone();
        bad_proof.proof_a.siblings[0].hash[0] ^= 0xff;
        assert!(!bad_proof.verify(&pk));

        // Tampered head: mutating the signed root invalidates the head's signature.
        let mut bad_head = good.clone();
        bad_head.head_a.root[0] ^= 0xff;
        assert!(!bad_head.verify(&pk));

        // Mismatched index claim: the proofs are for block 1, not 0.
        let mut wrong_index = good.clone();
        wrong_index.index = 0;
        assert!(!wrong_index.verify(&pk));
    }

    #[test]
    fn different_authors_are_not_a_fork() {
        // Two independent authors with differing length-3 logs are NOT a fork —
        // a fork is one author signing two histories, not two authors disagreeing.
        let a = core_with(44, &["a", "b", "c"]);
        let b = core_with(45, &["a", "b", "d"]); // different author and content
        assert_ne!(a.public_key(), b.public_key());

        // Neither key validates the other's head, so no same-length conflict.
        assert!(!conflicting_heads(&a.public_key(), a.head().unwrap(), b.head().unwrap()));
        assert!(!conflicting_heads(&b.public_key(), a.head().unwrap(), b.head().unwrap()));

        // A fork proof built across the two cores fails under either key — one
        // side is always signed by the other author.
        let cross = fork_proof_at(2, &a, &b);
        assert!(!cross.verify(&a.public_key()));
        assert!(!cross.verify(&b.public_key()));
    }

    // ---- truncate + fork counter (upstream `core.js` "append and truncate") ----

    #[test]
    fn append_and_truncate_tracks_fork_and_byte_length() {
        // Ports core.js "core - append and truncate": each truncate bumps the
        // fork counter and shrinks byteLength; lastTruncation records {from,to}
        // and the next append clears it. (byteLength is the *encoded* prefix size
        // — the bytes the tree commits — so we compare to a fresh prefix core
        // rather than raw payload lengths.)
        let blen = |items: &[&str]| -> u64 {
            let mut c = Hypercore::<Vec<u8>, _, _>::new(author(50), Bytes, MemoryStore::new());
            for s in items {
                c.append(&blk(s)).unwrap();
            }
            c.byte_length()
        };

        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(50), Bytes, MemoryStore::new());
        for s in ["hello", "world", "fo", "ooo"] {
            core.append(&blk(s)).unwrap();
        }
        assert_eq!(core.len(), 4);
        assert_eq!(core.byte_length(), blen(&["hello", "world", "fo", "ooo"]));
        assert_eq!(core.fork(), 0);
        assert_eq!(core.last_truncation(), None);
        assert!(core.verify_head());

        assert_eq!(core.truncate(3), Some(Truncation { from: 4, to: 3 }));
        assert_eq!(core.last_truncation(), Some(Truncation { from: 4, to: 3 }));
        assert_eq!(core.len(), 3);
        assert_eq!(core.byte_length(), blen(&["hello", "world", "fo"]));
        assert_eq!(core.fork(), 1);
        assert!(core.verify_head(), "head consistent after truncate");

        for s in ["a", "b", "c", "d"] {
            core.append(&blk(s)).unwrap();
        }
        assert_eq!(core.last_truncation(), None, "append clears lastTruncation");
        assert_eq!(core.len(), 7);

        assert_eq!(core.truncate(3), Some(Truncation { from: 7, to: 3 }));
        assert_eq!(core.fork(), 2);
        assert_eq!(core.len(), 3);
        assert_eq!(core.byte_length(), blen(&["hello", "world", "fo"]));

        assert_eq!(core.truncate(2), Some(Truncation { from: 3, to: 2 }));
        assert_eq!(core.fork(), 3);
        assert_eq!(core.len(), 2);
        assert_eq!(core.byte_length(), blen(&["hello", "world"]));

        // append-then-truncate cycles, each bumping fork by exactly one — mirrors
        // the upstream fork progression up to 7.
        let mut expect_fork = 3u64;
        for _ in 0..4 {
            core.append(&blk("a")).unwrap();
            assert_eq!(core.last_truncation(), None);
            assert_eq!(core.truncate(2), Some(Truncation { from: 3, to: 2 }));
            expect_fork += 1;
            assert_eq!(core.fork(), expect_fork);
            assert_eq!(core.len(), 2);
            assert_eq!(core.byte_length(), blen(&["hello", "world"]));
        }
        assert_eq!(core.fork(), 7, "seven truncations => fork 7");

        // no-op truncates change nothing.
        assert_eq!(core.truncate(2), None, "truncate to current length is a no-op");
        assert_eq!(core.truncate(9), None, "truncate beyond length is a no-op");
        assert_eq!(core.fork(), 7);
        assert!(core.verify_head());
        // surviving blocks are still readable; the truncated tail is gone.
        assert_eq!(core.get(0).unwrap(), Some(blk("hello")));
        assert_eq!(core.get(1).unwrap(), Some(blk("world")));
        assert_eq!(core.get(2).unwrap(), None);
    }

    #[test]
    fn truncated_head_matches_fresh_prefix() {
        // After truncating to L the tree root equals a fresh log of the first L
        // blocks (root is a pure function of the prefix); the heads differ only
        // by the fork counter (and thus the signature).
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(51), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            core.append(&blk(s)).unwrap();
        }
        core.truncate(3);

        let mut fresh = Hypercore::<Vec<u8>, _, _>::new(author(51), Bytes, MemoryStore::new());
        for s in ["a", "b", "c"] {
            fresh.append(&blk(s)).unwrap();
        }

        let th = core.head().unwrap();
        let fh = fresh.head().unwrap();
        assert_eq!(th.length, fh.length);
        assert_eq!(th.root, fh.root, "truncated root == fresh prefix root");
        assert_eq!(core.fork(), 1);
        assert_eq!(fresh.fork(), 0);
        assert_ne!(th, fh, "heads differ by the fork counter");
        for i in 0..3u64 {
            assert_eq!(core.get(i).unwrap(), fresh.get(i).unwrap());
        }
        assert_eq!(core.get(3).unwrap(), None, "the truncated block is gone");
    }

    #[test]
    fn replica_replicates_truncated_log() {
        // A replica replicating a truncated-and-rewritten source ends
        // byte-identical — the fork counter is carried through the signed head
        // (every block verifies against a head whose message binds the fork).
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(52), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            src.append(&blk(s)).unwrap();
        }
        src.truncate(2);
        src.append(&blk("Z")).unwrap(); // [a,b,Z], fork 1
        let head = src.head().unwrap().clone();
        assert_eq!(head.fork, 1);
        assert_eq!(src.len(), 3);

        let mut rep = Replica::<Vec<u8>, _, _>::new(src.public_key(), Bytes, MemoryStore::new());
        for i in 0..src.len() {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap(), "block {i} accepted");
        }
        assert_eq!(rep.len(), src.len());
        assert_eq!(rep.root_hash(), head.root);
        assert_eq!(rep.verified_head(), Some(&head));
        for i in 0..src.len() {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap());
        }
    }

    #[test]
    fn reorg_with_bumped_fork_is_not_equivocation() {
        // A writer that truncates and rewrites under a *new* fork is doing a
        // legitimate reorg, not equivocation: same-length heads at *different*
        // forks are not flagged, and a cross-fork ForkProof does not verify.
        let original = core_with(53, &["a", "b", "c", "d", "e"]); // fork 0

        let mut reorged = core_with(53, &["a", "b", "c", "d", "e"]);
        reorged.truncate(3);
        reorged.append(&blk("X")).unwrap();
        reorged.append(&blk("Y")).unwrap(); // [a,b,c,X,Y], fork 1
        let pk = original.public_key();
        assert_eq!(pk, reorged.public_key());

        let ho = original.head().unwrap();
        let hr = reorged.head().unwrap();
        assert_eq!(ho.length, hr.length);
        assert_ne!(ho.root, hr.root);
        assert_eq!(ho.fork, 0);
        assert_eq!(hr.fork, 1);
        assert!(
            !conflicting_heads(&pk, ho, hr),
            "different forks => legitimate reorg, not a conflict"
        );

        // The per-index disagreement at block 3 ('d' vs 'X') is across forks.
        let across = fork_proof_at(3, &original, &reorged);
        assert_ne!(across.data_a, across.data_b);
        assert!(!across.verify(&pk), "cross-fork divergence is a reorg, not equivocation");

        // Positive control: a second writer reaching the same rewritten content
        // at the *same* fork (0) IS a provable equivocation.
        let equivocating = core_with(53, &["a", "b", "c", "X", "Y"]); // fork 0
        let he = equivocating.head().unwrap();
        assert_eq!(he.fork, 0);
        assert_ne!(ho.root, he.root);
        assert!(
            conflicting_heads(&pk, ho, he),
            "same fork, different root => equivocation"
        );
        let fork = fork_proof_at(3, &original, &equivocating);
        assert!(fork.verify(&pk), "same-fork divergence is a provable fork");
    }
}
