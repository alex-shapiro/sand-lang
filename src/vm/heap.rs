//! The garbage-collected heap: an arena of objects addressed by `u32` handles,
//! collected by a precise mark-and-sweep collector.

use super::value::*;

pub struct Heap {
    objs: Vec<Obj>,
    marks: Vec<u64>,
    free_head: u32,
    /// Estimated live bytes (exact after a collection).
    pub bytes: usize,
    /// Collect when `bytes` exceeds this.
    pub next_gc: usize,
    pub limit: Option<usize>,
    pub live_objects: usize,
    /// Native resources currently open (sockets, listeners, ...).
    pub open_resources: usize,
    worklist: Vec<u32>,
}

const NONE: u32 = u32::MAX;
const MIN_GC: usize = 4 << 20;

impl Default for Heap {
    fn default() -> Self {
        Heap::new(None)
    }
}

impl Heap {
    pub fn new(limit: Option<usize>) -> Heap {
        Heap {
            objs: Vec::new(),
            marks: Vec::new(),
            free_head: NONE,
            bytes: 0,
            next_gc: MIN_GC,
            limit,
            live_objects: 0,
            open_resources: 0,
            worklist: Vec::new(),
        }
    }

    #[inline]
    pub fn alloc(&mut self, obj: Obj) -> Value {
        self.bytes += obj.size();
        self.live_objects += 1;
        if let Obj::Native(n) = &obj
            && n.is_resource()
        {
            self.open_resources += 1;
        }
        if self.free_head != NONE {
            let h = self.free_head;
            match self.objs[h as usize] {
                Obj::Free(next) => self.free_head = next,
                _ => unreachable!("corrupt free list"),
            }
            self.objs[h as usize] = obj;
            Value::Obj(h)
        } else {
            self.objs.push(obj);
            let h = (self.objs.len() - 1) as u32;
            if self.marks.len() * 64 < self.objs.len() {
                self.marks.push(0);
            }
            Value::Obj(h)
        }
    }

    /// True if a collection should run at the next safe point.
    #[inline]
    pub fn wants_gc(&self) -> bool {
        self.bytes > self.next_gc
    }

    /// Whether `extra` more bytes would exceed the memory limit.
    pub fn would_exceed(&self, extra: usize) -> bool {
        match self.limit {
            Some(l) => self.bytes.saturating_add(extra) > l,
            None => extra > (1usize << 40),
        }
    }

    pub fn over_limit(&self) -> bool {
        matches!(self.limit, Some(l) if self.bytes > l)
    }

    #[inline]
    pub fn get(&self, v: Value) -> &Obj {
        match v {
            Value::Obj(h) => &self.objs[h as usize],
            _ => &Obj::Free(0),
        }
    }

    #[inline]
    pub fn get_mut(&mut self, v: Value) -> &mut Obj {
        match v {
            Value::Obj(h) => &mut self.objs[h as usize],
            _ => panic!("internal error: not an object"),
        }
    }

    /// Adjusts accounting after an object grew or shrank.
    #[inline]
    pub fn account(&mut self, before: usize, after: usize) {
        if after >= before {
            self.bytes += after - before;
        } else {
            self.bytes = self.bytes.saturating_sub(before - after);
        }
    }

    // ------------------------------------------------------------------
    // Typed accessors

    pub fn str(&self, v: Value) -> &str {
        match self.get(v) {
            Obj::Str(s) => s,
            _ => "",
        }
    }

    pub fn bytes_of(&self, v: Value) -> &[u8] {
        match self.get(v) {
            Obj::Bytes(b) => b,
            _ => &[],
        }
    }

    pub fn list(&self, v: Value) -> &[Value] {
        match self.get(v) {
            Obj::List(l) => l,
            _ => &[],
        }
    }

    pub fn list_mut(&mut self, v: Value) -> &mut Vec<Value> {
        match self.get_mut(v) {
            Obj::List(l) => l,
            other => panic!("internal error: expected list, found {other:?}"),
        }
    }

    pub fn fields(&self, v: Value) -> &[Value] {
        match self.get(v) {
            Obj::Tuple(t) | Obj::Struct(_, t) | Obj::Variant(_, t) | Obj::Closure(_, t) => t,
            _ => &[],
        }
    }

