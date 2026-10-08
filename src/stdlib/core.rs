//! Natives for the prelude and the `task` module.

use crate::engine::Registry;
use crate::vm::compare::{values_cmp, values_eq};
use crate::vm::value::*;
use crate::vm::{BlockOn, Ctl, NResult, Vm, panic};
use std::cmp::Ordering;

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
    r.add_builtin("exit", |_, a| Err(Ctl::Exit(int(a[0]).clamp(i32::MIN as i64, i32::MAX as i64) as i32)));
    r.add_builtin("set", |vm, a| {
        let s = vm.heap.alloc(Obj::Set(Box::default()));
        let items = vm.heap.list(a[0]).to_vec();
        for x in items {
            vm.set_add(s, x).map_err(Ctl::Panic)?;
        }
        Ok(s)
    });

    // Tasks and channels.
    r.add_builtin("Task.wait", |_, a| Err(Ctl::Block(BlockOn::Join(a[0]))));
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
    r.add_builtin("Channel.send", |_, a| Err(Ctl::Block(BlockOn::Send(a[0], a[1]))));
    r.add_builtin("Channel.recv", |_, a| Err(Ctl::Block(BlockOn::Recv(a[0]))));
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
        Err(Ctl::Async(Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_nanos(d)).await;
            Box::new(|_: &mut Vm| Ok(Value::Unit)) as crate::vm::Finisher
        })))
    });
    r.add_builtin("task.yield_now", |_, _| Err(Ctl::Yield));
    r.add_builtin("task.race", |vm, a| {
        let tasks = vm.heap.list(a[0]).to_vec();
        if tasks.is_empty() {
            return panic("task.race called with an empty list");
        }
        Err(Ctl::Block(BlockOn::Race(tasks)))
    });
    r.add_builtin("task.channel", |vm, a| {
        let cap = int(a[0]);
        if cap < 0 {
            return panic("channel capacity must not be negative");
        }
        Ok(vm.heap.alloc(Obj::Channel(Box::new(ChannelObj { capacity: cap as usize, ..Default::default() }))))
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
        let v = c.map(|c| vm.new_str(c.to_string()));
        Ok(vm.option(v))
    });
    r.add_builtin("int.parse", |vm, a| {
        let radix = int(a[1]);
        if !(2..=36).contains(&radix) {
            return panic(format!("radix must be between 2 and 36, found {radix}"));
        }
        let t = vm.str(a[0]);
        let v = if t.starts_with('+') { None } else { i64::from_str_radix(t, radix as u32).ok() };
        Ok(vm.option(v.map(Value::Int)))
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
        if f.is_nan() || f < -9223372036854775808.0 || f >= 9223372036854775808.0 {
            return panic(format!("cannot convert {} to int", crate::vm::display::fmt_float(float(a[0]))));
        }
        Ok(Value::Int(f as i64))
    });
    r.add_builtin("float.to_str_fixed", |vm, a| {
        let d = int(a[1]).clamp(0, 1024) as usize;
        let s = format!("{:.*}", d, float(a[0]));
        Ok(vm.new_str(s))
    });
    r.add_builtin("float.parse", |vm, a| {
        let t = vm.str(a[0]);
        let ok = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
            || matches!(t, "inf" | "-inf" | "NaN");
        let v = if ok { t.parse::<f64>().ok() } else { None };
        Ok(vm.option(v.map(Value::Float)))
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

fn new_str_list(vm: &mut Vm, items: Vec<String>) -> Value {
    let vals: Vec<Value> = items.into_iter().map(|s| vm.new_str(s)).collect();
    vm.heap.new_list(vals)
}

fn char_class(t: &str, f: impl Fn(char) -> bool) -> Value {
    Value::Bool(!t.is_empty() && t.chars().all(f))
}

fn register_str(r: &mut Registry) {
    r.add_builtin("str.len", |vm, a| Ok(Value::Int(vm.str(a[0]).len() as i64)));
    r.add_builtin("str.is_empty", |vm, a| Ok(Value::Bool(vm.str(a[0]).is_empty())));
    r.add_builtin("str.chars", |vm, a| {
        let t = vm.str(a[0]).to_string();
        vm.reserve(t.chars().count() * 64)?;
        let items: Vec<String> = t.chars().map(|c| c.to_string()).collect();
        Ok(new_str_list(vm, items))
    });
    r.add_builtin("str.char_count", |vm, a| Ok(Value::Int(vm.str(a[0]).chars().count() as i64)));
    r.add_builtin("str.codepoints", |vm, a| {
        let items: Vec<Value> = vm.str(a[0]).chars().map(|c| Value::Int(c as i64)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("str.bytes", |vm, a| {
        let b = vm.str(a[0]).as_bytes().to_vec();
        Ok(vm.heap.new_bytes(b))
    });
    r.add_builtin("str.contains", |vm, a| Ok(Value::Bool(vm.str(a[0]).contains(vm.str(a[1])))));
    r.add_builtin("str.starts_with", |vm, a| Ok(Value::Bool(vm.str(a[0]).starts_with(vm.str(a[1])))));
    r.add_builtin("str.ends_with", |vm, a| Ok(Value::Bool(vm.str(a[0]).ends_with(vm.str(a[1])))));
    r.add_builtin("str.find", |vm, a| {
        let v = vm.str(a[0]).find(vm.str(a[1])).map(|i| Value::Int(i as i64));
        Ok(vm.option(v))
    });
    r.add_builtin("str.rfind", |vm, a| {
        let v = vm.str(a[0]).rfind(vm.str(a[1])).map(|i| Value::Int(i as i64));
        Ok(vm.option(v))
    });
    r.add_builtin("str.replace", |vm, a| {
        let (t, from, to) = (vm.str(a[0]), vm.str(a[1]), vm.str(a[2]));
        if from.is_empty() {
            return Ok(a[0]);
        }
        let est = if to.len() > from.len() { t.len() / from.len().max(1) * to.len() + t.len() } else { t.len() };
        vm.reserve(est)?;
        let s = t.replace(from, to);
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.split", |vm, a| {
        let (t, sep) = (vm.str(a[0]), vm.str(a[1]));
        if sep.is_empty() {
            return panic("split with an empty separator");
        }
        let items: Vec<String> = t.split(sep).map(String::from).collect();
        Ok(new_str_list(vm, items))
    });
    r.add_builtin("str.split_once", |vm, a| {
        let parts = vm.str(a[0]).split_once(vm.str(a[1])).map(|(x, y)| (x.to_string(), y.to_string()));
        match parts {
            Some((x, y)) => {
                let xv = vm.new_str(x);
                let yv = vm.new_str(y);
                let t = vm.heap.new_tuple(vec![xv, yv]);
                Ok(vm.some(t))
            }
            None => Ok(vm.none()),
        }
    });
    r.add_builtin("str.split_whitespace", |vm, a| {
        let items: Vec<String> = vm.str(a[0]).split_whitespace().map(String::from).collect();
        Ok(new_str_list(vm, items))
    });
    r.add_builtin("str.lines", |vm, a| {
        let items: Vec<String> = vm.str(a[0]).lines().map(String::from).collect();
        Ok(new_str_list(vm, items))
    });
    r.add_builtin("str.trim", |vm, a| {
        let s = vm.str(a[0]).trim().to_string();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.trim_start", |vm, a| {
        let s = vm.str(a[0]).trim_start().to_string();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.trim_end", |vm, a| {
        let s = vm.str(a[0]).trim_end().to_string();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.strip_prefix", |vm, a| {
        let s = vm.str(a[0]).strip_prefix(vm.str(a[1])).map(String::from);
        let v = s.map(|s| vm.new_str(s));
        Ok(vm.option(v))
    });
    r.add_builtin("str.strip_suffix", |vm, a| {
        let s = vm.str(a[0]).strip_suffix(vm.str(a[1])).map(String::from);
        let v = s.map(|s| vm.new_str(s));
        Ok(vm.option(v))
    });
    r.add_builtin("str.to_upper", |vm, a| {
        let s = vm.str(a[0]).to_uppercase();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.to_lower", |vm, a| {
        let s = vm.str(a[0]).to_lowercase();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.repeat", |vm, a| {
        let n = int(a[1]);
        if n < 0 {
            return panic(format!("negative repeat count: {n}"));
        }
        let total = vm.str(a[0]).len().checked_mul(n as usize).ok_or(Ctl::Limit(crate::LimitKind::Memory))?;
        vm.reserve(total)?;
        let s = vm.str(a[0]).repeat(n as usize);
        Ok(vm.new_str(s))
    });
    fn pad(vm: &mut Vm, a: &[Value], start: bool) -> NResult {
        let width = int(a[1]).max(0) as usize;
        let fill = vm.str(a[2]).to_string();
        let t = vm.str(a[0]).to_string();
        let n = t.chars().count();
        if n >= width || fill.is_empty() {
            return Ok(a[0]);
        }
        vm.reserve((width - n) * fill.len() + t.len())?;
        let padding: String = fill.chars().cycle().take(width - n).collect();
        let s = if start { padding + &t } else { t + &padding };
        Ok(vm.new_str(s))
    }
    r.add_builtin("str.pad_start", |vm, a| pad(vm, a, true));
    r.add_builtin("str.pad_end", |vm, a| pad(vm, a, false));
    r.add_builtin("str.parse_int", |vm, a| {
        let t = vm.str(a[0]);
        let v = if t.starts_with('+') { None } else { t.parse::<i64>().ok() };
        Ok(vm.option(v.map(Value::Int)))
    });
    r.add_builtin("str.parse_float", |vm, a| {
        let t = vm.str(a[0]);
        let ok = !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'));
        let v = if ok { t.parse::<f64>().ok() } else { None };
        Ok(vm.option(v.map(Value::Float)))
    });
    r.add_builtin("str.is_digit", |vm, a| Ok(char_class(vm.str(a[0]), |c| c.is_ascii_digit())));
    r.add_builtin("str.is_alpha", |vm, a| Ok(char_class(vm.str(a[0]), char::is_alphabetic)));
    r.add_builtin("str.is_alnum", |vm, a| Ok(char_class(vm.str(a[0]), char::is_alphanumeric)));
    r.add_builtin("str.is_space", |vm, a| Ok(char_class(vm.str(a[0]), char::is_whitespace)));
    r.add_builtin("str.is_upper", |vm, a| Ok(char_class(vm.str(a[0]), char::is_uppercase)));
    r.add_builtin("str.is_lower", |vm, a| Ok(char_class(vm.str(a[0]), char::is_lowercase)));
    r.add_builtin("str.char_at", |vm, a| {
        let t = vm.str(a[0]);
        let i = int(a[1]);
        let c = if i >= 0 && (i as usize) < t.len() && t.is_char_boundary(i as usize) { t[i as usize..].chars().next() } else { None };
        let v = c.map(|c| vm.new_str(c.to_string()));
        Ok(vm.option(v))
    });
    r.add_builtin("str.ord", |vm, a| {
        let t = vm.str(a[0]);
        let mut it = t.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Ok(Value::Int(c as i64)),
            _ => panic(format!("ord requires a one-character string, found {t:?}")),
        }
    });
    r.add_builtin("str.reversed", |vm, a| {
        let s: String = vm.str(a[0]).chars().rev().collect();
        Ok(vm.new_str(s))
    });
    r.add_builtin("str.join", |vm, a| join(vm, a[1], a[0]));
    r.add_builtin("List.join", |vm, a| join(vm, a[0], a[1]));
}

fn join(vm: &mut Vm, list: Value, sep: Value) -> NResult {
    let sep_s = vm.str(sep);
    let items = vm.heap.list(list);
    let total: usize = items.iter().map(|x| vm.heap.str(*x).len()).sum::<usize>() + sep_s.len() * items.len();
    vm.reserve(total)?;
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
        let i = int(a[1]);
        let v = if i >= 0 { vm.heap.bytes_of(a[0]).get(i as usize).map(|b| Value::Int(*b as i64)) } else { None };
        Ok(vm.option(v))
    });
    r.add_builtin("bytes.to_list", |vm, a| {
        let items: Vec<Value> = vm.heap.bytes_of(a[0]).iter().map(|b| Value::Int(*b as i64)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("bytes.to_str", |vm, a| match std::str::from_utf8(vm.heap.bytes_of(a[0])) {
        Ok(s) => {
            let s = s.to_string();
            let v = vm.new_str(s);
            Ok(vm.ok(v))
        }
        Err(e) => Ok(vm.err_value("invalid", &format!("invalid UTF-8: {e}"))),
    });
    r.add_builtin("bytes.to_str_lossy", |vm, a| {
        let s = String::from_utf8_lossy(vm.heap.bytes_of(a[0])).into_owned();
        Ok(vm.new_str(s))
    });
    r.add_builtin("bytes.to_hex", |vm, a| {
        let s: String = vm.heap.bytes_of(a[0]).iter().map(|b| format!("{b:02x}")).collect();
        Ok(vm.new_str(s))
    });
    r.add_builtin("bytes.contains", |vm, a| {
        let (h, n) = (vm.heap.bytes_of(a[0]), vm.heap.bytes_of(a[1]));
        Ok(Value::Bool(n.is_empty() || h.windows(n.len()).any(|w| w == n)))
    });
    r.add_builtin("bytes.find", |vm, a| {
        let (h, n) = (vm.heap.bytes_of(a[0]), vm.heap.bytes_of(a[1]));
        let pos = if n.is_empty() { Some(0) } else { h.windows(n.len()).position(|w| w == n) };
        Ok(vm.option(pos.map(|p| Value::Int(p as i64))))
    });
    r.add_builtin("bytes.from_list", |vm, a| {
        let mut out = Vec::new();
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

fn index_check(i: i64, len: usize, inclusive: bool) -> Result<usize, Ctl> {
    let max = if inclusive { len } else { len.saturating_sub(1) };
    if i < 0 || (i as usize) > max || (!inclusive && len == 0) {
        return Err(Ctl::Panic(format!("index out of bounds: index {i}, length {len}")));
    }
    Ok(i as usize)
}

fn sorted_copy(vm: &mut Vm, items: &mut [Value]) {
    let prog = vm.prog.clone();
    let heap = &vm.heap;
    items.sort_by(|a, b| values_cmp(&prog, heap, *a, *b));
}

fn register_list(r: &mut Registry) {
    r.add_builtin("List.len", |vm, a| Ok(Value::Int(list_len(vm, a[0]) as i64)));
    r.add_builtin("List.is_empty", |vm, a| Ok(Value::Bool(list_len(vm, a[0]) == 0)));
    r.add_builtin("List.push", |vm, a| {
        vm.heap.list_push(a[0], a[1]);
        Ok(Value::Unit)
    });
    r.add_builtin("List.pop", |vm, a| {
        let v = vm.heap.list_mut(a[0]).pop();
        Ok(vm.option(v))
    });
    r.add_builtin("List.insert", |vm, a| {
        let n = list_len(vm, a[0]);
        let i = index_check(int(a[1]), n, true)?;
        vm.heap.list_mut(a[0]).insert(i, a[2]);
        vm.heap.bytes += 16;
        Ok(Value::Unit)
    });
    r.add_builtin("List.remove", |vm, a| {
        let n = list_len(vm, a[0]);
        let i = index_check(int(a[1]), n, false)?;
        Ok(vm.heap.list_mut(a[0]).remove(i))
    });
    r.add_builtin("List.extend", |vm, a| {
        let other = vm.heap.list(a[1]).to_vec();
        vm.reserve(other.len() * 16)?;
        vm.heap.bytes += other.len() * 16;
        vm.heap.list_mut(a[0]).extend(other);
        Ok(Value::Unit)
    });
    r.add_builtin("List.clear", |vm, a| {
        vm.heap.list_mut(a[0]).clear();
        Ok(Value::Unit)
    });
    r.add_builtin("List.get", |vm, a| {
        let i = int(a[1]);
        let v = if i >= 0 { vm.heap.list(a[0]).get(i as usize).copied() } else { None };
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
    r.add_builtin("List.contains", |vm, a| {
        let found = vm.heap.list(a[0]).iter().any(|x| values_eq(&vm.heap, *x, a[1]));
        Ok(Value::Bool(found))
    });
    r.add_builtin("List.index_of", |vm, a| {
        let pos = vm.heap.list(a[0]).iter().position(|x| values_eq(&vm.heap, *x, a[1]));
        Ok(vm.option(pos.map(|p| Value::Int(p as i64))))
    });
    r.add_builtin("List.reverse", |vm, a| {
        vm.heap.list_mut(a[0]).reverse();
        Ok(Value::Unit)
    });
    r.add_builtin("List.reversed", |vm, a| {
        let mut v = vm.heap.list(a[0]).to_vec();
        v.reverse();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.sort", |vm, a| {
        let mut v = std::mem::take(vm.heap.list_mut(a[0]));
        sorted_copy(vm, &mut v);
        *vm.heap.list_mut(a[0]) = v;
        Ok(Value::Unit)
    });
    r.add_builtin("List.sorted", |vm, a| {
        let mut v = vm.heap.list(a[0]).to_vec();
        sorted_copy(vm, &mut v);
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
        {
            let prog = vm.prog.clone();
            let heap = &vm.heap;
            idx.sort_by(|&x, &y| values_cmp(&prog, heap, keys[x], keys[y]));
        }
        let sorted: Vec<Value> = idx.into_iter().map(|i| items[i]).collect();
        *vm.heap.list_mut(a[0]) = sorted;
        Ok(Value::Unit)
    });
    r.add_builtin("List.copy", |vm, a| {
        let v = vm.heap.list(a[0]).to_vec();
        vm.reserve(v.len() * 16)?;
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
        let n = (int(a[1]).max(0) as usize).min(l.len());
        let v = l[..n].to_vec();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.skip", |vm, a| {
        let l = vm.heap.list(a[0]);
        let n = (int(a[1]).max(0) as usize).min(l.len());
        let v = l[n..].to_vec();
        Ok(vm.heap.new_list(v))
    });
    r.add_builtin("List.chunks", |vm, a| {
        let n = int(a[1]);
        if n <= 0 {
            return panic(format!("chunk size must be positive, found {n}"));
        }
        let parts: Vec<Vec<Value>> = vm.heap.list(a[0]).chunks(n as usize).map(|c| c.to_vec()).collect();
        let vals: Vec<Value> = parts.into_iter().map(|p| vm.heap.new_list(p)).collect();
        Ok(vm.heap.new_list(vals))
    });
    r.add_builtin("List.windows", |vm, a| {
        let n = int(a[1]);
        if n <= 0 {
            return panic(format!("window size must be positive, found {n}"));
        }
        let l = vm.heap.list(a[0]);
        vm.reserve(l.len().saturating_mul(n as usize).saturating_mul(16))?;
        let parts: Vec<Vec<Value>> = l.windows(n as usize).map(|c| c.to_vec()).collect();
        let vals: Vec<Value> = parts.into_iter().map(|p| vm.heap.new_list(p)).collect();
        Ok(vm.heap.new_list(vals))
    });
    r.add_builtin("List.enumerate", |vm, a| {
        let items = vm.heap.list(a[0]).to_vec();
        let vals: Vec<Value> = items.into_iter().enumerate().map(|(i, x)| vm.heap.new_tuple(vec![Value::Int(i as i64), x])).collect();
        Ok(vm.heap.new_list(vals))
    });
    r.add_builtin("List.zip", |vm, a| {
        let xs = vm.heap.list(a[0]).to_vec();
        let ys = vm.heap.list(a[1]).to_vec();
        let vals: Vec<Value> = xs.into_iter().zip(ys).map(|(x, y)| vm.heap.new_tuple(vec![x, y])).collect();
        Ok(vm.heap.new_list(vals))
    });
    r.add_builtin("List.dedup", |vm, a| {
        let items = vm.heap.list(a[0]).to_vec();
        let mut out: Vec<Value> = Vec::new();
        for x in items {
            if out.last().is_none_or(|l| !values_eq(&vm.heap, *l, x)) {
                out.push(x);
            }
        }
        Ok(vm.heap.new_list(out))
    });
    r.add_builtin("List.unique", |vm, a| {
        let items = vm.heap.list(a[0]).to_vec();
        let set = vm.heap.alloc(Obj::Set(Box::default()));
        let mut out = Vec::new();
        for x in items {
            if vm.set_add(set, x).map_err(Ctl::Panic)? {
                out.push(x);
            }
        }
        Ok(vm.heap.new_list(out))
    });
    r.add_builtin("List.to_set", |vm, a| {
        let items = vm.heap.list(a[0]).to_vec();
        let set = vm.heap.alloc(Obj::Set(Box::default()));
        for x in items {
            vm.set_add(set, x).map_err(Ctl::Panic)?;
        }
        Ok(set)
    });
    fn fold_num(vm: &mut Vm, list: Value, product: bool) -> NResult {
        let items = vm.heap.list(list).to_vec();
        let mut acc = match items.first() {
            Some(Value::Float(_)) => Value::Float(if product { 1.0 } else { 0.0 }),
            _ => Value::Int(if product { 1 } else { 0 }),
        };
        for x in items {
            acc = vm.arith(if product { 2 } else { 0 }, acc, x).map_err(Ctl::Panic)?;
        }
        Ok(acc)
    }
    r.add_builtin("List.sum", |vm, a| fold_num(vm, a[0], false));
    r.add_builtin("List.product", |vm, a| fold_num(vm, a[0], true));
    fn extreme(vm: &mut Vm, list: Value, want: Ordering) -> NResult {
        let items = vm.heap.list(list).to_vec();
        let prog = vm.prog.clone();
        let mut best: Option<Value> = None;
        for x in items {
            best = Some(match best {
                None => x,
                Some(b) => {
                    if values_cmp(&prog, &vm.heap, x, b) == want {
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
        let n = e.saturating_sub(s).max(0) as usize;
        vm.reserve(n.saturating_mul(16))?;
        let items: Vec<Value> = (s..e).map(Value::Int).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("Range.step_by", |vm, a| {
        let (s, e) = range(vm, a[0]);
        let step = int(a[1]);
        if step <= 0 {
            return panic(format!("step must be positive, found {step}"));
        }
        let n = (e.saturating_sub(s).max(0) / step) as usize;
        vm.reserve(n.saturating_mul(16))?;
        let items: Vec<Value> = (s..e).step_by(step as usize).map(Value::Int).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("Range.reversed", |vm, a| {
        let (s, e) = range(vm, a[0]);
        let n = e.saturating_sub(s).max(0) as usize;
        vm.reserve(n.saturating_mul(16))?;
        let items: Vec<Value> = (s..e).rev().map(Value::Int).collect();
        Ok(vm.heap.new_list(items))
    });
}

fn register_map_set(r: &mut Registry) {
    fn map_entries(vm: &Vm, m: Value) -> Vec<(Value, Value)> {
        match vm.heap.get(m) {
            Obj::Map(mo) => mo.entries.iter().flatten().copied().collect(),
            _ => vec![],
        }
    }
    fn set_items(vm: &Vm, s: Value) -> Vec<Value> {
        match vm.heap.get(s) {
            Obj::Set(so) => so.entries.iter().flatten().copied().collect(),
            _ => vec![],
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
        let v = vm.map_get(a[0], a[1]).map_err(Ctl::Panic)?;
        Ok(vm.option(v))
    });
    r.add_builtin("Map.get_or", |vm, a| Ok(vm.map_get(a[0], a[1]).map_err(Ctl::Panic)?.unwrap_or(a[2])));
    r.add_builtin("Map.contains", |vm, a| Ok(Value::Bool(vm.map_get(a[0], a[1]).map_err(Ctl::Panic)?.is_some())));
    r.add_builtin("Map.insert", |vm, a| {
        let old = vm.map_insert(a[0], a[1], a[2]).map_err(Ctl::Panic)?;
        Ok(vm.option(old))
    });
    r.add_builtin("Map.remove", |vm, a| {
        let old = vm.map_remove(a[0], a[1]).map_err(Ctl::Panic)?;
        Ok(vm.option(old))
    });
    r.add_builtin("Map.keys", |vm, a| {
        let ks: Vec<Value> = map_entries(vm, a[0]).into_iter().map(|e| e.0).collect();
        Ok(vm.heap.new_list(ks))
    });
    r.add_builtin("Map.values", |vm, a| {
        let vs: Vec<Value> = map_entries(vm, a[0]).into_iter().map(|e| e.1).collect();
        Ok(vm.heap.new_list(vs))
    });
    r.add_builtin("Map.entries", |vm, a| {
        let es = map_entries(vm, a[0]);
        let vals: Vec<Value> = es.into_iter().map(|(k, v)| vm.heap.new_tuple(vec![k, v])).collect();
        Ok(vm.heap.new_list(vals))
    });
    r.add_builtin("Map.clear", |vm, a| {
        if let Obj::Map(m) = vm.heap.get_mut(a[0]) {
            m.entries.clear();
            m.index.clear();
            m.live = 0;
        }
        Ok(Value::Unit)
    });
    r.add_builtin("Map.copy", |vm, a| {
        let es = map_entries(vm, a[0]);
        let m = vm.heap.alloc(Obj::Map(Box::default()));
        for (k, v) in es {
            vm.map_insert(m, k, v).map_err(Ctl::Panic)?;
        }
        Ok(m)
    });
    r.add_builtin("Map.extend", |vm, a| {
        let es = map_entries(vm, a[1]);
        for (k, v) in es {
            vm.map_insert(a[0], k, v).map_err(Ctl::Panic)?;
        }
        Ok(Value::Unit)
    });
    r.add_builtin("Map.get_or_insert", |vm, a| match vm.map_get(a[0], a[1]).map_err(Ctl::Panic)? {
        Some(v) => Ok(v),
        None => {
            vm.map_insert(a[0], a[1], a[2]).map_err(Ctl::Panic)?;
            Ok(a[2])
        }
    });

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
    r.add_builtin("Set.add", |vm, a| Ok(Value::Bool(vm.set_add(a[0], a[1]).map_err(Ctl::Panic)?)));
    r.add_builtin("Set.remove", |vm, a| Ok(Value::Bool(vm.set_remove(a[0], a[1]).map_err(Ctl::Panic)?)));
    r.add_builtin("Set.contains", |vm, a| Ok(Value::Bool(vm.set_contains(a[0], a[1]).map_err(Ctl::Panic)?)));
    r.add_builtin("Set.to_list", |vm, a| {
        let items = set_items(vm, a[0]);
        Ok(vm.heap.new_list(items))
    });
    fn set_from(vm: &mut Vm, items: Vec<Value>) -> NResult {
        let s = vm.heap.alloc(Obj::Set(Box::default()));
        for x in items {
            vm.set_add(s, x).map_err(Ctl::Panic)?;
        }
        Ok(s)
    }
    r.add_builtin("Set.union", |vm, a| {
        let mut items = set_items(vm, a[0]);
        items.extend(set_items(vm, a[1]));
        set_from(vm, items)
    });
    r.add_builtin("Set.intersection", |vm, a| {
        let mut items = Vec::new();
        for x in set_items(vm, a[0]) {
            if vm.set_contains(a[1], x).map_err(Ctl::Panic)? {
                items.push(x);
            }
        }
        set_from(vm, items)
    });
    r.add_builtin("Set.difference", |vm, a| {
        let mut items = Vec::new();
        for x in set_items(vm, a[0]) {
            if !vm.set_contains(a[1], x).map_err(Ctl::Panic)? {
                items.push(x);
            }
        }
        set_from(vm, items)
    });
    r.add_builtin("Set.is_subset", |vm, a| {
        for x in set_items(vm, a[0]) {
            if !vm.set_contains(a[1], x).map_err(Ctl::Panic)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    r.add_builtin("Set.copy", |vm, a| {
        let items = set_items(vm, a[0]);
        set_from(vm, items)
    });
    r.add_builtin("Set.clear", |vm, a| {
        if let Obj::Set(s) = vm.heap.get_mut(a[0]) {
            s.entries.clear();
            s.index.clear();
            s.live = 0;
        }
        Ok(Value::Unit)
    });
}
