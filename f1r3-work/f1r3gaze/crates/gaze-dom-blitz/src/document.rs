//! [`RhoDocument`]: a Blitz [`Document`] whose only behaviour is f1r3lang.
//!
//! Its shape follows `blitz-vibey-script`'s `ScriptDocument` (wrap a
//! `BaseDocument`, implement `handle_ui_event` and `poll`), with the tab
//! executive where the JavaScript runtime was. Nothing here executes any
//! other script type; `blitz-vibey-script` is not linked.
//!
//! Loading is split so the shell can do the asynchronous parts (fetching,
//! verifying, asking the broker, prompting) between parsing and running:
//!
//! 1. [`RhoDocument::from_html`] parses; the page renders statically.
//! 2. [`RhoDocument::scripts`] lists the `application/f1r3lang` scripts.
//! 3. The shell fetches each `src`, checks it with [`ScriptRef::accept`],
//!    gets a grant plan, and calls [`RhoDocument::start`].
//! 4. [`Document::poll`] then drives frames: at most one per 16 ms in the
//!    foreground and one per 250 ms in the background, only while the page
//!    has work, a frame subscription, a due timer or fresh input.
//!
//! The executive runs on the window's thread. Each frame is bounded by the
//! manifest's step budget (and the meter), so a page cannot hold the thread
//! for longer than that; process isolation is phase P4.

use crate::backend::BlitzDom;
use crate::events::RhoEventHandler;
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, EventDriver, NoopEventHandler, local_name};
use blitz_traits::events::UiEvent;
use gaze_exec::{CapRequest, Class, Grant, LoadError, TabExec};
use gaze_knf::{Knf, KnfError, default_urn};
use k1ndl1ng_norm::{Name, Norm};
use k1ndl1ng_parse::Level;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Waker};
use std::time::{Duration, Instant};

pub const FOREGROUND_INTERVAL_MS: u64 = 16;
pub const BACKGROUND_INTERVAL_MS: u64 = 250;

/// One `<script type="application/f1r3lang">`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRef {
    pub src: Option<String>,
    pub integrity: Option<String>,
    /// Inline kernel text, when there is no `src`.
    pub inline: Option<String>,
    pub level: Level,
    /// `imports="ident=urn ident ..."`; a bare identifier takes its
    /// conventional capability.
    pub imports: Vec<(String, String)>,
    pub semiring: Option<String>,
}

fn parse_level(s: &str) -> Option<Level> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "" | "k1g" => Level::K1G,
        "k0" => Level::K0,
        "k1" => Level::K1,
        "k2" => Level::K2,
        _ => return None,
    })
}

impl ScriptRef {
    fn urns(&self) -> Vec<(&str, &str)> {
        self.imports.iter().map(|(i, u)| (i.as_str(), u.as_str())).collect()
    }

    fn compile(&self, text: &str) -> Result<Knf, KnfError> {
        let mut k = Knf::from_source(text, self.level, &self.urns())?;
        if let Some(s) = &self.semiring {
            k.manifest.semiring = s.clone();
        }
        Ok(k)
    }

    /// Inline kernel text, compiled at the declared level with a manifest
    /// built from the attributes.
    pub fn compile_inline(&self) -> Result<Knf, String> {
        let text = self.inline.as_deref().ok_or("script has neither src nor text")?;
        let k = self.compile(text).map_err(|e| format!("{e:?}"))?;
        if let Some(i) = &self.integrity {
            k.verify_integrity(i).map_err(|_| "integrity mismatch on inline script".to_string())?;
        }
        Ok(k)
    }

    /// Bytes fetched for `src`: a `.knf` container, or kernel text. The
    /// program hash is recomputed and compared with `integrity` when the
    /// attribute is present; a mismatch is fatal to the page.
    pub fn accept(&self, bytes: &[u8]) -> Result<Knf, String> {
        let k = if bytes.starts_with(gaze_knf::MAGIC) {
            Knf::decode(bytes).map_err(|e| format!("bad .knf: {e:?}"))?
        } else {
            let text = std::str::from_utf8(bytes).map_err(|_| "script is neither .knf nor UTF-8 text")?;
            self.compile(text).map_err(|e| format!("{e:?}"))?
        };
        if let Some(i) = &self.integrity {
            k.verify_integrity(i)
                .map_err(|_| format!("integrity mismatch for {}", self.src.as_deref().unwrap_or("script")))?;
        }
        Ok(k)
    }
}

