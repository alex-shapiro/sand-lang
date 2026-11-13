use super::*;
use crate::engine::{Config, Engine, HostModules, Limits, MemorySink};
use crate::native::NativeModule;
use crate::native::convert::HostError;
use crate::native::ty::{Param, Reflect};
use crate::stdlib::json::Json;
use crate::value::{Encode, StructValue, VariantValue};
use proptest::prelude::*;
use std::sync::{Arc, Mutex};

/// A `host` module that decodes stored `Value` inputs. `decode(i)` decodes
/// input `i` as `T` and runs its computed defaults. `keep(x)` stores the
/// `Vm::to_value` copy of `x`, and `keep_json_form(j)` stores the `Value` that
/// parsing the JSON text of `j` gives. Both return the index of the input.
const HOST: &str = "import json
pub native fn input<reflect T: Decode>(i: int) -> Result<(T, [fn()])>
pub native fn keep<T: Encode>(x: T) -> int
pub native fn keep_json_form(j: json.Json) -> int
pub fn decode<reflect T: Decode>(i: int) -> Result<T> {
    let (value, fills) = input(i)?
    for fill in fills {
        fill()
    }
    Ok(value)
}";

/// Returns the `Value` that parsing the JSON text of `json` gives.
fn json_form(json: &Json) -> Value {
    match json {
        Json::Null => Value::Unit,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Str(s.clone()),
        Json::Arr(items) => Value::List(items.iter().map(json_form).collect()),
        Json::Obj(members) => Value::Map(members.iter().map(|(k, v)| (Value::Str(k.clone()), json_form(v))).collect()),
    }
}

/// A decoded input and the thunks that compute its missing field defaults.
type DecodedInput = (Param<0>, VmVal<Vec<fn()>>);

fn host_module(inputs: Vec<Value>) -> NativeModule {
    let inputs = Arc::new(Mutex::new(inputs));
    let mut m = NativeModule::new("host");
    m.source(HOST);
    let stored = inputs.clone();
    m.function("input", move |vm: &mut Vm, ty: Reflect<0>, i: i64| -> Result<DecodedInput, HostError> {
        let value = stored.lock().unwrap()[i as usize].clone();
        let desc = ty.desc(&vm.heap)?.clone();
        match decode_value(vm, &desc, &value)? {
            Decoded::Done(v) => Ok((Param::from_raw(v), VmVal::from_raw(vm.heap.new_list(Vec::new())))),
            Decoded::Pending(v, fills) => Ok((Param::from_raw(v), fills)),
            Decoded::Failed(e) => Err(HostError::new(e.kind, e.message)),
        }
    });
    let keep = |inputs: &Mutex<Vec<Value>>, value: Value| {
        let mut inputs = inputs.lock().unwrap();
        inputs.push(value);
        inputs.len() as i64 - 1
    };
    let stored = inputs.clone();
    m.function("keep", move |vm: &mut Vm, x: Param<0>| -> Result<i64, Fail> { Ok(keep(&stored, vm.to_value(x.raw())?)) });
    m.function("keep_json_form", move |j: Json| keep(&inputs, json_form(&j)));
    m
}

