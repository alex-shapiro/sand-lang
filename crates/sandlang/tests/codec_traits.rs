//! Host API tests of the `Encode` and `Decode` acceptance scenarios. The Sand
//! side of each scenario is in `spec/codec_traits.test`.

mod common;

use common::rt;
use parking_lot::Mutex;
use sandlang::codec::Decoder;
use sandlang::engine::{Config, Engine, Error, HostModules, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::ty::{Encoded, VmVal};
use sandlang::value::{Decode, DecodeError, Encode, StructValue, Value, ValueReader, VariantValue};
use sandlang::vm::{Fail, Vm};
use std::sync::Arc;

/// The Sand types that the scenarios share: `Email` validates in `init`, and
/// `Money` encodes as a text such as `"12.50 USD"`.
const TYPES: &str = r#"
struct Email {
    address: str

    fn init(address: str) -> Result<Email> {
        if not address.contains("@") {
            return Err(Error("invalid email", kind: "invalid"))
        }
        Ok(Self(address: address))
    }
}

impl Encode for Email {
    fn encode(self, e: Encoder) {
        e.encode(self.address)
    }
}

impl Decode for Email {
    fn decode(d: Decoder) -> Result<Email> {
        Email(d.decode()?)
    }
}

struct Money { cents: int, currency: str }

impl Encode for Money {
    fn encode(self, e: Encoder) {
        let fraction = "\(self.cents % 100)".pad_start(2, "0")
        e.encode("\(self.cents / 100).\(fraction) \(self.currency)")
    }
}

impl Decode for Money {
    fn decode(d: Decoder) -> Result<Money> {
        let text: str = d.decode()?
        let parts = text.split(" ")
        let amount = parts[0].split(".")
        let (Some(whole), Some(cents)) = (amount[0].parse_int(), amount.get(1).and_then(c => c.parse_int())) else {
            return Err(Error("invalid amount", kind: "invalid"))
        }
        Ok(Money(cents: whole * 100 + cents, currency: parts.get(1) ?? ""))
    }
}

struct Account { owner: str, email: Email, balance: Money }
"#;

/// Builds the `host` module: `host.keep` records the `Encode` form of its
/// argument, and `host.input()` returns a `Decoder` over the next value of
/// `inputs`.
fn host_module(kept: Arc<Mutex<Vec<Value>>>, inputs: Vec<Value>) -> NativeModule {
    let inputs = Arc::new(Mutex::new(inputs));
    let mut host = NativeModule::new("host");
    host.source("pub native fn keep<T: Encode>(value: T)\npub native fn input() -> Decoder");
    host.function("keep", move |value: Encoded<0>| kept.lock().push(value.into_value()));
    host.function("input", move |vm: &mut Vm| -> Result<VmVal<Decoder>, Fail> {
        let value = inputs.lock().remove(0);
        vm.new_object(Decoder::from_value(value))
    });
    host
}

fn run(modules: Vec<NativeModule>, limits: Limits, src: &str) -> (Result<(), Error>, String) {
    let out = MemorySink::new();
    let config = Config::new().limits(limits).host_modules(HostModules::new(modules).unwrap()).stdout(out.clone());
    let mut engine = Engine::new(config);
    let r = rt().block_on(engine.run_source("main.sand", src)).map(drop);
    (r, out.contents())
}

fn str_(s: &str) -> Value {
    Value::Str(s.into())
}

fn account(email: &str, balance: &str) -> Value {
    Value::Struct(StructValue::new("main.Account", [("owner", str_("ana")), ("email", str_(email)), ("balance", str_(balance))]))
}

#[test]
fn scenario_6_host_values_go_through_the_hand_written_implementations() {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let inputs = vec![account("ana@x.io", "12.50 USD"), account("nope", "1.00 USD"), Value::List(vec![str_("a@b.c"), str_("x")])];
    let src = format!(
        "import host\n{TYPES}\n\
         host.keep(Account(owner: \"ana\", email: Email(\"ana@x.io\")?, balance: Money(cents: 1250, currency: \"USD\")))\n\
         let d = host.input()\nlet a: Account = d.finish(d.decode())?\nprint(a.email.address)\nprint(a.balance.cents)\n\
         let bad = host.input()\nlet b: Result<Account> = bad.finish(bad.decode())\nprint(b)\n\
         let list = host.input()\nlet c: Result<[Email]> = list.finish(list.decode())\nprint(c)\n"
    );
    let (r, out) = run(vec![host_module(kept.clone(), inputs)], Limits::default(), &src);
    r.unwrap();
    assert_eq!(*kept.lock(), [account("ana@x.io", "12.50 USD")]);
    assert_eq!(
        out,
        "ana@x.io\n1250\nErr(Error(message: \"$.email: invalid email\", kind: \"invalid\"))\n\
         Err(Error(message: \"$[1]: invalid email\", kind: \"invalid\"))\n"
    );
}

#[test]
fn a_host_decoder_reads_another_type_after_a_failed_read() {
    let inputs = vec![str_("x"), str_("y")];
    let src = "import host\n\
               let d = host.input()\nlet n: Result<int> = d.decode()\nprint(n)\nlet s: str = d.finish(d.decode())?\nprint(s)\n\
               let twice = host.input()\nlet a: str = twice.decode()?\nlet b: Result<str> = twice.decode()\nprint(b)\n";
    let (r, out) = run(vec![host_module(Arc::default(), inputs)], Limits::default(), src);
    r.unwrap();
    assert_eq!(
        out,
        "Err(Error(message: \"$: expected int, found str\", kind: \"parse\"))\nx\n\
         Err(Error(message: \"$: the decoder already read its value\", kind: \"invalid\"))\n"
    );
}

/// Returns the time and the fuel of a host decode of `leaves` leaves of an
/// enum, nested `depth` levels deep.
fn deep_host_decode(depth: usize, leaves: usize) -> (std::time::Duration, u64) {
    let variant = |name: &str, payload: Value| Value::Variant(VariantValue::new("main.T", name, [payload]));
    let mut value = variant("L", Value::List((0..leaves).map(|i| variant("I", Value::Int(i as i64))).collect()));
    for _ in 0..depth {
        value = variant("L", Value::List(vec![value]));
    }
    let mut meter = NativeModule::new("meter");
    meter.function("fuel", |vm: &mut Vm| vm.meter.used() as i64);
    let src = "import host\nimport meter\nenum T { L([T]), I(int) }\nlet d = host.input()\nlet start = meter.fuel()\n\
               let r: Result<T> = d.finish(d.decode())\nprint(meter.fuel() - start)\nprint(r.is_ok())\n";
    let modules = vec![host_module(Arc::default(), vec![value]), meter];
    let start = std::time::Instant::now();
    let (r, out) = run(modules, Limits::default(), src);
    let time = start.elapsed();
    r.unwrap();
    let (fuel, ok) = out.trim().split_once("\n").unwrap();
    assert_eq!(ok, "true");
    (time, fuel.parse().unwrap())
}

#[test]
fn a_host_decoder_reads_each_part_in_constant_time_at_any_depth() {
    let (shallow, shallow_fuel) = deep_host_decode(1, 20_000);
    let (deep, deep_fuel) = deep_host_decode(200, 20_000);
    assert!(deep_fuel < shallow_fuel + shallow_fuel / 10, "fuel {shallow_fuel} at depth 1, {deep_fuel} at depth 200");
    assert!(deep < shallow * 3, "{shallow:?} at depth 1, {deep:?} at depth 200");
}

/// Returns `true` if a decode of JSON text with `items` items and a `Set`
/// field passes under the memory limit `max_memory`. The `Set` is the first
/// field of the text if `early`, and the last if not.
fn decode_with_a_set_field(items: usize, early: bool, max_memory: usize) -> bool {
    let order = if early { "when: Set<int>, items: [Item]" } else { "items: [Item], when: Set<int>" };
    let src = format!(
        "import json\nstruct Item {{ name: str, n: int, tags: [str] }}\nstruct Doc {{ {order} }}\n\
         fn text() -> str {{\n    var items: [Item] = []\n    for i in 0..{items} {{\n        items.push(Item(name: \"item \\(i)\", n: i, tags: [\"a\", \"b\"]))\n    }}\n    json.encode(Doc(when: Set([1]), items: items))\n}}\n\
         let t = text()\nlet r: Result<Doc> = json.decode(t)\nprint(r.is_ok())\n"
    );
    let (r, out) = run(Vec::new(), Limits { max_memory: Some(max_memory), ..Limits::default() }, &src);
    r.is_ok() && out == "true\n"
}

#[test]
fn a_rare_field_late_in_json_text_needs_no_second_read() {
    let limit = 36 << 20;
    assert!(decode_with_a_set_field(50_000, true, limit), "the limit is too low for the decode");
    assert!(decode_with_a_set_field(50_000, false, limit), "a late `Set` field needs more memory than an early one");
}

#[test]
fn scenario_6_rust_and_sand_derived_forms_agree() {
    use sandlang::value::Encode;
    /// A shipment.
    #[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
    #[sand(module = "ship")]
    struct Shipment {
        id: i64,
        tags: Vec<String>,
        note: Option<String>,
    }
    let shipment = Shipment { id: 3, tags: vec!["a".into()], note: None };
    let kept = Arc::new(Mutex::new(Vec::new()));
    let mut ship = NativeModule::new("ship");
    ship.bind::<Shipment>();
    let made = shipment.clone();
    ship.function("make", move || made.clone());
    ship.function("take", |s: Shipment| s.id);
    let inputs = vec![shipment.encode()];
    let src = "import host\nimport ship\nhost.keep(ship.make())\nlet d = host.input()\nlet s: ship.Shipment = d.finish(d.decode())?\nprint(ship.take(s))\n";
    let (r, out) = run(vec![host_module(kept.clone(), inputs), ship], Limits::default(), src);
    r.unwrap();
    assert_eq!(out, "3\n");
    assert_eq!(*kept.lock(), [shipment.encode()]);
    assert_eq!(kept.lock()[0].decode::<Shipment>(), Ok(shipment));
}

/// A temperature reading whose Rust `Encode` and `Decode` write a text such
/// as `"21C"`, which is its form in every format.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "lab", no_value)]
pub struct Reading {
    pub celsius: i64,
}

impl Encode for Reading {
    fn encode(&self) -> Value {
        Value::Str(format!("{}C", self.celsius))
    }
}

impl Decode for Reading {
    fn decode(r: &mut ValueReader<'_>) -> Result<Reading, DecodeError> {
        let text: String = r.decode()?;
        let celsius = text.strip_suffix('C').and_then(|c| c.parse().ok());
        celsius.map(|celsius| Reading { celsius }).ok_or_else(|| DecodeError::new("expected a temperature such as 21C"))
    }
}

/// A value with a label, generic over its value.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "lab")]
pub struct Labeled<T> {
    pub label: String,
    pub value: T,
}

fn lab_module() -> NativeModule {
    let mut lab = NativeModule::new("lab");
    lab.bind::<Reading>().custom_codec::<Reading>().bind::<Labeled<()>>();
    lab
}

#[test]
fn scenario_7_native_implementations_compose_with_script_types() {
    let src = format!(
        "import json\nimport lab\n{TYPES}\n\
         struct Log {{ at: str, reading: lab.Reading }}\n\
         let log = Log(at: \"noon\", reading: lab.Reading(celsius: 21))\n\
         let text = json.encode(log)\nprint(text)\nlet back: Log = json.decode(text)?\nprint(back == log)\n\
         let bad: Result<[Log]> = json.decode(\"[{{\\\"at\\\": \\\"x\\\", \\\"reading\\\": \\\"hot\\\"}}]\")\nprint(bad)\n\
         let tagged = lab.Labeled(label: \"owner\", value: Email(\"o@x.io\")?)\n\
         let tagged_text = json.encode(tagged)\nprint(tagged_text)\n\
         let again: lab.Labeled<Email> = json.decode(tagged_text)?\nprint(again.value.address)\n\
         let wrong: Result<lab.Labeled<Email>> = json.decode(\"{{\\\"label\\\": \\\"l\\\", \\\"value\\\": \\\"v\\\"}}\")\nprint(wrong)\n"
    );
    let (r, out) = run(vec![lab_module()], Limits::default(), &src);
    r.unwrap();
    assert_eq!(
        out,
        "{\"at\":\"noon\",\"reading\":\"21C\"}\ntrue\n\
         Err(Error(message: \"$[0].reading: expected a temperature such as 21C\", kind: \"parse\"))\n\
         {\"label\":\"owner\",\"value\":\"o@x.io\"}\no@x.io\n\
         Err(Error(message: \"$.value: invalid email\", kind: \"invalid\"))\n"
    );
}

#[test]
fn a_native_decode_error_lets_the_caller_read_another_type() {
    let src = "import json\nimport lab\nenum Entry { Reading(lab.Reading), Note(str) }\n\
               impl Decode for Entry {\n    fn decode(d: Decoder) -> Result<Entry> {\n        let r: Result<lab.Reading> = d.decode()\n        if let Ok(v) = r {\n            return Ok(Entry.Reading(v))\n        }\n        Ok(Entry.Note(d.decode()?))\n    }\n}\n\
               let entries: Result<[Entry]> = json.decode(\"[\\\"21C\\\", \\\"hot\\\"]\")\nprint(entries)\n";
    let (r, out) = run(vec![lab_module()], Limits::default(), src);
    r.unwrap();
    assert_eq!(out, "Ok([Reading(Reading(celsius: 21)), Note(\"hot\")])\n");
}

#[test]
fn a_custom_codec_gives_one_form_to_rust_and_sand() {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let reading = Reading { celsius: 3 };
    let inputs = vec![reading.encode()];
    let src = "import host\nimport lab\nhost.keep(lab.Reading(celsius: 3))\n\
               let d = host.input()\nlet r: lab.Reading = d.finish(d.decode())?\nprint(r.celsius)\n";
    let (r, out) = run(vec![host_module(kept.clone(), inputs), lab_module()], Limits::default(), src);
    r.unwrap();
    assert_eq!(out, "3\n");
    assert_eq!(*kept.lock(), [Value::Str("3C".into())]);
    assert_eq!(kept.lock()[0].decode::<Reading>(), Ok(reading));
}

#[test]
fn a_hand_written_sand_codec_cannot_change_a_derived_rust_form() {
    /// A probe whose Rust `Encode` and `Decode` are derived.
    #[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
    #[sand(module = "lab")]
    pub struct Probe {
        pub depth: i64,
    }
    let mut lab = NativeModule::new("lab");
    lab.bind::<Probe>();
    lab.source("impl Encode for Probe {\n    fn encode(self, e: Encoder) {\n        e.encode(\"probe\")\n    }\n}");
    let Err(Error::Compile(errors)) = HostModules::new([lab]) else { panic!("the registration passed") };
    let message = &errors[0].message;
    assert!(message.starts_with("`lab.Probe` derives the Rust `value::Encode` and `value::Decode`"), "{message}");
}

#[test]
fn scenario_8_deep_input_needs_little_native_stack() {
    let deep_struct = "struct Link { next: Link? }\nfn chain(n: int) -> Link {\n    var link = Link(next: None)\n    for _ in 1..n {\n        link = Link(next: Some(link))\n    }\n    link\n}\n";
    let src = format!(
        "import json\n{deep_struct}\
         let text = json.encode(chain(511))\nprint(text.len())\nlet back: Link = json.decode(text)?\nprint(back == chain(511))\n\
         let arrays = \"[\".repeat(512) + \"]\".repeat(512)\nprint(json.parse(arrays).is_ok())\n\
         let deeper: Result<Link> = json.decode(json.encode(chain(511)).replace(\"null\", \"{{\\\"next\\\": null}}\"))\nprint(deeper.is_ok())\n\
         let too_deep: Result<json.Json> = json.decode(\"[\".repeat(513) + \"]\".repeat(513))\nprint(too_deep)\n"
    );
    let run_small = move || run(Vec::new(), Limits::default(), &src);
    let (r, out) = std::thread::Builder::new().stack_size(256 << 10).spawn(run_small).unwrap().join().unwrap();
    r.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[1..4], ["true", "true", "true"], "{out}");
    assert!(lines[4].contains("nesting too deep"), "{out}");
}

