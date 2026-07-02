//! The native Iroh transport + tokio driver (feature `iroh`).
//!
//! [`IrohTransport`] binds an iroh QUIC endpoint and moves `[RoomId | wire]`
//! frames between peers: outbound [`Outbound`]s are dialed to peers (gossip →
//! every known peer, `Peer` → one), inbound frames are decoded (via [`wire`]) and
//! queued. [`run_server`] is the tokio event loop that ties an [`IrohTransport`]
//! to a [`RoomServer`]: it feeds inbound messages into rooms, dials out the
//! resulting messages, drains finalized deltas to a [`ProjectionSink`], and
//! accepts local [`Command`]s (host / join / append).
//!
//! Net parameters are configurable via [`IrohConfig`] with sensible defaults.
//!
//! A node identity's public bytes are simultaneously its autobase [`WriterKey`]
//! and its iroh [`EndpointId`](iroh::EndpointId) (both raw ed25519), so peers are
//! dialed directly by their writer key.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::future::Future;

use iroh::endpoint::{presets::N0, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, PublicKey, SecretKey};
use tokio::sync::mpsc;

use crate::projection::Projection;
use crate::room::{Fanout, Outbound, PeerId};
use crate::server::{RoomId, RoomServer};
use crate::store::StoreFactory;
use crate::sync::SyncMessage;
use crate::transport::ProjectionSink;
use crate::wire;

/// Any transport error (bind/dial/stream), boxed.
pub type IrohResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Configurable Iroh network parameters (all fields have defaults via [`Default`]).
#[derive(Clone, Debug)]
pub struct IrohConfig {
    /// ALPN for the roomnet sync protocol. Default `b"roomnet/sync/0"`.
    pub alpn: Vec<u8>,
    /// 32-byte identity seed — also this node's autobase writer identity.
    pub seed: [u8; 32],
    /// Fixed UDP bind port; `None` binds an ephemeral port.
    pub bind_port: Option<u16>,
    /// Peers (their 32-byte endpoint ids) to seed the known-peer set with.
    pub bootstrap: Vec<[u8; 32]>,
    /// Max inbound frame size accepted per stream.
    pub max_message_bytes: usize,
}

impl Default for IrohConfig {
    fn default() -> Self {
        Self {
            alpn: b"roomnet/sync/0".to_vec(),
            seed: [0u8; 32],
            bind_port: None,
            bootstrap: Vec::new(),
            max_message_bytes: 16 * 1024 * 1024,
        }
    }
}

/// One decoded inbound frame: which room, which peer, which message.
#[derive(Clone, Debug)]
pub struct Inbound {
    pub room: RoomId,
    pub from: PeerId,
    pub msg: SyncMessage,
}

/// A local command to the driver.
#[derive(Clone, Debug)]
pub enum Command {
    /// Host a room locally.
    Host(RoomId),
    /// Replicate a remote room on demand, seeding it with an origin peer to dial.
    JoinRemote(RoomId, PeerId),
    /// Append a local op to a room.
    Append(RoomId, Vec<u8>),
    /// Stop the driver loop.
    Shutdown,
}

/// Driver loop timing.
#[derive(Clone, Debug)]
pub struct DriverOpts {
    /// How often the loop services inbound frames + Lane-3 draining.
    pub poll_interval: Duration,
    /// Re-advertise each room's head every N ticks.
    pub announce_every_ticks: u64,
}

impl Default for DriverOpts {
    fn default() -> Self {
        Self { poll_interval: Duration::from_millis(50), announce_every_ticks: 40 }
    }
}

/// A bound iroh endpoint that ships `[RoomId | wire]` frames between peers.
pub struct IrohTransport {
    endpoint: Endpoint,
    _router: Router,
    alpn: Vec<u8>,
    inbound_rx: mpsc::UnboundedReceiver<Inbound>,
    peers: Arc<Mutex<BTreeSet<[u8; 32]>>>,
}

impl IrohTransport {
    /// Bind the endpoint and start accepting sync connections.
    pub async fn bind(cfg: &IrohConfig) -> IrohResult<Self> {
        let sk = SecretKey::from_bytes(&cfg.seed);
        let mut builder = Endpoint::builder(N0).secret_key(sk).alpns(vec![cfg.alpn.clone()]);
        if let Some(port) = cfg.bind_port {
            builder = builder.bind_addr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))?;
        }
        let endpoint = builder.bind().await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let peers: Arc<Mutex<BTreeSet<[u8; 32]>>> =
            Arc::new(Mutex::new(cfg.bootstrap.iter().copied().collect()));
        let handler = SyncHandler { inbound: tx, peers: peers.clone(), max: cfg.max_message_bytes };
        let router = Router::builder(endpoint.clone()).accept(cfg.alpn.clone(), handler).spawn();

        Ok(Self { endpoint, _router: router, alpn: cfg.alpn.clone(), inbound_rx: rx, peers })
    }

    /// This node's 32-byte endpoint id (== its writer key).
    pub fn endpoint_id(&self) -> [u8; 32] {
        *self.endpoint.id().as_bytes()
    }

    /// Record a known peer to gossip to.
    pub fn add_peer(&self, id: [u8; 32]) {
        self.peers.lock().unwrap().insert(id);
    }

    /// The current known-peer set.
    pub fn peers(&self) -> Vec<[u8; 32]> {
        self.peers.lock().unwrap().iter().copied().collect()
    }

    /// Drain inbound frames received since the last call.
    pub fn drain_inbound(&mut self) -> Vec<Inbound> {
        let mut v = Vec::new();
        while let Ok(i) = self.inbound_rx.try_recv() {
            v.push(i);
        }
        v
    }

    /// Ship one [`Outbound`] for `room` per its [`Fanout`].
    pub fn dispatch(&self, room: RoomId, out: Outbound) {
        let targets: Vec<[u8; 32]> = match out.to {
            Fanout::Gossip => self.peers(),
            Fanout::Peer(p) => vec![p],
            // Lane 1 client fanout is a local concern; no peer transport here.
            Fanout::Clients => return,
        };
        let mut frame = room.to_vec();
        frame.extend_from_slice(&wire::encode(&out.msg));
        for t in targets {
            self.send_frame(t, frame.clone());
        }
    }

    fn send_frame(&self, target: [u8; 32], frame: Vec<u8>) {
        let endpoint = self.endpoint.clone();
        let alpn = self.alpn.clone();
        tokio::spawn(async move {
            let Ok(id) = PublicKey::from_bytes(&target) else { return };
            let Ok(conn) = endpoint.connect(id, alpn.as_slice()).await else { return };
            if let Ok((mut send, mut recv)) = conn.open_bi().await {
                let _ = send.write_all(&frame).await;
                let _ = send.finish();
                let _ = recv.read_to_end(16).await; // wait for the 1-byte ack
                conn.close(0u32.into(), b"done");
            }
        });
    }
}

