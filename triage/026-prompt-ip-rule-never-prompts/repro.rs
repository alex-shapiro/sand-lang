use sandlang::{Capability, Config, Engine, MemorySink, PromptAnswer, RulePolicy};

fn main() {
    let policy = RulePolicy::standard().allow_http("plaintext GET *").prompt_net("127.0.0.0/8 ::1");
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
