//! Arena-allocated DOM. Nodes live in one contiguous Vec and link to each other
//! by u32 index: no per-node heap allocation, no refcounting, cache friendly.

use crate::atom::Atom;

pub type NodeId = u32;
pub const NONE: NodeId = u32::MAX;

pub struct Element {
    pub tag: Atom,
    pub id: Option<Atom>,
    pub classes: Vec<Atom>,
    pub attrs: Vec<(Atom, String)>,
}

impl Element {
    #[inline]
    pub fn attr(&self, name: Atom) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }
}

pub enum Data {
    Document,
    Element(Element),
    Text(String),
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
    pub title: String,
    /// Contents of inline <style> elements, in document order.
    pub styles: Vec<String>,
    /// hrefs of <link rel=stylesheet>, in document order.
    pub stylesheet_links: Vec<String>,
    pub base: Option<String>,
    pub html: NodeId,
    pub body: NodeId,
    /// <script> elements in document order.
    pub scripts: Vec<NodeId>,
    /// No <!doctype html>: render in quirks mode.
    pub quirks: bool,
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
            title: String::new(),
            styles: Vec::new(),
            stylesheet_links: Vec::new(),
            base: None,
            html: NONE,
            body: NONE,
            scripts: Vec::new(),
            quirks: true,
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
        match &self.nodes[id as usize].data {
            Data::Element(e) => Some(e),
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
                Data::Element(_) => self.text_content(c, out),
                Data::Document => {}
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
        let data = match &src.nodes[n as usize].data {
            Data::Element(e) => Data::Element(Element { tag: e.tag, id: e.id, classes: e.classes.clone(), attrs: e.attrs.clone() }),
            Data::Text(t) => Data::Text(t.clone()),
            Data::Document => Data::Text(String::new()),
        };
        let id = self.add(data);
        for c in src.children(n).collect::<Vec<_>>() {
            let cc = self.import(src, c);
            self.append(id, cc);
        }
        id
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
        for c in self.children(id) {
            self.outer_html(c, out);
        }
    }

    pub fn outer_html(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id as usize].data {
            Data::Text(t) => {
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
                if matches!(name, "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr") {
                    return;
                }
                self.inner_html(id, out);
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
            Data::Document => self.inner_html(id, out),
        }
    }
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
