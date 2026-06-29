# hypercore-rs — agent guide

Clean-room Rust distillation of **Hypercore / Autobase / (Hyperbee)** as a domain-agnostic,
secure, append-only log substrate. **This is a public repo.** Read `README.md` for the
architecture and `docs/DEFINITION_OF_DONE.md` for what "done" means.

## Working rules

1. **Clean-room, not verbatim.** We do **not** transliterate upstream line-by-line, and we are
   **not** wire-/disk-/JS-compatible. Reimplement *behaviour*; cherry-pick *ideas*. (This is the
   opposite of a verbatim port — divergence is expected and good.)
2. **Public repo — never commit private or personal data.** No absolute disk paths
   (`/Users/...`, `/home/...`, `C:\...`), no machine/user names, no emails, tokens, or secrets,
   no internal hostnames, no consumer-project internals. Use **repo-relative paths** in all code,
   tests, docs, and the porting log. **Sanitize tool output before pasting it into anything
   committed.**
3. **L1 only — domain-agnostic.** Ordering and verification must never inspect a payload's
   contents. No application/domain types live here; tests use generic/toy payloads.
4. **Every test asserts.** Tests must fail honestly — never `#[ignore]` or `#[should_panic]` to
   hide a gap.
5. **Record every divergence from upstream as an ADR** in `docs/DECISIONS.md`.
6. **Networking is out of scope** (deferred to Iroh). Skip upstream tests about
   replication / wire protocol / disk-format interop / sessions / encryption — the relevance
   filter is `docs/UPSTREAM_TEST_MAP.md`.
7. **Never run `npm`/`node` for the JS reference on the host.** The upstream npm dependency tree is
   untrusted (supply-chain exploits). Run any reference JS — including the algorithmic-equivalence
   oracle — inside a sandbox/container via `scripts/node-sandbox.sh` (containerized, npm install
   scripts disabled). Porting an upstream test means **reading** the JS and reimplementing it in
   Rust, *not* executing it on the host.
8. **Single writer — no code-editing subagents.** The iteration agent makes every edit itself.
   Read-only exploration subagents (e.g. the `Explore` agent) are fine for searching the code; never
   spawn a code-editing subagent or run edits in parallel. (Fittingly: this is a single-writer log.)

## The loop

One iteration:
1. Read `docs/PORTING_LOG.md` (state) and `docs/DEFINITION_OF_DONE.md` + `docs/UPSTREAM_TEST_MAP.md`
   (what's red).
2. Pick the next red item — a capability or an upstream test to port. Study the matching source
   under `reference/`.
3. Implement it; write or port its test until green.
4. Run `just verify`.
5. Append an entry to `docs/PORTING_LOG.md` (what / decisions / lessons / next). Move any reusable
   gotcha to `docs/LESSONS.md`; any divergence to `docs/DECISIONS.md`; tick the relevant boxes.
6. Commit. End the message with the `Co-Authored-By` trailer.

**Done** = `just verify-full` green **and** every box in `docs/DEFINITION_OF_DONE.md` and
`docs/UPSTREAM_TEST_MAP.md` ticked.

## Running iterations (for the operator)

Iterations are driven by a script, invoked by number — not on a timer:

```sh
just iter 1            # run iteration 1
just iter-range 1 5    # run 1..5, stopping on a red gate or LOOP-DONE
scripts/iterate.sh 7   # same as `just iter 7`
```

Each invocation spawns a headless agent that performs exactly one step above, then the driver
**independently re-runs `just verify`** and only accepts the iteration if it is green. Commits stay
local (nothing is pushed). Env knobs: `HC_MODEL`, `HC_BUDGET` (per-iteration USD cap), `HC_PERM`.

## Where things are

- `crates/` — the workspace (dependency graph in `README.md`)
- `reference/` — upstream sources (git submodules), read-only: study + test porting
- `docs/` — `DEFINITION_OF_DONE.md`, `UPSTREAM_TEST_MAP.md`, `PORTING_LOG.md`, `DECISIONS.md`,
  `LESSONS.md`
- `Justfile` — `just verify` (loop gate), `just wasm`, `just oracle`
