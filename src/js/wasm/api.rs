//! Natives behind the `WebAssembly` namespace (the classes themselves are
//! in the prelude). Handles are host objects wrapping modules, memories,
//! tables and globals; the prelude wraps them in the public classes.

use super::exec::{self, ExternVal, Global, InstError, Memory, Table};
use super::*;
use crate::js::builtins::arg;
use crate::js::{binary, func_props, type_error, Func, Interp, Kind, NativeFn, ObjRef, Value, R};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub enum Host {
    Module(Rc<Module>),
    Memory(Rc<RefCell<Memory>>),
    Table(Rc<RefCell<Table>>),
    Global(Rc<Global>),
}

thread_local! {
    /// JS function object of each function store id.
    static FUNC_OBJS: RefCell<HashMap<u64, Value>> = RefCell::new(HashMap::new());
    /// Handle object of each memory / table / global (by Rc address).
    static HANDLES: RefCell<HashMap<usize, ObjRef>> = RefCell::new(HashMap::new());
    static CALL_NATIVE: RefCell<Option<Value>> = const { RefCell::new(None) };
}

fn host(v: &Value) -> Option<Rc<Host>> {
    let o = v.obj()?;
    let b = o.borrow();
    match &b.kind {
        Kind::Host(h) => h.clone().downcast::<Host>().ok(),
        _ => None,
    }
}

fn handle(it: &Interp, h: Host) -> Value {
    let key = match &h {
        Host::Module(m) => Rc::as_ptr(m) as *const () as usize,
        Host::Memory(m) => Rc::as_ptr(m) as *const () as usize,
        Host::Table(t) => Rc::as_ptr(t) as *const () as usize,
        Host::Global(g) => Rc::as_ptr(g) as *const () as usize,
    };
    if let Some(o) = HANDLES.with(|m| m.borrow().get(&key).cloned()) {
        return Value::Obj(o);
    }
    let rc: Rc<dyn Any> = Rc::new(h);
    let o = it.new_obj(it.p.object.clone(), Kind::Host(rc));
    HANDLES.with(|m| m.borrow_mut().insert(key, o.clone()));
    Value::Obj(o)
}

/// The JS function for a function store id (one object per id).
pub fn func_object(it: &Interp, id: u64) -> Value {
    if let Some(v) = FUNC_OBJS.with(|m| m.borrow().get(&id).cloned()) {
        return v;
    }
    let target = CALL_NATIVE.with(|c| c.borrow().clone()).unwrap_or_else(|| it.native(wasm_call, "wasm_call"));
    let o = it.new_obj(it.p.function.clone(), Kind::Func(Func::Bound(target, Value::Undef, vec![Value::Num(id as f64)])));
    let (name, len) = match exec::func(id) {
        Some(rec) => {
            let n = match &rec.entry {
                exec::FuncEntry::Wasm { idx, .. } => idx.to_string(),
                exec::FuncEntry::Host(_) => "0".to_string(),
            };
            (n, exec::canon_type(rec.ty).params.len())
        }
        None => ("0".into(), 0),
    };
    func_props(&o, Rc::from(name.as_str()), len);
    Interp::set_hidden(&o, "@@wasmfn", Value::Num(id as f64));
    let v = Value::Obj(o);
    FUNC_OBJS.with(|m| m.borrow_mut().insert(id, v.clone()));
    v
}

/// The function store id behind an exported wasm function.
pub fn func_id_of(v: &Value) -> Option<u64> {
    let o = v.obj()?;
    let b = o.borrow();
    match b.props.get("@@wasmfn").map(|p| p.v.clone()) {
        Some(Value::Num(n)) => Some(n as u64),
        _ => None,
    }
}

/// Calling an exported function: (id, ...args).
fn wasm_call(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let id = match arg(a, 0) {
        Value::Num(n) => n as u64,
        _ => return Err(type_error("not a wasm function")),
    };
    let Some(rec) = exec::func(id) else { return Err(type_error("not a wasm function")) };
    let t = exec::canon_type(rec.ty);
    let mut args = Vec::with_capacity(t.params.len());
    for (i, &ty) in t.params.iter().enumerate() {
        args.push(exec::from_js(it, ty, &arg(a, i + 1))?);
    }
    let res = exec::call_id(it, id, &args)?;
    Ok(match res.len() {
        0 => Value::Undef,
        1 => exec::to_js(it, t.results[0], res[0]),
        _ => {
            let items = t.results.iter().zip(&res).map(|(&ty, &v)| exec::to_js(it, ty, v)).collect();
            it.array(items)
        }
    })
}

fn bytes_arg(v: &Value) -> R<Vec<u8>> {
    binary::bytes_of(v).ok_or_else(|| type_error("WebAssembly: argument must be a buffer source"))
}

