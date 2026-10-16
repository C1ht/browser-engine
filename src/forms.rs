//! Form controls: values, what a control displays, click activation and
//! form submission (application/x-www-form-urlencoded).

use crate::atom::{self, k};
use crate::dom::{Data, Document, Element, NodeId, NONE};

/// A navigation request (a link, a form submission or a script).
#[derive(Clone, Debug, PartialEq)]
pub struct Nav {
    pub url: String,
    /// Request body for POST.
    pub post: Option<String>,
}

fn input_type(el: &Element) -> String {
    el.attr(k::type_).unwrap_or("text").trim().to_ascii_lowercase()
}

/// Text shown inside an `<input>` (its synthesized child text), if any.
pub fn input_text(el: &Element) -> Option<String> {
    let ty = input_type(el);
    let value = el.attr(k::value).filter(|v| !v.is_empty());
    let t = match ty.as_str() {
        "hidden" | "file" | "range" | "color" => return None,
        "checkbox" => return el.attr(k::checked).map(|_| "\u{2713}".to_owned()),
        "radio" => return None,
        "submit" => value.unwrap_or("Submit").to_owned(),
        "reset" => value.unwrap_or("Reset").to_owned(),
        "button" => value.unwrap_or("").to_owned(),
        "image" => el.attr(k::alt).unwrap_or("").to_owned(),
        "password" => "\u{2022}".repeat(value.map_or(0, |v| v.chars().count())),
        _ => value.or(el.attr(k::placeholder)).unwrap_or("").to_owned(),
    };
    // A non-breaking space keeps empty fields one line tall.
    Some(if t.is_empty() { "\u{a0}".to_owned() } else { t })
}

/// Does the control show its placeholder (`:placeholder-shown`)?
pub fn placeholder_shown(doc: &Document, id: NodeId) -> bool {
    let Some(el) = doc.elem(id) else { return false };
    if !matches!(el.tag, k::input | k::textarea) || el.attr(k::placeholder).is_none_or(str::is_empty) {
        return false;
    }
    if el.tag == k::textarea {
        return doc.children(id).next().is_none();
    }
    el.attr(k::value).is_none_or(str::is_empty)
}

/// Is this a control the user types into?
pub fn is_text_field(doc: &Document, id: NodeId) -> bool {
    let typable = match doc.elem(id) {
        Some(el) if el.tag == k::textarea => true,
        Some(el) if el.tag == k::input => matches!(
            input_type(el).as_str(),
            "text" | "search" | "email" | "url" | "tel" | "password" | "number" | "date" | "time" | "datetime-local" | "month" | "week" | ""
        ),
        _ => false,
    };
    typable && !disabled(doc, id)
}

fn disabled(doc: &Document, id: NodeId) -> bool {
    doc.attr(id, k::disabled).is_some() || doc.attr(id, k::readonly).is_some()
}

fn attr<'a>(doc: &'a Document, id: NodeId, name: &str) -> Option<&'a str> {
    atom::lookup(name).and_then(|a| doc.attr(id, a))
}

/// The control's current value.
pub fn value(doc: &Document, id: NodeId) -> String {
    match doc.tag(id) {
        Some(k::textarea) => {
            let mut s = String::new();
            doc.text_content(id, &mut s);
            s
        }
        Some(k::select) => {
            let opts = options(doc, id);
            let chosen = opts.iter().copied().find(|&o| doc.attr(o, k::selected).is_some()).or(opts.first().copied());
            chosen.map(|o| option_value(doc, o)).unwrap_or_default()
        }
        Some(k::input) => {
            let v = doc.attr(id, k::value).unwrap_or("");
            match doc.elem(id).map(input_type).as_deref() {
                Some("checkbox" | "radio") if v.is_empty() => "on".to_owned(),
                _ => v.to_owned(),
            }
        }
        Some(k::button) => doc.attr(id, k::value).unwrap_or("").to_owned(),
        _ => String::new(),
    }
}

