//! Image decoders written from scratch: PNG (with our own inflate), JPEG
//! (baseline + progressive), GIF (first frame) and WebP (in `webp.rs`). Decoded images carry a
//! mipmap chain so heavily downscaled images still look smooth.

/// A decoded image: 0xAARRGGBB pixels (not premultiplied) plus mip levels.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
    /// Successively halved levels: (w, h, pixels).
    pub mips: Vec<(u32, u32, Vec<u32>)>,
    /// Intrinsic size in CSS px (differs from w/h for vector images, which
    /// are rasterized at a higher density).
    pub iw: f32,
    pub ih: f32,
    /// Rendered from vector data (SVG): scales preserving aspect ratio.
    pub vector: bool,
    /// No intrinsic width (an inline `<svg>` without width/height): fills
    /// the containing block's width.
    pub fluid: bool,
}

const MAX_DIM: u32 = 4096;

pub fn decode(data: &[u8]) -> Option<Image> {
    if crate::svg::sniff(data) {
        let r = crate::svg::render(&String::from_utf8_lossy(data), 0xFF000000, None)?;
        return Some(Image { vector: true, ..from_pixels(r.w, r.h, r.px, r.iw, r.ih) });
    }
    let (w, h, px) =if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(data)?
    } else if data.starts_with(&[0xFF, 0xD8]) {
        jpeg(data)?
    } else if data.starts_with(b"GIF8") {
        gif(data)?
    } else if data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        crate::webp::decode(data)?
    } else if data.len() > 12 && &data[4..8] == b"ftyp" && data[8..data.len().min(64)].windows(4).any(|w| w == b"avif" || w == b"avis") {
        crate::avif::decode(data)?
    } else {
        return None;
    };
    if w == 0 || h == 0 || px.len() < (w * h) as usize {
        return None;
    }
    Some(from_pixels(w, h, px, w as f32, h as f32))
}

/// Build an image (with mipmaps) from decoded pixels.
pub fn from_pixels(w: u32, h: u32, px: Vec<u32>, iw: f32, ih: f32) -> Image {
    let (mut w, mut h, mut px) = (w, h, px);
    while w > MAX_DIM || h > MAX_DIM {
        (w, h, px) = halve(w, h, &px);
    }
    let mut mips = Vec::new();
    let (mut mw, mut mh, mut cur) = (w, h, None::<Vec<u32>>);
    while mw > 16 && mh > 16 {
        let src = cur.as_deref().unwrap_or(&px);
        let (nw, nh, next) = halve(mw, mh, src);
        mips.push((nw, nh, next.clone()));
        (mw, mh, cur) = (nw, nh, Some(next));
    }
    Image { w, h, px, mips, iw, ih, vector: false, fluid: false }
}

/// 2x2 box filter.
fn halve(w: u32, h: u32, px: &[u32]) -> (u32, u32, Vec<u32>) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity((nw * nh) as usize);
    for y in 0..nh {
        let (y0, y1) = ((y * 2).min(h - 1), (y * 2 + 1).min(h - 1));
        for x in 0..nw {
            let (x0, x1) = ((x * 2).min(w - 1), (x * 2 + 1).min(w - 1));
            let p = [px[(y0 * w + x0) as usize], px[(y0 * w + x1) as usize], px[(y1 * w + x0) as usize], px[(y1 * w + x1) as usize]];
            let mut c = 0u32;
            for sh in [0, 8, 16, 24] {
                let s: u32 = p.iter().map(|v| (v >> sh) & 0xFF).sum();
                c |= ((s + 2) / 4) << sh;
            }
            out.push(c);
        }
    }
    (nw, nh, out)
}

// ================================================================= inflate

struct Huff {
    /// Indexed by the next `bits` input bits: (symbol << 4) | code length.
    table: Vec<u16>,
    bits: u32,
}

fn reverse(mut code: u32, len: u32) -> u32 {
    let mut r = 0;
    for _ in 0..len {
        r = (r << 1) | (code & 1);
        code >>= 1;
    }
    r
}

impl Huff {
    fn new(lens: &[u8]) -> Option<Huff> {
        let max = *lens.iter().max()? as u32;
        if max == 0 {
            return Some(Huff { table: vec![0], bits: 0 });
        }
        let mut count = [0u32; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut next = [0u32; 16];
        let mut code = 0;
        for b in 1..16 {
            code = (code + count[b - 1]) << 1;
            next[b] = code;
        }
        let mut table = vec![0u16; 1 << max];
        for (sym, &l) in lens.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let l = l as u32;
            let c = next[l as usize];
            next[l as usize] += 1;
            let mut i = reverse(c, l) as usize;
            while i < table.len() {
                table[i] = ((sym as u16) << 4) | l as u16;
                i += 1 << l;
            }
        }
        Some(Huff { table, bits: max })
    }
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u64,
    n: u32,
}

