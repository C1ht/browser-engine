//! Arena-allocated DOM. Nodes live in one contiguous Vec and link to each other
//! by u32 index: no per-node heap allocation, no refcounting, cache friendly.

use crate::atom::{k, Atom};
use crate::fx::FxHashMap;

pub type NodeId = u32;
pub const NONE: NodeId = u32::MAX;

/// Element namespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Ns {
    #[default]
    Html,
    Svg,
    MathMl,
}

pub struct Element {
    pub tag: Atom,
    pub ns: Ns,
    pub id: Option<Atom>,
    pub classes: Vec<Atom>,
    pub attrs: Vec<(Atom, String)>,
}

impl Element {
    /// A new element; `id` and `classes` are derived from `attrs`.
    pub fn new(tag: Atom, ns: Ns, attrs: Vec<(Atom, String)>) -> Element {
        let mut el = Element { tag, ns, id: None, classes: Vec::new(), attrs };
        if let Some(id) = el.attr(k::id) {
            let id = id.trim();
            if !id.is_empty() {
                el.id = Some(crate::atom::intern(id));
            }
        }
        if let Some(c) = el.attr(k::class) {
            let mut classes = Vec::new();
            for c in c.split_ascii_whitespace() {
                let a = crate::atom::intern(c);
                if !classes.contains(&a) {
                    classes.push(a);
                }
            }
            el.classes = classes;
        }
        el
    }

    pub fn clone_shallow(&self) -> Element {
        Element { tag: self.tag, ns: self.ns, id: self.id, classes: self.classes.clone(), attrs: self.attrs.clone() }
    }

    #[inline]
    pub fn attr(&self, name: Atom) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }
}

pub enum Data {
    Document,
    Element(Element),
    Text(String),
    Comment(String),
    /// `<!DOCTYPE name>`.
    Doctype(Box<DocType>),
    /// A DocumentFragment (also `<template>` contents and shadow roots).
    Fragment,
}

impl Data {
    /// A copy of this node's own data (no children).
    pub fn clone_shallow(&self) -> Data {
        match self {
            Data::Document => Data::Fragment,
            Data::Element(e) => Data::Element(e.clone_shallow()),
            Data::Text(t) => Data::Text(t.clone()),
            Data::Comment(t) => Data::Comment(t.clone()),
            Data::Doctype(t) => Data::Doctype(t.clone()),
            Data::Fragment => Data::Fragment,
        }
    }
}

/// A CSSOM stylesheet: of a `<style>` / `<link>` element, or constructed
/// (`new CSSStyleSheet()`, index into `Document::free_sheets`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SheetRef {
    Node(NodeId),
    Free(usize),
}

/// CSSOM rules of an element's sheet. `base` is the element text they were
/// derived from; when the text changes, the rules are re-derived.
pub struct SheetData {
    pub base: String,
    pub rules: Vec<String>,
    pub init: bool,
}

/// A doctype node's name and identifiers.
#[derive(Clone)]
pub struct DocType {
    pub name: String,
    pub public: Option<String>,
    pub system: Option<String>,
}

pub struct Node {
    pub parent: NodeId,
    pub first_child: NodeId,
    pub last_child: NodeId,
    pub next: NodeId,
    pub prev: NodeId,
    pub data: Data,
}

pub struct Document {
    pub nodes: Vec<Node>,
    pub html: NodeId,
    pub head: NodeId,
    pub body: NodeId,
    /// `<template>` element -> its content fragment.
    pub templates: FxHashMap<NodeId, NodeId>,
    /// CSSOM-modified sheets of `<style>` / `<link>` elements.
    pub cssom: FxHashMap<NodeId, SheetData>,
    /// Constructed stylesheets' rules.
    pub free_sheets: Vec<Vec<String>>,
    /// `document.adoptedStyleSheets` (indices into `free_sheets`).
    pub adopted: Vec<usize>,
    /// <script> elements in document order.
    pub scripts: Vec<NodeId>,
    /// No <!doctype html>: render in quirks mode.
    pub quirks: bool,
    /// The focused element (for `:focus`), or NONE.
    pub focus: NodeId,
}

