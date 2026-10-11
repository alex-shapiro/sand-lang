//! Natives for the prelude and the `task` module.

use crate::engine::Registry;
use crate::vm::compare::{find_key, hash_key, set_slot, try_sort_by, try_sort_numbers, values_cmp_metered, values_eq_metered};
use crate::vm::heap::Heap;
use crate::vm::value::*;
use crate::vm::{BlockOn, Ctl, NResult, Vm, panic};
use std::cmp::Ordering;

/// Approximate heap cost of a small object (a string, tuple or list header)
/// plus the list slot holding it, for reserving memory before building many.
const ITEM: usize = std::mem::size_of::<Obj>() + 2 * std::mem::size_of::<Value>();

/// Approximate heap cost of a list element.
const SLOT: usize = std::mem::size_of::<Value>();

/// Charges for copying `n` bytes into a new value and checks the memory limit.
fn copy_bytes(vm: &Vm, n: usize) -> Result<(), Ctl> {
    vm.reserve(n)?;
    vm.charge_bytes(n)
}

/// Charges for producing `n` new list elements (plus `objs` new objects) and
/// checks the memory limit.
fn produce(vm: &Vm, n: usize, objs: usize) -> Result<(), Ctl> {
    vm.reserve(n.saturating_mul(SLOT).saturating_add(objs.saturating_mul(ITEM)))?;
    vm.charge(n.saturating_add(objs) as u64)
}

fn int(v: Value) -> i64 {
    v.as_int()
}

fn float(v: Value) -> f64 {
    v.as_float()
}

pub fn register(r: &mut Registry) {
    r.source("prelude", crate::check::ModKind::Prelude, include_str!("prelude.sand"));
    r.source("task", crate::check::ModKind::Std, include_str!("task.sand"));

    // Globals.
    r.add_builtin("print", |vm, a| {
        let mut s = vm.display(a[0])?;
        s.push('\n');
        vm.write_stream(1, s.as_bytes())?;
        Ok(Value::Unit)
    });
    r.add_builtin("eprint", |vm, a| {
        let mut s = vm.display(a[0])?;
        s.push('\n');
        vm.write_stream(2, s.as_bytes())?;
        Ok(Value::Unit)
    });
    r.add_builtin("panic", |vm, a| panic(vm.str(a[0]).to_string()));
    r.add_builtin("exit", |_, a| Err(Ctl::exit(int(a[0]).clamp(i32::MIN as i64, i32::MAX as i64) as i32)));
    r.add_builtin("set", |vm, a| {
        let set = set_of(vm, vm.heap.list(a[0]))?;
        Ok(new_set(vm, set))
    });

    // Tasks and channels.
    r.add_builtin("Task.wait", |vm, a| vm.block(BlockOn::Join(a[0])));
    r.add_builtin("Task.is_done", |vm, a| {
        Ok(Value::Bool(match vm.heap.get(a[0]) {
            Obj::Task(t) => t.state != TaskState::Running,
            _ => true,
        }))
    });
    r.add_builtin("Task.cancel", |vm, a| {
        vm.cancel_task(a[0]);
        Ok(Value::Unit)
    });
    r.add_builtin("Channel.send", |vm, a| vm.block(BlockOn::Send(a[0], a[1])));
    r.add_builtin("Channel.recv", |vm, a| vm.block(BlockOn::Recv(a[0])));
    r.add_builtin("Channel.try_recv", |vm, a| {
        let Obj::Channel(c) = vm.heap.get_mut(a[0]) else { return panic("not a channel") };
        let v = c.queue.pop_front();
        // Admit a blocked sender.
        let woken = if v.is_some() { c.senders.pop_front() } else { None };
        if let Some((sf, sg, sv)) = woken {
            if let Obj::Channel(c) = vm.heap.get_mut(a[0]) {
                c.queue.push_back(sv);
            }
            vm.deliver(sf, sg, Value::Bool(true));
        }
        Ok(vm.option(v))
    });
    r.add_builtin("Channel.close", |vm, a| {
        let Obj::Channel(c) = vm.heap.get_mut(a[0]) else { return panic("not a channel") };
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
        Ok(Value::Unit)
    });
    r.add_builtin("Channel.len", |vm, a| {
        Ok(Value::Int(match vm.heap.get(a[0]) {
            Obj::Channel(c) => c.queue.len() as i64,
            _ => 0,
        }))
    });
    r.add_builtin("Channel.is_closed", |vm, a| {
        Ok(Value::Bool(match vm.heap.get(a[0]) {
            Obj::Channel(c) => c.closed,
            _ => true,
        }))
    });
    r.add_builtin("task.sleep", |_, a| {
        let d = int(a[0]).max(0) as u64;
        Err(Ctl::suspend(Box::pin(async move {
            crate::platform::sleep(std::time::Duration::from_nanos(d)).await;
            Box::new(|_: &mut Vm| Ok(Value::Unit)) as crate::vm::Finisher
        })))
    });
    r.add_builtin("task.yield_now", |vm, _| Err(vm.yield_now()));
    r.add_builtin("task.race", |vm, a| {
        let n = vm.heap.list(a[0]).len();
        vm.charge(n as u64)?;
        vm.reserve(std::mem::size_of_val(vm.heap.list(a[0])))?;
        let tasks = vm.heap.list(a[0]).to_vec();
        if tasks.is_empty() {
            return panic("task.race called with an empty list");
        }
        vm.block(BlockOn::Race(tasks))
    });
    r.add_builtin("task.channel", |vm, a| {
        let cap = int(a[0]);
        if cap < 0 {
            return panic("channel capacity must not be negative");
        }
        Ok(vm.heap.alloc(Obj::Channel(Box::new(ChannelObj { capacity: to_usize_saturating(cap), ..Default::default() }))))
    });

    register_numbers(r);
    register_str(r);
    register_bytes(r);
    register_list(r);
    register_map_set(r);
}