impl Bits<'_> {
    #[inline]
    fn refill(&mut self) {
        while self.n <= 56 {
            let b = *self.d.get(self.pos).unwrap_or(&0);
            self.pos += 1;
            self.buf |= (b as u64) << self.n;
            self.n += 8;
        }
    }
    #[inline]
    fn peek(&mut self, k: u32) -> u32 {
        if self.n < k {
            self.refill();
        }
        (self.buf & ((1u64 << k) - 1)) as u32
    }
    #[inline]
    fn consume(&mut self, k: u32) {
        self.buf >>= k;
        self.n -= k;
    }
    #[inline]
    fn bits(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        let v = self.peek(k);
        self.consume(k);
        v
    }
    #[inline]
    fn sym(&mut self, h: &Huff) -> Option<u32> {
        let e = h.table[self.peek(h.bits) as usize];
        let l = (e & 15) as u32;
        if l == 0 {
            return None;
        }
        self.consume(l);
        Some((e >> 4) as u32)
    }
    fn overrun(&self) -> bool {
        self.pos > self.d.len() + 16
    }
}

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Decompress a raw DEFLATE stream.
pub fn inflate(data: &[u8], out: &mut Vec<u8>) -> Option<()> {
    let mut b = Bits { d: data, pos: 0, buf: 0, n: 0 };
    loop {
        let last = b.bits(1);
        match b.bits(2) {
            0 => {
                let drop = b.n % 8;
                b.consume(drop);
                let len = b.bits(16) as usize;
                let _nlen = b.bits(16);
                for _ in 0..len {
                    out.push(b.bits(8) as u8);
                }
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Huff::new(&l)?, Huff::new(&[5u8; 30])?)
                } else {
                    let hlit = b.bits(5) as usize + 257;
                    let hdist = b.bits(5) as usize + 1;
                    let hclen = b.bits(4) as usize + 4;
                    const ORD: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &o in ORD.iter().take(hclen) {
                        cl[o] = b.bits(3) as u8;
                    }
                    let clh = Huff::new(&cl)?;
                    let mut lens = vec![0u8; hlit + hdist];
                    let mut i = 0;
                    while i < lens.len() {
                        let s = b.sym(&clh)?;
                        match s {
                            0..=15 => {
                                lens[i] = s as u8;
                                i += 1;
                            }
                            16 => {
                                let prev = *lens.get(i.checked_sub(1)?)?;
                                for _ in 0..3 + b.bits(2) {
                                    *lens.get_mut(i)? = prev;
                                    i += 1;
                                }
                            }
                            17 => i += 3 + b.bits(3) as usize,
                            _ => i += 11 + b.bits(7) as usize,
                        }
                    }
                    if i > lens.len() {
                        return None;
                    }
                    (Huff::new(&lens[..hlit])?, Huff::new(&lens[hlit..])?)
                };
                loop {
                    let s = b.sym(&lit)?;
                    if s < 256 {
                        out.push(s as u8);
                    } else if s == 256 {
                        break;
                    } else {
                        let k = (s - 257) as usize;
                        let len = *LBASE.get(k)? as usize + b.bits(*LEXT.get(k)? as u32) as usize;
                        let ds = b.sym(&dist)? as usize;
                        let d = *DBASE.get(ds)? as usize + b.bits(*DEXT.get(ds)? as u32) as usize;
                        if d > out.len() {
                            return None;
                        }
                        let start = out.len() - d;
                        if d >= len {
                            out.extend_from_within(start..start + len);
                        } else {
                            for j in 0..len {
                                let v = out[start + j];
                                out.push(v);
                            }
                        }
                    }
                    if b.overrun() {
                        return None;
                    }
                }
            }
            _ => return None,
        }
        if last == 1 || b.overrun() {
            break;
        }
    }
    Some(())
}

pub fn zlib(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 2 || data[0] & 0x0F != 8 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 4);
    inflate(&data[2..], &mut out)?;
    Some(out)
}

// ===================================================================== PNG

fn be32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?))
}

