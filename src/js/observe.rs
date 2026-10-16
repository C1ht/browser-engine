//! MutationObserver, IntersectionObserver and ResizeObserver.
//!
//! Mutations are logged by the DOM (`Document::mutations`) while any
//! MutationObserver is registered and delivered at microtask checkpoints.
//! Intersection and resize observations are recomputed from the layout
//! geometry after each layout and scroll (`check_geometry`).

use super::builtins::arg;
use super::*;
use crate::dom::{Mutation, NONE};

pub struct MutReg {
    observer: ObjRef,
    target: NodeId,
    child_list: bool,
    attributes: bool,
    character_data: bool,
    subtree: bool,
    attr_old: bool,
    char_old: bool,
    filter: Option<Vec<String>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Geo {
    Intersection,
    Resize,
}

pub struct GeoReg {
    observer: ObjRef,
    target: NodeId,
    kind: Geo,
    /// Last reported state: (intersecting, threshold index) or (w, h).
    last: Option<(f32, f32)>,
}

fn opt(it: &mut Interp, o: &Value, k: &str) -> R<Option<bool>> {
    if !matches!(o, Value::Obj(_)) {
        return Ok(None);
    }
    Ok(match it.get(o, k)? {
        Value::Undef => None,
        v => Some(v.truthy()),
    })
}

fn callback_of(o: &ObjRef) -> Value {
    o.borrow().props.get("@@callback").map(|p| p.v.clone()).unwrap_or_default()
}

fn new_observer(it: &mut Interp, a: &[Value], proto: &str) -> R<Value> {
    let cb = arg(a, 0);
    if !Interp::is_callable(&cb) {
        return Err(type_error(format!("Failed to construct '{proto}': parameter 1 is not of type 'Function'")));
    }
    let o = it.new_obj(it.p.web.get(proto).cloned(), Kind::Plain);
    Interp::set_hidden(&o, "@@callback", cb);
    if proto == "IntersectionObserver" {
        let opts = arg(a, 1);
        let margin = if matches!(opts, Value::Obj(_)) { it.get(&opts, "rootMargin")? } else { Value::Undef };
        let margin = if let Value::Str(s) = margin { s.to_string() } else { "0px".into() };
        let th = if matches!(opts, Value::Obj(_)) { it.get(&opts, "threshold")? } else { Value::Undef };
        let ths: Vec<f64> = match &th {
            Value::Num(n) => vec![*n],
            Value::Obj(_) => it.iterate(&th)?.iter().filter_map(|v| if let Value::Num(n) = v { Some(*n) } else { None }).collect(),
            _ => vec![0.0],
        };
        Interp::set_hidden(&o, "@@margin", Value::str(&margin));
        let thv: Vec<Value> = ths.iter().map(|&n| Value::Num(n)).collect();
        let arr = it.array(thv);
        Interp::set_prop(&o, "thresholds", arr);
        Interp::set_prop(&o, "rootMargin", Value::str(&margin));
        Interp::set_prop(&o, "root", Value::Null);
    }
    Ok(Value::Obj(o))
}

fn mutation_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    new_observer(it, a, "MutationObserver")
}
fn intersection_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    new_observer(it, a, "IntersectionObserver")
}
fn resize_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    new_observer(it, a, "ResizeObserver")
}

