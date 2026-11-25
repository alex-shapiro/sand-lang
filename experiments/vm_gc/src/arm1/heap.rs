//! Values, heap objects, and the mark-sweep heap of Arm 1.
//!
//! The heap is a flat `Vec<Obj>` indexed by `Copy` handles. Any allocation
//! can collect garbage, so code that holds a handle across an allocation or
//! a callback must root it on [`Vm::roots`](super::vm::Vm::roots).

use super::codec_objects::{DecoderObj, EncoderObj};
use crate::shared::{GcPolicy, Limits, ShortStr, Tracker};
use std::any::Any;
use std::mem::size_of;

/// A handle to a heap object.
pub type Handle = u32;

/// An immediate value or a heap handle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Unit,
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A payload-less enum variant.
    Tag(u16),
    Short(ShortStr),
    Obj(Handle),
}

const _: () = assert!(size_of::<Value>() == 16);

/// A Rust object stored in the heap.
pub trait HostObject: Any + Send {
    /// Visits every value that the object holds.
    fn trace(&self, visit: &mut dyn FnMut(Value));

    /// Bytes owned by the object, counted against the memory limit. The size
    /// must not change while the object lives, because the collector
    /// releases the size it reports when it frees the object.
    fn size(&self) -> usize {
        0
    }
}

pub enum Obj {
    /// A free slot and the next free slot.
    Free(u32),
    Str(Box<str>),
    Struct {
        ty: u16,
        fields: Box<[Value]>,
    },
    Variant {
        tag: u16,
        fields: Box<[Value]>,
    },
    List(Vec<Value>),
    /// Entries in insertion order.
    Map(Vec<(Value, Value)>),
    Closure {
        func: u16,
        captures: Box<[Value]>,
    },
    Cell(Value),
    Encoder(Box<EncoderObj>),
    Decoder(Box<DecoderObj>),
    Host(Box<dyn HostObject>),
}

impl Obj {
    /// Bytes owned by the object outside its slot.
    pub fn size(&self) -> usize {
        match self {
            Obj::Free(_) | Obj::Cell(_) => 0,
            Obj::Str(s) => s.len(),
            Obj::Struct { fields, .. } | Obj::Variant { fields, .. } | Obj::Closure { captures: fields, .. } => {
                fields.len() * size_of::<Value>()
            }
            Obj::List(items) => items.capacity() * size_of::<Value>(),
            Obj::Map(entries) => entries.capacity() * size_of::<(Value, Value)>(),
            Obj::Encoder(e) => e.size(),
            Obj::Decoder(d) => d.size(),
            Obj::Host(h) => h.size(),
        }
    }

    #[inline]
    fn trace(&self, mark: &mut Marker) {
        match self {
            Obj::Struct { fields, .. } | Obj::Variant { fields, .. } | Obj::Closure { captures: fields, .. } => {
                fields.iter().for_each(|v| mark.value(*v))
            }
            Obj::List(items) => items.iter().for_each(|v| mark.value(*v)),
            Obj::Map(entries) => entries.iter().for_each(|(k, v)| {
                mark.value(*k);
                mark.value(*v);
            }),
            Obj::Cell(v) => mark.value(*v),
            Obj::Decoder(d) => mark.value(d.input),
            Obj::Host(h) => h.trace(&mut |v| mark.value(v)),
            Obj::Free(_) | Obj::Str(_) | Obj::Encoder(_) => {}
        }
    }
}

/// Marks reachable objects during a collection.
pub struct Marker<'a> {
    marks: &'a mut [u64],
    gray: &'a mut Vec<Handle>,
}

impl Marker<'_> {
    #[inline]
    pub fn value(&mut self, v: Value) {
        if let Value::Obj(h) = v {
            let (word, bit) = (h as usize / 64, 1u64 << (h % 64));
            if self.marks[word] & bit == 0 {
                self.marks[word] |= bit;
                debug_assert!(self.gray.len() < self.gray.capacity());
                self.gray.push(h);
            }
        }
    }

    pub fn values(&mut self, vs: &[Value]) {
        vs.iter().for_each(|v| self.value(*v));
    }
}

