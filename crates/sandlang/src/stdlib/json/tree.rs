//! The `json.Json` value format of `Decoder`, for `json.from_json`.

use super::input::{self, JsonKind, RangeFields};
use super::text::json_value;
use super::{JsonIds, json_from_vm};
use crate::codec::{DecodeFail, DecodeFormat, DecodeResult, MAX_NESTING_DEPTH, SeqKind};
use crate::value::{Value, integral_float_to_int};
use crate::vm::value::{Obj, ObjectValues, VALUE_SIZE, VmValue};
use crate::vm::{Fail, Vm};
use std::borrow::Cow;

/// The part of the current entry of an open object that the next operation
/// reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryPart {
    Key,
    Value,
}

/// An open array or object of a [`TreeDecoder`].
#[derive(Clone, Copy)]
struct Open {
    /// The index of the `Json.Arr` list or the `Json.Obj` map in the values
    /// of the decoder.
    at: usize,
    /// The index of the current element or entry, if any.
    pos: Option<usize>,
    /// For an object, the part of the current entry that the next
    /// operation reads.
    part: Option<EntryPart>,
}

/// The state of a [`TreeDecoder`] that [`DecodeFormat::mark`] keeps.
#[derive(Clone, Copy)]
struct TreeMark {
    values: usize,
    open: usize,
    top: Option<Open>,
    ready: bool,
}

/// Reads a Sand `json.Json` value in place, with the rules of JSON text
/// (see [`input`]). A `str` in the input becomes the decoded `str`, without a
/// copy.
pub(crate) struct TreeDecoder {
    /// The root `Json` value, and then the list or map of each open
    /// container. A `Decoder` keeps them, where the collector traces them,
    /// and lends them to the format for each native call (see
    /// [`DecodeFormat::swap_values`]).
    values: ObjectValues,
    open: Vec<Open>,
    /// Whether the current node waits for a read.
    ready: bool,
    marks: Vec<TreeMark>,
    ids: JsonIds,
    /// The text of the last key or variant name, which `next_entry`,
    /// `next_field`, and `variant` return.
    key: String,
}

/// A node of the input.
#[derive(Clone, Copy)]
enum Node {
    /// A `Json` value: its variant and its payload.
    Json(u32, Option<VmValue>),
    /// The `str` key of an object entry.
    Key(VmValue),
}

impl TreeDecoder {
    /// Returns a decoder of the `Json` value `root`, or of the values that a
    /// `Decoder` lends it if `root` is `None`.
    pub(crate) fn new(vm: &Vm, root: Option<VmValue>) -> Result<TreeDecoder, Fail> {
        let ids = JsonIds::new(&vm.prog)?;
        let values = root.into_iter().collect();
        Ok(TreeDecoder { values, open: Vec::new(), ready: true, marks: Vec::new(), ids, key: String::new() })
    }

    /// Stops counting the values that the decoder holds, after a native
    /// call that kept the decoder on the Rust stack.
    pub(crate) fn release_values(self, vm: &mut Vm) {
        let grown = self.values.capacity() - self.values.inline_size();
        vm.heap.account(grown * VALUE_SIZE, 0);
    }

    /// Returns the node that the next operation reads.
    fn node(&self, vm: &Vm) -> DecodeResult<Node> {
        if !self.ready {
            return Err(Fail::internal("decode: no value to read").into());
        }
        let v = match self.open.last() {
            None => self.values.first().copied(),
            Some(&Open { at, pos: Some(i), part }) => {
                let container = self.values[at];
                match (vm.heap.get(container), part) {
                    (Obj::List(items), None) => items.get(i).copied(),
                    (Obj::Map(m), Some(part)) => match m.entries.get(i).copied().flatten() {
                        Some((k, _)) if part == EntryPart::Key => return Ok(Node::Key(k)),
                        Some((_, v)) => Some(v),
                        None => None,
                    },
                    _ => None,
                }
            }
            Some(_) => None,
        };
        // A hand-written `Decode` can change a list or a map of the input.
        let Some(v) = v else { return Err(DecodeFail::Mismatch("the input changed while it was decoded".into())) };
        Ok(Node::Json(vm.heap.variant_id(v), vm.heap.field(v, 0)))
    }

