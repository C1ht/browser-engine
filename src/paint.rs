//! Display list generation and software rasterization.
//!
//! The display list is a flat Vec of rects and text runs in page coordinates.
//! Painting a frame: cull to the viewport, make sure every needed glyph is in
//! the cache (single-threaded), then split the framebuffer into horizontal
//! bands and rasterize them in parallel with no locking at all.

use crate::dom::Document;
use crate::fonts::{fonts, glyph_key, Fonts, GlyphCache};
use crate::layout::*;
use crate::style::*;

#[derive(Clone, Copy, Debug)]
pub enum DItem {
    Rect { x: f32, y: f32, w: f32, h: f32, c: u32 },
    /// `src` 0 = layout text buffer, 1 = display-list owned text (markers).
    Text { x: f32, y: f32, start: u32, end: u32, src: u8, face: u8, size: f32, color: u32 },
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
    pub extra: String,
    pub bg: u32,
}

impl DisplayList {
    pub fn link_at(&self, x: f32, y: f32) -> Option<u32> {
        self.links
            .iter()
            .rev()
            .find(|a| x >= a.x && x < a.x + a.w && y >= a.y && y < a.y + a.h)
            .map(|a| a.link)
    }
}

struct Builder<'a> {
    tree: &'a LayoutTree,
    styles: &'a [Style],
    f: &'static Fonts,
    dl: DisplayList,
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
            links: Vec::new(),
            extra: String::new(),
            bg: bg | 0xFF00_0000,
        },
    };
    b.paint_box(tree.root, 0.0, 0.0);
    b.dl
}

impl Builder<'_> {
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        if c >> 24 != 0 && w > 0.0 && h > 0.0 {
            self.dl.items.push(DItem::Rect { x, y, w, h, c });
        }
    }

    fn paint_box(&mut self, id: u32, ox: f32, oy: f32) {
        let tree = self.tree;
        let b = &tree.boxes[id as usize];
        let st = &self.styles[b.node as usize];
        let bx = ox + b.x;
        let by = oy + b.y;
        if !b.anon {
            if st.hidden {
                return;
            }
            let (bw, bh) = (b.border_w(), b.border_h());
            self.rect(bx, by, bw, bh, st.bg);
            self.rect(bx, by, bw, b.b[0], st.bc[0]);
            self.rect(bx, by + bh - b.b[2], bw, b.b[2], st.bc[2]);
            self.rect(bx, by, b.b[3], bh, st.bc[3]);
            self.rect(bx + bw - b.b[1], by, b.b[1], bh, st.bc[1]);
            if b.img && bw > 0.0 && bh > 0.0 {
                self.rect(bx, by, bw, bh, 0xFFEE_EEEE);
                for (x, y, w, h) in [(bx, by, bw, 1.0), (bx, by + bh - 1.0, bw, 1.0), (bx, by, 1.0, bh), (bx + bw - 1.0, by, 1.0, bh)] {
                    self.rect(x, y, w, h, 0xFFCC_CCCC);
                }
            }
            if b.link != NO_LINK {
                self.dl.links.push(LinkArea { x: bx, y: by, w: bw, h: bh, link: b.link });
            }
        }
        let cx = bx + b.b[3] + b.p[3];
        let cy = by + b.b[0] + b.p[0] + b.cell_off;

        if let Some(m) = &b.marker {
            let face = self.f.pick(st.family, st.bold, st.italic);
            let fs = st.font_size;
            let (asc, _, _) = self.f.metrics(face);
            let w = self.f.measure(face, fs, m);
            let base = cy + b.baseline.unwrap_or(asc * fs);
            let start = self.dl.extra.len() as u32;
            self.dl.extra.push_str(m);
            let end = self.dl.extra.len() as u32;
            self.dl.items.push(DItem::Text {
                x: cx - w - fs * 0.5,
                y: base,
                start,
                end,
                src: 1,
                face,
                size: fs,
                color: st.color,
            });
        }

        for &c in &b.children {
            self.paint_box(c, cx, cy);
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
                    if end > start {
                        self.dl.items.push(DItem::Text { x, y, start, end, src: 0, face, size: fs, color: ts.color });
                    }
                    let th = (fs / 14.0).round().max(1.0);
                    if ts.underline {
                        self.rect(x, y + (fs * 0.11).round(), w, th, ts.color);
                    }
                    if ts.strike {
                        self.rect(x, y - (fs * 0.3).round(), w, th, ts.color);
                    }
                    if link != NO_LINK {
                        self.dl.links.push(LinkArea { x, y: y - a * fs, w, h: (a + d) * fs, link });
                    }
                }
                FragKind::Atomic(a) => self.paint_box(a, cx, cy),
            }
        }
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

