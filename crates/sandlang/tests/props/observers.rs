//! Verifies that methods, loops, iterators, and comprehensions over ranges,
//! lists, strings, maps, and sets agree with reference models.

use super::{Expect, check_prints, int, int_lit, list_str, opt_str, str_lit};
use proptest::prelude::*;

const OVERFLOW: &str = "integer overflow";

fn ok(v: impl ToString) -> Expect {
    Ok(v.to_string())
}

fn exact(n: i128) -> Expect {
    i64::try_from(n).map(|n| n.to_string()).map_err(|_| OVERFLOW)
}

fn sum_left(xs: &[i64]) -> Expect {
    xs.iter().try_fold(0i64, |acc, &x| acc.checked_add(x)).map(|s| s.to_string()).ok_or(OVERFLOW)
}

fn product_left(xs: &[i64]) -> Expect {
    xs.iter().try_fold(1i64, |acc, &x| acc.checked_mul(x)).map(|s| s.to_string()).ok_or(OVERFLOW)
}

fn probes(vs: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = vs.iter().flat_map(|&v| [v.saturating_sub(1), v, v.saturating_add(1)]).collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Clone, Copy)]
struct Range {
    start: i64,
    end: i64,
    inclusive: bool,
}

impl Range {
    fn lit(&self) -> String {
        format!("({}..{}{})", int_lit(self.start), if self.inclusive { "=" } else { "" }, int_lit(self.end))
    }

    fn shown(&self) -> String {
        format!("{}..{}{}", self.start, if self.inclusive { "=" } else { "" }, self.end)
    }

    fn bounds(&self) -> Option<(i64, i64)> {
        let last = if self.inclusive { self.end } else { self.end.checked_sub(1)? };
        (self.start <= last).then_some((self.start, last))
    }

    fn len(&self) -> i128 {
        self.bounds().map_or(0, |(a, b)| b as i128 - a as i128 + 1)
    }

    fn elems(&self) -> Vec<i64> {
        self.bounds().map_or(vec![], |(a, b)| (a..=b).collect())
    }

    fn contains(&self, v: i64) -> bool {
        self.bounds().is_some_and(|(a, b)| a <= v && v <= b)
    }
}

fn narrow_range() -> impl Strategy<Value = Range> {
    (int(), -2i64..6, any::<bool>()).prop_map(|(end, width, inclusive)| Range { start: end.saturating_sub(width), end, inclusive })
}

fn wide_range() -> impl Strategy<Value = Range> {
    (int(), int(), any::<bool>()).prop_map(|(start, end, inclusive)| Range { start, end, inclusive })
}

/// Returns constant-time or early-stopping observations that are valid for any
/// range width.
fn range_basics(r: Range) -> Vec<(String, Expect)> {
    let first = r.bounds().map(|b| b.0);
    let last = r.bounds().map(|b| b.1);
    let first3: Vec<i64> = r.bounds().map_or(vec![], |(a, b)| (a..=b).take(3).collect());
    let mut prints = vec![
        ("x".to_string(), ok(r.shown())),
        ("x == ".to_string() + &r.lit(), ok(true)),
        ("x.start()".into(), ok(r.start)),
        ("x.end()".into(), if r.inclusive { exact(r.end as i128 + 1) } else { ok(r.end) }),
        ("x.len()".into(), exact(r.len())),
        ("x.is_empty()".into(), ok(r.bounds().is_none())),
        ("x.first()".into(), ok(opt_str(first))),
        ("x.min()".into(), ok(opt_str(first))),
        ("x.iter().first()".into(), ok(opt_str(first))),
        ("x.find(_ => true)".into(), ok(opt_str(first))),
        (format!("{}.find(_ => true)", r.lit()), ok(opt_str(first))),
        ("x.find_index(_ => true)".into(), ok(opt_str(first.map(|_| 0)))),
        ("x.last()".into(), ok(opt_str(last))),
        ("x.max()".into(), ok(opt_str(last))),
        ("x.any(_ => true)".into(), ok(first.is_some())),
        ("x.iter().take(3).to_list()".into(), ok(list_str(&first3))),
        (format!("{}.iter().take(3).to_list()", r.lit()), ok(list_str(&first3))),
        ("{ let out = []; for i in x { out.push(i); if out.len() == 3 { break } }; out }".into(), ok(list_str(&first3))),
        (format!("{{ let out = []; for i in {} {{ out.push(i); if out.len() == 3 {{ break }} }}; out }}", r.lit()), ok(list_str(&first3))),
        ("x.sum()".into(), r.bounds().map_or(ok(0), |(a, b)| exact(triangle(b as i128) - triangle(a as i128 - 1)))),
        ("x.product()".into(), product(r)),
    ];
    for v in probes(&[r.start, r.end, 0, i64::MIN, i64::MAX]) {
        prints.push((format!("x.contains({})", int_lit(v)), ok(r.contains(v))));
    }
    prints
}