fn png(d: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    let mut i = 8;
    let (mut w, mut h, mut depth, mut ctype, mut interlace) = (0, 0, 0u8, 0u8, 0u8);
    let mut palette: Vec<u32> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat = Vec::new();
    while i + 8 <= d.len() {
        let len = be32(d, i)? as usize;
        let ty = &d[i + 4..i + 8];
        let body = d.get(i + 8..i + 8 + len)?;
        match ty {
            b"IHDR" => {
                w = be32(body, 0)?;
                h = be32(body, 4)?;
                depth = body[8];
                ctype = body[9];
                interlace = body[12];
            }
            b"PLTE" => palette = body.chunks_exact(3).map(|c| 0xFF00_0000 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect(),
            b"tRNS" => trns = body.to_vec(),
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        i += 12 + len;
    }
    if w == 0 || h == 0 || w > 30000 || h > 30000 {
        return None;
    }
    for (k, a) in trns.iter().enumerate() {
        if ctype == 3 && k < palette.len() {
            palette[k] = (palette[k] & 0xFF_FFFF) | (*a as u32) << 24;
        }
    }
    let raw = zlib(&idat)?;
    let channels = match ctype {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return None,
    };
    let bpp_bits = channels * depth as usize;
    let bpp = bpp_bits.div_ceil(8).max(1);
    let mut out = vec![0u32; (w * h) as usize];
    let passes: &[(u32, u32, u32, u32)] = if interlace == 1 {
        &[(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
    } else {
        &[(0, 0, 1, 1)]
    };
    let trns_key = |v: [u16; 3]| -> bool {
        match ctype {
            0 if trns.len() >= 2 => u16::from_be_bytes([trns[0], trns[1]]) == v[0],
            2 if trns.len() >= 6 => {
                u16::from_be_bytes([trns[0], trns[1]]) == v[0] && u16::from_be_bytes([trns[2], trns[3]]) == v[1] && u16::from_be_bytes([trns[4], trns[5]]) == v[2]
            }
            _ => false,
        }
    };
    let mut pos = 0;
    for &(x0, y0, dx, dy) in passes {
        if x0 >= w || y0 >= h {
            continue;
        }
        let pw = (w - x0).div_ceil(dx);
        let ph = (h - y0).div_ceil(dy);
        let stride = (pw as usize * bpp_bits).div_ceil(8);
        let mut prev = vec![0u8; stride];
        let mut cur = vec![0u8; stride];
        for py in 0..ph {
            let filter = *raw.get(pos)?;
            let line = raw.get(pos + 1..pos + 1 + stride)?;
            pos += 1 + stride;
            for x in 0..stride {
                let a = if x >= bpp { cur[x - bpp] as i32 } else { 0 };
                let b = prev[x] as i32;
                let c = if x >= bpp { prev[x - bpp] as i32 } else { 0 };
                let v = line[x] as i32;
                cur[x] = match filter {
                    0 => v,
                    1 => v + a,
                    2 => v + b,
                    3 => v + (a + b) / 2,
                    4 => {
                        let p = a + b - c;
                        let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                        v + if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
                    }
                    _ => return None,
                } as u8;
            }
            let sample = |idx: usize| -> u16 {
                match depth {
                    16 => u16::from_be_bytes([cur[idx * 2], cur[idx * 2 + 1]]),
                    8 => cur[idx] as u16,
                    _ => {
                        let bit = idx * depth as usize;
                        let byte = cur[bit / 8];
                        let shift = 8 - depth as usize - bit % 8;
                        ((byte >> shift) & ((1u16 << depth) - 1) as u8) as u16
                    }
                }
            };
            let to8 = |v: u16| -> u32 {
                match depth {
                    16 => (v >> 8) as u32,
                    8 => v as u32,
                    1 => v as u32 * 255,
                    2 => v as u32 * 85,
                    4 => v as u32 * 17,
                    _ => v as u32,
                }
            };
            let y = y0 + py * dy;
            for px in 0..pw {
                let x = x0 + px * dx;
                let k = px as usize * channels;
                let c = match ctype {
                    3 => *palette.get(sample(k) as usize).unwrap_or(&0xFF00_0000),
                    0 => {
                        let v = sample(k);
                        let g = to8(v);
                        (if trns_key([v, 0, 0]) { 0 } else { 0xFF00_0000 }) | g << 16 | g << 8 | g
                    }
                    2 => {
                        let (r, g, b) = (sample(k), sample(k + 1), sample(k + 2));
                        (if trns_key([r, g, b]) { 0 } else { 0xFF00_0000 }) | to8(r) << 16 | to8(g) << 8 | to8(b)
                    }
                    4 => {
                        let g = to8(sample(k));
                        to8(sample(k + 1)) << 24 | g << 16 | g << 8 | g
                    }
                    _ => to8(sample(k + 3)) << 24 | to8(sample(k)) << 16 | to8(sample(k + 1)) << 8 | to8(sample(k + 2)),
                };
                out[(y * w + x) as usize] = c;
            }
            std::mem::swap(&mut prev, &mut cur);
        }
    }
    Some((w, h, out))
}

// ===================================================================== GIF

fn gif(d: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    let le16 = |i: usize| -> Option<u32> { Some(u16::from_le_bytes([*d.get(i)?, *d.get(i + 1)?]) as u32) };
    let (w, h) = (le16(6)?, le16(8)?);
    let flags = *d.get(10)?;
    let mut i = 13;
    let read_table = |i: &mut usize, n: usize| -> Option<Vec<u32>> {
        let t = d.get(*i..*i + 3 * n)?.chunks_exact(3).map(|c| 0xFF00_0000 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect();
        *i += 3 * n;
        Some(t)
    };
    let global = if flags & 0x80 != 0 { read_table(&mut i, 2 << (flags & 7))? } else { Vec::new() };
    let mut transparent: Option<u8> = None;
    let mut out = vec![0u32; (w * h) as usize];
    loop {
        match *d.get(i)? {
            0x21 => {
                let label = *d.get(i + 1)?;
                i += 2;
                if label == 0xF9 && *d.get(i)? >= 4 && d[i + 1] & 1 != 0 {
                    transparent = Some(*d.get(i + 4)?);
                }
                while *d.get(i)? != 0 {
                    i += 1 + d[i] as usize;
                }
                i += 1;
            }
            0x2C => {
                let (fx, fy, fw, fh) = (le16(i + 1)?, le16(i + 3)?, le16(i + 5)?, le16(i + 7)?);
                let f = *d.get(i + 9)?;
                i += 10;
                let local = if f & 0x80 != 0 { Some(read_table(&mut i, 2 << (f & 7))?) } else { None };
                let table = local.as_ref().unwrap_or(&global);
                let min = *d.get(i)? as u32;
                i += 1;
                let mut data = Vec::new();
                while *d.get(i)? != 0 {
                    let n = d[i] as usize;
                    data.extend_from_slice(d.get(i + 1..i + 1 + n)?);
                    i += 1 + n;
                }
                let idx = lzw(&data, min, (fw * fh) as usize)?;
                let rows: Vec<u32> = if f & 0x40 != 0 {
                    let mut r = Vec::new();
                    for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
                        r.extend((start..fh).step_by(step));
                    }
                    r
                } else {
                    (0..fh).collect()
                };
                for (ri, &ry) in rows.iter().enumerate() {
                    for x in 0..fw {
                        let Some(&c) = idx.get(ri * fw as usize + x as usize) else { continue };
                        if Some(c) == transparent {
                            continue;
                        }
                        let (ox, oy) = (fx + x, fy + ry);
                        if ox < w && oy < h {
                            out[(oy * w + ox) as usize] = *table.get(c as usize).unwrap_or(&0);
                        }
                    }
                }
                return Some((w, h, out));
            }
            _ => return None,
        }
    }
}

fn lzw(data: &[u8], min: u32, cap: usize) -> Option<Vec<u8>> {
    if !(2..=11).contains(&min) {
        return None;
    }
    let clear = 1u32 << min;
    let end = clear + 1;
    let mut prefix = vec![0u16; 4096];
    let mut suffix = vec![0u8; 4096];
    let mut first = vec![0u8; 4096];
    for c in 0..clear {
        suffix[c as usize] = c as u8;
        first[c as usize] = c as u8;
    }
    let mut out = Vec::with_capacity(cap);
    let (mut size, mut next, mut prev) = (min + 1, end + 1, None::<u32>);
    let (mut buf, mut n, mut pos) = (0u32, 0u32, 0usize);
    let mut stack = Vec::with_capacity(4096);
    loop {
        while n < size {
            if pos >= data.len() {
                return Some(out);
            }
            buf |= (data[pos] as u32) << n;
            pos += 1;
            n += 8;
        }
        let code = buf & ((1 << size) - 1);
        buf >>= size;
        n -= size;
        if code == clear {
            size = min + 1;
            next = end + 1;
            prev = None;
            continue;
        }
        if code == end {
            return Some(out);
        }
        let Some(p) = prev else {
            out.push(suffix[code as usize]);
            prev = Some(code);
            continue;
        };
        let k = if code < next { first[code as usize] } else { first[p as usize] };
        if next < 4096 {
            prefix[next as usize] = p as u16;
            suffix[next as usize] = k;
            first[next as usize] = first[p as usize];
            next += 1;
            if next == 1 << size && size < 12 {
                size += 1;
            }
        }
        if code >= next {
            return Some(out);
        }
        let mut c = code;
        stack.clear();
        while c >= clear {
            stack.push(suffix[c as usize]);
            c = prefix[c as usize] as u32;
        }
        stack.push(suffix[c as usize]);
        out.extend(stack.iter().rev());
        if out.len() >= cap {
            out.truncate(cap);
            return Some(out);
        }
        prev = Some(code);
    }
}

// ==================================================================== JPEG

const DEZIGZAG: [usize; 80] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50,
    43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63,
    63, 63, 63, 63, 63, 63, 63,
];

#[derive(Clone, Default)]
struct JHuff {
    fast: Vec<u16>, // 9-bit lookup: (len << 8) | symbol, 0 = slow path
    maxcode: [i32; 18],
    valptr: [i32; 17],
    mincode: [i32; 17],
    vals: Vec<u8>,
}

impl JHuff {
    fn new(counts: &[u8; 16], vals: &[u8]) -> JHuff {
        let mut h = JHuff { fast: vec![0; 512], vals: vals.to_vec(), ..Default::default() };
        let mut code = 0i32;
        let mut k = 0usize;
        for len in 1..=16 {
            let n = counts[len - 1] as usize;
            h.valptr[len] = k as i32;
            h.mincode[len] = code;
            for _ in 0..n {
                if len <= 9 && k < vals.len() {
                    let shift = 9 - len;
                    for f in 0..(1 << shift) {
                        h.fast[((code as usize) << shift) | f] = ((len as u16) << 8) | vals[k] as u16;
                    }
                }
                code += 1;
                k += 1;
            }
            h.maxcode[len] = if n > 0 { code - 1 } else { -1 };
            code <<= 1;
        }
        h.maxcode[17] = i32::MAX;
        h
    }
}

struct JBits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u32,
    n: u32,
    marker: bool,
}

impl JBits<'_> {
    #[inline]
    fn fill(&mut self) {
        while self.n <= 24 {
            let mut b = 0u32;
            if !self.marker && self.pos < self.d.len() {
                let v = self.d[self.pos];
                if v == 0xFF {
                    let nx = *self.d.get(self.pos + 1).unwrap_or(&0xD9);
                    if nx == 0 {
                        self.pos += 2;
                        b = 0xFF;
                    } else {
                        self.marker = true;
                    }
                } else {
                    self.pos += 1;
                    b = v as u32;
                }
            }
            self.buf |= b << (24 - self.n);
            self.n += 8;
        }
    }
    #[inline]
    fn peek(&mut self, k: u32) -> u32 {
        if self.n < k {
            self.fill();
        }
        self.buf >> (32 - k)
    }
    #[inline]
    fn skip(&mut self, k: u32) {
        self.buf <<= k;
        self.n -= k;
    }
    #[inline]
    fn bits(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        let v = self.peek(k);
        self.skip(k);
        v
    }
    #[inline]
    fn bit(&mut self) -> bool {
        self.bits(1) == 1
    }
    #[inline]
    fn receive_extend(&mut self, s: u32) -> i32 {
        if s == 0 {
            return 0;
        }
        let v = self.bits(s) as i32;
        if v < 1 << (s - 1) { v - (1 << s) + 1 } else { v }
    }
    fn decode(&mut self, h: &JHuff) -> Option<u8> {
        let f = h.fast[self.peek(9) as usize];
        if f != 0 {
            self.skip((f >> 8) as u32);
            return Some(f as u8);
        }
        for len in 10..=16 {
            let code = self.peek(len) as i32;
            if code <= h.maxcode[len as usize] {
                self.skip(len);
                let idx = h.valptr[len as usize] + code - h.mincode[len as usize];
                return h.vals.get(idx as usize).copied();
            }
        }
        None
    }
    fn restart(&mut self) {
        self.buf = 0;
        self.n = 0;
        self.marker = false;
        if self.pos + 1 < self.d.len() && self.d[self.pos] == 0xFF && (0xD0..=0xD7).contains(&self.d[self.pos + 1]) {
            self.pos += 2;
        }
    }
}

struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// Blocks per line / rows of blocks (padded to whole MCUs).
    bw: usize,
    bh: usize,
    coefs: Vec<i16>,
    dc_pred: i32,
    td: usize,
    ta: usize,
}

