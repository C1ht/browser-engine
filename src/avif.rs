//! AVIF: the HEIF/ISOBMFF container around AV1 images. Supports the primary
//! image (plain or grid), an alpha auxiliary image, rotation/mirroring, and
//! YUV to RGB conversion using the signalled (nclx) or bitstream colour info.

use crate::av1;

fn be16(d: &[u8], i: usize) -> Option<u32> {
    Some(u16::from_be_bytes(d.get(i..i + 2)?.try_into().ok()?) as u32)
}
fn be32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?))
}
fn be_n(d: &[u8], i: usize, n: usize) -> Option<u64> {
    let mut v = 0u64;
    for k in 0..n {
        v = (v << 8) | *d.get(i + k)? as u64;
    }
    Some(v)
}

/// Iterate boxes: (type, body).
fn boxes(mut d: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 8 {
        let mut size = u32::from_be_bytes(d[0..4].try_into().unwrap()) as u64;
        let ty: [u8; 4] = d[4..8].try_into().unwrap();
        let mut hdr = 8;
        if size == 1 {
            let Some(s) = be_n(d, 8, 8) else { break };
            size = s;
            hdr = 16;
        } else if size == 0 {
            size = d.len() as u64;
        }
        let size = (size as usize).min(d.len());
        if size < hdr {
            break;
        }
        out.push((ty, &d[hdr..size]));
        d = &d[size..];
    }
    out
}

#[derive(Default, Clone)]
struct Item {
    id: u32,
    ty: [u8; 4],
    /// (offset, length) extents and construction method (0 file, 1 idat).
    extents: Vec<(u64, u64)>,
    method: u8,
    props: Vec<usize>,
}

#[derive(Clone, Debug)]
enum Prop {
    Colr { primaries: u32, matrix: u32, full: bool },
    AuxC(String),
    Irot(u8),
    Imir(u8),
    Other,
}

struct Container<'a> {
    file: &'a [u8],
    idat: &'a [u8],
    items: Vec<Item>,
    props: Vec<Prop>,
    primary: u32,
    /// (type, from, to list)
    refs: Vec<([u8; 4], u32, Vec<u32>)>,
}

