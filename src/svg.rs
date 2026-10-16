//! SVG rendering: a small XML parser, styling (presentation attributes,
//! `<style>` sheets and `style` attributes), shapes and paths, transforms,
//! nested viewports and `<use>`, linear/radial gradients, clip paths,
//! strokes (joins, caps, dashes), basic text and images, and an
//! anti-aliased scanline rasterizer.

use crate::fx::FxHashMap;
use std::f32::consts::PI;

// =================================================================== XML

#[derive(Clone, Debug, Default)]
pub struct El {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub kids: Vec<Node>,
}

#[derive(Clone, Debug)]
pub enum Node {
    El(El),
    Text(String),
}

impl El {
    fn attr(&self, n: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }
    fn text(&self, out: &mut String) {
        for k in &self.kids {
            match k {
                Node::Text(t) => out.push_str(t),
                Node::El(e) => e.text(out),
            }
        }
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ if name.starts_with("#x") || name.starts_with("#X") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
            _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse XML and return the first `<svg>` element.
pub fn parse_xml(src: &str) -> Option<El> {
    let b = src.as_bytes();
    let n = b.len();
    let mut stack: Vec<El> = vec![El { name: "#root".into(), ..Default::default() }];
    let mut i = 0;
    let find = |from: usize, pat: &str| src[from..].find(pat).map(|p| p + from);
    while i < n {
        if b[i] != b'<' {
            let end = find(i, "<").unwrap_or(n);
            let t = &src[i..end];
            if !t.trim().is_empty() {
                stack.last_mut()?.kids.push(Node::Text(decode_entities(t)));
            }
            i = end;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i = find(i + 4, "-->").map_or(n, |p| p + 3);
        } else if rest.starts_with("<![CDATA[") {
            let end = find(i + 9, "]]>").unwrap_or(n);
            stack.last_mut()?.kids.push(Node::Text(src[i + 9..end].to_owned()));
            i = (end + 3).min(n);
        } else if rest.starts_with("<?") {
            i = find(i + 2, "?>").map_or(n, |p| p + 2);
        } else if rest.starts_with("<!") {
            let mut depth = 0;
            let mut j = i + 2;
            while j < n {
                match b[j] {
                    b'[' => depth += 1,
                    b']' => depth -= 1,
                    b'>' if depth <= 0 => break,
                    _ => {}
                }
                j += 1;
            }
            i = j + 1;
        } else if rest.starts_with("</") {
            let end = find(i, ">").unwrap_or(n);
            let name = src[i + 2..end].trim().to_ascii_lowercase();
            if let Some(pos) = stack.iter().rposition(|e| e.name == name) {
                if pos > 0 {
                    while stack.len() > pos {
                        let e = stack.pop()?;
                        stack.last_mut()?.kids.push(Node::El(e));
                    }
                }
            }
            i = end + 1;
        } else {
            // Start tag.
            let mut j = i + 1;
            while j < n && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                j += 1;
            }
            let mut el = El { name: src[i + 1..j].to_ascii_lowercase(), ..Default::default() };
            let mut self_closing = false;
            loop {
                while j < n && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= n {
                    break;
                }
                if b[j] == b'>' {
                    j += 1;
                    break;
                }
                if b[j] == b'/' {
                    self_closing = true;
                    j += 1;
                    continue;
                }
                let s = j;
                while j < n && !b[j].is_ascii_whitespace() && b[j] != b'=' && b[j] != b'>' && b[j] != b'/' {
                    j += 1;
                }
                let mut name = src[s..j].to_ascii_lowercase();
                if let Some(local) = name.strip_prefix("xlink:") {
                    name = local.to_owned();
                }
                while j < n && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let mut value = String::new();
                if j < n && b[j] == b'=' {
                    j += 1;
                    while j < n && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < n && (b[j] == b'"' || b[j] == b'\'') {
                        let q = b[j] as char;
                        let end = src[j + 1..].find(q).map_or(n, |p| p + j + 1);
                        value = decode_entities(&src[j + 1..end]);
                        j = (end + 1).min(n);
                    } else {
                        let s = j;
                        while j < n && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                            j += 1;
                        }
                        value = decode_entities(&src[s..j]);
                    }
                }
                if !name.is_empty() && el.attr(&name).is_none() {
                    el.attrs.push((name, value));
                }
            }
            i = j;
            if self_closing {
                stack.last_mut()?.kids.push(Node::El(el));
            } else if matches!(el.name.as_str(), "style" | "script") {
                // Raw text content.
                let close = format!("</{}", el.name);
                let lower = src[i..].to_ascii_lowercase();
                let end = lower.find(&close).map_or(n, |p| p + i);
                el.kids.push(Node::Text(src[i..end].to_owned()));
                stack.last_mut()?.kids.push(Node::El(el));
                i = find(end, ">").map_or(n, |p| p + 1);
            } else {
                stack.push(el);
            }
        }
    }
    while stack.len() > 1 {
        let e = stack.pop()?;
        stack.last_mut()?.kids.push(Node::El(e));
    }
    fn first_svg(e: El) -> Option<El> {
        if e.name == "svg" {
            return Some(e);
        }
        e.kids.into_iter().find_map(|k| match k {
            Node::El(e) => first_svg(e),
            _ => None,
        })
    }
    first_svg(stack.pop()?)
}

// =================================================================== CSS

#[derive(Debug, Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

#[derive(Debug)]
struct Rule {
    chain: Vec<Compound>,
    spec: u32,
    order: usize,
    decls: Vec<(String, String)>,
}

fn parse_decls(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            let v = v.trim().trim_end_matches("!important").trim();
            Some((k.trim().to_ascii_lowercase(), v.to_owned()))
        })
        .collect()
}

fn parse_sheet(css: &str, rules: &mut Vec<Rule>) {
    let mut css = css.to_owned();
    while let Some(s) = css.find("/*") {
        let e = css[s..].find("*/").map_or(css.len(), |p| s + p + 2);
        css.replace_range(s..e, "");
    }
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let sel = rest[..open].trim();
        let close = rest[open..].find('}').map_or(rest.len(), |p| open + p);
        let body = &rest[open + 1..close];
        rest = &rest[(close + 1).min(rest.len())..];
        if sel.starts_with('@') {
            // Skip at-rules (and the nested block of @media).
            if let Some(p) = rest.find('}') {
                if body.contains('{') {
                    rest = &rest[p + 1..];
                }
            }
            continue;
        }
        let decls = parse_decls(body);
        for s in sel.split(',') {
            if s.contains(':') || s.contains('[') {
                continue;
            }
            let mut chain = Vec::new();
            let mut spec = 0;
            for part in s.split(|c: char| c.is_whitespace() || c == '>' || c == '+' || c == '~').filter(|p| !p.is_empty()) {
                let mut c = Compound::default();
                let mut cur = String::new();
                let mut kind = 't';
                let flush = |c: &mut Compound, kind: char, cur: &mut String, spec: &mut u32| {
                    if cur.is_empty() {
                        return;
                    }
                    match kind {
                        '#' => {
                            c.id = Some(std::mem::take(cur));
                            *spec += 100;
                        }
                        '.' => {
                            c.classes.push(std::mem::take(cur));
                            *spec += 10;
                        }
                        _ => {
                            let t = std::mem::take(cur).to_ascii_lowercase();
                            if t != "*" {
                                c.tag = Some(t);
                                *spec += 1;
                            }
                        }
                    }
                };
                for ch in part.chars() {
                    if ch == '.' || ch == '#' {
                        flush(&mut c, kind, &mut cur, &mut spec);
                        kind = ch;
                    } else {
                        cur.push(ch);
                    }
                }
                flush(&mut c, kind, &mut cur, &mut spec);
                chain.push(c);
            }
            if !chain.is_empty() {
                let order = rules.len();
                rules.push(Rule { chain, spec, order, decls: decls.clone() });
            }
        }
    }
}

fn compound_matches(c: &Compound, e: &El) -> bool {
    if c.tag.as_ref().is_some_and(|t| *t != e.name) {
        return false;
    }
    if c.id.as_ref().is_some_and(|id| e.attr("id") != Some(id.as_str())) {
        return false;
    }
    if !c.classes.is_empty() {
        let cls = e.attr("class").unwrap_or("");
        if !c.classes.iter().all(|k| cls.split_ascii_whitespace().any(|x| x == k)) {
            return false;
        }
    }
    true
}

fn rule_matches(r: &Rule, e: &El, anc: &[&El]) -> bool {
    let (last, rest) = r.chain.split_last().unwrap();
    if !compound_matches(last, e) {
        return false;
    }
    let mut i = anc.len();
    for c in rest.iter().rev() {
        loop {
            if i == 0 {
                return false;
            }
            i -= 1;
            if compound_matches(c, anc[i]) {
                break;
            }
        }
    }
    true
}

