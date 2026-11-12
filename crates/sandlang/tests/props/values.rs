use super::{Expect, check, check_prints, int, int_lit, list_str, opt_str, str_lit};
use proptest::prelude::*;
use std::collections::BTreeMap;

const OVERFLOW: &str = "integer overflow";

fn floor_div(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        return Err("division by zero");
    }
    let q = a.checked_div(b).ok_or(OVERFLOW)?;
    if (a % b != 0) && ((a < 0) != (b < 0)) { Ok(q - 1) } else { Ok(q) }
}

fn floor_mod(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        return Err("division by zero");
    }
    let r = a.checked_rem(b).unwrap_or(0);
    Ok(if r != 0 && ((r < 0) != (b < 0)) { r + b } else { r })
}

fn pow(a: i64, b: i64) -> Result<i64, &'static str> {
    if b < 0 {
        return Err("negative exponent");
    }
    match a {
        0 | 1 => Ok(if b == 0 { 1 } else { a }),
        -1 => Ok(if b % 2 == 0 { 1 } else { -1 }),
        _ => u32::try_from(b).ok().and_then(|b| a.checked_pow(b)).ok_or(OVERFLOW),
    }
}

fn shift(a: i64, b: i64, left: bool) -> Result<i64, &'static str> {
    if !(0..64).contains(&b) {
        return Err("shift out of range");
    }
    Ok(if left { a << b } else { a >> b })
}

fn checked_sum(xs: &[i64]) -> Expect {
    xs.iter().try_fold(0i64, |acc, &x| acc.checked_add(x)).map(|s| s.to_string()).ok_or(OVERFLOW)
}