fn mo_observe(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let Value::Obj(o) = &t else { return Ok(Value::Undef) };
    let target = dombind::node_id(&arg(a, 0)).ok_or_else(|| type_error("MutationObserver.observe: parameter 1 is not of type 'Node'"))?;
    let opts = arg(a, 1);
    let attr_old = opt(it, &opts, "attributeOldValue")?;
    let char_old = opt(it, &opts, "characterDataOldValue")?;
    let filter = if matches!(opts, Value::Obj(_)) { it.get(&opts, "attributeFilter")? } else { Value::Undef };
    let filter = if matches!(filter, Value::Obj(_)) { Some(it.iterate(&filter)?.iter().map(|v| it.to_str(v).map(|s| s.to_ascii_lowercase())).collect::<R<Vec<_>>>()?) } else { None };
    let attributes = opt(it, &opts, "attributes")?.unwrap_or(attr_old.is_some() || filter.is_some());
    let reg = MutReg {
        observer: o.clone(),
        target,
        child_list: opt(it, &opts, "childList")?.unwrap_or(false),
        attributes,
        character_data: opt(it, &opts, "characterData")?.unwrap_or(char_old.is_some()),
        subtree: opt(it, &opts, "subtree")?.unwrap_or(false),
        attr_old: attr_old.unwrap_or(false),
        char_old: char_old.unwrap_or(false),
        filter,
    };
    if !(reg.child_list || reg.attributes || reg.character_data) {
        return Err(type_error("The options object must set at least one of 'attributes', 'characterData', or 'childList' to true."));
    }
    it.mut_regs.retain(|r| !(Rc::ptr_eq(&r.observer, o) && r.target == target));
    it.mut_regs.push(reg);
    if it.doc.mutations.is_none() {
        it.doc.mutations = Some(Vec::new());
    }
    Ok(Value::Undef)
}

fn mo_disconnect(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if let Value::Obj(o) = &t {
        it.mut_regs.retain(|r| !Rc::ptr_eq(&r.observer, o));
        it.mut_queue.retain(|(ob, _)| !Rc::ptr_eq(ob, o));
    }
    Ok(Value::Undef)
}

fn mo_take_records(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    it.flush_mutation_log();
    let Value::Obj(o) = &t else { return Ok(it.array(Vec::new())) };
    let mut mine = Vec::new();
    it.mut_queue.retain(|(ob, r)| {
        if Rc::ptr_eq(ob, o) {
            mine.push(r.clone());
            false
        } else {
            true
        }
    });
    Ok(it.array(mine))
}

fn geo_observe(it: &mut Interp, t: Value, a: &[Value], kind: Geo) -> R<Value> {
    let Value::Obj(o) = &t else { return Ok(Value::Undef) };
    let Some(target) = dombind::node_id(&arg(a, 0)) else { return Err(type_error("parameter 1 is not of type 'Element'")) };
    if !it.geo_regs.iter().any(|r| Rc::ptr_eq(&r.observer, o) && r.target == target) {
        // Resize observations start from 0x0; intersection ones always report.
        let last = if kind == Geo::Resize { Some((0.0, 0.0)) } else { None };
        it.geo_regs.push(GeoReg { observer: o.clone(), target, kind, last });
        it.geo_pending = true;
    }
    Ok(Value::Undef)
}
fn io_observe(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    geo_observe(it, t, a, Geo::Intersection)
}
fn ro_observe(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    geo_observe(it, t, a, Geo::Resize)
}
fn geo_unobserve(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    if let (Value::Obj(o), Some(target)) = (&t, dombind::node_id(&arg(a, 0))) {
        it.geo_regs.retain(|r| !(Rc::ptr_eq(&r.observer, o) && r.target == target));
    }
    Ok(Value::Undef)
}
fn geo_disconnect(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if let Value::Obj(o) = &t {
        it.geo_regs.retain(|r| !Rc::ptr_eq(&r.observer, o));
    }
    Ok(Value::Undef)
}
fn geo_take_records(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(it.array(Vec::new()))
}

pub fn rect_obj(it: &Interp, x: f32, y: f32, w: f32, h: f32) -> Value {
    let o = it.plain();
    for (k, v) in [("x", x), ("y", y), ("left", x), ("top", y), ("width", w), ("height", h), ("right", x + w), ("bottom", y + h)] {
        Interp::set_prop(&o, k, Value::Num(v as f64));
    }
    Value::Obj(o)
}

