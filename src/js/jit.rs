//! Baseline JIT: compiles hot bytecode functions to x86-64 machine code.
//!
//! Each bytecode op becomes a block of machine code. Number arithmetic,
//! comparisons, branches, moves of primitive values and loop back edges run
//! inline (guarded by type-tag checks); everything else, and every guard
//! failure, calls `jit_step`, which executes that one op in the VM and returns
//! the next op index. Native code can be entered at any op through a jump
//! table, so the VM switches a running loop over to it (on-stack
//! replacement) and exception handlers re-enter it at their target.
//!
//! The machine code reads and writes `Value`s in place. Their layout isn't
//! fixed by Rust, so it is probed at startup (`Layout`); the JIT stays off if
//! it isn't the expected tag-byte + payload form.

#![allow(clippy::identity_op)]

use super::vm::{BinOp, Code, Op};
use super::*;

// ------------------------------------------------------------------ layout

/// Where a `Value`'s type tag and payloads are.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    tag: i32,
    num: i32,
    boolean: i32,
    t_undef: u8,
    t_null: u8,
    t_bool: u8,
    t_num: u8,
    /// Tags of variants owning heap memory (writes over them must drop).
    heap: [u8; 2],
}

const VSIZE: i32 = std::mem::size_of::<Value>() as i32;

fn bytes_of(v: Value) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut m = std::mem::MaybeUninit::<Value>::zeroed();
    m.write(v);
    let p = m.as_ptr() as *const u8;
    let n = std::mem::size_of::<Value>().min(32);
    // SAFETY: the buffer started zeroed (padding stays zero).
    unsafe { std::ptr::copy_nonoverlapping(p, out.as_mut_ptr(), n) };
    unsafe { m.assume_init_drop() };
    out
}

fn probe() -> Option<Layout> {
    if VSIZE != 24 {
        return None;
    }
    // The Num payload: where a number's bits are (padding isn't reliable).
    let x = f64::from_bits(0x0123_4567_89AB_CDEF);
    let y = f64::from_bits(0x7EDC_BA98_7654_3211);
    let num = (0..=16).step_by(8).find(|&o| {
        bytes_of(Value::Num(x))[o..o + 8] == x.to_bits().to_le_bytes() && bytes_of(Value::Num(y))[o..o + 8] == y.to_bits().to_le_bytes()
    })?;
    // Read a value laid out in a zeroed buffer.
    fn read(buf: &[u8; 24], f: impl Fn(&Value) -> bool) -> bool {
        let mut m = std::mem::MaybeUninit::<Value>::zeroed();
        // SAFETY: only inspected through `f` when the bytes form a valid
        // primitive Value; candidates are tried with primitive tags only.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), m.as_mut_ptr() as *mut u8, 24);
            f(&*m.as_ptr())
        }
    }
    // The tag: a byte whose value selects the variant (checked by reading
    // back values built from scratch).
    for tag in 0..24usize {
        if (num..num + 8).contains(&tag) {
            continue;
        }
        let t_of = |v: Value| bytes_of(v)[tag];
        let (t_undef, t_null, t_hole, t_bool, t_num, t_big) = (t_of(Value::Undef), t_of(Value::Null), t_of(Value::Hole), t_of(Value::Bool(true)), t_of(Value::Num(1.0)), t_of(Value::big(1)));
        let t_str = t_of(Value::str("x"));
        let t_obj = t_of(Value::Obj(Rc::new(RefCell::new(Obj { props: Props::default(), proto: None, kind: Kind::Plain }))));
        let tags = [t_undef, t_null, t_hole, t_bool, t_num, t_big, t_str, t_obj];
        if (0..tags.len()).any(|i| (i + 1..tags.len()).any(|j| tags[i] == tags[j])) {
            continue;
        }
        let with = |t: u8| {
            let mut b = [0u8; 24];
            b[tag] = t;
            b
        };
        let mut nb = with(t_num);
        nb[num..num + 8].copy_from_slice(&2.5f64.to_le_bytes());
        if !(read(&with(t_undef), |v| matches!(v, Value::Undef)) && read(&with(t_null), |v| matches!(v, Value::Null)) && read(&nb, |v| matches!(v, Value::Num(n) if *n == 2.5))) {
            continue;
        }
        // The Bool payload byte.
        let boolean = (0..24).filter(|&o| o != tag).find(|&o| {
            let mut bt = with(t_bool);
            bt[o] = 1;
            read(&bt, |v| matches!(v, Value::Bool(true))) && read(&with(t_bool), |v| matches!(v, Value::Bool(false)))
        })?;
        return Some(Layout { tag: tag as i32, num: num as i32, boolean: boolean as i32, t_undef, t_null, t_bool, t_num, heap: [t_str, t_obj] });
    }
    None
}

/// Where machine code finds an object's shape id, prototype and property
/// values (all probed / measured, never assumed).
#[derive(Clone, Copy, Debug)]
pub struct ObjLayout {
    /// Value::Obj: offset of the Rc pointer.
    obj_ptr: i32,
    /// From the Rc pointer to the `Obj`.
    obj: i32,
    /// Within Obj: Props.shape (an Rc pointer), Props.vals' buffer pointer,
    /// proto (an Rc pointer or 0).
    shape: i32,
    vals: i32,
    proto: i32,
    /// From a Shape's Rc pointer to its id.
    shape_id: i32,
    prop_size: i32,
    prop_v: i32,
    prop_get: i32,
    prop_set: i32,
    /// Tag byte of `Option<Value>::None`.
    none_tag: u8,
    ic_ids: i32,
    ic_slot: i32,
    ic_depth: i32,
}