#[test]
fn scenario_8_a_hand_written_implementation_cannot_escape_the_limits() {
    let src = "import json\nstruct Forever { x: int }\nimpl Decode for Forever {\n    fn decode(d: Decoder) -> Result<Forever> {\n        let inner = Forever.decode(d)?\n        Ok(Forever(x: inner.x + 1))\n    }\n}\n\
               let f: Result<Forever> = json.decode(\"1\")\n";
    let (r, _) = run(Vec::new(), Limits::default(), src);
    assert!(matches!(&r, Err(Error::Runtime { message, .. }) if message.contains("stack overflow")), "{r:?}");
    let spin = "import json\nstruct Spin { x: int }\nimpl Decode for Spin {\n    fn decode(d: Decoder) -> Result<Spin> {\n        var n = 0\n        while true {\n            n += 1\n        }\n        Ok(Spin(x: n))\n    }\n}\n\
                let s: Result<Spin> = json.decode(\"1\")\n";
    let (r, _) = run(Vec::new(), Limits { max_instructions: Some(100_000), ..Limits::default() }, spin);
    assert!(matches!(r, Err(Error::Limit(sandlang::engine::LimitKind::Instructions))), "{r:?}");
    let grow = "import json\nstruct Grow { x: [int] }\nimpl Decode for Grow {\n    fn decode(d: Decoder) -> Result<Grow> {\n        let xs: [int] = []\n        while true {\n            xs.push(1)\n        }\n        Ok(Grow(x: xs))\n    }\n}\n\
                let g: Result<Grow> = json.decode(\"1\")\n";
    let (r, _) = run(Vec::new(), Limits { max_memory: Some(16 << 20), ..Limits::default() }, grow);
    assert!(matches!(r, Err(Error::Limit(sandlang::engine::LimitKind::Memory))), "{r:?}");
}

