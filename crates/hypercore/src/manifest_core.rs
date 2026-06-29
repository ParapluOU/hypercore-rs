//! Manifest-authorized append-only log — the L1 wiring of the multi-signer
//! [`identity::Manifest`] (iter 32, ADR-0035) into a hypercore-style append /
//! verify / replicate path.
//!
//! Where the single-key [`Hypercore`](crate::Hypercore) signs each head with one
//! author key, a [`ManifestCore`] is governed by a [`Manifest`] **quorum of
//! signers**:
//!
//! - the log's **key** is the content-addressed [`Manifest::hash`] — so *who may
//!   sign* cannot change without changing the identity (the manifest-hash-into-key
//!   binding deferred by ADR-0035);
//! - a head `(length, root)` is authorized only by **at least `quorum` distinct
//!   valid signatures** over it, each binding the manifest hash;
//! - a [`ManifestReplica`] — holding only the **public** [`Manifest`] (the policy)
//!   — verifies blocks against it and rebuilds a byte-identical log, trusting no
//!   sender.
//!
//! Single-signer is the special case: [`Manifest::single`] reproduces a plain
//! one-author core, so `ManifestCore::key()` then equals what a single-key
//! `Hypercore`'s identity would be (`Manifest::single(pk).hash()`).
//!
//! Scope (clean-room, ADR-0001; the L1 of `manifest.js`'s `multisig - append`
//! shape, minus sessions/networking): append / get / block / proof / verify +
//! verify-only replication. The fork counter + `truncate` / batch / snapshot /
//! streams already live on the single-key [`Hypercore`](crate::Hypercore); a head
//! here is at a single (implicit fork-0) history, so it carries no fork field.
//! **Unifying** the two cores — reframing `Hypercore` as a `Manifest::single`
//! special case (replacing its single-key `SignedHead` in place) — is a
//! mechanical follow-up, deliberately deferred to avoid churning the single-key
//! core's large test surface in one step (ADR-0036).

use std::marker::PhantomData;

use codec::Codec;
use identity::{Manifest, PartialSig, SecretKey};
use merkle::{Hash, MerkleTree, Proof};
use storage::Store;

use crate::Error;

/// A head of a [`ManifestCore`] authorized by a signer **quorum**.
///
/// `sigs` are the partial signatures collected over the head `(length, root)`;
/// the head is authorized iff at least the manifest's `quorum` of them are from
/// **distinct** declared signers and all are valid (see [`Manifest::verify`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestHead {
    pub length: u64,
    pub root: Hash,
    pub sigs: Vec<PartialSig>,
}

/// A typed, append-only log whose authority is a [`Manifest`] quorum of signers.
///
/// The log's identity is the content-addressed [`Manifest::hash`]
/// ([`key`](Self::key)). A [`ManifestCore`] holds whatever subset of the
/// manifest's signer secrets it can sign with locally; [`append`](Self::append)
/// collects a partial signature from each into the new head. A head verifies iff
/// those reach the manifest's quorum — so a core holding fewer than `quorum`
/// secrets produces an *unauthorized* head it cannot ratify alone.
pub struct ManifestCore<T, C, S> {
    manifest: Manifest,
    signers: Vec<SecretKey>,
    codec: C,
    store: S,
    tree: MerkleTree,
    head: Option<ManifestHead>,
    _t: PhantomData<fn() -> T>,
}

impl<T, C: Codec<T>, S: Store> ManifestCore<T, C, S> {
    /// Create a fresh, empty manifest-authorized log. `signers` are the local
    /// secret keys this core can sign with (each that is a declared
    /// [`Manifest`] signer contributes to every head).
    pub fn new(manifest: Manifest, signers: Vec<SecretKey>, codec: C, store: S) -> Self {
        Self {
            manifest,
            signers,
            codec,
            store,
            tree: MerkleTree::new(),
            head: None,
            _t: PhantomData,
        }
    }

    /// The log's content-addressed identity — the [`Manifest::hash`]. For a
    /// [`Manifest::single`] this is exactly a plain one-author core's key.
    pub fn key(&self) -> [u8; 32] {
        self.manifest.hash()
    }

