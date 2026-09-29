//! What every tab of a profile shares, and the per-tab services.

use crate::pages;
use crate::profile::{Settings, user_id};
use gaze_blob::{Blobs, ContentCache};
use gaze_broker::{Broker, FileGrants, NAV, NET, Refusal, SHARD, STORE, ShardClass, Site};
use gaze_dom_blitz::{Delivery, Services, WakeHandle};
use gaze_exec::{CapRequest, Class};
use gaze_net::{Http, NetError, Pool, Schemes, content_hash, fetch_reply, fetch_request, unhex};
use gaze_shard::{Bridge, DriveSource, FileKeystore, Keystore, ShardOut, ShardService, SiteAddr, SiteManifest};
use gaze_store::{OriginStore, path_for};
use k1ndl1ng_norm::{Name, Node, Norm};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Engine {
    pub dir: PathBuf,
    pub settings: Settings,
    pub http: Http,
    pub pool: Pool,
    pub schemes: Schemes,
    pub blobs: Arc<Blobs>,
    pub bridge: Arc<Bridge>,
    pub broker: RefCell<Broker<FileGrants>>,
    pub sites: Arc<SiteCache>,
}

/// Resolved site manifests, briefly cached (sub-resources of one page load
/// should not each resolve the site).
pub struct SiteCache {
    bridge: Arc<Bridge>,
    map: Mutex<BTreeMap<String, (Instant, SiteManifest)>>,
}

impl SiteCache {
    pub fn manifest(&self, a: &SiteAddr) -> Result<SiteManifest, String> {
        let key = a.registry_uri();
        if let Ok(m) = self.map.lock() {
            if let Some((t, m)) = m.get(&key) {
                if t.elapsed() < Duration::from_secs(15) {
                    return Ok(m.clone());
                }
            }
        }
        let (_, m) = self.bridge.resolve_site(a)?;
        if let Ok(mut map) = self.map.lock() {
            map.insert(key, (Instant::now(), m.clone()));
        }
        Ok(m)
    }
    pub fn file(&self, url: &str) -> Result<(String, Vec<u8>), String> {
        let a = SiteAddr::parse(url).ok_or_else(|| format!("bad f1r3 address {url}"))?;
        let m = self.manifest(&a)?;
        self.bridge.site_file(&m, &a.path)
    }
}

impl Engine {
    pub fn new(dir: PathBuf) -> Rc<Engine> {
        let settings = Settings::load(&dir);
        let http = Http::new();
        let pool = Pool::new(6);
        let blobs = Arc::new(Blobs::new(ContentCache::new(dir.join("cache"), settings.cache_bytes), http.clone()));
        for m in &settings.mirrors {
            blobs.add_source(Arc::new(gaze_blob::MirrorSource::new(m, http.clone())));
        }
        let mut shard_cfg = settings.shard.clone();
        shard_cfg.user = user_id(&dir);
        #[cfg(feature = "os-keyring")]
        let keys: Arc<dyn Keystore> = if cfg!(any(target_os = "macos", windows)) {
            Arc::new(gaze_shard::keys::OsKeystore::new("F1R3Gaze"))
        } else {
            Arc::new(FileKeystore::new(dir.join("keys")))
        };
        #[cfg(not(feature = "os-keyring"))]
        let keys: Arc<dyn Keystore> = Arc::new(FileKeystore::new(dir.join("keys")));
        let bridge = Bridge::new(shard_cfg, http.clone(), pool.clone(), keys, Arc::clone(&blobs));
        blobs.add_source(Arc::new(DriveSource {
            bridge: Arc::clone(&bridge),
            root: "/gaze-blob/".into(),
        }));
        let sites = Arc::new(SiteCache {
            bridge: Arc::clone(&bridge),
            map: Mutex::new(BTreeMap::new()),
        });
        let schemes = Schemes::default();
        {
            let b = Arc::clone(&blobs);
            schemes.register(
                "f1r3h",
                Arc::new(move |u: &str| {
                    let h = content_hash(u).ok_or_else(|| NetError::Url(u.into()))?;
                    b.get(&h, &[]).map_err(NetError::NotFound)
                }),
            );
            let s = Arc::clone(&sites);
            schemes.register("f1r3", Arc::new(move |u: &str| s.file(u).map(|(_, b)| b).map_err(NetError::NotFound)));
            schemes.register(
                "gaze",
                Arc::new(|u: &str| pages::builtin(u).map(|s| s.into_bytes()).ok_or_else(|| NetError::NotFound(u.into()))),
            );
            schemes.register(
                "file",
                Arc::new(|u: &str| {
                    let p = url::Url::parse(u).ok().and_then(|x| x.to_file_path().ok()).ok_or_else(|| NetError::Url(u.into()))?;
                    std::fs::read(p).map_err(|e| NetError::NotFound(e.to_string()))
                }),
            );
        }
        let broker = RefCell::new(Broker::new(FileGrants::new(dir.join("grants.tsv"))));
        Rc::new(Engine {
            dir,
            settings,
            http,
            pool,
            schemes,
            blobs,
            bridge,
            broker,
            sites,
        })
    }