fn triangle(n: i128) -> i128 {
    n * (n + 1) / 2
}

fn product(r: Range) -> Expect {
    let Some((a, b)) = r.bounds() else { return ok(1) };
    if a <= 0 && 0 <= b {
        return ok(0);
    }
    if r.len() > 64 {
        return Err(OVERFLOW);
    }
    let p = (a..=b).try_fold(1i128, |acc, i| acc.checked_mul(i as i128).filter(|p| i64::try_from(*p).is_ok()));
    p.map_or(Err(OVERFLOW), exact)
}

fn range_elements(r: Range) -> Vec<(String, Expect)> {
    let xs = r.elems();
    let all = ok(list_str(&xs));
    let n = ok(xs.len());
    let l = r.lit();
    let mut prints = vec![
        ("x.to_list()".to_string(), all.clone()),
        ("x.iter().to_list()".into(), all.clone()),
        ("{ let out = []; for i in x { out.push(i) }; out }".into(), all.clone()),
        (format!("{{ let out = []; for i in {l} {{ out.push(i) }}; out }}"), all.clone()),
        ("[i for i in x]".into(), all.clone()),
        (format!("[i for i in {l}]"), all.clone()),
        ("x.map(i => i)".into(), all.clone()),
        (format!("{l}.map(i => i)"), all.clone()),
        ("x.filter(_ => true)".into(), all.clone()),
        (format!("{l}.iter().filter(_ => true).to_list()"), all.clone()),
        ("x.filter_map(i => Some(i))".into(), all.clone()),
        ("x.flat_map(i => [i])".into(), all.clone()),
        ("x.take_while(_ => true)".into(), all.clone()),
        ("x.skip_while(_ => false)".into(), all.clone()),
        ("x.partition(_ => true).0".into(), all.clone()),
        ("x.iter().enumerate().map(p => p.1).to_list()".into(), all.clone()),
        ("x.reversed()".into(), ok(list_str(&xs.iter().rev().copied().collect::<Vec<_>>()))),
        ("x.iter().skip(1).to_list()".into(), ok(list_str(xs.get(1..).unwrap_or(&[])))),
        ("x.count(_ => true)".into(), n.clone()),
        (format!("{l}.count(_ => true)"), n.clone()),
        ("x.fold(0, (n, _) => n + 1)".into(), n.clone()),
        ("x.iter().count()".into(), n.clone()),
        ("x.iter().last()".into(), ok(opt_str(xs.last().copied()))),
        ("x.iter().nth(1)".into(), ok(opt_str(xs.get(1).copied()))),
        ("x.iter().min()".into(), ok(opt_str(xs.first().copied()))),
        ("x.iter().max()".into(), ok(opt_str(xs.last().copied()))),
        ("x.reduce((_, b) => b)".into(), ok(opt_str(xs.last().copied()))),
        ("x.min_by(_ => 0)".into(), ok(opt_str(xs.first().copied()))),
        ("x.max_by(_ => 0)".into(), ok(opt_str(xs.first().copied()))),
        ("x.max_by(i => -i)".into(), if xs.contains(&i64::MIN) { Err(OVERFLOW) } else { ok(opt_str(xs.first().copied())) }),
        ("x.iter().sum()".into(), sum_left(&xs)),
        (format!("{l}.fold(0, (a, i) => a + i)"), sum_left(&xs)),
        ("x.iter().product()".into(), product_left(&xs)),
    ];
    for k in 1..=3 {
        let stepped = ok(list_str(&xs.iter().step_by(k).copied().collect::<Vec<_>>()));
        prints.push((format!("x.step_by({k})"), stepped.clone()));
        prints.push((format!("x.iter().step_by({k}).to_list()"), stepped));
    }
    for v in probes(&[r.start, r.end]) {
        let (has, vl) = (xs.contains(&v), int_lit(v));
        prints.push((format!("x.any(i => i == {vl})"), ok(has)));
        prints.push((format!("x.all(i => i != {vl})"), ok(!has)));
        prints.push((format!("x.find_index(i => i == {vl})"), ok(opt_str(xs.iter().position(|&x| x == v).map(|i| i as i64)))));
    }
    prints
}

