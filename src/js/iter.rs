//! Built-in iterators (`%ArrayIterator%` and friends) and `%IteratorPrototype%`.
//!
//! `Array`, `Map`, `Set` and `String` iteration methods return real iterator
//! objects (with `next()` and `[Symbol.iterator]`), like in browsers, rather
//! than arrays. Arrays are iterated live; other collections from a snapshot.

use super::builtins::arg;
use super::*;

#[derive(Clone, Copy, PartialEq)]
pub enum IterKind {
    Values,
    Keys,
    Entries,
}

pub struct IterState {
    /// An array object to walk (live for real arrays, a snapshot otherwise).
    pub src: Value,
    pub i: usize,
    pub kind: IterKind,
    pub done: bool,
}

/// A built-in iterator over `src` (an array object).
pub fn make(it: &Interp, src: Value, kind: IterKind) -> Value {
    let st = IterState { src, i: 0, kind, done: false };
    Value::Obj(it.new_obj(it.p.array_iterator.clone(), Kind::Iter(Box::new(st))))
}

/// The values left in a built-in iterator (consumes it).
pub fn drain(it: &mut Interp, o: &ObjRef) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(v) = step(it, o) {
        out.push(v);
    }
    out
}

fn step(it: &mut Interp, o: &ObjRef) -> Option<Value> {
    let (src, i, kind) = match &mut o.borrow_mut().kind {
        Kind::Iter(st) if !st.done => (st.src.clone(), st.i, st.kind),
        _ => return None,
    };
    let elem = match &src {
        Value::Obj(a) => match &a.borrow().kind {
            Kind::Array(v) => v.get(i).cloned(),
            _ => None,
        },
        _ => None,
    };
    let Some(elem) = elem else {
        if let Kind::Iter(st) = &mut o.borrow_mut().kind {
            st.done = true;
        }
        return None;
    };
    if let Kind::Iter(st) = &mut o.borrow_mut().kind {
        st.i += 1;
    }
    Some(match kind {
        IterKind::Values => elem,
        IterKind::Keys => Value::Num(i as f64),
        IterKind::Entries => it.array(vec![Value::Num(i as f64), elem]),
    })
}

fn iter_next(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let Value::Obj(o) = &t else { return Err(type_error("next called on a non-iterator")) };
    let v = step(it, o);
    let done = v.is_none();
    Ok(builtins::iter_result(it, v.unwrap_or_default(), done))
}

fn identity(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(t)
}

/// Collect an iterator (built-in or user-defined) into a Vec.
fn collect(it: &mut Interp, t: &Value) -> R<Vec<Value>> {
    if let Value::Obj(o) = t {
        if matches!(o.borrow().kind, Kind::Iter(_)) {
            return Ok(drain(it, o));
        }
    }
    let next = it.get(t, "next")?;
    let mut out = Vec::new();
    loop {
        let r = it.call(&next, t.clone(), &[])?;
        if it.get(&r, "done")?.truthy() {
            return Ok(out);
        }
        out.push(it.get(&r, "value")?);
    }
}

fn to_array(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = collect(it, &t)?;
    Ok(it.array(v))
}

