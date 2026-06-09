use backon::{ExponentialBuilder, Retryable};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    ClientConfig, Connection, Endpoint, IdleTimeout, RecvStream, ServerConfig, TransportConfig,
    VarInt,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_FRAME_SIZE: usize = 4 * 1024 * 1024;
const MAX_INBOUND_CONNECTIONS: usize = 512;
const MAX_OUTBOUND_CONNECTIONS: usize = 512;
const MAX_CONCURRENT_UNI_STREAMS: u32 = 64;
const READ_CHUNK_BYTES: usize = 256 * 1024;
const INCOMING_CHANNEL_DEPTH: usize = 1024;
const STREAM_RECEIVE_WINDOW: u32 = MAX_FRAME_SIZE as u32;
const CONNECTION_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;
const KEEPALIVE: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const RIPPLE_ALPN: &[u8] = b"ripple/1";
const RIPPLE_SERVER_NAME: &str = "ripple";

struct NodeIdentity {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelTag {
    Gossip = 0x01,
    CrdtSync = 0x02,
}

impl ChannelTag {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::Gossip),
            0x02 => Some(Self::CrdtSync),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct IncomingFrame {
    pub from: SocketAddr,
    pub tag: ChannelTag,
    pub data: Vec<u8>,
}

struct PeerConn {
    conn: Connection,
    generation: u64,
}

pub struct Transport {
    endpoint: Endpoint,
    local_addr: SocketAddr,
    connections: Arc<parking_lot::Mutex<HashMap<SocketAddr, PeerConn>>>,
    connecting: Arc<parking_lot::Mutex<std::collections::HashSet<SocketAddr>>>,
    conn_generation: Arc<AtomicU64>,
    outbound_permits: Arc<Semaphore>,
    shutdown: CancellationToken,
    incoming_tx: mpsc::Sender<IncomingFrame>,
}

impl Transport {
    pub async fn bind(
        addr: SocketAddr,
        shutdown: CancellationToken,
    ) -> Result<(Self, mpsc::Receiver<IncomingFrame>), std::io::Error> {
        let server_config = build_server_config()
            .map_err(|e| std::io::Error::other(format!("ripple server config: {e}")))?;
        let client_config = build_client_config()
            .map_err(|e| std::io::Error::other(format!("ripple client config: {e}")))?;

        let mut endpoint = Endpoint::server(server_config, addr)?;
        endpoint.set_default_client_config(client_config);
        let local_addr = endpoint.local_addr()?;
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CHANNEL_DEPTH);

        let transport = Self {
            endpoint: endpoint.clone(),
            local_addr,
            connections: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            connecting: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            conn_generation: Arc::new(AtomicU64::new(0)),
            outbound_permits: Arc::new(Semaphore::new(MAX_OUTBOUND_CONNECTIONS)),
            shutdown: shutdown.clone(),
            incoming_tx: incoming_tx.clone(),
        };

