//! Generators, async functions and async generators on coroutines.
//!
//! Each generator (and each call of an async function) runs its body on its
//! own coroutine (`coro`). `yield` suspends back to whoever called `next()`;
//! `await` subscribes to the promise and suspends until a promise reaction
//! resumes the body. The object holding a coroutine is a `Kind::Generator`
//! (async functions get a hidden one).

use super::builtins::{self, bound_native};
use super::coro::{self, Out, Resume};
use super::*;

/// Thrown through a generator body to run its `finally` blocks when
/// `return()` is called (try/catch don't catch it).
pub const RETURN_MARKER: &str = "@@generator-return";

pub fn is_return_marker(e: &Value) -> bool {
    matches!(e, Value::Str(s) if &**s == RETURN_MARKER)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GenState {
    /// Created, body not started.
    Fresh,
    /// Suspended at a `yield`.
    Yielded,
    /// Suspended at an `await` (async functions / generators).
    Awaiting,
    Running,
    Done,
}

/// A queued `next` / `throw` / `return` call on an async generator.
pub enum Req {
    Next,
    Throw,
    Return,
}

impl Interp {
    fn make_coro(&mut self, st: GenStart) -> Option<coro::Co> {
        let ip: *mut Interp = self;
        coro::Co::new(Box::new(move |first| {
            // SAFETY: the interpreter is boxed (never moves) and outlives the
            // coroutines it starts; only one fiber runs at a time.
            let it = unsafe { &mut *ip };
            match first {
                Resume::Next(_) => it.call_closure(&st.def, st.env, None, st.home, st.this, &st.args, Some(st.fobj)),
                Resume::Throw(e) => Err(e),
                Resume::Return(v) => Ok(v),
            }
        }))
    }

    /// A generator object backed by a coroutine (None if unsupported).
    pub(crate) fn new_coro_gen(&mut self, st: GenStart, is_async: bool) -> Option<Value> {
        let co = self.make_coro(st)?;
        let g = Gen { co: Some(co), coro: true, is_async, ..Gen::default() };
        Some(Value::Obj(self.new_obj(self.p.generator.clone(), Kind::Generator(Box::new(g)))))
    }

    /// Call an async function: run its body until the first `await`.
    pub(crate) fn start_async(&mut self, st: GenStart) -> Option<Value> {
        let co = self.make_coro(st)?;
        let promise = builtins::new_promise(self);
        let g = Gen { co: Some(co), coro: true, is_async: true, promise: Some(promise.clone()), ..Gen::default() };
        let holder = self.new_obj(None, Kind::Generator(Box::new(g)));
        self.co_drive(&holder, Resume::Next(Value::Undef));
        Some(Value::Obj(promise))
    }

    /// Run `job` like an async function body (it may `await`); returns the
    /// promise for its result.
    pub(crate) fn start_job(&mut self, job: Box<dyn FnOnce(&mut Interp) -> R<Value>>) -> Value {
        if !coro::supported() {
            // The body can't suspend; run it to completion now.
            return match job(self) {
                Ok(v) => builtins::promise_resolved(self, v),
                Err(e) => builtins::promise_rejected(self, e),
            };
        }
        let ip: *mut Interp = self;
        let co = coro::Co::new(Box::new(move |first| {
            // SAFETY: as in `make_coro`.
            let it = unsafe { &mut *ip };
            match first {
                Resume::Next(_) => job(it),
                Resume::Throw(e) => Err(e),
                Resume::Return(v) => Ok(v),
            }
        }));
        let promise = builtins::new_promise(self);
        let Some(co) = co else {
            return builtins::promise_rejected(self, type_error("could not start coroutine"));
        };
        let g = Gen { co: Some(co), coro: true, is_async: true, promise: Some(promise.clone()), ..Gen::default() };
        let holder = self.new_obj(None, Kind::Generator(Box::new(g)));
        self.co_drive(&holder, Resume::Next(Value::Undef));
        Value::Obj(promise)
    }

    /// Resume `holder`'s coroutine, tracking the running-coroutine stack and
    /// giving the body its own call depth.
    fn run_co(&mut self, holder: &ObjRef, co: &coro::Co, r: Resume) -> Out {
        let depth = match &mut holder.borrow_mut().kind {
            Kind::Generator(g) => {
                g.state = GenState::Running;
                g.depth
            }
            _ => 0,
        };
        let saved = std::mem::replace(&mut self.depth, depth);
        self.co_stack.push(holder.clone());
        let out = co.resume(self as *mut Interp as *mut std::ffi::c_void, r);
        self.co_stack.pop();
        let depth = std::mem::replace(&mut self.depth, saved);
        if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
            g.depth = depth;
        }
        out
    }

    fn take_co(holder: &ObjRef) -> (Option<coro::Co>, GenState) {
        match &mut holder.borrow_mut().kind {
            Kind::Generator(g) => (g.co.take(), g.state),
            _ => (None, GenState::Done),
        }
    }

    fn put_co(holder: &ObjRef, co: Option<coro::Co>, state: GenState) {
        if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
            if co.is_some() {
                g.co = co;
            }
            g.state = state;
        }
    }

    /// The value a `return()` injected into a generator body.
    fn finished(holder: &ObjRef, r: R<Value>) -> R<Value> {
        match r {
            Err(e) if is_return_marker(&e) => Ok(match &mut holder.borrow_mut().kind {
                Kind::Generator(g) => std::mem::take(&mut g.ret_val),
                _ => Value::Undef,
            }),
            r => r,
        }
    }

    /// `next` / `throw` / `return` on a synchronous coroutine generator.
    pub(crate) fn gen_resume(&mut self, holder: &ObjRef, r: Resume) -> R<(Value, bool)> {
        let (co, state) = Self::take_co(holder);
        let done = |r: Resume| match r {
            Resume::Next(_) => Ok((Value::Undef, true)),
            Resume::Return(v) => Ok((v, true)),
            Resume::Throw(e) => Err(e),
        };
        match state {
            GenState::Running => return Err(type_error("Generator is already running")),
            GenState::Done => return done(r),
            GenState::Fresh if !matches!(r, Resume::Next(_)) => {
                Self::put_co(holder, None, GenState::Done);
                drop(co);
                return done(r);
            }
            _ => {}
        }
        let Some(co) = co else { return done(r) };
        match self.run_co(holder, &co, r) {
            Out::Yield(v) => {
                Self::put_co(holder, Some(co), GenState::Yielded);
                Ok((v, false))
            }
            Out::Await => {
                Self::put_co(holder, Some(co), GenState::Awaiting);
                Ok((Value::Undef, false))
            }
            Out::Done(res) => {
                co.recycle();
                Self::put_co(holder, None, GenState::Done);
                Self::finished(holder, res).map(|v| (v, true))
            }
        }
    }

    /// Resume an async function / async generator body and handle where it
    /// stops.
    pub(crate) fn co_drive(&mut self, holder: &ObjRef, r: Resume) {
        let (co, state) = Self::take_co(holder);
        let Some(co) = co else { return };
        if state == GenState::Running {
            Self::put_co(holder, Some(co), state);
            return;
        }
        match self.run_co(holder, &co, r) {
            Out::Await => Self::put_co(holder, Some(co), GenState::Awaiting),
            Out::Yield(v) => {
                Self::put_co(holder, Some(co), GenState::Yielded);
                if let Some(p) = self.agen_pop(holder) {
                    let res = builtins::iter_result(self, v, false);
                    builtins::settle(self, &p, true, res);
                }
                self.agen_pump(holder);
            }
            Out::Done(res) => {
                co.recycle();
                Self::put_co(holder, None, GenState::Done);
                let res = Self::finished(holder, res);
                let promise = match &holder.borrow().kind {
                    Kind::Generator(g) => g.promise.clone(),
                    _ => None,
                };
                if let Some(p) = promise {
                    // An async function's result.
                    match res {
                        Ok(v) => builtins::resolve_with(self, &p, v),
                        Err(e) => builtins::settle(self, &p, false, e),
                    }
                    return;
                }
                if let Some(p) = self.agen_pop(holder) {
                    match res {
                        Ok(v) => {
                            let r = builtins::iter_result(self, v, true);
                            builtins::settle(self, &p, true, r);
                        }
                        Err(e) => builtins::settle(self, &p, false, e),
                    }
                }
                self.agen_pump(holder);
            }
        }
    }

    fn agen_pop(&mut self, holder: &ObjRef) -> Option<ObjRef> {
        match &mut holder.borrow_mut().kind {
            Kind::Generator(g) => g.queue.pop_front().map(|(_, _, p)| p),
            _ => None,
        }
    }

    /// Queue a request on an async generator; returns its promise.
    pub(crate) fn agen_request(&mut self, holder: &ObjRef, req: Req, v: Value) -> Value {
        let p = builtins::new_promise(self);
        if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
            g.queue.push_back((req, v, p.clone()));
        }
        self.agen_pump(holder);
        Value::Obj(p)
    }

    /// Start the next queued async generator request if the body is idle.
    fn agen_pump(&mut self, holder: &ObjRef) {
        loop {
            let (state, front) = match &mut holder.borrow_mut().kind {
                Kind::Generator(g) => match g.queue.front_mut() {
                    Some((req, v, _)) => {
                        let r = match req {
                            Req::Next => Resume::Next(std::mem::take(v)),
                            Req::Throw => Resume::Throw(std::mem::take(v)),
                            Req::Return => Resume::Return(std::mem::take(v)),
                        };
                        (g.state, Some(r))
                    }
                    None => (g.state, None),
                },
                _ => return,
            };
            let Some(r) = front else { return };
            match state {
                GenState::Running | GenState::Awaiting => return,
                GenState::Fresh if !matches!(r, Resume::Next(_)) => {
                    let (co, _) = Self::take_co(holder);
                    drop(co);
                    Self::put_co(holder, None, GenState::Done);
                    continue;
                }
                GenState::Done => {
                    let Some(p) = self.agen_pop(holder) else { return };
                    match r {
                        Resume::Next(_) => {
                            let res = builtins::iter_result(self, Value::Undef, true);
                            builtins::settle(self, &p, true, res);
                        }
                        Resume::Return(v) => {
                            let res = builtins::iter_result(self, v, true);
                            builtins::settle(self, &p, true, res);
                        }
                        Resume::Throw(e) => builtins::settle(self, &p, false, e),
                    }
                }
                _ => {
                    self.co_drive(holder, r);
                    return;
                }
            }
        }
    }

    /// `await v` inside a coroutine-backed async body. None if not in one.
    pub(crate) fn co_await(&mut self, v: Value) -> Option<R<Value>> {
        let holder = self.co_stack.last()?.clone();
        if !matches!(&holder.borrow().kind, Kind::Generator(g) if g.is_async) {
            return None;
        }
        let p = match &v {
            Value::Obj(o) if matches!(o.borrow().kind, Kind::Promise(_)) => o.clone(),
            _ => {
                let p = builtins::new_promise(self);
                builtins::resolve_with(self, &p, v);
                p
            }
        };
        let ok = bound_native(self, co_resume_ok, Value::Obj(holder.clone()));
        let err = bound_native(self, co_resume_err, Value::Obj(holder.clone()));
        builtins::subscribe(self, &p, ok, err);
        Some(self.suspend_with(&holder, Out::Await))
    }

    /// Suspend the running body; translate how it was resumed.
    fn suspend_with(&mut self, holder: &ObjRef, out: Out) -> R<Value> {
        match coro::suspend(self as *mut Interp as *mut std::ffi::c_void, out) {
            Resume::Next(x) => Ok(x),
            Resume::Throw(e) => Err(e),
            Resume::Return(v) => {
                if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
                    g.ret_val = v;
                }
                Err(Value::str(RETURN_MARKER))
            }
        }
    }

    /// `yield v` / `yield* v` inside a coroutine generator. None if not in one.
    pub(crate) fn co_yield(&mut self, v: Value, delegate: bool) -> Option<R<Value>> {
        let holder = self.co_stack.last()?.clone();
        let is_async = match &holder.borrow().kind {
            Kind::Generator(g) if g.promise.is_none() => g.is_async,
            _ => return None,
        };
        Some(if delegate { self.yield_star(&holder, v, is_async) } else { self.yield_one(&holder, v, is_async) })
    }

    fn yield_one(&mut self, holder: &ObjRef, v: Value, is_async: bool) -> R<Value> {
        let v = if is_async { builtins::await_value(self, v)? } else { v };
        let r = self.suspend_with(holder, Out::Yield(v));
        match r {
            Err(e) if is_async && is_return_marker(&e) => {
                // `return(v)` on an async generator awaits v.
                let v = match &mut holder.borrow_mut().kind {
                    Kind::Generator(g) => std::mem::take(&mut g.ret_val),
                    _ => Value::Undef,
                };
                let v = builtins::await_value(self, v)?;
                if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
                    g.ret_val = v;
                }
                Err(e)
            }
            r => r,
        }
    }

    fn yield_star(&mut self, holder: &ObjRef, v: Value, is_async: bool) -> R<Value> {
        let mut iter_fn = if is_async { self.get(&v, "@@asyncIterator")? } else { Value::Undef };
        let mut inner_async = is_async && Self::is_callable(&iter_fn);
        if !inner_async {
            iter_fn = self.get(&v, "@@iterator")?;
            inner_async = false;
        }
        if !Self::is_callable(&iter_fn) {
            // Built-in iterables without an @@iterator method.
            for item in self.iterate(&v)? {
                self.yield_one(holder, item, is_async)?;
            }
            return Ok(Value::Undef);
        }
        let iter = self.call(&iter_fn, v.clone(), &[])?;
        let next = self.get(&iter, "next")?;
        let mut received = Resume::Next(Value::Undef);
        loop {
            let r = match received {
                Resume::Next(x) => self.call(&next, iter.clone(), &[x])?,
                Resume::Throw(e) => {
                    let t = self.get(&iter, "throw")?;
                    if !Self::is_callable(&t) {
                        let rf = self.get(&iter, "return")?;
                        if Self::is_callable(&rf) {
                            self.call(&rf, iter.clone(), &[])?;
                        }
                        return Err(type_error("The iterator does not provide a 'throw' method"));
                    }
                    self.call(&t, iter.clone(), &[e])?
                }
                Resume::Return(x) => {
                    let rf = self.get(&iter, "return")?;
                    if !Self::is_callable(&rf) {
                        if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
                            g.ret_val = x;
                        }
                        return Err(Value::str(RETURN_MARKER));
                    }
                    let mut r = self.call(&rf, iter.clone(), &[x])?;
                    if inner_async {
                        r = builtins::await_value(self, r)?;
                    }
                    if self.get(&r, "done")?.truthy() {
                        let v = self.get(&r, "value")?;
                        if let Kind::Generator(g) = &mut holder.borrow_mut().kind {
                            g.ret_val = v;
                        }
                        return Err(Value::str(RETURN_MARKER));
                    }
                    r
                }
            };
            let r = if inner_async { builtins::await_value(self, r)? } else { r };
            if self.get(&r, "done")?.truthy() {
                return self.get(&r, "value");
            }
            let item = self.get(&r, "value")?;
            received = match coro::suspend(self as *mut Interp as *mut std::ffi::c_void, Out::Yield(item)) {
                r @ (Resume::Next(_) | Resume::Throw(_) | Resume::Return(_)) => r,
            };
        }
    }
}

fn co_resume_ok(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    if let Value::Obj(h) = t {
        it.co_drive(&h, Resume::Next(a.first().cloned().unwrap_or_default()));
    }
    Ok(Value::Undef)
}

fn co_resume_err(it: &mut Interp, t: Value, a: &[Value]) -> R<Value> {
    if let Value::Obj(h) = t {
        it.co_drive(&h, Resume::Throw(a.first().cloned().unwrap_or_default()));
    }
    Ok(Value::Undef)
}
