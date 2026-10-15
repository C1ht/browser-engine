//! Box tree construction and layout.
//!
//! Supports block flow (with margin collapsing), inline formatting contexts
//! with line breaking, inline-blocks, replaced elements, list markers and an
//! automatic table layout algorithm (min/max-content column distribution).
//!
//! All positions are relative to the parent's content box, so moving a subtree
//! never requires touching its descendants.

use crate::atom::k;
use crate::dom::*;
use crate::fonts::{fonts, Fonts};
use crate::style::*;

pub const NO_LINK: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Block,
    Table,
    Row,
    Cell,
    Replaced,
}

#[derive(Clone, Copy, Debug)]
pub enum Item {
    Text { start: u32, end: u32, node: NodeId, link: u32, bg: u32 },
    Br,
    Atomic(u32),
}

#[derive(Clone, Copy, Debug)]
pub enum FragKind {
    Text { start: u32, end: u32, node: NodeId, link: u32, bg: u32, w: f32 },
    Atomic(u32),
}

/// A positioned piece of a line. For text, `y` is the baseline.
#[derive(Clone, Copy, Debug)]
pub struct Frag {
    pub x: f32,
    pub y: f32,
    pub kind: FragKind,
}

pub struct LBox {
    pub kind: Kind,
    pub node: NodeId,
    pub anon: bool,
    pub children: Vec<u32>,
    pub items: Vec<Item>,
    pub frags: Vec<Frag>,
    /// Border-box origin relative to the parent's content box.
    pub x: f32,
    pub y: f32,
    /// Content-box size.
    pub w: f32,
    pub h: f32,
    pub m: [f32; 4],
    pub b: [f32; 4],
    pub p: [f32; 4],
    pub intrinsic: Option<(f32, f32)>,
    pub marker: Option<String>,
    /// First baseline, relative to the content box top.
    pub baseline: Option<f32>,
    pub colspan: u32,
    pub cell_off: f32,
    pub img: bool,
    pub repl: (f32, f32),
    pub link: u32,
}

impl LBox {
    #[inline]
    pub fn border_w(&self) -> f32 {
        self.w + self.p[1] + self.p[3] + self.b[1] + self.b[3]
    }
    #[inline]
    pub fn border_h(&self) -> f32 {
        self.h + self.p[0] + self.p[2] + self.b[0] + self.b[2]
    }
}

pub struct LayoutTree {
    pub boxes: Vec<LBox>,
    pub text: String,
    pub links: Vec<String>,
    pub root: u32,
    pub height: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Fit,
    Forced(f32),
}

fn is_block_level(d: Display) -> bool {
    matches!(
        d,
        Display::Block
            | Display::ListItem
            | Display::Table
            | Display::TableRow
            | Display::TableCell
            | Display::TableRowGroup
            | Display::TableCaption
    )
}

fn is_replaced(tag: crate::atom::Atom) -> bool {
    matches!(tag, k::img | k::svg | k::iframe | k::video | k::canvas | k::embed | k::object)
}

#[inline]
fn collapse(a: f32, b: f32) -> f32 {
    if a >= 0.0 && b >= 0.0 {
        a.max(b)
    } else if a < 0.0 && b < 0.0 {
        a.min(b)
    } else {
        a + b
    }
}