fn compile(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let bytes = bytes_arg(&arg(a, 0))?;
    Ok(match parse(&bytes) {
        Ok(m) => handle(it, Host::Module(Rc::new(m))),
        Err(e) => Value::str(&format!("WebAssembly.Module(): {e}")),
    })
}

fn validate(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let bytes = bytes_arg(&arg(a, 0))?;
    Ok(Value::Bool(parse(&bytes).is_ok()))
}

fn module_of(v: &Value) -> R<Rc<Module>> {
    match host(v).as_deref() {
        Some(Host::Module(m)) => Ok(m.clone()),
        _ => Err(type_error("WebAssembly: argument must be a WebAssembly.Module")),
    }
}

fn kind_name(d: &ImportDesc) -> &'static str {
    match d {
        ImportDesc::Func(_) => "function",
        ImportDesc::Table(_) => "table",
        ImportDesc::Memory(_) => "memory",
        ImportDesc::Global(..) => "global",
        ImportDesc::Tag => "tag",
    }
}

fn module_imports(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let m = module_of(&arg(a, 0))?;
    let items = m
        .imports
        .iter()
        .map(|i| {
            let ty = match i.desc {
                ImportDesc::Global(t, _) => Value::str(t.name()),
                _ => Value::Undef,
            };
            it.array(vec![Value::str(&i.module), Value::str(&i.name), Value::str(kind_name(&i.desc)), ty])
        })
        .collect();
    Ok(it.array(items))
}

fn module_exports(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let m = module_of(&arg(a, 0))?;
    let items = m.exports.iter().map(|e| it.array(vec![Value::str(&e.name), Value::str(e.kind.name())])).collect();
    Ok(it.array(items))
}

fn custom_sections(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let m = module_of(&arg(a, 0))?;
    let name = it.to_str(&arg(a, 1))?;
    let items = m.customs.iter().filter(|(n, _)| **n == *name).map(|(_, d)| Value::Obj(binary::new_buffer(it, d.clone()))).collect();
    Ok(it.array(items))
}

fn instantiate(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let m = module_of(&arg(a, 0))?;
    let vals = it.iterate(&arg(a, 1))?;
    let mut imports = Vec::with_capacity(m.imports.len());
    for (i, imp) in m.imports.iter().enumerate() {
        let v = vals.get(i).cloned().unwrap_or_default();
        let h = host(&v);
        let ev = match (&imp.desc, h.as_deref()) {
            (ImportDesc::Func(t), _) if Interp::is_callable(&v) => match func_id_of(&v) {
                Some(id) => ExternVal::Func(id),
                None => {
                    let ty = exec::canon(&m.types[*t as usize]);
                    ExternVal::Func(exec::add_func(exec::FuncRec { entry: exec::FuncEntry::Host(v.clone()), ty }))
                }
            },
            (ImportDesc::Table(_), Some(Host::Table(t))) => ExternVal::Table(t.clone()),
            (ImportDesc::Memory(_), Some(Host::Memory(mm))) => ExternVal::Memory(mm.clone()),
            (ImportDesc::Global(..), Some(Host::Global(g))) => ExternVal::Global(g.clone()),
            (ImportDesc::Global(ty, false), None) if !matches!(v, Value::Obj(_)) || matches!(ty, ValType::ExternRef | ValType::FuncRef) => {
                let ok = match ty {
                    ValType::I64 => matches!(v, Value::BigInt(_)),
                    ValType::I32 | ValType::F32 | ValType::F64 => matches!(v, Value::Num(_)),
                    _ => true,
                };
                if !ok {
                    return Ok(Value::str(&format!(
                        "LinkError:WebAssembly.Instance(): Import #{i} \"{}\" \"{}\": global import must be a number, valid Wasm reference, or WebAssembly.Global object",
                        imp.module, imp.name
                    )));
                }
                let bits = exec::from_js(it, *ty, &v)?;
                ExternVal::Global(Rc::new(Global { v: Cell::new(bits), ty: *ty, mutable: false }))
            }
            (ImportDesc::Tag, _) => ExternVal::Tag,
            _ => ExternVal::Tag,
        };
        // (A mismatched kind reaches `instantiate` as Tag and fails there.)
        let ev = match (&imp.desc, ev) {
            (ImportDesc::Tag, e) => e,
            (_, ExternVal::Tag) => {
                let msg = match imp.desc {
                    ImportDesc::Func(_) => "function import requires a callable",
                    ImportDesc::Table(_) => "table import requires a WebAssembly.Table",
                    ImportDesc::Memory(_) => "memory import must be a WebAssembly.Memory object",
                    _ => "global import must be a number, valid Wasm reference, or WebAssembly.Global object",
                };
                return Ok(Value::str(&format!("LinkError:WebAssembly.Instance(): Import #{i} \"{}\" \"{}\": {msg}", imp.module, imp.name)));
            }
            (_, e) => e,
        };
        imports.push(ev);
    }
    let inst = match exec::instantiate(it, m.clone(), imports) {
        Ok(i) => i,
        Err(InstError::Link(msg)) => return Ok(Value::str(&format!("LinkError:WebAssembly.Instance(): {msg}"))),
        Err(InstError::Throw(e)) => return Err(e),
    };
    let mut out = Vec::with_capacity(m.exports.len());
    for e in &m.exports {
        let i = e.index as usize;
        let v = match e.kind {
            ExternKind::Func => func_object(it, inst.func_ids[i]),
            ExternKind::Memory => match &inst.memory {
                Some(mm) => handle(it, Host::Memory(mm.clone())),
                None => Value::Undef,
            },
            ExternKind::Table => handle(it, Host::Table(inst.tables[i].clone())),
            ExternKind::Global => handle(it, Host::Global(inst.globals[i].clone())),
            ExternKind::Tag => Value::Null,
        };
        out.push(it.array(vec![Value::str(&e.name), Value::str(e.kind.name()), v]));
    }
    Ok(it.array(out))
}

