//! Bytecode VM: functions are compiled from the AST to register-machine
//! bytecode on their first call and run here instead of by the tree-walking
//! interpreter.
//!
//! Locals live in a flat register file. A variable lives in a scope object
//! (`Env`, looked up by name) only when something else must find it by name:
//! a nested function or class that captures it, or an AST "island" — a
//! construct the compiler doesn't handle, evaluated by the tree-walking
//! interpreter against the current scope. Function objects stay ordinary
//! closures, so generators / async functions (coroutines), classes, Proxy and
//! the DOM bindings work unchanged.

use super::parser::*;
use super::*;
use crate::fx::FxHashSet;

pub type Reg = u16;

/// Fixed registers of every frame.
const R_THIS: Reg = 0;
const R_CALLEE: Reg = 1;
const R_ARGUMENTS: Reg = 2;
const R_REST: Reg = 3;
const R_ARG0: Reg = 4;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    StrictEq,
    StrictNe,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    In,
    InstanceOf,
}

impl BinOp {
    fn from_str(s: &str) -> Option<BinOp> {
        Some(match s {
            "+" => BinOp::Add,
            "-" => BinOp::Sub,
            "*" => BinOp::Mul,
            "/" => BinOp::Div,
            "%" => BinOp::Mod,
            "**" => BinOp::Pow,
            "<" => BinOp::Lt,
            ">" => BinOp::Gt,
            "<=" => BinOp::Le,
            ">=" => BinOp::Ge,
            "==" => BinOp::Eq,
            "!=" => BinOp::Ne,
            "===" => BinOp::StrictEq,
            "!==" => BinOp::StrictNe,
            "&" => BinOp::BitAnd,
            "|" => BinOp::BitOr,
            "^" => BinOp::BitXor,
            "<<" => BinOp::Shl,
            ">>" => BinOp::Shr,
            ">>>" => BinOp::UShr,
            "in" => BinOp::In,
            "instanceof" => BinOp::InstanceOf,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::Le => "<=",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::StrictEq => "===",
            BinOp::StrictNe => "!==",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::UShr => ">>>",
            BinOp::In => "in",
            BinOp::InstanceOf => "instanceof",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum UnOp {
    Not,
    Neg,
    Plus,
    BitNot,
    Typeof,
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Const { dst: Reg, k: u32 },
    Undef { dst: Reg },
    Move { dst: Reg, src: Reg },
    /// `this` of an arrow function (from the scope chain).
    ThisEnv { dst: Reg },
    GetName { dst: Reg, name: u32 },
    SetName { name: u32, src: Reg },
    /// Declare in the current scope object.
    DeclName { name: u32, src: Reg },
    TypeofName { dst: Reg, name: u32 },
    Bin { op: BinOp, dst: Reg, a: Reg, b: Reg },
    Un { op: UnOp, dst: Reg, src: Reg },
    /// dst = ToNumeric(src) + delta (for ++ / --).
    Step { dst: Reg, src: Reg, delta: i8 },
    ToNumeric { dst: Reg, src: Reg },
    Jump { to: u32 },
    /// A backward jump (loops): counts toward the script time limit.
    Loop { to: u32 },
    JumpIf { cond: Reg, to: u32 },
    JumpIfNot { cond: Reg, to: u32 },
    JumpIfNullish { src: Reg, to: u32 },
    JumpIfNotNullish { src: Reg, to: u32 },
    JumpIfNotUndef { src: Reg, to: u32 },
    GetProp { dst: Reg, obj: Reg, name: u32 },
    SetProp { obj: Reg, name: u32, src: Reg },
    GetIndex { dst: Reg, obj: Reg, key: Reg },
    SetIndex { obj: Reg, key: Reg, src: Reg },
    DelProp { dst: Reg, obj: Reg, name: u32 },
    DelIndex { dst: Reg, obj: Reg, key: Reg },
    Call { dst: Reg, f: Reg, this: Reg, args: Reg, argc: u16 },
    CallArr { dst: Reg, f: Reg, this: Reg, arr: Reg },
    New { dst: Reg, f: Reg, args: Reg, argc: u16 },
    NewArr { dst: Reg, f: Reg, arr: Reg },
    Array { dst: Reg, start: Reg, count: u16 },
    ArrayPush { arr: Reg, src: Reg },
    ArraySpread { arr: Reg, src: Reg },
    ArrayHole { arr: Reg },
    Object { dst: Reg },
    DefProp { obj: Reg, name: u32, src: Reg, method: bool },
    DefPropKey { obj: Reg, key: Reg, src: Reg, method: bool },
    DefAccessor { obj: Reg, key: Reg, func: Reg, getter: bool },
    ObjSpread { obj: Reg, src: Reg },
    Closure { dst: Reg, func: u32 },
    Class { dst: Reg, class: u32 },
    Regex { dst: Reg, idx: u32 },
    SetFnName { f: Reg, name: u32 },
    /// String concatenation of `count` registers (template literals).
    Concat { dst: Reg, start: Reg, count: u16 },
    /// Evaluate an AST expression against the current scope.
    Island { dst: Reg, expr: u32 },
    /// Execute an AST statement; a `return` inside it returns.
    IslandStmt { stmt: u32 },
    /// Bind an AST pattern (declaration kind 0 var / 1 let / 2 const / 3 none).
    BindPattern { pat: u32, src: Reg, kind: u8 },
    /// Materialize an iterable into an array (destructuring).
    ToArray { dst: Reg, src: Reg },
    /// dst = src.slice(from) for an array register.
    Slice { dst: Reg, src: Reg, from: u16 },
    RequireObject { src: Reg },
    Return { src: Reg },
    Throw { src: Reg },
    /// Protect code until TryPop: on an exception go to `target` with the
    /// exception in `exc` (`catch`: false for finally-only handlers).
    TryPush { target: u32, exc: Reg, catch: bool },
    TryPop,
    PushScope,
    PopScope { n: u16 },
    /// Replace the current scope by a copy (fresh per-iteration bindings).
    CopyScope,
    Await { dst: Reg, src: Reg },
    Yield { dst: Reg, src: Reg, delegate: bool },
    /// for-in (keys) / for-of (values) iteration in iterator slot `slot`.
    IterStart { slot: u16, src: Reg, of: bool },
    IterNext { slot: u16, dst: Reg, end: u32 },
    IterClose { slot: u16 },
}

pub struct Code {
    pub def: Rc<FuncDef>,
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    funcs: Vec<Rc<FuncDef>>,
    classes: Vec<Rc<ClassDef>>,
    regexes: Vec<(Rc<str>, Rc<str>)>,
    /// AST nodes evaluated by the interpreter (kept alive by `def`).
    exprs: Vec<*const Expr>,
    stmts: Vec<*const Stmt>,
    pats: Vec<*const Pattern>,
    nregs: usize,
    niters: usize,
    needs_env: bool,
    uses_arguments: bool,
}

enum IterState {
    Keys(Vec<Rc<str>>, usize, Value),
    Values(Vec<Value>, usize),
    Lazy { iter: Value, next: Value },
}

struct Handler {
    target: usize,
    exc: Reg,
    catch: bool,
    envs: usize,
}

// ------------------------------------------------------------------ analysis

/// Every identifier mentioned anywhere in an expression / statement.
fn idents_expr(e: &Expr, out: &mut FxHashSet<Rc<str>>) {
    let items = |v: &[ArrItem], out: &mut FxHashSet<Rc<str>>| {
        for i in v {
            if let ArrItem::Item(e) | ArrItem::Spread(e) = i {
                idents_expr(e, out);
            }
        }
    };
    match e {
        Expr::Ident(n) => {
            out.insert(n.clone());
        }
        Expr::Num(_) | Expr::BigInt(_) | Expr::Str(_) | Expr::Regex(..) | Expr::This | Expr::Super | Expr::Null | Expr::Bool(_) | Expr::ImportMeta => {}
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().for_each(|x| idents_expr(x, out)),
        Expr::TaggedTemplate(t, _, xs) => {
            idents_expr(t, out);
            xs.iter().for_each(|x| idents_expr(x, out));
        }
        Expr::Array(v) => items(v, out),
        Expr::Object(ps) => {
            for p in ps {
                match p {
                    PropDef::Value(k, v) => {
                        idents_key(k, out);
                        idents_expr(v, out);
                    }
                    PropDef::Getter(k, f) | PropDef::Setter(k, f) => {
                        idents_key(k, out);
                        idents_func(f, out);
                    }
                    PropDef::Spread(e) => idents_expr(e, out),
                }
            }
        }
        Expr::Func(f) => idents_func(f, out),
        Expr::Class(c) => idents_class(c, out),
        Expr::Unary(_, x) | Expr::Await(x) | Expr::ImportCall(x) => idents_expr(x, out),
        Expr::Update { target, .. } => idents_expr(target, out),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
            idents_expr(a, out);
            idents_expr(b, out);
        }
        Expr::Assign(_, p, v) => {
            idents_pat(p, out);
            idents_expr(v, out);
        }
        Expr::Cond(a, b, c) => {
            idents_expr(a, out);
            idents_expr(b, out);
            idents_expr(c, out);
        }
        Expr::Call(f, a, _) | Expr::New(f, a) => {
            idents_expr(f, out);
            items(a, out);
        }
        Expr::Member(o, _, _) => idents_expr(o, out),
        Expr::Index(o, k, _) => {
            idents_expr(o, out);
            idents_expr(k, out);
        }
        Expr::Yield(x, _) => {
            if let Some(x) = x {
                idents_expr(x, out);
            }
        }
    }
}

fn idents_key(k: &PropKey, out: &mut FxHashSet<Rc<str>>) {
    if let PropKey::Computed(e) = k {
        idents_expr(e, out);
    }
}

fn idents_pat(p: &Pattern, out: &mut FxHashSet<Rc<str>>) {
    match p {
        Pattern::Ident(n) => {
            out.insert(n.clone());
        }
        Pattern::Expr(e) => idents_expr(e, out),
        Pattern::Object(ps, rest) => {
            for (k, p) in ps {
                idents_key(k, out);
                idents_pat(p, out);
            }
            if let Some(r) = rest {
                idents_pat(r, out);
            }
        }
        Pattern::Array(ps, rest) => {
            for p in ps.iter().flatten() {
                idents_pat(p, out);
            }
            if let Some(r) = rest {
                idents_pat(r, out);
            }
        }
        Pattern::Default(p, d) => {
            idents_pat(p, out);
            idents_expr(d, out);
        }
    }
}

fn idents_func(f: &FuncDef, out: &mut FxHashSet<Rc<str>>) {
    // `arguments` / `this` of an arrow belong to the enclosing function.
    out.insert("arguments".into());
    for p in &f.params {
        idents_pat(p, out);
    }
    if let Some(r) = &f.rest {
        idents_pat(r, out);
    }
    match &f.body {
        Body::Expr(e) => idents_expr(e, out),
        Body::Block(b) => b.iter().for_each(|s| idents_stmt(s, out)),
    }
}

fn idents_class(c: &ClassDef, out: &mut FxHashSet<Rc<str>>) {
    if let Some(e) = &c.extends {
        idents_expr(e, out);
    }
    if let Some(f) = &c.ctor {
        idents_func(f, out);
    }
    for (_, k, _, f) in &c.methods {
        idents_key(k, out);
        idents_func(f, out);
    }
    for (_, k, v) in &c.fields {
        idents_key(k, out);
        if let Some(v) = v {
            idents_expr(v, out);
        }
    }
    if let Some(n) = &c.name {
        out.insert(n.clone());
    }
}

fn idents_stmt(s: &Stmt, out: &mut FxHashSet<Rc<str>>) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => idents_expr(e, out),
        Stmt::Var(_, ds) => {
            for (p, e) in ds {
                idents_pat(p, out);
                if let Some(e) = e {
                    idents_expr(e, out);
                }
            }
        }
        Stmt::Func(f) => {
            if let Some(n) = &f.name {
                out.insert(n.clone());
            }
            idents_func(f, out);
        }
        Stmt::Class(c) => idents_class(c, out),
        Stmt::Return(e) => {
            if let Some(e) = e {
                idents_expr(e, out);
            }
        }
        Stmt::If(c, a, b) => {
            idents_expr(c, out);
            idents_stmt(a, out);
            if let Some(b) = b {
                idents_stmt(b, out);
            }
        }
        Stmt::Block(b) => b.iter().for_each(|s| idents_stmt(s, out)),
        Stmt::For(i, t, u, b) => {
            if let Some(i) = i {
                idents_stmt(i, out);
            }
            if let Some(t) = t {
                idents_expr(t, out);
            }
            if let Some(u) = u {
                idents_expr(u, out);
            }
            idents_stmt(b, out);
        }
        Stmt::ForIn(init, o, b) | Stmt::ForOf(init, o, b) => {
            match init {
                ForInit::Var(_, p) | ForInit::Target(p) => idents_pat(p, out),
            }
            idents_expr(o, out);
            idents_stmt(b, out);
        }
        Stmt::While(c, b) | Stmt::DoWhile(b, c) => {
            idents_expr(c, out);
            idents_stmt(b, out);
        }
        Stmt::Try(a, c, f) => {
            a.iter().for_each(|s| idents_stmt(s, out));
            if let Some((p, c)) = c {
                if let Some(p) = p {
                    idents_pat(p, out);
                }
                c.iter().for_each(|s| idents_stmt(s, out));
            }
            if let Some(f) = f {
                f.iter().for_each(|s| idents_stmt(s, out));
            }
        }
        Stmt::Switch(d, cases) => {
            idents_expr(d, out);
            for (t, b) in cases {
                if let Some(t) = t {
                    idents_expr(t, out);
                }
                b.iter().for_each(|s| idents_stmt(s, out));
            }
        }
        Stmt::Labeled(_, b) => idents_stmt(b, out),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
    }
}

/// Names referenced inside nested functions and classes of a statement list
/// (they must live in a scope object to be captured).
fn captured_stmt(s: &Stmt, out: &mut FxHashSet<Rc<str>>, nested: &mut bool) {
    let mut sub = |e: &Expr, out: &mut FxHashSet<Rc<str>>, nested: &mut bool| captured_expr(e, out, nested);
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => sub(e, out, nested),
        Stmt::Var(_, ds) => {
            for (p, e) in ds {
                captured_pat(p, out, nested);
                if let Some(e) = e {
                    sub(e, out, nested);
                }
            }
        }
        Stmt::Func(f) => {
            *nested = true;
            idents_func(f, out);
        }
        Stmt::Class(c) => {
            *nested = true;
            idents_class(c, out);
        }
        Stmt::Return(e) => {
            if let Some(e) = e {
                sub(e, out, nested);
            }
        }
        Stmt::If(c, a, b) => {
            sub(c, out, nested);
            captured_stmt(a, out, nested);
            if let Some(b) = b {
                captured_stmt(b, out, nested);
            }
        }
        Stmt::Block(b) => b.iter().for_each(|s| captured_stmt(s, out, nested)),
        Stmt::For(i, t, u, b) => {
            if let Some(i) = i {
                captured_stmt(i, out, nested);
            }
            if let Some(t) = t {
                sub(t, out, nested);
            }
            if let Some(u) = u {
                sub(u, out, nested);
            }
            captured_stmt(b, out, nested);
        }
        Stmt::ForIn(init, o, b) | Stmt::ForOf(init, o, b) => {
            match init {
                ForInit::Var(_, p) | ForInit::Target(p) => captured_pat(p, out, nested),
            }
            sub(o, out, nested);
            captured_stmt(b, out, nested);
        }
        Stmt::While(c, b) | Stmt::DoWhile(b, c) => {
            sub(c, out, nested);
            captured_stmt(b, out, nested);
        }
        Stmt::Try(a, c, f) => {
            a.iter().for_each(|s| captured_stmt(s, out, nested));
            if let Some((p, c)) = c {
                if let Some(p) = p {
                    captured_pat(p, out, nested);
                }
                c.iter().for_each(|s| captured_stmt(s, out, nested));
            }
            if let Some(f) = f {
                f.iter().for_each(|s| captured_stmt(s, out, nested));
            }
        }
        Stmt::Switch(d, cases) => {
            sub(d, out, nested);
            for (t, b) in cases {
                if let Some(t) = t {
                    sub(t, out, nested);
                }
                b.iter().for_each(|s| captured_stmt(s, out, nested));
            }
        }
        Stmt::Labeled(_, b) => captured_stmt(b, out, nested),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
    }
}

