//! Binary data: ArrayBuffer, typed arrays, DataView, TextEncoder /
//! TextDecoder and crypto.getRandomValues.

use super::builtins::arg;
use super::*;

/// Typed array element types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ta {
    I8,
    U8,
    U8C,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

pub const TA_ALL: [Ta; 9] = [Ta::I8, Ta::U8, Ta::U8C, Ta::I16, Ta::U16, Ta::I32, Ta::U32, Ta::F32, Ta::F64];

impl Ta {
    pub fn size(self) -> usize {
        match self {
            Ta::I8 | Ta::U8 | Ta::U8C => 1,
            Ta::I16 | Ta::U16 => 2,
            Ta::I32 | Ta::U32 | Ta::F32 => 4,
            Ta::F64 => 8,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Ta::I8 => "Int8Array",
            Ta::U8 => "Uint8Array",
            Ta::U8C => "Uint8ClampedArray",
            Ta::I16 => "Int16Array",
            Ta::U16 => "Uint16Array",
            Ta::I32 => "Int32Array",
            Ta::U32 => "Uint32Array",
            Ta::F32 => "Float32Array",
            Ta::F64 => "Float64Array",
        }
    }
    fn index(self) -> usize {
        TA_ALL.iter().position(|&t| t == self).unwrap()
    }
}

/// A typed array view (also used for DataView with `ty == U8`).
#[derive(Clone)]
pub struct View {
    pub buffer: ObjRef,
    pub off: usize,
    pub len: usize,
    pub ty: Ta,
}

fn to_int(v: f64, bits: u32, signed: bool) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    let m = 1i128 << bits;
    let t = (v.trunc() as i128).rem_euclid(m);
    if signed && t >= m / 2 { (t - m) as i64 } else { t as i64 }
}

/// Read element `i` of a byte buffer (little-endian).
pub fn read(d: &[u8], at: usize, ty: Ta) -> f64 {
    let b = |n: usize| -> [u8; 8] {
        let mut a = [0u8; 8];
        a[..n].copy_from_slice(&d[at..at + n]);
        a
    };
    match ty {
        Ta::I8 => d[at] as i8 as f64,
        Ta::U8 | Ta::U8C => d[at] as f64,
        Ta::I16 => i16::from_le_bytes(b(2)[..2].try_into().unwrap()) as f64,
        Ta::U16 => u16::from_le_bytes(b(2)[..2].try_into().unwrap()) as f64,
        Ta::I32 => i32::from_le_bytes(b(4)[..4].try_into().unwrap()) as f64,
        Ta::U32 => u32::from_le_bytes(b(4)[..4].try_into().unwrap()) as f64,
        Ta::F32 => f32::from_le_bytes(b(4)[..4].try_into().unwrap()) as f64,
        Ta::F64 => f64::from_le_bytes(b(8)),
    }
}

pub fn write(d: &mut [u8], at: usize, ty: Ta, v: f64) {
    match ty {
        Ta::I8 | Ta::U8 => d[at] = to_int(v, 8, false) as u8,
        Ta::U8C => {
            d[at] = if v.is_nan() {
                0
            } else {
                // Round half to even, then clamp.
                let c = v.clamp(0.0, 255.0);
                let r = c.round();
                (if (c - c.floor() - 0.5).abs() < 1e-12 && r as i64 % 2 == 1 { r - 1.0 } else { r }) as u8
            }
        }
        Ta::I16 | Ta::U16 => d[at..at + 2].copy_from_slice(&(to_int(v, 16, false) as u16).to_le_bytes()),
        Ta::I32 | Ta::U32 => d[at..at + 4].copy_from_slice(&(to_int(v, 32, false) as u32).to_le_bytes()),
        Ta::F32 => d[at..at + 4].copy_from_slice(&(v as f32).to_le_bytes()),
        Ta::F64 => d[at..at + 8].copy_from_slice(&v.to_le_bytes()),
    }
}

