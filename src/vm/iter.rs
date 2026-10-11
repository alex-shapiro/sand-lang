//! Iterator functions (`Obj::IterFn`): the lazy iterators of lists, ranges,
//! strings, maps and sets, which the interpreter steps directly.
//!
//! An iterator of a map or set (including a `for` loop over one) visits the
//! entries it had when the iterator was made (spec §6.9, §12.13), without
//! copying them up front: it reads the entries in place by slot, skipping
//! removed ones, and pins the map or set (its `pins`). Every change to a map
//! or set first calls [`Vm::unpin`], which moves the iterators still reading
//! it to one private copy of its entries (with the same slots) and clears
//! the pins. So iterating costs no copy unless the map or set changes
//! meanwhile, in which case it costs one copy.

use super::*;
use crate::vm::interp::internal;

/// Pins are checked for staleness when there are at least this many.
const MIN_PRUNE: usize = 8;

/// Whether `it` is an iterator reading map or set `h` in place.
fn reads_in_place(heap: &Heap, it: u32, h: u32) -> bool {
    matches!(heap.get(Value::Obj(it)), Obj::IterFn(IterSrc::Map { map: o, .. } | IterSrc::Set { set: o, .. }) if *o == h)
}

/// The first live entry of map `map` at or after `slot`, and its slot.
#[inline]
fn map_entry(heap: &Heap, map: u32, slot: usize) -> Option<(usize, Value, Value)> {
    let Obj::Map(m) = heap.get(Value::Obj(map)) else { return None };
    let rest = m.entries.get(slot..)?;
    rest.iter().position(Option::is_some).and_then(|i| rest[i].map(|(k, v)| (slot + i, k, v)))
}

/// The first live element of set `set` at or after `slot`, and its slot.
#[inline]
fn set_entry(heap: &Heap, set: u32, slot: usize) -> Option<(usize, Value)> {
    let Obj::Set(s) = heap.get(Value::Obj(set)) else { return None };
    let rest = s.entries.get(slot..)?;
    rest.iter().position(Option::is_some).and_then(|i| rest[i].map(|x| (slot + i, x)))
}

impl Vm {
    /// A function value (`fn() -> T?`) producing the elements of `src`.
    pub fn iter_fn(&mut self, src: IterSrc) -> Value {
        self.heap.alloc(Obj::IterFn(src))
    }

    /// The iterator of map `c` (its (key, value) entries) or set `c`.
    pub fn container_iter(&mut self, c: Value) -> Result<Value, Ctl> {
        let Value::Obj(h) = c else { return Err(internal("not a map or set")) };
        let (src, slots, live, pins) = match self.heap.get(c) {
            Obj::Map(m) => (IterSrc::Map { map: h, slot: 0 }, m.entries.len(), m.live, &m.pins),
            Obj::Set(s) => (IterSrc::Set { set: h, slot: 0 }, s.entries.len(), s.live, &s.pins),
            _ => return Err(internal("not a map or set")),
        };
        if live == 0 {
            return Ok(self.iter_fn(IterSrc::Done));
        }
        let prune = pins.its.len() >= pins.prune_at.max(MIN_PRUNE);
        // The steps cannot fail, so the removed entries they will skip are
        // charged now: an iterator walks exactly these entries (the map or
        // set does not change before the iterator moves to a copy of them).
        let skipped = (slots - live) as u64;
        self.meter.charge(if prune { skipped + pins.its.len() as u64 } else { skipped })?;
        let it = self.iter_fn(src);
        let Value::Obj(ih) = it else { unreachable!() };
        let kept = if prune {
            let Some(pins) = self.heap.pins_mut(c) else { unreachable!() };
            let mut its = std::mem::take(&mut pins.its);
            its.retain(|&p| reads_in_place(&self.heap, p, h));
            Some(its)
        } else {
            None
        };
        let Some(pins) = self.heap.pins_mut(c) else { unreachable!() };
        let before = pins.bytes();
        if let Some(its) = kept {
            pins.prune_at = 2 * its.len();
            pins.its = its;
        }
        pins.its.push(ih);
        let after = pins.bytes();
        self.heap.account(before, after);
        Ok(it)
    }

    /// Call before map or set `c` changes: moves the iterators reading it in
    /// place to a copy of its entries, so that they go on with the entries
    /// they started with. On error nothing has changed.
    #[inline]
    pub(crate) fn unpin(&mut self, c: Value) -> Result<(), Ctl> {
        let pinned = match self.heap.get(c) {
            Obj::Map(m) => !m.pins.its.is_empty(),
            Obj::Set(s) => !s.pins.its.is_empty(),
            _ => false,
        };
        if pinned { self.detach_iters(c) } else { Ok(()) }
    }