fn captured_pat(p: &Pattern, out: &mut FxHashSet<Rc<str>>, nested: &mut bool) {
    match p {
        Pattern::Ident(_) => {}
        Pattern::Expr(e) => captured_expr(e, out, nested),
        Pattern::Object(ps, rest) => {
            for (k, p) in ps {
                if let PropKey::Computed(e) = k {
                    captured_expr(e, out, nested);
                }
                captured_pat(p, out, nested);
            }
            if let Some(r) = rest {
                captured_pat(r, out, nested);
            }
        }
        Pattern::Array(ps, rest) => {
            for p in ps.iter().flatten() {
                captured_pat(p, out, nested);
            }
            if let Some(r) = rest {
                captured_pat(r, out, nested);
            }
        }
        Pattern::Default(p, d) => {
            captured_pat(p, out, nested);
            captured_expr(d, out, nested);
        }
    }
}

fn captured_expr(e: &Expr, out: &mut FxHashSet<Rc<str>>, nested: &mut bool) {
    match e {
        Expr::Func(f) => {
            *nested = true;
            idents_func(f, out);
        }
        Expr::Class(c) => {
            *nested = true;
            idents_class(c, out);
        }
        Expr::Ident(_) | Expr::Num(_) | Expr::BigInt(_) | Expr::Str(_) | Expr::Regex(..) | Expr::This | Expr::Super | Expr::Null | Expr::Bool(_) | Expr::ImportMeta => {}
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().for_each(|x| captured_expr(x, out, nested)),
        Expr::TaggedTemplate(t, _, xs) => {
            captured_expr(t, out, nested);
            xs.iter().for_each(|x| captured_expr(x, out, nested));
        }
        Expr::Array(v) | Expr::Call(_, v, _) | Expr::New(_, v) => {
            if let Expr::Call(f, _, _) | Expr::New(f, _) = e {
                captured_expr(f, out, nested);
            }
            for i in v {
                if let ArrItem::Item(e) | ArrItem::Spread(e) = i {
                    captured_expr(e, out, nested);
                }
            }
        }
        Expr::Object(ps) => {
            for p in ps {
                match p {
                    PropDef::Value(k, v) => {
                        if let PropKey::Computed(e) = k {
                            captured_expr(e, out, nested);
                        }
                        captured_expr(v, out, nested);
                    }
                    PropDef::Getter(k, f) | PropDef::Setter(k, f) => {
                        if let PropKey::Computed(e) = k {
                            captured_expr(e, out, nested);
                        }
                        *nested = true;
                        idents_func(f, out);
                    }
                    PropDef::Spread(e) => captured_expr(e, out, nested),
                }
            }
        }
        Expr::Unary(_, x) | Expr::Await(x) | Expr::ImportCall(x) => captured_expr(x, out, nested),
        Expr::Update { target, .. } => captured_expr(target, out, nested),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
            captured_expr(a, out, nested);
            captured_expr(b, out, nested);
        }
        Expr::Assign(_, p, v) => {
            captured_pat(p, out, nested);
            captured_expr(v, out, nested);
        }
        Expr::Cond(a, b, c) => {
            captured_expr(a, out, nested);
            captured_expr(b, out, nested);
            captured_expr(c, out, nested);
        }
        Expr::Member(o, _, _) => captured_expr(o, out, nested),
        Expr::Index(o, k, _) => {
            captured_expr(o, out, nested);
            captured_expr(k, out, nested);
        }
        Expr::Yield(x, _) => {
            if let Some(x) = x {
                captured_expr(x, out, nested);
            }
        }
    }
}

/// Does a statement contain `break` / `continue` that leaves it (labels
/// defined outside, or unlabeled ones outside a loop / switch within it)?
fn jumps_out(s: &Stmt, in_loop: bool, in_switch: bool, labels: &mut Vec<Rc<str>>) -> bool {
    match s {
        Stmt::Break(None) => !(in_loop || in_switch),
        Stmt::Continue(None) => !in_loop,
        Stmt::Break(Some(l)) | Stmt::Continue(Some(l)) => !labels.contains(l),
        Stmt::If(_, a, b) => jumps_out(a, in_loop, in_switch, labels) || b.as_ref().is_some_and(|b| jumps_out(b, in_loop, in_switch, labels)),
        Stmt::Block(b) => b.iter().any(|s| jumps_out(s, in_loop, in_switch, labels)),
        Stmt::For(_, _, _, b) | Stmt::ForIn(_, _, b) | Stmt::ForOf(_, _, b) | Stmt::While(_, b) | Stmt::DoWhile(b, _) => jumps_out(b, true, in_switch, labels),
        Stmt::Try(a, c, f) => {
            a.iter().any(|s| jumps_out(s, in_loop, in_switch, labels))
                || c.as_ref().is_some_and(|(_, c)| c.iter().any(|s| jumps_out(s, in_loop, in_switch, labels)))
                || f.as_ref().is_some_and(|f| f.iter().any(|s| jumps_out(s, in_loop, in_switch, labels)))
        }
        Stmt::Switch(_, cases) => cases.iter().any(|(_, b)| b.iter().any(|s| jumps_out(s, in_loop, true, labels))),
        Stmt::Labeled(l, b) => {
            labels.push(l.clone());
            let r = jumps_out(b, in_loop, in_switch, labels);
            labels.pop();
            r
        }
        _ => false,
    }
}

/// `var` names of a function body (not inside nested functions).
fn var_names(s: &Stmt, out: &mut Vec<Rc<str>>) {
    match s {
        Stmt::Var(VarKind::Var, ds) => {
            for (p, _) in ds {
                pat_names(p, out);
            }
        }
        Stmt::If(_, a, b) => {
            var_names(a, out);
            if let Some(b) = b {
                var_names(b, out);
            }
        }
        Stmt::Block(b) => b.iter().for_each(|s| var_names(s, out)),
        Stmt::For(i, _, _, b) => {
            if let Some(i) = i {
                var_names(i, out);
            }
            var_names(b, out);
        }
        Stmt::ForIn(ForInit::Var(VarKind::Var, p), _, b) | Stmt::ForOf(ForInit::Var(VarKind::Var, p), _, b) => {
            pat_names(p, out);
            var_names(b, out);
        }
        Stmt::ForIn(_, _, b) | Stmt::ForOf(_, _, b) | Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) => var_names(b, out),
        Stmt::Try(a, c, f) => {
            a.iter().for_each(|s| var_names(s, out));
            if let Some((_, c)) = c {
                c.iter().for_each(|s| var_names(s, out));
            }
            if let Some(f) = f {
                f.iter().for_each(|s| var_names(s, out));
            }
        }
        Stmt::Switch(_, cases) => cases.iter().for_each(|(_, b)| b.iter().for_each(|s| var_names(s, out))),
        _ => {}
    }
}

fn uses_this_or_super(e: &FuncDef) -> bool {
    fn ex(e: &Expr) -> bool {
        match e {
            Expr::This | Expr::Super => true,
            Expr::Func(f) => f.arrow && fd(f),
            Expr::Class(_) => true,
            _ => {
                let mut found = false;
                walk_expr_children(e, &mut |c| found |= ex(c));
                found
            }
        }
    }
    fn fd(f: &FuncDef) -> bool {
        match &f.body {
            Body::Expr(e) => ex(e),
            Body::Block(b) => b.iter().any(st),
        }
    }
    fn st(s: &Stmt) -> bool {
        let mut found = false;
        walk_stmt_exprs(s, &mut |e| found |= ex(e));
        found
    }
    fd(e)
}

/// Call `f` on the direct child expressions of `e` (not into functions).
fn walk_expr_children(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    match e {
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().for_each(&mut *f),
        Expr::TaggedTemplate(t, _, xs) => {
            f(t);
            xs.iter().for_each(&mut *f);
        }
        Expr::Array(v) => v.iter().for_each(|i| {
            if let ArrItem::Item(e) | ArrItem::Spread(e) = i {
                f(e)
            }
        }),
        Expr::Object(ps) => {
            for p in ps {
                match p {
                    PropDef::Value(k, v) => {
                        if let PropKey::Computed(e) = k {
                            f(e);
                        }
                        f(v);
                    }
                    PropDef::Spread(e) => f(e),
                    _ => {}
                }
            }
        }
        Expr::Unary(_, x) | Expr::Await(x) | Expr::ImportCall(x) => f(x),
        Expr::Update { target, .. } => f(target),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => {
            f(a);
            f(b);
        }
        Expr::Assign(_, p, v) => {
            if let Pattern::Expr(e) = &**p {
                f(e);
            }
            f(v);
        }
        Expr::Cond(a, b, c) => {
            f(a);
            f(b);
            f(c);
        }
        Expr::Call(c, a, _) | Expr::New(c, a) => {
            f(c);
            a.iter().for_each(|i| {
                if let ArrItem::Item(e) | ArrItem::Spread(e) = i {
                    f(e)
                }
            });
        }
        Expr::Member(o, _, _) => f(o),
        Expr::Index(o, k, _) => {
            f(o);
            f(k);
        }
        Expr::Yield(Some(x), _) => f(x),
        _ => {}
    }
}

fn walk_stmt_exprs(s: &Stmt, f: &mut dyn FnMut(&Expr)) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => f(e),
        Stmt::Var(_, ds) => ds.iter().for_each(|(_, e)| {
            if let Some(e) = e {
                f(e)
            }
        }),
        Stmt::Return(Some(e)) => f(e),
        Stmt::If(c, a, b) => {
            f(c);
            walk_stmt_exprs(a, f);
            if let Some(b) = b {
                walk_stmt_exprs(b, f);
            }
        }
        Stmt::Block(b) => b.iter().for_each(|s| walk_stmt_exprs(s, f)),
        Stmt::For(i, t, u, b) => {
            if let Some(i) = i {
                walk_stmt_exprs(i, f);
            }
            if let Some(t) = t {
                f(t);
            }
            if let Some(u) = u {
                f(u);
            }
            walk_stmt_exprs(b, f);
        }
        Stmt::ForIn(_, o, b) | Stmt::ForOf(_, o, b) => {
            f(o);
            walk_stmt_exprs(b, f);
        }
        Stmt::While(c, b) | Stmt::DoWhile(b, c) => {
            f(c);
            walk_stmt_exprs(b, f);
        }
        Stmt::Try(a, c, fi) => {
            a.iter().for_each(|s| walk_stmt_exprs(s, f));
            if let Some((_, c)) = c {
                c.iter().for_each(|s| walk_stmt_exprs(s, f));
            }
            if let Some(fi) = fi {
                fi.iter().for_each(|s| walk_stmt_exprs(s, f));
            }
        }
        Stmt::Switch(d, cases) => {
            f(d);
            for (t, b) in cases {
                if let Some(t) = t {
                    f(t);
                }
                b.iter().for_each(|s| walk_stmt_exprs(s, f));
            }
        }
        Stmt::Labeled(_, b) => walk_stmt_exprs(b, f),
        Stmt::Class(_) => f(&Expr::This),
        _ => {}
    }
}

