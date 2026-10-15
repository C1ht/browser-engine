//! Single-pass HTML tokenizer + tree builder.
//!
//! Works directly on bytes, scans for '<' / '&' with a word-at-a-time memchr,
//! and builds the arena DOM as it goes (no token objects are materialized).
//! Implements the parts of the HTML5 tree-construction rules that matter for
//! real pages: implicit html/head/body, auto-closing p/li/dt/dd/td/tr/option,
//! void elements, raw-text elements and character references.

use crate::atom::{self, intern, intern_lower, k, Atom};
use crate::dom::*;

/// Find `needle` in `hay`, 8 bytes at a time (SWAR "has zero byte" trick).
#[inline]
pub fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let rep = LO.wrapping_mul(needle as u64);
    let n = hay.len();
    let mut i = 0;
    while i + 8 <= n {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap()) ^ rep;
        let z = w.wrapping_sub(LO) & !w & HI;
        if z != 0 {
            return Some(i + (z.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < n {
        if hay[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find(hay: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    let mut i = from;
    while i < hay.len() {
        let p = i + memchr(pat[0], &hay[i..])?;
        if hay[p..].starts_with(pat) {
            return Some(p);
        }
        i = p + 1;
    }
    None
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\t' | b'\r' | b'\x0c')
}

pub fn parse(src: &str) -> Document {
    let mut p = Parser::new();
    p.run(src);
    p.post_process();
    p.doc
}

struct Parser {
    doc: Document,
    stack: Vec<NodeId>,
    selects: Vec<NodeId>,
}

fn new_elem(tag: Atom) -> Data {
    Data::Element(Element { tag, id: None, classes: Vec::new(), attrs: Vec::new() })
}

fn is_void(t: Atom) -> bool {
    use k::*;
    matches!(
        t,
        area | base | br | col | embed | hr | img | input | link | meta | param | source | track
            | wbr
    )
}

fn closes_p(t: Atom) -> bool {
    use k::*;
    matches!(
        t,
        address | article | aside | blockquote | center | details | div | dl | fieldset
            | figure | footer | form | h1 | h2 | h3 | h4 | h5 | h6 | header | hr | main | menu
            | nav | ol | p | pre | section | table | ul | li | dd | dt | listing | xmp
    )
}

fn is_heading(t: Atom) -> bool {
    use k::*;
    matches!(t, h1 | h2 | h3 | h4 | h5 | h6)
}

impl Parser {
    fn new() -> Self {
        let mut doc = Document::new();
        let html = doc.add(new_elem(k::html));
        doc.append(0, html);
        let head = doc.add(new_elem(k::head));
        doc.append(html, head);
        let body = doc.add(new_elem(k::body));
        doc.append(html, body);
        doc.html = html;
        doc.body = body;
        Parser { doc, stack: vec![body], selects: Vec::new() }
    }

    #[inline]
    fn cur(&self) -> NodeId {
        *self.stack.last().unwrap()
    }

    #[inline]
    fn stack_tag(&self, idx: usize) -> Atom {
        self.doc.tag(self.stack[idx]).unwrap()
    }

    fn run(&mut self, src: &str) {
        let b = src.as_bytes();
        let n = b.len();
        let mut i = 0;
        while i < n {
            if b[i] != b'<' {
                let end = memchr(b'<', &b[i..]).map_or(n, |p| i + p);
                self.text(&src[i..end]);
                i = end;
                continue;
            }
            let next = if i + 1 < n { b[i + 1] } else { 0 };
            match next {
                b'!' => {
                    if b.len() >= i + 9 && b[i + 2..i + 9].eq_ignore_ascii_case(b"doctype") {
                        self.doc.quirks = false;
                    }
                    i = if b[i..].starts_with(b"<!--") {
                        find(b, i + 4, b"-->").map_or(n, |p| p + 3)
                    } else {
                        memchr(b'>', &b[i..]).map_or(n, |p| i + p + 1)
                    };
                }
                b'?' => i = memchr(b'>', &b[i..]).map_or(n, |p| i + p + 1),
                b'/' => {
                    let s = i + 2;
                    let mut j = s;
                    while j < n && !is_ws(b[j]) && b[j] != b'>' && b[j] != b'/' {
                        j += 1;
                    }
                    let gt = memchr(b'>', &b[j..]).map_or(n, |p| j + p + 1);
                    if j > s && b[s].is_ascii_alphabetic() {
                        self.end_tag(intern_lower(&src[s..j]));
                    }
                    i = gt;
                }
                c if c.is_ascii_alphabetic() => i = self.start_tag(src, i + 1),
                _ => {
                    self.text("<");
                    i += 1;
                }
            }
        }
    }

    fn start_tag(&mut self, src: &str, mut i: usize) -> usize {
        let b = src.as_bytes();
        let n = b.len();
        let s = i;
        while i < n && !is_ws(b[i]) && b[i] != b'>' && b[i] != b'/' {
            i += 1;
        }
        let tag = intern_lower(&src[s..i]);
        let mut attrs: Vec<(Atom, String)> = Vec::new();
        let mut self_closing = false;
        loop {
            while i < n && is_ws(b[i]) {
                i += 1;
            }
            if i >= n {
                break;
            }
            if b[i] == b'>' {
                i += 1;
                break;
            }
            if b[i] == b'/' {
                i += 1;
                if i < n && b[i] == b'>' {
                    self_closing = true;
                    i += 1;
                    break;
                }
                continue;
            }
            let s = i;
            while i < n && !is_ws(b[i]) && b[i] != b'=' && b[i] != b'>' && b[i] != b'/' {
                i += 1;
            }
            if i == s {
                i += 1; // stray '='
                continue;
            }
            let name = intern_lower(&src[s..i]);
            while i < n && is_ws(b[i]) {
                i += 1;
            }
            let mut val = String::new();
            if i < n && b[i] == b'=' {
                i += 1;
                while i < n && is_ws(b[i]) {
                    i += 1;
                }
                if i < n && (b[i] == b'"' || b[i] == b'\'') {
                    let q = b[i];
                    i += 1;
                    let e = memchr(q, &b[i..]).map_or(n, |p| i + p);
                    val = decode_entities(&src[i..e]);
                    i = (e + 1).min(n);
                } else {
                    let s = i;
                    while i < n && !is_ws(b[i]) && b[i] != b'>' {
                        i += 1;
                    }
                    val = decode_entities(&src[s..i]);
                }
            }
            if !attrs.iter().any(|(a, _)| *a == name) {
                attrs.push((name, val));
            }
        }
        let raw = matches!(tag, k::script | k::style | k::textarea | k::title | k::xmp);
        if raw && !self_closing {
            let (end, after) = find_end_tag(b, i, atom::name(tag).as_bytes());
            self.start(tag, attrs, false, Some(&src[i..end]));
            return after;
        }
        if tag == k::plaintext {
            self.start(k::pre, attrs, false, Some(&src[i..]));
            return n;
        }
        self.start(tag, attrs, self_closing, None);
        i
    }

    fn in_foreign(&self) -> bool {
        self.stack.iter().rev().any(|&n| matches!(self.doc.tag(n), Some(k::svg) | Some(k::math)))
    }

    fn start(&mut self, tag: Atom, attrs: Vec<(Atom, String)>, self_closing: bool, raw: Option<&str>) {
        match tag {
            k::html => return self.merge_attrs(self.doc.html, attrs),
            k::body => return self.merge_attrs(self.doc.body, attrs),
            k::head | k::meta | k::script => return,
            k::title if !self.in_foreign() => {
                if self.doc.title.is_empty() {
                    let t = decode_entities(raw.unwrap_or(""));
                    self.doc.title = t.split_whitespace().collect::<Vec<_>>().join(" ");
                }
                return;
            }
            k::style if !self.in_foreign() => {
                if !is_print_only(attr(&attrs, k::media)) {
                    self.doc.styles.push(raw.unwrap_or("").to_owned());
                }
                return;
            }
            k::link => {
                let rel = attr(&attrs, k::rel).unwrap_or("").to_ascii_lowercase();
                if rel.split_whitespace().any(|r| r == "stylesheet")
                    && !rel.contains("alternate")
                    && !is_print_only(attr(&attrs, k::media))
                {
                    if let Some(h) = attr(&attrs, k::href) {
                        self.doc.stylesheet_links.push(h.trim().to_owned());
                    }
                }
                return;
            }
            k::base => {
                if self.doc.base.is_none() {
                    self.doc.base = attr(&attrs, k::href).map(|s| s.trim().to_owned());
                }
                return;
            }
            _ => {}
        }
        self.implicit_close(tag);

        let mut el = Element { tag, id: None, classes: Vec::new(), attrs };
        if let Some(id) = el.attr(k::id) {
            let id = id.trim();
            if !id.is_empty() {
                el.id = Some(intern(id));
            }
        }
        if let Some(c) = el.attr(k::class) {
            let mut classes = Vec::new();
            for c in c.split_ascii_whitespace() {
                let a = intern(c);
                if !classes.contains(&a) {
                    classes.push(a);
                }
            }
            el.classes = classes;
        }
        let synth = if tag == k::input { input_text(&el) } else { None };
        let id = self.doc.add(Data::Element(el));
        let parent = self.cur();
        self.doc.append(parent, id);

        if let Some(t) = synth {
            self.doc.append_text(id, &t);
        }
        if tag == k::select {
            self.selects.push(id);
        }
        if let Some(t) = raw {
            let t = if tag == k::textarea { decode_entities(t) } else { t.to_owned() };
            let t = t.strip_prefix('\n').unwrap_or(&t);
            if !t.is_empty() {
                self.doc.append_text(id, t);
            }
            return;
        }
        if is_void(tag) || self_closing {
            return;
        }
        if self.stack.len() < 1000 {
            self.stack.push(id);
        }
    }

    fn merge_attrs(&mut self, node: NodeId, attrs: Vec<(Atom, String)>) {
        if let Data::Element(e) = &mut self.doc.nodes[node as usize].data {
            for (n, v) in attrs {
                if e.attr(n).is_none() {
                    if n == k::id {
                        e.id = Some(intern(v.trim()));
                    } else if n == k::class {
                        e.classes.extend(v.split_ascii_whitespace().map(intern));
                    }
                    e.attrs.push((n, v));
                }
            }
        }
    }

    /// Pop up to and including the nearest `targets` element, unless a
    /// `boundaries` element is found first.
    fn close_until(&mut self, targets: &[Atom], boundaries: &[Atom]) {
        for idx in (1..self.stack.len()).rev() {
            let t = self.stack_tag(idx);
            if targets.contains(&t) {
                self.stack.truncate(idx);
                return;
            }
            if boundaries.contains(&t) {
                return;
            }
        }
    }

    fn implicit_close(&mut self, tag: Atom) {
        use k::*;
        if closes_p(tag) {
            self.close_until(&[p], &[button, table, td, th, caption, object]);
        }
        match tag {
            li => self.close_until(&[li], &[ul, ol, menu, table, td, th]),
            dt | dd => self.close_until(&[dt, dd], &[dl, table, td, th]),
            tr => self.close_until(&[tr], &[table, tbody, thead, tfoot]),
            td | th => self.close_until(&[td, th], &[tr, table]),
            tbody | thead | tfoot => self.close_until(&[tbody, thead, tfoot, tr, td, th], &[table]),
            option | optgroup => {
                if self.stack.len() > 1 && self.stack_tag(self.stack.len() - 1) == option {
                    self.stack.pop();
                }
            }
            a => self.close_until(&[a], &[table, td, th]),
            _ if is_heading(tag) => {
                if self.stack.len() > 1 && is_heading(self.stack_tag(self.stack.len() - 1)) {
                    self.stack.pop();
                }
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: Atom) {
        use k::*;
        match tag {
            html | head | body => return,
            br => return self.start(br, Vec::new(), true, None),
            _ => {}
        }
        let boundaries: &[Atom] = match tag {
            td | th | tr | tbody | thead | tfoot | caption => &[table],
            table => &[],
            _ => &[td, th, table, caption],
        };
        self.close_until(&[tag], boundaries);
    }

    fn text(&mut self, s: &str) {
        let cur = self.cur();
        if memchr(b'&', s.as_bytes()).is_some() {
            self.doc.append_text(cur, &decode_entities(s));
        } else {
            self.doc.append_text(cur, s);
        }
    }

    /// <select> shows its selected option's text.
    fn post_process(&mut self) {
        for sel in std::mem::take(&mut self.selects) {
            let mut chosen = NONE;
            let mut stack = vec![sel];
            while let Some(n) = stack.pop() {
                for c in self.doc.children(n) {
                    if self.doc.tag(c) == Some(k::option) {
                        if chosen == NONE || self.doc.attr(c, k::selected).is_some() {
                            chosen = c;
                        }
                    } else if self.doc.tag(c) == Some(k::optgroup) {
                        stack.push(c);
                    }
                }
            }
            let mut t = String::new();
            if chosen != NONE {
                self.doc.text_content(chosen, &mut t);
            }
            let t = format!("{} \u{25be}", t.split_whitespace().collect::<Vec<_>>().join(" "));
            self.doc.append_text(sel, &t);
        }
    }
}

fn attr(attrs: &[(Atom, String)], name: Atom) -> Option<&str> {
    attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
}

fn is_print_only(media: Option<&str>) -> bool {
    match media {
        Some(m) => {
            let m = m.to_ascii_lowercase();
            m.contains("print") && !m.contains("screen") && !m.contains("all")
        }
        None => false,
    }
}

/// Text shown inside an <input>.
fn input_text(el: &Element) -> Option<String> {
    let ty = el.attr(k::type_).unwrap_or("text").to_ascii_lowercase();
    let value = el.attr(k::value).filter(|v| !v.is_empty());
    let t = match ty.as_str() {
        "hidden" | "checkbox" | "radio" | "file" | "range" | "color" => return None,
        "submit" => value.unwrap_or("Submit").to_owned(),
        "reset" => value.unwrap_or("Reset").to_owned(),
        "button" => value.unwrap_or("").to_owned(),
        "image" => el.attr(k::alt).unwrap_or("").to_owned(),
        "password" => "\u{2022}".repeat(value.map_or(0, |v| v.chars().count())),
        _ => value.or(el.attr(k::placeholder)).unwrap_or("").to_owned(),
    };
    // A non-breaking space keeps empty fields one line tall.
    Some(if t.is_empty() { "\u{a0}".to_owned() } else { t })
}

/// Returns (content_end, index_after_close_tag).
fn find_end_tag(b: &[u8], from: usize, name: &[u8]) -> (usize, usize) {
    let mut i = from;
    while let Some(p) = find(b, i, b"</") {
        let s = p + 2;
        let e = s + name.len();
        if e <= b.len()
            && b[s..e].eq_ignore_ascii_case(name)
            && (e == b.len() || is_ws(b[e]) || b[e] == b'>' || b[e] == b'/')
        {
            let after = memchr(b'>', &b[e..]).map_or(b.len(), |q| e + q + 1);
            return (p, after);
        }
        i = p + 2;
    }
    (b.len(), b.len())
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&", "lt" => "<", "gt" => ">", "quot" => "\"", "apos" => "'",
        "nbsp" => "\u{a0}", "copy" => "©", "reg" => "®", "trade" => "™",
        "mdash" => "—", "ndash" => "–", "hellip" => "…", "laquo" => "«", "raquo" => "»",
        "lsquo" => "‘", "rsquo" => "’", "ldquo" => "“", "rdquo" => "”", "sbquo" => "‚",
        "bdquo" => "„", "bull" => "•", "middot" => "·", "times" => "×", "divide" => "÷",
        "euro" => "€", "pound" => "£", "yen" => "¥", "cent" => "¢", "deg" => "°",
        "para" => "¶", "sect" => "§", "larr" => "←", "rarr" => "→", "uarr" => "↑",
        "darr" => "↓", "harr" => "↔", "hearts" => "♥", "plusmn" => "±", "frac12" => "½",
        "frac14" => "¼", "frac34" => "¾", "sup2" => "²", "sup3" => "³", "iexcl" => "¡",
        "iquest" => "¿", "shy" | "zwj" | "zwnj" | "lrm" | "rlm" => "", "ensp" => "\u{2002}",
        "emsp" => "\u{2003}", "thinsp" => "\u{2009}", "prime" => "′", "Prime" => "″",
        "dagger" => "†", "Dagger" => "‡", "permil" => "‰", "lsaquo" => "‹", "rsaquo" => "›",
        "minus" => "−", "le" => "≤", "ge" => "≥", "ne" => "≠", "infin" => "∞",
        "asymp" => "≈", "aacute" => "á", "eacute" => "é", "iacute" => "í", "oacute" => "ó",
        "uacute" => "ú", "Aacute" => "Á", "Eacute" => "É", "Iacute" => "Í", "Oacute" => "Ó",
        "Uacute" => "Ú", "agrave" => "à", "egrave" => "è", "igrave" => "ì", "ograve" => "ò",
        "ugrave" => "ù", "Agrave" => "À", "Egrave" => "È", "auml" => "ä", "euml" => "ë",
        "iuml" => "ï", "ouml" => "ö", "uuml" => "ü", "Auml" => "Ä", "Ouml" => "Ö",
        "Uuml" => "Ü", "szlig" => "ß", "ccedil" => "ç", "Ccedil" => "Ç", "ntilde" => "ñ",
        "Ntilde" => "Ñ", "acirc" => "â", "ecirc" => "ê", "icirc" => "î", "ocirc" => "ô",
        "ucirc" => "û", "aring" => "å", "Aring" => "Å", "aelig" => "æ", "AElig" => "Æ",
        "oslash" => "ø", "Oslash" => "Ø", "atilde" => "ã", "otilde" => "õ", "alpha" => "α",
        "beta" => "β", "gamma" => "γ", "delta" => "δ", "epsilon" => "ε", "theta" => "θ",
        "lambda" => "λ", "mu" => "μ", "pi" => "π", "sigma" => "σ", "tau" => "τ",
        "phi" => "φ", "omega" => "ω", "Delta" => "Δ", "Sigma" => "Σ", "Omega" => "Ω",
        "check" => "✓", "star" => "☆", "rang" => "⟩", "lang" => "⟨", "sdot" => "⋅",
        "uml" => "¨", "acute" => "´", "cedil" => "¸", "ordf" => "ª", "ordm" => "º",
        "not" => "¬", "macr" => "¯", "micro" => "µ", "curren" => "¤", "brvbar" => "¦",
        _ => return None,
    })
}

pub fn decode_entities(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut last = 0;
    while let Some(p) = memchr(b'&', &b[i..]) {
        let amp = i + p;
        i = amp + 1;
        let mut j = amp + 1;
        if j < b.len() && b[j] == b'#' {
            j += 1;
            let hex = j < b.len() && (b[j] == b'x' || b[j] == b'X');
            if hex {
                j += 1;
            }
            let ds = j;
            while j < b.len()
                && (if hex { b[j].is_ascii_hexdigit() } else { b[j].is_ascii_digit() })
                && j - ds < 8
            {
                j += 1;
            }
            if j == ds {
                continue;
            }
            let v = u32::from_str_radix(&s[ds..j], if hex { 16 } else { 10 }).unwrap_or(0xfffd);
            let v = match v {
                0x80 => 0x20ac,
                0x91 => 0x2018,
                0x92 => 0x2019,
                0x93 => 0x201c,
                0x94 => 0x201d,
                0x96 => 0x2013,
                0x97 => 0x2014,
                0 => 0xfffd,
                v => v,
            };
            let c = char::from_u32(v).unwrap_or('\u{fffd}');
            if j < b.len() && b[j] == b';' {
                j += 1;
            }
            out.push_str(&s[last..amp]);
            out.push(c);
            last = j;
            i = j;
        } else {
            while j < b.len() && b[j].is_ascii_alphanumeric() && j - amp < 33 {
                j += 1;
            }
            let name = &s[amp + 1..j];
            let semi = j < b.len() && b[j] == b';';
            if let Some(rep) = named_entity(name) {
                // Legacy entities are allowed without ';' (but not inside a longer word).
                let legacy = matches!(name, "amp" | "lt" | "gt" | "quot" | "nbsp" | "copy" | "reg");
                if semi || legacy {
                    out.push_str(&s[last..amp]);
                    out.push_str(rep);
                    last = if semi { j + 1 } else { j };
                    i = last;
                }
            }
        }
    }
    out.push_str(&s[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memchr_works() {
        let s = b"hello world, this is a <test> string";
        assert_eq!(memchr(b'<', s), Some(23));
        assert_eq!(memchr(b'z', s), None);
        assert_eq!(memchr(b'h', s), Some(0));
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#65;&#x42; &copy x&nbsp;y"), "a & b <c> AB © x\u{a0}y");
        assert_eq!(decode_entities("AT&T &unknown;"), "AT&T &unknown;");
    }

    #[test]
    fn tree() {
        let d = parse("<p>one<p>two<ul><li>a<li>b</ul><table><tr><td>x<td>y</table>");
        let body: Vec<_> = d.children(d.body).map(|c| d.tag(c)).collect();
        assert_eq!(body, vec![Some(k::p), Some(k::p), Some(k::ul), Some(k::table)]);
        let ul = d.children(d.body).nth(2).unwrap();
        assert_eq!(d.children(ul).count(), 2);
        let table = d.children(d.body).nth(3).unwrap();
        let tr = d.children(table).next().unwrap();
        assert_eq!(d.children(tr).count(), 2);
    }
}