fn jpeg(d: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    let mut i = 2;
    let mut qt = [[0u16; 64]; 4];
    let mut dc: [JHuff; 4] = Default::default();
    let mut ac: [JHuff; 4] = Default::default();
    let mut comps: Vec<Comp> = Vec::new();
    let (mut w, mut h) = (0usize, 0usize);
    let (mut hmax, mut vmax) = (1usize, 1usize);
    let mut restart = 0usize;
    let mut progressive = false;
    let mut adobe: Option<u8> = None;
    let mut eobrun;
    let be16 = |i: usize| -> Option<usize> { Some(u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]) as usize) };
    loop {
        while *d.get(i)? != 0xFF {
            i += 1;
        }
        while *d.get(i)? == 0xFF {
            i += 1;
        }
        let m = d[i];
        i += 1;
        match m {
            0xD8 | 0x01 | 0xD0..=0xD7 => continue,
            0xD9 => break,
            _ => {}
        }
        let len = be16(i)?;
        let seg = d.get(i + 2..i + len)?;
        match m {
            0xDB => {
                let mut p = 0;
                while p < seg.len() {
                    let (prec, t) = (seg[p] >> 4, (seg[p] & 3) as usize);
                    p += 1;
                    for k in 0..64 {
                        let v = if prec == 0 {
                            *seg.get(p + k)? as u16
                        } else {
                            u16::from_be_bytes([*seg.get(p + 2 * k)?, *seg.get(p + 2 * k + 1)?])
                        };
                        qt[t][DEZIGZAG[k]] = v;
                    }
                    p += if prec == 0 { 64 } else { 128 };
                }
            }
            0xC4 => {
                let mut p = 0;
                while p + 17 <= seg.len() {
                    let (class, t) = (seg[p] >> 4, (seg[p] & 3) as usize);
                    let counts: [u8; 16] = seg[p + 1..p + 17].try_into().ok()?;
                    let n: usize = counts.iter().map(|&c| c as usize).sum();
                    let vals = seg.get(p + 17..p + 17 + n)?;
                    let table = JHuff::new(&counts, vals);
                    if class == 0 {
                        dc[t] = table;
                    } else {
                        ac[t] = table;
                    }
                    p += 17 + n;
                }
            }
            0xDD => restart = be16(i + 2)?,
            0xEE => {
                if seg.starts_with(b"Adobe") && seg.len() >= 12 {
                    adobe = Some(seg[11]);
                }
            }
            0xC0 | 0xC1 | 0xC2 => {
                progressive = m == 0xC2;
                h = be16(i + 3)?;
                w = be16(i + 5)?;
                let n = *seg.get(5)? as usize;
                if w == 0 || h == 0 || w > 30000 || h > 30000 || n == 0 || n > 4 {
                    return None;
                }
                for k in 0..n {
                    let c = seg.get(6 + 3 * k..9 + 3 * k)?;
                    let (hs, vs) = ((c[1] >> 4).max(1) as usize, (c[1] & 15).max(1) as usize);
                    hmax = hmax.max(hs);
                    vmax = vmax.max(vs);
                    comps.push(Comp { id: c[0], h: hs, v: vs, tq: (c[2] & 3) as usize, bw: 0, bh: 0, coefs: Vec::new(), dc_pred: 0, td: 0, ta: 0 });
                }
                let (mcux, mcuy) = (w.div_ceil(8 * hmax), h.div_ceil(8 * vmax));
                for c in &mut comps {
                    c.bw = mcux * c.h;
                    c.bh = mcuy * c.v;
                    c.coefs = vec![0; c.bw * c.bh * 64];
                }
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None, // lossless / arithmetic
            0xDA => {
                let ns = *seg.first()? as usize;
                let mut sc = Vec::new();
                for k in 0..ns {
                    let id = *seg.get(1 + 2 * k)?;
                    let t = *seg.get(2 + 2 * k)?;
                    let ci = comps.iter().position(|c| c.id == id)?;
                    comps[ci].td = (t >> 4) as usize & 3;
                    comps[ci].ta = (t & 15) as usize & 3;
                    sc.push(ci);
                }
                let p = 1 + 2 * ns;
                let (ss, se) = (*seg.get(p)? as usize, (*seg.get(p + 1)? as usize).min(63));
                let (ah, al) = ((*seg.get(p + 2)? >> 4) as u32, (*seg.get(p + 2)? & 15) as u32);
                let mut bits = JBits { d, pos: i + len, buf: 0, n: 0, marker: false };
                for c in &mut comps {
                    c.dc_pred = 0;
                }
                eobrun = 0;
                let (mcux, mcuy) = (w.div_ceil(8 * hmax), h.div_ceil(8 * vmax));
                let mut todo = restart;
                let mut tick = |bits: &mut JBits, comps: &mut Vec<Comp>, eobrun: &mut u32| {
                    if restart > 0 {
                        todo -= 1;
                        if todo == 0 {
                            todo = restart;
                            bits.restart();
                            for c in comps.iter_mut() {
                                c.dc_pred = 0;
                            }
                            *eobrun = 0;
                        }
                    }
                };
                if sc.len() == 1 {
                    // Non-interleaved: iterate the component's own blocks.
                    let ci = sc[0];
                    let c = &comps[ci];
                    let cw = (w * c.h).div_ceil(hmax).div_ceil(8);
                    let chh = (h * c.v).div_ceil(vmax).div_ceil(8);
                    for by in 0..chh {
                        for bx in 0..cw {
                            let bi = by * comps[ci].bw + bx;
                            decode_block(&mut bits, &mut comps[ci], bi, &dc, &ac, progressive, ss, se, ah, al, &mut eobrun)?;
                            tick(&mut bits, &mut comps, &mut eobrun);
                        }
                    }
                } else {
                    for my in 0..mcuy {
                        for mx in 0..mcux {
                            for &ci in &sc {
                                let (hs, vs) = (comps[ci].h, comps[ci].v);
                                for y in 0..vs {
                                    for x in 0..hs {
                                        let bi = (my * vs + y) * comps[ci].bw + mx * hs + x;
                                        decode_block(&mut bits, &mut comps[ci], bi, &dc, &ac, progressive, ss, se, ah, al, &mut eobrun)?;
                                    }
                                }
                            }
                            tick(&mut bits, &mut comps, &mut eobrun);
                        }
                    }
                }
                i = bits.pos;
                continue;
            }
            _ => {}
        }
        i += len;
    }
    if comps.is_empty() {
        return None;
    }
    // Dequantize + IDCT into planes.
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(comps.len());
    for c in &comps {
        let pw = c.bw * 8;
        let mut plane = vec![0u8; pw * c.bh * 8];
        let q = &qt[c.tq];
        let mut blk = [0i32; 64];
        for by in 0..c.bh {
            for bx in 0..c.bw {
                let co = &c.coefs[(by * c.bw + bx) * 64..][..64];
                for k in 0..64 {
                    blk[k] = co[k] as i32 * q[k] as i32;
                }
                idct(&blk, &mut plane[by * 8 * pw + bx * 8..], pw);
            }
        }
        planes.push(plane);
    }
    let mut out = vec![0u32; w * h];
    let n = comps.len();
    let transform = adobe.unwrap_or(if n == 3 { 1 } else { 0 });
    // Centered bilinear upsampling ("fancy upsampling") for subsampled planes:
    // per output coordinate, two source taps and an 8-bit weight.
    let taps = |len: usize, num: usize, den: usize, plen: usize| -> Vec<(u32, u32, u32)> {
        (0..len)
            .map(|x| {
                if num == den {
                    return (x as u32, x as u32, 0);
                }
                let f = ((x as f32 + 0.5) * num as f32 / den as f32 - 0.5).max(0.0);
                let x0 = (f as usize).min(plen - 1);
                let x1 = (x0 + 1).min(plen - 1);
                (x0 as u32, x1 as u32, ((f - x0 as f32) * 256.0) as u32)
            })
            .collect()
    };
    let xt: Vec<Vec<(u32, u32, u32)>> = comps.iter().map(|c| taps(w, c.h, hmax, (w * c.h).div_ceil(hmax))).collect();
    let yt: Vec<Vec<(u32, u32, u32)>> = comps.iter().map(|c| taps(h, c.v, vmax, (h * c.v).div_ceil(vmax))).collect();
    for y in 0..h {
        for x in 0..w {
            let s = |k: usize| -> i32 {
                let c = &comps[k];
                let stride = c.bw * 8;
                let pl = &planes[k];
                let (x0, x1, wx) = xt[k][x];
                let (y0, y1, wy) = yt[k][y];
                if wx == 0 && wy == 0 {
                    return pl[y0 as usize * stride + x0 as usize] as i32;
                }
                let p = |xx: u32, yy: u32| pl[yy as usize * stride + xx as usize] as u32;
                let top = p(x0, y0) * (256 - wx) + p(x1, y0) * wx;
                let bot = p(x0, y1) * (256 - wx) + p(x1, y1) * wx;
                ((top * (256 - wy) + bot * wy + 32768) >> 16) as i32
            };
            let (r, g, b) = match n {
                1 | 2 => {
                    let v = s(0);
                    (v, v, v)
                }
                3 if transform == 0 => (s(0), s(1), s(2)),
                3 => ycc(s(0), s(1), s(2)),
                _ => {
                    let (c0, c1, c2, k) = (s(0), s(1), s(2), s(3));
                    let (c, m, yy) = if transform == 2 { ycc(c0, c1, c2) } else { (c0, c1, c2) };
                    // Adobe CMYK is stored inverted.
                    (c * k / 255, m * k / 255, yy * k / 255)
                }
            };
            out[y * w + x] = 0xFF00_0000 | (r.clamp(0, 255) as u32) << 16 | (g.clamp(0, 255) as u32) << 8 | b.clamp(0, 255) as u32;
        }
    }
    Some((w as u32, h as u32, out))
}

