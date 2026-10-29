use std::ops::{Range, RangeInclusive};

#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "editor")]
struct View {
    #[sand(default = 0..25)]
    lines: Range<i64>,
}

fn main() {
    let mut module = sandlang::NativeModule::new("editor");
    module.bind::<View>();
    module.function("count", |range: RangeInclusive<i64>| range.count() as i64);
}
