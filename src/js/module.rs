//! ES modules: loading (with import maps), linking and evaluation.
//!
//! Each module gets its own scope. Imported names are bound in it to marker
//! strings (`@@imp:<module>:<name>`) that `lookup` resolves on every read
//! through the exporting module's scope, so bindings are live and cyclic
//! imports work. A module's dependencies are fetched in one parallel batch
//! per level of the graph. Bodies run like async functions (top-level
//! `await`).

use super::builtins::{self, arg};
use super::parser::{self, ExportEntry, ImportName, ModuleInfo, Stmt};
use super::*;

pub const IMPORT_MARK: &str = "@@imp:";

#[derive(Clone, Copy, PartialEq)]
enum ModState {
    Linking,
    Linked,
    Evaluated,
}

pub struct Module {
    pub url: String,
    env: Env,
    body: Rc<Vec<Stmt>>,
    info: Rc<ModuleInfo>,
    /// Module ids of `info.requests`.
    deps: Vec<usize>,
    state: ModState,
    ns: Option<ObjRef>,
    /// Settles when the body (including top-level awaits) finishes.
    promise: Option<Value>,
}

enum Binding {
    Local(Name),
    Namespace,
}

fn err(kind: &str, msg: String) -> Value {
    builtins::make_error_value(kind, &msg)
}

