//! Structural equality, ordering and hashing of values. All algorithms are
//! iterative so that deeply nested values cannot overflow the host stack,
//! walk sequences lazily (scratch space grows with nesting depth, not with
//! the size of the values), and charge a [`Budget`] one unit per node plus
//! the bytes of strings compared, so that comparing huge values costs fuel
//! and can be interrupted.

use super::Ctl;
use super::bytecode::Program;
use super::heap::Heap;
use super::meter::{Budget, Scratch, Unmetered, ctl_message, message_ctl};
use super::value::*;
use std::cmp::Ordering;
use std::collections::HashSet;

pub const MAX_DEPTH: usize = 2000;

fn is_mutable_container(o: &Obj) -> bool {
    matches!(o, Obj::List(_) | Obj::Map(_) | Obj::Set(_) | Obj::Struct(..))
}

/// The pairs of mutable containers a comparison has compared, so that
/// shared and cyclic structures are compared once.
struct Seen {
    pairs: HashSet<(u32, u32)>,
}

impl Seen {
    fn new() -> Seen {
        Seen { pairs: HashSet::new() }
    }

    /// Records `pair`; false if it was recorded already.
    fn insert(&mut self, heap: &Heap, pair: (u32, u32)) -> Result<bool, Ctl> {
        if self.pairs.len() == self.pairs.capacity() && heap.would_exceed(self.pairs.capacity().max(4).saturating_mul(3 * 12)) {
            return Err(Ctl::Limit(crate::LimitKind::Memory));
        }
        Ok(self.pairs.insert(pair))
    }
}

/// The elements of a list, or the fields of a tuple, struct or variant.
fn items(heap: &Heap, v: Value) -> &[Value] {
    match heap.get(v) {
        Obj::List(l) => l,
        Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) => t,
        _ => &[],
    }
}

/// Equality of two immediate (non-object) values; inside composites NaN
/// equals NaN.
fn scalar_eq(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
        (Value::Tag(x), Value::Tag(y)) => x == y,
        _ => false,
    }
}

/// Deep structural equality. Floats at the top level use IEEE semantics;
/// inside composites NaN equals NaN.
pub fn values_eq(heap: &Heap, a: Value, b: Value) -> bool {
    values_eq_metered(heap, &Unmetered, a, b).unwrap_or(false)
}

/// Pending work of an equality check.
enum EqWork {
    Pair(Value, Value),
    /// Elements `i..` of two sequences of equal length.
    Seq(Value, Value, usize),
    /// Entries `i..` of map `.0`, looked up in map `.1`.
    Map(Value, Value, usize),
}