#[test]
fn scenario_9_one_implementation_serves_every_format() {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let inputs = vec![str_("w@x.io"), str_("3.25 EUR"), Value::Variant(VariantValue::new("Option", "Some", [str_("z@x.io")]))];
    let src = format!(
        "import host\nimport json\n{TYPES}\n\
         let e = Email(\"a@x.io\")?\nlet m = Money(cents: 1250, currency: \"USD\")\n\
         print(json.encode((e, m)))\nprint(json.to_json((e, m)))\nhost.keep((e, m))\n\
         let from_text: (Email, Money) = json.decode(json.encode((e, m)))?\nprint(from_text.1.cents)\n\
         let from_tree: (Email, Money) = json.from_json(json.to_json((e, m)))?\nprint(from_tree.0.address)\n\
         let d1 = host.input()\nlet v1: Email = d1.finish(d1.decode())?\nprint(v1.address)\n\
         let d2 = host.input()\nlet v2: Money = d2.finish(d2.decode())?\nprint(v2.cents)\n\
         let d3 = host.input()\nlet v3: Email? = d3.finish(d3.decode())?\nprint(v3?.address)\n"
    );
    let (r, out) = run(vec![host_module(kept.clone(), inputs)], Limits::default(), &src);
    r.unwrap();
    assert_eq!(out, "[\"a@x.io\",\"12.50 USD\"]\nArr([Str(\"a@x.io\"), Str(\"12.50 USD\")])\n1250\na@x.io\nw@x.io\n325\nz@x.io\n");
    assert_eq!(*kept.lock(), [Value::Tuple(vec![str_("a@x.io"), str_("12.50 USD")])]);
}
