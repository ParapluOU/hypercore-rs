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
use std::pin::Pin;

use futures_lite::StreamExt as _;
use iroh::endpoint::{presets::N0, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use iroh_gossip::{api::Event, Gossip, TopicId, ALPN as GOSSIP_ALPN};
use tokio::sync::mpsc;

use crate::driver::Driver;
use crate::projection::Projection;
use crate::room::{Fanout, Outbound, PeerId};
use crate::server::{RoomId, RoomServer};
use crate::store::StoreFactory;
use crate::transport::{Inbound, ProjectionSink, Transport};
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
    /// Request/response protocols served beside roomnet's own (a node's room
    /// list, say), answered on this endpoint and asked with a [`Requester`].
    pub services: Vec<Service>,
}

/// The answering half of a [`Service`]: the asking peer's id and its request,
/// to the response.
pub type Answer = Arc<dyn Fn([u8; 32], Vec<u8>) -> Pin<Box<dyn Future<Output = Vec<u8>> + Send>> + Send + Sync>;

/// A request/response protocol on the transport's endpoint: each connection
/// carries one request and gets one response, both whole messages.
#[derive(Clone)]
pub struct Service {
    pub alpn: Vec<u8>,
    pub max_request_bytes: usize,
    answer: Answer,
}

impl Service {
    /// Serve `alpn`, answering each request (at most `max_request_bytes`) with
    /// `answer`.
    pub fn new<F, Fut>(alpn: impl Into<Vec<u8>>, max_request_bytes: usize, answer: F) -> Self
    where
        F: Fn([u8; 32], Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<u8>> + Send + 'static,
    {
        let answer: Answer = Arc::new(move |peer, request| Box::pin(answer(peer, request)));
        Self { alpn: alpn.into(), max_request_bytes, answer }
    }
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").field("alpn", &String::from_utf8_lossy(&self.alpn)).finish()
    }
}

impl ProtocolHandler for Service {
    fn accept(&self, conn: Connection) -> impl Future<Output = Result<(), AcceptError>> + Send {
        let service = self.clone();
        async move {
            let from = *conn.remote_id().as_bytes();
            let (mut send, mut recv) = conn.accept_bi().await.map_err(AcceptError::from_err)?;
            let request = recv.read_to_end(service.max_request_bytes).await.map_err(AcceptError::from_err)?;
            let response = (service.answer)(from, request).await;
            send.write_all(&response).await.map_err(AcceptError::from_err)?;
            send.finish().map_err(AcceptError::from_err)?;
            conn.closed().await;
            Ok(())
        }
    }
}

/// Asks peers' [`Service`]s from a transport's endpoint; cheap to clone and
/// usable apart from the driver loop that owns the transport.
#[derive(Clone, Debug)]
pub struct Requester {
    endpoint: Endpoint,
}

impl Requester {
    /// Ask peer `peer`'s service `alpn` with `request`; its response, at most
    /// `max_response_bytes`.
    pub async fn request(&self, peer: [u8; 32], alpn: &[u8], request: &[u8], max_response_bytes: usize) -> IrohResult<Vec<u8>> {
        let id = PublicKey::from_bytes(&peer)?;
        // as dispatch: through our home relay when there is one, else by key
        let dest: EndpointAddr = match self.endpoint.addr().relay_urls().next() {
            Some(relay) => EndpointAddr::new(id).with_relay_url(relay.clone()),
            None => id.into(),
        };
        self.request_at(dest, alpn, request, max_response_bytes).await
    }

    /// [`Requester::request`] to a peer at a known address.
    pub async fn request_at(&self, peer: EndpointAddr, alpn: &[u8], request: &[u8], max_response_bytes: usize) -> IrohResult<Vec<u8>> {
        let conn = self.endpoint.connect(peer, alpn).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(request).await?;
        send.finish()?;
        let response = recv.read_to_end(max_response_bytes).await?;
        conn.close(0u32.into(), b"done");
        Ok(response)
    }
}

