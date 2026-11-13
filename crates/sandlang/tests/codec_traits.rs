//! Host API tests of the `Encode` and `Decode` acceptance scenarios. The Sand
//! side of each scenario is in `spec/codec_traits.test`.

mod common;

use common::rt;
use parking_lot::Mutex;
use sandlang::codec::{Decoder, Encoder};
use sandlang::engine::{Config, Engine, Error, HostModules, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::convert::HostError;
use sandlang::native::ty::{Encoded, VmVal};
use sandlang::value::{StructValue, Value, VariantValue};
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
/// as `"21C"`.
#[derive(Clone, Debug, PartialEq, sandlang::SandStruct)]
#[sand(module = "lab")]
pub struct Reading {
    pub celsius: i64,
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
    lab.bind::<Reading>().bind::<Labeled<()>>();
    lab.source("impl Encode for Reading {\n    native fn encode(self, e: Encoder)\n}\nimpl Decode for Reading {\n    native fn decode(d: Decoder) -> Result<Reading>\n}");
    lab.function("Reading.encode", |vm: &mut Vm, r: Reading, e: VmVal<Encoder>| e.write_str(vm, &format!("{}C", r.celsius)));
    lab.function("Reading.decode", |vm: &mut Vm, d: VmVal<Decoder>| -> Result<Reading, HostError> {
        let text = d.read_str(vm)?;
        match text.strip_suffix('C').and_then(|c| c.parse().ok()) {
            Some(celsius) => Ok(Reading { celsius }),
            None => Err(d.error(vm, "expected a temperature such as 21C")?),
        }
    });
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
