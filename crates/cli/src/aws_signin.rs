//! Signing in to AWS without the AWS CLI, for `forkrail install` on a computer that has no AWS
//! credentials yet: the same browser sign-in as `aws login` (AWS CLI 2.32 and later).
//!
//! It's OAuth 2.0 with PKCE against AWS Sign-In (`{region}.signin.aws.amazon.com`):
//! - on a computer with a browser, the browser goes to AWS's sign-in page and comes back to a port
//!   on 127.0.0.1 with a code (client `devtools/same-device`);
//! - on a server, the person opens the address on any device, signs in, and pastes back the code
//!   the page shows (client `devtools/cross-device`).
//!
//! The code is exchanged once for credentials that last 15 minutes and a refresh token, each
//! request proven with a key made for this sign-in (DPoP, RFC 9449, ES256). Both are saved where
//! `aws login` saves them: the tokens in `~/.aws/login/cache/<sha256 of the session>.json`, and
//! `login_session` in the profile. The AWS SDKs and the AWS CLI then refresh them as if `aws login`
//! had signed in.
//!
//! A session is refreshed in the region it began in, which the AWS CLI keeps as `signInRegion`.
//! The SDK for Rust refreshes in its client's region instead, which fails for an install in
//! another region; [`pinned`] gives a credentials provider that refreshes where the session began.

use std::convert::Infallible;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use aws_config::Region;
use aws_config::login::LoginCredentialsProvider;
use aws_config::provider_config::ProviderConfig;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_PAD_INDIFFERENT, URL_SAFE_NO_PAD};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::fetch::Http;

const SAME_DEVICE: &str = "arn:aws:signin:::devtools/same-device";
const CROSS_DEVICE: &str = "arn:aws:signin:::devtools/cross-device";
/// How long the installer waits for the person to sign in.
const WAIT: Duration = Duration::from_secs(10 * 60);
/// Settings that give a profile its credentials some other way: a profile with one isn't signed in to.
const OTHER_SOURCES: &[&str] =
    &["aws_access_key_id", "role_arn", "source_profile", "credential_source", "credential_process", "web_identity_token_file", "sso_session", "sso_start_url"];
/// What signing in as the root user needs, which a new account's owner meets first.
pub const ROOT_NEEDS_MFA: &str = "Signing in as the root user needs an MFA device on it first (IAM → Security credentials → Assign MFA device).";

/// How the person reaches AWS's sign-in page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// This computer's browser, which comes back to a port on 127.0.0.1.
    Browser,
    /// The same, opened by the person with the installer page's Sign in button rather than by the
    /// installer: nothing goes to AWS until they ask for it.
    Button,
    /// A browser on any device; the person pastes back the code the page shows.
    Paste,
}

#[derive(Debug, Clone)]
pub struct SignedIn {
    /// The AWS profile the sign-in is saved in.
    pub profile: String,
    pub account: String,
    /// Who signed in.
    pub arn: String,
}

