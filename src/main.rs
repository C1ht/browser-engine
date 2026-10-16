//! bolt — a small from-scratch browser engine focused on speed.
//!
//!   bolt [url|file]                       open a window
//!   bolt --bench [url|file] [--iters N]   benchmark the pipeline
//!   bolt --shot out.png [url|file]        render to a PNG (full page by default)
//!   common options: --width W --height H

#![allow(non_upper_case_globals)]

mod atom;
mod brotli;
mod brotli_tables;
mod css;
mod dom;
mod engine;
mod fonts;
mod fx;
mod html;
mod image;
mod js;
mod layout;
mod net;
mod woff2;
mod paint;
mod png;
mod style;

use engine::{ms, Page};
use fonts::{fonts, GlyphCache};
use minifb::{CursorStyle, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

const BAR: usize = 40;

struct Args {
    url: String,
    bench: bool,
    js: Option<String>,
    shot: Option<String>,
    dump: bool,
    boxes: bool,
    width: usize,
    height: usize,
    iters: usize,
}

fn parse_args() -> Args {
    let mut a = Args { url: "about:home".into(), bench: false, js: None, shot: None, dump: false, boxes: false, width: 1024, height: 0, iters: 30 };
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        match s.as_str() {
            "--bench" => a.bench = true,
            "--js" => a.js = it.next(),
            "--dump" => a.dump = true,
            "--boxes" => a.boxes = true,
            "--shot" => a.shot = it.next(),
            "--width" => a.width = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.width),
            "--height" => a.height = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--iters" => a.iters = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.iters),
            _ => a.url = net::normalize_input(&s),
        }
    }
    a
}

fn main() {
    // Deeply nested documents recurse in layout; give the engine a big stack.
    let h = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            fonts().prewarm();
            let args = parse_args();
            if let Some(path) = &args.js {
                // Run a JavaScript file (developer aid): prints console output and timing.
                let src = std::fs::read_to_string(path).unwrap_or_default();
                let mut it = js::Interp::new(dom::Document::new(), "about:blank", 800.0, 600.0);
                let t0 = Instant::now();
                let r = it.run_script(&src);
                it.run_microtasks();
                it.run_timers(1e12);
                if let Err(e) = r {
                    eprintln!("error: {e}");
                }
                eprintln!("[js] {:.1}ms, {} steps", ms(t0), it.steps);
            } else if args.boxes {
                match engine::load(&args.url) {
                    Ok(l) => {
                        let p = Page::new(l, args.width as f32, 768.0);
                        dump_boxes(&p, p.tree.root, 0);
                    }
                    Err(e) => eprintln!("load failed: {e}"),
                }
            } else if args.dump {
                match engine::load(&args.url) {
                    Ok(l) => dump(&l.doc, l.doc.html, 0),
                    Err(e) => eprintln!("load failed: {e}"),
                }
            } else if args.bench {
                bench(&args);
            } else if let Some(out) = &args.shot {
                screenshot(&args, out);
            } else {
                Browser::run(&args.url, args.width, if args.height == 0 { 768 } else { args.height });
            }
        })
        .unwrap();
    let _ = h.join();
}

/// Print the DOM tree (for debugging).
fn dump(doc: &dom::Document, n: dom::NodeId, depth: usize) {
    match &doc.nodes[n as usize].data {
        dom::Data::Element(e) => {
            let mut line = format!("{}<{}", "  ".repeat(depth), atom::name(e.tag));
            if let Some(id) = e.id {
                line += &format!(" #{}", atom::name(id));
            }
            for c in &e.classes {
                line += &format!(" .{}", atom::name(*c));
            }
            println!("{line}>");
            for c in doc.children(n) {
                dump(doc, c, depth + 1);
            }
        }
        dom::Data::Text(t) => {
            let t = t.trim();
            if !t.is_empty() {
                let short: String = t.chars().take(50).collect();
                println!("{}\"{}\"", "  ".repeat(depth), short);
            }
        }
        dom::Data::Document => {}
    }
}

