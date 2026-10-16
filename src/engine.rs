//! The rendering pipeline: load → parse → cascade → layout → display list,
//! plus image loading (fetched in parallel after the first render).

use crate::atom::k;
use crate::css::{self, Media};
use crate::dom::{Data, Document, NodeId, NONE};
use crate::forms::{self, Activation};
pub use crate::forms::Nav;
use crate::fx::FxHashMap;
use crate::image::{self, Image};
use crate::layout::{self, LayoutTree};
use crate::paint::{self, DisplayList};
use crate::style::{self, Display, Style};
use crate::{html, js, net};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

pub const HOME: &str = include_str!("../pages/home.html");
pub const FEATURES: &str = include_str!("../pages/features.html");
pub const JS_DEMO: &str = include_str!("../pages/js.html");

#[derive(Default, Clone, Copy, Debug)]
pub struct Timings {
    pub fetch: f64,
    pub parse: f64,
    pub css: f64,
    pub style: f64,
    pub layout: f64,
    pub display: f64,
    pub paint: f64,
    pub nodes: usize,
    pub rules: usize,
    pub boxes: usize,
}

impl Timings {
    pub fn summary(&self) -> String {
        format!(
            "parse {:.2}ms · css {:.2}ms · style {:.2}ms · layout {:.2}ms · display {:.2}ms · paint {:.2}ms  ({} nodes, {} rules, {} boxes)",
            self.parse, self.css, self.style, self.layout, self.display, self.paint, self.nodes, self.rules, self.boxes
        )
    }
}

#[inline]
pub fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Decoded images and which `<img>` uses which URL.
#[derive(Default)]
pub struct Resources {
    /// By absolute URL; None = failed or unsupported format.
    pub images: FxHashMap<String, Option<Arc<Image>>>,
    pub img_src: FxHashMap<NodeId, String>,
}

impl Resources {
    pub fn img(&self, node: NodeId) -> Option<&Arc<Image>> {
        self.images.get(self.img_src.get(&node)?)?.as_ref()
    }
    pub fn url(&self, url: &str) -> Option<&Arc<Image>> {
        self.images.get(url)?.as_ref()
    }
}

pub struct Loaded {
    pub url: String,
    pub html: String,
    /// (css, base url)
    pub sheets: Vec<(String, String)>,
    pub doc: Document,
    /// External script sources by absolute URL.
    pub scripts: FxHashMap<String, String>,
    pub fetch_ms: f64,
    pub parse_ms: f64,
}

fn base_of(doc: &Document, url: &str) -> String {
    doc.base.as_ref().map(|b| net::resolve(url, b)).unwrap_or_else(|| url.to_owned())
}

/// Fetch a document and its stylesheets (in parallel).
pub fn load(url: &str) -> Result<Loaded, String> {
    load_req(url, None)
}

/// Like `load`, POSTing `post` as the request body if given.
pub fn load_req(url: &str, post: Option<&str>) -> Result<Loaded, String> {
    let t = Instant::now();
    let (url, html) = match url {
        "about:home" | "about:" => (url.to_owned(), HOME.to_owned()),
        "about:blank" => (url.to_owned(), String::new()),
        "about:features" => (url.to_owned(), FEATURES.to_owned()),
        "about:js" => (url.to_owned(), JS_DEMO.to_owned()),
        "about:stress" => (url.to_owned(), stress_page()),
        _ => {
            let (u, bytes) = net::request(url, post)?;
            (u, net::decode(&bytes))
        }
    };
    let fetch_html = ms(t);
    let tp = Instant::now();
    let doc = html::parse(&html);
    let parse_ms = ms(tp);

    let t = Instant::now();
    let base = base_of(&doc, &url);
    let urls: Vec<String> = doc.stylesheet_links.iter().map(|h| net::resolve(&base, h)).collect();
    let script_urls: Vec<String> = doc
        .scripts
        .iter()
        .filter(|&&s| is_js(&doc, s))
        .filter_map(|&s| doc.attr(s, k::src))
        .map(|h| net::resolve(&base, h.trim()))
        .collect();
    // Stylesheets and scripts download together in one parallel batch.
    let all: Vec<String> = urls.iter().chain(&script_urls).cloned().collect();
    let mut bodies = net::fetch_many(&all).into_iter().map(|b| b.map(|b| net::decode(&b)).unwrap_or_default());
    let mut sheets: Vec<(String, String)> = urls.iter().map(|u| (bodies.next().unwrap_or_default(), u.clone())).collect();
    let scripts = script_urls.iter().map(|u| (u.clone(), bodies.next().unwrap_or_default())).collect();
    sheets.extend(doc.styles.iter().map(|s| (s.clone(), base.clone())));
    Ok(Loaded { url, html, sheets, doc, scripts, fetch_ms: fetch_html + ms(t), parse_ms })
}

/// Is this <script> classic JavaScript we should run?
fn is_js(doc: &Document, s: NodeId) -> bool {
    match doc.attr(s, k::type_).map(|t| t.trim().to_ascii_lowercase()) {
        None => true,
        Some(t) => t.is_empty() || t.contains("javascript") || t.contains("ecmascript"),
    }
}

/// Wall-clock budget for running a page's scripts at load.
const SCRIPT_BUDGET: f64 = 3000.0;
/// How long the first paint may wait for scripts' network requests.
const NET_WAIT: f64 = 1500.0;

