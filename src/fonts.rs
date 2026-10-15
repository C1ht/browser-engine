//! Font loading, text measurement and the glyph cache.
//!
//! * Faces are loaded lazily (and can be pre-warmed on a background thread).
//! * Advance widths are cached per (face, char) in lock-free atomic tables,
//!   computed at 1px and scaled linearly, so measuring a word is a few loads.
//! * Rasterized glyphs are cached per (face, char, size) and shared read-only
//!   by the paint threads.

use crate::fx::FxHashMap;
use crate::style::Family;
use fontdue::{Font, FontSettings};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

const FALLBACK_SANS: u8 = 12;
const FALLBACK_SYMBOL: u8 = 13;
const FALLBACK_CJK: u8 = 14;
const NFACES: usize = 15;

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

pub struct Fonts {
    faces: Vec<OnceLock<Option<FontData>>>,
    /// Lazily allocated 256-entry blocks of packed (face+1, advance) per face.
    tables: Vec<OnceLock<Box<[AtomicU64]>>>,
}

static FONTS: OnceLock<Fonts> = OnceLock::new();

pub fn fonts() -> &'static Fonts {
    FONTS.get_or_init(|| Fonts {
        faces: (0..NFACES).map(|_| OnceLock::new()).collect(),
        tables: (0..NFACES * 256).map(|_| OnceLock::new()).collect(),
    })
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
        self.faces[f as usize].get_or_init(|| load(f as usize)).as_ref()
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
