//! Stackful coroutines, so generator and async function bodies can suspend
//! in the middle of the tree-walking interpreter's native call stack.
//!
//! On Windows these are OS fibers: `SwitchToFiber` swaps stacks (and keeps
//! the thread's stack bounds right for guard pages). Finished fibers go back
//! to a per-thread pool and are reused, so starting a coroutine is cheap.
//! Elsewhere `supported()` is false and callers fall back to running bodies
//! eagerly.
//!
//! The interpreter is shared by raw pointer between fibers; only one fiber
//! of a thread runs at a time, so accesses never overlap.

use super::{Value, R};
use std::cell::RefCell;
use std::ffi::c_void;

/// How a suspended coroutine is resumed.
pub enum Resume {
    Next(Value),
    Throw(Value),
    Return(Value),
}

/// Why a coroutine handed control back.
pub enum Out {
    /// `yield v`.
    Yield(Value),
    /// `await`: it will be resumed by a promise reaction.
    Await,
    /// The body finished (returned or threw).
    Done(R<Value>),
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn ConvertThreadToFiber(p: *mut c_void) -> *mut c_void;
        pub fn CreateFiberEx(commit: usize, reserve: usize, flags: u32, start: extern "system" fn(*mut c_void), p: *mut c_void) -> *mut c_void;
        pub fn SwitchToFiber(f: *mut c_void);
        pub fn DeleteFiber(f: *mut c_void);
    }
}

/// Virtual stack reserved per coroutine (committed on demand).
const STACK_RESERVE: usize = 128 << 20;
/// Nesting depth of JS calls allowed inside one coroutine.
pub const MAX_DEPTH: u32 = 2500;

type Job = Box<dyn FnOnce(Resume) -> R<Value>>;

struct Ctx {
    fiber: *mut c_void,
    /// The fiber that resumed us (where suspend / finish switch back to).
    caller: *mut c_void,
    job: Option<Job>,
    resume: Option<Resume>,
    out: Option<Out>,
}

thread_local! {
    static MAIN: RefCell<*mut c_void> = const { RefCell::new(std::ptr::null_mut()) };
    static POOL: RefCell<Vec<*mut Ctx>> = const { RefCell::new(Vec::new()) };
    /// Running coroutines, innermost last.
    static RUNNING: RefCell<Vec<*mut Ctx>> = const { RefCell::new(Vec::new()) };
}

pub fn supported() -> bool {
    cfg!(windows)
}

#[cfg(windows)]
fn main_fiber() -> *mut c_void {
    MAIN.with(|m| {
        let mut m = m.borrow_mut();
        if m.is_null() {
            *m = unsafe { sys::ConvertThreadToFiber(std::ptr::null_mut()) };
        }
        *m
    })
}

#[cfg(windows)]
extern "system" fn fiber_main(p: *mut c_void) {
    let ctx = p as *mut Ctx;
    loop {
        // SAFETY: the context outlives its fiber; only this fiber touches it
        // while it runs.
        let (job, first) = unsafe { ((*ctx).job.take(), (*ctx).resume.take()) };
        let r = match (job, first) {
            (Some(job), Some(first)) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(first)))
                .unwrap_or_else(|_| Err(Value::str("InternalError: panic in coroutine"))),
            _ => Ok(Value::Undef),
        };
        unsafe {
            (*ctx).out = Some(Out::Done(r));
            sys::SwitchToFiber((*ctx).caller);
        }
    }
}

/// A coroutine that has not finished yet.
pub struct Co {
    ctx: *mut Ctx,
}

impl Co {
    /// Create a coroutine; `job` runs (with the first `Resume`) on the first `resume`.
    #[cfg(windows)]
    pub fn new(job: Job) -> Option<Co> {
        main_fiber();
        let ctx = POOL.with(|p| p.borrow_mut().pop()).or_else(|| {
            let ctx = Box::into_raw(Box::new(Ctx { fiber: std::ptr::null_mut(), caller: std::ptr::null_mut(), job: None, resume: None, out: None }));
            let f = unsafe { sys::CreateFiberEx(64 << 10, STACK_RESERVE, 0, fiber_main, ctx as *mut c_void) };
            if f.is_null() {
                drop(unsafe { Box::from_raw(ctx) });
                return None;
            }
            unsafe { (*ctx).fiber = f };
            Some(ctx)
        })?;
        unsafe { (*ctx).job = Some(job) };
        Some(Co { ctx })
    }

    #[cfg(not(windows))]
    pub fn new(_job: Job) -> Option<Co> {
        None
    }

    /// Run the coroutine until it yields, awaits or finishes.
    #[cfg(windows)]
    pub fn resume(&self, r: Resume) -> Out {
        let ctx = self.ctx;
        let me = RUNNING.with(|s| s.borrow().last().map(|&c| unsafe { (*c).fiber })).unwrap_or_else(main_fiber);
        unsafe {
            (*ctx).resume = Some(r);
            (*ctx).caller = me;
        }
        RUNNING.with(|s| s.borrow_mut().push(ctx));
        unsafe { sys::SwitchToFiber((*ctx).fiber) };
        RUNNING.with(|s| s.borrow_mut().pop());
        unsafe { (*ctx).out.take() }.unwrap_or(Out::Done(Ok(Value::Undef)))
    }

    #[cfg(not(windows))]
    pub fn resume(&self, _r: Resume) -> Out {
        Out::Done(Ok(Value::Undef))
    }

    /// Return a finished coroutine's fiber to the pool.
    pub fn recycle(self) {
        let ctx = self.ctx;
        std::mem::forget(self);
        POOL.with(|p| p.borrow_mut().push(ctx));
    }
}

impl Drop for Co {
    /// An abandoned (never finished) coroutine: free its fiber. Values on its
    /// stack are leaked rather than dropped.
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let running = RUNNING.with(|s| s.borrow().contains(&self.ctx));
            if running {
                return; // can't delete a fiber that is executing; leak it
            }
            sys::DeleteFiber((*self.ctx).fiber);
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// Suspend the innermost running coroutine, handing `out` to whoever resumed
/// it; returns how it was resumed.
#[cfg(windows)]
pub fn suspend(out: Out) -> Resume {
    let ctx = RUNNING.with(|s| *s.borrow().last().expect("suspend outside a coroutine"));
    unsafe {
        (*ctx).out = Some(out);
        sys::SwitchToFiber((*ctx).caller);
        (*ctx).resume.take().unwrap_or(Resume::Next(Value::Undef))
    }
}

#[cfg(not(windows))]
pub fn suspend(_out: Out) -> Resume {
    Resume::Next(Value::Undef)
}
