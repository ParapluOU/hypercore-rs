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
use merkle::{Hash, MerkleTree, Proof};
use storage::Store;

/// Domain tag for the head-signable message (separates it from any other thing
/// the author might sign).
const HEAD_DOMAIN: u8 = 0xC0;

fn head_message(length: u64, root: &Hash) -> Vec<u8> {
    let mut m = Vec::with_capacity(1 + 8 + 32);
    m.push(HEAD_DOMAIN);
    m.extend_from_slice(&length.to_le_bytes());
    m.extend_from_slice(root);
    m
}

/// The author's signature over the current tree head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHead {
    pub length: u64,
    pub root: Hash,
    pub sig: Sig,
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

    /// Append a value; returns its block index. Append-only: indices only grow.
    pub fn append(&mut self, value: &T) -> Result<u64, Error<S::Error>> {
        let bytes = self.codec.encode(value);
        let index = self.tree.len();
        self.store.put(index, &bytes).map_err(Error::Storage)?;
        self.tree.append(&bytes);

        let length = self.tree.len();
        let root = self.tree.root_hash();
        let sig = self.author.sign(&head_message(length, &root));
        self.head = Some(SignedHead { length, root, sig });
        Ok(index)
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
        let length = self.tree.len();
        let root = self.tree.root_hash();
        let sig = self.author.sign(&head_message(length, &root));
        self.head = Some(SignedHead { length, root, sig });
        Ok(Some(length))
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

    /// Internal-consistency + signature check of our own head.
    pub fn verify_head(&self) -> bool {
        match &self.head {
            None => self.tree.is_empty(),
            Some(h) => {
                h.length == self.tree.len()
                    && h.root == self.tree.root_hash()
                    && self.public.verify(&head_message(h.length, &h.root), &h.sig)
            }
        }
    }
}

/// Verify, from a signed head alone, that `data` is the block at `index` in the
/// log owned by `public`. This is what a replica/verifier uses — it needs only
/// the public key, the signed head, and the block's proof.
pub fn verify_block(public: &PublicKey, head: &SignedHead, index: u64, data: &[u8], proof: &Proof) -> bool {
    public.verify(&head_message(head.length, &head.root), &head.sig)
        && proof.block == index
        && proof.verify(data, &head.root)
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
        assert!(!b_pub.verify(&head_message(head.length, &head.root), &head.sig));
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
}