/// Runs `src` with the `host` module and returns its output, followed by the
/// error of the run, if any.
fn run_with(inputs: Vec<Value>, limits: Limits, src: &str) -> String {
    let out = MemorySink::new();
    let host_modules = HostModules::new([host_module(inputs)]).unwrap();
    let mut engine = Engine::new(Config::new().limits(limits).host_modules(host_modules).stdout(out.clone()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(engine.run_source("main.sand", src));
    format!("{}{}", out.contents(), r.err().map(|e| e.to_string()).unwrap_or_default())
}

fn run(inputs: Vec<Value>, src: &str) -> String {
    run_with(inputs, Limits::default(), src)
}

/// Decodes each input as `ty` and prints the result.
fn decode_each(ty: &str, inputs: Vec<Value>) -> String {
    let n = inputs.len();
    run(inputs, &format!("import host\nfor i in 0..{n} {{\n    let r: Result<{ty}> = host.decode(i)\n    print(r)\n}}"))
}

fn str(s: &str) -> Value {
    Value::Str(s.into())
}

fn map(entries: impl IntoIterator<Item = (Value, Value)>) -> Value {
    Value::Map(entries.into_iter().collect())
}

#[test]
fn values_decode_in_their_exact_and_json_forms() {
    let cases: Vec<(&str, Vec<Value>, &str)> = vec![
        ("int", vec![Value::Int(3), Value::Float(3.0)], "Ok(3)\nOk(3)\n"),
        ("float", vec![Value::Float(1.5), Value::Int(2), Value::Float(f64::NAN)], "Ok(1.5)\nOk(2.0)\nOk(nan)\n"),
        ("duration", vec![Value::Duration(jiff::SignedDuration::from_secs(1)), Value::Int(5)], "Ok(1s)\nOk(5ns)\n"),
        ("bytes", vec![Value::Bytes(vec![1, 2]), str("AQI=")], "Ok(b\"\\x01\\x02\")\nOk(b\"\\x01\\x02\")\n"),
        ("()", vec![Value::Unit], "Ok(())\n"),
        (
            "Set<int>",
            vec![Value::Set(vec![Value::Int(1)]), Value::List(vec![Value::Int(1), Value::Int(1)])],
            "Ok(Set([1]))\nOk(Set([1]))\n",
        ),
        (
            "(int, str)",
            vec![Value::Tuple(vec![Value::Int(1), str("a")]), Value::List(vec![Value::Int(1), str("a")])],
            "Ok((1, \"a\"))\nOk((1, \"a\"))\n",
        ),
        ("[int: str]", vec![map([(Value::Int(1), str("a"))]), map([(str("1"), str("a"))])], "Ok([1: \"a\"])\nOk([1: \"a\"])\n"),
        (
            "Range",
            vec![Value::Range { start: 1, end: 3, inclusive: true }, map([(str("start"), Value::Int(1)), (str("end"), Value::Int(3))])],
            "Ok(1..=3)\nOk(1..3)\n",
        ),
    ];
    for (ty, inputs, expected) in cases {
        assert_eq!(decode_each(ty, inputs), expected, "{ty}");
    }
}

#[test]
fn options_decode_some_none_from_an_exact_value() {
    let none = Value::Variant(VariantValue::new("Option", "None", []));
    let some = |v| Value::Variant(VariantValue::new("Option", "Some", [v]));
    let out =
        decode_each("int??", vec![some(none.clone()), none, Value::Unit, Value::Int(4), some(Value::Int(4)), some(some(Value::Int(4)))]);
    assert_eq!(out, "Ok(Some(None))\nOk(None)\nOk(None)\nOk(Some(Some(4)))\nOk(Some(Some(4)))\nOk(Some(Some(4)))\n");
}

#[test]
fn structs_and_variants_decode_by_name_from_both_forms() {
    let src = "import host
enum Shape { Dot, Circle(float), Rect(w: float, h: float) }
struct Item { name: str, shape: Shape, tags: [str] = [\"new\"], note: str? }
for i in 0..6 {
    let r: Result<Item> = host.decode(i)
    print(r)
}";
    let item = |shape| Value::Struct(StructValue::new("main.Item", [("name", str("a")), ("shape", shape)]));
    let inputs = vec![
        item(Value::Variant(VariantValue::new("main.Shape", "Dot", []))),
        item(Value::Variant(VariantValue::new("main.Shape", "Circle", [Value::Float(1.0)]))),
        item(Value::Variant(VariantValue::with_fields("main.Shape", "Rect", [("h", Value::Int(2)), ("w", Value::Int(1))]))),
        item(Value::Variant(VariantValue::new("main.Shape", "Rect", [Value::Int(1), Value::Int(2)]))),
        map([(str("name"), str("b")), (str("shape"), str("Dot")), (str("extra"), Value::Bytes(vec![]))]),
        map([(str("name"), str("c")), (str("shape"), map([(str("Rect"), map([(str("w"), Value::Int(1)), (str("h"), Value::Int(2))]))]))]),
    ];
    let expected = [
        "Ok(Item(name: \"a\", shape: Dot, tags: [\"new\"], note: None))",
        "Ok(Item(name: \"a\", shape: Circle(1.0), tags: [\"new\"], note: None))",
        "Ok(Item(name: \"a\", shape: Rect(w: 1.0, h: 2.0), tags: [\"new\"], note: None))",
        "Ok(Item(name: \"a\", shape: Rect(w: 1.0, h: 2.0), tags: [\"new\"], note: None))",
        "Ok(Item(name: \"b\", shape: Dot, tags: [\"new\"], note: None))",
        "Ok(Item(name: \"c\", shape: Rect(w: 1.0, h: 2.0), tags: [\"new\"], note: None))",
    ];
    assert_eq!(run(inputs, src), expected.map(|line| format!("{line}\n")).concat());
}

#[test]
fn json_variants_decode_as_their_json_form() {
    let src = "import host
import json
struct Point { x: int, y: int }
let j = json.parse(\"{\\\"x\\\": 1, \\\"y\\\": 2}\").unwrap()
let p: Result<Point> = host.decode(host.keep(j))
print(p)
let back: Result<json.Json> = host.decode(host.keep(Point(x: 3, y: 4)))
print(back)
let same: Result<json.Json> = host.decode(host.keep(j))
print(same == Ok(j))";
    let out = run(Vec::new(), src);
    assert_eq!(out, "Ok(Point(x: 1, y: 2))\nErr(Error(message: \"$: expected Json, found main.Point\", kind: \"parse\"))\ntrue\n");
}

#[test]
fn computed_defaults_and_set_fills_run_after_a_value_decodes() {
    let src = "import host
var calls = 0
fn next() -> int {
    calls += 1
    calls
}
struct Stamp { n: int, at: int = next() }
let r: Result<Set<Stamp>> = host.decode(0)
print(r)
print(calls)";
    let stamp = |n| map([(str("n"), Value::Int(n))]);
    assert_eq!(run(vec![Value::List(vec![stamp(1), stamp(2)])], src), "Ok(Set([Stamp(n: 1, at: 1), Stamp(n: 2, at: 2)]))\n2\n");
}

#[test]
fn map_keys_without_a_text_form_decode_from_values() {
    let src = "import host
var calls = 0
fn next() -> int {
    calls += 1
    calls
}
struct Stamp { n: int, at: int = next() }
enum Shape { Dot, Circle(int) }
let a: Result<[(int, str): bool]> = host.decode(0)
print(a)
let b: Result<[Shape: int]> = host.decode(1)
print(b)
let c: Result<[Stamp: str]> = host.decode(2)
print(c)
print(c.unwrap().get(Stamp(n: 2, at: 2)))
let d: Result<[(int, int): str]> = host.decode(3)
print(d)";
    let tuple = |a: i64, b: &str| Value::Tuple(vec![Value::Int(a), str(b)]);
    let circle = Value::Variant(VariantValue::new("main.Shape", "Circle", [Value::Int(2)]));
    let stamp = |n| map([(str("n"), Value::Int(n))]);
    let inputs = vec![
        map([(tuple(1, "a"), Value::Bool(true)), (tuple(1, "a"), Value::Bool(false)), (tuple(2, "b"), Value::Bool(true))]),
        map([(str("Dot"), Value::Int(1)), (circle, Value::Int(2))]),
        map([(stamp(1), str("one")), (stamp(2), str("two"))]),
        map([(Value::Tuple(vec![Value::Int(1), str("x")]), str("bad"))]),
    ];
    let expected = [
        "Ok([(1, \"a\"): false, (2, \"b\"): true])",
        "Ok([Dot: 1, Circle(2): 2])",
        "Ok([Stamp(n: 1, at: 1): \"one\", Stamp(n: 2, at: 2): \"two\"])",
        "Some(\"two\")",
        "Err(Error(message: \"$: invalid map key `(1, x)`\", kind: \"parse\"))",
    ];
    assert_eq!(run(inputs, src), expected.map(|line| format!("{line}\n")).concat());
}

#[test]
fn map_keys_decode_from_exact_values_and_key_text() {
    let src = "import host
import time
enum Color { Red, Green }
for i in 0..5 {
    let a: Result<[Color: bool]> = host.decode(i)
    print(a)
}
let d: Result<[time.PlainDate: int]> = host.decode(5)
print(d)";
    let red = Value::Variant(VariantValue::new("main.Color", "Red", []));
    let date = jiff::civil::date(2024, 3, 10).encode();
    let inputs = vec![
        map([(red.clone(), Value::Bool(true))]),
        map([(str("Green"), Value::Bool(false))]),
        map([(Value::Int(1), Value::Bool(true))]),
        map([(str("Blue"), Value::Bool(true))]),
        map([(red, str("yes"))]),
        map([(date, Value::Int(1)), (str("2024-03-11"), Value::Int(2))]),
    ];
    let expected = [
        "Ok([Red: true])",
        "Ok([Green: false])",
        "Err(Error(message: \"$: invalid map key `1`\", kind: \"parse\"))",
        "Err(Error(message: \"$: invalid map key `Blue`\", kind: \"parse\"))",
        "Err(Error(message: \"$.Red: expected bool, found str\", kind: \"parse\"))",
        "Ok([2024-03-10: 1, 2024-03-11: 2])",
    ];
    assert_eq!(run(inputs, src), expected.map(|line| format!("{line}\n")).concat());
}

#[test]
fn mismatches_name_the_expected_type_and_the_found_value() {
    let src = "import host
import json
struct Order { id: int, lines: [(str, float)] }
for i in 0..5 {
    let r: Result<Order> = host.decode(i)
    print(r)
}
let k: Result<[(int, int): str]> = host.decode(5)
print(k)
let j: Result<[(int, int): str]> = json.decode(\"{\\\"(1, 2)\\\": \\\"a\\\"}\")
print(j)";
    let order = |lines| map([(str("id"), Value::Int(1)), (str("lines"), lines)]);
    let inputs = vec![
        order(Value::List(vec![Value::Tuple(vec![str("a"), str("b")])])),
        order(Value::List(vec![Value::List(vec![str("a")])])),
        map([(str("lines"), Value::List(vec![]))]),
        Value::Float(1.5),
        map([(Value::Int(1), Value::Int(2))]),
        map([(Value::Tuple(vec![Value::Int(1), Value::Int(2)]), str("a"))]),
    ];
    let expected = [
        "Err(Error(message: \"$.lines[0][1]: expected float, found str\", kind: \"parse\"))",
        "Err(Error(message: \"$.lines[0]: expected list of length 2\", kind: \"parse\"))",
        "Err(Error(message: \"$: missing field `id`\", kind: \"parse\"))",
        "Err(Error(message: \"$: expected Order, found float\", kind: \"parse\"))",
        "Err(Error(message: \"$: expected str key, found int\", kind: \"parse\"))",
        "Ok([(1, 2): \"a\"])",
        "Err(Error(message: \"$: cannot decode map keys of type (int, int)\", kind: \"invalid\"))",
    ];
    assert_eq!(run(inputs, src), expected.map(|line| format!("{line}\n")).concat());
}

#[test]
fn deep_values_fail_at_the_nesting_limit() {
    let nested = |depth| (0..depth).fold(Value::Unit, |v, _| Value::List(vec![v]));
    let out = run(
        vec![nested(512), nested(513)],
        "import host\nimport json\nfor i in 0..2 {\n    let r: Result<json.Json> = host.decode(i)\n    print(r.is_ok())\n    print(r.err().map(e => e.message))\n}",
    );
    assert_eq!(out, "true\nNone\nfalse\nSome(\"value too deeply nested to decode\")\n");
}

#[test]
fn long_strings_count_against_the_memory_limit_before_they_are_copied() {
    let limits = Limits { max_memory: Some(4 << 20), ..Limits::default() };
    let out = run_with(vec![Value::Str("x".repeat(8 << 20))], limits, "import host\nlet r: Result<str> = host.decode(0)\nprint(r.is_ok())");
    assert!(out.contains("memory"), "{out}");
}

/// A Sand type and a generator of JSON text for it.
#[derive(Clone, Debug)]
enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    Unit,
    Duration,
    Json,
    Color,
    Rec,
    Range,
    Date,
    List(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Tuple(Vec<Ty>),
    Shape(Box<Ty>),
}

const TYPES: &str = "enum Color { Red, Green }
struct Rec { a: int, b: str?, c: [int] = [7] }
enum Shape<T> { Dot, One(T), Two(T, int), Named(v: T, w: str?) }";

impl Ty {
    fn sand(&self) -> String {
        match self {
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "str".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Unit => "()".into(),
            Ty::Duration => "duration".into(),
            Ty::Json => "json.Json".into(),
            Ty::Color => "Color".into(),
            Ty::Rec => "Rec".into(),
            Ty::Range => "Range".into(),
            Ty::Date => "time.PlainDate".into(),
            Ty::List(t) => format!("[{}]", t.sand()),
            Ty::Set(t) => format!("Set<{}>", t.sand()),
            Ty::Map(k, v) => format!("[{}: {}]", k.sand(), v.sand()),
            Ty::Option(t) => format!("{}?", t.sand()),
            Ty::Tuple(ts) => format!("({})", ts.iter().map(Ty::sand).collect::<Vec<_>>().join(", ")),
            Ty::Shape(t) => format!("Shape<{}>", t.sand()),
        }
    }
}

fn ty() -> impl Strategy<Value = Ty> {
    let key = prop_oneof![Just(Ty::Int), Just(Ty::Str), Just(Ty::Bool), Just(Ty::Duration), Just(Ty::Color), Just(Ty::Date)];
    let leaf = prop_oneof![
        Just(Ty::Int),
        Just(Ty::Float),
        Just(Ty::Bool),
        Just(Ty::Str),
        Just(Ty::Bytes),
        Just(Ty::Unit),
        Just(Ty::Duration),
        Just(Ty::Json),
        Just(Ty::Color),
        Just(Ty::Rec),
        Just(Ty::Range),
        Just(Ty::Date),
    ];
    leaf.prop_recursive(3, 12, 3, move |inner| {
        prop_oneof![
            inner.clone().prop_map(|t| Ty::List(Box::new(t))),
            key.clone().prop_map(|t| Ty::Set(Box::new(t))),
            (key.clone(), inner.clone()).prop_map(|(k, v)| Ty::Map(Box::new(k), Box::new(v))),
            inner.clone().prop_map(|t| Ty::Option(Box::new(t))),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
            inner.prop_map(|t| Ty::Shape(Box::new(t))),
        ]
    })
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
        options[self.below(options.len() as u64) as usize]
    }
}

/// Returns JSON text for `t`, or with probability `1 / wrong` at each level,
/// text of another shape.
fn json_text(t: &Ty, rng: &mut Rng, depth: u32, wrong: u64) -> String {
    if rng.below(wrong) == 0 {
        return rng.pick(&["7", "1.5", "\"zzz\"", "null", "[]", "{}", "true"]).into();
    }
    let many = |t: &Ty, rng: &mut Rng| {
        let n = if depth > 3 { 0 } else { rng.below(3) };
        (0..n).map(|_| json_text(t, rng, depth + 1, wrong)).collect::<Vec<_>>()
    };
    match t {
        Ty::Int | Ty::Duration => rng.pick(&["-2", "0", "3", "4.0", "1e2"]).into(),
        Ty::Float => rng.pick(&["1.5", "-2", "0.25", "1e300"]).into(),
        Ty::Bool => rng.pick(&["true", "false"]).into(),
        Ty::Str => rng.pick(&["\"a\"", "\"\"", "\"\\u00e9\\n\""]).into(),
        Ty::Bytes => rng.pick(&["\"AQI=\"", "\"\"", "\"!!\""]).into(),
        Ty::Unit => "null".into(),
        Ty::Json => rng.pick(&["{\"a\": [1, null]}", "2", "\"s\"", "[{}, 1.5]"]).into(),
        Ty::Color => rng.pick(&["\"Red\"", "\"Green\"", "\"Blue\""]).into(),
        Ty::Rec => rng.pick(&["{\"a\": 1, \"b\": \"x\"}", "{\"a\": 2, \"c\": []}", "{\"b\": null}", "{\"a\": 1, \"z\": [1]}"]).into(),
        Ty::Range => rng.pick(&["{\"start\": 1, \"end\": 3}", "{\"start\": 0, \"end\": 0, \"inclusive\": true}", "{\"end\": 1}"]).into(),
        Ty::Date => rng.pick(&["\"2024-03-10\"", "\"2024-13-01\""]).into(),
        Ty::List(e) | Ty::Set(e) => format!("[{}]", many(e, rng).join(", ")),
        Ty::Map(k, v) => {
            // Distinct keys, because `json.parse` keeps only the last value of a duplicate key.
            let key = |i: usize| match **k {
                Ty::Int | Ty::Duration => format!("\"{i}\""),
                Ty::Bool => ["\"true\"", "\"false\""][i].into(),
                Ty::Color => ["\"Red\"", "\"Green\""][i].into(),
                Ty::Date => ["\"2024-03-10\"", "\"2024-03-11\""][i].into(),
                _ => format!("\"k{i}\""),
            };
            let entries = many(v, rng).into_iter().enumerate().map(|(i, v)| format!("{}: {v}", key(i))).collect::<Vec<_>>();
            format!("{{{}}}", entries.join(", "))
        }
        Ty::Option(e) => match rng.below(3) {
            0 => "null".into(),
            _ => json_text(e, rng, depth + 1, wrong),
        },
        Ty::Tuple(ts) => format!("[{}]", ts.iter().map(|t| json_text(t, rng, depth + 1, wrong)).collect::<Vec<_>>().join(", ")),
        Ty::Shape(e) => match rng.below(5) {
            0 => "\"Dot\"".into(),
            1 => format!("{{\"One\": [{}]}}", json_text(e, rng, depth + 1, wrong)),
            2 => format!("{{\"Two\": [{}, 1]}}", json_text(e, rng, depth + 1, wrong)),
            3 => format!("{{\"Named\": {{\"v\": {}}}}}", json_text(e, rng, depth + 1, wrong)),
            _ => format!("{{\"Named\": [{}, \"w\"]}}", json_text(e, rng, depth + 1, wrong)),
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    /// A `Value` in JSON form decodes as its JSON text does, and the
    /// `Vm::to_value` copy of a decoded value decodes to an equal value.
    #[test]
    fn values_decode_as_json_text_decodes(t in ty(), seed in 1..u64::MAX) {
        let mut rng = Rng(seed);
        let texts: Vec<String> = (0..6).map(|_| format!("{:?}", json_text(&t, &mut rng, 0, 12))).collect();
        let src = format!(
            "import host\nimport json\nimport time\n{TYPES}\n\
             for text in [{}] {{\n\
                 let a: Result<{ty}> = json.decode(text)\n\
                 let b: Result<{ty}> = host.decode(host.keep_json_form(json.parse(text).unwrap()))\n\
                 match (a, b) {{\n\
                     (Ok(x), Ok(y)) => {{\n\
                         let c: Result<{ty}> = host.decode(host.keep(x))\n\
                         print(\"ok \\(x == y) \\(c == Ok(x))\")\n\
                     }}\n\
                     (Err(e), Err(f)) => print(\"err \\(e.kind == f.kind)\")\n\
                     (a, b) => print(\"differ \\(a) \\(b)\")\n\
                 }}\n\
             }}",
            texts.join(", "),
            ty = t.sand(),
        );
        let out = run(Vec::new(), &src);
        prop_assert_eq!(out.lines().count(), texts.len(), "{}\n{}", out, src);
        for line in out.lines() {
            prop_assert!(line == "ok true true" || line == "err true", "{}\n{}", line, src);
        }
    }
}

#[derive(Clone, Debug, PartialEq, crate::SandEnum)]
#[sand(module = "host")]
enum Color {
    Red,
    Green,
}

#[derive(Clone, Debug, PartialEq, crate::SandEnum)]
#[sand(module = "host")]
enum Shape {
    Dot,
    One(i64),
    Two(i64, String),
    Named { v: f64, w: Option<String> },
}

/// A struct with a field of each form that decoding converts.
#[derive(Clone, Debug, PartialEq, crate::SandStruct)]
#[sand(module = "host")]
struct Mix {
    a: i64,
    f: f64,
    d: jiff::SignedDuration,
    s: String,
    b: Vec<u8>,
    o: Option<Option<i64>>,
    l: Vec<Shape>,
    t: (i64, String),
    m: std::collections::BTreeMap<i64, Color>,
    set: std::collections::BTreeSet<String>,
    date: Option<jiff::civil::Date>,
    j: Json,
    pairs: std::collections::BTreeMap<(i64, i64), String>,
    waits: std::collections::BTreeMap<jiff::SignedDuration, i64>,
    days: std::collections::BTreeMap<jiff::civil::Date, String>,
    #[sand(default = 9)]
    n: i64,
}

fn random_mix(rng: &mut Rng) -> Mix {
    let small = |rng: &mut Rng| rng.below(5) as i64 - 2;
    let shape = |rng: &mut Rng| match rng.below(4) {
        0 => Shape::Dot,
        1 => Shape::One(small(rng)),
        2 => Shape::Two(small(rng), rng.pick(&["a", "b"]).into()),
        _ => Shape::Named { v: 0.5, w: (rng.below(2) == 0).then(|| "w".into()) },
    };
    let json = match rng.below(3) {
        0 => Json::Null,
        1 => Json::Arr(vec![Json::Int(1), Json::Str("x".into())]),
        _ => Json::Obj([("k".to_string(), Json::Float(1.5))].into_iter().collect()),
    };
    Mix {
        a: small(rng),
        f: [1.5, -2.0, 0.0][rng.below(3) as usize],
        d: jiff::SignedDuration::from_nanos(small(rng)),
        s: rng.pick(&["", "s", "é\n"]).into(),
        b: (0..rng.below(3) as u8).collect(),
        o: [None, Some(None), Some(Some(3))][rng.below(3) as usize],
        l: (0..rng.below(3)).map(|_| shape(rng)).collect(),
        t: (small(rng), "t".into()),
        m: (0..rng.below(3) as i64).map(|k| (k, if k % 2 == 0 { Color::Red } else { Color::Green })).collect(),
        set: (0..rng.below(3)).map(|i| format!("e{i}")).collect(),
        date: (rng.below(2) == 0).then(|| jiff::civil::date(2024, 3, 10)),
        j: json,
        pairs: (0..rng.below(3) as i64).map(|k| ((k, small(rng)), "p".into())).collect(),
        waits: (0..rng.below(3) as i64).map(|k| (jiff::SignedDuration::from_nanos(k), small(rng))).collect(),
        days: (1..=rng.below(3) as i8).map(|d| (jiff::civil::date(2024, 3, d), "d".into())).collect(),
        n: small(rng),
    }
}

/// Rewrites parts of `value` at random: each part keeps its exact form, takes
/// its JSON form, or, with probability `1 / wrong`, becomes a value of another
/// shape. A struct field can also disappear.
fn reshape(value: &Value, rng: &mut Rng, wrong: u64) -> Value {
    if rng.below(wrong) == 0 {
        return [Value::Int(7), Value::Float(1.5), str("zzz"), Value::Unit, Value::List(vec![]), map([])][rng.below(6) as usize].clone();
    }
    let all = |items: &[Value], rng: &mut Rng| items.iter().map(|v| reshape(v, rng, wrong)).collect::<Vec<_>>();
    let json_form = rng.below(2) == 0;
    match value {
        Value::Duration(d) if json_form => Value::Int(d.as_nanos() as i64),
        Value::Bytes(b) if json_form => Value::Str(crate::stdlib::base64::encode(b, false)),
        Value::Native(native) if json_form => Value::Str(native.text.clone()),
        Value::List(items) => Value::List(all(items, rng)),
        Value::Tuple(items) | Value::Set(items) if json_form => Value::List(all(items, rng)),
        Value::Tuple(items) => Value::Tuple(all(items, rng)),
        Value::Set(items) => Value::Set(all(items, rng)),
        Value::Map(entries) => Value::Map(
            entries
                .iter()
                .map(|(k, v)| {
                    let text_key = json_form && !matches!(k, Value::Tuple(_));
                    let k = if text_key { Value::Str(crate::value::key_text(k).into_owned()) } else { reshape_key(k, rng, wrong) };
                    (k, reshape(v, rng, wrong))
                })
                .collect(),
        ),
        Value::Struct(s) => {
            let mut fields = Vec::new();
            for (name, v) in s.named_fields() {
                if rng.below(wrong) != 0 {
                    fields.push((name, reshape(v, rng, wrong)));
                }
            }
            match json_form {
                true => map(fields.into_iter().map(|(name, v)| (str(name), v))),
                false => Value::Struct(StructValue::new(s.ty.type_name.clone(), fields)),
            }
        }
        Value::Variant(v) if is_option_variant(v) && json_form => match v.payload.first() {
            Some(payload) => reshape(payload, rng, wrong),
            None => Value::Unit,
        },
        Value::Variant(v) => {
            let payload = all(&v.payload, rng);
            match (json_form, &v.variant.fields) {
                (true, _) if payload.is_empty() => str(&v.variant.name),
                (true, Some(names)) => map([(str(&v.variant.name), map(names.iter().map(|n| str(n)).zip(payload)))]),
                (true, None) => map([(str(&v.variant.name), Value::List(payload))]),
                (false, _) => Value::Variant(VariantValue { variant: v.variant.clone(), payload: payload.into() }),
            }
        }
        value => value.clone(),
    }
}

/// Rewrites the map key `key` like [`reshape`], and also into the other forms
/// of a key: an integral `Float` for an `int`, an `Int` for a `duration`, and
/// a `Str` for a native value. With probability `1 / wrong`, a `Str` key
/// becomes a native value.
fn reshape_key(key: &Value, rng: &mut Rng, wrong: u64) -> Value {
    match (key, rng.below(3)) {
        (Value::Int(i), 0) => Value::Float(*i as f64),
        (Value::Duration(d), 0) => Value::Int(d.as_nanos() as i64),
        (Value::Native(native), 0) => Value::Str(native.text.clone()),
        (Value::Str(s), _) if rng.below(wrong) == 0 => {
            Value::Native(Box::new(crate::value::NativeValue { type_name: "time.PlainDate".into(), text: s.clone() }))
        }
        _ => reshape(key, rng, wrong),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `Decode` for Rust types accepts the same `Value` inputs as the VM, and
    /// decodes them to the same values.
    #[test]
    fn rust_decode_agrees_with_vm_decode(seed in 1..u64::MAX) {
        let mut rng = Rng(seed);
        let inputs: Vec<Value> = (0..16).map(|i| reshape(&random_mix(&mut rng).encode(), &mut rng, if i < 4 { u64::MAX } else { 40 })).collect();
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let mut m = host_module(inputs.clone());
        m.bind::<Color>().bind::<Shape>().bind::<Mix>();
        m.source("pub native fn report(m: Mix?)");
        let reported = decoded.clone();
        m.function("report", move |m: Option<Mix>| reported.lock().unwrap().push(m));
        let out = MemorySink::new();
        let mut engine = Engine::new(Config::new().host_modules(HostModules::new([m]).unwrap()).stdout(out.clone()));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let src = format!("import host\nfor i in 0..{} {{\n    let r: Result<host.Mix> = host.decode(i)\n    host.report(r.ok())\n}}", inputs.len());
        rt.block_on(engine.run_source("main.sand", &src)).unwrap();
        let vm = decoded.lock().unwrap().clone();
        let rust: Vec<Option<Mix>> = inputs.iter().map(|v| v.decode::<Mix>().ok()).collect();
        prop_assert!(rust[..4].iter().all(Option::is_some), "{:?}", &inputs[..4]);
        for (i, (rust, vm)) in rust.iter().zip(&vm).enumerate() {
            prop_assert_eq!(rust, vm, "{:?}\n{:?}", &inputs[i], inputs[i].decode::<Mix>());
        }
    }
}