impl Document {
    pub fn new() -> Self {
        Document {
            nodes: vec![Node {
                parent: NONE,
                first_child: NONE,
                last_child: NONE,
                next: NONE,
                prev: NONE,
                data: Data::Document,
            }],
            html: NONE,
            head: NONE,
            body: NONE,
            templates: FxHashMap::default(),
            cssom: FxHashMap::default(),
            free_sheets: Vec::new(),
            adopted: Vec::new(),
            scripts: Vec::new(),
            quirks: true,
            focus: NONE,
        }
    }

    pub fn add(&mut self, data: Data) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            last_child: NONE,
            next: NONE,
            prev: NONE,
            data,
        });
        id
    }

    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        let last = self.nodes[parent as usize].last_child;
        {
            let c = &mut self.nodes[child as usize];
            c.parent = parent;
            c.prev = last;
            c.next = NONE;
        }
        if last == NONE {
            self.nodes[parent as usize].first_child = child;
        } else {
            self.nodes[last as usize].next = child;
        }
        self.nodes[parent as usize].last_child = child;
    }

    /// Append text, merging with a preceding text node.
    pub fn append_text(&mut self, parent: NodeId, s: &str) {
        let last = self.nodes[parent as usize].last_child;
        if last != NONE {
            if let Data::Text(t) = &mut self.nodes[last as usize].data {
                t.push_str(s);
                return;
            }
        }
        let id = self.add(Data::Text(s.to_owned()));
        self.append(parent, id);
    }

    #[inline]
    pub fn elem(&self, id: NodeId) -> Option<&Element> {
        match self.nodes.get(id as usize).map(|n| &n.data) {
            Some(Data::Element(e)) => Some(e),
            _ => None,
        }
    }

    #[inline]
    pub fn tag(&self, id: NodeId) -> Option<Atom> {
        self.elem(id).map(|e| e.tag)
    }

    #[inline]
    pub fn attr(&self, id: NodeId, name: Atom) -> Option<&str> {
        self.elem(id).and_then(|e| e.attr(name))
    }

    #[inline]
    pub fn parent(&self, id: NodeId) -> NodeId {
        self.nodes[id as usize].parent
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children { doc: self, cur: self.nodes[id as usize].first_child }
    }

    /// Previous sibling that is an element.
    pub fn prev_element(&self, id: NodeId) -> NodeId {
        let mut p = self.nodes[id as usize].prev;
        while p != NONE {
            if self.elem(p).is_some() {
                return p;
            }
            p = self.nodes[p as usize].prev;
        }
        NONE
    }

    pub fn next_element(&self, id: NodeId) -> NodeId {
        let mut p = self.nodes[id as usize].next;
        while p != NONE {
            if self.elem(p).is_some() {
                return p;
            }
            p = self.nodes[p as usize].next;
        }
        NONE
    }

    /// Concatenated descendant text.
    pub fn text_content(&self, id: NodeId, out: &mut String) {
        for c in self.children(id) {
            match &self.nodes[c as usize].data {
                Data::Text(t) => out.push_str(t),
                Data::Element(_) | Data::Fragment => self.text_content(c, out),
                _ => {}
            }
        }
    }
}

impl Default for Document {
    fn default() -> Self {
        Document::new()
    }
}

impl Document {
    /// Remove a node from its parent (it stays in the arena, detached).
    pub fn detach(&mut self, id: NodeId) {
        let (parent, prev, next) = {
            let n = &self.nodes[id as usize];
            (n.parent, n.prev, n.next)
        };
        if parent == NONE {
            return;
        }
        if prev != NONE {
            self.nodes[prev as usize].next = next;
        } else {
            self.nodes[parent as usize].first_child = next;
        }
        if next != NONE {
            self.nodes[next as usize].prev = prev;
        } else {
            self.nodes[parent as usize].last_child = prev;
        }
        let n = &mut self.nodes[id as usize];
        n.parent = NONE;
        n.prev = NONE;
        n.next = NONE;
    }

    /// Is `a` an ancestor of (or equal to) `b`?
    pub fn contains(&self, a: NodeId, b: NodeId) -> bool {
        let mut cur = b;
        while cur != NONE {
            if cur == a {
                return true;
            }
            cur = self.nodes[cur as usize].parent;
        }
        false
    }

