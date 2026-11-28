use std::sync::Arc;

use forkrail_common::{TUNNEL_SERVER_NAME, hex};
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

use crate::BoxError;

pub struct TunnelTls {
    pub acceptor: TlsAcceptor,
    /// Workers pin this instead of trusting a CA.
    pub cert_sha256: String,
}

/// Fresh self-signed certificate for the tunnel listener, generated at startup.
pub fn self_signed() -> Result<TunnelTls, BoxError> {
    let certified = rcgen::generate_simple_self_signed(vec![TUNNEL_SERVER_NAME.to_owned()])?;
    let cert = certified.cert.der().clone();
    let cert_sha256 = hex(&Sha256::digest(&cert));
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    Ok(TunnelTls { acceptor: TlsAcceptor::from(Arc::new(config)), cert_sha256 })
}
