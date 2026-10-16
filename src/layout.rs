//! Box tree construction and layout.
//!
//! Supports block flow (with margin collapsing), floats, inline formatting
//! contexts with line breaking around floats, inline-blocks, replaced
//! elements, list markers, relative / absolute / fixed positioning, flexbox,
//! grid and an automatic table layout algorithm.
//!
//! All positions are relative to the parent's content box, so moving a subtree
//! never requires touching its descendants. Floats are tracked per block
//! formatting context (BFC) in that context's coordinate space.

use crate::atom::k;
use crate::css::{self, Breadth, Track};
use crate::dom::*;
use crate::engine::Resources;
use crate::fonts::{fonts, Fonts};
use crate::image::Image;
use crate::style::*;
use std::rc::Rc;
use std::sync::Arc;

pub const NO_LINK: u32 = u32::MAX;
const NO_BOX: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Block,
    Table,
    Row,
    Cell,
    Replaced,
    Flex,
    Grid,
}

#[derive(Clone, Copy, Debug)]
pub enum Item {
    Text { start: u32, end: u32, node: NodeId, link: u32, bg: u32 },
    Br,
    Atomic(u32),
    /// A floated box met in inline content.
    Float(u32),
    /// Where an absolutely positioned box would have been (its static position).
    Anchor(u32),
    /// Start / end of an inline element with padding, borders or a background.
    Open(NodeId),
    Close(NodeId),
}

#[derive(Clone, Copy, Debug)]
pub enum FragKind {
    Text { start: u32, end: u32, node: NodeId, link: u32, bg: u32, w: f32 },
    Atomic(u32),
    /// One line's piece of a decorated inline element (`first` / `last`
    /// pieces carry the left / right edges). `y` is the baseline.
    InlineBox { node: NodeId, w: f32, first: bool, last: bool },
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
    /// Border-box origin relative to the parent's content box (for absolutely
    /// positioned boxes: relative to the containing block's padding box).
    pub x: f32,
    pub y: f32,
    /// Content-box size.
    pub w: f32,
    pub h: f32,
    pub m: [f32; 4],
    pub b: [f32; 4],
    pub p: [f32; 4],
    pub intrinsic: Option<(f32, f32)>,
    /// Replaced content without an intrinsic width (fills the container).
    pub fluid: bool,
    pub marker: Option<String>,
    /// First baseline, relative to the content box top.
    pub baseline: Option<f32>,
    pub colspan: u32,
    pub cell_off: f32,
    pub img: bool,
    pub repl: (f32, f32),
    pub link: u32,
    /// The box in whose flow this box appears.
    pub parent: u32,
    /// position: absolute | fixed
    pub abs: bool,
    pub fixed: bool,
    pub float: bool,
    /// Floats placed in this box's flow (positions relative to its content box).
    pub floats: Vec<u32>,
    /// Absolutely positioned boxes using this box as containing block.
    pub abs_kids: Vec<u32>,
    /// Static position relative to the parent's content box.
    pub static_pos: (f32, f32),
    /// position: relative offset.
    pub rel: (f32, f32),
    /// Decoded image for <img>.
    pub image: Option<Arc<Image>>,
}

impl LayoutTree {
    /// Visit every box with its absolute border-box origin, in paint order.
    /// `f(box, x, y)`; text fragments are reported as (node, rect) via `t`.
    fn walk(&self, id: u32, ox: f32, oy: f32, f: &mut dyn FnMut(&LBox, f32, f32), t: &mut dyn FnMut(NodeId, [f32; 4])) {
        let b = &self.boxes[id as usize];
        let (bx, by) = (ox + b.x + b.rel.0, oy + b.y + b.rel.1);
        f(b, bx, by);
        let (px, py) = (bx + b.b[3], by + b.b[0]);
        let (cx, cy) = (px + b.p[3], py + b.p[0] + b.cell_off);
        for &c in &b.children {
            if !self.boxes[c as usize].abs {
                self.walk(c, cx, cy, f, t);
            }
        }
        for &fl in &b.floats {
            self.walk(fl, cx, cy, f, t);
        }
        for fr in &b.frags {
            match fr.kind {
                FragKind::Text { node, w, .. } | FragKind::InlineBox { node, w, .. } => t(node, [cx + fr.x, cy + fr.y - 14.0, w, 18.0]),
                FragKind::Atomic(a) => self.walk(a, cx, cy, f, t),
            }
        }
        for &a in &b.abs_kids {
            let (ax, ay) = if self.boxes[a as usize].fixed { (0.0, 0.0) } else { (px, py) };
            self.walk(a, ax, ay, f, t);
        }
    }

    /// Border boxes of elements (inline elements: union of their text).
    pub fn node_rects(&self) -> crate::fx::FxHashMap<NodeId, [f32; 4]> {
        let mut m: crate::fx::FxHashMap<NodeId, [f32; 4]> = crate::fx::FxHashMap::default();
        let mut add = |n: NodeId, r: [f32; 4]| {
            m.entry(n)
                .and_modify(|e| {
                    let (x0, y0) = (e[0].min(r[0]), e[1].min(r[1]));
                    let (x1, y1) = ((e[0] + e[2]).max(r[0] + r[2]), (e[1] + e[3]).max(r[1] + r[3]));
                    *e = [x0, y0, x1 - x0, y1 - y0];
                })
                .or_insert(r);
        };
        let mut boxes = Vec::new();
        let mut texts = Vec::new();
        self.walk(self.root, 0.0, 0.0, &mut |b, x, y| {
            if !b.anon {
                boxes.push((b.node, [x, y, b.border_w(), b.border_h()]));
            }
        }, &mut |n, r| texts.push((n, r)));
        for (n, r) in boxes.into_iter().chain(texts) {
            add(self.pseudo_owner.get(&n).copied().unwrap_or(n), r);
        }
        m
    }

    /// An element's content box (page coordinates) and its own text
    /// fragments as (x, baseline y, text), for placing a text caret.
    pub fn text_box(&self, node: NodeId) -> Option<([f32; 4], Vec<(f32, f32, &str)>)> {
        let mut found = None;
        self.walk(self.root, 0.0, 0.0, &mut |b, bx, by| {
            if found.is_none() && b.node == node && !b.anon {
                let (cx, cy) = (bx + b.b[3] + b.p[3], by + b.b[0] + b.p[0] + b.cell_off);
                found = Some(([cx, cy, b.w, b.h], b as *const LBox));
            }
        }, &mut |_, _| {});
        let (rect, ptr) = found?;
        let b = self.boxes.iter().find(|x| std::ptr::eq(*x, ptr))?;
        let mut frags = Vec::new();
        collect_text(self, b, rect[0], rect[1], &mut frags);
        Some((rect, frags))
    }

    /// The element at page point (x, y): the last-painted box or text under it.
    pub fn node_at(&self, x: f32, y: f32) -> Option<NodeId> {
        let mut hit = None;
        let inside = |r: [f32; 4]| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3];
        let mut last_box = None;
        let mut last_text = None;
        self.walk(self.root, 0.0, 0.0, &mut |b, bx, by| {
            if !b.anon && inside([bx, by, b.border_w(), b.border_h()]) {
                last_box = Some(b.node);
            }
        }, &mut |n, r| {
            if inside(r) {
                last_text = Some(n);
            }
        });
        // Text is painted over its container's background.
        if let Some(t) = last_text {
            hit = Some(t);
        }
        hit.or(last_box).map(|n| self.pseudo_owner.get(&n).copied().unwrap_or(n))
    }
}

/// Text fragments of a box and its anonymous wrappers, in order.
fn collect_text<'a>(t: &'a LayoutTree, b: &LBox, cx: f32, cy: f32, out: &mut Vec<(f32, f32, &'a str)>) {
    for fr in &b.frags {
        if let FragKind::Text { start, end, .. } = fr.kind {
            out.push((cx + fr.x, cy + fr.y, t.text.get(start as usize..end as usize).unwrap_or("")));
        }
    }
    for &c in &b.children {
        let k = &t.boxes[c as usize];
        if k.anon {
            let (x, y) = (cx + k.x + k.b[3] + k.p[3], cy + k.y + k.b[0] + k.p[0]);
            collect_text(t, k, x, y, out);
        }
    }
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
    pub text: Rc<str>,
    pub links: Vec<String>,
    pub root: u32,
    pub height: f32,
    /// ::before / ::after box ids -> the element they belong to.
    pub pseudo_owner: crate::fx::FxHashMap<NodeId, NodeId>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Normal,
    /// Shrink-to-fit (inline-blocks, floats, absolutely positioned boxes).
    Fit,
    /// Border-box width imposed by the parent (table cells, flex/grid items).
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
            | Display::Flex
            | Display::Grid
    )
}

fn is_replaced(tag: crate::atom::Atom) -> bool {
    matches!(tag, k::img | k::svg | k::iframe | k::video | k::canvas | k::embed | k::object)
}

#[inline]
fn out_of_flow(st: &Style) -> bool {
    matches!(st.position, Pos::Absolute | Pos::Fixed) || st.float != Side::None
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

// ------------------------------------------------------------------ floats

#[derive(Clone, Copy)]
struct FRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    left: bool,
}

/// The floats of one block formatting context, in its coordinate space.
#[derive(Default)]
struct Bfc {
    floats: Vec<FRect>,
}

impl Bfc {
    #[inline]
    fn hits(f: &FRect, y: f32, h: f32) -> bool {
        f.y < y + h.max(0.01) && f.y + f.h > y
    }

    /// Free interval [l, r) of [x0, x0 + w) for the band [y, y + h).
    fn band(&self, x0: f32, w: f32, y: f32, h: f32) -> (f32, f32) {
        let (mut l, mut r) = (x0, x0 + w);
        for f in &self.floats {
            if Self::hits(f, y, h) {
                if f.left {
                    l = l.max(f.x + f.w);
                } else {
                    r = r.min(f.x);
                }
            }
        }
        (l, r.max(l))
    }

    /// The nearest bottom edge of a float overlapping the band.
    fn next_bottom(&self, y: f32, h: f32) -> Option<f32> {
        self.floats.iter().filter(|f| Self::hits(f, y, h)).map(|f| f.y + f.h).reduce(f32::min)
    }

    fn place(&mut self, x0: f32, w: f32, mut y: f32, fw: f32, fh: f32, left: bool) -> (f32, f32) {
        // A float may not be higher than an earlier one.
        if let Some(f) = self.floats.last() {
            y = y.max(f.y);
        }
        loop {
            let (l, r) = self.band(x0, w, y, fh);
            let next = self.next_bottom(y, fh);
            if r - l >= fw - 0.01 || next.is_none() {
                let x = if left { l } else { r - fw };
                self.floats.push(FRect { x, y, w: fw, h: fh, left });
                return (x, y);
            }
            y = next.unwrap();
        }
    }

    fn clear_y(&self, side: Side) -> f32 {
        self.floats
            .iter()
            .filter(|f| side == Side::Both || (side == Side::Left) == f.left)
            .map(|f| f.y + f.h)
            .fold(f32::NEG_INFINITY, f32::max)
    }

