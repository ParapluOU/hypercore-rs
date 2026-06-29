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
