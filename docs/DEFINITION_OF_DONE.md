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
   verovio-style deterministic oracle, at the algorithm level.

## The gate

```
just verify       # always-runnable: cargo test --workspace + wasm compile
just verify-full  # verify + wasm-test (chrome) + oracle (node)
```

`just verify-full` green **and** all boxes in this file + `UPSTREAM_TEST_MAP.md` ticked = **done**.

## Checklist (high level)

- [x] Workspace scaffold (no data types)
- [ ] `merkle` — tree + proofs + tamper-rejection
- [ ] `codec` — round-trip + versioned/tolerant decode
- [ ] `identity` — sign/verify + forgery-rejection
- [ ] `storage` — trait + in-memory backend
- [ ] `storage` — IndexedDB backend (wasm)
- [ ] `hypercore` — append/get/verify + proof-based replication
- [ ] `autobase` — linearizer (causal order + deterministic tiebreak)
- [ ] `autobase` — quorum / finality-stability
- [ ] convergence simulation (gate #3)
- [ ] JS algorithmic-equivalence oracle (gate #4)
- [ ] WASM runtime / IndexedDB (gate #2)
- [ ] relevant upstream tests ported (see `UPSTREAM_TEST_MAP.md`)
- [ ] `hyperbee` (only if needed)
