//! The WebAssembly runtime: instances, memories, tables, globals and the
//! interpreter loop.
//!
//! Values are untyped 64-bit slots: i32 and f32 are stored zero-extended
//! (f32 as its bits), references as ids into the function / extern stores
//! (0 = null). A linear memory's bytes live in its ArrayBuffer object; a
//! running call takes them out ("checks them out") and puts them back
//! whenever JS can run, so JS views see the current contents.

use super::*;
use crate::js::{binary, Interp, Kind, ObjRef, Value, R};
use std::cell::{Cell, RefCell};

pub const PAGE: usize = 65536;
/// Pages a memory can have at most (4 GiB / 64 KiB, capped at 2 GiB).
pub const MAX_PAGES: u32 = 32768;

pub struct Memory {
    pub obj: ObjRef,
    pub max: Option<u32>,
    /// Byte length of `obj` (its bytes may be checked out by a call).
    pub len: usize,
}

pub struct Table {
    pub elems: Vec<u64>,
    pub max: Option<u32>,
    pub ty: ValType,
}

pub struct Global {
    pub v: Cell<u64>,
    pub ty: ValType,
    pub mutable: bool,
}

pub enum FuncEntry {
    Wasm { inst: Rc<Instance>, idx: u32 },
    Host(Value),
}

pub struct FuncRec {
    pub entry: FuncEntry,
    /// Canonical type id.
    pub ty: u32,
}

thread_local! {
    static FUNCS: RefCell<Vec<Rc<FuncRec>>> = const { RefCell::new(Vec::new()) };
    static EXTERNS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    static TYPES: RefCell<Vec<FuncType>> = const { RefCell::new(Vec::new()) };
    static STACKS: RefCell<Vec<Vec<u64>>> = const { RefCell::new(Vec::new()) };
}

/// Canonical id of a function type (equal types share an id).
pub fn canon(t: &FuncType) -> u32 {
    TYPES.with(|ts| {
        let mut ts = ts.borrow_mut();
        match ts.iter().position(|x| x == t) {
            Some(i) => i as u32,
            None => {
                ts.push(t.clone());
                ts.len() as u32 - 1
            }
        }
    })
}

pub fn canon_type(id: u32) -> FuncType {
    TYPES.with(|ts| ts.borrow()[id as usize].clone())
}

pub fn next_func_id() -> u64 {
    FUNCS.with(|f| f.borrow().len() as u64 + 1)
}

pub fn add_func(rec: FuncRec) -> u64 {
    FUNCS.with(|f| {
        let mut f = f.borrow_mut();
        f.push(Rc::new(rec));
        f.len() as u64
    })
}

pub fn func(id: u64) -> Option<Rc<FuncRec>> {
    if id == 0 {
        return None;
    }
    FUNCS.with(|f| f.borrow().get(id as usize - 1).cloned())
}

/// An externref id for a JS value (null is 0).
pub fn extern_id(v: &Value) -> u64 {
    if matches!(v, Value::Null) {
        return 0;
    }
    EXTERNS.with(|e| {
        let mut e = e.borrow_mut();
        e.push(v.clone());
        e.len() as u64
    })
}

pub fn extern_value(id: u64) -> Value {
    if id == 0 {
        return Value::Null;
    }
    EXTERNS.with(|e| e.borrow().get(id as usize - 1).cloned().unwrap_or(Value::Null))
}

pub struct Instance {
    pub module: Rc<Module>,
    /// Function store id of every function index.
    pub func_ids: Vec<u64>,
    /// Canonical ids of the module's types.
    pub type_canon: Vec<u32>,
    pub tables: Vec<Rc<RefCell<Table>>>,
    pub memory: Option<Rc<RefCell<Memory>>>,
    pub globals: Vec<Rc<Global>>,
    pub dropped_data: RefCell<Vec<bool>>,
    /// Element segment values (emptied when dropped).
    pub elem_vals: RefCell<Vec<Vec<u64>>>,
}

/// A trap: a WebAssembly.RuntimeError.
pub fn trap(it: &mut Interp, msg: &str) -> Value {
    let g = Value::Obj(it.global.clone());
    let make = it.get(&g, "@@wasmError").unwrap_or_default();
    it.call(&make, Value::Undef, &[Value::str("RuntimeError"), Value::str(msg)]).unwrap_or_else(|e| e)
}

fn take_mem(m: &Option<Rc<RefCell<Memory>>>) -> Vec<u8> {
    let Some(m) = m else { return Vec::new() };
    let mm = m.borrow();
    let mut o = mm.obj.borrow_mut();
    match &mut o.kind {
        Kind::Buffer(v) => std::mem::take(v),
        _ => Vec::new(),
    }
}

/// Put checked-out bytes back. A grown memory gets a new ArrayBuffer (the
/// old one stays detached, with length 0).
fn put_mem(it: &Interp, m: &Option<Rc<RefCell<Memory>>>, data: Vec<u8>, _len_before: usize) {
    let Some(m) = m else { return };
    let mut mm = m.borrow_mut();
    if data.len() != mm.len {
        mm.len = data.len();
        mm.obj = binary::new_buffer(it, data);
    } else if let Kind::Buffer(v) = &mut mm.obj.borrow_mut().kind {
        *v = data;
    }
}

/// Grow a memory from outside a running call; returns the old size in
/// pages, or None if it can't grow.
pub fn grow_memory(it: &Interp, m: &Rc<RefCell<Memory>>, delta: u32) -> Option<u32> {
    let mm = Some(m.clone());
    let mut data = take_mem(&mm);
    let before = data.len();
    let old = (before / PAGE) as u32;
    let max = m.borrow().max.unwrap_or(MAX_PAGES).min(MAX_PAGES);
    let new = old.checked_add(delta).filter(|&n| n <= max);
    if let Some(n) = new {
        if delta > 0 {
            data.resize(n as usize * PAGE, 0);
        } else {
            // memory.grow(0) still detaches the old buffer.
            m.borrow_mut().obj = binary::new_buffer(it, data);
            return Some(old);
        }
    }
    put_mem(it, &mm, data, before);
    new.map(|_| old)
}

