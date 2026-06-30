//! `hypercore` — typed, signed, append-only log.
//!
//! A single-writer log generic over a typed payload `T` (encoded via a
//! [`codec::Codec`]). Each appended value is encoded to bytes, stored by index
//! (via [`storage::Store`]), and folded into a [`merkle::MerkleTree`]; the writer
//! then signs the new tree head (`length`, `root`) with its [`identity`] key.
//!
//! That signed head + a per-block Merkle proof let *any* verifier — holding only
//! the author's public key — confirm that a block belongs to this log at a given
//! index, without trusting the sender. Ordering and verification never inspect
//! `T`: it is opaque bytes below the codec.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use identity::{PublicKey, SecretKey, Sig};
use merkle::{Hash, MerkleTree, Proof};
use storage::{Bitfield, Store};

mod manifest_core;
pub use manifest_core::{verify_manifest_block, ManifestCore, ManifestHead, ManifestReplica};

/// Domain tag for the head-signable message (separates it from any other thing
/// the author might sign).
const HEAD_DOMAIN: u8 = 0xC0;

/// Reserved [`Store`] keys for persisted core metadata (see [`Hypercore::persist`]
/// / [`Hypercore::open`]). Block bytes occupy keys `0..length`; these live at the
/// very top of the `u64` space, so a collision would need ~1.8e19 blocks — a clean
/// divergence from upstream's separate per-section files (ADR-0041).
const KEY_META: u64 = u64::MAX; // fork + signed head + prologue
const KEY_TREE: u64 = u64::MAX - 1; // serialized MerkleTree
const KEY_PRESENCE: u64 = u64::MAX - 2; // serialized presence Bitfield
const KEY_USERDATA: u64 = u64::MAX - 3; // app metadata map (set_user_data/get_user_data)

fn head_message(fork: u64, length: u64, root: &Hash) -> Vec<u8> {
    let mut m = Vec::with_capacity(1 + 8 + 8 + 32);
    m.push(HEAD_DOMAIN);
    m.extend_from_slice(&fork.to_le_bytes());
    m.extend_from_slice(&length.to_le_bytes());
    m.extend_from_slice(root);
    m
}

/// The author's signature over the current tree head.
///
/// The signed message binds a **`fork` counter** alongside the length and root.
/// The writer bumps `fork` whenever it deliberately rewinds and rewrites history
/// ([`Hypercore::truncate`]); a reader follows the highest fork. This makes a
/// legitimate reorg by the author distinguishable from an *equivocation* — two
/// contradictory histories signed at the **same** fork (see
/// [`conflicting_heads`] / [`ForkProof`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHead {
    pub fork: u64,
    pub length: u64,
    pub root: Hash,
    pub sig: Sig,
}

/// Records the most recent truncation: the log shrank from `from` blocks to `to`
/// blocks (`to < from`). Reset to `None` by the next append/commit — it reflects
/// only the immediately preceding operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncation {
    pub from: u64,
    pub to: u64,
}

/// A commitment binding a core to a fixed **prefix** of another log: its first
/// `length` blocks must have Merkle root `hash`.
///
/// Mint one from a source with [`Hypercore::prologue_at`], carry it on a new core
/// created with [`Hypercore::with_prologue`], then adopt the committed prefix into
/// that new core with [`Hypercore::copy_prologue`] (which re-signs the prefix under
/// the new key). This is the L1 essence of upstream `move-to.js`: migrating a log's
/// history onto a fresh identity (key rotation / fast-forward) while
/// *cryptographically* pinning that the first `length` blocks are unchanged.
///
/// The commitment is **content-addressed** — it names the prefix by its Merkle
/// hash, never by the author — so any holder of the same prefix *content* can
/// satisfy it, regardless of who wrote it. Because the head at a length is a pure
/// function of the first `length` blocks, [`verify_prologue`](Hypercore::verify_prologue)
/// is a checkable invariant, and the prologue length is a [`truncate`](Hypercore::truncate)
/// floor (the committed prefix can never be rewound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prologue {
    /// Number of blocks the commitment fixes (the prefix `[0, length)`).
    pub length: u64,
    /// The Merkle root of those first `length` blocks.
    pub hash: Hash,
}

/// Options for [`Hypercore::read_stream`] — a forward (or `reverse`) iteration
/// over the decoded blocks in `[start, end)`. The L1 form of upstream's
/// `createReadStream` options.
#[derive(Clone, Copy, Debug)]
pub struct ReadStreamOptions {
    /// First block index to emit (inclusive). Default `0`.
    pub start: u64,
    /// One past the last block index to emit (exclusive). `None` means the log's
    /// current length; an explicit value is clamped to it. Default `None`.
    pub end: Option<u64>,
    /// Emit the range highest-index-first. Default `false`.
    pub reverse: bool,
    /// Upstream's `live` (keep tailing past `end` for newly-appended blocks). At
    /// L1 there is no peer/async tail, so a read stream is always a point-in-time
    /// view of `[start, end)` and this flag is **ignored** (deferred with
    /// networking). Kept so upstream's "live should be ignored" case ports
    /// directly — set it `true` and the stream still stops at `end`.
    pub live: bool,
}

