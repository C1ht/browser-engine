//! Custom elements: `customElements.define`, upgrades and lifecycle
//! callbacks (connected / disconnected / attributeChanged).
//!
//! `class X extends HTMLElement` works through the interpreter's handling of
//! native parent constructors: `super()` reaches `html_element_ctor`, which
//! either creates the element (`new X()`) or returns the element being
//! upgraded. Lifecycle callbacks are driven by the DOM's change log
//! (`Document::ce_log`), processed after each DOM-modifying call.

use super::builtins::{self, arg};
use super::*;
use crate::dom::{Element, Mutation, Ns, NONE};

pub struct CeDef {
    pub name: Rc<str>,
    pub ctor: Value,
    pub proto: ObjRef,
    pub observed: Vec<Rc<str>>,
}

fn valid_name(n: &str) -> bool {
    n.contains('-') && n.starts_with(|c: char| c.is_ascii_lowercase()) && !matches!(n, "annotation-xml" | "color-profile" | "font-face" | "font-face-src" | "font-face-uri" | "font-face-format" | "font-face-name" | "missing-glyph")
}

impl Interp {
    fn ce_def_for(&self, n: NodeId) -> Option<usize> {
        let e = self.doc.elem(n)?;
        if e.ns != Ns::Html {
            return None;
        }
        let name = crate::atom::name(e.tag);
        if !name.contains('-') {
            return None;
        }
        self.ce_defs.iter().position(|d| &*d.name == name)
    }

    /// Run a definition's constructor on an existing element.
    pub(crate) fn ce_upgrade(&mut self, n: NodeId) {
        if self.ce_upgraded.contains_key(&n) {
            return;
        }
        let Some(di) = self.ce_def_for(n) else { return };
        self.ce_upgraded.insert(n, di);
        let (ctor, proto, observed) = {
            let d = &self.ce_defs[di];
            (d.ctor.clone(), d.proto.clone(), d.observed.clone())
        };
        let w = dombind::node_obj(self, n);
        if let Value::Obj(wo) = &w {
            wo.borrow_mut().proto = Some(proto);
        }
        self.ce_upgrading.push(n);
        let r = match &ctor {
            Value::Obj(co) if matches!(co.borrow().kind, Kind::Func(Func::Class(_))) => {
                let co = co.clone();
                self.init_class(&co, &w, &[])
            }
            _ => self.call(&ctor, w.clone(), &[]),
        };
        self.ce_upgrading.retain(|&x| x != n);
        if let Err(e) = r {
            let m = self.describe_error(&e);
            self.log(&m);
            return;
        }
        for name in observed {
            let v = crate::atom::lookup(&name).and_then(|a| self.doc.attr(n, a)).map(str::to_owned);
            if let Some(v) = v {
                self.ce_callback(n, "attributeChangedCallback", &[Value::Str(name.clone()), Value::Null, Value::str(&v)]);
            }
        }
        if self.doc.connected(n) {
            self.ce_callback(n, "connectedCallback", &[]);
        }
    }

    fn ce_callback(&mut self, n: NodeId, name: &str, args: &[Value]) {
        let w = dombind::node_obj(self, n);
        let f = self.get(&w, name).unwrap_or_default();
        if Self::is_callable(&f) {
            if let Err(e) = self.call(&f, w, args) {
                let m = self.describe_error(&e);
                self.log(&m);
            }
        }
    }

