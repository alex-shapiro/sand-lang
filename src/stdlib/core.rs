//! Natives for the prelude and the `task` module.

use crate::check::ModKind;
use crate::native::bound::{Eq as EqB, Hash as HashB, Ord as OrdB};
use crate::native::{self, Channel, HostError, Later, NativeModule, Never, Param, Task, Val};
use crate::vm::compare::{
    find_key, hash_key, set_slot, try_sort_by, try_sort_numbers, try_sort_packed, values_cmp_metered, values_eq_metered,
};
use crate::vm::heap::Heap;
use crate::vm::value::*;
use crate::vm::{BlockOn, Ctl, NResult, Vm, fail, internal};
use jiff::SignedDuration;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Approximate heap cost of a small object (a string, tuple or list header)
/// plus the list slot holding it, for reserving memory before building many.
const ITEM: usize = std::mem::size_of::<Obj>() + 2 * std::mem::size_of::<Value>();

/// Approximate heap cost of a list element.
const SLOT: usize = std::mem::size_of::<Value>();

type Str = Val<String>;
type Bytes = Val<Vec<u8>>;
type List<T = Param<0>> = Val<Vec<T>>;
type Map = Val<HashMap<Param<0, HashB>, Param<1>>>;
type Set = Val<HashSet<Param<0, HashB>>>;
type Range = Val<native::Range>;
type IterFn<T> = Val<fn() -> Option<T>>;

/// Charges for copying `n` bytes into a new value and checks the memory limit.
fn copy_bytes(vm: &Vm, n: usize) -> Result<(), Ctl> {
    vm.check_room(n)?;
    vm.charge_bytes(n)
}

fn reserve_list(vm: &Vm, n: usize) -> Result<(), Ctl> {
    vm.check_room(n.saturating_mul(SLOT))
}

fn params<const N: u8>(items: Vec<Value>) -> Vec<Param<N>> {
    items.into_iter().map(Param::from_raw).collect()
}

/// Charges for producing `n` new list elements (plus `objs` new objects) and
/// checks the memory limit.
fn produce(vm: &Vm, n: usize, objs: usize) -> Result<(), Ctl> {
    vm.check_room(n.saturating_mul(SLOT).saturating_add(objs.saturating_mul(ITEM)))?;
    vm.charge(n.saturating_add(objs) as u64)
}

fn nanos(d: SignedDuration) -> i64 {
    d.as_nanos() as i64
}

pub(crate) fn prelude() -> NativeModule {
    let mut m = NativeModule::std("prelude", ModKind::Prelude, include_str!("prelude.sand"));
    m.builtin_type("Task", "Handle to a concurrent task started with `spawn`.", &["T"]);
    m.builtin_type("Channel", "FIFO message queue for communicating between tasks (created with `task.channel`).", &["T"]);

    // Globals.
    m.vm_function("print", |vm: &mut Vm, x: Param<0>| {
        let mut s = vm.display(x.raw())?;
        s.push('\n');
        vm.write_stream(1, s.as_bytes())
    });
    m.vm_function("eprint", |vm: &mut Vm, x: Param<0>| {
        let mut s = vm.display(x.raw())?;
        s.push('\n');
        vm.write_stream(2, s.as_bytes())
    });
    m.function("fail", |message: &str, kind: &str| -> Result<Never, Ctl> { fail(kind, message) });
    m.function("exit", |code: i64| -> Result<Never, Ctl> { Err(Ctl::exit(code.clamp(i32::MIN as i64, i32::MAX as i64) as i32)) });
    m.vm_function("set", |vm: &mut Vm, items: List<Param<0, HashB>>| -> Result<_, Ctl> {
        let set = set_of(vm, items.items(&vm.heap))?;
        Ok(new_set(vm, set))
    });

    // Tasks and channels.
    m.vm_function("Task.wait", |vm: &mut Vm, t: Val<Task<Param<0>>>| vm.block(BlockOn::Join(t.raw())).map(Param::<0>::from_raw));
    m.vm_function("Task.is_done", |vm: &mut Vm, t: Val<Task<Param<0>>>| -> Result<bool, Ctl> {
        Ok(t.task(&vm.heap)?.state != TaskState::Running)
    });
    m.vm_function("Task.cancel", |vm: &mut Vm, t: Val<Task<Param<0>>>| -> Result<_, Ctl> {
        vm.cancel_task(t.raw());
        Ok(())
    });
    m.vm_function("Channel.send", |vm: &mut Vm, c: Val<Channel<Param<0>>>, x: Param<0>| {
        vm.block(BlockOn::Send(c.raw(), x.raw())).map(Val::<bool>::from_raw)
    });
    m.vm_function("Channel.recv", |vm: &mut Vm, c: Val<Channel<Param<0>>>| {
        vm.block(BlockOn::Recv(c.raw())).map(Val::<Option<Param<0>>>::from_raw)
    });
    m.vm_function("Channel.try_recv", |vm: &mut Vm, c: Val<Channel<Param<0>>>| -> Result<Option<Param<0>>, Ctl> {
        let Some((v, woken)) = c.channel_mut(&mut vm.heap)?.take() else { return Ok(None) };
        if let Some((sf, sg)) = woken {
            vm.deliver(sf, sg, Value::Bool(true));
        }
        Ok(Some(Param::from_raw(v)))
    });
    m.vm_function("Channel.close", |vm: &mut Vm, c: Val<Channel<Param<0>>>| -> Result<(), Ctl> {
        let c = c.channel_mut(&mut vm.heap)?;
        c.closed = true;
        let receivers: Vec<(u32, u64)> = c.receivers.drain(..).collect();
        let senders: Vec<(u32, u64, Value)> = c.senders.drain(..).collect();
        for (f, g) in receivers {
            let none = vm.none();
            vm.deliver(f, g, none);
        }
        for (f, g, _) in senders {
            vm.deliver(f, g, Value::Bool(false));
        }
        Ok(())
    });
    m.vm_function("Channel.len", |vm: &mut Vm, c: Val<Channel<Param<0>>>| -> Result<i64, Ctl> { Ok(c.channel(&vm.heap)?.len() as i64) });
    m.vm_function("Channel.is_closed", |vm: &mut Vm, c: Val<Channel<Param<0>>>| -> Result<bool, Ctl> { Ok(c.channel(&vm.heap)?.closed) });

    numbers(&mut m);
    strings(&mut m);
    bytes(&mut m);
    lists(&mut m);
    ranges(&mut m);
    maps(&mut m);
    sets(&mut m);
    m
}

pub(crate) fn task() -> NativeModule {
    let mut m = NativeModule::std("task", ModKind::Std, include_str!("task.sand"));
    m.vm_function("sleep", |vm: &mut Vm, d: SignedDuration| {
        let d = nanos(d).max(0) as u64;
        let sleep = vm.host.clock.sleep(std::time::Duration::from_nanos(d));
        Ok::<_, Ctl>(Later::new(async move {
            sleep.await;
            |_: &mut Vm| Ok::<_, Ctl>(())
        }))
    });
    m.vm_function("yield_now", |vm: &mut Vm| Err::<(), _>(vm.yield_now()));
    // `BlockOn::Race` charges for the tasks and checks the memory limit.
    m.vm_function("race", |vm: &mut Vm, tasks: List<Task<Param<0>>>| {
        let tasks = tasks.items(&vm.heap);
        vm.check_room(tasks.len().saturating_mul(SLOT))?;
        let tasks = tasks.to_vec();
        vm.block(BlockOn::Race(tasks)).map(Param::<0>::from_raw)
    });
    m.vm_function("channel", |vm: &mut Vm, cap: i64| -> Result<Val<Channel<Param<0>>>, Ctl> {
        if cap < 0 {
            return fail("invalid", "channel capacity must not be negative");
        }
        Ok(Val::from_raw(vm.heap.alloc(Obj::Channel(Box::new(ChannelObj::new(to_usize_saturating(cap)))))))
    });
    m
}