/// Evaluate a constant expression.
pub fn eval_const(e: &[ConstOp], globals: &[Rc<Global>], func_ids: &[u64]) -> u64 {
    let mut st: Vec<u64> = Vec::with_capacity(2);
    for op in e {
        let v = match *op {
            ConstOp::I32(v) => v as u32 as u64,
            ConstOp::I64(v) => v as u64,
            ConstOp::F32(v) => v as u64,
            ConstOp::F64(v) => v,
            ConstOp::RefNull => 0,
            ConstOp::RefFunc(f) => func_ids.get(f as usize).copied().unwrap_or(0),
            ConstOp::GlobalGet(g) => globals.get(g as usize).map_or(0, |g| g.v.get()),
            ConstOp::I32Add | ConstOp::I32Sub | ConstOp::I32Mul | ConstOp::I64Add | ConstOp::I64Sub | ConstOp::I64Mul => {
                let b = st.pop().unwrap_or(0);
                let a = st.pop().unwrap_or(0);
                match *op {
                    ConstOp::I32Add => (a as u32).wrapping_add(b as u32) as u64,
                    ConstOp::I32Sub => (a as u32).wrapping_sub(b as u32) as u64,
                    ConstOp::I32Mul => (a as u32).wrapping_mul(b as u32) as u64,
                    ConstOp::I64Add => a.wrapping_add(b),
                    ConstOp::I64Sub => a.wrapping_sub(b),
                    _ => a.wrapping_mul(b),
                }
            }
        };
        st.push(v);
    }
    st.pop().unwrap_or(0)
}

/// Call function `f` of `inst` (from outside wasm, or another instance).
pub fn invoke(it: &mut Interp, inst: &Rc<Instance>, f: u32, args: &[u64]) -> R<Vec<u64>> {
    let m = &inst.module;
    if f < m.n_func_imports {
        let id = inst.func_ids[f as usize];
        return call_id(it, id, args);
    }
    let mut mem = take_mem(&inst.memory);
    let before = mem.len();
    let mut st = STACKS.with(|s| s.borrow_mut().pop()).unwrap_or_else(|| vec![0u64; 4096]);
    let r = run(it, inst, &mut mem, &mut st, f, args);
    STACKS.with(|s| s.borrow_mut().push(st));
    put_mem(it, &inst.memory, mem, before);
    r
}

/// Call a function by store id.
pub fn call_id(it: &mut Interp, id: u64, args: &[u64]) -> R<Vec<u64>> {
    let Some(rec) = func(id) else { return Err(trap(it, "null function")) };
    match &rec.entry {
        FuncEntry::Wasm { inst, idx } => invoke(it, &inst.clone(), *idx, args),
        FuncEntry::Host(f) => {
            let t = canon_type(rec.ty);
            let jsargs: Vec<Value> = t.params.iter().zip(args).map(|(&ty, &v)| to_js(it, ty, v)).collect();
            let r = it.call(f, Value::Undef, &jsargs)?;
            match t.results.len() {
                0 => Ok(Vec::new()),
                1 => Ok(vec![from_js(it, t.results[0], &r)?]),
                n => {
                    let items = it.iterate(&r)?;
                    if items.len() != n {
                        return Err(crate::js::type_error("multi-value result has the wrong length"));
                    }
                    t.results.iter().zip(&items).map(|(&ty, v)| from_js(it, ty, v)).collect()
                }
            }
        }
    }
}

/// A wasm value as JS.
pub fn to_js(it: &Interp, ty: ValType, v: u64) -> Value {
    match ty {
        ValType::I32 => Value::Num(v as u32 as i32 as f64),
        ValType::I64 => Value::big(v as i64 as i128),
        ValType::F32 => Value::Num(f32::from_bits(v as u32) as f64),
        ValType::F64 => Value::Num(f64::from_bits(v)),
        ValType::ExternRef => extern_value(v),
        ValType::FuncRef => match v {
            0 => Value::Null,
            id => super::api::func_object(it, id),
        },
    }
}

/// A JS value as wasm (ToWebAssemblyValue).
pub fn from_js(it: &mut Interp, ty: ValType, v: &Value) -> R<u64> {
    Ok(match ty {
        ValType::I32 => {
            let n = it.to_num(v)?;
            (if n.is_finite() { (n.trunc().rem_euclid(4294967296.0)) as u64 } else { 0 }) as u32 as u64
        }
        ValType::I64 => match v {
            Value::BigInt(b) => b.get() as i64 as u64,
            Value::Bool(b) => *b as u64,
            Value::Str(s) => crate::js::bigint::parse_str(s).ok_or_else(|| crate::js::syntax_error("Cannot convert to BigInt"))? as i64 as u64,
            _ => return Err(crate::js::type_error("Cannot convert a Number to a BigInt (i64 parameter)")),
        },
        ValType::F32 => (it.to_num(v)? as f32).to_bits() as u64,
        ValType::F64 => it.to_num(v)?.to_bits(),
        ValType::ExternRef => extern_id(v),
        ValType::FuncRef => match v {
            Value::Null => 0,
            _ => match super::api::func_id_of(v) {
                Some(id) => id,
                None => return Err(crate::js::type_error("type incompatibility when transforming from/to JS")),
            },
        },
    })
}

struct Frame {
    code: u32,
    pc: u32,
    fp: u32,
}

#[inline]
fn fmin32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}
#[inline]
fn fmax32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}
#[inline]
fn fmin64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}
#[inline]
fn fmax64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}

/// An i32 binary or compare op (for fused instructions).
#[inline]
fn i32_bin(code: u16, a: u32, b: u32) -> Result<u32, &'static str> {
    Ok(match code {
        0x46 => (a == b) as u32,
        0x47 => (a != b) as u32,
        0x48 => ((a as i32) < (b as i32)) as u32,
        0x49 => (a < b) as u32,
        0x4A => ((a as i32) > (b as i32)) as u32,
        0x4B => (a > b) as u32,
        0x4C => ((a as i32) <= (b as i32)) as u32,
        0x4D => (a <= b) as u32,
        0x4E => ((a as i32) >= (b as i32)) as u32,
        0x4F => (a >= b) as u32,
        0x6A => a.wrapping_add(b),
        0x6B => a.wrapping_sub(b),
        0x6C => a.wrapping_mul(b),
        0x6D => {
            if b == 0 {
                return Err("divide by zero");
            }
            if a as i32 == i32::MIN && b as i32 == -1 {
                return Err("divide result unrepresentable");
            }
            ((a as i32) / (b as i32)) as u32
        }
        0x6E => {
            if b == 0 {
                return Err("divide by zero");
            }
            a / b
        }
        0x6F => {
            if b == 0 {
                return Err("remainder by zero");
            }
            (a as i32).wrapping_rem(b as i32) as u32
        }
        0x70 => {
            if b == 0 {
                return Err("remainder by zero");
            }
            a % b
        }
        0x71 => a & b,
        0x72 => a | b,
        0x73 => a ^ b,
        0x74 => a.wrapping_shl(b),
        0x75 => (a as i32).wrapping_shr(b) as u32,
        0x76 => a.wrapping_shr(b),
        0x77 => a.rotate_left(b & 31),
        0x78 => a.rotate_right(b & 31),
        _ => 0,
    })
}

