//! Font loading, text measurement and the glyph cache.
//!
//! * Faces are loaded lazily (and can be pre-warmed on a background thread).
//! * Advance widths are cached per (face, char) in lock-free atomic tables,
//!   computed at 1px and scaled linearly, so measuring a word is a few loads.
//! * Rasterized glyphs are cached per (face, char, size) and shared read-only
//!   by the paint threads.

use crate::fx::FxHashMap;
use crate::style::{Family, Style};
use fontdue::{Font, FontSettings};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

const FALLBACK_SANS: u8 = 12;
const FALLBACK_SYMBOL: u8 = 13;
const FALLBACK_CJK: u8 = 14;
const NFACES: usize = 15;
/// Slots for downloaded web fonts.
const NWEB: usize = 48;

/// Candidate files per face. Windows names first, then common Linux/macOS ones.
const FILES: [&[&str]; NFACES] = [
    &["arial.ttf", "LiberationSans-Regular.ttf", "DejaVuSans.ttf", "Arial.ttf"],
    &["arialbd.ttf", "LiberationSans-Bold.ttf", "DejaVuSans-Bold.ttf", "Arial Bold.ttf"],
    &["ariali.ttf", "LiberationSans-Italic.ttf", "DejaVuSans-Oblique.ttf", "Arial Italic.ttf"],
    &["arialbi.ttf", "LiberationSans-BoldItalic.ttf", "DejaVuSans-BoldOblique.ttf"],
    &["times.ttf", "LiberationSerif-Regular.ttf", "DejaVuSerif.ttf", "Times New Roman.ttf"],
    &["timesbd.ttf", "LiberationSerif-Bold.ttf", "DejaVuSerif-Bold.ttf"],
    &["timesi.ttf", "LiberationSerif-Italic.ttf", "DejaVuSerif-Italic.ttf"],
    &["timesbi.ttf", "LiberationSerif-BoldItalic.ttf", "DejaVuSerif-BoldItalic.ttf"],
    &["consola.ttf", "LiberationMono-Regular.ttf", "DejaVuSansMono.ttf", "Courier New.ttf"],
    &["consolab.ttf", "LiberationMono-Bold.ttf", "DejaVuSansMono-Bold.ttf"],
    &["consolai.ttf", "LiberationMono-Italic.ttf", "DejaVuSansMono-Oblique.ttf"],
    &["consolaz.ttf", "LiberationMono-BoldItalic.ttf", "DejaVuSansMono-BoldOblique.ttf"],
    &["segoeui.ttf", "DejaVuSans.ttf"],
    &["seguisym.ttf", "DejaVuSans.ttf"],
    &["msyh.ttc", "NotoSansCJK-Regular.ttc"],
];

const DIRS: &[&str] = &[
    "C:\\Windows\\Fonts\\",
    "/usr/share/fonts/truetype/liberation/",
    "/usr/share/fonts/truetype/dejavu/",
    "/usr/share/fonts/opentype/noto/",
    "/System/Library/Fonts/Supplemental/",
    "/Library/Fonts/",
];

pub struct FontData {
    pub font: Font,
    /// Line metrics for a 1px font.
    pub ascent: f32,
    pub descent: f32,
    pub line: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Adv {
    pub adv: f32,
    pub face: u8,
}

/// A registered web font face.
struct WebFace {
    family: Box<str>,
    url: String,
    weight: (u16, u16),
    italic: bool,
    /// Face slot, or None if it failed to load.
    face: Option<u8>,
}

pub struct Fonts {
    faces: Vec<OnceLock<Option<FontData>>>,
    web: std::sync::RwLock<Vec<WebFace>>,
    /// Lazily allocated 256-entry blocks of packed (face+1, advance) per face.
    tables: Vec<OnceLock<Box<[AtomicU64]>>>,
}

static FONTS: OnceLock<Fonts> = OnceLock::new();

pub fn fonts() -> &'static Fonts {
    FONTS.get_or_init(|| Fonts {
        faces: (0..NFACES + NWEB).map(|_| OnceLock::new()).collect(),
        web: std::sync::RwLock::new(Vec::new()),
        tables: (0..(NFACES + NWEB) * 256).map(|_| OnceLock::new()).collect(),
    })
}

fn font_data(bytes: Vec<u8>) -> Option<FontData> {
    let settings = FontSettings { scale: 40.0, ..FontSettings::default() };
    let font = Font::from_bytes(bytes, settings).ok()?;
    let lm = font.horizontal_line_metrics(1000.0)?;
    Some(FontData { ascent: lm.ascent / 1000.0, descent: -lm.descent / 1000.0, line: lm.new_line_size / 1000.0, font })
}