/// Signs in to AWS and saves the sign-in in an AWS profile: `profile` if given, else AWS_PROFILE's,
/// else `default` when it holds no other credentials, else `forkrail-<account>`. `show` gets the
/// address to open and what to do there. Nothing is saved for an account in `refused`.
pub async fn sign_in(region: &str, profile: Option<&str>, how: How, refused: &[String], show: &dyn Fn(&str, &str)) -> Result<SignedIn> {
    if region.starts_with("cn-") || region.starts_with("us-gov-") {
        bail!("the installer signs in to AWS's standard regions only; sign in with the AWS CLI for {region}");
    }
    let named = profile.map(str::to_owned).or_else(|| std::env::var("AWS_PROFILE").ok().filter(|p| !p.trim().is_empty()));
    if let Some(name) = &named
        && let Some(source) = other_source(name)
    {
        bail!("AWS profile {name} gets its credentials from `{source}`, so the installer won't sign in to it: fix that profile, or name another with --profile");
    }

    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).map_err(|_| anyhow!("making a key for the sign-in"))?;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).map_err(|_| anyhow!("reading the sign-in's key"))?;
    let verifier = verifier();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = uuid4();
    let base = format!("https://{region}.signin.aws.amazon.com");

    let (client, redirect, code) = match how {
        How::Browser | How::Button => {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("opening a local port for AWS's answer")?;
            let port = listener.local_addr()?.port();
            let redirect = format!("http://127.0.0.1:{port}/oauth/callback");
            let mut answers = serve(listener, authorize_url(&base, SAME_DEVICE, &state, &challenge, &redirect), state.clone());
            // The browser starts here and is sent on to AWS: a short address works on every system.
            let local = format!("http://127.0.0.1:{port}/");
            let first = if how == How::Button {
                "Sign in to AWS with the account forkrail goes in. The button opens AWS's own sign-in page in your browser; forkrail gets access to the account only once you've signed in there"
            } else if crate::github::open_browser(&local) {
                "Sign in to AWS in the browser tab that just opened, with the account forkrail goes in"
            } else {
                "Open this address in your browser and sign in to AWS, with the account forkrail goes in"
            };
            show(&local, &format!("{first}. {ROOT_NEEDS_MFA}"));
            // Someone reading the page first gets longer to click.
            let wait = if how == How::Button { WAIT * 3 } else { WAIT };
            let code = tokio::time::timeout(wait, answers.recv())
                .await
                .map_err(|_| anyhow!("no sign-in in {} minutes; run the installer again to sign in", wait.as_secs() / 60))?
                .context("the local sign-in page stopped")?
                .map_err(|said| anyhow!("AWS's sign-in didn't finish: {said}"))?;
            (SAME_DEVICE, redirect, code)
        }
        How::Paste => {
            let redirect = format!("{base}/v1/sessions/confirmation");
            let url = authorize_url(&base, CROSS_DEVICE, &state, &challenge, &redirect);
            show(
                &url,
                &format!("Open this address in a browser on any device, sign in to AWS with the account forkrail goes in, then paste the code the page shows here. {ROOT_NEEDS_MFA}"),
            );
            // Let the address print before the prompt does.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let pasted = tokio::task::spawn_blocking(|| -> Result<String> {
                let mut out = std::io::stderr();
                write!(out, "Code from AWS's page: ")?;
                out.flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).context("reading the code")?;
                Ok(line)
            })
            .await??;
            (CROSS_DEVICE, redirect, pasted_code(&pasted, &state)?)
        }
    };

    let body = json!({"clientId": client, "grantType": "authorization_code", "code": code, "codeVerifier": verifier, "redirectUri": redirect});
    let token = exchange(&base, &key, &body).await?;
    let id_token = token["idToken"].as_str().context("AWS's sign-in answered without an id token")?;
    let arn = claims(id_token)?["sub"].as_str().context("AWS's sign-in didn't say who signed in")?.to_owned();
    let account = arn.split(':').nth(4).filter(|a| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit())).with_context(|| format!("no account in {arn}"))?.to_owned();
    if refused.iter().any(|r| r.trim() == account) {
        bail!("you signed in to account {account}, which the installer is told never to touch; sign in with another account");
    }

    let access = &token["accessToken"];
    let field = |name: &str| access[name].as_str().map(str::to_owned).with_context(|| format!("AWS's sign-in answered without {name}"));
    let expires_in = token["expiresIn"].as_u64().unwrap_or(900);
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(i64::try_from(expires_in).unwrap_or(900))).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let cache = json!({
        "accessToken": {
            "accessKeyId": field("accessKeyId")?,
            "secretAccessKey": field("secretAccessKey")?,
            "sessionToken": field("sessionToken")?,
            "accountId": account,
            "expiresAt": expires_at,
        },
        "tokenType": token["tokenType"].as_str().unwrap_or("urn:aws:params:oauth:token-type:access_token_sigv4"),
        "clientId": client,
        "refreshToken": token["refreshToken"].as_str().context("AWS's sign-in answered without a refresh token")?,
        "idToken": id_token,
        "dpopKey": sec1_pem(pkcs8.as_ref(), key.public_key().as_ref())?,
        "signInRegion": region,
    });

    let profile = match named {
        Some(name) => name,
        None if other_source("default").is_none() => "default".to_owned(),
        None => format!("forkrail-{account}"),
    };
    let cache_path = cache_file(&arn).context("neither HOME nor AWS_LOGIN_CACHE_DIRECTORY is set, so there's nowhere to keep the sign-in")?;
    write_private(&cache_path, &cache.to_string())?;
    let config_path = config_file().context("HOME isn't set, so there's no AWS config file to save the sign-in in")?;
    let text = match std::fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", config_path.display())),
    };
    write_keeping_mode(&config_path, &with_session(&text, &profile, &arn, region))?;
    Ok(SignedIn { profile, account, arn })
}

