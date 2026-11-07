#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "coll")]
struct Tree<K: Ord> {
    key: K,
    children: Vec<Self>,
}

fn main() {
    let mut module = sandlang::native::NativeModule::new("coll");
    module.bind::<Tree<i64>>();
}