// ================================================================= style

#[derive(Clone, Debug, PartialEq)]
enum Paint {
    None,
    Color(u32),
    Current,
    Url(String, Box<Paint>),
}

#[derive(Clone, Debug)]
struct Style {
    fill: Paint,
    stroke: Paint,
    fill_op: f32,
    stroke_op: f32,
    sw: f32,
    cap: u8,
    join: u8,
    miter: f32,
    evenodd: bool,
    clip_evenodd: bool,
    dash: Vec<f32>,
    dash_off: f32,
    color: u32,
    font_size: f32,
    bold: bool,
    italic: bool,
    serif: bool,
    anchor: u8,
    visible: bool,
    // Not inherited:
    opacity: f32,
    hidden: bool,
    clip: Option<String>,
    stop_color: u32,
    stop_op: f32,
}

impl Style {
    fn root(color: u32) -> Style {
        Style {
            fill: Paint::Color(0xFF000000),
            stroke: Paint::None,
            fill_op: 1.0,
            stroke_op: 1.0,
            sw: 1.0,
            cap: 0,
            join: 0,
            miter: 4.0,
            evenodd: false,
            clip_evenodd: false,
            dash: Vec::new(),
            dash_off: 0.0,
            color,
            font_size: 16.0,
            bold: false,
            italic: false,
            serif: false,
            anchor: 0,
            visible: true,
            opacity: 1.0,
            hidden: false,
            clip: None,
            stop_color: 0xFF000000,
            stop_op: 1.0,
        }
    }
    fn child(&self) -> Style {
        Style { opacity: 1.0, hidden: false, clip: None, stop_color: 0xFF000000, stop_op: 1.0, ..self.clone() }
    }
}

fn num(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        return p.trim().parse::<f32>().ok().map(|v| v / 100.0);
    }
    s.parse().ok()
}

/// A length in user units; `pct` resolves percentages, `em` is the font size.
fn length(s: &str, pct: f32, em: f32) -> Option<f32> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic() || c == '%').unwrap_or(s.len());
    let v: f32 = s[..split].trim().parse().ok()?;
    Some(match &s[split..] {
        "" | "px" => v,
        "%" => v / 100.0 * pct,
        "pt" => v * 4.0 / 3.0,
        "pc" => v * 16.0,
        "mm" => v * 96.0 / 25.4,
        "cm" => v * 96.0 / 2.54,
        "in" => v * 96.0,
        "em" | "rem" => v * em,
        "ex" => v * em / 2.0,
        _ => v,
    })
}

fn color(v: &str) -> Option<u32> {
    let v = v.trim().to_ascii_lowercase();
    if v == "transparent" {
        return Some(0);
    }
    crate::css::parse_color(&v)
}

fn paint(v: &str, prev: &Paint) -> Paint {
    let v = v.trim();
    let lower = v.to_ascii_lowercase();
    match lower.as_str() {
        "none" => Paint::None,
        "currentcolor" => Paint::Current,
        "inherit" => prev.clone(),
        _ if lower.starts_with("url(") => {
            let end = v.find(')').unwrap_or(v.len());
            let id = v[4..end].trim().trim_matches(|c| c == '"' || c == '\'').trim_start_matches('#').to_owned();
            let fallback = paint(&v[(end + 1).min(v.len())..], &Paint::None);
            Paint::Url(id, Box::new(fallback))
        }
        _ => color(v).map_or(prev.clone(), Paint::Color),
    }
}

const PROPS: &[&str] = &[
    "fill", "stroke", "fill-opacity", "stroke-opacity", "opacity", "stroke-width", "stroke-linecap", "stroke-linejoin",
    "stroke-miterlimit", "fill-rule", "clip-rule", "stroke-dasharray", "stroke-dashoffset", "display", "visibility",
    "color", "font-size", "font-weight", "font-style", "font-family", "text-anchor", "clip-path", "stop-color",
    "stop-opacity", "font",
];

fn apply(st: &mut Style, name: &str, v: &str, vp_diag: f32) {
    let v = v.trim();
    if v == "inherit" {
        return;
    }
    let lower = v.to_ascii_lowercase();
    let op = |v: &str| num(v).map(|x| x.clamp(0.0, 1.0));
    match name {
        "fill" => st.fill = paint(v, &st.fill),
        "stroke" => st.stroke = paint(v, &st.stroke),
        "fill-opacity" => st.fill_op = op(v).unwrap_or(st.fill_op),
        "stroke-opacity" => st.stroke_op = op(v).unwrap_or(st.stroke_op),
        "opacity" => st.opacity = op(v).unwrap_or(1.0),
        "stroke-width" => st.sw = length(v, vp_diag, st.font_size).unwrap_or(st.sw).max(0.0),
        "stroke-linecap" => st.cap = match lower.as_str() { "round" => 1, "square" => 2, _ => 0 },
        "stroke-linejoin" => st.join = match lower.as_str() { "round" => 1, "bevel" => 2, _ => 0 },
        "stroke-miterlimit" => st.miter = num(v).unwrap_or(4.0).max(1.0),
        "fill-rule" => st.evenodd = lower == "evenodd",
        "clip-rule" => st.clip_evenodd = lower == "evenodd",
        "stroke-dasharray" => {
            st.dash = if lower == "none" {
                Vec::new()
            } else {
                v.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).filter_map(|s| length(s, vp_diag, st.font_size)).collect()
            }
        }
        "stroke-dashoffset" => st.dash_off = length(v, vp_diag, st.font_size).unwrap_or(0.0),
        "display" => st.hidden = lower == "none",
        "visibility" => st.visible = lower == "visible",
        "color" => {
            if let Some(c) = color(v) {
                st.color = c;
            }
        }
        "font-size" => {
            let parent = st.font_size;
            st.font_size = match lower.as_str() {
                "small" => 13.0,
                "medium" => 16.0,
                "large" => 18.0,
                "x-large" => 24.0,
                _ => length(v, parent, parent).unwrap_or(parent),
            }
        }
        "font-weight" => st.bold = lower == "bold" || lower == "bolder" || lower.parse::<u32>().is_ok_and(|w| w >= 600),
        "font-style" => st.italic = lower.starts_with("italic") || lower.starts_with("oblique"),
        "font-family" => st.serif = lower.contains("serif") && !lower.contains("sans") || lower.contains("times") || lower.contains("georgia"),
        "font" => {
            for t in v.split_whitespace() {
                if let Some(px) = length(t, st.font_size, st.font_size).filter(|_| t.ends_with("px") || t.ends_with("pt") || t.ends_with("em")) {
                    st.font_size = px;
                } else if t.eq_ignore_ascii_case("bold") {
                    st.bold = true;
                } else if t.eq_ignore_ascii_case("italic") {
                    st.italic = true;
                }
            }
        }
        "text-anchor" => st.anchor = match lower.as_str() { "middle" => 1, "end" => 2, _ => 0 },
        "clip-path" => st.clip = paint(v, &Paint::None).url_id(),
        "stop-color" => {
            st.stop_color = if lower == "currentcolor" { st.color } else { color(v).unwrap_or(st.stop_color) };
        }
        "stop-opacity" => st.stop_op = op(v).unwrap_or(1.0),
        _ => {}
    }
}

impl Paint {
    fn url_id(&self) -> Option<String> {
        match self {
            Paint::Url(id, _) => Some(id.clone()),
            _ => None,
        }
    }
}

// ============================================================== geometry

