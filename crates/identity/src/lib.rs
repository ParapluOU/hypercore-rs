//! `identity` — ed25519 author keys, signing & verification.
//!
//! Author identity for log entries. A [`PublicKey`] is the author id (and maps
//! cleanly onto an Iroh `NodeId`); every entry is signed by its author's
//! [`SecretKey`], so causal references point at verifiable, signed blocks rather
//! than forgeable plaintext ids.
//!
//! Keys are derived from a 32-byte seed ([`SecretKey::from_seed`]) — no RNG
//! dependency, so this builds for `wasm32` and stays deterministic in tests. The
//! host supplies entropy for real keys.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// An ed25519 signing key. Keep secret.
#[derive(Clone)]
pub struct SecretKey(SigningKey);

/// An ed25519 verifying key — the public author identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublicKey(VerifyingKey);

/// A 64-byte ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sig(Signature);

impl SecretKey {
    /// Derive a key deterministically from a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        SecretKey(SigningKey::from_bytes(seed))
    }

    /// This key's public author identity.
    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Sign a message. ed25519 signatures are deterministic.
    pub fn sign(&self, msg: &[u8]) -> Sig {
        Sig(self.0.sign(msg))
    }
}

impl PublicKey {
    /// 32-byte wire form.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Parse a 32-byte public key (rejects non-canonical / off-curve points).
    pub fn from_bytes(bytes: &[u8; 32]) -> Option<Self> {
        VerifyingKey::from_bytes(bytes).ok().map(PublicKey)
    }

    /// Verify `sig` over `msg` under this key. `false` on any mismatch.
    pub fn verify(&self, msg: &[u8], sig: &Sig) -> bool {
        self.0.verify(msg, &sig.0).is_ok()
    }
}

impl Sig {
    pub fn to_bytes(&self) -> [u8; 64] {
        self.0.to_bytes()
    }

    pub fn from_bytes(bytes: &[u8; 64]) -> Self {
        Sig(Signature::from_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> SecretKey {
        SecretKey::from_seed(&[b; 32])
    }

    #[test]
    fn sign_then_verify() {
        let sk = key(1);
        let pk = sk.public();
        let msg = b"hypercore entry";
        let sig = sk.sign(msg);
        assert!(pk.verify(msg, &sig), "honest signature must verify");
    }

    #[test]
    fn rejects_forgery() {
        let sk = key(1);
        let pk = sk.public();
        let sig = sk.sign(b"original");

        // wrong message
        assert!(!pk.verify(b"tampered", &sig));

        // wrong signer's public key
        let other = key(2).public();
        assert!(!other.verify(b"original", &sig));

        // tampered signature bytes
        let mut raw = sig.to_bytes();
        raw[0] ^= 0xff;
        assert!(!pk.verify(b"original", &Sig::from_bytes(&raw)));
    }

    #[test]
    fn deterministic_keys_and_sigs() {
        assert_eq!(key(7).public(), key(7).public(), "seed determines key");
        assert_ne!(key(7).public(), key(8).public());
        // ed25519 signing is deterministic
        assert_eq!(key(7).sign(b"x"), key(7).sign(b"x"));
    }

    #[test]
    fn public_key_byte_roundtrip() {
        let pk = key(3).public();
        let parsed = PublicKey::from_bytes(&pk.to_bytes()).expect("valid key");
        assert_eq!(pk, parsed);
    }
}