/// A credentials provider for `profile`'s sign-in (or AWS_PROFILE's, else `default`'s) that
/// refreshes in the region the sign-in began in. None when the profile has no sign-in, or
/// credentials come from elsewhere first; the SDK's own chain then decides.
pub fn pinned(profile: Option<&str>) -> Option<LoginCredentialsProvider> {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let name = profile.map(str::to_owned).or_else(|| std::env::var("AWS_PROFILE").ok().filter(|p| !p.trim().is_empty())).unwrap_or_else(|| "default".to_owned());
    if other_source(&name).is_some() {
        return None;
    }
    let config = std::fs::read_to_string(config_file()?).ok()?;
    let session = settings(&config, &name, true)?.into_iter().find(|(k, _)| k == "login_session")?.1;
    let cache: Value = serde_json::from_slice(&std::fs::read(cache_file(&session)?).ok()?).ok()?;
    // The SDK for Rust rewrites the file without signInRegion when it refreshes; the id token's
    // issuer names the region too.
    let region = cache["signInRegion"].as_str().map(str::to_owned).or_else(|| cache["idToken"].as_str().and_then(issuer_region))?;
    Some(LoginCredentialsProvider::builder(session).configure(&ProviderConfig::without_region().with_region(Some(Region::new(region)))).build())
}

fn authorize_url(base: &str, client: &str, state: &str, challenge: &str, redirect: &str) -> String {
    format!(
        "{base}/v1/authorize?response_type=code&client_id={}&state={state}&code_challenge_method=SHA-256&scope=openid&redirect_uri={}&code_challenge={challenge}",
        encode(client),
        encode(redirect)
    )
}

/// The code for the sign-in, sent once, with a retry on throttling or AWS's own errors.
async fn exchange(base: &str, key: &EcdsaKeyPair, body: &Value) -> Result<Value> {
    let http = Http::new()?;
    let url = format!("{base}/v1/token");
    let body = Bytes::from(body.to_string());
    let mut last = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let proof = dpop(key, &url)?;
        match http.exchange(Method::POST, &url, &[("content-type", "application/json"), ("dpop", &proof)], Some(body.clone())).await {
            Ok((200..=299, bytes)) => {
                let value: Value = serde_json::from_slice(&bytes).context("AWS's sign-in answered something that isn't JSON")?;
                return Ok(value.get("tokenOutput").cloned().unwrap_or(value));
            }
            Ok((status, bytes)) => {
                let said: Value = serde_json::from_slice(&bytes).unwrap_or_default();
                let code = said["error"].as_str().unwrap_or_default();
                let message = said["message"].as_str().map_or_else(|| String::from_utf8_lossy(&bytes).chars().take(300).collect(), str::to_owned);
                let text = format!("AWS's sign-in answered {status} {code}: {message}");
                if status == 429 || status >= 500 {
                    last = Some(anyhow!(text));
                    continue;
                }
                match code {
                    "AUTHCODE_EXPIRED" => bail!("the sign-in took too long and its code expired; run the installer again to sign in ({text})"),
                    "INSUFFICIENT_PERMISSIONS" => bail!("this AWS user may not sign in from a terminal: it needs permission for signin:CreateOAuth2Token ({text})"),
                    _ => bail!(text),
                }
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("AWS's sign-in didn't answer")))
}

