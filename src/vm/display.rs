//! Display and debug forms of values, and format specifications.

use super::Ctl;
use super::bytecode::Program;
use super::compare::MAX_DEPTH;
use super::heap::Heap;
use super::meter::{BYTES_PER_UNIT, Budget, Scratch};
use super::value::*;
use crate::hir::FormatSpec;
use std::fmt::Write as _;

pub fn fmt_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
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
    let units: [(&str, f64); 7] = [("d", 86400e9), ("h", 3600e9), ("m", 60e9), ("s", 1e9), ("ms", 1e6), ("us", 1e3), ("ns", 1.0)];
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

/// Pending output of `render`.
enum Work {
    Val(Value, usize, bool),
    Text(&'static str),
    /// Elements `i..` of a list or the fields of a tuple, struct or variant
    /// (with field names for structs), then `close`.
    Seq {
        obj: Value,
        i: usize,
        depth: usize,
        names: Option<u32>,
        close: &'static str,
    },
    /// Entries `i..` of a map (`key: value`) or set, then `close`.
    Entries {
        obj: Value,
        i: usize,
        depth: usize,
        close: &'static str,
    },
}

/// Output beyond this size fails even without a memory limit.
const MAX_OUTPUT: usize = 1 << 30;

/// [`render`], charging `budget` one unit per value plus the bytes written,
/// and checking the output and work stack against the memory limit as they
/// grow. Scratch space grows with the nesting depth, not the size of the value.
pub fn render_metered(prog: &Program, heap: &Heap, budget: &impl Budget, v: Value, debug: bool, out: &mut String) -> Result<(), Ctl> {
    let mut work = vec![Work::Val(v, 0, debug)];
    let mut scratch = Scratch::new();
    let mut charged = out.len();
    while let Some(w) = work.pop() {
        if out.len() - charged >= BYTES_PER_UNIT {
            budget.charge_bytes(out.len() - charged)?;
            charged = out.len();
        }
        scratch.check(heap, out.len() + work.len() * std::mem::size_of::<Work>())?;
        match w {
            Work::Text(t) => out.push_str(t),
            Work::Seq { obj, i, depth, names, close } => {
                let items = heap.fields(obj);
                let items = if items.is_empty() { heap.list(obj) } else { items };
                if i >= items.len() {
                    out.push_str(close);
                    continue;
                }
                if i > 0 {
                    out.push_str(", ");
                }
                if let Some(def) = names {
                    out.push_str(&prog.types[def as usize].fields[i]);
                    out.push_str(": ");
                }
                work.push(Work::Seq { obj, i: i + 1, depth, names, close });
                work.push(Work::Val(items[i], depth, true));
            }
            Work::Entries { obj, i, depth, close } => {
                let next = match heap.get(obj) {
                    Obj::Map(m) => m.entries.iter().enumerate().skip(i).find_map(|(j, e)| e.map(|(k, v)| (j, k, Some(v)))),
                    Obj::Set(s) => s.entries.iter().enumerate().skip(i).find_map(|(j, e)| e.map(|k| (j, k, None))),
                    _ => None,
                };
                let Some((j, k, v)) = next else {
                    out.push_str(close);
                    continue;
                };
                if i > 0 {
                    out.push_str(", ");
                }
                work.push(Work::Entries { obj, i: j + 1, depth, close });
                if let Some(v) = v {
                    work.push(Work::Val(v, depth, true));
                    work.push(Work::Text(": "));
                }
                work.push(Work::Val(k, depth, true));
            }
            Work::Val(v, depth, debug) => {
                if depth > MAX_DEPTH {
                    return Err(Ctl::panic("value too deeply nested to display"));
                }
                if out.len() > MAX_OUTPUT {
                    return Err(Ctl::panic("value too large to display"));
                }
                budget.charge(1)?;
                match v {
                    Value::Unit => out.push_str("()"),
                    Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
                    Value::Int(i) => {
                        let _ = write!(out, "{i}");
                    }
                    Value::Float(f) => out.push_str(&fmt_float(f)),
                    Value::Dur(d) => out.push_str(&fmt_duration(d)),
                    Value::Pair(a, b) => {
                        let _ = write!(out, "({a}, {b})");
                    }
                    Value::Uninit => out.push_str("<uninit>"),
                    Value::Tag(t) => out.push_str(prog.variants.get(t as usize).map(|v| v.name.as_str()).unwrap_or("?")),
                    Value::VUnit(_)
                    | Value::VBool(..)
                    | Value::VInt(..)
                    | Value::VFloat(..)
                    | Value::VDur(..)
                    | Value::VTag(..)
                    | Value::VObj(..) => {
                        let (id, payload) = v.as_inline_variant().unwrap_or((u32::MAX, Value::Unit));
                        out.push_str(prog.variants.get(id as usize).map(|v| v.name.as_str()).unwrap_or("?"));
                        out.push('(');
                        work.push(Work::Text(")"));
                        work.push(Work::Val(payload, depth + 1, true));
                    }
                    Value::Obj(_) => {
                        let d = depth + 1;
                        match heap.get(v) {
                            Obj::Str(_) | Obj::StrBuf(_) => {
                                let s = heap.str(v);
                                scratch.check(heap, out.len() + s.len())?;
                                if debug { escape_str_debug(s, out) } else { out.push_str(s) }
                            }
                            Obj::Bytes(b) => {
                                scratch.check(heap, out.len() + b.len())?;
                                escape_bytes(b, out)
                            }
                            Obj::List(_) => {
                                out.push('[');
                                work.push(Work::Seq { obj: v, i: 0, depth: d, names: None, close: "]" });
                            }
                            Obj::Tuple(_) => {
                                out.push('(');
                                work.push(Work::Seq { obj: v, i: 0, depth: d, names: None, close: ")" });
                            }
                            Obj::Set(_) => {
                                out.push_str("Set([");
                                work.push(Work::Entries { obj: v, i: 0, depth: d, close: "])" });
                            }
                            Obj::Map(m) => {
                                if m.live == 0 {
                                    out.push_str("[:]");
                                } else {
                                    out.push('[');
                                    work.push(Work::Entries { obj: v, i: 0, depth: d, close: "]" });
                                }
                            }
                            Obj::Struct(def, fields) => {
                                let rt = &prog.types[*def as usize];
                                if *def == prog.ids.error_def && !debug {
                                    let kind = heap.str(fields[0]);
                                    let msg = heap.str(fields[1]);
                                    scratch.check(heap, out.len() + kind.len() + msg.len())?;
                                    let _ = write!(out, "{kind}: {msg}");
                                    continue;
                                }
                                if fields.is_empty() {
                                    let _ = write!(out, "{} {{}}", rt.name);
                                    continue;
                                }
                                let _ = write!(out, "{} {{ ", rt.name);
                                work.push(Work::Seq { obj: v, i: 0, depth: d, names: Some(*def), close: " }" });
                            }
                            Obj::Variant(id, _) => {
                                let name = prog.variants.get(*id as usize).map(|v| v.name.as_str()).unwrap_or_default();
                                out.push_str(name);
                                out.push('(');
                                work.push(Work::Seq { obj: v, i: 0, depth: d, names: None, close: ")" });
                            }
                            Obj::Closure(p, _) => {
                                let name = &prog.protos[*p as usize].name;
                                if name.starts_with('<') {
                                    out.push_str("<fn>");
                                } else {
                                    let _ = write!(out, "<fn {name}>");
                                }
                            }
                            Obj::IterFn(_) => out.push_str("<fn>"),
                            Obj::NativeFn(n) => {
                                let name = prog.native_names.get(*n as usize).map_or("?", String::as_str);
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
    budget.charge_bytes(out.len() - charged)?;
    Ok(())
}

/// The display form of `v`, charging `budget` (see [`render_metered`]).
pub fn to_display_metered(prog: &Program, heap: &Heap, budget: &impl Budget, v: Value) -> Result<String, Ctl> {
    match v {
        Value::Int(i) => return Ok(i.to_string()),
        Value::Float(f) => return Ok(fmt_float(f)),
        Value::Bool(b) => return Ok(if b { "true" } else { "false" }.to_string()),
        _ => {}
    }
    if let Value::Obj(_) = v
        && let Obj::Str(s) = heap.get(v)
    {
        budget.charge_bytes(s.len())?;
        return Ok(s.to_string());
    }
    let mut s = String::new();
    render_metered(prog, heap, budget, v, false, &mut s)?;
    Ok(s)
}

/// Appends the display form of `v` to `out`, charging `budget` (see
/// [`render_metered`]).
pub fn write_display_metered(prog: &Program, heap: &Heap, budget: &impl Budget, v: Value, out: &mut String) -> Result<(), Ctl> {
    match v {
        Value::Int(i) => {
            let _ = write!(out, "{i}");
            return Ok(());
        }
        Value::Float(f) => {
            out.push_str(&fmt_float(f));
            return Ok(());
        }
        Value::Bool(b) => {
            out.push_str(if b { "true" } else { "false" });
            return Ok(());
        }
        _ => {}
    }
    if let Value::Obj(_) = v
        && let Obj::Str(s) = heap.get(v)
    {
        budget.charge_bytes(s.len())?;
        out.push_str(s);
        return Ok(());
    }
    render_metered(prog, heap, budget, v, false, out)
}

/// The debug form of `v`, charging `budget` (see [`render_metered`]).
pub fn to_debug_metered(prog: &Program, heap: &Heap, budget: &impl Budget, v: Value) -> Result<String, Ctl> {
    let mut s = String::new();
    render_metered(prog, heap, budget, v, true, &mut s)?;
    Ok(s)
}

/// [`format_value`], charging `budget` and checking large precisions and
/// widths against the memory limit before allocating.
pub fn format_value_metered(prog: &Program, heap: &Heap, budget: &impl Budget, v: Value, spec: &FormatSpec) -> Result<String, Ctl> {
    let numeric = matches!(v, Value::Int(_) | Value::Float(_));
    if let (Value::Float(_), Some(p)) = (v, spec.precision) {
        // `{x:.1000000000}` would print a billion digits.
        reserve(heap, budget, p)?;
    }
    let mut body = match (v, spec.kind) {
        (_, Some('?')) => to_debug_metered(prog, heap, budget, v)?,
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
            let s = to_display_metered(prog, heap, budget, v)?;
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
    reserve(heap, budget, body.len().saturating_add(pad.saturating_mul(spec.fill.len_utf8())))?;
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

/// Charges for and checks the memory limit before building `n` bytes of output.
fn reserve(heap: &Heap, budget: &impl Budget, n: usize) -> Result<(), Ctl> {
    if n > MAX_OUTPUT || heap.would_exceed(n) {
        return Err(Ctl::limit(crate::LimitKind::Memory));
    }
    budget.charge_bytes(n)
}
