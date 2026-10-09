//! The `regex` module, backed by the engine of the `regex` crate
//! (`regex_automata::meta`, configured exactly like `regex::Regex`).
//!
//! Each compiled regex owns its search cache (instead of the `regex` crate's
//! per-thread pool), so the memory of the compiled program and of the cache
//! can be measured and counted against `max_memory`: the object's size is
//! recomputed after every search and the heap's accounting adjusted.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::platform::Instant;
use crate::vm::meter::{SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, NResult, Vm, panic};
use regex_automata::meta::{self, Cache};
use regex_automata::util::captures::{Captures, GroupInfo};
use regex_automata::util::interpolate;
use regex_automata::util::iter::Searcher;
use regex_automata::util::syntax;
use regex_automata::{Input, Match, MatchKind, PatternID};
use std::any::Any;
use std::cell::{Cell, RefCell};

/// Limit on each compiled NFA (forward and reverse) of a regex.
const NFA_SIZE_LIMIT: usize = 4 << 20;

/// Limit on the one-pass DFA of a regex.
const ONEPASS_SIZE_LIMIT: usize = 1 << 20;

/// Capacity of each lazy DFA cache of a regex (forward and reverse).
const DFA_CACHE_CAPACITY: usize = 2 << 20;

/// Approximate heap cost of one `Match` value (struct plus string).
const MATCH_COST: usize = 128;

/// Bytes counted per regex object besides the measured program and cache.
const REGEX_OVERHEAD: usize = 256;

/// Fuel charged before compiling a regex: a base cost plus a cost per
/// pattern byte. Compiling then costs one unit per `SLOW_BYTES_PER_UNIT`
/// bytes of compiled program (or of the size limit, when it is exceeded).
const COMPILE_BASE: u64 = 1000;
const COMPILE_PER_BYTE: u64 = 16;

/// The time floor of regex work: whatever the deterministic charges, regex
/// work is charged at least one unit per this many nanoseconds it takes.
///
/// A search's work is not always proportional to the text it returns: the
/// leftmost-first semantics can make a search look far past the match it
/// finds (`[a-z]*X|[a-z]` on a long run of letters scans to the end of the
/// text for every one-letter match, so iterating is quadratic), and a search
/// that falls back from the lazy DFA to the PikeVM costs time proportional
/// to the size of the program per byte. The regex engine does not report how
/// much it scanned, so such work is bounded by measuring it: ordinary
/// searches cost far less than this per unit of their deterministic charge
/// (about a nanosecond per byte), so for them fuel stays deterministic.
const FLOOR_NANOS: u128 = 1000;

/// Metering of regex work. A single search cannot be stopped, so it is
/// charged up front (failing fast when the budget cannot cover it);
/// iteration over matches is charged per match, plus the text searched, and
/// checks the limits between matches. Every charge is raised to the time
/// floor ([`FLOOR_NANOS`]) of the call so far. What has been collected is
/// checked against the memory limit.
struct Scan {
    searched: usize,
    charged: u64,
    start: Instant,
    scratch: Scratch,
}

impl Scan {
    fn new() -> Scan {
        Scan { searched: 0, charged: 0, start: Instant::now(), scratch: Scratch::new() }
    }

    /// Charges `units`, plus whatever the time floor requires.
    fn charge(&mut self, vm: &Vm, units: u64) -> Result<(), Ctl> {
        self.charged = self.charged.saturating_add(units);
        let floor = u64::try_from(self.start.elapsed().as_nanos() / FLOOR_NANOS).unwrap_or(u64::MAX);
        let extra = floor.saturating_sub(self.charged);
        self.charged = self.charged.saturating_add(extra);
        vm.charge(units.saturating_add(extra))
    }

    /// Charges for one search through `text`, before it runs.
    fn search(&mut self, vm: &Vm, text: &str) -> Result<(), Ctl> {
        self.charge(vm, (text.len() / SLOW_BYTES_PER_UNIT) as u64)
    }

    /// A match ending at `end` was found; `collected` bytes are held so far.
    fn step(&mut self, vm: &Vm, end: usize, collected: usize) -> Result<(), Ctl> {
        let searched = end.saturating_sub(self.searched);
        self.searched = self.searched.max(end);
        self.charge(vm, 1 + (searched / SLOW_BYTES_PER_UNIT) as u64)?;
        self.scratch.check(&vm.heap, collected)
    }

