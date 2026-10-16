//! A small JavaScript interpreter: tree-walking evaluator over the AST from
//! `parser`, with prototype objects, closures, classes, exceptions,
//! promises/timers and a DOM binding (see `dom.rs`).

pub mod binary;
pub mod builtins;
pub mod coro;
pub mod dombind;
pub mod generator;
pub mod lexer;
pub mod parser;
pub mod regex;
pub mod web;

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
    Null,
    Bool(bool),
    Num(f64),
    Str(Rc<str>),
    Obj(ObjRef),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Undef => write!(f, "undefined"),
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(n) => write!(f, "{}", num_to_str(*n)),
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
        matches!(self, Value::Undef | Value::Null)
    }
    pub fn truthy(&self) -> bool {
        match self {
            Value::Undef | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
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
        return format!("{n:.0}");
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

/// Insertion-ordered property map (linear for small objects, indexed when large).
#[derive(Default, Clone)]
pub struct Props {
    pub list: Vec<(Rc<str>, Prop)>,
    index: Option<FxHashMap<Rc<str>, usize>>,
}

impl Props {
    pub fn find(&self, k: &str) -> Option<usize> {
        match &self.index {
            Some(ix) => ix.get(k).copied(),
            None => self.list.iter().position(|(n, _)| &**n == k),
        }
    }
    pub fn get(&self, k: &str) -> Option<&Prop> {
        self.find(k).map(|i| &self.list[i].1)
    }
    pub fn get_mut(&mut self, k: &str) -> Option<&mut Prop> {
        self.find(k).map(move |i| &mut self.list[i].1)
    }
    pub fn insert(&mut self, k: Rc<str>, p: Prop) {
        if let Some(i) = self.find(&k) {
            self.list[i].1 = p;
            return;
        }
        if let Some(ix) = &mut self.index {
            ix.insert(k.clone(), self.list.len());
        }
        self.list.push((k, p));
        if self.index.is_none() && self.list.len() > 12 {
            self.index = Some(self.list.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect());
        }
    }
    pub fn remove(&mut self, k: &str) -> bool {
        if let Some(i) = self.find(k) {
            self.list.remove(i);
            if self.index.is_some() {
                self.index = Some(self.list.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect());
            }
            true
        } else {
            false
        }
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
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
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
    /// ArrayBuffer contents.
    Buffer(Vec<u8>),
    /// A typed array view onto an ArrayBuffer.
    Typed(binary::View),
    DataView(binary::View),
    /// Headers / URLSearchParams / FormData entries.
    Pairs(web::PairKind, Vec<(Rc<str>, Rc<str>)>),
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

#[derive(Default)]
pub struct Protos {
    pub object: Option<ObjRef>,
    pub function: Option<ObjRef>,
    pub array: Option<ObjRef>,
    pub string: Option<ObjRef>,
    pub number: Option<ObjRef>,
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
    pub budget: u64,
    pub timers: Vec<Timer>,
    pub next_timer: u32,
    pub micro: VecDeque<(Value, Vec<Value>)>,
    /// Event listeners: (target node or WINDOW, type, callback).
    pub listeners: Vec<(u32, Rc<str>, Value)>,
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
    /// External scripts inserted by scripts, waiting to be fetched and run.
    pub pending_scripts: Vec<String>,
    pub cookie: String,
    /// Element border boxes from the last layout (x, y, w, h).
    pub rects: FxHashMap<NodeId, [f32; 4]>,
    pub computed: FxHashMap<NodeId, Vec<(String, String)>>,
    pub start: std::time::Instant,
    /// Label of the loop about to execute (for labeled break/continue).
    label: Option<Name>,
    /// Scripts are interrupted after this instant.
    pub deadline: Option<std::time::Instant>,
    /// Values yielded by the generators currently running.
    gen_sinks: Vec<Vec<Value>>,
    /// Nesting depth of JS function calls (per coroutine).
    pub depth: u32,
    /// Generators / async calls whose coroutines are running, innermost last.
    pub co_stack: Vec<ObjRef>,
    /// Navigation requested by a script (`location.assign`, `form.submit()`).
    pub nav: Option<crate::forms::Nav>,
    /// A form a script submitted, to be serialized by the page.
    pub submit_form: Option<NodeId>,
    /// fetch / XMLHttpRequest requests in flight.
    pub requests: Vec<web::Pending>,
}

pub const WINDOW: u32 = u32::MAX - 1;

pub fn type_error(msg: impl Into<String>) -> Value {
    builtins::make_error_value("TypeError", &msg.into())
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
            budget: 50_000_000,
            timers: Vec::new(),
            next_timer: 1,
            micro: VecDeque::new(),
            listeners: Vec::new(),
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
            computed: FxHashMap::default(),
            start: std::time::Instant::now(),
            label: None,
            deadline: None,
            gen_sinks: Vec::new(),
            depth: 0,
            co_stack: Vec::new(),
            nav: None,
            submit_form: None,
            requests: Vec::new(),
        });
        builtins::install(&mut it);
        binary::install(&mut it);
        dombind::install(&mut it);
        web::install(&mut it);
        if let Err(e) = it.run_script(include_str!("prelude.js")) {
            eprintln!("js prelude: {e}");
        }
        it
    }

    // ------------------------------------------------------------- objects

    pub fn new_obj(&self, proto: Option<ObjRef>, kind: Kind) -> ObjRef {
        Rc::new(RefCell::new(Obj { props: Props::default(), proto, kind }))
    }

    pub fn plain(&self) -> ObjRef {
        self.new_obj(self.p.object.clone(), Kind::Plain)
    }

    pub fn array(&self, v: Vec<Value>) -> Value {
        Value::Obj(self.new_obj(self.p.array.clone(), Kind::Array(v)))
    }

    pub fn native(&self, f: NativeFn, name: &'static str) -> Value {
        Value::Obj(self.new_obj(self.p.function.clone(), Kind::Func(Func::Native(f, name))))
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
        matches!(v, Value::Obj(o) if matches!(o.borrow().kind, Kind::Func(_)))
    }

    #[inline]
    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps & 0xFFFF == 0 {
            if self.steps > self.budget || self.deadline.is_some_and(|d| std::time::Instant::now() > d) {
                return Err(builtins::make_error_value("Error", "script took too long and was stopped"));
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------- conversion

    pub fn to_str(&mut self, v: &Value) -> R<Rc<str>> {
        Ok(match v {
            Value::Undef => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => if *b { "true".into() } else { "false".into() },
            Value::Num(n) => num_to_str(*n).into(),
            Value::Str(s) => s.clone(),
            Value::Obj(_) => {
                let p = self.to_primitive(v, true)?;
                return self.to_str(&p);
            }
        })
    }

    pub fn to_num(&mut self, v: &Value) -> R<f64> {
        Ok(match v {
            Value::Undef => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => *b as u8 as f64,
            Value::Num(n) => *n,
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
            Value::Undef => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(s) => if s.starts_with("@@sym:") { "symbol" } else { "string" },
            Value::Obj(o) => {
                if matches!(o.borrow().kind, Kind::Func(_)) { "function" } else { "object" }
            }
        }
    }

    // ---------------------------------------------------------- properties

    fn proto_of_prim(&self, v: &Value) -> Option<ObjRef> {
        match v {
            Value::Str(_) => self.p.string.clone(),
            Value::Num(_) => self.p.number.clone(),
            Value::Bool(_) => self.p.boolean.clone(),
            Value::Obj(o) => Some(o.clone()),
            _ => None,
        }
    }

    pub fn get(&mut self, target: &Value, key: &str) -> R<Value> {
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
                {
                    let ob = o.borrow();
                    match &ob.kind {
                        Kind::Array(a) => {
                            if key == "length" {
                                return Ok(Value::Num(a.len() as f64));
                            }
                            if let Some(i) = array_index(key) {
                                return Ok(a.get(i).cloned().unwrap_or_default());
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
                if special {
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
                    return Ok(a.get(*n as usize).cloned().unwrap_or_default());
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
                        a.resize(i + 1, Value::Undef);
                    }
                    a[i] = v;
                    return Ok(());
                }
                if key == "length" {
                    drop(ob);
                    let n = self.to_num(&v)? as usize;
                    if let Kind::Array(a) = &mut o.borrow_mut().kind {
                        a.resize(n, Value::Undef);
                    }
                    return Ok(());
                }
            }
        }
        let special = matches!(o.borrow().kind, Kind::Node(_) | Kind::Style(_) | Kind::ClassList(_) | Kind::Dataset(_));
        if special && dombind::set(self, o, key, &v)? {
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
                        a.resize(i + 1, Value::Undef);
                    }
                    a[i] = v;
                    return Ok(());
                }
            }
        }
        let k = self.key(key)?;
        self.put(target, &k, v)
    }

    pub fn has_prop(&mut self, target: &Value, key: &str) -> bool {
        if let Value::Obj(o) = target {
            if let Kind::Array(a) = &o.borrow().kind {
                if let Some(i) = array_index(key) {
                    return i < a.len();
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
            Value::Obj(o) => {
                let ob = o.borrow();
                let mut out: Vec<Rc<str>> = Vec::new();
                match &ob.kind {
                    Kind::Array(a) => out.extend((0..a.len()).map(|i| Rc::from(i.to_string()))),
                    Kind::Typed(t) => out.extend((0..t.len).map(|i| Rc::from(i.to_string()))),
                    _ => {}
                }
                out.extend(ob.props.list.iter().filter(|(k, p)| p.enumerable && !k.starts_with("@@")).map(|(k, _)| k.clone()));
                out
            }
            _ => Vec::new(),
        }
    }

    // --------------------------------------------------------------- calls

    pub fn call(&mut self, f: &Value, this: Value, args: &[Value]) -> R<Value> {
        self.tick()?;
        let Value::Obj(fo) = f else {
            return Err(type_error(format!("{:?} is not a function", f)));
        };
        enum C {
            Closure(Rc<FuncDef>, Env, Option<Value>, Option<ObjRef>),
            Class,
            Native(NativeFn),
            Bound(Value, Value, Vec<Value>),
        }
        let c = match &fo.borrow().kind {
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
                r
            }
            C::Bound(t, th, mut a) => {
                a.extend_from_slice(args);
                self.call(&t, th, &a)
            }
            C::Class => Err(type_error("Class constructor cannot be invoked without 'new'")),
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
        let r = self.call_closure_inner(def, env, arrow_this, home, this, args, fobj);
        self.depth -= 1;
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn call_closure_inner(&mut self, def: &Rc<FuncDef>, env: Env, arrow_this: Option<Value>, home: Option<ObjRef>, this: Value, args: &[Value], fobj: Option<ObjRef>) -> R<Value> {
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
                // Native parents (Error, Map, ...): build an instance and adopt its state.
                let inst = self.construct(parent, args)?;
                if let (Value::Obj(src), Value::Obj(dst)) = (&inst, this) {
                    let (props, kind) = {
                        let mut s = src.borrow_mut();
                        (std::mem::take(&mut s.props), std::mem::replace(&mut s.kind, Kind::Plain))
                    };
                    let mut d = dst.borrow_mut();
                    for (k, p) in props.list {
                        d.props.insert(k, p);
                    }
                    d.kind = kind;
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
            if let Some(v) = s.vars.borrow().get(name) {
                return Ok(v.clone());
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
        {
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
                env.vars.borrow_mut().insert(def.name.clone().unwrap(), f);
            }
        }
        Ok(())
    }

    pub fn make_closure(&mut self, def: &Rc<FuncDef>, env: &Env) -> Value {
        let this = if def.arrow { Some(Self::this_val(env)) } else { None };
        let home = if def.arrow { Self::home_of(env) } else { None };
        Value::Obj(self.new_obj(self.p.function.clone(), Kind::Func(Func::Closure { def: def.clone(), env: env.clone(), this, home })))
    }

    // ---------------------------------------------------------- statements

    pub fn run_script(&mut self, src: &str) -> Result<(), String> {
        let prog = parse_program(src)?;
        let env = self.genv.clone();
        let r = (|| -> R<()> {
            self.hoist(&prog, &env)?;
            for s in &prog {
                self.exec(s, &env)?;
            }
            Ok(())
        })();
        r.map_err(|e| self.describe_error(&e))
    }

    pub fn describe_error(&mut self, e: &Value) -> String {
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
                return Err(v);
            }
            Stmt::Try(block, catch, finally) => {
                let r = self.block(block, env);
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
                let inherited: Vec<Rc<str>> = p.borrow().props.list.iter().filter(|(k, pr)| pr.enumerable && !k.starts_with("@@")).map(|(k, _)| k.clone()).collect();
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
        if matches!(o.borrow().kind, Kind::Array(_) | Kind::Set(_) | Kind::Map(_) | Kind::Typed(_)) {
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
                let kind_items = match &o.borrow().kind {
                    Kind::Array(a) => Some(a.clone()),
                    Kind::Set(s) => Some(s.clone()),
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
                ArrItem::Hole => out.push(Value::Undef),
            }
        }
        Ok(out)
    }

    pub fn eval(&mut self, e: &Expr, env: &Env) -> R<Value> {
        Ok(match e {
            Expr::Num(n) => Value::Num(*n),
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
                let old = self.eval(target, env)?;
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
                let ov = self.eval(o, env)?;
                if *opt && ov.is_nullish() {
                    return Ok(Value::Undef);
                }
                self.get(&ov, k)?
            }
            Expr::Index(o, k, opt) => {
                let ov = self.eval(o, env)?;
                if *opt && ov.is_nullish() {
                    return Ok(Value::Undef);
                }
                let kv = self.eval(k, env)?;
                self.get_v(&ov, &kv)?
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
                let ov = if let Expr::Super = **o { Self::this_val(env) } else { self.eval(o, env)? };
                if *mopt && ov.is_nullish() {
                    return Ok(Value::Undef);
                }
                let f = if let Expr::Super = **o {
                    let sup = self.eval(o, env)?;
                    self.get(&sup, k)?
                } else {
                    self.get(&ov, k)?
                };
                if !Self::is_callable(&f) {
                    if opt && f.is_nullish() {
                        return Ok(Value::Undef);
                    }
                    return Err(type_error(format!("{}.{k} is not a function", expr_name(o))));
                }
                (f, ov)
            }
            Expr::Index(o, k, mopt) => {
                let ov = self.eval(o, env)?;
                if *mopt && ov.is_nullish() {
                    return Ok(Value::Undef);
                }
                let kv = self.eval(k, env)?;
                (self.get_v(&ov, &kv)?, ov)
            }
            _ => (self.eval(callee, env)?, Value::Undef),
        };
        if opt && f.is_nullish() {
            return Ok(Value::Undef);
        }
        if !Self::is_callable(&f) {
            return Err(type_error(format!("{} is not a function", expr_name(callee))));
        }
        let a = self.spread_args(args, env)?;
        self.call(&f, this, &a)
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

    fn delete(&mut self, o: &Value, k: &str) -> bool {
        if let Value::Obj(ob) = o {
            let mut b = ob.borrow_mut();
            if let Kind::Array(a) = &mut b.kind {
                if let Some(i) = array_index(k) {
                    if i < a.len() {
                        a[i] = Value::Undef;
                    }
                    return true;
                }
            }
            b.props.remove(k);
        }
        true
    }

    pub fn binary(&mut self, op: &str, a: &Value, b: &Value) -> R<Value> {
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
            (Value::Undef | Value::Null, Value::Undef | Value::Null) => true,
            (Value::Undef | Value::Null, _) | (_, Value::Undef | Value::Null) => false,
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
            let fv = Value::Obj(self.new_obj(self.p.function.clone(), Kind::Func(Func::Closure { def: f.clone(), env: scope.clone(), this: None, home: Some(target.clone()) })));
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
        let mut n = 0;
        while let Some((f, args)) = self.micro.pop_front() {
            if let Err(e) = self.call(&f, Value::Undef, &args) {
                let m = self.describe_error(&e);
                self.log(&format!("Uncaught (in promise) {m}"));
            }
            n += 1;
            if n > 100_000 {
                break;
            }
        }
    }

    /// Run timers that are due at `now` (ms). Returns how many ran.
    pub fn run_timers(&mut self, now: f64) -> usize {
        self.now = now;
        let mut ran = 0;
        for _ in 0..1000 {
            let Some(idx) = self.timers.iter().enumerate().filter(|(_, t)| t.at <= now).min_by(|a, b| a.1.at.partial_cmp(&b.1.at).unwrap()).map(|(i, _)| i) else {
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
        Expr::Num(_) | Expr::Str(_) | Expr::Regex(..) | Expr::Ident(_) | Expr::This | Expr::Super | Expr::Null | Expr::Bool(_) => false,
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().any(expr_has_closure),
        Expr::TaggedTemplate(t, _, xs) => expr_has_closure(t) || xs.iter().any(expr_has_closure),
        Expr::Array(v) => items(v),
        Expr::Object(ps) => ps.iter().any(|p| match p {
            PropDef::Value(k, v) => matches!(k, PropKey::Computed(e) if expr_has_closure(e)) || expr_has_closure(v),
            PropDef::Getter(..) | PropDef::Setter(..) => true,
            PropDef::Spread(e) => expr_has_closure(e),
        }),
        Expr::Unary(_, x) | Expr::Await(x) => expr_has_closure(x),
        Expr::Yield(x, _) => x.as_deref().is_some_and(expr_has_closure),
        Expr::Update { target, .. } => expr_has_closure(target),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) | Expr::Index(a, b, _) => expr_has_closure(a) || expr_has_closure(b),
        Expr::Assign(_, p, v) => pat_has_closure(p) || expr_has_closure(v),
        Expr::Cond(a, b, c) => expr_has_closure(a) || expr_has_closure(b) || expr_has_closure(c),
        Expr::Call(f, a, _) | Expr::New(f, a) => expr_has_closure(f) || items(a),
        Expr::Member(o, _, _) => expr_has_closure(o),
    }
}

fn name_fn(v: &Value, p: &Pattern) {
    if let (Value::Obj(o), Pattern::Ident(n)) = (v, p) {
        let mut ob = o.borrow_mut();
        if matches!(ob.kind, Kind::Func(Func::Closure { .. }) | Kind::Func(Func::Class(_))) && ob.props.get("name").is_none() {
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
        Expr::Str(s) => Expr::Str(s.clone()),
        Expr::Member(o, k, opt) => Expr::Member(Box::new(clone_simple(o)?), k.clone(), *opt),
        Expr::Index(o, k, opt) => Expr::Index(Box::new(clone_simple(o)?), Box::new(clone_simple(k)?), *opt),
        Expr::Binary(op, a, b) => Expr::Binary(op, Box::new(clone_simple(a)?), Box::new(clone_simple(b)?)),
        _ => return Err(type_error("unsupported update target")),
    })
}

fn expr_name(e: &Expr) -> String {
    match e {
        Expr::Ident(n) => n.to_string(),
        Expr::Member(o, k, _) => format!("{}.{k}", expr_name(o)),
        Expr::This => "this".into(),
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
        (Value::Undef, Value::Undef) | (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Num(x), Value::Num(y)) => x == y,
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

pub fn utf16_len(s: &str) -> usize {
    if s.is_ascii() { s.len() } else { s.encode_utf16().count() }
}

pub fn utf16_at(s: &str, i: usize) -> Option<String> {
    if s.is_ascii() {
        return s.get(i..i + 1).map(str::to_owned);
    }
    let u: Vec<u16> = s.encode_utf16().collect();
    u.get(i).map(|c| String::from_utf16_lossy(&[*c]))
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

    fn eval1(src: &str) -> String {
        run(&format!("console.log({src})")).join("\n")
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
