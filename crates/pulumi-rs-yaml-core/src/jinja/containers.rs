// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Lists and dicts a template can change, with Python's aliasing.
//!
//! minijinja's values are immutable, so `{% do xs.append(1) %}` had nothing
//! to act on. A template that calls a mutating method has its list and dict
//! literals -- and `dict(...)`, `x | list` -- built as the objects below
//! instead (see `prepass`). They are shared, as Python's are:
//! `{% set b = a %}{% do a.append(1) %}` and `b` sees the 1.
//!
//! Values the host hands in (config, context) and values filters return
//! stay read-only; mutating one is a typed error that says to copy it
//! (`x.copy()` or `x | list`), matching the read-only intent of a
//! program's inputs.
//!
//! Changing a container while a loop iterates it is an error (Python allows
//! it for lists; here it would be an unbounded loop or a silent skip).
//!
//! minijinja requires objects to be `Send + Sync` and holds them in an
//! `Arc`: that `Arc`, and the boxed iterator its `Enumerator` takes, are the
//! library boundary and the only ones here. Interior mutability is a
//! `std::sync::Mutex`, uncontended because a render is single-threaded: an
//! uncontended lock is an atomic swap, measured in the render benches.
//! A dict keeps its keys sorted, like every map literal in this engine.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use minijinja::value::{Enumerator, Object, ObjectRepr, Value, ValueKind};
use minijinja::{Error, ErrorKind, State};

use super::methods::{type_error, value_error, Call};

/// The global functions the prepass wraps literals in.
pub(super) const LIST_FN: &str = "__rsyaml_list";
pub(super) const DICT_FN: &str = "__rsyaml_dict";
pub(super) const OWN_FN: &str = "__rsyaml_own";

/// Every mutating method, list's and dict's.
pub(super) const MUTATORS: &[&str] = &[
    "append",
    "clear",
    "extend",
    "insert",
    "pop",
    "popitem",
    "remove",
    "reverse",
    "setdefault",
    "sort",
    "update",
];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn changed_during_iteration(kind: &str) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("{kind} changed during iteration: a loop over it is still running"),
    )
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// A list the template built and may change.
pub(super) struct PyList {
    items: Mutex<Vec<Value>>,
    /// Loops currently iterating this list.
    iterating: AtomicUsize,
}

impl fmt::Debug for PyList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(lock(&self.items).iter()).finish()
    }
}

pub(super) fn new_list(items: Vec<Value>) -> Value {
    Value::from_object(PyList {
        items: Mutex::new(items),
        iterating: AtomicUsize::new(0),
    })
}

/// A loop's view of a list: by index, live, holding the list against
/// changes until the loop ends.
struct ListIter {
    list: Arc<PyList>,
    at: usize,
}

impl Iterator for ListIter {
    type Item = Value;
    fn next(&mut self) -> Option<Value> {
        let v = lock(&self.list.items).get(self.at).cloned();
        self.at += 1;
        v
    }
}

impl Drop for ListIter {
    fn drop(&mut self) {
        self.list.iterating.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Python's index normalisation; `None` when out of range.
fn index(len: usize, i: i64) -> Option<usize> {
    let i = if i < 0 { i + len as i64 } else { i };
    (0..len as i64).contains(&i).then_some(i as usize)
}

/// Whether every value can be ordered against every other the way Python
/// allows: all numbers, all strings, or all sequences.
fn comparable(items: &[Value]) -> bool {
    let class = |v: &Value| match v.kind() {
        ValueKind::Number | ValueKind::Bool => 0,
        ValueKind::String => 1,
        ValueKind::Seq | ValueKind::Iterable => 2,
        _ => 3,
    };
    match items.first() {
        None => true,
        Some(first) => {
            let c = class(first);
            c != 3 && items.iter().all(|v| class(v) == c)
        }
    }
}

impl PyList {
    fn mutate(&self) -> Result<MutexGuard<'_, Vec<Value>>, Error> {
        if self.iterating.load(Ordering::Relaxed) > 0 {
            return Err(changed_during_iteration("list"));
        }
        Ok(lock(&self.items))
    }
}

impl Object for PyList {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let items = lock(&self.items);
        let i = i64::try_from(key.clone()).ok()?;
        index(items.len(), i).map(|i| items[i].clone())
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.iterating.fetch_add(1, Ordering::Relaxed);
        Enumerator::Iter(Box::new(ListIter {
            list: Arc::clone(self),
            at: 0,
        }))
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(lock(&self.items).len())
    }

