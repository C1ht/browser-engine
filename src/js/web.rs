//! Web platform APIs: fetch (Response, Headers, Request), XMLHttpRequest,
//! AbortController, URL / URLSearchParams, FormData, Blob, plus small DOM
//! additions (createEvent, Image/Option/Audio, performance, navigator).
//!
//! Requests run on background threads; results are delivered to the
//! interpreter by `Interp::poll_requests`, called from the page's event loop.

use super::binary::{bytes_of, new_buffer, uint8_array};
use super::builtins::{arg, make_error_value, new_promise, resolve_with, settle};
use super::*;
use std::sync::mpsc;
use std::time::Duration;

/// Which list-of-pairs object this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PairKind {
    Headers,
    Params,
    Form,
}

pub enum Target {
    Fetch(ObjRef, Value),
    Xhr(Value),
    Ignore,
}

pub struct Pending {
    pub rx: mpsc::Receiver<Result<crate::net::Http, String>>,
    pub target: Target,
}

type Pairs = Vec<(Rc<str>, Rc<str>)>;

fn proto(it: &Interp, name: &str) -> Option<ObjRef> {
    it.p.web.get(name).cloned()
}

fn pairs_of(v: &Value) -> Option<(PairKind, Pairs)> {
    match &v.obj()?.borrow().kind {
        Kind::Pairs(k, p) => Some((*k, p.clone())),
        _ => None,
    }
}

fn with_pairs<T>(v: &Value, f: impl FnOnce(&mut Pairs) -> T) -> R<T> {
    match v.obj() {
        Some(o) => match &mut o.borrow_mut().kind {
            Kind::Pairs(_, p) => Ok(f(p)),
            _ => Err(type_error("Illegal invocation")),
        },
        None => Err(type_error("Illegal invocation")),
    }
}

fn new_pairs(it: &Interp, kind: PairKind, p: Pairs) -> Value {
    let name = match kind {
        PairKind::Headers => "Headers",
        PairKind::Params => "URLSearchParams",
        PairKind::Form => "FormData",
    };
    Value::Obj(it.new_obj(proto(it, name), Kind::Pairs(kind, p)))
}

fn log_err(it: &mut Interp, e: &Value) {
    let m = match e {
        Value::Obj(_) => {
            let n = it.get(e, "name").ok().and_then(|v| it.to_str(&v).ok()).unwrap_or_else(|| Rc::from("Error"));
            let msg = it.get(e, "message").ok().and_then(|v| it.to_str(&v).ok()).unwrap_or_else(|| Rc::from(""));
            format!("{n}: {msg}")
        }
        v => it.to_str(v).map(|s| s.to_string()).unwrap_or_default(),
    };
    it.log(&m);
}

fn s(v: &str) -> Rc<str> {
    Rc::from(v)
}