fn list_observations(xs: &[i64]) -> Vec<(String, Expect)> {
    let all = ok(list_str(xs));
    let n = ok(xs.len());
    let len = xs.len() as i64;
    let mut sorted = xs.to_vec();
    sorted.sort();
    let mut unique: Vec<i64> = Vec::new();
    for &x in xs {
        if !unique.contains(&x) {
            unique.push(x);
        }
    }
    let mut dedup = xs.to_vec();
    dedup.dedup();
    let indices: Vec<i64> = (0..len).collect();
    let mut prints = vec![
        ("{ let out = []; for x in xs { out.push(x) }; out }".to_string(), all.clone()),
        ("xs.iter().to_list()".into(), all.clone()),
        ("[x for x in xs]".into(), all.clone()),
        ("xs.map(x => x)".into(), all.clone()),
        ("xs.filter(_ => true)".into(), all.clone()),
        ("xs.iter().filter(_ => true).map(x => x).to_list()".into(), all.clone()),
        ("xs.copy()".into(), all.clone()),
        ("xs[..]".into(), all.clone()),
        ("{ let out = []; for (_, x) in xs.enumerate() { out.push(x) }; out }".into(), all.clone()),
        ("{ let out = []; for (i, _) in xs.enumerate() { out.push(i) }; out }".into(), ok(list_str(&indices))),
        ("xs.enumerate().map(p => p.0)".into(), ok(list_str(&indices))),
        ("xs.len()".into(), n.clone()),
        ("xs.count(_ => true)".into(), n.clone()),
        ("xs.fold(0, (n, _) => n + 1)".into(), n.clone()),
        ("xs.iter().count()".into(), n.clone()),
        ("xs.is_empty()".into(), ok(xs.is_empty())),
        ("xs.first()".into(), ok(opt_str(xs.first().copied()))),
        ("xs.iter().first()".into(), ok(opt_str(xs.first().copied()))),
        ("xs.last()".into(), ok(opt_str(xs.last().copied()))),
        ("xs.iter().last()".into(), ok(opt_str(xs.last().copied()))),
        ("xs.min()".into(), ok(opt_str(xs.iter().min().copied()))),
        ("xs.iter().min()".into(), ok(opt_str(xs.iter().min().copied()))),
        ("xs.reduce((a, b) => if b < a { b } else { a })".into(), ok(opt_str(xs.iter().min().copied()))),
        ("xs.max()".into(), ok(opt_str(xs.iter().max().copied()))),
        ("xs.iter().max()".into(), ok(opt_str(xs.iter().max().copied()))),
        ("xs.reversed()".into(), ok(list_str(&xs.iter().rev().copied().collect::<Vec<_>>()))),
        ("{ let ys = xs.copy(); ys.reverse(); ys }".into(), ok(list_str(&xs.iter().rev().copied().collect::<Vec<_>>()))),
        ("xs.sorted()".into(), ok(list_str(&sorted))),
        ("xs.sorted_by(x => x)".into(), ok(list_str(&sorted))),
        ("xs.unique()".into(), ok(list_str(&unique))),
        ("xs.to_set().to_list()".into(), ok(list_str(&unique))),
        ("xs.dedup()".into(), ok(list_str(&dedup))),
        ("xs.sum()".into(), sum_left(xs)),
        ("xs.iter().sum()".into(), sum_left(xs)),
        ("xs.fold(0, (a, x) => a + x)".into(), sum_left(xs)),
        ("xs.product()".into(), product_left(xs)),
        ("xs.iter().product()".into(), product_left(xs)),
    ];
    for i in [-1, 0, 1, len - 1, len, len + 1] {
        let at = usize::try_from(i).ok().and_then(|i| xs.get(i)).copied();
        let clamped = i.clamp(0, len) as usize;
        let il = int_lit(i);
        prints.push((format!("xs.get({il})"), ok(opt_str(at))));
        prints.push((format!("xs[{il}]"), at.map(|x| x.to_string()).ok_or("index out of bounds")));
        prints.push((format!("xs.iter().nth({il})"), ok(opt_str(if i < 0 { None } else { at }))));
        prints.push((format!("xs.take({il})"), ok(list_str(&xs[..clamped]))));
        prints.push((format!("xs.iter().take({il}).to_list()"), ok(list_str(&xs[..clamped]))));
        prints.push((format!("xs.skip({il})"), ok(list_str(&xs[clamped..]))));
        prints.push((format!("xs.iter().skip({il}).to_list()"), ok(list_str(&xs[clamped..]))));
        let slice = if (0..=len).contains(&i) { ok(list_str(&xs[i as usize..])) } else { Err("out of range") };
        prints.push((format!("xs[{il}..]"), slice));
    }
    let mut values = xs.to_vec();
    values.extend([0, i64::MAX]);
    for v in probes(&values) {
        let (pos, vl) = (xs.iter().position(|&x| x == v).map(|i| i as i64), int_lit(v));
        prints.push((format!("xs.contains({vl})"), ok(pos.is_some())));
        prints.push((format!("xs.any(x => x == {vl})"), ok(pos.is_some())));
        prints.push((format!("xs.index_of({vl})"), ok(opt_str(pos))));
        prints.push((format!("xs.find_index(x => x == {vl})"), ok(opt_str(pos))));
        prints.push((format!("xs.iter().find_index(x => x == {vl})"), ok(opt_str(pos))));
        prints.push((format!("xs.count(x => x == {vl})"), ok(xs.iter().filter(|&&x| x == v).count())));
    }
    prints
}

