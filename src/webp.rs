//! WebP decoding: the RIFF container, VP8 lossy key frames (RFC 6386),
//! VP8L lossless images (RFC 9649), alpha planes and the first frame of
//! animations. Colour conversion and chroma upsampling follow libwebp so
//! output matches other browsers.

use crate::vp8_tables::*;

fn le16(d: &[u8], i: usize) -> Option<u32> {
    Some(u16::from_le_bytes(d.get(i..i + 2)?.try_into().ok()?) as u32)
}
fn le24(d: &[u8], i: usize) -> Option<u32> {
    let b = d.get(i..i + 3)?;
    Some(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
}

fn chunks(mut d: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 8 {
        let tag = &d[..4];
        let size = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
        let body = &d[8..(8 + size).min(d.len())];
        out.push((tag, body));
        let next = (8 + size + (size & 1)).min(d.len());
        d = &d[next..];
    }
    out
}

/// Decode a WebP file to (width, height, 0xAARRGGBB pixels).
pub fn decode(d: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    if d.len() < 12 || &d[..4] != b"RIFF" || &d[8..12] != b"WEBP" {
        return None;
    }
    let cs = chunks(&d[12..]);
    let canvas = cs.iter().find(|(t, _)| *t == b"VP8X").and_then(|(_, b)| Some((le24(b, 4)? + 1, le24(b, 7)? + 1)));
    if let Some((_, frame)) = cs.iter().find(|(t, _)| *t == b"ANMF") {
        // Animation: the first frame, placed on a transparent canvas.
        let (cw, ch) = canvas?;
        let (fx, fy) = (le24(frame, 0)? * 2, le24(frame, 3)? * 2);
        let (w, h, px) = image(&chunks(frame.get(16..)?))?;
        let mut out = vec![0u32; (cw * ch) as usize];
        for y in 0..h.min(ch.saturating_sub(fy)) {
            for x in 0..w.min(cw.saturating_sub(fx)) {
                out[((fy + y) * cw + fx + x) as usize] = px[(y * w + x) as usize];
            }
        }
        return Some((cw, ch, out));
    }
    image(&cs)
}

fn image(cs: &[(&[u8], &[u8])]) -> Option<(u32, u32, Vec<u32>)> {
    let alpha = cs.iter().find(|(t, _)| *t == b"ALPH").map(|c| c.1);
    for &(tag, body) in cs {
        match tag {
            b"VP8L" => return vp8l(body),
            b"VP8 " => {
                let (w, h, mut px) = vp8(body)?;
                if let Some(a) = alpha {
                    if let Some(plane) = alpha_plane(a, w as usize, h as usize) {
                        for (p, a) in px.iter_mut().zip(plane) {
                            *p = (*p & 0x00FF_FFFF) | (a as u32) << 24;
                        }
                    }
                }
                return Some((w, h, px));
            }
            _ => {}
        }
    }
    None
}

// ================================================================== VP8

/// The boolean entropy decoder (RFC 6386 section 7).
struct BoolDec<'a> {
    d: &'a [u8],
    pos: usize,
    value: u64,
    bits: i32,
    range: u32,
}

impl<'a> BoolDec<'a> {
    fn new(d: &'a [u8]) -> Self {
        let mut b = BoolDec { d, pos: 0, value: 0, bits: 0, range: 255 };
        b.fill();
        b
    }
    #[inline]
    fn fill(&mut self) {
        while self.bits <= 56 {
            let byte = self.d.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.value |= (byte as u64) << (56 - self.bits);
            self.bits += 8;
        }
    }
    #[inline]
    fn bool(&mut self, p: u8) -> bool {
        let split = 1 + (((self.range - 1) * p as u32) >> 8);
        if self.bits < 8 {
            self.fill();
        }
        let big = (split as u64) << 56;
        let r = if self.value >= big {
            self.range -= split;
            self.value -= big;
            true
        } else {
            self.range = split;
            false
        };
        let shift = self.range.leading_zeros() - 24;
        self.range <<= shift;
        self.value <<= shift;
        self.bits -= shift as i32;
        r
    }
    fn lit(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bool(128) as u32;
        }
        v
    }
    /// An optional signed value: flag, magnitude, sign.
    fn opt_signed(&mut self, n: u32) -> i32 {
        if !self.bool(128) {
            return 0;
        }
        let v = self.lit(n) as i32;
        if self.bool(128) { -v } else { v }
    }
}

const BANDS: [usize; 17] = [0, 1, 2, 3, 6, 4, 5, 6, 6, 6, 6, 6, 6, 6, 6, 7, 0];
const ZIGZAG: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Read one block's tokens; returns the index after the last token.
fn read_coeffs(br: &mut BoolDec, probs: &[u8], typ: usize, ctx: usize, first: usize, dq: [i32; 2], out: &mut [i32]) -> usize {
    let p = |band: usize, ctx: usize| ((typ * 8 + band) * 3 + ctx) * 11;
    let mut i = first;
    let mut pr = p(BANDS[i], ctx);
    if !br.bool(probs[pr]) {
        return i;
    }
    loop {
        while !br.bool(probs[pr + 1]) {
            i += 1;
            if i == 16 {
                return 16;
            }
            pr = p(BANDS[i], 0);
        }
        let (v, next_ctx) = if !br.bool(probs[pr + 2]) {
            (1, 1)
        } else {
            let v = if !br.bool(probs[pr + 3]) {
                if !br.bool(probs[pr + 4]) { 2 } else { 3 + br.bool(probs[pr + 5]) as i32 }
            } else if !br.bool(probs[pr + 6]) {
                if !br.bool(probs[pr + 7]) {
                    5 + br.bool(159) as i32
                } else {
                    7 + 2 * br.bool(165) as i32 + br.bool(145) as i32
                }
            } else {
                let b1 = br.bool(probs[pr + 8]) as usize;
                let b0 = br.bool(probs[pr + 9 + b1]) as usize;
                let (base, tbl): (i32, &[u8]) = match 2 * b1 + b0 {
                    0 => (11, &[173, 148, 140]),
                    1 => (19, &[176, 155, 140, 135]),
                    2 => (35, &[180, 157, 141, 134, 130]),
                    _ => (67, &[254, 254, 243, 230, 196, 177, 153, 140, 133, 130, 129]),
                };
                let mut e = 0;
                for &q in tbl {
                    e = (e << 1) | br.bool(q) as i32;
                }
                base + e
            };
            (v, 2)
        };
        let s = if br.bool(128) { -v } else { v };
        out[ZIGZAG[i]] = s * if i > 0 { dq[1] } else { dq[0] };
        i += 1;
        if i == 16 {
            return 16;
        }
        pr = p(BANDS[i], next_ctx);
        if !br.bool(probs[pr]) {
            return i;
        }
    }
}

fn iwht(input: &[i32], out: &mut [i32]) {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let a1 = input[i] + input[12 + i];
        let b1 = input[4 + i] + input[8 + i];
        let c1 = input[4 + i] - input[8 + i];
        let d1 = input[i] - input[12 + i];
        t[i] = a1 + b1;
        t[4 + i] = c1 + d1;
        t[8 + i] = a1 - b1;
        t[12 + i] = d1 - c1;
    }
    for i in 0..4 {
        let r = &t[i * 4..i * 4 + 4];
        let a1 = r[0] + r[3];
        let b1 = r[1] + r[2];
        let c1 = r[1] - r[2];
        let d1 = r[0] - r[3];
        // Output i*4+k is the DC of luma block i*4+k.
        out[(i * 4) * 16] = (a1 + b1 + 3) >> 3;
        out[(i * 4 + 1) * 16] = (c1 + d1 + 3) >> 3;
        out[(i * 4 + 2) * 16] = (a1 - b1 + 3) >> 3;
        out[(i * 4 + 3) * 16] = (d1 - c1 + 3) >> 3;
    }
}

