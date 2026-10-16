//! Display list generation and software rasterization.
//!
//! The display list is a flat Vec of drawing commands in page coordinates
//! (plus a second list for `position: fixed` content, in viewport
//! coordinates). Painting a frame: cull to the viewport, make sure every
//! needed glyph is in the cache (single-threaded), then split the framebuffer
//! into horizontal bands and rasterize them in parallel with no locking.

use crate::css;
use crate::dom::Document;
use crate::engine::Resources;
use crate::fonts::{fonts, glyph_key, Fonts, GlyphCache};
use crate::image::Image;
use std::sync::Arc;
use crate::layout::*;
use crate::style::*;

#[derive(Clone, Copy, Debug)]
pub struct RingData {
    /// Outer rect (x, y, w, h) and corner radii.
    pub o: [f32; 4],
    pub or: [f32; 4],
    /// Inner (padding-box) rect and radii.
    pub i: [f32; 4],
    pub ir: [f32; 4],
    pub c: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct ShadowData {
    /// Shadow shape (x, y, w, h) and radii.
    pub s: [f32; 4],
    pub r: [f32; 4],
    pub blur: f32,
    pub c: u32,
    /// The element's border box: outer shadows are not drawn under it.
    pub hole: [f32; 4],
    pub hr: [f32; 4],
}

pub struct ImgData {
    pub img: Arc<Image>,
    /// Where one copy of the image goes (x, y, w, h).
    pub dst: [f32; 4],
    /// Drawing is limited to this rect.
    pub clip: [f32; 4],
    /// Repeat horizontally / vertically.
    pub tile: (bool, bool),
    pub alpha: u32,
    /// Corner radii of the clip rect.
    pub radius: [f32; 4],
}

pub struct GradData {
    pub rect: [f32; 4],
    pub clip: [f32; 4],
    pub radial: bool,
    pub angle: f32,
    /// repeating-*-gradient: the pattern repeats every this fraction of the line.
    pub period: Option<f32>,
    pub lut: Box<[u32; 256]>,
    pub radius: [f32; 4],
}

pub enum DItem {
    Rect { x: f32, y: f32, w: f32, h: f32, c: u32 },
    /// `src` 0 = layout text buffer, 1 = display-list owned text (markers).
    Text { x: f32, y: f32, start: u32, end: u32, src: u8, face: u8, size: f32, color: u32 },
    /// Rounded rect: (x, y, w, h), radii, colour.
    RRect(Box<([f32; 4], [f32; 4], u32)>),
    Ring(Box<RingData>),
    Shadow(Box<ShadowData>),
    Image(Box<ImgData>),
    Gradient(Box<GradData>),
    Clip { x: f32, y: f32, w: f32, h: f32 },
    Unclip,
    /// A transformed element: its items are rasterized offscreen, then
    /// drawn through the matrix.
    Layer(Box<LayerData>),
}

pub struct LayerData {
    pub items: Vec<DItem>,
    /// Untransformed bounds of the items (page coordinates).
    pub bounds: [f32; 4],
    /// Page-to-page affine matrix [a, b, c, d, e, f].
    pub m: [f32; 6],
    /// Bounds after transformation.
    pub tbounds: [f32; 4],
    /// Group opacity.
    pub alpha: f32,
}

/// a · b (apply b first).
fn mat_mul(a: [f32; 6], b: [f32; 6]) -> [f32; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// The transform of a box with border box (x, y, w, h), about its origin.
fn box_matrix(st: &Style, x: f32, y: f32, w: f32, h: f32) -> Option<[f32; 6]> {
    let lists = [&st.x_translate, &st.x_rotate, &st.x_scale, &st.xform];
    if lists.iter().all(|l| l.is_none()) {
        return None;
    }
    let mut m = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    for op in lists.iter().filter_map(|l| l.as_ref()).flat_map(|l| l.iter()) {
        let om = match *op {
            Tf::Translate(tx, ty) => [1.0, 0.0, 0.0, 1.0, tx.px(w), ty.px(h)],
            Tf::M(mm) => mm,
        };
        m = mat_mul(m, om);
    }
    let (ox, oy) = (x + st.tf_origin[0].px(w), y + st.tf_origin[1].px(h));
    let m = mat_mul(mat_mul([1.0, 0.0, 0.0, 1.0, ox, oy], m), [1.0, 0.0, 0.0, 1.0, -ox, -oy]);
    let identity = (m[0] - 1.0).abs() < 1e-6 && m[1].abs() < 1e-6 && m[2].abs() < 1e-6 && (m[3] - 1.0).abs() < 1e-6 && m[4].abs() < 1e-4 && m[5].abs() < 1e-4;
    (!identity).then_some(m)
}

fn transform_rect(m: [f32; 6], r: [f32; 4]) -> [f32; 4] {
    let pts = [(r[0], r[1]), (r[0] + r[2], r[1]), (r[0], r[1] + r[3]), (r[0] + r[2], r[1] + r[3])];
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (x, y) in pts {
        let (tx, ty) = (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        x0 = x0.min(tx);
        y0 = y0.min(ty);
        x1 = x1.max(tx);
        y1 = y1.max(ty);
    }
    [x0, y0, x1 - x0, y1 - y0]
}

#[derive(Clone, Copy, Debug)]
pub struct LinkArea {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub link: u32,
}

pub struct DisplayList {
    pub items: Vec<DItem>,
    /// position: fixed content, in viewport coordinates.
    pub fixed: Vec<DItem>,
    pub links: Vec<LinkArea>,
    pub fixed_links: Vec<LinkArea>,
    pub extra: String,
    pub bg: u32,
}

impl DisplayList {
    /// Link under viewport point (vx, vy) when scrolled to `scroll`.
    pub fn link_at(&self, vx: f32, vy: f32, scroll: f32) -> Option<u32> {
        let hit = |list: &[LinkArea], x: f32, y: f32| {
            list.iter().rev().find(|a| x >= a.x && x < a.x + a.w && y >= a.y && y < a.y + a.h).map(|a| a.link)
        };
        hit(&self.fixed_links, vx, vy).or_else(|| hit(&self.links, vx, vy + scroll))
    }
}

struct Builder<'a> {
    tree: &'a LayoutTree,
    styles: &'a [Style],
    res: &'a Resources,
    f: &'static Fonts,
    dl: DisplayList,
    opacity: f32,
    in_fixed: bool,
    /// Positioned descendants collected per open stacking context.
    ctx: Vec<Vec<Deferred>>,
    /// Active overflow clip rects, and the depth at the current context root.
    clip_stack: Vec<[f32; 4]>,
    ctx_clip_base: usize,
    /// Paint the next box as a stacking context.
    force_ctx: bool,
    /// Record the display-list position after this box's decorations.
    mark_after_decor: Option<u32>,
    decor_mark: Option<usize>,
    /// This box's opacity is applied by its layer.
    group_opacity: Option<u32>,
}

pub fn build(tree: &LayoutTree, doc: &Document, styles: &[Style], res: &Resources) -> DisplayList {
    let html_bg = styles[doc.html as usize].bg;
    let body_bg = styles[doc.body as usize].bg;
    let bg = if html_bg >> 24 != 0 {
        html_bg
    } else if body_bg >> 24 != 0 {
        body_bg
    } else {
        0xFFFF_FFFF
    };
    let mut b = Builder {
        tree,
        styles,
        res,
        f: fonts(),
        dl: DisplayList {
            items: Vec::with_capacity(tree.boxes.len() * 2),
            fixed: Vec::new(),
            links: Vec::new(),
            fixed_links: Vec::new(),
            extra: String::new(),
            bg: bg | 0xFF00_0000,
        },
        opacity: 1.0,
        in_fixed: false,
        ctx: Vec::new(),
        clip_stack: Vec::new(),
        ctx_clip_base: 0,
        force_ctx: false,
        mark_after_decor: None,
        decor_mark: None,
        group_opacity: None,
    };
    b.paint_box(tree.root, 0.0, 0.0);
    b.dl
}

fn radii(st: &Style, w: f32, h: f32) -> [f32; 4] {
    let lim = w.min(h) / 2.0;
    let mut r = [0.0; 4];
    for i in 0..4 {
        r[i] = st.radius[i].px(w).clamp(0.0, lim);
    }
    r
}

impl Builder<'_> {
    fn push(&mut self, it: DItem) {
        if self.in_fixed {
            self.dl.fixed.push(it);
        } else {
            self.dl.items.push(it);
        }
    }

    fn fade(&self, c: u32) -> u32 {
        if self.opacity >= 1.0 {
            c
        } else {
            ((((c >> 24) as f32 * self.opacity) as u32) << 24) | (c & 0xFF_FFFF)
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        let c = self.fade(c);
        if c >> 24 != 0 && w > 0.0 && h > 0.0 {
            self.push(DItem::Rect { x, y, w, h, c });
        }
    }

    fn link(&mut self, x: f32, y: f32, w: f32, h: f32, link: u32) {
        let a = LinkArea { x, y, w, h, link };
        if self.in_fixed {
            self.dl.fixed_links.push(a);
        } else {
            self.dl.links.push(a);
        }
    }

    fn decorations(&mut self, b: &LBox, st: &Style, bx: f32, by: f32) {
        let (bw, bh) = (b.border_w(), b.border_h());
        let r = radii(st, bw, bh);
        let rounded = r.iter().any(|&v| v > 0.0);
        if let Some(sh) = st.shadow {
            if !sh.inset && sh.color >> 24 != 0 {
                let s = [bx + sh.x - sh.spread, by + sh.y - sh.spread, bw + 2.0 * sh.spread, bh + 2.0 * sh.spread];
                let sr = r.map(|v| if v > 0.0 { (v + sh.spread).max(0.0) } else { 0.0 });
                let c = self.fade(sh.color);
                self.push(DItem::Shadow(Box::new(ShadowData { s, r: sr, blur: sh.blur, c, hole: [bx, by, bw, bh], hr: r })));
            }
        }
        if rounded {
            let c = self.fade(st.bg);
            if c >> 24 != 0 {
                self.push(DItem::RRect(Box::new(([bx, by, bw, bh], r, c))));
            }
            self.background_image(b, st, bx, by);
            let bc = self.fade(st.bc[0]);
            if b.b.iter().any(|&v| v > 0.0) && bc >> 24 != 0 {
                let i = [bx + b.b[3], by + b.b[0], bw - b.b[1] - b.b[3], bh - b.b[0] - b.b[2]];
                let ir = [
                    (r[0] - b.b[3].max(b.b[0])).max(0.0),
                    (r[1] - b.b[1].max(b.b[0])).max(0.0),
                    (r[2] - b.b[1].max(b.b[2])).max(0.0),
                    (r[3] - b.b[3].max(b.b[2])).max(0.0),
                ];
                self.push(DItem::Ring(Box::new(RingData { o: [bx, by, bw, bh], or: r, i, ir, c: bc })));
            }
        } else {
            self.rect(bx, by, bw, bh, st.bg);
            self.background_image(b, st, bx, by);
            self.rect(bx, by, bw, b.b[0], st.bc[0]);
            self.rect(bx, by + bh - b.b[2], bw, b.b[2], st.bc[2]);
            self.rect(bx, by, b.b[3], bh, st.bc[3]);
            self.rect(bx + bw - b.b[1], by, b.b[1], bh, st.bc[1]);
        }
        if let Some(img) = &b.image {
            // <img> content, honouring object-fit.
            let (cx, cy) = (bx + b.b[3] + b.p[3], by + b.b[0] + b.p[0]);
            let (cw, ch) = (b.w, b.h);
            let (iw, ih) = (img.iw, img.ih);
            // SVGs keep their aspect ratio (preserveAspectRatio="xMidYMid meet").
            let fit = if img.vector && st.object_fit == 0 { 1 } else { st.object_fit };
            let s = match fit {
                1 => (cw / iw).min(ch / ih),
                2 => (cw / iw).max(ch / ih),
                3 => 1.0,
                4 => (cw / iw).min(ch / ih).min(1.0),
                _ => 0.0,
            };
            let dst = if fit == 0 { [cx, cy, cw, ch] } else { [cx + (cw - iw * s) / 2.0, cy + (ch - ih * s) / 2.0, iw * s, ih * s] };
            let alpha = (255.0 * self.opacity) as u32;
            let r = radii(st, b.border_w(), b.border_h());
            let radius = [0, 1, 2, 3].map(|i| (r[i] - b.b[3].max(b.b[0]) - b.p[3].max(b.p[0])).max(0.0));
            self.push(DItem::Image(Box::new(ImgData { img: img.clone(), dst, clip: [cx, cy, cw, ch], tile: (false, false), alpha, radius })));
        }
    }

    fn background_image(&mut self, b: &LBox, st: &Style, bx: f32, by: f32) {
        let Some(bi) = st.bg_image else { return };
        let (bw, bh) = (b.border_w(), b.border_h());
        let clip = [bx, by, bw, bh];
        let radius = radii(st, bw, bh);
        let (px, py) = (bx + b.b[3], by + b.b[0]);
        let (pw, ph) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
        if pw <= 0.0 || ph <= 0.0 {
            return;
        }
        let sized = |iw: f32, ih: f32| -> (f32, f32) {
            match st.bg_fit {
                1 => {
                    let s = (pw / iw).max(ph / ih);
                    (iw * s, ih * s)
                }
                2 => {
                    let s = (pw / iw).min(ph / ih);
                    (iw * s, ih * s)
                }
                _ => match (st.bg_size[0], st.bg_size[1]) {
                    (Len::Auto, Len::Auto) => (iw, ih),
                    (x, Len::Auto) => {
                        let w = x.px(pw);
                        (w, w * ih / iw)
                    }
                    (Len::Auto, y) => {
                        let h = y.px(ph);
                        (h * iw / ih, h)
                    }
                    (x, y) => (x.px(pw), y.px(ph)),
                },
            }
        };
        match css::bg_image(bi) {
            css::BgImage::Url(u) => {
                let Some(img) = self.res.url(&u) else { return };
                let (w, h) = sized(img.iw, img.ih);
                if w < 0.5 || h < 0.5 {
                    return;
                }
                let x = px + st.bg_pos[0].px(pw - w);
                let y = py + st.bg_pos[1].px(ph - h);
                let alpha = (255.0 * self.opacity) as u32;
                self.push(DItem::Image(Box::new(ImgData { img: img.clone(), dst: [x, y, w, h], clip, tile: st.bg_repeat, alpha, radius })));
            }
            css::BgImage::Gradient(g) => {
                let (w, h) = if st.bg_fit == 0 && st.bg_size[0] == Len::Auto && st.bg_size[1] == Len::Auto { (pw, ph) } else { sized(pw, ph) };
                let x = px + st.bg_pos[0].px(pw - w);
                let y = py + st.bg_pos[1].px(ph - h);
                let a = g.angle.to_radians();
                let len = if g.radial { (w * w + h * h).sqrt() / 2.0 } else { (w * a.sin()).abs() + (h * a.cos()).abs() };
                let (lut, period) = gradient_lut(&g, len.max(1.0), self.opacity);
                self.push(DItem::Gradient(Box::new(GradData { rect: [x, y, w, h], clip, radial: g.radial, angle: g.angle, period, lut, radius })));
            }
        }
    }

    /// Bounding box of an item (page coordinates).
    fn item_bbox(&self, it: &DItem) -> Option<[f32; 4]> {
        Some(match it {
            DItem::Rect { x, y, w, h, .. } => [*x, *y, *w, *h],
            DItem::RRect(d) => d.0,
            DItem::Ring(d) => d.o,
            DItem::Image(d) => d.clip,
            DItem::Gradient(d) => d.clip,
            DItem::Shadow(d) => {
                let e = d.blur * 1.5 + 1.0;
                [d.s[0] - e, d.s[1] - e, d.s[2] + 2.0 * e, d.s[3] + 2.0 * e]
            }
            DItem::Text { x, y, start, end, src, face, size, .. } => {
                let text = if *src == 0 { &self.tree.text[*start as usize..*end as usize] } else { &self.dl.extra[*start as usize..*end as usize] };
                let w = self.f.measure(*face, *size, text);
                [*x - 1.0, y - size * 1.2, w + 2.0, size * 1.6]
            }
            DItem::Layer(l) => l.tbounds,
            DItem::Clip { .. } | DItem::Unclip => return None,
        })
    }

    fn target(&mut self) -> &mut Vec<DItem> {
        if self.in_fixed { &mut self.dl.fixed } else { &mut self.dl.items }
    }

    fn paint_box(&mut self, id: u32, ox: f32, oy: f32) {
        let b = &self.tree.boxes[id as usize];
        let st = &self.styles[b.node as usize];
        if b.anon {
            return self.paint_box_inner(id, ox, oy);
        }
        let (bx, by) = (ox + b.x + b.rel.0, oy + b.y + b.rel.1);
        // (Translations are applied by layout.)
        let m = if st.has_transform() && st.pure_translate(b.border_w(), b.border_h()).is_none() { box_matrix(st, bx, by, b.border_w(), b.border_h()) } else { None };
        // Group opacity: the element and its content are composited as one.
        let group_alpha = st.opacity < 1.0 && st.opacity >= 0.004 && !st.hidden;
        if m.is_none() && !group_alpha {
            return self.paint_context_or_box(id, ox, oy);
        }
        let saved = std::mem::take(self.target());
        let saved_opacity = self.opacity;
        if group_alpha {
            self.opacity = 1.0;
            self.group_opacity = Some(id);
        }
        self.paint_context_or_box(id, ox, oy);
        self.opacity = saved_opacity;
        let items = std::mem::replace(self.target(), saved);
        let mut bb: Option<[f32; 4]> = None;
        for it in &items {
            if let Some(r) = self.item_bbox(it) {
                bb = Some(match bb {
                    None => r,
                    Some(u) => {
                        let (x0, y0) = (u[0].min(r[0]), u[1].min(r[1]));
                        let (x1, y1) = ((u[0] + u[2]).max(r[0] + r[2]), (u[1] + u[3]).max(r[1] + r[3]));
                        [x0, y0, x1 - x0, y1 - y0]
                    }
                });
            }
        }
        let Some(bounds) = bb else { return };
        let m = m.unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let tbounds = transform_rect(m, bounds);
        let alpha = if group_alpha { st.opacity * self.opacity } else { 1.0 };
        self.push(DItem::Layer(Box::new(LayerData { items, bounds, m, tbounds, alpha })));
    }

    /// Does this box start a stacking context (its positioned descendants
    /// are painted with it)?
    fn creates_context(&self, id: u32) -> bool {
        let b = &self.tree.boxes[id as usize];
        if id == self.tree.root {
            return true;
        }
        if b.anon {
            return false;
        }
        let st = &self.styles[b.node as usize];
        (st.position != Pos::Static && st.z.is_some()) || st.position == Pos::Fixed || st.opacity < 1.0 || st.has_transform()
    }

    fn is_positioned(&self, id: u32) -> bool {
        let b = &self.tree.boxes[id as usize];
        !b.anon && (b.abs || b.fixed || self.styles[b.node as usize].position != Pos::Static)
    }

    fn z(&self, id: u32) -> i32 {
        self.styles[self.tree.boxes[id as usize].node as usize].z.unwrap_or(0)
    }

    /// Collect a positioned descendant for the current stacking context.
    fn defer(&mut self, id: u32, ox: f32, oy: f32) {
        let fixed = self.tree.boxes[id as usize].fixed;
        let clips = if fixed { Vec::new() } else { self.clip_stack[self.ctx_clip_base..].to_vec() };
        let z = self.z(id);
        let order = self.ctx.last().map_or(0, |l| l.len());
        let d = Deferred { z, id, ox, oy, fixed, clips, order };
        match self.ctx.last_mut() {
            Some(l) => l.push(d),
            None => self.paint_queue(vec![d]),
        }
    }

    /// Paint a collected positioned box. A stacking context paints with all
    /// its positioned descendants; a `z-index: auto` box returns the ones it
    /// collected, which belong to the enclosing context.
    fn paint_deferred(&mut self, d: &Deferred) -> Vec<Deferred> {
        let saved_fixed = self.in_fixed;
        if d.fixed {
            self.in_fixed = true;
        }
        let saved_base = self.ctx_clip_base;
        let own_ctx = self.creates_context(d.id);
        if !own_ctx {
            // Descendants record the clips from here (including d's own).
            self.ctx_clip_base = self.clip_stack.len();
            self.ctx.push(Vec::new());
        }
        for c in &d.clips {
            self.push(DItem::Clip { x: c[0], y: c[1], w: c[2], h: c[3] });
            self.clip_stack.push(*c);
        }
        self.force_ctx = own_ctx;
        self.paint_box(d.id, d.ox, d.oy);
        self.force_ctx = false;
        for _ in &d.clips {
            self.push(DItem::Unclip);
            self.clip_stack.pop();
        }
        self.in_fixed = saved_fixed;
        if own_ctx {
            return Vec::new();
        }
        self.ctx_clip_base = saved_base;
        let mut extra = self.ctx.pop().unwrap_or_default();
        // Carry the fixed flag to descendants of fixed boxes.
        if d.fixed {
            for e in &mut extra {
                e.fixed = true;
            }
        }
        extra
    }

    /// Paint a sorted queue of positioned boxes, merging in the positioned
    /// descendants of `z-index: auto` boxes as they are found.
    fn paint_queue(&mut self, mut queue: Vec<Deferred>) {
        let mut i = 0;
        while i < queue.len() {
            let d = std::mem::replace(&mut queue[i], Deferred { z: 0, id: 0, ox: 0.0, oy: 0.0, fixed: false, clips: Vec::new(), order: 0 });
            let mut extra = self.paint_deferred(&d);
            i += 1;
            if extra.is_empty() {
                continue;
            }
            extra.sort_by_key(|e| (e.z, e.order));
            // Descendants come before later boxes of the same z (tree order).
            let tail = queue.split_off(i);
            let mut merged = Vec::with_capacity(extra.len() + tail.len());
            let (mut a, mut b) = (extra.into_iter().peekable(), tail.into_iter().peekable());
            loop {
                let take_a = match (a.peek(), b.peek()) {
                    (Some(x), Some(y)) => x.z <= y.z,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                merged.push(if take_a { a.next().unwrap() } else { b.next().unwrap() });
            }
            queue.extend(merged);
        }
    }

    fn paint_context_or_box(&mut self, id: u32, ox: f32, oy: f32) {
        if !std::mem::take(&mut self.force_ctx) && !self.creates_context(id) {
            return self.paint_box_inner(id, ox, oy);
        }
        // Stacking context: own background, negative z, in-flow content,
        // z auto / 0 in tree order, positive z.
        self.ctx.push(Vec::new());
        let saved_base = std::mem::replace(&mut self.ctx_clip_base, self.clip_stack.len());
        self.mark_after_decor = Some(id);
        self.paint_box_inner(id, ox, oy);
        let mark = std::mem::take(&mut self.decor_mark);
        let mut list = self.ctx.pop().unwrap_or_default();
        self.ctx_clip_base = saved_base;
        list.sort_by_key(|d| (d.z, d.order));
        let (neg, rest): (Vec<Deferred>, Vec<Deferred>) = list.into_iter().partition(|d| d.z < 0);
        if !neg.is_empty() {
            // Below the in-flow content, above the root's own background.
            let at = mark.unwrap_or(0).min(self.target().len());
            let tail = self.target().split_off(at);
            self.paint_queue(neg);
            self.target().extend(tail);
        }
        self.paint_queue(rest);
    }

    fn paint_box_inner(&mut self, id: u32, ox: f32, oy: f32) {
        let tree = self.tree;
        let b = &tree.boxes[id as usize];
        let st = &self.styles[b.node as usize];
        let bx = ox + b.x + b.rel.0;
        let by = oy + b.y + b.rel.1;
        if !b.anon && st.hidden {
            if self.mark_after_decor == Some(id) {
                self.mark_after_decor = None;
                self.decor_mark = Some(self.target().len());
            }
            return;
        }
        let saved_opacity = self.opacity;
        if !b.anon {
            if self.group_opacity == Some(id) {
                self.group_opacity = None;
            } else {
                self.opacity *= st.opacity;
            }
            if self.opacity < 0.004 {
                self.opacity = saved_opacity;
                if self.mark_after_decor == Some(id) {
                    self.mark_after_decor = None;
                    self.decor_mark = Some(self.target().len());
                }
                return;
            }
            self.decorations(b, st, bx, by);
            if b.link != NO_LINK {
                self.link(bx, by, b.border_w(), b.border_h(), b.link);
            }
        }
        if self.mark_after_decor == Some(id) {
            self.mark_after_decor = None;
            self.decor_mark = Some(self.target().len());
        }
        let (px, py) = (bx + b.b[3], by + b.b[0]);
        let cx = px + b.p[3];
        let cy = py + b.p[0] + b.cell_off;
        let clipped = !b.anon && st.overflow == Ovf::Hidden;
        if clipped {
            let (w, h) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
            self.push(DItem::Clip { x: px, y: py, w, h });
            self.clip_stack.push([px, py, w, h]);
        }

        if let Some(m) = &b.marker {
            let face = self.f.pick_style(st);
            let fs = st.font_size;
            let (asc, _, _) = self.f.metrics(face);
            let w = self.f.measure(face, fs, m);
            let base = cy + b.baseline.unwrap_or(asc * fs);
            let start = self.dl.extra.len() as u32;
            self.dl.extra.push_str(m);
            let end = self.dl.extra.len() as u32;
            let color = self.fade(st.color);
            self.push(DItem::Text { x: cx - w - fs * 0.5, y: base, start, end, src: 1, face, size: fs, color });
        }

        // In-flow blocks, floats, inline content; positioned descendants go
        // to the enclosing stacking context.
        for &a in &b.abs_kids {
            if tree.boxes[a as usize].fixed {
                self.defer(a, 0.0, 0.0);
            } else {
                self.defer(a, px, py);
            }
        }
        for &c in &b.children {
            if tree.boxes[c as usize].abs {
                continue;
            }
            if self.is_positioned(c) {
                self.defer(c, cx, cy);
            } else {
                self.paint_box(c, cx, cy);
            }
        }
        for &f in &b.floats {
            if self.is_positioned(f) {
                self.defer(f, cx, cy);
            } else {
                self.paint_box(f, cx, cy);
            }
        }

        for f in &b.frags {
            match f.kind {
                FragKind::Text { start, end, node, link, bg, w } => {
                    let ts = &self.styles[node as usize];
                    if ts.hidden {
                        continue;
                    }
                    let face = self.f.pick_style(ts);
                    let fs = ts.font_size;
                    let (a, d, _) = self.f.metrics(face);
                    let (x, y) = (cx + f.x, cy + f.y);
                    if bg >> 24 != 0 {
                        self.rect(x, y - a * fs, w, (a + d) * fs, bg);
                    }
                    let color = self.fade(ts.color);
                    if end > start {
                        self.push(DItem::Text { x, y, start, end, src: 0, face, size: fs, color });
                    }
                    let th = (fs / 14.0).round().max(1.0);
                    if ts.underline {
                        self.rect(x, y + (fs * 0.11).round(), w, th, ts.color);
                    }
                    if ts.strike {
                        self.rect(x, y - (fs * 0.3).round(), w, th, ts.color);
                    }
                    if link != NO_LINK {
                        self.link(x, y - a * fs, w, (a + d) * fs, link);
                    }
                }
                FragKind::Atomic(a) => {
                    if self.is_positioned(a) {
                        self.defer(a, cx, cy);
                    } else {
                        self.paint_box(a, cx, cy);
                    }
                }
            }
        }
        if clipped {
            self.push(DItem::Unclip);
            self.clip_stack.pop();
        }
        self.opacity = saved_opacity;
    }
}

/// A positioned box waiting to be painted by its stacking context.
struct Deferred {
    z: i32,
    id: u32,
    ox: f32,
    oy: f32,
    fixed: bool,
    /// Clip rects (page coordinates) between the context and the box.
    clips: Vec<[f32; 4]>,
    order: usize,
}

/// Sample a gradient's colour stops into a 256-entry table.
fn gradient_lut(g: &css::Gradient, len: f32, opacity: f32) -> (Box<[u32; 256]>, Option<f32>) {
    let n = g.stops.len();
    let mut pos: Vec<Option<f32>> = g.stops.iter().map(|s| s.frac.or(s.px.map(|p| p / len))).collect();
    if pos[0].is_none() {
        pos[0] = Some(0.0);
    }
    if pos[n - 1].is_none() {
        pos[n - 1] = Some(1.0);
    }
    // Spread unpositioned stops evenly between their neighbours.
    let mut i = 0;
    while i < n {
        if pos[i].is_none() {
            let start = i - 1;
            let mut end = i;
            while pos[end].is_none() {
                end += 1;
            }
            let (a, b) = (pos[start].unwrap(), pos[end].unwrap());
            for k in i..end {
                pos[k] = Some(a + (b - a) * (k - start) as f32 / (end - start) as f32);
            }
            i = end;
        }
        i += 1;
    }
    let mut last = f32::NEG_INFINITY;
    let pos: Vec<f32> = pos
        .into_iter()
        .map(|p| {
            last = last.max(p.unwrap());
            last
        })
        .collect();
    // Repeating gradients: build the table over one period.
    let period = (g.repeating && pos[n - 1] > pos[0] && pos[n - 1] > 0.0).then_some(pos[n - 1]);
    let pos: Vec<f32> = match period {
        Some(p) => pos.iter().map(|v| v / p).collect(),
        None => pos,
    };
    let ch = |c: u32, s: u32| ((c >> s) & 255) as f32;
    let mut lut = Box::new([0u32; 256]);
    for (i, e) in lut.iter_mut().enumerate() {
        let t = i as f32 / 255.0;
        let k = pos.iter().position(|&p| p >= t).unwrap_or(n);
        let c = if k == 0 {
            g.stops[0].color
        } else if k == n {
            g.stops[n - 1].color
        } else {
            let (p0, p1) = (pos[k - 1], pos[k]);
            let f = if p1 > p0 { (t - p0) / (p1 - p0) } else { 1.0 };
            let (a, b) = (g.stops[k - 1].color, g.stops[k].color);
            let mix = |s| (ch(a, s) + (ch(b, s) - ch(a, s)) * f).round() as u32;
            mix(24) << 24 | mix(16) << 16 | mix(8) << 8 | mix(0)
        };
        let alpha = ((c >> 24) as f32 * opacity) as u32;
        *e = alpha << 24 | (c & 0xFF_FFFF);
    }
    (lut, period)
}

// ------------------------------------------------------------- rasterizer

#[inline]
fn blend(dst: u32, src: u32, a: u32) -> u32 {
    let inv = 255 - a;
    let rb = (((src & 0xFF00FF) * a + (dst & 0xFF00FF) * inv) >> 8) & 0xFF00FF;
    let g = (((src & 0x00FF00) * a + (dst & 0x00FF00) * inv) >> 8) & 0x00FF00;
    0xFF00_0000 | rb | g
}

/// Signed distance from (px, py) to a rounded rect (negative inside).
#[inline]
fn rrect_sd(px: f32, py: f32, r4: &[f32; 4], rad: &[f32; 4]) -> f32 {
    let [x, y, w, h] = *r4;
    let (hw, hh) = (w * 0.5, h * 0.5);
    let (qx, qy) = (px - (x + hw), py - (y + hh));
    let r = if qx < 0.0 {
        if qy < 0.0 { rad[0] } else { rad[3] }
    } else if qy < 0.0 {
        rad[1]
    } else {
        rad[2]
    };
    let dx = qx.abs() - (hw - r);
    let dy = qy.abs() - (hh - r);
    let (ox, oy) = (dx.max(0.0), dy.max(0.0));
    (ox * ox + oy * oy).sqrt() + dx.max(dy).min(0.0) - r
}

#[inline]
fn erf(x: f32) -> f32 {
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0 - (((((1.061_405_4 * t - 1.453_152) * t) + 1.421_413_7) * t - 0.284_496_74) * t + 0.254_829_6) * t * (-x * x).exp();
    s * y
}

pub struct Surface<'a> {
    pub buf: &'a mut [u32],
    pub w: usize,
    pub h: usize,
    /// Page x/y that maps to pixel (0, 0).
    pub ox: f32,
    pub oy: f32,
    /// Clip rectangle in pixels: x0, y0, x1, y1.
    pub clip: (i32, i32, i32, i32),
}

impl<'a> Surface<'a> {
    pub fn new(buf: &'a mut [u32], w: usize, h: usize, ox: f32, oy: f32) -> Self {
        Surface { buf, w, h, ox, oy, clip: (0, 0, w as i32, h as i32) }
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, c: u32, a: u32) {
        let p = &mut self.buf[y as usize * self.w + x as usize];
        *p = if a >= 255 { c | 0xFF00_0000 } else { blend(*p, c, a) };
    }

    fn span(&mut self, y: i32, x0: i32, x1: i32, c: u32) {
        let (x0, x1) = (x0.max(self.clip.0), x1.min(self.clip.2));
        if x0 >= x1 || y < self.clip.1 || y >= self.clip.3 {
            return;
        }
        let a = c >> 24;
        let line = &mut self.buf[y as usize * self.w + x0 as usize..y as usize * self.w + x1 as usize];
        if a == 255 {
            line.fill(c);
        } else {
            for p in line {
                *p = blend(*p, c, a);
            }
        }
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        if c >> 24 == 0 {
            return;
        }
        let x0 = (x - self.ox).round() as i32;
        let x1 = (x + w - self.ox).round() as i32;
        let y0 = ((y - self.oy).round() as i32).max(self.clip.1);
        let y1 = ((y + h - self.oy).round() as i32).min(self.clip.3);
        for row in y0..y1 {
            self.span(row, x0, x1, c);
        }
    }

    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: [f32; 4], c: u32) {
        let a = c >> 24;
        if a == 0 {
            return;
        }
        let (x, y) = (x - self.ox, y - self.oy);
        let shape = [x, y, w, h];
        let (top, bot) = (r[0].max(r[1]), r[2].max(r[3]));
        let y0 = (y.round() as i32).max(self.clip.1);
        let y1 = ((y + h).round() as i32).min(self.clip.3);
        let x0 = (x.floor() as i32).max(self.clip.0);
        let x1 = ((x + w).ceil() as i32).min(self.clip.2);
        for py in y0..y1 {
            let fy = py as f32 + 0.5;
            if fy > y + top && fy < y + h - bot {
                self.span(py, x.round() as i32, (x + w).round() as i32, c);
                continue;
            }
            for px in x0..x1 {
                let cov = (0.5 - rrect_sd(px as f32 + 0.5, fy, &shape, &r)).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.put(px, py, c, (a as f32 * cov) as u32);
                }
            }
        }
    }