fn word_at(p: *const u8, off: usize) -> usize {
    // SAFETY: callers pass offsets inside the probed object.
    unsafe { (p.add(off) as *const usize).read_unaligned() }
}

fn probe_obj(l: &Layout) -> Option<ObjLayout> {
    let proto = Rc::new(RefCell::new(Obj { props: Props::default(), proto: None, kind: Kind::Plain }));
    let mut props = Props::default();
    props.insert("a".into(), Prop::data(Value::Num(1.0)));
    props.insert("b".into(), Prop::data(Value::Num(2.0)));
    let o: ObjRef = Rc::new(RefCell::new(Obj { props, proto: Some(proto.clone()), kind: Kind::Plain }));
    let v = Value::Obj(o.clone());
    // The Rc pointer inside the Value: a word w with w + k == the RefCell.
    let cell = Rc::as_ptr(&o) as usize;
    let vp = &v as *const Value as *const u8;
    let (obj_ptr, rc_off) = (0..=16).step_by(8).filter(|&off| off as i32 != l.tag).find_map(|off| {
        let w = word_at(vp, off);
        (w != 0 && cell >= w && cell - w <= 64).then_some((off, cell - w))
    })?;
    // RefCell<Obj>: where the Obj is.
    let obj_in_cell = o.as_ptr() as usize - cell;
    let objp = o.as_ptr() as *const u8;
    let obj = rc_off + obj_in_cell;
    let (props_off, vals_vec) = Props::offsets();
    let props_at = std::mem::offset_of!(Obj, props);
    let proto_at = std::mem::offset_of!(Obj, proto);
    // proto: the parent's Rc pointer (null for None).
    let ob = o.borrow();
    if word_at(objp, proto_at) + rc_off != Rc::as_ptr(&proto) as usize || word_at(proto.as_ptr() as *const u8, proto_at) != 0 {
        return None;
    }
    // Props.shape: an Rc<Shape> pointer; the id within the Shape.
    let shape_rc = Rc::as_ptr(ob.props.shape()) as usize;
    let sw = word_at(objp, props_at + props_off);
    if shape_rc < sw || shape_rc - sw > 64 {
        return None;
    }
    let shape_id = (shape_rc - sw) + std::mem::offset_of!(super::shape::Shape, id);
    if word_at(sw as *const u8, shape_id) as u64 != ob.props.shape_id() {
        return None;
    }
    // Props.vals: the buffer pointer within the Vec.
    let slot0 = ob.props.slot(0) as *const Prop as usize;
    let vals = (0..24).step_by(8).map(|k| props_at + vals_vec + k).find(|&off| word_at(objp, off) == slot0)?;
    let prop_size = std::mem::size_of::<Prop>();
    if ob.props.slot(1) as *const Prop as usize != slot0 + prop_size {
        return None;
    }
    let p = ob.props.slot(0);
    let pp = p as *const Prop as *const u8;
    let prop_get = std::mem::offset_of!(Prop, get);
    let none_tag = unsafe { *pp.add(prop_get + l.tag as usize) };
    let some = Prop { v: Value::Undef, get: Some(Value::Undef), set: None, enumerable: true };
    let some_tag = unsafe { *(&some as *const Prop as *const u8).add(prop_get + l.tag as usize) };
    if none_tag == some_tag || [l.t_undef, l.t_null, l.t_bool, l.t_num, l.heap[0], l.heap[1]].contains(&none_tag) {
        return None;
    }
    let (ic_ids, ic_slot, ic_depth) = super::vm::Ic::offsets();
    let r = ObjLayout {
        obj_ptr: obj_ptr as i32,
        obj: obj as i32,
        shape: (props_at + props_off) as i32,
        vals: vals as i32,
        proto: proto_at as i32,
        shape_id: shape_id as i32,
        prop_size: prop_size as i32,
        prop_v: std::mem::offset_of!(Prop, v) as i32,
        prop_get: prop_get as i32,
        prop_set: std::mem::offset_of!(Prop, set) as i32,
        none_tag,
        ic_ids: ic_ids as i32,
        ic_slot: ic_slot as i32,
        ic_depth: ic_depth as i32,
    };
    drop(ob);
    Some(r)
}

pub fn obj_layout() -> Option<ObjLayout> {
    thread_local! {
        static L: Option<ObjLayout> = layout().and_then(|l| probe_obj(&l));
    }
    L.with(|l| *l)
}

pub fn layout() -> Option<Layout> {
    thread_local! {
        static L: Option<Layout> = if jit_enabled() { probe() } else { None };
    }
    L.with(|l| *l)
}

/// `BOLT_JIT=0` turns the JIT off.
pub fn jit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cfg!(all(windows, target_arch = "x86_64")) && std::env::var("BOLT_JIT").map_or(true, |v| v != "0"))
}