impl Default for ReadStreamOptions {
    fn default() -> Self {
        Self { start: 0, end: None, reverse: false, live: false }
    }
}

/// Options for [`Hypercore::byte_stream`] — yields whole encoded blocks covering
/// the byte range `[byte_offset, byte_offset + byte_length)`. The L1 form of
/// upstream's `createByteStream` options.
#[derive(Clone, Copy, Debug)]
pub struct ByteStreamOptions {
    /// Byte offset at which to start. The stream begins at the block this offset
    /// falls in (located via the tree's [`seek`](merkle::MerkleTree::seek)).
    /// Default `0`.
    pub byte_offset: u64,
    /// Number of bytes to cover from `byte_offset`. `None` means "to the end" —
    /// the log's total byte length minus `byte_offset`. Default `None`.
    pub byte_length: Option<u64>,
}

impl Default for ByteStreamOptions {
    fn default() -> Self {
        Self { byte_offset: 0, byte_length: None }
    }
}

/// Errors from a [`Hypercore`], parameterised over the backend's error type.
#[derive(Debug, PartialEq, Eq)]
pub enum Error<SE> {
    Storage(SE),
    Codec(codec::Error),
    /// A stored block was missing where the tree says one exists, or persisted
    /// metadata was malformed / failed its self-consistency + signature check on
    /// [`open`](Hypercore::open).
    Corrupt,
    /// [`open`](Hypercore::open) found no persisted core in the store (a required
    /// metadata key is absent — the store was never [`persist`](Hypercore::persist)ed,
    /// or holds a different kind of data).
    NotPersisted,
    /// [`copy_prologue`](Hypercore::copy_prologue) was called on a core that
    /// carries no [`Prologue`] commitment to satisfy.
    NoPrologue,
    /// A source offered to [`copy_prologue`](Hypercore::copy_prologue) does not
    /// back the [`Prologue`]: it is shorter than the prologue length, is missing a
    /// prefix block, or its prefix hashes differently than the commitment names.
    /// Also returned if the target core already holds blocks (a prologue is copied
    /// only into a fresh, empty core).
    PrologueMismatch,
}

/// A staged, atomic multi-block append.
///
/// Open one with [`Hypercore::batch`], stage values into it with
/// [`Hypercore::stage`] (the log is **not** touched — staged blocks are only
/// visible through [`Hypercore::batch_get`]), then apply them all at once with
/// [`Hypercore::commit`]: every staged block lands under a **single** signed
/// head, identical to having appended them one by one. Dropping a batch without
/// committing leaves the log unchanged. A batch records the log length it was
/// opened against (`base`); if the log advances past that base before commit,
/// the commit is rejected (stale base) and the batch must be rebuilt.
pub struct Batch<T> {
    base: u64,
    encoded: Vec<Vec<u8>>,
    _t: PhantomData<fn() -> T>,
}

impl<T> Batch<T> {
    /// The log length this batch was opened against.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Number of blocks staged so far.
    pub fn staged(&self) -> usize {
        self.encoded.len()
    }

    /// The batch's logical length (`base` + staged blocks).
    pub fn length(&self) -> u64 {
        self.base + self.encoded.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.encoded.is_empty()
    }
}

/// A typed, signed, append-only log.
pub struct Hypercore<T, C, S> {
    author: SecretKey,
    public: PublicKey,
    codec: C,
    store: S,
    tree: MerkleTree,
    /// Local **presence map**: which block indices currently have their bytes in
    /// `store`. Separate from the Merkle tree (the authenticated *structure*): a
    /// block can be within the log's length yet absent — dropped by
    /// [`clear`](Hypercore::clear) to reclaim space, while the tree still
    /// authenticates it for a later re-fetch.
    presence: Bitfield,
    head: Option<SignedHead>,
    fork: u64,
    last_truncation: Option<Truncation>,
    /// A commitment to a fixed prefix this core was migrated onto (upstream's
    /// manifest `prologue`). `None` for an ordinary core. When set, the first
    /// `prologue.length` blocks are pinned to `prologue.hash`: it is a
    /// [`truncate`](Self::truncate) floor and a [`verify_prologue`](Self::verify_prologue)
    /// invariant.
    prologue: Option<Prologue>,
    _t: PhantomData<fn() -> T>,
}


/// Lazy iterator returned by [`Hypercore::read_stream`]. Yields `Result<T, _>`
/// for each present block in the configured range (forward or reverse),
/// no-wait-skipping absent blocks. See [`Hypercore::read_stream`] for semantics.
pub struct ReadStream<'a, T, C, S> {
    core: &'a Hypercore<T, C, S>,
    /// Remaining range is the half-open `[lo, hi)`; forward emits from `lo` up,
    /// reverse from `hi` down.
    lo: u64,
    hi: u64,
    reverse: bool,
}


