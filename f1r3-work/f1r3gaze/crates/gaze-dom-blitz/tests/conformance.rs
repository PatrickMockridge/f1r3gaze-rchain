//! Protocol conformance across backends (spec §13): the same pages, the same
//! scripted input, over the reference `MemDom` and over Blitz. Both must
//! commit identical documents, frame by frame.

use gaze_dom_blitz::{BNode, BlitzDom};
use gaze_dom_core::mem::MemDom;
use gaze_dom_core::DomBackend;
use gaze_exec::{default_policy, Record, TabExec, TabLog};
use gaze_knf::Knf;
use k1ndl1ng_norm::Norm;
use k1ndl1ng_parse::Level;

const LAMP: &str = r##"new found, sub, clicks, on, off in {
  doc!("query1", "#lamp", *found) |
  for (@("ok", *lamp) <- found) {
    lamp!("listen", "click", *clicks, {}, *sub) |
    off!(Nil) |
    for (_ <= clicks & _ <- off) { lamp!("classAdd", "lit")    | on!(Nil)  } |
    for (_ <= clicks & _ <- on)  { lamp!("classRemove", "lit") | off!(Nil) }
  }
}"##;

/// A list the page builds with structured fragments and `ref` markers; each
/// click on the button appends an item whose own button removes it.
const TODO: &str = r##"new b, l, clicks, made, sub, n in {
  doc!("query1", "#add", *b) | doc!("query1", "#list", *l) |
  for (@("ok", *btn) <- b & @("ok", *list) <- l) {
    btn!("listen", "click", *clicks, {}, *sub) |
    for (_ <= clicks) {
      new r, xs, sub2 in {
        list!("append", ("el", "li", {"class": "item"}, [("text", "task"), ("el", "button", {"class": "x"}, [("ref", "x"), ("text", "x")])]), *r) |
        for (@("ok", {"x": *x}) <- r) {
          x!("listen", "click", *xs, {"once": true}, *sub2) |
          for (@(_, {"target": *t, "type": _, "x": _}) <- xs) {
            t!("closest", "li", *made) |
            for (@("ok", *li) <- made) { li!("remove") }
          }
        }
      }
    }
  }
}"##;

const TODO_HTML: &str = r#"<html><head></head><body><button id="add">add</button><ul id="list"></ul></body></html>"#;
const LAMP_HTML: &str = r#"<html><head></head><body><main><button id="lamp">lamp</button></main></body></html>"#;

fn knf(src: &str) -> Knf {
    Knf::from_source(src, Level::K1G, &[]).expect("compiles")
}

fn frames<B: DomBackend>(tab: &mut TabExec<B>, t: &mut u64, n: usize) {
    for _ in 0..n {
        *t += 16;
        tab.frame(*t);
    }
}

/// Drive a page with a script of clicks given as selectors.
fn drive<B: DomBackend>(
    mut tab: TabExec<B>,
    clicks: &[&str],
    find: &dyn Fn(&TabExec<B>, &str) -> Option<B::Node>,
) -> (Vec<[u8; 32]>, String, TabExec<B>) {
    let mut t = 0;
    frames(&mut tab, &mut t, 6);
    for sel in clicks {
        let target = find(&tab, sel).unwrap_or_else(|| panic!("no {sel}"));
        tab.dispatch(target, "click", vec![("x".into(), Norm::int(1))]);
        frames(&mut tab, &mut t, 8);
    }
    let s = tab.dom.backend.serialize();
    (tab.commit_hashes(), s, tab)
}

fn mem_find(tab: &TabExec<MemDom>, sel: &str) -> Option<usize> {
    let r = tab.dom.backend.root();
    tab.dom.backend.query(r, sel, false).ok()?.first().copied()
}
fn blitz_find(tab: &TabExec<BlitzDom>, sel: &str) -> Option<BNode> {
    let r = tab.dom.backend.root();
    tab.dom.backend.query(r, sel, false).ok()?.first().copied()
}

fn both(src: &str, html: &str, clicks: &[&str]) -> (String, TabExec<BlitzDom>) {
    let k = knf(src);
    let mem = TabExec::load(&k, MemDom::from_html(html), [3; 32], &default_policy).unwrap();
    let blz = TabExec::load(&k, BlitzDom::from_html(html), [3; 32], &default_policy).unwrap();
    let (hm, sm, _) = drive(mem, clicks, &mem_find);
    let (hb, sb, tb) = drive(blz, clicks, &blitz_find);
    assert_eq!(sm, sb, "committed documents differ");
    assert_eq!(hm, hb, "per-frame commit hashes differ");
    (sb, tb)
}

