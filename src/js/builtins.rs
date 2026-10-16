//! Standard library: Object, Function, Array, String, Number, Math, JSON,
//! Error, RegExp, Date, Map, Set, Promise, Symbol, timers, URI functions.

use super::regex::Regex;
use super::*;

pub fn arg(a: &[Value], i: usize) -> Value {
    a.get(i).cloned().unwrap_or_default()
}

fn num(it: &mut Interp, a: &[Value], i: usize) -> R<f64> {
    it.to_num(&arg(a, i))
}

fn ok_num(n: f64) -> R<Value> {
    Ok(Value::Num(n))
}

pub fn make_error_value(name: &str, msg: &str) -> Value {
    // Callers without interpreter access: the prototype is attached lazily
    // via `name`; `instanceof` works for errors made through constructors.
    ERR_PROTOS.with(|p| {
        let proto = p.borrow().get(name).cloned().or_else(|| p.borrow().get("Error").cloned());
        let o = Rc::new(RefCell::new(Obj { props: Props::default(), proto, kind: Kind::Error }));
        Interp::set_hidden(&o, "message", Value::str(msg));
        Interp::set_hidden(&o, "stack", Value::str(&format!("{name}: {msg}")));
        Value::Obj(o)
    })
}

thread_local! {
    static ERR_PROTOS: RefCell<FxHashMap<String, ObjRef>> = RefCell::new(FxHashMap::default());
}

fn this_str(it: &mut Interp, this: &Value) -> R<Rc<str>> {
    if let Value::Obj(o) = this {
        if let Kind::Boxed(v) = &o.borrow().kind {
            let v = v.clone();
            return it.to_str(&v);
        }
    }
    if this.is_nullish() {
        return Err(type_error("String.prototype method called on null or undefined"));
    }
    it.to_str(this)
}

fn this_arr(this: &Value) -> R<ObjRef> {
    match this {
        Value::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => Ok(o.clone()),
        _ => Err(type_error("not an array")),
    }
}

fn arr_vec(this: &Value) -> R<Vec<Value>> {
    let o = this_arr(this)?;
    let b = o.borrow();
    if let Kind::Array(a) = &b.kind { Ok(a.clone()) } else { unreachable!() }
}

fn with_arr<T>(this: &Value, f: impl FnOnce(&mut Vec<Value>) -> T) -> R<T> {
    let o = this_arr(this)?;
    let mut b = o.borrow_mut();
    if let Kind::Array(a) = &mut b.kind { Ok(f(a)) } else { unreachable!() }
}

fn rel_index(i: f64, len: usize) -> usize {
    if i < 0.0 { (len as f64 + i).max(0.0) as usize } else { (i as usize).min(len) }
}

// ------------------------------------------------------------------ install

pub fn install(it: &mut Interp) {
    let object = Rc::new(RefCell::new(Obj { props: Props::default(), proto: None, kind: Kind::Plain }));
    it.p.object = Some(object.clone());
    let function = it.new_obj(Some(object.clone()), Kind::Func(Func::Native(|_, _, _| Ok(Value::Undef), "")));
    it.p.function = Some(function.clone());
    it.global.borrow_mut().proto = Some(object.clone());
    let mk = |it: &Interp| it.new_obj(Some(object.clone()), Kind::Plain);
    it.p.array = Some(it.new_obj(Some(object.clone()), Kind::Array(Vec::new())));
    it.p.string = Some(mk(it));
    it.p.number = Some(mk(it));
    it.p.boolean = Some(mk(it));
    it.p.error = Some(mk(it));
    it.p.regexp = Some(mk(it));
    it.p.date = Some(mk(it));
    it.p.promise = Some(mk(it));
    it.p.map = Some(mk(it));
    it.p.set = Some(mk(it));

    let g = it.global.clone();
    let gv = Value::Obj(g.clone());
    for n in ["window", "self", "globalThis", "top", "parent", "frames"] {
        Interp::set_hidden(&g, n, gv.clone());
    }
    Interp::set_hidden(&g, "NaN", Value::Num(f64::NAN));
    Interp::set_hidden(&g, "Infinity", Value::Num(f64::INFINITY));

    // --- constructors
    let ctor = |it: &mut Interp, name: &'static str, f: NativeFn, proto: &ObjRef| -> ObjRef {
        let c = it.new_obj(it.p.function.clone(), Kind::Func(Func::Native(f, name)));
        Interp::set_hidden(&c, "prototype", Value::Obj(proto.clone()));
        Interp::set_hidden(&c, "name", Value::str(name));
        Interp::set_hidden(proto, "constructor", Value::Obj(c.clone()));
        Interp::set_hidden(&it.global, name, Value::Obj(c.clone()));
        c
    };

    // Object
    let oc = ctor(it, "Object", object_ctor, &object);
    for (n, f) in [
        ("keys", obj_keys as NativeFn),
        ("values", obj_values),
        ("entries", obj_entries),
        ("assign", obj_assign),
        ("create", obj_create),
        ("getPrototypeOf", obj_get_proto),
        ("setPrototypeOf", obj_set_proto),
        ("defineProperty", obj_define_property),
        ("defineProperties", obj_define_properties),
        ("getOwnPropertyNames", obj_keys),
        ("getOwnPropertyDescriptor", obj_get_own_desc),
        ("freeze", identity),
        ("seal", identity),
        ("preventExtensions", identity),
        ("isFrozen", ret_false),
        ("isSealed", ret_false),
        ("isExtensible", ret_true),
        ("fromEntries", obj_from_entries),
        ("is", obj_is),
        ("getOwnPropertySymbols", empty_array),
    ] {
        it.def_method(&oc, n, f);
    }
    for (n, f) in [
        ("hasOwnProperty", obj_has_own as NativeFn),
        ("toString", obj_to_string),
        ("toLocaleString", obj_to_string),
        ("valueOf", identity_this),
        ("isPrototypeOf", obj_is_proto_of),
        ("propertyIsEnumerable", obj_has_own),
    ] {
        it.def_method(&object, n, f);
    }

    // Function
    let fc = ctor(it, "Function", function_ctor, &function);
    let _ = fc;
    for (n, f) in [("call", fn_call as NativeFn), ("apply", fn_apply), ("bind", fn_bind), ("toString", fn_to_string)] {
        it.def_method(&function, n, f);
    }

    // Array
    let ap = it.p.array.clone().unwrap();
    let ac = ctor(it, "Array", array_ctor, &ap);
    it.def_method(&ac, "isArray", array_is_array);
    it.def_method(&ac, "from", array_from);
    it.def_method(&ac, "of", array_of);
    for (n, f) in [
        ("push", arr_push as NativeFn),
        ("pop", arr_pop),
        ("shift", arr_shift),
        ("unshift", arr_unshift),
        ("slice", arr_slice),
        ("splice", arr_splice),
        ("concat", arr_concat),
        ("join", arr_join),
        ("toString", arr_join),
        ("reverse", arr_reverse),
        ("indexOf", arr_index_of),
        ("lastIndexOf", arr_last_index_of),
        ("includes", arr_includes),
        ("find", arr_find),
        ("findIndex", arr_find_index),
        ("findLast", arr_find_last),
        ("findLastIndex", arr_find_last_index),
        ("filter", arr_filter),
        ("map", arr_map),
        ("forEach", arr_for_each),
        ("reduce", arr_reduce),
        ("reduceRight", arr_reduce_right),
        ("some", arr_some),
        ("every", arr_every),
        ("sort", arr_sort),
        ("flat", arr_flat),
        ("flatMap", arr_flat_map),
        ("fill", arr_fill),
        ("keys", arr_keys),
        ("values", identity_this),
        ("entries", arr_entries),
        ("at", arr_at),
        ("toSorted", arr_to_sorted),
        ("toReversed", arr_to_reversed),
    ] {
        it.def_method(&ap, n, f);
    }

    // String
    let sp = it.p.string.clone().unwrap();
    let sc = ctor(it, "String", string_ctor, &sp);
    it.def_method(&sc, "fromCharCode", str_from_char_code);
    it.def_method(&sc, "fromCodePoint", str_from_char_code);
    it.def_method(&sc, "raw", str_raw);
    for (n, f) in [
        ("charAt", str_char_at as NativeFn),
        ("charCodeAt", str_char_code_at),
        ("codePointAt", str_char_code_at),
        ("indexOf", str_index_of),
        ("lastIndexOf", str_last_index_of),
        ("includes", str_includes),
        ("startsWith", str_starts_with),
        ("endsWith", str_ends_with),
        ("slice", str_slice),
        ("substring", str_substring),
        ("substr", str_substr),
        ("toUpperCase", str_upper),
        ("toLowerCase", str_lower),
        ("toLocaleUpperCase", str_upper),
        ("toLocaleLowerCase", str_lower),
        ("trim", str_trim),
        ("trimStart", str_trim_start),
        ("trimEnd", str_trim_end),
        ("trimLeft", str_trim_start),
        ("trimRight", str_trim_end),
        ("split", str_split),
        ("replace", str_replace),
        ("replaceAll", str_replace_all),
        ("match", str_match),
        ("matchAll", str_match_all),
        ("search", str_search),
        ("padStart", str_pad_start),
        ("padEnd", str_pad_end),
        ("repeat", str_repeat),
        ("concat", str_concat),
        ("at", str_at),
        ("localeCompare", str_locale_compare),
        ("normalize", identity_str),
        ("toString", identity_str),
        ("valueOf", identity_str),
    ] {
        it.def_method(&sp, n, f);
    }

    // Number / Boolean
    let np = it.p.number.clone().unwrap();
    let nc = ctor(it, "Number", number_ctor, &np);
    for (n, f) in [("isInteger", num_is_integer as NativeFn), ("isSafeInteger", num_is_integer), ("isFinite", num_is_finite), ("isNaN", num_is_nan), ("parseFloat", parse_float), ("parseInt", parse_int)] {
        it.def_method(&nc, n, f);
    }
    for (n, v) in [("MAX_SAFE_INTEGER", 9007199254740991.0), ("MIN_SAFE_INTEGER", -9007199254740991.0), ("EPSILON", f64::EPSILON), ("MAX_VALUE", f64::MAX), ("MIN_VALUE", 5e-324), ("POSITIVE_INFINITY", f64::INFINITY), ("NEGATIVE_INFINITY", f64::NEG_INFINITY), ("NaN", f64::NAN)] {
        Interp::set_hidden(&nc, n, Value::Num(v));
    }
    for (n, f) in [("toFixed", num_to_fixed as NativeFn), ("toString", num_to_string), ("toPrecision", num_to_precision), ("toLocaleString", num_to_locale), ("valueOf", num_value_of)] {
        it.def_method(&np, n, f);
    }
    let bp = it.p.boolean.clone().unwrap();
    ctor(it, "Boolean", boolean_ctor, &bp);
    it.def_method(&bp, "toString", bool_to_string);
    it.def_method(&bp, "valueOf", num_value_of);

    // Math
    let math = it.plain();
    for (n, f) in [
        ("abs", m_abs as NativeFn),
        ("floor", m_floor),
        ("ceil", m_ceil),
        ("round", m_round),
        ("trunc", m_trunc),
        ("sign", m_sign),
        ("sqrt", m_sqrt),
        ("cbrt", m_cbrt),
        ("sin", m_sin),
        ("cos", m_cos),
        ("tan", m_tan),
        ("asin", m_asin),
        ("acos", m_acos),
        ("atan", m_atan),
        ("exp", m_exp),
        ("log", m_log),
        ("log2", m_log2),
        ("log10", m_log10),
        ("log1p", m_log1p),
        ("atan2", m_atan2),
        ("pow", m_pow),
        ("min", m_min),
        ("max", m_max),
        ("hypot", m_hypot),
        ("random", m_random),
        ("fround", m_fround),
        ("imul", m_imul),
        ("clz32", m_clz32),
    ] {
        it.def_method(&math, n, f);
    }
    for (n, v) in [("PI", std::f64::consts::PI), ("E", std::f64::consts::E), ("LN2", std::f64::consts::LN_2), ("LN10", std::f64::consts::LN_10), ("LOG2E", std::f64::consts::LOG2_E), ("LOG10E", std::f64::consts::LOG10_E), ("SQRT2", std::f64::consts::SQRT_2), ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2)] {
        Interp::set_hidden(&math, n, Value::Num(v));
    }
    Interp::set_hidden(&g, "Math", Value::Obj(math));

    // JSON
    let json = it.plain();
    it.def_method(&json, "stringify", json_stringify);
    it.def_method(&json, "parse", json_parse);
    Interp::set_hidden(&g, "JSON", Value::Obj(json));

    // Errors
    let ep = it.p.error.clone().unwrap();
    ctor(it, "Error", error_ctor, &ep);
    Interp::set_hidden(&ep, "name", Value::str("Error"));
    Interp::set_hidden(&ep, "message", Value::str(""));
    it.def_method(&ep, "toString", error_to_string);
    ERR_PROTOS.with(|p| p.borrow_mut().insert("Error".into(), ep.clone()));
    for name in ["TypeError", "ReferenceError", "SyntaxError", "RangeError", "EvalError", "URIError", "DOMException"] {
        let proto = it.new_obj(Some(ep.clone()), Kind::Plain);
        Interp::set_hidden(&proto, "name", Value::str(name));
        let f: NativeFn = match name {
            "TypeError" => |it, t, a| error_named(it, t, a, "TypeError"),
            "ReferenceError" => |it, t, a| error_named(it, t, a, "ReferenceError"),
            "SyntaxError" => |it, t, a| error_named(it, t, a, "SyntaxError"),
            "RangeError" => |it, t, a| error_named(it, t, a, "RangeError"),
            "EvalError" => |it, t, a| error_named(it, t, a, "EvalError"),
            "URIError" => |it, t, a| error_named(it, t, a, "URIError"),
            _ => |it, t, a| error_named(it, t, a, "DOMException"),
        };
        ctor(it, name, f, &proto);
        ERR_PROTOS.with(|p| p.borrow_mut().insert(name.into(), proto.clone()));
        it.p.errors.insert(name, proto);
    }

    // RegExp
    let rp = it.p.regexp.clone().unwrap();
    ctor(it, "RegExp", regexp_ctor, &rp);
    for (n, f) in [("test", re_test as NativeFn), ("exec", re_exec), ("toString", re_to_string)] {
        it.def_method(&rp, n, f);
    }

    // Date
    let dp = it.p.date.clone().unwrap();
    let dc = ctor(it, "Date", date_ctor, &dp);
    it.def_method(&dc, "now", date_now);
    it.def_method(&dc, "parse", date_parse);
    it.def_method(&dc, "UTC", date_utc);
    for (n, f) in [
        ("getTime", date_get_time as NativeFn),
        ("valueOf", date_get_time),
        ("getFullYear", |it, t, _| date_part(it, &t, 0)),
        ("getMonth", |it, t, _| date_part(it, &t, 1)),
        ("getDate", |it, t, _| date_part(it, &t, 2)),
        ("getDay", |it, t, _| date_part(it, &t, 3)),
        ("getHours", |it, t, _| date_part(it, &t, 4)),
        ("getMinutes", |it, t, _| date_part(it, &t, 5)),
        ("getSeconds", |it, t, _| date_part(it, &t, 6)),
        ("getMilliseconds", |it, t, _| date_part(it, &t, 7)),
        ("getUTCFullYear", |it, t, _| date_part(it, &t, 0)),
        ("getUTCMonth", |it, t, _| date_part(it, &t, 1)),
        ("getUTCDate", |it, t, _| date_part(it, &t, 2)),
        ("getUTCDay", |it, t, _| date_part(it, &t, 3)),
        ("getUTCHours", |it, t, _| date_part(it, &t, 4)),
        ("getUTCMinutes", |it, t, _| date_part(it, &t, 5)),
        ("getUTCSeconds", |it, t, _| date_part(it, &t, 6)),
        ("getTimezoneOffset", |_, _, _| Ok(Value::Num(0.0))),
        ("toISOString", date_iso),
        ("toJSON", date_iso),
        ("toString", date_to_string),
        ("toUTCString", date_to_string),
        ("toDateString", date_to_string),
        ("toLocaleDateString", date_locale_date),
        ("toLocaleTimeString", date_locale_time),
        ("toLocaleString", date_to_string),
        ("setTime", date_set_time),
    ] {
        it.def_method(&dp, n, f);
    }

    // Map / Set
    let mp = it.p.map.clone().unwrap();
    ctor(it, "Map", map_ctor, &mp);
    ctor(it, "WeakMap", map_ctor, &mp);
    for (n, f) in [("get", map_get as NativeFn), ("set", map_set), ("has", map_has), ("delete", map_delete), ("clear", coll_clear), ("forEach", map_for_each), ("keys", map_keys), ("values", map_values), ("entries", map_entries)] {
        it.def_method(&mp, n, f);
    }
    it.def_getter(&mp, "size", coll_size);
    let stp = it.p.set.clone().unwrap();
    ctor(it, "Set", set_ctor, &stp);
    ctor(it, "WeakSet", set_ctor, &stp);
    for (n, f) in [("add", set_add as NativeFn), ("has", set_has), ("delete", set_delete), ("clear", coll_clear), ("forEach", set_for_each), ("values", set_values), ("keys", set_values), ("entries", set_values)] {
        it.def_method(&stp, n, f);
    }
    it.def_getter(&stp, "size", coll_size);

    // Promise
    let pp = it.p.promise.clone().unwrap();
    let pc = ctor(it, "Promise", promise_ctor, &pp);
    for (n, f) in [("resolve", promise_static_resolve as NativeFn), ("reject", promise_static_reject), ("all", promise_all), ("allSettled", promise_all_settled), ("race", promise_race), ("any", promise_race)] {
        it.def_method(&pc, n, f);
    }
    for (n, f) in [("then", promise_then as NativeFn), ("catch", promise_catch), ("finally", promise_finally)] {
        it.def_method(&pp, n, f);
    }

    // Generators
    let gp = it.new_obj(Some(object.clone()), Kind::Plain);
    for (n, f) in [("next", gen_next as NativeFn), ("return", gen_return), ("throw", gen_throw), ("@@iterator", identity_this), ("@@asyncIterator", identity_this)] {
        it.def_method(&gp, n, f);
    }
    it.p.generator = Some(gp);

    // Symbol
    let sym = it.native(symbol_fn, "Symbol");
    if let Value::Obj(s) = &sym {
        Interp::set_hidden(s, "iterator", Value::str("@@iterator"));
        Interp::set_hidden(s, "asyncIterator", Value::str("@@asyncIterator"));
        Interp::set_hidden(s, "toStringTag", Value::str("@@toStringTag"));
        Interp::set_hidden(s, "hasInstance", Value::str("@@hasInstance"));
        it.def_method(s, "for", symbol_for);
    }
    Interp::set_hidden(&g, "Symbol", sym);

    // Globals
    for (n, f) in [
        ("parseInt", parse_int as NativeFn),
        ("parseFloat", parse_float),
        ("isNaN", global_is_nan),
        ("isFinite", global_is_finite),
        ("encodeURIComponent", encode_uri_component),
        ("encodeURI", encode_uri),
        ("decodeURIComponent", decode_uri),
        ("decodeURI", decode_uri),
        ("escape", encode_uri_component),
        ("unescape", decode_uri),
        ("setTimeout", set_timeout),
        ("setInterval", set_interval),
        ("clearTimeout", clear_timer),
        ("clearInterval", clear_timer),
        ("requestAnimationFrame", request_animation_frame),
        ("cancelAnimationFrame", clear_timer),
        ("requestIdleCallback", set_timeout),
        ("cancelIdleCallback", clear_timer),
        ("queueMicrotask", queue_microtask),
        ("btoa", btoa),
        ("atob", atob),
        ("structuredClone", structured_clone),
    ] {
        it.def_method(&g, n, f);
    }
}

