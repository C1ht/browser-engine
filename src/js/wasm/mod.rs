//! WebAssembly: a decoder that compiles function bodies into a flat
//! instruction array (branch targets and stack heights resolved ahead of
//! time), an interpreter (`exec`) and the JS API (`api`).
//!
//! Supported: the core spec with multi-value, sign extension, saturating
//! conversions, bulk memory, reference types (multiple tables, externref)
//! and extended constant expressions. Not supported: SIMD, threads,
//! exception handling, memory64, GC.

pub mod api;
pub mod exec;

use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    FuncRef,
    ExternRef,
}

impl ValType {
    pub fn name(self) -> &'static str {
        match self {
            ValType::I32 => "i32",
            ValType::I64 => "i64",
            ValType::F32 => "f32",
            ValType::F64 => "f64",
            ValType::FuncRef => "anyfunc",
            ValType::ExternRef => "externref",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, Default)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub struct TableType {
    pub elem: ValType,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub enum ImportDesc {
    Func(u32),
    Table(TableType),
    Memory(Limits),
    Global(ValType, bool),
    Tag,
}

#[derive(Clone, Debug)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub desc: ImportDesc,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExternKind {
    Func,
    Table,
    Memory,
    Global,
    Tag,
}

impl ExternKind {
    pub fn name(self) -> &'static str {
        match self {
            ExternKind::Func => "function",
            ExternKind::Table => "table",
            ExternKind::Memory => "memory",
            ExternKind::Global => "global",
            ExternKind::Tag => "tag",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Export {
    pub name: String,
    pub kind: ExternKind,
    pub index: u32,
}

/// A constant expression, evaluated at instantiation.
#[derive(Clone, Debug)]
pub enum ConstOp {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    RefNull,
    RefFunc(u32),
    GlobalGet(u32),
    I32Add,
    I32Sub,
    I32Mul,
    I64Add,
    I64Sub,
    I64Mul,
}

#[derive(Clone, Debug)]
pub struct GlobalDef {
    pub ty: ValType,
    pub mutable: bool,
    pub init: Vec<ConstOp>,
}

#[derive(Clone, Debug)]
pub enum SegMode {
    Active { index: u32, offset: Vec<ConstOp> },
    Passive,
    Declarative,
}

#[derive(Clone, Debug)]
pub struct ElemSeg {
    pub mode: SegMode,
    #[allow(dead_code)]
    pub ty: ValType,
    pub items: Vec<Vec<ConstOp>>,
}

#[derive(Clone, Debug)]
pub struct DataSeg {
    pub mode: SegMode,
    pub bytes: Vec<u8>,
}

/// One compiled instruction, register style: operands and results are
/// frame slots (locals first, then the operand stack, whose height is
/// known at every instruction). `code` is the wasm opcode (0xFC-prefixed
/// ones are 0x100 + the sub-opcode); interpreter-only ops are >= 0x200.
///
/// Typical layouts: binary `a = b op c`; unary / load `a = op(b)` (loads
/// with offset `c`); store `mem[a + c] = b`; immediate binary
/// (`IMM + opcode`) `a = b op imm(c)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Op {
    pub code: u16,
    pub k: u16,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

/// A branch: jump to `target`, moving `keep` values from slot `src` to
/// slot `dst`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrEntry {
    pub target: u32,
    pub keep: u32,
    pub src: u32,
    pub dst: u32,
}

/// a = b
pub const OP_COPY: u16 = 0x200;
/// a = b | c << 32
pub const OP_CONST: u16 = 0x201;
/// pc = a
pub const OP_JUMP: u16 = 0x203;
/// if (b <cmp> c) branch brs[a]: OP_BR_CMP + (i32 compare opcode - 0x46)
pub const OP_BR_CMP: u16 = 0x400;
/// if (b <cmp> imm c) branch brs[a]
pub const OP_BR_CMPI: u16 = 0x410;
/// if !(b <cmp> c) pc = a   (an `if` on a compare)
pub const OP_IF_CMP: u16 = 0x420;
/// if !(b <cmp> imm c) pc = a
pub const OP_IF_CMPI: u16 = 0x430;
/// if (b == 0) branch brs[a]
pub const OP_BR_EQZ: u16 = 0x206;
/// Immediate forms of i32 / i64 binary ops: IMM + opcode.
pub const IMM: u16 = 0x300;

pub struct Code {
    /// Parameters + declared locals.
    pub nlocals: u32,
    pub nparams: u32,
    #[allow(dead_code)]
    pub nresults: u32,
    /// Slots the frame needs.
    pub max_height: u32,
    pub ops: Vec<Op>,
    pub brs: Vec<BrEntry>,
}

pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    /// Type index of every function (imports first).
    pub func_types: Vec<u32>,
    pub n_func_imports: u32,
    pub tables: Vec<TableType>,
    pub n_table_imports: u32,
    pub memories: Vec<Limits>,
    pub n_mem_imports: u32,
    pub globals: Vec<GlobalDef>,
    /// Types of imported globals (in import order).
    pub global_imports: Vec<(ValType, bool)>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elems: Vec<ElemSeg>,
    pub datas: Vec<DataSeg>,
    pub codes: Vec<Code>,
    pub customs: Vec<(String, Vec<u8>)>,
    pub n_tags: u32,
}

impl Module {
    pub fn func_type(&self, f: u32) -> &FuncType {
        &self.types[self.func_types[f as usize] as usize]
    }
}

type Res<T> = Result<T, String>;

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn eof(&self) -> bool {
        self.p >= self.d.len()
    }
    fn byte(&mut self) -> Res<u8> {
        let b = *self.d.get(self.p).ok_or("unexpected end")?;
        self.p += 1;
        Ok(b)
    }
    fn bytes(&mut self, n: usize) -> Res<&'a [u8]> {
        if self.p + n > self.d.len() {
            return Err("unexpected end".into());
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Res<u32> {
        let mut r: u64 = 0;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            r |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                return Err("integer representation too long".into());
            }
        }
        u32::try_from(r).map_err(|_| "integer too large".into())
    }
    fn sleb(&mut self, bits: u32) -> Res<i64> {
        let mut r: i64 = 0;
        let mut shift = 0;
        let mut b;
        loop {
            b = self.byte()?;
            if shift < 64 {
                r |= ((b & 0x7F) as i64) << shift;
            }
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
            if shift > bits + 7 {
                return Err("integer representation too long".into());
            }
        }
        if shift < 64 && b & 0x40 != 0 {
            r |= -1i64 << shift;
        }
        Ok(r)
    }
    fn i32(&mut self) -> Res<i32> {
        Ok(self.sleb(32)? as i32)
    }
    fn i64(&mut self) -> Res<i64> {
        self.sleb(64)
    }
    fn name(&mut self) -> Res<String> {
        let n = self.u32()? as usize;
        let b = self.bytes(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| "malformed UTF-8 encoding".into())
    }
    fn valtype(&mut self) -> Res<ValType> {
        valtype(self.byte()?)
    }
    fn limits(&mut self) -> Res<Limits> {
        let flag = self.byte()?;
        if flag & 0x04 != 0 {
            return Err("memory64 is not supported".into());
        }
        let min = self.u32()?;
        let max = if flag & 1 != 0 { Some(self.u32()?) } else { None };
        Ok(Limits { min, max })
    }
}

