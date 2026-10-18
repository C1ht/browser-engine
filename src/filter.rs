//! CSS filter functions (`filter`, `backdrop-filter`) on a premultiplied
//! RGBA image with channels in 0..=255.

use crate::css::FilterOp;

pub type Px = [f32; 4];

/// How far the filters can spread content outward: (left, top, right,
/// bottom) in px.
pub fn outset(ops: &[FilterOp]) -> [f32; 4] {
    let mut o = [0.0f32; 4];
    for op in ops {
        match *op {
            FilterOp::Blur(s) => {
                let r = (3.0 * s).ceil();
                o.iter_mut().for_each(|v| *v += r);
            }
            FilterOp::DropShadow(sh) => {
                let r = (1.5 * sh.blur).ceil();
                o[0] += (r - sh.x).max(0.0);
                o[1] += (r - sh.y).max(0.0);
                o[2] += (r + sh.x).max(0.0);
                o[3] += (r + sh.y).max(0.0);
            }
            _ => {}
        }
    }
    o
}

/// Apply `ops` in order to a w × h image.
pub fn apply(img: &mut [Px], w: usize, h: usize, ops: &[FilterOp]) {
    for op in ops {
        match *op {
            FilterOp::Blur(s) => blur(img, w, h, s),
            FilterOp::DropShadow(sh) => drop_shadow(img, w, h, sh.x, sh.y, sh.blur / 2.0, sh.color),
            FilterOp::Opacity(a) => img.iter_mut().for_each(|p| p.iter_mut().for_each(|c| *c *= a)),
            FilterOp::Brightness(k) => color_fn(img, |c| c.map(|v| v * k)),
            FilterOp::Contrast(k) => color_fn(img, |c| c.map(|v| (v - 0.5) * k + 0.5)),
            FilterOp::Invert(a) => color_fn(img, |c| c.map(|v| a + v * (1.0 - 2.0 * a))),
            FilterOp::Grayscale(a) => {
                let s = 1.0 - a;
                matrix(img, [
                    [0.2126 + 0.7874 * s, 0.7152 - 0.7152 * s, 0.0722 - 0.0722 * s],
                    [0.2126 - 0.2126 * s, 0.7152 + 0.2848 * s, 0.0722 - 0.0722 * s],
                    [0.2126 - 0.2126 * s, 0.7152 - 0.7152 * s, 0.0722 + 0.9278 * s],
                ])
            }
            FilterOp::Sepia(a) => {
                let s = 1.0 - a;
                matrix(img, [
                    [0.393 + 0.607 * s, 0.769 - 0.769 * s, 0.189 - 0.189 * s],
                    [0.349 - 0.349 * s, 0.686 + 0.314 * s, 0.168 - 0.168 * s],
                    [0.272 - 0.272 * s, 0.534 - 0.534 * s, 0.131 + 0.869 * s],
                ])
            }
            FilterOp::Saturate(s) => matrix(img, [
                [0.213 + 0.787 * s, 0.715 - 0.715 * s, 0.072 - 0.072 * s],
                [0.213 - 0.213 * s, 0.715 + 0.285 * s, 0.072 - 0.072 * s],
                [0.213 - 0.213 * s, 0.715 - 0.715 * s, 0.072 + 0.928 * s],
            ]),
            FilterOp::HueRotate(deg) => {
                let (s, c) = deg.to_radians().sin_cos();
                matrix(img, [
                    [0.213 + c * 0.787 - s * 0.213, 0.715 - c * 0.715 - s * 0.715, 0.072 - c * 0.072 + s * 0.928],
                    [0.213 - c * 0.213 + s * 0.143, 0.715 + c * 0.285 + s * 0.140, 0.072 - c * 0.072 - s * 0.283],
                    [0.213 - c * 0.213 - s * 0.787, 0.715 - c * 0.715 + s * 0.715, 0.072 + c * 0.928 + s * 0.072],
                ])
            }
        }
    }
}

