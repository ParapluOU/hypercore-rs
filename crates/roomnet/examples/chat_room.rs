//! `chat_room` — a collaborative chat over the real Iroh transport (feature `iroh`).
//!
//! Shows the whole node wiring with an **invented domain**: a chat room whose ops
//! are `SetName`/`Post`, folded by a `ChatProjection` into a message log. Run with:
//!
//! ```sh
//! cargo run -p roomnet --example chat_room --features iroh
//! ```
//!
//! It binds one node, hosts a room, and appends a few messages; the driver
//! finalizes them and the sink prints each finalized version. A second node would
//! join with `Command::JoinRemote(room, origin_id)` after seeding `bootstrap` with
//! this node's `endpoint_id()` — the two then converge over QUIC.

use std::collections::BTreeMap;
use std::time::Duration;

use codec::varint;
use identity::SecretKey;
use roomnet::{
    Command, DriverOpts, IrohConfig, IrohTransport, MemStoreFactory, NodeId, Projection,
    ProjectionSink, RoomServer, ServerConfig,
};

// ---- invented domain -----------------------------------------------------

/// A chat mutation. Encoded as the opaque `Entry.payload` roomnet carries.
enum ChatOp {
    SetName { name: String },
    Post { text: String },
}

const OP_NAME: u64 = 0;
const OP_POST: u64 = 1;

fn encode(op: &ChatOp) -> Vec<u8> {
    let mut out = Vec::new();
    match op {
        ChatOp::SetName { name } => {
            varint::write(&mut out, OP_NAME);
            put_str(&mut out, name);
        }
        ChatOp::Post { text } => {
            varint::write(&mut out, OP_POST);
            put_str(&mut out, text);
        }
    }
    out
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    varint::write(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn get_str(b: &mut &[u8]) -> Option<String> {
    let len = varint::read(b).ok()? as usize;
    if b.len() < len {
        return None;
    }
    let (s, rest) = b.split_at(len);
    *b = rest;
    String::from_utf8(s.to_vec()).ok()
}

/// The materialized chat: display names per author + the message log.
#[derive(Clone, Default)]
struct Chat {
    names: BTreeMap<[u8; 32], String>,
    log: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct ChatProjection {
    chat: Chat,
}

impl Projection for ChatProjection {
    type State = Chat;
    type Error = &'static str;

    fn apply(&mut self, node: NodeId, payload: &[u8]) -> Result<(), &'static str> {
        let mut b = payload;
        let tag = varint::read(&mut b).map_err(|_| "bad op")?;
        match tag {
            OP_NAME => {
                let name = get_str(&mut b).ok_or("bad name")?;
                self.chat.names.insert(node.key, name);
            }
            OP_POST => {
                let text = get_str(&mut b).ok_or("bad post")?;
                let who = self
                    .chat
                    .names
                    .get(&node.key)
                    .cloned()
                    .unwrap_or_else(|| short_hex(&node.key));
                self.chat.log.push((who, text));
            }
            _ => return Err("unknown op"),
        }
        Ok(())
    }

    fn snapshot(&self) -> &Chat {
        &self.chat
    }

    fn reset_to(&mut self, checkpoint: &Chat) {
        self.chat = checkpoint.clone();
    }
}

fn short_hex(key: &[u8; 32]) -> String {
    key[..3].iter().map(|b| format!("{b:02x}")).collect()
}

/// A projection sink that prints each finalized version (Lane 3 stand-in for TDB).
struct PrintSink;

impl ProjectionSink<Chat> for PrintSink {
    type Error = std::convert::Infallible;

    fn write(&mut self, version: u64, state: &Chat) -> Result<(), Self::Error> {
        if let Some((who, text)) = state.log.last() {
            println!("[finalized v{version}] {who}: {text}");
        }
        Ok(())
    }
}

// ---- wiring --------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let seed = [7u8; 32];
    let writer_key = SecretKey::from_seed(&seed).public().to_bytes();

    // Configurable Iroh params (defaults elsewhere): custom ALPN + identity seed.
    let cfg = IrohConfig { alpn: b"parture/roomnet/chat/1".to_vec(), seed, ..IrohConfig::default() };
    let transport = IrohTransport::bind(&cfg).await?;
    println!("chat node bound, endpoint/writer id = {}", short_hex(&transport.endpoint_id()));

    // This single node is its own indexer, so its posts finalize on their own.
    let server: RoomServer<MemStoreFactory, ChatProjection> = RoomServer::open(
        ServerConfig { identity_seed: seed, indexers: vec![writer_key], replica_stale_after: None },
        ChatProjection::default(),
    );

    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let driver = tokio::spawn(roomnet::run_server(
        server,
        transport,
        PrintSink,
        rx,
        DriverOpts::default(),
    ));

    let room = [1u8; 32];
    tx.send(Command::Host(room)).await?;
    tx.send(Command::Append(room, encode(&ChatOp::SetName { name: "alice".into() }))).await?;
    tx.send(Command::Append(room, encode(&ChatOp::Post { text: "hello from roomnet".into() }))).await?;
    tx.send(Command::Append(room, encode(&ChatOp::Post { text: "over iroh QUIC".into() }))).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    tx.send(Command::Shutdown).await?;
    driver.await?;
    Ok(())
}