fn opt_u32(it: &mut Interp, v: &Value) -> R<Option<u32>> {
    Ok(match v {
        Value::Undef => None,
        v => Some(it.to_num(v)?.clamp(0.0, u32::MAX as f64) as u32),
    })
}

fn mem_new(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let min = opt_u32(it, &arg(a, 0))?.unwrap_or(0);
    let max = opt_u32(it, &arg(a, 1))?;
    if min > exec::MAX_PAGES || max.is_some_and(|m| m < min) {
        return Err(crate::js::range_error("WebAssembly.Memory(): could not allocate memory"));
    }
    let m = exec::new_memory(it, Limits { min, max });
    Ok(handle(it, Host::Memory(m)))
}

fn mem_of(v: &Value) -> R<Rc<RefCell<Memory>>> {
    match host(v).as_deref() {
        Some(Host::Memory(m)) => Ok(m.clone()),
        _ => Err(type_error("Receiver is not a WebAssembly.Memory")),
    }
}

fn mem_buffer(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Obj(mem_of(&arg(a, 0))?.borrow().obj.clone()))
}

fn mem_grow(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let m = mem_of(&arg(a, 0))?;
    let delta = it.to_num(&arg(a, 1))?.max(0.0) as u32;
    match exec::grow_memory(it, &m, delta) {
        Some(old) => Ok(Value::Num(old as f64)),
        None => Err(crate::js::range_error("WebAssembly.Memory.grow(): Maximum memory size exceeded")),
    }
}

fn table_of(v: &Value) -> R<Rc<RefCell<Table>>> {
    match host(v).as_deref() {
        Some(Host::Table(t)) => Ok(t.clone()),
        _ => Err(type_error("Receiver is not a WebAssembly.Table")),
    }
}

fn elem_type(it: &mut Interp, v: &Value) -> R<ValType> {
    Ok(match &*it.to_str(v)? {
        "anyfunc" | "funcref" => ValType::FuncRef,
        "externref" => ValType::ExternRef,
        _ => return Err(type_error("WebAssembly.Table(): Descriptor property 'element' must be a WebAssembly reference type")),
    })
}

fn table_new(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let ty = elem_type(it, &arg(a, 0))?;
    let min = opt_u32(it, &arg(a, 1))?.unwrap_or(0);
    let max = opt_u32(it, &arg(a, 2))?;
    if min > 10_000_000 || max.is_some_and(|m| m < min) {
        return Err(crate::js::range_error("WebAssembly.Table(): invalid size"));
    }
    let init = match arg(a, 3) {
        Value::Undef if ty == ValType::FuncRef => 0,
        v => exec::from_js(it, ty, &v)?,
    };
    let t = Table { elems: vec![init; min as usize], max, ty };
    Ok(handle(it, Host::Table(Rc::new(RefCell::new(t)))))
}

fn table_len(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Num(table_of(&arg(a, 0))?.borrow().elems.len() as f64))
}

fn table_get(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let t = table_of(&arg(a, 0))?;
    let i = it.to_num(&arg(a, 1))?;
    let (v, ty) = {
        let t = t.borrow();
        match t.elems.get(i as usize).filter(|_| i >= 0.0) {
            Some(&v) => (v, t.ty),
            None => return Err(crate::js::range_error("WebAssembly.Table.get(): invalid address")),
        }
    };
    Ok(exec::to_js(it, ty, v))
}