fn numbers(m: &mut NativeModule) {
    m.function("int.to_float", |i: i64| i as f64);
    m.function("int.to_str_radix", |v: i64, radix: i64| -> Result<String, Ctl> {
        if !(2..=36).contains(&radix) {
            return fail("invalid", format!("radix must be between 2 and 36, found {radix}"));
        }
        let mut mag = v.unsigned_abs();
        let mut digits = Vec::new();
        loop {
            digits.push(std::char::from_digit((mag % radix as u64) as u32, radix as u32).unwrap());
            mag /= radix as u64;
            if mag == 0 {
                break;
            }
        }
        if v < 0 {
            digits.push('-');
        }
        Ok(digits.into_iter().rev().collect())
    });
    m.vm_function("int.to_char", |vm: &mut Vm, i: i64| -> Result<Option<Str>, Ctl> {
        let c = u32::try_from(i).ok().and_then(char::from_u32);
        Ok(c.map(|c| Val::from_raw(vm.short_str(c.encode_utf8(&mut [0; 4])))))
    });
    m.function("float.abs", f64::abs);
    m.function("float.floor", f64::floor);
    m.function("float.ceil", f64::ceil);
    m.function("float.round", f64::round);
    m.function("float.trunc", f64::trunc);
    m.function("float.sqrt", f64::sqrt);
    m.function("float.pow", |x: f64, e: f64| x.powf(e));
    m.function("float.min", |x: f64, y: f64| x.min(y));
    m.function("float.max", |x: f64, y: f64| x.max(y));
    m.function("float.is_nan", |x: f64| x.is_nan());
    m.function("float.is_finite", |x: f64| x.is_finite());
    m.function("float.is_infinite", |x: f64| x.is_infinite());
    m.function("float.to_int", |x: f64| -> Result<i64, Ctl> {
        let f = x.trunc();
        // NaN is in no range.
        if !(-9223372036854775808.0..9223372036854775808.0).contains(&f) {
            return fail("invalid", format!("cannot convert {} to int", crate::vm::display::fmt_float(x)));
        }
        Ok(f as i64)
    });
    m.function("float.to_str_fixed", |x: f64, digits: i64| format!("{:.*}", digits.clamp(0, 1024) as usize, x));
    m.function("duration.nanos", |d: SignedDuration| nanos(d));
    m.function("duration.micros", |d: SignedDuration| nanos(d) / 1_000);
    m.function("duration.millis", |d: SignedDuration| nanos(d) / 1_000_000);
    m.function("duration.secs", |d: SignedDuration| nanos(d) as f64 / 1e9);
    m.function("duration.abs", |d: SignedDuration| match nanos(d).checked_abs() {
        Some(v) => Ok(SignedDuration::from_nanos(v)),
        None => fail("overflow", "duration overflow"),
    });
    m.function("duration.from_nanos", |n: i64| SignedDuration::from_nanos(n));
    m.function("duration.from_millis", |n: i64| match n.checked_mul(1_000_000) {
        Some(v) => Ok(SignedDuration::from_nanos(v)),
        None => fail("overflow", "duration overflow"),
    });
    m.function("duration.from_secs", |s: f64| {
        let n = s * 1e9;
        if !n.is_finite() || n.abs() > 9.2e18 {
            return fail("overflow", "duration overflow");
        }
        Ok(SignedDuration::from_nanos(n as i64))
    });
}