impl View {
    pub fn get(&self, i: usize) -> Option<f64> {
        if i >= self.len {
            return None;
        }
        let b = self.buffer.borrow();
        let Kind::Buffer(d) = &b.kind else { return None };
        let at = self.off + i * self.ty.size();
        (at + self.ty.size() <= d.len()).then(|| read(d, at, self.ty))
    }
    pub fn set(&self, i: usize, v: f64) {
        if i >= self.len {
            return;
        }
        let mut b = self.buffer.borrow_mut();
        let Kind::Buffer(d) = &mut b.kind else { return };
        let at = self.off + i * self.ty.size();
        if at + self.ty.size() <= d.len() {
            write(d, at, self.ty, v);
        }
    }
    pub fn values(&self) -> Vec<f64> {
        (0..self.len).filter_map(|i| self.get(i)).collect()
    }
    pub fn bytes(&self) -> Vec<u8> {
        let b = self.buffer.borrow();
        let Kind::Buffer(d) = &b.kind else { return Vec::new() };
        let end = (self.off + self.len * self.ty.size()).min(d.len());
        d[self.off.min(end)..end].to_vec()
    }
}

/// The view behind a typed array or DataView value.
pub fn view_of(v: &Value) -> Option<View> {
    match &v.obj()?.borrow().kind {
        Kind::Typed(t) | Kind::DataView(t) => Some(t.clone()),
        _ => None,
    }
}

/// Bytes of an ArrayBuffer, typed array or DataView.
pub fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    let o = v.obj()?;
    if let Kind::Buffer(d) = &o.borrow().kind {
        return Some(d.clone());
    }
    view_of(v).map(|t| t.bytes())
}

pub fn new_buffer(it: &Interp, data: Vec<u8>) -> ObjRef {
    it.new_obj(it.p.array_buffer.clone(), Kind::Buffer(data))
}

pub fn new_typed(it: &Interp, ty: Ta, buffer: ObjRef, off: usize, len: usize) -> Value {
    let proto = it.p.typed.get(ty.index()).cloned();
    Value::Obj(it.new_obj(proto, Kind::Typed(View { buffer, off, len, ty })))
}

/// A new Uint8Array holding `data`.
pub fn uint8_array(it: &Interp, data: Vec<u8>) -> Value {
    let n = data.len();
    let b = new_buffer(it, data);
    new_typed(it, Ta::U8, b, 0, n)
}

fn this_view(t: &Value) -> R<View> {
    view_of(t).ok_or_else(|| type_error("not a typed array"))
}

fn construct_typed(it: &mut Interp, ty: Ta, a: &[Value]) -> R<Value> {
    let a0 = arg(a, 0);
    match &a0 {
        Value::Undef | Value::Null => {
            let b = new_buffer(it, Vec::new());
            Ok(new_typed(it, ty, b, 0, 0))
        }
        Value::Num(n) => {
            let n = *n as usize;
            if n > 1 << 30 {
                return Err(builtins::make_error_value("RangeError", "Invalid typed array length"));
            }
            let b = new_buffer(it, vec![0; n * ty.size()]);
            Ok(new_typed(it, ty, b, 0, n))
        }
        Value::Obj(o) => {
            let is_buf = matches!(o.borrow().kind, Kind::Buffer(_));
            if is_buf {
                let blen = match &o.borrow().kind {
                    Kind::Buffer(d) => d.len(),
                    _ => 0,
                };
                let off = match arg(a, 1) {
                    Value::Undef => 0,
                    v => it.to_num(&v)? as usize,
                };
                if off % ty.size() != 0 || off > blen {
                    return Err(builtins::make_error_value("RangeError", "start offset should be a multiple of the element size"));
                }
                let len = match arg(a, 2) {
                    Value::Undef => (blen - off) / ty.size(),
                    v => it.to_num(&v)? as usize,
                };
                if off + len * ty.size() > blen {
                    return Err(builtins::make_error_value("RangeError", "Invalid typed array length"));
                }
                return Ok(new_typed(it, ty, o.clone(), off, len));
            }
            let vals: Vec<f64> = if let Some(v) = view_of(&a0) {
                v.values()
            } else {
                let items = it.iterate(&a0)?;
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(it.to_num(x)?);
                }
                out
            };
            let n = vals.len();
            let b = new_buffer(it, vec![0; n * ty.size()]);
            let t = new_typed(it, ty, b, 0, n);
            let v = view_of(&t).unwrap();
            for (i, x) in vals.into_iter().enumerate() {
                v.set(i, x);
            }
            Ok(t)
        }
        other => {
            let n = it.to_num(other)? as usize;
            let b = new_buffer(it, vec![0; n * ty.size()]);
            Ok(new_typed(it, ty, b, 0, n))
        }
    }
}