type Mat = [f32; 6];
const IDENT: Mat = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn mul(m: &Mat, n: &Mat) -> Mat {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

fn apply_m(m: &Mat, x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

fn invert(m: &Mat) -> Option<Mat> {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-12 {
        return None;
    }
    let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
    Some([a, b, c, d, -(a * m[4] + c * m[5]), -(b * m[4] + d * m[5])])
}

fn scale_of(m: &Mat) -> f32 {
    (m[0] * m[3] - m[1] * m[2]).abs().sqrt().max(1e-6)
}

fn numbers(s: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut p = Parser { s: s.as_bytes(), i: 0 };
    while let Some(v) = p.number() {
        out.push(v);
    }
    out
}

fn parse_transform(s: &str) -> Mat {
    let mut m = IDENT;
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let close = rest[open..].find(')').map_or(rest.len(), |p| open + p);
        let a = numbers(&rest[open + 1..close]);
        rest = &rest[(close + 1).min(rest.len())..];
        let g = |i: usize| a.get(i).copied().unwrap_or(0.0);
        let t: Mat = match name.as_str() {
            "matrix" if a.len() >= 6 => [a[0], a[1], a[2], a[3], a[4], a[5]],
            "translate" => [1.0, 0.0, 0.0, 1.0, g(0), g(1)],
            "scale" => {
                let sx = a.first().copied().unwrap_or(1.0);
                [sx, 0.0, 0.0, a.get(1).copied().unwrap_or(sx), 0.0, 0.0]
            }
            "rotate" => {
                let (s, c) = (g(0) * PI / 180.0).sin_cos();
                let r = [c, s, -s, c, 0.0, 0.0];
                if a.len() >= 3 {
                    mul(&mul(&[1.0, 0.0, 0.0, 1.0, g(1), g(2)], &r), &[1.0, 0.0, 0.0, 1.0, -g(1), -g(2)])
                } else {
                    r
                }
            }
            "skewx" => [1.0, 0.0, (g(0) * PI / 180.0).tan(), 1.0, 0.0, 0.0],
            "skewy" => [1.0, (g(0) * PI / 180.0).tan(), 0.0, 1.0, 0.0, 0.0],
            _ => IDENT,
        };
        m = mul(&m, &t);
    }
    m
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_sep(&mut self) {
        while self.i < self.s.len() && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',') {
            self.i += 1;
        }
    }
    fn number(&mut self) -> Option<f32> {
        self.skip_sep();
        let s = self.s;
        let start = self.i;
        let mut j = self.i;
        if j < s.len() && (s[j] == b'-' || s[j] == b'+') {
            j += 1;
        }
        let mut digits = false;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
            digits = true;
        }
        if j < s.len() && s[j] == b'.' {
            j += 1;
            while j < s.len() && s[j].is_ascii_digit() {
                j += 1;
                digits = true;
            }
        }
        if !digits {
            return None;
        }
        if j < s.len() && (s[j] == b'e' || s[j] == b'E') {
            let mut k = j + 1;
            if k < s.len() && (s[k] == b'-' || s[k] == b'+') {
                k += 1;
            }
            if k < s.len() && s[k].is_ascii_digit() {
                while k < s.len() && s[k].is_ascii_digit() {
                    k += 1;
                }
                j = k;
            }
        }
        self.i = j;
        std::str::from_utf8(&s[start..j]).ok()?.parse().ok()
    }
    fn flag(&mut self) -> Option<bool> {
        self.skip_sep();
        let c = *self.s.get(self.i)?;
        self.i += 1;
        match c {
            b'0' => Some(false),
            b'1' => Some(true),
            _ => None,
        }
    }
}

/// A flattened subpath in user space.
#[derive(Clone, Debug, Default)]
struct Sub {
    pts: Vec<(f32, f32)>,
    closed: bool,
}

fn seg_count(len: f32, tol: f32) -> usize {
    ((len / tol).sqrt() * 0.6).ceil().clamp(1.0, 256.0) as usize
}

fn cubic(out: &mut Vec<(f32, f32)>, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), tol: f32) {
    let d = |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let n = seg_count(d(p0, p1) + d(p1, p2) + d(p2, p3), tol);
    for i in 1..=n {
        let t = i as f32 / n as f32;
        let u = 1.0 - t;
        let (a, b, c, e) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        out.push((a * p0.0 + b * p1.0 + c * p2.0 + e * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + e * p3.1));
    }
}

fn quad(out: &mut Vec<(f32, f32)>, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), tol: f32) {
    let d = |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
    let n = seg_count(d(p0, p1) + d(p1, p2), tol);
    for i in 1..=n {
        let t = i as f32 / n as f32;
        let u = 1.0 - t;
        out.push((u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0, u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1));
    }
}

