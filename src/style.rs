//! Style resolution: selector matching + cascade + inheritance.
//!
//! Speed tricks:
//!  * rules are bucketed by the subject's id / class / tag, so each element
//!    only tests rules that could possibly match;
//!  * a 256-bit bloom filter of ancestor atoms rejects most descendant
//!    selectors without walking up the tree (as in Blink/Servo);
//!  * the DOM arena guarantees parent index < child index, so styles are
//!    computed in a single linear pass with no recursion or traversal stack.

use crate::atom::{k, Atom};
use crate::css::{self, *};
use crate::dom::*;
use crate::fx::FxHashMap;
use std::rc::Rc;

/// Custom properties visible on an element (shared with the parent until modified).
pub type Vars = FxHashMap<Box<str>, Rc<str>>;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Len {
    Px(f32),
    Pct(f32),
    /// calc(pct% + px)
    Calc(f32, f32),
    Auto,
}

impl Len {
    #[inline]
    pub fn px(self, base: f32) -> f32 {
        match self {
            Len::Px(v) => v,
            Len::Pct(p) => p * base / 100.0,
            Len::Calc(p, x) => p * base / 100.0 + x,
            Len::Auto => 0.0,
        }
    }
    #[inline]
    pub fn is_auto(self) -> bool {
        self == Len::Auto
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Display {
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WhiteSpace {
    Normal,
    Pre,
    PreWrap,
    PreLine,
    NoWrap,
}

impl WhiteSpace {
    #[inline]
    pub fn preserves(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }
    #[inline]
    pub fn wraps(self) -> bool {
        !matches!(self, WhiteSpace::Pre | WhiteSpace::NoWrap)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Sans,
    Serif,
    Mono,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListStyle {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VAlign {
    Baseline,
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transform {
    None,
    Upper,
    Lower,
    Capitalize,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineHeight {
    Normal,
    Px(f32),
    Mult(f32),
}

#[derive(Clone, Debug)]
pub struct Style {
    pub display: Display,
    pub out_of_flow: bool,
    pub color: u32,
    pub bg: u32,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub family: Family,
    pub align: Align,
    /// text-align: -webkit-center (from <center> / align=center) also centers child blocks.
    pub center_blocks: bool,
    pub line_height: LineHeight,
    pub ws: WhiteSpace,
    pub underline: bool,
    pub strike: bool,
    pub transform: Transform,
    pub list: ListStyle,
    pub hidden: bool,
    pub valign: VAlign,
    pub border_box: bool,
    pub collapse: bool,
    pub spacing: f32,
    pub width: Len,
    pub height: Len,
    pub min_w: Len,
    pub max_w: Len,
    pub min_h: Len,
    pub max_h: Len,
    pub margin: [Len; 4],
    pub padding: [Len; 4],
    pub bw: [f32; 4],
    pub bc: [u32; 4],
    pub bs: [bool; 4],
    pub vars: Option<Rc<Vars>>,
}

pub const INITIAL: Style = Style {
    display: Display::Inline,
    out_of_flow: false,
    color: 0xFF00_0000,
    bg: 0,
    font_size: 16.0,
    bold: false,
    italic: false,
    family: Family::Sans,
    align: Align::Left,
    center_blocks: false,
    line_height: LineHeight::Normal,
    ws: WhiteSpace::Normal,
    underline: false,
    strike: false,
    transform: Transform::None,
    list: ListStyle::Disc,
    hidden: false,
    valign: VAlign::Baseline,
    border_box: false,
    collapse: false,
    spacing: 2.0,
    width: Len::Auto,
    height: Len::Auto,
    min_w: Len::Px(0.0),
    max_w: Len::Auto,
    min_h: Len::Px(0.0),
    max_h: Len::Auto,
    margin: [Len::Px(0.0); 4],
    padding: [Len::Px(0.0); 4],
    bw: [3.0; 4],
    bc: [CURRENT_COLOR; 4],
    bs: [false; 4],
    vars: None,
};

impl Style {
    /// A fresh style inheriting the inherited properties of `p`.
    #[inline]
    fn inherit(p: &Style) -> Style {
        Style {
            color: p.color,
            font_size: p.font_size,
            bold: p.bold,
            italic: p.italic,
            family: p.family,
            align: p.align,
            center_blocks: p.center_blocks,
            line_height: p.line_height,
            ws: p.ws,
            underline: p.underline,
            strike: p.strike,
            transform: p.transform,
            list: p.list,
            hidden: p.hidden,
            collapse: p.collapse,
            spacing: p.spacing,
            vars: p.vars.clone(),
            ..INITIAL
        }
    }

    /// Used line-height in px (Normal resolved by the font metrics).
    pub fn line_height_px(&self, normal_factor: f32) -> f32 {
        match self.line_height {
            LineHeight::Normal => self.font_size * normal_factor,
            LineHeight::Px(p) => p,
            LineHeight::Mult(m) => m * self.font_size,
        }
    }
}

fn copy_prop(d: &mut Style, s: &Style, p: Prop) {
    use Prop::*;
    match p {
        Display => d.display = s.display,
        Position => d.out_of_flow = s.out_of_flow,
        Color => d.color = s.color,
        Background => d.bg = s.bg,
        FontSize => d.font_size = s.font_size,
        FontWeight => d.bold = s.bold,
        FontStyle => d.italic = s.italic,
        FontFamily => d.family = s.family,
        TextAlign => {
            d.align = s.align;
            d.center_blocks = s.center_blocks
        }
        LineHeight => d.line_height = s.line_height,
        WhiteSpace => d.ws = s.ws,
        TextDecoration => {
            d.underline = s.underline;
            d.strike = s.strike
        }
        TextTransform => d.transform = s.transform,
        ListStyleType => d.list = s.list,
        Visibility => d.hidden = s.hidden,
        VerticalAlign => d.valign = s.valign,
        BoxSizing => d.border_box = s.border_box,
        BorderCollapse => d.collapse = s.collapse,
        BorderSpacing => d.spacing = s.spacing,
        Width => d.width = s.width,
        Height => d.height = s.height,
        MinWidth => d.min_w = s.min_w,
        MaxWidth => d.max_w = s.max_w,
        MinHeight => d.min_h = s.min_h,
        MaxHeight => d.max_h = s.max_h,
        MarginTop | MarginRight | MarginBottom | MarginLeft => {
            let i = p as usize - MarginTop as usize;
            d.margin[i] = s.margin[i]
        }
        PaddingTop | PaddingRight | PaddingBottom | PaddingLeft => {
            let i = p as usize - PaddingTop as usize;
            d.padding[i] = s.padding[i]
        }
        BorderTopWidth | BorderRightWidth | BorderBottomWidth | BorderLeftWidth => {
            let i = p as usize - BorderTopWidth as usize;
            d.bw[i] = s.bw[i]
        }
        BorderTopColor | BorderRightColor | BorderBottomColor | BorderLeftColor => {
            let i = p as usize - BorderTopColor as usize;
            d.bc[i] = s.bc[i]
        }
        BorderTopStyle | BorderRightStyle | BorderBottomStyle | BorderLeftStyle => {
            let i = p as usize - BorderTopStyle as usize;
            d.bs[i] = s.bs[i]
        }
        Custom | Unresolved => {}
    }
}

struct Ctx {
    root_fs: f32,
    vw: f32,
    vh: f32,
}

impl Ctx {
    #[inline]
    fn px(&self, v: Value, fs: f32) -> Option<f32> {
        Some(match v {
            Value::Px(p) => p,
            Value::Em(e) => e * fs,
            Value::Rem(r) => r * self.root_fs,
            Value::Vw(x) => x * self.vw / 100.0,
            Value::Vh(x) => x * self.vh / 100.0,
            Value::Num(n) if n == 0.0 => 0.0,
            Value::Calc(c) if c[0] == 0.0 => self.calc_px(c, fs),
            _ => return None,
        })
    }

    #[inline]
    fn calc_px(&self, c: [f32; 6], fs: f32) -> f32 {
        c[1] + c[2] * fs + c[3] * self.root_fs + c[4] * self.vw / 100.0 + c[5] * self.vh / 100.0
    }
    #[inline]
    fn len(&self, v: Value, fs: f32) -> Option<Len> {
        match v {
            Value::Pct(p) => Some(Len::Pct(p)),
            Value::Auto => Some(Len::Auto),
            Value::Calc(c) if c[0] != 0.0 => Some(Len::Calc(c[0], self.calc_px(c, fs))),
            _ => self.px(v, fs).map(Len::Px),
        }
    }
}

fn apply(s: &mut Style, ps: &Style, d: &Decl, c: &Ctx, font_pass: bool) {
    use Prop::*;
    if (d.prop == FontSize) != font_pass {
        return;
    }
    let v = d.value;
    match v {
        Value::Inherit => return copy_prop(s, ps, d.prop),
        Value::Initial => return copy_prop(s, &INITIAL, d.prop),
        _ => {}
    }
    let fs = s.font_size;
    let kw = if let Value::Kw(k) = v { Some(k) } else { None };
    match d.prop {
        FontSize => {
            s.font_size = match v {
                Value::Pct(p) => p * ps.font_size / 100.0,
                Value::Em(e) => e * ps.font_size,
                _ => match c.px(v, ps.font_size) {
                    Some(p) => p,
                    None => return,
                },
            }
            .max(0.0)
        }
        Display => {
            if let Some(k) = kw {
                s.display = match k {
                    Kw::None => self::Display::None,
                    Kw::Block => self::Display::Block,
                    Kw::Inline => self::Display::Inline,
                    Kw::InlineBlock => self::Display::InlineBlock,
                    Kw::ListItem => self::Display::ListItem,
                    Kw::Table => self::Display::Table,
                    Kw::InlineTable => self::Display::InlineTable,
                    Kw::TableRow => self::Display::TableRow,
                    Kw::TableCell => self::Display::TableCell,
                    Kw::TableRowGroup => self::Display::TableRowGroup,
                    Kw::TableCaption => self::Display::TableCaption,
                    Kw::Contents => self::Display::Contents,
                    _ => return,
                }
            }
        }
        Position => s.out_of_flow = kw == Some(Kw::Out),
        Color => {
            if let Value::Color(col) = v {
                s.color = if col == CURRENT_COLOR { ps.color } else { col }
            }
        }
        Background => {
            if let Value::Color(col) = v {
                s.bg = col
            }
        }
        FontWeight => s.bold = kw == Some(Kw::Bold),
        FontStyle => s.italic = kw == Some(Kw::Italic),
        FontFamily => {
            s.family = match kw {
                Some(Kw::Monospace) => Family::Mono,
                Some(Kw::Serif) => Family::Serif,
                _ => Family::Sans,
            }
        }
        TextAlign => {
            s.align = match kw {
                Some(Kw::Right) => Align::Right,
                Some(Kw::Center | Kw::WebkitCenter) => Align::Center,
                _ => Align::Left,
            };
            s.center_blocks = kw == Some(Kw::WebkitCenter);
        }
        LineHeight => {
            s.line_height = match v {
                Value::Kw(_) => self::LineHeight::Normal,
                Value::Num(n) => self::LineHeight::Mult(n),
                Value::Pct(p) => self::LineHeight::Px(p * fs / 100.0),
                _ => match c.px(v, fs) {
                    Some(p) => self::LineHeight::Px(p),
                    None => return,
                },
            }
        }
        WhiteSpace => {
            s.ws = match kw {
                Some(Kw::Pre) => self::WhiteSpace::Pre,
                Some(Kw::PreWrap) => self::WhiteSpace::PreWrap,
                Some(Kw::PreLine) => self::WhiteSpace::PreLine,
                Some(Kw::Nowrap) => self::WhiteSpace::NoWrap,
                _ => self::WhiteSpace::Normal,
            }
        }
        TextDecoration => {
            let (u, l) = match kw {
                Some(Kw::Underline) => (true, false),
                Some(Kw::LineThrough) => (false, true),
                Some(Kw::UnderlineLineThrough) => (true, true),
                _ => (false, false),
            };
            s.underline = u;
            s.strike = l;
        }
        TextTransform => {
            s.transform = match kw {
                Some(Kw::Uppercase) => Transform::Upper,
                Some(Kw::Lowercase) => Transform::Lower,
                Some(Kw::Capitalize) => Transform::Capitalize,
                _ => Transform::None,
            }
        }
        ListStyleType => {
            s.list = match kw {
                Some(Kw::None) => ListStyle::None,
                Some(Kw::Circle) => ListStyle::Circle,
                Some(Kw::Square) => ListStyle::Square,
                Some(Kw::Decimal) => ListStyle::Decimal,
                Some(Kw::LowerAlpha) => ListStyle::LowerAlpha,
                Some(Kw::UpperAlpha) => ListStyle::UpperAlpha,
                Some(Kw::LowerRoman) => ListStyle::LowerRoman,
                Some(Kw::UpperRoman) => ListStyle::UpperRoman,
                _ => ListStyle::Disc,
            }
        }
        Visibility => s.hidden = kw == Some(Kw::Hidden),
        VerticalAlign => {
            s.valign = match kw {
                Some(Kw::Top) => VAlign::Top,
                Some(Kw::Middle) => VAlign::Middle,
                Some(Kw::Bottom) => VAlign::Bottom,
                _ => VAlign::Baseline,
            }
        }
        BoxSizing => s.border_box = kw == Some(Kw::BorderBox),
        BorderCollapse => s.collapse = kw == Some(Kw::Collapse),
        BorderSpacing => {
            if let Some(p) = c.px(v, fs) {
                s.spacing = p
            }
        }
        Width | Height | MinWidth | MaxWidth | MinHeight | MaxHeight => {
            let Some(l) = c.len(v, fs) else { return };
            match d.prop {
                Width => s.width = l,
                Height => s.height = l,
                MinWidth => s.min_w = l,
                MaxWidth => s.max_w = l,
                MinHeight => s.min_h = l,
                _ => s.max_h = l,
            }
        }
        MarginTop | MarginRight | MarginBottom | MarginLeft => {
            if let Some(l) = c.len(v, fs) {
                s.margin[d.prop as usize - MarginTop as usize] = l
            }
        }
        PaddingTop | PaddingRight | PaddingBottom | PaddingLeft => {
            if let Some(l) = c.len(v, fs) {
                if l != Len::Auto {
                    s.padding[d.prop as usize - PaddingTop as usize] = l
                }
            }
        }
        BorderTopWidth | BorderRightWidth | BorderBottomWidth | BorderLeftWidth => {
            if let Some(p) = c.px(v, fs) {
                s.bw[d.prop as usize - BorderTopWidth as usize] = p.max(0.0)
            }
        }
        BorderTopColor | BorderRightColor | BorderBottomColor | BorderLeftColor => {
            if let Value::Color(col) = v {
                s.bc[d.prop as usize - BorderTopColor as usize] = col
            }
        }
        BorderTopStyle | BorderRightStyle | BorderBottomStyle | BorderLeftStyle => {
            s.bs[d.prop as usize - BorderTopStyle as usize] = kw == Some(Kw::Solid)
        }
        Custom | Unresolved => {}
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Pass {
    Custom,
    Font,
    Rest,
}

/// Apply one declaration for the given pass, resolving custom properties
/// and var() references.
#[allow(clippy::too_many_arguments)]
fn apply_decl(s: &mut Style, ps: &Style, d: &Decl, c: &Ctx, pass: Pass, raws: &[css::Raw], scratch: &mut Vec<Decl>) {
    match (d.prop, d.value) {
        (Prop::Custom, Value::Raw(i)) => {
            if pass == Pass::Custom {
                let (name, val) = &raws[i as usize];
                let vars = Rc::make_mut(s.vars.get_or_insert_with(Default::default));
                vars.insert(name.clone(), Rc::from(&**val));
            }
        }
        (Prop::Unresolved, Value::Raw(i)) => {
            if pass == Pass::Custom {
                return;
            }
            let (name, val) = &raws[i as usize];
            let vars = s.vars.clone();
            let lookup = |n: &str| vars.as_ref().and_then(|m| m.get(n)).map(|v| v.to_string());
            if let Some(text) = css::substitute_vars(val, &lookup, 0) {
                scratch.clear();
                css::parse_property(name, &text, d.important, scratch);
                for d in scratch.iter() {
                    apply(s, ps, d, c, pass == Pass::Font);
                }
            }
        }
        _ => {
            if pass != Pass::Custom {
                apply(s, ps, d, c, pass == Pass::Font)
            }
        }
    }
}

// ------------------------------------------------------------ cascade data

pub const UA_CSS: &str = r#"
html, address, blockquote, body, center, dialog, div, figure, figcaption, footer, form,
header, hr, legend, listing, main, p, plaintext, pre, search, xmp, article, aside,
h1, h2, h3, h4, h5, h6, hgroup, nav, section, dir, dd, dl, dt, menu, ol, ul,
fieldset, details, summary, optgroup { display: block }
head, script, style, link, meta, title, template, area, base, param, datalist, noembed,
noframes, rp, option, col, colgroup, [hidden], input[type=hidden], dialog:not([open]) { display: none }
li { display: list-item }
table { display: table; border-spacing: 2px; box-sizing: border-box }
caption { display: table-caption; text-align: center }
tr { display: table-row }
thead, tbody, tfoot { display: table-row-group }
td, th { display: table-cell; padding: 1px; vertical-align: middle }
th { font-weight: bold; text-align: center }
body { margin: 8px }
p, dl { margin: 1em 0 }
blockquote, figure { margin: 1em 40px }
h1 { font-size: 2em; margin: .67em 0; font-weight: bold }
h2 { font-size: 1.5em; margin: .83em 0; font-weight: bold }
h3 { font-size: 1.17em; margin: 1em 0; font-weight: bold }
h4 { margin: 1.33em 0; font-weight: bold }
h5 { font-size: .83em; margin: 1.67em 0; font-weight: bold }
h6 { font-size: .67em; margin: 2.33em 0; font-weight: bold }
ul, ol, menu, dir { margin: 1em 0; padding-left: 40px }
ul, menu, dir { list-style-type: disc }
ol { list-style-type: decimal }
ul ul, ol ul, ul ol, ol ol, ul menu, ol menu { margin: 0 }
ul ul, ol ul, menu ul { list-style-type: circle }
ul ul ul, ol ul ul, ul ol ul { list-style-type: square }
dd { margin-left: 40px }
pre, code, kbd, samp, tt, listing, xmp, plaintext { font-family: monospace; font-size: 13px }
pre, listing, xmp, plaintext { white-space: pre; margin: 1em 0 }
b, strong, dt { font-weight: bold }
i, em, cite, var, dfn, address { font-style: italic }
u, ins { text-decoration: underline }
s, strike, del { text-decoration: line-through }
a[href] { color: #0000ee; text-decoration: underline }
small, sub, sup { font-size: smaller }
big { font-size: larger }
center { text-align: -webkit-center }
hr { border: 1px solid #a0a0a0; margin: .5em 0; color: gray }
mark { background-color: yellow; color: black }
button, input, select, textarea { display: inline-block; font-size: 13.333px; font-family: sans-serif;
  border: 1px solid #767676; padding: 1px 4px; background-color: white; text-align: left;
  white-space: pre; text-decoration: none; color: black }
button, input[type=submit], input[type=button], input[type=reset] { background-color: #efefef; padding: 1px 6px; text-align: center }
input { width: 150px; overflow: hidden }
input[type=submit], input[type=button], input[type=reset], input[type=image] { width: auto }
input[type=checkbox], input[type=radio] { width: 13px; height: 13px; padding: 0; margin: 3px 3px 0 4px }
textarea { width: 180px; height: 36px; white-space: pre-wrap; font-family: monospace }
select { padding: 1px 2px }
fieldset { margin: 0 2px; border: 2px solid #c0c0c0; padding: .35em .75em .625em }
legend { padding: 0 2px }
iframe { border: 2px solid #c0c0c0 }
summary { display: block }
details:not([open]) > :not(summary) { display: none }
abbr[title] { text-decoration: underline }
"#;

struct CRule {
    sel: Selector,
    start: u32,
    end: u32,
    level: u8,
    order: u32,
    /// Declares custom properties (needs the extra cascade pass).
    custom: bool,
}

pub const LEVEL_UA: u8 = 0;
pub const LEVEL_AUTHOR: u8 = 1;

#[derive(Default)]
pub struct Cascade {
    rules: Vec<CRule>,
    decls: Vec<Decl>,
    raws: Vec<css::Raw>,
    by_id: FxHashMap<Atom, Vec<u32>>,
    by_class: FxHashMap<Atom, Vec<u32>>,
    by_tag: FxHashMap<Atom, Vec<u32>>,
    universal: Vec<u32>,
}

impl Cascade {
    pub fn add_sheet(&mut self, sheet: Stylesheet, level: u8) {
        let off = self.decls.len() as u32;
        let roff = self.raws.len() as u32;
        self.raws.extend(sheet.raws);
        self.decls.extend(sheet.decls.into_iter().map(|mut d| {
            if let Value::Raw(i) = d.value {
                d.value = Value::Raw(i + roff);
            }
            d
        }));
        for r in sheet.rules {
            let idx = self.rules.len() as u32;
            let subj = &r.sel.comps[0];
            let bucket = if let Some(id) = subj.id {
                self.by_id.entry(id).or_default()
            } else if let Some(&c) = subj.classes.first() {
                self.by_class.entry(c).or_default()
            } else if let Some(t) = subj.tag {
                self.by_tag.entry(t).or_default()
            } else {
                &mut self.universal
            };
            bucket.push(idx);
            let (start, end) = (r.start + off, r.end + off);
            let custom = self.decls[start as usize..end as usize].iter().any(|d| d.prop == Prop::Custom);
            self.rules.push(CRule { sel: r.sel, start, end, level, order: idx, custom });
        }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

// ------------------------------------------------------------------ bloom

type Bloom = [u64; 4];

#[inline]
fn bloom_bits(a: Atom) -> (usize, usize) {
    let h = a.0.wrapping_mul(0x9E37_79B1);
    ((h >> 24) as usize, ((h >> 16) & 0xff) as usize)
}

#[inline]
fn bloom_add(b: &mut Bloom, a: Atom) {
    let (x, y) = bloom_bits(a);
    b[x >> 6] |= 1 << (x & 63);
    b[y >> 6] |= 1 << (y & 63);
}

#[inline]
fn bloom_has(b: &Bloom, a: Atom) -> bool {
    let (x, y) = bloom_bits(a);
    (b[x >> 6] >> (x & 63)) & 1 == 1 && (b[y >> 6] >> (y & 63)) & 1 == 1
}

// --------------------------------------------------------------- matching

fn attr_match(a: &AttrSel, v: &str) -> bool {
    let lower;
    let v = if a.ci {
        lower = v.to_ascii_lowercase();
        lower.as_str()
    } else {
        v
    };
    let w = a.value.as_str();
    match a.op {
        AttrOp::Exists => true,
        AttrOp::Eq => v == w,
        AttrOp::Word => v.split_ascii_whitespace().any(|t| t == w),
        AttrOp::Prefix => !w.is_empty() && v.starts_with(w),
        AttrOp::Suffix => !w.is_empty() && v.ends_with(w),
        AttrOp::Substr => !w.is_empty() && v.contains(w),
        AttrOp::Dash => v == w || (v.starts_with(w) && v.as_bytes().get(w.len()) == Some(&b'-')),
    }
}

fn nth(a: i32, b: i32, idx: i32) -> bool {
    if a == 0 {
        idx == b
    } else {
        let d = idx - b;
        d % a == 0 && d / a >= 0
    }
}

fn match_compound(c: &Compound, doc: &Document, id: NodeId) -> bool {
    let Some(el) = doc.elem(id) else { return false };
    if let Some(t) = c.tag {
        if t != el.tag {
            return false;
        }
    }
    if let Some(i) = c.id {
        if el.id != Some(i) {
            return false;
        }
    }
    for cl in &c.classes {
        if !el.classes.contains(cl) {
            return false;
        }
    }
    for a in &c.attrs {
        match el.attr(a.name) {
            Some(v) if attr_match(a, v) => {}
            _ => return false,
        }
    }
    for p in &c.pseudos {
        let ok = match p {
            Pseudo::FirstChild => doc.prev_element(id) == NONE,
            Pseudo::LastChild => doc.next_element(id) == NONE,
            Pseudo::OnlyChild => doc.prev_element(id) == NONE && doc.next_element(id) == NONE,
            Pseudo::FirstOfType | Pseudo::LastOfType => {
                let first = matches!(p, Pseudo::FirstOfType);
                let mut s = if first { doc.prev_element(id) } else { doc.next_element(id) };
                let mut ok = true;
                while s != NONE {
                    if doc.tag(s) == Some(el.tag) {
                        ok = false;
                        break;
                    }
                    s = if first { doc.prev_element(s) } else { doc.next_element(s) };
                }
                ok
            }
            Pseudo::NthChild(a, b) | Pseudo::NthLastChild(a, b) => {
                let last = matches!(p, Pseudo::NthLastChild(..));
                let mut idx = 1;
                let mut s = if last { doc.next_element(id) } else { doc.prev_element(id) };
                while s != NONE {
                    idx += 1;
                    s = if last { doc.next_element(s) } else { doc.prev_element(s) };
                }
                nth(*a, *b, idx)
            }
            Pseudo::Link => matches!(el.tag, k::a | k::area) && el.attr(k::href).is_some(),
            Pseudo::Root => id == doc.html,
            Pseudo::Empty => doc.children(id).all(|c| match &doc.nodes[c as usize].data {
                Data::Text(t) => t.is_empty(),
                _ => false,
            }),
            Pseudo::Not(list) => !list.iter().any(|c| match_compound(c, doc, id)),
            Pseudo::Is(list) => list.iter().any(|c| match_compound(c, doc, id)),
            Pseudo::Never => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn match_from(sel: &Selector, idx: usize, doc: &Document, id: NodeId) -> bool {
    if !match_compound(&sel.comps[idx], doc, id) {
        return false;
    }
    if idx + 1 == sel.comps.len() {
        return true;
    }
    match sel.combs[idx] {
        Comb::Child => {
            let p = doc.parent(id);
            p != NONE && match_from(sel, idx + 1, doc, p)
        }
        Comb::Descendant => {
            let mut p = doc.parent(id);
            while p != NONE && doc.elem(p).is_some() {
                if match_from(sel, idx + 1, doc, p) {
                    return true;
                }
                p = doc.parent(p);
            }
            false
        }
        Comb::Next => {
            let s = doc.prev_element(id);
            s != NONE && match_from(sel, idx + 1, doc, s)
        }
        Comb::Subsequent => {
            let mut s = doc.prev_element(id);
            while s != NONE {
                if match_from(sel, idx + 1, doc, s) {
                    return true;
                }
                s = doc.prev_element(s);
            }
            false
        }
    }
}

// --------------------------------------------------- presentational hints

fn html_len(v: &str) -> Option<Value> {
    let v = v.trim();
    let digits = v.bytes().take_while(|b| b.is_ascii_digit() || *b == b'.').count();
    if digits == 0 {
        return None;
    }
    let n: f32 = v[..digits].parse().ok()?;
    Some(if v[digits..].starts_with('%') { Value::Pct(n) } else { Value::Px(n) })
}

fn hints(doc: &Document, id: NodeId, el: &Element, out: &mut Vec<Decl>) {
    use Prop::*;
    out.clear();
    let mut push = |prop: Prop, value: Value| out.push(Decl { prop, value, important: false });
    let tag = el.tag;
    for (name, val) in &el.attrs {
        match *name {
            k::bgcolor => {
                if let Some(c) = css::parse_color(val) {
                    push(Background, Value::Color(c))
                }
            }
            k::width if matches!(tag, k::table | k::td | k::th | k::img | k::hr | k::iframe | k::canvas | k::video | k::embed | k::object | k::svg | k::col) => {
                if let Some(v) = html_len(val) {
                    push(Width, v)
                }
            }
            k::height if matches!(tag, k::table | k::td | k::th | k::tr | k::img | k::iframe | k::canvas | k::video | k::embed | k::object | k::svg) => {
                if let Some(v) = html_len(val) {
                    push(Height, v)
                }
            }
            k::align => {
                let a = val.to_ascii_lowercase();
                if tag == k::table {
                    if a == "center" {
                        push(MarginLeft, Value::Auto);
                        push(MarginRight, Value::Auto);
                    }
                } else if tag != k::img {
                    let kw = match a.as_str() {
                        "center" | "middle" => Kw::WebkitCenter,
                        "right" => Kw::Right,
                        "left" => Kw::Left,
                        _ => continue,
                    };
                    push(TextAlign, Value::Kw(kw));
                }
            }
            k::valign => {
                let kw = match val.to_ascii_lowercase().as_str() {
                    "top" => Kw::Top,
                    "bottom" => Kw::Bottom,
                    "middle" | "center" => Kw::Middle,
                    _ => Kw::Baseline,
                };
                push(VerticalAlign, Value::Kw(kw));
            }
            k::color if tag == k::font => {
                if let Some(c) = css::parse_color(val) {
                    push(Color, Value::Color(c))
                }
            }
            k::text if tag == k::body => {
                if let Some(c) = css::parse_color(val) {
                    push(Color, Value::Color(c))
                }
            }
            k::size if tag == k::font => {
                let v = val.trim();
                let n = if let Some(r) = v.strip_prefix('+') {
                    3 + r.parse::<i32>().unwrap_or(0)
                } else if let Some(r) = v.strip_prefix('-') {
                    3 - r.parse::<i32>().unwrap_or(0)
                } else {
                    v.parse::<i32>().unwrap_or(3)
                };
                const SIZES: [f32; 7] = [10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0];
                push(FontSize, Value::Px(SIZES[(n.clamp(1, 7) - 1) as usize]));
            }
            k::face if tag == k::font => push(FontFamily, css_family(val)),
            k::nowrap if matches!(tag, k::td | k::th) => push(WhiteSpace, Value::Kw(Kw::Nowrap)),
            k::cellspacing if tag == k::table => {
                if let Some(Value::Px(p)) = html_len(val) {
                    push(BorderSpacing, Value::Px(p))
                }
            }
            k::border if tag == k::table => {
                let w = html_len(val).map_or(1.0, |v| if let Value::Px(p) = v { p } else { 1.0 });
                if w > 0.0 {
                    for i in 0..4 {
                        push(css::BWIDTH[i], Value::Px(w));
                        push(css::BSTYLE[i], Value::Kw(Kw::Solid));
                        push(css::BCOLOR[i], Value::Color(0xFF80_8080));
                    }
                }
            }
            _ => {}
        }
    }
    if matches!(tag, k::td | k::th) {
        // cellpadding / border on the enclosing table apply to cells.
        let mut p = doc.parent(id);
        while p != NONE {
            match doc.tag(p) {
                Some(k::table) => break,
                Some(k::tr | k::tbody | k::thead | k::tfoot | k::form) => p = doc.parent(p),
                _ => {
                    p = NONE;
                }
            }
        }
        if p != NONE {
            if let Some(Value::Px(cp)) = doc.attr(p, k::cellpadding).and_then(html_len) {
                for i in 0..4 {
                    push(css::PADDING[i], Value::Px(cp));
                }
            }
            if let Some(b) = doc.attr(p, k::border) {
                if html_len(b).map_or(true, |v| v != Value::Px(0.0)) {
                    for i in 0..4 {
                        push(css::BWIDTH[i], Value::Px(1.0));
                        push(css::BSTYLE[i], Value::Kw(Kw::Solid));
                        push(css::BCOLOR[i], Value::Color(0xFF80_8080));
                    }
                }
            }
        }
    }
}

fn css_family(v: &str) -> Value {
    let mut out = Vec::new();
    css::parse_property("font-family", v, false, &mut out);
    out.first().map_or(Value::Kw(Kw::SansSerif), |d| d.value)
}

// ---------------------------------------------------------------- compute

pub fn compute(doc: &Document, cas: &Cascade, vw: f32, vh: f32) -> Vec<Style> {
    let n = doc.nodes.len();
    let mut styles: Vec<Style> = vec![INITIAL; n];
    let mut blooms: Vec<Bloom> = vec![[0; 4]; n];
    let mut matched: Vec<u32> = Vec::with_capacity(64);
    let mut hint_decls: Vec<Decl> = Vec::new();
    let mut inline_decls: Vec<Decl> = Vec::new();
    let mut inline_raws: Vec<css::Raw> = Vec::new();
    let mut scratch: Vec<Decl> = Vec::new();
    let mut ctx = Ctx { root_fs: 16.0, vw, vh };

    for id in 1..n {
        let Data::Element(el) = &doc.nodes[id].data else { continue };
        let parent = doc.nodes[id].parent as usize;
        debug_assert!(parent < id);

        // Bloom filter of this element's ancestors-or-self.
        let mut bloom = blooms[parent];
        bloom_add(&mut bloom, el.tag);
        if let Some(i) = el.id {
            bloom_add(&mut bloom, i);
        }
        for &c in &el.classes {
            bloom_add(&mut bloom, c);
        }
        blooms[id] = bloom;
        let pb = &blooms[parent];

        // Collect matching rules from the relevant buckets.
        matched.clear();
        let mut consider = |list: &[u32]| {
            for &ri in list {
                let r = &cas.rules[ri as usize];
                if r.sel.ancestor_atoms.iter().all(|&a| bloom_has(pb, a))
                    && match_from(&r.sel, 0, doc, id as NodeId)
                {
                    matched.push(ri);
                }
            }
        };
        if let Some(i) = el.id {
            if let Some(l) = cas.by_id.get(&i) {
                consider(l);
            }
        }
        for c in &el.classes {
            if let Some(l) = cas.by_class.get(c) {
                consider(l);
            }
        }
        if let Some(l) = cas.by_tag.get(&el.tag) {
            consider(l);
        }
        consider(&cas.universal);
        matched.sort_unstable_by_key(|&ri| {
            let r = &cas.rules[ri as usize];
            (r.level, r.sel.spec, r.order)
        });

        hints(doc, id as NodeId, el, &mut hint_decls);
        inline_decls.clear();
        inline_raws.clear();
        if let Some(st) = el.attr(k::style) {
            css::parse_declarations(st, &mut inline_decls, &mut inline_raws);
        }

        let (before, after) = styles.split_at_mut(id);
        let ps = &before[parent];
        let s = &mut after[0];
        *s = Style::inherit(ps);

        // Custom properties get their own first pass, so var() can see them.
        let has_custom = matched.iter().any(|&ri| cas.rules[ri as usize].custom)
            || inline_decls.iter().any(|d| d.prop == Prop::Custom);
        let passes: &[Pass] = if has_custom { &[Pass::Custom, Pass::Font, Pass::Rest] } else { &[Pass::Font, Pass::Rest] };
        let raws = &cas.raws[..];
        for &pass in passes {
            let decls = |ri: &u32| {
                let r = &cas.rules[*ri as usize];
                (r.level, &cas.decls[r.start as usize..r.end as usize])
            };
            // normal declarations: UA, hints, author, inline
            for (_, ds) in matched.iter().map(decls).filter(|(l, _)| *l == LEVEL_UA) {
                for d in ds.iter().filter(|d| !d.important) {
                    apply_decl(s, ps, d, &ctx, pass, raws, &mut scratch);
                }
            }
            for d in &hint_decls {
                apply_decl(s, ps, d, &ctx, pass, &[], &mut scratch);
            }
            for (_, ds) in matched.iter().map(decls).filter(|(l, _)| *l != LEVEL_UA) {
                for d in ds.iter().filter(|d| !d.important) {
                    apply_decl(s, ps, d, &ctx, pass, raws, &mut scratch);
                }
            }
            for d in inline_decls.iter().filter(|d| !d.important) {
                apply_decl(s, ps, d, &ctx, pass, &inline_raws, &mut scratch);
            }
            // !important
            for (_, ds) in matched.iter().map(decls) {
                for d in ds.iter().filter(|d| d.important) {
                    apply_decl(s, ps, d, &ctx, pass, raws, &mut scratch);
                }
            }
            for d in inline_decls.iter().filter(|d| d.important) {
                apply_decl(s, ps, d, &ctx, pass, &inline_raws, &mut scratch);
            }
        }

        // Fix-ups.
        for i in 0..4 {
            if !s.bs[i] {
                s.bw[i] = 0.0;
            }
            if s.bc[i] == CURRENT_COLOR {
                s.bc[i] = s.color;
            }
        }
        if s.bg == CURRENT_COLOR {
            s.bg = s.color;
        }
        if s.out_of_flow {
            s.display = Display::None;
        }
        if id as NodeId == doc.html {
            ctx.root_fs = s.font_size;
        }
    }
    styles
}

/// Extra UA rules for documents without a doctype (as in quirks.css).
pub const QUIRKS_CSS: &str = "table { text-align: left; font-size: medium; font-weight: normal;     font-style: normal; line-height: normal; white-space: normal }";

pub fn build_cascade(author_sheets: &[String], media: &Media, quirks: bool) -> Cascade {
    let mut cas = Cascade::default();
    cas.add_sheet(css::parse_stylesheet(UA_CSS, media), LEVEL_UA);
    if quirks {
        cas.add_sheet(css::parse_stylesheet(QUIRKS_CSS, media), LEVEL_UA);
    }
    for s in author_sheets {
        cas.add_sheet(css::parse_stylesheet(s, media), LEVEL_AUTHOR);
    }
    cas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn cascade_basics() {
        let doc = html::parse(
            r#"<style>div p { color: red } #x { color: blue } .big { font-size: 2em }</style>
            <div><p>a</p><p id=x class=big>b</p></div><p>c</p>"#,
        );
        let m = Media { vw: 800.0, vh: 600.0 };
        let cas = build_cascade(&doc.styles, &m, doc.quirks);
        let st = compute(&doc, &cas, 800.0, 600.0);
        let elems = |n| doc.children(n).filter(|&c| doc.elem(c).is_some()).collect::<Vec<_>>();
        let body = elems(doc.body);
        let (div, p3) = (body[0], body[1]);
        let ps = elems(div);
        let (p1, p2) = (ps[0], ps[1]);
        assert_eq!(st[p1 as usize].color, 0xffff0000);
        assert_eq!(st[p2 as usize].color, 0xff0000ff);
        assert_eq!(st[p2 as usize].font_size, 32.0);
        assert_eq!(st[p3 as usize].color, 0xff000000);
        assert_eq!(st[p1 as usize].display, Display::Block);
    }

    #[test]
    fn custom_properties_and_calc() {
        let doc = html::parse(
            r#"<style>:root { --fg: #123456; --pad: 4px } p { color: var(--fg); padding-left: var(--pad);
               margin-left: calc(10% + 2 * var(--pad)); border-top: 1px solid var(--missing, red) }
               .x { --fg: blue }</style><p>a</p><div class=x><p>b</p></div>"#,
        );
        let m = Media { vw: 800.0, vh: 600.0 };
        let cas = build_cascade(&doc.styles, &m, doc.quirks);
        let st = compute(&doc, &cas, 800.0, 600.0);
        let ps: Vec<_> = (0..doc.nodes.len() as u32).filter(|&n| doc.tag(n) == Some(k::p)).collect();
        let (a, b) = (&st[ps[0] as usize], &st[ps[1] as usize]);
        assert_eq!(a.color, 0xff123456);
        assert_eq!(b.color, 0xff0000ff);
        assert_eq!(a.padding[3], Len::Px(4.0));
        assert_eq!(a.margin[3], Len::Calc(10.0, 8.0));
        assert_eq!(a.bc[0], 0xffff0000);
    }
}