/// Calls + loop iterations before a function is compiled to machine code.
pub fn jit_threshold() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("BOLT_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(500))
}

// ------------------------------------------------------------------ runtime glue

/// Shared between machine code and `jit_step` (offsets are hard-coded below).
#[repr(C)]
pub struct Ctx {
    pub interp: *mut Interp,
    pub frame: *mut u8,
    pub code: *const Code,
    pub regs: *mut Value,
    pub table: *const u64,
    pub consts: *const Value,
    pub step: usize,
    /// Back edges left before the next `tick`.
    pub budget: i64,
    /// Return value / exception when the code exits.
    pub out: Option<Value>,
}

const C_REGS: i32 = 24;
const C_TABLE: i32 = 32;
const C_CONSTS: i32 = 40;
const C_BUDGET: i32 = 56;

/// `jit_step` results beyond op indexes.
pub const EXIT_RET: u64 = 0xFFFF_FFF0;
pub const EXIT_ERR: u64 = 0xFFFF_FFF1;

/// Back edges between ticks.
pub const BUDGET: i64 = 1024;

pub struct JitCode {
    entry: unsafe extern "win64" fn(*mut Ctx, u64) -> u64,
    table: Vec<u64>,
}

impl JitCode {
    /// Run from op `ip` until the function returns (EXIT_RET) or throws
    /// (EXIT_ERR); the value is in `ctx.out`.
    pub unsafe fn run(&self, ctx: &mut Ctx, ip: usize) -> u64 {
        ctx.table = self.table.as_ptr();
        unsafe { (self.entry)(ctx, ip as u64) }
    }
}

// ------------------------------------------------------------------ executable memory

#[cfg(windows)]
mod mem {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn RtlAddFunctionTable(table: *const RuntimeFunction, count: u32, base: u64) -> u8;
        fn FlushInstructionCache(process: *mut c_void, addr: *const c_void, size: usize) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
    }
    #[repr(C)]
    pub struct RuntimeFunction {
        begin: u32,
        end: u32,
        unwind: u32,
    }

    const CHUNK: usize = 4 << 20;

    thread_local! {
        static ARENA: std::cell::RefCell<(usize, usize)> = const { std::cell::RefCell::new((0, 0)) };
    }

    /// Copy machine code (plus its unwind data) into executable memory and
    /// register it for stack walks. Returns the code's address.
    pub fn install(code: &[u8], unwind: &[u8], prologue_end: usize) -> Option<usize> {
        let need = code.len() + unwind.len() + 64;
        if need > CHUNK {
            return None;
        }
        ARENA.with(|a| {
            let mut a = a.borrow_mut();
            if a.0 == 0 || a.1 + need > CHUNK {
                // PAGE_EXECUTE_READWRITE
                let p = unsafe { VirtualAlloc(std::ptr::null_mut(), CHUNK, 0x3000, 0x40) };
                if p.is_null() {
                    return None;
                }
                *a = (p as usize, 0);
            }
            let base = a.0;
            let start = (a.1 + 15) & !15;
            let uw = (start + code.len() + 3) & !3;
            let rf = (uw + unwind.len() + 3) & !3;
            a.1 = rf + std::mem::size_of::<RuntimeFunction>();
            unsafe {
                std::ptr::copy_nonoverlapping(code.as_ptr(), (base + start) as *mut u8, code.len());
                std::ptr::copy_nonoverlapping(unwind.as_ptr(), (base + uw) as *mut u8, unwind.len());
                let f = (base + rf) as *mut RuntimeFunction;
                f.write(RuntimeFunction { begin: start as u32, end: (start + code.len()) as u32, unwind: uw as u32 });
                RtlAddFunctionTable(f, 1, base as u64);
                FlushInstructionCache(GetCurrentProcess(), (base + start) as *const c_void, code.len());
            }
            let _ = prologue_end;
            Some(base + start)
        })
    }
}

// ------------------------------------------------------------------ assembler

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const R8: u8 = 8;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R12: u8 = 12;

struct Asm {
    b: Vec<u8>,
}

/// x86 condition codes.
#[derive(Clone, Copy)]
enum Cc {
    E = 0x4,
    Ne = 0x5,
    Ae = 0x3,
    A = 0x7,
    P = 0xA,
}

