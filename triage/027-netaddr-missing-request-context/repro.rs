use sandlang::policy::Capability;
use sandlang::engine::{Config, Engine, MemorySink};
use sandlang::policy::rules::{HttpMethod, Methods, RulePolicy, Transports};

const SCRIPT: &str = r#"
import http
match http.get("http://localhost:18931/admin/secret") {
    Ok(r) => print("GET localhost/admin/secret -> \(r.text().unwrap_or("?"))"),
    Err(e) => print("GET localhost/admin/secret -> \(e)"),
}
"#;

fn rules() -> RulePolicy {
    RulePolicy::standard()
        .allow_http(Transports::Plaintext, Methods::only(HttpMethod::GET, []), "*".parse().unwrap())
        .deny_http(Transports::Both, Methods::all(), "127.0.0.1/admin/**".parse().unwrap())
        .deny_http(Transports::Both, Methods::all(), "[::1]/admin/**".parse().unwrap())
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
        let decision = inner.check(cap);
        if !matches!(cap, Capability::Stdout) {
            println!("  audit: {cap} -> {decision:?}");
        }
        decision
    }));
}
