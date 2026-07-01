//! `local_board` — the sans-IO core with an invented domain, no network.
//!
//! Two peers edit a shared **sticky-note board**; we pump their `SyncMessage`s in
//! process and watch the boards converge. Shows `Room`, a custom `Projection`, and
//! the message protocol without any transport. Run with:
//!
//! ```sh
//! cargo run -p roomnet --example local_board
//! ```

use std::collections::{BTreeMap, VecDeque};

use codec::varint;
use identity::SecretKey;
use roomnet::{
    Fanout, MemStoreFactory, NodeId, Outbound, PeerId, Projection, Room, RoomConfig, SyncMessage,
};

// ---- invented domain: a sticky-note board --------------------------------

enum BoardOp {
    Add { id: u32, text: String },
    Remove { id: u32 },
}

const ADD: u64 = 0;
const REMOVE: u64 = 1;

fn encode(op: &BoardOp) -> Vec<u8> {
    let mut out = Vec::new();
    match op {
        BoardOp::Add { id, text } => {
            varint::write(&mut out, ADD);
            varint::write(&mut out, *id as u64);
            varint::write(&mut out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        BoardOp::Remove { id } => {
            varint::write(&mut out, REMOVE);
            varint::write(&mut out, *id as u64);
        }
    }
    out
}

/// The board projection: note id -> text, folded in autobase order.
#[derive(Clone, Default)]
struct Board {
    notes: BTreeMap<u32, String>,
}

impl Projection for Board {
    type State = BTreeMap<u32, String>;
    type Error = &'static str;

    fn apply(&mut self, _node: NodeId, payload: &[u8]) -> Result<(), &'static str> {
        let mut b = payload;
        match varint::read(&mut b).map_err(|_| "bad tag")? {
            ADD => {
                let id = varint::read(&mut b).map_err(|_| "bad id")? as u32;
                let len = varint::read(&mut b).map_err(|_| "bad len")? as usize;
                if b.len() < len {
                    return Err("short text");
                }
                self.notes.insert(id, String::from_utf8_lossy(&b[..len]).into_owned());
            }
            REMOVE => {
                let id = varint::read(&mut b).map_err(|_| "bad id")? as u32;
                self.notes.remove(&id);
            }
            _ => return Err("unknown op"),
        }
        Ok(())
    }

    fn snapshot(&self) -> &Self::State {
        &self.notes
    }

    fn reset_to(&mut self, checkpoint: &Self::State) {
        self.notes = checkpoint.clone();
    }
}

// ---- a tiny in-process pump ----------------------------------------------

type R = Room<MemStoreFactory, Board>;

enum Tgt {
    All,
    One(PeerId),
}

fn converge(rooms: &mut [(PeerId, &mut R)]) {
    let mut q: VecDeque<(Tgt, PeerId, SyncMessage)> = VecDeque::new();
    let push = |q: &mut VecDeque<_>, from: PeerId, o: Outbound| match o.to {
        Fanout::Gossip => q.push_back((Tgt::All, from, o.msg)),
        Fanout::Peer(p) => q.push_back((Tgt::One(p), from, o.msg)),
        Fanout::Clients => {}
    };
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

fn main() {
    let alice = SecretKey::from_seed(&[1; 32]);
    let bob = SecretKey::from_seed(&[2; 32]);
    let ak = alice.public().to_bytes();
    let bk = bob.public().to_bytes();
    let indexers = vec![ak, bk];

    let mut ra = Room::open(RoomConfig::original(alice, indexers.clone()), MemStoreFactory, Board::default());
    let mut rb = Room::open(RoomConfig::original(bob, indexers), MemStoreFactory, Board::default());

    ra.local_append(&encode(&BoardOp::Add { id: 1, text: "buy strings".into() })).unwrap();
    rb.local_append(&encode(&BoardOp::Add { id: 2, text: "tune to drop D".into() })).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);

    ra.local_append(&encode(&BoardOp::Remove { id: 2 })).unwrap();
    rb.local_append(&encode(&BoardOp::Add { id: 3, text: "record demo".into() })).unwrap();
    converge(&mut [(ak, &mut ra), (bk, &mut rb)]);

    println!("alice's board: {:?}", ra.snapshot_live());
    println!("bob's board:   {:?}", rb.snapshot_live());
    assert_eq!(ra.snapshot_live(), rb.snapshot_live(), "the boards converge");
    println!("converged on {} notes", ra.snapshot_live().len());
}
