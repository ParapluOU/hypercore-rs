//! Integration tests: many `Room`s exchanging `SyncMessage`s over an in-process
//! pump, plus the `RoomServer` on-demand replication path and the stale-GC knob.

use std::collections::VecDeque;
use std::time::Duration;

use identity::SecretKey;
use roomnet::testkit::{del, put, KvProjection};
use roomnet::{
    Fanout, MemStoreFactory, Origin, Outbound, PeerId, Room, RoomConfig, RoomServer, ServerConfig,
    SyncMessage,
};

type R = Room<MemStoreFactory, KvProjection>;

/// A queued delivery: where it goes, who it is from, and the message.
enum Tgt {
    All,
    One(PeerId),
}

fn push(q: &mut VecDeque<(Tgt, PeerId, SyncMessage)>, from: PeerId, o: Outbound) {
    match o.to {
        Fanout::Gossip => q.push_back((Tgt::All, from, o.msg)),
        Fanout::Peer(p) => q.push_back((Tgt::One(p), from, o.msg)),
        Fanout::Clients => {} // Lane 1 — no client sinks in this harness
    }
}

/// Announce every room's head, then deliver messages to a fixpoint. A pure,
/// deterministic stand-in for the transport: it proves the sans-IO `Room` logic
/// converges regardless of the (arbitrary but FIFO) delivery order.
fn converge(rooms: &mut [(PeerId, &mut R)]) {
    let mut q: VecDeque<(Tgt, PeerId, SyncMessage)> = VecDeque::new();
    for (id, r) in rooms.iter() {
        for o in r.announce() {
            push(&mut q, *id, o);
        }
    }
    while let Some((tgt, from, msg)) = q.pop_front() {
        for i in 0..rooms.len() {
            let deliver = match tgt {
                Tgt::All => rooms[i].0 != from,
                Tgt::One(p) => rooms[i].0 == p,
            };
            if deliver {
                let id = rooms[i].0;
                let outs = rooms[i].1.on_inbound(from, msg.clone()).unwrap();
                for o in outs {
                    push(&mut q, id, o);
                }
            }
        }
    }
}

fn sk(n: u8) -> SecretKey {
    SecretKey::from_seed(&[n; 32])
}

#[test]
fn two_writers_converge_on_the_live_snapshot() {
    let a = sk(1);
    let b = sk(2);
    let ak = a.public().to_bytes();
    let bk = b.public().to_bytes();
    let idx = vec![ak, bk];

    let mut ra = Room::open(RoomConfig::original(a, idx.clone()), MemStoreFactory, KvProjection::new());
    let mut rb = Room::open(RoomConfig::original(b, idx.clone()), MemStoreFactory, KvProjection::new());

    ra.local_append(&put(b"x", b"1")).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    rb.local_append(&put(b"y", b"2")).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    ra.local_append(&put(b"x", b"3")).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    rb.local_append(&del(b"y")).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);

    // Same DAG ⇒ same deterministic order ⇒ same folded state on both replicas.
    assert_eq!(ra.order(), rb.order(), "both replicas linearize identically");
    assert_eq!(ra.snapshot_live(), rb.snapshot_live(), "live snapshots converge");

    let s = ra.snapshot_live();
    assert_eq!(s.get(b"x".as_slice()).map(Vec::as_slice), Some(b"3".as_slice()), "x overwritten");
    assert!(s.get(b"y".as_slice()).is_none(), "y deleted");
}

#[test]
fn three_writers_converge_exercising_cross_writer_buffering() {
    let (a, b, c) = (sk(1), sk(2), sk(3));
    let (ak, bk, ck) = (a.public().to_bytes(), b.public().to_bytes(), c.public().to_bytes());
    let idx = vec![ak, bk, ck];

    let mut ra = Room::open(RoomConfig::original(a, idx.clone()), MemStoreFactory, KvProjection::new());
    let mut rb = Room::open(RoomConfig::original(b, idx.clone()), MemStoreFactory, KvProjection::new());
    let mut rc = Room::open(RoomConfig::original(c, idx.clone()), MemStoreFactory, KvProjection::new());

    for round in 0..4u8 {
        ra.local_append(&put(b"a", &[round])).unwrap();
        rb.local_append(&put(b"b", &[round])).unwrap();
        rc.local_append(&put(b"c", &[round])).unwrap();
        converge(&mut [(ak, &mut ra), (bk, &mut rb), (ck, &mut rc)]);
    }

    assert_eq!(ra.order(), rb.order());
    assert_eq!(rb.order(), rc.order());
    assert_eq!(ra.snapshot_live(), rc.snapshot_live(), "all three converge");
    assert_eq!(ra.snapshot_live().len(), 3, "keys a, b, c all present");
}

