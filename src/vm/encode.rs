//! The walk over a Sand value that reports each value to an [`EncodeSink`].
//!
//! The Sand trait `Encode` defines the values that can leave the VM. [`walk`]
//! is the only code that reads such a value from the VM heap. A sink gives the
//! value a form outside the heap, such as JSON text or a
//! [`Value`](crate::value::Value).
//!
//! The walk keeps its position in each open container on a stack of frames
//! that it allocates, so its use of the Rust stack does not grow with the
//! depth of the value.

use super::bytecode::{Program, StructRt};
use super::heap::Heap;
use super::value::{Obj, ValueRef, VmValue};
use super::{Fail, internal};

/// The largest number of containers that a sink lets the walk nest.
///
/// A script can build a value that contains itself, so each sink must fail
/// with a limit error instead of opening a container beyond this depth.
pub(crate) const MAX_DEPTH: usize = 512;

/// The receiver of the values that [`walk`] reports.
///
/// The walk reports the children of a container in order, between the call of
/// [`EncodeSink::open`] and the call of [`EncodeSink::close`] for that
/// container. The children of a map are each key followed by its value. The
/// children of a struct or a variant are its field values in declaration order.
pub(crate) trait EncodeSink<'h> {
    /// The state of a container whose children the walk reports.
    type Open;

    /// Receives a value without children as the next child of `parent`.
    fn leaf(&mut self, parent: &mut Self::Open, leaf: Leaf<'h>) -> Result<(), Fail>;

    /// Receives a value with `len` children as the next child of `parent`. A
    /// map has `len` entries.
    fn open(&mut self, parent: &mut Self::Open, container: Container<'h>, len: usize) -> Result<Opened<Self::Open>, Fail>;

    /// Receives the end of the container `open`, which is a child of `parent`.
    fn close(&mut self, parent: &mut Self::Open, open: Self::Open) -> Result<(), Fail>;
}

/// A value without children.
pub(crate) enum Leaf<'h> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A `duration` in nanoseconds.
    Duration(i64),
    /// A string. `heap_str` is the value if it is an immutable `str` on the
    /// heap, which a sink can reference instead of a copy of `text`.
    Str {
        text: &'h str,
        heap_str: Option<VmValue>,
    },
    Bytes(&'h [u8]),
    Range {
        start: i64,
        end: i64,
        inclusive: bool,
    },
    /// An enum variant without a payload.
    Tag(EnumVariant<'h>),
    /// A value of a native value type.
    Native(ValueRef<'h>),
    /// A value whose type does not implement `Encode`.
    Opaque(OpaqueKind),
}

/// A value with children.
pub(crate) enum Container<'h> {
    List,
    Tuple,
    Set,
    Map,
    /// A struct of the type `ty`, which has the runtime type ID `def`.
    Struct {
        def: u32,
        ty: &'h StructRt,
    },
    /// An enum variant with a payload.
    Variant(EnumVariant<'h>),
}

/// An enum variant and its names.
#[derive(Clone, Copy)]
pub(crate) struct EnumVariant<'h> {
    pub value: VmValue,
    /// The runtime variant ID.
    pub id: u32,
    pub name: &'h str,
    /// The names of the payload fields, or `None` for a positional payload.
    pub field_names: Option<&'h [String]>,
    enum_def: u32,
    types: &'h [StructRt],
}

impl<'h> EnumVariant<'h> {
    pub(crate) fn enum_type(&self) -> &'h StructRt {
        &self.types[self.enum_def as usize]
    }
}

/// The kind of a value whose type does not implement `Encode`.
#[derive(Clone, Copy)]
pub(crate) enum OpaqueKind {
    Function,
    Task,
    Channel,
    /// A native object of the type `name` from the module `module`.
    Object {
        module: &'static str,
        name: &'static str,
    },
    /// A heap object that a script cannot hold.
    Internal,
}

impl OpaqueKind {
    /// Returns the kind name that [`Obj::kind_name`] gives the value.
    pub(crate) fn kind_name(self) -> &'static str {
        match self {
            OpaqueKind::Function => "function",
            OpaqueKind::Task => "task",
            OpaqueKind::Channel => "channel",
            OpaqueKind::Object { .. } => "native object",
            OpaqueKind::Internal => "internal object",
        }
    }
}

/// The answer of a sink to [`EncodeSink::open`].
pub(crate) enum Opened<O> {
    /// The walk reports the children, then closes the container with this state.
    Children(O),
    /// The walk reports the first child in place of the container.
    Unwrapped,
    /// The sink has finished the container, and the walk skips the children.
    Closed,
}