// ------------------------------------------------------------------ compiler

#[derive(Clone, Copy)]
enum Binding {
    Reg(Reg),
    /// In the scope object (looked up by name).
    Env,
}

struct Ctl {
    label: Option<Rc<str>>,
    /// Loops accept `continue`; labeled blocks / switch only `break`.
    is_loop: bool,
    breaks: Vec<usize>,
    continues: Vec<usize>,
    envs: usize,
    tries: usize,
    iters: usize,
}

/// An enclosing `finally` (its body is inlined on early exits).
struct Fin<'a> {
    body: &'a [Stmt],
    /// Handler depth at the try statement.
    tries: usize,
    envs: usize,
}

struct Compiler<'a> {
    def: &'a FuncDef,
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    name_ix: FxHashMap<Rc<str>, u32>,
    funcs: Vec<Rc<FuncDef>>,
    classes: Vec<Rc<ClassDef>>,
    regexes: Vec<(Rc<str>, Rc<str>)>,
    exprs: Vec<*const Expr>,
    stmts: Vec<*const Stmt>,
    pats: Vec<*const Pattern>,
    scopes: Vec<FxHashMap<Rc<str>, Binding>>,
    /// Names that must live in scope objects.
    env_names: &'a FxHashSet<Rc<str>>,
    /// Identifiers inside islands (fed back into `env_names`).
    island_names: FxHashSet<Rc<str>>,
    /// Names bound to registers somewhere.
    reg_names: FxHashSet<Rc<str>>,
    next: Reg,
    max: Reg,
    ctl: Vec<Ctl>,
    fins: Vec<Fin<'a>>,
    envs: usize,
    tries: usize,
    iters: usize,
    max_iters: usize,
    pending_label: Option<Rc<str>>,
    uses_arguments: bool,
    failed: bool,
    /// Per compile-time block: whether a runtime scope was pushed.
    scope_pushed: Vec<bool>,
    /// Registers below this hold declared variables (never reused as temps).
    floor: Reg,
}

type CResult = Result<(), ()>;

