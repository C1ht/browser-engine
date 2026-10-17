//! Object property storage with hidden classes ("shapes").
//!
//! An object's keys live in a shape shared by every object that got the same
//! keys in the same order (objects from one constructor or one object
//! literal), and the values in a per-object vector in slot order. Adding a
//! key moves the object along a cached transition to the next shape, so
//! property lookups can be cached per bytecode instruction: "shape id N →
//! slot i" (see `vm.rs`). Objects with many keys or deleted keys switch to a
//! private "dictionary" shape with a hash index.

use super::Prop;
use crate::fx::FxHashMap;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};

/// Keys beyond which an object gets its own dictionary shape.
const MAX_SHARED_KEYS: usize = 64;
/// Shapes with more keys than this get a hash index.
const LINEAR_KEYS: usize = 8;

thread_local! {
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    /// Ordinary objects, arrays, and everything else (DOM wrappers,
    /// proxies, ...) start from different roots, so equal shape ids imply
    /// the same kind class: inline caches needn't check the object kind.
    static ROOT: Rc<Shape> = Rc::new(Shape::new(Vec::new(), true));
    static ARRAY_ROOT: Rc<Shape> = Rc::new(Shape::new(Vec::new(), true));
    static SPECIAL_ROOT: Rc<Shape> = Rc::new(Shape::new(Vec::new(), true));
}

/// Which shape tree an object's properties belong to.
#[derive(Clone, Copy, PartialEq)]
pub enum ShapeClass {
    Ordinary,
    Array,
    Special,
}