/// An answer for the page: `chan!(args...)` in the next frame.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub class: Class,
    pub chan: Name,
    pub args: Vec<Norm>,
    /// Mint a name served outside the executive under this label, and
    /// deliver `("ok", "node", *name)` instead of `args` (shard sessions).
    pub bind: Option<String>,
}

impl Delivery {
    pub fn reply(class: Class, chan: Name, datum: Norm) -> Delivery {
        Delivery {
            class,
            chan,
            args: vec![datum],
            bind: None,
        }
    }
}

/// The external capabilities of one tab (`net`, `store`, `nav`, `shard`),
/// supplied by the shell. Replies are delivered in later frames.
pub trait Services {
    /// A request the page sent this frame.
    fn request(&mut self, req: CapRequest, out: &mut Vec<Delivery>);
    /// Asynchronous completions since the last call.
    fn poll(&mut self, out: &mut Vec<Delivery>);
}

/// Wakes the event loop from another thread: services call it when an
/// asynchronous answer arrives.
#[derive(Clone, Default)]
pub struct WakeHandle(Arc<Mutex<Option<Waker>>>);

impl WakeHandle {
    pub fn wake(&self) {
        if let Some(w) = self.0.lock().ok().and_then(|g| g.clone()) {
            w.wake();
        }
    }
    pub fn set_waker(&self, w: &Waker) {
        if let Ok(mut g) = self.0.lock() {
            if !g.as_ref().is_some_and(|o| o.will_wake(w)) {
                *g = Some(w.clone());
            }
        }
    }
}

/// A thread that wakes the loop at the next frame or timer.
struct Pacer {
    tx: Option<Sender<Instant>>,
    wake: WakeHandle,
}

impl Pacer {
    fn at(&mut self, when: Instant) {
        let wake = self.wake.clone();
        let tx = self.tx.get_or_insert_with(|| {
            let (tx, rx) = channel::<Instant>();
            let _ = std::thread::Builder::new()
                .name("gaze-pacer".into())
                .spawn(move || pacer_main(rx, wake));
            tx
        });
        if tx.send(when).is_err() {
            self.tx = None;
        }
    }
}

