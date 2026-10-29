use sandlang::{Config, Engine, MemorySink, NativeModule};
use std::collections::BTreeSet;

#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Sorted<T: Ord> {
    items: BTreeSet<T>,
}

#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Group<T: Ord> {
    name: String,
    members: Sorted<T>,
}

fn main() {
    let mut module = NativeModule::new("coll");
    module.bind::<Sorted<i64>>();
    module.bind::<Group<i64>>();
    module.function("group", |name: String| Group { name, members: Sorted { items: BTreeSet::from([3, 1, 2]) } });
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().module(module).stdout(out.clone())).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(engine.run_source("main.sand", "import coll\nprint(coll.group(\"a\").members.items)\n"));
    print!("{}", out.contents());
    println!("{result:?}");
}
