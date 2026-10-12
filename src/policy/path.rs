//! Path patterns (of files and URLs) and the trie that matches paths
//! against many of them at once.
//!
//! A pattern is a sequence of segments, each a name, a name with `*`
//! wildcards, `*` (any one name) or `**` (any number of names). A pattern
//! covers the paths it matches and everything below them, so a trailing
//! `**` adds nothing and is dropped.

use crate::vm::value::FxBuild;
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path};

/// A character of a pattern's text and whether it is a wildcard (an
/// unquoted `*`).
pub(crate) type Tok = (char, bool);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Seg {
    Lit(Box<str>),
    Glob(Glob),
    Star,
    GlobStar,
}

impl Seg {
    /// Parses one segment's text.
    pub(crate) fn parse(text: &[Tok]) -> Result<Seg, String> {
        let stars = text.iter().filter(|t| t.1).count();
        if stars == 0 {
            return Ok(Seg::Lit(text.iter().map(|t| t.0).collect()));
        }
        match text {
            [(_, true)] => return Ok(Seg::Star),
            [(_, true), (_, true)] => return Ok(Seg::GlobStar),
            _ => {}
        }
        if text.windows(2).any(|w| w[0].1 && w[1].1) {
            return Err("`**` must be a whole path segment".into());
        }
        let mut pieces = vec![String::new()];
        for &(c, wild) in text {
            if wild {
                pieces.push(String::new());
            } else if let Some(last) = pieces.last_mut() {
                last.push(c);
            }
        }
        Ok(Seg::Glob(Glob(pieces.into_iter().map(String::into_boxed_str).collect())))
    }

    /// How much the segment narrows what a pattern matches: names count 2,
    /// names with wildcards 1.
    pub(crate) fn weight(&self) -> u32 {
        match self {
            Seg::Lit(_) => 2,
            Seg::Glob(_) => 1,
            Seg::Star | Seg::GlobStar => 0,
        }
    }

    /// The segment with its names (and the pieces of its wildcard names)
    /// mapped by `f`.
    pub(crate) fn map(&self, f: impl Fn(&str) -> String) -> Seg {
        match self {
            Seg::Lit(s) => Seg::Lit(f(s).into()),
            Seg::Glob(g) => Seg::Glob(Glob(g.0.iter().map(|p| f(p).into()).collect())),
            other => other.clone(),
        }
    }
}

/// A name with `*` wildcards, as the literal pieces around them (one more
/// piece than wildcards).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Glob(Box<[Box<str>]>);

impl Glob {
    pub(crate) fn matches(&self, s: &str) -> bool {
        let p = &self.0;
        let (first, last) = (&*p[0], &*p[p.len() - 1]);
        if s.len() < first.len() + last.len() || !s.starts_with(first) || !s.ends_with(last) {
            return false;
        }
        let mut rest = &s[first.len()..s.len() - last.len()];
        for mid in &p[1..p.len() - 1] {
            match rest.find(&**mid) {
                Some(i) => rest = &rest[i + mid.len()..],
                None => return false,
            }
        }
        true
    }

    pub(crate) fn pieces(&self) -> &[Box<str>] {
        &self.0
    }

    /// Whether this glob matches every name `other` matches. False when
    /// unsure: it must find its first piece at the start of `other`'s first,
    /// its last at the end of `other`'s last, and the pieces in between, in
    /// order, within `other`'s pieces.
    fn covers(&self, other: &Glob) -> bool {
        let (p, q) = (&self.0, &other.0);
        let (first, last) = (&*p[0], &*p[p.len() - 1]);
        if !q[0].starts_with(first) || !q[q.len() - 1].ends_with(last) {
            return false;
        }
        // Where the next middle piece may start: a piece of `other`, and an offset in it.
        let mut at = (0, first.len());
        for mid in &p[1..p.len() - 1] {
            loop {
                let (k, start) = at;
                let piece = &*q[k];
                let end = if k == q.len() - 1 { piece.len() - last.len() } else { piece.len() };
                if let Some(i) = piece.get(start..end).and_then(|s| s.find(&**mid)) {
                    at = (k, start + i + mid.len());
                    break;
                }
                if k + 1 == q.len() {
                    return false;
                }
                at = (k + 1, 0);
            }
        }
        true
    }