/// Run a page's scripts (in document order), load events and zero-delay timers.
fn run_scripts(doc: Document, sources: &FxHashMap<String, String>, url: &str, base: &str, vw: f32, vh: f32) -> (Document, Option<js::Interp>, f64) {
    let has_handlers = doc.nodes.iter().any(|n| match &n.data {
        Data::Element(e) => e.attrs.iter().any(|(a, _)| crate::atom::name(*a).starts_with("on")),
        _ => false,
    });
    if doc.scripts.is_empty() && !has_handlers {
        return (doc, None, 0.0);
    }
    let t0 = Instant::now();
    let scripts = doc.scripts.clone();
    let mut it = js::Interp::new(doc, url, vw, vh);
    it.deadline = Some(t0 + std::time::Duration::from_millis(SCRIPT_BUDGET as u64));
    for s in scripts {
        if !is_js(&it.doc, s) || ms(t0) > SCRIPT_BUDGET {
            continue;
        }
        let code = match it.doc.attr(s, k::src) {
            Some(src) => sources.get(&net::resolve(base, src.trim())).cloned().unwrap_or_default(),
            None => {
                let mut c = String::new();
                it.doc.text_content(s, &mut c);
                c
            }
        };
        run_one(&mut it, &code);
        run_pending(&mut it, &t0);
    }
    js::dombind::dispatch(&mut it, 0, "DOMContentLoaded", |_, _| {});
    js::dombind::dispatch(&mut it, js::WINDOW, "load", |_, _| {});
    it.run_microtasks();
    // Timers that fire "immediately" so the first paint includes their work.
    for _ in 0..5 {
        if it.run_timers(1.0) == 0 || ms(t0) > SCRIPT_BUDGET {
            break;
        }
        run_pending(&mut it, &t0);
    }
    // Let data requested at startup (fetch / XHR) arrive before the first
    // paint, within a bounded wait; later responses arrive via `tick`.
    let net_start = Instant::now();
    while it.has_pending_requests() && ms(t0) < SCRIPT_BUDGET && ms(net_start) < NET_WAIT {
        let left = (SCRIPT_BUDGET - ms(t0)).min(NET_WAIT - ms(net_start)).max(1.0);
        if it.poll_requests(Some(std::time::Duration::from_millis(left.min(50.0) as u64))) {
            run_pending(&mut it, &t0);
            for _ in 0..3 {
                if it.run_timers(1.0) == 0 {
                    break;
                }
            }
        }
    }
    let elapsed = ms(t0);
    it.dirty = false;
    let doc = std::mem::take(&mut it.doc);
    (doc, Some(it), elapsed)
}

fn run_one(it: &mut js::Interp, code: &str) {
    if code.trim().is_empty() {
        return;
    }
    it.steps = 0;
    if let Err(e) = it.run_script(code) {
        it.log(&e);
    }
    it.run_microtasks();
}

/// Fetch and run scripts that were inserted by other scripts.
fn run_pending(it: &mut js::Interp, t0: &Instant) {
    for _ in 0..3 {
        let urls = std::mem::take(&mut it.pending_scripts);
        if urls.is_empty() || ms(*t0) > SCRIPT_BUDGET {
            return;
        }
        let codes: Vec<String> = net::fetch_many(&urls).into_iter().map(|b| b.map(|b| net::decode(&b)).unwrap_or_default()).collect();
        for c in codes {
            run_one(it, &c);
        }
    }
}

/// The URL an `<img>` shows (handles common lazy-loading attributes and srcset).
fn img_url(doc: &Document, n: NodeId, base: &str) -> Option<String> {
    let attr = |name: &str| crate::atom::lookup(name).and_then(|a| doc.attr(n, a)).map(str::trim).filter(|s| !s.is_empty());
    let src = doc.attr(n, k::src).map(str::trim).filter(|s| !s.is_empty());
    let placeholder = src.is_none_or(|s| s.starts_with("data:image/gif") || s.starts_with("data:image/svg") || s.contains("blank.gif"));
    let pick = if placeholder { attr("data-src").or(attr("data-lazy-src")).or(attr("data-original")).or(src) } else { src };
    let pick = pick.map(str::to_owned).or_else(|| {
        let set = attr("srcset").or(attr("data-srcset"))?;
        set.split(',').next()?.split_whitespace().next().map(str::to_owned)
    })?;
    Some(if pick.starts_with("data:") { pick } else { net::resolve(base, &pick) })
}

/// Download (one parallel batch) and register web fonts.
pub fn fetch_fonts(faces: &[css::FontFace]) {
    let urls: Vec<String> = faces.iter().map(|f| f.url.clone()).collect();
    let bodies = net::fetch_many(&urls);
    std::thread::scope(|s| {
        for (f, bytes) in faces.iter().zip(bodies) {
            s.spawn(move || crate::fonts::fonts().add_web(&f.family, &f.url, f.weight, f.italic, bytes));
        }
    });
}

/// Fetch images (one parallel batch) and decode them on all cores; `done`
/// is called as each finishes decoding.
pub fn fetch_images(urls: &[String], done: &(dyn Fn(String, Option<Arc<Image>>) + Sync)) {
    let bodies = net::fetch_many_accept(urls, net::ACCEPT_IMG);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(urls.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(u) = urls.get(i) else { break };
                let img = bodies[i].as_deref().and_then(image::decode).map(Arc::new);
                done(u.clone(), img);
            });
        }
    });
}

pub struct Page {
    pub url: String,
    pub base: String,
    pub doc: Document,
    pub sheets: Vec<(String, String)>,
    pub styles: Vec<Style>,
    pub tree: LayoutTree,
    pub dl: DisplayList,
    pub res: Resources,
    pub js: Option<js::Interp>,
    pub t: Timings,
    /// Time spent running scripts at load.
    pub script_ms: f64,
    /// Number of relayouts (used to tell whether a tick changed the page).
    pub layouts: u64,
    /// Caret position (in chars) within the focused text field.
    pub caret: usize,
    /// A navigation the page asked for (form submission or script).
    pub nav: Option<Nav>,
    /// The focused field's value when it gained focus (for `change` events).
    focus_value: String,
    vw: f32,
    vh: f32,
}

