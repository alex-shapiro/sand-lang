//! Structural equality, ordering and hashing of values. All algorithms are
//! iterative so that deeply nested values cannot overflow the host stack.

use super::bytecode::Program;
use super::heap::Heap;
use super::value::*;
use std::cmp::Ordering;
use std::collections::HashSet;

pub const MAX_DEPTH: usize = 2000;

fn is_mutable_container(o: &Obj) -> bool {
    matches!(o, Obj::List(_) | Obj::Map(_) | Obj::Set(_) | Obj::Struct(..))
}

/// Deep structural equality. Floats at the top level use IEEE semantics;
/// inside composites NaN equals NaN.
pub fn values_eq(heap: &Heap, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => return x == y,
        (Value::Obj(x), Value::Obj(y)) if x == y => return true,
        _ => {}
    }
    let mut stack: Vec<(Value, Value)> = vec![(a, b)];
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    while let Some((a, b)) = stack.pop() {
        match (a, b) {
            (Value::Unit, Value::Unit) => {}
            (Value::Bool(x), Value::Bool(y)) => {
                if x != y {
                    return false;
                }
            }
            (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => {
                if x != y {
                    return false;
                }
            }
            (Value::Float(x), Value::Float(y)) => {
                if !(x == y || (x.is_nan() && y.is_nan())) {
                    return false;
                }
            }
            (Value::Tag(x), Value::Tag(y)) => {
                if x != y {
                    return false;
                }
            }
            (Value::Obj(x), Value::Obj(y)) => {
                if x == y {
                    continue;
                }
                let (ox, oy) = (heap.get(a), heap.get(b));
                if is_mutable_container(ox) && !seen.insert((x, y)) {
                    // Already being compared (cycle): assume equal.
                    continue;
                }
                match (ox, oy) {
                    (Obj::Str(p), Obj::Str(q)) => {
                        if p != q {
                            return false;
                        }
                    }
                    (Obj::Bytes(p), Obj::Bytes(q)) => {
                        if p != q {
                            return false;
                        }
                    }
                    (Obj::List(p), Obj::List(q)) => {
                        if p.len() != q.len() {
                            return false;
                        }
                        stack.extend(p.iter().copied().zip(q.iter().copied()));
                    }
                    (Obj::Tuple(p), Obj::Tuple(q)) => {
                        if p.len() != q.len() {
                            return false;
                        }
                        stack.extend(p.iter().copied().zip(q.iter().copied()));
                    }
                    (Obj::Struct(d1, p), Obj::Struct(d2, q)) | (Obj::Variant(d1, p), Obj::Variant(d2, q)) => {
                        if d1 != d2 || p.len() != q.len() {
                            return false;
                        }
                        stack.extend(p.iter().copied().zip(q.iter().copied()));
                    }
                    (Obj::Range(a1, b1, i1), Obj::Range(a2, b2, i2)) => {
                        if (a1, b1, i1) != (a2, b2, i2) {
                            return false;
                        }
                    }
                    (Obj::Map(p), Obj::Map(q)) => {
                        if p.live != q.live {
                            return false;
                        }
                        // Order-insensitive: look up each key of p in q.
                        for (k, v) in p.entries.iter().flatten() {
                            match map_lookup_value(heap, q, *k) {
                                Some(qv) => stack.push((*v, qv)),
                                None => return false,
                            }
                        }
                    }
                    (Obj::Set(p), Obj::Set(q)) => {
                        if p.live != q.live {
                            return false;
                        }
                        for k in p.entries.iter().flatten() {
                            if set_lookup(heap, q, *k).is_none() {
                                return false;
                            }
                        }
                    }
                    // Identity types.
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    true
}

fn map_lookup_value(heap: &Heap, m: &MapObj, k: Value) -> Option<Value> {
    let slot = with_keyref(heap, k, |kr| m.index.get(kr)).ok().flatten()?;
    m.entries[slot].map(|(_, v)| v)
}

fn set_lookup(heap: &Heap, s: &SetObj, k: Value) -> Option<usize> {
    with_keyref(heap, k, |kr| s.index.get(kr)).ok().flatten()
}

/// Calls `f` with a borrowed key for `k`.
pub fn with_keyref<R>(heap: &Heap, k: Value, f: impl FnOnce(&KeyRef) -> R) -> Result<R, String> {
    match k {
        Value::Int(i) => Ok(f(&KeyRef::Int(i))),
        Value::Obj(_) => {
            if let Obj::Str(s) = heap.get(k) {
                return Ok(f(&KeyRef::Str(s)));
            }
            let h = hkey(heap, k)?;
            Ok(f(&KeyRef::Other(h)))
        }
        other => {
            let h = hkey(heap, other)?;
            Ok(f(&KeyRef::Other(h)))
        }
    }
}

/// An owned hashable key for a value.
pub fn hkey(heap: &Heap, v: Value) -> Result<HKey, String> {
    hkey_depth(heap, v, 0)
}

fn hkey_depth(heap: &Heap, v: Value, depth: usize) -> Result<HKey, String> {
    if depth > 200 {
        // Deep keys are hashed iteratively.
        return hkey_iter(heap, v);
    }
    Ok(match v {
        Value::Unit => HKey::Unit,
        Value::Bool(b) => HKey::Bool(b),
        Value::Int(i) => HKey::Int(i),
        Value::Dur(d) => HKey::Dur(d),
        Value::Float(f) => HKey::Int(f.to_bits() as i64),
        Value::Tag(t) => HKey::Tag(t),
        Value::Uninit => HKey::Unit,
        Value::Obj(h) => match heap.get(v) {
            Obj::Str(s) => HKey::Str(s.clone()),
            Obj::Bytes(b) => HKey::Bytes(b.clone()),
            Obj::Tuple(t) => HKey::Tuple(t.iter().map(|x| hkey_depth(heap, *x, depth + 1)).collect::<Result<_, _>>()?),
            Obj::Variant(id, t) => HKey::Variant(*id, t.iter().map(|x| hkey_depth(heap, *x, depth + 1)).collect::<Result<_, _>>()?),
            Obj::Struct(id, t) => HKey::Struct(*id, t.iter().map(|x| hkey_depth(heap, *x, depth + 1)).collect::<Result<_, _>>()?),
            Obj::Range(a, b, i) => HKey::Range(*a, *b, *i),
            _ => HKey::Ident(h),
        },
    })
}

/// Iterative key construction with a depth limit.
fn hkey_iter(heap: &Heap, v: Value) -> Result<HKey, String> {
    // Build bottom-up using an explicit stack of frames.
    enum Frame {
        Enter(Value, usize),
        Build(u8, u32, usize),
    }
    let mut work = vec![Frame::Enter(v, 0)];
    let mut out: Vec<HKey> = Vec::new();
    while let Some(fr) = work.pop() {
        match fr {
            Frame::Enter(v, depth) => {
                if depth > MAX_DEPTH {
                    return Err("value too deeply nested to hash".into());
                }
                match v {
                    Value::Obj(_) => match heap.get(v) {
                        Obj::Tuple(t) => {
                            work.push(Frame::Build(0, 0, t.len()));
                            for x in t.iter().rev() {
                                work.push(Frame::Enter(*x, depth + 1));
                            }
                        }
                        Obj::Variant(id, t) => {
                            work.push(Frame::Build(1, *id, t.len()));
                            for x in t.iter().rev() {
                                work.push(Frame::Enter(*x, depth + 1));
                            }
                        }
                        Obj::Struct(id, t) => {
                            work.push(Frame::Build(2, *id, t.len()));
                            for x in t.iter().rev() {
                                work.push(Frame::Enter(*x, depth + 1));
                            }
                        }
                        _ => out.push(hkey_depth(heap, v, 0)?),
                    },
                    other => out.push(hkey_depth(heap, other, 0)?),
                }
            }
            Frame::Build(kind, id, n) => {
                let items: Box<[HKey]> = out.split_off(out.len() - n).into_boxed_slice();
                out.push(match kind {
                    0 => HKey::Tuple(items),
                    1 => HKey::Variant(id, items),
                    _ => HKey::Struct(id, items),
                });
            }
        }
    }
    Ok(out.pop().unwrap())
}

fn total_cmp_f(x: f64, y: f64) -> Ordering {
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => x.partial_cmp(&y).unwrap(),
    }
}

fn variant_index(prog: &Program, heap: &Heap, v: Value) -> (u32, &'static [Value]) {
    let id = heap.variant_id(v);
    let idx = prog.variants.get(id as usize).map(|r| r.index).unwrap_or(0);
    (idx, &[])
}

/// Total ordering used by `sort` and comparisons of composite values.
pub fn values_cmp(prog: &Program, heap: &Heap, a: Value, b: Value) -> Ordering {
    let mut stack: Vec<(Value, Value)> = vec![(a, b)];
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    // Comparisons are lexicographic: we process pairs in order and stop at the
    // first difference. The stack holds pairs in reverse order of significance.
    while let Some((a, b)) = stack.pop() {
        let ord = match (a, b) {
            (Value::Unit, Value::Unit) => Ordering::Equal,
            (Value::Bool(x), Value::Bool(y)) => x.cmp(&y),
            (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => x.cmp(&y),
            (Value::Float(x), Value::Float(y)) => total_cmp_f(x, y),
            (Value::Tag(_) | Value::Obj(_), Value::Tag(_) | Value::Obj(_)) => {
                let is_variant = |v: Value| matches!(v, Value::Tag(_)) || matches!(heap.get(v), Obj::Variant(..));
                if is_variant(a) && is_variant(b) {
                    let (ia, _) = variant_index(prog, heap, a);
                    let (ib, _) = variant_index(prog, heap, b);
                    if ia != ib {
                        ia.cmp(&ib)
                    } else {
                        let (fa, fb) = (heap.fields(a), heap.fields(b));
                        for (x, y) in fa.iter().zip(fb.iter()).rev() {
                            stack.push((*x, *y));
                        }
                        Ordering::Equal
                    }
                } else if let (Value::Obj(x), Value::Obj(y)) = (a, b) {
                    if x == y {
                        Ordering::Equal
                    } else {
                        let (ox, oy) = (heap.get(a), heap.get(b));
                        if is_mutable_container(ox) && !seen.insert((x, y)) {
                            Ordering::Equal
                        } else {
                            match (ox, oy) {
                                (Obj::Str(p), Obj::Str(q)) => p.as_bytes().cmp(q.as_bytes()),
                                (Obj::Bytes(p), Obj::Bytes(q)) => p.cmp(q),
                                (Obj::List(p), Obj::List(q)) => {
                                    // Push the length comparison as the least significant step.
                                    stack.push((Value::Int(p.len() as i64), Value::Int(q.len() as i64)));
                                    for (x, y) in p.iter().zip(q.iter()).rev() {
                                        stack.push((*x, *y));
                                    }
                                    Ordering::Equal
                                }
                                (Obj::Tuple(p), Obj::Tuple(q)) | (Obj::Struct(_, p), Obj::Struct(_, q)) => {
                                    for (x, y) in p.iter().zip(q.iter()).rev() {
                                        stack.push((*x, *y));
                                    }
                                    Ordering::Equal
                                }
                                (Obj::Range(a1, b1, _), Obj::Range(a2, b2, _)) => (a1, b1).cmp(&(a2, b2)),
                                _ => x.cmp(&y),
                            }
                        }
                    }
                } else {
                    Ordering::Equal
                }
            }
            _ => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// `a < b` for generic values (IEEE semantics for top-level floats).
pub fn values_lt(prog: &Program, heap: &Heap, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x < y,
        _ => values_cmp(prog, heap, a, b) == Ordering::Less,
    }
}

pub fn values_le(prog: &Program, heap: &Heap, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x <= y,
        _ => values_cmp(prog, heap, a, b) != Ordering::Greater,
    }
}