/// Splits a string into a `[str]` of new strings (pieces of one ASCII
/// character are shared, see `Vm::short_str`). `split` reads the text from
/// the heap and calls its argument for each piece; it is called twice, to
/// count the pieces and check the memory limit before building them, and to
/// build them. The caller charges for scanning the text.
fn split_list(vm: &mut Vm, text: Str, split: impl for<'h> Fn(&'h Heap, &mut dyn FnMut(&'h str))) -> Result<List<String>, Ctl> {
    let mut count = 0usize;
    split(&vm.heap, &mut |_| count += 1);
    produce(vm, count, count)?;
    vm.check_room(text.str(&vm.heap).len().saturating_add(count.saturating_mul(ITEM)))?;
    Ok(Val::from_raw(vm.staged_strs(|heap, st, ascii| {
        st.expect(count + 1);
        let mut items = Vec::with_capacity(count);
        split(heap, &mut |p| {
            items.push(match p.as_bytes() {
                &[b] if b.is_ascii() => ascii[b as usize],
                _ => st.new_str(p),
            })
        });
        st.new_list(items)
    })))
}

/// A new string holding the bytes `range` of string `v` (copied once).
pub(crate) fn new_substr(vm: &mut Vm, v: Value, range: std::ops::Range<usize>) -> Value {
    let s = compact_str::CompactString::new(vm.str(v).get(range).unwrap_or_default());
    vm.heap.new_str(s)
}

/// The byte range of `part`, a slice of `whole`, within it.
pub(crate) fn span_of(whole: &str, part: &str) -> std::ops::Range<usize> {
    let start = (part.as_ptr() as usize).wrapping_sub(whole.as_ptr() as usize);
    start..start + part.len()
}

fn char_class(vm: &Vm, s: Str, f: impl Fn(char) -> bool) -> Result<bool, Ctl> {
    let t = s.str(&vm.heap);
    vm.charge_bytes(t.len())?;
    Ok(!t.is_empty() && t.chars().all(f))
}

fn strings(m: &mut NativeModule) {
    m.vm_function("str.len", |vm: &mut Vm, s: Str| -> Result<i64, Ctl> { Ok(s.str(&vm.heap).len() as i64) });
    m.vm_function("str.is_empty", |vm: &mut Vm, s: Str| -> Result<bool, Ctl> { Ok(s.str(&vm.heap).is_empty()) });
    m.vm_function("str.chars", |vm: &mut Vm, s: Str| {
        vm.charge_bytes(s.str(&vm.heap).len())?;
        split_list(vm, s, |heap, f| {
            let t = s.str(heap);
            t.char_indices().for_each(|(i, c)| f(&t[i..i + c.len_utf8()]))
        })
    });
    m.vm_function("str.char_count", |vm: &mut Vm, s: Str| -> Result<i64, Ctl> {
        let t = s.str(&vm.heap);
        vm.charge_bytes(t.len())?;
        Ok(t.chars().count() as i64)
    });
    m.vm_function("str.codepoints", |vm: &mut Vm, s: Str| -> Result<Vec<i64>, Ctl> {
        let t = s.str(&vm.heap);
        vm.charge_bytes(t.len())?;
        reserve_list(vm, t.len())?;
        Ok(t.chars().map(|c| c as i64).collect())
    });
    m.vm_function("str.bytes", |vm: &mut Vm, s: Str| -> Result<Vec<u8>, Ctl> {
        let t = s.str(&vm.heap);
        vm.check_room(t.len())?;
        Ok(t.as_bytes().to_vec())
    });
    // Searches charge for the needle too: preparing the search scans it,
    // however short the text searched.
    m.vm_function("str.contains", |vm: &mut Vm, s: Str, sub: Str| -> Result<bool, Ctl> {
        let (t, p) = (s.str(&vm.heap), sub.str(&vm.heap));
        vm.charge_bytes(t.len().saturating_add(p.len()))?;
        Ok(t.contains(p))
    });
    m.vm_function("str.starts_with", |vm: &mut Vm, s: Str, prefix: Str| -> Result<bool, Ctl> {
        let p = prefix.str(&vm.heap);
        vm.charge_bytes(p.len())?;
        Ok(s.str(&vm.heap).starts_with(p))
    });
    m.vm_function("str.ends_with", |vm: &mut Vm, s: Str, suffix: Str| -> Result<bool, Ctl> {
        let p = suffix.str(&vm.heap);
        vm.charge_bytes(p.len())?;
        Ok(s.str(&vm.heap).ends_with(p))
    });
    m.vm_function("str.find", |vm: &mut Vm, s: Str, sub: Str| -> Result<Option<i64>, Ctl> {
        let (t, p) = (s.str(&vm.heap), sub.str(&vm.heap));
        vm.charge_bytes(t.len().saturating_add(p.len()))?;
        Ok(t.find(p).map(|i| i as i64))
    });
    m.vm_function("str.rfind", |vm: &mut Vm, s: Str, sub: Str| -> Result<Option<i64>, Ctl> {
        let (t, p) = (s.str(&vm.heap), sub.str(&vm.heap));
        vm.charge_bytes(t.len().saturating_add(p.len()))?;
        Ok(t.rfind(p).map(|i| i as i64))
    });
    m.vm_function("str.replace", |vm: &mut Vm, s: Str, pattern: Str, replacement: Str| -> Result<String, Ctl> {
        let (t, from, to) = (s.str(&vm.heap), pattern.str(&vm.heap), replacement.str(&vm.heap));
        if from.is_empty() {
            vm.check_room(t.len())?;
            return Ok(t.to_string());
        }
        // Count the matches first so the output size is known exactly.
        vm.charge_bytes(t.len().saturating_add(from.len()))?;
        let n = t.matches(from).count();
        vm.check_room(t.len() - n * from.len() + n.saturating_mul(to.len()))?;
        Ok(t.replace(from, to))
    });
    m.vm_function("str.split", |vm: &mut Vm, s: Str, sep: Str| {
        let (t, sep_s) = (s.str(&vm.heap), sep.str(&vm.heap));
        if sep_s.is_empty() {
            return fail("invalid", "split with an empty separator");
        }
        vm.charge_bytes(t.len().saturating_add(sep_s.len()))?;
        split_list(vm, s, |heap, f| s.str(heap).split(sep.str(heap)).for_each(f))
    });
    m.vm_function("str.split_once", |vm: &mut Vm, s: Str, sep: Str| -> Result<Option<Val<(String, String)>>, Ctl> {
        let (t, sep) = (s.str(&vm.heap), sep.str(&vm.heap));
        vm.charge_bytes(t.len().saturating_add(sep.len()))?;
        let Some((x, y)) = t.split_once(sep) else { return Ok(None) };
        let (x, y) = (span_of(t, x), span_of(t, y));
        copy_bytes(vm, x.len() + y.len())?;
        let (x, y) = (new_substr(vm, s.raw(), x), new_substr(vm, s.raw(), y));
        Ok(Some(Val::from_raw(vm.heap.new_tuple(&[x, y]))))
    });
    m.vm_function("str.split_whitespace", |vm: &mut Vm, s: Str| {
        vm.charge_bytes(s.str(&vm.heap).len())?;
        split_list(vm, s, |heap, f| s.str(heap).split_whitespace().for_each(f))
    });
    m.vm_function("str.lines", |vm: &mut Vm, s: Str| {
        vm.charge_bytes(s.str(&vm.heap).len())?;
        split_list(vm, s, |heap, f| s.str(heap).lines().for_each(f))
    });
    fn substr(vm: &mut Vm, s: Str, f: impl Fn(&str) -> &str) -> Result<Str, Ctl> {
        let t = s.str(&vm.heap);
        vm.charge_bytes(t.len())?;
        let sub = f(t);
        if sub.len() == t.len() {
            return Ok(s);
        }
        let range = span_of(t, sub);
        copy_bytes(vm, range.len())?;
        Ok(Val::from_raw(new_substr(vm, s.raw(), range)))
    }
    m.vm_function("str.trim", |vm: &mut Vm, s: Str| substr(vm, s, str::trim));
    m.vm_function("str.trim_start", |vm: &mut Vm, s: Str| substr(vm, s, str::trim_start));
    m.vm_function("str.trim_end", |vm: &mut Vm, s: Str| substr(vm, s, str::trim_end));
    fn strip(vm: &mut Vm, s: Str, affix: Str, f: impl Fn(&str, &str) -> Option<usize>) -> Result<Option<Str>, Ctl> {
        let (t, affix) = (s.str(&vm.heap), affix.str(&vm.heap));
        vm.charge_bytes(affix.len())?;
        let Some(start) = f(t, affix) else { return Ok(None) };
        let range = start..start + t.len() - affix.len();
        copy_bytes(vm, range.len())?;
        Ok(Some(Val::from_raw(new_substr(vm, s.raw(), range))))
    }
    m.vm_function("str.strip_prefix", |vm: &mut Vm, s: Str, prefix: Str| strip(vm, s, prefix, |t, p| t.starts_with(p).then_some(p.len())));
    m.vm_function("str.strip_suffix", |vm: &mut Vm, s: Str, suffix: Str| strip(vm, s, suffix, |t, p| t.ends_with(p).then_some(0)));
    m.vm_function("str.to_upper", |vm: &mut Vm, s: Str| -> Result<String, Ctl> {
        let t = s.str(&vm.heap);
        vm.check_room(case_mapped_bound(t))?;
        vm.charge_slow_bytes(t.len())?;
        Ok(t.to_uppercase())
    });
    m.vm_function("str.to_lower", |vm: &mut Vm, s: Str| -> Result<String, Ctl> {
        let t = s.str(&vm.heap);
        vm.check_room(case_mapped_bound(t))?;
        vm.charge_slow_bytes(t.len())?;
        Ok(t.to_lowercase())
    });
    m.vm_function("str.repeat", |vm: &mut Vm, s: Str, n: i64| -> Result<String, Ctl> {
        if n < 0 {
            return fail("invalid", format!("negative repeat count: {n}"));
        }
        let t = s.str(&vm.heap);
        let n = to_usize_saturating(n);
        vm.check_room(t.len().checked_mul(n).ok_or(Ctl::limit(crate::LimitKind::Memory))?)?;
        Ok(t.repeat(n))
    });
    fn pad(vm: &mut Vm, s: Str, width: i64, fill: Str, start: bool) -> Result<Str, Ctl> {
        let width = to_usize_saturating(width);
        let (t, fill) = (s.str(&vm.heap), fill.str(&vm.heap));
        vm.charge_bytes(t.len())?;
        let n = t.chars().count();
        if n >= width || fill.is_empty() {
            return Ok(s);
        }
        let chars = width - n;
        copy_bytes(vm, chars.saturating_mul(fill.len().min(4)).saturating_add(t.len()))?;
        let padding: String = fill.chars().cycle().take(chars).collect();
        let p = if start { padding + t } else { format!("{t}{padding}") };
        Ok(Val::from_raw(vm.new_str(p)))
    }
    m.vm_function("str.pad_start", |vm: &mut Vm, s: Str, width: i64, fill: Str| pad(vm, s, width, fill, true));
    m.vm_function("str.pad_end", |vm: &mut Vm, s: Str, width: i64, fill: Str| pad(vm, s, width, fill, false));
    m.vm_function("str.parse_int", |vm: &mut Vm, s: Str, radix: i64| -> Result<Option<i64>, Ctl> {
        if !(2..=36).contains(&radix) {
            return fail("invalid", format!("radix must be between 2 and 36, found {radix}"));
        }
        let t = s.str(&vm.heap);
        vm.charge_bytes(t.len())?;
        Ok(if t.starts_with('+') { None } else { i64::from_str_radix(t, radix as u32).ok() })
    });
    m.vm_function("str.parse_float", |vm: &mut Vm, s: Str| -> Result<Option<f64>, Ctl> {
        let t = s.str(&vm.heap);
        vm.charge_bytes(t.len())?;
        // Decimal and scientific notation, and the non-finite values as
        // floats display them (Rust's parser also takes `infinity`, `NaN`, ...).
        let ok = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
            || matches!(t, "inf" | "-inf" | "nan");
        Ok(if ok { t.parse::<f64>().ok() } else { None })
    });
    m.vm_function("str.is_digit", |vm: &mut Vm, s: Str| char_class(vm, s, |c| c.is_ascii_digit()));
    m.vm_function("str.is_alpha", |vm: &mut Vm, s: Str| char_class(vm, s, char::is_alphabetic));
    m.vm_function("str.is_alnum", |vm: &mut Vm, s: Str| char_class(vm, s, char::is_alphanumeric));
    m.vm_function("str.is_space", |vm: &mut Vm, s: Str| char_class(vm, s, char::is_whitespace));
    m.vm_function("str.is_upper", |vm: &mut Vm, s: Str| char_class(vm, s, char::is_uppercase));
    m.vm_function("str.is_lower", |vm: &mut Vm, s: Str| char_class(vm, s, char::is_lowercase));
    m.vm_function("str.chars_iter_next", |vm: &mut Vm, s: Str| -> Result<IterFn<String>, Ctl> {
        Ok(Val::from_raw(vm.iter_fn(IterSrc::Chars { s: s.raw().handle().unwrap_or(0), i: 0 })))
    });
    m.vm_function("str.char_at", |vm: &mut Vm, s: Str, i: i64| -> Result<Option<Str>, Ctl> {
        let t = s.str(&vm.heap);
        let c = to_index(i).filter(|&i| i < t.len() && t.is_char_boundary(i)).and_then(|i| t[i..].chars().next());
        Ok(c.map(|c| Val::from_raw(vm.short_str(c.encode_utf8(&mut [0; 4])))))
    });
    m.vm_function("str.ord", |vm: &mut Vm, s: Str| -> Result<i64, Ctl> {
        let t = s.str(&vm.heap);
        let mut it = t.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Ok(c as i64),
            _ => fail("invalid", format!("ord requires a one-character string, found {:?}", super::truncate(t, 64))),
        }
    });
    m.vm_function("str.reversed", |vm: &mut Vm, s: Str| -> Result<String, Ctl> {
        let t = s.str(&vm.heap);
        vm.check_room(t.len())?;
        Ok(t.chars().rev().collect())
    });
    m.vm_function("List.join", join);
}

