//! Structural equality, ordering and hashing of values. All algorithms are
//! iterative so that deeply nested values cannot overflow the host stack,
//! walk sequences lazily (scratch space grows with nesting depth, not with
//! the size of the values), and charge a [`Budget`] one unit per node plus
//! the bytes of strings compared, so that comparing huge values costs fuel
//! and can be interrupted.

use super::Ctl;
use super::bytecode::Program;
use super::heap::Heap;
use super::meter::{Budget, Unmetered};
use super::value::*;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::hash::Hasher;

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
            return Err(Ctl::limit(crate::LimitKind::Memory));
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

fn map_lookup_value(heap: &Heap, budget: &impl Budget, m: &MapObj, k: Value) -> Result<Option<Value>, Ctl> {
    Ok(map_slot(heap, budget, m, k)?.and_then(|s| m.entries[s].map(|(_, v)| v)))
}

/// The slot of key `k` in map `m`.
#[inline]
pub fn map_slot(heap: &Heap, budget: &impl Budget, m: &MapObj, k: Value) -> Result<Option<usize>, Ctl> {
    if m.live == 0 {
        return Ok(None);
    }
    if let Value::Int(i) = k {
        // Int keys compare without the general (fallible) equality.
        return Ok(m.index.find(int_hash(i), |s| matches!(m.entries[s], Some((Value::Int(x), _)) if x == i)));
    }
    let hash = hash_key(heap, budget, k)?;
    find_key(heap, budget, &m.index, hash, k, |i| m.entries[i].map(|(k, _)| k))
}

/// The slot of element `k` in set `s`.
#[inline]
pub fn set_slot(heap: &Heap, budget: &impl Budget, s: &SetObj, k: Value) -> Result<Option<usize>, Ctl> {
    if s.live == 0 {
        return Ok(None);
    }
    if let Value::Int(i) = k {
        return Ok(s.index.find(int_hash(i), |e| matches!(s.entries[e], Some(Value::Int(x)) if x == i)));
    }
    let hash = hash_key(heap, budget, k)?;
    find_key(heap, budget, &s.index, hash, k, |i| s.entries[i])
}

fn set_lookup(heap: &Heap, budget: &impl Budget, s: &SetObj, k: Value) -> Result<Option<usize>, Ctl> {
    set_slot(heap, budget, s, k)
}

/// The slot of `table` holding key `k` (whose hash is `hash`); `key_at`
/// reads the key of a slot.
#[inline]
pub fn find_key(
    heap: &Heap,
    budget: &impl Budget,
    table: &KeyTable,
    hash: u64,
    k: Value,
    key_at: impl Fn(usize) -> Option<Value>,
) -> Result<Option<usize>, Ctl> {
    let mut failed = None;
    let slot = table.find(hash, |i| match key_at(i) {
        Some(x) => match keys_eq(heap, budget, x, k) {
            Ok(eq) => eq,
            Err(c) => {
                failed.get_or_insert(c);
                false
            }
        },
        None => false,
    });
    match failed {
        Some(c) => Err(c),
        None => Ok(slot),
    }
}

/// Equality of two keys (of a `Hash` type).
#[inline]
pub fn keys_eq(heap: &Heap, budget: &impl Budget, a: Value, b: Value) -> Result<bool, Ctl> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x == y),
        _ if a == b => Ok(true),
        _ => values_eq_metered(heap, budget, a, b),
    }
}

/// The hash of a map key or set element, consistent with [`values_eq`] for
/// the `Hash` types (variants hash the same in every representation, `0.0`
/// and `-0.0` alike, NaNs alike), computed where the value lives: `Hash`
/// types are deeply immutable, so keys are never copied.
///
/// Hashes are compositional (a container's hash combines the hashes of its
/// fields), so once a key turns out to be large the hashes of its containers
/// are memoized by handle: a key with shared substructure, whose tree is
/// exponentially larger than its heap size, hashes in time linear in the
/// latter. Charges one unit per node visited plus the bytes hashed;
/// iterative, so deep keys cannot overflow the host stack.
#[inline]
pub fn hash_key(heap: &Heap, budget: &impl Budget, v: Value) -> Result<u64, Ctl> {
    // Fast paths, inlined: the common scalar and string keys.
    match v {
        Value::Int(i) => Ok(int_hash(i)),
        Value::Obj(_) => match heap.get(v) {
            Obj::Str(s) => {
                budget.charge_bytes(s.len())?;
                Ok(bytes_hash(s.as_bytes()))
            }
            _ => hash_tree(heap, budget, v),
        },
        _ => hash_tree(heap, budget, v),
    }
}

