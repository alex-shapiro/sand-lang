// Run: CARGO_TARGET_DIR=/tmp/triage-embed/target cargo run --bin default019
use sandlang::jiff::{Timestamp, civil::Date};
use sandlang::{Config, Engine, MemorySink, NativeModule};

/// Retention settings that a script can construct or decode from JSON.
#[derive(Clone, Debug, sandlang::SandStruct)]
#[sand(module = "jobs")]
struct Retention {
    name: String,
    #[sand(default = Date::constant(2000, 1, 1))]
    since: Date,
    #[sand(default = Timestamp::UNIX_EPOCH)]
    last_run: Timestamp,
}

fn main() {
    let mut m = NativeModule::new("jobs");
    m.bind::<Retention>();
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().module(m).stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let src = "import jobs\nimport json\nlet r: jobs.Retention = json.decode(\"{\\\"name\\\": \\\"logs\\\"}\")?\nprint(r)\n";
    let r = rt.block_on(engine.run_source("main.sand", src));
    print!("{}", out.contents());
    println!("{r:?}");
}
