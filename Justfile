# hypercore-rs task runner. See docs/DEFINITION_OF_DONE.md.
# `just verify` is the loop's red/green gate.

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
    cargo build --target wasm32-unknown-unknown -p hypercore -p autobase -p storage

# WASM runtime test: IndexedDB persistence in headless Chrome. Requires wasm-pack + Chrome.
wasm-test:
    wasm-pack test --headless --chrome crates/storage

# JS algorithmic-equivalence oracle: compare our linearizer to reference/js/autobase. Requires node.
# (Implemented as an ignored cargo test that shells out to node; enabled once the linearizer exists.)
oracle:
    cargo test -p autobase --features oracle -- --ignored oracle