#[allow(clippy::too_many_arguments)]
fn arc(out: &mut Vec<(f32, f32)>, p1: (f32, f32), rx: f32, ry: f32, phi_deg: f32, large: bool, sweep: bool, p2: (f32, f32), tol: f32) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx < 1e-6 || ry < 1e-6 || p1 == p2 {
        out.push(p2);
        return;
    }
    let (sin, cos) = (phi_deg * PI / 180.0).sin_cos();
    let (dx, dy) = ((p1.0 - p2.0) / 2.0, (p1.1 - p2.1) / 2.0);
    let (x1p, y1p) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let lambda = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let mut coef = (num / den).max(0.0).sqrt();
    if large == sweep {
        coef = -coef;
    }
    let (cxp, cyp) = (coef * rx * y1p / ry, -coef * ry * x1p / rx);
    let (cx, cy) = (cos * cxp - sin * cyp + (p1.0 + p2.0) / 2.0, sin * cxp + cos * cyp + (p1.1 + p2.1) / 2.0);
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let t1 = angle(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dt = angle((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if !sweep && dt > 0.0 {
        dt -= 2.0 * PI;
    } else if sweep && dt < 0.0 {
        dt += 2.0 * PI;
    }
    let r = rx.max(ry);
    let step = 2.0 * (1.0 - (tol / r).min(1.0)).acos();
    let n = (dt.abs() / step.max(0.01)).ceil().clamp(1.0, 512.0) as usize;
    for i in 1..=n {
        let t = t1 + dt * i as f32 / n as f32;
        let (st, ct) = t.sin_cos();
        out.push((cx + rx * ct * cos - ry * st * sin, cy + rx * ct * sin + ry * st * cos));
    }
    if let Some(last) = out.last_mut() {
        *last = p2;
    }
}

fn parse_path(d: &str, tol: f32) -> Vec<Sub> {
    let mut subs: Vec<Sub> = Vec::new();
    let mut cur = Sub::default();
    let mut p = Parser { s: d.as_bytes(), i: 0 };
    let (mut x, mut y, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut last_ctrl: Option<(f32, f32)> = None;
    let mut cmd = b'M';
    let mut prev_cmd = b' ';
    loop {
        p.skip_sep();
        if p.i >= p.s.len() {
            break;
        }
        let c = p.s[p.i];
        if c.is_ascii_alphabetic() {
            cmd = c;
            p.i += 1;
        } else if cmd == b'Z' || cmd == b'z' {
            break; // numbers after Z: malformed
        }
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { (x, y) } else { (0.0, 0.0) };
        macro_rules! n {
            () => {
                match p.number() {
                    Some(v) => v,
                    None => break,
                }
            };
        }
        let mut ctrl = None;
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (nx, ny) = (n!() + ox, n!() + oy);
                if cur.pts.len() > 1 || cur.closed {
                    subs.push(std::mem::take(&mut cur));
                }
                cur = Sub { pts: vec![(nx, ny)], closed: false };
                (x, y, sx, sy) = (nx, ny, nx, ny);
                // Further pairs are implicit line-tos.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' => {
                (x, y) = (n!() + ox, n!() + oy);
                cur.pts.push((x, y));
            }
            b'H' => {
                x = n!() + ox;
                cur.pts.push((x, y));
            }
            b'V' => {
                y = n!() + if rel { y } else { 0.0 };
                cur.pts.push((x, y));
            }
            b'C' => {
                let (x1, y1, x2, y2, ex, ey) = (n!() + ox, n!() + oy, n!() + ox, n!() + oy, n!() + ox, n!() + oy);
                cubic(&mut cur.pts, (x, y), (x1, y1), (x2, y2), (ex, ey), tol);
                ctrl = Some((x2, y2));
                (x, y) = (ex, ey);
            }
            b'S' => {
                let (x1, y1) = match (last_ctrl, prev_cmd.to_ascii_uppercase()) {
                    (Some((cx, cy)), b'C' | b'S') => (2.0 * x - cx, 2.0 * y - cy),
                    _ => (x, y),
                };
                let (x2, y2, ex, ey) = (n!() + ox, n!() + oy, n!() + ox, n!() + oy);
                cubic(&mut cur.pts, (x, y), (x1, y1), (x2, y2), (ex, ey), tol);
                ctrl = Some((x2, y2));
                (x, y) = (ex, ey);
            }
            b'Q' => {
                let (x1, y1, ex, ey) = (n!() + ox, n!() + oy, n!() + ox, n!() + oy);
                quad(&mut cur.pts, (x, y), (x1, y1), (ex, ey), tol);
                ctrl = Some((x1, y1));
                (x, y) = (ex, ey);
            }
            b'T' => {
                let (x1, y1) = match (last_ctrl, prev_cmd.to_ascii_uppercase()) {
                    (Some((cx, cy)), b'Q' | b'T') => (2.0 * x - cx, 2.0 * y - cy),
                    _ => (x, y),
                };
                let (ex, ey) = (n!() + ox, n!() + oy);
                quad(&mut cur.pts, (x, y), (x1, y1), (ex, ey), tol);
                ctrl = Some((x1, y1));
                (x, y) = (ex, ey);
            }
            b'A' => {
                let (rx, ry, rot) = (n!(), n!(), n!());
                let (Some(large), Some(sweep)) = (p.flag(), p.flag()) else { break };
                let (ex, ey) = (n!() + ox, n!() + oy);
                arc(&mut cur.pts, (x, y), rx, ry, rot, large, sweep, (ex, ey), tol);
                (x, y) = (ex, ey);
            }
            b'Z' => {
                cur.closed = true;
                subs.push(std::mem::take(&mut cur));
                (x, y) = (sx, sy);
                cur.pts.push((x, y));
            }
            _ => break,
        }
        last_ctrl = ctrl;
        prev_cmd = cmd;
    }
    if cur.pts.len() > 1 {
        subs.push(cur);
    }
    subs
}

fn ellipse_sub(cx: f32, cy: f32, rx: f32, ry: f32, tol: f32) -> Sub {
    let r = rx.max(ry);
    let step = 2.0 * (1.0 - (tol / r).min(1.0)).acos();
    let n = ((2.0 * PI) / step.max(0.01)).ceil().clamp(8.0, 720.0) as usize;
    let pts = (0..n).map(|i| {
        let t = 2.0 * PI * i as f32 / n as f32;
        (cx + rx * t.cos(), cy + ry * t.sin())
    });
    Sub { pts: pts.collect(), closed: true }
}

// ============================================================ rasterizer

/// Coverage of a filled shape over a bounding region of the canvas.
struct Cov {
    x0: i32,
    y0: i32,
    w: usize,
    h: usize,
    data: Vec<f32>,
}

const SUB: usize = 4;

fn rasterize(polys: &[Vec<(f32, f32)>], evenodd: bool, cw: usize, ch: usize) -> Option<Cov> {
    struct Edge {
        y0: f32,
        y1: f32,
        x: f32,
        dxdy: f32,
        dir: i32,
    }
    let mut edges = Vec::new();
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for poly in polys {
        let n = poly.len();
        if n < 2 {
            continue;
        }
        for i in 0..n {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            if !(a.0.is_finite() && a.1.is_finite() && b.0.is_finite() && b.1.is_finite()) {
                continue;
            }
            minx = minx.min(a.0);
            maxx = maxx.max(a.0);
            miny = miny.min(a.1);
            maxy = maxy.max(a.1);
            if a.1 == b.1 {
                continue;
            }
            let (top, bot, dir) = if a.1 < b.1 { (a, b, 1) } else { (b, a, -1) };
            edges.push(Edge { y0: top.1, y1: bot.1, x: top.0, dxdy: (bot.0 - top.0) / (bot.1 - top.1), dir });
        }
    }
    if edges.is_empty() {
        return None;
    }
    let x0 = (minx.floor() as i32).max(0);
    let y0 = (miny.floor() as i32).max(0);
    let x1 = (maxx.ceil() as i32).min(cw as i32);
    let y1 = (maxy.ceil() as i32).min(ch as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut data = vec![0f32; w * h];
    edges.sort_by(|a, b| a.y0.total_cmp(&b.y0));
    let mut next = 0;
    let mut active: Vec<usize> = Vec::new();
    let mut xs: Vec<(f32, i32)> = Vec::new();
    let mut row = vec![0f32; w + 1];
    let inv = 1.0 / SUB as f32;
    for py in y0..y1 {
        row.iter_mut().for_each(|v| *v = 0.0);
        for s in 0..SUB {
            let sy = py as f32 + (s as f32 + 0.5) * inv;
            while next < edges.len() && edges[next].y0 <= sy {
                active.push(next);
                next += 1;
            }
            active.retain(|&e| edges[e].y1 > sy);
            xs.clear();
            for &e in &active {
                let ed = &edges[e];
                if ed.y0 <= sy {
                    xs.push((ed.x + (sy - ed.y0) * ed.dxdy, ed.dir));
                }
            }
            xs.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut wind = 0;
            for k in 0..xs.len().saturating_sub(1) {
                wind += xs[k].1;
                let inside = if evenodd { wind % 2 != 0 } else { wind != 0 };
                if !inside {
                    continue;
                }
                let a = (xs[k].0 - x0 as f32).clamp(0.0, w as f32);
                let b = (xs[k + 1].0 - x0 as f32).clamp(0.0, w as f32);
                if b <= a {
                    continue;
                }
                let (i0, i1) = (a as usize, b as usize);
                if i0 == i1 {
                    row[i0] += (b - a) * inv;
                } else {
                    row[i0] += (i0 as f32 + 1.0 - a) * inv;
                    for v in &mut row[i0 + 1..i1] {
                        *v += inv;
                    }
                    row[i1] += (b - i1 as f32) * inv;
                }
            }
        }
        let o = (py - y0) as usize * w;
        for x in 0..w {
            data[o + x] = row[x].min(1.0);
        }
    }
    Some(Cov { x0, y0, w, h, data })
}

// ================================================================ strokes

fn orient(mut p: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    let mut area = 0.0;
    for i in 0..p.len() {
        let (a, b) = (p[i], p[(i + 1) % p.len()]);
        area += a.0 * b.1 - b.0 * a.1;
    }
    if area < 0.0 {
        p.reverse();
    }
    p
}

fn circle_poly(c: (f32, f32), r: f32, tol: f32) -> Vec<(f32, f32)> {
    ellipse_sub(c.0, c.1, r, r, tol).pts
}

fn dashes(sub: &Sub, dash: &[f32], offset: f32) -> Vec<Sub> {
    let mut pts = sub.pts.clone();
    if sub.closed && pts.first() != pts.last() {
        pts.push(pts[0]);
    }
    let total: f32 = dash.iter().sum();
    if total <= 0.0 || dash.iter().any(|d| *d < 0.0) {
        return vec![sub.clone()];
    }
    let mut out = Vec::new();
    let mut idx = 0;
    let mut left = dash[0];
    let mut on = true;
    // Apply the offset.
    let mut off = offset.rem_euclid(total);
    while off > 0.0 {
        if off >= left {
            off -= left;
            idx = (idx + 1) % dash.len();
            left = dash[idx];
            on = !on;
        } else {
            left -= off;
            off = 0.0;
        }
    }
    let mut cur: Vec<(f32, f32)> = if on { vec![pts[0]] } else { Vec::new() };
    for w in pts.windows(2) {
        let (mut a, b) = (w[0], w[1]);
        let mut seg = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        while seg > 0.0 {
            if left >= seg {
                left -= seg;
                if on {
                    cur.push(b);
                }
                seg = 0.0;
            } else {
                let t = left / seg;
                let m = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                if on {
                    cur.push(m);
                    out.push(Sub { pts: std::mem::take(&mut cur), closed: false });
                } else {
                    cur = vec![m];
                }
                on = !on;
                seg -= left;
                a = m;
                idx = (idx + 1) % dash.len();
                left = dash[idx];
            }
        }
    }
    if on && cur.len() > 1 {
        out.push(Sub { pts: cur, closed: false });
    }
    out
}

/// Outline a stroked subpath as polygons (filled with the nonzero rule).
fn stroke_polys(sub: &Sub, st: &Style, tol: f32, out: &mut Vec<Vec<(f32, f32)>>) {
    let hw = st.sw / 2.0;
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(sub.pts.len());
    for &p in &sub.pts {
        if pts.last().is_none_or(|q: &(f32, f32)| (q.0 - p.0).abs() > 1e-5 || (q.1 - p.1).abs() > 1e-5) {
            pts.push(p);
        }
    }
    let closed = sub.closed;
    if closed && pts.len() > 2 && pts.first() == pts.last() {
        pts.pop();
    }
    if pts.len() == 1 {
        match st.cap {
            1 => out.push(orient(circle_poly(pts[0], hw, tol))),
            2 => {
                let (x, y) = pts[0];
                out.push(vec![(x - hw, y - hw), (x + hw, y - hw), (x + hw, y + hw), (x - hw, y + hw)]);
            }
            _ => {}
        }
        return;
    }
    let n = pts.len();
    let segs = if closed { n } else { n - 1 };
    let dir = |i: usize| {
        let (a, b) = (pts[i % n], pts[(i + 1) % n]);
        let l = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        ((b.0 - a.0) / l, (b.1 - a.1) / l)
    };
    for i in 0..segs {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        let d = dir(i);
        let nrm = (-d.1 * hw, d.0 * hw);
        out.push(orient(vec![(a.0 + nrm.0, a.1 + nrm.1), (b.0 + nrm.0, b.1 + nrm.1), (b.0 - nrm.0, b.1 - nrm.1), (a.0 - nrm.0, a.1 - nrm.1)]));
    }
    // Joins.
    let joins: Vec<usize> = if closed { (0..n).collect() } else { (1..n - 1).collect() };
    for v in joins {
        let d0 = dir((v + n - 1) % n);
        let d1 = dir(v);
        let p = pts[v];
        let cross = d0.0 * d1.1 - d0.1 * d1.0;
        let dot = d0.0 * d1.0 + d0.1 * d1.1;
        if cross.abs() < 1e-6 && dot > 0.0 {
            continue;
        }
        if st.join == 1 {
            out.push(orient(circle_poly(p, hw, tol)));
            continue;
        }
        let s = if cross > 0.0 { -1.0 } else { 1.0 };
        let n0 = (-d0.1 * hw * s, d0.0 * hw * s);
        let n1 = (-d1.1 * hw * s, d1.0 * hw * s);
        let a = (p.0 + n0.0, p.1 + n0.1);
        let b = (p.0 + n1.0, p.1 + n1.1);
        let half = ((1.0 + dot.clamp(-1.0, 1.0)) / 2.0).sqrt(); // cos of half the angle between normals
        if st.join == 0 && half > 1e-4 && 1.0 / half <= st.miter {
            let (mx, my) = (n0.0 + n1.0, n0.1 + n1.1);
            let ml = (mx * mx + my * my).sqrt().max(1e-6);
            let len = hw / half;
            let m = (p.0 + mx / ml * len, p.1 + my / ml * len);
            out.push(orient(vec![p, a, m, b]));
        } else {
            out.push(orient(vec![p, a, b]));
        }
    }
    // Caps.
    if !closed {
        for (p, d) in [(pts[0], { let d = dir(0); (-d.0, -d.1) }), (pts[n - 1], dir(n - 2))] {
            match st.cap {
                1 => out.push(orient(circle_poly(p, hw, tol))),
                2 => {
                    let nrm = (-d.1 * hw, d.0 * hw);
                    let e = (d.0 * hw, d.1 * hw);
                    out.push(orient(vec![
                        (p.0 + nrm.0, p.1 + nrm.1),
                        (p.0 + nrm.0 + e.0, p.1 + nrm.1 + e.1),
                        (p.0 - nrm.0 + e.0, p.1 - nrm.1 + e.1),
                        (p.0 - nrm.0, p.1 - nrm.1),
                    ]));
                }
                _ => {}
            }
        }
    }
}

// ============================================================== painting

#[derive(Clone)]
struct Grad {
    radial: bool,
    /// Device space to gradient space.
    inv: Mat,
    coords: [f32; 5], // linear: x1 y1 x2 y2; radial: cx cy r fx fy
    spread: u8,
}

enum Source {
    Solid([f32; 4]),
    Grad(Box<Grad>, Vec<[f32; 4]>),
}

impl Source {
    #[inline]
    fn at(&self, x: f32, y: f32) -> [f32; 4] {
        match self {
            Source::Solid(c) => *c,
            Source::Grad(g, lut) => {
                let (gx, gy) = apply_m(&g.inv, x, y);
                let c = &g.coords;
                let mut t = if !g.radial {
                    let (dx, dy) = (c[2] - c[0], c[3] - c[1]);
                    let l = dx * dx + dy * dy;
                    if l == 0.0 { 1.0 } else { ((gx - c[0]) * dx + (gy - c[1]) * dy) / l }
                } else {
                    let (r, fx, fy) = (c[2], c[3], c[4]);
                    let (dx, dy) = (gx - fx, gy - fy);
                    let (ex, ey) = (c[0] - fx, c[1] - fy);
                    let a = ex * ex + ey * ey - r * r;
                    let b = dx * ex + dy * ey;
                    let c0 = dx * dx + dy * dy;
                    if a.abs() < 1e-9 {
                        if b == 0.0 { 0.0 } else { c0 / (2.0 * b) }
                    } else {
                        (b - (b * b - a * c0).max(0.0).sqrt()) / a
                    }
                };
                t = match g.spread {
                    1 => {
                        let m = t.rem_euclid(2.0);
                        if m > 1.0 { 2.0 - m } else { m }
                    }
                    2 => t.rem_euclid(1.0),
                    _ => t.clamp(0.0, 1.0),
                };
                lut[(t * 255.0).round() as usize]
            }
        }
    }
}

fn premul(c: u32, op: f32) -> [f32; 4] {
    let a = ((c >> 24) & 255) as f32 / 255.0 * op;
    [((c >> 16) & 255) as f32 / 255.0 * a, ((c >> 8) & 255) as f32 / 255.0 * a, (c & 255) as f32 / 255.0 * a, a]
}

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    fn fill(&mut self, cov: &Cov, src: &Source, clip: Option<&[f32]>) {
        for y in 0..cov.h {
            let py = cov.y0 as usize + y;
            for x in 0..cov.w {
                let mut c = cov.data[y * cov.w + x];
                if c <= 0.0 {
                    continue;
                }
                let px = cov.x0 as usize + x;
                let i = py * self.w + px;
                if let Some(m) = clip {
                    c *= m[i];
                    if c <= 0.0 {
                        continue;
                    }
                }
                let s = src.at(px as f32 + 0.5, py as f32 + 0.5);
                let d = &mut self.px[i];
                let k = 1.0 - s[3] * c;
                for j in 0..4 {
                    d[j] = s[j] * c + d[j] * k;
                }
            }
        }
    }
}

// ================================================================ render

pub struct Rendered {
    /// Intrinsic (CSS px) size.
    pub iw: f32,
    pub ih: f32,
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

struct Ctx<'a> {
    ids: FxHashMap<&'a str, &'a El>,
    rules: Vec<Rule>,
    canvas: Canvas,
    /// Viewport size (for percentages) of the innermost viewport.
    vp: (f32, f32),
    depth: u32,
}

/// Viewport transform from a viewBox to a (x, y, w, h) rectangle.
fn view_box(vb: Option<&str>, par: Option<&str>, x: f32, y: f32, w: f32, h: f32) -> (Mat, Option<(f32, f32)>) {
    let Some(v) = vb.map(numbers).filter(|v| v.len() == 4 && v[2] > 0.0 && v[3] > 0.0) else {
        return ([1.0, 0.0, 0.0, 1.0, x, y], None);
    };
    let (sx, sy) = (w / v[2], h / v[3]);
    let par = par.unwrap_or("xMidYMid meet").to_ascii_lowercase();
    if par.trim_start().starts_with("none") {
        return ([sx, 0.0, 0.0, sy, x - v[0] * sx, y - v[1] * sy], Some((v[2], v[3])));
    }
    let s = if par.contains("slice") { sx.max(sy) } else { sx.min(sy) };
    let ax = if par.contains("xmin") { 0.0 } else if par.contains("xmax") { 1.0 } else { 0.5 };
    let ay = if par.contains("ymin") { 0.0 } else if par.contains("ymax") { 1.0 } else { 0.5 };
    let tx = x + (w - v[2] * s) * ax - v[0] * s;
    let ty = y + (h - v[3] * s) * ay - v[1] * s;
    ([s, 0.0, 0.0, s, tx, ty], Some((v[2], v[3])))
}

impl<'a> Ctx<'a> {
    fn style(&self, e: &El, parent: &Style, anc: &[&El]) -> Style {
        let mut st = parent.child();
        let diag = ((self.vp.0 * self.vp.0 + self.vp.1 * self.vp.1) / 2.0).sqrt();
        for (k, v) in &e.attrs {
            if PROPS.contains(&k.as_str()) {
                apply(&mut st, k, v, diag);
            }
        }
        let mut matched: Vec<&Rule> = self.rules.iter().filter(|r| rule_matches(r, e, anc)).collect();
        matched.sort_by_key(|r| (r.spec, r.order));
        for r in matched {
            for (k, v) in &r.decls {
                apply(&mut st, k, v, diag);
            }
        }
        if let Some(s) = e.attr("style") {
            for (k, v) in parse_decls(s) {
                apply(&mut st, &k, &v, diag);
            }
        }
        st
    }

    fn len(&self, e: &El, name: &str, axis: u8, em: f32) -> f32 {
        let pct = match axis {
            0 => self.vp.0,
            1 => self.vp.1,
            _ => ((self.vp.0 * self.vp.0 + self.vp.1 * self.vp.1) / 2.0).sqrt(),
        };
        e.attr(name).and_then(|v| length(v, pct, em)).unwrap_or(0.0)
    }

    /// Geometry of a shape element in user space.
    fn shape(&self, e: &El, st: &Style, tol: f32) -> Option<Vec<Sub>> {
        let em = st.font_size;
        let l = |n: &str, axis: u8| self.len(e, n, axis, em);
        Some(match e.name.as_str() {
            "path" => parse_path(e.attr("d")?, tol),
            "rect" => {
                let (x, y, w, h) = (l("x", 0), l("y", 1), l("width", 0), l("height", 1));
                if w <= 0.0 || h <= 0.0 {
                    return None;
                }
                let (mut rx, mut ry) = (e.attr("rx").map(|_| l("rx", 0)), e.attr("ry").map(|_| l("ry", 1)));
                if rx.is_none() {
                    rx = ry;
                }
                if ry.is_none() {
                    ry = rx;
                }
                let (rx, ry) = (rx.unwrap_or(0.0).min(w / 2.0), ry.unwrap_or(0.0).min(h / 2.0));
                if rx <= 0.0 || ry <= 0.0 {
                    vec![Sub { pts: vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)], closed: true }]
                } else {
                    let mut pts = vec![(x + rx, y), (x + w - rx, y)];
                    arc(&mut pts, (x + w - rx, y), rx, ry, 0.0, false, true, (x + w, y + ry), tol);
                    pts.push((x + w, y + h - ry));
                    arc(&mut pts, (x + w, y + h - ry), rx, ry, 0.0, false, true, (x + w - rx, y + h), tol);
                    pts.push((x + rx, y + h));
                    arc(&mut pts, (x + rx, y + h), rx, ry, 0.0, false, true, (x, y + h - ry), tol);
                    pts.push((x, y + ry));
                    arc(&mut pts, (x, y + ry), rx, ry, 0.0, false, true, (x + rx, y), tol);
                    vec![Sub { pts, closed: true }]
                }
            }
            "circle" => {
                let r = l("r", 2);
                if r <= 0.0 {
                    return None;
                }
                vec![ellipse_sub(l("cx", 0), l("cy", 1), r, r, tol)]
            }
            "ellipse" => {
                let (rx, ry) = (l("rx", 0), l("ry", 1));
                let (rx, ry) = match (e.attr("rx").is_some(), e.attr("ry").is_some()) {
                    (true, false) => (rx, rx),
                    (false, true) => (ry, ry),
                    _ => (rx, ry),
                };
                if rx <= 0.0 || ry <= 0.0 {
                    return None;
                }
                vec![ellipse_sub(l("cx", 0), l("cy", 1), rx, ry, tol)]
            }
            "line" => vec![Sub { pts: vec![(l("x1", 0), l("y1", 1)), (l("x2", 0), l("y2", 1))], closed: false }],
            "polyline" | "polygon" => {
                let v = numbers(e.attr("points")?);
                let pts: Vec<(f32, f32)> = v.chunks_exact(2).map(|c| (c[0], c[1])).collect();
                vec![Sub { pts, closed: e.name == "polygon" }]
            }
            _ => return None,
        })
    }

    fn source(&self, p: &Paint, st: &Style, op: f32, ctm: &Mat, bbox: [f32; 4]) -> Option<Source> {
        match p {
            Paint::None => None,
            Paint::Color(c) => Some(Source::Solid(premul(*c, op))),
            Paint::Current => Some(Source::Solid(premul(st.color, op))),
            Paint::Url(id, fallback) => match self.ids.get(id.as_str()) {
                Some(g) if g.name.ends_with("gradient") => self.gradient(g, st, op, ctm, bbox),
                _ => self.source(fallback, st, op, ctm, bbox),
            },
        }
    }

    fn gradient(&self, g: &El, st: &Style, op: f32, ctm: &Mat, bbox: [f32; 4]) -> Option<Source> {
        // Attributes and stops may come from referenced gradients.
        let mut chain = vec![g];
        while let Some(h) = chain.last().unwrap().attr("href").and_then(|h| self.ids.get(h.trim_start_matches('#'))) {
            if chain.len() > 8 || chain.iter().any(|c| std::ptr::eq(*c, *h)) {
                break;
            }
            chain.push(h);
        }
        let attr = |n: &str| chain.iter().find_map(|e| e.attr(n));
        let radial = g.name == "radialgradient";
        let user = attr("gradientunits") == Some("userSpaceOnUse");
        let stops_el = chain.iter().find(|e| e.kids.iter().any(|k| matches!(k, Node::El(s) if s.name == "stop")))?;
        let mut stops = Vec::new();
        let mut last = 0.0f32;
        for k in &stops_el.kids {
            let Node::El(s) = k else { continue };
            if s.name != "stop" {
                continue;
            }
            let sst = self.style(s, st, &[]);
            let off = s.attr("offset").and_then(num).unwrap_or(0.0).clamp(0.0, 1.0).max(last);
            last = off;
            let c = premul(sst.stop_color, sst.stop_op * op);
            stops.push((off, c));
        }
        if stops.is_empty() {
            return None;
        }
        if stops.len() == 1 {
            return Some(Source::Solid(stops[0].1));
        }
        let (vw, vh) = self.vp;
        let diag = ((vw * vw + vh * vh) / 2.0).sqrt();
        let coord = |n: &str, def: f32, axis: u8| -> f32 {
            match attr(n) {
                Some(v) if user => length(v, [vw, vh, diag][axis as usize], st.font_size).unwrap_or(def),
                Some(v) => num(v).unwrap_or(def),
                None => {
                    if user {
                        def * [vw, vh, diag][axis as usize]
                    } else {
                        def
                    }
                }
            }
        };
        let coords = if radial {
            let (cx, cy, r) = (coord("cx", 0.5, 0), coord("cy", 0.5, 1), coord("r", 0.5, 2));
            let fx = if attr("fx").is_some() { coord("fx", 0.5, 0) } else { cx };
            let fy = if attr("fy").is_some() { coord("fy", 0.5, 1) } else { cy };
            [cx, cy, r, fx, fy]
        } else {
            [coord("x1", 0.0, 0), coord("y1", 0.0, 1), coord("x2", 1.0, 0), coord("y2", 0.0, 1), 0.0]
        };
        let units = if user { IDENT } else { [bbox[2], 0.0, 0.0, bbox[3], bbox[0], bbox[1]] };
        if !user && (bbox[2] <= 0.0 || bbox[3] <= 0.0) {
            return None;
        }
        let gt = attr("gradienttransform").map(parse_transform).unwrap_or(IDENT);
        let m = mul(&mul(ctm, &units), &gt);
        let inv = invert(&m)?;
        let spread = match attr("spreadmethod") {
            Some("reflect") => 1,
            Some("repeat") => 2,
            _ => 0,
        };
        let mut lut = vec![[0f32; 4]; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            let t = i as f32 / 255.0;
            let k = stops.iter().position(|s| s.0 >= t).unwrap_or(stops.len());
            *v = if k == 0 {
                stops[0].1
            } else if k == stops.len() {
                stops[k - 1].1
            } else {
                let (a, b) = (&stops[k - 1], &stops[k]);
                let f = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 1.0 };
                [0, 1, 2, 3].map(|j| a.1[j] + (b.1[j] - a.1[j]) * f)
            };
        }
        Some(Source::Grad(Box::new(Grad { radial, inv, coords, spread }), lut))
    }

    /// Coverage mask for a clip-path reference, in canvas pixels.
    fn clip_mask(&mut self, id: &str, ctm: &Mat, bbox: [f32; 4], st: &Style) -> Option<Vec<f32>> {
        let cp = *self.ids.get(id)?;
        if cp.name != "clippath" || self.depth > 16 {
            return None;
        }
        self.depth += 1;
        let mut m = *ctm;
        if cp.attr("clippathunits") == Some("objectBoundingBox") {
            m = mul(&m, &[bbox[2], 0.0, 0.0, bbox[3], bbox[0], bbox[1]]);
        }
        if let Some(t) = cp.attr("transform") {
            m = mul(&m, &parse_transform(t));
        }
        let (w, h) = (self.canvas.w, self.canvas.h);
        let mut mask = vec![0f32; w * h];
        let cst = self.style(cp, st, &[]);
        for k in &cp.kids {
            let Node::El(e) = k else { continue };
            let est = self.style(e, &cst, &[]);
            if est.hidden {
                continue;
            }
            let em = match e.attr("transform") {
                Some(t) => mul(&m, &parse_transform(t)),
                None => m,
            };
            let (target, tm) = if e.name == "use" {
                let Some(t) = e.attr("href").and_then(|h| self.ids.get(h.trim_start_matches('#'))) else { continue };
                (*t, mul(&em, &[1.0, 0.0, 0.0, 1.0, self.len(e, "x", 0, 16.0), self.len(e, "y", 1, 16.0)]))
            } else {
                (e, em)
            };
            let tol = 0.2 / scale_of(&tm);
            let Some(subs) = self.shape(target, &est, tol) else { continue };
            let polys: Vec<Vec<(f32, f32)>> = subs.iter().map(|s| s.pts.iter().map(|&(x, y)| apply_m(&tm, x, y)).collect()).collect();
            if let Some(cov) = rasterize(&polys, est.clip_evenodd, w, h) {
                for y in 0..cov.h {
                    for x in 0..cov.w {
                        let i = (cov.y0 as usize + y) * w + cov.x0 as usize + x;
                        mask[i] = mask[i].max(cov.data[y * cov.w + x]);
                    }
                }
            }
        }
        self.depth -= 1;
        let nested = cp.attr("clip-path").and_then(|v| paint(v, &Paint::None).url_id());
        if let Some(n) = nested {
            if let Some(outer) = self.clip_mask(&n, ctm, bbox, st) {
                mask.iter_mut().zip(outer).for_each(|(a, b)| *a *= b);
            }
        }
        Some(mask)
    }

    fn draw_shape(&mut self, e: &El, st: &Style, ctm: &Mat, op: f32, clip: Option<&[f32]>) {
        let tol = 0.2 / scale_of(ctm);
        let Some(subs) = self.shape(e, st, tol) else { return };
        let (mut bx0, mut by0, mut bx1, mut by1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for s in &subs {
            for &(x, y) in &s.pts {
                bx0 = bx0.min(x);
                by0 = by0.min(y);
                bx1 = bx1.max(x);
                by1 = by1.max(y);
            }
        }
        let bbox = [bx0, by0, bx1 - bx0, by1 - by0];
        let own_clip = st.clip.clone().and_then(|id| self.clip_mask(&id, ctm, bbox, st));
        let clip = merge_clip(clip, own_clip.as_deref());
        let clip = clip.as_deref();
        let (w, h) = (self.canvas.w, self.canvas.h);
        if let Some(src) = self.source(&st.fill, st, st.fill_op * op, ctm, bbox) {
            let polys: Vec<Vec<(f32, f32)>> = subs.iter().filter(|s| s.pts.len() > 2).map(|s| s.pts.iter().map(|&(x, y)| apply_m(ctm, x, y)).collect()).collect();
            if let Some(cov) = rasterize(&polys, st.evenodd, w, h) {
                self.canvas.fill(&cov, &src, clip);
            }
        }
        if st.sw > 0.0 {
            if let Some(src) = self.source(&st.stroke, st, st.stroke_op * op, ctm, bbox) {
                let mut polys = Vec::new();
                for s in &subs {
                    if st.dash.is_empty() {
                        stroke_polys(s, st, tol, &mut polys);
                    } else {
                        let dash: Vec<f32> = if st.dash.len() % 2 == 1 { st.dash.repeat(2) } else { st.dash.clone() };
                        for d in dashes(s, &dash, st.dash_off) {
                            stroke_polys(&d, st, tol, &mut polys);
                        }
                    }
                }
                let polys: Vec<Vec<(f32, f32)>> = polys.into_iter().map(|p| p.into_iter().map(|(x, y)| apply_m(ctm, x, y)).collect()).collect();
                if let Some(cov) = rasterize(&polys, false, w, h) {
                    self.canvas.fill(&cov, &src, clip);
                }
            }
        }
    }

    fn draw_text(&mut self, e: &El, st: &Style, ctm: &Mat, op: f32, clip: Option<&[f32]>) {
        let mut text = String::new();
        e.text(&mut text);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return;
        }
        let first = |n: &str| e.attr(n).map(|v| numbers(v).first().copied().unwrap_or(0.0)).unwrap_or(0.0);
        let (mut x, y) = (first("x") + first("dx"), first("y") + first("dy"));
        let f = crate::fonts::fonts();
        let family = if st.serif { crate::style::Family::Serif } else { crate::style::Family::Sans };
        let face = f.pick(family, st.bold, st.italic);
        let width = f.measure(face, st.font_size, &text);
        x -= match st.anchor {
            1 => width / 2.0,
            2 => width,
            _ => 0.0,
        };
        let Some(src) = self.source(&st.fill, st, st.fill_op * op, ctm, [x, y - st.font_size, width, st.font_size]) else { return };
        let s = scale_of(ctm);
        let size = st.font_size * s;
        let (ox, oy) = apply_m(ctm, x, y);
        let mut pen = ox;
        let (w, h) = (self.canvas.w, self.canvas.h);
        for c in text.chars() {
            let a = f.adv(face, c);
            if let Some(fd) = f.face(a.face) {
                let (m, bmp) = fd.font.rasterize(c, size);
                let gx = (pen + m.xmin as f32).round() as i32;
                let gy = (oy - m.ymin as f32 - m.height as f32).round() as i32;
                let x0 = gx.max(0);
                let y0 = gy.max(0);
                let x1 = (gx + m.width as i32).min(w as i32);
                let y1 = (gy + m.height as i32).min(h as i32);
                if x1 > x0 && y1 > y0 {
                    let (cw, chh) = ((x1 - x0) as usize, (y1 - y0) as usize);
                    let mut data = vec![0f32; cw * chh];
                    for yy in 0..chh {
                        for xx in 0..cw {
                            let bx = (x0 - gx) as usize + xx;
                            let by = (y0 - gy) as usize + yy;
                            data[yy * cw + xx] = bmp[by * m.width + bx] as f32 / 255.0;
                        }
                    }
                    self.canvas.fill(&Cov { x0, y0, w: cw, h: chh, data }, &src, clip);
                }
            }
            pen += a.adv * size;
        }
    }

    fn draw_image(&mut self, e: &El, ctm: &Mat, op: f32) {
        let Some(href) = e.attr("href") else { return };
        let Some(bytes) = crate::image::data_url(href) else { return };
        if self.depth > 4 {
            return;
        }
        let Some(img) = crate::image::decode(&bytes) else { return };
        let (x, y) = (self.len(e, "x", 0, 16.0), self.len(e, "y", 1, 16.0));
        let w = e.attr("width").map_or(img.iw, |_| self.len(e, "width", 0, 16.0));
        let h = e.attr("height").map_or(img.ih, |_| self.len(e, "height", 1, 16.0));
        let (m, _) = view_box(Some(&format!("0 0 {} {}", img.w, img.h)), e.attr("preserveaspectratio"), x, y, w, h);
        let full = mul(ctm, &m);
        let Some(inv) = invert(&full) else { return };
        let corners = [(0.0, 0.0), (img.w as f32, 0.0), (0.0, img.h as f32), (img.w as f32, img.h as f32)].map(|(a, b)| apply_m(&full, a, b));
        let x0 = corners.iter().map(|c| c.0).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let y0 = corners.iter().map(|c| c.1).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let x1 = (corners.iter().map(|c| c.0).fold(f32::MIN, f32::max).ceil() as usize).min(self.canvas.w);
        let y1 = (corners.iter().map(|c| c.1).fold(f32::MIN, f32::max).ceil() as usize).min(self.canvas.h);
        for py in y0..y1 {
            for px in x0..x1 {
                let (sx, sy) = apply_m(&inv, px as f32 + 0.5, py as f32 + 0.5);
                if sx < 0.0 || sy < 0.0 || sx >= img.w as f32 || sy >= img.h as f32 {
                    continue;
                }
                let p = img.px[sy as usize * img.w as usize + sx as usize];
                let s = premul(p, op);
                let d = &mut self.canvas.px[py * self.canvas.w + px];
                for j in 0..4 {
                    d[j] = s[j] + d[j] * (1.0 - s[3]);
                }
            }
        }
    }

    fn render(&mut self, e: &'a El, parent: &Style, ctm: &Mat, op: f32, clip: Option<&[f32]>, anc: &mut Vec<&'a El>) {
        if self.depth > 64 {
            return;
        }
        if matches!(e.name.as_str(), "defs" | "clippath" | "lineargradient" | "radialgradient" | "symbol" | "mask" | "pattern" | "marker" | "style" | "script" | "title" | "desc" | "metadata" | "filter") {
            return;
        }
        let st = self.style(e, parent, anc);
        if st.hidden {
            return;
        }
        let mut m = match e.attr("transform") {
            Some(t) => mul(ctm, &parse_transform(t)),
            None => *ctm,
        };
        let op = op * st.opacity;
        if op <= 0.0 {
            return;
        }
        let saved_vp = self.vp;
        match e.name.as_str() {
            "svg" | "g" | "a" | "switch" => {
                if e.name == "svg" && !anc.is_empty() {
                    let (x, y) = (self.len(e, "x", 0, 16.0), self.len(e, "y", 1, 16.0));
                    let w = if e.attr("width").is_some() { self.len(e, "width", 0, 16.0) } else { self.vp.0 };
                    let h = if e.attr("height").is_some() { self.len(e, "height", 1, 16.0) } else { self.vp.1 };
                    let (vb, size) = view_box(e.attr("viewbox"), e.attr("preserveaspectratio"), x, y, w, h);
                    m = mul(&m, &vb);
                    self.vp = size.unwrap_or((w, h));
                }
                let own = st.clip.clone().and_then(|id| self.clip_mask(&id, &m, [0.0, 0.0, self.vp.0, self.vp.1], &st));
                let clip = merge_clip(clip, own.as_deref());
                anc.push(e);
                for k in &e.kids {
                    if let Node::El(c) = k {
                        self.render(c, &st, &m, op, clip.as_deref(), anc);
                        if e.name == "switch" {
                            break;
                        }
                    }
                }
                anc.pop();
            }
            "use" => {
                let Some(t) = e.attr("href").and_then(|h| self.ids.get(h.trim_start_matches('#')).copied()) else { return };
                if anc.iter().any(|a| std::ptr::eq(*a, t)) {
                    return;
                }
                let (x, y) = (self.len(e, "x", 0, 16.0), self.len(e, "y", 1, 16.0));
                m = mul(&m, &[1.0, 0.0, 0.0, 1.0, x, y]);
                self.depth += 1;
                anc.push(e);
                if t.name == "symbol" || t.name == "svg" {
                    let w = e.attr("width").or(t.attr("width")).map_or(self.vp.0, |_| self.len(if e.attr("width").is_some() { e } else { t }, "width", 0, 16.0));
                    let h = e.attr("height").or(t.attr("height")).map_or(self.vp.1, |_| self.len(if e.attr("height").is_some() { e } else { t }, "height", 1, 16.0));
                    let (vb, size) = view_box(t.attr("viewbox"), t.attr("preserveaspectratio"), 0.0, 0.0, w, h);
                    let tm = mul(&m, &vb);
                    self.vp = size.unwrap_or((w, h));
                    let tst = self.style(t, &st, anc);
                    anc.push(t);
                    for k in &t.kids {
                        if let Node::El(c) = k {
                            self.render(c, &tst, &tm, op, clip, anc);
                        }
                    }
                    anc.pop();
                } else {
                    self.render(t, &st, &m, op, clip, anc);
                }
                anc.pop();
                self.depth -= 1;
            }
            "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon" => {
                if st.visible {
                    self.draw_shape(e, &st, &m, op, clip);
                }
            }
            "text" => {
                if st.visible {
                    let own = st.clip.clone().and_then(|id| self.clip_mask(&id, &m, [0.0, 0.0, self.vp.0, self.vp.1], &st));
                    let clip = merge_clip(clip, own.as_deref());
                    self.draw_text(e, &st, &m, op, clip.as_deref());
                }
            }
            "image" => {
                self.depth += 1;
                self.draw_image(e, &m, op);
                self.depth -= 1;
            }
            _ => {}
        }
        self.vp = saved_vp;
    }
}

