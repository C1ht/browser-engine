//! Style resolution: selector matching + cascade + inheritance.
//!
//! Speed tricks:
//!  * rules are bucketed by the subject's id / class / tag, so each element
//!    only tests rules that could possibly match;
//!  * a 256-bit bloom filter of ancestor atoms rejects most descendant
//!    selectors without walking up the tree (as in Blink/Servo);
//!  * styles are computed in one tree-order pass with no recursion.

use crate::atom::{k, Atom};
use crate::css::{self, *};
use crate::dom::*;
use crate::fx::FxHashMap;
use std::rc::Rc;

/// Custom properties visible on an element (shared with the parent until modified).
pub type Vars = FxHashMap<Box<str>, Rc<str>>;

/// A length. Kept at 8 bytes: the rare percentage expressions (`calc()`,
/// `min()`, `max()`, `clamp()`) live in a side table.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Len {
    Px(f32),
    Pct(f32),
    /// clamp(lo, pct% + px, hi): index into the expression table.
    Ex(u32),
    Auto,
}

#[derive(Default)]
struct LenTable {
    exprs: Vec<[f32; 4]>,
    keys: FxHashMap<[u32; 4], u32>,
}

thread_local! {
    static LENS: std::cell::RefCell<LenTable> = std::cell::RefCell::new(LenTable::default());
}

impl Len {
    #[inline]
    pub fn px(self, base: f32) -> f32 {
        match self {
            Len::Px(v) => v,
            Len::Pct(p) => p * base / 100.0,
            Len::Ex(_) => {
                let [p, x, lo, hi] = self.parts();
                (p * base / 100.0 + x).min(hi).max(lo)
            }
            Len::Auto => 0.0,
        }
    }
    #[inline]
    pub fn is_auto(self) -> bool {
        self == Len::Auto
    }
    /// clamp(lo, p% + x, hi)
    pub fn clamp(p: f32, x: f32, lo: f32, hi: f32) -> Len {
        let v = [p, x, lo, hi];
        LENS.with(|t| {
            let mut t = t.borrow_mut();
            let key = v.map(f32::to_bits);
            if let Some(&i) = t.keys.get(&key) {
                return Len::Ex(i);
            }
            let i = t.exprs.len() as u32;
            t.exprs.push(v);
            t.keys.insert(key, i);
            Len::Ex(i)
        })
    }

    pub fn calc(p: f32, x: f32) -> Len {
        Len::clamp(p, x, f32::NEG_INFINITY, f32::INFINITY)
    }

    /// [pct, px, lo, hi] of a percentage-relative length.
    pub fn parts(self) -> [f32; 4] {
        match self {
            Len::Pct(p) => [p, 0.0, f32::NEG_INFINITY, f32::INFINITY],
            Len::Ex(i) => LENS.with(|t| t.borrow().exprs[i as usize]),
            Len::Px(x) => [0.0, x, f32::NEG_INFINITY, f32::INFINITY],
            Len::Auto => [0.0, 0.0, f32::NEG_INFINITY, f32::INFINITY],
        }
    }