    /// Fetch a top-level document: `(final URL, HTML)`. Non-HTML content is
    /// wrapped in a minimal page. Runs on a worker thread.
    pub fn fetch_document(http: &Http, schemes: &Schemes, url: &str, https_only: bool) -> Result<(String, String), String> {
        if https_only && url.starts_with("http://") {
            return Err(format!("plain HTTP is disabled in settings: {url}"));
        }
        let (final_url, body, ctype) = if url.starts_with("http://") || url.starts_with("https://") {
            let r = http
                .send_following(
                    &gaze_net::HttpRequest {
                        url: url.into(),
                        method: "GET".into(),
                        headers: vec![("accept".into(), "text/html, application/xhtml+xml, */*".into())],
                        body: Vec::new(),
                    },
                    &|_| true,
                )
                .map_err(|e| e.to_string())?;
            if !(200..300).contains(&r.status) {
                return Err(format!("{url}: HTTP {}", r.status));
            }
            let ct = r.header("content-type").unwrap_or("text/html").to_string();
            (r.url, r.body, ct)
        } else {
            let (u, b) = gaze_net::fetch_url(http, schemes, url).map_err(|e| e.to_string())?;
            let ct = if u.ends_with(".txt") || u.ends_with(".rho") { "text/plain" } else { "text/html" };
            (u, b, ct.to_string())
        };
        let text = String::from_utf8_lossy(&body).into_owned();
        let html = if ctype.contains("html") {
            text
        } else if ctype.starts_with("image/") {
            format!("<html><body style=\"margin:0\"><img src=\"{}\"></body></html>", escape(&final_url))
        } else {
            format!("<html><body><pre>{}</pre></body></html>", escape(&text))
        };
        Ok((final_url, html))
    }
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn as_name(n: &Norm) -> Option<Name> {
    match n.node() {
        Node::Eval(x) => Some(x.clone()),
        _ => None,
    }
}

fn err(code: &str, d: &str) -> Norm {
    Norm::tuple(vec![Norm::str("err"), Norm::str(code), Norm::str(d)])
}

/// A navigation a page or a link asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavRequest {
    Go(String),
    Replace(String),
    Back,
    /// Cross-site: open in a new tab.
    Open(String),
}

enum Held {
    Net { site: Site, args: Vec<Norm> },
    Shard { class: ShardClass, args: Vec<Norm> },
}

/// One tab's live services. Shared between the page's `Services` and the
/// chrome, which answers prompts and drains navigations.
pub struct TabCore {
    pub tab: u64,
    pub site: Site,
    pub base_url: String,
    pub grant_hash: [u8; 32],
    eng: Rc<Engine>,
    store: Option<OriginStore>,
    pub shard: Option<ShardService>,
    async_out: Arc<Mutex<Vec<Delivery>>>,
    pub wake: WakeHandle,
    held: Vec<(u64, String, Held)>,
    next: u64,
    pub nav: Vec<NavRequest>,
}

impl TabCore {
    pub fn new(eng: Rc<Engine>, tab: u64, site: Site, base_url: &str, grant_hash: [u8; 32], wake: WakeHandle) -> TabCore {
        let w = wake.clone();
        let shard = Some(ShardService::new(Arc::clone(&eng.bridge), site.as_str(), Arc::new(move || w.wake())));
        TabCore {
            tab,
            site,
            base_url: base_url.to_string(),
            grant_hash,
            eng,
            store: None,
            shard,
            async_out: Arc::default(),
            wake,
            held: Vec::new(),
            next: 0,
            nav: Vec::new(),
        }
    }

