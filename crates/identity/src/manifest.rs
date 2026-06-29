//! Multi-signer manifest verifier — the L1 distillation of upstream
//! `reference/js/hypercore/lib/{verifier,multisig,caps}.js` (exercised by
//! `test/manifest.js`).
//!
//! Upstream a log's authority is a **manifest**: a quorum-of-signers signing
//! policy that is *hashed into the log's key*, so who may sign cannot change
//! without changing the identity. A [`Manifest`] here is the same idea, stripped
//! to its L1 essence (no wire/disk format, no multi-version compat, no
//! `allowPatch` cross-length signing):
//!
//! - a [`Signer`] is an ed25519 [`PublicKey`] + a 32-byte `namespace` (entropy
//!   distinguishing a signer's *role* within the manifest);
//! - the manifest commits to its `quorum` + ordered signers (+ an optional
//!   [`Prologue`]); [`Manifest::hash`] is the **content-addressed identity** (the
//!   would-be log key);
//! - to authorize a head `(length, tree_hash)` a signer signs
//!   [`Manifest::signable`] — domain-tagged bytes binding the *manifest hash*
//!   (so a signature is valid only under this exact policy);
//! - [`Manifest::verify`] accepts iff at least `quorum` **distinct** signers
//!   produced a valid signature — the multisig quorum rule — with a
//!   [`Prologue`] prefix self-authorizing without any signature.
//!
//! Clean-room (ADR-0001): not byte/wire compatible. Only ed25519 signers exist
//! at the type level, so upstream's "unsupported curve" rejection is structural
//! (you cannot construct a non-ed25519 [`Signer`]). The single-signer manifest
//! ([`Manifest::single`]) is the identity used by a plain one-author core, so its
//! hash is that core's key — the content-addressed binding upstream's
//! `Hypercore.key(publicKey)` provides.

use crate::{PublicKey, SecretKey, Sig};

/// Domain tag for the content-addressed manifest hash (the log identity).
const MANIFEST_DOMAIN: u8 = 0xA0;
/// Domain tag for the multisig signable (the bytes each signer signs).
const SIGNABLE_DOMAIN: u8 = 0xA1;

/// Default per-signer namespace (a signer declared without explicit entropy).
///
/// Clean-room: any fixed 32-byte constant works — it is folded into the manifest
/// hash, never compared to upstream's `caps.DEFAULT_NAMESPACE`.
pub const DEFAULT_NAMESPACE: [u8; 32] = [0u8; 32];

/// One signer in a [`Manifest`]: an ed25519 public key plus a 32-byte namespace.
///
/// The namespace is *role entropy*: two manifests with identical public keys but
/// different namespaces are different policies (different [`Manifest::hash`]). In
/// this L1 form (the modern, non-compat path) every signer signs under the
/// manifest hash, so the namespace's effect is purely through that commitment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signer {
    pub public_key: PublicKey,
    pub namespace: [u8; 32],
}

impl Signer {
    /// A signer with the [`DEFAULT_NAMESPACE`].
    pub fn new(public_key: PublicKey) -> Self {
        Signer { public_key, namespace: DEFAULT_NAMESPACE }
    }

    /// A signer with an explicit namespace.
    pub fn with_namespace(public_key: PublicKey, namespace: [u8; 32]) -> Self {
        Signer { public_key, namespace }
    }
}

/// A content-addressed commitment to a prefix: a head at `length` whose Merkle
/// `hash` is `hash` is **self-authorizing** — no signature is required for it.
///
/// This is the manifest-level form of the hypercore prologue (ADR-0034): a
/// manifest carrying a prologue accepts the committed prefix on content alone,
/// and requires the signer quorum only past it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prologue {
    pub length: u64,
    pub hash: [u8; 32],
}

/// One signer's contribution to a multisig: which `signer` (index into
/// [`Manifest::signers`]) signed, and the `sig`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartialSig {
    pub signer: usize,
    pub sig: Sig,
}

/// A multi-signer signing policy. **Content-addressed**: [`Manifest::hash`] is
/// the log identity, so the policy cannot change without changing the key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Manifest {
    quorum: usize,
    signers: Vec<Signer>,
    prologue: Option<Prologue>,
}

impl Manifest {
    /// The majority-default quorum for `n` signers: `(n >> 1) + 1`.
    pub fn default_quorum(n: usize) -> usize {
        (n >> 1) + 1
    }

