# Architecture Decision Records

Terse, append-only. Record **every divergence from upstream** here.

## ADR-0001 — Clean-room, not a verbatim port
**Context:** Upstream is JS; we want a Rust substrate shaped for our consumer, not interop.
**Decision:** Reimplement behaviour; we are **not** wire-/disk-/JS-compatible. Cherry-pick ideas.
**Consequence:** No output-equivalence oracle; correctness comes from property tests + ported
upstream behaviour tests + the algorithmic oracle (ADR-0008).

## ADR-0002 — Typed log, generic over blob content
**Decision:** `Hypercore<T, C: Codec<T>>`; `T` is erased to bytes at the storage/proof boundary.
Ordering and verification stay content-blind.
**Consequence:** Ergonomic win over opaque buffers; the codec must be versioned & tolerant
(immutable history + content addressing make the wire format a permanent ABI).

## ADR-0003 — Networking deferred to Iroh
**Decision:** Do not port hyperswarm / HyperDHT / Noise / `hypercore-protocol-rs`. Build cores
against storage/transport abstractions; layer Iroh (core / blobs / gossip) underneath later.

## ADR-0004 — Monorepo Cargo workspace
**Decision:** One workspace; shared internals (`codec`, `merkle`, `identity`, `storage`) as their
own crates rather than copy-pasted, unlike upstream's per-package repos.

## ADR-0005 — WASM-first + browser storage
**Decision:** `hypercore`, `autobase`, and `storage` must build for `wasm32-unknown-unknown` and
run in a browser host; `storage` ships a `localStorage`/IndexedDB backend so logs persist locally.

## ADR-0006 — Loop log / lessons / decisions live in-repo
**Decision:** Keep the porting log, lessons, and ADRs under `docs/` (committed), not in a private
machine-local memory, so any instance/contributor sees full history.
**Consequence:** They are public → ADR-0010 applies.

## ADR-0007 — Convergence test is generic-only
**Decision:** The collaborative-editing test uses a generic toy document in this repo. Any
domain-specific (e.g. real editing-session) integration test lives in the **consuming** app, not
here — it would couple L1 to a domain and risk leaking.

## ADR-0008 — Upstream-JS algorithmic-equivalence oracle
**Decision:** Cross-check our linearizer against `reference/js/autobase` on random DAGs (via node):
same DAG ⇒ identical linearization order.
**Consequence:** Recovers a deterministic, upstream-anchored oracle without wire/disk compat.

## ADR-0009 — Port relevant upstream test suites
**Decision:** Translate the in-scope upstream tests into Rust tests as behavioural specs; track in
`docs/UPSTREAM_TEST_MAP.md`. Exclude networking / wire / disk-interop / sessions / encryption.

## ADR-0010 — Public repo: no private or personal data
**Decision:** Never commit absolute disk paths, machine/user names, emails, tokens/secrets,
internal hostnames, or consumer-project internals — in code, tests, docs, or the porting log. Use
repo-relative paths; sanitize tool output before committing.

## ADR-0011 — Run JS reference only in a sandbox/container
**Context:** The JS oracle (ADR-0008) executes `reference/js/autobase` via node, pulling an
untrusted npm dependency tree (supply-chain exploits are common).
**Decision:** Never run `npm`/`node` against the reference on the host. Route it through
`scripts/node-sandbox.sh` — a container runtime (Apple `container` or docker), with npm install
scripts disabled and optional no-network at runtime. Reading/porting JS to Rust is host-safe;
*executing* it is not.
**Consequence:** The `oracle` gate and any reference-JS execution go through the wrapper; a host with
no container runtime cannot run them, by design.

## ADR-0012 — Single-writer loop; no code-editing subagents
**Context:** Parallel editing agents within one iteration would race on files and the porting log.
**Decision:** Each iteration has exactly one writer — the iteration agent. It may spawn read-only
exploration subagents (e.g. `Explore`) for searching, but must not spawn code-editing subagents or
delegate edits.
**Consequence:** Enforced by the driver prompt (`scripts/iterate.sh`) and CLAUDE.md; subagent use in
the loop is read-only.