impl Asm {
    fn byte(&mut self, x: u8) {
        self.b.push(x);
    }
    fn bytes(&mut self, x: &[u8]) {
        self.b.extend_from_slice(x);
    }
    fn i32(&mut self, x: i32) {
        self.b.extend_from_slice(&x.to_le_bytes());
    }
    fn pos(&self) -> usize {
        self.b.len()
    }
    /// REX prefix (only emitted when needed unless `w`).
    fn rex(&mut self, w: bool, reg: u8, base: u8) {
        let r = 0x40 | (w as u8) << 3 | ((reg >> 3) & 1) << 2 | ((base >> 3) & 1);
        if r != 0x40 {
            self.byte(r);
        }
    }
    /// ModRM (+SIB) + disp32 for [base + disp].
    fn mem(&mut self, reg: u8, base: u8, disp: i32) {
        self.byte(0x80 | (reg & 7) << 3 | (base & 7));
        if base & 7 == 4 {
            self.byte(0x24);
        }
        self.i32(disp);
    }
    fn mov_load(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x8B);
        self.mem(dst, base, disp);
    }
    fn mov_store(&mut self, base: u8, disp: i32, src: u8) {
        self.rex(true, src, base);
        self.byte(0x89);
        self.mem(src, base, disp);
    }
    /// SSE2 scalar double op `F2 0F op` (or `66 0F op`) with a memory operand.
    fn sse_mem(&mut self, prefix: u8, op: u8, xmm: u8, base: u8, disp: i32) {
        self.byte(prefix);
        self.rex(false, xmm, base);
        self.bytes(&[0x0F, op]);
        self.mem(xmm, base, disp);
    }
    fn sse_rr(&mut self, prefix: u8, op: u8, x: u8, y: u8) {
        self.bytes(&[prefix, 0x0F, op, 0xC0 | (x & 7) << 3 | (y & 7)]);
    }
    fn movsd_load(&mut self, xmm: u8, base: u8, disp: i32) {
        self.sse_mem(0xF2, 0x10, xmm, base, disp);
    }
    fn movsd_store(&mut self, base: u8, disp: i32, xmm: u8) {
        self.sse_mem(0xF2, 0x11, xmm, base, disp);
    }
    fn cmp_byte(&mut self, base: u8, disp: i32, imm: u8) {
        self.rex(false, 0, base);
        self.byte(0x80);
        self.mem(7, base, disp);
        self.byte(imm);
    }
    fn mov_byte(&mut self, base: u8, disp: i32, imm: u8) {
        self.rex(false, 0, base);
        self.byte(0xC6);
        self.mem(0, base, disp);
        self.byte(imm);
    }
    /// jcc rel32 with a placeholder; returns the patch position.
    fn jcc(&mut self, cc: Cc) -> usize {
        self.bytes(&[0x0F, 0x80 | cc as u8]);
        self.i32(0);
        self.pos() - 4
    }
    fn jmp(&mut self) -> usize {
        self.byte(0xE9);
        self.i32(0);
        self.pos() - 4
    }
    fn patch(&mut self, at: usize, to: usize) {
        let rel = to as i64 - (at as i64 + 4);
        self.b[at..at + 4].copy_from_slice(&(rel as i32).to_le_bytes());
    }
}

// ------------------------------------------------------------------ compiler

/// A jump to patch: to op `to`, or to a slow path.
enum Fix {
    Op(usize, usize),
    Slow(usize),
}

fn reg_disp(r: u16, off: i32) -> i32 {
    r as i32 * VSIZE + off
}