/// Pairs from an init value: another pairs object, [[k, v], ...] or {k: v}.
fn pairs_from_init(it: &mut Interp, init: &Value, lower: bool) -> R<Pairs> {
    let norm = |k: Rc<str>| if lower { s(&k.to_ascii_lowercase()) } else { k };
    if let Some((_, p)) = pairs_of(init) {
        return Ok(p.into_iter().map(|(k, v)| (norm(k), v)).collect());
    }
    let mut out = Vec::new();
    match init {
        Value::Undef | Value::Null => {}
        Value::Str(q) => {
            let q = q.strip_prefix('?').unwrap_or(q);
            for part in q.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = part.split_once('=').unwrap_or((part, ""));
                let dec = |x: &str| crate::net::percent_decode(&x.replace('+', " "));
                out.push((s(&dec(k)), s(&dec(v))));
            }
        }
        Value::Obj(o) => {
            let is_arrayish = matches!(o.borrow().kind, Kind::Array(_)) || it.get(init, "@@iterator").map(|v| Interp::is_callable(&v)).unwrap_or(false);
            if is_arrayish {
                for e in it.iterate(init)? {
                    let k = it.get(&e, "0")?;
                    let v = it.get(&e, "1")?;
                    out.push((norm(it.to_str(&k)?), it.to_str(&v)?));
                }
            } else {
                for k in it.own_keys(init) {
                    let v = it.get(init, &k)?;
                    out.push((norm(k), it.to_str(&v)?));
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

fn serialize_params(p: &Pairs) -> String {
    p.iter().map(|(k, v)| format!("{}={}", crate::forms::urlencode(k), crate::forms::urlencode(v))).collect::<Vec<_>>().join("&")
}

// --------------------------------------------------------------- pairs API

fn pairs_ctor(it: &mut Interp, kind: PairKind, a: &[Value]) -> R<Value> {
    let init = arg(a, 0);
    let p = if kind == PairKind::Form {
        // new FormData(form)
        match super::dombind::node_id(&init) {
            Some(n) if it.doc.tag(n) == Some(crate::atom::k::form) => crate::forms::form_pairs(&it.doc, n, crate::dom::NONE).into_iter().map(|(k, v)| (s(&k), s(&v))).collect(),
            _ => Vec::new(),
        }
    } else {
        pairs_from_init(it, &init, kind == PairKind::Headers)?
    };
    Ok(new_pairs(it, kind, p))
}

fn key_arg(it: &mut Interp, t: &Value, a: &[Value]) -> R<Rc<str>> {
    let k = it.to_str(&arg(a, 0))?;
    let lower = matches!(pairs_of(t), Some((PairKind::Headers, _)));
    Ok(if lower { s(&k.to_ascii_lowercase()) } else { k })
}

fn pairs_get(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = key_arg(it, &t, a)?;
    let (kind, p) = pairs_of(&t).ok_or_else(|| type_error("Illegal invocation"))?;
    if kind == PairKind::Headers {
        let vals: Vec<&str> = p.iter().filter(|(x, _)| *x == k).map(|(_, v)| &**v).collect();
        return Ok(if vals.is_empty() { Value::Null } else { Value::str(&vals.join(", ")) });
    }
    Ok(p.iter().find(|(x, _)| *x == k).map_or(Value::Null, |(_, v)| Value::Str(v.clone())))
}

fn pairs_get_all(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = key_arg(it, &t, a)?;
    let (_, p) = pairs_of(&t).ok_or_else(|| type_error("Illegal invocation"))?;
    Ok(it.array(p.iter().filter(|(x, _)| *x == k).map(|(_, v)| Value::Str(v.clone())).collect()))
}

fn pairs_has(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = key_arg(it, &t, a)?;
    with_pairs(&t, |p| Value::Bool(p.iter().any(|(x, _)| *x == k)))
}

fn pairs_append(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = key_arg(it, &t, a)?;
    let v = value_string(it, &arg(a, 1))?;
    with_pairs(&t, |p| p.push((k, v)))?;
    Ok(Value::Undef)
}

fn value_string(it: &mut Interp, v: &Value) -> R<Rc<str>> {
    // FormData values can be Blobs; keep their text.
    if let Some(d) = blob_data(v) {
        return Ok(s(&String::from_utf8_lossy(&d)));
    }
    it.to_str(v)
}

fn pairs_set(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = key_arg(it, &t, a)?;
    let v = value_string(it, &arg(a, 1))?;
    with_pairs(&t, |p| {
        match p.iter().position(|(x, _)| *x == k) {
            Some(i) => {
                p[i].1 = v;
                let mut j = i + 1;
                while j < p.len() {
                    if p[j].0 == k {
                        p.remove(j);
                    } else {
                        j += 1;
                    }
                }
            }
            None => p.push((k, v)),
        }
    })?;
    Ok(Value::Undef)
}

fn pairs_delete(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = key_arg(it, &t, a)?;
    with_pairs(&t, |p| p.retain(|(x, _)| *x != k))?;
    Ok(Value::Undef)
}

fn pairs_for_each(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let f = arg(a, 0);
    let (_, p) = pairs_of(&t).ok_or_else(|| type_error("Illegal invocation"))?;
    for (k, v) in p {
        it.call(&f, arg(a, 1), &[Value::Str(v), Value::Str(k), t.clone()])?;
    }
    Ok(Value::Undef)
}

fn pairs_iter(it: &mut Interp, t: Value, which: u8) -> R<Value> {
    let (_, p) = pairs_of(&t).ok_or_else(|| type_error("Illegal invocation"))?;
    let items: Vec<Value> = p
        .into_iter()
        .map(|(k, v)| match which {
            0 => Value::Str(k),
            1 => Value::Str(v),
            _ => it.array(vec![Value::Str(k), Value::Str(v)]),
        })
        .collect();
    let arr = it.array(items);
    let m = it.get(&arr, "values")?;
    it.call(&m, arr, &[])
}

fn pairs_to_string(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let (_, p) = pairs_of(&t).ok_or_else(|| type_error("Illegal invocation"))?;
    Ok(Value::str(&serialize_params(&p)))
}

// ------------------------------------------------------------------- Blob

fn blob_data(v: &Value) -> Option<Vec<u8>> {
    let o = v.obj()?;
    let d = o.borrow().props.get("@@data").map(|p| p.v.clone())?;
    bytes_of(&d)
}

fn make_blob(it: &Interp, data: Vec<u8>, ty: &str) -> Value {
    let o = it.new_obj(proto(it, "Blob"), Kind::Plain);
    let n = data.len();
    Interp::set_hidden(&o, "@@data", Value::Obj(new_buffer(it, data)));
    Interp::set_prop(&o, "size", Value::Num(n as f64));
    Interp::set_prop(&o, "type", Value::str(ty));
    Value::Obj(o)
}

fn blob_parts(it: &mut Interp, parts: &Value) -> R<Vec<u8>> {
    let mut out = Vec::new();
    if parts.is_nullish() {
        return Ok(out);
    }
    for p in it.iterate(parts)? {
        if let Some(d) = blob_data(&p).or_else(|| bytes_of(&p)) {
            out.extend_from_slice(&d);
        } else {
            out.extend_from_slice(it.to_str(&p)?.as_bytes());
        }
    }
    Ok(out)
}

fn blob_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let data = blob_parts(it, &arg(a, 0))?;
    let opts = arg(a, 1);
    let ty = if opts.is_nullish() { Rc::from("") } else { let t = it.get(&opts, "type")?; if t.is_nullish() { Rc::from("") } else { it.to_str(&t)? } };
    Ok(make_blob(it, data, &ty))
}

fn file_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let data = blob_parts(it, &arg(a, 0))?;
    let name = it.to_str(&arg(a, 1))?;
    let opts = arg(a, 2);
    let ty = if opts.is_nullish() { Rc::from("") } else { let t = it.get(&opts, "type")?; if t.is_nullish() { Rc::from("") } else { it.to_str(&t)? } };
    let b = make_blob(it, data, &ty);
    if let Value::Obj(o) = &b {
        Interp::set_prop(o, "name", Value::Str(name));
        Interp::set_prop(o, "lastModified", Value::Num(it.start.elapsed().as_millis() as f64));
    }
    Ok(b)
}

// ----------------------------------------------------------------- bodies

fn resolved(it: &mut Interp, v: Value) -> Value {
    let p = new_promise(it);
    resolve_with(it, &p, v);
    Value::Obj(p)
}

fn rejected(it: &mut Interp, e: Value) -> Value {
    let p = new_promise(it);
    settle(it, &p, false, e);
    Value::Obj(p)
}

fn body_bytes_of(t: &Value) -> Vec<u8> {
    t.obj().and_then(|o| o.borrow().props.get("@@body").map(|p| p.v.clone())).and_then(|b| bytes_of(&b)).unwrap_or_default()
}

fn mark_used(it: &mut Interp, t: &Value) -> R<bool> {
    let used = it.get(t, "bodyUsed")?.truthy();
    if let Value::Obj(o) = t {
        Interp::set_prop(o, "bodyUsed", Value::Bool(true));
    }
    Ok(used)
}

fn body_text(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if mark_used(it, &t)? {
        return Ok(rejected(it, type_error("body stream already read")));
    }
    let d = body_bytes_of(&t);
    Ok(resolved(it, Value::str(&super::binary::decode_bytes("utf-8", &d))))
}

fn json_parse(it: &mut Interp, text: &str) -> R<Value> {
    let g = Value::Obj(it.global.clone());
    let json = it.get(&g, "JSON")?;
    let parse = it.get(&json, "parse")?;
    it.call(&parse, json, &[Value::str(text)])
}

fn body_json(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if mark_used(it, &t)? {
        return Ok(rejected(it, type_error("body stream already read")));
    }
    let d = body_bytes_of(&t);
    let text = super::binary::decode_bytes("utf-8", &d);
    Ok(match json_parse(it, &text) {
        Ok(v) => resolved(it, v),
        Err(e) => rejected(it, e),
    })
}

fn body_array_buffer(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    mark_used(it, &t)?;
    let d = body_bytes_of(&t);
    let b = Value::Obj(new_buffer(it, d));
    Ok(resolved(it, b))
}

fn body_bytes(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    mark_used(it, &t)?;
    let d = body_bytes_of(&t);
    let b = uint8_array(it, d);
    Ok(resolved(it, b))
}

fn body_blob(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    mark_used(it, &t)?;
    let d = body_bytes_of(&t);
    let headers = it.get(&t, "headers")?;
    let ty = pairs_of(&headers).and_then(|(_, p)| p.iter().find(|(k, _)| &**k == "content-type").map(|(_, v)| v.clone())).unwrap_or_else(|| s(""));
    let b = make_blob(it, d, &ty);
    Ok(resolved(it, b))
}

/// Request body bytes plus the content type it implies.
fn encode_body(it: &mut Interp, b: &Value) -> R<Option<(Vec<u8>, Option<String>)>> {
    if b.is_nullish() {
        return Ok(None);
    }
    if let Some((kind, p)) = pairs_of(b) {
        return Ok(Some((serialize_params(&p).into_bytes(), Some(match kind {
            PairKind::Form | PairKind::Params => "application/x-www-form-urlencoded;charset=UTF-8".into(),
            PairKind::Headers => "text/plain;charset=UTF-8".into(),
        }))));
    }
    if let Some(d) = blob_data(b) {
        let t = it.get(b, "type")?;
        let t = it.to_str(&t)?;
        return Ok(Some((d, (!t.is_empty()).then(|| t.to_string()))));
    }
    if let Some(d) = bytes_of(b) {
        return Ok(Some((d, None)));
    }
    let text = it.to_str(b)?;
    Ok(Some((text.as_bytes().to_vec(), Some("text/plain;charset=UTF-8".into()))))
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn make_response(it: &mut Interp, status: u16, url: &str, headers: Pairs, body: Vec<u8>) -> Value {
    let o = it.new_obj(proto(it, "Response"), Kind::Plain);
    Interp::set_prop(&o, "status", Value::Num(status as f64));
    Interp::set_prop(&o, "ok", Value::Bool((200..300).contains(&status)));
    Interp::set_prop(&o, "statusText", Value::str(status_text(status)));
    Interp::set_prop(&o, "url", Value::str(url));
    Interp::set_prop(&o, "redirected", Value::Bool(false));
    Interp::set_prop(&o, "type", Value::str("basic"));
    Interp::set_prop(&o, "bodyUsed", Value::Bool(false));
    let h = new_pairs(it, PairKind::Headers, headers);
    Interp::set_prop(&o, "headers", h);
    Interp::set_hidden(&o, "@@body", Value::Obj(new_buffer(it, body)));
    Value::Obj(o)
}

fn response_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let (body, ty) = encode_body(it, &arg(a, 0))?.unwrap_or_default();
    let init = arg(a, 1);
    let status = if init.is_nullish() { 200.0 } else { match it.get(&init, "status")? { Value::Undef => 200.0, v => it.to_num(&v)? } };
    let mut headers = if init.is_nullish() { Vec::new() } else { let h = it.get(&init, "headers")?; pairs_from_init(it, &h, true)? };
    if let Some(t) = ty {
        if !headers.iter().any(|(k, _)| &**k == "content-type") {
            headers.push((s("content-type"), s(&t)));
        }
    }
    Ok(make_response(it, status as u16, "", headers, body))
}

fn response_clone(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let status = it.get(&t, "status")?;
    let status = it.to_num(&status)? as u16;
    let url = it.get(&t, "url")?;
    let url = it.to_str(&url)?;
    let h = it.get(&t, "headers")?;
    let headers = pairs_of(&h).map(|(_, p)| p).unwrap_or_default();
    let body = body_bytes_of(&t);
    Ok(make_response(it, status, &url, headers, body))
}

// ------------------------------------------------------------------ fetch

fn input_url(it: &mut Interp, v: &Value) -> R<String> {
    let u = if let Value::Obj(_) = v {
        let href = it.get(v, "href")?;
        if !href.is_nullish() { it.to_str(&href)? } else { let u = it.get(v, "url")?; it.to_str(&u)? }
    } else {
        it.to_str(v)?
    };
    Ok(crate::net::resolve(&it.url, u.trim()))
}

fn abort_error(_: &mut Interp) -> Value {
    let e = make_error_value("Error", "The operation was aborted.");
    if let Value::Obj(o) = &e {
        Interp::set_prop(o, "name", Value::str("AbortError"));
    }
    e
}

fn w_fetch(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let input = arg(a, 0);
    let init = arg(a, 1);
    let is_request = input.obj().is_some_and(|o| o.borrow().props.get("@@request").is_some());
    let url = input_url(it, &input)?;
    let pick = |it: &mut Interp, key: &str| -> R<Value> {
        let v = if init.is_nullish() { Value::Undef } else { it.get(&init, key)? };
        if v.is_nullish() && is_request { it.get(&input, key) } else { Ok(v) }
    };
    let method = match pick(it, "method")? {
        Value::Undef | Value::Null => "GET".to_string(),
        m => it.to_str(&m)?.to_ascii_uppercase(),
    };
    let h = pick(it, "headers")?;
    let mut headers = pairs_from_init(it, &h, true)?;
    let body_v = if is_request && (init.is_nullish() || it.get(&init, "body")?.is_nullish()) {
        input.obj().and_then(|o| o.borrow().props.get("@@body").map(|p| p.v.clone())).unwrap_or_default()
    } else if init.is_nullish() {
        Value::Undef
    } else {
        it.get(&init, "body")?
    };
    let body = encode_body(it, &body_v)?;
    if let Some((_, Some(ct))) = &body {
        if !headers.iter().any(|(k, _)| &**k == "content-type") {
            headers.push((s("content-type"), s(ct)));
        }
    }
    let signal = pick(it, "signal")?;
    let p = new_promise(it);
    if !signal.is_nullish() && it.get(&signal, "aborted")?.truthy() {
        let e = abort_error(it);
        settle(it, &p, false, e);
        return Ok(Value::Obj(p));
    }
    let hdrs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    it.start_request(&method, &url, hdrs, body.map(|b| b.0), Target::Fetch(p.clone(), signal));
    Ok(Value::Obj(p))
}

fn request_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let input = arg(a, 0);
    let init = arg(a, 1);
    let url = input_url(it, &input)?;
    let o = it.plain();
    Interp::set_hidden(&o, "@@request", Value::Bool(true));
    Interp::set_prop(&o, "url", Value::str(&url));
    let method = if init.is_nullish() { Value::Undef } else { it.get(&init, "method")? };
    let method = if method.is_nullish() { "GET".to_string() } else { it.to_str(&method)?.to_ascii_uppercase() };
    Interp::set_prop(&o, "method", Value::str(&method));
    let h = if init.is_nullish() { Value::Undef } else { it.get(&init, "headers")? };
    let headers = pairs_from_init(it, &h, true)?;
    let hv = new_pairs(it, PairKind::Headers, headers);
    Interp::set_prop(&o, "headers", hv);
    if !init.is_nullish() {
        let b = it.get(&init, "body")?;
        Interp::set_hidden(&o, "@@body", b);
        let sig = it.get(&init, "signal")?;
        Interp::set_prop(&o, "signal", sig);
    }
    for (k, v) in [("credentials", "same-origin"), ("mode", "cors"), ("cache", "default"), ("redirect", "follow")] {
        Interp::set_prop(&o, k, Value::str(v));
    }
    Ok(Value::Obj(o))
}

// ------------------------------------------------------------------- XHR

fn xhr_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let o = it.new_obj(proto(it, "XMLHttpRequest"), Kind::Plain);
    for (k, v) in [
        ("readyState", Value::Num(0.0)),
        ("status", Value::Num(0.0)),
        ("statusText", Value::str("")),
        ("responseText", Value::str("")),
        ("response", Value::str("")),
        ("responseType", Value::str("")),
        ("responseURL", Value::str("")),
        ("responseXML", Value::Null),
        ("timeout", Value::Num(0.0)),
        ("withCredentials", Value::Bool(false)),
    ] {
        Interp::set_prop(&o, k, v);
    }
    let upload = it.plain();
    it.def_method(&upload, "addEventListener", |_, _, _| Ok(Value::Undef));
    it.def_method(&upload, "removeEventListener", |_, _, _| Ok(Value::Undef));
    Interp::set_prop(&o, "upload", Value::Obj(upload));
    let h = new_pairs(it, PairKind::Headers, Vec::new());
    Interp::set_hidden(&o, "@@headers", h);
    Interp::set_hidden(&o, "@@listeners", it.array(Vec::new()));
    Ok(Value::Obj(o))
}

