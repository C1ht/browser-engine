//! Canvas 2D: the bitmap and drawing state behind CanvasRenderingContext2D
//! (and OffscreenCanvas / ImageBitmap). The JS classes are in the prelude;
//! they call the natives at the end of this file.
//!
//! Pixels are premultiplied ARGB. Paths are kept in device space (points
//! are transformed as they're added, as the spec says); strokes are
//! outlined in the user space of the transform current at stroke time.

use super::builtins::arg;
use super::*;
use crate::css::FilterOp;
use crate::svg::{self, Cov, StrokeOpts, Sub};
use std::cell::RefCell;
use std::f32::consts::PI;

pub type M = [f32; 6];
const ID: M = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// a · b (apply b first).
pub fn mul(a: M, b: M) -> M {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}
#[inline]
fn ap(m: &M, x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}
pub fn invert(m: &M) -> Option<M> {
    let d = m[0] * m[3] - m[1] * m[2];
    if d.abs() < 1e-12 || !d.is_finite() {
        return None;
    }
    Some([m[3] / d, -m[1] / d, -m[2] / d, m[0] / d, (m[2] * m[5] - m[3] * m[4]) / d, (m[1] * m[4] - m[0] * m[5]) / d])
}
fn scale_of(m: &M) -> f32 {
    (m[0] * m[3] - m[1] * m[2]).abs().sqrt().max(1e-6)
}

#[inline]
fn unpack(c: u32) -> [f32; 4] {
    let f = |s: u32| ((c >> s) & 0xFF) as f32 / 255.0;
    [f(16), f(8), f(0), f(24)]
}
#[inline]
fn pack(p: [f32; 4]) -> u32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    b(p[3]) << 24 | b(p[0]) << 16 | b(p[1]) << 8 | b(p[2])
}
/// Straight ARGB to premultiplied floats.
#[inline]
fn premul(c: u32) -> [f32; 4] {
    let a = (c >> 24) as f32 / 255.0;
    let f = |s: u32| ((c >> s) & 0xFF) as f32 / 255.0 * a;
    [f(16), f(8), f(0), a]
}

/// A premultiplied ARGB image (an ImageBitmap, a canvas snapshot, a
/// pattern's image).
pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Bitmap {
    pub fn from_image(img: &crate::image::Image) -> Bitmap {
        let px = img
            .px
            .iter()
            .map(|&c| {
                let a = c >> 24;
                if a == 255 {
                    c
                } else {
                    pack(premul(c))
                }
            })
            .collect();
        Bitmap { w: img.w as usize, h: img.h as usize, px }
    }
    #[inline]
    fn at(&self, x: i32, y: i32) -> [f32; 4] {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return [0.0; 4];
        }
        unpack(self.px[y as usize * self.w + x as usize])
    }
    /// Sample at (x, y) in pixel units, clamped to the rect `r`.
    fn sample(&self, x: f32, y: f32, smooth: bool, r: [f32; 4]) -> [f32; 4] {
        let (x0, y0, x1, y1) = (r[0], r[1], r[0] + r[2], r[1] + r[3]);
        if !smooth {
            let xi = x.clamp(x0, x1 - 0.001).floor() as i32;
            let yi = y.clamp(y0, y1 - 0.001).floor() as i32;
            return self.at(xi, yi);
        }
        let fx = (x - 0.5).clamp(x0, (x1 - 1.0).max(x0));
        let fy = (y - 0.5).clamp(y0, (y1 - 1.0).max(y0));
        let (ix, iy) = (fx.floor() as i32, fy.floor() as i32);
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let mx = (x1 - 1.0).max(x0) as i32;
        let my = (y1 - 1.0).max(y0) as i32;
        let (ix1, iy1) = ((ix + 1).min(mx), (iy + 1).min(my));
        let (a, b, c, d) = (self.at(ix, iy), self.at(ix1, iy), self.at(ix, iy1), self.at(ix1, iy1));
        let mut o = [0.0; 4];
        for k in 0..4 {
            o[k] = (a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty) + (c[k] * (1.0 - tx) + d[k] * tx) * ty;
        }
        o
    }
}

#[derive(Clone)]
pub enum GradKind {
    Linear([f32; 4]),
    Radial([f32; 6]),
    Conic([f32; 3]),
}

pub struct Gradient {
    pub kind: GradKind,
    pub stops: Vec<(f32, u32)>,
}

impl Gradient {
    /// 256 premultiplied colors across t = 0..1.
    fn lut(&self, alpha: f32) -> Vec<[f32; 4]> {
        let mut stops = self.stops.clone();
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        (0..256)
            .map(|i| {
                let t = i as f32 / 255.0;
                let c = match stops.iter().position(|s| s.0 >= t) {
                    None => premul(stops.last().unwrap().1),
                    Some(0) => premul(stops[0].1),
                    Some(j) => {
                        let (a, b) = (stops[j - 1], stops[j]);
                        let f = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 1.0 };
                        let (ca, cb) = (premul(a.1), premul(b.1));
                        [0, 1, 2, 3].map(|k| ca[k] + (cb[k] - ca[k]) * f)
                    }
                };
                c.map(|v| v * alpha)
            })
            .collect()
    }
}

pub struct Pattern {
    pub img: Rc<Bitmap>,
    pub rep: (bool, bool),
    pub m: RefCell<M>,
}

#[derive(Clone)]
pub enum Paint {
    Color(u32),
    Grad(Rc<RefCell<Gradient>>),
    Pattern(Rc<Pattern>),
}

#[derive(Clone)]
pub struct FontSpec {
    pub css: String,
    pub size: f32,
    pub face: u8,
}

impl FontSpec {
    pub fn parse(s: &str) -> Option<FontSpec> {
        let t = s.trim();
        let mut bold = false;
        let mut italic = false;
        let size;
        let mut rest = t;
        // style / variant / weight / stretch keywords, then size[/line-height], then family.
        loop {
            let r = rest.trim_start();
            let end = r.find(char::is_whitespace).unwrap_or(r.len());
            let tok = &r[..end];
            let lower = tok.to_ascii_lowercase();
            match lower.as_str() {
                "italic" | "oblique" => italic = true,
                "bold" | "bolder" | "600" | "700" | "800" | "900" => bold = true,
                "normal" | "small-caps" | "lighter" | "100" | "200" | "300" | "400" | "500" | "condensed" | "expanded" | "semi-condensed" | "semi-expanded" => {}
                _ => {
                    let sz = lower.split('/').next().unwrap_or("");
                    let px = if let Some(v) = sz.strip_suffix("px") {
                        v.parse::<f32>().ok()
                    } else if let Some(v) = sz.strip_suffix("pt") {
                        v.parse::<f32>().ok().map(|v| v * 4.0 / 3.0)
                    } else if let Some(v) = sz.strip_suffix("rem").or_else(|| sz.strip_suffix("em")) {
                        v.parse::<f32>().ok().map(|v| v * 16.0)
                    } else if let Some(v) = sz.strip_suffix('%') {
                        v.parse::<f32>().ok().map(|v| v * 0.16)
                    } else {
                        match sz {
                            "xx-small" => Some(9.0),
                            "x-small" => Some(10.0),
                            "small" => Some(13.0),
                            "medium" => Some(16.0),
                            "large" => Some(18.0),
                            "x-large" => Some(24.0),
                            "xx-large" => Some(32.0),
                            _ => None,
                        }
                    };
                    size = px;
                    rest = &r[end..];
                    break;
                }
            }
            if end == r.len() {
                return None;
            }
            rest = &r[end..];
        }
        let size = size.filter(|s| s.is_finite() && *s >= 0.0)?;
        let family = rest.trim();
        if family.is_empty() {
            return None;
        }
        let (names, generic) = crate::css::family_names(family);
        let f = crate::fonts::fonts();
        let face = f.web_pick(&names, if bold { 700 } else { 400 }, italic).unwrap_or_else(|| {
            let fam = match generic {
                crate::css::Kw::Monospace => crate::style::Family::Mono,
                crate::css::Kw::Serif => crate::style::Family::Serif,
                _ => crate::style::Family::Sans,
            };
            f.pick(fam, bold, italic)
        });
        // Serialized as browsers do: "[italic ][bold ]<size>px <family>".
        let mut css = String::new();
        if italic {
            css.push_str("italic ");
        }
        if bold {
            css.push_str("bold ");
        }
        css.push_str(&format!("{}px {}", crate::js::num_to_str(size as f64), family));
        Some(FontSpec { css, size, face })
    }
}

#[derive(Clone)]
pub struct State {
    pub m: M,
    pub fill: Paint,
    pub stroke: Paint,
    pub line_width: f32,
    pub cap: u8,
    pub join: u8,
    pub miter: f32,
    pub dash: Vec<f32>,
    pub dash_off: f32,
    pub alpha: f32,
    pub op: u8,
    pub font: FontSpec,
    pub align: u8,
    pub baseline: u8,
    pub shadow_color: u32,
    pub shadow_blur: f32,
    pub shadow_x: f32,
    pub shadow_y: f32,
    pub clip: Option<Rc<Vec<u8>>>,
    pub smoothing: bool,
    pub filter: Option<Rc<[FilterOp]>>,
    pub filter_css: String,
    pub letter_spacing: f32,
    pub rtl: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            m: ID,
            fill: Paint::Color(0xFF00_0000),
            stroke: Paint::Color(0xFF00_0000),
            line_width: 1.0,
            cap: 0,
            join: 0,
            miter: 10.0,
            dash: Vec::new(),
            dash_off: 0.0,
            alpha: 1.0,
            op: 0,
            font: FontSpec::parse("10px sans-serif").unwrap(),
            align: 0,
            baseline: 0,
            shadow_color: 0,
            shadow_blur: 0.0,
            shadow_x: 0.0,
            shadow_y: 0.0,
            clip: None,
            smoothing: true,
            filter: None,
            filter_css: "none".into(),
            letter_spacing: 0.0,
            rtl: false,
        }
    }
}

pub const OPS: [&str; 26] = [
    "source-over", "source-in", "source-out", "source-atop", "destination-over", "destination-in", "destination-out", "destination-atop", "lighter", "copy", "xor",
    "multiply", "screen", "overlay", "darken", "lighten", "color-dodge", "color-burn", "hard-light", "soft-light", "difference", "exclusion", "hue", "saturation", "color",
    "luminosity",
];

/// Ops that affect pixels outside the drawn shape.
fn unbounded(op: u8) -> bool {
    matches!(op, 1 | 2 | 5 | 7 | 9)
}

fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}
fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut o = c;
    if n < 0.0 {
        o = o.map(|v| l + (v - l) * l / (l - n).max(1e-6));
    }
    if x > 1.0 {
        o = o.map(|v| l + (v - l) * (1.0 - l) / (x - l).max(1e-6));
    }
    o
}
fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color(c.map(|v| v + d))
}
fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}
fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let (mut i, mut mid, mut x) = (0usize, 1usize, 2usize);
    if c[i] > c[mid] {
        std::mem::swap(&mut i, &mut mid);
    }
    if c[mid] > c[x] {
        std::mem::swap(&mut mid, &mut x);
    }
    if c[i] > c[mid] {
        std::mem::swap(&mut i, &mut mid);
    }
    let mut o = [0.0; 3];
    if c[x] > c[i] {
        o[mid] = (c[mid] - c[i]) * s / (c[x] - c[i]);
        o[x] = s;
    }
    o
}

