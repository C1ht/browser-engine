//! WOFF2 font unpacking (W3C WOFF 2.0): Brotli-compressed tables, with the
//! transformed `glyf` / `loca` / `hmtx` tables rebuilt into plain TrueType.

const KNOWN: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm", b"glyf", b"loca", b"prep",
    b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern", b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE",
    b"GDEF", b"GPOS", b"GSUB", b"EBSC", b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt",
    b"avar", b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty", b"just", b"lcar",
    b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat", b"Gloc", b"Feat", b"Sill",
];

struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(d: &'a [u8]) -> Self {
        R { d, p: 0 }
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.d.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.bytes(2)?.try_into().ok()?))
    }
    fn i16(&mut self) -> Option<i16> {
        Some(self.u16()? as i16)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn base128(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 0 && b == 0x80 {
                return None;
            }
            v = v.checked_mul(128)? | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    fn u255(&mut self) -> Option<u16> {
        Some(match self.u8()? {
            253 => self.u16()?,
            255 => self.u8()? as u16 + 253,
            254 => self.u8()? as u16 + 506,
            c => c as u16,
        })
    }
    /// A sub-stream of `n` bytes.
    fn sub(&mut self, n: u32) -> Option<R<'a>> {
        Some(R::new(self.bytes(n as usize)?))
    }
}

/// Decode WOFF2 into (flavor, tables). Font collections are not supported.
pub fn decode(b: &[u8]) -> Option<(u32, Vec<([u8; 4], Vec<u8>)>)> {
    let mut r = R::new(b);
    if r.bytes(4)? != b"wOF2" {
        return None;
    }
    let flavor = r.u32()?;
    if flavor == u32::from_be_bytes(*b"ttcf") {
        return None;
    }
    r.u32()?; // length
    let n = r.u16()? as usize;
    r.u16()?;
    r.u32()?; // totalSfntSize
    let compressed = r.u32()? as usize;
    r.bytes(24)?; // version, metadata and private blocks
    struct Entry {
        tag: [u8; 4],
        orig: u32,
        len: u32,
        transformed: bool,
    }
    let mut dir = Vec::with_capacity(n);
    for _ in 0..n {
        let flags = r.u8()?;
        let tag: [u8; 4] = if flags & 63 == 63 { r.bytes(4)?.try_into().ok()? } else { *KNOWN[(flags & 63) as usize] };
        let version = flags >> 6;
        let orig = r.base128()?;
        let transformed = if &tag == b"glyf" || &tag == b"loca" { version == 0 } else { version != 0 };
        let len = if transformed { r.base128()? } else { orig };
        dir.push(Entry { tag, orig, len, transformed });
    }
    let stream = crate::brotli::decompress(b.get(r.p..r.p.checked_add(compressed)?)?)?;
    let mut s = R::new(&stream);
    let mut raw: Vec<(&[u8; 4], &[u8])> = Vec::with_capacity(n);
    for e in &dir {
        raw.push((&e.tag, s.bytes(e.len as usize)?));
    }
    let find = |t: &[u8; 4]| raw.iter().find(|(tag, _)| *tag == t).map(|(_, d)| *d);

    // Rebuild transformed tables.
    let mut glyf_loca: Option<(Vec<u8>, Vec<u8>, Vec<i16>)> = None;
    if dir.iter().any(|e| &e.tag == b"glyf" && e.transformed) {
        glyf_loca = Some(rebuild_glyf(find(b"glyf")?)?);
    }
    let mut out = Vec::with_capacity(n);
    for e in &dir {
        let data = find(&e.tag)?;
        let table = match (&e.tag, e.transformed) {
            (b"glyf", true) => glyf_loca.as_ref()?.0.clone(),
            (b"loca", true) => {
                let loca = glyf_loca.as_ref()?.1.clone();
                if loca.len() != e.orig as usize {
                    return None;
                }
                loca
            }
            (b"hmtx", true) => {
                let x_mins = &glyf_loca.as_ref()?.2;
                let hhea = find(b"hhea")?;
                let nh = u16::from_be_bytes(hhea.get(34..36)?.try_into().ok()?) as usize;
                rebuild_hmtx(data, nh, x_mins)?
            }
            (_, true) => return None,
            _ => data.to_vec(),
        };
        out.push((e.tag, table));
    }
    Some((flavor, out))
}