    /// Returns the `VmValue` of the current node.
    fn node_value(&self, vm: &Vm) -> Option<VmValue> {
        let open = self.open.last()?;
        let i = open.pos?;
        match vm.heap.get(self.values[open.at]) {
            Obj::List(items) => items.get(i).copied(),
            Obj::Map(m) => m.entries.get(i).copied().flatten().map(|(k, v)| if open.part == Some(EntryPart::Key) { k } else { v }),
            _ => None,
        }
    }

    /// Returns the kind of the `Json` variant `id`.
    fn kind(&self, id: u32) -> JsonKind {
        let ids = &self.ids;
        match id {
            _ if id == ids.null => JsonKind::Null,
            _ if id == ids.bool_ => JsonKind::Bool,
            _ if id == ids.int || id == ids.float => JsonKind::Number,
            _ if id == ids.str_ => JsonKind::String,
            _ if id == ids.arr => JsonKind::Array,
            _ => JsonKind::Object,
        }
    }

    /// Reads the current node with `read`, which gets the text of a map key,
    /// or the variant and the payload of a `Json` value.
    fn read<T>(&mut self, vm: &Vm, read: impl FnOnce(&Self, Result<&str, (u32, Option<VmValue>)>) -> DecodeResult<T>) -> DecodeResult<T> {
        let r = match self.node(vm)? {
            Node::Key(k) => read(self, Ok(vm.heap.str(k))),
            Node::Json(id, payload) => read(self, Err((id, payload))),
        }?;
        self.ready = false;
        Ok(r)
    }

    /// Reads an `int` from the `Json` value with variant `id` and `payload`.
    fn int_of(&self, id: u32, payload: Option<VmValue>) -> DecodeResult<i64> {
        match payload {
            Some(VmValue::Int(i)) if id == self.ids.int => Ok(i),
            Some(VmValue::Float(f)) if id == self.ids.float => {
                integral_float_to_int(f).ok_or_else(|| DecodeFail::Mismatch(input::not_int(f)))
            }
            _ => Err(input::mismatch("int", self.kind(id))),
        }
    }

    /// Counts room for one more value against the memory limit, before any
    /// change.
    fn room(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        if self.values.len() < self.values.capacity() {
            return Ok(());
        }
        let (before, after) = (self.values.capacity(), self.values.capacity().max(4) * 2);
        vm.check_memory((after - before) * VALUE_SIZE)?;
        vm.heap.account(before * VALUE_SIZE, after * VALUE_SIZE);
        self.values.reserve_exact(after - self.values.len());
        Ok(())
    }

    /// Opens `container`, the list of a `Json.Arr` or the map of a
    /// `Json.Obj`.
    fn enter(&mut self, vm: &mut Vm, container: VmValue, is_object: bool) -> DecodeResult<()> {
        if self.open.len() >= MAX_NESTING_DEPTH {
            return Err(DecodeFail::Mismatch("value too deeply nested to decode".into()));
        }
        self.room(vm)?;
        self.values.push(container);
        self.open.push(Open { at: self.values.len() - 1, pos: None, part: is_object.then_some(EntryPart::Key) });
        self.ready = false;
        Ok(())
    }

    /// Closes the innermost container.
    fn leave(&mut self) {
        if let Some(open) = self.open.pop() {
            self.values.truncate(open.at);
        }
        self.ready = false;
    }

    /// Moves to the next element of the innermost array. Returns `false`
    /// after the last.
    fn next_element(&mut self, vm: &Vm) -> DecodeResult<bool> {
        let Some(open) = self.open.last_mut().filter(|o| o.part.is_none()) else {
            return Err(Fail::internal("next_item: no open array").into());
        };
        let i = open.pos.map_or(0, |i| i + 1);
        if i < vm.heap.list(self.values[open.at]).len() {
            open.pos = Some(i);
            self.ready = true;
            return Ok(true);
        }
        self.leave();
        Ok(false)
    }

    /// Moves to the next entry of the innermost object, at `part`. Returns
    /// the text of its key, or `None` after the last entry.
    fn next_member(&mut self, vm: &Vm, part: EntryPart) -> DecodeResult<Option<Cow<'_, str>>> {
        let Some(open) = self.open.last_mut().filter(|o| o.part.is_some()) else {
            return Err(Fail::internal("next_entry: no open object").into());
        };
        let Obj::Map(m) = vm.heap.get(self.values[open.at]) else { return Err(Fail::internal("json: not an object").into()) };
        let from = open.pos.map_or(0, |i| i + 1);
        let Some((i, key)) = m.entries.get(from..).and_then(|rest| rest.iter().enumerate().find_map(|(k, e)| Some((from + k, (*e)?.0))))
        else {
            self.leave();
            return Ok(None);
        };
        (open.pos, open.part) = (Some(i), Some(part));
        self.ready = true;
        self.key.clear();
        self.key.push_str(vm.heap.str(key));
        Ok(Some(Cow::Borrowed(&self.key)))
    }
}

