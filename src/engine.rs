//! The rendering pipeline: load → parse → cascade → layout → display list,
//! plus image loading (fetched in parallel after the first render).

use crate::atom::k;
use crate::css::{self, Media};
use crate::dom::{Data, Document, Element, NodeId, NONE};
use crate::forms::{self, Activation};
pub use crate::forms::Nav;
use crate::fx::FxHashMap;
use crate::image::{self, Image};
use crate::layout::{self, LayoutTree};
use crate::paint::{self, DisplayList};
use crate::style::{self, Display, Ovf, Style};
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
    /// By absolute URL; None = failed or unsupported format (or still
    /// being fetched, if in `pending`).
    pub images: FxHashMap<String, Option<Arc<Image>>>,
    /// Image URLs requested but not yet arrived.
    pub pending: crate::fx::FxHashSet<String>,
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
    /// Fetched `<link rel=stylesheet>` sheets by absolute URL.
    pub css: FxHashMap<String, String>,
    pub doc: Document,
    /// External script sources by absolute URL.
    pub scripts: FxHashMap<String, String>,
    pub fetch_ms: f64,
    pub parse_ms: f64,
}

fn base_of(doc: &Document, url: &str) -> String {
    doc.base_href().map(|b| net::resolve(url, &b)).unwrap_or_else(|| url.to_owned())
}

/// The element a URL's fragment names (`:target`), or NONE.
fn target_of(doc: &Document, url: &str) -> NodeId {
    let frag = url.split_once('#').map(|(_, f)| net::percent_decode(f)).unwrap_or_default();
    if frag.is_empty() {
        return NONE;
    }
    let a = crate::atom::lookup(&frag);
    doc.find(|e| (a.is_some() && e.id == a) || (e.tag == k::a && e.attr(k::name) == Some(frag.as_str())))
}

fn print_only(media: Option<&str>) -> bool {
    media.is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("print") && !m.contains("screen") && !m.contains("all")
    })
}

/// The absolute URL of a `<link rel=stylesheet>` that applies to the screen.
fn stylesheet_link(e: &Element, base: &str) -> Option<String> {
    let rel = e.attr(k::rel)?.to_ascii_lowercase();
    if !rel.split_ascii_whitespace().any(|r| r == "stylesheet") || rel.contains("alternate") || print_only(e.attr(k::media)) || e.attr(k::disabled).is_some() {
        return None;
    }
    let h = e.attr(k::href)?.trim();
    (!h.is_empty()).then(|| net::resolve(base, h))
}

/// The document's stylesheets in tree order, as (css, base url): `<style>`
/// contents and `<link rel=stylesheet>` sheets found in `css`. Links not in
/// `css` yet are pushed to `missing`.
fn collect_sheets(doc: &Document, base: &str, css: &FxHashMap<String, String>, missing: &mut Vec<String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![0 as NodeId];
    while let Some(n) = stack.pop() {
        if let Some(e) = doc.elem(n) {
            if e.tag == k::style {
                if !print_only(e.attr(k::media)) && e.attr(k::type_).is_none_or(|t| t.trim().is_empty() || t.trim().eq_ignore_ascii_case("text/css")) {
                    let mut s = String::new();
                    doc.text_content(n, &mut s);
                    // Rules inserted through the CSSOM (insertRule) replace the text.
                    match doc.cssom.get(&n) {
                        Some(d) if d.init && d.base == s => out.push((d.rules.join("\n"), base.to_owned())),
                        _ => out.push((s, base.to_owned())),
                    }
                }
            } else if e.tag == k::link {
                if let Some(u) = stylesheet_link(e, base) {
                    match (css.get(&u), doc.cssom.get(&n)) {
                        // Read / edited through the CSSOM: its rules replace
                        // the fetched text.
                        (Some(t), Some(d)) if d.init && d.base == *t => out.push((d.rules.join("\n"), u)),
                        (Some(t), d) => {
                            out.push((t.clone(), u.clone()));
                            // Rules inserted before the text was known.
                            if let Some(d) = d.filter(|d| d.base.is_empty() && !d.rules.is_empty()) {
                                out.push((d.rules.join("\n"), u));
                            }
                        }
                        (None, _) => missing.push(u.clone()),
                    }
                }
            }
        }
        let mut c = doc.nodes[n as usize].last_child;
        while c != NONE {
            stack.push(c);
            c = doc.nodes[c as usize].prev;
        }
    }
    for &i in &doc.adopted {
        if let Some(rules) = doc.free_sheets.get(i) {
            out.push((rules.join("\n"), base.to_owned()));
        }
    }
    out
}

/// The document's current stylesheets, fetching any linked sheets that
/// haven't been downloaded yet (e.g. added by a script).
pub fn sheets_for(doc: &Document, base: &str, css: &mut FxHashMap<String, String>) -> Vec<(String, String)> {
    let mut missing = Vec::new();
    let mut sheets = collect_sheets(doc, base, css, &mut missing);
    if !missing.is_empty() {
        fetch_css(&mut missing, css);
        sheets = collect_sheets(doc, base, css, &mut Vec::new());
    }
    expand_imports(sheets, css, 0)
}

fn fetch_css(urls: &mut Vec<String>, css: &mut FxHashMap<String, String>) {
    urls.sort();
    urls.dedup();
    for (u, b) in urls.iter().zip(net::fetch_many(urls)) {
        css.insert(u.clone(), b.map(|b| net::decode(&b)).unwrap_or_default());
    }
}

/// The leading `@import` rules of a sheet: (absolute url, media) and the
/// rest of the text.
fn split_imports<'a>(text: &'a str, base: &str) -> (Vec<(String, String)>, &'a str) {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let t = rest.trim_start();
        if let Some(c) = t.strip_prefix("/*") {
            rest = c.find("*/").map_or("", |e| &c[e + 2..]);
            continue;
        }
        let lower = t.get(..8).map(str::to_ascii_lowercase).unwrap_or_default();
        if lower.starts_with("@charset") {
            rest = t.find(';').map_or("", |e| &t[e + 1..]);
            continue;
        }
        if !lower.starts_with("@import") {
            return (out, t);
        }
        let Some(end) = t.find(';') else { return (out, "") };
        let stmt = t[7..end].trim();
        rest = &t[end + 1..];
        let (url, media) = if let Some(u) = stmt.strip_prefix("url(") {
            let close = u.find(')').unwrap_or(u.len());
            (u[..close].trim().trim_matches(|c| c == '"' || c == '\''), u.get(close + 1..).unwrap_or(""))
        } else if let Some(q) = stmt.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let s = &stmt[1..];
            let close = s.find(q).unwrap_or(s.len());
            (&s[..close], s.get(close + 1..).unwrap_or(""))
        } else {
            continue;
        };
        // layer(...) / supports(...) conditions before the media list.
        let mut media = media.trim();
        if media.starts_with("layer") {
            media = media.find(')').map_or(media.trim_start_matches("layer"), |e| &media[e + 1..]).trim();
        }
        if let Some(s) = media.strip_prefix("supports(") {
            let close = s.rfind(')').unwrap_or(s.len());
            if !css::supports(&format!("({})", &s[..close])) {
                continue;
            }
            media = s.get(close + 1..).unwrap_or("").trim();
        }
        out.push((net::resolve(base, url), media.to_owned()));
    }
}