/// Composite premultiplied `s` over `d` with operator `op`.
#[inline]
pub(crate) fn composite(d: [f32; 4], s: [f32; 4], op: u8) -> [f32; 4] {
    let (sa, da) = (s[3], d[3]);
    let pd = |fa: f32, fb: f32| [0, 1, 2, 3].map(|k| s[k] * fa + d[k] * fb);
    match op {
        0 => pd(1.0, 1.0 - sa),
        1 => pd(da, 0.0),
        2 => pd(1.0 - da, 0.0),
        3 => pd(da, 1.0 - sa),
        4 => pd(1.0 - da, 1.0),
        5 => pd(0.0, sa),
        6 => pd(0.0, 1.0 - sa),
        7 => pd(1.0 - da, sa),
        8 => [0, 1, 2, 3].map(|k| (s[k] + d[k]).min(1.0)),
        9 => s,
        10 => pd(1.0 - da, 1.0 - sa),
        _ => {
            // Blend modes on unpremultiplied colors.
            let un = |p: [f32; 4]| if p[3] > 0.0 { [p[0] / p[3], p[1] / p[3], p[2] / p[3]] } else { [0.0; 3] };
            let (cs, cb) = (un(s), un(d));
            let sep = |f: &dyn Fn(f32, f32) -> f32| [f(cb[0], cs[0]), f(cb[1], cs[1]), f(cb[2], cs[2])];
            let hard = |b: f32, s: f32| if s <= 0.5 { b * 2.0 * s } else { let s2 = 2.0 * s - 1.0; b + s2 - b * s2 };
            let b: [f32; 3] = match op {
                11 => sep(&|b, s| b * s),
                12 => sep(&|b, s| b + s - b * s),
                13 => sep(&|b, s| hard(s, b)),
                14 => sep(&|b, s| b.min(s)),
                15 => sep(&|b, s| b.max(s)),
                16 => sep(&|b, s| if b == 0.0 { 0.0 } else if s >= 1.0 { 1.0 } else { (b / (1.0 - s)).min(1.0) }),
                17 => sep(&|b, s| if b >= 1.0 { 1.0 } else if s <= 0.0 { 0.0 } else { 1.0 - ((1.0 - b) / s).min(1.0) }),
                18 => sep(&|b, s| hard(b, s)),
                19 => sep(&|b, s| {
                    if s <= 0.5 {
                        b - (1.0 - 2.0 * s) * b * (1.0 - b)
                    } else {
                        let dd = if b <= 0.25 { ((16.0 * b - 12.0) * b + 4.0) * b } else { b.sqrt() };
                        b + (2.0 * s - 1.0) * (dd - b)
                    }
                }),
                20 => sep(&|b, s| (b - s).abs()),
                21 => sep(&|b, s| b + s - 2.0 * b * s),
                22 => set_lum(set_sat(cs, sat(cb)), lum(cb)),
                23 => set_lum(set_sat(cb, sat(cs)), lum(cb)),
                24 => set_lum(cs, lum(cb)),
                _ => set_lum(cb, lum(cs)),
            };
            let ao = sa + da * (1.0 - sa);
            let c = |k: usize| s[k] * (1.0 - da) + d[k] * (1.0 - sa) + sa * da * b[k];
            [c(0), c(1), c(2), ao]
        }
    }
}

/// Paths in device space, built with the transform `m` current when each
/// segment is added.
#[derive(Clone, Default)]
pub struct PathBuilder {
    pub subs: Vec<Sub>,
    /// The current point in device space.
    pub cur: Option<(f32, f32)>,
}

const TOL: f32 = 0.2;

impl PathBuilder {
    fn last(&mut self) -> &mut Sub {
        if self.subs.is_empty() {
            self.subs.push(Sub::default());
        }
        self.subs.last_mut().unwrap()
    }
    pub fn move_to(&mut self, m: &M, x: f32, y: f32) {
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        let p = ap(m, x, y);
        self.subs.push(Sub { pts: vec![p], closed: false });
        self.cur = Some(p);
    }
    fn ensure(&mut self, m: &M, x: f32, y: f32) {
        if self.cur.is_none() {
            self.move_to(m, x, y);
        }
    }
    pub fn line_to(&mut self, m: &M, x: f32, y: f32) {
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        if self.cur.is_none() {
            return self.move_to(m, x, y);
        }
        let p = ap(m, x, y);
        self.last().pts.push(p);
        self.cur = Some(p);
    }
    fn push_dev(&mut self, p: (f32, f32)) {
        self.last().pts.push(p);
        self.cur = Some(p);
    }
    pub fn quad_to(&mut self, m: &M, cx: f32, cy: f32, x: f32, y: f32) {
        if ![cx, cy, x, y].iter().all(|v| v.is_finite()) {
            return;
        }
        self.ensure(m, cx, cy);
        let p0 = self.cur.unwrap();
        let mut out = Vec::new();
        svg::quad(&mut out, p0, ap(m, cx, cy), ap(m, x, y), TOL);
        for p in out {
            self.push_dev(p);
        }
    }
    pub fn cubic_to(&mut self, m: &M, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        if ![c1x, c1y, c2x, c2y, x, y].iter().all(|v| v.is_finite()) {
            return;
        }
        self.ensure(m, c1x, c1y);
        let p0 = self.cur.unwrap();
        let mut out = Vec::new();
        svg::cubic(&mut out, p0, ap(m, c1x, c1y), ap(m, c2x, c2y), ap(m, x, y), TOL);
        for p in out {
            self.push_dev(p);
        }
    }
    pub fn close(&mut self) {
        let Some(s) = self.subs.last_mut() else { return };
        if s.pts.is_empty() {
            return;
        }
        s.closed = true;
        let start = s.pts[0];
        self.subs.push(Sub { pts: vec![start], closed: false });
        self.cur = Some(start);
    }
    /// An elliptical arc (user space center / radii / rotation).
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(&mut self, m: &M, x: f32, y: f32, rx: f32, ry: f32, rot: f32, a0: f32, a1: f32, ccw: bool) {
        if ![x, y, rx, ry, rot, a0, a1].iter().all(|v| v.is_finite()) {
            return;
        }
        let tau = 2.0 * PI;
        let sweep = if !ccw {
            if a1 - a0 >= tau { tau } else { (a1 - a0).rem_euclid(tau) }
        } else if a0 - a1 >= tau {
            -tau
        } else {
            -((a0 - a1).rem_euclid(tau))
        };
        let (sr, cr) = rot.sin_cos();
        let pt = |a: f32| {
            let (s, c) = a.sin_cos();
            let (ex, ey) = (rx * c, ry * s);
            ap(m, x + ex * cr - ey * sr, y + ex * sr + ey * cr)
        };
        let r_dev = rx.max(ry) * scale_of(m);
        let step = 2.0 * (1.0 - (TOL / r_dev.max(TOL)).min(1.0)).acos();
        let n = ((sweep.abs() / step.max(0.01)).ceil() as usize).clamp(1, 2048);
        let start = pt(a0);
        if self.cur.is_none() {
            self.subs.push(Sub { pts: vec![start], closed: false });
            self.cur = Some(start);
        } else {
            self.push_dev(start);
        }
        for i in 1..=n {
            self.push_dev(pt(a0 + sweep * i as f32 / n as f32));
        }
    }
    /// arcTo, with the current point given in user space.
    #[allow(clippy::too_many_arguments)]
    pub fn arc_to(&mut self, m: &M, x1: f32, y1: f32, x2: f32, y2: f32, r: f32) {
        if ![x1, y1, x2, y2, r].iter().all(|v| v.is_finite()) {
            return;
        }
        self.ensure(m, x1, y1);
        let Some(inv) = invert(m) else { return };
        let (x0, y0) = ap(&inv, self.cur.unwrap().0, self.cur.unwrap().1);
        let (d0x, d0y) = (x0 - x1, y0 - y1);
        let (d2x, d2y) = (x2 - x1, y2 - y1);
        let (l0, l2) = ((d0x * d0x + d0y * d0y).sqrt(), (d2x * d2x + d2y * d2y).sqrt());
        let cross = d0x * d2y - d0y * d2x;
        if r == 0.0 || l0 < 1e-6 || l2 < 1e-6 || cross.abs() < 1e-6 * l0 * l2 {
            return self.line_to(m, x1, y1);
        }
        let cos = ((d0x * d2x + d0y * d2y) / (l0 * l2)).clamp(-1.0, 1.0);
        let half = cos.acos() / 2.0;
        let dist = r / half.tan();
        let (t0x, t0y) = (x1 + d0x / l0 * dist, y1 + d0y / l0 * dist);
        let (t2x, t2y) = (x1 + d2x / l2 * dist, y1 + d2y / l2 * dist);
        // The center is along the bisector.
        let (bx, by) = (d0x / l0 + d2x / l2, d0y / l0 + d2y / l2);
        let bl = (bx * bx + by * by).sqrt();
        let cd = r / half.sin();
        let (cx, cy) = (x1 + bx / bl * cd, y1 + by / bl * cd);
        let a0 = (t0y - cy).atan2(t0x - cx);
        let a1 = (t2y - cy).atan2(t2x - cx);
        self.line_to(m, t0x, t0y);
        self.ellipse(m, cx, cy, r, r, 0.0, a0, a1, cross > 0.0);
    }
    pub fn rect(&mut self, m: &M, x: f32, y: f32, w: f32, h: f32) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) {
            return;
        }
        let pts = vec![ap(m, x, y), ap(m, x + w, y), ap(m, x + w, y + h), ap(m, x, y + h)];
        self.subs.push(Sub { pts, closed: true });
        self.move_to(m, x, y);
    }
    /// roundRect with per-corner (rx, ry) radii (tl, tr, br, bl).
    pub fn round_rect(&mut self, m: &M, x: f32, y: f32, w: f32, h: f32, r: [(f32, f32); 4]) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) {
            return;
        }
        // Negative sizes flip the corners.
        let (mut x, mut y, mut w, mut h, mut r) = (x, y, w, h, r);
        if w < 0.0 {
            x += w;
            w = -w;
            r = [r[1], r[0], r[3], r[2]];
        }
        if h < 0.0 {
            y += h;
            h = -h;
            r = [r[3], r[2], r[1], r[0]];
        }
        // Scale radii down so they fit.
        let f = [w / (r[0].0 + r[1].0), w / (r[3].0 + r[2].0), h / (r[0].1 + r[3].1), h / (r[1].1 + r[2].1)].iter().filter(|v| v.is_finite()).fold(1.0f32, |a, &b| a.min(b));
        let r = r.map(|(a, b)| (a * f, b * f));
        self.move_to(m, x + r[0].0, y);
        self.line_to(m, x + w - r[1].0, y);
        self.ellipse(m, x + w - r[1].0, y + r[1].1, r[1].0, r[1].1, 0.0, -PI / 2.0, 0.0, false);
        self.line_to(m, x + w, y + h - r[2].1);
        self.ellipse(m, x + w - r[2].0, y + h - r[2].1, r[2].0, r[2].1, 0.0, 0.0, PI / 2.0, false);
        self.line_to(m, x + r[3].0, y + h);
        self.ellipse(m, x + r[3].0, y + h - r[3].1, r[3].0, r[3].1, 0.0, PI / 2.0, PI, false);
        self.line_to(m, x, y + r[0].1);
        self.ellipse(m, x + r[0].0, y + r[0].1, r[0].0, r[0].1, 0.0, PI, 1.5 * PI, false);
        self.close();
        self.move_to(m, x, y);
    }
    /// Append another path's subpaths, transformed by `m`.
    pub fn add(&mut self, other: &PathBuilder, m: &M) {
        for s in &other.subs {
            self.subs.push(Sub { pts: s.pts.iter().map(|&(x, y)| ap(m, x, y)).collect(), closed: s.closed });
        }
        self.cur = other.cur.map(|(x, y)| ap(m, x, y));
    }
    fn polys(&self) -> Vec<Vec<(f32, f32)>> {
        self.subs.iter().filter(|s| s.pts.len() > 2).map(|s| s.pts.clone()).collect()
    }
}