    fn bottom(&self) -> f32 {
        self.floats.iter().map(|f| f.y + f.h).fold(0.0, f32::max)
    }
}

// ------------------------------------------------------------------ engine

struct Engine<'a> {
    doc: &'a Document,
    styles: &'a [Style],
    f: &'static Fonts,
    boxes: Vec<LBox>,
    /// Text buffer while building; frozen into `ltext` for layout.
    text: String,
    ltext: Rc<str>,
    links: Vec<String>,
    contains_block: Vec<bool>,
    ws_space: bool,
    /// Definite content height of the current containing block (for
    /// percentage heights), or None when it depends on content.
    avail_h: Option<f32>,
    /// A definite height imposed on the next block laid out (absolutely
    /// positioned boxes with top + bottom).
    abs_h: Option<f32>,
    vw: f32,
    vh: f32,
    bfcs: Vec<Bfc>,
    /// Positioned ancestors while building (containing blocks for absolute boxes).
    cbs: Vec<u32>,
    root: u32,
    res: &'a Resources,
}

pub fn layout(doc: &Document, styles: &[Style], vw: f32, vh: f32, res: &Resources) -> LayoutTree {
    let mut e = Engine {
        doc,
        styles,
        f: fonts(),
        boxes: Vec::with_capacity(doc.nodes.len() / 2),
        text: String::with_capacity(doc.nodes.len() * 16),
        ltext: Rc::from(""),
        links: Vec::new(),
        contains_block: vec![false; doc.nodes.len()],
        ws_space: true,
        avail_h: Some(vh),
        abs_h: None,
        vw,
        vh,
        bfcs: Vec::new(),
        cbs: Vec::new(),
        root: 0,
        res,
    };
    e.compute_contains_block();
    let root = e.build_box(doc.html, NO_LINK, 0);
    e.root = root;
    e.link_parents(root);
    e.ltext = Rc::from(std::mem::take(&mut e.text));
    e.bfcs.push(Bfc::default());
    e.layout_block(root, vw, Mode::Normal, 0.0, 0.0);
    e.bfcs.pop();
    let r = &mut e.boxes[root as usize];
    r.x = r.m[3];
    r.y = r.m[0];
    let (ry, mut height) = (r.y, r.y + r.border_h() + r.m[2]);
    for &a in &e.boxes[root as usize].abs_kids {
        let b = &e.boxes[a as usize];
        if !b.fixed {
            height = height.max(ry + b.y + b.border_h());
        }
    }
    let pseudo_owner = (doc.nodes.len()..styles.len()).map(|p| (p as NodeId, styles[p].owner)).collect();
    LayoutTree { boxes: e.boxes, text: e.ltext, links: e.links, root, height, pseudo_owner }
}

impl<'a> Engine<'a> {
    // ------------------------------------------------------------ building

    fn compute_contains_block(&mut self) {
        let doc = self.doc;
        // Reverse (flat) tree order visits descendants before their ancestors.
        for (id, p, _) in doc.flat_preorder().into_iter().rev() {
            if doc.elem(id).is_none() {
                continue;
            }
            if self.blockish(id) && p != NONE {
                self.contains_block[p as usize] = true;
            }
            // A block-level ::before / ::after also makes its owner a block container.
            let st = &self.styles[id as usize];
            if [st.before, st.after].iter().any(|&ps| ps != 0 && self.blockish(ps)) {
                self.contains_block[id as usize] = true;
            }
        }
    }

    #[inline]
    fn is_pseudo(&self, c: NodeId) -> bool {
        c as usize >= self.doc.nodes.len()
    }

    /// Flat-tree children plus the ::before / ::after boxes.
    fn kids(&self, n: NodeId) -> Vec<NodeId> {
        let mut v = self.doc.flat_children(n);
        let st = &self.styles[n as usize];
        if st.before != 0 {
            v.insert(0, st.before);
        }
        if st.after != 0 {
            v.push(st.after);
        }
        v
    }

    /// The text a ::before / ::after generates.
    fn pseudo_text(&self, p: NodeId) -> String {
        let st = &self.styles[p as usize];
        let mut out = String::new();
        for item in st.content.iter().flat_map(|c| c.iter()) {
            match item {
                css::ContentItem::Str(s) => out.push_str(s),
                css::ContentItem::Attr(a) => out.push_str(self.doc.attr(st.owner, *a).unwrap_or("")),
                css::ContentItem::OpenQuote => out.push('\u{201c}'),
                css::ContentItem::CloseQuote => out.push('\u{201d}'),
                css::ContentItem::Counter | css::ContentItem::Url => {}
            }
        }
        out
    }

    /// The box for a non-inline ::before / ::after.
    fn build_pseudo_box(&mut self, p: NodeId, link: u32) -> u32 {
        let bx = self.new_box(Kind::Block, p, false);
        self.boxes[bx as usize].link = link;
        let t = self.pseudo_text(p);
        if !t.is_empty() {
            let saved = self.ws_space;
            self.ws_space = true;
            self.push_text(&t, p, bx, link, 0);
            self.ws_space = saved;
        }
        bx
    }