fn join(vm: &mut Vm, list: List<String>, sep: Str) -> Result<String, Ctl> {
    let sep_s = sep.str(&vm.heap);
    let items = list.items(&vm.heap);
    vm.charge(items.len() as u64)?;
    let total = items.iter().fold(sep_s.len().saturating_mul(items.len()), |n, x| n.saturating_add(vm.heap.str(*x).len()));
    vm.check_room(total)?;
    let mut out = String::with_capacity(total);
    for (i, x) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(sep_s);
        }
        out.push_str(vm.heap.str(*x));
    }
    Ok(out)
}

fn bytes(m: &mut NativeModule) {
    m.vm_function("bytes.len", |vm: &mut Vm, b: Bytes| -> Result<i64, Ctl> { Ok(b.bytes(&vm.heap).len() as i64) });
    m.vm_function("bytes.is_empty", |vm: &mut Vm, b: Bytes| -> Result<bool, Ctl> { Ok(b.bytes(&vm.heap).is_empty()) });
    m.vm_function("bytes.get", |vm: &mut Vm, b: Bytes, i: i64| -> Result<Option<i64>, Ctl> {
        Ok(to_index(i).and_then(|i| b.bytes(&vm.heap).get(i)).map(|b| *b as i64))
    });
    m.vm_function("bytes.to_list", |vm: &mut Vm, b: Bytes| -> Result<Vec<i64>, Ctl> {
        let b = b.bytes(&vm.heap);
        reserve_list(vm, b.len())?;
        Ok(b.iter().map(|&x| x as i64).collect())
    });
    m.vm_function("bytes.to_str", |vm: &mut Vm, b: Bytes| -> Result<String, HostError> {
        let b = b.bytes(&vm.heap);
        vm.check_room(b.len())?;
        match std::str::from_utf8(b) {
            Ok(s) => Ok(s.to_string()),
            Err(e) => {
                vm.charge_bytes(b.len())?;
                Err(HostError::new("invalid", format!("invalid UTF-8: {e}")))
            }
        }
    });
    m.vm_function("bytes.to_str_lossy", |vm: &mut Vm, b: Bytes| -> Result<String, Ctl> {
        let b = b.bytes(&vm.heap);
        vm.check_room(b.len())?;
        Ok(String::from_utf8_lossy(b).into_owned())
    });
    m.vm_function("bytes.to_hex", |vm: &mut Vm, b: Bytes| -> Result<String, Ctl> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let b = b.bytes(&vm.heap);
        vm.check_room(b.len().saturating_mul(2))?;
        vm.charge_slow_bytes(b.len())?;
        let mut s = String::with_capacity(b.len() * 2);
        for x in b {
            s.push(HEX[(x >> 4) as usize] as char);
            s.push(HEX[(x & 15) as usize] as char);
        }
        Ok(s)
    });
    m.vm_function("bytes.contains", |vm: &mut Vm, b: Bytes, needle: Bytes| -> Result<bool, Ctl> {
        let pos = find_bytes(vm, b.bytes(&vm.heap), needle.bytes(&vm.heap))?;
        Ok(pos.is_some())
    });
    m.vm_function("bytes.find", |vm: &mut Vm, b: Bytes, needle: Bytes| -> Result<Option<i64>, Ctl> {
        let pos = find_bytes(vm, b.bytes(&vm.heap), needle.bytes(&vm.heap))?;
        Ok(pos.map(|p| p as i64))
    });
    m.vm_function("bytes.from_list", |vm: &mut Vm, items: List<i64>| -> Result<Vec<u8>, Ctl> {
        let n = list_len(vm, items);
        vm.check_room(n)?;
        vm.charge(n as u64)?;
        let mut out = Vec::with_capacity(n);
        for x in items.items(&vm.heap) {
            let i = x.as_int();
            if !(0..=255).contains(&i) {
                return fail("bounds", format!("byte out of range: {i}"));
            }
            out.push(i as u8);
        }
        Ok(out)
    });
    m.vm_function("bytes.from_hex", |vm: &mut Vm, text: Str| -> Result<Option<Vec<u8>>, Ctl> {
        let t = text.str(&vm.heap);
        vm.check_room(t.len() / 2)?;
        vm.charge_slow_bytes(t.len())?;
        let ok = t.len().is_multiple_of(2) && t.bytes().all(|b| b.is_ascii_hexdigit());
        if !ok {
            return Ok(None);
        }
        Ok(Some((0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).unwrap()).collect()))
    });
}

fn list_len<T: native::SandType>(vm: &Vm, xs: List<T>) -> usize {
    xs.items(&vm.heap).len()
}

/// The first position of `needle` in `hay` (Knuth-Morris-Pratt: linear time
/// even for adversarial inputs), charging for the scan.
fn find_bytes(vm: &Vm, hay: &[u8], needle: &[u8]) -> Result<Option<usize>, Ctl> {
    if needle.is_empty() {
        return Ok(Some(0));
    }
    if needle.len() > hay.len() {
        return Ok(None);
    }
    vm.check_room(needle.len().saturating_mul(std::mem::size_of::<usize>()))?;
    vm.charge_slow_bytes(hay.len() + needle.len())?;
    let mut fail = vec![0usize; needle.len()];
    let mut k = 0;
    for i in 1..needle.len() {
        while k > 0 && needle[i] != needle[k] {
            k = fail[k - 1];
        }
        if needle[i] == needle[k] {
            k += 1;
        }
        fail[i] = k;
    }
    let mut k = 0;
    for (i, &b) in hay.iter().enumerate() {
        while k > 0 && b != needle[k] {
            k = fail[k - 1];
        }
        if b == needle[k] {
            k += 1;
            if k == needle.len() {
                return Ok(Some(i + 1 - k));
            }
        }
    }
    Ok(None)
}

