//! Lazy iterator functions.
//!
//! An iterator visits collection entries without copying them up front. It
//! reads each entry in place, skipping removed ones, and registers itself in
//! the map or set's [`ActiveIters`]. Any modification must call
//! [`Vm::detach_iters`], which moves the iterators still reading in place to a
//! private copy and clears `active_iters`. Thus iterating does not copy unless
//! the collection is modified during iteration.

use super::*;
use crate::vm::interp::internal;

impl Vm {
    /// A function value (`fn() -> T?`) producing the elements of `state`.
    pub fn iter_fn(&mut self, state: IterState) -> VmValue {
        self.heap.alloc(Obj::IterFn(state))
    }

    /// The iterator of map `c` (its (key, value) entries) or set `c`.
    pub fn container_iter(&mut self, c: VmValue) -> Result<VmValue, Fail> {
        let VmValue::Obj(h) = c else { return Err(internal("not a map or set")) };
        let (state, slots, live, active_iters) = match self.heap.get(c) {
            Obj::Map(m) => (IterState::Map { map: h, slot: 0 }, m.entries.len(), m.live, &m.active_iters),
            Obj::Set(s) => (IterState::Set { set: h, slot: 0 }, s.entries.len(), s.live, &s.active_iters),
            _ => return Err(internal("not a map or set")),
        };
        if live == 0 {
            return Ok(self.iter_fn(IterState::Done));
        }
        let prune = active_iters.handles.len() >= active_iters.prune_at.max(MIN_PRUNE);
        // Charge here for the tombstones that the iterator will skip and for 1
        // handle slot. Charging 1 unit per iterator keeps prune and detach fuel
        // independent of how many dead iterators remain in `active_iters.handles`.
        self.meter.charge((slots - live) as u64 + 1)?;
        let it = self.iter_fn(state);
        let VmValue::Obj(ih) = it else { unreachable!() };
        let kept = if prune {
            let Some(active_iters) = self.heap.active_iters_mut(c) else { unreachable!() };
            let mut handles = std::mem::take(&mut active_iters.handles);
            handles.retain(|&handle| reads_in_place(&self.heap, handle, h));
            Some(handles)
        } else {
            None
        };
        let Some(active_iters) = self.heap.active_iters_mut(c) else { unreachable!() };
        let before = active_iters.bytes();
        if let Some(handles) = kept {
            active_iters.prune_at = 2 * handles.len();
            active_iters.handles = handles;
        }
        active_iters.handles.push(ih);
        active_iters.open += 1;
        let after = active_iters.bytes();
        self.heap.report_size_change(before, after);
        Ok(it)
    }

    /// Calls the iterator function `it` (an [`Obj::IterFn`]): the next
    /// element, or `None`.
    #[inline(never)]
    pub(crate) fn iter_step(&mut self, it: VmValue) -> VmValue {
        let next = self.iter_next(it);
        self.option(next)
    }

    /// [`Vm::iter_step`] without wrapping the element in `Some`.
    #[inline(never)]
    pub(crate) fn iter_next(&mut self, it: VmValue) -> Option<VmValue> {
        let Obj::IterFn(iter_state) = self.heap.get(it) else { return None };
        if let IterState::Chars { s, i } = *iter_state
            && let Some(&b) = self.heap.str(VmValue::Obj(s)).as_bytes().get(i as usize)
            && b.is_ascii()
        {
            if let Some(IterState::Chars { i, .. }) = self.heap.iter_state_mut(it) {
                *i += 1;
            }
            return Some(self.ascii_str(b));
        }
        let (next, state) = match *iter_state {
            IterState::List { list, i, n } => match self.heap.list(VmValue::Obj(list)).get(i as usize) {
                Some(&x) if i < n => (Some(x), IterState::List { list, i: i + 1, n }),
                _ => (None, IterState::List { list, i: n, n }),
            },
            IterState::Range { i, last } if i <= last => {
                let state = if i < last { IterState::Range { i: i + 1, last } } else { IterState::Done };
                (Some(VmValue::Int(i)), state)
            }
            IterState::Range { .. } | IterState::Done => return None,
            IterState::Chars { s, i } => {
                let c = self.heap.str(VmValue::Obj(s)).get(i as usize..).and_then(|t| t.chars().next())?;
                let state = IterState::Chars { s, i: i + c.len_utf8() as i64 };
                (Some(self.short_str(c.encode_utf8(&mut [0; 4]))), state)
            }
            IterState::Map { map, slot } => match map_entry(&self.heap, map, slot) {
                Some((s, k, v)) => {
                    let pair = self.heap.new_tuple(&[k, v]);
                    (Some(pair), IterState::Map { map, slot: s + 1 })
                }
                None => {
                    self.close_iter(map);
                    (None, IterState::Done)
                }
            },
            IterState::Set { set, slot } => match set_entry(&self.heap, set, slot) {
                Some((s, x)) => (Some(x), IterState::Set { set, slot: s + 1 }),
                None => {
                    self.close_iter(set);
                    (None, IterState::Done)
                }
            },
        };
        if let Some(iter_state) = self.heap.iter_state_mut(it) {
            *iter_state = state;
        }
        next
    }

