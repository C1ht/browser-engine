//! The rendering pipeline: load → parse → cascade → layout → display list.

use crate::css::Media;
use crate::dom::Document;
use crate::layout::{self, LayoutTree};
use crate::paint::{self, DisplayList};
use crate::style::{self, Style};
use crate::{html, net};
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

pub struct Loaded {
    pub url: String,
    pub html: String,
    pub sheets: Vec<String>,
    pub doc: Document,
    pub fetch_ms: f64,
    pub parse_ms: f64,
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
    let base = doc.base.as_ref().map(|b| net::resolve(&url, b)).unwrap_or_else(|| url.clone());
    let urls: Vec<String> = doc.stylesheet_links.iter().map(|h| net::resolve(&base, h)).collect();
    let mut sheets: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = urls
            .iter()
            .map(|u| s.spawn(move || net::fetch(u).map(|(_, b)| net::decode(&b)).unwrap_or_default()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    sheets.extend(doc.styles.iter().cloned());
    Ok(Loaded { url, html, sheets, doc, fetch_ms: fetch_html + ms(t), parse_ms })
}

pub struct Page {
    pub url: String,
    pub doc: Document,
    pub sheets: Vec<String>,
    pub styles: Vec<Style>,
    pub tree: LayoutTree,
    pub dl: DisplayList,
    pub t: Timings,
}

pub fn render(doc: &Document, sheets: &[String], vw: f32, vh: f32, t: &mut Timings) -> (Vec<Style>, LayoutTree, DisplayList) {
    let t0 = Instant::now();
    let cas = style::build_cascade(sheets, &Media { vw, vh }, doc.quirks);
    t.css = ms(t0);
    let t0 = Instant::now();
    let styles = style::compute(doc, &cas, vw, vh);
    t.style = ms(t0);
    let t0 = Instant::now();
    let tree = layout::layout(doc, &styles, vw, vh);
    t.layout = ms(t0);
    let t0 = Instant::now();
    let dl = paint::build(&tree, doc, &styles);
    t.display = ms(t0);
    t.nodes = doc.nodes.len();
    t.rules = cas.rule_count();
    t.boxes = tree.boxes.len();
    (styles, tree, dl)
}

impl Page {
    pub fn new(l: Loaded, vw: f32, vh: f32) -> Page {
        let mut t = Timings { fetch: l.fetch_ms, parse: l.parse_ms, ..Default::default() };
        let (styles, tree, dl) = render(&l.doc, &l.sheets, vw, vh, &mut t);
        Page { url: l.url, doc: l.doc, sheets: l.sheets, styles, tree, dl, t }
    }

    /// Re-run style + layout for a new viewport size (media queries may change).
    pub fn relayout(&mut self, vw: f32, vh: f32) {
        let (styles, tree, dl) = render(&self.doc, &self.sheets, vw, vh, &mut self.t);
        self.styles = styles;
        self.tree = tree;
        self.dl = dl;
    }

    pub fn title(&self) -> &str {
        &self.doc.title
    }

    pub fn link_href(&self, link: u32) -> Option<String> {
        let href = self.tree.links.get(link as usize)?;
        let base = self.doc.base.as_ref().map(|b| net::resolve(&self.url, b)).unwrap_or_else(|| self.url.clone());
        Some(net::resolve(&base, href))
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
