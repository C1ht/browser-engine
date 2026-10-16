//! HTML parser: a tokenizer and tree builder following the WHATWG parsing
//! algorithm — insertion modes, the list of active formatting elements with
//! the adoption agency algorithm, foster parenting, `<template>` contents,
//! foreign content (SVG / MathML) and fragment parsing for `innerHTML`.
//!
//! The tokenizer works on bytes: text runs are found with a word-at-a-time
//! memchr and handed to the tree builder in bulk, and tag / attribute names
//! are interned to atoms as they are read.

use crate::atom::{self, intern_lower, k, Atom};
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

fn all_ws(s: &str) -> bool {
    s.bytes().all(is_ws)
}

fn split_ws(s: &str) -> (&str, &str) {
    let n = s.bytes().take_while(|&b| is_ws(b)).count();
    (&s[..n], &s[n..])
}

/// Parse a complete document.
pub fn parse(src: &str) -> Document {
    let mut b = Builder::new(None);
    b.feed(src);
    b.finish()
}

/// Parse `src` as the contents of a `context` element (`innerHTML`). The
/// parsed nodes are the children of the returned document's `html` node.
pub fn parse_fragment(src: &str, context: Atom, ns: Ns) -> Document {
    let mut b = Builder::new(Some((context, ns)));
    b.feed(src);
    b.finish()
}

// ------------------------------------------------------------------ tokens

#[derive(Clone)]
struct Tag {
    name: Atom,
    attrs: Vec<(Atom, String)>,
    self_closing: bool,
}

fn tag(name: Atom) -> Tag {
    Tag { name, attrs: Vec::new(), self_closing: false }
}

impl Tag {
    fn attr(&self, name: Atom) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }
}

enum Tok {
    Doctype { name: String, public: Option<String>, system: Option<String>, force_quirks: bool },
    Start(Tag),
    End(Atom),
    Comment(String),
    Text(String),
    Eof,
}

/// Tokenizer content model, switched by the tree builder.
#[derive(Clone, Copy, PartialEq)]
enum TState {
    Data,
    RcData(Atom),
    RawText(Atom),
    Script,
    PlainText,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
}

enum Fmt {
    Marker,
    El(NodeId, Tag),
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Default,
    ListItem,
    Button,
    Table,
}

struct Builder {
    doc: Document,
    open: Vec<NodeId>,
    afe: Vec<Fmt>,
    mode: Mode,
    orig: Mode,
    tmpl_modes: Vec<Mode>,
    head: NodeId,
    form: NodeId,
    frameset_ok: bool,
    foster: bool,
    table_text: String,
    /// Fragment parsing context element (detached), or NONE.
    context: NodeId,
    ts: TState,
    skip_lf: bool,
    stopped: bool,
    /// Raw text from a fragment context element runs to the end of input.
    raw_to_eof: bool,
}

// ---------------------------------------------------------------- tokenizer

impl Builder {
    fn new(ctx: Option<(Atom, Ns)>) -> Builder {
        let mut b = Builder {
            doc: Document::new(),
            open: Vec::new(),
            afe: Vec::new(),
            mode: Mode::Initial,
            orig: Mode::Initial,
            tmpl_modes: Vec::new(),
            head: NONE,
            form: NONE,
            frameset_ok: true,
            foster: false,
            table_text: String::new(),
            context: NONE,
            ts: TState::Data,
            skip_lf: false,
            stopped: false,
            raw_to_eof: false,
        };
        if let Some((t, ns)) = ctx {
            b.doc.quirks = false;
            let root = b.doc.add(Data::Element(Element::new(k::html, Ns::Html, Vec::new())));
            b.doc.append(0, root);
            b.doc.html = root;
            b.open.push(root);
            b.context = b.doc.add(Data::Element(Element::new(t, ns, Vec::new())));
            if ns == Ns::Html {
                b.ts = match t {
                    k::title | k::textarea => TState::RcData(t),
                    k::style | k::xmp | k::iframe | k::noembed | k::noframes | k::noscript => TState::RawText(t),
                    k::script => TState::Script,
                    k::plaintext => TState::PlainText,
                    _ => TState::Data,
                };
                b.raw_to_eof = b.ts != TState::Data;
                if t == k::template {
                    b.tmpl_modes.push(Mode::InTemplate);
                }
            }
            b.reset_mode();
        }
        b
    }

    fn feed(&mut self, src: &str) {
        let owned;
        let src = if memchr(b'\r', src.as_bytes()).is_some() {
            owned = src.replace("\r\n", "\n").replace('\r', "\n");
            owned.as_str()
        } else {
            src
        };
        let b = src.as_bytes();
        let n = b.len();
        let mut i = 0;
        while i < n && !self.stopped {
            match self.ts {
                TState::PlainText => {
                    self.emit(Tok::Text(src[i..].replace('\0', "\u{fffd}")));
                    i = n;
                }
                TState::RcData(t) | TState::RawText(t) => {
                    let (end, after) = if std::mem::take(&mut self.raw_to_eof) { (n, n) } else { find_end_tag(src, i, atom::name(t).as_bytes()) };
                    if end > i {
                        let s = &src[i..end];
                        let s = if matches!(self.ts, TState::RcData(_)) { decode(s, false) } else { s.to_owned() };
                        self.emit(Tok::Text(s.replace('\0', "\u{fffd}")));
                    }
                    self.ts = TState::Data;
                    if end < n {
                        self.emit(Tok::End(t));
                    }
                    i = after;
                }
                TState::Script => {
                    let (end, after) = if std::mem::take(&mut self.raw_to_eof) { (n, n) } else { find_script_end(src, i) };
                    if end > i {
                        self.emit(Tok::Text(src[i..end].replace('\0', "\u{fffd}")));
                    }
                    self.ts = TState::Data;
                    if end < n {
                        self.emit(Tok::End(k::script));
                    }
                    i = after;
                }
                TState::Data => {
                    if b[i] != b'<' {
                        let end = memchr(b'<', &b[i..]).map_or(n, |p| i + p);
                        let s = &src[i..end];
                        let t = if memchr(b'&', s.as_bytes()).is_some() { decode(s, false) } else { s.to_owned() };
                        if !t.is_empty() {
                            self.emit(Tok::Text(t));
                        }
                        i = end;
                    } else {
                        i = self.markup(src, i);
                    }
                }
            }
        }
        if !self.stopped {
            self.emit(Tok::Eof);
        }
    }

    /// Tokenize markup starting at `<` (src[i]); returns the index after it.
    fn markup(&mut self, src: &str, i: usize) -> usize {
        let b = src.as_bytes();
        let n = b.len();
        let next = b.get(i + 1).copied().unwrap_or(0);
        match next {
            b'!' => {
                if b[i..].starts_with(b"<!--") {
                    let s = i + 4;
                    if b[s..].starts_with(b">") {
                        self.emit(Tok::Comment(String::new()));
                        return s + 1;
                    }
                    if b[s..].starts_with(b"->") {
                        self.emit(Tok::Comment(String::new()));
                        return s + 2;
                    }
                    let e1 = find(b, s, b"-->");
                    let e2 = find(b, s, b"--!>");
                    let (end, after) = match (e1, e2) {
                        (Some(a), Some(c)) if c < a => (c, c + 4),
                        (Some(a), _) => (a, a + 3),
                        (None, Some(c)) => (c, c + 4),
                        (None, None) => (n, n),
                    };
                    let mut c = &src[s..end];
                    if end == n {
                        c = c.strip_suffix("--").or_else(|| c.strip_suffix('-')).unwrap_or(c);
                    }
                    self.emit(Tok::Comment(c.to_owned()));
                    return after;
                }
                if n >= i + 9 && b[i + 2..i + 9].eq_ignore_ascii_case(b"doctype") {
                    return self.doctype(src, i + 9);
                }
                if b[i..].starts_with(b"<![CDATA[") && self.cdata_allowed() {
                    let s = i + 9;
                    let (end, after) = find(b, s, b"]]>").map_or((n, n), |p| (p, p + 3));
                    if end > s {
                        self.emit(Tok::Text(src[s..end].to_owned()));
                    }
                    return after;
                }
                self.bogus_comment(src, i + 2)
            }
            b'?' => self.bogus_comment(src, i + 1),
            b'/' => {
                let s = i + 2;
                if s >= n {
                    self.emit(Tok::Text("</".into()));
                    return n;
                }
                if b[s].is_ascii_alphabetic() {
                    return match read_tag(src, s) {
                        Some((t, j)) => {
                            self.emit(Tok::End(t.name));
                            j
                        }
                        None => n,
                    };
                }
                if b[s] == b'>' {
                    return s + 1;
                }
                self.bogus_comment(src, s)
            }
            c if c.is_ascii_alphabetic() => match read_tag(src, i + 1) {
                Some((t, j)) => {
                    self.emit(Tok::Start(t));
                    j
                }
                None => n,
            },
            _ => {
                self.emit(Tok::Text("<".into()));
                i + 1
            }
        }
    }

    fn bogus_comment(&mut self, src: &str, s: usize) -> usize {
        let b = src.as_bytes();
        let end = memchr(b'>', &b[s..]).map_or(b.len(), |p| s + p);
        self.emit(Tok::Comment(src[s..end].replace('\0', "\u{fffd}")));
        (end + 1).min(b.len())
    }