impl<'a> Compiler<'a> {
    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }

    fn here(&self) -> u32 {
        self.ops.len() as u32
    }

    fn patch(&mut self, at: usize, to: u32) {
        match &mut self.ops[at] {
            Op::Jump { to: t } | Op::Loop { to: t } | Op::JumpIf { to: t, .. } | Op::JumpIfNot { to: t, .. } | Op::JumpIfNullish { to: t, .. } | Op::JumpIfNotNullish { to: t, .. } | Op::JumpIfNotUndef { to: t, .. } => *t = to,
            Op::IterNext { end, .. } => *end = to,
            Op::TryPush { target, .. } => *target = to,
            _ => {}
        }
    }

    fn reg(&mut self) -> Reg {
        let r = self.next;
        self.next += 1;
        if self.next > self.max {
            self.max = self.next;
        }
        if self.next >= u16::MAX - 2 {
            self.failed = true;
        }
        r
    }

    fn regs(&mut self, n: usize) -> Reg {
        let r = self.next;
        for _ in 0..n {
            self.reg();
        }
        r
    }

    fn name(&mut self, n: &Rc<str>) -> u32 {
        if let Some(&i) = self.name_ix.get(n) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(n.clone());
        self.name_ix.insert(n.clone(), i);
        i
    }

    fn konst(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        (self.consts.len() - 1) as u32
    }

    fn lookup(&self, n: &str) -> Option<Binding> {
        for s in self.scopes.iter().rev() {
            if let Some(b) = s.get(n) {
                return Some(*b);
            }
        }
        None
    }

    /// Declare `n` in the innermost scope: a register, or the scope object.
    fn declare(&mut self, n: &Rc<str>) -> Binding {
        if let Some(b) = self.scopes.last().unwrap().get(n) {
            return *b;
        }
        let b = if self.env_names.contains(n) {
            Binding::Env
        } else {
            self.reg_names.insert(n.clone());
            let r = self.reg();
            self.floor = self.floor.max(self.next);
            Binding::Reg(r)
        };
        self.scopes.last_mut().unwrap().insert(n.clone(), b);
        b
    }

    /// Store `src` into a declared binding (let/const/var initialization).
    fn init_binding(&mut self, n: &Rc<str>, b: Binding, src: Reg) {
        match b {
            Binding::Reg(r) => {
                if r != src {
                    self.emit(Op::Move { dst: r, src });
                }
            }
            Binding::Env => {
                let name = self.name(n);
                self.emit(Op::DeclName { name, src });
            }
        }
    }

    /// Assign to an existing variable (or a free / global one).
    fn assign_name(&mut self, n: &Rc<str>, src: Reg) {
        match self.lookup(n) {
            Some(Binding::Reg(r)) => {
                if r != src {
                    self.emit(Op::Move { dst: r, src });
                }
            }
            _ => {
                let name = self.name(n);
                self.emit(Op::SetName { name, src });
            }
        }
    }

    fn island(&mut self, e: &Expr, dst: Reg) {
        idents_expr(e, &mut self.island_names);
        self.island_names.insert("arguments".into());
        self.exprs.push(e as *const Expr);
        let expr = (self.exprs.len() - 1) as u32;
        self.emit(Op::Island { dst, expr });
    }

    // ---------------------------------------------------------- functions

    fn function(&mut self, def: &'a FuncDef) -> CResult {
        self.scopes.push(FxHashMap::default());
        // Fixed registers.
        self.next = R_ARG0 + def.params.len() as Reg;
        self.max = self.next;
        // `arguments` (non-arrow functions).
        let mentions_arguments = {
            let mut ids = FxHashSet::default();
            match &def.body {
                Body::Expr(e) => idents_expr(e, &mut ids),
                Body::Block(b) => b.iter().for_each(|s| idents_stmt(s, &mut ids)),
            }
            def.params.iter().for_each(|p| idents_pat(p, &mut ids));
            ids.contains("arguments")
        };
        if !def.arrow && mentions_arguments {
            self.uses_arguments = true;
            let n: Rc<str> = "arguments".into();
            let b = self.declare(&n);
            self.init_binding(&n, b, R_ARGUMENTS);
        }
        // A named function expression can refer to itself.
        if let Some(name) = &def.name {
            if !def.arrow {
                let b = self.declare(name);
                self.init_binding(name, b, R_CALLEE);
            }
        }
        // Parameters.
        for (i, p) in def.params.iter().enumerate() {
            let src = R_ARG0 + i as Reg;
            self.bind_pattern(p, src, Some(VarKind::Let))?;
        }
        if let Some(r) = &def.rest {
            self.bind_pattern(r, R_REST, Some(VarKind::Let))?;
        }
        match &def.body {
            Body::Expr(e) => {
                let r = self.reg();
                self.expr(e, r)?;
                self.emit(Op::Return { src: r });
            }
            Body::Block(stmts) => {
                // var hoisting.
                let mut vars = Vec::new();
                stmts.iter().for_each(|s| var_names(s, &mut vars));
                for v in vars {
                    if self.scopes.last().unwrap().contains_key(&v) {
                        continue;
                    }
                    let b = self.declare(&v);
                    if let Binding::Env = b {
                        let u = self.reg();
                        self.emit(Op::Undef { dst: u });
                        self.init_binding(&v, b, u);
                    }
                }
                self.hoist_functions(stmts)?;
                for s in stmts {
                    self.stmt(s)?;
                }
                let r = self.reg();
                self.emit(Op::Undef { dst: r });
                self.emit(Op::Return { src: r });
            }
        }
        if self.failed { Err(()) } else { Ok(()) }
    }

    /// Function declarations of a statement list: created on entry.
    fn hoist_functions(&mut self, stmts: &'a [Stmt]) -> CResult {
        for s in stmts {
            if let Stmt::Func(f) = s {
                let name = f.name.clone().ok_or(())?;
                let b = self.declare(&name);
                let r = self.reg();
                self.funcs.push(f.clone());
                let func = (self.funcs.len() - 1) as u32;
                self.emit(Op::Closure { dst: r, func });
                self.init_binding(&name, b, r);
            }
        }
        Ok(())
    }

    /// Declare the lexical names of a block (let / const / class / function).
    fn block_decls(&mut self, stmts: &'a [Stmt]) -> bool {
        let mut any_env = false;
        for s in stmts {
            let mut names = Vec::new();
            match s {
                Stmt::Var(VarKind::Let | VarKind::Const, ds) => ds.iter().for_each(|(p, _)| pat_names(p, &mut names)),
                Stmt::Class(c) => names.extend(c.name.clone()),
                Stmt::Func(f) => names.extend(f.name.clone()),
                _ => {}
            }
            for n in names {
                if self.env_names.contains(&n) {
                    any_env = true;
                }
            }
        }
        any_env
    }

    /// Enter a block scope; returns whether a runtime scope was pushed.
    fn enter_block(&mut self, stmts: &'a [Stmt]) -> CResult {
        let push = self.block_decls(stmts);
        self.scopes.push(FxHashMap::default());
        if push {
            self.emit(Op::PushScope);
            self.envs += 1;
        }
        self.scope_pushed.push(push);
        // Lexical names are declared up front (so closures created earlier
        // in the block see later functions).
        for s in stmts {
            let mut names = Vec::new();
            if let Stmt::Var(VarKind::Let | VarKind::Const, ds) = s {
                ds.iter().for_each(|(p, _)| pat_names(p, &mut names));
            }
            if let Stmt::Class(c) = s {
                names.extend(c.name.clone());
            }
            for n in names {
                self.declare(&n);
            }
        }
        self.hoist_functions(stmts)
    }

    fn leave_block(&mut self) {
        self.scopes.pop();
        if self.scope_pushed.pop().unwrap_or(false) {
            self.emit(Op::PopScope { n: 1 });
            self.envs -= 1;
        }
    }

    // ---------------------------------------------------------- statements

    fn stmts(&mut self, stmts: &'a [Stmt]) -> CResult {
        self.enter_block(stmts)?;
        for s in stmts {
            self.stmt(s)?;
        }
        self.leave_block();
        Ok(())
    }

    fn stmt(&mut self, s: &'a Stmt) -> CResult {
        let mark = self.next;
        let r = self.stmt_inner(s);
        self.next = mark.max(self.floor);
        r
    }

    fn stmt_inner(&mut self, s: &'a Stmt) -> CResult {
        let label = self.pending_label.take();
        match s {
            Stmt::Expr(e) => {
                let r = self.reg();
                self.expr(e, r)?;
            }
            Stmt::Var(kind, decls) => {
                for (p, init) in decls {
                    match init {
                        Some(e) => {
                            let r = self.reg();
                            self.expr(e, r)?;
                            if let (Pattern::Ident(n), Expr::Func(_) | Expr::Class(_)) = (p, e) {
                                let name = self.name(n);
                                self.emit(Op::SetFnName { f: r, name });
                            }
                            self.bind_pattern(p, r, Some(*kind))?;
                        }
                        None if *kind == VarKind::Var => {}
                        None => {
                            let r = self.reg();
                            self.emit(Op::Undef { dst: r });
                            self.bind_pattern(p, r, Some(*kind))?;
                        }
                    }
                }
            }
            Stmt::Func(_) => {}
            Stmt::Class(cd) => {
                let r = self.reg();
                self.classes.push(cd.clone());
                let class = (self.classes.len() - 1) as u32;
                self.emit(Op::Class { dst: r, class });
                let n = cd.name.clone().ok_or(())?;
                let b = self.lookup(&n).unwrap_or(Binding::Env);
                self.init_binding(&n, b, r);
            }
            Stmt::Return(e) => {
                let r = self.reg();
                match e {
                    Some(e) => self.expr(e, r)?,
                    None => {
                        self.emit(Op::Undef { dst: r });
                    }
                }
                self.unwind_all()?;
                self.emit(Op::Return { src: r });
            }
            Stmt::If(c, a, b) => {
                let r = self.reg();
                self.expr(c, r)?;
                let j = self.emit(Op::JumpIfNot { cond: r, to: NONE });
                self.stmt(a)?;
                match b {
                    Some(b) => {
                        let j2 = self.emit(Op::Jump { to: NONE });
                        let h = self.here();
                        self.patch(j, h);
                        self.stmt(b)?;
                        let h = self.here();
                        self.patch(j2, h);
                    }
                    None => {
                        let h = self.here();
                        self.patch(j, h);
                    }
                }
            }
            Stmt::Block(b) => {
                if let Some(l) = label {
                    self.push_ctl(Some(l), false);
                    self.stmts(b)?;
                    self.pop_ctl(None);
                } else {
                    self.stmts(b)?;
                }
            }
            Stmt::For(init, test, update, body) => self.for_loop(label, init.as_deref(), test.as_ref(), update.as_ref(), body)?,
            Stmt::While(c, body) => {
                let top = self.here();
                self.push_ctl(label, true);
                let r = self.reg();
                self.expr(c, r)?;
                let exit = self.emit(Op::JumpIfNot { cond: r, to: NONE });
                self.stmt(body)?;
                self.emit(Op::Loop { to: top });
                let end = self.here();
                self.patch(exit, end);
                self.pop_ctl(Some(top));
            }
            Stmt::DoWhile(body, c) => {
                let top = self.here();
                self.push_ctl(label, true);
                self.stmt(body)?;
                let cont = self.here();
                let r = self.reg();
                self.expr(c, r)?;
                let j = self.emit(Op::JumpIf { cond: r, to: top });
                let _ = j;
                self.pop_ctl(Some(cont));
            }
            Stmt::ForIn(init, obj, body) => self.for_each(label, init, obj, body, false)?,
            Stmt::ForOf(init, obj, body) => self.for_each(label, init, obj, body, true)?,
            Stmt::Break(l) => self.jump_out(l.as_ref(), false)?,
            Stmt::Continue(l) => self.jump_out(l.as_ref(), true)?,
            Stmt::Throw(e) => {
                let r = self.reg();
                self.expr(e, r)?;
                self.emit(Op::Throw { src: r });
            }
            Stmt::Try(block, catch, finally) => self.try_stmt(block, catch.as_ref(), finally.as_deref())?,
            Stmt::Switch(d, cases) => self.switch(label, d, cases)?,
            Stmt::Labeled(l, body) => {
                self.pending_label = Some(l.clone());
                match &**body {
                    Stmt::For(..) | Stmt::ForIn(..) | Stmt::ForOf(..) | Stmt::While(..) | Stmt::DoWhile(..) | Stmt::Block(_) | Stmt::Switch(..) => self.stmt(body)?,
                    other => {
                        self.pending_label = None;
                        self.push_ctl(Some(l.clone()), false);
                        self.stmt(other)?;
                        self.pop_ctl(None);
                    }
                }
            }
            Stmt::Empty => {}
        }
        Ok(())
    }

    fn push_ctl(&mut self, label: Option<Rc<str>>, is_loop: bool) {
        self.ctl.push(Ctl { label, is_loop, breaks: Vec::new(), continues: Vec::new(), envs: self.envs, tries: self.tries, iters: self.iters });
    }

    /// Close a control context: breaks go to here, continues to `cont`.
    fn pop_ctl(&mut self, cont: Option<u32>) {
        let c = self.ctl.pop().unwrap();
        let end = self.here();
        for b in c.breaks {
            self.patch(b, end);
        }
        if let Some(cont) = cont {
            for j in c.continues {
                self.patch(j, cont);
            }
        }
    }

    /// `break` / `continue`: leave scopes, handlers and iterators down to
    /// the target, running enclosing `finally` blocks.
    fn jump_out(&mut self, label: Option<&Rc<str>>, cont: bool) -> CResult {
        let idx = self
            .ctl
            .iter()
            .rposition(|c| match label {
                Some(l) => c.label.as_ref() == Some(l) && (!cont || c.is_loop),
                None => if cont { c.is_loop } else { c.is_loop || c.label.is_none() },
            })
            .ok_or(())?;
        let (envs, tries, iters) = (self.ctl[idx].envs, self.ctl[idx].tries, self.ctl[idx].iters);
        self.unwind_to(envs, tries, iters)?;
        let j = self.emit(Op::Jump { to: NONE });
        if cont {
            self.ctl[idx].continues.push(j);
        } else {
            self.ctl[idx].breaks.push(j);
        }
        Ok(())
    }

    /// Emit the exits from the current position down to the given depths:
    /// close iterators, pop handlers (running finally blocks) and scopes.
    fn unwind_to(&mut self, envs: usize, tries: usize, iters: usize) -> CResult {
        for slot in (iters..self.iters).rev() {
            self.emit(Op::IterClose { slot: slot as u16 });
        }
        // Inline enclosing finally blocks (innermost first).
        let saved = (self.envs, self.tries, self.iters);
        let mut i = self.fins.len();
        while i > 0 {
            i -= 1;
            if self.fins[i].tries < tries {
                break;
            }
            let (ft, fe, body) = (self.fins[i].tries, self.fins[i].envs, self.fins[i].body);
            // Pop handlers down to the try statement, then run the finally.
            while self.tries > ft {
                self.emit(Op::TryPop);
                self.tries -= 1;
            }
            if self.envs > fe {
                self.emit(Op::PopScope { n: (self.envs - fe) as u16 });
                self.envs = fe;
            }
            let fins = std::mem::take(&mut self.fins);
            let r = self.stmts(body);
            self.fins = fins;
            self.fins.truncate(i + 1);
            let _ = &self.fins;
            r?;
        }
        while self.tries > tries {
            self.emit(Op::TryPop);
            self.tries -= 1;
        }
        if self.envs > envs {
            self.emit(Op::PopScope { n: (self.envs - envs) as u16 });
            self.envs = envs;
        }
        // These exits are jumps: compilation continues at the old depth.
        (self.envs, self.tries, self.iters) = saved;
        Ok(())
    }

    fn unwind_all(&mut self) -> CResult {
        if self.fins.is_empty() && self.iters == 0 {
            return Ok(());
        }
        self.unwind_to(0, 0, 0)
    }

    fn for_loop(&mut self, label: Option<Rc<str>>, init: Option<&'a Stmt>, test: Option<&'a Expr>, update: Option<&'a Expr>, body: &'a Stmt) -> CResult {
        // `for (let ...)`: a scope for the loop variables; a fresh copy per
        // iteration when closures capture them.
        let lexical = matches!(init, Some(Stmt::Var(VarKind::Let | VarKind::Const, _)));
        let mut per_iter = false;
        self.scopes.push(FxHashMap::default());
        if lexical {
            let mut names = Vec::new();
            if let Some(Stmt::Var(_, ds)) = init {
                ds.iter().for_each(|(p, _)| pat_names(p, &mut names));
            }
            per_iter = names.iter().any(|n| self.env_names.contains(n));
            if per_iter {
                self.emit(Op::PushScope);
                self.envs += 1;
            }
        }
        if let Some(i) = init {
            self.stmt(i)?;
        }
        let top = self.here();
        self.push_ctl(label, true);
        let exit = match test {
            Some(t) => {
                let r = self.reg();
                self.expr(t, r)?;
                let j = self.emit(Op::JumpIfNot { cond: r, to: NONE });
                self.next = r;
                Some(j)
            }
            None => None,
        };
        self.stmt(body)?;
        let cont = self.here();
        if per_iter {
            self.emit(Op::CopyScope);
        }
        if let Some(u) = update {
            let r = self.reg();
            self.expr(u, r)?;
            self.next = r;
        }
        self.emit(Op::Loop { to: top });
        if let Some(j) = exit {
            let end = self.here();
            self.patch(j, end);
        }
        self.pop_ctl(Some(cont));
        self.scopes.pop();
        if per_iter {
            self.emit(Op::PopScope { n: 1 });
            self.envs -= 1;
        }
        Ok(())
    }

    fn for_each(&mut self, label: Option<Rc<str>>, init: &'a ForInit, obj: &'a Expr, body: &'a Stmt, of: bool) -> CResult {
        let src = self.reg();
        self.expr(obj, src)?;
        let slot = self.iters as u16;
        self.iters += 1;
        self.max_iters = self.max_iters.max(self.iters);
        self.emit(Op::IterStart { slot, src, of });
        let top = self.here();
        self.push_ctl(label, true);
        // The iteration values' slot is closed by the loop itself.
        self.ctl.last_mut().unwrap().iters = self.iters;
        let v = self.reg();
        let next = self.emit(Op::IterNext { slot, dst: v, end: NONE });
        // Loop variable: a fresh scope per iteration if captured.
        let (kind, pat) = match init {
            ForInit::Var(k, p) => (Some(*k), p),
            ForInit::Target(p) => (None, p),
        };
        let mut names = Vec::new();
        pat_names(pat, &mut names);
        let lexical = matches!(kind, Some(VarKind::Let | VarKind::Const));
        let scoped = lexical && names.iter().any(|n| self.env_names.contains(n));
        self.scopes.push(FxHashMap::default());
        if scoped {
            self.emit(Op::PushScope);
            self.envs += 1;
        }
        // Continue / break inside the body must also pop this scope.
        self.ctl.last_mut().unwrap().envs = self.envs - scoped as usize;
        self.bind_pattern(pat, v, kind)?;
        let mark = self.next;
        self.stmt(body)?;
        self.next = mark;
        if scoped {
            self.emit(Op::PopScope { n: 1 });
            self.envs -= 1;
        }
        self.scopes.pop();
        self.emit(Op::Loop { to: top });
        let end = self.here();
        self.patch(next, end);
        // Breaks land after the IterNext-exhausted point: close the iterator.
        let c = self.ctl.pop().unwrap();
        let brk = self.here();
        for b in c.breaks {
            self.patch(b, brk);
        }
        for j in c.continues {
            self.patch(j, top);
        }
        self.emit(Op::IterClose { slot });
        self.iters -= 1;
        Ok(())
    }

    fn try_stmt(&mut self, block: &'a [Stmt], catch: Option<&'a (Option<Pattern>, Vec<Stmt>)>, finally: Option<&'a [Stmt]>) -> CResult {
        let exc = self.reg();
        if let Some(f) = finally {
            self.fins.push(Fin { body: f, tries: self.tries, envs: self.envs });
        }
        // Finally-only handler around everything (try + catch).
        let fin_push = finally.map(|_| {
            let at = self.emit(Op::TryPush { target: NONE, exc, catch: false });
            self.tries += 1;
            at
        });
        let catch_push = catch.map(|_| {
            let at = self.emit(Op::TryPush { target: NONE, exc, catch: true });
            self.tries += 1;
            at
        });
        self.stmts(block)?;
        let mut to_end = Vec::new();
        if let Some(cp) = catch_push {
            self.emit(Op::TryPop);
            self.tries -= 1;
            to_end.push(self.emit(Op::Jump { to: NONE }));
            let h = self.here();
            self.patch(cp, h);
            let (param, body) = catch.unwrap();
            self.enter_block(body)?;
            if let Some(p) = param {
                let mut names = Vec::new();
                pat_names(p, &mut names);
                self.bind_pattern(p, exc, Some(VarKind::Let))?;
            }
            for s in body {
                self.stmt(s)?;
            }
            self.leave_block();
        }
        match fin_push {
            Some(fp) => {
                // Normal completion of try / catch.
                for j in to_end.drain(..) {
                    let h = self.here();
                    self.patch(j, h);
                }
                self.emit(Op::TryPop);
                self.tries -= 1;
                self.fins.pop();
                let f = finally.unwrap();
                self.stmts(f)?;
                let done = self.emit(Op::Jump { to: NONE });
                // Exceptional completion: run finally, rethrow.
                let h = self.here();
                self.patch(fp, h);
                let saved = self.reg();
                self.emit(Op::Move { dst: saved, src: exc });
                self.stmts(f)?;
                self.emit(Op::Throw { src: saved });
                let end = self.here();
                self.patch(done, end);
            }
            None => {
                let end = self.here();
                for j in to_end {
                    self.patch(j, end);
                }
            }
        }
        Ok(())
    }

    fn switch(&mut self, label: Option<Rc<str>>, d: &'a Expr, cases: &'a [(Option<Expr>, Vec<Stmt>)]) -> CResult {
        let v = self.reg();
        self.expr(d, v)?;
        // One lexical scope for all cases.
        let all: Vec<&'a Stmt> = cases.iter().flat_map(|(_, b)| b.iter()).collect();
        let push = all.iter().any(|s| {
            let mut names = Vec::new();
            match s {
                Stmt::Var(VarKind::Let | VarKind::Const, ds) => ds.iter().for_each(|(p, _)| pat_names(p, &mut names)),
                Stmt::Class(c) => names.extend(c.name.clone()),
                Stmt::Func(f) => names.extend(f.name.clone()),
                _ => {}
            }
            names.iter().any(|n| self.env_names.contains(n))
        });
        self.scopes.push(FxHashMap::default());
        if push {
            self.emit(Op::PushScope);
            self.envs += 1;
        }
        for s in &all {
            let mut names = Vec::new();
            if let Stmt::Var(VarKind::Let | VarKind::Const, ds) = s {
                ds.iter().for_each(|(p, _)| pat_names(p, &mut names));
            }
            if let Stmt::Class(c) = s {
                names.extend(c.name.clone());
            }
            for n in names {
                self.declare(&n);
            }
        }
        for s in &all {
            if let Stmt::Func(f) = s {
                let name = f.name.clone().ok_or(())?;
                let b = self.declare(&name);
                let r = self.reg();
                self.funcs.push(f.clone());
                let func = (self.funcs.len() - 1) as u32;
                self.emit(Op::Closure { dst: r, func });
                self.init_binding(&name, b, r);
            }
        }
        self.push_ctl(label, false);
        // The `break` target pops the switch scope itself (below).
        self.ctl.last_mut().unwrap().envs = self.envs;
        let mut jumps = Vec::new();
        let t = self.reg();
        for (test, _) in cases {
            if let Some(test) = test {
                self.expr(test, t)?;
                self.emit(Op::Bin { op: BinOp::StrictEq, dst: t, a: v, b: t });
                jumps.push(Some(self.emit(Op::JumpIf { cond: t, to: NONE })));
            } else {
                jumps.push(None);
            }
        }
        let to_default = self.emit(Op::Jump { to: NONE });
        let mut default_at = None;
        for (i, (test, body)) in cases.iter().enumerate() {
            let h = self.here();
            match jumps[i] {
                Some(j) => self.patch(j, h),
                None => default_at = Some(h),
            }
            let _ = test;
            for s in body {
                self.stmt(s)?;
            }
        }
        let end = self.here();
        self.patch(to_default, default_at.unwrap_or(end));
        self.pop_ctl(None);
        self.scopes.pop();
        if push {
            self.emit(Op::PopScope { n: 1 });
            self.envs -= 1;
        }
        Ok(())
    }

    // ---------------------------------------------------------- patterns

    /// Bind a pattern to the value in `src` (declaration, or assignment
    /// with `kind` None).
    fn bind_pattern(&mut self, p: &'a Pattern, src: Reg, kind: Option<VarKind>) -> CResult {
        match p {
            Pattern::Ident(n) => {
                match kind {
                    Some(_) => {
                        let b = match self.lookup_local_decl(n, kind) {
                            Some(b) => b,
                            None => self.declare(n),
                        };
                        self.init_binding(n, b, src);
                    }
                    None => self.assign_name(n, src),
                }
                Ok(())
            }
            Pattern::Default(inner, d) => {
                let v = self.reg();
                self.emit(Op::Move { dst: v, src });
                let j = self.emit(Op::JumpIfNotUndef { src: v, to: NONE });
                self.expr(d, v)?;
                if let (Pattern::Ident(n), Expr::Func(_) | Expr::Class(_)) = (&**inner, d) {
                    let name = self.name(n);
                    self.emit(Op::SetFnName { f: v, name });
                }
                let h = self.here();
                self.patch(j, h);
                self.bind_pattern(inner, v, kind)
            }
            Pattern::Expr(e) => match e {
                Expr::Member(o, k, _) if !matches!(**o, Expr::Super) => {
                    let ob = self.reg();
                    self.expr(o, ob)?;
                    let name = self.name(k);
                    self.emit(Op::SetProp { obj: ob, name, src });
                    Ok(())
                }
                Expr::Index(o, k, _) => {
                    let ob = self.reg();
                    self.expr(o, ob)?;
                    let kr = self.reg();
                    self.expr(k, kr)?;
                    self.emit(Op::SetIndex { obj: ob, key: kr, src });
                    Ok(())
                }
                _ => self.bind_pattern_island(p, src, kind),
            },
            Pattern::Object(props, rest) => {
                if rest.is_some() {
                    return self.bind_pattern_island(p, src, kind);
                }
                self.emit(Op::RequireObject { src });
                for (k, pp) in props {
                    let v = self.reg();
                    match k {
                        PropKey::Name(n) => {
                            let name = self.name(n);
                            self.emit(Op::GetProp { dst: v, obj: src, name });
                        }
                        PropKey::Computed(e) => {
                            let kr = self.reg();
                            self.expr(e, kr)?;
                            self.emit(Op::GetIndex { dst: v, obj: src, key: kr });
                        }
                    }
                    self.bind_pattern(pp, v, kind)?;
                }
                Ok(())
            }
            Pattern::Array(items, rest) => {
                let arr = self.reg();
                self.emit(Op::ToArray { dst: arr, src });
                let idx = self.reg();
                for (i, it) in items.iter().enumerate() {
                    if let Some(pp) = it {
                        let k = self.konst(Value::Num(i as f64));
                        self.emit(Op::Const { dst: idx, k });
                        let v = self.reg();
                        self.emit(Op::GetIndex { dst: v, obj: arr, key: idx });
                        self.bind_pattern(pp, v, kind)?;
                    }
                }
                if let Some(r) = rest {
                    let v = self.reg();
                    self.emit(Op::Slice { dst: v, src: arr, from: items.len() as u16 });
                    self.bind_pattern(r, v, kind)?;
                }
                Ok(())
            }
        }
    }

    /// For a declaration: the binding already declared in the innermost
    /// scope (block decls / hoisted vars), else None.
    fn lookup_local_decl(&self, n: &str, kind: Option<VarKind>) -> Option<Binding> {
        match kind {
            Some(VarKind::Var) => self.lookup(n),
            _ => self.scopes.last().unwrap().get(n).copied(),
        }
    }

    fn bind_pattern_island(&mut self, p: &'a Pattern, src: Reg, kind: Option<VarKind>) -> CResult {
        idents_pat(p, &mut self.island_names);
        let mut names = Vec::new();
        pat_names(p, &mut names);
        for n in &names {
            self.island_names.insert(n.clone());
            if kind.is_some() && self.lookup_local_decl(n, kind).is_none() {
                self.declare(n);
            }
        }
        self.pats.push(p as *const Pattern);
        let pat = (self.pats.len() - 1) as u32;
        let kind = match kind {
            Some(VarKind::Var) => 0,
            Some(VarKind::Let) => 1,
            Some(VarKind::Const) => 2,
            None => 3,
        };
        self.emit(Op::BindPattern { pat, src, kind });
        Ok(())
    }

    // ---------------------------------------------------------- expressions

    /// Compile `e`, leaving its value in `dst`.
    fn expr(&mut self, e: &'a Expr, dst: Reg) -> CResult {
        let mark = self.next.max(dst + 1);
        self.next = mark;
        let r = self.expr_inner(e, dst);
        self.next = mark.max(self.floor);
        r
    }

    fn expr_inner(&mut self, e: &'a Expr, dst: Reg) -> CResult {
        match e {
            Expr::Num(n) => {
                let k = self.konst(Value::Num(*n));
                self.emit(Op::Const { dst, k });
            }
            Expr::BigInt(n) => {
                let k = self.konst(Value::BigInt(*n));
                self.emit(Op::Const { dst, k });
            }
            Expr::Str(s) => {
                let k = self.konst(Value::Str(s.clone()));
                self.emit(Op::Const { dst, k });
            }
            Expr::Bool(b) => {
                let k = self.konst(Value::Bool(*b));
                self.emit(Op::Const { dst, k });
            }
            Expr::Null => {
                let k = self.konst(Value::Null);
                self.emit(Op::Const { dst, k });
            }
            Expr::Ident(n) => {
                if &**n == "undefined" && self.lookup(n).is_none() {
                    self.emit(Op::Undef { dst });
                    return Ok(());
                }
                match self.lookup(n) {
                    Some(Binding::Reg(r)) => {
                        if r != dst {
                            self.emit(Op::Move { dst, src: r });
                        }
                    }
                    _ => {
                        let name = self.name(n);
                        self.emit(Op::GetName { dst, name });
                    }
                }
            }
            Expr::This => {
                if self.def.arrow {
                    self.emit(Op::ThisEnv { dst });
                } else {
                    self.emit(Op::Move { dst, src: R_THIS });
                }
            }
            Expr::Template(q, xs) => {
                let start = self.regs(q.len() + xs.len());
                let mut i = start;
                for (j, part) in q.iter().enumerate() {
                    let k = self.konst(Value::Str(part.clone()));
                    self.emit(Op::Const { dst: i, k });
                    i += 1;
                    if let Some(x) = xs.get(j) {
                        self.expr(x, i)?;
                        i += 1;
                    }
                }
                self.emit(Op::Concat { dst, start, count: i - start });
            }
            Expr::Regex(b, f) => {
                self.regexes.push((b.clone(), f.clone()));
                let idx = (self.regexes.len() - 1) as u32;
                self.emit(Op::Regex { dst, idx });
            }
            Expr::Array(items) => {
                let simple = items.iter().all(|i| matches!(i, ArrItem::Item(_)));
                if simple {
                    let start = self.regs(items.len());
                    for (i, it) in items.iter().enumerate() {
                        if let ArrItem::Item(e) = it {
                            self.expr(e, start + i as Reg)?;
                        }
                    }
                    self.emit(Op::Array { dst, start, count: items.len() as u16 });
                } else {
                    let arr = self.reg();
                    self.emit(Op::Array { dst: arr, start: 0, count: 0 });
                    let t = self.reg();
                    for it in items {
                        match it {
                            ArrItem::Item(e) => {
                                self.expr(e, t)?;
                                self.emit(Op::ArrayPush { arr, src: t });
                            }
                            ArrItem::Spread(e) => {
                                self.expr(e, t)?;
                                self.emit(Op::ArraySpread { arr, src: t });
                            }
                            ArrItem::Hole => {
                                self.emit(Op::ArrayHole { arr });
                            }
                        }
                    }
                    self.emit(Op::Move { dst, src: arr });
                }
            }
            Expr::Object(props) => {
                let o = self.reg();
                self.emit(Op::Object { dst: o });
                let v = self.reg();
                let kr = self.reg();
                for p in props {
                    match p {
                        PropDef::Value(PropKey::Name(n), ve) => {
                            self.expr(ve, v)?;
                            let name = self.name(n);
                            let method = matches!(ve, Expr::Func(_) | Expr::Class(_));
                            self.emit(Op::DefProp { obj: o, name, src: v, method });
                        }
                        PropDef::Value(PropKey::Computed(ke), ve) => {
                            self.expr(ke, kr)?;
                            self.expr(ve, v)?;
                            let method = matches!(ve, Expr::Func(_) | Expr::Class(_));
                            self.emit(Op::DefPropKey { obj: o, key: kr, src: v, method });
                        }
                        PropDef::Getter(k, f) | PropDef::Setter(k, f) => {
                            match k {
                                PropKey::Name(n) => {
                                    let kc = self.konst(Value::Str(n.clone()));
                                    self.emit(Op::Const { dst: kr, k: kc });
                                }
                                PropKey::Computed(ke) => self.expr(ke, kr)?,
                            }
                            self.funcs.push(f.clone());
                            let func = (self.funcs.len() - 1) as u32;
                            self.emit(Op::Closure { dst: v, func });
                            self.emit(Op::DefAccessor { obj: o, key: kr, func: v, getter: matches!(p, PropDef::Getter(..)) });
                        }
                        PropDef::Spread(se) => {
                            self.expr(se, v)?;
                            self.emit(Op::ObjSpread { obj: o, src: v });
                        }
                    }
                }
                self.emit(Op::Move { dst, src: o });
            }
            Expr::Func(def) => {
                self.funcs.push(def.clone());
                let func = (self.funcs.len() - 1) as u32;
                self.emit(Op::Closure { dst, func });
            }
            Expr::Class(cd) => {
                self.classes.push(cd.clone());
                let class = (self.classes.len() - 1) as u32;
                self.emit(Op::Class { dst, class });
            }
            Expr::Unary(op, x) => match *op {
                "typeof" => {
                    if let Expr::Ident(n) = &**x {
                        if !matches!(self.lookup(n), Some(Binding::Reg(_))) {
                            let name = self.name(n);
                            self.emit(Op::TypeofName { dst, name });
                            return Ok(());
                        }
                    }
                    self.expr(x, dst)?;
                    self.emit(Op::Un { op: UnOp::Typeof, dst, src: dst });
                }
                "delete" => match &**x {
                    Expr::Member(o, k, false) if !matches!(**o, Expr::Super) => {
                        let ob = self.reg();
                        self.expr(o, ob)?;
                        let name = self.name(k);
                        self.emit(Op::DelProp { dst, obj: ob, name });
                    }
                    Expr::Index(o, k, false) => {
                        let ob = self.reg();
                        self.expr(o, ob)?;
                        let kr = self.reg();
                        self.expr(k, kr)?;
                        self.emit(Op::DelIndex { dst, obj: ob, key: kr });
                    }
                    _ => self.island(e, dst),
                },
                "void" => {
                    self.expr(x, dst)?;
                    self.emit(Op::Undef { dst });
                }
                _ => {
                    let op = match *op {
                        "!" => UnOp::Not,
                        "-" => UnOp::Neg,
                        "+" => UnOp::Plus,
                        "~" => UnOp::BitNot,
                        _ => {
                            self.island(e, dst);
                            return Ok(());
                        }
                    };
                    self.expr(x, dst)?;
                    self.emit(Op::Un { op, dst, src: dst });
                }
            },
            Expr::Update { op, prefix, target } => {
                let delta: i8 = if *op == "++" { 1 } else { -1 };
                match &**target {
                    Expr::Ident(n) => {
                        let old = self.reg();
                        self.expr(target, old)?;
                        let new = self.reg();
                        self.emit(Op::Step { dst: new, src: old, delta });
                        self.assign_name(n, new);
                        if *prefix {
                            self.emit(Op::Move { dst, src: new });
                        } else {
                            self.emit(Op::ToNumeric { dst, src: old });
                        }
                    }
                    Expr::Member(o, k, false) if !matches!(**o, Expr::Super) => {
                        let ob = self.reg();
                        self.expr(o, ob)?;
                        let name = self.name(k);
                        let old = self.reg();
                        self.emit(Op::GetProp { dst: old, obj: ob, name });
                        let new = self.reg();
                        self.emit(Op::Step { dst: new, src: old, delta });
                        self.emit(Op::SetProp { obj: ob, name, src: new });
                        if *prefix {
                            self.emit(Op::Move { dst, src: new });
                        } else {
                            self.emit(Op::ToNumeric { dst, src: old });
                        }
                    }
                    Expr::Index(o, k, false) => {
                        let ob = self.reg();
                        self.expr(o, ob)?;
                        let kr = self.reg();
                        self.expr(k, kr)?;
                        let old = self.reg();
                        self.emit(Op::GetIndex { dst: old, obj: ob, key: kr });
                        let new = self.reg();
                        self.emit(Op::Step { dst: new, src: old, delta });
                        self.emit(Op::SetIndex { obj: ob, key: kr, src: new });
                        if *prefix {
                            self.emit(Op::Move { dst, src: new });
                        } else {
                            self.emit(Op::ToNumeric { dst, src: old });
                        }
                    }
                    _ => self.island(e, dst),
                }
            }
            Expr::Binary(op, a, b) => match BinOp::from_str(op) {
                Some(op) => {
                    self.expr(a, dst)?;
                    let rb = self.reg();
                    self.expr(b, rb)?;
                    self.emit(Op::Bin { op, dst, a: dst, b: rb });
                }
                None => self.island(e, dst),
            },
            Expr::Logical(op, a, b) => {
                self.expr(a, dst)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfNot { cond: dst, to: NONE }),
                    "||" => self.emit(Op::JumpIf { cond: dst, to: NONE }),
                    _ => self.emit(Op::JumpIfNotNullish { src: dst, to: NONE }),
                };
                self.expr(b, dst)?;
                let h = self.here();
                self.patch(j, h);
            }
            Expr::Cond(c, a, b) => {
                self.expr(c, dst)?;
                let j = self.emit(Op::JumpIfNot { cond: dst, to: NONE });
                self.expr(a, dst)?;
                let j2 = self.emit(Op::Jump { to: NONE });
                let h = self.here();
                self.patch(j, h);
                self.expr(b, dst)?;
                let h = self.here();
                self.patch(j2, h);
            }
            Expr::Seq(xs) => {
                for x in xs {
                    self.expr(x, dst)?;
                }
            }
            Expr::Assign(op, target, value) => self.assign(e, op, target, value, dst)?,
            Expr::Member(..) | Expr::Index(..) | Expr::Call(..) => {
                if chain_has_super(e) {
                    self.island(e, dst);
                    return Ok(());
                }
                let mut exits = Vec::new();
                self.chain(e, dst, None, &mut exits)?;
                if !exits.is_empty() {
                    let j = self.emit(Op::Jump { to: NONE });
                    let h = self.here();
                    for x in exits {
                        self.patch(x, h);
                    }
                    self.emit(Op::Undef { dst });
                    let h = self.here();
                    self.patch(j, h);
                }
            }
            Expr::New(callee, args) => {
                let f = self.reg();
                self.expr(callee, f)?;
                match self.args(args)? {
                    Args::Regs(start, n) => {
                        self.emit(Op::New { dst, f, args: start, argc: n });
                    }
                    Args::Array(arr) => {
                        self.emit(Op::NewArr { dst, f, arr });
                    }
                }
            }
            Expr::Await(x) => {
                self.expr(x, dst)?;
                self.emit(Op::Await { dst, src: dst });
            }
            Expr::Yield(x, delegate) => {
                match x {
                    Some(x) => self.expr(x, dst)?,
                    None => {
                        self.emit(Op::Undef { dst });
                    }
                }
                self.emit(Op::Yield { dst, src: dst, delegate: *delegate });
            }
            Expr::Super | Expr::TaggedTemplate(..) | Expr::ImportCall(_) | Expr::ImportMeta => self.island(e, dst),
        }
        Ok(())
    }

    /// Member / index / call chains (optional chaining: `exits` collects the
    /// jumps taken when a `?.` link meets null / undefined). With `this_out`,
    /// the object of the final member access is left there (method calls).
    fn chain(&mut self, e: &'a Expr, dst: Reg, this_out: Option<Reg>, exits: &mut Vec<usize>) -> CResult {
        match e {
            Expr::Member(o, k, opt) => {
                let ob = this_out.unwrap_or_else(|| self.reg());
                self.chain_part(o, ob, exits)?;
                if *opt {
                    exits.push(self.emit(Op::JumpIfNullish { src: ob, to: NONE }));
                }
                let name = self.name(k);
                self.emit(Op::GetProp { dst, obj: ob, name });
            }
            Expr::Index(o, k, opt) => {
                let ob = this_out.unwrap_or_else(|| self.reg());
                self.chain_part(o, ob, exits)?;
                if *opt {
                    exits.push(self.emit(Op::JumpIfNullish { src: ob, to: NONE }));
                }
                let kr = self.reg();
                self.expr(k, kr)?;
                self.emit(Op::GetIndex { dst, obj: ob, key: kr });
            }
            Expr::Call(callee, args, opt) => {
                let f = self.reg();
                let this = self.reg();
                match &**callee {
                    Expr::Member(..) | Expr::Index(..) => self.chain(callee, f, Some(this), exits)?,
                    _ => {
                        self.chain_part(callee, f, exits)?;
                        self.emit(Op::Undef { dst: this });
                    }
                }
                if *opt {
                    exits.push(self.emit(Op::JumpIfNullish { src: f, to: NONE }));
                }
                match self.args(args)? {
                    Args::Regs(start, n) => {
                        self.emit(Op::Call { dst, f, this, args: start, argc: n });
                    }
                    Args::Array(arr) => {
                        self.emit(Op::CallArr { dst, f, this, arr });
                    }
                }
            }
            _ => self.expr(e, dst)?,
        }
        Ok(())
    }

    /// The object part of a chain link (continuing the same chain).
    fn chain_part(&mut self, o: &'a Expr, dst: Reg, exits: &mut Vec<usize>) -> CResult {
        match o {
            Expr::Member(..) | Expr::Index(..) | Expr::Call(..) => self.chain(o, dst, None, exits),
            _ => self.expr(o, dst),
        }
    }

    fn args(&mut self, args: &'a [ArrItem]) -> Result<Args, ()> {
        if args.iter().all(|a| matches!(a, ArrItem::Item(_))) {
            let start = self.regs(args.len());
            for (i, a) in args.iter().enumerate() {
                if let ArrItem::Item(e) = a {
                    self.expr(e, start + i as Reg)?;
                }
            }
            return Ok(Args::Regs(start, args.len() as u16));
        }
        let arr = self.reg();
        self.emit(Op::Array { dst: arr, start: 0, count: 0 });
        let t = self.reg();
        for a in args {
            match a {
                ArrItem::Item(e) => {
                    self.expr(e, t)?;
                    self.emit(Op::ArrayPush { arr, src: t });
                }
                ArrItem::Spread(e) => {
                    self.expr(e, t)?;
                    self.emit(Op::ArraySpread { arr, src: t });
                }
                ArrItem::Hole => {
                    self.emit(Op::ArrayHole { arr });
                }
            }
        }
        Ok(Args::Array(arr))
    }

    fn assign(&mut self, e: &'a Expr, op: &str, target: &'a Pattern, value: &'a Expr, dst: Reg) -> CResult {
        let bin = if op == "=" || matches!(op, "&&=" | "||=" | "??=") { None } else { Some(BinOp::from_str(&op[..op.len() - 1]).ok_or(())?) };
        let logical = matches!(op, "&&=" | "||=" | "??=");
        // How to read and write the target.
        enum T {
            Name(Rc<str>),
            Prop(Reg, u32),
            Index(Reg, Reg),
        }
        let t = match target {
            Pattern::Ident(n) => T::Name(n.clone()),
            Pattern::Expr(Expr::Member(o, k, false)) if !matches!(**o, Expr::Super) => {
                let ob = self.reg();
                self.expr(o, ob)?;
                T::Prop(ob, self.name(k))
            }
            Pattern::Expr(Expr::Index(o, k, false)) => {
                let ob = self.reg();
                self.expr(o, ob)?;
                let kr = self.reg();
                self.expr(k, kr)?;
                T::Index(ob, kr)
            }
            Pattern::Object(..) | Pattern::Array(..) if op == "=" => {
                self.expr(value, dst)?;
                return self.bind_pattern(target, dst, None);
            }
            _ => {
                self.island(e, dst);
                return Ok(());
            }
        };
        let read = |c: &mut Self, into: Reg| match &t {
            T::Name(n) => match c.lookup(n) {
                Some(Binding::Reg(r)) => {
                    c.emit(Op::Move { dst: into, src: r });
                }
                _ => {
                    let name = c.name(n);
                    c.emit(Op::GetName { dst: into, name });
                }
            },
            T::Prop(ob, name) => {
                c.emit(Op::GetProp { dst: into, obj: *ob, name: *name });
            }
            T::Index(ob, kr) => {
                c.emit(Op::GetIndex { dst: into, obj: *ob, key: *kr });
            }
        };
        let write = |c: &mut Self, from: Reg| match &t {
            T::Name(n) => c.assign_name(n, from),
            T::Prop(ob, name) => {
                c.emit(Op::SetProp { obj: *ob, name: *name, src: from });
            }
            T::Index(ob, kr) => {
                c.emit(Op::SetIndex { obj: *ob, key: *kr, src: from });
            }
        };
        if logical {
            read(self, dst);
            let j = match op {
                "&&=" => self.emit(Op::JumpIfNot { cond: dst, to: NONE }),
                "||=" => self.emit(Op::JumpIf { cond: dst, to: NONE }),
                _ => self.emit(Op::JumpIfNotNullish { src: dst, to: NONE }),
            };
            self.expr(value, dst)?;
            write(self, dst);
            let h = self.here();
            self.patch(j, h);
            return Ok(());
        }
        match bin {
            None => {
                self.expr(value, dst)?;
                if let (T::Name(n), Expr::Func(_) | Expr::Class(_)) = (&t, value) {
                    let name = self.name(n);
                    self.emit(Op::SetFnName { f: dst, name });
                }
            }
            Some(op) => {
                let cur = self.reg();
                read(self, cur);
                let rv = self.reg();
                self.expr(value, rv)?;
                self.emit(Op::Bin { op, dst, a: cur, b: rv });
            }
        }
        write(self, dst);
        Ok(())
    }
}

