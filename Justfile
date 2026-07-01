# hypercore-rs task runner. See docs/DEFINITION_OF_DONE.md.
# `just verify` is the loop's red/green gate.

# Run one porting-loop iteration (spawns a headless agent, then re-checks the gate). e.g. `just iter 1`
iter n:
    scripts/iterate.sh {{n}}

# Run a range of iterations, stopping on a red gate or DONE. e.g. `just iter-range 1 5`
iter-range n m:
    scripts/iterate.sh {{n}} {{m}}

# Always-runnable gate: native tests (unit + property + ported upstream + convergence sim) + wasm compile.
verify: test wasm

# Full gate, incl. heavier tooling: + headless-chrome wasm runtime + node oracle.
verify-full: verify wasm-test oracle

# Native test suite across the workspace.
test:
    cargo test --workspace

# WASM compile gate (the WASM-first goal). Requires the wasm32 target:
#   rustup target add wasm32-unknown-unknown
wasm:
    cargo build --target wasm32-unknown-unknown -p hypercore -p autobase -p storage -p hyperbee -p roomnet

# WASM runtime test: OPFS persistence in headless Chrome. Requires wasm-pack + Chrome.
wasm-test:
    # Runs the OPFS worker tests in real headless Chrome: storage::opfs (raw KV) and
    # hypercore::opfs_browser_tests (a full Hypercore persist→reopen over OpfsStore — the
    # local-first payoff end-to-end). wasm-pack fetches the latest chromedriver, which must
    # match the installed Chrome's MAJOR version; if Chrome lags, fetch a version-matched
    # chromedriver and run cargo test via the matching wasm-bindgen-test-runner with
    # CHROMEDRIVER set (wasm-pack ignores CHROMEDRIVER).
    RUSTFLAGS='--cfg=web_sys_unstable_apis' wasm-pack test --headless --chrome crates/storage --features opfs
    RUSTFLAGS='--cfg=web_sys_unstable_apis' wasm-pack test --headless --chrome crates/hypercore --features opfs

# JS algorithmic-equivalence oracle: compare our linearizer to reference/js/autobase.
# Runs node ONLY inside a container via scripts/node-sandbox.sh (untrusted npm tree — see CLAUDE.md
# rule 7); requires a container runtime (Apple `container` or docker). Enabled once the linearizer exists.
oracle:
    cargo test -p autobase --features oracle -- --ignored oracle
