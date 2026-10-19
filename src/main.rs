//! bolt — a small from-scratch browser engine focused on speed.
//!
//!   bolt [url|file]                       open a window
//!   bolt --bench [url|file] [--iters N]   benchmark the pipeline
//!   bolt --shot out.png [url|file]        render to a PNG (full page by default)
//!   common options: --width W --height H

#![allow(non_upper_case_globals)]

mod sampler;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod atom;
mod av1;
mod avif;
mod av1_tables;
mod brotli;
mod brotli_tables;
mod css;
mod dom;
mod engine;
mod entities;
mod filter;
mod fonts;
mod forms;
mod fx;
mod html;
mod image;
mod js;
mod layout;
mod net;
mod webp;
mod woff2;
mod paint;
mod png;
mod style;
mod svg;
mod vp8_tables;

use engine::{ms, Click, Edit, Nav, Page};
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
    /// Screenshot mode: focus the first text field and type this.
    typed: Option<String>,
    /// Screenshot mode: scripts to run on the loaded page (results printed).
    eval: Vec<String>,
    /// Screenshot mode: print what's under these points.
    probe: Vec<(f32, f32)>,
    /// Screenshot mode: click these points (in order) before the shot.
    clicks: Vec<(f32, f32)>,
}

fn parse_args() -> Args {
    let mut a = Args { url: "about:home".into(), bench: false, js: None, shot: None, dump: false, boxes: false, width: 1024, height: 0, iters: 30, typed: None, eval: Vec::new(), probe: Vec::new(), clicks: Vec::new() };
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
            "--type" => a.typed = it.next(),
            "--eval" => a.eval.extend(it.next()),
            "--probe" => {
                if let Some((x, y)) = it.next().as_deref().and_then(|v| v.split_once(',')) {
                    a.probe.push((x.trim().parse().unwrap_or(0.0), y.trim().parse().unwrap_or(0.0)));
                }
            }
            "--click" => {
                if let Some((x, y)) = it.next().as_deref().and_then(|v| v.split_once(',')) {
                    a.clicks.push((x.trim().parse().unwrap_or(0.0), y.trim().parse().unwrap_or(0.0)));
                }
            }
            "--iters" => a.iters = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.iters),
            _ => a.url = net::normalize_input(&s),
        }
    }
    a
}

/// `BOLT_CRASH_TRACE=1`: on a crash (access violation, stack overflow),
/// print the exception and a backtrace before dying.
#[cfg(windows)]
fn install_crash_trace() {
    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        next: *mut ExceptionRecord,
        address: *mut std::ffi::c_void,
        nparams: u32,
        info: [usize; 15],
    }
    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut std::ffi::c_void,
    }
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32) -> *mut std::ffi::c_void;
        fn SetThreadStackGuarantee(size: *mut u32) -> i32;
    }
    unsafe extern "system" fn handler(p: *mut ExceptionPointers) -> i32 {
        let rec = unsafe { &*(*p).record };
        // Only fatal faults (not C++/debugger exceptions).
        if matches!(rec.code, 0xC000_0005 | 0xC000_00FD | 0xC000_001D | 0xC000_0096) {
            eprintln!("[crash] exception {:#x} at {:p} (info {:#x} {:#x})", rec.code, rec.address, rec.info[0], rec.info[1]);
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }
    if std::env::var_os("BOLT_CRASH_TRACE").is_some() {
        unsafe {
            let mut g: u32 = 256 << 10;
            SetThreadStackGuarantee(&mut g);
            AddVectoredExceptionHandler(1, handler);
        }
    }
}

fn main() {
    #[cfg(windows)]
    install_crash_trace();
    // Deeply nested documents recurse in layout; give the engine a big stack.
    let h = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            sampler::start();
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
            sampler::report();
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
        dom::Data::Comment(t) => println!("{}<!--{}-->", "  ".repeat(depth), t.chars().take(50).collect::<String>()),
        _ => {}
    }
}