fn parse(file: &[u8]) -> Option<Container<'_>> {
    let top = boxes(file);
    let meta = top.iter().find(|(t, _)| t == b"meta")?.1;
    let meta = boxes(meta.get(4..)?);
    let mut c = Container { file, idat: &[], items: Vec::new(), props: Vec::new(), primary: 0, refs: Vec::new() };
    for (t, b) in &meta {
        match t {
            b"pitm" => {
                c.primary = if b.first()? == &0 { be16(b, 4)? } else { be32(b, 4)? };
            }
            b"idat" => c.idat = b,
            b"iinf" => {
                let v = *b.first()?;
                let off = if v == 0 { 6 } else { 8 };
                for (t, e) in boxes(b.get(off..)?) {
                    if &t != b"infe" {
                        continue;
                    }
                    let ver = *e.first()?;
                    if ver < 2 {
                        continue;
                    }
                    let (id, p) = if ver == 2 { (be16(e, 4)?, 6) } else { (be32(e, 4)?, 8) };
                    let ty: [u8; 4] = e.get(p + 2..p + 6)?.try_into().ok()?;
                    c.items.push(Item { id, ty, ..Default::default() });
                }
            }
            _ => {}
        }
    }
    for (t, b) in &meta {
        match t {
            b"iloc" => {
                let ver = *b.first()?;
                let s1 = *b.get(4)?;
                let s2 = *b.get(5)?;
                let (off_sz, len_sz, base_sz) = ((s1 >> 4) as usize, (s1 & 15) as usize, (s2 >> 4) as usize);
                let idx_sz = if ver == 1 || ver == 2 { (s2 & 15) as usize } else { 0 };
                let mut p = 6;
                let count = if ver < 2 {
                    p += 2;
                    be16(b, 4 + 2)? as usize
                } else {
                    p += 4;
                    be32(b, 6)? as usize
                };
                for _ in 0..count {
                    let id = if ver < 2 {
                        p += 2;
                        be16(b, p - 2)?
                    } else {
                        p += 4;
                        be32(b, p - 4)?
                    };
                    let mut method = 0u8;
                    if ver == 1 || ver == 2 {
                        method = (be16(b, p)? & 15) as u8;
                        p += 2;
                    }
                    p += 2; // data_reference_index
                    let base = be_n(b, p, base_sz)?;
                    p += base_sz;
                    let n = be16(b, p)? as usize;
                    p += 2;
                    let mut extents = Vec::new();
                    for _ in 0..n {
                        p += idx_sz;
                        let o = be_n(b, p, off_sz)?;
                        p += off_sz;
                        let l = be_n(b, p, len_sz)?;
                        p += len_sz;
                        extents.push((base + o, l));
                    }
                    if let Some(it) = c.items.iter_mut().find(|i| i.id == id) {
                        it.extents = extents;
                        it.method = method;
                    }
                }
            }
            b"iprp" => {
                for (t, b) in boxes(b) {
                    if &t == b"ipco" {
                        for (t, p) in boxes(b) {
                            c.props.push(match &t {
                                b"colr" if p.get(0..4) == Some(b"nclx") => Prop::Colr {
                                    primaries: be16(p, 4)?,
                                    matrix: be16(p, 8)?,
                                    full: p.get(10)? & 0x80 != 0,
                                },
                                b"auxC" => {
                                    let s = p.get(4..)?;
                                    let end = s.iter().position(|&x| x == 0).unwrap_or(s.len());
                                    Prop::AuxC(String::from_utf8_lossy(&s[..end]).into_owned())
                                }
                                b"irot" => Prop::Irot(p.first()? & 3),
                                b"imir" => Prop::Imir(p.first()? & 1),
                                _ => Prop::Other,
                            });
                        }
                    } else if &t == b"ipma" {
                        let ver = *b.first()?;
                        let flags = be32(b, 0)? & 0xFFFFFF;
                        let n = be32(b, 4)? as usize;
                        let mut p = 8;
                        for _ in 0..n {
                            let id = if ver < 1 {
                                p += 2;
                                be16(b, p - 2)?
                            } else {
                                p += 4;
                                be32(b, p - 4)?
                            };
                            let k = *b.get(p)? as usize;
                            p += 1;
                            let mut list = Vec::new();
                            for _ in 0..k {
                                let idx = if flags & 1 != 0 {
                                    p += 2;
                                    (be16(b, p - 2)? & 0x7FFF) as usize
                                } else {
                                    p += 1;
                                    (*b.get(p - 1)? & 0x7F) as usize
                                };
                                if idx > 0 {
                                    list.push(idx - 1);
                                }
                            }
                            if let Some(it) = c.items.iter_mut().find(|i| i.id == id) {
                                it.props = list;
                            }
                        }
                    }
                }
            }
            b"iref" => {
                let ver = *b.first()?;
                for (t, r) in boxes(b.get(4..)?) {
                    let w = if ver == 0 { 2 } else { 4 };
                    let from = be_n(r, 0, w)? as u32;
                    let n = be16(r, w)? as usize;
                    let to: Vec<u32> = (0..n).filter_map(|i| be_n(r, w + 2 + i * w, w).map(|v| v as u32)).collect();
                    c.refs.push((t, from, to));
                }
            }
            _ => {}
        }
    }
    Some(c)
}

impl<'a> Container<'a> {
    fn item(&self, id: u32) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }
    fn data(&self, it: &Item) -> Option<Vec<u8>> {
        let src = if it.method == 1 { self.idat } else { self.file };
        let mut out = Vec::new();
        for &(o, l) in &it.extents {
            let o = o as usize;
            let l = if l == 0 { src.len().checked_sub(o)? } else { l as usize };
            out.extend_from_slice(src.get(o..o.checked_add(l)?)?);
        }
        Some(out)
    }
    fn prop<T>(&self, it: &Item, f: impl Fn(&Prop) -> Option<T>) -> Option<T> {
        it.props.iter().find_map(|&i| self.props.get(i).and_then(&f))
    }
}

/// Decoded image planes plus colour information.
struct Planes {
    frame: av1::Frame,
    colr: Option<(u32, u32, bool)>, // primaries, matrix, full range
}

fn decode_item(c: &Container, it: &Item) -> Option<av1::Frame> {
    av1::decode(&c.data(it)?)
}