impl Default for IrohConfig {
    fn default() -> Self {
        Self {
            alpn: b"roomnet/sync/0".to_vec(),
            seed: [0u8; 32],
            bind_port: None,
            bootstrap: Vec::new(),
            max_message_bytes: 16 * 1024 * 1024,
            services: Vec::new(),
        }
    }
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
    /// Per-room peer discovery. Subscribing to a room's topic finds co-subscribers
    /// (their `NeighborUp`s populate `peers`) and, because the overlay dials them,
    /// teaches the endpoint their addresses so the direct-dial sync path resolves.
    gossip: Gossip,
}

impl IrohTransport {
    /// Bind the endpoint and start accepting sync connections.
    pub async fn bind(cfg: &IrohConfig) -> IrohResult<Self> {
        let sk = SecretKey::from_bytes(&cfg.seed);
        let mut builder = Endpoint::builder(N0)
            .secret_key(sk)
            .alpns(
                [cfg.alpn.clone(), GOSSIP_ALPN.to_vec()]
                    .into_iter()
                    .chain(cfg.services.iter().map(|service| service.alpn.clone()))
                    .collect(),
            );
        if let Some(port) = cfg.bind_port {
            builder = builder.bind_addr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))?;
        }
        let endpoint = builder.bind().await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let peers: Arc<Mutex<BTreeSet<[u8; 32]>>> =
            Arc::new(Mutex::new(cfg.bootstrap.iter().copied().collect()));
        let handler = SyncHandler { inbound: tx, peers: peers.clone(), max: cfg.max_message_bytes };
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let mut router = Router::builder(endpoint.clone())
            .accept(cfg.alpn.clone(), handler)
            .accept(GOSSIP_ALPN.to_vec(), gossip.clone());
        for service in &cfg.services {
            router = router.accept(service.alpn.clone(), service.clone());
        }
        let router = router.spawn();

        Ok(Self {
            endpoint,
            _router: router,
            alpn: cfg.alpn.clone(),
            inbound_rx: rx,
            peers,
            gossip,
        })
    }

    /// Join a room's gossip topic for peer discovery. `bootstrap` are peers to seed
    /// the overlay with (the origin, for a replica; empty for a host that only needs
    /// to accept). Spawns a task that adds each discovered neighbour to `peers` —
    /// and, as a side effect of the overlay dialling them, the endpoint learns their
    /// addresses so the direct-dial sync path can reach them.
    pub fn discover_room(&self, room: [u8; 32], bootstrap: Vec<[u8; 32]>) {
        let gossip = self.gossip.clone();
        let peers = self.peers.clone();
        tokio::spawn(async move {
            let topic = TopicId::from_bytes(room);
            let boot: Vec<PublicKey> = bootstrap
                .iter()
                .filter_map(|k| PublicKey::from_bytes(k).ok())
                .collect();
            let mut sub = match gossip.subscribe(topic, boot).await {
                Ok(s) => s,
                Err(_) => return,
            };
            while let Some(ev) = sub.next().await {
                if let Ok(Event::NeighborUp(id)) = ev {
                    peers.lock().unwrap().insert(*id.as_bytes());
                }
            }
        });
    }

    /// This node's 32-byte endpoint id (== its writer key).
    /// Asks peers' services from this endpoint (see [`Service`]).
    pub fn requester(&self) -> Requester {
        Requester { endpoint: self.endpoint.clone() }
    }

    /// Where this endpoint can be reached, as far as it knows.
    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

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
            // Dial via our home relay (peers share the same n0 relay) so we don't
            // wait on discovery resolving the peer's key by DNS — which is slow and,
            // as observed, asymmetric. Fall back to a bare-key dial (discovery) if
            // the relay isn't up yet.
            let dest: EndpointAddr = match endpoint.addr().relay_urls().next() {
                Some(relay) => EndpointAddr::new(id).with_relay_url(relay.clone()),
                None => id.into(),
            };
            let Ok(conn) = endpoint.connect(dest, alpn.as_slice()).await else { return };
            if let Ok((mut send, mut recv)) = conn.open_bi().await {
                let _ = send.write_all(&frame).await;
                let _ = send.finish();
                let _ = recv.read_to_end(16).await; // wait for the 1-byte ack
                conn.close(0u32.into(), b"done");
            }
        });
    }
}