#[test]
fn blitz_parses_like_the_reference() {
    let b = BlitzDom::from_html(LAMP_HTML);
    assert_eq!(b.serialize(), MemDom::from_html(LAMP_HTML).serialize());
}

#[test]
fn lamp_agrees_across_backends() {
    let (s, _) = both(LAMP, LAMP_HTML, &["#lamp", "#lamp", "#lamp"]);
    assert!(s.contains(r#"<button id="lamp" class="lit">"#), "{s}");
}

#[test]
fn todo_agrees_across_backends_and_replays() {
    let (s, tab) = both(TODO, TODO_HTML, &["#add", "#add", "#add", "li .x"]);
    assert_eq!(s.matches(r#"<li class="item">"#).count(), 2, "{s}");

    // Replay the Blitz run from its log bytes, on Blitz. The click on the
    // item's own button targets a node the page never named before the
    // click: replay must mint the same names, which the dispatch record does.
    let log = TabLog::from_bytes(&tab.log.to_bytes()).unwrap();
    assert!(log.records.iter().any(|r| matches!(r, Record::Dispatch { .. })));
    let frames_n = log.records.iter().filter(|r| matches!(r, Record::Frame { .. })).count();
    let mut again = TabExec::replay(&knf(TODO), BlitzDom::from_html(TODO_HTML), &log).unwrap();
    let mut t = 0;
    frames(&mut again, &mut t, frames_n);
    assert_eq!(again.commit_hashes(), tab.commit_hashes());
    // ...and on the reference backend: the log is backend-independent.
    let mut on_mem = TabExec::replay(&knf(TODO), MemDom::from_html(TODO_HTML), &log).unwrap();
    let mut t = 0;
    frames(&mut on_mem, &mut t, frames_n);
    assert_eq!(on_mem.commit_hashes(), tab.commit_hashes());
}

#[test]
fn attenuated_queries_cannot_see_above_the_subtree() {
    let html = r#"<html><head></head><body><div class="outer"><section id="s"><span>t</span></section></div></body></html>"#;
    let b = BlitzDom::from_html(html);
    let s = b.by_id("s").unwrap();
    let root = b.root();
    assert_eq!(b.query(root, ".outer span", true).unwrap().len(), 1);
    // Blitz's own scoped query would answer 1 here; the backend must not.
    assert_eq!(b.query(s, ".outer span", true).unwrap().len(), 0);
    assert_eq!(b.query(s, "span", true).unwrap().len(), 1);
    assert_eq!(b.query(s, "section span", true).unwrap().len(), 0);
    let span = b.query(s, "span", false).unwrap()[0];
    assert_eq!(b.closest(span, "div", s).unwrap(), None);
    assert_eq!(b.closest(span, "section", s).unwrap(), Some(s));
    assert_eq!(b.closest(span, "div section", s).unwrap(), None);
    assert!(b.query(s, "span[", true).is_err());
}

#[test]
fn a_decide_listener_prevents_synchronously() {
    let src = r##"new f, ev, sub in {
      doc!("query1", "#go", *f) |
      for (@("ok", *go) <- f) {
        go!("listen", "click", *ev, {"decide": true}, *sub) |
        for (@(_, {"reply": *r, "target": _, "type": _}) <= ev) { r!("prevent") }
      }
    }"##;
    let html = r#"<html><head></head><body><a id="go" href="https://example.org/">go</a></body></html>"#;
    let mut tab = TabExec::load(&knf(src), BlitzDom::from_html(html), [9; 32], &default_policy).unwrap();
    let mut t = 0;
    frames(&mut tab, &mut t, 6);
    let go = tab.dom.backend.by_id("go").unwrap();
    let d = tab.dispatch_ext(go, "click", vec![], true);
    assert!(d.drained && d.prevent, "{d:?}");
}

#[test]
fn set_html_and_fragments_render() {
    let src = r##"new f, r in {
      doc!("query1", "#box", *f) |
      for (@("ok", *b) <- f) {
        b!("setHTML", "<p>one</p><script>alert(1)</script>") |
        b!("append", ("el", "p", {"id": "two"}, [("text", "two & more")]), *r)
      }
    }"##;
    let html = r#"<html><head></head><body><div id="box">old</div></body></html>"#;
    let (s, _) = both(src, html, &[]);
    assert!(s.contains("<p>one</p>"), "{s}");
    assert!(s.contains(r#"<p id="two">two &amp; more</p>"#), "{s}");
}
