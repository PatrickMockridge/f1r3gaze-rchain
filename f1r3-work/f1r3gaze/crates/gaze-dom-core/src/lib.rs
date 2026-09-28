//! `gaze-dom-core` — the DOM protocol (spec §7), implemented once over a
//! [`DomBackend`], so the native Blitz bridge and the reach-tier bridge are the
//! same code and differ only in their backend.
//!
//! Names. An element name stands for a pair *(root, node)*: the node it
//! addresses and the root of the subtree its holder may reach. `doc` is
//! *(document, document)*; a name obtained through a name inherits that name's
//! root, and `attenuate` mints a name whose root is its own node, which is
//! what a page hands to a component to confine it to a subtree. Every request
//! is checked against the root (attenuation is the tree),
//! and asking twice for the same pair yields the same name, so name equality
//! is node equality within one authority.
//!
//! Frames. Reads are answered at once from the document as last committed;
//! writes are queued and applied together by [`Engine::commit`]. The tab
//! executive delivers every reply in the next frame, so a page never observes
//! a half-applied frame.

#![forbid(unsafe_code)]

pub mod mem;

use k1ndl1ng_norm::hash::blake2b_256;
use k1ndl1ng_norm::{CollKind, Name, Node as NNode, Norm};
use std::collections::BTreeMap;
use std::fmt::Debug;