// ------------------------------------------------------------------- misc

fn identity(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(arg(a, 0))
}
fn identity_this(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(t)
}
fn identity_str(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::Str(this_str(it, &t)?))
}
fn ret_false(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(Value::Bool(false))
}
fn ret_true(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(Value::Bool(true))
}
fn empty_array(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    Ok(it.array(Vec::new()))
}

// ----------------------------------------------------------------- Object

fn object_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    match arg(a, 0) {
        v @ Value::Obj(_) => Ok(v),
        Value::Undef | Value::Null => Ok(Value::Obj(it.plain())),
        v => Ok(Value::Obj(it.new_obj(it.proto_for(&v), Kind::Boxed(v)))),
    }
}

impl Interp {
    fn proto_for(&self, v: &Value) -> Option<ObjRef> {
        match v {
            Value::Str(_) => self.p.string.clone(),
            Value::Num(_) => self.p.number.clone(),
            Value::Bool(_) => self.p.boolean.clone(),
            _ => self.p.object.clone(),
        }
    }
}

fn obj_keys(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let ks = it.own_keys(&arg(a, 0)).into_iter().map(Value::Str).collect();
    Ok(it.array(ks))
}
fn obj_values(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let o = arg(a, 0);
    let mut out = Vec::new();
    for k in it.own_keys(&o) {
        out.push(it.get(&o, &k)?);
    }
    Ok(it.array(out))
}
fn obj_entries(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let o = arg(a, 0);
    let mut out = Vec::new();
    for k in it.own_keys(&o) {
        let v = it.get(&o, &k)?;
        out.push(it.array(vec![Value::Str(k), v]));
    }
    Ok(it.array(out))
}
fn obj_assign(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let t = arg(a, 0);
    for s in a.iter().skip(1) {
        for k in it.own_keys(s) {
            let v = it.get(s, &k)?;
            it.put(&t, &k, v)?;
        }
    }
    Ok(t)
}
fn obj_create(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let proto = arg(a, 0).obj().cloned();
    let o = it.new_obj(proto, Kind::Plain);
    if let props @ Value::Obj(_) = arg(a, 1) {
        obj_define_properties(it, Value::Undef, &[Value::Obj(o.clone()), props])?;
    }
    Ok(Value::Obj(o))
}
fn obj_get_proto(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(match arg(a, 0) {
        Value::Obj(o) => o.borrow().proto.clone().map(Value::Obj).unwrap_or(Value::Null),
        v => it.proto_for(&v).map(Value::Obj).unwrap_or(Value::Null),
    })
}
fn obj_set_proto(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if let Value::Obj(o) = arg(a, 0) {
        o.borrow_mut().proto = arg(a, 1).obj().cloned();
    }
    Ok(arg(a, 0))
}
fn obj_define_property(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let (o, k, d) = (arg(a, 0), arg(a, 1), arg(a, 2));
    let key = it.key(&k)?;
    let get = it.get(&d, "get")?;
    let set = it.get(&d, "set")?;
    let enumerable = it.get(&d, "enumerable")?.truthy();
    if let Value::Obj(ob) = &o {
        if Interp::is_callable(&get) || Interp::is_callable(&set) {
            let mut b = ob.borrow_mut();
            let prev = b.props.get(&key).cloned();
            let mut p = prev.unwrap_or(Prop { v: Value::Undef, get: None, set: None, enumerable });
            if Interp::is_callable(&get) {
                p.get = Some(get);
            }
            if Interp::is_callable(&set) {
                p.set = Some(set);
            }
            b.props.insert(key, p);
        } else {
            let v = it.get(&d, "value")?;
            if matches!(ob.borrow().kind, Kind::Array(_)) {
                it.put(&o, &key, v)?;
            } else {
                ob.borrow_mut().props.insert(key, Prop { v, get: None, set: None, enumerable });
            }
        }
    }
    Ok(o)
}
fn obj_define_properties(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let (o, ps) = (arg(a, 0), arg(a, 1));
    for k in it.own_keys(&ps) {
        let d = it.get(&ps, &k)?;
        obj_define_property(it, Value::Undef, &[o.clone(), Value::Str(k), d])?;
    }
    Ok(o)
}
fn obj_get_own_desc(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let (o, k) = (arg(a, 0), arg(a, 1));
    let key = it.key(&k)?;
    let Value::Obj(ob) = &o else { return Ok(Value::Undef) };
    let p = ob.borrow().props.get(&key).cloned();
    let Some(p) = p else {
        if it.has_prop(&o, &key) {
            let v = it.get(&o, &key)?;
            let d = it.plain();
            Interp::set_prop(&d, "value", v);
            return Ok(Value::Obj(d));
        }
        return Ok(Value::Undef);
    };
    let d = it.plain();
    if p.get.is_some() || p.set.is_some() {
        Interp::set_prop(&d, "get", p.get.unwrap_or_default());
        Interp::set_prop(&d, "set", p.set.unwrap_or_default());
    } else {
        Interp::set_prop(&d, "value", p.v);
        Interp::set_prop(&d, "writable", Value::Bool(true));
    }
    Interp::set_prop(&d, "enumerable", Value::Bool(p.enumerable));
    Interp::set_prop(&d, "configurable", Value::Bool(true));
    Ok(Value::Obj(d))
}
fn obj_from_entries(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let o = it.plain();
    for e in it.iterate(&arg(a, 0))? {
        let k = it.get(&e, "0")?;
        let v = it.get(&e, "1")?;
        let k = it.key(&k)?;
        Interp::set_prop(&o, &k, v);
    }
    Ok(Value::Obj(o))
}
fn obj_is(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let (x, y) = (arg(a, 0), arg(a, 1));
    Ok(Value::Bool(match (&x, &y) {
        (Value::Num(p), Value::Num(q)) => (p.is_nan() && q.is_nan()) || (p == q && p.is_sign_negative() == q.is_sign_negative()),
        _ => strict_eq(&x, &y),
    }))
}
fn obj_has_own(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = it.key(&arg(a, 0))?;
    Ok(Value::Bool(match &t {
        Value::Obj(o) => {
            let b = o.borrow();
            b.props.get(&k).is_some() || matches!(&b.kind, Kind::Array(v) if array_index(&k).is_some_and(|i| i < v.len()))
        }
        Value::Str(s) => array_index(&k).is_some_and(|i| i < utf16_len(s)),
        _ => false,
    }))
}
fn obj_to_string(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::str(match &t {
        Value::Undef => "[object Undefined]",
        Value::Null => "[object Null]",
        Value::Obj(o) => match o.borrow().kind {
            Kind::Array(_) => "[object Array]",
            Kind::Func(_) => "[object Function]",
            Kind::Error => "[object Error]",
            Kind::Date(_) => "[object Date]",
            Kind::Regex(_) => "[object RegExp]",
            _ => "[object Object]",
        },
        Value::Str(_) => "[object String]",
        Value::Num(_) => "[object Number]",
        Value::Bool(_) => "[object Boolean]",
    }))
}
fn obj_is_proto_of(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let (Value::Obj(p), Value::Obj(o)) = (&t, &arg(a, 0)) else { return Ok(Value::Bool(false)) };
    let mut cur = o.borrow().proto.clone();
    while let Some(c) = cur {
        if Rc::ptr_eq(&c, p) {
            return Ok(Value::Bool(true));
        }
        cur = c.borrow().proto.clone();
    }
    Ok(Value::Bool(false))
}

