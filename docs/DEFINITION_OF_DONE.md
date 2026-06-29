# Definition of Done

Unlike a verbatim port, there is **no reference output to diff against** — we are clean-room and
not upstream-compatible. So "done" is **property satisfaction + ported upstream tests + integration
gates**, all reduced to one runnable signal: `just verify-full` exits 0.

## 1. Per-crate property tests (the spec)

| Crate | Capability | Property that must hold |
|-------|------------|-------------------------|
| `merkle` | tree + inclusion/range proofs | valid proof verifies; **a tampered proof is rejected** |
| `codec` | typed payload ⇄ bytes | `decode(encode(x)) == x`; **old bytes decode under a newer schema; unknown variants skip cleanly** |
| `identity` | keygen / sign / verify | a valid signature verifies; **a forged signature is rejected** |
| `storage` | byte backend | round-trip on in-memory; same contract upheld by the IndexedDB backend |
| `hypercore` | append / get / verify / replicate | append-only; each entry verifies vs signature + merkle; **a destination that applies proofs ends up byte-identical to the source log** |
| `autobase` | linearize + quorum | **determinism** (same DAG ⇒ same order everywhere); **causal-respect** (no node before its deps); **convergence** (replicas seeing the same set agree); **finality-stability** (a quorum-finalized prefix never reorders) |
| `hyperbee` | ordered KV + range | correctness over a hypercore (only if we build it) |

## 2. Ported upstream tests (the "ported everything" oracle)

Every **relevant** upstream test file becomes a passing Rust test. Tracking + relevance filter:
`docs/UPSTREAM_TEST_MAP.md`. A capability is not "done" until its mapped upstream tests are green.

## 3. Integration gates

1. **WASM compile** — `just wasm` (`cargo build --target wasm32-unknown-unknown` for `hypercore`,
   `autobase`, `storage`). Always in `verify`.
2. **WASM runtime** — `just wasm-test`: create a hypercore, persist to IndexedDB, reload, verify,
   in headless Chrome. Proves the WASM-*first* goal, not just compilation.
3. **Convergence simulation** — `crates/autobase/tests/convergence.rs`: N writers, **seeded**
   random causal visibility (partitions, reordering); assert convergence + state-equality +
   finalized-prefix-never-reorders. Generic toy document — no domain types. (Model:
   `reference/js/autobase/test/fuzz/`.)
4. **JS algorithmic-equivalence oracle** — `just oracle`: feed the same random DAGs to
   `reference/js/autobase`'s linearizer (via node) and ours; assert identical order. The
   verovio-style deterministic oracle, at the algorithm level. **node runs only inside a container**
   via `scripts/node-sandbox.sh` — never on the host (untrusted npm tree; see CLAUDE.md rule 7).

## The gate

```
just verify       # always-runnable: cargo test --workspace + wasm compile
just verify-full  # verify + wasm-test (chrome) + oracle (node)
```

`just verify-full` green **and** all boxes in this file + `UPSTREAM_TEST_MAP.md` ticked = **done**.

## Checklist (high level)

- [x] Workspace scaffold (no data types)
- [x] `merkle` — tree + inclusion **and range** proofs + tamper-rejection (contiguous-range proof via
      depth-climb, off-path-only boundary nodes, ADR-0017) + **length-extension `upgrade` proofs**
      (data-free consistency / anti-fork-across-lengths, ADR-0020) + **byte-offset seek** (tree-accelerated
      `seek` == linear scan + standalone data-free `SeekProof` locating byte→block against the signed root,
      ADR-0022) + **node recovery** (repair mode + data-free `NodeProof` authenticating any tree node against
      the signed root + atomic `recover_node`, ADR-0023) + **truncate** (pure in-memory rewind to a
      prefix — node-for-node identical to a fresh prefix, so `root_hash` is the prefix root — plus
      `byte_length`, ADR-0024) + **reorg / lowest-common-ancestor** (`lowest_common_ancestor` = the
      content-blind shared-prefix length via a monotone binary search over prefix root hashes;
      `reorg` keeps the LCA prefix and adopts the other tree's divergent suffix, byte-identically;
      fork-agnostic — the secure replica-level gate is deferred, ADR-0025); upstream
      `additionalNodes`/seek-`padding`/reorg-by-proof + replication-driven repair tracked separately
      on `merkle-tree.js`/`merkle-tree-recovery.js`
- [x] `codec` — round-trip + versioned/tolerant decode
- [x] `identity` — sign/verify + forgery-rejection + **multi-signer manifest verifier** (`Manifest`:
      content-addressed `hash()` = the would-be log key, `Manifest::single(pk).hash()` ≡
      `Hypercore.key(pk)`; `sign`/`verify` over a head `(length, tree_hash)` enforcing the multisig
      **quorum** rule — ≥ quorum distinct in-range signers, each valid over a manifest-hash-bound
      `signable` — with a `Prologue` prefix self-authorizing on content; non-ed25519 signers structurally
      impossible, invalid configs rejected at construction; ADR-0035 — wiring into `Hypercore` + compat
      v0 / `allowPatch` / multisig wire format / session `moveTo` tracked on `manifest.js`)
