//! Unit tests for the sans-IO `Room` core (single-room, no transport).

use codec::Codec;
use hypercore::Hypercore;
use identity::SecretKey;
use storage::MemoryStore;

use crate::config::RoomConfig;
use crate::entry::{Entry, EntryCodec};
use crate::room::Room;
use crate::store::MemStoreFactory;
use crate::sync::SyncMessage;
use crate::testkit::{del, put, CounterProjection, KvProjection};
use crate::wire;
use autobase::NodeId;

fn seed(n: u8) -> SecretKey {
    SecretKey::from_seed(&[n; 32])
}

#[test]
fn wire_round_trips_every_message_including_a_real_block() {
    // Mint a genuine signed head + Merkle proof from a hypercore.
    let mut hc = Hypercore::new(seed(1), EntryCodec, MemoryStore::new());
    hc.append(&Entry::new(vec![], b"op-payload".to_vec())).unwrap();
    let writer = hc.public_key().to_bytes();
    let head = hc.head().unwrap().clone();
    let bytes = hc.block(0).unwrap().unwrap();
    let proof = hc.proof(0).unwrap();

    let msgs = [
        SyncMessage::Head { writer, head: head.clone() },
        SyncMessage::Have { writer, length: 7 },
        SyncMessage::Want { writer, start: 2, end: 9 },
        SyncMessage::Block { writer, head, index: 0, bytes, proof },
    ];
    for m in msgs {
        let encoded = wire::encode(&m);
        assert_eq!(wire::decode(&encoded).unwrap(), m, "wire round-trips {m:?}");
    }
    // A truncated buffer is a clean error, not a panic.
    assert!(wire::decode(&[0u8]).is_err(), "short buffer rejected");
}

#[test]
fn entry_round_trips_through_its_codec() {
    let e = Entry::new(
        vec![NodeId::new([7; 32], 3), NodeId::new([9; 32], 0)],
        b"payload-bytes".to_vec(),
    );
    let bytes = EntryCodec.encode(&e);
    assert_eq!(EntryCodec.decode(&bytes).unwrap(), e, "Entry survives encode/decode");
}

#[test]
fn local_append_is_reflected_in_the_live_snapshot() {
    let cfg = RoomConfig::original(seed(2), vec![]);
    let mut room = Room::open(cfg, MemStoreFactory, KvProjection::new()).unwrap();

    room.local_append(&put(b"k", b"v")).unwrap();
    assert_eq!(
        room.snapshot_live().get(b"k".as_slice()).map(Vec::as_slice),
        Some(b"v".as_slice()),
        "a just-appended PUT is visible optimistically"
    );

    room.local_append(&del(b"k")).unwrap();
    assert!(
        room.snapshot_live().get(b"k".as_slice()).is_none(),
        "a following DEL is visible optimistically"
    );
}

#[test]
fn without_indexers_nothing_finalizes_but_live_reflects_everything() {
    let cfg = RoomConfig::original(seed(3), vec![]);
    let mut room = Room::open(cfg, MemStoreFactory, KvProjection::new()).unwrap();

    room.local_append(&put(b"a", b"1")).unwrap();
    room.local_append(&put(b"b", b"2")).unwrap();

    assert_eq!(room.finalized_len(), 0, "no indexers ⇒ no quorum ⇒ nothing finalizes");
    assert!(room.snapshot_finalized().is_empty(), "finalized view is empty");
    assert_eq!(room.snapshot_live().len(), 2, "live view holds both ops");
    assert!(room.poll_finalized().is_empty(), "no finalized deltas to drain");
}

#[test]
fn finalized_deltas_are_per_mutation_and_contiguous() {
    // A single writer that is its own sole indexer finalizes a growing prefix.
    let a = seed(1);
    let ak = a.public().to_bytes();
    let cfg = RoomConfig::original(a, vec![ak]);
    let mut room = Room::open(cfg, MemStoreFactory, CounterProjection::default()).unwrap();

    let mut drained_versions = Vec::new();
    for i in 0..8u64 {
        room.local_append(&[i as u8]).unwrap();
        for f in room.poll_finalized() {
            drained_versions.push(f.version);
        }
    }

    let fin = room.finalized_len();
    assert!(fin >= 1, "a self-indexing writer must finalize a prefix (got {fin})");
    // Exactly one Finalized per finalized mutation, in order, never coalesced.
    let expected: Vec<u64> = (1..=fin as u64).collect();
    assert_eq!(drained_versions, expected, "one finalized delta per mutation, contiguous");
    // The counter projection folded exactly the finalized mutations.
    assert_eq!(*room.snapshot_finalized(), fin as u64, "finalized fold count matches");
}