/// [`values_eq`], charging `budget` for the work.
pub fn values_eq_metered(heap: &Heap, budget: &impl Budget, a: Value, b: Value) -> Result<bool, Ctl> {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => return Ok(x == y),
        (Value::Obj(x), Value::Obj(y)) if x == y => return Ok(true),
        (Value::Obj(_), Value::Obj(_)) => {
            if let (Obj::Str(p), Obj::Str(q)) = (heap.get(a), heap.get(b)) {
                budget.charge_bytes(p.len().min(q.len()))?;
                return Ok(p == q);
            }
        }
        (Value::Obj(_), _) | (_, Value::Obj(_)) => return Ok(false),
        // Inline single-payload variants compare their payloads below.
        _ if a.as_inline_variant().is_some() && b.as_inline_variant().is_some() => {}
        _ => return Ok(scalar_eq(a, b)),
    }
    let mut stack: Vec<EqWork> = vec![EqWork::Pair(a, b)];
    let mut seen = Seen::new();
    while let Some(w) = stack.pop() {
        let (a, b) = match w {
            EqWork::Pair(a, b) => (a, b),
            EqWork::Seq(x, y, i) => {
                let (p, q) = (items(heap, x), items(heap, y));
                if i >= p.len() || i >= q.len() {
                    continue;
                }
                stack.push(EqWork::Seq(x, y, i + 1));
                (p[i], q[i])
            }
            EqWork::Map(x, y, i) => {
                let (Obj::Map(p), Obj::Map(q)) = (heap.get(x), heap.get(y)) else { continue };
                let Some(j) = (i..p.entries.len()).find(|&j| p.entries[j].is_some()) else { continue };
                stack.push(EqWork::Map(x, y, j + 1));
                let (k, v) = p.entries[j].unwrap();
                match map_lookup_value(heap, budget, q, k)? {
                    Some(qv) => (v, qv),
                    None => return Ok(false),
                }
            }
        };
        budget.charge(1)?;
        // Variants are canonical: a variant is inline in both values or in neither.
        if let (Some((ia, pa)), Some((ib, pb))) = (a.as_inline_variant(), b.as_inline_variant()) {
            if ia != ib {
                return Ok(false);
            }
            stack.push(EqWork::Pair(pa, pb));
            continue;
        }
        let (Value::Obj(x), Value::Obj(y)) = (a, b) else {
            if !scalar_eq(a, b) {
                return Ok(false);
            }
            continue;
        };
        if x == y {
            continue;
        }
        let (ox, oy) = (heap.get(a), heap.get(b));
        if is_mutable_container(ox) && !seen.insert(heap, (x, y))? {
            // Already being compared (cycle): assume equal.
            continue;
        }
        match (ox, oy) {
            (Obj::Str(p), Obj::Str(q)) => {
                budget.charge_bytes(p.len().min(q.len()))?;
                if p != q {
                    return Ok(false);
                }
            }
            (Obj::Bytes(p), Obj::Bytes(q)) => {
                budget.charge_bytes(p.len().min(q.len()))?;
                if p != q {
                    return Ok(false);
                }
            }
            (Obj::List(p), Obj::List(q)) => {
                if p.len() != q.len() {
                    return Ok(false);
                }
                stack.push(EqWork::Seq(a, b, 0));
            }
            (Obj::Tuple(p), Obj::Tuple(q)) => {
                if p.len() != q.len() {
                    return Ok(false);
                }
                stack.push(EqWork::Seq(a, b, 0));
            }
            (Obj::Struct(d1, p), Obj::Struct(d2, q)) | (Obj::Variant(d1, p), Obj::Variant(d2, q)) => {
                if d1 != d2 || p.len() != q.len() {
                    return Ok(false);
                }
                stack.push(EqWork::Seq(a, b, 0));
            }
            (Obj::Range(a1, b1, i1), Obj::Range(a2, b2, i2)) => {
                if (a1, b1, i1) != (a2, b2, i2) {
                    return Ok(false);
                }
            }
            (Obj::Map(p), Obj::Map(q)) => {
                if p.live != q.live {
                    return Ok(false);
                }
                // Order-insensitive: look up each key of p in q.
                stack.push(EqWork::Map(a, b, 0));
            }
            (Obj::Set(p), Obj::Set(q)) => {
                if p.live != q.live {
                    return Ok(false);
                }
                for k in p.entries.iter().flatten() {
                    budget.charge(1)?;
                    if set_lookup(heap, budget, q, *k)?.is_none() {
                        return Ok(false);
                    }
                }
            }
            // Identity types.
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Turns a key lookup error into either a hard failure (limits) or "not
/// found" (unhashable keys).
fn lookup_failure(msg: String) -> Result<(), Ctl> {
    match message_ctl(&msg) {
        Some(c) => Err(c),
        None => Ok(()),
    }
}

fn map_lookup_value(heap: &Heap, budget: &impl Budget, m: &MapObj, k: Value) -> Result<Option<Value>, Ctl> {
    match with_keyref_metered(heap, budget, k, |kr| m.index.get(kr)) {
        Ok(slot) => Ok(slot.and_then(|s| m.entries[s].map(|(_, v)| v))),
        Err(e) => lookup_failure(e).map(|()| None),
    }
}

fn set_lookup(heap: &Heap, budget: &impl Budget, s: &SetObj, k: Value) -> Result<Option<usize>, Ctl> {
    match with_keyref_metered(heap, budget, k, |kr| s.index.get(kr)) {
        Ok(slot) => Ok(slot),
        Err(e) => lookup_failure(e).map(|()| None),
    }
}

/// Calls `f` with a borrowed key for `k`.
pub fn with_keyref<R>(heap: &Heap, k: Value, f: impl FnOnce(&KeyRef) -> R) -> Result<R, String> {
    with_keyref_metered(heap, &Unmetered, k, f)
}

/// [`with_keyref`], charging `budget` for hashing the key. Limit errors are
/// encoded as internal messages (see `meter::ctl_message`).
pub fn with_keyref_metered<R>(heap: &Heap, budget: &impl Budget, k: Value, f: impl FnOnce(&KeyRef) -> R) -> Result<R, String> {
    match k {
        Value::Int(i) => Ok(f(&KeyRef::Int(i))),
        Value::Obj(_) => {
            if let Obj::Str(s) = heap.get(k) {
                budget.charge_bytes(s.len()).map_err(ctl_message)?;
                return Ok(f(&KeyRef::Str(s)));
            }
            let h = hkey_metered(heap, budget, k)?;
            Ok(f(&KeyRef::Other(h)))
        }
        other => {
            let h = hkey_metered(heap, budget, other)?;
            Ok(f(&KeyRef::Other(h)))
        }
    }
}

/// An owned hashable key for a value.
pub fn hkey(heap: &Heap, v: Value) -> Result<HKey, String> {
    hkey_metered(heap, &Unmetered, v)
}

/// [`hkey`], charging `budget` one unit per node plus the bytes copied, and
/// checking the key's size against the memory limit as it is built (keys
/// of values with shared substructure can be much larger than the values).
pub fn hkey_metered(heap: &Heap, budget: &impl Budget, v: Value) -> Result<HKey, String> {
    hkey_sized(heap, budget, v).map(|(k, _)| k)
}

/// [`hkey_metered`], also returning the number of bytes the key owns
/// (strings and nested nodes; a key of shared substructure owns a copy per
/// occurrence).
pub fn hkey_sized(heap: &Heap, budget: &impl Budget, v: Value) -> Result<(HKey, usize), String> {
    let mut kb = KeyBuilder { heap, budget, size: 0, scratch: Scratch::new() };
    let k = kb.build(v, 0)?;
    Ok((k, kb.size))
}

struct KeyBuilder<'a, B> {
    heap: &'a Heap,
    budget: &'a B,
    /// Bytes allocated for the key so far.
    size: usize,
    scratch: Scratch,
}

impl<B: Budget> KeyBuilder<'_, B> {
    /// Accounts for a key node (plus `bytes` of copied data).
    fn node(&mut self, bytes: usize) -> Result<(), String> {
        self.budget.charge(1 + (bytes / super::meter::BYTES_PER_UNIT) as u64).map_err(ctl_message)?;
        self.size = self.size.saturating_add(std::mem::size_of::<HKey>() + bytes);
        self.scratch.check(self.heap, self.size).map_err(ctl_message)
    }

    fn build(&mut self, v: Value, depth: usize) -> Result<HKey, String> {
        if depth > 200 {
            // Deep keys are hashed iteratively.
            return self.build_iter(v);
        }
        Ok(match v {
            Value::Unit => HKey::Unit,
            Value::Bool(b) => HKey::Bool(b),
            Value::Int(i) => HKey::Int(i),
            Value::Dur(d) => HKey::Dur(d),
            Value::Float(f) => HKey::Int(f.to_bits() as i64),
            Value::Tag(t) => HKey::Tag(t),
            Value::Uninit => HKey::Unit,
            Value::VUnit(_)
            | Value::VBool(..)
            | Value::VInt(..)
            | Value::VFloat(..)
            | Value::VDur(..)
            | Value::VTag(..)
            | Value::VObj(..) => {
                let (id, p) = v.as_inline_variant().unwrap_or((u32::MAX, Value::Unit));
                self.node(std::mem::size_of::<HKey>())?;
                HKey::Variant(id, Box::new([self.build(p, depth + 1)?]))
            }
            Value::Obj(h) => {
                let heap = self.heap;
                match heap.get(v) {
                    Obj::Str(s) => {
                        self.node(s.len())?;
                        HKey::Str(s.clone())
                    }
                    Obj::Bytes(b) => {
                        self.node(b.len())?;
                        HKey::Bytes(b.clone())
                    }
                    Obj::Tuple(t) => HKey::Tuple(self.fields(t, depth)?),
                    Obj::Variant(id, t) => HKey::Variant(*id, self.fields(t, depth)?),
                    Obj::Struct(id, t) => HKey::Struct(*id, self.fields(t, depth)?),
                    Obj::Range(a, b, i) => HKey::Range(*a, *b, *i),
                    _ => HKey::Ident(h),
                }
            }
        })
    }

    fn fields(&mut self, t: &[Value], depth: usize) -> Result<Box<[HKey]>, String> {
        self.node(t.len() * std::mem::size_of::<HKey>())?;
        t.iter().map(|x| self.build(*x, depth + 1)).collect()
    }

    /// Iterative key construction with a depth limit.
    fn build_iter(&mut self, v: Value) -> Result<HKey, String> {
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
                    if let Some((id, p)) = v.as_inline_variant() {
                        self.node(std::mem::size_of::<HKey>())?;
                        work.push(Frame::Build(1, id, 1));
                        work.push(Frame::Enter(p, depth + 1));
                        continue;
                    }
                    let heap = self.heap;
                    let (kind, id, t) = match (v, heap.get(v)) {
                        (Value::Obj(_), Obj::Tuple(t)) => (0, 0, t),
                        (Value::Obj(_), Obj::Variant(id, t)) => (1, *id, t),
                        (Value::Obj(_), Obj::Struct(id, t)) => (2, *id, t),
                        _ => {
                            let k = self.build(v, 0)?;
                            out.push(k);
                            continue;
                        }
                    };
                    self.node(t.len() * std::mem::size_of::<HKey>())?;
                    work.push(Frame::Build(kind, id, t.len()));
                    for x in t.iter().rev() {
                        work.push(Frame::Enter(*x, depth + 1));
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
}

fn total_cmp_f(x: f64, y: f64) -> Ordering {
    match (x.is_nan(), y.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => x.partial_cmp(&y).unwrap(),
    }
}

/// Declaration order of a variant within its enum.
fn variant_index(prog: &Program, id: u32) -> u32 {
    prog.variants.get(id as usize).map(|r| r.index).unwrap_or(0)
}

/// Total ordering used by `sort` and comparisons of composite values.
pub fn values_cmp(prog: &Program, heap: &Heap, a: Value, b: Value) -> Ordering {
    values_cmp_metered(prog, heap, &Unmetered, a, b).unwrap_or(Ordering::Equal)
}

/// Pending work of an ordering comparison.
enum CmpWork {
    Pair(Value, Value),
    /// Elements `i..` of two sequences; lists then compare their lengths.
    Seq {
        a: Value,
        b: Value,
        i: usize,
        list: bool,
    },
}

/// [`values_cmp`], charging `budget` for the work. Comparisons are
/// lexicographic and stop at the first difference.
pub fn values_cmp_metered(prog: &Program, heap: &Heap, budget: &impl Budget, a: Value, b: Value) -> Result<Ordering, Ctl> {
    // Fast paths for the common element types of sorted lists.
    match (a, b) {
        (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => {
            budget.charge(1)?;
            return Ok(x.cmp(&y));
        }
        (Value::Float(x), Value::Float(y)) => {
            budget.charge(1)?;
            return Ok(total_cmp_f(x, y));
        }
        (Value::Bool(x), Value::Bool(y)) => {
            budget.charge(1)?;
            return Ok(x.cmp(&y));
        }
        (Value::Obj(_), Value::Obj(_)) => {
            if let (Obj::Str(p), Obj::Str(q)) = (heap.get(a), heap.get(b)) {
                budget.charge(1 + (p.len().min(q.len()) / super::meter::BYTES_PER_UNIT) as u64)?;
                return Ok(p.as_bytes().cmp(q.as_bytes()));
            }
        }
        _ => {}
    }
    let mut stack: Vec<CmpWork> = vec![CmpWork::Pair(a, b)];
    let mut seen = Seen::new();
    while let Some(w) = stack.pop() {
        let (a, b) = match w {
            CmpWork::Pair(a, b) => (a, b),
            CmpWork::Seq { a, b, i, list } => {
                let (p, q) = (items(heap, a), items(heap, b));
                if i < p.len() && i < q.len() {
                    stack.push(CmpWork::Seq { a, b, i: i + 1, list });
                    (p[i], q[i])
                } else {
                    if list && p.len() != q.len() {
                        return Ok(p.len().cmp(&q.len()));
                    }
                    continue;
                }
            }
        };
        budget.charge(1)?;
        let ord = match (a, b) {
            (Value::Unit, Value::Unit) => Ordering::Equal,
            (Value::Bool(x), Value::Bool(y)) => x.cmp(&y),
            (Value::Int(x), Value::Int(y)) | (Value::Dur(x), Value::Dur(y)) => x.cmp(&y),
            (Value::Float(x), Value::Float(y)) => total_cmp_f(x, y),
            _ if a.as_inline_variant().is_some() || b.as_inline_variant().is_some() => match (heap.variant(a), heap.variant(b)) {
                (Some(va), Some(vb)) => {
                    let (ia, ib) = (variant_index(prog, va.id()), variant_index(prog, vb.id()));
                    if ia == ib {
                        // One side may be inline and the other on the heap
                        // (`Some(None)` vs `Some(Some(1))`).
                        for (x, y) in va.fields().iter().zip(vb.fields()).rev() {
                            stack.push(CmpWork::Pair(*x, *y));
                        }
                    }
                    ia.cmp(&ib)
                }
                _ => Ordering::Equal,
            },
            (Value::Tag(_) | Value::Obj(_), Value::Tag(_) | Value::Obj(_)) => {
                let is_variant = |v: Value| matches!(v, Value::Tag(_)) || matches!(heap.get(v), Obj::Variant(..));
                if is_variant(a) && is_variant(b) {
                    let (ia, ib) = (variant_index(prog, heap.variant_id(a)), variant_index(prog, heap.variant_id(b)));
                    if ia == ib {
                        stack.push(CmpWork::Seq { a, b, i: 0, list: false });
                    }
                    ia.cmp(&ib)
                } else if let (Value::Obj(x), Value::Obj(y)) = (a, b) {
                    if x == y {
                        Ordering::Equal
                    } else {
                        let (ox, oy) = (heap.get(a), heap.get(b));
                        if is_mutable_container(ox) && !seen.insert(heap, (x, y))? {
                            Ordering::Equal
                        } else {
                            match (ox, oy) {
                                (Obj::Str(p), Obj::Str(q)) => {
                                    budget.charge_bytes(p.len().min(q.len()))?;
                                    p.as_bytes().cmp(q.as_bytes())
                                }
                                (Obj::Bytes(p), Obj::Bytes(q)) => {
                                    budget.charge_bytes(p.len().min(q.len()))?;
                                    p.cmp(q)
                                }
                                (Obj::List(_), Obj::List(_)) => {
                                    stack.push(CmpWork::Seq { a, b, i: 0, list: true });
                                    Ordering::Equal
                                }
                                (Obj::Tuple(_), Obj::Tuple(_)) | (Obj::Struct(..), Obj::Struct(..)) => {
                                    stack.push(CmpWork::Seq { a, b, i: 0, list: false });
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
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
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

/// [`values_lt`], charging `budget`.
pub fn values_lt_metered(prog: &Program, heap: &Heap, budget: &impl Budget, a: Value, b: Value) -> Result<bool, Ctl> {
    Ok(match (a, b) {
        (Value::Float(x), Value::Float(y)) => x < y,
        _ => values_cmp_metered(prog, heap, budget, a, b)? == Ordering::Less,
    })
}

/// [`values_le`], charging `budget`.
pub fn values_le_metered(prog: &Program, heap: &Heap, budget: &impl Budget, a: Value, b: Value) -> Result<bool, Ctl> {
    Ok(match (a, b) {
        (Value::Float(x), Value::Float(y)) => x <= y,
        _ => values_cmp_metered(prog, heap, budget, a, b)? != Ordering::Greater,
    })
}

/// Sorts a list whose elements are all ints, all floats or all durations
/// (the common case) without the generic comparator; `None` for other lists.
/// The work is charged to `budget` ahead of each step: chunks are sorted by
/// the standard library's sort and then merged pass by pass, so no
/// uninterruptible step is long and the limits are checked in between. On
/// an error `v` is left a permutation of its elements.
pub fn try_sort_numbers(v: &mut [Value], budget: &impl Budget) -> Option<Result<(), Ctl>> {
    match v.first()? {
        Value::Int(_) if v.iter().all(|x| matches!(x, Value::Int(_))) => {
            Some(sort_keys(v, budget, |x| x.as_int(), Value::Int, |a: &i64, b: &i64| a.cmp(b)))
        }
        Value::Dur(_) if v.iter().all(|x| matches!(x, Value::Dur(_))) => {
            Some(sort_keys(v, budget, |x| x.as_int(), Value::Dur, |a: &i64, b: &i64| a.cmp(b)))
        }
        Value::Float(_) if v.iter().all(|x| matches!(x, Value::Float(_))) => {
            Some(sort_keys(v, budget, |x| x.as_float(), Value::Float, |a: &f64, b: &f64| total_cmp_f(*a, *b)))
        }
        _ => None,
    }
}

fn sort_keys<K: Copy>(
    v: &mut [Value],
    budget: &impl Budget,
    key: fn(Value) -> K,
    value: fn(K) -> Value,
    cmp: fn(&K, &K) -> Ordering,
) -> Result<(), Ctl> {
    const CHUNK: usize = 1 << 15;
    let n = v.len();
    let mut keys: Vec<K> = v.iter().map(|x| key(*x)).collect();
    let mut result = Ok(());
    for c in keys.chunks_mut(CHUNK) {
        // About log2(len) comparisons per element.
        let cost = c.len() as u64 * (usize::BITS - c.len().leading_zeros()) as u64;
        if let Err(e) = budget.charge(cost) {
            result = Err(e);
            break;
        }
        c.sort_by(cmp);
    }
    if result.is_ok() && n > CHUNK {
        let mut buf = keys.clone();
        let mut in_keys = true;
        let mut width = CHUNK;
        while width < n {
            if let Err(e) = budget.charge(n as u64) {
                result = Err(e);
                break;
            }
            let (src, dst): (&[K], &mut [K]) = if in_keys { (&keys[..], &mut buf[..]) } else { (&buf[..], &mut keys[..]) };
            for lo in (0..n).step_by(2 * width) {
                let (mid, hi) = ((lo + width).min(n), (lo + 2 * width).min(n));
                let (a, b, out) = (&src[lo..mid], &src[mid..hi], &mut dst[lo..hi]);
                let (mut i, mut j) = (0, 0);
                for slot in out.iter_mut() {
                    // Stable: take from the left run on ties.
                    if j >= b.len() || (i < a.len() && cmp(&b[j], &a[i]) != Ordering::Less) {
                        *slot = a[i];
                        i += 1;
                    } else {
                        *slot = b[j];
                        j += 1;
                    }
                }
            }
            in_keys = !in_keys;
            width *= 2;
        }
        if !in_keys {
            keys = buf;
        }
    }
    for (slot, k) in v.iter_mut().zip(keys) {
        *slot = value(k);
    }
    result
}

/// A stable merge sort whose comparator may fail (e.g. when the meter runs
/// out): the standard library's sorts cannot be stopped midway, and an
/// inconsistent comparator could make them panic. Uses `v.len()` elements
/// of scratch space; on error `v` holds a permutation of its elements.
pub fn try_sort_by<T: Copy>(v: &mut [T], mut cmp: impl FnMut(&T, &T) -> Result<Ordering, Ctl>) -> Result<(), Ctl> {
    const RUN: usize = 16;
    let n = v.len();
    // Already sorted, or strictly descending (reversing keeps it stable).
    let (mut ascending, mut descending) = (true, true);
    for i in 1..n {
        let o = cmp(&v[i - 1], &v[i])?;
        ascending &= o != Ordering::Greater;
        descending &= o == Ordering::Greater;
        if !ascending && !descending {
            break;
        }
    }
    if ascending {
        return Ok(());
    }
    if descending {
        v.reverse();
        return Ok(());
    }
    // Insertion-sort short runs.
    for start in (0..n).step_by(RUN) {
        let end = (start + RUN).min(n);
        for i in start + 1..end {
            let mut j = i;
            while j > start && cmp(&v[j - 1], &v[j])? == Ordering::Greater {
                v.swap(j - 1, j);
                j -= 1;
            }
        }
    }
    if n <= RUN {
        return Ok(());
    }
    // Merge runs bottom-up, alternating between `v` and `buf`.
    let mut buf: Vec<T> = v.to_vec();
    let mut in_v = true;
    let mut width = RUN;
    let mut result = Ok(());
    while width < n && result.is_ok() {
        let (src, dst): (&[T], &mut [T]) = if in_v { (&*v, &mut buf[..]) } else { (&buf[..], &mut *v) };
        let mut lo = 0;
        while lo < n {
            let mid = (lo + width).min(n);
            let hi = (lo + 2 * width).min(n);
            if let Err(e) = merge(&src[lo..mid], &src[mid..hi], &mut dst[lo..hi], &mut cmp) {
                // Leave a permutation behind: finish this pass by copying.
                dst[lo..].copy_from_slice(&src[lo..]);
                result = Err(e);
                break;
            }
            lo = hi;
        }
        in_v = !in_v;
        width *= 2;
    }
    if !in_v {
        v.copy_from_slice(&buf);
    }
    result
}

fn merge<T: Copy>(a: &[T], b: &[T], out: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Result<Ordering, Ctl>) -> Result<(), Ctl> {
    // Already in order (common for nearly sorted input).
    if a.is_empty() || b.is_empty() || cmp(&b[0], &a[a.len() - 1])? != Ordering::Less {
        out[..a.len()].copy_from_slice(a);
        out[a.len()..].copy_from_slice(b);
        return Ok(());
    }
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        // Take from `a` unless `b` is strictly smaller: stable.
        if cmp(&b[j], &a[i])? == Ordering::Less {
            out[k] = b[j];
            j += 1;
        } else {
            out[k] = a[i];
            i += 1;
        }
        k += 1;
    }
    out[k..k + a.len() - i].copy_from_slice(&a[i..]);
    k += a.len() - i;
    out[k..].copy_from_slice(&b[j..]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_sort_is_a_stable_sort() {
        let mut seed = 12345u64;
        for n in [0, 1, 2, 15, 16, 17, 31, 33, 100, 1000, 4097] {
            let mut v: Vec<(u32, usize)> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (((seed >> 33) % 50) as u32, i)
                })
                .collect();
            let mut expected = v.clone();
            expected.sort_by_key(|x| x.0);
            try_sort_by(&mut v, |a, b| Ok(a.0.cmp(&b.0))).unwrap();
            assert_eq!(v, expected, "n = {n}");
        }
    }

    #[test]
    fn try_sort_stops_on_error_leaving_a_permutation() {
        let mut v: Vec<u32> = (0..1000).map(|i| (i * 7919) % 1000).collect();
        let mut calls = 0;
        let r = try_sort_by(&mut v, |a, b| {
            calls += 1;
            if calls > 3000 { Err(Ctl::Interrupted) } else { Ok(a.cmp(b)) }
        });
        assert!(matches!(r, Err(Ctl::Interrupted)));
        assert!(calls <= 3001);
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(sorted, (0..1000).collect::<Vec<u32>>());
    }

    #[test]
    fn numeric_sort_matches_the_generic_order() {
        let mut seed = 99u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            seed >> 33
        };
        for n in [0, 1, 2, 100, (1 << 15) - 1, 1 << 15, (1 << 15) + 1, 100_000, 300_001] {
            let ints: Vec<Value> = (0..n).map(|_| Value::Int((next() % 1000) as i64 - 500)).collect();
            let mut got = ints.clone();
            assert!(matches!(try_sort_numbers(&mut got, &Unmetered), Some(Ok(())) | None));
            let mut want: Vec<i64> = ints.iter().map(|v| v.as_int()).collect();
            want.sort();
            assert_eq!(got.iter().map(|v| v.as_int()).collect::<Vec<_>>(), want, "n = {n}");
        }
        let specials = [f64::NAN, 1.0, -0.0, f64::NEG_INFINITY, 0.0, f64::INFINITY, -2.5];
        let mut floats: Vec<Value> = specials.iter().copied().cycle().take(70_001).map(Value::Float).collect();
        try_sort_numbers(&mut floats, &Unmetered).unwrap().unwrap();
        assert!(floats.windows(2).all(|w| total_cmp_f(w[0].as_float(), w[1].as_float()) != Ordering::Greater));
        assert!(floats.last().unwrap().as_float().is_nan());
        // Mixed lists use the generic sort.
        assert!(try_sort_numbers(&mut [Value::Int(1), Value::Float(1.0)], &Unmetered).is_none());
    }
}
