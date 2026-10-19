//! Display list generation and software rasterization.
//!
//! The display list is a flat Vec of drawing commands in page coordinates
//! (plus a second list for `position: fixed` content, in viewport
//! coordinates). Painting a frame: cull to the viewport, make sure every
//! needed glyph is in the cache (single-threaded), then split the framebuffer
//! into horizontal bands and rasterize them in parallel with no locking.

use crate::css::{self, FilterOp, CURRENT_COLOR};
use crate::dom::{Document, NodeId};
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
    /// (Inset shadows: the padding box they are drawn inside.)
    pub hole: [f32; 4],
    pub hr: [f32; 4],
    pub inset: bool,
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
    /// Nearest-neighbour scaling (image-rendering: pixelated).
    pub pixelated: bool,
}

pub struct GradData {
    pub rect: [f32; 4],
    pub clip: [f32; 4],
    /// 0 linear, 1 radial, 2 conic.
    pub kind: u8,
    /// Radial / conic center (relative to the rect) and radial radii.
    pub center: [f32; 2],
    pub radii: [f32; 2],
    pub angle: f32,
    /// repeating-*-gradient: the pattern repeats every this fraction of the line.
    pub period: Option<f32>,
    pub lut: Box<[u32; 256]>,
    pub radius: [f32; 4],
}

pub enum DItem {
    Rect { x: f32, y: f32, w: f32, h: f32, c: u32 },
    /// `src` 0 = layout text buffer, 1 = display-list owned text (markers).
    Text { x: f32, y: f32, start: u32, end: u32, src: u8, face: u8, size: f32, color: u32, sp: TextSp },
    /// Rounded rect: (x, y, w, h), radii, colour.
    RRect(Box<([f32; 4], [f32; 4], u32)>),
    Ring(Box<RingData>),
    Shadow(Box<ShadowData>),
    Image(Box<ImgData>),
    Gradient(Box<GradData>),
    Clip { x: f32, y: f32, w: f32, h: f32 },
    Unclip,
    /// Items between these are `position: fixed` content: viewport
    /// coordinates, not scrolled, own clip (painted in stacking order).
    FixedOn,
    FixedOff,
    /// A transformed element: its items are rasterized offscreen, then
    /// drawn through the matrix.
    Layer(Box<LayerData>),
    /// backdrop-filter: filter what is already painted under a rounded
    /// rect.
    Backdrop(Box<BackdropData>),
    /// A border (or outline) with per-side styles and colors.
    Border(Box<BorderData>),
    /// A filled vector shape.
    Path(Box<PathData>),
}

#[derive(Clone, Copy, Debug)]
pub struct BorderData {
    /// Outer rect and corner radii.
    pub o: [f32; 4],
    pub r: [f32; 4],
    /// Widths, style codes (see `Style::bs`) and colors per side
    /// (top, right, bottom, left).
    pub w: [f32; 4],
    pub s: [u8; 4],
    pub c: [u32; 4],
}

/// How a text run is spaced and shaped: letter and word spacing,
/// small caps and paragraph direction.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextSp {
    pub ls: f32,
    pub ws: f32,
    pub caps: u8,
    pub rtl: bool,
}

impl TextSp {
    pub fn of(st: &Style) -> TextSp {
        TextSp { ls: st.letter_sp, ws: st.word_sp, caps: st.caps, rtl: st.rtl }
    }
}

pub struct BackdropData {
    pub r: [f32; 4],
    pub radii: [f32; 4],
    pub filters: Arc<[FilterOp]>,
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
    /// CSS filters, applied to the rasterized layer.
    pub filters: Option<Arc<[FilterOp]>>,
    /// Masks (mask-image layers, clip-path, background-clip text): each
    /// is rasterized and multiplies the layer's alpha.
    pub masks: Vec<Vec<DItem>>,
    /// Masks use luminance (else alpha).
    pub mask_lum: bool,
    /// mix-blend-mode as a canvas composite operation (0 = normal).
    pub blend: u8,
    /// The masks cut out (1 - coverage) instead of keeping.
    pub invert: bool,
}