    /// Whether some name may match both globs. True when unsure.
    fn may_meet(&self, other: &Glob) -> bool {
        let (p, q) = (&self.0, &other.0);
        let prefixed = |a: &str, b: &str| a.starts_with(b) || b.starts_with(a);
        let suffixed = |a: &str, b: &str| a.ends_with(b) || b.ends_with(a);
        prefixed(&p[0], &q[0]) && suffixed(&p[p.len() - 1], &q[q.len() - 1])
    }
}

/// Whether `a` matches every name `b` matches (single-name segments; false
/// when unsure).
fn name_covers(a: &Seg, b: &Seg) -> bool {
    match (a, b) {
        (Seg::Star, _) => true,
        (Seg::Lit(x), Seg::Lit(y)) => x == y,
        (Seg::Glob(g), Seg::Lit(y)) => g.matches(y),
        (Seg::Glob(g), Seg::Glob(h)) => g.covers(h),
        _ => false,
    }
}

/// Whether some name may match both `a` and `b` (single-name segments;
/// true when unsure).
fn names_meet(a: &Seg, b: &Seg) -> bool {
    match (a, b) {
        (Seg::Star, _) | (_, Seg::Star) => true,
        (Seg::Lit(x), Seg::Lit(y)) => x == y,
        (Seg::Glob(g), Seg::Lit(y)) | (Seg::Lit(y), Seg::Glob(g)) => g.matches(y),
        (Seg::Glob(g), Seg::Glob(h)) => g.may_meet(h),
        _ => true,
    }
}

/// A pattern as [`covers`] and [`may_meet`] compare them: it matches whole
/// paths only, and with `below` also the paths below them.
#[derive(Clone, Copy)]
pub(crate) struct Pattern<'a> {
    pub segs: &'a [Seg],
    pub below: bool,
}

static GLOBSTAR: Seg = Seg::GlobStar;

impl<'a> Pattern<'a> {
    fn len(&self) -> usize {
        self.segs.len() + self.below as usize
    }

    fn get(&self, i: usize) -> Option<&'a Seg> {
        self.segs.get(i).or((self.below && i == self.segs.len()).then_some(&GLOBSTAR))
    }
}

/// The number of leading names `a` and `b` have in common, or `None` if
/// they differ in a name before either has a wildcard.
fn common_names(a: Pattern, b: Pattern) -> Option<usize> {
    let mut n = 0;
    loop {
        match (a.get(n), b.get(n)) {
            (Some(Seg::Lit(x)), Some(Seg::Lit(y))) if x == y => n += 1,
            (Some(Seg::Lit(_)), Some(Seg::Lit(_))) => return None,
            _ => return Some(n),
        }
    }
}

/// Memoized results of comparing the segments of two patterns from
/// position `start` on.
struct Memo {
    start: usize,
    width: usize,
    results: Vec<Option<bool>>,
}

impl Memo {
    fn new(a: Pattern, b: Pattern, start: usize) -> Memo {
        let width = b.len() + 1 - start;
        Memo { start, width, results: vec![None; (a.len() + 1 - start) * width] }
    }

    fn slot(&mut self, i: usize, j: usize) -> &mut Option<bool> {
        &mut self.results[(i - self.start) * self.width + j - self.start]
    }
}

/// Whether `a` matches every path that `b` matches. False when unsure.
pub(crate) fn covers(a: Pattern, b: Pattern) -> bool {
    fn go(a: Pattern, b: Pattern, i: usize, j: usize, memo: &mut Memo) -> bool {
        if let Some(r) = *memo.slot(i, j) {
            return r;
        }
        let r = match (a.get(i), b.get(j)) {
            (None, None) => true,
            // `a`'s `**` matches no more names, or also those of `b`'s next segment.
            (Some(Seg::GlobStar), _) => go(a, b, i + 1, j, memo) || (j < b.len() && go(a, b, i, j + 1, memo)),
            (None, Some(_)) | (Some(_), None) | (Some(_), Some(Seg::GlobStar)) => false,
            (Some(x), Some(y)) => name_covers(x, y) && go(a, b, i + 1, j + 1, memo),
        };
        *memo.slot(i, j) = Some(r);
        r
    }
    let Some(n) = common_names(a, b) else { return false };
    go(a, b, n, n, &mut Memo::new(a, b, n))
}

