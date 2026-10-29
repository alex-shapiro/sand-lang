use sandlang::json::Json;
use sandlang::{Config, Engine, MemorySink, NativeModule};

fn main() {
    let depth: usize = std::env::args().nth(1).map_or(512, |s| s.parse().unwrap());
    let mode = std::env::args().nth(2).unwrap_or("in".into());
    let doc = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let src = match mode.as_str() {
        "parse" => format!("import json\nlet doc = json.parse(\"{doc}\")?\nprint(\"parsed\")\n"),
        "in" => format!("import json\nimport host\nlet doc = json.parse(\"{doc}\")?\nprint(host.depth(doc))\n"),
        _ => format!("import json\nimport host\nprint(json.encode(host.nested({depth})).len())\n"),
    };
    let run = move || {
        let mut host = NativeModule::new("host");
        host.function("depth", |j: Json| {
            let (mut depth, mut current) = (0, &j);
            while let Json::Arr(items) = current {
                depth += 1;
                match items.first() {
                    Some(first) => current = first,
                    None => break,
                }
            }
            depth as i64
        });
        host.function("nested", |n: i64| (0..n).fold(Json::Null, |j, _| Json::Arr(vec![j])));
        let out = MemorySink::new();
        let mut engine = Engine::new(Config::new().module(host).stdout(out.clone())).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = rt.block_on(engine.run_source("main.sand", &src));
        print!("{}", out.contents());
        println!("{result:?}");
    };
    std::thread::Builder::new().stack_size(512 * 1024).spawn(run).unwrap().join().unwrap();
}
