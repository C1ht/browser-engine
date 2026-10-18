//! CSS parser. Produces fully-typed declarations (shorthands are expanded and
//! values parsed up front) so the cascade never touches strings again.

use crate::atom::{intern, intern_lower, Atom};
use crate::fx::FxHashMap;
use std::cell::RefCell;
use std::rc::Rc;

pub const CURRENT_COLOR: u32 = 0x00C0_FFEE; // sentinel, resolved during cascade

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    Px(f32),
    Em(f32),
    Rem(f32),
    Pct(f32),
    Vw(f32),
    Vh(f32),
    Num(f32),
    Color(u32),
    Kw(Kw),
    Auto,
    Inherit,
    Initial,
    /// calc(): coefficients of [%, px, em, rem, vw, vh].
    Calc([f32; 6]),
    /// Index into the sheet's raw (name, value) strings: custom properties
    /// and declarations that contain var() and are resolved during cascade.
    Raw(u32),
    /// min() / max() / clamp(): index into the expression table.
    Expr(u32),
    Shadow(Shadow),
    /// Grid track list: index into the track table.
    Tracks(u32),
    /// `span N` in grid placement.
    Span(i16),
    /// background-image: index into the image table.
    Image(u32),
    /// `content`: index into the content table.
    Content(u32),
    /// transform / translate / rotate / scale: index into the transform table.
    Transform(u32),
    /// animation-name list: index into the name-list table.
    Names(u32),
    /// grid-template-areas: index into the areas table.
    Areas(u32),
    /// filter / backdrop-filter: index into the filter table.
    Filter(u32),
    /// An identifier (interned), e.g. a grid area name.
    Name(u32),
    /// font-family list: index into the family table.
    Family(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kw {
    None,
    Block,
    Inline,
    InlineBlock,
    ListItem,
    Table,
    InlineTable,
    TableRow,
    TableCell,
    TableRowGroup,
    TableCaption,
    Contents,
    Normal,
    Bold,
    Italic,
    Left,
    Right,
    Center,
    WebkitCenter,
    Pre,
    PreWrap,
    PreLine,
    Nowrap,
    Disc,
    Circle,
    Square,
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    Hidden,
    Visible,
    Uppercase,
    Lowercase,
    Capitalize,
    Top,
    Middle,
    Bottom,
    Baseline,
    BorderBox,
    ContentBox,
    Serif,
    SansSerif,
    Monospace,
    Out, // position: absolute | fixed
    InFlow,
    Collapse,
    Separate,
    Solid,
    // text-decoration bit sets
    Underline,
    LineThrough,
    UnderlineLineThrough,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    FlowRoot,
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
    Both,
    Scroll,
    Row,
    RowReverse,
    Column,
    ColumnReverse,
    Wrap,
    Start,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    Stretch,
    Cover,
    Contain,
    Repeat,
    NoRepeat,
    RepeatX,
    RepeatY,
    Fill,
    ScaleDown,
    // intrinsic sizes (width / min-width / max-width)
    MinContent,
    MaxContent,
    FitContent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Prop {
    Display,
    Position,
    Color,
    Background,
    FontSize,
    FontWeight,
    FontStyle,
    FontFamily,
    TextAlign,
    LineHeight,
    WhiteSpace,
    TextDecoration,
    TextTransform,
    ListStyleType,
    Visibility,
    PointerEvents,
    VerticalAlign,
    BoxSizing,
    BorderCollapse,
    BorderSpacing,
    Width,
    Height,
    MinWidth,
    MaxWidth,
    MinHeight,
    MaxHeight,
    MarginTop,
    MarginRight,
    MarginBottom,
    MarginLeft,
    PaddingTop,
    PaddingRight,
    PaddingBottom,
    PaddingLeft,
    BorderTopWidth,
    BorderRightWidth,
    BorderBottomWidth,
    BorderLeftWidth,
    BorderTopColor,
    BorderRightColor,
    BorderBottomColor,
    BorderLeftColor,
    BorderTopStyle,
    BorderRightStyle,
    BorderBottomStyle,
    BorderLeftStyle,
    InsetTop,
    InsetRight,
    InsetBottom,
    InsetLeft,
    ZIndex,
    Float,
    Clear,
    Overflow,
    RadiusTL,
    RadiusTR,
    RadiusBR,
    RadiusBL,
    BoxShadow,
    Opacity,
    FlexDirection,
    FlexWrap,
    JustifyContent,
    AlignItems,
    AlignSelf,
    AlignContent,
    JustifyItems,
    JustifySelf,
    FlexGrow,
    FlexShrink,
    FlexBasis,
    Order,
    RowGap,
    ColumnGap,
    GridTemplateColumns,
    GridTemplateRows,
    GridAutoRows,
    GridColumnStart,
    GridColumnEnd,
    GridRowStart,
    GridRowEnd,
    GridTemplateAreas,
    GridArea,
    BackgroundImage,
    Content,
    TransformP,
    TranslateP,
    RotateP,
    ScaleP,
    TransformOriginX,
    TransformOriginY,
    AnimName,
    AnimFill,
    AnimIter,
    OverflowX,
    OverflowY,
    WordBreak,
    OverflowWrap,
    TextOverflow,
    LineClamp,
    BgSizeX,
    BgSizeY,
    BgPosX,
    BgPosY,
    BgRepeat,
    ObjectFit,
    FilterP,
    BackdropFilterP,
    Custom,
    Unresolved,
}

use Prop::*;
pub const MARGIN: [Prop; 4] = [MarginTop, MarginRight, MarginBottom, MarginLeft];
pub const PADDING: [Prop; 4] = [PaddingTop, PaddingRight, PaddingBottom, PaddingLeft];
pub const BWIDTH: [Prop; 4] = [BorderTopWidth, BorderRightWidth, BorderBottomWidth, BorderLeftWidth];
pub const BCOLOR: [Prop; 4] = [BorderTopColor, BorderRightColor, BorderBottomColor, BorderLeftColor];
pub const BSTYLE: [Prop; 4] = [BorderTopStyle, BorderRightStyle, BorderBottomStyle, BorderLeftStyle];
pub const INSET: [Prop; 4] = [InsetTop, InsetRight, InsetBottom, InsetLeft];
pub const RADIUS: [Prop; 4] = [RadiusTL, RadiusTR, RadiusBR, RadiusBL];

#[derive(Clone, Copy, Debug)]
pub struct Decl {
    pub prop: Prop,
    pub value: Value,
    pub important: bool,
}

// ---------------------------------------------------------------- selectors

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Comb {
    Descendant,
    Child,
    Next,
    Subsequent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrOp {
    Exists,
    Eq,
    Word,
    Prefix,
    Suffix,
    Substr,
    Dash,
}

#[derive(Clone, Debug)]
pub struct AttrSel {
    pub name: Atom,
    pub op: AttrOp,
    pub value: String,
    pub ci: bool,
}

#[derive(Clone, Debug)]
pub enum Pseudo {
    FirstChild,
    LastChild,
    OnlyChild,
    FirstOfType,
    LastOfType,
    NthChild(i32, i32),
    NthLastChild(i32, i32),
    Link,
    Root,
    Empty,
    Focus,
    FocusWithin,
    Checked,
    Disabled,
    PlaceholderShown,
    Not(Vec<Selector>),
    /// :is() (false) / :where() (true: zero specificity).
    Is(Vec<Selector>, bool),
    /// :has(<relative selector list>).
    Has(Vec<(Option<Comb>, Selector)>),
    NthOfType(i32, i32),
    NthLastOfType(i32, i32),
    OnlyOfType,
    Hover,
    Active,
    Target,
    Enabled,
    Required,
    Optional,
    ReadOnly,
    ReadWrite,
    /// Always matches (`:defined`, `:valid`, `:dir(ltr)` ...).
    Always,
    /// `::before` / `::after`: the rule styles that pseudo-element.
    Before,
    After,
    /// `:host` / `:host(<compound>)` (an empty list matches any host).
    Host(Vec<Compound>),
    /// `::slotted(<compound>)`.
    Slotted(Vec<Compound>),
    Never,
}

#[derive(Clone, Debug, Default)]
pub struct Compound {
    pub tag: Option<Atom>,
    pub id: Option<Atom>,
    pub classes: Vec<Atom>,
    pub attrs: Vec<AttrSel>,
    pub pseudos: Vec<Pseudo>,
}

#[derive(Clone, Debug)]
pub struct Selector {
    /// Rightmost (subject) compound first.
    pub comps: Vec<Compound>,
    /// combs[i] joins comps[i] to comps[i+1] (its left neighbour).
    pub combs: Vec<Comb>,
    pub spec: u32,
    /// Atoms that must appear on some ancestor; checked against a bloom filter.
    pub ancestor_atoms: Vec<Atom>,
}

pub struct StyleRule {
    pub sel: Selector,
    pub start: u32,
    pub end: u32,
    /// Cascade layer rank (UNLAYERED outside @layer).
    pub layer: u16,
}

#[derive(Default)]
pub struct Stylesheet {
    pub rules: Vec<StyleRule>,
    pub decls: Vec<Decl>,
    pub raws: Vec<Raw>,
    /// @keyframes: (name, decls range of the final keyframe).
    pub keyframes: Vec<(Rc<str>, u32, u32)>,
}

/// (property name, unparsed value)
pub type Raw = (Box<str>, Box<str>);

#[derive(Clone, Copy)]
pub struct Media {
    pub vw: f32,
    pub vh: f32,
}

// ------------------------------------------------------------------ helpers

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\t' | b'\r' | b'\x0c')
}

/// Index of the first byte in `stops` at nesting depth 0, skipping strings.
pub fn find_top(b: &[u8], mut i: usize, stops: &[u8]) -> Option<usize> {
    let mut depth = 0i32;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            _ if depth <= 0 && stops.contains(&c) => return Some(i),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Given the index of a '{', return the index of its matching '}'.
fn match_brace(b: &[u8], open: usize) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' => {
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

fn strip_comments(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("/*") {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("/*") {
        out.push_str(&rest[..p]);
        out.push(' ');
        match rest[p + 2..].find("*/") {
            Some(e) => rest = &rest[p + 2 + e + 2..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Split on whitespace, keeping parenthesised groups together.
fn tokens(v: &str) -> Vec<&str> {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (is_ws(b[i]) || b[i] == b',') {
            i += 1;
        }
        let s = i;
        let mut depth = 0;
        while i < b.len() && (depth > 0 || !(is_ws(b[i]) || b[i] == b',')) {
            match b[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if i > s {
            out.push(&v[s..i]);
        }
    }
    out
}

// ------------------------------------------------------------------- values

pub fn parse_len(t: &str) -> Option<Value> {
    let t = t.trim();
    let b = t.as_bytes();
    if b.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("auto") {
        return Some(Value::Auto);
    }
    if t.len() > 5 && t[..5].eq_ignore_ascii_case("calc(") && t.ends_with(')') {
        return parse_calc(&t[5..t.len() - 1]);
    }
    if t.len() > 4 && (t[..4].eq_ignore_ascii_case("min(") || t[..4].eq_ignore_ascii_case("max(") || t.len() > 6 && t[..6].eq_ignore_ascii_case("clamp(")) {
        return store_expr(t);
    }
    let mut i = 0;
    if b[0] == b'-' || b[0] == b'+' {
        i = 1;
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        i += 1;
    }
    if i == 0 || (i == 1 && !b[0].is_ascii_digit()) {
        return None;
    }
    let n: f32 = t[..i].parse().ok()?;
    let unit = t[i..].to_ascii_lowercase();
    Some(match unit.as_str() {
        "px" => Value::Px(n),
        "" => Value::Num(n),
        "em" => Value::Em(n),
        "rem" => Value::Rem(n),
        "%" => Value::Pct(n),
        "pt" => Value::Px(n * 4.0 / 3.0),
        "pc" => Value::Px(n * 16.0),
        "in" => Value::Px(n * 96.0),
        "cm" => Value::Px(n * 37.795),
        "mm" => Value::Px(n * 3.7795),
        "q" => Value::Px(n * 0.945),
        "vw" | "vmin" | "dvw" | "svw" | "lvw" => Value::Vw(n),
        "vh" | "vmax" | "dvh" | "svh" | "lvh" => Value::Vh(n),
        "ex" | "ch" => Value::Em(n * 0.5),
        _ => return None,
    })
}

/// A length where unitless numbers are only valid as 0.
fn length(t: &str) -> Option<Value> {
    match parse_len(t)? {
        Value::Num(n) if n != 0.0 => None,
        Value::Num(_) => Some(Value::Px(0.0)),
        v => Some(v),
    }
}

fn hex(c: u8) -> u32 {
    (c as char).to_digit(16).unwrap_or(0)
}

pub fn parse_color(t: &str) -> Option<u32> {
    let t = t.trim();
    if let Some(h) = t.strip_prefix('#') {
        let b = h.as_bytes();
        if !b.iter().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let (r, g, bl, a) = match b.len() {
            3 => (hex(b[0]) * 17, hex(b[1]) * 17, hex(b[2]) * 17, 255),
            4 => (hex(b[0]) * 17, hex(b[1]) * 17, hex(b[2]) * 17, hex(b[3]) * 17),
            6 => (
                hex(b[0]) * 16 + hex(b[1]),
                hex(b[2]) * 16 + hex(b[3]),
                hex(b[4]) * 16 + hex(b[5]),
                255,
            ),
            8 => (
                hex(b[0]) * 16 + hex(b[1]),
                hex(b[2]) * 16 + hex(b[3]),
                hex(b[4]) * 16 + hex(b[5]),
                hex(b[6]) * 16 + hex(b[7]),
            ),
            _ => return None,
        };
        return Some(a << 24 | r << 16 | g << 8 | bl);
    }
    let lower = t.to_ascii_lowercase();
    if let Some(p) = lower.find('(') {
        if !lower.ends_with(')') {
            return None;
        }
        return color_fn(&lower[..p], &lower[p + 1..lower.len() - 1]);
    }
    if let Some(c) = system_color(&lower) {
        return Some(0xFF00_0000 | c);
    }
    if lower == "transparent" {
        return Some(0);
    }
    named_color(&lower).map(|c| 0xFF00_0000 | c)
}

fn system_color(n: &str) -> Option<u32> {
    Some(match n {
        "canvas" | "field" | "window" | "buttonhighlight" => 0xFFFFFF,
        "canvastext" | "fieldtext" | "buttontext" | "windowtext" | "menutext" => 0x000000,
        "linktext" => 0x0000EE,
        "visitedtext" => 0x551A8B,
        "activetext" => 0xFF0000,
        "buttonface" | "buttonborder" | "threedface" => 0xEFEFEF,
        "graytext" => 0x808080,
        "highlight" | "selecteditem" | "accentcolor" => 0x0075FF,
        "highlighttext" | "selecteditemtext" | "accentcolortext" => 0xFFFFFF,
        "mark" => 0xFFFF00,
        "marktext" => 0x000000,
        _ => return None,
    })
}

/// Split a function's arguments at top-level commas.
fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0, 0);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// A number or percentage (`pct` = the value of 100%); `none` is 0.
fn cnum(s: &str, pct: f32) -> Option<f32> {
    if s == "none" {
        return Some(0.0);
    }
    match s.strip_suffix('%') {
        Some(p) => Some(p.parse::<f32>().ok()? * pct / 100.0),
        None => s.parse::<f32>().ok(),
    }
}

/// A hue in degrees.
fn hue(s: &str) -> Option<f32> {
    if s == "none" {
        return Some(0.0);
    }
    let (v, mul) = if let Some(v) = s.strip_suffix("deg") {
        (v, 1.0)
    } else if let Some(v) = s.strip_suffix("grad") {
        (v, 0.9)
    } else if let Some(v) = s.strip_suffix("rad") {
        (v, 180.0 / std::f32::consts::PI)
    } else if let Some(v) = s.strip_suffix("turn") {
        (v, 360.0)
    } else {
        (s, 1.0)
    };
    Some((v.parse::<f32>().ok()? * mul).rem_euclid(360.0))
}

fn srgb_encode(c: f32) -> u32 {
    let c = c.clamp(0.0, 1.0);
    let v = if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (v * 255.0).round().clamp(0.0, 255.0) as u32
}

fn oklab_rgb(l: f32, a: f32, b: f32) -> (u32, u32, u32) {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    (
        srgb_encode(4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3),
        srgb_encode(-1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3),
        srgb_encode(-0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3),
    )
}

/// CIE Lab (D50) to sRGB.
fn lab_rgb(l: f32, a: f32, b: f32) -> (u32, u32, u32) {
    let fy = (l + 16.0) / 116.0;
    let (fx, fz) = (fy + a / 500.0, fy - b / 200.0);
    let inv = |t: f32| if t > 6.0 / 29.0 { t * t * t } else { 3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0) };
    let (x, y, z) = (inv(fx) * 0.964_22, inv(fy), inv(fz) * 0.825_21);
    (
        srgb_encode(3.133_856 * x - 1.616_866_7 * y - 0.490_614_6 * z),
        srgb_encode(-0.978_768_4 * x + 1.916_141_5 * y + 0.033_454 * z),
        srgb_encode(0.071_945_3 * x - 0.228_991_4 * y + 1.405_242_7 * z),
    )
}

/// rgb() / hsl() / hwb() / lab() / lch() / oklab() / oklch() / color() /
/// color-mix() / light-dark(), lowercased.
fn color_fn(f: &str, inner: &str) -> Option<u32> {
    match f {
        "light-dark" => return parse_color(split_top_commas(inner).first()?),
        "color-mix" => {
            let args = split_top_commas(inner);
            if args.len() != 3 || !args[0].starts_with("in ") {
                return None;
            }
            let part = |s: &str| -> Option<(u32, Option<f32>)> {
                let s = s.trim();
                match s.rfind(' ') {
                    Some(i) if s.ends_with('%') => {
                        let p = s[i + 1..].trim_end_matches('%').parse::<f32>().ok()?;
                        Some((parse_color(&s[..i])?, Some(p / 100.0)))
                    }
                    _ => Some((parse_color(s)?, None)),
                }
            };
            let ((c1, p1), (c2, p2)) = (part(args[1])?, part(args[2])?);
            let (w1, w2) = match (p1, p2) {
                (Some(a), Some(b)) => (a, b),
                (Some(a), None) => (a, 1.0 - a),
                (None, Some(b)) => (1.0 - b, b),
                (None, None) => (0.5, 0.5),
            };
            let total = (w1 + w2).max(1e-6);
            let ch = |c: u32, sh: u32| ((c >> sh) & 0xFF) as f32;
            let (a1, a2) = (ch(c1, 24) / 255.0, ch(c2, 24) / 255.0);
            let alpha = (a1 * w1 + a2 * w2) / total;
            let mix = |sh: u32| -> u32 {
                if alpha <= 0.0 {
                    return 0;
                }
                (((ch(c1, sh) * a1 * w1 + ch(c2, sh) * a2 * w2) / total) / alpha).round().clamp(0.0, 255.0) as u32
            };
            let a = (alpha * total.min(1.0) * 255.0).round() as u32;
            return Some(a << 24 | mix(16) << 16 | mix(8) << 8 | mix(0));
        }
        _ => {}
    }
    if inner.trim_start().starts_with("from ") {
        return None; // relative color syntax
    }
    let parts: Vec<&str> = inner.split(|c: char| c == ',' || c == '/' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
    let (args, alpha_s) = if f == "color" {
        if parts.len() < 4 {
            return None;
        }
        (&parts[1..4], parts.get(4))
    } else {
        if parts.len() < 3 {
            return None;
        }
        (&parts[..3], parts.get(3))
    };
    let alpha = match alpha_s {
        Some(a) => cnum(a, 1.0)?,
        None => 1.0,
    };
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    let (r, g, b) = match f {
        "rgb" | "rgba" => {
            let ch = |s: &str| -> Option<u32> { Some(cnum(s, 255.0)?.round().clamp(0.0, 255.0) as u32) };
            (ch(args[0])?, ch(args[1])?, ch(args[2])?)
        }
        "hsl" | "hsla" => {
            let h = hue(args[0])? / 360.0;
            let s = cnum(args[1].trim_end_matches('%'), 1.0)? / 100.0;
            let l = cnum(args[2].trim_end_matches('%'), 1.0)? / 100.0;
            hsl(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0))
        }
        "hwb" => {
            let h = hue(args[0])? / 360.0;
            let w = cnum(args[1].trim_end_matches('%'), 1.0)? / 100.0;
            let bl = cnum(args[2].trim_end_matches('%'), 1.0)? / 100.0;
            if w + bl >= 1.0 {
                let g = (w / (w + bl) * 255.0).round() as u32;
                (g, g, g)
            } else {
                let (r, g, b) = hsl(h, 1.0, 0.5);
                let f = |c: u32| ((c as f32 / 255.0 * (1.0 - w - bl) + w) * 255.0).round() as u32;
                (f(r), f(g), f(b))
            }
        }
        "lab" => lab_rgb(cnum(args[0], 100.0)?, cnum(args[1], 125.0)?, cnum(args[2], 125.0)?),
        "lch" => {
            let (l, c, h) = (cnum(args[0], 100.0)?, cnum(args[1], 150.0)?, hue(args[2])?.to_radians());
            lab_rgb(l, c * h.cos(), c * h.sin())
        }
        "oklab" => oklab_rgb(cnum(args[0], 1.0)?, cnum(args[1], 0.4)?, cnum(args[2], 0.4)?),
        "oklch" => {
            let (l, c, h) = (cnum(args[0], 1.0)?, cnum(args[1], 0.4)?, hue(args[2])?.to_radians());
            oklab_rgb(l, c * h.cos(), c * h.sin())
        }
        "color" => {
            let ch = |s: &str| -> Option<u32> { Some((cnum(s, 1.0)? * 255.0).round().clamp(0.0, 255.0) as u32) };
            (ch(args[0])?, ch(args[1])?, ch(args[2])?)
        }
        _ => return None,
    };
    Some(a << 24 | r << 16 | g << 8 | b)
}

fn hsl(h: f32, s: f32, l: f32) -> (u32, u32, u32) {
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        if t < 0.0 {
            t += 1.0
        }
        if t > 1.0 {
            t -= 1.0
        }
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round() as u32
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

fn named_color(n: &str) -> Option<u32> {
    Some(match n {
        "black" => 0x000000, "white" => 0xffffff, "red" => 0xff0000, "green" => 0x008000,
        "blue" => 0x0000ff, "yellow" => 0xffff00, "gray" | "grey" => 0x808080,
        "silver" => 0xc0c0c0, "maroon" => 0x800000, "purple" => 0x800080,
        "fuchsia" | "magenta" => 0xff00ff, "lime" => 0x00ff00, "olive" => 0x808000,
        "navy" => 0x000080, "teal" => 0x008080, "aqua" | "cyan" => 0x00ffff,
        "orange" => 0xffa500, "darkgray" | "darkgrey" => 0xa9a9a9,
        "lightgray" | "lightgrey" => 0xd3d3d3, "gainsboro" => 0xdcdcdc,
        "whitesmoke" => 0xf5f5f5, "dimgray" | "dimgrey" => 0x696969,
        "darkred" => 0x8b0000, "darkgreen" => 0x006400, "darkblue" => 0x00008b,
        "lightblue" => 0xadd8e6, "lightgreen" => 0x90ee90, "lightyellow" => 0xffffe0,
        "pink" => 0xffc0cb, "brown" => 0xa52a2a, "gold" => 0xffd700, "beige" => 0xf5f5dc,
        "ivory" => 0xfffff0, "khaki" => 0xf0e68c, "coral" => 0xff7f50, "salmon" => 0xfa8072,
        "tomato" => 0xff6347, "crimson" => 0xdc143c, "indigo" => 0x4b0082,
        "violet" => 0xee82ee, "orchid" => 0xda70d6, "plum" => 0xdda0dd, "tan" => 0xd2b48c,
        "chocolate" => 0xd2691e, "sienna" => 0xa0522d, "peru" => 0xcd853f,
        "skyblue" => 0x87ceeb, "steelblue" => 0x4682b4, "royalblue" => 0x4169e1,
        "dodgerblue" => 0x1e90ff, "deepskyblue" => 0x00bfff, "cornflowerblue" => 0x6495ed,
        "midnightblue" => 0x191970, "slategray" | "slategrey" => 0x708090,
        "lightslategray" => 0x778899, "darkslategray" => 0x2f4f4f, "seagreen" => 0x2e8b57,
        "forestgreen" => 0x228b22, "limegreen" => 0x32cd32, "olivedrab" => 0x6b8e23,
        "darkorange" => 0xff8c00, "orangered" => 0xff4500, "firebrick" => 0xb22222,
        "hotpink" => 0xff69b4, "deeppink" => 0xff1493, "lavender" => 0xe6e6fa,
        "linen" => 0xfaf0e6, "mintcream" => 0xf5fffa, "aliceblue" => 0xf0f8ff,
        "honeydew" => 0xf0fff0, "azure" => 0xf0ffff, "snow" => 0xfffafa,
        "seashell" => 0xfff5ee, "oldlace" => 0xfdf5e6, "floralwhite" => 0xfffaf0,
        "ghostwhite" => 0xf8f8ff, "antiquewhite" => 0xfaebd7, "wheat" => 0xf5deb3,
        "cornsilk" => 0xfff8dc, "lemonchiffon" => 0xfffacd, "mistyrose" => 0xffe4e1,
        "turquoise" => 0x40e0d0, "darkcyan" => 0x008b8b, "cadetblue" => 0x5f9ea0,
        "darkviolet" => 0x9400d3, "darkmagenta" => 0x8b008b, "goldenrod" => 0xdaa520,
        "darkgoldenrod" => 0xb8860b, "rebeccapurple" => 0x663399, "slateblue" => 0x6a5acd,
        "lightcoral" => 0xf08080, "palegreen" => 0x98fb98, "lightcyan" => 0xe0ffff,
        "lightpink" => 0xffb6c1, "lightsalmon" => 0xffa07a, "lightskyblue" => 0x87cefa,
        "lightsteelblue" => 0xb0c4de, "powderblue" => 0xb0e0e6, "navajowhite" => 0xffdead,
        "moccasin" => 0xffe4b5, "bisque" => 0xffe4c4, "peachpuff" => 0xffdab9,
        "papayawhip" => 0xffefd5, "blanchedalmond" => 0xffebcd, "springgreen" => 0x00ff7f,
        "yellowgreen" => 0x9acd32, "chartreuse" => 0x7fff00, "lawngreen" => 0x7cfc00,
        "darkkhaki" => 0xbdb76b, "rosybrown" => 0xbc8f8f, "saddlebrown" => 0x8b4513,
        "darkslateblue" => 0x483d8b, "mediumblue" => 0x0000cd, "darkolivegreen" => 0x556b2f,
        "mediumseagreen" => 0x3cb371, "darkseagreen" => 0x8fbc8f, "indianred" => 0xcd5c5c,
        "palevioletred" => 0xdb7093, "mediumpurple" => 0x9370db, "thistle" => 0xd8bfd8,
        _ => return None,
    })
}

fn color_value(t: &str) -> Option<Value> {
    if t.eq_ignore_ascii_case("currentcolor") {
        return Some(Value::Color(CURRENT_COLOR));
    }
    parse_color(t).map(Value::Color)
}

fn is_border_style(t: &str) -> Option<Kw> {
    Some(match t {
        "none" | "hidden" => Kw::None,
        "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => {
            Kw::Solid
        }
        _ => return None,
    })
}

fn border_width(t: &str) -> Option<Value> {
    match t {
        "thin" => Some(Value::Px(1.0)),
        "medium" => Some(Value::Px(3.0)),
        "thick" => Some(Value::Px(5.0)),
        _ => length(t),
    }
}

fn font_size(t: &str) -> Option<Value> {
    Some(match t {
        "xx-small" => Value::Px(9.0),
        "x-small" => Value::Px(10.0),
        "small" => Value::Px(13.0),
        "medium" => Value::Px(16.0),
        "large" => Value::Px(18.0),
        "x-large" => Value::Px(24.0),
        "xx-large" => Value::Px(32.0),
        "xxx-large" => Value::Px(48.0),
        "smaller" => Value::Em(0.833),
        "larger" => Value::Em(1.2),
        _ => length(t)?,
    })
}

/// Generic family a name falls back to.
fn generic_of(f: &str) -> Kw {
    if f.contains("mono") || f.contains("courier") || f.contains("consol") || f.contains("menlo") || f == "code" {
        Kw::Monospace
    } else if f == "serif" || f.contains("times") || f.contains("georgia") || f.contains("garamond") || f.contains("palatino") || f.contains("book") || f.contains("cambria") {
        Kw::Serif
    } else {
        Kw::SansSerif
    }
}

thread_local! {
    static FAMILIES: RefCell<(Vec<(Rc<[Box<str>]>, Kw)>, FxHashMap<String, u32>)> = RefCell::new(Default::default());
    static FONT_FACES: RefCell<Vec<FontFace>> = const { RefCell::new(Vec::new()) };
}

/// The family names (lowercase) and generic fallback of a font-family value.
pub fn families(i: u32) -> (Rc<[Box<str>]>, Kw) {
    FAMILIES.with(|t| t.borrow().0[i as usize].clone())
}

/// A font-family list's names (lowercase) and generic fallback.
pub fn family_names(v: &str) -> (Rc<[Box<str>]>, Kw) {
    match font_family(v) {
        Value::Family(i) => families(i),
        _ => (Rc::from(Vec::new()), Kw::SansSerif),
    }
}

fn font_family(v: &str) -> Value {
    let key = v.trim().to_ascii_lowercase();
    if let Some(&i) = FAMILIES.with(|t| t.borrow().1.get(&key).copied()).as_ref() {
        return Value::Family(i);
    }
    let names: Vec<Box<str>> = key
        .split(',')
        .map(|n| n.trim().trim_matches(|c| c == '"' || c == '\'').trim().into())
        .filter(|n: &Box<str>| !n.is_empty())
        .collect();
    let generic = names
        .iter()
        .find_map(|n| match &**n {
            "monospace" | "ui-monospace" => Some(Kw::Monospace),
            "serif" | "ui-serif" => Some(Kw::Serif),
            "sans-serif" | "system-ui" | "ui-sans-serif" | "-apple-system" => Some(Kw::SansSerif),
            _ => None,
        })
        .unwrap_or_else(|| generic_of(names.first().map_or("", |n| n)));
    FAMILIES.with(|t| {
        let mut t = t.borrow_mut();
        let i = t.0.len() as u32;
        t.0.push((names.into(), generic));
        t.1.insert(key, i);
        Value::Family(i)
    })
}

/// An @font-face rule.
#[derive(Clone, Debug)]
pub struct FontFace {
    pub family: Box<str>,
    pub url: String,
    pub weight: (u16, u16),
    pub italic: bool,
}

/// All @font-face rules parsed so far (on this thread).
pub fn font_faces() -> Vec<FontFace> {
    FONT_FACES.with(|f| f.borrow().clone())
}

fn parse_font_face(body: &str) {
    let mut family = None;
    let mut url = None;
    let (mut weight, mut italic) = ((400u16, 400u16), false);
    let mut latin = true;
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let end = find_top(b, i, b";").unwrap_or(b.len());
        let decl = &body[i..end];
        i = end + 1;
        let Some((name, value)) = decl.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "font-family" => family = Some(value.trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase()),
            "src" => {
                // Prefer formats we can load: woff2, woff, then truetype/opentype.
                let mut best: Option<(u8, String)> = None;
                for part in split_top(value, b',') {
                    let part = part.trim();
                    let lower = part.to_ascii_lowercase();
                    let Some(us) = lower.find("url(") else { continue };
                    let Some(ue) = find_top(part.as_bytes(), us + 4, b")") else { continue };
                    let raw = part[us + 4..ue].trim().trim_matches(|c| c == '"' || c == '\'');
                    let fmt = lower[ue..].to_string();
                    let path = raw.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
                    let rank = if fmt.contains("woff2") || path.ends_with(".woff2") {
                        4
                    } else if fmt.contains("woff") || path.ends_with(".woff") {
                        3
                    } else if fmt.contains("truetype") || fmt.contains("opentype") || path.ends_with(".ttf") || path.ends_with(".otf") {
                        2
                    } else if fmt.contains("svg") || path.ends_with(".svg") || path.ends_with(".eot") {
                        0
                    } else {
                        1
                    };
                    if rank > 0 && best.as_ref().is_none_or(|b| rank > b.0) {
                        let abs = BASE.with(|base| crate::net::resolve(&base.borrow(), raw));
                        best = Some((rank, abs));
                    }
                }
                url = best.map(|b| b.1);
            }
            "font-weight" => {
                let w = |t: &str| match t {
                    "normal" => Some(400),
                    "bold" => Some(700),
                    n => n.parse::<f32>().ok().map(|v| v as u16),
                };
                let toks: Vec<&str> = value.split_whitespace().collect();
                if let Some(a) = toks.first().and_then(|t| w(t)) {
                    weight = (a, toks.get(1).and_then(|t| w(t)).unwrap_or(a));
                }
            }
            // Text runs use a single face, so only subsets covering Latin are kept.
            "unicode-range" => latin = covers(value, 'a' as u32),
            "font-style" => italic = value.to_ascii_lowercase().starts_with("italic") || value.to_ascii_lowercase().starts_with("oblique"),
            _ => {}
        }
    }
    if let (Some(family), Some(url), true) = (family, url, latin) {
        FONT_FACES.with(|f| {
            let mut f = f.borrow_mut();
            if !f.iter().any(|x| x.url == url && *x.family == *family) {
                f.push(FontFace { family: family.into(), url, weight, italic });
            }
        });
    }
}

/// Does a `unicode-range` value (`U+0000-00FF, U+0131, U+4??`) include `cp`?
fn covers(range: &str, cp: u32) -> bool {
    range.split(',').any(|r| {
        let r = r.trim();
        let Some(r) = r.strip_prefix("U+").or_else(|| r.strip_prefix("u+")) else { return false };
        let hex = |t: &str| u32::from_str_radix(t, 16).ok();
        let (lo, hi) = match r.split_once('-') {
            Some((a, b)) => (hex(a), hex(b)),
            None if r.contains('?') => (hex(&r.replace('?', "0")), hex(&r.replace('?', "F"))),
            None => (hex(r), hex(r)),
        };
        matches!((lo, hi), (Some(lo), Some(hi)) if (lo..=hi).contains(&cp))
    })
}

fn push4(out: &mut Vec<Decl>, props: [Prop; 4], vals: &[Value], important: bool) {
    let v = match vals.len() {
        1 => [vals[0]; 4],
        2 => [vals[0], vals[1], vals[0], vals[1]],
        3 => [vals[0], vals[1], vals[2], vals[1]],
        4 => [vals[0], vals[1], vals[2], vals[3]],
        _ => return,
    };
    for i in 0..4 {
        out.push(Decl { prop: props[i], value: v[i], important });
    }
}

/// Parse one `name: value` declaration, appending longhands to `out`.
/// Physical equivalents of a logical property (horizontal, left-to-right).
fn logical_props(name: &str, value: &str) -> Option<Vec<(String, String)>> {
    let pair = |a: &str, b: &str| -> Vec<(String, String)> {
        let parts = split_top(value.trim(), b' ').into_iter().filter(|p| !p.trim().is_empty()).collect::<Vec<_>>();
        let (v1, v2) = match parts.as_slice() {
            [x, y, ..] => (x.trim().to_owned(), y.trim().to_owned()),
            _ => (value.trim().to_owned(), value.trim().to_owned()),
        };
        vec![(a.to_owned(), v1), (b.to_owned(), v2)]
    };
    let one = |a: &str| vec![(a.to_owned(), value.to_owned())];
    let both = |a: &str, b: &str| vec![(a.to_owned(), value.to_owned()), (b.to_owned(), value.to_owned())];
    Some(match name {
        "margin-inline" => pair("margin-left", "margin-right"),
        "margin-block" => pair("margin-top", "margin-bottom"),
        "padding-inline" => pair("padding-left", "padding-right"),
        "padding-block" => pair("padding-top", "padding-bottom"),
        "inset-inline" => pair("left", "right"),
        "inset-block" => pair("top", "bottom"),
        "inset-inline-start" => one("left"),
        "inset-inline-end" => one("right"),
        "inset-block-start" => one("top"),
        "inset-block-end" => one("bottom"),
        "inline-size" => one("width"),
        "block-size" => one("height"),
        "min-inline-size" => one("min-width"),
        "max-inline-size" => one("max-width"),
        "min-block-size" => one("min-height"),
        "max-block-size" => one("max-height"),
        "border-inline" => both("border-left", "border-right"),
        "border-block" => both("border-top", "border-bottom"),
        "border-inline-start" => one("border-left"),
        "border-inline-end" => one("border-right"),
        "border-block-start" => one("border-top"),
        "border-block-end" => one("border-bottom"),
        "border-inline-width" => pair("border-left-width", "border-right-width"),
        "border-inline-color" => pair("border-left-color", "border-right-color"),
        "border-inline-style" => pair("border-left-style", "border-right-style"),
        "border-block-width" => pair("border-top-width", "border-bottom-width"),
        "border-block-color" => pair("border-top-color", "border-bottom-color"),
        "border-block-style" => pair("border-top-style", "border-bottom-style"),
        "border-inline-start-width" => one("border-left-width"),
        "border-inline-end-width" => one("border-right-width"),
        "border-block-start-width" => one("border-top-width"),
        "border-block-end-width" => one("border-bottom-width"),
        "border-inline-start-color" => one("border-left-color"),
        "border-inline-end-color" => one("border-right-color"),
        "border-block-start-color" => one("border-top-color"),
        "border-block-end-color" => one("border-bottom-color"),
        "border-start-start-radius" => one("border-top-left-radius"),
        "border-start-end-radius" => one("border-top-right-radius"),
        "border-end-start-radius" => one("border-bottom-left-radius"),
        "border-end-end-radius" => one("border-bottom-right-radius"),
        _ => return None,
    })
}

/// Properties whose value inherits by default (for `unset`).
fn inherited(p: Prop) -> bool {
    matches!(
        p,
        Color | FontSize | FontWeight | FontStyle | FontFamily | TextAlign | LineHeight | WhiteSpace | TextTransform | ListStyleType | Visibility | PointerEvents | BorderCollapse | WordBreak | OverflowWrap
    )
}

pub fn parse_property(name: &str, value: &str, important: bool, out: &mut Vec<Decl>) {
    if let Some(list) = logical_props(name, value) {
        for (n, v) in list {
            parse_property(&n, &v, important, out);
        }
        return;
    }
    let lower = value.trim().to_ascii_lowercase();
    let v = lower.as_str();
    macro_rules! push {
        ($p:expr, $v:expr) => {
            out.push(Decl { prop: $p, value: $v, important })
        };
    }

    // `unset` / `revert`: inherit for inherited properties, else initial.
    let unset = matches!(v, "unset" | "revert" | "revert-layer");
    let global = match v {
        "inherit" => Some(Value::Inherit),
        "initial" | "unset" | "revert" | "revert-layer" => Some(Value::Initial),
        _ => None,
    };
    macro_rules! push_global {
        ($p:expr, $g:expr) => {{
            let p = $p;
            push!(p, if unset && inherited(p) { Value::Inherit } else { $g })
        }};
    }
    if global.is_none() {
        match name {
            "transform" => {
                if let Some(t) = transform_value(&lower) {
                    push!(TransformP, t);
                }
                return;
            }
            "filter" | "-webkit-filter" | "backdrop-filter" | "-webkit-backdrop-filter" => {
                if let Some(f) = filter_value(&lower) {
                    push!(if name.ends_with("backdrop-filter") { BackdropFilterP } else { FilterP }, f);
                }
                return;
            }
            "translate" | "rotate" | "scale" => {
                if let Some(t) = transform_prop(name, &lower) {
                    push!(match name { "translate" => TranslateP, "rotate" => RotateP, _ => ScaleP }, t);
                }
                return;
            }
            "animation" | "animation-name" | "animation-fill-mode" | "animation-iteration-count" => {
                let (mut names, mut fill, mut finite) = (Vec::new(), false, true);
                for part in split_top(value, b',') {
                    let mut name = None;
                    for tok in part.split_whitespace() {
                        let t = tok.to_ascii_lowercase();
                        let is_time = t.ends_with("ms") || (t.ends_with('s') && t[..t.len() - 1].parse::<f32>().is_ok());
                        match t.as_str() {
                            "forwards" | "both" => fill = true,
                            "infinite" => finite = false,
                            "none" | "backwards" | "normal" | "reverse" | "alternate" | "alternate-reverse" | "running" | "paused" | "linear" | "ease"
                            | "ease-in" | "ease-out" | "ease-in-out" | "step-start" | "step-end" | "initial" | "inherit" => {}
                            _ if is_time || t.parse::<f32>().is_ok() || t.starts_with("cubic-bezier(") || t.starts_with("steps(") || t.ends_with(')') => {}
                            _ if name.is_none() => name = Some(tok.trim_matches(|c| c == '"' || c == '\'').to_owned()),
                            _ => {}
                        }
                    }
                    names.extend(name);
                }
                if matches!(name, "animation" | "animation-name") {
                    let list: Rc<[Rc<str>]> = names.into_iter().map(Rc::from).collect();
                    let i = NAME_LISTS.with(|t| {
                        let mut t = t.borrow_mut();
                        t.push(list);
                        t.len() as u32 - 1
                    });
                    push!(AnimName, Value::Names(i));
                }
                if matches!(name, "animation" | "animation-fill-mode") {
                    push!(AnimFill, Value::Num(fill as u8 as f32));
                }
                if matches!(name, "animation" | "animation-iteration-count") {
                    push!(AnimIter, Value::Num(finite as u8 as f32));
                }
                return;
            }
            "word-break" => {
                let n = match lower.as_str() {
                    "break-all" => 1.0,
                    "keep-all" => 2.0,
                    "break-word" => {
                        push!(OverflowWrap, Value::Num(1.0));
                        0.0
                    }
                    _ => 0.0,
                };
                push!(WordBreak, Value::Num(n));
                return;
            }
            "overflow-wrap" | "word-wrap" => {
                push!(OverflowWrap, Value::Num(if matches!(lower.as_str(), "anywhere" | "break-word") { 1.0 } else { 0.0 }));
                return;
            }
            "text-overflow" => {
                push!(TextOverflow, Value::Num(if lower.contains("ellipsis") { 1.0 } else { 0.0 }));
                return;
            }
            "line-clamp" | "-webkit-line-clamp" => {
                push!(LineClamp, Value::Num(lower.parse::<f32>().unwrap_or(0.0).max(0.0)));
                return;
            }
            "transform-origin" => {
                let mut tmp = Vec::new();
                bg_position(&tokens_ws(&lower), important, &mut tmp);
                for d in tmp {
                    out.push(Decl { prop: if d.prop == BgPosX { TransformOriginX } else { TransformOriginY }, ..d });
                }
                return;
            }
            _ => {}
        }
    }
    if global.is_none() && name == "content" {
        if let Some(c) = content_value(value) {
            push!(Content, c);
        }
        return;
    }
    if let Some(g) = global {
        // A global keyword applies to every longhand the property sets.
        let props: &[Prop] = match name {
            "margin" => &MARGIN,
            "padding" => &PADDING,
            "margin-block" => &[MarginTop, MarginBottom],
            "margin-inline" => &[MarginLeft, MarginRight],
            "padding-block" => &[PaddingTop, PaddingBottom],
            "padding-inline" => &[PaddingLeft, PaddingRight],
            "border" => &[
                BorderTopWidth, BorderRightWidth, BorderBottomWidth, BorderLeftWidth,
                BorderTopColor, BorderRightColor, BorderBottomColor, BorderLeftColor,
                BorderTopStyle, BorderRightStyle, BorderBottomStyle, BorderLeftStyle,
            ],
            "border-width" => &[BorderTopWidth, BorderRightWidth, BorderBottomWidth, BorderLeftWidth],
            "border-color" => &[BorderTopColor, BorderRightColor, BorderBottomColor, BorderLeftColor],
            "border-style" => &[BorderTopStyle, BorderRightStyle, BorderBottomStyle, BorderLeftStyle],
            "border-top" | "border-block-start" => &[BorderTopWidth, BorderTopColor, BorderTopStyle],
            "border-right" | "border-inline-end" => &[BorderRightWidth, BorderRightColor, BorderRightStyle],
            "border-bottom" | "border-block-end" => &[BorderBottomWidth, BorderBottomColor, BorderBottomStyle],
            "border-left" | "border-inline-start" => &[BorderLeftWidth, BorderLeftColor, BorderLeftStyle],
            "border-block" => &[BorderTopWidth, BorderTopColor, BorderTopStyle, BorderBottomWidth, BorderBottomColor, BorderBottomStyle],
            "border-inline" => &[BorderLeftWidth, BorderLeftColor, BorderLeftStyle, BorderRightWidth, BorderRightColor, BorderRightStyle],
            "border-radius" | "-webkit-border-radius" | "-moz-border-radius" => &[RadiusTL, RadiusTR, RadiusBR, RadiusBL],
            "border-top-left-radius" | "border-start-start-radius" => &[RadiusTL],
            "border-top-right-radius" | "border-start-end-radius" => &[RadiusTR],
            "border-bottom-right-radius" | "border-end-end-radius" => &[RadiusBR],
            "border-bottom-left-radius" | "border-end-start-radius" => &[RadiusBL],
            "inset" => &[InsetTop, InsetRight, InsetBottom, InsetLeft],
            "inset-block" => &[InsetTop, InsetBottom],
            "inset-inline" => &[InsetLeft, InsetRight],
            "inset-block-start" => &[InsetTop],
            "inset-block-end" => &[InsetBottom],
            "inset-inline-start" => &[InsetLeft],
            "inset-inline-end" => &[InsetRight],
            "inline-size" => &[Width],
            "block-size" => &[Height],
            "min-inline-size" => &[MinWidth],
            "max-inline-size" => &[MaxWidth],
            "min-block-size" => &[MinHeight],
            "max-block-size" => &[MaxHeight],
            "overflow" => &[Overflow, OverflowX, OverflowY],
            "overflow-x" => &[OverflowX],
            "overflow-y" => &[OverflowY],
            "flex" => &[FlexGrow, FlexShrink, FlexBasis],
            "flex-flow" => &[FlexDirection, FlexWrap],
            "gap" | "grid-gap" => &[RowGap, ColumnGap],
            "grid-row-gap" => &[RowGap],
            "grid-column-gap" => &[ColumnGap],
            "place-items" => &[AlignItems, JustifyItems],
            "place-content" => &[AlignContent, JustifyContent],
            "place-self" => &[AlignSelf, JustifySelf],
            "grid" | "grid-template" => &[GridTemplateColumns, GridTemplateRows, GridTemplateAreas, GridAutoRows],
            "grid-template-areas" => &[GridTemplateAreas],
            "grid-area" => &[GridArea, GridRowStart, GridColumnStart, GridRowEnd, GridColumnEnd],
            "grid-row" => &[GridRowStart, GridRowEnd],
            "grid-column" => &[GridColumnStart, GridColumnEnd],
            "grid-row-start" => &[GridRowStart],
            "grid-row-end" => &[GridRowEnd],
            "grid-column-start" => &[GridColumnStart],
            "grid-column-end" => &[GridColumnEnd],
            "background" => &[Background, BackgroundImage, BgSizeX, BgSizeY, BgPosX, BgPosY, BgRepeat],
            "background-color" => &[Background],
            "background-size" => &[BgSizeX, BgSizeY],
            "background-position" => &[BgPosX, BgPosY],
            "background-position-x" => &[BgPosX],
            "background-position-y" => &[BgPosY],
            "transform-origin" => &[TransformOriginX, TransformOriginY],
            "font" => &[FontSize, FontWeight, FontStyle, FontFamily, LineHeight],
            "word-break" => &[WordBreak],
            "overflow-wrap" | "word-wrap" => &[OverflowWrap],
            "text-overflow" => &[TextOverflow],
            "-webkit-line-clamp" | "line-clamp" => &[LineClamp],
            "animation" => &[AnimName, AnimFill, AnimIter],
            _ => match simple_prop(name) {
                Some(p) => {
                    push_global!(p, g);
                    return;
                }
                None => return,
            },
        };
        for &p in props {
            push_global!(p, g);
        }
        return;
    }

    if parse_more(name, v, value.trim(), important, out) {
        return;
    }
    let kw = |k: Kw| Some(Value::Kw(k));
    let val: Option<Value> = match name {
        "display" => match v {
            "block" | "flow-root" | "flex" | "grid" | "-webkit-box" | "-ms-flexbox" | "-webkit-flex" | "flow" => kw(Kw::Block),
            "inline" => kw(Kw::Inline),
            "inline-block" | "inline-flex" | "inline-grid" | "-webkit-inline-box" => kw(Kw::InlineBlock),
            "list-item" => kw(Kw::ListItem),
            "table" => kw(Kw::Table),
            "inline-table" => kw(Kw::InlineTable),
            "table-row" => kw(Kw::TableRow),
            "table-cell" => kw(Kw::TableCell),
            "table-row-group" | "table-header-group" | "table-footer-group" => kw(Kw::TableRowGroup),
            "table-caption" => kw(Kw::TableCaption),
            "contents" => kw(Kw::Contents),
            "none" | "table-column" | "table-column-group" => kw(Kw::None),
            _ => None,
        },
        "position" => match v {
            "absolute" | "fixed" => kw(Kw::Out),
            "static" | "relative" | "sticky" | "-webkit-sticky" => kw(Kw::InFlow),
            _ => None,
        },
        "color" => color_value(v),
        "background-color" => color_value(v),
        "background" => {
            // Shorthand resets the colour to transparent unless one is given.
            let c = tokens(v).into_iter().find_map(color_value).unwrap_or(Value::Color(0));
            push!(Background, c);
            return;
        }
        "font-size" => font_size(v),
        "font-weight" => match v {
            "bold" | "bolder" => kw(Kw::Bold),
            "normal" | "lighter" => kw(Kw::Normal),
            _ => v.parse::<f32>().ok().map(|n| Value::Kw(if n >= 600.0 { Kw::Bold } else { Kw::Normal })),
        },
        "font-style" => match v {
            "italic" | "oblique" => kw(Kw::Italic),
            "normal" => kw(Kw::Normal),
            _ => None,
        },
        "font-family" => Some(font_family(v)),
        "font" => {
            let toks = tokens(value.trim());
            let mut bold = false;
            let mut italic = false;
            for (i, t) in toks.iter().enumerate() {
                let tl = t.to_ascii_lowercase();
                let (size_part, lh_part) = match tl.split_once('/') {
                    Some((a, b)) => (a.to_owned(), Some(b.to_owned())),
                    None => (tl.clone(), None),
                };
                if let Some(sz) = font_size(&size_part) {
                    push!(FontSize, sz);
                    push!(FontWeight, Value::Kw(if bold { Kw::Bold } else { Kw::Normal }));
                    push!(FontStyle, Value::Kw(if italic { Kw::Italic } else { Kw::Normal }));
                    let lh = lh_part.and_then(|l| line_height(&l)).unwrap_or(Value::Kw(Kw::Normal));
                    push!(LineHeight, lh);
                    let rest = toks[i + 1..].join(" ");
                    if !rest.is_empty() {
                        push!(FontFamily, font_family(&rest));
                    }
                    return;
                }
                match tl.as_str() {
                    "bold" | "bolder" | "600" | "700" | "800" | "900" => bold = true,
                    "italic" | "oblique" => italic = true,
                    _ => {}
                }
            }
            return;
        }
        "text-align" => match v {
            "left" | "start" | "justify" | "-webkit-left" => kw(Kw::Left),
            "right" | "end" | "-webkit-right" => kw(Kw::Right),
            "center" => kw(Kw::Center),
            "-webkit-center" | "-moz-center" => kw(Kw::WebkitCenter),
            _ => None,
        },
        "line-height" => line_height(v),
        "white-space" | "white-space-collapse" => match v {
            "normal" => kw(Kw::Normal),
            "pre" => kw(Kw::Pre),
            "pre-wrap" | "break-spaces" | "preserve" => kw(Kw::PreWrap),
            "pre-line" => kw(Kw::PreLine),
            "nowrap" => kw(Kw::Nowrap),
            _ => None,
        },
        "text-wrap" | "text-wrap-mode" if v == "nowrap" => kw(Kw::Nowrap),
        "text-decoration" | "text-decoration-line" => {
            let u = v.contains("underline");
            let l = v.contains("line-through");
            match (u, l) {
                (true, true) => kw(Kw::UnderlineLineThrough),
                (true, false) => kw(Kw::Underline),
                (false, true) => kw(Kw::LineThrough),
                _ if v.split_whitespace().any(|t| t == "none") => kw(Kw::None),
                _ => None,
            }
        }
        "text-transform" => match v {
            "uppercase" => kw(Kw::Uppercase),
            "lowercase" => kw(Kw::Lowercase),
            "capitalize" => kw(Kw::Capitalize),
            "none" => kw(Kw::None),
            _ => None,
        },
        "list-style-type" | "list-style" => {
            let mut r = None;
            for t in tokens(v) {
                r = match t {
                    "none" => kw(Kw::None),
                    "disc" => kw(Kw::Disc),
                    "circle" => kw(Kw::Circle),
                    "square" => kw(Kw::Square),
                    "decimal" | "decimal-leading-zero" => kw(Kw::Decimal),
                    "lower-alpha" | "lower-latin" => kw(Kw::LowerAlpha),
                    "upper-alpha" | "upper-latin" => kw(Kw::UpperAlpha),
                    "lower-roman" => kw(Kw::LowerRoman),
                    "upper-roman" => kw(Kw::UpperRoman),
                    _ => continue,
                };
                break;
            }
            r
        }
        "pointer-events" => match v {
            "none" => kw(Kw::None),
            _ => kw(Kw::Visible),
        },
        "visibility" => match v {
            "hidden" | "collapse" => kw(Kw::Hidden),
            "visible" => kw(Kw::Visible),
            _ => None,
        },
        "vertical-align" => match v {
            "top" | "text-top" => kw(Kw::Top),
            "middle" => kw(Kw::Middle),
            "bottom" | "text-bottom" => kw(Kw::Bottom),
            _ => kw(Kw::Baseline),
        },
        "box-sizing" => match v {
            "border-box" => kw(Kw::BorderBox),
            "content-box" => kw(Kw::ContentBox),
            _ => None,
        },
        "border-collapse" => match v {
            "collapse" => kw(Kw::Collapse),
            "separate" => kw(Kw::Separate),
            _ => None,
        },
        "border-spacing" => tokens(v).first().and_then(|t| length(t)),
        "width" | "height" | "min-width" | "min-height" | "max-width" | "max-height" => match v {
            "none" if name.starts_with("max-") => Some(Value::Auto),
            "min-content" | "-webkit-min-content" | "-moz-min-content" => kw(Kw::MinContent),
            "max-content" | "-webkit-max-content" | "-moz-max-content" | "intrinsic" => kw(Kw::MaxContent),
            "fit-content" | "-webkit-fit-content" | "-moz-fit-content" => kw(Kw::FitContent),
            "-webkit-fill-available" | "-moz-available" | "stretch" if name.ends_with("width") => Some(Value::Auto),
            _ => length(v),
        },
        "margin" | "padding" => {
            let vals: Option<Vec<Value>> = tokens(v).into_iter().map(length).collect();
            if let Some(vals) = vals {
                let props = if name == "margin" { MARGIN } else { PADDING };
                push4(out, props, &vals, important);
            }
            return;
        }
        "border-width" | "border-color" | "border-style" => {
            let toks = tokens(v);
            let vals: Option<Vec<Value>> = match name {
                "border-width" => toks.iter().map(|t| border_width(t)).collect(),
                "border-color" => toks.iter().map(|t| color_value(t)).collect(),
                _ => toks.iter().map(|t| is_border_style(t).map(Value::Kw)).collect(),
            };
            if let Some(vals) = vals {
                let props = match name {
                    "border-width" => BWIDTH,
                    "border-color" => BCOLOR,
                    _ => BSTYLE,
                };
                push4(out, props, &vals, important);
            }
            return;
        }
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let sides: &[usize] = match name {
                "border-top" => &[0],
                "border-right" => &[1],
                "border-bottom" => &[2],
                "border-left" => &[3],
                _ => &[0, 1, 2, 3],
            };
            let mut w = Value::Px(3.0);
            let mut c = Value::Color(CURRENT_COLOR);
            let mut s = Kw::None;
            for t in tokens(v) {
                if let Some(st) = is_border_style(t) {
                    s = st;
                } else if let Some(bw) = border_width(t) {
                    w = bw;
                } else if let Some(cv) = color_value(t) {
                    c = cv;
                }
            }
            for &i in sides {
                out.push(Decl { prop: BWIDTH[i], value: w, important });
                out.push(Decl { prop: BCOLOR[i], value: c, important });
                out.push(Decl { prop: BSTYLE[i], value: Value::Kw(s), important });
            }
            return;
        }
        _ => {
            // margin-top, border-left-color, ...
            if let Some(p) = simple_prop(name) {
                let val = match p {
                    MarginTop | MarginRight | MarginBottom | MarginLeft | PaddingTop
                    | PaddingRight | PaddingBottom | PaddingLeft => length(v),
                    BorderTopWidth | BorderRightWidth | BorderBottomWidth | BorderLeftWidth => {
                        border_width(v)
                    }
                    BorderTopColor | BorderRightColor | BorderBottomColor | BorderLeftColor => {
                        color_value(v)
                    }
                    BorderTopStyle | BorderRightStyle | BorderBottomStyle | BorderLeftStyle => {
                        is_border_style(v).map(Value::Kw)
                    }
                    _ => None,
                };
                if let Some(val) = val {
                    push!(p, val);
                }
            }
            return;
        }
    };
    if let (Some(val), Some(p)) = (val, simple_prop(name)) {
        push!(p, val);
    }
}

fn line_height(v: &str) -> Option<Value> {
    if v == "normal" {
        return Some(Value::Kw(Kw::Normal));
    }
    parse_len(v)
}

fn simple_prop(name: &str) -> Option<Prop> {
    Some(match name {
        "filter" | "-webkit-filter" => FilterP,
        "backdrop-filter" | "-webkit-backdrop-filter" => BackdropFilterP,
        "display" => Display,
        "position" => Position,
        "color" => Color,
        "background-color" | "background" => Background,
        "font-size" => FontSize,
        "font-weight" => FontWeight,
        "font-style" => FontStyle,
        "font-family" => FontFamily,
        "text-align" => TextAlign,
        "line-height" => LineHeight,
        "white-space" | "white-space-collapse" | "text-wrap" | "text-wrap-mode" => WhiteSpace,
        "text-decoration" | "text-decoration-line" => TextDecoration,
        "text-transform" => TextTransform,
        "list-style-type" | "list-style" => ListStyleType,
        "visibility" => Visibility,
        "pointer-events" => PointerEvents,
        "vertical-align" => VerticalAlign,
        "box-sizing" => BoxSizing,
        "border-collapse" => BorderCollapse,
        "border-spacing" => BorderSpacing,
        "width" => Width,
        "height" => Height,
        "min-width" => MinWidth,
        "max-width" => MaxWidth,
        "min-height" => MinHeight,
        "max-height" => MaxHeight,
        "margin-top" => MarginTop,
        "margin-right" => MarginRight,
        "margin-bottom" => MarginBottom,
        "margin-left" => MarginLeft,
        "padding-top" => PaddingTop,
        "padding-right" => PaddingRight,
        "padding-bottom" => PaddingBottom,
        "padding-left" => PaddingLeft,
        "border-top-width" => BorderTopWidth,
        "border-right-width" => BorderRightWidth,
        "border-bottom-width" => BorderBottomWidth,
        "border-left-width" => BorderLeftWidth,
        "border-top-color" => BorderTopColor,
        "border-right-color" => BorderRightColor,
        "border-bottom-color" => BorderBottomColor,
        "border-left-color" => BorderLeftColor,
        "border-top-style" => BorderTopStyle,
        "border-right-style" => BorderRightStyle,
        "border-bottom-style" => BorderBottomStyle,
        "border-left-style" => BorderLeftStyle,
        "margin-block-start" => MarginTop,
        "margin-block-end" => MarginBottom,
        "margin-inline-start" => MarginLeft,
        "margin-inline-end" => MarginRight,
        "padding-block-start" => PaddingTop,
        "padding-block-end" => PaddingBottom,
        "padding-inline-start" => PaddingLeft,
        "padding-inline-end" => PaddingRight,
        "top" => InsetTop,
        "right" => InsetRight,
        "bottom" => InsetBottom,
        "left" => InsetLeft,
        "z-index" => ZIndex,
        "float" => Float,
        "clear" => Clear,
        "overflow" | "overflow-x" | "overflow-y" => Overflow,
        "box-shadow" => BoxShadow,
        "opacity" => Opacity,
        "flex-direction" => FlexDirection,
        "flex-wrap" => FlexWrap,
        "justify-content" => JustifyContent,
        "align-items" => AlignItems,
        "align-self" => AlignSelf,
        "align-content" => AlignContent,
        "justify-items" => JustifyItems,
        "justify-self" => JustifySelf,
        "flex-grow" => FlexGrow,
        "flex-shrink" => FlexShrink,
        "flex-basis" => FlexBasis,
        "order" => Order,
        "row-gap" => RowGap,
        "column-gap" => ColumnGap,
        "grid-template-columns" => GridTemplateColumns,
        "grid-template-rows" => GridTemplateRows,
        "grid-auto-rows" => GridAutoRows,
        "background-image" => BackgroundImage,
        "content" => Content,
        "transform" => TransformP,
        "translate" => TranslateP,
        "rotate" => RotateP,
        "scale" => ScaleP,
        "background-repeat" => BgRepeat,
        "object-fit" => ObjectFit,
        _ => return None,
    })
}

/// Parse the inside of a `{ ... }` block (or a style="" attribute).
pub fn parse_declarations(body: &str, out: &mut Vec<Decl>, raws: &mut Vec<Raw>) {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let end = find_top(b, i, b";{").unwrap_or(b.len());
        if end < b.len() && b[end] == b'{' {
            // Nested rule (CSS nesting) — not supported; skip it.
            i = match_brace(b, end) + 1;
            continue;
        }
        let decl = &body[i..end];
        i = end + 1;
        let Some(colon) = decl.find(':') else { continue };
        let name = decl[..colon].trim();
        if name.is_empty() {
            continue;
        }
        let mut value = decl[colon + 1..].trim();
        let mut important = false;
        if let Some(p) = value.rfind('!') {
            if value[p + 1..].trim().eq_ignore_ascii_case("important") {
                important = true;
                value = value[..p].trim();
            }
        }
        if name.starts_with("--") {
            raws.push((name.into(), value.into()));
            out.push(Decl { prop: Custom, value: Value::Raw(raws.len() as u32 - 1), important });
            continue;
        }
        let name = name.to_ascii_lowercase();
        if value.contains("var(") {
            // Resolved per element once custom property values are known.
            let n = name.strip_prefix("-webkit-").unwrap_or(&name);
            raws.push((n.into(), value.into()));
            out.push(Decl { prop: Unresolved, value: Value::Raw(raws.len() as u32 - 1), important });
            continue;
        }
        let name = name.strip_prefix("-webkit-").unwrap_or(&name);
        parse_property(name, value, important, out);
    }
}

/// Substitute `var(--x[, fallback])` references using `lookup`.
pub fn substitute_vars(v: &str, lookup: &dyn Fn(&str) -> Option<String>, depth: u32) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(p) = rest.find("var(") {
        out.push_str(&rest[..p]);
        let close = find_top(rest.as_bytes(), p + 4, b")")?;
        let inner = &rest[p + 4..close];
        let (name, fallback) = match find_top(inner.as_bytes(), 0, b",") {
            Some(c) => (inner[..c].trim(), Some(inner[c + 1..].trim())),
            None => (inner.trim(), None),
        };
        let val = lookup(name).or_else(|| fallback.map(str::to_owned))?;
        out.push_str(&substitute_vars(&val, lookup, depth + 1)?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

// -------------------------------------------------------------------- calc

/// Linear combination of units; `num` is set for plain numbers.
#[derive(Clone, Copy)]
struct Lin {
    c: [f32; 6],
    num: Option<f32>,
}

struct CalcParser<'a> {
    s: &'a str,
    i: usize,
}

impl CalcParser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s.as_bytes()[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> u8 {
        *self.s.as_bytes().get(self.i).unwrap_or(&0)
    }

    fn expr(&mut self) -> Option<Lin> {
        let mut a = self.term()?;
        loop {
            self.ws();
            let op = self.peek();
            if op != b'+' && op != b'-' {
                return Some(a);
            }
            self.i += 1;
            let b = self.term()?;
            let sign = if op == b'-' { -1.0 } else { 1.0 };
            a = match (a.num, b.num) {
                (Some(x), Some(y)) => Lin { c: [0.0; 6], num: Some(x + sign * y) },
                (None, None) => {
                    let mut c = a.c;
                    for k in 0..6 {
                        c[k] += sign * b.c[k];
                    }
                    Lin { c, num: None }
                }
                (Some(x), None) if x == 0.0 => Lin { c: b.c.map(|v| v * sign), num: None },
                (None, Some(y)) if y == 0.0 => a,
                _ => return None,
            };
        }
    }

    fn term(&mut self) -> Option<Lin> {
        let mut a = self.factor()?;
        loop {
            self.ws();
            let op = self.peek();
            if op != b'*' && op != b'/' {
                return Some(a);
            }
            self.i += 1;
            let b = self.factor()?;
            a = match (op, a.num, b.num) {
                (b'*', Some(x), Some(y)) => Lin { c: [0.0; 6], num: Some(x * y) },
                (b'*', Some(x), None) => Lin { c: b.c.map(|v| v * x), num: None },
                (b'*', None, Some(y)) => Lin { c: a.c.map(|v| v * y), num: None },
                (b'/', _, Some(y)) if y != 0.0 => match a.num {
                    Some(x) => Lin { c: [0.0; 6], num: Some(x / y) },
                    None => Lin { c: a.c.map(|v| v / y), num: None },
                },
                _ => return None,
            };
        }
    }

    fn factor(&mut self) -> Option<Lin> {
        self.ws();
        let rest = &self.s[self.i..];
        let paren = rest.starts_with('(');
        if paren || rest.len() > 5 && rest[..5].eq_ignore_ascii_case("calc(") {
            self.i += if paren { 1 } else { 5 };
            let v = self.expr()?;
            self.ws();
            if self.peek() != b')' {
                return None;
            }
            self.i += 1;
            return Some(v);
        }
        let start = self.i;
        let b = self.s.as_bytes();
        if self.i < b.len() && (b[self.i] == b'-' || b[self.i] == b'+') {
            self.i += 1;
        }
        while self.i < b.len() && (b[self.i].is_ascii_alphanumeric() || b[self.i] == b'.' || b[self.i] == b'%') {
            self.i += 1;
        }
        let mut c = [0.0f32; 6];
        match parse_len(&self.s[start..self.i])? {
            Value::Num(n) => return Some(Lin { c, num: Some(n) }),
            Value::Pct(v) => c[0] = v,
            Value::Px(v) => c[1] = v,
            Value::Em(v) => c[2] = v,
            Value::Rem(v) => c[3] = v,
            Value::Vw(v) => c[4] = v,
            Value::Vh(v) => c[5] = v,
            _ => return None,
        }
        Some(Lin { c, num: None })
    }
}

fn parse_calc(s: &str) -> Option<Value> {
    let mut p = CalcParser { s, i: 0 };
    let v = p.expr()?;
    p.ws();
    if p.i != s.len() {
        return None;
    }
    Some(match v.num {
        Some(n) => Value::Num(n),
        None => Value::Calc(v.c),
    })
}

// ------------------------------------------------------- layout extensions

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Shadow {
    pub x: f32,
    pub y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: u32,
    pub inset: bool,
}

/// One side of a grid track size.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Breadth {
    Len(Value),
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Track {
    pub min: Breadth,
    pub max: Breadth,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackList {
    pub tracks: Vec<Track>,
    /// repeat(auto-fill | auto-fit, ...): (insert position, repeated tracks)
    pub auto_repeat: Option<(usize, Vec<Track>)>,
}

#[derive(Clone, Debug)]
pub enum Expr {
    /// Coefficients of [%, px, em, rem, vw, vh].
    Lin([f32; 6]),
    Min(Vec<Expr>),
    Max(Vec<Expr>),
}

/// Parsed values that don't fit in a `Value` live in these tables, keyed by
/// their source text so re-parsing a sheet (on resize) doesn't grow them.
#[derive(Default)]
struct Tables {
    tracks: Vec<Rc<TrackList>>,
    track_keys: FxHashMap<String, u32>,
    exprs: Vec<Rc<Expr>>,
    expr_keys: FxHashMap<String, u32>,
}

thread_local! {
    static TABLES: RefCell<Tables> = RefCell::new(Tables::default());
}

pub fn track_list(i: u32) -> Rc<TrackList> {
    TABLES.with(|t| t.borrow().tracks[i as usize].clone())
}

pub fn expr(i: u32) -> Rc<Expr> {
    TABLES.with(|t| t.borrow().exprs[i as usize].clone())
}

fn store_expr(src: &str) -> Option<Value> {
    let key = src.to_ascii_lowercase();
    if let Some(i) = TABLES.with(|t| t.borrow().expr_keys.get(&key).copied()) {
        return Some(Value::Expr(i));
    }
    let e = parse_minmax(&key)?;
    TABLES.with(|t| {
        let mut t = t.borrow_mut();
        let i = t.exprs.len() as u32;
        t.exprs.push(Rc::new(e));
        t.expr_keys.insert(key, i);
        Some(Value::Expr(i))
    })
}

fn parse_minmax(t: &str) -> Option<Expr> {
    let t = t.trim();
    let (kind, inner) = if let Some(r) = t.strip_prefix("min(") {
        (0, r)
    } else if let Some(r) = t.strip_prefix("max(") {
        (1, r)
    } else if let Some(r) = t.strip_prefix("clamp(") {
        (2, r)
    } else {
        // Arguments are math expressions even without calc().
        let v = if t.starts_with("calc(") && t.ends_with(')') { parse_calc(&t[5..t.len() - 1])? } else { parse_len(t).or_else(|| parse_calc(t))? };
        let mut c = [0.0f32; 6];
        match v {
            Value::Calc(x) => c = x,
            Value::Pct(p) => c[0] = p,
            Value::Px(p) => c[1] = p,
            Value::Em(p) => c[2] = p,
            Value::Rem(p) => c[3] = p,
            Value::Vw(p) => c[4] = p,
            Value::Vh(p) => c[5] = p,
            Value::Num(n) if n == 0.0 => {}
            _ => return None,
        }
        return Some(Expr::Lin(c));
    };
    let inner = inner.strip_suffix(')')?;
    let args: Vec<Expr> = split_top(inner, b',').into_iter().map(parse_minmax).collect::<Option<_>>()?;
    Some(match kind {
        0 => Expr::Min(args),
        1 => Expr::Max(args),
        _ => {
            let [lo, v, hi]: [Expr; 3] = args.try_into().ok()?;
            Expr::Max(vec![lo, Expr::Min(vec![v, hi])])
        }
    })
}

fn breadth(t: &str) -> Option<Breadth> {
    Some(match t {
        "auto" => Breadth::Auto,
        "min-content" => Breadth::MinContent,
        "max-content" => Breadth::MaxContent,
        _ => {
            if let Some(n) = t.strip_suffix("fr") {
                Breadth::Fr(n.parse().ok()?)
            } else {
                Breadth::Len(length(t)?)
            }
        }
    })
}

fn parse_track(t: &str) -> Option<Track> {
    if let Some(inner) = t.strip_prefix("minmax(").and_then(|r| r.strip_suffix(')')) {
        let parts = split_top(inner, b',');
        if parts.len() != 2 {
            return None;
        }
        let min = breadth(parts[0].trim())?;
        let min = if let Breadth::Fr(_) = min { Breadth::Auto } else { min };
        return Some(Track { min, max: breadth(parts[1].trim())? });
    }
    if let Some(inner) = t.strip_prefix("fit-content(").and_then(|r| r.strip_suffix(')')) {
        return Some(Track { min: Breadth::Auto, max: Breadth::Len(length(inner.trim())?) });
    }
    let b = breadth(t)?;
    Some(match b {
        Breadth::Fr(_) => Track { min: Breadth::Auto, max: b },
        _ => Track { min: b, max: b },
    })
}

fn parse_tracks(v: &str, out: &mut TrackList, allow_auto: bool) -> Option<()> {
    // Drop [line-name] groups.
    let mut clean = String::with_capacity(v.len());
    let mut depth = 0;
    for ch in v.chars() {
        match ch {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => clean.push(ch),
            _ => {}
        }
    }
    for tok in tokens_ws(&clean) {
        if let Some(inner) = tok.strip_prefix("repeat(").and_then(|r| r.strip_suffix(')')) {
            let c = find_top(inner.as_bytes(), 0, b",")?;
            let count = inner[..c].trim();
            let mut rep = TrackList::default();
            parse_tracks(&inner[c + 1..], &mut rep, false)?;
            if count == "auto-fill" || count == "auto-fit" {
                if !allow_auto || out.auto_repeat.is_some() {
                    return None;
                }
                out.auto_repeat = Some((out.tracks.len(), rep.tracks));
            } else {
                let n: usize = count.parse().ok()?;
                for _ in 0..n.min(1000) {
                    out.tracks.extend_from_slice(&rep.tracks);
                }
            }
        } else {
            out.tracks.push(parse_track(tok)?);
        }
    }
    Some(())
}

/// Whitespace split that keeps parenthesised groups (with commas) together.
fn tokens_ws(v: &str) -> Vec<&str> {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && is_ws(b[i]) {
            i += 1;
        }
        let s = i;
        let mut depth = 0;
        while i < b.len() && (depth > 0 || !is_ws(b[i])) {
            match b[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if i > s {
            out.push(&v[s..i]);
        }
    }
    out
}

fn store_tracks(v: &str) -> Option<Value> {
    if v == "none" {
        return Some(Value::Kw(Kw::None));
    }
    if let Some(i) = TABLES.with(|t| t.borrow().track_keys.get(v).copied()) {
        return Some(Value::Tracks(i));
    }
    let mut tl = TrackList::default();
    parse_tracks(v, &mut tl, true)?;
    TABLES.with(|t| {
        let mut t = t.borrow_mut();
        let i = t.tracks.len() as u32;
        t.tracks.push(Rc::new(tl));
        t.track_keys.insert(v.to_owned(), i);
        Some(Value::Tracks(i))
    })
}

/// Named grid areas: rows × cols and each name's [row0, col0, row1, col1).
#[derive(Debug, Default, PartialEq)]
pub struct Areas {
    pub rows: usize,
    pub cols: usize,
    pub map: Vec<(u32, [usize; 4])>,
}

thread_local! {
    static AREAS: RefCell<(Vec<Rc<Areas>>, FxHashMap<String, u32>)> = RefCell::new(Default::default());
}

pub fn areas(i: u32) -> Rc<Areas> {
    AREAS.with(|t| t.borrow().0[i as usize].clone())
}

fn store_areas(v: &str) -> Option<Value> {
    if v.eq_ignore_ascii_case("none") {
        return Some(Value::Kw(Kw::None));
    }
    if let Some(&i) = AREAS.with(|t| t.borrow().1.get(v).copied()).as_ref() {
        return Some(Value::Areas(i));
    }
    let mut rows: Vec<Vec<&str>> = Vec::new();
    let mut rest = v;
    while let Some(q0) = rest.find(['"', '\'']) {
        let q = rest.as_bytes()[q0] as char;
        let end = rest[q0 + 1..].find(q)? + q0 + 1;
        rows.push(rest[q0 + 1..end].split_whitespace().collect());
        rest = &rest[end + 1..];
    }
    let cols = rows.iter().map(|r| r.len()).max()?;
    let mut a = Areas { rows: rows.len(), cols, map: Vec::new() };
    for (r, row) in rows.iter().enumerate() {
        for (c, name) in row.iter().enumerate() {
            if name.chars().all(|ch| ch == '.') {
                continue;
            }
            let id = crate::atom::intern(name).0;
            match a.map.iter_mut().find(|(n, _)| *n == id) {
                Some((_, b)) => {
                    b[0] = b[0].min(r);
                    b[1] = b[1].min(c);
                    b[2] = b[2].max(r + 1);
                    b[3] = b[3].max(c + 1);
                }
                None => a.map.push((id, [r, c, r + 1, c + 1])),
            }
        }
    }
    Some(AREAS.with(|t| {
        let mut t = t.borrow_mut();
        let i = t.0.len() as u32;
        t.0.push(Rc::new(a));
        t.1.insert(v.to_owned(), i);
        Value::Areas(i)
    }))
}

fn grid_line(t: &str) -> Value {
    let toks: Vec<&str> = t.split_whitespace().collect();
    match toks.as_slice() {
        ["span", n] => n.parse::<i16>().map_or(Value::Auto, |n| Value::Span(n.max(1))),
        [n] => n.parse::<i16>().map_or(Value::Auto, |n| Value::Num(n as f32)),
        _ => Value::Auto,
    }
}

fn shadow(v: &str) -> Option<Value> {
    if v == "none" {
        return Some(Value::Kw(Kw::None));
    }
    let first = split_top(v, b',').into_iter().next()?;
    let mut lens = Vec::new();
    let mut color = CURRENT_COLOR;
    let mut inset = false;
    for t in tokens_ws(first) {
        if t == "inset" {
            inset = true;
        } else if let Some(l) = length(t) {
            lens.push(match l {
                Value::Px(p) => p,
                Value::Em(e) | Value::Rem(e) => e * 16.0,
                _ => 0.0,
            });
        } else if let Some(Value::Color(c)) = color_value(t) {
            color = c;
        }
    }
    if lens.len() < 2 {
        return None;
    }
    Some(Value::Shadow(Shadow {
        x: lens[0],
        y: lens[1],
        blur: lens.get(2).copied().unwrap_or(0.0).max(0.0),
        spread: lens.get(3).copied().unwrap_or(0.0),
        color,
        inset,
    }))
}

fn align_kw(t: &str) -> Option<Value> {
    Some(match t {
        "auto" => Value::Auto,
        "normal" | "stretch" => Value::Kw(Kw::Stretch),
        "flex-start" | "start" | "self-start" | "left" => Value::Kw(Kw::Start),
        "flex-end" | "end" | "self-end" | "right" => Value::Kw(Kw::End),
        "center" => Value::Kw(Kw::Center),
        "baseline" | "first" | "last" => Value::Kw(Kw::Baseline),
        "space-between" => Value::Kw(Kw::SpaceBetween),
        "space-around" => Value::Kw(Kw::SpaceAround),
        "space-evenly" => Value::Kw(Kw::SpaceEvenly),
        _ => return None,
    })
}

/// Properties added for positioning, floats, effects, flexbox and grid.
/// Returns false if `name` isn't one of them.
fn parse_more(name: &str, v: &str, orig: &str, important: bool, out: &mut Vec<Decl>) -> bool {
    macro_rules! push {
        ($p:expr, $v:expr) => {
            out.push(Decl { prop: $p, value: $v, important })
        };
    }
    let kw = Value::Kw;
    match name {
        "display" => {
            let k = match v {
                "flex" | "-webkit-flex" | "-ms-flexbox" | "block flex" => Kw::Flex,
                // Today -webkit-box is mostly the -webkit-line-clamp idiom.
                "-webkit-box" => Kw::FlowRoot,
                "inline-flex" | "-webkit-inline-flex" | "-ms-inline-flexbox" | "-webkit-inline-box" | "inline flex" => Kw::InlineFlex,
                "grid" | "-ms-grid" | "block grid" => Kw::Grid,
                "inline-grid" | "-ms-inline-grid" | "inline grid" => Kw::InlineGrid,
                "flow-root" | "block flow-root" => Kw::FlowRoot,
                _ => return false,
            };
            push!(Display, kw(k));
        }
        "position" => {
            let k = match v {
                "static" => Kw::Static,
                "relative" => Kw::Relative,
                "absolute" => Kw::Absolute,
                "fixed" => Kw::Fixed,
                "sticky" | "-webkit-sticky" => Kw::Sticky,
                _ => return true,
            };
            push!(Position, kw(k));
        }
        "top" | "right" | "bottom" | "left" => {
            if let Some(l) = length(v) {
                push!(simple_prop(name).unwrap(), l);
            }
        }
        "inset" => {
            let vals: Option<Vec<Value>> = tokens_ws(v).into_iter().map(length).collect();
            if let Some(vals) = vals {
                push4(out, INSET, &vals, important);
            }
        }
        "z-index" => {
            if v == "auto" {
                push!(ZIndex, Value::Auto);
            } else if let Ok(n) = v.parse::<i32>() {
                push!(ZIndex, Value::Num(n as f32));
            }
        }
        "float" => {
            let k = match v {
                "left" | "inline-start" => Kw::Left,
                "right" | "inline-end" => Kw::Right,
                "none" => Kw::None,
                _ => return true,
            };
            push!(Float, kw(k));
        }
        "clear" => {
            let k = match v {
                "left" | "inline-start" => Kw::Left,
                "right" | "inline-end" => Kw::Right,
                "both" => Kw::Both,
                "none" => Kw::None,
                _ => return true,
            };
            push!(Clear, kw(k));
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            let kw_of = |t: &str| match t {
                "hidden" | "clip" => Some(Kw::Hidden),
                "auto" | "scroll" | "overlay" => Some(Kw::Scroll),
                "visible" => Some(Kw::Visible),
                _ => None,
            };
            let toks: Vec<&str> = v.split_whitespace().collect();
            let Some(x) = toks.first().and_then(|t| kw_of(t)) else { return true };
            let y = toks.get(1).and_then(|t| kw_of(t)).unwrap_or(x);
            match name {
                "overflow-x" => push!(OverflowX, kw(x)),
                "overflow-y" => push!(OverflowY, kw(x)),
                _ => {
                    push!(OverflowX, kw(x));
                    push!(OverflowY, kw(y));
                }
            }
        }
        "border-radius" | "-moz-border-radius" => {
            let first = v.split('/').next().unwrap_or("");
            let vals: Option<Vec<Value>> = tokens_ws(first).into_iter().map(length).collect();
            if let Some(vals) = vals {
                push4(out, RADIUS, &vals, important);
            }
        }
        "border-top-left-radius" | "border-top-right-radius" | "border-bottom-right-radius" | "border-bottom-left-radius"
        | "border-start-start-radius" | "border-start-end-radius" | "border-end-end-radius" | "border-end-start-radius" => {
            let i = match name {
                "border-top-left-radius" | "border-start-start-radius" => 0,
                "border-top-right-radius" | "border-start-end-radius" => 1,
                "border-bottom-right-radius" | "border-end-end-radius" => 2,
                _ => 3,
            };
            if let Some(l) = tokens_ws(v).first().and_then(|t| length(t)) {
                push!(RADIUS[i], l);
            }
        }
        "box-shadow" => {
            if let Some(s) = shadow(v) {
                push!(BoxShadow, s);
            }
        }
        "opacity" => {
            let n = if let Some(p) = v.strip_suffix('%') { p.parse::<f32>().ok().map(|p| p / 100.0) } else { v.parse::<f32>().ok() };
            if let Some(n) = n {
                push!(Opacity, Value::Num(n.clamp(0.0, 1.0)));
            }
        }
        "flex-direction" | "flex-wrap" | "flex-flow" => {
            for t in v.split_whitespace() {
                match t {
                    "row" => push!(FlexDirection, kw(Kw::Row)),
                    "row-reverse" => push!(FlexDirection, kw(Kw::RowReverse)),
                    "column" => push!(FlexDirection, kw(Kw::Column)),
                    "column-reverse" => push!(FlexDirection, kw(Kw::ColumnReverse)),
                    "wrap" | "wrap-reverse" => push!(FlexWrap, kw(Kw::Wrap)),
                    "nowrap" => push!(FlexWrap, kw(Kw::Nowrap)),
                    _ => {}
                }
            }
        }
        "flex" => {
            let (g, s, b) = match v {
                "none" => (0.0, 0.0, Value::Auto),
                "auto" => (1.0, 1.0, Value::Auto),
                _ => {
                    let mut nums = Vec::new();
                    let mut basis = None;
                    for t in tokens_ws(v) {
                        if let Ok(n) = t.parse::<f32>() {
                            nums.push(n);
                        } else if t == "auto" || t == "content" {
                            basis = Some(Value::Auto);
                        } else if let Some(l) = length(t) {
                            basis = Some(l);
                        }
                    }
                    let g = nums.first().copied().unwrap_or(1.0);
                    let s = nums.get(1).copied().unwrap_or(1.0);
                    // A third unitless number is a zero basis.
                    (g, s, basis.unwrap_or(if nums.is_empty() { Value::Auto } else { Value::Pct(0.0) }))
                }
            };
            push!(FlexGrow, Value::Num(g));
            push!(FlexShrink, Value::Num(s));
            push!(FlexBasis, b);
        }
        "flex-grow" | "flex-shrink" | "order" => {
            if let Ok(n) = v.parse::<f32>() {
                push!(simple_prop(name).unwrap(), Value::Num(n));
            }
        }
        "flex-basis" => {
            let b = if v == "auto" || v == "content" { Some(Value::Auto) } else { length(v) };
            if let Some(b) = b {
                push!(FlexBasis, b);
            }
        }
        "justify-content" | "align-items" | "align-self" | "align-content" | "justify-items" | "justify-self" => {
            let t = v.split_whitespace().last().unwrap_or("");
            if let Some(a) = align_kw(t) {
                push!(simple_prop(name).unwrap(), a);
            }
        }
        "place-items" | "place-content" | "place-self" => {
            let toks: Vec<&str> = v.split_whitespace().collect();
            let (a, j) = (toks.first().copied().unwrap_or(""), toks.get(1).or(toks.first()).copied().unwrap_or(""));
            let (pa, pj) = match name {
                "place-items" => (AlignItems, JustifyItems),
                "place-content" => (AlignContent, JustifyContent),
                _ => (AlignSelf, JustifySelf),
            };
            if let (Some(a), Some(j)) = (align_kw(a), align_kw(j)) {
                push!(pa, a);
                push!(pj, j);
            }
        }
        "gap" | "grid-gap" | "row-gap" | "grid-row-gap" | "column-gap" | "grid-column-gap" => {
            let gap = |t: &str| if t == "normal" { Some(Value::Px(0.0)) } else { length(t) };
            let toks = tokens_ws(v);
            match name {
                "gap" | "grid-gap" => {
                    if let Some(r) = toks.first().and_then(|t| gap(t)) {
                        let c = toks.get(1).and_then(|t| gap(t)).unwrap_or(r);
                        push!(RowGap, r);
                        push!(ColumnGap, c);
                    }
                }
                "row-gap" | "grid-row-gap" => {
                    if let Some(r) = toks.first().and_then(|t| gap(t)) {
                        push!(RowGap, r);
                    }
                }
                _ => {
                    if let Some(c) = toks.first().and_then(|t| gap(t)) {
                        push!(ColumnGap, c);
                    }
                }
            }
        }
        "grid-template-columns" | "grid-template-rows" | "grid-auto-rows" => {
            if let Some(t) = store_tracks(v) {
                push!(simple_prop(name).unwrap(), t);
            }
        }
        "grid-template" => {
            if let Some((r, c)) = v.split_once('/') {
                if let (Some(r), Some(c)) = (store_tracks(r.trim()), store_tracks(c.trim())) {
                    push!(GridTemplateRows, r);
                    push!(GridTemplateColumns, c);
                }
            }
        }
        "grid-column" | "grid-row" => {
            let (s, e) = match v.split_once('/') {
                Some((a, b)) => (grid_line(a), grid_line(b)),
                None => (grid_line(v), Value::Auto),
            };
            let (ps, pe) = if name == "grid-column" { (GridColumnStart, GridColumnEnd) } else { (GridRowStart, GridRowEnd) };
            push!(ps, s);
            push!(pe, e);
        }
        "background" => background(orig, important, out),
        "background-image" => {
            let first = split_top(orig, b',').into_iter().next().unwrap_or("").trim();
            push!(BackgroundImage, if first.eq_ignore_ascii_case("none") { Value::Kw(Kw::None) } else { image_value(first).unwrap_or(Value::Kw(Kw::None)) });
        }
        "background-size" => {
            let first = split_top(v, b',').into_iter().next().unwrap_or("");
            bg_size(first, important, out);
        }
        "background-position" => {
            let first = split_top(v, b',').into_iter().next().unwrap_or("");
            bg_position(&tokens_ws(first), important, out);
        }
        "background-repeat" => {
            let first = split_top(v, b',').into_iter().next().unwrap_or("");
            if let Some(r) = bg_repeat(&tokens_ws(first)) {
                push!(BgRepeat, r);
            }
        }
        "object-fit" => {
            let k = match v {
                "contain" => Kw::Contain,
                "cover" => Kw::Cover,
                "none" => Kw::None,
                "scale-down" => Kw::ScaleDown,
                _ => Kw::Fill,
            };
            push!(ObjectFit, Value::Kw(k));
        }
        "grid-column-start" => push!(GridColumnStart, grid_line(v)),
        "grid-column-end" => push!(GridColumnEnd, grid_line(v)),
        "grid-row-start" => push!(GridRowStart, grid_line(v)),
        "grid-row-end" => push!(GridRowEnd, grid_line(v)),
        "grid-template-areas" => {
            if let Some(a) = store_areas(orig) {
                push!(GridTemplateAreas, a);
            }
        }
        "grid-area" => {
            let parts: Vec<Value> = v.split('/').map(grid_line).collect();
            let first = orig.split('/').next().unwrap_or("").trim();
            if parts.len() == 1 && first.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '-') && first != "auto" && !first.starts_with("span") {
                push!(GridArea, Value::Name(crate::atom::intern(first).0));
            } else if parts.len() >= 2 {
                push!(GridRowStart, parts[0]);
                push!(GridColumnStart, parts[1]);
                push!(GridRowEnd, parts.get(2).copied().unwrap_or(Value::Auto));
                push!(GridColumnEnd, parts.get(3).copied().unwrap_or(Value::Auto));
            }
        }
        _ => return false,
    }
    true
}

// ------------------------------------------------------------- stylesheet

/// Parse a stylesheet; `url()`s resolve against `base`.
/// Split CSS source into its top-level rules (CSSOM `cssRules`).
pub fn split_rules(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut depth, mut i) = (0, 0usize, 0);
    let mut push = |s: &str| {
        let t = s.trim();
        if !t.is_empty() {
            out.push(t.to_owned());
        }
    };
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = src[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
                if depth == 0 && src[start..i].trim().is_empty() {
                    start = end;
                }
                i = end;
                continue;
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    push(&src[start..=i]);
                    start = i + 1;
                }
            }
            b';' if depth == 0 => {
                push(&src[start..=i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if depth > 0 {
        push(&src[start.min(src.len())..]);
    }
    out
}

pub fn parse_stylesheet(src: &str, media: &Media, base: &str) -> Stylesheet {
    let saved = BASE.with(|b| std::mem::replace(&mut *b.borrow_mut(), base.to_owned()));
    let mut sheet = Stylesheet::default();
    let s = strip_comments(src);
    parse_rules(&s, &mut sheet, media);
    BASE.with(|b| *b.borrow_mut() = saved);
    sheet
}

// ------------------------------------------------------------ backgrounds

thread_local! {
    /// Base URL for url() in the declarations being parsed.
    static BASE: RefCell<String> = const { RefCell::new(String::new()) };
    static IMAGES: RefCell<(Vec<BgImage>, FxHashMap<String, u32>)> = RefCell::new(Default::default());
}

/// Set the base URL used for url() in inline style attributes.
pub fn set_base(base: &str) {
    BASE.with(|b| *b.borrow_mut() = base.to_owned());
}

#[derive(Clone, Debug)]
pub struct Stop {
    pub color: u32,
    /// Position as a fraction of the gradient line, if given in %.
    pub frac: Option<f32>,
    /// Position in px, if given as a length.
    pub px: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct Gradient {
    pub radial: bool,
    pub repeating: bool,
    /// CSS angle in degrees (0 = towards the top, 90 = towards the right).
    pub angle: f32,
    pub stops: Vec<Stop>,
}

#[derive(Clone, Debug)]
pub enum BgImage {
    Url(Rc<str>),
    Gradient(Rc<Gradient>),
}

/// One part of a `content` value.
#[derive(Clone, Debug)]
pub enum ContentItem {
    Str(Rc<str>),
    Attr(Atom),
    OpenQuote,
    CloseQuote,
    /// counter() / counters(): rendered empty for now.
    Counter,
    /// url(): images aren't rendered yet.
    Url,
}

thread_local! {
    static CONTENTS: RefCell<Vec<Rc<[ContentItem]>>> = const { RefCell::new(Vec::new()) };
}

/// One transform function (lengths unresolved; angles in degrees).
#[derive(Clone, Copy, Debug)]
pub enum TfOp {
    Translate(Value, Value),
    Scale(f32, f32),
    Rotate(f32),
    Skew(f32, f32),
    Matrix([f32; 6]),
}

thread_local! {
    static TRANSFORMS: RefCell<Vec<Rc<[TfOp]>>> = const { RefCell::new(Vec::new()) };
}

thread_local! {
    static NAME_LISTS: RefCell<Vec<Rc<[Rc<str>]>>> = const { RefCell::new(Vec::new()) };
}

pub fn name_list(i: u32) -> Rc<[Rc<str>]> {
    NAME_LISTS.with(|t| t.borrow()[i as usize].clone())
}

pub fn transform_ops(i: u32) -> Rc<[TfOp]> {
    TRANSFORMS.with(|t| t.borrow()[i as usize].clone())
}

fn store_transform(ops: Vec<TfOp>) -> Value {
    TRANSFORMS.with(|t| {
        let mut t = t.borrow_mut();
        t.push(ops.into());
        Value::Transform(t.len() as u32 - 1)
    })
}

/// An angle in degrees.
fn angle_deg(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("deg") {
        v.parse().ok()
    } else if let Some(v) = s.strip_suffix("grad") {
        v.parse::<f32>().ok().map(|g| g * 0.9)
    } else if let Some(v) = s.strip_suffix("rad") {
        v.parse::<f32>().ok().map(f32::to_degrees)
    } else if let Some(v) = s.strip_suffix("turn") {
        v.parse::<f32>().ok().map(|t| t * 360.0)
    } else if s == "0" {
        Some(0.0)
    } else {
        None
    }
}

fn scale_num(s: &str) -> Option<f32> {
    match s.trim().strip_suffix('%') {
        Some(p) => p.parse::<f32>().ok().map(|v| v / 100.0),
        None => s.trim().parse().ok(),
    }
}

/// `transform` (a list of functions) or `none`.
fn transform_value(v: &str) -> Option<Value> {
    let v = v.trim();
    if v == "none" {
        return Some(Value::Kw(Kw::None));
    }
    let mut ops = Vec::new();
    let mut rest = v;
    while !rest.trim().is_empty() {
        let r = rest.trim_start();
        let open = r.find('(')?;
        let close = open + r[open..].find(')')?;
        let name = r[..open].trim();
        let args: Vec<&str> = r[open + 1..close].split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
        rest = &r[close + 1..];
        let len = |s: &str| length(s).or_else(|| (s == "0").then_some(Value::Px(0.0)));
        let a = |i: usize| args.get(i).copied();
        ops.push(match name {
            "translate" | "translate3d" => TfOp::Translate(len(a(0)?)?, a(1).map_or(Some(Value::Px(0.0)), len)?),
            "translatex" => TfOp::Translate(len(a(0)?)?, Value::Px(0.0)),
            "translatey" => TfOp::Translate(Value::Px(0.0), len(a(0)?)?),
            "translatez" | "perspective" | "rotatex" | "rotatey" => continue,
            "scale" | "scale3d" => {
                let x = scale_num(a(0)?)?;
                TfOp::Scale(x, a(1).map_or(Some(x), scale_num)?)
            }
            "scalex" => TfOp::Scale(scale_num(a(0)?)?, 1.0),
            "scaley" => TfOp::Scale(1.0, scale_num(a(0)?)?),
            "scalez" => continue,
            "rotate" | "rotatez" => TfOp::Rotate(angle_deg(a(0)?)?),
            "rotate3d" => {
                // Only rotations about the z axis are 2D.
                if a(2).and_then(scale_num) == Some(0.0) || a(0).and_then(scale_num) != Some(0.0) || a(1).and_then(scale_num) != Some(0.0) {
                    continue;
                }
                TfOp::Rotate(angle_deg(a(3)?)? * a(2).and_then(scale_num).unwrap_or(1.0).signum())
            }
            "skew" => TfOp::Skew(angle_deg(a(0)?)?, a(1).map_or(Some(0.0), angle_deg)?),
            "skewx" => TfOp::Skew(angle_deg(a(0)?)?, 0.0),
            "skewy" => TfOp::Skew(0.0, angle_deg(a(0)?)?),
            "matrix" => {
                let m: Vec<f32> = args.iter().filter_map(|s| s.parse().ok()).collect();
                if m.len() != 6 {
                    return None;
                }
                TfOp::Matrix([m[0], m[1], m[2], m[3], m[4], m[5]])
            }
            "matrix3d" => {
                let m: Vec<f32> = args.iter().filter_map(|s| s.parse().ok()).collect();
                if m.len() != 16 {
                    return None;
                }
                TfOp::Matrix([m[0], m[1], m[4], m[5], m[12], m[13]])
            }
            _ => return None,
        });
    }
    Some(store_transform(ops))
}

/// One filter function (lengths in px, amounts as fractions, angles in
/// degrees).
#[derive(Clone, Copy, Debug)]
pub enum FilterOp {
    Blur(f32),
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    HueRotate(f32),
    Invert(f32),
    Opacity(f32),
    Saturate(f32),
    Sepia(f32),
    DropShadow(Shadow),
}

thread_local! {
    static FILTERS: RefCell<Vec<Rc<[FilterOp]>>> = const { RefCell::new(Vec::new()) };
}

pub fn filter_ops(i: u32) -> Rc<[FilterOp]> {
    FILTERS.with(|t| t.borrow()[i as usize].clone())
}

/// A `filter` / `backdrop-filter` value: `none` or a list of filter
/// functions (SVG `url()` references are skipped).
fn filter_value(v: &str) -> Option<Value> {
    let v = v.trim();
    if v == "none" {
        return Some(Value::Kw(Kw::None));
    }
    let px = |s: &str| match length(s) {
        Some(Value::Px(p)) => Some(p),
        Some(Value::Em(e) | Value::Rem(e)) => Some(e * 16.0),
        _ => (s == "0").then_some(0.0),
    };
    // A number or percentage amount; `default` when omitted.
    let amount = |s: Option<&str>, default: f32| -> Option<f32> {
        match s {
            None => Some(default),
            Some(s) => match s.strip_suffix('%') {
                Some(p) => p.trim().parse::<f32>().ok().map(|p| p / 100.0),
                None => s.parse().ok(),
            },
        }
    };
    let mut ops = Vec::new();
    let mut rest = v;
    while !rest.trim().is_empty() {
        let r = rest.trim_start();
        let open = r.find('(')?;
        // The matching parenthesis (drop-shadow colours nest).
        let mut depth = 0;
        let mut close = None;
        for (i, ch) in r.char_indices().skip(open) {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let name = r[..open].trim();
        let inner = r[open + 1..close].trim();
        rest = &r[close + 1..];
        let arg = (!inner.is_empty()).then_some(inner);
        let clamp1 = |a: f32| a.clamp(0.0, 1.0);
        ops.push(match name {
            "blur" => FilterOp::Blur(arg.map_or(Some(0.0), px)?.max(0.0)),
            "brightness" => FilterOp::Brightness(amount(arg, 1.0)?.max(0.0)),
            "contrast" => FilterOp::Contrast(amount(arg, 1.0)?.max(0.0)),
            "grayscale" => FilterOp::Grayscale(clamp1(amount(arg, 1.0)?)),
            "invert" => FilterOp::Invert(clamp1(amount(arg, 1.0)?)),
            "opacity" => FilterOp::Opacity(clamp1(amount(arg, 1.0)?)),
            "saturate" => FilterOp::Saturate(amount(arg, 1.0)?.max(0.0)),
            "sepia" => FilterOp::Sepia(clamp1(amount(arg, 1.0)?)),
            "hue-rotate" => FilterOp::HueRotate(arg.map_or(Some(0.0), |a| angle_deg(a).or_else(|| (a == "0").then_some(0.0)))?),
            "drop-shadow" => match shadow(inner)? {
                Value::Shadow(sh) => FilterOp::DropShadow(Shadow { spread: 0.0, inset: false, ..sh }),
                _ => return None,
            },
            "url" => continue,
            _ => return None,
        });
    }
    if ops.is_empty() {
        return Some(Value::Kw(Kw::None));
    }
    FILTERS.with(|t| {
        let mut t = t.borrow_mut();
        t.push(ops.into());
        Some(Value::Filter(t.len() as u32 - 1))
    })
}

/// The individual `translate` / `rotate` / `scale` properties.
fn transform_prop(name: &str, v: &str) -> Option<Value> {
    let v = v.trim();
    if v == "none" {
        return Some(Value::Kw(Kw::None));
    }
    let toks: Vec<&str> = v.split_whitespace().collect();
    let op = match name {
        "translate" => {
            let l = |s: &str| length(s).or_else(|| (s == "0").then_some(Value::Px(0.0)));
            TfOp::Translate(l(toks.first()?)?, toks.get(1).map_or(Some(Value::Px(0.0)), |s| l(s))?)
        }
        "rotate" => TfOp::Rotate(angle_deg(toks.last()?)?),
        _ => {
            let x = scale_num(toks.first()?)?;
            TfOp::Scale(x, toks.get(1).map_or(Some(x), |s| scale_num(s))?)
        }
    };
    Some(store_transform(vec![op]))
}

pub fn content(i: u32) -> Rc<[ContentItem]> {
    CONTENTS.with(|t| t.borrow()[i as usize].clone())
}

/// Decode CSS string escapes (`\f101`, `\"`, `\\`).
fn css_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && it.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            hex.push(it.next().unwrap());
        }
        if hex.is_empty() {
            match it.next() {
                Some('\n') | None => {}
                Some(c) => out.push(c),
            }
        } else {
            if it.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                it.next();
            }
            out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).filter(|&c| c != '\0').unwrap_or('\u{fffd}'));
        }
    }
    out
}

/// Parse a `content` value (original case). None/normal give Kw(None).
fn content_value(v: &str) -> Option<Value> {
    let v = v.trim();
    // Alternative text after '/' is ignored.
    let mut items = Vec::new();
    let b = v.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' => i += 1,
            b'/' => break,
            q @ (b'"' | b'\'') => {
                let mut j = i + 1;
                while j < b.len() && b[j] != q {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                items.push(ContentItem::Str(css_unescape(&v[i + 1..j.min(b.len())]).into()));
                i = j + 1;
            }
            _ => {
                let start = i;
                let mut depth = 0;
                while i < b.len() && (depth > 0 || !matches!(b[i], b' ' | b'\t' | b'\n')) {
                    match b[i] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                let tok = &v[start..i];
                let lower = tok.to_ascii_lowercase();
                match lower.as_str() {
                    "none" | "normal" => return Some(Value::Kw(Kw::None)),
                    "open-quote" => items.push(ContentItem::OpenQuote),
                    "close-quote" => items.push(ContentItem::CloseQuote),
                    "no-open-quote" | "no-close-quote" => {}
                    _ if lower.starts_with("attr(") => {
                        let name = tok[5..tok.len().saturating_sub(1)].split_whitespace().next().unwrap_or("");
                        items.push(ContentItem::Attr(intern_lower(name)));
                    }
                    _ if lower.starts_with("counter") => items.push(ContentItem::Counter),
                    _ if lower.starts_with("url(") => {
                        items.push(ContentItem::Url);
                    }
                    _ => return None,
                }
            }
        }
    }
    let i = CONTENTS.with(|t| {
        let mut t = t.borrow_mut();
        t.push(items.into());
        t.len() as u32 - 1
    });
    Some(Value::Content(i))
}

pub fn bg_image(i: u32) -> BgImage {
    IMAGES.with(|t| t.borrow().0[i as usize].clone())
}

fn store_image(key: String, img: BgImage) -> Value {
    IMAGES.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(&i) = t.1.get(&key) {
            return Value::Image(i);
        }
        let i = t.0.len() as u32;
        t.0.push(img);
        t.1.insert(key, i);
        Value::Image(i)
    })
}

