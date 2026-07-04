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

/// Deliver every network-bound Outbound between two rooms until quiescent.
/// `Fanout::Clients` is a local push, not network traffic.
fn pump2<P: crate::projection::Projection + Clone>(
    a: &mut Room<MemStoreFactory, P>,
    a_id: [u8; 32],
    b: &mut Room<MemStoreFactory, P>,
    b_id: [u8; 32],
    outs: Vec<(bool, Vec<crate::room::Outbound>)>,
) {
    use crate::room::Fanout;
    let mut pending = outs;
    while let Some((from_a, outs)) = pending.pop() {
        for out in outs {
            match out.to {
                Fanout::Clients => continue,
                Fanout::Gossip | Fanout::Peer(_) => {
                    let (target, source) = if from_a { (&mut *b, a_id) } else { (&mut *a, b_id) };
                    let more = target.on_inbound(source, out.msg).unwrap();
                    pending.push((!from_a, more));
                }
            }
        }
    }
}

#[test]
fn appends_after_sync_order_after_what_they_saw() {
    // "I replied after seeing your write" must hold in the linearized order for
    // BOTH replicas, regardless of which writer key sorts first: local appends
    // link the DAG frontier (upstream getHeads()), not the roots.
    let (a, b) = (seed(1), seed(2));
    let (ak, bk) = (a.public().to_bytes(), b.public().to_bytes());
    let indexers = vec![ak, bk];
    let mut ra = Room::open(RoomConfig::original(a, indexers.clone()), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rb = Room::open(RoomConfig::original(b, indexers), MemStoreFactory, KvProjection::new()).unwrap();

    // Whoever has the LARGER key writes first — under root-linking, the other
    // side's later reply would have tiebroken *before* this write.
    let (first, first_id, second, second_id) =
        if ak > bk { (&mut ra, ak, &mut rb, bk) } else { (&mut rb, bk, &mut ra, ak) };

    let outs = first.local_append(&put(b"k", b"first")).unwrap();
    pump2(first, first_id, second, second_id, vec![(true, outs)]);

    // `second` has replicated the write; its overwrite is causally later.
    let outs = second.local_append(&put(b"k", b"second")).unwrap();
    pump2(first, first_id, second, second_id, vec![(false, outs)]);

    for room in [&*first, &*second] {
        assert_eq!(
            room.snapshot_live().get(b"k".as_slice()).map(Vec::as_slice),
            Some(b"second".as_slice()),
            "the causally-later overwrite wins on both replicas"
        );
    }
}

#[test]
fn non_indexer_writes_finalize_once_an_indexer_references_them() {
    // A non-indexing writer (e.g. a browser client) appends; the sole indexer
    // then appends something. The indexer's entry links the frontier, so it
    // causally sees — and thereby confirms — the client's entry.
    let (idx, client) = (seed(3), seed(4));
    let idx_key = idx.public().to_bytes();
    let indexers = vec![idx_key];
    let mut ri = Room::open(RoomConfig::original(idx, indexers.clone()), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rc = Room::open(RoomConfig::original(client, indexers), MemStoreFactory, KvProjection::new()).unwrap();
    let client_key = rc.local_key();

    let outs = rc.local_append(&put(b"from-client", b"1")).unwrap();
    pump2(&mut ri, idx_key, &mut rc, client_key, vec![(false, outs)]);

    // Not yet confirmed: no indexer entry references it.
    assert!(ri.snapshot_finalized().get(b"from-client".as_slice()).is_none());

    let outs = ri.local_append(&put(b"from-indexer", b"2")).unwrap();
    pump2(&mut ri, idx_key, &mut rc, client_key, vec![(true, outs)]);

    for room in [&ri, &rc] {
        assert_eq!(
            room.snapshot_finalized().get(b"from-client".as_slice()).map(Vec::as_slice),
            Some(b"1".as_slice()),
            "the client's entry is finalized on both replicas"
        );
    }
}

#[test]
fn late_joiner_backfills_whole_history_from_a_single_peer() {
    // A writes history; B replicates it; C then joins hearing only B. B's
    // announce advertises A's writer (Have) and serves A's blocks from its
    // stored verified triples — C gets the whole room without ever meeting A.
    let (a, b, c) = (seed(5), seed(6), seed(7));
    let ak = a.public().to_bytes();
    let indexers = vec![ak];
    let mut ra = Room::open(RoomConfig::original(a, indexers.clone()), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rb = Room::open(RoomConfig::original(b, indexers.clone()), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rc = Room::open(RoomConfig::original(c, indexers), MemStoreFactory, KvProjection::new()).unwrap();
    let (bk, ck) = (rb.local_key(), rc.local_key());

    let mut outs = ra.local_append(&put(b"x", b"1")).unwrap();
    outs.extend(ra.local_append(&put(b"y", b"2")).unwrap());
    pump2(&mut ra, ak, &mut rb, bk, vec![(true, outs)]);
    assert_eq!(
        rb.snapshot_live().get(b"x".as_slice()).map(Vec::as_slice),
        Some(b"1".as_slice()),
        "B replicated A's history"
    );

    // C talks only to B from here on.
    let announce = rb.announce();
    assert!(
        announce.iter().any(|o| matches!(o.msg, SyncMessage::Have { writer, .. } if writer == ak)),
        "B's announce advertises the replicated writer"
    );
    pump2(&mut rb, bk, &mut rc, ck, vec![(true, announce)]);

    assert_eq!(
        rc.snapshot_live().get(b"x".as_slice()).map(Vec::as_slice),
        Some(b"1".as_slice()),
        "late joiner received A's first entry via B"
    );
    assert_eq!(
        rc.snapshot_live().get(b"y".as_slice()).map(Vec::as_slice),
        Some(b"2".as_slice()),
        "late joiner received A's second entry via B"
    );
    assert_eq!(rc.logs(ak, 0, 10).unwrap().len(), 2, "C holds A's full log");
}