#[inline]
fn ycc(y: i32, cb: i32, cr: i32) -> (i32, i32, i32) {
    let (cb, cr) = (cb - 128, cr - 128);
    let y = y << 16;
    ((y + 91881 * cr + 32768) >> 16, (y - 22554 * cb - 46802 * cr + 32768) >> 16, (y + 116130 * cb + 32768) >> 16)
}

#[allow(clippy::too_many_arguments)]
fn decode_block(
    bits: &mut JBits,
    c: &mut Comp,
    bi: usize,
    dc: &[JHuff; 4],
    ac: &[JHuff; 4],
    progressive: bool,
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
    eobrun: &mut u32,
) -> Option<()> {
    let data = c.coefs.get_mut(bi * 64..bi * 64 + 64)?;
    if !progressive {
        let t = bits.decode(&dc[c.td])? as u32;
        c.dc_pred += bits.receive_extend(t);
        data[0] = c.dc_pred as i16;
        let mut k = 1;
        while k < 64 {
            let rs = bits.decode(&ac[c.ta])?;
            let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
            if s == 0 {
                if r != 15 {
                    break;
                }
                k += 16;
                continue;
            }
            k += r;
            if k > 63 {
                break;
            }
            data[DEZIGZAG[k]] = bits.receive_extend(s) as i16;
            k += 1;
        }
        return Some(());
    }
    if ss == 0 {
        // DC scans (AC never shares a scan with DC in progressive mode).
        if ah == 0 {
            let t = bits.decode(&dc[c.td])? as u32;
            c.dc_pred += bits.receive_extend(t);
            data[0] = (c.dc_pred * (1 << al)) as i16;
        } else if bits.bit() {
            data[0] |= (1 << al) as i16;
        }
        return Some(());
    }
    let h = &ac[c.ta];
    if ah == 0 {
        if *eobrun > 0 {
            *eobrun -= 1;
            return Some(());
        }
        let mut k = ss;
        while k <= se {
            let rs = bits.decode(h)?;
            let (r, s) = ((rs >> 4) as u32, (rs & 15) as u32);
            if s == 0 {
                if r < 15 {
                    *eobrun = (1 << r) - 1;
                    if r > 0 {
                        *eobrun += bits.bits(r);
                    }
                    break;
                }
                k += 16;
            } else {
                k += r as usize;
                if k > 63 {
                    break;
                }
                data[DEZIGZAG[k]] = (bits.receive_extend(s) * (1 << al)) as i16;
                k += 1;
            }
        }
        return Some(());
    }
    // AC successive-approximation refinement.
    let bit = (1i32 << al) as i16;
    if *eobrun > 0 {
        *eobrun -= 1;
        for k in ss..=se {
            let p = &mut data[DEZIGZAG[k]];
            if *p != 0 && bits.bit() && *p & bit == 0 {
                *p += if *p > 0 { bit } else { -bit };
            }
        }
        return Some(());
    }
    let mut k = ss;
    while k <= se {
        let rs = bits.decode(h)?;
        let (mut r, s) = ((rs >> 4) as i32, (rs & 15) as u32);
        let mut val = 0i16;
        if s == 0 {
            if r < 15 {
                *eobrun = (1 << r) - 1;
                if r > 0 {
                    *eobrun += bits.bits(r as u32);
                }
                r = 64; // refine the rest of this block, then stop
            }
        } else {
            val = if bits.bit() { bit } else { -bit };
        }
        while k <= se {
            let p = &mut data[DEZIGZAG[k]];
            k += 1;
            if *p != 0 {
                if bits.bit() && *p & bit == 0 {
                    *p += if *p > 0 { bit } else { -bit };
                }
            } else {
                if r == 0 {
                    *p = val;
                    break;
                }
                r -= 1;
            }
        }
    }
    Some(())
}