/// Inverse of the WOFF2 glyf transform. Returns (glyf, loca, per-glyph xMin).
fn rebuild_glyf(d: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<i16>)> {
    let mut h = R::new(d);
    h.u16()?;
    let options = h.u16()?;
    let ng = h.u16()? as usize;
    let long = h.u16()? != 0;
    let sizes: Vec<u32> = (0..7).map(|_| h.u32()).collect::<Option<_>>()?;
    let mut contours = h.sub(sizes[0])?;
    let mut npoints = h.sub(sizes[1])?;
    let mut flags = h.sub(sizes[2])?;
    let mut glyphs = h.sub(sizes[3])?;
    let mut comps = h.sub(sizes[4])?;
    let mut bbox = h.sub(sizes[5])?;
    let mut instrs = h.sub(sizes[6])?;
    let _overlap = if options & 1 != 0 { Some(h.bytes(ng.div_ceil(8))?) } else { None };
    let bbox_bits = bbox.bytes(4 * ng.div_ceil(32))?;

    let mut glyf = Vec::with_capacity(d.len() * 2);
    let mut offsets = Vec::with_capacity(ng + 1);
    let mut x_mins = vec![0i16; ng];
    let mut pts: Vec<(i32, i32, bool)> = Vec::new();
    let mut ends: Vec<u16> = Vec::new();
    for g in 0..ng {
        offsets.push(glyf.len());
        let has_bbox = bbox_bits[g >> 3] & (0x80 >> (g & 7)) != 0;
        let nc = contours.i16()?;
        if nc == 0 {
            if has_bbox {
                return None;
            }
            continue;
        }
        let explicit = if has_bbox { Some([bbox.i16()?, bbox.i16()?, bbox.i16()?, bbox.i16()?]) } else { None };
        if nc < 0 {
            // Composite: copy component records through.
            let start = comps.p;
            let mut instructions = false;
            loop {
                let f = comps.u16()?;
                comps.u16()?;
                let mut skip = if f & 1 != 0 { 4 } else { 2 };
                skip += if f & 8 != 0 {
                    2
                } else if f & 0x40 != 0 {
                    4
                } else if f & 0x80 != 0 {
                    8
                } else {
                    0
                };
                comps.bytes(skip)?;
                instructions |= f & 0x100 != 0;
                if f & 0x20 == 0 {
                    break;
                }
            }
            let bb = explicit?;
            x_mins[g] = bb[0];
            glyf.extend_from_slice(&(-1i16).to_be_bytes());
            bb.iter().for_each(|v| glyf.extend_from_slice(&v.to_be_bytes()));
            glyf.extend_from_slice(&comps.d[start..comps.p]);
            if instructions {
                let n = glyphs.u255()?;
                glyf.extend_from_slice(&n.to_be_bytes());
                glyf.extend_from_slice(instrs.bytes(n as usize)?);
            }
        } else {
            ends.clear();
            let mut total = 0u32;
            for _ in 0..nc {
                total += npoints.u255()? as u32;
                if total > 0xFFFF {
                    return None;
                }
                ends.push((total - 1) as u16);
            }
            pts.clear();
            let (mut x, mut y) = (0i32, 0i32);
            for _ in 0..total {
                let f = flags.u8()?;
                let on = f & 0x80 == 0;
                let f = f & 0x7F;
                let n = match f {
                    0..=83 => 1,
                    84..=119 => 2,
                    120..=123 => 3,
                    _ => 4,
                };
                let b = glyphs.bytes(n)?;
                let b = |i: usize| b[i] as i32;
                let sign = |flag: u8, v: i32| if flag & 1 != 0 { v } else { -v };
                let (dx, dy) = match f {
                    0..=9 => (0, sign(f, (((f & 14) as i32) << 7) + b(0))),
                    10..=19 => (sign(f, ((((f - 10) & 14) as i32) << 7) + b(0)), 0),
                    20..=83 => {
                        let b0 = (f - 20) as i32;
                        (sign(f, 1 + (b0 & 0x30) + (b(0) >> 4)), sign(f >> 1, 1 + ((b0 & 0x0C) << 2) + (b(0) & 0x0F)))
                    }
                    84..=119 => {
                        let b0 = (f - 84) as i32;
                        (sign(f, 1 + ((b0 / 12) << 8) + b(0)), sign(f >> 1, 1 + (((b0 % 12) >> 2) << 8) + b(1)))
                    }
                    120..=123 => (sign(f, (b(0) << 4) + (b(1) >> 4)), sign(f >> 1, ((b(1) & 0x0F) << 8) + b(2))),
                    _ => (sign(f, (b(0) << 8) + b(1)), sign(f >> 1, (b(2) << 8) + b(3))),
                };
                x += dx;
                y += dy;
                pts.push((x, y, on));
            }
            let ilen = glyphs.u255()?;
            let bb = explicit.unwrap_or_else(|| {
                let (mut a, mut b, mut c, mut d) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
                for &(x, y, _) in &pts {
                    a = a.min(x);
                    b = b.min(y);
                    c = c.max(x);
                    d = d.max(y);
                }
                if pts.is_empty() { [0; 4] } else { [a as i16, b as i16, c as i16, d as i16] }
            });
            x_mins[g] = bb[0];
            glyf.extend_from_slice(&nc.to_be_bytes());
            bb.iter().for_each(|v| glyf.extend_from_slice(&v.to_be_bytes()));
            ends.iter().for_each(|e| glyf.extend_from_slice(&e.to_be_bytes()));
            glyf.extend_from_slice(&ilen.to_be_bytes());
            glyf.extend_from_slice(instrs.bytes(ilen as usize)?);
            // Flags, then x and y coordinates as short or word deltas.
            let (mut xs, mut ys) = (Vec::new(), Vec::new());
            let (mut px, mut py) = (0i32, 0i32);
            for &(x, y, on) in &pts {
                let (dx, dy) = (x - px, y - py);
                (px, py) = (x, y);
                let mut fl = on as u8;
                if dx == 0 {
                    fl |= 0x10;
                } else if dx.abs() < 256 {
                    fl |= 0x02 | if dx > 0 { 0x10 } else { 0 };
                    xs.push(dx.unsigned_abs() as u8);
                } else {
                    xs.extend_from_slice(&(dx as i16).to_be_bytes());
                }
                if dy == 0 {
                    fl |= 0x20;
                } else if dy.abs() < 256 {
                    fl |= 0x04 | if dy > 0 { 0x20 } else { 0 };
                    ys.push(dy.unsigned_abs() as u8);
                } else {
                    ys.extend_from_slice(&(dy as i16).to_be_bytes());
                }
                glyf.push(fl);
            }
            glyf.extend_from_slice(&xs);
            glyf.extend_from_slice(&ys);
        }
        glyf.resize(glyf.len().div_ceil(4) * 4, 0);
    }
    offsets.push(glyf.len());
    let mut loca = Vec::with_capacity((ng + 1) * 4);
    for o in offsets {
        if long {
            loca.extend_from_slice(&(o as u32).to_be_bytes());
        } else {
            loca.extend_from_slice(&((o / 2) as u16).to_be_bytes());
        }
    }
    Some((glyf, loca, x_mins))
}

/// Inverse of the WOFF2 hmtx transform: left side bearings default to xMin.
fn rebuild_hmtx(d: &[u8], nh: usize, x_mins: &[i16]) -> Option<Vec<u8>> {
    let ng = x_mins.len();
    if nh == 0 || nh > ng {
        return None;
    }
    let mut r = R::new(d);
    let flags = r.u8()?;
    let adv: Vec<u16> = (0..nh).map(|_| r.u16()).collect::<Option<_>>()?;
    let lsb: Vec<i16> = if flags & 1 == 0 { (0..nh).map(|_| r.i16()).collect::<Option<_>>()? } else { x_mins[..nh].to_vec() };
    let mono: Vec<i16> = if flags & 2 == 0 { (nh..ng).map(|_| r.i16()).collect::<Option<_>>()? } else { x_mins[nh..].to_vec() };
    let mut out = Vec::with_capacity(nh * 4 + (ng - nh) * 2);
    for i in 0..nh {
        out.extend_from_slice(&adv[i].to_be_bytes());
        out.extend_from_slice(&lsb[i].to_be_bytes());
    }
    mono.iter().for_each(|v| out.extend_from_slice(&v.to_be_bytes()));
    Some(out)
}