    /// The signing policy governing this log.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn len(&self) -> u64 {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn head(&self) -> Option<&ManifestHead> {
        self.head.as_ref()
    }

    /// Re-sign the current tree head with every local signer that is a declared
    /// manifest signer. The partial signatures are collected in **signer-index
    /// order** (deterministic), so two cores with the same signer set produce
    /// byte-identical heads.
    fn resign(&mut self) {
        let length = self.tree.len();
        let root = self.tree.root_hash();
        let mut sigs: Vec<PartialSig> = self
            .signers
            .iter()
            .filter_map(|s| self.manifest.sign(s, length, &root))
            .collect();
        sigs.sort_by_key(|ps| ps.signer);
        sigs.dedup_by_key(|ps| ps.signer);
        self.head = Some(ManifestHead { length, root, sigs });
    }

    /// Append a value; returns its block index. The new head is signed by the
    /// local signer quorum (which only *verifies* if it reaches the manifest's
    /// quorum — see [`verify_head`](Self::verify_head)).
    pub fn append(&mut self, value: &T) -> Result<u64, Error<S::Error>> {
        let bytes = self.codec.encode(value);
        let index = self.tree.len();
        self.store.put(index, &bytes).map_err(Error::Storage)?;
        self.tree.append(&bytes);
        self.resign();
        Ok(index)
    }

    /// Decode the value at `index`, or `None` if out of range. (This focused
    /// core has no [`clear`](crate::Hypercore::clear); every in-range block is
    /// present.)
    pub fn get(&self, index: u64) -> Result<Option<T>, Error<S::Error>> {
        match self.block(index)? {
            Some(bytes) => Ok(Some(self.codec.decode(&bytes).map_err(Error::Codec)?)),
            None => Ok(None),
        }
    }

    /// The raw stored (codec-encoded) bytes of block `index` — the unit a
    /// verifier checks a proof against. `None` past the length.
    pub fn block(&self, index: u64) -> Result<Option<Vec<u8>>, Error<S::Error>> {
        if index >= self.tree.len() {
            return Ok(None);
        }
        let bytes = self.store.get(index).map_err(Error::Storage)?.ok_or(Error::Corrupt)?;
        Ok(Some(bytes))
    }

    /// A Merkle inclusion proof for `index` (pair with [`head`](Self::head) to
    /// make it independently verifiable via [`verify_manifest_block`]).
    pub fn proof(&self, index: u64) -> Option<Proof> {
        self.tree.proof(index)
    }

    /// Whether our own head is internally consistent **and authorized by the
    /// manifest quorum**. An empty core is consistent iff it has no head. A core
    /// holding fewer than `quorum` of the manifest's signer secrets fails here —
    /// its head is signed by too few distinct signers to be ratified.
    pub fn verify_head(&self) -> bool {
        match &self.head {
            None => self.tree.is_empty(),
            Some(h) => {
                h.length == self.tree.len()
                    && h.root == self.tree.root_hash()
                    && self.manifest.verify(h.length, &h.root, &h.sigs)
            }
        }
    }
}

/// Verify, from a public [`Manifest`] and a signed head alone, that `data` is the
/// block at `index` in the log governed by `manifest`. The multi-signer analogue
/// of [`verify_block`](crate::verify_block): the head must meet the manifest's
/// signer **quorum**, and the proof must place `data` at `index` under the head's
/// root.
pub fn verify_manifest_block(
    manifest: &Manifest,
    head: &ManifestHead,
    index: u64,
    data: &[u8],
    proof: &Proof,
) -> bool {
    manifest.verify(head.length, &head.root, &head.sigs)
        && proof.block == index
        && proof.verify(data, &head.root)
}

/// A verify-only replica of a [`ManifestCore`]. It holds no secret key — only the
/// **public** [`Manifest`] (the signing policy) — accepts blocks accompanied by a
/// proof against a quorum-authorized head, verifies each, and rebuilds an
/// **identical** log, never trusting the sender.
pub struct ManifestReplica<T, C, S> {
    manifest: Manifest,
    codec: C,
    store: S,
    tree: MerkleTree,
    head: Option<ManifestHead>,
    _t: PhantomData<fn() -> T>,
}