fn merge_clip(a: Option<&[f32]>, b: Option<&[f32]>) -> Option<Vec<f32>> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(a.to_vec()),
        (None, Some(b)) => Some(b.to_vec()),
        (Some(a), Some(b)) => Some(a.iter().zip(b).map(|(x, y)| x * y).collect()),
    }
}

fn collect_ids<'a>(e: &'a El, ids: &mut FxHashMap<&'a str, &'a El>, css: &mut String) {
    if let Some(id) = e.attr("id") {
        ids.entry(id).or_insert(e);
    }
    if e.name == "style" {
        e.text(css);
        css.push('\n');
    }
    for k in &e.kids {
        if let Node::El(c) = k {
            collect_ids(c, ids, css);
        }
    }
}

/// The intrinsic size of an SVG document root (CSS px).
fn intrinsic(root: &El) -> (f32, f32) {
    let vb = root.attr("viewbox").map(numbers).filter(|v| v.len() == 4 && v[2] > 0.0 && v[3] > 0.0);
    let abs = |n: &str| root.attr(n).filter(|v| !v.trim().ends_with('%')).and_then(|v| length(v, 0.0, 16.0)).filter(|v| *v > 0.0);
    match (abs("width"), abs("height"), vb) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some(v)) => (w, w * v[3] / v[2]),
        (None, Some(h), Some(v)) => (h * v[2] / v[3], h),
        (Some(w), None, None) => (w, 150.0),
        (None, Some(h), None) => (300.0, h),
        (None, None, Some(v)) => (v[2], v[3]),
        (None, None, None) => (300.0, 150.0),
    }
}