fn valtype(b: u8) -> Res<ValType> {
    Ok(match b {
        0x7F => ValType::I32,
        0x7E => ValType::I64,
        0x7D => ValType::F32,
        0x7C => ValType::F64,
        0x70 => ValType::FuncRef,
        0x6F => ValType::ExternRef,
        0x7B => return Err("SIMD (v128) is not supported".into()),
        _ => return Err(format!("invalid value type 0x{b:x}")),
    })
}

fn const_expr(r: &mut Reader) -> Res<Vec<ConstOp>> {
    let mut out = Vec::new();
    loop {
        let op = r.byte()?;
        out.push(match op {
            0x0B => return Ok(out),
            0x41 => ConstOp::I32(r.i32()?),
            0x42 => ConstOp::I64(r.i64()?),
            0x43 => ConstOp::F32(u32::from_le_bytes(r.bytes(4)?.try_into().unwrap())),
            0x44 => ConstOp::F64(u64::from_le_bytes(r.bytes(8)?.try_into().unwrap())),
            0xD0 => {
                r.byte()?;
                ConstOp::RefNull
            }
            0xD2 => ConstOp::RefFunc(r.u32()?),
            0x23 => ConstOp::GlobalGet(r.u32()?),
            0x6A => ConstOp::I32Add,
            0x6B => ConstOp::I32Sub,
            0x6C => ConstOp::I32Mul,
            0x7C => ConstOp::I64Add,
            0x7D => ConstOp::I64Sub,
            0x7E => ConstOp::I64Mul,
            _ => return Err(format!("constant expression required (opcode 0x{op:x})")),
        });
    }
}

