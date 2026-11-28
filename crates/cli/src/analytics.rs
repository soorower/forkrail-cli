//! The analytics store: ClickHouse on the analytics box (deploy/aws/analytics.sh), which Vector on
//! each router fills from the router's log. `logs`, `stats` and `usage` query it over HTTPS as the
//! read-only `reader` user. The box's address and certificate are in `state/analytics/` (the
//! certificate is self-signed, so it's pinned by its SHA-256), the password in SSM.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_sdk_s3::error::DisplayErrorContext;
use base64::Engine;
use bytes::Bytes;
use forkrail_common::hex;
use forkrail_common::install::{ANALYTICS_CERT_KEY as CERT_KEY, ANALYTICS_ENDPOINT_KEY as ENDPOINT_KEY, CLICKHOUSE_READER_PARAM as READER_PARAM};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;

use crate::deploy::Ctx;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Endpoint {
    public_ip: String,
    https_port: u16,
    #[serde(default = "reader")]
    reader_user: String,
    #[serde(default = "database")]
    database: String,
}

fn reader() -> String {
    "reader".into()
}

fn database() -> String {
    "forkrail".into()
}

pub struct Analytics {
    ip: IpAddr,
    port: u16,
    database: String,
    authorization: String,
    connector: TlsConnector,
    /// One keep-alive connection, for queries one after another (`logs --follow`).
    conn: Mutex<Option<SendRequest<Full<Bytes>>>>,
}

impl Analytics {
    pub async fn connect(ctx: &Ctx) -> Result<Self> {
        let endpoint: Endpoint = ctx
            .store
            .get_value(ENDPOINT_KEY)
            .await?
            .context("no analytics box yet: run deploy/aws/analytics.sh up")?;
        let pem = match ctx.store.s3.get_object().bucket(&ctx.store.bucket).key(CERT_KEY).send().await {
            Ok(out) => out.body.collect().await.context("reading the analytics box's certificate")?.into_bytes(),
            Err(e) => bail!("reading s3://{}/{CERT_KEY}: {}", ctx.store.bucket, DisplayErrorContext(&e)),
        };
        let der = pem_to_der(&String::from_utf8_lossy(&pem))?;
        let password = aws_sdk_ssm::Client::new(&ctx.config)
            .get_parameter()
            .name(READER_PARAM)
            .with_decryption(true)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("reading {READER_PARAM} from SSM: {}", aws_sdk_ssm::error::DisplayErrorContext(&e)))?
            .parameter
            .and_then(|p| p.value)
            .context("the reader's password is empty")?;
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{}:{password}", endpoint.reader_user));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedCert { sha256: hex(&Sha256::digest(&der)), provider: provider.clone() });
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(Self {
            ip: endpoint.public_ip.parse().context("the analytics box's address")?,
            port: endpoint.https_port,
            database: endpoint.database,
            authorization: format!("Basic {credentials}"),
            connector: TlsConnector::from(Arc::new(config)),
            conn: Mutex::new(None),
        })
    }

    /// Rows of `sql`, which reads `{name:Type}` placeholders from `params`.
    pub async fn query<T: DeserializeOwned>(&self, sql: &str, params: &[(&str, String)]) -> Result<Vec<T>> {
        let text = self.raw(sql, params).await?;
        text.lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).with_context(|| format!("reading a row: {line}")))
            .collect()
    }

    async fn raw(&self, sql: &str, params: &[(&str, String)]) -> Result<String> {
        let mut uri = format!(
            "/?database={}&default_format=JSONEachRow&output_format_json_quote_64bit_integers=0&output_format_json_quote_64bit_floats=0",
            encode(&self.database)
        );
        for (name, value) in params {
            uri.push_str(&format!("&param_{name}={}", encode(value)));
        }
        let mut conn = self.conn.lock().await;
        // A kept connection may have been closed by the server meanwhile: one fresh try then.
        for attempt in 0..2 {
            if conn.as_mut().is_none_or(|s| s.is_closed()) {
                *conn = Some(self.open().await?);
            }
            let sender = conn.as_mut().expect("just opened");
            let request = Request::post(&uri)
                .header(HOST, format!("{}:{}", self.ip, self.port))
                .header(AUTHORIZATION, &self.authorization)
                .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(Full::new(Bytes::from(sql.to_owned())))?;
            let sent = match sender.ready().await {
                Ok(()) => sender.send_request(request).await,
                Err(e) => Err(e),
            };
            let response = match sent {
                Ok(response) => response,
                Err(_) if attempt == 0 => {
                    *conn = None;
                    continue;
                }
                Err(e) => return Err(e).context("querying the analytics box"),
            };
            let status = response.status();
            let body = response.into_body().collect().await.context("reading the analytics box's answer")?.to_bytes();
            let text = String::from_utf8_lossy(&body).into_owned();
            if !status.is_success() {
                bail!("the analytics box answered {status}: {}", text.trim());
            }
            return Ok(text);
        }
        unreachable!("the second attempt returns")
    }

    async fn open(&self) -> Result<SendRequest<Full<Bytes>>> {
        let addr = (self.ip, self.port);
        let hint = "is this machine's address allowed? deploy/aws/analytics.sh allow-me";
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .with_context(|| format!("connecting to the analytics box at {}:{} timed out ({hint})", self.ip, self.port))?
            .with_context(|| format!("connecting to the analytics box at {}:{} ({hint})", self.ip, self.port))?;
        let tls = self.connector.connect(ServerName::IpAddress(self.ip.into()), tcp).await.context("TLS with the analytics box")?;
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(sender)
    }
}

/// A URL query value, percent-encoded.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END CERTIFICATE-----"))
        .collect();
    if body.is_empty() {
        bail!("no certificate in s3://…/{CERT_KEY}");
    }
    base64::engine::general_purpose::STANDARD.decode(body.trim()).context("decoding the analytics box's certificate")
}

/// Accepts the one certificate whose SHA-256 it was given. The box's address is pinned the same
/// way, so no name is checked.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(encode("atlas-v_1.2~"), "atlas-v_1.2~");
    }

    #[test]
    fn pem_bodies_decode() {
        let pem = "-----BEGIN CERTIFICATE-----\nAAEC\nAw==\n-----END CERTIFICATE-----\n";
        assert_eq!(pem_to_der(pem).unwrap(), vec![0, 1, 2, 3]);
        assert!(pem_to_der("nothing").is_err());
    }
}