/// Render an SVG document. `color` is `currentColor`; `scale` is the
/// rasterization density (device pixels per CSS px).
pub fn render(src: &str, color: u32, scale: Option<f32>) -> Option<Rendered> {
    let root = parse_xml(src)?;
    let (iw, ih) = intrinsic(&root);
    // Rasterize at a higher density so scaled-up icons stay sharp.
    let s = scale.unwrap_or_else(|| (768.0 / iw.max(ih)).clamp(1.0, 4.0)).min(4096.0 / iw.max(ih));
    let (w, h) = ((iw * s).round().max(1.0) as usize, (ih * s).round().max(1.0) as usize);
    let mut ids = FxHashMap::default();
    let mut css = String::new();
    collect_ids(&root, &mut ids, &mut css);
    let mut rules = Vec::new();
    parse_sheet(&css, &mut rules);
    let (vb, size) = view_box(root.attr("viewbox"), root.attr("preserveaspectratio"), 0.0, 0.0, iw, ih);
    let ctm = mul(&[s, 0.0, 0.0, s, 0.0, 0.0], &vb);
    let mut ctx = Ctx { ids, rules, canvas: Canvas { w, h, px: vec![[0.0; 4]; w * h] }, vp: size.unwrap_or((iw, ih)), depth: 0 };
    let base = Style::root(color);
    ctx.render(&root, &base, &ctm, 1.0, None, &mut Vec::new());
    let px = ctx
        .canvas
        .px
        .iter()
        .map(|p| {
            let a = p[3].clamp(0.0, 1.0);
            if a <= 0.0 {
                return 0;
            }
            let c = |v: f32| ((v / a).clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
            ((a * 255.0 + 0.5) as u32) << 24 | c(p[0]) << 16 | c(p[1]) << 8 | c(p[2])
        })
        .collect();
    Some(Rendered { iw, ih, w: w as u32, h: h as u32, px })
}

/// Does this look like an SVG document?
pub fn sniff(data: &[u8]) -> bool {
    let head = &data[..data.len().min(4096)];
    let s = String::from_utf8_lossy(head);
    let t = s.trim_start_matches('\u{feff}').trim_start();
    t.starts_with('<') && s.contains("<svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(r: &Rendered, x: f32, y: f32) -> u32 {
        let s = r.w as f32 / r.iw;
        r.px[(y * s) as usize * r.w as usize + (x * s) as usize]
    }

    #[test]
    fn shapes_paths_and_styles() {
        let svg = r##"<?xml version="1.0"?>
<!DOCTYPE svg>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 100 50" width="200">
  <style>.st0{fill:#00ff00} #big { fill: blue }</style>
  <defs>
    <linearGradient id="g" x1="0" x2="1"><stop offset="0" stop-color="#000"/><stop offset="100%" stop-color="#fff"/></linearGradient>
    <clipPath id="c"><rect x="60" y="0" width="10" height="50"/></clipPath>
    <path id="tri" d="M0,0 L10,0 L5,10z"/>
  </defs>
  <rect width="100" height="50" fill="url(#g)"/>
  <circle class="st0" cx="10" cy="10" r="8"/>
  <path d="M20 2h10v10h-10z" fill="red" stroke="black" stroke-width="2"/>
  <g transform="translate(40 0) scale(2)" fill="currentColor"><rect width="5" height="5"/></g>
  <rect id="big" x="55" y="20" width="30" height="30" clip-path="url(#c)"/>
  <use xlink:href="#tri" x="80" y="0" fill="#f0f"/>
  <path d="M5 30 a10 10 0 1 0 20 0 a10 10 0 1 0 -20 0" fill="none" stroke="#ff0000" stroke-width="3"/>
</svg>"##;
        let r = render(svg, 0xFFFF8800, Some(2.0)).unwrap();
        assert_eq!((r.iw, r.ih), (200.0, 100.0));
        assert_eq!((r.w, r.h), (400, 200));
        assert_eq!(at(&r, 20.0, 20.0), 0xFF00FF00, "class rule fill");
        assert_eq!(at(&r, 50.0, 14.0), 0xFFFF0000, "path fill");
        assert_eq!(at(&r, 40.5, 14.0) >> 24, 255, "stroke");
        assert_eq!(at(&r, 85.0, 5.0), 0xFFFF8800, "currentColor in transformed group");
        assert_eq!(at(&r, 130.0, 80.0), 0xFF0000FF, "inside clip");
        let outside = at(&r, 150.0, 80.0);
        assert_ne!(outside, 0xFF0000FF, "outside clip is gradient");
        assert_eq!(at(&r, 170.0, 4.0), 0xFFFF00FF, "use");
        assert_eq!(at(&r, 10.0, 60.0), 0xFFFF0000, "arc stroke");
        // Gradient runs dark to light.
        assert!((at(&r, 2.0, 98.0) & 0xFF) < (at(&r, 198.0, 98.0) & 0xFF));
    }

    #[test]
    fn path_syntax() {
        let s = parse_path("M10-20l.5.5L1e1,2 1 2z m 3 3 h1", 0.1);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].pts[..4], [(10.0, -20.0), (10.5, -19.5), (10.0, 2.0), (1.0, 2.0)]);
        assert!(s[0].closed);
        assert_eq!(s[1].pts, vec![(13.0, -17.0), (14.0, -17.0)]);
        let arc = parse_path("M0 0a5 5 0 1010 0", 0.1);
        assert!((arc[0].pts.last().unwrap().0 - 10.0).abs() < 1e-3);
    }
}