/// Pixel offsets of a `rootMargin` ("10px 0px" ...): (top, right, bottom, left).
fn margins(s: &str, vw: f32, vh: f32) -> [f32; 4] {
    let vals: Vec<f32> = s
        .split_whitespace()
        .enumerate()
        .map(|(i, p)| match p.strip_suffix('%') {
            Some(n) => n.parse::<f32>().unwrap_or(0.0) / 100.0 * if i % 2 == 0 { vh } else { vw },
            None => p.trim_end_matches("px").parse().unwrap_or(0.0),
        })
        .collect();
    match vals.len() {
        1 => [vals[0]; 4],
        2 => [vals[0], vals[1], vals[0], vals[1]],
        3 => [vals[0], vals[1], vals[2], vals[1]],
        4 => [vals[0], vals[1], vals[2], vals[3]],
        _ => [0.0; 4],
    }
}

impl Interp {
    /// Turn logged DOM changes into records for the observers interested.
    pub(crate) fn flush_mutation_log(&mut self) {
        let Some(log) = self.doc.mutations.as_mut().map(std::mem::take) else { return };
        if self.mut_regs.is_empty() {
            self.doc.mutations = None;
            return;
        }
        for (m, path) in log {
            let (target, ty) = match &m {
                Mutation::ChildList { target, .. } => (*target, 0),
                Mutation::Attr { target, .. } => (*target, 1),
                Mutation::Text { target, .. } => (*target, 2),
            };
            let mut hits: Vec<(ObjRef, bool)> = Vec::new();
            for r in &self.mut_regs {
                if r.target != target && !(r.subtree && path.contains(&r.target)) {
                    continue;
                }
                let wanted = match &m {
                    Mutation::ChildList { .. } => r.child_list,
                    Mutation::Attr { name, .. } => r.attributes && r.filter.as_ref().is_none_or(|f| f.iter().any(|a| a == crate::atom::name(*name))),
                    Mutation::Text { .. } => r.character_data,
                };
                if wanted && !hits.iter().any(|(o, _)| Rc::ptr_eq(o, &r.observer)) {
                    hits.push((r.observer.clone(), if ty == 1 { r.attr_old } else { r.char_old }));
                }
            }
            for (ob, old_wanted) in hits {
                let rec = self.plain();
                let tv = dombind::node_obj(self, target);
                Interp::set_prop(&rec, "target", tv);
                let empty = self.array(Vec::new());
                let (mut added, mut removed) = (empty.clone(), self.array(Vec::new()));
                let (mut prev_v, mut next_v, mut name_v, mut old_v) = (Value::Null, Value::Null, Value::Null, Value::Null);
                let ty_name = match &m {
                    Mutation::ChildList { added: a, removed: r, prev, next, .. } => {
                        if let Some(a) = a {
                            let n = dombind::node_obj(self, *a);
                            added = self.array(vec![n]);
                        }
                        if let Some(r) = r {
                            let n = dombind::node_obj(self, *r);
                            removed = self.array(vec![n]);
                        }
                        if *prev != NONE {
                            prev_v = dombind::node_obj(self, *prev);
                        }
                        if *next != NONE {
                            next_v = dombind::node_obj(self, *next);
                        }
                        "childList"
                    }
                    Mutation::Attr { name, old, .. } => {
                        name_v = Value::str(crate::atom::name(*name));
                        if old_wanted {
                            old_v = old.as_deref().map_or(Value::Null, Value::str);
                        }
                        "attributes"
                    }
                    Mutation::Text { old, .. } => {
                        if old_wanted {
                            old_v = Value::str(old);
                        }
                        "characterData"
                    }
                };
                Interp::set_prop(&rec, "type", Value::str(ty_name));
                Interp::set_prop(&rec, "addedNodes", added);
                Interp::set_prop(&rec, "removedNodes", removed);
                Interp::set_prop(&rec, "previousSibling", prev_v);
                Interp::set_prop(&rec, "nextSibling", next_v);
                Interp::set_prop(&rec, "attributeName", name_v);
                Interp::set_prop(&rec, "attributeNamespace", Value::Null);
                Interp::set_prop(&rec, "oldValue", old_v);
                self.mut_queue.push((ob, Value::Obj(rec)));
            }
        }
    }

