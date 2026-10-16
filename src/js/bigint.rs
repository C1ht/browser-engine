//! BigInt: arbitrary-precision integers, approximated with i128 (enough for
//! the 64-bit arithmetic real code uses them for; larger results throw a
//! RangeError instead of silently losing precision).

use super::builtins::arg;
use super::{type_error, Interp, NativeFn, Value, R};

pub fn too_big() -> Value {
    super::range_error("Maximum BigInt size exceeded")
}

/// A BigInt literal's digits (without the `n`): decimal, 0x, 0o or 0b.
pub fn parse_literal(s: &str) -> Option<i128> {
    let (radix, digits) = match s.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => (16, &s[2..]),
        Some("0o") => (8, &s[2..]),
        Some("0b") => (2, &s[2..]),
        _ => (10, s),
    };
    if digits.is_empty() {
        return None;
    }
    i128::from_str_radix(digits, radix).ok()
}

/// StringToBigInt: trimmed, optional sign (decimal only), "" is 0.
pub fn parse_str(s: &str) -> Option<i128> {
    let t = s.trim();
    if t.is_empty() {
        return Some(0);
    }
    if t.len() > 2 && t.as_bytes()[0] == b'0' && t.as_bytes()[1].is_ascii_alphabetic() {
        return parse_literal(t);
    }
    let (neg, d) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i128 = d.parse().ok()?;
    Some(if neg { -v } else { v })
}

fn to_bigint(it: &mut Interp, v: &Value) -> R<i128> {
    match v {
        Value::BigInt(n) => Ok(*n),
        Value::Bool(b) => Ok(*b as i128),
        Value::Str(s) => parse_str(s).ok_or_else(|| super::syntax_error(&format!("Cannot convert {s} to a BigInt"))),
        Value::Num(n) => {
            if n.fract() != 0.0 || !n.is_finite() {
                return Err(super::range_error(&format!("The number {} cannot be converted to a BigInt because it is not an integer", super::num_to_str(*n))));
            }
            Ok(*n as i128)
        }
        Value::Obj(_) => {
            let p = it.to_primitive(v, false)?;
            to_bigint(it, &p)
        }
        _ => Err(type_error(format!("Cannot convert {v:?} to a BigInt"))),
    }
}

/// Binary operators with a BigInt operand. `None`: not a BigInt operation
/// (string concatenation, equality, `in`, `instanceof`...), handled by the
/// caller.
pub fn binary(it: &mut Interp, op: &str, a: &Value, b: &Value) -> R<Option<Value>> {
    let arith = matches!(op, "-" | "*" | "/" | "%" | "**" | "&" | "|" | "^" | "<<" | ">>" | ">>>");
    if op == "+" {
        let pa = it.to_primitive(a, false)?;
        let pb = it.to_primitive(b, false)?;
        if matches!(pa, Value::Str(_)) || matches!(pb, Value::Str(_)) {
            return Ok(None);
        }
        return match (&pa, &pb) {
            (Value::BigInt(x), Value::BigInt(y)) => Ok(Some(Value::BigInt(x.checked_add(*y).ok_or_else(too_big)?))),
            _ => Err(type_error(format!("Cannot mix BigInt and other types, use explicit conversions ({pa:?} {op} {pb:?})"))),
        };
    }
    if matches!(op, "<" | ">" | "<=" | ">=") {
        let pa = it.to_primitive(a, false)?;
        let pb = it.to_primitive(b, false)?;
        let ord = match (&pa, &pb) {
            (Value::BigInt(x), Value::BigInt(y)) => Some(x.cmp(y)),
            (Value::BigInt(x), Value::Str(s)) => parse_str(s).map(|y| x.cmp(&y)),
            (Value::Str(s), Value::BigInt(y)) => parse_str(s).map(|x| x.cmp(y)),
            (Value::BigInt(x), o) => {
                let y = it.to_num(o)?;
                (*x as f64).partial_cmp(&y)
            }
            (o, Value::BigInt(y)) => {
                let x = it.to_num(o)?;
                x.partial_cmp(&(*y as f64))
            }
            _ => return Ok(None),
        };
        return Ok(Some(Value::Bool(match ord {
            None => false,
            Some(o) => match op {
                "<" => o.is_lt(),
                ">" => o.is_gt(),
                "<=" => o.is_le(),
                _ => o.is_ge(),
            },
        })));
    }
    if !arith {
        return Ok(None);
    }
    let pa = it.to_primitive(a, false)?;
    let pb = it.to_primitive(b, false)?;
    let (Value::BigInt(x), Value::BigInt(y)) = (pa, pb) else {
        return Err(type_error("Cannot mix BigInt and other types, use explicit conversions"));
    };
    let r = match op {
        "-" => x.checked_sub(y),
        "*" => x.checked_mul(y),
        "/" | "%" if y == 0 => return Err(super::range_error("Division by zero")),
        "/" => x.checked_div(y),
        "%" => x.checked_rem(y),
        "**" => {
            if y < 0 {
                return Err(super::range_error("Exponent must be non-negative"));
            }
            u32::try_from(y).ok().and_then(|e| x.checked_pow(e))
        }
        "&" => Some(x & y),
        "|" => Some(x | y),
        "^" => Some(x ^ y),
        "<<" | ">>" => {
            let left = (op == "<<") == (y >= 0);
            let sh = y.unsigned_abs();
            if left {
                if x == 0 {
                    Some(0)
                } else if sh >= 127 {
                    None
                } else {
                    x.checked_mul(1i128 << sh)
                }
            } else if sh >= 127 {
                Some(if x < 0 { -1 } else { 0 })
            } else {
                Some(x >> sh)
            }
        }
        ">>>" => return Err(type_error("BigInts have no unsigned right shift, use >> instead")),
        _ => return Ok(None),
    };
    Ok(Some(Value::BigInt(r.ok_or_else(too_big)?)))
}

