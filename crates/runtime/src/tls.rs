//! TLS client for the tunnel. The router's certificate is self-signed and
//! regenerated on every restart, so instead of a CA we pin the SHA-256
//! fingerprint the router put in the invoke payload.

use std::io;
use std::sync::{Arc, Mutex};

use forkrail_common::{TUNNEL_SERVER_NAME, hex};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Kept across warm invocations so reconnects can resume the TLS session.
static CONFIG: Mutex<Option<(String, Arc<ClientConfig>)>> = Mutex::new(None);

pub async fn connect(cert_sha256: &str, tcp: TcpStream) -> io::Result<TlsStream<TcpStream>> {
    let config = config_for(cert_sha256)?;
    let name = ServerName::try_from(TUNNEL_SERVER_NAME).expect("valid server name");
    TlsConnector::from(config).connect(name, tcp).await
}

fn config_for(cert_sha256: &str) -> io::Result<Arc<ClientConfig>> {
    let mut cached = CONFIG.lock().unwrap();
    if let Some((pinned, config)) = cached.as_ref()
        && pinned == cert_sha256
    {
        return Ok(config.clone());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCert { sha256: cert_sha256.to_owned(), provider: provider.clone() });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let config = Arc::new(config);
    *cached = Some((cert_sha256.to_owned(), config.clone()));
    Ok(config)
}

#[derive(Debug)]
struct PinnedCert {
    sha256: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if hex(&Sha256::digest(end_entity)) == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