/// An editing key sent to the focused text field.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    Insert(String),
    Backspace,
    /// Ctrl+Backspace: delete the previous word.
    BackspaceWord,
    Delete,
    Left,
    Right,
    Home,
    End,
    Enter,
    Tab,
    Escape,
}

/// What a click on the page did.
#[derive(Debug, PartialEq)]
pub enum Click {
    /// Nothing special: follow a link under the pointer, if any.
    Default,
    /// The page handled it (a script, a form control).
    Handled,
    /// The page wants to navigate (form submission, script).
    Navigate(Nav),
}

#[allow(clippy::too_many_arguments)]
pub fn render(doc: &Document, sheets: &[(String, String)], base: &str, res: &mut Resources, vw: f32, vh: f32, t: &mut Timings) -> (Vec<Style>, LayoutTree, DisplayList) {
    let t0 = Instant::now();
    let cas = style::build_cascade(sheets, &Media { vw, vh }, doc.quirks);
    t.css = ms(t0);
    let t0 = Instant::now();
    css::set_base(base);
    let styles = style::compute(doc, &cas, vw, vh);
    t.style = ms(t0);
    inline_svgs(doc, &styles, res);
    let t0 = Instant::now();
    let tree = layout::layout(doc, &styles, vw, vh, res);
    t.layout = ms(t0);
    let t0 = Instant::now();
    let dl = paint::build(&tree, doc, &styles, res);
    t.display = ms(t0);
    t.nodes = doc.nodes.len();
    t.rules = cas.rule_count();
    t.boxes = tree.boxes.len();
    (styles, tree, dl)
}

/// Rasterize inline `<svg>` elements as images (cached by markup and
/// `currentColor`).
fn inline_svgs(doc: &Document, styles: &[Style], res: &mut Resources) {
    use std::hash::{Hash, Hasher};
    for (i, n) in doc.nodes.iter().enumerate() {
        let Data::Element(e) = &n.data else { continue };
        let id = i as NodeId;
        if e.tag != k::svg || styles.get(i).is_none_or(|s| s.display == Display::None) {
            continue;
        }
        // Only outermost <svg> elements; nested ones are part of their parent.
        let mut p = doc.parent(id);
        let mut nested = false;
        while p != NONE {
            if doc.tag(p) == Some(k::svg) {
                nested = true;
                break;
            }
            p = doc.parent(p);
        }
        if nested {
            continue;
        }
        let mut markup = String::new();
        doc.outer_html(id, &mut markup);
        let color = styles[i].color;
        let fluid = e.attr(k::width).is_none() && e.attr(k::height).is_none();
        let mut h = crate::fx::FxHasher::default();
        markup.hash(&mut h);
        let key = format!("svg-inline:{:x}:{color:x}:{fluid}", h.finish());
        if !res.images.contains_key(&key) {
            let img = crate::svg::render(&markup, color, None).map(|r| Arc::new(Image { vector: true, fluid, ..image::from_pixels(r.w, r.h, r.px, r.iw, r.ih) }));
            res.images.insert(key.clone(), img);
        }
        res.img_src.insert(id, key);
    }
}

impl Page {
    pub fn new(l: Loaded, vw: f32, vh: f32) -> Page {
        let mut t = Timings { fetch: l.fetch_ms, parse: l.parse_ms, ..Default::default() };
        let base = base_of(&l.doc, &l.url);
        let (doc, js, script_ms) = run_scripts(l.doc, &l.scripts, &l.url, &base, vw, vh);
        let mut res = Resources::default();
        Self::collect_img_src(&doc, &base, &mut res);
        let (styles, tree, dl) = render(&doc, &l.sheets, &base, &mut res, vw, vh, &mut t);
        let mut page = Page { url: l.url, base, doc, sheets: l.sheets, styles, tree, dl, res, js, t, script_ms, layouts: 0, caret: 0, nav: None, focus_value: String::new(), vw, vh };
        page.sync_js_geometry();
        page.script_requests();
        page
    }

    fn collect_img_src(doc: &Document, base: &str, res: &mut Resources) {
        res.img_src.clear();
        for (i, n) in doc.nodes.iter().enumerate() {
            if let Data::Element(e) = &n.data {
                if e.tag == k::img {
                    if let Some(u) = img_url(doc, i as NodeId, base) {
                        res.img_src.insert(i as NodeId, u);
                    }
                }
            }
        }
    }

    /// Give scripts the latest element geometry.
    fn sync_js_geometry(&mut self) {
        if let Some(it) = &mut self.js {
            it.rects = self.tree.node_rects();
            it.vw = self.vw;
            it.vh = self.vh;
        }
    }

    /// Run `f` with the document moved into the script engine; relayout if
    /// the DOM changed.
    fn with_js<T>(&mut self, f: impl FnOnce(&mut js::Interp) -> T) -> Option<T> {
        let it = self.js.as_mut()?;
        it.doc = std::mem::take(&mut self.doc);
        // Event handlers and timers get a short leash.
        it.deadline = Some(Instant::now() + std::time::Duration::from_millis(500));
        let focus_before = it.doc.focus;
        let r = f(it);
        let t0 = Instant::now();
        run_pending(it, &t0);
        self.doc = std::mem::take(&mut it.doc);
        if self.doc.focus != focus_before {
            // A script moved focus (el.focus()).
            self.caret = forms::value(&self.doc, self.doc.focus).chars().count();
            self.focus_value = forms::value(&self.doc, self.doc.focus);
        }
        if std::mem::take(&mut it.dirty) {
            Self::collect_img_src(&self.doc, &self.base, &mut self.res);
            self.relayout(self.vw, self.vh);
        }
        self.script_requests();
        Some(r)
    }

    /// The DOM was changed outside scripts: run `f` (event dispatch), then
    /// restyle and relayout once.
    fn changed_then_js(&mut self, f: impl FnOnce(&mut js::Interp)) {
        if let Some(it) = &mut self.js {
            it.dirty = true;
        }
        if self.with_js(f).is_none() {
            self.relayout(self.vw, self.vh);
        }
    }

