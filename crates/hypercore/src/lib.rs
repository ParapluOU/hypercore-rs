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

use codec::Codec;
use identity::{PublicKey, SecretKey, Sig};
use merkle::{Hash, MerkleTree, Proof, UpgradeProof};
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

impl<T, C: Codec<T>, S: Store> Hypercore<T, C, S> {
    /// Create a fresh, empty log written by `author`.
    pub fn new(author: SecretKey, codec: C, store: S) -> Self {
        let public = author.public();
        Self {
            author,
            public,
            codec,
            store,
            tree: MerkleTree::new(),
            presence: Bitfield::new(),
            head: None,
            fork: 0,
            last_truncation: None,
            prologue: None,
            _t: PhantomData,
        }
    }

    /// Create a fresh, empty log written by `author` and **bound to a
    /// [`Prologue`]** — a commitment to a prefix of some other log. The core
    /// starts empty; [`copy_prologue`](Self::copy_prologue) then adopts the
    /// committed prefix's blocks under this `author`'s key. This is how a log is
    /// migrated onto a new identity (the L1 of upstream `move-to.js`).
    pub fn with_prologue(author: SecretKey, codec: C, store: S, prologue: Prologue) -> Self {
        let mut core = Self::new(author, codec, store);
        core.prologue = Some(prologue);
        core
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    /// The [`Prologue`] this core is bound to, if any (set by
    /// [`with_prologue`](Self::with_prologue)).
    pub fn prologue(&self) -> Option<&Prologue> {
        self.prologue.as_ref()
    }

    /// Mint a [`Prologue`] committing to this core's first `length` blocks
    /// (`{ length, root-of-the-prefix }`), or `None` if `length > len()`. The
    /// migration source hands this to [`with_prologue`](Self::with_prologue) so a
    /// new core can adopt the same prefix. Upstream's `{ length, hash }` manifest
    /// prologue.
    pub fn prologue_at(&self, length: u64) -> Option<Prologue> {
        Some(Prologue { length, hash: self.tree.prefix_root_hash(length)? })
    }

    /// Whether this core still satisfies its [`Prologue`] commitment: its first
    /// `prologue.length` blocks hash to `prologue.hash`. A core with no prologue
    /// trivially satisfies it. Maintained as an invariant — appends only extend the
    /// log and [`truncate`](Self::truncate) refuses to rewind below the prologue.
    pub fn verify_prologue(&self) -> bool {
        match self.prologue {
            None => true,
            Some(pr) => self.tree.prefix_root_hash(pr.length) == Some(pr.hash),
        }
    }

    pub fn len(&self) -> u64 {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn head(&self) -> Option<&SignedHead> {
        self.head.as_ref()
    }

    /// The current fork counter (`0` for a log that was never truncated). It
    /// increments by one on each [`truncate`](Self::truncate) and is signed into
    /// every head.
    pub fn fork(&self) -> u64 {
        self.fork
    }

    /// Total byte size of the live blocks (sum of the Merkle root subtree sizes).
    pub fn byte_length(&self) -> u64 {
        self.tree.byte_length()
    }

    /// The truncation performed by the immediately preceding operation, or `None`
    /// if the last operation was an append/commit (which clears it).
    pub fn last_truncation(&self) -> Option<Truncation> {
        self.last_truncation
    }

    /// Re-sign the current tree head under the current `fork`.
    fn resign(&mut self) {
        let length = self.tree.len();
        let root = self.tree.root_hash();
        let sig = self.author.sign(&head_message(self.fork, length, &root));
        self.head = Some(SignedHead { fork: self.fork, length, root, sig });
    }

    /// Append a value; returns its block index. Append-only: indices only grow.
    pub fn append(&mut self, value: &T) -> Result<u64, Error<S::Error>> {
        let bytes = self.codec.encode(value);
        let index = self.tree.len();
        self.store.put(index, &bytes).map_err(Error::Storage)?;
        self.tree.append(&bytes);
        self.presence.set(index, true);
        self.last_truncation = None;
        self.resign();
        Ok(index)
    }

    /// Rewind the log to its first `new_len` blocks, discarding every block at
    /// index `>= new_len`, then re-sign a new head under an **incremented `fork`
    /// counter**. Returns the [`Truncation`] performed, or `None` if
    /// `new_len >= len()` (nothing to truncate). Ports hypercore `core.js`'s
    /// "append and truncate" behaviour (length / byteLength / fork progression).
    ///
    /// The new tree is node-for-node the prefix tree, so the new `root` is exactly
    /// the prefix's root — but the bumped `fork` (signed into the head) marks this
    /// as a *deliberate* reorg by the author, so a later truncate-and-rewrite is
    /// not mistaken for an equivocation (which is a fork at the **same** counter;
    /// see [`conflicting_heads`] / [`ForkProof`]).
    ///
    /// Storage is not eagerly reclaimed: blocks at `>= new_len` become logically
    /// unreachable ([`get`](Self::get)/[`block`](Self::block) gate on the length)
    /// and are overwritten when those indices are re-appended. The logical
    /// truncation (tree + head) is a pure in-memory mutation, so it is atomic and
    /// infallible; physical reclamation is a separate concern (upstream
    /// `clear.js`/`purge.js`).
    pub fn truncate(&mut self, new_len: u64) -> Option<Truncation> {
        let from = self.tree.len();
        if let Some(pr) = self.prologue {
            if new_len < pr.length {
                return None; // cannot rewind into the committed prologue prefix
            }
        }
        if !self.tree.truncate(new_len) {
            return None; // new_len >= len: nothing to do
        }
        self.presence.set_range(new_len, from, false); // the discarded tail is no longer present
        self.fork += 1;
        let t = Truncation { from, to: new_len };
        self.last_truncation = Some(t);
        self.resign();
        Some(t)
    }

    /// Open an empty [`Batch`] based on the current log length.
    pub fn batch(&self) -> Batch<T> {
        Batch {
            base: self.tree.len(),
            encoded: Vec::new(),
            _t: PhantomData,
        }
    }

    /// Encode and stage `value` into `batch`. The log is untouched; the value is
    /// only visible through [`Self::batch_get`] until [`Self::commit`].
    pub fn stage(&self, batch: &mut Batch<T>, value: &T) {
        batch.encoded.push(self.codec.encode(value));
    }

    /// Read block `index` as seen *through* `batch`: indices below the batch's
    /// base come from the committed log, indices in the staged range from the
    /// batch itself. `None` past the batch's end.
    pub fn batch_get(&self, batch: &Batch<T>, index: u64) -> Result<Option<T>, Error<S::Error>> {
        if index < batch.base {
            return self.get(index);
        }
        match batch.encoded.get((index - batch.base) as usize) {
            Some(enc) => Ok(Some(self.codec.decode(enc).map_err(Error::Codec)?)),
            None => Ok(None),
        }
    }

    /// Atomically apply every staged block under a **single** signed head.
    ///
    /// All-or-nothing: blocks are written to storage first and, on any storage
    /// failure, the partial writes are rolled back and the Merkle tree + signed
    /// head are left **untouched** (the log's logical state never advances on a
    /// failed commit). Returns the new length on success.
    ///
    /// Returns `Ok(None)` — leaving the log unchanged — if the log advanced past
    /// the batch's base since it was opened (a *stale base*): the batch was built
    /// against a head that no longer exists and must be rebuilt. An empty batch
    /// is a successful no-op.
    pub fn commit(&mut self, batch: Batch<T>) -> Result<Option<u64>, Error<S::Error>> {
        if batch.base != self.tree.len() {
            return Ok(None); // stale base: the log moved under the batch
        }
        if batch.encoded.is_empty() {
            return Ok(Some(self.tree.len())); // empty batch: nothing to do
        }

        // Write every staged block first; this is the only fallible step. On
        // failure, undo the writes already made so the tree + head — the log's
        // source of truth — are never advanced on a partial batch.
        let start = self.tree.len();
        let mut written: Vec<u64> = Vec::with_capacity(batch.encoded.len());
        for (i, enc) in batch.encoded.iter().enumerate() {
            let idx = start + i as u64;
            if let Err(e) = self.store.put(idx, enc) {
                for w in &written {
                    let _ = self.store.delete(*w);
                }
                return Err(Error::Storage(e));
            }
            written.push(idx);
        }

        // All blocks stored — now fold them into the tree, mark them present, and
        // sign once. (Presence is set only after every write succeeded, so a
        // rolled-back failed commit leaves the presence map untouched too.)
        for (i, enc) in batch.encoded.iter().enumerate() {
            self.tree.append(enc);
            self.presence.set(start + i as u64, true);
        }
        self.last_truncation = None;
        self.resign();
        Ok(Some(self.tree.len()))
    }

    /// Adopt this (empty, prologue-bound) core's committed prefix from `source`,
    /// re-signing it under **our** key — the L1 of upstream `core.copyPrologue`.
    ///
    /// The migration step behind `move-to.js`: a fresh core created with
    /// [`with_prologue`](Self::with_prologue) copies in the first
    /// `prologue.length` blocks of an existing log and re-signs them under its own
    /// (new) identity, so the history continues under a rotated key while the
    /// prefix is preserved **byte-identically**. New blocks then [`append`](Self::append)
    /// on top of the migrated prefix as normal.
    ///
    /// Because a [`Prologue`] is **content-addressed** (it names the prefix by its
    /// Merkle hash, ADR-0034), `source` need not share our key — any log whose
    /// first `prologue.length` blocks hash to `prologue.hash` backs it. The match
    /// is checked *before* copying, so a non-matching `source` leaves this core
    /// untouched.
    ///
    /// Returns the migrated length (`= prologue.length`). Errors:
    /// [`NoPrologue`](Error::NoPrologue) if this core carries no prologue;
    /// [`PrologueMismatch`](Error::PrologueMismatch) if this core is non-empty, or
    /// `source` is too short / missing a prefix block / hashes differently than the
    /// commitment names.
    pub fn copy_prologue(&mut self, source: &Hypercore<T, C, S>) -> Result<u64, Error<S::Error>> {
        let pr = self.prologue.ok_or(Error::NoPrologue)?;
        if !self.is_empty() {
            return Err(Error::PrologueMismatch); // a prologue is copied only into a fresh core
        }
        // Content-addressed check: `source`'s first `pr.length` blocks must hash to
        // exactly what the commitment names. `prefix_root_hash` is `None` if the
        // source is shorter than `pr.length`, so a too-short source is rejected here.
        if source.tree.prefix_root_hash(pr.length) != Some(pr.hash) {
            return Err(Error::PrologueMismatch);
        }
        // Copy the committed prefix in, rebuilding an identical prefix tree (the
        // surviving nodes are a pure function of the block bytes, so our root ends
        // equal to `pr.hash`), marking each block present, and signing it under our
        // own key (fork 0).
        for i in 0..pr.length {
            let enc = source.block(i)?.ok_or(Error::PrologueMismatch)?;
            self.store.put(i, &enc).map_err(Error::Storage)?;
            self.tree.append(&enc);
            self.presence.set(i, true);
        }
        self.last_truncation = None;
        self.resign();
        Ok(pr.length)
    }

    /// Decode the value at `index`, or `None` if out of range **or locally
    /// absent** (never downloaded, or dropped by [`clear`](Self::clear)). This is
    /// a no-wait read: at L1 there is no peer to fetch a missing block from, so an
    /// absent block reads as `None` (upstream `get(i, { wait: false })`).
    ///
    /// [`Error::Corrupt`] is reserved for genuine corruption — the presence map
    /// says block `index` is here but its bytes are missing from `store`.
    pub fn get(&self, index: u64) -> Result<Option<T>, Error<S::Error>> {
        if !self.has(index) {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(index)
            .map_err(Error::Storage)?
            .ok_or(Error::Corrupt)?;
        let value = self.codec.decode(&bytes).map_err(Error::Codec)?;
        Ok(Some(value))
    }

    /// The raw stored (codec-encoded) bytes of block `index` — i.e. exactly what
    /// the Merkle tree committed to. This is the unit a verifier checks a proof
    /// against (it decodes only *after* verifying). `None` if out of range or
    /// locally absent (see [`get`](Self::get)).
    pub fn block(&self, index: u64) -> Result<Option<Vec<u8>>, Error<S::Error>> {
        if !self.has(index) {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(index)
            .map_err(Error::Storage)?
            .ok_or(Error::Corrupt)?;
        Ok(Some(bytes))
    }

    /// Whether block `index` is **present locally** — i.e. its bytes are in
    /// `store` and readable via [`get`](Self::get)/[`block`](Self::block). A block
    /// can be within the log's [`len`](Self::len) yet absent (dropped by
    /// [`clear`](Self::clear)); `false` for out-of-range indices. Ports upstream
    /// `core.has(index)`.
    pub fn has(&self, index: u64) -> bool {
        index < self.tree.len() && self.presence.get(index)
    }

    /// Length of the contiguous run of present blocks from index `0` (upstream
    /// `contiguousLength`). Equals [`len`](Self::len) for a fully-present log; it
    /// drops to the first hole after a [`clear`](Self::clear).
    pub fn contiguous_length(&self) -> u64 {
        // The first absent index is where the contiguous prefix ends; cap it at the
        // log length (`find_first(false, ..)` is always `Some` — the field is an
        // infinite-zero tail, so beyond a fully-present log it returns `len`).
        self.presence.find_first(false, 0).unwrap_or(0).min(self.tree.len())
    }

    /// Drop the locally-stored bytes for the present blocks in `[start, end)`,
    /// marking them **absent** and reclaiming their storage. Returns the number of
    /// blocks actually cleared (`0` if the range is empty, out of range, or only
    /// covers already-absent blocks — upstream's `null`/no-op).
    ///
    /// This is **presence** reclamation, *not* a [`truncate`](Self::truncate): the
    /// Merkle tree — length, root, every node — is **unchanged**, so the log still
    /// authenticates the cleared blocks ([`proof`](Self::proof) and the signed
    /// [`head`](Self::head) are unaffected) and they can be re-fetched and
    /// re-verified later. Clearing absent or out-of-range blocks has no effect (it
    /// never touches a block it doesn't hold — upstream's "no side effect from
    /// clearing unknown nodes").
    ///
    /// Physical reclamation is best-effort and decoupled from the logical state:
    /// the presence bit is cleared first (so the block immediately reads as absent),
    /// then the bytes are deleted; a `store` error is surfaced, having left the
    /// block correctly marked absent.
    pub fn clear(&mut self, start: u64, end: u64) -> Result<u64, Error<S::Error>> {
        let end = end.min(self.tree.len());
        let mut cleared = 0;
        let mut i = start;
        while i < end {
            if self.presence.get(i) {
                self.presence.set(i, false);
                self.store.delete(i).map_err(Error::Storage)?;
                cleared += 1;
            }
            i += 1;
        }
        Ok(cleared)
    }

    /// A read stream over the decoded blocks in a range — the L1 form of upstream
    /// `createReadStream`. Yields each present block's value in index order (or
    /// reverse, per [`ReadStreamOptions::reverse`]); `start`/`end` bound it to
    /// `[start, end)` (`end` defaults to, and is clamped to, [`len`](Self::len)).
    ///
    /// It is a **no-wait** stream (consistent with [`get`](Self::get)): a block in
    /// the range that is absent — never downloaded, or dropped by
    /// [`clear`](Self::clear) — is *skipped* rather than waited on, since at L1
    /// there is no peer to fetch it from. Each item is a `Result` because reading
    /// or decoding a present block can still fail (storage / codec error). `live`
    /// is ignored (see [`ReadStreamOptions`]).
    ///
    /// A `createWriteStream` is just a buffered [`append`](Self::append) of the
    /// same blocks, so it adds no L1 behaviour and is covered by append/batch.
    pub fn read_stream(&self, opts: ReadStreamOptions) -> ReadStream<'_, T, C, S> {
        let end = opts.end.unwrap_or_else(|| self.tree.len()).min(self.tree.len());
        let lo = opts.start.min(end);
        ReadStream { core: self, lo, hi: end, reverse: opts.reverse }
    }

    /// A byte stream over the log — the L1 form of upstream `createByteStream`.
    /// Yields whole **encoded** blocks (the raw stored bytes, exactly as the
    /// Merkle tree committed them) covering the byte range `[byte_offset,
    /// byte_offset + byte_length)`: it [`seek`](merkle::MerkleTree::seek)s to the
    /// block containing `byte_offset` and emits whole blocks until the byte budget
    /// is consumed (`byte_length` defaults to "to the end"). A block whose payload
    /// is empty is still emitted as long as the budget is not yet exhausted
    /// (upstream's "decode previous blocks even though they don't contribute to
    /// byte length").
    ///
    /// Byte offsets address the **encoded** byte layout the tree authenticates,
    /// not the decoded payload — we keep per-block framing out of L1 (the seek
    /// `padding` divergence, ADR-0022); a consumer subtracts its own framing
    /// before seeking. A non-boundary `byte_offset` emits the whole block it lands
    /// in (sub-block slicing is deferred). No-wait, like [`read_stream`](Self::read_stream).
    pub fn byte_stream(&self, opts: ByteStreamOptions) -> ByteStream<'_, T, C, S> {
        let total = self.tree.byte_length();
        let budget = opts.byte_length.unwrap_or_else(|| total.saturating_sub(opts.byte_offset));
        let (start_block, _) = self.tree.seek(opts.byte_offset);
        ByteStream { core: self, next: start_block, budget }
    }

    /// A Merkle inclusion proof for `index` (pair with [`Self::head`] to make it
    /// independently verifiable).
    pub fn proof(&self, index: u64) -> Option<Proof> {
        self.tree.proof(index)
    }

    /// A length-extension (consistency) proof bridging length `old` to `new` for
    /// this log. Pair it with the *new* [`Self::head`]: a replica that has
    /// already verified up to `old` can confirm the longer head is an honest
    /// append-only extension (the first `old` blocks weren't rewritten) **before**
    /// fetching the new blocks (see [`Replica::verify_upgrade`]). `None` unless
    /// `1 <= old < new <= len`.
    pub fn upgrade_proof(&self, old: u64, new: u64) -> Option<UpgradeProof> {
        self.tree.upgrade_proof(old, new)
    }

    /// Internal-consistency + signature check of our own head.
    pub fn verify_head(&self) -> bool {
        match &self.head {
            None => self.tree.is_empty(),
            Some(h) => {
                h.fork == self.fork
                    && h.length == self.tree.len()
                    && h.root == self.tree.root_hash()
                    && self.public.verify(&head_message(h.fork, h.length, &h.root), &h.sig)
            }
        }
    }

    /// Encode the in-memory authenticated *metadata* (the part not already in the
    /// block store): `fork`, the optional `SignedHead`, and the optional
    /// `Prologue`. Layout (little-endian): `[fork u64]`, then a head tag — `0` for
    /// none or `1` followed by `[fork u64][length u64][root 32B][sig 64B]` — then a
    /// prologue tag — `0` for none or `1` followed by `[length u64][hash 32B]`.
    /// The transient `last_truncation` guard is deliberately not persisted.
    fn encode_meta(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 1 + 112 + 1 + 40);
        out.extend_from_slice(&self.fork.to_le_bytes());
        match &self.head {
            Some(h) => {
                out.push(1);
                out.extend_from_slice(&h.fork.to_le_bytes());
                out.extend_from_slice(&h.length.to_le_bytes());
                out.extend_from_slice(&h.root);
                out.extend_from_slice(&h.sig.to_bytes());
            }
            None => out.push(0),
        }
        match &self.prologue {
            Some(p) => {
                out.push(1);
                out.extend_from_slice(&p.length.to_le_bytes());
                out.extend_from_slice(&p.hash);
            }
            None => out.push(0),
        }
        out
    }

    /// Inverse of [`encode_meta`](Self::encode_meta). `None` on any malformed or
    /// truncated buffer (including trailing bytes).
    fn decode_meta(bytes: &[u8]) -> Option<(u64, Option<SignedHead>, Option<Prologue>)> {
        let mut off = 0usize;
        let take = |off: &mut usize, n: usize| -> Option<&[u8]> {
            let s = bytes.get(*off..*off + n)?;
            *off += n;
            Some(s)
        };
        let u64_at = |off: &mut usize| -> Option<u64> {
            Some(u64::from_le_bytes(take(off, 8)?.try_into().ok()?))
        };

        let fork = u64_at(&mut off)?;

        let head = match take(&mut off, 1)?[0] {
            0 => None,
            1 => {
                let h_fork = u64_at(&mut off)?;
                let length = u64_at(&mut off)?;
                let mut root: Hash = [0u8; 32];
                root.copy_from_slice(take(&mut off, 32)?);
                let mut sig_bytes = [0u8; 64];
                sig_bytes.copy_from_slice(take(&mut off, 64)?);
                Some(SignedHead {
                    fork: h_fork,
                    length,
                    root,
                    sig: Sig::from_bytes(&sig_bytes),
                })
            }
            _ => return None,
        };

        let prologue = match take(&mut off, 1)?[0] {
            0 => None,
            1 => {
                let length = u64_at(&mut off)?;
                let mut hash: Hash = [0u8; 32];
                hash.copy_from_slice(take(&mut off, 32)?);
                Some(Prologue { length, hash })
            }
            _ => return None,
        };

        if off != bytes.len() {
            return None; // trailing garbage
        }
        Some((fork, head, prologue))
    }

    /// Persist this core's authenticated state to its [`Store`] so it can be
    /// reconstituted later with [`open`](Self::open) — the local-first payoff: a
    /// browser (OPFS) writer survives a reload. Block bytes are already written on
    /// [`append`](Self::append); this flushes the parts that otherwise live only in
    /// memory — the Merkle tree, the presence map, the signed head, the fork, and
    /// any prologue — under three reserved high keys ([`KEY_TREE`], [`KEY_PRESENCE`],
    /// [`KEY_META`]) that no realistic block index can collide with.
    ///
    /// The author's **secret key is never written** — it belongs to the caller's
    /// keyring, not the data store; `open` takes it back as a parameter.
    pub fn persist(&mut self) -> Result<(), Error<S::Error>> {
        let tree = self.tree.serialize();
        let presence = self.presence.serialize();
        let meta = self.encode_meta();
        self.store.put(KEY_TREE, &tree).map_err(Error::Storage)?;
        self.store.put(KEY_PRESENCE, &presence).map_err(Error::Storage)?;
        self.store.put(KEY_META, &meta).map_err(Error::Storage)?;
        Ok(())
    }

    /// Reconstitute a writable core from a [`Store`] previously written by
    /// [`persist`](Self::persist), pairing the persisted log data with the
    /// caller-supplied `author` key and `codec`. The loaded head is checked for
    /// internal consistency *and* verified against `author`'s public key
    /// ([`verify_head`](Self::verify_head)) — so opening a store under the wrong key,
    /// or one carrying tampered metadata, fails with [`Error::Corrupt`] rather than
    /// yielding a bogus core. [`Error::NotPersisted`] if the store was never
    /// persisted.
    pub fn open(author: SecretKey, codec: C, store: S) -> Result<Self, Error<S::Error>> {
        let public = author.public();

        let meta_bytes = store.get(KEY_META).map_err(Error::Storage)?;
        let tree_bytes = store.get(KEY_TREE).map_err(Error::Storage)?;
        let presence_bytes = store.get(KEY_PRESENCE).map_err(Error::Storage)?;
        let (Some(meta_bytes), Some(tree_bytes), Some(presence_bytes)) =
            (meta_bytes, tree_bytes, presence_bytes)
        else {
            return Err(Error::NotPersisted);
        };

        let tree = MerkleTree::deserialize(&tree_bytes).ok_or(Error::Corrupt)?;
        let presence = Bitfield::deserialize(&presence_bytes).ok_or(Error::Corrupt)?;
        let (fork, head, prologue) = Self::decode_meta(&meta_bytes).ok_or(Error::Corrupt)?;

        let core = Self {
            author,
            public,
            codec,
            store,
            tree,
            presence,
            head,
            fork,
            last_truncation: None,
            prologue,
            _t: PhantomData,
        };

        // The persisted head must be self-consistent with the persisted tree *and*
        // signed by this author — catches a wrong key, a mismatched tree/head pair,
        // or tampered metadata.
        if !core.verify_head() {
            return Err(Error::Corrupt);
        }
        Ok(core)
    }
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

impl<T, C: Codec<T>, S: Store> Iterator for ReadStream<'_, T, C, S> {
    type Item = Result<T, Error<S::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.lo < self.hi {
            let i = if self.reverse {
                self.hi -= 1;
                self.hi
            } else {
                let i = self.lo;
                self.lo += 1;
                i
            };
            match self.core.get(i) {
                Ok(Some(v)) => return Some(Ok(v)),
                Ok(None) => continue, // absent block: no-wait skip
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
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

impl<T, C: Codec<T>, S: Store> Iterator for ByteStream<'_, T, C, S> {
    type Item = Result<Vec<u8>, Error<S::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.budget > 0 && self.next < self.core.len() {
            let i = self.next;
            self.next += 1;
            match self.core.block(i) {
                Ok(Some(bytes)) => {
                    self.budget = self.budget.saturating_sub(bytes.len() as u64);
                    return Some(Ok(bytes));
                }
                Ok(None) => continue, // absent block: no-wait skip
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}

impl<T, C: Codec<T> + Clone, S: Store> Hypercore<T, C, S> {
    /// Capture a read-only, point-in-time [`Snapshot`] of the log at its current
    /// length. The snapshot owns an immutable copy of the present blocks `[0, len)`
    /// (their encoded bytes), the Merkle tree at that length, and the signed head —
    /// so it is **immune to any later mutation of this core** (append, truncate, or
    /// a truncate-and-rewrite): its [`length`](Snapshot::length) never changes and
    /// it keeps returning the blocks as they were at snapshot time, even after the
    /// core rewinds below the snapshot and re-appends different content over those
    /// indices. Ports upstream `snapshots.js`'s "snapshot does not change when
    /// original gets modified".
    ///
    /// Only **present** blocks are captured (a block dropped by
    /// [`clear`](Self::clear) cannot be snapshotted — there are no bytes to copy),
    /// so the snapshot reads `None` for an absent block, exactly like the core.
    /// Fallible only because reading the bytes to copy goes through the store.
    pub fn snapshot(&self) -> Result<Snapshot<T, C>, Error<S::Error>> {
        let length = self.tree.len();
        let mut blocks = BTreeMap::new();
        for i in 0..length {
            if self.presence.get(i) {
                if let Some(bytes) = self.store.get(i).map_err(Error::Storage)? {
                    blocks.insert(i, bytes);
                }
            }
        }
        Ok(Snapshot {
            length,
            fork: self.fork,
            head: self.head.clone(),
            tree: self.tree.clone(),
            blocks,
            codec: self.codec.clone(),
            _t: PhantomData,
        })
    }
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

impl<T, C: Codec<T>> Snapshot<T, C> {
    /// The snapshotted length — fixed for the life of the snapshot.
    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// The fork counter at snapshot time.
    pub fn fork(&self) -> u64 {
        self.fork
    }

    /// The signed head captured at snapshot time (`None` for an empty core).
    pub fn head(&self) -> Option<&SignedHead> {
        self.head.as_ref()
    }

    /// The Merkle root hash of the snapshotted prefix.
    pub fn root_hash(&self) -> Hash {
        self.tree.root_hash()
    }

    /// The raw (codec-encoded) bytes of block `index` as captured, or `None` past
    /// the snapshot's length or for a block that was absent at snapshot time. This
    /// is the unit the snapshot's [`proof`](Self::proof) authenticates.
    pub fn block(&self, index: u64) -> Option<&[u8]> {
        if index >= self.length {
            return None;
        }
        self.blocks.get(&index).map(Vec::as_slice)
    }

    /// Decode the value at `index`, or `None` past the snapshot's length (a no-wait
    /// read — upstream's out-of-range `get` throws `SNAPSHOT_NOT_AVAILABLE`; at L1
    /// we report absence as `None`, consistent with [`Hypercore::get`]).
    pub fn get(&self, index: u64) -> Result<Option<T>, codec::Error> {
        match self.block(index) {
            Some(bytes) => Ok(Some(self.codec.decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// A Merkle inclusion proof for `index` against the snapshot's captured head —
    /// so a snapshot block is independently verifiable ([`verify_block`]) even
    /// after the original has forked away.
    pub fn proof(&self, index: u64) -> Option<Proof> {
        self.tree.proof(index)
    }

    /// How much of this snapshot is still backed by `core`'s **current** signed
    /// head: the length of the longest prefix the snapshot and the live core still
    /// share. It equals the snapshot's length while the core's history still
    /// contains the snapshotted prefix, and drops when the core truncates below the
    /// snapshot (or rewrites a block within it) — upstream's `snapshot.signedLength`.
    ///
    /// Computed content-blind from the two trees' shared prefix
    /// ([`MerkleTree::lowest_common_ancestor`]), so it never inspects a payload and
    /// never exceeds [`length`](Self::length).
    pub fn signed_length<S: Store>(&self, core: &Hypercore<T, C, S>) -> u64 {
        self.tree.lowest_common_ancestor(&core.tree)
    }
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

impl ForkProof {
    /// Verify this is a genuine equivocation by `public`: both sides must be
    /// signed by `public` **at the same fork counter**, prove their block at
    /// `index`, and commit **different** bytes there. Returns `false` for anything
    /// else — a forged side, a cross-fork (legitimate-reorg) divergence, a
    /// consistent pair (same bytes), a tampered proof, or a mismatched index claim.
    pub fn verify(&self, public: &PublicKey) -> bool {
        self.head_a.fork == self.head_b.fork
            && verify_block(public, &self.head_a, self.index, &self.data_a, &self.proof_a)
            && verify_block(public, &self.head_b, self.index, &self.data_b, &self.proof_b)
            && self.data_a != self.data_b
    }
}

/// A verify-only replica of a [`Hypercore`]. It holds no secret key; it accepts
/// blocks accompanied by a proof against a signed head, verifies each, and
/// rebuilds an **identical** log — never trusting the sender.
pub struct Replica<T, C, S> {
    public: PublicKey,
    codec: C,
    store: S,
    tree: MerkleTree,
    head: Option<SignedHead>,
    _t: PhantomData<fn() -> T>,
}

impl<T, C: Codec<T>, S: Store> Replica<T, C, S> {
    pub fn new(public: PublicKey, codec: C, store: S) -> Self {
        Self {
            public,
            codec,
            store,
            tree: MerkleTree::new(),
            head: None,
            _t: PhantomData,
        }
    }

    pub fn len(&self) -> u64 {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn root_hash(&self) -> Hash {
        self.tree.root_hash()
    }

    /// The signed head we have fully replicated up to (if any).
    pub fn verified_head(&self) -> Option<&SignedHead> {
        self.head.as_ref()
    }

    /// Verify the next block (`index` must equal the current length) against
    /// `head`, then append it. Returns whether it was accepted; a rejected block
    /// is **not** stored.
    pub fn add_block(
        &mut self,
        head: &SignedHead,
        index: u64,
        enc: &[u8],
        proof: &Proof,
    ) -> Result<bool, Error<S::Error>> {
        if index != self.tree.len() {
            return Ok(false); // must apply in order
        }
        if !verify_block(&self.public, head, index, enc, proof) {
            return Ok(false);
        }
        self.store.put(index, enc).map_err(Error::Storage)?;
        self.tree.append(enc);
        if self.tree.len() == head.length && self.tree.root_hash() == head.root {
            self.head = Some(head.clone());
        }
        Ok(true)
    }

    /// Verify that `new_head` is a genuine **append-only extension** of this
    /// replica's current verified state, using a length-extension
    /// [`UpgradeProof`] — the gate a replica applies *before* fetching a longer
    /// head's blocks.
    ///
    /// [`Self::add_block`] verifies each block against the head it came with, but
    /// an inclusion proof only ties a block to *that* head's root. A writer that
    /// forked/rewrote old history produces a self-consistent longer head whose
    /// blocks all verify against its own (forked) root — so without this check a
    /// replica could be lured onto a forked history that contradicts what it
    /// already verified. `verify_upgrade` ties the longer head back to what we
    /// already trust: it folds the proof's fully-new nodes into our **own** roots
    /// and must rebuild `new_head.root`. A forked/rewritten prefix fails the fold.
    ///
    /// Returns `true` only if the author signed `new_head`, the proof bridges
    /// exactly from our current length (`old_len == len()`) to the new head
    /// (`new_len == new_head.length > len()`), and the fold from our trusted roots
    /// reconstructs `new_head.root`. It does **not** mutate the replica — apply
    /// the new blocks with [`Self::add_block`] (against `new_head`) afterward.
    pub fn verify_upgrade(&self, new_head: &SignedHead, proof: &UpgradeProof) -> bool {
        proof.old_len == self.tree.len()
            && proof.new_len == new_head.length
            && new_head.length > self.tree.len()
            && self
                .public
                .verify(&head_message(new_head.fork, new_head.length, &new_head.root), &new_head.sig)
            && proof.verify(&self.tree.roots(), &new_head.root)
    }

    /// Verify that `new_head` is a legitimate **reorg** this replica should
    /// follow: a *higher-fork* signed head whose history shares this replica's
    /// `[0, ancestors)` prefix and append-only-extends it. The cross-fork
    /// analogue of [`Self::verify_upgrade`] — pure (no mutation).
    ///
    /// Where [`Self::verify_upgrade`] handles a **same-fork** extension anchored
    /// on the replica's *entire* current head, a reorg is the author rewriting
    /// history under a bumped `fork` counter ([`Hypercore::truncate`]): readers
    /// follow the highest fork, so the new head shares only a **proper prefix**
    /// `[0, ancestors)` (the [lowest common ancestor]) and diverges after it. The
    /// gate re-anchors the same data-free [`UpgradeProof`] on the replica's own
    /// roots *at `ancestors`* (`tree.prefix_roots`): those roots are identical to
    /// the source's roots at that length **iff** the prefix is genuinely shared,
    /// so the fold reaches `new_head.root` only for a real shared ancestor.
    ///
    /// Returns `true` only if: we already trust a head; `new_head.fork` is
    /// **strictly greater** than ours (a same/lower fork is a stale head or an
    /// *equivocation* — an attack, see [`conflicting_heads`] — never a history to
    /// adopt); the author signed `new_head`; `ancestors <= len()` and
    /// `<= new_head.length`; and the prefix is authenticated:
    /// - `ancestors == new_head.length` — a **pure truncation**: the new head *is*
    ///   our prefix at `ancestors` (`prefix_root_hash` must equal `new_head.root`);
    ///   no `proof` needed.
    /// - `ancestors == 0` — **no shared prefix**: nothing to anchor (an upgrade
    ///   proof needs `old >= 1`), so the signed higher-fork head is adopted from
    ///   scratch and every refetched block is verified against it by
    ///   [`Self::add_block`]; no `proof` needed.
    /// - otherwise — `proof` must bridge exactly `ancestors -> new_head.length`
    ///   and fold our trusted prefix roots up to `new_head.root`.
    ///
    /// Soundness note on `ancestors`: the value is *authenticated*, not trusted —
    /// an **over-claim** (a larger `ancestors` than the true ancestor) names a
    /// prefix the replica holds but the new history does not, so the fold can't
    /// reach `new_head.root` and is rejected. An **under-claim** (smaller) is a
    /// genuine shorter shared prefix and is accepted; it only costs extra refetch
    /// (the maximal ancestor is the [`MerkleTree::lowest_common_ancestor`] binary
    /// search — a separate, efficiency concern). Either way the replica ends
    /// byte-identical to `new_head`.
    ///
    /// [lowest common ancestor]: merkle::MerkleTree::lowest_common_ancestor
    pub fn verify_reorg(
        &self,
        new_head: &SignedHead,
        ancestors: u64,
        proof: Option<&UpgradeProof>,
    ) -> bool {
        let cur = match &self.head {
            Some(h) => h,
            None => return false, // nothing trusted to reorg away from
        };
        if new_head.fork <= cur.fork {
            return false; // only a strictly higher fork is a reorg to follow
        }
        if !self
            .public
            .verify(&head_message(new_head.fork, new_head.length, &new_head.root), &new_head.sig)
        {
            return false;
        }
        if ancestors > self.tree.len() || ancestors > new_head.length {
            return false;
        }
        // Our own roots at `ancestors` are the trusted anchor — equal to the
        // source's roots there iff [0, ancestors) is genuinely shared.
        let anchor = match self.tree.prefix_roots(ancestors) {
            Some(r) => r,
            None => return false, // missing prefix nodes (not intact)
        };
        if ancestors == new_head.length {
            // Pure truncation: the new head must be exactly our prefix.
            self.tree.prefix_root_hash(ancestors) == Some(new_head.root)
        } else if ancestors == 0 {
            // No prefix to anchor: adopt the signed higher-fork head from scratch
            // (blocks are verified against it on refetch).
            true
        } else {
            match proof {
                Some(p) => {
                    p.old_len == ancestors
                        && p.new_len == new_head.length
                        && p.verify(&anchor, &new_head.root)
                }
                None => false,
            }
        }
    }

    /// Follow a reorg: verify `new_head` is a legitimate higher-fork rewrite that
    /// shares this replica's `[0, ancestors)` prefix (via [`Self::verify_reorg`]),
    /// then drop the divergent suffix, keeping that prefix. Returns `false` and
    /// leaves the replica **untouched** if verification fails.
    ///
    /// On success the replica is at length `ancestors`; fetch the new suffix
    /// `[ancestors, new_head.length)` with [`Self::add_block`] (against
    /// `new_head`) to finish. The shared prefix is preserved, not re-derived (the
    /// surviving nodes already equal the new history's prefix). If
    /// `ancestors == new_head.length` (a pure truncation) there is no suffix and
    /// `new_head` becomes the verified head immediately.
    pub fn reorg(&mut self, new_head: &SignedHead, ancestors: u64, proof: Option<&UpgradeProof>) -> bool {
        if !self.verify_reorg(new_head, ancestors, proof) {
            return false;
        }
        self.tree.truncate(ancestors); // keep the shared prefix (no-op if == len)
        if self.tree.len() == new_head.length && self.tree.root_hash() == new_head.root {
            self.head = Some(new_head.clone()); // pure truncation: reorg complete
        } else {
            self.head = None; // suffix refetch pending — no fully-verified head yet
        }
        true
    }

    /// Decode the value at `index`, or `None`.
    pub fn get(&self, index: u64) -> Result<Option<T>, Error<S::Error>> {
        if index >= self.tree.len() {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(index)
            .map_err(Error::Storage)?
            .ok_or(Error::Corrupt)?;
        Ok(Some(self.codec.decode(&bytes).map_err(Error::Codec)?))
    }
}


#[cfg(test)]
mod tests;

/// End-to-end browser persistence: a real Hypercore over the OPFS-backed LogStore,
/// persisted and reconstituted across a close+reopen in a Web Worker (wasm + opfs).
#[cfg(all(target_arch = "wasm32", feature = "opfs", test))]
mod opfs_browser_tests;
