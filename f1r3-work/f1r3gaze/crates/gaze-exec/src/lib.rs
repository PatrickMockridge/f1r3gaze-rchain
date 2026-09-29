//! `gaze-exec` — the tab executive (spec §6).
//!
//! One `TabExec` per tab: a CampF1R3 machine with a keyed minter and a page
//! meter, the DOM protocol engine, the capability services that do not need
//! the network (`doc`, `log`, `clock`, `rand`), a recorder, and replay.
//!
//! `frame(now)` is the whole driver and is a pure function of the executive's
//! state, its inbox and `now`:
//!
//! 1. due timers and frame subscriptions join the inbox;
//! 2. the inbox is ordered by (class, arrival) and each entry is recorded and
//!    injected;
//! 3. the machine runs for at most the frame budget, metered;
//! 4. the outbox is drained in order and routed to services; DOM reads are
//!    answered from the committed document, DOM writes are queued;
//! 5. the DOM commits the frame's writes together; replies wait for the next
//!    frame.

#![forbid(unsafe_code)]

use b3ll0ws::{ApertureId, Direct, Machine};
use campf1r3_core::{Abort, Config, Observer, PeekSet, SpaceError};
use gaze_dom_core::{as_name, err, ok, DomBackend, Engine, Key, Reply};
use gaze_graded::{Ceiling, Semiring, Xoshiro};
use gaze_knf::Knf;
use k1ndl1ng_campf1r3::{BindPat, Chan, Cont, Data};
use k1ndl1ng_norm::hash::blake2b_256;
use k1ndl1ng_norm::{CollKind, Keyed, Name, Node, Norm};
use k1ndl1ng_parse::Level;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

pub mod log;
pub use log::{Header, Record, TabLog};

// ---------------------------------------------------------------------------
// The page meter (spec §8.3).

/// Costs in meter units. A local budget, not a prediction of shard cost.
pub const COST_PRODUCE: i64 = 1;
pub const COST_CONSUME: i64 = 1;
pub const COST_COMM: i64 = 4;
pub const COST_APERTURE: i64 = 2;
/// Per 64 bytes of datum encoding.
pub const COST_PER_64B: i64 = 1;

#[derive(Clone, Default)]
pub struct PageMeter {
    allowance: Arc<AtomicI64>,
    spent: Arc<AtomicU64>,
}

impl PageMeter {
    pub fn refill(&self, units: i64) {
        self.allowance.store(units, Ordering::SeqCst);
    }
    pub fn remaining(&self) -> i64 {
        self.allowance.load(Ordering::SeqCst)
    }
    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::SeqCst)
    }
    fn debit(&self, units: i64) -> Result<(), Abort> {
        let prev = self.allowance.fetch_sub(units, Ordering::SeqCst);
        if prev < units {
            self.allowance.fetch_add(units, Ordering::SeqCst);
            return Err(Abort);
        }
        self.spent.fetch_add(units as u64, Ordering::SeqCst);
        Ok(())
    }
}

fn bytes_cost(d: &Data) -> i64 {
    let n: usize = d.args.iter().map(|a| a.encode().len()).sum();
    (n as i64 / 64) * COST_PER_64B
}

impl Observer<Chan, BindPat, Data, Cont> for PageMeter {
    fn observe_produce(&self, _c: &Chan, a: &Data, _p: bool) -> Result<(), Abort> {
        self.debit(COST_PRODUCE + bytes_cost(a))
    }
    fn observe_consume(&self, _g: &[Chan], _ps: &[BindPat], _k: &Cont, _p: bool, _pk: PeekSet) -> Result<(), Abort> {
        self.debit(COST_CONSUME)
    }
    fn observe_comm(&self, _k: &Cont, _p: bool, _d: &[(&Data, bool)]) -> Result<(), Abort> {
        self.debit(COST_COMM)
    }
    fn observe_aperture(&self, _c: &Chan, a: &Data) -> Result<(), Abort> {
        self.debit(COST_APERTURE + bytes_cost(a))
    }
}

// ---------------------------------------------------------------------------

