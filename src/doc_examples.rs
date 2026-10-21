//! Exposes Markdown files in `site/content/` as doc-test targets so `cargo test --doc`
//! verifies their Rust code blocks.

macro_rules! doc_examples {
    ($($name:ident = $path:literal),* $(,)?) => {
        $(
            #[cfg(doctest)]
            #[doc = include_str!($path)]
            struct $name;
        )*

        #[cfg(test)]
        const FILES: &[&str] = &[$($path),*];
    };
}

doc_examples!(SiteIndex = "../site/content/index.md", EmbeddingGuide = "../site/content/guide/15-embedding.md");

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                markdown_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }

    #[test]
    fn every_page_with_rust_examples_is_a_doctest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        markdown_files(&root.join("site/content"), &mut files);
        let src = root.join("src");
        let listed: Vec<PathBuf> = super::FILES.iter().map(|f| src.join(f).canonicalize().unwrap()).collect();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            if text.lines().any(|l| l.trim_start().starts_with("```rust")) {
                assert!(listed.contains(&file.canonicalize().unwrap()), "add {} to `doc_examples!`", file.display());
            }
        }
    }
}