fn options(doc: &Document, sel: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![sel];
    while let Some(n) = stack.pop() {
        let kids: Vec<NodeId> = doc.children(n).collect();
        for &c in kids.iter().rev() {
            match doc.tag(c) {
                Some(k::option) => out.push(c),
                Some(_) => stack.push(c),
                None => {}
            }
        }
    }
    out
}

fn option_value(doc: &Document, o: NodeId) -> String {
    doc.attr(o, k::value).map(str::to_owned).unwrap_or_else(|| {
        let mut s = String::new();
        doc.text_content(o, &mut s);
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    })
}

/// Set a text control's value and update what it displays.
pub fn set_value(doc: &mut Document, id: NodeId, v: &str) {
    if doc.tag(id) == Some(k::textarea) {
        doc.remove_children(id);
        if !v.is_empty() {
            doc.append_text(id, v);
        }
    } else {
        doc.set_attr(id, k::value, v);
        refresh(doc, id);
    }
}

/// Rebuild an `<input>`'s displayed text from its attributes.
pub fn refresh(doc: &mut Document, id: NodeId) {
    let Some(el) = doc.elem(id) else { return };
    if el.tag != k::input {
        return;
    }
    let text = input_text(el);
    doc.remove_children(id);
    if let Some(t) = text {
        doc.append_text(id, &t);
    }
}

/// The form a control belongs to (`form` attribute or nearest ancestor).
pub fn form_of(doc: &Document, id: NodeId) -> NodeId {
    if let Some(fid) = attr(doc, id, "form").and_then(atom::lookup) {
        if let Some(f) = (0..doc.nodes.len() as NodeId).find(|&n| doc.elem(n).is_some_and(|e| e.id == Some(fid) && e.tag == k::form)) {
            return f;
        }
    }
    let mut n = doc.parent(id);
    while n != NONE {
        if doc.tag(n) == Some(k::form) {
            return n;
        }
        n = doc.parent(n);
    }
    NONE
}

fn is_submit_button(doc: &Document, id: NodeId) -> bool {
    match doc.elem(id) {
        Some(el) if el.tag == k::button => matches!(el.attr(k::type_).map(|t| t.trim().to_ascii_lowercase()).as_deref(), None | Some("submit") | Some("")),
        Some(el) if el.tag == k::input => matches!(input_type(el).as_str(), "submit" | "image"),
        _ => false,
    }
}

/// What a click on an element does by default.
pub enum Activation {
    None,
    /// Focus this text field.
    Focus(NodeId),
    /// A checkbox or radio button changed.
    Toggled(NodeId),
    /// Submit `form`, with `submitter` (the button), or NONE.
    Submit(NodeId, NodeId),
}

/// The control a click on `node` activates: the nearest form control
/// ancestor, or the control a `<label>` points at. Links stop the search.
pub fn control_for(doc: &Document, node: NodeId) -> Option<NodeId> {
    let mut n = node;
    while n != NONE {
        if let Some(el) = doc.elem(n) {
            match el.tag {
                k::input | k::textarea | k::button | k::select => return Some(n),
                k::label => {
                    let target = attr(doc, n, "for").and_then(atom::lookup).and_then(|a| {
                        (0..doc.nodes.len() as NodeId).find(|&c| doc.elem(c).is_some_and(|e| e.id == Some(a)))
                    });
                    return target.or_else(|| first_control(doc, n));
                }
                k::a if el.attr(k::href).is_some() => return None,
                _ => {}
            }
        }
        n = doc.parent(n);
    }
    None
}

/// Find and activate the control a click on `node` targets.
#[cfg(test)]
pub fn activate(doc: &mut Document, node: NodeId) -> Activation {
    control_for(doc, node).map_or(Activation::None, |c| activate_control(doc, c))
}

/// Is this a checkbox or radio button?
pub fn is_checkable(doc: &Document, id: NodeId) -> bool {
    doc.tag(id) == Some(k::input) && doc.elem(id).is_some_and(|e| matches!(input_type(e).as_str(), "checkbox" | "radio"))
}

/// Which checkable inputs are checked (to undo a cancelled click).
pub fn checked_snapshot(doc: &Document) -> Vec<(NodeId, bool)> {
    (0..doc.nodes.len() as NodeId).filter(|&n| is_checkable(doc, n)).map(|n| (n, doc.attr(n, k::checked).is_some())).collect()
}

