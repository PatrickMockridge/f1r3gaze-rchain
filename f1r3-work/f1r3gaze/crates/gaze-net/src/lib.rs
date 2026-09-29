//! `gaze-net` — networking for F1R3Gaze (spec §8.2, WP C1).
//!
//! * [`Http`]: a blocking HTTP engine (rustls, no implicit redirects, size and
//!   time limits) run on a small worker [`Pool`].
//! * [`Schemes`]: handlers for the browser's own schemes — `f1r3h://` (content
//!   by hash), `f1r3://` (sites on the shard), `gaze://` (built-in pages) and
//!   `data:` — supplied by the shell.
//! * [`GazeNetProvider`]: Blitz's `NetProvider` for sub-resources, over both.
//!   Content-addressed URLs are verified before Blitz ever sees the bytes.
//! * [`fetch_request`]: the `net` capability's protocol, parsing a page's
//!   request term and building the reply term.

#![forbid(unsafe_code)]

use blitz_traits::net::{Body, Bytes, NetHandler, NetProvider, NetWaker, Request};
use k1ndl1ng_norm::hash::blake2b_256;
use k1ndl1ng_norm::{CollKind, Norm};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MAX_BODY: u64 = 64 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 10;
pub const USER_AGENT: &str = concat!("F1R3Gaze/", env!("CARGO_PKG_VERSION"));

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// BLAKE2b-256 of `b`, the hash every content address in F1R3Gaze uses.
pub fn digest(b: &[u8]) -> [u8; 32] {
    blake2b_256(b).0
}

/// `blake2b-256:<hex>`, the form of `integrity` attributes and replies.
pub fn integrity_of(b: &[u8]) -> String {
    format!("blake2b-256:{}", hex(&digest(b)))
}