    #[cold]
    #[inline(never)]
    fn detach_iters(&mut self, c: Value) -> Result<(), Ctl> {
        let Value::Obj(h) = c else { return Ok(()) };
        let (pins, slots, bytes) = match self.heap.get(c) {
            Obj::Map(m) => (&m.pins, m.entries.len(), m.entries.len() * MAP_ENTRY_SIZE),
            Obj::Set(s) => (&s.pins, s.entries.len(), s.entries.len() * SET_ENTRY_SIZE),
            _ => return Ok(()),
        };
        let pinned = pins.its.len();
        let live: Vec<u32> = pins.its.iter().copied().filter(|&p| reads_in_place(&self.heap, p, h)).collect();
        if !live.is_empty() {
            self.meter.charge((slots + pinned) as u64)?;
            self.reserve(bytes.saturating_add(std::mem::size_of::<Obj>()))?;
            let copy = match self.heap.get(c) {
                Obj::Map(m) => Obj::Map(Box::new(MapObj { entries: m.entries.clone(), live: m.live, ..MapObj::default() })),
                Obj::Set(s) => Obj::Set(Box::new(SetObj { entries: s.entries.clone(), live: s.live, ..SetObj::default() })),
                _ => return Ok(()),
            };
            let snap = self.heap.alloc(copy);
            let Value::Obj(sh) = snap else { unreachable!() };
            for p in live {
                if let Obj::IterFn(IterSrc::Map { map: o, .. } | IterSrc::Set { set: o, .. }) =
                    self.heap.get_mut_storing(Value::Obj(p), &[snap])
                    && *o == h
                {
                    *o = sh;
                }
            }
        }
        if let Some(pins) = self.heap.pins_mut(c) {
            let before = pins.bytes();
            *pins = Pins::default();
            self.heap.account(before, 0);
        }
        Ok(())
    }

    /// Calls the iterator function `it` (an [`Obj::IterFn`]): the next
    /// element, or `None`.
    #[inline(never)]
    pub(crate) fn iter_step(&mut self, it: Value) -> Value {
        let next = self.iter_next(it);
        self.option(next)
    }

    /// [`Vm::iter_step`] without wrapping the element in `Some`.
    #[inline(never)]
    pub(crate) fn iter_next(&mut self, it: Value) -> Option<Value> {
        let Obj::IterFn(src) = self.heap.get(it) else { return None };
        if let IterSrc::Chars { s, i } = *src
            && let Some(&b) = self.heap.str(Value::Obj(s)).as_bytes().get(i as usize)
            && b.is_ascii()
        {
            if let Some(IterSrc::Chars { i, .. }) = self.heap.iter_src_mut(it) {
                *i += 1;
            }
            return Some(self.ascii_str(b));
        }
        let (next, state) = match *src {
            IterSrc::List { list, i, n } => match self.heap.list(Value::Obj(list)).get(i as usize) {
                Some(&x) if i < n => (Some(x), IterSrc::List { list, i: i + 1, n }),
                _ => (None, IterSrc::List { list, i: n, n }),
            },
            IterSrc::Range { i, end } if i < end => (Some(Value::Int(i)), IterSrc::Range { i: i + 1, end }),
            IterSrc::Range { .. } | IterSrc::Done => return None,
            IterSrc::Chars { s, i } => {
                let c = self.heap.str(Value::Obj(s)).get(i as usize..).and_then(|t| t.chars().next())?;
                let state = IterSrc::Chars { s, i: i + c.len_utf8() as i64 };
                (Some(self.short_str(c.encode_utf8(&mut [0; 4]))), state)
            }
            IterSrc::Map { map, slot } => match map_entry(&self.heap, map, slot) {
                Some((s, k, v)) => {
                    let pair = self.heap.new_tuple(&[k, v]);
                    (Some(pair), IterSrc::Map { map, slot: s + 1 })
                }
                None => (None, IterSrc::Done),
            },
            IterSrc::Set { set, slot } => match set_entry(&self.heap, set, slot) {
                Some((s, x)) => (Some(x), IterSrc::Set { set, slot: s + 1 }),
                None => (None, IterSrc::Done),
            },
        };
        if let Some(src) = self.heap.iter_src_mut(it) {
            *src = state;
        }
        next
    }

    /// The step of a `for` loop over a set: writes the next element to
    /// `out`; false when there is none.
    #[inline(never)]
    pub(crate) fn set_step(&mut self, it: Value, out: &mut Value) -> bool {
        let Obj::IterFn(IterSrc::Set { set, slot }) = *self.heap.get(it) else { return false };
        let (state, more) = match set_entry(&self.heap, set, slot) {
            Some((s, x)) => {
                *out = x;
                (IterSrc::Set { set, slot: s + 1 }, true)
            }
            None => (IterSrc::Done, false),
        };
        if let Some(src) = self.heap.iter_src_mut(it) {
            *src = state;
        }
        more
    }

    /// The step of a `for` loop over a map: writes the next entry's key
    /// and value to `out[0]` and `out[1]`; false when there is none.
    #[inline(never)]
    pub(crate) fn map_step(&mut self, it: Value, out: &mut [Value]) -> bool {
        let Obj::IterFn(IterSrc::Map { map, slot }) = *self.heap.get(it) else { return false };
        let (state, more) = match map_entry(&self.heap, map, slot) {
            Some((s, k, v)) => {
                out[0] = k;
                out[1] = v;
                (IterSrc::Map { map, slot: s + 1 }, true)
            }
            None => (IterSrc::Done, false),
        };
        if let Some(src) = self.heap.iter_src_mut(it) {
            *src = state;
        }
        more
    }
}