    /// Collect navigations scripts asked for: `location.assign()`, setting
    /// `location.href`, `form.submit()`.
    fn script_requests(&mut self) {
        let Some(it) = &mut self.js else { return };
        if let Some(n) = it.nav.take() {
            self.nav = Some(n);
        }
        if let Some(form) = it.submit_form.take() {
            self.nav = forms::submission(&self.doc, form, NONE, &self.base).or(self.nav.take());
        }
        let global = js::Value::Obj(it.global.clone());
        if let Ok(js::Value::Obj(loc)) = it.get(&global, "location") {
            let lv = js::Value::Obj(loc.clone());
            if let Ok(href) = it.get(&lv, "href").and_then(|h| it.to_str(&h)) {
                if *href != *it.url {
                    let url = net::resolve(&it.url, &href);
                    js::Interp::set_prop(&loc, "href", js::Value::str(&it.url));
                    self.nav = Some(Nav { url, post: None });
                }
            }
        }
    }

    /// Take a pending navigation request.
    pub fn take_nav(&mut self) -> Option<Nav> {
        self.nav.take()
    }

    /// Is the node (or the control it belongs to) a text field?
    pub fn is_text_field_at(&self, n: NodeId) -> bool {
        forms::control_for(&self.doc, n).is_some_and(|c| forms::is_text_field(&self.doc, c))
    }

    /// The focused element, if any.
    pub fn focused(&self) -> Option<NodeId> {
        (self.doc.focus != NONE).then_some(self.doc.focus)
    }

    /// Move focus (NONE = blur), firing blur/change/focus events.
    pub fn set_focus(&mut self, n: NodeId) {
        let old = self.doc.focus;
        if old == n {
            return;
        }
        let changed = old != NONE && forms::value(&self.doc, old) != self.focus_value;
        self.doc.focus = n;
        self.caret = forms::value(&self.doc, n).chars().count();
        self.focus_value = forms::value(&self.doc, n);
        self.changed_then_js(|it| {
            if old != NONE {
                if changed {
                    js::dombind::dispatch(it, old, "change", |_, _| {});
                }
                js::dombind::dispatch_nobubble(it, old, "blur");
                js::dombind::dispatch(it, old, "focusout", |_, _| {});
            }
            if n != NONE {
                js::dombind::dispatch_nobubble(it, n, "focus");
                js::dombind::dispatch(it, n, "focusin", |_, _| {});
            }
        });
    }

    /// Dispatch a click on the element at a page point and run its default
    /// action (focus a field, toggle a checkbox, submit a form).
    pub fn click(&mut self, x: f32, y: f32) -> Click {
        let Some(node) = self.tree.node_at(x, y) else {
            self.set_focus(NONE);
            return Click::Default;
        };
        // Checkboxes and radios toggle before handlers run (and revert if
        // the click is cancelled), so handlers see the new state.
        let control = forms::control_for(&self.doc, node);
        let checkable = control.filter(|&c| forms::is_checkable(&self.doc, c));
        let snapshot = checkable.map(|_| forms::checked_snapshot(&self.doc));
        let early = checkable.map(|c| forms::activate_control(&mut self.doc, c));
        if early.is_some() {
            if let Some(it) = &mut self.js {
                it.dirty = true;
            }
        }
        let prevented = self
            .with_js(|it| {
                js::dombind::dispatch(it, node, "click", |it, e| {
                    let _ = it.put(e, "clientX", js::Value::Num(x as f64));
                    let _ = it.put(e, "clientY", js::Value::Num(y as f64));
                    let _ = it.put(e, "button", js::Value::Num(0.0));
                })
            })
            .unwrap_or(false);
        if let Some(n) = self.take_nav() {
            return Click::Navigate(n);
        }
        if prevented {
            if let Some(s) = snapshot {
                forms::restore_checked(&mut self.doc, &s);
                self.relayout(self.vw, self.vh);
            }
            return Click::Handled;
        }
        let act = match early {
            Some(a) => a,
            None => control.map_or(Activation::None, |c| forms::activate_control(&mut self.doc, c)),
        };
        match act {
            Activation::Focus(n) => {
                self.set_focus(n);
                self.caret = self.caret_at(x, y);
                Click::Handled
            }
            Activation::Toggled(n) => {
                self.set_focus(NONE);
                self.changed_then_js(|it| {
                    js::dombind::dispatch(it, n, "input", |_, _| {});
                    js::dombind::dispatch(it, n, "change", |_, _| {});
                });
                Click::Handled
            }
            Activation::Submit(form, submitter) => {
                self.set_focus(NONE);
                self.submit(form, submitter).map_or(Click::Handled, Click::Navigate)
            }
            Activation::None => {
                self.set_focus(NONE);
                Click::Default
            }
        }
    }

    /// Fire `submit` at a form and, unless cancelled, build its request.
    fn submit(&mut self, form: NodeId, submitter: NodeId) -> Option<Nav> {
        let prevented = self.with_js(|it| js::dombind::dispatch(it, form, "submit", |_, _| {})).unwrap_or(false);
        if let Some(n) = self.take_nav() {
            return Some(n);
        }
        if prevented {
            return None;
        }
        forms::submission(&self.doc, form, submitter, &self.base)
    }