enum Args {
    Regs(Reg, u16),
    Array(Reg),
}

/// `super.x` / `super(...)` anywhere in a member / call chain.
fn chain_has_super(e: &Expr) -> bool {
    match e {
        Expr::Super => true,
        Expr::Member(o, _, _) | Expr::Index(o, _, _) => chain_has_super(o),
        Expr::Call(c, _, _) => chain_has_super(c),
        _ => false,
    }
}

/// Compile a function, or None if the VM can't run it (the tree-walking
/// interpreter does).
pub fn compile(def: &Rc<FuncDef>) -> Option<Code> {
    if (def.is_gen || def.is_async) && !coro::supported() {
        return None;
    }
    // Names captured by nested functions / classes live in scope objects.
    let mut env_names = FxHashSet::default();
    let mut nested = false;
    match &def.body {
        Body::Expr(e) => captured_expr(e, &mut env_names, &mut nested),
        Body::Block(b) => b.iter().for_each(|s| captured_stmt(s, &mut env_names, &mut nested)),
    }
    for p in &def.params {
        captured_pat(p, &mut env_names, &mut nested);
    }
    // Islands referencing register variables force those into scope objects:
    // recompile until stable.
    for _ in 0..4 {
        let mut c = Compiler {
            def,
            ops: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            name_ix: FxHashMap::default(),
            funcs: Vec::new(),
            classes: Vec::new(),
            regexes: Vec::new(),
            exprs: Vec::new(),
            stmts: Vec::new(),
            pats: Vec::new(),
            scopes: Vec::new(),
            env_names: &env_names,
            island_names: FxHashSet::default(),
            reg_names: FxHashSet::default(),
            next: 0,
            max: 0,
            ctl: Vec::new(),
            fins: Vec::new(),
            envs: 0,
            tries: 0,
            iters: 0,
            max_iters: 0,
            pending_label: None,
            uses_arguments: false,
            failed: false,
            scope_pushed: Vec::new(),
            floor: 0,
        };
        if c.function(def).is_err() || c.failed {
            return None;
        }
        let conflicts: Vec<Rc<str>> = c.island_names.iter().filter(|n| c.reg_names.contains(*n)).cloned().collect();
        let islands = !c.exprs.is_empty() || !c.stmts.is_empty() || !c.pats.is_empty();
        if conflicts.is_empty() {
            let needs_env = nested || islands || env_names.iter().any(|n| c.reg_names.contains(n)) || !env_names.is_empty() || (!def.arrow && uses_this_or_super(def) && nested);
            return Some(Code {
                def: def.clone(),
                ops: c.ops,
                consts: c.consts,
                names: c.names,
                funcs: c.funcs,
                classes: c.classes,
                regexes: c.regexes,
                exprs: c.exprs,
                stmts: c.stmts,
                pats: c.pats,
                nregs: c.max as usize + 1,
                niters: c.max_iters,
                needs_env,
                uses_arguments: c.uses_arguments,
            });
        }
        let more = c.island_names.clone();
        drop(c);
        env_names.extend(more);
    }
    None
}

