//! Conformance: whole pages through the tab executive over the reference DOM.

use gaze_dom_core::mem::MemDom;
use gaze_dom_core::DomBackend;
use gaze_exec::{default_policy, LoadError, Record, TabExec, TabLog};
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
const LAMP_HTML: &str = r#"<main><button id="lamp">lamp</button></main>"#;

fn knf(src: &str) -> Knf {
    Knf::from_source(src, Level::K1G, &[]).expect("compiles")
}

fn run_until_quiet<B: DomBackend>(tab: &mut TabExec<B>, t: &mut u64) {
    for _ in 0..8 {
        *t += 16;
        tab.frame(*t);
    }
}

#[test]
fn the_lamp_toggles() {
    let k = Knf::decode(&knf(LAMP).encode()).unwrap();
    let mut tab = TabExec::load(&k, MemDom::from_html(LAMP_HTML), [1; 32], &default_policy).unwrap();
    let mut t = 0;
    run_until_quiet(&mut tab, &mut t);
    let lamp = tab.dom.backend.by_id("lamp").unwrap();
    assert_eq!(tab.dom.listener_count(), 1);
    assert!(tab.dom.backend.classes(lamp).is_empty());

    tab.dispatch(lamp, "click", vec![("x".into(), Norm::int(5))]);
    run_until_quiet(&mut tab, &mut t);
    assert_eq!(tab.dom.backend.classes(lamp), vec!["lit".to_string()]);

    tab.dispatch(lamp, "click", vec![]);
    run_until_quiet(&mut tab, &mut t);
    assert!(tab.dom.backend.classes(lamp).is_empty());

    // Two clicks in one frame: exactly one token, so exactly two toggles.
    tab.dispatch(lamp, "click", vec![]);
    tab.dispatch(lamp, "click", vec![]);
    run_until_quiet(&mut tab, &mut t);
    assert!(tab.dom.backend.classes(lamp).is_empty());
    assert_eq!(tab.dom.backend.serialize(), r#"<main><button id="lamp">lamp</button></main>"#);
}

#[test]
fn replay_reproduces_every_commit() {
    let k = knf(LAMP);
    let mut live = TabExec::load(&k, MemDom::from_html(LAMP_HTML), [7; 32], &default_policy).unwrap();
    let mut t = 0;
    run_until_quiet(&mut live, &mut t);
    let lamp = live.dom.backend.by_id("lamp").unwrap();
    for _ in 0..3 {
        live.dispatch(lamp, "click", vec![]);
        run_until_quiet(&mut live, &mut t);
    }
    // Through bytes and back.
    let bytes = live.log.to_bytes();
    let log = TabLog::from_bytes(&bytes).unwrap();
    let frames = log.records.iter().filter(|r| matches!(r, Record::Frame { .. })).count();

    let mut again = TabExec::replay(&k, MemDom::from_html(LAMP_HTML), &log).unwrap();
    let mut t2 = 0;
    for _ in 0..frames {
        t2 += 16;
        again.frame(t2);
    }
    assert_eq!(again.commit_hashes(), live.commit_hashes());
    assert_eq!(again.dom.backend.serialize(), live.dom.backend.serialize());
    assert_eq!(again.dom.backend.classes(lamp), vec!["lit".to_string()]);

    // A log for another program is refused.
    let other = knf(r#"doc!("query1", "p", Nil)"#);
    assert!(matches!(
        TabExec::replay(&other, MemDom::from_html(LAMP_HTML), &log),
        Err(LoadError::Replay(_))
    ));
}

#[test]
fn same_seed_same_run_different_seed_same_document() {
    let k = knf(LAMP);
    let go = |seed: [u8; 32]| {
        let mut tab = TabExec::load(&k, MemDom::from_html(LAMP_HTML), seed, &default_policy).unwrap();
        let mut t = 0;
        run_until_quiet(&mut tab, &mut t);
        let lamp = tab.dom.backend.by_id("lamp").unwrap();
        tab.dispatch(lamp, "click", vec![]);
        run_until_quiet(&mut tab, &mut t);
        (tab.commit_hashes(), tab.log.to_bytes())
    };
    let (h1, l1) = go([1; 32]);
    let (h2, l2) = go([1; 32]);
    let (h3, l3) = go([2; 32]);
    assert_eq!((h1.clone(), l1.clone()), (h2, l2));
    assert_eq!(h1, h3, "names differ by seed; the document does not");
    assert_ne!(l1, l3, "but the names in the log do");
}

#[test]
fn denied_capabilities_are_dead_channels() {
    // `net` is not served by this crate's broker: the send goes nowhere.
    let src = r##"new r in { net!("fetch", {"url": "https://example.org"}, *r) | for (_ <- r) { doc!("query1", "#lamp", Nil) } | log!("info", "ran") }"##;
    let mut tab = TabExec::load(&knf(src), MemDom::from_html(LAMP_HTML), [3; 32], &default_policy).unwrap();
    assert!(tab.grants.iter().any(|g| g.ident == "net" && !g.granted));
    let mut t = 0;
    run_until_quiet(&mut tab, &mut t);
    assert_eq!(tab.console, vec![("info".to_string(), "\"ran\"".to_string())]);
    // No reply ever arrived on r, so doc was never touched.
    assert_eq!(tab.dom.listener_count(), 0);
}

#[test]
fn a_component_given_a_subtree_cannot_reach_outside_it() {
    let html = r#"<div id="host"><div id="slot"><span id="mine">m</span></div><p id="secret">s</p></div>"#;
    // The page confines a component to #slot; the component tries to read
    // #secret through its name, and marks what it can reach.
    let src = r##"new r, a, slot, got, gone in {
      doc!("query1", "#slot", *r) |
      for (@("ok", *s) <- r) { s!("attenuate", *a) } |
      for (@("ok", *s) <- a) { slot!(*s) } |
      for (s <- slot) {
        s!("query1", "#secret", *got) | s!("query1", "#mine", *gone) |
        for (@("err", code, _) <- got) { log!("info", code) } |
        for (@("ok", *m) <- gone) { m!("setAttr", "data-reached", "yes") }
      }
    }"##;
    let mut tab = TabExec::load(&knf(src), MemDom::from_html(html), [4; 32], &default_policy).unwrap();
    let mut t = 0;
    run_until_quiet(&mut tab, &mut t);
    assert_eq!(tab.console, vec![("info".to_string(), "\"none\"".to_string())]);
    assert!(tab.dom.backend.serialize().contains(r#"<span id="mine" data-reached="yes">"#));
}

#[test]
fn clock_frames_and_timers() {
    let src = r#"new t, once in {
      clock!("frames", *t) | clock!("after", 40, *once) |
      for (@("tick", n, _) <= t) { log!("info", n) } |
      for (@("timer", _) <- once) { clock!("stop", *t) | log!("info", "done") }
    }"#;
    let mut tab = TabExec::load(&knf(src), MemDom::from_html("<p></p>"), [5; 32], &default_policy).unwrap();
    for i in 1..=8u64 {
        tab.frame(i * 16);
    }
    let ticks: Vec<&str> = tab.console.iter().map(|(_, v)| v.as_str()).collect();
    assert!(ticks.contains(&"\"done\""), "{ticks:?}");
    let after_done = ticks.iter().skip_while(|v| **v != "\"done\"").count();
    assert!(after_done <= 3, "ticks stop soon after the timer: {ticks:?}");
    assert_eq!(ticks[0], "2", "first tick is injected in frame 2");
}

#[test]
fn a_runaway_page_is_bounded_per_frame_and_keeps_its_work() {
    let src = r#"new c in { c!(Nil) | for (_ <= c) { c!(Nil) } }"#;
    let mut k = knf(src);
    k.manifest.frame_budget = 50;
    let mut tab = TabExec::load(&k, MemDom::from_html("<p></p>"), [6; 32], &default_policy).unwrap();
    for i in 1..=5u64 {
        tab.frame(i * 16);
    }
    assert_eq!(tab.frame_count(), 5);
    assert!(!tab.is_quiescent(), "still running, never finished, never lost");
    assert!(tab.stalled_frames() > 0, "the meter refused work in some frame");
    let spent = tab.meter().spent();
    assert!(spent > 0 && spent <= 5 * 50 * gaze_exec::COST_COMM as u64);
}

#[test]
fn graded_and_guarded_pages_are_refused_until_their_work_packages_land() {
    let mut k = knf(LAMP);
    k.manifest.semiring = "prob".into();
    assert!(matches!(TabExec::load(&k, MemDom::from_html(LAMP_HTML), [1; 32], &default_policy), Err(LoadError::Graded(_))));
    k.manifest.ceiling = "sampled".into();
    assert!(matches!(TabExec::load(&k, MemDom::from_html(LAMP_HTML), [1; 32], &default_policy), Err(LoadError::Graded(_))));
}

/// A listener receives `(type, fields)`. The field map carries the guaranteed
/// fields of its type and may carry more (docs/events.md). A listener written
/// with a remainder names what it uses and keeps working when a host adds
/// fields; an exact pattern stops matching.
#[test]
fn remainder_patterns_survive_extra_event_fields() {
    let src = r##"new found, sub, clicks in {
      doc!("query1", "#lamp", *found) |
      for (@("ok", *lamp) <- found) {
        lamp!("listen", "click", *clicks, {}, *sub) |
        for (@(_, {"target": *t, "x": x ..._}) <= clicks) { t!("setAttr", "data-x", "seen") | log!("info", x) }
      }
    }"##;
    let exact = r##"new found, sub, clicks in {
      doc!("query1", "#lamp", *found) |
      for (@("ok", *lamp) <- found) {
        lamp!("listen", "click", *clicks, {}, *sub) |
        for (@(_, {"target": *t, "type": _, "x": x, "y": _, "button": _, "mods": _}) <= clicks) { log!("info", x) }
      }
    }"##;
    let click = |extra: bool| {
        let mut f = vec![
            ("x".to_string(), Norm::int(7)),
            ("y".to_string(), Norm::int(9)),
            ("button".to_string(), Norm::int(0)),
            ("mods".to_string(), Norm::list(vec![])),
        ];
        if extra {
            f.push(("pointerType".to_string(), Norm::str("mouse")));
        }
        f
    };
    for (page, extra, expect) in [(src, false, 1), (src, true, 1), (exact, false, 1), (exact, true, 0)] {
        let k = Knf::from_source(page, Level::K1G, &[]).expect("compiles");
        let mut tab = TabExec::load(&k, MemDom::from_html(LAMP_HTML), [3; 32], &default_policy).unwrap();
        let mut t = 0;
        run_until_quiet(&mut tab, &mut t);
        let lamp = tab.dom.backend.by_id("lamp").unwrap();
        tab.dispatch(lamp, "click", click(extra));
        run_until_quiet(&mut tab, &mut t);
        assert_eq!(tab.console.len(), expect, "extra fields: {extra}, pattern: {}", if page == src { "remainder" } else { "exact" });
        if page == src {
            assert_eq!(tab.console[0].1, "7");
            assert!(tab.dom.backend.serialize().contains(r#"data-x="seen""#));
        }
    }
}
