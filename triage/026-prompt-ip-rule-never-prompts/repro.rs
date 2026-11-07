use sandlang::engine::{Config, Engine, MemorySink};
use sandlang::policy::rules::{HttpMethod, Methods, RulePolicy, Transports};
use sandlang::policy::{Capability, PromptAnswer};

fn main() {
    let policy = RulePolicy::standard()
        .allow_http(Transports::Plaintext, Methods::only(HttpMethod::GET, []), "*".parse().unwrap())
        .prompt_net(Transports::Both, "127.0.0.0/8".parse().unwrap())
        .prompt_net(Transports::Both, "::1".parse().unwrap());
    let prompter = |cap: Capability| async move {
        println!("  PROMPT: {cap}  -> answering y");
        PromptAnswer::AllowOnce
    };
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().policy(policy).prompter(prompter).stdout(out.clone())).unwrap();
    let script = r#"
import http
for url in ["http://127.0.0.1:18931/", "http://localhost:18931/"] {
    match http.get(url) {
        Ok(r) => print("\(url) -> status \(r.status)"),
        Err(e) => print("\(url) -> \(e)"),
    }
}
"#;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("main.sand", script)).unwrap();
    print!("{}", out.contents());
}