impl Interp {
    /// Resolve a module specifier against `base` (relative URLs, absolute
    /// URLs and import-map entries; bare names need an import map).
    pub fn resolve_specifier(&self, base: &str, spec: &str) -> R<String> {
        for (k, v) in &self.import_map {
            if spec == k {
                return Ok(v.clone());
            }
            if k.ends_with('/') && spec.starts_with(k.as_str()) {
                return Ok(format!("{v}{}", &spec[k.len()..]));
            }
        }
        let has_scheme = spec.find(':').is_some_and(|i| i > 1 && spec[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
        if spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../") || has_scheme {
            return Ok(crate::net::resolve(base, spec));
        }
        Err(err("TypeError", format!("Failed to resolve module specifier \"{spec}\"")))
    }

    /// Add the `imports` of an import map (`<script type=importmap>`).
    pub fn add_import_map(&mut self, json: &str, base: &str) {
        let mut parse = || -> R<Vec<(String, String)>> {
            let j = self.get(&Value::Obj(self.global.clone()), "JSON")?;
            let f = self.get(&j, "parse")?;
            let m = self.call(&f, j, &[Value::str(json)])?;
            let imports = self.get(&m, "imports")?;
            let mut out = Vec::new();
            if matches!(imports, Value::Obj(_)) {
                for k in self.own_keys(&imports) {
                    let v = self.get(&imports, &k)?;
                    if let Value::Str(v) = v {
                        out.push((k.to_string(), crate::net::resolve(base, &v)));
                    }
                }
            }
            Ok(out)
        };
        match parse() {
            Ok(entries) => {
                self.import_map.extend(entries);
                // Longest keys first, so the most specific prefix wins.
                self.import_map.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
            }
            Err(e) => {
                let m = self.describe_error(&e);
                self.log(&format!("invalid import map: {m}"));
            }
        }
    }

    fn fetch_module_source(&mut self, url: &str) -> R<String> {
        if let Some(s) = self.module_sources.remove(url) {
            return Ok(s);
        }
        // (Waiting on the network doesn't count against the task's time.)
        let t = std::time::Instant::now();
        let res = crate::net::http("GET", url, &[], None);
        if let Some(d) = self.deadline {
            self.deadline = Some(d + t.elapsed());
        }
        match res {
            Ok(r) if (200..300).contains(&r.status) || r.status == 0 => Ok(crate::net::decode(&r.body)),
            Ok(r) => Err(err("TypeError", format!("Failed to fetch module {url} (HTTP {})", r.status))),
            Err(e) => Err(err("TypeError", format!("Failed to fetch module {url}: {e}"))),
        }
    }

    /// Load, parse and link the module graph rooted at `url`. `src` is given
    /// for inline module scripts (which are not registered by URL).
    pub fn load_module(&mut self, url: &str, src: Option<String>) -> R<usize> {
        let inline = src.is_some();
        if !inline {
            if let Some(&id) = self.module_map.get(url) {
                return Ok(id);
            }
        }
        let src = match src {
            Some(s) => s,
            None => {
                if crate::js::trace_scripts() {
                    eprintln!("[script] load module {url}");
                }
                self.fetch_module_source(url)?
            }
        };
        let (body, info) = parser::parse_module(&src).map_err(|e| err("SyntaxError", format!("{} ({url})", e.trim_start_matches("SyntaxError: "))))?;
        let env = new_scope(Some(self.genv.clone()), Some(Value::Undef), None, true);
        let meta = self.plain();
        Interp::set_prop(&meta, "url", Value::str(url));
        let resolve = builtins::bound_native(self, meta_resolve, Value::str(url));
        Interp::set_prop(&meta, "resolve", resolve);
        {
            let mut vars = env.vars.borrow_mut();
            vars.insert("@@module_url".into(), Value::str(url));
            vars.insert("@@import_meta".into(), Value::Obj(meta));
        }
        let id = self.modules.len();
        let info = Rc::new(info);
        self.modules.push(Module { url: url.to_owned(), env: env.clone(), body: Rc::new(body), info: info.clone(), deps: Vec::new(), state: ModState::Linking, ns: None, promise: None });
        if !inline {
            self.module_map.insert(url.to_owned(), id);
        }
        let urls = info.requests.iter().map(|r| self.resolve_specifier(url, r)).collect::<R<Vec<String>>>()?;
        // Fetch this level's missing dependencies in parallel.
        let mut missing: Vec<String> = urls.iter().filter(|u| !self.module_map.contains_key(*u) && !self.module_sources.contains_key(*u) && (u.starts_with("http:") || u.starts_with("https:"))).cloned().collect();
        missing.sort();
        missing.dedup();
        if missing.len() > 1 {
            for (u, b) in missing.iter().zip(crate::net::fetch_many(&missing)) {
                if let Some(b) = b {
                    self.module_sources.insert(u.clone(), crate::net::decode(&b));
                }
            }
        }
        let mut deps = Vec::with_capacity(urls.len());
        for u in &urls {
            deps.push(self.load_module(u, None)?);
        }
        self.modules[id].deps = deps.clone();
        for (name, local, req) in &info.imports {
            let dep = deps[*req];
            let imported = match name {
                ImportName::Named(n) => n.to_string(),
                ImportName::Namespace => "*".to_owned(),
            };
            if let ImportName::Named(n) = name {
                if self.resolve_export(dep, n, &mut Vec::new()).is_none() && self.modules[dep].state != ModState::Linking {
                    let m = &self.modules[dep].url;
                    return Err(err("SyntaxError", format!("The requested module '{m}' does not provide an export named '{n}'")));
                }
            }
            env.vars.borrow_mut().insert(local.clone(), Value::str(&format!("{IMPORT_MARK}{dep}:{imported}")));
        }
        self.modules[id].state = ModState::Linked;
        Ok(id)
    }

    /// Evaluate a linked module after its dependencies; returns the promise
    /// for its evaluation.
    pub fn evaluate_module(&mut self, id: usize) -> Value {
        if self.modules[id].state == ModState::Evaluated {
            return self.modules[id].promise.clone().unwrap_or(Value::Undef);
        }
        self.modules[id].state = ModState::Evaluated;
        self.script_running += 1;
        let r = self.evaluate_module_inner(id);
        self.script_running -= 1;
        r
    }

    fn evaluate_module_inner(&mut self, id: usize) -> Value {
        let mut waits = Vec::new();
        for d in self.modules[id].deps.clone() {
            let p = self.evaluate_module(d);
            if let Value::Obj(po) = &p {
                match &po.borrow().kind {
                    Kind::Promise(PromiseState::Fulfilled(_)) => {}
                    Kind::Promise(_) => waits.push(p.clone()),
                    _ => {}
                }
            }
        }
        let (body, env) = (self.modules[id].body.clone(), self.modules[id].env.clone());
        let p = self.start_job(Box::new(move |it: &mut Interp| {
            // Dependencies with top-level await finish first.
            for w in waits {
                builtins::await_value(it, w)?;
            }
            it.hoist(&body, &env)?;
            for s in body.iter() {
                it.exec(s, &env)?;
            }
            Ok(Value::Undef)
        }));
        if let Value::Obj(po) = &p {
            let log = self.native(log_module_error, "logModuleError");
            builtins::subscribe(self, po, Value::Undef, log);
        }
        self.modules[id].promise = Some(p.clone());
        p
    }

    /// Run a module script: load its graph and evaluate it.
    pub fn run_module(&mut self, url: &str, src: Option<String>) {
        if crate::js::trace_scripts() {
            eprintln!("[script] module {}", if src.is_some() { "(inline)" } else { url });
        }
        match self.load_module(url, src) {
            Ok(id) => {
                self.evaluate_module(id);
            }
            Err(e) => {
                let m = self.describe_error(&e);
                self.log(&m);
            }
        }
        self.run_microtasks();
    }

    fn resolve_export(&self, id: usize, name: &str, seen: &mut Vec<(usize, String)>) -> Option<(usize, Binding)> {
        if seen.iter().any(|(m, n)| *m == id && n == name) {
            return None;
        }
        seen.push((id, name.to_owned()));
        let m = &self.modules[id];
        for e in &m.info.exports {
            match e {
                ExportEntry::Local { local, exported } if &**exported == name => return Some((id, Binding::Local(local.clone()))),
                ExportEntry::Indirect { req, imported, exported } if &**exported == name => {
                    let dep = *m.deps.get(*req)?;
                    if &**imported == "*" {
                        return Some((dep, Binding::Namespace));
                    }
                    return self.resolve_export(dep, imported, seen);
                }
                _ => {}
            }
        }
        if name == "default" {
            return None;
        }
        for e in &m.info.exports {
            if let ExportEntry::Star { req } = e {
                if let Some(&dep) = m.deps.get(*req) {
                    if let Some(b) = self.resolve_export(dep, name, seen) {
                        return Some(b);
                    }
                }
            }
        }
        None
    }

    fn export_names(&self, id: usize, seen: &mut Vec<usize>, out: &mut Vec<Name>) {
        if seen.contains(&id) {
            return;
        }
        seen.push(id);
        let m = &self.modules[id];
        for e in &m.info.exports {
            match e {
                ExportEntry::Local { exported, .. } | ExportEntry::Indirect { exported, .. } => {
                    if !out.contains(exported) {
                        out.push(exported.clone());
                    }
                }
                ExportEntry::Star { req } => {
                    if let Some(&dep) = m.deps.get(*req) {
                        let mut inner = Vec::new();
                        self.export_names(dep, seen, &mut inner);
                        for n in inner {
                            if &*n != "default" && !out.contains(&n) {
                                out.push(n);
                            }
                        }
                    }
                }
            }
        }
    }

    /// The current value of export `name` of module `id`.
    fn export_value(&mut self, id: usize, name: &str) -> R<Value> {
        match self.resolve_export(id, name, &mut Vec::new()) {
            Some((m, Binding::Local(local))) => {
                let env = self.modules[m].env.clone();
                self.lookup(&local, &env)
            }
            Some((m, Binding::Namespace)) => Ok(Value::Obj(self.module_namespace(m))),
            None => Ok(Value::Undef),
        }
    }

    /// Resolve an import marker (see `IMPORT_MARK`).
    pub(crate) fn import_binding(&mut self, mark: &str) -> R<Value> {
        let rest = &mark[IMPORT_MARK.len()..];
        let (id, name) = rest.split_once(':').unwrap_or((rest, ""));
        let id: usize = id.parse().unwrap_or(usize::MAX);
        if id >= self.modules.len() {
            return Ok(Value::Undef);
        }
        if name == "*" {
            return Ok(Value::Obj(self.module_namespace(id)));
        }
        self.export_value(id, name)
    }

    /// The module namespace object (`import * as ns`, `import()`).
    pub fn module_namespace(&mut self, id: usize) -> ObjRef {
        if let Some(ns) = &self.modules[id].ns {
            return ns.clone();
        }
        let ns = self.new_obj(None, Kind::Plain);
        let mut names = Vec::new();
        self.export_names(id, &mut Vec::new(), &mut names);
        names.sort();
        let getter = self.native(ns_get, "get");
        for n in names {
            let g = Value::Obj(self.new_obj(self.p.function.clone(), Kind::Func(Func::Bound(getter.clone(), Value::Undef, vec![Value::Num(id as f64), Value::Str(n.clone())]))));
            ns.borrow_mut().props.insert(n, Prop { v: Value::Undef, get: Some(g), set: None, enumerable: true });
        }
        Interp::set_hidden(&ns, "@@toStringTag", Value::str("Module"));
        self.modules[id].ns = Some(ns.clone());
        ns
    }

    /// `import(specifier)`: a promise for the module's namespace.
    pub(crate) fn dynamic_import(&mut self, spec: Value, env: &Env) -> Value {
        let base = match self.lookup("@@module_url", env) {
            Ok(Value::Str(s)) => s.to_string(),
            _ => self.url.clone(),
        };
        let url = (|| -> R<String> {
            let s = self.to_str(&spec)?;
            self.resolve_specifier(&base, &s)
        })();
        match url {
            // The module loads and runs later, never inside the caller (a
            // framework mid-render must not see a module's top-level code run).
            Ok(url) => self.start_job(Box::new(move |it: &mut Interp| {
                builtins::await_value(it, Value::Undef)?;
                let id = it.load_module(&url, None)?;
                let p = it.evaluate_module(id);
                builtins::await_value(it, p)?;
                Ok(Value::Obj(it.module_namespace(id)))
            })),
            Err(e) => builtins::promise_rejected(self, e),
        }
    }

    /// `import.meta` in the current module (an empty object elsewhere).
    pub(crate) fn import_meta(&mut self, env: &Env) -> Value {
        match self.lookup("@@import_meta", env) {
            Ok(v @ Value::Obj(_)) => v,
            _ => Value::Obj(self.plain()),
        }
    }
}

fn ns_get(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let id = match arg(a, 0) {
        Value::Num(n) => n as usize,
        _ => return Ok(Value::Undef),
    };
    let name = it.to_str(&arg(a, 1))?;
    it.export_value(id, &name)
}

fn meta_resolve(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let base = it.to_str(&t)?;
    let spec = it.to_str(&arg(a, 0))?;
    Ok(Value::str(&it.resolve_specifier(&base, &spec)?))
}

fn log_module_error(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let m = it.describe_error(&arg(a, 0));
    it.log(&format!("Uncaught (in module) {m}"));
    Ok(Value::Undef)
}