- [x] `storage` — trait + in-memory backend
- [x] `storage` — **sparse bitfield** (`Bitfield`: get / set / set_range / count(start,**length**,val) /
      find_first / find_last over an unbounded, sparse, paged local presence map; clean-room of
      `bitfield.js`'s L1 data structure — `find_first(false,..)` always `Some` (infinite-zero tail),
      a missing page is an all-`false` page never materialized on clear; persistence `open`/`flush` and
      replication `want` chunking out of scope, ADR-0030) — the foundation for `clear`/`purge`/sparse cores
- [ ] `storage` — IndexedDB backend (wasm)
- [x] `hypercore` — append/get/verify + proof-based replication + **batch / atomic append**
      (stage → single-head commit, stale-base reject, all-or-nothing rollback; ADR-0018) +
      **fork detection** (`conflicting_heads` same-length/different-root + per-index `ForkProof`; ADR-0019) +
      **verified length-extension replication** (`Replica::verify_upgrade` — accept a longer signed head only
      as an append-only extension of the replica's own roots *before* fetching the new blocks; ADR-0021) +
      **truncate** (logical rewind to a prefix + a signed `fork` counter; equivocation refined to a
      *same-fork* contradiction so a fork-bumped reorg is not flagged; ADR-0024) +
      **secure replica-level reorg** (`Replica::verify_reorg`/`reorg` — follow only a *strictly
      higher*-fork signed head, authenticate the claimed shared-prefix length by re-anchoring an
      `UpgradeProof` on the replica's own roots at that prefix, then drop the divergent suffix and
      refetch byte-identically; an over-claimed ancestor / forked old block is rejected; ADR-0026) +
      **sparse `clear`** (a `storage::Bitfield` presence map: `has`/`contiguous_length` ≡ upstream
      `has`/`contiguousLength`; `clear(start,end)` drops present blocks' bytes + bits returning the
      count cleared, leaving the Merkle tree / signed head **untouched** so a cleared block stays
      authenticated + re-verifiable from a holder; `get`/`block` read `None` for an absent block;
      clearing absent/out-of-range blocks is a no-op; ADR-0031) +
      **snapshots** (`Hypercore::snapshot()` → a self-contained by-value `Snapshot<T,C>` owning the
      present blocks + tree + signed head at capture time, immune to the core's later
      append/truncate/re-append; `length`/`fork`/`head`/`get`/`block`/`proof` fixed, `get` past the end
      `None`, a captured block independently authenticated against the snapshot head, and
      `signed_length(&core)` ≡ upstream `signedLength` via the shared-prefix LCA; ADR-0032 — by-value
      divergence from upstream's shared-storage COW; `signedLength`-over-replication / implicit-download
      snapshots deferred, tracked on `snapshots.js`) +
      **read/byte streams** (`read_stream(ReadStreamOptions{start,end,reverse,live})` → a no-wait
      `ReadStream` iterator over the decoded blocks in `[start,end)` ≡ upstream `createReadStream` —
      `end` clamped to `len`, absent/cleared blocks skipped, `live` accepted-but-ignored so "live should
      be ignored" ports directly; `byte_stream(ByteStreamOptions{byte_offset,byte_length})` → a
      `ByteStream` iterator over whole **encoded** blocks covering a byte range ≡ `createByteStream`,
      `seek`-located, empty-payload blocks still emitted; encoded-byte (padding-free, ADR-0022)
      addressing; ADR-0033 — `live` tailing / duplex backpressure / sub-block slicing / write-stream
      object deferred, tracked on `streams.js`) +
      **prologue migration / move-to** (`Prologue { length, hash }` = a content-addressed commitment to a
      prefix; `prologue_at` mints one from a source ≡ upstream `{ length, core.state.hash() }`;
      `with_prologue` creates a fresh core under a *new* key bound to it; `copy_prologue` ≡ `copyPrologue`
      content-checks `source.prefix_root_hash(length) == hash` then adopts the prefix **by value**,
      re-signing it under the new key; `verify_prologue` is the maintained invariant and the prologue
      length is a `truncate` floor — the L1 of `move-to.js`'s "move - basic"/"move - snapshots", ADR-0034;
      the full multi-signer manifest/`Verifier`/`multisig` + manifest-into-key identity, and the
      session-level `moveTo`/`migrate` re-homing, deferred) — `purge`/physical-reclamation
      guarantees + the replication re-download that refills a cleared block tracked on `clear.js`
- [x] `autobase` — linearizer (causal order + deterministic tiebreak); **`topolist.js` stable-ordering
      ported** — a host-safe in-Rust re-statement of upstream's non-optimistic insertion sort cross-checks
      that priority-Kahn `order()` agrees node-for-node with it (both = the lex-minimal linear extension)
      over the `DESIGN.md` DAGs + 200 seeded random fork/merge DAGs × delivery orders (ADR-0027)
- [~] `autobase` — quorum / finality-stability (recursive quorum degree + double-quorum finalized
      prefix + stability property done; fork/merge competition + 2-degree-lead caveat deferred, ADR-0015)
      + **view materialization** (`view`/`view_len`/`view_get` ≡ upstream `view`/`view.length`/`view.get`;
      `indexed_view_len` ≡ `getIndexedViewLength` — the fork-free `linearizer.js`/`dags.js` "simple" chain
      asserts the exact upstream `view.length 6` / `getIndexedViewLength 4`, plus cross-replica view +
      indexed-length convergence over every DAG; ADR-0028. The L1 fold is identity — one node, one entry —
      since apply is domain logic; the fork-case confirmed lengths await the deferred consensus)
- [x] convergence simulation (gate #3) — `crates/autobase/tests/convergence.rs`: seeded random
      partitioned/cooperative DAGs; order + state + finalized converge across delivery orders;
      finalized prefix monotone under cooperative growth (ADR-0016)
- [ ] JS algorithmic-equivalence oracle (gate #4)
- [ ] WASM runtime / IndexedDB (gate #2)
- [ ] relevant upstream tests ported (see `UPSTREAM_TEST_MAP.md`)
- [ ] `hyperbee` (only if needed)

### Audit follow-ups (after iteration 21; see ADR-0029)
- [x] `merkle` `SeekProof::verify` rejects non-leaf targets (P0 soundness) + `Proof`/`SeekProof`
      sibling-index guards (P1) — `seek_rejects_non_leaf_target`, `proof_rejects_falsified_sibling_index`
- [x] `hypercore`: replica rejects a block proven under a *different* same-author head (cross-head root
      binding) and under a wrong author key (negative-path gaps) —
      `add_block_binds_proof_to_the_specific_head` (a proof bound to one head's root is refused under a
      same-author fork head *and* a longer honest head, both directions; nothing stored),
      `add_block_rejects_wrong_author` (a replica keyed to A refuses an internally-honest log signed by B)
- [x] `hypercore`: atomic append — first/last-block fault injection + `delete`-failure handling
      (`commit_fault_on_first_staged_block_is_atomic` — abort before any write, storage pristine;
      `commit_fault_on_last_staged_block_rolls_back_all` — both earlier writes rolled back, no orphans;
      `commit_rollback_tolerates_delete_failure` — a swallowed rollback `delete` still surfaces the
      original `put` error and keeps logical state atomic, leaving one unreachable orphan that a later
      commit overwrites; all three recover byte-identically to the canonical six-block head)
- [x] `hypercore`: `verify_reorg` head-`None` branch — a reorg adopts a *strictly higher* fork than
      the one we trust, so a replica with no verified head has no current fork to gate against and
      refuses any reorg, untouched (`verify_reorg_requires_a_trusted_head`): a fresh empty replica
      refuses even an `ancestors == 0` from-scratch offer, and a replica *mid-reorg* (shared prefix
      kept, `head == None` while the suffix refetch is pending) refuses a second higher-fork reorg yet
      still completes its original refetch byte-identically
- [x] `merkle`: reorg / `lowest_common_ancestor` adversarial — corrupt `other`, gapped `self`,
      monotonicity-precondition violation; seek zero-size block
      (`lca_conservative_under_corruption` — a gap reads as disagreement, so corruption can only
      *shrink* the LCA, never over-claim; the returned length is always a genuine shared prefix even
      when the `agree` predicate is non-monotone; `lca_intact_agreement_is_monotone` — the
      binary-search precondition holds for intact inputs; `reorg_precondition_on_intact_other` —
      reorg copies `other`'s gaps verbatim (intact-other required) yet an intact `other` heals a
      gapped `self`; `seek_handles_zero_size_blocks` — empty blocks are skipped as seek targets,
      `seek` == linear scan, seek proofs authenticate, all-empty tree has no locatable byte)
- [x] `autobase`: quorum-degree *value* cross-checked against an independent computation over random
      DAGs (`crates/autobase/tests/quorum.rs`) — a **fixpoint-relaxation** reference oracle derived
      straight from the `DESIGN.md` recursion over inclusive causal closures (author self-vote
      *emergent*, not hardcoded), distinct from production's single-pass topological DP; the oracle is
      first validated against the `DESIGN.md` worked examples (chain 3/2/1/0, higher quorum 2/1,
      competing 1/1), then `quorum_degree(target)` is asserted equal to it node-for-node over seeded
      random partitioned DAGs × 3 indexer-set sizes × several causally-valid delivery orders, with
      non-vacuity guards (degrees 0/1/≥2 all occur, a double quorum forms)
