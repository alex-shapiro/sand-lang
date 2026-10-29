// Run: CARGO_TARGET_DIR=/tmp/triage-embed/target cargo run [--release] --bin json017 -- <depth> <parse|in|out>
// (scratch crate: sandlang = { path = "/Users/alex/Projects/sandlang" }, tokio = { features = ["rt"] })
use sandlang::json::Json;
use sandlang::{Config, Engine, MemorySink, NativeModule};

fn main() {
    let depth: usize = std::env::args().nth(1).map_or(512, |s| s.parse().unwrap());
    let mode = std::env::args().nth(2).unwrap_or("in".into());
    let doc = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let src = match mode.as_str() {
        // Control: the script parses the document and does not give it to the host.
        "parse" => format!("import json\nlet doc = json.parse(\"{doc}\")?\nprint(\"parsed\")\n"),
        // The script gives a parsed document to the host.
        "in" => format!("import json\nimport host\nlet doc = json.parse(\"{doc}\")?\nprint(host.depth(doc))\n"),
        // The host gives a document to the script.
        _ => format!("import json\nimport host\nprint(json.encode(host.nested({depth})).len())\n"),
    };
    let run = move || {
        let mut host = NativeModule::new("host");
        host.function("depth", |j: Json| {
            let (mut d, mut j) = (0, &j);
            while let Json::Arr(v) = j {
                d += 1;
                match v.first() {
                    Some(x) => j = x,
                    None => break,
                }
            }
            d as i64
        });
        host.function("nested", |n: i64| (0..n).fold(Json::Null, |j, _| Json::Arr(vec![j])));
        let out = MemorySink::new();
        let mut engine = Engine::new(Config::new().module(host).stdout(out.clone())).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let r = rt.block_on(engine.run_source("main.sand", &src));
        print!("{}", out.contents());
        println!("{r:?}");
    };
    // 512 KiB is the default stack size of a secondary thread on iOS.
    std::thread::Builder::new().stack_size(512 * 1024).spawn(run).unwrap().join().unwrap();
}