    /// Does this child force its parent into block context?
    #[inline]
    fn blockish(&self, c: NodeId) -> bool {
        if self.is_pseudo(c) {
            let st = &self.styles[c as usize];
            return !out_of_flow(st) && is_block_level(st.display);
        }
        let Some(el) = self.doc.elem(c) else { return false };
        let st = &self.styles[c as usize];
        if out_of_flow(st) {
            return false;
        }
        match st.display {
            Display::None => false,
            Display::Inline | Display::Contents => !is_replaced(el.tag) && self.contains_block[c as usize],
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
            fluid: false,
            marker: None,
            baseline: None,
            colspan: 1,
            cell_off: 0.0,
            img: false,
            repl: (0.0, 0.0),
            link: NO_LINK,
            parent: NO_BOX,
            abs: false,
            fixed: false,
            float: false,
            floats: Vec::new(),
            abs_kids: Vec::new(),
            static_pos: (0.0, 0.0),
            rel: (0.0, 0.0),
            image: None,
        });
        id
    }

    fn link_parents(&mut self, root: u32) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let (nc, ni) = (self.boxes[id as usize].children.len(), self.boxes[id as usize].items.len());
            for i in 0..nc {
                let c = self.boxes[id as usize].children[i];
                self.boxes[c as usize].parent = id;
                stack.push(c);
            }
            for i in 0..ni {
                if let Item::Atomic(c) | Item::Float(c) | Item::Anchor(c) = self.boxes[id as usize].items[i] {
                    self.boxes[c as usize].parent = id;
                    stack.push(c);
                }
            }
        }
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

    /// Build the box for element `c` (blockified) and its contents.
    fn build_box(&mut self, c: NodeId, link: u32, ordinal: i32) -> u32 {
        if self.is_pseudo(c) {
            return self.build_pseudo_box(c, link);
        }
        let styles = self.styles;
        let st = &styles[c as usize];
        let link = self.link_for(c, link);
        let tag = self.doc.tag(c).unwrap();
        let kind = if is_replaced(tag) {
            Kind::Replaced
        } else {
            match st.display {
                Display::Table | Display::InlineTable => Kind::Table,
                Display::Flex | Display::InlineFlex => Kind::Flex,
                Display::Grid | Display::InlineGrid => Kind::Grid,
                _ => Kind::Block,
            }
        };
        let bx = self.new_box(kind, c, false);
        self.boxes[bx as usize].link = link;
        let positioned = st.position != Pos::Static;
        if positioned {
            self.cbs.push(bx);
        }
        match kind {
            Kind::Replaced => {
                let size = match tag {
                    k::iframe | k::video | k::canvas | k::embed | k::object => (300.0, 150.0),
                    _ => (0.0, 0.0),
                };
                let img = if tag == k::img || tag == k::svg { self.res.img(c).cloned() } else { None };
                let b = &mut self.boxes[bx as usize];
                b.repl = img.as_ref().map_or(size, |i| (i.iw, i.ih));
                b.fluid = img.as_ref().is_some_and(|i| i.fluid);
                b.img = tag == k::img;
                b.image = img;
            }
            Kind::Table => {
                let mut anon_row = None;
                self.collect_rows(c, bx, link, &mut anon_row);
            }
            Kind::Flex | Kind::Grid => self.build_items(c, bx, link),
            _ => {
                if st.display == Display::ListItem {
                    self.boxes[bx as usize].marker = marker_text(st.list, ordinal);
                }
                self.build_container(c, bx, link);
            }
        }
        if positioned {
            self.cbs.pop();
        }
        bx
    }

    /// Build a float or an absolutely positioned box. Returns (box, is_float).
    fn build_oof(&mut self, c: NodeId, link: u32) -> (u32, bool) {
        let styles = self.styles;
        let st = &styles[c as usize];
        let id = self.build_box(c, link, 0);
        if matches!(st.position, Pos::Absolute | Pos::Fixed) {
            let fixed = st.position == Pos::Fixed;
            let cb = if fixed { self.root } else { self.cbs.last().copied().unwrap_or(self.root) };
            let b = &mut self.boxes[id as usize];
            b.abs = true;
            b.fixed = fixed;
            self.boxes[cb as usize].abs_kids.push(id);
            (id, false)
        } else {
            self.boxes[id as usize].float = true;
            (id, true)
        }
    }

    fn build_container(&mut self, node: NodeId, bx: u32, link: u32) {
        let saved = self.ws_space;
        self.ws_space = true;
        if !self.contains_block[node as usize] {
            match crate::forms::display_text(self.doc, node) {
                Some(t) => self.push_text(&t, node, bx, link, 0),
                None => {
                    for c in self.kids(node) {
                        self.collect_inline(c, bx, link, 0);
                    }
                }
            }
            if self.items_blank(bx) {
                self.boxes[bx as usize].items.clear();
            }
            self.ws_space = saved;
            return;
        }
        let mut anon: Option<u32> = None;
        let mut ordinal = self.doc.attr(node, k::start).and_then(|s| s.trim().parse::<i32>().ok()).unwrap_or(1);
        for c in self.kids(node) {
            if self.blockish(c) {
                if let Some(a) = anon.take() {
                    self.finish_anon(bx, a);
                }
                let n = if self.styles[c as usize].display == Display::ListItem {
                    ordinal += 1;
                    ordinal - 1
                } else {
                    0
                };
                let cb = self.build_box(c, link, n);
                self.boxes[bx as usize].children.push(cb);
            } else {
                if anon.is_none() {
                    if let Some(Data::Text(t)) = self.doc.nodes.get(c as usize).map(|n| &n.data) {
                        if !self.styles[node as usize].ws.preserves() && t.bytes().all(|b| b.is_ascii_whitespace()) {
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

    /// Children of a flex or grid container: every in-flow child becomes an item.
    fn build_items(&mut self, node: NodeId, bx: u32, link: u32) {
        let saved = self.ws_space;
        let doc = self.doc;
        for c in self.kids(node) {
            if self.is_pseudo(c) {
                let st = &self.styles[c as usize];
                let id = if matches!(st.position, Pos::Absolute | Pos::Fixed) { self.build_oof(c, link).0 } else { self.build_box(c, link, 0) };
                self.boxes[bx as usize].children.push(id);
                continue;
            }
            match &doc.nodes[c as usize].data {
                Data::Text(t) => {
                    if t.bytes().all(|b| b.is_ascii_whitespace()) {
                        continue;
                    }
                    let a = self.new_box(Kind::Block, node, true);
                    self.ws_space = true;
                    self.collect_inline(c, a, link, 0);
                    if !self.items_blank(a) {
                        self.boxes[bx as usize].children.push(a);
                    }
                }
                Data::Element(_) => {
                    let st = &self.styles[c as usize];
                    match st.display {
                        Display::None => continue,
                        Display::Contents => {
                            self.build_items(c, bx, link);
                            continue;
                        }
                        _ => {}
                    }
                    let id = if matches!(st.position, Pos::Absolute | Pos::Fixed) {
                        self.build_oof(c, link).0
                    } else {
                        self.build_box(c, link, 0)
                    };
                    self.boxes[bx as usize].children.push(id);
                }
                _ => {}
            }
        }
        self.ws_space = saved;
    }

    fn items_blank(&self, bx: u32) -> bool {
        self.boxes[bx as usize].items.iter().all(|it| match *it {
            Item::Text { start, end, node, .. } => {
                !self.styles[node as usize].ws.preserves() && self.text[start as usize..end as usize].bytes().all(|b| b == b' ')
            }
            _ => false,
        })
    }

    fn finish_anon(&mut self, parent: u32, a: u32) {
        if !self.items_blank(a) {
            self.boxes[parent as usize].children.push(a);
        }
    }

    fn collect_rows(&mut self, node: NodeId, t: u32, link: u32, anon_row: &mut Option<u32>) {
        for c in self.doc.flat_children(node) {
            if self.doc.elem(c).is_none() {
                continue;
            }
            match self.styles[c as usize].display {
                Display::None => {}
                Display::TableRow => {
                    *anon_row = None;
                    let r = self.new_box(Kind::Row, c, false);
                    self.collect_cells(c, r, link);
                    self.boxes[t as usize].children.push(r);
                }
                Display::TableCaption => {
                    *anon_row = None;
                    let b = self.build_box(c, link, 0);
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
        for c in self.doc.flat_children(row) {
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
        let span = self.doc.attr(c, k::colspan).and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(1).clamp(1, 1000);
        self.boxes[cell as usize].colspan = span;
        let positioned = self.styles[c as usize].position != Pos::Static;
        if positioned {
            self.cbs.push(cell);
        }
        self.build_container(c, cell, link);
        if positioned {
            self.cbs.pop();
        }
        cell
    }

    fn collect_inline(&mut self, c: NodeId, bx: u32, link: u32, bg: u32) {
        let doc = self.doc;
        let styles = self.styles;
        if self.is_pseudo(c) {
            let st = &styles[c as usize];
            if out_of_flow(st) {
                let (id, is_float) = self.build_oof(c, link);
                self.boxes[bx as usize].items.push(if is_float { Item::Float(id) } else { Item::Anchor(id) });
            } else if matches!(st.display, Display::Inline | Display::Contents) {
                let t = self.pseudo_text(c);
                let bg = if st.bg >> 24 != 0 { st.bg } else { bg };
                if !t.is_empty() {
                    self.push_text(&t, c, bx, link, bg);
                }
            } else {
                let saved = self.ws_space;
                let id = self.build_box(c, link, 0);
                self.ws_space = saved;
                self.boxes[bx as usize].items.push(Item::Atomic(id));
                self.ws_space = false;
            }
            return;
        }
        match &doc.nodes[c as usize].data {
            Data::Text(t) => {
                let parent = doc.nodes[c as usize].parent;
                self.push_text(t, parent, bx, link, bg);
            }
            Data::Element(el) => {
                let st = &styles[c as usize];
                if st.display == Display::None {
                    return;
                }
                if out_of_flow(st) {
                    let (id, is_float) = self.build_oof(c, link);
                    self.boxes[bx as usize].items.push(if is_float { Item::Float(id) } else { Item::Anchor(id) });
                    return;
                }
                if el.tag == k::br {
                    self.boxes[bx as usize].items.push(Item::Br);
                    self.ws_space = true;
                    return;
                }
                if el.tag == k::img && st.width.is_auto() && st.height.is_auto() && self.res.img(c).is_none() {
                    // Not loaded (yet): show the alt text instead.
                    if let Some(alt) = el.attr(k::alt) {
                        if !alt.trim().is_empty() {
                            let link = self.link_for(c, link);
                            self.push_text(alt, c, bx, link, bg);
                        }
                    }
                    return;
                }
                let atomic = is_replaced(el.tag)
                    || matches!(
                        st.display,
                        Display::InlineBlock
                            | Display::InlineTable
                            | Display::Table
                            | Display::InlineFlex
                            | Display::InlineGrid
                            | Display::Flex
                            | Display::Grid
                    );
                if atomic {
                    let saved = self.ws_space;
                    let id = self.build_box(c, link, 0);
                    self.ws_space = saved;
                    self.boxes[bx as usize].items.push(Item::Atomic(id));
                    self.ws_space = false;
                    return;
                }
                let link = self.link_for(c, link);
                let decorated = st.bg >> 24 != 0
                    || st.bg_image.is_some()
                    || st.bw.iter().any(|&v| v > 0.0)
                    || st.padding.iter().any(|p| *p != Len::Px(0.0))
                    || st.margin[1] != Len::Px(0.0)
                    || st.margin[3] != Len::Px(0.0);
                // A decorated inline paints its own background.
                let bg = if decorated { bg } else if st.bg >> 24 != 0 { st.bg } else { bg };
                if decorated {
                    self.boxes[bx as usize].items.push(Item::Open(c));
                }
                if let Some(t) = crate::forms::display_text(doc, c) {
                    self.push_text(&t, c, bx, link, bg);
                } else {
                    for cc in self.kids(c) {
                        self.collect_inline(cc, bx, link, bg);
                    }
                }
                if decorated {
                    self.boxes[bx as usize].items.push(Item::Close(c));
                }
            }
            _ => {}
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
            self.boxes[bx as usize].items.push(Item::Text { start: start as u32, end: self.text.len() as u32, node: style_node, link, bg });
        }
    }

    // ------------------------------------------------------------- helpers

    #[inline]
    /// A height (or min/max height) in px, if definite: percentages need a
    /// definite containing block height.
    fn resolve_h(&self, l: Len) -> Option<f32> {
        match l {
            Len::Auto => None,
            Len::Px(v) => Some(v),
            other if other.parts()[0] == 0.0 => Some(other.px(0.0)),
            other => self.avail_h.map(|b| other.px(b)),
        }
    }

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
        self.f.pick_style(st)
    }

    /// Resolve a CSS length value (used for grid tracks).
    fn val_px(&self, v: css::Value, base: f32, fs: f32) -> f32 {
        use css::Value as V;
        match v {
            V::Px(p) => p,
            V::Em(e) => e * fs,
            V::Rem(r) => r * 16.0,
            V::Pct(p) => p * base / 100.0,
            V::Vw(x) => x * self.vw / 100.0,
            V::Vh(x) => x * self.vh / 100.0,
            V::Calc(c) => c[0] * base / 100.0 + c[1] + c[2] * fs + c[3] * 16.0 + c[4] * self.vw / 100.0 + c[5] * self.vh / 100.0,
            _ => 0.0,
        }
    }

    fn is_bfc_root(&self, id: u32, mode: Mode) -> bool {
        let b = &self.boxes[id as usize];
        if mode != Mode::Normal || id == self.root || b.float || b.abs {
            return true;
        }
        if b.anon {
            return false;
        }
        if matches!(b.kind, Kind::Cell | Kind::Table | Kind::Flex | Kind::Grid | Kind::Replaced) {
            return true;
        }
        let st = self.st(id);
        st.overflow != Ovf::Visible || st.flow_root
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
                let p = st.padding[i].fixed().unwrap_or(0.0) + st.bw[i];
                pb += p;
                pbm += p;
                if kind != Kind::Cell {
                    pbm += st.margin[i].fixed().unwrap_or(0.0);
                }
            }
        }
        let (cmin, cmax) = self.intrinsic_content(id);
        if !anon {
            if let Some(w) = st.width.fixed() {
                let w = if st.border_box || kind == Kind::Table { w - pb } else { w }.max(0.0);
                if kind == Kind::Cell || kind == Kind::Table {
                    return (cmin.max(0.0) + pbm, w.max(cmin) + pbm);
                }
                return (w + pbm, w + pbm);
            }
        }
        let (mut mn, mut mx) = (cmin, cmax);
        if !anon {
            if let Some(m) = st.max_w.fixed() {
                let m = if st.border_box { m - pb } else { m };
                mx = mx.min(m);
                mn = mn.min(m);
            }
            if let Some(m) = st.min_w.fixed() {
                let m = if st.border_box { m - pb } else { m };
                mx = mx.max(m);
                mn = mn.max(m);
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
                let (mut nw, mut nh) = self.boxes[id as usize].repl;
                if self.boxes[id as usize].fluid && nw > 0.0 {
                    // CSS default object width for content with only an aspect ratio.
                    nh *= 300.0 / nw;
                    nw = 300.0;
                }
                let w = st.width.fixed().unwrap_or_else(|| match st.height.fixed() {
                    Some(h) if nh > 0.0 => h * nw / nh,
                    _ => nw,
                });
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
            Kind::Flex => self.flex_intrinsic(id),
            Kind::Grid => self.grid_intrinsic(id),
            _ if !self.boxes[id as usize].items.is_empty() => self.inline_intrinsic(id),
            _ => {
                let ch = self.boxes[id as usize].children.clone();
                let mut r = (0.0f32, 0.0f32);
                for &c in &ch {
                    if self.boxes[c as usize].abs {
                        continue;
                    }
                    let (a, b) = self.intrinsic_outer(c);
                    r = (r.0.max(a), r.1.max(b));
                }
                r
            }
        };
        self.boxes[id as usize].intrinsic = Some(v);
        v
    }

    fn in_flow_children(&self, id: u32) -> Vec<u32> {
        self.boxes[id as usize].children.iter().copied().filter(|&c| !self.boxes[c as usize].abs).collect()
    }

    fn flex_intrinsic(&mut self, id: u32) -> (f32, f32) {
        let st = self.st(id);
        let row = matches!(st.flex_dir, FlexDir::Row | FlexDir::RowRev);
        let items = self.in_flow_children(id);
        let gap = st.col_gap.fixed().unwrap_or(0.0) * (items.len().max(1) - 1) as f32;
        let mut r = (0.0f32, 0.0f32);
        for c in items {
            let (a, b) = self.intrinsic_outer(c);
            if row {
                r = (if st.wrap { r.0.max(a) } else { r.0 + a }, r.1 + b);
            } else {
                r = (r.0.max(a), r.1.max(b));
            }
        }
        if row {
            (r.0 + if st.wrap { 0.0 } else { gap }, r.1 + gap)
        } else {
            r
        }
    }

    fn grid_intrinsic(&mut self, id: u32) -> (f32, f32) {
        let st = self.st(id);
        let fs = st.font_size;
        let tracks: Vec<Track> = match &st.grid_cols {
            Some(tl) => {
                let mut t = tl.tracks.clone();
                if let Some((pos, rep)) = &tl.auto_repeat {
                    for (i, r) in rep.iter().enumerate() {
                        t.insert(pos + i, *r);
                    }
                }
                t
            }
            None => Vec::new(),
        };
        let n = tracks.len().max(1);
        let mut cols = vec![(0.0f32, 0.0f32); n];
        for (i, c) in self.in_flow_children(id).into_iter().enumerate() {
            let (a, b) = self.intrinsic_outer(c);
            let e = &mut cols[i % n];
            *e = (e.0.max(a), e.1.max(b));
        }
        for (i, t) in tracks.iter().enumerate() {
            if let Breadth::Len(v) = t.max {
                let px = self.val_px(v, 0.0, fs);
                cols[i] = (px, px);
            } else if let Breadth::Len(v) = t.min {
                let px = self.val_px(v, 0.0, fs);
                cols[i] = (cols[i].0.max(px), cols[i].1.max(px));
            }
        }
        let gap = st.col_gap.fixed().unwrap_or(0.0) * (n - 1) as f32;
        (cols.iter().map(|c| c.0).sum::<f32>() + gap, cols.iter().map(|c| c.1).sum::<f32>() + gap)
    }

    fn inline_intrinsic(&mut self, id: u32) -> (f32, f32) {
        let items = std::mem::take(&mut self.boxes[id as usize].items);
        let text = if self.ltext.is_empty() { Rc::from(self.text.as_str()) } else { self.ltext.clone() };
        let (mut mn, mut mx, mut line) = (0.0f32, 0.0f32, 0.0f32);
        let mut nowrap_run = 0.0f32;
        for it in &items {
            match *it {
                Item::Br => {
                    mx = mx.max(line);
                    line = 0.0;
                    nowrap_run = 0.0;
                }
                Item::Atomic(a) | Item::Float(a) => {
                    let (amn, amx) = self.intrinsic_outer(a);
                    mn = mn.max(amn);
                    line += amx;
                    nowrap_run = 0.0;
                }
                Item::Anchor(_) => {}
                Item::Open(n) | Item::Close(n) => {
                    let st = &self.styles[n as usize];
                    let side = if matches!(*it, Item::Open(_)) { 3 } else { 1 };
                    let edge = st.margin[side].fixed().unwrap_or(0.0) + st.bw[side] + st.padding[side].fixed().unwrap_or(0.0);
                    line += edge;
                    nowrap_run += edge;
                }
                Item::Text { start, end, node, .. } => {
                    let st = &self.styles[node as usize];
                    let face = self.face_of(st);
                    let fs = st.font_size;
                    let wraps = st.ws.wraps();
                    let space = self.f.measure(face, fs, " ");
                    for (pi, seg) in text[start as usize..end as usize].split('\n').enumerate() {
                        if pi > 0 {
                            mx = mx.max(line);
                            line = 0.0;
                            nowrap_run = 0.0;
                        }
                        if wraps {
                            for word in seg.split(' ') {
                                let w = self.f.measure(face, fs, word);
                                // Min-content: the widest unbreakable piece.
                                if st.wrap_anywhere {
                                    for (i, ch) in word.char_indices() {
                                        mn = mn.max(self.f.measure(face, fs, &word[i..i + ch.len_utf8()]));
                                    }
                                } else {
                                    let mut s0 = 0;
                                    for cut in word_breaks(word, st.word_break).into_iter().chain([word.len()]) {
                                        mn = mn.max(self.f.measure(face, fs, &word[s0..cut]));
                                        s0 = cut;
                                    }
                                }
                                line += w + space;
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

    /// Lay out box `id` in a containing block `cb_w` wide. (ox, oy) is the
    /// parent's content-left and this box's tentative border-top, in the
    /// coordinates of the current block formatting context.
    fn layout_block(&mut self, id: u32, cb_w: f32, mode: Mode, ox: f32, oy: f32) {
        self.resolve_edges(id, cb_w);
        let kind = self.boxes[id as usize].kind;
        if kind == Kind::Table {
            self.layout_table(id, cb_w, mode);
            return self.after_layout(id, cb_w);
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
            let (nw, nh) = self.boxes[id as usize].repl;
            let ratio = if nw > 0.0 && nh > 0.0 { Some(nh / nw) } else { None };
            let spec_w = match mode {
                Mode::Forced(fw) if st.width.is_auto() => Some(fw - pb_h),
                _ => (!st.width.is_auto()).then(|| st.width.px(cb_w) - adj_h),
            };
            let spec_h = self.resolve_h(st.height).map(|h| h - adj_v);
            let (mut w, mut h) = match (spec_w, spec_h) {
                (Some(w), Some(h)) => (w, h),
                (Some(w), None) => (w, ratio.map_or(nh, |r| w * r)),
                (None, Some(h)) => (ratio.map_or(nw, |r| h / r), h),
                (None, None) if self.boxes[id as usize].fluid => {
                    let w = (cb_w - pb_h - m[1] - m[3]).max(0.0);
                    (w, ratio.map_or(nh, |r| w * r))
                }
                (None, None) => (nw, nh),
            };
            // min/max constraints keep the aspect ratio of auto dimensions.
            if !st.max_w.is_auto() {
                let mx = st.max_w.px(cb_w) - adj_h;
                if w > mx {
                    w = mx;
                    if spec_h.is_none() {
                        h = ratio.map_or(h, |r| w * r);
                    }
                }
            }
            let mn = st.min_w.px(cb_w) - adj_h;
            if w < mn {
                w = mn;
                if spec_h.is_none() {
                    h = ratio.map_or(h, |r| w * r);
                }
            }
            if let Some(mx) = self.resolve_h(st.max_h) {
                let mx = mx - adj_v;
                if h > mx {
                    h = mx;
                    if spec_w.is_none() {
                        w = ratio.map_or(w, |r| h / r);
                    }
                }
            }
            let bx = &mut self.boxes[id as usize];
            bx.w = w.max(0.0);
            bx.h = h.max(0.0);
            return self.after_layout(id, cb_w);
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
            bx.floats.clear();
        }

        let new_bfc = self.is_bfc_root(id, mode);
        let (cx, cy) = if new_bfc { (0.0, 0.0) } else { (ox + m[3] + b[3] + p[3], oy + b[0] + p[0]) };
        if new_bfc {
            self.bfcs.push(Bfc::default());
        }
        // This box's definite content height is the base for its children's
        // percentage heights.
        let forced = self.abs_h.take();
        let def_h = if anon { self.avail_h } else { forced.or_else(|| self.resolve_h(st.height).map(|h| (h - adj_v).max(0.0))) };
        let saved_avail = std::mem::replace(&mut self.avail_h, def_h);
        let mut content_h = match kind {
            Kind::Flex => self.layout_flex(id, w),
            Kind::Grid => self.layout_grid(id, w),
            _ if !self.boxes[id as usize].items.is_empty() => self.layout_inline(id, w, cx, cy),
            _ => self.layout_children(id, w, new_bfc, cx, cy),
        };
        if new_bfc {
            let bfc = self.bfcs.pop().unwrap();
            content_h = content_h.max(bfc.bottom());
        }
        self.avail_h = saved_avail;

        let mut h = match def_h {
            Some(h) if !anon => h,
            _ => content_h,
        };
        if !anon {
            if let Some(mh) = self.resolve_h(st.max_h) {
                h = h.min(mh - adj_v);
            }
            if let Some(mh) = self.resolve_h(st.min_h) {
                h = h.max(mh - adj_v);
            }
        }
        self.boxes[id as usize].h = h.max(0.0);
        self.after_layout(id, cb_w);
    }

    /// Relative offsets, then absolutely positioned descendants.
    fn after_layout(&mut self, id: u32, cb_w: f32) {
        let st = self.st(id);
        if !self.boxes[id as usize].anon && matches!(st.position, Pos::Relative | Pos::Sticky) {
            let [t, r, b, l] = st.inset;
            let dx = if !l.is_auto() { l.px(cb_w) } else if !r.is_auto() { -r.px(cb_w) } else { 0.0 };
            let base = self.avail_h.unwrap_or(0.0);
            let dy = if !t.is_auto() { t.px(base) } else if !b.is_auto() { -b.px(base) } else { 0.0 };
            // Sticky elements stay in flow.
            self.boxes[id as usize].rel = if st.position == Pos::Relative { (dx, dy) } else { (0.0, 0.0) };
        }
        if !self.boxes[id as usize].anon {
            let (bw, bh) = (self.boxes[id as usize].border_w(), self.boxes[id as usize].border_h());
            if let Some((tx, ty)) = st.pure_translate(bw, bh) {
                let r = &mut self.boxes[id as usize].rel;
                r.0 += tx;
                r.1 += ty;
            }
        }
        if !self.boxes[id as usize].abs_kids.is_empty() {
            self.layout_abs(id);
        }
    }

    fn place_float(&mut self, owner: u32, f: u32, w: f32, cx: f32, cy: f32, y: f32) {
        self.layout_block(f, w, Mode::Fit, 0.0, 0.0);
        let fb = &self.boxes[f as usize];
        let (mw, mh) = (fb.m[3] + fb.border_w() + fb.m[1], fb.m[0] + fb.border_h() + fb.m[2]);
        let left = self.st(f).float != Side::Right;
        let (fx, fy) = self.bfcs.last_mut().unwrap().place(cx, w, cy + y, mw, mh, left);
        let fb = &mut self.boxes[f as usize];
        fb.x = fx - cx + fb.m[3];
        fb.y = fy - cy + fb.m[0];
        self.boxes[owner as usize].floats.push(f);
    }

    fn layout_children(&mut self, id: u32, w: f32, bfc_root: bool, cx: f32, cy: f32) -> f32 {
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
            if self.boxes[c as usize].abs {
                self.boxes[c as usize].static_pos = (0.0, cursor + pending.max(0.0));
                continue;
            }
            if self.boxes[c as usize].float {
                self.place_float(id, c, w, cx, cy, cursor + pending.max(0.0));
                continue;
            }
            let canon = self.boxes[c as usize].anon;
            let cst = self.st(c);
            if !canon && cst.clear != Side::None {
                let cl = self.bfcs.last().map_or(f32::NEG_INFINITY, |b| b.clear_y(cst.clear)) - cy;
                let mt = cst.margin[0].px(w);
                if cl > cursor + collapse(pending, mt) {
                    cursor = cl;
                    pending = -mt;
                    first = false;
                }
            }
            let own_mt = if canon { 0.0 } else { cst.margin[0].px(w) };
            let tent_y = cursor + if first && hoist_top { 0.0 } else { collapse(pending, own_mt) };
            // A new formatting context next to floats is narrowed to avoid them.
            let (mut avail, mut x_off) = (w, 0.0);
            if !canon && self.is_bfc_root(c, Mode::Normal) {
                if let Some(bf) = self.bfcs.last() {
                    if !bf.floats.is_empty() {
                        let (l, r) = bf.band(cx, w, cy + tent_y, 1.0);
                        x_off = l - cx;
                        avail = r - l;
                    }
                }
            }
            self.layout_block(c, avail, Mode::Normal, cx + x_off, cy + tent_y);
            if center && !canon {
                // -webkit-center: centre narrower child blocks without auto margins.
                let cb = &mut self.boxes[c as usize];
                let free = avail - (cb.m[3] + cb.border_w() + cb.m[1]);
                if free > 0.5 && !cst.margin[1].is_auto() && !cst.margin[3].is_auto() {
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
            cb.x = cb.m[3] + x_off;
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

    // ------------------------------------------------------------ position

    fn layout_abs(&mut self, cb: u32) {
        let kids = self.boxes[cb as usize].abs_kids.clone();
        let (cbw, cbh) = if cb == self.root {
            (self.vw, self.vh)
        } else {
            let b = &self.boxes[cb as usize];
            (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2])
        };
        for a in kids {
            let st = self.st(a);
            let (pw, ph) = if self.boxes[a as usize].fixed { (self.vw, self.vh) } else { (cbw, cbh) };
            let [t, r, bt, l] = st.inset;
            let mode = if st.width.is_auto() && !l.is_auto() && !r.is_auto() {
                let ml = st.margin[3].px(pw);
                let mr = st.margin[1].px(pw);
                Mode::Forced((pw - l.px(pw) - r.px(pw) - ml - mr).max(0.0))
            } else {
                Mode::Fit
            };
            if st.height.is_auto() && !t.is_auto() && !bt.is_auto() {
                // top + bottom define the height.
                let pbv = st.padding[0].px(pw) + st.padding[2].px(pw) + st.bw[0] + st.bw[2];
                let mv = st.margin[0].px(pw) + st.margin[2].px(pw);
                self.abs_h = Some((ph - t.px(ph) - bt.px(ph) - mv - pbv).max(0.0));
            }
            let saved = std::mem::replace(&mut self.avail_h, Some(ph));
            self.layout_block(a, pw, mode, 0.0, 0.0);
            self.avail_h = saved;
            self.abs_h = None;
            if st.height.is_auto() && !t.is_auto() && !bt.is_auto() {
                let b = &mut self.boxes[a as usize];
                let pbv = b.p[0] + b.p[2] + b.b[0] + b.b[2];
                b.h = (ph - t.px(ph) - bt.px(ph) - b.m[0] - b.m[2] - pbv).max(0.0);
            }
            let (sx, sy) = self.static_in_cb(a, cb);
            let b = &self.boxes[a as usize];
            let x = if !l.is_auto() {
                l.px(pw) + b.m[3]
            } else if !r.is_auto() {
                pw - r.px(pw) - b.m[1] - b.border_w()
            } else {
                sx + b.m[3]
            };
            let y = if !t.is_auto() {
                t.px(ph) + b.m[0]
            } else if !bt.is_auto() {
                ph - bt.px(ph) - b.m[2] - b.border_h()
            } else {
                sy + b.m[0]
            };
            let b = &mut self.boxes[a as usize];
            b.x = x;
            b.y = y;
        }
    }

    /// Static position of `a` in the padding-box coordinates of `cb`.
    fn static_in_cb(&self, a: u32, cb: u32) -> (f32, f32) {
        let (mut x, mut y) = self.boxes[a as usize].static_pos;
        let mut b = self.boxes[a as usize].parent;
        let mut guard = 0;
        while b != cb && b != NO_BOX && guard < 10_000 {
            let bb = &self.boxes[b as usize];
            x += bb.x + bb.b[3] + bb.p[3];
            y += bb.y + bb.b[0] + bb.p[0] + bb.cell_off;
            b = bb.parent;
            guard += 1;
        }
        if b == cb {
            let c = &self.boxes[cb as usize];
            (x + c.p[3], y + c.p[0])
        } else {
            (0.0, 0.0)
        }
    }

    // -------------------------------------------------------------- inline

    fn begin_line(&self, lb: &mut LineBuilder, w: f32, cx: f32, cy: f32, need: f32) {
        if lb.started {
            return;
        }
        lb.started = true;
        lb.left = 0.0;
        lb.width = w;
        let Some(bf) = self.bfcs.last() else { return };
        if bf.floats.is_empty() {
            return;
        }
        let h = lb.strut.0 + lb.strut.1;
        loop {
            let (l, r) = bf.band(cx, w, cy + lb.y, h);
            let next = bf.next_bottom(cy + lb.y, h);
            if r - l >= need.min(w) - 0.01 || next.is_none() {
                lb.left = l - cx;
                lb.width = r - l;
                return;
            }
            lb.y = next.unwrap() - cy;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn end_line(&mut self, id: u32, lb: &mut LineBuilder, forced: bool, w: f32, cx: f32, cy: f32) {
        lb.finish(forced);
        if !lb.pending_floats.is_empty() {
            for f in std::mem::take(&mut lb.pending_floats) {
                self.place_float(id, f, w, cx, cy, lb.y);
            }
        }
    }

    fn layout_inline(&mut self, id: u32, w: f32, cx: f32, cy: f32) -> f32 {
        let items = std::mem::take(&mut self.boxes[id as usize].items);
        let text = self.ltext.clone();
        let cst = self.st(id);
        let cface = self.face_of(cst);
        let (ca, cd, cl) = self.f.metrics(cface);
        let lh = cst.line_height_px(cl);
        let half = (lh - (ca + cd) * cst.font_size) / 2.0;
        let strut = (half + ca * cst.font_size, lh - (half + ca * cst.font_size));
        let mut lb = LineBuilder {
            frags: Vec::new(),
            atomics: Vec::new(),
            pending_floats: Vec::new(),
            line_start: 0,
            x: 0.0,
            y: 0.0,
            asc: strut.0,
            desc: strut.1,
            strut,
            has_content: false,
            started: false,
            trailing_w: 0.0,
            trailing_bytes: 0,
            left: 0.0,
            width: w,
            align: cst.align,
            baseline: None,
            line_bottoms: Vec::new(),
            open: Vec::new(),
        };
        for it in &items {
            match *it {
                Item::Br => {
                    self.begin_line(&mut lb, w, cx, cy, 0.0);
                    self.end_line(id, &mut lb, true, w, cx, cy);
                }
                Item::Float(f) => {
                    if lb.has_content {
                        lb.pending_floats.push(f);
                    } else {
                        self.place_float(id, f, w, cx, cy, lb.y);
                        lb.started = false;
                    }
                }
                Item::Anchor(a) => self.boxes[a as usize].static_pos = (lb.left + lb.x, lb.y),
                Item::Open(n) => {
                    let st = &self.styles[n as usize];
                    self.begin_line(&mut lb, w, cx, cy, 0.0);
                    lb.x += st.margin[3].px(w);
                    let idx = lb.frags.len();
                    lb.frags.push(Frag { x: lb.x, y: 0.0, kind: FragKind::InlineBox { node: n, w: 0.0, first: true, last: false } });
                    lb.x += st.bw[3] + st.padding[3].px(w);
                    lb.open.push((n, idx));
                    lb.has_content = true;
                    lb.trailing_w = 0.0;
                    lb.trailing_bytes = 0;
                }
                Item::Close(n) => {
                    let st = &self.styles[n as usize];
                    lb.x += st.padding[1].px(w) + st.bw[1];
                    if let Some(pos) = lb.open.iter().rposition(|&(m, _)| m == n) {
                        let (_, idx) = lb.open.remove(pos);
                        let x0 = lb.frags[idx].x;
                        if let FragKind::InlineBox { w: bw, last, .. } = &mut lb.frags[idx].kind {
                            *bw = lb.x - x0;
                            *last = true;
                        }
                    }
                    lb.x += st.margin[1].px(w);
                    lb.trailing_w = 0.0;
                    lb.trailing_bytes = 0;
                }
                Item::Atomic(a) => {
                    self.layout_block(a, w, Mode::Fit, 0.0, 0.0);
                    let ab = &self.boxes[a as usize];
                    let mw = ab.m[3] + ab.border_w() + ab.m[1];
                    let mh = ab.m[0] + ab.border_h() + ab.m[2];
                    let asc = match ab.baseline {
                        Some(bl) if matches!(ab.kind, Kind::Block | Kind::Flex | Kind::Grid) => ab.m[0] + ab.b[0] + ab.p[0] + bl,
                        _ => mh,
                    };
                    let mleft = ab.m[3];
                    let mtop = ab.m[0];
                    self.begin_line(&mut lb, w, cx, cy, mw);
                    if lb.has_content && lb.x + mw > lb.width + 0.01 {
                        self.end_line(id, &mut lb, false, w, cx, cy);
                        self.begin_line(&mut lb, w, cx, cy, mw);
                    }
                    lb.frags.push(Frag { x: lb.x + mleft, y: asc - mtop, kind: FragKind::Atomic(a) });
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
                    let t = &text[start as usize..end as usize];
                    let bytes = t.as_bytes();
                    let n = bytes.len();
                    let mut i = 0;
                    while i < n {
                        if bytes[i] == b'\n' {
                            self.begin_line(&mut lb, w, cx, cy, 0.0);
                            lb.asc = lb.asc.max(asc);
                            lb.desc = lb.desc.max(desc);
                            self.end_line(id, &mut lb, true, w, cx, cy);
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
                        // Break opportunities inside the run (hyphens, CJK, ...).
                        let mut cuts = if wraps && we > ws { word_breaks(&t[ws..we], st.word_break) } else { Vec::new() };
                        cuts.push(we - ws);
                        let mut ps = ws;
                        for (ci, &cut) in cuts.iter().enumerate() {
                            let pe = ws + cut;
                            let last = ci + 1 == cuts.len();
                            let (sp_bytes, sp_w) = if last { (se - we, (se - we) as f32 * space_w) } else { (0, 0.0) };
                            let seg_end = if last { se } else { pe };
                            let word_w = self.f.measure(face, fs, &t[ps..pe]);
                            self.begin_line(&mut lb, w, cx, cy, word_w);
                            if wraps && lb.has_content && pe > ps && lb.x + word_w > lb.width + 0.01 {
                                self.end_line(id, &mut lb, false, w, cx, cy);
                                self.begin_line(&mut lb, w, cx, cy, word_w);
                            }
                            if wraps && st.wrap_anywhere && word_w > lb.width - lb.x + 0.01 && pe > ps {
                                // overflow-wrap: split the run where it overflows.
                                let mut cs = ps;
                                let mut acc = 0.0;
                                for (off, ch) in t[ps..pe].char_indices() {
                                    let at = ps + off;
                                    let cw = self.f.measure(face, fs, &t[at..at + ch.len_utf8()]);
                                    if acc + cw > lb.width - lb.x + 0.01 && at > cs {
                                        lb.add_text(start + cs as u32, start + at as u32, acc, 0.0, 0, node, link, bg);
                                        lb.asc = lb.asc.max(asc);
                                        lb.desc = lb.desc.max(desc);
                                        self.end_line(id, &mut lb, false, w, cx, cy);
                                        self.begin_line(&mut lb, w, cx, cy, cw);
                                        cs = at;
                                        acc = 0.0;
                                    }
                                    acc += cw;
                                }
                                lb.add_text(start + cs as u32, start + seg_end as u32, acc, sp_w, sp_bytes as u32, node, link, bg);
                            } else {
                                lb.add_text(start + ps as u32, start + seg_end as u32, word_w, sp_w, sp_bytes as u32, node, link, bg);
                            }
                            lb.asc = lb.asc.max(asc);
                            lb.desc = lb.desc.max(desc);
                            ps = pe;
                        }
                    }
                }
            }
        }
        self.end_line(id, &mut lb, false, w, cx, cy);
        for &(a, x, y) in &lb.atomics {
            let b = &mut self.boxes[a as usize];
            b.x = x;
            b.y = y;
        }
        let bx = &mut self.boxes[id as usize];
        bx.items = items;
        bx.frags = lb.frags;
        bx.baseline = lb.baseline;
        // -webkit-line-clamp: the box is as tall as its first N lines.
        let clamp = cst.line_clamp as usize;
        if clamp > 0 && lb.line_bottoms.len() > clamp {
            return lb.line_bottoms[clamp - 1];
        }
        lb.y
    }

    // ---------------------------------------------------------------- flex

    fn layout_flex(&mut self, id: u32, w: f32) -> f32 {
        let st = self.st(id);
        let row = matches!(st.flex_dir, FlexDir::Row | FlexDir::RowRev);
        let rev = matches!(st.flex_dir, FlexDir::RowRev | FlexDir::ColRev);
        let mut items = Vec::new();
        for c in self.boxes[id as usize].children.clone() {
            if self.boxes[c as usize].abs {
                self.boxes[c as usize].static_pos = (0.0, 0.0);
            } else {
                items.push(c);
            }
        }
        items.sort_by_key(|&c| self.st(c).order);
        let gap_main = if row { st.col_gap.px(w) } else { st.row_gap.px(w) };
        let gap_cross = if row { st.row_gap.px(w) } else { st.col_gap.px(w) };
        let cb = &self.boxes[id as usize];
        let pb_v = cb.p[0] + cb.p[2] + cb.b[0] + cb.b[2];
        // (layout_block made this container's definite height the current one)
        let _ = pb_v;
        let spec_h = self.avail_h.filter(|_| !self.boxes[id as usize].anon);
        let main_size = if row { Some(w) } else { spec_h };
        let cross_size = if row { spec_h } else { Some(w) };

        struct Fi {
            id: u32,
            base: f32,
            hypo: f32,
            min: f32,
            max: f32,
            size: f32,
            margins: f32,
            frozen: bool,
            grow: f32,
            shrink: f32,
        }
        let mut fis: Vec<Fi> = Vec::with_capacity(items.len());
        for &c in &items {
            self.resolve_edges(c, w);
            let cs = self.st(c);
            let canon = self.boxes[c as usize].anon;
            let b = &self.boxes[c as usize];
            let (m, bw, pd) = (b.m, b.b, b.p);
            let pbm = if row { pd[1] + pd[3] + bw[1] + bw[3] } else { pd[0] + pd[2] + bw[0] + bw[2] };
            let margins = if row { m[1] + m[3] } else { m[0] + m[2] };
            let adj = if cs.border_box { 0.0 } else { pbm };
            let main_prop = if canon { Len::Auto } else if row { cs.width } else { cs.height };
            let definite = |l: Len| match l {
                Len::Auto => false,
                Len::Px(_) => true,
                _ => main_size.is_some(),
            };
            if !row {
                // Column items get their width now; their height is the content size.
                let al = cs.align_self.unwrap_or(st.align_items);
                let mode = if al == Al::Stretch && (canon || cs.width.is_auto()) {
                    Mode::Forced((w - (m[1] + m[3])).max(0.0))
                } else {
                    Mode::Fit
                };
                self.layout_block(c, w, mode, 0.0, 0.0);
            }
            let content = if row {
                self.intrinsic_outer(c).1 - (m[1] + m[3])
            } else {
                self.boxes[c as usize].border_h()
            };
            let base = if !canon && !cs.basis.is_auto() && definite(cs.basis) {
                cs.basis.px(main_size.unwrap_or(0.0)) + adj
            } else if definite(main_prop) {
                main_prop.px(main_size.unwrap_or(0.0)) + adj
            } else {
                content
            };
            let minp = if row { cs.min_w } else { cs.min_h };
            let maxp = if row { cs.max_w } else { cs.max_h };
            let min = if canon {
                0.0
            } else if minp != Len::Px(0.0) {
                minp.px(main_size.unwrap_or(0.0)) + adj
            } else if cs.overflow != Ovf::Visible {
                0.0
            } else if row {
                // min-width: auto = min-content (but no larger than a specified width)
                let mc = self.intrinsic_outer(c).0 - (m[1] + m[3]);
                if definite(main_prop) { mc.min(main_prop.px(w) + adj) } else { mc }
            } else {
                0.0
            };
            let max = if canon || maxp.is_auto() { f32::INFINITY } else { maxp.px(main_size.unwrap_or(0.0)) + adj };
            let hypo = base.min(max).max(min);
            fis.push(Fi { id: c, base, hypo, min, max, size: hypo, margins, frozen: false, grow: if canon { 0.0 } else { cs.grow }, shrink: if canon { 1.0 } else { cs.shrink } });
        }

        // Break into lines.
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let mut start = 0;
        if st.wrap && main_size.is_some() {
            let ms = main_size.unwrap();
            let mut used = 0.0;
            for i in 0..fis.len() {
                let outer = fis[i].hypo + fis[i].margins;
                if i > start && used + gap_main + outer > ms + 0.01 {
                    lines.push((start, i));
                    start = i;
                    used = outer;
                } else {
                    used += if i > start { gap_main + outer } else { outer };
                }
            }
        }
        if start < fis.len() || lines.is_empty() {
            lines.push((start, fis.len()));
        }

        // Resolve flexible lengths.
        if let Some(ms) = main_size {
            for &(s, e) in &lines {
                let line = &mut fis[s..e];
                let gaps = gap_main * (line.len().max(1) - 1) as f32;
                let used: f32 = line.iter().map(|f| f.hypo + f.margins).sum::<f32>() + gaps;
                let grow = used < ms;
                for f in line.iter_mut() {
                    f.frozen = (grow && (f.grow == 0.0 || f.base > f.hypo)) || (!grow && (f.shrink == 0.0 || f.base < f.hypo));
                    f.size = if f.frozen { f.hypo } else { f.base };
                }
                for _ in 0..=line.len() {
                    if line.iter().all(|f| f.frozen) {
                        break;
                    }
                    let taken: f32 = line.iter().map(|f| f.margins + if f.frozen { f.size } else { f.base }).sum::<f32>() + gaps;
                    let mut free = ms - taken;
                    let mut violation = 0.0;
                    let unfrozen = |f: &&mut Fi| !f.frozen;
                    if grow {
                        let sum: f32 = line.iter_mut().filter(unfrozen).map(|f| f.grow).sum();
                        if sum < 1.0 {
                            free *= sum;
                        }
                        for f in line.iter_mut().filter(|f| !f.frozen) {
                            let target = f.base + if sum > 0.0 { free * f.grow / sum } else { 0.0 };
                            f.size = target.min(f.max).max(f.min);
                            violation += f.size - target;
                        }
                    } else {
                        let sum: f32 = line.iter_mut().filter(unfrozen).map(|f| f.shrink * f.base).sum();
                        for f in line.iter_mut().filter(|f| !f.frozen) {
                            let target = f.base + if sum > 0.0 { free * f.shrink * f.base / sum } else { 0.0 };
                            f.size = target.min(f.max).max(f.min);
                            violation += f.size - target;
                        }
                    }
                    if violation.abs() < 0.01 {
                        break;
                    }
                    for f in line.iter_mut().filter(|f| !f.frozen) {
                        let clamped_min = f.size <= f.min + 0.001 && f.min > 0.0;
                        let clamped_max = f.size >= f.max - 0.001;
                        if (violation > 0.0 && clamped_min) || (violation < 0.0 && clamped_max) {
                            f.frozen = true;
                        }
                    }
                }
            }
        }

        // Cross sizes.
        let mut cross: Vec<f32> = vec![0.0; fis.len()];
        for (i, f) in fis.iter().enumerate() {
            let c = f.id;
            if row {
                self.layout_block(c, w, Mode::Forced(f.size.max(0.0)), 0.0, 0.0);
                let b = &self.boxes[c as usize];
                cross[i] = b.border_h() + b.m[0] + b.m[2];
            } else {
                let b = &mut self.boxes[c as usize];
                if main_size.is_some() {
                    let pbv = b.p[0] + b.p[2] + b.b[0] + b.b[2];
                    b.h = (f.size - pbv).max(0.0);
                }
                cross[i] = b.border_w() + b.m[1] + b.m[3];
            }
        }
        let mut line_cross: Vec<f32> = lines.iter().map(|&(s, e)| cross[s..e].iter().copied().fold(0.0, f32::max)).collect();
        if let Some(cs) = cross_size {
            if lines.len() == 1 {
                line_cross[0] = cs;
            } else if st.align_content == Justify::Stretch {
                let total: f32 = line_cross.iter().sum::<f32>() + gap_cross * (lines.len() - 1) as f32;
                if cs > total {
                    let extra = (cs - total) / lines.len() as f32;
                    line_cross.iter_mut().for_each(|c| *c += extra);
                }
            }
        }

        // Position.
        let mut cross_pos = 0.0;
        let mut main_extent = 0.0f32;
        let mut baseline = None;
        for (li, &(s, e)) in lines.iter().enumerate() {
            let n = e - s;
            let lc = line_cross[li];
            let used: f32 = fis[s..e].iter().map(|f| f.size + f.margins).sum::<f32>() + gap_main * (n.max(1) - 1) as f32;
            let avail = main_size.unwrap_or(used);
            let mut free = avail - used;
            // Auto margins in the main axis absorb free space first.
            let mut autos = 0;
            for f in &fis[s..e] {
                let cs = self.st(f.id);
                if !self.boxes[f.id as usize].anon {
                    let (a, b) = if row { (cs.margin[3], cs.margin[1]) } else { (cs.margin[0], cs.margin[2]) };
                    autos += a.is_auto() as usize + b.is_auto() as usize;
                }
            }
            let auto_share = if autos > 0 && free > 0.0 { free / autos as f32 } else { 0.0 };
            if autos > 0 && free > 0.0 {
                free = 0.0;
            }
            let (mut pos, mut between) = (0.0, gap_main);
            match st.justify {
                Justify::End => pos = free,
                Justify::Center => pos = free / 2.0,
                Justify::Between if n > 1 && free > 0.0 => between += free / (n - 1) as f32,
                Justify::Around if free > 0.0 => {
                    pos = free / (2 * n) as f32;
                    between += free / n as f32;
                }
                Justify::Evenly if free > 0.0 => {
                    pos = free / (n + 1) as f32;
                    between += free / (n + 1) as f32;
                }
                _ => {}
            }
            for (k, f) in fis[s..e].iter().enumerate() {
                let c = f.id;
                let cs = self.st(c);
                let canon = self.boxes[c as usize].anon;
                let (ma, mb) = if canon { (false, false) } else if row { (cs.margin[3].is_auto(), cs.margin[1].is_auto()) } else { (cs.margin[0].is_auto(), cs.margin[2].is_auto()) };
                if ma {
                    pos += auto_share;
                }
                let al = if canon { st.align_items } else { cs.align_self.unwrap_or(st.align_items) };
                let cross_auto = canon || if row { cs.height.is_auto() } else { cs.width.is_auto() };
                let b = &mut self.boxes[c as usize];
                let outer_main = f.size + f.margins;
                let mut outer_cross = cross[s + k];
                if al == Al::Stretch && cross_auto && row {
                    let pbv = b.p[0] + b.p[2] + b.b[0] + b.b[2];
                    b.h = b.h.max(lc - b.m[0] - b.m[2] - pbv);
                    outer_cross = lc;
                }
                let coff = match al {
                    Al::End => lc - outer_cross,
                    Al::Center => (lc - outer_cross) / 2.0,
                    _ => 0.0,
                };
                let main_start = if rev { avail - pos - outer_main } else { pos };
                if row {
                    b.x = main_start + b.m[3];
                    b.y = cross_pos + coff + b.m[0];
                } else {
                    b.y = main_start + b.m[0];
                    b.x = cross_pos + coff + b.m[3];
                }
                if baseline.is_none() {
                    if let Some(bl) = b.baseline {
                        baseline = Some(b.y + b.b[0] + b.p[0] + bl);
                    }
                }
                pos += outer_main + between;
                if mb {
                    pos += auto_share;
                }
            }
            main_extent = main_extent.max(avail);
            cross_pos += lc + gap_cross;
        }
        self.boxes[id as usize].baseline = baseline;
        if row {
            (cross_pos - gap_cross).max(0.0)
        } else {
            main_extent
        }
    }

    // ---------------------------------------------------------------- grid

    fn expand_tracks(&self, tl: Option<&Rc<css::TrackList>>, avail: f32, gap: f32, fs: f32) -> Vec<Track> {
        let Some(tl) = tl else { return Vec::new() };
        let mut out = tl.tracks.clone();
        if let Some((pos, rep)) = &tl.auto_repeat {
            let size_of = |t: &Track| match (t.max, t.min) {
                (Breadth::Len(v), _) | (_, Breadth::Len(v)) => self.val_px(v, avail, fs),
                _ => 0.0,
            };
            let other: f32 = out.iter().map(size_of).sum::<f32>() + gap * out.len() as f32;
            let rep_size: f32 = rep.iter().map(size_of).sum::<f32>() + gap * rep.len() as f32;
            let count = if rep_size > 0.0 { ((avail - other + gap) / rep_size).floor().clamp(1.0, 200.0) as usize } else { 1 };
            for _ in 0..count {
                for (i, t) in rep.iter().enumerate() {
                    out.insert(pos + i, *t);
                }
            }
        }
        out
    }

    /// Size grid columns: fixed tracks, content-sized tracks, then fr tracks.
    fn size_columns(&mut self, tracks: &[Track], placed: &[(u32, usize, usize)], avail: f32, gap: f32, fs: f32) -> Vec<f32> {
        let n = tracks.len();
        let mut base = vec![0.0f32; n];
        let mut limit = vec![f32::INFINITY; n];
        let mut fr = vec![0.0f32; n];
        let mut cmin = vec![0.0f32; n];
        let mut cmax = vec![0.0f32; n];
        for &(c, s, span) in placed {
            let (mn, mx) = self.intrinsic_outer(c);
            if span == 1 {
                cmin[s] = cmin[s].max(mn);
                cmax[s] = cmax[s].max(mx);
            }
        }
        for (i, t) in tracks.iter().enumerate() {
            base[i] = match t.min {
                Breadth::Len(v) => self.val_px(v, avail, fs),
                Breadth::MaxContent => cmax[i],
                _ => cmin[i],
            };
            match t.max {
                Breadth::Len(v) => limit[i] = self.val_px(v, avail, fs).max(base[i]),
                Breadth::Fr(f) => fr[i] = f,
                Breadth::MinContent => limit[i] = cmin[i].max(base[i]),
                _ => limit[i] = cmax[i].max(base[i]),
            }
        }
        // Spanning items: grow the spanned tracks evenly to fit their minimum.
        for &(c, s, span) in placed {
            if span > 1 {
                let (mn, _) = self.intrinsic_outer(c);
                let cur: f32 = base[s..s + span].iter().sum::<f32>() + gap * (span - 1) as f32;
                if mn > cur {
                    let add = (mn - cur) / span as f32;
                    base[s..s + span].iter_mut().for_each(|b| *b += add);
                }
            }
        }
        let gaps = gap * (n.max(1) - 1) as f32;
        let mut free = avail - base.iter().sum::<f32>() - gaps;
        // Grow non-flexible tracks up to their limits.
        if free > 0.0 {
            let room: f32 = (0..n).filter(|&i| fr[i] == 0.0 && limit[i].is_finite()).map(|i| limit[i] - base[i]).sum();
            if room > 0.0 {
                let k = (free / room).min(1.0);
                for i in (0..n).filter(|&i| fr[i] == 0.0 && limit[i].is_finite()) {
                    base[i] += (limit[i] - base[i]) * k;
                }
                free -= room.min(free);
            }
        }
        let sum_fr: f32 = fr.iter().sum();
        if sum_fr > 0.0 {
            let mut flex: Vec<bool> = fr.iter().map(|&f| f > 0.0).collect();
            for _ in 0..n {
                let fixed: f32 = (0..n).filter(|&i| !flex[i]).map(|i| base[i]).sum();
                let total_fr: f32 = (0..n).filter(|&i| flex[i]).map(|i| fr[i]).sum();
                if total_fr <= 0.0 {
                    break;
                }
                let unit = ((avail - gaps - fixed) / total_fr).max(0.0);
                let mut changed = false;
                for i in 0..n {
                    if flex[i] && unit * fr[i] < base[i] {
                        flex[i] = false;
                        changed = true;
                    }
                }
                if !changed {
                    for i in 0..n {
                        if flex[i] {
                            base[i] = unit * fr[i];
                        }
                    }
                    break;
                }
            }
        } else if free > 0.0 {
            // justify-content: normal stretches auto tracks.
            let autos: Vec<usize> = (0..n).filter(|&i| tracks[i].max == Breadth::Auto).collect();
            if !autos.is_empty() {
                let add = free / autos.len() as f32;
                for i in autos {
                    base[i] += add;
                }
            }
        }
        base
    }

    fn layout_grid(&mut self, id: u32, w: f32) -> f32 {
        let st = self.st(id);
        let fs = st.font_size;
        let gap_c = st.col_gap.px(w);
        let gap_r = st.row_gap.px(w);
        let mut items = Vec::new();
        for c in self.boxes[id as usize].children.clone() {
            if self.boxes[c as usize].abs {
                self.boxes[c as usize].static_pos = (0.0, 0.0);
            } else {
                items.push(c);
            }
        }
        items.sort_by_key(|&c| self.st(c).order);

        let auto = Track { min: Breadth::Auto, max: Breadth::Auto };
        let mut cols = self.expand_tracks(st.grid_cols.as_ref(), w, gap_c, fs);
        let areas = st.grid_areas.clone();
        if let Some(a) = &areas {
            while cols.len() < a.cols {
                cols.push(auto);
            }
        }
        if cols.is_empty() {
            cols.push(auto);
        }
        let row_tracks = self.expand_tracks(st.grid_rows.as_ref(), 0.0, gap_r, fs);
        let auto_rows = self.expand_tracks(st.grid_auto_rows.as_ref(), 0.0, gap_r, fs);
        let explicit_cols = cols.len();

        // Placement.
        let resolve = |a: GLine, b: GLine, n: usize| -> (Option<usize>, usize) {
            let line = |l: i16| -> usize { if l > 0 { (l - 1) as usize } else { (n as i32 + 1 + l as i32).max(0) as usize } };
            match (a, b) {
                (GLine::Line(s), GLine::Line(e)) => {
                    let (s, e) = (line(s), line(e));
                    (Some(s.min(e)), (s.max(e) - s.min(e)).max(1))
                }
                (GLine::Line(s), GLine::Span(k)) => (Some(line(s)), k as usize),
                (GLine::Line(s), GLine::Auto) => (Some(line(s)), 1),
                (GLine::Span(k), GLine::Line(e)) => (Some(line(e).saturating_sub(k as usize)), k as usize),
                (GLine::Auto, GLine::Line(e)) => (Some(line(e).saturating_sub(1)), 1),
                (GLine::Span(k), _) | (_, GLine::Span(k)) => (None, k as usize),
                _ => (None, 1),
            }
        };
        let mut ncols = cols.len();
        let mut occ: Vec<Vec<bool>> = Vec::new();
        let mut placed: Vec<(u32, usize, usize, usize, usize)> = Vec::new(); // (id, col, cspan, row, rspan)
        let mut cursor = (0usize, 0usize);
        let free_at = |occ: &Vec<Vec<bool>>, r: usize, c: usize, rs: usize, cs: usize, ncols: usize| -> bool {
            if c + cs > ncols {
                return false;
            }
            (r..r + rs).all(|rr| occ.get(rr).is_none_or(|row| (c..c + cs).all(|cc| !row.get(cc).copied().unwrap_or(false))))
        };
        for &c in &items {
            let cs = self.st(c);
            let canon = self.boxes[c as usize].anon;
            let (gc, gr) = if canon { ([GLine::Auto; 2], [GLine::Auto; 2]) } else { (cs.gcol, cs.grow_) };
            let (mut col, mut cspan) = resolve(gc[0], gc[1], explicit_cols);
            let (mut row, mut rspan) = resolve(gr[0], gr[1], row_tracks.len());
            // grid-area: <name> from grid-template-areas
            if let (Some(a), Some(name), false) = (&areas, cs.garea, canon) {
                if let Some((_, [r0, c0, r1, c1])) = a.map.iter().find(|(n, _)| *n == name) {
                    (row, rspan, col, cspan) = (Some(*r0), r1 - r0, Some(*c0), c1 - c0);
                }
            }
            if let Some(cl) = col {
                if cl + cspan > ncols {
                    ncols = cl + cspan;
                }
            } else {
                cspan = cspan.min(ncols);
            }
            let (r, cl) = match (row, col) {
                (Some(r), Some(cl)) => (r, cl),
                (None, Some(cl)) => {
                    let mut r = 0;
                    while !free_at(&occ, r, cl, rspan, cspan, ncols) {
                        r += 1;
                    }
                    (r, cl)
                }
                (Some(r), None) => {
                    let mut cl = 0;
                    while cl + cspan <= ncols && !free_at(&occ, r, cl, rspan, cspan, ncols) {
                        cl += 1;
                    }
                    (r, cl.min(ncols - cspan))
                }
                (None, None) => {
                    loop {
                        if cursor.1 + cspan > ncols {
                            cursor = (cursor.0 + 1, 0);
                        }
                        if free_at(&occ, cursor.0, cursor.1, rspan, cspan, ncols) {
                            break;
                        }
                        cursor.1 += 1;
                    }
                    let p = cursor;
                    cursor.1 += cspan;
                    p
                }
            };
            for rr in r..r + rspan {
                if occ.len() <= rr {
                    occ.resize(rr + 1, Vec::new());
                }
                let row = &mut occ[rr];
                if row.len() < ncols {
                    row.resize(ncols, false);
                }
                for cc in cl..(cl + cspan).min(ncols) {
                    row[cc] = true;
                }
            }
            placed.push((c, cl, cspan, r, rspan));
        }
        while cols.len() < ncols {
            cols.push(auto);
        }
        let col_items: Vec<(u32, usize, usize)> = placed.iter().map(|&(c, cl, cs, _, _)| (c, cl, cs)).collect();
        let col_w = self.size_columns(&cols, &col_items, w, gap_c, fs);
        let mut col_x = vec![0.0f32; ncols + 1];
        for i in 0..ncols {
            col_x[i + 1] = col_x[i] + col_w[i] + gap_c;
        }

        // Lay out items in their column areas, then size the rows.
        let nrows = placed.iter().map(|&(_, _, _, r, rs)| r + rs).max().unwrap_or(0).max(row_tracks.len());
        let mut heights = vec![0.0f32; placed.len()];
        for (i, &(c, cl, cs, _, _)) in placed.iter().enumerate() {
            let area_w = col_x[cl + cs] - col_x[cl] - gap_c;
            let ist = self.st(c);
            let canon = self.boxes[c as usize].anon;
            let js = if canon { st.justify_items } else { ist.justify_self.unwrap_or(st.justify_items) };
            self.resolve_edges(c, area_w);
            let mode = if js == Al::Stretch && (canon || ist.width.is_auto()) {
                let b = &self.boxes[c as usize];
                Mode::Forced((area_w - b.m[1] - b.m[3]).max(0.0))
            } else {
                Mode::Fit
            };
            self.layout_block(c, area_w, mode, 0.0, 0.0);
            let b = &self.boxes[c as usize];
            heights[i] = b.border_h() + b.m[0] + b.m[2];
        }
        let track_for = |r: usize| -> Track {
            if r < row_tracks.len() {
                row_tracks[r]
            } else if !auto_rows.is_empty() {
                auto_rows[(r - row_tracks.len()) % auto_rows.len()]
            } else {
                auto
            }
        };
        let mut row_h = vec![0.0f32; nrows];
        for (r, h) in row_h.iter_mut().enumerate() {
            if let Breadth::Len(v) = track_for(r).min {
                *h = self.val_px(v, 0.0, fs);
            }
        }
        for (i, &(_, _, _, r, rs)) in placed.iter().enumerate() {
            if rs == 1 && !matches!(track_for(r).max, Breadth::Len(_)) {
                row_h[r] = row_h[r].max(heights[i]);
            }
        }
        for (i, &(_, _, _, r, rs)) in placed.iter().enumerate() {
            if rs > 1 {
                let cur: f32 = row_h[r..r + rs].iter().sum::<f32>() + gap_r * (rs - 1) as f32;
                if heights[i] > cur {
                    row_h[r + rs - 1] += heights[i] - cur;
                }
            }
        }
        let mut row_y = vec![0.0f32; nrows + 1];
        for r in 0..nrows {
            row_y[r + 1] = row_y[r] + row_h[r] + gap_r;
        }

        // Whole-grid alignment along the inline axis.
        let used = col_x[ncols] - gap_c;
        let x_shift = match st.justify {
            Justify::Center if used < w => (w - used) / 2.0,
            Justify::End if used < w => w - used,
            _ => 0.0,
        };

        let mut baseline = None;
        for &(c, cl, cs, r, rs) in &placed {
            let area_w = col_x[cl + cs] - col_x[cl] - gap_c;
            let area_h = row_y[r + rs] - row_y[r] - gap_r;
            let ist = self.st(c);
            let canon = self.boxes[c as usize].anon;
            let js = if canon { st.justify_items } else { ist.justify_self.unwrap_or(st.justify_items) };
            let al = if canon { st.align_items } else { ist.align_self.unwrap_or(st.align_items) };
            let b = &mut self.boxes[c as usize];
            if al == Al::Stretch && (canon || ist.height.is_auto()) {
                let pbv = b.p[0] + b.p[2] + b.b[0] + b.b[2];
                b.h = b.h.max(area_h - b.m[0] - b.m[2] - pbv);
            }
            let ow = b.border_w() + b.m[1] + b.m[3];
            let oh = b.border_h() + b.m[0] + b.m[2];
            let jx = match js {
                Al::Center => (area_w - ow) / 2.0,
                Al::End => area_w - ow,
                _ => 0.0,
            };
            let ay = match al {
                Al::Center => (area_h - oh) / 2.0,
                Al::End => area_h - oh,
                _ => 0.0,
            };
            b.x = x_shift + col_x[cl] + jx + b.m[3];
            b.y = row_y[r] + ay + b.m[0];
            if baseline.is_none() {
                if let Some(bl) = b.baseline {
                    baseline = Some(b.y + b.b[0] + b.p[0] + bl);
                }
            }
        }
        self.boxes[id as usize].baseline = baseline;
        (row_y[nrows] - gap_r).max(0.0)
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
        let total_w = match (mode, spec) {
            (Mode::Forced(_), _) => avail.max(sum_min + sp_total),
            (_, Some(s)) => s.max(sum_min + sp_total),
            (_, None) if any_pct && mode == Mode::Normal => avail.max(sum_min + sp_total),
            _ => (sum_max + sp_total).min(avail).max(sum_min + sp_total),
        };
        let widths = distribute(&mins, &maxs, &pcts, total_w - sp_total);

        let children = std::mem::take(&mut self.boxes[id as usize].children);
        let mut y = if n > 0 { spacing } else { 0.0 };
        self.bfcs.push(Bfc::default());
        for &r in &children {
            if self.boxes[r as usize].kind != Kind::Row {
                // caption
                self.layout_block(r, total_w, Mode::Normal, 0.0, y);
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
                self.layout_block(c, cw, Mode::Forced(cw), 0.0, 0.0);
                let cb = &mut self.boxes[c as usize];
                cb.x = x;
                cb.y = 0.0;
                x += cw + spacing;
                col += span;
                row_h = row_h.max(cb.border_h());
            }
            let rst = self.st(r);
            if !self.boxes[r as usize].anon {
                if let Some(h) = self.resolve_h(rst.height) {
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
        self.bfcs.pop();
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
        let h = match self.resolve_h(st.height) {
            Some(h) => y.max(h - p[0] - p[2] - b[0] - b[2]),
            None => y,
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
    pending_floats: Vec<u32>,
    line_start: usize,
    x: f32,
    y: f32,
    asc: f32,
    desc: f32,
    strut: (f32, f32),
    has_content: bool,
    /// The line's float-free band has been computed.
    started: bool,
    trailing_w: f32,
    trailing_bytes: u32,
    /// Left edge and width of the current line (narrowed by floats).
    left: f32,
    width: f32,
    align: Align,
    baseline: Option<f32>,
    /// Bottom of each finished line.
    line_bottoms: Vec<f32>,
    /// Decorated inline elements open at this point: (node, frag index).
    open: Vec<(NodeId, usize)>,
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FFFF)
}

/// Characters that may not start a line (closing punctuation, small kana).
fn no_break_before(c: char) -> bool {
    ")]}!?,.:;\u{3001}\u{3002}\u{FF0C}\u{FF0E}\u{FF09}\u{300D}\u{300F}\u{3011}\u{3009}\u{300B}\u{FF01}\u{FF1F}\u{FF1A}\u{FF1B}\u{30FC}\u{30FB}\u{3005}\u{3041}\u{3043}\u{3045}\u{3047}\u{3049}\u{3063}\u{3083}\u{3085}\u{3087}\u{30A1}\u{30A3}\u{30A5}\u{30A7}\u{30A9}\u{30C3}\u{30E3}\u{30E5}\u{30E7}".contains(c)
}

/// Characters that may not end a line (opening punctuation).
fn no_break_after(c: char) -> bool {
    "([{\u{FF08}\u{300C}\u{300E}\u{3010}\u{3008}\u{300A}".contains(c)
}

/// Byte offsets inside a space-free run where a line may break.
fn word_breaks(word: &str, word_break: u8) -> Vec<usize> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut before_prev: Option<char> = None;
    for (i, c) in word.char_indices() {
        if let Some(p) = prev {
            let ok = if word_break == 1 {
                !no_break_before(c)
            } else {
                let hyphen = matches!(p, '-' | '\u{2010}' | '\u{2012}' | '\u{2013}' | '\u{2014}') && before_prev.is_some_and(|b| b.is_alphanumeric()) && !c.is_ascii_digit() && c != '-';
                let soft = matches!(p, '\u{200B}' | '\u{AD}');
                let cjk = word_break != 2 && (is_cjk(p) || is_cjk(c)) && !no_break_before(c) && !no_break_after(p);
                hyphen || soft || cjk
            };
            if ok {
                out.push(i);
            }
        }
        before_prev = prev;
        prev = Some(c);
    }
    out
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
            self.frags.push(Frag { x: self.x, y: 0.0, kind: FragKind::Text { start: s, end: e, node, link, bg, w: word_w + sp_w } });
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
        self.started = false;
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
        // Inline elements that continue on the next line end here.
        for &(_, idx) in &self.open {
            let x0 = self.frags[idx].x;
            if let FragKind::InlineBox { w, .. } = &mut self.frags[idx].kind {
                *w = (line_w - x0).max(0.0);
            }
        }
        let off = self.left
            + match self.align {
                Align::Left => 0.0,
                Align::Center => ((self.width - line_w) / 2.0).max(0.0),
                Align::Right => (self.width - line_w).max(0.0),
            };
        let baseline = self.asc;
        for f in &mut self.frags[self.line_start..] {
            f.x += off;
            match f.kind {
                FragKind::Text { .. } | FragKind::InlineBox { .. } => f.y = self.y + baseline,
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
        self.line_bottoms.push(self.y);
        self.x = 0.0;
        self.line_start = self.frags.len();
        for o in &mut self.open {
            let n = o.0;
            o.1 = self.frags.len();
            self.frags.push(Frag { x: 0.0, y: 0.0, kind: FragKind::InlineBox { node: n, w: 0.0, first: false, last: false } });
        }
        self.asc = self.strut.0;
        self.desc = self.strut.1;
        self.has_content = false;
        self.trailing_w = 0.0;
        self.trailing_bytes = 0;
    }
}