    /// Elements in `root`'s subtree (including shadow trees), in tree order.
    fn ce_subtree(&self, root: NodeId, out: &mut Vec<NodeId>) {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if self.doc.elem(n).is_some() {
                out.push(n);
            }
            let mut kids: Vec<NodeId> = self.doc.children(n).collect();
            if let Some(&sr) = self.doc.shadows.get(&n) {
                kids.insert(0, sr);
            }
            stack.extend(kids.into_iter().rev());
        }
    }

    /// Process custom element reactions for DOM changes since the last call.
    pub(crate) fn ce_process(&mut self) {
        for _ in 0..64 {
            let Some(log) = self.doc.ce_log.as_mut().map(std::mem::take) else { return };
            if log.is_empty() {
                return;
            }
            let mut connected_done: Vec<NodeId> = Vec::new();
            for m in log {
                match m {
                    Mutation::ChildList { added: Some(a), .. } if self.doc.connected(a) => {
                        let mut els = Vec::new();
                        self.ce_subtree(a, &mut els);
                        for e in els {
                            if connected_done.contains(&e) {
                                continue;
                            }
                            connected_done.push(e);
                            if self.ce_upgraded.contains_key(&e) {
                                self.ce_callback(e, "connectedCallback", &[]);
                            } else if self.ce_def_for(e).is_some() {
                                self.ce_upgrade(e);
                            }
                        }
                    }
                    Mutation::ChildList { removed: Some(r), .. } if !self.doc.connected(r) => {
                        let mut els = Vec::new();
                        self.ce_subtree(r, &mut els);
                        for e in els {
                            if self.ce_upgraded.contains_key(&e) {
                                self.ce_callback(e, "disconnectedCallback", &[]);
                            }
                        }
                    }
                    Mutation::Attr { target, name, old } => {
                        let Some(&di) = self.ce_upgraded.get(&target) else { continue };
                        let aname = crate::atom::name(name);
                        if !self.ce_defs[di].observed.iter().any(|o| &**o == aname) {
                            continue;
                        }
                        let new = self.doc.attr(target, name).map_or(Value::Null, Value::str);
                        let old = old.as_deref().map_or(Value::Null, Value::str);
                        self.ce_callback(target, "attributeChangedCallback", &[Value::str(aname), old, new]);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Upgrade a just-created element if its name is defined
    /// (`document.createElement('x-foo')`).
    pub(crate) fn ce_created(&mut self, n: NodeId) {
        if !self.ce_defs.is_empty() && self.ce_def_for(n).is_some() {
            self.ce_upgrade(n);
        }
    }
}

/// `HTMLElement` (and the other element interfaces) as a constructor: only
/// valid via `super()` from a custom element class.
pub fn html_element_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    if let Some(&n) = it.ce_upgrading.last() {
        return Ok(dombind::node_obj(it, n));
    }
    let proto = it.ce_this.take().and_then(|t| t.obj().and_then(|o| o.borrow().proto.clone()));
    let def = proto.and_then(|p| it.ce_defs.iter().position(|d| Rc::ptr_eq(&d.proto, &p)));
    let Some(di) = def else { return Err(type_error("Illegal constructor")) };
    let name = it.ce_defs[di].name.clone();
    let n = it.doc.add(crate::dom::Data::Element(Element::new(crate::atom::intern(&name), Ns::Html, Vec::new())));
    it.ce_upgraded.insert(n, di);
    Ok(dombind::node_obj(it, n))
}

fn ce_define(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let name = it.to_str(&arg(a, 0))?;
    if !valid_name(&name) {
        return Err(builtins::make_error_value("SyntaxError", &format!("Failed to execute 'define': \"{name}\" is not a valid custom element name")));
    }
    if it.ce_defs.iter().any(|d| d.name == name) {
        return Err(builtins::make_error_value("NotSupportedError", &format!("the name \"{name}\" has already been used with this registry")));
    }
    let ctor = arg(a, 1);
    if !Interp::is_callable(&ctor) {
        return Err(type_error("Failed to execute 'define': The provided value is not a constructor"));
    }
    let Value::Obj(proto) = it.get(&ctor, "prototype")? else { return Err(type_error("constructor has no prototype")) };
    let oa = it.get(&ctor, "observedAttributes")?;
    let observed = if oa.is_nullish() { Vec::new() } else { it.iterate(&oa)?.iter().map(|v| it.to_str(v).map(|s| Rc::from(s.to_ascii_lowercase()))).collect::<R<Vec<_>>>()? };
    it.ce_defs.push(CeDef { name: name.clone(), ctor: ctor.clone(), proto, observed });
    if it.doc.ce_log.is_none() {
        it.doc.ce_log = Some(Vec::new());
    }
    // Upgrade elements already in the document, in tree order.
    if let Some(tag) = crate::atom::lookup(&name) {
        let mut els = Vec::new();
        it.ce_subtree(0, &mut els);
        for e in els {
            if it.doc.tag(e) == Some(tag) {
                it.ce_upgrade(e);
            }
        }
    }
    // Resolve whenDefined promises.
    let waiting: Vec<ObjRef> = it.ce_when.iter().filter(|(n, _)| **n == *name).map(|(_, p)| p.clone()).collect();
    it.ce_when.retain(|(n, _)| **n != *name);
    for p in waiting {
        builtins::settle(it, &p, true, ctor.clone());
    }
    Ok(Value::Undef)
}

fn ce_get(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let name = it.to_str(&arg(a, 0))?;
    Ok(it.ce_defs.iter().find(|d| d.name == name).map(|d| d.ctor.clone()).unwrap_or_default())
}

fn ce_get_name(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let c = arg(a, 0);
    Ok(it.ce_defs.iter().find(|d| matches!((&d.ctor, &c), (Value::Obj(x), Value::Obj(y)) if Rc::ptr_eq(x, y))).map_or(Value::Null, |d| Value::Str(d.name.clone())))
}

fn ce_when_defined(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let name = it.to_str(&arg(a, 0))?;
    if let Some(d) = it.ce_defs.iter().find(|d| d.name == name) {
        let c = d.ctor.clone();
        return Ok(builtins::promise_resolved(it, c));
    }
    let p = builtins::new_promise(it);
    it.ce_when.push((name, p.clone()));
    Ok(Value::Obj(p))
}

fn ce_upgrade_fn(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if let Some(root) = dombind::node_id(&arg(a, 0)) {
        let mut els = Vec::new();
        it.ce_subtree(root, &mut els);
        for e in els {
            it.ce_upgrade(e);
        }
    }
    Ok(Value::Undef)
}

pub fn install(it: &mut Interp) {
    let reg = it.plain();
    for (n, f) in [("define", ce_define as NativeFn), ("get", ce_get), ("getName", ce_get_name), ("whenDefined", ce_when_defined), ("upgrade", ce_upgrade_fn)] {
        it.def_method(&reg, n, f);
    }
    Interp::set_hidden(&it.global.clone(), "customElements", Value::Obj(reg));
    let _ = NONE;
}