/// Inverse DCT of a 4x4 block, added to the prediction in `buf`.
fn idct_add(c: &[i32], buf: &mut [u8], off: usize, stride: usize) {
    const C1: i32 = 20091;
    const S1: i32 = 35468;
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (i0, i1, i2, i3) = (c[i], c[4 + i], c[8 + i], c[12 + i]);
        let a1 = i0 + i2;
        let b1 = i0 - i2;
        let c1 = ((i1 * S1) >> 16) - (i3 + ((i3 * C1) >> 16));
        let d1 = (i1 + ((i1 * C1) >> 16)) + ((i3 * S1) >> 16);
        t[i] = a1 + d1;
        t[12 + i] = a1 - d1;
        t[4 + i] = b1 + c1;
        t[8 + i] = b1 - c1;
    }
    for r in 0..4 {
        let (i0, i1, i2, i3) = (t[r * 4], t[r * 4 + 1], t[r * 4 + 2], t[r * 4 + 3]);
        let a1 = i0 + i2;
        let b1 = i0 - i2;
        let c1 = ((i1 * S1) >> 16) - (i3 + ((i3 * C1) >> 16));
        let d1 = (i1 + ((i1 * C1) >> 16)) + ((i3 * S1) >> 16);
        let o = off + r * stride;
        for (k, v) in [(a1 + d1 + 4) >> 3, (b1 + c1 + 4) >> 3, (b1 - c1 + 4) >> 3, (a1 - d1 + 4) >> 3].into_iter().enumerate() {
            buf[o + k] = (buf[o + k] as i32 + v).clamp(0, 255) as u8;
        }
    }
}

/// A plane with a one-pixel border above and to the left (127 above,
/// 129 to the left, as the spec defines for missing neighbours).
struct Plane {
    px: Vec<u8>,
    stride: usize,
}

impl Plane {
    fn new(w: usize, h: usize) -> Plane {
        let stride = w + 1 + 4;
        let mut px = vec![0u8; stride * (h + 1)];
        px[..stride].fill(127);
        for y in 1..=h {
            px[y * stride] = 129;
        }
        Plane { px, stride }
    }
    #[inline]
    fn at(&self, x: usize, y: usize) -> usize {
        (y + 1) * self.stride + x + 1
    }
}

/// 16x16 or 8x8 whole-block prediction (DC, V, H, TM).
fn predict_block(p: &mut Plane, x0: usize, y0: usize, n: usize, mode: u8, have_top: bool, have_left: bool) {
    let s = p.stride;
    let o = p.at(x0, y0);
    let shift = if n == 16 { 4 } else { 3 };
    match mode {
        0 => {
            let top: u32 = (0..n).map(|i| p.px[o - s + i] as u32).sum();
            let left: u32 = (0..n).map(|i| p.px[o + i * s - 1] as u32).sum();
            let v = match (have_top, have_left) {
                (true, true) => (top + left + n as u32) >> (shift + 1),
                (true, false) => (top + (n as u32 >> 1)) >> shift,
                (false, true) => (left + (n as u32 >> 1)) >> shift,
                _ => 128,
            } as u8;
            for y in 0..n {
                p.px[o + y * s..o + y * s + n].fill(v);
            }
        }
        1 => {
            for y in 0..n {
                p.px.copy_within(o - s..o - s + n, o + y * s);
            }
        }
        2 => {
            for y in 0..n {
                let v = p.px[o + y * s - 1];
                p.px[o + y * s..o + y * s + n].fill(v);
            }
        }
        _ => {
            let c = p.px[o - s - 1] as i32;
            for y in 0..n {
                let l = p.px[o + y * s - 1] as i32;
                for x in 0..n {
                    p.px[o + y * s + x] = (l + p.px[o - s + x] as i32 - c).clamp(0, 255) as u8;
                }
            }
        }
    }
}

