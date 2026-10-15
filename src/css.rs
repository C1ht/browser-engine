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
    Not(Vec<Compound>),
    Is(Vec<Compound>),
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
}

#[derive(Default)]
pub struct Stylesheet {
    pub rules: Vec<StyleRule>,
    pub decls: Vec<Decl>,
    pub raws: Vec<Raw>,
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
        let f = &lower[..p];
        let inner = lower[p + 1..].trim_end_matches(')');
        let parts: Vec<&str> = inner
            .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let alpha = parts.get(3).map_or(Some(1.0), |a| {
            if let Some(p) = a.strip_suffix('%') {
                p.parse::<f32>().ok().map(|v| v / 100.0)
            } else {
                a.parse::<f32>().ok()
            }
        })?;
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
        let (r, g, b) = match f {
            "rgb" | "rgba" => {
                let ch = |s: &str| -> Option<u32> {
                    let v = if let Some(p) = s.strip_suffix('%') {
                        p.parse::<f32>().ok()? * 2.55
                    } else {
                        s.parse::<f32>().ok()?
                    };
                    Some(v.round().clamp(0.0, 255.0) as u32)
                };
                (ch(parts[0])?, ch(parts[1])?, ch(parts[2])?)
            }
            "hsl" | "hsla" => {
                let h = parts[0].trim_end_matches("deg").parse::<f32>().ok()? / 360.0;
                let s = parts[1].trim_end_matches('%').parse::<f32>().ok()? / 100.0;
                let l = parts[2].trim_end_matches('%').parse::<f32>().ok()? / 100.0;
                hsl(h.rem_euclid(1.0), s.clamp(0.0, 1.0), l.clamp(0.0, 1.0))
            }
            _ => return None,
        };
        return Some(a << 24 | r << 16 | g << 8 | b);
    }
    if lower == "transparent" {
        return Some(0);
    }
    named_color(&lower).map(|c| 0xFF00_0000 | c)
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