pub struct Surface<'a> {
    pub buf: &'a mut [u32],
    pub w: usize,
    pub h: usize,
    /// Page x/y that maps to pixel (0, 0).
    pub ox: f32,
    pub oy: f32,
}

impl Surface<'_> {
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        let a = c >> 24;
        if a == 0 {
            return;
        }
        let x0 = ((x - self.ox).round() as i32).max(0);
        let x1 = ((x + w - self.ox).round() as i32).min(self.w as i32);
        let y0 = ((y - self.oy).round() as i32).max(0);
        let y1 = ((y + h - self.oy).round() as i32).min(self.h as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for row in y0 as usize..y1 as usize {
            let line = &mut self.buf[row * self.w + x0 as usize..row * self.w + x1 as usize];
            if a == 255 {
                line.fill(c);
            } else {
                for p in line {
                    *p = blend(*p, c, a);
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
        if base - (size * 2.0) as i32 > self.h as i32 || base + (size as i32) < 0 {
            return;
        }
        let (w, h) = (self.w as i32, self.h as i32);
        let mut pen = x - self.ox;
        for c in s.chars() {
            let a = f.adv(face, c);
            if pen > w as f32 {
                break;
            }
            if let Some(g) = cache.get(glyph_key(a.face, c, size)) {
                if g.w > 0 {
                    let gw = g.w as i32;
                    let gx = pen.round() as i32 + g.xmin as i32;
                    let gy = base - g.ymin as i32 - g.h as i32;
                    let c0 = (-gx).max(0);
                    let c1 = gw.min(w - gx);
                    if c0 < c1 {
                        for row in 0..g.h as i32 {
                            let sy = gy + row;
                            if sy < 0 || sy >= h {
                                continue;
                            }
                            let src = &g.data[(row * gw) as usize..((row + 1) * gw) as usize];
                            let dst = &mut self.buf[(sy * w + gx) as usize + c0 as usize..(sy * w + gx + c1) as usize];
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

/// Paint the page region starting at page y = `scroll` into `buf` (w × h).
pub fn paint(dl: &DisplayList, layout_text: &str, cache: &mut GlyphCache, buf: &mut [u32], w: usize, h: usize, scroll: f32) {
    let top = scroll;
    let bottom = scroll + h as f32;
    let visible: Vec<&DItem> = dl
        .items
        .iter()
        .filter(|it| match **it {
            DItem::Rect { y, h, .. } => y < bottom && y + h > top,
            DItem::Text { y, size, .. } => y - size * 1.5 < bottom && y + size * 0.6 > top,
        })
        .collect();
    let texts = [layout_text, dl.extra.as_str()];
    prepare_glyphs(&visible, texts, cache);
    let cache: &GlyphCache = cache;
    let f = fonts();
    let bg = dl.bg;

    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    let threads = if visible.len() < 64 || h < 64 { 1 } else { threads };
    let band = h.div_ceil(threads).max(1);
    let paint_band = |chunk: &mut [u32], i: usize| {
        chunk.fill(bg);
        let rows = chunk.len() / w;
        let mut s = Surface { buf: chunk, w, h: rows, ox: 0.0, oy: scroll + (i * band) as f32 };
        let (bt, bb) = (s.oy, s.oy + rows as f32);
        for it in &visible {
            match **it {
                DItem::Rect { x, y, w, h, c } => {
                    if y < bb && y + h > bt {
                        s.rect(x, y, w, h, c)
                    }
                }
                DItem::Text { x, y, start, end, src, face, size, color } => {
                    if y - size * 1.5 < bb && y + size * 0.6 > bt {
                        s.text(item_text(texts, src, start, end), x, y, face, size, color, f, cache)
                    }
                }
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