    /// Applies the time floor to the work done since the last charge.
    fn finish(&mut self, vm: &Vm) -> Result<(), Ctl> {
        self.charge(vm, 0)
    }
}

/// The opaque `regex.Regex` type.
struct RegexObj {
    re: meta::Regex,
    /// The search cache (lazy DFA states and engine scratch space). It grows
    /// during searches, up to a bound set by the size limits.
    cache: RefCell<Cache>,
    pattern: Box<str>,
    /// Runtime struct id of `regex.Match`.
    match_def: u32,
    /// The size last accounted for this object (see [`RegexObj::refresh_size`]).
    size: Cell<usize>,
}

impl RegexObj {
    fn new(re: meta::Regex, pattern: &str, match_def: u32) -> RegexObj {
        let cache = RefCell::new(re.create_cache());
        let obj = RegexObj { re, cache, pattern: pattern.into(), match_def, size: Cell::new(0) };
        obj.size.set(obj.measure());
        obj
    }

    /// The memory used by the compiled program and the cache.
    fn measure(&self) -> usize {
        let cache = self.cache.borrow().memory_usage();
        REGEX_OVERHEAD + self.pattern.len() + self.re.memory_usage() + cache
    }

    /// Re-measures the object after a search; returns the old and new sizes.
    fn refresh_size(&self) -> (usize, usize) {
        let new = self.measure();
        (self.size.replace(new), new)
    }
}

/// The opaque `regex.Captures` type: one match's groups, copied out of the text.
struct CapturesObj {
    group_info: GroupInfo,
    groups: Vec<Option<Group>>,
    match_def: u32,
}

struct Group {
    text: Box<str>,
    start: usize,
    end: usize,
}

