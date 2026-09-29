//! The bridge: shared state ([`Bridge`]) and one [`ShardService`] per tab.

use crate::deploy::{DeployData, SignedDeploy, public_key, sign};
use crate::expr::to_norm;
use crate::keys::{Keystore, fresh_key};
use crate::node::Node;
use crate::site::{SiteAddr, SiteManifest};
use crate::term::render;
use gaze_blob::{BlobSource, Blobs};
use gaze_knf::Knf;
use gaze_net::{Http, Pool, hex};
use k1ndl1ng_norm::{CollKind, Lit, Name, Node as NNode, Norm};
use k256::ecdsa::SigningKey;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct ShardConfig {
    /// Read-only observers, run by distinct operators for the quorum rung.
    pub observers: Vec<String>,
    /// Where deploys go.
    pub validator: String,
    pub shard_id: String,
    /// Agreeing observers needed for the quorum rung.
    pub quorum: usize,
    pub phlo_price: i64,
    /// The profile's user id, part of every site key's identity.
    pub user: String,
}

impl Default for ShardConfig {
    fn default() -> Self {
        ShardConfig {
            observers: vec!["http://localhost:40453".into()],
            validator: "http://localhost:40403".into(),
            shard_id: "root".into(),
            quorum: 2,
            phlo_price: 1,
            user: "default".into(),
        }
    }
}

/// How much to trust an answer (spec §9.5).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Rung {
    Node,
    Quorum,
    Proof,
    Replayed,
}

