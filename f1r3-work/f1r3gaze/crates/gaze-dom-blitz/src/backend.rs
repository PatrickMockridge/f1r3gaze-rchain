//! [`BlitzDom`]: the DOM protocol's backend over `blitz-dom`.
//!
//! Two things differ from a naive mapping onto Blitz's API, both for the
//! protocol's guarantees:
//!
//! * **Scoped queries are confined.** `BaseDocument::query_selector_all_in`
//!   lets selector parts match *above* the scope while relating descendants,
//!   so `.outer span` asked of a component's subtree would reveal whether an
//!   ancestor is `.outer`. Every complex selector is therefore rewritten to
//!   start at `:scope`, which forces its leftmost compound, and so every
//!   compound after it, to lie inside the subtree.
//! * **The committed document serialises canonically**, exactly as
//!   `gaze_dom_core::mem::MemDom` does (elements, attributes in order, text;
//!   comments and doctypes omitted), so the same page hashes the same on both
//!   backends and the conformance corpus is shared.
//!
//! A frame's writes are applied under one `DocumentMutator`, which flushes
//! once when dropped.

use blitz_dom::{Attribute, BaseDocument, DocumentMutator, LocalName, NodeData, NodeId, QualName, ns};
use gaze_dom_core::{ClassOp, DomBackend, Frag, Pos, Write, write_target};
use std::cell::RefCell;
use std::rc::Rc;

/// A Blitz node id with a total order (Blitz's `NodeId` is not `Ord`).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BNode(pub u64);

impl From<NodeId> for BNode {
    fn from(n: NodeId) -> BNode {
        BNode(n.as_u64())
    }
}
impl BNode {
    pub fn id(self) -> NodeId {
        NodeId::from_u64(self.0)
    }
}

const VOID: &[&str] = &["br", "img", "input", "meta", "link", "hr", "area", "base", "col", "source", "wbr"];

/// The backend. It shares the document with the `RhoDocument` that owns it.
#[derive(Clone)]
pub struct BlitzDom {
    pub doc: Rc<RefCell<BaseDocument>>,
}

fn html_name(tag: &str) -> QualName {
    QualName::new(None, ns!(html), LocalName::from(tag.to_ascii_lowercase()))
}
fn attr_name(k: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(k.to_ascii_lowercase()))
}

/// Split a selector list at top-level commas.
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