fn font_family(v: &str) -> Value {
    let first = v.split(',').next().unwrap_or("").trim().trim_matches(|c| c == '"' || c == '\'');
    let f = first.to_ascii_lowercase();
    let kw = if f.contains("mono") || f.contains("courier") || f.contains("consol") || f.contains("menlo") || f == "code" {
        Kw::Monospace
    } else if f == "serif" || f.contains("times") || f.contains("georgia") || f.contains("garamond") || f.contains("palatino") || f.contains("book") || f.contains("cambria") {
        Kw::Serif
    } else {
        Kw::SansSerif
    };
    Value::Kw(kw)
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
pub fn parse_property(name: &str, value: &str, important: bool, out: &mut Vec<Decl>) {
    let lower = value.trim().to_ascii_lowercase();
    let v = lower.as_str();
    macro_rules! push {
        ($p:expr, $v:expr) => {
            out.push(Decl { prop: $p, value: $v, important })
        };
    }

    let global = match v {
        "inherit" => Some(Value::Inherit),
        "initial" | "unset" | "revert" | "revert-layer" => Some(Value::Initial),
        _ => None,
    };
    if let Some(g) = global {
        let props: &[Prop] = match name {
            "margin" => &MARGIN,
            "padding" => &PADDING,
            "border" => &[
                BorderTopWidth, BorderRightWidth, BorderBottomWidth, BorderLeftWidth,
                BorderTopColor, BorderRightColor, BorderBottomColor, BorderLeftColor,
                BorderTopStyle, BorderRightStyle, BorderBottomStyle, BorderLeftStyle,
            ],
            "background" | "background-color" => &[Background],
            "font" => &[FontSize, FontWeight, FontStyle, FontFamily, LineHeight],
            _ => match simple_prop(name) {
                Some(p) => {
                    push!(p, g);
                    return;
                }
                None => return,
            },
        };
        for &p in props {
            push!(p, g);
        }
        return;
    }

    if parse_more(name, v, important, out) {
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
        "width" | "height" | "min-width" | "min-height" => length(v),
        "max-width" | "max-height" => if v == "none" { Some(Value::Auto) } else { length(v) },
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
fn parse_more(name: &str, v: &str, important: bool, out: &mut Vec<Decl>) -> bool {
    macro_rules! push {
        ($p:expr, $v:expr) => {
            out.push(Decl { prop: $p, value: $v, important })
        };
    }
    let kw = Value::Kw;
    match name {
        "display" => {
            let k = match v {
                "flex" | "-webkit-flex" | "-ms-flexbox" | "-webkit-box" | "block flex" => Kw::Flex,
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
            let k = match v.split_whitespace().next().unwrap_or("") {
                "hidden" | "clip" => Kw::Hidden,
                "auto" | "scroll" | "overlay" => Kw::Scroll,
                "visible" => Kw::Visible,
                _ => return true,
            };
            push!(Overflow, kw(k));
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
        "grid-column-start" => push!(GridColumnStart, grid_line(v)),
        "grid-column-end" => push!(GridColumnEnd, grid_line(v)),
        "grid-row-start" => push!(GridRowStart, grid_line(v)),
        "grid-row-end" => push!(GridRowEnd, grid_line(v)),
        "grid-area" => {
            let parts: Vec<Value> = v.split('/').map(grid_line).collect();
            if parts.len() >= 2 {
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

pub fn parse_stylesheet(src: &str, media: &Media) -> Stylesheet {
    let mut sheet = Stylesheet::default();
    let s = strip_comments(src);
    parse_rules(&s, &mut sheet, media);
    sheet
}

fn parse_rules(s: &str, sheet: &mut Stylesheet, media: &Media) {
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
            if b[pos] == b';' {
                i = pos + 1;
                continue;
            }
            let end = match_brace(b, pos);
            let prelude = &s[j..pos];
            let inner = &s[pos + 1..end.min(n)];
            match name.as_str() {
                "media" if eval_media(prelude, media) => parse_rules(inner, sheet, media),
                "supports" if !prelude.contains("not") => parse_rules(inner, sheet, media),
                "layer" | "container" | "scope" | "document" | "-moz-document" => {
                    parse_rules(inner, sheet, media)
                }
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
        let sels = parse_selector_list(prelude);
        if sels.is_empty() {
            continue;
        }
        let start = sheet.decls.len() as u32;
        parse_declarations(body, &mut sheet.decls, &mut sheet.raws);
        let end = sheet.decls.len() as u32;
        if end > start {
            for sel in sels {
                sheet.rules.push(StyleRule { sel, start, end });
            }
        }
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
    let (neg, p) = match p.strip_prefix("not ") {
        Some(r) => (true, r.trim()),
        None => (false, p),
    };
    let p = p.strip_prefix("only ").unwrap_or(p).trim();
    let ty = p.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("");
    let mut ok = matches!(ty, "" | "screen" | "all" | "and");
    let mut rest = p;
    while ok {
        let Some(open) = rest.find('(') else { break };
        let Some(close) = rest[open..].find(')') else { break };
        let f = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        let r = if let Some((name, val)) = f.split_once(':') {
            let val = val.trim();
            match name.trim() {
                "min-width" => m.vw >= media_px(val),
                "max-width" => m.vw <= media_px(val),
                "min-height" => m.vh >= media_px(val),
                "max-height" => m.vh <= media_px(val),
                "prefers-color-scheme" => val == "light",
                "prefers-reduced-motion" => val == "no-preference",
                "prefers-contrast" => val == "no-preference",
                "orientation" => (val == "landscape") == (m.vw >= m.vh),
                "hover" | "any-hover" => val == "hover",
                "pointer" | "any-pointer" => val == "fine",
                "min-resolution" | "-webkit-min-device-pixel-ratio" | "min--moz-device-pixel-ratio" => {
                    media_px(val) <= 1.0
                }
                _ => true,
            }
        } else if f.contains('<') || f.contains('>') {
            // Range syntax: (width >= 600px), (400px < width < 800px)
            let dim = if f.contains("height") { m.vh } else { m.vw };
            let norm = f.replace("width", " W ").replace("height", " W ");
            let toks: Vec<&str> = norm.split_whitespace().collect();
            let mut res = true;
            for w in toks.windows(3) {
                let (a, op, b) = (w[0], w[1], w[2]);
                let (l, r) = match (a, b) {
                    ("W", v) => (dim, media_px(v)),
                    (v, "W") => (media_px(v), dim),
                    _ => continue,
                };
                res &= match op {
                    ">=" => l >= r,
                    "<=" => l <= r,
                    ">" => l > r,
                    "<" => l < r,
                    "=" => l == r,
                    _ => true,
                };
            }
            res
        } else {
            true
        };
        ok &= r;
    }
    ok != neg
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
                    Pseudo::Never
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
                        "not" => parse_compound_list(arg).map_or(Pseudo::Never, Pseudo::Not),
                        "is" | "where" | "matches" | "-webkit-any" | "-moz-any" => {
                            parse_compound_list(arg).map_or(Pseudo::Never, Pseudo::Is)
                        }
                        "enabled" | "defined" => continue,
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
    let mut spec = 0u32;
    for c in &comps {
        if c.id.is_some() {
            spec += 1 << 16;
        }
        spec += ((c.classes.len() + c.attrs.len() + c.pseudos.len()) as u32) << 8;
        if c.tag.is_some() {
            spec += 1;
        }
    }
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
        );
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.decls.len(), 5 + 1);
        let r = &s.rules[0];
        assert_eq!(r.sel.comps.len(), 2);
        assert_eq!(r.sel.combs, vec![Comb::Child]);
        assert_eq!(r.sel.spec, (1 << 8) + 2);
    }
}