    fn call_method(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let c = Call::new(state, args)?;
        let none = Value::from(());
        match method {
            "append" => {
                let x = c
                    .get(0, "object")?
                    .ok_or_else(|| missing("append", "object"))?;
                c.done("append", 1)?;
                self.mutate()?.push(x);
                Ok(none)
            }
            "extend" => {
                let it = c
                    .get(0, "iterable")?
                    .ok_or_else(|| missing("extend", "iterable"))?;
                c.done("extend", 1)?;
                let new: Vec<Value> = it.try_iter()?.collect();
                self.mutate()?.extend(new);
                Ok(none)
            }
            "insert" => {
                let i = c
                    .get(0, "index")?
                    .ok_or_else(|| missing("insert", "index"))?;
                let x = c
                    .get(1, "object")?
                    .ok_or_else(|| missing("insert", "object"))?;
                c.done("insert", 2)?;
                let i =
                    i64::try_from(i).map_err(|_| type_error("insert: index must be an integer"))?;
                let mut items = self.mutate()?;
                let len = items.len() as i64;
                let at = if i < 0 { (i + len).max(0) } else { i.min(len) } as usize;
                items.insert(at, x);
                Ok(none)
            }
            "pop" => {
                let i = c.get(0, "index")?;
                c.done("pop", 1)?;
                let mut items = self.mutate()?;
                if items.is_empty() {
                    return Err(Error::new(
                        ErrorKind::InvalidOperation,
                        "pop from empty list",
                    ));
                }
                let i = match i {
                    Some(v) => {
                        i64::try_from(v).map_err(|_| type_error("pop: index must be an integer"))?
                    }
                    None => -1,
                };
                let at = index(items.len(), i).ok_or_else(|| {
                    Error::new(ErrorKind::InvalidOperation, "pop index out of range")
                })?;
                Ok(items.remove(at))
            }
            "remove" => {
                let x = c
                    .get(0, "value")?
                    .ok_or_else(|| missing("remove", "value"))?;
                c.done("remove", 1)?;
                let mut items = self.mutate()?;
                let at = items
                    .iter()
                    .position(|v| *v == x)
                    .ok_or_else(|| value_error("list.remove(x): x not in list"))?;
                items.remove(at);
                Ok(none)
            }
            "clear" => {
                c.done("clear", 0)?;
                self.mutate()?.clear();
                Ok(none)
            }
            "reverse" => {
                c.done("reverse", 0)?;
                self.mutate()?.reverse();
                Ok(none)
            }
            "sort" => {
                let key = c.get(0, "key")?.filter(|k| !k.is_none());
                let reverse = c.get(1, "reverse")?.is_some_and(|r| r.is_true());
                c.done("sort", 0)?;
                let snapshot: Vec<Value> = lock(&self.items).clone();
                let keys: Vec<Value> = match &key {
                    Some(f) => snapshot
                        .iter()
                        .map(|v| f.call(state, std::slice::from_ref(v)))
                        .collect::<Result<_, _>>()?,
                    None => snapshot.clone(),
                };
                if !comparable(&keys) {
                    return Err(type_error("'<' not supported between the list's values"));
                }
                let mut order: Vec<usize> = (0..snapshot.len()).collect();
                // Stable, and reversal keeps equal items in place, as Python's.
                order.sort_by(|&a, &b| {
                    let o = keys[a].cmp(&keys[b]);
                    if reverse {
                        o.reverse()
                    } else {
                        o
                    }
                });
                let sorted: Vec<Value> = order.into_iter().map(|i| snapshot[i].clone()).collect();
                *self.mutate()? = sorted;
                Ok(none)
            }
            "index" => {
                let x = c
                    .get(0, "value")?
                    .ok_or_else(|| missing("index", "value"))?;
                c.done("index", 1)?;
                lock(&self.items)
                    .iter()
                    .position(|v| *v == x)
                    .map(|i| Value::from(i as i64))
                    .ok_or_else(|| value_error(format!("{x} is not in list")))
            }
            "count" => {
                let x = c
                    .get(0, "value")?
                    .ok_or_else(|| missing("count", "value"))?;
                c.done("count", 1)?;
                Ok(Value::from(
                    lock(&self.items).iter().filter(|v| **v == x).count(),
                ))
            }
            "copy" => {
                c.done("copy", 0)?;
                Ok(new_list(lock(&self.items).clone()))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

fn missing(method: &str, arg: &str) -> Error {
    Error::new(
        ErrorKind::MissingArgument,
        format!("{method}() missing required argument '{arg}'"),
    )
}

// ---------------------------------------------------------------------------
// dict
// ---------------------------------------------------------------------------

/// A dict the template built and may change.
pub(super) struct PyDict {
    items: Mutex<BTreeMap<Value, Value>>,
    iterating: AtomicUsize,
}

impl fmt::Debug for PyDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(lock(&self.items).iter()).finish()
    }
}

pub(super) fn new_dict(pairs: Vec<(Value, Value)>) -> Value {
    // Inserted, not collected: a map's FromIterator sorts its input with a
    // sort instantiated per iterator type; later keys win either way.
    let mut items = BTreeMap::new();
    for (k, v) in pairs {
        items.insert(k, v);
    }
    Value::from_object(PyDict {
        items: Mutex::new(items),
        iterating: AtomicUsize::new(0),
    })
}

struct DictIter {
    dict: Arc<PyDict>,
    keys: std::vec::IntoIter<Value>,
}

impl Iterator for DictIter {
    type Item = Value;
    fn next(&mut self) -> Option<Value> {
        self.keys.next()
    }
}

impl Drop for DictIter {
    fn drop(&mut self) {
        self.dict.iterating.fetch_sub(1, Ordering::Relaxed);
    }
}

impl PyDict {
    fn mutate(&self) -> Result<MutexGuard<'_, BTreeMap<Value, Value>>, Error> {
        if self.iterating.load(Ordering::Relaxed) > 0 {
            return Err(changed_during_iteration("dictionary"));
        }
        Ok(lock(&self.items))
    }
}

/// The pairs of a mapping, or of an iterable of pairs.
fn pairs_of(v: &Value) -> Result<Vec<(Value, Value)>, Error> {
    let mut out = Vec::new();
    if v.kind() == ValueKind::Map {
        for k in v.try_iter()? {
            let val = v.get_item(&k)?;
            out.push((k, val));
        }
    } else {
        for pair in v.try_iter()? {
            let (k, val) = (pair.get_item_by_index(0)?, pair.get_item_by_index(1)?);
            out.push((k, val));
        }
    }
    Ok(out)
}

impl Object for PyDict {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        lock(&self.items).get(key).cloned()
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.iterating.fetch_add(1, Ordering::Relaxed);
        let keys: Vec<Value> = lock(&self.items).keys().cloned().collect();
        Enumerator::Iter(Box::new(DictIter {
            dict: Arc::clone(self),
            keys: keys.into_iter(),
        }))
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(lock(&self.items).len())
    }