/// Integer IDCT (the jidctint / stb_image formulation).
fn idct(d: &[i32; 64], out: &mut [u8], stride: usize) {
    #[inline(always)]
    fn f2f(x: f32) -> i32 {
        (x * 4096.0 + 0.5) as i32
    }
    #[inline(always)]
    fn idct1d(s: [i32; 8]) -> ([i32; 4], [i32; 4]) {
        let p2 = s[2];
        let p3 = s[6];
        let p1 = (p2 + p3) * f2f(0.541_196_1);
        let t2 = p1 + p3 * f2f(-1.847_759_1);
        let t3 = p1 + p2 * f2f(0.765_366_85);
        let t0 = (s[0] + s[4]) * 4096;
        let t1 = (s[0] - s[4]) * 4096;
        let x0 = t0 + t3;
        let x3 = t0 - t3;
        let x1 = t1 + t2;
        let x2 = t1 - t2;
        let (mut t0, mut t1, mut t2, mut t3) = (s[7], s[5], s[3], s[1]);
        let p3 = t0 + t2;
        let p4 = t1 + t3;
        let p1 = t0 + t3;
        let p2 = t1 + t2;
        let p5 = (p3 + p4) * f2f(1.175_875_6);
        t0 *= f2f(0.298_631_34);
        t1 *= f2f(2.053_119_9);
        t2 *= f2f(3.072_711);
        t3 *= f2f(1.501_321_1);
        let p1 = p5 + p1 * f2f(-0.899_976_2);
        let p2 = p5 + p2 * f2f(-2.562_915_4);
        let p3 = p3 * f2f(-1.961_570_6);
        let p4 = p4 * f2f(-0.390_180_64);
        t3 += p1 + p4;
        t2 += p2 + p3;
        t1 += p2 + p4;
        t0 += p1 + p3;
        ([x0, x1, x2, x3], [t0, t1, t2, t3])
    }
    let mut v = [0i32; 64];
    for i in 0..8 {
        if (1..8).all(|r| d[r * 8 + i] == 0) {
            let dc = d[i] * 4;
            for r in 0..8 {
                v[r * 8 + i] = dc;
            }
            continue;
        }
        let ([mut x0, mut x1, mut x2, mut x3], [t0, t1, t2, t3]) = idct1d(std::array::from_fn(|r| d[r * 8 + i]));
        x0 += 512;
        x1 += 512;
        x2 += 512;
        x3 += 512;
        v[i] = (x0 + t3) >> 10;
        v[56 + i] = (x0 - t3) >> 10;
        v[8 + i] = (x1 + t2) >> 10;
        v[48 + i] = (x1 - t2) >> 10;
        v[16 + i] = (x2 + t1) >> 10;
        v[40 + i] = (x2 - t1) >> 10;
        v[24 + i] = (x3 + t0) >> 10;
        v[32 + i] = (x3 - t0) >> 10;
    }
    for r in 0..8 {
        let ([mut x0, mut x1, mut x2, mut x3], [t0, t1, t2, t3]) = idct1d(std::array::from_fn(|c| v[r * 8 + c]));
        let bias = 65536 + (128 << 17);
        x0 += bias;
        x1 += bias;
        x2 += bias;
        x3 += bias;
        let o = &mut out[r * stride..r * stride + 8];
        let cl = |v: i32| (v >> 17).clamp(0, 255) as u8;
        o[0] = cl(x0 + t3);
        o[7] = cl(x0 - t3);
        o[1] = cl(x1 + t2);
        o[6] = cl(x1 - t2);
        o[2] = cl(x2 + t1);
        o[5] = cl(x2 - t1);
        o[3] = cl(x3 + t0);
        o[4] = cl(x3 - t0);
    }
}