    pub fn ring(&mut self, d: &RingData) {
        let a = d.c >> 24;
        if a == 0 {
            return;
        }
        let o = [d.o[0] - self.ox, d.o[1] - self.oy, d.o[2], d.o[3]];
        let i = [d.i[0] - self.ox, d.i[1] - self.oy, d.i[2], d.i[3]];
        let (top, bot) = (d.or[0].max(d.or[1]), d.or[2].max(d.or[3]));
        let y0 = (o[1].round() as i32).max(self.clip.1);
        let y1 = ((o[1] + o[3]).round() as i32).min(self.clip.3);
        let x0 = (o[0].floor() as i32).max(self.clip.0);
        let x1 = ((o[0] + o[2]).ceil() as i32).min(self.clip.2);
        for py in y0..y1 {
            let fy = py as f32 + 0.5;
            if fy > o[1] + top && fy < o[1] + o[3] - bot {
                if fy > i[1] && fy < i[1] + i[3] {
                    self.span(py, o[0].round() as i32, i[0].round() as i32, d.c);
                    self.span(py, (i[0] + i[2]).round() as i32, (o[0] + o[2]).round() as i32, d.c);
                } else {
                    self.span(py, o[0].round() as i32, (o[0] + o[2]).round() as i32, d.c);
                }
                continue;
            }
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let co = (0.5 - rrect_sd(fx, fy, &o, &d.or)).clamp(0.0, 1.0);
                if co <= 0.0 {
                    continue;
                }
                let ci = (0.5 - rrect_sd(fx, fy, &i, &d.ir)).clamp(0.0, 1.0);
                let cov = co * (1.0 - ci);
                if cov > 0.0 {
                    self.put(px, py, d.c, (a as f32 * cov) as u32);
                }
            }
        }
    }

    pub fn shadow(&mut self, d: &ShadowData) {
        let a = d.c >> 24;
        if a == 0 {
            return;
        }
        let s = [d.s[0] - self.ox, d.s[1] - self.oy, d.s[2], d.s[3]];
        let hole = [d.hole[0] - self.ox, d.hole[1] - self.oy, d.hole[2], d.hole[3]];
        let sigma = d.blur / 2.0;
        let ext = (sigma * 3.0).ceil() + 1.0;
        let y0 = ((s[1] - ext).floor() as i32).max(self.clip.1);
        let y1 = ((s[1] + s[3] + ext).ceil() as i32).min(self.clip.3);
        let x0 = ((s[0] - ext).floor() as i32).max(self.clip.0);
        let x1 = ((s[0] + s[2] + ext).ceil() as i32).min(self.clip.2);
        let hr_max = d.hr.iter().copied().fold(0.0, f32::max);
        let k = if sigma > 0.0 { 1.0 / (sigma * std::f32::consts::SQRT_2) } else { 0.0 };
        for py in y0..y1 {
            let fy = py as f32 + 0.5;
            // Pixels well inside the element's border box are covered by it.
            let (hx0, hx1) = if fy > hole[1] + hr_max + 1.0 && fy < hole[1] + hole[3] - hr_max - 1.0 {
                ((hole[0] + 1.0).ceil() as i32, (hole[0] + hole[2] - 1.0).floor() as i32)
            } else {
                (i32::MAX, i32::MIN)
            };
            for px in x0..x1 {
                if px >= hx0 && px < hx1 {
                    continue;
                }
                let fx = px as f32 + 0.5;
                let inside_hole = (0.5 - rrect_sd(fx, fy, &hole, &d.hr)).clamp(0.0, 1.0);
                if inside_hole >= 1.0 {
                    continue;
                }
                let dist = rrect_sd(fx, fy, &s, &d.r);
                let cov = if sigma > 0.0 { 0.5 * (1.0 - erf(dist * k)) } else { (0.5 - dist).clamp(0.0, 1.0) };
                let cov = cov * (1.0 - inside_hole);
                if cov > 0.002 {
                    self.put(px, py, d.c, (a as f32 * cov) as u32);
                }
            }
        }
    }

    pub fn image(&mut self, d: &ImgData) {
        let img = &d.img;
        let [dx, dy, dw, dh] = [d.dst[0] - self.ox, d.dst[1] - self.oy, d.dst[2], d.dst[3]];
        if dw < 0.5 || dh < 0.5 || img.w == 0 || img.h == 0 {
            return;
        }
        let cl = [d.clip[0] - self.ox, d.clip[1] - self.oy, d.clip[2], d.clip[3]];
        let mut x0 = (cl[0].round() as i32).max(self.clip.0);
        let mut x1 = ((cl[0] + cl[2]).round() as i32).min(self.clip.2);
        let mut y0 = (cl[1].round() as i32).max(self.clip.1);
        let mut y1 = ((cl[1] + cl[3]).round() as i32).min(self.clip.3);
        if !d.tile.0 {
            x0 = x0.max(dx.round() as i32);
            x1 = x1.min((dx + dw).round() as i32);
        }
        if !d.tile.1 {
            y0 = y0.max(dy.round() as i32);
            y1 = y1.min((dy + dh).round() as i32);
        }
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        // Pick the mip level closest to (but not below) the target size.
        let mut scale = (img.w as f32 / dw).min(img.h as f32 / dh);
        let mut level = 0;
        while scale >= 2.0 && level < img.mips.len() {
            scale /= 2.0;
            level += 1;
        }
        let (sw, sh, src) = if level == 0 { (img.w, img.h, &img.px) } else { let m = &img.mips[level - 1]; (m.0, m.1, &m.2) };
        let tap = |p: i32, d0: f32, dl: f32, tile: bool, sl: u32| -> (u32, u32, u32) {
            let mut u = p as f32 + 0.5 - d0;
            if tile {
                u = u.rem_euclid(dl);
            }
            let f = (u * sl as f32 / dl - 0.5).clamp(0.0, sl as f32 - 1.0);
            let i0 = f as u32;
            (i0, (i0 + 1).min(sl - 1), ((f - i0 as f32) * 256.0) as u32)
        };
        let cols: Vec<(u32, u32, u32)> = (x0..x1).map(|p| tap(p, dx, dw, d.tile.0, sw)).collect();
        #[inline]
        fn lerp(a: u32, b: u32, w: u32) -> u32 {
            let iw = 256 - w;
            let rb = (((a & 0xFF00FF) * iw + (b & 0xFF00FF) * w) >> 8) & 0xFF00FF;
            let ag = ((((a >> 8) & 0xFF00FF) * iw + ((b >> 8) & 0xFF00FF) * w)) & 0xFF00FF00;
            rb | ag
        }
        let rounded = d.radius.iter().any(|&r| r > 0.0);
        let rmax = d.radius.iter().copied().fold(0.0, f32::max);
        for py in y0..y1 {
            let (v0, v1, wv) = tap(py, dy, dh, d.tile.1, sh);
            let fy = py as f32 + 0.5;
            let corner_row = rounded && (fy < cl[1] + rmax || fy > cl[1] + cl[3] - rmax);
            let (r0, r1) = (&src[(v0 * sw) as usize..][..sw as usize], &src[(v1 * sw) as usize..][..sw as usize]);
            let row = &mut self.buf[py as usize * self.w..][..self.w];
            for (i, &(u0, u1, wu)) in cols.iter().enumerate() {
                let top = lerp(r0[u0 as usize], r0[u1 as usize], wu);
                let bot = lerp(r1[u0 as usize], r1[u1 as usize], wu);
                let c = lerp(top, bot, wv);
                let mut a = (c >> 24) * d.alpha / 255;
                if corner_row {
                    let cov = (0.5 - rrect_sd(x0 as f32 + i as f32 + 0.5, fy, &cl, &d.radius)).clamp(0.0, 1.0);
                    a = (a as f32 * cov) as u32;
                }
                if a == 0 {
                    continue;
                }
                let p = &mut row[(x0 + i as i32) as usize];
                *p = if a >= 255 { c | 0xFF00_0000 } else { blend(*p, c, a) };
            }
        }
    }

    pub fn gradient(&mut self, d: &GradData) {
        let [rx, ry, rw, rh] = [d.rect[0] - self.ox, d.rect[1] - self.oy, d.rect[2], d.rect[3]];
        let cl = [d.clip[0] - self.ox, d.clip[1] - self.oy, d.clip[2], d.clip[3]];
        let x0 = (cl[0].max(rx).round() as i32).max(self.clip.0);
        let x1 = ((cl[0] + cl[2]).min(rx + rw).round() as i32).min(self.clip.2);
        let y0 = (cl[1].max(ry).round() as i32).max(self.clip.1);
        let y1 = ((cl[1] + cl[3]).min(ry + rh).round() as i32).min(self.clip.3);
        let (cx, cy) = (rx + rw / 2.0, ry + rh / 2.0);
        let a = d.angle.to_radians();
        let (dxv, dyv) = (a.sin(), -a.cos());
        let len = ((rw * dxv).abs() + (rh * dyv).abs()).max(1.0);
        let (erx, ery) = ((rw / 2.0 * std::f32::consts::SQRT_2).max(1.0), (rh / 2.0 * std::f32::consts::SQRT_2).max(1.0));
        let rmax = d.radius.iter().copied().fold(0.0, f32::max);
        for py in y0..y1 {
            let fy = py as f32 + 0.5 - cy;
            let corner_row = rmax > 0.0 && (py as f32 + 0.5 < cl[1] + rmax || py as f32 + 0.5 > cl[1] + cl[3] - rmax);
            let row = &mut self.buf[py as usize * self.w..][..self.w];
            for px in x0..x1 {
                let fx = px as f32 + 0.5 - cx;
                let t = if d.radial { ((fx / erx).powi(2) + (fy / ery).powi(2)).sqrt() } else { (fx * dxv + fy * dyv) / len + 0.5 };
                let t = match d.period {
                    Some(p) => (t / p).rem_euclid(1.0),
                    None => t.clamp(0.0, 1.0),
                };
                let c = d.lut[(t * 255.0) as usize];
                let mut a = c >> 24;
                if corner_row {
                    let cov = (0.5 - rrect_sd(px as f32 + 0.5, py as f32 + 0.5, &cl, &d.radius)).clamp(0.0, 1.0);
                    a = (a as f32 * cov) as u32;
                }
                if a == 0 {
                    continue;
                }
                let p = &mut row[px as usize];
                *p = if a >= 255 { c } else { blend(*p, c, a) };
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(&mut self, s: &str, x: f32, y: f32, face: u8, size: f32, color: u32, f: &Fonts, cache: &GlyphCache) {
        let ca = color >> 24;
        if ca == 0 {
            return;
        }
        let base = (y - self.oy).round() as i32;
        let (cx0, cy0, cx1, cy1) = self.clip;
        if base - (size * 2.0) as i32 > cy1 || base + (size as i32) < cy0 {
            return;
        }
        let w = self.w as i32;
        let mut pen = x - self.ox;
        for ch in s.chars() {
            let a = f.adv(face, ch);
            if pen > cx1 as f32 {
                break;
            }
            if let Some(g) = cache.get(glyph_key(a.face, ch, size)) {
                if g.w > 0 {
                    let gw = g.w as i32;
                    let gx = pen.round() as i32 + g.xmin as i32;
                    let gy = base - g.ymin as i32 - g.h as i32;
                    let c0 = (cx0 - gx).max(0);
                    let c1 = gw.min(cx1 - gx);
                    if c0 < c1 {
                        for row in 0..g.h as i32 {
                            let sy = gy + row;
                            if sy < cy0 || sy >= cy1 {
                                continue;
                            }
                            let src = &g.data[(row * gw) as usize..((row + 1) * gw) as usize];
                            let dst = &mut self.buf[(sy * w + gx + c0) as usize..(sy * w + gx + c1) as usize];
                            for (d, &cov) in dst.iter_mut().zip(&src[c0 as usize..c1 as usize]) {
                                if cov == 0 {
                                    continue;
                                }
                                let a = if ca == 255 { cov as u32 } else { cov as u32 * ca / 255 };
                                *d = if a >= 255 { color } else { blend(*d, color, a) };
                            }
                        }
                    }
                }
            }
            pen += a.adv * size;
        }
    }
}

#[inline]
fn item_text<'a>(texts: [&'a str; 2], src: u8, start: u32, end: u32) -> &'a str {
    &texts[src as usize][start as usize..end as usize]
}