/// Decode a grid item into a single frame by placing its tiles.
fn decode_grid(c: &Container, it: &Item) -> Option<av1::Frame> {
    let d = c.data(it)?;
    let flags = *d.get(1)?;
    let rows = *d.get(2)? as usize + 1;
    let cols = *d.get(3)? as usize + 1;
    let (ow, oh) = if flags & 1 != 0 { (be32(&d, 4)? as usize, be32(&d, 8)? as usize) } else { (be16(&d, 4)? as usize, be16(&d, 6)? as usize) };
    let tiles = &c.refs.iter().find(|(t, from, _)| t == b"dimg" && *from == it.id)?.2;
    if tiles.len() < rows * cols || ow == 0 || oh == 0 || ow * oh > 64 << 20 {
        return None;
    }
    let mut out: Option<av1::Frame> = None;
    for (k, &tid) in tiles.iter().enumerate().take(rows * cols) {
        let t = decode_item(c, c.item(tid)?)?;
        let o = out.get_or_insert_with(|| {
            let planes = std::array::from_fn(|p| {
                if p > 0 && t.mono {
                    Vec::new()
                } else {
                    let (sx, sy) = if p == 0 { (0, 0) } else { (t.sub_x, t.sub_y) };
                    vec![0u16; ((ow + sx) >> sx) * ((oh + sy) >> sy)]
                }
            });
            av1::Frame { width: ow, height: oh, planes, ..t.clone_meta() }
        });
        let (tx, ty) = ((k % cols) * t.width, (k / cols) * t.height);
        for p in 0..3 {
            if o.planes[p].is_empty() || t.planes[p].is_empty() {
                continue;
            }
            let (sx, sy) = if p == 0 { (0, 0) } else { (t.sub_x, t.sub_y) };
            let (tw, th) = ((t.width + sx) >> sx, (t.height + sy) >> sy);
            let (ow2, oh2) = ((ow + sx) >> sx, (oh + sy) >> sy);
            let (x0, y0) = (tx >> sx, ty >> sy);
            for y in 0..th {
                if y0 + y >= oh2 {
                    break;
                }
                for x in 0..tw {
                    if x0 + x < ow2 {
                        o.planes[p][(y0 + y) * ow2 + x0 + x] = t.planes[p][y * tw + x];
                    }
                }
            }
        }
    }
    out
}

fn decode_any(c: &Container, it: &Item) -> Option<av1::Frame> {
    match &it.ty {
        b"av01" => decode_item(c, it),
        b"grid" => decode_grid(c, it),
        _ => None,
    }
}

impl av1::Frame {
    fn clone_meta(&self) -> av1::Frame {
        av1::Frame {
            width: self.width,
            height: self.height,
            bit_depth: self.bit_depth,
            sub_x: self.sub_x,
            sub_y: self.sub_y,
            mono: self.mono,
            planes: Default::default(),
            color_primaries: self.color_primaries,
            transfer: self.transfer,
            matrix: self.matrix,
            full_range: self.full_range,
        }
    }
}

/// Decode the primary image's YUV planes (used by the conformance tests).
#[cfg(test)]
pub fn decode_yuv(file: &[u8]) -> Option<av1::Frame> {
    let c = parse(file)?;
    let it = c.item(c.primary)?.clone();
    decode_any(&c, &it)
}

