//! The walk over a Sand value that reports each value to an [`EncodeSink`].
//!
//! The Sand trait `Encode` defines the values that can leave the VM. [`walk`]
//! reads such a value from the VM heap for a sink. The sink gives the value
//! another form, such as JSON text or a [`Value`](crate::value::Value).
//!
//! The walk keeps its position in each open container on a stack of frames
//! that it allocates, so its use of the Rust stack does not grow with the
//! depth of the value.

use super::bytecode::{Program, StructRt};
use super::heap::Heap;
use super::value::{Obj, ValueRef, VmValue};
use super::{Fail, fail, internal};
use smallvec::SmallVec;

/// The largest number of containers that the walk holds open.
///
/// A script can build a value that contains itself, so the walk fails with a
/// limit error instead of opening a container beyond this depth.
pub(crate) const MAX_DEPTH: usize = 512;

/// The number of open containers whose frames the walk stores without an
/// allocation. An allocation for each walk costs `json.encode` of a small
/// struct more than 10% of its time.
const INLINE_FRAMES: usize = 4;

/// The receiver of the values that [`walk`] reports.
///
/// The walk reports the children of a container in order, between the call of
/// [`EncodeSink::open`] and the call of [`EncodeSink::close`] for that
/// container. Each child has a [`Role`] in its container. The walk reports a
/// field value only with the name of its field. It reports no value beyond
/// the field names of a struct type or of a variant.
pub(crate) trait EncodeSink<'h> {
    /// The state of a container whose children the walk reports.
    type Open;

    /// The message of the limit error for a value that nests more than
    /// [`MAX_DEPTH`] containers.
    const TOO_DEEP: &'static str;

    /// Receives a value without children as the next child of `parent`.
    fn leaf(&mut self, parent: &mut Self::Open, role: Role<'h>, leaf: Leaf<'h>) -> Result<(), Fail>;

    /// Receives a value with `len` children as the next child of `parent`. A
    /// map has `len` entries.
    fn open(&mut self, parent: &mut Self::Open, role: Role<'h>, container: Container<'h>, len: usize) -> Result<Opened<Self::Open>, Fail>;

    /// Receives the end of the container `open`, which is a child of `parent`.
    fn close(&mut self, parent: &mut Self::Open, open: Self::Open) -> Result<(), Fail>;
}