    fn doctype(&mut self, src: &str, i: usize) -> usize {
        let b = src.as_bytes();
        let end = memchr(b'>', &b[i..]).map_or(b.len(), |p| i + p);
        let body = src[i..end].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let name_end = body.find(|c: char| c.is_ascii_whitespace()).unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        let mut rest = body[name_end..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let mut force_quirks = name.is_empty() || end == b.len();
        let (mut public, mut system) = (None, None);
        // A quoted identifier; None (and quirks) if malformed.
        fn quoted(s: &str) -> Option<(String, &str)> {
            let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
            let q = s.chars().next().filter(|&c| c == '"' || c == '\'')?;
            let close = s[1..].find(q)?;
            Some((s[1..1 + close].to_owned(), &s[2 + close..]))
        }
        if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("public") {
            match quoted(&rest[6..]) {
                Some((p, r)) => {
                    public = Some(p);
                    rest = r.trim_start_matches(|c: char| c.is_ascii_whitespace());
                    if !rest.is_empty() {
                        match quoted(rest) {
                            Some((s, _)) => system = Some(s),
                            None => force_quirks = true,
                        }
                    }
                }
                None => force_quirks = true,
            }
        } else if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("system") {
            match quoted(&rest[6..]) {
                Some((s, _)) => system = Some(s),
                None => force_quirks = true,
            }
        } else if !rest.is_empty() {
            force_quirks = true;
        }
        self.emit(Tok::Doctype { name, public, system, force_quirks });
        (end + 1).min(b.len())
    }

    fn cdata_allowed(&self) -> bool {
        !self.open.is_empty() && self.doc.elem(self.adjusted_cur()).is_some_and(|e| e.ns != Ns::Html)
    }

    fn emit(&mut self, t: Tok) {
        let t = if std::mem::take(&mut self.skip_lf) {
            match t {
                Tok::Text(s) if s.starts_with('\n') => {
                    if s.len() == 1 {
                        return;
                    }
                    Tok::Text(s[1..].to_owned())
                }
                t => t,
            }
        } else {
            t
        };
        if self.use_html_rules(&t) {
            // U+0000 in text is dropped in HTML content (but kept as U+FFFD
            // in foreign content and raw text, which replace it earlier).
            let t = match t {
                Tok::Text(s) if s.contains('\0') => {
                    let s = s.replace('\0', "");
                    if s.is_empty() {
                        return;
                    }
                    Tok::Text(s)
                }
                t => t,
            };
            self.html(t);
        } else {
            self.foreign(t);
        }
    }
}

/// Read a tag (name + attributes) starting at the first letter of its name.
/// Returns None at end of input inside the tag (the tag is dropped).
fn read_tag(src: &str, mut i: usize) -> Option<(Tag, usize)> {
    let b = src.as_bytes();
    let n = b.len();
    let s = i;
    while i < n && !is_ws(b[i]) && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    let name = intern_lower(&src[s..i].replace('\0', "\u{fffd}"));
    let mut attrs: Vec<(Atom, String)> = Vec::new();
    let mut self_closing = false;
    loop {
        while i < n && is_ws(b[i]) {
            i += 1;
        }
        if i >= n {
            return None;
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
        i += 1; // the first character may be '='
        while i < n && !is_ws(b[i]) && b[i] != b'=' && b[i] != b'>' && b[i] != b'/' {
            i += 1;
        }
        let aname = intern_lower(&src[s..i]);
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
                let e = memchr(q, &b[i..]).map(|p| i + p)?;
                val = decode(&src[i..e], true);
                i = e + 1;
            } else {
                let s = i;
                while i < n && !is_ws(b[i]) && b[i] != b'>' {
                    i += 1;
                }
                val = decode(&src[s..i], true);
            }
            if val.contains('\0') {
                val = val.replace('\0', "\u{fffd}");
            }
        }
        if !attrs.iter().any(|(a, _)| *a == aname) {
            attrs.push((aname, val));
        }
    }
    Some((Tag { name, attrs, self_closing }, i))
}

fn delim(b: &[u8], i: usize) -> bool {
    i < b.len() && (is_ws(b[i]) || b[i] == b'/' || b[i] == b'>')
}

/// The index after an end tag starting at `</` (attributes are skipped).
fn after_end_tag(src: &str, p: usize) -> usize {
    read_tag(src, p + 2).map_or(src.len(), |(_, j)| j)
}

/// End of RAWTEXT / RCDATA content: (content_end, index_after_end_tag).
fn find_end_tag(src: &str, from: usize, name: &[u8]) -> (usize, usize) {
    let b = src.as_bytes();
    let mut i = from;
    while let Some(p) = find(b, i, b"</") {
        let e = p + 2 + name.len();
        if e <= b.len() && b[p + 2..e].eq_ignore_ascii_case(name) && delim(b, e) {
            return (p, after_end_tag(src, p));
        }
        i = p + 2;
    }
    (b.len(), b.len())
}

/// End of script data, honoring the `<!-- <script> </script> -->` escape rules.
fn find_script_end(src: &str, from: usize) -> (usize, usize) {
    let b = src.as_bytes();
    let n = b.len();
    let is_tag = |p: usize, close: bool| {
        let s = p + if close { 2 } else { 1 };
        b[p..].starts_with(if close { b"</" } else { b"<" }) && s + 6 <= n && b[s..s + 6].eq_ignore_ascii_case(b"script") && delim(b, s + 6)
    };
    let after_close = |p: usize| after_end_tag(src, p);
    // 0: normal, 1: escaped (inside <!--), 2: double escaped (<!-- <script>).
    let mut state = 0;
    let mut i = from;
    loop {
        if state == 0 {
            let Some(p) = memchr(b'<', &b[i..]).map(|p| i + p) else { return (n, n) };
            if b[p..].starts_with(b"<!--") {
                state = 1;
                i = p + 2;
            } else if is_tag(p, true) {
                return (p, after_close(p));
            } else {
                i = p + 1;
            }
            continue;
        }
        let lt = memchr(b'<', &b[i..]).map(|p| i + p);
        let dd = find(b, i, b"-->");
        match (lt, dd) {
            (_, Some(d)) if lt.is_none_or(|l| d < l) => {
                state = 0;
                i = d + 3;
            }
            (Some(l), _) => {
                if state == 1 && is_tag(l, true) {
                    return (l, after_close(l));
                }
                if state == 1 && is_tag(l, false) {
                    state = 2;
                    i = l + 7;
                } else if state == 2 && is_tag(l, true) {
                    state = 1;
                    i = l + 8;
                } else {
                    i = l + 1;
                }
            }
            _ => return (n, n),
        }
    }
}

// ----------------------------------------------------------- tree building

fn is_heading(t: Atom) -> bool {
    matches!(t, k::h1 | k::h2 | k::h3 | k::h4 | k::h5 | k::h6)
}

impl Builder {
    fn finish(mut self) -> Document {
        let doc = &mut self.doc;
        if self.context == NONE {
            doc.html = doc.children(0).find(|&c| doc.elem(c).is_some()).unwrap_or(NONE);
            doc.head = self.head;
            if doc.html != NONE {
                doc.body = doc.children(doc.html).find(|&c| matches!(doc.tag(c), Some(k::body | k::frameset))).unwrap_or(NONE);
            }
        }
        let scripts = std::mem::take(&mut doc.scripts);
        doc.scripts = scripts.into_iter().filter(|&s| doc.connected(s)).collect();
        self.doc
    }

    // ------------------------------------------------------------ helpers

    #[inline]
    fn cur(&self) -> NodeId {
        self.open.last().copied().unwrap_or(0)
    }

    fn adjusted_cur(&self) -> NodeId {
        if self.context != NONE && self.open.len() == 1 {
            self.context
        } else {
            self.cur()
        }
    }

    /// Is `n` the HTML element `t`?
    #[inline]
    fn is(&self, n: NodeId, t: Atom) -> bool {
        matches!(self.doc.elem(n), Some(e) if e.ns == Ns::Html && e.tag == t)
    }

    fn is_any(&self, n: NodeId, ts: &[Atom]) -> bool {
        matches!(self.doc.elem(n), Some(e) if e.ns == Ns::Html && ts.contains(&e.tag))
    }

    fn has_template(&self) -> bool {
        self.open.iter().any(|&n| self.is(n, k::template))
    }

    fn is_special(&self, n: NodeId) -> bool {
        let Some(e) = self.doc.elem(n) else { return false };
        use k::*;
        match e.ns {
            Ns::Html => matches!(
                e.tag,
                address | applet | area | article | aside | base | basefont | bgsound | blockquote | body | br | button | caption
                    | center | col | colgroup | dd | details | dir_ | div | dl | dt | embed | fieldset | figcaption | figure
                    | footer | form | frame | frameset | h1 | h2 | h3 | h4 | h5 | h6 | head | header | hgroup | hr | html
                    | iframe | img | input | keygen | li | link | listing | main | marquee | menu | meta | nav | noembed
                    | noframes | noscript | object | ol | p | param | plaintext | pre | script | search | section | select
                    | source | style | summary | table | tbody | td | template | textarea | tfoot | th | thead | title | tr
                    | track | ul | wbr | xmp
            ),
            Ns::MathMl => matches!(e.tag, m_i | m_o | m_n | m_s | m_text | annotation_xml),
            Ns::Svg => matches!(e.tag, foreignobject | desc | title),
        }
    }

    fn mathml_text_ip(&self, n: NodeId) -> bool {
        matches!(self.doc.elem(n), Some(e) if e.ns == Ns::MathMl && matches!(e.tag, k::m_i | k::m_o | k::m_n | k::m_s | k::m_text))
    }

    fn html_ip(&self, n: NodeId) -> bool {
        match self.doc.elem(n) {
            Some(e) if e.ns == Ns::MathMl && e.tag == k::annotation_xml => {
                e.attr(k::encoding).is_some_and(|v| v.eq_ignore_ascii_case("text/html") || v.eq_ignore_ascii_case("application/xhtml+xml"))
            }
            Some(e) if e.ns == Ns::Svg => matches!(e.tag, k::foreignobject | k::desc | k::title),
            _ => false,
        }
    }

    fn use_html_rules(&self, t: &Tok) -> bool {
        if self.open.is_empty() {
            return true;
        }
        let n = self.adjusted_cur();
        let Some(e) = self.doc.elem(n) else { return true };
        if e.ns == Ns::Html {
            return true;
        }
        let text_ip = self.mathml_text_ip(n);
        match t {
            Tok::Start(tag) => {
                (text_ip && tag.name != k::mglyph && tag.name != k::malignmark)
                    || (e.ns == Ns::MathMl && e.tag == k::annotation_xml && tag.name == k::svg)
                    || self.html_ip(n)
            }
            Tok::Text(_) => text_ip || self.html_ip(n),
            Tok::Eof => true,
            _ => false,
        }
    }

