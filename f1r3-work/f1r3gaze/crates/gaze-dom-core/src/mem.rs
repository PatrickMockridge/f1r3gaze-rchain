//! `MemDom` — an in-memory reference [`DomBackend`]. It has no layout and no
//! styling; it exists so the protocol engine, the tab executive and the
//! conformance suite can run anywhere, including headless and in CI, and so
//! the Blitz backend has an oracle to be compared against.
//!
//! HTML support is a deliberately small subset (elements, attributes, text,
//! comments skipped, void elements, `&amp; &lt; &gt; &quot; &#39;`).
//! Selectors: type, `#id`, `.class`, `[attr]`, `[attr=v]`, `*`, compound
//! selectors, the descendant and child combinators, and `,` lists.

use crate::{ClassOp, DomBackend, Frag, Pos, Write};

#[derive(Clone, Debug)]
enum Kind {
    Doc,
    El { tag: String, attrs: Vec<(String, String)> },
    Text(String),
}

#[derive(Clone, Debug)]
struct MNode {
    parent: Option<usize>,
    children: Vec<usize>,
    kind: Kind,
}

#[derive(Clone, Debug)]
pub struct MemDom {
    nodes: Vec<MNode>,
    focus: Option<usize>,
}

const VOID: &[&str] = &["br", "img", "input", "meta", "link", "hr", "area", "base", "col", "source", "wbr"];

impl MemDom {
    pub fn from_html(html: &str) -> MemDom {
        let mut d = MemDom {
            nodes: vec![MNode {
                parent: None,
                children: Vec::new(),
                kind: Kind::Doc,
            }],
            focus: None,
        };
        let kids = d.parse_into(html);
        for k in kids {
            d.link(0, k, None);
        }
        d
    }

    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// First element with `id`, for tests and hosts.
    pub fn by_id(&self, id: &str) -> Option<usize> {
        self.query(0, &format!("#{id}"), false).ok()?.first().copied()
    }

