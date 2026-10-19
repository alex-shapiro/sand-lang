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
            Seg::Glob(g) => Seg::Glob(g.map(f)),
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

    /// The glob with its pieces mapped by `f`.
    pub(crate) fn map(&self, f: impl Fn(&str) -> String) -> Glob {
        Glob(self.0.iter().map(|p| f(p).into()).collect())
    }
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
                    match self.names.get(&EdgeRef(n, s)) {
                        Some(&c) => c,
                        None => *self.names.entry(Edge(n, s.clone())).or_insert(next),
                    }
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

    /// Returns true if a pattern matches the final component of `input`.
    pub(crate) fn matches_last(&self, input: &[&str]) -> bool {
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
        assert!(t.matches_last(&input("r/a")));
        assert!(t.matches_last(&input("r/q/q")), "r/**/x.pem");
        let mut t = Trie::default();
        t.insert(&pattern("r/a/*"), "", 1);
        assert!(!t.matches_last(&input("r/a/q")));
        assert!(t.matches_last(&input("r/q")));
    }
}
