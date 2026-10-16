//! `Proxy` and `Reflect`.
//!
//! A proxy is an object of `Kind::Proxy`; the interpreter's property access,
//! `in`, `delete`, key enumeration, calls and `new` route through the traps
//! here, falling back to the target when the handler has no trap.

use super::builtins::{arg, make_error_value};
use super::*;

pub struct ProxyData {
    pub target: Value,
    /// Null once revoked.
    pub handler: Value,
}

/// (target, handler) of a proxy object.
pub fn parts(o: &ObjRef) -> Option<(Value, Value)> {
    match &o.borrow().kind {
        Kind::Proxy(p) => Some((p.target.clone(), p.handler.clone())),
        _ => None,
    }
}

fn trap(it: &mut Interp, handler: &Value, name: &str) -> R<Option<Value>> {
    if handler.is_nullish() {
        return Err(type_error(format!("Cannot perform '{name}' on a proxy that has been revoked")));
    }
    let t = it.get(handler, name)?;
    Ok(if Interp::is_callable(&t) { Some(t) } else { None })
}

pub fn get(it: &mut Interp, o: &ObjRef, key: &str, receiver: &Value) -> R<Value> {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "get")? {
        Some(f) => it.call(&f, handler, &[target, Value::str(key), receiver.clone()]),
        None => it.get(&target, key),
    }
}

pub fn set(it: &mut Interp, o: &ObjRef, key: &str, v: Value, receiver: &Value) -> R<()> {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "set")? {
        Some(f) => it.call(&f, handler, &[target, Value::str(key), v, receiver.clone()]).map(|_| ()),
        None => it.put(&target, key, v),
    }
}

pub fn has(it: &mut Interp, o: &ObjRef, key: &str) -> bool {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "has") {
        Ok(Some(f)) => it.call(&f, handler, &[target, Value::str(key)]).is_ok_and(|v| v.truthy()),
        _ => it.has_prop(&target, key),
    }
}

pub fn delete(it: &mut Interp, o: &ObjRef, key: &str) -> R<bool> {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "deleteProperty")? {
        Some(f) => Ok(it.call(&f, handler, &[target, Value::str(key)])?.truthy()),
        None => Ok(it.delete(&target, key)),
    }
}

/// Keys for enumeration (`Object.keys`, `for in`, spread).
pub fn own_keys(it: &mut Interp, o: &ObjRef) -> Vec<Rc<str>> {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "ownKeys") {
        Ok(Some(f)) => match it.call(&f, handler, &[target]) {
            Ok(list) => it.iterate(&list).unwrap_or_default().into_iter().filter_map(|k| if let Value::Str(s) = k { Some(s) } else { None }).filter(|k| !k.starts_with("@@")).collect(),
            Err(_) => Vec::new(),
        },
        _ => it.own_keys(&target),
    }
}

pub fn apply(it: &mut Interp, o: &ObjRef, this: Value, args: &[Value]) -> R<Value> {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "apply")? {
        Some(f) => {
            let a = it.array(args.to_vec());
            it.call(&f, handler, &[target, this, a])
        }
        None => it.call(&target, this, args),
    }
}

pub fn construct(it: &mut Interp, o: &ObjRef, args: &[Value]) -> R<Value> {
    let (target, handler) = parts(o).unwrap();
    match trap(it, &handler, "construct")? {
        Some(f) => {
            let a = it.array(args.to_vec());
            let r = it.call(&f, handler, &[target, a, Value::Obj(o.clone())])?;
            if !matches!(r, Value::Obj(_)) {
                return Err(type_error("proxy [[Construct]] must return an object"));
            }
            Ok(r)
        }
        None => it.construct(&target, args),
    }
}

fn new_proxy(it: &mut Interp, target: Value, handler: Value) -> R<ObjRef> {
    if !matches!(target, Value::Obj(_)) || !matches!(handler, Value::Obj(_)) {
        return Err(type_error("Cannot create proxy with a non-object as target or handler"));
    }
    // The prototype is the target's (approximates getPrototypeOf / instanceof).
    let proto = target.obj().and_then(|t| t.borrow().proto.clone());
    Ok(it.new_obj(proto, Kind::Proxy(Box::new(ProxyData { target, handler }))))
}

fn proxy_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if !it.constructing {
        return Err(type_error("Constructor Proxy requires 'new'"));
    }
    Ok(Value::Obj(new_proxy(it, arg(a, 0), arg(a, 1))?))
}

fn revoke(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if let Value::Obj(o) = &t {
        if let Some(Value::Obj(p)) = o.borrow().props.get("@@proxy").map(|p| p.v.clone()) {
            if let Kind::Proxy(d) = &mut p.borrow_mut().kind {
                d.handler = Value::Null;
            }
        }
    }
    Ok(Value::Undef)
}

