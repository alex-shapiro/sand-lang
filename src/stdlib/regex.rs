//! The `regex` module, backed by the engine of the `regex` crate
//! (`regex_automata::meta`, configured exactly like `regex::Regex`).
//!
//! Each compiled regex owns its search cache, not the `regex` crate's
//! per-thread pool, so its memory can be counted against `max_memory`: the
//! object is re-measured after every search.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::platform::Instant;
use crate::vm::meter::{SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{Ctl, NResult, Vm, internal};
use regex_automata::meta::{self, Cache};
use regex_automata::util::captures::Captures;
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
/// Work is not always proportional to the text: a search may look far past
/// its match (`[a-z]*X|[a-z]` on a long run of letters scans to the end for
/// every one-letter match), and the PikeVM fallback costs time per byte in
/// the program's size. The engine does not report how much it scanned, so
/// such work is timed. Ordinary searches take about a nanosecond per byte,
/// far below the floor, so their fuel stays deterministic.
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
    /// Charges since the clock was last read.
    unclocked: u32,
    scratch: Scratch,
}

/// The clock is read every this many charges (and for every large charge):
/// reading it costs more than a short step of an iteration over matches.
const CLOCK_EVERY: u32 = 32;

impl Scan {
    fn new(vm: &Vm) -> Scan {
        Scan { searched: 0, charged: 0, start: vm.host.clock.now(), unclocked: 0, scratch: Scratch::new() }
    }