pub fn restore_checked(doc: &mut Document, snap: &[(NodeId, bool)]) {
    for &(n, on) in snap {
        if on {
            doc.set_attr(n, k::checked, "");
        } else {
            doc.remove_attr(n, k::checked);
        }
        refresh(doc, n);
    }
}

fn first_control(doc: &Document, root: NodeId) -> Option<NodeId> {
    let mut stack: Vec<NodeId> = doc.children(root).collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        if matches!(doc.tag(n), Some(k::input | k::textarea | k::select | k::button)) {
            return Some(n);
        }
        let mut kids: Vec<NodeId> = doc.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    None
}

pub fn activate_control(doc: &mut Document, id: NodeId) -> Activation {
    if doc.attr(id, k::disabled).is_some() {
        return Activation::None;
    }
    if is_text_field(doc, id) {
        return Activation::Focus(id);
    }
    let ty = doc.elem(id).map(input_type).unwrap_or_default();
    if doc.tag(id) == Some(k::input) && ty == "checkbox" {
        if doc.attr(id, k::checked).is_some() {
            doc.remove_attr(id, k::checked);
        } else {
            doc.set_attr(id, k::checked, "");
        }
        refresh(doc, id);
        return Activation::Toggled(id);
    }
    if doc.tag(id) == Some(k::input) && ty == "radio" {
        let name = doc.attr(id, k::name).map(str::to_owned);
        if let Some(name) = name {
            let form = form_of(doc, id);
            for o in 0..doc.nodes.len() as NodeId {
                if o != id && doc.tag(o) == Some(k::input) && doc.attr(o, k::name) == Some(&name) && form_of(doc, o) == form {
                    doc.remove_attr(o, k::checked);
                }
            }
        }
        doc.set_attr(id, k::checked, "");
        refresh(doc, id);
        return Activation::Toggled(id);
    }
    if doc.tag(id) == Some(k::select) {
        // No dropdown yet: clicking cycles through the options.
        let opts = options(doc, id);
        if opts.len() > 1 {
            let cur = opts.iter().position(|&o| doc.attr(o, k::selected).is_some()).unwrap_or(0);
            let next = opts[(cur + 1) % opts.len()];
            opts.iter().for_each(|&o| doc.remove_attr(o, k::selected));
            doc.set_attr(next, k::selected, "");
            let mut label = String::new();
            doc.text_content(next, &mut label);
            let label = format!("{} \u{25be}", label.split_whitespace().collect::<Vec<_>>().join(" "));
            // The select shows its chosen option as a trailing text child.
            let last = doc.nodes[id as usize].last_child;
            if last != NONE && matches!(doc.nodes[last as usize].data, Data::Text(_)) {
                doc.nodes[last as usize].data = Data::Text(label);
            }
            return Activation::Toggled(id);
        }
        return Activation::None;
    }
    if is_submit_button(doc, id) {
        let form = form_of(doc, id);
        if form != NONE {
            return Activation::Submit(form, id);
        }
    }
    Activation::None
}

/// The submit button implicit submission (Enter in a field) uses.
pub fn default_button(doc: &Document, form: NodeId) -> NodeId {
    let mut stack: Vec<NodeId> = doc.children(form).collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        if is_submit_button(doc, n) {
            return n;
        }
        let mut kids: Vec<NodeId> = doc.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    NONE
}

