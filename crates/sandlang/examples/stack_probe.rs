//! Runs one deep codec case on a thread with a given stack size.
//! Usage: stack_probe CASE STACK_KIB. Prints the script output.
use sandlang::engine::{Config, Engine, MemorySink};

const PRELUDE: &str = r#"import json
struct N { v: int, next: N? }
let depth = 512
var arr = ""
for _ in 0..depth { arr += "[" }
for _ in 0..depth { arr += "]" }
var obj = ""
for _ in 0..(depth - 1) { obj += "{\"v\":1,\"next\":" }
obj += "{\"v\":1,\"next\":null}"
for _ in 0..(depth - 1) { obj += "}" }
var chain = N(v: 1, next: None)
for _ in 0..(depth - 1) { chain = N(v: 1, next: Some(chain)) }
"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let body = match args[1].as_str() {
        "parse" => "print(json.parse(arr).is_ok())",
        "decode" => "let r: Result<N> = json.decode(obj)\nprint(r.is_ok())",
        "encode" => "print(json.encode(chain).len())",
        "none" => "print(obj.len())",
        c => panic!("unknown case {c}"),
    };
    let kib: usize = args[2].parse().unwrap();
    let src = format!("{PRELUDE}{body}\n");
    let out = std::thread::Builder::new()
        .stack_size(kib << 10)
        .spawn(move || {
            let out = MemorySink::new();
            let mut engine = Engine::new(Config::new().stdout(out.clone()));
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let r = rt.block_on(engine.run_source("probe.sand", &src));
            format!("{}{:?}", out.contents(), r.err())
        })
        .unwrap()
        .join()
        .unwrap();
    print!("{out}");
}
