// Run: CARGO_TARGET_DIR=/tmp/triage-embed/target cargo build --bin range022
use std::ops::{Range, RangeInclusive};

/// A slice of lines to show, such as `View(lines: 10..20)`.
#[derive(Clone, sandlang::SandStruct)]
#[sand(module = "editor")]
struct View {
    #[sand(default = 0..25)]
    lines: Range<i64>,
}

fn main() {
    let mut m = sandlang::NativeModule::new("editor");
    m.bind::<View>();
    m.function("count", |r: RangeInclusive<i64>| r.count() as i64);
}