/// The accept side: read one `[RoomId | wire]` frame per connection, enqueue it,
/// ack, and record the peer.
struct SyncHandler {
    inbound: mpsc::UnboundedSender<Inbound>,
    peers: Arc<Mutex<BTreeSet<[u8; 32]>>>,
    max: usize,
}

impl std::fmt::Debug for SyncHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHandler").finish()
    }
}

impl ProtocolHandler for SyncHandler {
    fn accept(
        &self,
        conn: Connection,
    ) -> impl Future<Output = Result<(), AcceptError>> + Send {
        let inbound = self.inbound.clone();
        let peers = self.peers.clone();
        let max = self.max;
        async move {
            let from = *conn.remote_id().as_bytes();
            peers.lock().unwrap().insert(from);

            let (mut send, mut recv) = conn.accept_bi().await.map_err(AcceptError::from_err)?;
            let buf = recv.read_to_end(max).await.map_err(AcceptError::from_err)?;
            if buf.len() >= 32 {
                let mut room = [0u8; 32];
                room.copy_from_slice(&buf[..32]);
                if let Ok(msg) = wire::decode(&buf[32..]) {
                    let _ = inbound.send(Inbound { room, from, msg });
                }
            }
            let _ = send.write_all(b"k").await; // ack so the sender's read_to_end returns
            let _ = send.finish();
            conn.closed().await;
            Ok(())
        }
    }
}

/// Run the node event loop: bind rooms to the transport, pump messages, drain
/// finalized deltas to `sink`, and service local [`Command`]s. Returns when a
/// [`Command::Shutdown`] arrives or the command channel closes.
pub async fn run_server<F, P, Sink>(
    mut server: RoomServer<F, P>,
    mut transport: IrohTransport,
    mut sink: Sink,
    mut commands: mpsc::Receiver<Command>,
    opts: DriverOpts,
) where
    F: StoreFactory + Default,
    P: Projection + Clone,
    Sink: ProjectionSink<P::State>,
{
    let mut tick = 0u64;
    loop {
        tokio::select! {
            maybe = commands.recv() => match maybe {
                Some(Command::Host(room)) => {
                    if server.host(room).is_ok() {
                        announce(&server, &transport, room);
                    }
                }
                Some(Command::JoinRemote(room, origin)) => {
                    transport.add_peer(origin);
                    if server.join_remote(room).is_ok() {
                        announce(&server, &transport, room);
                    }
                }
                Some(Command::Append(room, op)) => {
                    if let Some(r) = server.get_mut(room) {
                        if let Ok(outs) = r.local_append(&op) {
                            for o in outs {
                                transport.dispatch(room, o);
                            }
                        }
                    }
                }
                Some(Command::Shutdown) | None => break,
            },
            _ = tokio::time::sleep(opts.poll_interval) => {
                for Inbound { room, from, msg } in transport.drain_inbound() {
                    transport.add_peer(from);
                    if let Some(r) = server.get_mut(room) {
                        if let Ok(outs) = r.on_inbound(from, msg) {
                            for o in outs {
                                transport.dispatch(room, o);
                            }
                        }
                    }
                }
                let ids: Vec<RoomId> = server.rooms().map(|(id, _)| *id).collect();
                for id in ids {
                    if let Some(r) = server.get_mut(id) {
                        let deltas = r.poll_finalized();
                        for f in deltas {
                            let _ = sink.write(f.version, r.snapshot_finalized());
                        }
                    }
                    if tick % opts.announce_every_ticks == 0 {
                        announce(&server, &transport, id);
                    }
                }
                tick = tick.wrapping_add(1);
            }
        }
    }
}

fn announce<F, P>(server: &RoomServer<F, P>, transport: &IrohTransport, room: RoomId)
where
    F: StoreFactory + Default,
    P: Projection + Clone,
{
    if let Some(r) = server.get(room) {
        for o in r.announce() {
            transport.dispatch(room, o);
        }
    }
}