proptest! {
    #[test]
    fn integer_arithmetic_matches_reference(a in int(), b in int()) {
        let ok = |v: Option<i64>| v.map(|v| v.to_string()).ok_or(OVERFLOW);
        let shown = |r: Result<i64, &'static str>| r.map(|v| v.to_string());
        let prints = vec![
            ("a + b".to_string(), ok(a.checked_add(b))),
            ("a - b".into(), ok(a.checked_sub(b))),
            ("a * b".into(), ok(a.checked_mul(b))),
            ("a / b".into(), shown(floor_div(a, b))),
            ("a % b".into(), shown(floor_mod(a, b))),
            ("a ** b".into(), shown(pow(a, b))),
            ("a.pow(b)".into(), shown(pow(a, b))),
            ("a << b".into(), shown(shift(a, b, true))),
            ("a >> b".into(), shown(shift(a, b, false))),
            ("a & b".into(), Ok((a & b).to_string())),
            ("a | b".into(), Ok((a | b).to_string())),
            ("a ^ b".into(), Ok((a ^ b).to_string())),
            ("~a".into(), Ok((!a).to_string())),
            ("-a".into(), ok(a.checked_neg())),
            ("a.abs()".into(), ok(a.checked_abs())),
            ("(a < b, a == b, a >= b)".into(), Ok(format!("({}, {}, {})", a < b, a == b, a >= b))),
            ("(a.min(b), a.max(b), a.sign())".into(), Ok(format!("({}, {}, {})", a.min(b), a.max(b), a.signum()))),
        ];
        check_prints(&format!("let a = {}\nlet b = {}", int_lit(a), int_lit(b)), &prints)?;
    }

    #[test]
    fn float_display_roundtrips(x in any::<f64>().prop_filter("finite", |x| x.is_finite())) {
        let shown = if x.is_sign_negative() { format!("({x:?})") } else { format!("{x:?}") };
        let r = super::run(&format!("let x = {shown}\nprint(x)\nprint(x.to_str().parse_float() == Some(x))\n"))?;
        let mut lines = r.stdout.lines();
        let printed: f64 = lines.next().unwrap().parse().unwrap();
        prop_assert_eq!(printed.to_bits(), x.to_bits());
        prop_assert_eq!(lines.next(), Some("true"));
    }

    #[test]
    fn string_methods_match_rust(s in "[a-zA-Z0-9 ,é\\-_\t\n{}\"\\\\]{0,24}", sep in "[,a ]", needle in "[a-c]{0,2}") {
        let split: Vec<String> = s.split(sep.as_str()).map(str_lit).collect();
        let replaced = if needle.is_empty() { s.clone() } else { s.replace(needle.as_str(), "#") };
        let (sep_lit, needle_lit) = (str_lit(&sep), str_lit(&needle));
        let prints = vec![
            ("s.trim()".to_string(), Ok(s.trim().to_string())),
            ("s.to_upper()".into(), Ok(s.to_uppercase())),
            (format!("s.split({sep_lit})"), Ok(format!("[{}]", split.join(", ")))),
            (format!("s.contains({needle_lit})"), Ok(s.contains(needle.as_str()).to_string())),
            (format!("s.find({needle_lit})"), Ok(opt_str(s.find(needle.as_str()).map(|i| i as i64)))),
            ("s.len()".into(), Ok(s.len().to_string())),
            ("s.char_count()".into(), Ok(s.chars().count().to_string())),
            ("s.reversed()".into(), Ok(s.chars().rev().collect())),
            (format!("s.replace({needle_lit}, \"#\")"), Ok(replaced)),
            (format!("s.starts_with({needle_lit})"), Ok(s.starts_with(needle.as_str()).to_string())),
        ];
        check_prints(&format!("let s = {}", str_lit(&s)), &prints)?;
    }

    #[test]
    fn sorting_matches_rust(xs in prop::collection::vec(int(), 0..40)) {
        let mut sorted = xs.clone();
        sorted.sort();
        let mut keyed: Vec<(i64, usize)> = xs.iter().enumerate().map(|(i, x)| (x.rem_euclid(5), i)).collect();
        keyed.sort_by_key(|p| p.0);
        let order: Vec<i64> = keyed.iter().map(|p| p.1 as i64).collect();
        let mut unique = sorted.clone();
        unique.dedup();
        let prints = vec![
            ("xs.sorted()".to_string(), Ok(list_str(&sorted))),
            ("xs.enumerate().sorted_by(p => p.1 % 5).map(p => p.0)".into(), Ok(list_str(&order))),
            ("xs.unique().len()".into(), Ok(unique.len().to_string())),
            ("xs.min()".into(), Ok(opt_str(xs.iter().min().copied()))),
            ("xs.max()".into(), Ok(opt_str(xs.iter().max().copied()))),
            ("xs.sum()".into(), checked_sum(&xs)),
        ];
        check_prints(&format!("let xs: [int] = {}", list_str(&xs)), &prints)?;
    }

    #[test]
    fn maps_behave_like_an_ordered_model(keys in prop::collection::vec(int(), 1..8), ops in prop::collection::vec((0u8..3, any::<prop::sample::Index>(), int()), 0..80)) {
        let mut model: Vec<(i64, i64)> = Vec::new();
        let mut src = String::from("let m: [int: int] = [:]\nlet out = []\n");
        let mut out: Vec<String> = Vec::new();
        for (op, k, v) in &ops {
            let k = keys[k.index(keys.len())];
            let kl = int_lit(k);
            match op {
                0 => {
                    src.push_str(&format!("m[{kl}] = {}\n", int_lit(*v)));
                    match model.iter_mut().find(|e| e.0 == k) {
                        Some(e) => e.1 = *v,
                        None => model.push((k, *v)),
                    }
                }
                1 => {
                    src.push_str(&format!("out.push(m.remove({kl}).to_str())\n"));
                    let r = model.iter().position(|e| e.0 == k).map(|i| model.remove(i).1);
                    out.push(opt_str(r));
                }
                _ => {
                    src.push_str(&format!("out.push(m.get({kl}).to_str())\n"));
                    out.push(opt_str(model.iter().find(|e| e.0 == k).map(|e| e.1)));
                }
            }
        }
        src.push_str("print(m.entries())\nprint(out.join(\" \"))\nprint(m.len())\n");
        let entries = format!("[{}]", model.iter().map(|(k, v)| format!("({k}, {v})")).collect::<Vec<_>>().join(", "));
        check(&src, &format!("{entries}\n{}\n{}\n", out.join(" "), model.len()), None)?;
    }

    #[test]
    fn base64_matches_reference(data in prop::collection::vec(any::<u8>(), 0..64)) {
        let list = list_str(&data.iter().map(|&b| b as i64).collect::<Vec<_>>());
        let src = format!(
            "import base64\nlet d = bytes.from_list({list})\nlet e = base64.encode(d)\nprint(e)\nprint(base64.decode(e) == Ok(d))\nprint(base64.decode_url(base64.encode_url(d)) == Ok(d))\n"
        );
        check(&src, &format!("{}\ntrue\ntrue\n", reference_base64(&data)), None)?;
    }

    #[test]
    fn uri_component_roundtrip(s in "\\PC{0,20}") {
        let src = format!("import uri\nlet s = {}\nlet e = uri.encode_component(s)\nprint(uri.decode_component(e) == Ok(s))\nprint(e.chars().all(c => c.is_alnum() or \"-_.~%\".contains(c)))\n", str_lit(&s));
        check(&src, "true\ntrue\n", None)?;
    }

    #[test]
    fn json_roundtrip(j in json_value()) {
        let text = render_json(&j);
        let src = format!(
            "import json\nlet j = json.parse({})?\nlet t = json.encode(j)\nprint(t)\nprint(json.parse(t)? == j)\nprint(json.parse(json.encode(j, indent: 2))? == j)\n",
            str_lit(&text)
        );
        check(&src, &format!("{text}\ntrue\ntrue\n"), None)?;
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
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn json_value() -> impl Strategy<Value = J> {
    let leaf = prop_oneof![
        Just(J::Null),
        any::<bool>().prop_map(J::Bool),
        int().prop_map(J::Int),
        any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(J::Float),
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
        // As Sand encodes floats: with a fraction or an exponent.
        J::Float(f) => format!("{f:?}"),
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
