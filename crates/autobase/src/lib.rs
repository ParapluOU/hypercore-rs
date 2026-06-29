//! `autobase` — multi-writer causal linearizer.
//!
//! Combines multiple `hypercore`s (one per writer) into a single deterministic,
//! eventually-consistent order: causal DAG ordering (each node carries a clock
//! of references to other writers' heads), a deterministic tiebreak among
//! concurrent nodes, and indexer-quorum finalization. Never ordered by
//! timestamps.
//!
//! Scaffold only — no types yet.
