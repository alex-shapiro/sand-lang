//! Persistent collections for the checker's tables.
//!
//! The standard library is checked once, and every compilation (and every
//! input of a REPL session) extends a copy of the checker's state. These
//! collections make that copy cheap: cloning one costs a reference count
//! increment, and a change copies only the parts it touches (a chunk of a
//! [`PVec`], a path of a [`PHash`]) if they are shared with another copy.
//!
//! - [`PVec`]: a vector in chunks of [`CHUNK`] elements behind `Arc`s.
//! - [`PHash`]: a hash array mapped trie of values with their hashes; the
//!   caller hashes and compares.
//! - [`PMap`]: a map built on it, hashing keys with the process's seeded
//!   hasher (`FxBuild`), so that programs cannot choose colliding names.

use crate::vm::value::FxBuild;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;

const SHIFT: u32 = 6;
/// Elements per chunk of a [`PVec`].
pub const CHUNK: usize = 1 << SHIFT;
const MASK: usize = CHUNK - 1;

/// A persistent vector: `clone` is O(1); `push` and `get_mut` copy the
/// chunk they change (and the chunk list) only if another copy shares it.
pub struct PVec<T> {
    chunks: Arc<Vec<Arc<Vec<T>>>>,
    len: usize,
}

impl<T> Clone for PVec<T> {
    fn clone(&self) -> Self {
        PVec { chunks: self.chunks.clone(), len: self.len }
    }
}

impl<T> Default for PVec<T> {
    fn default() -> Self {
        PVec { chunks: Arc::new(Vec::new()), len: 0 }
    }
}

impl<T: fmt::Debug> fmt::Debug for PVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> PVec<T> {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&T> {
        if i < self.len { Some(&self.chunks[i >> SHIFT][i & MASK]) } else { None }
    }

    pub fn last(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter { chunks: self.chunks.iter(), cur: [].iter() }
    }

    /// The elements from index `start` on.
    pub fn iter_from(&self, start: usize) -> impl Iterator<Item = &T> + '_ {
        let first = start >> SHIFT;
        self.chunks.iter().skip(first).enumerate().flat_map(move |(k, c)| c.iter().skip(if k == 0 { start & MASK } else { 0 }))
    }
}

impl<T: Clone> PVec<T> {
    pub fn push(&mut self, x: T) {
        let chunks = Arc::make_mut(&mut self.chunks);
        if self.len & MASK == 0 {
            chunks.push(Arc::new(Vec::with_capacity(CHUNK)));
        }
        let last = chunks.last_mut().expect("a chunk");
        if Arc::get_mut(last).is_none() {
            // Shared with another copy: copy it, with room for a full chunk.
            let mut own = Vec::with_capacity(CHUNK);
            own.extend_from_slice(last);
            *last = Arc::new(own);
        }
        Arc::get_mut(last).expect("unique").push(x);
        self.len += 1;
    }

    /// The element at `i`, copied first if shared. Panics if out of bounds.
    pub fn get_mut(&mut self, i: usize) -> &mut T {
        assert!(i < self.len, "index {i} out of bounds ({})", self.len);
        let chunk = &mut Arc::make_mut(&mut self.chunks)[i >> SHIFT];
        &mut Arc::make_mut(chunk)[i & MASK]
    }

    pub fn last_mut(&mut self) -> Option<&mut T> {
        let i = self.len.checked_sub(1)?;
        Some(self.get_mut(i))
    }

    pub fn truncate(&mut self, n: usize) {
        if n >= self.len {
            return;
        }
        let chunks = Arc::make_mut(&mut self.chunks);
        chunks.truncate(n.div_ceil(CHUNK));
        if n & MASK != 0 {
            Arc::make_mut(chunks.last_mut().expect("a chunk")).truncate(n & MASK);
        }
        self.len = n;
    }
}