/// Reports `value` to `sink` as the only child of `root`, and returns `root`.
pub(crate) fn walk<'h, S: EncodeSink<'h>>(
    prog: &'h Program,
    heap: &'h Heap,
    sink: &mut S,
    root: S::Open,
    value: VmValue,
) -> Result<S::Open, Fail> {
    let mut outer = OuterFrames::new();
    let mut top = Frame { children: Children::One(None), open: root };
    // The value that the walk reports before the next child of `top`.
    let mut first = Some(value);
    loop {
        let child = match first.take() {
            Some(child) => child,
            None => match top.children.next() {
                Some(child) => child,
                None => {
                    let Some(parent) = outer.pop() else { return Ok(top.open) };
                    let closed = std::mem::replace(&mut top, parent);
                    sink.close(&mut top.open, closed.open)?;
                    continue;
                }
            },
        };
        match report(prog, heap, sink, &mut top.open, child)? {
            Reported::Closed => {}
            Reported::Opened(frame) => outer.push(std::mem::replace(&mut top, frame)),
            Reported::Unwrapped(payload) => first = Some(payload),
        }
    }
}

/// What the walk does after it reports a value.
enum Reported<'h, O> {
    /// The walk continues with the next child of the same parent.
    Closed,
    /// The walk continues with the children of the value in this frame.
    Opened(Frame<'h, O>),
    /// The walk reports this value next, as a child of the same parent.
    Unwrapped(VmValue),
}

/// Reports `value` as the next child of `parent`.
#[inline(always)]
fn report<'h, S: EncodeSink<'h>>(
    prog: &'h Program,
    heap: &'h Heap,
    sink: &mut S,
    parent: &mut S::Open,
    value: VmValue,
) -> Result<Reported<'h, S::Open>, Fail> {
    let (container, len, mut children) = match value {
        VmValue::Unit | VmValue::Uninit => return leaf(sink, parent, Leaf::Unit),
        VmValue::Bool(b) => return leaf(sink, parent, Leaf::Bool(b)),
        VmValue::Int(i) => return leaf(sink, parent, Leaf::Int(i)),
        VmValue::Float(f) => return leaf(sink, parent, Leaf::Float(f)),
        VmValue::Dur(d) => return leaf(sink, parent, Leaf::Duration(d)),
        VmValue::Tag(id) => return leaf(sink, parent, Leaf::Tag(enum_variant(prog, value, id)?)),
        VmValue::Pair(first, second) => (Container::Tuple, 2, Children::Pair(first, second)),
        VmValue::Obj(handle) => match heap.obj(handle) {
            Obj::Str(s) => return leaf(sink, parent, Leaf::Str { text: s, heap_str: Some(value) }),
            Obj::StrBuf(s) => return leaf(sink, parent, Leaf::Str { text: s, heap_str: None }),
            Obj::Bytes(b) => return leaf(sink, parent, Leaf::Bytes(b)),
            Obj::List(items) => (Container::List, items.len(), Children::Items(items.iter())),
            Obj::Tuple(items) => (Container::Tuple, items.len(), Children::Items(items.iter())),
            Obj::Set(set) => (Container::Set, set.live, Children::Set(set.entries.iter())),
            Obj::Map(map) => (Container::Map, map.live, Children::Map(map.entries.iter(), false)),
            Obj::Struct(def, fields) => {
                let ty = &prog.types[*def as usize];
                let len = fields.len().min(ty.fields.len());
                (Container::Struct { def: *def, ty }, len, Children::Items(fields[..len].iter()))
            }
            Obj::Variant(id, fields) => {
                let variant = enum_variant(prog, value, *id)?;
                let len = variant.field_names.map_or(fields.len(), |names| fields.len().min(names.len()));
                (Container::Variant(variant), len, Children::Items(fields[..len].iter()))
            }
            Obj::Range(start, end, inclusive) => {
                return leaf(sink, parent, Leaf::Range { start: *start, end: *end, inclusive: *inclusive });
            }
            Obj::Value(native) => return leaf(sink, parent, Leaf::Native(ValueRef::Boxed(&**native))),
            Obj::Packed(ty, bits) => return leaf(sink, parent, Leaf::Native(ValueRef::Packed(ty, *bits))),
            Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_) => return leaf(sink, parent, Leaf::Opaque(OpaqueKind::Function)),
            Obj::Task(_) => return leaf(sink, parent, Leaf::Opaque(OpaqueKind::Task)),
            Obj::Channel(_) => return leaf(sink, parent, Leaf::Opaque(OpaqueKind::Channel)),
            Obj::Native(object) => {
                let kind = OpaqueKind::Object { module: object.module_name(), name: object.type_name() };
                return leaf(sink, parent, Leaf::Opaque(kind));
            }
            Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_) => return leaf(sink, parent, Leaf::Opaque(OpaqueKind::Internal)),
        },
        VmValue::VUnit(..)
        | VmValue::VBool(..)
        | VmValue::VInt(..)
        | VmValue::VFloat(..)
        | VmValue::VDur(..)
        | VmValue::VTag(..)
        | VmValue::VObj(..) => {
            let Some((id, payload)) = value.as_inline_variant() else { return internal("encode: not an inline variant") };
            (Container::Variant(enum_variant(prog, value, id)?), 1, Children::One(Some(payload)))
        }
    };
    Ok(match sink.open(parent, container, len)? {
        Opened::Children(open) => Reported::Opened(Frame { children, open }),
        Opened::Unwrapped => Reported::Unwrapped(children.next().unwrap_or(VmValue::Unit)),
        Opened::Closed => Reported::Closed,
    })
}