    fn scope_boundary(&self, n: NodeId, kind: Scope) -> bool {
        let Some(e) = self.doc.elem(n) else { return true };
        use k::*;
        match kind {
            Scope::Table => e.ns == Ns::Html && matches!(e.tag, html | table | template),
            _ => {
                let bs = match e.ns {
                    Ns::Html => matches!(e.tag, applet | caption | html | table | td | th | marquee | object | template),
                    Ns::MathMl => matches!(e.tag, m_i | m_o | m_n | m_s | m_text | annotation_xml),
                    Ns::Svg => matches!(e.tag, foreignobject | desc | title),
                };
                bs || (kind == Scope::ListItem && e.ns == Ns::Html && matches!(e.tag, ol | ul))
                    || (kind == Scope::Button && e.ns == Ns::Html && e.tag == button)
            }
        }
    }

    fn scope_where(&self, target: impl Fn(&Self, NodeId) -> bool, kind: Scope) -> bool {
        for &n in self.open.iter().rev() {
            if target(self, n) {
                return true;
            }
            if self.scope_boundary(n, kind) {
                return false;
            }
        }
        false
    }

    fn in_scope_k(&self, t: Atom, kind: Scope) -> bool {
        self.scope_where(|s, n| s.is(n, t), kind)
    }

    fn in_scope(&self, t: Atom) -> bool {
        self.in_scope_k(t, Scope::Default)
    }

    fn gen_implied(&mut self, except: Option<Atom>) {
        use k::*;
        while let Some(&c) = self.open.last() {
            match self.doc.elem(c) {
                Some(e) if e.ns == Ns::Html && matches!(e.tag, dd | dt | li | optgroup | option | p | rb | rp | rt | rtc) && Some(e.tag) != except => {
                    self.open.pop();
                }
                _ => break,
            }
        }
    }

    fn gen_implied_thorough(&mut self) {
        use k::*;
        while let Some(&c) = self.open.last() {
            if self.is_any(c, &[caption, colgroup, dd, dt, li, optgroup, option, p, rb, rp, rt, rtc, tbody, td, tfoot, th, thead, tr]) {
                self.open.pop();
            } else {
                break;
            }
        }
    }

    fn pop_until(&mut self, t: Atom) {
        while let Some(c) = self.open.pop() {
            if self.is(c, t) {
                break;
            }
        }
    }

    fn pop_until_any(&mut self, ts: &[Atom]) {
        while let Some(c) = self.open.pop() {
            if self.is_any(c, ts) {
                break;
            }
        }
    }

    fn close_p(&mut self) {
        self.gen_implied(Some(k::p));
        self.pop_until(k::p);
    }

    fn close_p_if_open(&mut self) {
        if self.in_scope_k(k::p, Scope::Button) {
            self.close_p();
        }
    }

    fn clear_to_context(&mut self, ts: &[Atom]) {
        while !self.is_any(self.cur(), ts) && self.open.len() > 1 {
            self.open.pop();
        }
    }

    fn clear_to_table(&mut self) {
        self.clear_to_context(&[k::table, k::template, k::html]);
    }

    fn clear_to_table_body(&mut self) {
        self.clear_to_context(&[k::tbody, k::tfoot, k::thead, k::template, k::html]);
    }

    fn clear_to_row(&mut self) {
        self.clear_to_context(&[k::tr, k::template, k::html]);
    }

    // ---------------------------------------------------------- insertion

    /// The appropriate place for inserting a node: (parent, before).
    fn insert_pos(&mut self, target: Option<NodeId>) -> (NodeId, NodeId) {
        let target = target.unwrap_or_else(|| self.cur());
        let (p, before) = if self.foster && self.is_any(target, &[k::table, k::tbody, k::tfoot, k::thead, k::tr]) {
            let lt = self.open.iter().rposition(|&n| self.is(n, k::template));
            let lb = self.open.iter().rposition(|&n| self.is(n, k::table));
            match (lt, lb) {
                (Some(t), b) if b.is_none_or(|b| t > b) => (self.open[t], NONE),
                (_, None) => (self.open[0], NONE),
                (_, Some(b)) => {
                    let tbl = self.open[b];
                    let par = self.doc.parent(tbl);
                    if par != NONE {
                        (par, tbl)
                    } else {
                        (self.open[b - 1], NONE)
                    }
                }
            }
        } else {
            (target, NONE)
        };
        if self.is(p, k::template) {
            (self.doc.template_content(p), NONE)
        } else {
            (p, before)
        }
    }

    fn place(&mut self, p: NodeId, id: NodeId, before: NodeId) {
        self.doc.detach(id);
        if before == NONE {
            self.doc.append(p, id);
        } else {
            self.doc.insert_before(p, id, before);
        }
    }

    fn create(&mut self, t: &Tag, ns: Ns) -> NodeId {
        let id = self.doc.add(Data::Element(Element::new(t.name, ns, t.attrs.clone())));
        if ns == Ns::Html && t.name == k::template {
            self.doc.template_content(id);
        }
        id
    }

    fn insert_ns(&mut self, t: &Tag, ns: Ns) -> NodeId {
        let id = self.create(t, ns);
        let (p, before) = self.insert_pos(None);
        self.place(p, id, before);
        self.open.push(id);
        id
    }

    fn insert(&mut self, t: &Tag) -> NodeId {
        self.insert_ns(t, Ns::Html)
    }

    fn insert_void(&mut self, t: &Tag) {
        self.insert(t);
        self.open.pop();
    }