pub struct Iter<'a, T> {
    chunks: std::slice::Iter<'a, Arc<Vec<T>>>,
    cur: std::slice::Iter<'a, T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;
    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        loop {
            if let Some(x) = self.cur.next() {
                return Some(x);
            }
            self.cur = self.chunks.next()?.iter();
        }
    }
}

impl<'a, T> IntoIterator for &'a PVec<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<T> std::ops::Index<usize> for PVec<T> {
    type Output = T;
    #[inline]
    fn index(&self, i: usize) -> &T {
        &self.chunks[i >> SHIFT][i & MASK]
    }
}

impl<T: Clone> std::ops::IndexMut<usize> for PVec<T> {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut T {
        self.get_mut(i)
    }
}

impl<T: Clone> FromIterator<T> for PVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut v = PVec::new();
        for x in iter {
            v.push(x);
        }
        v
    }
}

impl<T: Clone> Extend<T> for PVec<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for x in iter {
            self.push(x);
        }
    }
}

// ----------------------------------------------------------------------
// Hash array mapped trie

/// Bits of the hash used per level.
const LEVEL: u32 = 5;

/// A persistent hash table of values stored with their 64-bit hashes; the
/// caller provides the hash and the comparison. `clone` is O(1); an insert
/// copies the nodes on its path that another copy shares.
pub struct PHash<V> {
    root: Arc<Node<V>>,
    len: usize,
}

impl<V> Clone for PHash<V> {
    fn clone(&self) -> Self {
        PHash { root: self.root.clone(), len: self.len }
    }
}

impl<V> Default for PHash<V> {
    fn default() -> Self {
        PHash { root: Arc::new(Node::Branch { bitmap: 0, slots: Vec::new() }), len: 0 }
    }
}

#[derive(Clone)]
enum Node<V> {
    /// Slot `k` of the level's 32 holds the values whose hash has `k` in
    /// the bits of this level, if bit `k` of `bitmap` is set (`slots`
    /// holds only those, in order).
    Branch { bitmap: u32, slots: Vec<Slot<V>> },
    /// Values with equal hashes.
    Bucket(Vec<(u64, V)>),
}

#[derive(Clone)]
enum Slot<V> {
    Leaf(u64, V),
    Sub(Arc<Node<V>>),
}

#[inline]
fn bit_of(h: u64, shift: u32) -> u32 {
    1 << ((h >> shift) & 31)
}

impl<V> PHash<V> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The value with hash `h` for which `eq` holds.
    #[inline]
    pub fn find(&self, h: u64, mut eq: impl FnMut(&V) -> bool) -> Option<&V> {
        let mut node = &*self.root;
        let mut shift = 0;
        loop {
            match node {
                Node::Branch { bitmap, slots } => {
                    let bit = bit_of(h, shift);
                    if bitmap & bit == 0 {
                        return None;
                    }
                    match &slots[(bitmap & (bit - 1)).count_ones() as usize] {
                        Slot::Leaf(h2, v) => return (*h2 == h && eq(v)).then_some(v),
                        Slot::Sub(n) => {
                            node = n;
                            shift += LEVEL;
                        }
                    }
                }
                Node::Bucket(items) => return items.iter().find(|(h2, v)| *h2 == h && eq(v)).map(|(_, v)| v),
            }
        }
    }

    /// Visits every value (in no particular order).
    pub fn for_each(&self, f: &mut impl FnMut(&V)) {
        fn go<V>(n: &Node<V>, f: &mut impl FnMut(&V)) {
            match n {
                Node::Branch { slots, .. } => {
                    for s in slots {
                        match s {
                            Slot::Leaf(_, v) => f(v),
                            Slot::Sub(n) => go(n, f),
                        }
                    }
                }
                Node::Bucket(items) => items.iter().for_each(|(_, v)| f(v)),
            }
        }
        go(&self.root, f)
    }
}

