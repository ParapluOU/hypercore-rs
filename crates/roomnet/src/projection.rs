//! The domain fold: turns the ordered op stream into a materialized state.

use autobase::NodeId;

/// A rolling projection — the domain `apply` that folds the autobase-ordered op
/// stream into a materialized state.
///
/// **Portable + deterministic**: no async, no I/O. One [`apply`](Self::apply) per
/// mutation, delivered in autobase order. The same impl runs on the node (folding
/// the *finalized* order → TerminusDB) and in a browser client (folding the *live*
/// order → render); only the sink differs. Determinism in `(node, payload)` and the
/// prior state is what makes two replicas of the same DAG converge to the same view.
pub trait Projection {
    /// The materialized view this projection maintains.
    type State;
    /// A domain apply error (a malformed payload, an invariant violation, …).
    type Error: core::fmt::Debug;

    /// Apply one ordered mutation. `payload` is the opaque domain op (the
    /// [`Entry::payload`](crate::entry::Entry) bytes); `node` is its stable causal
    /// id. Must be a pure function of the prior state and `(node, payload)`.
    fn apply(&mut self, node: NodeId, payload: &[u8]) -> Result<(), Self::Error>;

    /// The current materialized snapshot.
    fn snapshot(&self) -> &Self::State;

    /// Reset the state to a prior `checkpoint`. Used to recompute the live
    /// (optimistic) view on top of the finalized checkpoint when the unconfirmed
    /// tail reshuffles, and for boot replay.
    fn reset_to(&mut self, checkpoint: &Self::State);
}