    /// Apply an editing key to the focused field. Returns a navigation if
    /// it submitted a form.
    pub fn key(&mut self, e: Edit) -> Option<Nav> {
        let n = self.focused()?;
        let name = match &e {
            Edit::Insert(s) => s.clone(),
            Edit::Backspace | Edit::BackspaceWord => "Backspace".into(),
            Edit::Delete => "Delete".into(),
            Edit::Left => "ArrowLeft".into(),
            Edit::Right => "ArrowRight".into(),
            Edit::Home => "Home".into(),
            Edit::End => "End".into(),
            Edit::Enter => "Enter".into(),
            Edit::Tab => "Tab".into(),
            Edit::Escape => "Escape".into(),
        };
        let code = match name.as_str() {
            "Backspace" => 8,
            "Tab" => 9,
            "Enter" => 13,
            "Escape" => 27,
            "ArrowLeft" => 37,
            "ArrowRight" => 39,
            "Delete" => 46,
            s => s.chars().next().map_or(0, |c| c.to_ascii_uppercase() as u32),
        };
        let prevented = self
            .with_js(|it| {
                js::dombind::dispatch(it, n, "keydown", |it, ev| {
                    let _ = it.put(ev, "key", js::Value::str(&name));
                    let _ = it.put(ev, "keyCode", js::Value::Num(code as f64));
                    let _ = it.put(ev, "which", js::Value::Num(code as f64));
                })
            })
            .unwrap_or(false);
        if let Some(nav) = self.take_nav() {
            return Some(nav);
        }
        if prevented || self.focused() != Some(n) {
            return None;
        }
        let textarea = self.doc.tag(n) == Some(k::textarea);
        let mut chars: Vec<char> = forms::value(&self.doc, n).chars().collect();
        let mut caret = self.caret.min(chars.len());
        let mut edited = false;
        match e {
            Edit::Insert(s) => {
                let s: String = if textarea { s } else { s.chars().filter(|&c| c != '\n' && c != '\r').collect() };
                for c in s.chars() {
                    chars.insert(caret, c);
                    caret += 1;
                }
                edited = !s.is_empty();
            }
            Edit::Backspace if caret > 0 => {
                caret -= 1;
                chars.remove(caret);
                edited = true;
            }
            Edit::BackspaceWord if caret > 0 => {
                let mut i = caret;
                while i > 0 && chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                while i > 0 && !chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                chars.drain(i..caret);
                caret = i;
                edited = true;
            }
            Edit::Delete if caret < chars.len() => {
                chars.remove(caret);
                edited = true;
            }
            Edit::Left => caret = caret.saturating_sub(1),
            Edit::Right => caret = (caret + 1).min(chars.len()),
            Edit::Home => caret = 0,
            Edit::End => caret = chars.len(),
            Edit::Enter if textarea => {
                chars.insert(caret, '\n');
                caret += 1;
                edited = true;
            }
            Edit::Enter => {
                // Implicit submission: clicks the form's default button.
                let form = forms::form_of(&self.doc, n);
                if form == NONE {
                    return None;
                }
                let button = forms::default_button(&self.doc, form);
                if button != NONE {
                    let prevented = self.with_js(|it| js::dombind::dispatch(it, button, "click", |_, _| {})).unwrap_or(false);
                    if let Some(nav) = self.take_nav() {
                        return Some(nav);
                    }
                    if prevented {
                        return None;
                    }
                }
                return self.submit(form, button);
            }
            Edit::Tab => {
                let next = self.doc.preorder().into_iter().filter(|&c| forms::is_text_field(&self.doc, c)).skip_while(|&c| c != n).nth(1);
                self.set_focus(next.unwrap_or(NONE));
                return None;
            }
            Edit::Escape => {
                self.set_focus(NONE);
                return None;
            }
            _ => {}
        }
        self.caret = caret;
        if edited {
            let v: String = chars.into_iter().collect();
            forms::set_value(&mut self.doc, n, &v);
            self.changed_then_js(|it| {
                js::dombind::dispatch(it, n, "input", |_, _| {});
            });
        }
        self.take_nav()
    }

    /// Text shown by the focused control, as typed (password fields show
    /// one bullet per character).
    fn shown_value(&self, n: NodeId) -> String {
        let v = forms::value(&self.doc, n);
        if self.doc.elem(n).and_then(|e| e.attr(k::type_)).is_some_and(|t| t.eq_ignore_ascii_case("password")) {
            "\u{2022}".repeat(v.chars().count())
        } else {
            v
        }
    }

    /// Caret rectangle (page coordinates) in the focused text field.
    pub fn caret_rect(&self) -> Option<[f32; 4]> {
        let n = self.focused()?;
        if !forms::is_text_field(&self.doc, n) {
            return None;
        }
        let st = self.styles.get(n as usize)?;
        let size = st.font_size;
        let face = crate::fonts::fonts().pick_style(st);
        let (content, frags) = self.tree.text_box(n)?;
        let shown = self.shown_value(n);
        let caret_byte = shown.char_indices().nth(self.caret).map_or(shown.len(), |(i, _)| i);
        let (x, base) = if shown.is_empty() || frags.is_empty() {
            frags.first().map_or((content[0], content[1] + size), |f| (f.0, f.1))
        } else {
            // Find the fragment holding the caret by locating each fragment's
            // text in the value (fragments omit line breaks).
            let mut pos = 0;
            let mut hit = None;
            for &(fx, fy, t) in &frags {
                let Some(off) = shown[pos..].find(t).map(|o| o + pos) else { continue };
                if caret_byte >= off && caret_byte <= off + t.len() {
                    let w = crate::fonts::fonts().measure(face, size, &shown[off..caret_byte]);
                    hit = Some((fx + w, fy));
                }
                pos = off + t.len();
                if caret_byte < pos {
                    break;
                }
            }
            hit.or_else(|| {
                // After a trailing newline: start of the next line.
                let &(_, fy, _) = frags.last()?;
                if shown[..caret_byte].ends_with('\n') {
                    Some((content[0], fy + size * 1.2))
                } else {
                    let (lx, ly, lt) = *frags.last()?;
                    Some((lx + crate::fonts::fonts().measure(face, size, lt), ly))
                }
            })?
        };
        let x = x.min(content[0] + content[2]);
        Some([x.round(), base - size * 0.95, 1.0, size * 1.2])
    }