    /// Charges `units`, plus whatever the time floor requires.
    fn charge(&mut self, vm: &Vm, units: u64) -> Result<(), Ctl> {
        self.charged = self.charged.saturating_add(units);
        self.unclocked += 1;
        let mut extra = 0;
        if self.unclocked >= CLOCK_EVERY || units >= u64::from(CLOCK_EVERY) {
            self.unclocked = 0;
            let elapsed = vm.host.clock.now().saturating_duration_since(self.start);
            let floor = u64::try_from(elapsed.as_nanos() / FLOOR_NANOS).unwrap_or(u64::MAX);
            extra = floor.saturating_sub(self.charged);
            self.charged = self.charged.saturating_add(extra);
        }
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
        self.unclocked = CLOCK_EVERY;
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

fn regex_obj(vm: &Vm, v: Value) -> Result<&RegexObj, Ctl> {
    vm.heap.native::<RegexObj>(v).ok_or_else(|| Ctl::internal("not a Regex"))
}

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

/// Runs `search` with the regex `re` (a `Regex` value) and its cache, then
/// accounts for the cache's growth.
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

/// A `Match { text, start, end }` of the bytes `start..end` of string `text`.
fn new_match(vm: &mut Vm, def: u32, text: Value, start: usize, end: usize) -> Value {
    let t = super::core::new_substr(vm, text, start..end);
    vm.heap.alloc(Obj::Struct(def, Box::new([t, Value::Int(start as i64), Value::Int(end as i64)])))
}

/// Bytes counted per group of a match's spans (two list elements).
const SPAN_COST: usize = 2 * std::mem::size_of::<Value>();

/// The spans of the groups of a match, `start, end` per group (`-1, -1` for
/// a group that did not participate), as `regex.Captures` holds them.
fn spans(caps: &Captures) -> Vec<Value> {
    let mut out = Vec::with_capacity(2 * caps.group_len());
    for i in 0..caps.group_len() {
        let (s, e) = caps.get_group(i).map_or((-1, -1), |s| (s.start as i64, s.end as i64));
        out.extend([Value::Int(s), Value::Int(e)]);
    }
    out
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
        let mut scan = Scan::new(vm);
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
        let mut scan = Scan::new(vm);
        scan.charge(vm, COMPILE_BASE + COMPILE_PER_BYTE * vm.str(a[0]).len() as u64)?;
        let built = builder().build(vm.str(a[0]));
        // Compiling does work in proportion to the program it builds.
        let compiled = built.as_ref().map_or_else(|e| e.size_limit().unwrap_or(0), |re| re.memory_usage());
        scan.charge(vm, (compiled / SLOW_BYTES_PER_UNIT) as u64)?;
        scan.finish(vm)?;
        let re = match built {
            Ok(re) => re,
            Err(e) => {
                return Err(match (e.size_limit(), e.syntax_error()) {
                    (Some(limit), _) => Ctl::native_err("limit", format!("regex is too large: the compiled program exceeds {limit} bytes")),
                    (None, Some(syntax)) => Ctl::native_err("parse", syntax.to_string()),
                    (None, None) => Ctl::native_err("parse", e.to_string()),
                });
            }
        };
        let Some(match_def) = vm.prog.types.iter().position(|t| t.name == "Match" && t.module == "regex") else {
            return internal("regex.Match is not defined");
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
            let mut scan = Scan::new(vm);
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
            let mut scan = Scan::new(vm);
            scan.search(vm, text)?;
            let m = obj.re.search_with(cache, &Input::new(text));
            scan.finish(vm)?;
            vm.reserve(m.map_or(0, |m| m.len() + MATCH_COST))?;
            Ok(m.map(|m| (m.start(), m.end(), obj.match_def)))
        })?;
        let m = found.map(|(s, e, def)| new_match(vm, def, a[1], s, e));
        Ok(vm.option(m))
    });
    r.add_builtin("regex.Regex.find_all", |vm, a| {
        let (def, spans, cost) = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let (mut spans, mut cost, mut scan) = (Vec::new(), 0, Scan::new(vm));
            each_match(vm, obj, cache, text, &mut scan, |m| {
                spans.push((m.start(), m.end()));
                cost += MATCH_COST + m.len();
                cost
            })?;
            Ok((obj.match_def, spans, cost))
        })?;
        vm.reserve(cost)?;
        Ok(vm.heap.staged(|heap, st| {
            let text = heap.str(a[1]);
            let matches = spans
                .into_iter()
                .map(|(s, e)| {
                    let t = st.new_str(&text[s..e]);
                    st.alloc(Obj::Struct(def, Box::new([t, Value::Int(s as i64), Value::Int(e as i64)])))
                })
                .collect();
            st.new_list(matches)
        }))
    });
    r.add_builtin("regex.Regex.capture_spans", |vm, a| {
        let spans = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let mut scan = Scan::new(vm);
            scan.search(vm, text)?;
            let mut caps = obj.re.create_captures();
            obj.re.search_captures_with(cache, &Input::new(text), &mut caps);
            scan.finish(vm)?;
            vm.reserve(SPAN_COST * caps.group_len())?;
            vm.charge(caps.group_len() as u64)?;
            Ok(caps.is_match().then(|| spans(&caps)))
        })?;
        let v = spans.map(|s| vm.heap.new_list(s));
        Ok(vm.option(v))
    });
    r.add_builtin("regex.Regex.capture_spans_all", |vm, a| {
        let all = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let (mut all, mut cost, mut scan) = (Vec::new(), 0, Scan::new(vm));
            each_captures(vm, obj, cache, text, &mut scan, |caps| {
                vm.charge(caps.group_len() as u64)?;
                cost += 64 + SPAN_COST * caps.group_len();
                all.push(spans(caps));
                Ok(cost)
            })?;
            vm.reserve(cost)?;
            Ok(all)
        })?;
        let items = all.into_iter().map(|s| vm.heap.new_list(s)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("regex.Regex.group_index", |vm, a| {
        let index = regex_obj(vm, a[0])?.re.group_info().to_index(PatternID::ZERO, vm.str(a[1]));
        Ok(vm.option(index.map(|i| Value::Int(i as i64))))
    });
    r.add_builtin("regex.Regex.replace", |vm, a| replace(vm, a, false));
    r.add_builtin("regex.Regex.replace_all", |vm, a| replace(vm, a, true));
    r.add_builtin("regex.Regex.split", |vm, a| {
        let (spans, cost) = with_regex(vm, a[0], |vm, obj, cache| {
            let text = vm.str(a[1]);
            let (mut spans, mut cost, mut scan) = (Vec::new(), 0, Scan::new(vm));
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
        vm.reserve(cost)?;
        Ok(vm.heap.staged(|heap, st| {
            let text = heap.str(a[1]);
            let items = spans.into_iter().map(|(s, e)| st.new_str(&text[s..e])).collect();
            st.new_list(items)
        }))
    });
    r.add_builtin("regex.Regex.pattern", |vm, a| {
        let p = regex_obj(vm, a[0])?.pattern.to_string();
        Ok(vm.new_str(p))
    });
}