/// Make sure every glyph needed by `items` is rasterized.
pub fn prepare_glyphs(items: &[&DItem], texts: [&str; 2], cache: &mut GlyphCache) {
    let f = fonts();
    for it in items {
        match **it {
            DItem::Text { start, end, src, face, size, .. } => {
                for c in item_text(texts, src, start, end).chars() {
                    let a = f.adv(face, c);
                    cache.ensure(f, a.face, c, size);
                }
            }
            DItem::Layer(ref l) => {
                let inner: Vec<&DItem> = l.items.iter().collect();
                prepare_glyphs(&inner, texts, cache);
            }
            _ => {}
        }
    }
}

/// Vertical extent of an item, for culling. Clip state items are always kept.
fn extent(it: &DItem) -> Option<(f32, f32)> {
    Some(match it {
        DItem::Rect { y, h, .. } => (*y, y + h),
        DItem::RRect(d) => (d.0[1], d.0[1] + d.0[3]),
        DItem::Text { y, size, .. } => (y - size * 1.5, y + size * 0.6),
        DItem::Ring(d) => (d.o[1], d.o[1] + d.o[3]),
        DItem::Image(d) => (d.clip[1], d.clip[1] + d.clip[3]),
        DItem::Gradient(d) => (d.clip[1], d.clip[1] + d.clip[3]),
        DItem::Shadow(d) => (d.s[1] - d.blur * 1.5 - 1.0, d.s[1] + d.s[3] + d.blur * 1.5 + 1.0),
        DItem::Layer(l) => (l.tbounds[1], l.tbounds[1] + l.tbounds[3]),
        DItem::Clip { .. } | DItem::Unclip => return None,
    })
}