macro_rules! typed_ctors {
    ($($f:ident => $t:expr),*) => {
        $(fn $f(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
            construct_typed(it, $t, a)
        })*
        const CTORS: [NativeFn; 9] = [$($f),*];
    };
}

typed_ctors!(
    ctor_i8 => Ta::I8, ctor_u8 => Ta::U8, ctor_u8c => Ta::U8C, ctor_i16 => Ta::I16, ctor_u16 => Ta::U16,
    ctor_i32 => Ta::I32, ctor_u32 => Ta::U32, ctor_f32 => Ta::F32, ctor_f64 => Ta::F64
);

fn rel_index(it: &mut Interp, v: &Value, len: usize, default: usize) -> R<usize> {
    if matches!(v, Value::Undef) {
        return Ok(default);
    }
    let n = it.to_num(v)?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    Ok(if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { (n as usize).min(len) })
}

fn from_values(it: &Interp, ty: Ta, vals: &[f64]) -> Value {
    let b = new_buffer(it, vec![0; vals.len() * ty.size()]);
    let t = new_typed(it, ty, b, 0, vals.len());
    let v = view_of(&t).unwrap();
    for (i, &x) in vals.iter().enumerate() {
        v.set(i, x);
    }
    t
}

fn ta_set(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let src = arg(a, 0);
    let off = match arg(a, 1) {
        Value::Undef => 0,
        x => it.to_num(&x)? as usize,
    };
    let vals: Vec<f64> = if let Some(s) = view_of(&src) {
        s.values()
    } else {
        let items = it.iterate(&src)?;
        let mut out = Vec::new();
        for x in &items {
            out.push(it.to_num(x)?);
        }
        out
    };
    if off + vals.len() > v.len {
        return Err(builtins::make_error_value("RangeError", "offset is out of bounds"));
    }
    for (i, x) in vals.into_iter().enumerate() {
        v.set(off + i, x);
    }
    Ok(Value::Undef)
}

fn ta_subarray(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let s = rel_index(it, &arg(a, 0), v.len, 0)?;
    let e = rel_index(it, &arg(a, 1), v.len, v.len)?.max(s);
    Ok(new_typed(it, v.ty, v.buffer.clone(), v.off + s * v.ty.size(), e - s))
}

fn ta_slice(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let s = rel_index(it, &arg(a, 0), v.len, 0)?;
    let e = rel_index(it, &arg(a, 1), v.len, v.len)?.max(s);
    let vals = v.values();
    Ok(from_values(it, v.ty, &vals[s..e]))
}

fn ta_fill(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let x = it.to_num(&arg(a, 0))?;
    let s = rel_index(it, &arg(a, 1), v.len, 0)?;
    let e = rel_index(it, &arg(a, 2), v.len, v.len)?;
    for i in s..e {
        v.set(i, x);
    }
    Ok(t)
}

fn ta_copy_within(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let target = rel_index(it, &arg(a, 0), v.len, 0)?;
    let s = rel_index(it, &arg(a, 1), v.len, 0)?;
    let e = rel_index(it, &arg(a, 2), v.len, v.len)?;
    let vals = v.values();
    for (k, i) in (s..e).enumerate() {
        if target + k < v.len {
            v.set(target + k, vals[i]);
        }
    }
    Ok(t)
}

fn ta_index_of(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let x = it.to_num(&arg(a, 0))?;
    let s = rel_index(it, &arg(a, 1), v.len, 0)?;
    Ok(Value::Num(v.values().iter().skip(s).position(|&y| y == x).map_or(-1.0, |p| (p + s) as f64)))
}