fn xhr_open(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let o = t.obj().ok_or_else(|| type_error("Illegal invocation"))?.clone();
    let method = it.to_str(&arg(a, 0))?.to_ascii_uppercase();
    let url = input_url(it, &arg(a, 1))?;
    let is_async = a.len() < 3 || arg(a, 2).truthy();
    Interp::set_hidden(&o, "@@method", Value::str(&method));
    Interp::set_hidden(&o, "@@url", Value::str(&url));
    Interp::set_hidden(&o, "@@async", Value::Bool(is_async));
    Interp::set_hidden(&o, "@@aborted", Value::Bool(false));
    let h = new_pairs(it, PairKind::Headers, Vec::new());
    Interp::set_hidden(&o, "@@headers", h);
    Interp::set_prop(&o, "readyState", Value::Num(1.0));
    xhr_fire(it, &t, "readystatechange")?;
    Ok(Value::Undef)
}

fn xhr_set_header(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let h = it.get(&t, "@@headers")?;
    pairs_append(it, h, a)
}

fn xhr_send(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let method = it.get(&t, "@@method")?;
    let method = it.to_str(&method)?.to_string();
    let url = it.get(&t, "@@url")?;
    let url = it.to_str(&url)?.to_string();
    let h = it.get(&t, "@@headers")?;
    let mut headers = pairs_of(&h).map(|(_, p)| p).unwrap_or_default();
    let body = if method == "GET" || method == "HEAD" { None } else { encode_body(it, &arg(a, 0))? };
    if let Some((_, Some(ct))) = &body {
        if !headers.iter().any(|(k, _)| &**k == "content-type") {
            headers.push((s("content-type"), s(ct)));
        }
    }
    let hdrs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let body = body.map(|b| b.0);
    if it.get(&t, "@@async")?.truthy() {
        it.start_request(&method, &url, hdrs, body, Target::Xhr(t));
    } else {
        let r = crate::net::http(&method, &url, &with_referer(&it.url, hdrs), body.as_deref());
        xhr_complete(it, &t, r)?;
    }
    Ok(Value::Undef)
}

fn xhr_abort(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if let Value::Obj(o) = &t {
        Interp::set_hidden(o, "@@aborted", Value::Bool(true));
        Interp::set_prop(o, "readyState", Value::Num(0.0));
    }
    xhr_fire(it, &t, "abort")?;
    Ok(Value::Undef)
}

