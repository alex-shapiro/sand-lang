//! Property-based tests: the implementation is compared against simple Rust
//! reference models on randomly generated inputs.

mod common;

use common::run;
use proptest::prelude::*;
use std::collections::BTreeMap;

/// Renders a Rust string as a Sand string literal.
fn lit(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Sand's debug form of a string (as printed inside lists).
fn debug_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn list_of_strs(xs: &[String]) -> String {
    format!("[{}]", xs.iter().map(|s| debug_str(s)).collect::<Vec<_>>().join(", "))
}

fn floor_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    let q = a.checked_div(b)?;
    if (a % b != 0) && ((a < 0) != (b < 0)) { q.checked_sub(1) } else { Some(q) }
}

fn floor_mod(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    let r = a.checked_rem(b).unwrap_or(0);
    Some(if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r })
}

fn interesting_i64() -> impl Strategy<Value = i64> {
    prop_oneof![
        -100i64..100,
        any::<i64>(),
        Just(i64::MAX),
        Just(i64::MIN),
        Just(i64::MAX - 1),
        Just(i64::MIN + 1),
        (-3_037_000_499i64..3_037_000_499),
    ]
}

fn int_lit(v: i64) -> String {
    if v == i64::MIN {
        "(-9223372036854775807 - 1)".to_string()
    } else if v < 0 {
        format!("({v})")
    } else {
        v.to_string()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn integer_arithmetic_matches_reference(a in interesting_i64(), b in interesting_i64()) {
        type Op = (&'static str, fn(i64, i64) -> Option<i64>, &'static str);
        let ops: Vec<Op> = vec![
            ("+", |a, b| a.checked_add(b), "integer overflow"),
            ("-", |a, b| a.checked_sub(b), "integer overflow"),
            ("*", |a, b| a.checked_mul(b), "integer overflow"),
            ("/", floor_div, if b == 0 { "division by zero" } else { "integer overflow" }),
            ("%", floor_mod, "division by zero"),
            ("&", |a, b| Some(a & b), ""),
            ("|", |a, b| Some(a | b), ""),
            ("^", |a, b| Some(a ^ b), ""),
        ];
        let mut src = format!("let a = {}\nlet b = {}\n", int_lit(a), int_lit(b));
        let mut expected = String::new();
        let mut failing = None;
        for (op, f, msg) in &ops {
            match f(a, b) {
                Some(v) => {
                    src.push_str(&format!("print(a {op} b)\n"));
                    expected.push_str(&format!("{v}\n"));
                }
                None => failing = Some((*op, *msg)),
            }
        }
        src.push_str("print(a < b)\nprint(a == b)\nprint(a >= b)\n");
        expected.push_str(&format!("{}\n{}\n{}\n", a < b, a == b, a >= b));
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), expected.as_str());
        if let Some((op, msg)) = failing {
            let r = run(&format!("let a = {}\nlet b = {}\nprint(a {op} b)\n", int_lit(a), int_lit(b)));
            match r.result {
                Err(sandlang::Error::Runtime { message, .. }) => prop_assert!(message.contains(msg), "{} {} {}: {}", a, op, b, message),
                other => prop_assert!(false, "expected panic for {} {} {}, got {:?}", a, op, b, other),
            }
        }
    }

    #[test]
    fn float_display_roundtrips(x in any::<f64>().prop_filter("finite", |x| x.is_finite())) {
        let src = format!("let x = {x:?}\nprint(x)\nprint(x.to_str().parse_float() == Some(x))\n");
        let src = if x.is_sign_negative() { src.replacen(&format!("{x:?}"), &format!("({x:?})"), 1) } else { src };
        let r = run(&src);
        let out = r.ok_stdout().to_string();
        let mut lines = out.lines();
        let printed: f64 = lines.next().unwrap().parse().unwrap();
        prop_assert_eq!(printed.to_bits(), x.to_bits());
        prop_assert_eq!(lines.next(), Some("true"));
    }

    #[test]
    fn string_methods_match_rust(s in "[a-zA-Z0-9 ,é\\-_\t\n{}\"\\\\]{0,24}", sep in "[,a ]", needle in "[a-c]{0,2}") {
        let src = format!(
            "let s = {}\nprint(s.trim())\nprint(s.to_upper())\nprint(s.split({}))\nprint(s.contains({}))\nprint(s.find({}))\nprint(s.len())\nprint(s.char_count())\nprint(s.reversed())\nprint(s.replace({}, \"#\"))\nprint(s.starts_with({}))\n",
            lit(&s), lit(&sep), lit(&needle), lit(&needle), lit(&needle), lit(&needle)
        );
        let split: Vec<String> = s.split(sep.as_str()).map(String::from).collect();
        let find = match s.find(needle.as_str()) { Some(i) => format!("Some({i})"), None => "None".into() };
        let replaced = if needle.is_empty() { s.clone() } else { s.replace(needle.as_str(), "#") };
        let expected = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            s.trim(), s.to_uppercase(), list_of_strs(&split), s.contains(needle.as_str()), find,
            s.len(), s.chars().count(), s.chars().rev().collect::<String>(), replaced, s.starts_with(needle.as_str())
        );
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), expected.as_str());
    }

    #[test]
    fn sorting_matches_rust(xs in prop::collection::vec(-50i64..50, 0..60)) {
        let list = format!("[{}]", xs.iter().map(|x| int_lit(*x)).collect::<Vec<_>>().join(", "));
        let src = format!(
            "let xs = {list}\nprint(xs.sorted())\nlet pairs = xs.enumerate().map(p => (p.1 % 5, p.0))\nprint(pairs.sorted_by(p => p.0).map(p => p.1))\nprint(xs.unique().len())\nprint(xs.sum())\nprint(xs.max())\n"
        );
        let mut sorted = xs.clone();
        sorted.sort();
        // Sand's % is floor modulo, which for a positive divisor equals rem_euclid.
        let mut idx: Vec<(i64, usize)> = xs.iter().enumerate().map(|(i, x)| (x.rem_euclid(5), i)).collect();
        idx.sort_by_key(|p| p.0);
        let fmt = |v: &[i64]| format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "));
        let order: Vec<i64> = idx.iter().map(|p| p.1 as i64).collect();
        let mut uniq = xs.clone();
        uniq.sort();
        uniq.dedup();
        let max = match xs.iter().max() { Some(m) => format!("Some({m})"), None => "None".into() };
        let expected = format!("{}\n{}\n{}\n{}\n{}\n", fmt(&sorted), fmt(&order), uniq.len(), xs.iter().sum::<i64>(), max);
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), expected.as_str());
    }

    #[test]
    fn maps_behave_like_an_ordered_model(ops in prop::collection::vec((0u8..3, 0u8..8, -5i64..5), 0..80)) {
        // Model: insertion-ordered map; removal forgets position.
        let mut model: Vec<(u8, i64)> = Vec::new();
        let mut src = String::from("let m: [int: int] = [:]\nvar out = []\n");
        let mut expected_out: Vec<String> = Vec::new();
        for (op, k, v) in &ops {
            match op {
                0 => {
                    src.push_str(&format!("m[{k}] = {}\n", int_lit(*v)));
                    if let Some(e) = model.iter_mut().find(|e| e.0 == *k) { e.1 = *v } else { model.push((*k, *v)) }
                }
                1 => {
                    src.push_str(&format!("out.push(m.remove({k}).to_str())\n"));
                    let r = model.iter().position(|e| e.0 == *k).map(|i| model.remove(i).1);
                    expected_out.push(match r { Some(v) => format!("Some({v})"), None => "None".into() });
                }
                _ => {
                    src.push_str(&format!("out.push(m.get({k}).to_str())\n"));
                    let r = model.iter().find(|e| e.0 == *k).map(|e| e.1);
                    expected_out.push(match r { Some(v) => format!("Some({v})"), None => "None".into() });
                }
            }
        }
        src.push_str("print(m.entries())\nprint(out.join(\" \"))\nprint(m.len())\n");
        let entries = format!("[{}]", model.iter().map(|(k, v)| format!("({k}, {v})")).collect::<Vec<_>>().join(", "));
        let expected = format!("{entries}\n{}\n{}\n", expected_out.join(" "), model.len());
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), expected.as_str());
    }

    #[test]
    fn base64_matches_reference(data in prop::collection::vec(any::<u8>(), 0..64)) {
        let list = format!("[{}]", data.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", "));
        let src = format!(
            "import base64\nlet d = bytes.from_list({list})\nlet e = base64.encode(d)\nprint(e)\nprint(base64.decode(e) == Ok(d))\nprint(base64.decode_url(base64.encode_url(d)) == Ok(d))\n"
        );
        let expected = format!("{}\ntrue\ntrue\n", reference_base64(&data));
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), expected.as_str());
    }

    #[test]
    fn uri_component_roundtrip(s in "\\PC{0,20}") {
        let src = format!("import uri\nlet s = {}\nlet e = uri.encode_component(s)\nprint(uri.decode_component(e) == Ok(s))\nprint(e.chars().all(c => c.is_alnum() or \"-_.~%\".contains(c)))\n", lit(&s));
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), "true\ntrue\n");
    }

    #[test]
    fn json_roundtrip(j in json_value()) {
        let text = render_json(&j);
        let src = format!(
            "import json\nlet j = json.parse({})?\nlet t = json.stringify(j)\nprint(t)\nprint(json.parse(t)? == j)\nprint(json.parse(json.stringify(j, indent: 2))? == j)\n",
            lit(&text)
        );
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), format!("{text}\ntrue\ntrue\n"));
    }

    #[test]
    fn generated_programs_compute_like_rust(prog in program()) {
        let (src, expected) = prog;
        let r = run(&src);
        prop_assert_eq!(r.ok_stdout(), expected.as_str(), "program:\n{}", src);
    }

    #[test]
    fn formatter_is_idempotent_and_preserves_meaning(prog in program()) {
        let (src, expected) = prog;
        let formatted = sandlang::fmt::format_source(&src).unwrap();
        let again = sandlang::fmt::format_source(&formatted).unwrap();
        prop_assert_eq!(&again, &formatted);
        let r = run(&formatted);
        prop_assert_eq!(r.ok_stdout(), expected.as_str(), "formatted:\n{}", formatted);
    }
}