    /// Caret index nearest to a click at (x, y) in the focused field.
    fn caret_at(&self, x: f32, y: f32) -> usize {
        let Some(n) = self.focused() else { return 0 };
        let shown = self.shown_value(n);
        let Some(st) = self.styles.get(n as usize) else { return 0 };
        let (size, face) = (st.font_size, crate::fonts::fonts().pick_style(st));
        let Some((_, frags)) = self.tree.text_box(n) else { return 0 };
        if shown.is_empty() {
            return 0;
        }
        // The line under the click, then the nearest character boundary.
        let line = frags.iter().filter(|f| (f.1 - size..f.1 + size * 0.3).contains(&y)).collect::<Vec<_>>();
        let pool: Vec<_> = if line.is_empty() { frags.iter().collect() } else { line };
        let mut pos = 0;
        let mut best = (f32::MAX, shown.chars().count());
        for &(fx, fy, t) in &frags {
            let Some(off) = shown[pos..].find(t).map(|o| o + pos) else { continue };
            pos = off + t.len();
            if !pool.iter().any(|p| p.0 == fx && p.1 == fy) {
                continue;
            }
            let mut cx = fx;
            let base = shown[..off].chars().count();
            for (i, c) in t.chars().enumerate() {
                let w = crate::fonts::fonts().measure(face, size, c.encode_utf8(&mut [0; 4]));
                if (cx - x).abs() < best.0 {
                    best = ((cx - x).abs(), base + i);
                }
                cx += w;
            }
            if (cx - x).abs() < best.0 {
                best = ((cx - x).abs(), base + t.chars().count());
            }
        }
        best.1
    }

    /// Run due timers (`now` = ms since load). Returns true if the page changed.
    pub fn tick(&mut self, now: f64) -> bool {
        let Some(it) = &self.js else { return false };
        let due = it.next_timer_at().is_some_and(|t| t <= now);
        if !due && it.requests.is_empty() {
            return false;
        }
        let before = self.layouts;
        self.with_js(|it| {
            it.poll_requests(None);
            if due {
                it.run_timers(now);
            }
        });
        self.layouts != before
    }

    /// Re-run style + layout for a new viewport size (media queries may change).
    pub fn relayout(&mut self, vw: f32, vh: f32) {
        self.vw = vw;
        self.vh = vh;
        let (styles, tree, dl) = render(&self.doc, &self.sheets, &self.base, &mut self.res, vw, vh, &mut self.t);
        self.styles = styles;
        self.tree = tree;
        self.dl = dl;
        self.layouts += 1;
        self.sync_js_geometry();
    }

    /// Images used by rendered elements that haven't been fetched yet.
    pub fn wanted_images(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (i, st) in self.styles.iter().enumerate() {
            if st.display == Display::None || self.doc.elem(i as NodeId).is_none() {
                continue;
            }
            if let Some(u) = self.res.img_src.get(&(i as NodeId)) {
                if !self.res.images.contains_key(u) && seen.insert(u.clone()) {
                    out.push(u.clone());
                }
            }
            if let Some(bi) = st.bg_image {
                if let css::BgImage::Url(u) = css::bg_image(bi) {
                    if !self.res.images.contains_key(&*u) && seen.insert(u.to_string()) {
                        out.push(u.to_string());
                    }
                }
            }
        }
        out
    }

    /// @font-face fonts used by the page's styles that aren't loaded yet.
    pub fn wanted_fonts(&self) -> Vec<css::FontFace> {
        let faces = css::font_faces();
        if faces.is_empty() {
            return Vec::new();
        }
        let mut used: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut last: Option<*const [Box<str>]> = None;
        for st in &self.styles {
            if let Some(f) = &st.families {
                if last != Some(Rc::as_ptr(f)) {
                    last = Some(Rc::as_ptr(f));
                    used.extend(f.iter().map(|n| &**n));
                }
            }
        }
        let fonts = crate::fonts::fonts();
        faces.into_iter().filter(|f| used.contains(&*f.family) && !fonts.has_web(&f.url, &f.family)).collect()
    }

    /// Fetch and register web fonts now; returns true if any were added.
    pub fn load_fonts_blocking(&mut self, vw: f32, vh: f32) -> bool {
        let faces = self.wanted_fonts();
        if faces.is_empty() {
            return false;
        }
        fetch_fonts(&faces);
        self.relayout(vw, vh);
        true
    }

    /// Fetch every wanted image now (used for screenshots and benchmarks).
    pub fn load_images_blocking(&mut self, vw: f32, vh: f32) {
        let urls = self.wanted_images();
        if urls.is_empty() {
            return;
        }
        let got = std::sync::Mutex::new(Vec::new());
        fetch_images(&urls, &|u, img| got.lock().unwrap().push((u, img)));
        for (u, img) in got.into_inner().unwrap() {
            self.res.images.insert(u, img);
        }
        self.relayout(vw, vh);
    }

    pub fn title(&self) -> &str {
        &self.doc.title
    }