fn xhr_get_header(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let h = it.get(&t, "@@resp")?;
    if h.is_nullish() {
        return Ok(Value::Null);
    }
    pairs_get(it, h, a)
}

fn xhr_all_headers(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let h = it.get(&t, "@@resp")?;
    let p = pairs_of(&h).map(|(_, p)| p).unwrap_or_default();
    Ok(Value::str(&p.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect::<String>()))
}

fn listeners_add(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let l = it.get(&t, "@@listeners")?;
    let pair = it.array(vec![arg(a, 0), arg(a, 1)]);
    if let Some(o) = l.obj() {
        if let Kind::Array(v) = &mut o.borrow_mut().kind {
            v.push(pair);
        }
    }
    Ok(Value::Undef)
}

fn listeners_remove(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let l = it.get(&t, "@@listeners")?;
    let f = arg(a, 1);
    if let Some(o) = l.obj() {
        if let Kind::Array(v) = &mut o.borrow_mut().kind {
            v.retain(|e| match e.obj() {
                Some(o) => match &o.borrow().kind {
                    Kind::Array(p) => !(p.len() == 2 && super::strict_eq(&p[1], &f)),
                    _ => true,
                },
                None => true,
            });
        }
    }
    Ok(Value::Undef)
}

/// Fire an event at a non-DOM event target (XHR, AbortSignal).
fn xhr_fire(it: &mut Interp, t: &Value, ty: &str) -> R<()> {
    let ev = super::dombind::new_event(it, ty);
    it.put(&ev, "target", t.clone())?;
    it.put(&ev, "currentTarget", t.clone())?;
    let h = it.get(t, &format!("on{ty}"))?;
    if Interp::is_callable(&h) {
        if let Err(e) = it.call(&h, t.clone(), std::slice::from_ref(&ev)) {
            log_err(it, &e);
        }
    }
    let l = it.get(t, "@@listeners")?;
    let list: Vec<Value> = match l.obj().map(|o| match &o.borrow().kind {
        Kind::Array(v) => v.clone(),
        _ => Vec::new(),
    }) {
        Some(v) => v,
        None => Vec::new(),
    };
    for e in list {
        let k = it.get(&e, "0")?;
        if matches!(&k, Value::Str(x) if &**x == ty) {
            let f = it.get(&e, "1")?;
            let r = if Interp::is_callable(&f) {
                it.call(&f, t.clone(), std::slice::from_ref(&ev))
            } else {
                let he = it.get(&f, "handleEvent")?;
                it.call(&he, f.clone(), std::slice::from_ref(&ev))
            };
            if let Err(e) = r {
                log_err(it, &e);
            }
        }
    }
    Ok(())
}

fn xhr_complete(it: &mut Interp, t: &Value, r: Result<crate::net::Http, String>) -> R<()> {
    if it.get(t, "@@aborted")?.truthy() {
        return Ok(());
    }
    let Value::Obj(o) = t else { return Ok(()) };
    match r {
        Ok(h) => {
            Interp::set_prop(o, "status", Value::Num(h.status as f64));
            Interp::set_prop(o, "statusText", Value::str(status_text(h.status)));
            Interp::set_prop(o, "responseURL", Value::str(&h.url));
            let resp = new_pairs(it, PairKind::Headers, h.headers.iter().map(|(k, v)| (s(k), s(v))).collect());
            Interp::set_hidden(o, "@@resp", resp);
            let rt = it.get(t, "responseType")?;
            let rt = it.to_str(&rt)?.to_string();
            let text = super::binary::decode_bytes("utf-8", &h.body);
            let response = match rt.as_str() {
                "json" => json_parse(it, &text).unwrap_or(Value::Null),
                "arraybuffer" => Value::Obj(new_buffer(it, h.body.clone())),
                "blob" => make_blob(it, h.body.clone(), h.header("content-type").unwrap_or("")),
                "document" => Value::Null,
                _ => Value::str(&text),
            };
            if rt.is_empty() || rt == "text" {
                Interp::set_prop(o, "responseText", Value::str(&text));
            }
            Interp::set_prop(o, "response", response);
            Interp::set_prop(o, "readyState", Value::Num(4.0));
            xhr_fire(it, t, "readystatechange")?;
            xhr_fire(it, t, "load")?;
            xhr_fire(it, t, "loadend")?;
        }
        Err(_) => {
            Interp::set_prop(o, "status", Value::Num(0.0));
            Interp::set_prop(o, "readyState", Value::Num(4.0));
            xhr_fire(it, t, "readystatechange")?;
            xhr_fire(it, t, "error")?;
            xhr_fire(it, t, "loadend")?;
        }
    }
    Ok(())
}

// ------------------------------------------------------- AbortController

fn make_signal(it: &mut Interp) -> Value {
    let sig = it.new_obj(proto(it, "AbortSignal"), Kind::Plain);
    Interp::set_prop(&sig, "aborted", Value::Bool(false));
    Interp::set_prop(&sig, "reason", Value::Undef);
    Interp::set_prop(&sig, "onabort", Value::Null);
    Interp::set_hidden(&sig, "@@listeners", it.array(Vec::new()));
    Value::Obj(sig)
}

fn abort_controller_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let o = it.new_obj(proto(it, "AbortController"), Kind::Plain);
    let sig = make_signal(it);
    Interp::set_prop(&o, "signal", sig);
    Ok(Value::Obj(o))
}

fn abort_controller_abort(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let sig = it.get(&t, "signal")?;
    if it.get(&sig, "aborted")?.truthy() {
        return Ok(Value::Undef);
    }
    let reason = if arg(a, 0).is_nullish() { abort_error(it) } else { arg(a, 0) };
    it.put(&sig, "aborted", Value::Bool(true))?;
    it.put(&sig, "reason", reason)?;
    xhr_fire(it, &sig, "abort")?;
    Ok(Value::Undef)
}

// -------------------------------------------------------------------- URL

pub(crate) struct Parsed {
    pub protocol: String,
    pub username: String,
    pub password: String,
    pub hostname: String,
    pub port: String,
    pub pathname: String,
    pub search: String,
    pub hash: String,
}

pub(crate) fn parse_url(input: &str) -> Option<Parsed> {
    let input = input.trim();
    let colon = input.find(':')?;
    let scheme = &input[..colon];
    if scheme.is_empty() || !scheme.chars().next()?.is_ascii_alphabetic() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return None;
    }
    let protocol = format!("{}:", scheme.to_ascii_lowercase());
    let mut rest = &input[colon + 1..];
    let (rest_no_hash, hash) = match rest.find('#') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    rest = rest_no_hash;
    let (rest_no_q, search) = match rest.find('?') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    rest = rest_no_q;
    let special = matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "ws" | "wss" | "ftp" | "file");
    let (mut username, mut password, mut hostname, mut port) = (String::new(), String::new(), String::new(), String::new());
    let pathname;
    if let Some(after) = rest.strip_prefix("//").or_else(|| if special { rest.strip_prefix("\\\\") } else { None }) {
        let end = after.find(['/', '\\']).unwrap_or(after.len());
        let mut auth = &after[..end];
        if let Some(at) = auth.rfind('@') {
            let cred = &auth[..at];
            let (u, p) = cred.split_once(':').unwrap_or((cred, ""));
            username = u.to_string();
            password = p.to_string();
            auth = &auth[at + 1..];
        }
        let (h, p) = if auth.starts_with('[') {
            match auth.find(']') {
                Some(i) => (&auth[..=i], auth[i + 1..].strip_prefix(':').unwrap_or("")),
                None => (auth, ""),
            }
        } else {
            auth.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).unwrap_or((auth, ""))
        };
        hostname = h.to_ascii_lowercase();
        let default = match scheme.to_ascii_lowercase().as_str() {
            "http" | "ws" => "80",
            "https" | "wss" => "443",
            "ftp" => "21",
            _ => "",
        };
        port = if p == default { String::new() } else { p.to_string() };
        let path = after[end..].replace('\\', "/");
        pathname = if path.is_empty() && special { "/".to_string() } else { normalize_path(&path) };
        if special && hostname.is_empty() && scheme != "file" {
            return None;
        }
    } else {
        pathname = rest.to_string();
    }
    Some(Parsed { protocol, username, password, hostname, port, pathname, search, hash })
}