fn reference_base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
enum J {
    Null,
    Bool(bool),
    Int(i32),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn json_value() -> impl Strategy<Value = J> {
    let leaf = prop_oneof![
        Just(J::Null),
        any::<bool>().prop_map(J::Bool),
        any::<i32>().prop_map(J::Int),
        "[a-z \"\\\\é\n]{0,8}".prop_map(J::Str),
    ];
    leaf.prop_recursive(4, 32, 5, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(J::Arr),
            prop::collection::btree_map("[a-z]{1,4}", inner, 0..5).prop_map(|m: BTreeMap<String, J>| J::Obj(m.into_iter().collect())),
        ]
    })
}

fn render_json(j: &J) -> String {
    match j {
        J::Null => "null".into(),
        J::Bool(b) => b.to_string(),
        J::Int(i) => i.to_string(),
        J::Str(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        J::Arr(xs) => format!("[{}]", xs.iter().map(render_json).collect::<Vec<_>>().join(",")),
        J::Obj(kv) => format!("{{{}}}", kv.iter().map(|(k, v)| format!("\"{k}\":{}", render_json(v))).collect::<Vec<_>>().join(",")),
    }
}

// ---------------------------------------------------------------------------
// Random program generator with a Rust reference evaluator.

#[derive(Debug, Clone)]
enum E {
    Lit(i64),
    Var(usize),
    Add(Box<E>, Box<E>),
    Sub(Box<E>, Box<E>),
    Mul(Box<E>, Box<E>),
    Mod(Box<E>, Box<E>),
    If(Box<E>, Box<E>, Box<E>),
    Call(Box<E>),
    Neg(Box<E>),
}

fn expr(nvars: usize) -> impl Strategy<Value = E> {
    let leaf = prop_oneof![(-20i64..20).prop_map(E::Lit), (0..nvars.max(1)).prop_map(E::Var)];
    leaf.prop_recursive(5, 40, 3, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| E::Add(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| E::Sub(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| E::Mul(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| E::Mod(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone(), inner.clone()).prop_map(|(c, a, b)| E::If(Box::new(c), Box::new(a), Box::new(b))),
            inner.clone().prop_map(|a| E::Call(Box::new(a))),
            inner.prop_map(|a| E::Neg(Box::new(a))),
        ]
    })
}

fn show(e: &E) -> String {
    match e {
        E::Lit(v) => int_lit(*v),
        E::Var(i) => format!("v{i}"),
        E::Add(a, b) => format!("({} + {})", show(a), show(b)),
        E::Sub(a, b) => format!("({} - {})", show(a), show(b)),
        E::Mul(a, b) => format!("({} * {})", show(a), show(b)),
        E::Mod(a, b) => format!("safe_mod({}, {})", show(a), show(b)),
        E::If(c, a, b) => format!("(if {} > 0 {{ {} }} else {{ {} }})", show(c), show(a), show(b)),
        E::Call(a) => format!("clamp({})", show(a)),
        E::Neg(a) => format!("-{}", show(a)),
    }
}

fn clamp(v: i64) -> i64 {
    v.clamp(-1000, 1000)
}

fn eval(e: &E, vars: &[i64]) -> i64 {
    match e {
        E::Lit(v) => *v,
        E::Var(i) => vars[*i],
        E::Add(a, b) => clamp(eval(a, vars) + eval(b, vars)),
        E::Sub(a, b) => clamp(eval(a, vars) - eval(b, vars)),
        E::Mul(a, b) => clamp(eval(a, vars) * eval(b, vars)),
        E::Mod(a, b) => {
            let (x, y) = (eval(a, vars), eval(b, vars));
            if y == 0 { 0 } else { floor_mod(x, y).unwrap() }
        }
        E::If(c, a, b) => {
            if eval(c, vars) > 0 {
                eval(a, vars)
            } else {
                eval(b, vars)
            }
        }
        E::Call(a) => clamp(eval(a, vars)),
        E::Neg(a) => -eval(a, vars),
    }
}

/// Programs: a few `let`/`var` statements, a loop accumulating into a var,
/// and prints. Every arithmetic result is clamped so nothing overflows.
fn program() -> impl Strategy<Value = (String, String)> {
    (prop::collection::vec(expr(3), 3..6), 0i64..6).prop_map(|(exprs, iters)| {
        let mut src = String::from(
            "fn clamp(x: int) -> int { x.clamp(-1000, 1000) }\nfn safe_mod(a: int, b: int) -> int { if b == 0 { 0 } else { a % b } }\n",
        );
        let mut vars: Vec<i64> = Vec::new();
        let mut expected = String::new();
        for (i, e) in exprs.iter().enumerate() {
            // Each expression may only refer to variables defined before it.
            let e = restrict(e, vars.len());
            let shown = show_clamped(&e);
            let v = eval(&e, &vars);
            if i < 3 {
                src.push_str(&format!("let v{i} = {shown}\n"));
                vars.push(v);
            } else {
                src.push_str(&format!("print({shown})\n"));
                expected.push_str(&format!("{v}\n"));
            }
        }
        src.push_str(&format!("var acc = 0\nfor i in 0..{iters} {{\n    acc = clamp(acc + v0 * i - v1)\n}}\nprint(acc)\n"));
        let mut acc = 0i64;
        for i in 0..iters {
            acc = clamp(acc + vars[0] * i - vars[1]);
        }
        expected.push_str(&format!("{acc}\n"));
        (src, expected)
    })
}

fn restrict(e: &E, n: usize) -> E {
    match e {
        E::Var(i) if n == 0 => E::Lit(*i as i64),
        E::Var(i) => E::Var(i % n),
        E::Lit(v) => E::Lit(*v),
        E::Add(a, b) => E::Add(Box::new(restrict(a, n)), Box::new(restrict(b, n))),
        E::Sub(a, b) => E::Sub(Box::new(restrict(a, n)), Box::new(restrict(b, n))),
        E::Mul(a, b) => E::Mul(Box::new(restrict(a, n)), Box::new(restrict(b, n))),
        E::Mod(a, b) => E::Mod(Box::new(restrict(a, n)), Box::new(restrict(b, n))),
        E::If(c, a, b) => E::If(Box::new(restrict(c, n)), Box::new(restrict(a, n)), Box::new(restrict(b, n))),
        E::Call(a) => E::Call(Box::new(restrict(a, n))),
        E::Neg(a) => E::Neg(Box::new(restrict(a, n))),
    }
}

/// Like `show`, but wraps arithmetic in `clamp` to mirror the reference evaluator.
fn show_clamped(e: &E) -> String {
    match e {
        E::Add(a, b) => format!("clamp({} + {})", show_clamped(a), show_clamped(b)),
        E::Sub(a, b) => format!("clamp({} - {})", show_clamped(a), show_clamped(b)),
        E::Mul(a, b) => format!("clamp({} * {})", show_clamped(a), show_clamped(b)),
        E::Mod(a, b) => format!("safe_mod({}, {})", show_clamped(a), show_clamped(b)),
        E::If(c, a, b) => format!("(if {} > 0 {{ {} }} else {{ {} }})", show_clamped(c), show_clamped(a), show_clamped(b)),
        E::Call(a) => format!("clamp({})", show_clamped(a)),
        E::Neg(a) => format!("-{}", show_clamped(a)),
        other => show(other),
    }
}