const NO_FREE_SLOT: u32 = u32::MAX;

/// Bytes per slot: the slot, its mark-worklist entry, and its mark bit.
pub const SLOT_BYTES: usize = size_of::<Obj>() + size_of::<Handle>() + 1;

pub struct Heap {
    slots: Vec<Obj>,
    marks: Vec<u64>,
    /// Mark worklist with capacity for every slot, so collection never allocates.
    gray: Vec<Handle>,
    free_head: u32,
    pub live_objects: usize,
    pub tracker: Tracker,
    pub policy: GcPolicy,
    /// Tracked byte count at which `Vm::reserve` leaves its fast path.
    pub next_check: usize,
}

impl Heap {
    pub fn new(limits: &Limits) -> Heap {
        let mut heap = Heap {
            slots: Vec::new(),
            marks: Vec::new(),
            gray: Vec::new(),
            free_head: NO_FREE_SLOT,
            live_objects: 0,
            tracker: Tracker::new(limits.max_memory),
            policy: GcPolicy::new(limits),
            next_check: 0,
        };
        heap.update_next_check();
        heap
    }

    pub fn update_next_check(&mut self) {
        self.next_check = self.policy.threshold().min(self.tracker.limit());
    }

    /// Returns the slot count to add before the next insert, or 0.
    #[inline]
    pub fn slot_growth(&self) -> usize {
        if self.free_head == NO_FREE_SLOT && self.slots.len() == self.slots.capacity() { self.slots.capacity().max(256) } else { 0 }
    }

    /// Adds `extra` slots of capacity, whose bytes the caller has reserved.
    pub fn grow_slots(&mut self, extra: usize) {
        self.slots.reserve_exact(extra);
        let capacity = self.slots.capacity();
        self.gray.reserve_exact(capacity - self.gray.len());
        self.marks.resize(capacity.div_ceil(64), 0);
    }

    /// Stores `obj`, whose bytes the caller has reserved.
    #[inline]
    pub fn insert(&mut self, obj: Obj) -> Value {
        self.live_objects += 1;
        if self.free_head != NO_FREE_SLOT {
            let h = self.free_head;
            let slot = &mut self.slots[h as usize];
            let Obj::Free(next) = *slot else { unreachable!("corrupt free list") };
            self.free_head = next;
            *slot = obj;
            Value::Obj(h)
        } else {
            debug_assert!(self.slots.len() < self.slots.capacity());
            self.slots.push(obj);
            Value::Obj((self.slots.len() - 1) as u32)
        }
    }

    #[inline]
    pub fn get(&self, h: Handle) -> &Obj {
        &self.slots[h as usize]
    }

    #[inline]
    pub fn get_mut(&mut self, h: Handle) -> &mut Obj {
        &mut self.slots[h as usize]
    }

    /// Returns the text of a string value.
    #[inline]
    pub fn str<'a>(&'a self, v: &'a Value) -> Option<&'a str> {
        match v {
            Value::Short(s) => Some(s.as_str()),
            Value::Obj(h) => match self.get(*h) {
                Obj::Str(s) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    /// Marks everything reachable from the values that `roots` visits and
    /// frees every other object.
    pub fn collect(&mut self, roots: impl FnOnce(&mut Marker)) {
        let mut mark = Marker { marks: &mut self.marks, gray: &mut self.gray };
        roots(&mut mark);
        while let Some(h) = mark.gray.pop() {
            self.slots[h as usize].trace(&mut mark);
        }
        let mut freed = 0;
        let mut freed_bytes = 0;
        for (word_index, word) in self.marks.iter_mut().enumerate() {
            let start = word_index * 64;
            let end = (start + 64).min(self.slots.len());
            for i in start..end {
                if *word & (1 << (i - start)) != 0 {
                    continue;
                }
                let slot = &mut self.slots[i];
                if matches!(slot, Obj::Free(_)) {
                    continue;
                }
                freed_bytes += slot.size();
                freed += 1;
                *slot = Obj::Free(self.free_head);
                self.free_head = i as u32;
            }
            *word = 0;
        }
        self.live_objects -= freed;
        self.tracker.sub_mut(freed_bytes);
    }
}