// --------------------------------------------------------------- Function

fn function_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut parts = Vec::new();
    for v in a {
        parts.push(it.to_str(v)?.to_string());
    }
    let body = parts.pop().unwrap_or_default();
    let params: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
    let def = parser::parse_function(&params, &body).map_err(|e| make_error_value("SyntaxError", &e))?;
    let env = it.genv.clone();
    Ok(it.make_closure(&def, &env))
}
fn fn_call(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    it.call(&t, arg(a, 0), a.get(1..).unwrap_or(&[]))
}
fn fn_apply(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let args = match arg(a, 1) {
        Value::Undef | Value::Null => Vec::new(),
        v => it.iterate(&v)?,
    };
    it.call(&t, arg(a, 0), &args)
}
fn fn_bind(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Obj(it.new_obj(it.p.function.clone(), Kind::Func(Func::Bound(t, arg(a, 0), a.get(1..).unwrap_or(&[]).to_vec())))))
}
fn fn_to_string(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let name = match &t {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Func(Func::Native(_, n)) => n.to_string(),
            Kind::Func(Func::Closure { def, .. }) => def.name.as_deref().unwrap_or("").to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    Ok(Value::str(&format!("function {name}() {{ [native code] }}")))
}

// ------------------------------------------------------------------ Array

fn array_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if a.len() == 1 {
        if let Value::Num(n) = a[0] {
            if n < 0.0 || n.fract() != 0.0 || n > 1e8 {
                return Err(make_error_value("RangeError", "Invalid array length"));
            }
            return Ok(it.array(vec![Value::Undef; n as usize]));
        }
    }
    Ok(it.array(a.to_vec()))
}
fn array_is_array(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(matches!(&arg(a, 0), Value::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)))))
}
fn array_from(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let src = arg(a, 0);
    let items = if src.is_nullish() { Vec::new() } else { it.iterate(&src)? };
    let f = arg(a, 1);
    let out = if Interp::is_callable(&f) {
        let mut v = Vec::with_capacity(items.len());
        for (i, x) in items.into_iter().enumerate() {
            v.push(it.call(&f, Value::Undef, &[x, Value::Num(i as f64)])?);
        }
        v
    } else {
        items
    };
    Ok(it.array(out))
}
fn array_of(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(it.array(a.to_vec()))
}
fn arr_push(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = with_arr(&t, |v| {
        v.extend_from_slice(a);
        v.len()
    })?;
    ok_num(n as f64)
}
fn arr_pop(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(with_arr(&t, |v| v.pop())?.unwrap_or_default())
}
fn arr_shift(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(with_arr(&t, |v| if v.is_empty() { None } else { Some(v.remove(0)) })?.unwrap_or_default())
}
fn arr_unshift(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = with_arr(&t, |v| {
        v.splice(0..0, a.iter().cloned());
        v.len()
    })?;
    ok_num(n as f64)
}
fn arr_slice(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arr_vec(&t)?;
    let len = v.len();
    let s = rel_index(if a.is_empty() || matches!(a[0], Value::Undef) { 0.0 } else { num(it, a, 0)? }, len);
    let e = if a.len() < 2 || matches!(a[1], Value::Undef) { len } else { rel_index(num(it, a, 1)?, len) };
    Ok(it.array(if s < e { v[s..e].to_vec() } else { Vec::new() }))
}
fn arr_splice(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let len = arr_vec(&t)?.len();
    let s = rel_index(num(it, a, 0)?, len);
    let del = if a.len() < 2 { len - s } else { (num(it, a, 1)?.max(0.0) as usize).min(len - s) };
    let ins: Vec<Value> = a.get(2..).unwrap_or(&[]).to_vec();
    let removed = with_arr(&t, |v| v.splice(s..s + del, ins).collect::<Vec<_>>())?;
    Ok(it.array(removed))
}
fn arr_concat(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut v = arr_vec(&t)?;
    for x in a {
        match x {
            Value::Obj(o) if matches!(o.borrow().kind, Kind::Array(_)) => v.extend(arr_vec(x)?),
            _ => v.push(x.clone()),
        }
    }
    Ok(it.array(v))
}
fn arr_join(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arr_vec(&t)?;
    let sep = match arg(a, 0) {
        Value::Undef => Rc::from(","),
        s => it.to_str(&s)?,
    };
    let mut out = String::new();
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        if !x.is_nullish() {
            out.push_str(&it.to_str(x)?);
        }
    }
    Ok(Value::Str(out.into()))
}
fn arr_reverse(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    with_arr(&t, |v| v.reverse())?;
    Ok(t)
}
fn arr_index_of(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let x = arg(a, 0);
    ok_num(arr_vec(&t)?.iter().position(|v| strict_eq(v, &x)).map_or(-1.0, |i| i as f64))
}
fn arr_last_index_of(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let x = arg(a, 0);
    ok_num(arr_vec(&t)?.iter().rposition(|v| strict_eq(v, &x)).map_or(-1.0, |i| i as f64))
}
fn arr_includes(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let x = arg(a, 0);
    Ok(Value::Bool(arr_vec(&t)?.iter().any(|v| strict_eq(v, &x) || matches!((v, &x), (Value::Num(p), Value::Num(q)) if p.is_nan() && q.is_nan()))))
}
fn each(it: &mut Interp, t: &Value, a: &[Value], mut f: impl FnMut(&mut Interp, usize, &Value, Value) -> R<bool>) -> R<()> {
    let v = arr_vec(t)?;
    let cb = arg(a, 0);
    if !Interp::is_callable(&cb) {
        return Err(type_error("callback is not a function"));
    }
    let this = arg(a, 1);
    for (i, x) in v.iter().enumerate() {
        let r = it.call(&cb, this.clone(), &[x.clone(), Value::Num(i as f64), t.clone()])?;
        if !f(it, i, x, r)? {
            break;
        }
    }
    Ok(())
}
fn arr_find(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut found = Value::Undef;
    each(it, &t, a, |_, _, x, r| {
        if r.truthy() {
            found = x.clone();
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(found)
}
fn arr_find_index(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut found = -1.0;
    each(it, &t, a, |_, i, _, r| {
        if r.truthy() {
            found = i as f64;
            return Ok(false);
        }
        Ok(true)
    })?;
    ok_num(found)
}
fn arr_find_last(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let rev = it.array(arr_vec(&t)?.into_iter().rev().collect());
    arr_find(it, rev, a)
}
fn arr_find_last_index(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let len = arr_vec(&t)?.len() as f64;
    let rev = it.array(arr_vec(&t)?.into_iter().rev().collect());
    let i = arr_find_index(it, rev, a)?;
    Ok(match i {
        Value::Num(n) if n >= 0.0 => Value::Num(len - 1.0 - n),
        v => v,
    })
}
fn arr_filter(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut out = Vec::new();
    each(it, &t, a, |_, _, x, r| {
        if r.truthy() {
            out.push(x.clone());
        }
        Ok(true)
    })?;
    Ok(it.array(out))
}
fn arr_map(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut out = Vec::new();
    each(it, &t, a, |_, _, _, r| {
        out.push(r);
        Ok(true)
    })?;
    Ok(it.array(out))
}
fn arr_for_each(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    each(it, &t, a, |_, _, _, _| Ok(true))?;
    Ok(Value::Undef)
}
fn arr_some(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut any = false;
    each(it, &t, a, |_, _, _, r| {
        any = r.truthy();
        Ok(!any)
    })?;
    Ok(Value::Bool(any))
}
fn arr_every(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut all = true;
    each(it, &t, a, |_, _, _, r| {
        all = r.truthy();
        Ok(all)
    })?;
    Ok(Value::Bool(all))
}
fn arr_reduce(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arr_vec(&t)?;
    let f = arg(a, 0);
    let mut iter = v.into_iter().enumerate();
    let mut acc = if a.len() >= 2 {
        a[1].clone()
    } else {
        match iter.next() {
            Some((_, x)) => x,
            None => return Err(type_error("Reduce of empty array with no initial value")),
        }
    };
    for (i, x) in iter {
        acc = it.call(&f, Value::Undef, &[acc, x, Value::Num(i as f64), t.clone()])?;
    }
    Ok(acc)
}
fn arr_reduce_right(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let rev = it.array(arr_vec(&t)?.into_iter().rev().collect());
    arr_reduce(it, rev, a)
}
fn sort_values(it: &mut Interp, v: &mut [Value], f: &Value) -> R<()> {
    // Merge sort with a fallible comparator.
    let mut err = None;
    let mut keyed: Vec<(Value, Option<Rc<str>>)> = Vec::with_capacity(v.len());
    for x in v.iter() {
        let k = if Interp::is_callable(f) || x.is_nullish() { None } else { Some(it.to_str(x)?) };
        keyed.push((x.clone(), k));
    }
    let mut cmp = |a: &(Value, Option<Rc<str>>), b: &(Value, Option<Rc<str>>)| -> std::cmp::Ordering {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        if matches!(a.0, Value::Undef) || matches!(b.0, Value::Undef) {
            return matches!(a.0, Value::Undef).cmp(&matches!(b.0, Value::Undef));
        }
        if Interp::is_callable(f) {
            match it.call(f, Value::Undef, &[a.0.clone(), b.0.clone()]).and_then(|r| it.to_num(&r)) {
                Ok(n) => n.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal),
                Err(e) => {
                    err = Some(e);
                    std::cmp::Ordering::Equal
                }
            }
        } else {
            a.1.as_deref().unwrap_or("").encode_utf16().cmp(b.1.as_deref().unwrap_or("").encode_utf16())
        }
    };
    let mut sorted = keyed.clone();
    merge_sort(&mut sorted, &mut cmp);
    if let Some(e) = err {
        return Err(e);
    }
    for (slot, (x, _)) in v.iter_mut().zip(sorted) {
        *slot = x;
    }
    Ok(())
}
fn merge_sort<T: Clone>(v: &mut [T], cmp: &mut impl FnMut(&T, &T) -> std::cmp::Ordering) {
    if v.len() <= 1 {
        return;
    }
    if v.len() <= 8 {
        for i in 1..v.len() {
            let mut j = i;
            while j > 0 && cmp(&v[j - 1], &v[j]).is_gt() {
                v.swap(j - 1, j);
                j -= 1;
            }
        }
        return;
    }
    let mid = v.len() / 2;
    merge_sort(&mut v[..mid], cmp);
    merge_sort(&mut v[mid..], cmp);
    let mut out = Vec::with_capacity(v.len());
    let (mut i, mut j) = (0, mid);
    while i < mid && j < v.len() {
        if cmp(&v[j], &v[i]).is_lt() {
            out.push(v[j].clone());
            j += 1;
        } else {
            out.push(v[i].clone());
            i += 1;
        }
    }
    out.extend_from_slice(&v[i..mid]);
    out.extend_from_slice(&v[j..]);
    v.clone_from_slice(&out);
}
fn arr_sort(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut v = arr_vec(&t)?;
    sort_values(it, &mut v, &arg(a, 0))?;
    with_arr(&t, |x| *x = v)?;
    Ok(t)
}
fn arr_to_sorted(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut v = arr_vec(&t)?;
    sort_values(it, &mut v, &arg(a, 0))?;
    Ok(it.array(v))
}
fn arr_to_reversed(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(it.array(arr_vec(&t)?.into_iter().rev().collect()))
}
fn flatten(v: &[Value], depth: i64, out: &mut Vec<Value>) {
    for x in v {
        if depth > 0 {
            if let Value::Obj(o) = x {
                if let Kind::Array(inner) = &o.borrow().kind {
                    flatten(inner, depth - 1, out);
                    continue;
                }
            }
        }
        out.push(x.clone());
    }
}
fn arr_flat(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let d = if matches!(arg(a, 0), Value::Undef) { 1 } else { it.to_int(&arg(a, 0))? };
    let mut out = Vec::new();
    flatten(&arr_vec(&t)?, d, &mut out);
    Ok(it.array(out))
}
fn arr_flat_map(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let m = arr_map(it, t, a)?;
    let mut out = Vec::new();
    flatten(&arr_vec(&m)?, 1, &mut out);
    Ok(it.array(out))
}
fn arr_fill(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let len = arr_vec(&t)?.len();
    let s = if a.len() > 1 { rel_index(num(it, a, 1)?, len) } else { 0 };
    let e = if a.len() > 2 { rel_index(num(it, a, 2)?, len) } else { len };
    let x = arg(a, 0);
    with_arr(&t, |v| {
        for slot in v.iter_mut().take(e).skip(s) {
            *slot = x.clone();
        }
    })?;
    Ok(t)
}
fn arr_keys(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = arr_vec(&t)?.len();
    Ok(it.array((0..n).map(|i| Value::Num(i as f64)).collect()))
}
fn arr_entries(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = arr_vec(&t)?;
    let out = v.into_iter().enumerate().map(|(i, x)| it.array(vec![Value::Num(i as f64), x])).collect();
    Ok(it.array(out))
}
fn arr_at(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arr_vec(&t)?;
    let i = num(it, a, 0)?;
    let i = if i < 0.0 { v.len() as f64 + i } else { i };
    Ok(if i >= 0.0 { v.get(i as usize).cloned().unwrap_or_default() } else { Value::Undef })
}

// ----------------------------------------------------------------- String

fn string_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = if a.is_empty() { Rc::from("") } else { it.to_str(&a[0])? };
    if it.constructing {
        return Ok(Value::Obj(it.new_obj(it.p.string.clone(), Kind::Boxed(Value::Str(s)))));
    }
    Ok(Value::Str(s))
}
fn str_from_char_code(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut units = Vec::new();
    for v in a {
        let n = it.to_num(v)? as u32;
        if n > 0xFFFF {
            if let Some(c) = char::from_u32(n) {
                let mut buf = [0u16; 2];
                units.extend_from_slice(c.encode_utf16(&mut buf));
            }
        } else {
            units.push(n as u16);
        }
    }
    Ok(Value::str(&String::from_utf16_lossy(&units)))
}
fn str_raw(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let strs = it.get(&arg(a, 0), "raw")?;
    let parts = it.iterate(&strs)?;
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        out.push_str(&it.to_str(p)?);
        if i + 1 < parts.len() {
            if let Some(v) = a.get(i + 1) {
                out.push_str(&it.to_str(v)?);
            }
        }
    }
    Ok(Value::str(&out))
}
fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
fn from_units(u: &[u16]) -> Value {
    Value::str(&String::from_utf16_lossy(u))
}
fn str_char_at(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let i = num(it, a, 0)?;
    let i = if i.is_nan() { 0.0 } else { i };
    Ok(if i >= 0.0 { utf16_at(&s, i as usize).map(|c| Value::Str(c.into())).unwrap_or(Value::str("")) } else { Value::str("") })
}
fn str_char_code_at(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let i = num(it, a, 0)?;
    let i = if i.is_nan() { 0 } else { i as i64 };
    let u = units(&s);
    ok_num(if i >= 0 { u.get(i as usize).map_or(f64::NAN, |c| *c as f64) } else { f64::NAN })
}
fn str_index_of(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = units(&this_str(it, &t)?);
    let n = units(&it.to_str(&arg(a, 0))?);
    let from = if a.len() > 1 { num(it, a, 1)?.max(0.0) as usize } else { 0 };
    ok_num(find_units(&s, &n, from).map_or(-1.0, |i| i as f64))
}
fn find_units(s: &[u16], n: &[u16], from: usize) -> Option<usize> {
    if n.is_empty() {
        return Some(from.min(s.len()));
    }
    if n.len() > s.len() {
        return None;
    }
    (from..=s.len() - n.len()).find(|&i| &s[i..i + n.len()] == n)
}
fn str_last_index_of(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = units(&this_str(it, &t)?);
    let n = units(&it.to_str(&arg(a, 0))?);
    if n.len() > s.len() {
        return ok_num(-1.0);
    }
    ok_num((0..=s.len() - n.len()).rev().find(|&i| s[i..i + n.len()] == n[..]).map_or(-1.0, |i| i as f64))
}
fn str_includes(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let n = it.to_str(&arg(a, 0))?;
    Ok(Value::Bool(s.contains(&*n)))
}
fn str_starts_with(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = units(&this_str(it, &t)?);
    let n = units(&it.to_str(&arg(a, 0))?);
    let p = if a.len() > 1 { num(it, a, 1)?.max(0.0) as usize } else { 0 };
    Ok(Value::Bool(s.get(p..).is_some_and(|r| r.starts_with(&n))))
}
fn str_ends_with(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = units(&this_str(it, &t)?);
    let n = units(&it.to_str(&arg(a, 0))?);
    let e = if a.len() > 1 && !matches!(a[1], Value::Undef) { (num(it, a, 1)?.max(0.0) as usize).min(s.len()) } else { s.len() };
    Ok(Value::Bool(s[..e].ends_with(&n)))
}
fn str_slice(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let u = units(&s);
    let len = u.len();
    let st = rel_index(if a.is_empty() { 0.0 } else { num(it, a, 0)? }, len);
    let e = if a.len() < 2 || matches!(a[1], Value::Undef) { len } else { rel_index(num(it, a, 1)?, len) };
    Ok(if st < e { from_units(&u[st..e]) } else { Value::str("") })
}
fn str_substring(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let u = units(&this_str(it, &t)?);
    let len = u.len();
    let clamp = |x: f64| if x.is_nan() { 0 } else { (x.max(0.0) as usize).min(len) };
    let s = clamp(if a.is_empty() { 0.0 } else { num(it, a, 0)? });
    let e = if a.len() < 2 || matches!(a[1], Value::Undef) { len } else { clamp(num(it, a, 1)?) };
    let (s, e) = (s.min(e), s.max(e));
    Ok(from_units(&u[s..e]))
}
fn str_substr(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let u = units(&this_str(it, &t)?);
    let s = rel_index(num(it, a, 0)?, u.len());
    let n = if a.len() < 2 || matches!(a[1], Value::Undef) { u.len() - s } else { (num(it, a, 1)?.max(0.0) as usize).min(u.len() - s) };
    Ok(from_units(&u[s..s + n]))
}
fn str_upper(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::str(&this_str(it, &t)?.to_uppercase()))
}
fn str_lower(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::str(&this_str(it, &t)?.to_lowercase()))
}
fn str_trim(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::str(this_str(it, &t)?.trim()))
}
fn str_trim_start(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::str(this_str(it, &t)?.trim_start()))
}
fn str_trim_end(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::str(this_str(it, &t)?.trim_end()))
}
fn regex_of(v: &Value) -> Option<Rc<Regex>> {
    match v {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Regex(r) => Some(r.clone()),
            _ => None,
        },
        _ => None,
    }
}
fn str_split(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let limit = if a.len() > 1 && !matches!(a[1], Value::Undef) { num(it, a, 1)? as usize } else { usize::MAX };
    let sep = arg(a, 0);
    let mut out: Vec<Value> = Vec::new();
    if let Some(re) = regex_of(&sep) {
        let chars: Vec<char> = s.chars().collect();
        let mut last = 0;
        let mut i = 0;
        while i < chars.len() {
            match re.exec_at(&chars, i) {
                Some(caps) => {
                    let (ms, me) = caps[0].unwrap();
                    if me == ms && ms >= chars.len() {
                        break;
                    }
                    if me == ms && ms == last {
                        i += 1;
                        continue;
                    }
                    out.push(Value::str(&chars[last..ms].iter().collect::<String>()));
                    for c in caps.iter().skip(1) {
                        out.push(c.map(|(x, y)| Value::str(&chars[x..y].iter().collect::<String>())).unwrap_or_default());
                    }
                    last = me;
                    i = if me > ms { me } else { me + 1 };
                }
                None => break,
            }
        }
        out.push(Value::str(&chars[last.min(chars.len())..].iter().collect::<String>()));
    } else if matches!(sep, Value::Undef) {
        out.push(Value::Str(s));
    } else {
        let sep = it.to_str(&sep)?;
        if sep.is_empty() {
            out = s.chars().map(|c| Value::str(&c.to_string())).collect();
        } else {
            out = s.split(&*sep).map(Value::str).collect();
        }
    }
    out.truncate(limit);
    Ok(it.array(out))
}
/// Expand `$&`, `$1`.. in a replacement template.
fn expand_replacement(tpl: &str, chars: &[char], caps: &[Option<(usize, usize)>]) -> String {
    let t: Vec<char> = tpl.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < t.len() {
        if t[i] == '$' && i + 1 < t.len() {
            match t[i + 1] {
                '$' => {
                    out.push('$');
                    i += 2;
                    continue;
                }
                '&' => {
                    if let Some((s, e)) = caps[0] {
                        out.extend(&chars[s..e]);
                    }
                    i += 2;
                    continue;
                }
                d if d.is_ascii_digit() => {
                    let mut n = d.to_digit(10).unwrap() as usize;
                    let mut adv = 2;
                    if i + 2 < t.len() && t[i + 2].is_ascii_digit() {
                        let n2 = n * 10 + t[i + 2].to_digit(10).unwrap() as usize;
                        if n2 < caps.len() {
                            n = n2;
                            adv = 3;
                        }
                    }
                    if n > 0 && n < caps.len() {
                        if let Some((s, e)) = caps[n] {
                            out.extend(&chars[s..e]);
                        }
                        i += adv;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}
fn replace_impl(it: &mut Interp, t: Value, a: &[Value], all: bool) -> R<Value> {
    let s = this_str(it, &t)?;
    let pat = arg(a, 0);
    let rep = arg(a, 1);
    let chars: Vec<char> = s.chars().collect();
    let mut matches: Vec<Vec<Option<(usize, usize)>>> = Vec::new();
    if let Some(re) = regex_of(&pat) {
        let global = re.global || all;
        let mut i = 0;
        while i <= chars.len() {
            let Some(caps) = re.exec_at(&chars, i) else { break };
            let (ms, me) = caps[0].unwrap();
            matches.push(caps);
            if !global {
                break;
            }
            i = if me > ms { me } else { me + 1 };
        }
    } else {
        let p: Vec<char> = it.to_str(&pat)?.chars().collect();
        let mut i = 0;
        while i + p.len() <= chars.len() {
            if chars[i..i + p.len()] == p[..] {
                matches.push(vec![Some((i, i + p.len()))]);
                if !all {
                    break;
                }
                i += p.len().max(1);
            } else {
                i += 1;
            }
        }
    }
    let mut out = String::new();
    let mut last = 0;
    for caps in &matches {
        let (ms, me) = caps[0].unwrap();
        out.extend(&chars[last..ms]);
        if Interp::is_callable(&rep) {
            let mut args: Vec<Value> = caps.iter().map(|c| c.map(|(x, y)| Value::str(&chars[x..y].iter().collect::<String>())).unwrap_or_default()).collect();
            args.push(Value::Num(ms as f64));
            args.push(Value::Str(s.clone()));
            let r = it.call(&rep, Value::Undef, &args)?;
            out.push_str(&it.to_str(&r)?);
        } else {
            let tpl = it.to_str(&rep)?;
            out.push_str(&expand_replacement(&tpl, &chars, caps));
        }
        last = me;
    }
    out.extend(&chars[last..]);
    Ok(Value::str(&out))
}
fn str_replace(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    replace_impl(it, t, a, false)
}
fn str_replace_all(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    replace_impl(it, t, a, true)
}
fn match_array(it: &mut Interp, chars: &[char], caps: &[Option<(usize, usize)>], input: &Rc<str>) -> Value {
    let items = caps.iter().map(|c| c.map(|(x, y)| Value::str(&chars[x..y].iter().collect::<String>())).unwrap_or_default()).collect();
    let arr = it.array(items);
    if let Value::Obj(o) = &arr {
        Interp::set_prop(o, "index", Value::Num(caps[0].map_or(0, |c| c.0) as f64));
        Interp::set_prop(o, "input", Value::Str(input.clone()));
        Interp::set_prop(o, "groups", Value::Undef);
    }
    arr
}
fn str_match(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let pat = arg(a, 0);
    let re = match regex_of(&pat) {
        Some(r) => r,
        None => {
            let p = it.to_str(&pat)?;
            Rc::new(Regex::new(&p, "").map_err(|e| make_error_value("SyntaxError", &e))?)
        }
    };
    let chars: Vec<char> = s.chars().collect();
    if re.global {
        let mut out = Vec::new();
        let mut i = 0;
        while i <= chars.len() {
            let Some(caps) = re.exec_at(&chars, i) else { break };
            let (ms, me) = caps[0].unwrap();
            out.push(Value::str(&chars[ms..me].iter().collect::<String>()));
            i = if me > ms { me } else { me + 1 };
        }
        return Ok(if out.is_empty() { Value::Null } else { it.array(out) });
    }
    Ok(match re.exec_at(&chars, 0) {
        Some(caps) => match_array(it, &chars, &caps, &s),
        None => Value::Null,
    })
}
fn str_match_all(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let re = regex_of(&arg(a, 0)).ok_or_else(|| type_error("matchAll requires a RegExp"))?;
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i <= chars.len() {
        let Some(caps) = re.exec_at(&chars, i) else { break };
        let (ms, me) = caps[0].unwrap();
        out.push(match_array(it, &chars, &caps, &s));
        i = if me > ms { me } else { me + 1 };
    }
    Ok(it.array(out))
}
fn str_search(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let re = match regex_of(&arg(a, 0)) {
        Some(r) => r,
        None => Rc::new(Regex::new(&it.to_str(&arg(a, 0))?, "").map_err(|e| make_error_value("SyntaxError", &e))?),
    };
    let chars: Vec<char> = s.chars().collect();
    ok_num(re.exec_at(&chars, 0).map_or(-1.0, |c| c[0].unwrap().0 as f64))
}
fn pad(it: &mut Interp, t: Value, a: &[Value], start: bool) -> R<Value> {
    let s = this_str(it, &t)?;
    let target = num(it, a, 0)? as usize;
    let fill: Vec<u16> = if a.len() > 1 && !matches!(a[1], Value::Undef) { units(&it.to_str(&a[1])?) } else { vec![b' ' as u16] };
    let u = units(&s);
    if u.len() >= target || fill.is_empty() {
        return Ok(Value::Str(s));
    }
    let need = target - u.len();
    let padding: Vec<u16> = fill.iter().cycle().take(need).copied().collect();
    let mut out = Vec::with_capacity(target);
    if start {
        out.extend(padding);
        out.extend(u);
    } else {
        out.extend(u);
        out.extend(padding);
    }
    Ok(from_units(&out))
}
fn str_pad_start(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    pad(it, t, a, true)
}
fn str_pad_end(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    pad(it, t, a, false)
}
fn str_repeat(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let n = num(it, a, 0)?;
    if n < 0.0 || n.is_infinite() {
        return Err(make_error_value("RangeError", "Invalid count value"));
    }
    Ok(Value::str(&s.repeat((n as usize).min(10_000_000 / s.len().max(1)))))
}
fn str_concat(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mut s = this_str(it, &t)?.to_string();
    for v in a {
        s.push_str(&it.to_str(v)?);
    }
    Ok(Value::str(&s))
}
fn str_at(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let u = units(&this_str(it, &t)?);
    let i = num(it, a, 0)?;
    let i = if i < 0.0 { u.len() as f64 + i } else { i };
    Ok(if i >= 0.0 && (i as usize) < u.len() { from_units(&u[i as usize..i as usize + 1]) } else { Value::Undef })
}
fn str_locale_compare(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let s = this_str(it, &t)?;
    let o = it.to_str(&arg(a, 0))?;
    ok_num(match s.to_lowercase().cmp(&o.to_lowercase()).then(s.cmp(&o)) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    })
}

// ----------------------------------------------------------------- Number

fn this_num(it: &mut Interp, t: &Value) -> R<f64> {
    if let Value::Obj(o) = t {
        if let Kind::Boxed(v) = &o.borrow().kind {
            let v = v.clone();
            return it.to_num(&v);
        }
    }
    it.to_num(t)
}
fn number_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let n = if a.is_empty() { 0.0 } else { it.to_num(&a[0])? };
    if it.constructing {
        return Ok(Value::Obj(it.new_obj(it.p.number.clone(), Kind::Boxed(Value::Num(n)))));
    }
    ok_num(n)
}
fn boolean_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(arg(a, 0).truthy()).clone()).map(|v| if it.constructing { Value::Obj(it.new_obj(it.p.boolean.clone(), Kind::Boxed(v))) } else { v })
}
fn bool_to_string(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = match &t {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Boxed(v) => v.clone(),
            _ => t.clone(),
        },
        _ => t.clone(),
    };
    Ok(Value::Str(it.to_str(&v)?))
}
fn num_value_of(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(match &t {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Boxed(v) => v.clone(),
            _ => t.clone(),
        },
        _ => t,
    })
}
fn num_is_integer(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0)))
}
fn num_is_finite(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_finite())))
}
fn num_is_nan(_: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(matches!(arg(a, 0), Value::Num(n) if n.is_nan())))
}
fn global_is_nan(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(num(it, a, 0)?.is_nan()))
}
fn global_is_finite(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(num(it, a, 0)?.is_finite()))
}
fn parse_float(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let t = s.trim_start();
    if t.starts_with("Infinity") || t.starts_with("+Infinity") {
        return ok_num(f64::INFINITY);
    }
    if t.starts_with("-Infinity") {
        return ok_num(f64::NEG_INFINITY);
    }
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == ds || (i == ds + 1 && b[ds] == b'.') {
        return ok_num(f64::NAN);
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    ok_num(t[..i].parse().unwrap_or(f64::NAN))
}
fn parse_int(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let mut radix = if a.len() > 1 { it.to_int(&a[1])? as u32 } else { 0 };
    let mut t = s.trim();
    let neg = t.starts_with('-');
    if neg || t.starts_with('+') {
        t = &t[1..];
    }
    if (radix == 0 || radix == 16) && (t.starts_with("0x") || t.starts_with("0X")) {
        t = &t[2..];
        radix = 16;
    }
    if radix == 0 {
        radix = 10;
    }
    if !(2..=36).contains(&radix) {
        return ok_num(f64::NAN);
    }
    let digits: String = t.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return ok_num(f64::NAN);
    }
    let mut v = 0f64;
    for c in digits.chars() {
        v = v * radix as f64 + c.to_digit(radix).unwrap() as f64;
    }
    ok_num(if neg { -v } else { v })
}
fn num_to_fixed(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_num(it, &t)?;
    let d = (num(it, a, 0)?.max(0.0) as usize).min(100);
    if !n.is_finite() || n.abs() >= 1e21 {
        return Ok(Value::str(&num_to_str(n)));
    }
    Ok(Value::str(&format!("{n:.d$}")))
}
fn num_to_precision(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_num(it, &t)?;
    if a.is_empty() || matches!(a[0], Value::Undef) {
        return Ok(Value::str(&num_to_str(n)));
    }
    let p = (num(it, a, 0)? as i32).clamp(1, 100);
    if n == 0.0 || !n.is_finite() {
        return Ok(Value::str(&format!("{:.*}", (p - 1) as usize, n)));
    }
    let e = n.abs().log10().floor() as i32;
    let d = (p - 1 - e).max(0) as usize;
    Ok(Value::str(&format!("{n:.d$}")))
}
fn num_to_string(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_num(it, &t)?;
    let radix = if a.is_empty() || matches!(a[0], Value::Undef) { 10 } else { it.to_int(&a[0])? as u32 };
    if radix == 10 || !n.is_finite() {
        return Ok(Value::str(&num_to_str(n)));
    }
    if !(2..=36).contains(&radix) {
        return Err(make_error_value("RangeError", "toString() radix must be between 2 and 36"));
    }
    let neg = n < 0.0;
    let mut i = n.abs().trunc() as u128;
    let mut frac = n.abs().fract();
    let mut digits = Vec::new();
    loop {
        digits.push(std::char::from_digit((i % radix as u128) as u32, radix).unwrap());
        i /= radix as u128;
        if i == 0 {
            break;
        }
    }
    let mut s: String = digits.iter().rev().collect();
    if frac > 0.0 {
        s.push('.');
        for _ in 0..20 {
            frac *= radix as f64;
            let d = frac.trunc() as u32;
            s.push(std::char::from_digit(d, radix).unwrap());
            frac -= d as f64;
            if frac <= 0.0 {
                break;
            }
        }
    }
    Ok(Value::str(&if neg { format!("-{s}") } else { s }))
}
fn num_to_locale(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = this_num(it, &t)?;
    let mut max_frac = 3usize;
    let opts = arg(a, 1);
    if let Value::Obj(_) = &opts {
        if let Value::Num(m) = it.get(&opts, "maximumFractionDigits")? {
            max_frac = m as usize;
        }
    }
    let s = format!("{:.*}", max_frac, n.abs());
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(a, b)| (a, b));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Ok(Value::str(&if n < 0.0 { format!("-{out}") } else { out }))
}

