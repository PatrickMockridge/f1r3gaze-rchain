//! The reach tier against a host over `MemDom`: the JSON protocol, the
//! services round trip, and agreement with the native executive.

use gaze_dom_core::mem::MemDom;
use gaze_dom_core::{ClassOp, DomBackend, Frag, Pos, Write};
use gaze_exec::{TabExec, default_policy};
use gaze_knf::Knf;
use gaze_reach::json::J;
use gaze_reach::{Host, ReachTab, compile_inline};
use k1ndl1ng_parse::Level;
use std::cell::RefCell;

struct MemHost(RefCell<MemDom>);

fn n(j: &J, k: &str) -> usize {
    j.get(k).and_then(|x| x.int()).unwrap_or(0) as usize
}
fn s<'a>(j: &'a J, k: &str) -> &'a str {
    j.get(k).and_then(|x| x.str()).unwrap_or("")
}
fn frag(j: &J) -> Frag {
    if let Some(t) = j.get("text") {
        return Frag::Text(t.str().unwrap_or("").into());
    }
    if let Some(h) = j.get("html") {
        return Frag::Html(h.str().unwrap_or("").into());
    }
    Frag::El {
        tag: s(j, "el").into(),
        attrs: j
            .get("attrs")
            .and_then(|a| a.arr())
            .unwrap_or(&[])
            .iter()
            .map(|p| {
                let a = p.arr().unwrap();
                (a[0].str().unwrap().into(), a[1].str().unwrap().into())
            })
            .collect(),
        children: j.get("children").and_then(|a| a.arr()).unwrap_or(&[]).iter().map(frag).collect(),
        reference: j.get("ref").and_then(|r| r.str()).map(str::to_string),
    }
}
fn write(w: &J) -> Write<usize> {
    let a = w.arr().unwrap();
    let t = a[1].int().unwrap() as usize;
    let st = |i: usize| a[i].str().unwrap_or("").to_string();
    match a[0].str().unwrap() {
        "setAttr" => Write::SetAttr(t, st(2), st(3)),
        "removeAttr" => Write::RemoveAttr(t, st(2)),
        "setText" => Write::SetText(t, st(2)),
        "setValue" => Write::SetValue(t, st(2)),
        "class" => Write::Class(
            t,
            match a[2].str().unwrap() {
                "add" => ClassOp::Add,
                "remove" => ClassOp::Remove,
                _ => ClassOp::Toggle,
            },
            st(3),
        ),
        "style" => Write::Style(t, st(2), a[3].str().map(str::to_string)),
        "insert" => Write::Insert(
            t,
            match a[2].str().unwrap() {
                "append" => Pos::Append,
                "prepend" => Pos::Prepend,
                "before" => Pos::Before,
                "after" => Pos::After,
                _ => Pos::ReplaceWith,
            },
            frag(&a[3]),
        ),
        "setHTML" => Write::SetHtml(t, st(2)),
        "remove" => Write::Remove(t),
        "focus" => Write::Focus(t),
        _ => Write::Blur(t),
    }
}

impl Host for MemHost {
    fn call(&self, c: &J) -> J {
        let ids = |v: Vec<usize>| J::Arr(v.into_iter().map(|x| J::Int(x as i64)).collect());
        let op = s(c, "op");
        if op == "apply" {
            let mut d = self.0.borrow_mut();
            let ws: Vec<Write<usize>> = c.get("writes").unwrap().arr().unwrap().iter().map(write).collect();
            return J::Arr(
                d.apply_batch(&ws)
                    .into_iter()
                    .map(|r| match r {
                        None => J::Null,
                        Some(refs) => J::Arr(refs.into_iter().map(|(k, x)| J::Arr(vec![J::s(&k), J::Int(x as i64)])).collect()),
                    })
                    .collect(),
            );
        }
        let d = self.0.borrow();
        // The JS host passes `:scope`-prefixed selectors to querySelectorAll;
        // MemDom confines by construction, so strip the prefix.
        let unscope = |sel: &str| sel.replace(":scope ", "");
        match op {
            "root" => J::Int(d.root() as i64),
            "parent" => d.parent(n(c, "n")).map(|x| J::Int(x as i64)).unwrap_or(J::Null),
            "children" => ids(d.children(n(c, "n"))),
            "attached" => J::Bool(d.attached(n(c, "n"))),
            "isElement" => J::Bool(d.is_element(n(c, "n"))),
            "attr" => d.attr(n(c, "n"), s(c, "k")).map(|v| J::s(&v)).unwrap_or(J::Null),
            "text" => J::s(&d.text(n(c, "n"))),
            "query" => match d.query(n(c, "n"), &unscope(s(c, "sel")), c.get("all") == Some(&J::Bool(true))) {
                Ok(v) => ids(v),
                Err(e) => J::obj(vec![("err", J::s(&e))]),
            },
            "matches" => {
                let x = n(c, "n");
                J::Bool(d.closest(x, s(c, "sel"), x).ok().flatten() == Some(x))
            }
            "serialize" => J::s(&d.serialize()),
            _ => J::Null,
        }
    }
}