/// `IrohTransport` is the native realisation of the sans-IO [`Transport`] seam:
/// its inherent `dispatch`/`drain_inbound` already match the trait, so the impl
/// forwards to them. Dispatch is fire-and-forget (it spawns a dial task), so it is
/// [`Infallible`](core::convert::Infallible).
impl Transport for IrohTransport {
    type Error = core::convert::Infallible;

    fn dispatch(&mut self, room: RoomId, out: Outbound) -> Result<(), Self::Error> {
        IrohTransport::dispatch(self, room, out);
        Ok(())
    }

    fn drain_inbound(&mut self) -> Vec<Inbound> {
        IrohTransport::drain_inbound(self)
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
///
/// This is a thin tokio shell over the runtime-agnostic [`Driver`]: `select!`
/// routes each [`Command`] to the matching driver method and, on each
/// `poll_interval`, calls [`Driver::tick`] and feeds the finalized deltas it
/// returns to `sink`. (The inbound sender is already recorded as a peer by the
/// accept handler, so the driver's internal pump needs no extra `add_peer`.)
pub async fn run_server<F, P, Sink>(
    server: RoomServer<F, P>,
    transport: IrohTransport,
    mut sink: Sink,
    mut commands: mpsc::Receiver<Command>,
    opts: DriverOpts,
) where
    F: StoreFactory,
    P: Projection + Clone,
    Sink: ProjectionSink<P::State>,
{
    let mut driver = Driver::new(server, transport, opts.announce_every_ticks);
    loop {
        tokio::select! {
            maybe = commands.recv() => match maybe {
                Some(Command::Host(room)) => {
                    let _ = driver.host(room);
                }
                Some(Command::JoinRemote(room, origin)) => {
                    driver.transport_mut().add_peer(origin);
                    let _ = driver.join_remote(room);
                }
                Some(Command::Append(room, op)) => {
                    let _ = driver.append(room, &op);
                }
                Some(Command::Shutdown) | None => break,
            },
            _ = tokio::time::sleep(opts.poll_interval) => {
                for (room, deltas) in driver.tick() {
                    for f in deltas {
                        if let Some(r) = driver.server().get(room) {
                            let _ = sink.write(f.version, r.snapshot_finalized());
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod service_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "binds UDP sockets; run outside a restricted sandbox"]
    async fn a_service_answers_a_peers_request() {
        let echo = Service::new(b"test/echo/0".to_vec(), 1024, |peer: [u8; 32], request: Vec<u8>| async move {
            [&peer[..1], &request[..]].concat()
        });
        let a = IrohTransport::bind(&IrohConfig { seed: [1; 32], services: vec![echo], ..Default::default() }).await.unwrap();
        let b = IrohTransport::bind(&IrohConfig { seed: [2; 32], ..Default::default() }).await.unwrap();
        let mut at = a.endpoint_addr();
        for _ in 0..50 {
            if at.ip_addrs().next().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            at = a.endpoint_addr();
        }
        // a direct address only, so the test needs no relay
        let direct = EndpointAddr::from_parts(at.id, at.ip_addrs().map(|addr| iroh::TransportAddr::Ip(*addr)));
        let answer = b.requester().request_at(direct, b"test/echo/0", b"hello", 1024).await.unwrap();
        assert_eq!(answer, [&b.endpoint_id()[..1], b"hello"].concat(), "answered with the asking peer's id and its request");
    }
}
