use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::ConnectInfo;
use futures_util::StreamExt;
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rustls::ServerConfig;
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::Service;
use tracing::{debug, warn};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_secs(1);
const MAX_CONCURRENT_HANDSHAKES: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {message}")]
    Parse { path: String, message: String },
    #[error("no certificates found in {0}")]
    NoCertificates(String),
    #[error("no private key found in {0}")]
    NoPrivateKey(String),
    #[error("unusable private key: {0}")]
    SigningKey(String),
    #[error("building server config: {0}")]
    Config(String),
    #[error("certificate and private key do not match: {0}")]
    KeyMismatch(String),
}

pub struct ReloadableCertResolver {
    current: ArcSwap<CertifiedKey>,
}

impl std::fmt::Debug for ReloadableCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadableCertResolver")
            .finish_non_exhaustive()
    }
}

impl ReloadableCertResolver {
    pub fn new(initial: CertifiedKey) -> Self {
        Self {
            current: ArcSwap::from_pointee(initial),
        }
    }

    pub fn store(&self, key: CertifiedKey) {
        self.current.store(Arc::new(key));
    }
}

impl ResolvesServerCert for ReloadableCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

pub fn load_certified_key(cert_path: &str, key_path: &str) -> Result<CertifiedKey, TlsError> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let signing_key =
        ring::sign::any_supported_type(&key).map_err(|e| TlsError::SigningKey(e.to_string()))?;
    let certified = CertifiedKey::new(certs, signing_key);
    certified
        .keys_match()
        .map_err(|e| TlsError::KeyMismatch(e.to_string()))?;
    Ok(certified)
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_string(),
        source,
    })?;
    let mut reader = BufReader::new(bytes.as_slice());
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Parse {
            path: path.to_string(),
            message: e.to_string(),
        })?;
    match certs.is_empty() {
        true => Err(TlsError::NoCertificates(path.to_string())),
        false => Ok(certs),
    }
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_string(),
        source,
    })?;
    let mut reader = BufReader::new(bytes.as_slice());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| TlsError::Parse {
            path: path.to_string(),
            message: e.to_string(),
        })?
        .ok_or_else(|| TlsError::NoPrivateKey(path.to_string()))
}

pub fn build_server_config(
    resolver: Arc<ReloadableCertResolver>,
) -> Result<ServerConfig, TlsError> {
    let provider = Arc::new(ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::Config(e.to_string()))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

pub fn spawn_reload_handler(
    resolver: Arc<ReloadableCertResolver>,
    cert_path: String,
    key_path: String,
    shutdown: CancellationToken,
) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(stream) => stream,
            Err(e) => {
                tracing::error!("Failed to install SIGHUP handler: {e}");
                return;
            }
        };
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                received = hangup.recv() => {
                    if received.is_none() {
                        break;
                    }
                    match load_certified_key(&cert_path, &key_path) {
                        Ok(key) => {
                            resolver.store(key);
                            tracing::info!("TLS certificate and key reloaded");
                        }
                        Err(e) => {
                            warn!("TLS reload failed, keeping existing certificate: {e}");
                        }
                    }
                }
            }
        }
    });

    #[cfg(not(unix))]
    let _ = (resolver, cert_path, key_path, shutdown);
}

fn is_connection_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

pub async fn serve_tls(
    listener: TcpListener,
    app: Router,
    server_config: Arc<ServerConfig>,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from(server_config);
    let tracker = TaskTracker::new();
    let handshake_limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDSHAKES));

    let connections = futures_util::stream::unfold(listener, |listener| async move {
        Some((listener.accept().await, listener))
    });

    connections
        .take_until(shutdown.clone().cancelled_owned())
        .for_each(|accepted| {
            let acceptor = acceptor.clone();
            let app = app.clone();
            let conn_shutdown = shutdown.clone();
            let limiter = handshake_limiter.clone();
            let tracker = &tracker;
            async move {
                match accepted {
                    Ok((tcp, peer)) => {
                        let permit = tokio::select! {
                            biased;
                            _ = conn_shutdown.cancelled() => return,
                            permit = limiter.acquire_owned() => match permit {
                                Ok(permit) => permit,
                                Err(_) => return,
                            },
                        };
                        tracker.spawn(serve_connection(
                            acceptor,
                            app,
                            tcp,
                            peer,
                            conn_shutdown,
                            permit,
                        ));
                    }
                    Err(e) if is_connection_error(&e) => {
                        debug!("TLS accept connection error: {e}");
                    }
                    Err(e) => {
                        warn!(
                            "TLS accept failed, pausing {ACCEPT_ERROR_BACKOFF:?} before retry: {e}"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                            _ = conn_shutdown.cancelled() => {}
                        }
                    }
                }
            }
        })
        .await;

    tracker.close();
    tracker.wait().await;
    Ok(())
}

