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
#wallet{flex:none;max-height:45%;overflow:auto;border-top:1px solid #d0d5dd;background:#fafbfc;padding:8px 10px}
#wallet .row{display:flex;gap:6px;align-items:center;margin:4px 0}
#wallet input{font:inherit;padding:4px 6px;border:1px solid #c0c6d0;border-radius:5px}
#wallet button{font:inherit;padding:3px 9px;border:1px solid #c0c6d0;border-radius:5px;background:#fff}
#wallet .addr{font:12px ui-monospace,Menlo,monospace}
#wallet .active{font-weight:600}
#walletmsg{color:#445;margin-top:4px}
.muted{color:#667085}
#status{flex:none;padding:3px 10px;background:#f0f2f5;border-top:1px solid #d0d5dd;color:#556;font-size:12px;display:flex;gap:10px}
#status button{font:inherit;padding:0 6px}
.warn{color:#8a4b00}
"#;

const SHELL: &str = r#"<html><head><title>F1R3Gaze</title><style>__CSS__</style></head><body>
<div id="tabs"></div>
<div id="toolbar">
<button data-action="back" title="Back">&lt;</button><button data-action="fwd" title="Forward">&gt;</button><button data-action="reload" title="Reload">Reload</button>
<input id="url" type="text" value=""><button data-action="go">Go</button>
<button data-action="panel:grants">Grants</button><button data-action="panel:console">Console</button><button data-action="panel:wallet">Wallet</button>
</div>
<div id="prompt"></div>
<div id="main"></div>
<div id="panel"></div>
<div id="wallet" class="hidden">
<div id="walletlist"></div>
<div class="row"><button data-action="wallet:new">New wallet</button>
<span class="muted">or paste a wallet file (from F1R3Sky) or hex key:</span>
<input id="wallet-import" type="text" style="flex:1"><button data-action="wallet:import">Import</button></div>
<div class="row"><b>Send</b> <span class="muted">to</span> <input id="wallet-to" type="text" style="flex:2">
<span class="muted">amount</span> <input id="wallet-amount" type="text" style="width:80px">
<span class="muted">note</span> <input id="wallet-desc" type="text" style="flex:1"><button data-action="wallet:send">Send…</button></div>
<div id="walletmsg"></div>
</div>
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
    Wallet(String),
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
            "wallet" => Action::Wallet(rest.to_string()),
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
    wallet: std::sync::Arc<std::sync::Mutex<WalletView>>,
}

/// What the wallet panel shows; filled by background work.
#[derive(Default)]
struct WalletView {
    balances: BTreeMap<String, String>,
    message: String,
    /// A send or a removal waiting for its confirming click.
    confirm: Option<(String, String)>,
    pending_send: Option<(String, i64, Option<String>)>,
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
            wallet: Default::default(),
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
            Action::Panel(p) => {
                self.panel = if self.panel == p { String::new() } else { p };
                if self.panel == "wallet" {
                    self.refresh_balances();
                }
            }
            Action::Wallet(w) => self.wallet_action(&w),
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

    fn input_value(&self, id: &str) -> String {
        self.id(id)
            .and_then(|n| self.inner.get_node(n))
            .and_then(|n| n.element_data())
            .and_then(|e| e.text_input_data())
            .map(|t| t.editor.raw_text().trim().to_string())
            .unwrap_or_default()
    }

    fn clear_input(&mut self, id: &str) {
        if let Some(n) = self.id(id) {
            let mut m = self.inner.mutate();
            m.set_attribute(n, qn("value"), "");
        }
    }

    fn say(&self, msg: impl Into<String>) {
        if let Ok(mut v) = self.wallet.lock() {
            v.message = msg.into();
        }
    }

    fn refresh_balances(&self) {
        if self.eng.wallets.embers.is_none() {
            self.say("Balances and transfers need an Embers service: set embers_api in settings.conf.");
            return;
        }
        let (w, view, wake) = (self.eng.wallets.clone(), self.wallet.clone(), self.wake.clone());
        self.eng.pool.spawn(move || {
            for (e, _) in w.list() {
                let b = match w.state(&e.address) {
                    Ok(s) => s.balance.to_string(),
                    Err(_) => "?".into(),
                };
                if let Ok(mut v) = view.lock() {
                    v.balances.insert(e.address.as_str().to_string(), b);
                }
            }
            wake.wake();
        });
    }

    fn wallet_action(&mut self, w: &str) {
        use gaze_wallet::Address;
        let (verb, arg) = w.split_once(':').unwrap_or((w, ""));
        let wallets = self.eng.wallets.clone();
        match verb {
            "new" => match wallets.create("") {
                Ok(a) => self.say(format!("Created {a}. Export it (and keep the file safe) before funding it.")),
                Err(e) => self.say(e),
            },
            "import" => {
                let text = self.input_value("wallet-import");
                match wallets.import(&text, "imported") {
                    Ok(a) => {
                        self.clear_input("wallet-import");
                        self.say(format!("Imported {a}."));
                    }
                    Err(e) => self.say(format!("Could not import: {e}")),
                }
            }
            "use" => match Address::parse(arg).and_then(|a| wallets.set_active(&a)) {
                Ok(()) => self.say("This wallet now pays for deploys."),
                Err(e) => self.say(e),
            },
            "export" => {
                let r = Address::parse(arg).and_then(|a| {
                    let body = wallets.export(&a)?;
                    let dir = self.eng.dir.join("exports");
                    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                    let p = dir.join(format!("{a}.json"));
                    std::fs::write(&p, body).map_err(|e| e.to_string())?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
                    }
                    Ok(p)
                });
                match r {
                    Ok(p) => self.say(format!("Wallet file written to {} (F1R3Sky can import it).", p.display())),
                    Err(e) => self.say(e),
                }
            }
            "remove" => {
                if let Ok(mut v) = self.wallet.lock() {
                    v.confirm = Some(("remove".into(), arg.to_string()));
                    v.message = format!("Remove {arg}? Its key will be deleted. Export it first if it holds funds.");
                }
            }
            "send" => {
                let to = self.input_value("wallet-to");
                let amount = self.input_value("wallet-amount");
                let desc = self.input_value("wallet-desc");
                let parsed = Address::parse(&to).and_then(|t| {
                    let n: i64 = amount.parse().map_err(|_| "the amount must be a whole number".to_string())?;
                    if n <= 0 {
                        return Err("the amount must be positive".into());
                    }
                    Ok((t, n))
                });
                match (parsed, wallets.active()) {
                    (Ok((t, n)), Some(from)) => {
                        if let Ok(mut v) = self.wallet.lock() {
                            v.pending_send = Some((t.as_str().to_string(), n, Some(desc).filter(|d| !d.is_empty())));
                            v.confirm = Some(("send".into(), String::new()));
                            v.message = format!("Send {n} from {} to {t}?", gaze_shard::bridge::short(from.as_str()));
                        }
                    }
                    (Err(e), _) => self.say(e),
                    (_, None) => self.say("No wallet to send from."),
                }
            }
            "confirm" => {
                let c = self.wallet.lock().ok().and_then(|mut v| v.confirm.take());
                match c {
                    Some((kind, a)) if kind == "remove" => match Address::parse(&a).and_then(|a| wallets.remove(&a)) {
                        Ok(()) => self.say("Removed."),
                        Err(e) => self.say(e),
                    },
                    Some((kind, _)) if kind == "send" => {
                        let p = self.wallet.lock().ok().and_then(|mut v| v.pending_send.take());
                        let (Some((to, n, desc)), Some(from)) = (p, wallets.active()) else { return };
                        self.say("Sending: checking the prepared contract, then signing…");
                        let (view, wake) = (self.wallet.clone(), self.wake.clone());
                        self.eng.pool.spawn(move || {
                            let r = Address::parse(&to).and_then(|t| wallets.transfer(&from, &t, n, desc.as_deref()));
                            if let Ok(mut v) = view.lock() {
                                v.message = match r {
                                    Ok(id) => format!("Sent. Deploy {}.", gaze_shard::bridge::short(&id)),
                                    Err(e) => format!("Not sent: {e}"),
                                };
                            }
                            wake.wake();
                        });
                        self.clear_input("wallet-amount");
                    }
                    _ => {}
                }
                self.refresh_balances();
            }
            "cancel" => {
                if let Ok(mut v) = self.wallet.lock() {
                    v.confirm = None;
                    v.pending_send = None;
                    v.message = "Cancelled.".into();
                }
            }
            _ => {}
        }
    }

    fn render_wallet(&mut self) -> bool {
        let mut changed = false;
        if let Some(n) = self.id("wallet") {
            let want = if self.panel == "wallet" { "" } else { "hidden" };
            if attr_of(&self.inner, n, "class").unwrap_or_default() != want {
                let mut m = self.inner.mutate();
                m.set_attribute(n, qn("class"), want);
                changed = true;
            }
        }
        if self.panel != "wallet" {
            return changed;
        }
        let (balances, message, confirming) = match self.wallet.lock() {
            Ok(v) => (v.balances.clone(), v.message.clone(), v.confirm.is_some()),
            Err(_) => return changed,
        };
        let mut list = String::from("<b>Wallets</b> <span class=\"muted\">(the active wallet pays for every deploy)</span><br>");
        let ws = self.eng.wallets.list();
        if ws.is_empty() {
            list.push_str("No wallets yet. Create one, or import the file F1R3Sky saved.<br>");
        }
        for (e, active) in ws {
            let a = e.address.as_str();
            list.push_str(&format!(
                r#"<div class="row"><span class="addr{}">{}{}</span><span>{}</span><span>{}</span>{}<button data-action="wallet:export:{a}">Export</button><button data-action="wallet:remove:{a}">Remove</button></div>"#,
                if active { " active" } else { "" },
                if active { "● " } else { "" },
                escape(a),
                escape(&e.label),
                balances.get(a).map(|b| format!("balance {}", escape(b))).unwrap_or_default(),
                if active { String::new() } else { format!(r#"<button data-action="wallet:use:{a}">Pay with this</button>"#) },
            ));
        }
        changed |= self.set_html("walletlist", list);
        let mut msg = escape(&message);
        if confirming {
            msg.push_str(r#" <button data-action="wallet:confirm">Confirm</button><button data-action="wallet:cancel">Cancel</button>"#);
        }
        changed |= self.set_html("walletmsg", msg);
        changed
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
            _ => String::new(), // "wallet" has its own section
        };
        changed |= self.set_html("panel", panel);
        changed |= self.render_wallet();

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
        assert_eq!(Action::parse("wallet:use:1111abc"), Some(Action::Wallet("use:1111abc".into())));
    }
}