const TODO: &str = r##"new b, l, clicks, sub, got, put in {
  doc!("query1", "#add", *b) | doc!("query1", "#list", *l) |
  store!("put", "seen", 1, *put) |
  for (_ <- put) { store!("get", "seen", *got) | for (@("ok", v) <- got) { log!("info", v) } } |
  for (@("ok", *btn) <- b & @("ok", *list) <- l) {
    btn!("listen", "click", *clicks, {}, *sub) |
    for (_ <= clicks) { new r in { list!("append", ("el", "li", {"class": "item"}, [("text", "task")]), *r) } }
  }
}"##;
const HTML: &str = r#"<html><head></head><body><button id="add">add</button><ul id="list"></ul></body></html>"#;

#[test]
fn reach_commits_what_the_native_executive_commits() {
    let k = Knf::from_source(TODO, Level::K1G, &[]).unwrap();
    let mut reach = ReachTab::load(&k, MemHost(RefCell::new(MemDom::from_html(HTML))), [4; 32]).unwrap();
    // The native side grants store too, and answers it from the same values.
    let policy = |u: &str| default_policy(u) || u == "rho:gaze:store";
    let mut native = TabExec::load(&k, MemDom::from_html(HTML), [4; 32], &policy).unwrap();
    let mut store: std::collections::BTreeMap<String, String> = Default::default();
    let mut t = 0u64;
    for step in 0..12 {
        t += 16;
        let out = reach.frame(t);
        native.frame(t);
        // The host's store: answer every request.
        for r in out.get("store").unwrap().arr().unwrap() {
            let id = n(r, "id") as u32;
            let ans = match s(r, "op") {
                "put" => {
                    store.insert(s(r, "key").into(), s(r, "value").into());
                    J::obj(vec![("ok", J::Bool(true))])
                }
                _ => J::obj(vec![("value", store.get(s(r, "key")).map(|v| J::s(v)).unwrap_or(J::Null))]),
            };
            reach.store_done(id, &ans);
        }
        // The native store answers in kind.
        for req in native.take_requests() {
            let v = match req.args[0].as_str().unwrap() {
                "put" => gaze_exec::Class::Store,
                _ => gaze_exec::Class::Store,
            };
            let ret = match req.args.last().unwrap().node() {
                k1ndl1ng_norm::Node::Eval(x) => x.clone(),
                _ => continue,
            };
            let datum = if req.args[0].as_str() == Some("put") {
                k1ndl1ng_norm::Norm::tuple(vec![k1ndl1ng_norm::Norm::str("ok"), k1ndl1ng_norm::Norm::nil()])
            } else {
                k1ndl1ng_norm::Norm::tuple(vec![k1ndl1ng_norm::Norm::str("ok"), k1ndl1ng_norm::Norm::int(1)])
            };
            native.deliver(v, ret, vec![datum]);
        }
        if step == 5 || step == 8 {
            let r_btn = reach.exec.dom.backend.query(reach.exec.dom.backend.root(), "#add", false).unwrap()[0];
            let n_btn = native.dom.backend.by_id("add").unwrap();
            reach.dispatch(r_btn, "click", &J::obj(vec![("x", J::Int(1))]), true);
            native.dispatch(n_btn, "click", vec![("x".into(), k1ndl1ng_norm::Norm::int(1))]);
        }
    }
    assert_eq!(reach.exec.commit_hashes(), native.commit_hashes());
    let doc = reach.exec.dom.backend.serialize();
    assert_eq!(doc.matches(r#"<li class="item">task</li>"#).count(), 2, "{doc}");
    assert_eq!(reach.exec.console.len(), 1, "the stored value came back");
}

#[test]
fn integrity_and_denials_in_the_reach_tier() {
    let src = r##"new r, s in { net!("fetch", {"url": "/x", "integrity": "blake2b-256:00"}, *r) | shard!("lookup", "rho:id:x", *s) |
      for (@("err", code, _) <- r) { log!("warn", code) } | for (@x <- s) { log!("error", x) } }"##;
    let k = compile_inline(src, "net log shard", "k1g").unwrap();
    let mut t = ReachTab::load(&k, MemHost(RefCell::new(MemDom::from_html(HTML))), [1; 32]).unwrap();
    let out = t.frame(16);
    let net = out.get("net").unwrap().arr().unwrap();
    assert_eq!(net.len(), 1);
    t.net_done(n(&net[0], "id") as u32, 200, &J::Arr(vec![]), b"tampered", None);
    for i in 0..4 {
        t.frame(32 + i * 16);
    }
    assert_eq!(t.exec.console, vec![("warn".to_string(), "\"integrity\"".to_string())]);
    // `shard` is a dead channel in the reach tier: no answer at all.
    assert!(t.exec.grants.iter().any(|g| g.urn == "rho:gaze:shard" && !g.granted));
}