/// Compile a function's bytecode. None: unsupported (the VM runs it).
pub fn compile(code: &Code) -> Option<JitCode> {
    let l = layout()?;
    let ops = code.ops();
    let consts = code.consts();
    let n = ops.len();
    let mut a = Asm { b: Vec::with_capacity(n * 48) };
    // Prologue: callee-saved rbx, rsi, rdi, r12, r13; 32 bytes of shadow space.
    a.bytes(&[0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x48, 0x83, 0xEC, 0x20]);
    let prologue = a.pos();
    a.bytes(&[0x48, 0x89, 0xCE]); // mov rsi, rcx (ctx)
    a.mov_load(RBX, RSI, C_REGS);
    a.mov_load(RDI, RSI, C_TABLE);
    a.mov_load(R12, RSI, C_CONSTS);
    a.bytes(&[0xFF, 0x24, 0xD7]); // jmp [rdi + rdx*8]
    let mut starts = vec![0usize; n];
    let mut fixes: Vec<Fix> = Vec::new();
    // Slow paths: (op index, where its guard jumps are).
    let mut slow_jumps: Vec<Vec<usize>> = vec![Vec::new(); n];
    let t = l.tag;
    let heap_guard = |a: &mut Asm, r: u16, slow: &mut Vec<usize>| {
        // The destination must not own heap memory (it isn't dropped).
        a.cmp_byte(RBX, reg_disp(r, t), l.heap[0]);
        slow.push(a.jcc(Cc::E));
        a.cmp_byte(RBX, reg_disp(r, t), l.heap[1]);
        slow.push(a.jcc(Cc::E));
    };
    let num_guard = |a: &mut Asm, base: u8, disp: i32, slow: &mut Vec<usize>| {
        a.cmp_byte(base, disp + t, l.t_num);
        slow.push(a.jcc(Cc::Ne));
    };
    let store_num = |a: &mut Asm, dst: u16| {
        a.mov_byte(RBX, reg_disp(dst, t), l.t_num);
        a.movsd_store(RBX, reg_disp(dst, l.num), 0);
    };
    let store_bool_from_flags = |a: &mut Asm, dst: u16, cc: CmpCc| {
        match cc {
            CmpCc::Simple(c) => a.bytes(&[0x0F, 0x90 | c as u8, 0xC0]), // setcc al
            // ==: ZF and not PF
            CmpCc::Eq => a.bytes(&[0x0F, 0x94, 0xC0, 0x0F, 0x9B, 0xC1, 0x20, 0xC8]),
            // !=: not ZF or PF
            CmpCc::Ne => a.bytes(&[0x0F, 0x95, 0xC0, 0x0F, 0x9A, 0xC1, 0x08, 0xC8]),
        }
        a.mov_byte(RBX, reg_disp(dst, t), l.t_bool);
        // mov [rbx+disp], al
        a.byte(0x88);
        a.mem(RAX, RBX, reg_disp(dst, l.boolean));
    };
    let ol = obj_layout();
    let const_num = |k: u32| -> bool { matches!(consts.get(k as usize), Some(Value::Num(_))) };
    let const_prim = |k: u32| -> bool { !matches!(consts.get(k as usize), Some(Value::Str(_) | Value::Obj(_)) | None) };

    for (i, op) in ops.iter().enumerate() {
        starts[i] = a.pos();
        let mut slow = std::mem::take(&mut slow_jumps[i]);
        let mut inline = true;
        match *op {
            Op::Jump { to } => fixes.push(Fix::Op(a.jmp(), to as usize)),
            Op::Loop { to } => {
                // dec qword [rsi+budget]; jle slow (tick); jmp to
                a.bytes(&[0x48, 0xFF]);
                a.mem(1, RSI, C_BUDGET);
                slow.push(a.jcc_le());
                fixes.push(Fix::Op(a.jmp(), to as usize));
            }
            Op::Undef { dst } => {
                heap_guard(&mut a, dst, &mut slow);
                a.mov_byte(RBX, reg_disp(dst, t), l.t_undef);
            }
            Op::Move { dst, src } if dst != src => {
                heap_guard(&mut a, dst, &mut slow);
                heap_guard(&mut a, src, &mut slow);
                for w in 0..3 {
                    a.mov_load(RAX, RBX, reg_disp(src, w * 8));
                    a.mov_store(RBX, reg_disp(dst, w * 8), RAX);
                }
            }
            Op::Const { dst, k } if const_prim(k) => {
                heap_guard(&mut a, dst, &mut slow);
                for w in 0..3 {
                    a.mov_load(RAX, R12, k as i32 * VSIZE + w * 8);
                    a.mov_store(RBX, reg_disp(dst, w * 8), RAX);
                }
            }
            Op::Bin { op, dst, a: x, b: y } if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                num_guard(&mut a, RBX, reg_disp(y, 0), &mut slow);
                heap_guard(&mut a, dst, &mut slow);
                a.movsd_load(0, RBX, reg_disp(x, l.num));
                a.sse_mem(0xF2, arith_op(op), 0, RBX, reg_disp(y, l.num));
                store_num(&mut a, dst);
            }
            Op::BinK { op, dst, a: x, k } if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) && const_num(k) => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                heap_guard(&mut a, dst, &mut slow);
                a.movsd_load(0, RBX, reg_disp(x, l.num));
                a.sse_mem(0xF2, arith_op(op), 0, R12, k as i32 * VSIZE + l.num);
                store_num(&mut a, dst);
            }
            Op::GetProp { dst, obj, ic, .. } if ol.is_some() && code.ic_addr(ic).is_some() => {
                let o = ol.unwrap();
                let ic_addr = code.ic_addr(ic).unwrap();
                // rax = the receiver's Obj; rcx = its shape id.
                a.cmp_byte(RBX, reg_disp(obj, t), l.heap[1]);
                slow.push(a.jcc(Cc::Ne));
                a.mov_load(RAX, RBX, reg_disp(obj, o.obj_ptr));
                a.lea(RAX, RAX, o.obj);
                a.mov_load(RCX, RAX, o.shape);
                a.mov_load(RCX, RCX, o.shape_id);
                a.mov_imm64(RDX, ic_addr as u64);
                a.cmp_load(RCX, RDX, o.ic_ids);
                slow.push(a.jcc(Cc::Ne));
                a.cmp_byte(RDX, o.ic_depth, 0);
                let own = a.jcc(Cc::E);
                a.cmp_byte(RDX, o.ic_depth, 1);
                slow.push(a.jcc(Cc::Ne));
                // Depth 1: the prototype holds it.
                a.mov_load(RAX, RAX, o.proto);
                a.test_self(RAX);
                slow.push(a.jcc(Cc::E));
                a.lea(RAX, RAX, o.obj);
                a.mov_load(RCX, RAX, o.shape);
                a.mov_load(RCX, RCX, o.shape_id);
                a.cmp_load(RCX, RDX, o.ic_ids + 8);
                slow.push(a.jcc(Cc::Ne));
                let here = a.pos();
                a.patch(own, here);
                emit_prop_slot(&mut a, &l, &o, &mut slow);
                // rcx = &prop.v → registers[dst]
                emit_copy_value(&mut a, &l, RCX, o.prop_v, RBX, reg_disp(dst, 0));
            }
            Op::SetProp { obj, src, ic, .. } if ol.is_some() && code.ic_addr(ic).is_some() => {
                let o = ol.unwrap();
                let ic_addr = code.ic_addr(ic).unwrap();
                a.cmp_byte(RBX, reg_disp(obj, t), l.heap[1]);
                slow.push(a.jcc(Cc::Ne));
                a.mov_load(RAX, RBX, reg_disp(obj, o.obj_ptr));
                a.lea(RAX, RAX, o.obj);
                a.mov_load(RCX, RAX, o.shape);
                a.mov_load(RCX, RCX, o.shape_id);
                a.mov_imm64(RDX, ic_addr as u64);
                a.cmp_load(RCX, RDX, o.ic_ids);
                slow.push(a.jcc(Cc::Ne));
                // Only stores to an existing own data property.
                a.cmp_byte(RDX, o.ic_depth, 0);
                slow.push(a.jcc(Cc::Ne));
                emit_prop_slot(&mut a, &l, &o, &mut slow);
                emit_copy_value(&mut a, &l, RBX, reg_disp(src, 0), RCX, o.prop_v);
            }
            Op::Bin { op: BinOp::Mod, dst, a: x, b: y } => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                num_guard(&mut a, RBX, reg_disp(y, 0), &mut slow);
                heap_guard(&mut a, dst, &mut slow);
                a.movsd_load(0, RBX, reg_disp(x, l.num));
                a.movsd_load(1, RBX, reg_disp(y, l.num));
                call_abs(&mut a, super::vm::jit_mod as *const () as usize);
                store_num(&mut a, dst);
            }
            Op::BinK { op: BinOp::Mod, dst, a: x, k } if const_num(k) => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                heap_guard(&mut a, dst, &mut slow);
                a.movsd_load(0, RBX, reg_disp(x, l.num));
                a.movsd_load(1, R12, k as i32 * VSIZE + l.num);
                call_abs(&mut a, super::vm::jit_mod as *const () as usize);
                store_num(&mut a, dst);
            }
            Op::Bin { op, dst, a: x, b: y } if op.is_cmp() => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                num_guard(&mut a, RBX, reg_disp(y, 0), &mut slow);
                heap_guard(&mut a, dst, &mut slow);
                let cc = compare(&mut a, op, (RBX, reg_disp(x, l.num)), (RBX, reg_disp(y, l.num)));
                store_bool_from_flags(&mut a, dst, cc);
            }
            Op::BinK { op, dst, a: x, k } if op.is_cmp() && const_num(k) => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                heap_guard(&mut a, dst, &mut slow);
                let cc = compare(&mut a, op, (RBX, reg_disp(x, l.num)), (R12, k as i32 * VSIZE + l.num));
                store_bool_from_flags(&mut a, dst, cc);
            }
            Op::CmpJump { op, a: x, b: y, to, when } => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                num_guard(&mut a, RBX, reg_disp(y, 0), &mut slow);
                let cc = compare(&mut a, op, (RBX, reg_disp(x, l.num)), (RBX, reg_disp(y, l.num)));
                branch(&mut a, cc, when, to as usize, &mut fixes);
            }
            Op::CmpJumpK { op, a: x, k, to, when } if const_num(k) => {
                num_guard(&mut a, RBX, reg_disp(x, 0), &mut slow);
                let cc = compare(&mut a, op, (RBX, reg_disp(x, l.num)), (R12, k as i32 * VSIZE + l.num));
                branch(&mut a, cc, when, to as usize, &mut fixes);
            }
            Op::JumpIf { cond, to } | Op::JumpIfNot { cond, to } => {
                let when = matches!(op, Op::JumpIf { .. });
                // Bool: test the byte; Num: compare with 0 (NaN is falsy).
                a.cmp_byte(RBX, reg_disp(cond, t), l.t_bool);
                let not_bool = a.jcc(Cc::Ne);
                a.cmp_byte(RBX, reg_disp(cond, l.boolean), 0);
                fixes.push(Fix::Op(a.jcc(if when { Cc::Ne } else { Cc::E }), to as usize));
                let done = a.jmp();
                let here = a.pos();
                a.patch(not_bool, here);
                num_guard(&mut a, RBX, reg_disp(cond, 0), &mut slow);
                a.sse_rr(0x66, 0x57, 1, 1); // xorpd xmm1, xmm1
                a.movsd_load(0, RBX, reg_disp(cond, l.num));
                a.sse_rr(0x66, 0x2E, 0, 1); // ucomisd xmm0, xmm1
                // Truthy iff ZF = 0 (NaN sets ZF).
                fixes.push(Fix::Op(a.jcc(if when { Cc::Ne } else { Cc::E }), to as usize));
                let here = a.pos();
                a.patch(done, here);
            }
            Op::Step { dst, src, delta } => {
                num_guard(&mut a, RBX, reg_disp(src, 0), &mut slow);
                if dst != src {
                    heap_guard(&mut a, dst, &mut slow);
                }
                a.movsd_load(0, RBX, reg_disp(src, l.num));
                // xmm1 = ±1.0 (mov rax, imm64; movq xmm1, rax)
                let one: f64 = delta as f64;
                a.bytes(&[0x48, 0xB8]);
                a.bytes(&one.to_bits().to_le_bytes());
                a.bytes(&[0x66, 0x48, 0x0F, 0x6E, 0xC8]); // movq xmm1, rax
                a.sse_rr(0xF2, 0x58, 0, 1); // addsd xmm0, xmm1
                store_num(&mut a, dst);
            }
            _ => inline = false,
        }
        // Inline code falls through to the next op.
        if !inline {
            // The op runs in the VM (no guard jumps: it's all slow path).
            emit_step(&mut a, i, n, &mut fixes, super::vm::jit_helper_for(op));
        } else {
            slow_jumps[i] = slow;
        }
    }
    // Falling off the end can't happen (functions end in Return); trap.
    a.byte(0xCC);
    // Out-of-line slow paths for guarded ops.
    for i in 0..n {
        if slow_jumps[i].is_empty() {
            continue;
        }
        let here = a.pos();
        for &j in &slow_jumps[i] {
            a.patch(j, here);
        }
        emit_step(&mut a, i, n, &mut fixes, super::vm::jit_helper_for(&ops[i]));
    }
    // Shared dispatch: rax = next op index or an exit code.
    let dispatch = a.pos();
    a.bytes(&[0x48, 0x3D]);
    a.i32(n as i32); // cmp rax, n
    let exit = a.jcc(Cc::Ae);
    a.bytes(&[0xFF, 0x24, 0xC7]); // jmp [rdi + rax*8]
    let epilogue = a.pos();
    a.patch(exit, epilogue);
    a.bytes(&[0x48, 0x83, 0xC4, 0x20, 0x41, 0x5D, 0x41, 0x5C, 0x5F, 0x5E, 0x5B, 0xC3]);
    for f in fixes {
        match f {
            Fix::Op(at, to) => {
                let target = if to < n { starts[to] } else { epilogue };
                a.patch(at, target);
            }
            Fix::Slow(at) => a.patch(at, dispatch),
        }
    }
    // Unwind info: version 1, prologue size, 6 codes, no frame register.
    let unwind = [
        1u8,
        prologue as u8,
        6,
        0,
        11,
        0x32, // sub rsp, 32: UWOP_ALLOC_SMALL (32/8 - 1 = 3)
        7,
        0xD0, // push r13
        5,
        0xC0, // push r12
        3,
        0x70, // push rdi
        2,
        0x60, // push rsi
        1,
        0x30, // push rbx
    ];
    let base = mem::install(&a.b, &unwind, prologue)?;
    let table = starts.iter().map(|&s| (base + s) as u64).collect();
    // SAFETY: freshly written machine code following the win64 ABI.
    let entry = unsafe { std::mem::transmute::<usize, unsafe extern "win64" fn(*mut Ctx, u64) -> u64>(base) };
    Some(JitCode { entry, table })
}

