//! `identity` — ed25519 author keys, signing & verification.
//!
//! Author identity for log entries. Maps cleanly onto an Iroh `NodeId`. Every
//! entry is signed by its author, so causal references point at verifiable,
//! signed blocks rather than forgeable plaintext ids.
//!
//! Scaffold only — no types yet.