    pub fn new_str(&mut self, s: impl Into<Box<str>>) -> Value {
        self.alloc(Obj::Str(s.into()))
    }

    pub fn new_bytes(&mut self, b: impl Into<Box<[u8]>>) -> Value {
        self.alloc(Obj::Bytes(b.into()))
    }

    pub fn new_list(&mut self, items: Vec<Value>) -> Value {
        self.alloc(Obj::List(items))
    }

    pub fn new_tuple(&mut self, items: Vec<Value>) -> Value {
        self.alloc(Obj::Tuple(items.into_boxed_slice()))
    }

    pub fn list_push(&mut self, list: Value, v: Value) {
        let Obj::List(l) = self.get_mut(list) else { return };
        let before = l.capacity();
        l.push(v);
        let after = l.capacity();
        if after != before {
            self.bytes += (after - before) * std::mem::size_of::<Value>();
        }
    }

    pub fn variant_id(&self, v: Value) -> u32 {
        match v {
            Value::Tag(t) => t,
            Value::Obj(_) => match self.get(v) {
                Obj::Variant(id, _) => *id,
                _ => u32::MAX,
            },
            _ => u32::MAX,
        }
    }

    pub fn native<T: 'static>(&self, v: Value) -> Option<&T> {
        match self.get(v) {
            Obj::Native(n) => n.as_any().downcast_ref::<T>(),
            _ => None,
        }
    }

    pub fn native_mut<T: 'static>(&mut self, v: Value) -> Option<&mut T> {
        match self.get_mut(v) {
            Obj::Native(n) => n.as_any_mut().downcast_mut::<T>(),
            _ => None,
        }
    }

    /// Releases a native resource early (e.g. `conn.close()`), keeping the object.
    pub fn resource_closed(&mut self) {
        self.open_resources = self.open_resources.saturating_sub(1);
    }

    pub fn len(&self) -> usize {
        self.live_objects
    }

    pub fn is_empty(&self) -> bool {
        self.live_objects == 0
    }

    // ------------------------------------------------------------------
    // Collection

    #[inline]
    fn mark_value(&mut self, v: Value) {
        if let Value::Obj(h) = v {
            let (w, b) = ((h / 64) as usize, h % 64);
            if self.marks[w] & (1 << b) == 0 {
                self.marks[w] |= 1 << b;
                self.worklist.push(h);
            }
        }
    }

    /// Marks from `roots` and frees everything unreachable.
    /// `closed` is called for each collected native resource.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = Value>) {
        self.marks.iter_mut().for_each(|m| *m = 0);
        self.marks.resize(self.objs.len().div_ceil(64), 0);
        for r in roots {
            self.mark_value(r);
        }
        let mut children = Vec::new();
        while let Some(h) = self.worklist.pop() {
            children.clear();
            self.objs[h as usize].children(|v| children.push(v));
            for c in &children {
                self.mark_value(*c);
            }
        }
        let mut live_bytes = 0;
        let mut live = 0;
        let mut open = 0;
        for i in 0..self.objs.len() {
            let marked = self.marks[i / 64] & (1 << (i % 64)) != 0;
            if matches!(self.objs[i], Obj::Free(_)) {
                continue;
            }
            if marked {
                live_bytes += self.objs[i].size();
                live += 1;
                if let Obj::Native(n) = &self.objs[i]
                    && n.is_resource()
                {
                    open += 1;
                }
            } else {
                self.objs[i] = Obj::Free(self.free_head);
                self.free_head = i as u32;
            }
        }
        // Resources closed explicitly are still objects; keep the explicit count if lower.
        self.open_resources = self.open_resources.min(open);
        self.bytes = live_bytes;
        self.live_objects = live;
        self.next_gc = (live_bytes * 2).max(MIN_GC);
        if let Some(l) = self.limit {
            // Collect more eagerly as we approach the limit.
            if self.next_gc > l {
                self.next_gc = l.max(live_bytes + (l - live_bytes.min(l)) / 2);
            }
        }
    }
}