#[inline(always)]
fn leaf<'h, S: EncodeSink<'h>>(sink: &mut S, parent: &mut S::Open, leaf: Leaf<'h>) -> Result<Reported<'h, S::Open>, Fail> {
    sink.leaf(parent, leaf)?;
    Ok(Reported::Closed)
}

#[inline(always)]
fn enum_variant(prog: &Program, value: VmValue, id: u32) -> Result<EnumVariant<'_>, Fail> {
    let Some(rt) = prog.variants.get(id as usize) else { return internal("encode: unknown variant ID") };
    Ok(EnumVariant { value, id, name: &rt.name, field_names: rt.field_names.as_deref(), enum_def: rt.def, types: &prog.types })
}

/// An open container: its children that the walk has not reported, and the
/// state that the sink keeps for it.
struct Frame<'h, O> {
    children: Children<'h>,
    open: O,
}

/// The children of a container that the walk has not reported.
enum Children<'h> {
    Items(std::slice::Iter<'h, VmValue>),
    Set(std::slice::Iter<'h, Option<VmValue>>),
    /// The entries of a map from the next key on. The flag is true if the walk
    /// reported the key of the first entry and its value is next.
    Map(std::slice::Iter<'h, Option<(VmValue, VmValue)>>, bool),
    One(Option<VmValue>),
    /// The two integers of an inline tuple.
    Pair(i32, i64),
}

impl Children<'_> {
    #[inline(always)]
    fn next(&mut self) -> Option<VmValue> {
        match self {
            Children::Items(items) => items.next().copied(),
            Children::Set(entries) => entries.find_map(|entry| *entry),
            Children::Map(entries, value_is_next @ true) => {
                *value_is_next = false;
                entries.next().copied().flatten().map(|(_, value)| value)
            }
            Children::Map(entries, value_is_next) => loop {
                match entries.as_slice().first()? {
                    Some((key, _)) => {
                        *value_is_next = true;
                        return Some(*key);
                    }
                    None => entries.next(),
                };
            },
            Children::One(value) => value.take(),
            Children::Pair(first, second) => {
                let first = VmValue::Int(i64::from(*first));
                *self = Children::One(Some(VmValue::Int(*second)));
                Some(first)
            }
        }
    }
}

/// The number of frames that [`OuterFrames`] stores without an allocation.
const INLINE_FRAMES: usize = 4;
/// The number of frames in the first allocation of [`OuterFrames`].
const FIRST_SPILLED_FRAMES: usize = 16;

/// The frames of the open containers around the innermost one.
///
/// The first frames are stored inline, so the walk of a value with few levels
/// does not allocate. An allocation for each walk costs `json.encode` of a
/// small struct more than 10% of its time.
struct OuterFrames<F> {
    inline: [Option<F>; INLINE_FRAMES],
    len: usize,
    spilled: Vec<F>,
}

impl<F> OuterFrames<F> {
    fn new() -> Self {
        OuterFrames { inline: [const { None }; INLINE_FRAMES], len: 0, spilled: Vec::new() }
    }

    #[inline(always)]
    fn push(&mut self, frame: F) {
        match self.inline.get_mut(self.len) {
            Some(slot) => *slot = Some(frame),
            None => self.spill(frame),
        }
        self.len += 1;
    }

    /// Reserves the space for [`MAX_DEPTH`] frames when the first reservation
    /// is full, so that the frames of a deep value move only one time.
    #[inline(never)]
    fn spill(&mut self, frame: F) {
        if self.spilled.len() == self.spilled.capacity() {
            self.spilled.reserve_exact(if self.spilled.is_empty() { FIRST_SPILLED_FRAMES } else { MAX_DEPTH });
        }
        self.spilled.push(frame);
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<F> {
        self.len = self.len.checked_sub(1)?;
        match self.inline.get_mut(self.len) {
            Some(slot) => slot.take(),
            None => self.spilled.pop(),
        }
    }
}