impl Asm {
    fn lea(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x8D);
        self.mem(dst, base, disp);
    }
    /// add dst, [base + disp]
    fn add_load(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(true, dst, base);
        self.byte(0x03);
        self.mem(dst, base, disp);
    }
    /// cmp reg, [base + disp]
    fn cmp_load(&mut self, reg: u8, base: u8, disp: i32) {
        self.rex(true, reg, base);
        self.byte(0x3B);
        self.mem(reg, base, disp);
    }
    /// mov r32, [base + disp] (zero-extended)
    fn mov_load32(&mut self, dst: u8, base: u8, disp: i32) {
        self.rex(false, dst, base);
        self.byte(0x8B);
        self.mem(dst, base, disp);
    }
    /// imul reg, reg, imm32
    fn imul_imm(&mut self, reg: u8, imm: i32) {
        self.rex(true, reg, reg);
        self.byte(0x69);
        self.byte(0xC0 | (reg & 7) << 3 | (reg & 7));
        self.i32(imm);
    }
    fn mov_imm64(&mut self, reg: u8, imm: u64) {
        self.rex(true, 0, reg);
        self.byte(0xB8 | (reg & 7));
        self.bytes(&imm.to_le_bytes());
    }
    fn test_self(&mut self, reg: u8) {
        self.rex(true, reg, reg);
        self.byte(0x85);
        self.byte(0xC0 | (reg & 7) << 3 | (reg & 7));
    }

    fn jcc_le(&mut self) -> usize {
        self.bytes(&[0x0F, 0x8E]);
        self.i32(0);
        self.pos() - 4
    }
}