/// `a, b > c` becomes `:scope a, :scope b > c`; a relative `> c` becomes
/// `:scope > c`.
pub fn scope_selector(sel: &str) -> String {
    split_list(sel)
        .into_iter()
        .map(|p| format!(":scope {}", p.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a selector has a combinator at top level (so matching it at the
/// scope node itself would need nodes above the scope).
fn has_combinator(sel: &str) -> bool {
    let (mut depth, mut quote) = (0i32, None::<char>);
    let t = sel.trim();
    for c in t.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ' ' | '>' | '+' | '~' | '\t' | '\n') if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn escape(s: &str, attr: bool) -> String {
    let s = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    if attr { s.replace('"', "&quot;") } else { s }
}

impl BlitzDom {
    pub fn new(doc: Rc<RefCell<BaseDocument>>) -> BlitzDom {
        BlitzDom { doc }
    }

    /// Parse `html` into a fresh document (headless: no fonts, no network).
    pub fn from_html(html: &str) -> BlitzDom {
        BlitzDom::new(Rc::new(RefCell::new(crate::parse_html(html, blitz_dom::DocumentConfig::default()))))
    }

    /// First element with `id`, for tests and hosts.
    pub fn by_id(&self, id: &str) -> Option<BNode> {
        self.doc.borrow().get_element_by_id(id).map(BNode::from)
    }

    pub fn classes(&self, n: BNode) -> Vec<String> {
        self.attr(n, "class")
            .map(|c| c.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn scoped_all(&self, doc: &BaseDocument, scope: BNode, sel: &str) -> Result<Vec<BNode>, String> {
        let root = doc.root_node().id;
        let ids = if scope.id() == root {
            doc.query_selector_all_in(root, sel).map_err(|e| format!("{e:?}"))?
        } else {
            let scoped = scope_selector(sel);
            // Parse the original too, so errors name what the page wrote.
            doc.try_parse_selector_list(sel).map_err(|e| format!("{e:?}"))?;
            doc.query_selector_all_in(scope.id(), &scoped).map_err(|e| format!("{e:?}"))?
        };
        Ok(ids.into_iter().map(BNode::from).collect())
    }

    fn serialize_into(doc: &BaseDocument, id: NodeId, out: &mut String) {
        let Some(node) = doc.get_node(id) else { return };
        match &node.data {
            NodeData::Document(_) => {
                for c in node.children.iter() {
                    Self::serialize_into(doc, *c, out);
                }
            }
            NodeData::Text(t) => out.push_str(&escape(&t.content, false)),
            NodeData::Element(el) => {
                let tag = el.name.local.as_ref();
                out.push('<');
                out.push_str(tag);
                for a in el.attrs().iter() {
                    out.push(' ');
                    out.push_str(a.name.local.as_ref());
                    out.push_str("=\"");
                    out.push_str(&escape(&a.value, true));
                    out.push('"');
                }
                out.push('>');
                if VOID.contains(&tag) {
                    return;
                }
                for c in node.children.iter() {
                    Self::serialize_into(doc, *c, out);
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
            _ => {} // comments, doctypes, anonymous blocks
        }
    }
}

fn get_attr(doc: &BaseDocument, n: NodeId, k: &str) -> Option<String> {
    let el = doc.get_node(n)?.element_data()?;
    el.attrs().iter().find(|a| a.name.local.as_ref() == k).map(|a| a.value.clone())
}

fn attached_in(doc: &BaseDocument, n: NodeId) -> bool {
    let root = doc.root_node().id;
    let mut cur = n;
    loop {
        let Some(node) = doc.get_node(cur) else { return false };
        if cur == root {
            return true;
        }
        match node.parent {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// Build the nodes of a fragment, detached. Returns (nodes, refs).
fn build(m: &mut DocumentMutator<'_>, f: &Frag, refs: &mut Vec<(String, BNode)>) -> Vec<NodeId> {
    match f {
        Frag::Text(t) => vec![m.create_text_node(t)],
        Frag::Html(h) => {
            let tmp = m.create_element(html_name("div"), Vec::new());
            m.set_inner_html(tmp, h);
            let kids: Vec<NodeId> = m.child_ids(tmp).to_vec();
            for k in &kids {
                m.remove_node(*k);
            }
            m.remove_and_drop_node(tmp);
            kids
        }
        Frag::El {
            tag,
            attrs,
            children,
            reference,
        } => {
            let attrs = attrs
                .iter()
                .map(|(k, v)| Attribute {
                    name: attr_name(k),
                    value: v.clone(),
                })
                .collect();
            let el = m.create_element(html_name(tag), attrs);
            if let Some(r) = reference {
                refs.push((r.clone(), BNode::from(el)));
            }
            let mut kids = Vec::new();
            for c in children {
                kids.extend(build(m, c, refs));
            }
            if !kids.is_empty() {
                m.append_children(el, &kids);
            }
            vec![el]
        }
    }
}

fn set_or_remove(m: &mut DocumentMutator<'_>, n: NodeId, k: &str, v: Option<String>) {
    match v {
        Some(v) => m.set_attribute(n, attr_name(k), &v),
        None => m.clear_attribute(n, attr_name(k)),
    }
}

/// Apply one write inside a mutation session. Focus changes are returned for
/// the caller to apply after the session, since they need the document.
fn apply_one(m: &mut DocumentMutator<'_>, w: &Write<BNode>, focus: &mut Vec<(BNode, bool)>) -> Vec<(String, BNode)> {
    let mut refs = Vec::new();
    match w {
        Write::SetAttr(n, k, v) => m.set_attribute(n.id(), attr_name(k), v),
        Write::RemoveAttr(n, k) => m.clear_attribute(n.id(), attr_name(k)),
        Write::SetValue(n, v) => m.set_attribute(n.id(), attr_name("value"), v),
        Write::SetText(n, t) => {
            m.remove_and_drop_all_children(n.id());
            // Always one text node, even when empty, so the tree (and the
            // replay log's paths) agree with the reference backend.
            let tn = m.create_text_node(t);
            m.append_children(n.id(), &[tn]);
        }
        Write::Class(n, op, c) => {
            let cur = get_attr(m.doc, n.id(), "class").unwrap_or_default();
            let mut cs: Vec<String> = cur.split_whitespace().map(str::to_string).collect();
            let has = cs.contains(c);
            match (op, has) {
                (ClassOp::Add, false) | (ClassOp::Toggle, false) => cs.push(c.clone()),
                (ClassOp::Remove, true) | (ClassOp::Toggle, true) => cs.retain(|x| x != c),
                _ => {}
            }
            set_or_remove(m, n.id(), "class", if cs.is_empty() { None } else { Some(cs.join(" ")) });
        }
        Write::Style(n, k, v) => {
            let cur = get_attr(m.doc, n.id(), "style").unwrap_or_default();
            let mut decls: Vec<(String, String)> = cur
                .split(';')
                .filter_map(|d| d.split_once(':'))
                .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
                .collect();
            decls.retain(|(x, _)| x != k);
            if let Some(v) = v {
                decls.push((k.clone(), v.clone()));
            }
            let s: Vec<String> = decls.iter().map(|(a, b)| format!("{a}: {b}")).collect();
            set_or_remove(m, n.id(), "style", if s.is_empty() { None } else { Some(s.join("; ")) });
        }
        Write::Insert(n, pos, f) => {
            let made = build(m, f, &mut refs);
            let n = n.id();
            match pos {
                Pos::Append => m.append_children(n, &made),
                Pos::Prepend => m.prepend_nodes(n, &made),
                Pos::Before => m.insert_nodes_before(n, &made),
                Pos::After => m.insert_nodes_after(n, &made),
                Pos::ReplaceWith => m.replace_node_with(n, &made),
            }
        }
        Write::SetHtml(n, h) => m.set_inner_html(n.id(), h),
        Write::Remove(n) => {
            m.remove_and_drop_node(n.id());
        }
        Write::Focus(n) => focus.push((*n, true)),
        Write::Blur(n) => focus.push((*n, false)),
    }
    refs
}

impl DomBackend for BlitzDom {
    type Node = BNode;

    fn root(&self) -> BNode {
        BNode::from(self.doc.borrow().root_node().id)
    }
    fn parent(&self, n: BNode) -> Option<BNode> {
        self.doc.borrow().get_node(n.id())?.parent.map(BNode::from)
    }
    fn children(&self, n: BNode) -> Vec<BNode> {
        self.doc
            .borrow()
            .get_node(n.id())
            .map(|x| x.children.iter().copied().map(BNode::from).collect())
            .unwrap_or_default()
    }
    fn attached(&self, n: BNode) -> bool {
        attached_in(&self.doc.borrow(), n.id())
    }
    fn is_element(&self, n: BNode) -> bool {
        self.doc.borrow().get_node(n.id()).is_some_and(|x| x.is_element())
    }
    fn attr(&self, n: BNode, k: &str) -> Option<String> {
        let doc = self.doc.borrow();
        // A text control's live value is its editor's, not its attribute.
        if k == "value" {
            if let Some(ti) = doc.get_node(n.id()).and_then(|x| x.element_data()).and_then(|e| e.text_input_data()) {
                return Some(ti.editor.raw_text().to_string());
            }
        }
        get_attr(&doc, n.id(), k)
    }
    fn text(&self, n: BNode) -> String {
        self.doc.borrow().get_node(n.id()).map(|x| x.text_content()).unwrap_or_default()
    }
    fn query(&self, scope: BNode, sel: &str, all: bool) -> Result<Vec<BNode>, String> {
        let doc = self.doc.borrow();
        let mut v = self.scoped_all(&doc, scope, sel)?;
        if !all {
            v.truncate(1);
        }
        Ok(v)
    }
    fn closest(&self, n: BNode, sel: &str, scope: BNode) -> Result<Option<BNode>, String> {
        let doc = self.doc.borrow();
        let root = BNode::from(doc.root_node().id);
        let matches_here = |c: BNode| -> Result<bool, String> {
            doc.matches_selector(c.id(), sel).map_err(|e| format!("{e:?}"))
        };
        if scope == root {
            let mut cur = Some(n);
            while let Some(c) = cur {
                if matches_here(c)? {
                    return Ok(Some(c));
                }
                cur = doc.get_node(c.id()).and_then(|x| x.parent).map(BNode::from);
            }
            return Ok(None);
        }
        // Confined: a candidate strictly inside the scope must match with its
        // relationships inside the scope; the scope itself may match only a
        // selector with no combinator.
        let inside: std::collections::BTreeSet<BNode> = self.scoped_all(&doc, scope, sel)?.into_iter().collect();
        let mut cur = Some(n);
        while let Some(c) = cur {
            if c == scope {
                return Ok(if !has_combinator(sel) && matches_here(c)? { Some(c) } else { None });
            }
            if inside.contains(&c) {
                return Ok(Some(c));
            }
            cur = doc.get_node(c.id()).and_then(|x| x.parent).map(BNode::from);
        }
        Ok(None)
    }
    fn rect(&self, n: BNode) -> Option<[i64; 4]> {
        let r = self.doc.borrow().get_client_bounding_rect(n.id())?;
        Some([r.x.round() as i64, r.y.round() as i64, r.width.round() as i64, r.height.round() as i64])
    }
    fn apply(&mut self, w: &Write<BNode>) -> Vec<(String, BNode)> {
        self.apply_batch(std::slice::from_ref(w)).pop().flatten().unwrap_or_default()
    }
    fn apply_batch(&mut self, ws: &[Write<BNode>]) -> Vec<Option<Vec<(String, BNode)>>> {
        let mut out = Vec::with_capacity(ws.len());
        let mut focus = Vec::new();
        {
            let mut doc = self.doc.borrow_mut();
            let mut m = doc.mutate();
            for w in ws {
                if attached_in(m.doc, write_target(w).id()) {
                    out.push(Some(apply_one(&mut m, w, &mut focus)));
                } else {
                    out.push(None);
                }
            }
            // `m` drops here: one flush for the whole frame.
        }
        let mut doc = self.doc.borrow_mut();
        for (n, on) in focus {
            if !attached_in(&doc, n.id()) {
                continue;
            }
            if on {
                doc.set_focus_to(n.id());
            } else if doc.get_focussed_node_id() == Some(n.id()) {
                doc.clear_focus();
            }
        }
        out
    }
    fn serialize(&self) -> String {
        let doc = self.doc.borrow();
        let mut s = String::new();
        Self::serialize_into(&doc, doc.root_node().id, &mut s);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoping_rewrites_each_complex_selector() {
        assert_eq!(scope_selector("a, b > c"), ":scope a, :scope b > c");
        assert_eq!(scope_selector("> li"), ":scope > li");
        assert_eq!(scope_selector("a[x=\"1,2\"], :is(b, c)"), ":scope a[x=\"1,2\"], :scope :is(b, c)");
        assert!(has_combinator(".a .b") && has_combinator("a>b") && !has_combinator("a.b[x='y z']"));
    }
}