    pub fn classes(&self, n: usize) -> Vec<String> {
        self.attr(n, "class")
            .map(|c| c.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn new_node(&mut self, kind: Kind) -> usize {
        self.nodes.push(MNode {
            parent: None,
            children: Vec::new(),
            kind,
        });
        self.nodes.len() - 1
    }

    fn unlink(&mut self, n: usize) {
        if let Some(p) = self.nodes[n].parent.take() {
            self.nodes[p].children.retain(|c| *c != n);
        }
    }

    /// Attach `n` under `parent`, before `before` if given.
    fn link(&mut self, parent: usize, n: usize, before: Option<usize>) {
        self.unlink(n);
        self.nodes[n].parent = Some(parent);
        let at = before
            .and_then(|b| self.nodes[parent].children.iter().position(|c| *c == b))
            .unwrap_or(self.nodes[parent].children.len());
        self.nodes[parent].children.insert(at, n);
    }

    fn parse_into(&mut self, html: &str) -> Vec<usize> {
        let b = html.as_bytes();
        let mut i = 0;
        let mut top: Vec<usize> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        let mut text = String::new();
        macro_rules! flush {
            () => {
                if !text.is_empty() {
                    let t = self.new_node(Kind::Text(decode(&text)));
                    match open.last() {
                        Some(p) => self.link(*p, t, None),
                        None => top.push(t),
                    }
                    text.clear();
                }
            };
        }
        while i < b.len() {
            if b[i] == b'<' {
                if html[i..].starts_with("<!--") {
                    flush!();
                    i = html[i..].find("-->").map(|j| i + j + 3).unwrap_or(b.len());
                    continue;
                }
                if html[i..].starts_with("<!") {
                    flush!();
                    i = html[i..].find('>').map(|j| i + j + 1).unwrap_or(b.len());
                    continue;
                }
                let end = match html[i..].find('>') {
                    Some(j) => i + j,
                    None => {
                        text.push_str(&html[i..]);
                        break;
                    }
                };
                flush!();
                let inner = &html[i + 1..end];
                i = end + 1;
                if let Some(name) = inner.strip_prefix('/') {
                    let name = name.trim().to_ascii_lowercase();
                    if let Some(pos) = open.iter().rposition(|n| self.tag_of(*n) == Some(name.as_str())) {
                        open.truncate(pos);
                    }
                    continue;
                }
                let self_closing = inner.ends_with('/');
                let inner = inner.trim_end_matches('/');
                let (tag, attrs) = parse_tag(inner);
                let el = self.new_node(Kind::El {
                    tag: tag.clone(),
                    attrs,
                });
                match open.last() {
                    Some(p) => self.link(*p, el, None),
                    None => top.push(el),
                }
                if !self_closing && !VOID.contains(&tag.as_str()) {
                    open.push(el);
                }
            } else {
                let next = html[i..].find('<').map(|j| i + j).unwrap_or(b.len());
                text.push_str(&html[i..next]);
                i = next;
            }
        }
        flush!();
        top
    }

    fn tag_of(&self, n: usize) -> Option<&str> {
        match &self.nodes[n].kind {
            Kind::El { tag, .. } => Some(tag),
            _ => None,
        }
    }

    fn attrs_mut(&mut self, n: usize) -> Option<&mut Vec<(String, String)>> {
        match &mut self.nodes[n].kind {
            Kind::El { attrs, .. } => Some(attrs),
            _ => None,
        }
    }

    fn set_attr(&mut self, n: usize, k: &str, v: &str) {
        if let Some(a) = self.attrs_mut(n) {
            match a.iter_mut().find(|(x, _)| x == k) {
                Some(e) => e.1 = v.to_string(),
                None => a.push((k.to_string(), v.to_string())),
            }
        }
    }

    fn remove_attr(&mut self, n: usize, k: &str) {
        if let Some(a) = self.attrs_mut(n) {
            a.retain(|(x, _)| x != k);
        }
    }

    fn build(&mut self, f: &Frag, refs: &mut Vec<(String, usize)>) -> Vec<usize> {
        match f {
            Frag::Text(t) => vec![self.new_node(Kind::Text(t.clone()))],
            Frag::Html(h) => self.parse_into(h),
            Frag::El {
                tag,
                attrs,
                children,
                reference,
            } => {
                let el = self.new_node(Kind::El {
                    tag: tag.to_ascii_lowercase(),
                    attrs: attrs.clone(),
                });
                if let Some(r) = reference {
                    refs.push((r.clone(), el));
                }
                for c in children {
                    for k in self.build(c, refs) {
                        self.link(el, k, None);
                    }
                }
                vec![el]
            }
        }
    }

    fn matches_complex(&self, n: usize, sel: &[(char, Compound)], scope: usize) -> bool {
        // Right to left. `sel` is [(combinator-before, compound)], first comb ' '.
        let Some(((comb, last), rest)) = sel.split_last() else { return false };
        if !self.matches_compound(n, last) {
            return false;
        }
        if rest.is_empty() {
            return true;
        }
        let up = |m: usize| if m == scope { None } else { self.nodes[m].parent };
        match comb {
            '>' => match up(n) {
                Some(p) => self.matches_complex(p, rest, scope),
                None => false,
            },
            _ => {
                let mut cur = up(n);
                while let Some(p) = cur {
                    if self.matches_complex(p, rest, scope) {
                        return true;
                    }
                    cur = up(p);
                }
                false
            }
        }
    }

    fn matches_compound(&self, n: usize, c: &Compound) -> bool {
        let Kind::El { tag, attrs } = &self.nodes[n].kind else { return false };
        let get = |k: &str| attrs.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        if let Some(t) = &c.tag {
            if t != tag {
                return false;
            }
        }
        if let Some(id) = &c.id {
            if get("id") != Some(id.as_str()) {
                return false;
            }
        }
        let classes: Vec<&str> = get("class").map(|v| v.split_whitespace().collect()).unwrap_or_default();
        if !c.classes.iter().all(|k| classes.contains(&k.as_str())) {
            return false;
        }
        c.attrs.iter().all(|(k, v)| match (get(k), v) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(a), Some(b)) => a == b,
        })
    }

