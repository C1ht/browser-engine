//! DOM bindings: document/element/text wrappers, events, window objects.

use super::builtins::{arg, make_error_value};
use super::*;
use crate::atom::{self, k};
use crate::dom::{Data, Element, Ns, NONE};

// ------------------------------------------------------------- wrappers

pub fn node_obj(it: &mut Interp, id: NodeId) -> Value {
    if id == NONE {
        return Value::Null;
    }
    if let Some(o) = it.node_objs.get(&id) {
        return Value::Obj(o.clone());
    }
    let o = it.new_obj(it.p.node.clone(), Kind::Node(id));
    it.node_objs.insert(id, o.clone());
    Value::Obj(o)
}

pub(crate) fn node_id(v: &Value) -> Option<NodeId> {
    match v {
        Value::Obj(o) => match o.borrow().kind {
            Kind::Node(id) => Some(id),
            _ => None,
        },
        _ => None,
    }
}

fn this_node(t: &Value) -> R<NodeId> {
    node_id(t).ok_or_else(|| type_error("Illegal invocation"))
}

fn is_elem(it: &Interp, id: NodeId) -> bool {
    it.doc.elem(id).is_some()
}

fn elems(it: &mut Interp, ids: Vec<NodeId>) -> Value {
    let v = ids.into_iter().map(|i| node_obj(it, i)).collect();
    it.array(v)
}

