//! Finds if a decision changes when you remove a rule. The search examines
//! each type of path that the rule applies to. For each path, it finds the
//! rule that decides with the rule and without the rule.
//!
//! The search moves along paths one name at a time. It does not examine all
//! names. At each step, it puts the names into a small number of classes: one
//! class for each name that a rule writes in full, and classes for each
//! combination of wildcard patterns that other names can match. The search
//! follows one path for each class. It can examine combinations that no real
//! name makes, but it does not miss a combination that a real name makes.
//! Thus, if the search finds that a rule is not necessary, this is correct.

use super::super::fold_name;
use super::super::path::{Glob, Seg};
use crate::vm::value::{FxBuild, FxHashMap};
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashSet};

type FxHashSet<T> = HashSet<T, FxBuild>;

/// A rule as it may decide requests in one context (a file operation, or
/// the method, scheme, host and port of http requests).
#[derive(Clone, Copy)]
pub(super) struct Cand<'a> {
    pub rule: usize,
    pub allow: bool,
    /// How it ranks before the depth of its match (http rules: the host
    /// pattern, then the scheme).
    pub head: [u32; 2],
    /// How it ranks after the depth (the weight of the pattern, then for
    /// http rules whether it names methods).
    pub tail: [u32; 2],
    /// Its path pattern; a deny rule's with folded names, as the policy
    /// compares them.
    pub segs: &'a [Seg],
}

impl Cand<'_> {
    /// How it ranks against a rule of the same head matching as deep: the
    /// heavier, then a deny, then the first.
    fn rank(&self) -> ([u32; 2], bool, Reverse<usize>) {
        (self.tail, !self.allow, Reverse(self.rule))
    }
}

/// What decides in place of a rule, gathered over the contexts it applies in.
#[derive(Default)]
pub(super) struct Effect {
    /// The rules deciding, without the rule, what it decides (the same way).
    pub backups: BTreeSet<usize>,
    /// The rules deciding what the rule matches but does not decide.
    pub overriders: BTreeSet<usize>,
}

/// Limits on the work of the linter. A rule whose analysis reaches them is
/// taken to be needed.
pub(super) struct Budget {
    /// States one search may visit.
    pub states: usize,
    /// Steps (of a rule through a name, or of gathering a rule) left for
    /// every rule's analysis together.
    pub steps: usize,
}

impl Budget {
    /// Spends `n` steps; `None` if there are not that many left.
    pub(super) fn spend(&mut self, n: usize) -> Option<()> {
        self.steps = self.steps.checked_sub(n)?;
        Some(())
    }

    fn spend_or_stop(&mut self, n: usize) -> Result<(), Stop> {
        self.spend(n).ok_or(Stop::Unknown)
    }
}

/// Adds to `effect` what decides in place of `cands[me]` in a context where
/// `cands` are the rules that may match paths it matches. False if removing
/// it changes a decision (or leaves a request undecided), or if the search
/// cannot tell.
pub(super) fn search(cands: &[Cand], me: usize, sample: &[Cow<str>], effect: &mut Effect, budget: &mut Budget) -> bool {
    // Rules deciding like the rule searched for only stand in for it: a
    // search without some that finds it unneeded is right. First without
    // those that match neither its sample path nor a path above it, then
    // with all, then without those with wildcard names (fewer to tell apart).
    let mine = cands[me];
    let likely = |c: &Cand| c.rule == mine.rule || c.allow != mine.allow || matches_above(c, sample);
    let plain = |c: &Cand| c.rule == mine.rule || c.allow != mine.allow || !c.segs.iter().any(|s| matches!(s, Seg::Glob(_)));
    let fewer = |keep: &dyn Fn(&Cand) -> bool| -> Vec<Cand> { cands.iter().filter(|c| keep(c)).copied().collect() };
    let within = |cands: &[Cand], effect: &mut Effect, budget: &mut Budget| {
        let me = cands.iter().position(|c| c.rule == mine.rule).unwrap_or(0);
        search_all(cands, me, effect, budget)
    };
    let likely = fewer(&likely);
    if likely.len() < cands.len() {
        let mut found = Effect::default();
        if within(&likely, &mut found, budget).is_ok() {
            effect.backups.extend(found.backups);
            effect.overriders.extend(found.overriders);
            return true;
        }
    }
    match within(cands, effect, budget) {
        Ok(()) => true,
        Err(Stop::Needed) => false,
        Err(Stop::Unknown) => {
            let plain = fewer(&plain);
            plain.len() < cands.len() && within(&plain, effect, budget).is_ok()
        }
    }
}