    /// A signing manifest of `signers` requiring `quorum` distinct signatures.
    ///
    /// `None` if the configuration is invalid: no signers, `quorum == 0`, or
    /// `quorum > signers.len()` (upstream's `createManifest` rejections — a
    /// quorum cannot exceed, nor be vacuous over, the signer set). The
    /// signer-less / quorum-0 "static signer" is [`Manifest::static_signer`].
    pub fn new(quorum: usize, signers: Vec<Signer>) -> Option<Self> {
        if signers.is_empty() || quorum == 0 || quorum > signers.len() {
            return None;
        }
        Some(Manifest { quorum, signers, prologue: None })
    }

    /// The default single-signer manifest (quorum 1, one [`DEFAULT_NAMESPACE`]
    /// signer) — the identity of a plain one-author core. Its [`Manifest::hash`]
    /// is that core's key.
    pub fn single(public_key: PublicKey) -> Self {
        Manifest { quorum: 1, signers: vec![Signer::new(public_key)], prologue: None }
    }

    /// A "static signer": no signers, quorum 0, only a self-authorizing
    /// [`Prologue`]. It verifies *only* the committed prefix (nothing past it can
    /// ever be signed) — upstream's `quorum: 0, signers: []` + prologue manifest.
    pub fn static_signer(prologue: Prologue) -> Self {
        Manifest { quorum: 0, signers: Vec::new(), prologue: Some(prologue) }
    }

    /// Attach a [`Prologue`] floor: the committed prefix is self-authorizing, the
    /// signer quorum governs everything past it.
    pub fn with_prologue(mut self, prologue: Prologue) -> Self {
        self.prologue = Some(prologue);
        self
    }

    pub fn quorum(&self) -> usize {
        self.quorum
    }

    pub fn signers(&self) -> &[Signer] {
        &self.signers
    }

    pub fn prologue(&self) -> Option<Prologue> {
        self.prologue
    }

