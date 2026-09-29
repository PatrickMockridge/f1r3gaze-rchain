//! One tab's load pipeline (spec §5.4):
//! fetch → parse (renders statically) → fetch and verify scripts →
//! grant plan → prompts → run. The chrome and the headless runner both
//! drive it.

use crate::engine::{Engine, NavRequest, TabCore, TabServices};
use crate::pages;
use crate::profile::seed;
use blitz_dom::DocumentConfig;
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::NetWaker;
use gaze_broker::{GrantPlan, Site, Status};
use gaze_dom_blitz::{PageState, RhoDocument, WakeHandle};
use gaze_knf::Knf;
use gaze_net::GazeNetProvider;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Fetching,
    Scripts,
    Grants,
    Running,
    Static,
    Failed(String),
}

enum Msg {
    Doc(u64, Result<(String, String), String>),
    Script(u64, usize, Result<Knf, String>),
}

struct WakeNet(WakeHandle);
impl NetWaker for WakeNet {
    fn wake(&self, _doc: usize) {
        self.0.wake();
    }
}

/// Link clicks and form submissions from the page's own default actions.
struct LinkNav {
    queue: Arc<Mutex<Vec<String>>>,
    wake: WakeHandle,
}
impl NavigationProvider for LinkNav {
    fn navigate_to(&self, o: NavigationOptions) {
        if let Ok(mut q) = self.queue.lock() {
            q.push(o.url.to_string());
        }
        self.wake.wake();
    }
}

pub const PLAN_PROMPT: u64 = 1 << 50;

pub struct Tab {
    pub id: u64,
    pub url: String,
    pub history: Vec<String>,
    pub hist: usize,
    pub stage: Stage,
    pub title: String,
    /// Shown in the status line (e.g. "JavaScript was not run").
    pub notice: Option<String>,
    eng: Rc<Engine>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    generation: u64,
    knfs: Vec<Option<Knf>>,
    plans: Vec<GrantPlan>,
    fail: Option<String>,
    pub core: Option<Rc<RefCell<TabCore>>>,
    links: Arc<Mutex<Vec<String>>>,
    pub wake: WakeHandle,
}

impl Tab {
    pub fn new(eng: Rc<Engine>, id: u64, wake: WakeHandle) -> Tab {
        let (tx, rx) = channel();
        Tab {
            id,
            url: String::new(),
            history: Vec::new(),
            hist: 0,
            stage: Stage::Fetching,
            title: String::new(),
            notice: None,
            eng,
            tx,
            rx,
            generation: 0,
            knfs: Vec::new(),
            plans: Vec::new(),
            fail: None,
            core: None,
            links: Arc::default(),
            wake,
        }
    }

    /// Start loading `url`. With `record`, it becomes a new history entry.
    pub fn navigate(&mut self, url: &str, record: bool) {
        let url = normalise_input(url);
        if record {
            self.history.truncate(if self.history.is_empty() { 0 } else { self.hist + 1 });
            self.history.push(url.clone());
            self.hist = self.history.len() - 1;
        }
        self.generation += 1;
        self.url = url.clone();
        self.stage = Stage::Fetching;
        self.notice = None;
        self.fail = None;
        self.knfs.clear();
        self.plans.clear();
        self.core = None;
        self.eng.broker.borrow_mut().close_tab(self.id);
        let (tx, g, w) = (self.tx.clone(), self.generation, self.wake.clone());
        let (http, schemes, https_only) = (self.eng.http.clone(), self.eng.schemes.clone(), self.eng.settings.https_only);
        self.eng.pool.spawn(move || {
            let _ = tx.send(Msg::Doc(g, Engine::fetch_document(&http, &schemes, &url, https_only)));
            w.wake();
        });
    }

    pub fn back(&mut self) -> bool {
        if self.hist == 0 {
            return false;
        }
        self.hist -= 1;
        let u = self.history[self.hist].clone();
        self.navigate(&u, false);
        true
    }

    pub fn forward(&mut self) -> bool {
        if self.hist + 1 >= self.history.len() {
            return false;
        }
        self.hist += 1;
        let u = self.history[self.hist].clone();
        self.navigate(&u, false);
        true
    }

