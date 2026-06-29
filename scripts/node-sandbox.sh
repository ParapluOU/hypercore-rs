#!/usr/bin/env bash
#
# Run a node/npm command against the UNTRUSTED JS reference inside a container.
# NEVER run npm/node for reference/js/* directly on the host (supply-chain risk).
#
#   scripts/node-sandbox.sh <subdir> <cmd...>
#     scripts/node-sandbox.sh reference/js/autobase npm ci --ignore-scripts
#     scripts/node-sandbox.sh tools/oracle        node run.mjs
#
# Behaviour:
#   - mounts the repo at /work, runs in /work/<subdir>
#   - disables npm install/postinstall scripts (npm_config_ignore_scripts=true)
#   - prefers Apple's `container` (macOS), falls back to `docker`; refuses on the host
#   - set HC_NO_NET=1 to also cut network (use for the run step, not for install)
#   - override the image with HC_NODE_IMAGE
#
set -euo pipefail

WORKDIR="${1:?usage: node-sandbox.sh <subdir> <cmd...>}"; shift
[ $# -ge 1 ] || { echo "usage: node-sandbox.sh <subdir> <cmd...>" >&2; exit 2; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${HC_NODE_IMAGE:-node:lts-bookworm-slim}"

NET=""
[ -n "${HC_NO_NET:-}" ] && NET="--network none"

if command -v container >/dev/null 2>&1; then
  # Apple `container` CLI (macOS).
  exec container run --rm $NET \
    -v "$ROOT:/work" -w "/work/$WORKDIR" \
    -e npm_config_ignore_scripts=true \
    "$IMAGE" "$@"
elif command -v docker >/dev/null 2>&1; then
  exec docker run --rm $NET \
    --cap-drop ALL --security-opt no-new-privileges \
    -v "$ROOT:/work" -w "/work/$WORKDIR" \
    -e npm_config_ignore_scripts=true \
    "$IMAGE" "$@"
else
  {
    echo "error: no container runtime found (Apple 'container' or 'docker')."
    echo "Refusing to run npm/node against the JS reference on the host (supply-chain risk)."
    echo "See CLAUDE.md rule 7."
  } >&2
  exit 1
fi