fn draw(s: &mut Surface, it: &DItem, texts: [&str; 2], f: &Fonts, cache: &GlyphCache, clips: &mut Vec<(i32, i32, i32, i32)>) {
    let (bt, bb) = (s.oy, s.oy + s.h as f32);
    if let Some((t, b)) = extent(it) {
        if b < bt || t > bb {
            return;
        }
    }
    match it {
        DItem::Rect { x, y, w, h, c } => s.rect(*x, *y, *w, *h, *c),
        DItem::RRect(d) => s.rrect(d.0[0], d.0[1], d.0[2], d.0[3], d.1, d.2),
        DItem::Ring(d) => s.ring(d),
        DItem::Shadow(d) => s.shadow(d),
        DItem::Image(d) => s.image(d),
        DItem::Gradient(d) => s.gradient(d),
        DItem::Text { x, y, start, end, src, face, size, color } => {
            s.text(item_text(texts, *src, *start, *end), *x, *y, *face, *size, *color, f, cache)
        }
        DItem::Clip { x, y, w, h } => {
            let c = s.clip;
            let n = (
                ((x - s.ox).round() as i32).max(c.0),
                ((y - s.oy).round() as i32).max(c.1),
                ((x + w - s.ox).round() as i32).min(c.2),
                ((y + h - s.oy).round() as i32).min(c.3),
            );
            clips.push(c);
            s.clip = (n.0, n.1, n.2.max(n.0), n.3.max(n.1));
        }
        DItem::Unclip => {
            if let Some(c) = clips.pop() {
                s.clip = c;
            }
        }
        DItem::Layer(l) => draw_layer(s, l, texts, f, cache),
    }
}