/// A DPoP proof for one POST to `url`: a JWT signed with the sign-in's key, the public key in its header.
fn dpop(key: &EcdsaKeyPair, url: &str) -> Result<String> {
    let public = key.public_key().as_ref();
    ensure!(public.len() == 65 && public[0] == 4, "the sign-in's key isn't an uncompressed P-256 point");
    let header = format!(
        r#"{{"typ":"dpop+jwt","alg":"ES256","jwk":{{"kty":"EC","x":"{}","y":"{}","crv":"P-256"}}}}"#,
        URL_SAFE_NO_PAD.encode(&public[1..33]),
        URL_SAFE_NO_PAD.encode(&public[33..65])
    );
    let iat = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let payload = json!({"htm": "POST", "htu": url, "iat": iat, "jti": uuid4()}).to_string();
    let input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(payload));
    // r and s, 32 bytes each: the form JWTs take.
    let signature = key.sign(&SystemRandom::new(), input.as_bytes()).map_err(|_| anyhow!("signing the sign-in's proof"))?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())))
}

/// The key as the AWS CLI keeps it: SEC1 `EC PRIVATE KEY` PEM, with the curve and the public key.
/// ring makes PKCS#8, whose ECPrivateKey holds the 32-byte private key after `02 01 01 04 20`.
fn sec1_pem(pkcs8: &[u8], public: &[u8]) -> Result<String> {
    let at = pkcs8.windows(5).position(|w| w == [0x02, 0x01, 0x01, 0x04, 0x20]).context("the sign-in's key isn't in the expected form")? + 5;
    let private = pkcs8.get(at..at + 32).context("the sign-in's key is cut short")?;
    ensure!(public.len() == 65 && public[0] == 4, "the sign-in's key isn't an uncompressed P-256 point");
    let mut der = vec![0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20];
    der.extend_from_slice(private);
    // [0] prime256v1, [1] the public key as a BIT STRING.
    der.extend_from_slice(&[0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0xa1, 0x44, 0x03, 0x42, 0x00]);
    der.extend_from_slice(public);
    let encoded = STANDARD.encode(&der);
    let lines: Vec<&str> = encoded.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64 is ASCII")).collect();
    Ok(format!("-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n", lines.join("\n")))
}

/// A JWT's claims, unverified: AWS's own answer over TLS, read only for who signed in.
fn claims(jwt: &str) -> Result<Value> {
    let payload = jwt.split('.').nth(1).context("the id token isn't a JWT")?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).context("the id token isn't a JWT")?;
    serde_json::from_slice(&bytes).context("the id token isn't a JWT")
}

/// The region in an id token's issuer, `https://{region}.signin.aws.amazon.com/signin`.
fn issuer_region(jwt: &str) -> Option<String> {
    let claims = claims(jwt).ok()?;
    let host = claims["iss"].as_str()?.strip_prefix("https://")?.split('/').next()?;
    let region = host.strip_suffix(".signin.aws.amazon.com")?;
    (!region.is_empty() && region.contains('-') && !region.contains('.')).then(|| region.to_owned())
}

/// The code in what the person pasted: base64 of `code=…&state=…`, from this sign-in.
fn pasted_code(pasted: &str, state: &str) -> Result<String> {
    let compact: String = pasted.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = STANDARD_PAD_INDIFFERENT
        .decode(compact.as_bytes())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .context("that isn't the code AWS's page shows: copy the whole code and paste it again")?;
    let fields = query(&decoded);
    let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    match (get("code"), get("state")) {
        (Some(code), Some(theirs)) if theirs == state => Ok(code),
        (Some(_), Some(_)) => bail!("that code is from another sign-in; use the address the installer printed this time"),
        _ => bail!("that isn't the code AWS's page shows: copy the whole code and paste it again"),
    }
}