    fn insert_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let (p, before) = self.insert_pos(None);
        if p == 0 {
            return;
        }
        if before == NONE {
            self.doc.append_text(p, s);
            return;
        }
        let prev = self.doc.nodes[before as usize].prev;
        if prev != NONE {
            if let Data::Text(t) = &mut self.doc.nodes[prev as usize].data {
                t.push_str(s);
                return;
            }
        }
        let id = self.doc.add(Data::Text(s.to_owned()));
        self.doc.insert_before(p, id, before);
    }

    fn insert_comment(&mut self, c: String) {
        let (p, before) = self.insert_pos(None);
        let id = self.doc.add(Data::Comment(c));
        self.place(p, id, before);
    }

    fn comment_to(&mut self, parent: NodeId, c: String) {
        let id = self.doc.add(Data::Comment(c));
        self.doc.append(parent, id);
    }

    fn merge_attrs(&mut self, node: NodeId, attrs: Vec<(Atom, String)>) {
        for (n, v) in attrs {
            if self.doc.attr(node, n).is_none() {
                self.doc.set_attr(node, n, &v);
            }
        }
    }

    fn generic_text(&mut self, t: &Tag, ts: TState) {
        self.insert(t);
        self.ts = ts;
        self.orig = self.mode;
        self.mode = Mode::Text;
    }

    fn raw_text(&mut self, t: &Tag) {
        self.generic_text(t, TState::RawText(t.name));
    }

    // ------------------------------------------- active formatting elements

    fn push_afe(&mut self, id: NodeId, t: Tag) {
        let mut same = Vec::new();
        for (i, f) in self.afe.iter().enumerate().rev() {
            match f {
                Fmt::Marker => break,
                Fmt::El(_, o) => {
                    if o.name == t.name && o.attrs.len() == t.attrs.len() && o.attrs.iter().all(|a| t.attrs.contains(a)) {
                        same.push(i);
                    }
                }
            }
        }
        if same.len() >= 3 {
            self.afe.remove(*same.last().unwrap());
        }
        self.afe.push(Fmt::El(id, t));
    }

    fn afe_index(&self, id: NodeId) -> Option<usize> {
        self.afe.iter().position(|f| matches!(f, Fmt::El(n, _) if *n == id))
    }

    /// The last formatting element named `name` after the last marker.
    fn afe_find(&self, name: Atom) -> Option<(usize, NodeId)> {
        for (i, f) in self.afe.iter().enumerate().rev() {
            match f {
                Fmt::Marker => return None,
                Fmt::El(n, t) if t.name == name => return Some((i, *n)),
                _ => {}
            }
        }
        None
    }

    fn reconstruct(&mut self) {
        match self.afe.last() {
            None | Some(Fmt::Marker) => return,
            Some(Fmt::El(n, _)) if self.open.contains(n) => return,
            _ => {}
        }
        let mut j = self.afe.len() - 1;
        while j > 0 {
            match &self.afe[j - 1] {
                Fmt::Marker => break,
                Fmt::El(n, _) if self.open.contains(n) => break,
                _ => j -= 1,
            }
        }
        for idx in j..self.afe.len() {
            let t = match &self.afe[idx] {
                Fmt::El(_, t) => t.clone(),
                Fmt::Marker => continue,
            };
            let id = self.insert(&t);
            self.afe[idx] = Fmt::El(id, t);
        }
    }

    fn clear_afe_to_marker(&mut self) {
        while let Some(f) = self.afe.pop() {
            if matches!(f, Fmt::Marker) {
                break;
            }
        }
    }

    /// The adoption agency algorithm. Returns false when the caller should
    /// act as for "any other end tag".
    fn adoption(&mut self, subject: Atom) -> bool {
        let cur = self.cur();
        if self.is(cur, subject) && self.afe_index(cur).is_none() {
            self.open.pop();
            return true;
        }
        for _ in 0..8 {
            let Some((fi, fe)) = self.afe_find(subject) else { return false };
            let Some(fe_open) = self.open.iter().position(|&n| n == fe) else {
                self.afe.remove(fi);
                return true;
            };
            if !self.scope_where(|_, n| n == fe, Scope::Default) {
                return true;
            }
            let Some(fb_pos) = (fe_open + 1..self.open.len()).find(|&i| self.is_special(self.open[i])) else {
                self.open.truncate(fe_open);
                self.afe.remove(fi);
                return true;
            };
            let fb = self.open[fb_pos];
            let common = self.open[fe_open - 1];
            let mut bookmark = fi;
            let mut node_pos = fb_pos;
            let mut last = fb;
            let mut inner = 0;
            loop {
                inner += 1;
                node_pos -= 1;
                let node = self.open[node_pos];
                if node == fe {
                    break;
                }
                let mut ai = self.afe_index(node);
                if inner > 3 {
                    if let Some(a) = ai {
                        self.afe.remove(a);
                        if a < bookmark {
                            bookmark -= 1;
                        }
                        ai = None;
                    }
                }
                let Some(a) = ai else {
                    self.open.remove(node_pos);
                    continue;
                };
                let t = match &self.afe[a] {
                    Fmt::El(_, t) => t.clone(),
                    Fmt::Marker => unreachable!(),
                };
                let new = self.create(&t, Ns::Html);
                self.afe[a] = Fmt::El(new, t);
                self.open[node_pos] = new;
                if last == fb {
                    bookmark = a + 1;
                }
                self.doc.detach(last);
                self.doc.append(new, last);
                last = new;
            }
            self.doc.detach(last);
            let (p, before) = self.insert_pos(Some(common));
            self.place(p, last, before);
            let fi = self.afe_index(fe).unwrap();
            let t = match &self.afe[fi] {
                Fmt::El(_, t) => t.clone(),
                Fmt::Marker => unreachable!(),
            };
            let new = self.create(&t, Ns::Html);
            while self.doc.nodes[fb as usize].first_child != NONE {
                let c = self.doc.nodes[fb as usize].first_child;
                self.doc.detach(c);
                self.doc.append(new, c);
            }
            self.doc.append(fb, new);
            self.afe.remove(fi);
            let bm = if fi < bookmark { bookmark - 1 } else { bookmark };
            self.afe.insert(bm.min(self.afe.len()), Fmt::El(new, t));
            if let Some(p) = self.open.iter().position(|&n| n == fe) {
                self.open.remove(p);
            }
            let fbp = self.open.iter().position(|&n| n == fb).unwrap();
            self.open.insert(fbp + 1, new);
        }
        true
    }

    fn reset_mode(&mut self) {
        use k::*;
        for i in (0..self.open.len()).rev() {
            let last = i == 0;
            let node = if last && self.context != NONE { self.context } else { self.open[i] };
            let Some(e) = self.doc.elem(node) else { continue };
            if e.ns != Ns::Html {
                if last {
                    self.mode = Mode::InBody;
                    return;
                }
                continue;
            }
            self.mode = match e.tag {
                td | th if !last => Mode::InCell,
                tr => Mode::InRow,
                tbody | thead | tfoot => Mode::InTableBody,
                caption => Mode::InCaption,
                colgroup => Mode::InColumnGroup,
                table => Mode::InTable,
                template => *self.tmpl_modes.last().unwrap_or(&Mode::InTemplate),
                head if !last => Mode::InHead,
                body => Mode::InBody,
                frameset => Mode::InFrameset,
                html => {
                    if self.head == NONE {
                        Mode::BeforeHead
                    } else {
                        Mode::AfterHead
                    }
                }
                _ if last => Mode::InBody,
                _ => continue,
            };
            return;
        }
        self.mode = Mode::InBody;
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.open.clear();
    }

    // ---------------------------------------------------------- dispatch

    fn html(&mut self, t: Tok) {
        let mut t = t;
        while let Some(r) = self.step(t) {
            t = r;
        }
    }

    fn step(&mut self, t: Tok) -> Option<Tok> {
        match self.mode {
            Mode::Initial => self.initial(t),
            Mode::BeforeHtml => self.before_html(t),
            Mode::BeforeHead => self.before_head(t),
            Mode::InHead => self.in_head(t),
            Mode::AfterHead => self.after_head(t),
            Mode::InBody => self.in_body(t),
            Mode::Text => self.text_mode(t),
            Mode::InTable => self.in_table(t),
            Mode::InTableText => self.in_table_text(t),
            Mode::InCaption => self.in_caption(t),
            Mode::InColumnGroup => self.in_column_group(t),
            Mode::InTableBody => self.in_table_body(t),
            Mode::InRow => self.in_row(t),
            Mode::InCell => self.in_cell(t),
            Mode::InTemplate => self.in_template(t),
            Mode::AfterBody => self.after_body(t),
            Mode::InFrameset => self.in_frameset(t),
            Mode::AfterFrameset => self.after_frameset(t),
            Mode::AfterAfterBody => self.after_after_body(t),
            Mode::AfterAfterFrameset => self.after_after_frameset(t),
        }
    }

    // ------------------------------------------------------------- modes

    fn initial(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(s) => {
                let (_, rest) = split_ws(&s);
                if rest.is_empty() {
                    return None;
                }
                self.doc.quirks = true;
                self.mode = Mode::BeforeHtml;
                Some(Tok::Text(rest.to_owned()))
            }
            Tok::Comment(c) => {
                self.comment_to(0, c);
                None
            }
            Tok::Doctype { name, public, system, force_quirks } => {
                self.doc.quirks = doctype_quirks(&name, public.as_deref(), system.as_deref(), force_quirks);
                let id = self.doc.add(Data::Doctype(Box::new(DocType { name, public, system })));
                self.doc.append(0, id);
                self.mode = Mode::BeforeHtml;
                None
            }
            t => {
                self.doc.quirks = true;
                self.mode = Mode::BeforeHtml;
                Some(t)
            }
        }
    }

    fn before_html(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Doctype { .. } => None,
            Tok::Comment(c) => {
                self.comment_to(0, c);
                None
            }
            Tok::Text(s) => {
                let (_, rest) = split_ws(&s);
                if rest.is_empty() {
                    return None;
                }
                self.before_html_else();
                Some(Tok::Text(rest.to_owned()))
            }
            Tok::Start(tg) if tg.name == k::html => {
                let id = self.create(&tg, Ns::Html);
                self.doc.append(0, id);
                self.open.push(id);
                self.mode = Mode::BeforeHead;
                None
            }
            Tok::End(n) if !matches!(n, k::head | k::body | k::html | k::br) => None,
            t => {
                self.before_html_else();
                Some(t)
            }
        }
    }

    fn before_html_else(&mut self) {
        let id = self.create(&tag(k::html), Ns::Html);
        self.doc.append(0, id);
        self.open.push(id);
        self.mode = Mode::BeforeHead;
    }

    fn before_head(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(s) => {
                let (_, rest) = split_ws(&s);
                if rest.is_empty() {
                    return None;
                }
                self.head = self.insert(&tag(k::head));
                self.mode = Mode::InHead;
                Some(Tok::Text(rest.to_owned()))
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(tg) if tg.name == k::html => self.in_body(Tok::Start(tg)),
            Tok::Start(tg) if tg.name == k::head => {
                self.head = self.insert(&tg);
                self.mode = Mode::InHead;
                None
            }
            Tok::End(n) if !matches!(n, k::head | k::body | k::html | k::br) => None,
            t => {
                self.head = self.insert(&tag(k::head));
                self.mode = Mode::InHead;
                Some(t)
            }
        }
    }

    fn in_head(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Text(s) => {
                let (ws, rest) = split_ws(&s);
                self.insert_text(ws);
                if rest.is_empty() {
                    return None;
                }
                self.head_else(Tok::Text(rest.to_owned()))
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(tg) => match tg.name {
                html => self.in_body(Tok::Start(tg)),
                base | basefont | bgsound | link | meta => {
                    self.insert_void(&tg);
                    None
                }
                title => {
                    self.generic_text(&tg, TState::RcData(title));
                    None
                }
                noscript | noframes | style => {
                    self.raw_text(&tg);
                    None
                }
                script => {
                    self.generic_text(&tg, TState::Script);
                    None
                }
                template => {
                    self.insert(&tg);
                    self.afe.push(Fmt::Marker);
                    self.frameset_ok = false;
                    self.mode = Mode::InTemplate;
                    self.tmpl_modes.push(Mode::InTemplate);
                    None
                }
                head => None,
                _ => self.head_else(Tok::Start(tg)),
            },
            Tok::End(n) => match n {
                head => {
                    self.open.pop();
                    self.mode = Mode::AfterHead;
                    None
                }
                body | html | br => self.head_else(Tok::End(n)),
                template => {
                    if self.has_template() {
                        self.gen_implied_thorough();
                        self.pop_until(template);
                        self.clear_afe_to_marker();
                        self.tmpl_modes.pop();
                        self.reset_mode();
                    }
                    None
                }
                _ => None,
            },
            Tok::Eof => self.head_else(Tok::Eof),
        }
    }

    fn head_else(&mut self, t: Tok) -> Option<Tok> {
        self.open.pop();
        self.mode = Mode::AfterHead;
        Some(t)
    }

    fn after_head(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Text(s) => {
                let (ws, rest) = split_ws(&s);
                self.insert_text(ws);
                if rest.is_empty() {
                    return None;
                }
                self.after_head_else(Tok::Text(rest.to_owned()))
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(tg) => match tg.name {
                html => self.in_body(Tok::Start(tg)),
                body => {
                    self.insert(&tg);
                    self.frameset_ok = false;
                    self.mode = Mode::InBody;
                    None
                }
                frameset => {
                    self.insert(&tg);
                    self.mode = Mode::InFrameset;
                    None
                }
                base | basefont | bgsound | link | meta | noframes | script | style | template | title => {
                    let h = self.head;
                    self.open.push(h);
                    let r = self.in_head(Tok::Start(tg));
                    if let Some(pos) = self.open.iter().position(|&n| n == h) {
                        self.open.remove(pos);
                    }
                    r
                }
                head => None,
                _ => self.after_head_else(Tok::Start(tg)),
            },
            Tok::End(n) => match n {
                template => self.in_head(Tok::End(n)),
                body | html | br => self.after_head_else(Tok::End(n)),
                _ => None,
            },
            Tok::Eof => self.after_head_else(Tok::Eof),
        }
    }

    fn after_head_else(&mut self, t: Tok) -> Option<Tok> {
        self.insert(&tag(k::body));
        self.mode = Mode::InBody;
        Some(t)
    }

    fn in_body(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(s) => {
                if s.is_empty() {
                    return None;
                }
                self.reconstruct();
                if !all_ws(&s) {
                    self.frameset_ok = false;
                }
                self.insert_text(&s);
                None
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(tg) => self.body_start(tg),
            Tok::End(n) => self.body_end(n),
            Tok::Eof => {
                if !self.tmpl_modes.is_empty() {
                    return self.in_template(Tok::Eof);
                }
                self.stop();
                None
            }
        }
    }

    fn body_start(&mut self, mut tg: Tag) -> Option<Tok> {
        use k::*;
        match tg.name {
            html => {
                if !self.has_template() && !self.open.is_empty() {
                    let root = self.open[0];
                    self.merge_attrs(root, tg.attrs);
                }
            }
            base | basefont | bgsound | link | meta | noframes | script | style | template | title => return self.in_head(Tok::Start(tg)),
            body => {
                if self.open.len() < 2 || !self.is(self.open[1], body) || self.has_template() {
                    return None;
                }
                self.frameset_ok = false;
                let b = self.open[1];
                self.merge_attrs(b, tg.attrs);
            }
            frameset => {
                if self.open.len() < 2 || !self.is(self.open[1], body) || !self.frameset_ok {
                    return None;
                }
                let b = self.open[1];
                self.doc.detach(b);
                self.open.truncate(1);
                self.insert(&tg);
                self.mode = Mode::InFrameset;
            }
            address | article | aside | blockquote | center | details | dialog | dir_ | div | dl | fieldset | figcaption | figure
            | footer | header | hgroup | main | menu | nav | ol | p | search | section | summary | ul => {
                self.close_p_if_open();
                self.insert(&tg);
            }
            h1 | h2 | h3 | h4 | h5 | h6 => {
                self.close_p_if_open();
                if self.doc.elem(self.cur()).is_some_and(|e| e.ns == Ns::Html && is_heading(e.tag)) {
                    self.open.pop();
                }
                self.insert(&tg);
            }
            pre | listing => {
                self.close_p_if_open();
                self.insert(&tg);
                self.skip_lf = true;
                self.frameset_ok = false;
            }
            form => {
                let tmpl = self.has_template();
                if self.form != NONE && !tmpl {
                    return None;
                }
                self.close_p_if_open();
                let el = self.insert(&tg);
                if !tmpl {
                    self.form = el;
                }
            }
            li | dd | dt => {
                self.frameset_ok = false;
                let targets: &[Atom] = if tg.name == li { &[li] } else { &[dd, dt] };
                for idx in (0..self.open.len()).rev() {
                    let node = self.open[idx];
                    if let Some(&t) = targets.iter().find(|&&t| self.is(node, t)) {
                        self.gen_implied(Some(t));
                        self.pop_until(t);
                        break;
                    }
                    if self.is_special(node) && !self.is_any(node, &[address, div, p]) {
                        break;
                    }
                }
                self.close_p_if_open();
                self.insert(&tg);
            }
            plaintext => {
                self.close_p_if_open();
                self.insert(&tg);
                self.ts = TState::PlainText;
            }
            button => {
                if self.in_scope(button) {
                    self.gen_implied(None);
                    self.pop_until(button);
                }
                self.reconstruct();
                self.insert(&tg);
                self.frameset_ok = false;
            }
            a => {
                if let Some((_, old)) = self.afe_find(a) {
                    self.adoption(a);
                    if let Some(ix) = self.afe_index(old) {
                        self.afe.remove(ix);
                    }
                    if let Some(ix) = self.open.iter().position(|&n| n == old) {
                        self.open.remove(ix);
                    }
                }
                self.reconstruct();
                let el = self.insert(&tg);
                self.push_afe(el, tg);
            }
            b_ | big | code | em | font | i_ | s_ | small | strike | strong | tt | u_ => {
                self.reconstruct();
                let el = self.insert(&tg);
                self.push_afe(el, tg);
            }
            nobr => {
                self.reconstruct();
                if self.in_scope(nobr) {
                    if !self.adoption(nobr) {
                        self.any_other_end(nobr);
                    }
                    self.reconstruct();
                }
                let el = self.insert(&tg);
                self.push_afe(el, tg);
            }
            applet | marquee | object => {
                self.reconstruct();
                self.insert(&tg);
                self.afe.push(Fmt::Marker);
                self.frameset_ok = false;
            }
            table => {
                if !self.doc.quirks {
                    self.close_p_if_open();
                }
                self.insert(&tg);
                self.frameset_ok = false;
                self.mode = Mode::InTable;
            }
            area | br | embed | img | keygen | wbr => {
                self.reconstruct();
                self.insert_void(&tg);
                self.frameset_ok = false;
            }
            input => {
                if self.is(self.context, select) {
                    return None;
                }
                if self.in_scope(select) {
                    self.pop_until(select);
                }
                self.reconstruct();
                let is_hidden = tg.attr(type_).is_some_and(|t| t.eq_ignore_ascii_case("hidden"));
                self.insert_void(&tg);
                if !is_hidden {
                    self.frameset_ok = false;
                }
            }
            param | source | track => self.insert_void(&tg),
            hr => {
                self.close_p_if_open();
                if self.in_scope(select) {
                    self.gen_implied(None);
                }
                self.insert_void(&tg);
                self.frameset_ok = false;
            }
            image_ => {
                tg.name = img;
                return Some(Tok::Start(tg));
            }
            textarea => {
                self.insert(&tg);
                self.skip_lf = true;
                self.ts = TState::RcData(textarea);
                self.orig = self.mode;
                self.frameset_ok = false;
                self.mode = Mode::Text;
            }
            xmp => {
                self.close_p_if_open();
                self.reconstruct();
                self.frameset_ok = false;
                self.raw_text(&tg);
            }
            iframe => {
                self.frameset_ok = false;
                self.raw_text(&tg);
            }
            noembed | noscript => self.raw_text(&tg),
            select => {
                if self.is(self.context, select) {
                    return None;
                }
                if self.in_scope(select) {
                    self.pop_until(select);
                    return None;
                }
                self.reconstruct();
                self.insert(&tg);
                self.frameset_ok = false;
            }
            option | optgroup => {
                if self.in_scope(select) {
                    self.gen_implied(if tg.name == option { Some(optgroup) } else { None });
                } else if self.is(self.cur(), option) {
                    self.open.pop();
                }
                self.reconstruct();
                self.insert(&tg);
            }
            rb | rtc => {
                if self.in_scope(ruby) {
                    self.gen_implied(None);
                }
                self.insert(&tg);
            }
            rp | rt => {
                if self.in_scope(ruby) {
                    self.gen_implied(Some(rtc));
                }
                self.insert(&tg);
            }
            math | svg => {
                self.reconstruct();
                let ns = if tg.name == math { Ns::MathMl } else { Ns::Svg };
                self.insert_ns(&tg, ns);
                if tg.self_closing {
                    self.open.pop();
                }
            }
            caption | col | colgroup | frame | head | tbody | td | tfoot | th | thead | tr => {}
            _ => {
                self.reconstruct();
                self.insert(&tg);
            }
        }
        None
    }

    fn body_end(&mut self, n: Atom) -> Option<Tok> {
        use k::*;
        match n {
            template => return self.in_head(Tok::End(n)),
            body => {
                if self.in_scope(body) {
                    self.mode = Mode::AfterBody;
                }
            }
            html => {
                if self.in_scope(body) {
                    self.mode = Mode::AfterBody;
                    return Some(Tok::End(n));
                }
            }
            address | article | aside | blockquote | button | center | details | dialog | dir_ | div | dl | fieldset
            | figcaption | figure | footer | header | hgroup | listing | main | menu | nav | ol | pre | search | section
            | select | summary | ul => {
                if self.in_scope(n) {
                    self.gen_implied(None);
                    self.pop_until(n);
                }
            }
            form => {
                if !self.has_template() {
                    let node = std::mem::replace(&mut self.form, NONE);
                    if node == NONE || !self.scope_where(|_, x| x == node, Scope::Default) {
                        return None;
                    }
                    self.gen_implied(None);
                    if let Some(pos) = self.open.iter().position(|&x| x == node) {
                        self.open.remove(pos);
                    }
                } else if self.in_scope(form) {
                    self.gen_implied(None);
                    self.pop_until(form);
                }
            }
            p => {
                if !self.in_scope_k(p, Scope::Button) {
                    self.insert(&tag(p));
                }
                self.close_p();
            }
            li => {
                if self.in_scope_k(li, Scope::ListItem) {
                    self.gen_implied(Some(li));
                    self.pop_until(li);
                }
            }
            dd | dt => {
                if self.in_scope(n) {
                    self.gen_implied(Some(n));
                    self.pop_until(n);
                }
            }
            h1 | h2 | h3 | h4 | h5 | h6 => {
                if self.scope_where(|s, x| s.doc.elem(x).is_some_and(|e| e.ns == Ns::Html && is_heading(e.tag)), Scope::Default) {
                    self.gen_implied(None);
                    self.pop_until_any(&[h1, h2, h3, h4, h5, h6]);
                }
            }
            a | b_ | big | code | em | font | i_ | nobr | s_ | small | strike | strong | tt | u_ => {
                if !self.adoption(n) {
                    self.any_other_end(n);
                }
            }
            applet | marquee | object => {
                if self.in_scope(n) {
                    self.gen_implied(None);
                    self.pop_until(n);
                    self.clear_afe_to_marker();
                }
            }
            br => return Some(Tok::Start(tag(br))),
            _ => self.any_other_end(n),
        }
        None
    }

    fn any_other_end(&mut self, n: Atom) {
        for idx in (0..self.open.len()).rev() {
            let node = self.open[idx];
            if self.is(node, n) {
                self.gen_implied(Some(n));
                self.open.truncate(idx);
                return;
            }
            if self.is_special(node) {
                return;
            }
        }
    }

    fn text_mode(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(s) => {
                self.insert_text(&s);
                None
            }
            Tok::Eof => {
                self.open.pop();
                self.mode = self.orig;
                Some(Tok::Eof)
            }
            Tok::End(n) => {
                let el = self.open.pop().unwrap_or(NONE);
                if n == k::script && self.is(el, k::script) {
                    self.doc.scripts.push(el);
                }
                self.mode = self.orig;
                None
            }
            _ => None,
        }
    }

    fn in_table(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Text(s) if self.is_any(self.cur(), &[table, tbody, template, tfoot, thead, tr]) => {
                self.table_text.clear();
                self.orig = self.mode;
                self.mode = Mode::InTableText;
                Some(Tok::Text(s))
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(tg) => match tg.name {
                caption => {
                    self.clear_to_table();
                    self.afe.push(Fmt::Marker);
                    self.insert(&tg);
                    self.mode = Mode::InCaption;
                    None
                }
                colgroup => {
                    self.clear_to_table();
                    self.insert(&tg);
                    self.mode = Mode::InColumnGroup;
                    None
                }
                col => {
                    self.clear_to_table();
                    self.insert(&tag(colgroup));
                    self.mode = Mode::InColumnGroup;
                    Some(Tok::Start(tg))
                }
                tbody | tfoot | thead => {
                    self.clear_to_table();
                    self.insert(&tg);
                    self.mode = Mode::InTableBody;
                    None
                }
                td | th | tr => {
                    self.clear_to_table();
                    self.insert(&tag(tbody));
                    self.mode = Mode::InTableBody;
                    Some(Tok::Start(tg))
                }
                table => {
                    if !self.in_scope_k(table, Scope::Table) {
                        return None;
                    }
                    self.pop_until(table);
                    self.reset_mode();
                    Some(Tok::Start(tg))
                }
                style | script | template => self.in_head(Tok::Start(tg)),
                input if tg.attr(type_).is_some_and(|t| t.eq_ignore_ascii_case("hidden")) => {
                    self.insert_void(&tg);
                    None
                }
                form => {
                    if self.has_template() || self.form != NONE {
                        return None;
                    }
                    self.form = self.insert(&tg);
                    self.open.pop();
                    None
                }
                _ => self.table_else(Tok::Start(tg)),
            },
            Tok::End(n) => match n {
                table => {
                    if self.in_scope_k(table, Scope::Table) {
                        self.pop_until(table);
                        self.reset_mode();
                    }
                    None
                }
                body | caption | col | colgroup | html | tbody | td | tfoot | th | thead | tr => None,
                template => self.in_head(Tok::End(n)),
                _ => self.table_else(Tok::End(n)),
            },
            Tok::Eof => self.in_body(Tok::Eof),
            t => self.table_else(t),
        }
    }

    fn table_else(&mut self, t: Tok) -> Option<Tok> {
        self.foster = true;
        let r = self.in_body(t);
        self.foster = false;
        r
    }

    fn in_table_text(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(s) => {
                self.table_text.push_str(&s);
                None
            }
            t => {
                let pending = std::mem::take(&mut self.table_text);
                if !all_ws(&pending) {
                    self.table_else(Tok::Text(pending));
                } else {
                    self.insert_text(&pending);
                }
                self.mode = self.orig;
                Some(t)
            }
        }
    }

    fn close_caption(&mut self) -> bool {
        if !self.in_scope_k(k::caption, Scope::Table) {
            return false;
        }
        self.gen_implied(None);
        self.pop_until(k::caption);
        self.clear_afe_to_marker();
        self.mode = Mode::InTable;
        true
    }

    fn in_caption(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::End(caption) if caption == k::caption => {
                self.close_caption();
                None
            }
            Tok::Start(tg) if matches!(tg.name, caption | col | colgroup | tbody | td | tfoot | th | thead | tr) => {
                if self.close_caption() {
                    Some(Tok::Start(tg))
                } else {
                    None
                }
            }
            Tok::End(table) if table == k::table => {
                if self.close_caption() {
                    Some(Tok::End(table))
                } else {
                    None
                }
            }
            Tok::End(n) if matches!(n, body | col | colgroup | html | tbody | td | tfoot | th | thead | tr) => None,
            t => self.in_body(t),
        }
    }

    fn in_column_group(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Text(s) => {
                let (ws, rest) = split_ws(&s);
                self.insert_text(ws);
                if rest.is_empty() {
                    return None;
                }
                self.colgroup_else(Tok::Text(rest.to_owned()))
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(tg) if tg.name == html => self.in_body(Tok::Start(tg)),
            Tok::Start(tg) if tg.name == col => {
                self.insert_void(&tg);
                None
            }
            Tok::End(n) if n == colgroup => {
                if self.is(self.cur(), colgroup) {
                    self.open.pop();
                    self.mode = Mode::InTable;
                }
                None
            }
            Tok::End(n) if n == col => None,
            Tok::Start(tg) if tg.name == template => self.in_head(Tok::Start(tg)),
            Tok::End(n) if n == template => self.in_head(Tok::End(n)),
            Tok::Eof => self.in_body(Tok::Eof),
            t => self.colgroup_else(t),
        }
    }

    fn colgroup_else(&mut self, t: Tok) -> Option<Tok> {
        if !self.is(self.cur(), k::colgroup) {
            return None;
        }
        self.open.pop();
        self.mode = Mode::InTable;
        Some(t)
    }

    fn in_table_body(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Start(tg) if tg.name == tr => {
                self.clear_to_table_body();
                self.insert(&tg);
                self.mode = Mode::InRow;
                None
            }
            Tok::Start(tg) if matches!(tg.name, th | td) => {
                self.clear_to_table_body();
                self.insert(&tag(tr));
                self.mode = Mode::InRow;
                Some(Tok::Start(tg))
            }
            Tok::End(n) if matches!(n, tbody | tfoot | thead) => {
                if self.in_scope_k(n, Scope::Table) {
                    self.clear_to_table_body();
                    self.open.pop();
                    self.mode = Mode::InTable;
                }
                None
            }
            Tok::Start(ref tg) if matches!(tg.name, caption | col | colgroup | tbody | tfoot | thead) => self.table_body_exit(t),
            Tok::End(table) if table == k::table => self.table_body_exit(t),
            Tok::End(n) if matches!(n, body | caption | col | colgroup | html | td | th | tr) => None,
            t => self.in_table(t),
        }
    }

    fn table_body_exit(&mut self, t: Tok) -> Option<Tok> {
        if !(self.in_scope_k(k::tbody, Scope::Table) || self.in_scope_k(k::thead, Scope::Table) || self.in_scope_k(k::tfoot, Scope::Table)) {
            return None;
        }
        self.clear_to_table_body();
        self.open.pop();
        self.mode = Mode::InTable;
        Some(t)
    }

    fn close_row(&mut self) -> bool {
        if !self.in_scope_k(k::tr, Scope::Table) {
            return false;
        }
        self.clear_to_row();
        self.open.pop();
        self.mode = Mode::InTableBody;
        true
    }

    fn in_row(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Start(tg) if matches!(tg.name, th | td) => {
                self.clear_to_row();
                self.insert(&tg);
                self.mode = Mode::InCell;
                self.afe.push(Fmt::Marker);
                None
            }
            Tok::End(n) if n == tr => {
                self.close_row();
                None
            }
            Tok::Start(ref tg) if matches!(tg.name, caption | col | colgroup | tbody | tfoot | thead | tr) => {
                if self.close_row() {
                    Some(t)
                } else {
                    None
                }
            }
            Tok::End(n) if n == table => {
                if self.close_row() {
                    Some(t)
                } else {
                    None
                }
            }
            Tok::End(n) if matches!(n, tbody | tfoot | thead) => {
                if !self.in_scope_k(n, Scope::Table) {
                    return None;
                }
                if self.close_row() {
                    Some(t)
                } else {
                    None
                }
            }
            Tok::End(n) if matches!(n, body | caption | col | colgroup | html | td | th) => None,
            t => self.in_table(t),
        }
    }

    fn close_cell(&mut self) {
        self.gen_implied(None);
        self.pop_until_any(&[k::td, k::th]);
        self.clear_afe_to_marker();
        self.mode = Mode::InRow;
    }

    fn in_cell(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::End(n) if matches!(n, td | th) => {
                if self.in_scope_k(n, Scope::Table) {
                    self.gen_implied(None);
                    self.pop_until(n);
                    self.clear_afe_to_marker();
                    self.mode = Mode::InRow;
                }
                None
            }
            Tok::Start(ref tg) if matches!(tg.name, caption | col | colgroup | tbody | td | tfoot | th | thead | tr) => {
                if !(self.in_scope_k(td, Scope::Table) || self.in_scope_k(th, Scope::Table)) {
                    return None;
                }
                self.close_cell();
                Some(t)
            }
            Tok::End(n) if matches!(n, body | caption | col | colgroup | html) => None,
            Tok::End(n) if matches!(n, table | tbody | tfoot | thead | tr) => {
                if !self.in_scope_k(n, Scope::Table) {
                    return None;
                }
                self.close_cell();
                Some(t)
            }
            t => self.in_body(t),
        }
    }

    fn in_template(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Text(_) | Tok::Comment(_) | Tok::Doctype { .. } => self.in_body(t),
            Tok::Start(ref tg) => {
                let m = match tg.name {
                    base | basefont | bgsound | link | meta | noframes | script | style | template | title => return self.in_head(t),
                    caption | colgroup | tbody | tfoot | thead => Mode::InTable,
                    col => Mode::InColumnGroup,
                    tr => Mode::InTableBody,
                    td | th => Mode::InRow,
                    _ => Mode::InBody,
                };
                self.tmpl_modes.pop();
                self.tmpl_modes.push(m);
                self.mode = m;
                Some(t)
            }
            Tok::End(n) if n == template => self.in_head(t),
            Tok::End(_) => None,
            Tok::Eof => {
                if !self.has_template() {
                    self.stop();
                    return None;
                }
                self.pop_until(template);
                self.clear_afe_to_marker();
                self.tmpl_modes.pop();
                self.reset_mode();
                Some(Tok::Eof)
            }
        }
    }

    fn after_body(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(ref s) if all_ws(s) => self.in_body(t),
            Tok::Comment(c) => {
                let root = self.open.first().copied().unwrap_or(self.doc.html);
                self.comment_to(root, c);
                None
            }
            Tok::Doctype { .. } => None,
            Tok::Start(ref tg) if tg.name == k::html => self.in_body(t),
            Tok::End(n) if n == k::html => {
                if self.context == NONE {
                    self.mode = Mode::AfterAfterBody;
                }
                None
            }
            Tok::Eof => {
                self.stop();
                None
            }
            t => {
                self.mode = Mode::InBody;
                Some(t)
            }
        }
    }

    fn in_frameset(&mut self, t: Tok) -> Option<Tok> {
        use k::*;
        match t {
            Tok::Text(s) => {
                let ws: String = s.chars().filter(|c| c.is_ascii_whitespace()).collect();
                self.insert_text(&ws);
                None
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Start(ref tg) if tg.name == html => self.in_body(t),
            Tok::Start(tg) if tg.name == frameset => {
                self.insert(&tg);
                None
            }
            Tok::End(n) if n == frameset => {
                if self.open.len() > 1 {
                    self.open.pop();
                    if self.context == NONE && !self.is(self.cur(), frameset) {
                        self.mode = Mode::AfterFrameset;
                    }
                }
                None
            }
            Tok::Start(tg) if tg.name == frame => {
                self.insert_void(&tg);
                None
            }
            Tok::Start(ref tg) if tg.name == noframes => self.in_head(t),
            Tok::Eof => {
                self.stop();
                None
            }
            _ => None,
        }
    }

    fn after_frameset(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Text(s) => {
                let ws: String = s.chars().filter(|c| c.is_ascii_whitespace()).collect();
                self.insert_text(&ws);
                None
            }
            Tok::Comment(c) => {
                self.insert_comment(c);
                None
            }
            Tok::Start(ref tg) if tg.name == k::html => self.in_body(t),
            Tok::End(n) if n == k::html => {
                self.mode = Mode::AfterAfterFrameset;
                None
            }
            Tok::Start(ref tg) if tg.name == k::noframes => self.in_head(t),
            Tok::Eof => {
                self.stop();
                None
            }
            _ => None,
        }
    }

    fn after_after_body(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Comment(c) => {
                self.comment_to(0, c);
                None
            }
            Tok::Doctype { .. } => self.in_body(t),
            Tok::Text(ref s) if all_ws(s) => self.in_body(t),
            Tok::Start(ref tg) if tg.name == k::html => self.in_body(t),
            Tok::Eof => {
                self.stop();
                None
            }
            t => {
                self.mode = Mode::InBody;
                Some(t)
            }
        }
    }

    fn after_after_frameset(&mut self, t: Tok) -> Option<Tok> {
        match t {
            Tok::Comment(c) => {
                self.comment_to(0, c);
                None
            }
            Tok::Doctype { .. } => self.in_body(t),
            Tok::Text(ref s) if all_ws(s) => self.in_body(t),
            Tok::Start(ref tg) if tg.name == k::html => self.in_body(t),
            Tok::Start(ref tg) if tg.name == k::noframes => self.in_head(t),
            Tok::Eof => {
                self.stop();
                None
            }
            _ => None,
        }
    }

    // ---------------------------------------------------- foreign content

    fn foreign(&mut self, t: Tok) {
        use k::*;
        match t {
            Tok::Text(s) => {
                if s.bytes().any(|b| b != 0 && !is_ws(b)) {
                    self.frameset_ok = false;
                }
                let s = if s.contains('\0') { s.replace('\0', "\u{fffd}") } else { s };
                self.insert_text(&s);
            }
            Tok::Comment(c) => self.insert_comment(c),
            Tok::Doctype { .. } => {}
            Tok::Start(tg) => {
                let breakout = matches!(
                    tg.name,
                    b_ | big | blockquote | body | br | center | code | dd | div | dl | dt | em | embed | h1 | h2 | h3 | h4
                        | h5 | h6 | head | hr | i_ | img | li | listing | menu | meta | nobr | ol | p | pre | ruby | s_ | small
                        | span | strong | strike | sub | sup | table | tt | u_ | ul | var_
                ) || (tg.name == font && tg.attrs.iter().any(|(at, _)| matches!(*at, color | face | size)));
                if breakout {
                    while let Some(&c) = self.open.last() {
                        if self.doc.elem(c).is_some_and(|e| e.ns == Ns::Html) || self.mathml_text_ip(c) || self.html_ip(c) || self.open.len() == 1 {
                            break;
                        }
                        self.open.pop();
                    }
                    self.html(Tok::Start(tg));
                    return;
                }
                let ns = self.doc.elem(self.adjusted_cur()).map_or(Ns::Html, |e| e.ns);
                self.insert_ns(&tg, ns);
                if tg.self_closing {
                    self.open.pop();
                }
            }
            Tok::End(n) if n == br || n == p => {
                while let Some(&c) = self.open.last() {
                    if self.doc.elem(c).is_some_and(|e| e.ns == Ns::Html) || self.mathml_text_ip(c) || self.html_ip(c) || self.open.len() == 1 {
                        break;
                    }
                    self.open.pop();
                }
                self.html(Tok::End(n));
            }
            Tok::End(n) => {
                let Some(mut idx) = self.open.len().checked_sub(1) else { return };
                loop {
                    if idx == 0 {
                        return;
                    }
                    let node = self.open[idx];
                    if self.doc.tag(node) == Some(n) {
                        self.open.truncate(idx);
                        return;
                    }
                    idx -= 1;
                    if self.doc.elem(self.open[idx]).is_some_and(|e| e.ns == Ns::Html) {
                        self.html(Tok::End(n));
                        return;
                    }
                }
            }
            Tok::Eof => self.html(Tok::Eof),
        }
    }
}