    fn resolve(&self, u: &str) -> Option<String> {
        url::Url::parse(&self.base_url).ok()?.join(u).ok().map(|x| x.to_string()).or_else(|| url::Url::parse(u).ok().map(|x| x.to_string()))
    }

    fn hold(&mut self, text: String, h: Held) {
        self.next += 1;
        self.held.push((self.next, text, h));
        self.wake.wake();
    }

    /// Prompts for the chrome: `(id, text)`. Ids at or above `1 << 40` are
    /// the shard service's own (deploy and session consent).
    pub fn prompts(&self) -> Vec<(u64, String)> {
        let mut v: Vec<(u64, String)> = self.held.iter().map(|(i, t, _)| (*i, t.clone())).collect();
        if let Some(s) = &self.shard {
            v.extend(s.prompts().into_iter().map(|p| ((1 << 40) + p.id, p.text)));
        }
        v
    }

    pub fn answer(&mut self, id: u64, yes: bool, remember: bool) {
        if id >= 1 << 40 {
            if let Some(s) = &mut self.shard {
                s.answer(id - (1 << 40), yes);
            }
            return;
        }
        let Some(i) = self.held.iter().position(|(x, _, _)| *x == id) else { return };
        let (_, _, h) = self.held.remove(i);
        match h {
            Held::Net { site, args } => {
                if yes {
                    self.eng.broker.borrow_mut().allow_net(self.tab, site);
                    self.net(args);
                } else if let Some(r) = args.last().and_then(as_name) {
                    self.push(Class::Net, r, err("denied", "the user declined"));
                }
            }
            Held::Shard { class, args } => {
                if yes {
                    let gh = remember.then_some(self.grant_hash);
                    let _ = self.eng.broker.borrow_mut().allow_shard(self.tab, class, gh);
                    if let Some(s) = &mut self.shard {
                        s.request(SHARD, &args);
                    }
                } else if let Some(r) = args.last().and_then(as_name) {
                    self.push(Class::Shard, r, err("denied", "the user declined"));
                }
            }
        }
    }

    fn push(&self, class: Class, chan: Name, datum: Norm) {
        if let Ok(mut o) = self.async_out.lock() {
            o.push(Delivery::reply(class, chan, datum));
        }
        self.wake.wake();
    }

    fn net(&mut self, args: Vec<Norm>) {
        let Some(ret) = args.last().and_then(as_name) else { return };
        let f = match fetch_request(&args) {
            Ok(f) => f,
            Err(code) => return self.push(Class::Net, ret, err(code, "fetch")),
        };
        let url = match self.resolve(&f.request.url) {
            Some(u) => u,
            None => return self.push(Class::Net, ret, err("type", &f.request.url)),
        };
        let check = self.eng.broker.borrow().check_net(self.tab, &url);
        match check {
            Ok(()) => {
                let allowed = self.eng.broker.borrow().route(self.tab, NET).map(|r| r.att.net_sites.clone()).unwrap_or_default();
                let (http, schemes, out, wake) = (self.eng.http.clone(), self.eng.schemes.clone(), Arc::clone(&self.async_out), self.wake.clone());
                let mut f = f;
                f.request.url = url;
                self.eng.pool.spawn(move || {
                    let r = if f.request.url.starts_with("http") {
                        // Every redirect hop is checked against the allow-list.
                        http.send_following(&f.request, &|hop| Site::of_url(hop).is_some_and(|s| allowed.contains(&s)))
                    } else {
                        gaze_net::fetch_url(&http, &schemes, &f.request.url).map(|(u, body)| gaze_net::HttpResponse {
                            url: u,
                            status: 200,
                            headers: Vec::new(),
                            body,
                        })
                    };
                    let datum = fetch_reply(&f, r);
                    if let Ok(mut o) = out.lock() {
                        o.push(Delivery::reply(Class::Net, ret, datum));
                    }
                    wake.wake();
                });
            }
            Err(Refusal::Ask(text)) => {
                let site = Site::of_url(&url).expect("checked");
                self.hold(text, Held::Net { site, args });
            }
            Err(Refusal::Denied(d)) => self.push(Class::Net, ret, err("denied", &d)),
            Err(Refusal::Revoked) => self.push(Class::Net, ret, err("revoked", "")),
        }
    }

