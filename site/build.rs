//! Turns the Rust code blocks of `content/` into a module that the crate's
//! tests compile (`src/main.rs`), so that they follow the embedding API.
//! A block that defines `fn main` is compiled as it is; any other block is
//! the body of an async function that returns a `Result`. Lines that start
//! with `# ` are compiled but not shown. `rust ignore` blocks are skipped.

use std::fmt::Write;
use std::path::{Path, PathBuf};

fn main() {
    let content = Path::new(env!("CARGO_MANIFEST_DIR")).join("content");
    println!("cargo::rerun-if-changed={}", content.display());
    let mut files = Vec::new();
    markdown_files(&content, &mut files);
    files.sort();
    let mut out = String::new();
    let mut n = 0;
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let name = file.strip_prefix(&content).unwrap().display().to_string();
        let mut lines = text.lines().enumerate();
        while let Some((at, line)) = lines.next() {
            let Some(info) = line.strip_prefix("```rust") else { continue };
            let code: Vec<&str> = lines.by_ref().map(|(_, l)| l).take_while(|l| !l.starts_with("```")).collect();
            if info.split_whitespace().any(|w| w == "ignore") {
                continue;
            }
            let code: String = code.iter().map(|l| format!("{}\n", l.strip_prefix("# ").unwrap_or(l))).collect();
            n += 1;
            writeln!(out, "// content/{name}, line {}", at + 1).unwrap();
            writeln!(out, "#[allow(unused, clippy::all)]\nmod example_{n} {{").unwrap();
            if code.contains("fn main(") {
                out.push_str(&code);
            } else {
                writeln!(out, "async fn example() -> Result<(), Box<dyn std::error::Error>> {{\n{code}Ok(())\n}}").unwrap();
            }
            out.push_str("}\n\n");
        }
    }
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("rust_examples.rs");
    std::fs::write(dest, out).unwrap();
}

fn markdown_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            markdown_files(&path, files);
        } else if path.extension().is_some_and(|e| e == "md") {
            files.push(path);
        }
    }
}