/// Whether `c` matches the path `names` or a path above it.
fn matches_above(c: &Cand, names: &[Cow<str>]) -> bool {
    if c.segs.len() >= 64 {
        return false;
    }
    let mut bits = closure(c.segs, 1);
    for name in names {
        if bits & (1 << c.segs.len()) != 0 {
            return true;
        }
        let folded = if c.allow { Cow::Borrowed(&**name) } else { fold_name(name) };
        bits = step(c.segs, bits, &folded);
    }
    bits & (1 << c.segs.len()) != 0
}

/// The segments of `segs` matched after one more name, from those in `bits`.
fn step(segs: &[Seg], bits: u64, name: &str) -> u64 {
    let mut next = 0;
    for (p, seg) in segs.iter().enumerate().filter(|&(p, _)| bits & (1 << p) != 0) {
        match seg {
            Seg::GlobStar => next |= 1 << p,
            Seg::Star => next |= 1 << (p + 1),
            Seg::Lit(n) if **n == *name => next |= 1 << (p + 1),
            Seg::Glob(g) if g.matches(name) => next |= 1 << (p + 1),
            _ => {}
        }
    }
    if next == 0 { 0 } else { closure(segs, next) }
}

/// Why a search stopped early.
enum Stop {
    /// Removing the rule changes a decision.
    Needed,
    /// The search reached a limit.
    Unknown,
}

fn search_all(cands: &[Cand], me: usize, effect: &mut Effect, budget: &mut Budget) -> Result<(), Stop> {
    if cands.iter().any(|c| c.segs.len() >= 64) {
        return Err(Stop::Unknown);
    }
    let mut heads: Vec<[u32; 2]> = cands.iter().map(|c| c.head).collect();
    heads.sort_unstable_by(|a, b| b.cmp(a));
    heads.dedup();
    let search = Search {
        cands,
        me: me as u32,
        level: cands.iter().map(|c| heads.iter().position(|h| *h == c.head).unwrap_or(0)).collect(),
        levels: heads.len(),
        classes: FxHashMap::default(),
    };
    search.run(effect, budget)
}

/// Whether `cands[me]` decides the path `names` or a path above it unlike
/// the other rules would (or decides one they leave undecided): a quick
/// way to show that a rule is needed.
pub(super) fn decides_at(cands: &[Cand], me: usize, names: &[Cow<str>]) -> bool {
    let me = &cands[me];
    let mut bits: Vec<u64> = cands.iter().map(|c| if c.segs.len() < 64 { closure(c.segs, 1) } else { 0 }).collect();
    let key = |c: &Cand| (c.head, c.rank());
    // The deciding rule so far, and the deciding rule but for `me`.
    let mut best: [Option<&Cand>; 2] = [None, None];
    for depth in 0..=names.len() {
        let (raw, folded) = match depth {
            0 => ("", "".into()),
            _ => (&*names[depth - 1], fold_name(&names[depth - 1])),
        };
        // Rules matching here outrank those matching above with the same head.
        let mut here: [Option<&Cand>; 2] = [None, None];
        for (c, b) in cands.iter().zip(&mut bits) {
            if depth > 0 && *b != 0 {
                *b = step(c.segs, *b, if c.allow { raw } else { &folded });
            }
            if *b & (1 << c.segs.len()) != 0 {
                for slot in &mut here[..1 + (c.rule != me.rule) as usize] {
                    if slot.is_none_or(|s| key(c) > key(s)) {
                        *slot = Some(c);
                    }
                }
            }
        }
        for (slot, new) in best.iter_mut().zip(here) {
            if let Some(new) = new
                && slot.is_none_or(|old| old.head <= new.head)
            {
                *slot = Some(new);
            }
        }
        if best[0].is_some_and(|c| c.rule == me.rule) && best[1].is_none_or(|c| c.allow != me.allow) {
            return true;
        }
    }
    false
}