fn roman(mut n: u32, upper: bool) -> String {
    const T: [(u32, &str); 13] = [
        (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"),
        (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i"),
    ];
    let mut s = String::new();
    for (v, r) in T {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    if upper { s.to_uppercase() } else { s }
}

fn marker_text(list: ListStyle, n: i32) -> Option<String> {
    Some(match list {
        ListStyle::None => return None,
        ListStyle::Disc => "\u{2022}".into(),
        ListStyle::Circle => "\u{25e6}".into(),
        ListStyle::Square => "\u{25aa}".into(),
        ListStyle::Decimal => format!("{n}."),
        ListStyle::LowerAlpha | ListStyle::UpperAlpha => {
            let base = if list == ListStyle::LowerAlpha { b'a' } else { b'A' };
            let c = (base + ((n.max(1) - 1) % 26) as u8) as char;
            format!("{c}.")
        }
        ListStyle::LowerRoman => format!("{}.", roman(n.max(1) as u32, false)),
        ListStyle::UpperRoman => format!("{}.", roman(n.max(1) as u32, true)),
    })
}

struct Engine<'a> {
    doc: &'a Document,
    styles: &'a [Style],
    f: &'static Fonts,
    boxes: Vec<LBox>,
    text: String,
    links: Vec<String>,
    contains_block: Vec<bool>,
    ws_space: bool,
}

pub fn layout(doc: &Document, styles: &[Style], vw: f32) -> LayoutTree {
    let mut e = Engine {
        doc,
        styles,
        f: fonts(),
        boxes: Vec::with_capacity(doc.nodes.len() / 2),
        text: String::with_capacity(doc.nodes.len() * 16),
        links: Vec::new(),
        contains_block: vec![false; doc.nodes.len()],
        ws_space: true,
    };
    e.compute_contains_block();
    let root = e.new_box(Kind::Block, doc.html, false);
    e.build_container(doc.html, root, NO_LINK);
    e.layout_block(root, vw, Mode::Normal);
    let r = &mut e.boxes[root as usize];
    r.x = r.m[3];
    r.y = r.m[0];
    let height = r.y + r.border_h() + r.m[2];
    LayoutTree { boxes: e.boxes, text: e.text, links: e.links, root, height }
}

impl<'a> Engine<'a> {
    // ------------------------------------------------------------ building

    fn compute_contains_block(&mut self) {
        let doc = self.doc;
        for id in (1..doc.nodes.len()).rev() {
            if doc.elem(id as NodeId).is_none() {
                continue;
            }
            if self.blockish(id as NodeId) {
                let p = doc.nodes[id].parent;
                if p != NONE {
                    self.contains_block[p as usize] = true;
                }
            }
        }
    }

    #[inline]
    fn blockish(&self, c: NodeId) -> bool {
        let Some(el) = self.doc.elem(c) else { return false };
        let d = self.styles[c as usize].display;
        match d {
            Display::None => false,
            Display::Inline | Display::Contents => {
                !is_replaced(el.tag) && self.contains_block[c as usize]
            }
            d => is_block_level(d),
        }
    }

    fn new_box(&mut self, kind: Kind, node: NodeId, anon: bool) -> u32 {
        let id = self.boxes.len() as u32;
        self.boxes.push(LBox {
            kind,
            node,
            anon,
            children: Vec::new(),
            items: Vec::new(),
            frags: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            m: [0.0; 4],
            b: [0.0; 4],
            p: [0.0; 4],
            intrinsic: None,
            marker: None,
            baseline: None,
            colspan: 1,
            cell_off: 0.0,
            img: false,
            repl: (0.0, 0.0),
            link: NO_LINK,
        });
        id
    }

    fn link_for(&mut self, c: NodeId, link: u32) -> u32 {
        if self.doc.tag(c) == Some(k::a) {
            if let Some(h) = self.doc.attr(c, k::href) {
                self.links.push(h.trim().to_owned());
                return (self.links.len() - 1) as u32;
            }
        }
        link
    }

    fn build_container(&mut self, node: NodeId, bx: u32, link: u32) {
        let saved = self.ws_space;
        self.ws_space = true;
        if !self.contains_block[node as usize] {
            for c in self.doc.children(node) {
                self.collect_inline(c, bx, link, 0);
            }
            if self.items_blank(bx) {
                self.boxes[bx as usize].items.clear();
            }
            self.ws_space = saved;
            return;
        }
        let mut anon: Option<u32> = None;
        let mut ordinal = self
            .doc
            .attr(node, k::start)
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(1);
        for c in self.doc.children(node) {
            if self.blockish(c) {
                if let Some(a) = anon.take() {
                    self.finish_anon(bx, a);
                }
                let st = &self.styles[c as usize];
                let n = if st.display == Display::ListItem {
                    ordinal += 1;
                    ordinal - 1
                } else {
                    0
                };
                let cb = self.build_block_level(c, link, n);
                self.boxes[bx as usize].children.push(cb);
            } else {
                if anon.is_none() {
                    if let Data::Text(t) = &self.doc.nodes[c as usize].data {
                        if !self.styles[node as usize].ws.preserves()
                            && t.bytes().all(|b| b.is_ascii_whitespace())
                        {
                            continue;
                        }
                    }
                    if self.doc.elem(c).is_some() && self.styles[c as usize].display == Display::None {
                        continue;
                    }
                    anon = Some(self.new_box(Kind::Block, node, true));
                    self.ws_space = true;
                }
                self.collect_inline(c, anon.unwrap(), link, 0);
            }
        }
        if let Some(a) = anon {
            self.finish_anon(bx, a);
        }
        self.ws_space = saved;
    }

    fn items_blank(&self, bx: u32) -> bool {
        self.boxes[bx as usize].items.iter().all(|it| match *it {
            Item::Text { start, end, node, .. } => {
                !self.styles[node as usize].ws.preserves()
                    && self.text[start as usize..end as usize].bytes().all(|b| b == b' ')
            }
            _ => false,
        })
    }

    fn finish_anon(&mut self, parent: u32, a: u32) {
        if !self.items_blank(a) {
            self.boxes[parent as usize].children.push(a);
        }
    }

    fn build_block_level(&mut self, c: NodeId, link: u32, ordinal: i32) -> u32 {
        let st = &self.styles[c as usize];
        let link = self.link_for(c, link);
        let tag = self.doc.tag(c).unwrap();
        if is_replaced(tag) {
            return self.build_replaced(c);
        }
        if st.display == Display::Table {
            return self.build_table(c, link);
        }
        let bx = self.new_box(Kind::Block, c, false);
        if st.display == Display::ListItem {
            self.boxes[bx as usize].marker = marker_text(st.list, ordinal);
        }
        self.boxes[bx as usize].link = link;
        self.build_container(c, bx, link);
        bx
    }

    fn build_replaced(&mut self, c: NodeId) -> u32 {
        let tag = self.doc.tag(c).unwrap();
        let bx = self.new_box(Kind::Replaced, c, false);
        let (w, h) = match tag {
            k::iframe | k::video | k::canvas | k::embed | k::object => (300.0, 150.0),
            _ => (0.0, 0.0),
        };
        let b = &mut self.boxes[bx as usize];
        b.repl = (w, h);
        b.img = tag == k::img;
        bx
    }

    fn build_table(&mut self, node: NodeId, link: u32) -> u32 {
        let t = self.new_box(Kind::Table, node, false);
        let mut anon_row = None;
        self.collect_rows(node, t, link, &mut anon_row);
        t
    }

    fn collect_rows(&mut self, node: NodeId, t: u32, link: u32, anon_row: &mut Option<u32>) {
        for c in self.doc.children(node) {
            if self.doc.elem(c).is_none() {
                continue;
            }
            let d = self.styles[c as usize].display;
            match d {
                Display::None => {}
                Display::TableRow => {
                    *anon_row = None;
                    let r = self.new_box(Kind::Row, c, false);
                    self.collect_cells(c, r, link);
                    self.boxes[t as usize].children.push(r);
                }
                Display::TableCaption => {
                    *anon_row = None;
                    let b = self.build_block_level(c, link, 0);
                    self.boxes[t as usize].children.push(b);
                }
                Display::TableCell => {
                    let r = match *anon_row {
                        Some(r) => r,
                        None => {
                            let r = self.new_box(Kind::Row, node, true);
                            self.boxes[t as usize].children.push(r);
                            *anon_row = Some(r);
                            r
                        }
                    };
                    let cell = self.build_cell(c, link);
                    self.boxes[r as usize].children.push(cell);
                }
                _ => self.collect_rows(c, t, link, anon_row),
            }
        }
    }

    fn collect_cells(&mut self, row: NodeId, r: u32, link: u32) {
        for c in self.doc.children(row) {
            if self.doc.elem(c).is_none() {
                continue;
            }
            match self.styles[c as usize].display {
                Display::None => {}
                Display::TableCell => {
                    let cell = self.build_cell(c, link);
                    self.boxes[r as usize].children.push(cell);
                }
                _ => self.collect_cells(c, r, link),
            }
        }
    }

    fn build_cell(&mut self, c: NodeId, link: u32) -> u32 {
        let cell = self.new_box(Kind::Cell, c, false);
        let span = self
            .doc
            .attr(c, k::colspan)
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(1)
            .clamp(1, 1000);
        self.boxes[cell as usize].colspan = span;
        self.build_container(c, cell, link);
        cell
    }

    fn collect_inline(&mut self, c: NodeId, bx: u32, link: u32, bg: u32) {
        let doc = self.doc;
        match &doc.nodes[c as usize].data {
            Data::Text(t) => {
                let parent = doc.nodes[c as usize].parent;
                self.push_text(t, parent, bx, link, bg);
            }
            Data::Element(el) => {
                let st = &self.styles[c as usize];
                if st.display == Display::None {
                    return;
                }
                if el.tag == k::br {
                    self.boxes[bx as usize].items.push(Item::Br);
                    self.ws_space = true;
                    return;
                }
                let link = self.link_for(c, link);
                if el.tag == k::img && st.width.is_auto() && st.height.is_auto() {
                    // We don't decode images; show the alt text instead.
                    if let Some(alt) = el.attr(k::alt) {
                        if !alt.trim().is_empty() {
                            let alt = alt.to_owned();
                            self.push_text(&alt, c, bx, link, bg);
                        }
                    }
                    return;
                }
                let atomic = is_replaced(el.tag)
                    || matches!(
                        st.display,
                        Display::InlineBlock | Display::InlineTable | Display::Table
                    );
                if atomic {
                    let saved = self.ws_space;
                    let id = if is_replaced(el.tag) {
                        self.build_replaced(c)
                    } else if matches!(st.display, Display::InlineTable | Display::Table) {
                        self.build_table(c, link)
                    } else {
                        let id = self.new_box(Kind::Block, c, false);
                        self.build_container(c, id, link);
                        id
                    };
                    self.boxes[id as usize].link = link;
                    self.ws_space = saved;
                    self.boxes[bx as usize].items.push(Item::Atomic(id));
                    self.ws_space = false;
                    return;
                }
                let bg = if st.bg >> 24 != 0 { st.bg } else { bg };
                for cc in doc.children(c) {
                    self.collect_inline(cc, bx, link, bg);
                }
            }
            Data::Document => {}
        }
    }

    fn push_text(&mut self, t: &str, style_node: NodeId, bx: u32, link: u32, bg: u32) {
        let st = &self.styles[style_node as usize];
        let start = self.text.len();
        let tf = st.transform;
        let mut prev_space = true;
        let push = |out: &mut String, ch: char, prev_space: &mut bool| {
            match tf {
                Transform::Upper => out.extend(ch.to_uppercase()),
                Transform::Lower => out.extend(ch.to_lowercase()),
                Transform::Capitalize if *prev_space => out.extend(ch.to_uppercase()),
                _ => out.push(ch),
            }
            *prev_space = false;
        };
        if st.ws.preserves() {
            let mut col = 0;
            for ch in t.chars() {
                match ch {
                    '\r' => {}
                    '\n' => {
                        self.text.push('\n');
                        col = 0;
                        prev_space = true;
                    }
                    '\t' => {
                        let n = 8 - col % 8;
                        for _ in 0..n {
                            self.text.push(' ');
                        }
                        col += n;
                    }
                    ' ' => {
                        self.text.push(' ');
                        col += 1;
                        prev_space = true;
                    }
                    _ => {
                        push(&mut self.text, ch, &mut prev_space);
                        col += 1;
                    }
                }
            }
            self.ws_space = false;
        } else {
            let keep_nl = st.ws == WhiteSpace::PreLine;
            for ch in t.chars() {
                if matches!(ch, ' ' | '\n' | '\t' | '\r' | '\x0c') {
                    if keep_nl && ch == '\n' {
                        if self.text.len() > start && self.text.ends_with(' ') {
                            self.text.pop();
                        }
                        self.text.push('\n');
                        self.ws_space = true;
                    } else if !self.ws_space {
                        self.text.push(' ');
                        self.ws_space = true;
                    }
                    prev_space = true;
                } else {
                    push(&mut self.text, ch, &mut prev_space);
                    self.ws_space = false;
                }
            }
        }
        if self.text.len() > start {
            self.boxes[bx as usize].items.push(Item::Text {
                start: start as u32,
                end: self.text.len() as u32,
                node: style_node,
                link,
                bg,
            });
        }
    }

    // ------------------------------------------------------------- helpers

    #[inline]
    fn st(&self, id: u32) -> &'a Style {
        let styles: &'a [Style] = self.styles;
        &styles[self.boxes[id as usize].node as usize]
    }

    fn resolve_edges(&mut self, id: u32, cb_w: f32) {
        let st = self.st(id);
        let b = &mut self.boxes[id as usize];
        if b.anon || b.kind == Kind::Row {
            b.m = [0.0; 4];
            b.b = [0.0; 4];
            b.p = [0.0; 4];
            return;
        }
        for i in 0..4 {
            b.p[i] = st.padding[i].px(cb_w).max(0.0);
            b.b[i] = st.bw[i];
            b.m[i] = if b.kind == Kind::Cell { 0.0 } else { st.margin[i].px(cb_w) };
        }
    }

    fn face_of(&self, st: &Style) -> u8 {
        self.f.pick(st.family, st.bold, st.italic)
    }

    // ----------------------------------------------------------- intrinsic

    /// (min-content, max-content) of the margin box.
    fn intrinsic_outer(&mut self, id: u32) -> (f32, f32) {
        let st = self.st(id);
        let b = &self.boxes[id as usize];
        let anon = b.anon || b.kind == Kind::Row;
        let kind = b.kind;
        let (mut pbm, mut pb) = (0.0, 0.0);
        if !anon {
            for i in [1, 3] {
                let p = match st.padding[i] {
                    Len::Px(v) => v,
                    _ => 0.0,
                } + st.bw[i];
                pb += p;
                pbm += p;
                if kind != Kind::Cell {
                    if let Len::Px(m) = st.margin[i] {
                        pbm += m;
                    }
                }
            }
        }
        let (cmin, cmax) = self.intrinsic_content(id);
        if !anon {
            if let Len::Px(w) = st.width {
                let w = if st.border_box || kind == Kind::Table { w - pb } else { w }.max(0.0);
                if kind == Kind::Cell || kind == Kind::Table {
                    let v = w.max(cmin);
                    return (cmin.max(0.0) + pbm, v + pbm);
                }
                return (w + pbm, w + pbm);
            }
        }
        let (mut mn, mut mx) = (cmin, cmax);
        if !anon {
            if let Len::Px(m) = st.max_w {
                let m = if st.border_box { m - pb } else { m };
                mx = mx.min(m);
                mn = mn.min(m);
            }
        }
        (mn + pbm, mx.max(mn) + pbm)
    }

    fn intrinsic_content(&mut self, id: u32) -> (f32, f32) {
        if let Some(v) = self.boxes[id as usize].intrinsic {
            return v;
        }
        let kind = self.boxes[id as usize].kind;
        let v = match kind {
            Kind::Replaced => {
                let st = self.st(id);
                let w = match st.width {
                    Len::Px(w) => w,
                    _ => self.boxes[id as usize].repl.0,
                };
                (w, w)
            }
            Kind::Table => {
                let st = self.st(id);
                let spacing = if st.collapse { 0.0 } else { st.spacing };
                let (mins, maxs, _) = self.table_columns(id);
                let sp = spacing * (mins.len() as f32 + 1.0);
                (mins.iter().sum::<f32>() + sp, maxs.iter().sum::<f32>() + sp)
            }
            Kind::Row => {
                let ch = self.boxes[id as usize].children.clone();
                ch.iter().fold((0.0, 0.0), |(a, b), &c| {
                    let (x, y) = self.intrinsic_outer(c);
                    (a + x, b + y)
                })
            }
            _ if !self.boxes[id as usize].items.is_empty() => self.inline_intrinsic(id),
            _ => {
                let ch = std::mem::take(&mut self.boxes[id as usize].children);
                let mut r = (0.0f32, 0.0f32);
                for &c in &ch {
                    let (a, b) = self.intrinsic_outer(c);
                    r = (r.0.max(a), r.1.max(b));
                }
                self.boxes[id as usize].children = ch;
                r
            }
        };
        self.boxes[id as usize].intrinsic = Some(v);
        v
    }

    fn inline_intrinsic(&mut self, id: u32) -> (f32, f32) {
        let items = std::mem::take(&mut self.boxes[id as usize].items);
        let (mut mn, mut mx, mut line) = (0.0f32, 0.0f32, 0.0f32);
        let mut nowrap_run = 0.0f32;
        for it in &items {
            match *it {
                Item::Br => {
                    mx = mx.max(line);
                    line = 0.0;
                    nowrap_run = 0.0;
                }
                Item::Atomic(a) => {
                    let (amn, amx) = self.intrinsic_outer(a);
                    mn = mn.max(amn);
                    line += amx;
                    nowrap_run = 0.0;
                }
                Item::Text { start, end, node, .. } => {
                    let st = &self.styles[node as usize];
                    let face = self.face_of(st);
                    let fs = st.font_size;
                    let wraps = st.ws.wraps();
                    let text = &self.text[start as usize..end as usize];
                    for (pi, seg) in text.split('\n').enumerate() {
                        if pi > 0 {
                            mx = mx.max(line);
                            line = 0.0;
                            nowrap_run = 0.0;
                        }
                        if wraps {
                            for word in seg.split(' ') {
                                let w = self.f.measure(face, fs, word);
                                mn = mn.max(w);
                                line += w + self.f.measure(face, fs, " ");
                            }
                            nowrap_run = 0.0;
                        } else {
                            let w = self.f.measure(face, fs, seg);
                            line += w;
                            nowrap_run += w;
                            mn = mn.max(nowrap_run);
                        }
                    }
                }
            }
            mx = mx.max(line);
        }
        self.boxes[id as usize].items = items;
        (mn.ceil(), mx.max(mn).ceil())
    }

    // -------------------------------------------------------------- blocks

    fn layout_block(&mut self, id: u32, cb_w: f32, mode: Mode) {
        self.resolve_edges(id, cb_w);
        let kind = self.boxes[id as usize].kind;
        if kind == Kind::Table {
            return self.layout_table(id, cb_w, mode);
        }
        let st = self.st(id);
        let anon = self.boxes[id as usize].anon;
        let (mut m, b, p) = {
            let bx = &self.boxes[id as usize];
            (bx.m, bx.b, bx.p)
        };
        let pb_h = p[1] + p[3] + b[1] + b[3];
        let pb_v = p[0] + p[2] + b[0] + b[2];
        let adj_h = if st.border_box { pb_h } else { 0.0 };
        let adj_v = if st.border_box { pb_v } else { 0.0 };

        if kind == Kind::Replaced {
            let repl = self.boxes[id as usize].repl;
            let w = match st.width {
                Len::Auto => repl.0,
                l => l.px(cb_w) - adj_h,
            };
            let h = match st.height {
                Len::Px(h) => h - adj_v,
                _ => {
                    if st.width.is_auto() { repl.1 } else { w * if repl.0 > 0.0 { repl.1 / repl.0 } else { 0.0 } }
                }
            };
            let bx = &mut self.boxes[id as usize];
            bx.w = w.max(0.0);
            bx.h = h.max(0.0);
            return;
        }

        let specified = if anon || st.width.is_auto() { None } else { Some(st.width.px(cb_w) - adj_h) };
        let mut w = match mode {
            Mode::Forced(fw) => fw - pb_h,
            _ => match specified {
                Some(w) => w,
                None if mode == Mode::Fit => {
                    let (mn, mx) = self.intrinsic_content(id);
                    let avail = cb_w - m[1] - m[3] - pb_h;
                    mx.min(avail).max(mn)
                }
                None => cb_w - m[1] - m[3] - pb_h,
            },
        };
        if !anon && !matches!(mode, Mode::Forced(_)) {
            if !st.max_w.is_auto() {
                w = w.min(st.max_w.px(cb_w) - adj_h);
            }
            w = w.max(st.min_w.px(cb_w) - adj_h);
        }
        w = w.max(0.0);
        if mode == Mode::Normal && !anon {
            let free = cb_w - w - pb_h - m[1] - m[3];
            if free > 0.0 {
                let (l, r) = (st.margin[3].is_auto(), st.margin[1].is_auto());
                if l && r {
                    m[3] += free / 2.0;
                    m[1] += free / 2.0;
                } else if l {
                    m[3] += free;
                }
            }
        }
        {
            let bx = &mut self.boxes[id as usize];
            bx.w = w;
            bx.m = m;
        }

        let bfc_root = kind == Kind::Cell || mode != Mode::Normal || id == 0;
        let content_h = if !self.boxes[id as usize].items.is_empty() {
            self.layout_inline(id, w)
        } else {
            self.layout_children(id, w, bfc_root)
        };

        let mut h = match st.height {
            Len::Px(h) if !anon => (h - adj_v).max(0.0),
            _ => content_h,
        };
        if !anon {
            if let Len::Px(mh) = st.max_h {
                h = h.min(mh - adj_v);
            }
            if let Len::Px(mh) = st.min_h {
                h = h.max(mh - adj_v);
            }
        }
        self.boxes[id as usize].h = h.max(0.0);
    }

    fn layout_children(&mut self, id: u32, w: f32, bfc_root: bool) -> f32 {
        let children = std::mem::take(&mut self.boxes[id as usize].children);
        let (b, p) = (self.boxes[id as usize].b, self.boxes[id as usize].p);
        let st = self.st(id);
        let anon = self.boxes[id as usize].anon;
        let hoist_top = !bfc_root && !anon && b[0] == 0.0 && p[0] == 0.0;
        let hoist_bottom = !bfc_root && !anon && b[2] == 0.0 && p[2] == 0.0 && st.height.is_auto();

        let mut cursor = 0.0f32;
        let mut pending = 0.0f32;
        let mut first = true;
        let mut baseline = None;
        let center = st.center_blocks;
        for &c in &children {
            self.layout_block(c, w, Mode::Normal);
            if center && !self.boxes[c as usize].anon {
                // -webkit-center: centre narrower child blocks without auto margins.
                let cs = self.st(c);
                let cb = &mut self.boxes[c as usize];
                let free = w - (cb.m[3] + cb.border_w() + cb.m[1]);
                if free > 0.5 && !cs.margin[1].is_auto() && !cs.margin[3].is_auto() {
                    cb.m[3] += free / 2.0;
                    cb.m[1] += free / 2.0;
                }
            }
            let cb = &self.boxes[c as usize];
            let (mt, mb, bh) = (cb.m[0], cb.m[2], cb.border_h());
            let collapsed = if first && hoist_top {
                let pm = self.boxes[id as usize].m[0];
                self.boxes[id as usize].m[0] = collapse(pm, mt);
                0.0
            } else {
                collapse(pending, mt)
            };
            let cb = &mut self.boxes[c as usize];
            cb.x = cb.m[3];
            if bh == 0.0 && cb.children.is_empty() && cb.items.is_empty() && cb.kind == Kind::Block {
                // Empty block: its margins collapse through it.
                cb.y = cursor + collapsed;
                pending = if first && hoist_top { mb } else { collapse(collapsed, mb) };
                first = false;
                continue;
            }
            first = false;
            cb.y = cursor + collapsed;
            if baseline.is_none() {
                if let Some(bl) = cb.baseline {
                    baseline = Some(cb.y + cb.b[0] + cb.p[0] + bl);
                }
            }
            cursor = cb.y + bh;
            pending = mb;
        }
        self.boxes[id as usize].children = children;
        self.boxes[id as usize].baseline = baseline;
        if hoist_bottom && !first {
            let pm = self.boxes[id as usize].m[2];
            self.boxes[id as usize].m[2] = collapse(pm, pending);
            cursor
        } else {
            cursor + pending.max(0.0)
        }
    }

    // -------------------------------------------------------------- inline

    fn layout_inline(&mut self, id: u32, w: f32) -> f32 {
        let items = std::mem::take(&mut self.boxes[id as usize].items);
        let cst = self.st(id);
        let cface = self.face_of(cst);
        let (ca, cd, cl) = self.f.metrics(cface);
        let lh = cst.line_height_px(cl);
        let half = (lh - (ca + cd) * cst.font_size) / 2.0;
        let strut = (half + ca * cst.font_size, lh - (half + ca * cst.font_size));
        let mut lb = LineBuilder {
            frags: Vec::new(),
            atomics: Vec::new(),
            line_start: 0,
            x: 0.0,
            y: 0.0,
            asc: strut.0,
            desc: strut.1,
            strut,
            has_content: false,
            trailing_w: 0.0,
            trailing_bytes: 0,
            width: w,
            align: cst.align,
            baseline: None,
        };
        for it in &items {
            match *it {
                Item::Br => lb.finish(true),
                Item::Atomic(a) => {
                    self.layout_block(a, w, Mode::Fit);
                    let ab = &self.boxes[a as usize];
                    let mw = ab.m[3] + ab.border_w() + ab.m[1];
                    let mh = ab.m[0] + ab.border_h() + ab.m[2];
                    let asc = match ab.baseline {
                        Some(bl) if ab.kind == Kind::Block => ab.m[0] + ab.b[0] + ab.p[0] + bl,
                        _ => mh,
                    };
                    if lb.has_content && lb.x + mw > w + 0.01 {
                        lb.finish(false);
                    }
                    lb.frags.push(Frag { x: lb.x + ab.m[3], y: asc - ab.m[0], kind: FragKind::Atomic(a) });
                    lb.x += mw;
                    lb.asc = lb.asc.max(asc);
                    lb.desc = lb.desc.max(mh - asc);
                    lb.has_content = true;
                    lb.trailing_w = 0.0;
                    lb.trailing_bytes = 0;
                }
                Item::Text { start, end, node, link, bg } => {
                    let st = &self.styles[node as usize];
                    let face = self.face_of(st);
                    let fs = st.font_size;
                    let (a, d, l) = self.f.metrics(face);
                    let ilh = st.line_height_px(l);
                    let half = (ilh - (a + d) * fs) / 2.0;
                    let asc = half + a * fs;
                    let desc = ilh - asc;
                    let wraps = st.ws.wraps();
                    let collapsible = !st.ws.preserves();
                    let space_w = self.f.adv(face, ' ').adv * fs;
                    let text = &self.text[start as usize..end as usize];
                    let bytes = text.as_bytes();
                    let n = bytes.len();
                    let mut i = 0;
                    while i < n {
                        if bytes[i] == b'\n' {
                            lb.asc = lb.asc.max(asc);
                            lb.desc = lb.desc.max(desc);
                            lb.finish(true);
                            i += 1;
                            continue;
                        }
                        let ws = i;
                        while i < n && bytes[i] != b' ' && bytes[i] != b'\n' {
                            i += 1;
                        }
                        let we = i;
                        while i < n && bytes[i] == b' ' {
                            i += 1;
                        }
                        let se = i;
                        if ws == we && !lb.has_content && collapsible {
                            continue;
                        }
                        let word_w = self.f.measure(face, fs, &text[ws..we]);
                        let sp_w = (se - we) as f32 * space_w;
                        if wraps && lb.has_content && we > ws && lb.x + word_w > w + 0.01 {
                            lb.finish(false);
                        }
                        lb.add_text(start + ws as u32, start + se as u32, word_w, sp_w, (se - we) as u32, node, link, bg);
                        lb.asc = lb.asc.max(asc);
                        lb.desc = lb.desc.max(desc);
                    }
                }
            }
        }
        lb.finish(false);
        for &(a, x, y) in &lb.atomics {
            let b = &mut self.boxes[a as usize];
            b.x = x;
            b.y = y;
        }
        let bx = &mut self.boxes[id as usize];
        bx.items = items;
        bx.frags = lb.frags;
        bx.baseline = lb.baseline;
        lb.y
    }

    // --------------------------------------------------------------- tables

    /// Per-column (min, max, percent) widths.
    fn table_columns(&mut self, id: u32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let st = self.st(id);
        let spacing = if st.collapse { 0.0 } else { st.spacing };
        let rows = self.boxes[id as usize].children.clone();
        let mut mins: Vec<f32> = Vec::new();
        let mut maxs: Vec<f32> = Vec::new();
        let mut pcts: Vec<f32> = Vec::new();
        let mut spans = Vec::new();
        for &r in &rows {
            if self.boxes[r as usize].kind != Kind::Row {
                continue;
            }
            let cells = self.boxes[r as usize].children.clone();
            let mut col = 0usize;
            for &c in &cells {
                let span = self.boxes[c as usize].colspan as usize;
                if mins.len() < col + span {
                    mins.resize(col + span, 0.0);
                    maxs.resize(col + span, 0.0);
                    pcts.resize(col + span, 0.0);
                }
                let (mn, mx) = self.intrinsic_outer(c);
                if let Len::Pct(p) = self.st(c).width {
                    if span == 1 {
                        pcts[col] = pcts[col].max(p);
                    }
                }
                if span == 1 {
                    mins[col] = mins[col].max(mn);
                    maxs[col] = maxs[col].max(mx);
                } else {
                    spans.push((col, span, mn, mx));
                }
                col += span;
            }
        }
        for (col, span, mn, mx) in spans {
            let gap = spacing * (span as f32 - 1.0);
            let cur_min: f32 = mins[col..col + span].iter().sum::<f32>() + gap;
            if mn > cur_min {
                let add = (mn - cur_min) / span as f32;
                mins[col..col + span].iter_mut().for_each(|v| *v += add);
            }
            let cur_max: f32 = maxs[col..col + span].iter().sum::<f32>() + gap;
            if mx > cur_max {
                let add = (mx - cur_max) / span as f32;
                maxs[col..col + span].iter_mut().for_each(|v| *v += add);
            }
        }
        for i in 0..maxs.len() {
            maxs[i] = maxs[i].max(mins[i]);
        }
        (mins, maxs, pcts)
    }

    fn layout_table(&mut self, id: u32, cb_w: f32, mode: Mode) {
        let st = self.st(id);
        let (mut m, b, p) = {
            let bx = &self.boxes[id as usize];
            (bx.m, bx.b, bx.p)
        };
        let pb_h = p[1] + p[3] + b[1] + b[3];
        let spacing = if st.collapse { 0.0 } else { st.spacing };
        let (mins, maxs, pcts) = self.table_columns(id);
        let n = mins.len();
        let sp_total = spacing * (n as f32 + 1.0);
        let sum_min: f32 = mins.iter().sum();
        let sum_max: f32 = maxs.iter().sum();
        let avail = match mode {
            Mode::Forced(fw) => fw - pb_h,
            _ => cb_w - m[1] - m[3] - pb_h,
        };
        let spec = match st.width {
            Len::Auto => None,
            l => Some(l.px(cb_w) - pb_h),
        };
        let any_pct = pcts.iter().any(|&p| p > 0.0);
        let total_w = match spec {
            Some(s) => s.max(sum_min + sp_total),
            None if any_pct && mode == Mode::Normal => avail.max(sum_min + sp_total),
            None => (sum_max + sp_total).min(avail).max(sum_min + sp_total),
        };
        let widths = distribute(&mins, &maxs, &pcts, total_w - sp_total);

        let children = std::mem::take(&mut self.boxes[id as usize].children);
        let mut y = if n > 0 { spacing } else { 0.0 };
        for &r in &children {
            if self.boxes[r as usize].kind != Kind::Row {
                // caption
                self.layout_block(r, total_w, Mode::Normal);
                let cb = &mut self.boxes[r as usize];
                cb.x = cb.m[3];
                cb.y = y + cb.m[0];
                y += cb.m[0] + cb.border_h() + cb.m[2];
                continue;
            }
            self.resolve_edges(r, total_w);
            let cells = std::mem::take(&mut self.boxes[r as usize].children);
            let mut x = spacing;
            let mut col = 0usize;
            let mut row_h = 0.0f32;
            for &c in &cells {
                let span = (self.boxes[c as usize].colspan as usize).min(n.saturating_sub(col)).max(1);
                let end = (col + span).min(n);
                let cw = widths[col.min(n)..end].iter().sum::<f32>() + spacing * (span as f32 - 1.0);
                self.layout_block(c, cw, Mode::Forced(cw));
                let cb = &mut self.boxes[c as usize];
                cb.x = x;
                cb.y = 0.0;
                x += cw + spacing;
                col += span;
                row_h = row_h.max(cb.border_h());
            }
            let rst = self.st(r);
            if !self.boxes[r as usize].anon {
                if let Len::Px(h) = rst.height {
                    row_h = row_h.max(h);
                }
            }
            for &c in &cells {
                let valign = self.st(c).valign;
                let cb = &mut self.boxes[c as usize];
                let extra = row_h - cb.border_h();
                cb.cell_off = match valign {
                    VAlign::Middle => extra / 2.0,
                    VAlign::Bottom => extra,
                    _ => 0.0,
                };
                cb.h += extra;
            }
            let rb = &mut self.boxes[r as usize];
            rb.children = cells;
            rb.x = 0.0;
            rb.y = y;
            rb.w = total_w;
            rb.h = row_h;
            y += row_h + spacing;
        }
        self.boxes[id as usize].children = children;

        if mode == Mode::Normal {
            let free = cb_w - total_w - pb_h - m[1] - m[3];
            if free > 0.0 {
                let (l, r) = (st.margin[3].is_auto(), st.margin[1].is_auto());
                if l && r {
                    m[3] += free / 2.0;
                    m[1] += free / 2.0;
                } else if l {
                    m[3] += free;
                }
            }
        }
        let h = match st.height {
            Len::Px(h) => y.max(h - p[0] - p[2] - b[0] - b[2]),
            _ => y,
        };
        let bx = &mut self.boxes[id as usize];
        bx.m = m;
        bx.w = total_w;
        bx.h = h;
    }
}

fn distribute(mins: &[f32], maxs: &[f32], pcts: &[f32], target: f32) -> Vec<f32> {
    let n = mins.len();
    let mut w = vec![0.0f32; n];
    let mut remaining = target;
    for i in 0..n {
        if pcts[i] > 0.0 {
            w[i] = (pcts[i] / 100.0 * target).max(mins[i]);
            remaining -= w[i];
        }
    }
    let others: Vec<usize> = (0..n).filter(|&i| pcts[i] <= 0.0).collect();
    if others.is_empty() {
        return w;
    }
    let omin: f32 = others.iter().map(|&i| mins[i]).sum();
    let omax: f32 = others.iter().map(|&i| maxs[i]).sum();
    if remaining >= omax {
        let extra = remaining - omax;
        for &i in &others {
            w[i] = maxs[i] + if omax > 0.0 { extra * maxs[i] / omax } else { extra / others.len() as f32 };
        }
    } else if remaining <= omin {
        for &i in &others {
            w[i] = mins[i];
        }
    } else {
        let t = (remaining - omin) / (omax - omin);
        for &i in &others {
            w[i] = mins[i] + (maxs[i] - mins[i]) * t;
        }
    }
    w
}

struct LineBuilder {
    frags: Vec<Frag>,
    atomics: Vec<(u32, f32, f32)>,
    line_start: usize,
    x: f32,
    y: f32,
    asc: f32,
    desc: f32,
    strut: (f32, f32),
    has_content: bool,
    trailing_w: f32,
    trailing_bytes: u32,
    width: f32,
    align: Align,
    baseline: Option<f32>,
}

impl LineBuilder {
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn add_text(&mut self, s: u32, e: u32, word_w: f32, sp_w: f32, sp_bytes: u32, node: NodeId, link: u32, bg: u32) {
        let in_line = self.frags.len() > self.line_start;
        let extend = match self.frags.last_mut() {
            Some(Frag { kind: FragKind::Text { end, node: n, link: l, bg: b, w, .. }, .. })
                if in_line && *end == s && *n == node && *l == link && *b == bg =>
            {
                *end = e;
                *w += word_w + sp_w;
                true
            }
            _ => false,
        };
        if !extend {
            self.frags.push(Frag {
                x: self.x,
                y: 0.0,
                kind: FragKind::Text { start: s, end: e, node, link, bg, w: word_w + sp_w },
            });
        }
        self.x += word_w + sp_w;
        if word_w > 0.0 || e - s > sp_bytes {
            self.trailing_w = sp_w;
            self.trailing_bytes = sp_bytes;
        } else {
            self.trailing_w += sp_w;
            self.trailing_bytes += sp_bytes;
        }
        self.has_content = true;
    }

    fn finish(&mut self, forced: bool) {
        if !self.has_content && !forced {
            self.asc = self.strut.0;
            self.desc = self.strut.1;
            return;
        }
        if let Some(Frag { kind: FragKind::Text { end, w, start, .. }, .. }) = self.frags[self.line_start..].last_mut() {
            let tb = self.trailing_bytes.min(*end - *start);
            *end -= tb;
            *w -= self.trailing_w;
        }
        let line_w = self.x - self.trailing_w;
        let off = match self.align {
            Align::Left => 0.0,
            Align::Center => ((self.width - line_w) / 2.0).max(0.0),
            Align::Right => (self.width - line_w).max(0.0),
        };
        let baseline = self.asc;
        for f in &mut self.frags[self.line_start..] {
            f.x += off;
            match f.kind {
                FragKind::Text { .. } => f.y = self.y + baseline,
                FragKind::Atomic(a) => {
                    // f.y holds the distance from the border-box top to the baseline.
                    let top = self.y + baseline - f.y;
                    f.y = top;
                    self.atomics.push((a, f.x, top));
                }
            }
        }
        if self.baseline.is_none() {
            self.baseline = Some(self.y + baseline);
        }
        self.y += self.asc + self.desc;
        self.x = 0.0;
        self.line_start = self.frags.len();
        self.asc = self.strut.0;
        self.desc = self.strut.1;
        self.has_content = false;
        self.trailing_w = 0.0;
        self.trailing_bytes = 0;
    }
}