    /// Deliver queued mutation records. Returns true if any callback ran.
    pub(crate) fn deliver_mutations(&mut self) -> bool {
        self.flush_mutation_log();
        if self.mut_queue.is_empty() {
            return false;
        }
        let queue = std::mem::take(&mut self.mut_queue);
        let mut order: Vec<ObjRef> = Vec::new();
        for (o, _) in &queue {
            if !order.iter().any(|x| Rc::ptr_eq(x, o)) {
                order.push(o.clone());
            }
        }
        for ob in order {
            let recs: Vec<Value> = queue.iter().filter(|(o, _)| Rc::ptr_eq(o, &ob)).map(|(_, r)| r.clone()).collect();
            let list = self.array(recs);
            let cb = callback_of(&ob);
            if let Err(e) = self.call(&cb, Value::Obj(ob.clone()), &[list, Value::Obj(ob.clone())]) {
                let m = self.describe_error(&e);
                self.log(&m);
            }
        }
        true
    }

    pub fn has_geometry_observers(&self) -> bool {
        !self.geo_regs.is_empty()
    }

    /// Report intersection / size changes since the last check (after a
    /// layout or scroll). Returns true if any callback ran.
    pub fn check_geometry(&mut self) -> bool {
        self.geo_pending = false;
        if self.geo_regs.is_empty() {
            return false;
        }
        let (vw, vh, sy) = (self.vw, self.vh, self.scroll_y);
        let mut out: Vec<(ObjRef, Value)> = Vec::new();
        for i in 0..self.geo_regs.len() {
            let (target, kind, last, ob) = {
                let r = &self.geo_regs[i];
                (r.target, r.kind, r.last, r.observer.clone())
            };
            let connected = self.doc.connected(target);
            let rect = if connected { self.rects.get(&target).copied() } else { None };
            match kind {
                Geo::Intersection => {
                    let m = match ob.borrow().props.get("@@margin").map(|p| p.v.clone()) {
                        Some(Value::Str(s)) => margins(&s, vw, vh),
                        _ => [0.0; 4],
                    };
                    let root = [-m[3], sy - m[0], vw + m[1] + m[3], vh + m[0] + m[2]];
                    let (inter, ratio, irect) = match rect {
                        Some([x, y, w, h]) => {
                            let x0 = x.max(root[0]);
                            let y0 = y.max(root[1]);
                            let x1 = (x + w).min(root[0] + root[2]);
                            let y1 = (y + h).min(root[1] + root[3]);
                            let hit = x1 >= x0 && y1 >= y0 && (w > 0.0 || h > 0.0 || (x >= root[0] && y >= root[1]));
                            let area = w * h;
                            let ratio = if !hit { 0.0 } else if area > 0.0 { ((x1 - x0) * (y1 - y0) / area).clamp(0.0, 1.0) } else { 1.0 };
                            (hit, ratio, [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)])
                        }
                        None => (false, 0.0, [0.0; 4]),
                    };
                    let ths: Vec<f64> = match ob.borrow().props.get("thresholds").map(|p| p.v.clone()) {
                        Some(Value::Obj(a)) => match &a.borrow().kind {
                            Kind::Array(v) => v.iter().filter_map(|x| if let Value::Num(n) = x { Some(*n) } else { None }).collect(),
                            _ => vec![0.0],
                        },
                        _ => vec![0.0],
                    };
                    let idx = if inter { ths.iter().filter(|&&t| ratio as f64 >= t).count() as f32 } else { -1.0 };
                    let state = (if inter { 1.0 } else { 0.0 }, idx);
                    if last == Some(state) {
                        continue;
                    }
                    self.geo_regs[i].last = Some(state);
                    let e = self.plain();
                    let tv = dombind::node_obj(self, target);
                    Interp::set_prop(&e, "target", tv);
                    Interp::set_prop(&e, "isIntersecting", Value::Bool(inter));
                    Interp::set_prop(&e, "intersectionRatio", Value::Num(ratio as f64));
                    let [x, y, w, h] = rect.unwrap_or([0.0; 4]);
                    let br = rect_obj(self, x, y - sy, w, h);
                    Interp::set_prop(&e, "boundingClientRect", br);
                    let ir = rect_obj(self, irect[0], irect[1] - sy, irect[2], irect[3]);
                    Interp::set_prop(&e, "intersectionRect", ir);
                    let rb = rect_obj(self, root[0], root[1] - sy, root[2], root[3]);
                    Interp::set_prop(&e, "rootBounds", rb);
                    Interp::set_prop(&e, "time", Value::Num(self.now));
                    out.push((ob, Value::Obj(e)));
                }
                Geo::Resize => {
                    let (w, h) = rect.map_or((0.0, 0.0), |r| (r[2], r[3]));
                    if last == Some((w, h)) {
                        continue;
                    }
                    self.geo_regs[i].last = Some((w, h));
                    let e = self.plain();
                    let tv = dombind::node_obj(self, target);
                    Interp::set_prop(&e, "target", tv);
                    let cr = rect_obj(self, 0.0, 0.0, w, h);
                    Interp::set_prop(&e, "contentRect", cr);
                    for k in ["borderBoxSize", "contentBoxSize", "devicePixelContentBoxSize"] {
                        let s = self.plain();
                        Interp::set_prop(&s, "inlineSize", Value::Num(w as f64));
                        Interp::set_prop(&s, "blockSize", Value::Num(h as f64));
                        let arr = self.array(vec![Value::Obj(s)]);
                        Interp::set_prop(&e, k, arr);
                    }
                    out.push((ob, Value::Obj(e)));
                }
            }
        }
        if out.is_empty() {
            return false;
        }
        let mut order: Vec<ObjRef> = Vec::new();
        for (o, _) in &out {
            if !order.iter().any(|x| Rc::ptr_eq(x, o)) {
                order.push(o.clone());
            }
        }
        for ob in order {
            let entries: Vec<Value> = out.iter().filter(|(o, _)| Rc::ptr_eq(o, &ob)).map(|(_, e)| e.clone()).collect();
            let list = self.array(entries);
            let cb = callback_of(&ob);
            if let Err(e) = self.call(&cb, Value::Obj(ob.clone()), &[list, Value::Obj(ob.clone())]) {
                let m = self.describe_error(&e);
                self.log(&m);
            }
        }
        self.run_microtasks();
        true
    }
}