    fn store(&mut self, args: &[Norm]) {
        let Some(ret) = args.last().and_then(as_name) else {
            // Writes without an acknowledgement still happen.
            if let Some(s) = self.store_mut() {
                let _ = s.serve(args);
            }
            return;
        };
        if self.eng.broker.borrow().route(self.tab, STORE).is_none() {
            return self.push(Class::Store, ret, err("revoked", ""));
        }
        let datum = match self.store_mut() {
            Some(s) => s.serve(args).unwrap_or_else(|| err("type", "store")),
            None => err("io", "store unavailable"),
        };
        self.push(Class::Store, ret, datum);
    }

    fn store_mut(&mut self) -> Option<&mut OriginStore> {
        if self.store.is_none() {
            let quota = self.eng.settings.store_quota;
            self.store = OriginStore::open(path_for(&self.eng.dir.join("store"), self.site.as_str()), quota).ok();
        }
        self.store.as_mut()
    }

    fn nav_req(&mut self, args: &[Norm]) {
        let verb = args.first().and_then(|v| v.as_str()).unwrap_or("");
        let target = args.get(1).and_then(|v| v.as_str()).and_then(|u| self.resolve(u));
        let req = match (verb, target) {
            ("go", Some(u)) | ("replace", Some(u)) => {
                let same = Site::of_url(&u).is_some_and(|s| s.same_site(&self.site));
                if !same {
                    NavRequest::Open(u)
                } else if verb == "go" {
                    NavRequest::Go(u)
                } else {
                    NavRequest::Replace(u)
                }
            }
            ("back", _) => NavRequest::Back,
            _ => return,
        };
        self.nav.push(req);
        self.wake.wake();
    }

    fn shard_req(&mut self, urn: &str, args: Vec<Norm>) {
        if urn != SHARD {
            // A session name: consent was given when the session opened.
            if let Some(s) = &mut self.shard {
                s.request(urn, &args);
            }
            return;
        }
        let verb = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        let check = self.eng.broker.borrow().check_shard(self.tab, &verb);
        match check {
            Ok(_) => {
                if let Some(s) = &mut self.shard {
                    s.request(SHARD, &args);
                }
            }
            Err(Refusal::Ask(text)) => {
                let class = ShardClass::of_verb(&verb).unwrap_or(ShardClass::Read);
                self.hold(text, Held::Shard { class, args });
            }
            Err(e) => {
                if let Some(r) = args.last().and_then(as_name) {
                    let (c, d) = match e {
                        Refusal::Revoked => ("revoked", String::new()),
                        Refusal::Denied(d) => ("verb", d),
                        Refusal::Ask(d) => ("denied", d),
                    };
                    self.push(Class::Shard, r, err(c, &d));
                }
            }
        }
    }
}

/// The page's view of its tab's services.
pub struct TabServices(pub Rc<RefCell<TabCore>>);

impl Services for TabServices {
    fn request(&mut self, req: CapRequest, _out: &mut Vec<Delivery>) {
        let mut c = self.0.borrow_mut();
        match req.urn.as_str() {
            NET => c.net(req.args),
            STORE => c.store(&req.args),
            NAV => c.nav_req(&req.args),
            u if u == SHARD || u.starts_with("rho:gaze:shard/session/") => c.shard_req(u, req.args),
            _ => {
                if let Some(r) = req.args.last().and_then(as_name) {
                    c.push(Class::DomReply, r, err("revoked", ""));
                }
            }
        }
    }

    fn poll(&mut self, out: &mut Vec<Delivery>) {
        let mut c = self.0.borrow_mut();
        if let Ok(mut o) = c.async_out.lock() {
            out.append(&mut o);
        }
        if let Some(s) = &mut c.shard {
            for o in s.drain() {
                out.push(match o {
                    ShardOut::Reply { chan, datum } => Delivery::reply(Class::Shard, chan, datum),
                    ShardOut::BindSession { chan, label } => Delivery {
                        class: Class::Shard,
                        chan,
                        args: Vec::new(),
                        bind: Some(label),
                    },
                });
            }
        }
    }
}

/// Parse a hash in the forms pages and settings use.
pub fn parse_hash(s: &str) -> Option<[u8; 32]> {
    unhex(s.strip_prefix("blake2b-256:").unwrap_or(s))?.try_into().ok()
}
