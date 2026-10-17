//! Insertion-ordered hash map keyed by SameValueZero: the storage behind
//! Map, Set, WeakMap and WeakSet.

use super::Value;
use crate::fx::FxHashMap;
use std::rc::Rc;

/// A Value reduced to what SameValueZero compares (objects by identity:
/// the map holds the key value, so its address stays valid).
#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Undef,
    Null,
    Bool(bool),
    Num(u64),
    BigInt(i128),
    Str(Rc<str>),
    Obj(usize),
}

fn key(v: &Value) -> Key {
    match v {
        Value::Undef | Value::Hole => Key::Undef,
        Value::Null => Key::Null,
        Value::Bool(b) => Key::Bool(*b),
        // -0 and +0 are the same key, as are all NaNs.
        Value::Num(n) if *n == 0.0 => Key::Num(0f64.to_bits()),
        Value::Num(n) if n.is_nan() => Key::Num(f64::NAN.to_bits()),
        Value::Num(n) => Key::Num(n.to_bits()),
        Value::BigInt(b) => Key::BigInt(b.get()),
        Value::Str(s) => Key::Str(s.clone()),
        Value::Obj(o) => Key::Obj(Rc::as_ptr(o) as *const () as usize),
    }
}

#[derive(Clone, Default)]
pub struct OrdMap {
    /// Entries in insertion order; deleted ones are tombstones.
    entries: Vec<Option<(Value, Value)>>,
    index: FxHashMap<Key, usize>,
}

impl OrdMap {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
    }

    pub fn get(&self, k: &Value) -> Option<&Value> {
        let i = *self.index.get(&key(k))?;
        self.entries[i].as_ref().map(|e| &e.1)
    }

    pub fn has(&self, k: &Value) -> bool {
        self.index.contains_key(&key(k))
    }

    pub fn set(&mut self, k: Value, v: Value) {
        let kk = key(&k);
        match self.index.get(&kk) {
            Some(&i) => {
                if let Some(e) = &mut self.entries[i] {
                    e.1 = v;
                }
            }
            None => {
                // -0 is stored as +0.
                let k = if matches!(k, Value::Num(n) if n == 0.0) { Value::Num(0.0) } else { k };
                self.index.insert(kk, self.entries.len());
                self.entries.push(Some((k, v)));
            }
        }
    }

    pub fn remove(&mut self, k: &Value) -> bool {
        let Some(i) = self.index.remove(&key(k)) else { return false };
        self.entries[i] = None;
        // Compact once most entries are tombstones.
        if self.entries.len() > 32 && self.index.len() * 2 < self.entries.len() {
            self.entries.retain(Option::is_some);
            for (j, e) in self.entries.iter().enumerate() {
                if let Some((k, _)) = e {
                    self.index.insert(key(k), j);
                }
            }
        }
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Value, Value)> {
        self.entries.iter().flatten()
    }

    pub fn to_vec(&self) -> Vec<(Value, Value)> {
        self.iter().cloned().collect()
    }

    pub fn keys(&self) -> Vec<Value> {
        self.iter().map(|(k, _)| k.clone()).collect()
    }
}