/// `bits` with the positions after each `**` at a position in it: `**`
/// may match no names.
fn closure(segs: &[Seg], mut bits: u64) -> u64 {
    for (p, seg) in segs.iter().enumerate() {
        if bits & (1 << p) != 0 && *seg == Seg::GlobStar {
            bits |= 1 << (p + 1);
        }
    }
    bits
}

/// The most wildcard names a step of a search tells apart.
const MAX_GLOBS: usize = 32;

/// No rule (in [`State::with`] and [`State::without`]).
const NONE: u32 = u32::MAX;

/// Where a search is, after some paths.
#[derive(Clone)]
struct State {
    key: Key,
    /// For each level of heads (highest first), the rule that ranks first
    /// among those matching deepest.
    with: Vec<u32>,
    /// The same, leaving out the rule searched for.
    without: Vec<u32>,
}

/// What a state is to the search: two paths with the same key are alike
/// for every rule, as are all paths that continue them.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    /// The rules that may match a longer path, with the numbers of their
    /// segments the path so far matches (bits: bit `n` for the first `n`).
    alive: Vec<(u32, u64)>,
    /// For each level, the kinds ([`Search::kind`]) of the rules in
    /// [`State::with`] and [`State::without`].
    kinds: Vec<(u8, u8)>,
    /// Whether the rule searched for matches the path or an ancestor.
    seen: bool,
}

/// A class of names: those that compare alike with every pattern at a step.
#[derive(Clone, Copy)]
enum Name<'a> {
    /// A name an allow rule spells out.
    Raw(&'a str),
    /// A name that a deny rule spells out (folded), spelled unlike every
    /// allow rule's names, and matching the allow rules' wildcard names in
    /// the bits.
    Folded(&'a str, u64),
    /// Any other name, matching the wildcard names of allow and deny rules
    /// in the bits.
    Other(u64, u64),
}

/// The patterns rules may match the next name with.
#[derive(Default)]
struct Next<'a> {
    allow_names: FxHashMap<&'a str, Vec<u32>>,
    deny_names: FxHashMap<&'a str, Vec<u32>>,
    allow_globs: Vec<&'a Glob>,
    deny_globs: Vec<&'a Glob>,
    /// The rules that may match any name there (with `*`, `**` or a
    /// wildcard name), by their position in `alive`.
    any: Vec<u32>,
}

struct Search<'a, 'b> {
    cands: &'b [Cand<'a>],
    me: u32,
    level: Vec<usize>,
    levels: usize,
    /// [`other_names`] by the allow and deny wildcard names it was given.
    classes: FxHashMap<[Vec<&'a Glob>; 2], Option<Matchings>>,
}