fn doctype_quirks(name: &str, public: Option<&str>, system: Option<&str>, force: bool) -> bool {
    const QUIRKY: &[&str] = &[
        "+//silmaril//dtd html pro v0r11 19970101//", "-//as//dtd html 3.0 aswedit + extensions//",
        "-//advasoft ltd//dtd html 3.0 aswedit + extensions//", "-//ietf//dtd html 2.0 level 1//",
        "-//ietf//dtd html 2.0 level 2//", "-//ietf//dtd html 2.0 strict level 1//", "-//ietf//dtd html 2.0 strict level 2//",
        "-//ietf//dtd html 2.0 strict//", "-//ietf//dtd html 2.0//", "-//ietf//dtd html 2.1e//", "-//ietf//dtd html 3.0//",
        "-//ietf//dtd html 3.2 final//", "-//ietf//dtd html 3.2//", "-//ietf//dtd html 3//", "-//ietf//dtd html level 0//",
        "-//ietf//dtd html level 1//", "-//ietf//dtd html level 2//", "-//ietf//dtd html level 3//",
        "-//ietf//dtd html strict level 0//", "-//ietf//dtd html strict level 1//", "-//ietf//dtd html strict level 2//",
        "-//ietf//dtd html strict level 3//", "-//ietf//dtd html strict//", "-//ietf//dtd html//",
        "-//metrius//dtd metrius presentational//", "-//microsoft//dtd internet explorer 2.0 html strict//",
        "-//microsoft//dtd internet explorer 2.0 html//", "-//microsoft//dtd internet explorer 2.0 tables//",
        "-//microsoft//dtd internet explorer 3.0 html strict//", "-//microsoft//dtd internet explorer 3.0 html//",
        "-//microsoft//dtd internet explorer 3.0 tables//", "-//netscape comm. corp.//dtd html//",
        "-//netscape comm. corp.//dtd strict html//", "-//o'reilly and associates//dtd html 2.0//",
        "-//o'reilly and associates//dtd html extended 1.0//", "-//o'reilly and associates//dtd html extended relaxed 1.0//",
        "-//sq//dtd html 2.0 hotmetal + extensions//",
        "-//softquad software//dtd hotmetal pro 6.0::19990601::extensions to html 4.0//",
        "-//softquad//dtd hotmetal pro 4.0::19971010::extensions to html 4.0//", "-//spyglass//dtd html 2.0 extended//",
        "-//sun microsystems corp.//dtd hotjava html//", "-//sun microsystems corp.//dtd hotjava strict html//",
        "-//w3c//dtd html 3 1995-03-24//", "-//w3c//dtd html 3.2 draft//", "-//w3c//dtd html 3.2 final//",
        "-//w3c//dtd html 3.2//", "-//w3c//dtd html 3.2s draft//", "-//w3c//dtd html 4.0 frameset//",
        "-//w3c//dtd html 4.0 transitional//", "-//w3c//dtd html experimental 19960712//",
        "-//w3c//dtd html experimental 970421//", "-//w3c//dtd w3 html//", "-//w3o//dtd w3 html 3.0//",
        "-//webtechs//dtd mozilla html 2.0//", "-//webtechs//dtd mozilla html//",
    ];
    if force || name != "html" {
        return true;
    }
    let p = public.unwrap_or("").to_ascii_lowercase();
    let s = system.map(|s| s.to_ascii_lowercase());
    if matches!(p.as_str(), "-//w3o//dtd w3 html strict 3.0//en//" | "-/w3c/dtd html 4.0 transitional/en" | "html")
        || s.as_deref() == Some("http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd")
        || QUIRKY.iter().any(|q| p.starts_with(q))
    {
        return true;
    }
    s.is_none() && (p.starts_with("-//w3c//dtd html 4.01 frameset//") || p.starts_with("-//w3c//dtd html 4.01 transitional//"))
}