/// Rasterize a layer's items offscreen (over black and over white, which
/// gives both colour and coverage), then draw them through the matrix with
/// bilinear sampling.
fn draw_layer(s: &mut Surface, l: &LayerData, texts: [&str; 2], f: &Fonts, cache: &GlyphCache) {
    let m = l.m;
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-6 {
        return;
    }
    let tb = l.tbounds;
    let x0 = ((tb[0] - s.ox).floor() as i32).max(s.clip.0);
    let x1 = ((tb[0] + tb[2] - s.ox).ceil() as i32).min(s.clip.2);
    let y0 = ((tb[1] - s.oy).floor() as i32).max(s.clip.1);
    let y1 = ((tb[1] + tb[3] - s.oy).ceil() as i32).min(s.clip.3);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (lx, ly) = (l.bounds[0].floor(), l.bounds[1].floor());
    let w = ((l.bounds[0] + l.bounds[2] - lx).ceil() as usize + 1).min(4096);
    let h = ((l.bounds[1] + l.bounds[3] - ly).ceil() as usize + 1).min(4096);
    let mut black = vec![0xFF00_0000u32; w * h];
    let mut white = vec![0xFFFF_FFFFu32; w * h];
    for buf in [&mut black, &mut white] {
        let mut sub = Surface::new(buf, w, h, lx, ly);
        let mut clips = Vec::new();
        for it in &l.items {
            draw(&mut sub, it, texts, f, cache, &mut clips);
        }
    }
    // Inverse matrix: page -> layer.
    let inv = [m[3] / det, -m[1] / det, -m[2] / det, m[0] / det, (m[2] * m[5] - m[3] * m[4]) / det, (m[1] * m[4] - m[0] * m[5]) / det];
    // Premultiplied colour (over black) and alpha (white - black) at a pixel.
    let at = |x: i32, y: i32| -> [f32; 4] {
        if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
            return [0.0; 4];
        }
        let i = y as usize * w + x as usize;
        let (b, wt) = (black[i], white[i]);
        let ch = |c: u32, sh: u32| ((c >> sh) & 0xFF) as f32;
        let a = 255.0 - ((ch(wt, 16) - ch(b, 16)) + (ch(wt, 8) - ch(b, 8)) + (ch(wt, 0) - ch(b, 0))) / 3.0;
        [ch(b, 16), ch(b, 8), ch(b, 0), a.clamp(0.0, 255.0)]
    };
    for py in y0..y1 {
        let gy = py as f32 + 0.5 + s.oy;
        let row = py as usize * s.w;
        for px in x0..x1 {
            let gx = px as f32 + 0.5 + s.ox;
            let u = inv[0] * gx + inv[2] * gy + inv[4] - lx - 0.5;
            let v = inv[1] * gx + inv[3] * gy + inv[5] - ly - 0.5;
            if u < -1.0 || v < -1.0 || u >= w as f32 || v >= h as f32 {
                continue;
            }
            let (iu, iv) = (u.floor() as i32, v.floor() as i32);
            let (fu, fv) = (u - iu as f32, v - iv as f32);
            let (p00, p10, p01, p11) = (at(iu, iv), at(iu + 1, iv), at(iu, iv + 1), at(iu + 1, iv + 1));
            let mut c = [0.0f32; 4];
            for k in 0..4 {
                c[k] = (p00[k] * (1.0 - fu) + p10[k] * fu) * (1.0 - fv) + (p01[k] * (1.0 - fu) + p11[k] * fu) * fv;
            }
            if l.alpha < 1.0 {
                for v in c.iter_mut() {
                    *v *= l.alpha;
                }
            }
            if c[3] < 0.5 {
                continue;
            }
            let d = s.buf[row + px as usize];
            let ia = 1.0 - c[3] / 255.0;
            let mix = |k: usize, sh: u32| ((c[k] + ((d >> sh) & 0xFF) as f32 * ia).round().clamp(0.0, 255.0) as u32) << sh;
            s.buf[row + px as usize] = 0xFF00_0000 | mix(0, 16) | mix(1, 8) | mix(2, 0);
        }
    }
}