## ADR-0013 — Scoped permission allowlist for the driver (no blanket bypass)
**Context:** Headless driver agents must not block on approval prompts, but
`--permission-mode bypassPermissions` disables *all* gates — an autonomous loop running arbitrary
shell, which is unsafe (and was correctly flagged).
**Decision:** Run with `--permission-mode acceptEdits` plus a scoped `.claude/settings.json`
allowlist: allow `cargo` / `rustup` / `rustc` / `just` / `wasm-pack` / `scripts/node-sandbox.sh` and
non-push `git`; **deny** `git push`, `rm`, `curl`/`wget`, and host `node`/`npm`/`npx` (node only via
the sandbox wrapper — reinforces ADR-0011). Subagents are not allowlisted → strictly single-writer.
**Consequence:** Autonomous iterations are constrained to build / test / commit. They cannot push,
delete, exfiltrate over the network, or run untrusted node on the host.

## ADR-0014 — Linearizer is a priority-Kahn topo sort, not upstream's incremental tip
**Context:** Upstream (`reference/js/autobase/lib/topolist.js`) keeps an incremental sorted "tip"
and shuffles each arriving node into place (`moveDown`/`moveUp`), tracking `undo`/`shared` so a
streaming view can be patched cheaply. That bookkeeping is an optimization for live updates, not the
ordering definition.
**Decision:** Reimplement the *behaviour* with a **priority Kahn topological sort**: recompute the
order each call, at every step emitting the causally-ready node with the smallest `NodeId`
(`(writer_key, seq)`, lowest-key-first — the documented "lowest key wins" tiebreak). Enforce causal
delivery on `add` (no duplicate / no seq gap / no dangling head) so the DAG is always acyclic and
causally closed.
**Consequence:** Determinism is *manifest* — `order()` is a pure function of the node set,
independent of arrival order — and it reproduces the canonical linearizations in
`reference/js/autobase/DESIGN.md` (incl. the recursive `[a0, c0, a1, b0, b1, c1, b2]` example). We
do **not** port the `undo`/`shared` reorder-tracking (a streaming optimization) nor the
consensus/quorum confirmation (next capability; the upstream `linearizer.js`/`dags.js` assertions on
*indexed* view length depend on it). Equivalence is at the linearization level for causally-closed
DAGs.

## ADR-0015 — Quorum is a recompute-from-scratch degree; finality is the conservative snapshot form
**Context:** Upstream confirmation lives in `reference/js/autobase/lib/consensus.js` — an
*incremental* `Consensus` machine over vector clocks (`confirms`/`shift`/`_isConfirmed`/
`_isConfirmableAt`, plus merge bookkeeping) that streams the indexed view as nodes arrive. The
*definition* of a quorum, though, is in `DESIGN.md` ("Quorums"): a **vote** is a reference from an
indexer to a node; a node has a degree-1 quorum once a majority of indexers reference it, and the
degree increases each time a majority reference the lower-degree quorum.
**Decision:** Reimplement the *definition*, not the machine. `quorum_degree(target)` is a single
bottom-up pass over a topological order (`order()`): for each node we carry, per indexer, the best
degree any of that indexer's nodes reached over the target within its causal closure, and a node
witnesses degree `k` once a majority vouch level `k-1` (its own author vouching every level up to its
degree). Votes are read purely from causal reachability (`sees`, the graph equivalent of
`clock.includes`) — never a timestamp or a payload. `finalized()` returns the conservative
**snapshot / no-active-fork** prefix: the maximal prefix of `order()` whose nodes have a **double
quorum** (degree ≥ 2) *and* are causally comparable to every other node (no unresolved concurrent
fork around them).
**Consequence:** Determinism is manifest (a pure function of the DAG ⇒ replicas seeing the same set
agree) and the recursive degree reproduces every worked `DESIGN.md` example (the `a0` 1'/2'/3'
quorum chain; the `c0-b0-c1` higher quorum; the conflicting single-quorum pair that must *not*
finalize). We **defer** two things, each its own iteration: (a) the fork/merge competition rule and
the **2-degree-lead caveat** (`DESIGN.md` "Tails, Forks and Merges // todo"; `consensus.js` merge
handling) — `finalized()` refuses to commit either arm of an unresolved fork until a confirmed merge
makes the contested nodes comparable, which is safe but conservative (it may confirm later than
upstream, never earlier/wrongly); and (b) view materialization, so the upstream
`getIndexedViewLength`/`view.get` assertions in `linearizer.js`/`dags.js` stay `[~]`. Finality is
validated as a *property* (a finalized prefix never reorders under cooperative growth), to be
strengthened against arbitrary partitions by the convergence sim (gate #3) and the JS oracle
(gate #4).