// ------------------------------------------------------------------ runtime

struct Frame {
    regs: Vec<Value>,
    envs: Vec<Env>,
    handlers: Vec<Handler>,
    iters: Vec<Option<IterState>>,
}

impl Interp {
    /// The VM code for a function, compiling it on first use (None: the
    /// tree-walking interpreter runs it).
    pub(crate) fn vm_code(&mut self, def: &Rc<FuncDef>) -> Option<Rc<Code>> {
        let key = Rc::as_ptr(def) as usize;
        let calls = match self.vm_cache.get_mut(&key) {
            Some(VmEntry::Code(c)) => return c.clone(),
            Some(VmEntry::Calls(n)) => {
                *n += 1;
                *n
            }
            None => {
                self.vm_cache.insert(key, VmEntry::Calls(1));
                1
            }
        };
        // Functions run once (module wrappers, setup code) aren't worth
        // compiling: the tree-walking interpreter runs them.
        if calls < compile_threshold() && !has_loop(def) {
            return None;
        }
        let t = std::time::Instant::now();
        let code = if vm_enabled() { compile(def).map(Rc::new) } else { None };
        if vm_trace() {
            let us = t.elapsed().as_micros();
            match &code {
                Some(c) => self.log(&format!("[vm] compiled {} in {us}us ({} ops, {} regs, env {}, islands {})", def.name.as_deref().unwrap_or("<anon>"), c.ops.len(), c.nregs, c.needs_env, c.exprs.len() + c.pats.len())),
                None => self.log(&format!("[vm] fallback {}", def.name.as_deref().unwrap_or("<anon>"))),
            }
        }
        self.vm_cache.insert(key, VmEntry::Code(code.clone()));
        code
    }