/// 64 characters for PKCE, from the ones RFC 7636 allows.
fn verifier() -> String {
    const ALLOWED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut out = String::with_capacity(64);
    while out.len() < 64 {
        let mut bytes = [0u8; 64];
        random(&mut bytes);
        // Only bytes below a multiple of the alphabet's size, so every character is as likely.
        let limit = (256 / ALLOWED.len() * ALLOWED.len()) as u8;
        out.extend(bytes.iter().filter(|b| **b < limit).map(|b| ALLOWED[usize::from(*b) % ALLOWED.len()] as char).take(64 - out.len()));
    }
    out
}

fn random(buf: &mut [u8]) {
    ring::rand::SecureRandom::fill(&SystemRandom::new(), buf).expect("OS random source unavailable");
}

/// A random (version 4) UUID.
fn uuid4() -> String {
    let mut b = [0u8; 16];
    random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = forkrail_common::hex(&b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn encode(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A query string's pairs, decoded as a form is (`+` is a space).
fn query(text: &str) -> Vec<(String, String)> {
    let decode = |s: &str| {
        let s = s.replace('+', " ");
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%'
                && let Some(v) = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    text.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or_else(|| (decode(p), String::new()), |(k, v)| (decode(k), decode(v)))).collect()
}

/// The page on 127.0.0.1 the browser starts at and comes back to: `/` sends it on to AWS, and
/// `/oauth/callback` takes AWS's answer (a code for this sign-in's `state`, or an error).
fn serve(listener: TcpListener, authorize: String, state: String) -> mpsc::Receiver<Result<String, String>> {
    let (answers, received) = mpsc::channel(1);
    let shared = Arc::new((authorize, state, answers));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let shared = shared.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let shared = shared.clone();
                    async move { Ok::<_, Infallible>(answer(&request, &shared.0, &shared.1, &shared.2).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    received
}

async fn answer(request: &Request<Incoming>, authorize: &str, state: &str, answers: &mpsc::Sender<Result<String, String>>) -> Response<Full<Bytes>> {
    let page = |status: StatusCode, title: &str, text: &str| {
        Response::builder()
            .status(status)
            .header("content-type", "text/html; charset=utf-8")
            .header("cache-control", "no-store")
            .body(Full::new(Bytes::from(PAGE.replace("{title}", title).replace("{text}", text))))
            .expect("a response")
    };
    if request.method() != Method::GET {
        return page(StatusCode::METHOD_NOT_ALLOWED, "Not here", "This page only answers the browser.");
    }
    match request.uri().path() {
        "/" => Response::builder().status(StatusCode::FOUND).header("location", authorize).header("cache-control", "no-store").body(Full::new(Bytes::new())).expect("a response"),
        "/oauth/callback" => {
            let fields = query(request.uri().query().unwrap_or_default());
            let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            if let Some(error) = get("error") {
                let said = get("error_description").map_or(error.clone(), |d| format!("{error}: {d}"));
                let _ = answers.send(Err(said)).await;
                return page(StatusCode::OK, "Sign-in didn't finish", "AWS didn't sign you in. The installer says what happened; run it again to try again.");
            }
            match (get("code"), get("state")) {
                (Some(code), Some(theirs)) if theirs == state => {
                    let _ = answers.send(Ok(code)).await;
                    page(StatusCode::OK, "Signed in", "You're signed in to AWS. Close this tab and go back to the installer.")
                }
                _ => page(StatusCode::BAD_REQUEST, "Not this sign-in", "This answer is for another sign-in. Use the address the installer printed this time."),
            }
        }
        _ => page(StatusCode::NOT_FOUND, "Not here", "Nothing here."),
    }
}

const PAGE: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title>
<style>:root{--bg:#0d1512;--text:#e6efe9;--muted:#8fa39a}@media (prefers-color-scheme: light){:root{--bg:#f6f8f5;--text:#14201b;--muted:#5d6f66}}
body{margin:0;background:var(--bg);color:var(--text);font:15px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}main{max-width:520px;margin:0 auto;padding:64px 16px}h1{font-size:22px;margin:0 0 8px}p{color:var(--muted)}</style>
</head><body><main><h1>{title}</h1><p>{text}</p></main></body></html>"#;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn config_file() -> Option<PathBuf> {
    std::env::var_os("AWS_CONFIG_FILE").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".aws").join("config")))
}

fn credentials_file() -> Option<PathBuf> {
    std::env::var_os("AWS_SHARED_CREDENTIALS_FILE").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".aws").join("credentials")))
}