async fn serve_connection(
    acceptor: TlsAcceptor,
    app: Router,
    tcp: TcpStream,
    peer: SocketAddr,
    shutdown: CancellationToken,
    handshake_permit: OwnedSemaphorePermit,
) {
    let tls_stream = tokio::select! {
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)) => match result {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                debug!("TLS handshake with {peer} failed: {e}");
                return;
            }
            Err(_) => {
                debug!("TLS handshake with {peer} timed out after {HANDSHAKE_TIMEOUT:?}");
                return;
            }
        },
        _ = shutdown.cancelled() => {
            debug!("shutdown during TLS handshake with {peer}");
            return;
        }
    };
    drop(handshake_permit);

    let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
        request.extensions_mut().insert(ConnectInfo(peer));
        app.clone().call(request)
    });

    let builder = auto::Builder::new(TokioExecutor::new());
    let connection = builder.serve_connection_with_upgrades(TokioIo::new(tls_stream), service);
    tokio::pin!(connection);

    tokio::select! {
        result = connection.as_mut() => {
            if let Err(e) = result {
                debug!("connection from {peer} ended: {e}");
            }
        }
        _ = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            match tokio::time::timeout(SHUTDOWN_GRACE, connection.as_mut()).await {
                Ok(Err(e)) => debug!("connection from {peer} ended during shutdown: {e}"),
                Err(_) => debug!("connection from {peer} did not drain within grace, dropping"),
                Ok(Ok(())) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const CERT_1: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBrTCCAVKgAwIBAgIUWIlnxLpgk7qp8We8ya6UW1I7p0MwCgYIKoZIzj0EAwIw\n\
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUyNjEyMTQyMloXDTM2MDUyMzEy\n\
MTQyMlowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D\n\
AQcDQgAEgq5UvmRilQh66D5C+78TdULpCuIrI7dtvBB589iJK8Gq14SW9ewkbiWD\n\
QrXirV47GPzRnODrDIqFSCa4yH+dz6OBgTB/MB0GA1UdDgQWBBSVcvSAd4XB3SCU\n\
e8MKSOm9i6yigjAfBgNVHSMEGDAWgBSVcvSAd4XB3SCUe8MKSOm9i6yigjAPBgNV\n\
HRMBAf8EBTADAQH/MCwGA1UdEQQlMCOCCWxvY2FsaG9zdIcQAAAAAAAAAAAAAAAA\n\
AAAAAYcEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA6pIKG7uRbgzuOCDY1Rm+QCuF\n\
/UTOjWKrfZhoDnXP+swCIQCV7p6vRSt0GnbRzIIcN8UM68cXDZX+Nk0XofZaN217\n\
mg==\n\
-----END CERTIFICATE-----\n";

    const KEY_1: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgMobX2BajiDVtV5Ti\n\
kiJ8qEbduI0HvT/qORtLjjCXQ5OhRANCAASCrlS+ZGKVCHroPkL7vxN1QukK4isj\n\
t228EHnz2IkrwarXhJb17CRuJYNCteKtXjsY/NGc4OsMioVIJrjIf53P\n\
-----END PRIVATE KEY-----\n";

    const CERT_2: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBrDCCAVKgAwIBAgIUJjaLQsKBClkIbtSmDK9vZ9gCrbQwCgYIKoZIzj0EAwIw\n\
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUyNjEyMTQyMloXDTM2MDUyMzEy\n\
MTQyMlowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D\n\
AQcDQgAEI6ljji6CAII88C48Hu7kzEjnV9gMVs8v8Oom04PfcXPR/GSUc0MYz3y4\n\
LXZC2yNJl40ynzuXNhisk/mQjYbKYaOBgTB/MB0GA1UdDgQWBBTqLGV3rtN9hiuR\n\
oHUPNnvkwz/DbDAfBgNVHSMEGDAWgBTqLGV3rtN9hiuRoHUPNnvkwz/DbDAPBgNV\n\
HRMBAf8EBTADAQH/MCwGA1UdEQQlMCOCCWxvY2FsaG9zdIcQAAAAAAAAAAAAAAAA\n\
AAAAAYcEfwAAATAKBggqhkjOPQQDAgNIADBFAiAMVxuI5vyDYi1RtsyuiB+sIl1D\n\
SdSOaWIgtxPVs5E0CQIhAIrrra+TPrmE8JrjwJBlsONl3oTlOcfDA9WP/FnYbHuv\n\
-----END CERTIFICATE-----\n";

    const KEY_2: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgyBJsGRjta0gqCcBH\n\
LI5Q1uj42QD1KUfmkOj+o4jlDlmhRANCAAQjqWOOLoIAgjzwLjwe7uTMSOdX2AxW\n\
zy/w6ibTg99xc9H8ZJRzQxjPfLgtdkLbI0mXjTKfO5c2GKyT+ZCNhsph\n\
-----END PRIVATE KEY-----\n";

    #[derive(Debug)]
    struct AcceptAnyServerCert;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    fn write_temp(contents: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tranquil_tls_test_{}_{unique}.pem",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temp pem");
        path
    }

    #[test]
    fn loads_certificate_and_key() {
        let cert = write_temp(CERT_1);
        let key = write_temp(KEY_1);
        let certified = load_certified_key(cert.to_str().unwrap(), key.to_str().unwrap())
            .expect("load certified key");
        assert_eq!(certified.cert.len(), 1);
    }

    #[test]
    fn missing_certificate_file_is_read_error() {
        let result = load_certs("/nonexistent/tranquil/cert.pem");
        assert!(matches!(result, Err(TlsError::Read { .. })));
    }

    #[test]
    fn empty_certificate_file_has_no_certificates() {
        let cert = write_temp("");
        let result = load_certs(cert.to_str().unwrap());
        assert!(matches!(result, Err(TlsError::NoCertificates(_))));
    }

    #[test]
    fn certificate_without_key_is_missing_key() {
        let cert_only = write_temp(CERT_1);
        let result = load_private_key(cert_only.to_str().unwrap());
        assert!(matches!(result, Err(TlsError::NoPrivateKey(_))));
    }

    #[test]
    fn server_config_advertises_h2_and_http1() {
        let cert = write_temp(CERT_1);
        let key = write_temp(KEY_1);
        let certified = load_certified_key(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
        let resolver = Arc::new(ReloadableCertResolver::new(certified));
        let config = build_server_config(resolver).expect("build server config");
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn reload_swaps_the_served_certificate() {
        let cert1 = write_temp(CERT_1);
        let key1 = write_temp(KEY_1);
        let cert2 = write_temp(CERT_2);
        let key2 = write_temp(KEY_2);

        let first = load_certified_key(cert1.to_str().unwrap(), key1.to_str().unwrap()).unwrap();
        let resolver = ReloadableCertResolver::new(first);
        let before = resolver.current.load_full().cert.clone();

        let second = load_certified_key(cert2.to_str().unwrap(), key2.to_str().unwrap()).unwrap();
        resolver.store(second);
        let after = resolver.current.load_full().cert.clone();

        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn terminates_tls_over_ipv6_and_negotiates_alpn() {
        use rustls::pki_types::ServerName;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::TlsConnector;

        let cert = write_temp(CERT_1);
        let key = write_temp(KEY_1);
        let certified = load_certified_key(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
        let resolver = Arc::new(ReloadableCertResolver::new(certified));
        let server_config = Arc::new(build_server_config(resolver).unwrap());

        let app = Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.is_ipv6(), "expected ipv6 bind, got {addr}");

        let shutdown = CancellationToken::new();
        let server = tokio::spawn(serve_tls(listener, app, server_config, shutdown.clone()));

        let mut client_config =
            rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
                .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = TlsConnector::from(Arc::new(client_config));
        let server_name = ServerName::try_from("localhost").unwrap();

        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();

        let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
        assert_eq!(alpn, Some(b"http/1.1".to_vec()));

        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "unexpected response: {text}"
        );
        assert!(text.trim_end().ends_with("ok"), "unexpected body: {text}");

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }

    #[test]
    fn mismatched_cert_and_key_is_rejected() {
        let cert = write_temp(CERT_1);
        let key = write_temp(KEY_2);
        let result = load_certified_key(cert.to_str().unwrap(), key.to_str().unwrap());
        assert!(matches!(result, Err(TlsError::KeyMismatch(_))));
    }

    #[tokio::test]
    async fn negotiates_h2_when_client_offers_only_h2() {
        use rustls::pki_types::ServerName;
        use tokio_rustls::TlsConnector;

        let cert = write_temp(CERT_1);
        let key = write_temp(KEY_1);
        let certified = load_certified_key(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
        let resolver = Arc::new(ReloadableCertResolver::new(certified));
        let server_config = Arc::new(build_server_config(resolver).unwrap());

        let app = Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let shutdown = CancellationToken::new();
        let server = tokio::spawn(serve_tls(listener, app, server_config, shutdown.clone()));

        let mut client_config =
            rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
                .with_no_client_auth();
        client_config.alpn_protocols = vec![b"h2".to_vec()];
        let connector = TlsConnector::from(Arc::new(client_config));
        let server_name = ServerName::try_from("localhost").unwrap();

        let tcp = TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect(server_name, tcp).await.unwrap();

        let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
        assert_eq!(alpn, Some(b"h2".to_vec()));

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }
}
