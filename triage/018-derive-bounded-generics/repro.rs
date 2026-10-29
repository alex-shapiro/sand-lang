// Run: CARGO_TARGET_DIR=/tmp/triage-embed/target cargo build --bin sorted018
use sandlang::{Config, Engine, MemorySink, NativeModule};
use std::collections::BTreeSet;

/// A set of items in sorted order.
#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Sorted<T: Ord> {
    items: BTreeSet<T>,
}

/// A named group that holds a sorted set.
#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Group<T: Ord> {
    name: String,
    members: Sorted<T>,
}

fn main() {
    let mut m = NativeModule::new("coll");
    m.bind::<Sorted<i64>>();
    m.bind::<Group<i64>>();
    m.function("group", |name: String| Group { name, members: Sorted { items: BTreeSet::from([3, 1, 2]) } });
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().module(m).stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(engine.run_source("main.sand", "import coll\nprint(coll.group(\"a\").members.items)\n"));
    print!("{}", out.contents());
    println!("{r:?}");
}
