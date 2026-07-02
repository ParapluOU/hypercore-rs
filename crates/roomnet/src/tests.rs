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