    fn serialize_into(&self, n: usize, out: &mut String) {
        match &self.nodes[n].kind {
            Kind::Doc => {
                for c in &self.nodes[n].children {
                    self.serialize_into(*c, out);
                }
            }
            Kind::Text(t) => out.push_str(&escape(t, false)),
            Kind::El { tag, attrs } => {
                out.push('<');
                out.push_str(tag);
                for (k, v) in attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(&escape(v, true));
                    out.push('"');
                }
                out.push('>');
                if VOID.contains(&tag.as_str()) {
                    return;
                }
                for c in &self.nodes[n].children {
                    self.serialize_into(*c, out);
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    fn text_into(&self, n: usize, out: &mut String) {
        match &self.nodes[n].kind {
            Kind::Text(t) => out.push_str(t),
            _ => {
                for c in &self.nodes[n].children {
                    self.text_into(*c, out);
                }
            }
        }
    }
}

impl DomBackend for MemDom {
    type Node = usize;

    fn root(&self) -> usize {
        0
    }
    fn parent(&self, n: usize) -> Option<usize> {
        self.nodes.get(n)?.parent
    }
    fn children(&self, n: usize) -> Vec<usize> {
        self.nodes.get(n).map(|x| x.children.clone()).unwrap_or_default()
    }
    fn attached(&self, n: usize) -> bool {
        n < self.nodes.len() && self.is_inclusive_descendant(n, 0)
    }
    fn is_element(&self, n: usize) -> bool {
        matches!(self.nodes.get(n).map(|x| &x.kind), Some(Kind::El { .. }))
    }
    fn attr(&self, n: usize, k: &str) -> Option<String> {
        match &self.nodes.get(n)?.kind {
            Kind::El { attrs, .. } => attrs.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone()),
            _ => None,
        }
    }
    fn text(&self, n: usize) -> String {
        let mut s = String::new();
        self.text_into(n, &mut s);
        s
    }
    fn query(&self, scope: usize, sel: &str, all: bool) -> Result<Vec<usize>, String> {
        let list = parse_selector(sel)?;
        let mut out = Vec::new();
        let mut stack: Vec<usize> = self.nodes[scope].children.iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            if list.iter().any(|c| self.matches_complex(n, c, scope)) {
                out.push(n);
                if !all {
                    break;
                }
            }
            stack.extend(self.nodes[n].children.iter().rev().copied());
        }
        Ok(out)
    }
    fn closest(&self, n: usize, sel: &str, scope: usize) -> Result<Option<usize>, String> {
        let list = parse_selector(sel)?;
        let mut cur = Some(n);
        while let Some(c) = cur {
            if list.iter().any(|x| self.matches_complex(c, x, scope)) {
                return Ok(Some(c));
            }
            if c == scope {
                break;
            }
            cur = self.nodes[c].parent;
        }
        Ok(None)
    }
    fn rect(&self, _n: usize) -> Option<[i64; 4]> {
        None // no layout in the reference backend
    }
    fn apply(&mut self, w: &Write<usize>) -> Vec<(String, usize)> {
        let mut refs = Vec::new();
        match w {
            Write::SetAttr(n, k, v) => self.set_attr(*n, k, v),
            Write::RemoveAttr(n, k) => self.remove_attr(*n, k),
            Write::SetValue(n, v) => self.set_attr(*n, "value", v),
            Write::SetText(n, t) => {
                for c in self.nodes[*n].children.clone() {
                    self.unlink(c);
                }
                let t = self.new_node(Kind::Text(t.clone()));
                self.link(*n, t, None);
            }
            Write::Class(n, op, c) => {
                let mut cs = self.classes(*n);
                let has = cs.contains(c);
                match (op, has) {
                    (ClassOp::Add, false) | (ClassOp::Toggle, false) => cs.push(c.clone()),
                    (ClassOp::Remove, true) | (ClassOp::Toggle, true) => cs.retain(|x| x != c),
                    _ => {}
                }
                if cs.is_empty() {
                    self.remove_attr(*n, "class");
                } else {
                    self.set_attr(*n, "class", &cs.join(" "));
                }
            }
            Write::Style(n, k, v) => {
                let cur = self.attr(*n, "style").unwrap_or_default();
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
                if s.is_empty() {
                    self.remove_attr(*n, "style");
                } else {
                    self.set_attr(*n, "style", &s.join("; "));
                }
            }
            Write::Insert(n, pos, f) => {
                let made = self.build(f, &mut refs);
                let n = *n;
                match pos {
                    Pos::Append => made.iter().for_each(|m| self.link(n, *m, None)),
                    Pos::Prepend => {
                        let first = self.nodes[n].children.first().copied();
                        made.iter().for_each(|m| self.link(n, *m, first));
                    }
                    Pos::Before | Pos::After | Pos::ReplaceWith => {
                        if let Some(p) = self.nodes[n].parent {
                            let anchor = match pos {
                                Pos::After => {
                                    let sib = &self.nodes[p].children;
                                    sib.iter().position(|c| *c == n).and_then(|i| sib.get(i + 1)).copied()
                                }
                                _ => Some(n),
                            };
                            made.iter().for_each(|m| self.link(p, *m, anchor));
                            if *pos == Pos::ReplaceWith {
                                self.unlink(n);
                            }
                        }
                    }
                }
            }
            Write::SetHtml(n, h) => {
                for c in self.nodes[*n].children.clone() {
                    self.unlink(c);
                }
                for m in self.parse_into(h) {
                    self.link(*n, m, None);
                }
            }
            Write::Remove(n) => self.unlink(*n),
            Write::Focus(n) => self.focus = Some(*n),
            Write::Blur(n) => {
                if self.focus == Some(*n) {
                    self.focus = None;
                }
            }
        }
        refs
    }
    fn serialize(&self) -> String {
        let mut s = String::new();
        self.serialize_into(0, &mut s);
        s
    }
}

#[derive(Clone, Debug, Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<(String, Option<String>)>,
}

