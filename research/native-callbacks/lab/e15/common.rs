// E15: can lifetimes stop a native from holding a raw heap handle across a
// call that may collect garbage? `Val<'vm>` borrows the VM immutably, and a
// nested call needs `&mut Vm`, so the borrow checker rejects any `Val` that
// lives across the call. `Handle` is a GC root that survives the call.
use std::marker::PhantomData;

pub struct Vm { heap: Vec<Option<String>>, roots: std::cell::RefCell<Vec<usize>> }
#[derive(Clone, Copy)]
pub struct Val<'vm> { index: usize, vm: PhantomData<&'vm Vm> }
pub struct Handle(usize);

impl Vm {
    pub fn new() -> Vm { Vm { heap: vec![Some("arg".into())], roots: Default::default() } }
    pub fn arg(&self, i: usize) -> Val<'_> { Val { index: i, vm: PhantomData } }
    pub fn get(&self, v: Val<'_>) -> &str { self.heap[v.index].as_deref().expect("freed") }
    pub fn root(&self, v: Val<'_>) -> Handle { let mut r = self.roots.borrow_mut(); r.push(v.index); Handle(r.len() - 1) }
    pub fn load(&self, h: &Handle) -> Val<'_> { Val { index: self.roots.borrow()[h.0], vm: PhantomData } }
    /// Runs a callback; a GC frees every unrooted object.
    pub fn call_nested(&mut self) {
        for i in 0..self.heap.len() {
            if !self.roots.borrow().contains(&i) { self.heap[i] = None; }
        }
    }
}