/// Convert YUV planes to 0xAARRGGBB.
fn to_rgb(p: &Planes, alpha: Option<&av1::Frame>) -> Vec<u32> {
    let f = &p.frame;
    let (w, h) = (f.width, f.height);
    let (matrix, full) = match p.colr {
        Some((_, m, full)) => (m, full),
        None => (f.matrix, f.full_range),
    };
    let bd = f.bit_depth;
    let maxv = ((1u32 << bd) - 1) as f32;
    // Kr, Kb for the matrix (unspecified is treated as BT.601, as libavif does).
    let (kr, kb) = match matrix {
        1 => (0.2126, 0.0722),
        9 | 10 => (0.2627, 0.0593),
        4 => (0.30, 0.11),
        7 => (0.212, 0.087),
        _ => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;
    let identity = matrix == 0;
    let (yoff, yrange, crange) = if full {
        (0.0, maxv, maxv)
    } else {
        let s = (1 << (bd - 8)) as f32;
        (16.0 * s, 219.0 * s, 224.0 * s)
    };
    let mid = (1 << (bd - 1)) as f32;
    let (sx, sy) = (f.sub_x, f.sub_y);
    let cw = (w + sx) >> sx;
    let ch = (h + sy) >> sy;
    // Bilinear chroma upsampling (centered samples).
    let chroma = |plane: &[u16], x: usize, y: usize| -> f32 {
        if sx == 0 && sy == 0 {
            return plane[y * cw + x] as f32;
        }
        let fx = if sx == 1 { (x as f32 - 0.5) / 2.0 } else { x as f32 };
        let fy = if sy == 1 { (y as f32 - 0.5) / 2.0 } else { y as f32 };
        let x0 = fx.floor().max(0.0) as usize;
        let y0 = fy.floor().max(0.0) as usize;
        let x1 = (x0 + 1).min(cw - 1);
        let y1 = (y0 + 1).min(ch - 1);
        let ax = (fx - x0 as f32).clamp(0.0, 1.0);
        let ay = (fy - y0 as f32).clamp(0.0, 1.0);
        let g = |x: usize, y: usize| plane[y * cw + x] as f32;
        let top = g(x0, y0) * (1.0 - ax) + g(x1, y0) * ax;
        let bot = g(x0, y1) * (1.0 - ax) + g(x1, y1) * ax;
        top * (1.0 - ay) + bot * ay
    };
    let mut out = vec![0u32; w * h];
    for y in 0..h {
        for x in 0..w {
            let yv = f.planes[0][y * w + x] as f32;
            let (r, g, b) = if f.mono {
                let l = ((yv - yoff) / yrange).clamp(0.0, 1.0);
                (l, l, l)
            } else {
                let u = chroma(&f.planes[1], x, y);
                let v = chroma(&f.planes[2], x, y);
                if identity {
                    // GBR stored as Y=G, U=B, V=R.
                    ((v - yoff) / yrange, (yv - yoff) / yrange, (u - yoff) / yrange)
                } else {
                    let yn = (yv - yoff) / yrange;
                    let cb = (u - mid) / crange;
                    let cr = (v - mid) / crange;
                    let r = yn + (2.0 - 2.0 * kr) * cr;
                    let b = yn + (2.0 - 2.0 * kb) * cb;
                    let g = (yn - kr * r - kb * b) / kg;
                    (r, g, b)
                }
            };
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
            let a = match alpha {
                Some(a) if a.width == w && a.height == h => {
                    let amax = ((1u32 << a.bit_depth) - 1) as f32;
                    let av = a.planes[0][y * w + x] as f32;
                    let av = if a.full_range {
                        av / amax
                    } else {
                        let s = (1 << (a.bit_depth - 8)) as f32;
                        (av - 16.0 * s) / (219.0 * s)
                    };
                    q(av)
                }
                _ => 255,
            };
            out[y * w + x] = a << 24 | q(r) << 16 | q(g) << 8 | q(b);
        }
    }
    out
}

/// Decode an AVIF file to (width, height, 0xAARRGGBB pixels).
pub fn decode(file: &[u8]) -> Option<(u32, u32, Vec<u32>)> {
    let c = parse(file)?;
    let it = c.item(c.primary)?.clone();
    let frame = decode_any(&c, &it)?;
    let colr = c.prop(&it, |p| match p {
        Prop::Colr { primaries, matrix, full, .. } => Some((*primaries, *matrix, *full)),
        _ => None,
    });
    // Alpha: an auxiliary item ("auxl" reference to the primary) with an alpha auxC URN.
    let alpha = c
        .refs
        .iter()
        .filter(|(t, _, to)| t == b"auxl" && to.contains(&it.id))
        .filter_map(|(_, from, _)| c.item(*from))
        .find(|a| c.prop(a, |p| matches!(p, Prop::AuxC(s) if s.contains("alpha")).then_some(())).is_some())
        .and_then(|a| decode_any(&c, a));
    let (mut w, mut h) = (frame.width, frame.height);
    let mut px = to_rgb(&Planes { frame, colr }, alpha.as_ref());
    // Transforms: rotation (anticlockwise quarter turns) then mirroring.
    if let Some(rot) = c.prop(&it, |p| if let Prop::Irot(r) = p { Some(*r) } else { None }) {
        for _ in 0..rot {
            let mut n = vec![0u32; px.len()];
            for y in 0..h {
                for x in 0..w {
                    n[(w - 1 - x) * h + y] = px[y * w + x];
                }
            }
            px = n;
            (w, h) = (h, w);
        }
    }
    if let Some(axis) = c.prop(&it, |p| if let Prop::Imir(a) = p { Some(*a) } else { None }) {
        if axis == 0 {
            px = (0..h).flat_map(|y| (0..w).map(move |x| (y, x))).map(|(y, x)| px[(h - 1 - y) * w + x]).collect();
        } else {
            px = (0..h).flat_map(|y| (0..w).map(move |x| (y, x))).map(|(y, x)| px[y * w + (w - 1 - x)]).collect();
        }
    }
    Some((w as u32, h as u32, px))
}

#[cfg(test)]
mod tests {
    /// Compare decoded planes with dav1d's (via ffmpeg) for every `x.avif`
    /// with an `x.yuv` in `$BOLT_AVIF_DIR`.
    #[test]
    #[ignore]
    fn directory_yuv() {
        check_dir(&std::env::var("BOLT_AVIF_DIR").expect("BOLT_AVIF_DIR"));
    }

    /// Committed fixtures: 4:2:0 / 4:4:4, 10-bit, alpha, screen content
    /// (palette, intra block copy), film grain, loop restoration, 128x128
    /// superblocks and tiles. References decoded by dav1d.
    #[test]
    fn fixtures_match_dav1d() {
        check_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/avif"));
    }

    fn check_dir(dir: &str) {
        let mut failed = 0;
        let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for p in entries {
            if p.extension().is_none_or(|x| x != "avif") {
                continue;
            }
            let file = std::fs::read(&p).unwrap();
            let Ok(want) = std::fs::read(p.with_extension("yuv")) else { continue };
            let t = std::time::Instant::now();
            let Some(f) = super::decode_yuv(&file) else {
                eprintln!("{}: decode failed", p.display());
                failed += 1;
                continue;
            };
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut off = 0;
            let mut bad = false;
            for (pi, plane) in f.planes.iter().enumerate() {
                if plane.is_empty() {
                    continue;
                }
                let (sx, sy) = if pi == 0 { (0, 0) } else { (f.sub_x, f.sub_y) };
                let (w, h) = ((f.width + sx) >> sx, (f.height + sy) >> sy);
                let mut first = None;
                let mut n = 0;
                let bytes = if f.bit_depth > 8 { 2 } else { 1 };
                for i in 0..w * h {
                    let got = plane[i];
                    let k = off + i * bytes;
                    let exp = if bytes == 2 {
                        u16::from_le_bytes([*want.get(k).unwrap_or(&0), *want.get(k + 1).unwrap_or(&0)])
                    } else {
                        *want.get(k).unwrap_or(&0) as u16
                    };
                    if got != exp {
                        n += 1;
                        if first.is_none() {
                            first = Some((i % w, i / w, got, exp));
                        }
                    }
                }
                off += w * h * bytes;
                if n > 0 {
                    bad = true;
                    eprintln!("{} plane {pi} ({w}x{h}): {n} mismatches, first at {:?}", p.display(), first.unwrap());
                }
            }
            if bad {
                failed += 1;
            } else {
                eprintln!("{}: exact ({}x{}, {ms:.1} ms)", p.display(), f.width, f.height);
            }
        }
        assert_eq!(failed, 0);
    }
}

#[cfg(test)]
mod rgb_tests {
    /// RGB output against Pillow (libavif) for `x.avif` + `x.rgba` pairs.
    #[test]
    #[ignore]
    fn directory_rgb() {
        let dir = std::env::var("BOLT_AVIF_DIR").expect("BOLT_AVIF_DIR");
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let Ok(reference) = std::fs::read(p.with_extension("rgba")) else { continue };
            if p.extension().is_none_or(|x| x != "avif") {
                continue;
            }
            let (w, h, px) = super::decode(&std::fs::read(&p).unwrap()).expect("decode");
            let rw = u32::from_le_bytes(reference[0..4].try_into().unwrap());
            let rh = u32::from_le_bytes(reference[4..8].try_into().unwrap());
            assert_eq!((w, h), (rw, rh), "{}", p.display());
            let mut worst = 0;
            let mut total = 0u64;
            for (i, v) in px.iter().enumerate() {
                let got = [(v >> 16) & 255, (v >> 8) & 255, v & 255, v >> 24];
                for c in 0..4 {
                    let d = (got[c] as i32 - reference[8 + i * 4 + c] as i32).abs();
                    worst = worst.max(d);
                    total += d as u64;
                }
            }
            eprintln!("{}: max diff {worst}, mean {:.3}", p.display(), total as f64 / (px.len() * 4) as f64);
        }
    }
}
