// Run: start ../025-matrix-param-deny-bypass/server.py on port 18931, then build this as a bin in a crate with deps sandlang (path) and tokio (features ["rt"]).
use sandlang::{Capability, Config, Engine, MemorySink, RulePolicy};

const SCRIPT: &str = r#"
import http
match http.get("http://localhost:18931/admin/secret") {
    Ok(r) => print("GET localhost/admin/secret -> \(r.text().unwrap_or("?"))"),
    Err(e) => print("GET localhost/admin/secret -> \(e)"),
}
"#;

/// "Allow GET anywhere, but never reach the admin path of the loopback service."
fn rules() -> RulePolicy {
    RulePolicy::standard()
        .allow_http("plaintext GET *")
        .deny_http("127.0.0.1/admin/**")
        .deny_http("[::1]/admin/**")
}

fn run(config: Config) {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("main.sand", SCRIPT)).unwrap();
    print!("  {}", out.contents());
}

fn main() {
    println!("RulePolicy directly:");
    run(Config::new().policy(rules()));

    println!("Same RulePolicy behind an audit-logging CustomPolicy:");
    let inner = rules();
    run(Config::new().custom_policy(move |cap: &Capability| {
        let d = inner.check(cap);
        if !matches!(cap, Capability::Stdout) {
            println!("  audit: {cap} -> {d:?}");
        }
        d
    }));
}
