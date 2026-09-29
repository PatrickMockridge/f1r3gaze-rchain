//! The window: a Blitz document of its own (the chrome) that hosts each
//! tab's `RhoDocument` as a sub-document of a view element. The chrome is
//! plain HTML driven from Rust; making it a f1r3lang page with a privileged
//! capability is work package K1 (P4).

use crate::engine::{Engine, NavRequest, escape};
use crate::tab::{Stage, Tab};
use anyrender_vello::VelloWindowRenderer;
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, DocumentConfig, EventDriver, EventHandler, LocalName, NodeId, QualName, ns};
use blitz_shell::{BlitzApplication, BlitzShellProxy, ControlFlow, EventLoop, WindowConfig};
use blitz_traits::events::{DomEvent, DomEventData, EventState, UiEvent};
use gaze_dom_blitz::{RhoDocument, WakeHandle};
use keyboard_types::{Key, Modifiers};
use std::any::Any;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::task::Context as TaskContext;

const CSS: &str = r#"
html,body{margin:0;height:100%;font:13px system-ui,-apple-system,"Segoe UI",sans-serif;color:#1d2330;background:#fff}
body{display:flex;flex-direction:column}
#tabs{display:flex;background:#dfe3ea;padding:4px 4px 0;gap:2px;flex:none}
.tab{display:flex;align-items:center;gap:6px;padding:6px 10px;background:#c9ced8;border-radius:6px 6px 0 0;max-width:220px;overflow:hidden;white-space:nowrap}
.tab.on{background:#f6f7f9}
.tab .x{padding:0 4px;border-radius:3px;color:#667085}
#newtab{padding:6px 10px;color:#445}
#toolbar{display:flex;align-items:center;gap:6px;padding:6px;background:#f6f7f9;border-bottom:1px solid #d0d5dd;flex:none}
#toolbar button,.bar button,#panel button{font:inherit;padding:4px 10px;border:1px solid #c0c6d0;border-radius:5px;background:#fff}
#url{flex:1;font:inherit;padding:5px 8px;border:1px solid #c0c6d0;border-radius:5px}
.bar{display:flex;align-items:center;gap:8px;padding:8px 10px;background:#fff7d6;border-bottom:1px solid #e5d08a;flex:none}
.bar .text{flex:1}
#main{flex:1;position:relative;display:flex;min-height:0}
.view{flex:1;width:100%;height:100%;overflow:hidden}
.hidden{display:none}
#panel{flex:none;max-height:40%;overflow:auto;border-top:1px solid #d0d5dd;background:#fafbfc;padding:6px 10px;font:12px ui-monospace,Menlo,monospace}
#panel:empty{display:none}
#status{flex:none;padding:3px 10px;background:#f0f2f5;border-top:1px solid #d0d5dd;color:#556;font-size:12px;display:flex;gap:10px}
#status button{font:inherit;padding:0 6px}
.warn{color:#8a4b00}
"#;

const SHELL: &str = r#"<html><head><title>F1R3Gaze</title><style>__CSS__</style></head><body>
<div id="tabs"></div>
<div id="toolbar">
<button data-action="back" title="Back">&lt;</button><button data-action="fwd" title="Forward">&gt;</button><button data-action="reload" title="Reload">Reload</button>
<input id="url" type="text" value=""><button data-action="go">Go</button>
<button data-action="panel:grants">Grants</button><button data-action="panel:console">Console</button>
</div>
<div id="prompt"></div>
<div id="main"></div>
<div id="panel"></div>
<div id="status"></div>
</body></html>"#;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Back,
    Fwd,
    Reload,
    Go(Option<String>),
    NewTab,
    Close(u64),
    Select(u64),
    Answer(u64, u64, bool),
    Remember,
    Revoke(String),
    Panel(String),
    LetItRun,
    SaveLog,
    FocusUrl,
}

impl Action {
    fn parse(s: &str) -> Option<Action> {
        let (head, rest) = s.split_once(':').unwrap_or((s, ""));
        let num = |x: &str| x.parse::<u64>().ok();
        Some(match head {
            "back" => Action::Back,
            "fwd" => Action::Fwd,
            "reload" => Action::Reload,
            "go" => Action::Go(None),
            "newtab" => Action::NewTab,
            "close" => Action::Close(num(rest)?),
            "select" => Action::Select(num(rest)?),
            "allow" | "deny" => {
                let (t, p) = rest.split_once(':')?;
                Action::Answer(num(t)?, num(p)?, head == "allow")
            }
            "remember" => Action::Remember,
            "revoke" => Action::Revoke(rest.to_string()),
            "panel" => Action::Panel(rest.to_string()),
            "letitrun" => Action::LetItRun,
            "savelog" => Action::SaveLog,
            _ => return None,
        })
    }
}

fn attr_of(doc: &BaseDocument, id: NodeId, k: &str) -> Option<String> {
    doc.get_node(id)?.element_data()?.attrs().iter().find(|a| a.name.local.as_ref() == k).map(|a| a.value.clone())
}

struct ChromeHandler<'a> {
    actions: &'a mut Vec<Action>,
    url_node: Option<NodeId>,
}

impl EventHandler for ChromeHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, doc: &mut dyn Document, state: &mut EventState) {
        let d = doc.inner();
        match &event.data {
            DomEventData::Click(_) => {
                for id in chain {
                    if let Some(a) = attr_of(&d, *id, "data-action").and_then(|s| Action::parse(&s)) {
                        self.actions.push(a);
                        state.request_redraw();
                        break;
                    }
                }
            }
            DomEventData::KeyDown(k) => {
                let cmd = k.modifiers.contains(Modifiers::CONTROL) || k.modifiers.contains(Modifiers::META);
                if k.key == Key::Enter && Some(event.target) == self.url_node {
                    let v = d
                        .get_node(event.target)
                        .and_then(|n| n.element_data())
                        .and_then(|e| e.text_input_data())
                        .map(|t| t.editor.raw_text().to_string());
                    self.actions.push(Action::Go(v));
                    state.prevent_default();
                } else if cmd {
                    let a = match &k.key {
                        Key::Character(c) if c == "t" => Some(Action::NewTab),
                        Key::Character(c) if c == "r" => Some(Action::Reload),
                        Key::Character(c) if c == "l" => Some(Action::FocusUrl),
                        Key::Character(c) if c == "w" => Some(Action::Close(u64::MAX)),
                        _ => None,
                    };
                    if let Some(a) = a {
                        self.actions.push(a);
                        state.prevent_default();
                    }
                }
            }
            _ => {}
        }
    }
}

pub struct ChromeDocument {
    inner: BaseDocument,
    eng: Rc<Engine>,
    tabs: Vec<(Tab, NodeId)>,
    active: usize,
    next_id: u64,
    wake: WakeHandle,
    panel: String,
    remember: bool,
    consoles: BTreeMap<u64, Vec<(String, String)>>,
    rendered: BTreeMap<&'static str, String>,
    url_dirty: bool,
    title: String,
    saved: Option<String>,
}

fn qn(k: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(k))
}

impl ChromeDocument {
    pub fn new(eng: Rc<Engine>, url: &str) -> ChromeDocument {
        let html = SHELL.replace("__CSS__", CSS);
        let inner = gaze_dom_blitz::parse_html(&html, DocumentConfig::default());
        let mut c = ChromeDocument {
            inner,
            eng,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            wake: WakeHandle::default(),
            panel: String::new(),
            remember: false,
            consoles: BTreeMap::new(),
            rendered: BTreeMap::new(),
            url_dirty: true,
            title: String::new(),
            saved: None,
        };
        c.open_tab(url);
        c
    }

    fn id(&self, id: &str) -> Option<NodeId> {
        self.inner.get_element_by_id(id)
    }

    fn open_tab(&mut self, url: &str) {
        self.next_id += 1;
        let main = self.id("main").expect("chrome has #main");
        let view = {
            let mut m = self.inner.mutate();
            let v = m.create_element(
                QualName::new(None, ns!(html), LocalName::from("div")),
                vec![blitz_dom::Attribute {
                    name: qn("class"),
                    value: "view".into(),
                }],
            );
            m.append_children(main, &[v]);
            v
        };
        let mut tab = Tab::new(Rc::clone(&self.eng), self.next_id, self.wake.clone());
        tab.navigate(url, true);
        self.tabs.push((tab, view));
        self.select(self.tabs.len() - 1);
    }

    fn select(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.active = i;
        let views: Vec<(NodeId, bool)> = self.tabs.iter().enumerate().map(|(j, (_, v))| (*v, j == i)).collect();
        {
            let mut m = self.inner.mutate();
            for (v, on) in &views {
                m.set_attribute(*v, qn("class"), if *on { "view" } else { "view hidden" });
            }
        }
        for (v, on) in views {
            if let Some(r) = rho_mut(&mut self.inner, v) {
                r.set_foreground(on);
            }
        }
        self.url_dirty = true;
    }

    fn close(&mut self, id: u64) {
        let Some(i) = self.tabs.iter().position(|(t, _)| t.id == id || id == u64::MAX && t.id == self.tabs[self.active].0.id) else { return };
        let (mut tab, view) = self.tabs.remove(i);
        tab.close();
        self.inner.remove_sub_document(view);
        {
            let mut m = self.inner.mutate();
            m.remove_and_drop_node(view);
        }
        self.consoles.remove(&tab.id);
        if self.tabs.is_empty() {
            let home = self.eng.settings.home.clone();
            self.open_tab(&home);
        } else {
            self.select(self.active.min(self.tabs.len() - 1));
        }
    }

    fn act(&mut self, a: Action) {
        let i = self.active;
        match a {
            Action::Back => {
                self.tabs[i].0.back();
                self.url_dirty = true;
            }
            Action::Fwd => {
                self.tabs[i].0.forward();
                self.url_dirty = true;
            }
            Action::Reload => self.tabs[i].0.reload(),
            Action::Go(v) => {
                let v = v.or_else(|| {
                    let n = self.id("url")?;
                    self.inner.get_node(n)?.element_data()?.text_input_data().map(|t| t.editor.raw_text().to_string())
                });
                if let Some(u) = v.filter(|u| !u.trim().is_empty()) {
                    self.tabs[i].0.navigate(&u, true);
                    self.url_dirty = true;
                }
            }
            Action::NewTab => {
                let home = self.eng.settings.home.clone();
                self.open_tab(&home);
            }
            Action::Close(id) => self.close(id),
            Action::Select(id) => {
                if let Some(j) = self.tabs.iter().position(|(t, _)| t.id == id) {
                    self.select(j);
                }
            }
            Action::Answer(tid, pid, yes) => {
                let remember = self.remember;
                if let Some((t, _)) = self.tabs.iter_mut().find(|(t, _)| t.id == tid) {
                    t.answer(pid, yes, remember);
                }
            }
            Action::Remember => self.remember = !self.remember,
            Action::Revoke(urn) => {
                let view = self.tabs[i].1;
                if let Some(r) = rho_mut(&mut self.inner, view) {
                    self.tabs[i].0.revoke(&urn, r);
                }
            }
            Action::Panel(p) => self.panel = if self.panel == p { String::new() } else { p },
            Action::LetItRun => {
                let view = self.tabs[i].1;
                if let Some(r) = rho_mut(&mut self.inner, view) {
                    r.grant_budget(200_000);
                }
            }
            Action::SaveLog => {
                let view = self.tabs[i].1;
                let tid = self.tabs[i].0.id;
                if let Some(bytes) = rho_mut(&mut self.inner, view).and_then(|r| r.log_bytes()) {
                    let p = self.eng.dir.join("logs").join(format!("tab-{tid}-{}.gzlog", std::process::id()));
                    let _ = std::fs::create_dir_all(p.parent().expect("has parent"));
                    self.saved = Some(match std::fs::write(&p, bytes) {
                        Ok(()) => format!("Replay log saved to {}", p.display()),
                        Err(e) => format!("Could not save the log: {e}"),
                    });
                }
            }
            Action::FocusUrl => {
                if let Some(n) = self.id("url") {
                    self.inner.set_focus_to(n);
                }
            }
        }
        self.wake.wake();
    }

    fn set_html(&mut self, id: &'static str, html: String) -> bool {
        if self.rendered.get(id) == Some(&html) {
            return false;
        }
        if let Some(n) = self.id(id) {
            let mut m = self.inner.mutate();
            m.set_inner_html(n, &html);
        }
        self.rendered.insert(id, html);
        true
    }

    fn render(&mut self) -> bool {
        let mut changed = false;
        // Tab strip.
        let mut tabs = String::new();
        for (j, (t, _)) in self.tabs.iter().enumerate() {
            let title = if t.title.is_empty() { t.url.as_str() } else { t.title.as_str() };
            tabs.push_str(&format!(
                r#"<div class="tab{}" data-action="select:{}"><span>{}</span><span class="x" data-action="close:{}">x</span></div>"#,
                if j == self.active { " on" } else { "" },
                t.id,
                escape(&title.chars().take(40).collect::<String>()),
                t.id
            ));
        }
        tabs.push_str(r#"<div id="newtab" data-action="newtab">+</div>"#);
        changed |= self.set_html("tabs", tabs);

        let view = self.tabs[self.active].1;
        let (tid, first_prompt, status_text, notice, url, title, running) = {
            let tab = &self.tabs[self.active].0;
            (
                tab.id,
                tab.prompts().into_iter().next(),
                tab.status(),
                tab.notice.clone(),
                tab.url.clone(),
                tab.title.clone(),
                tab.stage == Stage::Running,
            )
        };
        // Prompt bar: the first pending question of the active tab.
        let prompt = match first_prompt {
            Some((pid, text)) => format!(
                r#"<div class="bar"><span class="text">{}</span><label data-action="remember">[{}] remember</label><button data-action="deny:{}:{}">Deny</button><button data-action="allow:{}:{}">Allow</button></div>"#,
                escape(&text),
                if self.remember { "x" } else { " " },
                tid,
                pid,
                tid,
                pid
            ),
            None => String::new(),
        };
        changed |= self.set_html("prompt", prompt);

        // Panels.
        let (grants, stalled) = match rho_mut(&mut self.inner, view) {
            Some(r) => (r.grants(), r.stalled_frames()),
            None => (Vec::new(), 0),
        };
        let panel = match self.panel.as_str() {
            "grants" => {
                let mut s = String::from("<b>Capabilities of this page</b><br>");
                if grants.is_empty() {
                    s.push_str("none (the page is not running)");
                }
                for g in &grants {
                    s.push_str(&format!(
                        "{} = {} : {} {}<br>",
                        escape(&g.ident),
                        escape(&g.urn),
                        if g.granted { "granted" } else { "denied" },
                        if g.granted { format!(r#"<button data-action="revoke:{}">Revoke</button>"#, escape(&g.urn)) } else { String::new() }
                    ));
                }
                s
            }
            "console" => {
                let mut s = String::from(r#"<b>Console</b> <button data-action="savelog">Save replay log</button><br>"#);
                for (lvl, line) in self.consoles.get(&tid).map(|v| v.as_slice()).unwrap_or(&[]).iter().rev().take(200) {
                    s.push_str(&format!("[{}] {}<br>", escape(lvl), escape(line)));
                }
                s
            }
            _ => String::new(),
        };
        changed |= self.set_html("panel", panel);

        let mut status = format!("<span>{}</span>", escape(&status_text));
        if let Some(n) = notice {
            status.push_str(&format!(r#"<span class="warn">{}</span>"#, escape(&n)));
        }
        if stalled > 0 && running {
            status.push_str(r#"<span class="warn">This page used its whole budget in some frames.</span><button data-action="letitrun">Let it run</button>"#);
        }
        if let Some(s) = &self.saved {
            status.push_str(&format!("<span>{}</span>", escape(s)));
        }
        changed |= self.set_html("status", status);

        if self.url_dirty {
            self.url_dirty = false;
            if let Some(n) = self.id("url") {
                let mut m = self.inner.mutate();
                m.set_attribute(n, qn("value"), &url);
            }
            changed = true;
        }
        let wt = if title.is_empty() { "F1R3Gaze".to_string() } else { format!("{title} — F1R3Gaze") };
        if wt != self.title {
            self.inner.shell_provider.set_window_title(wt.clone());
            self.title = wt;
        }
        changed
    }
}

fn rho_mut(doc: &mut BaseDocument, view: NodeId) -> Option<&mut RhoDocument> {
    let d: &mut dyn Document = doc.get_node_mut(view)?.subdoc_mut()?;
    let a: &mut dyn Any = d;
    a.downcast_mut::<RhoDocument>()
}

impl Document for ChromeDocument {
    fn inner(&self) -> DocGuard<'_> {
        DocGuard::Ref(&self.inner)
    }
    fn inner_mut(&mut self) -> DocGuardMut<'_> {
        DocGuardMut::Ref(&mut self.inner)
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        let mut actions = Vec::new();
        let url_node = self.id("url");
        {
            let handler = ChromeHandler {
                actions: &mut actions,
                url_node,
            };
            let mut driver = EventDriver::new(&mut self.inner, handler);
            driver.handle_ui_event(event);
        }
        for a in actions {
            self.act(a);
        }
    }

    fn poll(&mut self, cx: Option<TaskContext>) -> bool {
        if let Some(cx) = &cx {
            self.wake.set_waker(cx.waker());
        }
        let waker = cx.as_ref().map(|c| c.waker().clone());
        let mut changed = self.inner.poll_subdocuments(waker.as_ref());
        for i in 0..self.tabs.len() {
            if let Some(doc) = self.tabs[i].0.pump() {
                let view = self.tabs[i].1;
                self.inner.remove_sub_document(view);
                self.inner.set_sub_document(view, Box::new(doc));
                if let Some(r) = rho_mut(&mut self.inner, view) {
                    r.set_foreground(i == self.active);
                }
                if i == self.active {
                    self.url_dirty = true;
                }
                changed = true;
            }
            let view = self.tabs[i].1;
            let tid = self.tabs[i].0.id;
            if let Some(r) = rho_mut(&mut self.inner, view) {
                self.tabs[i].0.pump_doc(r);
                let lines = r.take_console();
                if !lines.is_empty() {
                    self.consoles.entry(tid).or_default().extend(lines);
                }
            }
            for n in self.tabs[i].0.take_nav() {
                match n {
                    NavRequest::Go(u) => self.tabs[i].0.navigate(&u, true),
                    NavRequest::Replace(u) => self.tabs[i].0.navigate(&u, false),
                    NavRequest::Back => {
                        self.tabs[i].0.back();
                    }
                    NavRequest::Open(u) => self.open_tab(&u),
                }
                if i == self.active {
                    self.url_dirty = true;
                }
            }
        }
        changed |= self.render();
        changed
    }
}

/// Open the browser window and run until it closes.
pub fn launch(eng: Rc<Engine>, url: &str) -> Result<(), String> {
    // Built here rather than by `create_default_event_loop`, which panics
    // when there is no display; the user gets a message instead.
    let event_loop = EventLoop::builder()
        .build()
        .map_err(|e| format!("cannot open a window ({e}); use --headless to run pages without one"))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let (proxy, rx) = BlitzShellProxy::new(event_loop.create_proxy());
    let mut app = BlitzApplication::new(proxy, rx);
    let chrome = ChromeDocument::new(eng, url);
    app.add_window(WindowConfig::new(Box::new(chrome), VelloWindowRenderer::new()));
    event_loop.run_app(app).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_parse() {
        assert_eq!(Action::parse("allow:3:77"), Some(Action::Answer(3, 77, true)));
        assert_eq!(Action::parse("revoke:rho:gaze:net"), Some(Action::Revoke("rho:gaze:net".into())));
        assert_eq!(Action::parse("select:x"), None);
    }
}