// ------------------------------------------------------------------- Math

macro_rules! math1 {
    ($($name:ident => $f:expr),* $(,)?) => {
        $(fn $name(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
            let x = num(it, a, 0)?;
            let f: fn(f64) -> f64 = $f;
            ok_num(f(x))
        })*
    };
}
math1! {
    m_abs => f64::abs, m_floor => f64::floor, m_ceil => f64::ceil,
    m_round => |x| (x + 0.5).floor(), m_trunc => f64::trunc,
    m_sign => |x| if x.is_nan() { x } else if x > 0.0 { 1.0 } else if x < 0.0 { -1.0 } else { x },
    m_sqrt => f64::sqrt, m_cbrt => f64::cbrt, m_sin => f64::sin, m_cos => f64::cos, m_tan => f64::tan,
    m_asin => f64::asin, m_acos => f64::acos, m_atan => f64::atan, m_exp => f64::exp, m_log => f64::ln,
    m_log2 => f64::log2, m_log10 => f64::log10, m_log1p => f64::ln_1p, m_fround => |x| x as f32 as f64,
    m_clz32 => |x| (to_i32(x) as u32).leading_zeros() as f64,
}
fn m_atan2(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    ok_num(num(it, a, 0)?.atan2(num(it, a, 1)?))
}
fn m_pow(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    ok_num(num(it, a, 0)?.powf(num(it, a, 1)?))
}
fn m_imul(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    ok_num(to_i32(num(it, a, 0)?).wrapping_mul(to_i32(num(it, a, 1)?)) as f64)
}
fn m_min(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut m = f64::INFINITY;
    for v in a {
        let x = it.to_num(v)?;
        if x.is_nan() {
            return ok_num(f64::NAN);
        }
        m = m.min(x);
    }
    ok_num(m)
}
fn m_max(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut m = f64::NEG_INFINITY;
    for v in a {
        let x = it.to_num(v)?;
        if x.is_nan() {
            return ok_num(f64::NAN);
        }
        m = m.max(x);
    }
    ok_num(m)
}
fn m_hypot(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut s = 0.0;
    for v in a {
        let x = it.to_num(v)?;
        s += x * x;
    }
    ok_num(s.sqrt())
}
fn m_random(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    it.rng ^= it.rng << 13;
    it.rng ^= it.rng >> 7;
    it.rng ^= it.rng << 17;
    ok_num((it.rng >> 11) as f64 / (1u64 << 53) as f64)
}