/// Winding test of a point against polygons.
fn inside(polys: &[Vec<(f32, f32)>], x: f32, y: f32, evenodd: bool) -> bool {
    let mut wind = 0i32;
    for p in polys {
        let n = p.len();
        for i in 0..n {
            let (a, b) = (p[i], p[(i + 1) % n]);
            if (a.1 <= y) != (b.1 <= y) {
                let t = (y - a.1) / (b.1 - a.1);
                if a.0 + t * (b.0 - a.0) > x {
                    wind += if b.1 > a.1 { 1 } else { -1 };
                }
            }
        }
    }
    if evenodd { wind % 2 != 0 } else { wind != 0 }
}

pub struct Canvas2D {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    pub dirty: bool,
    pub st: State,
    pub stack: Vec<State>,
    pub path: PathBuilder,
    pub alpha: bool,
}

/// What a draw call paints: a paint style, or an image sampler.
enum Src<'a> {
    Paint(&'a Paint),
    Image { bmp: &'a Bitmap, inv: M, rect: [f32; 4] },
}

impl Canvas2D {
    pub fn new(w: usize, h: usize) -> Canvas2D {
        let (w, h) = (w.min(16384), h.min(16384));
        Canvas2D { w, h, px: vec![0; w * h], dirty: true, st: State::default(), stack: Vec::new(), path: PathBuilder::default(), alpha: true }
    }

    /// canvas.width / height assignment: a fresh bitmap and state.
    pub fn resize(&mut self, w: usize, h: usize) {
        let alpha = self.alpha;
        *self = Canvas2D::new(w, h);
        self.alpha = alpha;
    }

    pub fn reset(&mut self) {
        self.px.iter_mut().for_each(|p| *p = 0);
        self.st = State::default();
        self.stack.clear();
        self.path = PathBuilder::default();
        self.dirty = true;
    }

    /// Straight ARGB pixels for display (an opaque canvas has black under).
    pub fn display_pixels(&self) -> Vec<u32> {
        self.px
            .iter()
            .map(|&c| {
                let a = c >> 24;
                if !self.alpha {
                    // An opaque canvas: premultiplied color over black.
                    return c | 0xFF00_0000;
                }
                if a == 255 || a == 0 {
                    c
                } else {
                    let p = unpack(c);
                    let f = 1.0 / p[3];
                    (a << 24) | (pack([p[0] * f, p[1] * f, p[2] * f, 1.0]) & 0xFF_FFFF)
                }
            })
            .collect()
    }

    pub fn snapshot(&self) -> Bitmap {
        Bitmap { w: self.w, h: self.h, px: self.px.clone() }
    }

    /// RGBA bytes (unpremultiplied) of a region; outside the canvas is 0.
    pub fn get_image_data(&self, sx: i32, sy: i32, sw: usize, sh: usize) -> Vec<u8> {
        let mut out = vec![0u8; sw * sh * 4];
        for y in 0..sh {
            let py = sy + y as i32;
            if py < 0 || py as usize >= self.h {
                continue;
            }
            for x in 0..sw {
                let px = sx + x as i32;
                if px < 0 || px as usize >= self.w {
                    continue;
                }
                let c = self.px[py as usize * self.w + px as usize];
                let a = c >> 24;
                let o = (y * sw + x) * 4;
                if a == 0 {
                    continue;
                }
                let un = |s: u32| {
                    let v = (c >> s) & 0xFF;
                    if a == 255 { v as u8 } else { ((v * 255 + a / 2) / a).min(255) as u8 }
                };
                out[o] = un(16);
                out[o + 1] = un(8);
                out[o + 2] = un(0);
                out[o + 3] = a as u8;
            }
        }
        out
    }

    /// putImageData: raw copy of RGBA bytes (no compositing or clip).
    #[allow(clippy::too_many_arguments)]
    pub fn put_image_data(&mut self, data: &[u8], iw: usize, ih: usize, dx: i32, dy: i32, rect: [i32; 4]) {
        let [rx, ry, rw, rh] = rect;
        for y in ry.max(0)..(ry + rh).min(ih as i32) {
            for x in rx.max(0)..(rx + rw).min(iw as i32) {
                let (tx, ty) = (dx + x, dy + y);
                if tx < 0 || ty < 0 || tx as usize >= self.w || ty as usize >= self.h {
                    continue;
                }
                let o = (y as usize * iw + x as usize) * 4;
                let Some(p) = data.get(o..o + 4) else { continue };
                let a = p[3] as u32;
                let pm = |v: u8| (v as u32 * a + 127) / 255;
                self.px[ty as usize * self.w + tx as usize] = a << 24 | pm(p[0]) << 16 | pm(p[1]) << 8 | pm(p[2]);
            }
        }
        self.dirty = true;
    }

    // ------------------------------------------------------------ drawing

    fn shadowed(&self) -> bool {
        self.st.shadow_color >> 24 != 0 && (self.st.shadow_blur > 0.0 || self.st.shadow_x != 0.0 || self.st.shadow_y != 0.0)
    }

    /// The premultiplied color source for a paint at device pixels, with
    /// global alpha applied.
    fn sampler<'a>(&self, src: &'a Src<'a>) -> Box<dyn Fn(f32, f32) -> [f32; 4] + 'a> {
        let alpha = self.st.alpha;
        let inv = invert(&self.st.m).unwrap_or(ID);
        let smooth = self.st.smoothing;
        match src {
            Src::Paint(Paint::Color(c)) => {
                let p = premul(*c).map(|v| v * alpha);
                Box::new(move |_, _| p)
            }
            Src::Paint(Paint::Grad(g)) => {
                let g = g.borrow();
                if g.stops.is_empty() {
                    return Box::new(|_, _| [0.0; 4]);
                }
                let lut = g.lut(alpha);
                let kind = g.kind.clone();
                let at = move |t: f32| lut[(t.clamp(0.0, 1.0) * 255.0).round() as usize];
                match kind {
                    GradKind::Linear([x0, y0, x1, y1]) => {
                        let (dx, dy) = (x1 - x0, y1 - y0);
                        let l = dx * dx + dy * dy;
                        Box::new(move |x, y| {
                            if l == 0.0 {
                                return [0.0; 4];
                            }
                            let (ux, uy) = ap(&inv, x, y);
                            at(((ux - x0) * dx + (uy - y0) * dy) / l)
                        })
                    }
                    GradKind::Radial([x0, y0, r0, x1, y1, r1]) => Box::new(move |x, y| {
                        let (ux, uy) = ap(&inv, x, y);
                        let (cdx, cdy, dr) = (x1 - x0, y1 - y0, r1 - r0);
                        let (pdx, pdy) = (ux - x0, uy - y0);
                        let a = cdx * cdx + cdy * cdy - dr * dr;
                        let b = pdx * cdx + pdy * cdy + r0 * dr;
                        let c = pdx * pdx + pdy * pdy - r0 * r0;
                        let t = if a.abs() < 1e-6 {
                            if b == 0.0 {
                                return [0.0; 4];
                            }
                            c / (2.0 * b)
                        } else {
                            let disc = b * b - a * c;
                            if disc < 0.0 {
                                return [0.0; 4];
                            }
                            let sq = disc.sqrt();
                            let (t1, t2) = ((b + sq) / a, (b - sq) / a);
                            let (hi, lo) = if t1 > t2 { (t1, t2) } else { (t2, t1) };
                            if r0 + hi * dr >= 0.0 {
                                hi
                            } else if r0 + lo * dr >= 0.0 {
                                lo
                            } else {
                                return [0.0; 4];
                            }
                        };
                        at(t)
                    }),
                    GradKind::Conic([start, cx, cy]) => Box::new(move |x, y| {
                        let (ux, uy) = ap(&inv, x, y);
                        let a = (uy - cy).atan2(ux - cx) - start;
                        at(a.rem_euclid(2.0 * PI) / (2.0 * PI))
                    }),
                }
            }
            Src::Paint(Paint::Pattern(p)) => {
                let pm = mul(self.st.m, *p.m.borrow());
                let pinv = invert(&pm).unwrap_or(ID);
                let img = p.img.clone();
                let rep = p.rep;
                Box::new(move |x, y| {
                    let (mut u, mut v) = ap(&pinv, x, y);
                    let (w, h) = (img.w as f32, img.h as f32);
                    if w == 0.0 || h == 0.0 {
                        return [0.0; 4];
                    }
                    if rep.0 {
                        u = u.rem_euclid(w);
                    } else if !(0.0..w).contains(&u) {
                        return [0.0; 4];
                    }
                    if rep.1 {
                        v = v.rem_euclid(h);
                    } else if !(0.0..h).contains(&v) {
                        return [0.0; 4];
                    }
                    img.at(u as i32, v as i32).map(|c| c * alpha)
                })
            }
            Src::Image { bmp, inv, rect } => {
                let (bmp, inv, rect) = (*bmp, *inv, *rect);
                Box::new(move |x, y| {
                    let (u, v) = ap(&inv, x, y);
                    bmp.sample(u, v, smooth, rect).map(|c| c * alpha)
                })
            }
        }
    }

    /// Paint coverage `cov` with `src`, honoring clip, compositing,
    /// shadows and filters.
    fn draw(&mut self, cov: Option<Cov>, src: Src) {
        self.dirty = true;
        let op = self.st.op;
        if self.shadowed() || self.st.filter.is_some() {
            return self.draw_layered(cov, &src);
        }
        let sample = self.sampler(&src);
        let clip = self.st.clip.clone();
        let (w, h) = (self.w, self.h);
        let cov_at = |x: usize, y: usize| -> f32 {
            match &cov {
                Some(c) => {
                    let (lx, ly) = (x as i32 - c.x0, y as i32 - c.y0);
                    if lx < 0 || ly < 0 || lx as usize >= c.w || ly as usize >= c.h {
                        0.0
                    } else {
                        c.data[ly as usize * c.w + lx as usize]
                    }
                }
                None => 0.0,
            }
        };
        let (x0, y0, x1, y1) = if unbounded(op) {
            (0, 0, w, h)
        } else {
            match &cov {
                Some(c) => (c.x0.max(0) as usize, c.y0.max(0) as usize, (c.x0 as usize + c.w).min(w), (c.y0 as usize + c.h).min(h)),
                None => return,
            }
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let mut c = cov_at(x, y);
                if let Some(m) = &clip {
                    let k = m[y * w + x] as f32 / 255.0;
                    if k <= 0.0 {
                        continue;
                    }
                    c *= k;
                } else if c <= 0.0 && !unbounded(op) {
                    continue;
                }
                let i = y * w + x;
                let d = unpack(self.px[i]);
                let s = if c > 0.0 { sample(x as f32 + 0.5, y as f32 + 0.5) } else { [0.0; 4] };
                let r = if unbounded(op) {
                    // Outside the shape the source is transparent.
                    let full = composite(d, s, op);
                    if c >= 1.0 {
                        full
                    } else {
                        let out = composite(d, [0.0; 4], op);
                        [0, 1, 2, 3].map(|k| out[k] + (full[k] - out[k]) * c)
                    }
                } else if op == 0 {
                    [0, 1, 2, 3].map(|k| s[k] * c + d[k] * (1.0 - s[3] * c))
                } else {
                    let full = composite(d, s, op);
                    [0, 1, 2, 3].map(|k| d[k] + (full[k] - d[k]) * c)
                };
                self.px[i] = pack(r);
            }
        }
    }

    /// Drawing with a shadow and/or filter: render the shape to a layer,
    /// filter it, draw its shadow, then composite it.
    fn draw_layered(&mut self, cov: Option<Cov>, src: &Src) {
        let Some(cov) = cov else { return };
        let (w, h) = (self.w, self.h);
        let sample = self.sampler(src);
        let mut pad = 0.0f32;
        if let Some(f) = &self.st.filter {
            let o = crate::filter::outset(f);
            pad = o.iter().copied().fold(0.0, f32::max);
        }
        if self.shadowed() {
            pad = pad.max(self.st.shadow_blur * 1.5 + 2.0);
        }
        let pad = pad.ceil() as i32;
        let lx0 = (cov.x0 - pad).max(0);
        let ly0 = (cov.y0 - pad).max(0);
        let lx1 = (cov.x0 + cov.w as i32 + pad).min(w as i32);
        let ly1 = (cov.y0 + cov.h as i32 + pad).min(h as i32);
        if lx1 <= lx0 || ly1 <= ly0 {
            return;
        }
        let (lw, lh) = ((lx1 - lx0) as usize, (ly1 - ly0) as usize);
        let mut layer = vec![[0.0f32; 4]; lw * lh];
        for y in 0..cov.h {
            for x in 0..cov.w {
                let c = cov.data[y * cov.w + x];
                if c <= 0.0 {
                    continue;
                }
                let (dx, dy) = (cov.x0 + x as i32, cov.y0 + y as i32);
                if dx < lx0 || dy < ly0 || dx >= lx1 || dy >= ly1 {
                    continue;
                }
                let s = sample(dx as f32 + 0.5, dy as f32 + 0.5);
                layer[(dy - ly0) as usize * lw + (dx - lx0) as usize] = s.map(|v| v * c * 255.0);
            }
        }
        drop(sample);
        if let Some(f) = self.st.filter.clone() {
            crate::filter::apply(&mut layer, lw, lh, &f);
        }
        let op = self.st.op;
        let clip = self.st.clip.clone();
        let put = |canvas: &mut Canvas2D, img: &[[f32; 4]], ox: i32, oy: i32| {
            for y in 0..lh {
                for x in 0..lw {
                    let s = img[y * lw + x];
                    if s[3] <= 0.0 {
                        continue;
                    }
                    let (dx, dy) = (lx0 + x as i32 + ox, ly0 + y as i32 + oy);
                    if dx < 0 || dy < 0 || dx >= w as i32 || dy >= h as i32 {
                        continue;
                    }
                    let i = dy as usize * w + dx as usize;
                    let k = clip.as_ref().map_or(1.0, |m| m[i] as f32 / 255.0);
                    if k <= 0.0 {
                        continue;
                    }
                    let s = s.map(|v| v / 255.0);
                    let d = unpack(canvas.px[i]);
                    let full = composite(d, s, op);
                    canvas.px[i] = pack([0, 1, 2, 3].map(|j| d[j] + (full[j] - d[j]) * k));
                }
            }
        };
        if self.shadowed() {
            let sc = premul(self.st.shadow_color);
            let mut sh: Vec<[f32; 4]> = layer.iter().map(|p| sc.map(|v| v * p[3])).collect();
            crate::filter::blur(&mut sh, lw, lh, self.st.shadow_blur / 2.0);
            let (ox, oy) = (self.st.shadow_x.round() as i32, self.st.shadow_y.round() as i32);
            put(self, &sh, ox, oy);
        }
        put(self, &layer, 0, 0);
    }

    pub fn fill_path(&mut self, path: &PathBuilder, evenodd: bool) {
        let polys = path.polys();
        let cov = svg::rasterize(&polys, evenodd, self.w, self.h);
        let paint = self.st.fill.clone();
        self.draw(cov, Src::Paint(&paint));
    }

    /// The device-space outline polygons of stroking `path`.
    fn stroke_polys(&self, path: &PathBuilder) -> Vec<Vec<(f32, f32)>> {
        let st = &self.st;
        let mut out = Vec::new();
        if st.line_width <= 0.0 || !st.line_width.is_finite() {
            return out;
        }
        let Some(inv) = invert(&st.m) else { return out };
        let opts = StrokeOpts { sw: st.line_width, cap: st.cap, join: st.join, miter: st.miter };
        let tol = TOL / scale_of(&st.m);
        let dash: Vec<f32> = if st.dash.len() % 2 == 1 { st.dash.repeat(2) } else { st.dash.clone() };
        for s in &path.subs {
            if s.pts.len() < 2 && !(s.pts.len() == 1 && st.cap != 0 && false) {
                continue;
            }
            let user = Sub { pts: s.pts.iter().map(|&(x, y)| ap(&inv, x, y)).collect(), closed: s.closed };
            let pieces = if dash.is_empty() || dash.iter().all(|&d| d == 0.0) { vec![user] } else { svg::dashes(&user, &dash, st.dash_off) };
            let mut polys = Vec::new();
            for p in &pieces {
                svg::stroke_polys(p, &opts, tol, &mut polys);
            }
            for p in polys {
                out.push(p.into_iter().map(|(x, y)| ap(&st.m, x, y)).collect());
            }
        }
        out
    }

    pub fn stroke_path(&mut self, path: &PathBuilder) {
        let polys = self.stroke_polys(path);
        let cov = svg::rasterize(&polys, false, self.w, self.h);
        let paint = self.st.stroke.clone();
        self.draw(cov, Src::Paint(&paint));
    }

    pub fn clip_path(&mut self, path: &PathBuilder, evenodd: bool) {
        let mut mask = vec![0u8; self.w * self.h];
        if let Some(c) = svg::rasterize(&path.polys(), evenodd, self.w, self.h) {
            for y in 0..c.h {
                for x in 0..c.w {
                    let (dx, dy) = (c.x0 as usize + x, c.y0 as usize + y);
                    mask[dy * self.w + dx] = (c.data[y * c.w + x] * 255.0).round() as u8;
                }
            }
        }
        if let Some(old) = &self.st.clip {
            for (m, &o) in mask.iter_mut().zip(old.iter()) {
                *m = ((*m as u32 * o as u32 + 127) / 255) as u8;
            }
        }
        self.st.clip = Some(Rc::new(mask));
    }

    pub fn point_in_path(&self, path: &PathBuilder, x: f32, y: f32, evenodd: bool) -> bool {
        inside(&path.polys(), x, y, evenodd)
    }

    pub fn point_in_stroke(&self, path: &PathBuilder, x: f32, y: f32) -> bool {
        inside(&self.stroke_polys(path), x, y, false)
    }

    pub fn rect_path(&self, x: f32, y: f32, w: f32, h: f32) -> PathBuilder {
        let mut p = PathBuilder::default();
        p.rect(&self.st.m, x, y, w, h);
        p
    }

    pub fn clear_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let p = self.rect_path(x, y, w, h);
        let Some(c) = svg::rasterize(&p.polys(), false, self.w, self.h) else { return };
        for yy in 0..c.h {
            for xx in 0..c.w {
                let mut k = c.data[yy * c.w + xx];
                let i = (c.y0 as usize + yy) * self.w + c.x0 as usize + xx;
                if let Some(m) = &self.st.clip {
                    k *= m[i] as f32 / 255.0;
                }
                if k <= 0.0 {
                    continue;
                }
                let d = unpack(self.px[i]);
                self.px[i] = pack(d.map(|v| v * (1.0 - k)));
            }
        }
        self.dirty = true;
    }

    /// drawImage: the source rect (sx, sy, sw, sh) of `bmp` into the
    /// destination rect in user space.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image(&mut self, bmp: &Bitmap, sx: f32, sy: f32, sw: f32, sh: f32, dx: f32, dy: f32, dw: f32, dh: f32) {
        if sw == 0.0 || sh == 0.0 || dw == 0.0 || dh == 0.0 || bmp.w == 0 || bmp.h == 0 {
            return;
        }
        // Clip the source rect to the image (shrinking the destination).
        let (mut sx, mut sy, mut sw, mut sh, mut dx, mut dy, mut dw, mut dh) = (sx, sy, sw, sh, dx, dy, dw, dh);
        if sw < 0.0 {
            sx += sw;
            sw = -sw;
        }
        if sh < 0.0 {
            sy += sh;
            sh = -sh;
        }
        if dw < 0.0 {
            dx += dw;
            dw = -dw;
        }
        if dh < 0.0 {
            dy += dh;
            dh = -dh;
        }
        let (fx, fy) = (dw / sw, dh / sh);
        let (iw, ih) = (bmp.w as f32, bmp.h as f32);
        if sx < 0.0 {
            dx -= sx * fx;
            dw += sx * fx;
            sw += sx;
            sx = 0.0;
        }
        if sy < 0.0 {
            dy -= sy * fy;
            dh += sy * fy;
            sh += sy;
            sy = 0.0;
        }
        if sx + sw > iw {
            dw -= (sx + sw - iw) * fx;
            sw = iw - sx;
        }
        if sy + sh > ih {
            dh -= (sy + sh - ih) * fy;
            sh = ih - sy;
        }
        if sw <= 0.0 || sh <= 0.0 || dw <= 0.0 || dh <= 0.0 {
            return;
        }
        let p = self.rect_path(dx, dy, dw, dh);
        let cov = svg::rasterize(&p.polys(), false, self.w, self.h);
        // Device -> source pixels.
        let img_m = mul(self.st.m, [dw / sw, 0.0, 0.0, dh / sh, dx - sx * dw / sw, dy - sy * dh / sh]);
        let Some(inv) = invert(&img_m) else { return };
        self.draw(cov, Src::Image { bmp, inv, rect: [sx, sy, sw, sh] });
    }

    // --------------------------------------------------------------- text

    /// Width of `text` in the current font (user units, letter spacing
    /// included).
    pub fn text_width(&self, text: &str) -> f32 {
        let f = crate::fonts::fonts();
        let font = &self.st.font;
        let n = text.chars().count();
        text.chars().map(|c| f.adv(font.face, c).adv * font.size).sum::<f32>() + self.st.letter_spacing * n as f32
    }

    /// (ascent, descent) of the font in px.
    fn font_metrics(&self) -> (f32, f32) {
        let (a, d, _) = crate::fonts::fonts().metrics(self.st.font.face);
        (a * self.st.font.size, d * self.st.font.size)
    }

    /// Offset from the given y to the alphabetic baseline.
    fn baseline_shift(&self) -> f32 {
        let (a, d) = self.font_metrics();
        match self.st.baseline {
            1 | 2 => a,            // top, hanging
            3 => (a - d) / 2.0,    // middle
            4 | 5 => -d,           // ideographic, bottom
            _ => 0.0,              // alphabetic
        }
    }

    /// Coverage of a text run in device space.
    fn text_cov(&self, text: &str, x: f32, y: f32, max_width: Option<f32>) -> Option<Cov> {
        let f = crate::fonts::fonts();
        let font = self.st.font.clone();
        if font.size <= 0.0 || text.is_empty() {
            return None;
        }
        let text: String = text.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).collect();
        let width = self.text_width(&text);
        let mut kx = 1.0;
        if let Some(mw) = max_width {
            if mw <= 0.0 || !mw.is_finite() {
                return None;
            }
            if width > mw {
                kx = mw / width;
            }
        }
        let rtl = self.st.rtl;
        let ax = match (self.st.align, rtl) {
            (2, _) => -width * kx / 2.0,                       // center
            (3, _) | (1, true) | (4, false) => -width * kx,    // right, start in rtl, end in ltr
            _ => 0.0,                                          // left, start (ltr), end (rtl)
        };
        let by = y + self.baseline_shift();
        let tm = mul(self.st.m, [kx, 0.0, 0.0, 1.0, x + ax - x * kx, 0.0]);
        let Some(inv) = invert(&tm) else { return None };
        let s = scale_of(&tm);
        let px = font.size * s;
        // Glyph bitmaps at device resolution, positioned in user space.
        struct G {
            pen: f32,
            xmin: f32,
            ymin: f32,
            w: usize,
            h: usize,
            data: Vec<u8>,
        }
        let mut glyphs = Vec::new();
        let mut pen = x;
        for c in text.chars() {
            let a = f.adv(font.face, c);
            if c != ' ' {
                if let Some(fd) = f.face(a.face) {
                    let (m, data) = fd.font.rasterize(c, px);
                    if m.width > 0 && m.height > 0 {
                        glyphs.push(G { pen, xmin: m.xmin as f32, ymin: m.ymin as f32, w: m.width, h: m.height, data });
                    }
                }
            }
            pen += a.adv * font.size + self.st.letter_spacing;
        }
        if glyphs.is_empty() {
            return None;
        }
        // Device bounding box.
        let (mut bx0, mut by0, mut bx1, mut by1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for g in &glyphs {
            let (gx0, gy1) = (g.pen + g.xmin / s, by - g.ymin / s);
            let (gx1, gy0) = (gx0 + g.w as f32 / s, gy1 - g.h as f32 / s);
            for (ux, uy) in [(gx0, gy0), (gx1, gy0), (gx0, gy1), (gx1, gy1)] {
                let (dx, dy) = ap(&tm, ux, uy);
                bx0 = bx0.min(dx);
                by0 = by0.min(dy);
                bx1 = bx1.max(dx);
                by1 = by1.max(dy);
            }
        }
        let x0 = (bx0.floor() as i32 - 1).max(0);
        let y0 = (by0.floor() as i32 - 1).max(0);
        let x1 = (bx1.ceil() as i32 + 1).min(self.w as i32);
        let y1 = (by1.ceil() as i32 + 1).min(self.h as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let (cw, ch) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut data = vec![0f32; cw * ch];
        let axis = tm[1] == 0.0 && tm[2] == 0.0 && tm[0] > 0.0 && (tm[0] - tm[3]).abs() < 1e-4;
        for g in &glyphs {
            if axis {
                // Axis-aligned: blit at the rounded device position.
                let (ox, oy) = ap(&tm, g.pen, by);
                let gx = (ox + g.xmin).round() as i32;
                let gy = (oy - g.ymin - g.h as f32).round() as i32;
                for r in 0..g.h {
                    let dy = gy + r as i32;
                    if dy < y0 || dy >= y1 {
                        continue;
                    }
                    for c in 0..g.w {
                        let dx = gx + c as i32;
                        if dx < x0 || dx >= x1 {
                            continue;
                        }
                        let v = g.data[r * g.w + c] as f32 / 255.0;
                        let o = &mut data[(dy - y0) as usize * cw + (dx - x0) as usize];
                        *o = (*o + v).min(1.0);
                    }
                }
                continue;
            }
            // General transform: sample the glyph bitmap per device pixel.
            let at = |u: i32, v: i32| -> f32 {
                if u < 0 || v < 0 || u as usize >= g.w || v as usize >= g.h {
                    0.0
                } else {
                    g.data[v as usize * g.w + u as usize] as f32 / 255.0
                }
            };
            for dy in y0..y1 {
                for dx in x0..x1 {
                    let (ux, uy) = ap(&inv, dx as f32 + 0.5, dy as f32 + 0.5);
                    let u = (ux - g.pen) * s - g.xmin - 0.5;
                    let v = (uy - by) * s + g.ymin + g.h as f32 - 0.5;
                    if u < -1.0 || v < -1.0 || u > g.w as f32 || v > g.h as f32 {
                        continue;
                    }
                    let (iu, iv) = (u.floor() as i32, v.floor() as i32);
                    let (tu, tv) = (u - iu as f32, v - iv as f32);
                    let val = (at(iu, iv) * (1.0 - tu) + at(iu + 1, iv) * tu) * (1.0 - tv) + (at(iu, iv + 1) * (1.0 - tu) + at(iu + 1, iv + 1) * tu) * tv;
                    if val > 0.0 {
                        let o = &mut data[(dy - y0) as usize * cw + (dx - x0) as usize];
                        *o = (*o + val).min(1.0);
                    }
                }
            }
        }
        Some(Cov { x0, y0, w: cw, h: ch, data })
    }

    pub fn fill_text(&mut self, text: &str, x: f32, y: f32, max_width: Option<f32>) {
        let cov = self.text_cov(text, x, y, max_width);
        let paint = self.st.fill.clone();
        self.draw(cov, Src::Paint(&paint));
    }

    /// strokeText: the outline of the glyph shapes (dilated minus eroded
    /// coverage, `lineWidth` wide).
    pub fn stroke_text(&mut self, text: &str, x: f32, y: f32, max_width: Option<f32>) {
        let Some(c) = self.text_cov(text, x, y, max_width) else { return };
        let r = ((self.st.line_width * scale_of(&self.st.m)) / 2.0).max(1.0);
        let ri = r.ceil() as i32;
        let (w, h) = (c.w as i32 + 2 * ri, c.h as i32 + 2 * ri);
        let get = |x: i32, y: i32| -> f32 {
            let (lx, ly) = (x - ri, y - ri);
            if lx < 0 || ly < 0 || lx >= c.w as i32 || ly >= c.h as i32 {
                0.0
            } else {
                c.data[(ly * c.w as i32 + lx) as usize]
            }
        };
        let mut out = vec![0f32; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let (mut hi, mut lo) = (0.0f32, 1.0f32);
                for dy in -ri..=ri {
                    for dx in -ri..=ri {
                        if (dx * dx + dy * dy) as f32 > r * r + 0.5 {
                            continue;
                        }
                        let v = get(x + dx, y + dy);
                        hi = hi.max(v);
                        lo = lo.min(v);
                    }
                }
                out[(y * w + x) as usize] = (hi - lo).clamp(0.0, 1.0);
            }
        }
        let x0 = c.x0 - ri;
        let y0 = c.y0 - ri;
        // Re-clip to the canvas.
        let (cx0, cy0) = (x0.max(0), y0.max(0));
        let (cx1, cy1) = ((x0 + w).min(self.w as i32), (y0 + h).min(self.h as i32));
        if cx1 <= cx0 || cy1 <= cy0 {
            return;
        }
        let (cw, chh) = ((cx1 - cx0) as usize, (cy1 - cy0) as usize);
        let mut data = vec![0f32; cw * chh];
        for y in 0..chh {
            for x in 0..cw {
                data[y * cw + x] = out[((y as i32 + cy0 - y0) * w + (x as i32 + cx0 - x0)) as usize];
            }
        }
        let paint = self.st.stroke.clone();
        self.draw(Some(Cov { x0: cx0, y0: cy0, w: cw, h: chh, data }), Src::Paint(&paint));
    }

    /// TextMetrics values: width, actual bounding box (left, right,
    /// ascent, descent), font bounding box (ascent, descent).
    pub fn measure(&self, text: &str) -> [f32; 7] {
        let f = crate::fonts::fonts();
        let font = &self.st.font;
        let width = self.text_width(text);
        let (fa, fd) = self.font_metrics();
        let shift = self.baseline_shift();
        let (mut asc, mut desc, mut left, mut right) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut pen = 0.0;
        let mut first = true;
        for c in text.chars() {
            let a = f.adv(font.face, c);
            if let Some(face) = f.face(a.face) {
                let m = face.font.metrics(c, font.size);
                if m.width > 0 {
                    asc = asc.max(m.ymin as f32 + m.height as f32);
                    desc = desc.max(-(m.ymin as f32));
                    if first {
                        left = -(m.xmin as f32);
                        first = false;
                    }
                    right = right.max(pen + m.xmin as f32 + m.width as f32);
                }
            }
            pen += a.adv * font.size + self.st.letter_spacing;
        }
        let ax = match self.st.align {
            2 => width / 2.0,
            3 | 4 => width,
            _ => 0.0,
        };
        [width, left + ax, right - ax, asc - shift, desc + shift, fa - shift, fd + shift]
    }
}

