use std::collections::BTreeMap;
use std::marker::PhantomData;

use codec::Codec;
use identity::{PublicKey, SecretKey, Sig};
use merkle::{Hash, MerkleTree, Proof, UpgradeProof};
use storage::{Bitfield, Store};

use crate::*;
use crate::head_message;

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

    /// Locate the block containing byte offset `byte_offset` (over the *encoded*
    /// blocks): returns `(block_index, offset_within_block)`, tree-accelerated
    /// (upstream `seek`). An offset at or past the end returns `(len, leftover)`.
    pub fn seek(&self, byte_offset: u64) -> (u64, u64) {
        self.tree.seek(byte_offset)
    }

    /// Store app-defined metadata under `key` (upstream `setUserData`). Persisted in
    /// the core's `Store` under a reserved key, so it survives `persist`/`open`.
    pub fn set_user_data(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error<S::Error>> {
        let mut map = self.load_user_data()?;
        map.insert(key.to_vec(), value.to_vec());
        self.store
            .put(KEY_USERDATA, &encode_user_data(&map))
            .map_err(Error::Storage)
    }

    /// Read app-defined metadata for `key` (upstream `getUserData`), or `None`.
    pub fn get_user_data(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error<S::Error>> {
        Ok(self.load_user_data()?.remove(key))
    }

    fn load_user_data(&self) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, Error<S::Error>> {
        match self.store.get(KEY_USERDATA).map_err(Error::Storage)? {
            Some(bytes) => decode_user_data(&bytes).ok_or(Error::Corrupt),
            None => Ok(BTreeMap::new()),
        }
    }

    /// **Purge**: delete this core's blocks and metadata from the `Store` and reset it
    /// to empty (upstream `purge`). Physical reclamation is the backend's job (the
    /// log-structured store compacts); this removes the logical data.
    pub fn purge(&mut self) -> Result<(), Error<S::Error>> {
        for i in 0..self.tree.len() {
            self.store.delete(i).map_err(Error::Storage)?;
        }
        for k in [KEY_META, KEY_TREE, KEY_PRESENCE, KEY_USERDATA] {
            self.store.delete(k).map_err(Error::Storage)?;
        }
        self.tree = MerkleTree::new();
        self.presence = Bitfield::new();
        self.head = None;
        self.fork = 0;
        self.prologue = None;
        self.last_truncation = None;
        Ok(())
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

/// Encode the user-data map: `[count]` then per entry `[klen][k][vlen][v]`, all
/// little-endian. (Internal to `set_user_data`/`get_user_data`.)
fn encode_user_data(map: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (k, v) in map {
        out.extend_from_slice(&(k.len() as u64).to_le_bytes());
        out.extend_from_slice(k);
        out.extend_from_slice(&(v.len() as u64).to_le_bytes());
        out.extend_from_slice(v);
    }
    out
}

/// Inverse of [`encode_user_data`]; `None` on a malformed/truncated buffer.
fn decode_user_data(bytes: &[u8]) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
    fn u64_at(b: &mut &[u8]) -> Option<u64> {
        if b.len() < 8 {
            return None;
        }
        let (h, r) = b.split_at(8);
        *b = r;
        Some(u64::from_le_bytes(h.try_into().ok()?))
    }
    fn bytes_at(b: &mut &[u8], n: usize) -> Option<Vec<u8>> {
        if b.len() < n {
            return None;
        }
        let (h, r) = b.split_at(n);
        *b = r;
        Some(h.to_vec())
    }
    let mut b = bytes;
    let count = u64_at(&mut b)? as usize;
    let mut map = BTreeMap::new();
    for _ in 0..count {
        let kl = u64_at(&mut b)? as usize;
        let k = bytes_at(&mut b, kl)?;
        let vl = u64_at(&mut b)? as usize;
        let v = bytes_at(&mut b, vl)?;
        map.insert(k, v);
    }
    if !b.is_empty() {
        return None;
    }
    Some(map)
}