// ------------------------------------------------------------------- JSON

fn json_quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_ser(it: &mut Interp, v: &Value, indent: &str, cur: &str, out: &mut String, depth: usize) -> R<bool> {
    if depth > 200 {
        return Err(type_error("Converting circular structure to JSON"));
    }
    let mut v = v.clone();
    if let Value::Obj(_) = &v {
        let tj = it.get(&v, "toJSON")?;
        if Interp::is_callable(&tj) {
            v = it.call(&tj, v.clone(), &[])?;
        }
    }
    match &v {
        Value::Undef => return Ok(false),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => out.push_str(&if n.is_finite() { num_to_str(*n) } else { "null".into() }),
        Value::Str(s) => json_quote(s, out),
        Value::Obj(o) => {
            if matches!(o.borrow().kind, Kind::Func(_)) {
                return Ok(false);
            }
            if let Kind::Boxed(b) = &o.borrow().kind {
                let b = b.clone();
                return json_ser(it, &b, indent, cur, out, depth + 1);
            }
            let inner = format!("{cur}{indent}");
            let nl = if indent.is_empty() { "" } else { "\n" };
            let colon = if indent.is_empty() { ":" } else { ": " };
            let arr = if let Kind::Array(a) = &o.borrow().kind { Some(a.clone()) } else { None };
            if let Some(a) = arr {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(nl);
                    out.push_str(&inner);
                    if !json_ser(it, x, indent, &inner, out, depth + 1)? {
                        out.push_str("null");
                    }
                }
                if !a.is_empty() {
                    out.push_str(nl);
                    out.push_str(cur);
                }
                out.push(']');
            } else {
                out.push('{');
                let mut first = true;
                for k in it.own_keys(&v) {
                    let pv = it.get(&v, &k)?;
                    let mut tmp = String::new();
                    if !json_ser(it, &pv, indent, &inner, &mut tmp, depth + 1)? {
                        continue;
                    }
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    out.push_str(nl);
                    out.push_str(&inner);
                    json_quote(&k, out);
                    out.push_str(colon);
                    out.push_str(&tmp);
                }
                if !first {
                    out.push_str(nl);
                    out.push_str(cur);
                }
                out.push('}');
            }
        }
    }
    Ok(true)
}