fn ta_last_index_of(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let x = it.to_num(&arg(a, 0))?;
    Ok(Value::Num(v.values().iter().rposition(|&y| y == x).map_or(-1.0, |p| p as f64)))
}

fn ta_includes(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let x = it.to_num(&arg(a, 0))?;
    Ok(Value::Bool(v.values().iter().any(|&y| y == x || (x.is_nan() && y.is_nan()))))
}

fn ta_join(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let sep = match arg(a, 0) {
        Value::Undef => Rc::from(","),
        s => it.to_str(&s)?,
    };
    Ok(Value::str(&v.values().iter().map(|&x| num_to_str(x)).collect::<Vec<_>>().join(&sep)))
}

fn ta_reverse(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let vals = v.values();
    for (i, x) in vals.into_iter().rev().enumerate() {
        v.set(i, x);
    }
    Ok(t)
}

fn ta_sort(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let mut vals = v.values();
    let cmp = arg(a, 0);
    if Interp::is_callable(&cmp) {
        // Insertion sort through the comparator (stable).
        for i in 1..vals.len() {
            let mut j = i;
            while j > 0 {
                let r = it.call(&cmp, Value::Undef, &[Value::Num(vals[j - 1]), Value::Num(vals[j])])?;
                if it.to_num(&r)? > 0.0 {
                    vals.swap(j - 1, j);
                    j -= 1;
                } else {
                    break;
                }
            }
        }
    } else {
        vals.sort_by(|x, y| x.total_cmp(y));
    }
    for (i, x) in vals.into_iter().enumerate() {
        v.set(i, x);
    }
    Ok(t)
}

/// Shared driver for the callback methods.
fn ta_callback(it: &mut Interp, t: Value, a: &[Value], kind: u8) -> R<Value> {
    let v = this_view(&t)?;
    let f = arg(a, 0);
    if !Interp::is_callable(&f) {
        return Err(type_error("callback is not a function"));
    }
    let this_arg = arg(a, 1);
    let vals = v.values();
    let mut mapped = Vec::new();
    for (i, &x) in vals.iter().enumerate() {
        let r = it.call(&f, this_arg.clone(), &[Value::Num(x), Value::Num(i as f64), t.clone()])?;
        match kind {
            0 => {}
            1 => mapped.push(it.to_num(&r)?),
            2 => {
                if r.truthy() {
                    mapped.push(x);
                }
            }
            3 => {
                if !r.truthy() {
                    return Ok(Value::Bool(false));
                }
            }
            4 => {
                if r.truthy() {
                    return Ok(Value::Bool(true));
                }
            }
            5 => {
                if r.truthy() {
                    return Ok(Value::Num(x));
                }
            }
            _ => {
                if r.truthy() {
                    return Ok(Value::Num(i as f64));
                }
            }
        }
    }
    Ok(match kind {
        0 => Value::Undef,
        1 | 2 => from_values(it, v.ty, &mapped),
        3 => Value::Bool(true),
        4 => Value::Bool(false),
        5 => Value::Undef,
        _ => Value::Num(-1.0),
    })
}

fn ta_reduce(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let f = arg(a, 0);
    let vals = v.values();
    let mut iter = vals.iter().enumerate();
    let mut acc = if a.len() > 1 {
        arg(a, 1)
    } else {
        match iter.next() {
            Some((_, &x)) => Value::Num(x),
            None => return Err(type_error("Reduce of empty array with no initial value")),
        }
    };
    for (i, &x) in iter {
        acc = it.call(&f, Value::Undef, &[acc, Value::Num(x), Value::Num(i as f64), t.clone()])?;
    }
    Ok(acc)
}

fn ta_at(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = this_view(&t)?;
    let n = it.to_num(&arg(a, 0))?.trunc();
    let i = if n < 0.0 { v.len as f64 + n } else { n };
    Ok(if i < 0.0 { Value::Undef } else { v.get(i as usize).map_or(Value::Undef, Value::Num) })
}

