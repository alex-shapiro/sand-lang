#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Tree<K: Ord> {
    key: K,
    children: Vec<Self>,
}

fn main() {
    let mut module = sandlang::NativeModule::new("coll");
    module.bind::<Tree<i64>>();
}