impl<'a> Search<'a, '_> {
    fn run(mut self, effect: &mut Effect, budget: &mut Budget) -> Result<(), Stop> {
        let alive: Vec<(u32, u64)> = self.cands.iter().enumerate().map(|(c, cand)| (c as u32, closure(cand.segs, 1))).collect();
        let start = self.arrive(alive, vec![NONE; self.levels], vec![NONE; self.levels], false);
        self.judge(&start, effect)?;
        let mut visited = FxHashSet::default();
        let mut stack = vec![start];
        while let Some(state) = stack.pop() {
            if visited.len() > budget.states {
                return Err(Stop::Unknown);
            }
            for next in self.successors(&state, budget)? {
                if next.key.seen || next.key.alive.iter().any(|&(c, _)| c == self.me) {
                    self.judge(&next, effect)?;
                    if visited.insert(next.key.clone()) {
                        stack.push(next);
                    }
                }
            }
        }
        Ok(())
    }

    /// What kind of rule `c` is in a state: none, the rule searched for,
    /// another allow rule or another deny rule.
    fn kind(&self, c: u32) -> u8 {
        match c {
            NONE => 0,
            c if c == self.me => 1,
            c => 2 + !self.cands[c as usize].allow as u8,
        }
    }

    /// The state after a step to the positions `alive`, given the best
    /// rules before it.
    fn arrive(&self, mut alive: Vec<(u32, u64)>, mut with: Vec<u32>, mut without: Vec<u32>, mut seen: bool) -> State {
        let mut here_with = vec![NONE; self.levels];
        let mut here_without = vec![NONE; self.levels];
        let better = |slot: &mut u32, c: u32| {
            if *slot == NONE || self.cands[c as usize].rank() > self.cands[*slot as usize].rank() {
                *slot = c;
            }
        };
        for &(c, bits) in &alive {
            if bits & (1 << self.cands[c as usize].segs.len()) == 0 {
                continue;
            }
            let level = self.level[c as usize];
            better(&mut here_with[level], c);
            if c == self.me {
                seen = true;
            } else {
                better(&mut here_without[level], c);
            }
        }
        for level in 0..self.levels {
            if here_with[level] != NONE {
                with[level] = here_with[level];
            }
            if here_without[level] != NONE {
                without[level] = here_without[level];
            }
        }
        // Rules that matched all their segments go no further.
        alive.retain(|&(c, bits)| bits & !(1 << self.cands[c as usize].segs.len()) != 0);
        let kinds = with.iter().zip(&without).map(|(&w, &o)| (self.kind(w), self.kind(o))).collect();
        State { key: Key { alive, kinds, seen }, with, without }
    }

    /// Records what decides at `state` in place of the rule searched for.
    fn judge(&self, state: &State, effect: &mut Effect) -> Result<(), Stop> {
        if !state.key.seen {
            return Ok(());
        }
        let top = |rules: &[u32]| rules.iter().copied().find(|&c| c != NONE);
        let with = top(&state.with).ok_or(Stop::Needed)?;
        if with != self.me {
            effect.overriders.insert(self.cands[with as usize].rule);
            return Ok(());
        }
        let without = &self.cands[top(&state.without).ok_or(Stop::Needed)? as usize];
        if without.allow != self.cands[self.me as usize].allow {
            return Err(Stop::Needed);
        }
        effect.backups.insert(without.rule);
        Ok(())
    }

    /// The states one name further on, a state per class of names.
    fn successors(&mut self, state: &State, budget: &mut Budget) -> Result<Vec<State>, Stop> {
        let mut next = Next::default();
        let mut known: (FxHashSet<&Glob>, FxHashSet<&Glob>) = Default::default();
        budget.spend_or_stop(state.key.alive.len())?;
        for (at, &(c, bits)) in state.key.alive.iter().enumerate() {
            let cand = &self.cands[c as usize];
            let mut any = false;
            for (_, seg) in cand.segs.iter().enumerate().filter(|&(p, _)| bits & (1 << p) != 0) {
                match (seg, cand.allow) {
                    (Seg::Lit(n), true) => next.allow_names.entry(n).or_default().push(at as u32),
                    (Seg::Lit(n), false) => next.deny_names.entry(n).or_default().push(at as u32),
                    (Seg::Glob(g), allow) => {
                        let (globs, known) =
                            if allow { (&mut next.allow_globs, &mut known.0) } else { (&mut next.deny_globs, &mut known.1) };
                        if known.insert(g) {
                            globs.push(g);
                        }
                        any = true;
                    }
                    (Seg::Star | Seg::GlobStar, _) => any = true,
                }
            }
            if any {
                next.any.push(at as u32);
            }
        }
        if next.allow_globs.len() + next.deny_globs.len() > MAX_GLOBS {
            return Err(Stop::Unknown);
        }
        let mut names: Vec<Name> = next.allow_names.keys().map(|&n| Name::Raw(n)).collect();
        for &m in next.deny_names.keys() {
            // The allow rules' wildcard names that a spelling of `m` may match.
            let maybe: Vec<usize> = (0..next.allow_globs.len())
                .filter(|&k| {
                    let g = next.allow_globs[k];
                    !g.pieces().iter().all(|p| p.is_ascii()) || folded(g).matches(m)
                })
                .collect();
            if maybe.len() > 6 {
                return Err(Stop::Unknown);
            }
            for subset in 0..1u64 << maybe.len() {
                let bits = maybe.iter().enumerate().filter(|&(j, _)| subset & (1 << j) != 0).fold(0, |b, (_, &k)| b | 1 << k);
                names.push(Name::Folded(m, bits));
            }
        }
        let key = [next.allow_globs.clone(), next.deny_globs.clone()];
        let others =
            self.classes.entry(key).or_insert_with(|| other_names(&next.allow_globs, &next.deny_globs)).clone().ok_or(Stop::Unknown)?;
        names.extend(others.into_iter().map(|(a, d)| Name::Other(a, d)));

        let mut states = Vec::with_capacity(names.len());
        let mut touched: Vec<u32> = Vec::new();
        for name in names {
            touched.clear();
            touched.extend_from_slice(&next.any);
            match name {
                Name::Raw(n) => {
                    touched.extend(next.allow_names.get(n).into_iter().flatten());
                    touched.extend(next.deny_names.get(&*fold_name(n)).into_iter().flatten());
                }
                Name::Folded(m, _) => touched.extend(next.deny_names.get(m).into_iter().flatten()),
                Name::Other(..) => {}
            }
            touched.sort_unstable();
            touched.dedup();
            budget.spend_or_stop(touched.len())?;
            let mut alive = Vec::with_capacity(touched.len());
            for &at in &touched {
                let (c, bits) = state.key.alive[at as usize];
                let bits = self.step(c, bits, name, &next);
                if bits != 0 {
                    alive.push((c, bits));
                }
            }
            states.push(self.arrive(alive, state.with.clone(), state.without.clone(), state.key.seen));
        }
        Ok(states)
    }

    /// The segments of rule `c` matched after one more name (of class
    /// `name`), from those in `bits`.
    fn step(&self, c: u32, bits: u64, name: Name, next: &Next) -> u64 {
        let cand = &self.cands[c as usize];
        let mut out = 0;
        for (p, seg) in cand.segs.iter().enumerate().filter(|&(p, _)| bits & (1 << p) != 0) {
            let matched = match (seg, cand.allow, name) {
                (Seg::GlobStar, ..) => {
                    out |= 1 << p;
                    false
                }
                (Seg::Star, ..) => true,
                (Seg::Lit(n), true, Name::Raw(r)) => **n == *r,
                (Seg::Lit(n), false, Name::Raw(r)) => **n == *fold_name(r),
                (Seg::Lit(n), false, Name::Folded(m, _)) => **n == *m,
                (Seg::Lit(_), ..) => false,
                (Seg::Glob(g), true, Name::Raw(r)) => g.matches(r),
                (Seg::Glob(g), false, Name::Raw(r)) => g.matches(&fold_name(r)),
                (Seg::Glob(g), false, Name::Folded(m, _)) => g.matches(m),
                (Seg::Glob(g), true, Name::Folded(_, a) | Name::Other(a, _)) => bit(a, &next.allow_globs, g),
                (Seg::Glob(g), false, Name::Other(_, d)) => bit(d, &next.deny_globs, g),
            };
            if matched {
                out |= 1 << (p + 1);
            }
        }
        if out == 0 { 0 } else { closure(cand.segs, out) }
    }
}