/// The index of the first element equal to `x`, charging for the comparisons.
fn index_of(vm: &Vm, list: List, x: Param<0, EqB>) -> Result<Option<usize>, Ctl> {
    for (i, y) in list.items(&vm.heap).iter().enumerate() {
        vm.charge(1)?;
        if values_eq_metered(&vm.heap, &vm.meter, *y, x.raw())? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn index_check(i: i64, len: usize, inclusive: bool) -> Result<usize, Ctl> {
    let max = if inclusive { len } else { len.saturating_sub(1) };
    match to_index(i) {
        Some(at) if at <= max && (inclusive || len > 0) => Ok(at),
        _ => Err(Ctl::fail("bounds", format!("index out of bounds: index {i}, length {len}"))),
    }
}

/// Sorts values by the language's total order; each comparison is charged
/// to the meter, so sorting a huge list can be interrupted.
fn sort_values(vm: &Vm, items: &mut [Value]) -> Result<(), Ctl> {
    produce(vm, items.len(), 0)?; // scratch for the merge sort
    if let Some(r) = try_sort_numbers(items, &vm.meter) {
        return r;
    }
    if let Some(r) = try_sort_packed(items, &vm.heap, &vm.meter) {
        return r;
    }
    let (prog, heap, meter) = (&vm.prog, &vm.heap, &vm.meter);
    try_sort_by(items, |a, b| values_cmp_metered(prog, heap, meter, *a, *b))
}

fn lists(m: &mut NativeModule) {
    m.vm_function("List.len", |vm: &mut Vm, xs: List| -> Result<i64, Ctl> { Ok(list_len(vm, xs) as i64) });
    m.vm_function("List.is_empty", |vm: &mut Vm, xs: List| -> Result<bool, Ctl> { Ok(list_len(vm, xs) == 0) });
    m.vm_function("List.push", |vm: &mut Vm, xs: List, x: Param<0>| -> Result<_, Ctl> {
        vm.heap.list_push(xs.raw(), x.raw()).map_err(Ctl::limit)?;
        Ok(())
    });
    m.vm_function("List.iter_next", |vm: &mut Vm, xs: List| -> Result<IterFn<Param<0>>, Ctl> {
        let n = list_len(vm, xs) as i64;
        Ok(Val::from_raw(vm.iter_fn(IterSrc::List { list: xs.raw().handle().unwrap_or(0), i: 0, n })))
    });
    m.vm_function("List.pop", |vm: &mut Vm, xs: List| -> Result<Option<Param<0>>, Ctl> {
        Ok(vm.heap.list_shrink(xs.raw()).and_then(Vec::pop).map(Param::from_raw))
    });
    m.vm_function("List.insert", |vm: &mut Vm, xs: List, i: i64, x: Param<0>| -> Result<_, Ctl> {
        let n = list_len(vm, xs);
        let i = index_check(i, n, true)?;
        vm.charge_bytes((n - i) * SLOT)?;
        vm.heap.list_reserve(xs.raw(), 1).map_err(Ctl::limit)?;
        vm.heap.list_mut(xs.raw()).insert(i, x.raw());
        Ok(())
    });
    m.vm_function("List.remove", |vm: &mut Vm, xs: List, i: i64| -> Result<_, Ctl> {
        let n = list_len(vm, xs);
        let i = index_check(i, n, false)?;
        vm.charge_bytes((n - i) * SLOT)?;
        Ok(Param::<0>::from_raw(vm.heap.list_mut(xs.raw()).remove(i)))
    });
    m.vm_function("List.extend", |vm: &mut Vm, xs: List, items: List| -> Result<_, Ctl> {
        let n = list_len(vm, items);
        vm.charge(n as u64)?;
        vm.heap.list_reserve(xs.raw(), n).map_err(Ctl::limit)?;
        vm.heap.list_extend(xs.raw(), items.raw());
        Ok(())
    });
    m.vm_function("List.clear", |vm: &mut Vm, xs: List| -> Result<_, Ctl> {
        if let Some(l) = vm.heap.list_shrink(xs.raw()) {
            l.clear();
        }
        Ok(())
    });
    m.vm_function("List.get", |vm: &mut Vm, xs: List, i: i64| -> Result<Option<Param<0>>, Ctl> {
        Ok(to_index(i).and_then(|i| xs.items(&vm.heap).get(i)).copied().map(Param::from_raw))
    });
    m.vm_function("List.first", |vm: &mut Vm, xs: List| -> Result<Option<Param<0>>, Ctl> {
        Ok(xs.items(&vm.heap).first().copied().map(Param::from_raw))
    });
    m.vm_function("List.last", |vm: &mut Vm, xs: List| -> Result<Option<Param<0>>, Ctl> {
        Ok(xs.items(&vm.heap).last().copied().map(Param::from_raw))
    });
    m.vm_function("List.contains", |vm: &mut Vm, xs: List, x: Param<0, EqB>| -> Result<bool, Ctl> { Ok(index_of(vm, xs, x)?.is_some()) });
    m.vm_function("List.index_of", |vm: &mut Vm, xs: List, x: Param<0, EqB>| -> Result<Option<i64>, Ctl> {
        Ok(index_of(vm, xs, x)?.map(|p| p as i64))
    });
    m.vm_function("List.reverse", |vm: &mut Vm, xs: List| -> Result<_, Ctl> {
        vm.charge(list_len(vm, xs) as u64)?;
        vm.heap.list_mut(xs.raw()).reverse();
        Ok(())
    });
    m.vm_function("List.reversed", |vm: &mut Vm, xs: List| -> Result<Vec<Param<0>>, Ctl> {
        reserve_list(vm, list_len(vm, xs))?;
        Ok(params(xs.items(&vm.heap).iter().rev().copied().collect()))
    });
    m.vm_function("List.sort", |vm: &mut Vm, xs: List<Param<0, OrdB>>| {
        let mut v = std::mem::take(vm.heap.list_mut(xs.raw()));
        let r = sort_values(vm, &mut v);
        *vm.heap.list_mut(xs.raw()) = v;
        r
    });
    m.vm_function("List.sorted", |vm: &mut Vm, xs: List<Param<0, OrdB>>| -> Result<Vec<Param<0>>, Ctl> {
        reserve_list(vm, list_len(vm, xs))?;
        let mut v = xs.items(&vm.heap).to_vec();
        sort_values(vm, &mut v)?;
        Ok(params(v))
    });
    m.vm_function("List.sort_by_keys", |vm: &mut Vm, xs: List, keys: List<Param<1, OrdB>>| {
        let keys = keys.items(&vm.heap).to_vec();
        let items = std::mem::take(vm.heap.list_mut(xs.raw()));
        if keys.len() != items.len() {
            *vm.heap.list_mut(xs.raw()) = items;
            return fail("invalid", "list changed while computing sort keys");
        }
        let mut idx: Vec<usize> = (0..items.len()).collect();
        let r = produce(vm, 2 * items.len(), 0).and_then(|()| {
            let (prog, heap, meter) = (&vm.prog, &vm.heap, &vm.meter);
            try_sort_by(&mut idx, |&x, &y| values_cmp_metered(prog, heap, meter, keys[x], keys[y]))
        });
        let sorted: Vec<Value> = match r {
            Ok(()) => idx.into_iter().map(|i| items[i]).collect(),
            Err(_) => items,
        };
        *vm.heap.list_mut(xs.raw()) = sorted;
        r
    });
    m.vm_function("List.copy", |vm: &mut Vm, xs: List| -> Result<Vec<Param<0>>, Ctl> {
        reserve_list(vm, list_len(vm, xs))?;
        Ok(params(xs.items(&vm.heap).to_vec()))
    });
    m.vm_function("List.swap", |vm: &mut Vm, xs: List, i: i64, j: i64| -> Result<_, Ctl> {
        let n = list_len(vm, xs);
        let i = index_check(i, n, false)?;
        let j = index_check(j, n, false)?;
        vm.heap.list_mut(xs.raw()).swap(i, j);
        Ok(())
    });
    m.vm_function("List.take", |vm: &mut Vm, xs: List, n: i64| -> Result<Vec<Param<0>>, Ctl> {
        let l = xs.items(&vm.heap);
        let n = to_usize_saturating(n).min(l.len());
        reserve_list(vm, n)?;
        Ok(params(l[..n].to_vec()))
    });
    m.vm_function("List.skip", |vm: &mut Vm, xs: List, n: i64| -> Result<Vec<Param<0>>, Ctl> {
        let l = xs.items(&vm.heap);
        let n = to_usize_saturating(n).min(l.len());
        reserve_list(vm, l.len() - n)?;
        Ok(params(l[n..].to_vec()))
    });
    m.vm_function("List.chunks", |vm: &mut Vm, xs: List, n: i64| -> Result<List<Vec<Param<0>>>, Ctl> {
        if n <= 0 {
            return fail("invalid", format!("chunk size must be positive, found {n}"));
        }
        let (n, len) = (to_usize_saturating(n), list_len(vm, xs));
        produce(vm, len, len.div_ceil(n))?;
        Ok(Val::from_raw(vm.heap.staged(|heap, st| {
            let parts = xs.items(heap).chunks(n).map(|c| st.new_list(c.to_vec())).collect();
            st.new_list(parts)
        })))
    });
    m.vm_function("List.windows", |vm: &mut Vm, xs: List, n: i64| -> Result<List<Vec<Param<0>>>, Ctl> {
        if n <= 0 {
            return fail("invalid", format!("window size must be positive, found {n}"));
        }
        let (n, l) = (to_usize_saturating(n), xs.items(&vm.heap));
        let windows = (l.len() + 1).saturating_sub(n);
        produce(vm, windows.saturating_mul(n), windows)?;
        Ok(Val::from_raw(vm.heap.staged(|heap, st| {
            let parts = xs.items(heap).windows(n).map(|w| st.new_list(w.to_vec())).collect();
            st.new_list(parts)
        })))
    });
    m.vm_function("List.enumerate", |vm: &mut Vm, xs: List| -> Result<List<(i64, Param<0>)>, Ctl> {
        produce(vm, list_len(vm, xs), list_len(vm, xs))?;
        Ok(Val::from_raw(vm.heap.staged(|heap, st| {
            let pairs = xs.items(heap).iter().enumerate().map(|(i, &x)| st.new_tuple(vec![Value::Int(i as i64), x])).collect();
            st.new_list(pairs)
        })))
    });
    m.vm_function("List.zip", |vm: &mut Vm, xs: List, ys: List<Param<1>>| -> Result<List<(Param<0>, Param<1>)>, Ctl> {
        let n = list_len(vm, xs).min(list_len(vm, ys));
        produce(vm, n, n)?;
        Ok(Val::from_raw(vm.heap.staged(|heap, st| {
            let pairs = xs.items(heap).iter().zip(ys.items(heap)).map(|(&x, &y)| st.new_tuple(vec![x, y])).collect();
            st.new_list(pairs)
        })))
    });
    m.vm_function("List.dedup", |vm: &mut Vm, xs: List<Param<0, EqB>>| -> Result<Vec<Param<0>>, Ctl> {
        let items = xs.items(&vm.heap);
        vm.charge(items.len() as u64)?;
        reserve_list(vm, items.len())?;
        let mut out: Vec<Value> = Vec::new();
        for &x in items {
            let dup = match out.last() {
                Some(l) => values_eq_metered(&vm.heap, &vm.meter, *l, x)?,
                None => false,
            };
            if !dup {
                out.push(x);
            }
        }
        Ok(params(out))
    });
    m.vm_function("List.unique", |vm: &mut Vm, xs: List<Param<0, HashB>>| -> Result<Vec<Param<0>>, Ctl> {
        let set = set_of(vm, xs.items(&vm.heap))?;
        reserve_list(vm, set.live)?;
        Ok(params(set.entries.into_iter().flatten().collect()))
    });
    m.vm_function("List.to_set", |vm: &mut Vm, xs: List<Param<0, HashB>>| -> Result<_, Ctl> {
        let set = set_of(vm, xs.items(&vm.heap))?;
        Ok(new_set(vm, set))
    });
    // Values are erased, so the identity (`T.zero()` / `T.one()`, from the
    // `Num` evidence of the caller) is passed in: an empty `[float]` sums to 0.0.
    fn fold_num(vm: &mut Vm, xs: List, init: Param<0>, product: bool) -> Result<Param<0>, Ctl> {
        produce(vm, list_len(vm, xs), 0)?;
        let items = xs.items(&vm.heap).to_vec();
        let mut acc = init.raw();
        for x in items {
            acc = vm.arith(if product { 2 } else { 0 }, acc, x)?;
        }
        Ok(Param::from_raw(acc))
    }
    m.vm_function("List.sum_from", |vm: &mut Vm, xs: List, init: Param<0>| fold_num(vm, xs, init, false));
    m.vm_function("List.product_from", |vm: &mut Vm, xs: List, init: Param<0>| fold_num(vm, xs, init, true));
    fn extreme(vm: &mut Vm, xs: List<Param<0, OrdB>>, want: Ordering) -> Result<Option<Param<0>>, Ctl> {
        let items = xs.items(&vm.heap);
        let mut best: Option<Value> = None;
        for &x in items {
            best = Some(match best {
                None => x,
                Some(b) => {
                    if values_cmp_metered(&vm.prog, &vm.heap, &vm.meter, x, b)? == want {
                        x
                    } else {
                        b
                    }
                }
            });
        }
        Ok(best.map(Param::from_raw))
    }
    m.vm_function("List.min", |vm: &mut Vm, xs: List<Param<0, OrdB>>| extreme(vm, xs, Ordering::Less));
    m.vm_function("List.max", |vm: &mut Vm, xs: List<Param<0, OrdB>>| extreme(vm, xs, Ordering::Greater));
}

/// Returns the inclusive `(first, last)` bounds, or `None` if empty.
fn inclusive(vm: &Vm, r: Range) -> Result<Option<(i64, i64)>, Ctl> {
    let (a, b, incl) = r.bounds(&vm.heap)?;
    let last = if incl { Some(b) } else { b.checked_sub(1) };
    Ok(last.filter(|&last| a <= last).map(|last| (a, last)))
}

fn count(bounds: Option<(i64, i64)>) -> i128 {
    bounds.map_or(0, |(first, last)| last as i128 - first as i128 + 1)
}

fn exact(n: i128) -> Result<i64, Ctl> {
    i64::try_from(n).map_err(|_| Ctl::fail("overflow", "integer overflow"))
}

fn ranges(m: &mut NativeModule) {
    m.vm_function("Range.iter_next", |vm: &mut Vm, r: Range| -> Result<IterFn<i64>, Ctl> {
        let src = match inclusive(vm, r)? {
            Some((i, last)) => IterSrc::Range { i, last },
            None => IterSrc::Done,
        };
        Ok(Val::from_raw(vm.iter_fn(src)))
    });
    m.vm_function("Range.start", |vm: &mut Vm, r: Range| -> Result<i64, Ctl> { Ok(r.bounds(&vm.heap)?.0) });
    m.vm_function("Range.end", |vm: &mut Vm, r: Range| match r.bounds(&vm.heap)? {
        (_, b, true) => exact(b as i128 + 1),
        (_, b, false) => Ok(b),
    });
    m.vm_function("Range.len", |vm: &mut Vm, r: Range| exact(count(inclusive(vm, r)?)));
    m.vm_function("Range.is_empty", |vm: &mut Vm, r: Range| -> Result<bool, Ctl> { Ok(inclusive(vm, r)?.is_none()) });
    m.vm_function("Range.contains", |vm: &mut Vm, r: Range, i: i64| -> Result<bool, Ctl> {
        Ok(inclusive(vm, r)?.is_some_and(|(first, last)| (first..=last).contains(&i)))
    });
    for name in ["Range.first", "Range.min"] {
        m.vm_function(name, |vm: &mut Vm, r: Range| -> Result<Option<i64>, Ctl> { Ok(inclusive(vm, r)?.map(|(first, _)| first)) });
    }
    for name in ["Range.last", "Range.max"] {
        m.vm_function(name, |vm: &mut Vm, r: Range| -> Result<Option<i64>, Ctl> { Ok(inclusive(vm, r)?.map(|(_, last)| last)) });
    }
    m.vm_function("Range.sum", |vm: &mut Vm, r: Range| {
        let Some((first, last)) = inclusive(vm, r)? else { return Ok(0) };
        let n = count(Some((first, last)));
        let twice = n.checked_mul(first as i128 + last as i128).ok_or_else(|| Ctl::fail("overflow", "integer overflow"))?;
        exact(twice / 2)
    });
    m.vm_function("Range.product", |vm: &mut Vm, r: Range| {
        let Some((first, last)) = inclusive(vm, r)? else { return Ok(1) };
        if first <= 0 && 0 <= last {
            return Ok(0);
        }
        // Any non-zero factor other than ±1 at least doubles the magnitude, so
        // the loop exits within 64 iterations before `i128` can overflow.
        let mut acc: i128 = 1;
        for i in first..=last {
            acc *= i as i128;
            if i64::try_from(acc).is_err() {
                break;
            }
        }
        exact(acc)
    });
    m.vm_function("Range.to_list", |vm: &mut Vm, r: Range| -> Result<Vec<i64>, Ctl> {
        let bounds = inclusive(vm, r)?;
        reserve_list(vm, usize::try_from(count(bounds)).unwrap_or(usize::MAX))?;
        Ok(bounds.into_iter().flat_map(|(first, last)| first..=last).collect())
    });
    m.vm_function("Range.step_by", |vm: &mut Vm, r: Range, step: i64| -> Result<Vec<i64>, Ctl> {
        if step <= 0 {
            return fail("invalid", format!("step must be positive, found {step}"));
        }
        let Some((first, last)) = inclusive(vm, r)? else { return Ok(Vec::new()) };
        reserve_list(vm, usize::try_from(last.abs_diff(first) / step as u64 + 1).unwrap_or(usize::MAX))?;
        // Stepped in `i64`: a step past `usize::MAX` (32-bit targets) must not be truncated.
        Ok(std::iter::successors(Some(first), |i| i.checked_add(step).filter(|i| *i <= last)).collect())
    });
    m.vm_function("Range.reversed", |vm: &mut Vm, r: Range| -> Result<Vec<i64>, Ctl> {
        let bounds = inclusive(vm, r)?;
        reserve_list(vm, usize::try_from(count(bounds)).unwrap_or(usize::MAX))?;
        Ok(bounds.into_iter().flat_map(|(first, last)| (first..=last).rev()).collect())
    });
}

/// Approximate memory of a set element or map entry built in bulk: the
/// entry plus its share of the index.
const ENTRY: usize = 3 * SLOT;

/// Adds `x` to `set`, a set being built outside the heap (so it needs no
/// write barrier); false if it was there already.
fn set_insert(vm: &Vm, set: &mut SetObj, x: Value) -> Result<bool, Ctl> {
    let hash = hash_key(&vm.heap, &vm.meter, x)?;
    if find_key(&vm.heap, &vm.meter, &set.index, hash, x, |i| set.entries[i])?.is_some() {
        return Ok(false);
    }
    set_push(set, hash, x)?;
    Ok(true)
}

/// Appends `x`, which is not in `set` and has hash `hash`.
fn set_push(set: &mut SetObj, hash: u64, x: Value) -> Result<(), Ctl> {
    if !slot_fits(set.entries.len()) {
        return fail("limit", "set too large");
    }
    set.index.insert(x, hash, set.entries.len());
    set.entries.push(Some(x));
    set.live += 1;
    Ok(())
}

/// The set of `items` (the first of equal elements, in order).
fn set_of(vm: &Vm, items: &[Value]) -> Result<SetObj, Ctl> {
    produce(vm, items.len().saturating_mul(ENTRY / SLOT), 0)?;
    let mut set = SetObj::default();
    for &x in items {
        set_insert(vm, &mut set, x)?;
    }
    Ok(set)
}

/// The set of the elements of set `s` that satisfy `keep`, in order.
fn set_filter(vm: &Vm, s: Set, mut keep: impl FnMut(Value) -> Result<bool, Ctl>) -> Result<SetObj, Ctl> {
    let src = s.set(&vm.heap)?;
    produce(vm, src.live.saturating_mul(ENTRY / SLOT), 0)?;
    let mut set = SetObj::default();
    for &x in src.entries.iter().flatten() {
        if keep(x)? {
            // Elements of a set are distinct: no lookup is needed.
            let hash = hash_key(&vm.heap, &vm.meter, x)?;
            set_push(&mut set, hash, x)?;
        }
    }
    Ok(set)
}

fn new_set(vm: &mut Vm, set: SetObj) -> Set {
    Val::from_raw(vm.heap.alloc(Obj::Set(Box::new(set))))
}

fn alloc_copy(vm: &mut Vm, copy: Obj) -> NResult {
    let size = copy.size();
    vm.check_room(size)?;
    vm.charge_bytes(size)?;
    Ok(vm.heap.alloc(copy))
}

/// Empties map or set `v`, accounting for the memory freed.
fn clear_container(vm: &mut Vm, v: Value) -> Result<(), Ctl> {
    vm.unpin(v)?;
    let before = vm.heap.get(v).size();
    match vm.heap.get_mut(v) {
        Obj::Map(m) => m.clear(),
        Obj::Set(s) => s.clear(),
        _ => return internal("not a map or set"),
    }
    let after = vm.heap.get(v).size();
    vm.heap.account(before, after);
    Ok(())
}

fn maps(m: &mut NativeModule) {
    fn map_column<const N: u8>(vm: &mut Vm, m: Map) -> Result<Vec<Param<N>>, Ctl> {
        let mo = m.map(&vm.heap)?;
        reserve_list(vm, mo.live)?;
        Ok(mo.entries.iter().flatten().map(|&(k, v)| Param::from_raw(if N == 0 { k } else { v })).collect())
    }
    m.vm_function("Map.len", |vm: &mut Vm, map: Map| -> Result<i64, Ctl> { Ok(map.map(&vm.heap)?.live as i64) });
    m.vm_function("Map.is_empty", |vm: &mut Vm, map: Map| -> Result<bool, Ctl> { Ok(map.map(&vm.heap)?.live == 0) });
    m.vm_function("Map.get", |vm: &mut Vm, map: Map, key: Param<0>| -> Result<Option<Param<1>>, Ctl> {
        Ok(vm.map_get(map.raw(), key.raw())?.map(Param::from_raw))
    });
    m.vm_function("Map.get_or", |vm: &mut Vm, map: Map, key: Param<0>, default: Param<1>| -> Result<_, Ctl> {
        Ok(Param::<1>::from_raw(vm.map_get(map.raw(), key.raw())?.unwrap_or(default.raw())))
    });
    m.vm_function("Map.contains", |vm: &mut Vm, map: Map, key: Param<0>| -> Result<bool, Ctl> {
        Ok(vm.map_get(map.raw(), key.raw())?.is_some())
    });
    m.vm_function("Map.insert", |vm: &mut Vm, map: Map, key: Param<0>, value: Param<1>| -> Result<Option<Param<1>>, Ctl> {
        Ok(vm.map_insert(map.raw(), key.raw(), value.raw())?.map(Param::from_raw))
    });
    m.vm_function("Map.remove", |vm: &mut Vm, map: Map, key: Param<0>| -> Result<Option<Param<1>>, Ctl> {
        Ok(vm.map_remove(map.raw(), key.raw())?.map(Param::from_raw))
    });
    m.vm_function("Map.iter_next", |vm: &mut Vm, map: Map| vm.container_iter(map.raw()).map(IterFn::<(Param<0>, Param<1>)>::from_raw));
    m.vm_function("Map.keys", |vm: &mut Vm, map: Map| map_column::<0>(vm, map));
    m.vm_function("Map.values", |vm: &mut Vm, map: Map| map_column::<1>(vm, map));
    m.vm_function("Map.entries", |vm: &mut Vm, map: Map| -> Result<List<(Param<0>, Param<1>)>, Ctl> {
        let n = map.map(&vm.heap)?.live;
        produce(vm, n, n)?;
        let list = vm.heap.staged(|heap, st| -> Result<Value, Ctl> {
            let items = map.map(heap)?.entries.iter().flatten().map(|&(k, v)| st.new_tuple(vec![k, v])).collect();
            Ok(st.new_list(items))
        })?;
        Ok(Val::from_raw(list))
    });
    m.vm_function("Map.clear", |vm: &mut Vm, map: Map| clear_container(vm, map.raw()));
    m.vm_function("Map.copy", |vm: &mut Vm, map: Map| {
        let copy = Obj::Map(Box::new(map.map(&vm.heap)?.clone()));
        alloc_copy(vm, copy).map(Map::from_raw)
    });
    m.vm_function("Map.extend", |vm: &mut Vm, map: Map, other: Map| -> Result<_, Ctl> {
        let mo = other.map(&vm.heap)?;
        produce(vm, 6 * mo.live, 0)?;
        let es: Vec<(Value, Value)> = mo.entries.iter().flatten().copied().collect();
        for (k, v) in es {
            vm.map_insert(map.raw(), k, v)?;
        }
        Ok(())
    });
    m.vm_function("Map.get_or_insert", |vm: &mut Vm, map: Map, key: Param<0>, default: Param<1>| -> Result<_, Ctl> {
        Ok(Param::<1>::from_raw(vm.map_get_or_insert(map.raw(), key.raw(), default.raw())?.unwrap_or(default.raw())))
    });
}

fn sets(m: &mut NativeModule) {
    m.vm_function("Set.len", |vm: &mut Vm, s: Set| -> Result<i64, Ctl> { Ok(s.set(&vm.heap)?.live as i64) });
    m.vm_function("Set.is_empty", |vm: &mut Vm, s: Set| -> Result<bool, Ctl> { Ok(s.set(&vm.heap)?.live == 0) });
    m.vm_function("Set.add", |vm: &mut Vm, s: Set, x: Param<0>| vm.set_add(s.raw(), x.raw()));
    m.vm_function("Set.remove", |vm: &mut Vm, s: Set, x: Param<0>| vm.set_remove(s.raw(), x.raw()));
    m.vm_function("Set.contains", |vm: &mut Vm, s: Set, x: Param<0>| vm.set_contains(s.raw(), x.raw()));
    m.vm_function("Set.iter_next", |vm: &mut Vm, s: Set| vm.container_iter(s.raw()).map(IterFn::<Param<0>>::from_raw));
    m.vm_function("Set.to_list", |vm: &mut Vm, s: Set| -> Result<Vec<Param<0>>, Ctl> {
        let so = s.set(&vm.heap)?;
        reserve_list(vm, so.live)?;
        Ok(params(so.entries.iter().flatten().copied().collect()))
    });
    m.vm_function("Set.union", |vm: &mut Vm, s: Set, other: Set| -> Result<Set, Ctl> {
        let mut set = s.set(&vm.heap)?.clone();
        let more = &other.set(&vm.heap)?.entries;
        produce(vm, (set.entries.len() + more.len()).saturating_mul(ENTRY / SLOT), 0)?;
        for &x in more.iter().flatten() {
            set_insert(vm, &mut set, x)?;
        }
        Ok(new_set(vm, set))
    });
    m.vm_function("Set.intersection", |vm: &mut Vm, s: Set, other: Set| -> Result<Set, Ctl> {
        let (x, y) = (s.set(&vm.heap)?, other.set(&vm.heap)?);
        if x.live <= y.live {
            let set = set_filter(vm, s, |e| vm.set_contains(other.raw(), e))?;
            return Ok(new_set(vm, set));
        }
        // Look the smaller set's elements up in the larger, and keep the
        // larger's order (the order of `self`).
        produce(vm, y.live.saturating_mul(ENTRY / SLOT), 0)?;
        let mut slots = Vec::new();
        for &e in y.entries.iter().flatten() {
            if let Some(slot) = set_slot(&vm.heap, &vm.meter, x, e)? {
                slots.push(slot);
            }
        }
        vm.charge(slots.len() as u64)?;
        slots.sort_unstable();
        let mut set = SetObj::default();
        for slot in slots {
            let e = x.entries[slot].unwrap_or(Value::Unit);
            set_push(&mut set, hash_key(&vm.heap, &vm.meter, e)?, e)?;
        }
        Ok(new_set(vm, set))
    });
    m.vm_function("Set.difference", |vm: &mut Vm, s: Set, other: Set| -> Result<_, Ctl> {
        let set = set_filter(vm, s, |e| Ok(!vm.set_contains(other.raw(), e)?))?;
        Ok(new_set(vm, set))
    });
    m.vm_function("Set.is_subset", |vm: &mut Vm, s: Set, other: Set| -> Result<bool, Ctl> {
        for &x in s.set(&vm.heap)?.entries.iter().flatten() {
            if !vm.set_contains(other.raw(), x)? {
                return Ok(false);
            }
        }
        Ok(true)
    });
    m.vm_function("Set.copy", |vm: &mut Vm, s: Set| {
        let copy = Obj::Set(Box::new(s.set(&vm.heap)?.clone()));
        alloc_copy(vm, copy).map(Set::from_raw)
    });
    m.vm_function("Set.clear", |vm: &mut Vm, s: Set| clear_container(vm, s.raw()));
}

/// An upper bound of the length of `t` in upper or lower case: case mapping
/// leaves ASCII text's length unchanged, and otherwise at most triples it
/// (e.g. `ΐ`, two bytes, becomes three characters, six bytes, in upper case).
fn case_mapped_bound(t: &str) -> usize {
    if t.is_ascii() { t.len() } else { t.len().saturating_mul(3) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn case_mapping_at_most_triples_the_length() {
        for c in (0..=0x10ffff).filter_map(char::from_u32) {
            let (s, n) = (c.to_string(), c.len_utf8());
            let bound = super::case_mapped_bound(&s);
            assert!(s.to_uppercase().len() <= bound && s.to_lowercase().len() <= bound, "{c:?}");
            assert!(bound <= 3 * n);
        }
    }
}