#[inline]
fn load(mem: &[u8], code: u16, addr: u64) -> Option<u64> {
    let a = addr as usize;
    macro_rules! rd {
        ($n:expr) => {{
            let s = mem.get(a..a.checked_add($n)?)?;
            let mut b = [0u8; 8];
            b[..$n].copy_from_slice(s);
            u64::from_le_bytes(b)
        }};
    }
    Some(match code {
        0x28 | 0x2A => rd!(4),
        0x29 | 0x2B => rd!(8),
        0x2C => rd!(1) as u8 as i8 as i32 as u32 as u64,
        0x2D => rd!(1),
        0x2E => rd!(2) as u16 as i16 as i32 as u32 as u64,
        0x2F => rd!(2),
        0x30 => rd!(1) as u8 as i8 as i64 as u64,
        0x31 => rd!(1),
        0x32 => rd!(2) as u16 as i16 as i64 as u64,
        0x33 => rd!(2),
        0x34 => rd!(4) as u32 as i32 as i64 as u64,
        0x35 => rd!(4),
        _ => 0,
    })
}

#[inline]
fn store(mem: &mut [u8], code: u16, addr: u64, v: u64) -> Option<()> {
    let a = addr as usize;
    let n = match code {
        0x36 | 0x38 | 0x3E => 4,
        0x37 | 0x39 => 8,
        0x3A | 0x3C => 1,
        _ => 2,
    };
    let d = mem.get_mut(a..a.checked_add(n)?)?;
    d.copy_from_slice(&v.to_le_bytes()[..n]);
    Some(())
}

fn trunc_check(x: f64, lo: f64, hi: f64) -> Result<(), &'static str> {
    if x.is_nan() {
        Err("float unrepresentable in integer range")
    } else if !(x > lo && x < hi) {
        Err("float unrepresentable in integer range")
    } else {
        Ok(())
    }
}

/// An i64 division or remainder (traps on zero / overflow).
#[inline]
fn i64_bin(code: u16, a: u64, b: u64) -> Result<u64, &'static str> {
    if b == 0 {
        return Err(if code >= 0x81 { "remainder by zero" } else { "divide by zero" });
    }
    Ok(match code {
        0x7F => {
            if a as i64 == i64::MIN && b as i64 == -1 {
                return Err("divide result unrepresentable");
            }
            ((a as i64) / (b as i64)) as u64
        }
        0x80 => a / b,
        0x81 => (a as i64).wrapping_rem(b as i64) as u64,
        _ => a % b,
    })
}