/// url(...) or a gradient function (original case preserved).
fn image_value(t: &str) -> Option<Value> {
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("url(") {
        let inner = t[4..].strip_suffix(')')?.trim().trim_matches(|c| c == '"' || c == '\'');
        if inner.is_empty() {
            return None;
        }
        let abs = BASE.with(|b| crate::net::resolve(&b.borrow(), inner));
        return Some(store_image(format!("u{abs}"), BgImage::Url(Rc::from(abs.as_str()))));
    }
    let name_end = lower.find('(')?;
    let name = lower[..name_end].trim_start_matches("-webkit-").trim_start_matches("-moz-");
    let (radial, repeating) = match name {
        "linear-gradient" => (false, false),
        "repeating-linear-gradient" => (false, true),
        "radial-gradient" => (true, false),
        "repeating-radial-gradient" => (true, true),
        _ => return None,
    };
    let legacy = lower.starts_with("-webkit-") || lower.starts_with("-moz-");
    let inner = lower[name_end + 1..].strip_suffix(')')?;
    let args = split_top(inner, b',');
    let mut angle = 180.0;
    let mut first = 0;
    if let Some(a) = args.first().map(|a| a.trim()) {
        let side = |s: &str| -> Option<f32> {
            let (mut x, mut y) = (0.0f32, 0.0f32);
            for w in s.split_whitespace() {
                match w {
                    "top" => y = -1.0,
                    "bottom" => y = 1.0,
                    "left" => x = -1.0,
                    "right" => x = 1.0,
                    _ => return None,
                }
            }
            Some(x.atan2(-y).to_degrees().rem_euclid(360.0))
        };
        if let Some(rest) = a.strip_prefix("to ") {
            if let Some(d) = side(rest) {
                angle = d;
                first = 1;
            }
        } else if legacy && side(a).is_some() {
            angle = (side(a).unwrap() + 180.0).rem_euclid(360.0);
            first = 1;
        } else if let Some(n) = a.strip_suffix("deg") {
            if let Ok(d) = n.parse::<f32>() {
                angle = if legacy { 90.0 - d } else { d };
                first = 1;
            }
        } else if let Some(n) = a.strip_suffix("turn") {
            if let Ok(d) = n.parse::<f32>() {
                angle = d * 360.0;
                first = 1;
            }
        } else if let Some(n) = a.strip_suffix("rad") {
            if let Ok(d) = n.parse::<f32>() {
                angle = d.to_degrees();
                first = 1;
            }
        } else if radial && tokens_ws(a).first().and_then(|t| color_value(t)).is_none() {
            first = 1; // shape / size / position: ignored
        }
    }
    let mut stops = Vec::new();
    for a in &args[first..] {
        let toks = tokens_ws(a.trim());
        let Some(Value::Color(c)) = toks.iter().find_map(|t| color_value(t)) else { continue };
        let color = if c == CURRENT_COLOR { 0xFF00_0000 } else { c };
        let mut positions = toks.iter().filter_map(|t| length(t));
        let pos = |v: Option<Value>| match v {
            Some(Value::Pct(p)) => (Some(p / 100.0), None),
            Some(Value::Px(p)) => (None, Some(p)),
            Some(Value::Em(e)) | Some(Value::Rem(e)) => (None, Some(e * 16.0)),
            _ => (None, None),
        };
        let (frac, px) = pos(positions.next());
        stops.push(Stop { color, frac, px });
        // A second position repeats the colour (hard stops).
        let (f2, p2) = pos(positions.next());
        if f2.is_some() || p2.is_some() {
            stops.push(Stop { color, frac: f2, px: p2 });
        }
    }
    if stops.is_empty() {
        return None;
    }
    Some(store_image(format!("g{lower}"), BgImage::Gradient(Rc::new(Gradient { radial, repeating, angle, stops }))))
}

