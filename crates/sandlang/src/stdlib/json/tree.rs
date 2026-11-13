//! The `json.Json` value format of `Encoder` and `Decoder`, for `json.to_json`
//! and `json.from_json`.

use super::input::{self, JsonKind, RangeFields};
use super::text::{emit_json, field_name, invalid_key_error, json_value};
use super::{JsonIds, base64, json_from_vm};
use crate::codec::value_format::write_json_form;
use crate::codec::{Budget, DecodeFail, DecodeFormat, DecodeResult, EncodeFormat, MAX_NESTING_DEPTH, SeqKind};
use crate::value::{Value, integral_float_to_int};
use crate::vm::compare::str_hash;
use crate::vm::value::{FxHashMap, KeyIndex, MAP_ENTRY_SIZE, MapObj, slot_fits};
use crate::vm::value::{Obj, ObjectValues, VALUE_SIZE, VmValue};
use crate::vm::{Fail, Vm};
use crate::vm::{fail, internal};
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

/// An open container of a [`TreeEncoder`], with the index of its first part
/// in the encoder's values.
#[derive(Clone, Copy)]
enum TreeOpen {
    Arr(usize),
    /// An object whose member keys come from `field` calls on the struct or
    /// the variant `names`, or from the bounds of a `Range`.
    Fields(usize, Option<(u32, bool)>),
    /// A map, and `true` while the next value is a key.
    Map(usize, bool),
    /// The outer object of a variant with a payload: the variant name, then
    /// the payload.
    VariantWrapper(usize),
    /// `Some`, which writes only its payload.
    Some,
}

/// The state of a [`TreeEncoder`] that [`EncodeFormat::save`] keeps.
#[derive(Clone, Copy)]
struct TreeState {
    values: usize,
    open: usize,
    top: Option<TreeOpen>,
    depth: usize,
}

/// Builds a `json.Json` value as the encode runs.
pub(crate) struct TreeEncoder {
    /// The finished parts of the open containers, and then the finished
    /// value. The `Encoder` keeps them, where the collector traces them, and
    /// lends them to the format for each native call (see
    /// [`EncodeFormat::swap_values`]).
    values: ObjectValues,
    open: Vec<TreeOpen>,
    /// The number of open arrays and objects.
    depth: usize,
    saved: Option<TreeState>,
    /// The key `str` of each field and variant name that this native call
    /// made.
    names: FxHashMap<(u32, bool, u32), VmValue>,
    /// The hashes of the texts of those keys, by heap handle.
    name_hashes: FxHashMap<u32, u64>,
    /// The variant ids of `Json`, from the first call.
    ids: Option<JsonIds>,
    budget: Budget,
}

impl TreeEncoder {
    pub(crate) fn new() -> TreeEncoder {
        TreeEncoder {
            values: ObjectValues::new(),
            open: Vec::new(),
            depth: 0,
            saved: None,
            names: FxHashMap::default(),
            name_hashes: FxHashMap::default(),
            ids: None,
            budget: Budget::default(),
        }
    }

    /// Returns the encoded value.
    pub(crate) fn encoded(&self) -> Result<VmValue, Fail> {
        match (self.open.is_empty(), self.values.as_slice()) {
            (true, &[root]) => Ok(root),
            _ => internal("json.to_json: no value"),
        }
    }

    /// Stops counting the values that the encoder holds, after a native
    /// call that kept the encoder on the Rust stack.
    pub(crate) fn release(self, vm: &mut Vm) {
        // `room` counted the growth past the inline slot.
        let grown = self.values.capacity() - self.values.inline_size();
        vm.heap.account(grown * VALUE_SIZE + self.budget.counted(), 0);
    }

    fn in_key(&self) -> bool {
        matches!(self.open.last(), Some(TreeOpen::Map(_, true)))
    }

    fn state(&self) -> TreeState {
        TreeState { values: self.values.len(), open: self.open.len(), top: self.open.last().copied(), depth: self.depth }
    }

    fn rewind(&mut self, s: TreeState) {
        self.values.truncate(s.values);
        self.open.truncate(s.open);
        if let (Some(top), Some(last)) = (s.top, self.open.last_mut()) {
            *last = top;
        }
        self.depth = s.depth;
    }

    /// Runs `op`, which makes more than one change, and goes back to the
    /// state before it if it fails, so that the native function can run
    /// again.
    fn atomic(&mut self, vm: &mut Vm, op: impl FnOnce(&mut Self, &mut Vm) -> Result<(), Fail>) -> Result<(), Fail> {
        let before = self.state();
        let r = op(self, vm);
        if r.is_err() {
            self.rewind(before);
        }
        r
    }