// ================================================================ natives

/// The host data behind canvas handles.
pub enum Host {
    Ctx(Rc<RefCell<Canvas2D>>),
    Grad(Rc<RefCell<Gradient>>),
    Pattern(Rc<Pattern>),
    Path(Rc<RefCell<PathBuilder>>),
    Bitmap(Rc<Bitmap>),
}

fn host(v: &Value) -> Option<Rc<Host>> {
    let o = v.obj()?;
    let b = o.borrow();
    match &b.kind {
        Kind::Host(h) => h.clone().downcast::<Host>().ok(),
        _ => None,
    }
}

fn wrap(it: &Interp, h: Host) -> Value {
    let rc: Rc<dyn std::any::Any> = Rc::new(h);
    Value::Obj(it.new_obj(it.p.object.clone(), Kind::Host(rc)))
}

fn ctx_of(v: &Value) -> R<Rc<RefCell<Canvas2D>>> {
    match host(v).as_deref() {
        Some(Host::Ctx(c)) => Ok(c.clone()),
        _ => Err(type_error("Illegal invocation")),
    }
}

fn num(it: &mut Interp, a: &[Value], i: usize) -> R<f32> {
    Ok(it.to_num(&arg(a, i))? as f32)
}

/// A CSS color string (canvas colors: currentColor is black).
pub fn parse_color(s: &str) -> Option<u32> {
    let t = s.trim().to_ascii_lowercase();
    if t == "currentcolor" {
        return Some(0xFF00_0000);
    }
    crate::css::parse_color(&t)
}