/// Where `aws login` keeps a session's tokens.
fn cache_file(session: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("AWS_LOGIN_CACHE_DIRECTORY").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".aws").join("login").join("cache")))?;
    Some(dir.join(format!("{}.json", forkrail_common::hex(&Sha256::digest(session.trim().as_bytes())))))
}

/// The setting through which `profile` gets credentials other than a sign-in, if it has one.
fn other_source(profile: &str) -> Option<String> {
    let read = |path: Option<PathBuf>| path.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let config = settings(&read(config_file()), profile, true).unwrap_or_default();
    let credentials = settings(&read(credentials_file()), profile, false).unwrap_or_default();
    config.into_iter().chain(credentials).map(|(k, _)| k).find(|k| OTHER_SOURCES.contains(&k.as_str()))
}

/// A profile's own settings in an AWS config file (`[profile NAME]`, or `[default]`) or
/// credentials file (`[NAME]`); None without the section.
fn settings(text: &str, profile: &str, config: bool) -> Option<Vec<(String, String)>> {
    let mut lines = text.lines();
    lines.by_ref().find(|line| section(line, config).as_deref() == Some(profile))?;
    let mut out = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if section(line, config).is_some() || trimmed.starts_with('[') {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') || line.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            out.push((key.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Some(out)
}

/// The profile a section header names.
fn section(line: &str, config: bool) -> Option<String> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
    if !config {
        return Some(inner.to_owned());
    }
    if inner == "default" {
        return Some(inner.to_owned());
    }
    let rest = inner.strip_prefix("profile")?;
    rest.starts_with(char::is_whitespace).then(|| rest.trim().to_owned())
}

/// The config file's text with `profile` signed in to `session`: its `login_session` set, and its
/// region set if it had none. Everything else is kept as it was.
fn with_session(text: &str, profile: &str, session: &str, region: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|line| section(line, true).as_deref() == Some(profile)) else {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let header = if profile == "default" { "[default]".to_owned() } else { format!("[profile {profile}]") };
        out.push_str(&format!("{header}\nlogin_session = {session}\nregion = {region}\n"));
        return out;
    };
    let end = lines[start + 1..].iter().position(|line| line.trim().starts_with('[')).map_or(lines.len(), |i| start + 1 + i);
    let key = |line: &str| line.split_once('=').map(|(k, _)| k.trim().to_ascii_lowercase());
    let has_region = lines[start + 1..end].iter().any(|line| key(line).as_deref() == Some("region"));
    let mut out: Vec<String> = lines[..=start].iter().map(|l| (*l).to_owned()).collect();
    out.push(format!("login_session = {session}"));
    if !has_region {
        out.push(format!("region = {region}"));
    }
    out.extend(lines[start + 1..end].iter().filter(|line| key(line).as_deref() != Some("login_session")).map(|l| (*l).to_owned()));
    out.extend(lines[end..].iter().map(|l| (*l).to_owned()));
    out.join("\n") + "\n"
}

/// Writes a file only its owner can read.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        options.mode(0o600);
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(|| format!("protecting {}", path.display()))?;
        }
    }
    let mut file = options.open(path).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

