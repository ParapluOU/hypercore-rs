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

/// A homeserver must acknowledge the newest peer entry, then stop producing
/// reference-only checkpoints once the entire batch has finalized.
#[test]
fn hosted_indexer_settles_after_a_peer_batch() {
    let host = seed(71);
    let peer = seed(72);
    let hk = host.public().to_bytes();
    let pk = peer.public().to_bytes();
    let mut hosted = Room::open(
        RoomConfig::original(host, vec![hk]),
        MemStoreFactory,
        KvProjection::new(),
    ).unwrap();
    let mut client = Room::open(
        RoomConfig::original(peer, vec![hk]),
        MemStoreFactory,
        KvProjection::new(),
    ).unwrap();
    for value in 0..5 {
        client.local_append(&put(b"selected-beat", &[value])).unwrap();
    }
    let mut to_host: Vec<_> = client.announce().into_iter().map(|o| o.msg).collect();
    let mut to_peer = Vec::new();
    for _ in 0..8 {
        for msg in std::mem::take(&mut to_host) {
            to_peer.extend(hosted.on_inbound(pk, msg).unwrap().into_iter().map(|o| o.msg));
        }
        for msg in std::mem::take(&mut to_peer) {
            to_host.extend(client.on_inbound(hk, msg).unwrap().into_iter().map(|o| o.msg));
        }
    }
    assert_eq!(hosted.snapshot_live().get(b"selected-beat".as_slice()), Some(&vec![4]));
    for _ in 0..12 {
        hosted.checkpoint().unwrap();
    }
    assert!(!hosted.needs_checkpoint(),
        "a finite peer edit batch must not create an unbounded checkpoint stream");
    assert_eq!(hosted.snapshot_finalized().get(b"selected-beat".as_slice()), Some(&vec![4]),
        "the host must confirm the latest peer entry, not only its root");
}

#[test]
fn rejoined_writer_recovers_its_authenticated_missing_suffix_before_appending() {
    let identity = seed(81);
    let key = identity.public().to_bytes();
    let mut original = Room::open(
        RoomConfig::original(identity.clone(), vec![key]),
        MemStoreFactory,
        KvProjection::new(),
    )
    .unwrap();
    let mut rejoined = Room::open(
        RoomConfig::original(identity, vec![key]),
        MemStoreFactory,
        KvProjection::new(),
    )
    .unwrap();
    // Identical root represents the last persisted writer prefix, not a new fork.
    for room in [&mut original, &mut rejoined] {
        room.local_append(&put(b"beat", b"quarter")).unwrap();
    }
    let stale = rejoined.snapshot_feeds().unwrap()[0].head.clone();
    original.local_append(&put(b"beat", b"eighth")).unwrap();
    original
        .local_append(&put(b"second", b"sixteenth"))
        .unwrap();
    let head = original.snapshot_feeds().unwrap()[0].head.clone();
    let wants = rejoined
        .on_inbound(
            [82; 32],
            SyncMessage::Head {
                writer: key,
                head: head.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        wants.len(),
        1,
        "same-identity rejoin must pull its missing signed suffix"
    );
    assert!(
        rejoined.local_append(&put(b"fork", b"forbidden")).is_err(),
        "cannot append while advertised own history is missing"
    );
    let mut blocks = original.on_inbound(key, wants[0].msg.clone()).unwrap();
    blocks.reverse(); // verified suffix delivery need not be in order
    rejoined
        .on_inbound(
            [82; 32],
            SyncMessage::Head {
                writer: key,
                head: stale.clone(),
            },
        )
        .unwrap();
    let mut tampered = blocks[0].msg.clone();
    if let SyncMessage::Block { bytes, .. } = &mut tampered {
        bytes[0] ^= 1;
    }
    assert!(rejoined.on_inbound([82; 32], tampered).is_err());
    assert_eq!(rejoined.writer_lengths()[&key], 1);
    let duplicate = blocks[0].msg.clone();
    rejoined.on_inbound([82; 32], duplicate.clone()).unwrap();
    rejoined.on_inbound([82; 32], duplicate).unwrap();
    assert!(
        rejoined.local_append(&[]).is_err(),
        "partial recovery remains gated"
    );
    for block in blocks {
        rejoined.on_inbound([82; 32], block.msg).unwrap();
    }
    assert_eq!(rejoined.snapshot_live(), original.snapshot_live());
    assert_eq!(
        rejoined.snapshot_feeds().unwrap()[0].blocks,
        original.snapshot_feeds().unwrap()[0].blocks,
        "signed bytes preserved"
    );
    assert_eq!(rejoined.snapshot_feeds().unwrap()[0].head, head);
    rejoined
        .on_inbound(
            [82; 32],
            SyncMessage::Head {
                writer: key,
                head: stale,
            },
        )
        .unwrap();
    rejoined.local_append(&put(b"continued", b"yes")).unwrap();
    assert_eq!(rejoined.writer_lengths()[&key], 4);
}

#[test]
fn own_feed_conflicting_prefix_never_promotes_or_reenables_writes() {
    let identity = seed(85);
    let key = identity.public().to_bytes();
    let mut trusted = Room::open(
        RoomConfig::original(identity.clone(), vec![key]),
        MemStoreFactory,
        KvProjection::new(),
    )
    .unwrap();
    let mut conflict = Room::open(
        RoomConfig::original(identity, vec![key]),
        MemStoreFactory,
        KvProjection::new(),
    )
    .unwrap();
    trusted.local_append(&put(b"beat", b"trusted")).unwrap();
    conflict.local_append(&put(b"beat", b"fork")).unwrap();
    let same_length = conflict.snapshot_feeds().unwrap()[0].head.clone();
    assert!(trusted
        .on_inbound(
            key,
            SyncMessage::Head {
                writer: key,
                head: same_length
            }
        )
        .is_err());
    let before = trusted.snapshot_feeds().unwrap();
    conflict.local_append(&put(b"beat", b"later")).unwrap();
    let head = conflict.snapshot_feeds().unwrap()[0].head.clone();
    let want = trusted
        .on_inbound(key, SyncMessage::Head { writer: key, head })
        .unwrap();
    for block in conflict.on_inbound(key, want[0].msg.clone()).unwrap() {
        assert!(trusted.on_inbound(key, block.msg).is_err());
    }
    assert_eq!(trusted.snapshot_feeds().unwrap()[0].head, before[0].head);
    assert_eq!(
        trusted.snapshot_feeds().unwrap()[0].blocks,
        before[0].blocks
    );
    assert_eq!(
        trusted.snapshot_live().get(b"beat".as_slice()),
        Some(&b"trusted".to_vec())
    );
    assert!(trusted.local_append(&[]).is_err());
    assert!(!trusted.needs_checkpoint());
}
