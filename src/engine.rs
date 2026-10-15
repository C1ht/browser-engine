//! The rendering pipeline: load → parse → cascade → layout → display list,
//! plus image loading (fetched in parallel after the first render).

use crate::atom::k;
use crate::css::{self, Media};
use crate::dom::{Data, Document, NodeId};
use crate::fx::FxHashMap;
use crate::image::{self, Image};
use crate::layout::{self, LayoutTree};
use crate::paint::{self, DisplayList};
use crate::style::{self, Display, Style};
use crate::{html, net};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

pub const HOME: &str = include_str!("../pages/home.html");
pub const FEATURES: &str = include_str!("../pages/features.html");

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
    pub fetch_ms: f64,
    pub parse_ms: f64,
}

fn base_of(doc: &Document, url: &str) -> String {
    doc.base.as_ref().map(|b| net::resolve(url, b)).unwrap_or_else(|| url.to_owned())
}

/// Fetch a document and its stylesheets (in parallel).
pub fn load(url: &str) -> Result<Loaded, String> {
    let t = Instant::now();
    let (url, html) = match url {
        "about:home" | "about:" => (url.to_owned(), HOME.to_owned()),
        "about:blank" => (url.to_owned(), String::new()),
        "about:features" => (url.to_owned(), FEATURES.to_owned()),
        "about:stress" => (url.to_owned(), stress_page()),
        _ => {
            let (u, bytes) = net::fetch(url)?;
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
    let mut sheets: Vec<(String, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = urls
            .iter()
            .map(|u| s.spawn(move || (net::fetch(u).map(|(_, b)| net::decode(&b)).unwrap_or_default(), u.clone())))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    sheets.extend(doc.styles.iter().map(|s| (s.clone(), base.clone())));
    Ok(Loaded { url, html, sheets, doc, fetch_ms: fetch_html + ms(t), parse_ms })
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

/// Download and register web fonts in parallel.
pub fn fetch_fonts(faces: &[css::FontFace]) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..faces.len().min(6) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(f) = faces.get(i) else { break };
                let bytes = net::fetch(&f.url).ok().map(|(_, b)| b);
                crate::fonts::fonts().add_web(&f.family, &f.url, f.weight, f.italic, bytes);
            });
        }
    });
}

/// Fetch and decode images in parallel; `done` is called as each finishes.
pub fn fetch_images(urls: &[String], done: &(dyn Fn(String, Option<Arc<Image>>) + Sync)) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..urls.len().min(8) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(u) = urls.get(i) else { break };
                let bytes = if u.starts_with("data:") { image::data_url(u) } else { net::fetch(u).ok().map(|(_, b)| b) };
                let img = bytes.and_then(|b| image::decode(&b)).map(Arc::new);
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
    pub t: Timings,
}

#[allow(clippy::too_many_arguments)]
pub fn render(doc: &Document, sheets: &[(String, String)], base: &str, res: &Resources, vw: f32, vh: f32, t: &mut Timings) -> (Vec<Style>, LayoutTree, DisplayList) {
    let t0 = Instant::now();
    let cas = style::build_cascade(sheets, &Media { vw, vh }, doc.quirks);
    t.css = ms(t0);
    let t0 = Instant::now();
    css::set_base(base);
    let styles = style::compute(doc, &cas, vw, vh);
    t.style = ms(t0);
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

impl Page {
    pub fn new(l: Loaded, vw: f32, vh: f32) -> Page {
        let mut t = Timings { fetch: l.fetch_ms, parse: l.parse_ms, ..Default::default() };
        let base = base_of(&l.doc, &l.url);
        let mut res = Resources::default();
        for (i, n) in l.doc.nodes.iter().enumerate() {
            if let Data::Element(e) = &n.data {
                if e.tag == k::img {
                    if let Some(u) = img_url(&l.doc, i as NodeId, &base) {
                        res.img_src.insert(i as NodeId, u);
                    }
                }
            }
        }
        let (styles, tree, dl) = render(&l.doc, &l.sheets, &base, &res, vw, vh, &mut t);
        Page { url: l.url, base, doc: l.doc, sheets: l.sheets, styles, tree, dl, res, t }
    }

    /// Re-run style + layout for a new viewport size (media queries may change).
    pub fn relayout(&mut self, vw: f32, vh: f32) {
        let (styles, tree, dl) = render(&self.doc, &self.sheets, &self.base, &self.res, vw, vh, &mut self.t);
        self.styles = styles;
        self.tree = tree;
        self.dl = dl;
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