/// A color as canvas serializes it: #rrggbb when opaque, else rgba().
pub fn color_string(c: u32) -> String {
    let a = c >> 24;
    let (r, g, b) = ((c >> 16) & 255, (c >> 8) & 255, c & 255);
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        let af = a as f64 / 255.0;
        // Shortest decimal that round-trips the 8-bit alpha.
        let mut s = String::new();
        for p in 1..=4 {
            let v = (af * 10f64.powi(p)).round() / 10f64.powi(p);
            if ((v * 255.0).round() as u32) == a {
                s = crate::js::num_to_str(v);
                break;
            }
        }
        if s.is_empty() {
            s = crate::js::num_to_str(af);
        }
        format!("rgba({r}, {g}, {b}, {s})")
    }
}

/// The bitmap of a drawImage / createPattern source: an <img>, a canvas
/// (or OffscreenCanvas / ImageBitmap handle) — None if not ready.
fn source_bitmap(it: &mut Interp, v: &Value) -> R<Option<Rc<Bitmap>>> {
    match host(v).as_deref() {
        Some(Host::Bitmap(b)) => return Ok(Some(b.clone())),
        Some(Host::Ctx(c)) => return Ok(Some(Rc::new(c.borrow().snapshot()))),
        _ => {}
    }
    let Some(n) = super::dombind::node_id(v) else {
        // An object wrapping a handle (OffscreenCanvas / ImageBitmap).
        if let Value::Obj(_) = v {
            let h = it.get(v, "@@h")?;
            if !matches!(h, Value::Undef) {
                return source_bitmap(it, &h);
            }
        }
        return Err(type_error("Failed to execute 'drawImage': The provided value is not of a supported image type."));
    };
    match it.doc.tag(n) {
        Some(crate::atom::k::canvas) => Ok(it.canvases.get(&n).map(|c| Rc::new(c.borrow().snapshot())).or_else(|| Some(Rc::new(Bitmap { w: 0, h: 0, px: Vec::new() })))),
        Some(crate::atom::k::img) => {
            let src = it.doc.attr(n, crate::atom::k::src).map(|s| crate::net::resolve(&it.url, s));
            let img = src.and_then(|u| it.images.get(&u).cloned().flatten());
            Ok(img.map(|i| bitmap_cached(&i)))
        }
        _ => Ok(None),
    }
}

