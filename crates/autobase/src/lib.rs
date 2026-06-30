//! `autobase` — multi-writer causal linearizer.
//!
//! Combines multiple `hypercore`s (one per writer) into a single deterministic,
//! eventually-consistent order. Each node carries a *clock*: a set of causal
//! references to other writers' heads. The linearization is a topological order
//! of that DAG, tie-broken deterministically among concurrent nodes by **lowest
//! writer key first, then lowest seq** — never by a timestamp or any self-reported
//! scalar (that is what makes forged "append times" a non-attack), and never by
//! inspecting a payload (this layer is domain-agnostic, L1).
//!
//! On top of the linearizer (causal order + deterministic tiebreak) this layer
//! adds **indexer quorum** and a **finalized prefix**: by counting how many
//! indexers reference a node — and, recursively, how many reference *that*
//! quorum — we determine when a node is permanently confirmed and can no longer
//! be reordered (DESIGN.md "Consistency"). Votes are counted by causal
//! reachability only — never a timestamp, never a payload peek.
//!
//! ## Clean-room divergence
//!
//! Upstream (`reference/js/autobase/lib/topolist.js`) keeps an *incremental*
//! sorted tip and shuffles each arriving node into place (`moveDown`/`moveUp`),
//! tracking `undo`/`shared` so a streaming view can be patched cheaply. We instead
//! recompute the whole order with a **priority Kahn topological sort** — at each
//! step emit the causally-ready node with the smallest [`NodeId`]. This is simpler,
//! is *manifestly* independent of arrival order (so determinism is obvious), and
//! reproduces the canonical linearizations in `reference/js/autobase/DESIGN.md`.
//! See ADR-0014.

use std::collections::{BTreeMap, BTreeSet};

/// A writer's stable identity: the 32-byte author public key
/// (`identity::PublicKey::to_bytes()`, equivalently an Iroh `NodeId`).
///
/// The linearizer treats it purely as an opaque, totally-ordered label — it never
/// verifies crypto, decodes a payload, or reads a clock. Byte-lexicographic order
/// on the key *is* the tiebreak ("lowest key wins").
pub type WriterKey = [u8; 32];

/// Address of one node in the causal DAG: writer [`WriterKey`] plus `seq`, the
/// 0-based position in that writer's append-only log.
///
/// `Ord` compares `key` first, then `seq` — exactly the deterministic tiebreak the
/// linearizer applies to concurrent nodes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId {
    pub key: WriterKey,
    pub seq: u64,
}

impl NodeId {
    pub fn new(key: WriterKey, seq: u64) -> Self {
        Self { key, seq }
    }
}

/// A **vector clock** over the DAG: for each writer, the number of that writer's
/// nodes in some node's causal history (its `length` — highest `seq` seen + 1;
/// absent ⇒ `0`).
///
/// This is the reachability form of upstream `consensus.js`'s per-node `clock`,
/// which every confirmation predicate (`_strictlyNewer`, `_acks`, `confirms`, …)
/// iterates. Because writers are append-only and gap-free, "saw `length` nodes of
/// `key`" is exactly "sees `NodeId{key, length - 1}`" — so the clock is a faithful
/// restatement of [`Linearizer::sees`], never a self-reported scalar. Built by
/// [`Linearizer::clock`]. The L1 substrate for the faithful `consensus.js` port
/// (ADR-0015); does not itself read a payload or a timestamp.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    lengths: BTreeMap<WriterKey, u64>,
}

impl Clock {
    /// Nodes of `key` seen — the clock's `length` for `key` (`0` if unseen).
    pub fn get(&self, key: &WriterKey) -> u64 {
        self.lengths.get(key).copied().unwrap_or(0)
    }

    /// Has this clock seen at least `length` nodes of `key` (i.e. `NodeId{key,
    /// length - 1}`)? `length == 0` is vacuously false — matches upstream
    /// `clock.includes`.
    pub fn includes(&self, key: &WriterKey, length: u64) -> bool {
        length > 0 && self.get(key) >= length
    }

    /// Number of writers this clock has seen at least one node from.
    pub fn writers(&self) -> usize {
        self.lengths.len()
    }

    /// `(key, length)` pairs in ascending key order (upstream iterates `clock`).
    pub fn iter(&self) -> impl Iterator<Item = (WriterKey, u64)> + '_ {
        self.lengths.iter().map(|(k, v)| (*k, *v))
    }

    /// Raise `key`'s length to at least `length` (monotone join).
    fn raise(&mut self, key: WriterKey, length: u64) {
        let e = self.lengths.entry(key).or_insert(0);
        if length > *e {
            *e = length;
        }
    }
}

/// How an indexer's view relates to a confirmation target (upstream `consensus.js`
/// `UNSEEN`/`NEWER`/`ACKED`): `Unseen` = no strictly-newer node of this indexer;
/// `Newer` = it has a strictly-newer node but it does not yet see a majority of the
/// acks; `Acked` = it has a strictly-newer node that sees a majority of the acks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Confirm {
    Unseen,
    Newer,
    Acked,
}

/// Why a node could not be added. Each variant is a violation of **causal
/// delivery** — the L1 guarantee that a node arrives only after everything it
/// references. With it upheld, the DAG is always acyclic and causally closed, so
/// [`Linearizer::order`] is always well-defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// A node already exists at this id (writers are append-only; ids are unique).
    Duplicate(NodeId),
    /// `seq` skips ahead of the writer's log — its predecessor must arrive first.
    Gap {
        writer: WriterKey,
        expected: u64,
        got: u64,
    },
    /// A referenced causal head has not been delivered yet.
    MissingHead(NodeId),
}

/// A growing causal DAG of nodes from many writers, linearizable on demand.
///
/// Add nodes with [`add`](Self::add) (respecting causal delivery); call
/// [`order`](Self::order) for the current deterministic linearization. `order` is a
/// pure function of the node set, so two replicas holding the same DAG produce the
/// same order regardless of arrival order — the convergence property.
#[derive(Default)]
pub struct Linearizer {
    /// node -> its direct causal dependencies (referenced heads ∪ same-writer predecessor).
    deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// node -> the nodes that directly depend on it (reverse edges, for Kahn).
    dependents: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// next expected `seq` per writer — enforces append-only, gap-free delivery.
    next_seq: BTreeMap<WriterKey, u64>,
    /// The **indexers**: writers whose references count as votes toward quorums
    /// (DESIGN.md "Indexing Writer"). `None` ⇒ quorum disabled (pure ordering); a
    /// node from a writer outside this set is still ordered but never votes
    /// (a non-indexing writer).
    indexers: Option<BTreeSet<WriterKey>>,
}

mod linearizer;