/// keys / values / entries: iterate a plain array copy.
fn ta_iter(it: &mut Interp, t: Value, which: &str) -> R<Value> {
    let v = this_view(&t)?;
    let vals: Vec<Value> = v.values().into_iter().map(Value::Num).collect();
    let arr = it.array(vals);
    let m = it.get(&arr, which)?;
    it.call(&m, arr, &[])
}

fn ta_to_string(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    ta_join(it, t, &[])
}

fn ta_from(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    // `this` is the constructor; build via it.
    let items = it.iterate(&arg(a, 0))?;
    let f = arg(a, 1);
    let mut vals = Vec::with_capacity(items.len());
    for (i, x) in items.into_iter().enumerate() {
        vals.push(if Interp::is_callable(&f) { it.call(&f, Value::Undef, &[x, Value::Num(i as f64)])? } else { x });
    }
    let arr = it.array(vals);
    it.construct(&t, &[arr])
}

fn ta_of(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let arr = it.array(a.to_vec());
    it.construct(&t, &[arr])
}

// ------------------------------------------------------------ ArrayBuffer

fn buffer_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let n = it.to_num(&arg(a, 0))?;
    let n = if n.is_finite() && n > 0.0 { n as usize } else { 0 };
    if n > 1 << 30 {
        return Err(builtins::make_error_value("RangeError", "Array buffer allocation failed"));
    }
    Ok(Value::Obj(new_buffer(it, vec![0; n])))
}

fn buffer_slice(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let d = bytes_of(&t).ok_or_else(|| type_error("not an ArrayBuffer"))?;
    let s = rel_index(it, &arg(a, 0), d.len(), 0)?;
    let e = rel_index(it, &arg(a, 1), d.len(), d.len())?.max(s);
    Ok(Value::Obj(new_buffer(it, d[s..e].to_vec())))
}

// --------------------------------------------------------------- DataView

fn dataview_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let b = arg(a, 0);
    let Some(bo) = b.obj().filter(|o| matches!(o.borrow().kind, Kind::Buffer(_))).cloned() else {
        return Err(type_error("First argument to DataView constructor must be an ArrayBuffer"));
    };
    let blen = bytes_of(&b).map_or(0, |d| d.len());
    let off = match arg(a, 1) {
        Value::Undef => 0,
        v => it.to_num(&v)? as usize,
    };
    let len = match arg(a, 2) {
        Value::Undef => blen.saturating_sub(off),
        v => it.to_num(&v)? as usize,
    };
    if off + len > blen {
        return Err(builtins::make_error_value("RangeError", "Invalid DataView length"));
    }
    Ok(Value::Obj(it.new_obj(it.p.data_view.clone(), Kind::DataView(View { buffer: bo, off, len, ty: Ta::U8 }))))
}

fn dv_access(it: &mut Interp, t: &Value, a: &[Value], ty: Ta, set: bool) -> R<Value> {
    let v = this_view(t)?;
    let at = it.to_num(&arg(a, 0))? as usize;
    if at + ty.size() > v.len {
        return Err(builtins::make_error_value("RangeError", "Offset is outside the bounds of the DataView"));
    }
    let little = arg(a, if set { 2 } else { 1 }).truthy();
    let mut buf = v.buffer.borrow_mut();
    let Kind::Buffer(d) = &mut buf.kind else { return Ok(Value::Undef) };
    let pos = v.off + at;
    if pos + ty.size() > d.len() {
        return Err(type_error("Cannot perform DataView access on a detached ArrayBuffer"));
    }
    if set {
        let x = drop_and_num(it, &arg(a, 1));
        let mut tmp = [0u8; 8];
        write(&mut tmp, 0, ty, x);
        let n = ty.size();
        if !little {
            tmp[..n].reverse();
        }
        d[pos..pos + n].copy_from_slice(&tmp[..n]);
        Ok(Value::Undef)
    } else {
        let n = ty.size();
        let mut tmp = [0u8; 8];
        tmp[..n].copy_from_slice(&d[pos..pos + n]);
        if !little {
            tmp[..n].reverse();
        }
        Ok(Value::Num(read(&tmp, 0, ty)))
    }
}