// ================================================================== base64

/// Decode a `data:` URL payload.
pub fn data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let (meta, payload) = (&rest[..comma], &rest[comma + 1..]);
    if !meta.ends_with(";base64") {
        // Percent-encoded text (e.g. data:image/svg+xml,%3Csvg...).
        let b = payload.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        return Some(out);
    }
    let mut out = Vec::with_capacity(payload.len() * 3 / 4);
    let (mut buf, mut n) = (0u32, 0);
    for c in payload.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        };
        buf = (buf << 6) | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((buf >> n) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        // Our own encoder writes stored deflate blocks; the decoder must read them.
        let px: Vec<u32> = (0..64 * 40).map(|i| 0xFF00_0000 | (i * 2654435761u32) & 0xFF_FFFF).collect();
        let enc = crate::png::encode(&px, 64, 40);
        let img = decode(&enc).expect("decode");
        assert_eq!((img.w, img.h), (64, 40));
        assert_eq!(img.px, px);
    }

    #[test]
    fn inflate_fixed_and_dynamic() {
        let z = [120, 218, 203, 72, 205, 201, 201, 87, 200, 64, 39, 1, 104, 3, 8, 177];
        assert_eq!(&zlib(&z).unwrap(), b"hello hello hello hello");
        let big = zlib(include_bytes!("../testdata/z_out.bin")).unwrap();
        assert_eq!(&big[..], &include_bytes!("../testdata/z_in.bin")[..]);
    }

    /// Compare against Pillow's decode: (mean abs error, max error).
    fn compare(file: &[u8], reference: &[u8]) -> (f64, u32) {
        let img = decode(file).expect("decode");
        assert_eq!((img.w * img.h * 4) as usize, reference.len());
        let (mut sum, mut max) = (0u64, 0u32);
        for (i, &p) in img.px.iter().enumerate() {
            let r = &reference[i * 4..i * 4 + 4];
            let ours = [(p >> 16) & 255, (p >> 8) & 255, p & 255, p >> 24];
            for k in 0..4 {
                let d = (ours[k] as i32 - r[k] as i32).unsigned_abs();
                sum += d as u64;
                max = max.max(d);
            }
        }
        (sum as f64 / reference.len() as f64, max)
    }

    macro_rules! fixture {
        ($name:literal) => {
            (include_bytes!(concat!("../testdata/", $name)).as_slice(), include_bytes!(concat!("../testdata/", $name, ".rgba")).as_slice())
        };
    }

    #[test]
    fn png_and_gif_exact() {
        for (f, r) in [fixture!("rgba.png"), fixture!("pal.png"), fixture!("img.gif")] {
            assert_eq!(compare(f, r), (0.0, 0));
        }
    }

    #[test]
    fn jpeg_close_to_reference() {
        for (name, (f, r)) in [("base444", fixture!("base444.jpg")), ("base420", fixture!("base420.jpg")), ("prog", fixture!("prog.jpg")), ("gray", fixture!("gray.jpg"))] {
            let (mean, max) = compare(f, r);
            assert!(mean < 2.5 && max < 64, "{name}: mean {mean:.2} max {max}");
        }
    }

    #[test]
    fn png_16bit() {
        let img = decode(include_bytes!("../testdata/gray16.png")).unwrap();
        let (x, y) = (10u32, 7u32);
        let v = ((x * 1200 + y * 300) % 65536) >> 8;
        assert_eq!(img.px[(y * img.w + x) as usize] & 0xFF, v);
    }

    #[test]
    fn base64() {
        assert_eq!(data_url("data:text/plain;base64,aGVsbG8=").unwrap(), b"hello");
    }
}
