use super::*;
use crate::mem::MemDom;

fn ret(k: u8) -> Name {
    Name::Unforgeable([k; 32])
}
fn call(e: &mut Engine<MemDom>, on: &Key, verb: &str, mut args: Vec<Norm>, r: Option<u8>) -> Vec<Reply> {
    args.insert(0, Norm::str(verb));
    if let Some(r) = r {
        args.push(Norm::eval(ret(r)));
    }
    e.handle(on, &args)
}
fn ok_name(rs: &[Reply]) -> Key {
    let t = rs[0].args[0].as_coll(CollKind::Tuple).expect("tuple");
    assert_eq!(t[0].as_str(), Some("ok"), "{:?}", k1ndl1ng_norm::show(&rs[0].args[0]));
    match as_name(&t[1]) {
        Some(Name::Unforgeable(k)) => k,
        _ => panic!("not a name"),
    }
}
fn err_code(rs: &[Reply]) -> String {
    let t = rs[0].args[0].as_coll(CollKind::Tuple).expect("tuple");
    assert_eq!(t[0].as_str(), Some("err"));
    t[1].as_str().unwrap().to_string()
}

const PAGE: &str = r#"<main><div id="widget"><button id="b">go</button></div><p id="secret">s</p></main>"#;

#[test]
fn names_are_stable_per_authority() {
    let mut e = Engine::new(MemDom::from_html(PAGE), [1; 32]);
    let doc = e.grant_document();
    let a = ok_name(&call(&mut e, &doc, "query1", vec![Norm::str("#b")], Some(1)));
    let b = ok_name(&call(&mut e, &doc, "query1", vec![Norm::str("button")], Some(1)));
    assert_eq!(a, b, "same node, same authority, same name");
    let w = ok_name(&call(&mut e, &doc, "query1", vec![Norm::str("#widget")], Some(1)));
    let c = ok_name(&call(&mut e, &w, "query1", vec![Norm::str("button")], Some(1)));
    assert_eq!(a, c, "authority is inherited through queries");
    let wa = ok_name(&call(&mut e, &w, "attenuate", vec![], Some(1)));
    let d = ok_name(&call(&mut e, &wa, "query1", vec![Norm::str("button")], Some(1)));
    assert_ne!(a, d, "a different authority gets a different name");
}

#[test]
fn attenuation_is_the_tree() {
    let mut e = Engine::new(MemDom::from_html(PAGE), [1; 32]);
    let doc = e.grant_document();
    let w0 = ok_name(&call(&mut e, &doc, "query1", vec![Norm::str("#widget")], Some(1)));
    let w = ok_name(&call(&mut e, &w0, "attenuate", vec![], Some(1)));
    // The widget cannot see the secret, climb out, or remove itself.
    assert_eq!(err_code(&call(&mut e, &w, "query1", vec![Norm::str("#secret")], Some(1))), "none");
    assert_eq!(err_code(&call(&mut e, &w, "parent", vec![], Some(1))), "attenuated");
    assert_eq!(err_code(&call(&mut e, &w, "remove", vec![], Some(1))), "attenuated");
    assert_eq!(err_code(&call(&mut e, &w, "after", vec![Norm::str("<i>x</i>")], Some(1))), "attenuated");
    // Inside, parent works up to the widget itself.
    let b = ok_name(&call(&mut e, &w, "query1", vec![Norm::str("#b")], Some(1)));
    assert_eq!(ok_name(&call(&mut e, &b, "parent", vec![], Some(1))), w);
    assert_eq!(err_code(&call(&mut e, &b, "closest", vec![Norm::str("main")], Some(1))), "none");
}

#[test]
fn writes_wait_for_commit_and_refs_come_back() {
    let mut e = Engine::new(MemDom::from_html(r#"<ul id="l"></ul>"#), [2; 32]);
    let doc = e.grant_document();
    let l = ok_name(&call(&mut e, &doc, "query1", vec![Norm::str("#l")], Some(1)));
    let frag = Norm::tuple(vec![
        Norm::str("el"),
        Norm::str("li"),
        Norm::map(vec![(Norm::str("class"), Norm::str("todo"))]),
        Norm::list(vec![
            Norm::tuple(vec![Norm::str("text"), Norm::str("buy milk")]),
            Norm::tuple(vec![Norm::str("ref"), Norm::str("item")]),
        ]),
    ]);
    assert!(call(&mut e, &l, "append", vec![frag], Some(3)).is_empty());
    // Not yet visible to a read in the same frame.
    let rs = call(&mut e, &l, "text", vec![], Some(4));
    assert_eq!(rs[0].args[0].as_coll(CollKind::Tuple).unwrap()[1].as_str(), Some(""));
    let before = e.doc_hash();
    let (replies, after) = e.commit();
    assert_ne!(before, after);
    assert_eq!(e.backend.serialize(), r#"<ul id="l"><li class="todo">buy milk</li></ul>"#);
    let item = replies[0].args[0].as_coll(CollKind::Tuple).unwrap()[1].map_get("item").unwrap().clone();
    let Some(Name::Unforgeable(ik)) = as_name(&item) else { panic!() };
    call(&mut e, &ik, "classAdd", vec![Norm::str("done")], None);
    call(&mut e, &ik, "remove", vec![], None);
    call(&mut e, &ik, "classAdd", vec![Norm::str("late")], Some(5));
    let (replies, _) = e.commit();
    assert_eq!(e.backend.serialize(), r#"<ul id="l"></ul>"#);
    assert_eq!(err_code(&replies), "detached");
}

#[test]
fn events_capture_bubble_stop_once() {
    let mut e = Engine::new(MemDom::from_html(PAGE), [3; 32]);
    let doc = e.grant_document();
    let w0 = ok_name(&call(&mut e, &doc, "query1", vec![Norm::str("#widget")], Some(1)));
    let w = ok_name(&call(&mut e, &w0, "attenuate", vec![], Some(1)));
    let b = ok_name(&call(&mut e, &w, "query1", vec![Norm::str("#b")], Some(1)));
    let flags = |pairs: &[(&str, bool)]| Norm::map(pairs.iter().map(|(k, v)| (Norm::str(k), Norm::bool(*v))).collect());
    call(&mut e, &doc, "listen", vec![Norm::str("click"), Norm::eval(ret(10)), flags(&[("capture", true)])], None);
    call(&mut e, &b, "listen", vec![Norm::str("click"), Norm::eval(ret(11)), flags(&[("once", true), ("prevent", true)])], None);
    call(&mut e, &w, "listen", vec![Norm::str("click"), Norm::eval(ret(12)), flags(&[("stop", true)])], None);
    call(&mut e, &doc, "listen", vec![Norm::str("click"), Norm::eval(ret(13)), flags(&[])], None);
    let bnode = e.backend.by_id("b").unwrap();
    let f = e.fire(bnode, "click", vec![("x".into(), Norm::int(3))]);
    let order: Vec<Name> = f.injections.iter().map(|r| r.chan.clone()).collect();
    assert_eq!(order, vec![ret(10), ret(11), ret(12)], "capture, target, bubble to the stop");
    assert!(f.prevent && f.stop);
    // The target field is named under each listener's own authority.
    let t_widget = f.injections[2].args[0].as_coll(CollKind::Tuple).unwrap()[1].map_get("target").cloned().unwrap();
    assert_eq!(as_name(&t_widget), Some(Name::Unforgeable(b)));
    let f2 = e.fire(bnode, "click", vec![]);
    assert_eq!(f2.injections.len(), 2, "once removed the target listener");
}