impl NativeObject for RegexObj {
    fn type_name(&self) -> &'static str {
        "Regex"
    }
    fn size(&self) -> usize {
        self.size.get()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl NativeObject for CapturesObj {
    fn type_name(&self) -> &'static str {
        "Captures"
    }
    fn size(&self) -> usize {
        64 + self.groups.iter().flatten().map(|g| 32 + g.text.len()).sum::<usize>()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn regex_obj(vm: &Vm, v: Value) -> Result<&RegexObj, Ctl> {
    vm.heap.native::<RegexObj>(v).ok_or_else(|| Ctl::Panic("not a Regex".into()))
}

fn captures_obj(vm: &Vm, v: Value) -> Result<&CapturesObj, Ctl> {
    vm.heap.native::<CapturesObj>(v).ok_or_else(|| Ctl::Panic("not a Captures".into()))
}

/// Runs `search` with the regex `re` (a `Regex` value) and its cache, then
/// accounts for the cache's growth.
/// Slots of the per-engine cache of compiled regexes (direct-mapped by the
/// pattern's hash).
const REGEX_CACHE_SLOTS: usize = 64;

/// Fuel for a `regex.new` served from the cache (plus the pattern's bytes).
const CACHED_COST: u64 = 4;

/// The pattern's hash and its slot in the regex cache.
fn cache_slot(pattern: &str) -> (u64, usize) {
    use std::hash::{BuildHasher, BuildHasherDefault};
    let h = BuildHasherDefault::<crate::vm::value::FxHasher>::default().hash_one(pattern);
    (h, (h % REGEX_CACHE_SLOTS as u64) as usize)
}

/// A live regex compiled from `pattern`, if the cache knows one. The cache
/// holds handles, not roots: the object found at the handle must be a regex
/// with the same pattern (a freed or reused slot is simply a miss), and one
/// that a collection has already found unreachable is not used again.
fn cached_regex(vm: &Vm, pattern: Value, (hash, slot): (u64, usize)) -> Option<Value> {
    let &(h, handle) = vm.host.regex_cache.get(slot)?;
    if h != hash || handle == u32::MAX {
        return None;
    }
    let re = Value::Obj(handle);
    if vm.heap.condemned(re) {
        return None;
    }
    let obj = vm.heap.native::<RegexObj>(re)?;
    (*obj.pattern == *vm.str(pattern)).then_some(re)
}

fn with_regex<T>(vm: &mut Vm, re: Value, search: impl FnOnce(&Vm, &RegexObj, &mut Cache) -> Result<T, Ctl>) -> Result<T, Ctl> {
    let (result, (before, after)) = {
        let obj = regex_obj(vm, re)?;
        let result = search(vm, obj, &mut obj.cache.borrow_mut());
        (result, obj.refresh_size())
    };
    vm.heap.account(before, after);
    result
}

/// Calls `found` for every non-overlapping match in `text`, left to right
/// (as `regex::Regex::find_iter` does), charging `scan` for each step.
/// `found` returns the bytes collected so far.
fn each_match(
    vm: &Vm,
    obj: &RegexObj,
    cache: &mut Cache,
    text: &str,
    scan: &mut Scan,
    mut found: impl FnMut(Match) -> usize,
) -> Result<(), Ctl> {
    let mut it = Searcher::new(Input::new(text));
    let mut collected = 0;
    while let Some(m) = it.advance(|input| Ok(obj.re.search_with(cache, input))) {
        collected = found(m);
        scan.step(vm, m.end(), collected)?;
    }
    scan.step(vm, text.len(), collected)
}

/// Like [`each_match`] with the capture groups of each match (as
/// `regex::Regex::captures_iter` does); `found` may fail.
fn each_captures(
    vm: &Vm,
    obj: &RegexObj,
    cache: &mut Cache,
    text: &str,
    scan: &mut Scan,
    mut found: impl FnMut(&Captures) -> Result<usize, Ctl>,
) -> Result<(), Ctl> {
    let mut it = Searcher::new(Input::new(text));
    let mut caps = obj.re.create_captures();
    let mut collected = 0;
    while let Some(m) = it.advance(|input| {
        obj.re.search_captures_with(cache, input, &mut caps);
        Ok(caps.get_match())
    }) {
        collected = found(&caps)?;
        scan.step(vm, m.end(), collected)?;
    }
    scan.step(vm, text.len(), collected)
}

/// Allocates a `Match { text, start, end }`.
fn new_match(vm: &mut Vm, def: u32, text: &str, start: usize, end: usize) -> Value {
    let t = vm.new_str(text);
    vm.heap.alloc(Obj::Struct(def, Box::new([t, Value::Int(start as i64), Value::Int(end as i64)])))
}

/// Copies the groups of a match out of `text`, as an object of
/// `CapturesObj::size()` bytes. Nested groups can each hold most of the
/// match, so the copies can be many times the text's size: they are charged
/// and checked against the memory limit (with the `collected` bytes held
/// already) before they are made.
fn new_captures(vm: &Vm, obj: &RegexObj, text: &str, caps: &Captures, collected: usize) -> Result<CapturesObj, Ctl> {
    let spans = || (0..caps.group_len()).map(|i| caps.get_group(i));
    let copied: usize = spans().flatten().map(|s| s.len()).sum();
    vm.reserve(collected.saturating_add(64 + 32 * caps.group_len() + copied))?;
    vm.charge(caps.group_len() as u64)?;
    vm.charge_bytes(copied)?;
    let groups = spans().map(|s| s.map(|s| Group { text: text[s.start..s.end].into(), start: s.start, end: s.end })).collect();
    Ok(CapturesObj { group_info: obj.re.group_info().clone(), groups, match_def: obj.match_def })
}

/// The length of `rep` with the groups of `caps` interpolated (as
/// `Captures::interpolate_string_into` writes it), measured without copying
/// the groups. `literal` is scratch space for the replacement's own text.
fn expansion_len(caps: &Captures, rep: &str, literal: &mut String) -> usize {
    literal.clear();
    let mut groups = 0usize;
    interpolate::string(
        rep,
        |i, _| groups = groups.saturating_add(caps.get_group(i).map_or(0, |s| s.len())),
        |name| caps.group_info().to_index(caps.pattern()?, name),
        literal,
    );
    literal.len().saturating_add(groups)
}

/// Replaces the first (or every) match, checking the memory limit as the
/// output grows.
fn replace(vm: &mut Vm, a: &[Value], all: bool) -> NResult {
    let out = with_regex(vm, a[0], |vm, obj, cache| {
        let (text, rep) = (vm.str(a[1]), vm.str(a[2]));
        let mut scan = Scan::new();
        if !all {
            scan.search(vm, text)?;
        }
        // Capture groups are only needed if the replacement refers to them.
        let expand = rep.contains('$');
        let mut literal = String::new();
        let mut out = String::new();
        let mut last = 0;
        let mut it = Searcher::new(Input::new(text));
        let mut caps = obj.re.create_captures();
        while let Some(m) = it.advance(|input| {
            if !expand {
                return Ok(obj.re.search_with(cache, input));
            }
            obj.re.search_captures_with(cache, input, &mut caps);
            Ok(caps.get_match())
        }) {
            let added = if expand {
                scan.charge(vm, (rep.len() / SLOW_BYTES_PER_UNIT) as u64)?;
                expansion_len(&caps, rep, &mut literal)
            } else {
                rep.len()
            };
            vm.reserve(out.len().saturating_add(m.start() - last).saturating_add(added))?;
            out.push_str(&text[last..m.start()]);
            if expand {
                caps.interpolate_string_into(text, rep, &mut out);
            } else {
                out.push_str(rep);
            }
            last = m.end();
            scan.step(vm, last, out.len())?;
            if !all {
                break;
            }
        }
        scan.step(vm, text.len(), out.len())?;
        out.push_str(&text[last..]);
        Ok(out)
    })?;
    vm.reserve(out.len())?;
    vm.charge_bytes(out.len())?;
    Ok(vm.new_str(out))
}

/// A builder compiling patterns as `regex::Regex::new` does, with this
/// module's size limits.
fn builder() -> meta::Builder {
    let config = meta::Config::new()
        .match_kind(MatchKind::LeftmostFirst)
        .utf8_empty(true)
        .nfa_size_limit(Some(NFA_SIZE_LIMIT))
        .onepass_size_limit(Some(ONEPASS_SIZE_LIMIT))
        .hybrid_cache_capacity(DFA_CACHE_CAPACITY);
    let mut b = meta::Builder::new();
    b.configure(config).syntax(syntax::Config::new().utf8(true));
    b
}

pub fn register(r: &mut Registry) {
    r.source("regex", ModKind::Std, include_str!("regex.sand"));

    r.add_builtin("regex.new", |vm, a| {
        let slot = cache_slot(vm.str(a[0]));
        if let Some(re) = cached_regex(vm, a[0], slot) {
            vm.charge(CACHED_COST)?;
            vm.charge_bytes(vm.str(a[0]).len())?;
            return Ok(vm.ok(re));
        }
        let mut scan = Scan::new();
        scan.charge(vm, COMPILE_BASE + COMPILE_PER_BYTE * vm.str(a[0]).len() as u64)?;
        let built = builder().build(vm.str(a[0]));
        // Compiling does work in proportion to the program it builds.
        let compiled = built.as_ref().map_or_else(|e| e.size_limit().unwrap_or(0), |re| re.memory_usage());
        scan.charge(vm, (compiled / SLOW_BYTES_PER_UNIT) as u64)?;
        scan.finish(vm)?;
        let re = match built {
            Ok(re) => re,
            Err(e) => {
                return Ok(match (e.size_limit(), e.syntax_error()) {
                    (Some(limit), _) => vm.err_value("limit", &format!("regex is too large: the compiled program exceeds {limit} bytes")),
                    (None, Some(syntax)) => vm.err_value("parse", &syntax.to_string()),
                    (None, None) => vm.err_value("parse", &e.to_string()),
                });
            }
        };
        let Some(match_def) = vm.prog.types.iter().position(|t| t.name == "Match" && t.module == "regex") else {
            return panic("regex.Match is not defined");
        };
        let obj = RegexObj::new(re, vm.str(a[0]), match_def as u32);
        vm.reserve(obj.size())?;
        let obj = vm.heap.alloc(Obj::Native(Box::new(obj)));
        if let Some(h) = obj.handle() {
            let cache = &mut vm.host.regex_cache;
            if cache.is_empty() {
                cache.resize(REGEX_CACHE_SLOTS, (0, u32::MAX));
            }
            cache[slot.1] = (slot.0, h);
        }
        Ok(vm.ok(obj))
    });
    r.add_builtin("regex.Regex.is_match", |vm, a| {
        let found = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let mut scan = Scan::new();
            scan.search(vm, text)?;
            let found = obj.re.search_half_with(cache, &Input::new(text).earliest(true)).is_some();
            scan.finish(vm)?;
            Ok(found)
        })?;
        Ok(Value::Bool(found))
    });
    r.add_builtin("regex.Regex.find", |vm, a| {
        let found = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let mut scan = Scan::new();
            scan.search(vm, text)?;
            let m = obj.re.search_with(cache, &Input::new(text));
            scan.finish(vm)?;
            Ok(m.map(|m| (text[m.range()].to_string(), m.start(), m.end(), obj.match_def)))
        })?;
        let m = found.map(|(text, s, e, def)| new_match(vm, def, &text, s, e));
        Ok(vm.option(m))
    });
    r.add_builtin("regex.Regex.find_all", |vm, a| {
        let (def, spans, cost) = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let (mut spans, mut cost, mut scan) = (Vec::new(), 0, Scan::new());
            each_match(vm, obj, cache, text, &mut scan, |m| {
                spans.push((m.start(), m.end()));
                cost += MATCH_COST + m.len();
                cost
            })?;
            Ok((obj.match_def, spans, cost))
        })?;
        vm.reserve(cost + vm.str(a[1]).len())?;
        let text = vm.str(a[1]).to_string();
        let matches = spans.into_iter().map(|(s, e)| new_match(vm, def, &text[s..e], s, e)).collect();
        Ok(vm.heap.new_list(matches))
    });
    r.add_builtin("regex.Regex.captures", |vm, a| {
        let caps = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let mut scan = Scan::new();
            scan.search(vm, text)?;
            let mut caps = obj.re.create_captures();
            obj.re.search_captures_with(cache, &Input::new(text), &mut caps);
            scan.finish(vm)?;
            caps.is_match().then(|| new_captures(vm, obj, text, &caps, 0)).transpose()
        })?;
        let v = caps.map(|c| vm.heap.alloc(Obj::Native(Box::new(c))));
        Ok(vm.option(v))
    });
    r.add_builtin("regex.Regex.captures_all", |vm, a| {
        let (all, cost) = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let (mut all, mut cost, mut scan) = (Vec::new(), 0, Scan::new());
            each_captures(vm, obj, cache, text, &mut scan, |caps| {
                let caps = new_captures(vm, obj, text, caps, cost)?;
                cost += caps.size();
                all.push(caps);
                Ok(cost)
            })?;
            Ok((all, cost))
        })?;
        vm.reserve(cost)?;
        let items = all.into_iter().map(|c| vm.heap.alloc(Obj::Native(Box::new(c)))).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("regex.Regex.replace", |vm, a| replace(vm, a, false));
    r.add_builtin("regex.Regex.replace_all", |vm, a| replace(vm, a, true));
    r.add_builtin("regex.Regex.split", |vm, a| {
        let (spans, cost) = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let (mut spans, mut cost, mut scan) = (Vec::new(), 0, Scan::new());
            let mut last = 0;
            each_match(vm, obj, cache, text, &mut scan, |m| {
                spans.push((last, m.start()));
                cost += 64 + m.start() - last;
                last = m.end();
                cost
            })?;
            spans.push((last, text.len()));
            Ok((spans, cost))
        })?;
        vm.reserve(cost + vm.str(a[1]).len())?;
        let text = vm.str(a[1]).to_string();
        let items = spans.into_iter().map(|(s, e)| vm.new_str(&text[s..e])).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("regex.Regex.pattern", |vm, a| {
        let p = regex_obj(vm, a[0])?.pattern.to_string();
        Ok(vm.new_str(p))
    });

    r.add_builtin("regex.Captures.get", |vm, a| {
        let caps = captures_obj(vm, a[0])?;
        let group = usize::try_from(a[1].as_int()).ok().and_then(|i| caps.groups.get(i)).and_then(Option::as_ref);
        group_match(vm, caps.match_def, group.map(|g| (g.text.to_string(), g.start, g.end)))
    });
    r.add_builtin("regex.Captures.name", |vm, a| {
        let caps = captures_obj(vm, a[0])?;
        let index = caps.group_info.to_index(PatternID::ZERO, vm.str(a[1]));
        let group = index.and_then(|i| caps.groups.get(i)).and_then(Option::as_ref);
        group_match(vm, caps.match_def, group.map(|g| (g.text.to_string(), g.start, g.end)))
    });
    r.add_builtin("regex.Captures.len", |vm, a| Ok(Value::Int(captures_obj(vm, a[0])?.groups.len() as i64)));
    r.add_builtin("regex.Captures.text", |vm, a| {
        let caps = captures_obj(vm, a[0])?;
        let group = usize::try_from(a[1].as_int()).ok().and_then(|i| caps.groups.get(i)).and_then(Option::as_ref);
        let text = group.map(|g| g.text.to_string());
        let v = text.map(|t| vm.new_str(t));
        Ok(vm.option(v))
    });
}

/// `Some(Match)` for a participating group, else `None`.
fn group_match(vm: &mut Vm, def: u32, group: Option<(String, usize, usize)>) -> NResult {
    let m = group.map(|(text, s, e)| new_match(vm, def, &text, s, e));
    Ok(vm.option(m))
}
