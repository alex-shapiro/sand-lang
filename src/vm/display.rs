//! Display and debug forms of values, and format specifications.

use super::bytecode::Program;
use super::compare::MAX_DEPTH;
use super::heap::Heap;
use super::value::*;
use crate::hir::FormatSpec;
use std::fmt::Write as _;

pub fn fmt_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    format!("{f:?}")
}

pub fn fmt_duration(nanos: i64) -> String {
    if nanos == 0 {
        return "0s".into();
    }
    let units: [(&str, f64); 6] = [("h", 3600e9), ("m", 60e9), ("s", 1e9), ("ms", 1e6), ("us", 1e3), ("ns", 1.0)];
    let abs = (nanos as f64).abs();
    for (name, size) in units {
        if abs >= size || name == "ns" {
            // Exact integer division where possible to avoid float noise.
            let size_i = size as i64;
            if nanos % size_i == 0 {
                return format!("{}{name}", nanos / size_i);
            }
            let v = nanos as f64 / size;
            let mut s = format!("{v}");
            if s.contains('e') {
                s = format!("{v:.6}").trim_end_matches('0').trim_end_matches('.').to_string();
            }
            return format!("{s}{name}");
        }
    }
    format!("{nanos}ns")
}

pub fn escape_str_debug(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn escape_bytes(b: &[u8], out: &mut String) {
    out.push_str("b\"");
    for &x in b {
        match x {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(x as char),
            _ => {
                let _ = write!(out, "\\x{x:02x}");
            }
        }
    }
    out.push('"');
}

enum Work {
    Val(Value, usize, bool),
    Text(&'static str),
    Owned(String),
}

/// Renders a value. `debug` selects the debug form at the top level; nested
/// values always use the debug form.
pub fn render(prog: &Program, heap: &Heap, v: Value, debug: bool, out: &mut String) -> Result<(), String> {
    let mut work = vec![Work::Val(v, 0, debug)];
    while let Some(w) = work.pop() {
        match w {
            Work::Text(t) => out.push_str(t),
            Work::Owned(t) => out.push_str(&t),
            Work::Val(v, depth, debug) => {
                if depth > MAX_DEPTH {
                    return Err("value too deeply nested to display".into());
                }
                if out.len() > 1 << 30 {
                    return Err("value too large to display".into());
                }
                match v {
                    Value::Unit => out.push_str("()"),
                    Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
                    Value::Int(i) => {
                        let _ = write!(out, "{i}");
                    }
                    Value::Float(f) => out.push_str(&fmt_float(f)),
                    Value::Dur(d) => out.push_str(&fmt_duration(d)),
                    Value::Uninit => out.push_str("<uninit>"),
                    Value::Tag(t) => out.push_str(prog.variants.get(t as usize).map(|v| v.name.as_str()).unwrap_or("?")),
                    Value::Obj(_) => {
                        let d = depth + 1;
                        // Pushes items in reverse with separators.
                        let push_seq = |work: &mut Vec<Work>, items: &[Value], open: &'static str, close: &'static str| {
                            work.push(Work::Text(close));
                            for (i, x) in items.iter().enumerate().rev() {
                                work.push(Work::Val(*x, d, true));
                                if i > 0 {
                                    work.push(Work::Text(", "));
                                }
                            }
                            work.push(Work::Text(open));
                        };
                        match heap.get(v) {
                            Obj::Str(s) => {
                                if debug {
                                    escape_str_debug(s, out)
                                } else {
                                    out.push_str(s)
                                }
                            }
                            Obj::Bytes(b) => escape_bytes(b, out),
                            Obj::List(l) => push_seq(&mut work, l, "[", "]"),
                            Obj::Tuple(t) => push_seq(&mut work, t, "(", ")"),
                            Obj::Set(s) => {
                                let items: Vec<Value> = s.entries.iter().flatten().copied().collect();
                                push_seq(&mut work, &items, "Set([", "])");
                            }
                            Obj::Map(m) => {
                                if m.live == 0 {
                                    out.push_str("[:]");
                                } else {
                                    work.push(Work::Text("]"));
                                    let entries: Vec<(Value, Value)> = m.entries.iter().flatten().copied().collect();
                                    for (i, (k, val)) in entries.iter().enumerate().rev() {
                                        work.push(Work::Val(*val, d, true));
                                        work.push(Work::Text(": "));
                                        work.push(Work::Val(*k, d, true));
                                        if i > 0 {
                                            work.push(Work::Text(", "));
                                        }
                                    }
                                    work.push(Work::Text("["));
                                }
                            }
                            Obj::Struct(def, fields) => {
                                let rt = &prog.types[*def as usize];
                                if *def == prog.ids.error_def && !debug {
                                    // Error displays as `kind: message`.
                                    let kind = heap.str(fields[0]);
                                    let msg = heap.str(fields[1]);
                                    let _ = write!(out, "{kind}: {msg}");
                                    continue;
                                }
                                if fields.is_empty() {
                                    let _ = write!(out, "{} {{}}", rt.name);
                                    continue;
                                }
                                work.push(Work::Text(" }"));
                                for (i, x) in fields.iter().enumerate().rev() {
                                    work.push(Work::Val(*x, d, true));
                                    work.push(Work::Owned(format!("{}: ", rt.fields[i])));
                                    if i > 0 {
                                        work.push(Work::Text(", "));
                                    }
                                }
                                work.push(Work::Owned(format!("{} {{ ", rt.name)));
                            }
                            Obj::Variant(id, fields) => {
                                let name = prog.variants.get(*id as usize).map(|v| v.name.clone()).unwrap_or_default();
                                work.push(Work::Text(")"));
                                for (i, x) in fields.iter().enumerate().rev() {
                                    work.push(Work::Val(*x, d, true));
                                    if i > 0 {
                                        work.push(Work::Text(", "));
                                    }
                                }
                                work.push(Work::Owned(format!("{name}(")));
                            }
                            Obj::Closure(p, _) => {
                                let name = &prog.protos[*p as usize].name;
                                if name.starts_with('<') {
                                    out.push_str("<fn>");
                                } else {
                                    let _ = write!(out, "<fn {name}>");
                                }
                            }
                            Obj::NativeFn(n) => {
                                let sig = prog.native_help.get(*n as usize).map(|h| h.signature.as_str()).unwrap_or("fn ?");
                                let name = sig.trim_start_matches("fn ").split('(').next().unwrap_or("?");
                                let _ = write!(out, "<fn {name}>");
                            }
                            Obj::Cell(inner) => work.push(Work::Val(*inner, depth, debug)),
                            Obj::Range(a, b, incl) => {
                                let _ = write!(out, "{a}..{}{b}", if *incl { "=" } else { "" });
                            }
                            Obj::Task(_) => out.push_str("<task>"),
                            Obj::Channel(_) => out.push_str("<channel>"),
                            Obj::Native(n) => {
                                let _ = write!(out, "<{}>", n.type_name());
                            }
                            Obj::TypeDesc(_) => out.push_str("<type>"),
                            Obj::Free(_) => out.push_str("<freed>"),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn to_display(prog: &Program, heap: &Heap, v: Value) -> Result<String, String> {
    // Fast paths for scalars.
    match v {
        Value::Int(i) => return Ok(i.to_string()),
        Value::Float(f) => return Ok(fmt_float(f)),
        Value::Bool(b) => return Ok(if b { "true" } else { "false" }.to_string()),
        _ => {}
    }
    if let Value::Obj(_) = v
        && let Obj::Str(s) = heap.get(v)
    {
        return Ok(s.to_string());
    }
    let mut s = String::new();
    render(prog, heap, v, false, &mut s)?;
    Ok(s)
}

pub fn to_debug(prog: &Program, heap: &Heap, v: Value) -> Result<String, String> {
    let mut s = String::new();
    render(prog, heap, v, true, &mut s)?;
    Ok(s)
}

/// Applies a format specification.
pub fn format_value(prog: &Program, heap: &Heap, v: Value, spec: &FormatSpec) -> Result<String, String> {
    let numeric = matches!(v, Value::Int(_) | Value::Float(_));
    let mut body = match (v, spec.kind) {
        (_, Some('?')) => to_debug(prog, heap, v)?,
        (Value::Int(i), Some(k @ ('x' | 'X' | 'b' | 'o'))) => {
            let mag = i.unsigned_abs();
            let digits = match k {
                'x' => format!("{mag:x}"),
                'X' => format!("{mag:X}"),
                'b' => format!("{mag:b}"),
                _ => format!("{mag:o}"),
            };
            if i < 0 { format!("-{digits}") } else { digits }
        }
        (Value::Float(f), Some('e')) => match spec.precision {
            Some(p) => format!("{f:.p$e}"),
            None => format!("{f:e}"),
        },
        (Value::Float(f), _) => match spec.precision {
            Some(p) if f.is_finite() => format!("{f:.p$}"),
            _ => fmt_float(f),
        },
        _ => {
            let s = to_display(prog, heap, v)?;
            match spec.precision {
                Some(p) if !numeric => s.chars().take(p).collect(),
                _ => s,
            }
        }
    };
    if numeric && spec.sign && !body.starts_with('-') {
        body.insert(0, '+');
    }
    let width = spec.width.unwrap_or(0);
    let len = body.chars().count();
    if len >= width {
        return Ok(body);
    }
    let pad = width - len;
    if spec.zero && numeric && spec.align.is_none() {
        let (sign, digits) = if body.starts_with('-') || body.starts_with('+') { body.split_at(1) } else { ("", body.as_str()) };
        return Ok(format!("{sign}{}{digits}", "0".repeat(pad)));
    }
    let align = spec.align.unwrap_or(if numeric { '>' } else { '<' });
    let fill = spec.fill.to_string();
    Ok(match align {
        '<' => format!("{body}{}", fill.repeat(pad)),
        '>' => format!("{}{body}", fill.repeat(pad)),
        _ => {
            let left = pad / 2;
            format!("{}{body}{}", fill.repeat(left), fill.repeat(pad - left))
        }
    })
}