    /// The length if it doesn't depend on a percentage base.
    #[inline]
    pub fn fixed(self) -> Option<f32> {
        match self {
            Len::Px(v) => Some(v),
            _ => None,
        }
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
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pos {
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    None,
    Left,
    Right,
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ovf {
    Visible,
    Hidden,
    Scroll,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlexDir {
    Row,
    RowRev,
    Col,
    ColRev,
}

/// justify-content / align-content
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Justify {
    Start,
    End,
    Center,
    Between,
    Around,
    Evenly,
    Stretch,
}

/// align-items / align-self / justify-items / justify-self
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Al {
    Stretch,
    Start,
    End,
    Center,
    Baseline,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GLine {
    Auto,
    Line(i16),
    Span(i16),
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
    pub position: Pos,
    /// top, right, bottom, left
    pub inset: [Len; 4],
    pub z: Option<i32>,
    pub float: Side,
    pub clear: Side,
    /// Visible if both axes are; Scroll if either scrolls; else Hidden (clip).
    pub overflow: Ovf,
    pub ovf_x: Ovf,
    pub ovf_y: Ovf,
    pub flow_root: bool,
    /// top-left, top-right, bottom-right, bottom-left
    pub radius: [Len; 4],
    pub shadow: Option<css::Shadow>,
    pub opacity: f32,
    pub flex_dir: FlexDir,
    pub wrap: bool,
    pub justify: Justify,
    pub align_content: Justify,
    pub align_items: Al,
    pub align_self: Option<Al>,
    pub justify_items: Al,
    pub justify_self: Option<Al>,
    pub grow: f32,
    pub shrink: f32,
    pub basis: Len,
    pub order: i32,
    pub row_gap: Len,
    pub col_gap: Len,
    pub grid_cols: Option<Rc<css::TrackList>>,
    pub grid_rows: Option<Rc<css::TrackList>>,
    pub grid_auto_rows: Option<Rc<css::TrackList>>,
    /// grid-column start / end
    pub gcol: [GLine; 2],
    /// grid-row start / end
    pub grow_: [GLine; 2],
    pub grid_areas: Option<Rc<css::Areas>>,
    /// grid-area: <name> (interned atom id)
    pub garea: Option<u32>,
    pub color: u32,
    pub bg: u32,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub family: Family,
    /// font-family names (lowercase), for matching web fonts.
    pub families: Option<Rc<[Box<str>]>>,
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
    /// pointer-events: none (inherited).
    pub no_pointer: bool,
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
    pub bg_image: Option<u32>,
    /// background-size: lengths, or Ex-free markers for cover/contain via `bg_fit`.
    pub bg_size: [Len; 2],
    /// 0 = explicit/auto, 1 = cover, 2 = contain
    pub bg_fit: u8,
    pub bg_pos: [Len; 2],
    pub bg_repeat: (bool, bool),
    /// object-fit: 0 fill, 1 contain, 2 cover, 3 none, 4 scale-down
    pub object_fit: u8,
    /// `content` (None = normal / none).
    pub content: Option<Rc<[css::ContentItem]>>,
    /// Indices (into the styles vector) of this element's ::before / ::after
    /// styles; 0 = none. Pseudo-element styles follow the node styles.
    pub before: u32,
    pub after: u32,
    /// For a pseudo-element style: the element it belongs to.
    pub owner: u32,
    /// transform, translate, rotate, scale (each None if none).
    pub xform: Option<Rc<[Tf]>>,
    pub x_translate: Option<Rc<[Tf]>>,
    pub x_rotate: Option<Rc<[Tf]>>,
    pub x_scale: Option<Rc<[Tf]>>,
    pub tf_origin: [Len; 2],
    /// word-break: 0 normal, 1 break-all, 2 keep-all (inherited).
    pub word_break: u8,
    /// overflow-wrap: anywhere / break-word (inherited).
    pub wrap_anywhere: bool,
    /// text-overflow: ellipsis.
    pub ellipsis: bool,
    /// -webkit-line-clamp (0 = none).
    pub line_clamp: u16,
    /// animation-name, and whether it fills forwards with a finite count.
    pub anim_names: Option<Rc<[Rc<str>]>>,
    pub anim_fill: bool,
    pub anim_finite: bool,
}

/// A resolved transform step: a translation (percentages of the border box)
/// or a 2D matrix [a, b, c, d, e, f].
#[derive(Clone, Copy, Debug)]
pub enum Tf {
    Translate(Len, Len),
    M([f32; 6]),
}

impl Style {
    pub fn has_transform(&self) -> bool {
        self.xform.is_some() || self.x_translate.is_some() || self.x_rotate.is_some() || self.x_scale.is_some()
    }

    /// If the transform is only translations: the offset for a border box
    /// of w × h (layout applies it, so hit-testing and rects follow).
    pub fn pure_translate(&self, w: f32, h: f32) -> Option<(f32, f32)> {
        if !self.has_transform() {
            return None;
        }
        let (mut x, mut y) = (0.0, 0.0);
        for op in [&self.x_translate, &self.x_rotate, &self.x_scale, &self.xform].iter().filter_map(|l| l.as_ref()).flat_map(|l| l.iter()) {
            match *op {
                Tf::Translate(tx, ty) => {
                    x += tx.px(w);
                    y += ty.px(h);
                }
                Tf::M(m) if m[0] == 1.0 && m[1] == 0.0 && m[2] == 0.0 && m[3] == 1.0 => {
                    x += m[4];
                    y += m[5];
                }
                Tf::M(_) => return None,
            }
        }
        Some((x, y))
    }
}

pub const INITIAL: Style = Style {
    display: Display::Inline,
    position: Pos::Static,
    inset: [Len::Auto; 4],
    z: None,
    float: Side::None,
    clear: Side::None,
    overflow: Ovf::Visible,
    ovf_x: Ovf::Visible,
    ovf_y: Ovf::Visible,
    flow_root: false,
    radius: [Len::Px(0.0); 4],
    shadow: None,
    opacity: 1.0,
    flex_dir: FlexDir::Row,
    wrap: false,
    justify: Justify::Start,
    align_content: Justify::Stretch,
    align_items: Al::Stretch,
    align_self: None,
    justify_items: Al::Stretch,
    justify_self: None,
    grow: 0.0,
    shrink: 1.0,
    basis: Len::Auto,
    order: 0,
    row_gap: Len::Px(0.0),
    col_gap: Len::Px(0.0),
    grid_cols: None,
    grid_rows: None,
    grid_auto_rows: None,
    gcol: [GLine::Auto; 2],
    grow_: [GLine::Auto; 2],
    grid_areas: None,
    garea: None,
    color: 0xFF00_0000,
    bg: 0,
    font_size: 16.0,
    bold: false,
    italic: false,
    family: Family::Sans,
    families: None,
    align: Align::Left,
    center_blocks: false,
    line_height: LineHeight::Normal,
    ws: WhiteSpace::Normal,
    underline: false,
    strike: false,
    transform: Transform::None,
    list: ListStyle::Disc,
    hidden: false,
    no_pointer: false,
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
    bg_image: None,
    bg_size: [Len::Auto; 2],
    bg_fit: 0,
    bg_pos: [Len::Pct(0.0); 2],
    bg_repeat: (true, true),
    object_fit: 0,
    content: None,
    before: 0,
    after: 0,
    owner: u32::MAX,
    xform: None,
    x_translate: None,
    x_rotate: None,
    x_scale: None,
    tf_origin: [Len::Pct(50.0), Len::Pct(50.0)],
    word_break: 0,
    wrap_anywhere: false,
    ellipsis: false,
    line_clamp: 0,
    anim_names: None,
    anim_fill: false,
    anim_finite: true,
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
            families: p.families.clone(),
            align: p.align,
            center_blocks: p.center_blocks,
            line_height: p.line_height,
            ws: p.ws,
            underline: p.underline,
            strike: p.strike,
            transform: p.transform,
            list: p.list,
            hidden: p.hidden,
            no_pointer: p.no_pointer,
            collapse: p.collapse,
            spacing: p.spacing,
            word_break: p.word_break,
            wrap_anywhere: p.wrap_anywhere,
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
        Display => {
            d.display = s.display;
            d.flow_root = s.flow_root
        }
        Position => d.position = s.position,
        InsetTop | InsetRight | InsetBottom | InsetLeft => {
            let i = p as usize - InsetTop as usize;
            d.inset[i] = s.inset[i]
        }
        RadiusTL | RadiusTR | RadiusBR | RadiusBL => {
            let i = p as usize - RadiusTL as usize;
            d.radius[i] = s.radius[i]
        }
        ZIndex => d.z = s.z,
        Float => d.float = s.float,
        Clear => d.clear = s.clear,
        Overflow => d.overflow = s.overflow,
        OverflowX => d.ovf_x = s.ovf_x,
        OverflowY => d.ovf_y = s.ovf_y,
        BoxShadow => d.shadow = s.shadow,
        Opacity => d.opacity = s.opacity,
        FlexDirection => d.flex_dir = s.flex_dir,
        FlexWrap => d.wrap = s.wrap,
        JustifyContent => d.justify = s.justify,
        AlignContent => d.align_content = s.align_content,
        AlignItems => d.align_items = s.align_items,
        AlignSelf => d.align_self = s.align_self,
        JustifyItems => d.justify_items = s.justify_items,
        JustifySelf => d.justify_self = s.justify_self,
        FlexGrow => d.grow = s.grow,
        FlexShrink => d.shrink = s.shrink,
        FlexBasis => d.basis = s.basis,
        Order => d.order = s.order,
        RowGap => d.row_gap = s.row_gap,
        ColumnGap => d.col_gap = s.col_gap,
        GridTemplateColumns => d.grid_cols = s.grid_cols.clone(),
        GridTemplateRows => d.grid_rows = s.grid_rows.clone(),
        GridAutoRows => d.grid_auto_rows = s.grid_auto_rows.clone(),
        GridColumnStart => d.gcol[0] = s.gcol[0],
        GridColumnEnd => d.gcol[1] = s.gcol[1],
        GridRowStart => d.grow_[0] = s.grow_[0],
        GridRowEnd => d.grow_[1] = s.grow_[1],
        GridTemplateAreas => d.grid_areas = s.grid_areas.clone(),
        GridArea => d.garea = s.garea,
        BackgroundImage => d.bg_image = s.bg_image,
        Content => d.content = s.content.clone(),
        TransformP => d.xform = s.xform.clone(),
        TranslateP => d.x_translate = s.x_translate.clone(),
        RotateP => d.x_rotate = s.x_rotate.clone(),
        ScaleP => d.x_scale = s.x_scale.clone(),
        TransformOriginX => d.tf_origin[0] = s.tf_origin[0],
        TransformOriginY => d.tf_origin[1] = s.tf_origin[1],
        BgSizeX | BgSizeY => {
            d.bg_size = s.bg_size;
            d.bg_fit = s.bg_fit
        }
        BgPosX => d.bg_pos[0] = s.bg_pos[0],
        BgPosY => d.bg_pos[1] = s.bg_pos[1],
        BgRepeat => d.bg_repeat = s.bg_repeat,
        ObjectFit => d.object_fit = s.object_fit,
        Color => d.color = s.color,
        Background => d.bg = s.bg,
        FontSize => d.font_size = s.font_size,
        FontWeight => d.bold = s.bold,
        FontStyle => d.italic = s.italic,
        FontFamily => {
            d.family = s.family;
            d.families = s.families.clone()
        }
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
        PointerEvents => d.no_pointer = s.no_pointer,
        VerticalAlign => d.valign = s.valign,
        BoxSizing => d.border_box = s.border_box,
        BorderCollapse => d.collapse = s.collapse,
        WordBreak => d.word_break = s.word_break,
        OverflowWrap => d.wrap_anywhere = s.wrap_anywhere,
        TextOverflow => d.ellipsis = s.ellipsis,
        LineClamp => d.line_clamp = s.line_clamp,
        AnimName => d.anim_names = s.anim_names.clone(),
        AnimFill => d.anim_fill = s.anim_fill,
        AnimIter => d.anim_finite = s.anim_finite,
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
            Value::Expr(i) => return self.expr_len(&css::expr(i), fs)?.fixed(),
            _ => return None,
        })
    }

    /// Resolve min()/max()/clamp() into a length; percentages stay symbolic.
    fn expr_len(&self, e: &css::Expr, fs: f32) -> Option<Len> {
        match e {
            css::Expr::Lin(c) => Some(if c[0] == 0.0 { Len::Px(self.calc_px(*c, fs)) } else { Len::calc(c[0], self.calc_px(*c, fs)) }),
            css::Expr::Min(args) | css::Expr::Max(args) => {
                let is_min = matches!(e, css::Expr::Min(_));
                let mut bound: Option<f32> = None;
                let mut rel: Option<Len> = None;
                for a in args {
                    match self.expr_len(a, fs)? {
                        Len::Px(v) => bound = Some(bound.map_or(v, |b| if is_min { b.min(v) } else { b.max(v) })),
                        other => {
                            rel.get_or_insert(other);
                        }
                    }
                }
                match (rel, bound) {
                    (None, b) => b.map(Len::Px),
                    (Some(r), None) => Some(r),
                    (Some(r), Some(b)) => {
                        if r == Len::Auto {
                            return Some(Len::Px(b));
                        }
                        let [p, x, lo, hi] = r.parts();
                        Some(if is_min { Len::clamp(p, x, lo, hi.min(b)) } else { Len::clamp(p, x, lo.max(b), hi) })
                    }
                }
            }
        }
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
            Value::Calc(c) if c[0] != 0.0 => Some(Len::calc(c[0], self.calc_px(c, fs))),
            Value::Expr(i) => self.expr_len(&css::expr(i), fs),
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
                    Kw::Flex => self::Display::Flex,
                    Kw::InlineFlex => self::Display::InlineFlex,
                    Kw::Grid => self::Display::Grid,
                    Kw::InlineGrid => self::Display::InlineGrid,
                    Kw::FlowRoot => self::Display::Block,
                    _ => return,
                };
                s.flow_root = k == Kw::FlowRoot;
            }
        }
        Position => {
            s.position = match kw {
                Some(Kw::Relative) => Pos::Relative,
                Some(Kw::Absolute) => Pos::Absolute,
                Some(Kw::Fixed) => Pos::Fixed,
                Some(Kw::Sticky) => Pos::Sticky,
                _ => Pos::Static,
            }
        }
        InsetTop | InsetRight | InsetBottom | InsetLeft => {
            if let Some(l) = c.len(v, fs) {
                s.inset[d.prop as usize - InsetTop as usize] = l
            }
        }
        RadiusTL | RadiusTR | RadiusBR | RadiusBL => {
            if let Some(l) = c.len(v, fs) {
                s.radius[d.prop as usize - RadiusTL as usize] = l
            }
        }
        ZIndex => {
            s.z = match v {
                Value::Num(n) => Some(n as i32),
                _ => None,
            }
        }
        Float | Clear => {
            let side = match kw {
                Some(Kw::Left) => Side::Left,
                Some(Kw::Right) => Side::Right,
                Some(Kw::Both) => Side::Both,
                _ => Side::None,
            };
            if d.prop == Float {
                s.float = side
            } else {
                s.clear = side
            }
        }
        Overflow | OverflowX | OverflowY => {
            let o = match kw {
                Some(Kw::Hidden) => Ovf::Hidden,
                Some(Kw::Scroll) => Ovf::Scroll,
                _ => Ovf::Visible,
            };
            match d.prop {
                OverflowX => s.ovf_x = o,
                OverflowY => s.ovf_y = o,
                _ => {
                    s.ovf_x = o;
                    s.ovf_y = o;
                }
            }
            // A non-visible axis makes the other one auto.
            s.overflow = match (s.ovf_x, s.ovf_y) {
                (Ovf::Visible, Ovf::Visible) => Ovf::Visible,
                (Ovf::Hidden, Ovf::Hidden) => Ovf::Hidden,
                _ => Ovf::Scroll,
            };
        }
        BoxShadow => {
            s.shadow = match v {
                Value::Shadow(mut sh) => {
                    if sh.color == CURRENT_COLOR {
                        sh.color = s.color;
                    }
                    Some(sh)
                }
                _ => None,
            }
        }
        Opacity => {
            if let Value::Num(n) = v {
                s.opacity = n
            }
        }
        FlexDirection => {
            s.flex_dir = match kw {
                Some(Kw::RowReverse) => FlexDir::RowRev,
                Some(Kw::Column) => FlexDir::Col,
                Some(Kw::ColumnReverse) => FlexDir::ColRev,
                _ => FlexDir::Row,
            }
        }
        FlexWrap => s.wrap = kw == Some(Kw::Wrap),
        JustifyContent | AlignContent => {
            let j = match kw {
                Some(Kw::End) => Justify::End,
                Some(Kw::Center) => Justify::Center,
                Some(Kw::SpaceBetween) => Justify::Between,
                Some(Kw::SpaceAround) => Justify::Around,
                Some(Kw::SpaceEvenly) => Justify::Evenly,
                Some(Kw::Stretch) => Justify::Stretch,
                _ => Justify::Start,
            };
            if d.prop == JustifyContent {
                s.justify = j
            } else {
                s.align_content = j
            }
        }
        AlignItems | AlignSelf | JustifyItems | JustifySelf => {
            let a = match kw {
                Some(Kw::Start) => Some(Al::Start),
                Some(Kw::End) => Some(Al::End),
                Some(Kw::Center) => Some(Al::Center),
                Some(Kw::Baseline) => Some(Al::Baseline),
                Some(_) => Some(Al::Stretch),
                None => None,
            };
            match d.prop {
                AlignItems => s.align_items = a.unwrap_or(Al::Stretch),
                JustifyItems => s.justify_items = a.unwrap_or(Al::Stretch),
                AlignSelf => s.align_self = a,
                _ => s.justify_self = a,
            }
        }
        FlexGrow | FlexShrink | Order => {
            if let Value::Num(n) = v {
                match d.prop {
                    FlexGrow => s.grow = n.max(0.0),
                    FlexShrink => s.shrink = n.max(0.0),
                    _ => s.order = n as i32,
                }
            }
        }
        FlexBasis | RowGap | ColumnGap => {
            if let Some(l) = c.len(v, fs) {
                match d.prop {
                    FlexBasis => s.basis = l,
                    RowGap => s.row_gap = l,
                    _ => s.col_gap = l,
                }
            }
        }
        GridTemplateColumns | GridTemplateRows | GridAutoRows => {
            let t = match v {
                Value::Tracks(i) => Some(css::track_list(i)),
                _ => None,
            };
            match d.prop {
                GridTemplateColumns => s.grid_cols = t,
                GridTemplateRows => s.grid_rows = t,
                _ => s.grid_auto_rows = t,
            }
        }
        GridTemplateAreas => {
            s.grid_areas = match v {
                Value::Areas(i) => Some(css::areas(i)),
                _ => None,
            }
        }
        GridArea => {
            s.garea = match v {
                Value::Name(n) => Some(n),
                _ => None,
            }
        }
        BackgroundImage => {
            s.bg_image = match v {
                Value::Image(i) => Some(i),
                _ => None,
            }
        }
        TransformP | TranslateP | RotateP | ScaleP => {
            let ops = match v {
                Value::Transform(i) => {
                    let ops: Vec<Tf> = css::transform_ops(i)
                        .iter()
                        .filter_map(|op| match *op {
                            css::TfOp::Translate(x, y) => Some(Tf::Translate(c.len(x, fs)?, c.len(y, fs)?)),
                            css::TfOp::Scale(a, b) => Some(Tf::M([a, 0.0, 0.0, b, 0.0, 0.0])),
                            css::TfOp::Rotate(deg) => {
                                let r = deg.to_radians();
                                Some(Tf::M([r.cos(), r.sin(), -r.sin(), r.cos(), 0.0, 0.0]))
                            }
                            css::TfOp::Skew(ax, ay) => Some(Tf::M([1.0, ay.to_radians().tan(), ax.to_radians().tan(), 1.0, 0.0, 0.0])),
                            css::TfOp::Matrix(m) => Some(Tf::M(m)),
                        })
                        .collect();
                    (!ops.is_empty()).then(|| Rc::from(ops))
                }
                _ => None,
            };
            match d.prop {
                TransformP => s.xform = ops,
                TranslateP => s.x_translate = ops,
                RotateP => s.x_rotate = ops,
                _ => s.x_scale = ops,
            }
        }
        AnimName => {
            s.anim_names = match v {
                Value::Names(i) => Some(css::name_list(i)),
                _ => None,
            }
        }
        AnimFill => s.anim_fill = matches!(v, Value::Num(n) if n > 0.0),
        AnimIter => s.anim_finite = matches!(v, Value::Num(n) if n > 0.0),
        WordBreak | OverflowWrap | TextOverflow | LineClamp => {
            if let Value::Num(n) = v {
                match d.prop {
                    WordBreak => s.word_break = n as u8,
                    OverflowWrap => s.wrap_anywhere = n > 0.0,
                    TextOverflow => s.ellipsis = n > 0.0,
                    _ => s.line_clamp = n.min(1000.0) as u16,
                }
            }
        }
        TransformOriginX | TransformOriginY => {
            if let Some(l) = c.len(v, fs) {
                s.tf_origin[(d.prop == TransformOriginY) as usize] = l;
            }
        }
        Content => {
            s.content = match v {
                Value::Content(i) => Some(css::content(i)),
                _ => None,
            }
        }
        BgSizeX | BgSizeY => {
            let i = (d.prop == BgSizeY) as usize;
            match v {
                Value::Kw(Kw::Cover) => s.bg_fit = 1,
                Value::Kw(Kw::Contain) => s.bg_fit = 2,
                _ => {
                    if let Some(l) = c.len(v, fs) {
                        s.bg_size[i] = l;
                        if i == 0 {
                            s.bg_fit = 0;
                        }
                    }
                }
            }
        }
        BgPosX | BgPosY => {
            if let Some(l) = c.len(v, fs) {
                s.bg_pos[(d.prop == BgPosY) as usize] = l
            }
        }
        BgRepeat => {
            s.bg_repeat = match kw {
                Some(Kw::NoRepeat) => (false, false),
                Some(Kw::RepeatX) => (true, false),
                Some(Kw::RepeatY) => (false, true),
                _ => (true, true),
            }
        }
        ObjectFit => {
            s.object_fit = match kw {
                Some(Kw::Contain) => 1,
                Some(Kw::Cover) => 2,
                Some(Kw::None) => 3,
                Some(Kw::ScaleDown) => 4,
                _ => 0,
            }
        }
        GridColumnStart | GridColumnEnd | GridRowStart | GridRowEnd => {
            let l = match v {
                Value::Num(n) => GLine::Line(n as i16),
                Value::Span(n) => GLine::Span(n),
                _ => GLine::Auto,
            };
            match d.prop {
                GridColumnStart => s.gcol[0] = l,
                GridColumnEnd => s.gcol[1] = l,
                GridRowStart => s.grow_[0] = l,
                _ => s.grow_[1] = l,
            }
        }
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
            let (names, generic) = match v {
                Value::Family(i) => {
                    let (n, g) = css::families(i);
                    (Some(n), Some(g))
                }
                _ => (None, kw),
            };
            s.family = match generic {
                Some(Kw::Monospace) => Family::Mono,
                Some(Kw::Serif) => Family::Serif,
                _ => Family::Sans,
            };
            s.families = names;
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
        PointerEvents => s.no_pointer = kw == Some(Kw::None),
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
/// Parsed results of var()-substituted declarations, keyed by (declaration,
/// custom-property map). Elements mostly share their parent's map, so most
/// lookups hit and the string substitution + parse happens once.
#[derive(Default)]
struct VarCache {
    map: FxHashMap<(u32, usize), Rc<[Decl]>>,
    scratch: Vec<Decl>,
}

#[allow(clippy::too_many_arguments)]
fn apply_decl(s: &mut Style, ps: &Style, d: &Decl, c: &Ctx, pass: Pass, raws: &[css::Raw], inline: bool, cache: &mut VarCache) {
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
            let key = (i, s.vars.as_ref().map_or(0, |v| Rc::as_ptr(v) as usize));
            if !inline {
                if let Some(ds) = cache.map.get(&key).cloned() {
                    for d in ds.iter() {
                        apply(s, ps, d, c, pass == Pass::Font);
                    }
                    return;
                }
            }
            let (name, val) = &raws[i as usize];
            let vars = s.vars.clone();
            let lookup = |n: &str| vars.as_ref().and_then(|m| m.get(n)).map(|v| v.to_string());
            cache.scratch.clear();
            if let Some(text) = css::substitute_vars(val, &lookup, 0) {
                css::parse_property(name, &text, d.important, &mut cache.scratch);
            }
            for d in cache.scratch.iter() {
                apply(s, ps, d, c, pass == Pass::Font);
            }
            if !inline {
                cache.map.insert(key, Rc::from(&cache.scratch[..]));
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
noframes, noscript, rp, option, col, colgroup, [hidden], input[type=hidden], dialog:not([open]) { display: none }
li { display: list-item }
slot { display: contents }
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
input[type=checkbox], input[type=radio] { width: 13px; height: 13px; padding: 0; margin: 3px 3px 0 4px;
  box-sizing: border-box; border-radius: 2px; font-size: 11px; line-height: 11px; text-align: center; color: white }
input[type=radio] { border-radius: 50% }
input[type=checkbox]:checked { background-color: #0075ff; border-color: #0075ff }
input[type=radio]:checked { border-color: #0075ff; background-image: radial-gradient(circle, #0075ff 40%, white 50%) }
input:placeholder-shown, textarea:placeholder-shown { color: #757575 !important }
input:disabled, button:disabled, select:disabled, textarea:disabled { color: #6d6d6d; border-color: #c6c6c6 }
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
    /// Has !important declarations.
    important: bool,
    /// Can affect font-size (needed in the font pass).
    font: bool,
    /// The subject is `:host...` (applies to the shadow host).
    host: bool,
    /// The subject is `::slotted(...)` (applies to slotted light-DOM nodes).
    slotted: bool,
    /// Pseudo-element the rule styles: 0 none, 1 ::before, 2 ::after.
    pe: u8,
    /// Cascade layer rank (css::UNLAYERED for unlayered rules).
    layer: u16,
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
    /// @keyframes name -> decls of its final keyframe.
    keyframes: FxHashMap<Rc<str>, (u32, u32)>,
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
        for (name, s, e) in sheet.keyframes {
            self.keyframes.insert(name, (s + off, e + off));
        }
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
            let ds = &self.decls[start as usize..end as usize];
            let custom = ds.iter().any(|d| d.prop == Prop::Custom);
            let important = ds.iter().any(|d| d.important);
            let font = ds.iter().any(|d| matches!(d.prop, Prop::FontSize | Prop::Unresolved));
            let host = r.sel.comps[0].pseudos.iter().any(|p| matches!(p, Pseudo::Host(_)));
            let slotted = r.sel.comps[0].pseudos.iter().any(|p| matches!(p, Pseudo::Slotted(_)));
            let pe = r.sel.comps[0].pseudos.iter().find_map(|p| match p {
                Pseudo::Before => Some(1),
                Pseudo::After => Some(2),
                _ => None,
            });
            self.rules.push(CRule { sel: r.sel, start, end, level, order: idx, custom, important, font, host, slotted, pe: pe.unwrap_or(0), layer: r.layer });
        }
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

// ------------------------------------------------------------------ bloom

/// 512-bit filter: 256 bits saturate on deep trees with many classes.
type Bloom = [u64; 8];

#[inline]
fn bloom_bits(a: Atom) -> (usize, usize) {
    let h = a.0.wrapping_mul(0x9E37_79B1);
    ((h >> 23) as usize, ((h >> 12) & 0x1ff) as usize)
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
            Pseudo::Focus => doc.focus == id,
            Pseudo::FocusWithin => doc.focus != NONE && doc.contains(id, doc.focus),
            Pseudo::Checked => match el.tag {
                k::input => el.attr(k::checked).is_some(),
                k::option => el.attr(k::selected).is_some(),
                _ => false,
            },
            Pseudo::Disabled => matches!(el.tag, k::input | k::button | k::select | k::textarea | k::option | k::fieldset) && el.attr(k::disabled).is_some(),
            Pseudo::PlaceholderShown => crate::forms::placeholder_shown(doc, id),
            Pseudo::Not(list) => !list.iter().any(|s| match_from(s, 0, doc, id)),
            Pseudo::Is(list, _) => list.iter().any(|s| match_from(s, 0, doc, id)),
            Pseudo::Has(list) => list.iter().any(|(comb, s)| match comb {
                Some(Comb::Child) => doc.children(id).any(|c| doc.elem(c).is_some() && match_from(s, 0, doc, c)),
                Some(Comb::Next) => {
                    let n = doc.next_element(id);
                    n != NONE && match_from(s, 0, doc, n)
                }
                Some(Comb::Subsequent) => {
                    let mut n = doc.next_element(id);
                    while n != NONE && !match_from(s, 0, doc, n) {
                        n = doc.next_element(n);
                    }
                    n != NONE
                }
                _ => has_descendant(doc, id, s),
            }),
            Pseudo::NthOfType(a, b) | Pseudo::NthLastOfType(a, b) => {
                let last = matches!(p, Pseudo::NthLastOfType(..));
                let mut idx = 1;
                let mut s = if last { doc.next_element(id) } else { doc.prev_element(id) };
                while s != NONE {
                    if doc.tag(s) == Some(el.tag) {
                        idx += 1;
                    }
                    s = if last { doc.next_element(s) } else { doc.prev_element(s) };
                }
                nth(*a, *b, idx)
            }
            Pseudo::OnlyOfType => {
                let same = |mut s: NodeId, fwd: bool| {
                    while s != NONE {
                        if doc.tag(s) == Some(el.tag) {
                            return true;
                        }
                        s = if fwd { doc.next_element(s) } else { doc.prev_element(s) };
                    }
                    false
                };
                !same(doc.prev_element(id), false) && !same(doc.next_element(id), true)
            }
            Pseudo::Hover => doc.hover != NONE && doc.contains(id, doc.hover),
            Pseudo::Active => doc.active != NONE && doc.contains(id, doc.active),
            Pseudo::Target => doc.target == id,
            Pseudo::Enabled => matches!(el.tag, k::input | k::button | k::select | k::textarea | k::option | k::fieldset) && el.attr(k::disabled).is_none(),
            Pseudo::Required | Pseudo::Optional => {
                let req = matches!(el.tag, k::input | k::select | k::textarea) && crate::atom::lookup("required").is_some_and(|a| el.attr(a).is_some());
                if matches!(p, Pseudo::Required) { req } else { matches!(el.tag, k::input | k::select | k::textarea) && !req }
            }
            Pseudo::ReadOnly | Pseudo::ReadWrite => {
                let editable = (matches!(el.tag, k::input | k::textarea) && el.attr(k::readonly).is_none() && el.attr(k::disabled).is_none())
                    || crate::atom::lookup("contenteditable").and_then(|a| el.attr(a)).is_some_and(|v| v != "false");
                editable == matches!(p, Pseudo::ReadWrite)
            }
            Pseudo::Always => true,
            Pseudo::Before | Pseudo::After => true,
            Pseudo::Host(list) => doc.shadows.contains_key(&id) && (list.is_empty() || list.iter().any(|c| match_compound(c, doc, id))),
            Pseudo::Slotted(list) => doc.shadows.contains_key(&doc.parent(id)) && list.iter().any(|c| match_compound(c, doc, id)),
            Pseudo::Never => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Does `sel` match element `id`? (used by querySelector and friends)
pub fn selector_matches(sel: &Selector, doc: &Document, id: NodeId) -> bool {
    doc.elem(id).is_some() && match_from(sel, 0, doc, id)
}

/// The parent for ancestor combinators: a shadow root leads to its host when
/// the next compound is `:host...`.
fn up(doc: &Document, n: NodeId, next: &Compound) -> NodeId {
    let p = doc.parent(n);
    if p != NONE && doc.elem(p).is_none() {
        if let Some(&h) = doc.shadow_hosts.get(&p) {
            if next.pseudos.iter().any(|x| matches!(x, Pseudo::Host(_))) {
                return h;
            }
        }
    }
    p
}

thread_local! {
    /// Shadow-tree cascades by (stylesheet text hash, viewport).
    static SHADOW_CASCADES: std::cell::RefCell<FxHashMap<(u64, u32, u32), Rc<Cascade>>> = std::cell::RefCell::new(FxHashMap::default());
}

/// The cascade for a shadow tree: its `<style>` elements and adopted sheets.
fn shadow_cascade(doc: &Document, root: NodeId, media: &Media) -> Rc<Cascade> {
    use std::hash::{Hash, Hasher};
    let mut text = String::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if doc.tag(n) == Some(k::style) {
            let mut s = String::new();
            doc.text_content(n, &mut s);
            match doc.cssom.get(&n) {
                Some(d) if d.init && d.base == s => text.push_str(&d.rules.join("\n")),
                _ => text.push_str(&s),
            }
            text.push('\n');
            continue;
        }
        let mut c = doc.nodes[n as usize].last_child;
        while c != NONE {
            stack.push(c);
            c = doc.nodes[c as usize].prev;
        }
    }
    for &i in doc.shadow_adopted.get(&root).into_iter().flatten() {
        if let Some(r) = doc.free_sheets.get(i) {
            text.push_str(&r.join("\n"));
            text.push('\n');
        }
    }
    let mut h = crate::fx::FxHasher::default();
    text.hash(&mut h);
    let key = (h.finish(), media.vw.to_bits(), media.vh.to_bits());
    SHADOW_CASCADES.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(cas) = c.get(&key) {
            return cas.clone();
        }
        if c.len() > 512 {
            c.clear();
        }
        let cas = Rc::new(build_cascade(&[(text, String::new())], media, false));
        c.insert(key, cas.clone());
        cas
    })
}

fn has_descendant(doc: &Document, root: NodeId, s: &Selector) -> bool {
    let mut stack: Vec<NodeId> = doc.children(root).collect();
    while let Some(n) = stack.pop() {
        if doc.elem(n).is_some() {
            if match_from(s, 0, doc, n) {
                return true;
            }
            stack.extend(doc.children(n));
        }
    }
    false
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
            let p = up(doc, id, &sel.comps[idx + 1]);
            p != NONE && match_from(sel, idx + 1, doc, p)
        }
        Comb::Descendant => {
            let mut p = up(doc, id, &sel.comps[idx + 1]);
            while p != NONE && doc.elem(p).is_some() {
                if match_from(sel, idx + 1, doc, p) {
                    return true;
                }
                p = up(doc, p, &sel.comps[idx + 1]);
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

/// Apply matched rules (sorted) to a pseudo-element style.
fn apply_matched(s: &mut Style, ps: &Style, cas: &Cascade, matched: &[u32], ctx: &Ctx, scratch: &mut VarCache) {
    let has_custom = matched.iter().any(|&ri| cas.rules[ri as usize].custom);
    let passes: &[Pass] = if has_custom { &[Pass::Custom, Pass::Font, Pass::Rest] } else { &[Pass::Font, Pass::Rest] };
    for &pass in passes {
        let font_pass = pass == Pass::Font;
        for important in [false, true] {
            for &ri in matched {
                let r = &cas.rules[ri as usize];
                if font_pass && !r.font {
                    continue;
                }
                for d in cas.decls[r.start as usize..r.end as usize].iter().filter(|d| d.important == important) {
                    apply_decl(s, ps, d, ctx, pass, &cas.raws, false, scratch);
                }
            }
        }
    }
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
}

pub fn compute(doc: &Document, cas: &Cascade, vw: f32, vh: f32) -> Vec<Style> {
    let n = doc.nodes.len();
    let mut styles: Vec<Style> = vec![INITIAL; n];
    // ::before / ::after styles, appended after the node styles.
    let mut pseudo_styles: Vec<Style> = Vec::new();
    let mut pe_matched: Vec<(u8, u32)> = Vec::new();
    let mut blooms: Vec<Bloom> = vec![[0; 8]; n];
    let mut matched: Vec<u32> = Vec::with_capacity(64);
    let mut hint_decls: Vec<Decl> = Vec::new();
    let mut inline_decls: Vec<Decl> = Vec::new();
    let mut inline_raws: Vec<css::Raw> = Vec::new();
    let mut scratch = VarCache::default();
    let mut ctx = Ctx { root_fs: 16.0, vw, vh };

    // Shadow trees are styled by their own cascades.
    let media = Media { vw, vh };
    let scoped: FxHashMap<NodeId, Rc<Cascade>> = doc.shadows.values().map(|&sr| (sr, shadow_cascade(doc, sr, &media))).collect();
    let doc_cas = cas;

    // Flat tree order: parents (and slots) before children; scripts may
    // have moved nodes, so arena order isn't enough.
    for (id, fparent, tree) in doc.flat_preorder() {
        let id = id as usize;
        let Data::Element(el) = &doc.nodes[id].data else { continue };
        let cas: &Cascade = if tree == 0 { doc_cas } else { scoped.get(&tree).map_or(doc_cas, |c| &**c) };
        // Selectors see the DOM parent; inheritance follows the flat tree.
        let dom_parent = doc.nodes[id].parent;
        let parent = if fparent == NONE { 0 } else { fparent as usize };
        let bp = if dom_parent == NONE || dom_parent as usize >= blooms.len() { 0 } else { dom_parent as usize };

        // Bloom filter of this element's ancestors-or-self.
        let mut bloom = if dom_parent == NONE { [0; 8] } else { blooms[bp] };
        bloom_add(&mut bloom, el.tag);
        if let Some(i) = el.id {
            bloom_add(&mut bloom, i);
        }
        for &c in &el.classes {
            bloom_add(&mut bloom, c);
        }
        blooms[id] = bloom;
        let pb = &blooms[bp];

        // Collect matching rules from the relevant buckets.
        matched.clear();
        pe_matched.clear();
        let mut consider = |list: &[u32]| {
            for &ri in list {
                let r = &cas.rules[ri as usize];
                if r.sel.ancestor_atoms.iter().all(|&a| bloom_has(pb, a))
                    && match_from(&r.sel, 0, doc, id as NodeId)
                {
                    if r.pe == 0 {
                        matched.push(ri);
                    } else {
                        pe_matched.push((r.pe, ri));
                    }
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
            (r.level, r.layer, r.sel.spec, r.order)
        });

        // `:host` rules of this element's shadow tree and `::slotted` rules
        // of the tree it is slotted into apply before the author's rules.
        let mut extra: Vec<(&Cascade, u32)> = Vec::new();
        if let Some(sc) = doc.shadows.get(&(id as NodeId)).and_then(|sr| scoped.get(sr)) {
            for (ri, r) in sc.rules.iter().enumerate() {
                if r.host && match_from(&r.sel, 0, doc, id as NodeId) {
                    extra.push((sc, ri as u32));
                }
            }
        }
        if dom_parent != NONE && doc.shadows.contains_key(&dom_parent) {
            let slot = doc.assigned_slot(id as NodeId);
            if let Some(sc) = (slot != NONE).then(|| doc.shadow_root_of(slot)).and_then(|sr| scoped.get(&sr)) {
                for (ri, r) in sc.rules.iter().enumerate() {
                    if r.slotted && match_from(&r.sel, 0, doc, id as NodeId) {
                        extra.push((sc, ri as u32));
                    }
                }
            }
        }
        extra.sort_by_key(|(c, ri)| {
            let r = &c.rules[*ri as usize];
            (r.sel.spec, r.order)
        });

        hints(doc, id as NodeId, el, &mut hint_decls);
        inline_decls.clear();
        inline_raws.clear();
        if let Some(st) = el.attr(k::style) {
            css::parse_declarations(st, &mut inline_decls, &mut inline_raws);
        }

        let ps = &styles[parent];
        let mut own = Style::inherit(ps);
        let s = &mut own;

        // Custom properties get their own first pass, so var() can see them.
        let has_custom = matched.iter().any(|&ri| cas.rules[ri as usize].custom)
            || extra.iter().any(|(c, ri)| c.rules[*ri as usize].custom)
            || inline_decls.iter().any(|d| d.prop == Prop::Custom);
        let passes: &[Pass] = if has_custom { &[Pass::Custom, Pass::Font, Pass::Rest] } else { &[Pass::Font, Pass::Rest] };
        let raws = &cas.raws[..];
        for &pass in passes {
            let font_pass = pass == Pass::Font;
            let rules = || matched.iter().map(|&ri| &cas.rules[ri as usize]).filter(|r| !font_pass || r.font);
            let decls = |r: &CRule| (r.level, &cas.decls[r.start as usize..r.end as usize]);
            // normal declarations: UA, hints, author, inline
            for (_, ds) in rules().map(decls).filter(|(l, _)| *l == LEVEL_UA) {
                for d in ds.iter().filter(|d| !d.important) {
                    apply_decl(s, ps, d, &ctx, pass, raws, false, &mut scratch);
                }
            }
            for d in &hint_decls {
                apply_decl(s, ps, d, &ctx, pass, &[], true, &mut scratch);
            }
            for (c, ri) in &extra {
                let r = &c.rules[*ri as usize];
                if font_pass && !r.font {
                    continue;
                }
                for d in c.decls[r.start as usize..r.end as usize].iter().filter(|d| !d.important) {
                    apply_decl(s, ps, d, &ctx, pass, &c.raws, false, &mut scratch);
                }
            }
            for (_, ds) in rules().map(decls).filter(|(l, _)| *l != LEVEL_UA) {
                for d in ds.iter().filter(|d| !d.important) {
                    apply_decl(s, ps, d, &ctx, pass, raws, false, &mut scratch);
                }
            }
            for d in inline_decls.iter().filter(|d| !d.important) {
                apply_decl(s, ps, d, &ctx, pass, &inline_raws, true, &mut scratch);
            }
            // The end state of animations that fill forwards.
            if pass == Pass::Rest && s.anim_fill && s.anim_finite {
                if let Some(names) = s.anim_names.clone() {
                    for n in names.iter() {
                        if let Some(&(a, b)) = cas.keyframes.get(n) {
                            for d in &cas.decls[a as usize..b as usize] {
                                apply_decl(s, ps, d, &ctx, pass, raws, false, &mut scratch);
                            }
                        }
                    }
                }
            }
            // !important
            for (_, ds) in rules().filter(|r| r.important).map(decls) {
                for d in ds.iter().filter(|d| d.important) {
                    apply_decl(s, ps, d, &ctx, pass, raws, false, &mut scratch);
                }
            }
            for d in inline_decls.iter().filter(|d| d.important) {
                apply_decl(s, ps, d, &ctx, pass, &inline_raws, true, &mut scratch);
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
        if id as NodeId == doc.html {
            ctx.root_fs = s.font_size;
        }
        styles[id] = own;

        // ::before / ::after
        for pe in [1u8, 2] {
            let mut list: Vec<u32> = pe_matched.iter().filter(|(p, _)| *p == pe).map(|(_, r)| *r).collect();
            if list.is_empty() {
                continue;
            }
            list.sort_unstable_by_key(|&ri| {
                let r = &cas.rules[ri as usize];
                (r.level, r.layer, r.sel.spec, r.order)
            });
            let parent = styles[id].clone();
            let mut ps = Style::inherit(&parent);
            apply_matched(&mut ps, &parent, cas, &list, &ctx, &mut scratch);
            if ps.content.is_none() || ps.display == Display::None {
                continue;
            }
            ps.owner = id as u32;
            pseudo_styles.push(ps);
            let idx = (n + pseudo_styles.len() - 1) as u32;
            if pe == 1 {
                styles[id].before = idx;
            } else {
                styles[id].after = idx;
            }
        }
    }
    styles.extend(pseudo_styles);
    styles
}

fn css_color(c: u32) -> String {
    let (a, r, g, b) = (c >> 24, (c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF);
    if a == 255 {
        format!("rgb({r}, {g}, {b})")
    } else {
        let a = (a as f32 / 255.0 * 1000.0).round() / 1000.0;
        format!("rgba({r}, {g}, {b}, {a})")
    }
}

fn css_px(v: f32) -> String {
    let r = (v * 100.0).round() / 100.0;
    if r.fract() == 0.0 { format!("{}px", r as i64) } else { format!("{r}px") }
}

fn css_len(l: Len, base: f32) -> String {
    match l {
        Len::Auto => "auto".into(),
        l => css_px(l.px(base)),
    }
}

/// `getComputedStyle` values (property name, serialized value) for a
/// computed style; `rect` is the element's border box from layout.
pub fn computed_values(st: &Style, rect: Option<[f32; 4]>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| out.push((k.to_owned(), v));
    let display = match st.display {
        Display::None => "none",
        Display::Block => "block",
        Display::Inline => "inline",
        Display::InlineBlock => "inline-block",
        Display::ListItem => "list-item",
        Display::Table => "table",
        Display::InlineTable => "inline-table",
        Display::TableRow => "table-row",
        Display::TableCell => "table-cell",
        Display::TableRowGroup => "table-row-group",
        Display::TableCaption => "table-caption",
        Display::Contents => "contents",
        Display::Flex => "flex",
        Display::InlineFlex => "inline-flex",
        Display::Grid => "grid",
        Display::InlineGrid => "inline-grid",
    };
    put("display", if st.flow_root && st.display == Display::Block { "flow-root".into() } else { display.into() });
    put(
        "position",
        match st.position {
            Pos::Static => "static",
            Pos::Relative => "relative",
            Pos::Absolute => "absolute",
            Pos::Fixed => "fixed",
            Pos::Sticky => "sticky",
        }
        .into(),
    );
    put("color", css_color(st.color));
    put("background-color", css_color(st.bg));
    put("opacity", format!("{}", (st.opacity * 1000.0).round() / 1000.0));
    put("visibility", if st.hidden { "hidden" } else { "visible" }.into());
    put("z-index", st.z.map_or("auto".into(), |z| z.to_string()));
    let ovf = |o: Ovf| match o {
        Ovf::Visible => "visible",
        Ovf::Hidden => "hidden",
        Ovf::Scroll => "auto",
    };
    put("overflow-x", ovf(st.ovf_x).into());
    put("overflow-y", ovf(st.ovf_y).into());
    put("overflow", if st.ovf_x == st.ovf_y { ovf(st.ovf_x).into() } else { format!("{} {}", ovf(st.ovf_x), ovf(st.ovf_y)) });
    let [_, _, bw, bh] = rect.unwrap_or([0.0; 4]);
    let pad = st.padding.map(|p| p.px(bw));
    let (cw, ch) = ((bw - pad[1] - pad[3] - st.bw[1] - st.bw[3]).max(0.0), (bh - pad[0] - pad[2] - st.bw[0] - st.bw[2]).max(0.0));
    let (w, h) = if st.border_box { (bw, bh) } else { (cw, ch) };
    put("width", if rect.is_some() && st.display != Display::Inline { css_px(if st.border_box { w } else { cw }) } else { "auto".into() });
    put("height", if rect.is_some() && st.display != Display::Inline { css_px(if st.border_box { h } else { ch }) } else { "auto".into() });
    // Min / max sizes report percentages as specified (as browsers do).
    let size = |l: Len, base: f32| match l {
        Len::Pct(p) => format!("{}%", (p * 1000.0).round() / 1000.0),
        Len::Auto => "auto".into(),
        l => css_len(l, base),
    };
    put("min-width", size(st.min_w, bw));
    put("max-width", if st.max_w == Len::Auto { "none".into() } else { size(st.max_w, bw) });
    put("min-height", size(st.min_h, bh));
    put("max-height", if st.max_h == Len::Auto { "none".into() } else { size(st.max_h, bh) });
    for (i, side) in ["top", "right", "bottom", "left"].iter().enumerate() {
        put(&format!("margin-{side}"), css_len(st.margin[i], bw));
        put(&format!("padding-{side}"), css_px(pad[i]));
        put(&format!("border-{side}-width"), css_px(st.bw[i]));
        put(&format!("border-{side}-color"), css_color(st.bc[i]));
        put(&format!("border-{side}-style"), if st.bs[i] { "solid" } else { "none" }.into());
        put(side, css_len(st.inset[i], if i % 2 == 0 { bh } else { bw }));
    }
    put("margin", ["top", "right", "bottom", "left"].iter().enumerate().map(|(i, _)| css_len(st.margin[i], bw)).collect::<Vec<_>>().join(" "));
    put("padding", pad.iter().map(|p| css_px(*p)).collect::<Vec<_>>().join(" "));
    put("box-sizing", if st.border_box { "border-box" } else { "content-box" }.into());
    put("font-size", css_px(st.font_size));
    put("font-weight", if st.bold { "700" } else { "400" }.into());
    put("font-style", if st.italic { "italic" } else { "normal" }.into());
    put(
        "font-family",
        match &st.families {
            Some(f) => f.join(", "),
            None => match st.family {
                Family::Sans => "sans-serif",
                Family::Serif => "serif",
                Family::Mono => "monospace",
            }
            .into(),
        },
    );
    put(
        "line-height",
        match st.line_height {
            LineHeight::Normal => "normal".into(),
            LineHeight::Px(p) => css_px(p),
            LineHeight::Mult(m) => css_px(m * st.font_size),
        },
    );
    put(
        "text-align",
        match st.align {
            Align::Left => "start",
            Align::Right => "right",
            Align::Center => "center",
        }
        .into(),
    );
    put(
        "white-space",
        match st.ws {
            WhiteSpace::Normal => "normal",
            WhiteSpace::Pre => "pre",
            WhiteSpace::PreWrap => "pre-wrap",
            WhiteSpace::PreLine => "pre-line",
            WhiteSpace::NoWrap => "nowrap",
        }
        .into(),
    );
    put("text-decoration-line", match (st.underline, st.strike) {
        (true, true) => "underline line-through",
        (true, false) => "underline",
        (false, true) => "line-through",
        _ => "none",
    }.into());
    put(
        "float",
        match st.float {
            Side::Left => "left",
            Side::Right => "right",
            _ => "none",
        }
        .into(),
    );
    put(
        "flex-direction",
        match st.flex_dir {
            FlexDir::Row => "row",
            FlexDir::RowRev => "row-reverse",
            FlexDir::Col => "column",
            FlexDir::ColRev => "column-reverse",
        }
        .into(),
    );
    put("flex-wrap", if st.wrap { "wrap" } else { "nowrap" }.into());
    put("flex-grow", format!("{}", st.grow));
    put("flex-shrink", format!("{}", st.shrink));
    put("flex-basis", css_len(st.basis, bw));
    put("transform", if st.has_transform() { "matrix(1, 0, 0, 1, 0, 0)".into() } else { "none".into() });
    put("pointer-events", if st.no_pointer { "none" } else { "auto" }.into());
    put("cursor", "auto".into());
    put(
        "content",
        match &st.content {
            Some(items) => items
                .iter()
                .map(|i| match i {
                    css::ContentItem::Str(s) => format!("\"{s}\""),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join(" "),
            None => "none".into(),
        },
    );
    out
}

/// Extra UA rules for documents without a doctype (as in quirks.css).
pub const QUIRKS_CSS: &str = "table { text-align: left; font-size: medium; font-weight: normal;     font-style: normal; line-height: normal; white-space: normal }";

thread_local! {
    /// The last cascade built for a page's sheets (forced layouts and
    /// relayouts reuse it while the CSS and viewport are unchanged).
    static CASCADE_CACHE: std::cell::RefCell<Option<(u64, Rc<Cascade>)>> = const { std::cell::RefCell::new(None) };
}

/// `build_cascade`, reusing the previous result for identical inputs.
pub fn cached_cascade(author_sheets: &[(String, String)], media: &Media, quirks: bool) -> Rc<Cascade> {
    use std::hash::{Hash, Hasher};
    let mut h = crate::fx::FxHasher::default();
    for (text, base) in author_sheets {
        text.hash(&mut h);
        base.hash(&mut h);
    }
    media.vw.to_bits().hash(&mut h);
    media.vh.to_bits().hash(&mut h);
    quirks.hash(&mut h);
    let key = h.finish();
    if let Some(c) = CASCADE_CACHE.with(|c| c.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, c)| c.clone())) {
        return c;
    }
    let cas = Rc::new(build_cascade(author_sheets, media, quirks));
    CASCADE_CACHE.with(|c| *c.borrow_mut() = Some((key, cas.clone())));
    cas
}

/// Author stylesheets as (css text, base url).
pub fn build_cascade(author_sheets: &[(String, String)], media: &Media, quirks: bool) -> Cascade {
    css::reset_layers();
    let mut cas = Cascade::default();
    cas.add_sheet(css::parse_stylesheet(UA_CSS, media, ""), LEVEL_UA);
    if quirks {
        cas.add_sheet(css::parse_stylesheet(QUIRKS_CSS, media, ""), LEVEL_UA);
    }
    for (s, base) in author_sheets {
        cas.add_sheet(css::parse_stylesheet(s, media, base), LEVEL_AUTHOR);
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
        let sheets = crate::engine::sheets_for(&doc, "", &mut Default::default());
        let cas = build_cascade(&sheets, &m, doc.quirks);
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
    fn animation_end_state() {
        let doc = html::parse(
            r#"<!doctype html><style>
            @keyframes fadeIn { from { opacity: 0 } to { opacity: 1; color: rgb(1, 2, 3) } }
            @keyframes spin { to { color: red } }
            .f { opacity: 0; animation: fadeIn .3s ease-out .1s 1 forwards }
            .g { opacity: 0; animation-name: fadeIn; animation-fill-mode: both }
            .s { color: rgb(9, 9, 9); animation: spin 1s linear infinite forwards }
            .n { opacity: 0; animation: fadeIn 1s }
            </style><p class=f id=f>f</p><p class=g id=g>g</p><p class=s id=s>s</p><p class=n id=n>n</p>"#,
        );
        let m = Media { vw: 800.0, vh: 600.0 };
        let sheets = crate::engine::sheets_for(&doc, "", &mut Default::default());
        let cas = build_cascade(&sheets, &m, doc.quirks);
        let st = compute(&doc, &cas, 800.0, 600.0);
        let by = |id: &str| {
            let a = crate::atom::lookup(id).unwrap();
            &st[(0..doc.nodes.len() as u32).find(|&n| doc.elem(n).is_some_and(|e| e.id == Some(a))).unwrap() as usize]
        };
        assert_eq!(by("f").opacity, 1.0);
        assert_eq!(by("f").color & 0xFFFFFF, 0x010203);
        assert_eq!(by("g").opacity, 1.0);
        assert_eq!(by("s").color & 0xFFFFFF, 0x090909, "infinite animations keep the base style");
        assert_eq!(by("n").opacity, 0.0, "no fill: the element's own style");
    }

    #[test]
    fn modern_css() {
        let doc = html::parse(
            r#"<!doctype html><style>
            @layer base, theme;
            @layer theme { #a { color: rgb(0, 0, 255) } }
            @layer base { #a { color: rgb(255, 0, 0) } }
            #b { color: green } @layer theme { #b { color: red } }
            .card { & .title { color: oklch(62.8% 0.2577 29.23) } &:first-child { padding-left: 7px } }
            @supports (display: grid) and (not (foo: bar)) { #c { color: #010203 } }
            @supports (backdrop-filter: blur(2px)) { #c { color: red } }
            @media (width>=500px) and (not (hover: none)) { #d { color: rgb(4, 5, 6) } }
            @media (forced-colors: active) { #d { color: red } }
            li:nth-of-type(2) { color: color-mix(in srgb, rgb(255, 0, 0), rgb(0, 0, 255)) }
            ul:has(> .x) { margin-inline: 3px 4px } :is(ul .x, ol .y) { color: hwb(120 0% 0%) }
            p { color: rgb(9, 9, 9) } :where(div) p { color: red }
            .u { color: blue } .u span { color: unset }
            </style>
            <div id=a>a</div><div id=b>b</div><div class=card><p class=title id=t>t</p></div>
            <div id=c>c</div><div id=d>d</div>
            <ul id=l><li>1</li><span class=x>x</span><li id=li2>2</li></ul><p id=p>p</p><div class=u><span id=s>s</span></div>"#,
        );
        let m = Media { vw: 800.0, vh: 600.0 };
        let sheets = crate::engine::sheets_for(&doc, "", &mut Default::default());
        let cas = build_cascade(&sheets, &m, doc.quirks);
        let st = compute(&doc, &cas, 800.0, 600.0);
        let by = |id: &str| {
            let a = crate::atom::lookup(id).unwrap();
            &st[(0..doc.nodes.len() as u32).find(|&n| doc.elem(n).is_some_and(|e| e.id == Some(a))).unwrap() as usize]
        };
        let rgb = |id: &str| by(id).color & 0xFFFFFF;
        assert_eq!(rgb("a"), 0x0000FF, "later layer wins");
        assert_eq!(rgb("b"), 0x008000, "unlayered beats layered");
        let t = rgb("t");
        assert!((t >> 16) > 0xF0 && (t & 0xFFFF) < 0x1010, "oklch red: {t:06x}");
        assert_eq!(rgb("c"), 0x010203);
        assert_eq!(rgb("d"), 0x040506);
        assert_eq!(rgb("li2"), 0x800080);
        assert_eq!(rgb("p"), 0x090909, ":where has zero specificity");
        assert_eq!(rgb("s"), 0x0000FF, "unset inherits color");
        assert_eq!(by("l").margin[3], Len::Px(3.0));
        assert_eq!(by("l").margin[1], Len::Px(4.0));
        let x = doc.find(|e| e.classes.contains(&crate::atom::intern("x")));
        assert_eq!(st[x as usize].color & 0xFFFFFF, 0x00FF00);
        let title = doc.find(|e| e.id == crate::atom::lookup("t"));
        let _ = title;
    }

    #[test]
    fn custom_properties_and_calc() {
        let doc = html::parse(
            r#"<style>:root { --fg: #123456; --pad: 4px } p { color: var(--fg); padding-left: var(--pad);
               margin-left: calc(10% + 2 * var(--pad)); border-top: 1px solid var(--missing, red) }
               .x { --fg: blue }</style><p>a</p><div class=x><p>b</p></div>"#,
        );
        let m = Media { vw: 800.0, vh: 600.0 };
        let sheets = crate::engine::sheets_for(&doc, "", &mut Default::default());
        let cas = build_cascade(&sheets, &m, doc.quirks);
        let st = compute(&doc, &cas, 800.0, 600.0);
        let ps: Vec<_> = (0..doc.nodes.len() as u32).filter(|&n| doc.tag(n) == Some(k::p)).collect();
        let (a, b) = (&st[ps[0] as usize], &st[ps[1] as usize]);
        assert_eq!(a.color, 0xff123456);
        assert_eq!(b.color, 0xff0000ff);
        assert_eq!(a.padding[3], Len::Px(4.0));
        assert_eq!(a.margin[3].px(100.0), 18.0);
        assert_eq!(a.bc[0], 0xffff0000);
    }
}

