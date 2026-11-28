//! The installer's page: on a computer with a browser, `forkrail install` asks its questions and
//! shows its progress on a page served from this machine (127.0.0.1, on a port of its own), while
//! the terminal prints the same. On a server over SSH, or with `--terminal`, it all stays in the
//! terminal.
//!
//! Only this browser can use the page: its address holds a random key, and the answers must come
//! with that key in a header, which a page from another site can't send to this one.
//!
//! The Mac app (deploy/macos) shows the page in a window of its own instead: it runs the installer
//! with `FORKRAIL_PAGE_IN_WINDOW=1`, which prints the page's address on stdout as
//! `FORKRAIL_PAGE_URL=<url>` rather than opening a browser.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::Event;
use super::checks::Verdict;

/// What the page asks for, with what it suggests.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Questions {
    /// The region suggested (or fixed), and the ones to pick from.
    pub region: String,
    pub regions: Vec<Region>,
    /// What the install costs a day, for the page to say.
    pub price: String,
    pub domain: String,
    pub email: String,
    /// Whether a Cloudflare token is needed (an earlier run may have stored it).
    pub token: bool,
    /// An earlier run stored one: a new one replaces it, and none keeps it.
    pub token_saved: bool,
    pub token_page: String,
    /// An earlier run started here, with this region and domain: changing either starts over.
    pub started: bool,
    /// The install has finished: its region and domain stay as they are.
    pub fixed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Region {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Answers {
    pub region: String,
    pub domain: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub token: String,
    /// The person saw that their new region or domain starts the install over, and went on.
    #[serde(default)]
    pub start_over: bool,
}

impl Answers {
    /// The domain as given, without a final dot, in lower case.
    pub fn domain(&self) -> String {
        self.domain.trim().trim_end_matches('.').to_ascii_lowercase()
    }

    /// Whether these answers move a started install to another region or domain.
    pub fn moves(&self, questions: &Questions) -> bool {
        questions.started && !questions.fixed && (self.region.trim() != questions.region || (!questions.domain.is_empty() && self.domain() != questions.domain))
    }
}

#[derive(Default)]
struct State {
    questions: Option<Questions>,
    answers: Option<oneshot::Sender<Answers>>,
    /// What was answered, without the token: for a page opened again.
    answered: Option<Value>,
    events: Vec<Value>,
    done: bool,
    /// The page has read the end.
    shown: bool,
}

/// The running page.
#[derive(Clone)]
pub struct Page {
    state: Arc<Mutex<State>>,
    pub url: String,
}

/// Set by the Mac app, which shows the page in its own window.
pub const IN_WINDOW_ENV: &str = "FORKRAIL_PAGE_IN_WINDOW";
/// The line the installer prints for the Mac app when the install has ended, either way
/// (deploy/macos/Forkrail/main.swift).
pub const ENDED_LINE: &str = "FORKRAIL_INSTALL_ENDED";
/// The line the app reads the page's address from.
pub const WINDOW_LINE: &str = "FORKRAIL_PAGE_URL=";

pub fn in_window() -> bool {
    std::env::var_os(IN_WINDOW_ENV).is_some_and(|v| v == "1")
}

/// Whether this machine has a browser for the page: a Mac or Windows, or Linux with a display,
/// and not a session over SSH.
pub fn desktop() -> bool {
    if in_window() {
        return true;
    }
    let ssh = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let display = cfg!(target_os = "macos") || cfg!(target_os = "windows") || std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    display && !ssh
}

impl Page {
    /// Serves the page and opens it in the browser; None if the browser couldn't be opened.
    pub async fn open() -> Option<Page> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
        let port = listener.local_addr().ok()?.port();
        let key = forkrail_common::random_hex(16);
        let page = Page { state: Arc::new(Mutex::new(State::default())), url: format!("http://127.0.0.1:{port}/?k={key}") };
        let state = page.state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (state, key) = (state.clone(), key.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let (state, key) = (state.clone(), key.clone());
                        async move { Ok::<_, Infallible>(answer(request, &state, &key).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        if in_window() {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{WINDOW_LINE}{}", page.url).and_then(|()| out.flush());
            return Some(page);
        }
        crate::github::open_browser(&page.url).then_some(page)
    }

    /// Asks the page's questions and waits for the answers (the person's go ahead, too).
    pub async fn ask(&self, questions: Questions) -> Option<Answers> {
        let (send, receive) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("page state");
            state.questions = Some(questions);
            state.answers = Some(send);
        }
        receive.await.ok()
    }

    pub fn event(&self, event: &Event) {
        let mut value = event_json(event);
        // When it happened, for the page's clocks.
        value["at"] = json!(super::now_ms());
        let mut state = self.state.lock().expect("page state");
        state.events.push(value);
    }

    /// The install ended (either way): the page stops asking for news after it has shown it. In
    /// the Mac app's window, the app hears it too: closing it now stops nothing.
    pub fn finish(&self) {
        self.state.lock().expect("page state").done = true;
        if in_window() {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{ENDED_LINE}").and_then(|()| out.flush());
        }
    }

    /// Waits until the page has read the end, or `longest`.
    pub async fn shown(&self, longest: std::time::Duration) {
        let until = tokio::time::Instant::now() + longest;
        while !self.state.lock().expect("page state").shown && tokio::time::Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

fn event_json(event: &Event) -> Value {
    match event {
        Event::Note { step, text } => json!({"kind": "note", "text": text, "step": step}),
        Event::Account { account, arn } => json!({"kind": "account", "account": account, "who": super::who(arn)}),
        Event::GitHubReady { name, url } => json!({"kind": "github", "name": name, "url": url}),
        Event::Check(check) => json!({
            "kind": "check", "name": check.name, "detail": check.detail, "fix": check.fix, "link": check.link,
            "verdict": match check.verdict { Verdict::Pass => "pass", Verdict::Warn => "warn", Verdict::Fail => "fail" },
        }),
        Event::Plan(steps) => json!({
            "kind": "plan",
            "steps": steps.iter().map(|(id, title)| json!({"id": id, "title": title, "about": super::steps::about(id), "est": super::steps::estimate(id), "needs": super::steps::needs(id)})).collect::<Vec<_>>(),
        }),
        Event::Started(id) => json!({"kind": "started", "id": id}),
        Event::Finished { id, skipped, secs } => json!({"kind": "finished", "id": id, "skipped": skipped, "secs": secs}),
        Event::Failed(p) => json!({"kind": "failed", "step": p.step, "action": p.action, "said": super::problem::brief(&p.said), "cause": p.cause, "fix": p.fix, "link": p.link}),
        Event::Open { url, why } => json!({"kind": "open", "url": url, "why": why}),
        Event::Installed { dashboard } => json!({"kind": "installed", "dashboard": dashboard}),
    }
}

async fn answer(request: Request<Incoming>, state: &Mutex<State>, key: &str) -> Response<Full<Bytes>> {
    let respond = |status: StatusCode, kind: &str, body: Bytes| {
        Response::builder()
            .status(status)
            .header("content-type", kind)
            .header("cache-control", "no-store")
            .header("x-frame-options", "DENY")
            .header("content-security-policy", "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'none'")
            .body(Full::new(body))
            .expect("a response")
    };
    let json_response = |value: Value| respond(StatusCode::OK, "application/json", Bytes::from(value.to_string()));
    let keyed = request.headers().get("x-forkrail-key").and_then(|v| v.to_str().ok()) == Some(key);
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/") => {
            let given = request.uri().query().unwrap_or_default().split('&').any(|pair| pair == format!("k={key}"));
            if given {
                respond(StatusCode::OK, "text/html; charset=utf-8", Bytes::from_static(HTML.as_bytes()))
            } else {
                respond(StatusCode::FORBIDDEN, "text/plain", Bytes::from_static(b"Open the address the installer printed."))
            }
        }
        (&Method::GET, "/state") if keyed => {
            let mut state = state.lock().expect("page state");
            state.shown |= state.done;
            let asking = state.answers.is_some();
            json_response(json!({"questions": state.questions, "asking": asking, "answered": state.answered, "events": state.events, "done": state.done, "window": in_window()}))
        }
        (&Method::POST, "/answers") if keyed => {
            let Ok(body) = request.into_body().collect().await else {
                return respond(StatusCode::BAD_REQUEST, "text/plain", Bytes::from_static(b"no body"));
            };
            let Ok(answers) = serde_json::from_slice::<Answers>(&body.to_bytes()) else {
                return respond(StatusCode::BAD_REQUEST, "text/plain", Bytes::from_static(b"not the page's answers"));
            };
            let questions = state.lock().expect("page state").questions.clone().unwrap_or_default();
            if let Err(said) = check_answers(&answers, &questions) {
                return respond(StatusCode::BAD_REQUEST, "text/plain", Bytes::from(said));
            }
            let mut held = state.lock().expect("page state");
            match held.answers.take() {
                Some(sender) => {
                    held.answered = Some(json!({"domain": answers.domain(), "region": answers.region.trim(), "email": answers.email.trim()}));
                    drop(held);
                    let _ = sender.send(answers);
                    json_response(json!({"ok": true}))
                }
                None => respond(StatusCode::CONFLICT, "text/plain", Bytes::from_static(b"the installer isn't asking anything now")),
            }
        }
        _ => respond(StatusCode::NOT_FOUND, "text/plain", Bytes::from_static(b"not found")),
    }
}

/// What's wrong with the page's answers, in words for the page: a region from the list, a domain,
/// an email that looks like one. A finished install's region and domain aren't the page's to
/// change, and a started one's change only with the person's go ahead to start over.
fn check_answers(answers: &Answers, questions: &Questions) -> Result<(), String> {
    if !questions.fixed {
        if !super::REGIONS.iter().any(|(code, _)| *code == answers.region.trim()) {
            return Err("Pick a region from the list.".into());
        }
        if !super::valid_domain(&answers.domain()) {
            return Err("That isn't a domain: give one like example.com, without https:// or a path.".into());
        }
        if answers.moves(questions) && !answers.start_over {
            return Err("A new domain or region starts the install over. Confirm that first.".into());
        }
    }
    let token = answers.token.trim();
    if !token.is_empty() && (token.len() < 20 || token.contains(char::is_whitespace)) {
        return Err("That doesn't look like a Cloudflare API token. Copy the whole token from Cloudflare.".into());
    }
    let email = answers.email.trim();
    if !email.is_empty() && !(email.contains('@') && email.rsplit('@').next().is_some_and(|d| d.contains('.')) && !email.contains(char::is_whitespace)) {
        return Err("That email doesn't look right. Leave it empty for no alerts.".into());
    }
    Ok(())
}

const HTML: &str = include_str!("page.html");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::problem::Problem;

    #[test]
    fn events_become_what_the_page_draws() {
        let failed = event_json(&Event::Failed(Problem {
            step: "Start the router".into(),
            action: "setting it up".into(),
            said: "boom".into(),
            cause: "c".into(),
            fix: "f".into(),
            link: None,
        }));
        assert_eq!(failed["kind"], "failed");
        assert_eq!(failed["step"], "Start the router");
        let plan = event_json(&Event::Plan(vec![("router", "The router")]));
        assert!(plan["steps"][0]["about"].as_str().is_some_and(|a| a.contains("visit")));
        assert!(plan["steps"][0]["est"].as_u64().is_some_and(|s| s > 0));
        let note = event_json(&Event::Note { step: Some("protection"), text: "a line".into() });
        assert_eq!((note["kind"].as_str(), note["step"].as_str()), (Some("note"), Some("protection")));
        let finished = event_json(&Event::Finished { id: "bucket", skipped: true, secs: 0 });
        assert_eq!((finished["id"].as_str(), finished["skipped"].as_bool()), (Some("bucket"), Some(true)));
    }

    #[test]
    fn a_started_install_moves_only_when_the_person_says_so() {
        let questions = Questions { region: "us-east-1".into(), domain: "example.com".into(), started: true, ..Default::default() };
        let answers = |region: &str, domain: &str, start_over: bool| Answers { region: region.into(), domain: domain.into(), email: String::new(), token: String::new(), start_over };
        assert!(!answers("us-east-1", "Example.com.", false).moves(&questions));
        assert!(check_answers(&answers("us-east-1", "example.com", false), &questions).is_ok());
        assert!(answers("us-west-2", "example.com", false).moves(&questions));
        assert!(check_answers(&answers("us-west-2", "example.com", false), &questions).is_err());
        assert!(check_answers(&answers("us-east-1", "other.com", true), &questions).is_ok());
        // A finished install keeps both, whatever the page sends.
        let finished = Questions { fixed: true, ..questions.clone() };
        assert!(!answers("us-west-2", "other.com", false).moves(&finished));
        // Only a bucket so far: no domain to change.
        let bucket = Questions { domain: String::new(), ..questions };
        assert!(!answers("us-east-1", "new.com", false).moves(&bucket));
        let short = Answers { token: "abc".into(), ..answers("us-east-1", "example.com", false) };
        assert!(check_answers(&short, &bucket).is_err());
    }

    #[test]
    fn the_page_never_puts_text_in_unescaped() {
        // Every value from an event goes through esc() before it reaches innerHTML.
        for line in HTML.lines().filter(|l| l.contains("e.") && l.contains("innerHTML")) {
            assert!(!line.contains("+ e."), "{line}");
        }
    }
}