fn arith_op(op: BinOp) -> u8 {
    match op {
        BinOp::Add => 0x58,
        BinOp::Sub => 0x5C,
        BinOp::Mul => 0x59,
        _ => 0x5E,
    }
}

/// How a comparison's result reads from the flags after `ucomisd`.
#[derive(Clone, Copy)]
enum CmpCc {
    /// True iff the condition holds (`ja` / `jae`).
    Simple(Cc),
    /// ==: ZF = 1 and PF = 0.
    Eq,
    /// !=: ZF = 0 or PF = 1.
    Ne,
}

/// Emit `ucomisd` for `x op y` (operands as (base, disp)); the result
/// condition. `<` / `<=` swap the operands so NaN compares false.
fn compare(a: &mut Asm, op: BinOp, x: (u8, i32), y: (u8, i32)) -> CmpCc {
    let (first, second, cc) = match op {
        BinOp::Lt => (y, x, CmpCc::Simple(Cc::A)),
        BinOp::Le => (y, x, CmpCc::Simple(Cc::Ae)),
        BinOp::Gt => (x, y, CmpCc::Simple(Cc::A)),
        BinOp::Ge => (x, y, CmpCc::Simple(Cc::Ae)),
        BinOp::Eq | BinOp::StrictEq => (x, y, CmpCc::Eq),
        _ => (x, y, CmpCc::Ne),
    };
    a.movsd_load(0, first.0, first.1);
    a.sse_mem(0x66, 0x2E, 0, second.0, second.1);
    cc
}

