//! CSSOM: `CSSStyleSheet` objects for `<style>` / `<link>` elements
//! (`el.sheet`, `document.styleSheets`), constructable sheets and
//! `document.adoptedStyleSheets`. CSS-in-JS libraries (styled-components,
//! emotion) inject their rules through `insertRule`.
//!
//! Rules live in `Document::cssom` / `free_sheets`; the page's stylesheet
//! collector reads them back when it rebuilds the cascade.

use super::builtins::arg;
use super::*;
use crate::dom::{SheetData, SheetRef, NONE};

fn sheet_ref(t: &Value) -> R<SheetRef> {
    match t {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Sheet(r) => Ok(*r),
            _ => Err(type_error("not a CSSStyleSheet")),
        },
        _ => Err(type_error("not a CSSStyleSheet")),
    }
}

/// Run `f` on a sheet's rule list (derived from the element's text, and
/// re-derived if the text changed since).
fn with_rules<T>(it: &mut Interp, r: SheetRef, f: impl FnOnce(&mut Vec<String>) -> T) -> T {
    match r {
        SheetRef::Node(n) => {
            let text = if it.doc.tag(n) == Some(crate::atom::k::style) {
                let mut s = String::new();
                it.doc.text_content(n, &mut s);
                s
            } else {
                String::new()
            };
            let entry = it.doc.cssom.entry(n).or_insert_with(|| SheetData { base: String::new(), rules: Vec::new(), init: false });
            if !entry.init || entry.base != text {
                *entry = SheetData { rules: crate::css::split_rules(&text), base: text, init: true };
            }
            f(&mut entry.rules)
        }
        SheetRef::Free(i) => f(&mut it.doc.free_sheets[i]),
    }
}

pub fn sheet_obj(it: &Interp, r: SheetRef) -> Value {
    Value::Obj(it.new_obj(it.p.web.get("CSSStyleSheet").cloned(), Kind::Sheet(r)))
}

fn insert_rule(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let r = sheet_ref(&t)?;
    let rule = it.to_str(&arg(a, 0))?.to_string();
    let idx = match arg(a, 1) {
        Value::Undef => 0.0,
        v => it.to_num(&v)?,
    };
    let len = with_rules(it, r, |v| v.len());
    if idx < 0.0 || idx as usize > len {
        return Err(builtins::make_error_value("IndexSizeError", "The index provided is out of range"));
    }
    if crate::css::split_rules(&rule).len() != 1 {
        return Err(builtins::make_error_value("SyntaxError", "Failed to parse the rule"));
    }
    with_rules(it, r, |v| v.insert(idx as usize, rule));
    it.dirty = true;
    Ok(Value::Num(idx))
}

fn add_rule(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let sel = it.to_str(&arg(a, 0))?;
    let body = it.to_str(&arg(a, 1))?;
    let len = with_rules(it, sheet_ref(&t)?, |v| v.len());
    let idx = if matches!(arg(a, 2), Value::Undef) { Value::Num(len as f64) } else { arg(a, 2) };
    insert_rule(it, t, &[Value::str(&format!("{sel} {{ {body} }}")), idx])?;
    Ok(Value::Num(-1.0))
}

fn delete_rule(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let r = sheet_ref(&t)?;
    let idx = it.to_num(&arg(a, 0))?;
    let ok = with_rules(it, r, |v| {
        if idx >= 0.0 && (idx as usize) < v.len() {
            v.remove(idx as usize);
            true
        } else {
            false
        }
    });
    if !ok {
        return Err(builtins::make_error_value("IndexSizeError", "The index provided is out of range"));
    }
    it.dirty = true;
    Ok(Value::Undef)
}

fn replace_sync(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let r = sheet_ref(&t)?;
    let text = it.to_str(&arg(a, 0))?;
    let rules = crate::css::split_rules(&text);
    with_rules(it, r, |v| *v = rules);
    it.dirty = true;
    Ok(Value::Undef)
}

fn replace(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    replace_sync(it, t.clone(), a)?;
    Ok(builtins::promise_resolved(it, t))
}