/// Print the layout box tree (for debugging).
fn dump_boxes(p: &Page, id: u32, depth: usize) {
    if depth > 80 {
        return;
    }
    let b = &p.tree.boxes[id as usize];
    let tag = p.doc.tag(b.node).map_or("#", atom::name);
    let class = p.doc.elem(b.node).and_then(|e| e.attr(atom::k::class)).map(|c| format!(".{}", c.split_whitespace().collect::<Vec<_>>().join("."))).unwrap_or_default();
    println!(
        "{}{:?}{} <{}{}> n={} x={:.0} y={:.0} w={:.0} h={:.0} frags={}",
        "  ".repeat(depth), b.kind, if b.anon { "(anon)" } else { "" }, tag, class, b.node, b.x, b.y, b.w, b.h, b.frags.len()
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
    println!("bench {}  ({} bytes html, {} stylesheets, fetch {:.1}ms)", l.url, l.html.len(), l.css.len(), l.fetch_ms);
    let (html, url) = (l.html.clone(), l.url.clone());
    // Load images once so layout and paint include them.
    let mut page = Page::new(l, w as f32, h as f32);
    page.load_fonts_blocking(w as f32, h as f32);
    page.load_images_blocking(w as f32, h as f32);
    let sheets = engine::sheets_for(&page.doc, &page.base, &mut page.css);
    let (mut res, base) = (page.res, page.base);
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
        let (_styles, tree, dl) = engine::render(&doc, &sheets, &base, &mut res, w as f32, h as f32, &mut t);
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

/// Wait (in real time, bounded) for iframe documents that are loading,
/// then let them run briefly.
fn settle_frames(page: &mut Page, now: &mut f64) {
    let t = Instant::now();
    let mut extra = 0;
    while ms(t) < 10000.0 {
        let loading = page.frames_loading();
        if !loading {
            extra += 1;
            if extra > 60 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(if loading { 16 } else { 2 }));
        *now += 16.0;
        page.tick(*now);
    }
}

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
    // Let timers and network responses from the first seconds play out, as
    // they would in the window (simulated clock, bounded wall time).
    let t_settle = Instant::now();
    let mut now = 0.0;
    // BOLT_SETTLE_MS: how much page time to simulate (default 3000).
    let settle: f64 = std::env::var("BOLT_SETTLE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000.0);
    while (now < settle || page.frames_loading()) && ms(t_settle) < 8000.0f64.max(settle * 3.0) {
        now += 16.0;
        // Frames load in real time: keep pace with them.
        if page.frames_loading() {
            std::thread::sleep(std::time::Duration::from_millis(16));
        } else if page.has_pending_requests() {
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        page.tick(now);
        if !page.has_pending_work(now) && now > 500.0 {
            break;
        }
    }
    // BOLT_SCROLL_THROUGH: scroll down the page a viewport at a time (as a
    // reader would), so scroll-triggered content gets revealed.
    if std::env::var_os("BOLT_SCROLL_THROUGH").is_some() {
        let mut y = 0.0;
        while y < page.tree.height {
            y += vh * 0.8;
            page.set_scroll(y);
            for _ in 0..10 {
                now += 16.0;
                page.tick(now);
            }
        }
        page.set_scroll(0.0);
        for _ in 0..10 {
            now += 16.0;
            page.tick(now);
        }
    }
    let t_fonts = Instant::now();
    page.load_fonts_blocking(w as f32, vh);
    let fonts_ms = ms(t_fonts);
    let t_imgs = Instant::now();
    page.load_images_blocking(w as f32, vh);
    settle_frames(&mut page, &mut now);
    eprintln!("[bolt] fetch {fetch_ms:.0}ms | page {page_ms:.0}ms (scripts {:.0}ms) | fonts {fonts_ms:.0}ms | images {:.0}ms", page.script_ms, ms(t_imgs));
    for &(x, y) in &a.clicks {
        let r = page.click_full(x, y);
        println!("[click {x},{y}] {}", match &r {
            Click::Default => "default".to_string(),
            Click::Handled => "handled".to_string(),
            Click::Navigate(n) => format!("navigate {}", n.url),
        });
        // Let the page react (handlers, requests, animations).
        // BOLT_CLICK_MS: page time to simulate after each click (default 3000).
        let click_ms: f64 = std::env::var("BOLT_CLICK_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000.0);
        let t = Instant::now();
        let until = now + click_ms;
        while now < until && ms(t) < 15000.0f64.max(click_ms * 3.0) {
            now += 16.0;
            if page.has_pending_requests() {
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
            page.tick(now);
        }
        settle_frames(&mut page, &mut now);
    }
    for src in &a.eval {
        println!("[eval] {}", page.eval(src).unwrap_or_else(|| "(no script engine)".into()));
    }
    if std::env::var_os("BOLT_DUMP_BOXES").is_some() {
        if std::env::var_os("BOLT_FORCE_RELAYOUT").is_some() {
            page.relayout(w as f32, vh);
        }
        dump_boxes(&page, page.tree.root, 0);
        for (i, b) in page.tree.boxes.iter().enumerate() {
            if b.abs || b.fixed {
                let desc = match page.doc.elem(b.node) {
                    Some(e) => format!("<{}{}{}>", atom::name(e.tag), e.id.map(|i| format!("#{}", atom::name(i))).unwrap_or_default(), e.classes.iter().map(|c| format!(".{}", atom::name(*c))).collect::<String>()),
                    None => String::new(),
                };
                println!("[abs box{i}] {desc} n={} x={} y={} w={} h={} fixed={}", b.node, b.x, b.y, b.w, b.h, b.fixed);
                if std::env::var_os("BOLT_DUMP_ABS_TREES").is_some() {
                    dump_boxes(&page, i as u32, 1);
                }
            }
        }
    }
    for &(x, y) in &a.probe {
        let mut n = page.tree.node_at(x, y).unwrap_or(dom::NONE);
        let rects = page.tree.node_rects();
        print!("[probe {x},{y}] n={n} nodes={} {:?}", page.doc.nodes.len(), rects.get(&n));
        for (i, b) in page.tree.boxes.iter().enumerate().filter(|(_, b)| b.node == n || page.tree.pseudo_owner.get(&b.node) == Some(&n)) {
            print!(" [box{i} node={} {:?} x={} y={} w={} h={} anon={}]", b.node, b.kind, b.x, b.y, b.w, b.h, b.anon);
        }
        while n != dom::NONE {
            match page.doc.elem(n) {
                Some(e) => {
                    let mut d = crate::atom::name(e.tag).to_string();
                    if let Some(id) = e.attr(atom::k::id) {
                        d += &format!("#{id}");
                    }
                    if let Some(c) = e.attr(atom::k::class) {
                        d += &format!(".{}", c.split_whitespace().take(3).collect::<Vec<_>>().join("."));
                    }
                    print!(" < {d}");
                }
                None if n == 0 => print!(" < #document"),
                None => print!(" < #text"),
            }
            n = page.doc.parent(n);
        }
        println!();
    }
    let h = if a.height == 0 { (page.tree.height.ceil() as usize).clamp(1, 16000) } else { a.height };
    if let Some(text) = &a.typed {
        let field = page.doc.preorder().into_iter().find(|&n| page.is_text_field_at(n) && page.doc.elem(n).is_some());
        if let Some(f) = field {
            page.set_focus(f);
            // A trailing "\n" (backslash, n) presses Enter and prints where it navigates.
            let (text, enter) = match text.strip_suffix("\\n") {
                Some(t) => (t, true),
                None => (text.as_str(), false),
            };
            page.key(Edit::Insert(text.to_owned()));
            if enter {
                let nav = page.key(Edit::Enter);
                println!("[enter] {:?}", nav.map(|n| n.url));
            }
        }
    }
    let mut buf = vec![0u32; w * h];
    let mut glyphs = GlyphCache::default();
    let t0 = Instant::now();
    paint::paint(&page.dl, &page.tree.text, &mut glyphs, &mut buf, w, h, 0.0);
    paint_focus(&page, &mut buf, w, h, 0.0, true);
    let mut t = page.t;
    t.paint = ms(t0);
    std::fs::write(out, png::encode(&buf, w, h)).expect("write png");
    println!("{} -> {out} ({w}x{h})\n{}", page.url, t.summary());
}

/// Draw the focus ring and caret of the focused form field.
fn paint_focus(p: &Page, buf: &mut [u32], w: usize, h: usize, scroll: f32, caret: bool) {
    let Some(n) = p.focused() else { return };
    if let Some(r) = p.tree.node_rects().get(&n).copied() {
        let mut s = paint::Surface::new(buf, w, h, 0.0, 0.0);
        let (x, y, rw, rh) = (r[0] - 1.0, r[1] - scroll - 1.0, r[2] + 2.0, r[3] + 2.0);
        let c = 0xFF10_5CD8;
        s.rect(x, y, rw, 2.0, c);
        s.rect(x, y + rh - 2.0, rw, 2.0, c);
        s.rect(x, y, 2.0, rh, c);
        s.rect(x + rw - 2.0, y, 2.0, rh, c);
    }
    if caret {
        if let Some(c) = p.caret_rect() {
            paint::Surface::new(buf, w, h, 0.0, 0.0).rect(c[0], c[1] - scroll, c[2], c[3], 0xFF00_0000);
        }
    }
}

// --------------------------------------------------------------- browser

struct CharSink(Rc<RefCell<Vec<u32>>>);

impl minifb::InputCallback for CharSink {
    fn add_char(&mut self, c: u32) {
        self.0.borrow_mut().push(c);
    }
}

/// A session history entry. `generation` / `pos` identify the document and
/// its own (pushState) history position, for same-document back / forward.
struct HistEntry {
    url: String,
    scroll: f32,
    generation: u64,
    pos: usize,
}

struct Browser {
    window: Window,
    page: Option<Page>,
    error: Option<String>,
    scroll: f32,
    url_input: String,
    editing: bool,
    select_all: bool,
    history: Vec<HistEntry>,
    forward: Vec<HistEntry>,
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
    /// Caret blink phase start, and whether the caret is drawn.
    blink: Instant,
    caret_on: bool,
    last_mouse: Option<(f32, f32)>,
    /// A navigation whose network fetch and parse run on a background
    /// thread (the window stays responsive meanwhile).
    pending: Option<PendingLoad>,
}

struct PendingLoad {
    url: String,
    rx: std::sync::mpsc::Receiver<Result<engine::Loaded, String>>,
    started: Instant,
}

impl Browser {
    fn run(url: &str, w: usize, h: usize) {
        engine::set_interactive(true);
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
            blink: Instant::now(),
            caret_on: false,
            last_mouse: None,
            pending: None,
        };
        b.navigate(url, true);
        b.event_loop();
    }

    fn vh(&self) -> usize {
        self.h.saturating_sub(BAR).max(1)
    }

    fn navigate(&mut self, url: &str, push_history: bool) {
        self.navigate_to(Nav { url: url.to_owned(), post: None }, push_history);
    }

    fn navigate_to(&mut self, nav: Nav, push_history: bool) {
        let Nav { url, post } = nav;
        // Same-document fragment navigation.
        if let (Some(p), Some(hash), None) = (&self.page, url.find('#'), &post) {
            let cur = p.url.split('#').next().unwrap_or("");
            if &url[..hash] == cur {
                let frag = &url[hash + 1..];
                self.scroll = if frag.is_empty() || frag == "top" { 0.0 } else { self.find_anchor(frag).unwrap_or(self.scroll) };
                self.repaint();
                return;
            }
        }
        if push_history {
            if let Some(e) = self.current_entry() {
                self.history.push(e);
                self.forward.clear();
            }
        }
        self.url_input = url.clone();
        self.editing = false;
        self.window.set_title(&format!("Loading {url} — bolt"));
        // Fetch and parse off the UI thread; `poll_load` finishes the job.
        // A newer navigation replaces this one (its result is dropped).
        let (tx, rx) = std::sync::mpsc::channel();
        let req_url = url.clone();
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let _ = tx.send(engine::load_req(&req_url, post.as_deref()));
            })
            .expect("spawn loader");
        self.pending = Some(PendingLoad { url, rx, started: Instant::now() });
        self.repaint();
        let _ = self.window.update_with_buffer(&self.buf, self.w, self.h);
    }

    /// Finish a background navigation once its document has arrived.
    /// Returns whether anything changed.
    fn poll_load(&mut self) -> bool {
        let Some(p) = &self.pending else { return false };
        let result = match p.rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("loader failed".into()),
        };
        let url = self.pending.take().map(|p| p.url).unwrap_or_default();
        self.window.set_title(&format!("Running {url} — bolt"));
        self.finish_navigation(result);
        true
    }

    fn finish_navigation(&mut self, result: Result<engine::Loaded, String>) {
        match result {
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
            let title = p.title();
            let title = if title.is_empty() { p.url.as_str() } else { title.as_str() };
            self.window.set_title(&format!("{title} — bolt"));
            eprintln!("[bolt] {}\n        fetch {:.1}ms | scripts {:.1}ms | {}", p.url, p.t.fetch, p.script_ms, p.t.summary());
        }
        // A script redirect at load (location.href = ...).
        if let Some(n) = self.page.as_mut().and_then(|p| p.take_nav()) {
            self.navigate_to(n, false);
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
            page.res.images.insert(u.clone(), None);
            page.res.pending.insert(u.clone());
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
                    p.res.pending.remove(&u);
                    p.res.images.insert(u, img);
                }
                changed = true;
            }
        }
        if changed {
            let (w, vh) = (self.w as f32, self.vh() as f32);
            if let Some(p) = &mut self.page {
                p.relayout(w, vh);
                p.fire_image_events();
                // Newly visible elements (e.g. via media queries) may want more images.
                let urls = p.wanted_images();
                if !urls.is_empty() {
                    let (tx, generation) = (self.img_tx.clone(), self.generation);
                    for u in &urls {
                        p.res.images.insert(u.clone(), None);
                        p.res.pending.insert(u.clone());
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
        if let Some(p) = &mut self.page {
            p.set_scroll(self.scroll);
        }
        let (w, vh) = (self.w, self.vh());
        let t0 = Instant::now();
        if let Some(p) = &self.page {
            paint::paint(&p.dl, &p.tree.text, &mut self.glyphs, &mut self.buf[BAR * w..], w, vh, self.scroll.round());
            paint_focus(p, &mut self.buf[BAR * w..], w, vh, self.scroll.round(), self.caret_on);
        } else {
            self.buf[BAR * w..].fill(0xFFFF_FFFF);
            if let Some(e) = self.error.clone() {
                self.draw_text(&format!("Could not load page: {e}"), 20.0, BAR as f32 + 40.0, 16.0, 0xFFB0_0020);
            }
        }
        if let Some(p) = &mut self.page {
            p.t.paint = ms(t0);
        }
        self.draw_chrome(self.pending.is_some());
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
        s_.text(s, x, y, face, size, color, &paint::TextSp::default(), f, &self.glyphs);
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
            // Indeterminate progress: a segment sweeping across the field.
            let t = self.pending.as_ref().map_or(0.0, |p| p.started.elapsed().as_secs_f32());
            let span = w - 50.0;
            let seg = span * 0.25;
            let x = ((t * 0.8).fract() * (span + seg)) - seg;
            let (x0, x1) = (x.max(0.0), (x + seg).min(span));
            if x1 > x0 {
                self.fill(41.0 + x0, 30.0, x1 - x0, 2.0, 0xFF1A_56DB);
            }
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

    /// A history entry for where we are now.
    fn current_entry(&self) -> Option<HistEntry> {
        let p = self.page.as_ref()?;
        Some(HistEntry { url: p.url.clone(), scroll: self.scroll, generation: self.generation, pos: p.hist_pos() })
    }

    /// Go to a history entry: within the current document (pushState
    /// entries) fire popstate; otherwise load it.
    fn restore(&mut self, e: HistEntry) {
        if e.generation == self.generation && self.page.is_some() {
            if let Some(p) = self.page.as_mut() {
                p.pop_state(e.pos);
                self.url_input = p.url.clone();
            }
        } else {
            self.navigate(&e.url, false);
        }
        self.scroll = e.scroll;
        self.repaint();
    }

    fn go_back(&mut self) {
        if let Some(e) = self.history.pop() {
            if let Some(cur) = self.current_entry() {
                self.forward.push(cur);
            }
            self.restore(e);
        }
    }

    fn go_forward(&mut self) {
        if let Some(e) = self.forward.pop() {
            if let Some(cur) = self.current_entry() {
                self.history.push(cur);
            }
            self.restore(e);
        }
    }

    /// Apply pushState / replaceState / history.go() requests from the page.
    fn page_history(&mut self) {
        let Some(p) = self.page.as_mut() else { return };
        let events = p.take_url_events();
        let go = p.take_history_go();
        let url = p.url.clone();
        for ev in events {
            if ev.push {
                self.history.push(HistEntry { url: ev.prev_url, scroll: self.scroll, generation: self.generation, pos: ev.prev_pos });
                self.forward.clear();
            }
        }
        if !self.editing {
            self.url_input = url;
        }
        match go {
            Some(n) if n < 0 => (0..-n).for_each(|_| self.go_back()),
            Some(n) if n > 0 => (0..n).for_each(|_| self.go_forward()),
            _ => {}
        }
    }

    fn event_loop(&mut self) {
        while self.window.is_open() {
            let mut dirty = false;
            if self.poll_load() {
                dirty = true;
            }
            // Animate the loading bar.
            if self.pending.is_some() {
                dirty = true;
            }
            if self.poll_images() {
                dirty = true;
            }
            // JavaScript timers.
            let now = self.loaded_at.elapsed().as_secs_f64() * 1000.0;
            if self.page.as_mut().is_some_and(|p| p.tick(now)) {
                dirty = true;
            }
            self.page_history();
            if let Some(y) = self.page.as_mut().and_then(|p| p.take_scroll_request()) {
                self.scroll = y;
                dirty = true;
            }
            if let Some(n) = self.page.as_mut().and_then(|p| p.take_nav()) {
                self.navigate_to(n, true);
                continue;
            }
            let caret_on = self.page.as_ref().is_some_and(|p| p.focused().is_some()) && (self.blink.elapsed().as_millis() / 530) % 2 == 0;
            if caret_on != self.caret_on {
                self.caret_on = caret_on;
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

            let field_focused = !self.editing && self.page.as_ref().is_some_and(|p| p.focused().is_some());
            if field_focused {
                let mut edits: Vec<Edit> = Vec::new();
                let text: String = typed.iter().filter_map(|&c| char::from_u32(c)).filter(|c| !c.is_control()).collect();
                if !text.is_empty() && !ctrl {
                    edits.push(Edit::Insert(text));
                }
                for k in self.window.get_keys_pressed(KeyRepeat::Yes) {
                    edits.push(match k {
                        Key::Backspace if ctrl => Edit::BackspaceWord,
                        Key::Backspace => Edit::Backspace,
                        Key::Delete => Edit::Delete,
                        Key::Left => Edit::Left,
                        Key::Right => Edit::Right,
                        Key::Home => Edit::Home,
                        Key::End => Edit::End,
                        Key::Enter | Key::NumPadEnter => Edit::Enter,
                        Key::Tab => Edit::Tab,
                        Key::Escape => Edit::Escape,
                        Key::V if ctrl => match clipboard() {
                            Some(t) => Edit::Insert(t),
                            None => continue,
                        },
                        _ => continue,
                    });
                }
                if (ctrl && self.window.is_key_pressed(Key::L, KeyRepeat::No)) || self.window.is_key_pressed(Key::F6, KeyRepeat::No) {
                    if let Some(p) = &mut self.page {
                        p.set_focus(dom::NONE);
                    }
                    self.editing = true;
                    self.select_all = true;
                    dirty = true;
                }
                let mut nav = None;
                for e in edits {
                    if let Some(p) = &mut self.page {
                        nav = p.key(e).or(nav);
                    }
                    self.blink = Instant::now();
                    dirty = true;
                }
                if let Some(n) = nav {
                    self.navigate_to(n, true);
                    continue;
                }
            } else if self.editing {
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

            if let Some((dx, dy)) = self.window.get_scroll_wheel() {
                // Scroll containers under the pointer scroll first.
                let mouse = self.window.get_mouse_pos(MouseMode::Discard);
                let inner = match (mouse, self.page.as_mut()) {
                    (Some((mx, my)), Some(p)) if my as usize >= BAR => p.wheel(mx, my - BAR as f32 + self.scroll.round(), -dx * 5.0, -dy * 5.0),
                    _ => false,
                };
                if !inner {
                    self.scroll -= dy * 5.0;
                }
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
            if hover != self.hover || mouse != self.last_mouse {
                let over_field = match (mouse, &self.page) {
                    (Some((mx, my)), Some(p)) if hover.is_none() && my as usize >= BAR => {
                        p.tree.node_at(mx, my - BAR as f32 + self.scroll.round()).is_some_and(|n| p.is_text_field_at(n))
                    }
                    _ => false,
                };
                let cursor = if hover.is_some() {
                    CursorStyle::ClosedHand
                } else if over_field {
                    CursorStyle::Ibeam
                } else {
                    CursorStyle::Arrow
                };
                self.window.set_cursor_style(cursor);
                self.last_mouse = mouse;
            }
            if hover != self.hover {
                self.hover = hover.clone();
                dirty = true;
            }
            // :hover / :active and mouse events.
            let under = match (mouse, &self.page) {
                (Some((mx, my)), Some(p)) if my as usize >= BAR => p.tree.node_at(mx, my - BAR as f32 + self.scroll.round()),
                _ => None,
            };
            if let (Some(p), Some((mx, my))) = (self.page.as_mut(), mouse) {
                p.pointer_at = Some((mx, my - BAR as f32 + self.scroll.round()));
            }
            if self.page.as_mut().is_some_and(|p| p.set_pointer(under, down)) {
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
                        let result = self.page.as_mut().map_or(Click::Default, |p| p.click(mx, py));
                        self.blink = Instant::now();
                        if let Click::Navigate(n) = result {
                            self.hover = None;
                            self.navigate_to(n, true);
                            continue;
                        } else if let (Click::Default, Some(target)) = (&result, hover) {
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

/// Text on the system clipboard (for Ctrl+V into form fields).
fn clipboard() -> Option<String> {
    let out = if cfg!(windows) {
        std::process::Command::new("powershell").args(["-NoProfile", "-Command", "Get-Clipboard -Raw"]).output()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("pbpaste").output()
    } else {
        std::process::Command::new("xclip").args(["-o", "-selection", "clipboard"]).output()
    }
    .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\r', '\n']).replace("\r\n", "\n");
    (!t.is_empty()).then_some(t)
}
