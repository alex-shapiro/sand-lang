//! The `json.Json` value format of `Decoder`, for `json.from_json`.

use super::text::json_value;
use super::{JsonIds, json_from_vm};
use crate::codec::value_input::{self, InputNode};
use crate::codec::{DecodeFail, DecodeFormat, DecodeResult, MAX_NESTING_DEPTH, SeqKind};
use crate::value::Value;
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

/// Reads a Sand `json.Json` value in place, with the rules of host `Value`
/// input. A `str` in the input becomes the decoded `str`, without a copy.
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

    /// Returns the scalar rules of `node`, and `true` if it is a map key.
    fn input<'v>(&self, vm: &'v Vm, node: Node) -> (InputNode<'v>, bool) {
        let ids = &self.ids;
        let n = match node {
            Node::Key(k) => return (InputNode::Str(vm.heap.str(k)), true),
            Node::Json(id, _) if id == ids.null => InputNode::Unit,
            Node::Json(id, Some(VmValue::Bool(b))) if id == ids.bool_ => InputNode::Bool(b),
            Node::Json(id, Some(VmValue::Int(i))) if id == ids.int => InputNode::Int(i),
            Node::Json(id, Some(VmValue::Float(f))) if id == ids.float => InputNode::Float(f),
            Node::Json(id, Some(s)) if id == ids.str_ => InputNode::Str(vm.heap.str(s)),
            Node::Json(id, _) if id == ids.arr => InputNode::Container("list"),
            _ => InputNode::Container("map"),
        };
        (n, false)
    }

    /// Reads the current node with `read`.
    fn read<T>(&mut self, vm: &mut Vm, read: impl FnOnce(InputNode<'_>, bool) -> value_input::InputResult<T>) -> DecodeResult<T> {
        let node = self.node(vm)?;
        let (n, in_key) = self.input(vm, node);
        let r = read(n, in_key)?;
        self.ready = false;
        Ok(r)
    }

    /// Returns the payload of the current node if it is the `Json` variant
    /// `id`.
    fn payload(&self, vm: &Vm, id: u32) -> DecodeResult<Option<VmValue>> {
        Ok(match self.node(vm)? {
            Node::Json(i, p) if i == id => p,
            _ => None,
        })
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

impl DecodeFormat for TreeDecoder {
    fn unit(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        self.read(vm, |n, in_key| n.unit(in_key))
    }

    fn bool(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        self.read(vm, |n, in_key| n.bool(in_key))
    }

    fn int(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        self.read(vm, |n, in_key| n.int(in_key))
    }

    fn float(&mut self, vm: &mut Vm) -> DecodeResult<f64> {
        self.read(vm, |n, in_key| n.float(in_key))
    }

    fn duration(&mut self, vm: &mut Vm) -> DecodeResult<i64> {
        self.read(vm, |n, in_key| n.duration(in_key))
    }

    fn str(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let s = match self.node(vm)? {
            Node::Key(k) => k,
            Node::Json(id, Some(s)) if id == self.ids.str_ => s,
            node => {
                let (n, in_key) = self.input(vm, node);
                return Err(n.mismatch("str", in_key).into());
            }
        };
        self.ready = false;
        Ok(s)
    }

    fn bytes(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let node = self.node(vm)?;
        let b = {
            let (n, in_key) = self.input(vm, node);
            n.bytes(in_key)?.into_owned()
        };
        vm.check_memory(b.len())?;
        vm.charge_bytes(b.len())?;
        self.ready = false;
        Ok(vm.heap.new_bytes(b))
    }

    fn native_text(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<String> {
        self.read(vm, |n, in_key| n.native_text(name, in_key).map(str::to_owned))
    }

    fn range(&mut self, vm: &mut Vm) -> DecodeResult<(i64, i64, bool)> {
        let Some(map) = self.payload(vm, self.ids.obj)? else {
            return self.read(vm, |n, in_key| Err(n.mismatch("Range", in_key)));
        };
        let Obj::Map(m) = vm.heap.get(map) else { return Err(Fail::internal("json: not an object").into()) };
        let entries = m.entries.iter().flatten().map(|&(k, v)| {
            let node = Node::Json(vm.heap.variant_id(v), vm.heap.field(v, 0));
            (InputNode::Str(vm.heap.str(k)), self.input(vm, node).0)
        });
        let r = value_input::range_of(entries)?;
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
        let Some(list) = self.payload(vm, self.ids.arr)? else {
            let what = match kind {
                SeqKind::List => "list".into(),
                SeqKind::Set => "set".into(),
                SeqKind::Tuple(n) => format!("tuple of {n} elements"),
            };
            return self.read(vm, |n, in_key| Err(n.mismatch(&what, in_key)));
        };
        if let SeqKind::Tuple(n) = kind
            && vm.heap.list(list).len() != n
        {
            return Err(DecodeFail::Mismatch(self.length_mismatch(n)));
        }
        self.enter(vm, list, false)
    }

    fn next_item(&mut self, vm: &mut Vm) -> DecodeResult<bool> {
        self.next_element(vm)
    }

    fn begin_map(&mut self, vm: &mut Vm) -> DecodeResult<()> {
        match self.payload(vm, self.ids.obj)? {
            Some(map) => self.enter(vm, map, true),
            None => self.read(vm, |n, in_key| Err(n.mismatch("map", in_key))),
        }
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
        match self.payload(vm, self.ids.obj)? {
            Some(map) => self.enter(vm, map, true),
            None => self.read(vm, |n, in_key| Err(n.mismatch(if name.is_empty() { "map" } else { name }, in_key))),
        }
    }

    fn next_field(&mut self, vm: &mut Vm) -> DecodeResult<Option<Cow<'_, str>>> {
        self.next_member(vm, EntryPart::Value)
    }

    fn skip(&mut self, _: &mut Vm) -> DecodeResult<()> {
        self.ready = false;
        Ok(())
    }

    fn variant(&mut self, vm: &mut Vm, name: &str) -> DecodeResult<(Cow<'_, str>, bool)> {
        let node = self.node(vm)?;
        let tag = match node {
            Node::Key(k) => k,
            Node::Json(id, Some(s)) if id == self.ids.str_ => s,
            Node::Json(id, Some(map)) if id == self.ids.obj => {
                let Obj::Map(m) = vm.heap.get(map) else { return Err(Fail::internal("json: not an object").into()) };
                if m.live != 1 {
                    let what = if m.live == 0 { "expected a variant" } else { "expected a single variant" };
                    return Err(DecodeFail::Mismatch(format!("{what} of {name}")));
                }
                self.enter(vm, map, true)?;
                let Some(tag) = self.next_member(vm, EntryPart::Value)? else { return Err(Fail::internal("json: no variant").into()) };
                return Ok((tag, true));
            }
            node => {
                let (n, in_key) = self.input(vm, node);
                return Err(n.mismatch(name, in_key).into());
            }
        };
        self.ready = false;
        self.key.clear();
        self.key.push_str(vm.heap.str(tag));
        Ok((Cow::Borrowed(&self.key), false))
    }

    fn end_variant(&mut self, _: &mut Vm, _: &str) -> DecodeResult<()> {
        self.leave();
        Ok(())
    }

    fn json(&mut self, vm: &mut Vm) -> DecodeResult<VmValue> {
        let v = match self.node(vm)? {
            Node::Key(k) => {
                vm.check_memory(size_of::<Obj>())?;
                vm.heap.new_variant(self.ids.str_, &[k])
            }
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
        let v = match self.node(vm)? {
            Node::Key(k) => Value::Str(vm.heap.str(k).to_string()),
            Node::Json(..) => {
                let v = if self.open.is_empty() { Some(self.values[0]) } else { self.node_value(vm) };
                json_value(&json_from_vm(vm, v.ok_or_else(|| Fail::internal("json: no value"))?)?)
            }
        };
        self.ready = false;
        Ok(v)
    }

    fn length_mismatch(&self, len: usize) -> String {
        value_input::length_mismatch(len)
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