fn rule_obj(it: &mut Interp, text: &str, sheet: &Value) -> Value {
    let r = it.plain();
    let head0 = text.split('{').next().unwrap_or("").trim();
    let iface = match head0.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("") {
        "@media" => "CSSMediaRule",
        "@keyframes" | "@-webkit-keyframes" => "CSSKeyframesRule",
        "@font-face" => "CSSFontFaceRule",
        "@import" => "CSSImportRule",
        "@supports" => "CSSSupportsRule",
        "@layer" => "CSSLayerBlockRule",
        "@container" => "CSSContainerRule",
        "@page" => "CSSPageRule",
        "@namespace" => "CSSNamespaceRule",
        _ => "CSSStyleRule",
    };
    r.borrow_mut().proto = it.p.web.get(iface).cloned().or_else(|| it.p.object.clone());
    let head = text.split('{').next().unwrap_or("").trim();
    let ty = if head.starts_with("@media") {
        4.0
    } else if head.starts_with("@keyframes") || head.starts_with("@-webkit-keyframes") {
        7.0
    } else if head.starts_with("@font-face") {
        5.0
    } else if head.starts_with("@import") {
        3.0
    } else if head.starts_with("@supports") {
        12.0
    } else {
        1.0
    };
    Interp::set_prop(&r, "cssText", Value::str(text.trim()));
    Interp::set_prop(&r, "type", Value::Num(ty));
    if ty == 1.0 {
        Interp::set_prop(&r, "selectorText", Value::str(head));
        let decls = text.split_once('{').map_or("", |x| x.1).trim_end().trim_end_matches('}');
        let style = it.plain();
        Interp::set_prop(&style, "cssText", Value::str(decls.trim()));
        Interp::set_prop(&r, "style", Value::Obj(style));
    } else if ty == 4.0 {
        Interp::set_prop(&r, "conditionText", Value::str(head.trim_start_matches("@media").trim()));
    } else if ty == 7.0 {
        Interp::set_prop(&r, "name", Value::str(head.split_whitespace().nth(1).unwrap_or("")));
    }
    Interp::set_prop(&r, "parentStyleSheet", sheet.clone());
    Value::Obj(r)
}

fn css_rules(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let r = sheet_ref(&t)?;
    let rules = with_rules(it, r, |v| v.clone());
    let objs = rules.iter().map(|s| rule_obj(it, s, &t)).collect();
    Ok(it.array(objs))
}

fn owner_node(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(match sheet_ref(&t)? {
        SheetRef::Node(n) => dombind::node_obj(it, n),
        SheetRef::Free(_) => Value::Null,
    })
}

fn href(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(match sheet_ref(&t)? {
        SheetRef::Node(n) => match it.doc.attr(n, crate::atom::k::href) {
            Some(h) => Value::str(&crate::net::resolve(&it.url, h)),
            None => Value::Null,
        },
        SheetRef::Free(_) => Value::Null,
    })
}

fn sheet_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    it.doc.free_sheets.push(Vec::new());
    Ok(sheet_obj(it, SheetRef::Free(it.doc.free_sheets.len() - 1)))
}

/// Is `n` an element with a stylesheet (`<style>`, `<link rel=stylesheet>`)?
pub fn has_sheet(it: &Interp, n: NodeId) -> bool {
    match it.doc.elem(n) {
        Some(e) if e.tag == crate::atom::k::style => true,
        Some(e) if e.tag == crate::atom::k::link => e.attr(crate::atom::k::rel).is_some_and(|r| r.split_ascii_whitespace().any(|w| w.eq_ignore_ascii_case("stylesheet"))),
        _ => false,
    }
}

/// `document.styleSheets`.
pub fn document_sheets(it: &mut Interp) -> Value {
    let mut out = Vec::new();
    let mut stack = vec![0 as NodeId];
    while let Some(n) = stack.pop() {
        if has_sheet(it, n) {
            out.push(sheet_obj(it, SheetRef::Node(n)));
        }
        let mut c = it.doc.nodes[n as usize].last_child;
        while c != NONE {
            stack.push(c);
            c = it.doc.nodes[c as usize].prev;
        }
    }
    it.array(out)
}