/// Replace sheets' `@import` rules by the imported sheets (fetched as needed).
fn expand_imports(sheets: Vec<(String, String)>, css: &mut FxHashMap<String, String>, depth: u32) -> Vec<(String, String)> {
    let mut missing: Vec<String> = Vec::new();
    for (text, base) in &sheets {
        if text.contains("@import") {
            missing.extend(split_imports(text, base).0.into_iter().map(|(u, _)| u).filter(|u| !css.contains_key(u)));
        }
    }
    if !missing.is_empty() {
        fetch_css(&mut missing, css);
    }
    let mut out = Vec::with_capacity(sheets.len());
    for (text, base) in sheets {
        if depth > 4 || !text.contains("@import") {
            out.push((text, base));
            continue;
        }
        let (imports, rest) = split_imports(&text, &base);
        if imports.is_empty() {
            out.push((text, base));
            continue;
        }
        let imported: Vec<(String, String)> = imports
            .into_iter()
            .map(|(u, media)| {
                let t = css.get(&u).cloned().unwrap_or_default();
                let t = if media.is_empty() { t } else { format!("@media {media} {{\n{t}\n}}") };
                (t, u)
            })
            .collect();
        out.extend(expand_imports(imported, css, depth + 1));
        out.push((rest.to_owned(), base));
    }
    out
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
    let mut urls = Vec::new();
    collect_sheets(&doc, &base, &FxHashMap::default(), &mut urls);
    let script_urls: Vec<String> = doc
        .scripts
        .iter()
        .filter(|&&s| matches!(script_kind(&doc, s), ScriptKind::Classic | ScriptKind::Module))
        .filter_map(|&s| doc.attr(s, k::src))
        .map(|h| net::resolve(&base, h.trim()))
        .collect();
    // Stylesheets and scripts download together in one parallel batch.
    let all: Vec<String> = urls.iter().chain(&script_urls).cloned().collect();
    let mut bodies = net::fetch_many(&all).into_iter().map(|b| b.map(|b| net::decode(&b)).unwrap_or_default());
    let css = urls.iter().map(|u| (u.clone(), bodies.next().unwrap_or_default())).collect();
    let scripts = script_urls.iter().map(|u| (u.clone(), bodies.next().unwrap_or_default())).collect();
    Ok(Loaded { url, html, css, doc, scripts, fetch_ms: fetch_html + ms(t), parse_ms })
}

/// Style and layout for scripts that read geometry: from cached stylesheets
/// only (no network, images unloaded). Returns element rects, scroll
/// metrics and computed styles.
#[allow(clippy::type_complexity)]
pub fn script_layout(doc: &Document, src: &js::LayoutSrc, images: &FxHashMap<String, Option<Arc<Image>>>, vw: f32, vh: f32) -> (FxHashMap<NodeId, [f32; 4]>, FxHashMap<NodeId, [f32; 4]>, Vec<Style>) {
    let mut missing = Vec::new();
    let sheets = collect_sheets(doc, &src.base, &src.css, &mut missing);
    let cas = style::cached_cascade(&sheets, &Media { vw, vh }, doc.quirks);
    css::set_base(&src.base);
    let styles = style::compute(doc, &cas, vw, vh);
    // Loaded images keep their sizes in script-side layout.
    let mut res = Resources { images: images.clone(), ..Resources::default() };
    Page::collect_img_src(doc, &src.base, &mut res);
    let tree = layout::layout(doc, &styles, vw, vh, &mut res);
    let mut metrics = FxHashMap::default();
    for b in &tree.boxes {
        if b.anon || metrics.contains_key(&b.node) {
            continue;
        }
        let (cw, ch) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
        metrics.insert(b.node, [cw.max(b.scroll_w + b.p[1] + b.p[3]), ch.max(b.scroll_h + b.p[0] + b.p[2]), cw, ch]);
    }
    (tree.node_rects(), metrics, styles)
}

#[derive(Clone, Copy, PartialEq)]
enum ScriptKind {
    Classic,
    Module,
    ImportMap,
    /// Data blocks (`type=application/json`...) and `nomodule` fallbacks.
    Other,
}

fn script_kind(doc: &Document, s: NodeId) -> ScriptKind {
    let nomodule = crate::atom::lookup("nomodule").is_some_and(|a| doc.attr(s, a).is_some());
    match doc.attr(s, k::type_).map(|t| t.trim().to_ascii_lowercase()).as_deref() {
        Some("module") => ScriptKind::Module,
        Some("importmap") => ScriptKind::ImportMap,
        _ if nomodule => ScriptKind::Other,
        None | Some("") => ScriptKind::Classic,
        Some(t) if t.contains("javascript") || t.contains("ecmascript") || t == "text/jscript" => ScriptKind::Classic,
        _ => ScriptKind::Other,
    }
}

fn has_attr(doc: &Document, s: NodeId, name: &str) -> bool {
    crate::atom::lookup(name).is_some_and(|a| doc.attr(s, a).is_some())
}

/// Run one `<script>` element (classic or module).
fn run_script_node(it: &mut js::Interp, s: NodeId, kind: ScriptKind, sources: &FxHashMap<String, String>, base: &str) {
    let src = it.doc.attr(s, k::src).map(|v| net::resolve(base, v.trim()));
    let inline = || {
        let mut c = String::new();
        it.doc.text_content(s, &mut c);
        c
    };
    if kind == ScriptKind::Module {
        match src {
            Some(url) => {
                if let Some(code) = sources.get(&url) {
                    it.module_sources.insert(url.clone(), code.clone());
                }
                it.run_module(&url, None);
            }
            None => {
                let code = inline();
                it.run_module(base, Some(code));
            }
        }
        return;
    }
    let external = src.is_some();
    let code = match src {
        Some(url) => match sources.get(&url) {
            Some(c) => c.clone(),
            None => {
                js::dombind::dispatch_nobubble(it, s, "error");
                return;
            }
        },
        None => inline(),
    };
    let saved = std::mem::replace(&mut it.current_script, s);
    run_one(it, &code);
    it.current_script = saved;
    if external {
        js::dombind::dispatch_nobubble(it, s, "load");
    }
}

/// Wall-clock budget for running a page's scripts at load.
const SCRIPT_BUDGET_MS: f64 = 3000.0;

