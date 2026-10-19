//! Interrupt buffers: an `Int32Array` over a `SharedArrayBuffer` that
//! another thread (the page) sets to stop an engine busy on this one.
//!
//! The engine wants a `Send` signal, and JavaScript values are not `Send`,
//! so the arrays live in a thread-local table and the signal holds their key.

use js_sys::{Atomics, Int32Array, SharedArrayBuffer};
use sandlang::InterruptSignal;
use std::cell::RefCell;
use std::collections::HashMap;
use wasm_bindgen::JsCast;

thread_local! {
    static BUFFERS: RefCell<HashMap<u32, Int32Array>> = RefCell::new(HashMap::new());
    static NEXT_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

/// Whether `array` is an `Int32Array` over a `SharedArrayBuffer`.
pub(crate) fn is_shared(array: &Int32Array) -> bool {
    array.buffer().is_instance_of::<SharedArrayBuffer>() && array.length() > 0
}

/// Element 0 of an interrupt buffer: nonzero asks the engine to stop.
/// Dropping it unregisters the buffer.
pub(crate) struct JsSignal {
    id: u32,
}

impl JsSignal {
    pub(crate) fn new(array: Int32Array) -> JsSignal {
        let id = NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id.wrapping_add(1).max(1));
            id
        });
        BUFFERS.with(|b| b.borrow_mut().insert(id, array));
        JsSignal { id }
    }
}

impl Drop for JsSignal {
    fn drop(&mut self) {
        BUFFERS.with(|b| b.borrow_mut().remove(&self.id));
    }
}

impl InterruptSignal for JsSignal {
    fn is_set(&self) -> bool {
        BUFFERS.with(|b| b.borrow().get(&self.id).is_some_and(|a| Atomics::load(a, 0).is_ok_and(|v| v != 0)))
    }

    fn clear(&self) {
        BUFFERS.with(|b| {
            if let Some(a) = b.borrow().get(&self.id) {
                let _ = Atomics::store(a, 0, 0);
            }
        });
    }
}
