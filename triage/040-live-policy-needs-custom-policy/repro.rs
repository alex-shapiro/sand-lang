// Run: `cargo run` in a scratch crate that depends on sandlang and tokio (nothing needs to listen on port 9).
use sandlang::policy::PolicyFile;
use sandlang::{Capability, Config, Decision, Engine, MemorySink, RulePolicy};
use std::path::Path;
use std::sync::{Arc, RwLock};

const RULES: &str = "allow http plaintext *\ndeny http 127.0.0.1/admin [::1]/admin  # keep scripts out of the local admin UI\n";
const SRC: &str = "import http\nmatch http.get(\"http://localhost:9/admin\") {\n    Ok(r) => print(\"status \\(r.status)\"),\n    Err(e) => print(\"\\(e)\"),\n}";

fn run(config: Config) -> String {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("main.sand", SRC)).unwrap();
    out.contents().trim().to_string()
}

fn main() {
    let rules = || PolicyFile::parse(RULES, Path::new("/"), None).unwrap().rules.with_defaults(RulePolicy::standard());
    println!("Config::policy(rules):        {}", run(Config::new().policy(rules())));
    let shared = Arc::new(RwLock::new(rules()));
    let live = shared.clone();
    let custom = move |cap: &Capability| -> Decision { live.read().unwrap().check(cap) };
    println!("custom_policy(shared rules):  {}", run(Config::new().custom_policy(custom)));
}
