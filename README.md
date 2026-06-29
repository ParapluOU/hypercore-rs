# hypercore-rs

Rust distillations of the [Holepunch](https://github.com/holepunchto) data structures —
**Hypercore**, **Autobase**, and (likely) **Hyperbee** — rebuilt as the secure, distributed,
append-only log substrate for [Parture](https://parture.org).

> **Not an upstream-compatible port.** Wire-format, disk-format, and JS-interop compatibility
> are explicit *non-goals*. These are clean-room Rust reimplementations shaped for Parture's
> needs, using the upstream JavaScript and the [datrs](https://github.com/datrs) Rust ports as
> reference only. We cherry-pick the *ideas*, not the *compatibility baggage*.

---

## Why this exists

A collaborative application built around an operation log eventually grows, in effect, a
domain-specific op-based CRDT: it linearizes a causal DAG of edits and folds them into application
state. The easy mistake is to **fuse two concerns that should be separate layers** — the
distributed log itself and the domain merge semantics — into one type:

- **L1 — transport / causality (this repo):** signed append-only logs, content addressing, causal
  references, deterministic multi-writer linearization. **Domain-agnostic** — it never inspects
  payload internals.
- **L2 — merge semantics (the application):** the domain op vocabulary, insertion anchors, stable
  entity IDs, tombstones. **Consumes an order; never touches the wire.**

This repo distills the L1 substrate — which is exactly what Hypercore + Autobase already are — so
an application's op-based CRDT can sit cleanly on top, instead of re-implementing transport,
identity, and causal ordering by hand and welding them to its domain types.

The one *legitimate* coupling between the layers is **causal delivery**: L2's preconditions ("an
anchor must exist before something attaches to it") are only satisfiable because L1 guarantees a
node's causal references are delivered before it. That is an interface contract, not an
implementation entanglement.

---

## Monorepo layout

Unlike upstream — where `hypercore`, `autobase`, `hyperbee`, `corestore`, `hypercore-crypto`, etc.
are each a **separate npm package and repo** — this is a single **Cargo workspace** of related
crates. The shared pieces (codec, Merkle/verified storage, identity) are factored into their own
crates instead of being copy-pasted across repos.

```
hypercore-rs/                  # Cargo workspace (monorepo)
├── Cargo.toml                 # [workspace] root
├── crates/
│   ├── hypercore/             # typed, signed, append-only log (the core primitive)
│   ├── autobase/              # multi-writer causal linearizer over many hypercores
│   ├── hyperbee/              # ordered index / materialized view (maybe — see below)
│   ├── merkle/                # shared: BLAKE3 tree, range/inclusion proofs
│   ├── codec/                 # shared: typed-payload <-> bytes, versioned & tolerant
│   └── identity/              # shared: ed25519 author keys, signing/verification
└── reference/                 # read-only upstream sources (git submodules, study only)
    ├── rust/datrs-hypercore
    └── js/{hypercore, autobase, hyperbee}
```

(Crate names/boundaries are provisional; the point is one workspace, shared internals.)

---

## Components

### `hypercore` — typed, signed, append-only log

A single-writer, hash-linked, append-only log. Each entry is signed by the author key; the log is
a BLAKE3/Merkle structure enabling verified random access and range proofs.

**Headline design departure from upstream:** the log is **generic over a typed payload `T`** with a
pluggable codec, rather than opaque `Buffer` / `&[u8]`.

```rust
Hypercore<T, C: Codec<T>>     // typed, ergonomic API surface
        │  C::encode / C::decode
        ▼
      (bytes)                 // Merkle tree, signatures, storage & proofs are content-blind
```

`T` is real at the API and **erased to bytes at the storage/proof boundary**. Ordering and
verification must *never* inspect `T`'s fields — if they ever need to, that is the tell that
domain semantics have leaked into the transport.

> **Schema permanence.** A log is immutable history *forever*, so the codec must be **versioned and
> tolerant** (self-describing frames, `#[non_exhaustive]` enums, unknown-variant skipping). Changing
> the encoding changes every content address, so the wire format is a permanent ABI. This is the
> single biggest footgun of baking the content type into a permanent log, and the reason upstream
> stayed on bytes — we accept the typing and pay for it with codec discipline.

### `autobase` — multi-writer causal linearizer

Combines multiple `hypercore`s (one per writer) into a single deterministic, eventually-consistent
order. Ordering is **causal** — each node carries a *clock* that is a set of references to other
writers' heads — tie-broken deterministically by key, and finalized by **indexer quorums**.
Crucially, it is **not** ordered by timestamps, which is what makes "inventing crazy append times"
a non-attack: time is never consulted.

The hand-rolled version of this that applications build when they lack this layer — a per-writer
back-reference chain plus a self-reported scalar clock — is exactly what we want to *replace*: a
forgeable scalar ordering gives way to causal-DAG order plus a deterministic tiebreak.

### `hyperbee` — ordered index / materialized view *(probably)*

An append-only B-tree over a hypercore: ordered keys, range queries — the materialized-view/index
layer. An application may use this directly, or subsume it into its own view/index layer. Included
in the plan; priority TBD.

---

## Explicitly out of scope (for now)

- **Networking / replication / wire protocol.** We do **not** port hyperswarm, the HyperDHT, the
  Noise transport, or `hypercore-protocol-rs`. Networking is deliberately deferred and will be
  **reinvented on top of [Iroh](https://iroh.computer)**, which already provides most of it:
  - `iroh` core → QUIC transport, hole-punching, node identity (ed25519 pubkey), discovery
  - `iroh-blobs` → BLAKE3 verified, content-addressed storage/streaming (the verified-range property)
  - `iroh-gossip` → broadcast for head/entry advertisement and anti-entropy

  The cores here are therefore built against **storage/transport abstractions**, not a concrete
  network, so the Iroh layer can slot in underneath later.
- **Upstream disk-format and wire-format compatibility.**
- **JS interoperability.**

---

## Design goals

1. **Typed, generic over blob content** — `Hypercore<T, Codec>`, erased to bytes at the proof/storage
   boundary. The headline ergonomic win over upstream's opaque buffers.
2. **Content-blind ordering & verification** — the L1 layer never reads payload internals.
3. **Signed identity** — author = ed25519 key (maps cleanly onto an Iroh `NodeId`); every entry
   signed. No forgeable plaintext agent ids.
4. **Storage/transport-abstract** — pluggable backends, no hard network dependency, and
   **WASM-friendly** (must build for `wasm32-unknown-unknown`).
5. **Monorepo with shared internals** — one workspace; codec, Merkle, and identity factored out
   rather than duplicated.
6. **Maximally useful for Parture, not faithful to upstream.**

---

## Reference material (git submodules)

Vendored read-only under `reference/` for study. Nothing here depends on them at build time.

| Path | Upstream | Lang | Role |
|------|----------|------|------|
| `reference/rust/datrs-hypercore` | [datrs/hypercore](https://github.com/datrs/hypercore) | Rust | existing partial byte-level port + Merkle proofs |
| `reference/js/hypercore` | [holepunchto/hypercore](https://github.com/holepunchto/hypercore) | JS | original append-only log |
| `reference/js/autobase` | [holepunchto/autobase](https://github.com/holepunchto/autobase) | JS | original multi-writer linearizer (see its `DESIGN.md`) |
| `reference/js/hyperbee` | [holepunchto/hyperbee](https://github.com/holepunchto/hyperbee) | JS | original append-only B-tree |

```sh
git clone --recurse-submodules https://github.com/ParapluOU/hypercore-rs.git
# or, after a plain clone:
git submodule update --init --recursive
```

---

## Status / roadmap

- [ ] Workspace scaffold (`Cargo.toml` + `crates/*`)
- [ ] `codec`: versioned, tolerant typed-payload ⇄ bytes
- [ ] `merkle`: BLAKE3 tree + range/inclusion proofs (study datrs + iroh-blobs)
- [ ] `identity`: ed25519 author keys + entry signing/verification
- [ ] `hypercore`: typed single-writer append-only log over the above
- [ ] `autobase`: causal DAG order + deterministic tiebreak
- [ ] `autobase`: indexer/quorum finalization
- [ ] `hyperbee`: ordered index (if needed)
- [ ] Iroh-backed networking layer (later)
- [ ] integrate an application op-based CRDT as the L2 consumer

---

## License

TBD.