// ----------------------------------------------------- character references

/// Decode character references in text content.
pub fn decode_entities(s: &str) -> String {
    decode(s, false)
}

fn numeric_ref(v: u32) -> char {
    let v = match v {
        0x80 => 0x20ac, 0x82 => 0x201a, 0x83 => 0x0192, 0x84 => 0x201e, 0x85 => 0x2026, 0x86 => 0x2020,
        0x87 => 0x2021, 0x88 => 0x02c6, 0x89 => 0x2030, 0x8a => 0x0160, 0x8b => 0x2039, 0x8c => 0x0152,
        0x8e => 0x017d, 0x91 => 0x2018, 0x92 => 0x2019, 0x93 => 0x201c, 0x94 => 0x201d, 0x95 => 0x2022,
        0x96 => 0x2013, 0x97 => 0x2014, 0x98 => 0x02dc, 0x99 => 0x2122, 0x9a => 0x0161, 0x9b => 0x203a,
        0x9c => 0x0153, 0x9e => 0x017e, 0x9f => 0x0178, 0 => 0xfffd,
        v => v,
    };
    char::from_u32(v).unwrap_or('\u{fffd}')
}

fn lookup_entity(name: &str) -> Option<&'static str> {
    crate::entities::ENTITIES.binary_search_by(|(k, _)| (*k).cmp(name)).ok().map(|i| crate::entities::ENTITIES[i].1)
}

