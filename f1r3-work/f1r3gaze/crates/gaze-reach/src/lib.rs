//! `gaze-reach` — the reach tier (spec §12): the tab executive and the DOM
//! protocol compiled into one wasm module, run by `gaze-reach.js` in any
//! modern browser. The protocol engine and the executive are the same code
//! as the native browser; only the [`DomBackend`] differs — here it is the
//! host page's DOM, reached through [`Host`].
//!
//! Export conventions follow `f1r3x-wasm`: no `wasm-bindgen`; every call
//! takes and returns integers, and bytes move through buffers the module
//! allocates (`gaze_alloc`) and results the caller copies out
//! (`gaze_result_len` / `gaze_result_read`). Tabs live behind `u32` handles.
//! The unsafe code is confined to the `wasm` module's byte copies.
//!
//! In v1 the reach tier offers `doc`, `log`, `clock`, `rand`, `net` (same
//! origin, integrity-checked here) , `store` (IndexedDB) and `nav`. `shard`
//! is a dead channel: a stock browser has no key custody a page cannot reach.

pub mod json;

use gaze_dom_core::{ClassOp, DomBackend, Frag, Pos, Write};
use gaze_exec::{Class, TabExec};
use gaze_knf::Knf;
use json::J;
use k1ndl1ng_norm::hash::blake2b_256;
use k1ndl1ng_norm::{CollKind, Name, Node, Norm};
use k1ndl1ng_parse::Level;
use std::collections::BTreeMap;

/// The host page's DOM, as the module sees it: one JSON command, one JSON
/// answer. `gaze-reach.js` implements it over the real DOM; tests implement
/// it over `MemDom`.
pub trait Host {
    fn call(&self, cmd: &J) -> J;
}

pub struct HostDom<H: Host> {
    pub host: H,
}

fn ids(j: &J) -> Vec<u32> {
    j.arr().map(|v| v.iter().filter_map(|x| x.int()).map(|i| i as u32).collect()).unwrap_or_default()
}

fn split_list(sel: &str) -> Vec<&str> {
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0);
    let mut out = Vec::new();
    for (i, c) in sel.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(&sel[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&sel[start..]);
    out
}

/// Confine a selector to a scope (see `gaze-dom-blitz`): the host browser's
/// `querySelectorAll` would otherwise match ancestors above the scope.
pub fn scope_selector(sel: &str) -> String {
    split_list(sel).into_iter().map(|p| format!(":scope {}", p.trim())).collect::<Vec<_>>().join(", ")
}

fn frag_j(f: &Frag) -> J {
    match f {
        Frag::Text(t) => J::obj(vec![("text", J::s(t))]),
        Frag::Html(h) => J::obj(vec![("html", J::s(h))]),
        Frag::El {
            tag,
            attrs,
            children,
            reference,
        } => J::obj(vec![
            ("el", J::s(tag)),
            ("attrs", J::Arr(attrs.iter().map(|(k, v)| J::Arr(vec![J::s(k), J::s(v)])).collect())),
            ("children", J::Arr(children.iter().map(frag_j).collect())),
            ("ref", reference.as_ref().map(|r| J::s(r)).unwrap_or(J::Null)),
        ]),
    }
}

pub fn write_j(w: &Write<u32>) -> J {
    let n = |x: &u32| J::Int(*x as i64);
    match w {
        Write::SetAttr(x, k, v) => J::Arr(vec![J::s("setAttr"), n(x), J::s(k), J::s(v)]),
        Write::RemoveAttr(x, k) => J::Arr(vec![J::s("removeAttr"), n(x), J::s(k)]),
        Write::SetText(x, t) => J::Arr(vec![J::s("setText"), n(x), J::s(t)]),
        Write::SetValue(x, v) => J::Arr(vec![J::s("setValue"), n(x), J::s(v)]),
        Write::Class(x, op, c) => J::Arr(vec![
            J::s("class"),
            n(x),
            J::s(match op {
                ClassOp::Add => "add",
                ClassOp::Remove => "remove",
                ClassOp::Toggle => "toggle",
            }),
            J::s(c),
        ]),
        Write::Style(x, k, v) => J::Arr(vec![J::s("style"), n(x), J::s(k), v.as_ref().map(|v| J::s(v)).unwrap_or(J::Null)]),
        Write::Insert(x, p, f) => J::Arr(vec![
            J::s("insert"),
            n(x),
            J::s(match p {
                Pos::Append => "append",
                Pos::Prepend => "prepend",
                Pos::Before => "before",
                Pos::After => "after",
                Pos::ReplaceWith => "replaceWith",
            }),
            frag_j(f),
        ]),
        Write::SetHtml(x, h) => J::Arr(vec![J::s("setHTML"), n(x), J::s(h)]),
        Write::Remove(x) => J::Arr(vec![J::s("remove"), n(x)]),
        Write::Focus(x) => J::Arr(vec![J::s("focus"), n(x)]),
        Write::Blur(x) => J::Arr(vec![J::s("blur"), n(x)]),
    }
}

