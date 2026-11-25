//! Coverage-guided fuzz target for JSON parsing, `Encode`/`Decode` walkers,
//! custom `Encoder`/`Decoder` callbacks, `init` decoding, `json.to_json`, and
//! `ValueSink`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use sandlang::engine::{CompiledProgram, Config, Engine, Error, HostModules, Limits, MemorySink};
use sandlang::native::NativeModule;
use sandlang::native::context::CallContext;
use sandlang::native::convert::HostError;
use sandlang::native::ty::{Encoded, Param, TraitDict};
use sandlang::policy::rules::RulePolicy;
use sandlang::types::traits;
use sandlang::vm::Fail;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const HARNESS: &str = r#"
import fuzz_in
import json
import time

enum Color { Red, Green, Blue }

enum Tree {
    Empty
    Leaf(int)
    Pair(int, str)
    Node(tag: str, kids: [Tree], color: Color)
}

fn default_tag() -> str { "def" }

struct BigDoc {
    f0: int = 0
    f1: float = 0.5
    f2: bool = false
    f3: str = default_tag()
    f4: bytes = b""
    f5: duration = 1ms
    f6: Range = 0..1
    f7: int?? = None
    f8: Set<str> = Set([])
    f9: (int, Color?) = (1, None)
}

struct Checked {
    n: int
    label: str
    fn init(n: int, label: str = "n=\(n)") -> Result<Checked> {
        if n < -1000 or n > 1000 {
            return Err(Error(message: "out of range", kind: "invalid"))
        }
        Ok(Self(n: n, label: label))
    }
}

struct Flex {
    tag: str
    val: int
}

impl Encode for Flex {
    fn encode(self, encoder: Encoder) -> Result<()> {
        if self.tag == "i" {
            encoder.encode(self.val)
        } else {
            encoder.encode((self.tag, self.val))
        }
    }
}

impl Decode for Flex {
    fn decode(decoder: Decoder) -> Result<Flex> {
        match decoder.kind()? {
            InputKind.Int => Ok(Flex(tag: "i", val: decoder.decode()?))
            InputKind.Str => {
                let s: str = decoder.decode()?
                Ok(Flex(tag: s, val: s.len()))
            }
            InputKind.List => {
                let (t, v): (str, int) = decoder.decode()?
                Ok(Flex(tag: t, val: v))
            }
            InputKind.Null => Ok(Flex(tag: "null", val: 0))
            _ => Err(Error(message: "bad flex", kind: "parse"))
        }
    }
}

struct Maps {
    si: [str: int] = [:]
    ii: [int: str] = [:]
    bi: [bool: int] = [:]
    di: [duration: int] = [:]
    ci: [Color: int] = [:]
    pi: [time.PlainDate: int] = [:]
}

struct Wrap<T> {
    item: T
    extra: Flex?
}

fn check_roundtrip<T: Encode + Decode + Eq>(v: T) -> Result<()> {
    let compact = json.encode(v)?
    let pretty = json.encode(v, indent: 2)?
    let j = json.to_json(v)?
    let a: T = json.decode(compact)?
    let b: T = json.decode(pretty)?
    if a != v or b != v {
        fail("roundtrip mismatch")
    }
    if json.parse(compact)? != j {
        fail("to_json mismatch")
    }
    if json.encode(j)? != compact {
        fail("re-encode mismatch")
    }
    fuzz_in.encode_val(v)
    let _ = fuzz_in.ctx_to_value(v)?
    Ok(())
}

fn try_decode<T: Encode + Decode + Eq>(s: str) -> Result<T> {
    let r: Result<T> = json.decode(s)
    if let Ok(v) = r {
        let _ = check_roundtrip(v)
    }
    r
}

let s = fuzz_in.input()
let parsed = json.parse(s)
let decoded_json: Result<json.Json> = json.decode(s)
if parsed.is_ok() != decoded_json.is_ok() {
    fail("parse vs decode<Json> status mismatch")
}
if let Ok(j) = parsed {
    if j != decoded_json.unwrap() {
        fail("parse vs decode<Json> value mismatch")
    }
    let _ = check_roundtrip(j)
}
let _: Result<BigDoc> = try_decode(s)
let _: Result<Checked> = try_decode(s)
let _: Result<Tree> = try_decode(s)
let _: Result<Flex> = try_decode(s)
let _: Result<Maps> = try_decode(s)
let _: Result<Wrap<Flex>> = try_decode(s)
let _: Result<[Tree]> = try_decode(s)
let _: Result<[never]> = json.decode(s)
"#;

struct Harness {
    input: Arc<Mutex<String>>,
    modules: HostModules,
    program: CompiledProgram,
    rt: tokio::runtime::Runtime,
}

fn harness() -> &'static Harness {
    static CELL: OnceLock<Harness> = OnceLock::new();
    CELL.get_or_init(|| {
        let input = Arc::new(Mutex::new(String::new()));
        let mut module = NativeModule::new("fuzz_in");
        module.source(
            "pub native fn input() -> str\n\
             pub native fn encode_val<T: Encode>(x: T) -> ()\n\
             pub native fn ctx_to_value<T: Encode>(x: T) -> Result<str>",
        );
        let shared = input.clone();
        module.function("input", move || shared.lock().unwrap().clone());
        module.function("encode_val", |_: Encoded<0>| {});
        module.function(
            "ctx_to_value",
            |ctx: &mut CallContext, dict: TraitDict<0, traits::Encode>, value: Param<0>| -> Result<String, HostError<Fail>> {
                let val = ctx.to_value(value.raw(), dict)?;
                Ok(format!("{val:?}").chars().take(32).collect())
            },
        );
        let modules = HostModules::new([module]).expect("fuzz_in module");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let engine = Engine::new(Config::new().host_modules(modules.clone()));
        let program = rt.block_on(engine.compile_source("codec_fuzz.sand", HARNESS)).expect("harness compiles");
        Harness { input, modules, program, rt }
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let h = harness();
    *h.input.lock().unwrap() = s.to_string();
    let config = Config::new()
        .host_modules(h.modules.clone())
        .policy(RulePolicy::standard())
        .stdout(MemorySink::new())
        .stderr(MemorySink::new())
        .limits(Limits {
            max_instructions: Some(250_000),
            max_memory: Some(16 << 20),
            max_wall_time: Some(Duration::from_secs(1)),
            max_call_depth: 256,
            ..Limits::default()
        });
    let mut engine = Engine::new(config);
    match h.rt.block_on(engine.run_program(&h.program)) {
        Ok(_) | Err(Error::Limit(_)) => {}
        Err(e @ Error::Internal { .. }) => panic!("internal error on {s:?}: {e}"),
        Err(Error::Runtime { message, .. })
            if message.contains("mismatch") =>
        {
            panic!("invariant failure on {s:?}: {message}");
        }
        Err(_) => {}
    }
});