/// Numeric conversion for primitives (DataView setters take numbers).
fn drop_and_num(_: &mut Interp, v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Bool(b) => *b as u8 as f64,
        Value::Str(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

macro_rules! dv_methods {
    ($($get:ident, $set:ident => $t:expr),*) => {
        $(
            fn $get(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> { dv_access(it, &t, a, $t, false) }
            fn $set(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> { dv_access(it, &t, a, $t, true) }
        )*
    };
}
dv_methods!(
    dv_get_i8, dv_set_i8 => Ta::I8, dv_get_u8, dv_set_u8 => Ta::U8, dv_get_i16, dv_set_i16 => Ta::I16,
    dv_get_u16, dv_set_u16 => Ta::U16, dv_get_i32, dv_set_i32 => Ta::I32, dv_get_u32, dv_set_u32 => Ta::U32,
    dv_get_f32, dv_set_f32 => Ta::F32, dv_get_f64, dv_set_f64 => Ta::F64
);

// ------------------------------------------------------------------ text

fn text_encoder_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let o = it.plain();
    Interp::set_prop(&o, "encoding", Value::str("utf-8"));
    it.def_method(&o, "encode", |it, _, a| {
        let s = match arg(a, 0) {
            Value::Undef => Rc::from(""),
            v => it.to_str(&v)?,
        };
        Ok(uint8_array(it, s.as_bytes().to_vec()))
    });
    it.def_method(&o, "encodeInto", |it, _, a| {
        let s = it.to_str(&arg(a, 0))?;
        let v = view_of(&arg(a, 1)).ok_or_else(|| type_error("not a Uint8Array"))?;
        let mut written = 0;
        let mut read_units = 0;
        for c in s.chars() {
            let mut b = [0u8; 4];
            let enc = c.encode_utf8(&mut b);
            if written + enc.len() > v.len {
                break;
            }
            for (k, &byte) in enc.as_bytes().iter().enumerate() {
                v.set(written + k, byte as f64);
            }
            written += enc.len();
            read_units += c.len_utf16();
        }
        let r = it.plain();
        Interp::set_prop(&r, "read", Value::Num(read_units as f64));
        Interp::set_prop(&r, "written", Value::Num(written as f64));
        Ok(Value::Obj(r))
    });
    Ok(Value::Obj(o))
}

/// Decode bytes with a WHATWG encoding label (UTF-8, Latin-1 family, UTF-16LE).
pub fn decode_bytes(label: &str, d: &[u8]) -> String {
    let l = label.trim().to_ascii_lowercase();
    if l.contains("utf-16") || l == "unicode" {
        let units: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if matches!(l.as_str(), "latin1" | "iso-8859-1" | "windows-1252" | "ascii" | "us-ascii") {
        return d.iter().map(|&b| b as char).collect();
    }
    let d = d.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(d);
    String::from_utf8_lossy(d).into_owned()
}

fn text_decoder_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let label = match arg(a, 0) {
        Value::Undef => Rc::from("utf-8"),
        v => it.to_str(&v)?,
    };
    let o = it.plain();
    Interp::set_prop(&o, "encoding", Value::str(&label.to_ascii_lowercase()));
    it.def_method(&o, "decode", |it, t, a| {
        let label = it.get(&t, "encoding")?;
        let label = it.to_str(&label)?;
        let d = bytes_of(&arg(a, 0)).unwrap_or_default();
        Ok(Value::str(&decode_bytes(&label, &d)))
    });
    Ok(Value::Obj(o))
}

// ---------------------------------------------------------------- crypto

fn next_rand(it: &mut Interp) -> u64 {
    let mut x = it.rng;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    it.rng = x;
    x
}

fn get_random_values(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let t = arg(a, 0);
    let v = this_view(&t)?;
    for i in 0..v.len {
        let r = next_rand(it);
        let x = match v.ty {
            Ta::F32 | Ta::F64 => return Err(type_error("The data is not an integer typed array")),
            _ => (r & 0xFFFF_FFFF) as f64,
        };
        v.set(i, x);
    }
    Ok(t)
}

fn random_uuid(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let mut b = [0u8; 16];
    for c in b.chunks_mut(8) {
        c.copy_from_slice(&next_rand(it).to_le_bytes());
    }
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(Value::str(&format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])))
}

// --------------------------------------------------------------- install

pub fn install(it: &mut Interp) {
    let g = it.global.clone();
    let object = it.p.object.clone();
    let ctor = |it: &mut Interp, name: &'static str, f: NativeFn, proto: &ObjRef| -> ObjRef {
        let c = it.native(f, name);
        let co = c.obj().unwrap().clone();
        Interp::set_hidden(&co, "prototype", Value::Obj(proto.clone()));
        Interp::set_hidden(proto, "constructor", c.clone());
        Interp::set_hidden(&it.global.clone(), name, c);
        co
    };

    // ArrayBuffer
    let abp = it.new_obj(object.clone(), Kind::Plain);
    it.def_getter(&abp, "byteLength", |_, t, _| Ok(Value::Num(bytes_of(&t).map_or(0, |d| d.len()) as f64)));
    it.def_method(&abp, "slice", buffer_slice);
    it.p.array_buffer = Some(abp.clone());
    let abc = ctor(it, "ArrayBuffer", buffer_ctor, &abp);
    it.def_method(&abc, "isView", |_, _, a| Ok(Value::Bool(view_of(&arg(a, 0)).is_some())));
    Interp::set_hidden(&g, "SharedArrayBuffer", Value::Obj(abc));

    // %TypedArray%.prototype
    let tap = it.new_obj(object.clone(), Kind::Plain);
    for (n, f) in [
        ("set", ta_set as NativeFn),
        ("subarray", ta_subarray),
        ("slice", ta_slice),
        ("fill", ta_fill),
        ("copyWithin", ta_copy_within),
        ("indexOf", ta_index_of),
        ("lastIndexOf", ta_last_index_of),
        ("includes", ta_includes),
        ("join", ta_join),
        ("reverse", ta_reverse),
        ("sort", ta_sort),
        ("forEach", |it, t, a| ta_callback(it, t, a, 0)),
        ("map", |it, t, a| ta_callback(it, t, a, 1)),
        ("filter", |it, t, a| ta_callback(it, t, a, 2)),
        ("every", |it, t, a| ta_callback(it, t, a, 3)),
        ("some", |it, t, a| ta_callback(it, t, a, 4)),
        ("find", |it, t, a| ta_callback(it, t, a, 5)),
        ("findIndex", |it, t, a| ta_callback(it, t, a, 6)),
        ("reduce", ta_reduce),
        ("at", ta_at),
        ("keys", |it, t, _| ta_iter(it, t, "keys")),
        ("values", |it, t, _| ta_iter(it, t, "values")),
        ("entries", |it, t, _| ta_iter(it, t, "entries")),
        ("@@iterator", |it, t, _| ta_iter(it, t, "values")),
        ("toString", ta_to_string),
        ("toLocaleString", ta_to_string),
    ] {
        it.def_method(&tap, n, f);
    }
    it.def_getter(&tap, "buffer", |_, t, _| Ok(view_of(&t).map_or(Value::Undef, |v| Value::Obj(v.buffer))));
    it.def_getter(&tap, "byteLength", |_, t, _| Ok(Value::Num(view_of(&t).map_or(0, |v| v.len * v.ty.size()) as f64)));
    it.def_getter(&tap, "byteOffset", |_, t, _| Ok(Value::Num(view_of(&t).map_or(0, |v| v.off) as f64)));
    it.def_getter(&tap, "length", |_, t, _| Ok(Value::Num(view_of(&t).map_or(0, |v| v.len) as f64)));
    let mut protos = Vec::new();
    for (i, ty) in TA_ALL.iter().enumerate() {
        let p = it.new_obj(Some(tap.clone()), Kind::Plain);
        Interp::set_hidden(&p, "BYTES_PER_ELEMENT", Value::Num(ty.size() as f64));
        Interp::set_hidden(&p, "@@toStringTag", Value::str(ty.name()));
        let c = ctor(it, ty.name(), CTORS[i], &p);
        Interp::set_hidden(&c, "BYTES_PER_ELEMENT", Value::Num(ty.size() as f64));
        it.def_method(&c, "from", ta_from);
        it.def_method(&c, "of", ta_of);
        protos.push(p);
    }
    it.p.typed = protos;

    // DataView
    let dvp = it.new_obj(object.clone(), Kind::Plain);
    for (n, f) in [
        ("getInt8", dv_get_i8 as NativeFn),
        ("setInt8", dv_set_i8),
        ("getUint8", dv_get_u8),
        ("setUint8", dv_set_u8),
        ("getInt16", dv_get_i16),
        ("setInt16", dv_set_i16),
        ("getUint16", dv_get_u16),
        ("setUint16", dv_set_u16),
        ("getInt32", dv_get_i32),
        ("setInt32", dv_set_i32),
        ("getUint32", dv_get_u32),
        ("setUint32", dv_set_u32),
        ("getFloat32", dv_get_f32),
        ("setFloat32", dv_set_f32),
        ("getFloat64", dv_get_f64),
        ("setFloat64", dv_set_f64),
    ] {
        it.def_method(&dvp, n, f);
    }
    it.def_getter(&dvp, "buffer", |_, t, _| Ok(view_of(&t).map_or(Value::Undef, |v| Value::Obj(v.buffer))));
    it.def_getter(&dvp, "byteLength", |_, t, _| Ok(Value::Num(view_of(&t).map_or(0, |v| v.len) as f64)));
    it.def_getter(&dvp, "byteOffset", |_, t, _| Ok(Value::Num(view_of(&t).map_or(0, |v| v.off) as f64)));
    it.p.data_view = Some(dvp.clone());
    ctor(it, "DataView", dataview_ctor, &dvp);

    // TextEncoder / TextDecoder
    let tep = it.new_obj(object.clone(), Kind::Plain);
    ctor(it, "TextEncoder", text_encoder_ctor, &tep);
    let tdp = it.new_obj(object.clone(), Kind::Plain);
    ctor(it, "TextDecoder", text_decoder_ctor, &tdp);

    // crypto
    let crypto = it.plain();
    it.def_method(&crypto, "getRandomValues", get_random_values);
    it.def_method(&crypto, "randomUUID", random_uuid);
    Interp::set_hidden(&g, "crypto", Value::Obj(crypto));
}