fn next_id() -> u64 {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

#[repr(C)]
pub struct Shape {
    /// Unique per shape (and per version of a dictionary shape): the key
    /// inline caches compare.
    pub id: u64,
    keys: Vec<Rc<str>>,
    index: OnceCell<FxHashMap<Rc<str>, u32>>,
    /// Shared shapes: the shape reached by adding each key.
    transitions: RefCell<FxHashMap<Rc<str>, Weak<Shape>>>,
    shared: bool,
}

impl Shape {
    fn new(keys: Vec<Rc<str>>, shared: bool) -> Shape {
        Shape { id: next_id(), keys, index: OnceCell::new(), transitions: RefCell::new(FxHashMap::default()), shared }
    }

    #[inline]
    pub fn find(&self, k: &str) -> Option<usize> {
        if self.keys.len() <= LINEAR_KEYS {
            // Interned names usually share the pointer.
            let p = k.as_ptr();
            for (i, n) in self.keys.iter().enumerate() {
                if n.as_ptr() == p && n.len() == k.len() {
                    return Some(i);
                }
            }
            return self.keys.iter().position(|n| &**n == k);
        }
        self.index.get_or_init(|| self.keys.iter().enumerate().map(|(i, k)| (k.clone(), i as u32)).collect()).get(k).map(|&i| i as usize)
    }

    pub fn is_shared(&self) -> bool {
        self.shared
    }

    /// The shared shape with `k` added.
    fn transition(self: &Rc<Shape>, k: &Rc<str>) -> Rc<Shape> {
        if let Some(s) = self.transitions.borrow().get(k).and_then(|w| w.upgrade()) {
            return s;
        }
        let mut keys = Vec::with_capacity(self.keys.len() + 1);
        keys.extend(self.keys.iter().cloned());
        keys.push(k.clone());
        let s = Rc::new(Shape::new(keys, true));
        let mut t = self.transitions.borrow_mut();
        if t.len() > 64 {
            t.retain(|_, w| w.strong_count() > 0);
        }
        t.insert(k.clone(), Rc::downgrade(&s));
        s
    }
}

pub struct Props {
    shape: Rc<Shape>,
    vals: Vec<Prop>,
}

impl Default for Props {
    fn default() -> Props {
        Props { shape: ROOT.with(|r| r.clone()), vals: Vec::new() }
    }
}

impl Clone for Props {
    fn clone(&self) -> Props {
        let shape = if self.shape.shared { self.shape.clone() } else { Rc::new(Shape::new(self.shape.keys.clone(), false)) };
        Props { shape, vals: self.vals.clone() }
    }
}

impl Props {
    pub fn new(class: ShapeClass) -> Props {
        let shape = match class {
            ShapeClass::Ordinary => ROOT.with(|r| r.clone()),
            ShapeClass::Array => ARRAY_ROOT.with(|r| r.clone()),
            ShapeClass::Special => SPECIAL_ROOT.with(|r| r.clone()),
        };
        Props { shape, vals: Vec::new() }
    }

    /// The same entries in another shape tree (an object changing kind).
    pub fn reclass(self, class: ShapeClass) -> Props {
        let mut p = Props::new(class);
        for (k, v) in self.into_entries() {
            p.insert(k, v);
        }
        p
    }

    /// Field offsets for machine code: (shape, values vector).
    pub fn offsets() -> (usize, usize) {
        (std::mem::offset_of!(Props, shape), std::mem::offset_of!(Props, vals))
    }

    #[inline]
    pub fn shape_id(&self) -> u64 {
        self.shape.id
    }

    #[inline]
    pub fn shape(&self) -> &Rc<Shape> {
        &self.shape
    }

    #[inline]
    pub fn find(&self, k: &str) -> Option<usize> {
        self.shape.find(k)
    }

    #[inline]
    pub fn get(&self, k: &str) -> Option<&Prop> {
        self.find(k).map(|i| &self.vals[i])
    }

    #[inline]
    pub fn get_mut(&mut self, k: &str) -> Option<&mut Prop> {
        self.find(k).map(move |i| &mut self.vals[i])
    }

    #[inline]
    pub fn slot(&self, i: usize) -> &Prop {
        &self.vals[i]
    }

    #[inline]
    pub fn slot_mut(&mut self, i: usize) -> &mut Prop {
        &mut self.vals[i]
    }

    pub fn len(&self) -> usize {
        self.vals.len()
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &Prop)> {
        self.shape.keys.iter().zip(self.vals.iter())
    }

    pub fn into_entries(self) -> impl Iterator<Item = (Rc<str>, Prop)> {
        let keys = self.shape.keys.clone();
        keys.into_iter().zip(self.vals)
    }

    /// This object's own dictionary shape (mutable in place), with a new id.
    fn dict(&mut self) -> &mut Shape {
        let fresh = !self.shape.shared && Rc::get_mut(&mut self.shape).is_some();
        if !fresh {
            self.shape = Rc::new(Shape::new(self.shape.keys.clone(), false));
        }
        let s = Rc::get_mut(&mut self.shape).unwrap();
        s.id = next_id();
        s
    }

    pub fn insert(&mut self, k: Rc<str>, p: Prop) {
        if let Some(i) = self.find(&k) {
            self.vals[i] = p;
            return;
        }
        self.push_new(k, p);
    }

    /// Add a key known to be absent.
    pub fn push_new(&mut self, k: Rc<str>, p: Prop) {
        if self.shape.shared && self.vals.len() < MAX_SHARED_KEYS {
            self.shape = self.shape.transition(&k);
        } else {
            let s = self.dict();
            let i = s.keys.len() as u32;
            if let Some(ix) = s.index.get_mut() {
                ix.insert(k.clone(), i);
            }
            s.keys.push(k);
        }
        self.vals.push(p);
    }

    /// Move to an already-computed transition (inline-cached adds).
    #[inline]
    pub fn push_with_shape(&mut self, shape: Rc<Shape>, p: Prop) {
        debug_assert_eq!(shape.keys.len(), self.vals.len() + 1);
        self.shape = shape;
        self.vals.push(p);
    }

    pub fn remove(&mut self, k: &str) -> bool {
        let Some(i) = self.find(k) else { return false };
        let s = self.dict();
        s.keys.remove(i);
        s.index = OnceCell::new();
        self.vals.remove(i);
        true
    }
}
