//! postMessage between windows (a page and its iframes) and MessagePorts
//! (which can be transferred from one frame's interpreter to another).
//!
//! All interpreters run on one thread. A port has a process-unique id; its
//! messages wait in a shared queue until the interpreter that owns the port
//! (`Interp::owned_ports`) polls them. Window messages to another frame go
//! to the sender's `frame_outbox`, which the page moves between
//! interpreters (see `engine::Page::tick_frames`). Message data crosses as
//! a JSON-based structured serialization made by the prelude.

use super::builtins::arg;
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

/// A port message: serialized data and the transferred ports (id, peer).
type PortMsg = (String, Vec<(u64, u64)>);

thread_local! {
    static NEXT_PORT: Cell<u64> = const { Cell::new(1) };
    static QUEUES: RefCell<FxHashMap<u64, VecDeque<PortMsg>>> = RefCell::new(FxHashMap::default());
}

/// Where a window message goes.
#[derive(Clone, Debug)]
pub enum MsgTarget {
    /// The window containing this frame.
    Parent,
    /// An iframe element's window.
    Frame(NodeId),
    /// Another frame of the parent page, by name (`parent.frames[name]`).
    Sibling(String),
}

/// A window message waiting to be moved to another interpreter.
#[derive(Clone, Debug)]
pub struct FrameMsg {
    pub to: MsgTarget,
    pub data: String,
    pub ports: Vec<(u64, u64)>,
    pub target_origin: String,
}

/// The origin of a URL ("null" for opaque ones).
pub fn origin_of(url: &str) -> String {
    match super::web::parse_url(url) {
        Some(p) if !p.hostname.is_empty() => {
            let port = if p.port.is_empty() { String::new() } else { format!(":{}", p.port) };
            format!("{}//{}{port}", p.protocol, p.hostname)
        }
        _ => "null".into(),
    }
}

fn pairs(it: &mut Interp, v: &Value) -> R<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    for p in it.iterate(v)? {
        let id = it.get(&p, "0")?;
        let peer = it.get(&p, "1")?;
        out.push((it.to_num(&id)? as u64, it.to_num(&peer)? as u64));
    }
    Ok(out)
}

fn pairs_value(it: &mut Interp, ports: &[(u64, u64)]) -> Value {
    let items = ports
        .iter()
        .map(|&(id, peer)| {
            let pair = vec![Value::Num(id as f64), Value::Num(peer as f64)];
            it.array(pair)
        })
        .collect();
    it.array(items)
}

/// `@@portNew()`: a fresh port id, owned by this interpreter.
fn port_new(it: &mut Interp, _: Value, _: &[Value]) -> R<Value> {
    let id = NEXT_PORT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    it.owned_ports.insert(id);
    Ok(Value::Num(id as f64))
}

/// `@@portSend(toPort, data, [[id, peer]...])`: queue a message for a port.
fn port_send(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let to = it.to_num(&arg(a, 0))? as u64;
    let data = it.to_str(&arg(a, 1))?.to_string();
    let ports = pairs(it, &arg(a, 2))?;
    for &(id, _) in &ports {
        it.owned_ports.remove(&id);
    }
    QUEUES.with(|q| q.borrow_mut().entry(to).or_default().push_back((data, ports)));
    Ok(Value::Undef)
}

/// `@@frameSend(kind, element, data, ports, targetOrigin)`: a window
/// message to the parent window ("parent") or an iframe's ("frame").
fn frame_send(it: &mut Interp, _: Value, a: &[Value]) -> R<Value> {
    let kind = it.to_str(&arg(a, 0))?;
    let to = if &*kind == "parent" {
        MsgTarget::Parent
    } else if &*kind == "sibling" {
        MsgTarget::Sibling(it.to_str(&arg(a, 1))?.to_string())
    } else {
        match &arg(a, 1) {
            Value::Obj(o) => match o.borrow().kind {
                Kind::Node(n) => MsgTarget::Frame(n),
                _ => return Ok(Value::Undef),
            },
            _ => return Ok(Value::Undef),
        }
    };
    let data = it.to_str(&arg(a, 2))?.to_string();
    let ports = pairs(it, &arg(a, 3))?;
    for &(id, _) in &ports {
        it.owned_ports.remove(&id);
    }
    let target_origin = match arg(a, 4) {
        Value::Undef | Value::Null => "/".to_string(),
        v => it.to_str(&v)?.to_string(),
    };
    it.frame_outbox.push(FrameMsg { to, data, ports, target_origin });
    Ok(Value::Undef)
}

pub fn install(it: &mut Interp) {
    let g = it.global.clone();
    for (n, f) in [("@@portNew", port_new as NativeFn), ("@@portSend", port_send), ("@@frameSend", frame_send)] {
        let v = it.native(f, n);
        Interp::set_hidden(&g, n, v);
    }
}

impl Interp {
    /// Deliver messages queued for this interpreter's ports.
    pub fn poll_ports(&mut self) -> bool {
        if self.owned_ports.is_empty() {
            return false;
        }
        let mut due: Vec<(u64, PortMsg)> = Vec::new();
        QUEUES.with(|q| {
            let mut q = q.borrow_mut();
            for &id in &self.owned_ports {
                if let Some(msgs) = q.get_mut(&id) {
                    due.extend(msgs.drain(..).map(|m| (id, m)));
                }
            }
        });
        if due.is_empty() {
            return false;
        }
        let deliver = self.get(&Value::Obj(self.global.clone()), "@@portDeliver").unwrap_or_default();
        for (id, (data, ports)) in due {
            for &(p, _) in &ports {
                self.owned_ports.insert(p);
            }
            let pv = pairs_value(self, &ports);
            if let Err(e) = self.call(&deliver, Value::Undef, &[Value::Num(id as f64), Value::str(&data), pv]) {
                let m = self.describe_error(&e);
                self.log(&m);
            }
            self.run_microtasks();
        }
        true
    }

    /// Deliver a window message from another frame: `source` is the
    /// iframe element it came from (None: the parent window).
    pub fn deliver_window_message(&mut self, msg: &FrameMsg, origin: &str, source: Option<NodeId>) {
        // targetOrigin: "*", "/" (same origin) or an origin.
        let mine = origin_of(&self.url);
        if msg.target_origin != "*" && msg.target_origin != "/" && origin_of(&msg.target_origin) != mine {
            return;
        }
        for &(p, _) in &msg.ports {
            self.owned_ports.insert(p);
        }
        let deliver = self.get(&Value::Obj(self.global.clone()), "@@windowDeliver").unwrap_or_default();
        let pv = pairs_value(self, &msg.ports);
        let src = match source {
            Some(n) => super::dombind::node_obj(self, n),
            None => Value::Null,
        };
        if let Err(e) = self.call(&deliver, Value::Undef, &[Value::str(&msg.data), pv, Value::str(origin), src]) {
            let m = self.describe_error(&e);
            self.log(&m);
        }
        self.run_microtasks();
    }
}
