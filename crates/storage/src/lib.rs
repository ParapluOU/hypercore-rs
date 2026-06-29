//! `storage` — pluggable random-access byte storage for logs.
//!
//! Abstraction over where a log's bytes live, with swappable backends:
//! in-memory, native disk, and — for the browser/wasm host — `localStorage` /
//! IndexedDB, so a user's hypercores persist locally with no server required.
//!
//! Content-blind: stores opaque bytes, never the typed payload.
//!
//! Scaffold only — no types yet.
