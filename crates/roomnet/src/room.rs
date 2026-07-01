//! The `Room` — the shared, sans-IO room-replication unit (Tier 1).
//!
//! One `Room` = one autobase-linearized multiwriter log (one hypercore per
//! writer) + a rolling finalized/live [`Projection`] + the block-sync state
//! machine. It is a **pure state machine**: no async, no transport, no clock. A
//! driver feeds it inbound `(PeerId, SyncMessage)` and ships the [`Outbound`]s it
//! returns; the node and the browser share this exact code.

use std::collections::BTreeMap;

use autobase::{AddError, Linearizer, NodeId, WriterKey};
use codec::Codec;
use hypercore::{Hypercore, Replica, SignedHead};
use identity::PublicKey;
use storage::Store;

use crate::config::{Origin, RoomConfig};
use crate::entry::{Entry, EntryCodec};
use crate::projection::Projection;
use crate::store::StoreFactory;
use crate::sync::SyncMessage;

/// An opaque peer label the driver maps to a connection. Here it is a peer's
/// writer key, but the core treats it as an opaque routing tag.
pub type PeerId = [u8; 32];

/// The storage error type of a [`StoreFactory`]'s stores.
pub type StoreErr<F> = <<F as StoreFactory>::Store as Store>::Error;

/// Where an [`Outbound`] message should be delivered.
#[derive(Clone, Debug)]
pub enum Fanout {
    /// Broadcast advert to all room peers (gossip).
    Gossip,
    /// Targeted reply to one peer.
    Peer(PeerId),
    /// Fan out to locally-subscribed clients (Lane 1 — optimistic, immediate).
    Clients,
}

/// A message the driver should send, with its [`Fanout`].
#[derive(Clone, Debug)]
pub struct Outbound {
    pub msg: SyncMessage,
    pub to: Fanout,
}

/// A newly-finalized mutation, surfaced by [`Room::poll_finalized`] for Lane 3.
///
/// One is produced **per finalized mutation** (never coalesced); a throttled
/// driver drains them to the projection sink.
#[derive(Clone, Debug)]
pub struct Finalized {
    pub node: NodeId,
    pub payload: Vec<u8>,
    /// 1-based position of this mutation in the finalized order (its version).
    pub version: u64,
}

/// Errors surfaced by a [`Room`].
#[derive(Debug)]
pub enum Error<SE> {
    /// An error from an underlying hypercore/replica (storage, codec, corruption).
    Hypercore(hypercore::Error<SE>),
    /// A block's bytes did not decode as an [`Entry`].
    Codec(codec::Error),
    /// A writer key was not a valid ed25519 public key.
    BadWriterKey,
    /// The [`Projection`] rejected a mutation (rendered via its `Debug`).
    Projection(String),
}

impl<SE> From<hypercore::Error<SE>> for Error<SE> {
    fn from(e: hypercore::Error<SE>) -> Self {
        Error::Hypercore(e)
    }
}

/// A single room: an autobase-ordered multiwriter log with rolling projections.
pub struct Room<F: StoreFactory, P: Projection> {
    local_key: WriterKey,
    origin: Origin,

    /// The one writer this node owns and can *serve* proofs for.
    local: Hypercore<Entry, EntryCodec, F::Store>,
    /// Verify-only replicas of every other writer we have seen.
    remotes: BTreeMap<WriterKey, Replica<Entry, EntryCodec, F::Store>>,
    factory: F,

    /// The causal DAG + quorum machine.
    lin: Linearizer,
    /// Decoded op payload per delivered node (for projection folds without re-reads).
    payloads: BTreeMap<NodeId, Vec<u8>>,
    /// Blocks accepted into a replica but whose causal deps are not yet present.
    pending: Vec<(NodeId, Entry)>,

    /// Folds the *finalized* order; monotonic. Drives Lane 3 + the queryable snapshot.
    finalized_proj: P,
    /// Recomputed each mutation: finalized state + the speculative tail (optimistic).
    live_proj: P,
    /// How many nodes of `lin.finalized()` have been folded into `finalized_proj`.
    finalized_len: usize,
    /// Finalized mutations not yet drained by [`poll_finalized`].
    finalized_since_poll: Vec<Finalized>,

    /// Monotone logical activity counter (bumped on every append/ingest). The
    /// driver reads it to time stale-eviction of replicated rooms.
    last_activity: u64,
}

