//! A small JavaScript interpreter: tree-walking evaluator over the AST from
//! `parser`, with prototype objects, closures, classes, exceptions,
//! promises/timers and a DOM binding (see `dom.rs`).

pub mod bigint;
pub mod binary;
pub mod builtins;
pub mod coro;
pub mod cssom;
pub mod custom;
pub mod dombind;
pub mod generator;
pub mod iter;
pub mod module;
pub mod observe;
pub mod ordmap;
pub mod proxy;
pub mod lexer;
pub mod parser;
pub mod regex;
pub mod web;
pub mod vm;
pub mod shape;
pub mod jit;
pub mod messaging;
pub mod wasm;

use crate::dom::{Document, NodeId};
use crate::fx::FxHashMap;
use parser::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

pub type ObjRef = Rc<RefCell<Obj>>;
pub type R<T> = Result<T, Value>;
pub type NativeFn = fn(&mut Interp, Value, &[Value]) -> R<Value>;

#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Undef,
    /// An array hole: only inside array storage (reads see `undefined`;
    /// map / forEach / in / keys skip it). Elsewhere it acts as undefined.
    Hole,
    Null,
    Bool(bool),
    Num(f64),
    /// BigInt (64-bit-and-then-some: values beyond i128 throw RangeError).
    BigInt(Big),
    Str(Rc<str>),
    Obj(ObjRef),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Undef | Value::Hole => write!(f, "undefined"),
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(n) => write!(f, "{}", num_to_str(*n)),
            Value::BigInt(n) => write!(f, "{n}n"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Obj(_) => write!(f, "[object]"),
        }
    }
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }
    pub fn obj(&self) -> Option<&ObjRef> {
        if let Value::Obj(o) = self { Some(o) } else { None }
    }
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undef | Value::Null | Value::Hole)
    }

    /// Holes read as undefined.
    pub fn unhole(self) -> Value {
        if let Value::Hole = self { Value::Undef } else { self }
    }
    pub fn truthy(&self) -> bool {
        match self {
            Value::Undef | Value::Null | Value::Hole => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::BigInt(n) => n.get() != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Obj(_) => true,
        }
    }
}

pub fn num_to_str(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if n == 0.0 {
        return "0".into();
    }
    if n.fract() == 0.0 && n.abs() < 1e21 {
        if n.abs() < 9007199254740992.0 {
            return format!("{n:.0}");
        }
        // Shortest round-trip digits, padded with zeros (not the exact
        // binary value).
        let s = format!("{:e}", n.abs());
        if let Some((m, e)) = s.split_once('e') {
            let digits: String = m.chars().filter(|c| c.is_ascii_digit()).collect();
            let exp: usize = e.parse().unwrap_or(0);
            let mut out = String::from(if n < 0.0 { "-" } else { "" });
            out.push_str(&digits);
            for _ in digits.len()..exp + 1 {
                out.push('0');
            }
            return out;
        }
    }
    if n.abs() >= 1e21 || n.abs() < 1e-6 {
        let s = format!("{n:e}");
        return if let Some((m, e)) = s.split_once('e') {
            if e.starts_with('-') { format!("{m}e{e}") } else { format!("{m}e+{e}") }
        } else {
            s
        };
    }
    format!("{n}")
}

#[derive(Clone)]
pub struct Prop {
    pub v: Value,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub enumerable: bool,
}

impl Prop {
    pub fn data(v: Value) -> Prop {
        Prop { v, get: None, set: None, enumerable: true }
    }
}

pub use shape::Props;
use shape::ShapeClass;

/// The shape tree for objects of a kind (see `shape.rs`).
pub fn shape_class(k: &Kind) -> ShapeClass {
    match k {
        Kind::Plain | Kind::Func(_) | Kind::Error | Kind::Date(_) | Kind::Regex(_) | Kind::Map(_) | Kind::Set(_) | Kind::Promise(_) => ShapeClass::Ordinary,
        Kind::Array(_) => ShapeClass::Array,
        _ => ShapeClass::Special,
    }
}

/// A BigInt's value: an i128 stored with 8-byte alignment, so a `Value`
/// stays 24 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Big([u64; 2]);

impl Big {
    #[inline]
    pub fn new(v: i128) -> Big {
        Big([v as u64, (v >> 64) as u64])
    }
    #[inline]
    pub fn get(self) -> i128 {
        ((self.0[1] as i128) << 64) | self.0[0] as i128
    }
}