fn pacer_main(rx: Receiver<Instant>, wake: WakeHandle) {
    let mut next: Option<Instant> = None;
    loop {
        match next {
            None => match rx.recv() {
                Ok(t) => next = Some(t),
                Err(_) => return,
            },
            Some(t) => {
                let now = Instant::now();
                if t <= now {
                    wake.wake();
                    next = None;
                    continue;
                }
                match rx.recv_timeout(t - now) {
                    Ok(t2) => next = Some(t2.min(t)),
                    Err(RecvTimeoutError::Timeout) => {
                        wake.wake();
                        next = None;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }
}

/// Where a page is in its life.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageState {
    /// No f1r3lang: rendered as a document.
    Static,
    /// Scripts found; the shell is fetching and asking.
    Loading,
    Running,
    Failed(String),
}

pub struct RhoDocument {
    inner: Rc<RefCell<BaseDocument>>,
    tab: Option<TabExec<BlitzDom>>,
    services: Option<Box<dyn Services>>,
    state: PageState,
    epoch: Instant,
    last_frame: Option<u64>,
    last_hash: [u8; 32],
    foreground: bool,
    pending_input: bool,
    wake: WakeHandle,
    pacer: Pacer,
    console_seen: usize,
}

impl RhoDocument {
    pub fn from_html(html: &str, config: blitz_dom::DocumentConfig) -> RhoDocument {
        RhoDocument::from_base_document(crate::parse_html(html, config))
    }

    pub fn from_base_document(doc: BaseDocument) -> RhoDocument {
        let wake = WakeHandle::default();
        let mut d = RhoDocument {
            inner: Rc::new(RefCell::new(doc)),
            tab: None,
            services: None,
            state: PageState::Static,
            epoch: Instant::now(),
            last_frame: None,
            last_hash: [0; 32],
            foreground: true,
            pending_input: false,
            pacer: Pacer {
                tx: None,
                wake: wake.clone(),
            },
            wake,
            console_seen: 0,
        };
        if !d.scripts().is_empty() {
            d.state = PageState::Loading;
        }
        d
    }

    pub fn base(&self) -> Rc<RefCell<BaseDocument>> {
        Rc::clone(&self.inner)
    }

    /// The document's f1r3lang scripts, in document order.
    pub fn scripts(&self) -> Vec<ScriptRef> {
        self.collect().0
    }

    /// Whether the document has JavaScript scripts (which never run here).
    /// With no f1r3lang script, the shell offers a legacy tab.
    pub fn has_js_scripts(&self) -> bool {
        self.collect().1
    }

    fn collect(&self) -> (Vec<ScriptRef>, bool) {
        let doc = self.inner.borrow();
        let mut out = Vec::new();
        let mut js = false;
        let mut stack = vec![doc.root_node().id];
        while let Some(id) = stack.pop() {
            let Some(node) = doc.get_node(id) else { continue };
            if let Some(el) = node.element_data() {
                if el.name.local == local_name!("script") {
                    let ty = el.attr(local_name!("type")).unwrap_or("").trim().to_ascii_lowercase();
                    match ty.as_str() {
                        "application/f1r3lang" => {
                            let get = |k: &str| {
                                el.attrs()
                                    .iter()
                                    .find(|a| a.name.local.as_ref() == k)
                                    .map(|a| a.value.clone())
                            };
                            let level = get("level").as_deref().map(parse_level).unwrap_or(Some(Level::K1G));
                            let imports = get("imports")
                                .unwrap_or_default()
                                .split_whitespace()
                                .filter_map(|t| match t.split_once('=') {
                                    Some((i, u)) => Some((i.to_string(), u.to_string())),
                                    None => default_urn(t).map(|u| (t.to_string(), u.to_string())),
                                })
                                .collect();
                            let src = get("src");
                            let text = node.text_content();
                            out.push(ScriptRef {
                                inline: if src.is_none() { Some(text) } else { None },
                                src,
                                integrity: get("integrity"),
                                level: level.unwrap_or(Level::K1G),
                                imports,
                                semiring: get("semiring"),
                            });
                        }
                        "" | "text/javascript" | "application/javascript" | "module" => js = true,
                        _ => {}
                    }
                    continue;
                }
            }
            stack.extend(node.children.iter().rev().copied());
        }
        (out, js)
    }

    /// Run the page: the first script loads the executive, the rest join it
    /// in order. `policy` is the settled grant plan; `seed` comes from the
    /// OS and is recorded in the replay log.
    pub fn start(
        &mut self,
        knfs: &[Knf],
        seed: [u8; 32],
        policy: &dyn Fn(&str) -> bool,
        services: Box<dyn Services>,
    ) -> Result<(), LoadError> {
        let Some((first, rest)) = knfs.split_first() else {
            self.state = PageState::Static;
            return Ok(());
        };
        let backend = BlitzDom::new(Rc::clone(&self.inner));
        let run = || -> Result<TabExec<BlitzDom>, LoadError> {
            let mut tab = TabExec::load(first, backend, seed, policy)?;
            for k in rest {
                tab.add_script(k, policy)?;
            }
            Ok(tab)
        };
        match run() {
            Ok(tab) => {
                self.tab = Some(tab);
                self.services = Some(services);
                self.state = PageState::Running;
                self.epoch = Instant::now();
                self.pending_input = true; // run the first frame now
                self.wake.wake();
                Ok(())
            }
            Err(e) => {
                self.state = PageState::Failed(e.to_string());
                Err(e)
            }
        }
    }

    pub fn fail(&mut self, why: impl Into<String>) {
        self.tab = None;
        self.services = None;
        self.state = PageState::Failed(why.into());
    }

    pub fn state(&self) -> &PageState {
        &self.state
    }
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }
    pub fn tab(&self) -> Option<&TabExec<BlitzDom>> {
        self.tab.as_ref()
    }
    pub fn tab_mut(&mut self) -> Option<&mut TabExec<BlitzDom>> {
        self.tab.as_mut()
    }
    pub fn grants(&self) -> Vec<Grant> {
        self.tab.as_ref().map(|t| t.grants.clone()).unwrap_or_default()
    }
    /// Revoke a capability for this page (the shell has already dropped the
    /// broker's route).
    pub fn revoke(&mut self, urn: &str) {
        if let Some(t) = &mut self.tab {
            t.revoke(urn);
        }
    }
    /// Console lines since the last call.
    pub fn take_console(&mut self) -> Vec<(String, String)> {
        match &self.tab {
            Some(t) => {
                let new = t.console[self.console_seen.min(t.console.len())..].to_vec();
                self.console_seen = t.console.len();
                new
            }
            None => Vec::new(),
        }
    }
    /// Frames the page could not finish within its budget.
    pub fn stalled_frames(&self) -> u64 {
        self.tab.as_ref().map(|t| t.stalled_frames()).unwrap_or(0)
    }
    /// The user's "let it run": a larger per-frame budget.
    pub fn grant_budget(&mut self, steps: u64) {
        if let Some(t) = &mut self.tab {
            t.set_frame_budget(steps);
        }
    }
    pub fn set_foreground(&mut self, on: bool) {
        self.foreground = on;
        self.wake.wake();
    }
    /// The replay log so far, as `.gzlog` bytes.
    pub fn log_bytes(&self) -> Option<Vec<u8>> {
        self.tab.as_ref().map(|t| t.log.to_bytes())
    }
    pub fn title(&self) -> String {
        let doc = self.inner.borrow();
        doc.find_title_node().map(|n| n.text_content().trim().to_string()).unwrap_or_default()
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn drive(&mut self) -> bool {
        let Some(tab) = &mut self.tab else { return false };
        let mut out = Vec::new();
        if let Some(s) = &mut self.services {
            s.poll(&mut out);
        }
        let got_answers = !out.is_empty();
        for d in out.drain(..) {
            deliver(tab, d);
        }
        let now = self.epoch.elapsed().as_millis() as u64;
        let interval = if self.foreground { FOREGROUND_INTERVAL_MS } else { BACKGROUND_INTERVAL_MS };
        let due = tab.next_deadline().is_some_and(|t| t <= now);
        let want = tab.wants_frame() || due || self.pending_input || got_answers;
        let mut changed = false;
        if want && self.last_frame.is_none_or(|l| now >= l + interval) {
            let h = tab.frame(now);
            self.last_frame = Some(now);
            self.pending_input = false;
            changed = h != self.last_hash;
            self.last_hash = h;
            let reqs = tab.take_requests();
            if let Some(s) = &mut self.services {
                for r in reqs {
                    s.request(r, &mut out);
                }
            }
            for d in out.drain(..) {
                deliver(tab, d);
            }
        }
        // Schedule the next wake-up.
        let last = self.last_frame.unwrap_or(now);
        let next = if tab.wants_frame() || self.pending_input {
            Some(last + interval)
        } else {
            tab.next_deadline()
        };
        if let Some(t) = next {
            let at = self.epoch + Duration::from_millis(t.max(now));
            self.pacer.at(at);
        }
        changed
    }
}

fn deliver(tab: &mut TabExec<BlitzDom>, d: Delivery) {
    match d.bind {
        Some(label) => {
            let k = tab.bind_ext(&label);
            let v = Norm::tuple(vec![Norm::str("ok"), Norm::str("node"), Norm::eval(Name::Unforgeable(k))]);
            tab.deliver(d.class, d.chan, vec![v]);
        }
        None => tab.deliver(d.class, d.chan, d.args),
    }
}

impl Document for RhoDocument {
    fn inner(&self) -> DocGuard<'_> {
        DocGuard::RefCell(self.inner.borrow())
    }

    fn inner_mut(&mut self) -> DocGuardMut<'_> {
        DocGuardMut::RefCell(self.inner.borrow_mut())
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        let mut touched = false;
        match &mut self.tab {
            Some(tab) => {
                let handler = RhoEventHandler {
                    tab,
                    touched: &mut touched,
                };
                let mut driver = EventDriver::new(&mut self.inner, handler);
                driver.handle_ui_event(event);
            }
            None => {
                let mut driver = EventDriver::new(&mut self.inner, NoopEventHandler);
                driver.handle_ui_event(event);
            }
        }
        if touched {
            self.pending_input = true;
            self.wake.wake();
        }
    }

    fn poll(&mut self, task_context: Option<TaskContext>) -> bool {
        if let Some(cx) = &task_context {
            self.wake.set_waker(cx.waker());
        }
        let _ = self.now_ms();
        self.drive()
    }
}