/// [`hash_key`] for keys that are not ints or strings.
#[inline(never)]
fn hash_tree(heap: &Heap, budget: &impl Budget, v: Value) -> Result<u64, Ctl> {
    let mut stack = HashStack::default();
    let mut memo: Option<FxHashMap<u32, u64>> = None;
    let mut nodes = 0usize;
    let mut next = Some(v);
    loop {
        // The hash of the next finished node, if any.
        let done = match next.take() {
            Some(v) => {
                budget.charge(1)?;
                nodes += 1;
                if nodes == MEMO_AFTER {
                    memo = Some(FxHashMap::default());
                }
                match leaf_hash(heap, budget, v)? {
                    Some(h) => Some(h),
                    None => {
                        if let (Some(memo), Value::Obj(h)) = (&memo, v)
                            && let Some(&x) = memo.get(&h)
                        {
                            Some(x)
                        } else {
                            push_container(heap, &mut stack, v)?;
                            None
                        }
                    }
                }
            }
            None => {
                let Some(top) = stack.top() else { unreachable!("hash of an empty stack") };
                match heap.field(top.v, top.i) {
                    Some(f) => {
                        top.i += 1;
                        next = Some(f);
                        None
                    }
                    None => {
                        let (c, h) = (top.v, top.h.finish());
                        stack.pop();
                        // (Only heap containers: an inline variant's `handle` is its payload's.)
                        if let (Some(memo), Value::Obj(handle)) = (&mut memo, c) {
                            if memo.len() == memo.capacity() && heap.would_exceed(memo.len().max(4) * 2 * 24) {
                                return Err(Ctl::limit(crate::LimitKind::Memory));
                            }
                            memo.insert(handle, h);
                        }
                        Some(h)
                    }
                }
            }
        };
        if let Some(h) = done {
            match stack.top() {
                Some(top) => top.h.write_u64(h),
                None => return Ok(h),
            }
        }
    }
}

/// Nodes a key may have before [`hash_key`] memoizes the hashes of its
/// containers.
const MEMO_AFTER: usize = 256;

/// [`hash_key`] of a string with contents `s`, for keys that are not on
/// the heap yet (see `Heap::staged`).
#[inline]
pub fn str_hash(s: &str) -> u64 {
    bytes_hash(s.as_bytes())
}

// Scalar and string hashes carry no type tag: the keys of a map, and the
// values at any one position of a key (a field, an element, a variant's
// payload), all have one static type, so values of different types are
// never compared. Ints hash with one multiply.

#[inline]
fn int_hash(i: i64) -> u64 {
    let mut h = FxHasher::default();
    h.write_i64(i);
    h.finish()
}

/// The length keeps strings that differ by trailing NULs apart (the last
/// partial word is zero-padded).
#[inline]
fn bytes_hash(b: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write_usize(b.len());
    h.write(b);
    h.finish()
}

