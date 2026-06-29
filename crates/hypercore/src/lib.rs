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
}