#[test]
fn indexers_reach_the_same_finalized_view() {
    let a = sk(1);
    let b = sk(2);
    let ak = a.public().to_bytes();
    let bk = b.public().to_bytes();
    let idx = vec![ak, bk];

    let mut ra = Room::open(RoomConfig::original(a, idx.clone()), MemStoreFactory, KvProjection::new());
    let mut rb = Room::open(RoomConfig::original(b, idx.clone()), MemStoreFactory, KvProjection::new());

    // Interleave several rounds so mutual references accumulate and finality advances.
    for round in 0..6u8 {
        ra.local_append(&put(b"a", &[round])).unwrap();
        converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
        rb.local_append(&put(b"b", &[round])).unwrap();
        converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    }

    assert!(ra.finalized_len() > 0, "finality advanced");
    assert_eq!(ra.finalized_len(), rb.finalized_len(), "same finalized depth");
    assert_eq!(
        ra.snapshot_finalized(),
        rb.snapshot_finalized(),
        "finalized (authoritative) snapshots converge"
    );
}

#[test]
fn replica_enforces_in_order_delivery_and_recovers() {
    let a = sk(1);
    let b = sk(2);
    let ak = a.public().to_bytes();
    let bk = b.public().to_bytes();
    let idx = vec![ak, bk];

    let mut ra = Room::open(RoomConfig::original(a, idx.clone()), MemStoreFactory, KvProjection::new());
    let mut rb = Room::open(RoomConfig::original(b, idx), MemStoreFactory, KvProjection::new());

    ra.local_append(&put(b"k", b"v0")).unwrap();
    ra.local_append(&put(b"k", b"v1")).unwrap();

    // Ask A to serve both of its blocks (as it would answer a Want).
    let blocks = ra.on_inbound(bk, SyncMessage::Want { writer: ak, start: 0, end: 2 }).unwrap();
    assert_eq!(blocks.len(), 2, "A serves both blocks");
    let m0 = blocks[0].msg.clone();
    let m1 = blocks[1].msg.clone();

    // Deliver index 1 first: the replica must reject it (out of order), applying nothing.
    rb.on_inbound(ak, m1.clone()).unwrap();
    assert!(rb.order().is_empty(), "out-of-order block is not applied");

    // Deliver in order: 0 then the re-sent 1.
    rb.on_inbound(ak, m0).unwrap();
    rb.on_inbound(ak, m1).unwrap();
    assert_eq!(
        rb.snapshot_live().get(b"k".as_slice()).map(Vec::as_slice),
        Some(b"v1".as_slice()),
        "in-order delivery converges to the latest value"
    );
}

#[test]
fn room_server_join_remote_replicates_a_hosted_room() {
    let dk = sk(10).public().to_bytes();
    let room_id = [42u8; 32];
    // Both servers agree the room's sole indexer is D (the host).
    let mut d = RoomServer::open(
        ServerConfig { identity_seed: [10; 32], indexers: vec![dk], replica_stale_after: None },
        KvProjection::new(),
    );
    let mut c = RoomServer::open(
        ServerConfig { identity_seed: [11; 32], indexers: vec![dk], replica_stale_after: None },
        KvProjection::new(),
    );

    // D hosts and edits the room.
    {
        let r = d.host(room_id);
        r.local_append(&put(b"song", b"abc")).unwrap();
        r.local_append(&put(b"tempo", b"120")).unwrap();
    }

    // A client of C reaches for the room ⇒ C replicates it on demand.
    c.join_remote(room_id);
    {
        let dr = d.get_mut(room_id).unwrap();
        let cr = c.get_mut(room_id).unwrap();
        let dk2 = dr.local_key();
        let ck2 = cr.local_key();
        converge(&mut [(dk2, dr), (ck2, cr)]);
    }

    let cs = c.get(room_id).unwrap();
    let ds = d.get(room_id).unwrap();
    assert_eq!(cs.snapshot_live(), ds.snapshot_live(), "replica matches origin");
    assert_eq!(cs.snapshot_live().get(b"song".as_slice()).map(Vec::as_slice), Some(b"abc".as_slice()));
    assert!(matches!(cs.origin(), Origin::Replicated { .. }), "C's copy is a replica");
    assert!(matches!(ds.origin(), Origin::Original), "D's copy is original");
}

#[test]
fn replicated_rooms_are_evictable_originals_are_not() {
    let mut s: RoomServer<MemStoreFactory, KvProjection> = RoomServer::open(
        ServerConfig {
            identity_seed: [5; 32],
            indexers: vec![],
            replica_stale_after: Some(Duration::from_secs(1)),
        },
        KvProjection::new(),
    );
    s.host([1u8; 32]);
    s.join_remote([2u8; 32]);

    // Judge every room stale: only the replicated one is eligible for eviction.
    let dropped = s.evict_stale(|_| true);
    assert_eq!(dropped, vec![[2u8; 32]], "only the replica is evicted");
    assert!(s.get([1u8; 32]).is_some(), "the original is kept");
    assert!(s.get([2u8; 32]).is_none(), "the replica is gone");
}