/// Decode and compile a module.
pub fn parse(bytes: &[u8]) -> Res<Module> {
    let mut r = Reader { d: bytes, p: 0 };
    if r.bytes(4).ok() != Some(b"\0asm") {
        return Err("magic header not detected".into());
    }
    if r.bytes(4).ok() != Some(&[1, 0, 0, 0]) {
        return Err("unknown binary version".into());
    }
    let mut m = Module {
        types: Vec::new(),
        imports: Vec::new(),
        func_types: Vec::new(),
        n_func_imports: 0,
        tables: Vec::new(),
        n_table_imports: 0,
        memories: Vec::new(),
        n_mem_imports: 0,
        globals: Vec::new(),
        global_imports: Vec::new(),
        exports: Vec::new(),
        start: None,
        elems: Vec::new(),
        datas: Vec::new(),
        codes: Vec::new(),
        customs: Vec::new(),
        n_tags: 0,
    };
    let mut bodies: Vec<&[u8]> = Vec::new();
    while !r.eof() {
        let id = r.byte()?;
        let len = r.u32()? as usize;
        let body = r.bytes(len)?;
        let mut s = Reader { d: body, p: 0 };
        match id {
            0 => {
                let name = s.name()?;
                m.customs.push((name, body[s.p..].to_vec()));
            }
            1 => {
                for _ in 0..s.u32()? {
                    let form = s.byte()?;
                    if form != 0x60 {
                        return Err(format!("unsupported type form 0x{form:x}"));
                    }
                    let np = s.u32()?;
                    let params = (0..np).map(|_| s.valtype()).collect::<Res<Vec<_>>>()?;
                    let nr = s.u32()?;
                    let results = (0..nr).map(|_| s.valtype()).collect::<Res<Vec<_>>>()?;
                    m.types.push(FuncType { params, results });
                }
            }
            2 => {
                for _ in 0..s.u32()? {
                    let module = s.name()?;
                    let name = s.name()?;
                    let desc = match s.byte()? {
                        0 => {
                            let t = s.u32()?;
                            if t as usize >= m.types.len() {
                                return Err("unknown type".into());
                            }
                            m.func_types.push(t);
                            m.n_func_imports += 1;
                            ImportDesc::Func(t)
                        }
                        1 => {
                            let elem = s.valtype()?;
                            let limits = s.limits()?;
                            let t = TableType { elem, limits };
                            m.tables.push(t);
                            m.n_table_imports += 1;
                            ImportDesc::Table(t)
                        }
                        2 => {
                            let l = s.limits()?;
                            m.memories.push(l);
                            m.n_mem_imports += 1;
                            ImportDesc::Memory(l)
                        }
                        3 => {
                            let t = s.valtype()?;
                            let mutable = s.byte()? == 1;
                            m.global_imports.push((t, mutable));
                            ImportDesc::Global(t, mutable)
                        }
                        4 => {
                            s.byte()?;
                            s.u32()?;
                            m.n_tags += 1;
                            ImportDesc::Tag
                        }
                        k => return Err(format!("invalid import kind {k}")),
                    };
                    m.imports.push(Import { module, name, desc });
                }
            }
            3 => {
                for _ in 0..s.u32()? {
                    let t = s.u32()?;
                    if t as usize >= m.types.len() {
                        return Err("unknown type".into());
                    }
                    m.func_types.push(t);
                }
            }
            4 => {
                for _ in 0..s.u32()? {
                    let elem = s.valtype()?;
                    let limits = s.limits()?;
                    m.tables.push(TableType { elem, limits });
                }
            }
            5 => {
                for _ in 0..s.u32()? {
                    m.memories.push(s.limits()?);
                }
                if m.memories.len() > 1 {
                    return Err("multiple memories are not supported".into());
                }
            }
            6 => {
                for _ in 0..s.u32()? {
                    let ty = s.valtype()?;
                    let mutable = s.byte()? == 1;
                    let init = const_expr(&mut s)?;
                    m.globals.push(GlobalDef { ty, mutable, init });
                }
            }
            7 => {
                for _ in 0..s.u32()? {
                    let name = s.name()?;
                    let kind = match s.byte()? {
                        0 => ExternKind::Func,
                        1 => ExternKind::Table,
                        2 => ExternKind::Memory,
                        3 => ExternKind::Global,
                        4 => ExternKind::Tag,
                        k => return Err(format!("invalid export kind {k}")),
                    };
                    let index = s.u32()?;
                    m.exports.push(Export { name, kind, index });
                }
            }
            8 => m.start = Some(s.u32()?),
            9 => {
                for _ in 0..s.u32()? {
                    m.elems.push(elem_seg(&mut s)?);
                }
            }
            10 => {
                let n = s.u32()?;
                for _ in 0..n {
                    let size = s.u32()? as usize;
                    bodies.push(s.bytes(size)?);
                }
            }
            11 => {
                for _ in 0..s.u32()? {
                    let flag = s.u32()?;
                    let mode = match flag {
                        0 => SegMode::Active { index: 0, offset: const_expr(&mut s)? },
                        1 => SegMode::Passive,
                        2 => {
                            let index = s.u32()?;
                            SegMode::Active { index, offset: const_expr(&mut s)? }
                        }
                        _ => return Err("invalid data segment flags".into()),
                    };
                    let n = s.u32()? as usize;
                    m.datas.push(DataSeg { mode, bytes: s.bytes(n)?.to_vec() });
                }
            }
            12 => {
                s.u32()?;
            }
            13 => {
                for _ in 0..s.u32()? {
                    s.byte()?;
                    s.u32()?;
                    m.n_tags += 1;
                }
            }
            _ => return Err(format!("unknown section id {id}")),
        }
        if id != 0 && s.p != body.len() {
            return Err(format!("section size mismatch (section {id})"));
        }
    }
    let defined = m.func_types.len() - m.n_func_imports as usize;
    if bodies.len() != defined {
        return Err("function and code section have inconsistent lengths".into());
    }
    for (i, body) in bodies.into_iter().enumerate() {
        let f = m.n_func_imports as usize + i;
        let code = compile(&m, f as u32, body).map_err(|e| format!("Compiling function #{f} failed: {e}"))?;
        m.codes.push(code);
    }
    Ok(m)
}