impl std::fmt::Display for Big {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl Value {
    #[inline]
    pub fn big(v: i128) -> Value {
        Value::BigInt(Big::new(v))
    }
}

pub enum PromiseState {
    Pending(Vec<(Value, Value, ObjRef)>),
    Fulfilled(Value),
    Rejected(Value),
}

pub struct ClassInfo {
    pub def: Rc<ClassDef>,
    pub parent: Option<Value>,
    pub env: Env,
}

pub enum Func {
    Closure { def: Rc<FuncDef>, env: Env, this: Option<Value>, home: Option<ObjRef> },
    Class(Rc<ClassInfo>),
    Native(NativeFn, &'static str),
    Bound(Value, Value, Vec<Value>),
}

pub enum Kind {
    Plain,
    Array(Vec<Value>),
    Func(Func),
    Error,
    Date(f64),
    Regex(Rc<regex::Regex>),
    Map(ordmap::OrdMap),
    Set(ordmap::OrdMap),
    Promise(PromiseState),
    /// A DOM node wrapper.
    Node(NodeId),
    /// element.style / element.classList / element.dataset views.
    Style(NodeId),
    ClassList(NodeId),
    Dataset(NodeId),
    /// Boxed primitive (new String(...), etc).
    Boxed(Value),
    Generator(Box<Gen>),
    /// A built-in iterator (array / map / set / string iteration).
    Iter(Box<iter::IterState>),
    Proxy(Box<proxy::ProxyData>),
    /// A CSSStyleSheet.
    Sheet(crate::dom::SheetRef),
    /// ArrayBuffer contents.
    Buffer(Vec<u8>),
    /// A typed array view onto an ArrayBuffer.
    Typed(binary::View),
    DataView(binary::View),
    /// Headers / URLSearchParams / FormData entries.
    Pairs(web::PairKind, Vec<(Rc<str>, Rc<str>)>),
    /// Native data behind a host object (WebAssembly modules, instances,
    /// memories, tables, globals).
    Host(Rc<dyn std::any::Any>),
}

/// A generator (or the hidden state of an async function call).
///
/// Normally the body runs on a coroutine (`gen.rs`). Where coroutines are
/// unsupported, the body runs to completion on the first `next()` (with a cap
/// on yields) and the yielded values are handed out one by one.
#[derive(Default)]
pub struct Gen {
    /// Coroutine-backed (the fields below `is_async` apply).
    pub coro: bool,
    pub is_async: bool,
    pub co: Option<coro::Co>,
    pub state: generator::GenState,
    /// JS call depth inside the body while it is suspended.
    pub depth: u32,
    /// Value passed to `return()`, delivered when the body finishes.
    pub ret_val: Value,
    /// An async function call's result promise.
    pub promise: Option<ObjRef>,
    /// Async generator requests waiting for the body.
    pub queue: VecDeque<(generator::Req, Value, ObjRef)>,
    // Eager fallback:
    pub start: Option<GenStart>,
    pub buf: VecDeque<Value>,
    pub ret: Option<Value>,
    pub err: Option<Value>,
}

impl Default for generator::GenState {
    fn default() -> Self {
        generator::GenState::Fresh
    }
}

pub struct GenStart {
    pub def: Rc<FuncDef>,
    pub env: Env,
    pub home: Option<ObjRef>,
    pub this: Value,
    pub args: Vec<Value>,
    pub fobj: ObjRef,
}

/// An event listener: target (node or WINDOW), type, callback, options.
pub struct Listener {
    pub ty: Rc<str>,
    pub f: Value,
    pub capture: bool,
    pub once: bool,
    pub passive: bool,
    /// An AbortSignal that removes the listener.
    pub signal: Option<Value>,
}

pub struct LayoutSrc {
    pub css: FxHashMap<String, String>,
    pub base: String,
}

impl Interp {
    /// Make `rects` / `scroll_metrics` reflect the current DOM (a forced,
    /// synchronous layout when scripts changed it since the last one).
    pub fn ensure_layout(&mut self) {
        let want = (self.doc.version, self.vw.to_bits(), self.vh.to_bits());
        if self.geom_version == want {
            return;
        }
        let Some(src) = self.layout_src.clone() else { return };
        let t = std::time::Instant::now();
        let (rects, metrics, styles) = crate::engine::script_layout(&self.doc, &src, &self.images, self.vw, self.vh);
        if std::env::var_os("BOLT_PROFILE").is_some() {
            eprintln!("[profile] forced layout {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        self.rects = rects;
        self.scroll_metrics = metrics;
        self.styles = Some((self.doc.version, Rc::new(styles)));
        self.geom_version = want;
    }

    /// Computed styles of the current DOM.
    pub fn computed_styles(&mut self) -> Option<Rc<Vec<crate::style::Style>>> {
        if !matches!(&self.styles, Some((v, _)) if *v == self.doc.version) {
            self.geom_version = (u64::MAX, 0, 0);
            self.ensure_layout();
        }
        self.styles.as_ref().map(|s| s.1.clone())
    }
}

/// A same-document URL change (`history.pushState` / `replaceState`).
pub struct UrlEvent {
    pub push: bool,
    /// The session history position before the change.
    pub prev_pos: usize,
    pub prev_url: String,
}

/// Maximum JS call depth on the main stack.
const MAX_DEPTH: u32 = 7000;

/// Thrown internally to stop a generator that yielded too many values.
const GEN_CAP_MARKER: &str = "@@generator-cap";
const GEN_CAP: usize = 100_000;

pub struct Obj {
    pub props: Props,
    pub proto: Option<ObjRef>,
    pub kind: Kind,
}

pub struct Scope {
    pub vars: RefCell<FxHashMap<Rc<str>, Value>>,
    pub parent: Option<Env>,
    /// Function scopes record `this` (arrow functions don't).
    pub this: Option<Value>,
    pub func: Option<ObjRef>,
    pub is_func: bool,
}

pub type Env = Rc<Scope>;

pub fn new_scope(parent: Option<Env>, this: Option<Value>, func: Option<ObjRef>, is_func: bool) -> Env {
    Rc::new(Scope { vars: RefCell::new(FxHashMap::default()), parent, this, func, is_func })
}

pub enum Flow {
    Normal,
    Ret(Value),
    Brk(Option<Name>),
    Cont(Option<Name>),
}

pub struct Timer {
    pub id: u32,
    pub at: f64,
    pub interval: Option<f64>,
    pub func: Value,
    pub args: Vec<Value>,
}

/// Well-known symbols (stored as "@@name" strings).
pub const WELL_KNOWN_SYMBOLS: &[&str] = &[
    "@@iterator", "@@asyncIterator", "@@toStringTag", "@@hasInstance", "@@toPrimitive", "@@species",
    "@@isConcatSpreadable", "@@unscopables", "@@match", "@@matchAll", "@@replace", "@@search", "@@split",
];

/// Symbols are strings "@@sym:<id>:<description>" / "@@sym:for:<key>" or a
/// well-known "@@name".
pub fn is_symbol_str(s: &str) -> bool {
    s.starts_with("@@sym:") || WELL_KNOWN_SYMBOLS.contains(&s)
}

/// A symbol's description (None: not a symbol, or no description).
pub fn symbol_description(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix("@@sym:") {
        let d = rest.strip_prefix("for:").unwrap_or_else(|| rest.split_once(':').map_or("", |(_, d)| d));
        return (!d.is_empty() || rest.starts_with("for:")).then(|| d.to_string());
    }
    WELL_KNOWN_SYMBOLS.contains(&s).then(|| format!("Symbol.{}", &s[2..]))
}

#[derive(Default)]
pub struct Protos {
    pub symbol: Option<ObjRef>,
    pub object: Option<ObjRef>,
    pub function: Option<ObjRef>,
    pub array: Option<ObjRef>,
    pub string: Option<ObjRef>,
    pub number: Option<ObjRef>,
    pub bigint: Option<ObjRef>,
    pub boolean: Option<ObjRef>,
    pub error: Option<ObjRef>,
    pub regexp: Option<ObjRef>,
    pub date: Option<ObjRef>,
    pub promise: Option<ObjRef>,
    pub map: Option<ObjRef>,
    pub set: Option<ObjRef>,
    pub node: Option<ObjRef>,
    pub event: Option<ObjRef>,
    pub generator: Option<ObjRef>,
    pub iterator: Option<ObjRef>,
    pub array_iterator: Option<ObjRef>,
    pub style: Option<ObjRef>,
    pub classlist: Option<ObjRef>,
    pub array_buffer: Option<ObjRef>,
    pub data_view: Option<ObjRef>,
    /// Per element type, in `binary::TA_ALL` order.
    pub typed: Vec<ObjRef>,
    /// Web API prototypes by interface name.
    pub web: FxHashMap<&'static str, ObjRef>,
    pub errors: FxHashMap<&'static str, ObjRef>,
}

pub struct Interp {
    pub global: ObjRef,
    pub genv: Env,
    pub p: Protos,
    pub doc: Document,
    /// The DOM was modified (restyle + relayout needed).
    pub dirty: bool,
    pub steps: u64,
    /// Steps a task may run (the time limit normally stops it first).
    pub budget: u64,
    pub timers: Vec<Timer>,
    pub next_timer: u32,
    pub micro: VecDeque<(Value, Vec<Value>)>,
    /// Event listeners, in registration order.
    /// Event listeners by target node (registration order within a target).
    pub listeners: FxHashMap<u32, Vec<Listener>>,
    /// document.readyState and document.currentScript.
    pub ready_state: &'static str,
    pub current_script: NodeId,
    pub node_objs: FxHashMap<NodeId, ObjRef>,
    pub url: String,
    pub storage: FxHashMap<String, String>,
    pub now: f64,
    pub vw: f32,
    pub vh: f32,
    pub constructing: bool,
    pub rng: u64,
    pub symbols: u32,
    pub console: Vec<String>,
    /// External scripts inserted by scripts, waiting to be fetched and run:
    /// (url, script element).
    pub pending_scripts: Vec<(String, NodeId)>,
    pub cookie: String,
    /// Element border boxes from the last layout (x, y, w, h).
    pub rects: FxHashMap<NodeId, [f32; 4]>,
    pub start: std::time::Instant,
    /// Label of the loop about to execute (for labeled break/continue).
    label: Option<Name>,
    /// Scripts are interrupted after this instant.
    pub deadline: Option<std::time::Instant>,
    /// Values yielded by the generators currently running.
    gen_sinks: Vec<Vec<Value>>,
    /// Nesting depth of JS function calls (per coroutine).
    pub depth: u32,
    /// The last member access / call short-circuited an optional chain.
    short: bool,
    /// Session history of this document: (url, state); see `history`.
    pub hist: Vec<(String, Value)>,
    pub hist_pos: usize,
    /// URL changes made by pushState / replaceState (push?), for the browser.
    pub url_events: Vec<UrlEvent>,
    /// history.go(n) requested by a script.
    pub history_go: Option<i32>,
    /// ES modules, by id; `module_map` maps URLs to ids.
    pub modules: Vec<module::Module>,
    pub module_map: FxHashMap<String, usize>,
    /// Prefetched module sources by URL.
    pub module_sources: FxHashMap<String, String>,
    /// Import map entries (specifier or prefix -> URL), longest first.
    pub import_map: Vec<(String, String)>,
    pub mut_regs: Vec<observe::MutReg>,
    /// Mutation records waiting for delivery: (observer, record).
    pub mut_queue: Vec<(ObjRef, Value)>,
    pub geo_regs: Vec<observe::GeoReg>,
    /// A geometry observer was added (check at the next opportunity).
    pub geo_pending: bool,
    pub ce_defs: Vec<custom::CeDef>,
    /// Custom elements that have been constructed (-> definition index).
    pub ce_upgraded: FxHashMap<NodeId, usize>,
    /// Elements whose upgrade constructor is running (innermost last).
    pub ce_upgrading: Vec<NodeId>,
    /// `this` of a class constructor calling a native parent via `super()`.
    pub ce_this: Option<Value>,
    pub ce_when: Vec<(Rc<str>, ObjRef)>,
    /// The page's vertical scroll offset.
    pub scroll_y: f32,
    /// A scroll position requested by a script (`scrollTo`, ...).
    pub scroll_request: Option<f32>,
    /// Element scroll offsets (scroll containers), synced with the page;
    /// `elem_scroll_dirty` when a script changed them.
    pub elem_scroll: FxHashMap<NodeId, (f32, f32)>,
    pub elem_scroll_dirty: bool,
    /// Per element: [scrollWidth, scrollHeight, clientWidth, clientHeight].
    pub scroll_metrics: FxHashMap<NodeId, [f32; 4]>,
    /// What scripts need to lay the page out themselves (when they read
    /// geometry of a changed DOM): cached stylesheets and the base URL.
    pub layout_src: Option<Rc<LayoutSrc>>,
    /// Document version / viewport the geometry above was computed for.
    pub geom_version: (u64, u32, u32),
    /// Computed styles from a script-triggered layout (for getComputedStyle).
    pub styles: Option<(u64, Rc<Vec<crate::style::Style>>)>,
    /// Generators / async calls whose coroutines are running, innermost last.
    pub co_stack: Vec<ObjRef>,
    /// Top-level scripts / module bodies being evaluated (JS on the stack).
    pub script_running: u32,
    /// The current task overran its time limit and is being unwound
    /// (try / catch / finally don't intercept it).
    pub terminating: bool,
    /// Images the page has loaded, by URL (None: failed). For script-side
    /// layout, naturalWidth / complete, and load events.
    pub images: FxHashMap<String, Option<std::sync::Arc<crate::image::Image>>>,
    /// <img> elements whose load / error event has fired (for which URL).
    pub img_fired: FxHashMap<NodeId, String>,
    /// <link rel=stylesheet> elements whose load event has fired.
    pub link_fired: FxHashMap<NodeId, String>,
    /// Page scripts that didn't fit the startup budget: run one per event
    /// loop turn (the window stays responsive), in order.
    pub script_queue: std::collections::VecDeque<NodeId>,
    /// External script sources for the queued scripts.
    pub script_sources: Option<Rc<FxHashMap<String, String>>>,
    /// DOMContentLoaded / load still to fire (after the queue drains).
    pub load_events_pending: bool,
    /// Promises rejected with no handler yet (reported after microtasks).
    pub unhandled_rejections: Vec<ObjRef>,
    /// Navigation requested by a script (`location.assign`, `form.submit()`).
    pub nav: Option<crate::forms::Nav>,
    /// A form a script submitted, to be serialized by the page.
    pub submit_form: Option<NodeId>,
    /// fetch / XMLHttpRequest requests in flight.
    pub requests: Vec<web::Pending>,
    /// MessagePorts this interpreter receives messages for.
    pub owned_ports: crate::fx::FxHashSet<u64>,
    /// Window messages to other frames, moved by the page.
    pub frame_outbox: Vec<messaging::FrameMsg>,
    /// Recycled VM register files / scope stacks.
    pub vm_pool: Vec<Vec<Value>>,
    pub vm_env_pool: Vec<Vec<Env>>,
}

pub const WINDOW: u32 = u32::MAX - 1;

/// Thrown to unwind a task that overran its time limit.
pub const TERMINATE: &str = "@@terminate";

pub fn type_error(msg: impl Into<String>) -> Value {
    builtins::make_error_value("TypeError", &msg.into())
}

pub fn range_error(msg: &str) -> Value {
    builtins::make_error_value("RangeError", msg)
}

pub fn syntax_error(msg: &str) -> Value {
    builtins::make_error_value("SyntaxError", msg)
}

impl Interp {
    /// A new interpreter. Boxed: suspended coroutines keep pointers to it,
    /// so it must not move.
    pub fn new(doc: Document, url: &str, vw: f32, vh: f32) -> Box<Interp> {
        let global = Rc::new(RefCell::new(Obj { props: Props::default(), proto: None, kind: Kind::Plain }));
        let genv = new_scope(None, Some(Value::Obj(global.clone())), None, true);
        let mut it = Box::new(Interp {
            global,
            genv,
            p: Protos::default(),
            doc,
            dirty: false,
            steps: 0,
            budget: 5_000_000_000,
            timers: Vec::new(),
            next_timer: 1,
            micro: VecDeque::new(),
            listeners: FxHashMap::default(),
            ready_state: "complete",
            current_script: crate::dom::NONE,
            node_objs: FxHashMap::default(),
            url: url.to_owned(),
            storage: FxHashMap::default(),
            now: 0.0,
            vw,
            vh,
            constructing: false,
            rng: 0x2545F4914F6CDD1D,
            symbols: 0,
            console: Vec::new(),
            pending_scripts: Vec::new(),
            cookie: String::new(),
            rects: FxHashMap::default(),
            start: std::time::Instant::now(),
            label: None,
            deadline: None,
            gen_sinks: Vec::new(),
            depth: 0,
            short: false,
            hist: vec![(url.to_owned(), Value::Null)],
            hist_pos: 0,
            url_events: Vec::new(),
            history_go: None,
            modules: Vec::new(),
            module_map: FxHashMap::default(),
            module_sources: FxHashMap::default(),
            import_map: Vec::new(),
            mut_regs: Vec::new(),
            mut_queue: Vec::new(),
            geo_regs: Vec::new(),
            geo_pending: false,
            ce_defs: Vec::new(),
            ce_upgraded: FxHashMap::default(),
            ce_upgrading: Vec::new(),
            ce_this: None,
            ce_when: Vec::new(),
            scroll_y: 0.0,
            scroll_request: None,
            elem_scroll: FxHashMap::default(),
            elem_scroll_dirty: false,
            scroll_metrics: FxHashMap::default(),
            layout_src: None,
            geom_version: (u64::MAX, 0, 0),
            styles: None,
            co_stack: Vec::new(),
            script_running: 0,
            terminating: false,
            vm_pool: Vec::new(),
            owned_ports: Default::default(),
            frame_outbox: Vec::new(),
            vm_env_pool: Vec::new(),
            images: FxHashMap::default(),
            img_fired: FxHashMap::default(),
            link_fired: FxHashMap::default(),
            script_queue: Default::default(),
            script_sources: None,
            load_events_pending: false,
            unhandled_rejections: Vec::new(),
            nav: None,
            submit_form: None,
            requests: Vec::new(),
        });
        builtins::install(&mut it);
        bigint::install(&mut it);
        iter::install(&mut it);
        proxy::install(&mut it);
        cssom::install(&mut it);
        binary::install(&mut it);
        dombind::install(&mut it);
        observe::install(&mut it);
        custom::install(&mut it);
        web::install(&mut it);
        messaging::install(&mut it);
        wasm::api::install(&mut it);
        if let Err(e) = it.run_script(include_str!("prelude.js")) {
            eprintln!("js prelude: {e}");
        }
        it
    }

    // ------------------------------------------------------------- objects

    pub fn new_obj(&self, proto: Option<ObjRef>, kind: Kind) -> ObjRef {
        Rc::new(RefCell::new(Obj { props: Props::new(shape_class(&kind)), proto, kind }))
    }

    pub fn plain(&self) -> ObjRef {
        self.new_obj(self.p.object.clone(), Kind::Plain)
    }

    pub fn array(&self, v: Vec<Value>) -> Value {
        Value::Obj(self.new_obj(self.p.array.clone(), Kind::Array(v)))
    }

    pub fn native(&self, f: NativeFn, name: &'static str) -> Value {
        let o = self.new_obj(self.p.function.clone(), Kind::Func(Func::Native(f, name)));
        func_props(&o, Rc::from(name), builtins::native_arity(name));
        Value::Obj(o)
    }

    pub fn set_prop(o: &ObjRef, k: &str, v: Value) {
        o.borrow_mut().props.insert(Rc::from(k), Prop::data(v));
    }

    pub fn set_hidden(o: &ObjRef, k: &str, v: Value) {
        o.borrow_mut().props.insert(Rc::from(k), Prop { v, get: None, set: None, enumerable: false });
    }

    pub fn def_method(&self, o: &ObjRef, name: &'static str, f: NativeFn) {
        Self::set_hidden(o, name, self.native(f, name));
    }

    pub fn def_getter(&self, o: &ObjRef, name: &'static str, f: NativeFn) {
        let g = self.native(f, name);
        o.borrow_mut().props.insert(Rc::from(name), Prop { v: Value::Undef, get: Some(g), set: None, enumerable: false });
    }

    pub fn is_callable(v: &Value) -> bool {
        match v {
            Value::Obj(o) => match &o.borrow().kind {
                Kind::Func(_) => true,
                Kind::Proxy(p) => Self::is_callable(&p.target),
                _ => false,
            },
            _ => false,
        }
    }

    #[inline]
    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.terminating {
            return Err(Value::str(TERMINATE));
        }
        if self.steps & 0xFFFF == 0 && (self.steps > self.budget || self.deadline.is_some_and(|d| std::time::Instant::now() > d)) {
            // Stop the whole task: scripts can't catch this.
            self.terminating = true;
            if trace_scripts() {
                eprintln!("[script] task stopped after {} steps ({})", self.steps, self.url);
            }
            if trace_on() {
                self.log(&format!("[trace] task stopped (time limit){}", trace_stack()));
            }
            return Err(Value::str(TERMINATE));
        }
        Ok(())
    }

    /// Start a new task's time limit (and end any termination in progress).
    pub fn set_deadline(&mut self, d: std::time::Instant) {
        self.deadline = Some(d);
        self.terminating = false;
    }

    // ---------------------------------------------------------- conversion

    pub fn to_str(&mut self, v: &Value) -> R<Rc<str>> {
        Ok(match v {
            Value::Undef | Value::Hole => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => if *b { "true".into() } else { "false".into() },
            Value::Num(n) => num_to_str(*n).into(),
            Value::BigInt(n) => n.to_string().into(),
            Value::Str(s) => s.clone(),
            Value::Obj(_) => {
                let p = self.to_primitive(v, true)?;
                return self.to_str(&p);
            }
        })
    }

    pub fn to_num(&mut self, v: &Value) -> R<f64> {
        Ok(match v {
            Value::Undef | Value::Hole => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => *b as u8 as f64,
            Value::Num(n) => *n,
            // (A TypeError for implicit conversion per spec; Number(b) is
            // handled by the Number constructor.)
            Value::BigInt(n) => n.get() as f64,
            Value::Str(s) => str_to_num(s),
            Value::Obj(_) => {
                let p = self.to_primitive(v, false)?;
                return self.to_num(&p);
            }
        })
    }

    pub fn to_int(&mut self, v: &Value) -> R<i64> {
        let n = self.to_num(v)?;
        Ok(if n.is_nan() { 0 } else { n.trunc().clamp(-9e18, 9e18) as i64 })
    }

    pub fn to_primitive(&mut self, v: &Value, prefer_string: bool) -> R<Value> {
        let Value::Obj(o) = v else { return Ok(v.clone()) };
        let order = if prefer_string { ["toString", "valueOf"] } else { ["valueOf", "toString"] };
        for m in order {
            let f = self.get(v, m)?;
            if Self::is_callable(&f) {
                let r = self.call(&f, v.clone(), &[])?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        let _ = o;
        Ok(Value::str("[object Object]"))
    }

    pub fn key(&mut self, v: &Value) -> R<Rc<str>> {
        match v {
            Value::Str(s) => Ok(s.clone()),
            Value::Num(n) => Ok(num_to_str(*n).into()),
            _ => self.to_str(v),
        }
    }

    pub fn type_of(v: &Value) -> &'static str {
        match v {
            Value::Undef | Value::Hole => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::BigInt(_) => "bigint",
            Value::Str(s) => if is_symbol_str(s) { "symbol" } else { "string" },
            Value::Obj(_) => {
                if Self::is_callable(v) { "function" } else { "object" }
            }
        }
    }

    // ---------------------------------------------------------- properties

    fn proto_of_prim(&self, v: &Value) -> Option<ObjRef> {
        match v {
            Value::Str(s) if is_symbol_str(s) => self.p.symbol.clone(),
            Value::Str(_) => self.p.string.clone(),
            Value::Num(_) => self.p.number.clone(),
            Value::BigInt(_) => self.p.bigint.clone(),
            Value::Bool(_) => self.p.boolean.clone(),
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        }
    }

    pub fn get(&mut self, target: &Value, key: &str) -> R<Value> {
        if key == "__proto__" {
            if let Value::Obj(o) = target {
                if !matches!(o.borrow().kind, Kind::Proxy(_)) {
                    return Ok(o.borrow().proto.clone().map_or(Value::Null, Value::Obj));
                }
            }
        }
        // Fast paths for arrays and strings.
        match target {
            Value::Undef | Value::Null => {
                return Err(type_error(format!("Cannot read properties of {:?} (reading '{key}')", target)));
            }
            Value::Str(s) => {
                if key == "length" {
                    return Ok(Value::Num(utf16_len(s) as f64));
                }
                if let Some(i) = array_index(key) {
                    return Ok(utf16_at(s, i).map(|c| Value::Str(c.into())).unwrap_or_default());
                }
            }
            Value::Obj(o) => {
                if matches!(o.borrow().kind, Kind::Proxy(_)) {
                    return proxy::get(self, o, key, target);
                }
                {
                    let ob = o.borrow();
                    match &ob.kind {
                        Kind::Array(a) => {
                            if key == "length" {
                                return Ok(Value::Num(a.len() as f64));
                            }
                            if let Some(i) = array_index(key) {
                                return Ok(a.get(i).cloned().unwrap_or_default().unhole());
                            }
                        }
                        Kind::Typed(t) => {
                            if key == "length" {
                                return Ok(Value::Num(t.len as f64));
                            }
                            if let Some(i) = array_index(key) {
                                return Ok(t.get(i).map_or(Value::Undef, Value::Num));
                            }
                        }
                        Kind::Node(_) | Kind::Style(_) | Kind::ClassList(_) | Kind::Dataset(_) => {}
                        _ => {}
                    }
                }
                let special = matches!(o.borrow().kind, Kind::Node(_) | Kind::Style(_) | Kind::ClassList(_) | Kind::Dataset(_));
                if special && !self.js_override(o, key) {
                    if let Some(v) = dombind::get(self, o, key)? {
                        return Ok(v);
                    }
                }
                // Lazily created `prototype` for user functions.
                if key == "prototype" {
                    let needs = {
                        let ob = o.borrow();
                        matches!(&ob.kind, Kind::Func(Func::Closure { def, .. }) if !def.arrow) && ob.props.get("prototype").is_none()
                    };
                    if needs {
                        let proto = self.plain();
                        Self::set_hidden(&proto, "constructor", target.clone());
                        Self::set_hidden(o, "prototype", Value::Obj(proto.clone()));
                        return Ok(Value::Obj(proto));
                    }
                }
            }
            _ => {}
        }
        let mut cur = self.proto_of_prim(target);
        let mut depth = 0;
        while let Some(o) = cur {
            let (found, next) = {
                let ob = o.borrow();
                (ob.props.get(key).cloned(), ob.proto.clone())
            };
            if let Some(p) = found {
                if let Some(g) = p.get {
                    return self.call(&g, target.clone(), &[]);
                }
                if p.set.is_some() {
                    return Ok(Value::Undef);
                }
                return Ok(p.v);
            }
            cur = next;
            depth += 1;
            if depth > 1000 {
                break;
            }
        }
        Ok(Value::Undef)
    }

    pub fn get_v(&mut self, target: &Value, key: &Value) -> R<Value> {
        if let (Value::Obj(o), Value::Num(n)) = (target, key) {
            match &o.borrow().kind {
                Kind::Array(a) if *n >= 0.0 && n.fract() == 0.0 => {
                    return Ok(a.get(*n as usize).cloned().unwrap_or_default().unhole());
                }
                Kind::Typed(t) if *n >= 0.0 && n.fract() == 0.0 => {
                    return Ok(t.get(*n as usize).map_or(Value::Undef, Value::Num));
                }
                _ => {}
            }
        }
        let k = self.key(key)?;
        self.get(target, &k)
    }

    pub fn put(&mut self, target: &Value, key: &str, v: Value) -> R<()> {
        let Value::Obj(o) = target else {
            if target.is_nullish() {
                return Err(type_error(format!("Cannot set properties of {:?} (setting '{key}')", target)));
            }
            return Ok(());
        };
        if matches!(o.borrow().kind, Kind::Proxy(_)) {
            return proxy::set(self, o, key, v, target);
        }
        if key == "__proto__" {
            // obj.__proto__ = p sets the prototype (ignoring cycles / primitives).
            match v {
                Value::Obj(p) => {
                    let mut cur = Some(p.clone());
                    while let Some(c) = cur {
                        if Rc::ptr_eq(&c, o) {
                            return Err(type_error("Cyclic __proto__ value"));
                        }
                        cur = c.borrow().proto.clone();
                    }
                    o.borrow_mut().proto = Some(p);
                }
                Value::Null => o.borrow_mut().proto = None,
                _ => {}
            }
            return Ok(());
        }
        let typed = match &o.borrow().kind {
            Kind::Typed(t) => Some(t.clone()),
            _ => None,
        };
        if let Some(t) = typed {
            if let Some(i) = array_index(key) {
                let x = self.to_num(&v)?;
                t.set(i, x);
                return Ok(());
            }
            if key == "length" {
                return Ok(());
            }
        }
        {
            let mut ob = o.borrow_mut();
            if let Kind::Array(a) = &mut ob.kind {
                if let Some(i) = array_index(key) {
                    if i >= a.len() {
                        if i > a.len() + 10_000_000 {
                            return Err(builtins::make_error_value("RangeError", "array too large"));
                        }
                        a.resize(i + 1, Value::Hole);
                    }
                    a[i] = v;
                    return Ok(());
                }
                if key == "length" {
                    drop(ob);
                    let n = self.to_num(&v)? as usize;
                    if let Kind::Array(a) = &mut o.borrow_mut().kind {
                        a.resize(n, Value::Hole);
                    }
                    return Ok(());
                }
            }
        }
        let special = matches!(o.borrow().kind, Kind::Node(_) | Kind::Style(_) | Kind::ClassList(_) | Kind::Dataset(_));
        if special && !self.js_override(o, key) && dombind::set(self, o, key, &v)? {
            if self.doc.ce_log.as_ref().is_some_and(|l| !l.is_empty()) {
                self.ce_process();
            }
            return Ok(());
        }
        // Setter on the prototype chain?
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let (setter, is_own_data, next) = {
                let cb = c.borrow();
                match cb.props.get(key) {
                    Some(p) if p.set.is_some() || p.get.is_some() => (p.set.clone(), false, None),
                    Some(_) if Rc::ptr_eq(&c, o) => (None, true, None),
                    Some(_) => (None, false, None),
                    None => (None, false, cb.proto.clone()),
                }
            };
            if let Some(s) = setter {
                self.call(&s, target.clone(), &[v])?;
                return Ok(());
            }
            if is_own_data {
                if let Some(p) = o.borrow_mut().props.get_mut(key) {
                    p.v = v;
                }
                return Ok(());
            }
            cur = next;
        }
        o.borrow_mut().props.insert(Rc::from(key), Prop::data(v));
        Ok(())
    }

    pub fn put_v(&mut self, target: &Value, key: &Value, v: Value) -> R<()> {
        if let (Value::Obj(o), Value::Num(n)) = (target, key) {
            let typed = match &o.borrow().kind {
                Kind::Typed(t) if *n >= 0.0 && n.fract() == 0.0 => Some(t.clone()),
                _ => None,
            };
            if let Some(t) = typed {
                let x = self.to_num(&v)?;
                t.set(*n as usize, x);
                return Ok(());
            }
            let mut ob = o.borrow_mut();
            if let Kind::Array(a) = &mut ob.kind {
                if *n >= 0.0 && n.fract() == 0.0 && (*n as usize) < a.len() + 1000 {
                    let i = *n as usize;
                    if i >= a.len() {
                        a.resize(i + 1, Value::Hole);
                    }
                    a[i] = v;
                    return Ok(());
                }
            }
        }
        let k = self.key(key)?;
        self.put(target, &k, v)
    }

    /// Whether script defined `key` on a node object itself or on a
    /// prototype more derived than the built-in node prototype (a custom
    /// element class, Shady DOM's ShadyRoot...): such definitions take
    /// precedence over the native DOM property, as in browsers.
    fn js_override(&self, o: &ObjRef, key: &str) -> bool {
        if !matches!(o.borrow().kind, Kind::Node(_)) {
            return false;
        }
        if o.borrow().props.get(key).is_some_and(|p| p.get.is_some() || p.set.is_some()) {
            return true;
        }
        let base = self.p.node.as_ref();
        let mut derived = true;
        let mut cur = o.borrow().proto.clone();
        while let Some(c) = cur {
            if base.is_some_and(|b| Rc::ptr_eq(b, &c)) {
                derived = false;
            }
            let found = c.borrow().props.get(key).map(|p| (p.get.clone(), p.set.clone()));
            if let Some((g, st)) = found {
                if derived {
                    return true;
                }
                // On the built-in prototypes: the native accessors (and plain
                // data such as methods / constants) defer to the DOM layer;
                // script-installed accessors (polyfill patches) win.
                let native = |f: &Option<Value>| match f {
                    Some(Value::Obj(fo)) => fo.borrow().props.get("@@nativeAccessor").is_some(),
                    _ => true,
                };
                return (g.is_some() || st.is_some()) && !(native(&g) && native(&st));
            }
            cur = c.borrow().proto.clone();
        }
        false
    }

    /// A function's `prototype` object is created on first use; anything
    /// that inspects own properties creates it first (`'prototype' in f`).
    pub fn materialize_prototype(&mut self, v: &Value) {
        if let Value::Obj(o) = v {
            let lazy = {
                let b = o.borrow();
                matches!(&b.kind, Kind::Func(Func::Closure { def, .. }) if !def.arrow) && b.props.get("prototype").is_none()
            };
            if lazy {
                let _ = self.get(v, "prototype");
            }
        }
    }

    pub fn has_prop(&mut self, target: &Value, key: &str) -> bool {
        if key == "prototype" {
            self.materialize_prototype(target);
        }
        if let Value::Obj(o) = target {
            if matches!(o.borrow().kind, Kind::Proxy(_)) {
                return proxy::has(self, o, key);
            }
            if let Kind::Array(a) = &o.borrow().kind {
                if let Some(i) = array_index(key) {
                    return i < a.len() && !matches!(a[i], Value::Hole);
                }
                if key == "length" {
                    return true;
                }
            }
            if let Kind::Typed(t) = &o.borrow().kind {
                if let Some(i) = array_index(key) {
                    return i < t.len;
                }
            }
            if matches!(o.borrow().kind, Kind::Node(_)) && dombind::has(self, o, key) {
                return true;
            }
        }
        let mut cur = self.proto_of_prim(target);
        while let Some(o) = cur {
            if o.borrow().props.get(key).is_some() {
                return true;
            }
            cur = o.borrow().proto.clone();
        }
        false
    }

    /// Own enumerable string keys (arrays include their indices).
    pub fn own_keys(&mut self, v: &Value) -> Vec<Rc<str>> {
        match v {
            Value::Str(s) => (0..utf16_len(s)).map(|i| Rc::from(i.to_string())).collect(),
            Value::Obj(o) if matches!(o.borrow().kind, Kind::Proxy(_)) => proxy::own_keys(self, o),
            Value::Obj(o) => {
                let ob = o.borrow();
                let mut out: Vec<Rc<str>> = Vec::new();
                match &ob.kind {
                    Kind::Array(a) => out.extend((0..a.len()).filter(|&i| !matches!(a[i], Value::Hole)).map(|i| Rc::from(i.to_string()))),
                    Kind::Typed(t) => out.extend((0..t.len).map(|i| Rc::from(i.to_string()))),
                    _ => {}
                }
                out.extend(ob.props.iter().filter(|(k, p)| p.enumerable && !k.starts_with("@@")).map(|(k, _)| k.clone()));
                out
            }
            _ => Vec::new(),
        }
    }

    /// All own string-keyed property names (getOwnPropertyNames), or the
    /// symbol keys (getOwnPropertySymbols). Engine-internal `@@` keys are
    /// in neither.
    pub fn own_property_keys(&mut self, v: &Value, symbols: bool) -> Vec<Rc<str>> {
        self.materialize_prototype(v);
        let is_symbol = |k: &str| is_symbol_str(k);
        match v {
            Value::Str(s) if !symbols => {
                let mut out: Vec<Rc<str>> = (0..utf16_len(s)).map(|i| Rc::from(i.to_string())).collect();
                out.push("length".into());
                out
            }
            Value::Obj(o) if matches!(o.borrow().kind, Kind::Proxy(_)) => if symbols { Vec::new() } else { proxy::own_keys(self, o) },
            Value::Obj(o) => {
                let ob = o.borrow();
                let mut out: Vec<Rc<str>> = Vec::new();
                if !symbols {
                    match &ob.kind {
                        Kind::Array(a) => {
                            out.extend((0..a.len()).filter(|&i| !matches!(a[i], Value::Hole)).map(|i| Rc::from(i.to_string())));
                            out.push("length".into());
                        }
                        Kind::Typed(t) => out.extend((0..t.len).map(|i| Rc::from(i.to_string()))),
                        _ => {}
                    }
                }
                out.extend(
                    ob.props
                        .iter()
                        .filter(|(k, _)| if symbols { is_symbol(k) } else { !k.starts_with("@@") })
                        .map(|(k, _)| k.clone()),
                );
                out
            }
            _ => Vec::new(),
        }
    }

    // --------------------------------------------------------------- calls

    pub fn call(&mut self, f: &Value, this: Value, args: &[Value]) -> R<Value> {
        self.tick()?;
        let Value::Obj(fo) = f else {
            if trace_on() {
                self.log(&format!("[trace] call of {f:?}{}", trace_stack()));
            }
            return Err(type_error(format!("{:?} is not a function", f)));
        };
        enum C {
            Closure(Rc<FuncDef>, Env, Option<Value>, Option<ObjRef>),
            Class,
            Native(NativeFn),
            Bound(Value, Value, Vec<Value>),
            Proxy,
        }
        let c = match &fo.borrow().kind {
            Kind::Proxy(_) => C::Proxy,
            Kind::Func(Func::Closure { def, env, this: t, home }) => C::Closure(def.clone(), env.clone(), t.clone(), home.clone()),
            Kind::Func(Func::Class(_)) => C::Class,
            Kind::Func(Func::Native(n, _)) => C::Native(*n),
            Kind::Func(Func::Bound(t, th, a)) => C::Bound(t.clone(), th.clone(), a.clone()),
            _ => return Err(type_error("not a function")),
        };
        match c {
            C::Native(n) => {
                let saved = std::mem::replace(&mut self.constructing, false);
                let r = n(self, this, args);
                self.constructing = saved;
                // Custom element reactions for DOM changes the call made.
                if self.doc.ce_log.as_ref().is_some_and(|l| !l.is_empty()) {
                    self.ce_process();
                }
                r
            }
            C::Bound(t, th, mut a) => {
                a.extend_from_slice(args);
                self.call(&t, th, &a)
            }
            C::Class => Err(type_error("Class constructor cannot be invoked without 'new'")),
            C::Proxy => proxy::apply(self, fo, this, args),
            C::Closure(def, env, arrow_this, home) => {
                if (def.is_gen || def.is_async) && coro::supported() {
                    let st = GenStart { def: def.clone(), env: env.clone(), home: home.clone(), this: this.clone(), args: args.to_vec(), fobj: fo.clone() };
                    let v = if def.is_gen { self.new_coro_gen(st, def.is_async) } else { self.start_async(st) };
                    if let Some(v) = v {
                        return Ok(v);
                    }
                }
                if def.is_gen {
                    let g = Gen {
                        start: Some(GenStart { def: def.clone(), env, home, this, args: args.to_vec(), fobj: fo.clone() }),
                        is_async: def.is_async,
                        ..Gen::default()
                    };
                    let _ = arrow_this;
                    return Ok(Value::Obj(self.new_obj(self.p.generator.clone(), Kind::Generator(Box::new(g)))));
                }
                if def.is_async {
                    // Run synchronously; wrap the outcome in a promise.
                    let r = self.call_closure(&def, env, arrow_this, home, this, args, Some(fo.clone()));
                    return Ok(match r {
                        Ok(v) => builtins::promise_resolved(self, v),
                        Err(e) => builtins::promise_rejected(self, e),
                    });
                }
                self.call_closure(&def, env, arrow_this, home, this, args, Some(fo.clone()))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call_closure(&mut self, def: &Rc<FuncDef>, env: Env, arrow_this: Option<Value>, home: Option<ObjRef>, this: Value, args: &[Value], fobj: Option<ObjRef>) -> R<Value> {
        let limit = if self.co_stack.is_empty() { MAX_DEPTH } else { coro::MAX_DEPTH };
        if self.depth >= limit {
            return Err(builtins::make_error_value("RangeError", "Maximum call stack size exceeded"));
        }
        self.depth += 1;
        let r = if prof::on() {
            self.call_closure_profiled(def, env, arrow_this, home, this, args, fobj)
        } else if trace_on() || trace_throw_on() {
            TRACE_STACK.with(|s| s.borrow_mut().push(def.clone()));
            let r = self.call_closure_inner(def, env, arrow_this, home, this, args, fobj);
            TRACE_STACK.with(|s| s.borrow_mut().pop());
            r
        } else {
            self.call_closure_inner(def, env, arrow_this, home, this, args, fobj)
        };
        self.depth -= 1;
        r
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn call_closure_profiled(&mut self, def: &Rc<FuncDef>, env: Env, arrow_this: Option<Value>, home: Option<ObjRef>, this: Value, args: &[Value], fobj: Option<ObjRef>) -> R<Value> {
        prof::enter();
        let r = self.call_closure_inner(def, env, arrow_this, home, this, args, fobj);
        prof::exit(def);
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn call_closure_inner(&mut self, def: &Rc<FuncDef>, env: Env, arrow_this: Option<Value>, home: Option<ObjRef>, this: Value, args: &[Value], fobj: Option<ObjRef>) -> R<Value> {
        if let Some(code) = self.vm_code(def) {
            return self.vm_call(&code, env, this, args, fobj, home);
        }
        // Sloppy-mode functions see the global object for a missing `this`.
        let this = if !def.strict && this.is_nullish() { Value::Obj(self.global.clone()) } else { this };
        let scope = if def.arrow {
            let _ = arrow_this;
            new_scope(Some(env), None, None, true)
        } else {
            new_scope(Some(env), Some(this), home.or(fobj.clone()), true)
        };
        if !def.arrow {
            if let Some(name) = &def.name {
                // Named function expressions can refer to themselves.
                if let Some(f) = &fobj {
                    scope.vars.borrow_mut().entry(name.clone()).or_insert(Value::Obj(f.clone()));
                }
            }
            scope.vars.borrow_mut().insert("arguments".into(), self.array(args.to_vec()));
        }
        for (i, p) in def.params.iter().enumerate() {
            let v = args.get(i).cloned().unwrap_or_default();
            self.bind(p, v, &scope, Some(VarKind::Let))?;
        }
        if let Some(r) = &def.rest {
            let rest = self.array(args.get(def.params.len()..).map(|s| s.to_vec()).unwrap_or_default());
            self.bind(r, rest, &scope, Some(VarKind::Let))?;
        }
        match &def.body {
            Body::Expr(e) => self.eval(e, &scope),
            Body::Block(stmts) => {
                self.hoist(stmts, &scope)?;
                for s in stmts {
                    match self.exec(s, &scope)? {
                        Flow::Ret(v) => return Ok(v),
                        Flow::Normal => {}
                        _ => break,
                    }
                }
                Ok(Value::Undef)
            }
        }
    }

    pub fn construct(&mut self, f: &Value, args: &[Value]) -> R<Value> {
        let Value::Obj(fo) = f else { return Err(type_error("not a constructor")) };
        if matches!(fo.borrow().kind, Kind::Proxy(_)) {
            return proxy::construct(self, fo, args);
        }
        let is_class = matches!(fo.borrow().kind, Kind::Func(Func::Class(_)));
        let native = matches!(fo.borrow().kind, Kind::Func(Func::Native(..)));
        if native {
            let Kind::Func(Func::Native(n, _)) = fo.borrow().kind else { unreachable!() };
            let saved = std::mem::replace(&mut self.constructing, true);
            let r = n(self, Value::Undef, args);
            self.constructing = saved;
            return r;
        }
        if let Kind::Func(Func::Bound(t, _, a)) = &fo.borrow().kind {
            let (t, mut a) = (t.clone(), a.clone());
            a.extend_from_slice(args);
            return self.construct(&t, &a);
        }
        let proto = match self.get(f, "prototype")? {
            Value::Obj(p) => Some(p),
            _ => self.p.object.clone(),
        };
        let obj = Value::Obj(self.new_obj(proto, Kind::Plain));
        let r = if is_class { self.init_class(fo, &obj, args)? } else { self.call(f, obj.clone(), args)? };
        Ok(if matches!(r, Value::Obj(_)) { r } else { obj })
    }

    /// Run a class constructor (and its parents') on `this`.
    fn init_class(&mut self, fo: &ObjRef, this: &Value, args: &[Value]) -> R<Value> {
        let info = match &fo.borrow().kind {
            Kind::Func(Func::Class(ci)) => ci.clone(),
            _ => return self.call(&Value::Obj(fo.clone()), this.clone(), args),
        };
        let proto = self.get(&Value::Obj(fo.clone()), "prototype")?;
        match &info.def.ctor {
            None => {
                if let Some(p) = &info.parent {
                    self.super_call(p, this, args)?;
                }
                self.init_fields(&info, this, proto.obj().cloned())?;
                Ok(Value::Undef)
            }
            Some(ctor) => {
                if info.parent.is_none() {
                    self.init_fields(&info, this, proto.obj().cloned())?;
                }
                let scope_env = info.env.clone();
                // `super(...)` inside looks up the class via the scope's func.
                let r = self.call_closure(ctor, scope_env, None, proto.obj().cloned(), this.clone(), args, Some(fo.clone()));
                r.map(|v| if matches!(v, Value::Obj(_)) { v } else { Value::Undef })
            }
        }
    }

    fn super_call(&mut self, parent: &Value, this: &Value, args: &[Value]) -> R<()> {
        let Value::Obj(po) = parent else { return Ok(()) };
        let kind = match &po.borrow().kind {
            Kind::Func(Func::Class(_)) => 0,
            Kind::Func(Func::Closure { .. }) => 1,
            Kind::Func(Func::Native(..)) => 2,
            _ => 3,
        };
        match kind {
            0 => {
                self.init_class(po, this, args)?;
            }
            1 => {
                self.call(parent, this.clone(), args)?;
            }
            2 => {
                // Native parents (Error, Map, HTMLElement...): build an
                // instance and adopt its state.
                self.ce_this = Some(this.clone());
                let inst = self.construct(parent, args);
                self.ce_this = None;
                let inst = inst?;
                if let (Value::Obj(src), Value::Obj(dst)) = (&inst, this) {
                    if Rc::ptr_eq(src, dst) {
                        return Ok(()); // a custom element upgrade: already `this`
                    }
                    let (props, kind) = {
                        let mut s = src.borrow_mut();
                        (std::mem::take(&mut s.props), std::mem::replace(&mut s.kind, Kind::Plain))
                    };
                    let node = if let Kind::Node(n) = kind { Some(n) } else { None };
                    {
                        let mut d = dst.borrow_mut();
                        for (k, p) in props.into_entries() {
                            d.props.insert(k, p);
                        }
                        d.kind = kind;
                        let class = shape_class(&d.kind);
                        let old = std::mem::take(&mut d.props);
                        d.props = old.reclass(class);
                    }
                    if let Some(n) = node {
                        // `this` is now the element's wrapper.
                        self.node_objs.insert(n, dst.clone());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn init_fields(&mut self, info: &Rc<ClassInfo>, this: &Value, proto: Option<ObjRef>) -> R<()> {
        if info.def.fields.iter().all(|f| f.0) {
            return Ok(());
        }
        let scope = new_scope(Some(info.env.clone()), Some(this.clone()), proto, true);
        for (is_static, key, init) in &info.def.fields {
            if *is_static {
                continue;
            }
            let k = self.prop_key(key, &scope)?;
            let v = match init {
                Some(e) => self.eval(e, &scope)?,
                None => Value::Undef,
            };
            self.put(this, &k, v)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------- scoping

    pub fn lookup(&mut self, name: &str, env: &Env) -> R<Value> {
        let mut cur = Some(env);
        while let Some(s) = cur {
            let found = s.vars.borrow().get(name).cloned();
            if let Some(v) = found {
                if let Value::Str(m) = &v {
                    if m.starts_with(module::IMPORT_MARK) {
                        return self.import_binding(m);
                    }
                }
                return Ok(v);
            }
            cur = s.parent.as_ref();
        }
        let g = Value::Obj(self.global.clone());
        if self.has_prop(&g, name) {
            return self.get(&g, name);
        }
        Err(builtins::make_error_value("ReferenceError", &format!("{name} is not defined")))
    }

    fn assign_var(&mut self, name: &Rc<str>, v: Value, env: &Env) -> R<()> {
        let mut cur = Some(env);
        while let Some(s) = cur {
            if let Some(slot) = s.vars.borrow_mut().get_mut(name) {
                *slot = v;
                return Ok(());
            }
            cur = s.parent.as_ref();
        }
        self.put(&Value::Obj(self.global.clone()), name, v)
    }

    fn func_scope(env: &Env) -> &Env {
        let mut cur = env;
        while !cur.is_func {
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        cur
    }

    fn this_val(env: &Env) -> Value {
        let mut cur = Some(env);
        while let Some(s) = cur {
            if let Some(t) = &s.this {
                return t.clone();
            }
            cur = s.parent.as_ref();
        }
        Value::Undef
    }

    fn home_of(env: &Env) -> Option<ObjRef> {
        let mut cur = Some(env);
        while let Some(s) = cur {
            if s.this.is_some() {
                return s.func.clone();
            }
            cur = s.parent.as_ref();
        }
        None
    }

    /// Hoist `var`s and function declarations of a function body.
    fn hoist(&mut self, stmts: &[Stmt], env: &Env) -> R<()> {
        fn vars(s: &Stmt, out: &mut Vec<Rc<str>>) {
            match s {
                Stmt::Var(VarKind::Var, ds) => {
                    for (p, _) in ds {
                        pat_names(p, out);
                    }
                }
                Stmt::If(_, a, b) => {
                    vars(a, out);
                    if let Some(b) = b {
                        vars(b, out);
                    }
                }
                Stmt::Block(b) => b.iter().for_each(|s| vars(s, out)),
                Stmt::For(i, _, _, b) => {
                    if let Some(i) = i {
                        vars(i, out);
                    }
                    vars(b, out);
                }
                Stmt::ForIn(ForInit::Var(VarKind::Var, p), _, b) | Stmt::ForOf(ForInit::Var(VarKind::Var, p), _, b) => {
                    pat_names(p, out);
                    vars(b, out);
                }
                Stmt::ForIn(_, _, b) | Stmt::ForOf(_, _, b) | Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) => vars(b, out),
                Stmt::Try(a, c, f) => {
                    a.iter().for_each(|s| vars(s, out));
                    if let Some((_, c)) = c {
                        c.iter().for_each(|s| vars(s, out));
                    }
                    if let Some(f) = f {
                        f.iter().for_each(|s| vars(s, out));
                    }
                }
                Stmt::Switch(_, cases) => cases.iter().for_each(|(_, b)| b.iter().for_each(|s| vars(s, out))),
                _ => {}
            }
        }
        let mut names = Vec::new();
        for s in stmts {
            vars(s, &mut names);
        }
        let fs = Self::func_scope(env).clone();
        if Rc::ptr_eq(&fs, &self.genv) {
            let g = Value::Obj(self.global.clone());
            for n in names {
                if !fs.vars.borrow().contains_key(&n) && !self.has_prop(&g, &n) {
                    self.put(&g, &n, Value::Undef)?;
                }
            }
        } else {
            let mut v = fs.vars.borrow_mut();
            for n in names {
                v.entry(n).or_insert(Value::Undef);
            }
        }
        self.declare_functions(stmts, env)
    }

    fn declare_functions(&mut self, stmts: &[Stmt], env: &Env) -> R<()> {
        for s in stmts {
            if let Stmt::Func(def) = s {
                let f = self.make_closure(def, env);
                let name = def.name.clone().unwrap();
                if Rc::ptr_eq(env, &self.genv) && !env.vars.borrow().contains_key(&name) {
                    self.put(&Value::Obj(self.global.clone()), &name, f)?;
                } else {
                    env.vars.borrow_mut().insert(name, f);
                }
            }
        }
        Ok(())
    }

    pub fn make_closure(&mut self, def: &Rc<FuncDef>, env: &Env) -> Value {
        let this = if def.arrow { Some(Self::this_val(env)) } else { None };
        let home = if def.arrow { Self::home_of(env) } else { None };
        let f = self.new_obj(self.p.function.clone(), Kind::Func(Func::Closure { def: def.clone(), env: env.clone(), this, home }));
        func_props(&f, def.name.clone().unwrap_or_else(|| Rc::from("")), formal_length(def));
        Value::Obj(f)
    }

    // ---------------------------------------------------------- statements

    pub fn run_script(&mut self, src: &str) -> Result<(), String> {
        let t = std::time::Instant::now();
        let prog = parse_program(src)?;
        if src.len() > 100_000 && std::env::var_os("BOLT_PROFILE").is_some() {
            self.log(&format!("[profile] parsed {} bytes in {:.1}ms", src.len(), t.elapsed().as_secs_f64() * 1000.0));
        }
        let env = self.genv.clone();
        // Scripts with loops run on the bytecode VM (in the global scope).
        let def = Rc::new(FuncDef { name: None, params: Vec::new(), rest: None, body: Body::Block(prog), arrow: false, is_async: false, is_gen: false, strict: false, vm: Default::default() });
        let Body::Block(prog) = &def.body else { unreachable!() };
        self.script_running += 1;
        let r = (|| -> R<()> {
            self.hoist(prog, &env)?;
            if let Some(code) = vm::compile_script(&def) {
                let code = Rc::new(code);
                self.vm_script(&code)?;
                return Ok(());
            }
            for s in prog {
                self.exec(s, &env)?;
            }
            Ok(())
        })();
        self.script_running -= 1;
        r.map_err(|e| self.describe_error(&e))
    }

    /// Indirect `eval`: run `src` as a global script and return the value of
    /// its last expression statement.
    pub fn eval_global(&mut self, src: &str) -> R<Value> {
        let prog = parse_program(src).map_err(|e| builtins::make_error_value("SyntaxError", &e))?;
        let env = self.genv.clone();
        self.script_running += 1;
        let r = (|| -> R<Value> {
            self.hoist(&prog, &env)?;
            let mut last = Value::Undef;
            for s in &prog {
                match s {
                    Stmt::Expr(e) => last = self.eval(e, &env)?,
                    s => {
                        self.exec(s, &env)?;
                    }
                }
            }
            Ok(last)
        })();
        self.script_running -= 1;
        r
    }

    /// Run `src` as a function body (developer aid: `return` a value) and
    /// stringify the result.
    pub fn eval_str(&mut self, src: &str) -> String {
        let wrapped = format!("globalThis.__bolt_eval = (function () {{
{src}
}})();");
        if let Err(e) = self.run_script(&wrapped) {
            return format!("error: {e}");
        }
        let g = Value::Obj(self.global.clone());
        let v = self.get(&g, "__bolt_eval").unwrap_or_default();
        self.to_str(&v).map(|s| s.to_string()).unwrap_or_else(|e| self.describe_error(&e))
    }

    pub fn describe_error(&mut self, e: &Value) -> String {
        if matches!(e, Value::Str(s) if &**s == TERMINATE) {
            return "Error: script took too long and was stopped".into();
        }
        if let Value::Obj(_) = e {
            let name = self.get(e, "name").ok().and_then(|v| if let Value::Str(s) = v { Some(s) } else { None });
            let msg = self.get(e, "message").ok().and_then(|v| if let Value::Str(s) = v { Some(s) } else { None });
            if let (Some(n), Some(m)) = (name, msg) {
                return format!("{n}: {m}");
            }
        }
        format!("Uncaught {:?}", e)
    }

    fn block(&mut self, stmts: &[Stmt], env: &Env) -> R<Flow> {
        let lexical = stmts.iter().any(|s| matches!(s, Stmt::Var(VarKind::Let | VarKind::Const, _) | Stmt::Class(_) | Stmt::Func(_)));
        let scope = if lexical { new_scope(Some(env.clone()), None, None, false) } else { env.clone() };
        if lexical {
            self.declare_functions(stmts, &scope)?;
        }
        for s in stmts {
            match self.exec(s, &scope)? {
                Flow::Normal => {}
                f => return Ok(f),
            }
        }
        Ok(Flow::Normal)
    }

    pub fn exec(&mut self, s: &Stmt, env: &Env) -> R<Flow> {
        self.tick()?;
        match s {
            Stmt::Expr(e) => {
                self.eval(e, env)?;
            }
            Stmt::Var(kind, decls) => {
                for (p, init) in decls {
                    let v = match init {
                        Some(e) => {
                            let v = self.eval(e, env)?;
                            name_fn(&v, p);
                            v
                        }
                        None if *kind == VarKind::Var => continue,
                        None => Value::Undef,
                    };
                    self.bind(p, v, env, Some(*kind))?;
                }
            }
            Stmt::Func(_) => {}
            Stmt::Class(cd) => {
                let c = self.make_class(cd, env)?;
                env.vars.borrow_mut().insert(cd.name.clone().unwrap(), c);
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undef,
                };
                return Ok(Flow::Ret(v));
            }
            Stmt::If(c, a, b) => {
                if self.eval(c, env)?.truthy() {
                    return self.exec(a, env);
                } else if let Some(b) = b {
                    return self.exec(b, env);
                }
            }
            Stmt::Block(b) => return self.block(b, env),
            Stmt::For(init, test, update, body) => {
                let my = self.label.take();
                // Fresh per-iteration bindings only matter if closures can capture them.
                let per_iter = matches!(init.as_deref(), Some(Stmt::Var(VarKind::Let | VarKind::Const, _))) && stmt_has_closure(body);
                let mut scope = new_scope(Some(env.clone()), None, None, false);
                if let Some(i) = init {
                    self.exec(i, &scope)?;
                }
                loop {
                    if let Some(t) = test {
                        if !self.eval(t, &scope)?.truthy() {
                            break;
                        }
                    }
                    match self.exec(body, &scope)? {
                        Flow::Brk(None) => break,
                        Flow::Cont(None) | Flow::Normal => {}
                        Flow::Brk(Some(l)) if my.as_ref() == Some(&l) => break,
                        Flow::Cont(Some(l)) if my.as_ref() == Some(&l) => {}
                        f => return Ok(f),
                    }
                    if per_iter {
                        // Fresh binding per iteration so closures capture each value.
                        let copy = new_scope(Some(env.clone()), None, None, false);
                        *copy.vars.borrow_mut() = scope.vars.borrow().clone();
                        scope = copy;
                    }
                    if let Some(u) = update {
                        self.eval(u, &scope)?;
                    }
                }
            }
            Stmt::ForIn(init, obj, body) => {
                let my = self.label.take();
                let o = self.eval(obj, env)?;
                let keys = self.for_in_keys(&o);
                for k in keys {
                    let scope = new_scope(Some(env.clone()), None, None, false);
                    self.for_bind(init, Value::Str(k), &scope)?;
                    match self.exec(body, &scope)? {
                        Flow::Brk(None) => break,
                        Flow::Cont(None) | Flow::Normal => {}
                        Flow::Brk(Some(l)) if my.as_ref() == Some(&l) => break,
                        Flow::Cont(Some(l)) if my.as_ref() == Some(&l) => {}
                        f => return Ok(f),
                    }
                }
            }
            Stmt::ForOf(init, obj, body) => {
                let my = self.label.take();
                let o = self.eval(obj, env)?;
                if let Some((iter, next)) = self.lazy_iterator(&o)? {
                    // Pull values one at a time (generators may be infinite).
                    loop {
                        let mut r = self.call(&next, iter.clone(), &[])?;
                        if matches!(&r, Value::Obj(p) if matches!(p.borrow().kind, Kind::Promise(_))) {
                            r = builtins::await_value(self, r)?;
                        }
                        if self.get(&r, "done")?.truthy() {
                            break;
                        }
                        let v = self.get(&r, "value")?;
                        let scope = new_scope(Some(env.clone()), None, None, false);
                        let flow = self.for_bind(init, v, &scope).and_then(|_| self.exec(body, &scope));
                        let stop = match flow {
                            Ok(Flow::Cont(None) | Flow::Normal) => None,
                            Ok(Flow::Cont(Some(l))) if my.as_ref() == Some(&l) => None,
                            Ok(Flow::Brk(None)) => Some(Ok(Flow::Normal)),
                            Ok(Flow::Brk(Some(l))) if my.as_ref() == Some(&l) => Some(Ok(Flow::Normal)),
                            other => Some(other),
                        };
                        if let Some(res) = stop {
                            // IteratorClose: let the iterator clean up.
                            let ret = self.get(&iter, "return")?;
                            if Self::is_callable(&ret) {
                                let closed = self.call(&ret, iter.clone(), &[]);
                                if res.is_ok() {
                                    closed?;
                                }
                            }
                            match res? {
                                Flow::Normal => break,
                                f => return Ok(f),
                            }
                        }
                    }
                    return Ok(Flow::Normal);
                }
                let items = self.iterate(&o)?;
                for v in items {
                    let scope = new_scope(Some(env.clone()), None, None, false);
                    self.for_bind(init, v, &scope)?;
                    match self.exec(body, &scope)? {
                        Flow::Brk(None) => break,
                        Flow::Cont(None) | Flow::Normal => {}
                        Flow::Brk(Some(l)) if my.as_ref() == Some(&l) => break,
                        Flow::Cont(Some(l)) if my.as_ref() == Some(&l) => {}
                        f => return Ok(f),
                    }
                }
            }
            Stmt::While(c, body) => {
                let my = self.label.take();
                while self.eval(c, env)?.truthy() {
                    match self.exec(body, env)? {
                        Flow::Brk(None) => break,
                        Flow::Cont(None) | Flow::Normal => {}
                        Flow::Brk(Some(l)) if my.as_ref() == Some(&l) => break,
                        Flow::Cont(Some(l)) if my.as_ref() == Some(&l) => {}
                        f => return Ok(f),
                    }
                }
            }
            Stmt::DoWhile(body, c) => {
                let my = self.label.take();
                loop {
                    match self.exec(body, env)? {
                        Flow::Brk(None) => break,
                        Flow::Cont(None) | Flow::Normal => {}
                        Flow::Brk(Some(l)) if my.as_ref() == Some(&l) => break,
                        Flow::Cont(Some(l)) if my.as_ref() == Some(&l) => {}
                        f => return Ok(f),
                    }
                    if !self.eval(c, env)?.truthy() {
                        break;
                    }
                }
            }
            Stmt::Break(l) => return Ok(Flow::Brk(l.clone())),
            Stmt::Continue(l) => return Ok(Flow::Cont(l.clone())),
            Stmt::Throw(e) => {
                let v = self.eval(e, env)?;
                if trace_throw() {
                    let m = self.describe_error(&v);
                    self.log(&format!("[throw] {m}{}", trace_stack()));
                }
                return Err(v);
            }
            Stmt::Try(block, catch, finally) => {
                let r = self.block(block, env);
                if self.terminating {
                    return r;
                }
                let r = match (r, catch) {
                    (Err(e), Some((param, cb))) if !generator::is_return_marker(&e) => {
                        let scope = new_scope(Some(env.clone()), None, None, false);
                        if let Some(p) = param {
                            self.bind(p, e, &scope, Some(VarKind::Let))?;
                        }
                        self.block(cb, &scope)
                    }
                    (r, _) => r,
                };
                if let Some(f) = finally {
                    match self.block(f, env)? {
                        Flow::Normal => {}
                        other => return Ok(other),
                    }
                }
                return r;
            }
            Stmt::Switch(d, cases) => {
                let v = self.eval(d, env)?;
                let scope = new_scope(Some(env.clone()), None, None, false);
                let mut matched = None;
                for (i, (test, _)) in cases.iter().enumerate() {
                    if let Some(t) = test {
                        let tv = self.eval(t, &scope)?;
                        if strict_eq(&v, &tv) {
                            matched = Some(i);
                            break;
                        }
                    }
                }
                let start = matched.or_else(|| cases.iter().position(|(t, _)| t.is_none()));
                if let Some(start) = start {
                    for (_, body) in &cases[start..] {
                        for s in body {
                            match self.exec(s, &scope)? {
                                Flow::Normal => {}
                                Flow::Brk(None) => return Ok(Flow::Normal),
                                f => return Ok(f),
                            }
                        }
                    }
                }
            }
            Stmt::Labeled(l, body) => {
                if matches!(**body, Stmt::For(..) | Stmt::ForIn(..) | Stmt::ForOf(..) | Stmt::While(..) | Stmt::DoWhile(..)) {
                    self.label = Some(l.clone());
                }
                let r = self.exec(body, env);
                self.label = None;
                match r? {
                    Flow::Brk(Some(x)) if &x == l => {}
                    f => return Ok(f),
                }
            }
            Stmt::Empty => {}
        }
        Ok(Flow::Normal)
    }

    fn for_bind(&mut self, init: &ForInit, v: Value, scope: &Env) -> R<()> {
        match init {
            ForInit::Var(kind, p) => self.bind(p, v, scope, Some(*kind)),
            ForInit::Target(p) => self.bind(p, v, scope, None),
        }
    }

    fn for_in_keys(&mut self, o: &Value) -> Vec<Rc<str>> {
        let mut keys = self.own_keys(o);
        if let Value::Obj(ob) = o {
            let mut cur = ob.borrow().proto.clone();
            while let Some(p) = cur {
                let inherited: Vec<Rc<str>> = p.borrow().props.iter().filter(|(k, pr)| pr.enumerable && !k.starts_with("@@")).map(|(k, _)| k.clone()).collect();
                for k in inherited {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                cur = p.borrow().proto.clone();
            }
        }
        keys
    }

    /// For user-defined iterables (generators, objects with `@@iterator`):
    /// the iterator and its `next` method. None for built-in collections.
    fn lazy_iterator(&mut self, v: &Value) -> R<Option<(Value, Value)>> {
        let Value::Obj(o) = v else { return Ok(None) };
        if matches!(o.borrow().kind, Kind::Array(_) | Kind::Set(_) | Kind::Map(_) | Kind::Typed(_) | Kind::Iter(_)) {
            return Ok(None);
        }
        let f = self.get(v, "@@iterator")?;
        if !Self::is_callable(&f) {
            return Ok(None);
        }
        let iter = self.call(&f, v.clone(), &[])?;
        let next = self.get(&iter, "next")?;
        Ok(Some((iter, next)))
    }

    /// Materialize an iterable into a Vec.
    pub fn iterate(&mut self, v: &Value) -> R<Vec<Value>> {
        match v {
            Value::Str(s) => return Ok(s.chars().map(|c| Value::Str(c.to_string().into())).collect()),
            Value::Obj(o) => {
                if matches!(o.borrow().kind, Kind::Iter(_)) {
                    return Ok(iter::drain(self, o));
                }
                let kind_items = match &o.borrow().kind {
                    Kind::Array(a) => Some(a.iter().cloned().map(Value::unhole).collect()),
                    Kind::Set(s) => Some(s.keys()),
                    Kind::Typed(t) => Some(t.values().into_iter().map(Value::Num).collect()),
                    Kind::Map(m) => Some(m.iter().map(|(k, v)| (k.clone(), v.clone())).map(|(k, v)| self.array(vec![k, v])).collect()),
                    _ => None,
                };
                if let Some(items) = kind_items {
                    return Ok(items);
                }
                let it = self.get(v, "@@iterator")?;
                if Self::is_callable(&it) {
                    let iter = self.call(&it, v.clone(), &[])?;
                    let next = self.get(&iter, "next")?;
                    let mut out = Vec::new();
                    for _ in 0..10_000_000 {
                        let mut r = self.call(&next, iter.clone(), &[])?;
                        if matches!(&r, Value::Obj(o) if matches!(o.borrow().kind, Kind::Promise(_))) {
                            // Async iterators (for await).
                            r = builtins::await_value(self, r)?;
                        }
                        if self.get(&r, "done")?.truthy() {
                            break;
                        }
                        out.push(self.get(&r, "value")?);
                    }
                    return Ok(out);
                }
                // Array-like (NodeList, arguments, ...).
                let len = self.get(v, "length")?;
                if let Value::Num(n) = len {
                    let mut out = Vec::with_capacity(n as usize);
                    for i in 0..n as usize {
                        out.push(self.get(v, &i.to_string())?);
                    }
                    return Ok(out);
                }
            }
            _ => {}
        }
        Err(type_error(format!("{:?} is not iterable", v)))
    }

    // ------------------------------------------------------------- binding

    /// Bind a pattern: `decl` = Some(kind) declares in `env`, None assigns.
    pub fn bind(&mut self, p: &Pattern, v: Value, env: &Env, decl: Option<VarKind>) -> R<()> {
        match p {
            Pattern::Ident(n) => match decl {
                Some(VarKind::Var) => {
                    let fs = Self::func_scope(env).clone();
                    if Rc::ptr_eq(&fs, &self.genv) && !fs.vars.borrow().contains_key(n) {
                        // Global `var`s are properties of the global object.
                        return self.put(&Value::Obj(self.global.clone()), n, v);
                    }
                    fs.vars.borrow_mut().insert(n.clone(), v);
                    Ok(())
                }
                Some(_) => {
                    env.vars.borrow_mut().insert(n.clone(), v);
                    Ok(())
                }
                None => self.assign_var(n, v, env),
            },
            Pattern::Expr(e) => match e {
                Expr::Member(o, k, _) => {
                    let ov = self.eval(o, env)?;
                    self.put(&ov, k, v)
                }
                Expr::Index(o, k, _) => {
                    let ov = self.eval(o, env)?;
                    let kv = self.eval(k, env)?;
                    self.put_v(&ov, &kv, v)
                }
                _ => Err(type_error("invalid assignment target")),
            },
            Pattern::Default(inner, d) => {
                let v = if matches!(v, Value::Undef) {
                    let dv = self.eval(d, env)?;
                    name_fn(&dv, inner);
                    dv
                } else {
                    v
                };
                self.bind(inner, v, env, decl)
            }
            Pattern::Array(items, rest) => {
                let vals = if v.is_nullish() { return Err(type_error("cannot destructure null")) } else { self.iterate(&v)? };
                for (i, it) in items.iter().enumerate() {
                    if let Some(p) = it {
                        self.bind(p, vals.get(i).cloned().unwrap_or_default(), env, decl)?;
                    }
                }
                if let Some(r) = rest {
                    let tail = self.array(vals.get(items.len()..).map(|s| s.to_vec()).unwrap_or_default());
                    self.bind(r, tail, env, decl)?;
                }
                Ok(())
            }
            Pattern::Object(props, rest) => {
                if v.is_nullish() {
                    return Err(type_error(format!("Cannot destructure '{:?}'", v)));
                }
                let mut used = Vec::new();
                for (k, p) in props {
                    let key = self.prop_key(k, env)?;
                    let pv = self.get(&v, &key)?;
                    used.push(key);
                    self.bind(p, pv, env, decl)?;
                }
                if let Some(r) = rest {
                    let o = self.plain();
                    for k in self.own_keys(&v) {
                        if !used.contains(&k) {
                            let pv = self.get(&v, &k)?;
                            Self::set_prop(&o, &k, pv);
                        }
                    }
                    self.bind(r, Value::Obj(o), env, decl)?;
                }
                Ok(())
            }
        }
    }

    fn prop_key(&mut self, k: &PropKey, env: &Env) -> R<Rc<str>> {
        match k {
            PropKey::Name(n) => Ok(n.clone()),
            PropKey::Computed(e) => {
                let v = self.eval(e, env)?;
                self.key(&v)
            }
        }
    }

    // --------------------------------------------------------- expressions

    fn spread_args(&mut self, items: &[ArrItem], env: &Env) -> R<Vec<Value>> {
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            match it {
                ArrItem::Item(e) => out.push(self.eval(e, env)?),
                ArrItem::Spread(e) => {
                    let v = self.eval(e, env)?;
                    out.extend(self.iterate(&v)?);
                }
                ArrItem::Hole => out.push(Value::Hole),
            }
        }
        Ok(out)
    }

    pub fn eval(&mut self, e: &Expr, env: &Env) -> R<Value> {
        Ok(match e {
            Expr::Num(n) => Value::Num(*n),
            Expr::BigInt(n) => Value::big(*n),
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Null => Value::Null,
            Expr::Ident(n) => {
                if &**n == "undefined" {
                    return Ok(Value::Undef);
                }
                return self.lookup(n, env);
            }
            Expr::This => Self::this_val(env),
            Expr::Super => {
                let home = Self::home_of(env);
                let proto = home.and_then(|h| h.borrow().proto.clone());
                proto.map(Value::Obj).unwrap_or_default()
            }
            Expr::Template(q, xs) => {
                let mut s = String::new();
                for (i, part) in q.iter().enumerate() {
                    s.push_str(part);
                    if let Some(x) = xs.get(i) {
                        let v = self.eval(x, env)?;
                        s.push_str(&self.to_str(&v)?);
                    }
                }
                Value::Str(s.into())
            }
            Expr::TaggedTemplate(tag, q, xs) => {
                let f = self.eval(tag, env)?;
                let strs = self.array(q.iter().map(|s| Value::Str(s.clone())).collect());
                if let Value::Obj(o) = &strs {
                    Self::set_hidden(o, "raw", strs.clone());
                }
                let mut args = vec![strs];
                for x in xs {
                    args.push(self.eval(x, env)?);
                }
                return self.call(&f, Value::Undef, &args);
            }
            Expr::Regex(b, f) => builtins::make_regex(self, b, f)?,
            Expr::Array(items) => {
                let v = self.spread_args(items, env)?;
                self.array(v)
            }
            Expr::Object(props) => {
                let o = self.plain();
                let ov = Value::Obj(o.clone());
                for p in props {
                    match p {
                        PropDef::Value(k, ve) => {
                            let key = self.prop_key(k, env)?;
                            let v = self.eval(ve, env)?;
                            if let (Value::Obj(f), Expr::Func(_)) = (&v, ve) {
                                // Methods: `super` refers to this object's prototype.
                                if let Kind::Func(Func::Closure { home, def, .. }) = &mut f.borrow_mut().kind {
                                    if !def.arrow {
                                        *home = Some(o.clone());
                                    }
                                }
                                name_fn(&v, &Pattern::Ident(key.clone()));
                            }
                            if &*key == "__proto__" {
                                if let Value::Obj(p) = &v {
                                    o.borrow_mut().proto = Some(p.clone());
                                }
                                continue;
                            }
                            o.borrow_mut().props.insert(key, Prop::data(v));
                        }
                        PropDef::Getter(k, f) | PropDef::Setter(k, f) => {
                            let key = self.prop_key(k, env)?;
                            let fv = self.make_closure(f, env);
                            let mut ob = o.borrow_mut();
                            let is_get = matches!(p, PropDef::Getter(..));
                            match ob.props.get_mut(&key) {
                                Some(pr) => {
                                    if is_get { pr.get = Some(fv) } else { pr.set = Some(fv) }
                                }
                                None => {
                                    let pr = if is_get { Prop { v: Value::Undef, get: Some(fv), set: None, enumerable: true } } else { Prop { v: Value::Undef, get: None, set: Some(fv), enumerable: true } };
                                    ob.props.insert(key, pr);
                                }
                            }
                        }
                        PropDef::Spread(e) => {
                            let v = self.eval(e, env)?;
                            for k in self.own_keys(&v) {
                                let pv = self.get(&v, &k)?;
                                o.borrow_mut().props.insert(k, Prop::data(pv));
                            }
                        }
                    }
                }
                ov
            }
            Expr::Func(def) => self.make_closure(def, env),
            Expr::Class(cd) => self.make_class(cd, env)?,
            Expr::Unary(op, x) => {
                if *op == "typeof" {
                    if let Expr::Ident(n) = &**x {
                        return Ok(match self.lookup(n, env) {
                            Ok(v) => Value::str(Self::type_of(&v)),
                            Err(_) => Value::str("undefined"),
                        });
                    }
                    let v = self.eval(x, env)?;
                    return Ok(Value::str(Self::type_of(&v)));
                }
                if *op == "delete" {
                    return Ok(Value::Bool(match &**x {
                        Expr::Member(o, k, _) => {
                            let ov = self.eval(o, env)?;
                            self.delete(&ov, k)
                        }
                        Expr::Index(o, k, _) => {
                            let ov = self.eval(o, env)?;
                            let kv = self.eval(k, env)?;
                            let k = self.key(&kv)?;
                            self.delete(&ov, &k)
                        }
                        _ => true,
                    }));
                }
                let v = self.eval(x, env)?;
                if let Value::BigInt(b) = v {
                    let b = b.get();
                    return match *op {
                        "-" => Ok(Value::big(b.checked_neg().ok_or_else(bigint::too_big)?)),
                        "~" => Ok(Value::big(!b)),
                        "+" => Err(type_error("Cannot convert a BigInt value to a number")),
                        "!" => Ok(Value::Bool(b == 0)),
                        _ => Ok(Value::Undef),
                    };
                }
                match *op {
                    "!" => Value::Bool(!v.truthy()),
                    "-" => Value::Num(-self.to_num(&v)?),
                    "+" => Value::Num(self.to_num(&v)?),
                    "~" => Value::Num(!(to_i32(self.to_num(&v)?)) as f64),
                    "void" => Value::Undef,
                    _ => Value::Undef,
                }
            }
            Expr::Update { op, prefix, target } => {
                // Member targets: object and key evaluated once (`f().x++`,
                // `a[i++]++`).
                let member = match &**target {
                    Expr::Member(o, k, _) if !matches!(**o, Expr::Super) => Some((o, Some(Value::Str(k.clone())), None)),
                    Expr::Index(o, k, _) => Some((o, None, Some(k))),
                    _ => None,
                };
                if let Some((o, key, key_expr)) = member {
                    let ov = self.eval(o, env)?;
                    let kv = match (key, key_expr) {
                        (Some(k), _) => k,
                        (None, Some(k)) => self.eval(k, env)?,
                        _ => Value::Undef,
                    };
                    let old = self.get_v(&ov, &kv)?;
                    let (new, ret) = match old {
                        Value::BigInt(b) => {
                            let b = b.get();
                            let n = if *op == "++" { b.checked_add(1) } else { b.checked_sub(1) }.ok_or_else(bigint::too_big)?;
                            (Value::big(n), Value::big(if *prefix { n } else { b }))
                        }
                        v => {
                            let n = self.to_num(&v)?;
                            let new = if *op == "++" { n + 1.0 } else { n - 1.0 };
                            (Value::Num(new), Value::Num(if *prefix { new } else { n }))
                        }
                    };
                    self.put_v(&ov, &kv, new)?;
                    return Ok(ret);
                }
                let old = self.eval(target, env)?;
                if let Value::BigInt(b) = old {
                    let b = b.get();
                    let new = if *op == "++" { b.checked_add(1) } else { b.checked_sub(1) }.ok_or_else(bigint::too_big)?;
                    let p = expr_target(target)?;
                    self.bind_ref(&p, Value::big(new), env)?;
                    return Ok(Value::big(if *prefix { new } else { b }));
                }
                let n = self.to_num(&old)?;
                let new = if *op == "++" { n + 1.0 } else { n - 1.0 };
                let p = expr_target(target)?;
                self.bind_ref(&p, Value::Num(new), env)?;
                Value::Num(if *prefix { new } else { n })
            }
            Expr::Binary(op, a, b) => {
                let av = self.eval(a, env)?;
                let bv = self.eval(b, env)?;
                self.binary(op, &av, &bv)?
            }
            Expr::Logical(op, a, b) => {
                let av = self.eval(a, env)?;
                match *op {
                    "&&" => if av.truthy() { self.eval(b, env)? } else { av },
                    "||" => if av.truthy() { av } else { self.eval(b, env)? },
                    _ => if av.is_nullish() { self.eval(b, env)? } else { av },
                }
            }
            Expr::Assign(op, target, value) => {
                // Member targets: the object and key are evaluated once,
                // before the value (`a[i] = ++i`, `a[i++] += 1`).
                let member = match &**target {
                    Pattern::Expr(Expr::Member(o, k, _)) if !matches!(**o, Expr::Super) => Some((o, Value::Str(k.clone()), None)),
                    Pattern::Expr(Expr::Index(o, k, _)) => Some((o, Value::Undef, Some(k))),
                    _ => None,
                };
                if let Some((o, key, key_expr)) = member {
                    let ov = self.eval(o, env)?;
                    let kv = match key_expr {
                        Some(k) => self.eval(k, env)?,
                        None => key,
                    };
                    let v = match *op {
                        "=" => self.eval(value, env)?,
                        _ => {
                            let cur = self.get_v(&ov, &kv)?;
                            match *op {
                                "&&=" => if cur.truthy() { self.eval(value, env)? } else { return Ok(cur) },
                                "||=" => if cur.truthy() { return Ok(cur) } else { self.eval(value, env)? },
                                "??=" => if cur.is_nullish() { self.eval(value, env)? } else { return Ok(cur) },
                                _ => {
                                    let rv = self.eval(value, env)?;
                                    self.binary(&op[..op.len() - 1], &cur, &rv)?
                                }
                            }
                        }
                    };
                    self.put_v(&ov, &kv, v.clone())?;
                    return Ok(v);
                }
                if *op == "=" {
                    let v = self.eval(value, env)?;
                    name_fn(&v, target);
                    self.bind(target, v.clone(), env, None)?;
                    return Ok(v);
                }
                let cur = self.eval_pattern_value(target, env)?;
                let v = match *op {
                    "&&=" => if cur.truthy() { self.eval(value, env)? } else { return Ok(cur) },
                    "||=" => if cur.truthy() { return Ok(cur) } else { self.eval(value, env)? },
                    "??=" => if cur.is_nullish() { self.eval(value, env)? } else { return Ok(cur) },
                    _ => {
                        let rv = self.eval(value, env)?;
                        self.binary(&op[..op.len() - 1], &cur, &rv)?
                    }
                };
                self.bind_ref(target, v.clone(), env)?;
                v
            }
            Expr::Cond(c, a, b) => {
                if self.eval(c, env)?.truthy() { self.eval(a, env)? } else { self.eval(b, env)? }
            }
            Expr::Seq(xs) => {
                let mut v = Value::Undef;
                for x in xs {
                    v = self.eval(x, env)?;
                }
                v
            }
            Expr::Member(o, k, opt) => {
                if let Expr::Super = **o {
                    let sup = self.eval(o, env)?;
                    return self.get(&sup, k);
                }
                let Some(ov) = self.chain_obj(o, env)? else { return Ok(self.short_circuit()) };
                if *opt && ov.is_nullish() {
                    return Ok(self.short_circuit());
                }
                if ov.is_nullish() && trace_on() {
                    self.log(&format!("[trace] {}.{k} on {:?}{}", expr_name(o), ov, trace_stack()));
                }
                let v = self.get(&ov, k)?;
                self.short = false;
                v
            }
            Expr::Index(o, k, opt) => {
                let Some(ov) = self.chain_obj(o, env)? else { return Ok(self.short_circuit()) };
                if *opt && ov.is_nullish() {
                    return Ok(self.short_circuit());
                }
                let kv = self.eval(k, env)?;
                let v = self.get_v(&ov, &kv)?;
                self.short = false;
                v
            }
            Expr::Call(callee, args, opt) => return self.eval_call(callee, args, *opt, env),
            Expr::New(callee, args) => {
                let f = self.eval(callee, env)?;
                let a = self.spread_args(args, env)?;
                if !Self::is_callable(&f) {
                    return Err(type_error("not a constructor"));
                }
                self.construct(&f, &a)?
            }
            Expr::Await(x) => {
                let v = self.eval(x, env)?;
                builtins::await_value(self, v)?
            }
            Expr::ImportCall(x) => {
                let spec = self.eval(x, env)?;
                self.dynamic_import(spec, env)
            }
            Expr::ImportMeta => self.import_meta(env),
            Expr::Yield(arg, delegate) => {
                let v = match arg {
                    Some(a) => self.eval(a, env)?,
                    None => Value::Undef,
                };
                if let Some(r) = self.co_yield(v.clone(), *delegate) {
                    return r;
                }
                let items = if *delegate { self.iterate(&v)? } else { vec![v] };
                let Some(sink) = self.gen_sinks.last_mut() else { return Ok(Value::Undef) };
                sink.extend(items);
                if sink.len() > GEN_CAP {
                    return Err(Value::str(GEN_CAP_MARKER));
                }
                Value::Undef
            }
        })
    }

    fn eval_call(&mut self, callee: &Expr, args: &[ArrItem], opt: bool, env: &Env) -> R<Value> {
        // super(...) in a derived class constructor
        if let Expr::Super = callee {
            let a = self.spread_args(args, env)?;
            let this = Self::this_val(env);
            let class = Self::home_of(env).and_then(|h| h.borrow().props.get("constructor").map(|p| p.v.clone()));
            if let Some(Value::Obj(c)) = class {
                let info = match &c.borrow().kind {
                    Kind::Func(Func::Class(ci)) => Some(ci.clone()),
                    _ => None,
                };
                if let Some(info) = info {
                    if let Some(p) = &info.parent {
                        self.super_call(p, &this, &a)?;
                    }
                    let proto = self.get(&Value::Obj(c.clone()), "prototype")?;
                    self.init_fields(&info, &this, proto.obj().cloned())?;
                }
            }
            return Ok(Value::Undef);
        }
        let (f, this) = match callee {
            Expr::Member(o, k, mopt) => {
                let ov = if let Expr::Super = **o {
                    Self::this_val(env)
                } else {
                    let Some(ov) = self.chain_obj(o, env)? else { return Ok(self.short_circuit()) };
                    ov
                };
                if *mopt && ov.is_nullish() {
                    return Ok(self.short_circuit());
                }
                if ov.is_nullish() && trace_on() {
                    self.log(&format!("[trace] call {}.{k}() on {:?}{}", expr_name(o), ov, trace_stack()));
                }
                let f = if let Expr::Super = **o {
                    let sup = self.eval(o, env)?;
                    self.get(&sup, k)?
                } else {
                    self.get(&ov, k)?
                };
                if !Self::is_callable(&f) {
                    if opt && f.is_nullish() {
                        return Ok(self.short_circuit());
                    }
                    if trace_on() {
                        let keys: Vec<String> = match &ov {
                            Value::Obj(ob) => ob.borrow().props.iter().take(12).map(|(k, _)| k.to_string()).collect(),
                            _ => Vec::new(),
                        };
                        let ctor = self.get(&ov, "constructor").ok().and_then(|c| self.get(&c, "name").ok()).unwrap_or_default();
                        self.log(&format!("[trace] {}.{k} is {:?}; object ({:?}) keys: {}{}", expr_name(o), f, ctor, keys.join(","), trace_stack()));
                    }
                    return Err(type_error(format!("{}.{k} is not a function", expr_name(o))));
                }
                (f, ov)
            }
            Expr::Index(o, k, mopt) => {
                let Some(ov) = self.chain_obj(o, env)? else { return Ok(self.short_circuit()) };
                if *mopt && ov.is_nullish() {
                    return Ok(self.short_circuit());
                }
                let kv = self.eval(k, env)?;
                (self.get_v(&ov, &kv)?, ov)
            }
            _ => {
                let Some(f) = self.chain_obj(callee, env)? else { return Ok(self.short_circuit()) };
                (f, Value::Undef)
            }
        };
        if opt && f.is_nullish() {
            return Ok(self.short_circuit());
        }
        if !Self::is_callable(&f) {
            return Err(type_error(format!("{} is not a function", expr_name(callee))));
        }
        let a = self.spread_args(args, env)?;
        let r = self.call(&f, this, &a)?;
        self.short = false;
        Ok(r)
    }

    /// Evaluate the object of a member access or call. None if it is part of
    /// an optional chain that already short-circuited (`a?.b.c` with `a`
    /// nullish skips `.c`).
    fn chain_obj(&mut self, o: &Expr, env: &Env) -> R<Option<Value>> {
        self.short = false;
        let v = self.eval(o, env)?;
        let chained = matches!(o, Expr::Member(..) | Expr::Index(..) | Expr::Call(..));
        Ok(if std::mem::take(&mut self.short) && chained { None } else { Some(v) })
    }

    fn short_circuit(&mut self) -> Value {
        self.short = true;
        Value::Undef
    }

    fn eval_pattern_value(&mut self, p: &Pattern, env: &Env) -> R<Value> {
        match p {
            Pattern::Ident(n) => self.lookup(n, env),
            Pattern::Expr(e) => self.eval(e, env),
            _ => Err(type_error("invalid compound assignment")),
        }
    }

    fn bind_ref(&mut self, p: &Pattern, v: Value, env: &Env) -> R<()> {
        self.bind(p, v, env, None)
    }

    pub(crate) fn delete(&mut self, o: &Value, k: &str) -> bool {
        if let Value::Obj(ob) = o {
            if matches!(ob.borrow().kind, Kind::Proxy(_)) {
                return proxy::delete(self, ob, k).unwrap_or(false);
            }
            let mut b = ob.borrow_mut();
            if let Kind::Array(a) = &mut b.kind {
                if let Some(i) = array_index(k) {
                    if i < a.len() {
                        a[i] = Value::Hole;
                    }
                    return true;
                }
            }
            b.props.remove(k);
        }
        true
    }

    pub fn binary(&mut self, op: &str, a: &Value, b: &Value) -> R<Value> {
        if matches!(a, Value::BigInt(_)) || matches!(b, Value::BigInt(_)) {
            if let Some(r) = bigint::binary(self, op, a, b)? {
                return Ok(r);
            }
        }
        Ok(match op {
            "+" => {
                let pa = self.to_primitive(a, false)?;
                let pb = self.to_primitive(b, false)?;
                if matches!(pa, Value::Str(_)) || matches!(pb, Value::Str(_)) {
                    let mut s = self.to_str(&pa)?.to_string();
                    s.push_str(&self.to_str(&pb)?);
                    Value::Str(s.into())
                } else {
                    Value::Num(self.to_num(&pa)? + self.to_num(&pb)?)
                }
            }
            "-" => Value::Num(self.to_num(a)? - self.to_num(b)?),
            "*" => Value::Num(self.to_num(a)? * self.to_num(b)?),
            "/" => Value::Num(self.to_num(a)? / self.to_num(b)?),
            "%" => Value::Num(self.to_num(a)? % self.to_num(b)?),
            "**" => Value::Num(self.to_num(a)?.powf(self.to_num(b)?)),
            "==" => Value::Bool(self.loose_eq(a, b)?),
            "!=" => Value::Bool(!self.loose_eq(a, b)?),
            "===" => Value::Bool(strict_eq(a, b)),
            "!==" => Value::Bool(!strict_eq(a, b)),
            "<" | ">" | "<=" | ">=" => {
                let pa = self.to_primitive(a, false)?;
                let pb = self.to_primitive(b, false)?;
                let ord = if let (Value::Str(x), Value::Str(y)) = (&pa, &pb) {
                    Some(x.encode_utf16().cmp(y.encode_utf16()))
                } else {
                    let (x, y) = (self.to_num(&pa)?, self.to_num(&pb)?);
                    x.partial_cmp(&y)
                };
                Value::Bool(match ord {
                    None => false,
                    Some(o) => match op {
                        "<" => o.is_lt(),
                        ">" => o.is_gt(),
                        "<=" => o.is_le(),
                        _ => o.is_ge(),
                    },
                })
            }
            "&" => Value::Num((to_i32(self.to_num(a)?) & to_i32(self.to_num(b)?)) as f64),
            "|" => Value::Num((to_i32(self.to_num(a)?) | to_i32(self.to_num(b)?)) as f64),
            "^" => Value::Num((to_i32(self.to_num(a)?) ^ to_i32(self.to_num(b)?)) as f64),
            "<<" => Value::Num((to_i32(self.to_num(a)?).wrapping_shl(to_i32(self.to_num(b)?) as u32 & 31)) as f64),
            ">>" => Value::Num((to_i32(self.to_num(a)?) >> (to_i32(self.to_num(b)?) as u32 & 31)) as f64),
            ">>>" => Value::Num(((to_i32(self.to_num(a)?) as u32) >> (to_i32(self.to_num(b)?) as u32 & 31)) as f64),
            "instanceof" => {
                let proto = self.get(b, "prototype")?;
                let Value::Obj(p) = proto else { return Ok(Value::Bool(false)) };
                let mut cur = match a {
                    Value::Obj(o) => o.borrow().proto.clone(),
                    _ => None,
                };
                let mut r = false;
                while let Some(c) = cur {
                    if Rc::ptr_eq(&c, &p) {
                        r = true;
                        break;
                    }
                    cur = c.borrow().proto.clone();
                }
                Value::Bool(r)
            }
            "in" => {
                let k = self.key(a)?;
                Value::Bool(self.has_prop(b, &k))
            }
            _ => return Err(type_error(format!("unknown operator {op}"))),
        })
    }

    pub fn loose_eq(&mut self, a: &Value, b: &Value) -> R<bool> {
        Ok(match (a, b) {
            (Value::Undef | Value::Null | Value::Hole, Value::Undef | Value::Null | Value::Hole) => true,
            (Value::Undef | Value::Null | Value::Hole, _) | (_, Value::Undef | Value::Null | Value::Hole) => false,
            (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
            (Value::Obj(_), _) => {
                let p = self.to_primitive(a, false)?;
                return self.loose_eq(&p, b);
            }
            (_, Value::Obj(_)) => {
                let p = self.to_primitive(b, false)?;
                return self.loose_eq(a, &p);
            }
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::BigInt(x), Value::BigInt(y)) => x == y,
            (Value::BigInt(x), Value::Str(s)) | (Value::Str(s), Value::BigInt(x)) => bigint::parse_str(s) == Some(x.get()),
            _ => self.to_num(a)? == self.to_num(b)?,
        })
    }

    fn make_class(&mut self, cd: &Rc<ClassDef>, env: &Env) -> R<Value> {
        let parent = match &cd.extends {
            Some(e) => Some(self.eval(e, env)?),
            None => None,
        };
        let scope = new_scope(Some(env.clone()), None, None, false);
        let info = Rc::new(ClassInfo { def: cd.clone(), parent: parent.clone(), env: scope.clone() });
        let ctor_proto = match &parent {
            Some(Value::Obj(p)) => Some(p.clone()),
            _ => self.p.function.clone(),
        };
        let class = self.new_obj(ctor_proto, Kind::Func(Func::Class(info)));
        func_props(&class, cd.name.clone().unwrap_or_else(|| Rc::from("")), cd.ctor.as_ref().map_or(0, |c| formal_length(c)));
        let proto_parent = match &parent {
            Some(Value::Null) => None,
            Some(p @ Value::Obj(_)) => self.get(p, "prototype")?.obj().cloned(),
            _ => self.p.object.clone(),
        };
        let proto = self.new_obj(proto_parent, Kind::Plain);
        Self::set_hidden(&proto, "constructor", Value::Obj(class.clone()));
        Self::set_hidden(&class, "prototype", Value::Obj(proto.clone()));
        if let Some(n) = &cd.name {
            Self::set_hidden(&class, "name", Value::Str(n.clone()));
            scope.vars.borrow_mut().insert(n.clone(), Value::Obj(class.clone()));
        }
        for (is_static, key, kind, f) in &cd.methods {
            let target = if *is_static { class.clone() } else { proto.clone() };
            let k = self.prop_key(key, &scope)?;
            let fo = self.new_obj(self.p.function.clone(), Kind::Func(Func::Closure { def: f.clone(), env: scope.clone(), this: None, home: Some(target.clone()) }));
            let fname: Rc<str> = match kind {
                MethodKind::Get => format!("get {k}").into(),
                MethodKind::Set => format!("set {k}").into(),
                _ => k.clone(),
            };
            func_props(&fo, fname, formal_length(f));
            let fv = Value::Obj(fo);
            let mut t = target.borrow_mut();
            match kind {
                MethodKind::Method => t.props.insert(k, Prop { v: fv, get: None, set: None, enumerable: false }),
                MethodKind::Get | MethodKind::Set => {
                    let is_get = *kind == MethodKind::Get;
                    match t.props.get_mut(&k) {
                        Some(p) => {
                            if is_get { p.get = Some(fv) } else { p.set = Some(fv) }
                        }
                        None => t.props.insert(k, Prop { v: Value::Undef, get: if is_get { Some(fv.clone()) } else { None }, set: if is_get { None } else { Some(fv) }, enumerable: false }),
                    }
                }
            }
        }
        // Static fields.
        let cv = Value::Obj(class.clone());
        let sscope = new_scope(Some(scope.clone()), Some(cv.clone()), Some(class.clone()), true);
        for (is_static, key, init) in &cd.fields {
            if !*is_static {
                continue;
            }
            let v = match init {
                Some(e) => self.eval(e, &sscope)?,
                None => Value::Undef,
            };
            let k = self.prop_key(key, &sscope)?;
            if !k.is_empty() {
                self.put(&cv, &k, v)?;
            }
        }
        Ok(cv)
    }

    /// Run a generator's body (once), buffering what it yields.
    pub fn gen_run(&mut self, o: &ObjRef) {
        let start = match &mut o.borrow_mut().kind {
            Kind::Generator(g) => g.start.take(),
            _ => None,
        };
        let Some(st) = start else { return };
        self.gen_sinks.push(Vec::new());
        let r = self.call_closure(&st.def, st.env, None, st.home, st.this, &st.args, Some(st.fobj));
        let sink = self.gen_sinks.pop().unwrap_or_default();
        if let Kind::Generator(g) = &mut o.borrow_mut().kind {
            g.buf = sink.into();
            match r {
                Ok(v) => g.ret = Some(v),
                Err(Value::Str(s)) if &*s == GEN_CAP_MARKER => g.ret = Some(Value::Undef),
                Err(e) => g.err = Some(e),
            }
        }
    }

    // ------------------------------------------------------------ the loop

    /// Run queued promise reactions.
    pub fn run_microtasks(&mut self) {
        // A microtask checkpoint happens only with no JS on the stack: not
        // from natives called by scripts (dispatchEvent, click(), inserted
        // scripts...) and never inside a coroutine, which a microtask could
        // otherwise try to resume while it is still running.
        if self.depth > 0 || self.script_running > 0 || !self.co_stack.is_empty() || coro::running() {
            return;
        }
        let mut n = 0;
        loop {
            // Mutations made before a microtask was queued are delivered
            // first (their notification was queued at the first mutation).
            self.deliver_mutations();
            while let Some((f, args)) = self.micro.pop_front() {
                if let Err(e) = self.call(&f, Value::Undef, &args) {
                    let m = self.describe_error(&e);
                    self.log(&format!("Uncaught (in promise) {m}"));
                }
                n += 1;
                if n > 100_000 {
                    return;
                }
            }
            // MutationObserver callbacks run at the microtask checkpoint.
            if !self.deliver_mutations() || n > 100_000 {
                break;
            }
            n += 1;
        }
        for p in std::mem::take(&mut self.unhandled_rejections) {
            let reason = match &p.borrow().kind {
                Kind::Promise(PromiseState::Rejected(v)) => v.clone(),
                _ => continue,
            };
            let m = self.describe_error(&reason);
            self.log(&format!("Uncaught (in promise) {m}"));
        }
    }

    /// Run timers that are due at `now` (ms). Returns how many ran.
    pub fn run_timers(&mut self, now: f64) -> usize {
        self.now = now;
        let mut ran = 0;
        for _ in 0..1000 {
            // MessagePort messages are tasks too.
            let polled = self.poll_ports();
            let Some(idx) = self.timers.iter().enumerate().filter(|(_, t)| t.at <= now).min_by(|a, b| a.1.at.partial_cmp(&b.1.at).unwrap()).map(|(i, _)| i) else {
                if polled {
                    continue;
                }
                break;
            };
            let t = &mut self.timers[idx];
            let (f, args) = (t.func.clone(), t.args.clone());
            match t.interval {
                Some(iv) => t.at = now.max(t.at) + iv.max(4.0),
                None => {
                    self.timers.remove(idx);
                }
            }
            self.steps = 0;
            if let Err(e) = self.call(&f, Value::Undef, &args) {
                let m = self.describe_error(&e);
                self.log(&format!("Uncaught {m}"));
            }
            self.run_microtasks();
            ran += 1;
        }
        ran
    }

    pub fn next_timer_at(&self) -> Option<f64> {
        self.timers.iter().map(|t| t.at).reduce(f64::min)
    }

    pub fn log(&mut self, s: &str) {
        eprintln!("[js] {s}");
        if self.console.len() < 1000 {
            self.console.push(s.to_owned());
        }
    }
}

/// Does a statement create functions (which could capture loop variables)?
fn stmt_has_closure(s: &Stmt) -> bool {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => expr_has_closure(e),
        Stmt::Var(_, ds) => ds.iter().any(|(p, e)| pat_has_closure(p) || e.as_ref().is_some_and(expr_has_closure)),
        Stmt::Func(_) | Stmt::Class(_) => true,
        Stmt::Return(e) => e.as_ref().is_some_and(expr_has_closure),
        Stmt::If(c, a, b) => expr_has_closure(c) || stmt_has_closure(a) || b.as_deref().is_some_and(stmt_has_closure),
        Stmt::Block(b) => b.iter().any(stmt_has_closure),
        Stmt::For(i, t, u, b) => i.as_deref().is_some_and(stmt_has_closure) || t.as_ref().is_some_and(expr_has_closure) || u.as_ref().is_some_and(expr_has_closure) || stmt_has_closure(b),
        Stmt::ForIn(_, e, b) | Stmt::ForOf(_, e, b) | Stmt::While(e, b) | Stmt::DoWhile(b, e) => expr_has_closure(e) || stmt_has_closure(b),
        Stmt::Try(a, c, f) => a.iter().any(stmt_has_closure) || c.as_ref().is_some_and(|(_, c)| c.iter().any(stmt_has_closure)) || f.as_ref().is_some_and(|f| f.iter().any(stmt_has_closure)),
        Stmt::Switch(d, cases) => expr_has_closure(d) || cases.iter().any(|(t, b)| t.as_ref().is_some_and(expr_has_closure) || b.iter().any(stmt_has_closure)),
        Stmt::Labeled(_, b) => stmt_has_closure(b),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => false,
    }
}

fn pat_has_closure(p: &Pattern) -> bool {
    match p {
        Pattern::Ident(_) => false,
        Pattern::Expr(e) => expr_has_closure(e),
        Pattern::Default(i, e) => pat_has_closure(i) || expr_has_closure(e),
        Pattern::Array(items, r) => items.iter().flatten().any(pat_has_closure) || r.as_deref().is_some_and(pat_has_closure),
        Pattern::Object(props, r) => props.iter().any(|(k, p)| matches!(k, PropKey::Computed(e) if expr_has_closure(e)) || pat_has_closure(p)) || r.as_deref().is_some_and(pat_has_closure),
    }
}

fn expr_has_closure(e: &Expr) -> bool {
    let items = |v: &[ArrItem]| v.iter().any(|i| matches!(i, ArrItem::Item(e) | ArrItem::Spread(e) if expr_has_closure(e)));
    match e {
        Expr::Func(_) | Expr::Class(_) => true,
        Expr::Num(_) | Expr::BigInt(_) | Expr::Str(_) | Expr::Regex(..) | Expr::Ident(_) | Expr::This | Expr::Super | Expr::Null | Expr::Bool(_) => false,
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().any(expr_has_closure),
        Expr::TaggedTemplate(t, _, xs) => expr_has_closure(t) || xs.iter().any(expr_has_closure),
        Expr::Array(v) => items(v),
        Expr::Object(ps) => ps.iter().any(|p| match p {
            PropDef::Value(k, v) => matches!(k, PropKey::Computed(e) if expr_has_closure(e)) || expr_has_closure(v),
            PropDef::Getter(..) | PropDef::Setter(..) => true,
            PropDef::Spread(e) => expr_has_closure(e),
        }),
        Expr::Unary(_, x) | Expr::Await(x) | Expr::ImportCall(x) => expr_has_closure(x),
        Expr::ImportMeta => false,
        Expr::Yield(x, _) => x.as_deref().is_some_and(expr_has_closure),
        Expr::Update { target, .. } => expr_has_closure(target),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) | Expr::Index(a, b, _) => expr_has_closure(a) || expr_has_closure(b),
        Expr::Assign(_, p, v) => pat_has_closure(p) || expr_has_closure(v),
        Expr::Cond(a, b, c) => expr_has_closure(a) || expr_has_closure(b) || expr_has_closure(c),
        Expr::Call(f, a, _) | Expr::New(f, a) => expr_has_closure(f) || items(a),
        Expr::Member(o, _, _) => expr_has_closure(o),
    }
}

/// A function's own `length` and `name` (non-enumerable, in spec order).
pub fn func_props(f: &ObjRef, name: Rc<str>, len: usize) {
    let mut b = f.borrow_mut();
    b.props.insert("length".into(), Prop { v: Value::Num(len as f64), get: None, set: None, enumerable: false });
    b.props.insert("name".into(), Prop { v: Value::Str(name), get: None, set: None, enumerable: false });
}

/// Parameters before the first default / rest (a function's `length`).
pub fn formal_length(def: &FuncDef) -> usize {
    def.params.iter().take_while(|p| !matches!(p, Pattern::Default(..))).count()
}

fn name_fn(v: &Value, p: &Pattern) {
    if let (Value::Obj(o), Pattern::Ident(n)) = (v, p) {
        let mut ob = o.borrow_mut();
        let unnamed = match ob.props.get("name") {
            None => true,
            Some(p) => matches!(&p.v, Value::Str(s) if s.is_empty()),
        };
        if matches!(ob.kind, Kind::Func(Func::Closure { .. }) | Kind::Func(Func::Class(_))) && unnamed {
            ob.props.insert("name".into(), Prop { v: Value::Str(n.clone()), get: None, set: None, enumerable: false });
        }
    }
}

fn expr_target(e: &Expr) -> R<Pattern> {
    Ok(match e {
        Expr::Ident(n) => Pattern::Ident(n.clone()),
        Expr::Member(o, k, _) => Pattern::Expr(Expr::Member(Box::new(clone_simple(o)?), k.clone(), false)),
        Expr::Index(o, k, _) => Pattern::Expr(Expr::Index(Box::new(clone_simple(o)?), Box::new(clone_simple(k)?), false)),
        _ => return Err(type_error("invalid update target")),
    })
}

/// Clone simple (side-effect free) expressions for update targets.
fn clone_simple(e: &Expr) -> R<Expr> {
    Ok(match e {
        Expr::Ident(n) => Expr::Ident(n.clone()),
        Expr::This => Expr::This,
        Expr::Num(n) => Expr::Num(*n),
        Expr::BigInt(n) => Expr::BigInt(*n),
        Expr::Str(s) => Expr::Str(s.clone()),
        Expr::Member(o, k, opt) => Expr::Member(Box::new(clone_simple(o)?), k.clone(), *opt),
        Expr::Index(o, k, opt) => Expr::Index(Box::new(clone_simple(o)?), Box::new(clone_simple(k)?), *opt),
        Expr::Binary(op, a, b) => Expr::Binary(op, Box::new(clone_simple(a)?), Box::new(clone_simple(b)?)),
        _ => return Err(type_error("unsupported update target")),
    })
}

/// `BOLT_PROFILE=2`: self time per JS function (by definition), printed by
/// `prof::report()`.
pub mod prof {
    use super::FuncDef;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Instant;

    struct Frame {
        start: Instant,
        child: f64,
    }

    thread_local! {
        static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
        static TOTALS: RefCell<crate::fx::FxHashMap<usize, (Rc<FuncDef>, u64, f64)>> = RefCell::new(Default::default());
    }

    pub fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("BOLT_PROFILE").is_ok_and(|v| v == "2"))
    }

    pub fn enter() {
        STACK.with(|s| s.borrow_mut().push(Frame { start: Instant::now(), child: 0.0 }));
    }

    pub fn exit(def: &Rc<FuncDef>) {
        let Some(f) = STACK.with(|s| s.borrow_mut().pop()) else { return };
        let total = f.start.elapsed().as_secs_f64() * 1000.0;
        STACK.with(|s| {
            if let Some(p) = s.borrow_mut().last_mut() {
                p.child += total;
            }
        });
        TOTALS.with(|t| {
            let mut t = t.borrow_mut();
            let e = t.entry(Rc::as_ptr(def) as usize).or_insert_with(|| (def.clone(), 0, 0.0));
            e.1 += 1;
            e.2 += total - f.child;
        });
    }

    /// Print the hottest functions and reset.
    pub fn report() {
        if !on() {
            return;
        }
        let mut v: Vec<(Rc<FuncDef>, u64, f64)> = TOTALS.with(|t| t.borrow_mut().drain().map(|(_, e)| e).collect());
        v.sort_by(|a, b| b.2.total_cmp(&a.2));
        for (def, n, ms) in v.iter().take(25) {
            let body = format!("{:?}", def.body);
            let hint: String = body.chars().take(160).collect();
            eprintln!("[prof] {ms:8.1}ms {n:8}x {}({} params) {hint}", def.name.as_deref().unwrap_or("<anon>"), def.params.len());
        }
    }
}

// `BOLT_JS_TRACE=1`: log the expression behind property reads of
// undefined / null (minified code has no other clue).
thread_local! {
    /// JS functions being called (only tracked with BOLT_JS_TRACE).
    static TRACE_STACK: RefCell<Vec<Rc<FuncDef>>> = const { RefCell::new(Vec::new()) };
}

/// The innermost traced JS calls, as `name(params)` with a body snippet.
pub(crate) fn trace_stack() -> String {
    TRACE_STACK.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .take(std::env::var("BOLT_TRACE_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(4))
            .map(|d| {
                let params: Vec<String> = d.params.iter().map(|p| match p {
                    Pattern::Ident(n) => n.to_string(),
                    _ => "_".into(),
                }).collect();
                let body: String = format!("{:?}", d.body).chars().take(std::env::var("BOLT_TRACE_BODY").ok().and_then(|v| v.parse().ok()).unwrap_or(90)).collect();
                format!("
      at {}({}) {body}", d.name.as_deref().unwrap_or("<anon>"), params.join(","))
            })
            .collect()
    })
}

/// `BOLT_TRACE_THROW=1`: log the first `throw` statements executed.
fn trace_throw_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BOLT_TRACE_THROW").is_some())
}

fn trace_throw() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    *ON.get_or_init(|| std::env::var_os("BOLT_TRACE_THROW").is_some()) && COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 30
}

/// `BOLT_TRACE_SCRIPTS=1`: log each script as it runs.
pub(crate) fn trace_scripts() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BOLT_TRACE_SCRIPTS").is_some())
}

pub(crate) fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BOLT_JS_TRACE").is_some())
}

fn expr_name(e: &Expr) -> String {
    match e {
        Expr::Ident(n) => n.to_string(),
        Expr::Member(o, k, _) => format!("{}.{k}", expr_name(o)),
        Expr::This => "this".into(),
        Expr::Index(o, _, _) => format!("{}[..]", expr_name(o)),
        Expr::Call(f, ..) => format!("{}()", expr_name(f)),
        _ => "expression".into(),
    }
}

pub fn pat_names(p: &Pattern, out: &mut Vec<Rc<str>>) {
    match p {
        Pattern::Ident(n) => out.push(n.clone()),
        Pattern::Default(i, _) => pat_names(i, out),
        Pattern::Array(items, rest) => {
            items.iter().flatten().for_each(|p| pat_names(p, out));
            if let Some(r) = rest {
                pat_names(r, out);
            }
        }
        Pattern::Object(props, rest) => {
            props.iter().for_each(|(_, p)| pat_names(p, out));
            if let Some(r) = rest {
                pat_names(r, out);
            }
        }
        Pattern::Expr(_) => {}
    }
}

pub fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Undef | Value::Hole, Value::Undef | Value::Hole) | (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

pub fn to_i32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    (n.trunc() as i64 as u64 & 0xFFFF_FFFF) as u32 as i32
}

pub fn str_to_num(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    match t {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => {
            if t.bytes().any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E') {
                return f64::NAN;
            }
            t.parse().unwrap_or(f64::NAN)
        }
    }
}

pub fn array_index(k: &str) -> Option<usize> {
    let b = k.as_bytes();
    if b.is_empty() || b.len() > 10 || !b.iter().all(|c| c.is_ascii_digit()) || (b.len() > 1 && b[0] == b'0') {
        return None;
    }
    k.parse().ok()
}

/// Strings shorter than this are re-encoded on demand; longer ones have
/// their UTF-16 form cached (scripts index into big strings in loops).
const UNITS_CACHE_MIN: usize = 64;

thread_local! {
    /// Recently used long strings and their UTF-16 code units (the cache
    /// holds the string, so its address can't be reused while cached).
    static UNITS: RefCell<Vec<(Rc<str>, Rc<[u16]>)>> = const { RefCell::new(Vec::new()) };
}

/// The UTF-16 code units of a string.
pub fn str_units(s: &Rc<str>) -> Rc<[u16]> {
    if s.len() < UNITS_CACHE_MIN {
        return s.encode_utf16().collect();
    }
    UNITS.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(i) = c.iter().position(|(k, _)| Rc::ptr_eq(k, s)) {
            let e = c.remove(i);
            let u = e.1.clone();
            c.push(e);
            return u;
        }
        let u: Rc<[u16]> = s.encode_utf16().collect();
        if c.len() >= 8 {
            c.remove(0);
        }
        c.push((s.clone(), u.clone()));
        u
    })
}

pub fn utf16_len(s: &Rc<str>) -> usize {
    if s.len() < UNITS_CACHE_MIN && s.is_ascii() { s.len() } else { str_units(s).len() }
}

pub fn utf16_at(s: &Rc<str>, i: usize) -> Option<String> {
    if s.len() < UNITS_CACHE_MIN && s.is_ascii() {
        return s.get(i..i + 1).map(str::to_owned);
    }
    str_units(s).get(i).map(|c| String::from_utf16_lossy(&[*c]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Vec<String> {
        let mut it = Interp::new(Document::new(), "https://example.com/a/b.html", 800.0, 600.0);
        if let Err(e) = it.run_script(src) {
            panic!("script failed: {e}");
        }
        it.run_microtasks();
        it.run_timers(1e9);
        it.console.clone()
    }

    /// Hot code runs on the bytecode VM and the JIT: results must match
    /// the language (loops past the JIT threshold, deopts, caches).
    #[test]
    fn vm_and_jit() {
        let mut it = Interp::new(Document::new(), "https://example.com/", 800.0, 600.0);
        let scripts = [
            // Numeric loops, type changes mid-loop (guards), mod / compare.
            "function sum(n) { let s = 0; for (let i = 0; i < n; i++) { s = (s + i * 3) % 1000003; } return s; }
             var r1 = sum(5000);
             function mix(n) { let x = 0; for (let i = 0; i < n; i++) { x = i < n / 2 ? x + 1 : x + '!'; } return x; }
             var r2 = mix(2000).length;
             console.log(r1, r2, 7 % -3, -7 % 3, -0 % 5 === 0 && 1 / (-0 % 5));",
            // Shapes and inline caches: same constructor, then a different shape.
            "function P(x, y) { this.x = x; this.y = y; }
             P.prototype.len = function () { return this.x + this.y; };
             var t = 0; for (var i = 0; i < 3000; i++) { var p = new P(i, 1); t += p.len(); }
             var q = { y: 5, x: 1, len: P.prototype.len }; t += q.len();
             var o = new P(1, 2); Object.defineProperty(o, 'x', { get() { return 100; } }); t += o.len();
             console.log(t);",
            // Exceptions thrown and caught inside hot loops; finally.
            "function f(n) { let c = 0; for (let i = 0; i < n; i++) { try { if (i % 7 === 0) throw i; c++; } catch (e) { c += 2; } finally { c += 1; } } return c; }
             console.log(f(1000));",
            // A global read in a hot function, then shadowed by a later script.
            "window.h = 1; function rd() { return h; } var acc = 0; for (var i = 0; i < 2000; i++) acc += rd(); console.log(acc);",
            "let h = 5; console.log(rd());",
            // Top-level loop with var / let globals and closures over let.
            "var fs = []; for (let k = 0; k < 600; k++) fs.push(() => k); let tot = 0; for (var j = 0; j < fs.length; j++) tot += fs[j](); console.log(tot, typeof j, j);",
        ];
        for s in scripts {
            if let Err(e) = it.run_script(s) {
                panic!("script failed: {e}");
            }
        }
        assert_eq!(it.console, ["492389 1004 1 -1 -Infinity", "4501608", "2143", "2000", "5", "179700 number 600"]);
    }

    fn eval1(src: &str) -> String {
        run(&format!("console.log({src})")).join("\n")
    }

    #[test]
    fn language_compat() {
        let out = run(r#"
            var gv = 1; function gf() { return 2; } let lv = 3;
            console.log(window.gv, typeof window.gf, window.lv, gv + gf());
            var a = null;
            console.log(a?.b.c, a?.[0].x, a?.b(), a?.b.c(), ({x: {y: 5}}).x?.y);
            var jq = {length: 0}; Array.prototype.push.call(jq, 'p', 'q');
            console.log(jq.length, jq[1], [].slice.call({length: 2, 0: 'x', 1: 'y'}).join());
            function args() { return Array.prototype.map.call(arguments, x => x * 2).join(); }
            console.log(args(1, 2, 3));
            var m = new Map([[1, 'a'], [2, 'b']]);
            var mi = m.values(); console.log(mi.next().value, [...m.keys()].join(), typeof m[Symbol.iterator]);
            console.log(Object.prototype.toString.call(m), Object.prototype.toString.call(new Set()));
            var ai = [7, 8][Symbol.iterator](); ai.next(); console.log(JSON.stringify(ai.next()), JSON.stringify(ai.next()));
            console.log([...'héllo'].length, Array.from(new Set([1, 1, 2]).entries()).join('|'));
            console.log([1, 2, 1, 2].indexOf(1, 1), [1, 2, 1].lastIndexOf(1, 1), [1, 2, 3].includes(1, 1));
            console.log([1, 2, 3].with(1, 9).join(), [1, 2, 3, 4].copyWithin(0, 2).join(), [1, 2, 3].toSpliced(1, 1).join());
            function* g() { yield 1; yield 2; yield 3; }
            console.log(g().map(x => x * 10).toArray().join(), g().take(2).toArray().join(), typeof Iterator.prototype.map);
        "#);
        assert_eq!(
            out,
            vec![
                "1 function undefined 3",
                "undefined undefined undefined undefined 5",
                "2 q x,y",
                "2,4,6",
                "a 1,2 function",
                "[object Map] [object Set]",
                r#"{"value":8,"done":false} {"done":true}"#,
                "5 1,1|2,2",
                "2 0 false",
                "1,9,3 3,4,3,4 1,3",
                "10,20,30 1,2 function",
            ]
        );
    }

    #[test]
    fn custom_elements() {
        let out = run(r#"
            const root = document.createElement('div');
            document.appendChild(root);
            root.innerHTML = '<x-early a="1">e</x-early>';
            class XEl extends HTMLElement {
                static get observedAttributes() { return ['a']; }
                constructor() { super(); this.made = true; console.log('ctor', this.tagName); }
                connectedCallback() { console.log('connected', this.tagName, this.isConnected !== false); }
                disconnectedCallback() { console.log('disconnected', this.tagName); }
                attributeChangedCallback(n, o, v) { console.log('attr', n, o, v); }
                hello() { return 'hi ' + this.getAttribute('a'); }
            }
            customElements.define('x-early', XEl);
            const e = root.firstChild;
            console.log(e.made, e.hello(), e instanceof XEl, e instanceof HTMLElement);
            e.setAttribute('a', '2');
            e.setAttribute('b', '3');
            customElements.whenDefined('x-new').then(c => console.log('defined', c.name));
            class XNew extends HTMLElement { connectedCallback() { this.textContent = 'rendered'; } }
            customElements.define('x-new', XNew);
            const n = new XNew();
            console.log(n.tagName, n instanceof XNew);
            root.appendChild(n);
            console.log(n.textContent, document.createElement('x-new') instanceof XNew, customElements.get('x-new') === XNew);
            root.removeChild(e);
        "#);
        assert_eq!(
            out,
            vec![
                "ctor X-EARLY",
                "attr a null 1",
                "connected X-EARLY true",
                "true hi 1 true true",
                "attr a 1 2",
                "X-NEW true",
                "rendered true true",
                "disconnected X-EARLY",
                "defined XNew",
            ]
        );
    }

    #[test]
    fn mutation_observer() {
        let out = run(r#"
            const root = document.createElement('div');
            document.appendChild(root);
            const log = [];
            const mo = new MutationObserver((recs, obs) => {
                for (const r of recs) log.push(r.type + ':' + (r.attributeName || '') + ':' + r.addedNodes.length + r.removedNodes.length + ':' + (r.oldValue ?? '-'));
                console.log(log.join(' '), obs === mo);
            });
            mo.observe(root, { childList: true, attributes: true, subtree: true, attributeOldValue: true, characterData: true });
            const c = document.createElement('span');
            root.appendChild(c);
            c.setAttribute('class', 'x');
            c.setAttribute('class', 'y');
            root.removeChild(c);
            const t = document.createTextNode('a'); root.appendChild(t); t.data = 'b';
            console.log('sync', log.length);
            Promise.resolve().then(() => { mo.disconnect(); root.appendChild(document.createElement('i')); });
        "#);
        assert_eq!(
            out,
            vec![
                "sync 0".to_string(),
                "childList::10:- attributes:class:00:- attributes:class:00:x childList::01:- childList::10:- characterData::00:- true".to_string(),
            ]
        );
    }

    #[test]
    fn es_modules() {
        let dir = std::env::temp_dir().join(format!("bolt-modules-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let files = [
            ("main.js", r#"
                import def, { count, inc, fromB as b } from './a.js';
                import * as ns from './lib/all.js';
                import { util } from 'lib/';
                inc(); inc();
                console.log(def(), count, b(), Object.keys(ns).join(), ns.c, util);
                console.log(import.meta.url.endsWith('/main.js'), import.meta.resolve('./x.js').endsWith('/x.js'));
                const dyn = await import('./lib/c.js');
                console.log('dyn', dyn.c, dyn.default);
                export const done = true;
            "#),
            ("a.js", "import { b } from './b.js';\nexport let count = 0;\nexport function inc() { count++; }\nexport function fromB() { return b(); }\nexport default function () { return 'def'; }\nexport { fromB as fromBAlias };\n"),
            ("b.js", "import { count } from './a.js';\nexport function b() { return 'b sees ' + count; }\n"),
            ("lib/all.js", "export * from './c.js';\nexport * as cns from './c.js';\nexport const own = 1;\n"),
            ("lib/c.js", "await null;\nexport const c = 'C';\nexport default 'cdef';\n"),
            ("lib/util.js", "export const util = 'U';\n"),
        ];
        for (name, src) in files {
            std::fs::write(dir.join(name), src).unwrap();
        }
        let base = format!("file:///{}/", dir.display().to_string().replace('\\', "/").trim_start_matches('/'));
        let mut it = Interp::new(Document::new(), &base, 800.0, 600.0);
        it.add_import_map(&format!(r#"{{"imports": {{"lib/": "{base}lib/util.js?"}}}}"#), &base);
        // `lib/` maps to a prefix: "lib/" -> ".../lib/util.js?" + "".
        it.run_module(&format!("{base}main.js"), None);
        it.run_microtasks();
        it.run_timers(1e9);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            it.console,
            vec![
                "def 2 b sees 2 c,cns,own C U".to_string(),
                "true true".to_string(),
                "dyn C cdef".to_string(),
            ]
        );
    }

    #[test]
    fn location_history_and_documents() {
        let out = run(r#"
            console.log(location.pathname, location.host, location.origin, location.search === '');
            addEventListener('hashchange', e => console.log('hash', e.newURL));
            addEventListener('popstate', e => console.log('pop', JSON.stringify(e.state)));
            history.pushState({page: 2}, '', '/app/two?x=1');
            console.log(location.href, location.search, history.length, JSON.stringify(history.state));
            history.replaceState({page: 3}, '', 'three');
            console.log(location.pathname, history.length);
            location.hash = 'sec';
            console.log(location.hash, history.length);
            const d = document.implementation.createHTMLDocument('t');
            const div = d.createElement('div'); d.body.appendChild(div); div.innerHTML = '<b>x</b>';
            console.log(d.body.innerHTML, d.title === undefined ? 'no title' : 'ok', Node.ELEMENT_NODE, div.nodeType === Node.ELEMENT_NODE);
            const c = document.createComment('hi');
            console.log(c.nodeType, c.nodeName, c.data, document.createDocumentFragment().nodeType);
        "#);
        assert_eq!(
            out,
            vec![
                "/a/b.html example.com https://example.com true",
                "https://example.com/app/two?x=1 ?x=1 2 {\"page\":2}",
                "/app/three 2",
                "hash https://example.com/app/three#sec",
                "#sec 3",
                "<div><b>x</b></div> ok 1 true",
                "8 #comment hi 11",
            ]
        );
    }

    #[test]
    fn proxy_and_reflect() {
        let out = run(r#"
            const log = [];
            function reactive(o) {
                return new Proxy(o, {
                    get(t, k, r) { if (typeof k === 'string') log.push('get ' + k); return Reflect.get(t, k, r); },
                    set(t, k, v) { log.push('set ' + k + '=' + v); t[k] = v; return true; },
                    has(t, k) { return k === 'secret' ? false : k in t; },
                    deleteProperty(t, k) { log.push('del ' + k); delete t[k]; return true; },
                    ownKeys(t) { return Reflect.ownKeys(t).filter(k => k !== 'secret'); },
                });
            }
            const s = reactive({count: 1, secret: 2});
            s.count++;
            console.log(s.count, 'secret' in s, 'count' in s, Object.keys(s).join());
            delete s.count;
            console.log(log.join('|'));
            const f = new Proxy(function (a, b) { return a + b; }, { apply(t, th, args) { return t(...args) * 10; } });
            console.log(f(1, 2), typeof f);
            class P { constructor(x) { this.x = x; } }
            const C = new Proxy(P, { construct(t, args) { return new t(args[0] + 1); } });
            console.log(new C(1).x, Reflect.has({a: 1}, 'a'), Reflect.ownKeys({a: 1, b: 2}).join(), Reflect.apply(Math.max, null, [1, 5, 3]));
            const { proxy, revoke } = Proxy.revocable({}, {}); revoke();
            try { proxy.x; } catch (e) { console.log(e.name); }
        "#);
        assert_eq!(
            out,
            vec![
                "2 false true count",
                "get count|set count=2|get count|del count",
                "30 function",
                "2 true a,b 5",
                "TypeError",
            ]
        );
    }

    #[test]
    fn suspending_generators_and_async() {
        let out = run(r#"
            function* g() { var x = yield 1; console.log('got ' + x); try { yield 2; } finally { console.log('fin'); } return 3; }
            var it = g();
            console.log(JSON.stringify(it.next()), JSON.stringify(it.next('a')), JSON.stringify(it.return(9)), JSON.stringify(it.next()));
            function* nat() { var i = 0; while (true) yield i++; }
            var s = 0; for (var n of nat()) { if (n > 5) break; s += n; } console.log('sum ' + s);
            function* inner() { var r = yield 'i1'; return 'ret ' + r; }
            function* outer() { var v = yield* inner(); yield v; }
            var o = outer(); o.next(); console.log(o.next('X').value);
            async function f() {
                console.log('a1'); await null; console.log('a2');
                var v = await new Promise(r => setTimeout(() => r(42), 10));
                console.log('a3 ' + v); return v + 1;
            }
            f().then(v => console.log('done ' + v));
            async function t() { try { await Promise.reject(new Error('boom')); } catch (e) { console.log('caught ' + e.message); } }
            t();
            async function* ag() { yield 1; await null; yield 2; }
            (async () => { for await (const x of ag()) console.log('ag ' + x); })();
            async function deep(n) { return n ? deep(n - 1) : 'bottom'; }
            async function rec(n) { await null; return n ? 1 + await rec(n - 1) : 0; }
            rec(200).then(v => console.log('rec ' + v));
            function boom(n) { return boom(n + 1); }
            (async () => { await null; try { boom(0); } catch (e) { console.log(e.name); } })();
            console.log('sync end');
        "#);
        assert_eq!(
            out,
            vec![
                "got a",
                "fin",
                r#"{"value":1,"done":false} {"value":2,"done":false} {"value":9,"done":true} {"done":true}"#,
                "sum 15",
                "ret X",
                "a1",
                "sync end",
                "a2",
                "caught boom",
                "RangeError",
                "ag 1",
                "ag 2",
                "rec 200",
                "a3 42",
                "done 43",
            ]
        );
    }

    #[test]
    fn basics() {
        assert_eq!(eval1("1 + 2 * 3"), "7");
        assert_eq!(eval1("'a' + 1 + 2"), "a12");
        assert_eq!(eval1("2 ** 10"), "1024");
        assert_eq!(eval1("7 % 3 - -1"), "2");
        assert_eq!(eval1("0.1 + 0.2"), "0.30000000000000004");
        assert_eq!(eval1("1 / 0"), "Infinity");
        assert_eq!(eval1("typeof null + typeof undefined + typeof (() => 1)"), "objectundefinedfunction");
        assert_eq!(eval1("[1, 2, 3].map(x => x * 2).join('-')"), "2-4-6");
        assert_eq!(eval1("null ?? 'd'"), "d");
        assert_eq!(eval1("({a: {b: 1}}).a?.c?.d"), "undefined");
        assert_eq!(eval1("'5' == 5 && '5' !== 5"), "true");
        assert_eq!(eval1("`x=${1 + 1}`"), "x=2");
        assert_eq!(eval1("(5).toFixed(2)"), "5.00");
        assert_eq!(eval1("parseInt('ff', 16) + parseFloat('1.5e1')"), "270");
        assert_eq!(eval1("1e21"), "1e+21");
        assert_eq!(eval1("-7 >> 1"), "-4");
        assert_eq!(eval1("-7 >>> 28"), "15");
    }

    #[test]
    fn functions_and_closures() {
        let out = run(r#"
            function counter() { let n = 0; return { inc: () => ++n, get value() { return n; } }; }
            const c = counter(); c.inc(); c.inc();
            console.log(c.value);
            const fns = []; for (let i = 0; i < 3; i++) fns.push(() => i);
            console.log(fns.map(f => f()).join(','));
            function sum(...xs) { return xs.reduce((a, b) => a + b, 0); }
            console.log(sum(...[1, 2, 3], 4));
            const { a, b: [x, y = 9], ...rest } = { a: 1, b: [2], c: 3, d: 4 };
            console.log(a, x, y, JSON.stringify(rest));
            function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); }
            console.log(fact(10));
            var hoisted = typeof later; function later() {}
            console.log(hoisted);
            console.log((function () { return arguments.length; })(1, 2, 3));
            const bound = function (p) { return this.v + p; }.bind({ v: 10 });
            console.log(bound(5));
        "#);
        assert_eq!(out, ["2", "0,1,2", "10", "1 2 9 {\"c\":3,\"d\":4}", "3628800", "function", "3", "15"]);
    }

    #[test]
    fn classes() {
        let out = run(r#"
            class Animal {
                static count = 0;
                legs = 4;
                constructor(name) { this.name = name; Animal.count++; }
                speak() { return `${this.name} makes a sound`; }
                get kind() { return 'animal'; }
            }
            class Dog extends Animal {
                constructor(name) { super(name); this.tricks = []; }
                speak() { return super.speak() + ' (woof)'; }
            }
            const d = new Dog('Rex');
            console.log(d.speak(), d.legs, d.kind, Animal.count, d instanceof Animal, d instanceof Dog);
            class MyErr extends Error { constructor(m) { super(m); this.name = 'MyErr'; } }
            try { throw new MyErr('boom'); } catch (e) { console.log(e.name, e.message, e instanceof Error); }
        "#);
        assert_eq!(out, ["Rex makes a sound (woof) 4 animal 1 true true", "MyErr boom true"]);
    }

    #[test]
    fn control_flow() {
        let out = run(r#"
            let s = '';
            outer: for (const i of [1, 2, 3]) { for (const j of [1, 2, 3]) { if (j === 2) continue outer; if (i === 3) break outer; s += i + '' + j + ' '; } }
            console.log(s.trim());
            switch (3) { case 1: console.log('one'); case 3: console.log('three'); case 4: console.log('four'); break; default: console.log('no'); }
            function f() { try { return 'try'; } finally { console.log('finally'); } }
            console.log(f());
            let k = 0; do { k++; } while (k < 5); console.log(k);
            const o = { x: 1, y: 2 }; const keys = []; for (const key in o) keys.push(key); console.log(keys.join());
            try { undefinedFn(); } catch (e) { console.log(e instanceof ReferenceError, e.message); }
        "#);
        assert_eq!(out, ["11 21", "three", "four", "finally", "try", "5", "x,y", "true undefinedFn is not defined"]);
    }

    #[test]
    fn library() {
        let out = run(r#"
            console.log(JSON.stringify({ a: [1, 'x', null, true], b: { c: 1.5 } }));
            console.log(JSON.parse('{"a":[1,2,{"b":"é"}]}').a[2].b);
            console.log('Hello World'.replace(/o/g, '0'), 'a-b-c'.split('-').length, ' x '.trim() + '|');
            console.log('2024-03-15'.match(/(\d+)-(\d+)-(\d+)/).slice(1).join('/'));
            console.log('abc'.replace(/(b)/, '[$1]'), 'aaa'.replace(/a/g, (m, i) => i));
            const m = new Map([[1, 'one']]); m.set('two', 2); console.log(m.size, m.get(1), m.has('two'));
            const st = new Set([1, 2, 2, 3]); console.log(st.size, [...st].join(''));
            console.log([3, 1, 10, 2].sort().join(), [3, 1, 10, 2].sort((a, b) => a - b).join());
            console.log(Object.entries({ p: 1, q: 2 }).map(([k, v]) => k + v).join());
            console.log(new Date(Date.UTC(2020, 1, 29, 12, 0, 0)).toISOString());
            console.log([1, [2, [3, [4]]]].flat(2).length, Array.from({ length: 3 }, (_, i) => i * i).join());
            console.log(Math.max(1, 5, 3), Math.round(2.5), Math.floor(-1.5), (255).toString(16), (1234567.891).toLocaleString());
            console.log(encodeURIComponent('a b&c'), decodeURIComponent('%E2%9C%93'));
            console.log(/^[\w.]+@\w+\.\w+$/.test('a.b@c.de'), 'x'.padStart(3, '-'));
        "#);
        assert_eq!(out[0], r#"{"a":[1,"x",null,true],"b":{"c":1.5}}"#);
        assert_eq!(out[1], "é");
        assert_eq!(out[2], "Hell0 W0rld 3 x|");
        assert_eq!(out[3], "2024/03/15");
        assert_eq!(out[4], "a[b]c 012");
        assert_eq!(out[5], "2 one true");
        assert_eq!(out[6], "3 123");
        assert_eq!(out[7], "1,10,2,3 1,2,3,10");
        assert_eq!(out[8], "p1,q2");
        assert_eq!(out[9], "2020-02-29T12:00:00.000Z");
        assert_eq!(out[10], "4 0,1,4");
        assert_eq!(out[11], "5 3 -2 ff 1,234,567.891");
        assert_eq!(out[12], "a%20b%26c ✓");
        assert_eq!(out[13], "true --x");
    }

    #[test]
    fn promises_and_timers() {
        let out = run(r#"
            const order = [];
            setTimeout(() => order.push('timeout'), 0);
            Promise.resolve(1).then(v => { order.push('then' + v); return v + 1; }).then(v => order.push('then' + v));
            queueMicrotask(() => order.push('micro'));
            order.push('sync');
            async function f() { const v = await Promise.resolve(42); return v + 1; }
            f().then(v => order.push('async' + v));
            setTimeout(() => console.log(order.join(' ')), 10);
            Promise.all([1, Promise.resolve(2)]).then(v => console.log('all', v.join()));
            new Promise((_, rej) => rej(new Error('nope'))).catch(e => console.log('caught', e.message));
        "#);
        // The rejection's handler runs in the first microtask tick; the
        // aggregate promise of Promise.all only resolves in the second.
        assert_eq!(out[0], "caught nope");
        assert_eq!(out[1], "all 1,2");
        assert_eq!(out[2], "sync then1 micro then2 async43 timeout");
    }

    #[test]
    fn generators() {
        let out = run(r#"
            function* range(n) { for (let i = 0; i < n; i++) yield i; return 'end'; }
            console.log([...range(4)].join());
            const g = range(2);
            console.log(JSON.stringify([g.next(), g.next(), g.next(), g.next()]));
            function* nat() { let i = 0; while (true) yield i++; }
            const firstFive = []; for (const x of nat()) { if (x >= 5) break; firstFive.push(x); }
            console.log(firstFive.join());
            function* both() { yield* [1, 2]; yield* range(2); }
            console.log(Array.from(both()).join());
            const obj = { *[Symbol.iterator]() { yield 'a'; yield 'b'; } };
            console.log([...obj].join(''));
            class Tree { constructor(v, kids = []) { this.v = v; this.kids = kids; }
              *walk() { yield this.v; for (const k of this.kids) yield* k.walk(); } }
            console.log([...new Tree(1, [new Tree(2, [new Tree(3)]), new Tree(4)]).walk()].join());
            async function* agen() { yield 1; yield await Promise.resolve(2); }
            (async () => { const out = []; for await (const v of agen()) out.push(v); console.log('async', out.join()); })();
        "#);
        assert_eq!(out, ["0,1,2,3", r#"[{"value":0,"done":false},{"value":1,"done":false},{"value":"end","done":true},{"done":true}]"#, "0,1,2,3,4", "1,2,0,1", "ab", "1,2,3,4", "async 1,2"]);
    }

    #[test]
    fn dom() {
        let doc = crate::html::parse("<div id=app><p class='x y'>Hello</p><ul><li>a</li><li>b</li></ul></div>");
        let mut it = Interp::new(doc, "about:blank", 800.0, 600.0);
        it.run_script(r#"
            const app = document.getElementById('app');
            const p = document.querySelector('#app .x');
            p.textContent = 'Hi';
            p.classList.add('z'); p.classList.remove('y');
            const li = document.createElement('li'); li.textContent = 'c'; li.setAttribute('data-n', '3');
            document.querySelector('ul').appendChild(li);
            console.log(document.querySelectorAll('li').length, p.className, li.dataset.n, app.children.length);
            let clicks = 0; app.addEventListener('click', e => { clicks++; e.preventDefault(); });
            li.click();
            console.log(clicks, li.parentNode.tagName, p.style.color === '');
            p.style.color = 'red';
            app.innerHTML += '<b>new</b>';
            console.log(app.querySelector('b').textContent, document.body.innerHTML.length > 20);
        "#).unwrap();
        assert_eq!(it.console, ["3 x z 3 2", "1 UL true", "new true"]);
        assert!(it.dirty);
        let mut s = String::new();
        it.doc.inner_html(it.doc.body, &mut s);
        assert!(s.contains("style=\"color: red\""), "{s}");
    }
}