/// The interpreter loop: run defined function `entry` to completion.
///
/// Slots are addressed through `r`, the current frame's base pointer into
/// `st`. The compiler guarantees every slot index an op uses is below the
/// function's `max_height`, and calls make room for the callee's frame
/// before entering it, so the accesses stay in bounds.
fn run(it: &mut Interp, inst: &Rc<Instance>, mem: &mut Vec<u8>, st: &mut Vec<u64>, entry: u32, args: &[u64]) -> R<Vec<u64>> {
    let m: &Module = &inst.module;
    let nimp = m.n_func_imports;
    let mut frames: Vec<Frame> = Vec::new();
    let mut ci = entry - nimp;
    let mut code = &m.codes[ci as usize];
    let mut ops: &[Op] = &code.ops;
    let mut pc: usize = 0;
    let mut fp: usize = 0;
    let need = code.max_height as usize + 16;
    if st.len() < need {
        st.resize(need, 0);
    }
    st[..args.len()].copy_from_slice(args);
    st[args.len()..code.nlocals as usize].fill(0);
    let mut r: *mut u64 = st.as_mut_ptr();
    let mut ticks: u32 = 0;

    macro_rules! R {
        ($i:expr) => {
            *r.add($i as usize)
        };
    }
    macro_rules! trap {
        ($msg:expr) => {
            return Err(trap(it, $msg))
        };
    }
    macro_rules! tr {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(msg) => trap!(msg),
            }
        };
    }
    macro_rules! branch {
        ($i:expr) => {{
            let e = *code.brs.get_unchecked($i as usize);
            if e.keep > 0 && e.src != e.dst {
                std::ptr::copy(r.add(e.src as usize), r.add(e.dst as usize), e.keep as usize);
            }
            let t = e.target as usize;
            if t < pc {
                // A loop: check the time limit now and then.
                ticks = ticks.wrapping_add(1);
                if ticks & 0x3FFFF == 0 && (it.terminating || it.deadline.is_some_and(|d| std::time::Instant::now() > d)) {
                    it.terminating = true;
                    return Err(Value::str(crate::js::TERMINATE));
                }
            }
            pc = t;
        }};
    }
    // Calls that leave this instance hand memory back to JS around them.
    macro_rules! external {
        ($id:expr, $base:expr, $np:expr) => {{
            let base = $base as usize;
            let args: Vec<u64> = std::slice::from_raw_parts(r.add(base), $np).to_vec();
            let before = mem.len();
            put_mem(it, &inst.memory, std::mem::take(mem), before);
            let res = call_id(it, $id, &args);
            *mem = take_mem(&inst.memory);
            let res = res?;
            if fp + base + res.len() + 16 > st.len() {
                st.resize(fp + base + res.len() + 16, 0);
                r = st.as_mut_ptr().add(fp);
            }
            for (i, v) in res.into_iter().enumerate() {
                R!(base + i) = v;
            }
        }};
    }
    macro_rules! call_local {
        ($f:expr, $base:expr) => {{
            let f: u32 = $f;
            let callee = &m.codes[(f - nimp) as usize];
            let new_fp = fp + $base as usize;
            let need = new_fp + callee.max_height as usize + 16;
            if need > st.len() {
                if need > 64 << 20 {
                    return Err(crate::js::range_error("Maximum call stack size exceeded"));
                }
                st.resize(need.max(st.len() * 2), 0);
            }
            frames.push(Frame { code: ci, pc: pc as u32, fp: fp as u32 });
            if frames.len() > 50_000 {
                return Err(crate::js::range_error("Maximum call stack size exceeded"));
            }
            st[new_fp + callee.nparams as usize..new_fp + callee.nlocals as usize].fill(0);
            ci = f - nimp;
            code = callee;
            ops = &code.ops;
            pc = 0;
            fp = new_fp;
            r = st.as_mut_ptr().add(fp);
        }};
    }

    loop {
        let op = unsafe { *ops.get_unchecked(pc) };
        pc += 1;
        unsafe {
            match op.code {
            0x00 => trap!("unreachable"),
            0x01 => {}
            0x04 => {
                if R!(op.b) as u32 == 0 {
                    pc = op.a as usize;
                }
            }
            OP_JUMP => pc = op.a as usize,
            OP_COPY => R!(op.a) = R!(op.b),
            OP_CONST => R!(op.a) = op.b as u64 | (op.c as u64) << 32,
            0x0C => branch!(op.a),
            0x0D => {
                if R!(op.b) as u32 != 0 {
                    branch!(op.a);
                }
            }
            OP_BR_EQZ => {
                if R!(op.b) as u32 == 0 {
                    branch!(op.a);
                }
            }
            0x0E => {
                let i = (R!(op.a) as u32).min(op.c);
                branch!(op.b + i);
            }
            0x0F => {
                let k = op.k as usize;
                if k > 0 && op.a != 0 {
                    std::ptr::copy(r.add(op.a as usize), r, k);
                }
                match frames.pop() {
                    Some(f) => {
                        ci = f.code;
                        code = &m.codes[ci as usize];
                        ops = &code.ops;
                        pc = f.pc as usize;
                        fp = f.fp as usize;
                        r = st.as_mut_ptr().add(fp);
                    }
                    None => return Ok(st[..k].to_vec()),
                }
            }
            0x10 => {
                if op.a < nimp {
                    let np = m.func_type(op.a).params.len();
                    external!(inst.func_ids[op.a as usize], op.b, np);
                } else {
                    call_local!(op.a, op.b);
                }
            }
            0x11 => {
                let np = m.types[op.a as usize].params.len();
                let i = R!(op.b as usize + np) as u32 as usize;
                let id = match inst.tables.get(op.c as usize).and_then(|t| t.borrow().elems.get(i).copied()) {
                    Some(id) => id,
                    None => trap!("table index is out of bounds"),
                };
                let Some(rec) = func(id) else { trap!("null function or function signature mismatch") };
                if rec.ty != inst.type_canon[op.a as usize] {
                    trap!("null function or function signature mismatch");
                }
                match &rec.entry {
                    FuncEntry::Wasm { inst: other, idx } if Rc::ptr_eq(other, inst) => call_local!(*idx, op.b),
                    _ => external!(id, op.b, np),
                }
            }
            0x1B => {
                let a = op.a as usize;
                if R!(a + 2) as u32 == 0 {
                    R!(a) = R!(a + 1);
                }
            }
            0x23 => R!(op.a) = inst.globals.get_unchecked(op.b as usize).v.get(),
            0x24 => inst.globals.get_unchecked(op.a as usize).v.set(R!(op.b)),
            0x25 => {
                let i = R!(op.b) as u32 as usize;
                let v = inst.tables[op.c as usize].borrow().elems.get(i).copied();
                match v {
                    Some(v) => R!(op.a) = v,
                    None => trap!("table index is out of bounds"),
                }
            }
            0x26 => {
                let i = R!(op.b) as u32 as usize;
                let v = R!(op.c);
                let ok = match inst.tables[op.a as usize].borrow_mut().elems.get_mut(i) {
                    Some(e) => {
                        *e = v;
                        true
                    }
                    None => false,
                };
                if !ok {
                    trap!("table index is out of bounds");
                }
            }
            0x28 | 0x2A => {
                let a = (R!(op.b) as u32 as usize).wrapping_add(op.c as usize);
                match mem.get(a..a.wrapping_add(4)) {
                    Some(s) if a <= usize::MAX - 4 => R!(op.a) = u32::from_le_bytes(s.try_into().unwrap()) as u64,
                    _ => trap!("memory access out of bounds"),
                }
            }
            0x29 | 0x2B => {
                let a = (R!(op.b) as u32 as usize).wrapping_add(op.c as usize);
                match mem.get(a..a.wrapping_add(8)) {
                    Some(s) if a <= usize::MAX - 8 => R!(op.a) = u64::from_le_bytes(s.try_into().unwrap()),
                    _ => trap!("memory access out of bounds"),
                }
            }
            0x2C..=0x35 => {
                let addr = R!(op.b) as u32 as u64 + op.c as u64;
                match load(mem, op.code, addr) {
                    Some(v) => R!(op.a) = v,
                    None => trap!("memory access out of bounds"),
                }
            }
            0x36 | 0x38 => {
                let a = (R!(op.a) as u32 as usize).wrapping_add(op.c as usize);
                match mem.get_mut(a..a.wrapping_add(4)) {
                    Some(d) if a <= usize::MAX - 4 => d.copy_from_slice(&(R!(op.b) as u32).to_le_bytes()),
                    _ => trap!("memory access out of bounds"),
                }
            }
            0x37 | 0x39 => {
                let a = (R!(op.a) as u32 as usize).wrapping_add(op.c as usize);
                match mem.get_mut(a..a.wrapping_add(8)) {
                    Some(d) if a <= usize::MAX - 8 => d.copy_from_slice(&R!(op.b).to_le_bytes()),
                    _ => trap!("memory access out of bounds"),
                }
            }
            0x3A..=0x3E => {
                let addr = R!(op.a) as u32 as u64 + op.c as u64;
                if store(mem, op.code, addr, R!(op.b)).is_none() {
                    trap!("memory access out of bounds");
                }
            }
            0x3F => R!(op.a) = (mem.len() / PAGE) as u64,
            0x40 => {
                let delta = R!(op.b) as u32;
                let old = (mem.len() / PAGE) as u32;
                let max = inst.memory.as_ref().and_then(|m| m.borrow().max).unwrap_or(MAX_PAGES).min(MAX_PAGES);
                R!(op.a) = match old.checked_add(delta).filter(|&n| n <= max) {
                    Some(n) => {
                        mem.resize(n as usize * PAGE, 0);
                        old as u64
                    }
                    None => u32::MAX as u64,
                };
            }
            0xD2 => R!(op.a) = inst.func_ids[op.b as usize],
            0x108 => {
                let a = op.a as usize;
                let (d, s, n) = (R!(a) as u32 as usize, R!(a + 1) as u32 as usize, R!(a + 2) as u32 as usize);
                let dropped = inst.dropped_data.borrow()[op.b as usize];
                let seg: &[u8] = if dropped { &[] } else { &m.datas[op.b as usize].bytes };
                if s + n > seg.len() || d + n > mem.len() {
                    trap!("memory access out of bounds");
                }
                mem[d..d + n].copy_from_slice(&seg[s..s + n]);
            }
            0x109 => inst.dropped_data.borrow_mut()[op.b as usize] = true,
            0x10A => {
                let a = op.a as usize;
                let (d, s, n) = (R!(a) as u32 as usize, R!(a + 1) as u32 as usize, R!(a + 2) as u32 as usize);
                if s + n > mem.len() || d + n > mem.len() {
                    trap!("memory access out of bounds");
                }
                mem.copy_within(s..s + n, d);
            }
            0x10B => {
                let a = op.a as usize;
                let (d, v, n) = (R!(a) as u32 as usize, R!(a + 1) as u8, R!(a + 2) as u32 as usize);
                if d + n > mem.len() {
                    trap!("memory access out of bounds");
                }
                mem[d..d + n].fill(v);
            }
            0x10C => {
                let a = op.a as usize;
                let (d, s, n) = (R!(a) as u32 as usize, R!(a + 1) as u32 as usize, R!(a + 2) as u32 as usize);
                let ok = {
                    let ev = inst.elem_vals.borrow();
                    let seg = &ev[op.b as usize];
                    let mut t = inst.tables[op.c as usize].borrow_mut();
                    if s + n > seg.len() || d + n > t.elems.len() {
                        false
                    } else {
                        t.elems[d..d + n].copy_from_slice(&seg[s..s + n]);
                        true
                    }
                };
                if !ok {
                    trap!("table index is out of bounds");
                }
            }
            0x10D => inst.elem_vals.borrow_mut()[op.b as usize] = Vec::new(),
            0x10E => {
                let a = op.a as usize;
                let (d, s, n) = (R!(a) as u32 as usize, R!(a + 1) as u32 as usize, R!(a + 2) as u32 as usize);
                let src: Option<Vec<u64>> = inst.tables[op.c as usize].borrow().elems.get(s..s + n).map(|x| x.to_vec());
                let ok = match src {
                    Some(src) => match inst.tables[op.b as usize].borrow_mut().elems.get_mut(d..d + n) {
                        Some(dst) => {
                            dst.copy_from_slice(&src);
                            true
                        }
                        None => false,
                    },
                    None => false,
                };
                if !ok {
                    trap!("table index is out of bounds");
                }
            }
            0x10F => {
                let a = op.a as usize;
                let (v, n) = (R!(a), R!(a + 1) as u32);
                let mut t = inst.tables[op.b as usize].borrow_mut();
                let old = t.elems.len() as u32;
                let max = t.max.unwrap_or(10_000_000).min(10_000_000);
                R!(a) = match old.checked_add(n).filter(|&x| x <= max) {
                    Some(x) => {
                        t.elems.resize(x as usize, v);
                        old as u64
                    }
                    None => u32::MAX as u64,
                };
            }
            0x110 => R!(op.a) = inst.tables[op.b as usize].borrow().elems.len() as u64,
            0x111 => {
                let a = op.a as usize;
                let (d, v, n) = (R!(a) as u32 as usize, R!(a + 1), R!(a + 2) as u32 as usize);
                let ok = match inst.tables[op.b as usize].borrow_mut().elems.get_mut(d..d.saturating_add(n)) {
                    Some(s) => {
                        s.fill(v);
                        true
                    }
                    None => false,
                };
                if !ok {
                    trap!("table index is out of bounds");
                }
            }
            0x46 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a == b) as u32) as u64;
            }
            0x346 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a == b) as u32) as u64;
            }
            0x47 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a != b) as u32) as u64;
            }
            0x347 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a != b) as u32) as u64;
            }
            0x48 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (((a as i32) < (b as i32)) as u32) as u64;
            }
            0x348 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (((a as i32) < (b as i32)) as u32) as u64;
            }
            0x49 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a < b) as u32) as u64;
            }
            0x349 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a < b) as u32) as u64;
            }
            0x4A => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (((a as i32) > (b as i32)) as u32) as u64;
            }
            0x34A => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (((a as i32) > (b as i32)) as u32) as u64;
            }
            0x4B => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a > b) as u32) as u64;
            }
            0x34B => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a > b) as u32) as u64;
            }
            0x4C => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (((a as i32) <= (b as i32)) as u32) as u64;
            }
            0x34C => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (((a as i32) <= (b as i32)) as u32) as u64;
            }
            0x4D => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a <= b) as u32) as u64;
            }
            0x34D => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a <= b) as u32) as u64;
            }
            0x4E => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (((a as i32) >= (b as i32)) as u32) as u64;
            }
            0x34E => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (((a as i32) >= (b as i32)) as u32) as u64;
            }
            0x4F => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a >= b) as u32) as u64;
            }
            0x34F => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a >= b) as u32) as u64;
            }
            0x6A => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.wrapping_add(b)) as u64;
            }
            0x36A => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.wrapping_add(b)) as u64;
            }
            0x6B => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.wrapping_sub(b)) as u64;
            }
            0x36B => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.wrapping_sub(b)) as u64;
            }
            0x6C => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.wrapping_mul(b)) as u64;
            }
            0x36C => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.wrapping_mul(b)) as u64;
            }
            0x6D => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = tr!(i32_bin(0x6D, a, b)) as u64;
            }
            0x36D => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = tr!(i32_bin(0x6D, a, b)) as u64;
            }
            0x6E => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = tr!(i32_bin(0x6E, a, b)) as u64;
            }
            0x36E => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = tr!(i32_bin(0x6E, a, b)) as u64;
            }
            0x6F => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = tr!(i32_bin(0x6F, a, b)) as u64;
            }
            0x36F => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = tr!(i32_bin(0x6F, a, b)) as u64;
            }
            0x70 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = tr!(i32_bin(0x70, a, b)) as u64;
            }
            0x370 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = tr!(i32_bin(0x70, a, b)) as u64;
            }
            0x71 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a & b) as u64;
            }
            0x371 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a & b) as u64;
            }
            0x72 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a | b) as u64;
            }
            0x372 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a | b) as u64;
            }
            0x73 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a ^ b) as u64;
            }
            0x373 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a ^ b) as u64;
            }
            0x74 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.wrapping_shl(b)) as u64;
            }
            0x374 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.wrapping_shl(b)) as u64;
            }
            0x75 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = ((a as i32).wrapping_shr(b) as u32) as u64;
            }
            0x375 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = ((a as i32).wrapping_shr(b) as u32) as u64;
            }
            0x76 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.wrapping_shr(b)) as u64;
            }
            0x376 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.wrapping_shr(b)) as u64;
            }
            0x77 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.rotate_left(b & 31)) as u64;
            }
            0x377 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.rotate_left(b & 31)) as u64;
            }
            0x78 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                R!(op.a) = (a.rotate_right(b & 31)) as u64;
            }
            0x378 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                R!(op.a) = (a.rotate_right(b & 31)) as u64;
            }
            0x51 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a == b) as u64) as u64;
            }
            0x351 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a == b) as u64) as u64;
            }
            0x52 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a != b) as u64) as u64;
            }
            0x352 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a != b) as u64) as u64;
            }
            0x53 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (((a as i64) < (b as i64)) as u64) as u64;
            }
            0x353 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (((a as i64) < (b as i64)) as u64) as u64;
            }
            0x54 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a < b) as u64) as u64;
            }
            0x354 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a < b) as u64) as u64;
            }
            0x55 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (((a as i64) > (b as i64)) as u64) as u64;
            }
            0x355 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (((a as i64) > (b as i64)) as u64) as u64;
            }
            0x56 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a > b) as u64) as u64;
            }
            0x356 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a > b) as u64) as u64;
            }
            0x57 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (((a as i64) <= (b as i64)) as u64) as u64;
            }
            0x357 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (((a as i64) <= (b as i64)) as u64) as u64;
            }
            0x58 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a <= b) as u64) as u64;
            }
            0x358 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a <= b) as u64) as u64;
            }
            0x59 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (((a as i64) >= (b as i64)) as u64) as u64;
            }
            0x359 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (((a as i64) >= (b as i64)) as u64) as u64;
            }
            0x5A => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a >= b) as u64) as u64;
            }
            0x35A => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a >= b) as u64) as u64;
            }
            0x7C => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.wrapping_add(b)) as u64;
            }
            0x37C => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.wrapping_add(b)) as u64;
            }
            0x7D => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.wrapping_sub(b)) as u64;
            }
            0x37D => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.wrapping_sub(b)) as u64;
            }
            0x7E => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.wrapping_mul(b)) as u64;
            }
            0x37E => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.wrapping_mul(b)) as u64;
            }
            0x7F => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = tr!(i64_bin(0x7F, a, b));
            }
            0x37F => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = tr!(i64_bin(0x7F, a, b));
            }
            0x80 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = tr!(i64_bin(0x80, a, b));
            }
            0x380 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = tr!(i64_bin(0x80, a, b));
            }
            0x81 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = tr!(i64_bin(0x81, a, b));
            }
            0x381 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = tr!(i64_bin(0x81, a, b));
            }
            0x82 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = tr!(i64_bin(0x82, a, b));
            }
            0x382 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = tr!(i64_bin(0x82, a, b));
            }
            0x83 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a & b) as u64;
            }
            0x383 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a & b) as u64;
            }
            0x84 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a | b) as u64;
            }
            0x384 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a | b) as u64;
            }
            0x85 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a ^ b) as u64;
            }
            0x385 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a ^ b) as u64;
            }
            0x86 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.wrapping_shl(b as u32)) as u64;
            }
            0x386 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.wrapping_shl(b as u32)) as u64;
            }
            0x87 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = ((a as i64).wrapping_shr(b as u32) as u64) as u64;
            }
            0x387 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = ((a as i64).wrapping_shr(b as u32) as u64) as u64;
            }
            0x88 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.wrapping_shr(b as u32)) as u64;
            }
            0x388 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.wrapping_shr(b as u32)) as u64;
            }
            0x89 => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.rotate_left((b & 63) as u32)) as u64;
            }
            0x389 => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.rotate_left((b & 63) as u32)) as u64;
            }
            0x8A => {
                let a = R!(op.b);
                let b = R!(op.c);
                R!(op.a) = (a.rotate_right((b & 63) as u32)) as u64;
            }
            0x38A => {
                let a = R!(op.b);
                let b = op.c as i32 as i64 as u64;
                R!(op.a) = (a.rotate_right((b & 63) as u32)) as u64;
            }
            0x92 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a + b).to_bits() as u64;
            }
            0x93 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a - b).to_bits() as u64;
            }
            0x94 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a * b).to_bits() as u64;
            }
            0x95 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a / b).to_bits() as u64;
            }
            0x96 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (fmin32(a, b)).to_bits() as u64;
            }
            0x97 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (fmax32(a, b)).to_bits() as u64;
            }
            0x98 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a.copysign(b)).to_bits() as u64;
            }
            0xA0 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a + b).to_bits();
            }
            0xA1 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a - b).to_bits();
            }
            0xA2 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a * b).to_bits();
            }
            0xA3 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a / b).to_bits();
            }
            0xA4 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (fmin64(a, b)).to_bits();
            }
            0xA5 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (fmax64(a, b)).to_bits();
            }
            0xA6 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a.copysign(b)).to_bits();
            }
            0x5B => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a == b) as u64;
            }
            0x5C => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a != b) as u64;
            }
            0x5D => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a < b) as u64;
            }
            0x5E => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a > b) as u64;
            }
            0x5F => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a <= b) as u64;
            }
            0x60 => {
                let a = f32::from_bits(R!(op.b) as u32);
                let b = f32::from_bits(R!(op.c) as u32);
                R!(op.a) = (a >= b) as u64;
            }
            0x61 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a == b) as u64;
            }
            0x62 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a != b) as u64;
            }
            0x63 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a < b) as u64;
            }
            0x64 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a > b) as u64;
            }
            0x65 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a <= b) as u64;
            }
            0x66 => {
                let a = f64::from_bits(R!(op.b));
                let b = f64::from_bits(R!(op.c));
                R!(op.a) = (a >= b) as u64;
            }
            0x45 => {
                let x = R!(op.b);
                R!(op.a) = ((x as u32) == 0) as u64;
            }
            0x50 => {
                let x = R!(op.b);
                R!(op.a) = (x == 0) as u64;
            }
            0x67 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32).leading_zeros() as u64;
            }
            0x68 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32).trailing_zeros() as u64;
            }
            0x69 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32).count_ones() as u64;
            }
            0x79 => {
                let x = R!(op.b);
                R!(op.a) = x.leading_zeros() as u64;
            }
            0x7A => {
                let x = R!(op.b);
                R!(op.a) = x.trailing_zeros() as u64;
            }
            0x7B => {
                let x = R!(op.b);
                R!(op.a) = x.count_ones() as u64;
            }
            0x8B => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32).abs().to_bits() as u64;
            }
            0x8C => {
                let x = R!(op.b);
                R!(op.a) = ((x as u32) ^ 0x8000_0000) as u64;
            }
            0x8D => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32).ceil().to_bits() as u64;
            }
            0x8E => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32).floor().to_bits() as u64;
            }
            0x8F => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32).trunc().to_bits() as u64;
            }
            0x90 => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32).round_ties_even().to_bits() as u64;
            }
            0x91 => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32).sqrt().to_bits() as u64;
            }
            0x99 => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x).abs().to_bits();
            }
            0x9A => {
                let x = R!(op.b);
                R!(op.a) = x ^ (1u64 << 63);
            }
            0x9B => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x).ceil().to_bits();
            }
            0x9C => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x).floor().to_bits();
            }
            0x9D => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x).trunc().to_bits();
            }
            0x9E => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x).round_ties_even().to_bits();
            }
            0x9F => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x).sqrt().to_bits();
            }
            0xA7 => {
                let x = R!(op.b);
                R!(op.a) = x as u32 as u64;
            }
            0xAC => {
                let x = R!(op.b);
                R!(op.a) = x as u32 as i32 as i64 as u64;
            }
            0xAD => {
                let x = R!(op.b);
                R!(op.a) = x as u32 as u64;
            }
            0xB2 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32 as i32 as f32).to_bits() as u64;
            }
            0xB3 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32 as f32).to_bits() as u64;
            }
            0xB4 => {
                let x = R!(op.b);
                R!(op.a) = (x as i64 as f32).to_bits() as u64;
            }
            0xB5 => {
                let x = R!(op.b);
                R!(op.a) = (x as f32).to_bits() as u64;
            }
            0xB6 => {
                let x = R!(op.b);
                R!(op.a) = (f64::from_bits(x) as f32).to_bits() as u64;
            }
            0xB7 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32 as i32 as f64).to_bits();
            }
            0xB8 => {
                let x = R!(op.b);
                R!(op.a) = (x as u32 as f64).to_bits();
            }
            0xB9 => {
                let x = R!(op.b);
                R!(op.a) = (x as i64 as f64).to_bits();
            }
            0xBA => {
                let x = R!(op.b);
                R!(op.a) = (x as f64).to_bits();
            }
            0xBB => {
                let x = R!(op.b);
                R!(op.a) = (f32::from_bits(x as u32) as f64).to_bits();
            }
            0xBC => {
                let x = R!(op.b);
                R!(op.a) = x;
            }
            0xBD => {
                let x = R!(op.b);
                R!(op.a) = x;
            }
            0xBE => {
                let x = R!(op.b);
                R!(op.a) = x;
            }
            0xBF => {
                let x = R!(op.b);
                R!(op.a) = x;
            }
            0xC0 => {
                let x = R!(op.b);
                R!(op.a) = x as u8 as i8 as i32 as u32 as u64;
            }
            0xC1 => {
                let x = R!(op.b);
                R!(op.a) = x as u16 as i16 as i32 as u32 as u64;
            }
            0xC2 => {
                let x = R!(op.b);
                R!(op.a) = x as u8 as i8 as i64 as u64;
            }
            0xC3 => {
                let x = R!(op.b);
                R!(op.a) = x as u16 as i16 as i64 as u64;
            }
            0xC4 => {
                let x = R!(op.b);
                R!(op.a) = x as u32 as i32 as i64 as u64;
            }
            0x100 => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32) as i32 as u32 as u64;
            }
            0x101 => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32) as u32 as u64;
            }
            0x102 => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x) as i32 as u32 as u64;
            }
            0x103 => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x) as u32 as u64;
            }
            0x104 => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32) as i64 as u64;
            }
            0x105 => {
                let x = R!(op.b);
                R!(op.a) = f32::from_bits(x as u32) as u64;
            }
            0x106 => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x) as i64 as u64;
            }
            0x107 => {
                let x = R!(op.b);
                R!(op.a) = f64::from_bits(x) as u64;
            }
            0xA8 => {
                let x = f32::from_bits(R!(op.b) as u32) as f64;
                tr!(trunc_check(x, -2147483649.0, 2147483648.0));
                R!(op.a) = x as i32 as u32 as u64;
            }
            0xA9 => {
                let x = f32::from_bits(R!(op.b) as u32) as f64;
                tr!(trunc_check(x, -1.0, 4294967296.0));
                R!(op.a) = x as u32 as u64;
            }
            0xAA => {
                let x = f64::from_bits(R!(op.b));
                tr!(trunc_check(x, -2147483649.0, 2147483648.0));
                R!(op.a) = x as i32 as u32 as u64;
            }
            0xAB => {
                let x = f64::from_bits(R!(op.b));
                tr!(trunc_check(x, -1.0, 4294967296.0));
                R!(op.a) = x as u32 as u64;
            }
            0xAE => {
                let x = f32::from_bits(R!(op.b) as u32) as f64;
                tr!(trunc_check(x, -9223372036854777856.0, 9223372036854775808.0));
                R!(op.a) = x as i64 as u64;
            }
            0xAF => {
                let x = f32::from_bits(R!(op.b) as u32) as f64;
                tr!(trunc_check(x, -1.0, 18446744073709551616.0));
                R!(op.a) = x as u64;
            }
            0xB0 => {
                let x = f64::from_bits(R!(op.b));
                tr!(trunc_check(x, -9223372036854777856.0, 9223372036854775808.0));
                R!(op.a) = x as i64 as u64;
            }
            0xB1 => {
                let x = f64::from_bits(R!(op.b));
                tr!(trunc_check(x, -1.0, 18446744073709551616.0));
                R!(op.a) = x as u64;
            }
            0x400 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if a == b {
                    branch!(op.a);
                }
            }
            0x410 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if a == b {
                    branch!(op.a);
                }
            }
            0x420 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !(a == b) {
                    pc = op.a as usize;
                }
            }
            0x430 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !(a == b) {
                    pc = op.a as usize;
                }
            }
            0x401 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if a != b {
                    branch!(op.a);
                }
            }
            0x411 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if a != b {
                    branch!(op.a);
                }
            }
            0x421 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !(a != b) {
                    pc = op.a as usize;
                }
            }
            0x431 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !(a != b) {
                    pc = op.a as usize;
                }
            }
            0x402 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if (a as i32) < (b as i32) {
                    branch!(op.a);
                }
            }
            0x412 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if (a as i32) < (b as i32) {
                    branch!(op.a);
                }
            }
            0x422 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !((a as i32) < (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x432 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !((a as i32) < (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x403 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if a < b {
                    branch!(op.a);
                }
            }
            0x413 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if a < b {
                    branch!(op.a);
                }
            }
            0x423 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !(a < b) {
                    pc = op.a as usize;
                }
            }
            0x433 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !(a < b) {
                    pc = op.a as usize;
                }
            }
            0x404 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if (a as i32) > (b as i32) {
                    branch!(op.a);
                }
            }
            0x414 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if (a as i32) > (b as i32) {
                    branch!(op.a);
                }
            }
            0x424 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !((a as i32) > (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x434 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !((a as i32) > (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x405 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if a > b {
                    branch!(op.a);
                }
            }
            0x415 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if a > b {
                    branch!(op.a);
                }
            }
            0x425 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !(a > b) {
                    pc = op.a as usize;
                }
            }
            0x435 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !(a > b) {
                    pc = op.a as usize;
                }
            }
            0x406 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if (a as i32) <= (b as i32) {
                    branch!(op.a);
                }
            }
            0x416 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if (a as i32) <= (b as i32) {
                    branch!(op.a);
                }
            }
            0x426 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !((a as i32) <= (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x436 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !((a as i32) <= (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x407 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if a <= b {
                    branch!(op.a);
                }
            }
            0x417 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if a <= b {
                    branch!(op.a);
                }
            }
            0x427 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !(a <= b) {
                    pc = op.a as usize;
                }
            }
            0x437 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !(a <= b) {
                    pc = op.a as usize;
                }
            }
            0x408 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if (a as i32) >= (b as i32) {
                    branch!(op.a);
                }
            }
            0x418 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if (a as i32) >= (b as i32) {
                    branch!(op.a);
                }
            }
            0x428 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !((a as i32) >= (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x438 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !((a as i32) >= (b as i32)) {
                    pc = op.a as usize;
                }
            }
            0x409 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if a >= b {
                    branch!(op.a);
                }
            }
            0x419 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if a >= b {
                    branch!(op.a);
                }
            }
            0x429 => {
                let a = R!(op.b) as u32;
                let b = R!(op.c) as u32;
                if !(a >= b) {
                    pc = op.a as usize;
                }
            }
            0x439 => {
                let a = R!(op.b) as u32;
                let b = op.c;
                if !(a >= b) {
                    pc = op.a as usize;
                }
            }
            c => trap!(&format!("unsupported instruction 0x{c:x}")),
            }
        }
    }
}

