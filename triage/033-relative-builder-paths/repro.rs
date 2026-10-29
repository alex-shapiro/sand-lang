use sandlang::policy::PolicyFile;
use sandlang::{Config, Engine, MemorySink, RulePolicy};
use std::path::Path;

fn main() {
    let app = Path::new("/tmp/triage-policy2/app");
    let policy = RulePolicy::standard().allow_write("data").deny_read("data/secrets");
    let text = policy.to_string();
    print!("Display:\n{text}");
    if let Err(errors) = PolicyFile::parse(&text, app, None) {
        for err in errors {
            println!("parse back, line {}: {}", err.line, err.message);
        }
    }
    let out = MemorySink::new();
    let config = Config::new().cwd(app).policy(policy).stdout(out.clone());
    let mut engine = Engine::new(config).unwrap();
    let src = "import fs\nprint(fs.write_text(\"data/out.txt\", \"hi\"))";
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(engine.run_source("main.sand", src)).unwrap();
    print!("script in {}: {}", app.display(), out.contents());
}