fn json_stringify(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let indent = match arg(a, 2) {
        Value::Num(n) => " ".repeat((n as usize).min(10)),
        Value::Str(s) => s.chars().take(10).collect(),
        _ => String::new(),
    };
    let mut out = String::new();
    Ok(if json_ser(it, &arg(a, 0), &indent, "", &mut out, 0)? { Value::str(&out) } else { Value::Undef })
}

struct JsonP<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonP<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn val(&mut self, it: &mut Interp, depth: usize) -> Option<Value> {
        if depth > 500 {
            return None;
        }
        self.ws();
        let c = *self.s.get(self.i)?;
        Some(match c {
            b'{' => {
                self.i += 1;
                let o = it.plain();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Some(Value::Obj(o));
                }
                loop {
                    self.ws();
                    let Value::Str(k) = self.val(it, depth + 1)? else { return None };
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return None;
                    }
                    self.i += 1;
                    let v = self.val(it, depth + 1)?;
                    o.borrow_mut().props.insert(k, Prop::data(v));
                    self.ws();
                    match self.s.get(self.i)? {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
                Value::Obj(o)
            }
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Some(it.array(v));
                }
                loop {
                    v.push(self.val(it, depth + 1)?);
                    self.ws();
                    match self.s.get(self.i)? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
                it.array(v)
            }
            b'"' => {
                self.i += 1;
                let mut out: Vec<u16> = Vec::new();
                loop {
                    let c = *self.s.get(self.i)?;
                    self.i += 1;
                    match c {
                        b'"' => break,
                        b'\\' => {
                            let e = *self.s.get(self.i)?;
                            self.i += 1;
                            match e {
                                b'n' => out.push(10),
                                b't' => out.push(9),
                                b'r' => out.push(13),
                                b'b' => out.push(8),
                                b'f' => out.push(12),
                                b'u' => {
                                    let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
                                    out.push(u16::from_str_radix(h, 16).ok()?);
                                    self.i += 4;
                                }
                                other => out.push(other as u16),
                            }
                        }
                        _ => {
                            // Copy a UTF-8 run.
                            let start = self.i - 1;
                            let mut end = self.i;
                            while end < self.s.len() && self.s[end] != b'"' && self.s[end] != b'\\' {
                                end += 1;
                            }
                            out.extend(std::str::from_utf8(&self.s[start..end]).ok()?.encode_utf16());
                            self.i = end;
                        }
                    }
                }
                Value::str(&String::from_utf16_lossy(&out))
            }
            b't' if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                Value::Bool(true)
            }
            b'f' if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                Value::Bool(false)
            }
            b'n' if self.s[self.i..].starts_with(b"null") => {
                self.i += 4;
                Value::Null
            }
            _ => {
                let start = self.i;
                while self.i < self.s.len() && matches!(self.s[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    self.i += 1;
                }
                Value::Num(std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()?)
            }
        })
    }
}

fn json_parse(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let mut p = JsonP { s: s.as_bytes(), i: 0 };
    let v = p.val(it, 0);
    p.ws();
    match v {
        Some(v) if p.i == p.s.len() => Ok(v),
        _ => Err(make_error_value("SyntaxError", "Unexpected token in JSON")),
    }
}

fn structured_clone(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = json_stringify(it, Value::Undef, &a[..a.len().min(1)])?;
    if matches!(s, Value::Undef) {
        return Ok(Value::Undef);
    }
    json_parse(it, Value::Undef, &[s])
}

// ------------------------------------------------------------------ Error

fn error_named(it: &mut Interp, _: Value, a: &[Value], name: &str) -> R<Value> {
    let msg = match arg(a, 0) {
        Value::Undef => Rc::from(""),
        v => it.to_str(&v)?,
    };
    let e = make_error_value(name, &msg);
    if let (Value::Obj(o), Value::Obj(_)) = (&e, &arg(a, 1)) {
        let cause = it.get(&arg(a, 1), "cause")?;
        Interp::set_hidden(o, "cause", cause);
    }
    Ok(e)
}
fn error_ctor(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    error_named(it, t, a, "Error")
}
fn error_to_string(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let n = it.get(&t, "name")?;
    let m = it.get(&t, "message")?;
    let n = it.to_str(&n)?;
    let m = it.to_str(&m)?;
    Ok(Value::str(&if m.is_empty() { n.to_string() } else { format!("{n}: {m}") }))
}

// ----------------------------------------------------------------- RegExp

pub fn make_regex(it: &mut Interp, body: &str, flags: &str) -> R<Value> {
    let re = Regex::new(body, flags).map_err(|e| make_error_value("SyntaxError", &format!("Invalid regular expression: /{body}/: {e}")))?;
    let o = it.new_obj(it.p.regexp.clone(), Kind::Regex(Rc::new(re)));
    Interp::set_hidden(&o, "lastIndex", Value::Num(0.0));
    Interp::set_hidden(&o, "source", Value::str(body));
    Interp::set_hidden(&o, "flags", Value::str(flags));
    Interp::set_hidden(&o, "global", Value::Bool(flags.contains('g')));
    Interp::set_hidden(&o, "ignoreCase", Value::Bool(flags.contains('i')));
    Interp::set_hidden(&o, "multiline", Value::Bool(flags.contains('m')));
    Ok(Value::Obj(o))
}
fn regexp_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let p = arg(a, 0);
    let src = if let Some(r) = regex_of(&p) { Rc::from(r.source.as_str()) } else if matches!(p, Value::Undef) { Rc::from("(?:)") } else { it.to_str(&p)? };
    let flags = match arg(a, 1) {
        Value::Undef => regex_of(&p).map(|r| Rc::from(r.flags.as_str())).unwrap_or_else(|| Rc::from("")),
        f => it.to_str(&f)?,
    };
    make_regex(it, &src, &flags)
}
fn re_exec(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let re = regex_of(&t).ok_or_else(|| type_error("not a RegExp"))?;
    let s = it.to_str(&arg(a, 0))?;
    let chars: Vec<char> = s.chars().collect();
    let sticky = re.global || re.sticky;
    let start = if sticky { it.get(&t, "lastIndex")?.clone() } else { Value::Num(0.0) };
    let start = it.to_num(&start)? as usize;
    if start > chars.len() {
        it.put(&t, "lastIndex", Value::Num(0.0))?;
        return Ok(Value::Null);
    }
    match re.exec_at(&chars, start) {
        Some(caps) if !re.sticky || caps[0].unwrap().0 == start => {
            if sticky {
                let (ms, me) = caps[0].unwrap();
                it.put(&t, "lastIndex", Value::Num(if me == ms { me + 1 } else { me } as f64))?;
            }
            Ok(match_array(it, &chars, &caps, &s))
        }
        _ => {
            if sticky {
                it.put(&t, "lastIndex", Value::Num(0.0))?;
            }
            Ok(Value::Null)
        }
    }
}
fn re_test(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    Ok(Value::Bool(!matches!(re_exec(it, t, a)?, Value::Null)))
}
fn re_to_string(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let s = it.get(&t, "source")?;
    let f = it.get(&t, "flags")?;
    Ok(Value::str(&format!("/{}/{}", it.to_str(&s)?, it.to_str(&f)?)))
}

// ------------------------------------------------------------------- Date