/// Check `b` against an `integrity` string.
pub fn verify_integrity(b: &[u8], integrity: &str) -> bool {
    integrity.trim().eq_ignore_ascii_case(&integrity_of(b))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    Url(String),
    Status(u16),
    Transport(String),
    TooLarge,
    Integrity(String),
    NotFound(String),
    Refused(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Url(u) => write!(f, "bad URL {u}"),
            NetError::Status(s) => write!(f, "HTTP {s}"),
            NetError::Transport(e) => write!(f, "network error: {e}"),
            NetError::TooLarge => write!(f, "response too large"),
            NetError::Integrity(u) => write!(f, "integrity check failed for {u}"),
            NetError::NotFound(u) => write!(f, "not found: {u}"),
            NetError::Refused(u) => write!(f, "refused: {u}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct HttpResponse {
    /// The URL that answered (after redirects, if any were followed).
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(x, _)| x.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
    pub fn redirect_target(&self) -> Option<String> {
        if !(300..400).contains(&self.status) {
            return None;
        }
        let loc = self.header("location")?;
        url::Url::parse(&self.url).ok()?.join(loc).ok().map(|u| u.to_string())
    }
}

/// The HTTP engine. Redirects are never followed implicitly: the caller
/// decides, so a capability can re-check every hop against its allow-list.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

impl Http {
    pub fn new() -> Http {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirects(0)
            .user_agent(USER_AGENT)
            .build();
        Http { agent }
    }

    /// One exchange. Non-2xx statuses are responses, not errors.
    pub fn send(&self, r: &HttpRequest) -> Result<HttpResponse, NetError> {
        let method = if r.method.is_empty() { "GET" } else { r.method.as_str() };
        let mut req = self.agent.request(method, &r.url);
        for (k, v) in &r.headers {
            req = req.set(k, v);
        }
        let res = if r.body.is_empty() && matches!(method, "GET" | "HEAD" | "DELETE") {
            req.call()
        } else {
            req.send_bytes(&r.body)
        };
        let resp = match res {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(ureq::Error::Transport(t)) => return Err(NetError::Transport(t.to_string())),
        };
        let status = resp.status();
        let headers = resp
            .headers_names()
            .into_iter()
            .filter_map(|n| resp.header(&n).map(|v| (n.to_ascii_lowercase(), v.to_string())))
            .collect();
        let url = resp.get_url().to_string();
        let mut body = Vec::new();
        resp.into_reader()
            .take(MAX_BODY + 1)
            .read_to_end(&mut body)
            .map_err(|e| NetError::Transport(e.to_string()))?;
        if body.len() as u64 > MAX_BODY {
            return Err(NetError::TooLarge);
        }
        Ok(HttpResponse {
            url,
            status,
            headers,
            body,
        })
    }

    /// Follow redirects, asking `allow` before each hop.
    pub fn send_following(
        &self,
        r: &HttpRequest,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<HttpResponse, NetError> {
        let mut cur = r.clone();
        for _ in 0..=MAX_REDIRECTS {
            if !allow(&cur.url) {
                return Err(NetError::Refused(cur.url));
            }
            let resp = self.send(&cur)?;
            match resp.redirect_target() {
                Some(next) => {
                    // 303, and 301/302 after a POST, become GET.
                    if resp.status == 303 || (cur.method != "GET" && matches!(resp.status, 301 | 302)) {
                        cur.method = "GET".into();
                        cur.body.clear();
                    }
                    cur.url = next;
                }
                None => return Ok(resp),
            }
        }
        Err(NetError::Transport("too many redirects".into()))
    }
}

/// A fixed set of worker threads for blocking I/O.
#[derive(Clone)]
pub struct Pool {
    tx: Sender<Box<dyn FnOnce() + Send>>,
}

impl Pool {
    pub fn new(threads: usize) -> Pool {
        let (tx, rx) = channel::<Box<dyn FnOnce() + Send>>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads.max(1) {
            let rx = Arc::clone(&rx);
            let _ = std::thread::Builder::new().name(format!("gaze-net-{i}")).spawn(move || {
                loop {
                    let job = match rx.lock() {
                        Ok(g) => g.recv(),
                        Err(_) => return,
                    };
                    match job {
                        Ok(f) => f(),
                        Err(_) => return,
                    }
                }
            });
        }
        Pool { tx }
    }

    pub fn spawn(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.tx.send(Box::new(f));
    }
}

/// A handler for a non-HTTP scheme. Called on a worker thread.
pub trait SchemeHandler: Send + Sync + 'static {
    fn get(&self, url: &str) -> Result<Vec<u8>, NetError>;
}

impl<F: Fn(&str) -> Result<Vec<u8>, NetError> + Send + Sync + 'static> SchemeHandler for F {
    fn get(&self, url: &str) -> Result<Vec<u8>, NetError> {
        self(url)
    }
}

/// Scheme name to handler.
#[derive(Clone, Default)]
pub struct Schemes(Arc<Mutex<BTreeMap<String, Arc<dyn SchemeHandler>>>>);

impl Schemes {
    pub fn register(&self, scheme: &str, h: Arc<dyn SchemeHandler>) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(scheme.to_ascii_lowercase(), h);
        }
    }
    pub fn get(&self, scheme: &str) -> Option<Arc<dyn SchemeHandler>> {
        self.0.lock().ok()?.get(&scheme.to_ascii_lowercase()).cloned()
    }
}

/// The hash a content-addressed URL names: `f1r3h://blake2b-256/<hex>`.
pub fn content_hash(url: &str) -> Option<[u8; 32]> {
    let rest = url.strip_prefix("f1r3h://")?;
    let hexpart = rest.strip_prefix("blake2b-256/").unwrap_or(rest);
    let hexpart = hexpart.split(['?', '#', '/']).next()?;
    let v = unhex(hexpart)?;
    v.try_into().ok()
}