fn parse_selector(s: &str) -> Result<Vec<Vec<(char, Compound)>>, String> {
    let mut list = Vec::new();
    for part in s.split(',') {
        let mut complex: Vec<(char, Compound)> = Vec::new();
        let mut comb = ' ';
        let spaced = part.replace('>', " > ");
        for tok in spaced.split_whitespace() {
            if tok == ">" {
                comb = '>';
                continue;
            }
            complex.push((comb, parse_compound(tok)?));
            comb = ' ';
        }
        if complex.is_empty() {
            return Err(format!("empty selector in `{s}`"));
        }
        list.push(complex);
    }
    Ok(list)
}

fn parse_compound(t: &str) -> Result<Compound, String> {
    let mut c = Compound::default();
    let b: Vec<char> = t.chars().collect();
    let mut i = 0;
    let ident = |i: &mut usize| {
        let st = *i;
        while *i < b.len() && (b[*i].is_alphanumeric() || b[*i] == '-' || b[*i] == '_') {
            *i += 1;
        }
        b[st..*i].iter().collect::<String>()
    };
    if i < b.len() && b[i] == '*' {
        i += 1;
    } else if i < b.len() && b[i].is_alphabetic() {
        c.tag = Some(ident(&mut i).to_ascii_lowercase());
    }
    while i < b.len() {
        match b[i] {
            '#' => {
                i += 1;
                c.id = Some(ident(&mut i));
            }
            '.' => {
                i += 1;
                c.classes.push(ident(&mut i));
            }
            '[' => {
                let end = b[i..].iter().position(|x| *x == ']').ok_or("unclosed [")? + i;
                let inner: String = b[i + 1..end].iter().collect();
                match inner.split_once('=') {
                    Some((k, v)) => c.attrs.push((k.trim().into(), Some(v.trim().trim_matches(|q| q == '"' || q == '\'').into()))),
                    None => c.attrs.push((inner.trim().into(), None)),
                }
                i = end + 1;
            }
            other => return Err(format!("unsupported selector syntax `{other}` in `{t}`")),
        }
    }
    Ok(c)
}

fn parse_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let inner = inner.trim();
    let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
    let tag = inner[..name_end].to_ascii_lowercase();
    let mut attrs: Vec<(String, String)> = Vec::new();
    let rest: Vec<char> = inner[name_end..].chars().collect();
    let mut i = 0;
    while i < rest.len() {
        while i < rest.len() && rest[i].is_whitespace() {
            i += 1;
        }
        let st = i;
        while i < rest.len() && !rest[i].is_whitespace() && rest[i] != '=' {
            i += 1;
        }
        if st == i {
            break;
        }
        let k: String = rest[st..i].iter().collect::<String>().to_ascii_lowercase();
        let mut v = String::new();
        if i < rest.len() && rest[i] == '=' {
            i += 1;
            if i < rest.len() && (rest[i] == '"' || rest[i] == '\'') {
                let q = rest[i];
                i += 1;
                let vs = i;
                while i < rest.len() && rest[i] != q {
                    i += 1;
                }
                v = rest[vs..i].iter().collect();
                i += 1;
            } else {
                let vs = i;
                while i < rest.len() && !rest[i].is_whitespace() {
                    i += 1;
                }
                v = rest[vs..i].iter().collect();
            }
        }
        if !attrs.iter().any(|(x, _)| *x == k) {
            attrs.push((k, decode(&v)));
        }
    }
    (tag, attrs)
}

fn decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn escape(s: &str, attr: bool) -> String {
    let s = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    if attr { s.replace('"', "&quot;") } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_and_serialise() {
        let d = MemDom::from_html(r#"<!doctype html><ul id="l"><li class="a b">x &amp; y</li><li>z<br></li></ul><p>q</p>"#);
        let ul = d.by_id("l").unwrap();
        assert_eq!(d.query(0, "ul > li.a", true).unwrap().len(), 1);
        assert_eq!(d.query(0, "li", true).unwrap().len(), 2);
        assert_eq!(d.query(ul, "p", true).unwrap().len(), 0, "scoped to the subtree");
        assert_eq!(d.text(ul), "x & yz");
        assert_eq!(
            d.serialize(),
            r#"<ul id="l"><li class="a b">x &amp; y</li><li>z<br></li></ul><p>q</p>"#
        );
    }

    #[test]
    fn selectors_do_not_see_above_scope() {
        let d = MemDom::from_html(r#"<div class="outer"><section id="s"><span>t</span></section></div>"#);
        let s = d.by_id("s").unwrap();
        assert_eq!(d.query(0, ".outer span", true).unwrap().len(), 1);
        assert_eq!(d.query(s, ".outer span", true).unwrap().len(), 0);
    }
}
