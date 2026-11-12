//! [`ContainerChildren`] iterates over the children of a heap container.
//!
//! `json.encode`, `json.to_json`, and [`Vm::to_value`](super::Vm::to_value)
//! walk heap values iteratively. Each function keeps a `ContainerChildren`
//! iterator for every open container on its stack.

use super::bytecode::VariantRt;
use super::value::VmValue;
use std::slice::Iter;

/// Maximum nesting depth accepted by the `json` module and
/// [`Vm::to_value`](super::Vm::to_value).
///
/// Exceeding this depth returns a limit error, which also stops cyclic values.
pub(crate) const MAX_NESTING_DEPTH: usize = 512;

/// Number of open containers stored inline before spilling to the heap.
pub(crate) const INLINE_OPEN_CONTAINERS: usize = 4;

/// Role of a child value within its parent container.
#[derive(Clone, Copy)]
pub(crate) enum ChildRole<'h> {
    /// Element of a list, tuple, set, or positional variant payload.
    Item,
    MapKey,
    /// Value corresponding to the preceding `MapKey`.
    MapValue,
    /// Value of a named field in a struct or variant payload.
    Field(&'h str),
}

/// Remaining children of an open heap container.
pub(crate) enum ContainerChildren<'h> {
    Items(Iter<'h, VmValue>),
    Set(Iter<'h, Option<VmValue>>),
    /// Remaining map entries and the pending value for the current key.
    Map(Iter<'h, Option<(VmValue, VmValue)>>, Option<VmValue>),
    /// Field names and values of a struct or named variant payload.
    Fields(Iter<'h, String>, Iter<'h, VmValue>),
    /// A single child value and its role.
    Single(ChildRole<'h>, Option<VmValue>),
    /// The two integers of an inline pair tuple.
    Pair(i32, i64),
}

impl<'h> ContainerChildren<'h> {
    /// Returns an empty child iterator.
    pub(crate) fn empty() -> Self {
        ContainerChildren::Items(Iter::default())
    }

    /// Returns an iterator over the payload `values` of `variant`.
    pub(crate) fn from_payload(variant: &'h VariantRt, values: &'h [VmValue]) -> Self {
        match &variant.field_names {
            Some(names) => ContainerChildren::Fields(names.iter(), values.iter()),
            None => ContainerChildren::Items(values.iter()),
        }
    }

    /// Returns an iterator over the single inline payload `value` of `variant`.
    pub(crate) fn from_inline_payload(variant: &'h VariantRt, value: VmValue) -> Self {
        let role = variant.field_names.as_ref().and_then(|names| names.first()).map_or(ChildRole::Item, |name| ChildRole::Field(name));
        ContainerChildren::Single(role, Some(value))
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
            ContainerChildren::Single(role, value) => Some((*role, value.take()?)),
            ContainerChildren::Pair(first, second) => {
                let first = VmValue::Int(i64::from(*first));
                *self = ContainerChildren::Single(ChildRole::Item, Some(VmValue::Int(*second)));
                Some(item(first))
            }
        }
    }
}