/// Iterator helpers (`map`, `filter`, ...) are applied eagerly and return an
/// iterator over the result.
fn helper(it: &mut Interp, t: Value, a: &[Value], name: &str) -> R<Value> {
    let f = arg(a, 0);
    if matches!(name, "take" | "drop") {
        let n = it.to_num(&f)?.max(0.0) as usize;
        let next = it.get(&t, "next")?;
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            if name == "take" && out.len() >= n {
                let ret = it.get(&t, "return")?;
                if Interp::is_callable(&ret) {
                    it.call(&ret, t.clone(), &[])?;
                }
                break;
            }
            let r = it.call(&next, t.clone(), &[])?;
            if it.get(&r, "done")?.truthy() {
                break;
            }
            let v = it.get(&r, "value")?;
            if name == "take" || i >= n {
                out.push(v);
            }
            i += 1;
        }
        let arr = it.array(out);
        return Ok(make(it, arr, IterKind::Values));
    }
    if !Interp::is_callable(&f) {
        return Err(type_error(format!("{f:?} is not a function")));
    }
    let items = collect(it, &t)?;
    let mut out = Vec::new();
    let mut acc = a.get(1).cloned();
    for (i, v) in items.into_iter().enumerate() {
        let idx = Value::Num(i as f64);
        match name {
            "map" => out.push(it.call(&f, Value::Undef, &[v, idx])?),
            "filter" => {
                if it.call(&f, Value::Undef, &[v.clone(), idx])?.truthy() {
                    out.push(v);
                }
            }
            "flatMap" => {
                let inner = it.call(&f, Value::Undef, &[v, idx])?;
                out.extend(it.iterate(&inner)?);
            }
            "forEach" => {
                it.call(&f, Value::Undef, &[v, idx])?;
            }
            "some" => {
                if it.call(&f, Value::Undef, &[v, idx])?.truthy() {
                    return Ok(Value::Bool(true));
                }
            }
            "every" => {
                if !it.call(&f, Value::Undef, &[v, idx])?.truthy() {
                    return Ok(Value::Bool(false));
                }
            }
            "find" => {
                if it.call(&f, Value::Undef, &[v.clone(), idx])?.truthy() {
                    return Ok(v);
                }
            }
            "reduce" => {
                acc = Some(match acc {
                    None => v,
                    Some(a) => it.call(&f, Value::Undef, &[a, v, idx])?,
                });
            }
            _ => {}
        }
    }
    Ok(match name {
        "map" | "filter" | "flatMap" => {
            let arr = it.array(out);
            make(it, arr, IterKind::Values)
        }
        "some" => Value::Bool(false),
        "every" => Value::Bool(true),
        "reduce" => match acc {
            Some(v) => v,
            None => return Err(type_error("Reduce of empty iterator with no initial value")),
        },
        _ => Value::Undef,
    })
}

macro_rules! helpers {
    ($($f:ident = $n:literal),*) => {
        $(fn $f(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> { helper(it, t, a, $n) })*
        const HELPERS: &[(&str, NativeFn)] = &[$(($n, $f as NativeFn)),*];
    };
}
helpers!(h_map = "map", h_filter = "filter", h_flat_map = "flatMap", h_for_each = "forEach", h_some = "some", h_every = "every", h_find = "find", h_reduce = "reduce", h_take = "take", h_drop = "drop");

fn iterator_ctor(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Err(type_error("Iterator is an abstract class"))
}

fn iterator_from(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let v = arg(a, 0);
    let f = it.get(&v, "@@iterator")?;
    if Interp::is_callable(&f) {
        return it.call(&f, v, &[]);
    }
    Ok(v)
}

pub fn install(it: &mut Interp) {
    let ip = it.new_obj(it.p.object.clone(), Kind::Plain);
    it.def_method(&ip, "@@iterator", identity);
    it.def_method(&ip, "toArray", to_array);
    for (n, f) in HELPERS {
        it.def_method(&ip, n, *f);
    }
    let ap = it.new_obj(Some(ip.clone()), Kind::Plain);
    it.def_method(&ap, "next", iter_next);
    Interp::set_hidden(&ap, "@@toStringTag", Value::str("Array Iterator"));
    if let Some(g) = &it.p.generator {
        g.borrow_mut().proto = Some(ip.clone());
    }
    let ctor = it.native(iterator_ctor, "Iterator");
    if let Value::Obj(c) = &ctor {
        Interp::set_hidden(c, "prototype", Value::Obj(ip.clone()));
        Interp::set_hidden(&ip, "constructor", ctor.clone());
        it.def_method(c, "from", iterator_from);
    }
    Interp::set_hidden(&it.global.clone(), "Iterator", ctor);
    it.p.iterator = Some(ip);
    it.p.array_iterator = Some(ap);
}