fn bg_pos_kw(t: &str) -> Option<(Value, Option<bool>)> {
    // (value, Some(true) if it can only be horizontal, Some(false) if only vertical)
    Some(match t {
        "left" => (Value::Pct(0.0), Some(true)),
        "right" => (Value::Pct(100.0), Some(true)),
        "top" => (Value::Pct(0.0), Some(false)),
        "bottom" => (Value::Pct(100.0), Some(false)),
        "center" => (Value::Pct(50.0), None),
        _ => (length(t)?, None),
    })
}

fn bg_position(toks: &[&str], important: bool, out: &mut Vec<Decl>) {
    // Four-value syntax: `right 10px bottom 20px`.
    if toks.len() >= 3 {
        let (mut x, mut y) = (Value::Pct(50.0), Value::Pct(50.0));
        let mut i = 0;
        while i < toks.len() {
            let kw = toks[i];
            let off = toks.get(i + 1).and_then(|t| length(t)).filter(|_| !matches!(toks.get(i + 1), Some(&("left" | "right" | "top" | "bottom" | "center"))));
            let px = match off {
                Some(Value::Px(p)) => p,
                Some(Value::Em(e)) | Some(Value::Rem(e)) => e * 16.0,
                _ => 0.0,
            };
            let pct_off = if let Some(Value::Pct(p)) = off { p } else { 0.0 };
            let v = |from_end: bool| {
                if from_end { Value::Calc([100.0 - pct_off, -px, 0.0, 0.0, 0.0, 0.0]) } else { Value::Calc([pct_off, px, 0.0, 0.0, 0.0, 0.0]) }
            };
            match kw {
                "left" => x = v(false),
                "right" => x = v(true),
                "top" => y = v(false),
                "bottom" => y = v(true),
                _ => {}
            }
            i += if off.is_some() { 2 } else { 1 };
        }
        out.push(Decl { prop: BgPosX, value: x, important });
        out.push(Decl { prop: BgPosY, value: y, important });
        return;
    }
    let vals: Vec<(Value, Option<bool>)> = toks.iter().filter_map(|t| bg_pos_kw(t)).take(2).collect();
    let (x, y) = match vals.as_slice() {
        [(v, Some(false))] => (Value::Pct(50.0), *v),
        [(v, _)] => (*v, Value::Pct(50.0)),
        [(a, sa), (b, sb)] => {
            if *sa == Some(false) || *sb == Some(true) {
                (*b, *a)
            } else {
                (*a, *b)
            }
        }
        _ => return,
    };
    out.push(Decl { prop: BgPosX, value: x, important });
    out.push(Decl { prop: BgPosY, value: y, important });
}

