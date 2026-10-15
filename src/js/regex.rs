//! A backtracking regular-expression engine for JavaScript RegExp syntax.
//! Matching is over chars (code points); indices are char offsets.

use std::cell::Cell;

#[derive(Debug, Clone)]
enum ClassItem {
    Ch(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(Vec<ClassItem>, bool),
    Start,
    End,
    WordB(bool),
    Group(Box<Node>, Option<usize>),
    Look(Box<Node>, bool, bool),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, usize, usize, bool),
    Backref(usize),
}

pub struct Regex {
    node: Node,
    groups: usize,
    pub global: bool,
    pub sticky: bool,
    icase: bool,
    multiline: bool,
    dotall: bool,
    pub source: String,
    pub flags: String,
}

type Caps = Vec<Option<(usize, usize)>>;

struct P<'a> {
    c: &'a [char],
    i: usize,
    groups: usize,
}

impl P<'_> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn alt(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.concat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.concat()?);
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap() } else { Node::Alt(alts) })
    }
    fn concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            let atom = self.quant(atom)?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Concat(items),
        })
    }
    fn number(&mut self) -> Option<usize> {
        let s = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == s {
            return None;
        }
        self.c[s..self.i].iter().collect::<String>().parse().ok()
    }
    fn quant(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, usize::MAX)
            }
            Some('+') => {
                self.i += 1;
                (1, usize::MAX)
            }
            Some('?') => {
                self.i += 1;
                (0, 1)
            }
            Some('{') => {
                let save = self.i;
                self.i += 1;
                let Some(a) = self.number() else {
                    self.i = save;
                    return Ok(atom);
                };
                let b = if self.peek() == Some(',') {
                    self.i += 1;
                    self.number().unwrap_or(usize::MAX)
                } else {
                    a
                };
                if self.peek() != Some('}') {
                    self.i = save;
                    return Ok(atom);
                }
                self.i += 1;
                (a, b)
            }
            _ => return Ok(atom),
        };
        let greedy = if self.peek() == Some('?') {
            self.i += 1;
            false
        } else {
            true
        };
        Ok(Node::Repeat(Box::new(atom), min, max, greedy))
    }
    fn escape_class(&mut self, c: char) -> Option<ClassItem> {
        Some(match c {
            'd' => ClassItem::Digit(true),
            'D' => ClassItem::Digit(false),
            'w' => ClassItem::Word(true),
            'W' => ClassItem::Word(false),
            's' => ClassItem::Space(true),
            'S' => ClassItem::Space(false),
            _ => return None,
        })
    }
    fn escape_char(&mut self, c: char) -> char {
        match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'f' => '\u{c}',
            'v' => '\u{b}',
            '0' => '\0',
            'x' => {
                let h: String = self.c.get(self.i..self.i + 2).map(|s| s.iter().collect()).unwrap_or_default();
                if let Ok(v) = u32::from_str_radix(&h, 16) {
                    self.i += 2;
                    char::from_u32(v).unwrap_or('x')
                } else {
                    'x'
                }
            }
            'u' => {
                if self.peek() == Some('{') {
                    let end = self.c[self.i..].iter().position(|&x| x == '}').map(|p| self.i + p);
                    if let Some(end) = end {
                        let h: String = self.c[self.i + 1..end].iter().collect();
                        self.i = end + 1;
                        return u32::from_str_radix(&h, 16).ok().and_then(char::from_u32).unwrap_or('u');
                    }
                }
                let h: String = self.c.get(self.i..self.i + 4).map(|s| s.iter().collect()).unwrap_or_default();
                if let Ok(v) = u32::from_str_radix(&h, 16) {
                    self.i += 4;
                    char::from_u32(v).unwrap_or('\u{fffd}')
                } else {
                    'u'
                }
            }
            c => c,
        }
    }
    fn atom(&mut self) -> Result<Node, String> {
        let c = self.peek().ok_or("unexpected end")?;
        self.i += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '(' => {
                let mut cap = None;
                let mut look = None;
                if self.peek() == Some('?') {
                    self.i += 1;
                    match self.peek() {
                        Some(':') => self.i += 1,
                        Some('=') => {
                            self.i += 1;
                            look = Some((true, false));
                        }
                        Some('!') => {
                            self.i += 1;
                            look = Some((true, true));
                        }
                        Some('<') if matches!(self.c.get(self.i + 1), Some('=') | Some('!')) => {
                            let neg = self.c[self.i + 1] == '!';
                            self.i += 2;
                            look = Some((false, neg));
                        }
                        Some('<') => {
                            // named group
                            while self.peek().is_some_and(|c| c != '>') {
                                self.i += 1;
                            }
                            self.i += 1;
                            self.groups += 1;
                            cap = Some(self.groups);
                        }
                        _ => return Err("bad group".into()),
                    }
                } else {
                    self.groups += 1;
                    cap = Some(self.groups);
                }
                let inner = self.alt()?;
                if self.peek() != Some(')') {
                    return Err("missing )".into());
                }
                self.i += 1;
                match look {
                    Some((ahead, neg)) => Node::Look(Box::new(inner), ahead, neg),
                    None => Node::Group(Box::new(inner), cap),
                }
            }
            '[' => {
                let mut neg = false;
                if self.peek() == Some('^') {
                    neg = true;
                    self.i += 1;
                }
                let mut items = Vec::new();
                let mut first = true;
                while let Some(c) = self.peek() {
                    if c == ']' && !first {
                        break;
                    }
                    if c == ']' && first {
                        break;
                    }
                    first = false;
                    self.i += 1;
                    let lo = if c == '\\' {
                        let e = self.peek().ok_or("bad escape")?;
                        self.i += 1;
                        if let Some(ci) = self.escape_class(e) {
                            items.push(ci);
                            continue;
                        }
                        if e == 'b' { '\u{8}' } else { self.escape_char(e) }
                    } else {
                        c
                    };
                    if self.peek() == Some('-') && self.c.get(self.i + 1).is_some_and(|&n| n != ']') {
                        self.i += 1;
                        let mut hi = self.peek().unwrap();
                        self.i += 1;
                        if hi == '\\' {
                            let e = self.peek().ok_or("bad escape")?;
                            self.i += 1;
                            hi = self.escape_char(e);
                        }
                        items.push(ClassItem::Range(lo, hi));
                    } else {
                        items.push(ClassItem::Ch(lo));
                    }
                }
                if self.peek() != Some(']') {
                    return Err("missing ]".into());
                }
                self.i += 1;
                Node::Class(items, neg)
            }
            '\\' => {
                let e = self.peek().ok_or("trailing backslash")?;
                self.i += 1;
                if let Some(ci) = self.escape_class(e) {
                    return Ok(Node::Class(vec![ci], false));
                }
                match e {
                    'b' => Node::WordB(true),
                    'B' => Node::WordB(false),
                    '1'..='9' => {
                        self.i -= 1;
                        Node::Backref(self.number().unwrap_or(1))
                    }
                    'k' if self.peek() == Some('<') => {
                        while self.peek().is_some_and(|c| c != '>') {
                            self.i += 1;
                        }
                        self.i += 1;
                        Node::Empty
                    }
                    e => Node::Char(self.escape_char(e)),
                }
            }
            c => Node::Char(c),
        })
    }
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl Regex {
    pub fn new(src: &str, flags: &str) -> Result<Regex, String> {
        let chars: Vec<char> = src.chars().collect();
        let mut p = P { c: &chars, i: 0, groups: 0 };
        let node = p.alt()?;
        if p.i < chars.len() {
            return Err("unmatched )".into());
        }
        Ok(Regex {
            node,
            groups: p.groups,
            global: flags.contains('g'),
            sticky: flags.contains('y'),
            icase: flags.contains('i'),
            multiline: flags.contains('m'),
            dotall: flags.contains('s'),
            source: src.to_owned(),
            flags: flags.to_owned(),
        })
    }

    /// First match starting at or after `start`: caps[0] is the whole match.
    pub fn exec_at(&self, s: &[char], start: usize) -> Option<Caps> {
        let steps = Cell::new(0usize);
        let last = if self.sticky { start } else { s.len() };
        for i in start..=last.min(s.len()) {
            let mut caps: Caps = vec![None; self.groups + 1];
            let mut end = None;
            if self.m(&self.node, s, i, &mut caps, &steps, &mut |j, _| {
                end = Some(j);
                true
            }) {
                caps[0] = Some((i, end.unwrap()));
                return Some(caps);
            }
            if steps.get() > 5_000_000 {
                return None;
            }
        }
        None
    }

    fn eq(&self, a: char, b: char) -> bool {
        a == b || (self.icase && a.to_lowercase().eq(b.to_lowercase()))
    }

    fn class_match(&self, items: &[ClassItem], c: char) -> bool {
        items.iter().any(|it| match *it {
            ClassItem::Ch(x) => self.eq(x, c),
            ClassItem::Range(a, b) => {
                (a..=b).contains(&c) || (self.icase && ((a..=b).contains(&c.to_ascii_lowercase()) || (a..=b).contains(&c.to_ascii_uppercase())))
            }
            ClassItem::Digit(p) => c.is_ascii_digit() == p,
            ClassItem::Word(p) => is_word(c) == p,
            ClassItem::Space(p) => c.is_whitespace() == p,
        })
    }

    #[allow(clippy::only_used_in_recursion)]
    fn m(&self, n: &Node, s: &[char], i: usize, caps: &mut Caps, steps: &Cell<usize>, k: &mut dyn FnMut(usize, &mut Caps) -> bool) -> bool {
        steps.set(steps.get() + 1);
        if steps.get() > 5_000_000 {
            return false;
        }
        match n {
            Node::Empty => k(i, caps),
            Node::Char(c) => i < s.len() && self.eq(*c, s[i]) && k(i + 1, caps),
            Node::Any => i < s.len() && (self.dotall || (s[i] != '\n' && s[i] != '\r')) && k(i + 1, caps),
            Node::Class(items, neg) => i < s.len() && self.class_match(items, s[i]) != *neg && k(i + 1, caps),
            Node::Start => (i == 0 || (self.multiline && s[i - 1] == '\n')) && k(i, caps),
            Node::End => (i == s.len() || (self.multiline && s[i] == '\n')) && k(i, caps),
            Node::WordB(want) => {
                let a = i > 0 && is_word(s[i - 1]);
                let b = i < s.len() && is_word(s[i]);
                ((a != b) == *want) && k(i, caps)
            }
            Node::Group(inner, cap) => {
                let Some(ci) = *cap else { return self.m(inner, s, i, caps, steps, k) };
                self.m(inner, s, i, caps, steps, &mut |j, caps| {
                    let old = caps[ci];
                    caps[ci] = Some((i, j));
                    if k(j, caps) {
                        true
                    } else {
                        caps[ci] = old;
                        false
                    }
                })
            }
            Node::Look(inner, ahead, neg) => {
                if !*ahead {
                    // Lookbehind: try every start before i.
                    let mut found = false;
                    for st in (0..=i).rev() {
                        let mut c2 = caps.clone();
                        if self.m(inner, s, st, &mut c2, steps, &mut |j, _| j == i) {
                            found = true;
                            break;
                        }
                    }
                    return found != *neg && k(i, caps);
                }
                let mut c2 = caps.clone();
                let ok = self.m(inner, s, i, &mut c2, steps, &mut |_, _| true);
                if ok != *neg {
                    if ok {
                        *caps = c2;
                    }
                    k(i, caps)
                } else {
                    false
                }
            }
            Node::Concat(items) => self.seq(items, s, i, caps, steps, k),
            Node::Alt(alts) => {
                for a in alts {
                    if self.m(a, s, i, caps, steps, k) {
                        return true;
                    }
                }
                false
            }
            Node::Repeat(inner, min, max, greedy) => self.rep(inner, *min, *max, *greedy, 0, s, i, caps, steps, k),
            Node::Backref(n) => {
                let Some(Some((a, b))) = caps.get(*n).copied() else { return k(i, caps) };
                let len = b - a;
                if i + len <= s.len() && (0..len).all(|d| self.eq(s[a + d], s[i + d])) {
                    k(i + len, caps)
                } else {
                    false
                }
            }
        }
    }

    fn seq(&self, items: &[Node], s: &[char], i: usize, caps: &mut Caps, steps: &Cell<usize>, k: &mut dyn FnMut(usize, &mut Caps) -> bool) -> bool {
        match items.split_first() {
            None => k(i, caps),
            Some((first, rest)) => self.m(first, s, i, caps, steps, &mut |j, caps| self.seq(rest, s, j, caps, steps, k)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rep(&self, inner: &Node, min: usize, max: usize, greedy: bool, count: usize, s: &[char], i: usize, caps: &mut Caps, steps: &Cell<usize>, k: &mut dyn FnMut(usize, &mut Caps) -> bool) -> bool {
        // Fast path: greedy single-char atoms without backreferences.
        if greedy && count == 0 {
            if let Some(single) = self.single(inner) {
                let mut j = i;
                let mut n = 0;
                while n < max && j < s.len() && single(self, s[j]) {
                    j += 1;
                    n += 1;
                }
                loop {
                    if n < min {
                        return false;
                    }
                    if k(i + n, caps) {
                        return true;
                    }
                    if n == 0 {
                        return false;
                    }
                    n -= 1;
                }
            }
        }
        let try_more = |this: &Self, caps: &mut Caps, k: &mut dyn FnMut(usize, &mut Caps) -> bool| -> bool {
            count < max
                && this.m(inner, s, i, caps, steps, &mut |j, caps| {
                    // Zero-width iterations can't loop forever.
                    if j == i && count >= min {
                        return false;
                    }
                    this.rep(inner, min, max, greedy, count + 1, s, j, caps, steps, k)
                })
        };
        if greedy {
            if try_more(self, caps, k) {
                return true;
            }
            count >= min && k(i, caps)
        } else {
            if count >= min && k(i, caps) {
                return true;
            }
            try_more(self, caps, k)
        }
    }

    #[allow(clippy::type_complexity)]
    fn single(&self, n: &Node) -> Option<fn(&Regex, char) -> bool> {
        match n {
            Node::Any if self.dotall => Some(|_, _| true),
            Node::Any => Some(|_, c| c != '\n' && c != '\r'),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(re: &str, flags: &str, s: &str) -> Option<String> {
        let r = Regex::new(re, flags).unwrap();
        let c: Vec<char> = s.chars().collect();
        r.exec_at(&c, 0).map(|caps| {
            let (a, b) = caps[0].unwrap();
            c[a..b].iter().collect()
        })
    }

    #[test]
    fn basics() {
        assert_eq!(find("a+b", "", "xxaaab"), Some("aaab".into()));
        assert_eq!(find("^\\d{3}-\\d{4}$", "", "555-1234"), Some("555-1234".into()));
        assert_eq!(find("(foo|bar)baz", "", "xbarbaz"), Some("barbaz".into()));
        assert_eq!(find("[a-z]+", "i", "123HeLLo!"), Some("HeLLo".into()));
        assert_eq!(find("a.*?b", "", "a1b2b"), Some("a1b".into()));
        assert_eq!(find("a.*b", "", "a1b2b"), Some("a1b2b".into()));
        assert_eq!(find("\\bcat\\b", "", "concat cat"), Some("cat".into()));
        assert_eq!(find("(\\w)\\1", "", "abccd"), Some("cc".into()));
        assert_eq!(find("foo(?=bar)", "", "foobaz foobar"), Some("foo".into()));
        assert_eq!(find("^\\s*$", "", "   "), Some("   ".into()));
        assert_eq!(find("x", "", "abc"), None);
    }
}