pub fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}
/// Days since 1970-01-01 → (year, month 0-11, day 1-31).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m - 1, d)
}
fn days_from_civil(y: i64, m0: i64, d: i64) -> i64 {
    let (y, m) = (y + m0.div_euclid(12), m0.rem_euclid(12) + 1);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
fn parse_date(s: &str) -> f64 {
    // ISO 8601: YYYY-MM-DD[THH:MM[:SS[.sss]]][Z|±HH:MM]
    let b = s.trim();
    let num = |r: std::ops::Range<usize>| -> Option<i64> { b.get(r)?.parse().ok() };
    let (Some(y), Some(mo), Some(d)) = (num(0..4), num(5..7), num(8..10)) else { return f64::NAN };
    let mut ms = days_from_civil(y, mo - 1, d) as f64 * 86_400_000.0;
    if b.len() >= 16 {
        let (h, mi) = (num(11..13).unwrap_or(0), num(14..16).unwrap_or(0));
        let sec: f64 = b.get(17..).map(|r| r.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect::<String>()).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        ms += (h * 3600 + mi * 60) as f64 * 1000.0 + sec * 1000.0;
        if let Some(p) = b[16..].find(['+', '-']) {
            let off = &b[16 + p..];
            let sign = if off.starts_with('-') { -1.0 } else { 1.0 };
            let oh: f64 = off.get(1..3).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let om: f64 = off.get(4..6).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            ms -= sign * (oh * 60.0 + om) * 60_000.0;
        }
    }
    ms
}
fn this_date(t: &Value) -> R<f64> {
    match t {
        Value::Obj(o) => match o.borrow().kind {
            Kind::Date(d) => Ok(d),
            _ => Err(type_error("this is not a Date object")),
        },
        _ => Err(type_error("this is not a Date object")),
    }
}
fn date_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    if !it.constructing {
        return date_to_string(it, Value::Obj(it.new_obj(None, Kind::Date(now_ms()))), &[]);
    }
    let t = match a.len() {
        0 => now_ms(),
        1 => match &a[0] {
            Value::Str(s) => parse_date(s),
            Value::Obj(o) if matches!(o.borrow().kind, Kind::Date(_)) => this_date(&a[0])?,
            v => it.to_num(v)?,
        },
        _ => {
            let mut p = [0f64; 7];
            p[2] = 1.0;
            for (i, v) in a.iter().take(7).enumerate() {
                p[i] = it.to_num(v)?;
            }
            days_from_civil(p[0] as i64, p[1] as i64, p[2] as i64) as f64 * 86_400_000.0 + p[3] * 3_600_000.0 + p[4] * 60_000.0 + p[5] * 1000.0 + p[6]
        }
    };
    Ok(Value::Obj(it.new_obj(it.p.date.clone(), Kind::Date(t))))
}
fn date_now(_: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    ok_num(now_ms().floor())
}
fn date_parse(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    ok_num(parse_date(&s))
}
fn date_utc(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut p = [0f64; 7];
    p[2] = 1.0;
    for (i, v) in a.iter().take(7).enumerate() {
        p[i] = it.to_num(v)?;
    }
    ok_num(days_from_civil(p[0] as i64, p[1] as i64, p[2] as i64) as f64 * 86_400_000.0 + p[3] * 3_600_000.0 + p[4] * 60_000.0 + p[5] * 1000.0 + p[6])
}
fn date_get_time(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    ok_num(this_date(&t)?)
}
fn date_set_time(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let n = num(it, a, 0)?;
    if let Value::Obj(o) = &t {
        o.borrow_mut().kind = Kind::Date(n);
    }
    ok_num(n)
}
fn date_fields(ms: f64) -> [i64; 8] {
    let ms_i = ms as i64;
    let days = ms_i.div_euclid(86_400_000);
    let rem = ms_i.rem_euclid(86_400_000);
    let (y, m, d) = civil(days);
    [y, m, d, (days + 4).rem_euclid(7), rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000]
}
fn date_part(_: &mut Interp, t: &Value, i: usize) -> R<Value> {
    let ms = this_date(t)?;
    if ms.is_nan() {
        return ok_num(f64::NAN);
    }
    ok_num(date_fields(ms)[i] as f64)
}
fn date_iso(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let ms = this_date(&t)?;
    if ms.is_nan() {
        return Err(make_error_value("RangeError", "Invalid time value"));
    }
    let f = date_fields(ms);
    Ok(Value::str(&format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", f[0], f[1] + 1, f[2], f[4], f[5], f[6], f[7])))
}
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
fn date_to_string(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let ms = this_date(&t)?;
    if ms.is_nan() {
        return Ok(Value::str("Invalid Date"));
    }
    let f = date_fields(ms);
    Ok(Value::str(&format!("{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)", DAYS[f[3] as usize], MONTHS[f[1] as usize], f[2], f[0], f[4], f[5], f[6])))
}
fn date_locale_date(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let f = date_fields(this_date(&t)?);
    Ok(Value::str(&format!("{}/{}/{}", f[1] + 1, f[2], f[0])))
}
fn date_locale_time(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let f = date_fields(this_date(&t)?);
    let (h, ampm) = (if f[4] % 12 == 0 { 12 } else { f[4] % 12 }, if f[4] < 12 { "AM" } else { "PM" });
    Ok(Value::str(&format!("{h}:{:02}:{:02} {ampm}", f[5], f[6])))
}

// --------------------------------------------------------------- Map / Set

fn coll_items(it: &mut Interp, a: &[Value]) -> R<Vec<Value>> {
    match arg(a, 0) {
        Value::Undef | Value::Null => Ok(Vec::new()),
        v => it.iterate(&v),
    }
}
fn map_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut m: Vec<(Value, Value)> = Vec::new();
    for e in coll_items(it, a)? {
        let k = it.get(&e, "0")?;
        let v = it.get(&e, "1")?;
        match m.iter_mut().find(|(x, _)| same_value_zero(x, &k)) {
            Some(slot) => slot.1 = v,
            None => m.push((k, v)),
        }
    }
    Ok(Value::Obj(it.new_obj(it.p.map.clone(), Kind::Map(m))))
}
fn same_value_zero(a: &Value, b: &Value) -> bool {
    strict_eq(a, b) || matches!((a, b), (Value::Num(x), Value::Num(y)) if x.is_nan() && y.is_nan())
}
fn with_map<T>(t: &Value, f: impl FnOnce(&mut Vec<(Value, Value)>) -> T) -> R<T> {
    match t {
        Value::Obj(o) => match &mut o.borrow_mut().kind {
            Kind::Map(m) => Ok(f(m)),
            _ => Err(type_error("not a Map")),
        },
        _ => Err(type_error("not a Map")),
    }
}
fn with_set<T>(t: &Value, f: impl FnOnce(&mut Vec<Value>) -> T) -> R<T> {
    match t {
        Value::Obj(o) => match &mut o.borrow_mut().kind {
            Kind::Set(m) => Ok(f(m)),
            _ => Err(type_error("not a Set")),
        },
        _ => Err(type_error("not a Set")),
    }
}
fn map_get(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = arg(a, 0);
    with_map(&t, |m| m.iter().find(|(x, _)| same_value_zero(x, &k)).map(|(_, v)| v.clone()).unwrap_or_default())
}
fn map_set(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let (k, v) = (arg(a, 0), arg(a, 1));
    with_map(&t, |m| match m.iter_mut().find(|(x, _)| same_value_zero(x, &k)) {
        Some(slot) => slot.1 = v,
        None => m.push((k, v)),
    })?;
    Ok(t)
}
fn map_has(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = arg(a, 0);
    Ok(Value::Bool(with_map(&t, |m| m.iter().any(|(x, _)| same_value_zero(x, &k)))?))
}
fn map_delete(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let k = arg(a, 0);
    Ok(Value::Bool(with_map(&t, |m| {
        let n = m.len();
        m.retain(|(x, _)| !same_value_zero(x, &k));
        n != m.len()
    })?))
}
fn coll_clear(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    if let Value::Obj(o) = &t {
        match &mut o.borrow_mut().kind {
            Kind::Map(m) => m.clear(),
            Kind::Set(s) => s.clear(),
            _ => {}
        }
    }
    Ok(Value::Undef)
}
fn coll_size(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(match &t {
        Value::Obj(o) => match &o.borrow().kind {
            Kind::Map(m) => Value::Num(m.len() as f64),
            Kind::Set(s) => Value::Num(s.len() as f64),
            _ => Value::Undef,
        },
        _ => Value::Undef,
    })
}
fn map_for_each(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let items = with_map(&t, |m| m.clone())?;
    let f = arg(a, 0);
    for (k, v) in items {
        it.call(&f, arg(a, 1), &[v, k, t.clone()])?;
    }
    Ok(Value::Undef)
}
fn map_keys(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = with_map(&t, |m| m.iter().map(|(k, _)| k.clone()).collect())?;
    Ok(it.array(v))
}
fn map_values(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = with_map(&t, |m| m.iter().map(|(_, v)| v.clone()).collect())?;
    Ok(it.array(v))
}
fn map_entries(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = with_map(&t, |m| m.clone())?;
    let out = v.into_iter().map(|(k, v)| it.array(vec![k, v])).collect();
    Ok(it.array(out))
}
fn set_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let mut s: Vec<Value> = Vec::new();
    for v in coll_items(it, a)? {
        if !s.iter().any(|x| same_value_zero(x, &v)) {
            s.push(v);
        }
    }
    Ok(Value::Obj(it.new_obj(it.p.set.clone(), Kind::Set(s))))
}
fn set_add(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arg(a, 0);
    with_set(&t, |s| {
        if !s.iter().any(|x| same_value_zero(x, &v)) {
            s.push(v)
        }
    })?;
    Ok(t)
}
fn set_has(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arg(a, 0);
    Ok(Value::Bool(with_set(&t, |s| s.iter().any(|x| same_value_zero(x, &v)))?))
}
fn set_delete(_: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let v = arg(a, 0);
    Ok(Value::Bool(with_set(&t, |s| {
        let n = s.len();
        s.retain(|x| !same_value_zero(x, &v));
        n != s.len()
    })?))
}
fn set_for_each(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let items = with_set(&t, |s| s.clone())?;
    let f = arg(a, 0);
    for v in items {
        it.call(&f, arg(a, 1), &[v.clone(), v, t.clone()])?;
    }
    Ok(Value::Undef)
}
fn set_values(it: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    let v = with_set(&t, |s| s.clone())?;
    Ok(it.array(v))
}

// ---------------------------------------------------------------- Promise

pub fn new_promise(it: &Interp) -> ObjRef {
    it.new_obj(it.p.promise.clone(), Kind::Promise(PromiseState::Pending(Vec::new())))
}

fn is_promise(v: &Value) -> Option<ObjRef> {
    match v {
        Value::Obj(o) if matches!(o.borrow().kind, Kind::Promise(_)) => Some(o.clone()),
        _ => None,
    }
}

/// Settle `p`; schedules its reactions as microtasks.
pub fn settle(it: &mut Interp, p: &ObjRef, ok: bool, v: Value) {
    let reactions = {
        let mut b = p.borrow_mut();
        let Kind::Promise(st) = &mut b.kind else { return };
        if !matches!(st, PromiseState::Pending(_)) {
            return;
        }
        let new = if ok { PromiseState::Fulfilled(v.clone()) } else { PromiseState::Rejected(v.clone()) };
        match std::mem::replace(st, new) {
            PromiseState::Pending(r) => r,
            _ => Vec::new(),
        }
    };
    for (on_ok, on_err, child) in reactions {
        schedule_reaction(it, if ok { on_ok } else { on_err }, child, ok, v.clone());
    }
}

/// React to `p` settling by calling `ok(value)` or `err(reason)`.
pub fn subscribe(it: &mut Interp, p: &ObjRef, ok: Value, err: Value) {
    let _ = promise_then(it, Value::Obj(p.clone()), &[ok, err]);
}

/// Resolve `p` with `v`, adopting the state of promises.
pub fn resolve_with(it: &mut Interp, p: &ObjRef, v: Value) {
    if let Some(inner) = is_promise(&v) {
        if Rc::ptr_eq(&inner, p) {
            settle(it, p, false, type_error("Chaining cycle detected"));
            return;
        }
        let resolve = bound_native(it, promise_resolve_fn, Value::Obj(p.clone()));
        let reject = bound_native(it, promise_reject_fn, Value::Obj(p.clone()));
        add_reaction(it, &inner, resolve, reject, None);
        return;
    }
    if let Value::Obj(_) = &v {
        if let Ok(then) = it.get(&v, "then") {
            if Interp::is_callable(&then) {
                let resolve = bound_native(it, promise_resolve_fn, Value::Obj(p.clone()));
                let reject = bound_native(it, promise_reject_fn, Value::Obj(p.clone()));
                if let Err(e) = it.call(&then, v.clone(), &[resolve, reject]) {
                    settle(it, p, false, e);
                }
                return;
            }
        }
    }
    settle(it, p, true, v);
}

pub(crate) fn bound_native(it: &Interp, f: NativeFn, this: Value) -> Value {
    let nf = it.native(f, "");
    Value::Obj(it.new_obj(it.p.function.clone(), Kind::Func(Func::Bound(nf, this, Vec::new()))))
}

fn promise_resolve_fn(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    if let Value::Obj(p) = &t {
        resolve_with(it, p, arg(a, 0));
    }
    Ok(Value::Undef)
}
fn promise_reject_fn(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    if let Value::Obj(p) = &t {
        settle(it, p, false, arg(a, 0));
    }
    Ok(Value::Undef)
}

/// Microtask body: run a handler, then resolve/reject the child promise.
fn reaction_job(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let handler = it.get(&t, "h")?;
    let child = it.get(&t, "c")?;
    let ok = it.get(&t, "ok")?.truthy();
    let v = arg(a, 0);
    let child = child.obj().cloned();
    if Interp::is_callable(&handler) {
        match it.call(&handler, Value::Undef, &[v]) {
            Ok(r) => {
                if let Some(c) = &child {
                    resolve_with(it, c, r);
                }
            }
            Err(e) => {
                if let Some(c) = &child {
                    settle(it, c, false, e);
                }
            }
        }
    } else if let Some(c) = &child {
        if ok {
            resolve_with(it, c, v);
        } else {
            settle(it, c, false, v);
        }
    }
    Ok(Value::Undef)
}

fn schedule_reaction(it: &mut Interp, handler: Value, child: ObjRef, ok: bool, v: Value) {
    let data = it.plain();
    Interp::set_prop(&data, "h", handler);
    Interp::set_prop(&data, "c", Value::Obj(child));
    Interp::set_prop(&data, "ok", Value::Bool(ok));
    let job = bound_native(it, reaction_job, Value::Obj(data));
    it.micro.push_back((job, vec![v]));
}

fn add_reaction(it: &mut Interp, p: &ObjRef, on_ok: Value, on_err: Value, child: Option<ObjRef>) -> ObjRef {
    let child = child.unwrap_or_else(|| new_promise(it));
    let state = {
        let mut b = p.borrow_mut();
        match &mut b.kind {
            Kind::Promise(PromiseState::Pending(r)) => {
                r.push((on_ok.clone(), on_err.clone(), child.clone()));
                None
            }
            Kind::Promise(PromiseState::Fulfilled(v)) => Some((true, v.clone())),
            Kind::Promise(PromiseState::Rejected(v)) => Some((false, v.clone())),
            _ => None,
        }
    };
    if let Some((ok, v)) = state {
        schedule_reaction(it, if ok { on_ok } else { on_err }, child.clone(), ok, v);
    }
    child
}

pub fn promise_resolved(it: &mut Interp, v: Value) -> Value {
    if is_promise(&v).is_some() {
        return v;
    }
    let p = new_promise(it);
    resolve_with(it, &p, v);
    Value::Obj(p)
}