    /// The content-addressed manifest hash — the log identity. Domain-separated
    /// and length-bound over `quorum`, the ordered signers (key + namespace), and
    /// the prologue, so any policy change yields a different key.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(&[MANIFEST_DOMAIN]);
        h.update(&(self.quorum as u64).to_le_bytes());
        h.update(&(self.signers.len() as u64).to_le_bytes());
        for s in &self.signers {
            h.update(&s.public_key.to_bytes());
            h.update(&s.namespace);
        }
        match self.prologue {
            Some(p) => {
                h.update(&[1]);
                h.update(&p.length.to_le_bytes());
                h.update(&p.hash);
            }
            None => {
                h.update(&[0]);
            }
        };
        *h.finalize().as_bytes()
    }

    /// The bytes a signer signs to authorize the head `(length, tree_hash)`.
    ///
    /// Binds the **manifest hash** (so a signature is valid only under this exact
    /// policy — the modern `ctx = manifestHash` path) alongside the length and
    /// root, domain-separated. Mirrors upstream `caps.treeSignable`.
    pub fn signable(&self, length: u64, tree_hash: &[u8; 32]) -> Vec<u8> {
        let mut m = Vec::with_capacity(1 + 32 + 8 + 32);
        m.push(SIGNABLE_DOMAIN);
        m.extend_from_slice(&self.hash());
        m.extend_from_slice(&length.to_le_bytes());
        m.extend_from_slice(tree_hash);
        m
    }

    /// Produce this `secret`'s partial signature over the head `(length,
    /// tree_hash)`, if its public key is a declared signer. `None` otherwise
    /// (upstream `sign` throws "public key is not a declared signer").
    pub fn sign(&self, secret: &SecretKey, length: u64, tree_hash: &[u8; 32]) -> Option<PartialSig> {
        let public = secret.public();
        let signer = self.signers.iter().position(|s| s.public_key == public)?;
        let sig = secret.sign(&self.signable(length, tree_hash));
        Some(PartialSig { signer, sig })
    }

    /// Verify authorization for the head `(length, tree_hash)`.
    ///
    /// A [`Prologue`] prefix (`length <= prologue.length`) is accepted on content
    /// alone — iff it is *exactly* the committed length with the committed hash —
    /// requiring no signature. Otherwise the **multisig quorum** rule: at least
    /// `quorum` signatures, each from a *distinct* in-range signer, each valid
    /// over [`Manifest::signable`]. Any out-of-range or repeated signer, or any
    /// invalid signature among those supplied, rejects the whole multisig.
    pub fn verify(&self, length: u64, tree_hash: &[u8; 32], sigs: &[PartialSig]) -> bool {
        if let Some(p) = self.prologue {
            if length <= p.length {
                return length == p.length && tree_hash == &p.hash;
            }
        }

        if self.quorum == 0 || sigs.len() < self.quorum {
            return false;
        }

        let msg = self.signable(length, tree_hash);
        let mut seen = vec![false; self.signers.len()];
        let mut valid = 0usize;
        for ps in sigs {
            if ps.signer >= self.signers.len() || seen[ps.signer] {
                return false;
            }
            seen[ps.signer] = true;
            if !self.signers[ps.signer].public_key.verify(&msg, &ps.sig) {
                return false;
            }
            valid += 1;
        }
        valid >= self.quorum
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SecretKey, Sig};

    fn key(b: u8) -> SecretKey {
        SecretKey::from_seed(&[b; 32])
    }

    // upstream "create verifier - static signer": a prologue-only manifest accepts
    // the committed prefix on content alone and nothing else.
    #[test]
    fn static_signer_authorizes_only_the_committed_prefix() {
        let tree_hash = [1u8; 32];
        let m = Manifest::static_signer(Prologue { length: 1, hash: tree_hash });

        // exactly the committed prefix verifies with no signature.
        assert!(m.verify(1, &tree_hash, &[]));

        // a longer head is past the prologue and there are no signers → never.
        assert!(!m.verify(2, &tree_hash, &[]));

        // the committed length with a different hash is rejected.
        let mut bad = tree_hash;
        bad[0] ^= 0xff;
        assert!(!m.verify(1, &bad, &[]));
    }

    // upstream "create verifier - single signer": one signer, quorum 1; sign then
    // verify; a tampered signature is rejected.
    #[test]
    fn single_signer_sign_then_verify() {
        let sk = key(1);
        let m = Manifest::single(sk.public());
        assert_eq!(m.quorum(), 1);

        let length = 1u64;
        let tree_hash = [7u8; 32];
        let ps = m.sign(&sk, length, &tree_hash).expect("declared signer");
        assert_eq!(ps.signer, 0);
        assert!(m.verify(length, &tree_hash, &[ps]));

        // tampered signature byte → reject.
        let mut raw = ps.sig.to_bytes();
        raw[5] ^= 0xff;
        let tampered = PartialSig { signer: 0, sig: Sig::from_bytes(&raw) };
        assert!(!m.verify(length, &tree_hash, &[tampered]));

        // a key that is not a declared signer cannot sign.
        assert!(m.sign(&key(2), length, &tree_hash).is_none());

        // a partial signed over a *different* head does not verify here.
        let other = m.sign(&sk, length, &[8u8; 32]).unwrap();
        assert!(!m.verify(length, &tree_hash, &[other]));
    }

    // upstream "create verifier - multi signer": two signers, quorum 2; a quorum
    // of distinct valid signatures passes, and every degenerate multisig fails.
    #[test]
    fn multi_signer_quorum_and_distinctness() {
        let a = key(1);
        let b = key(2);
        let m = Manifest::new(
            2,
            vec![
                Signer::with_namespace(a.public(), [2u8; 32]),
                Signer::with_namespace(b.public(), [3u8; 32]),
            ],
        )
        .expect("valid manifest");
        assert_eq!(m.quorum(), 2);

        let length = 1u64;
        let tree_hash = [1u8; 32];
        let asig = m.sign(&a, length, &tree_hash).unwrap();
        let bsig = m.sign(&b, length, &tree_hash).unwrap();
        assert_eq!(asig.signer, 0);
        assert_eq!(bsig.signer, 1);

        // two distinct, valid signatures → accept.
        assert!(m.verify(length, &tree_hash, &[asig, bsig]));

        // signer 1's slot carrying a's signature → a's sig fails under b's key.
        let bad = PartialSig { signer: 1, sig: asig.sig };
        assert!(!m.verify(length, &tree_hash, &[asig, bad]));

        // the same signer twice → not two distinct signers.
        let dup = PartialSig { signer: 0, sig: asig.sig };
        assert!(!m.verify(length, &tree_hash, &[asig, dup]));

        // a single signature when quorum is 2 → too few.
        assert!(!m.verify(length, &tree_hash, &[asig]));

        // an out-of-range signer index → reject.
        let oob = PartialSig { signer: 9, sig: asig.sig };
        assert!(!m.verify(length, &tree_hash, &[asig, oob]));
    }

    // upstream "create verifier - defaults" + `Hypercore.key(manifest) ==
    // Hypercore.key(publicKey)`: the single-signer manifest is the content-
    // addressed identity, deterministic and key-derived; different keys /
    // namespaces / quorums are different identities.
    #[test]
    fn manifest_hash_is_content_addressed_identity() {
        let pk = key(3).public();

        // `single` is exactly a one-signer, default-namespace manifest.
        let built = Manifest::new(1, vec![Signer::new(pk)]).unwrap();
        assert_eq!(Manifest::single(pk).hash(), built.hash());
        assert_eq!(Manifest::single(pk).hash(), Manifest::single(pk).hash());

        // a different author → a different identity.
        assert_ne!(Manifest::single(pk).hash(), Manifest::single(key(4).public()).hash());

        // the namespace is part of the commitment.
        let ns_a = Manifest::new(1, vec![Signer::with_namespace(pk, [1u8; 32])]).unwrap();
        let ns_b = Manifest::new(1, vec![Signer::with_namespace(pk, [2u8; 32])]).unwrap();
        assert_ne!(ns_a.hash(), ns_b.hash());

        // the quorum is part of the commitment.
        let q1 = Manifest::new(1, vec![Signer::new(pk), Signer::new(key(5).public())]).unwrap();
        let q2 = Manifest::new(2, vec![Signer::new(pk), Signer::new(key(5).public())]).unwrap();
        assert_ne!(q1.hash(), q2.hash());

        // a signature is bound to the manifest: it does not verify under a manifest
        // with a different policy (same key, different namespace).
        let sk = key(3);
        let ps = ns_a.sign(&sk, 1, &[9u8; 32]).unwrap();
        assert!(ns_a.verify(1, &[9u8; 32], &[ps]));
        assert!(!ns_b.verify(1, &[9u8; 32], &[PartialSig { signer: 0, sig: ps.sig }]));
    }

    // invalid manifests are rejected at construction (upstream `createManifest`
    // throwing); a non-ed25519 signer is structurally impossible to build.
    #[test]
    fn invalid_manifests_are_rejected() {
        let a = key(1).public();
        // quorum exceeds the signer set.
        assert!(Manifest::new(2, vec![Signer::new(a)]).is_none());
        // a vacuous quorum.
        assert!(Manifest::new(0, vec![Signer::new(a)]).is_none());
        // no signers (the static-signer case must go through `static_signer`).
        assert!(Manifest::new(1, Vec::new()).is_none());
        // a 3-signer majority default.
        assert_eq!(Manifest::default_quorum(3), 2);
        assert_eq!(Manifest::default_quorum(4), 3);
    }

    // a manifest may carry both a prologue floor *and* signers: the committed
    // prefix self-authorizes, the quorum governs everything past it (upstream
    // `verify`'s prologue short-circuit then `_verifyMulti`).
    #[test]
    fn prologue_floor_with_signers_past_it() {
        let sk = key(1);
        let committed = [4u8; 32];
        let m = Manifest::single(sk.public()).with_prologue(Prologue { length: 2, hash: committed });

        // within the prologue: self-authorizing on content, no signature.
        assert!(m.verify(2, &committed, &[]));
        assert!(!m.verify(2, &[0u8; 32], &[])); // wrong committed hash
        assert!(!m.verify(1, &committed, &[])); // not exactly the committed length

        // past the prologue: the signer quorum is required.
        let head = [5u8; 32];
        assert!(!m.verify(3, &head, &[])); // no signature
        let ps = m.sign(&sk, 3, &head).unwrap();
        assert!(m.verify(3, &head, &[ps]));
    }

    // more than `quorum` distinct valid signatures is still valid (a 2-of-3 with
    // all three supplied), and supplying fewer than quorum distinct-valid fails.
    #[test]
    fn extra_signatures_beyond_quorum_are_accepted() {
        let a = key(1);
        let b = key(2);
        let c = key(3);
        let m = Manifest::new(
            2,
            vec![Signer::new(a.public()), Signer::new(b.public()), Signer::new(c.public())],
        )
        .unwrap();
        assert_eq!(m.quorum(), 2);

        let length = 1u64;
        let h = [1u8; 32];
        let sa = m.sign(&a, length, &h).unwrap();
        let sb = m.sign(&b, length, &h).unwrap();
        let sc = m.sign(&c, length, &h).unwrap();

        // all three distinct valid → accept (count 3 >= quorum 2).
        assert!(m.verify(length, &h, &[sa, sb, sc]));
        // exactly the quorum → accept.
        assert!(m.verify(length, &h, &[sa, sc]));
        // one valid signature, quorum 2 → reject.
        assert!(!m.verify(length, &h, &[sb]));
    }
}