fn register_numbers(r: &mut Registry) {
    r.add_builtin("int.to_float", |_, a| Ok(Value::Float(int(a[0]) as f64)));
    r.add_builtin("int.to_str_radix", |vm, a| {
        let radix = int(a[1]);
        if !(2..=36).contains(&radix) {
            return panic(format!("radix must be between 2 and 36, found {radix}"));
        }
        let v = int(a[0]);
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
        let s: String = digits.into_iter().rev().collect();
        Ok(vm.new_str(s))
    });
    r.add_builtin("int.to_char", |vm, a| {
        let c = u32::try_from(int(a[0])).ok().and_then(char::from_u32);
        let v = c.map(|c| vm.short_str(c.encode_utf8(&mut [0; 4])));
        Ok(vm.option(v))
    });
    macro_rules! fl {
        ($name:expr, $f:expr) => {
            r.add_builtin($name, |_, a| Ok(Value::Float($f(float(a[0])))));
        };
    }
    fl!("float.abs", f64::abs);
    fl!("float.floor", f64::floor);
    fl!("float.ceil", f64::ceil);
    fl!("float.round", f64::round);
    fl!("float.trunc", f64::trunc);
    fl!("float.sqrt", f64::sqrt);
    r.add_builtin("float.pow", |_, a| Ok(Value::Float(float(a[0]).powf(float(a[1])))));
    r.add_builtin("float.min", |_, a| Ok(Value::Float(float(a[0]).min(float(a[1])))));
    r.add_builtin("float.max", |_, a| Ok(Value::Float(float(a[0]).max(float(a[1])))));
    r.add_builtin("float.is_nan", |_, a| Ok(Value::Bool(float(a[0]).is_nan())));
    r.add_builtin("float.is_finite", |_, a| Ok(Value::Bool(float(a[0]).is_finite())));
    r.add_builtin("float.is_infinite", |_, a| Ok(Value::Bool(float(a[0]).is_infinite())));
    r.add_builtin("float.to_int", |_, a| {
        let f = float(a[0]).trunc();
        // NaN is in no range.
        if !(-9223372036854775808.0..9223372036854775808.0).contains(&f) {
            return panic(format!("cannot convert {} to int", crate::vm::display::fmt_float(float(a[0]))));
        }
        Ok(Value::Int(f as i64))
    });
    r.add_builtin("float.to_str_fixed", |vm, a| {
        let d = int(a[1]).clamp(0, 1024) as usize;
        let s = format!("{:.*}", d, float(a[0]));
        Ok(vm.new_str(s))
    });
    r.add_builtin("duration.nanos", |_, a| Ok(Value::Int(int(a[0]))));
    r.add_builtin("duration.micros", |_, a| Ok(Value::Int(int(a[0]) / 1_000)));
    r.add_builtin("duration.millis", |_, a| Ok(Value::Int(int(a[0]) / 1_000_000)));
    r.add_builtin("duration.secs", |_, a| Ok(Value::Float(int(a[0]) as f64 / 1e9)));
    r.add_builtin("duration.abs", |_, a| match int(a[0]).checked_abs() {
        Some(v) => Ok(Value::Dur(v)),
        None => panic("duration overflow"),
    });
    r.add_builtin("duration.from_nanos", |_, a| Ok(Value::Dur(int(a[0]))));
    r.add_builtin("duration.from_millis", |_, a| match int(a[0]).checked_mul(1_000_000) {
        Some(v) => Ok(Value::Dur(v)),
        None => panic("duration overflow"),
    });
    r.add_builtin("duration.from_secs", |_, a| {
        let n = float(a[0]) * 1e9;
        if !n.is_finite() || n.abs() > 9.2e18 {
            return panic("duration overflow");
        }
        Ok(Value::Dur(n as i64))
    });
}

