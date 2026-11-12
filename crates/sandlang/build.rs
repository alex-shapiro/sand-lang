//! Sets `cfg(web)` for WebAssembly in a JavaScript host
//! (`wasm32-unknown-unknown`), where the clock, timers and entropy come from
//! JavaScript and there are no threads (see `src/platform.rs`).

fn main() {
    println!("cargo::rustc-check-cfg=cfg(web)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if arch == "wasm32" && os == "unknown" {
        println!("cargo::rustc-cfg=web");
    }
}