impl<V: Clone> PHash<V> {
    /// Inserts `v` (hash `h`), replacing and returning the value for which
    /// `eq` holds, if any.
    pub fn insert(&mut self, h: u64, v: V, mut eq: impl FnMut(&V) -> bool) -> Option<V> {
        let old = insert_in(Arc::make_mut(&mut self.root), h, v, 0, &mut eq);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// The value with hash `h` for which `eq` holds, copying its path first
    /// if shared.
    pub fn get_mut(&mut self, h: u64, mut eq: impl FnMut(&V) -> bool) -> Option<&mut V> {
        self.find(h, &mut eq)?;
        let mut node = Arc::make_mut(&mut self.root);
        let mut shift = 0;
        loop {
            match node {
                Node::Branch { bitmap, slots } => {
                    let bit = bit_of(h, shift);
                    match &mut slots[(*bitmap & (bit - 1)).count_ones() as usize] {
                        Slot::Leaf(_, v) => return Some(v),
                        Slot::Sub(n) => {
                            node = Arc::make_mut(n);
                            shift += LEVEL;
                        }
                    }
                }
                Node::Bucket(items) => return items.iter_mut().find(|(_, v)| eq(v)).map(|(_, v)| v),
            }
        }
    }
}

fn insert_in<V: Clone>(node: &mut Node<V>, h: u64, v: V, shift: u32, eq: &mut impl FnMut(&V) -> bool) -> Option<V> {
    match node {
        Node::Bucket(items) => {
            if let Some((_, x)) = items.iter_mut().find(|(_, x)| eq(x)) {
                return Some(std::mem::replace(x, v));
            }
            items.push((h, v));
            None
        }
        Node::Branch { bitmap, slots } => {
            let bit = bit_of(h, shift);
            let idx = (*bitmap & (bit - 1)).count_ones() as usize;
            if *bitmap & bit == 0 {
                *bitmap |= bit;
                slots.insert(idx, Slot::Leaf(h, v));
                return None;
            }
            match &mut slots[idx] {
                Slot::Sub(n) => insert_in(Arc::make_mut(n), h, v, shift + LEVEL, eq),
                Slot::Leaf(h2, x) if *h2 == h && eq(x) => Some(std::mem::replace(x, v)),
                Slot::Leaf(..) => {
                    let Slot::Leaf(h2, x) = slots.remove(idx) else { unreachable!() };
                    let sub = if h2 == h { Node::Bucket(vec![(h2, x), (h, v)]) } else { pair(shift + LEVEL, (h2, x), (h, v)) };
                    slots.insert(idx, Slot::Sub(Arc::new(sub)));
                    None
                }
            }
        }
    }
}

/// A node holding two values with different hashes.
fn pair<V>(shift: u32, a: (u64, V), b: (u64, V)) -> Node<V> {
    let (ba, bb) = (bit_of(a.0, shift), bit_of(b.0, shift));
    if ba == bb {
        let sub = pair(shift + LEVEL, a, b);
        return Node::Branch { bitmap: ba, slots: vec![Slot::Sub(Arc::new(sub))] };
    }
    let (first, second) = if ba < bb { (a, b) } else { (b, a) };
    Node::Branch { bitmap: ba | bb, slots: vec![Slot::Leaf(first.0, first.1), Slot::Leaf(second.0, second.1)] }
}

/// A persistent hash map (see [`PHash`]).
pub struct PMap<K, V> {
    table: PHash<(K, V)>,
    hasher: FxBuild,
}

impl<K, V> Clone for PMap<K, V> {
    fn clone(&self) -> Self {
        PMap { table: self.table.clone(), hasher: self.hasher }
    }
}

impl<K, V> Default for PMap<K, V> {
    fn default() -> Self {
        PMap { table: PHash::default(), hasher: FxBuild::default() }
    }
}

impl<K: Hash + Eq + Clone, V: Clone> PMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    #[inline]
    pub fn get<Q: Hash + Eq + ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.table.find(self.hasher.hash_one(k), |(x, _)| x.borrow() == k).map(|(_, v)| v)
    }

    pub fn contains_key<Q: Hash + Eq + ?Sized>(&self, k: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
    {
        self.get(k).is_some()
    }

    /// The value of `k`, inserting `v` first if there is none.
    pub fn get_or_insert(&mut self, k: K, v: V) -> V {
        if let Some(x) = self.get(&k) {
            return x.clone();
        }
        self.insert(k, v.clone());
        v
    }

    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.table.get_mut(self.hasher.hash_one(k), |(x, _)| x == k).map(|(_, v)| v)
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let h = self.hasher.hash_one(&k);
        let probe = k.clone();
        self.table.insert(h, (k, v), |(x, _)| *x == probe).map(|(_, v)| v)
    }

    /// The entries, in no particular order.
    pub fn entries(&self) -> Vec<(&K, &V)> {
        let mut out = Vec::with_capacity(self.len());
        // Not `for_each`: its callback's references cannot outlive the call.
        fn collect<'a, K, V>(n: &'a Node<(K, V)>, out: &mut Vec<(&'a K, &'a V)>) {
            match n {
                Node::Branch { slots, .. } => {
                    for s in slots {
                        match s {
                            Slot::Leaf(_, (k, v)) => out.push((k, v)),
                            Slot::Sub(n) => collect(n, out),
                        }
                    }
                }
                Node::Bucket(items) => out.extend(items.iter().map(|(_, (k, v))| (k, v))),
            }
        }
        collect(&self.table.root, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn pvec_shares_and_copies_on_write() {
        let mut a: PVec<u32> = (0..200).collect();
        let b = a.clone();
        a.push(200);
        a[3] = 99;
        assert_eq!(a.len(), 201);
        assert_eq!(b.len(), 200);
        assert_eq!(b[3], 3);
        assert_eq!(a[3], 99);
        assert_eq!(a[200], 200);
        assert!(b.get(200).is_none());
        assert_eq!(a.iter().count(), 201);
        assert_eq!(a.iter_from(130).copied().collect::<Vec<_>>(), (130..201).collect::<Vec<_>>());
        assert_eq!(a.iter_from(64).count(), 137);
        let mut c = a.clone();
        c.truncate(65);
        assert_eq!(c.len(), 65);
        c.push(7);
        assert_eq!(c[65], 7);
        assert_eq!(a[65], 65);
    }

    #[test]
    fn pmap_matches_hashmap() {
        let mut m: PMap<u64, u64> = PMap::new();
        let mut std: HashMap<u64, u64> = HashMap::new();
        let mut copies = Vec::new();
        let mut x = 1u64;
        for i in 0..5000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let k = x % 3000;
            assert_eq!(m.insert(k, i), std.insert(k, i));
            if i % 1000 == 0 {
                copies.push((m.clone(), std.clone()));
            }
        }
        assert_eq!(m.len(), std.len());
        for (k, v) in &std {
            assert_eq!(m.get(k), Some(v));
        }
        for (m, std) in copies {
            assert_eq!(m.len(), std.len());
            for (k, v) in &std {
                assert_eq!(m.get(k), Some(v));
            }
            assert_eq!(m.entries().len(), std.len());
        }
        *m.get_mut(&5).unwrap() = 1;
        assert_eq!(m.get(&5), Some(&1));
    }

    #[test]
    fn phash_full_collisions() {
        let mut t: PHash<u32> = PHash::default();
        for i in 0..10 {
            assert!(t.insert(42, i, |&x| x == i).is_none());
        }
        assert!(t.insert(7, 100, |&x| x == 100).is_none());
        for i in 0..10 {
            assert_eq!(t.find(42, |&x| x == i), Some(&i));
        }
        assert_eq!(t.find(7, |&x| x == 100), Some(&100));
        assert_eq!(t.len(), 11);
    }
}
