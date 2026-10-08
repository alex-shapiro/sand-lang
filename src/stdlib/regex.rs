//! The `regex` module, backed by the `regex` crate.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::{NativeObject, Obj, Value};
use crate::vm::{NResult, Vm, panic};
use ::regex::{Regex, RegexBuilder};
use std::any::Any;

/// Limit on the compiled program and on the lazy DFA cache of each regex.
const SIZE_LIMIT: usize = 10 << 20;

/// Approximate heap cost of one `Match` value (struct plus string).
const MATCH_COST: usize = 128;

/// The opaque `regex.Regex` type.
struct RegexObj {
    re: Regex,
    /// Runtime struct id of `regex.Match`.
    match_def: u32,
}

/// The opaque `regex.Captures` type: one match's groups, copied out of the text.
struct CapturesObj {
    re: Regex,
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
        // The compiled program is not observable; charge for the pattern
        // and a nominal amount for the automaton.
        4096 + 16 * self.re.as_str().len()
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

fn regex_obj(vm: &Vm, v: Value) -> Result<&RegexObj, crate::vm::Ctl> {
    vm.heap.native::<RegexObj>(v).ok_or_else(|| crate::vm::Ctl::Panic("not a Regex".into()))
}

fn captures_obj(vm: &Vm, v: Value) -> Result<&CapturesObj, crate::vm::Ctl> {
    vm.heap.native::<CapturesObj>(v).ok_or_else(|| crate::vm::Ctl::Panic("not a Captures".into()))
}

/// Allocates a `Match { text, start, end }`.
fn new_match(vm: &mut Vm, def: u32, text: &str, start: usize, end: usize) -> Value {
    let t = vm.new_str(text);
    vm.heap.alloc(Obj::Struct(def, Box::new([t, Value::Int(start as i64), Value::Int(end as i64)])))
}

fn new_captures(re: &Regex, caps: &::regex::Captures, match_def: u32) -> CapturesObj {
    let groups = caps.iter().map(|g| g.map(|m| Group { text: m.as_str().into(), start: m.start(), end: m.end() })).collect();
    CapturesObj { re: re.clone(), groups, match_def }
}

/// Replaces the first (or every) match, checking the memory limit as the
/// output grows.
fn replace(vm: &mut Vm, a: &[Value], all: bool) -> NResult {
    let re = &regex_obj(vm, a[0])?.re;
    let (text, rep) = (vm.str(a[1]), vm.str(a[2]));
    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(text) {
        let m = caps.get(0).expect("group 0 always participates");
        out.push_str(&text[last..m.start()]);
        caps.expand(rep, &mut out);
        last = m.end();
        vm.reserve(out.len())?;
        if !all {
            break;
        }
    }
    out.push_str(&text[last..]);
    vm.reserve(out.len())?;
    Ok(vm.new_str(out))
}

pub fn register(r: &mut Registry) {
    r.source("regex", ModKind::Std, include_str!("regex.sand"));

    r.add_builtin("regex.new", |vm, a| {
        let built = RegexBuilder::new(vm.str(a[0])).size_limit(SIZE_LIMIT).dfa_size_limit(SIZE_LIMIT).build();
        let re = match built {
            Ok(re) => re,
            Err(::regex::Error::CompiledTooBig(limit)) => {
                return Ok(vm.err_value("limit", &format!("regex is too large: the compiled program exceeds {limit} bytes")));
            }
            Err(e) => return Ok(vm.err_value("parse", &e.to_string())),
        };
        let Some(match_def) = vm.prog.types.iter().position(|t| t.name == "Match" && t.module == "regex") else {
            return panic("regex.Match is not defined");
        };
        let obj = vm.heap.alloc(Obj::Native(Box::new(RegexObj { re, match_def: match_def as u32 })));
        Ok(vm.ok(obj))
    });
    r.add_builtin("regex.Regex.is_match", |vm, a| {
        let re = &regex_obj(vm, a[0])?.re;
        Ok(Value::Bool(re.is_match(vm.str(a[1]))))
    });
    r.add_builtin("regex.Regex.find", |vm, a| {
        let obj = regex_obj(vm, a[0])?;
        let def = obj.match_def;
        let found = obj.re.find(vm.str(a[1])).map(|m| (m.start(), m.end()));
        let m = found.map(|(s, e)| {
            let text = vm.str(a[1])[s..e].to_string();
            new_match(vm, def, &text, s, e)
        });
        Ok(vm.option(m))
    });
    r.add_builtin("regex.Regex.find_all", |vm, a| {
        let obj = regex_obj(vm, a[0])?;
        let def = obj.match_def;
        let text = vm.str(a[1]);
        let spans: Vec<(usize, usize)> = obj.re.find_iter(text).map(|m| (m.start(), m.end())).collect();
        vm.reserve(spans.iter().map(|(s, e)| MATCH_COST + e - s).sum())?;
        let text = text.to_string();
        let matches = spans.into_iter().map(|(s, e)| new_match(vm, def, &text[s..e], s, e)).collect();
        Ok(vm.heap.new_list(matches))
    });
    r.add_builtin("regex.Regex.captures", |vm, a| {
        let obj = regex_obj(vm, a[0])?;
        let caps = obj.re.captures(vm.str(a[1])).map(|c| new_captures(&obj.re, &c, obj.match_def));
        let v = caps.map(|c| vm.heap.alloc(Obj::Native(Box::new(c))));
        Ok(vm.option(v))
    });
    r.add_builtin("regex.Regex.captures_all", |vm, a| {
        let obj = regex_obj(vm, a[0])?;
        let all: Vec<CapturesObj> = obj.re.captures_iter(vm.str(a[1])).map(|c| new_captures(&obj.re, &c, obj.match_def)).collect();
        vm.reserve(all.iter().map(NativeObject::size).sum())?;
        let items = all.into_iter().map(|c| vm.heap.alloc(Obj::Native(Box::new(c)))).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("regex.Regex.replace", |vm, a| replace(vm, a, false));
    r.add_builtin("regex.Regex.replace_all", |vm, a| replace(vm, a, true));
    r.add_builtin("regex.Regex.split", |vm, a| {
        let re = &regex_obj(vm, a[0])?.re;
        let pieces: Vec<String> = re.split(vm.str(a[1])).map(str::to_string).collect();
        vm.reserve(pieces.iter().map(|p| 64 + p.len()).sum())?;
        let items = pieces.into_iter().map(|p| vm.new_str(p)).collect();
        Ok(vm.heap.new_list(items))
    });
    r.add_builtin("regex.Regex.pattern", |vm, a| {
        let p = regex_obj(vm, a[0])?.re.as_str().to_string();
        Ok(vm.new_str(p))
    });

    r.add_builtin("regex.Captures.get", |vm, a| {
        let caps = captures_obj(vm, a[0])?;
        let group = usize::try_from(a[1].as_int()).ok().and_then(|i| caps.groups.get(i)).and_then(Option::as_ref);
        group_match(vm, caps.match_def, group.map(|g| (g.text.to_string(), g.start, g.end)))
    });
    r.add_builtin("regex.Captures.name", |vm, a| {
        let caps = captures_obj(vm, a[0])?;
        let name = vm.str(a[1]);
        let index = caps.re.capture_names().position(|n| n == Some(name));
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