/// A filled vector shape (page coordinates).
pub struct PathData {
    pub polys: Vec<Vec<(f32, f32)>>,
    pub evenodd: bool,
    pub color: u32,
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
    pub links: Vec<LinkArea>,
    pub fixed_links: Vec<LinkArea>,
    pub extra: String,
    pub bg: u32,
    /// Some item reads back painted pixels (backdrop-filter).
    pub has_backdrop: bool,
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
    /// The element whose background image paints the whole canvas (html,
    /// or body when html has no background); NO_NODE if none.
    canvas_img: NodeId,
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
            links: Vec::new(),
            fixed_links: Vec::new(),
            extra: String::new(),
            bg: bg | 0xFF00_0000,
            has_backdrop: false,
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
        canvas_img: NodeId::MAX,
    };
    // The root background (or body's, if the root has none) paints the
    // canvas.
    let html_st = &styles[doc.html as usize];
    let owner = if html_st.bg_image.is_some() || html_bg >> 24 != 0 { doc.html } else { doc.body };
    let rb = &tree.boxes[tree.root as usize];
    if styles.get(owner as usize).is_some_and(|s| s.bg_image.is_some() && !s.hidden) && rb.node == doc.html && !rb.anon {
        b.canvas_img = owner;
        let canvas = [0.0, 0.0, rb.border_w().max(rb.x + rb.border_w() + rb.m[1]), tree.height.max(rb.border_h()).max(1.0)];
        b.background_image(rb, &styles[owner as usize], rb.x, rb.y, Some(canvas));
    }
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
        self.dl.items.push(it);
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
        // Outer shadows, the first one on top.
        if let Some(list) = st.shadow.clone() {
            for sh in list.iter().rev() {
                if !sh.inset && sh.color >> 24 != 0 {
                    let s = [bx + sh.x - sh.spread, by + sh.y - sh.spread, bw + 2.0 * sh.spread, bh + 2.0 * sh.spread];
                    let sr = r.map(|v| if v > 0.0 { (v + sh.spread).max(0.0) } else { 0.0 });
                    let c = self.fade(sh.color);
                    self.push(DItem::Shadow(Box::new(ShadowData { s, r: sr, blur: sh.blur, c, hole: [bx, by, bw, bh], hr: r, inset: false })));
                }
            }
        }
        // Borders other than uniformly solid ones (styles, per-side colors
        // on rounded corners) go through the general border painter.
        let has_border = b.b.iter().any(|&v| v > 0.0);
        let plain = st.bs.iter().zip(&b.b).all(|(&s, &w)| s <= 1 || w <= 0.0) && (!rounded || (0..4).all(|i| b.b[i] <= 0.0 || st.bc[i] == st.bc[0]));
        let bimg = self.border_image_source(b, st);
        if let Some(src) = bimg {
            self.bg_color(b, st, bx, by, r);
            if b.node != self.canvas_img {
                self.background_image(b, st, bx, by, None);
            }
            self.inset_shadows(b, st, bx, by, r);
            self.border_image(b, st, bx, by, &src);
        } else
        if has_border && !plain {
            self.bg_color(b, st, bx, by, r);
            if b.node != self.canvas_img {
                self.background_image(b, st, bx, by, None);
            }
            self.inset_shadows(b, st, bx, by, r);
            let colors = st.bc.map(|c| self.fade(c));
            self.push(DItem::Border(Box::new(BorderData { o: [bx, by, bw, bh], r, w: b.b, s: st.bs, c: colors })));
        } else if rounded {
            self.bg_color(b, st, bx, by, r);
            if b.node != self.canvas_img {
                self.background_image(b, st, bx, by, None);
            }
            self.inset_shadows(b, st, bx, by, r);
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
            self.bg_color(b, st, bx, by, r);
            if b.node != self.canvas_img {
                self.background_image(b, st, bx, by, None);
            }
            self.inset_shadows(b, st, bx, by, r);
            self.rect(bx, by, bw, b.b[0], st.bc[0]);
            self.rect(bx, by + bh - b.b[2], bw, b.b[2], st.bc[2]);
            self.rect(bx, by, b.b[3], bh, st.bc[3]);
            self.rect(bx + bw - b.b[1], by, b.b[1], bh, st.bc[1]);
        }
        if st.ow > 0.0 && st.os != 0 && st.oc >> 24 != 0 {
            // The outline follows the border box (and its corners), outside
            // it by outline-offset.
            let (ow, off) = (st.ow, st.oo);
            let grow = ow + off;
            let o = [bx - grow, by - grow, bw + 2.0 * grow, bh + 2.0 * grow];
            if o[2] > 0.0 && o[3] > 0.0 {
                let rr = r.map(|v| if v > 0.0 { (v + grow).max(0.0) } else { 0.0 });
                let c = self.fade(st.oc);
                self.push(DItem::Border(Box::new(BorderData { o, r: rr, w: [ow; 4], s: [st.os; 4], c: [c; 4] })));
            }
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
            let dst = if fit == 0 { [cx, cy, cw, ch] } else { [cx + st.obj_pos[0].px(cw - iw * s), cy + st.obj_pos[1].px(ch - ih * s), iw * s, ih * s] };
            let alpha = (255.0 * self.opacity) as u32;
            let r = radii(st, b.border_w(), b.border_h());
            let radius = [0, 1, 2, 3].map(|i| (r[i] - b.b[3].max(b.b[0]) - b.p[3].max(b.p[0])).max(0.0));
            self.push(DItem::Image(Box::new(ImgData { img: img.clone(), dst, clip: [cx, cy, cw, ch], tile: (false, false), alpha, radius, pixelated: st.pixelated })));
        }
    }

    /// border-image-source as an image (gradients rendered at the border
    /// box size), if it can be drawn.
    fn border_image_source(&self, b: &LBox, st: &Style) -> Option<Arc<Image>> {
        let id = st.bimg?;
        match css::bg_image(id) {
            css::BgImage::Url(u) => self.res.url(&u).cloned(),
            css::BgImage::Gradient(g) => {
                let (w, h) = (b.border_w().max(1.0), b.border_h().max(1.0));
                let gd = grad_data(&g, [0.0, 0.0, w, h], [0.0, 0.0, w, h], [0.0; 4], 1.0);
                let (tw, th) = (w.round().clamp(1.0, 2048.0) as u32, h.round().clamp(1.0, 2048.0) as u32);
                let mut px = Vec::with_capacity((tw * th) as usize);
                for y in 0..th {
                    for x in 0..tw {
                        px.push(grad_pixel(&gd, (x as f32 + 0.5) * w / tw as f32, (y as f32 + 0.5) * h / th as f32));
                    }
                }
                Some(Arc::new(crate::image::from_pixels(tw, th, px, w, h)))
            }
        }
    }

    /// Nine-slice border image.
    fn border_image(&mut self, b: &LBox, st: &Style, bx: f32, by: f32, img: &Arc<Image>) {
        let (iw, ih) = (img.iw.max(1e-3), img.ih.max(1e-3));
        let (bw, bh) = (b.border_w(), b.border_h());
        let bws = [b.b[0], b.b[1], b.b[2], b.b[3]];
        // Slices in image px (top, right, bottom, left).
        let sl = [st.bimg_slice[0].px(ih), st.bimg_slice[1].px(iw), st.bimg_slice[2].px(ih), st.bimg_slice[3].px(iw)].map(|v| v.max(0.0));
        let sl = [sl[0].min(ih), sl[1].min(iw), sl[2].min(ih), sl[3].min(iw)];
        // The border image area: the border box grown by the outsets.
        let out: Vec<f32> = (0..4).map(|i| match st.bimg_outset[i] {
            (0, n) => n * bws[i],
            (_, v) => v,
        }).collect();
        let area = [bx - out[3], by - out[0], bw + out[1] + out[3], bh + out[0] + out[2]];
        let mut wid: Vec<f32> = (0..4).map(|i| match st.bimg_width[i] {
            (0, n) => n * bws[i],
            (2, p) => p * if i % 2 == 0 { area[3] } else { area[2] } / 100.0,
            (3, _) => sl[i],
            (_, v) => v,
        }).collect();
        let f = (area[2] / (wid[1] + wid[3]).max(1e-3)).min(area[3] / (wid[0] + wid[2]).max(1e-3)).min(1.0);
        wid.iter_mut().for_each(|v| *v *= f);
        let (sx, sy) = (img.w as f32 / iw, img.h as f32 / ih);
        // Source and destination columns / rows.
        let scol = [0.0, sl[3], iw - sl[1], iw];
        let srow = [0.0, sl[0], ih - sl[2], ih];
        let dcol = [area[0], area[0] + wid[3], area[0] + area[2] - wid[1], area[0] + area[2]];
        let drow = [area[1], area[1] + wid[0], area[1] + area[3] - wid[2], area[1] + area[3]];
        let alpha = (255.0 * self.opacity) as u32;
        for r in 0..3 {
            for c in 0..3 {
                if r == 1 && c == 1 && !st.bimg_fill {
                    continue;
                }
                let src = [scol[c], srow[r], scol[c + 1] - scol[c], srow[r + 1] - srow[r]];
                let dst = [dcol[c], drow[r], dcol[c + 1] - dcol[c], drow[r + 1] - drow[r]];
                if src[2] <= 0.0 || src[3] <= 0.0 || dst[2] <= 0.5 || dst[3] <= 0.5 {
                    continue;
                }
                let piece = crop_image(img, [src[0] * sx, src[1] * sy, src[2] * sx, src[3] * sy], src[2], src[3]);
                // Tile size: edges scale to the border width they sit in.
                let mut tw = dst[2];
                let mut th = dst[3];
                let rx = if r != 1 && c == 1 { st.bimg_rep & 0xF } else if c == 1 { st.bimg_rep & 0xF } else { 0 };
                let ry = if c != 1 && r == 1 { st.bimg_rep >> 4 } else if r == 1 { st.bimg_rep >> 4 } else { 0 };
                if c == 1 && r != 1 {
                    tw = src[2] * dst[3] / src[3];
                }
                if r == 1 && c != 1 {
                    th = src[3] * dst[2] / src[2];
                }
                if r == 1 && c == 1 {
                    // The middle scales like the top and left edges.
                    let kx = if wid[0] > 0.0 { wid[0] / sl[0].max(1e-3) } else { 1.0 };
                    let ky = if wid[3] > 0.0 { wid[3] / sl[3].max(1e-3) } else { 1.0 };
                    tw = src[2] * kx;
                    th = src[3] * ky;
                }
                let fit = |len: f32, t: f32, mode: u8| -> (f32, f32, bool) {
                    match mode {
                        1 => (t, (len - t) / 2.0, true),
                        2 => {
                            let n = (len / t).round().max(1.0);
                            (len / n, 0.0, true)
                        }
                        3 => {
                            let n = (len / t).floor();
                            if n < 1.0 { (t, 0.0, false) } else { (t, 0.0, true) }
                        }
                        _ => (len, 0.0, false),
                    }
                };
                let (w, ox, tile_x) = fit(dst[2], tw, if c == 1 { rx } else { 0 });
                let (h, oy, tile_y) = fit(dst[3], th, if r == 1 { ry } else { 0 });
                self.push(DItem::Image(Box::new(ImgData {
                    img: piece,
                    dst: [dst[0] + ox, dst[1] + oy, w, h],
                    clip: dst,
                    tile: (tile_x, tile_y),
                    alpha,
                    radius: [0.0; 4],
                    pixelated: st.pixelated,
                })));
            }
        }
    }

    /// The background color, in the bottom layer's clip box.
    fn bg_color(&mut self, b: &LBox, st: &Style, bx: f32, by: f32, r: [f32; 4]) {
        let c = self.fade(st.bg);
        if c >> 24 == 0 {
            return;
        }
        let layers = st.bg_images.as_ref().map_or(1, |l| l.len().max(1));
        let clip = st.bg_clip.as_ref().map_or(0, |l| l[(layers - 1) % l.len()]);
        let (bw, bh) = (b.border_w(), b.border_h());
        let shrink = |r: [f32; 4], e: [f32; 4]| [(r[0] - e[3].max(e[0])).max(0.0), (r[1] - e[1].max(e[0])).max(0.0), (r[2] - e[1].max(e[2])).max(0.0), (r[3] - e[3].max(e[2])).max(0.0)];
        let (rect, rr) = match clip {
            1 | 2 => {
                let pad = [bx + b.b[3], by + b.b[0], b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]];
                let pr = shrink(r, b.b);
                if clip == 1 {
                    (pad, pr)
                } else {
                    ([pad[0] + b.p[3], pad[1] + b.p[0], b.w, b.h], shrink(pr, b.p))
                }
            }
            3 => return,
            _ => ([bx, by, bw, bh], r),
        };
        if rr.iter().any(|&v| v > 0.0) {
            self.push(DItem::RRect(Box::new((rect, rr, c))));
        } else {
            self.rect(rect[0], rect[1], rect[2], rect[3], st.bg);
        }
    }

    /// -webkit-text-stroke: offset copies of the glyphs in the stroke
    /// color (cut out of the glyph shapes when the fill is transparent).
    #[allow(clippy::too_many_arguments)]
    fn text_stroke(&mut self, x: f32, y: f32, w: f32, start: u32, end: u32, face: u8, fs: f32, line: f32, sw: f32, sc: u32, hollow: bool, sp: TextSp) {
        let r = (sw / 2.0).max(0.5);
        let n = if r <= 1.0 { 8 } else { 16 };
        let mut items = Vec::new();
        for k in 0..n {
            let t = k as f32 / n as f32 * std::f32::consts::TAU;
            items.push(DItem::Text { x: x + r * t.cos(), y: y + r * t.sin(), start, end, src: 0, face, size: fs, color: sc, sp });
        }
        let pad = r.ceil() + 2.0;
        let bounds = [x - pad, y - line * fs - pad, w + 2.0 * pad, line * fs * 1.6 + 2.0 * pad];
        let cut = if hollow { vec![vec![DItem::Text { x, y, start, end, src: 0, face, size: fs, color: 0xFF00_0000, sp }]] } else { Vec::new() };
        let inverted = !cut.is_empty();
        self.push(DItem::Layer(Box::new(LayerData { items, bounds, m: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], tbounds: bounds, alpha: 1.0, filters: None, masks: cut, mask_lum: inverted, blend: 0, invert: inverted })));
    }

    /// One text decoration line set for a text run at (x, baseline y).
    #[allow(clippy::too_many_arguments)]
    fn decoration(&mut self, dc: &Deco, x: f32, y: f32, w: f32, fs: f32, ascent: f32, descent: f32, under: bool) {
        let th = if dc.thick.is_nan() { (fs / 14.0).round().max(1.0) } else { dc.thick.max(0.5) };
        let color = self.fade(dc.color);
        if color >> 24 == 0 || w <= 0.0 {
            return;
        }
        let mut ys = Vec::new();
        if dc.line & 1 != 0 {
            let base = if under { descent * fs * 0.8 } else { (fs * 0.11).round() };
            ys.push(y + if dc.offset.is_nan() { base } else { dc.offset.max(0.0) + th / 2.0 });
        }
        if dc.line & 2 != 0 {
            ys.push(y - ascent * fs);
        }
        if dc.line & 4 != 0 {
            ys.push(y - (fs * 0.3).round());
        }
        for ly in ys {
            match dc.style {
                1 => {
                    self.rect(x, ly, w, th, dc.color);
                    self.rect(x, ly + th * 2.0, w, th, dc.color);
                }
                2 | 3 => {
                    // dotted: squares a thickness apart; dashed: 3x long.
                    let (on, off) = if dc.style == 2 { (th, th) } else { (th * 3.0, th * 2.0) };
                    let mut px = x;
                    while px < x + w {
                        self.rect(px, ly, on.min(x + w - px), th, dc.color);
                        px += on + off;
                    }
                }
                4 => {
                    // A sine wave as a filled band.
                    let amp = th * 1.2;
                    let period = (th * 6.0).max(4.0);
                    let n = ((w / period) * 12.0).ceil().max(2.0) as usize;
                    let pt = |i: usize, dy: f32| {
                        let px = x + w * i as f32 / n as f32;
                        (px, ly + th / 2.0 + amp * ((px - x) / period * std::f32::consts::TAU).sin() + dy)
                    };
                    let mut poly: Vec<(f32, f32)> = (0..=n).map(|i| pt(i, -th / 2.0)).collect();
                    poly.extend((0..=n).rev().map(|i| pt(i, th / 2.0)));
                    self.push(DItem::Path(Box::new(PathData { polys: vec![poly], evenodd: false, color })));
                }
                _ => self.rect(x, ly, w, th, dc.color),
            }
        }
    }

    /// text-shadow for a text run at (x, baseline y), the first shadow on
    /// top: sharp ones are offset copies, blurred ones a blurred layer.
    #[allow(clippy::too_many_arguments)]
    fn text_shadows(&mut self, list: &[css::Shadow], x: f32, y: f32, w: f32, start: u32, end: u32, face: u8, fs: f32, line: f32, sp: TextSp) {
        for sh in list.iter().rev() {
            if sh.color >> 24 == 0 {
                continue;
            }
            let color = self.fade(sh.color);
            let item = DItem::Text { x: x + sh.x, y: y + sh.y, start, end, src: 0, face, size: fs, color, sp };
            if sh.blur < 0.5 {
                self.push(item);
                continue;
            }
            let pad = (sh.blur * 1.5).ceil() + 2.0;
            let bounds = [x + sh.x - pad, y + sh.y - line * fs - pad, w + 2.0 * pad, line * fs * 1.6 + 2.0 * pad];
            let filters: Arc<[FilterOp]> = Arc::from(vec![FilterOp::Blur(sh.blur / 2.0)]);
            self.push(DItem::Layer(Box::new(LayerData { items: vec![item], bounds, m: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], tbounds: bounds, alpha: 1.0, filters: Some(filters), masks: Vec::new(), mask_lum: false, blend: 0, invert: false })));
        }
    }

    /// Inset box shadows, inside the padding box (the last one first).
    fn inset_shadows(&mut self, b: &LBox, st: &Style, bx: f32, by: f32, r: [f32; 4]) {
        let Some(list) = st.shadow.clone() else { return };
        let pad = [bx + b.b[3], by + b.b[0], b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]];
        if pad[2] <= 0.0 || pad[3] <= 0.0 {
            return;
        }
        let pr = [
            (r[0] - b.b[3].max(b.b[0])).max(0.0),
            (r[1] - b.b[1].max(b.b[0])).max(0.0),
            (r[2] - b.b[1].max(b.b[2])).max(0.0),
            (r[3] - b.b[3].max(b.b[2])).max(0.0),
        ];
        for sh in list.iter().rev() {
            if !sh.inset || sh.color >> 24 == 0 {
                continue;
            }
            let s = [pad[0] + sh.x + sh.spread, pad[1] + sh.y + sh.spread, (pad[2] - 2.0 * sh.spread).max(0.0), (pad[3] - 2.0 * sh.spread).max(0.0)];
            let sr = pr.map(|v| if v > 0.0 { (v - sh.spread).max(0.0) } else { 0.0 });
            let c = self.fade(sh.color);
            self.push(DItem::Shadow(Box::new(ShadowData { s, r: sr, blur: sh.blur, c, hole: pad, hr: pr, inset: true })));
        }
    }

    /// Paint `st`'s background layers for box `b` (the last layer first);
    /// `canvas` paints a root background over the whole canvas instead of
    /// the border box.
    fn background_image(&mut self, b: &LBox, st: &Style, bx: f32, by: f32, canvas: Option<[f32; 4]>) {
        let layers = st.bg_layers();
        if layers.is_empty() {
            return;
        }
        let (bw, bh) = (b.border_w(), b.border_h());
        let border = [bx, by, bw, bh];
        let pad = [bx + b.b[3], by + b.b[0], b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]];
        let content = [pad[0] + b.p[3], pad[1] + b.p[0], b.w, b.h];
        let r = radii(st, bw, bh);
        let shrink = |r: [f32; 4], e: [f32; 4]| [(r[0] - e[3].max(e[0])).max(0.0), (r[1] - e[1].max(e[0])).max(0.0), (r[2] - e[1].max(e[2])).max(0.0), (r[3] - e[3].max(e[2])).max(0.0)];
        let pad_r = shrink(r, b.b);
        let content_r = shrink(pad_r, b.p);
        for l in layers.iter().rev() {
            if l.image == u32::MAX || l.clip == 3 {
                // (background-clip: text is painted with the text.)
                continue;
            }
            let mut area = match l.origin {
                0 => border,
                2 => content,
                _ => pad,
            };
            let (mut paint, mut radius) = match l.clip {
                1 => (pad, pad_r),
                2 => (content, content_r),
                _ => (border, r),
            };
            if let Some(c) = canvas {
                // (An auto-sized gradient covers the canvas rather than tiling.)
                paint = c;
                radius = [0.0; 4];
                area[3] = area[3].max(c[3] - area[1]);
            }
            self.bg_layer(l, area, paint, radius);
        }
    }

    /// One background layer: positioned in `area`, painted in `paint`.
    fn bg_layer(&mut self, l: &BgLayer, area: [f32; 4], paint: [f32; 4], radius: [f32; 4]) {
        let [ax, ay, aw, ah] = area;
        if aw <= 0.0 || ah <= 0.0 || paint[2] <= 0.0 || paint[3] <= 0.0 {
            return;
        }
        let img = css::bg_image(l.image);
        let intrinsic = match &img {
            css::BgImage::Url(u) => match self.res.url(u) {
                Some(i) => Some((i.iw, i.ih, i.clone())),
                None => return,
            },
            css::BgImage::Gradient(_) => None,
        };
        // An image with only an aspect ratio (an SVG with just a viewBox)
        // is sized like `contain` when its size is auto.
        let fluid = intrinsic.as_ref().is_some_and(|i| i.2.fluid);
        let l = &BgLayer { size: if fluid && l.size.fit == 0 && l.size.x == Len::Auto && l.size.y == Len::Auto { BgSize { x: Len::Auto, y: Len::Auto, fit: 2 } } else { l.size }, ..*l };
        // Tile size.
        let (mut w, mut h) = match (intrinsic.as_ref().map(|i| (i.0, i.1)), l.size.fit) {
            (Some((iw, ih)), 1) => {
                let s = (aw / iw).max(ah / ih);
                (iw * s, ih * s)
            }
            (Some((iw, ih)), 2) => {
                let s = (aw / iw).min(ah / ih);
                (iw * s, ih * s)
            }
            (None, 1 | 2) => (aw, ah),
            (i, _) => match (l.size.x, l.size.y, i) {
                (Len::Auto, Len::Auto, Some((iw, ih))) => (iw, ih),
                (Len::Auto, Len::Auto, None) => (aw, ah),
                (x, Len::Auto, Some((iw, ih))) => {
                    let w = x.px(aw);
                    (w, if iw > 0.0 { w * ih / iw } else { ah })
                }
                (Len::Auto, y, Some((iw, ih))) => {
                    let h = y.px(ah);
                    (if ih > 0.0 { h * iw / ih } else { aw }, h)
                }
                (x, Len::Auto, None) => (x.px(aw), ah),
                (Len::Auto, y, None) => (aw, y.px(ah)),
                (x, y, _) => (x.px(aw), y.px(ah)),
            },
        };
        // `round`: scale so a whole number of tiles fits.
        if l.rep.0 == 3 && w > 0.0 {
            let n = (aw / w).round().max(1.0);
            let nw = aw / n;
            if l.rep.1 != 3 && l.size.y == Len::Auto {
                h *= nw / w;
            }
            w = nw;
        }
        if l.rep.1 == 3 && h > 0.0 {
            let n = (ah / h).round().max(1.0);
            h = ah / n;
        }
        if w < 0.5 || h < 0.5 {
            return;
        }
        let mut x = ax + l.pos[0].px(aw - w);
        let mut y = ay + l.pos[1].px(ah - h);
        // `space`: whole tiles spread across the area.
        let (mut gap_x, mut gap_y) = (0.0, 0.0);
        let mut tile = (l.rep.0 != 0, l.rep.1 != 0);
        if l.rep.0 == 2 {
            let n = (aw / w).floor();
            if n >= 2.0 {
                gap_x = (aw - n * w) / (n - 1.0);
                x = ax;
            } else {
                tile.0 = false;
            }
        }
        if l.rep.1 == 2 {
            let n = (ah / h).floor();
            if n >= 2.0 {
                gap_y = (ah - n * h) / (n - 1.0);
                y = ay;
            } else {
                tile.1 = false;
            }
        }
        let alpha = (255.0 * self.opacity) as u32;
        // The painted region: the clip box, cut to the tile on axes that
        // don't repeat.
        let mut clip = paint;
        if !tile.0 {
            let (x0, x1) = (clip[0].max(x), (clip[0] + clip[2]).min(x + w));
            clip[0] = x0;
            clip[2] = (x1 - x0).max(0.0);
        }
        if !tile.1 {
            let (y0, y1) = (clip[1].max(y), (clip[1] + clip[3]).min(y + h));
            clip[1] = y0;
            clip[3] = (y1 - y0).max(0.0);
        }
        if clip[2] <= 0.0 || clip[3] <= 0.0 {
            return;
        }
        if gap_x > 0.0 || gap_y > 0.0 {
            // Spaced tiles: one item each.
            let (sx, sy) = (w + gap_x, h + gap_y);
            let first_x = if tile.0 { x - ((x - paint[0]) / sx).ceil() * sx } else { x };
            let first_y = if tile.1 { y - ((y - paint[1]) / sy).ceil() * sy } else { y };
            let mut items = 0;
            let mut ty = first_y;
            while ty < paint[1] + paint[3] && items < 4000 {
                let mut tx = first_x;
                while tx < paint[0] + paint[2] && items < 4000 {
                    self.tile_item(&img, intrinsic.as_ref().map(|i| &i.2), [tx, ty, w, h], paint, (false, false), alpha, radius);
                    items += 1;
                    if !tile.0 {
                        break;
                    }
                    tx += sx;
                }
                if !tile.1 {
                    break;
                }
                ty += sy;
            }
            return;
        }
        self.tile_item(&img, intrinsic.as_ref().map(|i| &i.2), [x, y, w, h], clip, tile, alpha, radius);
    }

    /// An image or gradient tile at `dst`, repeated on the `tile` axes
    /// within `clip`.
    #[allow(clippy::too_many_arguments)]
    fn tile_item(&mut self, img: &css::BgImage, decoded: Option<&Arc<Image>>, dst: [f32; 4], clip: [f32; 4], tile: (bool, bool), alpha: u32, radius: [f32; 4]) {
        match img {
            css::BgImage::Url(_) => {
                if let Some(i) = decoded {
                    self.push(DItem::Image(Box::new(ImgData { img: i.clone(), dst, clip, tile, alpha, radius, pixelated: false })));
                }
            }
            css::BgImage::Gradient(g) => {
                let covers = (!tile.0 || (dst[0] <= clip[0] && dst[0] + dst[2] >= clip[0] + clip[2])) && (!tile.1 || (dst[1] <= clip[1] && dst[1] + dst[3] >= clip[1] + clip[3]));
                let gd = grad_data(g, dst, clip, radius, self.opacity);
                if covers {
                    self.push(DItem::Gradient(Box::new(gd)));
                } else {
                    // A repeated gradient: render one tile as an image.
                    let (tw, th) = (dst[2].round().clamp(1.0, 2048.0) as u32, dst[3].round().clamp(1.0, 2048.0) as u32);
                    let mut px = Vec::with_capacity((tw * th) as usize);
                    let sx = dst[2] / tw as f32;
                    let sy = dst[3] / th as f32;
                    for y in 0..th {
                        for x in 0..tw {
                            px.push(grad_pixel(&gd, (x as f32 + 0.5) * sx, (y as f32 + 0.5) * sy));
                        }
                    }
                    let img = Arc::new(crate::image::from_pixels(tw, th, px, dst[2], dst[3]));
                    self.push(DItem::Image(Box::new(ImgData { img, dst, clip, tile, alpha: 255, radius, pixelated: false })));
                }
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
            DItem::Backdrop(d) => d.r,
            DItem::Border(d) => d.o,
            DItem::Path(d) => poly_bbox(&d.polys)?,
            DItem::Clip { .. } | DItem::Unclip | DItem::FixedOn | DItem::FixedOff => return None,
        })
    }

    fn target(&mut self) -> &mut Vec<DItem> {
        &mut self.dl.items
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
        // backdrop-filter: the backdrop is everything painted so far, under
        // the element's border box (outside any layer of its own).
        if let Some(f) = st.backdrop.as_ref().filter(|_| !st.hidden && st.opacity >= 0.004) {
            let (w, h) = (b.border_w(), b.border_h());
            let (r, radii) = match m {
                Some(m) => (transform_rect(m, [bx, by, w, h]), [0.0; 4]),
                None => ([bx, by, w, h], radii(st, w, h)),
            };
            if r[2] > 0.0 && r[3] > 0.0 {
                self.dl.has_backdrop = true;
                self.push(DItem::Backdrop(Box::new(BackdropData { r, radii, filters: Arc::from(&f[..]) })));
            }
        }
        let (masks, mask_lum) = if st.hidden { (Vec::new(), false) } else { self.masks_for(b, st, bx, by) };
        let blend = if st.hidden { 0 } else { st.blend };
        let text_bg = !st.hidden && st.bg_clip.as_ref().is_some_and(|c| c.contains(&3));
        let start = self.target().len();
        if m.is_none() && !group_alpha && (st.filter.is_none() || st.hidden) && masks.is_empty() && blend == 0 {
            self.paint_context_or_box(id, ox, oy);
            if text_bg {
                self.text_background(b, st, bx, by, start);
            }
            return;
        }
        let saved = std::mem::take(self.target());
        let saved_opacity = self.opacity;
        if group_alpha {
            self.opacity = 1.0;
            self.group_opacity = Some(id);
        }
        self.paint_context_or_box(id, ox, oy);
        if text_bg {
            self.text_background(b, st, bx, by, 0);
        }
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
        let alpha = if group_alpha { st.opacity * self.opacity } else { 1.0 };
        let filters = st.filter.as_ref().filter(|_| !st.hidden).map(|f| Arc::from(&f[..]));
        // Room for blurs and shadows to spread.
        let bounds = match &filters {
            Some(f) => {
                let o = crate::filter::outset(f);
                [bounds[0] - o[0], bounds[1] - o[1], bounds[2] + o[0] + o[2], bounds[3] + o[1] + o[3]]
            }
            None => bounds,
        };
        let tbounds = transform_rect(m, bounds);
        self.push(DItem::Layer(Box::new(LayerData { items, bounds, m, tbounds, alpha, filters, masks, mask_lum, blend, invert: false })));
    }

    /// Mask lists for an element: its mask-image layers and clip-path.
    fn masks_for(&mut self, b: &LBox, st: &Style, bx: f32, by: f32) -> (Vec<Vec<DItem>>, bool) {
        let mut masks = Vec::new();
        let mut lum = false;
        let (bw, bh) = (b.border_w(), b.border_h());
        let border = [bx, by, bw, bh];
        let pad = [bx + b.b[3], by + b.b[0], b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]];
        let content = [pad[0] + b.p[3], pad[1] + b.p[0], b.w, b.h];
        let boxes = |code: u8| match code {
            1 => pad,
            2 => content,
            _ => border,
        };
        let layers = st.mask_layers();
        if !layers.is_empty() {
            let saved = std::mem::take(self.target());
            let saved_op = std::mem::replace(&mut self.opacity, 1.0);
            for l in layers.iter().rev() {
                if l.image == u32::MAX {
                    continue;
                }
                if l.attach == 1 {
                    lum = true;
                }
                self.bg_layer(l, boxes(l.origin), boxes(l.clip), [0.0; 4]);
            }
            self.opacity = saved_op;
            let items = std::mem::replace(self.target(), saved);
            masks.push(items);
        }
        if let Some(i) = st.clip_path {
            let (shape, bx_code) = css::clip_shape_of(i);
            let r = boxes(bx_code);
            let polys = clip_polys(&shape, r, radii(st, bw, bh));
            let evenodd = matches!(&*shape, css::ClipShape::Polygon(true, _) | css::ClipShape::Path(true, _));
            masks.push(vec![DItem::Path(Box::new(PathData { polys, evenodd, color: 0xFF00_0000 }))]);
        }
        (masks, lum)
    }

    /// background-clip: text — the text-clipped background layers, masked
    /// by the glyphs painted for the element since `start`, inserted there.
    fn text_background(&mut self, b: &LBox, st: &Style, bx: f32, by: f32, start: usize) {
        let mut mask = Vec::new();
        let items = self.target();
        let from = start.min(items.len());
        collect_text(&items[from..], &mut mask);
        if mask.is_empty() {
            return;
        }
        let (bw, bh) = (b.border_w(), b.border_h());
        let border = [bx, by, bw, bh];
        let pad = [bx + b.b[3], by + b.b[0], b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]];
        let content = [pad[0] + b.p[3], pad[1] + b.p[0], b.w, b.h];
        let saved = std::mem::take(self.target());
        let c = self.fade(st.bg);
        let layers = st.bg_layers();
        if c >> 24 != 0 && layers.last().is_some_and(|l| l.clip == 3) {
            self.push(DItem::Rect { x: bx, y: by, w: bw, h: bh, c });
        }
        for l in layers.iter().rev() {
            if l.clip != 3 || l.image == u32::MAX {
                continue;
            }
            let area = match l.origin {
                0 => border,
                2 => content,
                _ => pad,
            };
            self.bg_layer(l, area, border, [0.0; 4]);
        }
        let items = std::mem::replace(self.target(), saved);
        if items.is_empty() {
            return;
        }
        let bounds = border;
        let layer = DItem::Layer(Box::new(LayerData { items, bounds, m: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], tbounds: bounds, alpha: 1.0, filters: None, masks: vec![mask], mask_lum: false, blend: 0, invert: false }));
        let at = start.min(self.target().len());
        self.target().insert(at, layer);
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
        (st.position != Pos::Static && st.z.is_some()) || st.position == Pos::Fixed || st.opacity < 1.0 || st.has_transform() || st.filter.is_some() || st.backdrop.is_some()
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
        let enter_fixed = d.fixed && !saved_fixed;
        if d.fixed {
            self.in_fixed = true;
        }
        if enter_fixed {
            self.push(DItem::FixedOn);
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
        if enter_fixed {
            self.push(DItem::FixedOff);
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
        let (mut px, mut py) = (bx + b.b[3], by + b.b[0]);
        let clipped = !b.anon && st.overflow != Ovf::Visible;
        if clipped {
            let (w, h) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
            self.push(DItem::Clip { x: px, y: py, w, h });
            self.clip_stack.push([px, py, w, h]);
            // Scrolled content moves under the clip.
            if let Some(&(sx, sy)) = self.tree.scroll.get(&b.node) {
                px -= sx;
                py -= sy;
            }
        }
        let cx = px + b.p[3];
        let cy = py + b.p[0] + b.cell_off;

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
            self.push(DItem::Text { x: cx - w - fs * 0.5, y: base, start, end, src: 1, face, size: fs, color, sp: TextSp::default() });
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
                FragKind::Text { start, end, node, link, bg, w, extra } => {
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
                    let color = self.fade(if ts.text_fill == CURRENT_COLOR { ts.color } else { ts.text_fill });
                    let right = cx + b.w;
                    if st.ellipsis && clipped && x + w > right + 0.5 && end > start {
                        // text-overflow: ellipsis
                        let text = &self.tree.text[start as usize..end as usize];
                        let ew = self.f.measure_sp(face, fs, "\u{2026}", ts.letter_sp, ts.word_sp, ts.caps);
                        let avail = right - x - ew;
                        let mut cut = 0;
                        let mut acc = 0.0;
                        for (i, ch) in text.char_indices() {
                            let cw = self.f.measure_sp(face, fs, &text[i..i + ch.len_utf8()], ts.letter_sp, ts.word_sp, ts.caps);
                            if acc + cw > avail {
                                break;
                            }
                            acc += cw;
                            cut = i + ch.len_utf8();
                        }
                        if x < right {
                            let s0 = self.dl.extra.len() as u32;
                            self.dl.extra.push_str(text[..cut].trim_end());
                            self.dl.extra.push('\u{2026}');
                            let e0 = self.dl.extra.len() as u32;
                            self.push(DItem::Text { x, y, start: s0, end: e0, src: 1, face, size: fs, color, sp: TextSp::of(ts) });
                        }
                        continue;
                    }
                    let sp = TextSp { ws: ts.word_sp + extra, ..TextSp::of(ts) };
                    if end > start {
                        if let Some(list) = ts.text_shadow.clone() {
                            self.text_shadows(&list, x, y, w, start, end, face, fs, a + d, sp);
                        }
                        if ts.stroke_w > 0.0 {
                            let sc = self.fade(if ts.stroke_c == CURRENT_COLOR { ts.color } else { ts.stroke_c });
                            self.text_stroke(x, y, w, start, end, face, fs, a + d, ts.stroke_w, sc, color >> 24 == 0, sp);
                        }
                        self.push(DItem::Text { x, y, start, end, src: 0, face, size: fs, color, sp });
                    }
                    if let Some(decos) = ts.decos.clone() {
                        for dc in decos.iter() {
                            self.decoration(dc, x, y, w, fs, a, d, ts.under_pos);
                        }
                    }
                    if link != NO_LINK {
                        self.link(x, y - a * fs, w, (a + d) * fs, link);
                    }
                }
                FragKind::InlineBox { node, w, first, last } => {
                    let ist = &self.styles[node as usize];
                    if ist.hidden {
                        continue;
                    }
                    let face = self.f.pick_style(ist);
                    let fs = ist.font_size;
                    let (a, d, _) = self.f.metrics(face);
                    let (pt, pb) = (ist.padding[0].px(b.w), ist.padding[2].px(b.w));
                    let (bt, bb) = (ist.bw[0], ist.bw[2]);
                    let bl = if first { ist.bw[3] } else { 0.0 };
                    let br = if last { ist.bw[1] } else { 0.0 };
                    let x = cx + f.x;
                    let top = cy + f.y - a * fs - pt - bt;
                    let h = (a + d) * fs + pt + pb + bt + bb;
                    let r = radii(ist, w, h);
                    let r = [if first { r[0] } else { 0.0 }, if last { r[1] } else { 0.0 }, if last { r[2] } else { 0.0 }, if first { r[3] } else { 0.0 }];
                    let bgc = self.fade(ist.bg);
                    if r.iter().any(|&v| v > 0.0) {
                        if bgc >> 24 != 0 {
                            self.push(DItem::RRect(Box::new(([x, top, w, h], r, bgc))));
                        }
                        let c = self.fade(ist.bc[0]);
                        if (bt > 0.0 || bb > 0.0 || bl > 0.0 || br > 0.0) && c >> 24 != 0 {
                            let i = [x + bl, top + bt, w - bl - br, h - bt - bb];
                            let ir = r.map(|v| (v - bt.max(bl)).max(0.0));
                            self.push(DItem::Ring(Box::new(RingData { o: [x, top, w, h], or: r, i, ir, c })));
                        }
                    } else {
                        self.rect(x, top, w, h, ist.bg);
                        self.rect(x, top, w, bt, ist.bc[0]);
                        self.rect(x, top + h - bb, w, bb, ist.bc[2]);
                        self.rect(x, top, bl, h, ist.bc[3]);
                        self.rect(x + w - br, top, br, h, ist.bc[1]);
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
/// A sub-rectangle (pixel coordinates) of an image, `iw` x `ih` CSS px.
fn crop_image(img: &Arc<Image>, r: [f32; 4], iw: f32, ih: f32) -> Arc<Image> {
    let x0 = (r[0].round() as i64).clamp(0, img.w as i64) as u32;
    let y0 = (r[1].round() as i64).clamp(0, img.h as i64) as u32;
    let x1 = ((r[0] + r[2]).round() as i64).clamp(x0 as i64 + 1, img.w as i64) as u32;
    let y1 = ((r[1] + r[3]).round() as i64).clamp(y0 as i64 + 1, img.h as i64) as u32;
    let (w, h) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
    let mut px = Vec::with_capacity((w * h) as usize);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            px.push(img.px.get((y.min(img.h - 1) * img.w + x.min(img.w - 1)) as usize).copied().unwrap_or(0));
        }
    }
    Arc::new(crate::image::from_pixels(w, h, px, iw, ih))
}

/// Copies of the text items in `items` (recursing into untransformed
/// layers), opaque, for a text mask.
fn collect_text(items: &[DItem], out: &mut Vec<DItem>) {
    for it in items {
        match it {
            DItem::Text { x, y, start, end, src, face, size, sp, .. } => {
                out.push(DItem::Text { x: *x, y: *y, start: *start, end: *end, src: *src, face: *face, size: *size, color: 0xFF00_0000, sp: *sp });
            }
            DItem::Layer(l) if l.m == [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] => collect_text(&l.items, out),
            _ => {}
        }
    }
}

/// A clip-path shape as polygons in page coordinates, within reference
/// box `r` (whose corner radii are `br`).
fn clip_polys(shape: &css::ClipShape, r: [f32; 4], br: [f32; 4]) -> Vec<Vec<(f32, f32)>> {
    let [x, y, w, h] = r;
    let px = |v: css::Value, base: f32| match v {
        css::Value::Pct(p) => p * base / 100.0,
        css::Value::Px(p) => p,
        css::Value::Em(e) | css::Value::Rem(e) => e * 16.0,
        css::Value::Calc(c) => c[0] * base / 100.0 + c[1] + (c[2] + c[3]) * 16.0,
        _ => 0.0,
    };
    let ellipse = |cx: f32, cy: f32, rx: f32, ry: f32| -> Vec<(f32, f32)> {
        let n = ((rx.max(ry) * 0.8).ceil() as usize).clamp(16, 360);
        (0..n).map(|i| {
            let t = i as f32 / n as f32 * std::f32::consts::TAU;
            (cx + rx * t.cos(), cy + ry * t.sin())
        }).collect()
    };
    match shape {
        css::ClipShape::Inset(ins, rad) => {
            let (t, rr, b, l) = (px(ins[0], h), px(ins[1], w), px(ins[2], h), px(ins[3], w));
            let rect = [x + l, y + t, (w - l - rr).max(0.0), (h - t - b).max(0.0)];
            let mut radius = [px(rad[0], w), px(rad[1], w), px(rad[2], w), px(rad[3], w)];
            if ins.iter().all(|v| matches!(v, css::Value::Px(p) if *p == 0.0)) && radius.iter().all(|&v| v == 0.0) {
                // A bare box keyword keeps the box's own corners.
                radius = br;
            }
            vec![rrect_poly(rect, radius)]
        }
        css::ClipShape::Circle(rv, c) => {
            let (cx, cy) = (x + px(c[0], w), y + px(c[1], h));
            let rad = match rv {
                css::Value::Auto => (cx - x).abs().min((x + w - cx).abs()).min((cy - y).abs()).min((y + h - cy).abs()),
                css::Value::Kw(_) => (cx - x).abs().max((x + w - cx).abs()).max((cy - y).abs()).max((y + h - cy).abs()),
                v => px(*v, (w * w + h * h).sqrt() / std::f32::consts::SQRT_2),
            };
            vec![ellipse(cx, cy, rad, rad)]
        }
        css::ClipShape::Ellipse(rv, c) => {
            let (cx, cy) = (x + px(c[0], w), y + px(c[1], h));
            let side = |v: css::Value, lo: f32, hi: f32, base: f32| match v {
                css::Value::Auto => lo.abs().min(hi.abs()),
                css::Value::Kw(_) => lo.abs().max(hi.abs()),
                v => px(v, base),
            };
            vec![ellipse(cx, cy, side(rv[0], cx - x, x + w - cx, w), side(rv[1], cy - y, y + h - cy, h))]
        }
        css::ClipShape::Polygon(_, pts) => vec![pts.iter().map(|p| (x + px(p[0], w), y + px(p[1], h))).collect()],
        css::ClipShape::Path(_, d) => crate::svg::parse_path_data(d).into_iter().map(|s| s.pts.into_iter().map(|(a, b)| (x + a, y + b)).collect()).collect(),
    }
}

/// A rounded rect as a polygon.
fn rrect_poly(r: [f32; 4], rad: [f32; 4]) -> Vec<(f32, f32)> {
    let [x, y, w, h] = r;
    let lim = w.min(h) / 2.0;
    let rad = rad.map(|v| v.clamp(0.0, lim));
    let mut out = Vec::new();
    let corners = [(x + w - rad[1], y + rad[1], -90.0f32), (x + w - rad[2], y + h - rad[2], 0.0), (x + rad[3], y + h - rad[3], 90.0), (x + rad[0], y + rad[0], 180.0)];
    let rs = [rad[1], rad[2], rad[3], rad[0]];
    for (k, &(cx, cy, a0)) in corners.iter().enumerate() {
        let rr = rs[k];
        if rr <= 0.0 {
            let p = match k {
                0 => (x + w, y),
                1 => (x + w, y + h),
                2 => (x, y + h),
                _ => (x, y),
            };
            out.push(p);
            continue;
        }
        let n = ((rr * 0.6).ceil() as usize).clamp(4, 64);
        for i in 0..=n {
            let t = (a0 + 90.0 * i as f32 / n as f32).to_radians();
            out.push((cx + rr * t.cos(), cy + rr * t.sin()));
        }
    }
    out
}

/// A gradient laid out in `rect` (painted within `clip`).
fn grad_data(g: &css::Gradient, rect: [f32; 4], clip: [f32; 4], radius: [f32; 4], opacity: f32) -> GradData {
    let (w, h) = (rect[2].max(1e-3), rect[3].max(1e-3));
    let px = |v: css::Value, base: f32| match v {
        css::Value::Pct(p) => p * base / 100.0,
        css::Value::Px(p) => p,
        css::Value::Em(e) | css::Value::Rem(e) => e * 16.0,
        css::Value::Calc(c) => c[0] * base / 100.0 + c[1] + (c[2] + c[3]) * 16.0,
        _ => base / 2.0,
    };
    let center = [px(g.pos[0], w), px(g.pos[1], h)];
    let kind = if g.conic { 2 } else if g.radial { 1 } else { 0 };
    let mut radii = [w / 2.0, h / 2.0];
    if g.radial {
        let (dl, dr, dt, db) = (center[0].abs(), (w - center[0]).abs(), center[1].abs(), (h - center[1]).abs());
        let (cx, cy) = (dl.min(dr), dt.min(db));
        let (fx, fy) = (dl.max(dr), dt.max(db));
        radii = match (g.size, g.circle) {
            (3, true) => [cx.min(cy); 2],
            (3, false) => [cx, cy],
            (1, true) => [fx.max(fy); 2],
            (1, false) => [fx, fy],
            (2, true) => [(cx * cx + cy * cy).sqrt(); 2],
            (2, false) => [cx * std::f32::consts::SQRT_2, cy * std::f32::consts::SQRT_2],
            (4, _) => {
                let rx = px(g.radii[0], w);
                [rx, if g.circle { rx } else { px(g.radii[1], h) }]
            }
            (_, true) => [(fx * fx + fy * fy).sqrt(); 2],
            (_, false) => [fx * std::f32::consts::SQRT_2, fy * std::f32::consts::SQRT_2],
        };
        radii = radii.map(|r| r.max(1e-3));
    }
    let a = g.angle.to_radians();
    let len = match kind {
        1 => radii[0],
        2 => 1.0,
        _ => (w * a.sin()).abs() + (h * a.cos()).abs(),
    };
    let (lut, period) = gradient_lut(g, len.max(1e-3), opacity);
    GradData { rect, clip, kind, center, radii, angle: g.angle, period, lut, radius }
}

/// The gradient's color at (fx, fy) relative to its rect (straight ARGB).
#[inline]
fn grad_pixel(d: &GradData, fx: f32, fy: f32) -> u32 {
    let t = match d.kind {
        1 => {
            let (x, y) = (fx - d.center[0], fy - d.center[1]);
            ((x / d.radii[0]).powi(2) + (y / d.radii[1]).powi(2)).sqrt()
        }
        2 => {
            let (x, y) = (fx - d.center[0], fy - d.center[1]);
            ((x.atan2(-y).to_degrees() - d.angle) / 360.0).rem_euclid(1.0)
        }
        _ => {
            let (w, h) = (d.rect[2], d.rect[3]);
            let a = d.angle.to_radians();
            let (dxv, dyv) = (a.sin(), -a.cos());
            let len = ((w * dxv).abs() + (h * dyv).abs()).max(1e-3);
            ((fx - w / 2.0) * dxv + (fy - h / 2.0) * dyv) / len + 0.5
        }
    };
    let t = match d.period {
        Some(p) => (t / p).rem_euclid(1.0),
        None => t.clamp(0.0, 1.0),
    };
    d.lut[(t * 255.0) as usize]
}

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
            // Interpolated premultiplied (transparent stops don't darken).
            let (a, b) = (g.stops[k - 1].color, g.stops[k].color);
            let (aa, ba) = (ch(a, 24) / 255.0, ch(b, 24) / 255.0);
            let alpha = aa + (ba - aa) * f;
            let mix = |s| {
                let v = ch(a, s) * aa + (ch(b, s) * ba - ch(a, s) * aa) * f;
                if alpha > 0.0 { (v / alpha).round().clamp(0.0, 255.0) as u32 } else { 0 }
            };
            ((alpha * 255.0).round() as u32) << 24 | mix(16) << 16 | mix(8) << 8 | mix(0)
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

fn poly_bbox(polys: &[Vec<(f32, f32)>]) -> Option<[f32; 4]> {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in polys.iter().flatten() {
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    (x1 >= x0 && y1 >= y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// Coverage of the pixel centered at (fx, fy) by a rect.
#[inline]
fn cover_rect(fx: f32, fy: f32, r: &[f32; 4]) -> f32 {
    let cx = ((fx + 0.5).min(r[0] + r[2]) - (fx - 0.5).max(r[0])).clamp(0.0, 1.0);
    let cy = ((fy + 0.5).min(r[1] + r[3]) - (fy - 0.5).max(r[1])).clamp(0.0, 1.0);
    cx * cy
}

/// The darker or lighter variant of a border color (groove, ridge,
/// inset, outset).
fn shade(c: u32, dark: bool) -> u32 {
    let a = c & 0xFF00_0000;
    let ch = |s: u32| ((c >> s) & 0xFF) as f32;
    let (r, g, b) = (ch(16), ch(8), ch(0));
    let (r, g, b) = if dark {
        (r * 0.6, g * 0.6, b * 0.6)
    } else if r + g + b < 60.0 {
        // Black stays visible: its light variant is a gray.
        (r.max(160.0), g.max(160.0), b.max(160.0))
    } else {
        (r, g, b)
    };
    a | (r as u32) << 16 | (g as u32) << 8 | b as u32
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

    /// Fill a vector shape.
    pub fn path(&mut self, d: &PathData) {
        let a = d.color >> 24;
        if a == 0 {
            return;
        }
        let polys: Vec<Vec<(f32, f32)>> = d.polys.iter().map(|p| p.iter().map(|&(x, y)| (x - self.ox, y - self.oy)).collect()).collect();
        let Some(cov) = crate::svg::rasterize(&polys, d.evenodd, self.w, self.h) else { return };
        for y in 0..cov.h {
            let py = cov.y0 + y as i32;
            if py < self.clip.1 || py >= self.clip.3 {
                continue;
            }
            for x in 0..cov.w {
                let px = cov.x0 + x as i32;
                if px < self.clip.0 || px >= self.clip.2 {
                    continue;
                }
                let c = cov.data[y * cov.w + x];
                if c > 0.0 {
                    self.put(px, py, d.color, (a as f32 * c) as u32);
                }
            }
        }
    }

    /// Paint a border ring: per pixel, the side is chosen by the diagonal
    /// corner joins; each side has its own style and color.
    pub fn border(&mut self, d: &BorderData) {
        let o = [d.o[0] - self.ox, d.o[1] - self.oy, d.o[2], d.o[3]];
        let [wt, wr, wb, wl] = d.w;
        let i = [o[0] + wl, o[1] + wt, (o[2] - wl - wr).max(0.0), (o[3] - wt - wb).max(0.0)];
        let ir = [
            (d.r[0] - wl.max(wt)).max(0.0),
            (d.r[1] - wr.max(wt)).max(0.0),
            (d.r[2] - wr.max(wb)).max(0.0),
            (d.r[3] - wl.max(wb)).max(0.0),
        ];
        let x0 = (o[0].floor() as i32).max(self.clip.0);
        let x1 = ((o[0] + o[2]).ceil() as i32).min(self.clip.2);
        let y0 = (o[1].floor() as i32).max(self.clip.1);
        let y1 = ((o[1] + o[3]).ceil() as i32).min(self.clip.3);
        let rounded = d.r.iter().any(|&v| v > 0.0);
        let inner = i[2] > 0.0 && i[3] > 0.0;
        // Dash / dot pattern along a side of length `len` (style 2 / 3).
        let fit = |len: f32, unit: f32| -> f32 {
            let n = (len / unit).round().max(1.0);
            len / n
        };
        for py in y0..y1 {
            let fy = py as f32 + 0.5;
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let co = if rounded { (0.5 - rrect_sd(fx, fy, &o, &d.r)).clamp(0.0, 1.0) } else { cover_rect(fx, fy, &o) };
                if co <= 0.0 {
                    continue;
                }
                let ci = if !inner {
                    0.0
                } else if rounded {
                    (0.5 - rrect_sd(fx, fy, &i, &ir)).clamp(0.0, 1.0)
                } else {
                    cover_rect(fx, fy, &i)
                };
                let ring = co * (1.0 - ci);
                if ring <= 0.0 {
                    continue;
                }
                // Normalized depth into each side; the smallest wins.
                let depth = |dist: f32, w: f32| if w > 0.0 { dist / w } else { f32::INFINITY };
                let ds = [depth(fy - o[1], wt), depth(o[0] + o[2] - fx, wr), depth(o[1] + o[3] - fy, wb), depth(fx - o[0], wl)];
                let mut side = 0;
                for k in 1..4 {
                    if ds[k] < ds[side] {
                        side = k;
                    }
                }
                let (style, bw, t) = (d.s[side], d.w[side], ds[side].clamp(0.0, 1.0));
                if style == 0 || bw <= 0.0 {
                    continue;
                }
                let mut color = d.c[side];
                let pw = 1.0 / bw;
                // Fraction of the pixel's depth range inside [a, b].
                let band = |a: f32, b: f32| ((t + pw / 2.0).min(b) - (t - pw / 2.0).max(a)).max(0.0) / pw;
                let along = if side % 2 == 0 { fx - o[0] } else { fy - o[1] };
                let len = if side % 2 == 0 { o[2] } else { o[3] };
                let k = match style {
                    2 => {
                        // Dashes 3x the width (2x for thin borders), as long as the gaps.
                        let unit = fit(len, if bw < 3.0 { 6.0 * bw } else { 4.0 * bw });
                        let ph = (along + unit / 4.0).rem_euclid(unit);
                        let edge = |x: f32| x.clamp(0.0, 1.0);
                        edge(unit / 2.0 - ph + 0.5).min(edge(ph + 0.5))
                    }
                    3 => {
                        // Round dots one width across, one width apart.
                        let unit = fit(len, 2.0 * bw);
                        let cx = (along / unit).floor() * unit + unit / 2.0;
                        let (dx, dy) = (along - cx, (t - 0.5) * bw);
                        let dist = (dx * dx + dy * dy).sqrt();
                        if bw <= 2.0 {
                            // Thin dots are pixel-aligned squares.
                            let unit = 2.0 * bw.round().max(1.0);
                            if (along - 0.5).rem_euclid(unit) < unit / 2.0 { 1.0 } else { 0.0 }
                        } else {
                            (bw / 2.0 - dist + 0.5).clamp(0.0, 1.0)
                        }
                    }
                    4 => {
                        if bw < 3.0 { 1.0 } else { band(0.0, 1.0 / 3.0) + band(2.0 / 3.0, 1.0) }
                    }
                    5..=8 => {
                        let top_left = side == 0 || side == 3;
                        let outer_half = t < 0.5;
                        let dark = match style {
                            5 => top_left == outer_half,  // groove
                            6 => top_left != outer_half,  // ridge
                            7 => top_left,                // inset
                            _ => !top_left,               // outset
                        };
                        color = shade(color, dark);
                        1.0
                    }
                    _ => 1.0,
                };
                let a = (color >> 24) as f32 * ring * k;
                if a >= 0.5 {
                    self.put(px, py, color, a.min(255.0) as u32);
                }
            }
        }
    }

    pub fn shadow(&mut self, d: &ShadowData) {
        let a = d.c >> 24;
        if a == 0 {
            return;
        }
        if d.inset {
            return self.inset_shadow(d);
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

    /// An inset shadow: the padding box minus the (blurred) inner shape.
    fn inset_shadow(&mut self, d: &ShadowData) {
        let a = d.c >> 24;
        let s = [d.s[0] - self.ox, d.s[1] - self.oy, d.s[2], d.s[3]];
        let clip = [d.hole[0] - self.ox, d.hole[1] - self.oy, d.hole[2], d.hole[3]];
        let sigma = d.blur / 2.0;
        let k = if sigma > 0.0 { 1.0 / (sigma * std::f32::consts::SQRT_2) } else { 0.0 };
        let y0 = (clip[1].floor() as i32).max(self.clip.1);
        let y1 = ((clip[1] + clip[3]).ceil() as i32).min(self.clip.3);
        let x0 = (clip[0].floor() as i32).max(self.clip.0);
        let x1 = ((clip[0] + clip[2]).ceil() as i32).min(self.clip.2);
        let reach = sigma * 3.0 + 1.0;
        for py in y0..y1 {
            let fy = py as f32 + 0.5;
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let inside = (0.5 - rrect_sd(fx, fy, &clip, &d.hr)).clamp(0.0, 1.0);
                if inside <= 0.0 {
                    continue;
                }
                let dist = if s[2] > 0.0 && s[3] > 0.0 { rrect_sd(fx, fy, &s, &d.r) } else { reach + 1.0 };
                if dist < -reach {
                    continue;
                }
                let covered = if sigma > 0.0 { 0.5 * (1.0 - erf(dist * k)) } else { (0.5 - dist).clamp(0.0, 1.0) };
                let cov = (1.0 - covered) * inside;
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
        while scale >= 2.0 && level < img.mips.len() && !d.pixelated {
            scale /= 2.0;
            level += 1;
        }
        let (sw, sh, src) = if level == 0 { (img.w, img.h, &img.px) } else { let m = &img.mips[level - 1]; (m.0, m.1, &m.2) };
        let tap = |p: i32, d0: f32, dl: f32, tile: bool, sl: u32| -> (u32, u32, u32) {
            let mut u = p as f32 + 0.5 - d0;
            if tile {
                u = u.rem_euclid(dl);
            }
            if d.pixelated {
                let i0 = ((u * sl as f32 / dl) as u32).min(sl - 1);
                return (i0, i0, 0);
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
        let _ = rw;
        let rmax = d.radius.iter().copied().fold(0.0, f32::max);
        for py in y0..y1 {
            let fy = py as f32 + 0.5 - ry;
            let corner_row = rmax > 0.0 && (py as f32 + 0.5 < cl[1] + rmax || py as f32 + 0.5 > cl[1] + cl[3] - rmax);
            let row = &mut self.buf[py as usize * self.w..][..self.w];
            for px in x0..x1 {
                let fx = px as f32 + 0.5 - rx;
                let c = grad_pixel(d, fx, fy);
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
    pub fn text(&mut self, s: &str, x: f32, y: f32, face: u8, size: f32, color: u32, sp: &TextSp, f: &Fonts, cache: &GlyphCache) {
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
        let plain = *sp == TextSp::default() && s.chars().all(|c| (c as u32) < 0x590);
        let glyphs: Vec<(char, f32, f32)> = if plain { s.chars().map(|c| (c, 1.0, 0.0)).collect() } else { visual_glyphs(s, sp) };
        for (ch, k, after) in glyphs {
            let size_k = size * k;
            let a = f.adv(face, ch);
            if pen > cx1 as f32 {
                break;
            }
            if let Some(g) = cache.get(glyph_key(a.face, ch, size_k)) {
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
            pen += a.adv * size_k + after;
        }
    }
}

/// The glyphs of a text run in visual order: (char, size factor, extra
/// advance after it). Applies small caps, letter / word spacing, Arabic
/// joining forms and bidi reordering (right-to-left runs reversed; their
/// order reversed too in right-to-left paragraphs).
pub fn visual_glyphs(s: &str, sp: &TextSp) -> Vec<(char, f32, f32)> {
    let mut v: Vec<(char, f32, f32)> = Vec::with_capacity(s.len());
    for c in s.chars() {
        let extra = sp.ls + if c == ' ' || c == '\u{A0}' { sp.ws } else { 0.0 };
        let start = v.len();
        for (ch, k) in crate::fonts::caps_chars(c, sp.caps) {
            v.push((ch, k, 0.0));
        }
        if let Some(last) = v[start..].last_mut() {
            last.2 = extra;
        }
    }
    let any_rtl = v.iter().any(|g| is_rtl(g.0));
    if !any_rtl && !sp.rtl {
        return v;
    }
    shape_arabic(&mut v);
    // Resolve directions: strong letters, then neutrals from their
    // neighbours (else the paragraph direction).
    let strong: Vec<Option<bool>> = v.iter().map(|g| if is_rtl(g.0) { Some(true) } else if g.0.is_alphabetic() || g.0.is_ascii_digit() { Some(false) } else { None }).collect();
    let n = v.len();
    let mut dir = vec![sp.rtl; n];
    for i in 0..n {
        dir[i] = match strong[i] {
            Some(d) => d,
            None => {
                let prev = strong[..i].iter().rev().find_map(|d| *d).unwrap_or(sp.rtl);
                let next = strong[i + 1..].iter().find_map(|d| *d).unwrap_or(sp.rtl);
                if prev == next { prev } else { sp.rtl }
            }
        };
    }
    let mut runs: Vec<Vec<(char, f32, f32)>> = Vec::new();
    let mut i = 0;
    while i < n {
        let d = dir[i];
        let mut j = i;
        while j < n && dir[j] == d {
            j += 1;
        }
        let mut run = v[i..j].to_vec();
        if d {
            run.reverse();
            for g in &mut run {
                g.0 = mirror(g.0);
            }
        }
        runs.push(run);
        i = j;
    }
    if sp.rtl {
        runs.reverse();
    }
    runs.concat()
}

fn is_rtl(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF)
}

/// Paired punctuation is mirrored in right-to-left runs.
fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '\u{AB}' => '\u{BB}',
        '\u{BB}' => '\u{AB}',
        c => c,
    }
}

/// Arabic letters as presentation forms by joining context (isolated,
/// final, initial, medial).
fn shape_arabic(v: &mut [(char, f32, f32)]) {
    // (letter, isolated form, dual-joining)
    const T: &[(u32, u32, bool)] = &[
        (0x0621, 0xFE80, false), (0x0622, 0xFE81, false), (0x0623, 0xFE83, false), (0x0624, 0xFE85, false), (0x0625, 0xFE87, false),
        (0x0626, 0xFE89, true), (0x0627, 0xFE8D, false), (0x0628, 0xFE8F, true), (0x0629, 0xFE93, false), (0x062A, 0xFE95, true),
        (0x062B, 0xFE99, true), (0x062C, 0xFE9D, true), (0x062D, 0xFEA1, true), (0x062E, 0xFEA5, true), (0x062F, 0xFEA9, false),
        (0x0630, 0xFEAB, false), (0x0631, 0xFEAD, false), (0x0632, 0xFEAF, false), (0x0633, 0xFEB1, true), (0x0634, 0xFEB5, true),
        (0x0635, 0xFEB9, true), (0x0636, 0xFEBD, true), (0x0637, 0xFEC1, true), (0x0638, 0xFEC5, true), (0x0639, 0xFEC9, true),
        (0x063A, 0xFECD, true), (0x0641, 0xFED1, true), (0x0642, 0xFED5, true), (0x0643, 0xFED9, true), (0x0644, 0xFEDD, true),
        (0x0645, 0xFEE1, true), (0x0646, 0xFEE5, true), (0x0647, 0xFEE9, true), (0x0648, 0xFEED, false), (0x0649, 0xFEEF, false),
        (0x064A, 0xFEF1, true),
    ];
    let info = |c: char| T.iter().find(|t| t.0 == c as u32);
    let transparent = |c: char| matches!(c as u32, 0x064B..=0x065F | 0x0670);
    let joins_both = |c: char| info(c).is_some_and(|t| t.2) || c == '\u{0640}';
    let joins_prev = |c: char| (info(c).is_some_and(|t| t.0 != 0x0621)) || c == '\u{0640}';
    let chars: Vec<char> = v.iter().map(|g| g.0).collect();
    for i in 0..chars.len() {
        let Some(&(_, iso, dual)) = info(chars[i]) else { continue };
        if chars[i] == '\u{0621}' {
            v[i].0 = char::from_u32(iso).unwrap_or(chars[i]);
            continue;
        }
        let prev = chars[..i].iter().rev().find(|c| !transparent(**c)).copied();
        let next = chars[i + 1..].iter().find(|c| !transparent(**c)).copied();
        let p = prev.is_some_and(joins_both);
        let nx = dual && next.is_some_and(joins_prev);
        let form = match (p, nx) {
            (true, true) => iso + 3,
            (true, false) => iso + 1,
            (false, true) => iso + 2,
            _ => iso,
        };
        v[i].0 = char::from_u32(form).unwrap_or(chars[i]);
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
            DItem::Text { start, end, src, face, size, sp, .. } => {
                for (c, k, _) in visual_glyphs(item_text(texts, src, start, end), &sp) {
                    let a = f.adv(face, c);
                    cache.ensure(f, a.face, c, size * k);
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
        DItem::Backdrop(d) => (d.r[1], d.r[1] + d.r[3]),
        DItem::Border(d) => (d.o[1], d.o[1] + d.o[3]),
        DItem::Path(d) => {
            let b = poly_bbox(&d.polys)?;
            (b[1], b[1] + b[3])
        }
        DItem::Clip { .. } | DItem::Unclip | DItem::FixedOn | DItem::FixedOff => return None,
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
        DItem::Text { x, y, start, end, src, face, size, color, sp } => {
            s.text(item_text(texts, *src, *start, *end), *x, *y, *face, *size, *color, sp, f, cache)
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
        // (Handled by the top-level painter.)
        DItem::FixedOn | DItem::FixedOff => {}
        DItem::Layer(l) => draw_layer(s, l, texts, f, cache),
        DItem::Backdrop(d) => draw_backdrop(s, d),
        DItem::Border(d) => s.border(d),
        DItem::Path(d) => s.path(d),
    }
}

/// Filter the pixels under a rounded rect in place. The backdrop is
/// clipped to the rect, so blurs extend its edge pixels.
fn draw_backdrop(s: &mut Surface, d: &BackdropData) {
    let [x, y, w, h] = d.r;
    let x0 = ((x - s.ox).floor() as i32).max(s.clip.0);
    let y0 = ((y - s.oy).floor() as i32).max(s.clip.1);
    let x1 = ((x + w - s.ox).ceil() as i32).min(s.clip.2);
    let y1 = ((y + h - s.oy).ceil() as i32).min(s.clip.3);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (rw, rh) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut img: Vec<crate::filter::Px> = Vec::with_capacity(rw * rh);
    for py in y0..y1 {
        let row = py as usize * s.w;
        for px in x0..x1 {
            let c = s.buf[row + px as usize];
            img.push([((c >> 16) & 0xFF) as f32, ((c >> 8) & 0xFF) as f32, (c & 0xFF) as f32, 255.0]);
        }
    }
    crate::filter::apply(&mut img, rw, rh, &d.filters);
    let rounded = d.radii.iter().any(|&r| r > 0.0);
    let shape = [x - s.ox, y - s.oy, w, h];
    for py in y0..y1 {
        let row = py as usize * s.w;
        for px in x0..x1 {
            // Coverage of the pixel by the (rounded) rect.
            let cov = if rounded {
                (0.5 - rrect_sd(px as f32 + 0.5, py as f32 + 0.5, &shape, &d.radii)).clamp(0.0, 1.0)
            } else {
                let cx = ((px + 1) as f32).min(shape[0] + w) - (px as f32).max(shape[0]);
                let cy = ((py + 1) as f32).min(shape[1] + h) - (py as f32).max(shape[1]);
                cx.clamp(0.0, 1.0) * cy.clamp(0.0, 1.0)
            };
            if cov <= 0.0 {
                continue;
            }
            let p = img[(py - y0) as usize * rw + (px - x0) as usize];
            let dst = &mut s.buf[row + px as usize];
            let d0 = *dst;
            let ch = |sh: u32| ((d0 >> sh) & 0xFF) as f32;
            // The filtered backdrop replaces the original (over it, if the
            // filters left it translucent).
            let ia = 1.0 - p[3] / 255.0;
            let mix = |k: usize, sh: u32| {
                let f = p[k] + ch(sh) * ia;
                ((ch(sh) + (f - ch(sh)) * cov).round().clamp(0.0, 255.0) as u32) << sh
            };
            *dst = 0xFF00_0000 | mix(0, 16) | mix(1, 8) | mix(2, 0);
        }
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
    // The part of the layer that can reach visible pixels: all of it when
    // transformed, else what's under the clip plus the filters' reach.
    let mut lb = l.bounds;
    if m[0] == 1.0 && m[1] == 0.0 && m[2] == 0.0 && m[3] == 1.0 {
        let o = l.filters.as_deref().map_or([0.0; 4], crate::filter::outset);
        let vx0 = x0 as f32 + s.ox - m[4] - o[2];
        let vy0 = y0 as f32 + s.oy - m[5] - o[3];
        let vx1 = x1 as f32 + s.ox - m[4] + o[0];
        let vy1 = y1 as f32 + s.oy - m[5] + o[1];
        let (bx0, by0) = (lb[0].max(vx0), lb[1].max(vy0));
        let (bx1, by1) = ((lb[0] + lb[2]).min(vx1), (lb[1] + lb[3]).min(vy1));
        if bx0 >= bx1 || by0 >= by1 {
            return;
        }
        lb = [bx0, by0, bx1 - bx0, by1 - by0];
    }
    let (lx, ly) = (lb[0].floor(), lb[1].floor());
    let w = ((lb[0] + lb[2] - lx).ceil() as usize + 1).min(4096);
    let h = ((lb[1] + lb[3] - ly).ceil() as usize + 1).min(4096);
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
    // Premultiplied colour (over black) and alpha (white - black).
    let ch = |c: u32, sh: u32| ((c >> sh) & 0xFF) as f32;
    let mut img: Vec<crate::filter::Px> = black
        .iter()
        .zip(&white)
        .map(|(&b, &wt)| {
            let a = (255.0 - ((ch(wt, 16) - ch(b, 16)) + (ch(wt, 8) - ch(b, 8)) + (ch(wt, 0) - ch(b, 0))) / 3.0).clamp(0.0, 255.0);
            [ch(b, 16).min(a), ch(b, 8).min(a), ch(b, 0).min(a), a]
        })
        .collect();
    drop((black, white));
    if let Some(f) = &l.filters {
        crate::filter::apply(&mut img, w, h, f);
    }
    // Masks multiply the layer's alpha (coverage from the same black /
    // white rendering).
    for mask in &l.masks {
        let mut black = vec![0xFF00_0000u32; w * h];
        let mut white = vec![0xFFFF_FFFFu32; w * h];
        for buf in [&mut black, &mut white] {
            let mut sub = Surface::new(buf, w, h, lx, ly);
            let mut clips = Vec::new();
            for it in mask {
                draw(&mut sub, it, texts, f, cache, &mut clips);
            }
        }
        for (i, p) in img.iter_mut().enumerate() {
            let (b, wt) = (black[i], white[i]);
            let a = (255.0 - ((ch(wt, 16) - ch(b, 16)) + (ch(wt, 8) - ch(b, 8)) + (ch(wt, 0) - ch(b, 0))) / 3.0).clamp(0.0, 255.0);
            let k = if l.invert {
                1.0 - a / 255.0
            } else if l.mask_lum {
                // Luminance of the (premultiplied over black) mask color.
                (0.2126 * ch(b, 16) + 0.7152 * ch(b, 8) + 0.0722 * ch(b, 0)) / 255.0
            } else {
                a / 255.0
            };
            if k < 1.0 {
                for v in p.iter_mut() {
                    *v *= k;
                }
            }
        }
    }
    let at = |x: i32, y: i32| -> [f32; 4] {
        if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
            return [0.0; 4];
        }
        img[y as usize * w + x as usize]
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
            if l.blend != 0 {
                // mix-blend-mode: the canvas compositing math.
                let dp = [((d >> 16) & 0xFF) as f32 / 255.0, ((d >> 8) & 0xFF) as f32 / 255.0, (d & 0xFF) as f32 / 255.0, 1.0];
                let o = crate::js::canvas::composite(dp, c.map(|v| v / 255.0), l.blend);
                let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
                s.buf[row + px as usize] = 0xFF00_0000 | b(o[0]) << 16 | b(o[1]) << 8 | b(o[2]);
                continue;
            }
            let ia = 1.0 - c[3] / 255.0;
            let mix = |k: usize, sh: u32| ((c[k] + ((d >> sh) & 0xFF) as f32 * ia).round().clamp(0.0, 255.0) as u32) << sh;
            s.buf[row + px as usize] = 0xFF00_0000 | mix(0, 16) | mix(1, 8) | mix(2, 0);
        }
    }
}

/// Paint the page region starting at page y = `scroll` into `buf` (w × h).
pub fn paint(dl: &DisplayList, layout_text: &str, cache: &mut GlyphCache, buf: &mut [u32], w: usize, h: usize, scroll: f32) {
    let (top, bottom) = (scroll, scroll + h as f32);
    // Cull scrolled items outside the viewport (fixed ones always show).
    let mut in_fixed = false;
    let visible: Vec<&DItem> = dl
        .items
        .iter()
        .filter(|it| {
            match it {
                DItem::FixedOn => in_fixed = true,
                DItem::FixedOff => in_fixed = false,
                _ => {}
            }
            in_fixed || extent(it).is_none_or(|(t, b)| t < bottom && b > top)
        })
        .collect();
    let texts = [layout_text, dl.extra.as_str()];
    prepare_glyphs(&visible, texts, cache);
    let cache: &GlyphCache = cache;
    let f = fonts();
    let bg = dl.bg;

    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    // (Backdrop filters read pixels across bands.)
    let threads = if visible.len() < 64 || h < 64 || dl.has_backdrop { 1 } else { threads };
    let band = h.div_ceil(threads).max(1);
    let paint_band = |chunk: &mut [u32], i: usize| {
        chunk.fill(bg);
        let rows = chunk.len() / w;
        let mut clips = Vec::new();
        let mut s = Surface::new(chunk, w, rows, 0.0, scroll + (i * band) as f32);
        // Fixed content: viewport coordinates and its own clip.
        let mut saved: Option<(f32, (i32, i32, i32, i32), usize)> = None;
        for it in &visible {
            match it {
                DItem::FixedOn => {
                    saved = Some((s.oy, s.clip, clips.len()));
                    s.oy = (i * band) as f32;
                    s.clip = (0, 0, w as i32, rows as i32);
                }
                DItem::FixedOff => {
                    if let Some((oy, clip, n)) = saved.take() {
                        s.oy = oy;
                        s.clip = clip;
                        clips.truncate(n);
                    }
                }
                _ => draw(&mut s, it, texts, f, cache, &mut clips),
            }
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
