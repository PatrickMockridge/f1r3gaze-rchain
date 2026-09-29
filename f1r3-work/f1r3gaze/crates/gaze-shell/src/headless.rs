//! The browser without a window: load a page, run it, report the committed
//! document. For CI smoke tests of published sites, and for the gateway's
//! eventual projection (spec §12).

use crate::engine::{Engine, NavRequest};
use crate::tab::{Stage, Tab};
use gaze_dom_blitz::{BlitzDom, RhoDocument, WakeHandle};
use gaze_dom_core::DomBackend;
use k1ndl1ng_norm::Norm;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Answer every prompt "yes" (otherwise "no").
    pub allow: bool,
    /// Selectors to click, in order, once the page has settled.
    pub clicks: Vec<String>,
    pub timeout: Duration,
    /// Keep running at least this long (replies from the shard can take
    /// seconds: a deploy answers once when sent and again when finalized).
    pub wait: Duration,
}

#[derive(Debug)]
pub struct Report {
    pub url: String,
    pub stage: Stage,
    pub title: String,
    pub document: String,
    pub console: Vec<(String, String)>,
    pub prompts: Vec<String>,
    pub notice: Option<String>,
    pub log: Option<Vec<u8>>,
}

pub fn run(eng: Rc<Engine>, url: &str, opts: &Options) -> Report {
    let wake = WakeHandle::default();
    let mut tab = Tab::new(Rc::clone(&eng), 1, wake);
    tab.navigate(url, true);
    let mut doc: Option<RhoDocument> = None;
    let mut console = Vec::new();
    let mut prompts = Vec::new();
    let mut clicks = opts.clicks.clone().into_iter();
    let started = Instant::now();
    let mut quiet_since: Option<Instant> = None;
    let timeout = if opts.timeout.is_zero() { Duration::from_secs(20) } else { opts.timeout };
    while started.elapsed() < timeout {
        if let Some(d) = tab.pump() {
            doc = Some(d);
        }
        let Some(d) = doc.as_mut() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        tab.pump_doc(d);
        for (id, text) in tab.prompts() {
            prompts.push(text);
            tab.answer(id, opts.allow, false);
        }
        use blitz_dom::Document;
        d.poll(None);
        console.extend(d.take_console());
        let navs = tab.take_nav();
        let busy = d.tab().is_some_and(|t| t.wants_frame() || t.next_deadline().is_some());
        if let Some(NavRequest::Go(u) | NavRequest::Replace(u)) = navs.into_iter().find(|n| !matches!(n, NavRequest::Back | NavRequest::Open(_))) {
            tab.navigate(&u, true);
            doc = None;
            quiet_since = None;
            continue;
        }
        let settled = match tab.stage {
            Stage::Static | Stage::Failed(_) => true,
            Stage::Running => !busy,
            _ => false,
        };
        if settled && started.elapsed() >= opts.wait {
            let q = *quiet_since.get_or_insert_with(Instant::now);
            if q.elapsed() > Duration::from_millis(150) {
                match clicks.next() {
                    Some(sel) => {
                        if let Some(t) = d.tab_mut() {
                            let root = t.dom.backend.root();
                            if let Some(n) = t.dom.backend.query(root, &sel, false).ok().and_then(|v| v.first().copied()) {
                                t.dispatch(n, "click", vec![("x".into(), Norm::int(0)), ("y".into(), Norm::int(0))]);
                            }
                        }
                        quiet_since = None;
                    }
                    None => break,
                }
            }
        } else {
            quiet_since = None;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    let document = doc.as_ref().map(|d| BlitzDom::new(d.base()).serialize()).unwrap_or_default();
    Report {
        url: tab.url.clone(),
        stage: tab.stage.clone(),
        title: tab.title.clone(),
        document,
        console,
        prompts,
        notice: tab.notice.clone(),
        log: doc.as_ref().and_then(|d| d.log_bytes()),
    }
}