/// Print the layout box tree (for debugging).
fn dump_boxes(p: &Page, id: u32, depth: usize) {
    if depth > 14 {
        return;
    }
    let b = &p.tree.boxes[id as usize];
    let tag = p.doc.tag(b.node).map_or("#", atom::name);
    println!(
        "{}{:?}{} <{}> x={:.0} y={:.0} w={:.0} h={:.0} frags={}",
        "  ".repeat(depth), b.kind, if b.anon { "(anon)" } else { "" }, tag, b.x, b.y, b.w, b.h, b.frags.len()
    );
    for &c in &b.children {
        dump_boxes(p, c, depth + 1);
    }
    for f in &b.frags {
        if let layout::FragKind::Atomic(a) = f.kind {
            dump_boxes(p, a, depth + 1);
        }
    }
}

// ----------------------------------------------------------------- bench

fn bench(a: &Args) {
    let l = match engine::load(&a.url) {
        Ok(l) => l,
        Err(e) => return eprintln!("load failed: {e}"),
    };
    let (w, h) = (a.width, if a.height == 0 { 768 } else { a.height });
    println!("bench {}  ({} bytes html, {} stylesheets, fetch {:.1}ms)", l.url, l.html.len(), l.sheets.len(), l.fetch_ms);
    let (html, sheets, url) = (l.html.clone(), l.sheets.clone(), l.url.clone());
    // Load images once so layout and paint include them.
    let mut page = Page::new(l, w as f32, h as f32);
    page.load_fonts_blocking(w as f32, h as f32);
    page.load_images_blocking(w as f32, h as f32);
    let (res, base) = (page.res, page.base);
    let _ = url;
    let mut glyphs = GlyphCache::default();
    let mut buf = vec![0u32; w * h];
    let mut runs: Vec<[f64; 7]> = Vec::new();
    let mut last = engine::Timings::default();
    let (mut items, mut height) = (0, 0.0);
    for i in 0..a.iters.max(1) + 1 {
        let total = Instant::now();
        let t0 = Instant::now();
        let doc = html::parse(&html);
        let parse = ms(t0);
        let mut t = engine::Timings::default();
        let (_styles, tree, dl) = engine::render(&doc, &sheets, &base, &res, w as f32, h as f32, &mut t);
        let t0 = Instant::now();
        paint::paint(&dl, &tree.text, &mut glyphs, &mut buf, w, h, 0.0);
        let paint = ms(t0);
        let tot = ms(total);
        if i > 0 {
            // iteration 0 warms font/glyph caches
            runs.push([parse, t.css, t.style, t.layout, t.display, paint, tot]);
        }
        last = t;
        items = dl.items.len();
        height = tree.height;
    }
    let names = ["parse", "css", "style", "layout", "display list", "paint (1 frame)", "TOTAL"];
    println!("{} nodes, {} rules, {} boxes, {} display items, page height {:.0}px",
        last.nodes, last.rules, last.boxes, items, height);
    println!("{:<16} {:>9} {:>9}", "phase", "median", "min");
    for (i, n) in names.iter().enumerate() {
        let mut v: Vec<f64> = runs.iter().map(|r| r[i]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{:<16} {:>7.2}ms {:>7.2}ms", n, v[v.len() / 2], v[0]);
    }
}

// ------------------------------------------------------------ screenshot

fn screenshot(a: &Args, out: &str) {
    let l = match engine::load(&a.url) {
        Ok(l) => l,
        Err(e) => return eprintln!("load failed: {e}"),
    };
    let w = a.width;
    let vh = if a.height == 0 { 768.0 } else { a.height as f32 };
    let fetch_ms = l.fetch_ms;
    let t_page = Instant::now();
    let mut page = Page::new(l, w as f32, vh);
    let page_ms = ms(t_page);
    let t_fonts = Instant::now();
    page.load_fonts_blocking(w as f32, vh);
    let fonts_ms = ms(t_fonts);
    let t_imgs = Instant::now();
    page.load_images_blocking(w as f32, vh);
    eprintln!("[bolt] fetch {fetch_ms:.0}ms | page {page_ms:.0}ms (scripts {:.0}ms) | fonts {fonts_ms:.0}ms | images {:.0}ms", page.script_ms, ms(t_imgs));
    let h = if a.height == 0 { (page.tree.height.ceil() as usize).clamp(1, 16000) } else { a.height };
    let mut buf = vec![0u32; w * h];
    let mut glyphs = GlyphCache::default();
    let t0 = Instant::now();
    paint::paint(&page.dl, &page.tree.text, &mut glyphs, &mut buf, w, h, 0.0);
    let mut t = page.t;
    t.paint = ms(t0);
    std::fs::write(out, png::encode(&buf, w, h)).expect("write png");
    println!("{} -> {out} ({w}x{h})\n{}", page.url, t.summary());
}

// --------------------------------------------------------------- browser

struct CharSink(Rc<RefCell<Vec<u32>>>);

impl minifb::InputCallback for CharSink {
    fn add_char(&mut self, c: u32) {
        self.0.borrow_mut().push(c);
    }
}

struct Browser {
    window: Window,
    page: Option<Page>,
    error: Option<String>,
    scroll: f32,
    url_input: String,
    editing: bool,
    select_all: bool,
    history: Vec<(String, f32)>,
    forward: Vec<(String, f32)>,
    glyphs: GlyphCache,
    buf: Vec<u32>,
    w: usize,
    h: usize,
    hover: Option<String>,
    chars: Rc<RefCell<Vec<u32>>>,
    was_down: bool,
    /// Decoded images arriving from the background loader, tagged with the
    /// page generation they were requested for.
    img_rx: std::sync::mpsc::Receiver<(u64, String, Option<std::sync::Arc<image::Image>>)>,
    img_tx: std::sync::mpsc::Sender<(u64, String, Option<std::sync::Arc<image::Image>>)>,
    generation: u64,
    loaded_at: Instant,
}

impl Browser {
    fn run(url: &str, w: usize, h: usize) {
        let mut window = Window::new(
            "bolt",
            w,
            h,
            WindowOptions { resize: true, ..WindowOptions::default() },
        )
        .expect("create window");
        window.set_target_fps(120);
        let chars = Rc::new(RefCell::new(Vec::new()));
        window.set_input_callback(Box::new(CharSink(chars.clone())));
        let (img_tx, img_rx) = std::sync::mpsc::channel();
        let mut b = Browser {
            window,
            page: None,
            error: None,
            scroll: 0.0,
            url_input: String::new(),
            editing: false,
            select_all: false,
            history: Vec::new(),
            forward: Vec::new(),
            glyphs: GlyphCache::default(),
            buf: vec![0; w * h],
            w,
            h,
            hover: None,
            chars,
            was_down: false,
            img_rx,
            img_tx,
            generation: 0,
            loaded_at: Instant::now(),
        };
        b.navigate(url, true);
        b.event_loop();
    }

    fn vh(&self) -> usize {
        self.h.saturating_sub(BAR).max(1)
    }

    fn navigate(&mut self, url: &str, push_history: bool) {
        let url = url.to_owned();
        // Same-document fragment navigation.
        if let (Some(p), Some(hash)) = (&self.page, url.find('#')) {
            let cur = p.url.split('#').next().unwrap_or("");
            if &url[..hash] == cur {
                let frag = &url[hash + 1..];
                self.scroll = if frag.is_empty() || frag == "top" { 0.0 } else { self.find_anchor(frag).unwrap_or(self.scroll) };
                self.repaint();
                return;
            }
        }
        if push_history {
            if let Some(p) = &self.page {
                self.history.push((p.url.clone(), self.scroll));
                self.forward.clear();
            }
        }
        self.url_input = url.clone();
        self.editing = false;
        self.window.set_title(&format!("Loading {url} — bolt"));
        self.draw_chrome(true);
        let _ = self.window.update_with_buffer(&self.buf, self.w, self.h);

        match engine::load(&url) {
            Ok(l) => {
                let page = Page::new(l, self.w as f32, self.vh() as f32);
                self.url_input = page.url.clone();
                self.generation += 1;
                self.loaded_at = Instant::now();
                let mut page = page;
                self.request_images(&mut page);
                self.page = Some(page);
                self.error = None;
            }
            Err(e) => {
                self.page = None;
                self.error = Some(e);
            }
        }
        self.scroll = 0.0;
        self.repaint();
        if let Some(p) = &self.page {
            let title = if p.title().is_empty() { p.url.as_str() } else { p.title() };
            self.window.set_title(&format!("{title} — bolt"));
            eprintln!("[bolt] {}\n        fetch {:.1}ms | scripts {:.1}ms | {}", p.url, p.t.fetch, p.script_ms, p.t.summary());
        }
    }

    /// Fetch the page's images on a background thread.
    fn request_images(&self, page: &mut Page) {
        let urls = page.wanted_images();
        let fonts = page.wanted_fonts();
        if urls.is_empty() && fonts.is_empty() {
            return;
        }
        for u in &urls {
            page.res.images.insert(u.clone(), None); // pending
        }
        let (tx, generation) = (self.img_tx.clone(), self.generation);
        std::thread::spawn(move || {
            if !fonts.is_empty() {
                // Fonts change text metrics: an empty URL asks for a relayout.
                engine::fetch_fonts(&fonts);
                let _ = tx.send((generation, String::new(), None));
            }
            engine::fetch_images(&urls, &|u, img| {
                let _ = tx.send((generation, u, img));
            });
        });
    }

    /// Apply images that finished loading; returns true if the page changed.
    fn poll_images(&mut self) -> bool {
        let mut changed = false;
        while let Ok((generation, u, img)) = self.img_rx.try_recv() {
            if generation != self.generation {
                continue;
            }
            if let Some(p) = &mut self.page {
                if !u.is_empty() {
                    p.res.images.insert(u, img);
                }
                changed = true;
            }
        }
        if changed {
            let (w, vh) = (self.w as f32, self.vh() as f32);
            if let Some(p) = &mut self.page {
                p.relayout(w, vh);
                // Newly visible elements (e.g. via media queries) may want more images.
                let urls = p.wanted_images();
                if !urls.is_empty() {
                    let (tx, generation) = (self.img_tx.clone(), self.generation);
                    for u in &urls {
                        p.res.images.insert(u.clone(), None);
                    }
                    std::thread::spawn(move || {
                        engine::fetch_images(&urls, &|u, img| {
                            let _ = tx.send((generation, u, img));
                        });
                    });
                }
            }
        }
        changed
    }

    fn find_anchor(&self, frag: &str) -> Option<f32> {
        let p = self.page.as_ref()?;
        let atom = atom::lookup(frag)?;
        let node = (0..p.doc.nodes.len()).find(|&i| p.doc.elem(i as u32).is_some_and(|e| e.id == Some(atom)))? as u32;
        // Depth-first walk to find the first box generated by the node or a descendant.
        fn walk(t: &layout::LayoutTree, id: u32, ox: f32, oy: f32, node: u32) -> Option<f32> {
            let b = &t.boxes[id as usize];
            let (bx, by) = (ox + b.x, oy + b.y);
            if b.node >= node && !b.anon {
                return Some(by);
            }
            let (cx, cy) = (bx + b.b[3] + b.p[3], by + b.b[0] + b.p[0]);
            for &c in &b.children {
                if let Some(y) = walk(t, c, cx, cy, node) {
                    return Some(y);
                }
            }
            None
        }
        walk(&p.tree, p.tree.root, 0.0, 0.0, node)
    }

    fn max_scroll(&self) -> f32 {
        self.page.as_ref().map_or(0.0, |p| (p.tree.height - self.vh() as f32).max(0.0))
    }

    fn repaint(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
        let (w, vh) = (self.w, self.vh());
        let t0 = Instant::now();
        if let Some(p) = &self.page {
            paint::paint(&p.dl, &p.tree.text, &mut self.glyphs, &mut self.buf[BAR * w..], w, vh, self.scroll.round());
        } else {
            self.buf[BAR * w..].fill(0xFFFF_FFFF);
            if let Some(e) = self.error.clone() {
                self.draw_text(&format!("Could not load page: {e}"), 20.0, BAR as f32 + 40.0, 16.0, 0xFFB0_0020);
            }
        }
        if let Some(p) = &mut self.page {
            p.t.paint = ms(t0);
        }
        self.draw_chrome(false);
    }

    fn draw_text(&mut self, s: &str, x: f32, y: f32, size: f32, color: u32) {
        let f = fonts();
        let face = f.pick(style::Family::Sans, false, false);
        for c in s.chars() {
            let a = f.adv(face, c);
            self.glyphs.ensure(f, a.face, c, size);
        }
        let (w, h) = (self.w, self.h);
        let mut s_ = paint::Surface::new(&mut self.buf, w, h, 0.0, 0.0);
        s_.text(s, x, y, face, size, color, f, &self.glyphs);
    }

    fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, c: u32) {
        let (bw, bh) = (self.w, self.h);
        paint::Surface::new(&mut self.buf, bw, bh, 0.0, 0.0).rect(x, y, w, h, c);
    }

    fn draw_chrome(&mut self, loading: bool) {
        let w = self.w as f32;
        self.fill(0.0, 0.0, w, BAR as f32, 0xFFEC_EEF2);
        self.fill(0.0, BAR as f32 - 1.0, w, 1.0, 0xFFC8_CCD4);
        // back button
        let back_col = if self.history.is_empty() { 0xFFB0_B4BC } else { 0xFF30_3440 };
        self.draw_text("\u{2190}", 12.0, 26.0, 18.0, back_col);
        // address field
        let border = if self.editing { 0xFF1A_56DB } else { 0xFFC8_CCD4 };
        self.fill(40.0, 7.0, w - 48.0, 26.0, border);
        self.fill(41.0, 8.0, w - 50.0, 24.0, 0xFFFF_FFFF);
        if self.editing && self.select_all {
            let tw = fonts().measure(0, 14.0, &self.url_input);
            self.fill(49.0, 11.0, tw + 2.0, 18.0, 0xFFC7_DBFF);
        }
        let shown = self.url_input.clone();
        self.draw_text(&shown, 50.0, 25.0, 14.0, 0xFF1D_2433);
        if self.editing {
            let tw = fonts().measure(0, 14.0, &shown);
            self.fill(51.0 + tw, 11.0, 1.0, 18.0, 0xFF1D_2433);
        }
        if loading {
            self.fill(41.0, 30.0, (w - 50.0) * 0.6, 2.0, 0xFF1A_56DB);
        }
        // status bubble for hovered links
        if let Some(hv) = self.hover.clone() {
            let tw = fonts().measure(0, 12.0, &hv).min(w - 20.0);
            let y = self.h as f32 - 22.0;
            self.fill(0.0, y, tw + 14.0, 22.0, 0xFFEC_EEF2);
            self.fill(0.0, y, tw + 14.0, 1.0, 0xFFC8_CCD4);
            self.draw_text(&hv, 7.0, y + 15.0, 12.0, 0xFF30_3440);
        }
    }

    fn go_back(&mut self) {
        if let Some((u, s)) = self.history.pop() {
            if let Some(p) = &self.page {
                self.forward.push((p.url.clone(), self.scroll));
            }
            self.navigate(&u, false);
            self.scroll = s;
            self.repaint();
        }
    }

    fn go_forward(&mut self) {
        if let Some((u, s)) = self.forward.pop() {
            if let Some(p) = &self.page {
                self.history.push((p.url.clone(), self.scroll));
            }
            self.navigate(&u, false);
            self.scroll = s;
            self.repaint();
        }
    }

    fn event_loop(&mut self) {
        while self.window.is_open() {
            let mut dirty = false;
            if self.poll_images() {
                dirty = true;
            }
            // JavaScript timers.
            let now = self.loaded_at.elapsed().as_secs_f64() * 1000.0;
            if self.page.as_mut().is_some_and(|p| p.tick(now)) {
                dirty = true;
            }
            let (w, h) = self.window.get_size();
            if (w, h) != (self.w, self.h) && w > 0 && h > BAR {
                let width_changed = w != self.w;
                self.w = w;
                self.h = h;
                self.buf = vec![0; w * h];
                if width_changed {
                    let vh = self.vh() as f32;
                    if let Some(p) = &mut self.page {
                        p.relayout(w as f32, vh);
                    }
                }
                dirty = true;
            }

            let ctrl = self.window.is_key_down(Key::LeftCtrl) || self.window.is_key_down(Key::RightCtrl);
            let alt = self.window.is_key_down(Key::LeftAlt) || self.window.is_key_down(Key::RightAlt);
            let typed: Vec<u32> = std::mem::take(&mut *self.chars.borrow_mut());

            if self.editing {
                for c in typed {
                    if let Some(ch) = char::from_u32(c) {
                        if !ch.is_control() {
                            if self.select_all {
                                self.url_input.clear();
                                self.select_all = false;
                            }
                            self.url_input.push(ch);
                            dirty = true;
                        }
                    }
                }
                if self.window.is_key_pressed(Key::Backspace, KeyRepeat::Yes) {
                    if self.select_all {
                        self.url_input.clear();
                        self.select_all = false;
                    } else {
                        self.url_input.pop();
                    }
                    dirty = true;
                }
                if self.window.is_key_pressed(Key::Enter, KeyRepeat::No) {
                    let target = net::normalize_input(&self.url_input);
                    self.navigate(&target, true);
                    continue;
                }
                if self.window.is_key_pressed(Key::Escape, KeyRepeat::No) {
                    self.editing = false;
                    self.url_input = self.page.as_ref().map(|p| p.url.clone()).unwrap_or_default();
                    dirty = true;
                }
            } else {
                let vh = self.vh() as f32;
                let step = |k: Key| -> Option<f32> {
                    Some(match k {
                        Key::Down => 40.0,
                        Key::Up => -40.0,
                        Key::PageDown | Key::Space => vh * 0.9,
                        Key::PageUp => -vh * 0.9,
                        Key::Home => -1e9,
                        Key::End => 1e9,
                        _ => return None,
                    })
                };
                for k in self.window.get_keys_pressed(KeyRepeat::Yes) {
                    if let Some(d) = step(k) {
                        self.scroll += d;
                        dirty = true;
                    }
                }
                if (ctrl && self.window.is_key_pressed(Key::L, KeyRepeat::No)) || self.window.is_key_pressed(Key::F6, KeyRepeat::No) {
                    self.editing = true;
                    self.select_all = true;
                    dirty = true;
                }
                if self.window.is_key_pressed(Key::Backspace, KeyRepeat::No) || (alt && self.window.is_key_pressed(Key::Left, KeyRepeat::No)) {
                    self.go_back();
                    continue;
                }
                if alt && self.window.is_key_pressed(Key::Right, KeyRepeat::No) {
                    self.go_forward();
                    continue;
                }
                if self.window.is_key_pressed(Key::F5, KeyRepeat::No) {
                    if let Some(u) = self.page.as_ref().map(|p| p.url.clone()) {
                        let s = self.scroll;
                        self.navigate(&u, false);
                        self.scroll = s;
                        self.repaint();
                    }
                    continue;
                }
                if self.window.is_key_pressed(Key::F12, KeyRepeat::No) {
                    if let Some(p) = &self.page {
                        eprintln!("[bolt] {}", p.t.summary());
                    }
                }
            }

            if let Some((_, dy)) = self.window.get_scroll_wheel() {
                self.scroll -= dy * 5.0;
                dirty = true;
            }

            // Hover + click.
            let mouse = self.window.get_mouse_pos(MouseMode::Discard);
            let down = self.window.get_mouse_down(MouseButton::Left);
            let clicked = down && !self.was_down;
            self.was_down = down;
            let mut hover = None;
            if let (Some((mx, my)), Some(p)) = (mouse, &self.page) {
                if my as usize >= BAR {
                    if let Some(l) = p.dl.link_at(mx, my - BAR as f32, self.scroll.round()) {
                        hover = p.link_href(l);
                    }
                }
            }
            if hover != self.hover {
                self.window.set_cursor_style(if hover.is_some() { CursorStyle::ClosedHand } else { CursorStyle::Arrow });
                self.hover = hover.clone();
                dirty = true;
            }
            if clicked {
                if let Some((mx, my)) = mouse {
                    if (my as usize) < BAR {
                        if mx < 36.0 {
                            self.go_back();
                            continue;
                        }
                        self.editing = true;
                        self.select_all = true;
                        dirty = true;
                    } else {
                        // Scripts see the click first; preventDefault() stops navigation.
                        let py = my - BAR as f32 + self.scroll.round();
                        let prevented = self.page.as_mut().is_some_and(|p| p.click(mx, py));
                        if prevented {
                            dirty = true;
                        } else if let Some(target) = hover {
                            self.hover = None;
                            if !target.starts_with("javascript:") {
                                self.navigate(&target, true);
                                continue;
                            }
                        }
                        // The page may have changed (handlers ran).
                        dirty = true;
                    }
                    if self.editing && (my as usize) >= BAR {
                        self.editing = false;
                        dirty = true;
                    }
                }
            }

            if dirty {
                self.repaint();
                let _ = self.window.update_with_buffer(&self.buf, self.w, self.h);
            } else {
                self.window.update();
            }
        }
    }
}