    pub fn reload(&mut self) {
        let u = self.url.clone();
        self.navigate(&u, false);
    }

    fn config(&self, base: &str) -> DocumentConfig {
        DocumentConfig {
            base_url: Some(base.to_string()),
            net_provider: Some(Arc::new(GazeNetProvider::new(
                self.eng.http.clone(),
                self.eng.schemes.clone(),
                self.eng.pool.clone(),
                Some(Arc::new(WakeNet(self.wake.clone()))),
            ))),
            navigation_provider: Some(Arc::new(LinkNav {
                queue: Arc::clone(&self.links),
                wake: self.wake.clone(),
            })),
            ..Default::default()
        }
    }

    /// Process finished background work. Returns a new document to show,
    /// when one has been parsed.
    pub fn pump(&mut self) -> Option<RhoDocument> {
        let mut out = None;
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Doc(g, _) | Msg::Script(g, _, _) if g != self.generation => {}
                Msg::Doc(_, Ok((url, html))) => {
                    self.url = url.clone();
                    if let Some(h) = self.history.get_mut(self.hist) {
                        *h = url.clone();
                    }
                    let doc = RhoDocument::from_html(&html, self.config(&url));
                    self.title = doc.title();
                    let scripts = doc.scripts();
                    if scripts.is_empty() {
                        self.stage = Stage::Static;
                        if doc.has_js_scripts() {
                            self.notice = Some("This page's JavaScript was not run; F1R3Gaze runs only f1r3lang.".into());
                        }
                    } else {
                        self.stage = Stage::Scripts;
                        self.knfs = vec![None; scripts.len()];
                        for (i, s) in scripts.into_iter().enumerate() {
                            match &s.src {
                                None => {
                                    let r = s.compile_inline();
                                    let _ = self.tx.send(Msg::Script(self.generation, i, r));
                                }
                                Some(src) => {
                                    let Some(abs) = url::Url::parse(&url).ok().and_then(|b| b.join(src).ok()) else {
                                        let _ = self.tx.send(Msg::Script(self.generation, i, Err(format!("bad script URL {src}"))));
                                        continue;
                                    };
                                    let (tx, g, w) = (self.tx.clone(), self.generation, self.wake.clone());
                                    let (http, schemes) = (self.eng.http.clone(), self.eng.schemes.clone());
                                    self.eng.pool.spawn(move || {
                                        let r = gaze_net::fetch_url(&http, &schemes, abs.as_str())
                                            .map_err(|e| format!("{abs}: {e}"))
                                            .and_then(|(_, b)| s.accept(&b));
                                        let _ = tx.send(Msg::Script(g, i, r));
                                        w.wake();
                                    });
                                }
                            }
                        }
                    }
                    out = Some(doc);
                }
                Msg::Doc(_, Err(e)) => {
                    self.stage = Stage::Failed(e.clone());
                    self.title = "Could not load".into();
                    out = Some(RhoDocument::from_html(&pages::error(&self.url, &e), self.config("gaze://error")));
                    self.stage = Stage::Failed(e);
                }
                Msg::Script(_, i, Ok(k)) => {
                    if let Some(slot) = self.knfs.get_mut(i) {
                        *slot = Some(k);
                    }
                    if self.stage == Stage::Scripts && self.knfs.iter().all(Option::is_some) {
                        let site = Site::of_url(&self.url).unwrap_or_else(|| Site::of_url("gaze://unknown").expect("valid"));
                        let b = self.eng.broker.borrow();
                        self.plans = self.knfs.iter().flatten().map(|k| b.plan(&site, k)).collect();
                        self.stage = Stage::Grants;
                    }
                }
                Msg::Script(_, _, Err(e)) => {
                    self.fail = Some(e.clone());
                    self.stage = Stage::Failed(e);
                }
            }
        }
        out
    }

    /// Advance the stages that need the tab's document.
    pub fn pump_doc(&mut self, doc: &mut RhoDocument) {
        if let Some(f) = self.fail.take() {
            doc.fail(format!("script refused: {f}"));
        }
        if self.stage == Stage::Grants && self.plans.iter().all(GrantPlan::is_settled) {
            let site = self.plans[0].site.clone();
            {
                let mut b = self.eng.broker.borrow_mut();
                for p in &self.plans {
                    b.install(self.id, p);
                }
            }
            let core = Rc::new(RefCell::new(TabCore::new(
                Rc::clone(&self.eng),
                self.id,
                site,
                &self.url,
                self.plans[0].grant_hash,
                doc.wake_handle(),
            )));
            let plans = self.plans.clone();
            let policy = move |urn: &str| plans.iter().any(|p| p.granted(urn));
            let knfs: Vec<Knf> = self.knfs.iter().flatten().cloned().collect();
            match doc.start(&knfs, seed(), &policy, Box::new(TabServices(Rc::clone(&core)))) {
                Ok(()) => {
                    self.stage = Stage::Running;
                    self.core = Some(core);
                }
                Err(e) => self.stage = Stage::Failed(e.to_string()),
            }
        }
        if let PageState::Failed(e) = doc.state() {
            if !matches!(self.stage, Stage::Failed(_)) {
                self.stage = Stage::Failed(e.clone());
            }
        }
        let t = doc.title();
        if !t.is_empty() {
            self.title = t;
        }
    }

    /// Undecided capabilities of the load, deduplicated by capability.
    fn plan_pending(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = Vec::new();
        for p in &self.plans {
            for e in &p.entries {
                if let Status::Ask(t) = &e.status {
                    if !v.iter().any(|(u, _)| *u == e.urn) {
                        v.push((e.urn.clone(), t.clone()));
                    }
                }
            }
        }
        v
    }

    pub fn prompts(&self) -> Vec<(u64, String)> {
        let mut v: Vec<(u64, String)> = self
            .plan_pending()
            .into_iter()
            .enumerate()
            .map(|(i, (_, t))| (PLAN_PROMPT + i as u64, t))
            .collect();
        if let Some(c) = &self.core {
            v.extend(c.borrow().prompts());
        }
        v
    }

    pub fn answer(&mut self, id: u64, yes: bool, remember: bool) {
        if (PLAN_PROMPT..(1 << 51)).contains(&id) {
            let pending = self.plan_pending();
            if let Some((urn, _)) = pending.get((id - PLAN_PROMPT) as usize) {
                let mut b = self.eng.broker.borrow_mut();
                for p in &mut self.plans {
                    if p.pending().any(|e| &e.urn == urn) {
                        let _ = b.answer(p, urn, yes, remember);
                    }
                }
            }
            self.wake.wake();
        } else if let Some(c) = &self.core {
            c.borrow_mut().answer(id, yes, remember);
        }
    }

    /// Navigations the page asked for, and link clicks.
    pub fn take_nav(&mut self) -> Vec<NavRequest> {
        let mut v: Vec<NavRequest> = match &self.core {
            Some(c) => std::mem::take(&mut c.borrow_mut().nav),
            None => Vec::new(),
        };
        if let Ok(mut l) = self.links.lock() {
            // A link the user clicked navigates this tab, whatever its site.
            v.extend(l.drain(..).map(NavRequest::Go));
        }
        v
    }

    /// Revoke a capability for this tab.
    pub fn revoke(&mut self, urn: &str, doc: &mut RhoDocument) {
        self.eng.broker.borrow_mut().revoke(self.id, urn);
        doc.revoke(urn);
    }

    pub fn close(&mut self) {
        self.eng.broker.borrow_mut().close_tab(self.id);
        self.core = None;
    }

    pub fn status(&self) -> String {
        match &self.stage {
            Stage::Fetching => "Loading…".into(),
            Stage::Scripts => "Fetching and verifying scripts…".into(),
            Stage::Grants => "Waiting for your answer before the page runs".into(),
            Stage::Running => "Running f1r3lang".into(),
            Stage::Static => "Static document".into(),
            Stage::Failed(e) => format!("Failed: {e}"),
        }
    }
}

/// What the user typed, as a URL: bare hosts become https, known schemes
/// pass through.
pub fn normalise_input(s: &str) -> String {
    let s = s.trim();
    if s.contains("://") || s.starts_with("data:") || s.starts_with("about:") {
        return s.to_string();
    }
    if s.starts_with('/') {
        return format!("file://{s}");
    }
    format!("https://{s}")
}
