//! The authority seam: writers whose **verdicts** decide which entries take effect.
//!
//! By default a room folds every entry at its position in the order, and the only
//! refusal is the domain [`Projection`](crate::Projection) returning `Err` — a
//! decision every replica can reach alone, from the state and the op.
//!
//! Some refusals cannot be reached that way: the check needs something only one
//! party holds (state outside the room, or a rule only it may judge). A room with an
//! [`Authority`] is **gated** for exactly that case. An entry written by anyone but
//! an authority then takes no effect at its own position (unless the authority says
//! its kind [needs no verdict](Authority::needs_verdict)); it waits until an
//! authority writes a [`Verdict`] naming it, and takes effect — or is dropped — at
//! the verdict's position. Because the verdict is itself an entry in the log, every
//! replica reaches the same decision by folding the same order, with nothing but the
//! log: no side channel, and nothing the replica has to re-derive.
//!
//! roomnet stays domain-agnostic here as everywhere: it never reads a payload. The
//! application says who the authorities are and which of their payloads are
//! verdicts, through this trait.

use autobase::{NodeId, WriterKey};

/// What an authority decided about entries written before it.
///
/// An entry named in both lists is accepted. An id that is not waiting where the
/// verdict stands in the order — unknown, written by an authority, ordered after the
/// verdict, or already settled — is ignored, so the first verdict on an entry stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Entries that take effect now, in this order.
    pub accept: Vec<NodeId>,
    /// Entries that never take effect.
    pub refuse: Vec<NodeId>,
}

/// Who may settle a room's entries, and how their verdicts are told apart.
///
/// Both methods must be pure: every replica of the room asks the same questions and
/// must get the same answers, or the replicas diverge.
pub trait Authority: Send + Sync {
    /// Is `writer` an authority of this room? An authority's own entries take
    /// effect where they stand, without a verdict.
    fn is_authority(&self, writer: &WriterKey) -> bool;

    /// The verdict `payload` carries, if it is one. Asked only about entries an
    /// authority wrote; `None` means an ordinary op, handed to the projection.
    fn verdict(&self, payload: &[u8]) -> Option<Verdict>;

    /// Does an entry with this `payload`, written by someone who is not an
    /// authority, wait for a verdict? `false` lets it take effect where it stands,
    /// as in a room that is not gated — for ops no authority needs to judge (ones
    /// that commute, say), so they do not cost a verdict each. Defaults to `true`.
    fn needs_verdict(&self, _payload: &[u8]) -> bool {
        true
    }
}