fn bg_size(v: &str, important: bool, out: &mut Vec<Decl>) {
    let toks = tokens_ws(v);
    let (x, y) = match toks.as_slice() {
        ["cover"] => (Value::Kw(Kw::Cover), Value::Auto),
        ["contain"] => (Value::Kw(Kw::Contain), Value::Auto),
        [a] => (length(a).unwrap_or(Value::Auto), Value::Auto),
        [a, b, ..] => (length(a).unwrap_or(Value::Auto), length(b).unwrap_or(Value::Auto)),
        _ => return,
    };
    out.push(Decl { prop: BgSizeX, value: x, important });
    out.push(Decl { prop: BgSizeY, value: y, important });
}

fn bg_repeat(toks: &[&str]) -> Option<Value> {
    let k = |t: &str| -> Option<bool> {
        match t {
            "repeat" | "space" | "round" => Some(true),
            "no-repeat" => Some(false),
            _ => None,
        }
    };
    Some(Value::Kw(match toks {
        ["repeat-x"] => Kw::RepeatX,
        ["repeat-y"] => Kw::RepeatY,
        [a] => {
            if k(a)? { Kw::Repeat } else { Kw::NoRepeat }
        }
        [a, b, ..] => match (k(a)?, k(b)?) {
            (true, true) => Kw::Repeat,
            (false, false) => Kw::NoRepeat,
            (true, false) => Kw::RepeatX,
            (false, true) => Kw::RepeatY,
        },
        _ => return None,
    }))
}