/// Where injections come from, in the order a frame delivers them.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Class {
    DomReply = 0,
    Timer = 1,
    Input = 2,
    Net = 3,
    Store = 4,
    Shard = 5,
}

impl Class {
    pub fn from_u8(b: u8) -> Option<Class> {
        Some(match b {
            0 => Class::DomReply,
            1 => Class::Timer,
            2 => Class::Input,
            3 => Class::Net,
            4 => Class::Store,
            5 => Class::Shard,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Injection {
    pub class: Class,
    pub chan: Name,
    pub args: Vec<Norm>,
    pub seq: u64,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Cap {
    Log,
    Clock,
    Rand,
}

#[derive(Clone, Debug)]
enum Target {
    Dom(Key),
    Cap(Cap),
    /// A capability served outside the executive (`net`, `store`, `nav`,
    /// `shard`, ...): its sends surface as [`CapRequest`]s.
    Ext(String),
    /// The reply name of a synchronous `decide` drain.
    Decide,
}

/// A request a page sent to a capability the executive does not serve
/// itself. The host routes it to its service, which answers later with
/// [`TabExec::deliver`] on whatever reply name the request carried.
#[derive(Clone, Debug)]
pub struct CapRequest {
    pub urn: String,
    pub args: Vec<Norm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    Level(&'static str),
    Graded(String),
    Replay(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Level(m) => write!(f, "unsupported level: {m}"),
            LoadError::Graded(m) => write!(f, "graded page refused: {m}"),
            LoadError::Replay(m) => write!(f, "replay refused: {m}"),
        }
    }
}

/// What the broker decided for one import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub ident: String,
    pub urn: String,
    pub granted: bool,
}

/// The capabilities `gaze-exec` serves by itself.
pub const INTERNAL_CAPS: &[&str] = &["rho:gaze:doc", "rho:gaze:log", "rho:gaze:clock", "rho:gaze:rand"];

/// The stub broker's default policy: the internal capabilities are granted;
/// everything else is denied here and becomes a dead channel. The native
/// broker (`gaze-broker`) widens this.
pub fn default_policy(urn: &str) -> bool {
    INTERNAL_CAPS.contains(&urn)
}

/// What a user-input dispatch decided for the host's default action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    pub prevent: bool,
    pub stop: bool,
    /// Whether a synchronous `decide` drain ran.
    pub drained: bool,
}

pub struct TabExec<B: DomBackend> {
    machine: Machine<Direct, Keyed, PageMeter>,
    meter: PageMeter,
    pub dom: Engine<B>,
    targets: BTreeMap<u32, Target>,
    next_aperture: u32,
    inbox: Vec<Injection>,
    seq: u64,
    frame: u64,
    now: u64,
    frame_subs: Vec<Name>,
    timers: Vec<(u64, u64, Name)>,
    rng: Xoshiro,
    frame_budget: u64,
    sync_budget: u64,
    seed: [u8; 32],
    /// Keys by (identifier, capability), shared by every script of the page.
    import_keys: BTreeMap<(String, String), Key>,
    scripts: u32,
    bound_keys: std::collections::BTreeSet<Key>,
    ext_minted: u64,
    requests: Vec<CapRequest>,
    decide_key: Key,
    decided: (bool, bool),
    pub grants: Vec<Grant>,
    pub console: Vec<(String, String)>,
    pub log: TabLog,
    replaying: Option<Replaying>,
    stalled_frames: u64,
}

struct Replaying {
    injections: BTreeMap<u64, Vec<Injection>>,
    /// Dispatches recorded after frame `n`, re-run at the start of `n + 1`.
    dispatches: BTreeMap<u64, Vec<(Vec<u32>, String, Norm, bool)>>,
}

fn derive(label: &[u8], seed: &[u8; 32], extra: &[u8]) -> [u8; 32] {
    let mut pre = label.to_vec();
    pre.extend_from_slice(seed);
    pre.extend_from_slice(extra);
    blake2b_256(&pre).0
}

fn check_knf(knf: &Knf) -> Result<(), LoadError> {
    if knf.level >= Level::K2 {
        return Err(LoadError::Level("K2 guards need work package U2 (k1ndl1ng-eval)"));
    }
    let m = &knf.manifest;
    let sr = Semiring::parse(&m.semiring).ok_or_else(|| LoadError::Graded(format!("unknown semiring {}", m.semiring)))?;
    let ceiling = Ceiling::parse(&m.ceiling).ok_or_else(|| LoadError::Graded(format!("unknown ceiling {}", m.ceiling)))?;
    if sr.strength() > ceiling {
        return Err(LoadError::Graded(format!("semiring {} exceeds ceiling {}", sr.name(), ceiling.name())));
    }
    if sr != Semiring::Boolean {
        return Err(LoadError::Graded("graded resolution needs work package U5 (the store's resolver seam)".into()));
    }
    Ok(())
}

impl<B: DomBackend> TabExec<B> {
    /// Load a page's behaviour into a fresh tab over `backend`.
    pub fn load(knf: &Knf, backend: B, seed: [u8; 32], policy: &dyn Fn(&str) -> bool) -> Result<TabExec<B>, LoadError> {
        check_knf(knf)?;
        let m = &knf.manifest;
        let meter = PageMeter::default();
        let machine = Machine::with_observer(
            Config::default(),
            Direct::default(),
            Keyed::new(derive(b"gaze/minter/", &seed, &[])),
            meter.clone(),
        );
        let dom = Engine::new(backend, derive(b"gaze/dom-seed/", &seed, &[]));
        let mut tab = TabExec {
            machine,
            meter,
            dom,
            targets: BTreeMap::new(),
            next_aperture: 0,
            inbox: Vec::new(),
            seq: 0,
            frame: 0,
            now: 0,
            frame_subs: Vec::new(),
            timers: Vec::new(),
            rng: Xoshiro::new(derive(b"gaze/rand/", &seed, &[])),
            frame_budget: m.frame_budget,
            sync_budget: m.sync_budget,
            seed,
            import_keys: BTreeMap::new(),
            scripts: 0,
            bound_keys: Default::default(),
            ext_minted: 0,
            requests: Vec::new(),
            decide_key: derive(b"gaze/decide/", &seed, &[]),
            decided: (false, false),
            grants: Vec::new(),
            console: Vec::new(),
            log: TabLog::new(Header {
                program_hash: knf.program_hash(),
                grant_hash: knf.grant_hash(),
                seed,
                manifest: m.to_norm().encode().to_vec(),
            }),
            replaying: None,
            stalled_frames: 0,
        };
        let dk = tab.decide_key;
        tab.bind(dk, Target::Decide);
        tab.deploy(knf, policy);
        Ok(tab)
    }

    /// Load a document's further `f1r3lang` scripts, in document order, into
    /// the same executive. Imports are shared by identifier: a script that
    /// names `doc` gets the same `doc` as the first. Each is recorded, so a
    /// replay must be given the same scripts.
    pub fn add_script(&mut self, knf: &Knf, policy: &dyn Fn(&str) -> bool) -> Result<(), LoadError> {
        check_knf(knf)?;
        self.log.records.push(Record::Script {
            program_hash: knf.program_hash(),
            grant_hash: knf.grant_hash(),
        });
        self.deploy(knf, policy);
        Ok(())
    }

    fn deploy(&mut self, knf: &Knf, policy: &dyn Fn(&str) -> bool) {
        let script = self.scripts;
        self.scripts += 1;
        let mut keys = Vec::with_capacity(knf.manifest.imports.len());
        for (i, (ident, urn)) in knf.manifest.imports.iter().enumerate() {
            let pair = (ident.clone(), urn.clone());
            if let Some(k) = self.import_keys.get(&pair) {
                keys.push(*k);
                continue;
            }
            let granted = policy(urn);
            let grant = Grant {
                ident: ident.clone(),
                urn: urn.clone(),
                granted,
            };
            let key = if !granted {
                // A dead channel: fresh, bound to nothing, never answered.
                let mut extra = script.to_le_bytes().to_vec();
                extra.extend_from_slice(&(i as u32).to_le_bytes());
                derive(b"gaze/dead/", &self.seed, &extra)
            } else {
                match urn.as_str() {
                    "rho:gaze:doc" => self.dom.grant_document(),
                    other => {
                        let k = derive(b"gaze/cap/", &self.seed, other.as_bytes());
                        let t = match other {
                            "rho:gaze:log" => Target::Cap(Cap::Log),
                            "rho:gaze:clock" => Target::Cap(Cap::Clock),
                            "rho:gaze:rand" => Target::Cap(Cap::Rand),
                            _ => Target::Ext(other.to_string()),
                        };
                        // One key per capability: a second identifier for
                        // the same capability shares its aperture.
                        if !self.bound_keys.contains(&k) {
                            self.bound_keys.insert(k);
                            self.bind(k, t);
                        }
                        k
                    }
                }
            };
            self.grants.push(grant);
            self.import_keys.insert(pair, key);
            keys.push(key);
        }
        self.sync_dom_keys();
        self.machine.spawn(knf.body.ground_free(&keys));
    }

    /// Load a tab that replays `log` instead of consulting live services.
    /// `extra` are the document's further scripts, in the order recorded.
    pub fn replay(knf: &Knf, backend: B, log: &TabLog) -> Result<TabExec<B>, LoadError> {
        TabExec::replay_all(knf, &[], backend, log, &default_policy)
    }

    /// Replay with the page's further scripts and the policy it ran under
    /// (grants decide which names are live, so they are part of the input).
    pub fn replay_all(
        knf: &Knf,
        extra: &[Knf],
        backend: B,
        log: &TabLog,
        policy: &dyn Fn(&str) -> bool,
    ) -> Result<TabExec<B>, LoadError> {
        if log.header.program_hash != knf.program_hash() || log.header.grant_hash != knf.grant_hash() {
            return Err(LoadError::Replay("log was recorded for a different program or manifest".into()));
        }
        let recorded: Vec<([u8; 32], [u8; 32])> = log
            .records
            .iter()
            .filter_map(|r| match r {
                Record::Script { program_hash, grant_hash } => Some((*program_hash, *grant_hash)),
                _ => None,
            })
            .collect();
        let given: Vec<([u8; 32], [u8; 32])> = extra.iter().map(|k| (k.program_hash(), k.grant_hash())).collect();
        if recorded != given {
            return Err(LoadError::Replay("log was recorded with different further scripts".into()));
        }
        let mut tab = TabExec::load(knf, backend, log.header.seed, policy)?;
        for k in extra {
            tab.add_script(k, policy)?;
        }
        let mut rp = Replaying {
            injections: BTreeMap::new(),
            dispatches: BTreeMap::new(),
        };
        let mut cur = 0;
        for r in &log.records {
            match r {
                Record::Frame { n, .. } => cur = *n,
                Record::Inject(i) => rp.injections.entry(cur).or_default().push(i.clone()),
                Record::Dispatch { n, path, ty, fields, bubbles } => {
                    rp.dispatches.entry(*n).or_default().push((path.clone(), ty.clone(), fields.clone(), *bubbles))
                }
                _ => {}
            }
        }
        tab.replaying = Some(rp);
        Ok(tab)
    }

    fn bind(&mut self, k: Key, t: Target) {
        let id = self.next_aperture;
        self.next_aperture += 1;
        self.machine.bind_aperture(Name::Unforgeable(k), ApertureId(id));
        self.targets.insert(id, t);
    }

    fn sync_dom_keys(&mut self) {
        for k in self.dom.take_new_keys() {
            self.bind(k, Target::Dom(k));
        }
    }

    fn enqueue(&mut self, class: Class, chan: Name, args: Vec<Norm>) {
        if self.replaying.is_some() {
            return; // in replay, every injection comes from the log
        }
        self.seq += 1;
        self.inbox.push(Injection {
            class,
            chan,
            args,
            seq: self.seq,
        });
    }

    fn enqueue_replies(&mut self, class: Class, rs: Vec<Reply>) {
        for r in rs {
            self.enqueue(class, r.chan, r.args);
        }
    }

    /// A service's answer to a [`CapRequest`]: `chan!(args...)`, delivered in
    /// the next frame, ordered by `class`. Ignored during replay, where every
    /// answer comes from the log.
    pub fn deliver(&mut self, class: Class, chan: Name, args: Vec<Norm>) {
        self.enqueue(class, chan, args);
    }

    /// Mint a fresh name served outside the executive, labelled `label`:
    /// sends on it surface as [`CapRequest`]s with `urn == label`. The shard
    /// bridge uses this for session names, which the page holds while the
    /// bridge holds the key. The name derives from the tab's seed and a
    /// counter, so it is unguessable and replays identically.
    pub fn bind_ext(&mut self, label: &str) -> Key {
        self.ext_minted += 1;
        let mut extra = label.as_bytes().to_vec();
        extra.extend_from_slice(&self.ext_minted.to_le_bytes());
        let k = derive(b"gaze/ext/", &self.seed, &extra);
        self.bound_keys.insert(k);
        self.bind(k, Target::Ext(label.to_string()));
        k
    }

    /// Retire every name bound under `label` (a closed session).
    pub fn unbind_ext(&mut self, label: &str) {
        self.targets.retain(|_, t| !matches!(t, Target::Ext(u) if u == label));
    }

    /// Requests for external capabilities since the last call.
    pub fn take_requests(&mut self) -> Vec<CapRequest> {
        std::mem::take(&mut self.requests)
    }

    /// Revoke a capability: its routing entry goes, and every later send on
    /// its name is answered `("err", "revoked")` — indistinguishable to the
    /// page from a name that was never live.
    pub fn revoke(&mut self, urn: &str) {
        let ids: Vec<u32> = self
            .targets
            .iter()
            .filter(|(_, t)| match t {
                Target::Ext(u) => u == urn,
                Target::Cap(Cap::Log) => urn == "rho:gaze:log",
                Target::Cap(Cap::Clock) => urn == "rho:gaze:clock",
                Target::Cap(Cap::Rand) => urn == "rho:gaze:rand",
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.targets.remove(&id);
        }
        if urn == "rho:gaze:clock" {
            self.frame_subs.clear();
            self.timers.clear();
        }
        for g in &mut self.grants {
            if g.urn == urn {
                g.granted = false;
            }
        }
    }

    /// Deliver a user-input event to a node of the document. Static listener
    /// options decide `prevent` and `stop` now. Ordinary handlers run in the
    /// next frame. `decide` listeners get the datum at once, with a `reply`
    /// name, and the executive runs for at most the manifest's `sync` budget
    /// in steps; `"prevent"` or `"stop"` sent on `reply` in that time count.
    /// The budget is in steps, so the decision is deterministic and replays.
    pub fn dispatch(&mut self, target: B::Node, ty: &str, fields: Vec<(String, Norm)>) -> (bool, bool) {
        let d = self.dispatch_ext(target, ty, fields, true);
        (d.prevent, d.stop)
    }

    /// [`dispatch`](Self::dispatch) for events that may not bubble.
    pub fn dispatch_ext(&mut self, target: B::Node, ty: &str, fields: Vec<(String, Norm)>, bubbles: bool) -> Decision {
        if self.replaying.is_none() {
            let path = self.dom.path_of(target);
            let fmap = Norm::map(fields.iter().map(|(k, v)| (Norm::str(k), v.clone())).collect());
            self.log.records.push(Record::Dispatch {
                n: self.frame,
                path,
                ty: ty.to_string(),
                fields: fmap,
                bubbles,
            });
        }
        self.dispatch_inner(target, ty, fields, bubbles)
    }

    fn dispatch_inner(&mut self, target: B::Node, ty: &str, fields: Vec<(String, Norm)>, bubbles: bool) -> Decision {
        let dk = self.decide_key;
        let f = self.dom.fire_with(target, ty, fields, bubbles, Some(dk));
        self.sync_dom_keys();
        self.enqueue_replies(Class::Input, f.injections);
        let mut d = Decision {
            prevent: f.prevent,
            stop: f.stop,
            drained: false,
        };
        if !f.sync.is_empty() {
            d.drained = true;
            self.decided = (false, false);
            for r in f.sync {
                self.machine.inject(r.chan, r.args);
            }
            self.meter.refill(self.sync_budget as i64 * COST_COMM);
            let _ = self.machine.run(self.sync_budget);
            self.route_outbound();
            d.prevent |= self.decided.0;
            d.stop |= self.decided.1;
        }
        d
    }

    pub fn frame_count(&self) -> u64 {
        self.frame
    }
    pub fn meter(&self) -> &PageMeter {
        &self.meter
    }
    pub fn is_quiescent(&self) -> bool {
        self.machine.is_quiescent() && self.inbox.is_empty()
    }
    pub fn stalled_frames(&self) -> u64 {
        self.stalled_frames
    }
    pub fn is_replaying(&self) -> bool {
        self.replaying.is_some()
    }

    /// Whether the host should schedule another frame: there is work queued,
    /// a frame subscription, or a timer. `next_deadline` says when.
    pub fn wants_frame(&self) -> bool {
        !self.is_quiescent() || !self.frame_subs.is_empty() || !self.requests.is_empty()
    }

    /// The earliest pending timer, in the executive's clock.
    pub fn next_deadline(&self) -> Option<u64> {
        self.timers.iter().map(|(t, _, _)| *t).min()
    }

    /// Grant more budget for the next frame (the user's "let it run").
    pub fn set_frame_budget(&mut self, steps: u64) {
        self.frame_budget = steps.max(1);
    }

    /// Run one frame at time `now_ms`. Returns the committed document's hash.
    pub fn frame(&mut self, now_ms: u64) -> [u8; 32] {
        // 0. In replay, the dispatches that happened after the last frame.
        if let Some(rp) = &mut self.replaying {
            let ds = rp.dispatches.remove(&self.frame).unwrap_or_default();
            for (path, ty, fields, bubbles) in ds {
                if let Some(t) = self.dom.node_at(&path) {
                    let f: Vec<(String, Norm)> = fields
                        .as_coll(CollKind::Map)
                        .unwrap_or(&[])
                        .chunks(2)
                        .filter_map(|kv| Some((kv[0].as_str()?.to_string(), kv[1].clone())))
                        .collect();
                    self.dispatch_inner(t, &ty, f, bubbles);
                }
            }
        }

        self.frame += 1;
        self.now = now_ms;
        let n = self.frame;
        self.meter.refill(self.frame_budget as i64 * COST_COMM);

        // 1. Clock sources.
        let due: Vec<(u64, u64, Name)> = {
            let (mut due, keep): (Vec<_>, Vec<_>) = self.timers.drain(..).partition(|(t, _, _)| *t <= now_ms);
            self.timers = keep;
            due.sort_by_key(|(t, s, _)| (*t, *s));
            due
        };
        for (_, _, ch) in due {
            self.enqueue(Class::Timer, ch, vec![Norm::tuple(vec![Norm::str("timer"), Norm::int(now_ms as i64)])]);
        }
        for ch in self.frame_subs.clone() {
            self.enqueue(
                Class::Timer,
                ch,
                vec![Norm::tuple(vec![Norm::str("tick"), Norm::int(n as i64), Norm::int(now_ms as i64)])],
            );
        }

        // 2. Order, record, inject.
        let mut batch: Vec<Injection> = match &mut self.replaying {
            Some(rp) => rp.injections.remove(&n).unwrap_or_default(),
            None => std::mem::take(&mut self.inbox),
        };
        batch.sort_by_key(|i| (i.class, i.seq));
        self.log.records.push(Record::Frame { n, now_ms });
        for i in batch {
            self.log.records.push(Record::Inject(i.clone()));
            self.machine.inject(i.chan, i.args);
        }

        // 3. Run, metered.
        match self.machine.run(self.frame_budget) {
            Ok(_) => {}
            Err(SpaceError::Aborted) => {
                self.stalled_frames += 1;
                self.log.records.push(Record::Budget { n });
            }
            Err(e) => self.log.records.push(Record::Error {
                n,
                msg: format!("{e:?}"),
            }),
        }

        // 4. Route the outbox.
        self.route_outbound();

        // 5. Commit.
        let (replies, hash) = self.dom.commit();
        self.sync_dom_keys();
        self.enqueue_replies(Class::DomReply, replies);
        self.log.records.push(Record::Commit { n, hash });
        hash
    }

    fn route_outbound(&mut self) {
        for o in self.machine.take_outbound() {
            let target = self.targets.get(&o.aperture.0).cloned();
            match target {
                Some(Target::Dom(k)) => {
                    let rs = self.dom.handle(&k, &o.args);
                    self.sync_dom_keys();
                    self.enqueue_replies(Class::DomReply, rs);
                }
                Some(Target::Cap(Cap::Log)) => {
                    let level = o.args.first().and_then(|a| a.as_str()).unwrap_or("info").to_string();
                    let text = o.args.get(1).map(k1ndl1ng_norm::show_pretty).unwrap_or_default();
                    self.console.push((level, text));
                }
                Some(Target::Cap(Cap::Clock)) => self.clock(&o.args),
                Some(Target::Cap(Cap::Rand)) => self.rand(&o.args),
                Some(Target::Ext(urn)) => {
                    if self.replaying.is_none() {
                        self.requests.push(CapRequest { urn, args: o.args });
                    }
                }
                Some(Target::Decide) => match o.args.first().and_then(|a| a.as_str()) {
                    Some("prevent") => self.decided.0 = true,
                    Some("stop") => self.decided.1 = true,
                    _ => {}
                },
                None => {
                    if let Some(r) = o.args.last().and_then(as_name) {
                        self.enqueue(Class::DomReply, r, vec![err("revoked", "")]);
                    }
                }
            }
        }
    }

    fn clock(&mut self, a: &[Norm]) {
        let verb = a.first().and_then(|v| v.as_str()).unwrap_or("");
        match verb {
            "frames" => {
                if let Some(ch) = a.get(1).and_then(as_name) {
                    if !self.frame_subs.contains(&ch) {
                        self.frame_subs.push(ch);
                    }
                }
            }
            "stop" => {
                if let Some(ch) = a.get(1).and_then(as_name) {
                    self.frame_subs.retain(|c| *c != ch);
                }
            }
            "after" => {
                if let (Some(ms), Some(ch)) = (a.get(1).and_then(|x| x.as_int()), a.get(2).and_then(as_name)) {
                    self.seq += 1;
                    self.timers.push((self.now.saturating_add(ms.max(0) as u64), self.seq, ch));
                }
            }
            "now" => {
                if let Some(r) = a.get(1).and_then(as_name) {
                    self.enqueue(Class::Timer, r, vec![ok(Norm::int(self.now as i64))]);
                }
            }
            _ => {}
        }
    }

    fn rand(&mut self, a: &[Norm]) {
        let verb = a.first().and_then(|v| v.as_str()).unwrap_or("");
        let Some(r) = a.last().and_then(as_name) else { return };
        let v = match verb {
            "u64" => ok(Norm::int(self.rng.next_u64() as i64)),
            "bytes" => {
                let n = a.get(1).and_then(|x| x.as_int()).unwrap_or(0).clamp(0, 4096) as usize;
                let mut b = vec![0u8; n];
                self.rng.fill(&mut b);
                ok(Norm::bytes(&b))
            }
            _ => err("verb", verb),
        };
        self.enqueue(Class::Timer, r, vec![v]);
    }

    /// The commit hashes recorded so far, one per frame.
    pub fn commit_hashes(&self) -> Vec<[u8; 32]> {
        self.log
            .records
            .iter()
            .filter_map(|r| match r {
                Record::Commit { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect()
    }
}

/// Render an injection's channel for diagnostics.
pub fn chan_hex(n: &Name) -> String {
    match n {
        Name::Unforgeable(k) => k[..4].iter().map(|b| format!("{b:02x}")).collect(),
        _ => "quoted".into(),
    }
}

/// Is this term a send? Used by the log codec.
pub(crate) fn as_send(n: &Norm) -> Option<(Name, Vec<Norm>)> {
    match n.node() {
        Node::Send { chan, args, .. } => Some((chan.clone(), args.clone())),
        _ => None,
    }
}