impl Rung {
    pub fn name(self) -> &'static str {
        match self {
            Rung::Node => "node",
            Rung::Quorum => "quorum",
            Rung::Proof => "proof",
            Rung::Replayed => "replayed",
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Collapse a node answer (a list of values) to one term.
fn collapse(vs: &[Value]) -> Norm {
    match vs {
        [] => Norm::nil(),
        [one] => to_norm(one),
        many => Norm::list(many.iter().map(to_norm).collect()),
    }
}

/// Notifies subscribers of `block-finalised` events from `/ws/events`,
/// reconnecting with backoff. Subscribers that fall silent are dropped.
pub struct EventHub {
    subs: Mutex<Vec<Sender<()>>>,
}

impl EventHub {
    pub fn start(url: String) -> Arc<EventHub> {
        let hub = Arc::new(EventHub { subs: Mutex::new(Vec::new()) });
        let h = Arc::clone(&hub);
        let _ = std::thread::Builder::new().name("gaze-shard-events".into()).spawn(move || {
            let mut backoff = 1u64;
            loop {
                if Arc::strong_count(&h) == 1 {
                    return; // nobody left
                }
                if let Ok((mut ws, _)) = tungstenite::connect(&url) {
                    backoff = 1;
                    while let Ok(msg) = ws.read() {
                        if let Ok(t) = msg.to_text() {
                            let v: Value = serde_json::from_str(t).unwrap_or(Value::Null);
                            if v.get("event").and_then(|e| e.as_str()) == Some("block-finalised") {
                                h.notify();
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
            }
        });
        hub
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = channel();
        if let Ok(mut s) = self.subs.lock() {
            s.push(tx);
        }
        rx
    }

    pub fn notify(&self) {
        if let Ok(mut s) = self.subs.lock() {
            s.retain(|t| t.send(()).is_ok());
        }
    }
}

/// Shared by every tab of a profile.
pub struct Bridge {
    pub cfg: ShardConfig,
    pub http: Http,
    pub pool: Pool,
    pub keys: Arc<dyn Keystore>,
    pub blobs: Arc<Blobs>,
    /// Highest finalized block at which each binding was seen (freshness).
    seen: Mutex<BTreeMap<String, i64>>,
    pub events: Option<Arc<EventHub>>,
}

impl Bridge {
    pub fn new(cfg: ShardConfig, http: Http, pool: Pool, keys: Arc<dyn Keystore>, blobs: Arc<Blobs>) -> Arc<Bridge> {
        let events = cfg.observers.first().map(|o| EventHub::start(Node::new(o, http.clone()).events_url()));
        Arc::new(Bridge {
            cfg,
            http,
            pool,
            keys,
            blobs,
            seen: Mutex::new(BTreeMap::new()),
            events,
        })
    }

    fn observers(&self) -> Vec<Node> {
        self.cfg.observers.iter().map(|o| Node::new(o, self.http.clone())).collect()
    }
    fn validator(&self) -> Node {
        Node::new(&self.cfg.validator, self.http.clone())
    }

    /// Ask every observer the same question at the same finalized block and
    /// grade the answer: one observer is `node`; `quorum` needs `k` identical
    /// canonical encodings from distinct observers.
    fn graded(
        &self,
        ask: &dyn Fn(&Node, &str) -> Result<Norm, String>,
    ) -> Result<(Rung, Norm, i64), String> {
        let obs = self.observers();
        let first = obs.first().ok_or("no observers configured")?;
        let (block, num) = first.last_finalized()?;
        let mut answers: Vec<Norm> = Vec::new();
        let mut errors = Vec::new();
        for o in &obs {
            match ask(o, &block) {
                Ok(v) => answers.push(v),
                Err(e) => errors.push(e),
            }
        }
        let Some(a0) = answers.first().cloned() else {
            return Err(errors.join("; "));
        };
        if obs.len() == 1 {
            return Ok((Rung::Node, a0, num));
        }
        let mut best: Option<(usize, Norm)> = None;
        for a in &answers {
            let n = answers.iter().filter(|b| b.encode() == a.encode()).count();
            if best.as_ref().is_none_or(|(m, _)| n > *m) {
                best = Some((n, a.clone()));
            }
        }
        let (n, v) = best.expect("nonempty");
        if n >= self.cfg.quorum.max(2) {
            Ok((Rung::Quorum, v, num))
        } else {
            Err(format!("observers disagree at block {block} ({n} of {} agree)", obs.len()))
        }
    }

    fn check_fresh(&self, binding: &str, num: i64) -> Result<(), String> {
        let mut seen = self.seen.lock().map_err(|_| "poisoned")?;
        let e = seen.entry(binding.to_string()).or_insert(num);
        if num < *e {
            return Err(format!("stale answer: block {num} is older than {e}, already seen for {binding}"));
        }
        *e = num;
        Ok(())
    }

    pub fn lookup(&self, uri: &str) -> Result<(Rung, Norm), String> {
        let (rung, v, num) = self.graded(&|o, b| o.registry(uri, Some(b)).map(|(d, _, _)| collapse(&d)))?;
        self.check_fresh(uri, num)?;
        Ok((rung, v))
    }

    pub fn read_private(&self, hex: &str) -> Result<(Rung, Norm), String> {
        let (rung, v, _) = self.graded(&|o, b| o.data_at_private(hex, b).map(|d| collapse(&d)))?;
        Ok((rung, v))
    }

    pub fn explore(&self, term: &str) -> Result<(Rung, Norm), String> {
        let (rung, v, _) = self.graded(&|o, _| o.explore(term).map(|(d, _, _)| collapse(&d)))?;
        Ok((rung, v))
    }

    /// Resolve a site's manifest (with freshness).
    pub fn resolve_site(&self, addr: &SiteAddr) -> Result<(Rung, SiteManifest), String> {
        let uri = addr.registry_uri();
        let (rung, v, num) = self.graded(&|o, b| o.registry(&uri, Some(b)).map(|(d, _, _)| collapse(&d)))?;
        self.check_fresh(&addr.binding(), num)?;
        Ok((rung, SiteManifest::from_norm(&v)?))
    }

    /// One file of a site, verified by hash.
    pub fn site_file(&self, m: &SiteManifest, path: &str) -> Result<(String, Vec<u8>), String> {
        let (name, h) = m.file_for(path).ok_or_else(|| format!("no such file: {path}"))?;
        Ok((name.to_string(), self.blobs.get(&h, &m.mirrors)?))
    }

    /// The deploy term for the `.knf` a page named by hash.
    pub fn render_by_hash(&self, h: &[u8; 32], args: &[Norm]) -> Result<String, String> {
        let bytes = self.blobs.get(h, &[])?;
        let knf = Knf::decode(&bytes).map_err(|e| format!("not a .knf: {e:?}"))?;
        render(&knf, args)
    }

    pub fn estimate(&self, term: &str, key: &SigningKey) -> Result<u64, String> {
        let obs = self.observers();
        obs.first().ok_or("no observers")?.estimate_cost(term, &hex(&public_key(key)))
    }

    pub fn sign_and_deploy(&self, key: &SigningKey, term: &str, phlo_limit: i64) -> Result<SignedDeploy, String> {
        let (_, num) = self.validator().last_finalized()?;
        let now = now_ms();
        let d = sign(
            key,
            DeployData {
                term: term.to_string(),
                timestamp: now,
                phlo_price: self.cfg.phlo_price,
                phlo_limit,
                valid_after_block_number: num,
                shard_id: self.cfg.shard_id.clone(),
                // Replay protection besides the timestamp and block bound.
                expiration_timestamp: Some(now + 5 * 60 * 1000),
            },
        )?;
        self.validator().deploy(&d)?;
        Ok(d)
    }

    pub fn finalization(&self, id: &str) -> Result<(String, Option<String>), String> {
        self.validator().finalization(id)
    }
}

// ---------------------------------------------------------------------------

/// What the service wants the tab to do.
#[derive(Clone, Debug)]
pub enum ShardOut {
    /// `chan!(datum)` in the next frame.
    Reply { chan: Name, datum: Norm },
    /// Mint a session name labelled `label` and reply
    /// `("ok", rung, *session)` on `chan`.
    BindSession { chan: Name, label: String },
}

/// Something only the user can decide.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub id: u64,
    pub text: String,
}

enum Pending {
    Deploy { term: String, cost: u64, ret: Option<Name> },
    Session { uri: String, ret: Option<Name> },
}

struct Session {
    key: SigningKey,
    uri: String,
}

struct Shared {
    out: Vec<ShardOut>,
    prompts: Vec<(Prompt, Pending)>,
    sessions: BTreeMap<String, Session>,
    watches: Vec<(String, Name, Vec<u8>)>,
}

/// One tab's shard capability.
pub struct ShardService {
    bridge: Arc<Bridge>,
    site: String,
    shared: Arc<Mutex<Shared>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    next: u64,
    watcher: bool,
}

fn ok3(rung: &str, v: Norm) -> Norm {
    Norm::tuple(vec![Norm::str("ok"), Norm::str(rung), v])
}
fn err3(code: &str, d: &str) -> Norm {
    Norm::tuple(vec![Norm::str("err"), Norm::str(code), Norm::str(d)])
}
fn as_name(n: &Norm) -> Option<Name> {
    match n.node() {
        NNode::Eval(x) => Some(x.clone()),
        _ => None,
    }
}
fn hash_arg(n: &Norm) -> Option<[u8; 32]> {
    match n.as_lit() {
        Some(Lit::Bytes(b)) => b.to_vec().try_into().ok(),
        _ => {
            let s = n.as_str()?;
            gaze_net::unhex(s.strip_prefix("blake2b-256:").unwrap_or(s))?.try_into().ok()
        }
    }
}

impl ShardService {
    pub fn new(bridge: Arc<Bridge>, site: &str, wake: Arc<dyn Fn() + Send + Sync>) -> ShardService {
        ShardService {
            bridge,
            site: site.to_string(),
            shared: Arc::new(Mutex::new(Shared {
                out: Vec::new(),
                prompts: Vec::new(),
                sessions: BTreeMap::new(),
                watches: Vec::new(),
            })),
            wake,
            next: 0,
            watcher: false,
        }
    }

    fn push(shared: &Arc<Mutex<Shared>>, wake: &Arc<dyn Fn() + Send + Sync>, o: ShardOut) {
        if let Ok(mut s) = shared.lock() {
            s.out.push(o);
        }
        wake();
    }

    fn job(&self, f: impl FnOnce(&Bridge) -> Option<ShardOut> + Send + 'static) {
        let (b, sh, w) = (Arc::clone(&self.bridge), Arc::clone(&self.shared), Arc::clone(&self.wake));
        self.bridge.pool.spawn(move || {
            if let Some(o) = f(&b) {
                Self::push(&sh, &w, o);
            }
        });
    }

    /// A request from the page, on the `shard` capability (`label ==
    /// "rho:gaze:shard"`) or on a session name the bridge minted. The shell
    /// has already checked the verb class against the broker.
    pub fn request(&mut self, label: &str, args: &[Norm]) {
        let ret = args.last().and_then(as_name);
        if label != "rho:gaze:shard" {
            return self.session_send(label, args);
        }
        let verb = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        let reply = move |d: Norm| ret.clone().map(|chan| ShardOut::Reply { chan, datum: d });
        match verb.as_str() {
            "lookup" => {
                let Some(uri) = args.get(1).and_then(|u| u.as_str()).map(str::to_string) else {
                    return self.now(reply(err3("type", "lookup")));
                };
                self.job(move |b| {
                    reply(match b.lookup(&uri) {
                        Ok((r, v)) => ok3(r.name(), v),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "read" => {
                let target = args.get(1).cloned().unwrap_or_else(Norm::nil);
                self.job(move |b| {
                    let r = match (target.as_str(), target.as_coll(CollKind::Tuple)) {
                        (Some(s), _) if s.starts_with("rho:") => b.lookup(s),
                        (Some(h), _) => b.read_private(h),
                        (None, Some([_, _, h])) => b.read_private(h.as_str().unwrap_or("")),
                        _ => Err("read wants a URN or an unforgeable name".into()),
                    };
                    reply(match r {
                        Ok((r, v)) => ok3(r.name(), v),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "explore" => {
                let (Some(h), pargs) = (args.get(1).and_then(hash_arg), args.get(2).cloned()) else {
                    return self.now(reply(err3("type", "explore wants a program hash")));
                };
                let pargs: Vec<Norm> = pargs.and_then(|l| l.as_coll(CollKind::List).map(|x| x.to_vec())).unwrap_or_default();
                self.job(move |b| {
                    reply(match b.render_by_hash(&h, &pargs).and_then(|t| b.explore(&t)) {
                        Ok((r, v)) => ok3(r.name(), v),
                        Err(e) => err3("shard", &e),
                    })
                });
            }
            "deploy" => {
                let (Some(h), pargs) = (args.get(1).and_then(hash_arg), args.get(2).cloned()) else {
                    return self.now(reply(err3("type", "deploy wants a program hash")));
                };
                let pargs: Vec<Norm> = pargs.and_then(|l| l.as_coll(CollKind::List).map(|x| x.to_vec())).unwrap_or_default();
                let site = self.site.clone();
                self.next += 1;
                let id = self.next;
                let (sh, w) = (Arc::clone(&self.shared), Arc::clone(&self.wake));
                let ret2 = args.last().and_then(as_name);
                // Quote the cost first; the deploy itself waits for the user.
                self.job(move |b| {
                    let r = b.keys.site_key(&b.cfg.user, &site).and_then(|k| {
                        let term = b.render_by_hash(&h, &pargs)?;
                        let cost = b.estimate(&term, &k)?;
                        Ok((term, cost))
                    });
                    match r {
                        Ok((term, cost)) => {
                            if let Ok(mut s) = sh.lock() {
                                s.prompts.push((
                                    Prompt {
                                        id,
                                        text: format!(
                                            "{site} wants to deploy program {} to the shard, signed with this site's key. Estimated cost: {cost} phlo.",
                                            &hex(&h)[..12]
                                        ),
                                    },
                                    Pending::Deploy { term, cost, ret: ret2 },
                                ));
                            }
                            w();
                            None
                        }
                        Err(e) => reply(err3("shard", &e)),
                    }
                });
            }
            "watch" => {
                let (Some(uri), Some(ch)) = (args.get(1).and_then(|u| u.as_str()), args.get(2).and_then(as_name)) else {
                    return;
                };
                if let Ok(mut s) = self.shared.lock() {
                    s.watches.push((uri.to_string(), ch, Vec::new()));
                }
                self.start_watcher();
            }
            "session" => {
                let Some(uri) = args.get(1).and_then(|u| u.as_str()).map(str::to_string) else {
                    return self.now(reply(err3("type", "session")));
                };
                self.next += 1;
                let id = self.next;
                if let Ok(mut s) = self.shared.lock() {
                    s.prompts.push((
                        Prompt {
                            id,
                            text: format!("{} wants to open a session with {uri}", self.site),
                        },
                        Pending::Session {
                            uri,
                            ret: args.last().and_then(as_name),
                        },
                    ));
                }
                (self.wake)();
            }
            "proof" | "replayed" => self.now(reply(err3(
                "unavailable",
                "the proof and replayed rungs need node work package N1 and the rspace adapter",
            ))),
            _ => self.now(reply(err3("verb", &verb))),
        }
    }

    fn now(&self, o: Option<ShardOut>) {
        if let Some(o) = o {
            Self::push(&self.shared, &self.wake, o);
        }
    }

    /// A send on a session name becomes a deploy signed by the session key,
    /// delivered to the service as `svc!("msg", pubkey, args...)`.
    fn session_send(&mut self, label: &str, args: &[Norm]) {
        let s = match self.shared.lock() {
            Ok(g) => g.sessions.get(label).map(|s| (s.key.clone(), s.uri.clone())),
            Err(_) => None,
        };
        let Some((key, uri)) = s else { return };
        let args = args.to_vec();
        let ret = args.last().and_then(as_name);
        self.job(move |b| {
            let payload: Vec<Norm> = args.iter().filter(|a| as_name(a).is_none()).cloned().collect();
            if !payload.iter().all(crate::term::portable) {
                return ret.map(|chan| ShardOut::Reply { chan, datum: err3("type", "session payload") });
            }
            let shown: Vec<String> = payload.iter().map(k1ndl1ng_norm::show).collect();
            let term = format!(
                "new lookup(`rho:registry:lookup`), ch in {{ lookup!(`{uri}`, *ch) | for (svc <- ch) {{ svc!(\"msg\", \"{}\"{}{}) }} }}",
                hex(&public_key(&key)),
                if shown.is_empty() { "" } else { ", " },
                shown.join(", ")
            );
            let r = b.sign_and_deploy(&key, &term, 250_000);
            ret.map(|chan| ShardOut::Reply {
                chan,
                datum: match r {
                    Ok(d) => ok3("node", Norm::map(vec![(Norm::str("deploy"), Norm::str(&d.id()))])),
                    Err(e) => err3("shard", &e),
                },
            })
        });
    }

    fn start_watcher(&mut self) {
        if self.watcher {
            return;
        }
        self.watcher = true;
        let (b, sh, w) = (Arc::clone(&self.bridge), Arc::downgrade(&self.shared), Arc::clone(&self.wake));
        let rx = b.events.as_ref().map(|e| e.subscribe());
        let _ = std::thread::Builder::new().name("gaze-shard-watch".into()).spawn(move || {
            loop {
                let Some(shared) = sh.upgrade() else { return }; // tab closed
                let watches: Vec<(String, Name, Vec<u8>)> = shared.lock().map(|s| s.watches.clone()).unwrap_or_default();
                for (i, (uri, ch, last)) in watches.into_iter().enumerate() {
                    if let Ok((r, v)) = b.lookup(&uri) {
                        if v.encode() != last.as_slice() {
                            if let Ok(mut s) = shared.lock() {
                                if let Some(wt) = s.watches.get_mut(i) {
                                    wt.2 = v.encode().to_vec();
                                }
                                s.out.push(ShardOut::Reply {
                                    chan: ch,
                                    datum: Norm::tuple(vec![Norm::str("changed"), Norm::str(r.name()), v]),
                                });
                            }
                            w();
                        }
                    }
                }
                drop(shared);
                // Re-resolve on each finalized block; poll if there is no stream.
                match &rx {
                    Some(r) => {
                        let _ = r.recv_timeout(Duration::from_secs(30));
                    }
                    None => std::thread::sleep(Duration::from_secs(10)),
                }
            }
        });
    }

    /// Prompts waiting for the user.
    pub fn prompts(&self) -> Vec<Prompt> {
        self.shared.lock().map(|s| s.prompts.iter().map(|(p, _)| p.clone()).collect()).unwrap_or_default()
    }

    /// The user's answer to a prompt.
    pub fn answer(&mut self, id: u64, yes: bool) {
        let pending = match self.shared.lock() {
            Ok(mut s) => {
                let i = s.prompts.iter().position(|(p, _)| p.id == id);
                i.map(|i| s.prompts.remove(i).1)
            }
            Err(_) => None,
        };
        let Some(p) = pending else { return };
        match p {
            Pending::Deploy { ret, .. } | Pending::Session { ret, .. } if !yes => {
                self.now(ret.map(|chan| ShardOut::Reply { chan, datum: err3("denied", "the user declined") }))
            }
            Pending::Deploy { term, cost, ret } => {
                let site = self.site.clone();
                let (sh, w) = (Arc::clone(&self.shared), Arc::clone(&self.wake));
                self.job(move |b| {
                    let limit = (cost as i64).saturating_mul(3) / 2 + 10_000;
                    let d = match b.keys.site_key(&b.cfg.user, &site).and_then(|k| b.sign_and_deploy(&k, &term, limit)) {
                        Ok(d) => d,
                        Err(e) => return ret.map(|chan| ShardOut::Reply { chan, datum: err3("shard", &e) }),
                    };
                    let id = d.id();
                    if let Some(chan) = ret.clone() {
                        Self::push(
                            &sh,
                            &w,
                            ShardOut::Reply {
                                chan,
                                datum: ok3("node", Norm::map(vec![(Norm::str("deploy"), Norm::str(&id))])),
                            },
                        );
                    }
                    // Then once more with the outcome.
                    for _ in 0..100 {
                        std::thread::sleep(Duration::from_secs(3));
                        if let Ok((st, blk)) = b.finalization(&id) {
                            if st != "Pending" {
                                return ret.map(|chan| ShardOut::Reply {
                                    chan,
                                    datum: ok3(
                                        "node",
                                        Norm::map(vec![
                                            (Norm::str("deploy"), Norm::str(&id)),
                                            (Norm::str("status"), Norm::str(&st)),
                                            (Norm::str("block"), blk.map(|h| Norm::str(&h)).unwrap_or_else(Norm::nil)),
                                        ]),
                                    ),
                                });
                            }
                        }
                    }
                    None
                });
            }
            Pending::Session { uri, ret } => {
                self.next += 1;
                let label = format!("rho:gaze:shard/session/{}", self.next);
                let sh = Arc::clone(&self.shared);
                self.job(move |b| {
                    let r = fresh_key().and_then(|key| {
                        let term = format!(
                            "new lookup(`rho:registry:lookup`), ch in {{ lookup!(`{uri}`, *ch) | for (svc <- ch) {{ svc!(\"open\", \"{}\") }} }}",
                            hex(&public_key(&key))
                        );
                        b.sign_and_deploy(&key, &term, 250_000)?;
                        Ok(key)
                    });
                    let chan = ret?;
                    match r {
                        Ok(key) => {
                            if let Ok(mut s) = sh.lock() {
                                s.sessions.insert(label.clone(), Session { key, uri });
                            }
                            Some(ShardOut::BindSession { chan, label })
                        }
                        Err(e) => Some(ShardOut::Reply { chan, datum: err3("shard", &e) }),
                    }
                });
            }
        }
    }

    /// Everything ready for the tab.
    pub fn drain(&mut self) -> Vec<ShardOut> {
        self.shared.lock().map(|mut s| std::mem::take(&mut s.out)).unwrap_or_default()
    }

    /// Close a session: its key is destroyed.
    pub fn close_session(&mut self, label: &str) {
        if let Ok(mut s) = self.shared.lock() {
            s.sessions.remove(label);
        }
    }
}

// ---------------------------------------------------------------------------

/// Small blobs in F1R3Drive's on-chain layout (spec §9.7): a metadata map
/// `{"type": "f", "firstChunk": bytes, "otherChunks": {i: path}}` on the
/// public channel `@"<root><hex>"`, further chunks on their own channels.
/// Read by exploratory deploys that peek without consuming.
pub struct DriveSource {
    pub bridge: Arc<Bridge>,
    pub root: String,
}

pub const DRIVE_MAX: usize = 256 * 1024;

fn peek_term(path: &str) -> String {
    let p = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("new return in {{ for (@v <<- @\"{p}\") {{ return!(v) }} }}")
}

fn bytes_of(n: &Norm) -> Option<Vec<u8>> {
    match n.as_lit() {
        Some(Lit::Bytes(b)) => Some(b.to_vec()),
        _ => n.as_coll(CollKind::List).and_then(|l| {
            let mut out = Vec::new();
            for x in l {
                out.extend(bytes_of(x)?);
            }
            Some(out)
        }),
    }
}

impl BlobSource for DriveSource {
    fn name(&self) -> &str {
        "shard"
    }
    fn get(&self, h: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
        let path = format!("{}{}", self.root, hex(h));
        let (_, meta) = self.bridge.explore(&peek_term(&path))?;
        if meta.is_nil() {
            return Ok(None);
        }
        let mut out = meta.map_get("firstChunk").and_then(bytes_of).unwrap_or_default();
        let mut others: Vec<(i64, String)> = meta
            .map_get("otherChunks")
            .and_then(|m| m.as_coll(CollKind::Map))
            .map(|kv| {
                kv.chunks(2)
                    .filter_map(|p| {
                        let i = p[0].as_int().or_else(|| p[0].as_str().and_then(|s| s.parse().ok()))?;
                        Some((i, p[1].as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        others.sort();
        for (_, p) in others {
            if out.len() > DRIVE_MAX {
                return Err("on-chain blob exceeds 256 KiB".into());
            }
            let (_, c) = self.bridge.explore(&peek_term(&p))?;
            out.extend(bytes_of(&c).ok_or("chunk is not bytes")?);
        }
        Ok(Some(out))
    }
}