fn string_observations(s: &str) -> Vec<(String, Expect)> {
    let chars: Vec<String> = s.chars().map(|c| str_lit(&c.to_string())).collect();
    let all = ok(format!("[{}]", chars.join(", ")));
    let n = ok(s.chars().count());
    let codes = ok(list_str(&s.chars().map(|c| c as i64).collect::<Vec<_>>()));
    let mut prints = vec![
        ("s.chars()".to_string(), all.clone()),
        ("{ let out = []; for c in s.chars() { out.push(c) }; out }".into(), all.clone()),
        ("[c for c in s.chars()]".into(), all.clone()),
        ("s.chars_iter().to_list()".into(), all.clone()),
        ("s.chars().iter().to_list()".into(), all.clone()),
        ("s.chars().map(c => c)".into(), all.clone()),
        ("{ let out = []; for (_, c) in s.chars().enumerate() { out.push(c) }; out }".into(), all.clone()),
        ("s.char_count()".into(), n.clone()),
        ("s.chars().len()".into(), n.clone()),
        ("s.chars_iter().count()".into(), n.clone()),
        ("s.codepoints().len()".into(), n.clone()),
        ("s.codepoints()".into(), codes.clone()),
        ("s.chars().map(c => c.ord())".into(), codes.clone()),
        ("s.len()".into(), ok(s.len())),
        ("s.bytes().len()".into(), ok(s.len())),
        ("s.reversed()".into(), ok(s.chars().rev().collect::<String>())),
        ("s.chars().reversed().join(\"\")".into(), ok(s.chars().rev().collect::<String>())),
        ("s.chars().join(\"\") == s".into(), ok(true)),
    ];
    for i in -1..=s.len() as i64 + 1 {
        let c = usize::try_from(i).ok().filter(|&i| s.is_char_boundary(i)).and_then(|i| s[i..].chars().next());
        let shown = c.map_or("None".to_string(), |c| format!("Some({})", str_lit(&c.to_string())));
        prints.push((format!("s.char_at({})", int_lit(i)), ok(shown)));
    }
    prints
}