    fn call_method(
        self: &Arc<Self>,
        state: &State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let c = Call::new(state, args)?;
        let none = Value::from(());
        match method {
            "update" => {
                let other = c.get(0, "other")?;
                // Keyword arguments are entries too.
                let mut new = match other {
                    Some(o) => pairs_of(&o)?,
                    None => Vec::new(),
                };
                if c.args().len() > 1 {
                    return Err(Error::new(
                        ErrorKind::TooManyArguments,
                        "update expected at most 1 argument",
                    ));
                }
                if let Some(kw) = c.kwargs() {
                    for key in kw.args().map(str::to_string).collect::<Vec<_>>() {
                        let v: Value = kw.get(&key)?;
                        new.push((Value::from(key), v));
                    }
                }
                let mut items = self.mutate()?;
                items.extend(new);
                Ok(none)
            }
            "setdefault" => {
                let key = c
                    .get(0, "key")?
                    .ok_or_else(|| missing("setdefault", "key"))?;
                let default = c.get(1, "default")?.unwrap_or(Value::from(()));
                c.done("setdefault", 2)?;
                if let Some(v) = lock(&self.items).get(&key) {
                    return Ok(v.clone());
                }
                self.mutate()?.insert(key, default.clone());
                Ok(default)
            }
            "pop" => {
                let key = c.get(0, "key")?.ok_or_else(|| missing("pop", "key"))?;
                let default = c.get(1, "default")?;
                c.done("pop", 2)?;
                let mut items = self.mutate()?;
                match (items.remove(&key), default) {
                    (Some(v), _) => Ok(v),
                    (None, Some(d)) => Ok(d),
                    (None, None) => Err(Error::new(
                        ErrorKind::InvalidOperation,
                        format!("KeyError: {key}"),
                    )),
                }
            }
            "popitem" => {
                c.done("popitem", 0)?;
                let mut items = self.mutate()?;
                let (k, v) = items.pop_last().ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidOperation,
                        "popitem(): dictionary is empty",
                    )
                })?;
                Ok(Value::from(vec![k, v]))
            }
            "clear" => {
                c.done("clear", 0)?;
                self.mutate()?.clear();
                Ok(none)
            }
            "get" => {
                let key = c.get(0, "key")?.ok_or_else(|| missing("get", "key"))?;
                let default = c.get(1, "default")?;
                c.done("get", 2)?;
                Ok(lock(&self.items)
                    .get(&key)
                    .cloned()
                    .or(default)
                    .unwrap_or(none))
            }
            "items" => {
                c.done("items", 0)?;
                Ok(Value::from(
                    lock(&self.items)
                        .iter()
                        .map(|(k, v)| Value::from(vec![k.clone(), v.clone()]))
                        .collect::<Vec<_>>(),
                ))
            }
            "keys" => {
                c.done("keys", 0)?;
                Ok(Value::from(
                    lock(&self.items).keys().cloned().collect::<Vec<_>>(),
                ))
            }
            "values" => {
                c.done("values", 0)?;
                Ok(Value::from(
                    lock(&self.items).values().cloned().collect::<Vec<_>>(),
                ))
            }
            "copy" => {
                c.done("copy", 0)?;
                Ok(Value::from_object(PyDict {
                    items: Mutex::new(lock(&self.items).clone()),
                    iterating: AtomicUsize::new(0),
                }))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

// ---------------------------------------------------------------------------
// The wrapping functions
// ---------------------------------------------------------------------------

/// `__rsyaml_list(x)`: a fresh mutable list of `x`'s items.
pub(super) fn wrap_list(v: Value) -> Result<Value, Error> {
    if v.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    Ok(new_list(v.try_iter()?.collect()))
}

/// `__rsyaml_own(x)`: what a filter or `dict(...)` returned, as Python
/// would hold it -- a sequence or mapping becomes a fresh mutable list or
/// dict; anything else passes through untouched.
pub(super) fn own(v: Value) -> Result<Value, Error> {
    if v.downcast_object_ref::<PyList>().is_some() || v.downcast_object_ref::<PyDict>().is_some() {
        return Ok(v);
    }
    match v.kind() {
        ValueKind::Seq | ValueKind::Iterable => Ok(new_list(v.try_iter()?.collect())),
        ValueKind::Map => Ok(new_dict(pairs_of(&v)?)),
        _ => Ok(v),
    }
}

/// `__rsyaml_dict(x)`: a fresh mutable dict of `x`'s pairs.
pub(super) fn wrap_dict(v: Value) -> Result<Value, Error> {
    if v.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    Ok(new_dict(pairs_of(&v)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_index_normalisation() {
        assert_eq!(index(3, 0), Some(0));
        assert_eq!(index(3, -1), Some(2));
        assert_eq!(index(3, 3), None);
        assert_eq!(index(3, -4), None);
        assert_eq!(index(0, 0), None);
    }

    #[test]
    fn comparability_follows_python() {
        assert!(comparable(&[
            Value::from(1),
            Value::from(2.5),
            Value::from(true)
        ]));
        assert!(comparable(&[Value::from("a"), Value::from("b")]));
        assert!(!comparable(&[Value::from(1), Value::from("a")]));
        assert!(!comparable(&[Value::from(())]));
        assert!(comparable(&[]));
    }
}