/// The `background` shorthand: first layer's image, last layer's colour.
fn background(orig: &str, important: bool, out: &mut Vec<Decl>) {
    let layers = split_top(orig, b',');
    let mut color = Value::Color(0);
    let mut image = Value::Kw(Kw::None);
    let mut repeat = Value::Kw(Kw::Repeat);
    let (mut pos, mut size): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for (li, layer) in layers.iter().enumerate() {
        // Space out a '/' between position and size.
        let mut spaced = String::new();
        let mut depth = 0;
        for ch in layer.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if ch == '/' && depth == 0 {
                spaced.push_str(" / ");
            } else {
                spaced.push(ch);
            }
        }
        let mut after_slash = false;
        let mut reps = Vec::new();
        for t in tokens_ws(&spaced) {
            let lt = t.to_ascii_lowercase();
            if li == 0 {
                if lt == "/" {
                    after_slash = true;
                    continue;
                }
                if let Some(img) = image_value(t) {
                    image = img;
                    continue;
                }
                if matches!(lt.as_str(), "repeat" | "no-repeat" | "repeat-x" | "repeat-y" | "space" | "round") {
                    reps.push(lt);
                    continue;
                }
                if after_slash && (length(&lt).is_some() || lt == "auto" || lt == "cover" || lt == "contain") {
                    size.push(lt);
                    continue;
                }
                if bg_pos_kw(&lt).is_some() {
                    pos.push(lt);
                    continue;
                }
            }
            if li == layers.len() - 1 {
                if let Some(c) = color_value(&lt) {
                    color = c;
                }
            }
        }
        if li == 0 && !reps.is_empty() {
            let r: Vec<&str> = reps.iter().map(|s| s.as_str()).collect();
            repeat = bg_repeat(&r).unwrap_or(repeat);
        }
    }
    out.push(Decl { prop: Background, value: color, important });
    out.push(Decl { prop: BackgroundImage, value: image, important });
    out.push(Decl { prop: BgRepeat, value: repeat, important });
    let p: Vec<&str> = pos.iter().map(|s| s.as_str()).collect();
    if p.is_empty() {
        out.push(Decl { prop: BgPosX, value: Value::Pct(0.0), important });
        out.push(Decl { prop: BgPosY, value: Value::Pct(0.0), important });
    } else {
        bg_position(&p, important, out);
    }
    if size.is_empty() {
        out.push(Decl { prop: BgSizeX, value: Value::Auto, important });
        out.push(Decl { prop: BgSizeY, value: Value::Auto, important });
    } else {
        bg_size(&size.join(" "), important, out);
    }
}

