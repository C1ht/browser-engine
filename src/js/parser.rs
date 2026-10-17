//! JavaScript parser: ES2015+ subset into an AST (Pratt parser for
//! expressions, recursive descent for statements, ASI).

use super::lexer::{tokenize, Tok, Token};
use std::rc::Rc;

pub type Name = Rc<str>;

#[derive(Debug)]
pub enum Expr {
    Num(f64),
    BigInt(i128),
    Str(Name),
    Template(Vec<Name>, Vec<Expr>),
    Regex(Name, Name),
    Ident(Name),
    This,
    Super,
    Null,
    Bool(bool),
    Array(Vec<ArrItem>),
    Object(Vec<PropDef>),
    Func(Rc<FuncDef>),
    Class(Rc<ClassDef>),
    Unary(&'static str, Box<Expr>),
    Update { op: &'static str, prefix: bool, target: Box<Expr> },
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Logical(&'static str, Box<Expr>, Box<Expr>),
    Assign(&'static str, Box<Pattern>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<ArrItem>, bool),
    New(Box<Expr>, Vec<ArrItem>),
    Member(Box<Expr>, Name, bool),
    Index(Box<Expr>, Box<Expr>, bool),
    Seq(Vec<Expr>),
    Await(Box<Expr>),
    /// `import(specifier)`.
    ImportCall(Box<Expr>),
    /// `import.meta`.
    ImportMeta,
    /// `yield` / `yield*`
    Yield(Option<Box<Expr>>, bool),
    TaggedTemplate(Box<Expr>, Vec<Name>, Vec<Expr>),
}

#[derive(Debug)]
pub enum ArrItem {
    Hole,
    Item(Expr),
    Spread(Expr),
}

#[derive(Debug)]
pub enum PropKey {
    Name(Name),
    Computed(Expr),
}

#[derive(Debug)]
pub enum PropDef {
    Value(PropKey, Expr),
    Getter(PropKey, Rc<FuncDef>),
    Setter(PropKey, Rc<FuncDef>),
    Spread(Expr),
}

#[derive(Debug)]
pub enum Pattern {
    Ident(Name),
    /// A member expression target (assignments only).
    Expr(Expr),
    Object(Vec<(PropKey, Pattern)>, Option<Box<Pattern>>),
    Array(Vec<Option<Pattern>>, Option<Box<Pattern>>),
    Default(Box<Pattern>, Expr),
}

#[derive(Debug)]
pub enum Body {
    Block(Vec<Stmt>),
    Expr(Expr),
}

#[derive(Debug)]
pub struct FuncDef {
    pub name: Option<Name>,
    pub params: Vec<Pattern>,
    pub rest: Option<Pattern>,
    pub body: Body,
    pub arrow: bool,
    pub is_async: bool,
    pub is_gen: bool,
    /// Strict mode code (`this` isn't coerced to the global object).
    pub strict: bool,
    /// Bytecode compiled for this function (see `vm.rs`).
    pub vm: std::cell::RefCell<super::vm::VmEntry>,
}

#[derive(Debug)]
pub struct ClassDef {
    pub name: Option<Name>,
    pub extends: Option<Expr>,
    pub ctor: Option<Rc<FuncDef>>,
    /// (is_static, key, kind, function)
    pub methods: Vec<(bool, PropKey, MethodKind, Rc<FuncDef>)>,
    /// (is_static, key, initializer)
    pub fields: Vec<(bool, PropKey, Option<Expr>)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MethodKind {
    Method,
    Get,
    Set,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug)]
pub enum ForInit {
    Var(VarKind, Pattern),
    Target(Pattern),
}

#[derive(Debug)]
pub enum Stmt {
    Expr(Expr),
    Var(VarKind, Vec<(Pattern, Option<Expr>)>),
    Func(Rc<FuncDef>),
    Class(Rc<ClassDef>),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    ForIn(ForInit, Expr, Box<Stmt>),
    ForOf(ForInit, Expr, Box<Stmt>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Expr),
    Try(Vec<Stmt>, Option<(Option<Pattern>, Vec<Stmt>)>, Option<Vec<Stmt>>),
    Switch(Expr, Vec<(Option<Expr>, Vec<Stmt>)>),
    Labeled(Name, Box<Stmt>),
    Empty,
}

pub struct Parser {
    toks: Vec<Token>,
    i: usize,
    in_func: bool,
    /// Inside a generator body (`yield` is an operator).
    in_gen: bool,
    /// Parsing a module: import / export declarations are collected here.
    module: Option<ModuleInfo>,
    /// Inside strict mode code.
    strict: bool,
}

/// What a module imports and exports (its body has the `import` / `export`
/// syntax stripped; exported declarations remain as plain statements).
#[derive(Debug, Default)]
pub struct ModuleInfo {
    /// Module specifiers, in source order (deduplicated).
    pub requests: Vec<Name>,
    /// (imported name, local name, request index).
    pub imports: Vec<(ImportName, Name, usize)>,
    pub exports: Vec<ExportEntry>,
}

#[derive(Debug)]
pub enum ImportName {
    /// `import x from` (the "default" export).
    Named(Name),
    /// `import * as ns from`.
    Namespace,
}

#[derive(Debug)]
pub enum ExportEntry {
    /// A binding of this module.
    Local { local: Name, exported: Name },
    /// `export { a as b } from "m"`; imported "*" = `export * as b from`.
    Indirect { req: usize, imported: Name, exported: Name },
    /// `export * from "m"`.
    Star { req: usize },
}

/// The local binding `export default <expression>` creates.
pub const DEFAULT_LOCAL: &str = "*default*";

fn bound_names(p: &Pattern, out: &mut Vec<Name>) {
    match p {
        Pattern::Ident(n) => out.push(n.clone()),
        Pattern::Expr(_) => {}
        Pattern::Object(props, rest) => {
            props.iter().for_each(|(_, p)| bound_names(p, out));
            if let Some(r) = rest {
                bound_names(r, out);
            }
        }
        Pattern::Array(items, rest) => {
            items.iter().flatten().for_each(|p| bound_names(p, out));
            if let Some(r) = rest {
                bound_names(r, out);
            }
        }
        Pattern::Default(p, _) => bound_names(p, out),
    }
}

/// Replace an error's `@@offset` marker with the line, column and source text.
fn locate(src: &str, e: String) -> String {
    let Some(i) = e.rfind("@@") else { return e };
    let Ok(pos) = e[i + 2..].trim().parse::<usize>() else { return e };
    let chars: Vec<char> = src.chars().collect();
    let pos = pos.min(chars.len());
    let line = chars[..pos].iter().filter(|&&c| c == '\n').count() + 1;
    let line_start = chars[..pos].iter().rposition(|&c| c == '\n').map_or(0, |p| p + 1);
    let from = pos.saturating_sub(40).max(line_start);
    let to = (pos + 40).min(chars.len());
    let snip: String = chars[from..to].iter().map(|&c| if c == '\n' { ' ' } else { c }).collect();
    format!("{}(line {line}, col {}: `{snip}`)", &e[..i], pos - line_start + 1)
}

/// Parse an ES module.
pub fn parse_module(src: &str) -> PResult<(Vec<Stmt>, ModuleInfo)> {
    parse_module_inner(src).map_err(|e| locate(src, e))
}

fn parse_module_inner(src: &str) -> PResult<(Vec<Stmt>, ModuleInfo)> {
    let toks = tokenize(src).map_err(|e| format!("SyntaxError: {} at {}", e.0, e.1))?;
    // Top-level await is allowed in modules.
    let mut p = Parser { toks, i: 0, in_func: true, in_gen: false, module: Some(ModuleInfo::default()), strict: true };
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.statement()?);
    }
    Ok((out, p.module.take().unwrap_or_default()))
}

pub type PResult<T> = Result<T, String>;

pub fn parse_program(src: &str) -> PResult<Vec<Stmt>> {
    parse_program_inner(src).map_err(|e| locate(src, e))
}

fn parse_program_inner(src: &str) -> PResult<Vec<Stmt>> {
    let toks = tokenize(src).map_err(|e| format!("SyntaxError: {} at {}", e.0, e.1))?;
    let mut p = Parser { toks, i: 0, in_func: false, in_gen: false, module: None, strict: false };
    p.strict = p.use_strict_directive();
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.statement()?);
    }
    Ok(out)
}