fn descendants(it: &Interp, root: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack: Vec<NodeId> = it.doc.children(root).collect::<Vec<_>>().into_iter().rev().collect();
    while let Some(n) = stack.pop() {
        if is_elem(it, n) {
            out.push(n);
        }
        let mut kids: Vec<NodeId> = it.doc.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

fn text_of(it: &Interp, id: NodeId) -> String {
    match &it.doc.nodes[id as usize].data {
        Data::Text(t) | Data::Comment(t) => t.clone(),
        Data::Doctype(_) | Data::Document => String::new(),
        _ => {
            let mut s = String::new();
            it.doc.text_content(id, &mut s);
            s
        }
    }
}

fn set_text(it: &mut Interp, id: NodeId, s: &str) {
    if let Data::Text(t) | Data::Comment(t) = &mut it.doc.nodes[id as usize].data {
        *t = s.to_owned();
    } else {
        let id = it.doc.templates.get(&id).copied().unwrap_or(id);
        it.doc.remove_children(id);
        if !s.is_empty() {
            let t = it.doc.add(Data::Text(s.to_owned()));
            it.doc.append(id, t);
        }
    }
    it.dirty = true;
}

/// Parse an HTML fragment (as the contents of `context`) into detached
/// nodes of the live document.
fn parse_fragment(it: &mut Interp, html: &str, context: NodeId) -> Vec<NodeId> {
    let (tag, ns) = match it.doc.elem(context) {
        Some(e) => (e.tag, e.ns),
        None => (k::body, Ns::Html),
    };
    let frag = crate::html::parse_fragment(html, tag, ns);
    let mut out = Vec::new();
    for c in frag.children(frag.html).collect::<Vec<_>>() {
        out.push(it.doc.import(&frag, c));
    }
    out
}

fn clone_node(it: &mut Interp, n: NodeId, deep: bool) -> NodeId {
    it.doc.clone_node(n, deep)
}

fn is_fragment(it: &Interp, id: NodeId) -> bool {
    matches!(it.doc.nodes[id as usize].data, Data::Fragment)
}

/// Insert (moving fragments' children) and run newly connected scripts.
fn insert(it: &mut Interp, parent: NodeId, child: NodeId, before: NodeId) {
    let kids = if is_fragment(it, child) { it.doc.children(child).collect::<Vec<_>>() } else { vec![child] };
    for c in kids {
        it.doc.insert_before(parent, c, before);
        if it.doc.tag(c) == Some(k::script) && it.doc.contains(0, c) {
            run_inserted_script(it, c);
        }
    }
    it.dirty = true;
}

fn run_inserted_script(it: &mut Interp, s: NodeId) {
    if it.doc.attr(s, k::type_).is_some_and(|t| t.trim().eq_ignore_ascii_case("module")) {
        let base = it.url.clone();
        match it.doc.attr(s, k::src) {
            Some(src) => {
                let url = crate::net::resolve(&base, src);
                it.run_module(&url, None);
            }
            None => {
                let code = text_of(it, s);
                it.run_module(&base, Some(code));
            }
        }
        return;
    }
    if let Some(src) = it.doc.attr(s, k::src) {
        let url = crate::net::resolve(&it.url, src);
        it.pending_scripts.push(url);
        return;
    }
    let code = text_of(it, s);
    if !code.trim().is_empty() {
        if let Err(e) = it.run_script(&code) {
            it.log(&e);
        }
    }
}

fn kebab(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    if out.starts_with("webkit-") || out.starts_with("moz-") || out.starts_with("ms-") {
        out.insert(0, '-');
    }
    out
}

fn camel(s: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in s.chars() {
        if c == '-' {
            up = true;
        } else if up {
            out.push(c.to_ascii_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn style_decls(it: &Interp, id: NodeId) -> Vec<(String, String)> {
    it.doc
        .attr(id, k::style)
        .unwrap_or("")
        .split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(a, b)| (a.trim().to_ascii_lowercase(), b.trim().to_owned()))
        .filter(|(a, _)| !a.is_empty())
        .collect()
}

fn set_style_prop(it: &mut Interp, id: NodeId, prop: &str, value: &str) {
    let mut decls = style_decls(it, id);
    decls.retain(|(n, _)| n != prop);
    if !value.is_empty() {
        decls.push((prop.to_owned(), value.to_owned()));
    }
    let s: Vec<String> = decls.iter().map(|(a, b)| format!("{a}: {b}")).collect();
    it.doc.set_attr(id, k::style, &s.join("; "));
    it.dirty = true;
}

fn attr_atom(name: &str) -> Atom {
    atom::intern_lower(name)
}

use crate::atom::Atom;

fn get_attr(it: &Interp, id: NodeId, name: &str) -> Option<String> {
    let a = atom::lookup(&name.to_ascii_lowercase())?;
    it.doc.attr(id, a).map(str::to_owned)
}

fn classes_str(it: &Interp, id: NodeId) -> String {
    it.doc.attr(id, k::class).unwrap_or("").to_owned()
}

/// Rect of an element from the last layout: (x, y, w, h).
fn rect(it: &Interp, id: NodeId) -> [f32; 4] {
    it.rects.get(&id).copied().unwrap_or([0.0; 4])
}

// --------------------------------------------------------- get / set hooks

pub fn has(it: &mut Interp, o: &ObjRef, key: &str) -> bool {
    let Kind::Node(id) = o.borrow().kind else { return false };
    matches!(key, "textContent" | "innerHTML" | "id" | "className" | "style" | "children" | "parentNode") || (is_elem(it, id) && get_attr(it, id, key).is_some() && key.starts_with("on"))
}

pub fn get(it: &mut Interp, o: &ObjRef, key: &str) -> R<Option<Value>> {
    let kind = match o.borrow().kind {
        Kind::Node(id) => (0, id),
        Kind::Style(id) => (1, id),
        Kind::ClassList(id) => (2, id),
        Kind::Dataset(id) => (3, id),
        _ => return Ok(None),
    };
    let id = kind.1;
    match kind.0 {
        1 => {
            if key == "cssText" {
                return Ok(Some(Value::str(it.doc.attr(id, k::style).unwrap_or(""))));
            }
            if o.borrow().props.get(key).is_some() || is_proto_method(it, o, key) {
                return Ok(None);
            }
            let p = kebab(key);
            let v = style_decls(it, id).into_iter().find(|(n, _)| *n == p).map(|(_, v)| v).unwrap_or_default();
            return Ok(Some(Value::str(&v)));
        }
        2 => {
            let cls = classes_str(it, id);
            return Ok(match key {
                "length" => Some(Value::Num(cls.split_whitespace().count() as f64)),
                "value" => Some(Value::str(&cls)),
                _ => array_index(key).map(|i| cls.split_whitespace().nth(i).map(Value::str).unwrap_or_default()),
            });
        }
        3 => {
            if is_proto_method(it, o, key) {
                return Ok(None);
            }
            let name = format!("data-{}", kebab(key));
            return Ok(Some(get_attr(it, id, &name).map(|v| Value::str(&v)).unwrap_or_default()));
        }
        _ => {}
    }
    let doc_node = matches!(it.doc.nodes[id as usize].data, Data::Document);
    let v = match key {
        "nodeType" => Value::Num(match &it.doc.nodes[id as usize].data {
            Data::Element(_) => 1.0,
            Data::Text(_) => 3.0,
            Data::Document => 9.0,
            Data::Comment(_) => 8.0,
            Data::Doctype(_) => 10.0,
            Data::Fragment => 11.0,
        }),
        "nodeName" | "tagName" | "localName" => match &it.doc.nodes[id as usize].data {
            Data::Element(e) => {
                let n = atom::name(e.tag);
                Value::str(&match e.ns {
                    Ns::Svg => crate::dom::svg_tag_case(n).to_owned(),
                    _ if key == "localName" => n.to_owned(),
                    Ns::Html => n.to_ascii_uppercase(),
                    Ns::MathMl => n.to_owned(),
                })
            }
            _ if key != "nodeName" => Value::Undef,
            Data::Text(_) => Value::str("#text"),
            Data::Document => Value::str("#document"),
            Data::Comment(_) => Value::str("#comment"),
            Data::Fragment => Value::str("#document-fragment"),
            Data::Doctype(d) => Value::str(&d.name),
        },
        "name" | "publicId" | "systemId" if matches!(it.doc.nodes[id as usize].data, Data::Doctype(_)) => match &it.doc.nodes[id as usize].data {
            Data::Doctype(d) => Value::str(match key {
                "name" => &d.name,
                "publicId" => d.public.as_deref().unwrap_or(""),
                _ => d.system.as_deref().unwrap_or(""),
            }),
            _ => Value::Undef,
        },
        "parentNode" | "parentElement" => {
            let p = it.doc.parent(id);
            if key == "parentElement" && (p == NONE || p == 0 || it.doc.elem(p).is_none()) { Value::Null } else { node_obj(it, p) }
        }
        "childNodes" => {
            let kids = it.doc.children(id).collect();
            elems(it, kids)
        }
        "children" => {
            let kids = it.doc.children(id).filter(|&c| is_elem(it, c)).collect();
            elems(it, kids)
        }
        "childElementCount" => Value::Num(it.doc.children(id).filter(|&c| is_elem(it, c)).count() as f64),
        "firstChild" => node_obj(it, it.doc.nodes[id as usize].first_child),
        "lastChild" => node_obj(it, it.doc.nodes[id as usize].last_child),
        "nextSibling" => node_obj(it, it.doc.nodes[id as usize].next),
        "previousSibling" => node_obj(it, it.doc.nodes[id as usize].prev),
        "firstElementChild" => {
            let c = it.doc.children(id).find(|&c| is_elem(it, c)).unwrap_or(NONE);
            node_obj(it, c)
        }
        "lastElementChild" => {
            let c = it.doc.children(id).filter(|&c| is_elem(it, c)).last().unwrap_or(NONE);
            node_obj(it, c)
        }
        "content" if it.doc.tag(id) == Some(k::template) => {
            let c = it.doc.template_content(id);
            node_obj(it, c)
        }
        "namespaceURI" => match it.doc.elem(id).map(|e| e.ns) {
            Some(Ns::Html) => Value::str("http://www.w3.org/1999/xhtml"),
            Some(Ns::Svg) => Value::str("http://www.w3.org/2000/svg"),
            Some(Ns::MathMl) => Value::str("http://www.w3.org/1998/Math/MathML"),
            None => Value::Null,
        },
        "doctype" if doc_node => {
            let d = it.doc.children(0).find(|&c| matches!(it.doc.nodes[c as usize].data, Data::Doctype(_))).unwrap_or(NONE);
            node_obj(it, d)
        }
        "nextElementSibling" => node_obj(it, it.doc.next_element(id)),
        "previousElementSibling" => node_obj(it, it.doc.prev_element(id)),
        "textContent" | "innerText" | "nodeValue" | "data" | "outerText" => {
            if doc_node && key == "textContent" {
                Value::Null
            } else {
                Value::str(&text_of(it, id))
            }
        }
        "innerHTML" => {
            let mut s = String::new();
            it.doc.inner_html(id, &mut s);
            Value::str(&s)
        }
        "outerHTML" => {
            let mut s = String::new();
            it.doc.outer_html(id, &mut s);
            Value::str(&s)
        }
        "id" if !doc_node => Value::str(it.doc.attr(id, k::id).unwrap_or("")),
        "className" => Value::str(&classes_str(it, id)),
        "classList" => Value::Obj(it.new_obj(it.p.classlist.clone(), Kind::ClassList(id))),
        "style" => Value::Obj(it.new_obj(it.p.style.clone(), Kind::Style(id))),
        "dataset" => Value::Obj(it.new_obj(it.p.object.clone(), Kind::Dataset(id))),
        "ownerDocument" => node_obj(it, 0),
        "isConnected" => Value::Bool(it.doc.contains(0, id)),
        "value" if !doc_node => match it.doc.tag(id) {
            Some(k::textarea | k::select) => Value::str(&crate::forms::value(&it.doc, id)),
            _ => Value::str(it.doc.attr(id, k::value).unwrap_or("")),
        },
        "form" if !doc_node => match crate::forms::form_of(&it.doc, id) {
            NONE => Value::Null,
            f => node_obj(it, f),
        },
        "checked" | "disabled" | "hidden" | "required" | "readOnly" | "multiple" | "selected" | "open" => Value::Bool(get_attr(it, id, &key.to_ascii_lowercase()).is_some()),
        "href" | "src" | "action" if !doc_node => match get_attr(it, id, key) {
            Some(v) => Value::str(&crate::net::resolve(&it.url, &v)),
            None => Value::str(""),
        },
        "offsetWidth" | "clientWidth" | "scrollWidth" => Value::Num(rect(it, id)[2] as f64),
        "offsetHeight" | "clientHeight" | "scrollHeight" => Value::Num(rect(it, id)[3] as f64),
        "offsetLeft" => Value::Num(rect(it, id)[0] as f64),
        "offsetTop" => Value::Num(rect(it, id)[1] as f64),
        "scrollTop" | "scrollLeft" | "clientTop" | "clientLeft" => Value::Num(0.0),
        "offsetParent" => node_obj(it, it.doc.body),
        // document-only
        "body" | "head" | "documentElement" | "scrollingElement" if doc_node && id != 0 => {
            // A document from document.implementation.
            let html = it.doc.children(id).find(|&c| it.doc.tag(c) == Some(k::html)).unwrap_or(NONE);
            let n = match key {
                "body" | "head" if html != NONE => {
                    let want = if key == "body" { k::body } else { k::head };
                    it.doc.children(html).find(|&c| it.doc.tag(c) == Some(want)).unwrap_or(NONE)
                }
                "body" | "head" => NONE,
                _ => html,
            };
            node_obj(it, n)
        }
        "implementation" if doc_node => {
            let o = it.plain();
            it.def_method(&o, "createHTMLDocument", d_create_html_document);
            it.def_method(&o, "createDocument", d_create_html_document);
            it.def_method(&o, "hasFeature", |_, _, _| Ok(Value::Bool(true)));
            Value::Obj(o)
        }
        "body" if doc_node => node_obj(it, it.doc.body),
        "head" if doc_node => node_obj(it, it.doc.head),
        "documentElement" | "scrollingElement" if doc_node => node_obj(it, it.doc.html),
        "title" if doc_node => Value::str(&it.doc.title()),
        "readyState" if doc_node => Value::str("complete"),
        "visibilityState" if doc_node => Value::str("visible"),
        "URL" | "documentURI" if doc_node => Value::str(&it.url),
        "cookie" if doc_node => Value::str(&it.cookie),
        "referrer" if doc_node => Value::str(""),
        "domain" if doc_node => Value::str(&crate::js::web::parse_url(&it.url).map(|p| p.hostname).unwrap_or_default()),
        "characterSet" | "charset" if doc_node => Value::str("UTF-8"),
        "compatMode" if doc_node => Value::str(if it.doc.quirks { "BackCompat" } else { "CSS1Compat" }),
        "defaultView" if doc_node => Value::Obj(it.global.clone()),
        "location" if doc_node => it.get(&Value::Obj(it.global.clone()), "location")?,
        "activeElement" if doc_node => node_obj(it, if it.doc.focus != NONE { it.doc.focus } else { it.doc.body }),
        "forms" | "images" | "links" | "scripts" if doc_node => {
            let tag = match key {
                "forms" => k::form,
                "images" => k::img,
                "links" => k::a,
                _ => k::script,
            };
            let all = descendants(it, 0).into_iter().filter(|&n| it.doc.tag(n) == Some(tag)).collect();
            elems(it, all)
        }
        _ => {
            // Reflected attributes (type, name, alt, rel, placeholder, ...).
            if !doc_node && is_elem(it, id) && o.borrow().props.get(key).is_none() && !is_proto_method(it, o, key) {
                if matches!(key, "type" | "name" | "alt" | "rel" | "placeholder" | "target" | "lang" | "dir" | "title" | "role" | "width" | "height" | "content" | "method" | "colSpan" | "rowSpan" | "htmlFor" | "tabIndex") {
                    let attr = match key {
                        "htmlFor" => "for",
                        "colSpan" => "colspan",
                        "rowSpan" => "rowspan",
                        "tabIndex" => "tabindex",
                        k => k,
                    };
                    let v = get_attr(it, id, attr).unwrap_or_default();
                    return Ok(Some(if matches!(key, "width" | "height" | "colSpan" | "rowSpan" | "tabIndex") { Value::Num(v.parse().unwrap_or(0.0)) } else { Value::str(&v) }));
                }
                // Inline event handler attributes become functions on demand.
                if let Some(code) = key.strip_prefix("on").and_then(|_| get_attr(it, id, key)) {
                    if let Ok(def) = parser::parse_function(&["event"], &code) {
                        let env = it.genv.clone();
                        let f = it.make_closure(&def, &env);
                        o.borrow_mut().props.insert(Rc::from(key), Prop::data(f.clone()));
                        return Ok(Some(f));
                    }
                }
            }
            return Ok(None);
        }
    };
    Ok(Some(v))
}

fn is_proto_method(it: &Interp, o: &ObjRef, key: &str) -> bool {
    let mut cur = o.borrow().proto.clone();
    while let Some(p) = cur {
        if p.borrow().props.get(key).is_some() {
            return true;
        }
        cur = p.borrow().proto.clone();
    }
    let _ = it;
    false
}

pub fn set(it: &mut Interp, o: &ObjRef, key: &str, v: &Value) -> R<bool> {
    let kind = match o.borrow().kind {
        Kind::Node(id) => (0, id),
        Kind::Style(id) => (1, id),
        Kind::Dataset(id) => (3, id),
        _ => return Ok(false),
    };
    let id = kind.1;
    let s = || -> R<String> { Ok(String::new()) };
    let _ = s;
    match kind.0 {
        1 => {
            let val = it.to_str(v)?;
            if key == "cssText" {
                it.doc.set_attr(id, k::style, &val);
                it.dirty = true;
            } else {
                set_style_prop(it, id, &kebab(key), &val);
            }
            return Ok(true);
        }
        3 => {
            let val = it.to_str(v)?;
            let name = format!("data-{}", kebab(key));
            it.doc.set_attr(id, attr_atom(&name), &val);
            it.dirty = true;
            return Ok(true);
        }
        _ => {}
    }
    match key {
        "textContent" | "innerText" | "nodeValue" | "data" => {
            let t = if v.is_nullish() { Rc::from("") } else { it.to_str(v)? };
            set_text(it, id, &t);
        }
        "innerHTML" => {
            let html = it.to_str(v)?;
            let target = it.doc.templates.get(&id).copied().unwrap_or(id);
            it.doc.remove_children(target);
            for n in parse_fragment(it, &html, id) {
                it.doc.append(target, n);
            }
            it.dirty = true;
        }
        "outerHTML" => {
            let html = it.to_str(v)?;
            let p = it.doc.parent(id);
            if p != NONE {
                let next = it.doc.nodes[id as usize].next;
                for n in parse_fragment(it, &html, p) {
                    it.doc.insert_before(p, n, next);
                }
                it.doc.detach(id);
                it.dirty = true;
            }
        }
        "id" => {
            let val = it.to_str(v)?;
            it.doc.set_attr(id, k::id, &val);
        }
        "className" => {
            let val = it.to_str(v)?;
            it.doc.set_attr(id, k::class, &val);
            it.dirty = true;
        }
        "style" => {
            let val = it.to_str(v)?;
            it.doc.set_attr(id, k::style, &val);
            it.dirty = true;
        }
        "value" => {
            let val = it.to_str(v)?;
            crate::forms::set_value(&mut it.doc, id, &val);
            it.dirty = true;
        }
        "checked" | "disabled" | "hidden" | "required" | "readOnly" | "selected" | "open" => {
            let a = attr_atom(&key.to_ascii_lowercase());
            if v.truthy() {
                it.doc.set_attr(id, a, "");
            } else {
                it.doc.remove_attr(id, a);
            }

            it.dirty = true;
        }
        "title" if id == 0 => {
            let t = it.to_str(v)?;
            it.doc.set_title(&t);
        }
        "cookie" if id == 0 => {
            let c = it.to_str(v)?;
            let pair = c.split(';').next().unwrap_or("").trim().to_owned();
            if !pair.is_empty() {
                let name = pair.split('=').next().unwrap_or("").to_owned();
                let mut jar: Vec<String> = it.cookie.split("; ").filter(|s| !s.is_empty() && !s.starts_with(&format!("{name}="))).map(str::to_owned).collect();
                jar.push(pair);
                it.cookie = jar.join("; ");
            }
        }
        "href" | "src" | "type" | "name" | "alt" | "rel" | "placeholder" | "target" | "lang" | "dir" | "role" | "action" | "method" | "htmlFor" | "width" | "height" | "colSpan" | "tabIndex" => {
            if id == 0 || !is_elem(it, id) {
                return Ok(false);
            }
            let attr = match key {
                "htmlFor" => "for",
                "colSpan" => "colspan",
                "tabIndex" => "tabindex",
                k => k,
            };
            let val = it.to_str(v)?;
            it.doc.set_attr(id, attr_atom(attr), &val);
            if it.doc.tag(id) == Some(k::script) && key == "src" && it.doc.contains(0, id) {
                run_inserted_script(it, id);
            }
            it.dirty = true;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

// --------------------------------------------------------- node methods

fn arg_node(a: &[Value], i: usize) -> R<NodeId> {
    node_id(&arg(a, i)).ok_or_else(|| type_error("parameter is not of type 'Node'"))
}

/// Strings passed to append/prepend become text nodes.
fn to_node(it: &mut Interp, v: &Value) -> R<NodeId> {
    match node_id(v) {
        Some(n) => Ok(n),
        None => {
            let s = it.to_str(v)?;
            Ok(it.doc.add(Data::Text(s.to_string())))
        }
    }
}

fn n_append_child(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let p = this_node(&t)?;
    let c = arg_node(a, 0)?;
    insert(it, p, c, NONE);
    Ok(arg(a, 0))
}
fn n_insert_before(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let p = this_node(&t)?;
    let c = arg_node(a, 0)?;
    let before = node_id(&arg(a, 1)).unwrap_or(NONE);
    insert(it, p, c, before);
    Ok(arg(a, 0))
}
fn n_remove_child(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let c = arg_node(a, 0)?;
    it.doc.detach(c);
    it.dirty = true;
    Ok(arg(a, 0))
}
fn n_replace_child(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let p = this_node(&t)?;
    let new = arg_node(a, 0)?;
    let old = arg_node(a, 1)?;
    let next = it.doc.nodes[old as usize].next;
    it.doc.detach(old);
    insert(it, p, new, next);
    Ok(arg(a, 1))
}
fn n_remove(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    it.doc.detach(n);
    it.dirty = true;
    Ok(Value::Undef)
}
fn n_append(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let p = this_node(&t)?;
    for v in a {
        let c = to_node(it, v)?;
        insert(it, p, c, NONE);
    }
    Ok(Value::Undef)
}
fn n_prepend(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let p = this_node(&t)?;
    for v in a.iter().rev() {
        let c = to_node(it, v)?;
        let first = it.doc.nodes[p as usize].first_child;
        insert(it, p, c, first);
    }
    Ok(Value::Undef)
}
fn n_before(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let p = it.doc.parent(n);
    if p != NONE {
        for v in a {
            let c = to_node(it, v)?;
            insert(it, p, c, n);
        }
    }
    Ok(Value::Undef)
}
fn n_after(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let p = it.doc.parent(n);
    if p != NONE {
        let mut next = it.doc.nodes[n as usize].next;
        for v in a {
            let c = to_node(it, v)?;
            insert(it, p, c, next);
            next = it.doc.nodes[c as usize].next;
        }
    }
    Ok(Value::Undef)
}
fn n_replace_with(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    n_before(it, t.clone(), a)?;
    n_remove(it, t, &[])
}
fn n_clone(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let c = clone_node(it, n, arg(a, 0).truthy());
    Ok(node_obj(it, c))
}
fn n_contains(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    Ok(Value::Bool(node_id(&arg(a, 0)).is_some_and(|c| it.doc.contains(n, c))))
}
fn n_has_child_nodes(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    Ok(Value::Bool(it.doc.nodes[n as usize].first_child != NONE))
}
fn n_get_attribute(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let name = it.to_str(&arg(a, 0))?;
    Ok(get_attr(it, n, &name).map(|v| Value::str(&v)).unwrap_or(Value::Null))
}
fn n_set_attribute(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let name = it.to_str(&arg(a, 0))?;
    let val = it.to_str(&arg(a, 1))?;
    it.doc.set_attr(n, attr_atom(&name), &val);
    if name.starts_with("on") {
        if let Value::Obj(o) = &t {
            o.borrow_mut().props.remove(&name.to_ascii_lowercase());
        }
    }
    if it.doc.tag(n) == Some(k::script) && &*name == "src" && it.doc.contains(0, n) {
        run_inserted_script(it, n);
    }
    it.dirty = true;
    Ok(Value::Undef)
}
fn n_remove_attribute(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let name = it.to_str(&arg(a, 0))?;
    it.doc.remove_attr(n, attr_atom(&name));
    it.dirty = true;
    Ok(Value::Undef)
}
fn n_has_attribute(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let name = it.to_str(&arg(a, 0))?;
    Ok(Value::Bool(get_attr(it, n, &name).is_some()))
}
fn n_toggle_attribute(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let name = it.to_str(&arg(a, 0))?;
    let has = get_attr(it, n, &name).is_some();
    let want = if a.len() > 1 { a[1].truthy() } else { !has };
    if want {
        it.doc.set_attr(n, attr_atom(&name), "");
    } else {
        it.doc.remove_attr(n, attr_atom(&name));
    }
    it.dirty = true;
    Ok(Value::Bool(want))
}
fn n_attribute_names(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let names: Vec<Value> = it.doc.elem(n).map(|e| e.attrs.iter().map(|(k, _)| Value::str(atom::name(*k))).collect()).unwrap_or_default();
    Ok(it.array(names))
}

fn select(it: &mut Interp, root: NodeId, sel: &str, first: bool) -> R<Vec<NodeId>> {
    let sels = crate::css::parse_selector_list(sel);
    if sels.is_empty() {
        return Err(make_error_value("SyntaxError", &format!("'{sel}' is not a valid selector")));
    }
    let mut out = Vec::new();
    for n in descendants(it, root) {
        if sels.iter().any(|s| crate::style::selector_matches(s, &it.doc, n)) {
            out.push(n);
            if first {
                break;
            }
        }
    }
    Ok(out)
}
fn n_query_selector(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let s = it.to_str(&arg(a, 0))?;
    let r = select(it, n, &s, true)?;
    Ok(node_obj(it, r.first().copied().unwrap_or(NONE)))
}
fn n_query_selector_all(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let s = it.to_str(&arg(a, 0))?;
    let r = select(it, n, &s, false)?;
    let arr = elems(it, r);
    add_list_methods(it, &arr);
    Ok(arr)
}
fn add_list_methods(it: &mut Interp, arr: &Value) {
    if let Value::Obj(o) = arr {
        it.def_method(o, "item", |it, t, a| {
            let i = it.to_num(&arg(a, 0))?;
            it.get_v(&t, &Value::Num(i))
        });
    }
}
fn n_by_class(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let s = it.to_str(&arg(a, 0))?;
    let want: Vec<Option<Atom>> = s.split_whitespace().map(atom::lookup).collect();
    let r: Vec<NodeId> = if want.iter().any(|w| w.is_none()) {
        Vec::new()
    } else {
        descendants(it, n).into_iter().filter(|&d| it.doc.elem(d).is_some_and(|e| want.iter().all(|w| e.classes.contains(&w.unwrap())))).collect()
    };
    let arr = elems(it, r);
    add_list_methods(it, &arr);
    Ok(arr)
}
fn n_by_tag(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let s = it.to_str(&arg(a, 0))?.to_ascii_lowercase();
    let tag = atom::lookup(&s);
    let r: Vec<NodeId> = descendants(it, n).into_iter().filter(|&d| s == "*" || (tag.is_some() && it.doc.tag(d) == tag)).collect();
    let arr = elems(it, r);
    add_list_methods(it, &arr);
    Ok(arr)
}
fn n_closest(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut n = this_node(&t)?;
    let s = it.to_str(&arg(a, 0))?;
    let sels = crate::css::parse_selector_list(&s);
    while n != NONE && n != 0 {
        if is_elem(it, n) && sels.iter().any(|sel| crate::style::selector_matches(sel, &it.doc, n)) {
            return Ok(node_obj(it, n));
        }
        n = it.doc.parent(n);
    }
    Ok(Value::Null)
}
fn n_matches(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let s = it.to_str(&arg(a, 0))?;
    let sels = crate::css::parse_selector_list(&s);
    Ok(Value::Bool(sels.iter().any(|sel| crate::style::selector_matches(sel, &it.doc, n))))
}
fn n_insert_adjacent_html(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let pos = it.to_str(&arg(a, 0))?.to_ascii_lowercase();
    let html = it.to_str(&arg(a, 1))?;
    let p = it.doc.parent(n);
    let ctx = if matches!(pos.as_str(), "beforebegin" | "afterend") { p } else { n };
    let nodes = parse_fragment(it, &html, ctx);
    for c in nodes {
        match pos.as_str() {
            "beforebegin" if p != NONE => it.doc.insert_before(p, c, n),
            "afterend" if p != NONE => {
                let next = it.doc.nodes[n as usize].next;
                it.doc.insert_before(p, c, next)
            }
            "afterbegin" => {
                let first = it.doc.nodes[n as usize].first_child;
                it.doc.insert_before(n, c, first)
            }
            _ => it.doc.append(n, c),
        }
    }
    it.dirty = true;
    Ok(Value::Undef)
}
fn n_bounding_rect(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    let [x, y, w, h] = rect(it, n);
    let o = it.plain();
    for (k, v) in [("x", x), ("y", y), ("left", x), ("top", y), ("width", w), ("height", h), ("right", x + w), ("bottom", y + h)] {
        Interp::set_prop(&o, k, Value::Num(v as f64));
    }
    Ok(Value::Obj(o))
}
fn n_click(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    dispatch(it, n, "click", |_, _| {});
    Ok(Value::Undef)
}
fn n_focus(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    if it.doc.focus != n && is_elem(it, n) {
        let old = std::mem::replace(&mut it.doc.focus, n);
        if old != NONE {
            dispatch_nobubble(it, old, "blur");
        }
        dispatch_nobubble(it, n, "focus");
        dispatch(it, n, "focusin", |_, _| {});
        it.dirty = true;
    }
    Ok(Value::Undef)
}
fn n_blur(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    if it.doc.focus == n {
        it.doc.focus = NONE;
        dispatch_nobubble(it, n, "blur");
        dispatch(it, n, "focusout", |_, _| {});
        it.dirty = true;
    }
    Ok(Value::Undef)
}
/// Dispatch an event that doesn't bubble (focus, blur).
pub fn dispatch_nobubble(it: &mut Interp, target: NodeId, ty: &str) -> bool {
    dispatch(it, target, ty, |it, e| {
        let _ = it.put(e, "bubbles", Value::Bool(false));
    })
}
fn n_submit(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    if it.doc.tag(n) == Some(k::form) {
        it.submit_form = Some(n);
    }
    Ok(Value::Undef)
}
fn n_request_submit(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = this_node(&t)?;
    if it.doc.tag(n) == Some(k::form) && !dispatch(it, n, "submit", |_, _| {}) {
        it.submit_form = Some(n);
    }
    Ok(Value::Undef)
}
fn noop(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(Value::Undef)
}

// ---------------------------------------------------------------- events

fn target_of(t: &Value, it: &Interp) -> u32 {
    match t {
        Value::Obj(o) if Rc::ptr_eq(o, &it.global) => WINDOW,
        _ => node_id(t).unwrap_or(WINDOW),
    }
}

fn add_listener(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let target = target_of(&t, it);
    let ty = it.to_str(&arg(a, 0))?;
    let f = arg(a, 1);
    if !Interp::is_callable(&f) && !matches!(f, Value::Obj(_)) {
        return Ok(Value::Undef);
    }
    if !it.listeners.iter().any(|(tg, ty2, f2)| *tg == target && *ty2 == ty && strict_eq(f2, &f)) {
        it.listeners.push((target, ty, f));
    }
    Ok(Value::Undef)
}
fn remove_listener(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let target = target_of(&t, it);
    let ty = it.to_str(&arg(a, 0))?;
    let f = arg(a, 1);
    it.listeners.retain(|(tg, ty2, f2)| !(*tg == target && *ty2 == ty && strict_eq(f2, &f)));
    Ok(Value::Undef)
}
fn dispatch_event_m(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let target = target_of(&t, it);
    let ev = arg(a, 0);
    let ty = it.get(&ev, "type")?;
    let ty = it.to_str(&ty)?;
    let prevented = dispatch_obj(it, target, &ty, ev)?;
    Ok(Value::Bool(!prevented))
}

pub(crate) fn new_event(it: &mut Interp, ty: &str) -> Value {
    let e = it.new_obj(it.p.event.clone(), Kind::Plain);
    Interp::set_prop(&e, "type", Value::str(ty));
    Interp::set_prop(&e, "bubbles", Value::Bool(true));
    Interp::set_prop(&e, "cancelable", Value::Bool(true));
    Interp::set_prop(&e, "defaultPrevented", Value::Bool(false));
    Interp::set_prop(&e, "timeStamp", Value::Num(it.now));
    Interp::set_prop(&e, "isTrusted", Value::Bool(true));
    Value::Obj(e)
}

/// Dispatch a new event of type `ty` at `target`; returns true if a handler
/// called preventDefault().
pub fn dispatch(it: &mut Interp, target: u32, ty: &str, init: impl FnOnce(&mut Interp, &Value)) -> bool {
    let ev = new_event(it, ty);
    init(it, &ev);
    dispatch_obj(it, target, ty, ev).unwrap_or(false)
}

fn dispatch_obj(it: &mut Interp, target: u32, ty: &str, ev: Value) -> R<bool> {
    let tv = if target == WINDOW { Value::Obj(it.global.clone()) } else { node_obj(it, target) };
    it.put(&ev, "target", tv.clone())?;
    it.put(&ev, "srcElement", tv)?;
    // Propagation path: target, ancestors, document, window.
    let mut path = Vec::new();
    if target != WINDOW {
        let mut n = target;
        while n != NONE {
            path.push(n);
            n = it.doc.parent(n);
        }
    }
    path.push(WINDOW);
    let bubbles = it.get(&ev, "bubbles")?.truthy();
    for (i, &cur) in path.iter().enumerate() {
        if i > 0 && !bubbles {
            break;
        }
        let cv = if cur == WINDOW { Value::Obj(it.global.clone()) } else { node_obj(it, cur) };
        it.put(&ev, "currentTarget", cv.clone())?;
        let ls: Vec<Value> = it.listeners.iter().filter(|(t, y, _)| *t == cur && &**y == ty).map(|(_, _, f)| f.clone()).collect();
        // onclick-style handlers (property or attribute).
        let prop = it.get(&cv, &format!("on{ty}")).unwrap_or_default();
        for f in ls.into_iter().chain(Interp::is_callable(&prop).then_some(prop)) {
            it.steps = 0;
            let r = if Interp::is_callable(&f) {
                it.call(&f, cv.clone(), std::slice::from_ref(&ev))
            } else {
                // { handleEvent() } listener objects
                let h = it.get(&f, "handleEvent")?;
                it.call(&h, f.clone(), std::slice::from_ref(&ev))
            };
            match r {
                Ok(Value::Bool(false)) => {
                    it.put(&ev, "defaultPrevented", Value::Bool(true))?;
                }
                Ok(_) => {}
                Err(e) => {
                    let m = it.describe_error(&e);
                    it.log(&format!("Uncaught {m}"));
                }
            }
            it.run_microtasks();
        }
        if it.get(&ev, "cancelBubble")?.truthy() {
            break;
        }
    }
    Ok(it.get(&ev, "defaultPrevented")?.truthy())
}

fn ev_prevent_default(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    it.put(&t, "defaultPrevented", Value::Bool(true))?;
    Ok(Value::Undef)
}
fn ev_stop(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    it.put(&t, "cancelBubble", Value::Bool(true))?;
    Ok(Value::Undef)
}
fn event_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let ty = it.to_str(&arg(a, 0))?;
    let e = new_event(it, &ty);
    let init = arg(a, 1);
    if let Value::Obj(_) = &init {
        for k in it.own_keys(&init) {
            let v = it.get(&init, &k)?;
            it.put(&e, &k, v)?;
        }
    }
    if !matches!(init, Value::Obj(_)) || !it.has_prop(&init, "bubbles") {
        it.put(&e, "bubbles", Value::Bool(false))?;
    }
    it.put(&e, "isTrusted", Value::Bool(false))?;
    Ok(e)
}

// ------------------------------------------------------------- document

fn d_get_by_id(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let Some(want) = atom::lookup(&s) else { return Ok(Value::Null) };
    let found = descendants(it, 0).into_iter().find(|&n| it.doc.elem(n).is_some_and(|e| e.id == Some(want)));
    Ok(node_obj(it, found.unwrap_or(NONE)))
}
fn d_create_element(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let tag = it.to_str(&arg(a, 0))?;
    let id = it.doc.add(Data::Element(Element::new(atom::intern_lower(&tag), Ns::Html, Vec::new())));
    Ok(node_obj(it, id))
}
fn d_create_element_ns(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let ns = match arg(a, 0) {
        Value::Str(s) if &*s == "http://www.w3.org/2000/svg" => Ns::Svg,
        Value::Str(s) if &*s == "http://www.w3.org/1998/Math/MathML" => Ns::MathMl,
        _ => Ns::Html,
    };
    let tag = it.to_str(&arg(a, 1))?;
    let local = tag.rsplit(':').next().unwrap_or(&tag);
    let id = it.doc.add(Data::Element(Element::new(atom::intern_lower(local), ns, Vec::new())));
    Ok(node_obj(it, id))
}
/// `document.implementation.createHTMLDocument(title)`: a detached document
/// (sharing this document's node arena).
fn d_create_html_document(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let d = it.doc.add(Data::Document);
    let mk = |it: &mut Interp, tag| it.doc.add(Data::Element(Element::new(tag, Ns::Html, Vec::new())));
    let (html, head, body) = (mk(it, k::html), mk(it, k::head), mk(it, k::body));
    it.doc.append(d, html);
    it.doc.append(html, head);
    it.doc.append(html, body);
    if let Value::Str(t) = arg(a, 0) {
        let title = mk(it, k::title);
        it.doc.append(head, title);
        it.doc.append_text(title, &t);
    }
    Ok(node_obj(it, d))
}
fn d_create_text(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let id = it.doc.add(Data::Text(s.to_string()));
    Ok(node_obj(it, id))
}
fn d_create_comment(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = if a.is_empty() { Rc::from("") } else { it.to_str(&arg(a, 0))? };
    let id = it.doc.add(Data::Comment(s.to_string()));
    Ok(node_obj(it, id))
}
fn d_create_fragment(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let id = it.doc.add(Data::Fragment);
    Ok(node_obj(it, id))
}
fn d_write(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    // document.write after parsing: append to <body>.
    let mut html = String::new();
    for v in a {
        html.push_str(&it.to_str(v)?);
    }
    let body = it.doc.body;
    for n in parse_fragment(it, &html, body) {
        it.doc.append(body, n);
    }
    it.dirty = true;
    Ok(Value::Undef)
}

// --------------------------------------------------------------- styles

fn style_set_property(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let Value::Obj(o) = &t else { return Ok(Value::Undef) };
    let Kind::Style(id) = o.borrow().kind else { return Ok(Value::Undef) };
    let name = it.to_str(&arg(a, 0))?.to_ascii_lowercase();
    let val = it.to_str(&arg(a, 1))?;
    set_style_prop(it, id, &name, &val);
    Ok(Value::Undef)
}
fn style_remove_property(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let Value::Obj(o) = &t else { return Ok(Value::Undef) };
    let Kind::Style(id) = o.borrow().kind else { return Ok(Value::Undef) };
    let name = it.to_str(&arg(a, 0))?.to_ascii_lowercase();
    set_style_prop(it, id, &name, "");
    Ok(Value::Undef)
}
fn style_get_property(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let name = it.to_str(&arg(a, 0))?;
    it.get(&t, &camel(&name))
}

fn cl_id(t: &Value) -> R<NodeId> {
    match t {
        Value::Obj(o) => match o.borrow().kind {
            Kind::ClassList(id) => Ok(id),
            _ => Err(type_error("Illegal invocation")),
        },
        _ => Err(type_error("Illegal invocation")),
    }
}
fn cl_update(it: &mut Interp, id: NodeId, f: impl FnOnce(&mut Vec<String>)) {
    let mut v: Vec<String> = classes_str(it, id).split_whitespace().map(str::to_owned).collect();
    f(&mut v);
    it.doc.set_attr(id, k::class, &v.join(" "));
    it.dirty = true;
}
fn cl_add(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let id = cl_id(&t)?;
    let mut names = Vec::new();
    for v in a {
        names.push(it.to_str(v)?.to_string());
    }
    cl_update(it, id, |c| {
        for n in names {
            if !c.contains(&n) {
                c.push(n);
            }
        }
    });
    Ok(Value::Undef)
}
fn cl_remove(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let id = cl_id(&t)?;
    let mut names = Vec::new();
    for v in a {
        names.push(it.to_str(v)?.to_string());
    }
    cl_update(it, id, |c| c.retain(|x| !names.contains(x)));
    Ok(Value::Undef)
}
fn cl_toggle(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let id = cl_id(&t)?;
    let n = it.to_str(&arg(a, 0))?.to_string();
    let has = classes_str(it, id).split_whitespace().any(|c| c == n);
    let want = if a.len() > 1 { a[1].truthy() } else { !has };
    cl_update(it, id, |c| {
        c.retain(|x| *x != n);
        if want {
            c.push(n);
        }
    });
    Ok(Value::Bool(want))
}
fn cl_contains(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let id = cl_id(&t)?;
    let n = it.to_str(&arg(a, 0))?;
    Ok(Value::Bool(classes_str(it, id).split_whitespace().any(|c| c == &*n)))
}
fn cl_replace(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let id = cl_id(&t)?;
    let (old, new) = (it.to_str(&arg(a, 0))?.to_string(), it.to_str(&arg(a, 1))?.to_string());
    let mut done = false;
    cl_update(it, id, |c| {
        for x in c.iter_mut() {
            if *x == old {
                *x = new.clone();
                done = true;
            }
        }
    });
    Ok(Value::Bool(done))
}

// --------------------------------------------------------------- window

fn w_get_computed_style(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    // Approximation: the element's inline style view plus a few computed values.
    let n = node_id(&arg(a, 0)).unwrap_or(0);
    let o = it.new_obj(it.p.style.clone(), Kind::Style(n));
    if let Some(cs) = it.computed.get(&n).cloned() {
        for (k, v) in cs {
            Interp::set_prop(&o, &k, Value::str(&v));
        }
    }
    Ok(Value::Obj(o))
}
fn w_match_media(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let q = it.to_str(&arg(a, 0))?;
    let m = crate::css::eval_media(&q, &crate::css::Media { vw: it.vw, vh: it.vh });
    let o = it.plain();
    Interp::set_prop(&o, "matches", Value::Bool(m));
    Interp::set_prop(&o, "media", Value::Str(q));
    it.def_method(&o, "addListener", noop);
    it.def_method(&o, "removeListener", noop);
    it.def_method(&o, "addEventListener", noop);
    it.def_method(&o, "removeEventListener", noop);
    Ok(Value::Obj(o))
}
fn w_alert(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    it.log(&format!("alert: {s}"));
    Ok(Value::Undef)
}
fn w_confirm(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    w_alert(it, t, a)?;
    Ok(Value::Bool(false))
}
fn w_fetch(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(super::builtins::promise_rejected(it, type_error("fetch is not supported")))
}
fn w_unsupported_ctor(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Err(type_error("not supported"))
}
fn observer_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let o = it.plain();
    for m in ["observe", "unobserve", "disconnect", "takeRecords"] {
        it.def_method(&o, m, noop);
    }
    Ok(Value::Obj(o))
}
fn console_log(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut parts = Vec::new();
    for v in a {
        parts.push(match v {
            Value::Str(s) => s.to_string(),
            Value::Obj(o) if matches!(o.borrow().kind, Kind::Array(_) | Kind::Plain) => {
                let j = super::builtins::arg(&[v.clone()], 0);
                let json = it.get(&Value::Obj(it.global.clone()), "JSON")?;
                let sf = it.get(&json, "stringify")?;
                match it.call(&sf, json, &[j]) {
                    Ok(Value::Str(s)) => s.to_string(),
                    _ => it.to_str(v)?.to_string(),
                }
            }
            _ => it.to_str(v)?.to_string(),
        });
    }
    it.log(&parts.join(" "));
    Ok(Value::Undef)
}
fn perf_now(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(Value::Num(it.start.elapsed().as_secs_f64() * 1000.0))
}
fn storage_get(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let k = it.to_str(&arg(a, 0))?;
    Ok(it.storage.get(&*k).map(|v| Value::str(v)).unwrap_or(Value::Null))
}
fn storage_set(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let k = it.to_str(&arg(a, 0))?.to_string();
    let v = it.to_str(&arg(a, 1))?.to_string();
    it.storage.insert(k, v);
    Ok(Value::Undef)
}
fn storage_remove(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let k = it.to_str(&arg(a, 0))?;
    it.storage.remove(&*k);
    Ok(Value::Undef)
}
fn storage_clear(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    it.storage.clear();
    Ok(Value::Undef)
}
fn history_state_change(it: &mut Interp, a: &[Value], push: bool) -> R<Value> {
    let url = match arg(a, 2) {
        v if v.is_nullish() => it.url.clone(),
        v => {
            let u = it.to_str(&v)?;
            crate::net::resolve(&it.url, &u)
        }
    };
    let (prev_pos, prev_url) = (it.hist_pos, it.url.clone());
    let state = arg(a, 0);
    if push {
        it.hist.truncate(it.hist_pos + 1);
        it.hist.push((url.clone(), state));
        it.hist_pos = it.hist.len() - 1;
    } else if let Some(e) = it.hist.get_mut(it.hist_pos) {
        *e = (url.clone(), state);
    }
    it.url = url.clone();
    it.url_events.push(crate::js::UrlEvent { push, prev_pos, prev_url });
    Ok(Value::Undef)
}
fn history_push(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    history_state_change(it, a, true)
}
fn history_replace(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    history_state_change(it, a, false)
}
fn history_go(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let n = it.to_num(&arg(a, 0))?;
    if n == 0.0 || n.is_nan() {
        it.nav = Some(crate::forms::Nav { url: it.url.clone(), post: None });
    } else {
        it.history_go = Some(n as i32);
    }
    Ok(Value::Undef)
}
fn history_back(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    it.history_go = Some(-1);
    Ok(Value::Undef)
}
fn history_forward(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    it.history_go = Some(1);
    Ok(Value::Undef)
}
fn history_length(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(Value::Num(it.hist.len() as f64))
}
fn history_state(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(it.hist.get(it.hist_pos).map(|e| e.1.clone()).unwrap_or(Value::Null))
}

/// Move to session history entry `pos` of this document (back / forward
/// across pushState entries): update the URL and fire `popstate`.
pub fn pop_state(it: &mut Interp, pos: usize) {
    let Some((url, state)) = it.hist.get(pos).cloned() else { return };
    let old = std::mem::replace(&mut it.url, url.clone());
    it.hist_pos = pos;
    dispatch(it, WINDOW, "popstate", |it, ev| {
        let _ = it.put(ev, "state", state);
    });
    let strip = |u: &str| u.split('#').next().unwrap_or("").to_owned();
    if strip(&old) == strip(&url) && old != url {
        fire_hashchange(it, &old, &url);
    }
}

fn fire_hashchange(it: &mut Interp, old: &str, new: &str) {
    let (old, new) = (old.to_owned(), new.to_owned());
    dispatch(it, WINDOW, "hashchange", |it, ev| {
        let _ = it.put(ev, "oldURL", Value::str(&old));
        let _ = it.put(ev, "newURL", Value::str(&new));
    });
}

// ---------------------------------------------------------------- location

fn loc_parts(it: &Interp) -> crate::js::web::Parsed {
    crate::js::web::parse_url(&it.url).unwrap_or(crate::js::web::Parsed {
        protocol: "about:".into(),
        username: String::new(),
        password: String::new(),
        hostname: String::new(),
        port: String::new(),
        pathname: it.url.split_once(':').map_or("", |x| x.1).to_owned(),
        search: String::new(),
        hash: String::new(),
    })
}

fn loc_host(p: &crate::js::web::Parsed) -> String {
    if p.port.is_empty() { p.hostname.clone() } else { format!("{}:{}", p.hostname, p.port) }
}

fn loc_origin(p: &crate::js::web::Parsed) -> String {
    if p.hostname.is_empty() { "null".into() } else { format!("{}//{}", p.protocol, loc_host(p)) }
}

fn loc_href_of(p: &crate::js::web::Parsed) -> String {
    let auth = if p.hostname.is_empty() && p.protocol != "file:" { String::new() } else { format!("//{}", loc_host(p)) };
    format!("{}{}{}{}{}", p.protocol, auth, p.pathname, p.search, p.hash)
}

/// Go to `url` from script: a fragment-only change stays in the document.
fn loc_navigate(it: &mut Interp, url: String) {
    let strip = |u: &str| u.split('#').next().unwrap_or("").to_owned();
    if url.contains('#') && strip(&url) == strip(&it.url) {
        if url != it.url {
            let old = std::mem::replace(&mut it.url, url.clone());
            it.hist.truncate(it.hist_pos + 1);
            it.hist.push((url.clone(), Value::Null));
            let prev_pos = it.hist_pos;
            it.hist_pos = it.hist.len() - 1;
            it.url_events.push(crate::js::UrlEvent { push: true, prev_pos, prev_url: old.clone() });
            fire_hashchange(it, &old, &url);
        }
        return;
    }
    it.nav = Some(crate::forms::Nav { url, post: None });
}

macro_rules! loc_props {
    ($($name:literal: $get:ident = $g:expr, $set:ident = $s:expr;)*) => {
        $(
            fn $get(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
                let p = loc_parts(it);
                let f: fn(&Interp, &crate::js::web::Parsed) -> String = $g;
                Ok(Value::str(&f(it, &p)))
            }
            fn $set(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
                let v = it.to_str(&arg(a, 0))?.to_string();
                let mut p = loc_parts(it);
                let f: fn(&Interp, &mut crate::js::web::Parsed, String) -> Option<String> = $s;
                if let Some(url) = f(it, &mut p, v) {
                    loc_navigate(it, url);
                }
                Ok(Value::Undef)
            }
        )*
        const LOCATION_PROPS: &[(&str, NativeFn, NativeFn)] = &[$(($name, $get as NativeFn, $set as NativeFn)),*];
    };
}

loc_props! {
    "href": loc_get_href = |it, _| it.url.clone(), loc_set_href = |it, _, v| Some(crate::net::resolve(&it.url, &v));
    "protocol": loc_get_protocol = |_, p| p.protocol.clone(), loc_set_protocol = |_, p, v| { p.protocol = format!("{}:", v.trim_end_matches(':')); Some(loc_href_of(p)) };
    "host": loc_get_host = |_, p| loc_host(p), loc_set_host = |_, p, v| { let (h, port) = v.split_once(':').unwrap_or((&v, "")); p.hostname = h.into(); p.port = port.into(); Some(loc_href_of(p)) };
    "hostname": loc_get_hostname = |_, p| p.hostname.clone(), loc_set_hostname = |_, p, v| { p.hostname = v; Some(loc_href_of(p)) };
    "port": loc_get_port = |_, p| p.port.clone(), loc_set_port = |_, p, v| { p.port = v; Some(loc_href_of(p)) };
    "pathname": loc_get_pathname = |_, p| p.pathname.clone(), loc_set_pathname = |_, p, v| { p.pathname = if v.starts_with('/') { v } else { format!("/{v}") }; p.hash.clear(); Some(loc_href_of(p)) };
    "search": loc_get_search = |_, p| if p.search == "?" { String::new() } else { p.search.clone() }, loc_set_search = |_, p, v| { let v = v.trim_start_matches('?'); p.search = if v.is_empty() { String::new() } else { format!("?{v}") }; p.hash.clear(); Some(loc_href_of(p)) };
    "hash": loc_get_hash = |_, p| if p.hash == "#" { String::new() } else { p.hash.clone() }, loc_set_hash = |_, p, v| { p.hash = format!("#{}", v.trim_start_matches('#')); Some(loc_href_of(p)) };
    "origin": loc_get_origin = |_, p| loc_origin(p), loc_set_origin = |_, _, _| None;
}

fn loc_reload(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    it.nav = Some(crate::forms::Nav { url: it.url.clone(), post: None });
    Ok(Value::Undef)
}

fn loc_assign(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let u = it.to_str(&arg(a, 0))?;
    let url = crate::net::resolve(&it.url, &u);
    loc_navigate(it, url);
    Ok(Value::Undef)
}

// --------------------------------------------------------------- install

pub fn install(it: &mut Interp) {
    let obj_proto = it.p.object.clone();
    let np = it.new_obj(obj_proto.clone(), Kind::Plain);
    it.p.node = Some(np.clone());
    let methods: &[(&'static str, NativeFn)] = &[
        ("appendChild", n_append_child),
        ("insertBefore", n_insert_before),
        ("removeChild", n_remove_child),
        ("replaceChild", n_replace_child),
        ("remove", n_remove),
        ("append", n_append),
        ("prepend", n_prepend),
        ("before", n_before),
        ("after", n_after),
        ("replaceWith", n_replace_with),
        ("cloneNode", n_clone),
        ("contains", n_contains),
        ("hasChildNodes", n_has_child_nodes),
        ("getAttribute", n_get_attribute),
        ("setAttribute", n_set_attribute),
        ("setAttributeNS", |it, t, a| n_set_attribute(it, t, &a[1.min(a.len())..])),
        ("removeAttribute", n_remove_attribute),
        ("hasAttribute", n_has_attribute),
        ("toggleAttribute", n_toggle_attribute),
        ("getAttributeNames", n_attribute_names),
        ("querySelector", n_query_selector),
        ("querySelectorAll", n_query_selector_all),
        ("getElementsByClassName", n_by_class),
        ("getElementsByTagName", n_by_tag),
        ("closest", n_closest),
        ("matches", n_matches),
        ("insertAdjacentHTML", n_insert_adjacent_html),
        ("getBoundingClientRect", n_bounding_rect),
        ("addEventListener", add_listener),
        ("removeEventListener", remove_listener),
        ("dispatchEvent", dispatch_event_m),
        ("click", n_click),
        ("getContext", |_, _, _| Ok(Value::Null)),
        ("toDataURL", |_, _, _| Ok(Value::str("data:,"))),
        ("focus", n_focus),
        ("blur", n_blur),
        ("submit", n_submit),
        ("requestSubmit", n_request_submit),
        ("select", noop),
        ("setSelectionRange", noop),
        ("scrollIntoView", noop),
        ("scrollTo", noop),
        ("scrollBy", noop),
        ("setPointerCapture", noop),
        ("releasePointerCapture", noop),
        ("attachShadow", |_, t, _| Ok(t)),
        ("getElementById", d_get_by_id),
        ("createElement", d_create_element),
        ("createElementNS", d_create_element_ns),
        ("createTextNode", d_create_text),
        ("createComment", d_create_comment),
        ("createDocumentFragment", d_create_fragment),
        ("write", d_write),
        ("writeln", d_write),
        ("hasFocus", |_, _, _| Ok(Value::Bool(true))),
    ];
    for (n, f) in methods {
        it.def_method(&np, n, *f);
    }

    let style = it.new_obj(obj_proto.clone(), Kind::Plain);
    it.def_method(&style, "setProperty", style_set_property);
    it.def_method(&style, "removeProperty", style_remove_property);
    it.def_method(&style, "getPropertyValue", style_get_property);
    it.p.style = Some(style);
    let cl = it.new_obj(obj_proto.clone(), Kind::Plain);
    for (n, f) in [("add", cl_add as NativeFn), ("remove", cl_remove), ("toggle", cl_toggle), ("contains", cl_contains), ("replace", cl_replace)] {
        it.def_method(&cl, n, f);
    }
    it.p.classlist = Some(cl);

    let ev = it.new_obj(obj_proto.clone(), Kind::Plain);
    it.def_method(&ev, "preventDefault", ev_prevent_default);
    it.def_method(&ev, "stopPropagation", ev_stop);
    it.def_method(&ev, "stopImmediatePropagation", ev_stop);
    it.def_method(&ev, "composedPath", |it, _, _| Ok(it.array(Vec::new())));
    it.p.event = Some(ev.clone());

    let g = it.global.clone();
    let doc = node_obj(it, 0);
    Interp::set_hidden(&g, "document", doc);
    for n in ["addEventListener", "removeEventListener", "dispatchEvent"] {
        let f = it.get(&Value::Obj(np.clone()), n).unwrap_or_default();
        Interp::set_hidden(&g, n, f);
    }
    let ctor = |it: &mut Interp, name: &'static str, f: NativeFn, proto: Option<ObjRef>| {
        let c = it.native(f, name);
        if let (Value::Obj(co), Some(p)) = (&c, proto) {
            Interp::set_hidden(co, "prototype", Value::Obj(p));
        }
        Interp::set_hidden(&it.global.clone(), name, c);
    };
    ctor(it, "Event", event_ctor, Some(ev.clone()));
    ctor(it, "CustomEvent", event_ctor, Some(ev.clone()));
    ctor(it, "KeyboardEvent", event_ctor, Some(ev.clone()));
    ctor(it, "MouseEvent", event_ctor, Some(ev));
    for n in [
        "Node", "Element", "HTMLElement", "HTMLDivElement", "HTMLSpanElement", "HTMLInputElement", "HTMLAnchorElement",
        "HTMLImageElement", "HTMLDocument", "Document", "DocumentFragment", "Text", "Comment", "CharacterData",
        "EventTarget", "SVGElement", "SVGSVGElement", "HTMLTemplateElement", "HTMLScriptElement", "HTMLStyleElement",
        "HTMLLinkElement", "HTMLMetaElement", "HTMLButtonElement", "HTMLFormElement", "HTMLSelectElement",
        "HTMLOptionElement", "HTMLTextAreaElement", "HTMLLabelElement", "HTMLIFrameElement", "HTMLCanvasElement",
        "HTMLVideoElement", "HTMLAudioElement", "HTMLMediaElement", "HTMLBodyElement", "HTMLHeadElement",
        "HTMLHtmlElement", "HTMLParagraphElement", "HTMLHeadingElement", "HTMLUListElement", "HTMLLIElement",
        "HTMLTableElement", "HTMLSlotElement", "HTMLUnknownElement", "ShadowRoot",
    ] {
        ctor(it, n, w_unsupported_ctor, Some(np.clone()));
    }
    // Node type and document-position constants.
    let consts = [
        ("ELEMENT_NODE", 1.0), ("ATTRIBUTE_NODE", 2.0), ("TEXT_NODE", 3.0), ("CDATA_SECTION_NODE", 4.0),
        ("PROCESSING_INSTRUCTION_NODE", 7.0), ("COMMENT_NODE", 8.0), ("DOCUMENT_NODE", 9.0),
        ("DOCUMENT_TYPE_NODE", 10.0), ("DOCUMENT_FRAGMENT_NODE", 11.0),
        ("DOCUMENT_POSITION_DISCONNECTED", 1.0), ("DOCUMENT_POSITION_PRECEDING", 2.0), ("DOCUMENT_POSITION_FOLLOWING", 4.0),
        ("DOCUMENT_POSITION_CONTAINS", 8.0), ("DOCUMENT_POSITION_CONTAINED_BY", 16.0),
    ];
    let node_ctor = it.get(&Value::Obj(g.clone()), "Node").unwrap_or_default();
    for (name, v) in consts {
        Interp::set_hidden(&np, name, Value::Num(v));
        if let Value::Obj(c) = &node_ctor {
            Interp::set_hidden(c, name, Value::Num(v));
        }
    }
    Interp::set_hidden(&np, "constructor", node_ctor);
    for n in ["MutationObserver", "IntersectionObserver", "ResizeObserver", "PerformanceObserver"] {
        ctor(it, n, observer_ctor, None);
    }
    for n in ["XMLHttpRequest", "WebSocket", "Worker", "EventSource"] {
        ctor(it, n, w_unsupported_ctor, None);
    }

    let console = it.plain();
    for n in ["log", "info", "warn", "error", "debug", "trace", "dir", "table"] {
        it.def_method(&console, n, console_log);
    }
    for n in ["group", "groupEnd", "groupCollapsed", "time", "timeEnd", "assert", "count"] {
        it.def_method(&console, n, noop);
    }
    Interp::set_hidden(&g, "console", Value::Obj(console));

    let loc = it.plain();
    for (name, get, set) in LOCATION_PROPS {
        let (g, s) = (it.native(*get, name), it.native(*set, name));
        loc.borrow_mut().props.insert(Rc::from(*name), crate::js::Prop { v: Value::Undef, get: Some(g), set: Some(s), enumerable: true });
    }
    it.def_method(&loc, "reload", loc_reload);
    it.def_method(&loc, "assign", loc_assign);
    it.def_method(&loc, "replace", loc_assign);
    it.def_method(&loc, "toString", loc_get_href);
    Interp::set_hidden(&g, "location", Value::Obj(loc));

    let nav = it.plain();
    Interp::set_prop(&nav, "userAgent", Value::str(crate::net::UA));
    Interp::set_prop(&nav, "language", Value::str("en-US"));
    Interp::set_prop(&nav, "languages", it.array(vec![Value::str("en-US"), Value::str("en")]));
    Interp::set_prop(&nav, "platform", Value::str("Win32"));
    Interp::set_prop(&nav, "cookieEnabled", Value::Bool(true));
    Interp::set_prop(&nav, "onLine", Value::Bool(true));
    Interp::set_prop(&nav, "hardwareConcurrency", Value::Num(4.0));
    Interp::set_prop(&nav, "maxTouchPoints", Value::Num(0.0));
    it.def_method(&nav, "sendBeacon", |_, _, _| Ok(Value::Bool(true)));
    Interp::set_hidden(&g, "navigator", Value::Obj(nav));

    for name in ["localStorage", "sessionStorage"] {
        let st = it.plain();
        it.def_method(&st, "getItem", storage_get);
        it.def_method(&st, "setItem", storage_set);
        it.def_method(&st, "removeItem", storage_remove);
        it.def_method(&st, "clear", storage_clear);
        Interp::set_hidden(&g, name, Value::Obj(st));
    }
    let hist = it.plain();
    it.def_method(&hist, "pushState", history_push);
    it.def_method(&hist, "replaceState", history_replace);
    it.def_method(&hist, "back", history_back);
    it.def_method(&hist, "forward", history_forward);
    it.def_method(&hist, "go", history_go);
    it.def_getter(&hist, "length", history_length);
    it.def_getter(&hist, "state", history_state);
    Interp::set_prop(&hist, "scrollRestoration", Value::str("auto"));
    Interp::set_hidden(&g, "history", Value::Obj(hist));

    let perf = it.plain();
    it.def_method(&perf, "now", perf_now);
    for n in ["mark", "measure", "clearMarks"] {
        it.def_method(&perf, n, noop);
    }
    it.def_method(&perf, "getEntriesByType", |it, _, _| Ok(it.array(Vec::new())));
    it.def_method(&perf, "getEntriesByName", |it, _, _| Ok(it.array(Vec::new())));
    Interp::set_hidden(&g, "performance", Value::Obj(perf));

    let screen = it.plain();
    Interp::set_prop(&screen, "width", Value::Num(it.vw as f64));
    Interp::set_prop(&screen, "height", Value::Num(it.vh as f64));
    Interp::set_hidden(&g, "screen", Value::Obj(screen));
    for (k, v) in [("innerWidth", it.vw as f64), ("innerHeight", it.vh as f64), ("outerWidth", it.vw as f64), ("outerHeight", it.vh as f64), ("devicePixelRatio", 1.0), ("scrollX", 0.0), ("scrollY", 0.0), ("pageXOffset", 0.0), ("pageYOffset", 0.0)] {
        Interp::set_hidden(&g, k, Value::Num(v));
    }
    for (n, f) in [
        ("getComputedStyle", w_get_computed_style as NativeFn),
        ("matchMedia", w_match_media),
        ("alert", w_alert),
        ("confirm", w_confirm),
        ("prompt", |_, _, _| Ok(Value::Null)),
        ("fetch", w_fetch),
        ("scrollTo", noop),
        ("scrollBy", noop),
        ("scroll", noop),
        ("open", |_, _, _| Ok(Value::Null)),
        ("close", noop),
        ("print", noop),
        ("focus", noop),
        ("blur", noop),
        ("postMessage", noop),
    ] {
        it.def_method(&g, n, f);
    }
    let ce = it.plain();
    it.def_method(&ce, "define", noop);
    it.def_method(&ce, "get", |_, _, _| Ok(Value::Undef));
    it.def_method(&ce, "whenDefined", |it, _, _| Ok(super::builtins::promise_resolved(it, Value::Undef)));
    Interp::set_hidden(&g, "customElements", Value::Obj(ce));
}
