//! [`ContainerChildren`], an iterator over the children of one container on the VM
//! heap: the items of a list, the entries of a map, or the fields of a struct.
//!
//! `json.encode`, `json.to_json`, and [`Vm::to_value`](super::Vm::to_value)
//! read a value without recursion. Each of them keeps one `ContainerChildren` for each
//! container that it has entered and not yet left.

use super::bytecode::VariantRt;
use super::value::VmValue;
use std::slice::Iter;

/// The largest nesting depth of a value that the `json` module and
/// [`Vm::to_value`](super::Vm::to_value) accept.
///
/// A script can build a value that contains itself, so each of these
/// functions fails with a limit error beyond this depth.
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// The number of open containers that `json.encode`, `json.to_json`, and
/// [`Vm::to_value`](super::Vm::to_value) store without an allocation. An
/// allocation for each call costs `json.encode` of a small struct more than
/// 10% of its time.
pub(crate) const INLINE_OPEN_CONTAINERS: usize = 4;

/// The place of a child in its container.
#[derive(Clone, Copy)]
pub(crate) enum ChildRole<'h> {
    /// An item of a list, tuple, or set, or a value of a positional payload.
    Item,
    MapKey,
    /// The value of the map key that came last.
    MapValue,
    /// The value of the field with this name in a struct or in a named payload.
    Field(&'h str),
}

/// The children of one container that the iterator has not returned.
pub(crate) enum ContainerChildren<'h> {
    Items(Iter<'h, VmValue>),
    Set(Iter<'h, Option<VmValue>>),
    /// The entries of a map from the next key on, and the value of the key
    /// that came last, until that value comes.
    Map(Iter<'h, Option<(VmValue, VmValue)>>, Option<VmValue>),
    /// The field names and the field values of a struct or of a named payload.
    Fields(Iter<'h, String>, Iter<'h, VmValue>),
    /// One child and its role.
    One(ChildRole<'h>, Option<VmValue>),
    /// The two integers of an inline tuple.
    Pair(i32, i64),
}

impl<'h> ContainerChildren<'h> {
    /// Returns an iterator without children.
    pub(crate) fn empty() -> Self {
        ContainerChildren::Items(Iter::default())
    }

    /// Returns the children of the payload `values` of `variant`.
    pub(crate) fn from_payload(variant: &'h VariantRt, values: &'h [VmValue]) -> Self {
        match &variant.field_names {
            Some(names) => ContainerChildren::Fields(names.iter(), values.iter()),
            None => ContainerChildren::Items(values.iter()),
        }
    }

    /// Returns the one child of the payload `value` of `variant`.
    pub(crate) fn from_inline_payload(variant: &'h VariantRt, value: VmValue) -> Self {
        let role = variant.field_names.as_ref().and_then(|names| names.first()).map_or(ChildRole::Item, |name| ChildRole::Field(name));
        ContainerChildren::One(role, Some(value))
    }
}

impl<'h> Iterator for ContainerChildren<'h> {
    type Item = (ChildRole<'h>, VmValue);

    #[inline(always)]
    fn next(&mut self) -> Option<(ChildRole<'h>, VmValue)> {
        let item = |value| (ChildRole::Item, value);
        match self {
            ContainerChildren::Items(items) => items.next().copied().map(item),
            ContainerChildren::Set(entries) => entries.find_map(|entry| *entry).map(item),
            ContainerChildren::Map(entries, value) => match value.take() {
                Some(value) => Some((ChildRole::MapValue, value)),
                None => {
                    let (key, key_value) = entries.find_map(|entry| *entry)?;
                    *value = Some(key_value);
                    Some((ChildRole::MapKey, key))
                }
            },
            ContainerChildren::Fields(names, values) => Some((ChildRole::Field(names.next()?), *values.next()?)),
            ContainerChildren::One(role, value) => Some((*role, value.take()?)),
            ContainerChildren::Pair(first, second) => {
                let first = VmValue::Int(i64::from(*first));
                *self = ContainerChildren::One(ChildRole::Item, Some(VmValue::Int(*second)));
                Some(item(first))
            }
        }
    }
}