/// A resolved import (or export).
#[derive(Clone)]
pub enum ExternVal {
    Func(u64),
    Table(Rc<RefCell<Table>>),
    Memory(Rc<RefCell<Memory>>),
    Global(Rc<Global>),
    Tag,
}

pub enum InstError {
    Link(String),
    Throw(Value),
}

pub fn new_memory(it: &Interp, l: Limits) -> Rc<RefCell<Memory>> {
    let pages = l.min.min(MAX_PAGES) as usize;
    Rc::new(RefCell::new(Memory { obj: binary::new_buffer(it, vec![0u8; pages * PAGE]), max: l.max, len: pages * PAGE }))
}

pub fn memory_len(m: &Rc<RefCell<Memory>>) -> usize {
    m.borrow().len
}

/// Instantiate a module with resolved imports (one per module import).
pub fn instantiate(it: &mut Interp, module: Rc<Module>, imports: Vec<ExternVal>) -> Result<Rc<Instance>, InstError> {
    let m = &*module;
    let link = |s: String| InstError::Link(s);
    let mut func_ids = Vec::with_capacity(m.func_types.len());
    let mut tables = Vec::new();
    let mut memory = None;
    let mut globals = Vec::new();
    for (imp, v) in m.imports.iter().zip(imports) {
        let what = format!("Import #{} \"{}\" \"{}\"", func_ids.len() + tables.len() + globals.len(), imp.module, imp.name);
        match (&imp.desc, v) {
            (ImportDesc::Func(t), ExternVal::Func(id)) => {
                let want = canon(&m.types[*t as usize]);
                if func(id).map(|r| r.ty) != Some(want) {
                    return Err(link(format!("{what}: imported function does not match the expected type")));
                }
                func_ids.push(id);
            }
            (ImportDesc::Table(tt), ExternVal::Table(t)) => {
                {
                    let tb = t.borrow();
                    if tb.ty != tt.elem || (tb.elems.len() as u32) < tt.limits.min {
                        return Err(link(format!("{what}: table import has the wrong type or is too small")));
                    }
                    if let Some(max) = tt.limits.max {
                        if tb.max.is_none_or(|x| x > max) {
                            return Err(link(format!("{what}: table import has a larger maximum size than expected")));
                        }
                    }
                }
                tables.push(t);
            }
            (ImportDesc::Memory(l), ExternVal::Memory(mm)) => {
                let pages = (memory_len(&mm) / PAGE) as u32;
                if pages < l.min {
                    return Err(link(format!("{what}: memory import is smaller than the declared initial size")));
                }
                if let Some(max) = l.max {
                    if mm.borrow().max.is_none_or(|x| x > max) {
                        return Err(link(format!("{what}: memory import has a larger maximum size than declared")));
                    }
                }
                memory = Some(mm);
            }
            (ImportDesc::Global(ty, mutable), ExternVal::Global(g)) => {
                if g.ty != *ty || g.mutable != *mutable {
                    return Err(link(format!("{what}: imported global does not match the expected type")));
                }
                globals.push(g);
            }
            (ImportDesc::Tag, _) => {}
            (desc, _) => {
                let kind = match desc {
                    ImportDesc::Func(_) => "function import requires a callable",
                    ImportDesc::Table(_) => "table import requires a WebAssembly.Table",
                    ImportDesc::Memory(_) => "memory import must be a WebAssembly.Memory object",
                    _ => "global import must be a number, valid Wasm reference, or WebAssembly.Global object",
                };
                return Err(link(format!("{what}: {kind}")));
            }
        }
    }
    let base = next_func_id();
    for i in 0..m.codes.len() as u64 {
        func_ids.push(base + i);
    }
    for g in &m.globals {
        let v = eval_const(&g.init, &globals, &func_ids);
        globals.push(Rc::new(Global { v: Cell::new(v), ty: g.ty, mutable: g.mutable }));
    }
    for t in &m.tables[m.n_table_imports as usize..] {
        tables.push(Rc::new(RefCell::new(Table { elems: vec![0; t.limits.min.min(10_000_000) as usize], max: t.limits.max, ty: t.elem })));
    }
    if memory.is_none() {
        if let Some(l) = m.memories.first() {
            memory = Some(new_memory(it, *l));
        }
    }
    let elem_vals: Vec<Vec<u64>> = m.elems.iter().map(|e| e.items.iter().map(|x| eval_const(x, &globals, &func_ids)).collect()).collect();
    let type_canon = m.types.iter().map(canon).collect();
    let inst = Rc::new(Instance {
        module: module.clone(),
        func_ids,
        type_canon,
        tables,
        memory,
        globals,
        dropped_data: RefCell::new(vec![false; m.datas.len()]),
        elem_vals: RefCell::new(elem_vals),
    });
    for i in 0..m.codes.len() as u32 {
        let idx = m.n_func_imports + i;
        let ty = inst.type_canon[m.func_types[idx as usize] as usize];
        add_func(FuncRec { entry: FuncEntry::Wasm { inst: inst.clone(), idx }, ty });
    }
    // Active element segments, then data segments.
    for (i, e) in m.elems.iter().enumerate() {
        match &e.mode {
            SegMode::Active { index, offset } => {
                let off = eval_const(offset, &inst.globals, &inst.func_ids) as u32 as usize;
                let vals = std::mem::take(&mut inst.elem_vals.borrow_mut()[i]);
                let Some(t) = inst.tables.get(*index as usize) else { return Err(link("unknown table".into())) };
                let mut t = t.borrow_mut();
                if off + vals.len() > t.elems.len() {
                    drop(t);
                    return Err(InstError::Throw(trap(it, "table index is out of bounds")));
                }
                t.elems[off..off + vals.len()].copy_from_slice(&vals);
            }
            SegMode::Declarative => inst.elem_vals.borrow_mut()[i] = Vec::new(),
            SegMode::Passive => {}
        }
    }
    for (i, d) in m.datas.iter().enumerate() {
        if let SegMode::Active { offset, .. } = &d.mode {
            let off = eval_const(offset, &inst.globals, &inst.func_ids) as u32 as usize;
            let mut mem = take_mem(&inst.memory);
            let before = mem.len();
            let ok = off + d.bytes.len() <= mem.len();
            if ok {
                mem[off..off + d.bytes.len()].copy_from_slice(&d.bytes);
            }
            put_mem(it, &inst.memory, mem, before);
            if !ok {
                return Err(InstError::Throw(trap(it, "data segment is out of bounds")));
            }
            inst.dropped_data.borrow_mut()[i] = true;
        }
    }
    if let Some(s) = m.start {
        invoke(it, &inst, s, &[]).map_err(InstError::Throw)?;
    }
    Ok(inst)
}
