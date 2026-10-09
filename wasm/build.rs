//! The WebAssembly module's stack. The compiler's passes recurse over the
//! syntax tree; natively they run on a thread with a large stack, on the web
//! on the module's own stack (see `sandlang::platform`), which must hold the
//! deepest nesting the parser accepts.

/// Bytes of stack (the default is 1 MiB).
const STACK: usize = 16 << 20;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo::rustc-link-arg-cdylib=-zstack-size={STACK}");
    }
}
