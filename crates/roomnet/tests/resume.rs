//! Durable resume: a room recovers its full derived state from **disk alone**,
//! with no peers online. Uses the real `DiskStoreFactory` (a file per writer under
//! a temp directory) so this exercises the actual on-disk path, not an in-mem shim.

#![cfg(unix)]

use std::collections::VecDeque;
use std::path::PathBuf;

use identity::SecretKey;
use roomnet::testkit::{put, KvProjection};
use roomnet::{
    CachedFactory, DiskStoreFactory, Fanout, MemStoreFactory, Outbound, PeerId, Projection, Room,
    RoomConfig, StoreFactory, SyncMessage,
};

enum Tgt {
    All,
    One(PeerId),
}

/// Announce every room's head, then deliver to a fixpoint (the in-process pump,
/// generic over the store factory so it works over disk-backed rooms too).
fn converge<F, P>(rooms: &mut [(PeerId, &mut Room<F, P>)])
where
    F: StoreFactory,
    P: Projection + Clone,
{
    fn push(q: &mut VecDeque<(Tgt, PeerId, SyncMessage)>, from: PeerId, o: Outbound) {
        match o.to {
            Fanout::Gossip => q.push_back((Tgt::All, from, o.msg)),
            Fanout::Peer(p) => q.push_back((Tgt::One(p), from, o.msg)),
            Fanout::Clients => {}
        }
    }
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

/// A fresh, empty temp directory unique to this process + tag.
fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("roomnet-resume-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn two_writers_resume_from_disk_without_peers() {
    let a = SecretKey::from_seed(&[1; 32]);
    let b = SecretKey::from_seed(&[2; 32]);
    let ak = a.public().to_bytes();
    let bk = b.public().to_bytes();
    let idx = vec![ak, bk];
    let dir_a = tmpdir("2a");
    let dir_b = tmpdir("2b");

    // Build A + B over disk and converge to non-zero finality.
    let (order, finalized, live, finalized_len) = {
        let mut ra = Room::open(
            RoomConfig::original(a, idx.clone()),
            DiskStoreFactory::new(&dir_a).unwrap(),
            KvProjection::new(),
        )
        .unwrap();
        let mut rb = Room::open(
            RoomConfig::original(b, idx.clone()),
            DiskStoreFactory::new(&dir_b).unwrap(),
            KvProjection::new(),
        )
        .unwrap();

        for round in 0..6u8 {
            ra.local_append(&put(b"a", &[round])).unwrap();
            converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
            rb.local_append(&put(b"b", &[round])).unwrap();
            converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
        }

        assert!(ra.finalized_len() > 0, "finality advanced before restart");
        (
            ra.order(),
            ra.snapshot_finalized().clone(),
            ra.snapshot_live().clone(),
            ra.finalized_len(),
        )
        // ra + rb dropped here: everything in memory is gone; only the disk files remain.
    };

    // Reopen A from its directory alone — no B, no pump, no network.
    let ra2 = Room::open(
        RoomConfig::original(SecretKey::from_seed(&[1; 32]), idx.clone()),
        DiskStoreFactory::new(&dir_a).unwrap(),
        KvProjection::new(),
    )
    .unwrap();

    assert_eq!(ra2.order(), order, "DAG order recovered from disk");
    assert_eq!(ra2.snapshot_finalized(), &finalized, "finalized state recovered from disk");
    assert_eq!(ra2.snapshot_live(), &live, "live state recovered from disk");
    assert_eq!(ra2.finalized_len(), finalized_len, "finalized depth recovered");
    let mut ra2 = ra2;
    assert!(ra2.poll_finalized().is_empty(), "resume must not re-emit finalized deltas");

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn single_writer_resumes_from_disk() {
    let a = SecretKey::from_seed(&[3; 32]);
    let ak = a.public().to_bytes();
    let dir = tmpdir("solo");

    let (finalized, finalized_len) = {
        let mut r = Room::open(
            RoomConfig::original(a, vec![ak]),
            DiskStoreFactory::new(&dir).unwrap(),
            KvProjection::new(),
        )
        .unwrap();
        for i in 0..8u8 {
            r.local_append(&put(b"k", &[i])).unwrap();
        }
        assert!(r.finalized_len() > 0, "a self-indexer finalizes a prefix");
        (r.snapshot_finalized().clone(), r.finalized_len())
    };

    let mut r2 = Room::open(
        RoomConfig::original(SecretKey::from_seed(&[3; 32]), vec![ak]),
        DiskStoreFactory::new(&dir).unwrap(),
        KvProjection::new(),
    )
    .unwrap();
    assert_eq!(r2.snapshot_finalized(), &finalized, "local writer state recovered from disk");
    assert_eq!(r2.finalized_len(), finalized_len);
    assert!(r2.poll_finalized().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn room_recovers_from_the_disk_cache_when_the_implementor_store_is_lost() {
    // The implementor's factory is ephemeral (MemStoreFactory) — it stands in for
    // a remote/DB store that a fresh container does NOT have locally. roomnet's
    // CachedFactory keeps a local disk copy regardless, so the room still resumes.
    let a = SecretKey::from_seed(&[9; 32]);
    let ak = a.public().to_bytes();
    let cache_dir = tmpdir("cache");

    let (finalized, finalized_len) = {
        let f = CachedFactory::new(MemStoreFactory, &cache_dir).unwrap();
        let mut room =
            Room::open(RoomConfig::original(a, vec![ak]), f, KvProjection::new()).unwrap();
        for i in 0..8u8 {
            room.local_append(&put(b"k", &[i])).unwrap();
        }
        assert!(room.finalized_len() > 0);
        (room.snapshot_finalized().clone(), room.finalized_len())
    };

    // "Container restart": a brand-new (empty) implementor store, but the SAME
    // local disk cache directory. Recovery must come from the cache alone.
    let f2 = CachedFactory::new(MemStoreFactory, &cache_dir).unwrap();
    let r2 = Room::open(
        RoomConfig::original(SecretKey::from_seed(&[9; 32]), vec![ak]),
        f2,
        KvProjection::new(),
    )
    .unwrap();
    assert_eq!(
        r2.snapshot_finalized(),
        &finalized,
        "recovered from the local disk cache even though the implementor store was empty"
    );
    assert_eq!(r2.finalized_len(), finalized_len);

    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// An op the projection refuses (here: malformed bytes) takes no effect, and neither
/// stops the room finalizing past it nor stops it reopening: it stays in the confirmed
/// log, which nobody can change, and every replay skips it alike.
#[test]
fn a_refused_op_is_skipped_and_the_room_still_reopens() {
    let a = SecretKey::from_seed(&[4; 32]);
    let ak = a.public().to_bytes();
    let dir = tmpdir("refused");

    let (finalized, finalized_len) = {
        let mut r = Room::open(RoomConfig::original(a, vec![ak]), DiskStoreFactory::new(&dir).unwrap(), KvProjection::new()).unwrap();
        r.local_append(&put(b"k", b"1")).unwrap();
        r.local_append(b"\xffnot an op").unwrap();
        r.local_append(&put(b"k", b"2")).unwrap();
        assert_eq!(r.finalized_len(), 3, "finalized past the refused op");
        assert_eq!(r.snapshot_finalized().get(b"k".as_slice()), Some(&b"2".to_vec()));
        (r.snapshot_finalized().clone(), r.finalized_len())
    };

    let r2 = Room::open(RoomConfig::original(SecretKey::from_seed(&[4; 32]), vec![ak]), DiskStoreFactory::new(&dir).unwrap(), KvProjection::new())
        .expect("a refused op does not stop the room reopening");
    assert_eq!((r2.snapshot_finalized(), r2.finalized_len()), (&finalized, finalized_len));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A room that reopens without some remote entries its own entries build on (their
/// replica was lost, or reopened shorter) holds those own entries back until the
/// remote ones arrive again. Until then it refuses to append — a new entry would follow
/// its own undelivered one — and once they are back it appends again.
#[test]
fn own_entries_waiting_on_lost_remote_ones_hold_appends_back_until_they_return() {
    let a = SecretKey::from_seed(&[5; 32]);
    let b = SecretKey::from_seed(&[6; 32]);
    let (ak, bk) = (a.public().to_bytes(), b.public().to_bytes());
    let idx = vec![ak, bk];
    let (dir_a, dir_b) = (tmpdir("held-a"), tmpdir("held-b"));

    {
        let mut ra = Room::open(RoomConfig::original(a, idx.clone()), DiskStoreFactory::new(&dir_a).unwrap(), KvProjection::new()).unwrap();
        let mut rb = Room::open(RoomConfig::original(b, idx.clone()), DiskStoreFactory::new(&dir_b).unwrap(), KvProjection::new()).unwrap();
        rb.local_append(&put(b"b", b"1")).unwrap();
        converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
        ra.local_append(&put(b"a", b"1")).unwrap(); // builds on B's entry
    }
    // A loses its copy of B's log
    let b_file: String = bk.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::remove_file(dir_a.join(b_file)).unwrap();

    let mut ra = Room::open(RoomConfig::original(SecretKey::from_seed(&[5; 32]), idx.clone()), DiskStoreFactory::new(&dir_a).unwrap(), KvProjection::new()).unwrap();
    assert!(matches!(ra.local_append(&put(b"a", b"2")), Err(roomnet::Error::OwnFeedPending)), "an append would follow an undelivered own entry");

    let mut rb = Room::open(RoomConfig::original(SecretKey::from_seed(&[6; 32]), idx.clone()), DiskStoreFactory::new(&dir_b).unwrap(), KvProjection::new()).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    ra.local_append(&put(b"a", b"2")).expect("appends again once B's entries are back");
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    assert_eq!(ra.snapshot_live().get(b"a".as_slice()), Some(&b"2".to_vec()));

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// With confirmation stalled (an indexer that never answers), an indexer checkpoints a
/// peer's pending op once, not on every poll: a second checkpoint would reference nothing
/// the first did not.
#[test]
fn a_stalled_room_is_checkpointed_once_per_new_entry() {
    let a = SecretKey::from_seed(&[7; 32]);
    let b = SecretKey::from_seed(&[8; 32]);
    let absent = SecretKey::from_seed(&[9; 32]).public().to_bytes();
    let (ak, bk) = (a.public().to_bytes(), b.public().to_bytes());
    let idx = vec![ak, absent, bk];
    let mut ra = Room::open(RoomConfig::original(a, idx.clone()), MemStoreFactory, KvProjection::new()).unwrap();
    let mut rb = Room::open(RoomConfig::original(b, vec![ak, absent]), MemStoreFactory, KvProjection::new()).unwrap();
    let _ = &idx;
    rb.local_append(&put(b"b", b"1")).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    assert!(ra.needs_checkpoint(), "a peer's op waits");
    let before = ra.writer_lengths()[&ak];
    for _ in 0..20 {
        ra.checkpoint().unwrap();
    }
    assert_eq!(ra.writer_lengths()[&ak], before + 1, "one checkpoint, however often asked");
    assert!(!ra.needs_checkpoint());

    // a new entry from the peer earns one more
    rb.local_append(&put(b"b", b"2")).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);
    for _ in 0..20 {
        ra.checkpoint().unwrap();
    }
    assert!(ra.writer_lengths()[&ak] <= before + 3, "a few checkpoints per new entry, not one per poll");
}