fn normalize_path(p: &str) -> String {
    if !p.starts_with('/') {
        return p.to_string();
    }
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = p[1..].split('/').collect();
    let n = segs.len();
    for (i, seg) in segs.into_iter().enumerate() {
        match seg {
            "." => {
                if i == n - 1 {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == n - 1 {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

fn url_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let input = input_str(it, &arg(a, 0))?;
    let base = arg(a, 1);
    let full = if base.is_nullish() {
        input.clone()
    } else {
        let b = input_str(it, &base)?;
        if parse_url(&b).is_none() {
            return Err(type_error(format!("Failed to construct 'URL': Invalid base URL")));
        }
        crate::net::resolve(&b, &input)
    };
    let p = parse_url(&full).ok_or_else(|| type_error(format!("Failed to construct 'URL': Invalid URL '{input}'")))?;
    Ok(make_url(it, p))
}

fn input_str(it: &mut Interp, v: &Value) -> R<String> {
    if let Value::Obj(_) = v {
        let h = it.get(v, "href")?;
        if !h.is_nullish() {
            return Ok(it.to_str(&h)?.to_string());
        }
    }
    Ok(it.to_str(v)?.to_string())
}

fn make_url(it: &mut Interp, p: Parsed) -> Value {
    let o = it.new_obj(proto(it, "URL"), Kind::Plain);
    set_url_parts(it, &o, p);
    Value::Obj(o)
}

fn set_url_parts(it: &mut Interp, o: &ObjRef, p: Parsed) {
    for (k, v) in [("protocol", &p.protocol), ("username", &p.username), ("password", &p.password), ("hostname", &p.hostname), ("port", &p.port), ("pathname", &p.pathname), ("hash", &p.hash)] {
        Interp::set_prop(o, k, Value::str(v));
    }
    let params: Pairs = pairs_from_str(&p.search);
    let sp = new_pairs(it, PairKind::Params, params);
    Interp::set_prop(o, "searchParams", sp);
    // An explicit empty query ("?") is kept so href round-trips.
    Interp::set_hidden(o, "@@qmark", Value::Bool(p.search == "?"));
}

fn pairs_from_str(q: &str) -> Pairs {
    let q = q.strip_prefix('?').unwrap_or(q);
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let dec = |x: &str| crate::net::percent_decode(&x.replace('+', " "));
            (s(&dec(k)), s(&dec(v)))
        })
        .collect()
}

fn url_field(it: &mut Interp, t: &Value, k: &str) -> R<String> {
    let v = it.get(t, k)?;
    Ok(if v.is_nullish() { String::new() } else { it.to_str(&v)?.to_string() })
}

fn url_search(it: &mut Interp, t: &Value) -> R<String> {
    let sp = it.get(t, "searchParams")?;
    let p = pairs_of(&sp).map(|(_, p)| p).unwrap_or_default();
    if p.is_empty() {
        return Ok(if it.get(t, "@@qmark")?.truthy() { "?".into() } else { String::new() });
    }
    Ok(format!("?{}", serialize_params(&p)))
}

fn url_host(it: &mut Interp, t: &Value) -> R<String> {
    let h = url_field(it, t, "hostname")?;
    let p = url_field(it, t, "port")?;
    Ok(if p.is_empty() { h } else { format!("{h}:{p}") })
}

fn url_href(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let proto_s = url_field(it, &t, "protocol")?;
    let host = url_host(it, &t)?;
    let user = url_field(it, &t, "username")?;
    let pass = url_field(it, &t, "password")?;
    let path = url_field(it, &t, "pathname")?;
    let search = url_search(it, &t)?;
    let hash = url_field(it, &t, "hash")?;
    let creds = if user.is_empty() { String::new() } else if pass.is_empty() { format!("{user}@") } else { format!("{user}:{pass}@") };
    let authority = if host.is_empty() && !matches!(proto_s.as_str(), "http:" | "https:" | "file:" | "ws:" | "wss:" | "ftp:") { String::new() } else { format!("//{creds}{host}") };
    Ok(Value::str(&format!("{proto_s}{authority}{path}{search}{hash}")))
}

fn url_set_href(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = it.to_str(&arg(a, 0))?;
    let p = parse_url(&v).ok_or_else(|| type_error("Invalid URL"))?;
    if let Value::Obj(o) = &t {
        set_url_parts(it, o, p);
    }
    Ok(Value::Undef)
}

fn url_set_search(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = it.to_str(&arg(a, 0))?;
    let sp = it.get(&t, "searchParams")?;
    with_pairs(&sp, |p| *p = pairs_from_str(&v))?;
    Ok(Value::Undef)
}

fn url_origin(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let p = url_field(it, &t, "protocol")?;
    let h = url_host(it, &t)?;
    Ok(Value::str(&if h.is_empty() { "null".to_string() } else { format!("{p}//{h}") }))
}

fn url_set_host(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = it.to_str(&arg(a, 0))?;
    let (h, p) = v.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).unwrap_or((&v, ""));
    it.put(&t, "hostname", Value::str(h))?;
    it.put(&t, "port", Value::str(p))?;
    Ok(Value::Undef)
}

// ------------------------------------------------------------------- misc

fn create_event(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(super::dombind::new_event(it, ""))
}

fn element_ctor(it: &mut Interp, tag: &str) -> R<Value> {
    let doc = Value::Obj(it.global.clone());
    let doc = it.get(&doc, "document")?;
    let ce = it.get(&doc, "createElement")?;
    it.call(&ce, doc, &[Value::str(tag)])
}

fn image_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let el = element_ctor(it, "img")?;
    if !arg(a, 0).is_nullish() {
        let w = it.to_str(&arg(a, 0))?;
        let sa = it.get(&el, "setAttribute")?;
        it.call(&sa, el.clone(), &[Value::str("width"), Value::Str(w)])?;
    }
    if !arg(a, 1).is_nullish() {
        let h = it.to_str(&arg(a, 1))?;
        let sa = it.get(&el, "setAttribute")?;
        it.call(&sa, el.clone(), &[Value::str("height"), Value::Str(h)])?;
    }
    Ok(el)
}

fn option_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let el = element_ctor(it, "option")?;
    if !arg(a, 0).is_nullish() {
        it.put(&el, "textContent", arg(a, 0))?;
    }
    if !arg(a, 1).is_nullish() {
        let v = it.to_str(&arg(a, 1))?;
        let sa = it.get(&el, "setAttribute")?;
        it.call(&sa, el.clone(), &[Value::str("value"), Value::Str(v)])?;
    }
    Ok(el)
}

fn audio_ctor(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    element_ctor(it, "audio")
}

pub fn with_referer(page: &str, mut h: Vec<(String, String)>) -> Vec<(String, String)> {
    if (page.starts_with("http://") || page.starts_with("https://")) && !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("referer")) {
        h.push(("Referer".into(), page.split('#').next().unwrap_or(page).to_string()));
    }
    h
}

impl Interp {
    /// Start a background request whose result is delivered by `poll_requests`.
    pub fn start_request(&mut self, method: &str, url: &str, headers: Vec<(String, String)>, body: Option<Vec<u8>>, target: Target) {
        let (tx, rx) = mpsc::channel();
        let (m, u) = (method.to_string(), url.to_string());
        let h = with_referer(&self.url, headers);
        std::thread::spawn(move || {
            let _ = tx.send(crate::net::http(&m, &u, &h, body.as_deref()));
        });
        self.requests.push(Pending { rx, target });
    }

    /// Requests whose results the page is waiting on (beacons excluded).
    pub fn has_pending_requests(&self) -> bool {
        self.requests.iter().any(|r| !matches!(r.target, Target::Ignore))
    }