thread_local! {
    /// Cascade layer names in order of first appearance (for the whole
    /// cascade being built; see `reset_layers`).
    static LAYERS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Start a new cascade: forget layer order.
pub fn reset_layers() {
    LAYERS.with(|l| l.borrow_mut().clear());
}

/// The rank of layer `name` (registering it). Unlayered rules use UNLAYERED.
fn layer_rank(name: &str) -> u16 {
    LAYERS.with(|l| {
        let mut l = l.borrow_mut();
        match l.iter().position(|n| n == name) {
            Some(i) => i as u16,
            None => {
                l.push(name.to_owned());
                (l.len() - 1).min(u16::MAX as usize - 1) as u16
            }
        }
    })
}

/// Rules outside any @layer win over layered ones.
pub const UNLAYERED: u16 = u16::MAX;

/// Rule-parsing context: enclosing layer and nesting.
#[derive(Clone)]
struct RuleCtx {
    /// Full name of the enclosing layer ("" = none).
    layer: String,
}

impl RuleCtx {
    fn rank(&self) -> u16 {
        if self.layer.is_empty() { UNLAYERED } else { layer_rank(&self.layer) }
    }
    fn sub_layer(&self, name: &str) -> RuleCtx {
        let name = if name.is_empty() {
            LAYERS.with(|l| format!("#anon{}", l.borrow().len()))
        } else {
            name.to_owned()
        };
        RuleCtx { layer: if self.layer.is_empty() { name } else { format!("{}.{name}", self.layer) } }
    }
}

/// Evaluate an @supports condition.
pub fn supports(cond: &str) -> bool {
    let c = cond.trim();
    if c.is_empty() {
        return false;
    }
    if let Some(rest) = c.strip_prefix("not ").or_else(|| c.strip_prefix("not(").map(|_| &c[3..])) {
        return !supports(rest);
    }
    let ands = split_words_top(c, "and");
    if ands.len() > 1 {
        return ands.iter().all(|p| supports(p));
    }
    let ors = split_words_top(c, "or");
    if ors.len() > 1 {
        return ors.iter().any(|p| supports(p));
    }
    let lower = c.to_ascii_lowercase();
    for (f, ok) in [("selector(", true), ("font-tech(", false), ("font-format(", true)] {
        if lower.starts_with(f) {
            let Some(close) = match_paren(c, f.len() - 1) else { return false };
            let arg = &c[f.len()..close];
            return ok && (f != "selector(" || !parse_selector_list(arg).is_empty());
        }
    }
    if c.starts_with('(') {
        let Some(close) = match_paren(c, 0) else { return false };
        if close + 1 != c.len() {
            return false;
        }
        let inner = c[1..close].trim();
        if inner.starts_with('(') || inner.starts_with("not") || inner.starts_with("selector(") {
            return supports(inner);
        }
        let Some((name, value)) = inner.split_once(':') else { return false };
        let name = name.trim().to_ascii_lowercase();
        if name.starts_with("--") {
            return true;
        }
        let name = name.strip_prefix("-webkit-").unwrap_or(&name);
        let mut out = Vec::new();
        parse_property(name, value.trim(), false, &mut out);
        return !out.is_empty();
    }
    false
}

/// Combine a nested selector with its parent (CSS nesting).
fn nest_selector(parent: &str, child: &str) -> String {
    if child.contains('&') {
        child.replace('&', parent)
    } else {
        format!("{parent} {child}")
    }
}

fn parse_rules(s: &str, sheet: &mut Stylesheet, media: &Media) {
    parse_rules_in(s, sheet, media, &RuleCtx { layer: String::new() });
}

fn parse_rules_in(s: &str, sheet: &mut Stylesheet, media: &Media, ctx: &RuleCtx) {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    loop {
        while i < n && (is_ws(b[i]) || b[i] == b'}' || b[i] == b';') {
            i += 1;
        }
        if i >= n {
            break;
        }
        if b[i] == b'@' {
            let mut j = i + 1;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
                j += 1;
            }
            let name = s[i + 1..j].to_ascii_lowercase();
            let Some(pos) = find_top(b, j, b";{") else { break };
            let prelude = s[j..pos].trim();
            if b[pos] == b';' {
                if name == "layer" {
                    // `@layer a, b;` fixes the order of layers.
                    for l in prelude.split(',') {
                        ctx.sub_layer(l.trim()).rank();
                    }
                }
                i = pos + 1;
                continue;
            }
            let end = match_brace(b, pos);
            let inner = &s[pos + 1..end.min(n)];
            match name.as_str() {
                "media" if eval_media(prelude, media) => parse_rules_in(inner, sheet, media, ctx),
                "container" => {
                    // Approximation: the container is the viewport.
                    let cond = prelude.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_').trim();
                    let cond = if prelude.starts_with('(') || prelude.starts_with("not") { prelude } else { cond };
                    if cond.is_empty() || eval_media(cond, media) {
                        parse_rules_in(inner, sheet, media, ctx);
                    }
                }
                "font-face" => parse_font_face(inner),
                "keyframes" | "-webkit-keyframes" | "-moz-keyframes" => {
                    let kname: Rc<str> = Rc::from(prelude.trim().trim_matches(|c| c == '"' || c == '\''));
                    // Only the final keyframe is used (the end state of
                    // animations that fill forwards).
                    let ib = inner.as_bytes();
                    let mut k = 0;
                    while let Some(open) = find_top(ib, k, b"{") {
                        let close = match_brace(ib, open);
                        let sel = inner[k..open].trim().trim_start_matches(['}', ';']).trim().to_ascii_lowercase();
                        if sel.split(',').any(|s| matches!(s.trim(), "to" | "100%")) {
                            let start = sheet.decls.len() as u32;
                            parse_declarations(&inner[open + 1..close.min(inner.len())], &mut sheet.decls, &mut sheet.raws);
                            sheet.keyframes.push((kname.clone(), start, sheet.decls.len() as u32));
                        }
                        k = close + 1;
                    }
                }
                "supports" if supports(prelude) => parse_rules_in(inner, sheet, media, ctx),
                "layer" => parse_rules_in(inner, sheet, media, &ctx.sub_layer(prelude)),
                "scope" | "document" | "-moz-document" | "starting-style" => {}
                _ => {}
            }
            i = end + 1;
            continue;
        }
        let Some(brace) = find_top(b, i, b"{") else { break };
        let end = match_brace(b, brace);
        let prelude = &s[i..brace];
        let body = &s[brace + 1..end.min(n)];
        i = end + 1;
        parse_style_rule(prelude, body, sheet, media, ctx);
    }
}