/// Whether the bit of `g` among `globs` is set in `bits`.
fn bit(bits: u64, globs: &[&Glob], g: &Glob) -> bool {
    globs.iter().position(|h| *h == g).is_some_and(|k| bits & (1 << k) != 0)
}

/// An ASCII wildcard name with its pieces folded: if a name matches the
/// name, its folded spelling matches this.
fn folded(g: &Glob) -> Glob {
    g.map(|p| fold_name(p).into_owned())
}

/// Wildcard names a name matches: bits for the allow and the deny rules'.
type Matchings = Vec<(u64, u64)>;

/// The ways names outside the classes that rules spell out may match the
/// wildcard names `allow` (as written) and `deny` (folded): pairs of bits
/// for the allow and the deny wildcard names they match. Every way a name
/// can match them is among the pairs. `None` if there are too many to tell.
fn other_names(allow: &[&Glob], deny: &[&Glob]) -> Option<Matchings> {
    let raw = matchings(allow)?;
    // What the folded spelling of a name matching an ASCII allow wildcard
    // name must match: the name folded.
    let ascii: Vec<usize> = (0..allow.len()).filter(|&k| allow[k].pieces().iter().all(|p| p.is_ascii())).collect();
    let links: Vec<Glob> = ascii.iter().map(|&k| folded(allow[k])).collect();
    let both: Vec<&Glob> = deny.iter().copied().chain(&links).collect();
    let spelled = matchings(&both)?;
    let mut pairs = FxHashSet::default();
    for &a in &raw {
        for &f in &spelled {
            let consistent = ascii.iter().enumerate().all(|(j, &k)| a & (1 << k) == 0 || f & (1 << (deny.len() + j)) != 0);
            if consistent {
                pairs.insert((a, f & ((1 << deny.len()) - 1)));
            }
        }
    }
    Some(pairs.into_iter().collect())
}