thread_local! {
    /// Premultiplied copies of decoded images, by image address.
    static BITMAPS: RefCell<Vec<(usize, std::sync::Arc<crate::image::Image>, Rc<Bitmap>)>> = const { RefCell::new(Vec::new()) };
}

fn bitmap_cached(img: &std::sync::Arc<crate::image::Image>) -> Rc<Bitmap> {
    let key = std::sync::Arc::as_ptr(img) as usize;
    BITMAPS.with(|b| {
        let mut b = b.borrow_mut();
        if let Some(e) = b.iter().find(|e| e.0 == key) {
            return e.2.clone();
        }
        let bm = Rc::new(Bitmap::from_image(img));
        if b.len() > 32 {
            b.remove(0);
        }
        b.push((key, img.clone(), bm.clone()));
        bm
    })
}

/// `@@canvasCtx(canvasElement | null, w, h)`: the context of a canvas
/// element (created once), or a detached one for OffscreenCanvas.
fn new_ctx(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let w = num(it, a, 1)?.max(0.0) as usize;
    let h = num(it, a, 2)?.max(0.0) as usize;
    let c = match super::dombind::node_id(&arg(a, 0)) {
        Some(n) => it.canvases.entry(n).or_insert_with(|| Rc::new(RefCell::new(Canvas2D::new(w, h)))).clone(),
        None => Rc::new(RefCell::new(Canvas2D::new(w, h))),
    };
    if !arg(a, 3).truthy() && matches!(arg(a, 3), Value::Bool(false)) {
        c.borrow_mut().alpha = false;
    }
    Ok(wrap(it, Host::Ctx(c)))
}

/// `@@canvasResize(ctx, w, h)`
fn resize(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let c = ctx_of(&arg(a, 0))?;
    let (w, h) = (num(it, a, 1)?.max(0.0) as usize, num(it, a, 2)?.max(0.0) as usize);
    c.borrow_mut().resize(w, h);
    Ok(Value::Undef)
}

fn path_of(v: &Value) -> Option<Rc<RefCell<PathBuilder>>> {
    match host(v).as_deref() {
        Some(Host::Path(p)) => Some(p.clone()),
        _ => None,
    }
}

fn fill_rule(it: &mut Interp, v: &Value) -> R<bool> {
    Ok(matches!(v, Value::Str(_)) && &*it.to_str(v)? == "evenodd")
}

/// Corner radii for roundRect: a number, DOMPointInit or a list of 1-4.
fn radii(it: &mut Interp, v: &Value) -> R<[(f32, f32); 4]> {
    let one = |it: &mut Interp, v: &Value| -> R<(f32, f32)> {
        if let Value::Obj(_) = v {
            let x = it.get(v, "x")?;
            let y = it.get(v, "y")?;
            let (x, y) = (it.to_num(&x)? as f32, it.to_num(&y)? as f32);
            return Ok((x, y));
        }
        let r = it.to_num(v)? as f32;
        Ok((r, r))
    };
    let list: Vec<Value> = match v {
        Value::Undef => vec![Value::Num(0.0)],
        Value::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => it.iterate(v)?,
        v => vec![v.clone()],
    };
    let r: Vec<(f32, f32)> = list.iter().map(|x| one(it, x)).collect::<R<_>>()?;
    if r.is_empty() || r.len() > 4 || r.iter().any(|&(x, y)| x < 0.0 || y < 0.0 || !x.is_finite() || !y.is_finite()) {
        return Err(super::range_error("Failed to execute 'roundRect': The radii provided are invalid."));
    }
    Ok(match r.len() {
        1 => [r[0]; 4],
        2 => [r[0], r[1], r[0], r[1]],
        3 => [r[0], r[1], r[2], r[1]],
        _ => [r[0], r[1], r[2], r[3]],
    })
}

/// Path commands shared by the context and Path2D: (path, transform).
fn path_cmd(it: &mut Interp, name: &str, p: &mut PathBuilder, m: &M, a: &[Value]) -> R<bool> {
    let n = |it: &mut Interp, i: usize| num(it, a, i);
    match name {
        "moveTo" => {
            let (x, y) = (n(it, 0)?, n(it, 1)?);
            p.move_to(m, x, y)
        }
        "lineTo" => {
            let (x, y) = (n(it, 0)?, n(it, 1)?);
            p.line_to(m, x, y)
        }
        "quadraticCurveTo" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?];
            p.quad_to(m, v[0], v[1], v[2], v[3])
        }
        "bezierCurveTo" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?, n(it, 4)?, n(it, 5)?];
            p.cubic_to(m, v[0], v[1], v[2], v[3], v[4], v[5])
        }
        "arc" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?, n(it, 4)?];
            if v[2] < 0.0 {
                return Err(make_dom_error("IndexSizeError", "The radius provided is negative."));
            }
            p.ellipse(m, v[0], v[1], v[2], v[2], 0.0, v[3], v[4], arg(a, 5).truthy())
        }
        "arcTo" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?, n(it, 4)?];
            if v[4] < 0.0 {
                return Err(make_dom_error("IndexSizeError", "The radius provided is negative."));
            }
            p.arc_to(m, v[0], v[1], v[2], v[3], v[4])
        }
        "ellipse" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?, n(it, 4)?, n(it, 5)?, n(it, 6)?];
            if v[2] < 0.0 || v[3] < 0.0 {
                return Err(make_dom_error("IndexSizeError", "The radius provided is negative."));
            }
            p.ellipse(m, v[0], v[1], v[2], v[3], v[4], v[5], v[6], arg(a, 7).truthy())
        }
        "rect" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?];
            p.rect(m, v[0], v[1], v[2], v[3])
        }
        "roundRect" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?];
            let r = radii(it, &arg(a, 4))?;
            p.round_rect(m, v[0], v[1], v[2], v[3], r)
        }
        "closePath" => p.close(),
        _ => return Ok(false),
    }
    Ok(true)
}

fn make_dom_error(name: &str, msg: &str) -> Value {
    super::builtins::make_error_value(name, msg)
}