/// A style rule; nested rules (CSS nesting) are flattened.
fn parse_style_rule(prelude: &str, body: &str, sheet: &mut Stylesheet, media: &Media, ctx: &RuleCtx) {
    let rank = ctx.rank();
    let emit = |sheet: &mut Stylesheet, prelude: &str, decls: &str| {
        let sels = parse_selector_list(prelude);
        if sels.is_empty() {
            return;
        }
        let start = sheet.decls.len() as u32;
        parse_declarations(decls, &mut sheet.decls, &mut sheet.raws);
        let end = sheet.decls.len() as u32;
        if end > start {
            for sel in sels {
                sheet.rules.push(StyleRule { sel, start, end, layer: rank });
            }
        }
    };
    let bb = body.as_bytes();
    if find_top(bb, 0, b"{").is_none() {
        emit(sheet, prelude, body);
        return;
    }
    // Declarations and nested rules apply in source order: declarations
    // after a nested rule form their own rule (CSSNestedDeclarations).
    let parents: Vec<&str> = split_top(prelude, b',').into_iter().map(str::trim).filter(|p| !p.is_empty()).collect();
    let mut decls = String::new();
    let mut i = 0;
    while i < bb.len() {
        let end = find_top(bb, i, b";{").unwrap_or(bb.len());
        if end < bb.len() && bb[end] == b'{' {
            let close = match_brace(bb, end);
            let pre = body[i..end].trim();
            let inner = &body[end + 1..close.min(bb.len())];
            i = close + 1;
            if !decls.trim().is_empty() {
                emit(sheet, prelude, &decls);
                decls.clear();
            }
            if let Some(at) = pre.strip_prefix('@') {
                let name_len = at.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(at.len());
                let (name, cond) = (at[..name_len].to_ascii_lowercase(), at[name_len..].trim());
                match name.as_str() {
                    "media" if eval_media(cond, media) => parse_style_rule(prelude, inner, sheet, media, ctx),
                    "supports" if supports(cond) => parse_style_rule(prelude, inner, sheet, media, ctx),
                    "container" | "scope" => parse_style_rule(prelude, inner, sheet, media, ctx),
                    "layer" => parse_style_rule(prelude, inner, sheet, media, &ctx.sub_layer(cond)),
                    _ => {}
                }
                continue;
            }
            let sel: Vec<String> = split_top(pre, b',').into_iter().flat_map(|c| parents.iter().map(move |p| nest_selector(p, c.trim()))).collect();
            parse_style_rule(&sel.join(", "), inner, sheet, media, ctx);
        } else {
            decls.push_str(&body[i..end.min(bb.len())]);
            decls.push(';');
            i = end + 1;
        }
    }
    if !decls.trim().is_empty() {
        emit(sheet, prelude, &decls);
    }
}

pub fn eval_media(q: &str, m: &Media) -> bool {
    let q = q.to_ascii_lowercase();
    q.split(',').any(|part| eval_media_one(part.trim(), m))
}

fn media_px(v: &str) -> f32 {
    match parse_len(v.trim()) {
        Some(Value::Px(p)) => p,
        Some(Value::Em(e)) | Some(Value::Rem(e)) => e * 16.0,
        Some(Value::Num(n)) => n,
        _ => 0.0,
    }
}

fn eval_media_one(p: &str, m: &Media) -> bool {
    let p = p.trim();
    // `a or b` at the top level (media queries level 4).
    if p.contains(" or ") {
        return split_words_top(p, "or").iter().any(|q| eval_media_one(q, m));
    }
    let (neg, p) = match p.strip_prefix("not ") {
        Some(r) => (true, r.trim()),
        None => (false, p),
    };
    let p = p.strip_prefix("only ").unwrap_or(p).trim();
    let ty = p.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("");
    let mut ok = matches!(ty, "" | "screen" | "all" | "and" | "not");
    let mut rest = p;
    while ok {
        let Some(open) = rest.find('(') else { break };
        let Some(close) = match_paren(rest, open) else { break };
        // `not (feature)` inside a condition
        let negate = rest[..open].trim_end().ends_with("not");
        let f = rest[open + 1..close].trim();
        rest = &rest[close + 1..];
        let r = if f.starts_with('(') || f.starts_with("not ") || f.contains(" or ") {
            eval_media_one(f, m)
        } else {
            media_feature(f, m)
        };
        ok &= r != negate;
    }
    ok != neg
}