/// Splits a string into a `[str]` of new strings (pieces of one ASCII
/// character are shared, see `Vm::short_str`). `split` reads the text from
/// the heap and calls its argument for each piece; it is called twice, to
/// count the pieces and check the memory limit before building them, and to
/// build them. The caller charges for scanning the text.
fn split_list(vm: &mut Vm, text: Value, split: impl for<'h> Fn(&'h Heap, &mut dyn FnMut(&'h str))) -> NResult {
    let mut count = 0usize;
    split(&vm.heap, &mut |_| count += 1);
    produce(vm, count, count)?;
    vm.reserve(vm.str(text).len().saturating_add(count.saturating_mul(ITEM)))?;
    Ok(vm.staged_strs(|heap, st, ascii| {
        st.expect(count + 1);
        let mut items = Vec::with_capacity(count);
        split(heap, &mut |p| {
            items.push(match p.as_bytes() {
                &[b] if b.is_ascii() => ascii[b as usize],
                _ => st.new_str(p),
            })
        });
        st.new_list(items)
    }))
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

fn char_class(vm: &Vm, v: Value, f: impl Fn(char) -> bool) -> NResult {
    let t = vm.str(v);
    vm.charge_bytes(t.len())?;
    Ok(Value::Bool(!t.is_empty() && t.chars().all(f)))
}

fn register_str(r: &mut Registry) {
    r.add_builtin("str.len", |vm, a| Ok(Value::Int(vm.str(a[0]).len() as i64)));
    r.add_builtin("str.is_empty", |vm, a| Ok(Value::Bool(vm.str(a[0]).is_empty())));
    r.add_builtin("str.chars", |vm, a| {
        vm.charge_bytes(vm.str(a[0]).len())?;
        split_list(vm, a[0], |heap, f| {
            let t = heap.str(a[0]);
            t.char_indices().for_each(|(i, c)| f(&t[i..i + c.len_utf8()]))
        })
    });
    r.add_builtin("str.char_count", |vm, a| {
        let t = vm.str(a[0]);
        vm.charge_bytes(t.len())?;
        Ok(Value::Int(t.chars().count() as i64))
    });
    r.add_builtin("str.codepoints", |vm, a| {
        let t = vm.str(a[0]);
        vm.charge_bytes(t.len())?;
        produce(vm, t.len(), 0)?;
        let items: Vec<Value> = t.chars().map(|c| Value::Int(c as i64)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("str.bytes", |vm, a| {
        let t = vm.str(a[0]);
        copy_bytes(vm, t.len())?;
        let b = t.as_bytes().to_vec();
        Ok(vm.heap.new_bytes(b))
    });
    // Searches charge for the needle too: preparing the search scans it,
    // however short the text searched.
    r.add_builtin("str.contains", |vm, a| {
        let (t, p) = (vm.str(a[0]), vm.str(a[1]));
        vm.charge_bytes(t.len().saturating_add(p.len()))?;
        Ok(Value::Bool(t.contains(p)))
    });
    r.add_builtin("str.starts_with", |vm, a| {
        let p = vm.str(a[1]);
        vm.charge_bytes(p.len())?;
        Ok(Value::Bool(vm.str(a[0]).starts_with(p)))
    });
    r.add_builtin("str.ends_with", |vm, a| {
        let p = vm.str(a[1]);
        vm.charge_bytes(p.len())?;
        Ok(Value::Bool(vm.str(a[0]).ends_with(p)))
    });
    r.add_builtin("str.find", |vm, a| {
        let (t, p) = (vm.str(a[0]), vm.str(a[1]));
        vm.charge_bytes(t.len().saturating_add(p.len()))?;
        let v = t.find(p).map(|i| Value::Int(i as i64));
        Ok(vm.option(v))
    });
    r.add_builtin("str.rfind", |vm, a| {
        let (t, p) = (vm.str(a[0]), vm.str(a[1]));
        vm.charge_bytes(t.len().saturating_add(p.len()))?;
        let v = t.rfind(p).map(|i| Value::Int(i as i64));
        Ok(vm.option(v))
    });
    r.add_builtin("str.replace", |vm, a| {
        let (t, from, to) = (vm.str(a[0]), vm.str(a[1]), vm.str(a[2]));
        if from.is_empty() {
            return Ok(a[0]);
        }
        // Count the matches first so the output size is known exactly.
        vm.charge_bytes(t.len().saturating_add(from.len()))?;
        let n = t.matches(from).count();
        let len = t.len() - n * from.len() + n.saturating_mul(to.len());
        copy_bytes(vm, len)?;
        let s = t.replace(from, to);
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.split", |vm, a| {
        let (t, sep) = (vm.str(a[0]), vm.str(a[1]));
        if sep.is_empty() {
            return panic("split with an empty separator");
        }
        vm.charge_bytes(t.len().saturating_add(sep.len()))?;
        split_list(vm, a[0], |heap, f| heap.str(a[0]).split(heap.str(a[1])).for_each(f))
    });
    r.add_builtin("str.split_once", |vm, a| {
        let (t, sep) = (vm.str(a[0]), vm.str(a[1]));
        vm.charge_bytes(t.len().saturating_add(sep.len()))?;
        let Some((x, y)) = t.split_once(sep) else { return Ok(vm.none()) };
        let (x, y) = (span_of(t, x), span_of(t, y));
        copy_bytes(vm, x.len() + y.len())?;
        let (x, y) = (new_substr(vm, a[0], x), new_substr(vm, a[0], y));
        let pair = vm.heap.new_tuple(&[x, y]);
        Ok(vm.some(pair))
    });
    r.add_builtin("str.split_whitespace", |vm, a| {
        vm.charge_bytes(vm.str(a[0]).len())?;
        split_list(vm, a[0], |heap, f| heap.str(a[0]).split_whitespace().for_each(f))
    });
    r.add_builtin("str.lines", |vm, a| {
        vm.charge_bytes(vm.str(a[0]).len())?;
        split_list(vm, a[0], |heap, f| heap.str(a[0]).lines().for_each(f))
    });
    fn substr(vm: &mut Vm, v: Value, f: impl Fn(&str) -> &str) -> NResult {
        let t = vm.str(v);
        vm.charge_bytes(t.len())?;
        let s = f(t);
        if s.len() == t.len() {
            return Ok(v);
        }
        let range = span_of(t, s);
        copy_bytes(vm, range.len())?;
        Ok(new_substr(vm, v, range))
    }
    r.add_builtin("str.trim", |vm, a| substr(vm, a[0], str::trim));
    r.add_builtin("str.trim_start", |vm, a| substr(vm, a[0], str::trim_start));
    r.add_builtin("str.trim_end", |vm, a| substr(vm, a[0], str::trim_end));
    fn strip(vm: &mut Vm, a: &[Value], f: impl Fn(&str, &str) -> Option<usize>) -> NResult {
        let (t, affix) = (vm.str(a[0]), vm.str(a[1]));
        vm.charge_bytes(affix.len())?;
        let Some(start) = f(t, affix) else { return Ok(vm.none()) };
        let range = start..start + t.len() - affix.len();
        copy_bytes(vm, range.len())?;
        let s = new_substr(vm, a[0], range);
        Ok(vm.some(s))
    }
    r.add_builtin("str.strip_prefix", |vm, a| strip(vm, a, |t, p| t.starts_with(p).then_some(p.len())));
    r.add_builtin("str.strip_suffix", |vm, a| strip(vm, a, |t, p| t.ends_with(p).then_some(0)));
    r.add_builtin("str.to_upper", |vm, a| {
        let t = vm.str(a[0]);
        vm.reserve(case_mapped_bound(t))?;
        vm.charge_slow_bytes(t.len())?;
        let s = t.to_uppercase();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.to_lower", |vm, a| {
        let t = vm.str(a[0]);
        vm.reserve(case_mapped_bound(t))?;
        vm.charge_slow_bytes(t.len())?;
        let s = t.to_lowercase();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.repeat", |vm, a| {
        let n = int(a[1]);
        if n < 0 {
            return panic(format!("negative repeat count: {n}"));
        }
        let n = to_usize_saturating(n);
        let total = vm.str(a[0]).len().checked_mul(n).ok_or(Ctl::limit(crate::LimitKind::Memory))?;
        copy_bytes(vm, total)?;
        let s = vm.str(a[0]).repeat(n);
        Ok(vm.new_str(s))
    });
    fn pad(vm: &mut Vm, a: &[Value], start: bool) -> NResult {
        let width = to_usize_saturating(int(a[1]));
        let (t, fill) = (vm.str(a[0]), vm.str(a[2]));
        vm.charge_bytes(t.len())?;
        let n = t.chars().count();
        if n >= width || fill.is_empty() {
            return Ok(a[0]);
        }
        let chars = width - n;
        copy_bytes(vm, chars.saturating_mul(fill.len().min(4)).saturating_add(t.len()))?;
        let padding: String = fill.chars().cycle().take(chars).collect();
        let s = if start { padding + t } else { format!("{t}{padding}") };
        Ok(vm.new_str(s))
    }
    r.add_builtin("str.pad_start", |vm, a| pad(vm, a, true));
    r.add_builtin("str.pad_end", |vm, a| pad(vm, a, false));
    r.add_builtin("str.parse_int", |vm, a| {
        let radix = int(a[1]);
        if !(2..=36).contains(&radix) {
            return panic(format!("radix must be between 2 and 36, found {radix}"));
        }
        let t = vm.str(a[0]);
        vm.charge_bytes(t.len())?;
        let v = if t.starts_with('+') { None } else { i64::from_str_radix(t, radix as u32).ok() };
        Ok(vm.option(v.map(Value::Int)))
    });
    r.add_builtin("str.parse_float", |vm, a| {
        let t = vm.str(a[0]);
        vm.charge_bytes(t.len())?;
        // Decimal and scientific notation, and the non-finite values as
        // floats display them (Rust's parser also takes `infinity`, `nan`, ...).
        let ok = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
            || matches!(t, "inf" | "-inf" | "NaN");
        let v = if ok { t.parse::<f64>().ok() } else { None };
        Ok(vm.option(v.map(Value::Float)))
    });
    r.add_builtin("str.is_digit", |vm, a| char_class(vm, a[0], |c| c.is_ascii_digit()));
    r.add_builtin("str.is_alpha", |vm, a| char_class(vm, a[0], char::is_alphabetic));
    r.add_builtin("str.is_alnum", |vm, a| char_class(vm, a[0], char::is_alphanumeric));
    r.add_builtin("str.is_space", |vm, a| char_class(vm, a[0], char::is_whitespace));
    r.add_builtin("str.is_upper", |vm, a| char_class(vm, a[0], char::is_uppercase));
    r.add_builtin("str.is_lower", |vm, a| char_class(vm, a[0], char::is_lowercase));
    r.add_builtin("str.chars_iter_next", |vm, a| Ok(vm.iter_fn(IterSrc::Chars { s: a[0].handle().unwrap_or(0), i: 0 })));
    r.add_builtin("str.char_at", |vm, a| {
        let t = vm.str(a[0]);
        let c = to_index(int(a[1])).filter(|&i| i < t.len() && t.is_char_boundary(i)).and_then(|i| t[i..].chars().next());
        let v = c.map(|c| vm.short_str(c.encode_utf8(&mut [0; 4])));
        Ok(vm.option(v))
    });
    r.add_builtin("str.ord", |vm, a| {
        let t = vm.str(a[0]);
        let mut it = t.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Ok(Value::Int(c as i64)),
            _ => panic(format!("ord requires a one-character string, found {:?}", super::truncate(t, 64))),
        }
    });
    r.add_builtin("str.reversed", |vm, a| {
        let t = vm.str(a[0]);
        copy_bytes(vm, t.len())?;
        let s: String = t.chars().rev().collect();
        Ok(vm.new_str(s))
    });
    r.add_builtin("List.join", |vm, a| join(vm, a[0], a[1]));
}

fn join(vm: &mut Vm, list: Value, sep: Value) -> NResult {
    let sep_s = vm.str(sep);
    let items = vm.heap.list(list);
    vm.charge(items.len() as u64)?;
    let total = items.iter().fold(sep_s.len().saturating_mul(items.len()), |n, x| n.saturating_add(vm.heap.str(*x).len()));
    copy_bytes(vm, total)?;
    let mut out = String::with_capacity(total);
    for (i, x) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(sep_s);
        }
        out.push_str(vm.heap.str(*x));
    }
    Ok(vm.new_str(out))
}

fn register_bytes(r: &mut Registry) {
    r.add_builtin("bytes.len", |vm, a| Ok(Value::Int(vm.heap.bytes_of(a[0]).len() as i64)));
    r.add_builtin("bytes.is_empty", |vm, a| Ok(Value::Bool(vm.heap.bytes_of(a[0]).is_empty())));
    r.add_builtin("bytes.get", |vm, a| {
        let v = to_index(int(a[1])).and_then(|i| vm.heap.bytes_of(a[0]).get(i)).map(|b| Value::Int(*b as i64));
        Ok(vm.option(v))
    });
    r.add_builtin("bytes.to_list", |vm, a| {
        produce(vm, vm.heap.bytes_of(a[0]).len(), 0)?;
        let items: Vec<Value> = vm.heap.bytes_of(a[0]).iter().map(|b| Value::Int(*b as i64)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("bytes.to_str", |vm, a| {
        copy_bytes(vm, vm.heap.bytes_of(a[0]).len())?;
        match std::str::from_utf8(vm.heap.bytes_of(a[0])) {
            Ok(s) => {
                let s = s.to_string();
                let v = vm.new_str(s);
                Ok(vm.ok(v))
            }
            Err(e) => Ok(vm.err_value("invalid", &format!("invalid UTF-8: {e}"))),
        }
    });
    r.add_builtin("bytes.to_str_lossy", |vm, a| {
        copy_bytes(vm, vm.heap.bytes_of(a[0]).len())?;
        let s = String::from_utf8_lossy(vm.heap.bytes_of(a[0])).into_owned();
        Ok(vm.new_str(s))
    });
    r.add_builtin("bytes.to_hex", |vm, a| {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let b = vm.heap.bytes_of(a[0]);
        vm.reserve(b.len().saturating_mul(2))?;
        vm.charge_slow_bytes(b.len())?;
        let mut s = String::with_capacity(b.len() * 2);
        for x in b {
            s.push(HEX[(x >> 4) as usize] as char);
            s.push(HEX[(x & 15) as usize] as char);
        }
        Ok(vm.new_str(s))
    });
    r.add_builtin("bytes.contains", |vm, a| {
        let pos = find_bytes(vm, vm.heap.bytes_of(a[0]), vm.heap.bytes_of(a[1]))?;
        Ok(Value::Bool(pos.is_some()))
    });
    r.add_builtin("bytes.find", |vm, a| {
        let pos = find_bytes(vm, vm.heap.bytes_of(a[0]), vm.heap.bytes_of(a[1]))?;
        Ok(vm.option(pos.map(|p| Value::Int(p as i64))))
    });
    r.add_builtin("bytes.from_list", |vm, a| {
        let n = list_len(vm, a[0]);
        vm.reserve(n)?;
        vm.charge(n as u64)?;
        let mut out = Vec::with_capacity(n);
        for x in vm.heap.list(a[0]) {
            let i = x.as_int();
            if !(0..=255).contains(&i) {
                return panic(format!("byte out of range: {i}"));
            }
            out.push(i as u8);
        }
        Ok(vm.heap.new_bytes(out))
    });
    r.add_builtin("bytes.from_hex", |vm, a| {
        let t = vm.str(a[0]);
        vm.reserve(t.len() / 2)?;
        vm.charge_slow_bytes(t.len())?;
        let ok = t.len() % 2 == 0 && t.bytes().all(|b| b.is_ascii_hexdigit());
        if !ok {
            return Ok(vm.none());
        }
        let out: Vec<u8> = (0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).unwrap()).collect();
        let b = vm.heap.new_bytes(out);
        Ok(vm.some(b))
    });
}

fn list_len(vm: &Vm, v: Value) -> usize {
    vm.heap.list(v).len()
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
    vm.reserve(needle.len().saturating_mul(std::mem::size_of::<usize>()))?;
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
fn index_of(vm: &Vm, list: Value, x: Value) -> Result<Option<usize>, Ctl> {
    for (i, y) in vm.heap.list(list).iter().enumerate() {
        vm.charge(1)?;
        if values_eq_metered(&vm.heap, &vm.meter, *y, x)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn index_check(i: i64, len: usize, inclusive: bool) -> Result<usize, Ctl> {
    let max = if inclusive { len } else { len.saturating_sub(1) };
    match to_index(i) {
        Some(at) if at <= max && (inclusive || len > 0) => Ok(at),
        _ => Err(Ctl::panic(format!("index out of bounds: index {i}, length {len}"))),
    }
}

/// Sorts values by the language's total order; each comparison is charged
/// to the meter, so sorting a huge list can be interrupted.
fn sort_values(vm: &Vm, items: &mut [Value]) -> Result<(), Ctl> {
    produce(vm, items.len(), 0)?; // scratch for the merge sort
    if let Some(r) = try_sort_numbers(items, &vm.meter) {
        return r;
    }
    let (prog, heap, meter) = (&vm.prog, &vm.heap, &vm.meter);
    try_sort_by(items, |a, b| values_cmp_metered(prog, heap, meter, *a, *b))
}

fn register_list(r: &mut Registry) {
    r.add_builtin("List.len", |vm, a| Ok(Value::Int(list_len(vm, a[0]) as i64)));
    r.add_builtin("List.is_empty", |vm, a| Ok(Value::Bool(list_len(vm, a[0]) == 0)));
    r.add_builtin("List.push", |vm, a| {
        vm.heap.list_push(a[0], a[1]);
        Ok(Value::Unit)
    });
    r.add_builtin("List.iter_next", |vm, a| {
        let n = list_len(vm, a[0]) as i64;
        Ok(vm.iter_fn(IterSrc::List { list: a[0].handle().unwrap_or(0), i: 0, n }))
    });
    r.add_builtin("List.pop", |vm, a| {
        let v = vm.heap.list_shrink(a[0]).and_then(Vec::pop);
        Ok(vm.option(v))
    });
    r.add_builtin("List.insert", |vm, a| {
        let n = list_len(vm, a[0]);
        let i = index_check(int(a[1]), n, true)?;
        vm.charge_bytes((n - i) * SLOT)?;
        let before = vm.heap.get(a[0]).size();
        vm.heap.list_mut(a[0]).insert(i, a[2]);
        let after = vm.heap.get(a[0]).size();
        vm.heap.account(before, after);
        Ok(Value::Unit)
    });
    r.add_builtin("List.remove", |vm, a| {
        let n = list_len(vm, a[0]);
        let i = index_check(int(a[1]), n, false)?;
        vm.charge_bytes((n - i) * SLOT)?;
        Ok(vm.heap.list_mut(a[0]).remove(i))
    });
    r.add_builtin("List.extend", |vm, a| {
        produce(vm, list_len(vm, a[1]), 0)?;
        // A copy: the list may extend itself.
        let other = vm.heap.list(a[1]).to_vec();
        let before = vm.heap.get(a[0]).size();
        vm.heap.list_mut(a[0]).extend(other);
        let after = vm.heap.get(a[0]).size();
        vm.heap.account(before, after);
        Ok(Value::Unit)
    });
    r.add_builtin("List.clear", |vm, a| {
        if let Some(l) = vm.heap.list_shrink(a[0]) {
            l.clear();
        }
        Ok(Value::Unit)
    });
    r.add_builtin("List.get", |vm, a| {
        let v = to_index(int(a[1])).and_then(|i| vm.heap.list(a[0]).get(i)).copied();
        Ok(vm.option(v))
    });
    r.add_builtin("List.first", |vm, a| {
        let v = vm.heap.list(a[0]).first().copied();
        Ok(vm.option(v))
    });
    r.add_builtin("List.last", |vm, a| {
        let v = vm.heap.list(a[0]).last().copied();
        Ok(vm.option(v))
    });
    r.add_builtin("List.contains", |vm, a| Ok(Value::Bool(index_of(vm, a[0], a[1])?.is_some())));
    r.add_builtin("List.index_of", |vm, a| {
        let pos = index_of(vm, a[0], a[1])?;
        Ok(vm.option(pos.map(|p| Value::Int(p as i64))))
    });
    r.add_builtin("List.reverse", |vm, a| {
        vm.charge(list_len(vm, a[0]) as u64)?;
        vm.heap.list_mut(a[0]).reverse();
        Ok(Value::Unit)
    });
    r.add_builtin("List.reversed", |vm, a| {
        produce(vm, list_len(vm, a[0]), 0)?;
        let mut v = vm.heap.list(a[0]).to_vec();
        v.reverse();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.sort", |vm, a| {
        let mut v = std::mem::take(vm.heap.list_mut(a[0]));
        let r = sort_values(vm, &mut v);
        *vm.heap.list_mut(a[0]) = v;
        r.map(|()| Value::Unit)
    });
    r.add_builtin("List.sorted", |vm, a| {
        produce(vm, list_len(vm, a[0]), 0)?;
        let mut v = vm.heap.list(a[0]).to_vec();
        sort_values(vm, &mut v)?;
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.sort_by_keys", |vm, a| {
        let keys = vm.heap.list(a[1]).to_vec();
        let items = std::mem::take(vm.heap.list_mut(a[0]));
        if keys.len() != items.len() {
            *vm.heap.list_mut(a[0]) = items;
            return panic("list changed while computing sort keys");
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
        *vm.heap.list_mut(a[0]) = sorted;
        r.map(|()| Value::Unit)
    });
    r.add_builtin("List.copy", |vm, a| {
        produce(vm, list_len(vm, a[0]), 0)?;
        let v = vm.heap.list(a[0]).to_vec();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.swap", |vm, a| {
        let n = list_len(vm, a[0]);
        let i = index_check(int(a[1]), n, false)?;
        let j = index_check(int(a[2]), n, false)?;
        vm.heap.list_mut(a[0]).swap(i, j);
        Ok(Value::Unit)
    });
    r.add_builtin("List.take", |vm, a| {
        let l = vm.heap.list(a[0]);
        let n = to_usize_saturating(int(a[1])).min(l.len());
        produce(vm, n, 0)?;
        let v = l[..n].to_vec();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.skip", |vm, a| {
        let l = vm.heap.list(a[0]);
        let n = to_usize_saturating(int(a[1])).min(l.len());
        produce(vm, l.len() - n, 0)?;
        let v = l[n..].to_vec();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.chunks", |vm, a| {
        let n = int(a[1]);
        if n <= 0 {
            return panic(format!("chunk size must be positive, found {n}"));
        }
        let (n, len) = (to_usize_saturating(n), list_len(vm, a[0]));
        produce(vm, len, len.div_ceil(n))?;
        Ok(vm.heap.staged(|heap, st| {
            let parts = heap.list(a[0]).chunks(n).map(|c| st.new_list(c.to_vec())).collect();
            st.new_list(parts)
        }))
    });
    r.add_builtin("List.windows", |vm, a| {
        let n = int(a[1]);
        if n <= 0 {
            return panic(format!("window size must be positive, found {n}"));
        }
        let (n, l) = (to_usize_saturating(n), vm.heap.list(a[0]));
        let windows = (l.len() + 1).saturating_sub(n);
        produce(vm, windows.saturating_mul(n), windows)?;
        Ok(vm.heap.staged(|heap, st| {
            let parts = heap.list(a[0]).windows(n).map(|w| st.new_list(w.to_vec())).collect();
            st.new_list(parts)
        }))
    });
    r.add_builtin("List.enumerate", |vm, a| {
        produce(vm, list_len(vm, a[0]), list_len(vm, a[0]))?;
        Ok(vm.heap.staged(|heap, st| {
            let pairs = heap.list(a[0]).iter().enumerate().map(|(i, &x)| st.new_tuple(vec![Value::Int(i as i64), x])).collect();
            st.new_list(pairs)
        }))
    });
    r.add_builtin("List.zip", |vm, a| {
        let n = list_len(vm, a[0]).min(list_len(vm, a[1]));
        produce(vm, n, n)?;
        Ok(vm.heap.staged(|heap, st| {
            let pairs = heap.list(a[0]).iter().zip(heap.list(a[1])).map(|(&x, &y)| st.new_tuple(vec![x, y])).collect();
            st.new_list(pairs)
        }))
    });
    r.add_builtin("List.dedup", |vm, a| {
        produce(vm, 2 * list_len(vm, a[0]), 0)?;
        let items = vm.heap.list(a[0]).to_vec();
        let mut out: Vec<Value> = Vec::new();
        for x in items {
            let dup = match out.last() {
                Some(l) => values_eq_metered(&vm.heap, &vm.meter, *l, x)?,
                None => false,
            };
            if !dup {
                out.push(x);
            }
        }
        Ok(vm.heap.new_list(out))
    });
    r.add_builtin("List.unique", |vm, a| {
        let set = set_of(vm, vm.heap.list(a[0]))?;
        produce(vm, set.live, 0)?;
        let items = set.entries.into_iter().flatten().collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("List.to_set", |vm, a| {
        let set = set_of(vm, vm.heap.list(a[0]))?;
        Ok(new_set(vm, set))
    });
    // Values are erased, so the identity (`T.zero()` / `T.one()`, from the
    // `Num` evidence of the caller) is passed in: an empty `[float]` sums to 0.0.
    fn fold_num(vm: &mut Vm, list: Value, init: Value, product: bool) -> NResult {
        produce(vm, list_len(vm, list), 0)?;
        let items = vm.heap.list(list).to_vec();
        let mut acc = init;
        for x in items {
            acc = vm.arith(if product { 2 } else { 0 }, acc, x)?;
        }
        Ok(acc)
    }
    r.add_builtin("List.sum_from", |vm, a| fold_num(vm, a[0], a[1], false));
    r.add_builtin("List.product_from", |vm, a| fold_num(vm, a[0], a[1], true));
    fn extreme(vm: &mut Vm, list: Value, want: Ordering) -> NResult {
        let items = vm.heap.list(list);
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
        Ok(vm.option(best))
    }
    r.add_builtin("List.min", |vm, a| extreme(vm, a[0], Ordering::Less));
    r.add_builtin("List.max", |vm, a| extreme(vm, a[0], Ordering::Greater));

    // Ranges.
    fn range(vm: &Vm, v: Value) -> (i64, i64) {
        match vm.heap.get(v) {
            Obj::Range(a, b, incl) => (*a, if *incl { b.saturating_add(1) } else { *b }),
            _ => (0, 0),
        }
    }
    r.add_builtin("Range.iter_next", |vm, a| {
        let (i, end) = range(vm, a[0]);
        Ok(vm.iter_fn(IterSrc::Range { i, end }))
    });
    r.add_builtin("Range.start", |vm, a| Ok(Value::Int(range(vm, a[0]).0)));
    r.add_builtin("Range.end", |vm, a| Ok(Value::Int(range(vm, a[0]).1)));
    r.add_builtin("Range.len", |vm, a| {
        let (s, e) = range(vm, a[0]);
        Ok(Value::Int(e.saturating_sub(s).max(0)))
    });
    r.add_builtin("Range.contains", |vm, a| {
        let (s, e) = range(vm, a[0]);
        Ok(Value::Bool((s..e).contains(&int(a[1]))))
    });
    r.add_builtin("Range.to_list", |vm, a| {
        let (s, e) = range(vm, a[0]);
        let n = to_usize_saturating(e.saturating_sub(s));
        produce(vm, n, 0)?;
        let items: Vec<Value> = (s..e).map(Value::Int).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("Range.step_by", |vm, a| {
        let (s, e) = range(vm, a[0]);
        let step = int(a[1]);
        if step <= 0 {
            return panic(format!("step must be positive, found {step}"));
        }
        let n = to_usize_saturating(e.saturating_sub(s) / step);
        produce(vm, n, 0)?;
        // Stepped in `i64`: a step past `usize::MAX` (32-bit targets) must not be truncated.
        let items: Vec<Value> =
            std::iter::successors((s < e).then_some(s), |i| i.checked_add(step).filter(|i| *i < e)).map(Value::Int).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("Range.reversed", |vm, a| {
        let (s, e) = range(vm, a[0]);
        let n = to_usize_saturating(e.saturating_sub(s));
        produce(vm, n, 0)?;
        let items: Vec<Value> = (s..e).rev().map(Value::Int).collect();
        Ok(vm.heap.new_list(items))
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
        return panic("set too large");
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

/// The set of the elements of set `v` that satisfy `keep`, in order.
fn set_filter(vm: &Vm, v: Value, mut keep: impl FnMut(Value) -> Result<bool, Ctl>) -> Result<SetObj, Ctl> {
    let Obj::Set(src) = vm.heap.get(v) else { return Ok(SetObj::default()) };
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

fn new_set(vm: &mut Vm, set: SetObj) -> Value {
    vm.heap.alloc(Obj::Set(Box::new(set)))
}

/// A copy of map or set object `v` (sharing no state): the entries and
/// their index are copied without rehashing any key.
fn copy_container(vm: &mut Vm, v: Value) -> NResult {
    let copy = match vm.heap.get(v) {
        Obj::Map(m) => Obj::Map(m.clone()),
        Obj::Set(s) => Obj::Set(s.clone()),
        _ => return panic("not a map or set"),
    };
    let size = copy.size();
    vm.reserve(size)?;
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
        _ => {}
    }
    let after = vm.heap.get(v).size();
    vm.heap.account(before, after);
    Ok(())
}

fn register_map_set(r: &mut Registry) {
    /// The keys (`0`) or values (`1`) of map `m`, as a list.
    fn map_column(vm: &mut Vm, m: Value, col: usize) -> NResult {
        let Obj::Map(mo) = vm.heap.get(m) else { return Ok(vm.heap.new_list(Vec::new())) };
        produce(vm, mo.live, 0)?;
        let items = mo.entries.iter().flatten().map(|&(k, v)| if col == 0 { k } else { v }).collect();
        Ok(vm.heap.new_list(items))
    }
    fn map_entries(vm: &Vm, m: Value) -> Result<Vec<(Value, Value)>, Ctl> {
        match vm.heap.get(m) {
            Obj::Map(mo) => {
                produce(vm, 2 * mo.live, 0)?;
                Ok(mo.entries.iter().flatten().copied().collect())
            }
            _ => Ok(vec![]),
        }
    }
    fn set_items(vm: &Vm, s: Value) -> &[Option<Value>] {
        match vm.heap.get(s) {
            Obj::Set(so) => &so.entries,
            _ => &[],
        }
    }
    r.add_builtin("Map.len", |vm, a| {
        Ok(Value::Int(match vm.heap.get(a[0]) {
            Obj::Map(m) => m.live as i64,
            _ => 0,
        }))
    });
    r.add_builtin("Map.is_empty", |vm, a| {
        Ok(Value::Bool(match vm.heap.get(a[0]) {
            Obj::Map(m) => m.live == 0,
            _ => true,
        }))
    });
    r.add_builtin("Map.get", |vm, a| {
        let v = vm.map_get(a[0], a[1])?;
        Ok(vm.option(v))
    });
    r.add_builtin("Map.get_or", |vm, a| Ok(vm.map_get(a[0], a[1])?.unwrap_or(a[2])));
    r.add_builtin("Map.contains", |vm, a| Ok(Value::Bool(vm.map_get(a[0], a[1])?.is_some())));
    r.add_builtin("Map.insert", |vm, a| {
        let old = vm.map_insert(a[0], a[1], a[2])?;
        Ok(vm.option(old))
    });
    r.add_builtin("Map.remove", |vm, a| {
        let old = vm.map_remove(a[0], a[1])?;
        Ok(vm.option(old))
    });
    r.add_builtin("Map.iter_next", |vm, a| vm.container_iter(a[0]));
    r.add_builtin("Map.keys", |vm, a| map_column(vm, a[0], 0));
    r.add_builtin("Map.values", |vm, a| map_column(vm, a[0], 1));
    r.add_builtin("Map.entries", |vm, a| {
        let Obj::Map(mo) = vm.heap.get(a[0]) else { return Ok(vm.heap.new_list(Vec::new())) };
        produce(vm, mo.live, mo.live)?;
        Ok(vm.heap.staged(|heap, st| {
            let Obj::Map(mo) = heap.get(a[0]) else { unreachable!() };
            let items = mo.entries.iter().flatten().map(|&(k, v)| st.new_tuple(vec![k, v])).collect();
            st.new_list(items)
        }))
    });
    r.add_builtin("Map.clear", |vm, a| {
        clear_container(vm, a[0])?;
        Ok(Value::Unit)
    });
    r.add_builtin("Map.copy", |vm, a| copy_container(vm, a[0]));
    r.add_builtin("Map.extend", |vm, a| {
        let es = map_entries(vm, a[1])?;
        produce(vm, 4 * es.len(), 0)?;
        for (k, v) in es {
            vm.map_insert(a[0], k, v)?;
        }
        Ok(Value::Unit)
    });
    r.add_builtin("Map.get_or_insert", |vm, a| Ok(vm.map_get_or_insert(a[0], a[1], a[2])?.unwrap_or(a[2])));

    r.add_builtin("Set.len", |vm, a| {
        Ok(Value::Int(match vm.heap.get(a[0]) {
            Obj::Set(s) => s.live as i64,
            _ => 0,
        }))
    });
    r.add_builtin("Set.is_empty", |vm, a| {
        Ok(Value::Bool(match vm.heap.get(a[0]) {
            Obj::Set(s) => s.live == 0,
            _ => true,
        }))
    });
    r.add_builtin("Set.add", |vm, a| Ok(Value::Bool(vm.set_add(a[0], a[1])?)));
    r.add_builtin("Set.remove", |vm, a| Ok(Value::Bool(vm.set_remove(a[0], a[1])?)));
    r.add_builtin("Set.contains", |vm, a| Ok(Value::Bool(vm.set_contains(a[0], a[1])?)));
    r.add_builtin("Set.iter_next", |vm, a| vm.container_iter(a[0]));
    r.add_builtin("Set.to_list", |vm, a| {
        let items = set_items(vm, a[0]);
        produce(vm, items.len(), 0)?;
        let items = items.iter().flatten().copied().collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("Set.union", |vm, a| {
        let Obj::Set(first) = vm.heap.get(a[0]) else { return panic("not a set") };
        let mut set = (**first).clone();
        let more = set_items(vm, a[1]);
        produce(vm, (set.entries.len() + more.len()).saturating_mul(ENTRY / SLOT), 0)?;
        for &x in more.iter().flatten() {
            set_insert(vm, &mut set, x)?;
        }
        Ok(new_set(vm, set))
    });
    r.add_builtin("Set.intersection", |vm, a| {
        let (Obj::Set(x), Obj::Set(y)) = (vm.heap.get(a[0]), vm.heap.get(a[1])) else { return panic("not a set") };
        if x.live <= y.live {
            let set = set_filter(vm, a[0], |e| vm.set_contains(a[1], e))?;
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
    r.add_builtin("Set.difference", |vm, a| {
        let set = set_filter(vm, a[0], |e| Ok(!vm.set_contains(a[1], e)?))?;
        Ok(new_set(vm, set))
    });
    r.add_builtin("Set.is_subset", |vm, a| {
        for &x in set_items(vm, a[0]).iter().flatten() {
            if !vm.set_contains(a[1], x)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    r.add_builtin("Set.copy", |vm, a| copy_container(vm, a[0]));
    r.add_builtin("Set.clear", |vm, a| {
        clear_container(vm, a[0])?;
        Ok(Value::Unit)
    });
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