    /// Deliver finished requests (waiting up to `wait` for the first one).
    /// Returns true if any completed.
    pub fn poll_requests(&mut self, wait: Option<Duration>) -> bool {
        if self.requests.is_empty() {
            return false;
        }
        let deadline = wait.map(|w| std::time::Instant::now() + w);
        let mut done = Vec::new();
        loop {
            let mut i = 0;
            while i < self.requests.len() {
                match self.requests[i].rx.try_recv() {
                    Ok(r) => done.push((self.requests.remove(i).target, r)),
                    Err(mpsc::TryRecvError::Disconnected) => done.push((self.requests.remove(i).target, Err("network error".into()))),
                    Err(mpsc::TryRecvError::Empty) => i += 1,
                }
            }
            if !done.is_empty() || deadline.is_none_or(|d| std::time::Instant::now() >= d) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let any = !done.is_empty();
        for (target, r) in done {
            self.steps = 0;
            match target {
                Target::Fetch(p, signal) => {
                    let aborted = !signal.is_nullish() && self.get(&signal, "aborted").map(|v| v.truthy()).unwrap_or(false);
                    if aborted {
                        let e = abort_error(self);
                        settle(self, &p, false, e);
                    } else {
                        match r {
                            Ok(h) => {
                                let headers = h.headers.iter().map(|(k, v)| (s(k), s(v))).collect();
                                let resp = make_response(self, h.status, &h.url, headers, h.body);
                                resolve_with(self, &p, resp);
                            }
                            Err(_) => settle(self, &p, false, type_error("Failed to fetch")),
                        }
                    }
                }
                Target::Xhr(x) => {
                    if let Err(e) = xhr_complete(self, &x, r) {
                        log_err(self, &e);
                    }
                }
                Target::Ignore => {}
            }
            self.run_microtasks();
        }
        any
    }
}

// ---------------------------------------------------------------- install

pub fn install(it: &mut Interp) {
    let g = it.global.clone();
    let object = it.p.object.clone();
    let mk_proto = |it: &mut Interp, name: &'static str| -> ObjRef {
        let p = it.new_obj(object.clone(), Kind::Plain);
        Interp::set_hidden(&p, "@@toStringTag", Value::str(name));
        it.p.web.insert(name, p.clone());
        p
    };
    let ctor = |it: &mut Interp, name: &'static str, f: NativeFn, proto: &ObjRef| -> ObjRef {
        let c = it.native(f, name);
        let co = c.obj().unwrap().clone();
        Interp::set_hidden(&co, "prototype", Value::Obj(proto.clone()));
        Interp::set_hidden(proto, "constructor", c.clone());
        Interp::set_hidden(&it.global.clone(), name, c);
        co
    };

    // Headers / URLSearchParams / FormData share the pairs methods.
    for (name, kind) in [("Headers", PairKind::Headers), ("URLSearchParams", PairKind::Params), ("FormData", PairKind::Form)] {
        let p = mk_proto(it, name);
        for (n, f) in [
            ("get", pairs_get as NativeFn),
            ("getAll", pairs_get_all),
            ("has", pairs_has),
            ("append", pairs_append),
            ("set", pairs_set),
            ("delete", pairs_delete),
            ("forEach", pairs_for_each),
            ("keys", |it, t, _| pairs_iter(it, t, 0)),
            ("values", |it, t, _| pairs_iter(it, t, 1)),
            ("entries", |it, t, _| pairs_iter(it, t, 2)),
            ("@@iterator", |it, t, _| pairs_iter(it, t, 2)),
            ("toString", pairs_to_string),
            ("sort", |_, t, _| {
                with_pairs(&t, |p| p.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16())))?;
                Ok(Value::Undef)
            }),
        ] {
            it.def_method(&p, n, f);
        }
        it.def_getter(&p, "size", |_, t, _| Ok(Value::Num(pairs_of(&t).map_or(0, |(_, p)| p.len()) as f64)));
        let f: NativeFn = match kind {
            PairKind::Headers => |it, _, a| pairs_ctor(it, PairKind::Headers, a),
            PairKind::Params => |it, _, a| pairs_ctor(it, PairKind::Params, a),
            PairKind::Form => |it, _, a| pairs_ctor(it, PairKind::Form, a),
        };
        ctor(it, name, f, &p);
    }

    // Blob / File
    let bp = mk_proto(it, "Blob");
    it.def_method(&bp, "text", |it, t, _| {
        let d = blob_data(&t).unwrap_or_default();
        Ok(resolved(it, Value::str(&String::from_utf8_lossy(&d))))
    });
    it.def_method(&bp, "arrayBuffer", |it, t, _| {
        let d = blob_data(&t).unwrap_or_default();
        let b = Value::Obj(new_buffer(it, d));
        Ok(resolved(it, b))
    });
    it.def_method(&bp, "bytes", |it, t, _| {
        let d = blob_data(&t).unwrap_or_default();
        let b = uint8_array(it, d);
        Ok(resolved(it, b))
    });
    it.def_method(&bp, "slice", |it, t, a| {
        let d = blob_data(&t).unwrap_or_default();
        let n = d.len() as f64;
        let idx = |it: &mut Interp, v: &Value, def: f64| -> R<usize> {
            if v.is_nullish() {
                return Ok(def as usize);
            }
            let x = it.to_num(v)?;
            Ok((if x < 0.0 { (n + x).max(0.0) } else { x.min(n) }) as usize)
        };
        let st = idx(it, &arg(a, 0), 0.0)?;
        let en = idx(it, &arg(a, 1), n)?.max(st);
        let ty = if arg(a, 2).is_nullish() { Rc::from("") } else { it.to_str(&arg(a, 2))? };
        Ok(make_blob(it, d[st..en].to_vec(), &ty))
    });
    ctor(it, "Blob", blob_ctor, &bp);
    let fp = it.new_obj(Some(bp.clone()), Kind::Plain);
    ctor(it, "File", file_ctor, &fp);

    // Response / Request / fetch
    let rp = mk_proto(it, "Response");
    for (n, f) in [
        ("text", body_text as NativeFn),
        ("json", body_json),
        ("arrayBuffer", body_array_buffer),
        ("bytes", body_bytes),
        ("blob", body_blob),
        ("clone", response_clone),
        ("formData", |it, _, _| Ok(rejected(it, type_error("formData() is not supported")))),
    ] {
        it.def_method(&rp, n, f);
    }
    let rc = ctor(it, "Response", response_ctor, &rp);
    it.def_method(&rc, "json", |it, _, a| {
        let g = Value::Obj(it.global.clone());
        let json = it.get(&g, "JSON")?;
        let st = it.get(&json, "stringify")?;
        let text = it.call(&st, json, &[arg(a, 0)])?;
        let text = it.to_str(&text)?;
        Ok(make_response(it, 200, "", vec![(s("content-type"), s("application/json"))], text.as_bytes().to_vec()))
    });
    it.def_method(&rc, "error", |it, _, _| Ok(make_response(it, 0, "", Vec::new(), Vec::new())));
    let rqp = mk_proto(it, "Request");
    for (n, f) in [("text", body_text as NativeFn), ("json", body_json), ("arrayBuffer", body_array_buffer)] {
        it.def_method(&rqp, n, f);
    }
    ctor(it, "Request", request_ctor, &rqp);
    let fetch = it.native(w_fetch, "fetch");
    Interp::set_hidden(&g, "fetch", fetch);

    // XMLHttpRequest
    let xp = mk_proto(it, "XMLHttpRequest");
    for (n, f) in [
        ("open", xhr_open as NativeFn),
        ("send", xhr_send),
        ("abort", xhr_abort),
        ("setRequestHeader", xhr_set_header),
        ("getResponseHeader", xhr_get_header),
        ("getAllResponseHeaders", xhr_all_headers),
        ("overrideMimeType", |_, _, _| Ok(Value::Undef)),
        ("addEventListener", listeners_add),
        ("removeEventListener", listeners_remove),
    ] {
        it.def_method(&xp, n, f);
    }
    let xc = ctor(it, "XMLHttpRequest", xhr_ctor, &xp);
    for (i, n) in ["UNSENT", "OPENED", "HEADERS_RECEIVED", "LOADING", "DONE"].iter().enumerate() {
        Interp::set_hidden(&xc, n, Value::Num(i as f64));
        Interp::set_hidden(&xp, n, Value::Num(i as f64));
    }

    // AbortController / AbortSignal
    let sp = mk_proto(it, "AbortSignal");
    it.def_method(&sp, "addEventListener", listeners_add);
    it.def_method(&sp, "removeEventListener", listeners_remove);
    it.def_method(&sp, "throwIfAborted", |it, t, _| if it.get(&t, "aborted")?.truthy() { Err(it.get(&t, "reason")?) } else { Ok(Value::Undef) });
    let sc = ctor(it, "AbortSignal", |_, _, _| Err(type_error("Illegal constructor")), &sp);
    it.def_method(&sc, "timeout", |it, _, _| Ok(make_signal(it)));
    it.def_method(&sc, "any", |it, _, _| Ok(make_signal(it)));
    it.def_method(&sc, "abort", |it, _, a| {
        let s = make_signal(it);
        let r = if arg(a, 0).is_nullish() { abort_error(it) } else { arg(a, 0) };
        it.put(&s, "aborted", Value::Bool(true))?;
        it.put(&s, "reason", r)?;
        Ok(s)
    });
    let ap = mk_proto(it, "AbortController");
    it.def_method(&ap, "abort", abort_controller_abort);
    ctor(it, "AbortController", abort_controller_ctor, &ap);

    // URL
    let up = mk_proto(it, "URL");
    let accessor = |it: &mut Interp, o: &ObjRef, name: &'static str, get: NativeFn, set: Option<NativeFn>| {
        let g = it.native(get, name);
        let s = set.map(|f| it.native(f, name));
        o.borrow_mut().props.insert(Rc::from(name), Prop { v: Value::Undef, get: Some(g), set: s, enumerable: false });
    };
    accessor(it, &up, "href", url_href, Some(url_set_href));
    accessor(it, &up, "search", |it, t, _| Ok(Value::str(&url_search(it, &t)?)), Some(url_set_search));
    accessor(it, &up, "host", |it, t, _| Ok(Value::str(&url_host(it, &t)?)), Some(url_set_host));
    accessor(it, &up, "origin", url_origin, None);
    it.def_method(&up, "toString", url_href);
    it.def_method(&up, "toJSON", url_href);
    let uc = ctor(it, "URL", url_ctor, &up);
    it.def_method(&uc, "canParse", |it, _, a| {
        let ok = url_ctor(it, Value::Undef, a).is_ok();
        Ok(Value::Bool(ok))
    });
    it.def_method(&uc, "parse", |it, _, a| Ok(url_ctor(it, Value::Undef, a).unwrap_or(Value::Null)));
    it.def_method(&uc, "createObjectURL", |it, _, _| {
        let id = it.symbols;
        it.symbols += 1;
        Ok(Value::str(&format!("blob:{}/{id}", it.url.split('/').take(3).collect::<Vec<_>>().join("/"))))
    });
    it.def_method(&uc, "revokeObjectURL", |_, _, _| Ok(Value::Undef));
    Interp::set_hidden(&g, "webkitURL", Value::Obj(uc));

    // Element constructors and document.createEvent
    let ic = it.native(image_ctor, "Image");
    Interp::set_hidden(&g, "Image", ic);
    let oc = it.native(option_ctor, "Option");
    Interp::set_hidden(&g, "Option", oc);
    let auc = it.native(audio_ctor, "Audio");
    Interp::set_hidden(&g, "Audio", auc);
    if let Some(ev) = it.p.event.clone() {
        it.def_method(&ev, "initEvent", |it, t, a| {
            it.put(&t, "type", arg(a, 0))?;
            it.put(&t, "bubbles", Value::Bool(arg(a, 1).truthy()))?;
            it.put(&t, "cancelable", Value::Bool(arg(a, 2).truthy()))?;
            Ok(Value::Undef)
        });
        it.def_method(&ev, "initCustomEvent", |it, t, a| {
            it.put(&t, "type", arg(a, 0))?;
            it.put(&t, "bubbles", Value::Bool(arg(a, 1).truthy()))?;
            it.put(&t, "cancelable", Value::Bool(arg(a, 2).truthy()))?;
            it.put(&t, "detail", arg(a, 3))?;
            Ok(Value::Undef)
        });
    }
    let docv = it.get(&Value::Obj(g.clone()), "document").unwrap_or_default();
    if let Value::Obj(d) = &docv {
        it.def_method(d, "createEvent", create_event);
    }

    // performance.timing / navigation entries
    let gv = Value::Obj(g.clone());
    if let Ok(Value::Obj(perf)) = it.get(&gv, "performance") {
        let origin = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        let timing = it.plain();
        for (k, off) in [
            ("navigationStart", 0.0),
            ("fetchStart", 1.0),
            ("domainLookupStart", 1.0),
            ("domainLookupEnd", 1.0),
            ("connectStart", 1.0),
            ("connectEnd", 1.0),
            ("requestStart", 2.0),
            ("responseStart", 50.0),
            ("responseEnd", 60.0),
            ("domLoading", 60.0),
            ("domInteractive", 80.0),
            ("domContentLoadedEventStart", 80.0),
            ("domContentLoadedEventEnd", 81.0),
            ("domComplete", 90.0),
            ("loadEventStart", 90.0),
            ("loadEventEnd", 91.0),
        ] {
            Interp::set_prop(&timing, k, Value::Num(origin + off));
        }
        Interp::set_prop(&perf, "timing", Value::Obj(timing));
        Interp::set_prop(&perf, "timeOrigin", Value::Num(origin));
        let navi = it.plain();
        Interp::set_prop(&navi, "type", Value::Num(0.0));
        Interp::set_prop(&navi, "redirectCount", Value::Num(0.0));
        Interp::set_prop(&perf, "navigation", Value::Obj(navi));
        it.def_method(&perf, "getEntriesByType", |it, _, a| {
            let t = it.to_str(&arg(a, 0))?;
            if &*t != "navigation" {
                return Ok(it.array(Vec::new()));
            }
            let e = it.plain();
            for (k, v) in [("startTime", 0.0), ("responseStart", 50.0), ("responseEnd", 60.0), ("domInteractive", 80.0), ("domContentLoadedEventEnd", 81.0), ("loadEventEnd", 91.0), ("duration", 91.0), ("transferSize", 0.0)] {
                Interp::set_prop(&e, k, Value::Num(v));
            }
            Interp::set_prop(&e, "entryType", Value::str("navigation"));
            Interp::set_prop(&e, "name", Value::str(&it.url));
            Interp::set_prop(&e, "type", Value::str("navigate"));
            Ok(it.array(vec![Value::Obj(e)]))
        });
        it.def_method(&perf, "getEntries", |it, _, _| Ok(it.array(Vec::new())));
        it.def_method(&perf, "toJSON", |it, _, _| Ok(Value::Obj(it.plain())));
    }

    // navigator extras (a Chrome-like profile)
    if let Ok(Value::Obj(nav)) = it.get(&gv, "navigator") {
        for (k, v) in [
            ("vendor", "Google Inc."),
            ("appName", "Netscape"),
            ("appCodeName", "Mozilla"),
            ("product", "Gecko"),
            ("productSub", "20030107"),
            ("appVersion", "5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"),
        ] {
            Interp::set_prop(&nav, k, Value::str(v));
        }
        Interp::set_prop(&nav, "webdriver", Value::Bool(false));
        Interp::set_prop(&nav, "pdfViewerEnabled", Value::Bool(false));
        Interp::set_prop(&nav, "deviceMemory", Value::Num(8.0));
        let brands: Vec<Value> = [("Chromium", "131"), ("Google Chrome", "131"), ("Not_A Brand", "24")]
            .iter()
            .map(|(b, v)| {
                let o = it.plain();
                Interp::set_prop(&o, "brand", Value::str(b));
                Interp::set_prop(&o, "version", Value::str(v));
                Value::Obj(o)
            })
            .collect();
        let uad = it.plain();
        let ba = it.array(brands);
        Interp::set_prop(&uad, "brands", ba);
        Interp::set_prop(&uad, "mobile", Value::Bool(false));
        Interp::set_prop(&uad, "platform", Value::str("Windows"));
        it.def_method(&uad, "getHighEntropyValues", |it, t, _| {
            let o = it.plain();
            let b = it.get(&t, "brands")?;
            Interp::set_prop(&o, "brands", b);
            Interp::set_prop(&o, "mobile", Value::Bool(false));
            Interp::set_prop(&o, "platform", Value::str("Windows"));
            Interp::set_prop(&o, "platformVersion", Value::str("15.0.0"));
            Interp::set_prop(&o, "architecture", Value::str("x86"));
            Interp::set_prop(&o, "bitness", Value::str("64"));
            Interp::set_prop(&o, "uaFullVersion", Value::str("131.0.0.0"));
            Ok(resolved(it, Value::Obj(o)))
        });
        Interp::set_prop(&nav, "userAgentData", Value::Obj(uad));
        let conn = it.plain();
        Interp::set_prop(&conn, "effectiveType", Value::str("4g"));
        Interp::set_prop(&conn, "downlink", Value::Num(10.0));
        Interp::set_prop(&conn, "rtt", Value::Num(50.0));
        Interp::set_prop(&conn, "saveData", Value::Bool(false));
        it.def_method(&conn, "addEventListener", |_, _, _| Ok(Value::Undef));
        Interp::set_prop(&nav, "connection", Value::Obj(conn));
        let clip = it.plain();
        it.def_method(&clip, "writeText", |it, _, _| Ok(resolved(it, Value::Undef)));
        it.def_method(&clip, "readText", |it, _, _| Ok(resolved(it, Value::str(""))));
        Interp::set_prop(&nav, "clipboard", Value::Obj(clip));
        let perms = it.plain();
        it.def_method(&perms, "query", |it, _, _| {
            let o = it.plain();
            Interp::set_prop(&o, "state", Value::str("prompt"));
            Ok(resolved(it, Value::Obj(o)))
        });
        Interp::set_prop(&nav, "permissions", Value::Obj(perms));
        it.def_method(&nav, "sendBeacon", |it, _, a| {
            let url = input_url(it, &arg(a, 0))?;
            let body = encode_body(it, &arg(a, 1))?;
            let mut h = Vec::new();
            if let Some((_, Some(ct))) = &body {
                h.push(("Content-Type".to_string(), ct.clone()));
            }
            it.start_request("POST", &url, h, body.map(|b| b.0), Target::Ignore);
            Ok(Value::Bool(true))
        });
        it.def_method(&nav, "javaEnabled", |_, _, _| Ok(Value::Bool(false)));
        it.def_method(&nav, "vibrate", |_, _, _| Ok(Value::Bool(false)));
    }
}