impl<T, C: Codec<T>, S: Store> ManifestReplica<T, C, S> {
    pub fn new(manifest: Manifest, codec: C, store: S) -> Self {
        Self {
            manifest,
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

    /// The quorum-authorized head we have fully replicated up to (if any).
    pub fn verified_head(&self) -> Option<&ManifestHead> {
        self.head.as_ref()
    }

    /// Verify the next block (`index` must equal the current length) against the
    /// quorum-authorized `head`, then append it. Returns whether it was accepted;
    /// a rejected block is **not** stored.
    pub fn add_block(
        &mut self,
        head: &ManifestHead,
        index: u64,
        enc: &[u8],
        proof: &Proof,
    ) -> Result<bool, Error<S::Error>> {
        if index != self.tree.len() {
            return Ok(false); // must apply in order
        }
        if !verify_manifest_block(&self.manifest, head, index, enc, proof) {
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
        let bytes = self.store.get(index).map_err(Error::Storage)?.ok_or(Error::Corrupt)?;
        Ok(Some(self.codec.decode(&bytes).map_err(Error::Codec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::Bytes;
    use identity::{Manifest, ManifestSigner, PartialSig, SecretKey};
    use storage::MemoryStore;

    fn key(b: u8) -> SecretKey {
        SecretKey::from_seed(&[b; 32])
    }

    /// A single-signer manifest core is the plain one-author core: its `key()`
    /// is `Manifest::single(pk).hash()` (deterministic, author-derived), it
    /// round-trips append/get, its head verifies, and a holder of the manifest
    /// authenticates each block.
    #[test]
    fn single_signer_core_is_the_plain_identity() {
        let sk = key(1);
        let m = Manifest::single(sk.public());
        let mut core =
            ManifestCore::<Vec<u8>, _, _>::new(m.clone(), vec![sk.clone()], Bytes, MemoryStore::new());

        // key() is the content-addressed manifest hash, deterministic and
        // distinct for a different author.
        assert_eq!(core.key(), Manifest::single(sk.public()).hash());
        assert_eq!(core.key(), m.hash());
        assert_ne!(core.key(), Manifest::single(key(2).public()).hash());

        for v in [b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()] {
            core.append(&v).unwrap();
        }
        assert_eq!(core.len(), 3);
        assert!(core.verify_head());
        assert_eq!(core.get(1).unwrap(), Some(b"bb".to_vec()));
        assert_eq!(core.get(3).unwrap(), None);

        // each block authenticates against the manifest + head.
        let head = core.head().unwrap().clone();
        for i in 0..3 {
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(verify_manifest_block(&m, &head, i, &enc, &proof));
            // wrong index claim / forged bytes rejected.
            assert!(!verify_manifest_block(&m, &head, i, b"forged", &proof));
            let wrong = (i + 1) % 3;
            assert!(!verify_manifest_block(&m, &head, wrong, &enc, &proof));
        }
    }

    /// A 2-of-3 manifest core holding all three secrets self-assembles a quorum:
    /// every head verifies, and a replica holding only the **public** manifest
    /// rebuilds the log byte-identically.
    #[test]
    fn multi_signer_quorum_authorizes_and_replicates() {
        let (a, b, c) = (key(10), key(11), key(12));
        let m = Manifest::new(
            2,
            vec![
                ManifestSigner::new(a.public()),
                ManifestSigner::new(b.public()),
                ManifestSigner::new(c.public()),
            ],
        )
        .unwrap();

        let mut core = ManifestCore::<Vec<u8>, _, _>::new(
            m.clone(),
            vec![a.clone(), b.clone(), c.clone()],
            Bytes,
            MemoryStore::new(),
        );
        let blocks = [b"one".to_vec(), b"two".to_vec(), b"three".to_vec(), b"four".to_vec()];
        for v in &blocks {
            core.append(v).unwrap();
        }
        assert!(core.verify_head(), "a >=quorum head verifies");
        // the head carries (at least) the quorum of distinct signatures.
        assert!(core.head().unwrap().sigs.len() >= m.quorum());

        // A replica with only the public manifest replicates byte-identically.
        let head = core.head().unwrap().clone();
        let mut rep = ManifestReplica::<Vec<u8>, _, _>::new(m.clone(), Bytes, MemoryStore::new());
        for i in 0..core.len() {
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap(), "honest block accepted");
        }
        assert_eq!(rep.len(), core.len());
        assert_eq!(rep.root_hash(), head.root);
        assert_eq!(rep.verified_head(), Some(&head));
        for (i, v) in blocks.iter().enumerate() {
            assert_eq!(rep.get(i as u64).unwrap().as_ref(), Some(v));
        }
    }

    /// A core holding fewer than `quorum` of the signer secrets produces an
    /// **unauthorized** head: it cannot ratify it alone, and a replica refuses
    /// its blocks (the quorum gate, the whole point of the manifest).
    #[test]
    fn head_below_quorum_is_unauthorized() {
        let (a, b, c) = (key(20), key(21), key(22));
        let m = Manifest::new(
            2,
            vec![
                ManifestSigner::new(a.public()),
                ManifestSigner::new(b.public()),
                ManifestSigner::new(c.public()),
            ],
        )
        .unwrap();

        // holds only ONE of the three secrets -> 1 signature, quorum is 2.
        let mut core =
            ManifestCore::<Vec<u8>, _, _>::new(m.clone(), vec![a.clone()], Bytes, MemoryStore::new());
        core.append(&b"x".to_vec()).unwrap();
        assert_eq!(core.head().unwrap().sigs.len(), 1);
        assert!(!core.verify_head(), "one signature does not meet quorum 2");

        // a replica refuses the under-quorum block; nothing stored.
        let head = core.head().unwrap().clone();
        let enc = core.block(0).unwrap().unwrap();
        let proof = core.proof(0).unwrap();
        let mut rep = ManifestReplica::<Vec<u8>, _, _>::new(m, Bytes, MemoryStore::new());
        assert!(!rep.add_block(&head, 0, &enc, &proof).unwrap());
        assert_eq!(rep.len(), 0);
    }

    /// The manifest is the content-addressed authority: two cores with the
    /// **same** author secret but a different signing policy (different signer
    /// namespace) have different keys, and a head authorized under one policy
    /// does **not** verify under the other — signatures bind the manifest hash,
    /// so *who may sign* cannot change without changing the identity.
    #[test]
    fn manifest_is_the_content_addressed_authority() {
        let sk = key(30);
        let m_a = Manifest::new(1, vec![ManifestSigner::with_namespace(sk.public(), [1u8; 32])]).unwrap();
        let m_b = Manifest::new(1, vec![ManifestSigner::with_namespace(sk.public(), [2u8; 32])]).unwrap();
        assert_ne!(m_a.hash(), m_b.hash(), "different policy => different identity");

        let mut core_a =
            ManifestCore::<Vec<u8>, _, _>::new(m_a.clone(), vec![sk.clone()], Bytes, MemoryStore::new());
        core_a.append(&b"v".to_vec()).unwrap();
        assert_ne!(core_a.key(), m_b.hash());
        assert!(core_a.verify_head());

        // core_a's head is authorized under m_a but NOT under m_b.
        let head = core_a.head().unwrap().clone();
        let enc = core_a.block(0).unwrap().unwrap();
        let proof = core_a.proof(0).unwrap();
        assert!(verify_manifest_block(&m_a, &head, 0, &enc, &proof));
        assert!(!verify_manifest_block(&m_b, &head, 0, &enc, &proof));
    }

    /// A replica rejects every degenerate multisig head — a tampered signature,
    /// a non-signer's signature, and the same signer twice (distinctness) —
    /// leaving nothing stored.
    #[test]
    fn forged_and_non_distinct_heads_are_rejected() {
        let (a, b, c) = (key(40), key(41), key(42));
        let m = Manifest::new(
            2,
            vec![
                ManifestSigner::new(a.public()),
                ManifestSigner::new(b.public()),
                ManifestSigner::new(c.public()),
            ],
        )
        .unwrap();
        let mut core = ManifestCore::<Vec<u8>, _, _>::new(
            m.clone(),
            vec![a.clone(), b.clone(), c.clone()],
            Bytes,
            MemoryStore::new(),
        );
        core.append(&b"data".to_vec()).unwrap();
        let good = core.head().unwrap().clone();
        let enc = core.block(0).unwrap().unwrap();
        let proof = core.proof(0).unwrap();
        // sanity: the honest head verifies.
        assert!(verify_manifest_block(&m, &good, 0, &enc, &proof));

        let mut rep = ManifestReplica::<Vec<u8>, _, _>::new(m.clone(), Bytes, MemoryStore::new());

        // (1) a tampered signature byte on one of the quorum sigs.
        let mut tampered = good.clone();
        let mut raw = tampered.sigs[0].sig.to_bytes();
        raw[7] ^= 0xff;
        tampered.sigs[0] = PartialSig { signer: tampered.sigs[0].signer, sig: identity::Sig::from_bytes(&raw) };
        assert!(!rep.add_block(&tampered, 0, &enc, &proof).unwrap());

        // (2) the same signer twice is not two distinct signatures.
        let dup = ManifestHead { length: good.length, root: good.root, sigs: vec![good.sigs[0], good.sigs[0]] };
        assert!(!rep.add_block(&dup, 0, &enc, &proof).unwrap());

        // (3) a signature from a key that is not a declared signer, placed in a
        // real signer's slot, fails to verify under that signer's key.
        let outsider = key(99);
        let forged_sig = outsider.sign(&m.signable(good.length, &good.root));
        let forged = ManifestHead {
            length: good.length,
            root: good.root,
            sigs: vec![PartialSig { signer: 0, sig: forged_sig }, good.sigs[1]],
        };
        assert!(!rep.add_block(&forged, 0, &enc, &proof).unwrap());

        assert_eq!(rep.len(), 0, "no rejected block was stored");
        // the honest head still replicates cleanly after the rejections.
        assert!(rep.add_block(&good, 0, &enc, &proof).unwrap());
        assert_eq!(rep.len(), 1);
    }
}
