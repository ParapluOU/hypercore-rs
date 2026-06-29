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
    /// A stored block was missing where the tree says one exists.
    Corrupt,
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
mod tests {
    use super::*;
    use codec::{Bytes, U64};
    use storage::MemoryStore;

    fn author(seed: u8) -> SecretKey {
        SecretKey::from_seed(&[seed; 32])
    }

    #[test]
    fn append_get_roundtrip() {
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(1), Bytes, MemoryStore::new());
        assert!(core.is_empty());
        for i in 0..10u8 {
            let idx = core.append(&vec![i, i + 1, i + 2]).unwrap();
            assert_eq!(idx, i as u64);
        }
        assert_eq!(core.len(), 10);
        for i in 0..10u8 {
            assert_eq!(core.get(i as u64).unwrap(), Some(vec![i, i + 1, i + 2]));
        }
        assert_eq!(core.get(10).unwrap(), None);
    }

    #[test]
    fn head_is_signed_and_consistent() {
        let mut core = Hypercore::<u64, _, _>::new(author(2), U64, MemoryStore::new());
        assert!(core.verify_head()); // empty core
        core.append(&7).unwrap();
        core.append(&8).unwrap();
        assert!(core.verify_head());
        let head = core.head().unwrap();
        assert_eq!(head.length, 2);
    }

    #[test]
    fn blocks_verify_against_signed_head() {
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(3), Bytes, MemoryStore::new());
        let blocks: Vec<Vec<u8>> = (0..7).map(|i| format!("b{i}").into_bytes()).collect();
        for b in &blocks {
            core.append(b).unwrap();
        }
        let head = core.head().unwrap().clone();
        let pk = core.public_key();

        for i in 0..blocks.len() as u64 {
            // The verifier checks the *encoded* (stored) bytes, then decodes.
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(verify_block(&pk, &head, i, &enc, &proof), "honest block verifies");

            // tampered data
            assert!(!verify_block(&pk, &head, i, b"forged-encoded-bytes", &proof));
            // wrong author key
            assert!(!verify_block(&author(99).public(), &head, i, &enc, &proof));
            // wrong index claim (proof.block != claimed index)
            let wrong = (i + 1) % blocks.len() as u64;
            assert!(!verify_block(&pk, &head, wrong, &enc, &proof));
        }
    }

    #[test]
    fn forged_head_does_not_verify_under_real_key() {
        // Author A's head must not verify under author B's key.
        let mut a = Hypercore::<u64, _, _>::new(author(4), U64, MemoryStore::new());
        a.append(&1).unwrap();
        let head = a.head().unwrap();
        let b_pub = author(5).public();
        assert!(!b_pub.verify(&head_message(head.fork, head.length, &head.root), &head.sig));
    }

    #[test]
    fn deterministic_log() {
        // Same author + same appends => identical signed head (ed25519 is deterministic).
        let build = || {
            let mut c = Hypercore::<u64, _, _>::new(author(6), U64, MemoryStore::new());
            c.append(&100).unwrap();
            c.append(&200).unwrap();
            c.head().unwrap().clone()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn replica_ends_byte_identical() {
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(10), Bytes, MemoryStore::new());
        let data: Vec<Vec<u8>> = (0..9).map(|i| format!("blk-{i}").into_bytes()).collect();
        for d in &data {
            src.append(d).unwrap();
        }
        let head = src.head().unwrap().clone();

        let mut rep = Replica::<Vec<u8>, _, _>::new(src.public_key(), Bytes, MemoryStore::new());
        for i in 0..data.len() as u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap(), "verified block accepted");
        }
        assert_eq!(rep.len(), src.len());
        assert_eq!(rep.root_hash(), head.root, "replica root == source signed root");
        assert!(rep.verified_head().is_some());
        for i in 0..data.len() as u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap(), "decoded values match");
        }
    }

    #[test]
    fn replica_rejects_bad_and_out_of_order() {
        let mut src = Hypercore::<u64, _, _>::new(author(11), U64, MemoryStore::new());
        for v in [1u64, 2, 3] {
            src.append(&v).unwrap();
        }
        let head = src.head().unwrap().clone();
        let mut rep = Replica::<u64, _, _>::new(src.public_key(), U64, MemoryStore::new());

        let p1 = src.proof(1).unwrap();
        let e1 = src.block(1).unwrap().unwrap();
        // out of order: index 1 before 0
        assert!(!rep.add_block(&head, 1, &e1, &p1).unwrap());

        // index 0 with tampered bytes
        let p0 = src.proof(0).unwrap();
        assert!(!rep.add_block(&head, 0, b"garbage", &p0).unwrap());
        assert_eq!(rep.len(), 0, "nothing stored on rejection");

        // honest 0 then 1
        let e0 = src.block(0).unwrap().unwrap();
        assert!(rep.add_block(&head, 0, &e0, &p0).unwrap());
        assert!(rep.add_block(&head, 1, &e1, &p1).unwrap());
        assert_eq!(rep.len(), 2);
    }

    #[test]
    fn add_block_binds_proof_to_the_specific_head() {
        // An inclusion proof is bound to the root of the head it was generated
        // against: it carries that tree's root nodes. An honest block+proof from
        // one head, presented under a *different* head **from the same author**
        // (a fork at the same length, or a longer honest head — both validly
        // signed), must be rejected — the proof can't fold to the other head's
        // root — and nothing is stored. Positive-path replica tests never
        // exercise this; the audit (after iter 21) flagged the gap.
        let core_a = core_with(70, &["a", "b", "c", "d", "e"]); // root R_a
        let core_f = core_with(70, &["a", "b", "c", "d", "X"]); // root R_f (block 4 differs)
        let pk = core_a.public_key();
        assert_eq!(pk, core_f.public_key(), "same seed => same author");
        let head_a = core_a.head().unwrap().clone();
        let head_f = core_f.head().unwrap().clone();
        assert_eq!(head_a.length, head_f.length);
        assert_ne!(head_a.root, head_f.root, "same length, different root => the binding matters");

        let enc0 = core_a.block(0).unwrap().unwrap();
        let proof0_a = core_a.proof(0).unwrap();
        assert_eq!(enc0, core_f.block(0).unwrap().unwrap(), "block 0 ('a') is shared");

        // Positive control: under its own head, block 0 is accepted.
        {
            let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
            assert!(rep.add_block(&head_a, 0, &enc0, &proof0_a).unwrap(), "honest block accepted");
        }

        // Same-length cross-head: the honest block+proof bound to head_a is
        // refused under the forked head_f (proof0_a carries head_a's other root,
        // block 4 = 'e', which can't fold to head_f.root built from 'X').
        {
            let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
            assert!(
                !rep.add_block(&head_f, 0, &enc0, &proof0_a).unwrap(),
                "proof bound to head_a rejected under the forked head_f"
            );
            assert_eq!(rep.len(), 0, "nothing stored on a cross-head rejection");
            assert!(rep.verified_head().is_none());
        }

        // Different-length cross-head, both directions: a length-5 proof is
        // refused under a longer honest head, and a length-7 proof under the
        // shorter head — the root structure differs either way.
        let core_long = core_with(70, &["a", "b", "c", "d", "e", "f", "g"]); // length 7
        let head_long = core_long.head().unwrap().clone();
        assert_ne!(head_a.root, head_long.root);
        let proof0_long = core_long.proof(0).unwrap();
        {
            let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
            assert!(
                !rep.add_block(&head_long, 0, &enc0, &proof0_a).unwrap(),
                "length-5 proof rejected under the length-7 head"
            );
            assert_eq!(rep.len(), 0);
            assert!(
                !rep.add_block(&head_a, 0, &enc0, &proof0_long).unwrap(),
                "length-7 proof rejected under the length-5 head"
            );
            assert_eq!(rep.len(), 0);
        }
    }

    #[test]
    fn add_block_rejects_wrong_author() {
        // A replica keyed to author A must reject a fully-valid, internally-honest
        // log signed by a *different* author B: A's key never signed B's head, so
        // the head-signature check in `verify_block` fails and no block is stored —
        // even though B's own proofs verify under B's key. (Negative-path gap, audit
        // follow-up after iter 21.)
        let a_pub = author(71).public();
        let b_core = core_with(72, &["a", "b", "c"]); // a DIFFERENT author (seed 72)
        assert_ne!(a_pub, b_core.public_key(), "distinct authors");
        let b_head = b_core.head().unwrap().clone();

        // Sanity: B's log is internally honest — every block verifies under B's key.
        for i in 0..3u64 {
            let enc = b_core.block(i).unwrap().unwrap();
            let proof = b_core.proof(i).unwrap();
            assert!(verify_block(&b_core.public_key(), &b_head, i, &enc, &proof), "honest under B");
            // ...but NOT under A's key (the head signature is B's).
            assert!(!verify_block(&a_pub, &b_head, i, &enc, &proof), "not honest under A");
        }

        // A replica keyed to A refuses B's first block and stores nothing.
        let mut rep = Replica::<Vec<u8>, _, _>::new(a_pub, Bytes, MemoryStore::new());
        let enc0 = b_core.block(0).unwrap().unwrap();
        let proof0 = b_core.proof(0).unwrap();
        assert!(
            !rep.add_block(&b_head, 0, &enc0, &proof0).unwrap(),
            "block from another author refused"
        );
        assert_eq!(rep.len(), 0, "nothing stored");
        assert!(rep.verified_head().is_none());
    }

    // ---- verified length-extension replication (merkle upgrade proof, ADR-0020) ----

    #[test]
    fn replica_upgrades_to_longer_head() {
        // A replica fully replicates a length-5 log, then accepts a *verified*
        // append-only extension to length 9 and fetches only the new blocks.
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(30), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            src.append(&blk(s)).unwrap();
        }
        let head5 = src.head().unwrap().clone();
        let pk = src.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.verified_head(), Some(&head5));

        // The source extends the log.
        for s in ["f", "g", "h", "i"] {
            src.append(&blk(s)).unwrap();
        }
        let head9 = src.head().unwrap().clone();

        // Before fetching, the replica verifies the longer head is an honest
        // extension of what it already trusts — no block data needed.
        let up = src.upgrade_proof(5, 9).unwrap();
        assert!(!up.nodes.is_empty(), "extension supplies new subtree nodes");
        assert!(rep.verify_upgrade(&head9, &up), "honest extension accepted");

        // Then it fetches only the new blocks [5, 9) against the new head and
        // ends byte-identical to the source at length 9.
        for i in 5..9u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head9, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 9);
        assert_eq!(rep.root_hash(), head9.root, "replica root == new signed root");
        assert_eq!(rep.verified_head(), Some(&head9));
        for i in 0..9u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap(), "decoded values match");
        }
    }

    #[test]
    fn replica_rejects_forked_upgrade() {
        // A replica trusting the honest length-5 prefix must reject a longer head
        // from a forking writer (same author) that rewrote an old block: the
        // upgrade proof's new nodes can't fold into the honest roots to reach the
        // forked root. This is the anti-fork guarantee at the replication level.
        let mut honest = Hypercore::<Vec<u8>, _, _>::new(author(31), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            honest.append(&blk(s)).unwrap();
        }
        let head5 = honest.head().unwrap().clone();
        let pk = honest.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = honest.block(i).unwrap().unwrap();
            let proof = honest.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }

        // Forking writer: same author seed, but block 2 ('c' -> 'Z') is rewritten,
        // then the log is extended to length 9.
        let mut forked = Hypercore::<Vec<u8>, _, _>::new(author(31), Bytes, MemoryStore::new());
        for s in ["a", "b", "Z", "d", "e", "f", "g", "h", "i"] {
            forked.append(&blk(s)).unwrap();
        }
        let forked_head9 = forked.head().unwrap().clone();
        let forked_up = forked.upgrade_proof(5, 9).unwrap();

        // The forked head *is* signed by the same author (signature alone passes)...
        assert!(pk.verify(
            &head_message(forked_head9.fork, forked_head9.length, &forked_head9.root),
            &forked_head9.sig
        ));
        // ...but the replica's honest roots can't fold the forked extension up to
        // the forked root, so the upgrade is refused and the replica is untouched.
        assert!(
            !rep.verify_upgrade(&forked_head9, &forked_up),
            "forked extension rejected against the honest prefix"
        );
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.verified_head(), Some(&head5));
    }

    #[test]
    fn verify_upgrade_rejects_malformed_or_tampered() {
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(32), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e", "f", "g"] {
            src.append(&blk(s)).unwrap();
        }
        let pk = src.public_key();

        // The replica replicates only the first 4 blocks (under the length-4 head).
        let mut early = Hypercore::<Vec<u8>, _, _>::new(author(32), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d"] {
            early.append(&blk(s)).unwrap();
        }
        let head4 = early.head().unwrap().clone();
        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..4u64 {
            let enc = early.block(i).unwrap().unwrap();
            let proof = early.proof(i).unwrap();
            assert!(rep.add_block(&head4, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 4);

        let head7 = src.head().unwrap().clone();
        let up = src.upgrade_proof(4, 7).unwrap();
        assert!(!up.nodes.is_empty());
        assert!(rep.verify_upgrade(&head7, &up), "honest baseline accepted");

        // Tampered new-head root: the head signature no longer verifies.
        let mut bad_head = head7.clone();
        bad_head.root[0] ^= 0xff;
        assert!(!rep.verify_upgrade(&bad_head, &up));

        // Tampered proof node: the fold no longer reaches the new root.
        let mut bad_up = up.clone();
        bad_up.nodes[0].hash[0] ^= 0xff;
        assert!(!rep.verify_upgrade(&head7, &bad_up));

        // Proof bridging from the wrong old length (not the replica's length).
        let up_wrong_old = src.upgrade_proof(3, 7).unwrap();
        assert!(!rep.verify_upgrade(&head7, &up_wrong_old), "old_len must equal replica length");

        // Proof whose new_len disagrees with the head's length.
        let up_wrong_new = src.upgrade_proof(4, 6).unwrap();
        assert!(!rep.verify_upgrade(&head7, &up_wrong_new), "new_len must equal head length");

        // A length-7 head signed by a *different* author is refused.
        let other_head = {
            let mut o = Hypercore::<Vec<u8>, _, _>::new(author(33), Bytes, MemoryStore::new());
            for s in ["a", "b", "c", "d", "e", "f", "g"] {
                o.append(&blk(s)).unwrap();
            }
            o.head().unwrap().clone()
        };
        assert!(!rep.verify_upgrade(&other_head, &up), "head signed by another author refused");
    }

    // ---- batch / atomic append (upstream `batch.js` / `atomic.js`) ----

    fn blk(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn batch_stages_without_touching_log() {
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(20), Bytes, MemoryStore::new());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();

        let mut b = core.batch();
        core.stage(&mut b, &blk("de"));
        core.stage(&mut b, &blk("fg"));

        // The log itself is untouched while the batch is open.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);

        // The batch presents a length of 5 and reads both committed and staged.
        assert_eq!(b.base(), 3);
        assert_eq!(b.staged(), 2);
        assert_eq!(b.length(), 5);
        assert_eq!(core.batch_get(&b, 0).unwrap(), Some(blk("a"))); // committed
        assert_eq!(core.batch_get(&b, 2).unwrap(), Some(blk("c"))); // committed
        assert_eq!(core.batch_get(&b, 3).unwrap(), Some(blk("de"))); // staged
        assert_eq!(core.batch_get(&b, 4).unwrap(), Some(blk("fg"))); // staged
        assert_eq!(core.batch_get(&b, 5).unwrap(), None); // past the batch

        // Committing advances the log to the batch length.
        assert_eq!(core.commit(b).unwrap(), Some(5));
        assert_eq!(core.len(), 5);
        assert_eq!(core.get(3).unwrap(), Some(blk("de")));
        assert_eq!(core.get(4).unwrap(), Some(blk("fg")));
    }

    #[test]
    fn commit_equals_sequential_appends() {
        // Same author + same blocks: one committed batch == N single appends,
        // down to the signed head (root, length, signature).
        let all = ["a", "b", "c", "d", "e"];

        let mut seq = Hypercore::<Vec<u8>, _, _>::new(author(21), Bytes, MemoryStore::new());
        for s in all {
            seq.append(&blk(s)).unwrap();
        }

        let mut bat = Hypercore::<Vec<u8>, _, _>::new(author(21), Bytes, MemoryStore::new());
        for s in &all[..3] {
            bat.append(&blk(s)).unwrap();
        }
        let mut b = bat.batch();
        for s in &all[3..] {
            bat.stage(&mut b, &blk(s));
        }
        assert_eq!(bat.commit(b).unwrap(), Some(5));

        assert_eq!(bat.head().unwrap(), seq.head().unwrap(), "single head identical");
        for i in 0..5 {
            assert_eq!(bat.get(i).unwrap(), seq.get(i).unwrap());
        }
    }

    #[test]
    fn committed_batch_blocks_verify_and_replicate() {
        // A batch is invisible to verifiers: every block proves against the one
        // signed head, and a replica rebuilds the core byte-identically.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(22), Bytes, MemoryStore::new());
        core.append(&blk("g0")).unwrap();
        let mut b = core.batch();
        for s in ["g1", "g2", "g3"] {
            core.stage(&mut b, &blk(s));
        }
        core.commit(b).unwrap();

        let head = core.head().unwrap().clone();
        let pk = core.public_key();
        for i in 0..core.len() {
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(verify_block(&pk, &head, i, &enc, &proof));
        }

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..core.len() {
            let enc = core.block(i).unwrap().unwrap();
            let proof = core.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.root_hash(), head.root);
        assert_eq!(rep.len(), core.len());
    }

    #[test]
    fn stale_base_batch_is_rejected() {
        // Open a batch, then append to the log directly: the batch's base is now
        // stale, so commit is refused and the direct append stands.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(23), Bytes, MemoryStore::new());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let mut b = core.batch(); // base = 3
        core.stage(&mut b, &blk("from-batch"));

        core.append(&blk("from-core")).unwrap(); // log now length 4
        let head_after_core = core.head().unwrap().clone();

        assert_eq!(core.commit(b).unwrap(), None, "stale-base batch rejected");
        assert_eq!(core.len(), 4, "log unchanged by the rejected commit");
        assert_eq!(core.get(3).unwrap(), Some(blk("from-core")));
        assert_eq!(core.head().unwrap(), &head_after_core);
    }

    #[test]
    fn empty_batch_is_noop() {
        let mut core = Hypercore::<u64, _, _>::new(author(24), U64, MemoryStore::new());
        core.append(&1).unwrap();
        let head_before = core.head().unwrap().clone();
        let b = core.batch();
        assert!(b.is_empty());
        assert_eq!(core.commit(b).unwrap(), Some(1));
        assert_eq!(core.len(), 1);
        assert_eq!(core.head().unwrap(), &head_before);
    }

    #[test]
    fn dropped_batch_leaves_log_unchanged() {
        let mut core = Hypercore::<u64, _, _>::new(author(25), U64, MemoryStore::new());
        core.append(&10).unwrap();
        let head_before = core.head().unwrap().clone();
        {
            let mut b = core.batch();
            core.stage(&mut b, &20);
            core.stage(&mut b, &30);
            // b is dropped here without commit
        }
        assert_eq!(core.len(), 1);
        assert_eq!(core.head().unwrap(), &head_before);
    }

    /// A store that injects a failure on the `put` (and optionally the `delete`)
    /// at a chosen key, to prove commit atomicity. Otherwise an in-memory map.
    #[derive(Default)]
    struct FaultyStore {
        inner: MemoryStore,
        fail_at: Option<u64>,
        fail_delete_at: Option<u64>,
    }
    impl Store for FaultyStore {
        type Error = &'static str;
        fn put(&mut self, key: u64, value: &[u8]) -> Result<(), &'static str> {
            if self.fail_at == Some(key) {
                return Err("injected put failure");
            }
            self.inner.put(key, value).unwrap();
            Ok(())
        }
        fn get(&self, key: u64) -> Result<Option<Vec<u8>>, &'static str> {
            Ok(self.inner.get(key).unwrap())
        }
        fn delete(&mut self, key: u64) -> Result<(), &'static str> {
            if self.fail_delete_at == Some(key) {
                return Err("injected delete failure");
            }
            self.inner.delete(key).unwrap();
            Ok(())
        }
        fn len(&self) -> Result<u64, &'static str> {
            Ok(self.inner.len().unwrap())
        }
    }

    /// The signed head of a freshly-built log of `blocks` under author `seed` —
    /// the canonical state a fault-then-recover commit path must land on
    /// (ed25519 is deterministic, so head equality is exact).
    fn head_of(seed: u8, blocks: &[&str]) -> SignedHead {
        let mut c = Hypercore::<Vec<u8>, _, _>::new(author(seed), Bytes, MemoryStore::new());
        for b in blocks {
            c.append(&blk(b)).unwrap();
        }
        c.head().unwrap().clone()
    }

    #[test]
    fn failed_commit_is_atomic() {
        // Append a, b, c cleanly, then arm a storage failure at index 4 (the 2nd
        // staged block of a 3-block batch). The commit must fail, roll back its
        // partial write at index 3, and leave the log's logical state untouched.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(26), Bytes, FaultyStore::default());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();
        core.store.fail_at = Some(4);

        let mut b = core.batch(); // base = 3, blocks at 3,4,5
        for s in ["d", "e", "f"] {
            core.stage(&mut b, &blk(s));
        }
        assert_eq!(core.commit(b), Err(Error::Storage("injected put failure")));

        // Logical state unchanged: length, head, and reads all intact; the
        // rolled-back partial write at index 3 is gone.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);
        assert_eq!(core.store.get(3).unwrap(), None, "partial write rolled back");
        assert_eq!(core.store.len().unwrap(), 3, "no orphan blocks left behind");

        // Recovery: clear the fault and the batch commits cleanly to the right state.
        core.store.fail_at = None;
        let mut b2 = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b2, &blk(s));
        }
        assert_eq!(core.commit(b2).unwrap(), Some(6));
        assert_eq!(core.get(5).unwrap(), Some(blk("f")));
    }

    #[test]
    fn commit_fault_on_first_staged_block_is_atomic() {
        // Fault on the *first* staged block (index 3 of a 3-block batch): the
        // commit aborts before any write succeeds, so there is nothing to roll
        // back — storage is left pristine, not just logically unchanged.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(50), Bytes, FaultyStore::default());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();
        core.store.fail_at = Some(3); // the first staged index

        let mut b = core.batch(); // base = 3, blocks at 3,4,5
        for s in ["d", "e", "f"] {
            core.stage(&mut b, &blk(s));
        }
        assert_eq!(core.commit(b), Err(Error::Storage("injected put failure")));

        // Nothing was written, nothing to roll back: logical state and storage
        // are both exactly as before the batch.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);
        assert_eq!(core.store.get(3).unwrap(), None, "no partial write at all");
        assert_eq!(core.store.len().unwrap(), 3, "storage untouched");

        // Recovery lands on the canonical six-block head (byte-identical).
        core.store.fail_at = None;
        let mut b2 = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b2, &blk(s));
        }
        assert_eq!(core.commit(b2).unwrap(), Some(6));
        assert_eq!(core.get(5).unwrap(), Some(blk("f")));
        assert_eq!(core.head().unwrap(), &head_of(50, &["a", "b", "c", "d", "e", "f"]));
    }

    #[test]
    fn commit_fault_on_last_staged_block_rolls_back_all() {
        // Fault on the *last* staged block (index 5): the two earlier successful
        // writes (3, 4) must both be rolled back, leaving no orphans.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(51), Bytes, FaultyStore::default());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();
        core.store.fail_at = Some(5); // the last staged index

        let mut b = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b, &blk(s));
        }
        assert_eq!(core.commit(b), Err(Error::Storage("injected put failure")));

        // Both successful partial writes (3, 4) were rolled back.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);
        assert_eq!(core.store.get(3).unwrap(), None, "first partial write rolled back");
        assert_eq!(core.store.get(4).unwrap(), None, "second partial write rolled back");
        assert_eq!(core.store.len().unwrap(), 3, "no orphan blocks left behind");

        // Recovery lands on the canonical six-block head (byte-identical).
        core.store.fail_at = None;
        let mut b2 = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b2, &blk(s));
        }
        assert_eq!(core.commit(b2).unwrap(), Some(6));
        assert_eq!(core.get(5).unwrap(), Some(blk("f")));
        assert_eq!(core.head().unwrap(), &head_of(51, &["a", "b", "c", "d", "e", "f"]));
    }

    #[test]
    fn commit_rollback_tolerates_delete_failure() {
        // Fault the last `put` (index 5) *and* the rollback `delete` of the first
        // written block (index 3). The rollback's delete error is swallowed
        // (`let _ = store.delete(..)`), so the commit still reports the original
        // *put* failure — and, critically, the log's *logical* state never advances
        // even though one orphan block is physically left behind. A later commit
        // overwrites it.
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(52), Bytes, FaultyStore::default());
        for s in ["a", "b", "c"] {
            core.append(&blk(s)).unwrap();
        }
        let head_before = core.head().unwrap().clone();
        core.store.fail_at = Some(5); // last staged put fails
        core.store.fail_delete_at = Some(3); // rollback of index 3 also fails

        let mut b = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b, &blk(s));
        }
        // The *put* error surfaces, not the secondary delete error.
        assert_eq!(core.commit(b), Err(Error::Storage("injected put failure")));

        // Logical state is still atomic: length, head, and reads are untouched.
        assert_eq!(core.len(), 3);
        assert_eq!(core.head().unwrap(), &head_before);
        assert_eq!(core.get(3).unwrap(), None);

        // Physical reality: index 4's rollback succeeded, but index 3's delete
        // failed, so exactly one unreachable orphan remains — the *encoded* block
        // (codec adds a varint length prefix), never exposed by the length-gated
        // read API.
        assert_eq!(
            core.store.get(3).unwrap(),
            Some(Bytes.encode(&blk("d"))),
            "orphan survives the failed delete"
        );
        assert_eq!(core.store.get(4).unwrap(), None, "index 4 rolled back cleanly");
        assert_eq!(core.store.len().unwrap(), 4, "exactly one orphan left behind");

        // Recovery: clear both faults; the commit overwrites the orphan and lands
        // byte-identically on the canonical six-block head — no stray keys remain.
        core.store.fail_at = None;
        core.store.fail_delete_at = None;
        let mut b2 = core.batch();
        for s in ["d", "e", "f"] {
            core.stage(&mut b2, &blk(s));
        }
        assert_eq!(core.commit(b2).unwrap(), Some(6));
        assert_eq!(core.get(3).unwrap(), Some(blk("d")));
        assert_eq!(core.get(5).unwrap(), Some(blk("f")));
        assert_eq!(core.head().unwrap(), &head_of(52, &["a", "b", "c", "d", "e", "f"]));
        assert_eq!(core.store.len().unwrap(), 6, "orphan overwritten; no stray keys");
    }

    // ---- fork detection (upstream `conflicts.js`, L1) ----

    type ByteCore = Hypercore<Vec<u8>, Bytes, MemoryStore>;

    fn core_with(seed: u8, blocks: &[&str]) -> ByteCore {
        let mut c = Hypercore::<Vec<u8>, _, _>::new(author(seed), Bytes, MemoryStore::new());
        for b in blocks {
            c.append(&blk(b)).unwrap();
        }
        c
    }

    /// Assemble a [`ForkProof`] at `index` from two cores (each supplies its own
    /// signed head, block bytes, and inclusion proof at that index).
    fn fork_proof_at(index: u64, a: &ByteCore, b: &ByteCore) -> ForkProof {
        ForkProof {
            index,
            head_a: a.head().unwrap().clone(),
            data_a: a.block(index).unwrap().unwrap(),
            proof_a: a.proof(index).unwrap(),
            head_b: b.head().unwrap().clone(),
            data_b: b.block(index).unwrap().unwrap(),
            proof_b: b.proof(index).unwrap(),
        }
    }

    #[test]
    fn forking_writer_is_detected() {
        // Same author (seed 40), two logs sharing the prefix [a,b,c,d] but
        // diverging at index 4 — mirrors conflicts.js (a=[..e], c=[..f]).
        let a = core_with(40, &["a", "b", "c", "d", "e"]);
        let c = core_with(40, &["a", "b", "c", "d", "f"]);
        let pk = a.public_key();
        assert_eq!(pk, c.public_key(), "same seed => same author key");

        // Both heads are length 5 with different roots: a proof-free fork.
        let ha = a.head().unwrap();
        let hc = c.head().unwrap();
        assert_eq!(ha.length, hc.length);
        assert_ne!(ha.root, hc.root);
        assert!(conflicting_heads(&pk, ha, hc), "same-length conflicting heads = fork");

        // And the per-index fork proof at the divergence (block 4: 'e' vs 'f').
        let fork = fork_proof_at(4, &a, &c);
        assert!(fork.verify(&pk), "per-index fork proof verifies");
    }

    #[test]
    fn honest_extension_is_not_a_fork() {
        // A length-5 log and an honest length-7 continuation by the same author:
        // shared blocks agree, so neither detector flags a fork.
        let short = core_with(41, &["a", "b", "c", "d", "e"]);
        let long = core_with(41, &["a", "b", "c", "d", "e", "f", "g"]);
        let pk = short.public_key();

        // Different lengths => conflicting_heads never flags (it judges equal lengths only).
        assert!(!conflicting_heads(&pk, short.head().unwrap(), long.head().unwrap()));

        // A "fork proof" over any shared index has identical data on both sides => not a fork.
        for i in 0..5u64 {
            let not_fork = fork_proof_at(i, &short, &long);
            assert_eq!(not_fork.data_a, not_fork.data_b, "shared block agrees at {i}");
            assert!(!not_fork.verify(&pk), "consistent block is not a fork (i={i})");
        }
    }

    #[test]
    fn identical_logs_do_not_conflict() {
        // Same author, same appends => identical deterministic heads => no conflict.
        let a = core_with(42, &["x", "y", "z"]);
        let b = core_with(42, &["x", "y", "z"]);
        let pk = a.public_key();
        assert_eq!(a.head().unwrap(), b.head().unwrap());
        assert!(!conflicting_heads(&pk, a.head().unwrap(), b.head().unwrap()));
    }

    #[test]
    fn fork_proof_rejects_forgery() {
        // Diverge at index 1 (block 'b' vs 'Z') in a 4-block log, so the block-1
        // inclusion proof carries interior siblings to tamper with.
        let a = core_with(43, &["a", "b", "c", "d"]);
        let c = core_with(43, &["a", "Z", "c", "d"]);
        let pk = a.public_key();
        let good = fork_proof_at(1, &a, &c);
        assert!(good.verify(&pk));
        assert!(!good.proof_a.siblings.is_empty(), "block 1 proof has siblings");

        // Wrong author key: neither head is signed by it.
        assert!(!good.verify(&author(99).public()));

        // Tampered data on one side: its proof no longer matches the head root.
        let mut bad_data = good.clone();
        bad_data.data_a = blk("zzz");
        assert!(!bad_data.verify(&pk));

        // Tampered proof sibling on one side.
        let mut bad_proof = good.clone();
        bad_proof.proof_a.siblings[0].hash[0] ^= 0xff;
        assert!(!bad_proof.verify(&pk));

        // Tampered head: mutating the signed root invalidates the head's signature.
        let mut bad_head = good.clone();
        bad_head.head_a.root[0] ^= 0xff;
        assert!(!bad_head.verify(&pk));

        // Mismatched index claim: the proofs are for block 1, not 0.
        let mut wrong_index = good.clone();
        wrong_index.index = 0;
        assert!(!wrong_index.verify(&pk));
    }

    #[test]
    fn different_authors_are_not_a_fork() {
        // Two independent authors with differing length-3 logs are NOT a fork —
        // a fork is one author signing two histories, not two authors disagreeing.
        let a = core_with(44, &["a", "b", "c"]);
        let b = core_with(45, &["a", "b", "d"]); // different author and content
        assert_ne!(a.public_key(), b.public_key());

        // Neither key validates the other's head, so no same-length conflict.
        assert!(!conflicting_heads(&a.public_key(), a.head().unwrap(), b.head().unwrap()));
        assert!(!conflicting_heads(&b.public_key(), a.head().unwrap(), b.head().unwrap()));

        // A fork proof built across the two cores fails under either key — one
        // side is always signed by the other author.
        let cross = fork_proof_at(2, &a, &b);
        assert!(!cross.verify(&a.public_key()));
        assert!(!cross.verify(&b.public_key()));
    }

    // ---- truncate + fork counter (upstream `core.js` "append and truncate") ----

    #[test]
    fn append_and_truncate_tracks_fork_and_byte_length() {
        // Ports core.js "core - append and truncate": each truncate bumps the
        // fork counter and shrinks byteLength; lastTruncation records {from,to}
        // and the next append clears it. (byteLength is the *encoded* prefix size
        // — the bytes the tree commits — so we compare to a fresh prefix core
        // rather than raw payload lengths.)
        let blen = |items: &[&str]| -> u64 {
            let mut c = Hypercore::<Vec<u8>, _, _>::new(author(50), Bytes, MemoryStore::new());
            for s in items {
                c.append(&blk(s)).unwrap();
            }
            c.byte_length()
        };

        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(50), Bytes, MemoryStore::new());
        for s in ["hello", "world", "fo", "ooo"] {
            core.append(&blk(s)).unwrap();
        }
        assert_eq!(core.len(), 4);
        assert_eq!(core.byte_length(), blen(&["hello", "world", "fo", "ooo"]));
        assert_eq!(core.fork(), 0);
        assert_eq!(core.last_truncation(), None);
        assert!(core.verify_head());

        assert_eq!(core.truncate(3), Some(Truncation { from: 4, to: 3 }));
        assert_eq!(core.last_truncation(), Some(Truncation { from: 4, to: 3 }));
        assert_eq!(core.len(), 3);
        assert_eq!(core.byte_length(), blen(&["hello", "world", "fo"]));
        assert_eq!(core.fork(), 1);
        assert!(core.verify_head(), "head consistent after truncate");

        for s in ["a", "b", "c", "d"] {
            core.append(&blk(s)).unwrap();
        }
        assert_eq!(core.last_truncation(), None, "append clears lastTruncation");
        assert_eq!(core.len(), 7);

        assert_eq!(core.truncate(3), Some(Truncation { from: 7, to: 3 }));
        assert_eq!(core.fork(), 2);
        assert_eq!(core.len(), 3);
        assert_eq!(core.byte_length(), blen(&["hello", "world", "fo"]));

        assert_eq!(core.truncate(2), Some(Truncation { from: 3, to: 2 }));
        assert_eq!(core.fork(), 3);
        assert_eq!(core.len(), 2);
        assert_eq!(core.byte_length(), blen(&["hello", "world"]));

        // append-then-truncate cycles, each bumping fork by exactly one — mirrors
        // the upstream fork progression up to 7.
        let mut expect_fork = 3u64;
        for _ in 0..4 {
            core.append(&blk("a")).unwrap();
            assert_eq!(core.last_truncation(), None);
            assert_eq!(core.truncate(2), Some(Truncation { from: 3, to: 2 }));
            expect_fork += 1;
            assert_eq!(core.fork(), expect_fork);
            assert_eq!(core.len(), 2);
            assert_eq!(core.byte_length(), blen(&["hello", "world"]));
        }
        assert_eq!(core.fork(), 7, "seven truncations => fork 7");

        // no-op truncates change nothing.
        assert_eq!(core.truncate(2), None, "truncate to current length is a no-op");
        assert_eq!(core.truncate(9), None, "truncate beyond length is a no-op");
        assert_eq!(core.fork(), 7);
        assert!(core.verify_head());
        // surviving blocks are still readable; the truncated tail is gone.
        assert_eq!(core.get(0).unwrap(), Some(blk("hello")));
        assert_eq!(core.get(1).unwrap(), Some(blk("world")));
        assert_eq!(core.get(2).unwrap(), None);
    }

    #[test]
    fn truncated_head_matches_fresh_prefix() {
        // After truncating to L the tree root equals a fresh log of the first L
        // blocks (root is a pure function of the prefix); the heads differ only
        // by the fork counter (and thus the signature).
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(51), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            core.append(&blk(s)).unwrap();
        }
        core.truncate(3);

        let mut fresh = Hypercore::<Vec<u8>, _, _>::new(author(51), Bytes, MemoryStore::new());
        for s in ["a", "b", "c"] {
            fresh.append(&blk(s)).unwrap();
        }

        let th = core.head().unwrap();
        let fh = fresh.head().unwrap();
        assert_eq!(th.length, fh.length);
        assert_eq!(th.root, fh.root, "truncated root == fresh prefix root");
        assert_eq!(core.fork(), 1);
        assert_eq!(fresh.fork(), 0);
        assert_ne!(th, fh, "heads differ by the fork counter");
        for i in 0..3u64 {
            assert_eq!(core.get(i).unwrap(), fresh.get(i).unwrap());
        }
        assert_eq!(core.get(3).unwrap(), None, "the truncated block is gone");
    }

    #[test]
    fn replica_replicates_truncated_log() {
        // A replica replicating a truncated-and-rewritten source ends
        // byte-identical — the fork counter is carried through the signed head
        // (every block verifies against a head whose message binds the fork).
        let mut src = Hypercore::<Vec<u8>, _, _>::new(author(52), Bytes, MemoryStore::new());
        for s in ["a", "b", "c", "d", "e"] {
            src.append(&blk(s)).unwrap();
        }
        src.truncate(2);
        src.append(&blk("Z")).unwrap(); // [a,b,Z], fork 1
        let head = src.head().unwrap().clone();
        assert_eq!(head.fork, 1);
        assert_eq!(src.len(), 3);

        let mut rep = Replica::<Vec<u8>, _, _>::new(src.public_key(), Bytes, MemoryStore::new());
        for i in 0..src.len() {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head, i, &enc, &proof).unwrap(), "block {i} accepted");
        }
        assert_eq!(rep.len(), src.len());
        assert_eq!(rep.root_hash(), head.root);
        assert_eq!(rep.verified_head(), Some(&head));
        for i in 0..src.len() {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap());
        }
    }

    #[test]
    fn reorg_with_bumped_fork_is_not_equivocation() {
        // A writer that truncates and rewrites under a *new* fork is doing a
        // legitimate reorg, not equivocation: same-length heads at *different*
        // forks are not flagged, and a cross-fork ForkProof does not verify.
        let original = core_with(53, &["a", "b", "c", "d", "e"]); // fork 0

        let mut reorged = core_with(53, &["a", "b", "c", "d", "e"]);
        reorged.truncate(3);
        reorged.append(&blk("X")).unwrap();
        reorged.append(&blk("Y")).unwrap(); // [a,b,c,X,Y], fork 1
        let pk = original.public_key();
        assert_eq!(pk, reorged.public_key());

        let ho = original.head().unwrap();
        let hr = reorged.head().unwrap();
        assert_eq!(ho.length, hr.length);
        assert_ne!(ho.root, hr.root);
        assert_eq!(ho.fork, 0);
        assert_eq!(hr.fork, 1);
        assert!(
            !conflicting_heads(&pk, ho, hr),
            "different forks => legitimate reorg, not a conflict"
        );

        // The per-index disagreement at block 3 ('d' vs 'X') is across forks.
        let across = fork_proof_at(3, &original, &reorged);
        assert_ne!(across.data_a, across.data_b);
        assert!(!across.verify(&pk), "cross-fork divergence is a reorg, not equivocation");

        // Positive control: a second writer reaching the same rewritten content
        // at the *same* fork (0) IS a provable equivocation.
        let equivocating = core_with(53, &["a", "b", "c", "X", "Y"]); // fork 0
        let he = equivocating.head().unwrap();
        assert_eq!(he.fork, 0);
        assert_ne!(ho.root, he.root);
        assert!(
            conflicting_heads(&pk, ho, he),
            "same fork, different root => equivocation"
        );
        let fork = fork_proof_at(3, &original, &equivocating);
        assert!(fork.verify(&pk), "same-fork divergence is a provable fork");
    }

    // ---- secure replica-level reorg (follow a higher-fork truncate-and-rewrite) ----

    #[test]
    fn replica_follows_reorg_and_refetches_suffix() {
        // A replica fully replicates [a,b,c,d,e] (fork 0). The author then
        // reorgs: rewind to 3 (bumping the fork) and rewrite the tail to [X,Y].
        // The replica follows it — it verifies the higher-fork head shares its
        // [0,3) prefix, drops the divergent suffix, and refetches [3,5) — ending
        // byte-identical to the source's new history. The cross-fork analogue of
        // the verified length-extension flow (ADR-0021/0025).
        let mut src = core_with(60, &["a", "b", "c", "d", "e"]); // fork 0
        let head5 = src.head().unwrap().clone();
        let pk = src.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.verified_head(), Some(&head5));

        // The author reorgs: rewind to 3 (fork -> 1), then rewrite [X, Y].
        src.truncate(3);
        src.append(&blk("X")).unwrap();
        src.append(&blk("Y")).unwrap();
        let head_r = src.head().unwrap().clone();
        assert_eq!(head_r.fork, 1);
        assert_eq!(head_r.length, 5);
        assert_ne!(head_r.root, head5.root);

        // Shared prefix is [0,3) (a,b,c kept; block 3 d -> X). The author proves
        // the new history append-only-extends that shared prefix.
        let ancestors = 3u64;
        let up = src.upgrade_proof(ancestors, 5).unwrap();
        assert!(rep.verify_reorg(&head_r, ancestors, Some(&up)), "legit reorg accepted");

        assert!(rep.reorg(&head_r, ancestors, Some(&up)));
        assert_eq!(rep.len(), 3, "divergent suffix dropped, shared prefix kept");
        assert!(rep.verified_head().is_none(), "no verified head until suffix refetched");

        // Refetch the new suffix [3,5) against the new head.
        for i in 3..5u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head_r, i, &enc, &proof).unwrap(), "suffix block {i}");
        }
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.root_hash(), head_r.root, "replica root == reorged signed root");
        assert_eq!(rep.verified_head(), Some(&head_r));
        for i in 0..5u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap(), "byte-identical to reorged source");
        }
    }

    #[test]
    fn replica_reorg_pure_truncation() {
        // The author simply rewinds to a shorter prefix under a bumped fork (no
        // rewrite). The replica follows it with no upgrade proof: the new head
        // *is* its own [0,2) prefix, so the reorg completes immediately.
        let mut src = core_with(61, &["a", "b", "c", "d", "e"]); // fork 0
        let head5 = src.head().unwrap().clone();
        let pk = src.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }

        src.truncate(2); // [a,b], fork 1
        let head2 = src.head().unwrap().clone();
        assert_eq!(head2.fork, 1);
        assert_eq!(head2.length, 2);

        // ancestors == new length: a pure truncation, no proof required.
        assert!(rep.verify_reorg(&head2, 2, None));
        assert!(rep.reorg(&head2, 2, None));
        assert_eq!(rep.len(), 2);
        assert_eq!(rep.root_hash(), head2.root);
        assert_eq!(rep.verified_head(), Some(&head2), "pure truncation completes the reorg");
        for i in 0..2u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap());
        }
        assert_eq!(rep.get(2).unwrap(), None, "dropped block gone");
    }

    #[test]
    fn replica_reorg_from_scratch() {
        // A higher-fork head sharing *no* prefix (block 0 differs): the replica
        // discards everything (ancestors = 0, no proof) and refetches against the
        // signed new head, which authenticates every block.
        let mut src = core_with(62, &["a", "b", "c"]); // fork 0
        let head3 = src.head().unwrap().clone();
        let pk = src.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..3u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head3, i, &enc, &proof).unwrap());
        }

        src.truncate(0); // fork 1, empty
        for s in ["x", "y", "z", "w"] {
            src.append(&blk(s)).unwrap();
        }
        let head_new = src.head().unwrap().clone();
        assert_eq!(head_new.fork, 1);
        assert_eq!(head_new.length, 4);

        assert!(rep.verify_reorg(&head_new, 0, None));
        assert!(rep.reorg(&head_new, 0, None));
        assert_eq!(rep.len(), 0, "no shared prefix: replica reset");
        assert!(rep.verified_head().is_none());

        for i in 0..4u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head_new, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.len(), 4);
        assert_eq!(rep.root_hash(), head_new.root);
        assert_eq!(rep.verified_head(), Some(&head_new));
        for i in 0..4u64 {
            assert_eq!(rep.get(i).unwrap(), src.get(i).unwrap());
        }
    }

    #[test]
    fn replica_rejects_illegitimate_reorg() {
        // A replica trusting the honest [a,b,c,d,e] (fork 0) must reject every
        // illegitimate "reorg" and stay untouched.
        let honest = core_with(63, &["a", "b", "c", "d", "e"]); // fork 0
        let head5 = honest.head().unwrap().clone();
        let pk = honest.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = honest.block(i).unwrap().unwrap();
            let proof = honest.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }

        // (a) An honest higher-fork reorg, but the caller *over-claims* the shared
        // ancestor (4, when the histories diverge at block 3): the replica's own
        // prefix at 4 ('d') disagrees with the new history ('X'), so the fold
        // can't reach the new root and the reorg is refused.
        let mut src = core_with(63, &["a", "b", "c", "d", "e"]);
        src.truncate(3);
        src.append(&blk("X")).unwrap();
        src.append(&blk("Y")).unwrap(); // [a,b,c,X,Y], fork 1
        let head_r = src.head().unwrap().clone();
        let over = src.upgrade_proof(4, 5).unwrap();
        assert!(!rep.reorg(&head_r, 4, Some(&over)), "over-claimed ancestor rejected");
        // ...while the *true* ancestor (3) for the same head is accepted (the
        // head itself is an honest reorg — only the claimed ancestor was wrong).
        let good = src.upgrade_proof(3, 5).unwrap();
        assert!(rep.verify_reorg(&head_r, 3, Some(&good)), "true ancestor accepted");

        // (b) A forking writer rewrote an *old* block (block 1: b -> Z) under a
        // bumped fork, claiming to share [0,5). The forked head is validly
        // self-signed by the same author, but the replica's honest prefix at 5
        // can't fold to the forked root.
        let mut forker = core_with(63, &["a", "b", "c", "d", "e"]);
        forker.truncate(0); // fork 1
        for s in ["a", "Z", "c", "d", "e", "f"] {
            forker.append(&blk(s)).unwrap();
        }
        let bad_head = forker.head().unwrap().clone();
        assert_eq!(bad_head.fork, 1);
        let bad_up = forker.upgrade_proof(5, 6).unwrap();
        assert!(pk.verify(
            &head_message(bad_head.fork, bad_head.length, &bad_head.root),
            &bad_head.sig
        ));
        assert!(!rep.reorg(&bad_head, 5, Some(&bad_up)), "forked old block rejected");

        // (c) A same-fork divergence is an equivocation, never a reorg to follow:
        // refused regardless of the claimed ancestor (and with no proof).
        let equiv = core_with(63, &["a", "b", "c", "X", "Y"]); // fork 0
        let eq_head = equiv.head().unwrap().clone();
        assert_eq!(eq_head.fork, 0);
        assert!(!rep.reorg(&eq_head, 0, None), "same-fork head is not a reorg");
        assert!(!rep.reorg(&eq_head, 3, None), "same-fork head refused at any ancestor");

        // Throughout, the replica is untouched at its honest fork-0 head.
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.verified_head(), Some(&head5));
        for i in 0..5u64 {
            assert_eq!(rep.get(i).unwrap(), honest.get(i).unwrap());
        }
    }

    #[test]
    fn verify_reorg_requires_a_trusted_head() {
        // The head-`None` branch of `verify_reorg`: a reorg adopts a *strictly
        // higher* fork than the one we currently trust, so a replica with no
        // verified head has no current fork to gate against and must refuse —
        // regardless of how legitimate the offered head is. Two situations reach
        // `self.head == None`: a fresh empty replica, and a replica mid-reorg
        // (the shared prefix kept, but the suffix refetch still pending).

        // (a) Fresh empty replica (len 0, no head). Even an `ancestors == 0`
        // "from scratch" reorg is refused: a replica with nothing trusted can't
        // know it is moving to a higher fork — from-scratch replication is
        // `add_block` against a head, not `reorg`.
        let mut src = core_with(64, &["a", "b", "c"]); // fork 0
        src.truncate(1);
        src.append(&blk("X")).unwrap();
        src.append(&blk("Y")).unwrap(); // [a,X,Y], fork 1
        let head_r1 = src.head().unwrap().clone();
        assert_eq!(head_r1.fork, 1);
        let pk = src.public_key();
        let up1 = src.upgrade_proof(1, 3).unwrap();

        let mut empty = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        assert_eq!(empty.len(), 0);
        assert!(empty.verified_head().is_none());
        assert!(!empty.verify_reorg(&head_r1, 0, None), "from-scratch reorg needs a trusted head");
        assert!(!empty.verify_reorg(&head_r1, 1, Some(&up1)), "even a valid offer is refused");
        assert!(!empty.reorg(&head_r1, 1, Some(&up1)));
        assert_eq!(empty.len(), 0, "empty replica untouched");
        assert!(empty.verified_head().is_none());

        // (b) Mid-reorg replica: it followed one reorg (dropping the divergent
        // suffix), so `head == None` while the suffix refetch is pending — even
        // though the tree holds the shared prefix. A *second*, even-higher-fork
        // reorg arriving now must be refused (no trusted head), and the replica
        // must be left able to finish its *current* refetch.
        let mut src = core_with(65, &["a", "b", "c", "d", "e"]); // fork 0
        let head5 = src.head().unwrap().clone();
        let pk = src.public_key();

        let mut rep = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = src.block(i).unwrap().unwrap();
            let proof = src.proof(i).unwrap();
            assert!(rep.add_block(&head5, i, &enc, &proof).unwrap());
        }
        assert_eq!(rep.verified_head(), Some(&head5));

        // First reorg (fork 1): rewind to 3, rewrite [X, Y]; replica follows it.
        src.truncate(3);
        src.append(&blk("X")).unwrap();
        src.append(&blk("Y")).unwrap();
        let head_r = src.head().unwrap().clone();
        assert_eq!(head_r.fork, 1);
        let up = src.upgrade_proof(3, 5).unwrap();
        // Capture head_r's suffix [3,5) before mutating src into a higher fork.
        let suffix: Vec<_> = (3..5u64)
            .map(|i| (i, src.block(i).unwrap().unwrap(), src.proof(i).unwrap()))
            .collect();

        assert!(rep.reorg(&head_r, 3, Some(&up)));
        assert_eq!(rep.len(), 3, "shared prefix kept");
        assert!(rep.verified_head().is_none(), "mid-reorg: no trusted head until suffix refetched");

        // Second, higher reorg (fork 2) arrives while the replica is mid-reorg.
        src.truncate(1);
        src.append(&blk("P")).unwrap();
        src.append(&blk("Q")).unwrap(); // [a,P,Q], fork 2
        let head_r2 = src.head().unwrap().clone();
        assert_eq!(head_r2.fork, 2);
        let up2 = src.upgrade_proof(1, 3).unwrap();
        assert!(
            !rep.verify_reorg(&head_r2, 1, Some(&up2)),
            "no trusted head => a second reorg is refused mid-reorg"
        );
        assert!(!rep.reorg(&head_r2, 1, Some(&up2)));
        assert_eq!(rep.len(), 3, "mid-reorg replica untouched by the refused second reorg");
        assert!(rep.verified_head().is_none());

        // The refusal didn't corrupt the replica: it can still finish its
        // *original* pending refetch to head_r, ending byte-identical to it.
        for (i, enc, proof) in &suffix {
            assert!(rep.add_block(&head_r, *i, enc, proof).unwrap(), "suffix block {i}");
        }
        assert_eq!(rep.len(), 5);
        assert_eq!(rep.root_hash(), head_r.root);
        assert_eq!(rep.verified_head(), Some(&head_r));
        assert_eq!(rep.get(3).unwrap(), Some(blk("X")));
        assert_eq!(rep.get(4).unwrap(), Some(blk("Y")));
    }

    // ---- clear: sparse presence reclamation (upstream `clear.js`, L1) ----

    #[test]
    fn clear_marks_blocks_absent_and_updates_contiguous_length() {
        // Ports clear.js "clear": clearing a middle block makes it absent while the
        // surrounding blocks stay present, drops `contiguousLength` to the first
        // hole, and leaves the authenticated tree (length, root, signed head, the
        // cleared block's proof) completely untouched.
        let mut a = core_with(80, &["a", "b", "c"]);
        assert_eq!(a.contiguous_length(), 3);
        assert!(a.has(0) && a.has(1) && a.has(2));
        let head_before = a.head().unwrap().clone();

        assert_eq!(a.clear(1, 2).unwrap(), 1, "exactly one block cleared");
        assert_eq!(a.contiguous_length(), 1, "contig drops to the first hole");
        assert!(a.has(0), "has 0");
        assert!(!a.has(1), "has not 1");
        assert!(a.has(2), "has 2");
        assert_eq!(a.get(0).unwrap(), Some(blk("a")));
        assert_eq!(a.get(1).unwrap(), None, "cleared block reads absent (no-wait get)");
        assert_eq!(a.get(2).unwrap(), Some(blk("c")));
        assert_eq!(a.block(1).unwrap(), None, "raw bytes gone too");

        // Clear is presence reclamation, not truncation: the tree/head are intact.
        assert_eq!(a.len(), 3, "length unchanged by clear");
        assert_eq!(a.head().unwrap(), &head_before, "signed head unchanged");
        assert!(a.proof(1).is_some(), "the tree still proves the cleared block");
        assert!(a.verify_head());
    }

    #[test]
    fn clear_single_block_in_a_larger_log() {
        // Ports clear.js "incorrect clear": a 129-block log, clear just block 127.
        // (129 crosses a Merkle root boundary, so block 128 is a fresh root — a
        // good check that a single-block clear is exact.)
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(81), Bytes, MemoryStore::new());
        for _ in 0..129 {
            core.append(&blk("tick")).unwrap();
        }
        assert_eq!(core.contiguous_length(), 129);

        assert_eq!(core.clear(127, 128).unwrap(), 1);
        assert!(!core.has(127));
        assert!(core.has(128));
        assert_eq!(core.get(127).unwrap(), None);
        assert_eq!(core.get(128).unwrap(), Some(blk("tick")));
        assert_eq!(core.contiguous_length(), 127, "prefix ends at the hole");
        assert_eq!(core.len(), 129, "length unchanged by clear");
    }

    #[test]
    fn clear_out_of_range_is_noop() {
        // Ports clear.js "clear blocks with diff option": clearing past the end
        // clears nothing (upstream returns `null`; we return a count of 0) and the
        // log is untouched.
        let mut core = core_with(82, &["only"]);
        let head_before = core.head().unwrap().clone();

        assert_eq!(core.clear(1337, 1338).unwrap(), 0, "nothing in range (upstream null)");
        assert_eq!(core.clear(5, 100).unwrap(), 0, "far-out range clears nothing");

        assert_eq!(core.len(), 1);
        assert!(core.has(0));
        assert_eq!(core.get(0).unwrap(), Some(blk("only")));
        assert_eq!(core.contiguous_length(), 1);
        assert_eq!(core.head().unwrap(), &head_before);
    }

    #[test]
    fn clear_unknown_blocks_has_no_side_effect() {
        // Ports clear.js "clear - no side effect from clearing unknown nodes":
        // clearing a block you don't hold is a harmless no-op (never panics, never
        // touches a block it doesn't have), and clears are idempotent.
        let mut core = core_with(83, &["a", "b", "c", "d"]);

        // Clear three blocks once each...
        assert_eq!(core.clear(0, 1).unwrap(), 1);
        assert_eq!(core.clear(1, 2).unwrap(), 1);
        assert_eq!(core.clear(2, 3).unwrap(), 1);
        // ...and again: now already absent, so each is a no-op.
        assert_eq!(core.clear(0, 1).unwrap(), 0);
        assert_eq!(core.clear(1, 2).unwrap(), 0);
        assert_eq!(core.clear(2, 3).unwrap(), 0);

        // A wide range over the mostly-absent log clears only the still-present block (3).
        assert_eq!(core.clear(0, 4).unwrap(), 1);
        assert_eq!(core.clear(0, 4).unwrap(), 0, "a fully-cleared range is a no-op");

        // Everything is absent now, but the length is intact and nothing is
        // contiguously present from 0.
        for i in 0..4u64 {
            assert!(!core.has(i));
            assert_eq!(core.get(i).unwrap(), None);
        }
        assert_eq!(core.contiguous_length(), 0);
        assert_eq!(core.len(), 4);
    }

    #[test]
    fn clear_interior_range_leaves_a_hole() {
        // A range clear leaves an interior hole: the blocks at the range boundaries
        // stay present, every interior block is absent, and `contiguousLength`
        // stops at the first hole. (Small-scale analogue of clear.js "clear - large
        // cores", which clears interior ranges; the bitfield's page-boundary
        // behaviour is already pinned in `storage::bitfield`.)
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(84), Bytes, MemoryStore::new());
        for i in 0..40u64 {
            core.append(&format!("Block-{i}").into_bytes()).unwrap();
        }

        assert_eq!(core.clear(10, 20).unwrap(), 10, "ten interior blocks cleared");
        assert_eq!(core.get(9).unwrap(), Some(b"Block-9".to_vec()), "lower boundary present");
        assert_eq!(core.get(10).unwrap(), None);
        assert_eq!(core.get(19).unwrap(), None);
        assert_eq!(core.get(20).unwrap(), Some(b"Block-20".to_vec()), "upper boundary present");
        assert_eq!(core.contiguous_length(), 10, "prefix ends at the hole");

        for i in 0..40u64 {
            let present = !(10..20).contains(&i);
            assert_eq!(core.has(i), present, "presence of block {i}");
        }

        assert_eq!(core.clear(10, 20).unwrap(), 0, "re-clearing the hole is a no-op");
        assert_eq!(core.len(), 40, "length unchanged");
        assert!(core.verify_head());
    }

    #[test]
    fn cleared_block_stays_authenticated_and_refetchable() {
        // The L1 form of clear.js "clear + replication": a clears a block, a holder
        // (b) still has it, and because clear leaves a's authenticated tree
        // untouched, the block b holds re-verifies against a's signed head with a's
        // still-valid inclusion proof — i.e. it is re-fetchable. (The wire exchange
        // that moves the bytes back is networking, deferred.)
        let mut a = core_with(85, &["a", "b", "c", "d", "e"]);
        let head = a.head().unwrap().clone();
        let pk = a.public_key();

        // b fully replicates a (so b holds block 1).
        let mut b = Replica::<Vec<u8>, _, _>::new(pk, Bytes, MemoryStore::new());
        for i in 0..5u64 {
            let enc = a.block(i).unwrap().unwrap();
            let proof = a.proof(i).unwrap();
            assert!(b.add_block(&head, i, &enc, &proof).unwrap());
        }

        // a clears block 1; b is unaffected.
        assert_eq!(a.clear(1, 2).unwrap(), 1);
        assert!(!a.has(1), "a cleared block 1");
        assert_eq!(a.get(1).unwrap(), None);
        assert_eq!(b.get(1).unwrap(), Some(blk("b")), "b not cleared");

        // a's signed head and the tree are untouched, so the block re-supplied by
        // the holder verifies against a's head with a's still-valid proof.
        assert_eq!(a.head().unwrap(), &head, "clear left the signed head untouched");
        let from_b = b.get(1).unwrap().unwrap();
        let enc_from_b = Bytes.encode(&from_b); // codec is deterministic -> the committed bytes
        let proof1 = a.proof(1).expect("a's tree still proves block 1 after clear");
        assert!(
            verify_block(&pk, &head, 1, &enc_from_b, &proof1),
            "a block re-supplied by a holder verifies against a's unchanged head"
        );

        // a itself still has the hole until a refetch fills it (the refetch is the
        // networking step we defer); the point proven here is that authentication
        // survives the clear, so the bytes remain recoverable.
        assert_eq!(a.contiguous_length(), 1, "a still has the hole until refetch");
    }

    // ---- snapshots (upstream `snapshots.js`, L1) ----

    #[test]
    fn snapshot_is_immune_to_truncate_and_rewrite() {
        // Upstream `snapshots.js`: "snapshot does not change when original gets
        // modified". A snapshot pins the length AND the block bytes at snapshot
        // time, surviving the core's later append / truncate / re-append; its
        // `signed_length` tracks how much of it the *current* core still backs.
        let mut core = core_with(50, &["block0", "block1", "block2"]);
        let snap = core.snapshot().unwrap();
        assert_eq!(snap.length(), 3, "correct length");
        assert_eq!(snap.signed_length(&core), 3, "correct signed length");
        assert_eq!(snap.get(2).unwrap(), Some(blk("block2")), "block exists");

        core.append(&blk("Block3")).unwrap();
        assert_eq!(snap.length(), 3);
        assert_eq!(snap.signed_length(&core), 3);
        assert_eq!(snap.get(2).unwrap(), Some(blk("block2")));

        core.truncate(3); // drops Block3; the snapshotted prefix is untouched
        assert_eq!(snap.length(), 3);
        assert_eq!(snap.signed_length(&core), 3);
        assert_eq!(snap.get(2).unwrap(), Some(blk("block2")));

        core.truncate(2); // now below the snapshot
        assert_eq!(snap.length(), 3);
        assert_eq!(
            snap.signed_length(&core),
            2,
            "signed length now lower since it truncated below snap"
        );
        assert_eq!(snap.get(2).unwrap(), Some(blk("block2")));

        core.append(&blk("new Block2")).unwrap(); // re-appends different content over index 2
        assert_eq!(snap.length(), 3);
        assert_eq!(
            snap.signed_length(&core),
            2,
            "signed length remains at lowest value after re-appending"
        );
        assert_eq!(
            snap.get(2).unwrap(),
            Some(blk("block2")),
            "old block still (snapshot did not change)"
        );
        // The core itself moved on to the rewritten history.
        assert_eq!(core.get(2).unwrap(), Some(blk("new Block2")));

        // A read over the snapshot yields exactly the three snapshotted blocks
        // (the L1 analogue of upstream's `createReadStream`).
        let read: Vec<Vec<u8>> = (0..snap.length()).map(|i| snap.get(i).unwrap().unwrap()).collect();
        assert_eq!(read, vec![blk("block0"), blk("block1"), blk("block2")]);
    }

    #[test]
    fn snapshot_block_is_independently_authenticated() {
        // A snapshot carries its own signed head + tree, so each captured block
        // stays verifiable against the snapshot's head even after the core forks
        // away beneath it (the host-safe L1 form of `snapshots.js`'s "snapshots
        // are consistent" — no wire).
        let mut core = core_with(51, &["a", "b", "c", "d", "e"]);
        let snap = core.snapshot().unwrap();
        let head = snap.head().unwrap().clone();
        let pk = core.public_key();
        assert_eq!(snap.root_hash(), head.root);

        // Truncate-and-rewrite the core under the snapshot (bumps the fork).
        core.truncate(2);
        core.append(&blk("X")).unwrap();
        core.append(&blk("Y")).unwrap();
        core.append(&blk("Z")).unwrap();
        assert_eq!(core.fork(), 1, "core rewound and rewrote");
        assert_eq!(core.get(2).unwrap(), Some(blk("X")));

        // The snapshot is untouched and every captured block still authenticates.
        assert_eq!(snap.length(), 5);
        assert_eq!(snap.fork(), 0, "snapshot keeps its fork");
        for i in 0..5u64 {
            let enc = snap.block(i).expect("snapshot holds the block");
            let proof = snap.proof(i).expect("snapshot proves the block");
            assert!(verify_block(&pk, &head, i, enc, &proof), "snapshot block {i} authenticated");
        }
        assert_eq!(snap.get(2).unwrap(), Some(blk("c")), "snapshot keeps the old block 2");
        // Only the shared two-block prefix is still backed by the live core.
        assert_eq!(snap.signed_length(&core), 2);
    }

    #[test]
    fn empty_and_static_snapshots() {
        // Upstream `snapshots.js`: "snapshots wait for ready" — a snapshot's length
        // is fixed at capture time (an empty snapshot stays empty); plus the
        // out-of-range read (upstream `SNAPSHOT_NOT_AVAILABLE`, reported as `None`
        // at L1).
        let mut core = Hypercore::<Vec<u8>, _, _>::new(author(52), Bytes, MemoryStore::new());
        let s1 = core.snapshot().unwrap(); // captured at length 0
        assert!(s1.is_empty());
        assert_eq!(s1.head(), None, "empty core has no signed head");

        core.append(&blk("block #0.0")).unwrap();
        core.append(&blk("block #1.0")).unwrap();
        let s2 = core.snapshot().unwrap(); // captured at length 2
        core.append(&blk("block #2.0")).unwrap();

        assert_eq!(s1.length(), 0, "empty snapshot");
        assert_eq!(s2.length(), 2, "set at capture time");

        core.append(&blk("block #3.0")).unwrap();
        assert_eq!(s1.length(), 0, "is static");
        assert_eq!(s2.length(), 2, "is static");

        // Reads: in-range decodes, out-of-range is None.
        assert_eq!(s1.get(0).unwrap(), None, "nothing in an empty snapshot");
        assert_eq!(s2.get(1).unwrap(), Some(blk("block #1.0")));
        assert_eq!(s2.get(2).unwrap(), None, "out of range -> None");
        assert_eq!(s2.block(2), None);

        assert_eq!(s1.signed_length(&core), 0);
        assert_eq!(s2.signed_length(&core), 2, "the 2-block prefix is still backed");
    }

    #[test]
    fn snapshot_is_independent_of_clear() {
        // A snapshot owns its bytes by value, so clearing the core's presence map
        // afterwards (dropping its local bytes) doesn't affect the snapshot.
        let mut core = core_with(53, &["a", "b", "c", "d"]);
        let snap = core.snapshot().unwrap();

        assert_eq!(core.clear(1, 3).unwrap(), 2, "two blocks cleared on the core");
        assert_eq!(core.get(1).unwrap(), None, "core dropped block 1");
        assert_eq!(core.get(2).unwrap(), None, "core dropped block 2");

        // The snapshot still has every block.
        assert_eq!(snap.length(), 4);
        for (i, s) in ["a", "b", "c", "d"].iter().enumerate() {
            assert_eq!(snap.get(i as u64).unwrap(), Some(blk(s)), "snapshot keeps block {i}");
        }
        // Clearing presence does not touch the tree, so the prefix is still fully shared.
        assert_eq!(snap.signed_length(&core), 4);
    }

    // ---- read / byte streams (upstream `streams.js`, L1) ----

    /// Collect a read stream into the decoded values, asserting no read error.
    fn collect_read(core: &ByteCore, opts: ReadStreamOptions) -> Vec<Vec<u8>> {
        core.read_stream(opts).map(|r| r.unwrap()).collect()
    }

    /// Collect a byte stream into the raw encoded-block byte vectors.
    fn collect_bytes(core: &ByteCore, opts: ByteStreamOptions) -> Vec<Vec<u8>> {
        core.byte_stream(opts).map(|r| r.unwrap()).collect()
    }

    #[test]
    fn read_stream_basic_and_range() {
        // Upstream `streams.js`: "basic read stream" + "read stream with start /
        // end" (+ "basic write+read stream", which is append-then-read at L1).
        let core = core_with(60, &["hello", "world", "verden", "welt"]);
        let all: Vec<Vec<u8>> = ["hello", "world", "verden", "welt"].iter().map(|s| blk(s)).collect();

        // whole log
        assert_eq!(collect_read(&core, ReadStreamOptions::default()), all);

        // start: 1 -> from index 1 to the end
        assert_eq!(
            collect_read(&core, ReadStreamOptions { start: 1, ..Default::default() }),
            all[1..].to_vec()
        );

        // start: 2, end: 3 -> just block 2
        assert_eq!(
            collect_read(&core, ReadStreamOptions { start: 2, end: Some(3), ..Default::default() }),
            all[2..3].to_vec()
        );

        // reverse over the whole log
        let mut rev = all.clone();
        rev.reverse();
        assert_eq!(collect_read(&core, ReadStreamOptions { reverse: true, ..Default::default() }), rev);

        // empty range (start >= end) yields nothing; an out-of-range end clamps to len
        assert!(collect_read(
            &core,
            ReadStreamOptions { start: 3, end: Some(3), ..Default::default() }
        )
        .is_empty());
        assert_eq!(
            collect_read(&core, ReadStreamOptions { start: 1, end: Some(99), ..Default::default() }),
            all[1..].to_vec()
        );
    }

    #[test]
    fn read_stream_end_ignores_live() {
        // Upstream `streams.js`: "read stream with end and live (live should be
        // ignored)" — with `end` set, `live: true` must not tail; the stream stops
        // at `end`. (At L1 `live` is always ignored — there is no peer to tail.)
        let core = core_with(61, &["alpha", "beta", "gamma", "delta", "epsilon"]);
        let collected =
            collect_read(&core, ReadStreamOptions { end: Some(3), live: true, ..Default::default() });
        let expected: Vec<Vec<u8>> = ["alpha", "beta", "gamma"].iter().map(|s| blk(s)).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn read_stream_skips_cleared_holes() {
        // The read stream is no-wait: a block dropped by `clear` is skipped (not
        // waited on) — the L1 consequence of `get` returning `None` for an absent
        // block. The stream yields the present blocks in order, both directions.
        let mut core = core_with(62, &["a", "b", "c", "d", "e"]);
        assert_eq!(core.clear(1, 3).unwrap(), 2, "drop blocks 1 and 2");
        assert_eq!(
            collect_read(&core, ReadStreamOptions::default()),
            vec![blk("a"), blk("d"), blk("e")],
            "holes skipped"
        );
        assert_eq!(
            collect_read(&core, ReadStreamOptions { reverse: true, ..Default::default() }),
            vec![blk("e"), blk("d"), blk("a")]
        );
    }

    #[test]
    fn byte_stream_basic_and_ranges() {
        // Upstream `streams.js`: "basic byte stream" + the byteOffset/byteLength
        // cases. A byte stream yields whole *encoded* blocks covering a byte range;
        // we address the encoded byte layout the tree authenticates (the `padding`
        // divergence, ADR-0022), so offsets/lengths are derived from the encoded
        // sizes rather than the raw payload sizes.
        let core = core_with(63, &["hello", "world", "verden", "welt"]);
        let enc: Vec<Vec<u8>> = (0..4).map(|i| core.block(i).unwrap().unwrap()).collect();
        let size = |i: usize| enc[i].len() as u64;
        let off = |i: usize| (0..i).map(size).sum::<u64>(); // byte offset at the start of block i

        // whole log (default offset 0, default length)
        assert_eq!(collect_bytes(&core, ByteStreamOptions::default()), enc);

        // byteOffset at block 1, byteLength covering exactly blocks 1 and 2
        assert_eq!(
            collect_bytes(
                &core,
                ByteStreamOptions { byte_offset: off(1), byte_length: Some(size(1) + size(2)) }
            ),
            vec![enc[1].clone(), enc[2].clone()]
        );

        // byteOffset at block 2, byteLength covering exactly block 2
        assert_eq!(
            collect_bytes(
                &core,
                ByteStreamOptions { byte_offset: off(2), byte_length: Some(size(2)) }
            ),
            vec![enc[2].clone()]
        );

        // byteOffset at block 2, default byteLength -> to the end (blocks 2 and 3)
        assert_eq!(
            collect_bytes(&core, ByteStreamOptions { byte_offset: off(2), byte_length: None }),
            vec![enc[2].clone(), enc[3].clone()]
        );

        // a byteOffset at/past the end yields nothing
        let total = core.byte_length();
        assert!(collect_bytes(
            &core,
            ByteStreamOptions { byte_offset: total, byte_length: None }
        )
        .is_empty());
    }

    #[test]
    fn byte_stream_yields_empty_payload_blocks() {
        // Upstream `streams.js`: "basic byte stream w/ empty buffers" — blocks with
        // empty payloads are still emitted (they do not end the stream) as long as
        // the byte budget isn't exhausted. With our framing an empty payload still
        // encodes to a 1-byte block (the ADR-0022 padding divergence), so the
        // observable property — every block in the byte range is emitted — holds.
        let core = core_with(64, &["hello", "world", "", "", "end"]);
        assert_eq!(core.len(), 5, "all blocks appended");
        let enc: Vec<Vec<u8>> = (0..5).map(|i| core.block(i).unwrap().unwrap()).collect();
        // the default byte stream covers the whole log -> every block, incl. the empties
        assert_eq!(collect_bytes(&core, ByteStreamOptions::default()), enc);
    }

    // ---- prologue migration / move-to (upstream `move-to.js`, L1) ----

    #[test]
    fn move_to_basic_preserves_prefix_under_new_key() {
        // Ports `move-to.js` "move - basic": a log of [1,2,3] is migrated onto a
        // NEW core under a fresh key, committing to the prefix via a prologue; the
        // prefix is preserved byte-identically and a new block appends on top — the
        // L1 of `copyPrologue` + `moveTo` + append('4').
        let src = core_with(70, &["1", "2", "3"]);
        assert_eq!(src.len(), 3);
        let pr = src.prologue_at(3).unwrap(); // { length: 3, hash: root-of-[1,2,3] }
        assert_eq!(pr.length, 3);

        // A new core under a DIFFERENT author, bound to the prologue.
        let mut migrated =
            Hypercore::<Vec<u8>, _, _>::with_prologue(author(71), Bytes, MemoryStore::new(), pr);
        assert_ne!(migrated.public_key(), src.public_key(), "migration is to a new identity");
        assert_eq!(migrated.prologue(), Some(&pr));
        assert!(migrated.is_empty());

        assert_eq!(migrated.copy_prologue(&src).unwrap(), 3);
        assert_eq!(migrated.len(), 3);
        assert!(migrated.verify_prologue());
        assert!(migrated.verify_head());

        // The prefix is byte-identical (raw stored bytes and decoded values).
        for i in 0..3u64 {
            assert_eq!(migrated.block(i).unwrap(), src.block(i).unwrap(), "block {i} bytes");
            assert_eq!(migrated.get(i).unwrap(), src.get(i).unwrap(), "block {i} value");
        }

        // The migrated head is signed by the NEW key, not the source's.
        let h = migrated.head().unwrap().clone();
        assert!(
            migrated.public_key().verify(&head_message(h.fork, h.length, &h.root), &h.sig),
            "new key signs the migrated head"
        );
        assert!(
            !src.public_key().verify(&head_message(h.fork, h.length, &h.root), &h.sig),
            "old key does NOT sign the migrated head"
        );

        // Continue the log under the new key: append('4').
        assert_eq!(migrated.append(&blk("4")).unwrap(), 3);
        assert_eq!(migrated.len(), 4);
        assert_eq!(migrated.get(3).unwrap(), Some(blk("4")));
        assert!(migrated.verify_prologue(), "prologue prefix still intact after append");

        // Every block authenticates against the new head (prefix + new block alike).
        let head = migrated.head().unwrap().clone();
        let pk = migrated.public_key();
        for i in 0..4u64 {
            let enc = migrated.block(i).unwrap().unwrap();
            let proof = migrated.proof(i).unwrap();
            assert!(verify_block(&pk, &head, i, &enc, &proof), "migrated block {i} authenticated");
        }
    }

    #[test]
    fn copy_prologue_is_content_addressed_and_rejects_mismatch() {
        // A prologue names a prefix by its Merkle hash, not by author — so any log
        // with the same prefix content satisfies it, while diverging / too-short /
        // unbound cases are rejected without touching the target core.
        let src = core_with(72, &["1", "2", "3", "4"]);
        let pr = src.prologue_at(3).unwrap(); // commits to the first 3 blocks

        // A DIFFERENT author whose first 3 blocks match the content backs it.
        let cross_author = core_with(73, &["1", "2", "3", "different-tail"]);
        assert_ne!(cross_author.public_key(), src.public_key());
        let mut m_ok =
            Hypercore::<Vec<u8>, _, _>::with_prologue(author(74), Bytes, MemoryStore::new(), pr);
        assert_eq!(m_ok.copy_prologue(&cross_author).unwrap(), 3, "content-addressed: cross-author ok");
        assert!(m_ok.verify_prologue());
        for (i, s) in ["1", "2", "3"].iter().enumerate() {
            assert_eq!(m_ok.get(i as u64).unwrap(), Some(blk(s)));
        }

        // A source diverging within the prefix (block 2 differs) does NOT back it.
        let diverging = core_with(75, &["1", "2", "X"]);
        let mut m_bad =
            Hypercore::<Vec<u8>, _, _>::with_prologue(author(74), Bytes, MemoryStore::new(), pr);
        assert_eq!(m_bad.copy_prologue(&diverging), Err(Error::PrologueMismatch));
        assert!(m_bad.is_empty(), "nothing copied on a content mismatch");

        // A source shorter than the prologue length cannot back it.
        let short = core_with(76, &["1", "2"]);
        let mut m_short =
            Hypercore::<Vec<u8>, _, _>::with_prologue(author(74), Bytes, MemoryStore::new(), pr);
        assert_eq!(m_short.copy_prologue(&short), Err(Error::PrologueMismatch));
        assert!(m_short.is_empty());

        // copy_prologue on a core with no prologue is rejected.
        let mut plain = Hypercore::<Vec<u8>, _, _>::new(author(77), Bytes, MemoryStore::new());
        assert_eq!(plain.copy_prologue(&src), Err(Error::NoPrologue));

        // copy_prologue into a non-empty (already-migrated) core is rejected.
        assert_eq!(m_ok.copy_prologue(&src), Err(Error::PrologueMismatch));
        assert_eq!(m_ok.len(), 3, "a second copy_prologue leaves the core unchanged");
    }

    #[test]
    fn move_to_after_truncate_with_surviving_snapshot() {
        // Ports `move-to.js` "move - snapshots": the source is truncated-and-
        // rewritten, then migrated onto a new core; a snapshot taken *before* the
        // rewrite still returns its own captured blocks (by-value immunity, iter 29),
        // unaffected by the migration of the live core.
        let mut core = core_with(78, &["hello", "world", "again"]); // len 3
        let snap = core.snapshot().unwrap(); // captured at length 3

        core.truncate(1); // rewind to [hello]
        core.append(&blk("break")).unwrap(); // core = [hello, break], len 2, fork 1
        assert_eq!(core.len(), 2);
        assert_eq!(core.fork(), 1);
        assert_eq!(snap.length(), 3, "snapshot is unaffected by the truncate-and-rewrite");

        // Migrate the rewritten core onto a new identity.
        let pr = core.prologue_at(2).unwrap(); // commits to [hello, break]
        let mut migrated =
            Hypercore::<Vec<u8>, _, _>::with_prologue(author(79), Bytes, MemoryStore::new(), pr);
        assert_eq!(migrated.copy_prologue(&core).unwrap(), 2);
        assert_eq!(migrated.len(), 2);
        assert!(migrated.verify_prologue());
        assert_eq!(migrated.get(0).unwrap(), Some(blk("hello")));
        assert_eq!(migrated.get(1).unwrap(), Some(blk("break")));

        // The snapshot still has its three original blocks (moveTo on a by-value
        // snapshot is a no-op for the observable behaviour).
        assert_eq!(snap.length(), 3);
        assert_eq!(snap.get(0).unwrap(), Some(blk("hello")));
        assert_eq!(snap.get(1).unwrap(), Some(blk("world")));
        assert_eq!(snap.get(2).unwrap(), Some(blk("again")));
    }

    #[test]
    fn prologue_is_a_truncate_floor() {
        // A prologue-bound core can never rewind into the committed prefix —
        // truncating below `prologue.length` is refused, keeping `verify_prologue`
        // an invariant. (Upstream forbids truncating into the prologue.)
        let src = core_with(80, &["a", "b", "c"]);
        let pr = src.prologue_at(2).unwrap(); // floor at length 2
        let mut m =
            Hypercore::<Vec<u8>, _, _>::with_prologue(author(81), Bytes, MemoryStore::new(), pr);
        m.copy_prologue(&src).unwrap(); // m = [a, b], len 2
        m.append(&blk("c2")).unwrap();
        m.append(&blk("d2")).unwrap(); // m = [a, b, c2, d2], len 4

        // Above the floor: allowed.
        assert_eq!(m.truncate(3), Some(Truncation { from: 4, to: 3 }));
        // Exactly the floor: allowed (keeps the prologue exactly).
        assert_eq!(m.truncate(2), Some(Truncation { from: 3, to: 2 }));
        assert!(m.verify_prologue());
        // Below the floor: refused, core untouched.
        assert_eq!(m.truncate(1), None, "cannot truncate into the prologue prefix");
        assert_eq!(m.truncate(0), None);
        assert_eq!(m.len(), 2);
        assert!(m.verify_prologue());
        assert_eq!(m.get(0).unwrap(), Some(blk("a")));
        assert_eq!(m.get(1).unwrap(), Some(blk("b")));
    }
}
