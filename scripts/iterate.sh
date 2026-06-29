#!/usr/bin/env bash
#
# Drive the hypercore-rs porting loop one iteration at a time.
#
#   scripts/iterate.sh <n>          run iteration n
#   scripts/iterate.sh <n> <m>      run iterations n..m (stops on a red gate / DONE)
#
# Each iteration spawns a headless Claude Code agent that performs EXACTLY ONE
# loop step (see CLAUDE.md + docs/), then this driver independently re-runs the
# gate `just verify`. Nothing is pushed; commits stay local.
#
# Env:
#   HC_PERM     permission mode (default: acceptEdits). Bash is governed by the
#               scoped allowlist in .claude/settings.json -- NOT blanket bypass.
#   HC_MODEL    optional --model alias/name (default: inherit configured model)
#   HC_BUDGET   optional per-iteration USD cap (--max-budget-usd)
#
set -euo pipefail

usage() { echo "usage: scripts/iterate.sh <n> [m]   (run iteration n, or range n..m)" >&2; exit 2; }

[ $# -ge 1 ] || usage
FROM="$1"; TO="${2:-$1}"
case "$FROM$TO" in *[!0-9]*) usage ;; esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

command -v claude >/dev/null || { echo "error: 'claude' CLI not on PATH" >&2; exit 1; }
command -v just   >/dev/null || { echo "error: 'just' not on PATH (needed for the verify gate)" >&2; exit 1; }

# the wasm gate needs the target; idempotent, quiet.
rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true

read -r -d '' BODY <<'EOF' || true
Read, in order: CLAUDE.md, docs/PORTING_LOG.md, docs/DEFINITION_OF_DONE.md,
docs/UPSTREAM_TEST_MAP.md, docs/DECISIONS.md, docs/LESSONS.md.

If every box in DEFINITION_OF_DONE.md and UPSTREAM_TEST_MAP.md is already ticked,
output a line containing only LOOP-DONE (nothing else on that line) and stop
without committing.

Otherwise do EXACTLY ONE iteration:
1. Pick the next red item (a capability or an upstream test to port), preferring
   the "Next" note in PORTING_LOG.md and the order in DEFINITION_OF_DONE.md.
2. Study the matching source under reference/. Implement it; write or port its
   test until it passes. Stay L1 / domain-agnostic. Obey every CLAUDE.md rule:
   clean-room (not verbatim), and NO private/personal data (repo-relative paths
   only; sanitize tool output before committing).
3. Run: just verify   -- it MUST be green before you finish.
4. Append an entry to docs/PORTING_LOG.md (what / decisions / lessons / next),
   tick the boxes you completed in DEFINITION_OF_DONE.md and UPSTREAM_TEST_MAP.md,
   move any reusable gotcha to LESSONS.md and any divergence to DECISIONS.md (a
   new ADR).
5. Commit using the "iter <n>: " prefix given above; end with the Co-Authored-By
   trailer. Do NOT push.

You are the SINGLE WRITER for this iteration: you may spawn read-only exploration
subagents (e.g. the Explore agent) to search the code, but you MUST NOT spawn
code-editing subagents or delegate any edit -- make every change yourself.

Stop after one iteration.
EOF

# Scoped permissions for the headless agent, passed on the CLI so they are honored
# even when the workspace is untrusted (headless ignores .claude/settings.json
# there). Build / test / commit only — no push, no rm, no host node/npm.
ALLOW=(
  Read Grep Glob Edit Write MultiEdit TodoWrite
  "Bash(cargo:*)" "Bash(rustup:*)" "Bash(rustc:*)" "Bash(just:*)" "Bash(wasm-pack:*)"
  "Bash(scripts/node-sandbox.sh:*)"
  "Bash(git add:*)" "Bash(git commit:*)" "Bash(git status:*)" "Bash(git diff:*)"
  "Bash(git log:*)" "Bash(git restore:*)" "Bash(mkdir:*)" "Bash(ls:*)" "Bash(cat:*)"
)
DENY=(
  "Bash(git push:*)" "Bash(rm:*)" "Bash(curl:*)" "Bash(wget:*)"
  "Bash(npm:*)" "Bash(npx:*)" "Bash(node:*)"
)

tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT

for ((i = FROM; i <= TO; i++)); do
  echo
  echo "================ hypercore-rs :: iteration ${i} ================"
  PROMPT="You are performing iteration ${i} of the hypercore-rs porting loop.
Use \"iter ${i}: \" as the commit message prefix.

${BODY}"

  # acceptEdits auto-applies file edits; Bash is constrained by the explicit
  # allow/deny lists below. No blanket bypass; subagents are not allowlisted
  # (single writer).
  args=( -p "$PROMPT" --permission-mode "${HC_PERM:-acceptEdits}" )
  [ -n "${HC_MODEL:-}" ]  && args+=( --model "$HC_MODEL" )
  [ -n "${HC_BUDGET:-}" ] && args+=( --max-budget-usd "$HC_BUDGET" )
  args+=( --allowedTools "${ALLOW[@]}" --disallowedTools "${DENY[@]}" )

  : > "$tmp"
  set +e
  claude "${args[@]}" 2>&1 | tee "$tmp"
  rc=${PIPESTATUS[0]}
  set -e

  # whole-line match: prose like "this is not LOOP-DONE" must NOT trip it
  if grep -qE '^[[:space:]]*LOOP-DONE[[:space:]]*$' "$tmp"; then
    echo "=== loop reports DONE at iteration ${i}; stopping. ==="
    break
  fi
  [ "$rc" -eq 0 ] || { echo "=== agent exited ${rc} at iteration ${i}; stopping. ===" >&2; exit "$rc"; }

  echo "---------------- verify gate (iteration ${i}) ----------------"
  if ! just verify; then
    echo "=== verify RED after iteration ${i}; stopping for inspection. ===" >&2
    exit 1
  fi
  echo "=== iteration ${i} accepted (verify green). ==="
done