        let inbound_permits = Arc::new(Semaphore::new(MAX_INBOUND_CONNECTIONS));
        let cancel = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    incoming = endpoint.accept() => {
                        let Some(incoming) = incoming else { break };
                        if !incoming.remote_address_validated() {
                            let _ = incoming.retry();
                            continue;
                        }
                        let Ok(permit) = inbound_permits.clone().try_acquire_owned() else {
                            tracing::warn!(
                                peer = %incoming.remote_address(),
                                max = MAX_INBOUND_CONNECTIONS,
                                "rejecting inbound connection: limit reached"
                            );
                            incoming.refuse();
                            continue;
                        };
                        let tx = incoming_tx.clone();
                        let conn_cancel = cancel.child_token();
                        tokio::spawn(async move {
                            let _permit = permit;
                            match incoming.await {
                                Ok(conn) => {
                                    let from = conn.remote_address();
                                    tracing::debug!(peer = %from, "accepted inbound connection");
                                    run_conn_reader(conn, from, tx, conn_cancel).await;
                                }
                                Err(e) => tracing::warn!(error = %e, "inbound handshake failed"),
                            }
                        });
                    }
                }
            }
            endpoint.close(0u32.into(), b"shutdown");
        });

        tracing::info!(addr = %local_addr, "ripple quic transport bound");
        Ok((transport, incoming_rx))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn try_queue(&self, target: SocketAddr, tag: ChannelTag, data: &[u8]) -> bool {
        let conn = self.connections.lock().get(&target).map(|p| p.conn.clone());
        let Some(conn) = conn else { return false };
        let data = data.to_vec();
        tokio::spawn(async move {
            if let Err(e) = write_frame(&conn, tag, &data).await {
                tracing::debug!(error = %e, "queued write failed");
            }
        });
        true
    }

    pub async fn send(&self, target: SocketAddr, tag: ChannelTag, data: &[u8]) {
        let existing = self
            .connections
            .lock()
            .get(&target)
            .map(|p| (p.conn.clone(), p.generation));
        if let Some((conn, generation)) = existing {
            match write_frame(&conn, tag, data).await {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    tracing::debug!(peer = %target, "write timed out, keeping connection");
                    return;
                }
                Err(_) => {
                    let mut conns = self.connections.lock();
                    if conns
                        .get(&target)
                        .is_some_and(|p| p.generation == generation)
                    {
                        conns.remove(&target);
                    }
                }
            }
        }
        self.connect_and_send(target, tag, data).await;
    }

    async fn connect_and_send(&self, target: SocketAddr, tag: ChannelTag, data: &[u8]) {
        {
            let mut connecting = self.connecting.lock();
            if connecting.contains(&target) {
                tracing::warn!(peer = %target, "connection already in-flight, dropping frame");
                return;
            }
            connecting.insert(target);
        }

        self.connect_and_send_inner(target, tag, data).await;
        self.connecting.lock().remove(&target);
    }

    async fn connect_and_send_inner(&self, target: SocketAddr, tag: ChannelTag, data: &[u8]) {
        let Ok(permit) = self.outbound_permits.clone().try_acquire_owned() else {
            tracing::warn!(
                peer = %target,
                max = MAX_OUTBOUND_CONNECTIONS,
                "outbound connection limit reached, dropping"
            );
            return;
        };
        let shutdown = self.shutdown.clone();
        let endpoint = self.endpoint.clone();
        let conn = (|| async {
            let connecting = endpoint
                .connect(target, RIPPLE_SERVER_NAME)
                .map_err(std::io::Error::other)?;
            tokio::time::timeout(CONNECT_TIMEOUT, connecting)
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))?
                .map_err(std::io::Error::other)
        })
        .retry(
            ExponentialBuilder::default()
                .with_min_delay(Duration::from_millis(50))
                .with_max_delay(Duration::from_secs(2))
                .with_max_times(3),
        )
        .when(|_| !shutdown.is_cancelled())
        .await;

        match conn {
            Ok(conn) => {
                let generation = self.conn_generation.fetch_add(1, Ordering::Relaxed);
                self.connections.lock().insert(
                    target,
                    PeerConn {
                        conn: conn.clone(),
                        generation,
                    },
                );
                if let Err(e) = write_frame(&conn, tag, data).await {
                    tracing::warn!(peer = %target, error = %e, "initial write failed");
                }

                let reader_cancel = self.shutdown.child_token();
                tokio::spawn(run_conn_reader(
                    conn.clone(),
                    target,
                    self.incoming_tx.clone(),
                    reader_cancel.clone(),
                ));

                let connections = self.connections.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    conn.closed().await;
                    {
                        let mut conns = connections.lock();
                        if conns
                            .get(&target)
                            .is_some_and(|p| p.generation == generation)
                        {
                            conns.remove(&target);
                        }
                    }
                    reader_cancel.cancel();
                });
                tracing::debug!(peer = %target, "established outbound connection");
            }
            Err(e) => {
                tracing::warn!(peer = %target, error = %e, "failed to connect after retries");
            }
        }
    }
}