#[cfg(test)]
mod tests {
    use super::super::Interp;
    use crate::dom::Document;

    fn run(src: &str) -> String {
        let mut it = Interp::new(Document::new(), "https://example.com/", 800.0, 600.0);
        it.run_script(src).unwrap_or_else(|e| panic!("{e:?}"));
        it.console.join("\n")
    }

    #[test]
    fn typed_arrays() {
        let out = run(r#"
            const b = new ArrayBuffer(8);
            const u8 = new Uint8Array(b);
            const u32 = new Uint32Array(b);
            u32[0] = 0x01020304;
            console.log(Array.from(u8.subarray(0, 4)).join(','));
            const f = new Float64Array([1.5, -2, 3]);
            console.log(f.length, f.byteLength, f.map(x => x * 2).join(' '));
            const c = new Uint8ClampedArray([300, -5, 1.5, 2.5]);
            console.log(c.join(','));
            const i8 = Int8Array.from([127, 128, 255]);
            console.log(i8.join(','), [...i8].length);
            const dv = new DataView(b);
            dv.setUint16(4, 0xABCD);
            console.log(dv.getUint16(4).toString(16), dv.getUint16(4, true).toString(16), u8[4]);
            const t = new TextEncoder().encode('héllo');
            console.log(t.length, new TextDecoder().decode(t));
            console.log(crypto.getRandomValues(new Uint8Array(4)).length, Object.keys(new Uint16Array(3)).join(''));
            let s = 0; for (const x of new Int16Array([1, 2, 3])) s += x; console.log(s);
        "#);
        assert_eq!(out, "4,3,2,1\n3 24 3 -4 6\n255,0,2,2\n127,-128,-1 3\nabcd cdab 171\n6 héllo\n4 012\n6");
    }
}