    /// Notes that a value or a key of the innermost container ended.
    #[inline(always)]
    fn item_done(&mut self) {
        if let Some(TreeOpen::Map(_, is_key)) = self.open.last_mut() {
            *is_key = !*is_key;
        }
    }

    /// Counts room for one more value against the memory limit, before any
    /// change.
    #[inline(always)]
    fn room(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.values.len() < self.values.capacity() {
            return Ok(());
        }
        let (before, after) = (self.values.capacity(), self.values.capacity().max(4) * 2);
        vm.check_memory((after - before) * VALUE_SIZE)?;
        vm.heap.account(before * VALUE_SIZE, after * VALUE_SIZE);
        self.values.reserve_exact(after - self.values.len());
        Ok(())
    }

    /// Adds the finished part `v`, after [`TreeEncoder::room`].
    #[inline(always)]
    fn put(&mut self, v: VmValue) {
        self.values.push(v);
        self.item_done();
    }

    /// Adds the scalar `v`, which cannot be a map key.
    #[inline(always)]
    fn value(&mut self, vm: &mut Vm, v: VmValue) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.room(vm)?;
        self.put(v);
        Ok(())
    }

    #[inline(always)]
    fn ids(&mut self, vm: &Vm) -> Result<JsonIds, Fail> {
        match self.ids {
            Some(ids) => Ok(ids),
            None => Ok(*self.ids.insert(JsonIds::new(&vm.prog)?)),
        }
    }

    /// Adds the `str` `s`, as a `Json.Str` or as a map key.
    fn str_part(&mut self, vm: &mut Vm, s: VmValue) -> Result<(), Fail> {
        let str_ = self.ids(vm)?.str_;
        let v = if self.in_key() { s } else { vm.heap.new_variant(str_, &[s]) };
        self.room(vm)?;
        self.put(v);
        Ok(())
    }

    /// Adds a new `str` with the text `text`, as a `Json.Str` or as a map key.
    fn text(&mut self, vm: &mut Vm, text: &str) -> Result<(), Fail> {
        vm.charge_bytes(text.len())?;
        vm.check_memory(size_of::<Obj>() + text.len())?;
        let s = vm.heap.new_str(text);
        self.str_part(vm, s)
    }

    /// Returns the key `str` of field `index` of `owner`, or of the name of
    /// variant `owner` if `index` is `u32::MAX`.
    fn name(&mut self, vm: &mut Vm, owner: u32, variant: bool, index: u32) -> Result<VmValue, Fail> {
        if let Some(&s) = self.names.get(&(owner, variant, index)) {
            return Ok(s);
        }
        let text = match index {
            u32::MAX => vm.prog.variants.get(owner as usize).map(|v| v.name.clone()),
            _ => field_name(&vm.prog, owner, variant, index as usize).map(str::to_string),
        };
        let Some(text) = text else { return internal("json.to_json: unknown field or variant") };
        vm.check_memory(size_of::<Obj>() + text.len())?;
        vm.charge_bytes(text.len())?;
        let hash = str_hash(&text);
        let s = vm.heap.new_str(text);
        self.names.insert((owner, variant, index), s);
        if let Some(h) = s.handle() {
            self.name_hashes.insert(h, hash);
        }
        Ok(s)
    }

    /// Opens `open`, after the checks.
    fn begin(&mut self, vm: &mut Vm, open: TreeOpen) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        if self.depth >= MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to convert to JSON");
        }
        self.budget.grow(vm, (self.open.len() + 1) * size_of::<TreeOpen>())?;
        self.open.push(open);
        self.depth += 1;
        Ok(())
    }

    /// Makes the `Json` array or object of the parts that `open` holds.
    fn finish(&mut self, vm: &mut Vm, open: TreeOpen) -> Result<VmValue, Fail> {
        let ids = self.ids(vm)?;
        match open {
            TreeOpen::Arr(base) => {
                let items = &self.values[base..];
                vm.check_memory(size_of::<Obj>() + items.len() * VALUE_SIZE)?;
                let list = vm.heap.new_list(items.to_vec());
                Ok(vm.heap.new_variant(ids.arr, &[list]))
            }
            TreeOpen::Fields(base, _) | TreeOpen::Map(base, _) | TreeOpen::VariantWrapper(base) => {
                let map = self.object(vm, base, !matches!(open, TreeOpen::Map(..)))?;
                Ok(vm.heap.new_variant(ids.obj, &[map]))
            }
            TreeOpen::Some => internal("json.to_json: no open container"),
        }
    }

    /// Makes the map of the key and value pairs from `base`. A later pair
    /// replaces an earlier one with the same key, unless the keys are
    /// `distinct`.
    fn object(&self, vm: &mut Vm, base: usize, distinct: bool) -> Result<VmValue, Fail> {
        let members = &self.values[base..];
        let n = members.len() / 2;
        vm.check_memory(size_of::<Obj>() + n * (MAP_ENTRY_SIZE + 2 * VALUE_SIZE))?;
        let keys = members.iter().step_by(2).copied();
        let mut map = MapObj { entries: Vec::with_capacity(n), index: KeyIndex::for_keys(n, keys), ..MapObj::default() };
        for &[key, value] in members.as_chunks::<2>().0 {
            let hash = match key.handle().and_then(|h| self.name_hashes.get(&h)) {
                Some(&hash) => hash,
                None => {
                    let text = vm.heap.str(key);
                    vm.charge_bytes(text.len())?;
                    str_hash(text)
                }
            };
            if !distinct {
                let (heap, entries) = (&vm.heap, &map.entries);
                let same = |slot: usize| entries[slot].is_some_and(|(k, _)| heap.str(k) == heap.str(key));
                if let Some(slot) = map.index.find(key, hash, same) {
                    map.entries[slot] = Some((key, value));
                    continue;
                }
            }
            let slot = map.entries.len();
            if !slot_fits(slot) {
                return fail("limit", "map too large");
            }
            map.index.insert(key, hash, slot);
            map.entries.push(Some((key, value)));
        }
        map.live = map.entries.len();
        Ok(vm.heap.alloc(Obj::Map(Box::new(map))))
    }
}