fn proxy_revocable(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let p = new_proxy(it, arg(a, 0), arg(a, 1))?;
    let holder = it.plain();
    Interp::set_hidden(&holder, "@@proxy", Value::Obj(p.clone()));
    let r = it.plain();
    Interp::set_prop(&r, "proxy", Value::Obj(p));
    let f = builtins::bound_native(it, revoke, Value::Obj(holder));
    Interp::set_prop(&r, "revoke", f);
    Ok(Value::Obj(r))
}

/// Call `Object.<name>(args)`.
fn object_fn(it: &mut Interp, name: &str, args: &[Value]) -> R<Value> {
    let o = it.get(&Value::Obj(it.global.clone()), "Object")?;
    let f = it.get(&o, name)?;
    it.call(&f, o, args)
}

fn need_obj(v: &Value, what: &str) -> R<()> {
    if matches!(v, Value::Obj(_)) {
        Ok(())
    } else {
        Err(type_error(format!("Reflect.{what} called on non-object")))
    }
}

fn r_get(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "get")?;
    it.get_v(&arg(a, 0), &arg(a, 1))
}
fn r_set(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "set")?;
    it.put_v(&arg(a, 0), &arg(a, 1), arg(a, 2))?;
    Ok(Value::Bool(true))
}
fn r_has(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "has")?;
    let k = it.key(&arg(a, 1))?;
    Ok(Value::Bool(it.has_prop(&arg(a, 0), &k)))
}
fn r_delete(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "deleteProperty")?;
    let k = it.key(&arg(a, 1))?;
    let t = arg(a, 0);
    if let Value::Obj(o) = &t {
        if parts(o).is_some() {
            return Ok(Value::Bool(delete(it, o, &k)?));
        }
    }
    Ok(Value::Bool(it.delete(&t, &k)))
}
fn r_own_keys(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let t = arg(a, 0);
    need_obj(&t, "ownKeys")?;
    if let Value::Obj(o) = &t {
        if let Some((target, handler)) = parts(o) {
            if let Some(f) = trap(it, &handler, "ownKeys")? {
                return it.call(&f, handler, &[target]);
            }
            return r_own_keys(it, Value::Undef, &[target]);
        }
    }
    let names = object_fn(it, "getOwnPropertyNames", &[t.clone()])?;
    let syms = object_fn(it, "getOwnPropertySymbols", &[t])?;
    let mut all = it.iterate(&names)?;
    all.extend(it.iterate(&syms)?);
    Ok(it.array(all))
}
fn r_apply(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let args = if arg(a, 2).is_nullish() { Vec::new() } else { it.iterate(&arg(a, 2))? };
    it.call(&arg(a, 0), arg(a, 1), &args)
}
fn r_construct(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let args = if arg(a, 1).is_nullish() { Vec::new() } else { it.iterate(&arg(a, 1))? };
    let r = it.construct(&arg(a, 0), &args)?;
    // A different new.target only changes the prototype.
    if let (Value::Obj(o), Value::Obj(nt)) = (&r, arg(a, 2)) {
        let same = matches!(arg(a, 0), Value::Obj(f) if Rc::ptr_eq(&f, &nt));
        if !same {
            if let Value::Obj(p) = it.get(&Value::Obj(nt), "prototype")? {
                o.borrow_mut().proto = Some(p);
            }
        }
    }
    Ok(r)
}
fn r_define(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "defineProperty")?;
    Ok(Value::Bool(object_fn(it, "defineProperty", a).is_ok()))
}
fn r_get_desc(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "getOwnPropertyDescriptor")?;
    object_fn(it, "getOwnPropertyDescriptor", a)
}
fn r_get_proto(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "getPrototypeOf")?;
    object_fn(it, "getPrototypeOf", a)
}
fn r_set_proto(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    need_obj(&arg(a, 0), "setPrototypeOf")?;
    Ok(Value::Bool(object_fn(it, "setPrototypeOf", a).is_ok()))
}
fn r_true(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if !matches!(arg(a, 0), Value::Obj(_)) {
        return Err(make_error_value("TypeError", "Reflect method called on non-object"));
    }
    Ok(Value::Bool(true))
}

pub fn install(it: &mut Interp) {
    let ctor = it.native(proxy_ctor, "Proxy");
    if let Value::Obj(c) = &ctor {
        it.def_method(c, "revocable", proxy_revocable);
    }
    Interp::set_hidden(&it.global.clone(), "Proxy", ctor);
    let r = it.plain();
    for (n, f) in [
        ("get", r_get as NativeFn),
        ("set", r_set),
        ("has", r_has),
        ("deleteProperty", r_delete),
        ("ownKeys", r_own_keys),
        ("apply", r_apply),
        ("construct", r_construct),
        ("defineProperty", r_define),
        ("getOwnPropertyDescriptor", r_get_desc),
        ("getPrototypeOf", r_get_proto),
        ("setPrototypeOf", r_set_proto),
        ("isExtensible", r_true),
        ("preventExtensions", r_true),
    ] {
        it.def_method(&r, n, f);
    }
    Interp::set_hidden(&r, "@@toStringTag", Value::str("Reflect"));
    Interp::set_hidden(&it.global.clone(), "Reflect", Value::Obj(r));
}