impl TreeDecoder {
    /// Opens an object, which is a value of `what` for a message.
    fn begin_object(&mut self, vm: &mut Vm, what: &str) -> DecodeResult<()> {
        match self.node(vm)? {
            Node::Key(_) => Err(input::container_key(what)),
            Node::Json(id, Some(map)) if id == self.ids.obj => self.enter(vm, map, true),
            Node::Json(id, _) => Err(input::mismatch("object", self.kind(id))),
        }
    }
}

impl DecodeFormat for TreeDecoder {
    fn unit(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        self.read(vm, |t, node| match node {
            Ok(key) => Err(input::invalid_key(key)),
            Err((id, _)) if id == t.ids.null => Ok(()),
            Err((id, _)) => Err(input::mismatch("null", t.kind(id))),
        })
    }

    fn bool(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        self.read(vm, |t, node| match node {
            Ok(key) => input::key_bool(key),
            Err((id, Some(VmValue::Bool(b)))) if id == t.ids.bool_ => Ok(b),
            Err((id, _)) => Err(input::mismatch("bool", t.kind(id))),
        })
    }

    fn int(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        self.read(vm, |t, node| match node {
            Ok(key) => input::key_int(key),
            Err((id, payload)) => t.int_of(id, payload),
        })
    }

    fn float(&mut self, vm: &mut Vm) -> DecodeResult<f64> {
        self.read(vm, |t, node| match node {
            Ok(_) => Err(input::container_key("float")),
            Err((id, Some(VmValue::Int(i)))) if id == t.ids.int => Ok(i as f64),
            Err((id, Some(VmValue::Float(f)))) if id == t.ids.float => Ok(f),
            Err((id, _)) => Err(input::mismatch("float", t.kind(id))),
        })
    }