pub fn promise_rejected(it: &mut Interp, e: Value) -> Value {
    let p = new_promise(it);
    settle(it, &p, false, e);
    Value::Obj(p)
}

/// `await`: suspend the running async body until the promise settles; outside
/// a coroutine, drain microtasks until it does.
pub fn await_value(it: &mut Interp, v: Value) -> R<Value> {
    if let Some(r) = it.co_await(v.clone()) {
        return r;
    }
    let Some(p) = is_promise(&v) else {
        if let Value::Obj(_) = &v {
            let then = it.get(&v, "then")?;
            if Interp::is_callable(&then) {
                let wrapped = promise_resolved(it, Value::Undef);
                if let Value::Obj(w) = &wrapped {
                    resolve_with(it, w, v);
                }
                return await_value(it, wrapped);
            }
        }
        return Ok(v);
    };
    for _ in 0..100 {
        let state = match &p.borrow().kind {
            Kind::Promise(PromiseState::Fulfilled(v)) => Some(Ok(v.clone())),
            Kind::Promise(PromiseState::Rejected(e)) => Some(Err(e.clone())),
            _ => None,
        };
        if let Some(r) = state {
            return r;
        }
        if it.micro.is_empty() {
            break;
        }
        // Run pending reactions (bounded).
        let batch: Vec<_> = it.micro.drain(..).collect();
        for (f, args) in batch {
            let _ = it.call(&f, Value::Undef, &args);
        }
    }
    Ok(Value::Undef)
}

fn promise_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let exec = arg(a, 0);
    if !Interp::is_callable(&exec) {
        return Err(type_error("Promise resolver is not a function"));
    }
    let p = new_promise(it);
    let resolve = bound_native(it, promise_resolve_fn, Value::Obj(p.clone()));
    let reject = bound_native(it, promise_reject_fn, Value::Obj(p.clone()));
    if let Err(e) = it.call(&exec, Value::Undef, &[resolve, reject]) {
        settle(it, &p, false, e);
    }
    Ok(Value::Obj(p))
}
fn promise_then(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let p = is_promise(&t).ok_or_else(|| type_error("not a promise"))?;
    Ok(Value::Obj(add_reaction(it, &p, arg(a, 0), arg(a, 1), None)))
}
fn promise_catch(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    promise_then(it, t, &[Value::Undef, arg(a, 0)])
}
fn finally_job(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let f = it.get(&t, "f")?;
    let rethrow = it.get(&t, "e")?.truthy();
    if Interp::is_callable(&f) {
        it.call(&f, Value::Undef, &[])?;
    }
    if rethrow { Err(arg(a, 0)) } else { Ok(arg(a, 0)) }
}
fn promise_finally(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let mk = |it: &mut Interp, e: bool| {
        let d = it.plain();
        Interp::set_prop(&d, "f", arg(a, 0));
        Interp::set_prop(&d, "e", Value::Bool(e));
        bound_native(it, finally_job, Value::Obj(d))
    };
    let (ok, err) = (mk(it, false), mk(it, true));
    promise_then(it, t, &[ok, err])
}
fn promise_static_resolve(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(promise_resolved(it, arg(a, 0)))
}
fn promise_static_reject(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(promise_rejected(it, arg(a, 0)))
}
fn all_job(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    // t = { p: aggregate promise, vals: array, i: index, left: {n} , settled: bool }
    let vals = it.get(&t, "vals")?;
    let i = it.get(&t, "i")?;
    let settled_mode = it.get(&t, "s")?.truthy();
    let ok = it.get(&t, "ok")?.truthy();
    let v = if settled_mode {
        let o = it.plain();
        Interp::set_prop(&o, "status", Value::str(if ok { "fulfilled" } else { "rejected" }));
        Interp::set_prop(&o, if ok { "value" } else { "reason" }, arg(a, 0));
        Value::Obj(o)
    } else {
        arg(a, 0)
    };
    it.put_v(&vals, &i, v)?;
    let left = it.get(&t, "left")?;
    let n = it.get(&left, "n")?;
    let n = it.to_num(&n)? - 1.0;
    it.put(&left, "n", Value::Num(n))?;
    if n == 0.0 {
        if let Some(p) = it.get(&t, "p")?.obj() {
            resolve_with(it, p, vals);
        }
    }
    Ok(Value::Undef)
}
fn promise_all_impl(it: &mut Interp, a: &[Value], settled: bool) -> R<Value> {
    let items = it.iterate(&arg(a, 0))?;
    let agg = new_promise(it);
    let vals = it.array(vec![Value::Undef; items.len()]);
    if items.is_empty() {
        resolve_with(it, &agg, vals);
        return Ok(Value::Obj(agg));
    }
    let left = it.plain();
    Interp::set_prop(&left, "n", Value::Num(items.len() as f64));
    for (i, x) in items.into_iter().enumerate() {
        let p = promise_resolved(it, x);
        let p = is_promise(&p).unwrap();
        let mk = |it: &mut Interp, ok: bool| {
            let d = it.plain();
            Interp::set_prop(&d, "p", Value::Obj(agg.clone()));
            Interp::set_prop(&d, "vals", vals.clone());
            Interp::set_prop(&d, "i", Value::Num(i as f64));
            Interp::set_prop(&d, "left", Value::Obj(left.clone()));
            Interp::set_prop(&d, "s", Value::Bool(settled));
            Interp::set_prop(&d, "ok", Value::Bool(ok));
            bound_native(it, all_job, Value::Obj(d))
        };
        let on_ok = mk(it, true);
        let on_err = if settled { mk(it, false) } else { bound_native(it, promise_reject_fn, Value::Obj(agg.clone())) };
        add_reaction(it, &p, on_ok, on_err, None);
    }
    Ok(Value::Obj(agg))
}
fn promise_all(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    promise_all_impl(it, a, false)
}
fn promise_all_settled(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    promise_all_impl(it, a, true)
}
fn promise_race(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let items = it.iterate(&arg(a, 0))?;
    let agg = new_promise(it);
    for x in items {
        let p = promise_resolved(it, x);
        let p = is_promise(&p).unwrap();
        let ok = bound_native(it, promise_resolve_fn, Value::Obj(agg.clone()));
        let err = bound_native(it, promise_reject_fn, Value::Obj(agg.clone()));
        add_reaction(it, &p, ok, err, None);
    }
    Ok(Value::Obj(agg))
}

// -------------------------------------------------------------- Generator

fn gen_obj(t: &Value) -> R<ObjRef> {
    match t {
        Value::Obj(o) if matches!(o.borrow().kind, Kind::Generator(_)) => Ok(o.clone()),
        _ => Err(type_error("not a generator")),
    }
}
pub fn iter_result(it: &mut Interp, value: Value, done: bool) -> Value {
    let r = it.plain();
    Interp::set_prop(&r, "value", value);
    Interp::set_prop(&r, "done", Value::Bool(done));
    Value::Obj(r)
}
fn gen_finish(it: &mut Interp, o: &ObjRef, r: R<Value>) -> R<Value> {
    let is_async = matches!(&o.borrow().kind, Kind::Generator(g) if g.is_async);
    if !is_async {
        return r;
    }
    Ok(match r {
        Ok(v) => promise_resolved(it, v),
        Err(e) => promise_rejected(it, e),
    })
}
/// next / throw / return on a coroutine-backed generator.
fn coro_gen_call(it: &mut Interp, o: &ObjRef, req: crate::js::generator::Req, v: Value) -> Option<R<Value>> {
    use crate::js::coro::Resume;
    use crate::js::generator::Req;
    let (coro, is_async) = match &o.borrow().kind {
        Kind::Generator(g) => (g.coro, g.is_async),
        _ => return None,
    };
    if !coro {
        return None;
    }
    if is_async {
        return Some(Ok(it.agen_request(o, req, v)));
    }
    let r = match req {
        Req::Next => Resume::Next(v),
        Req::Throw => Resume::Throw(v),
        Req::Return => Resume::Return(v),
    };
    Some(it.gen_resume(o, r).map(|(v, done)| iter_result(it, v, done)))
}
fn gen_next(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let o = gen_obj(&t)?;
    if let Some(r) = coro_gen_call(it, &o, crate::js::generator::Req::Next, arg(a, 0)) {
        return r;
    }
    it.gen_run(&o);
    let step = match &mut o.borrow_mut().kind {
        Kind::Generator(g) => match g.buf.pop_front() {
            Some(v) => Ok((v, false)),
            None => match g.err.take() {
                Some(e) => Err(e),
                None => Ok((g.ret.take().unwrap_or_default(), true)),
            },
        },
        _ => unreachable!(),
    };
    let r = step.map(|(v, done)| iter_result(it, v, done));
    gen_finish(it, &o, r)
}
fn gen_return(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let o = gen_obj(&t)?;
    if let Some(r) = coro_gen_call(it, &o, crate::js::generator::Req::Return, arg(a, 0)) {
        return r;
    }
    if let Kind::Generator(g) = &mut o.borrow_mut().kind {
        g.start = None;
        g.buf.clear();
        g.ret = None;
        g.err = None;
    }
    let r = Ok(iter_result(it, arg(a, 0), true));
    gen_finish(it, &o, r)
}
fn gen_throw(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let o = gen_obj(&t)?;
    if let Some(r) = coro_gen_call(it, &o, crate::js::generator::Req::Throw, arg(a, 0)) {
        return r;
    }
    if let Kind::Generator(g) = &mut o.borrow_mut().kind {
        g.start = None;
        g.buf.clear();
        g.ret = None;
    }
    gen_finish(it, &o, Err(arg(a, 0)))
}

// ----------------------------------------------------------------- Symbol

fn symbol_fn(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    it.symbols += 1;
    let desc = match arg(a, 0) {
        Value::Undef => Rc::from(""),
        v => it.to_str(&v)?,
    };
    Ok(Value::str(&format!("@@sym:{}:{desc}", it.symbols)))
}
fn symbol_for(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let k = it.to_str(&arg(a, 0))?;
    Ok(Value::str(&format!("@@sym:for:{k}")))
}

// ----------------------------------------------------------------- timers

fn add_timer(it: &mut Interp, a: &[Value], repeat: bool) -> R<Value> {
    let f = arg(a, 0);
    if !Interp::is_callable(&f) {
        // String timers: compile as code.
        if let Value::Str(s) = &f {
            let def = parser::parse_function(&[], s).map_err(|e| make_error_value("SyntaxError", &e))?;
            let env = it.genv.clone();
            let c = it.make_closure(&def, &env);
            return add_timer(it, &[&[c][..], a.get(1..).unwrap_or(&[])].concat(), repeat);
        }
        return Ok(Value::Num(0.0));
    }
    let delay = if a.len() > 1 { it.to_num(&a[1])?.max(0.0) } else { 0.0 };
    let delay = if delay.is_nan() { 0.0 } else { delay };
    let id = it.next_timer;
    it.next_timer += 1;
    let at = it.now + delay;
    it.timers.push(Timer { id, at, interval: repeat.then_some(delay), func: f, args: a.get(2..).unwrap_or(&[]).to_vec() });
    ok_num(id as f64)
}
fn set_timeout(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    add_timer(it, a, false)
}
fn set_interval(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    add_timer(it, a, true)
}
fn request_animation_frame(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let ts = Value::Num(it.now + 16.0);
    add_timer(it, &[arg(a, 0), Value::Num(16.0), ts], false)
}
fn clear_timer(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let id = it.to_num(&arg(a, 0))? as u32;
    it.timers.retain(|t| t.id != id);
    Ok(Value::Undef)
}
fn queue_microtask(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    it.micro.push_back((arg(a, 0), Vec::new()));
    Ok(Value::Undef)
}

// -------------------------------------------------------------------- URI

fn uri_encode(s: &str, keep: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || keep.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
fn encode_uri_component(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    Ok(Value::str(&uri_encode(&s, "-_.!~*'()")))
}
fn encode_uri(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    Ok(Value::str(&uri_encode(&s, "-_.!~*'();/?:@&=+$,#")))
}
fn decode_uri(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Ok(Value::str(&String::from_utf8_lossy(&out)))
}
const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
fn btoa(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let bytes: Vec<u8> = s.chars().map(|c| c as u32 as u8).collect();
    let mut out = String::new();
    for ch in bytes.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= ch.len() {
                out.push(B64[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    Ok(Value::str(&out))
}
fn atob(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let s = it.to_str(&arg(a, 0))?;
    let bytes = crate::image::data_url(&format!("data:;base64,{s}")).unwrap_or_default();
    Ok(Value::str(&bytes.iter().map(|&b| b as char).collect::<String>()))
}