/// Parse a function body given parameter and body source (for `new Function`
/// and inline event handler attributes).
pub fn parse_function(params: &[&str], body: &str) -> PResult<Rc<FuncDef>> {
    let src = format!("(function({}) {{\n{}\n}})", params.join(","), body);
    let mut prog = parse_program(&src)?;
    match prog.pop() {
        Some(Stmt::Expr(Expr::Func(f))) => Ok(f),
        _ => Err("bad function".into()),
    }
}

fn parse_expr_src(src: &str) -> PResult<Expr> {
    let toks = tokenize(src).map_err(|e| format!("SyntaxError: {}", e.0))?;
    let mut p = Parser { toks, i: 0, in_func: true, in_gen: false, module: None, strict: false };
    let e = p.expression(false)?;
    if !p.at_eof() {
        return Err("SyntaxError: junk in template expression".into());
    }
    Ok(e)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }
    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.i + k).min(self.toks.len() - 1)].tok
    }
    fn nl_before(&self) -> bool {
        self.toks[self.i].nl
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }
    fn next(&mut self) -> Tok {
        let t = self.toks[self.i].tok.clone();
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
        t
    }
    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(x) if *x == p)
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Ident(x) if &**x == w)
    }
    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn err<T>(&self, msg: &str) -> PResult<T> {
        Err(format!("SyntaxError: {msg} near {:?} @@{}", self.peek(), self.pos()))
    }

    /// Source offset of the current token.
    fn pos(&self) -> u32 {
        self.toks.get(self.i).or(self.toks.last()).map_or(0, |t| t.pos)
    }
    fn expect(&mut self, p: &str) -> PResult<()> {
        if self.eat(p) {
            Ok(())
        } else {
            self.err(&format!("expected {p}"))
        }
    }
    fn semicolon(&mut self) -> PResult<()> {
        if self.eat(";") || self.is("}") || self.at_eof() || self.nl_before() {
            Ok(())
        } else {
            self.err("expected ;")
        }
    }
    fn ident(&mut self) -> PResult<Name> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            t => Err(format!("SyntaxError: expected identifier, got {t:?}")),
        }
    }
    /// Property names may be any identifier, keyword, string or number.
    fn prop_name(&mut self) -> PResult<PropKey> {
        Ok(match self.next() {
            Tok::Ident(s) | Tok::Str(s) => PropKey::Name(s),
            Tok::Num(n) => PropKey::Name(super::num_to_str(n).into()),
            Tok::Punct("[") => {
                let e = self.assign(false)?;
                self.expect("]")?;
                PropKey::Computed(e)
            }
            t => return Err(format!("SyntaxError: bad property name {t:?}")),
        })
    }

    // ------------------------------------------------------------ statements

    fn statement(&mut self) -> PResult<Stmt> {
        let t = self.peek().clone();
        match t {
            Tok::Punct("{") => {
                self.next();
                Ok(Stmt::Block(self.block_body()?))
            }
            Tok::Punct(";") => {
                self.next();
                Ok(Stmt::Empty)
            }
            Tok::Ident(w) => match &*w {
                "var" | "const" => {
                    self.next();
                    let kind = if &*w == "var" { VarKind::Var } else { VarKind::Const };
                    let d = self.var_decls(kind, false)?;
                    self.semicolon()?;
                    Ok(d)
                }
                "let" if matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{")) => {
                    self.next();
                    let d = self.var_decls(VarKind::Let, false)?;
                    self.semicolon()?;
                    Ok(d)
                }
                "function" => {
                    self.next();
                    Ok(Stmt::Func(self.function_rest(false, true)?))
                }
                "async" if matches!(self.peek_at(1), Tok::Ident(x) if &**x == "function") && !self.toks[self.i + 1].nl => {
                    self.next();
                    self.next();
                    Ok(Stmt::Func(self.function_rest(true, true)?))
                }
                "class" => {
                    self.next();
                    Ok(Stmt::Class(self.class_rest(true)?))
                }
                "if" => {
                    self.next();
                    self.expect("(")?;
                    let c = self.expression(false)?;
                    self.expect(")")?;
                    let a = self.statement()?;
                    let b = if self.eat_word("else") { Some(Box::new(self.statement()?)) } else { None };
                    Ok(Stmt::If(c, Box::new(a), b))
                }
                "for" => self.for_stmt(),
                "while" => {
                    self.next();
                    self.expect("(")?;
                    let c = self.expression(false)?;
                    self.expect(")")?;
                    Ok(Stmt::While(c, Box::new(self.statement()?)))
                }
                "do" => {
                    self.next();
                    let body = self.statement()?;
                    if !self.eat_word("while") {
                        return self.err("expected while");
                    }
                    self.expect("(")?;
                    let c = self.expression(false)?;
                    self.expect(")")?;
                    self.eat(";");
                    Ok(Stmt::DoWhile(Box::new(body), c))
                }
                "return" => {
                    self.next();
                    let v = if self.is(";") || self.is("}") || self.at_eof() || self.nl_before() { None } else { Some(self.expression(false)?) };
                    self.semicolon()?;
                    Ok(Stmt::Return(v))
                }
                "break" | "continue" => {
                    self.next();
                    let label = match self.peek() {
                        Tok::Ident(l) if !self.nl_before() => {
                            let l = l.clone();
                            self.next();
                            Some(l)
                        }
                        _ => None,
                    };
                    self.semicolon()?;
                    Ok(if &*w == "break" { Stmt::Break(label) } else { Stmt::Continue(label) })
                }
                "throw" => {
                    self.next();
                    let e = self.expression(false)?;
                    self.semicolon()?;
                    Ok(Stmt::Throw(e))
                }
                "try" => {
                    self.next();
                    self.expect("{")?;
                    let block = self.block_body()?;
                    let catch = if self.eat_word("catch") {
                        let param = if self.eat("(") {
                            let p = self.binding_target()?;
                            self.expect(")")?;
                            Some(p)
                        } else {
                            None
                        };
                        self.expect("{")?;
                        Some((param, self.block_body()?))
                    } else {
                        None
                    };
                    let finally = if self.eat_word("finally") {
                        self.expect("{")?;
                        Some(self.block_body()?)
                    } else {
                        None
                    };
                    Ok(Stmt::Try(block, catch, finally))
                }
                "switch" => {
                    self.next();
                    self.expect("(")?;
                    let d = self.expression(false)?;
                    self.expect(")")?;
                    self.expect("{")?;
                    let mut cases = Vec::new();
                    while !self.eat("}") {
                        let test = if self.eat_word("case") {
                            Some(self.expression(false)?)
                        } else if self.eat_word("default") {
                            None
                        } else {
                            return self.err("expected case");
                        };
                        self.expect(":")?;
                        let mut body = Vec::new();
                        while !self.is("}") && !self.is_word("case") && !self.is_word("default") {
                            body.push(self.statement()?);
                        }
                        cases.push((test, body));
                    }
                    Ok(Stmt::Switch(d, cases))
                }
                "import" if self.module.is_some() && !matches!(self.peek_at(1), Tok::Punct("(") | Tok::Punct(".")) => {
                    self.next();
                    self.import_decl()
                }
                "export" if self.module.is_some() => {
                    self.next();
                    self.export_decl()
                }
                "import" if !matches!(self.peek_at(1), Tok::Punct("(") | Tok::Punct(".")) => self.err("Cannot use import statement outside a module"),
                "export" => self.err("Unexpected token 'export'"),
                _ if matches!(self.peek_at(1), Tok::Punct(":")) && !matches!(&*w, "true" | "false" | "null" | "this") => {
                    self.next();
                    self.next();
                    Ok(Stmt::Labeled(w, Box::new(self.statement()?)))
                }
                _ => self.expr_stmt(),
            },
            _ => self.expr_stmt(),
        }
    }

    fn expr_stmt(&mut self) -> PResult<Stmt> {
        let e = self.expression(false)?;
        self.semicolon()?;
        Ok(Stmt::Expr(e))
    }

    // --------------------------------------------------------------- modules

    fn request(&mut self) -> PResult<usize> {
        let spec = match self.next() {
            Tok::Str(s) => s,
            t => return Err(format!("SyntaxError: expected module specifier, got {t:?}")),
        };
        // Import attributes (`with { type: "json" }`) are ignored.
        if (self.is_word("with") || self.is_word("assert")) && !self.nl_before() {
            self.next();
            self.expect("{")?;
            let mut depth = 1;
            while depth > 0 && !self.at_eof() {
                match self.next() {
                    Tok::Punct("{") => depth += 1,
                    Tok::Punct("}") => depth -= 1,
                    _ => {}
                }
            }
        }
        let m = self.module.as_mut().unwrap();
        Ok(match m.requests.iter().position(|r| *r == spec) {
            Some(i) => i,
            None => {
                m.requests.push(spec);
                m.requests.len() - 1
            }
        })
    }

    /// An import / export binding name (identifier or string).
    fn module_name(&mut self) -> PResult<Name> {
        match self.next() {
            Tok::Ident(s) | Tok::Str(s) => Ok(s),
            t => Err(format!("SyntaxError: expected name, got {t:?}")),
        }
    }

    fn import_decl(&mut self) -> PResult<Stmt> {
        if matches!(self.peek(), Tok::Str(_)) {
            self.request()?;
            self.semicolon()?;
            return Ok(Stmt::Empty);
        }
        let mut specs: Vec<(ImportName, Name)> = Vec::new();
        if matches!(self.peek(), Tok::Ident(_)) {
            let local = self.ident()?;
            specs.push((ImportName::Named("default".into()), local));
            self.eat(",");
        }
        if self.eat("*") {
            if !self.eat_word("as") {
                return self.err("expected 'as'");
            }
            specs.push((ImportName::Namespace, self.ident()?));
        } else if self.eat("{") {
            while !self.eat("}") {
                let imported = self.module_name()?;
                let local = if self.eat_word("as") { self.ident()? } else { imported.clone() };
                specs.push((ImportName::Named(imported), local));
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
        }
        if !self.eat_word("from") {
            return self.err("expected 'from'");
        }
        let req = self.request()?;
        self.semicolon()?;
        let m = self.module.as_mut().unwrap();
        for (name, local) in specs {
            m.imports.push((name, local, req));
        }
        Ok(Stmt::Empty)
    }

    fn export_local(&mut self, local: Name, exported: Name) {
        self.module.as_mut().unwrap().exports.push(ExportEntry::Local { local, exported });
    }

    fn export_decl(&mut self) -> PResult<Stmt> {
        if self.eat("*") {
            let exported = if self.eat_word("as") { Some(self.module_name()?) } else { None };
            if !self.eat_word("from") {
                return self.err("expected 'from'");
            }
            let req = self.request()?;
            self.semicolon()?;
            let e = match exported {
                Some(exported) => ExportEntry::Indirect { req, imported: "*".into(), exported },
                None => ExportEntry::Star { req },
            };
            self.module.as_mut().unwrap().exports.push(e);
            return Ok(Stmt::Empty);
        }
        if self.eat_word("default") {
            let is_async_fn = self.is_word("async") && matches!(self.peek_at(1), Tok::Ident(x) if &**x == "function") && !self.toks[self.i + 1].nl;
            if self.is_word("function") || is_async_fn {
                if is_async_fn {
                    self.next();
                }
                self.next();
                let f = self.function_rest(is_async_fn, false)?;
                return Ok(match f.name.clone() {
                    Some(n) => {
                        self.export_local(n, "default".into());
                        Stmt::Func(f)
                    }
                    None => {
                        self.export_local(DEFAULT_LOCAL.into(), "default".into());
                        Stmt::Var(VarKind::Let, vec![(Pattern::Ident(DEFAULT_LOCAL.into()), Some(Expr::Func(f)))])
                    }
                });
            }
            if self.eat_word("class") {
                let c = self.class_rest(false)?;
                return Ok(match c.name.clone() {
                    Some(n) => {
                        self.export_local(n, "default".into());
                        Stmt::Class(c)
                    }
                    None => {
                        self.export_local(DEFAULT_LOCAL.into(), "default".into());
                        Stmt::Var(VarKind::Let, vec![(Pattern::Ident(DEFAULT_LOCAL.into()), Some(Expr::Class(c)))])
                    }
                });
            }
            let e = self.assign(false)?;
            self.semicolon()?;
            self.export_local(DEFAULT_LOCAL.into(), "default".into());
            return Ok(Stmt::Var(VarKind::Let, vec![(Pattern::Ident(DEFAULT_LOCAL.into()), Some(e))]));
        }
        if self.eat("{") {
            let mut names = Vec::new();
            while !self.eat("}") {
                let local = self.module_name()?;
                let exported = if self.eat_word("as") { self.module_name()? } else { local.clone() };
                names.push((local, exported));
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
            if self.eat_word("from") {
                let req = self.request()?;
                for (imported, exported) in names {
                    self.module.as_mut().unwrap().exports.push(ExportEntry::Indirect { req, imported, exported });
                }
            } else {
                for (local, exported) in names {
                    self.export_local(local, exported);
                }
            }
            self.semicolon()?;
            return Ok(Stmt::Empty);
        }
        let decl = self.statement()?;
        let mut names = Vec::new();
        match &decl {
            Stmt::Var(_, ds) => ds.iter().for_each(|(p, _)| bound_names(p, &mut names)),
            Stmt::Func(f) => names.extend(f.name.clone()),
            Stmt::Class(c) => names.extend(c.name.clone()),
            _ => return self.err("unexpected export"),
        }
        for n in names {
            self.export_local(n.clone(), n);
        }
        Ok(decl)
    }

    fn block_body(&mut self) -> PResult<Vec<Stmt>> {
        let mut out = Vec::new();
        while !self.eat("}") {
            if self.at_eof() {
                return self.err("unexpected end of input");
            }
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn var_decls(&mut self, kind: VarKind, no_in: bool) -> PResult<Stmt> {
        let mut decls = Vec::new();
        loop {
            let target = self.binding_target()?;
            let init = if self.eat("=") { Some(self.assign(no_in)?) } else { None };
            decls.push((target, init));
            if !self.eat(",") {
                break;
            }
        }
        Ok(Stmt::Var(kind, decls))
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        self.next();
        if self.is_word("await") {
            self.next();
        }
        self.expect("(")?;
        let mut init = None;
        if self.eat(";") {
        } else {
            let kind = match self.peek() {
                Tok::Ident(w) if &**w == "var" => Some(VarKind::Var),
                Tok::Ident(w) if &**w == "const" => Some(VarKind::Const),
                Tok::Ident(w) if &**w == "let" && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("[") | Tok::Punct("{")) => Some(VarKind::Let),
                _ => None,
            };
            if let Some(kind) = kind {
                self.next();
                let save = self.i;
                let target = self.binding_target()?;
                if self.eat_word("of") || self.eat_word("in") {
                    let is_of = matches!(&self.toks[self.i - 1].tok, Tok::Ident(w) if &**w == "of");
                    let obj = if is_of { self.assign(false)? } else { self.expression(false)? };
                    self.expect(")")?;
                    let body = Box::new(self.statement()?);
                    let fi = ForInit::Var(kind, target);
                    return Ok(if is_of { Stmt::ForOf(fi, obj, body) } else { Stmt::ForIn(fi, obj, body) });
                }
                self.i = save;
                init = Some(Box::new(self.var_decls(kind, true)?));
            } else {
                let e = self.expression(true)?;
                if self.eat_word("of") || self.eat_word("in") {
                    let is_of = matches!(&self.toks[self.i - 1].tok, Tok::Ident(w) if &**w == "of");
                    let obj = if is_of { self.assign(false)? } else { self.expression(false)? };
                    self.expect(")")?;
                    let body = Box::new(self.statement()?);
                    let fi = ForInit::Target(to_pattern(e)?);
                    return Ok(if is_of { Stmt::ForOf(fi, obj, body) } else { Stmt::ForIn(fi, obj, body) });
                }
                init = Some(Box::new(Stmt::Expr(e)));
            }
            self.expect(";")?;
        }
        let test = if self.is(";") { None } else { Some(self.expression(false)?) };
        self.expect(";")?;
        let update = if self.is(")") { None } else { Some(self.expression(false)?) };
        self.expect(")")?;
        Ok(Stmt::For(init, test, update, Box::new(self.statement()?)))
    }

    /// Identifier or destructuring pattern (with defaults inside).
    fn binding_target(&mut self) -> PResult<Pattern> {
        if self.eat("[") {
            let mut items = Vec::new();
            let mut rest = None;
            while !self.eat("]") {
                if self.eat(",") {
                    items.push(None);
                    continue;
                }
                if self.eat("...") {
                    rest = Some(Box::new(self.binding_target()?));
                    self.eat(",");
                    continue;
                }
                let t = self.binding_element()?;
                items.push(Some(t));
                if !self.is("]") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Array(items, rest));
        }
        if self.eat("{") {
            let mut props = Vec::new();
            let mut rest = None;
            while !self.eat("}") {
                if self.eat("...") {
                    rest = Some(Box::new(self.binding_target()?));
                    self.eat(",");
                    continue;
                }
                let key = self.prop_name()?;
                let target = if self.eat(":") {
                    self.binding_element()?
                } else {
                    let PropKey::Name(n) = &key else { return self.err("bad shorthand") };
                    let p = Pattern::Ident(n.clone());
                    if self.eat("=") { Pattern::Default(Box::new(p), self.assign(false)?) } else { p }
                };
                props.push((key, target));
                if !self.is("}") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Object(props, rest));
        }
        Ok(Pattern::Ident(self.ident()?))
    }

    fn binding_element(&mut self) -> PResult<Pattern> {
        let t = self.binding_target()?;
        Ok(if self.eat("=") { Pattern::Default(Box::new(t), self.assign(false)?) } else { t })
    }

    fn params(&mut self) -> PResult<(Vec<Pattern>, Option<Pattern>)> {
        self.expect("(")?;
        let mut ps = Vec::new();
        let mut rest = None;
        while !self.eat(")") {
            if self.eat("...") {
                rest = Some(self.binding_target()?);
                self.eat(",");
                continue;
            }
            ps.push(self.binding_element()?);
            if !self.is(")") {
                self.expect(",")?;
            }
        }
        Ok((ps, rest))
    }

    /// A `"use strict"` directive in the prologue starting at the cursor.
    fn use_strict_directive(&self) -> bool {
        let mut j = self.i;
        while let Some(Tok::Str(s)) = self.toks.get(j).map(|t| &t.tok) {
            if &**s == "use strict" {
                return true;
            }
            j += 1;
            if matches!(self.toks.get(j).map(|t| &t.tok), Some(Tok::Punct(";"))) {
                j += 1;
            }
        }
        false
    }

    /// Parse a function body; returns it and whether it is strict code.
    fn func_body(&mut self, is_gen: bool) -> PResult<(Vec<Stmt>, bool)> {
        self.expect("{")?;
        let saved = (self.in_func, self.in_gen, self.strict);
        self.in_func = true;
        self.in_gen = is_gen;
        self.strict = self.strict || self.use_strict_directive();
        let strict = self.strict;
        let b = self.block_body();
        (self.in_func, self.in_gen, self.strict) = saved;
        Ok((b?, strict))
    }

    fn function_rest(&mut self, is_async: bool, need_name: bool) -> PResult<Rc<FuncDef>> {
        let is_gen = self.eat("*");
        let name = match self.peek() {
            Tok::Ident(n) if !self.is("(") => {
                let n = n.clone();
                self.next();
                Some(n)
            }
            _ if need_name => return self.err("expected function name"),
            _ => None,
        };
        let (params, rest) = self.params()?;
        let (body, strict) = self.func_body(is_gen)?;
        Ok(Rc::new(FuncDef { name, params, rest, body: Body::Block(body), arrow: false, is_async, is_gen, strict, vm: Default::default() }))
    }

    fn method(&mut self, name: Option<Name>, is_async: bool, is_gen: bool) -> PResult<Rc<FuncDef>> {
        let (params, rest) = self.params()?;
        let (body, strict) = self.func_body(is_gen)?;
        Ok(Rc::new(FuncDef { name, params, rest, body: Body::Block(body), arrow: false, is_async, is_gen, strict, vm: Default::default() }))
    }

    fn class_rest(&mut self, need_name: bool) -> PResult<Rc<ClassDef>> {
        let name = match self.peek() {
            Tok::Ident(n) if &**n != "extends" => {
                let n = n.clone();
                self.next();
                Some(n)
            }
            _ if need_name => return self.err("expected class name"),
            _ => None,
        };
        let extends = if self.eat_word("extends") { Some(self.unary_postfix()?) } else { None };
        self.expect("{")?;
        // Class bodies are strict mode code.
        let saved_strict = std::mem::replace(&mut self.strict, true);
        let r = self.class_body(name, extends);
        self.strict = saved_strict;
        r
    }

    fn class_body(&mut self, name: Option<Name>, extends: Option<Expr>) -> PResult<Rc<ClassDef>> {
        let mut cd = ClassDef { name, extends, ctor: None, methods: Vec::new(), fields: Vec::new() };
        while !self.eat("}") {
            if self.eat(";") {
                continue;
            }
            let mut is_static = false;
            if self.is_word("static") && !matches!(self.peek_at(1), Tok::Punct("(") | Tok::Punct("=")) {
                self.next();
                is_static = true;
                if self.is("{") {
                    // static initialization block: run as a static method-less field
                    self.next();
                    let body = self.block_body()?;
                    let f = Rc::new(FuncDef { name: None, params: vec![], rest: None, body: Body::Block(body), arrow: false, is_async: false, is_gen: false, strict: true, vm: Default::default() });
                    cd.fields.push((true, PropKey::Name("".into()), Some(Expr::Call(Box::new(Expr::Func(f)), vec![], false))));
                    continue;
                }
            }
            let mut kind = MethodKind::Method;
            let mut is_async = false;
            if (self.is_word("get") || self.is_word("set")) && !matches!(self.peek_at(1), Tok::Punct("(") | Tok::Punct("=") | Tok::Punct(";") | Tok::Punct("}")) {
                kind = if self.is_word("get") { MethodKind::Get } else { MethodKind::Set };
                self.next();
            } else if self.is_word("async") && !matches!(self.peek_at(1), Tok::Punct("(") | Tok::Punct("=")) {
                self.next();
                is_async = true;
            }
            let is_gen = self.eat("*");
            let key = self.prop_name()?;
            if self.is("(") {
                let fname = if let PropKey::Name(n) = &key { Some(n.clone()) } else { None };
                let f = self.method(fname.clone(), is_async, is_gen)?;
                if !is_static && kind == MethodKind::Method && fname.as_deref() == Some("constructor") {
                    cd.ctor = Some(f);
                } else {
                    cd.methods.push((is_static, key, kind, f));
                }
            } else {
                let init = if self.eat("=") { Some(self.assign(false)?) } else { None };
                self.semicolon()?;
                cd.fields.push((is_static, key, init));
            }
        }
        Ok(Rc::new(cd))
    }

    // ----------------------------------------------------------- expressions

    pub fn expression(&mut self, no_in: bool) -> PResult<Expr> {
        let e = self.assign(no_in)?;
        if self.is(",") {
            let mut v = vec![e];
            while self.eat(",") {
                v.push(self.assign(no_in)?);
            }
            return Ok(Expr::Seq(v));
        }
        Ok(e)
    }

    /// Is the `(` at the current position the start of arrow parameters?
    fn arrow_ahead(&self) -> bool {
        let mut depth = 0;
        let mut k = self.i;
        while k < self.toks.len() {
            match &self.toks[k].tok {
                Tok::Punct("(") | Tok::Punct("[") | Tok::Punct("{") => depth += 1,
                Tok::Punct(")") | Tok::Punct("]") | Tok::Punct("}") => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.toks.get(k + 1).map(|t| &t.tok), Some(Tok::Punct("=>")));
                    }
                }
                Tok::Eof => return false,
                _ => {}
            }
            k += 1;
        }
        false
    }

    fn arrow_body(&mut self, params: Vec<Pattern>, rest: Option<Pattern>, is_async: bool) -> PResult<Expr> {
        self.expect("=>")?;
        let body = if self.is("{") {
            Body::Block(self.func_body(false)?.0)
        } else {
            Body::Expr(self.assign(false)?)
        };
        Ok(Expr::Func(Rc::new(FuncDef { name: None, params, rest, body, arrow: true, is_async, is_gen: false, strict: self.strict, vm: Default::default() })))
    }

    fn assign(&mut self, no_in: bool) -> PResult<Expr> {
        // Arrow functions.
        let is_async = self.is_word("async") && !self.toks[self.i + 1].nl && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("("));
        let off = is_async as usize;
        if matches!(self.peek_at(off), Tok::Ident(_)) && matches!(self.peek_at(off + 1), Tok::Punct("=>")) {
            if is_async {
                self.next();
            }
            let n = self.ident()?;
            return self.arrow_body(vec![Pattern::Ident(n)], None, is_async);
        }
        if matches!(self.peek_at(off), Tok::Punct("(")) {
            let save = self.i;
            self.i += off;
            if self.arrow_ahead() {
                let (ps, rest) = self.params()?;
                return self.arrow_body(ps, rest, is_async);
            }
            self.i = save;
        }
        if self.is_word("yield") && self.in_gen {
            self.next();
            let delegate = self.eat("*");
            let ends = self.nl_before() || matches!(self.peek(), Tok::Punct(")" | "]" | "}" | "," | ";" | ":") | Tok::Eof);
            let arg = if ends && !delegate { None } else { Some(Box::new(self.assign(no_in)?)) };
            return Ok(Expr::Yield(arg, delegate));
        }
        let lhs = self.conditional(no_in)?;
        if let Tok::Punct(op) = self.peek().clone() {
            if matches!(op, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>=" | ">>>=" | "&=" | "|=" | "^=" | "&&=" | "||=" | "??=") {
                self.next();
                let target = to_pattern(lhs)?;
                let rhs = self.assign(no_in)?;
                return Ok(Expr::Assign(op, Box::new(target), Box::new(rhs)));
            }
        }
        Ok(lhs)
    }

    fn conditional(&mut self, no_in: bool) -> PResult<Expr> {
        let c = self.binary(0, no_in)?;
        if self.eat("?") {
            let a = self.assign(false)?;
            self.expect(":")?;
            let b = self.assign(no_in)?;
            return Ok(Expr::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn binop(&self, no_in: bool) -> Option<(&'static str, u8)> {
        let p = match self.peek() {
            Tok::Punct(p) => *p,
            Tok::Ident(w) if &**w == "instanceof" => "instanceof",
            Tok::Ident(w) if &**w == "in" && !no_in => "in",
            _ => return None,
        };
        let prec = match p {
            "??" => 1,
            "||" => 2,
            "&&" => 3,
            "|" => 4,
            "^" => 5,
            "&" => 6,
            "==" | "!=" | "===" | "!==" => 7,
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 8,
            "<<" | ">>" | ">>>" => 9,
            "+" | "-" => 10,
            "*" | "/" | "%" => 11,
            "**" => 12,
            _ => return None,
        };
        Some((p, prec))
    }

    fn binary(&mut self, min: u8, no_in: bool) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        // Precedence climbing: operands of left-associative operators bind at
        // prec + 1, so equal-precedence operators fold left; `**` folds right.
        while let Some((op, prec)) = self.binop(no_in) {
            if prec < min {
                break;
            }
            self.next();
            let rhs = if op == "**" { self.binary(prec, no_in)? } else { self.binary(prec + 1, no_in)? };
            lhs = match op {
                "&&" | "||" | "??" => Expr::Logical(op, Box::new(lhs), Box::new(rhs)),
                _ => Expr::Binary(op, Box::new(lhs), Box::new(rhs)),
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        match self.peek().clone() {
            Tok::Punct(op @ ("!" | "~" | "+" | "-")) => {
                self.next();
                Ok(Expr::Unary(op, Box::new(self.unary()?)))
            }
            Tok::Punct(op @ ("++" | "--")) => {
                self.next();
                let t = self.unary()?;
                Ok(Expr::Update { op, prefix: true, target: Box::new(t) })
            }
            Tok::Ident(w) if matches!(&*w, "typeof" | "void" | "delete") => {
                self.next();
                let op = match &*w {
                    "typeof" => "typeof",
                    "void" => "void",
                    _ => "delete",
                };
                Ok(Expr::Unary(op, Box::new(self.unary()?)))
            }
            Tok::Ident(w) if &*w == "await" && self.in_func => {
                self.next();
                Ok(Expr::Await(Box::new(self.unary()?)))
            }
            _ => self.unary_postfix(),
        }
    }

    fn unary_postfix(&mut self) -> PResult<Expr> {
        let e = self.call_member()?;
        if let Tok::Punct(op @ ("++" | "--")) = self.peek().clone() {
            if !self.nl_before() {
                self.next();
                return Ok(Expr::Update { op, prefix: false, target: Box::new(e) });
            }
        }
        Ok(e)
    }

    fn args(&mut self) -> PResult<Vec<ArrItem>> {
        let mut out = Vec::new();
        while !self.eat(")") {
            if self.eat("...") {
                out.push(ArrItem::Spread(self.assign(false)?));
            } else {
                out.push(ArrItem::Item(self.assign(false)?));
            }
            if !self.is(")") {
                self.expect(",")?;
            }
        }
        Ok(out)
    }

    fn call_member(&mut self) -> PResult<Expr> {
        let mut e = if self.eat_word("new") {
            if self.eat(".") {
                self.ident()?; // new.target
                Expr::Null
            } else {
                let callee = self.member_only()?;
                let args = if self.eat("(") { self.args()? } else { Vec::new() };
                Expr::New(Box::new(callee), args)
            }
        } else {
            self.primary()?
        };
        loop {
            if self.eat(".") {
                let name = self.ident_name()?;
                e = Expr::Member(Box::new(e), name, false);
            } else if self.eat("?.") {
                if self.eat("(") {
                    e = Expr::Call(Box::new(e), self.args()?, true);
                } else if self.eat("[") {
                    let k = self.expression(false)?;
                    self.expect("]")?;
                    e = Expr::Index(Box::new(e), Box::new(k), true);
                } else {
                    let name = self.ident_name()?;
                    e = Expr::Member(Box::new(e), name, true);
                }
            } else if self.eat("[") {
                let k = self.expression(false)?;
                self.expect("]")?;
                e = Expr::Index(Box::new(e), Box::new(k), false);
            } else if self.eat("(") {
                e = Expr::Call(Box::new(e), self.args()?, false);
            } else if let Tok::Template(q, x) = self.peek().clone() {
                self.next();
                let exprs = x.iter().map(|s| parse_expr_src(s)).collect::<PResult<Vec<_>>>()?;
                e = Expr::TaggedTemplate(Box::new(e), q, exprs);
            } else {
                break;
            }
        }
        Ok(e)
    }

    /// Callee of `new`: member accesses without calls.
    fn member_only(&mut self) -> PResult<Expr> {
        let mut e = if self.eat_word("new") {
            let c = self.member_only()?;
            let args = if self.eat("(") { self.args()? } else { Vec::new() };
            Expr::New(Box::new(c), args)
        } else {
            self.primary()?
        };
        loop {
            if self.eat(".") {
                let name = self.ident_name()?;
                e = Expr::Member(Box::new(e), name, false);
            } else if self.eat("[") {
                let k = self.expression(false)?;
                self.expect("]")?;
                e = Expr::Index(Box::new(e), Box::new(k), false);
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn ident_name(&mut self) -> PResult<Name> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            t => Err(format!("SyntaxError: expected property name, got {t:?}")),
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.next();
        Ok(match t {
            Tok::Num(n) => Expr::Num(n),
            Tok::BigInt(n) => Expr::BigInt(n),
            Tok::Str(s) => Expr::Str(s),
            Tok::Template(q, x) => {
                let exprs = x.iter().map(|s| parse_expr_src(s)).collect::<PResult<Vec<_>>>()?;
                Expr::Template(q, exprs)
            }
            Tok::Regex(b, f) => Expr::Regex(b, f),
            Tok::Punct("(") => {
                let e = self.expression(false)?;
                self.expect(")")?;
                e
            }
            Tok::Punct("[") => {
                let mut items = Vec::new();
                while !self.eat("]") {
                    if self.is(",") {
                        self.next();
                        items.push(ArrItem::Hole);
                        continue;
                    }
                    if self.eat("...") {
                        items.push(ArrItem::Spread(self.assign(false)?));
                    } else {
                        items.push(ArrItem::Item(self.assign(false)?));
                    }
                    if !self.is("]") {
                        self.expect(",")?;
                    }
                }
                Expr::Array(items)
            }
            Tok::Punct("{") => self.object_literal()?,
            Tok::Ident(w) => match &*w {
                "function" => Expr::Func(self.function_rest(false, false)?),
                "async" if self.is_word("function") => {
                    self.next();
                    Expr::Func(self.function_rest(true, false)?)
                }
                "class" => Expr::Class(self.class_rest(false)?),
                "this" => Expr::This,
                "super" => Expr::Super,
                "null" => Expr::Null,
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                "import" => {
                    if self.eat(".") {
                        self.ident()?;
                        Expr::ImportMeta
                    } else {
                        self.expect("(")?;
                        let spec = self.assign(false)?;
                        // An options argument (import attributes) is ignored.
                        if self.eat(",") && !self.is(")") {
                            self.assign(false)?;
                            self.eat(",");
                        }
                        self.expect(")")?;
                        Expr::ImportCall(Box::new(spec))
                    }
                }
                _ => Expr::Ident(w),
            },
            t => return Err(format!("SyntaxError: unexpected token {t:?} @@{}", self.toks.get(self.i.saturating_sub(1)).map_or(0, |t| t.pos))),
        })
    }

    fn object_literal(&mut self) -> PResult<Expr> {
        let mut props = Vec::new();
        while !self.eat("}") {
            if self.eat("...") {
                props.push(PropDef::Spread(self.assign(false)?));
            } else {
                let mut is_async = false;
                let mut accessor = None;
                if (self.is_word("get") || self.is_word("set")) && !matches!(self.peek_at(1), Tok::Punct(",") | Tok::Punct(":") | Tok::Punct("(") | Tok::Punct("}")) {
                    accessor = Some(self.is_word("get"));
                    self.next();
                } else if self.is_word("async") && !matches!(self.peek_at(1), Tok::Punct(",") | Tok::Punct(":") | Tok::Punct("(") | Tok::Punct("}")) {
                    self.next();
                    is_async = true;
                }
                let is_gen = self.eat("*");
                let shorthand_name = if let Tok::Ident(n) = self.peek() { Some(n.clone()) } else { None };
                let key = self.prop_name()?;
                let name = if let PropKey::Name(n) = &key { Some(n.clone()) } else { None };
                if let Some(get) = accessor {
                    let f = self.method(name, false, false)?;
                    props.push(if get { PropDef::Getter(key, f) } else { PropDef::Setter(key, f) });
                } else if self.is("(") {
                    props.push(PropDef::Value(key, Expr::Func(self.method(name, is_async, is_gen)?)));
                } else if self.eat(":") {
                    props.push(PropDef::Value(key, self.assign(false)?));
                } else if let Some(n) = shorthand_name {
                    // `{ a }` or `{ a = 1 }` (the latter only valid as a pattern)
                    if self.eat("=") {
                        let d = self.assign(false)?;
                        props.push(PropDef::Value(key, Expr::Assign("=", Box::new(Pattern::Ident(n)), Box::new(d))));
                    } else {
                        props.push(PropDef::Value(key, Expr::Ident(n)));
                    }
                } else {
                    return self.err("bad object literal");
                }
            }
            if !self.is("}") {
                self.expect(",")?;
            }
        }
        Ok(Expr::Object(props))
    }
}

/// Reinterpret an expression as an assignment target.
pub fn to_pattern(e: Expr) -> PResult<Pattern> {
    Ok(match e {
        Expr::Ident(n) => Pattern::Ident(n),
        e @ (Expr::Member(..) | Expr::Index(..)) => Pattern::Expr(e),
        Expr::Assign("=", t, d) => Pattern::Default(t, *d),
        Expr::Array(items) => {
            let mut out = Vec::new();
            let mut rest = None;
            for it in items {
                match it {
                    ArrItem::Hole => out.push(None),
                    ArrItem::Item(e) => out.push(Some(to_pattern(e)?)),
                    ArrItem::Spread(e) => rest = Some(Box::new(to_pattern(e)?)),
                }
            }
            Pattern::Array(out, rest)
        }
        Expr::Object(props) => {
            let mut out = Vec::new();
            let mut rest = None;
            for p in props {
                match p {
                    PropDef::Value(k, v) => out.push((k, to_pattern(v)?)),
                    PropDef::Spread(e) => rest = Some(Box::new(to_pattern(e)?)),
                    _ => return Err("SyntaxError: invalid destructuring target".into()),
                }
            }
            Pattern::Object(out, rest)
        }
        _ => return Err("SyntaxError: invalid assignment target".into()),
    })
}