    fn duration(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        self.int(vm)
    }

    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let s = match self.node(vm)? {
            Node::Key(k) => k,
            Node::Json(id, Some(s)) if id == self.ids.str_ => s,
            Node::Json(id, _) => return Err(input::mismatch("str", self.kind(id))),
        };
        self.ready = false;
        Ok(s)
    }

    fn bytes(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let b = match self.node(vm)? {
            Node::Key(_) => return Err(input::container_key("bytes")),
            Node::Json(id, Some(s)) if id == self.ids.str_ => input::bytes_of(vm.heap.str(s)).map_err(DecodeFail::Mismatch)?,
            Node::Json(id, _) => return Err(input::mismatch("base64 str", self.kind(id))),
        };
        vm.check_memory(b.len())?;
        vm.charge_bytes(b.len())?;
        self.ready = false;
        Ok(vm.heap.new_bytes(b))
    }

    fn native_text(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<String> {
        self.read(vm, |t, node| match node {
            Ok(key) => Ok(key.to_owned()),
            Err((id, Some(s))) if id == t.ids.str_ => Ok(vm.heap.str(s).to_owned()),
            Err((id, _)) => Err(input::mismatch(name, t.kind(id))),
        })
    }

    fn range(&mut self, vm: &mut Vm) -> DecodeResult<(i64, i64, bool)> {
        let map = match self.node(vm)? {
            Node::Key(_) => return Err(input::container_key("Range")),
            Node::Json(id, Some(map)) if id == self.ids.obj => map,
            Node::Json(id, _) => return Err(input::mismatch("object", self.kind(id))),
        };
        let Obj::Map(m) = vm.heap.get(map) else { return Err(Fail::internal("json: not an object").into()) };
        let mut range = RangeFields::default();
        for &(k, v) in m.entries.iter().flatten() {
            let (id, payload) = (vm.heap.variant_id(v), vm.heap.field(v, 0));
            match vm.heap.str(k) {
                "start" => range.start = Some(self.int_of(id, payload)?),
                "end" => range.end = Some(self.int_of(id, payload)?),
                "inclusive" => {
                    range.inclusive = match payload {
                        Some(VmValue::Bool(b)) if id == self.ids.bool_ => b,
                        _ => return Err(input::mismatch("bool", self.kind(id))),
                    }
                }
                _ => {}
            }
        }
        let r = range.finish()?;
        self.ready = false;
        Ok(r)
    }

    fn some(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        match self.node(vm)? {
            Node::Json(id, _) if id == self.ids.null => {
                self.ready = false;
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    fn begin_seq(&mut self, vm: &mut Vm, kind: SeqKind) -> DecodeResult<()> {
        match self.node(vm)? {
            Node::Key(_) => Err(input::container_key(input::seq_key_type(kind))),
            Node::Json(id, Some(list)) if id == self.ids.arr => self.enter(vm, list, false),
            Node::Json(id, _) => Err(input::mismatch(&input::seq_expected(kind), self.kind(id))),
        }
    }

    fn next_item(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        self.next_element(vm)
    }

    fn begin_map(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        self.begin_object(vm, "map")
    }

    fn next_entry(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        self.next_member(vm, EntryPart::Key)
    }

    fn map_value(&mut self, _: &mut Vm) -> DecodeResult<()> {
        let Some(open) = self.open.last_mut().filter(|o| o.part.is_some()) else {
            return Err(Fail::internal("map_value: no open object").into());
        };
        open.part = Some(EntryPart::Value);
        self.ready = true;
        Ok(())
    }

    fn begin_fields(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()> {
        self.begin_object(vm, if name.is_empty() { "variant" } else { name })
    }

    fn next_field(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        self.next_member(vm, EntryPart::Value)
    }

    fn skip(&mut self, _: &mut Vm) -> DecodeResult<()> {
        self.ready = false;
        Ok(())
    }

    fn variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)> {
        let tag = match self.node(vm)? {
            Node::Key(k) => k,
            Node::Json(id, Some(s)) if id == self.ids.str_ => s,
            Node::Json(id, Some(map)) if id == self.ids.obj => {
                self.enter(vm, map, true)?;
                let Some(tag) = self.next_member(vm, EntryPart::Value)? else { return Err(input::no_variant(name)) };
                return Ok((tag, true));
            }
            Node::Json(id, _) => return Err(input::mismatch(name, self.kind(id))),
        };
        self.ready = false;
        self.key.clear();
        self.key.push_str(vm.heap.str(tag));
        Ok((Cow::Borrowed(&self.key), false))
    }

    fn end_variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<()> {
        if self.next_member(vm, EntryPart::Value)?.is_some() {
            return Err(input::not_single_variant(name));
        }
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let v = match self.node(vm)? {
            Node::Key(_) => return Err(input::container_key("Json")),
            Node::Json(..) => match self.open.is_empty() {
                true => self.values[0],
                false => self.node_value(vm).ok_or_else(|| Fail::internal("json: no value"))?,
            },
        };
        self.ready = false;
        Ok(v)
    }

    fn finish(&mut self, _: &mut Vm) -> DecodeResult<()> {
        Ok(())
    }

    fn value(&mut self, vm: &mut Vm) -> DecodeResult<Value> {
        let v = self.json(vm)?;
        Ok(json_value(&json_from_vm(vm, v)?))
    }

    fn mark(&mut self) -> usize {
        let (values, open, top) = (self.values.len(), self.open.len(), self.open.last().copied());
        self.marks.push(TreeMark { values, open, top, ready: self.ready });
        self.marks.len() - 1
    }

    fn rewind(&mut self, mark: usize) {
        let Some(&m) = self.marks.get(mark) else { return };
        self.values.truncate(m.values);
        self.open.truncate(m.open);
        if let (Some(top), Some(last)) = (m.top, self.open.last_mut()) {
            *last = top;
        }
        self.ready = m.ready;
    }

    fn release(&mut self, mark: usize) {
        self.marks.truncate(mark);
    }

    fn rewind_to_start(&mut self) {
        self.values.truncate(1);
        self.open.clear();
        self.ready = true;
    }

    fn keeps_values(&self) -> bool {
        true
    }

    fn swap_values(&mut self, values: &mut ObjectValues) {
        std::mem::swap(&mut self.values, values);
    }

    fn heap_size(&self) -> usize {
        self.open.capacity() * size_of::<Open>() + self.marks.capacity() * size_of::<TreeMark>() + self.key.capacity()
    }
}