/// Paint the page region starting at page y = `scroll` into `buf` (w × h).
pub fn paint(dl: &DisplayList, layout_text: &str, cache: &mut GlyphCache, buf: &mut [u32], w: usize, h: usize, scroll: f32) {
    let (top, bottom) = (scroll, scroll + h as f32);
    let visible: Vec<&DItem> = dl
        .items
        .iter()
        .filter(|it| extent(it).is_none_or(|(t, b)| t < bottom && b > top))
        .collect();
    let fixed: Vec<&DItem> = dl.fixed.iter().collect();
    let texts = [layout_text, dl.extra.as_str()];
    prepare_glyphs(&visible, texts, cache);
    prepare_glyphs(&fixed, texts, cache);
    let cache: &GlyphCache = cache;
    let f = fonts();
    let bg = dl.bg;

    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    let threads = if visible.len() < 64 || h < 64 { 1 } else { threads };
    let band = h.div_ceil(threads).max(1);
    let paint_band = |chunk: &mut [u32], i: usize| {
        chunk.fill(bg);
        let rows = chunk.len() / w;
        let mut clips = Vec::new();
        let mut s = Surface::new(chunk, w, rows, 0.0, scroll + (i * band) as f32);
        for it in &visible {
            draw(&mut s, it, texts, f, cache, &mut clips);
        }
        // position: fixed content is drawn on top, unaffected by scrolling.
        s.oy = (i * band) as f32;
        s.clip = (0, 0, w as i32, rows as i32);
        clips.clear();
        for it in &fixed {
            draw(&mut s, it, texts, f, cache, &mut clips);
        }
    };
    if threads == 1 {
        paint_band(&mut buf[..w * h], 0);
    } else {
        std::thread::scope(|sc| {
            for (i, chunk) in buf[..w * h].chunks_mut(w * band).enumerate() {
                let pb = &paint_band;
                sc.spawn(move || pb(chunk, i));
            }
        });
    }
}