/// The RELAY (homeserver) path, star topology: A and B never talk directly — every
/// frame goes through R. Proves the three relay primitives together:
///
/// 1. a fresh room's `announce()` is non-empty (the zero-length `Have` **hello** a
///    connection-oriented transport needs to register a silent joiner);
/// 2. `announce()` advertises REPLICATED writers (so a late joiner learns, from R
///    alone, that writer A exists);
/// 3. the `Want` arm re-serves a replicated writer's verified blocks (so B pulls
///    A's log *through* R, verifying against A's own signed head).
///
/// Plus the checkpoint regression: two Originals must not ping-pong checkpoints
/// forever (a peer's EMPTY checkpoint block is not itself checkpoint-worthy).
#[test]
fn late_joiner_converges_through_a_relay_it_never_shared_a_frame_with() {
    let (a, r, b) = (seed(11), seed(12), seed(13));
    let (ak, rk, bk) = (a.public().to_bytes(), r.public().to_bytes(), b.public().to_bytes());

    // Every participant is an Original self-indexer — exactly the browser/homeserver
    // shape (the node auto-hosts; each browser hosts its own session's room).
    let mut ra = Room::open(RoomConfig::original(a, vec![ak]), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rr = Room::open(RoomConfig::original(r, vec![rk]), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rb = Room::open(RoomConfig::original(b, vec![bk]), MemStoreFactory, KvProjection::new()).unwrap();

    // (1) A fresh, empty room still announces — the hello.
    let hello = rb.announce();
    assert!(
        !hello.is_empty(),
        "an empty room's announce must be non-empty (the hello a WS relay registers by)"
    );

    // A edits, then syncs with R ONLY (B is not even connected yet).
    ra.local_append(&put(b"riff", b"E5-A5-D5")).unwrap();
    let mut inbox_r: Vec<SyncMessage> = ra.announce().into_iter().map(|o| o.msg).collect();
    let mut inbox_a: Vec<SyncMessage> = Vec::new();
    for _ in 0..8 {
        let to_r = std::mem::take(&mut inbox_r);
        for m in to_r {
            for o in rr.on_inbound(ak, m).unwrap() {
                inbox_a.push(o.msg);
            }
        }
        let to_a = std::mem::take(&mut inbox_a);
        for m in to_a {
            for o in ra.on_inbound(rk, m).unwrap() {
                inbox_r.push(o.msg);
            }
        }
    }
    assert_eq!(
        rr.snapshot_live().get(b"riff".as_slice()).map(Vec::as_slice),
        Some(b"E5-A5-D5".as_slice()),
        "the relay replicated + folded A's op"
    );

    // (2) B joins LATE: R's announce must advertise writer A (a replicated writer).
    let adverts = rr.announce();
    assert!(
        adverts.iter().any(|o| matches!(o.msg, SyncMessage::Have { writer, length } if writer == ak && length == 1)),
        "the relay's announce advertises the replicated writer it can serve"
    );

    // (3) Deliver R's adverts to B; B Wants writer A FROM R; R re-serves A's block.
    let mut inbox_r2: Vec<SyncMessage> = Vec::new();
    for o in adverts {
        for reply in rb.on_inbound(rk, o.msg).unwrap() {
            inbox_r2.push(reply.msg);
        }
    }
    assert!(!inbox_r2.is_empty(), "B wants the advertised writer");
    let mut inbox_b: Vec<SyncMessage> = Vec::new();
    for m in inbox_r2 {
        for o in rr.on_inbound(bk, m).unwrap() {
            inbox_b.push(o.msg);
        }
    }
    for m in inbox_b {
        rb.on_inbound(rk, m).unwrap();
    }
    assert_eq!(
        rb.snapshot_live().get(b"riff".as_slice()).map(Vec::as_slice),
        Some(b"E5-A5-D5".as_slice()),
        "the late joiner converged on A's edit through the relay alone"
    );

    // Checkpoint regression: R (holding A's real op) may checkpoint once; but A,
    // whose only pending peer entries would be empty checkpoints, must NOT answer
    // with a checkpoint of its own (else the two Originals ping-pong forever).
    if rr.needs_checkpoint() {
        let outs = rr.checkpoint().unwrap();
        for o in outs {
            if let SyncMessage::Head { .. } | SyncMessage::Have { .. } = o.msg {
                for reply in ra.on_inbound(rk, o.msg.clone()).unwrap() {
                    // A pulls R's checkpoint block.
                    for served in rr.on_inbound(ak, reply.msg).unwrap() {
                        ra.on_inbound(rk, served.msg).unwrap();
                    }
                }
            }
        }
    }
    assert!(
        !ra.needs_checkpoint(),
        "a peer's empty checkpoint must not trigger a counter-checkpoint (ping-pong)"
    );
}
