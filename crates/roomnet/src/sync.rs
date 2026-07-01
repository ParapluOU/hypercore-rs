//! The wire protocol: how peers advertise and transfer each writer's blocks.

use autobase::WriterKey;
use hypercore::SignedHead;
use merkle::Proof;

/// The **wire** protocol peers use to discover and transfer each writer's
/// hypercore blocks.
///
/// A [`Room`](crate::Room) is backed by many hypercores — one per writer — so
/// replicating a room means learning which writers exist and fetching each one's
/// blocks. This enum is that negotiation. It is domain-agnostic (it never inspects
/// a payload) and maps onto the node's three current ALPN protocols.
///
/// A [`Block`](Self::Block) is **self-verifying**: it carries the signed `head`
/// and the Merkle inclusion `proof`, so a pushed block (Lane 1 fanout) and a
/// pulled block (a [`Want`](Self::Want) reply) are handled identically —
/// [`Replica::add_block`](hypercore::Replica::add_block) verifies it against
/// `head` with no side channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncMessage {
    /// Advert: "writer `writer` advanced to `head.length`." Tiny; gossiped.
    Head { writer: WriterKey, head: SignedHead },
    /// Anti-entropy: "I hold `writer` up to `length`."
    Have { writer: WriterKey, length: u64 },
    /// Request: "send me `writer`'s blocks in `[start, end)`."
    Want { writer: WriterKey, start: u64, end: u64 },
    /// Data: `writer`'s block at `index`. `bytes` is an encoded
    /// [`Entry`](crate::entry::Entry); `head` + `proof` make it self-verifying.
    Block {
        writer: WriterKey,
        head: SignedHead,
        index: u64,
        bytes: Vec<u8>,
        proof: Proof,
    },
}