fn elem_seg(s: &mut Reader) -> Res<ElemSeg> {
    let flag = s.u32()?;
    let funcs = |s: &mut Reader| -> Res<Vec<Vec<ConstOp>>> { (0..s.u32()?).map(|_| Ok(vec![ConstOp::RefFunc(s.u32()?)])).collect() };
    let exprs = |s: &mut Reader| -> Res<Vec<Vec<ConstOp>>> { (0..s.u32()?).map(|_| const_expr(s)).collect() };
    Ok(match flag {
        0 => {
            let offset = const_expr(s)?;
            ElemSeg { mode: SegMode::Active { index: 0, offset }, ty: ValType::FuncRef, items: funcs(s)? }
        }
        1 => {
            s.byte()?;
            ElemSeg { mode: SegMode::Passive, ty: ValType::FuncRef, items: funcs(s)? }
        }
        2 => {
            let index = s.u32()?;
            let offset = const_expr(s)?;
            s.byte()?;
            ElemSeg { mode: SegMode::Active { index, offset }, ty: ValType::FuncRef, items: funcs(s)? }
        }
        3 => {
            s.byte()?;
            ElemSeg { mode: SegMode::Declarative, ty: ValType::FuncRef, items: funcs(s)? }
        }
        4 => {
            let offset = const_expr(s)?;
            ElemSeg { mode: SegMode::Active { index: 0, offset }, ty: ValType::FuncRef, items: exprs(s)? }
        }
        5 => {
            let ty = s.valtype()?;
            ElemSeg { mode: SegMode::Passive, ty, items: exprs(s)? }
        }
        6 => {
            let index = s.u32()?;
            let offset = const_expr(s)?;
            let ty = s.valtype()?;
            ElemSeg { mode: SegMode::Active { index, offset }, ty, items: exprs(s)? }
        }
        7 => {
            let ty = s.valtype()?;
            ElemSeg { mode: SegMode::Declarative, ty, items: exprs(s)? }
        }
        _ => return Err("invalid element segment flags".into()),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Block,
    Loop,
    If,
    Else,
}

struct Ctl {
    kind: BlockKind,
    /// Operand stack length below the block's parameters.
    base: usize,
    nparams: u32,
    nresults: u32,
    /// Loop start.
    start: u32,
    /// Branches to the block's end, patched when it closes.
    fixups: Vec<Fixup>,
    /// The `if` op to point at the else branch.
    if_op: Option<usize>,
    unreachable: bool,
}

enum Fixup {
    Op(usize),
    Br(usize),
}

/// A compile-time operand: the value is in a slot (its own stack slot, or
/// a local not yet copied) or a constant not yet materialized.
#[derive(Clone, Copy, PartialEq)]
enum V {
    S(u32),
    C(u64),
}

fn is_unary(c: u16) -> bool {
    matches!(c, 0x45 | 0x50 | 0x67..=0x69 | 0x79..=0x7B | 0x8B..=0x91 | 0x99..=0x9F | 0xA7..=0xC4 | 0x100..=0x107)
}
fn is_binary(c: u16) -> bool {
    matches!(c, 0x46..=0x4F | 0x51..=0x66 | 0x6A..=0x78 | 0x7C..=0x8A | 0x92..=0x98 | 0xA0..=0xA6)
}
fn is_i32_bin(c: u16) -> bool {
    matches!(c, 0x46..=0x4F | 0x6A..=0x78)
}
fn is_i64_bin(c: u16) -> bool {
    matches!(c, 0x51..=0x5A | 0x7C..=0x8A)
}
fn is_i32_cmp(c: u16) -> bool {
    matches!(c, 0x46..=0x4F)
}
/// Ops whose only effect is writing slot `a` (so the result can be
/// redirected straight into a local).
fn writes_a(c: u16) -> bool {
    matches!(c, 0x23 | 0x25 | 0x28..=0x35 | 0x3F | 0x40 | 0xD2 | 0x110 | OP_COPY | OP_CONST) || is_unary(c) || is_binary(c) || (c >= IMM && is_binary(c - IMM))
}

struct Cx {
    ops: Vec<Op>,
    brs: Vec<BrEntry>,
    vs: Vec<V>,
    ctl: Vec<Ctl>,
    nlocals: u32,
    max: u32,
    /// Index of the first op after the latest branch target.
    label: usize,
}

impl Cx {
    fn h(&self) -> u32 {
        self.nlocals + self.vs.len() as u32
    }
    fn note(&mut self, slot: u32) {
        self.max = self.max.max(slot + 1);
    }
    fn emit(&mut self, code: u16, k: u16, a: u32, b: u32, c: u32) {
        self.ops.push(Op { code, k, a, b, c });
    }
    fn konst(&mut self, dst: u32, v: u64) {
        self.note(dst);
        self.emit(OP_CONST, 0, dst, v as u32, (v >> 32) as u32);
    }
    fn materialize(&mut self, i: usize) {
        let want = self.nlocals + i as u32;
        match self.vs[i] {
            V::S(s) if s == want => {}
            V::S(s) => self.emit(OP_COPY, 0, want, s, 0),
            V::C(v) => self.konst(want, v),
        }
        self.vs[i] = V::S(want);
        self.note(want);
    }
    fn materialize_all(&mut self) {
        for i in 0..self.vs.len() {
            self.materialize(i);
        }
    }
    fn materialize_top(&mut self, n: usize) -> Res<()> {
        self.need(n)?;
        for i in self.vs.len() - n..self.vs.len() {
            self.materialize(i);
        }
        Ok(())
    }
    /// Copy out pending reads of local `l` before it changes.
    fn materialize_local(&mut self, l: u32) {
        for i in 0..self.vs.len() {
            if self.vs[i] == V::S(l) {
                self.materialize(i);
            }
        }
    }
    fn top(&self) -> &Ctl {
        self.ctl.last().unwrap()
    }
    /// Make sure `n` operands are available (unreachable code may pop
    /// from an empty stack).
    fn need(&mut self, n: usize) -> Res<()> {
        let base = self.top().base;
        while self.vs.len() < base + n {
            if !self.top().unreachable {
                return Err("type mismatch: not enough operands".into());
            }
            let s = self.h();
            self.note(s);
            self.vs.insert(base, V::S(s));
            // (Dummy slots are only ever seen by code that can't run.)
        }
        Ok(())
    }
    fn pop(&mut self) -> Res<V> {
        self.need(1)?;
        Ok(self.vs.pop().unwrap())
    }
    /// Pop into a slot; a constant goes to the popped value's own slot.
    fn pop_slot(&mut self) -> Res<u32> {
        Ok(match self.pop()? {
            V::S(s) => s,
            V::C(v) => {
                let tmp = self.h();
                self.konst(tmp, v);
                tmp
            }
        })
    }
    fn push_result(&mut self) -> u32 {
        let s = self.h();
        self.note(s);
        self.vs.push(V::S(s));
        s
    }
    fn last_writes(&self, slot: u32) -> Option<Op> {
        let op = *self.ops.last()?;
        (self.ops.len() > self.label && op.a == slot && writes_a(op.code)).then_some(op)
    }
    fn unary(&mut self, code: u16) -> Res<()> {
        let x = self.pop_slot()?;
        let dst = self.h();
        self.emit(code, 0, dst, x, 0);
        self.push_result();
        Ok(())
    }
    fn binary(&mut self, code: u16) -> Res<()> {
        let y = self.pop()?;
        let x = self.pop()?;
        let dst = self.h();
        self.note(dst + 1);
        let xs = match x {
            V::S(s) => s,
            V::C(v) => {
                self.konst(dst, v);
                dst
            }
        };
        let fits = |v: u64| is_i32_bin(code) || (is_i64_bin(code) && v as i64 == v as i32 as i64);
        match y {
            V::C(v) if fits(v) => self.emit(IMM + code, 0, dst, xs, v as u32),
            V::C(v) => {
                self.konst(dst + 1, v);
                self.emit(code, 0, dst, xs, dst + 1);
            }
            V::S(s) => self.emit(code, 0, dst, xs, s),
        }
        self.push_result();
        Ok(())
    }
    fn set_local(&mut self, l: u32, tee: bool) -> Res<()> {
        let v = self.pop()?;
        self.materialize_local(l);
        let h = self.h();
        match v {
            V::S(s) if s == l => {}
            V::S(s) if s == h && self.last_writes(s).is_some() => self.ops.last_mut().unwrap().a = l,
            V::S(s) => self.emit(OP_COPY, 0, l, s, 0),
            V::C(c) => self.konst(l, c),
        }
        if tee {
            self.vs.push(V::S(l));
        }
        Ok(())
    }
    /// The branch entry for label depth `d`, values taken from the top
    /// of the (materialized) stack.
    fn branch(&mut self, d: u32) -> Res<usize> {
        let n = self.ctl.len();
        let i = n.checked_sub(1 + d as usize).ok_or("unknown label")?;
        let idx = self.brs.len();
        let c = &mut self.ctl[i];
        let (target, keep) = if c.kind == BlockKind::Loop {
            (c.start, c.nparams)
        } else {
            c.fixups.push(Fixup::Br(idx));
            (0, c.nresults)
        };
        let dst = self.nlocals + c.base as u32;
        self.need(keep as usize)?;
        let src = self.h() - keep;
        self.brs.push(BrEntry { target, keep, src, dst });
        Ok(idx)
    }
    fn mark_unreachable(&mut self) {
        let c = self.ctl.last_mut().unwrap();
        c.unreachable = true;
        let base = c.base;
        self.vs.truncate(base);
    }
    fn push_canonical(&mut self, n: u32) {
        for _ in 0..n {
            self.push_result();
        }
    }
}

/// Stack effect of the memory ops (pops, pushes).
fn is_load(c: u16) -> bool {
    (0x28..=0x35).contains(&c)
}

/// Compile one function body.
fn compile(m: &Module, f: u32, body: &[u8]) -> Res<Code> {
    let ft = m.func_type(f);
    let nparams = ft.params.len() as u32;
    let nresults = ft.results.len() as u32;
    let mut r = Reader { d: body, p: 0 };
    let mut nlocals = nparams as u64;
    for _ in 0..r.u32()? {
        let n = r.u32()? as u64;
        r.valtype()?;
        nlocals += n;
        if nlocals > 50_000 {
            return Err("too many locals".into());
        }
    }
    let nlocals = nlocals as u32;
    let nfuncs = m.func_types.len() as u32;
    let nglobals = (m.global_imports.len() + m.globals.len()) as u32;
    let mut cx = Cx { ops: Vec::with_capacity(body.len() / 2), brs: Vec::new(), vs: Vec::new(), ctl: Vec::new(), nlocals, max: nlocals + 1, label: 0 };
    cx.ctl.push(Ctl { kind: BlockKind::Block, base: 0, nparams: 0, nresults, start: 0, fixups: Vec::new(), if_op: None, unreachable: false });
    let block_type = |r: &mut Reader| -> Res<(u32, u32)> {
        let b = r.d.get(r.p).copied().ok_or("unexpected end")?;
        if b == 0x40 {
            r.p += 1;
            return Ok((0, 0));
        }
        if valtype(b).is_ok() {
            r.p += 1;
            return Ok((0, 1));
        }
        let t = r.sleb(33)?;
        let ft = m.types.get(t as usize).ok_or("unknown type")?;
        Ok((ft.params.len() as u32, ft.results.len() as u32))
    };

    while !cx.ctl.is_empty() {
        let byte = r.byte()?;
        let mut code = byte as u16;
        if byte == 0xFC {
            code = 0x100 + r.u32()? as u16;
        }
        match code {
            0x00 => {
                cx.emit(0x00, 0, 0, 0, 0);
                cx.mark_unreachable();
            }
            0x01 => {}
            0x02 | 0x03 => {
                let (np, nr) = block_type(&mut r)?;
                cx.need(np as usize)?;
                cx.materialize_all();
                let base = cx.vs.len() - np as usize;
                let start = cx.ops.len() as u32;
                if code == 0x03 {
                    cx.label = cx.ops.len();
                }
                cx.ctl.push(Ctl {
                    kind: if code == 0x02 { BlockKind::Block } else { BlockKind::Loop },
                    base,
                    nparams: np,
                    nresults: nr,
                    start,
                    fixups: Vec::new(),
                    if_op: None,
                    unreachable: false,
                });
            }
            0x04 => {
                let (np, nr) = block_type(&mut r)?;
                let cond = cx.pop()?;
                cx.need(np as usize)?;
                let h = cx.h();
                // A compare feeding the `if` directly becomes one op.
                let fused = match cond {
                    V::S(s) if s == h => cx.last_writes(s).filter(|op| is_i32_cmp(op.code) || (op.code >= IMM && is_i32_cmp(op.code - IMM))),
                    _ => None,
                };
                let at = cx.ops.len();
                match (fused, cond) {
                    (Some(op), _) => {
                        let fop = if op.code >= IMM { Op { code: OP_IF_CMPI + (op.code - IMM - 0x46), k: 0, a: 0, b: op.b, c: op.c } } else { Op { code: OP_IF_CMP + (op.code - 0x46), k: 0, a: 0, b: op.b, c: op.c } };
                        *cx.ops.last_mut().unwrap() = fop;
                        cx.materialize_all();
                        if cx.ops.len() != at {
                            // (Copies were needed after the compare: keep
                            // them before the branch.)
                            let tail: Vec<Op> = cx.ops.drain(at - 1..).collect();
                            let (head, rest) = tail.split_first().unwrap();
                            cx.ops.extend_from_slice(rest);
                            cx.ops.push(*head);
                        }
                    }
                    (None, V::S(s)) => {
                        cx.materialize_all();
                        cx.emit(0x04, 0, 0, s, 0);
                    }
                    (None, V::C(v)) => {
                        cx.materialize_all();
                        let s = cx.h();
                        cx.konst(s, v);
                        cx.emit(0x04, 0, 0, s, 0);
                    }
                }
                let base = cx.vs.len() - np as usize;
                let if_op = Some(cx.ops.len() - 1);
                cx.ctl.push(Ctl { kind: BlockKind::If, base, nparams: np, nresults: nr, start: 0, fixups: Vec::new(), if_op, unreachable: false });
            }
            0x05 => {
                if cx.top().kind != BlockKind::If {
                    return Err("else without if".into());
                }
                if !cx.top().unreachable {
                    cx.materialize_all();
                }
                let j = cx.ops.len();
                cx.emit(OP_JUMP, 0, 0, 0, 0);
                let at = cx.ops.len() as u32;
                cx.label = cx.ops.len();
                let c = cx.ctl.last_mut().unwrap();
                c.fixups.push(Fixup::Op(j));
                let i = c.if_op.take().unwrap();
                c.kind = BlockKind::Else;
                c.unreachable = false;
                let (base, np) = (c.base, c.nparams);
                cx.ops[i].a = at;
                cx.vs.truncate(base);
                cx.push_canonical(np);
            }
            0x0B => {
                if !cx.top().unreachable {
                    let n = cx.top().nresults as usize;
                    cx.need(n)?;
                    cx.materialize_all();
                }
                let c = cx.ctl.pop().unwrap();
                let end = cx.ops.len() as u32;
                if let Some(i) = c.if_op {
                    // No else: a false condition skips to the end.
                    cx.ops[i].a = end;
                }
                for fx in c.fixups {
                    match fx {
                        Fixup::Op(i) => cx.ops[i].a = end,
                        Fixup::Br(i) => cx.brs[i].target = end,
                    }
                }
                cx.label = cx.ops.len();
                cx.vs.truncate(c.base);
                cx.push_canonical(c.nresults);
                if cx.ctl.is_empty() {
                    cx.emit(0x0F, nresults as u16, nlocals, 0, 0);
                }
            }
            0x0C => {
                let d = r.u32()?;
                cx.materialize_all();
                let i = cx.branch(d)?;
                cx.emit(0x0C, 0, i as u32, 0, 0);
                cx.mark_unreachable();
            }
            0x0D => {
                let d = r.u32()?;
                let cond = cx.pop()?;
                let h = cx.h();
                let fused = match cond {
                    V::S(s) if s == h => cx.last_writes(s).filter(|op| op.code == 0x45 || is_i32_cmp(op.code) || (op.code >= IMM && is_i32_cmp(op.code - IMM))),
                    _ => None,
                };
                if let Some(op) = fused {
                    let at = cx.ops.len();
                    cx.materialize_all();
                    if cx.ops.len() == at {
                        cx.ops.pop();
                        let i = cx.branch(d)? as u32;
                        let fop = match op.code {
                            0x45 => Op { code: OP_BR_EQZ, k: 0, a: i, b: op.b, c: 0 },
                            c if c >= IMM => Op { code: OP_BR_CMPI + (c - IMM - 0x46), k: 0, a: i, b: op.b, c: op.c },
                            c => Op { code: OP_BR_CMP + (c - 0x46), k: 0, a: i, b: op.b, c: op.c },
                        };
                        cx.ops.push(fop);
                        continue;
                    }
                    let i = cx.branch(d)? as u32;
                    cx.emit(0x0D, 0, i, h, 0);
                    continue;
                }
                match cond {
                    V::C(v) => {
                        if v as u32 != 0 {
                            cx.materialize_all();
                            let i = cx.branch(d)? as u32;
                            cx.emit(0x0C, 0, i, 0, 0);
                        }
                    }
                    V::S(s) => {
                        cx.materialize_all();
                        let i = cx.branch(d)? as u32;
                        cx.emit(0x0D, 0, i, s, 0);
                    }
                }
            }
            0x0E => {
                let n = r.u32()?;
                let idx = cx.pop_slot()?;
                cx.materialize_all();
                let first = cx.brs.len() as u32;
                for _ in 0..=n {
                    let d = r.u32()?;
                    cx.branch(d)?;
                }
                cx.emit(0x0E, 0, idx, first, n);
                cx.mark_unreachable();
            }
            0x0F => {
                cx.need(nresults as usize)?;
                cx.materialize_all();
                let src = cx.h() - nresults;
                cx.emit(0x0F, nresults as u16, src, 0, 0);
                cx.mark_unreachable();
            }
            0x10 => {
                let fi = r.u32()?;
                if fi >= nfuncs {
                    return Err("unknown function".into());
                }
                let t = m.func_type(fi);
                let (np, nr) = (t.params.len(), t.results.len() as u32);
                cx.materialize_top(np)?;
                let base = cx.h() - np as u32;
                cx.emit(0x10, 0, fi, base, 0);
                cx.vs.truncate(cx.vs.len() - np);
                cx.push_canonical(nr);
            }
            0x11 => {
                let ti = r.u32()?;
                let table = r.u32()?;
                if table as usize >= m.tables.len() {
                    return Err("unknown table".into());
                }
                let t = m.types.get(ti as usize).ok_or("unknown type")?;
                let (np, nr) = (t.params.len(), t.results.len() as u32);
                cx.materialize_top(np + 1)?;
                let base = cx.h() - np as u32 - 1;
                cx.emit(0x11, 0, ti, base, table);
                cx.vs.truncate(cx.vs.len() - np - 1);
                cx.push_canonical(nr);
            }
            0x1A => {
                cx.pop()?;
            }
            0x1B | 0x1C => {
                if code == 0x1C {
                    for _ in 0..r.u32()? {
                        r.valtype()?;
                    }
                }
                cx.materialize_top(3)?;
                let base = cx.h() - 3;
                cx.emit(0x1B, 0, base, 0, 0);
                cx.vs.truncate(cx.vs.len() - 3);
                cx.push_result();
            }
            0x20..=0x22 => {
                let l = r.u32()?;
                if l >= nlocals {
                    return Err("unknown local".into());
                }
                match code {
                    0x20 => cx.vs.push(V::S(l)),
                    0x21 => cx.set_local(l, false)?,
                    _ => cx.set_local(l, true)?,
                }
            }
            0x23 => {
                let g = r.u32()?;
                if g >= nglobals {
                    return Err("unknown global".into());
                }
                let dst = cx.h();
                cx.emit(0x23, 0, dst, g, 0);
                cx.push_result();
            }
            0x24 => {
                let g = r.u32()?;
                if g >= nglobals {
                    return Err("unknown global".into());
                }
                let s = cx.pop_slot()?;
                cx.emit(0x24, 0, g, s, 0);
            }
            0x25 => {
                let t = r.u32()?;
                if t as usize >= m.tables.len() {
                    return Err("unknown table".into());
                }
                let i = cx.pop_slot()?;
                let dst = cx.h();
                cx.emit(0x25, 0, dst, i, t);
                cx.push_result();
            }
            0x26 => {
                let t = r.u32()?;
                if t as usize >= m.tables.len() {
                    return Err("unknown table".into());
                }
                let v = cx.pop()?;
                let i = cx.pop()?;
                let h = cx.h();
                cx.note(h + 1);
                let is = match i {
                    V::S(s) => s,
                    V::C(c) => {
                        cx.konst(h, c);
                        h
                    }
                };
                let vs_ = match v {
                    V::S(s) => s,
                    V::C(c) => {
                        cx.konst(h + 1, c);
                        h + 1
                    }
                };
                cx.emit(0x26, 0, t, is, vs_);
            }
            0x28..=0x3E => {
                if m.memories.is_empty() {
                    return Err("unknown memory 0".into());
                }
                let align = r.u32()?;
                if align & 0x40 != 0 {
                    return Err("multiple memories are not supported".into());
                }
                let offset = r.u32()?;
                if is_load(code) {
                    let a = cx.pop_slot()?;
                    let dst = cx.h();
                    cx.emit(code, 0, dst, a, offset);
                    cx.push_result();
                } else {
                    let v = cx.pop()?;
                    let a = cx.pop()?;
                    let h = cx.h();
                    cx.note(h + 1);
                    let as_ = match a {
                        V::S(s) => s,
                        V::C(c) => {
                            cx.konst(h, c);
                            h
                        }
                    };
                    let vs_ = match v {
                        V::S(s) => s,
                        V::C(c) => {
                            cx.konst(h + 1, c);
                            h + 1
                        }
                    };
                    cx.emit(code, 0, as_, vs_, offset);
                }
            }
            0x3F => {
                if r.byte()? != 0 {
                    return Err("multiple memories are not supported".into());
                }
                let dst = cx.h();
                cx.emit(0x3F, 0, dst, 0, 0);
                cx.push_result();
            }
            0x40 => {
                if r.byte()? != 0 {
                    return Err("multiple memories are not supported".into());
                }
                cx.unary(0x40)?;
            }
            0x41 => {
                let v = r.i32()?;
                cx.vs.push(V::C(v as u32 as u64));
            }
            0x42 => {
                let v = r.i64()?;
                cx.vs.push(V::C(v as u64));
            }
            0x43 => {
                let v = u32::from_le_bytes(r.bytes(4)?.try_into().unwrap());
                cx.vs.push(V::C(v as u64));
            }
            0x44 => {
                let v = u64::from_le_bytes(r.bytes(8)?.try_into().unwrap());
                cx.vs.push(V::C(v));
            }
            0xD0 => {
                r.byte()?;
                cx.vs.push(V::C(0));
            }
            0xD1 => cx.unary(0x50)?,
            0xD2 => {
                let fi = r.u32()?;
                if fi >= nfuncs {
                    return Err("unknown function".into());
                }
                let dst = cx.h();
                cx.emit(0xD2, 0, dst, fi, 0);
                cx.push_result();
            }
            // Bulk memory / table ops: operands materialized at `a`.
            0x108 => {
                let seg = r.u32()?;
                r.byte()?;
                if seg as usize >= m.datas.len() {
                    return Err("unknown data segment".into());
                }
                cx.materialize_top(3)?;
                let base = cx.h() - 3;
                cx.emit(code, 0, base, seg, 0);
                cx.vs.truncate(cx.vs.len() - 3);
            }
            0x109 => {
                let seg = r.u32()?;
                if seg as usize >= m.datas.len() {
                    return Err("unknown data segment".into());
                }
                cx.emit(code, 0, 0, seg, 0);
            }
            0x10D => {
                let seg = r.u32()?;
                if seg as usize >= m.elems.len() {
                    return Err("unknown elem segment".into());
                }
                cx.emit(code, 0, 0, seg, 0);
            }
            0x10A | 0x10B => {
                r.byte()?;
                if code == 0x10A {
                    r.byte()?;
                }
                cx.materialize_top(3)?;
                let base = cx.h() - 3;
                cx.emit(code, 0, base, 0, 0);
                cx.vs.truncate(cx.vs.len() - 3);
            }
            0x10C | 0x10E => {
                let x = r.u32()?;
                let y = r.u32()?;
                if code == 0x10C && (x as usize >= m.elems.len() || y as usize >= m.tables.len()) {
                    return Err("unknown table or elem segment".into());
                }
                if code == 0x10E && (x as usize >= m.tables.len() || y as usize >= m.tables.len()) {
                    return Err("unknown table".into());
                }
                cx.materialize_top(3)?;
                let base = cx.h() - 3;
                cx.emit(code, 0, base, x, y);
                cx.vs.truncate(cx.vs.len() - 3);
            }
            0x10F => {
                let t = r.u32()?;
                if t as usize >= m.tables.len() {
                    return Err("unknown table".into());
                }
                cx.materialize_top(2)?;
                let base = cx.h() - 2;
                cx.emit(code, 0, base, t, 0);
                cx.vs.truncate(cx.vs.len() - 2);
                cx.push_result();
            }
            0x110 => {
                let t = r.u32()?;
                if t as usize >= m.tables.len() {
                    return Err("unknown table".into());
                }
                let dst = cx.h();
                cx.emit(code, 0, dst, t, 0);
                cx.push_result();
            }
            0x111 => {
                let t = r.u32()?;
                if t as usize >= m.tables.len() {
                    return Err("unknown table".into());
                }
                cx.materialize_top(3)?;
                let base = cx.h() - 3;
                cx.emit(code, 0, base, t, 0);
                cx.vs.truncate(cx.vs.len() - 3);
            }
            0xFD => return Err("SIMD is not supported".into()),
            0x06..=0x09 | 0x18 | 0x19 | 0x1F => return Err("exception handling is not supported".into()),
            c if is_unary(c) => cx.unary(c)?,
            c if is_binary(c) => cx.binary(c)?,
            c => return Err(format!("invalid opcode 0x{c:x}")),
        }
    }
    if r.p != body.len() {
        return Err("operators remaining after end of function".into());
    }
    Ok(Code { nlocals, nparams, nresults, max_height: cx.max + 4, ops: cx.ops, brs: cx.brs })
}


#[cfg(test)]
mod tests {
    use crate::dom::Document;
    use crate::js::Interp;

    /// add(i32, i32), fac(i64) (recursive, if/else), sum(n) (block/loop/br_if).
    const MODULE: &str = "0,97,115,109,1,0,0,0,\
        1,17,3,96,2,127,127,1,127,96,1,126,1,126,96,1,127,1,127,\
        3,4,3,0,1,2,\
        7,19,3,3,97,100,100,0,0,3,102,97,99,0,1,3,115,117,109,0,2,\
        10,65,3,\
        7,0,32,0,32,1,106,11,\
        21,0,32,0,80,4,126,66,1,5,32,0,32,0,66,1,125,16,1,126,11,11,\
        33,1,1,127,2,64,3,64,32,0,69,13,1,32,1,32,0,106,33,1,32,0,65,1,107,33,0,12,0,11,11,32,1,11";

    #[test]
    fn instantiate_and_call() {
        let mut it = Interp::new(Document::new(), "https://example.com/", 800.0, 600.0);
        let src = format!(
            "var m = new WebAssembly.Module(new Uint8Array([{MODULE}]));\
             var e = new WebAssembly.Instance(m, {{}}).exports;\
             console.log(WebAssembly.validate(new Uint8Array([{MODULE}])), e.add(2, 40), e.add(0x7fffffff, 1), String(e.fac(20n)), e.sum(100000));\
             try {{ e.fac(1); }} catch (x) {{ console.log(x instanceof TypeError); }}\
             try {{ new WebAssembly.Module(new Uint8Array([1, 2, 3])); }} catch (x) {{ console.log(x instanceof WebAssembly.CompileError); }}"
        );
        if let Err(e) = it.run_script(&src) {
            panic!("script failed: {e}");
        }
        assert_eq!(it.console, vec!["true 42 -2147483648 2432902008176640000 705082704", "true", "true"]);
    }
}