#[cfg(test)]
mod tests {
    use super::super::Interp;
    use crate::dom::Document;

    fn run(src: &str) -> String {
        let mut it = Interp::new(Document::new(), "https://example.com/dir/page.html", 800.0, 600.0);
        it.run_script(src).unwrap_or_else(|e| panic!("{e:?}"));
        it.run_microtasks();
        while it.has_pending_requests() {
            it.poll_requests(Some(std::time::Duration::from_secs(5)));
        }
        it.run_microtasks();
        it.console.join("\n")
    }

    #[test]
    fn url_and_params() {
        let out = run(r#"
            const u = new URL('../a/b.html?x=1&y=two+words#frag', 'https://user@Example.com:443/dir/page.html');
            console.log(u.href, u.host, u.pathname, u.search, u.hash, u.origin);
            u.searchParams.set('x', '3');
            u.searchParams.append('z', 'a&b');
            console.log(u.toString());
            const p = new URLSearchParams({a: '1', b: 'x y'});
            p.append('a', '2');
            console.log(p.toString(), p.getAll('a').join('|'), p.get('missing'), [...p.keys()].join(''));
            let bad = false; try { new URL('not a url'); } catch (e) { bad = e instanceof TypeError; }
            console.log(bad, URL.canParse('mailto:x@y.z'), new URL('mailto:x@y.z').pathname);
            const h = new Headers({'Content-Type': 'text/html'});
            h.append('X-A', '1'); h.append('x-a', '2');
            console.log(h.get('content-type'), h.get('X-A'), h.has('nope'));
        "#);
        assert_eq!(
            out,
            "https://user@example.com/a/b.html?x=1&y=two+words#frag example.com /a/b.html ?x=1&y=two+words #frag https://example.com\n\
             https://user@example.com/a/b.html?x=3&y=two+words&z=a%26b#frag\n\
             a=1&b=x+y&a=2 1|2 null aba\n\
             true true x@y.z\n\
             text/html 1, 2 false"
        );
    }

    #[test]
    fn fetch_and_xhr_data_urls() {
        let out = run(r#"
            fetch('data:application/json,{"a":[1,2]}').then(r => r.json()).then(j => console.log('fetch', j.a[1]));
            fetch('data:text/plain,hello').then(r => r.arrayBuffer()).then(b => console.log('bytes', new Uint8Array(b)[0]));
            const x = new XMLHttpRequest();
            x.onload = () => console.log('xhr', x.status, x.responseText);
            x.open('GET', 'data:text/plain,hi%20there');
            x.send();
            const c = new AbortController();
            c.abort();
            fetch('data:text/plain,x', {signal: c.signal}).catch(e => console.log('aborted', e.name));
            new Blob(['ab', new Uint8Array([99])], {type: 'text/plain'}).text().then(t => console.log('blob', t));
            new Response('{"ok":1}').json().then(j => console.log('resp', j.ok));
        "#);
        let mut lines: Vec<&str> = out.lines().collect();
        lines.sort();
        assert_eq!(lines, ["aborted AbortError", "blob abc", "bytes 104", "fetch 2", "resp 1", "xhr 200 hi there"]);
    }
}

#[cfg(test)]
mod live_tests {
    /// Needs the network: `cargo test --release -- --ignored live_fetch`.
    #[test]
    #[ignore]
    fn live_fetch() {
        let mut it = super::super::Interp::new(crate::dom::Document::new(), "https://example.com/", 800.0, 600.0);
        it.run_script(
            r#"
            fetch('https://api.github.com/repos/rust-lang/rust').then(r => { console.log('status', r.status, r.headers.get('content-type')); return r.json(); }).then(j => console.log('repo', j.full_name));
            fetch('https://httpbin.org/post', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({a: 1})}).then(r => r.json()).then(j => console.log('echo', j.json.a, j.headers['Content-Type']));
            const x = new XMLHttpRequest();
            x.open('GET', 'https://httpbin.org/get?q=bolt');
            x.responseType = 'json';
            x.onload = () => console.log('xhr', x.status, x.response.args.q);
            x.send();
            fetch('https://httpbin.org/status/404').then(r => console.log('notfound', r.status, r.ok));
        "#,
        )
        .unwrap();
        let t = std::time::Instant::now();
        while it.has_pending_requests() && t.elapsed().as_secs() < 30 {
            it.poll_requests(Some(std::time::Duration::from_millis(100)));
        }
        it.run_microtasks();
        let mut lines = it.console.clone();
        lines.sort();
        eprintln!("{lines:#?}");
        assert!(lines.iter().any(|l| l == "repo rust-lang/rust"));
        assert!(lines.iter().any(|l| l == "echo 1 application/json"));
        assert!(lines.iter().any(|l| l == "xhr 200 bolt"));
        assert!(lines.iter().any(|l| l == "notfound 404 false"));
    }
}
