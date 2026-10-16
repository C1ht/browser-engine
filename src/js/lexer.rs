//! JavaScript tokenizer. The whole source is tokenized up front; the parser
//! needs arbitrary lookahead to tell arrow functions from parenthesized
//! expressions.

use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Num(f64),
    BigInt(i128),
    Str(Rc<str>),
    /// Template literal: cooked string parts and the source of each `${}`.
    Template(Vec<Rc<str>>, Vec<String>),
    Ident(Rc<str>),
    Punct(&'static str),
    Regex(Rc<str>, Rc<str>),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    /// A line terminator precedes this token (for automatic semicolons).
    pub nl: bool,
}

const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=", "<=", ">=", "&&", "||", "??", "?.", "++", "--",
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "**", "<<", ">>", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "?", ":", "=", ".", "@",
];

/// Keywords after which a `/` starts a regular expression.
const REGEX_AFTER: &[&str] = &[
    "return", "typeof", "case", "in", "of", "do", "else", "new", "delete", "void", "throw", "instanceof", "yield", "await",
];

pub fn is_id_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$' || c == '#'
}

pub fn is_id_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '\u{200c}' || c == '\u{200d}'
}

pub struct LexError(pub String, pub usize);

pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut out: Vec<Token> = Vec::with_capacity(src.len() / 3);
    let mut nl = false;
    while i < n {
        let c = chars[i];
        // whitespace / newlines
        if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
            nl = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() || c == '\u{feff}' {
            i += 1;
            continue;
        }
        // comments
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    nl = true;
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '<' && chars[i..].starts_with(&['<', '!', '-', '-']) {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        let tok = if c.is_ascii_digit() || (c == '.' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            let (v, j) = number(&chars, i).ok_or(LexError("bad number".into(), i))?;
            if chars.get(j) == Some(&'n') {
                // BigInt literal
                let digits: String = chars[i..j].iter().filter(|&&c| c != '_').collect();
                i = j + 1;
                Tok::BigInt(crate::js::bigint::parse_literal(&digits).ok_or(LexError("bad BigInt".into(), i))?)
            } else {
                i = j;
                Tok::Num(v)
            }
        } else if c == '"' || c == '\'' {
            let (s, j) = string(&chars, i).ok_or(LexError("unterminated string".into(), i))?;
            i = j;
            Tok::Str(s.into())
        } else if c == '`' {
            let (q, e, j) = template(&chars, i).ok_or(LexError("unterminated template".into(), i))?;
            i = j;
            Tok::Template(q, e)
        } else if is_id_start(c) || c == '\\' {
            let mut s = String::new();
            while i < n && (is_id_part(chars[i]) || (i == start && chars[i] == '#') || chars[i] == '\\') {
                if chars[i] == '\\' && i + 1 < n && chars[i + 1] == 'u' {
                    let (ch, j) = unicode_escape(&chars, i + 2).ok_or(LexError("bad escape".into(), i))?;
                    s.push(ch);
                    i = j;
                } else {
                    s.push(chars[i]);
                    i += 1;
                }
            }
            Tok::Ident(s.into())
        } else if c == '/' && regex_allowed(out.last()) {
            let mut j = i + 1;
            let mut class = false;
            while j < n && (chars[j] != '/' || class) {
                match chars[j] {
                    '\\' => j += 1,
                    '[' => class = true,
                    ']' => class = false,
                    '\n' => return Err(LexError("unterminated regex".into(), i)),
                    _ => {}
                }
                j += 1;
            }
            let body: String = chars[i + 1..j.min(n)].iter().collect();
            j += 1;
            let fs = j;
            while j < n && is_id_part(chars[j]) {
                j += 1;
            }
            let flags: String = chars[fs.min(n)..j.min(n)].iter().collect();
            i = j;
            Tok::Regex(body.into(), flags.into())
        } else {
            let mut found = None;
            for p in PUNCTS {
                if p.chars().enumerate().all(|(k, pc)| chars.get(i + k) == Some(&pc)) {
                    // `a?.5:b` is a conditional, not optional chaining.
                    if *p == "?." && i + 2 < n && chars[i + 2].is_ascii_digit() {
                        continue;
                    }
                    found = Some(*p);
                    break;
                }
            }
            let p = found.ok_or(LexError(format!("unexpected character {c:?}"), i))?;
            i += p.chars().count();
            Tok::Punct(p)
        };
        out.push(Token { tok, nl });
        nl = false;
    }
    out.push(Token { tok: Tok::Eof, nl: true });
    Ok(out)
}

fn regex_allowed(prev: Option<&Token>) -> bool {
    match prev.map(|t| &t.tok) {
        None => true,
        Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}" | "++" | "--"),
        Some(Tok::Ident(s)) => REGEX_AFTER.contains(&&**s),
        _ => false,
    }
}