/// Build the request for submitting `form` (resolved against `base`).
pub fn submission(doc: &Document, form: NodeId, submitter: NodeId, base: &str) -> Option<Nav> {
    let sattr = |name: &str| if submitter != NONE { attr(doc, submitter, name) } else { None };
    let action = sattr("formaction").or_else(|| attr(doc, form, "action")).unwrap_or("").trim();
    let method = sattr("formmethod").or_else(|| attr(doc, form, "method")).unwrap_or("get").trim().to_ascii_lowercase();
    let url = if action.is_empty() { base.to_owned() } else { crate::net::resolve(base, action) };
    if url.starts_with("javascript:") {
        return None;
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for n in doc.preorder() {
        let Some(el) = doc.elem(n) else { continue };
        if !matches!(el.tag, k::input | k::textarea | k::select | k::button) || el.attr(k::disabled).is_some() || form_of(doc, n) != form {
            continue;
        }
        let Some(name) = el.attr(k::name).filter(|s| !s.is_empty()) else { continue };
        let is_button = el.tag == k::button || (el.tag == k::input && matches!(input_type(el).as_str(), "submit" | "image" | "button" | "reset"));
        if is_button {
            if n == submitter {
                if el.tag == k::input && input_type(el) == "image" {
                    pairs.push((format!("{name}.x"), "0".into()));
                    pairs.push((format!("{name}.y"), "0".into()));
                } else {
                    pairs.push((name.to_owned(), value(doc, n)));
                }
            }
            continue;
        }
        if el.tag == k::input {
            match input_type(el).as_str() {
                "checkbox" | "radio" if el.attr(k::checked).is_none() => continue,
                "file" => continue,
                _ => {}
            }
        }
        pairs.push((name.to_owned(), value(doc, n)));
    }
    let body = pairs.iter().map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v))).collect::<Vec<_>>().join("&");
    if method == "post" {
        Some(Nav { url, post: Some(body) })
    } else {
        let stem = url.split(['?', '#']).next().unwrap_or(&url);
        Some(Nav { url: format!("{stem}?{body}"), post: None })
    }
}

/// application/x-www-form-urlencoded byte serializer.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(doc: &Document, id: &str) -> NodeId {
        let a = atom::lookup(id).unwrap();
        (0..doc.nodes.len() as NodeId).find(|&n| doc.elem(n).is_some_and(|e| e.id == Some(a))).unwrap()
    }

    #[test]
    fn submit_get_form() {
        let mut doc = crate::html::parse(
            r#"<form action="/search" id=f>
                 <input id=q name=q value="">
                 <input type=hidden name=hl value=en>
                 <input type=checkbox name=safe id=safe>
                 <input type=radio name=r value=a checked><input type=radio name=r value=b id=rb>
                 <select name=s><option>One</option><option value=2 selected>Two</option></select>
                 <textarea name=t>hi</textarea>
                 <input type=submit name=btn value="Go" id=go>
                 <button name=other value=x>Other</button>
               </form>"#,
        );
        let (q, go, form) = (find(&doc, "q"), find(&doc, "go"), find(&doc, "f"));
        assert!(matches!(activate(&mut doc, q), Activation::Focus(n) if n == q));
        set_value(&mut doc, q, "rust & café");
        let safe = find(&doc, "safe");
        assert!(matches!(activate(&mut doc, safe), Activation::Toggled(_)));
        let rb = find(&doc, "rb");
        activate(&mut doc, rb);
        assert!(matches!(activate(&mut doc, go), Activation::Submit(f, s) if f == form && s == go));
        let nav = submission(&doc, form, go, "https://example.com/page?x=1").unwrap();
        assert_eq!(nav.url, "https://example.com/search?q=rust+%26+caf%C3%A9&hl=en&safe=on&r=b&s=2&t=hi&btn=Go");
        assert_eq!(default_button(&doc, form), go);
    }

    #[test]
    fn post_and_labels() {
        let mut doc = crate::html::parse(r#"<form method=post id=f><label>Name <input name=n id=n></label><label for=c>Check</label><input type=checkbox id=c name=c value=yes></form>"#);
        let (form, n) = (find(&doc, "f"), find(&doc, "n"));
        let label = doc.parent(n);
        assert!(matches!(activate(&mut doc, label), Activation::Focus(x) if x == n));
        let lc = doc.next_element(label);
        activate(&mut doc, lc);
        set_value(&mut doc, n, "a b");
        let nav = submission(&doc, form, NONE, "https://example.com/form").unwrap();
        assert_eq!(nav, Nav { url: "https://example.com/form".into(), post: Some("n=a+b&c=yes".into()) });
    }
}