impl EncodeFormat for TreeEncoder {
    fn unit(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        let null = self.ids(vm)?.null;
        self.value(vm, VmValue::Tag(null))
    }

    fn bool(&mut self, vm: &mut Vm, b: bool) -> Result<(), Fail> {
        if self.in_key() {
            return self.text(vm, if b { "true" } else { "false" });
        }
        let v = vm.heap.new_variant(self.ids(vm)?.bool_, &[VmValue::Bool(b)]);
        self.value(vm, v)
    }

    fn int(&mut self, vm: &mut Vm, i: i64) -> Result<(), Fail> {
        if self.in_key() {
            return self.text(vm, itoa::Buffer::new().format(i));
        }
        let v = vm.heap.new_variant(self.ids(vm)?.int, &[VmValue::Int(i)]);
        self.value(vm, v)
    }

    fn float(&mut self, vm: &mut Vm, f: f64) -> Result<(), Fail> {
        if !f.is_finite() {
            return self.unit(vm);
        }
        let v = vm.heap.new_variant(self.ids(vm)?.float, &[VmValue::Float(f)]);
        self.value(vm, v)
    }

    fn duration(&mut self, vm: &mut Vm, nanos: i64) -> Result<(), Fail> {
        self.int(vm, nanos)
    }

    fn str(&mut self, vm: &mut Vm, s: &str) -> Result<(), Fail> {
        self.text(vm, s)
    }

    fn keeps_values(&self) -> bool {
        true
    }

    fn str_value(&mut self, vm: &mut Vm, s: VmValue) -> Result<(), Fail> {
        vm.charge_bytes(vm.heap.str(s).len())?;
        self.str_part(vm, s)
    }