fn number(c: &[char], mut i: usize) -> Option<(f64, usize)> {
    let n = c.len();
    if c[i] == '0' && i + 1 < n && matches!(c[i + 1], 'x' | 'X' | 'o' | 'O' | 'b' | 'B') {
        let radix = match c[i + 1] {
            'x' | 'X' => 16,
            'o' | 'O' => 8,
            _ => 2,
        };
        i += 2;
        let mut v = 0f64;
        while i < n && (c[i].is_digit(radix) || c[i] == '_') {
            if c[i] != '_' {
                v = v * radix as f64 + c[i].to_digit(radix)? as f64;
            }
            i += 1;
        }
        return Some((v, i));
    }
    let mut s = String::new();
    while i < n && (c[i].is_ascii_digit() || c[i] == '_') {
        if c[i] != '_' {
            s.push(c[i]);
        }
        i += 1;
    }
    if i < n && c[i] == '.' {
        s.push('.');
        i += 1;
        while i < n && (c[i].is_ascii_digit() || c[i] == '_') {
            if c[i] != '_' {
                s.push(c[i]);
            }
            i += 1;
        }
    }
    if i < n && (c[i] == 'e' || c[i] == 'E') {
        let mut j = i + 1;
        let mut e = String::from("e");
        if j < n && (c[j] == '+' || c[j] == '-') {
            e.push(c[j]);
            j += 1;
        }
        if j < n && c[j].is_ascii_digit() {
            while j < n && c[j].is_ascii_digit() {
                e.push(c[j]);
                j += 1;
            }
            s.push_str(&e);
            i = j;
        }
    }
    // Legacy octal like 0755.
    if s.len() > 1 && s.starts_with('0') && s.chars().all(|d| d.is_digit(8)) {
        return Some((i64::from_str_radix(&s, 8).ok()? as f64, i));
    }
    Some((s.parse().ok()?, i))
}

fn unicode_escape(c: &[char], i: usize) -> Option<(char, usize)> {
    if c.get(i) == Some(&'{') {
        let end = i + c[i..].iter().position(|&x| x == '}')?;
        let hex: String = c[i + 1..end].iter().collect();
        return Some((char::from_u32(u32::from_str_radix(&hex, 16).ok()?).unwrap_or('\u{fffd}'), end + 1));
    }
    let hex: String = c.get(i..i + 4)?.iter().collect();
    let v = u32::from_str_radix(&hex, 16).ok()?;
    // Surrogate pairs.
    if (0xD800..0xDC00).contains(&v) && c.get(i + 4) == Some(&'\\') && c.get(i + 5) == Some(&'u') {
        let lo: String = c.get(i + 6..i + 10)?.iter().collect();
        if let Ok(lo) = u32::from_str_radix(&lo, 16) {
            if (0xDC00..0xE000).contains(&lo) {
                let cp = 0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00);
                return Some((char::from_u32(cp).unwrap_or('\u{fffd}'), i + 10));
            }
        }
    }
    Some((char::from_u32(v).unwrap_or('\u{fffd}'), i + 4))
}

/// Process one escape starting after the backslash; returns (text, next index).
fn escape(c: &[char], i: usize) -> Option<(Option<char>, usize)> {
    let e = *c.get(i)?;
    Some(match e {
        'n' => (Some('\n'), i + 1),
        't' => (Some('\t'), i + 1),
        'r' => (Some('\r'), i + 1),
        'b' => (Some('\u{8}'), i + 1),
        'f' => (Some('\u{c}'), i + 1),
        'v' => (Some('\u{b}'), i + 1),
        '0' if !c.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => (Some('\0'), i + 1),
        'x' => {
            let hex: String = c.get(i + 1..i + 3)?.iter().collect();
            (char::from_u32(u32::from_str_radix(&hex, 16).ok()?), i + 3)
        }
        'u' => {
            let (ch, j) = unicode_escape(c, i + 1)?;
            (Some(ch), j)
        }
        '\r' => (None, if c.get(i + 1) == Some(&'\n') { i + 2 } else { i + 1 }),
        '\n' | '\u{2028}' | '\u{2029}' => (None, i + 1),
        other => (Some(other), i + 1),
    })
}

fn string(c: &[char], mut i: usize) -> Option<(String, usize)> {
    let q = c[i];
    i += 1;
    let mut s = String::new();
    while i < c.len() {
        match c[i] {
            ch if ch == q => return Some((s, i + 1)),
            '\\' => {
                let (ch, j) = escape(c, i + 1)?;
                if let Some(ch) = ch {
                    s.push(ch);
                }
                i = j;
            }
            '\n' => return None,
            ch => {
                s.push(ch);
                i += 1;
            }
        }
    }
    None
}

#[allow(clippy::type_complexity)]
fn template(c: &[char], mut i: usize) -> Option<(Vec<Rc<str>>, Vec<String>, usize)> {
    i += 1;
    let mut quasis = Vec::new();
    let mut exprs = Vec::new();
    let mut s = String::new();
    while i < c.len() {
        match c[i] {
            '`' => {
                quasis.push(s.into());
                return Some((quasis, exprs, i + 1));
            }
            '\\' => {
                let (ch, j) = escape(c, i + 1)?;
                if let Some(ch) = ch {
                    s.push(ch);
                }
                i = j;
            }
            '$' if c.get(i + 1) == Some(&'{') => {
                quasis.push(std::mem::take(&mut s).into());
                i += 2;
                let start = i;
                let mut depth = 1;
                while i < c.len() && depth > 0 {
                    match c[i] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        q @ ('"' | '\'') => {
                            i += 1;
                            while i < c.len() && c[i] != q {
                                if c[i] == '\\' {
                                    i += 1;
                                }
                                i += 1;
                            }
                        }
                        '`' => {
                            let (_, _, j) = template(c, i)?;
                            i = j - 1;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                exprs.push(c[start..i - 1].iter().collect());
            }
            ch => {
                s.push(if ch == '\r' { '\n' } else { ch });
                i += 1;
            }
        }
    }
    None
}