/// Jump to op `to` when the comparison's truth equals `when`.
fn branch(a: &mut Asm, cc: CmpCc, when: bool, to: usize, fixes: &mut Vec<Fix>) {
    // Normalize != to == with the opposite sense.
    let (cc, when) = match cc {
        CmpCc::Ne => (CmpCc::Eq, !when),
        c => (c, when),
    };
    match (cc, when) {
        (CmpCc::Simple(c), true) => fixes.push(Fix::Op(a.jcc(c), to)),
        (CmpCc::Simple(c), false) => {
            // Not above / not above-or-equal (includes NaN).
            let inv = match c {
                Cc::A => 0x6, // jbe
                _ => 0x2,     // jb
            };
            a.bytes(&[0x0F, 0x80 | inv]);
            a.i32(0);
            let at = a.pos() - 4;
            fixes.push(Fix::Op(at, to));
        }
        (CmpCc::Eq, true) => {
            // jp skip; je to
            let skip = a.jcc(Cc::P);
            fixes.push(Fix::Op(a.jcc(Cc::E), to));
            let here = a.pos();
            a.patch(skip, here);
        }
        (CmpCc::Eq, false) => {
            // jp to; jne to
            fixes.push(Fix::Op(a.jcc(Cc::P), to));
            fixes.push(Fix::Op(a.jcc(Cc::Ne), to));
        }
        (CmpCc::Ne, _) => unreachable!(),
    }
}

/// Call op `i`'s helper (`jit_step` or a dedicated one), then continue at
/// the op it returns.
fn emit_step(a: &mut Asm, i: usize, n: usize, fixes: &mut Vec<Fix>, helper: usize) {
    a.bytes(&[0x48, 0x89, 0xF1]); // mov rcx, rsi
    a.byte(0xBA);
    a.i32(i as i32); // mov edx, i
    a.bytes(&[0x48, 0xB8]);
    a.bytes(&(helper as u64).to_le_bytes()); // mov rax, helper
    a.bytes(&[0xFF, 0xD0]); // call rax
    // Reload the register base (the frame is fixed, but stay safe).
    a.mov_load(RBX, RSI, C_REGS);
    if i + 1 < n {
        a.bytes(&[0x48, 0x3D]);
        a.i32(i as i32 + 1); // cmp rax, i+1
        fixes.push(Fix::Op(a.jcc(Cc::E), i + 1));
    }
    fixes.push(Fix::Slow(a.jmp()));
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_layout() {
        for v in [Value::Undef, Value::Null, Value::Hole, Value::Bool(true), Value::Bool(false), Value::Num(1.0), Value::Num(2.0), Value::big(5), Value::str("x")] {
            eprintln!("{:<12} {:02x?}", format!("{v:?}"), &bytes_of(v)[..24]);
        }
        eprintln!("{:?}", probe());
        assert!(probe().is_some());
    }
}

/// mov rax, addr; call rax (stack stays aligned; volatile registers lost).
fn call_abs(a: &mut Asm, addr: usize) {
    a.bytes(&[0x48, 0xB8]);
    a.bytes(&(addr as u64).to_le_bytes());
    a.bytes(&[0xFF, 0xD0]);
}

/// With rax = an Obj and rdx = its inline cache: rcx = &props[ic.slot],
/// which must be a data property.
fn emit_prop_slot(a: &mut Asm, l: &Layout, o: &ObjLayout, slow: &mut Vec<usize>) {
    a.mov_load32(RCX, RDX, o.ic_slot);
    a.imul_imm(RCX, o.prop_size);
    a.add_load(RCX, RAX, o.vals);
    a.cmp_byte(RCX, o.prop_get + l.tag, o.none_tag);
    slow.push(a.jcc(Cc::Ne));
    a.cmp_byte(RCX, o.prop_set + l.tag, o.none_tag);
    slow.push(a.jcc(Cc::Ne));
}

/// Copy the Value at [sb + sd] to [db + dd]: inline for primitives (the
/// destination holding one too), else through `jit_assign` (reference
/// counts). Clobbers rax, rcx, rdx, r8.
fn emit_copy_value(a: &mut Asm, l: &Layout, sb: u8, sd: i32, db: u8, dd: i32) {
    let mut to_assign = Vec::new();
    for (b, d) in [(sb, sd), (db, dd)] {
        a.cmp_byte(b, d + l.tag, l.heap[0]);
        to_assign.push(a.jcc(Cc::E));
        a.cmp_byte(b, d + l.tag, l.heap[1]);
        to_assign.push(a.jcc(Cc::E));
    }
    for w in 0..3 {
        a.mov_load(R8, sb, sd + w * 8);
        a.mov_store(db, dd + w * 8, R8);
    }
    let done = a.jmp();
    let here = a.pos();
    for j in to_assign {
        a.patch(j, here);
    }
    // jit_assign(dst, src)
    a.lea(RDX, sb, sd);
    a.lea(RCX, db, dd);
    call_abs(a, super::vm::jit_assign as *const () as usize);
    let here = a.pos();
    a.patch(done, here);
}

#[cfg(test)]
mod obj_tests {
    #[test]
    fn obj_layout_probes() {
        let l = super::obj_layout();
        eprintln!("{l:?}");
        assert!(l.is_some());
    }
}