/// Whether some path may match both patterns. True when unsure.
pub(crate) fn may_meet(a: Pattern, b: Pattern) -> bool {
    fn go(a: Pattern, b: Pattern, i: usize, j: usize, memo: &mut Memo) -> bool {
        if let Some(r) = *memo.slot(i, j) {
            return r;
        }
        let r = match (a.get(i), b.get(j)) {
            (None, None) => true,
            (Some(Seg::GlobStar), _) => go(a, b, i + 1, j, memo) || (j < b.len() && go(a, b, i, j + 1, memo)),
            (_, Some(Seg::GlobStar)) => go(a, b, i, j + 1, memo) || (i < a.len() && go(a, b, i + 1, j, memo)),
            (None, Some(_)) | (Some(_), None) => false,
            (Some(x), Some(y)) => names_meet(x, y) && go(a, b, i + 1, j + 1, memo),
        };
        *memo.slot(i, j) = Some(r);
        r
    }
    let Some(n) = common_names(a, b) else { return false };
    go(a, b, n, n, &mut Memo::new(a, b, n))
}

/// The fewest names a path matching `pattern` has, and the most (`None`:
/// any number, after a `**`).
pub(crate) fn name_counts(pattern: &[Seg]) -> (usize, Option<usize>) {
    let fixed = pattern.iter().filter(|s| **s != Seg::GlobStar).count();
    (fixed, (fixed == pattern.len()).then_some(fixed))
}

/// The names of a normalized path, its root (`/`, or a Windows prefix and
/// root) first, as rules and requests are compared.
pub(crate) fn path_segments(p: &Path) -> Vec<Cow<'_, str>> {
    p.components()
        .filter_map(|c| match c {
            Component::Prefix(x) => Some(x.as_os_str().to_string_lossy()),
            Component::RootDir => Some(Cow::Borrowed("/")),
            Component::Normal(n) => Some(n.to_string_lossy()),
            Component::CurDir | Component::ParentDir => None,
        })
        .collect()
}

/// A trie of patterns: matching a path against all of them takes one
/// step per name of the path (more where patterns have `**`), whatever
/// their number.
#[derive(Debug)]
pub(crate) struct Trie<V> {
    nodes: Vec<Node<V>>,
    /// The name children of every node.
    names: hashbrown::HashMap<Edge, u32, FxBuild>,
}

#[derive(Debug)]
struct Node<V> {
    values: Vec<V>,
    /// Bits of the values here and below ([`Trie::insert`]'s `mask`).
    below: u8,
    parent: u32,
    /// Whether the node is a `**`, which may match more names.
    repeats: bool,
    /// Whether a `**` is on the way to the node: only then can matching
    /// reach it in several ways.
    after_globstar: bool,
    has_names: bool,
    globs: Vec<(Glob, u32)>,
    /// Child for `*` (0: none; the root is no one's child).
    star: u32,
    globstar: u32,
}

impl<V> Node<V> {
    fn new(parent: u32) -> Node<V> {
        Node {
            values: Vec::new(),
            below: 0,
            parent,
            repeats: false,
            after_globstar: false,
            has_names: false,
            globs: Vec::new(),
            star: 0,
            globstar: 0,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Edge(u32, Box<str>);

/// An [`Edge`] to look up without allocating.
struct EdgeRef<'a>(u32, &'a str);

impl Hash for EdgeRef<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.hash(h);
        self.1.hash(h);
    }
}

impl hashbrown::Equivalent<Edge> for EdgeRef<'_> {
    fn equivalent(&self, e: &Edge) -> bool {
        self.0 == e.0 && self.1 == &*e.1
    }
}

impl<V> Default for Trie<V> {
    fn default() -> Self {
        Trie { nodes: vec![Node::new(0)], names: Default::default() }
    }
}

