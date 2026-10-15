//! Display list generation and software rasterization.
//!
//! The display list is a flat Vec of drawing commands in page coordinates
//! (plus a second list for `position: fixed` content, in viewport
//! coordinates). Painting a frame: cull to the viewport, make sure every
//! needed glyph is in the cache (single-threaded), then split the framebuffer
//! into horizontal bands and rasterize them in parallel with no locking.

use crate::dom::Document;
use crate::fonts::{fonts, glyph_key, Fonts, GlyphCache};
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

#[derive(Clone, Debug)]
pub enum DItem {
    Rect { x: f32, y: f32, w: f32, h: f32, c: u32 },
    /// `src` 0 = layout text buffer, 1 = display-list owned text (markers).
    Text { x: f32, y: f32, start: u32, end: u32, src: u8, face: u8, size: f32, color: u32 },
    /// Rounded rect: (x, y, w, h), radii, colour.
    RRect(Box<([f32; 4], [f32; 4], u32)>),
    Ring(Box<RingData>),
    Shadow(Box<ShadowData>),
    Clip { x: f32, y: f32, w: f32, h: f32 },
    Unclip,
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
    f: &'static Fonts,
    dl: DisplayList,
    opacity: f32,
    in_fixed: bool,
}

pub fn build(tree: &LayoutTree, doc: &Document, styles: &[Style]) -> DisplayList {
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

    fn positioned(&self, id: u32) -> bool {
        let b = &self.tree.boxes[id as usize];
        !b.anon && matches!(self.styles[b.node as usize].position, Pos::Relative | Pos::Sticky)
    }

    fn z(&self, id: u32) -> i32 {
        self.styles[self.tree.boxes[id as usize].node as usize].z.unwrap_or(0)
    }

    fn paint_abs(&mut self, a: u32, px: f32, py: f32) {
        if self.tree.boxes[a as usize].fixed {
            let saved = self.in_fixed;
            self.in_fixed = true;
            self.paint_box(a, 0.0, 0.0);
            self.in_fixed = saved;
        } else {
            self.paint_box(a, px, py);
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
            self.rect(bx, by, bw, b.b[0], st.bc[0]);
            self.rect(bx, by + bh - b.b[2], bw, b.b[2], st.bc[2]);
            self.rect(bx, by, b.b[3], bh, st.bc[3]);
            self.rect(bx + bw - b.b[1], by, b.b[1], bh, st.bc[1]);
        }
        if b.img && bw > 0.0 && bh > 0.0 {
            self.rect(bx, by, bw, bh, 0xFFEE_EEEE);
            for (x, y, w, h) in [(bx, by, bw, 1.0), (bx, by + bh - 1.0, bw, 1.0), (bx, by, 1.0, bh), (bx + bw - 1.0, by, 1.0, bh)] {
                self.rect(x, y, w, h, 0xFFCC_CCCC);
            }
        }
    }

    fn paint_box(&mut self, id: u32, ox: f32, oy: f32) {
        let tree = self.tree;
        let b = &tree.boxes[id as usize];
        let st = &self.styles[b.node as usize];
        let bx = ox + b.x + b.rel.0;
        let by = oy + b.y + b.rel.1;
        if !b.anon && st.hidden {
            return;
        }
        let saved_opacity = self.opacity;
        if !b.anon {
            self.opacity *= st.opacity;
            if self.opacity < 0.004 {
                self.opacity = saved_opacity;
                return;
            }
            self.decorations(b, st, bx, by);
            if b.link != NO_LINK {
                self.link(bx, by, b.border_w(), b.border_h(), b.link);
            }
        }
        let (px, py) = (bx + b.b[3], by + b.b[0]);
        let cx = px + b.p[3];
        let cy = py + b.p[0] + b.cell_off;
        let clipped = !b.anon && st.overflow == Ovf::Hidden;
        if clipped {
            let (w, h) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
            self.push(DItem::Clip { x: px, y: py, w, h });
        }

        if let Some(m) = &b.marker {
            let face = self.f.pick(st.family, st.bold, st.italic);
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

        // Stacking (simplified): negative z, in-flow blocks, floats, inline
        // content, then positioned descendants in z order.
        let mut abs: Vec<(i32, u32)> = b.abs_kids.iter().map(|&a| (self.z(a), a)).collect();
        abs.sort_by_key(|&(z, _)| z);
        for &(_, a) in abs.iter().filter(|(z, _)| *z < 0) {
            self.paint_abs(a, px, py);
        }
        let mut deferred = Vec::new();
        for &c in &b.children {
            if tree.boxes[c as usize].abs {
                continue;
            }
            if self.positioned(c) {
                deferred.push(c);
            } else {
                self.paint_box(c, cx, cy);
            }
        }
        for &f in &b.floats {
            self.paint_box(f, cx, cy);
        }

        for f in &b.frags {
            match f.kind {
                FragKind::Text { start, end, node, link, bg, w } => {
                    let ts = &self.styles[node as usize];
                    if ts.hidden {
                        continue;
                    }
                    let face = self.f.pick(ts.family, ts.bold, ts.italic);
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
                    if self.positioned(a) {
                        deferred.push(a);
                    } else {
                        self.paint_box(a, cx, cy);
                    }
                }
            }
        }
        deferred.sort_by_key(|&c| self.z(c));
        for c in deferred {
            self.paint_box(c, cx, cy);
        }
        for &(_, a) in abs.iter().filter(|(z, _)| *z >= 0) {
            self.paint_abs(a, px, py);
        }
        if clipped {
            self.push(DItem::Unclip);
        }
        self.opacity = saved_opacity;
    }
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
        if let DItem::Text { start, end, src, face, size, .. } = **it {
            for c in item_text(texts, src, start, end).chars() {
                let a = f.adv(face, c);
                cache.ensure(f, a.face, c, size);
            }
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
        DItem::Shadow(d) => (d.s[1] - d.blur * 1.5 - 1.0, d.s[1] + d.s[3] + d.blur * 1.5 + 1.0),
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