/// TTF/OTF bytes as-is; WOFF 1.0 unpacked back into an sfnt. (WOFF2 needs
/// Brotli and isn't supported.)
fn sfnt_bytes(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 44 {
        return None;
    }
    match &b[..4] {
        b"\x00\x01\x00\x00" | b"OTTO" | b"true" => return Some(b.to_vec()),
        b"wOFF" => {}
        _ => return None,
    }
    let be16 = |i: usize| -> Option<usize> { Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize) };
    let be32 = |i: usize| -> Option<usize> { Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize) };
    let n = be16(12)?;
    let mut tables = Vec::with_capacity(n);
    for t in 0..n {
        let e = 44 + t * 20;
        let (tag, off, clen, olen, sum) = (b.get(e..e + 4)?, be32(e + 4)?, be32(e + 8)?, be32(e + 12)?, be32(e + 16)?);
        let raw = b.get(off..off + clen)?;
        let data = if clen < olen { crate::image::zlib(raw)? } else { raw.to_vec() };
        tables.push((tag, sum as u32, data));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&b[4..8]); // flavor
    let mut pow = 1;
    let mut log = 0u16;
    while pow * 2 <= n {
        pow *= 2;
        log += 1;
    }
    for v in [n as u16, (pow * 16) as u16, log, (n * 16 - pow * 16) as u16] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut offset = 12 + 16 * n;
    for (tag, sum, data) in &tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&sum.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().div_ceil(4) * 4;
    }
    for (_, _, data) in &tables {
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    Some(out)
}

fn load(face: usize) -> Option<FontData> {
    let windir = std::env::var("WINDIR").map(|w| format!("{w}\\Fonts\\")).ok();
    let dirs = windir.iter().map(|s| s.as_str()).chain(DIRS.iter().copied());
    for dir in dirs {
        for f in FILES[face] {
            let path = format!("{dir}{f}");
            if let Ok(bytes) = std::fs::read(&path) {
                let settings = FontSettings { scale: 40.0, ..FontSettings::default() };
                if let Ok(font) = Font::from_bytes(bytes, settings) {
                    let lm = font.horizontal_line_metrics(1000.0)?;
                    return Some(FontData {
                        ascent: lm.ascent / 1000.0,
                        descent: -lm.descent / 1000.0,
                        line: lm.new_line_size / 1000.0,
                        font,
                    });
                }
            }
        }
    }
    None
}

impl Fonts {
    #[inline]
    pub fn face(&self, f: u8) -> Option<&FontData> {
        let slot = self.faces.get(f as usize)?;
        if f as usize >= NFACES {
            return slot.get()?.as_ref();
        }
        slot.get_or_init(|| load(f as usize)).as_ref()
    }

    /// Is a web font from `url` already registered (loaded or failed)?
    pub fn has_web(&self, url: &str, family: &str) -> bool {
        self.web.read().unwrap().iter().any(|w| w.url == url && *w.family == *family)
    }

    /// Register a downloaded web font (TTF/OTF/WOFF bytes, or None on failure).
    pub fn add_web(&self, family: &str, url: &str, weight: (u16, u16), italic: bool, bytes: Option<Vec<u8>>) {
        let mut web = self.web.write().unwrap();
        if web.iter().any(|w| w.url == url && *w.family == *family) {
            return;
        }
        let used = web.iter().filter(|w| w.face.is_some()).count();
        let data = if used < NWEB { bytes.and_then(|b| sfnt_bytes(&b)).and_then(|b| font_data(b)) } else { None };
        let face = data.map(|d| {
            // Reuse the slot of an identical URL loaded for another family.
            let slot = (NFACES + used) as u8;
            let _ = self.faces[slot as usize].set(Some(d));
            slot
        });
        web.push(WebFace { family: family.into(), url: url.to_owned(), weight, italic, face });
    }

    /// Best web font face for a family list, if any family is a loaded web font.
    fn web_pick(&self, names: &[Box<str>], bold: bool, italic: bool) -> Option<u8> {
        let web = self.web.read().ok()?;
        if web.is_empty() {
            return None;
        }
        let want = if bold { 700 } else { 400 };
        for n in names {
            let best = web
                .iter()
                .filter(|w| w.face.is_some() && *w.family == **n)
                .min_by_key(|w| {
                    let d = if want < w.weight.0 { w.weight.0 - want } else { want.saturating_sub(w.weight.1) };
                    (w.italic != italic) as u32 * 1000 + d as u32
                });
            if let Some(w) = best {
                return w.face;
            }
        }
        None
    }

    /// Face for a computed style (web fonts first, then system families).
    pub fn pick_style(&self, st: &Style) -> u8 {
        if let Some(names) = &st.families {
            if let Some(f) = self.web_pick(names, st.bold, st.italic) {
                return f;
            }
        }
        self.pick(st.family, st.bold, st.italic)
    }

    /// Start loading the common faces in the background.
    pub fn prewarm(&'static self) {
        for f in [0u8, 1, 2, 8, FALLBACK_SANS, FALLBACK_SYMBOL] {
            std::thread::spawn(move || {
                self.face(f);
            });
        }
    }