/// `document.adoptedStyleSheets`.
pub fn adopted(it: &mut Interp) -> Value {
    let list: Vec<Value> = it.doc.adopted.clone().into_iter().map(|i| sheet_obj(it, SheetRef::Free(i))).collect();
    it.array(list)
}

pub fn set_adopted(it: &mut Interp, v: &Value) -> R<()> {
    let mut list = Vec::new();
    for s in it.iterate(v)? {
        if let Ok(SheetRef::Free(i)) = sheet_ref(&s) {
            list.push(i);
        }
    }
    it.doc.adopted = list;
    it.dirty = true;
    Ok(())
}

pub fn install(it: &mut Interp) {
    let proto = it.plain();
    for (n, f) in [
        ("insertRule", insert_rule as NativeFn),
        ("deleteRule", delete_rule),
        ("addRule", add_rule),
        ("removeRule", delete_rule),
        ("replaceSync", replace_sync),
        ("replace", replace),
    ] {
        it.def_method(&proto, n, f);
    }
    it.def_getter(&proto, "cssRules", css_rules);
    it.def_getter(&proto, "rules", css_rules);
    it.def_getter(&proto, "ownerNode", owner_node);
    it.def_getter(&proto, "href", href);
    Interp::set_prop(&proto, "disabled", Value::Bool(false));
    Interp::set_prop(&proto, "type", Value::str("text/css"));
    let media = it.plain();
    Interp::set_prop(&media, "mediaText", Value::str(""));
    Interp::set_prop(&proto, "media", Value::Obj(media));
    let ctor = it.native(sheet_ctor, "CSSStyleSheet");
    if let Value::Obj(c) = &ctor {
        Interp::set_hidden(c, "prototype", Value::Obj(proto.clone()));
        Interp::set_hidden(&proto, "constructor", ctor.clone());
    }
    Interp::set_hidden(&it.global.clone(), "CSSStyleSheet", ctor);
    it.p.web.insert("CSSStyleSheet", proto);

    // CSSRule interfaces (constructors throw; rule objects use these protos).
    let iface = |it: &mut Interp, name: &'static str, parent: Option<&'static str>| {
        let p = it.new_obj(parent.and_then(|n| it.p.web.get(n).cloned()).or_else(|| it.p.object.clone()), Kind::Plain);
        let c = it.native(illegal_ctor, name);
        if let Value::Obj(co) = &c {
            Interp::set_hidden(co, "prototype", Value::Obj(p.clone()));
        }
        Interp::set_hidden(&p, "constructor", c.clone());
        Interp::set_hidden(&it.global.clone(), name, c);
        it.p.web.insert(name, p);
    };
    iface(it, "CSSRule", None);
    for (n, parent) in [
        ("CSSStyleRule", "CSSRule"),
        ("CSSGroupingRule", "CSSRule"),
        ("CSSConditionRule", "CSSGroupingRule"),
        ("CSSMediaRule", "CSSConditionRule"),
        ("CSSSupportsRule", "CSSConditionRule"),
        ("CSSContainerRule", "CSSConditionRule"),
        ("CSSLayerBlockRule", "CSSGroupingRule"),
        ("CSSKeyframesRule", "CSSRule"),
        ("CSSKeyframeRule", "CSSRule"),
        ("CSSFontFaceRule", "CSSRule"),
        ("CSSImportRule", "CSSRule"),
        ("CSSPageRule", "CSSGroupingRule"),
        ("CSSNamespaceRule", "CSSRule"),
    ] {
        iface(it, n, Some(parent));
    }
    if let Some(rp) = it.p.web.get("CSSRule").cloned() {
        for (k, v) in [("STYLE_RULE", 1.0), ("IMPORT_RULE", 3.0), ("MEDIA_RULE", 4.0), ("FONT_FACE_RULE", 5.0), ("PAGE_RULE", 6.0), ("KEYFRAMES_RULE", 7.0), ("KEYFRAME_RULE", 8.0), ("SUPPORTS_RULE", 12.0)] {
            Interp::set_hidden(&rp, k, Value::Num(v));
        }
    }
}

fn illegal_ctor(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Err(crate::js::type_error("Illegal constructor"))
}