pub type Key = [u8; 32];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pos {
    Append,
    Prepend,
    Before,
    After,
    ReplaceWith,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClassOp {
    Add,
    Remove,
    Toggle,
}

/// A fragment to insert: structured (preferred) or HTML text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Frag {
    El {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<Frag>,
        /// Set by a `("ref", k)` marker among the children.
        reference: Option<String>,
    },
    Text(String),
    Html(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Write<N> {
    SetAttr(N, String, String),
    RemoveAttr(N, String),
    SetText(N, String),
    SetValue(N, String),
    Class(N, ClassOp, String),
    Style(N, String, Option<String>),
    Insert(N, Pos, Frag),
    SetHtml(N, String),
    Remove(N),
    Focus(N),
    Blur(N),
}

/// What a document engine must offer. Implemented by `mem::MemDom` here, by
/// `gaze-dom-blitz` over `blitz-dom`, and by `gaze-reach` over a host DOM.
pub trait DomBackend {
    type Node: Copy + Ord + Eq + Debug;
    fn root(&self) -> Self::Node;
    fn parent(&self, n: Self::Node) -> Option<Self::Node>;
    fn children(&self, n: Self::Node) -> Vec<Self::Node>;
    fn attached(&self, n: Self::Node) -> bool;
    fn is_element(&self, n: Self::Node) -> bool;
    fn attr(&self, n: Self::Node, k: &str) -> Option<String>;
    fn text(&self, n: Self::Node) -> String;
    /// Matches strictly inside `scope`, in document order. Selector matching
    /// must not look above `scope`, so that a query reveals nothing outside it.
    fn query(&self, scope: Self::Node, sel: &str, all: bool) -> Result<Vec<Self::Node>, String>;
    /// The nearest inclusive ancestor of `n` matching `sel`, not above `scope`.
    fn closest(&self, n: Self::Node, sel: &str, scope: Self::Node) -> Result<Option<Self::Node>, String>;
    fn rect(&self, n: Self::Node) -> Option<[i64; 4]>;
    /// Apply one write; return the nodes created for `("ref", k)` markers.
    fn apply(&mut self, w: &Write<Self::Node>) -> Vec<(String, Self::Node)>;
    /// A canonical serialisation of the committed document.
    fn serialize(&self) -> String;

    fn is_inclusive_descendant(&self, n: Self::Node, of: Self::Node) -> bool {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if c == of {
                return true;
            }
            cur = self.parent(c);
        }
        false
    }
}

/// A send the engine wants delivered to the page: `chan!(args...)`.
#[derive(Clone, Debug)]
pub struct Reply {
    pub chan: Name,
    pub args: Vec<Norm>,
}

pub fn ok(v: Norm) -> Norm {
    Norm::tuple(vec![Norm::str("ok"), v])
}
pub fn err(code: &str, detail: &str) -> Norm {
    Norm::tuple(vec![Norm::str("err"), Norm::str(code), Norm::str(detail)])
}
pub fn name_norm(k: &Key) -> Norm {
    Norm::eval(Name::Unforgeable(*k))
}
/// The name a process argument carries, if it is `*x`.
pub fn as_name(n: &Norm) -> Option<Name> {
    match n.node() {
        NNode::Eval(x) => Some(x.clone()),
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum Entry<N> {
    Element { node: N, root: N },
    Sub,
}

#[derive(Clone, Debug)]
struct Listener<N> {
    node: N,
    root: N,
    ty: String,
    ch: Name,
    capture: bool,
    once: bool,
    prevent: bool,
    stop: bool,
    order: u64,
}

struct Pending<N> {
    write: Write<N>,
    ret: Option<Name>,
    root: N,
}

/// What dispatching one event produced.
#[derive(Debug, Default)]
pub struct Fired {
    pub injections: Vec<Reply>,
    pub prevent: bool,
    pub stop: bool,
}

pub struct Engine<B: DomBackend> {
    pub backend: B,
    seed: [u8; 32],
    counter: u64,
    entries: BTreeMap<Key, Entry<B::Node>>,
    by_pair: BTreeMap<(B::Node, B::Node), Key>,
    listeners: BTreeMap<Key, Listener<B::Node>>,
    order: u64,
    batch: Vec<Pending<B::Node>>,
    new_keys: Vec<Key>,
}

impl<B: DomBackend> Engine<B> {
    pub fn new(backend: B, seed: [u8; 32]) -> Engine<B> {
        Engine {
            backend,
            seed,
            counter: 0,
            entries: BTreeMap::new(),
            by_pair: BTreeMap::new(),
            listeners: BTreeMap::new(),
            order: 0,
            batch: Vec::new(),
            new_keys: Vec::new(),
        }
    }

    fn mint(&mut self) -> Key {
        let mut pre = b"gaze/dom/".to_vec();
        pre.extend_from_slice(&self.seed);
        pre.extend_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        let k = blake2b_256(&pre).0;
        self.new_keys.push(k);
        k
    }

    /// The name for `node` under the authority of `root`.
    pub fn name_for(&mut self, root: B::Node, node: B::Node) -> Key {
        if let Some(k) = self.by_pair.get(&(root, node)) {
            return *k;
        }
        let k = self.mint();
        self.entries.insert(k, Entry::Element { node, root });
        self.by_pair.insert((root, node), k);
        k
    }

    /// The `doc` capability: the whole document.
    pub fn grant_document(&mut self) -> Key {
        let r = self.backend.root();
        self.name_for(r, r)
    }

    /// Keys minted since the last call; the executive binds each as an aperture.
    pub fn take_new_keys(&mut self) -> Vec<Key> {
        std::mem::take(&mut self.new_keys)
    }

    pub fn node_of(&self, k: &Key) -> Option<B::Node> {
        match self.entries.get(k) {
            Some(Entry::Element { node, .. }) => Some(*node),
            _ => None,
        }
    }

    pub fn owns(&self, k: &Key) -> bool {
        self.entries.contains_key(k)
    }

    /// Handle one request sent on the engine's name `k`. Returns the replies
    /// that are ready now (reads, and errors); writes reply at commit.
    pub fn handle(&mut self, k: &Key, args: &[Norm]) -> Vec<Reply> {
        let entry = match self.entries.get(k) {
            Some(e) => e.clone(),
            None => return Vec::new(),
        };
        let verb = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        match entry {
            Entry::Sub => {
                if verb == "cancel" {
                    self.listeners.remove(k);
                }
                Vec::new()
            }
            Entry::Element { node, root } => self.element(node, root, &verb, &args[args.len().min(1)..]),
        }
    }

    fn element(&mut self, node: B::Node, root: B::Node, verb: &str, a: &[Norm]) -> Vec<Reply> {
        // The return channel, where one is expected, is the last argument.
        let ret = a.last().and_then(as_name);
        let reply = |v: Norm| -> Vec<Reply> {
            match &ret {
                Some(r) => vec![Reply {
                    chan: r.clone(),
                    args: vec![v],
                }],
                None => Vec::new(),
            }
        };
        if !self.backend.attached(node) {
            return reply(err("detached", verb));
        }
        let s = |i: usize| a.get(i).and_then(|x| x.as_str()).map(str::to_string);
        let bt = |_: ()| err("type", verb);
        match verb {
            "query1" | "query" => {
                let Some(sel) = s(0) else { return reply(bt(())) };
                match self.backend.query(node, &sel, verb == "query") {
                    Err(e) => reply(err("selector", &e)),
                    Ok(v) if verb == "query1" => match v.first() {
                        Some(n) => {
                            let k = self.name_for(root, *n);
                            reply(ok(name_norm(&k)))
                        }
                        None => reply(err("none", &sel)),
                    },
                    Ok(v) => {
                        let names = v.iter().map(|n| name_norm(&self.name_for(root, *n))).collect();
                        reply(ok(Norm::list(names)))
                    }
                }
            }
            // A name for this node whose authority is this node's subtree
            // alone: what a page hands to a component it wants to confine.
            "attenuate" => {
                let k = self.name_for(node, node);
                reply(ok(name_norm(&k)))
            }
            "children" => {
                let kids: Vec<B::Node> =
                    self.backend.children(node).into_iter().filter(|c| self.backend.is_element(*c)).collect();
                let names = kids.iter().map(|n| name_norm(&self.name_for(root, *n))).collect();
                reply(ok(Norm::list(names)))
            }
            "parent" => match self.backend.parent(node) {
                Some(p) if node != root && self.backend.is_inclusive_descendant(p, root) => {
                    let k = self.name_for(root, p);
                    reply(ok(name_norm(&k)))
                }
                _ => reply(err("attenuated", "parent")),
            },
            "closest" => {
                let Some(sel) = s(0) else { return reply(bt(())) };
                match self.backend.closest(node, &sel, root) {
                    Err(e) => reply(err("selector", &e)),
                    Ok(Some(n)) if self.backend.is_inclusive_descendant(n, root) => {
                        let k = self.name_for(root, n);
                        reply(ok(name_norm(&k)))
                    }
                    Ok(_) => reply(err("none", &sel)),
                }
            }
            "attr" => {
                let Some(k) = s(0) else { return reply(bt(())) };
                reply(ok(match self.backend.attr(node, &k) {
                    Some(v) => Norm::str(&v),
                    None => Norm::nil(),
                }))
            }
            "text" => reply(ok(Norm::str(&self.backend.text(node)))),
            "value" => reply(ok(Norm::str(&self.backend.attr(node, "value").unwrap_or_default()))),
            "rect" => match self.backend.rect(node) {
                Some([x, y, w, h]) => {
                    reply(ok(Norm::tuple(vec![Norm::int(x), Norm::int(y), Norm::int(w), Norm::int(h)])))
                }
                None => reply(err("layout", "no layout")),
            },
            "listen" => {
                let (Some(ty), Some(ch)) = (s(0), a.get(1).and_then(as_name)) else {
                    return reply(bt(()));
                };
                let flag = |k: &str| a.get(2).and_then(|m| m.map_get(k)).and_then(|v| v.as_bool()).unwrap_or(false);
                let sub = self.mint();
                self.entries.insert(sub, Entry::Sub);
                self.order += 1;
                self.listeners.insert(
                    sub,
                    Listener {
                        node,
                        root,
                        ty,
                        ch,
                        capture: flag("capture"),
                        once: flag("once"),
                        prevent: flag("prevent"),
                        stop: flag("stop"),
                        order: self.order,
                    },
                );
                // `ret` is the fourth argument when present; the third is opts.
                if a.len() >= 4 { reply(ok(name_norm(&sub))) } else { Vec::new() }
            }
            _ => self.queue_write(node, root, verb, a, ret),
        }
    }

    fn queue_write(&mut self, node: B::Node, root: B::Node, verb: &str, a: &[Norm], ret: Option<Name>) -> Vec<Reply> {
        let s = |i: usize| a.get(i).and_then(|x| x.as_str()).map(str::to_string);
        let fail = |code: &str| -> Vec<Reply> {
            match &ret {
                Some(r) => vec![Reply {
                    chan: r.clone(),
                    args: vec![err(code, verb)],
                }],
                None => Vec::new(),
            }
        };
        // Writes that would reach outside the holder's subtree.
        let at_root = node == root;
        let w = match verb {
            "setAttr" => match (s(0), s(1)) {
                (Some(k), Some(v)) => Write::SetAttr(node, k, v),
                _ => return fail("type"),
            },
            "removeAttr" => match s(0) {
                Some(k) => Write::RemoveAttr(node, k),
                None => return fail("type"),
            },
            "setText" => match s(0) {
                Some(t) => Write::SetText(node, t),
                None => return fail("type"),
            },
            "setValue" => match s(0) {
                Some(t) => Write::SetValue(node, t),
                None => return fail("type"),
            },
            "classAdd" | "classRemove" | "classToggle" => match s(0) {
                Some(c) => Write::Class(
                    node,
                    match verb {
                        "classAdd" => ClassOp::Add,
                        "classRemove" => ClassOp::Remove,
                        _ => ClassOp::Toggle,
                    },
                    c,
                ),
                None => return fail("type"),
            },
            "style" => match (s(0), s(1)) {
                (Some(k), Some(v)) => Write::Style(node, k, Some(v)),
                _ => return fail("type"),
            },
            "unstyle" => match s(0) {
                Some(k) => Write::Style(node, k, None),
                None => return fail("type"),
            },
            "append" | "prepend" | "before" | "after" | "replaceWith" => {
                let pos = match verb {
                    "append" => Pos::Append,
                    "prepend" => Pos::Prepend,
                    "before" => Pos::Before,
                    "after" => Pos::After,
                    _ => Pos::ReplaceWith,
                };
                if at_root && matches!(pos, Pos::Before | Pos::After | Pos::ReplaceWith) {
                    return fail("attenuated");
                }
                match a.first().and_then(frag_of) {
                    Some(f) => Write::Insert(node, pos, f),
                    None => return fail("type"),
                }
            }
            "setHTML" => match s(0) {
                Some(h) => Write::SetHtml(node, h),
                None => return fail("type"),
            },
            "remove" => {
                if at_root {
                    return fail("attenuated");
                }
                Write::Remove(node)
            }
            "focus" => Write::Focus(node),
            "blur" => Write::Blur(node),
            _ => return fail("verb"),
        };
        self.batch.push(Pending { write: w, ret, root });
        Vec::new()
    }

    /// Apply the frame's writes together. Returns the replies they produce and
    /// the hash of the committed document.
    pub fn commit(&mut self) -> (Vec<Reply>, [u8; 32]) {
        let mut out = Vec::new();
        let batch = std::mem::take(&mut self.batch);
        for p in batch {
            let target = match &p.write {
                Write::SetAttr(n, ..)
                | Write::RemoveAttr(n, _)
                | Write::SetText(n, _)
                | Write::SetValue(n, _)
                | Write::Class(n, ..)
                | Write::Style(n, ..)
                | Write::Insert(n, ..)
                | Write::SetHtml(n, _)
                | Write::Remove(n)
                | Write::Focus(n)
                | Write::Blur(n) => *n,
            };
            if !self.backend.attached(target) {
                if let Some(r) = p.ret {
                    out.push(Reply {
                        chan: r,
                        args: vec![err("detached", "write")],
                    });
                }
                continue;
            }
            let created = self.backend.apply(&p.write);
            if let Some(r) = p.ret {
                let refs: Vec<(Norm, Norm)> = created
                    .iter()
                    .map(|(k, n)| (Norm::str(k), name_norm(&self.name_for(p.root, *n))))
                    .collect();
                out.push(Reply {
                    chan: r,
                    args: vec![ok(Norm::map(refs))],
                });
            }
        }
        (out, self.doc_hash())
    }

    pub fn doc_hash(&self) -> [u8; 32] {
        blake2b_256(self.backend.serialize().as_bytes()).0
    }

    pub fn pending_writes(&self) -> usize {
        self.batch.len()
    }

    /// Dispatch one event at `target`: capture listeners root to target, then
    /// bubble listeners target to root, honouring static `stop`, `prevent`
    /// and `once`. `fields` are the event's own data.
    pub fn fire(&mut self, target: B::Node, ty: &str, fields: Vec<(String, Norm)>) -> Fired {
        let mut chain = Vec::new();
        let mut cur = Some(target);
        while let Some(c) = cur {
            chain.push(c);
            cur = self.backend.parent(c);
        }
        chain.reverse(); // root .. target
        let mut fired = Fired::default();
        let phases: [(bool, Vec<B::Node>); 2] =
            [(true, chain.clone()), (false, chain.iter().rev().copied().collect())];
        for (capture, nodes) in phases {
            for n in nodes {
                let mut here: Vec<(Key, Listener<B::Node>)> = self
                    .listeners
                    .iter()
                    .filter(|(_, l)| l.node == n && l.ty == ty && l.capture == capture)
                    .map(|(k, l)| (*k, l.clone()))
                    .collect();
                here.sort_by_key(|(_, l)| l.order);
                for (sub, l) in here {
                    let t = self.name_for(l.root, target);
                    let mut m = fields.clone();
                    m.push(("target".into(), name_norm(&t)));
                    m.push(("type".into(), Norm::str(ty)));
                    let datum = Norm::tuple(vec![
                        Norm::str(ty),
                        Norm::map(m.into_iter().map(|(k, v)| (Norm::str(&k), v)).collect()),
                    ]);
                    fired.injections.push(Reply {
                        chan: l.ch.clone(),
                        args: vec![datum],
                    });
                    fired.prevent |= l.prevent;
                    fired.stop |= l.stop;
                    if l.once {
                        self.listeners.remove(&sub);
                    }
                }
                if fired.stop {
                    return fired;
                }
            }
        }
        fired
    }

    pub fn listener_count(&self) -> usize {
        self.listeners.len()
    }
}

/// Read a fragment term.
pub fn frag_of(n: &Norm) -> Option<Frag> {
    if let Some(h) = n.as_str() {
        return Some(Frag::Html(h.to_string()));
    }
    let t = n.as_coll(CollKind::Tuple)?;
    match t.first()?.as_str()? {
        "text" => Some(Frag::Text(t.get(1)?.as_str()?.to_string())),
        "el" => {
            let tag = t.get(1)?.as_str()?.to_string();
            let mut attrs = Vec::new();
            if let Some(m) = t.get(2) {
                if !m.is_nil() {
                    for kv in m.as_coll(CollKind::Map)?.chunks(2) {
                        attrs.push((kv[0].as_str()?.to_string(), kv[1].as_str()?.to_string()));
                    }
                }
            }
            let mut children = Vec::new();
            let mut reference = None;
            if let Some(l) = t.get(3) {
                for c in l.as_coll(CollKind::List)? {
                    if let Some(ct) = c.as_coll(CollKind::Tuple) {
                        if ct.first().and_then(|x| x.as_str()) == Some("ref") {
                            reference = Some(ct.get(1)?.as_str()?.to_string());
                            continue;
                        }
                    }
                    children.push(frag_of(c)?);
                }
            }
            Some(Frag::El {
                tag,
                attrs,
                children,
                reference,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
