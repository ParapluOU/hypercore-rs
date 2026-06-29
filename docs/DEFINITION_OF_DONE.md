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
- [x] `identity` — sign/verify + forgery-rejection
- [x] `storage` — trait + in-memory backend
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
      refetch byte-identically; an over-claimed ancestor / forked old block is rejected; ADR-0026)
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
- [ ] `hypercore`: `verify_reorg` head-`None` branch (untested)
- [ ] `merkle`: reorg / `lowest_common_ancestor` adversarial — corrupt `other`, gapped `self`,
      monotonicity-precondition violation; seek zero-size block
- [ ] `autobase`: quorum-degree *value* cross-checked against an independent computation over random
      DAGs (today only convergence + monotonicity are fuzzed, not the degree value)