    /// Run a compiled function.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn vm_call(&mut self, code: &Rc<Code>, closure_env: Env, this: Value, args: &[Value], fobj: Option<ObjRef>, home: Option<ObjRef>) -> R<Value> {
        let def = &code.def;
        let this = if def.arrow {
            Value::Undef
        } else if !def.strict && this.is_nullish() {
            Value::Obj(self.global.clone())
        } else {
            this
        };
        let env = if code.needs_env {
            if def.arrow {
                new_scope(Some(closure_env), None, None, true)
            } else {
                new_scope(Some(closure_env), Some(this.clone()), home.or(fobj.clone()), true)
            }
        } else {
            closure_env
        };
        let mut regs = vec![Value::Undef; code.nregs];
        regs[R_THIS as usize] = this;
        if let Some(f) = &fobj {
            regs[R_CALLEE as usize] = Value::Obj(f.clone());
        }
        if code.uses_arguments {
            regs[R_ARGUMENTS as usize] = self.array(args.to_vec());
        }
        let np = def.params.len();
        for (i, a) in args.iter().take(np).enumerate() {
            regs[R_ARG0 as usize + i] = a.clone();
        }
        if def.rest.is_some() {
            regs[R_REST as usize] = self.array(args.get(np..).map(|s| s.to_vec()).unwrap_or_default());
        }
        let mut f = Frame { regs, envs: vec![env], handlers: Vec::new(), iters: (0..code.niters).map(|_| None).collect() };
        let mut ip = 0usize;
        loop {
            match self.vm_run(code, &mut f, &mut ip) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if self.terminating {
                        return Err(e);
                    }
                    // Find a handler: catch handlers don't see generator
                    // `return()` (only finally does).
                    let ret = generator::is_return_marker(&e);
                    let mut found = None;
                    while let Some(h) = f.handlers.pop() {
                        if ret && h.catch {
                            continue;
                        }
                        found = Some(h);
                        break;
                    }
                    let Some(h) = found else { return Err(e) };
                    f.envs.truncate(h.envs);
                    f.regs[h.exc as usize] = e;
                    ip = h.target;
                }
            }
        }
    }

    fn vm_env(f: &Frame) -> &Env {
        f.envs.last().unwrap()
    }

    #[inline(never)]
    fn vm_run(&mut self, code: &Code, f: &mut Frame, ip: &mut usize) -> R<Value> {
        let ops = &code.ops[..];
        loop {
            let op = ops[*ip];
            *ip += 1;
            match op {
                Op::Const { dst, k } => f.regs[dst as usize] = code.consts[k as usize].clone(),
                Op::Undef { dst } => f.regs[dst as usize] = Value::Undef,
                Op::Move { dst, src } => {
                    let v = f.regs[src as usize].clone();
                    f.regs[dst as usize] = v;
                }
                Op::ThisEnv { dst } => f.regs[dst as usize] = Self::this_val(Self::vm_env(f)),
                Op::GetName { dst, name } => {
                    let env = Self::vm_env(f).clone();
                    f.regs[dst as usize] = self.lookup(&code.names[name as usize], &env)?;
                }
                Op::SetName { name, src } => {
                    let env = Self::vm_env(f).clone();
                    let v = f.regs[src as usize].clone();
                    self.assign_var(&code.names[name as usize], v, &env)?;
                }
                Op::DeclName { name, src } => {
                    let v = f.regs[src as usize].clone();
                    Self::vm_env(f).vars.borrow_mut().insert(code.names[name as usize].clone(), v);
                }
                Op::TypeofName { dst, name } => {
                    let env = Self::vm_env(f).clone();
                    let v = match self.lookup(&code.names[name as usize], &env) {
                        Ok(v) => Value::str(Self::type_of(&v)),
                        Err(_) => Value::str("undefined"),
                    };
                    f.regs[dst as usize] = v;
                }
                Op::Bin { op, dst, a, b } => {
                    let v = match (&f.regs[a as usize], &f.regs[b as usize]) {
                        (Value::Num(x), Value::Num(y)) => {
                            let (x, y) = (*x, *y);
                            match op {
                                BinOp::Add => Value::Num(x + y),
                                BinOp::Sub => Value::Num(x - y),
                                BinOp::Mul => Value::Num(x * y),
                                BinOp::Div => Value::Num(x / y),
                                BinOp::Mod => Value::Num(x % y),
                                BinOp::Lt => Value::Bool(x < y),
                                BinOp::Gt => Value::Bool(x > y),
                                BinOp::Le => Value::Bool(x <= y),
                                BinOp::Ge => Value::Bool(x >= y),
                                BinOp::Eq | BinOp::StrictEq => Value::Bool(x == y),
                                BinOp::Ne | BinOp::StrictNe => Value::Bool(x != y),
                                BinOp::BitAnd => Value::Num((to_i32(x) & to_i32(y)) as f64),
                                BinOp::BitOr => Value::Num((to_i32(x) | to_i32(y)) as f64),
                                BinOp::BitXor => Value::Num((to_i32(x) ^ to_i32(y)) as f64),
                                BinOp::Shl => Value::Num(to_i32(x).wrapping_shl(to_i32(y) as u32 & 31) as f64),
                                BinOp::Shr => Value::Num((to_i32(x) >> (to_i32(y) as u32 & 31)) as f64),
                                BinOp::UShr => Value::Num(((to_i32(x) as u32) >> (to_i32(y) as u32 & 31)) as f64),
                                _ => {
                                    let (a, b) = (f.regs[a as usize].clone(), f.regs[b as usize].clone());
                                    self.binary(op.as_str(), &a, &b)?
                                }
                            }
                        }
                        (x, y) if matches!(op, BinOp::StrictEq | BinOp::StrictNe) => {
                            let eq = strict_eq(x, y);
                            Value::Bool(if op == BinOp::StrictEq { eq } else { !eq })
                        }
                        (Value::Str(x), Value::Str(y)) if op == BinOp::Add => {
                            let mut s = String::with_capacity(x.len() + y.len());
                            s.push_str(x);
                            s.push_str(y);
                            Value::Str(s.into())
                        }
                        _ => {
                            let (a, b) = (f.regs[a as usize].clone(), f.regs[b as usize].clone());
                            self.binary(op.as_str(), &a, &b)?
                        }
                    };
                    f.regs[dst as usize] = v;
                }
                Op::Un { op, dst, src } => {
                    let v = f.regs[src as usize].clone();
                    let r = match (op, &v) {
                        (UnOp::Not, v) => Value::Bool(!v.truthy()),
                        (UnOp::Typeof, v) => Value::str(Self::type_of(v)),
                        (UnOp::Neg, Value::Num(n)) => Value::Num(-n),
                        (UnOp::Plus, Value::Num(n)) => Value::Num(*n),
                        (_, Value::BigInt(b)) => match op {
                            UnOp::Neg => Value::BigInt(b.checked_neg().ok_or_else(bigint::too_big)?),
                            UnOp::BitNot => Value::BigInt(!b),
                            _ => return Err(type_error("Cannot convert a BigInt value to a number")),
                        },
                        (UnOp::Neg, v) => Value::Num(-self.to_num(v)?),
                        (UnOp::Plus, v) => Value::Num(self.to_num(v)?),
                        (UnOp::BitNot, v) => Value::Num(!(to_i32(self.to_num(v)?)) as f64),
                    };
                    f.regs[dst as usize] = r;
                }
                Op::Step { dst, src, delta } => {
                    let v = match &f.regs[src as usize] {
                        Value::Num(n) => Value::Num(n + delta as f64),
                        Value::BigInt(b) => Value::BigInt(b.checked_add(delta as i128).ok_or_else(bigint::too_big)?),
                        v => {
                            let v = v.clone();
                            Value::Num(self.to_num(&v)? + delta as f64)
                        }
                    };
                    f.regs[dst as usize] = v;
                }
                Op::ToNumeric { dst, src } => {
                    let v = match &f.regs[src as usize] {
                        v @ (Value::Num(_) | Value::BigInt(_)) => v.clone(),
                        v => {
                            let v = v.clone();
                            Value::Num(self.to_num(&v)?)
                        }
                    };
                    f.regs[dst as usize] = v;
                }
                Op::Jump { to } => *ip = to as usize,
                Op::Loop { to } => {
                    self.tick()?;
                    *ip = to as usize;
                }
                Op::JumpIf { cond, to } => {
                    if f.regs[cond as usize].truthy() {
                        *ip = to as usize;
                    }
                }
                Op::JumpIfNot { cond, to } => {
                    if !f.regs[cond as usize].truthy() {
                        *ip = to as usize;
                    }
                }
                Op::JumpIfNullish { src, to } => {
                    if f.regs[src as usize].is_nullish() {
                        *ip = to as usize;
                    }
                }
                Op::JumpIfNotNullish { src, to } => {
                    if !f.regs[src as usize].is_nullish() {
                        *ip = to as usize;
                    }
                }
                Op::JumpIfNotUndef { src, to } => {
                    if !matches!(f.regs[src as usize], Value::Undef) {
                        *ip = to as usize;
                    }
                }
                Op::GetProp { dst, obj, name } => {
                    let o = f.regs[obj as usize].clone();
                    f.regs[dst as usize] = self.get(&o, &code.names[name as usize])?;
                }
                Op::SetProp { obj, name, src } => {
                    let o = f.regs[obj as usize].clone();
                    let v = f.regs[src as usize].clone();
                    self.put(&o, &code.names[name as usize], v)?;
                }
                Op::GetIndex { dst, obj, key } => {
                    let o = f.regs[obj as usize].clone();
                    let k = f.regs[key as usize].clone();
                    f.regs[dst as usize] = self.get_v(&o, &k)?;
                }
                Op::SetIndex { obj, key, src } => {
                    let o = f.regs[obj as usize].clone();
                    let k = f.regs[key as usize].clone();
                    let v = f.regs[src as usize].clone();
                    self.put_v(&o, &k, v)?;
                }
                Op::DelProp { dst, obj, name } => {
                    let o = f.regs[obj as usize].clone();
                    f.regs[dst as usize] = Value::Bool(self.delete(&o, &code.names[name as usize]));
                }
                Op::DelIndex { dst, obj, key } => {
                    let o = f.regs[obj as usize].clone();
                    let k = f.regs[key as usize].clone();
                    let k = self.key(&k)?;
                    f.regs[dst as usize] = Value::Bool(self.delete(&o, &k));
                }
                Op::Call { dst, f: fr, this, args, argc } => {
                    let func = f.regs[fr as usize].clone();
                    if !Self::is_callable(&func) {
                        return Err(type_error(format!("{} is not a function", Self::vm_describe(&func))));
                    }
                    let th = f.regs[this as usize].clone();
                    let a = f.regs[args as usize..args as usize + argc as usize].to_vec();
                    let v = self.call(&func, th, &a)?;
                    f.regs[dst as usize] = v;
                }
                Op::CallArr { dst, f: fr, this, arr } => {
                    let func = f.regs[fr as usize].clone();
                    if !Self::is_callable(&func) {
                        return Err(type_error(format!("{} is not a function", Self::vm_describe(&func))));
                    }
                    let th = f.regs[this as usize].clone();
                    let a = Self::vm_array_items(&f.regs[arr as usize]);
                    let v = self.call(&func, th, &a)?;
                    f.regs[dst as usize] = v;
                }
                Op::New { dst, f: fr, args, argc } => {
                    let func = f.regs[fr as usize].clone();
                    if !Self::is_callable(&func) {
                        return Err(type_error("not a constructor"));
                    }
                    let a = f.regs[args as usize..args as usize + argc as usize].to_vec();
                    f.regs[dst as usize] = self.construct(&func, &a)?;
                }
                Op::NewArr { dst, f: fr, arr } => {
                    let func = f.regs[fr as usize].clone();
                    if !Self::is_callable(&func) {
                        return Err(type_error("not a constructor"));
                    }
                    let a = Self::vm_array_items(&f.regs[arr as usize]);
                    f.regs[dst as usize] = self.construct(&func, &a)?;
                }
                Op::Array { dst, start, count } => {
                    let items = f.regs[start as usize..start as usize + count as usize].to_vec();
                    f.regs[dst as usize] = self.array(items);
                }
                Op::ArrayPush { arr, src } => {
                    let v = f.regs[src as usize].clone();
                    if let Value::Obj(o) = &f.regs[arr as usize] {
                        if let Kind::Array(a) = &mut o.borrow_mut().kind {
                            a.push(v);
                        }
                    }
                }
                Op::ArraySpread { arr, src } => {
                    let v = f.regs[src as usize].clone();
                    let items = self.iterate(&v)?;
                    if let Value::Obj(o) = &f.regs[arr as usize] {
                        if let Kind::Array(a) = &mut o.borrow_mut().kind {
                            a.extend(items);
                        }
                    }
                }
                Op::ArrayHole { arr } => {
                    if let Value::Obj(o) = &f.regs[arr as usize] {
                        if let Kind::Array(a) = &mut o.borrow_mut().kind {
                            a.push(Value::Hole);
                        }
                    }
                }
                Op::Object { dst } => f.regs[dst as usize] = Value::Obj(self.plain()),
                Op::DefProp { obj, name, src, method } => {
                    let key = code.names[name as usize].clone();
                    let v = f.regs[src as usize].clone();
                    self.vm_def_prop(&f.regs[obj as usize].clone(), key, v, method);
                }
                Op::DefPropKey { obj, key, src, method } => {
                    let k = f.regs[key as usize].clone();
                    let key = self.key(&k)?;
                    let v = f.regs[src as usize].clone();
                    self.vm_def_prop(&f.regs[obj as usize].clone(), key, v, method);
                }
                Op::DefAccessor { obj, key, func, getter } => {
                    let k = f.regs[key as usize].clone();
                    let key = self.key(&k)?;
                    let fv = f.regs[func as usize].clone();
                    if let Value::Obj(o) = &f.regs[obj as usize] {
                        if let (Value::Obj(fo), Kind::Func(Func::Closure { home, .. })) = (&fv, &mut fv.obj().unwrap().borrow_mut().kind) {
                            let _ = fo;
                            *home = Some(o.clone());
                        }
                        let mut ob = o.borrow_mut();
                        match ob.props.get_mut(&key) {
                            Some(pr) => {
                                if getter {
                                    pr.get = Some(fv)
                                } else {
                                    pr.set = Some(fv)
                                }
                            }
                            None => {
                                let pr = if getter { Prop { v: Value::Undef, get: Some(fv), set: None, enumerable: true } } else { Prop { v: Value::Undef, get: None, set: Some(fv), enumerable: true } };
                                ob.props.insert(key, pr);
                            }
                        }
                    }
                }
                Op::ObjSpread { obj, src } => {
                    let v = f.regs[src as usize].clone();
                    let o = f.regs[obj as usize].clone();
                    if let Value::Obj(ob) = &o {
                        for k in self.own_keys(&v) {
                            let pv = self.get(&v, &k)?;
                            ob.borrow_mut().props.insert(k, Prop::data(pv));
                        }
                    }
                }
                Op::Closure { dst, func } => {
                    let env = Self::vm_env(f).clone();
                    f.regs[dst as usize] = self.make_closure(&code.funcs[func as usize], &env);
                }
                Op::Class { dst, class } => {
                    let env = Self::vm_env(f).clone();
                    f.regs[dst as usize] = self.make_class(&code.classes[class as usize], &env)?;
                }
                Op::Regex { dst, idx } => {
                    let (b, fl) = &code.regexes[idx as usize];
                    f.regs[dst as usize] = builtins::make_regex(self, b, fl)?;
                }
                Op::SetFnName { f: fr, name } => {
                    name_fn(&f.regs[fr as usize], &Pattern::Ident(code.names[name as usize].clone()));
                }
                Op::Concat { dst, start, count } => {
                    let mut s = String::new();
                    for i in start..start + count {
                        match &f.regs[i as usize] {
                            Value::Str(x) => s.push_str(x),
                            v => {
                                let v = v.clone();
                                s.push_str(&self.to_str(&v)?);
                            }
                        }
                    }
                    f.regs[dst as usize] = Value::Str(s.into());
                }
                Op::Island { dst, expr } => {
                    let env = Self::vm_env(f).clone();
                    // SAFETY: the expression belongs to `code.def`, which
                    // `code` keeps alive.
                    let e = unsafe { &*code.exprs[expr as usize] };
                    f.regs[dst as usize] = self.eval(e, &env)?;
                }
                Op::IslandStmt { stmt } => {
                    let env = Self::vm_env(f).clone();
                    // SAFETY: as for Island.
                    let s = unsafe { &*code.stmts[stmt as usize] };
                    if let Flow::Ret(v) = self.exec(s, &env)? {
                        return Ok(v);
                    }
                }
                Op::BindPattern { pat, src, kind } => {
                    let env = Self::vm_env(f).clone();
                    // SAFETY: as for Island.
                    let p = unsafe { &*code.pats[pat as usize] };
                    let v = f.regs[src as usize].clone();
                    let kind = match kind {
                        0 => Some(VarKind::Var),
                        1 => Some(VarKind::Let),
                        2 => Some(VarKind::Const),
                        _ => None,
                    };
                    self.bind(p, v, &env, kind)?;
                }
                Op::ToArray { dst, src } => {
                    let v = f.regs[src as usize].clone();
                    if v.is_nullish() {
                        return Err(type_error("cannot destructure null"));
                    }
                    let items = self.iterate(&v)?;
                    f.regs[dst as usize] = self.array(items);
                }
                Op::Slice { dst, src, from } => {
                    let items = Self::vm_array_items(&f.regs[src as usize]);
                    f.regs[dst as usize] = self.array(items.get(from as usize..).map(|s| s.to_vec()).unwrap_or_default());
                }
                Op::RequireObject { src } => {
                    let v = &f.regs[src as usize];
                    if v.is_nullish() {
                        return Err(type_error(format!("Cannot destructure '{:?}'", v)));
                    }
                }
                Op::Return { src } => return Ok(std::mem::take(&mut f.regs[src as usize])),
                Op::Throw { src } => {
                    let v = f.regs[src as usize].clone();
                    if trace_throw() {
                        let m = self.describe_error(&v);
                        self.log(&format!("[throw] {m}{}", trace_stack()));
                    }
                    return Err(v);
                }
                Op::TryPush { target, exc, catch } => {
                    f.handlers.push(Handler { target: target as usize, exc, catch, envs: f.envs.len() });
                }
                Op::TryPop => {
                    f.handlers.pop();
                }
                Op::PushScope => {
                    let env = Self::vm_env(f).clone();
                    f.envs.push(new_scope(Some(env), None, None, false));
                }
                Op::PopScope { n } => {
                    let keep = f.envs.len().saturating_sub(n as usize).max(1);
                    f.envs.truncate(keep);
                }
                Op::CopyScope => {
                    let cur = f.envs.pop().unwrap();
                    let copy = new_scope(cur.parent.clone(), None, None, false);
                    *copy.vars.borrow_mut() = cur.vars.borrow().clone();
                    f.envs.push(copy);
                }
                Op::Await { dst, src } => {
                    let v = f.regs[src as usize].clone();
                    f.regs[dst as usize] = builtins::await_value(self, v)?;
                }
                Op::Yield { dst, src, delegate } => {
                    let v = f.regs[src as usize].clone();
                    f.regs[dst as usize] = match self.co_yield(v.clone(), delegate) {
                        Some(r) => r?,
                        None => {
                            // Eagerly collected generator.
                            let items = if delegate { self.iterate(&v)? } else { vec![v] };
                            if let Some(sink) = self.gen_sinks.last_mut() {
                                sink.extend(items);
                                if sink.len() > GEN_CAP {
                                    return Err(Value::str(GEN_CAP_MARKER));
                                }
                            }
                            Value::Undef
                        }
                    };
                }
                Op::IterStart { slot, src, of } => {
                    let v = f.regs[src as usize].clone();
                    let st = if of {
                        match self.lazy_iterator(&v)? {
                            Some((iter, next)) => IterState::Lazy { iter, next },
                            None => IterState::Values(self.iterate(&v)?, 0),
                        }
                    } else {
                        IterState::Keys(self.for_in_keys(&v), 0, v)
                    };
                    f.iters[slot as usize] = Some(st);
                }
                Op::IterNext { slot, dst, end } => {
                    let next = match &mut f.iters[slot as usize] {
                        Some(IterState::Values(items, i)) => {
                            let v = items.get(*i).cloned();
                            *i += 1;
                            v
                        }
                        Some(IterState::Keys(keys, i, _)) => {
                            let v = keys.get(*i).cloned().map(Value::Str);
                            *i += 1;
                            v
                        }
                        Some(IterState::Lazy { iter, next }) => {
                            let (iter, next) = (iter.clone(), next.clone());
                            let mut r = self.call(&next, iter, &[])?;
                            if matches!(&r, Value::Obj(p) if matches!(p.borrow().kind, Kind::Promise(_))) {
                                r = builtins::await_value(self, r)?;
                            }
                            if self.get(&r, "done")?.truthy() {
                                f.iters[slot as usize] = None;
                                None
                            } else {
                                Some(self.get(&r, "value")?)
                            }
                        }
                        None => None,
                    };
                    match next {
                        Some(v) => f.regs[dst as usize] = v,
                        None => {
                            f.iters[slot as usize] = None;
                            *ip = end as usize;
                        }
                    }
                }
                Op::IterClose { slot } => {
                    if let Some(IterState::Lazy { iter, .. }) = f.iters[slot as usize].take() {
                        let ret = self.get(&iter, "return")?;
                        if Self::is_callable(&ret) {
                            self.call(&ret, iter, &[])?;
                        }
                    }
                }
            }
        }
    }

    fn vm_array_items(v: &Value) -> Vec<Value> {
        match v {
            Value::Obj(o) => match &o.borrow().kind {
                Kind::Array(a) => a.iter().cloned().map(Value::unhole).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn vm_describe(v: &Value) -> String {
        match v {
            Value::Obj(_) => "object".into(),
            v => format!("{v:?}"),
        }
    }

    /// An object literal property (methods get their home object and name).
    fn vm_def_prop(&mut self, o: &Value, key: Rc<str>, v: Value, method: bool) {
        let Value::Obj(ob) = o else { return };
        if method {
            if let Value::Obj(f) = &v {
                if let Kind::Func(Func::Closure { home, def, .. }) = &mut f.borrow_mut().kind {
                    if !def.arrow {
                        *home = Some(ob.clone());
                    }
                }
            }
            name_fn(&v, &Pattern::Ident(key.clone()));
        }
        if &*key == "__proto__" {
            if let Value::Obj(p) = &v {
                ob.borrow_mut().proto = Some(p.clone());
            }
            return;
        }
        ob.borrow_mut().props.insert(key, Prop::data(v));
    }
}

/// Does a function's own body (not nested functions) contain a loop?
fn has_loop(def: &FuncDef) -> bool {
    fn st(s: &Stmt) -> bool {
        match s {
            Stmt::For(..) | Stmt::ForIn(..) | Stmt::ForOf(..) | Stmt::While(..) | Stmt::DoWhile(..) => true,
            Stmt::If(_, a, b) => st(a) || b.as_deref().is_some_and(st),
            Stmt::Block(b) => b.iter().any(st),
            Stmt::Try(a, c, f) => a.iter().any(st) || c.as_ref().is_some_and(|(_, c)| c.iter().any(st)) || f.as_ref().is_some_and(|f| f.iter().any(st)),
            Stmt::Switch(_, cases) => cases.iter().any(|(_, b)| b.iter().any(st)),
            Stmt::Labeled(_, b) => st(b),
            _ => false,
        }
    }
    match &def.body {
        Body::Block(b) => b.iter().any(st),
        Body::Expr(_) => false,
    }
}

/// Calls before a function is compiled (`BOLT_VM_THRESHOLD`, default 2).
fn compile_threshold() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("BOLT_VM_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(2))
}

pub enum VmEntry {
    /// Not compiled yet: calls so far.
    Calls(u32),
    /// Compiled (None: the VM can't run it).
    Code(Option<Rc<Code>>),
}

/// `BOLT_VM_TRACE=1` logs each function's compilation.
pub fn vm_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BOLT_VM_TRACE").is_some())
}

/// `BOLT_VM=0` turns the bytecode VM off (everything runs on the AST).
pub fn vm_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("BOLT_VM").map_or(true, |v| v != "0"))
}