/// The place of a value in its container.
#[derive(Clone, Copy)]
pub(crate) enum Role<'h> {
    /// The value that the walk starts from.
    Root,
    /// The payload that the walk reports in place of its container. The sink
    /// received the role of the container with the container.
    Payload,
    /// An item of a list, tuple, or set, or a value of a positional payload.
    Item,
    MapKey,
    /// The value of the map key that the walk reported last.
    MapValue,
    /// The value of the field with this name in a struct or in a named payload.
    Field(&'h str),
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
    Tag(VariantRef<'h>),
    /// A value of a native value type.
    Native(ValueRef<'h>),
    /// A value whose type does not implement `Encode`. `kind_name` is the
    /// [`Obj::kind_name`] of the value.
    Opaque {
        kind: OpaqueKind,
        kind_name: &'static str,
    },
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
    Variant(VariantRef<'h>),
}

/// An enum variant and its names.
#[derive(Clone, Copy)]
pub(crate) struct VariantRef<'h> {
    pub value: VmValue,
    /// The runtime variant ID.
    pub id: u32,
    pub name: &'h str,
    /// The names of the payload fields, or `None` for a positional payload.
    pub field_names: Option<&'h [String]>,
    enum_def: u32,
    types: &'h [StructRt],
}

impl<'h> VariantRef<'h> {
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

/// The answer of a sink to [`EncodeSink::open`].
pub(crate) enum Opened<O> {
    /// The walk reports the children, then closes the container with this state.
    Children(O),
    /// The walk reports the first child in place of the container, with the
    /// role [`Role::Payload`].
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
    let mut outer = SmallVec::<[Frame<'h, S::Open>; INLINE_FRAMES]>::new();
    let mut top = Frame { children: Children::One(Role::Root, Some(value)), open: root };
    // The payload that the walk reports before the next child of `top`.
    let mut payload = None;
    loop {
        let (role, child) = match payload.take() {
            Some(payload) => (Role::Payload, payload),
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
        match report(prog, heap, sink, &mut top.open, role, child)? {
            Reported::Closed => {}
            Reported::Opened(frame) => {
                if outer.len() >= MAX_DEPTH {
                    return fail("limit", S::TOO_DEEP);
                }
                outer.push(std::mem::replace(&mut top, frame));
            }
            Reported::Unwrapped(unwrapped) => payload = Some(unwrapped),
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
    role: Role<'h>,
    value: VmValue,
) -> Result<Reported<'h, S::Open>, Fail> {
    let opaque = |obj: &Obj, kind| Leaf::Opaque { kind, kind_name: obj.kind_name() };
    let (container, len, mut children) = match value {
        VmValue::Unit | VmValue::Uninit => return leaf(sink, parent, role, Leaf::Unit),
        VmValue::Bool(b) => return leaf(sink, parent, role, Leaf::Bool(b)),
        VmValue::Int(i) => return leaf(sink, parent, role, Leaf::Int(i)),
        VmValue::Float(f) => return leaf(sink, parent, role, Leaf::Float(f)),
        VmValue::Dur(d) => return leaf(sink, parent, role, Leaf::Duration(d)),
        VmValue::Tag(id) => return leaf(sink, parent, role, Leaf::Tag(variant_ref(prog, value, id)?)),
        VmValue::Pair(first, second) => (Container::Tuple, 2, Children::Pair(first, second)),
        VmValue::Obj(handle) => match heap.obj(handle) {
            Obj::Str(s) => return leaf(sink, parent, role, Leaf::Str { text: s, heap_str: Some(value) }),
            Obj::StrBuf(s) => return leaf(sink, parent, role, Leaf::Str { text: s, heap_str: None }),
            Obj::Bytes(b) => return leaf(sink, parent, role, Leaf::Bytes(b)),
            Obj::List(items) => (Container::List, items.len(), Children::Items(items.iter())),
            Obj::Tuple(items) => (Container::Tuple, items.len(), Children::Items(items.iter())),
            Obj::Set(set) => (Container::Set, set.live, Children::Set(set.entries.iter())),
            Obj::Map(map) => (Container::Map, map.live, Children::Map(map.entries.iter(), None)),
            Obj::Struct(def, values) => {
                let ty = &prog.types[*def as usize];
                fields(Container::Struct { def: *def, ty }, &ty.fields, values)
            }
            Obj::Variant(id, values) => {
                let variant = variant_ref(prog, value, *id)?;
                match variant.field_names {
                    Some(names) => fields(Container::Variant(variant), names, values),
                    None => (Container::Variant(variant), values.len(), Children::Items(values.iter())),
                }
            }
            Obj::Range(start, end, inclusive) => {
                return leaf(sink, parent, role, Leaf::Range { start: *start, end: *end, inclusive: *inclusive });
            }
            Obj::Value(native) => return leaf(sink, parent, role, Leaf::Native(ValueRef::Boxed(&**native))),
            Obj::Packed(ty, bits) => return leaf(sink, parent, role, Leaf::Native(ValueRef::Packed(ty, *bits))),
            obj @ (Obj::Closure(..) | Obj::NativeFn(_) | Obj::IterFn(_)) => {
                return leaf(sink, parent, role, opaque(obj, OpaqueKind::Function));
            }
            obj @ Obj::Task(_) => return leaf(sink, parent, role, opaque(obj, OpaqueKind::Task)),
            obj @ Obj::Channel(_) => return leaf(sink, parent, role, opaque(obj, OpaqueKind::Channel)),
            obj @ Obj::Native(object) => {
                let kind = OpaqueKind::Object { module: object.module_name(), name: object.type_name() };
                return leaf(sink, parent, role, opaque(obj, kind));
            }
            obj @ (Obj::TypeDesc(_) | Obj::Cell(_) | Obj::Free(_)) => return leaf(sink, parent, role, opaque(obj, OpaqueKind::Internal)),
        },
        VmValue::VUnit(..)
        | VmValue::VBool(..)
        | VmValue::VInt(..)
        | VmValue::VFloat(..)
        | VmValue::VDur(..)
        | VmValue::VTag(..)
        | VmValue::VObj(..) => {
            let Some((id, payload)) = value.as_inline_variant() else { return internal("encode: not an inline variant") };
            let variant = variant_ref(prog, value, id)?;
            let role = variant.field_names.and_then(<[String]>::first).map_or(Role::Item, |name| Role::Field(name));
            (Container::Variant(variant), 1, Children::One(role, Some(payload)))
        }
    };
    Ok(match sink.open(parent, role, container, len)? {
        Opened::Children(open) => Reported::Opened(Frame { children, open }),
        Opened::Unwrapped => Reported::Unwrapped(children.next().map_or(VmValue::Unit, |(_, payload)| payload)),
        Opened::Closed => Reported::Closed,
    })
}

#[inline(always)]
fn leaf<'h, S: EncodeSink<'h>>(sink: &mut S, parent: &mut S::Open, role: Role<'h>, leaf: Leaf<'h>) -> Result<Reported<'h, S::Open>, Fail> {
    sink.leaf(parent, role, leaf)?;
    Ok(Reported::Closed)
}

/// Returns `container` with a child for each of `names` that has a value in
/// `values`.
#[inline(always)]
fn fields<'h>(container: Container<'h>, names: &'h [String], values: &'h [VmValue]) -> (Container<'h>, usize, Children<'h>) {
    let len = names.len().min(values.len());
    (container, len, Children::Fields(names[..len].iter(), values[..len].iter()))
}

#[inline(always)]
fn variant_ref(prog: &Program, value: VmValue, id: u32) -> Result<VariantRef<'_>, Fail> {
    let Some(rt) = prog.variants.get(id as usize) else { return internal("encode: unknown variant ID") };
    Ok(VariantRef { value, id, name: &rt.name, field_names: rt.field_names.as_deref(), enum_def: rt.def, types: &prog.types })
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
    /// The entries of a map from the next key on, and the value of the key that
    /// the walk reported last, until the walk reports that value.
    Map(std::slice::Iter<'h, Option<(VmValue, VmValue)>>, Option<VmValue>),
    /// The field names and the field values of a struct or of a named payload.
    Fields(std::slice::Iter<'h, String>, std::slice::Iter<'h, VmValue>),
    /// One child and its role.
    One(Role<'h>, Option<VmValue>),
    /// The two integers of an inline tuple.
    Pair(i32, i64),
}

impl<'h> Children<'h> {
    #[inline(always)]
    fn next(&mut self) -> Option<(Role<'h>, VmValue)> {
        let item = |value| (Role::Item, value);
        match self {
            Children::Items(items) => items.next().copied().map(item),
            Children::Set(entries) => entries.find_map(|entry| *entry).map(item),
            Children::Map(entries, value) => match value.take() {
                Some(value) => Some((Role::MapValue, value)),
                None => {
                    let (key, key_value) = entries.find_map(|entry| *entry)?;
                    *value = Some(key_value);
                    Some((Role::MapKey, key))
                }
            },
            Children::Fields(names, values) => Some((Role::Field(names.next()?.as_str()), *values.next()?)),
            Children::One(role, value) => Some((*role, value.take()?)),
            Children::Pair(first, second) => {
                let first = VmValue::Int(i64::from(*first));
                *self = Children::One(Role::Item, Some(VmValue::Int(*second)));
                Some(item(first))
            }
        }
    }
}