impl<V> Trie<V> {
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.len() == 1 && self.nodes[0].values.is_empty()
    }

    /// Adds `value` for `pattern`; `mask` marks it for [`Trie::any_below`].
    pub(crate) fn insert(&mut self, pattern: &[Seg], value: V, mask: u8) {
        let mut n = 0u32;
        for seg in pattern {
            let next = self.nodes.len() as u32;
            let node = &mut self.nodes[n as usize];
            let child = match seg {
                Seg::Lit(s) => {
                    node.has_names = true;
                    *self.names.entry(Edge(n, s.clone())).or_insert(next)
                }
                Seg::Glob(g) => match node.globs.iter().find(|(h, _)| h == g) {
                    Some(&(_, c)) => c,
                    None => {
                        node.globs.push((g.clone(), next));
                        next
                    }
                },
                Seg::Star => *zero_or(&mut node.star, next),
                Seg::GlobStar => *zero_or(&mut node.globstar, next),
            };
            if child == next {
                let repeats = *seg == Seg::GlobStar;
                let after_globstar = repeats || self.nodes[n as usize].after_globstar;
                self.nodes.push(Node { repeats, after_globstar, ..Node::new(n) });
            }
            n = child;
        }
        self.nodes[n as usize].values.push(value);
        while self.nodes[n as usize].below & mask != mask {
            self.nodes[n as usize].below |= mask;
            if n == 0 {
                break;
            }
            n = self.nodes[n as usize].parent;
        }
    }

    /// Calls `f(node, i)` for every state of matching `input`: the node
    /// reached having matched its first `i` names.
    fn states(&self, input: &[&str], mut f: impl FnMut(&Node<V>, usize)) {
        let mut stack = vec![(0u32, 0usize)];
        let mut seen = hashbrown::HashSet::<(u32, usize), FxBuild>::default();
        while let Some((n, i)) = stack.pop() {
            let node = &self.nodes[n as usize];
            if node.after_globstar && !seen.insert((n, i)) {
                continue;
            }
            f(node, i);
            // `**` matches no names, or one more than it has.
            if node.globstar != 0 {
                stack.push((node.globstar, i));
            }
            let Some(&name) = input.get(i) else { continue };
            if node.repeats {
                stack.push((n, i + 1));
            }
            if node.has_names
                && let Some(&c) = self.names.get(&EdgeRef(n, name))
            {
                stack.push((c, i + 1));
            }
            stack.extend(node.globs.iter().filter(|(g, _)| g.matches(name)).map(|&(_, c)| (c, i + 1)));
            if node.star != 0 {
                stack.push((node.star, i + 1));
            }
        }
    }

    /// Calls `f(value, depth)` for each pattern matching a leading part of
    /// `input` (a path or one of its ancestors), with the number of names
    /// it matched.
    pub(crate) fn matches(&self, input: &[&str], mut f: impl FnMut(&V, usize)) {
        self.states(input, |node, i| node.values.iter().for_each(|v| f(v, i)));
    }

    /// Whether a value with a bit of `mask` has a pattern that may match
    /// `input` or a path below it.
    pub(crate) fn any_below(&self, input: &[&str], mask: u8) -> bool {
        let mut found = false;
        self.states(input, |node, i| found |= i == input.len() && node.below & mask != 0);
        found
    }

    /// Whether some pattern names the last name of `input` (exactly or
    /// with wildcards around text), so that whether it matches may depend
    /// on the name's spelling.
    pub(crate) fn names_last(&self, input: &[&str]) -> bool {
        let mut found = false;
        self.states(input, |node, i| found |= i + 1 == input.len() && (node.has_names || !node.globs.is_empty()));
        found
    }
}