/// The hash of `v` if it is not a container (a tuple, struct or variant
/// with a payload).
#[inline]
fn leaf_hash(heap: &Heap, budget: &impl Budget, v: Value) -> Result<Option<u64>, Ctl> {
    let mut h = FxHasher::default();
    match v {
        Value::Unit | Value::Uninit => h.write_u8(0),
        Value::Bool(b) => {
            h.write_u8(1);
            h.write_u8(b as u8);
        }
        Value::Int(i) => return Ok(Some(int_hash(i))),
        Value::Dur(d) => return Ok(Some(int_hash(d))),
        Value::Float(f) => {
            h.write_u8(4);
            h.write_u64(if f == 0.0 {
                0
            } else if f.is_nan() {
                f64::NAN.to_bits()
            } else {
                f.to_bits()
            });
        }
        // A payload-less variant (canonical: never a heap object).
        Value::Tag(id) => {
            h.write_u8(5);
            h.write_u32(id);
            h.write_usize(0);
        }
        Value::Obj(handle) => match heap.get(v) {
            Obj::Str(s) => {
                budget.charge_bytes(s.len())?;
                return Ok(Some(bytes_hash(s.as_bytes())));
            }
            Obj::Bytes(b) => {
                budget.charge_bytes(b.len())?;
                return Ok(Some(bytes_hash(b)));
            }
            Obj::Variant(..) | Obj::Tuple(_) | Obj::Struct(..) => return Ok(None),
            Obj::Range(a, b, incl) => {
                h.write_u8(10);
                h.write_i64(*a);
                h.write_i64(*b);
                h.write_u8(*incl as u8);
            }
            // Identity types (tasks, channels, ...).
            _ => {
                h.write_u8(11);
                h.write_u32(handle);
            }
        },
        // Inline single-payload variants are containers.
        _ => return Ok(None),
    }
    Ok(Some(h.finish()))
}

/// The hasher of container `v` after its header (kind, id, arity); its
/// fields' hashes follow.
fn container_hasher(heap: &Heap, v: Value) -> FxHasher {
    let mut h = FxHasher::default();
    if let Some((id, _)) = v.as_inline_variant() {
        h.write_u8(5);
        h.write_u32(id);
        h.write_usize(1);
        return h;
    }
    match heap.get(v) {
        Obj::Variant(id, f) => {
            h.write_u8(5);
            h.write_u32(*id);
            h.write_usize(f.len());
        }
        Obj::Tuple(f) => {
            h.write_u8(8);
            h.write_usize(f.len());
        }
        Obj::Struct(def, _) => {
            h.write_u8(9);
            h.write_u32(*def);
        }
        _ => {}
    }
    h
}

/// A container [`hash_key`] is in: its next field and its hasher.
#[derive(Clone, Copy)]
struct HashFrame {
    v: Value,
    i: usize,
    h: FxHasher,
}

/// The containers [`hash_key`] is in: on the host stack up to a small depth
/// (so hashing a typical tuple key allocates nothing), then on the heap.
#[derive(Default)]
struct HashStack {
    small: [Option<HashFrame>; 8],
    n: usize,
    big: Vec<HashFrame>,
}

impl HashStack {
    #[inline]
    fn top(&mut self) -> Option<&mut HashFrame> {
        match self.big.last_mut() {
            Some(t) => Some(t),
            None => self.n.checked_sub(1).and_then(|i| self.small[i].as_mut()),
        }
    }

    #[inline]
    fn pop(&mut self) {
        if self.big.pop().is_none() {
            self.n -= 1;
        }
    }

    fn len(&self) -> usize {
        self.n + self.big.len()
    }
}

/// Starts hashing container `v`.
fn push_container(heap: &Heap, stack: &mut HashStack, v: Value) -> Result<(), Ctl> {
    let frame = HashFrame { v, i: 0, h: container_hasher(heap, v) };
    if stack.big.is_empty() && stack.n < stack.small.len() {
        stack.small[stack.n] = Some(frame);
        stack.n += 1;
        return Ok(());
    }
    if stack.len() >= MAX_DEPTH {
        return Err(Ctl::panic("value too deeply nested to hash"));
    }
    if stack.big.len() == stack.big.capacity() && heap.would_exceed(stack.len().max(4) * 2 * std::mem::size_of::<HashFrame>()) {
        return Err(Ctl::limit(crate::LimitKind::Memory));
    }
    if stack.big.is_empty() {
        stack.big.extend(stack.small[..stack.n].iter().flatten());
        stack.n = 0;
    }
    stack.big.push(frame);
    Ok(())
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
    use crate::vm::CtlKind;

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
            if calls > 3000 { Err(Ctl::interrupted()) } else { Ok(a.cmp(b)) }
        });
        assert!(matches!(r.map_err(Ctl::into_kind), Err(CtlKind::Interrupted)));
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
