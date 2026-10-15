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