async fn run_conn_reader(
    conn: Connection,
    from: SocketAddr,
    incoming_tx: mpsc::Sender<IncomingFrame>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = conn.accept_uni() => match accepted {
                Ok(recv) => {
                    tokio::spawn(read_frame(
                        recv,
                        from,
                        incoming_tx.clone(),
                    ));
                }
                Err(_) => break,
            }
        }
    }
}

enum FrameReadError {
    Oversize,
    Stream(String),
}

async fn read_frame(recv: RecvStream, from: SocketAddr, incoming_tx: mpsc::Sender<IncomingFrame>) {
    let read = async {
        let mut recv = recv;
        let mut tag_byte = [0u8; 1];
        recv.read_exact(&mut tag_byte)
            .await
            .map_err(|e| FrameReadError::Stream(e.to_string()))?;
        let mut data = Vec::with_capacity(READ_CHUNK_BYTES);
        loop {
            match recv.read_chunk(READ_CHUNK_BYTES, true).await {
                Ok(Some(chunk)) => {
                    let len = chunk.bytes.len();
                    if data.len() + len > MAX_FRAME_SIZE {
                        return Err(FrameReadError::Oversize);
                    }
                    data.extend_from_slice(&chunk.bytes);
                }
                Ok(None) => break,
                Err(e) => return Err(FrameReadError::Stream(e.to_string())),
            }
        }
        Ok::<(u8, Vec<u8>), FrameReadError>((tag_byte[0], data))
    };

    match tokio::time::timeout(READ_TIMEOUT, read).await {
        Ok(Ok((tag_byte, data))) => match ChannelTag::from_u8(tag_byte) {
            Some(tag) => {
                let frame = IncomingFrame { from, tag, data };
                if let Err(e) = incoming_tx.try_send(frame) {
                    tracing::warn!(peer = %from, error = %e, "incoming frame channel full, dropping frame");
                }
            }
            None => tracing::debug!(tag = tag_byte, "unknown channel tag, dropping frame"),
        },
        Ok(Err(FrameReadError::Oversize)) => {
            tracing::debug!(peer = %from, max = MAX_FRAME_SIZE, "inbound frame exceeds max size, dropping");
        }
        Ok(Err(FrameReadError::Stream(msg))) => {
            tracing::debug!(peer = %from, error = %msg, "failed reading uni stream");
        }
        Err(_) => {
            tracing::debug!(peer = %from, "inbound frame read timed out, dropping");
        }
    }
}

async fn write_frame(conn: &Connection, tag: ChannelTag, data: &[u8]) -> std::io::Result<()> {
    if data.len() > MAX_FRAME_SIZE {
        tracing::warn!(
            frame_len = data.len(),
            max = MAX_FRAME_SIZE,
            "refusing to send oversized frame"
        );
        return Ok(());
    }
    let timed_out = || std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout");
    let deadline = tokio::time::Instant::now() + WRITE_TIMEOUT;
    match tokio::time::timeout_at(deadline, conn.open_uni()).await {
        Ok(Ok(mut send)) => {
            let write = async {
                send.write_all(&[tag as u8])
                    .await
                    .map_err(std::io::Error::other)?;
                send.write_all(data).await.map_err(std::io::Error::other)?;
                send.finish().map_err(std::io::Error::other)
            };
            let outcome = tokio::time::timeout_at(deadline, write).await;
            match outcome {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => {
                    let _ = send.reset(0u32.into());
                    Err(e)
                }
                Err(_) => {
                    let _ = send.reset(0u32.into());
                    Err(timed_out())
                }
            }
        }
        Ok(Err(e)) => Err(std::io::Error::other(e)),
        Err(_) => Err(timed_out()),
    }
}