fn zero_or(slot: &mut u32, next: u32) -> &mut u32 {
    if *slot == 0 {
        *slot = next;
    }
    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(s: &str) -> Vec<Seg> {
        s.split('/').map(|seg| Seg::parse(&seg.chars().map(|c| (c, c == '*')).collect::<Vec<_>>()).unwrap()).collect()
    }

    fn matches(t: &Trie<&'static str>, path: &str) -> Vec<(&'static str, usize)> {
        let input: Vec<&str> = path.split('/').collect();
        let mut out = Vec::new();
        t.matches(&input, |v, i| out.push((*v, i)));
        out.sort();
        out
    }

    /// Matching takes a step per node and name at most, however many `**`
    /// a pattern has.
    #[test]
    fn globstars_match_in_linear_time() {
        let mut t = Trie::default();
        t.insert(&pattern("r/**/b/**/b/**/c"), "", 1);
        let input: Vec<&str> = std::iter::once("r").chain(std::iter::repeat_n("b", 2000)).collect();
        let mut states = 0;
        t.states(&input, |_, _| states += 1);
        assert!(states <= t.nodes.len() * (input.len() + 1), "{states} states");
    }

    #[test]
    fn globs() {
        let g = |s: &str| match pattern(s).pop() {
            Some(Seg::Glob(g)) => g,
            other => panic!("{other:?}"),
        };
        assert!(g("*.pem").matches("key.pem"));
        assert!(g("*.pem").matches(".pem"));
        assert!(!g("*.pem").matches("key.pem.bak"));
        assert!(g("a*b*c").matches("abc"));
        assert!(g("a*b*c").matches("a-b-b-c"));
        assert!(!g("a*b*c").matches("ac"));
        assert!(!g("ab*ba").matches("aba"));
        assert_eq!(Seg::parse(&[('a', false), ('*', true), ('*', true)]), Err("`**` must be a whole path segment".into()));
    }

    #[test]
    fn covering_and_meeting_patterns() {
        fn exact(segs: &[Seg]) -> Pattern<'_> {
            Pattern { segs, below: false }
        }
        let covers = |a: &str, b: &str| super::covers(exact(&pattern(a)), exact(&pattern(b)));
        let meet = |a: &str, b: &str| may_meet(exact(&pattern(a)), exact(&pattern(b)));
        assert!(covers("r/**", "r/*") && covers("r/**", "r/a/**/b") && covers("r/**", "r"));
        assert!(!covers("r/*", "r/**") && !covers("r/*", "r"));
        assert!(covers("r/**/b", "r/*/b") && covers("r/**/b", "r/**/b") && covers("**/x", "r/**/x"));
        assert!(!covers("r/*/b", "r/**/b") && !covers("r/**/b", "r/*/c"));
        assert!(covers("r/*/b", "r/a/b") && !covers("r/a/b", "r/*/b"));
        assert!(covers("r/*.pem", "r/key*.pem") && covers("r/*.pem", "r/k.pem") && !covers("r/k*.pem", "r/*.pem"));
        assert!(covers("a*b*c", "ab*xbx*c") && covers("*b*", "x*b") && !covers("*b*", "x*y"));
        // `a*a` matches `aa`, which `aa*a` does not.
        assert!(covers("a*a", "aa*a") && !covers("aa*a", "a*a"));
        assert!(meet("r/*.pem", "r/key*") && meet("r/**/x", "r/a/b/x") && meet("r/**", "r") && meet("r/*/**", "**/b"));
        assert!(!meet("r/a", "r/b") && !meet("r/*.pem", "r/*.key") && !meet("r/*", "r") && !meet("r/**/x", "s/**"));
        assert_eq!(name_counts(&pattern("r/*/x")), (3, Some(3)));
        assert_eq!(name_counts(&pattern("r/**/x")), (2, None));
    }

    #[test]
    fn patterns_match_paths_and_what_is_below_them() {
        let mut t = Trie::default();
        for p in ["r/a", "r/a/*/c", "r/**/x.pem", "r/*.txt", "r/a/b"] {
            t.insert(&pattern(p), p, 1);
        }
        assert_eq!(matches(&t, "r/a/b/c/d"), [("r/a", 2), ("r/a/*/c", 4), ("r/a/b", 3)]);
        assert_eq!(matches(&t, "r/n.txt/x.pem"), [("r/**/x.pem", 3), ("r/*.txt", 2)]);
        assert_eq!(matches(&t, "r/x.pem"), [("r/**/x.pem", 2)]);
        assert_eq!(matches(&t, "r/a/b/c/x.pem"), [("r/**/x.pem", 5), ("r/a", 2), ("r/a/*/c", 4), ("r/a/b", 3)]);
        assert_eq!(matches(&t, "s/a"), []);
        let input = |s: &'static str| s.split('/').collect::<Vec<_>>();
        assert!(t.any_below(&input("r/a/q"), 1), "r/a/*/c");
        assert!(t.any_below(&input("r/z/y"), 1), "r/**/x.pem");
        assert!(!t.any_below(&input("s"), 1));
        assert!(t.names_last(&input("r/a")));
        assert!(t.names_last(&input("r/q/q")), "r/**/x.pem");
        let mut t = Trie::default();
        t.insert(&pattern("r/a/*"), "", 1);
        assert!(!t.names_last(&input("r/a/q")));
        assert!(t.names_last(&input("r/q")));
    }
}