    fn bytes(&mut self, vm: &mut Vm, b: &[u8]) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        vm.charge_slow_bytes(b.len())?;
        self.text(vm, &base64::encode(b, false))
    }

    fn native(&mut self, vm: &mut Vm, _: &str, text: &str) -> Result<(), Fail> {
        self.text(vm, text)
    }

    fn range(&mut self, vm: &mut Vm, start: i64, end: i64, inclusive: bool) -> Result<(), Fail> {
        self.atomic(vm, |f, vm| {
            f.begin(vm, TreeOpen::Fields(f.values.len(), None))?;
            let members = [("start", start, false), ("end", end, false)];
            for (name, bound, is_bool) in members.into_iter().chain(inclusive.then_some(("inclusive", 1, true))) {
                vm.charge_bytes(name.len())?;
                vm.check_memory(size_of::<Obj>() + name.len())?;
                let key = vm.heap.new_str(name);
                f.room(vm)?;
                f.values.push(key);
                let ids = f.ids(vm)?;
                let v = match is_bool {
                    true => vm.heap.new_variant(ids.bool_, &[VmValue::Bool(true)]),
                    false => vm.heap.new_variant(ids.int, &[VmValue::Int(bound)]),
                };
                f.room(vm)?;
                f.values.push(v);
            }
            f.end(vm)
        })
    }

    fn begin_seq(&mut self, vm: &mut Vm, _: SeqKind, _: usize) -> Result<(), Fail> {
        self.begin(vm, TreeOpen::Arr(self.values.len()))
    }

    fn begin_map(&mut self, vm: &mut Vm, _: usize) -> Result<(), Fail> {
        self.begin(vm, TreeOpen::Map(self.values.len(), true))
    }

    fn begin_struct(&mut self, vm: &mut Vm, def: u32) -> Result<(), Fail> {
        self.begin(vm, TreeOpen::Fields(self.values.len(), Some((def, false))))
    }

    fn field(&mut self, vm: &mut Vm, index: usize) -> Result<(), Fail> {
        let Some(&TreeOpen::Fields(_, Some((owner, variant)))) = self.open.last() else { return internal("json.to_json: no open struct") };
        let key = self.name(vm, owner, variant, index as u32)?;
        self.room(vm)?;
        self.values.push(key);
        Ok(())
    }

    fn unit_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        if variant == vm.prog.ids.none {
            return self.none(vm);
        }
        let s = self.name(vm, variant, true, u32::MAX)?;
        self.str_part(vm, s)
    }

    fn begin_variant(&mut self, vm: &mut Vm, variant: u32) -> Result<(), Fail> {
        let Some(named) = vm.prog.variants.get(variant as usize).map(|v| v.field_names.is_some()) else {
            return internal("json.to_json: unknown variant");
        };
        if self.depth + 2 > MAX_NESTING_DEPTH {
            return fail("limit", "value too deeply nested to convert to JSON");
        }
        let key = self.name(vm, variant, true, u32::MAX)?;
        self.atomic(vm, |f, vm| {
            f.room(vm)?;
            f.begin(vm, TreeOpen::VariantWrapper(f.values.len()))?;
            f.values.push(key);
            match named {
                true => f.begin(vm, TreeOpen::Fields(f.values.len(), Some((variant, true)))),
                false => f.begin(vm, TreeOpen::Arr(f.values.len())),
            }
        })
    }

    fn begin_some(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        if self.in_key() {
            return invalid_key_error();
        }
        self.budget.grow(vm, (self.open.len() + 1) * size_of::<TreeOpen>())?;
        self.open.push(TreeOpen::Some);
        Ok(())
    }

    fn none(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        self.unit(vm)
    }

    fn end(&mut self, vm: &mut Vm) -> Result<(), Fail> {
        let Some(&top) = self.open.last() else { return internal("json.to_json: no open container") };
        if let TreeOpen::Some = top {
            self.open.pop();
            self.item_done();
            return Ok(());
        }
        // Builds the value, and the outer object of a variant, before any
        // change.
        self.room(vm)?;
        let mut v = self.finish(vm, top)?;
        let mut closed = 1;
        let mut base = match top {
            TreeOpen::Arr(base) | TreeOpen::Fields(base, _) | TreeOpen::Map(base, _) | TreeOpen::VariantWrapper(base) => base,
            TreeOpen::Some => 0,
        };
        if let Some(&TreeOpen::VariantWrapper(outer)) = self.open.len().checked_sub(2).and_then(|i| self.open.get(i)) {
            // The wrapper's key is in the values; its payload is `v`.
            self.values.truncate(base);
            self.values.push(v);
            let map = self.object(vm, outer, true);
            self.values.pop();
            v = vm.heap.new_variant(self.ids(vm)?.obj, &[map?]);
            (closed, base) = (2, outer);
        }
        for _ in 0..closed {
            self.open.pop();
            self.depth -= 1;
        }
        self.values.truncate(base);
        self.put(v);
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm, json: VmValue) -> Result<(), Fail> {
        emit_json(vm, json, self)
    }

    fn value(&mut self, vm: &mut Vm, value: &Value) -> Result<(), Fail> {
        write_json_form(self, vm, value)
    }

    fn save(&mut self) {
        self.saved = Some(self.state());
    }

    fn restore(&mut self) {
        if let Some(s) = self.saved.take() {
            self.rewind(s);
        }
    }

    fn swap_values(&mut self, values: &mut ObjectValues) {
        std::mem::swap(&mut self.values, values);
        self.names.clear();
        self.name_hashes.clear();
    }

    fn heap_size(&self) -> usize {
        self.budget.counted()
    }
}
