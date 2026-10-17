//! String interning. Tag names, attribute names, ids and classes become 32-bit
//! `Atom`s so selector matching compares integers instead of strings.
//! Well-known names get fixed ids at compile time (`k::div`, `k::href`, ...).

use crate::fx::FxHashMap;
use std::cell::RefCell;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Atom(pub u32);

macro_rules! static_atoms {
    ($($name:ident = $s:literal),* $(,)?) => {
        #[allow(non_camel_case_types, dead_code, clippy::upper_case_acronyms)]
        #[repr(u32)]
        enum Idx { $($name),* }
        #[allow(non_upper_case_globals, dead_code)]
        pub mod k {
            use super::Atom;
            $(pub const $name: Atom = Atom(super::Idx::$name as u32);)*
        }
        const STATIC: &[&str] = &[$($s),*];
    };
}

static_atoms! {
    html = "html", head = "head", body = "body", title = "title", style = "style",
    script = "script", link = "link", meta = "meta", base = "base", noscript = "noscript",
    template = "template", div = "div", span = "span", p = "p", a = "a", img = "img",
    br = "br", hr = "hr", ul = "ul", ol = "ol", li = "li", dl = "dl", dt = "dt", dd = "dd",
    h1 = "h1", h2 = "h2", h3 = "h3", h4 = "h4", h5 = "h5", h6 = "h6",
    table = "table", thead = "thead", tbody = "tbody", tfoot = "tfoot", tr = "tr",
    td = "td", th = "th", caption = "caption", col = "col", colgroup = "colgroup",
    pre = "pre", code = "code", blockquote = "blockquote", form = "form", input = "input",
    button = "button", textarea = "textarea", select = "select", option = "option",
    optgroup = "optgroup", label = "label", center = "center", font = "font",
    header = "header", footer = "footer", nav = "nav", main = "main", section = "section",
    article = "article", aside = "aside", figure = "figure", address = "address",
    details = "details", summary = "summary", menu = "menu", fieldset = "fieldset",
    area = "area", embed = "embed", param = "param", source = "source", track = "track",
    wbr = "wbr", iframe = "iframe", svg = "svg", video = "video", audio = "audio",
    canvas = "canvas", object = "object", picture = "picture", math = "math",
    xmp = "xmp", plaintext = "plaintext", listing = "listing",
    applet = "applet", b_ = "b", basefont = "basefont", bgsound = "bgsound", big = "big",
    dialog = "dialog", dir_ = "dir", em = "em", figcaption = "figcaption", frame = "frame",
    frameset = "frameset", hgroup = "hgroup", i_ = "i", image_ = "image", keygen = "keygen",
    marquee = "marquee", nobr = "nobr", noembed = "noembed", noframes = "noframes",
    rb = "rb", rp = "rp", rt = "rt", rtc = "rtc", ruby = "ruby", s_ = "s", search = "search",
    small = "small", strike = "strike", strong = "strong", tt = "tt", u_ = "u",
    sub = "sub", sup = "sup", var_ = "var", m_i = "mi", m_o = "mo", m_n = "mn", m_s = "ms",
    m_text = "mtext", mglyph = "mglyph", malignmark = "malignmark",
    annotation_xml = "annotation-xml", foreignobject = "foreignobject", desc = "desc",
    encoding = "encoding", charset = "charset", slot = "slot",
    // attributes
    id = "id", class = "class", href = "href", src = "src", alt = "alt", width = "width",
    height = "height", rel = "rel", type_ = "type", value = "value",
    placeholder = "placeholder", colspan = "colspan", align = "align", valign = "valign",
    bgcolor = "bgcolor", color = "color", cellpadding = "cellpadding",
    cellspacing = "cellspacing", border = "border", start = "start", size = "size",
    face = "face", hidden = "hidden", media = "media", selected = "selected",
    checked = "checked", nowrap = "nowrap", text = "text", name = "name",
    disabled = "disabled", readonly = "readonly", action = "action", method = "method",
}

struct Interner {
    map: FxHashMap<&'static str, Atom>,
    names: Vec<&'static str>,
}

/// One table for the whole process: documents are parsed on loader threads
/// and used on the UI thread, so atom ids must mean the same everywhere.
static INTERNER: std::sync::OnceLock<std::sync::RwLock<Interner>> = std::sync::OnceLock::new();

fn interner() -> &'static std::sync::RwLock<Interner> {
    INTERNER.get_or_init(|| std::sync::RwLock::new(Interner::new()))
}

thread_local! {
    /// This thread's copy of the (append-only) name list, for lock-free
    /// `name()`; refreshed from the global table on a miss.
    static NAMES: RefCell<Vec<&'static str>> = RefCell::new(STATIC.to_vec());
}

impl Interner {
    fn new() -> Self {
        let mut map = FxHashMap::default();
        map.reserve(STATIC.len() * 4);
        for (i, s) in STATIC.iter().enumerate() {
            map.insert(*s, Atom(i as u32));
        }
        Interner { map, names: STATIC.to_vec() }
    }
}

/// Intern `s` exactly (case-sensitive; used for ids and classes).
pub fn intern(s: &str) -> Atom {
    if let Some(&a) = interner().read().unwrap_or_else(|e| e.into_inner()).map.get(s) {
        return a;
    }
    let mut i = interner().write().unwrap_or_else(|e| e.into_inner());
    if let Some(&a) = i.map.get(s) {
        return a;
    }
    // Atoms live for the whole process; names are leaked deliberately.
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    let a = Atom(i.names.len() as u32);
    i.names.push(leaked);
    i.map.insert(leaked, a);
    a
}

/// Intern an ASCII-case-insensitive name (tags, attributes).
pub fn intern_lower(s: &str) -> Atom {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        intern(&s.to_ascii_lowercase())
    } else {
        intern(s)
    }
}

/// Look up an atom without creating it.
pub fn lookup(s: &str) -> Option<Atom> {
    interner().read().unwrap_or_else(|e| e.into_inner()).map.get(s).copied()
}

pub fn name(a: Atom) -> &'static str {
    NAMES.with(|n| {
        let mut n = n.borrow_mut();
        if let Some(s) = n.get(a.0 as usize) {
            return *s;
        }
        let g = interner().read().unwrap_or_else(|e| e.into_inner());
        let have = n.len();
        n.extend_from_slice(&g.names[have..]);
        n.get(a.0 as usize).copied().unwrap_or("")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atoms_are_shared_across_threads() {
        // A document parsed on a loader thread is used on the UI thread.
        let a = std::thread::spawn(|| intern("x-made-on-another-thread")).join().unwrap();
        assert_eq!(name(a), "x-made-on-another-thread");
        assert_eq!(lookup("x-made-on-another-thread"), Some(a));
        assert_eq!(intern("x-made-on-another-thread"), a);
    }
}