impl<H: Host> HostDom<H> {
    fn q(&self, op: &str, args: Vec<(&str, J)>) -> J {
        let mut kv = vec![("op", J::s(op))];
        kv.extend(args);
        self.host.call(&J::obj(kv))
    }
}

fn nj(n: u32) -> J {
    J::Int(n as i64)
}

fn has_combinator(sel: &str) -> bool {
    let (mut depth, mut quote) = (0i32, None::<char>);
    for c in sel.trim().chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ' ' | '>' | '+' | '~') if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

impl<H: Host> DomBackend for HostDom<H> {
    type Node = u32;
    fn root(&self) -> u32 {
        self.q("root", vec![]).int().unwrap_or(0) as u32
    }
    fn parent(&self, n: u32) -> Option<u32> {
        self.q("parent", vec![("n", nj(n))]).int().map(|x| x as u32)
    }
    fn children(&self, n: u32) -> Vec<u32> {
        ids(&self.q("children", vec![("n", nj(n))]))
    }
    fn attached(&self, n: u32) -> bool {
        self.q("attached", vec![("n", nj(n))]) == J::Bool(true)
    }
    fn is_element(&self, n: u32) -> bool {
        self.q("isElement", vec![("n", nj(n))]) == J::Bool(true)
    }
    fn attr(&self, n: u32, k: &str) -> Option<String> {
        self.q("attr", vec![("n", nj(n)), ("k", J::s(k))]).str().map(str::to_string)
    }
    fn text(&self, n: u32) -> String {
        self.q("text", vec![("n", nj(n))]).str().unwrap_or("").to_string()
    }
    fn query(&self, scope: u32, sel: &str, all: bool) -> Result<Vec<u32>, String> {
        let s = if scope == self.root() { sel.to_string() } else { scope_selector(sel) };
        let r = self.q("query", vec![("n", nj(scope)), ("sel", J::s(&s)), ("all", J::Bool(all))]);
        match r.get("err") {
            Some(e) => Err(e.str().unwrap_or("selector").to_string()),
            None => Ok(ids(&r)),
        }
    }
    fn closest(&self, n: u32, sel: &str, scope: u32) -> Result<Option<u32>, String> {
        // Confined as in the Blitz backend.
        let whole = scope == self.root();
        let inside: std::collections::BTreeSet<u32> =
            if whole { Default::default() } else { self.query(scope, sel, true)?.into_iter().collect() };
        let matches = |c: u32| -> Result<bool, String> {
            match self.q("matches", vec![("n", nj(c)), ("sel", J::s(sel))]) {
                J::Bool(b) => Ok(b),
                r => Err(r.get("err").and_then(|e| e.str()).unwrap_or("selector").to_string()),
            }
        };
        let mut cur = Some(n);
        while let Some(c) = cur {
            let hit = if whole {
                matches(c)?
            } else if c == scope {
                !has_combinator(sel) && matches(c)?
            } else {
                inside.contains(&c)
            };
            if hit {
                return Ok(Some(c));
            }
            if c == scope {
                break;
            }
            cur = self.parent(c);
        }
        Ok(None)
    }
    fn rect(&self, n: u32) -> Option<[i64; 4]> {
        let v = self.q("rect", vec![("n", nj(n))]);
        let a = v.arr()?;
        (a.len() == 4).then(|| [a[0].int().unwrap_or(0), a[1].int().unwrap_or(0), a[2].int().unwrap_or(0), a[3].int().unwrap_or(0)])
    }
    fn apply(&mut self, w: &Write<u32>) -> Vec<(String, u32)> {
        self.apply_batch(std::slice::from_ref(w)).pop().flatten().unwrap_or_default()
    }
    fn apply_batch(&mut self, ws: &[Write<u32>]) -> Vec<Option<Vec<(String, u32)>>> {
        let r = self.q("apply", vec![("writes", J::Arr(ws.iter().map(write_j).collect()))]);
        let res = r.arr().map(|v| v.to_vec()).unwrap_or_default();
        (0..ws.len())
            .map(|i| match res.get(i) {
                Some(J::Arr(refs)) => Some(
                    refs.iter()
                        .filter_map(|p| {
                            let a = p.arr()?;
                            Some((a.first()?.str()?.to_string(), a.get(1)?.int()? as u32))
                        })
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    }
    fn serialize(&self) -> String {
        self.q("serialize", vec![]).str().unwrap_or("").to_string()
    }
}

// ---------------------------------------------------------------------------
// Terms from the host.

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}
fn hexs(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// JSON from the host (event fields) as a ground term: numbers, strings,
/// booleans, arrays as lists, objects as maps with string keys.
pub fn j_to_norm(j: &J) -> Norm {
    match j {
        J::Null => Norm::nil(),
        J::Bool(b) => Norm::bool(*b),
        J::Int(i) => Norm::int(*i),
        J::Str(s) => Norm::str(s),
        J::Arr(v) => Norm::list(v.iter().map(j_to_norm).collect()),
        J::Obj(kv) => Norm::map(kv.iter().map(|(k, v)| (Norm::str(k), j_to_norm(v))).collect()),
    }
}

// ---------------------------------------------------------------------------
// A tab in the reach tier.

pub const REACH_CAPS: &[&str] = &[
    "rho:gaze:doc",
    "rho:gaze:log",
    "rho:gaze:clock",
    "rho:gaze:rand",
    "rho:gaze:net",
    "rho:gaze:store",
    "rho:gaze:nav",
];

pub fn reach_policy(urn: &str) -> bool {
    REACH_CAPS.contains(&urn)
}

pub const STORE_QUOTA: usize = 5 * 1024 * 1024;

fn err(code: &str, d: &str) -> Norm {
    Norm::tuple(vec![Norm::str("err"), Norm::str(code), Norm::str(d)])
}
fn okv(v: Norm) -> Norm {
    Norm::tuple(vec![Norm::str("ok"), v])
}
fn as_name(n: &Norm) -> Option<Name> {
    match n.node() {
        Node::Eval(x) => Some(x.clone()),
        _ => None,
    }
}
pub fn integrity_of(b: &[u8]) -> String {
    format!("blake2b-256:{}", hexs(&blake2b_256(b).0))
}

enum Waiting {
    Net { ret: Name, integrity: Option<String>, url: String },
    Store { ret: Option<Name> },
}

pub struct ReachTab<H: Host> {
    pub exec: TabExec<HostDom<H>>,
    waiting: BTreeMap<u32, Waiting>,
    next: u32,
    console_seen: usize,
}

/// Compile an inline script: kernel text with `imports="doc net=urn ..."`.
pub fn compile_inline(text: &str, imports: &str, level: &str) -> Result<Knf, String> {
    let lvl = match level.trim().to_ascii_lowercase().as_str() {
        "" | "k1g" => Level::K1G,
        "k0" => Level::K0,
        "k1" => Level::K1,
        "k2" => Level::K2,
        l => return Err(format!("unknown level {l}")),
    };
    let pairs: Vec<(String, String)> = imports
        .split_whitespace()
        .filter_map(|t| match t.split_once('=') {
            Some((i, u)) => Some((i.to_string(), u.to_string())),
            None => gaze_knf::default_urn(t).map(|u| (t.to_string(), u.to_string())),
        })
        .collect();
    let urns: Vec<(&str, &str)> = pairs.iter().map(|(i, u)| (i.as_str(), u.as_str())).collect();
    Knf::from_source(text, lvl, &urns).map_err(|e| format!("{e:?}"))
}

/// Fetched script bytes, checked against an `integrity` attribute.
pub fn accept_script(bytes: &[u8], integrity: Option<&str>) -> Result<Knf, String> {
    let k = Knf::decode(bytes).map_err(|e| format!("bad .knf: {e:?}"))?;
    if let Some(i) = integrity.filter(|i| !i.trim().is_empty()) {
        k.verify_integrity(i).map_err(|_| "integrity mismatch".to_string())?;
    }
    Ok(k)
}

impl<H: Host> ReachTab<H> {
    pub fn load(knf: &Knf, host: H, seed: [u8; 32]) -> Result<ReachTab<H>, String> {
        let exec = TabExec::load(knf, HostDom { host }, seed, &reach_policy).map_err(|e| e.to_string())?;
        Ok(ReachTab {
            exec,
            waiting: BTreeMap::new(),
            next: 0,
            console_seen: 0,
        })
    }

    pub fn add_script(&mut self, knf: &Knf) -> Result<(), String> {
        self.exec.add_script(knf, &reach_policy).map_err(|e| e.to_string())
    }

    fn reply(&mut self, class: Class, ret: Option<Name>, v: Norm) {
        if let Some(r) = ret {
            self.exec.deliver(class, r, vec![v]);
        }
    }

    /// Run a frame. Returns what the host must do next:
    /// `{"hash", "console", "net", "store", "nav", "busy", "deadline"}`.
    pub fn frame(&mut self, now_ms: u64) -> J {
        let h = self.exec.frame(now_ms);
        let (mut net, mut store, mut nav) = (Vec::new(), Vec::new(), Vec::new());
        for r in self.exec.take_requests() {
            let ret = r.args.last().and_then(as_name);
            let verb = r.args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
            match r.urn.as_str() {
                "rho:gaze:net" => {
                    let parsed = r.args.get(1).filter(|_| verb == "fetch").and_then(|m| {
                        let url = m.map_get("url")?.as_str()?.to_string();
                        Some((m.clone(), url))
                    });
                    let Some((m, url)) = parsed else {
                        self.reply(Class::Net, ret, err("type", "fetch"));
                        continue;
                    };
                    let Some(ret) = ret else { continue };
                    let method = m.map_get("method").and_then(|x| x.as_str()).unwrap_or("GET").to_ascii_uppercase();
                    let body = m.map_get("body").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let mut hdrs = Vec::new();
                    if let Some(hm) = m.map_get("headers").and_then(|x| x.as_coll(CollKind::Map)) {
                        for kv in hm.chunks(2) {
                            if let (Some(k), Some(v)) = (kv[0].as_str(), kv[1].as_str()) {
                                hdrs.push(J::Arr(vec![J::s(k), J::s(v)]));
                            }
                        }
                    }
                    self.next += 1;
                    self.waiting.insert(
                        self.next,
                        Waiting::Net {
                            ret,
                            integrity: m.map_get("integrity").and_then(|x| x.as_str()).map(str::to_string),
                            url: url.clone(),
                        },
                    );
                    net.push(J::obj(vec![
                        ("id", J::Int(self.next as i64)),
                        ("url", J::s(&url)),
                        ("method", J::s(&method)),
                        ("headers", J::Arr(hdrs)),
                        ("body", J::s(&body)),
                    ]));
                }
                "rho:gaze:store" => {
                    if !matches!(verb.as_str(), "get" | "put" | "del" | "list") {
                        self.reply(Class::Store, ret, err("verb", &verb));
                        continue;
                    }
                    let key = r.args.get(1).and_then(|k| k.as_str()).unwrap_or("").to_string();
                    let mut op = vec![("op", J::s(&verb)), ("key", J::s(&key))];
                    if verb == "put" {
                        let v = r.args.get(2).cloned().unwrap_or_else(Norm::nil);
                        if gaze_store::has_unforgeable(&v) || key.is_empty() || key.len() > 1024 {
                            self.reply(Class::Store, ret, err("type", &key));
                            continue;
                        }
                        if v.encode().len() + key.len() > STORE_QUOTA {
                            self.reply(Class::Store, ret, err("quota", &key));
                            continue;
                        }
                        op.push(("value", J::Str(hexs(v.encode()))));
                    }
                    self.next += 1;
                    self.waiting.insert(self.next, Waiting::Store { ret });
                    op.push(("id", J::Int(self.next as i64)));
                    store.push(J::obj(op));
                }
                "rho:gaze:nav" => {
                    let url = r.args.get(1).and_then(|u| u.as_str()).unwrap_or("").to_string();
                    nav.push(J::obj(vec![("op", J::s(&verb)), ("url", J::s(&url))]));
                }
                _ => self.reply(Class::DomReply, ret, err("revoked", "")),
            }
        }
        let console: Vec<J> =
            self.exec.console[self.console_seen..].iter().map(|(l, t)| J::Arr(vec![J::s(l), J::s(t)])).collect();
        self.console_seen = self.exec.console.len();
        J::obj(vec![
            ("hash", J::Str(hexs(&h))),
            ("console", J::Arr(console)),
            ("net", J::Arr(net)),
            ("store", J::Arr(store)),
            ("nav", J::Arr(nav)),
            ("busy", J::Bool(self.exec.wants_frame())),
            ("deadline", self.exec.next_deadline().map(|d| J::Int(d as i64)).unwrap_or(J::Null)),
        ])
    }

    /// The host finished a fetch. The body is verified here, so a page's
    /// `integrity` means the same as in the native browser.
    pub fn net_done(&mut self, id: u32, status: i64, headers: &J, body: &[u8], failure: Option<&str>) {
        let Some(Waiting::Net { ret, integrity, url }) = self.waiting.remove(&id) else { return };
        let datum = if let Some(f) = failure {
            err("net", f)
        } else if integrity.as_deref().is_some_and(|i| !i.trim().eq_ignore_ascii_case(&integrity_of(body))) {
            err("integrity", &url)
        } else {
            let hs: Vec<(Norm, Norm)> = headers
                .arr()
                .unwrap_or(&[])
                .iter()
                .filter_map(|p| {
                    let a = p.arr()?;
                    Some((Norm::str(&a.first()?.str()?.to_ascii_lowercase()), Norm::str(a.get(1)?.str()?)))
                })
                .collect();
            let text = hs.iter().any(|(k, v)| {
                k.as_str() == Some("content-type") && v.as_str().is_some_and(|c| c.starts_with("text/") || c.contains("json"))
            });
            let b = match (text, std::str::from_utf8(body)) {
                (true, Ok(s)) => Norm::str(s),
                _ => Norm::bytes(body),
            };
            okv(Norm::map(vec![
                (Norm::str("status"), Norm::int(status)),
                (Norm::str("headers"), Norm::map(hs)),
                (Norm::str("body"), b),
                (Norm::str("digest"), Norm::str(&integrity_of(body))),
            ]))
        };
        self.exec.deliver(Class::Net, ret, vec![datum]);
    }

    /// The host finished a store operation: `{"value": hex|null}`,
    /// `{"keys": [...]}`, `{"ok": true}` or `{"err": code}`.
    pub fn store_done(&mut self, id: u32, r: &J) {
        let Some(Waiting::Store { ret }) = self.waiting.remove(&id) else { return };
        let datum = if let Some(e) = r.get("err").and_then(|e| e.str()) {
            err(e, "store")
        } else if let Some(v) = r.get("value") {
            okv(v.str().and_then(unhex).and_then(|b| Norm::decode(&b).ok()).unwrap_or_else(Norm::nil))
        } else if let Some(k) = r.get("keys").and_then(|k| k.arr()) {
            okv(Norm::list(k.iter().filter_map(|x| x.str().map(Norm::str)).collect()))
        } else {
            okv(Norm::nil())
        };
        self.reply(Class::Store, ret, datum);
    }

    /// A DOM event at a host node. Returns bit 0 for prevent, bit 1 for stop.
    pub fn dispatch(&mut self, node: u32, ty: &str, fields: &J, bubbles: bool) -> u32 {
        if !self.exec.dom.has_listener(ty) {
            return 0;
        }
        let f: Vec<(String, Norm)> = match fields {
            J::Obj(kv) => kv.iter().map(|(k, v)| (k.clone(), j_to_norm(v))).collect(),
            _ => Vec::new(),
        };
        let d = self.exec.dispatch_ext(node, ty, f, bubbles);
        (d.prevent as u32) | ((d.stop as u32) << 1)
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm;