/// Lazy iterator returned by [`Hypercore::byte_stream`]. Yields `Result<Vec<u8>,
/// _>` of whole encoded blocks covering the configured byte range. See
/// [`Hypercore::byte_stream`] for semantics.
pub struct ByteStream<'a, T, C, S> {
    core: &'a Hypercore<T, C, S>,
    /// Next block index to consider.
    next: u64,
    /// Remaining byte budget; the stream stops once it reaches `0`.
    budget: u64,
}


/// A read-only, point-in-time view of a [`Hypercore`], captured by
/// [`Hypercore::snapshot`].
///
/// A snapshot is **self-contained**: it owns a copy of the log's first `length`
/// blocks (encoded bytes), the Merkle tree at that length, and the signed head, so
/// it observes the log exactly as it was when taken — no shared mutable state with
/// the original. Later appends/truncations/rewrites of the original never change
/// what the snapshot reports.
///
/// Block bytes are copied **by value** (a clean-room divergence: upstream shares
/// storage between a core and its snapshots and relies on copy-on-write at the
/// disk layer, which is disk-format/storage plumbing we do not port — ADR-0032).
/// The observable behaviour is identical.
pub struct Snapshot<T, C> {
    length: u64,
    fork: u64,
    head: Option<SignedHead>,
    tree: MerkleTree,
    blocks: BTreeMap<u64, Vec<u8>>,
    codec: C,
    _t: PhantomData<fn() -> T>,
}

/// Verify, from a signed head alone, that `data` is the block at `index` in the
/// log owned by `public`. This is what a replica/verifier uses — it needs only
/// the public key, the signed head, and the block's proof.
pub fn verify_block(public: &PublicKey, head: &SignedHead, index: u64, data: &[u8], proof: &Proof) -> bool {
    public.verify(&head_message(head.fork, head.length, &head.root), &head.sig)
        && proof.block == index
        && proof.verify(data, &head.root)
}

/// Whether two signed heads from the **same author** are a proven *equivocation*
/// (a fork at one fork counter).
///
/// At a fixed `(fork, length)` the head's root is a deterministic pure function
/// of the first `length` blocks. So two heads of **equal fork and equal length
/// but different root**, each carrying the author's signature, are non-repudiable
/// evidence that the author signed two incompatible logs at the same counter —
/// an equivocation. This is the proof-free detector: it needs only the two
/// heads, and it is how a verifier first *notices* a fork — two contradictory
/// heads at one length (upstream's replication-time `'conflict'` at a length;
/// ADR-0019).
///
/// **Different forks are not flagged.** When the author deliberately rewinds and
/// rewrites it bumps the `fork` counter ([`Hypercore::truncate`]); a reader
/// follows the highest fork, so two heads at different forks are a legitimate
/// reorg, not equivocation. Heads of **different lengths** are likewise not
/// flagged here — an honest log of length `L2 > L1` legitimately extends the
/// length-`L1` head — so a same-fork divergence across different lengths must
/// instead be pinned to a shared block index with a [`ForkProof`].
pub fn conflicting_heads(public: &PublicKey, a: &SignedHead, b: &SignedHead) -> bool {
    a.fork == b.fork
        && a.length == b.length
        && a.root != b.root
        && public.verify(&head_message(a.fork, a.length, &a.root), &a.sig)
        && public.verify(&head_message(b.fork, b.length, &b.root), &b.sig)
}

/// Non-repudiable evidence that one author committed **two different blocks at
/// the same index** — a fork proven at a specific block index.
///
/// Each side pairs a signed head with an inclusion proof and the block bytes it
/// commits at `index`. If both sides are signed by the author **at the same fork
/// counter** and prove their block at `index`, and the two blocks differ, the
/// author signed two incompatible histories at one counter (under leaf
/// collision-resistance, different bytes ⇒ a different committed leaf) — an
/// equivocation. Unlike [`conflicting_heads`], this works across heads of
/// **different lengths** (e.g. an equivocation that also truncated one side): it
/// pins the disagreement to one shared index rather than the whole-tree root.
///
/// A divergence across **different** forks is *not* a fork: that is a legitimate
/// reorg by the author ([`Hypercore::truncate`] bumps the counter), which is why
/// `verify` requires both heads to carry the same `fork`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkProof {
    /// The block index at which the two logs disagree.
    pub index: u64,
    pub head_a: SignedHead,
    pub data_a: Vec<u8>,
    pub proof_a: Proof,
    pub head_b: SignedHead,
    pub data_b: Vec<u8>,
    pub proof_b: Proof,
}

pub struct Replica<T, C, S> {
    public: PublicKey,
    codec: C,
    store: S,
    tree: MerkleTree,
    head: Option<SignedHead>,
    _t: PhantomData<fn() -> T>,
}

mod core;
mod stream;
mod snapshot;
mod replica;

#[cfg(test)]
mod tests;

/// End-to-end browser persistence (wasm + opfs): Hypercore over OPFS-backed LogStore.
#[cfg(all(target_arch = "wasm32", feature = "opfs", test))]
mod opfs_browser_tests;