/// `@@canvasCall(ctx, name, ...args)`: a context method.
fn call(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let cv = ctx_of(&arg(a, 0))?;
    let name = it.to_str(&arg(a, 1))?;
    let a = if a.len() > 2 { &a[2..] } else { &[] };
    let n = |it: &mut Interp, i: usize| num(it, a, i);
    {
        let mut c = cv.borrow_mut();
        let m = c.st.m;
        let mut path = std::mem::take(&mut c.path);
        drop(c);
        let done = path_cmd(it, &name, &mut path, &m, a);
        cv.borrow_mut().path = path;
        if done? {
            return Ok(Value::Undef);
        }
    }
    match &*name {
        "save" => {
            let mut c = cv.borrow_mut();
            let s = c.st.clone();
            if c.stack.len() < 1024 {
                c.stack.push(s);
            }
        }
        "restore" => {
            let mut c = cv.borrow_mut();
            if let Some(s) = c.stack.pop() {
                c.st = s;
            }
        }
        "reset" => cv.borrow_mut().reset(),
        "beginPath" => cv.borrow_mut().path = PathBuilder::default(),
        "fill" | "stroke" | "clip" => {
            // (path?, rule?)
            let (path, rule) = match path_of(&arg(a, 0)) {
                Some(p) => (Some(p), arg(a, 1)),
                None => (None, arg(a, 0)),
            };
            let evenodd = fill_rule(it, &rule)?;
            let mut c = cv.borrow_mut();
            let p = match &path {
                Some(p) => {
                    let mut q = PathBuilder::default();
                    q.add(&p.borrow(), &c.st.m);
                    q
                }
                None => c.path.clone(),
            };
            match &*name {
                "fill" => c.fill_path(&p, evenodd),
                "stroke" => c.stroke_path(&p),
                _ => c.clip_path(&p, evenodd),
            }
        }
        "isPointInPath" | "isPointInStroke" => {
            let (path, rest) = match path_of(&arg(a, 0)) {
                Some(p) => (Some(p), &a[1..]),
                None => (None, a),
            };
            let x = num(it, rest, 0)?;
            let y = num(it, rest, 1)?;
            let evenodd = fill_rule(it, &arg(rest, 2))?;
            let c = cv.borrow();
            let p = match &path {
                Some(p) => {
                    let mut q = PathBuilder::default();
                    q.add(&p.borrow(), &c.st.m);
                    q
                }
                None => c.path.clone(),
            };
            return Ok(Value::Bool(if &*name == "isPointInPath" { c.point_in_path(&p, x, y, evenodd) } else { c.point_in_stroke(&p, x, y) }));
        }
        "fillRect" | "strokeRect" | "clearRect" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?];
            if !v.iter().all(|x| x.is_finite()) {
                return Ok(Value::Undef);
            }
            let mut c = cv.borrow_mut();
            match &*name {
                "fillRect" => {
                    let p = c.rect_path(v[0], v[1], v[2], v[3]);
                    c.fill_path(&p, false);
                }
                "strokeRect" => {
                    let mut p = PathBuilder::default();
                    let m = c.st.m;
                    if v[2] == 0.0 || v[3] == 0.0 {
                        p.move_to(&m, v[0], v[1]);
                        p.line_to(&m, v[0] + v[2], v[1] + v[3]);
                    } else {
                        p.rect(&m, v[0], v[1], v[2], v[3]);
                    }
                    c.stroke_path(&p);
                }
                _ => c.clear_rect(v[0], v[1], v[2], v[3]),
            }
        }
        "fillText" | "strokeText" => {
            let text = it.to_str(&arg(a, 0))?.to_string();
            let (x, y) = (n(it, 1)?, n(it, 2)?);
            let mw = match arg(a, 3) {
                Value::Undef => None,
                v => Some(it.to_num(&v)? as f32),
            };
            if !(x.is_finite() && y.is_finite()) {
                return Ok(Value::Undef);
            }
            let mut c = cv.borrow_mut();
            if &*name == "fillText" {
                c.fill_text(&text, x, y, mw);
            } else {
                c.stroke_text(&text, x, y, mw);
            }
        }
        "measureText" => {
            let text = it.to_str(&arg(a, 0))?.to_string();
            let v = cv.borrow().measure(&text);
            return Ok(it.array(v.iter().map(|&x| Value::Num(x as f64)).collect()));
        }
        "drawImage" => {
            let Some(bmp) = source_bitmap(it, &arg(a, 0))? else { return Ok(Value::Undef) };
            let (iw, ih) = (bmp.w as f32, bmp.h as f32);
            let nums: Vec<f32> = (1..a.len()).map(|i| num(it, a, i)).collect::<R<_>>()?;
            if nums.iter().any(|v| !v.is_finite()) {
                return Ok(Value::Undef);
            }
            let (nw, nh) = (iw, ih);
            let (sx, sy, sw, sh, dx, dy, dw, dh) = match nums.len() {
                2 => (0.0, 0.0, iw, ih, nums[0], nums[1], nw, nh),
                4 => (0.0, 0.0, iw, ih, nums[0], nums[1], nums[2], nums[3]),
                n if n >= 8 => (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5], nums[6], nums[7]),
                _ => return Err(type_error("Failed to execute 'drawImage': Valid arities are: [3, 5, 9].")),
            };
            cv.borrow_mut().draw_image(&bmp, sx, sy, sw, sh, dx, dy, dw, dh);
        }
        "getImageData" => {
            let v = [n(it, 0)?, n(it, 1)?, n(it, 2)?, n(it, 3)?];
            if v[2] == 0.0 || v[3] == 0.0 || !v.iter().all(|x| x.is_finite()) {
                return Err(make_dom_error("IndexSizeError", "The source width is 0."));
            }
            let (mut x, mut y, mut w, mut h) = (v[0] as i32, v[1] as i32, v[2] as i32, v[3] as i32);
            if w < 0 {
                x += w;
                w = -w;
            }
            if h < 0 {
                y += h;
                h = -h;
            }
            if (w as u64) * (h as u64) > 1 << 28 {
                return Err(super::range_error("Out of memory at ImageData creation"));
            }
            let data = cv.borrow().get_image_data(x, y, w as usize, h as usize);
            let arr = super::binary::new_buffer(it, data);
            let len = (w * h * 4) as usize;
            let ta = super::binary::new_typed(it, super::binary::Ta::U8C, arr, 0, len);
            return Ok(it.array(vec![ta, Value::Num(w as f64), Value::Num(h as f64)]));
        }
        "putImageData" => {
            // (data, width, height, dx, dy, dirtyX, dirtyY, dirtyW, dirtyH)
            let bytes = super::binary::bytes_of(&arg(a, 0)).unwrap_or_default();
            let (iw, ih) = (n(it, 1)? as usize, n(it, 2)? as usize);
            let (dx, dy) = (n(it, 3)? as i32, n(it, 4)? as i32);
            let rect = if a.len() >= 9 {
                let (mut rx, mut ry, mut rw, mut rh) = (n(it, 5)? as i32, n(it, 6)? as i32, n(it, 7)? as i32, n(it, 8)? as i32);
                if rw < 0 {
                    rx += rw;
                    rw = -rw;
                }
                if rh < 0 {
                    ry += rh;
                    rh = -rh;
                }
                [rx, ry, rw, rh]
            } else {
                [0, 0, iw as i32, ih as i32]
            };
            cv.borrow_mut().put_image_data(&bytes, iw, ih, dx, dy, rect);
        }
        "transform" | "setTransform" | "translate" | "scale" | "rotate" | "resetTransform" => {
            let v: Vec<f32> = (0..6).map(|i| n(it, i)).collect::<R<_>>()?;
            let mut c = cv.borrow_mut();
            let t: M = match &*name {
                "translate" => [1.0, 0.0, 0.0, 1.0, v[0], v[1]],
                "scale" => [v[0], 0.0, 0.0, v[1], 0.0, 0.0],
                "rotate" => {
                    let (s, co) = v[0].sin_cos();
                    [co, s, -s, co, 0.0, 0.0]
                }
                _ => [v[0], v[1], v[2], v[3], v[4], v[5]],
            };
            let used = match &*name {
                "translate" | "scale" => &v[..2],
                "rotate" => &v[..1],
                "resetTransform" => &v[..0],
                _ => &v[..],
            };
            if used.iter().any(|x| !x.is_finite()) {
                return Ok(Value::Undef);
            }
            match &*name {
                "setTransform" => c.st.m = t,
                "resetTransform" => c.st.m = ID,
                _ => c.st.m = mul(c.st.m, t),
            }
        }
        "getTransform" => {
            let m = cv.borrow().st.m;
            return Ok(it.array(m.iter().map(|&x| Value::Num(x as f64)).collect()));
        }
        "setLineDash" => {
            let items = it.iterate(&arg(a, 0))?;
            let mut d = Vec::new();
            for v in &items {
                let x = it.to_num(v)? as f32;
                if !x.is_finite() || x < 0.0 {
                    return Ok(Value::Undef);
                }
                d.push(x);
            }
            if d.len() % 2 == 1 {
                d = d.repeat(2);
            }
            cv.borrow_mut().st.dash = d;
        }
        "getLineDash" => {
            let d = cv.borrow().st.dash.clone();
            return Ok(it.array(d.iter().map(|&x| Value::Num(x as f64)).collect()));
        }
        "toDataURL" | "pixels" => {
            let c = cv.borrow();
            let data = c.get_image_data(0, 0, c.w, c.h);
            let png = encode_png(c.w, c.h, &data);
            if &*name == "pixels" {
                drop(c);
                return Ok(super::binary::uint8_array(it, png));
            }
            return Ok(Value::str(&format!("data:image/png;base64,{}", super::builtins::base64_encode(&png))));
        }
        "size" => {
            let c = cv.borrow();
            return Ok(it.array(vec![Value::Num(c.w as f64), Value::Num(c.h as f64)]));
        }
        _ => return Err(type_error(format!("canvas: unknown method {name}"))),
    }
    Ok(Value::Undef)
}

/// `@@canvasSet(ctx, prop, value)`: set a state property; returns the
/// value the getter should now report (undefined: ignored).
fn set(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let cv = ctx_of(&arg(a, 0))?;
    let prop = it.to_str(&arg(a, 1))?;
    let v = arg(a, 2);
    let mut c = cv.borrow_mut();
    let st = &mut c.st;
    let f = |it: &mut Interp, v: &Value| -> R<Option<f32>> {
        let x = it.to_num(v)? as f32;
        Ok(x.is_finite().then_some(x))
    };
    let s = |it: &mut Interp, v: &Value| -> R<String> { Ok(it.to_str(v)?.to_string()) };
    match &*prop {
        "fillStyle" | "strokeStyle" => {
            let p = match host(&v).as_deref() {
                Some(Host::Grad(g)) => Some(Paint::Grad(g.clone())),
                Some(Host::Pattern(p)) => Some(Paint::Pattern(p.clone())),
                _ if matches!(v, Value::Obj(_)) => None,
                _ => parse_color(&s(it, &v)?).map(Paint::Color),
            };
            let Some(p) = p else { return Ok(Value::Undef) };
            let out = match &p {
                Paint::Color(c) => Value::str(&color_string(*c)),
                _ => v.clone(),
            };
            if &*prop == "fillStyle" {
                st.fill = p;
            } else {
                st.stroke = p;
            }
            return Ok(out);
        }
        "lineWidth" => match f(it, &v)? {
            Some(x) if x > 0.0 => st.line_width = x,
            _ => return Ok(Value::Undef),
        },
        "miterLimit" => match f(it, &v)? {
            Some(x) if x > 0.0 => st.miter = x,
            _ => return Ok(Value::Undef),
        },
        "lineDashOffset" => match f(it, &v)? {
            Some(x) => st.dash_off = x,
            None => return Ok(Value::Undef),
        },
        "globalAlpha" => match f(it, &v)? {
            Some(x) if (0.0..=1.0).contains(&x) => st.alpha = x,
            _ => return Ok(Value::Undef),
        },
        "shadowBlur" => match f(it, &v)? {
            Some(x) if x >= 0.0 => st.shadow_blur = x,
            _ => return Ok(Value::Undef),
        },
        "shadowOffsetX" | "shadowOffsetY" => match f(it, &v)? {
            Some(x) => {
                if &*prop == "shadowOffsetX" {
                    st.shadow_x = x
                } else {
                    st.shadow_y = x
                }
            }
            None => return Ok(Value::Undef),
        },
        "shadowColor" => match parse_color(&s(it, &v)?) {
            Some(col) => {
                st.shadow_color = col;
                return Ok(Value::str(&color_string(col)));
            }
            None => return Ok(Value::Undef),
        },
        "lineCap" => match &*s(it, &v)? {
            "butt" => st.cap = 0,
            "round" => st.cap = 1,
            "square" => st.cap = 2,
            _ => return Ok(Value::Undef),
        },
        "lineJoin" => match &*s(it, &v)? {
            "miter" => st.join = 0,
            "round" => st.join = 1,
            "bevel" => st.join = 2,
            _ => return Ok(Value::Undef),
        },
        "globalCompositeOperation" => {
            let x = s(it, &v)?;
            match OPS.iter().position(|o| *o == x) {
                Some(i) => st.op = i as u8,
                None => return Ok(Value::Undef),
            }
        }
        "font" => match FontSpec::parse(&s(it, &v)?) {
            Some(fs) => {
                let css = fs.css.clone();
                st.font = fs;
                return Ok(Value::str(&css));
            }
            None => return Ok(Value::Undef),
        },
        "textAlign" => match &*s(it, &v)? {
            "start" => st.align = 0,
            "left" => st.align = 1,
            "center" => st.align = 2,
            "right" => st.align = 3,
            "end" => st.align = 4,
            _ => return Ok(Value::Undef),
        },
        "textBaseline" => match &*s(it, &v)? {
            "alphabetic" => st.baseline = 0,
            "top" => st.baseline = 1,
            "hanging" => st.baseline = 2,
            "middle" => st.baseline = 3,
            "ideographic" => st.baseline = 4,
            "bottom" => st.baseline = 5,
            _ => return Ok(Value::Undef),
        },
        "direction" => match &*s(it, &v)? {
            "ltr" | "inherit" => st.rtl = false,
            "rtl" => st.rtl = true,
            _ => return Ok(Value::Undef),
        },
        "imageSmoothingEnabled" => st.smoothing = v.truthy(),
        "letterSpacing" => {
            let x = s(it, &v)?;
            match x.trim().strip_suffix("px").and_then(|n| n.trim().parse::<f32>().ok()) {
                Some(px) => st.letter_spacing = px,
                None if x.trim() == "0" || x.trim() == "normal" => st.letter_spacing = 0.0,
                None => return Ok(Value::Undef),
            }
        }
        "filter" => {
            let x = s(it, &v)?;
            let mut decls = Vec::new();
            crate::css::parse_property("filter", &x, false, &mut decls);
            match decls.first().map(|d| d.value) {
                Some(crate::css::Value::Filter(i)) => {
                    st.filter = Some(crate::css::filter_ops(i));
                    st.filter_css = x;
                }
                Some(crate::css::Value::Kw(_)) => {
                    st.filter = None;
                    st.filter_css = "none".into();
                }
                _ => return Ok(Value::Undef),
            }
        }
        _ => return Ok(Value::Undef),
    }
    Ok(v)
}