/// The load-time script budget (`BOLT_SCRIPT_BUDGET` ms overrides it).
fn script_budget() -> f64 {
    static B: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *B.get_or_init(|| std::env::var("BOLT_SCRIPT_BUDGET").ok().and_then(|v| v.parse().ok()).unwrap_or(SCRIPT_BUDGET_MS))
}
/// How long the first paint may wait for scripts' network requests.
const NET_WAIT: f64 = 1500.0;

static INTERACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The interactive browser paints as soon as scripts have run; responses
/// that arrive later are delivered by `Page::tick`. (Screenshots and
/// benchmarks wait for startup requests instead.)
pub fn set_interactive(on: bool) {
    INTERACTIVE.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn net_wait() -> f64 {
    if INTERACTIVE.load(std::sync::atomic::Ordering::Relaxed) { 100.0 } else { NET_WAIT }
}

/// Run a page's scripts (in document order), load events and zero-delay timers.
fn run_scripts(doc: Document, sources: &FxHashMap<String, String>, css: &FxHashMap<String, String>, url: &str, base: &str, vw: f32, vh: f32) -> (Document, Option<Box<js::Interp>>, f64) {
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
    it.ready_state = "loading";
    it.deadline = Some(t0 + std::time::Duration::from_millis(script_budget() as u64));
    it.layout_src = Some(std::rc::Rc::new(js::LayoutSrc { css: css.clone(), base: base.to_owned() }));
    // Parser-blocking classic scripts run in order; `defer` and module
    // scripts after them (in order), then `async` ones. Scripts that don't
    // fit the startup budget are queued and run one per event loop turn.
    let (mut classic, mut deferred, mut later) = (Vec::new(), Vec::new(), Vec::new());
    for s in scripts {
        let kind = script_kind(&it.doc, s);
        let external = it.doc.attr(s, k::src).is_some();
        match kind {
            ScriptKind::ImportMap => {
                let mut c = String::new();
                it.doc.text_content(s, &mut c);
                it.add_import_map(&c, base);
            }
            ScriptKind::Other => {}
            _ if has_attr(&it.doc, s, "async") && (external || kind == ScriptKind::Module) => later.push(s),
            ScriptKind::Module => deferred.push(s),
            ScriptKind::Classic if external && has_attr(&it.doc, s, "defer") => deferred.push(s),
            ScriptKind::Classic => classic.push(s),
        }
    }
    for s in classic.into_iter().chain(deferred).chain(later) {
        if it.script_queue.is_empty() && ms(t0) <= script_budget() {
            it.deadline = Some(Instant::now() + std::time::Duration::from_millis(SCRIPT_TASK_MS));
            let kind = script_kind(&it.doc, s);
            run_script_node(&mut it, s, kind, sources, base);
            run_pending(&mut it, &t0);
        } else {
            it.script_queue.push_back(s);
        }
    }
    if it.script_queue.is_empty() {
        fire_load_events(&mut it);
    } else {
        it.script_sources = Some(std::rc::Rc::new(sources.clone()));
        it.load_events_pending = true;
    }
    it.run_microtasks();
    if profile_scripts() {
        eprintln!("[profile] scripts + load events done at {:.0}ms", ms(t0));
        js::prof::report();
    }
    // Timers that fire "immediately" so the first paint includes their work.
    for _ in 0..5 {
        if it.run_timers(1.0) == 0 || ms(t0) > script_budget() {
            break;
        }
        run_pending(&mut it, &t0);
    }
    if profile_scripts() {
        eprintln!("[profile] zero-delay timers done at {:.0}ms", ms(t0));
    }
    // Let data requested at startup (fetch / XHR) arrive before the first
    // paint, within a bounded wait; later responses arrive via `tick`.
    let net_start = Instant::now();
    while it.has_pending_requests() && ms(t0) < script_budget() && ms(net_start) < net_wait() {
        let left = (script_budget() - ms(t0)).min(net_wait() - ms(net_start)).max(1.0);
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
    if profile_scripts() {
        eprintln!("[profile] timers + network done at {elapsed:.0}ms (pending requests: {})", it.has_pending_requests());
        js::prof::report();
    }
    it.dirty = false;
    let doc = std::mem::take(&mut it.doc);
    (doc, Some(it), elapsed)
}

/// How long one script (or event handler / timer task) may run.
const SCRIPT_TASK_MS: u64 = 5000;

/// The document finished loading: readystatechange, DOMContentLoaded, load.
fn fire_load_events(it: &mut js::Interp) {
    it.load_events_pending = false;
    it.ready_state = "interactive";
    js::dombind::dispatch_nobubble(it, 0, "readystatechange");
    js::dombind::dispatch(it, 0, "DOMContentLoaded", |_, _| {});
    it.ready_state = "complete";
    js::dombind::dispatch_nobubble(it, 0, "readystatechange");
    js::dombind::dispatch_nobubble(it, js::WINDOW, "load");
    js::dombind::dispatch_nobubble(it, js::WINDOW, "pageshow");
}

fn run_one(it: &mut js::Interp, code: &str) {
    if code.trim().is_empty() {
        return;
    }
    it.steps = 0;
    let t = Instant::now();
    if let Err(e) = it.run_script(code) {
        it.log(&e);
    }
    let run = ms(t);
    it.run_microtasks();
    if profile_scripts() {
        let head: String = code.trim_start().chars().take(60).filter(|c| !c.is_control()).collect();
        eprintln!("[profile] {run:.0}ms run + {:.0}ms microtasks, {} bytes: {head}", ms(t) - run, code.len());
    }
}

/// `BOLT_PROFILE=1`: log how long each script takes.
fn profile_scripts() -> bool {
    static P: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *P.get_or_init(|| std::env::var_os("BOLT_PROFILE").is_some())
}

/// Fetch and run scripts that were inserted by other scripts; they fire
/// `load` (or `error`) when done.
fn run_pending(it: &mut js::Interp, t0: &Instant) {
    for _ in 0..8 {
        let pending = std::mem::take(&mut it.pending_scripts);
        if pending.is_empty() || ms(*t0) > script_budget() {
            return;
        }
        let urls: Vec<String> = pending.iter().map(|(u, _)| u.clone()).collect();
        let bodies = net::fetch_many(&urls);
        for ((_, node), body) in pending.into_iter().zip(bodies) {
            match body {
                Some(b) => {
                    let saved = std::mem::replace(&mut it.current_script, node);
                    run_one(it, &net::decode(&b));
                    it.current_script = saved;
                    js::dombind::dispatch_nobubble(it, node, "load");
                }
                None => {
                    js::dombind::dispatch_nobubble(it, node, "error");
                }
            }
            it.run_microtasks();
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
    /// Downloaded linked stylesheets by URL.
    pub css: FxHashMap<String, String>,
    pub styles: Vec<Style>,
    pub tree: LayoutTree,
    pub dl: DisplayList,
    pub res: Resources,
    pub js: Option<Box<js::Interp>>,
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
    url_events: Vec<js::UrlEvent>,
    history_go: Option<i32>,
    /// Vertical scroll offset as last told by the browser.
    scroll: f32,
    scroll_changed: bool,
    /// Layout changed since geometry observers were last checked.
    geo_dirty: bool,
    /// Scroll offsets of scroll containers (element -> (x, y)).
    pub elem_scroll: FxHashMap<NodeId, (f32, f32)>,
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
    let cas = style::cached_cascade(sheets, &Media { vw, vh }, doc.quirks);
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
        let (doc, js, script_ms) = run_scripts(l.doc, &l.scripts, &l.css, &l.url, &base, vw, vh);
        let mut res = Resources::default();
        Self::collect_img_src(&doc, &base, &mut res);
        let mut css = l.css;
        let mut doc = doc;
        doc.target = target_of(&doc, &l.url);
        let sheets = sheets_for(&doc, &base, &mut css);
        let (styles, tree, dl) = render(&doc, &sheets, &base, &mut res, vw, vh, &mut t);
        let mut page = Page { url: l.url, base, doc, css, styles, tree, dl, res, js, t, script_ms, layouts: 0, caret: 0, nav: None, focus_value: String::new(), url_events: Vec::new(), history_go: None, scroll: 0.0, scroll_changed: false, geo_dirty: true, elem_scroll: FxHashMap::default(), vw, vh };
        page.sync_js_geometry();
        page.script_requests();
        // Initial IntersectionObserver / ResizeObserver callbacks (lazy
        // loading etc.) before the first paint.
        for _ in 0..3 {
            if !page.check_geometry() {
                break;
            }
        }
        page
    }

    pub(crate) fn collect_img_src(doc: &Document, base: &str, res: &mut Resources) {
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

    /// Fire `load` / `error` at <img> elements whose image has arrived
    /// (or failed) since the last call.
    pub fn fire_image_events(&mut self) {
        let Some(it) = &self.js else { return };
        let due: Vec<(NodeId, String, bool)> = self
            .res
            .img_src
            .iter()
            .filter_map(|(&n, u)| {
                if self.res.pending.contains(u) {
                    return None;
                }
                let ok = self.res.images.get(u)?.is_some();
                (it.img_fired.get(&n) != Some(u)).then(|| (n, u.clone(), ok))
            })
            .collect();
        if due.is_empty() {
            return;
        }
        self.with_js(|it| {
            for (n, u, ok) in due {
                it.img_fired.insert(n, u);
                js::dombind::dispatch_nobubble(it, n, if ok { "load" } else { "error" });
            }
        });
    }

    /// Give scripts the latest element geometry.
    fn sync_js_geometry(&mut self) {
        if let Some(it) = &mut self.js {
            if it.images.len() + self.res.pending.len() != self.res.images.len() {
                it.images = self.res.images.iter().filter(|(u, _)| !self.res.pending.contains(*u)).map(|(u, i)| (u.clone(), i.clone())).collect();
            }
            it.rects = self.tree.node_rects();
            it.vw = self.vw;
            it.vh = self.vh;
            // scrollWidth / scrollHeight / clientWidth / clientHeight
            it.scroll_metrics.clear();
            for b in &self.tree.boxes {
                if b.anon || it.scroll_metrics.contains_key(&b.node) {
                    continue;
                }
                let (cw, ch) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
                let (sw, sh) = (cw.max(b.scroll_w + b.p[1] + b.p[3]), ch.max(b.scroll_h + b.p[0] + b.p[2]));
                it.scroll_metrics.insert(b.node, [sw, sh, cw, ch]);
            }
            it.elem_scroll = self.elem_scroll.clone();
            it.geom_version = (self.doc.version, self.vw.to_bits(), self.vh.to_bits());
            if it.layout_src.as_ref().is_none_or(|s| s.css.len() != self.css.len()) {
                it.layout_src = Some(std::rc::Rc::new(js::LayoutSrc { css: self.css.clone(), base: self.base.clone() }));
            }
            self.geo_dirty = true;
        }
    }

    /// Rebuild the display list after scroll containers moved.
    fn apply_elem_scroll(&mut self) {
        self.tree.scroll = self.elem_scroll.clone();
        self.dl = paint::build(&self.tree, &self.doc, &self.styles, &self.res);
        if let Some(it) = &mut self.js {
            it.rects = self.tree.node_rects();
            it.elem_scroll = self.elem_scroll.clone();
        }
    }

    /// Mouse wheel over page point (x, y): scroll the innermost scroll
    /// container that can move. Returns false if none can (the page scrolls).
    pub fn wheel(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> bool {
        let Some(mut n) = self.tree.node_at(x, y) else { return false };
        loop {
            if self.doc.elem(n).is_some() && self.styles.get(n as usize).is_some_and(|s| s.overflow == Ovf::Scroll) {
                if let Some(b) = self.tree.boxes.iter().find(|b| b.node == n && !b.anon) {
                    let (cw, ch) = (b.w + b.p[1] + b.p[3], b.h + b.p[0] + b.p[2]);
                    let max = ((b.scroll_w + b.p[1] + b.p[3] - cw).max(0.0), (b.scroll_h + b.p[0] + b.p[2] - ch).max(0.0));
                    let st = &self.styles[n as usize];
                    let (ox, oy) = self.elem_scroll.get(&n).copied().unwrap_or((0.0, 0.0));
                    let nx = if st.ovf_x == Ovf::Scroll { (ox + dx).clamp(0.0, max.0) } else { ox };
                    let ny = if st.ovf_y == Ovf::Scroll { (oy + dy).clamp(0.0, max.1) } else { oy };
                    if (nx, ny) != (ox, oy) {
                        self.elem_scroll.insert(n, (nx, ny));
                        self.apply_elem_scroll();
                        let target = n;
                        self.with_js(|it| {
                            js::dombind::dispatch_nobubble(it, target, "scroll");
                        });
                        return true;
                    }
                }
            }
            let p = self.doc.parent(n);
            if p == NONE || p == 0 {
                return false;
            }
            n = p;
        }
    }

    /// Run IntersectionObserver / ResizeObserver callbacks if the layout or
    /// scroll position changed. Returns true if any ran.
    fn check_geometry(&mut self) -> bool {
        let wanted = self.js.as_ref().is_some_and(|it| it.has_geometry_observers() && (self.geo_dirty || it.geo_pending));
        self.geo_dirty = false;
        if !wanted {
            return false;
        }
        self.with_js(|it| it.check_geometry()).unwrap_or(false)
    }

    /// The pointer moved over `node` (`down`: button pressed). Updates
    /// :hover / :active, fires mouse / pointer events. Returns true if the
    /// page changed.
    pub fn set_pointer(&mut self, node: Option<NodeId>, down: bool) -> bool {
        let el = node.map_or(NONE, |n| if self.doc.elem(n).is_some() { n } else { self.doc.parent(n) });
        let active = if down { el } else { NONE };
        let (old_hover, old_active) = (self.doc.hover, self.doc.active);
        if el == old_hover && active == old_active {
            return false;
        }
        let before = self.layouts;
        self.doc.hover = el;
        self.doc.active = active;
        // (node, type, bubbles)
        let mut evs: Vec<(NodeId, &'static str, bool)> = Vec::new();
        if el != old_hover {
            if old_hover != NONE {
                evs.extend([(old_hover, "pointerout", true), (old_hover, "mouseout", true), (old_hover, "pointerleave", false), (old_hover, "mouseleave", false)]);
            }
            if el != NONE {
                evs.extend([(el, "pointerover", true), (el, "mouseover", true), (el, "pointerenter", false), (el, "mouseenter", false)]);
            }
        }
        if active != old_active {
            if active != NONE {
                evs.extend([(active, "pointerdown", true), (active, "mousedown", true)]);
            } else if el != NONE {
                evs.extend([(el, "pointerup", true), (el, "mouseup", true)]);
            }
        }
        let restyle = css::hover_rules_seen();
        if let Some(it) = &mut self.js {
            it.dirty |= restyle;
            self.with_js(|it| {
                for (n, ty, bubbles) in evs {
                    if bubbles {
                        js::dombind::dispatch(it, n, ty, |_, _| {});
                    } else {
                        js::dombind::dispatch_nobubble(it, n, ty);
                    }
                }
            });
        } else if restyle {
            self.relayout(self.vw, self.vh);
        }
        self.layouts != before
    }

    /// The browser scrolled the page.
    pub fn set_scroll(&mut self, y: f32) {
        if (y - self.scroll).abs() > 0.5 {
            self.scroll = y;
            self.scroll_changed = true;
            if let Some(it) = &mut self.js {
                it.scroll_y = y;
            }
        }
    }

    /// A scroll position requested by a script.
    pub fn take_scroll_request(&mut self) -> Option<f32> {
        self.js.as_mut().and_then(|it| it.scroll_request.take())
    }

    /// Run `f` with the document moved into the script engine; relayout if
    /// the DOM changed.
    fn with_js<T>(&mut self, f: impl FnOnce(&mut js::Interp) -> T) -> Option<T> {
        let it = self.js.as_mut()?;
        it.doc = std::mem::take(&mut self.doc);
        // Event handlers and timers get a leash; generous, since a killed
        // task (half-rendered component) is worse than a slow one.
        it.deadline = Some(Instant::now() + std::time::Duration::from_millis(SCRIPT_TASK_MS));
        let focus_before = it.doc.focus;
        let r = f(it);
        it.run_microtasks();
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

    /// Run a script against the live page (developer aid); returns the
    /// completion value or the error, as text.
    pub fn eval(&mut self, src: &str) -> Option<String> {
        self.with_js(|it| {
            let r = it.eval_str(src);
            it.run_microtasks();
            r
        })
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
        // pushState / replaceState / fragment changes: the document's URL
        // changes without a navigation.
        self.url_events.append(&mut it.url_events);
        if self.url != it.url {
            self.url = it.url.clone();
            self.doc.target = target_of(&self.doc, &self.url);
            if self.doc.base_href().is_none() {
                self.base = self.url.clone();
            }
        }
        if let Some(n) = it.history_go.take() {
            self.history_go = Some(n);
        }
        if std::mem::take(&mut it.elem_scroll_dirty) {
            self.elem_scroll = it.elem_scroll.clone();
            self.apply_elem_scroll();
        }
    }

    /// Same-document URL changes since the last call (for session history).
    pub fn take_url_events(&mut self) -> Vec<js::UrlEvent> {
        std::mem::take(&mut self.url_events)
    }

    /// This document's current session history position.
    pub fn hist_pos(&self) -> usize {
        self.js.as_ref().map_or(0, |it| it.hist_pos)
    }

    /// `history.go(n)` requested by a script.
    pub fn take_history_go(&mut self) -> Option<i32> {
        self.history_go.take()
    }

    /// Back / forward to this document's session history entry `pos`.
    pub fn pop_state(&mut self, pos: usize) {
        self.with_js(|it| js::dombind::pop_state(it, pos));
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
        // Page scripts deferred past the startup budget: one per turn.
        if self.js.as_ref().is_some_and(|it| !it.script_queue.is_empty() || it.load_events_pending) {
            let base = self.base.clone();
            self.with_js(|it| {
                if let Some(s) = it.script_queue.pop_front() {
                    let sources = it.script_sources.clone().unwrap_or_default();
                    let kind = script_kind(&it.doc, s);
                    it.dirty = true;
                    run_script_node(it, s, kind, &sources, &base);
                }
                if it.script_queue.is_empty() && it.load_events_pending {
                    it.script_sources = None;
                    fire_load_events(it);
                }
            });
            return true;
        }
        let Some(it) = &self.js else { return false };
        let due = it.next_timer_at().is_some_and(|t| t <= now);
        let geo = it.has_geometry_observers() && (self.geo_dirty || it.geo_pending || self.scroll_changed);
        if !due && it.requests.is_empty() && !self.scroll_changed && !geo {
            return false;
        }
        let before = self.layouts;
        if std::mem::take(&mut self.scroll_changed) {
            self.geo_dirty = true;
            self.with_js(|it| {
                js::dombind::dispatch(it, 0, "scroll", |_, _| {});
            });
        }
        if due || !self.js.as_ref().is_some_and(|it| it.requests.is_empty()) {
            self.with_js(|it| {
                it.poll_requests(None);
                if due {
                    it.run_timers(now);
                }
            });
        }
        self.check_geometry();
        self.layouts != before
    }

    /// Whether scripts still have timers or requests outstanding (for
    /// headless runs deciding when the page has settled).
    pub fn has_pending_work(&self, _now: f64) -> bool {
        self.js.as_ref().is_some_and(|it| it.next_timer_at().is_some() || it.has_pending_requests())
    }

    pub fn has_pending_requests(&self) -> bool {
        self.js.as_ref().is_some_and(|it| it.has_pending_requests())
    }

    /// Re-run style + layout for a new viewport size (media queries may change).
    pub fn relayout(&mut self, vw: f32, vh: f32) {
        self.vw = vw;
        self.vh = vh;
        let sheets = sheets_for(&self.doc, &self.base, &mut self.css);
        let (styles, tree, dl) = render(&self.doc, &sheets, &self.base, &mut self.res, vw, vh, &mut self.t);
        self.styles = styles;
        self.tree = tree;
        self.dl = dl;
        if !self.elem_scroll.is_empty() {
            self.tree.scroll = self.elem_scroll.clone();
            self.dl = paint::build(&self.tree, &self.doc, &self.styles, &self.res);
        }
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
        self.fire_image_events();
    }

    pub fn title(&self) -> String {
        self.doc.title()
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
        let l = Loaded { url: "https://example.com/start".into(), html: html.into(), css: FxHashMap::default(), doc, scripts: FxHashMap::default(), fetch_ms: 0.0, parse_ms: 0.0 };
        Page::new(l, 800.0, 600.0)
    }

    fn node_by_id(p: &Page, id: &str) -> NodeId {
        let a = crate::atom::lookup(id).unwrap();
        (0..p.doc.nodes.len() as u32).find(|&n| p.doc.elem(n).is_some_and(|e| e.id == Some(a))).unwrap()
    }

    #[test]
    fn intersection_observer() {
        let mut p = page_from(
            r#"<!doctype html><div id=top style="height:100px">top</div><div style="height:3000px"></div><div id=far style="height:50px">far</div>
               <script>
                 window.seen = [];
                 const io = new IntersectionObserver(es => es.forEach(e => seen.push(e.target.id + '=' + e.isIntersecting)));
                 io.observe(document.getElementById('top')); io.observe(document.getElementById('far'));
                 new ResizeObserver(es => seen.push('size ' + es[0].contentRect.height)).observe(document.getElementById('top'));
               </script>"#,
        );
        let seen = |p: &mut Page| p.with_js(|it| {
            let g = js::Value::Obj(it.global.clone());
            let s = it.get(&g, "seen").unwrap();
            let j = it.get(&g, "JSON").unwrap();
            let f = it.get(&j, "stringify").unwrap();
            let v = it.call(&f, j, &[s]).unwrap();
            it.to_str(&v).unwrap().to_string()
        }).unwrap();
        assert_eq!(seen(&mut p), r#"["top=true","far=false","size 100"]"#);
        p.set_scroll(3000.0);
        p.tick(0.0);
        assert_eq!(seen(&mut p), r#"["top=true","far=false","size 100","top=false","far=true"]"#);
    }

    #[test]
    fn shadow_dom_renders() {
        let p = page_from(
            r#"<!doctype html><style>b { color: rgb(255, 0, 0) } .in { color: rgb(9, 9, 9) }</style>
               <x-card id=card><b id=light slot=title>Title</b><i id=body>Body</i></x-card>
               <script>
                 customElements.define('x-card', class extends HTMLElement {
                   constructor() {
                     super();
                     const r = this.attachShadow({ mode: 'open' });
                     r.innerHTML = `<style>:host { display: block; color: rgb(0, 0, 255) }
                       .in { font-weight: bold } ::slotted(i) { color: rgb(0, 128, 0) }</style>
                       <h2><slot name=title>no title</slot></h2><p class=in id=inner>inner <slot></slot></p>`;
                   }
                 });
                 const c = document.getElementById('card');
                 c.shadowRoot.getElementById('inner').addEventListener('click', () => {}, false);
                 c.addEventListener('ping', () => { document.title = 'host got it'; });
                 c.shadowRoot.getElementById('inner').dispatchEvent(new CustomEvent('ping', { bubbles: true }));
                 document.title += ' ' + (c.shadowRoot.getElementById('inner').getRootNode() === c.shadowRoot);
               </script>"#,
        );
        let inner = {
            let root = p.doc.shadows[&node_by_id(&p, "card")];
            (0..p.doc.nodes.len() as u32).find(|&n| p.doc.tree_root(n) == root && p.doc.elem(n).is_some_and(|e| e.id == crate::atom::lookup("inner"))).unwrap()
        };
        let color = |n: NodeId| p.styles[n as usize].color & 0xFFFFFF;
        // :host styles the host; shadow styles don't leak out, document styles don't leak in.
        assert_eq!(color(node_by_id(&p, "card")), 0x0000FF);
        assert_eq!(color(inner), 0x0000FF);
        assert!(p.styles[inner as usize].bold);
        // Light DOM keeps document styles; ::slotted applies.
        assert_eq!(color(node_by_id(&p, "light")), 0xFF0000);
        assert_eq!(color(node_by_id(&p, "body")), 0x008000);
        assert_eq!(p.title(), "host got it true");
        let text: String = p.tree.text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(text.contains("TitleinnerBody"), "{text}");
    }

    #[test]
    fn script_geometry_and_computed_style() {
        let p = page_from(
            r#"<!doctype html><style>body { margin: 0 } #a { width: 120px; padding: 5px; color: rgb(1, 2, 3); display: flex }
               .wide { width: 300px !important } #a::after { content: "x" }</style>
               <div id=a>a</div>
               <script>
                 const a = document.getElementById('a');
                 const before = a.offsetWidth;
                 a.classList.add('wide');
                 const after = a.getBoundingClientRect().width;
                 const cs = getComputedStyle(a);
                 const d = document.createElement('div'); d.style.height = '42px'; document.body.appendChild(d);
                 document.title = [before, after, cs.display, cs.color, cs.getPropertyValue('padding-left'), cs.width, d.offsetHeight, getComputedStyle(a, '::after').content].join('|');
               </script>"#,
        );
        assert_eq!(p.title(), r#"130|310|flex|rgb(1, 2, 3)|5px|300px|42|"x""#);
    }

    #[test]
    fn scroll_containers() {
        let mut p = page_from(
            r#"<!doctype html><style>body { margin: 0 } #box { height: 100px; overflow-y: auto; width: 200px }
               .row { height: 50px } #r3 { background: rgb(255, 0, 0) }</style>
               <div id=box><div class=row></div><div class=row></div><div class=row id=r3></div><div class=row></div></div>
               <p id=after>after</p>
               <script>
                 const b = document.getElementById('box');
                 b.addEventListener('scroll', () => document.title += ' scrolled ' + b.scrollTop);
               </script>"#,
        );
        p.with_js(|it| {
            let _ = it.run_script("const bx = document.getElementById('box'); document.title = [bx.scrollHeight, bx.clientHeight, bx.scrollTop].join(',')");
        });
        assert_eq!(p.title(), "200,100,0");
        // Content is clipped: #after starts right below the 100px box.
        assert_eq!(p.tree.node_rects()[&node_by_id(&p, "after")][1], 116.0);
        let (w, h) = (400, 300);
        let paint_px = |p: &Page, x: usize, y: usize| {
            let mut buf = vec![0u32; w * h];
            let mut glyphs = crate::fonts::GlyphCache::default();
            paint::paint(&p.dl, &p.tree.text, &mut glyphs, &mut buf, w, h, 0.0);
            buf[y * w + x] & 0xFFFFFF
        };
        assert_eq!(paint_px(&p, 10, 50), 0xFFFFFF);
        // Wheel over the box scrolls it (by up to 100px), not the page.
        assert!(p.wheel(10.0, 50.0, 0.0, 80.0));
        assert_eq!(paint_px(&p, 10, 50), 0xFF0000);
        assert!(p.title().ends_with("scrolled 80"), "{}", p.title());
        assert!(p.wheel(10.0, 50.0, 0.0, 80.0));
        assert!(!p.wheel(10.0, 50.0, 0.0, 80.0), "at the end the page scrolls instead");
        p.with_js(|it| {
            let _ = it.run_script("document.getElementById('box').scrollTop = 0; document.title = 'top ' + document.getElementById('box').scrollTop");
        });
        assert_eq!(p.title(), "top 0");
        assert_eq!(paint_px(&p, 10, 50), 0xFFFFFF);
    }

    #[test]
    fn inline_boxes() {
        let p = page_from(
            r#"<!doctype html><style>body { margin: 0; font-size: 16px; line-height: 20px }
               #b { padding: 2px 30px; background: rgb(255, 0, 0); border-radius: 3px }
               #l { border-left: 4px solid rgb(0, 0, 255) }</style>
               <p id=p>a<span id=b>X</span>c</p><p><span id=l>z</span></p>"#,
        );
        let (w, h) = (400, 200);
        let mut buf = vec![0u32; w * h];
        let mut glyphs = crate::fonts::GlyphCache::default();
        paint::paint(&p.dl, &p.tree.text, &mut glyphs, &mut buf, w, h, 0.0);
        let rp = p.tree.node_rects()[&node_by_id(&p, "p")];
        let px = |x: f32, y: f32| buf[y as usize * w + x as usize] & 0xFFFFFF;
        // The span's left padding (30px) is painted red right after "a".
        let mid = rp[1] + rp[3] / 2.0;
        let a_w = 10.0;
        assert_eq!(px(a_w + 15.0, mid), 0xFF0000);
        // "c" is pushed right by both paddings: the paragraph is wider than a+X+c alone.
        assert!(rp[2] > 60.0, "{:?}", rp);
        let rl = p.tree.node_rects()[&node_by_id(&p, "l")];
        assert_eq!(px(rl[0] + 1.0, rl[1] + rl[3] / 2.0), 0x0000FF);
    }

    #[test]
    fn line_breaking() {
        let p = page_from(
            "<!doctype html><style>body { margin: 0; font-size: 16px } div { width: 100px }
               #url { overflow-wrap: anywhere } #nowrap { width: 60px }
               #ell { width: 60px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis }
               #clamp { -webkit-line-clamp: 2; display: -webkit-box; overflow: hidden; line-height: 20px }</style>
               <div id=cjk>\u{4ECA}\u{65E5}\u{306F}\u{3044}\u{3044}\u{5929}\u{6C17}\u{3067}\u{3059}\u{306D}\u{3002}\u{660E}\u{65E5}\u{3082}\u{6674}\u{308C}\u{308B}\u{3067}\u{3057}\u{3087}\u{3046}\u{3002}</div>
               <div id=url>https://example.com/a/very/long/path/that/never/ends</div>
               <div id=hy>well-known-hyphenated-compound</div>
               <div id=ell>a long sentence that overflows</div>
               <div id=clamp>one two three four five six seven eight nine ten eleven twelve</div>",
        );
        let rect = |id: &str| p.tree.node_rects()[&node_by_id(&p, id)];
        // CJK breaks between characters, so it wraps onto several lines within 100px.
        assert!(rect("cjk")[3] > 30.0 && rect("cjk")[2] <= 100.0, "{:?}", rect("cjk"));
        assert!(rect("url")[3] > 30.0, "long URL wraps with overflow-wrap: anywhere");
        assert!(rect("hy")[3] > 30.0, "breaks after hyphens");
        let clamp = node_by_id(&p, "clamp");
        let cb = p.tree.boxes.iter().find(|b| b.node == clamp && !b.anon).unwrap();
        assert_eq!(cb.h, 40.0, "line-clamp: 2 lines");
        assert!(p.dl.extra.contains('\u{2026}'), "ellipsis emitted");
    }

    #[test]
    fn stacking_order() {
        let p = page_from(
            r#"<!doctype html><style>body { margin: 0 } div { box-sizing: border-box }
               #hdr { position: relative; height: 50px }
               #menu { position: absolute; top: 40px; left: 0; width: 100px; height: 100px; background: rgb(255, 0, 0); z-index: 10 }
               #later { position: relative; height: 200px; background: rgb(0, 0, 255) }
               #behind { position: absolute; left: 300px; top: 0; width: 50px; height: 50px; background: rgb(0, 255, 0); z-index: -1 }
               #flow { margin-left: 300px; width: 50px; height: 20px; background: rgb(9, 9, 9) }
               #clip { position: absolute; left: 500px; top: 0; width: 40px; height: 40px; overflow: hidden }
               #inner { position: relative; width: 100px; height: 100px; background: rgb(255, 0, 255) }
               #grp { position: absolute; left: 600px; top: 0; opacity: 0.5 }
               #a, #b { width: 40px; height: 40px; background: rgb(0, 0, 0) } #b { margin-top: -20px }</style>
               <div id=hdr><div id=menu></div></div><div id=later></div>
               <div id=behind></div>
               <div id=clip><div id=inner></div></div>
               <div id=grp><div id=a></div><div id=b></div></div>"#,
        );
        let (w, h) = (800, 400);
        let mut buf = vec![0u32; w * h];
        let mut glyphs = crate::fonts::GlyphCache::default();
        paint::paint(&p.dl, &p.tree.text, &mut glyphs, &mut buf, w, h, 0.0);
        let px = |x: usize, y: usize| buf[y * w + x] & 0xFFFFFF;
        assert_eq!(px(50, 100), 0xFF0000, "z-index dropdown above later positioned content");
        assert_eq!(px(320, 60), 0x0000FF, "later content beyond the dropdown");
        assert_eq!(px(560, 45), 0xFFFFFF, "overflow:hidden clips positioned children");
        assert_eq!(px(520, 20), 0xFF00FF);
        // Group opacity: the overlap of #a and #b is the same grey as the rest.
        assert_eq!(px(620, 10), px(620, 30));
    }

    #[test]
    fn transforms() {
        let p = page_from(
            r#"<!doctype html><style>body { margin: 0 }
               #modal { position: fixed; left: 50%; top: 50%; width: 200px; height: 100px; transform: translate(-50%, -50%); background: rgb(0, 0, 255) }
               #rot { position: absolute; left: 20px; top: 20px; width: 100px; height: 20px; background: rgb(255, 0, 0); transform: rotate(90deg) }
               #sc { position: absolute; left: 600px; top: 20px; width: 40px; height: 40px; background: rgb(0, 255, 0); scale: 2 }</style>
               <div id=modal></div><div id=rot></div><div id=sc></div>"#,
        );
        let r = p.tree.node_rects()[&node_by_id(&p, "modal")];
        assert_eq!(r, [300.0, 250.0, 200.0, 100.0]);
        assert_eq!(p.tree.node_at(400.0, 300.0), Some(node_by_id(&p, "modal")));
        let (w, h) = (800, 600);
        let mut buf = vec![0u32; w * h];
        let mut glyphs = crate::fonts::GlyphCache::default();
        paint::paint(&p.dl, &p.tree.text, &mut glyphs, &mut buf, w, h, 0.0);
        let px = |x: usize, y: usize| buf[y * w + x] & 0xFFFFFF;
        // Rotated 90° about its centre (70, 30): now spans x 60..80, y -20..80.
        assert_eq!(px(70, 70), 0xFF0000);
        assert_ne!(px(110, 30), 0xFF0000);
        // Scaled 2x about its centre (620, 40): spans 580..660.
        assert_eq!(px(585, 45), 0x00FF00);
        assert_eq!(px(400, 300), 0x0000FF);
    }

    #[test]
    fn percentage_heights() {
        let p = page_from(
            r#"<!doctype html><style>html, body { height: 100%; margin: 0 } #half { height: 50% }
               #inner { height: 25% } #auto > div { height: 50% } #abs { position: absolute; top: 10px; bottom: 90px; left: 0; width: 10px }
               #abs > div { height: 50% } #min { min-height: 10% }</style>
               <div id=half><div id=inner></div></div><div id=auto><div id=nope>x</div></div>
               <div id=abs><div id=absc></div></div><div id=min></div>"#,
        );
        let h = |id: &str| p.tree.node_rects()[&node_by_id(&p, id)][3];
        assert_eq!(h("half"), 300.0);
        assert_eq!(h("inner"), 75.0);
        // A percentage of an auto-height parent behaves as auto.
        assert!(h("nope") < 40.0);
        assert_eq!(h("abs"), 500.0);
        assert_eq!(h("absc"), 250.0);
        assert_eq!(h("min"), 60.0);
    }

    #[test]
    fn pseudo_elements() {
        let p = page_from(
            r#"<!doctype html><style>
                 .q::before { content: open-quote "Hi " attr(data-n) } .q::after { content: '\21' close-quote }
                 .box { position: relative; height: 50px } .box::after { content: ''; position: absolute; left: 0; top: 0; width: 20px; height: 10px; background: red }
                 .blk::before { content: 'B'; display: block } .icon::before { content: "\2713" }
               </style><p class=q data-n=Bob id=q>x</p><div class=box id=box></div><div class=blk>after</div><span class=icon></span>"#,
        );
        let text: String = p.tree.text.chars().collect();
        assert!(text.contains("\u{201c}Hi Bob") && text.contains("x!\u{201d}"), "{text}");
        assert!(text.contains('\u{2713}'), "{text}");
        // The absolutely positioned overlay is a 20x10 box owned by #box.
        let bx = node_by_id(&p, "box");
        let owned: Vec<_> = p.tree.boxes.iter().filter(|b| p.tree.pseudo_owner.get(&b.node) == Some(&bx)).collect();
        assert_eq!(owned.len(), 1);
        assert_eq!((owned[0].w, owned[0].h), (20.0, 10.0));
        // Hit-testing a pseudo box gives its owner.
        let r = p.tree.node_rects()[&node_by_id(&p, "q")];
        assert_eq!(p.tree.node_at(r[0] + 2.0, r[1] + r[3] / 2.0), Some(node_by_id(&p, "q")));
    }

    #[test]
    fn script_styles_apply() {
        let p = page_from(
            r#"<!doctype html><style id=s>#a { color: rgb(0, 0, 255) }</style>
               <div id=a>a</div><div id=b>b</div><div id=c>c</div><div id=d>d</div>
               <script>
                 const s = document.getElementById('s').sheet;
                 s.insertRule('#b { color: rgb(255, 0, 0) }', s.cssRules.length);
                 const st = document.createElement('style'); st.textContent = '#c { color: rgb(0, 128, 0) }';
                 document.head.appendChild(st);
                 const c = new CSSStyleSheet(); c.replaceSync('#d { color: rgb(1, 2, 3) }');
                 document.adoptedStyleSheets = [c];
                 document.title = s.cssRules.length + ' ' + document.styleSheets.length + ' ' + s.cssRules[1].selectorText;
               </script>"#,
        );
        let color = |id: &str| p.styles[node_by_id(&p, id) as usize].color & 0xFFFFFF;
        assert_eq!((color("a"), color("b"), color("c"), color("d")), (0x0000FF, 0xFF0000, 0x008000, 0x010203));
        assert_eq!(p.title(), "2 2 #b");
    }

    #[test]
    fn event_dispatch_semantics() {
        let p = page_from(
            r#"<!doctype html><div id=o><a id=l href="/next"><span id=i>x</span></a></div>
               <input type=checkbox id=c><input type=checkbox id=d>
               <script>
                 const log = [];
                 document.readyState === 'loading' && log.push('loading:' + (document.currentScript && document.currentScript.tagName));
                 const o = document.getElementById('o'), i = document.getElementById('i');
                 o.addEventListener('click', e => log.push('oc' + e.eventPhase), true);
                 o.addEventListener('click', e => log.push('ob' + e.eventPhase));
                 i.addEventListener('click', e => log.push('i1'), { once: true });
                 i.addEventListener('click', e => { log.push('i2'); e.stopImmediatePropagation(); });
                 i.addEventListener('click', e => log.push('i3'));
                 window.addEventListener('click', e => log.push('w' + e.composedPath().length), { capture: true });
                 i.click(); i.click();
                 const c = document.getElementById('c');
                 c.addEventListener('change', () => log.push('change' + c.checked));
                 c.click();
                 const d = document.getElementById('d');
                 d.addEventListener('click', e => e.preventDefault());
                 d.click();
                 log.push('d' + d.checked);
                 const ac = new AbortController();
                 o.addEventListener('x', () => log.push('aborted?'), { signal: ac.signal });
                 ac.abort();
                 o.dispatchEvent(new Event('x'));
                 document.addEventListener('readystatechange', () => log.push(document.readyState));
                 window.addEventListener('load', () => document.title = log.join(' '));
               </script>"#,
        );
        assert_eq!(
            p.title(),
            "loading:SCRIPT w7 oc1 i1 i2 w7 oc1 i2 w5 changetrue w5 dfalse interactive complete"
        );
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

#[allow(dead_code)]
fn _loaded_is_send() {
    fn is_send<T: Send>() {}
    is_send::<Loaded>();
}