fn map_observations(entries: &[(i64, i64)]) -> (String, Vec<(String, Expect)>) {
    let mut model: Vec<(i64, i64)> = Vec::new();
    let mut setup = String::from("let m: [int: int] = [:]\n");
    for &(k, v) in entries {
        setup.push_str(&format!("m[{}] = {}\n", int_lit(k), int_lit(v)));
        match model.iter_mut().find(|e| e.0 == k) {
            Some(e) => e.1 = v,
            None => model.push((k, v)),
        }
    }
    let keys: Vec<i64> = model.iter().map(|e| e.0).collect();
    setup.push_str(&format!("let s: Set<int> = Set({})", list_str(&keys)));
    let pairs = ok(format!("[{}]", model.iter().map(|(k, v)| format!("({k}, {v})")).collect::<Vec<_>>().join(", ")));
    let ks = ok(list_str(&keys));
    let mut prints = vec![
        ("m.entries()".to_string(), pairs.clone()),
        ("{ let out = []; for (k, v) in m { out.push((k, v)) }; out }".into(), pairs.clone()),
        ("m.iter().to_list()".into(), pairs.clone()),
        ("[(k, v) for (k, v) in m]".into(), pairs.clone()),
        ("m.keys()".into(), ks.clone()),
        ("m.iter().map(e => e.0).to_list()".into(), ks.clone()),
        ("m.values()".into(), ok(list_str(&model.iter().map(|e| e.1).collect::<Vec<_>>()))),
        ("m.len()".into(), ok(model.len())),
        ("m.iter().count()".into(), ok(model.len())),
        ("s.to_list()".into(), ks.clone()),
        ("{ let out = []; for k in s { out.push(k) }; out }".into(), ks.clone()),
        ("s.iter().to_list()".into(), ks.clone()),
        ("s.len()".into(), ok(keys.len())),
    ];
    for k in probes(&keys) {
        let v = model.iter().find(|e| e.0 == k).map(|e| e.1);
        let kl = int_lit(k);
        prints.push((format!("m.get({kl})"), ok(opt_str(v))));
        prints.push((format!("m.contains({kl})"), ok(v.is_some())));
        prints.push((format!("s.contains({kl})"), ok(v.is_some())));
    }
    (setup, prints)
}

proptest! {
    #[test]
    fn narrow_ranges_agree_with_the_model(r in narrow_range()) {
        let mut prints = range_basics(r);
        prints.extend(range_elements(r));
        check_prints(&format!("let x = {}", r.lit()), &prints)?;
    }

    #[test]
    fn wide_ranges_agree_with_the_model(r in wide_range()) {
        check_prints(&format!("let x = {}", r.lit()), &range_basics(r))?;
    }

    #[test]
    fn lists_agree_with_the_model(xs in prop::collection::vec(int(), 0..8)) {
        check_prints(&format!("let xs: [int] = {}", list_str(&xs)), &list_observations(&xs))?;
    }

    #[test]
    fn strings_agree_with_the_model(s in "[a-zé😀\u{301} ]{0,8}") {
        check_prints(&format!("let s = {}", str_lit(&s)), &string_observations(&s))?;
    }

    #[test]
    fn maps_and_sets_agree_with_the_model(entries in prop::collection::vec((prop_oneof![0i64..4, int()], int()), 0..8)) {
        let (setup, prints) = map_observations(&entries);
        check_prints(&setup, &prints)?;
    }
}