/// `@@canvasGradient(kind, ...coords)`
fn gradient(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let kind = it.to_str(&arg(a, 0))?;
    let v: Vec<f32> = (1..7).map(|i| num(it, a, i)).collect::<R<_>>()?;
    let k = match &*kind {
        "linear" => {
            if v[..4].iter().any(|x| !x.is_finite()) {
                return Err(type_error("The provided double value is non-finite."));
            }
            GradKind::Linear([v[0], v[1], v[2], v[3]])
        }
        "radial" => {
            if v.iter().any(|x| !x.is_finite()) {
                return Err(type_error("The provided double value is non-finite."));
            }
            if v[2] < 0.0 || v[5] < 0.0 {
                return Err(make_dom_error("IndexSizeError", "The radius provided is negative."));
            }
            GradKind::Radial([v[0], v[1], v[2], v[3], v[4], v[5]])
        }
        _ => GradKind::Conic([v[0], v[1], v[2]]),
    };
    Ok(wrap(it, Host::Grad(Rc::new(RefCell::new(Gradient { kind: k, stops: Vec::new() })))))
}

/// `@@canvasStop(gradient, offset, color)`
fn add_stop(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let Some(Host::Grad(g)) = host(&arg(a, 0)).as_deref().map(|h| match h {
        Host::Grad(g) => Host::Grad(g.clone()),
        _ => Host::Path(Rc::default()),
    }) else {
        return Err(type_error("Illegal invocation"));
    };
    let off = num(it, a, 1)?;
    if !(0.0..=1.0).contains(&off) {
        return Err(make_dom_error("IndexSizeError", "The provided value is outside the range [0, 1]."));
    }
    let s = it.to_str(&arg(a, 2))?;
    let Some(c) = parse_color(&s) else {
        return Err(make_dom_error("SyntaxError", &format!("The value provided ('{s}') could not be parsed as a color.")));
    };
    g.borrow_mut().stops.push((off, c));
    Ok(Value::Undef)
}

/// `@@canvasPattern(image, repetition)`
fn pattern(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let Some(bmp) = source_bitmap(it, &arg(a, 0))? else { return Ok(Value::Null) };
    let rep = match arg(a, 1) {
        Value::Null | Value::Undef => "repeat".to_string(),
        v => it.to_str(&v)?.to_string(),
    };
    let rep = match rep.as_str() {
        "" | "repeat" => (true, true),
        "repeat-x" => (true, false),
        "repeat-y" => (false, true),
        "no-repeat" => (false, false),
        _ => return Err(make_dom_error("SyntaxError", "The provided repetition type is invalid.")),
    };
    Ok(wrap(it, Host::Pattern(Rc::new(Pattern { img: bmp, rep, m: RefCell::new(ID) }))))
}

/// `@@canvasPatternTransform(pattern, a, b, c, d, e, f)`
fn pattern_transform(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if let Some(Host::Pattern(p)) = host(&arg(a, 0)).as_deref() {
        let v: Vec<f32> = (1..7).map(|i| num(it, a, i)).collect::<R<_>>()?;
        *p.m.borrow_mut() = [v[0], v[1], v[2], v[3], v[4], v[5]];
    }
    Ok(Value::Undef)
}

/// `@@canvasPath(source?)`: a new Path2D (copying another, or from SVG
/// path data).
fn new_path(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut p = PathBuilder::default();
    match &arg(a, 0) {
        Value::Str(d) => {
            for s in svg::parse_path_data(d) {
                p.subs.push(s);
            }
            p.cur = p.subs.last().and_then(|s| s.pts.last().copied());
        }
        v => {
            if let Some(o) = path_of(v) {
                p = o.borrow().clone();
            }
        }
    }
    Ok(wrap(it, Host::Path(Rc::new(RefCell::new(p)))))
}

/// `@@canvasPathCall(path, name, ...args)`
fn path_call(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let Some(p) = path_of(&arg(a, 0)) else { return Err(type_error("Illegal invocation")) };
    let name = it.to_str(&arg(a, 1))?;
    let args = if a.len() > 2 { &a[2..] } else { &[] };
    if &*name == "addPath" {
        let Some(other) = path_of(&arg(args, 0)) else { return Ok(Value::Undef) };
        let m: Vec<f32> = (1..7).map(|i| num(it, args, i)).collect::<R<_>>()?;
        let m = if m.iter().all(|x| x.is_finite()) { [m[0], m[1], m[2], m[3], m[4], m[5]] } else { ID };
        let o = other.borrow().clone();
        p.borrow_mut().add(&o, &m);
        return Ok(Value::Undef);
    }
    let mut path = std::mem::take(&mut *p.borrow_mut());
    let r = path_cmd(it, &name, &mut path, &ID, args);
    *p.borrow_mut() = path;
    r?;
    Ok(Value::Undef)
}

/// `@@canvasBitmap(source, sx, sy, sw, sh)`: an ImageBitmap's pixels.
fn bitmap(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let src = &arg(a, 0);
    // ImageData-like { data, width, height }.
    if let Value::Obj(_) = src {
        let d = it.get(src, "data")?;
        if let Some(bytes) = super::binary::bytes_of(&d) {
            let w = it.get(src, "width")?;
            let h = it.get(src, "height")?;
            let (w, h) = (it.to_num(&w)? as usize, it.to_num(&h)? as usize);
            let mut c = Canvas2D::new(w, h);
            c.put_image_data(&bytes, w, h, 0, 0, [0, 0, w as i32, h as i32]);
            return Ok(wrap(it, Host::Bitmap(Rc::new(c.snapshot()))));
        }
    }
    let Some(b) = source_bitmap(it, src)? else { return Ok(Value::Null) };
    if a.len() >= 5 {
        let v: Vec<f32> = (1..5).map(|i| num(it, a, i)).collect::<R<_>>()?;
        let (w, h) = (v[2].abs().max(1.0) as usize, v[3].abs().max(1.0) as usize);
        let mut c = Canvas2D::new(w, h);
        c.draw_image(&b, v[0], v[1], v[2], v[3], 0.0, 0.0, w as f32, h as f32);
        return Ok(wrap(it, Host::Bitmap(Rc::new(c.snapshot()))));
    }
    Ok(wrap(it, Host::Bitmap(b)))
}

/// `@@canvasDecode(bytes)`: an ImageBitmap from encoded image bytes.
fn decode(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let bytes = super::binary::bytes_of(&arg(a, 0)).unwrap_or_default();
    let img = if crate::svg::sniff(&bytes) {
        crate::svg::render(&String::from_utf8_lossy(&bytes), 0xFF00_0000, None).map(|r| crate::image::from_pixels(r.w, r.h, r.px, r.iw, r.ih))
    } else {
        crate::image::decode(&bytes)
    };
    Ok(match img {
        Some(i) => wrap(it, Host::Bitmap(Rc::new(Bitmap::from_image(&i)))),
        None => Value::Null,
    })
}

/// `@@canvasBitmapSize(bitmap)`
fn bitmap_size(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let (w, h) = match host(&arg(a, 0)).as_deref() {
        Some(Host::Bitmap(b)) => (b.w, b.h),
        Some(Host::Ctx(c)) => (c.borrow().w, c.borrow().h),
        _ => (0, 0),
    };
    Ok(it.array(vec![Value::Num(w as f64), Value::Num(h as f64)]))
}

pub fn install(it: &mut Interp) {
    let o = it.plain();
    let fns: [(&'static str, NativeFn); 11] = [
        ("ctx", new_ctx),
        ("resize", resize),
        ("call", call),
        ("set", set),
        ("gradient", gradient),
        ("stop", add_stop),
        ("pattern", pattern),
        ("patternTransform", pattern_transform),
        ("path", new_path),
        ("pathCall", path_call),
        ("bitmap", bitmap),
    ];
    for (n, f) in fns {
        it.def_method(&o, n, f);
    }
    it.def_method(&o, "bitmapSize", bitmap_size);
    it.def_method(&o, "decode", decode);
    let g = it.global.clone();
    Interp::set_hidden(&g, "@@canvas", Value::Obj(o));
}

// ==================================================================== PNG

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (n, e) in t.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// An RGBA image as PNG (zlib with stored blocks: large but exact).
pub fn encode_png(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w * 4..(y + 1) * w * 4]);
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = if raw.is_empty() { vec![&[][..]] } else { raw.chunks(65535).collect() };
    let n = chunks.len();
    for (i, c) in chunks.into_iter().enumerate() {
        z.push((i + 1 == n) as u8);
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |ty: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = ty.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}
