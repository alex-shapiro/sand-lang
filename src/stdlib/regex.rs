//! The `regex` module, backed by the engine of the `regex` crate
//! (`regex_automata::meta`, configured exactly like `regex::Regex`).
//!
//! Each compiled regex owns its search cache, not the `regex` crate's
//! per-thread pool, so its memory can be counted against `max_memory`: the
//! object is re-measured after every search.

use crate::check::ModKind;
use crate::native::convert::HostError;
use crate::native::ty::Val;
use crate::native::{NativeModule, NativeObject};
use crate::platform::Instant;
use crate::vm::meter::{BYTES_PER_UNIT, SLOW_BYTES_PER_UNIT, Scratch};
use crate::vm::value::{FxHasher, VmValue};
use crate::vm::{Fail, Vm};
use regex_automata::meta::{self, Cache};
use regex_automata::util::captures::Captures as Groups;
use regex_automata::util::interpolate;
use regex_automata::util::iter::Searcher;
use regex_automata::util::syntax;
use regex_automata::{Input, MatchKind, PatternID};
use std::cell::{Cell, RefCell};
use std::hash::Hasher;

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

/// Nanoseconds per unit of the regex time floor.
///
/// The meter charges at least 1 unit per `FLOOR_NANOS` of elapsed time. This
/// floor bounds leftmost-first searches that scan past a match and PikeVM
/// fallback searches whose cost grows with the pattern size. Ordinary searches
/// take about 1 nanosecond per byte. The floor adds at most a few units to
/// those searches.
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
    fn charge(&mut self, vm: &Vm, units: u64) -> Result<(), Fail> {
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
    fn search(&mut self, vm: &Vm, text: &str) -> Result<(), Fail> {
        self.charge(vm, (text.len() / SLOW_BYTES_PER_UNIT) as u64)
    }

    /// A match ending at `end` was found; `collected` bytes are held so far.
    fn step(&mut self, vm: &Vm, end: usize, collected: usize) -> Result<(), Fail> {
        let searched = end.saturating_sub(self.searched);
        self.searched = self.searched.max(end);
        self.charge(vm, 1 + (searched / SLOW_BYTES_PER_UNIT) as u64)?;
        self.scratch.check(&vm.heap, collected)
    }

    /// Applies the time floor to the work done since the last charge.
    fn finish(&mut self, vm: &Vm) -> Result<(), Fail> {
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

/// Number of slots in the direct-mapped regex cache.
const REGEX_CACHE_SLOTS: usize = 64;

/// Base fuel cost for a cache hit in `regex.Regex` before the pattern-byte
/// charge.
const CACHED_COST: u64 = 4;

/// A slot in the per-engine regex cache.
///
/// A full collection clears `regex` and keeps `pattern_hash`. A later compile
/// of the same pattern still charges the cache-hit cost.
pub(crate) struct RegexCacheEntry {
    pattern_hash: u64,
    /// The compiled `RegexObj`, which is a GC root while present.
    pub(crate) regex: Option<VmValue>,
}

/// Computes the unseeded hash of `pattern` for the regex cache.
///
/// An unseeded hash maps a pattern to the same cache slot in every process.
fn pattern_hash(pattern: &str) -> u64 {
    let mut hasher = FxHasher::unseeded();
    hasher.write(pattern.as_bytes());
    hasher.write_usize(pattern.len());
    hasher.finish()
}

fn cache_slot(pattern_hash: u64) -> usize {
    (pattern_hash % REGEX_CACHE_SLOTS as u64) as usize
}

/// Lookup result in the regex cache.
enum CachedRegex {
    Absent,
    /// The pattern was cached before a full collection cleared its compiled object.
    Released,
    Live(Val<RegexObj>),
}

fn lookup_cached_regex(vm: &Vm, pattern: &str, pattern_hash: u64) -> CachedRegex {
    let Some(Some(entry)) = vm.host.regex_cache.get(cache_slot(pattern_hash)) else { return CachedRegex::Absent };
    if entry.pattern_hash != pattern_hash {
        return CachedRegex::Absent;
    }
    let Some(regex) = entry.regex else { return CachedRegex::Released };
    match vm.heap.native::<RegexObj>(regex) {
        Some(obj) if *obj.pattern == *pattern => CachedRegex::Live(Val::from_raw(regex)),
        _ => CachedRegex::Absent,
    }
}

/// Runs `search` with the regex `re` and its cache, then accounts for the
/// cache's growth.
fn with_regex<T>(vm: &mut Vm, re: Val<RegexObj>, search: impl FnOnce(&Vm, &RegexObj, &mut Cache) -> Result<T, Fail>) -> Result<T, Fail> {
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
) -> Result<(), Fail> {
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
    mut found: impl FnMut(&Groups) -> Result<usize, Fail>,
) -> Result<(), Fail> {
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
const SPAN_COST: usize = 2 * std::mem::size_of::<VmValue>();

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
fn replace(vm: &mut Vm, re: Val<RegexObj>, text: Val<String>, rep: Val<String>, all: bool) -> Result<String, Fail> {
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
        let text_len = pattern.str(&vm.heap).len();
        let hash = pattern_hash(pattern.str(&vm.heap));
        let cached = lookup_cached_regex(vm, pattern.str(&vm.heap), hash);
        let is_new_pattern = matches!(cached, CachedRegex::Absent);
        let cached_cost = CACHED_COST + (text_len / BYTES_PER_UNIT) as u64;
        let mut scan = match cached {
            CachedRegex::Live(regex) => {
                vm.charge(cached_cost)?;
                return Ok(regex);
            }
            CachedRegex::Released => {
                let mut scan = Scan::new(vm);
                scan.charge(vm, cached_cost)?;
                scan
            }
            CachedRegex::Absent => {
                let mut scan = Scan::new(vm);
                scan.charge(vm, COMPILE_BASE + COMPILE_PER_BYTE * text_len as u64)?;
                scan
            }
        };
        let built = builder().build(pattern.str(&vm.heap));
        if is_new_pattern {
            let compiled_bytes = built.as_ref().map_or_else(|e| e.size_limit().unwrap_or(0), |re| re.memory_usage());
            scan.charge(vm, (compiled_bytes / SLOW_BYTES_PER_UNIT) as u64)?;
        }
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
        let cache = &mut vm.host.regex_cache;
        cache.resize_with(REGEX_CACHE_SLOTS, || None);
        cache[cache_slot(hash)] = Some(RegexCacheEntry { pattern_hash: hash, regex: Some(obj.raw()) });
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
    m.vm_function("Regex.find", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Option<Match>, Fail> {
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
    m.vm_function("Regex.find_all", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Vec<Match>, Fail> {
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
    m.vm_function("Regex.captures", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Option<Captures>, Fail> {
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
    m.vm_function("Regex.captures_all", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Vec<Captures>, Fail> {
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
    m.vm_function("Regex.group_index", |vm: &mut Vm, re: Val<RegexObj>, name: Val<String>| -> Result<Option<i64>, Fail> {
        let index = re.get(&vm.heap)?.re.group_info().to_index(PatternID::ZERO, name.str(&vm.heap));
        Ok(index.map(|i| i as i64))
    });
    m.vm_function("Regex.replace", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>, rep: Val<String>| replace(vm, re, text, rep, false));
    m.vm_function("Regex.replace_all", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>, rep: Val<String>| {
        replace(vm, re, text, rep, true)
    });
    m.vm_function("Regex.split", |vm: &mut Vm, re: Val<RegexObj>, text: Val<String>| -> Result<Val<Vec<String>>, Fail> {
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

#[cfg(test)]
mod tests {
    use super::pattern_hash;

    #[test]
    fn pattern_hash_is_fixed_across_processes() {
        assert_eq!([pattern_hash("a+b"), pattern_hash("")], [17277465237014725391, 0]);
    }
}