pub fn install(it: &mut Interp) {
    let g = it.global.clone();
    let defs: [(&'static str, NativeFn, &[(&'static str, NativeFn)]); 3] = [
        ("MutationObserver", mutation_ctor, &[("observe", mo_observe), ("disconnect", mo_disconnect), ("takeRecords", mo_take_records)]),
        ("IntersectionObserver", intersection_ctor, &[("observe", io_observe), ("unobserve", geo_unobserve), ("disconnect", geo_disconnect), ("takeRecords", geo_take_records)]),
        ("ResizeObserver", resize_ctor, &[("observe", ro_observe), ("unobserve", geo_unobserve), ("disconnect", geo_disconnect)]),
    ];
    for (name, ctor, methods) in defs {
        let proto = it.plain();
        for (m, f) in methods {
            it.def_method(&proto, m, *f);
        }
        let c = it.native(ctor, name);
        if let Value::Obj(co) = &c {
            Interp::set_hidden(co, "prototype", Value::Obj(proto.clone()));
            Interp::set_hidden(&proto, "constructor", c.clone());
        }
        Interp::set_hidden(&g, name, c);
        it.p.web.insert(name, proto);
    }
    let wk = it.get(&Value::Obj(g.clone()), "MutationObserver").unwrap_or_default();
    Interp::set_hidden(&g, "WebKitMutationObserver", wk);
}