fn decode(s: &str, in_attr: bool) -> String {
    let b = s.as_bytes();
    if memchr(b'&', b).is_none() {
        return s.to_owned();
    }
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
            let mut v: u32 = 0;
            while j < b.len() && (if hex { b[j].is_ascii_hexdigit() } else { b[j].is_ascii_digit() }) {
                let d = (b[j] as char).to_digit(16).unwrap();
                v = v.saturating_mul(if hex { 16 } else { 10 }).saturating_add(d);
                j += 1;
            }
            if j == ds {
                continue;
            }
            if j < b.len() && b[j] == b';' {
                j += 1;
            }
            out.push_str(&s[last..amp]);
            out.push(if v > 0x10ffff { '\u{fffd}' } else { numeric_ref(v) });
            last = j;
            i = j;
        } else {
            while j < b.len() && b[j].is_ascii_alphanumeric() && j - amp <= 32 {
                j += 1;
            }
            if j == amp + 1 {
                continue;
            }
            let run = &s[amp + 1..j];
            let semi = j < b.len() && b[j] == b';';
            // Longest match: the full name with ';', then legacy prefixes without it.
            let mut found = None;
            if semi {
                if let Some(v) = lookup_entity(&s[amp + 1..j + 1]) {
                    found = Some((v, j + 1, true));
                }
            }
            if found.is_none() {
                for len in (2..=run.len()).rev() {
                    if let Some(v) = lookup_entity(&run[..len]) {
                        found = Some((v, amp + 1 + len, false));
                        break;
                    }
                }
            }
            if let Some((v, end, with_semi)) = found {
                if !with_semi && in_attr && end < b.len() && (b[end] == b'=' || b[end].is_ascii_alphanumeric()) {
                    continue;
                }
                out.push_str(&s[last..amp]);
                out.push_str(v);
                last = end;
                i = end;
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
        assert_eq!(decode_entities("AT&T &unknown; &notit; &NotEqualTilde; &#x80;"), "AT&T &unknown; ¬it; \u{2242}\u{338} €");
        assert_eq!(decode("?a=1&copy=2", true), "?a=1&copy=2");
    }

    #[test]
    fn tree() {
        let d = parse("<p>one<p>two<ul><li>a<li>b</ul><table><tr><td>x<td>y</table>");
        let body: Vec<_> = d.children(d.body).map(|c| d.tag(c)).collect();
        assert_eq!(body, vec![Some(k::p), Some(k::p), Some(k::ul), Some(k::table)]);
        let ul = d.children(d.body).nth(2).unwrap();
        assert_eq!(d.children(ul).count(), 2);
        let table = d.children(d.body).nth(3).unwrap();
        let tbody = d.children(table).next().unwrap();
        assert_eq!(d.tag(tbody), Some(k::tbody));
        let tr = d.children(tbody).next().unwrap();
        assert_eq!(d.children(tr).count(), 2);
    }

    fn html_of(src: &str) -> String {
        let d = parse(src);
        let mut s = String::new();
        d.outer_html(0, &mut s);
        s
    }

    #[test]
    fn spec_recovery() {
        // Adoption agency.
        assert_eq!(html_of("<!DOCTYPE html><b>1<p>2</b>3</p>"), "<!DOCTYPE html><html><head></head><body><b>1</b><p><b>2</b>3</p></body></html>");
        // Foster parenting.
        assert_eq!(html_of("<!DOCTYPE html><table>x<tr><td>y</table>"), "<!DOCTYPE html><html><head></head><body>x<table><tbody><tr><td>y</td></tr></tbody></table></body></html>");
        // Head content, comments, template contents.
        assert_eq!(
            html_of("<!DOCTYPE html><title>T</title><style>p{}</style><!--c--><template><td>x</td></template>"),
            "<!DOCTYPE html><html><head><title>T</title><style>p{}</style><!--c--><template><td>x</td></template></head><body></body></html>"
        );
        // Script escapes.
        assert_eq!(html_of("<script><!-- <script></script> --></script>x"), "<html><head><script><!-- <script></script> --></script></head><body>x</body></html>");
        // SVG foreign content and breakout.
        assert_eq!(html_of("<svg><g><p>x</svg>"), "<html><head></head><body><svg><g></g></svg><p>x</p></body></html>");
    }

    fn svg_attr_case(a: &str) -> &str {
        const NAMES: &[&str] = &[
            "attributeName", "attributeType", "baseFrequency", "baseProfile", "calcMode", "clipPathUnits",
            "diffuseConstant", "edgeMode", "filterUnits", "glyphRef", "gradientTransform", "gradientUnits",
            "kernelMatrix", "kernelUnitLength", "keyPoints", "keySplines", "keyTimes", "lengthAdjust",
            "limitingConeAngle", "markerHeight", "markerUnits", "markerWidth", "maskContentUnits", "maskUnits",
            "numOctaves", "pathLength", "patternContentUnits", "patternTransform", "patternUnits", "pointsAtX",
            "pointsAtY", "pointsAtZ", "preserveAlpha", "preserveAspectRatio", "primitiveUnits", "refX", "refY",
            "repeatCount", "repeatDur", "requiredExtensions", "requiredFeatures", "specularConstant",
            "specularExponent", "spreadMethod", "startOffset", "stdDeviation", "stitchTiles", "surfaceScale",
            "systemLanguage", "tableValues", "targetX", "targetY", "textLength", "viewBox", "viewTarget",
            "xChannelSelector", "yChannelSelector", "zoomAndPan",
        ];
        NAMES.iter().find(|n| n.eq_ignore_ascii_case(a)).copied().unwrap_or(a)
    }

    /// Serialize in the html5lib tree-construction test format.
    fn dump(d: &Document, n: NodeId, depth: usize, out: &mut String) {
        for c in d.children(n) {
            let pad = format!("| {}", "  ".repeat(depth));
            match &d.nodes[c as usize].data {
                Data::Element(e) => {
                    let name = atom::name(e.tag);
                    let shown = match e.ns {
                        Ns::Html => name.to_owned(),
                        Ns::Svg => format!("svg {}", crate::dom::svg_tag_case(name)),
                        Ns::MathMl => format!("math {name}"),
                    };
                    out.push_str(&format!("{pad}<{shown}>\n"));
                    let mut attrs: Vec<(String, &str)> = e
                        .attrs
                        .iter()
                        .map(|(a, v)| {
                            let a = atom::name(*a);
                            let a = match (e.ns, a) {
                                (Ns::Html, a) => a.to_owned(),
                                (_, "xlink:actuate" | "xlink:arcrole" | "xlink:href" | "xlink:role" | "xlink:show" | "xlink:title" | "xlink:type" | "xml:lang" | "xml:space" | "xmlns:xlink") => a.replacen(':', " ", 1),
                                (Ns::Svg, a) => svg_attr_case(a).to_owned(),
                                (Ns::MathMl, "definitionurl") => "definitionURL".to_owned(),
                                (_, a) => a.to_owned(),
                            };
                            (a, v.as_str())
                        })
                        .collect();
                    attrs.sort();
                    for (a, v) in attrs {
                        out.push_str(&format!("{pad}  {a}=\"{v}\"\n"));
                    }
                    if let Some(&content) = d.templates.get(&c) {
                        out.push_str(&format!("{pad}  content\n"));
                        dump(d, content, depth + 2, out);
                    } else {
                        dump(d, c, depth + 1, out);
                    }
                }
                Data::Text(t) => out.push_str(&format!("{pad}\"{t}\"\n")),
                Data::Comment(t) => out.push_str(&format!("{pad}<!-- {t} -->\n")),
                Data::Doctype(t) if t.public.is_none() && t.system.is_none() => out.push_str(&format!("{pad}<!DOCTYPE {}>\n", t.name)),
                Data::Doctype(t) => out.push_str(&format!("{pad}<!DOCTYPE {} \"{}\" \"{}\">\n", t.name, t.public.as_deref().unwrap_or(""), t.system.as_deref().unwrap_or(""))),
                _ => {}
            }
        }
    }

    /// Run the html5lib tree-construction suite:
    /// `BOLT_HTML5LIB=<html5lib-tests/tree-construction> cargo test --release html5lib -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn html5lib() {
        let Ok(dir) = std::env::var("BOLT_HTML5LIB") else { return };
        let (mut pass, mut fail, mut shown) = (0, 0, 0);
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "dat")).collect();
        files.sort();
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap();
            let (mut fp, mut ff) = (0, 0);
            for case in src.split("\n#data\n").map(|c| c.strip_prefix("#data\n").unwrap_or(c)) {
                let mut sections: Vec<(&str, String)> = Vec::new();
                let mut cur = ("data", String::new());
                for line in case.split('\n') {
                    if line.starts_with('#') && matches!(line, "#errors" | "#new-errors" | "#document-fragment" | "#script-off" | "#script-on" | "#document") {
                        sections.push(std::mem::replace(&mut cur, (&line[1..], String::new())));
                    } else {
                        cur.1.push_str(line);
                        cur.1.push('\n');
                    }
                }
                sections.push(cur);
                let get = |k: &str| sections.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
                if get("script-off").is_some() {
                    continue;
                }
                let Some(data) = get("data") else { continue };
                let data = data.strip_suffix('\n').unwrap_or(&data).to_owned();
                let Some(expected) = get("document") else { continue };
                let expected = expected.trim_end().to_owned();
                let mut out = String::new();
                if let Some(ctx) = get("document-fragment") {
                    let ctx = ctx.trim();
                    let (ns, name) = match ctx.split_once(' ') {
                        Some(("svg", n)) => (Ns::Svg, n),
                        Some(("math", n)) => (Ns::MathMl, n),
                        _ => (Ns::Html, ctx),
                    };
                    let d = parse_fragment(&data, intern_lower(name), ns);
                    dump(&d, d.html, 0, &mut out);
                } else {
                    let d = parse(&data);
                    dump(&d, 0, 0, &mut out);
                }
                if out.trim_end() == expected {
                    fp += 1;
                } else {
                    ff += 1;
                    if shown < 25 && !expected.contains("<!DOCTYPE html \"") && !expected.contains("<!DOCTYPE html PUBLIC") {
                        shown += 1;
                        println!("--- {} \n{data}\n-- expected\n{expected}\n-- got\n{}", f.display(), out.trim_end());
                    }
                }
            }
            println!("{:>4} / {:<4} {}", fp, fp + ff, f.file_name().unwrap().to_string_lossy());
            pass += fp;
            fail += ff;
        }
        println!("html5lib tree construction: {pass} / {} passed", pass + fail);
    }

    #[test]
    fn fragments() {
        let d = parse_fragment("<td>a</td><td>b", k::tr, Ns::Html);
        let kids: Vec<_> = d.children(d.html).map(|c| d.tag(c)).collect();
        assert_eq!(kids, vec![Some(k::td), Some(k::td)]);
        let d = parse_fragment("x<b>y", k::div, Ns::Html);
        assert_eq!(d.children(d.html).count(), 2);
    }
}