    /// Pick an available face for a style.
    pub fn pick(&self, family: Family, bold: bool, italic: bool) -> u8 {
        let base = match family {
            Family::Sans => 0,
            Family::Serif => 4,
            Family::Mono => 8,
        };
        let want = base + bold as u8 + 2 * italic as u8;
        for f in [want, base + bold as u8, base, 0, FALLBACK_SANS] {
            if self.face(f).is_some() {
                return f;
            }
        }
        0
    }

    fn compute(&self, face: u8, c: char) -> Adv {
        let cp = c as u32;
        let mut chain = [face, FALLBACK_SANS, FALLBACK_SYMBOL, u8::MAX];
        if (0x2E80..=0x9FFF).contains(&cp) || (0xAC00..=0xD7AF).contains(&cp) || (0xFF00..=0xFFEF).contains(&cp) {
            chain[3] = FALLBACK_CJK;
        }
        let mut chosen = face;
        if cp >= 0x80 {
            for &f in &chain {
                if f == u8::MAX {
                    break;
                }
                if let Some(fd) = self.face(f) {
                    if fd.font.lookup_glyph_index(c) != 0 {
                        chosen = f;
                        break;
                    }
                }
            }
        }
        let adv = match self.face(chosen) {
            Some(fd) => fd.font.metrics(c, 1000.0).advance_width / 1000.0,
            None => 0.5,
        };
        Adv { adv, face: chosen }
    }

    /// Advance width at 1px and the face that actually has the glyph.
    #[inline]
    pub fn adv(&self, face: u8, c: char) -> Adv {
        let cp = c as u32;
        if cp >= 0x10000 {
            return self.compute(face, c);
        }
        let t = self.tables[face as usize * 256 + (cp >> 8) as usize]
            .get_or_init(|| (0..256).map(|_| AtomicU64::new(0)).collect());
        let e = &t[(cp & 255) as usize];
        let v = e.load(Relaxed);
        if v != 0 {
            return Adv { adv: f32::from_bits(v as u32), face: ((v >> 32) - 1) as u8 };
        }
        let a = self.compute(face, c);
        e.store(((a.face as u64 + 1) << 32) | a.adv.to_bits() as u64, Relaxed);
        a
    }

    /// Width of `s` in px.
    #[inline]
    pub fn measure(&self, face: u8, size: f32, s: &str) -> f32 {
        let mut w = 0.0;
        for c in s.chars() {
            w += self.adv(face, c).adv;
        }
        w * size
    }

    /// (ascent, descent, normal line height) for a 1px font.
    pub fn metrics(&self, face: u8) -> (f32, f32, f32) {
        match self.face(face) {
            Some(fd) => (fd.ascent, fd.descent, fd.line),
            None => (0.9, 0.2, 1.15),
        }
    }
}

// ------------------------------------------------------------ glyph cache

pub struct Glyph {
    pub w: u16,
    pub h: u16,
    pub xmin: i16,
    pub ymin: i16,
    pub data: Box<[u8]>,
}

#[derive(Default)]
pub struct GlyphCache {
    map: FxHashMap<u64, Glyph>,
}

/// Coverage boost: fontdue produces linear coverage which looks thin and
/// washed out; this approximates the contrast of platform text renderers.
fn gamma_lut() -> &'static [u8; 256] {
    static LUT: OnceLock<[u8; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u8; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = ((i as f32 / 255.0).powf(1.0 / 1.45) * 255.0).round() as u8;
        }
        t
    })
}

#[inline]
pub fn glyph_key(face: u8, c: char, size: f32) -> u64 {
    let sq = (size * 4.0).round() as u64 & 0xFF_FFFF;
    (face as u64) << 56 | sq << 32 | c as u64
}

impl GlyphCache {
    pub fn ensure(&mut self, fonts: &Fonts, face: u8, c: char, size: f32) {
        let key = glyph_key(face, c, size);
        if self.map.contains_key(&key) {
            return;
        }
        let g = match fonts.face(face) {
            Some(fd) if !c.is_whitespace() && !c.is_control() => {
                let px = (size * 4.0).round() / 4.0;
                let (m, data) = fd.font.rasterize(c, px);
                let lut = gamma_lut();
                Glyph {
                    w: m.width as u16,
                    h: m.height as u16,
                    xmin: m.xmin as i16,
                    ymin: m.ymin as i16,
                    data: data.into_iter().map(|v| lut[v as usize]).collect(),
                }
            }
            _ => Glyph { w: 0, h: 0, xmin: 0, ymin: 0, data: Box::new([]) },
        };
        self.map.insert(key, g);
    }

    #[inline]
    pub fn get(&self, key: u64) -> Option<&Glyph> {
        self.map.get(&key)
    }
}