    /// Insert `child` into `parent` before `before` (NONE = append).
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, before: NodeId) {
        if self.contains(child, parent) {
            return; // would create a cycle
        }
        self.detach(child);
        if before == NONE || self.nodes[before as usize].parent != parent {
            self.append(parent, child);
            return;
        }
        let prev = self.nodes[before as usize].prev;
        {
            let c = &mut self.nodes[child as usize];
            c.parent = parent;
            c.prev = prev;
            c.next = before;
        }
        self.nodes[before as usize].prev = child;
        if prev == NONE {
            self.nodes[parent as usize].first_child = child;
        } else {
            self.nodes[prev as usize].next = child;
        }
    }

    pub fn remove_children(&mut self, id: NodeId) {
        while self.nodes[id as usize].first_child != NONE {
            let c = self.nodes[id as usize].first_child;
            self.detach(c);
        }
    }

    pub fn set_attr(&mut self, id: NodeId, name: Atom, value: &str) {
        if let Data::Element(e) = &mut self.nodes[id as usize].data {
            match e.attrs.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = value.to_owned(),
                None => e.attrs.push((name, value.to_owned())),
            }
            if name == crate::atom::k::id {
                e.id = if value.trim().is_empty() { None } else { Some(crate::atom::intern(value.trim())) };
            } else if name == crate::atom::k::class {
                let mut classes = Vec::new();
                for c in value.split_ascii_whitespace() {
                    let a = crate::atom::intern(c);
                    if !classes.contains(&a) {
                        classes.push(a);
                    }
                }
                e.classes = classes;
            }
        }
    }

    pub fn remove_attr(&mut self, id: NodeId, name: Atom) {
        if let Data::Element(e) = &mut self.nodes[id as usize].data {
            e.attrs.retain(|(n, _)| *n != name);
            if name == crate::atom::k::id {
                e.id = None;
            } else if name == crate::atom::k::class {
                e.classes.clear();
            }
        }
    }

    /// Copy `src`'s subtree rooted at `n` into this document; returns the new id.
    pub fn import(&mut self, src: &Document, n: NodeId) -> NodeId {
        let id = self.add(src.nodes[n as usize].data.clone_shallow());
        for c in src.children(n).collect::<Vec<_>>() {
            let cc = self.import(src, c);
            self.append(id, cc);
        }
        if let Some(&content) = src.templates.get(&n) {
            let cc = self.import(src, content);
            self.templates.insert(id, cc);
        }
        id
    }

    /// Deep- or shallow-copy a node within this document.
    pub fn clone_node(&mut self, n: NodeId, deep: bool) -> NodeId {
        let id = self.add(self.nodes[n as usize].data.clone_shallow());
        if deep {
            for c in self.children(n).collect::<Vec<_>>() {
                let cc = self.clone_node(c, true);
                self.append(id, cc);
            }
        }
        if let Some(&content) = self.templates.get(&n) {
            let cc = if deep { self.clone_node(content, true) } else { self.add(Data::Fragment) };
            self.templates.insert(id, cc);
        }
        id
    }

    /// The content fragment of a `<template>` (created on demand).
    pub fn template_content(&mut self, t: NodeId) -> NodeId {
        if let Some(&c) = self.templates.get(&t) {
            return c;
        }
        let c = self.add(Data::Fragment);
        self.templates.insert(t, c);
        c
    }

    /// Is the node in the document tree (reachable from the root)?
    pub fn connected(&self, mut n: NodeId) -> bool {
        while n != NONE {
            if n == 0 {
                return true;
            }
            n = self.nodes[n as usize].parent;
        }
        false
    }

    /// First element in tree order matching `f`.
    pub fn find(&self, f: impl Fn(&Element) -> bool) -> NodeId {
        let mut stack = vec![0 as NodeId];
        while let Some(n) = stack.pop() {
            if let Data::Element(e) = &self.nodes[n as usize].data {
                if f(e) {
                    return n;
                }
            }
            let mut c = self.nodes[n as usize].last_child;
            while c != NONE {
                stack.push(c);
                c = self.nodes[c as usize].prev;
            }
        }
        NONE
    }

    /// `document.title`: the first HTML `<title>`'s text, whitespace-collapsed.
    pub fn title(&self) -> String {
        let t = self.find(|e| e.tag == k::title && e.ns == Ns::Html);
        if t == NONE {
            return String::new();
        }
        let mut s = String::new();
        self.text_content(t, &mut s);
        s.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
    }

    pub fn set_title(&mut self, s: &str) {
        let mut t = self.find(|e| e.tag == k::title && e.ns == Ns::Html);
        if t == NONE {
            if self.head == NONE {
                return;
            }
            t = self.add(Data::Element(Element::new(k::title, Ns::Html, Vec::new())));
            self.append(self.head, t);
        }
        self.remove_children(t);
        self.append_text(t, s);
    }

    /// `href` of the first `<base>` element.
    pub fn base_href(&self) -> Option<String> {
        let b = self.find(|e| e.tag == k::base && e.attr(k::href).is_some());
        self.attr(b, k::href).map(|h| h.trim().to_owned())
    }

    /// Nodes reachable from the root, in tree order.
    pub fn preorder(&self) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![0 as NodeId];
        while let Some(n) = stack.pop() {
            out.push(n);
            let mut c = self.nodes[n as usize].last_child;
            while c != NONE {
                stack.push(c);
                c = self.nodes[c as usize].prev;
            }
        }
        out
    }

    /// Serialize a node's children as HTML.
    pub fn inner_html(&self, id: NodeId, out: &mut String) {
        let id = self.templates.get(&id).copied().unwrap_or(id);
        for c in self.children(id) {
            self.outer_html(c, out);
        }
    }

    pub fn outer_html(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id as usize].data {
            Data::Text(t) => {
                let p = self.nodes[id as usize].parent;
                if p != NONE && self.elem(p).is_some_and(|e| e.ns == Ns::Html && matches!(crate::atom::name(e.tag), "style" | "script" | "xmp" | "iframe" | "noembed" | "noframes" | "plaintext" | "noscript")) {
                    out.push_str(t);
                    return;
                }
                for ch in t.chars() {
                    match ch {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        '\u{a0}' => out.push_str("&nbsp;"),
                        c => out.push(c),
                    }
                }
            }
            Data::Element(e) => {
                let name = crate::atom::name(e.tag);
                out.push('<');
                out.push_str(name);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(crate::atom::name(*k));
                    out.push_str("=\"");
                    out.push_str(&v.replace('&', "&amp;").replace('"', "&quot;"));
                    out.push('"');
                }
                out.push('>');
                if e.ns == Ns::Html && matches!(name, "area" | "base" | "basefont" | "bgsound" | "br" | "col" | "embed" | "frame" | "hr" | "img" | "input" | "keygen" | "link" | "meta" | "param" | "source" | "track" | "wbr") {
                    return;
                }
                self.inner_html(id, out);
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
            Data::Comment(t) => {
                out.push_str("<!--");
                out.push_str(t);
                out.push_str("-->");
            }
            Data::Doctype(d) => {
                out.push_str("<!DOCTYPE ");
                out.push_str(&d.name);
                out.push('>');
            }
            Data::Document | Data::Fragment => self.inner_html(id, out),
        }
    }
}

/// The mixed-case spelling of an SVG element name (the DOM stores names lowercased).
pub fn svg_tag_case(name: &str) -> &str {
    const NAMES: &[&str] = &[
        "altGlyph", "altGlyphDef", "altGlyphItem", "animateColor", "animateMotion", "animateTransform", "clipPath",
        "feBlend", "feColorMatrix", "feComponentTransfer", "feComposite", "feConvolveMatrix", "feDiffuseLighting",
        "feDisplacementMap", "feDistantLight", "feDropShadow", "feFlood", "feFuncA", "feFuncB", "feFuncG", "feFuncR",
        "feGaussianBlur", "feImage", "feMerge", "feMergeNode", "feMorphology", "feOffset", "fePointLight",
        "feSpecularLighting", "feSpotLight", "feTile", "feTurbulence", "foreignObject", "glyphRef", "linearGradient",
        "radialGradient", "textPath",
    ];
    NAMES.iter().find(|n| n.eq_ignore_ascii_case(name)).copied().unwrap_or(name)
}

pub struct Children<'a> {
    doc: &'a Document,
    cur: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;
    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        if self.cur == NONE {
            return None;
        }
        let c = self.cur;
        self.cur = self.doc.nodes[c as usize].next;
        Some(c)
    }
}