/// Replaces a file whole, keeping its permissions (a new one only its owner can read).
fn write_keeping_mode(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("the AWS config file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.forkrail-tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("config")));
    write_private(&tmp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions()).with_context(|| format!("writing {}", path.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_in_a_profile_keeping_the_rest() {
        let text = "[default]\nregion = us-east-1\n\n[profile forkrail]\nlogin_session = arn:aws:iam::111111111111:root\nregion = us-east-1\n";
        let out = with_session(text, "default", "arn:aws:iam::222222222222:root", "us-east-2");
        assert_eq!(out, "[default]\nlogin_session = arn:aws:iam::222222222222:root\nregion = us-east-1\n\n[profile forkrail]\nlogin_session = arn:aws:iam::111111111111:root\nregion = us-east-1\n");

        let again = with_session(&out, "default", "arn:aws:iam::333333333333:root", "us-east-2");
        assert_eq!(settings(&again, "default", true).unwrap(), vec![("login_session".into(), "arn:aws:iam::333333333333:root".into()), ("region".into(), "us-east-1".into())]);
        assert_eq!(settings(&again, "forkrail", true).unwrap()[0].1, "arn:aws:iam::111111111111:root");

        let new = with_session("[profile x]\nregion = eu-west-1", "forkrail-222222222222", "arn:aws:iam::222222222222:root", "us-east-2");
        assert_eq!(new, "[profile x]\nregion = eu-west-1\n\n[profile forkrail-222222222222]\nlogin_session = arn:aws:iam::222222222222:root\nregion = us-east-2\n");
        assert_eq!(with_session("", "default", "s", "r"), "[default]\nlogin_session = s\nregion = r\n");
    }

    #[test]
    fn reads_profiles_the_way_the_aws_cli_writes_them() {
        let config = "[default]\nregion=us-east-1\n[profile  work ]\nsso_session = corp\n# note\n[sso-session corp]\nsso_region = us-east-1\n";
        assert_eq!(settings(config, "work", true).unwrap(), vec![("sso_session".into(), "corp".into())]);
        assert!(settings(config, "corp", true).is_none());
        assert!(settings("[work]\naws_access_key_id = AKIA\n", "work", false).unwrap().iter().any(|(k, _)| k == "aws_access_key_id"));
        assert!(settings("[profile work]\n", "work", false).is_none());
    }

    #[test]
    fn takes_only_this_sign_ins_pasted_code() {
        let pasted = STANDARD.encode("code=abc%2Bdef&state=s-1");
        assert_eq!(pasted_code(&format!("  {pasted}\n"), "s-1").unwrap(), "abc+def");
        assert!(pasted_code(&pasted, "s-2").is_err());
        assert!(pasted_code("not base64!", "s-1").is_err());
        assert_eq!(pasted_code(pasted.trim_end_matches('='), "s-1").unwrap(), "abc+def");
    }

    #[test]
    fn makes_a_proof_and_a_key_the_aws_cli_reads() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let pem = sec1_pem(pkcs8.as_ref(), key.public_key().as_ref()).unwrap();
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        let der = STANDARD.decode(body).unwrap();
        assert_eq!(der.len(), 121);
        assert_eq!(&der[der.len() - 65..], key.public_key().as_ref());

        let proof = dpop(&key, "https://us-east-2.signin.aws.amazon.com/v1/token").unwrap();
        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["typ"], "dpop+jwt");
        assert_eq!(header["jwk"]["crv"], "P-256");
        let claims = claims(&proof).unwrap();
        assert_eq!(claims["htm"], "POST");
        assert_eq!(claims["htu"], "https://us-east-2.signin.aws.amazon.com/v1/token");
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, key.public_key().as_ref());
        public.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).unwrap();
    }

    #[test]
    fn finds_the_sign_in_region_in_the_id_token() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"iss":"https://us-east-2.signin.aws.amazon.com/signin","sub":"arn:aws:iam::222222222222:root"}"#);
        assert_eq!(issuer_region(&format!("e30.{payload}.sig")).as_deref(), Some("us-east-2"));
        assert_eq!(issuer_region("e30.e30.sig"), None);
    }

    #[test]
    fn small_parts() {
        let v = verifier();
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)));
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_eq!(encode("http://127.0.0.1:5/oauth/callback"), "http%3A%2F%2F127.0.0.1%3A5%2Foauth%2Fcallback");
        assert_eq!(cache_file("arn:aws:iam::123456789012:root").unwrap().file_name().unwrap().to_str().unwrap().len(), 64 + 5);
    }
}