/// Decode a `data:` URL (base64 or percent-encoded).
pub fn data_url(url: &str) -> Result<Vec<u8>, NetError> {
    let rest = url.strip_prefix("data:").ok_or_else(|| NetError::Url(url.into()))?;
    let (meta, payload) = rest.split_once(',').ok_or_else(|| NetError::Url(url.into()))?;
    if meta.ends_with(";base64") {
        base64_decode(payload).ok_or_else(|| NetError::Url(url.into()))
    } else {
        Ok(percent_decode(payload))
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let clean: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let n = chunk.len();
        if n < 2 {
            return None;
        }
        out.push((acc >> 16) as u8);
        if n > 2 {
            out.push((acc >> 8) as u8);
        }
        if n > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// Fetch any URL the browser understands, following HTTP redirects.
/// Content-addressed URLs are verified here.
pub fn fetch_url(http: &Http, schemes: &Schemes, url: &str) -> Result<(String, Vec<u8>), NetError> {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => {
            let r = http.send_following(
                &HttpRequest {
                    url: url.into(),
                    method: "GET".into(),
                    ..Default::default()
                },
                &|_| true,
            )?;
            if !(200..300).contains(&r.status) {
                return Err(NetError::Status(r.status));
            }
            Ok((r.url, r.body))
        }
        "data" => Ok((url.into(), data_url(url)?)),
        other => {
            let h = schemes.get(other).ok_or_else(|| NetError::Url(url.into()))?;
            let bytes = h.get(url)?;
            if let Some(want) = content_hash(url) {
                if digest(&bytes) != want {
                    return Err(NetError::Integrity(url.into()));
                }
            }
            Ok((url.into(), bytes))
        }
    }
}

/// Blitz's sub-resource loader (stylesheets, images, fonts, iframes) over
/// [`fetch_url`], on the pool.
pub struct GazeNetProvider {
    pub http: Http,
    pub schemes: Schemes,
    pool: Pool,
    waker: Option<Arc<dyn NetWaker>>,
}

impl GazeNetProvider {
    pub fn new(http: Http, schemes: Schemes, pool: Pool, waker: Option<Arc<dyn NetWaker>>) -> GazeNetProvider {
        GazeNetProvider {
            http,
            schemes,
            pool,
            waker,
        }
    }
}

impl NetProvider for GazeNetProvider {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let (http, schemes, waker) = (self.http.clone(), self.schemes.clone(), self.waker.clone());
        self.pool.spawn(move || {
            if request.signal.as_ref().is_some_and(|s| s.aborted()) {
                return;
            }
            let url = request.url.to_string();
            let result = if request.method == blitz_traits::net::Method::GET {
                fetch_url(&http, &schemes, &url)
            } else {
                let body = match &request.body {
                    Body::Bytes(b) => b.to_vec(),
                    _ => Vec::new(),
                };
                http.send_following(
                    &HttpRequest {
                        url: url.clone(),
                        method: request.method.to_string(),
                        headers: request.content_type.iter().map(|c| ("content-type".into(), c.clone())).collect(),
                        body,
                    },
                    &|_| true,
                )
                .map(|r| (r.url, r.body))
            };
            if let Ok((resolved, bytes)) = result {
                handler.bytes(resolved, Bytes::from(bytes));
                if let Some(w) = &waker {
                    w.wake(doc_id);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// The `net` capability's protocol.

/// A page's `("fetch", {...}, ret)` request, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageFetch {
    pub request: HttpRequest,
    pub integrity: Option<String>,
}

/// Parse `("fetch", {"url", "method", "headers", "body", "integrity"}, ret)`.
/// `headers` is a map of strings; `body` is a string or bytes.
pub fn fetch_request(args: &[Norm]) -> Result<PageFetch, &'static str> {
    if args.first().and_then(|v| v.as_str()) != Some("fetch") {
        return Err("verb");
    }
    let m = args.get(1).ok_or("type")?;
    let url = m.map_get("url").and_then(|v| v.as_str()).ok_or("type")?.to_string();
    let method = m.map_get("method").and_then(|v| v.as_str()).unwrap_or("GET").to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "PATCH") {
        return Err("type");
    }
    let mut headers = Vec::new();
    if let Some(h) = m.map_get("headers") {
        for kv in h.as_coll(CollKind::Map).ok_or("type")?.chunks(2) {
            let (k, v) = (kv[0].as_str().ok_or("type")?, kv[1].as_str().ok_or("type")?);
            // The page may not set what identifies the browser or the user.
            if matches!(k.to_ascii_lowercase().as_str(), "cookie" | "host" | "user-agent" | "authorization") {
                continue;
            }
            headers.push((k.to_string(), v.to_string()));
        }
    }
    let body = match m.map_get("body") {
        None => Vec::new(),
        Some(b) => match (b.as_str(), b.as_lit()) {
            (Some(s), _) => s.as_bytes().to_vec(),
            (None, Some(k1ndl1ng_norm::Lit::Bytes(x))) => x.to_vec(),
            _ if b.is_nil() => Vec::new(),
            _ => return Err("type"),
        },
    };
    Ok(PageFetch {
        request: HttpRequest {
            url,
            method,
            headers,
            body,
        },
        integrity: m.map_get("integrity").and_then(|v| v.as_str()).map(str::to_string),
    })
}

/// The reply term: `("ok", {"status", "headers", "body", "digest"})`, or
/// `("err", "integrity", url)` when a requested integrity does not match.
pub fn fetch_reply(f: &PageFetch, r: Result<HttpResponse, NetError>) -> Norm {
    let err = |code: &str, d: &str| Norm::tuple(vec![Norm::str("err"), Norm::str(code), Norm::str(d)]);
    match r {
        Err(NetError::Refused(u)) => err("denied", &u),
        Err(NetError::TooLarge) => err("quota", &f.request.url),
        Err(e) => err("net", &e.to_string()),
        Ok(r) => {
            if let Some(i) = &f.integrity {
                if !verify_integrity(&r.body, i) {
                    return err("integrity", &f.request.url);
                }
            }
            let headers = Norm::map(r.headers.iter().map(|(k, v)| (Norm::str(k), Norm::str(v))).collect());
            let body = match std::str::from_utf8(&r.body) {
                Ok(s) if r.header("content-type").is_some_and(|c| c.starts_with("text/") || c.contains("json")) => {
                    Norm::str(s)
                }
                _ => Norm::bytes(&r.body),
            };
            Norm::tuple(vec![
                Norm::str("ok"),
                Norm::map(vec![
                    (Norm::str("status"), Norm::int(r.status as i64)),
                    (Norm::str("headers"), headers),
                    (Norm::str("body"), body),
                    (Norm::str("digest"), Norm::str(&integrity_of(&r.body))),
                ]),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hashes_and_integrity() {
        let d = digest(b"hello");
        let url = format!("f1r3h://blake2b-256/{}", hex(&d));
        assert_eq!(content_hash(&url), Some(d));
        assert!(verify_integrity(b"hello", &integrity_of(b"hello")));
        assert!(!verify_integrity(b"hellO", &integrity_of(b"hello")));
    }

    #[test]
    fn data_urls() {
        assert_eq!(data_url("data:text/plain,a%20b").unwrap(), b"a b");
        assert_eq!(data_url("data:text/plain;base64,aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn scheme_handlers_are_verified() {
        let schemes = Schemes::default();
        schemes.register("f1r3h", Arc::new(|_u: &str| Ok(b"tampered".to_vec())));
        let url = format!("f1r3h://blake2b-256/{}", hex(&digest(b"original")));
        assert!(matches!(fetch_url(&Http::new(), &schemes, &url), Err(NetError::Integrity(_))));
        let good = format!("f1r3h://blake2b-256/{}", hex(&digest(b"tampered")));
        assert_eq!(fetch_url(&Http::new(), &schemes, &good).unwrap().1, b"tampered");
    }

    #[test]
    fn page_requests_parse_and_strip_identity_headers() {
        let req = Norm::map(vec![
            (Norm::str("url"), Norm::str("https://a.example/x")),
            (Norm::str("method"), Norm::str("post")),
            (
                Norm::str("headers"),
                Norm::map(vec![
                    (Norm::str("accept"), Norm::str("application/json")),
                    (Norm::str("cookie"), Norm::str("steal")),
                ]),
            ),
            (Norm::str("body"), Norm::str("{}")),
        ]);
        let f = fetch_request(&[Norm::str("fetch"), req]).unwrap();
        assert_eq!(f.request.method, "POST");
        assert_eq!(f.request.headers, vec![("accept".to_string(), "application/json".to_string())]);
        let ok = fetch_reply(
            &f,
            Ok(HttpResponse {
                url: f.request.url.clone(),
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: b"[1]".to_vec(),
            }),
        );
        assert_eq!(ok.as_coll(CollKind::Tuple).unwrap()[0].as_str(), Some("ok"));
        let mut g = f.clone();
        g.integrity = Some(integrity_of(b"other"));
        let bad = fetch_reply(
            &g,
            Ok(HttpResponse {
                body: b"[1]".to_vec(),
                ..Default::default()
            }),
        );
        assert_eq!(bad.as_coll(CollKind::Tuple).unwrap()[1].as_str(), Some("integrity"));
    }
}