/// 4x4 subblock prediction; `a` = above row (8, including above-right),
/// `l` = left column, `c` = corner.
fn predict_sub(p: &mut Plane, o: usize, a: [u8; 8], l: [u8; 4], c: u8, mode: u8) {
    let s = p.stride;
    let a: [i32; 8] = a.map(|v| v as i32);
    let l: [i32; 4] = l.map(|v| v as i32);
    let c = c as i32;
    let avg3 = |x: i32, y: i32, z: i32| (x + 2 * y + z + 2) >> 2;
    let avg2 = |x: i32, y: i32| (x + y + 1) >> 1;
    let e = [l[3], l[2], l[1], l[0], c, a[0], a[1], a[2], a[3]];
    let mut b = [[0i32; 4]; 4];
    match mode {
        0 => {
            let v = (a[..4].iter().sum::<i32>() + l.iter().sum::<i32>() + 4) >> 3;
            b = [[v; 4]; 4];
        }
        1 => {
            for r in 0..4 {
                for col in 0..4 {
                    b[r][col] = (l[r] + a[col] - c).clamp(0, 255);
                }
            }
        }
        2 => {
            for col in 0..4 {
                let prev = if col == 0 { c } else { a[col - 1] };
                let v = avg3(prev, a[col], a[col + 1]);
                for row in b.iter_mut() {
                    row[col] = v;
                }
            }
        }
        3 => {
            let rows = [avg3(c, l[0], l[1]), avg3(l[0], l[1], l[2]), avg3(l[1], l[2], l[3]), avg3(l[2], l[3], l[3])];
            for r in 0..4 {
                b[r] = [rows[r]; 4];
            }
        }
        4 => {
            // LD: down-left diagonal.
            for r in 0..4 {
                for col in 0..4 {
                    let i = r + col;
                    b[r][col] = if i < 6 { avg3(a[i], a[i + 1], a[i + 2]) } else { avg3(a[6], a[7], a[7]) };
                }
            }
        }
        5 => {
            // RD: down-right diagonal.
            for r in 0..4 {
                for col in 0..4 {
                    let i = 4 + col - r;
                    b[r][col] = avg3(e[i - 1], e[i], e[i + 1]);
                }
            }
        }
        6 => {
            // VR
            let p3 = |i: usize| avg3(e[i - 1], e[i], e[i + 1]);
            let p2 = |i: usize| avg2(e[i], e[i + 1]);
            b[3][0] = p3(2);
            b[2][0] = p3(3);
            b[3][1] = p3(4);
            b[1][0] = p3(4);
            b[2][1] = p2(4);
            b[0][0] = p2(4);
            b[3][2] = p3(5);
            b[1][1] = p3(5);
            b[2][2] = p2(5);
            b[0][1] = p2(5);
            b[3][3] = p3(6);
            b[1][2] = p3(6);
            b[2][3] = p2(6);
            b[0][2] = p2(6);
            b[1][3] = p3(7);
            b[0][3] = p2(7);
        }
        7 => {
            // VL
            let p3 = |i: usize| avg3(a[i - 1], a[i], a[i + 1]);
            let p2 = |i: usize| avg2(a[i], a[i + 1]);
            b[0][0] = p2(0);
            b[1][0] = p3(1);
            b[2][0] = p2(1);
            b[0][1] = p2(1);
            b[1][1] = p3(2);
            b[3][0] = p3(2);
            b[2][1] = p2(2);
            b[0][2] = p2(2);
            b[3][1] = p3(3);
            b[1][2] = p3(3);
            b[2][2] = p2(3);
            b[0][3] = p2(3);
            b[3][2] = p3(4);
            b[1][3] = p3(4);
            b[2][3] = p3(5);
            b[3][3] = p3(6);
        }
        8 => {
            // HD
            let p3 = |i: usize| avg3(e[i - 1], e[i], e[i + 1]);
            let p2 = |i: usize| avg2(e[i], e[i + 1]);
            b[3][0] = p2(0);
            b[3][1] = p3(1);
            b[2][0] = p2(1);
            b[3][2] = p2(1);
            b[2][1] = p3(2);
            b[3][3] = p3(2);
            b[2][2] = p2(2);
            b[1][0] = p2(2);
            b[2][3] = p3(3);
            b[1][1] = p3(3);
            b[1][2] = p2(3);
            b[0][0] = p2(3);
            b[1][3] = p3(4);
            b[0][1] = p3(4);
            b[0][2] = p3(5);
            b[0][3] = p3(6);
        }
        _ => {
            // HU
            b[0][0] = avg2(l[0], l[1]);
            b[0][1] = avg3(l[0], l[1], l[2]);
            b[0][2] = avg2(l[1], l[2]);
            b[1][0] = b[0][2];
            b[0][3] = avg3(l[1], l[2], l[3]);
            b[1][1] = b[0][3];
            b[1][2] = avg2(l[2], l[3]);
            b[2][0] = b[1][2];
            b[1][3] = avg3(l[2], l[3], l[3]);
            b[2][1] = b[1][3];
            b[2][2] = l[3];
            b[2][3] = l[3];
            b[3] = [l[3]; 4];
        }
    }
    for r in 0..4 {
        for col in 0..4 {
            p.px[o + r * s + col] = b[r][col] as u8;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct MbInfo {
    seg: u8,
    i4: bool,
    coeffs: bool,
}

fn vp8(d: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    let tag = le24(d, 0)?;
    if tag & 1 != 0 || d.get(3..6)? != [0x9d, 0x01, 0x2a] {
        return None; // not a key frame
    }
    let first_size = (tag >> 5) as usize;
    let (w, h) = ((le16(d, 6)? & 0x3fff) as usize, (le16(d, 8)? & 0x3fff) as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let mut br = BoolDec::new(d.get(10..10 + first_size)?);
    br.lit(2); // colour space, clamping
    let seg_enabled = br.bool(128);
    let (mut seg_map, mut seg_abs, mut seg_q, mut seg_lf, mut seg_probs) = (false, false, [0i32; 4], [0i32; 4], [255u8; 3]);
    if seg_enabled {
        seg_map = br.bool(128);
        if br.bool(128) {
            seg_abs = br.bool(128);
            seg_q = [0; 4].map(|_| br.opt_signed(7));
            seg_lf = [0; 4].map(|_| br.opt_signed(6));
        }
        if seg_map {
            seg_probs = [0; 3].map(|_| if br.bool(128) { br.lit(8) as u8 } else { 255 });
        }
    }
    let simple = br.bool(128);
    let level = br.lit(6) as i32;
    let sharp = br.lit(3) as i32;
    let (mut ref_delta, mut mode_delta) = (0, 0);
    let use_lf_delta = br.bool(128);
    if use_lf_delta && br.bool(128) {
        let r = [0; 4].map(|_| br.opt_signed(6));
        let m = [0; 4].map(|_| br.opt_signed(6));
        (ref_delta, mode_delta) = (r[0], m[0]);
    }
    let nparts = 1usize << br.lit(2);
    let q = br.lit(7) as i32;
    let deltas = [0; 5].map(|_| br.opt_signed(4)); // y dc, y2 dc, y2 ac, uv dc, uv ac
    br.lit(1); // refresh entropy probs
    let mut probs = DEFAULT_COEFF_PROBS;
    for i in 0..probs.len() {
        if br.bool(COEFF_UPDATE_PROBS[i]) {
            probs[i] = br.lit(8) as u8;
        }
    }
    let skip_prob = if br.bool(128) { Some(br.lit(8) as u8) } else { None };

    // Token partitions.
    let rest = d.get(10 + first_size..)?;
    let mut off = 3 * (nparts - 1);
    let mut parts = Vec::with_capacity(nparts);
    for i in 0..nparts {
        let size = if i + 1 < nparts { le24(rest, 3 * i)? as usize } else { rest.len().saturating_sub(off) };
        let start = off.min(rest.len());
        parts.push(BoolDec::new(&rest[start..(off + size).min(rest.len())]));
        off += size;
    }

    // Dequantization factors per segment: [y1, y2, uv] x [dc, ac].
    let quant: Vec<[[i32; 2]; 3]> = (0..4)
        .map(|s| {
            let base = if seg_enabled { if seg_abs { seg_q[s] } else { q + seg_q[s] } } else { q };
            let c = |v: i32| v.clamp(0, 127) as usize;
            [
                [DC_Q[c(base + deltas[0])], AC_Q[c(base)]],
                [DC_Q[c(base + deltas[1])] * 2, (AC_Q[c(base + deltas[2])] * 155 / 100).max(8)],
                [DC_Q[c(base + deltas[3])].min(132), AC_Q[c(base + deltas[4])]],
            ]
        })
        .collect();

    let (mbw, mbh) = (w.div_ceil(16), h.div_ceil(16));
    let mut yp = Plane::new(mbw * 16, mbh * 16);
    let mut up = Plane::new(mbw * 8, mbh * 8);
    let mut vp = Plane::new(mbw * 8, mbh * 8);
    let mut info = vec![MbInfo::default(); mbw * mbh];
    let mut above_modes = vec![0u8; mbw * 4];
    let mut above_nz = vec![[0u8; 9]; mbw];
    let mut coeffs = [0i32; 25 * 16];
    for mby in 0..mbh {
        let mut left_modes = [0u8; 4];
        let mut left_nz = [0u8; 9];
        let part = &mut parts[mby % nparts];
        for mbx in 0..mbw {
            let seg = if seg_map {
                if !br.bool(seg_probs[0]) { br.bool(seg_probs[1]) as u8 } else { 2 + br.bool(seg_probs[2]) as u8 }
            } else {
                0
            };
            let skip = skip_prob.is_some_and(|p| br.bool(p));
            let ymode: u8 = if !br.bool(145) {
                4
            } else if !br.bool(156) {
                if br.bool(163) { 1 } else { 0 }
            } else if br.bool(128) {
                3
            } else {
                2
            };
            let mut bmodes = [0u8; 16];
            if ymode == 4 {
                for i in 0..16 {
                    let a = if i < 4 { above_modes[mbx * 4 + i] } else { bmodes[i - 4] };
                    let l = if i % 4 == 0 { left_modes[i / 4] } else { bmodes[i - 1] };
                    let pr = &KF_BMODE_PROBS[(a as usize * 10 + l as usize) * 9..][..9];
                    bmodes[i] = if !br.bool(pr[0]) {
                        0
                    } else if !br.bool(pr[1]) {
                        1
                    } else if !br.bool(pr[2]) {
                        2
                    } else if !br.bool(pr[3]) {
                        if !br.bool(pr[4]) {
                            3
                        } else if !br.bool(pr[5]) {
                            5
                        } else {
                            6
                        }
                    } else if !br.bool(pr[6]) {
                        4
                    } else if !br.bool(pr[7]) {
                        7
                    } else if !br.bool(pr[8]) {
                        8
                    } else {
                        9
                    };
                }
            } else {
                bmodes = [[0, 2, 3, 1][ymode as usize]; 16];
            }
            for c in 0..4 {
                above_modes[mbx * 4 + c] = bmodes[12 + c];
                left_modes[c] = bmodes[c * 4 + 3];
            }
            let uvmode: u8 = if !br.bool(142) {
                0
            } else if !br.bool(114) {
                1
            } else if !br.bool(183) {
                2
            } else {
                3
            };

            // Residuals.
            coeffs.fill(0);
            let mut has = false;
            let an = &mut above_nz[mbx];
            let qs = &quant[seg as usize];
            if !skip {
                let (first, ytype) = if ymode != 4 {
                    let mut dc = [0i32; 16];
                    let n = read_coeffs(part, &probs, 1, (an[8] + left_nz[8]) as usize, 0, qs[1], &mut dc);
                    let nz = (n > 0) as u8;
                    an[8] = nz;
                    left_nz[8] = nz;
                    iwht(&dc, &mut coeffs);
                    (1, 0)
                } else {
                    (0, 3)
                };
                for y in 0..4 {
                    for x in 0..4 {
                        let b = (y * 4 + x) * 16;
                        let n = read_coeffs(part, &probs, ytype, (an[x] + left_nz[y]) as usize, first, qs[0], &mut coeffs[b..b + 16]);
                        let nz = (n > first) as u8;
                        an[x] = nz;
                        left_nz[y] = nz;
                        has |= n > 1 || coeffs[b] != 0;
                    }
                }
                for c in 0..2 {
                    for y in 0..2 {
                        for x in 0..2 {
                            let b = (16 + c * 4 + y * 2 + x) * 16;
                            let (ai, li) = (4 + c * 2 + x, 4 + c * 2 + y);
                            let n = read_coeffs(part, &probs, 2, (an[ai] + left_nz[li]) as usize, 0, qs[2], &mut coeffs[b..b + 16]);
                            let nz = (n > 0) as u8;
                            an[ai] = nz;
                            left_nz[li] = nz;
                            has |= n > 1 || coeffs[b] != 0;
                        }
                    }
                }
            } else {
                an[..8].fill(0);
                left_nz[..8].fill(0);
                if ymode != 4 {
                    an[8] = 0;
                    left_nz[8] = 0;
                }
            }
            info[mby * mbw + mbx] = MbInfo { seg, i4: ymode == 4, coeffs: has };

            // Prediction and reconstruction.
            let (x0, y0) = (mbx * 16, mby * 16);
            if ymode == 4 {
                // Above-right of the macroblock (used by the right column of subblocks).
                let tr: [u8; 4] = if mby == 0 {
                    [127; 4]
                } else if mbx + 1 == mbw {
                    [yp.px[yp.at(x0 + 15, y0 - 1)]; 4]
                } else {
                    let o = yp.at(x0 + 16, y0 - 1);
                    yp.px[o..o + 4].try_into().unwrap()
                };
                let s = yp.stride;
                for i in 0..16 {
                    let (bx, by) = (x0 + (i % 4) * 4, y0 + (i / 4) * 4);
                    let o = yp.at(bx, by);
                    let mut a = [0u8; 8];
                    a[..4].copy_from_slice(&yp.px[o - s..o - s + 4]);
                    if i % 4 == 3 {
                        a[4..].copy_from_slice(&tr);
                    } else {
                        a[4..].copy_from_slice(&yp.px[o - s + 4..o - s + 8]);
                    }
                    let l = [yp.px[o - 1], yp.px[o + s - 1], yp.px[o + 2 * s - 1], yp.px[o + 3 * s - 1]];
                    let c = yp.px[o - s - 1];
                    predict_sub(&mut yp, o, a, l, c, bmodes[i]);
                    idct_add(&coeffs[i * 16..i * 16 + 16], &mut yp.px, o, s);
                }
            } else {
                predict_block(&mut yp, x0, y0, 16, ymode, mby > 0, mbx > 0);
                for i in 0..16 {
                    let o = yp.at(x0 + (i % 4) * 4, y0 + (i / 4) * 4);
                    idct_add(&coeffs[i * 16..i * 16 + 16], &mut yp.px, o, yp.stride);
                }
            }
            let (cx, cy) = (mbx * 8, mby * 8);
            for (c, pl) in [&mut up, &mut vp].into_iter().enumerate() {
                predict_block(pl, cx, cy, 8, uvmode, mby > 0, mbx > 0);
                for i in 0..4 {
                    let o = pl.at(cx + (i % 2) * 4, cy + (i / 2) * 4);
                    let b = (16 + c * 4 + i) * 16;
                    idct_add(&coeffs[b..b + 16], &mut pl.px, o, pl.stride);
                }
            }
        }
    }

    if level > 0 {
        loop_filter(&mut yp, &mut up, &mut vp, &info, mbw, mbh, |m: &MbInfo| {
            let mut l = if seg_enabled {
                if seg_abs { seg_lf[m.seg as usize] } else { level + seg_lf[m.seg as usize] }
            } else {
                level
            };
            if use_lf_delta {
                l += ref_delta;
                if m.i4 {
                    l += mode_delta;
                }
            }
            l.clamp(0, 63)
        }, sharp, simple);
    }
    Some((w as u32, h as u32, yuv_to_rgb(&yp, &up, &vp, w, h)))
}

// ------------------------------------------------------------ loop filter

#[inline]
fn sclip1(v: i32) -> i32 {
    v.clamp(-128, 127)
}
#[inline]
fn sclip2(v: i32) -> i32 {
    v.clamp(-16, 15)
}
#[inline]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

struct Edge<'a> {
    px: &'a mut [u8],
}

impl Edge<'_> {
    #[inline]
    fn g(&self, p: usize, k: isize, step: usize) -> i32 {
        self.px[(p as isize + k * step as isize) as usize] as i32
    }
    #[inline]
    fn s(&mut self, p: usize, k: isize, step: usize, v: u8) {
        self.px[(p as isize + k * step as isize) as usize] = v;
    }
    fn filter2(&mut self, p: usize, st: usize) {
        let (p1, p0, q0, q1) = (self.g(p, -2, st), self.g(p, -1, st), self.g(p, 0, st), self.g(p, 1, st));
        let a = 3 * (q0 - p0) + sclip1(p1 - q1);
        let a1 = sclip2((a + 4) >> 3);
        let a2 = sclip2((a + 3) >> 3);
        self.s(p, -1, st, clip1(p0 + a2));
        self.s(p, 0, st, clip1(q0 - a1));
    }
    fn filter4(&mut self, p: usize, st: usize) {
        let (p1, p0, q0, q1) = (self.g(p, -2, st), self.g(p, -1, st), self.g(p, 0, st), self.g(p, 1, st));
        let a = 3 * (q0 - p0);
        let a1 = sclip2((a + 4) >> 3);
        let a2 = sclip2((a + 3) >> 3);
        let a3 = (a1 + 1) >> 1;
        self.s(p, -2, st, clip1(p1 + a3));
        self.s(p, -1, st, clip1(p0 + a2));
        self.s(p, 0, st, clip1(q0 - a1));
        self.s(p, 1, st, clip1(q1 - a3));
    }
    fn filter6(&mut self, p: usize, st: usize) {
        let (p2, p1, p0) = (self.g(p, -3, st), self.g(p, -2, st), self.g(p, -1, st));
        let (q0, q1, q2) = (self.g(p, 0, st), self.g(p, 1, st), self.g(p, 2, st));
        let a = sclip1(3 * (q0 - p0) + sclip1(p1 - q1));
        let a1 = (27 * a + 63) >> 7;
        let a2 = (18 * a + 63) >> 7;
        let a3 = (9 * a + 63) >> 7;
        self.s(p, -3, st, clip1(p2 + a3));
        self.s(p, -2, st, clip1(p1 + a2));
        self.s(p, -1, st, clip1(p0 + a1));
        self.s(p, 0, st, clip1(q0 - a1));
        self.s(p, 1, st, clip1(q1 - a2));
        self.s(p, 2, st, clip1(q2 - a3));
    }
    fn hev(&self, p: usize, st: usize, t: i32) -> bool {
        (self.g(p, -2, st) - self.g(p, -1, st)).abs() > t || (self.g(p, 1, st) - self.g(p, 0, st)).abs() > t
    }
    fn needs(&self, p: usize, st: usize, t: i32) -> bool {
        4 * (self.g(p, -1, st) - self.g(p, 0, st)).abs() + (self.g(p, -2, st) - self.g(p, 1, st)).abs() <= t
    }
    fn needs2(&self, p: usize, st: usize, t: i32, it: i32) -> bool {
        if !self.needs(p, st, t) {
            return false;
        }
        let v: [i32; 8] = [-4, -3, -2, -1, 0, 1, 2, 3].map(|k| self.g(p, k, st));
        (v[0] - v[1]).abs() <= it
            && (v[1] - v[2]).abs() <= it
            && (v[2] - v[3]).abs() <= it
            && (v[7] - v[6]).abs() <= it
            && (v[6] - v[5]).abs() <= it
            && (v[5] - v[4]).abs() <= it
    }
    /// Filter `n` pixels along an edge: `p` is the first pixel after the
    /// edge, `st` steps across it and `along` steps along it.
    fn simple(&mut self, p: usize, st: usize, along: usize, n: usize, thresh: i32) {
        let t = 2 * thresh + 1;
        for i in 0..n {
            let q = p + i * along;
            if self.needs(q, st, t) {
                self.filter2(q, st);
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn normal(&mut self, p: usize, st: usize, along: usize, n: usize, thresh: i32, it: i32, hev_t: i32, mb_edge: bool) {
        let t = 2 * thresh + 1;
        for i in 0..n {
            let q = p + i * along;
            if self.needs2(q, st, t, it) {
                if self.hev(q, st, hev_t) {
                    self.filter2(q, st);
                } else if mb_edge {
                    self.filter6(q, st);
                } else {
                    self.filter4(q, st);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn loop_filter(yp: &mut Plane, up: &mut Plane, vp: &mut Plane, info: &[MbInfo], mbw: usize, mbh: usize, level_of: impl Fn(&MbInfo) -> i32, sharp: i32, simple: bool) {
    for mby in 0..mbh {
        for mbx in 0..mbw {
            let m = &info[mby * mbw + mbx];
            let level = level_of(m);
            if level == 0 {
                continue;
            }
            let mut il = level;
            if sharp > 0 {
                il >>= if sharp > 4 { 2 } else { 1 };
                il = il.min(9 - sharp);
            }
            let il = il.max(1);
            let limit = 2 * level + il;
            let hev_t = if level >= 40 { 2 } else if level >= 15 { 1 } else { 0 };
            let inner = m.i4 || m.coeffs;
            let ys = yp.stride;
            let yo = yp.at(mbx * 16, mby * 16);
            if simple {
                let mut e = Edge { px: &mut yp.px };
                if mbx > 0 {
                    e.simple(yo, 1, ys, 16, limit + 4);
                }
                if inner {
                    for k in 1..4 {
                        e.simple(yo + 4 * k, 1, ys, 16, limit);
                    }
                }
                if mby > 0 {
                    e.simple(yo, ys, 1, 16, limit + 4);
                }
                if inner {
                    for k in 1..4 {
                        e.simple(yo + 4 * k * ys, ys, 1, 16, limit);
                    }
                }
                continue;
            }
            let cs = up.stride;
            let co = up.at(mbx * 8, mby * 8);
            // Vertical edges (filtering across x), then horizontal edges.
            if mbx > 0 {
                Edge { px: &mut yp.px }.normal(yo, 1, ys, 16, limit + 4, il, hev_t, true);
                Edge { px: &mut up.px }.normal(co, 1, cs, 8, limit + 4, il, hev_t, true);
                Edge { px: &mut vp.px }.normal(co, 1, cs, 8, limit + 4, il, hev_t, true);
            }
            if inner {
                for k in 1..4 {
                    Edge { px: &mut yp.px }.normal(yo + 4 * k, 1, ys, 16, limit, il, hev_t, false);
                }
                Edge { px: &mut up.px }.normal(co + 4, 1, cs, 8, limit, il, hev_t, false);
                Edge { px: &mut vp.px }.normal(co + 4, 1, cs, 8, limit, il, hev_t, false);
            }
            if mby > 0 {
                Edge { px: &mut yp.px }.normal(yo, ys, 1, 16, limit + 4, il, hev_t, true);
                Edge { px: &mut up.px }.normal(co, cs, 1, 8, limit + 4, il, hev_t, true);
                Edge { px: &mut vp.px }.normal(co, cs, 1, 8, limit + 4, il, hev_t, true);
            }
            if inner {
                for k in 1..4 {
                    Edge { px: &mut yp.px }.normal(yo + 4 * k * ys, ys, 1, 16, limit, il, hev_t, false);
                }
                Edge { px: &mut up.px }.normal(co + 4 * cs, cs, 1, 8, limit, il, hev_t, false);
                Edge { px: &mut vp.px }.normal(co + 4 * cs, cs, 1, 8, limit, il, hev_t, false);
            }
        }
    }
}

// ------------------------------------------------------------- colour

#[inline]
fn clip8(v: i32) -> u32 {
    if v & !16383 == 0 {
        (v >> 6) as u32
    } else if v < 0 {
        0
    } else {
        255
    }
}

#[inline]
fn rgb(y: i32, u: i32, v: i32) -> u32 {
    let hi = |v: i32, c: i32| (v * c) >> 8;
    let yy = hi(y, 19077);
    let r = clip8(yy + hi(v, 26149) - 14234);
    let g = clip8(yy - hi(u, 6419) - hi(v, 13320) + 8708);
    let b = clip8(yy + hi(u, 33050) - 17685);
    0xFF00_0000 | r << 16 | g << 8 | b
}

/// YUV 4:2:0 to RGB with libwebp's "fancy" (bilinear) chroma upsampling.
fn yuv_to_rgb(yp: &Plane, up: &Plane, vp: &Plane, w: usize, h: usize) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    let row = |p: &Plane, y: usize| p.at(0, y);
    // One output line: chroma interpolated 3:1 between `near` and `far` rows.
    let line = |out: &mut [u32], yrow: usize, near: usize, far: usize| {
        let yo = row(yp, yrow);
        let (nu, fu, nv, fv) = (row(up, near), row(up, far), row(vp, near), row(vp, far));
        for x in 0..w {
            let cx = x / 2;
            let (u, v) = if x == 0 || (x == w - 1 && w % 2 == 0) {
                let cx = if x == 0 { 0 } else { (w - 1) / 2 };
                ((3 * up.px[nu + cx] as i32 + up.px[fu + cx] as i32 + 2) >> 2, (3 * vp.px[nv + cx] as i32 + vp.px[fv + cx] as i32 + 2) >> 2)
            } else {
                // Nearest chroma column `a` and the other neighbour `b`. This
                // is libwebp's pairwise "diagonal" form written per pixel:
                // roughly 9/16 nearest + 3/16 each adjacent + 1/16 opposite.
                let (a, b) = if x % 2 == 1 { (cx, cx + 1) } else { (cx, cx - 1) };
                let interp = |p: &Plane, n: usize, f: usize| -> i32 {
                    let (nn, nf, fnn, ff) = (p.px[n + a] as i32, p.px[n + b] as i32, p.px[f + a] as i32, p.px[f + b] as i32);
                    let diag = (nn + nf + fnn + ff + 8 + 2 * (nf + fnn)) >> 3;
                    (diag + nn) >> 1
                };
                (interp(up, nu, fu), interp(vp, nv, fv))
            };
            out[x] = rgb(yp.px[yo + x] as i32, u, v);
        }
    };
    for y in 0..h {
        let cy = y / 2;
        // Nearest chroma row and the other row it is blended with.
        let (near, far) = if y == 0 || (y == h - 1 && h % 2 == 0) {
            let r = if y == 0 { 0 } else { (h - 1) / 2 };
            (r, r)
        } else if y % 2 == 1 {
            (cy, cy + 1)
        } else {
            (cy, cy - 1)
        };
        line(&mut out[y * w..y * w + w], y, near, far);
    }
    out
}

// ---------------------------------------------------------------- alpha

fn alpha_plane(a: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    let hdr = *a.first()?;
    let mut p: Vec<u8> = if hdr & 3 == 0 {
        a.get(1..1 + w * h)?.to_vec()
    } else {
        let mut br = LBits::new(&a[1..]);
        level0(&mut br, w, h)?.iter().map(|p| (p >> 8) as u8).collect()
    };
    let filter = (hdr >> 2) & 3;
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let pred = if y == 0 {
                if x == 0 { 0 } else { p[i - 1] }
            } else if x == 0 {
                p[i - w]
            } else {
                match filter {
                    1 => p[i - 1],
                    2 => p[i - w],
                    3 => (p[i - 1] as i32 + p[i - w] as i32 - p[i - w - 1] as i32).clamp(0, 255) as u8,
                    _ => 0,
                }
            };
            if filter != 0 {
                p[i] = p[i].wrapping_add(pred);
            }
        }
    }
    Some(p)
}

// ================================================================= VP8L

struct LBits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u64,
    n: u32,
}

impl<'a> LBits<'a> {
    fn new(d: &'a [u8]) -> Self {
        LBits { d, pos: 0, buf: 0, n: 0 }
    }
    #[inline]
    fn peek(&mut self, k: u32) -> u32 {
        while self.n <= 56 {
            let b = self.d.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.buf |= (b as u64) << self.n;
            self.n += 8;
        }
        (self.buf & ((1u64 << k) - 1)) as u32
    }
    #[inline]
    fn skip(&mut self, k: u32) {
        self.buf >>= k;
        self.n -= k;
    }
    #[inline]
    fn read(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        let v = self.peek(k);
        self.skip(k);
        v
    }
    fn overrun(&self) -> bool {
        self.pos > self.d.len() + 16
    }
}

/// Canonical prefix code: 10-bit lookup table, bit-serial fallback.
struct Code {
    single: Option<u32>,
    fast: Vec<u32>,
    count: [u16; 16],
    symbols: Vec<u16>,
}

const FAST: u32 = 10;

impl Code {
    fn new(lens: &[u8]) -> Option<Code> {
        let nz: Vec<usize> = (0..lens.len()).filter(|&i| lens[i] > 0).collect();
        if nz.len() <= 1 {
            return Some(Code { single: Some(nz.first().copied().unwrap_or(0) as u32), fast: Vec::new(), count: [0; 16], symbols: Vec::new() });
        }
        let mut count = [0u16; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut symbols = Vec::with_capacity(nz.len());
        for l in 1..16u8 {
            symbols.extend((0..lens.len()).filter(|&s| lens[s] == l).map(|s| s as u16));
        }
        let mut fast = vec![0u32; 1 << FAST];
        let (mut code, mut k) = (0u32, 0);
        for len in 1..16u32 {
            for _ in 0..count[len as usize] {
                if len <= FAST {
                    let mut r = 0;
                    for b in 0..len {
                        r |= ((code >> b) & 1) << (len - 1 - b);
                    }
                    let mut i = r as usize;
                    while i < fast.len() {
                        fast[i] = (symbols[k] as u32) << 8 | len;
                        i += 1 << len;
                    }
                }
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Some(Code { single: None, fast, count, symbols })
    }

    #[inline]
    fn decode(&self, b: &mut LBits) -> Option<u32> {
        if let Some(s) = self.single {
            return Some(s);
        }
        let e = self.fast[b.peek(FAST) as usize];
        if e != 0 {
            b.skip(e & 0xFF);
            return Some(e >> 8);
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.read(1) as i32;
            let c = self.count[len] as i32;
            if code - first < c {
                return self.symbols.get((index + code - first) as usize).map(|&s| s as u32);
            }
            index += c;
            first = (first + c) << 1;
            code <<= 1;
        }
        None
    }
}

fn read_code(b: &mut LBits, alphabet: usize) -> Option<Code> {
    let mut lens = vec![0u8; alphabet];
    if b.read(1) == 1 {
        let n = b.read(1) + 1;
        let first_bits = if b.read(1) == 1 { 8 } else { 1 };
        let s0 = b.read(first_bits) as usize;
        *lens.get_mut(s0)? = 1;
        if n == 2 {
            *lens.get_mut(b.read(8) as usize)? = 1;
        }
        return Code::new(&lens);
    }
    const ORDER: [usize; 19] = [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut cl = [0u8; 19];
    let n = b.read(4) as usize + 4;
    for &o in ORDER.iter().take(n) {
        cl[o] = b.read(3) as u8;
    }
    let clc = Code::new(&cl)?;
    let mut max = if b.read(1) == 1 {
        let nbits = 2 + 2 * b.read(3);
        let m = 2 + b.read(nbits) as usize;
        if m > alphabet {
            return None;
        }
        m
    } else {
        alphabet
    };
    let (mut sym, mut prev) = (0usize, 8u8);
    while sym < alphabet {
        if max == 0 {
            break;
        }
        max -= 1;
        let c = clc.decode(b)?;
        if c < 16 {
            lens[sym] = c as u8;
            sym += 1;
            if c != 0 {
                prev = c as u8;
            }
        } else {
            let (extra, offset) = [(2, 3), (3, 3), (7, 11)][(c - 16) as usize];
            let repeat = b.read(extra) as usize + offset;
            if sym + repeat > alphabet {
                return None;
            }
            let v = if c == 16 { prev } else { 0 };
            lens[sym..sym + repeat].fill(v);
            sym += repeat;
        }
        if b.overrun() {
            return None;
        }
    }
    Code::new(&lens)
}

const DIST_MAP: [(i8, i8); 120] = [
    (0, 1), (1, 0), (1, 1), (-1, 1), (0, 2), (2, 0), (1, 2), (-1, 2), (2, 1), (-2, 1), (2, 2), (-2, 2), (0, 3), (3, 0),
    (1, 3), (-1, 3), (3, 1), (-3, 1), (2, 3), (-2, 3), (3, 2), (-3, 2), (0, 4), (4, 0), (1, 4), (-1, 4), (4, 1), (-4, 1),
    (3, 3), (-3, 3), (2, 4), (-2, 4), (4, 2), (-4, 2), (0, 5), (3, 4), (-3, 4), (4, 3), (-4, 3), (5, 0), (1, 5), (-1, 5),
    (5, 1), (-5, 1), (2, 5), (-2, 5), (5, 2), (-5, 2), (4, 4), (-4, 4), (3, 5), (-3, 5), (5, 3), (-5, 3), (0, 6), (6, 0),
    (1, 6), (-1, 6), (6, 1), (-6, 1), (2, 6), (-2, 6), (6, 2), (-6, 2), (4, 5), (-4, 5), (5, 4), (-5, 4), (3, 6), (-3, 6),
    (6, 3), (-6, 3), (0, 7), (7, 0), (1, 7), (-1, 7), (5, 5), (-5, 5), (7, 1), (-7, 1), (4, 6), (-4, 6), (6, 4), (-6, 4),
    (2, 7), (-2, 7), (7, 2), (-7, 2), (3, 7), (-3, 7), (7, 3), (-7, 3), (5, 6), (-5, 6), (6, 5), (-6, 5), (8, 0), (4, 7),
    (-4, 7), (7, 4), (-7, 4), (8, 1), (8, 2), (6, 6), (-6, 6), (8, 3), (5, 7), (-5, 7), (7, 5), (-7, 5), (8, 4), (6, 7),
    (-6, 7), (7, 6), (-7, 6), (8, 5), (7, 7), (-7, 7), (8, 6), (8, 7),
];

fn prefix_value(sym: u32, b: &mut LBits) -> usize {
    if sym < 4 {
        return sym as usize + 1;
    }
    let extra = (sym - 2) >> 1;
    let offset = (2 + (sym & 1)) << extra;
    (offset + b.read(extra)) as usize + 1
}

/// Entropy-coded image data (with optional colour cache and, at the top
/// level, meta prefix codes).
fn entropy_image(b: &mut LBits, w: usize, h: usize, top: bool) -> Option<Vec<u32>> {
    let cache_bits = if b.read(1) == 1 {
        let c = b.read(4);
        if !(1..=11).contains(&c) {
            return None;
        }
        c
    } else {
        0
    };
    let (meta_bits, meta, meta_w) = if top && b.read(1) == 1 {
        let bits = b.read(3) + 2;
        let (mw, mh) = (w.div_ceil(1 << bits), h.div_ceil(1 << bits));
        (bits, entropy_image(b, mw, mh, false)?, mw)
    } else {
        (0, Vec::new(), 0)
    };
    let groups_n = meta.iter().map(|p| ((p >> 8) & 0xFFFF) as usize + 1).max().unwrap_or(1);
    let cache_size = if cache_bits > 0 { 1usize << cache_bits } else { 0 };
    let mut groups: Vec<[Code; 5]> = Vec::with_capacity(groups_n);
    for _ in 0..groups_n {
        groups.push([
            read_code(b, 256 + 24 + cache_size)?,
            read_code(b, 256)?,
            read_code(b, 256)?,
            read_code(b, 256)?,
            read_code(b, 40)?,
        ]);
    }
    let total = w * h;
    let mut px = vec![0u32; total];
    let mut cache = vec![0u32; cache_size];
    let (mut pos, mut cached, mut x, mut y) = (0usize, 0usize, 0usize, 0usize);
    while pos < total {
        let g = if meta.is_empty() { &groups[0] } else { &groups[((meta[(y >> meta_bits) * meta_w + (x >> meta_bits)] >> 8) & 0xFFFF) as usize] };
        let code = g[0].decode(b)?;
        let n = if code < 256 {
            let r = g[1].decode(b)?;
            let bl = g[2].decode(b)?;
            let a = g[3].decode(b)?;
            px[pos] = a << 24 | r << 16 | code << 8 | bl;
            1
        } else if code < 280 {
            let len = prefix_value(code - 256, b);
            let dsym = g[4].decode(b)?;
            let dcode = prefix_value(dsym, b);
            let dist = if dcode > 120 {
                dcode - 120
            } else {
                let (xi, yi) = DIST_MAP[dcode - 1];
                (xi as isize + yi as isize * w as isize).max(1) as usize
            };
            if dist > pos || pos + len > total {
                return None;
            }
            for i in pos..pos + len {
                px[i] = px[i - dist];
            }
            len
        } else {
            px[pos] = *cache.get((code - 280) as usize)?;
            1
        };
        pos += n;
        x += n;
        while x >= w {
            x -= w;
            y += 1;
        }
        if cache_size > 0 {
            while cached < pos {
                let p = px[cached];
                cache[(0x1e35a7bdu32.wrapping_mul(p) >> (32 - cache_bits)) as usize] = p;
                cached += 1;
            }
        }
        if b.overrun() {
            return None;
        }
    }
    Some(px)
}

#[inline]
fn add_px(a: u32, b: u32) -> u32 {
    (((a & 0xFF00FF00).wrapping_add(b & 0xFF00FF00)) & 0xFF00FF00) | (((a & 0x00FF00FF).wrapping_add(b & 0x00FF00FF)) & 0x00FF00FF)
}
#[inline]
fn avg2(a: u32, b: u32) -> u32 {
    (((a ^ b) & 0xFEFEFEFE) >> 1).wrapping_add(a & b)
}

fn ch(p: u32, s: u32) -> i32 {
    ((p >> s) & 0xFF) as i32
}

fn predict(mode: u32, l: u32, t: u32, tl: u32, tr: u32) -> u32 {
    match mode {
        0 => 0xFF000000,
        1 => l,
        2 => t,
        3 => tr,
        4 => tl,
        5 => avg2(avg2(l, tr), t),
        6 => avg2(l, tl),
        7 => avg2(l, t),
        8 => avg2(tl, t),
        9 => avg2(t, tr),
        10 => avg2(avg2(l, tl), avg2(t, tr)),
        11 => {
            let (mut pl, mut pt) = (0, 0);
            for s in [24, 16, 8, 0] {
                let p = ch(l, s) + ch(t, s) - ch(tl, s);
                pl += (p - ch(l, s)).abs();
                pt += (p - ch(t, s)).abs();
            }
            if pl < pt { l } else { t }
        }
        12 => [24, 16, 8, 0].iter().fold(0, |acc, &s| acc | ((ch(l, s) + ch(t, s) - ch(tl, s)).clamp(0, 255) as u32) << s),
        13 => {
            let a = avg2(l, t);
            [24, 16, 8, 0].iter().fold(0, |acc, &s| acc | ((ch(a, s) + (ch(a, s) - ch(tl, s)) / 2).clamp(0, 255) as u32) << s)
        }
        _ => 0,
    }
}

enum Transform {
    Predict { bits: u32, w: usize, img: Vec<u32> },
    Color { bits: u32, w: usize, img: Vec<u32> },
    SubtractGreen,
    Index { table: Vec<u32>, wbits: u32, w: usize },
}

/// A full image stream: transforms, then entropy-coded pixels.
fn level0(b: &mut LBits, w: usize, h: usize) -> Option<Vec<u32>> {
    let mut ts = Vec::new();
    let mut xs = w;
    while b.read(1) == 1 {
        if ts.len() >= 4 {
            return None;
        }
        match b.read(2) {
            t @ (0 | 1) => {
                let bits = b.read(3) + 2;
                let img = entropy_image(b, xs.div_ceil(1 << bits), h.div_ceil(1 << bits), false)?;
                ts.push(if t == 0 { Transform::Predict { bits, w: xs, img } } else { Transform::Color { bits, w: xs, img } });
            }
            2 => ts.push(Transform::SubtractGreen),
            _ => {
                let n = b.read(8) as usize + 1;
                let mut table = entropy_image(b, n, 1, false)?;
                for i in 1..n {
                    table[i] = add_px(table[i], table[i - 1]);
                }
                let wbits = match n {
                    0..=2 => 3,
                    3..=4 => 2,
                    5..=16 => 1,
                    _ => 0,
                };
                ts.push(Transform::Index { table, wbits, w: xs });
                xs = xs.div_ceil(1 << wbits);
            }
        }
    }
    let mut px = entropy_image(b, xs, h, true)?;
    for t in ts.iter().rev() {
        match t {
            Transform::Predict { bits, w, img } => {
                let (w, bw) = (*w, w.div_ceil(1 << bits));
                for y in 0..h {
                    for x in 0..w {
                        let i = y * w + x;
                        let pred = if y == 0 {
                            if x == 0 { 0xFF000000 } else { px[i - 1] }
                        } else if x == 0 {
                            px[i - w]
                        } else {
                            let mode = (img[(y >> bits) * bw + (x >> bits)] >> 8) & 0xF;
                            predict(mode, px[i - 1], px[i - w], px[i - w - 1], px[i - w + 1])
                        };
                        px[i] = add_px(px[i], pred);
                    }
                }
            }
            Transform::Color { bits, w, img } => {
                let (w, bw) = (*w, w.div_ceil(1 << bits));
                let delta = |t: u32, c: u32| ((t as u8 as i8 as i32) * (c as u8 as i8 as i32)) >> 5;
                for y in 0..h {
                    for x in 0..w {
                        let e = img[(y >> bits) * bw + (x >> bits)];
                        let (g2r, g2b, r2b) = (e & 0xFF, (e >> 8) & 0xFF, (e >> 16) & 0xFF);
                        let p = px[y * w + x];
                        let g = (p >> 8) & 0xFF;
                        let r = ((p >> 16) as i32 + delta(g2r, g)) & 0xFF;
                        let bl = ((p as i32 & 0xFF) + delta(g2b, g) + delta(r2b, r as u32)) & 0xFF;
                        px[y * w + x] = (p & 0xFF00FF00) | (r as u32) << 16 | bl as u32;
                    }
                }
            }
            Transform::SubtractGreen => {
                for p in px.iter_mut() {
                    let g = (*p >> 8) & 0xFF;
                    *p = (*p & 0xFF00FF00) | ((((*p >> 16) + g) & 0xFF) << 16) | ((*p + g) & 0xFF);
                }
            }
            Transform::Index { table, wbits, w } => {
                let (w, packed_w) = (*w, w.div_ceil(1 << wbits));
                let bpp = 8 >> wbits;
                let mask = (1u32 << bpp) - 1;
                let mut out = vec![0u32; w * h];
                for y in 0..h {
                    for x in 0..w {
                        let p = px[y * packed_w + (x >> wbits)];
                        let idx = ((p >> 8) >> ((x & ((1 << wbits) - 1)) as u32 * bpp)) & mask;
                        out[y * w + x] = table.get(idx as usize).copied().unwrap_or(0);
                    }
                }
                px = out;
            }
        }
    }
    Some(px)
}

fn vp8l(d: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    if *d.first()? != 0x2f {
        return None;
    }
    let mut b = LBits::new(&d[1..]);
    let w = b.read(14) as usize + 1;
    let h = b.read(14) as usize + 1;
    b.read(1); // alpha hint
    if b.read(3) != 0 {
        return None;
    }
    let px = level0(&mut b, w, h)?;
    Some((w as u32, h as u32, px))
}

#[cfg(test)]
mod tests {
    /// Compare against Pillow (libwebp) decodes of the same files.
    fn check(file: &[u8], reference: &[u8], tolerance: i32) {
        let (w, h, px) = super::decode(file).expect("decode");
        let want_w = u32::from_le_bytes(reference[0..4].try_into().unwrap());
        let want_h = u32::from_le_bytes(reference[4..8].try_into().unwrap());
        assert_eq!((w, h), (want_w, want_h));
        let rgba = &reference[8..];
        let mut worst = 0;
        for (i, p) in px.iter().enumerate() {
            let got = [(p >> 16) & 255, (p >> 8) & 255, p & 255, p >> 24];
            for c in 0..4 {
                worst = worst.max((got[c] as i32 - rgba[i * 4 + c] as i32).abs());
            }
        }
        assert!(worst <= tolerance, "max channel difference {worst}");
    }

    #[test]
    fn lossless() {
        check(include_bytes!("../testdata/webp_lossless.webp"), include_bytes!("../testdata/webp_lossless.rgba"), 0);
        check(include_bytes!("../testdata/webp_palette.webp"), include_bytes!("../testdata/webp_palette.rgba"), 0);
    }

    #[test]
    fn lossy() {
        check(include_bytes!("../testdata/webp_lossy.webp"), include_bytes!("../testdata/webp_lossy.rgba"), 0);
        check(include_bytes!("../testdata/webp_lossy_alpha.webp"), include_bytes!("../testdata/webp_lossy_alpha.rgba"), 0);
        check(include_bytes!("../testdata/webp_small.webp"), include_bytes!("../testdata/webp_small.rgba"), 0);
    }

    /// Decode every `name.webp` in `$BOLT_WEBP_DIR` and compare it with
    /// `name.rgba` (8-byte width/height header, then RGBA).
    #[test]
    #[ignore]
    fn directory() {
        let dir = std::env::var("BOLT_WEBP_DIR").expect("BOLT_WEBP_DIR");
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "webp") {
                let t = std::time::Instant::now();
                let file = std::fs::read(&p).unwrap();
                let reference = std::fs::read(p.with_extension("rgba")).unwrap();
                super::decode(&file).expect("decode");
                eprintln!("{} decoded in {:.1} ms", p.display(), t.elapsed().as_secs_f64() * 1000.0);
                check(&file, &reference, 0);
            }
        }
    }

    #[test]
    fn animated_first_frame() {
        check(include_bytes!("../testdata/webp_anim.webp"), include_bytes!("../testdata/webp_anim.rgba"), 0);
    }
}