/// The sets of `globs` (as bits) that some nonempty name matches, found
/// by running them on every name over the characters they spell out and
/// one other. `None` if that takes too many steps.
fn matchings(globs: &[&Glob]) -> Option<Vec<u64>> {
    if globs.is_empty() {
        return Some(vec![0]);
    }
    // Each glob as characters and wildcards (`None`), at most 127.
    let tokens: Vec<Vec<Option<char>>> = globs
        .iter()
        .map(|g| {
            let mut t = Vec::new();
            for (k, piece) in g.pieces().iter().enumerate() {
                if k > 0 {
                    t.push(None);
                }
                t.extend(piece.chars().map(Some));
            }
            t
        })
        .collect();
    if tokens.iter().any(|t| t.len() > 127) {
        return None;
    }
    let mut chars: Vec<char> = tokens.iter().flatten().flatten().copied().collect();
    chars.sort_unstable();
    chars.dedup();
    chars.push(('!'..).find(|c| !chars.contains(c))?);
    let closure = |t: &[Option<char>], mut s: u128| {
        for (p, tok) in t.iter().enumerate() {
            if s & (1 << p) != 0 && tok.is_none() {
                s |= 1 << (p + 1);
            }
        }
        s
    };
    let start: Vec<u128> = tokens.iter().map(|t| closure(t, 1)).collect();
    let mut seen = FxHashSet::default();
    let mut found = FxHashSet::default();
    let mut stack = vec![start];
    while let Some(state) = stack.pop() {
        for &ch in &chars {
            let next: Vec<u128> = tokens
                .iter()
                .zip(&state)
                .map(|(t, &s)| {
                    let mut out = 0;
                    for (p, tok) in t.iter().enumerate().filter(|&(p, _)| s & (1 << p) != 0) {
                        match tok {
                            None => out |= 1 << p,
                            Some(c) if *c == ch => out |= 1 << (p + 1),
                            Some(_) => {}
                        }
                    }
                    closure(t, out)
                })
                .collect();
            let matched = tokens.iter().zip(&next).enumerate().fold(0u64, |b, (k, (t, &s))| b | (((s >> t.len()) & 1) as u64) << k);
            found.insert(matched);
            if !seen.contains(&next) {
                if seen.len() >= 4096 {
                    return None;
                }
                seen.insert(next.clone());
                stack.push(next);
            }
        }
    }
    Some(found.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(s: &str) -> Glob {
        match Seg::parse(&s.chars().map(|c| (c, c == '*')).collect::<Vec<_>>()).unwrap() {
            Seg::Glob(g) => g,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn names_match_wildcard_names_in_every_combination_they_can() {
        let (a, b, c) = (glob("*.rs"), glob("a*"), glob("*b.rs"));
        let mut found = matchings(&[&a, &b, &c]).unwrap();
        found.sort_unstable();
        // `ab.rs` matches all three; nothing matches `*b.rs` but not `*.rs`.
        assert_eq!(found, [0b000, 0b001, 0b010, 0b011, 0b101, 0b111]);
        // Every name a folded allow wildcard name matches, `a*` matching
        // `A*` folded: the pairs where `A*` matches but `a*` does not are
        // left out.
        let (upper, lower) = (glob("A*"), glob("a*"));
        let mut pairs = other_names(&[&upper], &[&lower]).unwrap();
        pairs.sort_unstable();
        assert_eq!(pairs, [(0, 0), (0, 1), (1, 1)]);
    }
}