/// A per-pixel function on unpremultiplied colour in 0..=1.
fn color_fn(img: &mut [Px], f: impl Fn([f32; 3]) -> [f32; 3]) {
    for p in img.iter_mut() {
        let a = p[3];
        if a <= 0.0 {
            continue;
        }
        let c = f([p[0] / a, p[1] / a, p[2] / a]);
        for k in 0..3 {
            p[k] = c[k].clamp(0.0, 1.0) * a;
        }
    }
}

fn matrix(img: &mut [Px], m: [[f32; 3]; 3]) {
    color_fn(img, |c| {
        let row = |r: [f32; 3]| r[0] * c[0] + r[1] * c[1] + r[2] * c[2];
        [row(m[0]), row(m[1]), row(m[2])]
    })
}

/// Box sizes whose three passes approximate a Gaussian of deviation `s`.
fn box_sizes(s: f32) -> [usize; 3] {
    let n = 3.0;
    let ideal = (12.0 * s * s / n + 1.0).sqrt();
    let mut wl = ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let wlf = wl as f32;
    let m = ((12.0 * s * s - n * wlf * wlf - 4.0 * n * wlf - 3.0 * n) / (-4.0 * wlf - 4.0)).round() as i32;
    let mut out = [0usize; 3];
    for (i, o) in out.iter_mut().enumerate() {
        *o = if (i as i32) < m { wl as usize } else { wu as usize };
    }
    out
}

/// Gaussian blur with standard deviation `s` px (edges extend the border
/// pixels).
pub fn blur(img: &mut [Px], w: usize, h: usize, s: f32) {
    if s < 0.25 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![[0.0f32; 4]; img.len()];
    for size in box_sizes(s) {
        let r = size / 2;
        if r == 0 {
            continue;
        }
        box_pass(img, &mut tmp, w, h, r, true);
        box_pass(&tmp, img, w, h, r, false);
    }
}

/// One box-blur pass of radius `r` along rows (`horiz`) or columns.
fn box_pass(src: &[Px], dst: &mut [Px], w: usize, h: usize, r: usize, horiz: bool) {
    let (n, lines) = if horiz { (w, h) } else { (h, w) };
    let idx = |line: usize, i: usize| if horiz { line * w + i } else { i * w + line };
    let scale = 1.0 / (2 * r + 1) as f32;
    for line in 0..lines {
        let at = |i: isize| src[idx(line, i.clamp(0, n as isize - 1) as usize)];
        let mut acc = [0.0f32; 4];
        for i in -(r as isize)..=(r as isize) {
            let p = at(i);
            for k in 0..4 {
                acc[k] += p[k];
            }
        }
        for i in 0..n {
            dst[idx(line, i)] = acc.map(|v| v * scale);
            let (add, sub) = (at(i as isize + r as isize + 1), at(i as isize - r as isize));
            for k in 0..4 {
                acc[k] += add[k] - sub[k];
            }
        }
    }
}

/// The image over a blurred, offset, coloured copy of its alpha.
fn drop_shadow(img: &mut [Px], w: usize, h: usize, dx: f32, dy: f32, s: f32, color: u32) {
    let ca = (color >> 24) as f32 / 255.0;
    let rgb = [((color >> 16) & 0xFF) as f32, ((color >> 8) & 0xFF) as f32, (color & 0xFF) as f32];
    let (dx, dy) = (dx.round() as isize, dy.round() as isize);
    let mut sh = vec![[0.0f32; 4]; img.len()];
    for y in 0..h as isize {
        let sy = y - dy;
        if sy < 0 || sy >= h as isize {
            continue;
        }
        for x in 0..w as isize {
            let sx = x - dx;
            if sx < 0 || sx >= w as isize {
                continue;
            }
            let a = img[sy as usize * w + sx as usize][3] / 255.0 * ca;
            sh[y as usize * w + x as usize] = [rgb[0] * a, rgb[1] * a, rgb[2] * a, 255.0 * a];
        }
    }
    blur(&mut sh, w, h, s);
    for (p, s) in img.iter_mut().zip(&sh) {
        let ia = 1.0 - p[3] / 255.0;
        for k in 0..4 {
            p[k] += s[k] * ia;
        }
    }
}