    pub fn link_href(&self, link: u32) -> Option<String> {
        let href = self.tree.links.get(link as usize)?;
        Some(net::resolve(&self.base, href))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_hit_testable() {
        let p = Page::new(load("about:home").unwrap(), 1000.0, 800.0);
        let area = p
            .dl
            .links
            .iter()
            .find(|a| p.link_href(a.link).as_deref() == Some("about:stress"))
            .expect("about:stress link area");
        let hit = p.dl.link_at(area.x + area.w / 2.0, area.y + area.h / 2.0, 0.0);
        assert_eq!(hit.and_then(|l| p.link_href(l)).as_deref(), Some("about:stress"));
        assert!(p.tree.height > 500.0);
    }

    fn text_of(p: &Page, id: &str) -> String {
        let a = crate::atom::lookup(id).unwrap();
        let n = (0..p.doc.nodes.len() as u32).find(|&n| p.doc.elem(n).is_some_and(|e| e.id == Some(a))).unwrap();
        let mut s = String::new();
        p.doc.text_content(n, &mut s);
        s
    }

    fn center_of(p: &Page, id: &str) -> (f32, f32) {
        let a = crate::atom::lookup(id).unwrap();
        let n = (0..p.doc.nodes.len() as u32).find(|&n| p.doc.elem(n).is_some_and(|e| e.id == Some(a))).unwrap();
        let r = p.tree.node_rects()[&n];
        (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0)
    }

    fn page_from(html: &str) -> Page {
        let doc = html::parse(html);
        let l = Loaded { url: "https://example.com/start".into(), html: html.into(), sheets: Vec::new(), doc, scripts: FxHashMap::default(), fetch_ms: 0.0, parse_ms: 0.0 };
        Page::new(l, 800.0, 600.0)
    }

    #[test]
    fn typing_into_forms() {
        let mut p = page_from(
            r#"<!doctype html><form action="/search"><input id=q name=q placeholder="Search">
               <label><input type=checkbox id=c name=c> safe</label><button id=go>Go</button></form>
               <p id=echo></p><p id=keys></p>
               <script>
                 const q = document.getElementById('q');
                 q.addEventListener('input', () => document.getElementById('echo').textContent = q.value.toUpperCase());
                 q.addEventListener('keydown', e => { document.getElementById('keys').textContent += e.key; if (e.key === '!') e.preventDefault(); });
               </script>"#,
        );
        let (x, y) = center_of(&p, "q");
        assert_eq!(p.click(x, y), Click::Handled);
        let q = p.focused().expect("focused");
        let caret0 = p.caret_rect().expect("caret")[0];
        for e in [Edit::Insert("rust".into()), Edit::Insert("!".into()), Edit::Insert(" lang".into()), Edit::Left, Edit::Left, Edit::Left, Edit::Left, Edit::Backspace] {
            p.key(e);
        }
        // '!' was cancelled by the keydown handler; the backspace removed the space.
        assert_eq!(forms::value(&p.doc, q), "rustlang");
        assert_eq!(text_of(&p, "echo"), "RUSTLANG");
        assert!(text_of(&p, "keys").starts_with("rust!"));
        assert_eq!(p.caret, 4);
        let caret1 = p.caret_rect().unwrap()[0];
        assert!(caret1 > caret0 + 10.0, "{caret0} {caret1}");
        // Clicking the label text toggles the checkbox.
        let label = p.doc.parent(p.doc.preorder().into_iter().find(|&n| p.doc.elem(n).is_some_and(|e| e.id == crate::atom::lookup("c"))).unwrap());
        let r = p.tree.node_rects()[&label];
        assert_eq!(p.click(r[0] + r[2] - 5.0, r[1] + r[3] / 2.0), Click::Handled);
        assert!(p.focused().is_none());
        let (x, y) = center_of(&p, "q");
        p.click(x, y);
        assert_eq!(p.key(Edit::Enter), Some(Nav { url: "https://example.com/search?q=rustlang&c=on".into(), post: None }));
    }

    /// Load a page; on Google's EU cookie-consent page, press "accept all"
    /// (a POST form) like a user would.
    fn load_past_consent(nav: Nav) -> Page {
        let p = Page::new(load_req(&nav.url, nav.post.as_deref()).unwrap(), 1100.0, 800.0);
        if !p.url.contains("consent.google") {
            return p;
        }
        let mut p = p;
        let accept = p
            .doc
            .preorder()
            .into_iter()
            .filter(|&n| p.doc.tag(n) == Some(k::input) && p.doc.attr(n, k::type_) == Some("submit"))
            .find(|&n| p.doc.attr(n, k::value).is_some_and(|v| v.to_lowercase().starts_with("accept")))
            .expect("accept button");
        let r = p.tree.node_rects()[&accept];
        let Click::Navigate(nav) = p.click(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0) else { panic!("consent form did not submit") };
        eprintln!("consent: {nav:?}");
        assert!(nav.post.is_some());
        Page::new(load_req(&nav.url, nav.post.as_deref()).unwrap(), 1100.0, 800.0)
    }

    /// Search Google like a user: click the box, type, press Enter.
    /// Needs the network: `cargo test --release -- --ignored live_google`.
    #[test]
    #[ignore]
    fn live_google_search() {
        let mut p = load_past_consent(Nav { url: "https://www.google.com/".into(), post: None });
        let q = p.doc.preorder().into_iter().find(|&n| p.doc.attr(n, k::name) == Some("q")).expect("search box");
        let r = p.tree.node_rects()[&q];
        assert_eq!(p.click(r[0] + 10.0, r[1] + r[3] / 2.0), Click::Handled);
        assert_eq!(p.focused(), Some(q));
        p.key(Edit::Insert("rust programming language".into()));
        let nav = p.key(Edit::Enter).expect("submits");
        eprintln!("submitted: {nav:?}");
        assert!(nav.url.contains("q=rust+programming+language"));
        // Google gates result pages behind a JavaScript challenge that bolt
        // can't pass yet, so only the request is checked here.
        let results = load_past_consent(nav);
        eprintln!("results: {} ({})", results.title(), results.url);
    }

    /// DuckDuckGo's HTML version works without JavaScript: type a query
    /// into its form and check the results. Needs the network.
    #[test]
    #[ignore]
    fn live_duckduckgo_search() {
        let mut p = Page::new(load("https://html.duckduckgo.com/html/").unwrap(), 1100.0, 800.0);
        let q = p.doc.preorder().into_iter().find(|&n| p.doc.attr(n, k::name) == Some("q")).expect("search box");
        let r = p.tree.node_rects()[&q];
        assert_eq!(p.click(r[0] + 10.0, r[1] + r[3] / 2.0), Click::Handled);
        p.key(Edit::Insert("rust programming language".into()));
        let nav = p.key(Edit::Enter).expect("submits");
        eprintln!("submitted: {nav:?}");
        let results = Page::new(load_req(&nav.url, nav.post.as_deref()).unwrap(), 1100.0, 800.0);
        let mut text = String::new();
        results.doc.text_content(results.doc.body, &mut text);
        assert!(text.contains("rust-lang.org"), "{}", &text[..text.len().min(800)]);
        eprintln!("results: {} links", results.tree.links.len());
    }
    #[test]
    fn scripts_can_cancel_or_submit_forms() {
        let mut p = page_from(
            r#"<form id=f action="/go"><input name=a value=1><button id=b>Send</button></form>
               <script>document.getElementById('f').addEventListener('submit', e => e.preventDefault());</script>"#,
        );
        let (x, y) = center_of(&p, "b");
        assert_eq!(p.click(x, y), Click::Handled);
        let mut p = page_from(r#"<form id=f action="/go"><input name=a value=1></form><button id=b onclick="document.getElementById('f').submit()">x</button>"#);
        let (x, y) = center_of(&p, "b");
        assert_eq!(p.click(x, y), Click::Navigate(Nav { url: "https://example.com/go?a=1".into(), post: None }));
        let mut p = page_from(r#"<button id=b onclick="location.href = '/next'">x</button>"#);
        let (x, y) = center_of(&p, "b");
        assert_eq!(p.click(x, y), Click::Navigate(Nav { url: "https://example.com/next".into(), post: None }));
    }

    #[test]
    fn javascript_page_and_clicks() {
        let mut p = Page::new(load("about:js").unwrap(), 900.0, 800.0);
        assert_eq!(text_of(&p, "title"), "JavaScript ✓");
        assert_eq!(text_of(&p, "count"), "0");
        let (x, y) = center_of(&p, "inc");
        p.click(x, y);
        p.click(x, y);
        assert_eq!(text_of(&p, "count"), "2");
        let (x, y) = center_of(&p, "dec");
        p.click(x, y);
        assert_eq!(text_of(&p, "count"), "1");
        // inline onclick attribute handler
        let reset = (0..p.doc.nodes.len() as u32).find(|&n| p.doc.attr(n, crate::atom::lookup("onclick").unwrap()).is_some()).unwrap();
        let r = p.tree.node_rects()[&reset];
        p.click(r[0] + 5.0, r[1] + 5.0);
        assert_eq!(text_of(&p, "count"), "0");
        // clicking a to-do toggles it; the page relays out
        let before = p.layouts;
        let todos = text_of(&p, "todos");
        assert!(todos.contains("Wire up the DOM"), "{todos}");
        let (x, y) = center_of(&p, "add");
        p.click(x, y);
        assert!(text_of(&p, "todos").contains("Item 4"));
        assert!(p.layouts > before);
        // timers advance the progress text
        assert!(p.tick(250.0));
        assert!(text_of(&p, "ticks").contains("1 tick"));
        // Intervals don't catch up after a stall; step through time like frames do.
        for i in 1..20 {
            p.tick(250.0 + 200.0 * i as f64);
        }
        assert!(text_of(&p, "ticks").contains("done"), "{}", text_of(&p, "ticks"));
    }
}

/// A large synthetic document for benchmarking.
pub fn stress_page() -> String {
    let mut s = String::with_capacity(4 << 20);
    s.push_str("<!doctype html><html><head><title>Stress test</title><style>\n");
    s.push_str("body { font-family: sans-serif; max-width: 900px; margin: 0 auto; padding: 16px }\n");
    for i in 0..200 {
        s.push_str(&format!(".c{i} {{ color: #{:06x} }} .section .c{i} b {{ font-weight: bold }}\n", (i * 7919) & 0xffffff));
    }
    s.push_str("nav a { margin-right: 8px } .note { background: #fff3c4; padding: 8px; border-left: 4px solid #e0a800 }\n");
    s.push_str("table.data { border-collapse: collapse; width: 100% } table.data td, table.data th { border: 1px solid #ccc; padding: 4px 8px }\n");
    s.push_str("tr:nth-child(even) td { background: #f6f8fa } .section > h2 + p { font-size: 17px }\n");
    s.push_str("</style></head><body><nav>");
    for i in 0..30 {
        s.push_str(&format!("<a href=\"#s{i}\">Section {i}</a> "));
    }
    s.push_str("</nav>");
    for i in 0..1500 {
        s.push_str(&format!(
            "<div class=\"section\" id=\"s{i}\"><h2>Section {i}: the quick brown fox</h2>\
             <p class=\"c{}\">Lorem ipsum dolor sit amet, <b>consectetur</b> adipiscing elit, sed do <i>eiusmod</i> tempor \
             incididunt ut labore et dolore magna aliqua. Ut enim ad <a href=\"/x/{i}\">minim veniam</a>, quis nostrud \
             exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit \
             in voluptate velit esse cillum dolore eu fugiat nulla pariatur.</p>\
             <ul><li>First item &amp; more</li><li>Second <code>item()</code></li><li>Third item</li></ul>",
            i % 200
        ));
        if i % 10 == 0 {
            s.push_str("<table class=\"data\"><tr><th>Name</th><th>Value</th><th>Description</th></tr>");
            for r in 0..8 {
                s.push_str(&format!("<tr><td>row {r}</td><td>{}</td><td>Some longer descriptive text for row {r}</td></tr>", r * i));
            }
            s.push_str("</table>");
        }
        if i % 7 == 0 {
            s.push_str("<p class=\"note\">Note: this is a highlighted note block with some <em>emphasis</em>.</p>");
        }
        s.push_str("</div>");
    }
    s.push_str("</body></html>");
    s
}
