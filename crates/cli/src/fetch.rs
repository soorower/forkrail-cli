//! Plain HTTPS requests: GETs and PUTs for presigned S3 URLs (how the builder reads its order and
//! source and writes its output without credentials of its own, builder.rs), and calls to GitHub's
//! API (github.rs). One connection per request; a request that fails on the way, or that is
//! answered 5xx or 429, is tried again a few times.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, HOST, LOCATION, USER_AGENT};
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// For a whole request, a big body included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const TRIES: u32 = 4;

pub struct Http {
    connector: TlsConnector,
}

/// What a URL answered other than 2xx.
#[derive(Debug)]
pub struct Status(pub u16, pub String);

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.0, self.1)
    }
}

impl std::error::Error for Status {}

impl Http {
    pub fn new() -> Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            bail!("no root certificates found on this machine");
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self { connector: TlsConnector::from(Arc::new(config)) })
    }

    /// The body of `url`, or `None` if it answers 404 (or 403, which S3 says for a missing key when
    /// the signer can't list the bucket).
    pub async fn get(&self, url: &str) -> Result<Option<Bytes>> {
        let response = match self.send(Method::GET, url, None, &[]).await {
            Ok(r) => r,
            Err(e) if e.downcast_ref::<Status>().is_some_and(|s| s.0 == 404 || s.0 == 403) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(response.into_body().collect().await.context("reading the body")?.to_bytes()))
    }

    /// `url`'s body into the file `path`, streamed; its size, or `None` if it answers 404 or 403
    /// (as `get`).
    pub async fn get_to_file(&self, url: &str, path: &Path) -> Result<Option<u64>> {
        let mut last = None;
        for attempt in 0..TRIES {
            if attempt > 0 {
                tokio::time::sleep(backoff(attempt)).await;
            }
            let response = match self.send(Method::GET, url, None, &[]).await {
                Ok(r) => r,
                Err(e) if e.downcast_ref::<Status>().is_some_and(|s| s.0 == 404 || s.0 == 403) => return Ok(None),
                Err(e) => return Err(e),
            };
            let mut file = tokio::fs::File::create(path).await.with_context(|| format!("creating {}", path.display()))?;
            let mut body = response.into_body();
            let mut size = 0u64;
            let copied = async {
                while let Some(frame) = body.frame().await {
                    if let Ok(data) = frame.context("reading the body")?.into_data() {
                        size += data.len() as u64;
                        file.write_all(&data).await?;
                    }
                }
                file.flush().await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match copied {
                Ok(()) => return Ok(Some(size)),
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("tried")).context("downloading")
    }

    /// GET `url` into the file `path`, following redirects; its size. `headers` go only to `url`
    /// itself: where it redirects to (GitHub sends an archive's download to another host, with a
    /// short-lived address) gets none, so a token never leaves the host it's for. More than `max`
    /// bytes is refused.
    pub async fn download(&self, url: &str, headers: &[(&str, &str)], path: &Path, max: u64) -> Result<u64> {
        self.download_watched(url, headers, path, max, &|_, _| {}).await
    }

    /// `download`, telling `progress` the bytes so far and the whole size (when the answer says it)
    /// as they come.
    pub async fn download_watched(&self, url: &str, headers: &[(&str, &str)], path: &Path, max: u64, progress: &(dyn Fn(u64, Option<u64>) + Send + Sync)) -> Result<u64> {
        let mut url = url.to_owned();
        let mut headers = headers;
        for _ in 0..5 {
            let response = tokio::time::timeout(REQUEST_TIMEOUT, self.once(Method::GET, &url, None, headers))
                .await
                .map_err(|_| anyhow::anyhow!("no answer in {} s", REQUEST_TIMEOUT.as_secs()))??;
            let status = response.status();
            if status.is_redirection() {
                let to = response.headers().get(LOCATION).and_then(|v| v.to_str().ok()).context("a redirect without an address")?;
                if !to.starts_with("https://") && !to.starts_with("http://") {
                    bail!("a redirect to {to:?}, which isn't a whole address");
                }
                url = to.to_owned();
                headers = &[];
                continue;
            }
            if !status.is_success() {
                let text = response.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                return Err(Status(status.as_u16(), s3_message(&text)).into());
            }
            let whole = response.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let mut file = tokio::fs::File::create(path).await.with_context(|| format!("creating {}", path.display()))?;
            let mut body = response.into_body();
            let mut size = 0u64;
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame.context("reading the body")?.into_data() {
                    size += data.len() as u64;
                    if size > max {
                        bail!("more than {} MB", max >> 20);
                    }
                    file.write_all(&data).await?;
                    progress(size, whole);
                }
            }
            file.flush().await?;
            return Ok(size);
        }
        bail!("too many redirects")
    }

    /// PUT the file `path` to `url`.
    pub async fn put_file(&self, url: &str, path: &Path) -> Result<()> {
        let body = Bytes::from(tokio::fs::read(path).await.with_context(|| format!("reading {}", path.display()))?);
        self.put(url, body).await
    }

    pub async fn put(&self, url: &str, body: Bytes) -> Result<()> {
        self.send(Method::PUT, url, Some(body), &[]).await.map(|_| ())
    }

    /// One request to an API, with its headers; the body of a 2xx answer, else the error (a
    /// [`Status`] when the API answered).
    pub async fn call(&self, method: Method, url: &str, headers: &[(&str, &str)], body: Option<Bytes>) -> Result<Bytes> {
        let response = self.send(method, url, body, headers).await?;
        Ok(response.into_body().collect().await.context("reading the body")?.to_bytes())
    }

    /// One request, sent once, and whatever it's answered: the status and the body. For an API whose
    /// refusals are worth reading, and whose requests mustn't be repeated unasked.
    pub async fn exchange(&self, method: Method, url: &str, headers: &[(&str, &str)], body: Option<Bytes>) -> Result<(u16, Bytes)> {
        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.once(method, url, body, headers))
            .await
            .map_err(|_| anyhow::anyhow!("no answer in {} s", REQUEST_TIMEOUT.as_secs()))??;
        let status = response.status().as_u16();
        Ok((status, response.into_body().collect().await.context("reading the body")?.to_bytes()))
    }

    /// One request, tried again on a failure that may pass; a 2xx response, else the error.
    async fn send(&self, method: Method, url: &str, body: Option<Bytes>, headers: &[(&str, &str)]) -> Result<Response<Incoming>> {
        let mut last = None;
        for attempt in 0..TRIES {
            if attempt > 0 {
                tokio::time::sleep(backoff(attempt)).await;
            }
            let once = tokio::time::timeout(REQUEST_TIMEOUT, self.once(method.clone(), url, body.clone(), headers)).await;
            match once {
                Ok(Ok(response)) if response.status().is_success() => return Ok(response),
                Ok(Ok(response)) => {
                    let status = response.status().as_u16();
                    let text = response.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                    let error = Status(status, s3_message(&text));
                    if status < 500 && status != 429 && status != 408 {
                        return Err(error.into());
                    }
                    last = Some(anyhow::Error::from(error));
                }
                Ok(Err(e)) => last = Some(e),
                Err(_) => last = Some(anyhow::anyhow!("no answer in {} s", REQUEST_TIMEOUT.as_secs())),
            }
        }
        Err(last.expect("tried"))
    }

    async fn once(&self, method: Method, url: &str, body: Option<Bytes>, headers: &[(&str, &str)]) -> Result<Response<Incoming>> {
        let uri: hyper::Uri = url.parse().context("not a URL")?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => bail!("only http and https URLs"),
        };
        let host = uri.host().context("no host in the URL")?.to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        let authority = if (https && port == 443) || (!https && port == 80) { host.clone() } else { format!("{host}:{port}") };
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, authority)
            .header(USER_AGENT, concat!("forkrail/", env!("CARGO_PKG_VERSION")));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port)))
            .await
            .context("connecting timed out")?
            .with_context(|| format!("connecting to {host}:{port}"))?;
        tcp.set_nodelay(true)?;
        macro_rules! exchange {
            ($io:expr) => {{
                let (mut sender, connection) = hyper::client::conn::http1::handshake($io).await?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                let request = match body {
                    Some(body) => request.header(CONTENT_LENGTH, body.len()).body(Full::new(body))?,
                    None => request.body(Full::new(Bytes::new()))?,
                };
                sender.send_request(request).await?
            }};
        }
        Ok(if https {
            let name = ServerName::try_from(host.clone()).context("the URL's host name")?;
            let tls = self.connector.connect(name, tcp).await.context("TLS")?;
            exchange!(TokioIo::new(tls))
        } else {
            exchange!(TokioIo::new(tcp))
        })
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(4)))
}

/// S3's error code and message from its XML, or the start of whatever came back.
fn s3_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let between = |tag: &str| {
        let open = format!("<{tag}>");
        let start = text.find(&open)? + open.len();
        let end = text[start..].find(&format!("</{tag}>"))? + start;
        Some(text[start..end].to_owned())
    };
    match (between("Code"), between("Message")) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        _ => text.chars().take(200).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_errors_read() {
        let xml = b"<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>";
        assert_eq!(s3_message(xml), "AccessDenied: Request has expired");
        assert_eq!(s3_message(b"plain"), "plain");
    }
}