fn transport_config() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_concurrent_uni_streams(VarInt::from(MAX_CONCURRENT_UNI_STREAMS));
    tc.stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW));
    tc.receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW));
    tc.keep_alive_interval(Some(KEEPALIVE));
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(IDLE_TIMEOUT).expect("idle timeout fits in varint"),
    ));
    tc
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn ephemeral_identity() -> Result<NodeIdentity, BoxError> {
    let cert = rcgen::generate_simple_self_signed(vec![RIPPLE_SERVER_NAME.to_string()])?;
    Ok(NodeIdentity {
        cert: cert.cert.der().clone(),
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())),
    })
}

fn build_server_config() -> Result<ServerConfig, BoxError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?;
    let ephemeral = ephemeral_identity()?;
    let mut crypto = builder
        .with_no_client_auth()
        .with_single_cert(vec![ephemeral.cert], ephemeral.key)?;
    crypto.alpn_protocols = vec![RIPPLE_ALPN.to_vec()];

    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

fn build_client_config() -> Result<ClientConfig, BoxError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(SkipServerVerification::new(provider.clone()));
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![RIPPLE_ALPN.to_vec()];

    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self(provider)
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quic_frame_roundtrip() {
        let shutdown = CancellationToken::new();
        let (sender, _rx_sender) =
            Transport::bind("127.0.0.1:0".parse().unwrap(), shutdown.clone())
                .await
                .expect("bind sender");
        let (receiver, mut rx_receiver) =
            Transport::bind("127.0.0.1:0".parse().unwrap(), shutdown.clone())
                .await
                .expect("bind receiver");
        let target = receiver.local_addr();

        sender
            .send(target, ChannelTag::Gossip, b"hello ripple")
            .await;

        let frame = tokio::time::timeout(Duration::from_secs(5), rx_receiver.recv())
            .await
            .expect("frame arrives before timeout")
            .expect("channel open");
        assert_eq!(frame.tag, ChannelTag::Gossip);
        assert_eq!(frame.data, b"hello ripple");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn distinct_channels_roundtrip() {
        let shutdown = CancellationToken::new();
        let (sender, _rx_sender) =
            Transport::bind("127.0.0.1:0".parse().unwrap(), shutdown.clone())
                .await
                .expect("bind sender");
        let (receiver, mut rx_receiver) =
            Transport::bind("127.0.0.1:0".parse().unwrap(), shutdown.clone())
                .await
                .expect("bind receiver");
        let target = receiver.local_addr();

        sender.send(target, ChannelTag::Gossip, b"first").await;
        sender.send(target, ChannelTag::CrdtSync, b"second").await;

        let mut seen = Vec::new();
        for _ in 0..2 {
            let frame = tokio::time::timeout(Duration::from_secs(5), rx_receiver.recv())
                .await
                .expect("frame arrives before timeout")
                .expect("channel open");
            seen.push((frame.tag, frame.data));
        }
        assert!(seen.contains(&(ChannelTag::Gossip, b"first".to_vec())));
        assert!(seen.contains(&(ChannelTag::CrdtSync, b"second".to_vec())));

        shutdown.cancel();
    }

    #[tokio::test]
    async fn incoming_frame_from_matches_peer_listen_addr() {
        let shutdown = CancellationToken::new();
        let (sender, _rx_sender) =
            Transport::bind("127.0.0.1:0".parse().unwrap(), shutdown.clone())
                .await
                .expect("bind sender");
        let (receiver, mut rx_receiver) =
            Transport::bind("127.0.0.1:0".parse().unwrap(), shutdown.clone())
                .await
                .expect("bind receiver");

        sender
            .send(receiver.local_addr(), ChannelTag::Gossip, b"addr check")
            .await;

        let frame = tokio::time::timeout(Duration::from_secs(5), rx_receiver.recv())
            .await
            .expect("frame arrives before timeout")
            .expect("channel open");
        assert_eq!(
            frame.from,
            sender.local_addr(),
            "inbound frames must report the peer's canonical listen address"
        );

        shutdown.cancel();
    }
}
