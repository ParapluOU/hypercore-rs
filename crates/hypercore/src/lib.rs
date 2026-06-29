//! `hypercore` — typed, signed, append-only log.
//!
//! The core primitive: a single-writer, hash-linked, append-only log generic
//! over a typed payload `T` (encoded via the `codec` crate), with a
//! BLAKE3/Merkle structure (`merkle`) for verified random access and range
//! proofs, and per-entry signing (`identity`). Ordering and verification stay
//! content-blind — they never inspect `T`.
//!
//! Scaffold only — no types yet.