/// Split at top-level occurrences of the word `w`.
fn split_words_top<'a>(s: &'a str, w: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    let b = s.as_bytes();
    let pat = format!(" {w} ");
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && s[i..].starts_with(&pat) => {
                out.push(s[start..i].trim());
                i += pat.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(s[start..].trim());
    out
}

fn match_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

fn media_ratio(v: &str) -> f32 {
    match v.split_once('/') {
        Some((a, b)) => a.trim().parse::<f32>().unwrap_or(0.0) / b.trim().parse::<f32>().unwrap_or(1.0).max(1e-6),
        None => v.trim().parse().unwrap_or(0.0),
    }
}

fn media_resolution(v: &str) -> f32 {
    let v = v.trim();
    if let Some(n) = v.strip_suffix("dppx").or_else(|| v.strip_suffix('x')) {
        n.parse().unwrap_or(1.0)
    } else if let Some(n) = v.strip_suffix("dpi") {
        n.parse::<f32>().unwrap_or(96.0) / 96.0
    } else if let Some(n) = v.strip_suffix("dpcm") {
        n.parse::<f32>().unwrap_or(37.8) * 2.54 / 96.0
    } else {
        v.parse().unwrap_or(1.0)
    }
}

/// One media feature test, e.g. `min-width: 600px`, `width >= 600px`, `hover`.
fn media_feature(f: &str, m: &Media) -> bool {
    // Value of a feature by name, for range comparisons.
    let value = |name: &str| -> Option<f32> {
        Some(match name {
            "width" | "device-width" => m.vw,
            "height" | "device-height" => m.vh,
            "aspect-ratio" | "device-aspect-ratio" => m.vw / m.vh.max(1.0),
            "resolution" => 1.0,
            "color" => 8.0,
            "monochrome" | "color-index" | "grid" => 0.0,
            _ => return None,
        })
    };
    let parse_val = |name: &str, v: &str| -> f32 {
        match name {
            "aspect-ratio" | "device-aspect-ratio" => media_ratio(v),
            "resolution" => media_resolution(v),
            "color" | "monochrome" | "color-index" | "grid" => v.trim().parse().unwrap_or(0.0),
            _ => media_px(v),
        }
    };
    if f.contains(['<', '>', '=']) && !f.contains(':') {
        // Range syntax: (width >= 600px), (400px < width <= 800px)
        let mut toks: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut chars = f.chars().peekable();
        while let Some(c) = chars.next() {
            if matches!(c, '<' | '>' | '=') {
                if !cur.trim().is_empty() {
                    toks.push(cur.trim().to_owned());
                }
                cur.clear();
                let mut op = c.to_string();
                if chars.peek() == Some(&'=') {
                    op.push(chars.next().unwrap());
                }
                toks.push(op);
            } else {
                cur.push(c);
            }
        }
        if !cur.trim().is_empty() {
            toks.push(cur.trim().to_owned());
        }
        let name = toks.iter().find(|t| value(t).is_some()).cloned();
        let Some(name) = name else { return false };
        let dim = value(&name).unwrap_or(0.0);
        let mut res = true;
        for w in toks.windows(3) {
            let (a, op, b) = (&w[0], w[1].as_str(), &w[2]);
            let (l, r) = if *a == name {
                (dim, parse_val(&name, b))
            } else if *b == name {
                (parse_val(&name, a), dim)
            } else {
                continue;
            };
            res &= match op {
                ">=" => l >= r,
                "<=" => l <= r,
                ">" => l > r,
                "<" => l < r,
                "=" => (l - r).abs() < 0.01,
                _ => true,
            };
        }
        return res;
    }
    let (name, val) = match f.split_once(':') {
        Some((n, v)) => (n.trim(), Some(v.trim())),
        None => (f.trim(), None),
    };
    let Some(val) = val else {
        // Boolean context: true unless the feature's value is 0 / none.
        return match name {
            "hover" | "any-hover" | "pointer" | "any-pointer" | "color" | "width" | "height" | "aspect-ratio" | "resolution" | "scripting" | "update" | "orientation" => true,
            _ => false,
        };
    };
    if let Some(base) = name.strip_prefix("min-") {
        if let Some(v) = value(base).or_else(|| (base == "device-pixel-ratio").then_some(1.0)) {
            return v >= parse_val(base, val) - 0.001;
        }
    }
    if let Some(base) = name.strip_prefix("max-") {
        if let Some(v) = value(base).or_else(|| (base == "device-pixel-ratio").then_some(1.0)) {
            return v <= parse_val(base, val) + 0.001;
        }
    }
    match name {
        "-webkit-min-device-pixel-ratio" | "min--moz-device-pixel-ratio" => media_resolution(val) <= 1.0,
        "-webkit-max-device-pixel-ratio" | "max--moz-device-pixel-ratio" => media_resolution(val) >= 1.0,
        "prefers-color-scheme" => val == "light",
        "prefers-reduced-motion" | "prefers-reduced-transparency" | "prefers-reduced-data" | "prefers-contrast" => val == "no-preference",
        "orientation" => (val == "landscape") == (m.vw >= m.vh),
        "hover" | "any-hover" => val == "hover",
        "pointer" | "any-pointer" => val == "fine",
        "display-mode" => val == "browser",
        "forced-colors" | "inverted-colors" => val == "none",
        "scripting" => val == "enabled",
        "update" => val == "fast",
        "dynamic-range" | "video-dynamic-range" => val == "standard",
        "color-gamut" => val == "srgb",
        "overflow-block" => val == "scroll",
        "overflow-inline" => val == "scroll",
        _ => match value(name) {
            Some(v) => (v - parse_val(name, val)).abs() < 0.5,
            None => false,
        },
    }
}

// ---------------------------------------------------------- selector parsing

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'-' || c == b'\\' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'\\' || c >= 0x80
}

/// Parse an identifier (with escapes) starting at byte i.
fn ident(s: &str, mut i: usize) -> (String, usize) {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut run = i;
    while i < b.len() && is_ident_char(b[i]) {
        if b[i] == b'\\' {
            out.push_str(&s[run..i]);
            i += 1;
            if i >= b.len() {
                run = i;
                break;
            }
            if b[i].is_ascii_hexdigit() {
                let hs = i;
                while i < b.len() && b[i].is_ascii_hexdigit() && i - hs < 6 {
                    i += 1;
                }
                let v = u32::from_str_radix(&s[hs..i], 16).unwrap_or(0xfffd);
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                if i < b.len() && b[i] == b' ' {
                    i += 1;
                }
            } else {
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            run = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[run..i]);
    (out, i)
}

fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while let Some(p) = find_top(b, i, &[sep]) {
        out.push(&s[start..p]);
        start = p + 1;
        i = p + 1;
    }
    out.push(&s[start..]);
    out
}

pub fn parse_selector_list(s: &str) -> Vec<Selector> {
    split_top(s, b',').into_iter().filter_map(|p| parse_selector(p.trim())).collect()
}

thread_local! {
    /// A stylesheet used :hover / :active (the page restyles on pointer moves).
    static HOVER_RULES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn hover_rules_seen() -> bool {
    HOVER_RULES.with(|h| h.get())
}

fn max_spec(list: &[Selector]) -> u32 {
    list.iter().map(|s| s.spec).max().unwrap_or(0)
}

fn compound_spec(c: &Compound) -> u32 {
    let mut spec = if c.id.is_some() { 1 << 16 } else { 0 };
    spec += ((c.classes.len() + c.attrs.len()) as u32) << 8;
    if c.tag.is_some() {
        spec += 1;
    }
    for p in &c.pseudos {
        spec += match p {
            Pseudo::Is(_, true) | Pseudo::Always => 0,
            Pseudo::Is(l, false) | Pseudo::Not(l) => max_spec(l),
            Pseudo::Has(l) => l.iter().map(|(_, s)| s.spec).max().unwrap_or(0),
            Pseudo::Before | Pseudo::After => 1,
            Pseudo::Host(l) | Pseudo::Slotted(l) => (1 << 8) + l.iter().map(compound_spec).max().unwrap_or(0),
            _ => 1 << 8,
        };
    }
    spec
}

fn parse_compound_list(s: &str) -> Option<Vec<Compound>> {
    split_top(s, b',')
        .into_iter()
        .map(|p| {
            let p = p.trim();
            let (c, end) = parse_compound(p, 0)?;
            (end == p.len()).then_some(c)
        })
        .collect()
}

fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let a: String = arg.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match a.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    if let Some(p) = a.find('n') {
        let an = &a[..p];
        let bn = &a[p + 1..];
        let av = match an {
            "" | "+" => 1,
            "-" => -1,
            x => x.parse().ok()?,
        };
        let bv = if bn.is_empty() { 0 } else { bn.trim_start_matches('+').parse().ok()? };
        Some((av, bv))
    } else {
        Some((0, a.parse().ok()?))
    }
}

fn parse_compound(s: &str, mut i: usize) -> Option<(Compound, usize)> {
    let b = s.as_bytes();
    let n = b.len();
    let start = i;
    let mut c = Compound::default();
    if i < n && b[i] == b'*' {
        i += 1;
    } else if i < n && is_ident_start(b[i]) {
        let (name, j) = ident(s, i);
        c.tag = Some(intern_lower(&name));
        i = j;
    }
    while i < n {
        match b[i] {
            b'#' => {
                let (name, j) = ident(s, i + 1);
                if name.is_empty() {
                    return None;
                }
                c.id = Some(intern(&name));
                i = j;
            }
            b'.' => {
                let (name, j) = ident(s, i + 1);
                if name.is_empty() {
                    return None;
                }
                c.classes.push(intern(&name));
                i = j;
            }
            b'[' => {
                let close = i + 1 + find_top(b, i + 1, b"]")? - (i + 1);
                let inner = s[i + 1..close].trim();
                i = close + 1;
                let ops: [(&str, AttrOp); 6] = [
                    ("~=", AttrOp::Word),
                    ("^=", AttrOp::Prefix),
                    ("$=", AttrOp::Suffix),
                    ("*=", AttrOp::Substr),
                    ("|=", AttrOp::Dash),
                    ("=", AttrOp::Eq),
                ];
                let mut sel = None;
                for (tok, op) in ops {
                    if let Some(p) = inner.find(tok) {
                        let name = inner[..p].trim();
                        let mut val = inner[p + tok.len()..].trim();
                        let mut ci = false;
                        if val.ends_with(" i") || val.ends_with(" I") {
                            ci = true;
                            val = val[..val.len() - 2].trim();
                        }
                        let val = val.trim_matches(|c| c == '"' || c == '\'');
                        sel = Some(AttrSel {
                            name: intern_lower(name),
                            op,
                            value: if ci { val.to_ascii_lowercase() } else { val.to_owned() },
                            ci,
                        });
                        break;
                    }
                }
                c.attrs.push(sel.unwrap_or(AttrSel {
                    name: intern_lower(inner),
                    op: AttrOp::Exists,
                    value: String::new(),
                    ci: false,
                }));
            }
            b':' => {
                let element = i + 1 < n && b[i + 1] == b':';
                i += if element { 2 } else { 1 };
                let (name, j) = ident(s, i);
                i = j;
                let mut arg = "";
                if i < n && b[i] == b'(' {
                    let close = find_top(b, i + 1, b")").unwrap_or(n);
                    arg = &s[i + 1..close];
                    i = (close + 1).min(n);
                }
                let name = name.to_ascii_lowercase();
                let p = if element {
                    match name.as_str() {
                        "slotted" => parse_compound_list(arg).map_or(Pseudo::Never, Pseudo::Slotted),
                        "before" => Pseudo::Before,
                        "after" => Pseudo::After,
                        _ => Pseudo::Never,
                    }
                } else {
                    match name.as_str() {
                        "first-child" => Pseudo::FirstChild,
                        "last-child" => Pseudo::LastChild,
                        "only-child" => Pseudo::OnlyChild,
                        "first-of-type" => Pseudo::FirstOfType,
                        "last-of-type" => Pseudo::LastOfType,
                        "nth-child" => parse_nth(arg).map_or(Pseudo::Never, |(a, b)| Pseudo::NthChild(a, b)),
                        "nth-last-child" => parse_nth(arg).map_or(Pseudo::Never, |(a, b)| Pseudo::NthLastChild(a, b)),
                        "link" | "any-link" => Pseudo::Link,
                        "root" => Pseudo::Root,
                        "empty" => Pseudo::Empty,
                        "focus" | "focus-visible" => Pseudo::Focus,
                        "focus-within" => Pseudo::FocusWithin,
                        "checked" => Pseudo::Checked,
                        "disabled" => Pseudo::Disabled,
                        "placeholder-shown" => Pseudo::PlaceholderShown,
                        "not" => {
                            let l = parse_selector_list(arg);
                            if l.is_empty() { Pseudo::Never } else { Pseudo::Not(l) }
                        }
                        "is" | "where" | "matches" | "-webkit-any" | "-moz-any" => {
                            let l = parse_selector_list(arg);
                            if l.is_empty() { Pseudo::Never } else { Pseudo::Is(l, name == "where") }
                        }
                        "has" => {
                            let mut rel = Vec::new();
                            for part in split_top(arg, b',') {
                                let p = part.trim();
                                let (comb, rest) = match p.as_bytes().first() {
                                    Some(b'>') => (Some(Comb::Child), &p[1..]),
                                    Some(b'+') => (Some(Comb::Next), &p[1..]),
                                    Some(b'~') => (Some(Comb::Subsequent), &p[1..]),
                                    _ => (None, p),
                                };
                                if let Some(sel) = parse_selector(rest.trim()) {
                                    rel.push((comb, sel));
                                }
                            }
                            if rel.is_empty() { Pseudo::Never } else { Pseudo::Has(rel) }
                        }
                        "nth-of-type" => parse_nth(arg).map_or(Pseudo::Never, |(a, b)| Pseudo::NthOfType(a, b)),
                        "nth-last-of-type" => parse_nth(arg).map_or(Pseudo::Never, |(a, b)| Pseudo::NthLastOfType(a, b)),
                        "only-of-type" => Pseudo::OnlyOfType,
                        "hover" => {
                            HOVER_RULES.with(|h| h.set(true));
                            Pseudo::Hover
                        }
                        "active" => {
                            HOVER_RULES.with(|h| h.set(true));
                            Pseudo::Active
                        }
                        "target" => Pseudo::Target,
                        "enabled" => Pseudo::Enabled,
                        "required" => Pseudo::Required,
                        "optional" => Pseudo::Optional,
                        "read-only" | "-moz-read-only" => Pseudo::ReadOnly,
                        "read-write" | "-moz-read-write" => Pseudo::ReadWrite,
                        "scope" => Pseudo::Root,
                        "dir" if arg.trim() == "ltr" => Pseudo::Always,
                        "defined" | "valid" | "user-valid" | "in-range" | "lang" => Pseudo::Always,
                        "before" => Pseudo::Before,
                        "after" => Pseudo::After,
                        "host" | "host-context" if arg.trim().is_empty() => Pseudo::Host(Vec::new()),
                        "host" | "host-context" => parse_compound_list(arg).map_or(Pseudo::Never, Pseudo::Host),
                        _ => Pseudo::Never,
                    }
                };
                c.pseudos.push(p);
            }
            _ => break,
        }
    }
    (i > start).then_some((c, i))
}

fn parse_selector(s: &str) -> Option<Selector> {
    let b = s.as_bytes();
    let n = b.len();
    let mut comps = Vec::new();
    let mut combs = Vec::new();
    let mut pending: Option<Comb> = None;
    let mut i = 0;
    loop {
        while i < n && is_ws(b[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }
        let comb = match b[i] {
            b'>' => Some(Comb::Child),
            b'+' => Some(Comb::Next),
            b'~' => Some(Comb::Subsequent),
            _ => None,
        };
        if let Some(c) = comb {
            if pending.is_some() || comps.is_empty() {
                return None;
            }
            pending = Some(c);
            i += 1;
            continue;
        }
        if !comps.is_empty() {
            combs.push(pending.take().unwrap_or(Comb::Descendant));
        }
        let (c, j) = parse_compound(s, i)?;
        comps.push(c);
        i = j;
    }
    if comps.is_empty() || pending.is_some() {
        return None;
    }
    comps.reverse();
    combs.reverse();
    let spec = comps.iter().map(compound_spec).sum();
    // Ancestor atoms: compounds reachable from the subject via child/descendant.
    let mut ancestor_atoms = Vec::new();
    for (idx, comb) in combs.iter().enumerate() {
        if matches!(comb, Comb::Next | Comb::Subsequent) {
            break;
        }
        let c = &comps[idx + 1];
        ancestor_atoms.extend(c.tag);
        ancestor_atoms.extend(c.id);
        ancestor_atoms.extend(c.classes.iter().copied());
    }
    Some(Selector { comps, combs, spec, ancestor_atoms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(parse_color("#fff"), Some(0xffffffff));
        assert_eq!(parse_color("#336699"), Some(0xff336699));
        assert_eq!(parse_color("rgb(255, 0, 0)"), Some(0xffff0000));
        assert_eq!(parse_color("rgba(0 0 0 / 50%)"), Some(0x80000000));
        assert_eq!(parse_color("navy"), Some(0xff000080));
    }

    #[test]
    fn sheet() {
        let m = Media { vw: 1000.0, vh: 800.0 };
        let s = parse_stylesheet(
            "/* c */ div > p.x, #a { margin: 1px 2px; color: red !important }
             @media (max-width: 500px) { p { color: blue } }
             @media (min-width: 500px) { p { color: green } }",
            &m,
            "",
        );
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.decls.len(), 5 + 1);
        let r = &s.rules[0];
        assert_eq!(r.sel.comps.len(), 2);
        assert_eq!(r.sel.combs, vec![Comb::Child]);
        assert_eq!(r.sel.spec, (1 << 8) + 2);
    }
}
