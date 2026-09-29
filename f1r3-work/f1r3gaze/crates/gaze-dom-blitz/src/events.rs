//! [`RhoEventHandler`]: Blitz's event driver hands every DOM event to the tab
//! executive before Blitz's default action.
//!
//! Blitz decides `prevent_default` and `stop_propagation` synchronously
//! (spec Finding 6), so static listener options decide them, and a `decide`
//! listener gets a synchronous drain bounded in steps by the manifest.

use crate::backend::{BNode, BlitzDom};
use blitz_dom::{Document, EventHandler, NodeId};
use blitz_traits::events::{BlitzKeyEvent, BlitzPointerEvent, DomEvent, DomEventData, EventState};
use gaze_exec::TabExec;
use k1ndl1ng_norm::Norm;
use keyboard_types::Modifiers;

pub struct RhoEventHandler<'t> {
    pub tab: &'t mut TabExec<BlitzDom>,
    /// Set when the page took any event, so the host schedules a frame.
    pub touched: &'t mut bool,
}

fn mods(m: Modifiers) -> Norm {
    let mut v = Vec::new();
    for (flag, name) in [
        (Modifiers::SHIFT, "shift"),
        (Modifiers::CONTROL, "ctrl"),
        (Modifiers::ALT, "alt"),
        (Modifiers::META, "meta"),
    ] {
        if m.contains(flag) {
            v.push(Norm::str(name));
        }
    }
    Norm::list(v)
}

fn pointer(p: &BlitzPointerEvent) -> Vec<(String, Norm)> {
    vec![
        ("x".into(), Norm::int(p.page_x().round() as i64)),
        ("y".into(), Norm::int(p.page_y().round() as i64)),
        ("button".into(), Norm::int(p.button as i64)),
        ("mods".into(), mods(p.mods)),
    ]
}

fn key(k: &BlitzKeyEvent) -> Vec<(String, Norm)> {
    vec![
        ("key".into(), Norm::str(&k.key.to_string())),
        ("code".into(), Norm::str(&k.code.to_string())),
        ("mods".into(), mods(k.modifiers)),
        ("repeat".into(), Norm::bool(k.is_auto_repeating)),
    ]
}

/// The event's own data, as the protocol's field map (spec §7.3). Fields a
/// type does not have are absent.
pub fn event_fields(d: &DomEventData) -> Vec<(String, Norm)> {
    use DomEventData as E;
    match d {
        E::PointerMove(p)
        | E::PointerDown(p)
        | E::PointerUp(p)
        | E::PointerCancel(p)
        | E::PointerEnter(p)
        | E::PointerLeave(p)
        | E::PointerOver(p)
        | E::PointerOut(p)
        | E::MouseMove(p)
        | E::MouseDown(p)
        | E::MouseUp(p)
        | E::MouseEnter(p)
        | E::MouseLeave(p)
        | E::MouseOver(p)
        | E::MouseOut(p)
        | E::TouchStart(p)
        | E::TouchMove(p)
        | E::TouchEnd(p)
        | E::TouchCancel(p)
        | E::Click(p)
        | E::ContextMenu(p)
        | E::DoubleClick(p) => pointer(p),
        E::KeyPress(k) | E::KeyDown(k) | E::KeyUp(k) => key(k),
        E::Input(i) => vec![("value".into(), Norm::str(&i.value))],
        _ => Vec::new(),
    }
}

impl EventHandler for RhoEventHandler<'_> {
    fn handle_event(&mut self, _chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, state: &mut EventState) {
        let ty = event.data.name();
        if !self.tab.dom.has_listener(ty) {
            return;
        }
        let d = self
            .tab
            .dispatch_ext(BNode::from(event.target), ty, event_fields(&event.data), event.bubbles);
        *self.touched = true;
        if d.prevent && event.cancelable {
            state.prevent_default();
        }
        if d.stop {
            state.stop_propagation();
        }
        state.request_redraw();
    }
}
