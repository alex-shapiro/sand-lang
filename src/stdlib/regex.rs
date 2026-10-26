//! The `regex` module, backed by the engine of the `regex` crate
//! (`regex_automata::meta`, configured exactly like `regex::Regex`).
//!
//! Each compiled regex owns its search cache, not the `regex` crate's
//! per-thread pool, so its memory can be counted against `max_memory`: the
//! object is re-measured after every search.

use crate::check::ModKind;
use crate::native::{HostError, NativeModule, NativeObject, Val};
use crate::platform::Instant;
use crate::vm::meter::{SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::Value;
use crate::vm::{Ctl, Vm};
use regex_automata::meta::{self, Cache};
use regex_automata::util::captures::Captures as Groups;
use regex_automata::util::interpolate;
use regex_automata::util::iter::Searcher;
use regex_automata::util::syntax;
use regex_automata::{Input, MatchKind, PatternID};
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

/// A compiled regular expression.
#[derive(crate::SandObject)]
#[sand(module = "regex", name = "Regex")]
struct RegexObj {
    re: meta::Regex,
    /// The search cache (lazy DFA states and engine scratch space). It grows
    /// during searches, up to a bound set by the size limits.
    cache: RefCell<Cache>,
    pattern: Box<str>,
    /// The size last accounted for this object (see [`RegexObj::refresh_size`]).
    size: Cell<usize>,
}

impl RegexObj {
    fn new(re: meta::Regex, pattern: &str) -> RegexObj {
        let cache = RefCell::new(re.create_cache());
        let obj = RegexObj { re, cache, pattern: pattern.into(), size: Cell::new(0) };
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
    fn size(&self) -> usize {
        self.size.get()
    }
}

/// Slots of the per-engine cache of compiled regexes (direct-mapped by the
/// pattern's hash).
const REGEX_CACHE_SLOTS: usize = 64;

/// Fuel for a `regex.Regex(pattern)` served from the cache (plus the pattern's bytes).
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
fn cached_regex(vm: &Vm, pattern: &str, (hash, slot): (u64, usize)) -> Option<Val<RegexObj>> {
    let &(h, handle) = vm.host.regex_cache.get(slot)?;
    if h != hash || handle == u32::MAX {
        return None;
    }
    let re = Value::Obj(handle);
    if vm.heap.condemned(re) {
        return None;
    }
    let obj = vm.heap.native::<RegexObj>(re)?;
    (*obj.pattern == *pattern).then_some(Val::from_raw(re))
}

/// Runs `search` with the regex `re` and its cache, then accounts for the
/// cache's growth.
fn with_regex<T>(vm: &mut Vm, re: Val<RegexObj>, search: impl FnOnce(&Vm, &RegexObj, &mut Cache) -> Result<T, Ctl>) -> Result<T, Ctl> {
    let (result, (before, after)) = {
        let obj = re.get(&vm.heap)?;
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
    mut found: impl FnMut(regex_automata::Match) -> usize,
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
    mut found: impl FnMut(&Groups) -> Result<usize, Ctl>,
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

/// A substring match and its UTF-8 byte range `start..end`.
#[derive(crate::SandStruct)]
#[sand(module = "regex")]
struct Match {
    /// Matched substring.
    text: Val<String>,
    /// Inclusive start byte offset.
    start: i64,
    /// Exclusive end byte offset.
    end: i64,
}

impl Match {
    fn new(vm: &mut Vm, text: Val<String>, span: std::ops::Range<usize>) -> Match {
        let (start, end) = (span.start as i64, span.end as i64);
        Match { text: Val::from_raw(super::core::new_substr(vm, text.raw(), span)), start, end }
    }
}

/// Capture groups from a regular expression match. Group `0` corresponds to
/// the full match.
#[derive(crate::SandStruct)]
#[sand(module = "regex")]
struct Captures {
    /// Input string that was searched.
    text: Val<String>,
    /// Start and end byte offsets per group (`-1, -1` if unmatched).
    #[sand(private)]
    spans: Vec<i64>,
    #[sand(private)]
    regex: Val<RegexObj>,
}

/// Bytes counted per group of a match's spans (two list elements).
const SPAN_COST: usize = 2 * std::mem::size_of::<Value>();

/// Approximate heap cost of a `Captures` value excluding `spans`.
const CAPTURES_COST: usize = 64;

fn spans(caps: &Groups) -> Vec<i64> {
    let mut out = Vec::with_capacity(2 * caps.group_len());
    for i in 0..caps.group_len() {
        let (s, e) = caps.get_group(i).map_or((-1, -1), |s| (s.start as i64, s.end as i64));
        out.extend([s, e]);
    }
    out
}

/// The length of `rep` with the groups of `caps` interpolated (as
/// `Captures::interpolate_string_into` writes it), measured without copying
/// the groups. `literal` is scratch space for the replacement's own text.
fn expansion_len(caps: &Groups, rep: &str, literal: &mut String) -> usize {
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
fn replace(vm: &mut Vm, re: Val<RegexObj>, text: Val<String>, rep: Val<String>, all: bool) -> Result<String, Ctl> {
    with_regex(vm, re, |vm, obj, cache| {
        let (text, rep) = (text.str(&vm.heap), rep.str(&vm.heap));
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
            vm.check_memory(out.len().saturating_add(m.start() - last).saturating_add(added))?;
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
    })
}

fn syntax_message(pattern: &str, error: &str) -> String {
    if pattern.len() <= 200 {
        return error.to_string();
    }
    let what = error.lines().rev().find_map(|l| l.strip_prefix("error: ")).unwrap_or("invalid syntax");
    format!("invalid regex {:?}: {}", super::truncate(pattern, 64), super::truncate(what, 256))
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

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("regex", ModKind::Std, include_str!("regex.sand"));
    m.bind::<RegexObj>().bind::<Match>().bind::<Captures>();

    m.vm_function("Regex.init", |vm: &mut Vm, pattern: Val<String>| -> Result<Val<RegexObj>, HostError> {
        let slot = cache_slot(pattern.str(&vm.heap));
        if let Some(re) = cached_regex(vm, pattern.str(&vm.heap), slot) {
            vm.charge(CACHED_COST)?;
            vm.charge_bytes(pattern.str(&vm.heap).len())?;
            return Ok(re);
        }
        let mut scan = Scan::new(vm);
        scan.charge(vm, COMPILE_BASE + COMPILE_PER_BYTE * pattern.str(&vm.heap).len() as u64)?;
        let built = builder().build(pattern.str(&vm.heap));
        // Compiling does work in proportion to the program it builds.
        let compiled = built.as_ref().map_or_else(|e| e.size_limit().unwrap_or(0), |re| re.memory_usage());
        scan.charge(vm, (compiled / SLOW_BYTES_PER_UNIT) as u64)?;
        scan.finish(vm)?;
        let re = match built {
            Ok(re) => re,
            Err(e) => {
                return Err(match (e.size_limit(), e.syntax_error()) {
                    (Some(limit), _) => HostError::new("limit", format!("regex is too large: the compiled program exceeds {limit} bytes")),
                    (None, Some(syntax)) => HostError::new("parse", syntax_message(pattern.str(&vm.heap), &syntax.to_string())),
                    (None, None) => HostError::new("parse", e.to_string()),
                });
            }
        };
        let obj = vm.new_object(RegexObj::new(re, pattern.str(&vm.heap)))?;
        if let Some(h) = obj.raw().handle() {
            let cache = &mut vm.host.regex_cache;
            if cache.is_empty() {
                cache.resize(REGEX_CACHE_SLOTS, (0, u32::MAX));
            }
            cache[slot.1] = (slot.0, h);
        }
        Ok(obj)
    });
    m.vm_function("Regex.is_match", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| {
        with_regex(vm, re, |vm, obj, cache| {
            let text = text.str(&vm.heap);
            let mut scan = Scan::new(vm);
            scan.search(vm, text)?;
            let found = obj.re.search_half_with(cache, &Input::new(text).earliest(true)).is_some();
            scan.finish(vm)?;
            Ok(found)
        })
    });
    m.vm_function("Regex.find", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Option<Match>, Ctl> {
        let found = with_regex(vm, re, |vm, obj, cache| {
            let t = text.str(&vm.heap);
            let mut scan = Scan::new(vm);
            scan.search(vm, t)?;
            let m = obj.re.search_with(cache, &Input::new(t));
            scan.finish(vm)?;
            vm.check_memory(m.map_or(0, |m| m.len() + MATCH_COST))?;
            Ok(m.map(|m| m.range()))
        })?;
        Ok(found.map(|span| Match::new(vm, text, span)))
    });
    m.vm_function("Regex.find_all", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Vec<Match>, Ctl> {
        let spans = with_regex(vm, re, |vm, obj, cache| {
            let t = text.str(&vm.heap);
            let (mut spans, mut cost, mut scan) = (Vec::new(), 0, Scan::new(vm));
            each_match(vm, obj, cache, t, &mut scan, |m| {
                spans.push(m.range());
                cost += MATCH_COST + m.len();
                cost
            })?;
            vm.check_memory(cost)?;
            Ok(spans)
        })?;
        Ok(spans.into_iter().map(|span| Match::new(vm, text, span)).collect())
    });
    m.vm_function("Regex.captures", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Option<Captures>, Ctl> {
        with_regex(vm, re, |vm, obj, cache| {
            let t = text.str(&vm.heap);
            let mut scan = Scan::new(vm);
            scan.search(vm, t)?;
            let mut caps = obj.re.create_captures();
            obj.re.search_captures_with(cache, &Input::new(t), &mut caps);
            scan.finish(vm)?;
            vm.check_memory(CAPTURES_COST + SPAN_COST * caps.group_len())?;
            Ok(caps.is_match().then(|| Captures { text, spans: spans(&caps), regex: re }))
        })
    });
    m.vm_function("Regex.captures_all", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Vec<Captures>, Ctl> {
        with_regex(vm, re, |vm, obj, cache| {
            let t = text.str(&vm.heap);
            let (mut all, mut cost, mut scan) = (Vec::new(), 0, Scan::new(vm));
            each_captures(vm, obj, cache, t, &mut scan, |caps| {
                cost += CAPTURES_COST + SPAN_COST * caps.group_len();
                all.push(Captures { text, spans: spans(caps), regex: re });
                Ok(cost)
            })?;
            vm.check_memory(cost)?;
            Ok(all)
        })
    });
    m.vm_function("Regex.group_index", |vm: &mut Vm, re: Val<RegexObj>, name: Val<String>| -> Result<Option<i64>, Ctl> {
        let index = re.get(&vm.heap)?.re.group_info().to_index(PatternID::ZERO, name.str(&vm.heap));
        Ok(index.map(|i| i as i64))
    });
    m.vm_function("Regex.replace", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>, rep: Val<String>| replace(vm, re, text, rep, false));
    m.vm_function("Regex.replace_all", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>, rep: Val<String>| {
        replace(vm, re, text, rep, true)
    });
    m.vm_function("Regex.split", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Val<Vec<String>>, Ctl> {
        let (spans, cost) = with_regex(vm, re, |vm, obj, cache| {
            let text = text.str(&vm.heap);
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
        vm.check_memory(cost)?;
        Ok(Val::from_raw(vm.heap.staged(|heap, st| {
            let text = text.str(heap);
            let items = spans.into_iter().map(|(s, e)| st.new_str(&text[s..e])).collect();
            st.new_list(items)
        })))
    });
    m.function("Regex.pattern", |re: &RegexObj| re.pattern.to_string());
    m
}