fn table_set(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let t = table_of(&arg(a, 0))?;
    let i = it.to_num(&arg(a, 1))?;
    let ty = t.borrow().ty;
    let v = match arg(a, 2) {
        Value::Undef if ty == ValType::FuncRef => 0,
        v => exec::from_js(it, ty, &v)?,
    };
    let mut t = t.borrow_mut();
    match t.elems.get_mut(i as usize).filter(|_| i >= 0.0) {
        Some(e) => *e = v,
        None => return Err(crate::js::range_error("WebAssembly.Table.set(): invalid address")),
    }
    Ok(Value::Undef)
}

fn table_grow(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let t = table_of(&arg(a, 0))?;
    let n = it.to_num(&arg(a, 1))?.max(0.0) as u32;
    let ty = t.borrow().ty;
    let v = match arg(a, 2) {
        Value::Undef if ty == ValType::FuncRef => 0,
        v => exec::from_js(it, ty, &v)?,
    };
    let mut t = t.borrow_mut();
    let old = t.elems.len() as u32;
    let max = t.max.unwrap_or(10_000_000).min(10_000_000);
    match old.checked_add(n).filter(|&x| x <= max) {
        Some(x) => {
            t.elems.resize(x as usize, v);
            Ok(Value::Num(old as f64))
        }
        None => Err(crate::js::range_error("WebAssembly.Table.grow(): failed to grow table")),
    }
}

fn val_type(it: &mut Interp, v: &Value) -> R<ValType> {
    Ok(match &*it.to_str(v)? {
        "i32" => ValType::I32,
        "i64" => ValType::I64,
        "f32" => ValType::F32,
        "f64" => ValType::F64,
        "anyfunc" | "funcref" => ValType::FuncRef,
        "externref" => ValType::ExternRef,
        _ => return Err(type_error("WebAssembly.Global(): Descriptor property 'value' must be a WebAssembly type")),
    })
}

fn global_of(v: &Value) -> R<Rc<Global>> {
    match host(v).as_deref() {
        Some(Host::Global(g)) => Ok(g.clone()),
        _ => Err(type_error("Receiver is not a WebAssembly.Global")),
    }
}

fn global_new(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let ty = val_type(it, &arg(a, 0))?;
    let mutable = arg(a, 1).truthy();
    let v = match arg(a, 2) {
        Value::Undef => match ty {
            ValType::ExternRef => exec::extern_id(&Value::Undef),
            _ => 0,
        },
        v => exec::from_js(it, ty, &v)?,
    };
    Ok(handle(it, Host::Global(Rc::new(Global { v: Cell::new(v), ty, mutable }))))
}

fn global_get(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let g = global_of(&arg(a, 0))?;
    Ok(exec::to_js(it, g.ty, g.v.get()))
}

fn global_set(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let g = global_of(&arg(a, 0))?;
    if !g.mutable {
        return Err(type_error("WebAssembly.Global: Can't set the value of an immutable global."));
    }
    let v = exec::from_js(it, g.ty, &arg(a, 1))?;
    g.v.set(v);
    Ok(Value::Undef)
}

fn global_info(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let g = global_of(&arg(a, 0))?;
    Ok(it.array(vec![Value::str(g.ty.name()), Value::Bool(g.mutable)]))
}

pub fn install(it: &mut Interp) {
    let call = it.native(wasm_call, "wasm_call");
    CALL_NATIVE.with(|c| *c.borrow_mut() = Some(call));
    let o = it.plain();
    let fns: [(&'static str, NativeFn); 20] = [
        ("compile", compile),
        ("validate", validate),
        ("imports", module_imports),
        ("exports", module_exports),
        ("customSections", custom_sections),
        ("instantiate", instantiate),
        ("memNew", mem_new),
        ("memBuffer", mem_buffer),
        ("memGrow", mem_grow),
        ("tableNew", table_new),
        ("tableLen", table_len),
        ("tableGet", table_get),
        ("tableSet", table_set),
        ("tableGrow", table_grow),
        ("globalNew", global_new),
        ("globalGet", global_get),
        ("globalSet", global_set),
        ("globalInfo", global_info),
        ("isModule", is_module),
        ("kindOf", kind_of),
    ];
    for (n, f) in fns {
        it.def_method(&o, n, f);
    }
    let g = it.global.clone();
    Interp::set_hidden(&g, "@@wasm", Value::Obj(o));
}

fn is_module(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(matches!(host(&arg(a, 0)).as_deref(), Some(Host::Module(_)))))
}

/// "module" / "memory" / "table" / "global" for a handle.
fn kind_of(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(match host(&arg(a, 0)).as_deref() {
        Some(Host::Module(_)) => Value::str("module"),
        Some(Host::Memory(_)) => Value::str("memory"),
        Some(Host::Table(_)) => Value::str("table"),
        Some(Host::Global(_)) => Value::str("global"),
        None => Value::Undef,
    })
}