fn as_uint_n(bits: u32, x: i128) -> R<i128> {
    if bits >= 128 {
        return if x >= 0 { Ok(x) } else { Err(too_big()) };
    }
    if bits == 0 {
        return Ok(0);
    }
    Ok(((x as u128) & ((1u128 << bits) - 1)) as i128)
}

fn as_int_n(bits: u32, x: i128) -> i128 {
    if bits >= 128 {
        return x;
    }
    if bits == 0 {
        return 0;
    }
    let shift = 128 - bits;
    (x << shift) >> shift
}

fn bigint_ctor(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    Ok(Value::BigInt(to_bigint(it, &arg(a, 0))?))
}

fn bits_arg(it: &mut Interp, v: &Value) -> R<u32> {
    let n = it.to_num(v)?;
    if !(0.0..=9007199254740991.0).contains(&n) {
        return Err(super::range_error("Invalid value: not (convertible to) a safe integer"));
    }
    Ok(n.min(u32::MAX as f64) as u32)
}

fn b_as_uint_n(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let bits = bits_arg(it, &arg(a, 0))?;
    let x = to_bigint(it, &arg(a, 1))?;
    Ok(Value::BigInt(as_uint_n(bits, x)?))
}

fn b_as_int_n(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let bits = bits_arg(it, &arg(a, 0))?;
    let x = to_bigint(it, &arg(a, 1))?;
    Ok(Value::BigInt(as_int_n(bits, x)))
}

fn this_bigint(t: &Value) -> R<i128> {
    match t {
        Value::BigInt(n) => Ok(*n),
        _ => Err(type_error("BigInt.prototype method called on incompatible receiver")),
    }
}

fn b_to_string(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    let x = this_bigint(&t)?;
    let radix = match arg(a, 0) {
        Value::Undef => 10,
        r => it.to_num(&r)? as u32,
    };
    if !(2..=36).contains(&radix) {
        return Err(super::range_error("toString() radix must be between 2 and 36"));
    }
    if radix == 10 {
        return Ok(Value::str(&x.to_string()));
    }
    let mut n = x.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        digits.push(std::char::from_digit((n % radix as u128) as u32, radix).unwrap_or('0'));
        n /= radix as u128;
        if n == 0 {
            break;
        }
    }
    if x < 0 {
        digits.push('-');
    }
    Ok(Value::str(&digits.iter().rev().collect::<String>()))
}

fn b_value_of(_: &mut Interp, t: Value, _: &[Value]) -> R<Value> {
    Ok(Value::BigInt(this_bigint(&t)?))
}

pub fn install(it: &mut Interp) {
    let proto = it.plain();
    it.p.bigint = Some(proto.clone());
    let c = it.new_obj(it.p.function.clone(), super::Kind::Func(super::Func::Native(bigint_ctor, "BigInt")));
    Interp::set_hidden(&c, "prototype", Value::Obj(proto.clone()));
    Interp::set_hidden(&c, "name", Value::str("BigInt"));
    Interp::set_hidden(&proto, "constructor", Value::Obj(c.clone()));
    Interp::set_hidden(&proto, "@@toStringTag", Value::str("BigInt"));
    Interp::set_hidden(&it.global, "BigInt", Value::Obj(c.clone()));
    it.def_method(&c, "asUintN", b_as_uint_n);
    it.def_method(&c, "asIntN", b_as_int_n);
    for (n, f) in [("toString", b_to_string as NativeFn), ("toLocaleString", b_to_string), ("valueOf", b_value_of)] {
        it.def_method(&proto, n, f);
    }
}