impl<F, P> Room<F, P>
where
    F: StoreFactory,
    P: Projection + Clone,
{
    /// Open a room with this identity, indexer set, storage, and projection.
    ///
    /// Creates the local writer core (fresh here; durable re-open of a persisted
    /// local core and cross-writer resume-from-checkpoint are follow-ons). Remote
    /// writers are learned and pulled on demand as their adverts arrive.
    pub fn open(cfg: RoomConfig, mut factory: F, projection: P) -> Self {
        let RoomConfig { identity, indexers, origin } = cfg;
        let local_key = identity.public().to_bytes();
        let store = factory.open(local_key);
        let local = Hypercore::new(identity, EntryCodec, store);
        let lin = if indexers.is_empty() {
            Linearizer::new()
        } else {
            Linearizer::with_indexers(indexers.iter().copied())
        };
        Room {
            local_key,
            origin,
            local,
            remotes: BTreeMap::new(),
            factory,
            lin,
            payloads: BTreeMap::new(),
            pending: Vec::new(),
            live_proj: projection.clone(),
            finalized_proj: projection,
            finalized_len: 0,
            finalized_since_poll: Vec::new(),
            last_activity: 0,
        }
    }

    /// This node's writer key.
    pub fn local_key(&self) -> WriterKey {
        self.local_key
    }

    /// The room's provenance/GC policy.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Logical activity counter — bumped on every local append and remote ingest.
    /// The driver diffs it over time to evict cold *replicated* rooms.
    pub fn last_activity(&self) -> u64 {
        self.last_activity
    }

    /// Number of mutations in the finalized order.
    pub fn finalized_len(&self) -> usize {
        self.finalized_len
    }

    /// The current deterministic linear order of all delivered nodes.
    pub fn order(&self) -> Vec<NodeId> {
        self.lin.order()
    }

    /// The authoritative (finalized) materialized snapshot — what the node API
    /// serves and what Lane 3 persists.
    pub fn snapshot_finalized(&self) -> &P::State {
        self.finalized_proj.snapshot()
    }

    /// The optimistic (live) snapshot — finalized state plus the unconfirmed tail.
    /// What a client renders immediately.
    pub fn snapshot_live(&self) -> &P::State {
        self.live_proj.snapshot()
    }

    /// Append a local op. Signs a new head (durable via the local store — Lane 2),
    /// feeds the linearizer, and returns the outbounds: a gossip [`Head`] advert
    /// and a client fanout of the new block (Lane 1).
    pub fn local_append(&mut self, op: &[u8]) -> Result<Vec<Outbound>, Error<StoreErr<F>>> {
        let seq = self.local.len();
        let heads: Vec<NodeId> = self.lin.tails().into_iter().collect();
        let entry = Entry::new(heads.clone(), op.to_vec());
        self.local.append(&entry)?;

        let node = NodeId::new(self.local_key, seq);
        match self.lin.add(node, &heads) {
            Ok(()) | Err(AddError::Duplicate(_)) => {}
            // A local append references only already-delivered tails, so a gap or
            // missing head would be a bug in this room, not a network condition.
            Err(_) => debug_assert!(false, "local append violated causal delivery"),
        }
        self.payloads.insert(node, op.to_vec());
        self.touch();
        self.advance()?;

        let head = self.local.head().cloned().expect("head exists after append");
        let bytes = self.local.block(seq)?.expect("block exists after append");
        let proof = self.local.proof(seq).expect("proof exists after append");
        Ok(vec![
            Outbound { msg: SyncMessage::Head { writer: self.local_key, head: head.clone() }, to: Fanout::Gossip },
            Outbound {
                msg: SyncMessage::Block { writer: self.local_key, head, index: seq, bytes, proof },
                to: Fanout::Clients,
            },
        ])
    }

    /// Handle one inbound message from `from`. Returns any outbounds to send.
    pub fn on_inbound(
        &mut self,
        from: PeerId,
        msg: SyncMessage,
    ) -> Result<Vec<Outbound>, Error<StoreErr<F>>> {
        let mut out = Vec::new();
        match msg {
            SyncMessage::Head { writer, head } => {
                if writer != self.local_key && head.length > self.writer_len(writer) {
                    out.push(Outbound {
                        msg: SyncMessage::Want { writer, start: self.writer_len(writer), end: head.length },
                        to: Fanout::Peer(from),
                    });
                }
            }
            SyncMessage::Have { writer, length } => {
                if writer != self.local_key && length > self.writer_len(writer) {
                    out.push(Outbound {
                        msg: SyncMessage::Want { writer, start: self.writer_len(writer), end: length },
                        to: Fanout::Peer(from),
                    });
                }
            }
            SyncMessage::Want { writer, start, end } => {
                // Only writers we host locally can be served (we hold their proofs).
                // Relaying a replicated writer's blocks is a follow-on.
                if writer == self.local_key {
                    if let Some(head) = self.local.head().cloned() {
                        let hi = end.min(self.local.len());
                        for i in start..hi {
                            if let (Some(bytes), Some(proof)) = (self.local.block(i)?, self.local.proof(i)) {
                                out.push(Outbound {
                                    msg: SyncMessage::Block { writer, head: head.clone(), index: i, bytes, proof },
                                    to: Fanout::Peer(from),
                                });
                            }
                        }
                    }
                }
            }
            SyncMessage::Block { writer, head, index, bytes, proof } => {
                if writer != self.local_key
                    && self.ingest_remote_block(writer, &head, index, &bytes, &proof)?
                {
                    self.drain_pending();
                    self.advance()?;
                    // Immediately fan the verified block out to local clients (Lane 1).
                    out.push(Outbound {
                        msg: SyncMessage::Block { writer, head, index, bytes, proof },
                        to: Fanout::Clients,
                    });
                }
            }
        }
        Ok(out)
    }

    /// A gossip advert of the local writer's current head (sent on connect).
    pub fn announce(&self) -> Vec<Outbound> {
        match self.local.head().cloned() {
            Some(head) => vec![Outbound {
                msg: SyncMessage::Head { writer: self.local_key, head },
                to: Fanout::Gossip,
            }],
            None => Vec::new(),
        }
    }

    /// Drain the finalized mutations produced since the last call (Lane 3). One
    /// entry per finalized mutation — never coalesced.
    pub fn poll_finalized(&mut self) -> Vec<Finalized> {
        std::mem::take(&mut self.finalized_since_poll)
    }

    /// The decoded entries of `writer` in `[start, end)` (a hypercore-logs query).
    pub fn logs(
        &self,
        writer: WriterKey,
        start: u64,
        end: u64,
    ) -> Result<Vec<Entry>, Error<StoreErr<F>>> {
        let hi = end.min(self.writer_len(writer));
        let mut v = Vec::new();
        for i in start..hi {
            let e = if writer == self.local_key {
                self.local.get(i)?
            } else {
                match self.remotes.get(&writer) {
                    Some(r) => r.get(i)?,
                    None => None,
                }
            };
            if let Some(e) = e {
                v.push(e);
            }
        }
        Ok(v)
    }

    // ---- internals -------------------------------------------------------

    fn writer_len(&self, writer: WriterKey) -> u64 {
        if writer == self.local_key {
            self.local.len()
        } else {
            self.remotes.get(&writer).map(|r| r.len()).unwrap_or(0)
        }
    }

    fn touch(&mut self) {
        self.last_activity = self.last_activity.wrapping_add(1);
    }

    /// Verify + store one remote block, then attempt to deliver it to the DAG.
    /// Returns whether the block was accepted by the replica.
    fn ingest_remote_block(
        &mut self,
        writer: WriterKey,
        head: &SignedHead,
        index: u64,
        bytes: &[u8],
        proof: &merkle::Proof,
    ) -> Result<bool, Error<StoreErr<F>>> {
        if !self.remotes.contains_key(&writer) {
            let public = PublicKey::from_bytes(&writer).ok_or(Error::BadWriterKey)?;
            let store = self.factory.open(writer);
            self.remotes.insert(writer, Replica::new(public, EntryCodec, store));
        }
        let replica = self.remotes.get_mut(&writer).expect("just inserted");
        if !replica.add_block(head, index, bytes, proof)? {
            return Ok(false);
        }
        let entry = EntryCodec.decode(bytes).map_err(Error::Codec)?;
        self.try_add_node(NodeId::new(writer, index), entry);
        self.touch();
        Ok(true)
    }

    /// Try to add one node to the DAG; buffer it if its causal deps are missing.
    fn try_add_node(&mut self, node: NodeId, entry: Entry) {
        match self.lin.add(node, &entry.heads) {
            Ok(()) => {
                self.payloads.insert(node, entry.payload);
            }
            Err(AddError::Duplicate(_)) => {}
            Err(AddError::MissingHead(_)) | Err(AddError::Gap { .. }) => {
                self.pending.push((node, entry));
            }
        }
    }

    /// Retry buffered blocks until no further progress (a dep may have just arrived).
    fn drain_pending(&mut self) {
        loop {
            let mut progressed = false;
            let mut still = Vec::new();
            for (node, entry) in std::mem::take(&mut self.pending) {
                match self.lin.add(node, &entry.heads) {
                    Ok(()) => {
                        self.payloads.insert(node, entry.payload);
                        progressed = true;
                    }
                    Err(AddError::Duplicate(_)) => progressed = true,
                    Err(_) => still.push((node, entry)),
                }
            }
            self.pending = still;
            if !progressed {
                break;
            }
        }
    }

    /// Recompute derived state: extend the finalized fold, then rebuild the live
    /// fold as finalized-state + the speculative tail.
    fn advance(&mut self) -> Result<(), Error<StoreErr<F>>> {
        // 1. Finalized prefix grows monotonically — fold only the new suffix.
        let fin = self.lin.finalized();
        if fin.len() > self.finalized_len {
            for i in self.finalized_len..fin.len() {
                let node = fin[i];
                let payload = self.payloads.get(&node).cloned().unwrap_or_default();
                self.finalized_proj
                    .apply(node, &payload)
                    .map_err(|e| Error::Projection(format!("{:?}", e)))?;
                self.finalized_since_poll.push(Finalized {
                    node,
                    payload,
                    version: (i as u64) + 1,
                });
            }
            self.finalized_len = fin.len();
        }

        // 2. Live view = finalized checkpoint + the unconfirmed tail (optimistic).
        //    finalized() is a true prefix of order(), so the tail is order[finalized_len..].
        self.live_proj.reset_to(self.finalized_proj.snapshot());
        let order = self.lin.order();
        debug_assert!(order.len() >= self.finalized_len, "finalized must be a prefix of order");
        for node in &order[self.finalized_len..] {
            let payload = self.payloads.get(node).cloned().unwrap_or_default();
            // Live is best-effort/optimistic; a tail apply error must not abort ingest.
            let _ = self.live_proj.apply(*node, &payload);
        }
        Ok(())
    }
}