    /// The step of a `for` loop over a set: writes the next element to
    /// `out`; false when there is none.
    #[inline(never)]
    pub(crate) fn set_step(&mut self, it: VmValue, out: &mut VmValue) -> bool {
        let Obj::IterFn(IterState::Set { set, slot }) = *self.heap.get(it) else { return false };
        let (state, more) = match set_entry(&self.heap, set, slot) {
            Some((s, x)) => {
                *out = x;
                (IterState::Set { set, slot: s + 1 }, true)
            }
            None => {
                self.close_iter(set);
                (IterState::Done, false)
            }
        };
        if let Some(iter_state) = self.heap.iter_state_mut(it) {
            *iter_state = state;
        }
        more
    }

    /// The step of a `for` loop over a map: writes the next entry's key
    /// and value to `out[0]` and `out[1]`; false when there is none.
    #[inline(never)]
    pub(crate) fn map_step(&mut self, it: VmValue, out: &mut [VmValue]) -> bool {
        let Obj::IterFn(IterState::Map { map, slot }) = *self.heap.get(it) else { return false };
        let (state, more) = match map_entry(&self.heap, map, slot) {
            Some((s, k, v)) => {
                out[0] = k;
                out[1] = v;
                (IterState::Map { map, slot: s + 1 }, true)
            }
            None => {
                self.close_iter(map);
                (IterState::Done, false)
            }
        };
        if let Some(iter_state) = self.heap.iter_state_mut(it) {
            *iter_state = state;
        }
        more
    }

    /// Detaches iterators reading map or set `c` in place onto a snapshot copy
    /// before `c` is modified. Leaves `c` unchanged on error.
    #[inline]
    pub(crate) fn detach_iters(&mut self, c: VmValue) -> Result<(), Fail> {
        // Check `active_iters.open` so `detach_iters_slow` charges for the copy
        // even if garbage collection already freed the unreachable iterators.
        let has_open = match self.heap.get(c) {
            Obj::Map(m) => m.active_iters.open > 0,
            Obj::Set(s) => s.active_iters.open > 0,
            _ => false,
        };
        if has_open { self.detach_iters_slow(c) } else { Ok(()) }
    }

    // ------------------------------------------------------------------
    // Internal helpers

    #[cold]
    #[inline(never)]
    fn detach_iters_slow(&mut self, c: VmValue) -> Result<(), Fail> {
        let VmValue::Obj(h) = c else { return Ok(()) };
        let (active_iters, slots, bytes) = match self.heap.get(c) {
            Obj::Map(m) => (&m.active_iters, m.entries.len(), m.entries.len() * MAP_ENTRY_SIZE),
            Obj::Set(s) => (&s.active_iters, s.entries.len(), s.entries.len() * SET_ENTRY_SIZE),
            _ => return Ok(()),
        };
        self.meter.charge(slots as u64)?;
        let live: Vec<u32> = active_iters.handles.iter().copied().filter(|&handle| reads_in_place(&self.heap, handle, h)).collect();
        if !live.is_empty() {
            self.check_memory(bytes.saturating_add(std::mem::size_of::<Obj>()))?;
            let copy = match self.heap.get(c) {
                Obj::Map(m) => Obj::Map(Box::new(MapObj { entries: m.entries.clone(), live: m.live, ..MapObj::default() })),
                Obj::Set(s) => Obj::Set(Box::new(SetObj { entries: s.entries.clone(), live: s.live, ..SetObj::default() })),
                _ => return Ok(()),
            };
            let snap = self.heap.alloc(copy);
            let VmValue::Obj(sh) = snap else { unreachable!() };
            for handle in live {
                if let Obj::IterFn(IterState::Map { map: o, .. } | IterState::Set { set: o, .. }) =
                    self.heap.get_mut_storing(VmValue::Obj(handle), &[snap])
                    && *o == h
                {
                    *o = sh;
                }
            }
        }
        if let Some(active_iters) = self.heap.active_iters_mut(c) {
            let before = active_iters.bytes();
            *active_iters = ActiveIters::default();
            self.heap.report_size_change(before, 0);
        }
        Ok(())
    }

    /// Records that an iterator reading map or set `container` finished.
    fn close_iter(&mut self, container: u32) {
        if let Some(active_iters) = self.heap.active_iters_mut(VmValue::Obj(container)) {
            active_iters.open = active_iters.open.saturating_sub(1);
        }
    }
}

// ------------------------------------------------------------------
// Internal utilities and constants

/// Minimum handle count before pruning stale entries from [`ActiveIters`].
const MIN_PRUNE: usize = 8;

/// Whether `it` is an iterator reading map or set `h` in place.
fn reads_in_place(heap: &Heap, it: u32, h: u32) -> bool {
    matches!(heap.get_obj(it), Some(Obj::IterFn(IterState::Map { map: o, .. } | IterState::Set { set: o, .. })) if *o == h)
}

/// The first live entry of map `map` at or after `slot`, and its slot.
#[inline]
fn map_entry(heap: &Heap, map: u32, slot: usize) -> Option<(usize, VmValue, VmValue)> {
    let Obj::Map(m) = heap.get(VmValue::Obj(map)) else { return None };
    let rest = m.entries.get(slot..)?;
    rest.iter().position(Option::is_some).and_then(|i| rest[i].map(|(k, v)| (slot + i, k, v)))
}

/// The first live element of set `set` at or after `slot`, and its slot.
#[inline]
fn set_entry(heap: &Heap, set: u32, slot: usize) -> Option<(usize, VmValue)> {
    let Obj::Set(s) = heap.get(VmValue::Obj(set)) else { return None };
    let rest = s.entries.get(slot..)?;
    rest.iter().position(Option::is_some).and_then(|i| rest[i].map(|x| (slot + i, x)))
}
