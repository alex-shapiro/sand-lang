// Run: CARGO_TARGET_DIR=/tmp/triage-embed/target cargo build --bin tree018
/// A search tree whose children are of the same type.
#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Tree<K: Ord> {
    key: K,
    children: Vec<Self>,
}

fn main() {
    let mut m = sandlang::NativeModule::new("coll");
    m.bind::<Tree<i64>>();
}
