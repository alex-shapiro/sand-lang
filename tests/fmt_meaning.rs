//! Formatting preserves meaning: for every source of the spec tests and
//! every example, the formatted text has the same syntax tree (positions
//! aside) and its block comments between the same tokens (see
//! `sandlang::fmt::meaning`), and formatting it again changes nothing.

use sandlang::fmt::{format_source, meaning};
use std::path::{Path, PathBuf};

/// The sources of the cases of a `.test` file (see tests/spec_runner.rs).
fn cases(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut in_source = false;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("=== ") {
            out.push((name.to_string(), String::new()));
            in_source = true;
        } else if line.starts_with("--- ") {
            in_source = false;
        } else if in_source && let Some((_, src)) = out.last_mut() {
            src.push_str(line);
            src.push('\n');
        }
    }
    out
}

fn files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}

#[test]
fn formatting_preserves_meaning() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    let mut paths = Vec::new();
    files(&root.join("tests/spec"), "test", &mut paths);
    for p in &paths {
        for (name, src) in cases(&std::fs::read_to_string(p).unwrap()) {
            sources.push((format!("{} / {name}", p.display()), src));
        }
    }
    paths.clear();
    files(&root.join("examples"), "sand", &mut paths);
    sources.extend(paths.iter().map(|p| (p.display().to_string(), std::fs::read_to_string(p).unwrap())));

    let mut failures = Vec::new();
    let mut checked = 0;
    for (name, src) in &sources {
        // Cases with syntax errors are not formatted.
        let Ok(before) = meaning(src) else { continue };
        checked += 1;
        let formatted = match format_source(src) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{name}: cannot format: {e}"));
                continue;
            }
        };
        match meaning(&formatted) {
            Ok(after) if after == before => {}
            Ok(after) => {
                let line = before.lines().zip(after.lines()).position(|(a, b)| a != b).unwrap_or(0);
                let show = |m: &str| m.lines().skip(line.saturating_sub(3)).take(8).collect::<Vec<_>>().join("\n");
                failures.push(format!(
                    "{name}: meaning changed\n--- source\n{src}--- formatted\n{formatted}--- before\n{}\n--- after\n{}",
                    show(&before),
                    show(&after)
                ));
            }
            Err(e) => failures.push(format!("{name}: formatted source does not parse: {e}\n{formatted}")),
        }
        if format_source(&formatted).as_deref() != Ok(formatted.as_str()) {
            failures.push(format!("{name}: formatting is not idempotent"));
        }
    }
    assert!(checked > 500, "only {checked} sources checked");
    assert!(failures.is_empty(), "{} of {checked} sources:\n\n{}", failures.len(), failures.join("\n\n"));
}

/// A block comment between any two tokens stays between them.
#[test]
fn block_comments_stay_in_place() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources: Vec<(String, String)> = Vec::new();
    for file in ["fmt", "patterns", "structs", "traits"] {
        let p = root.join(format!("tests/spec/{file}.test"));
        sources.extend(cases(&std::fs::read_to_string(&p).unwrap()).into_iter().map(|(n, s)| (format!("{file} / {n}"), s)));
    }
    let mut failures = Vec::new();
    for (name, src) in &sources {
        let parsed = sandlang::syntax::parse_module(src, 0);
        if !parsed.errors.is_empty() {
            continue;
        }
        // After each token (at most 150 per source).
        for t in parsed.tokens.iter().filter(|t| t.hi > t.lo).take(150) {
            let at = t.hi as usize;
            let commented = format!("{} /* c */{}", &src[..at], &src[at..]);
            let Ok(before) = meaning(&commented) else { continue };
            let Ok(formatted) = format_source(&commented) else {
                failures.push(format!("{name}: cannot format\n{commented}"));
                continue;
            };
            if meaning(&formatted).as_ref() != Ok(&before) {
                failures.push(format!("{name}:\n{commented}--- formatted\n{formatted}"));
            }
        }
    }
    assert!(failures.is_empty(), "{} failures:\n\n{}", failures.len(), failures.iter().take(20).cloned().collect::<Vec<_>>().join("\n\n"));
}

/// Found by the `fmt` fuzz target. The formatter removes trailing spaces, and
/// it can move an item away from a `///` that follows code on the same line.
/// Thus documentation does not include trailing spaces, and a `///` after code
/// does not document an item. A `//!` after code is an error, because the
/// formatter can move it into the module documentation.
#[test]
fn trailing_spaces_in_doc_comments_do_not_change_the_meaning() {
    for src in [
        "//!( ",
        "//! a \n//!\t\nfn f() {}\n",
        "/// b  \nfn f() {}\n",
        "\"\"///\nfn f(){}",
        "let x = 1 /// c\nfn f() {}\n",
        "let l=///\n3",
        "///!",
        "let x = 1\n////\n",
        "///\n//!",
        "/* a */\n//! b\nlet x = 1\n",
    ] {
        let formatted = format_source(src).unwrap();
        assert_eq!(meaning(src), meaning(&formatted), "{src:?} formatted as {formatted:?}");
    }
    assert!(meaning("//!\0\ni//! \n/ t").is_err());
}

/// Found by the `fmt` fuzz target. The formatter keeps the space in a tuple
/// index on a number literal (`1 .0`), because `1.0` is a float literal.
#[test]
fn tuple_indices_on_number_literals_stay_indices() {
    for src in ["(1 .0)\n", "print(2.5 .1)\n", "print(2 .0 .1)\n"] {
        let formatted = format_source(src).unwrap();
        assert_eq!(meaning(src), meaning(&formatted), "{src:?} formatted as {formatted:?}");
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }
}

/// Found by the `fmt` fuzz target. The formatter keeps `..base` if it is the
/// only argument.
#[test]
fn a_lone_base_argument_keeps_its_dots() {
    for src in ["t(..3)\n", "let q = P(..p)\n", "let q = P(\n    ..p,\n)\n", "let q = P(x: 1, /* c */ ..p)\n"] {
        let formatted = format_source(src).unwrap();
        assert_eq!(meaning(src), meaning(&formatted), "{src:?} formatted as {formatted:?}");
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }
}

/// Found by the `fmt` fuzz target. A keyword used as a member name (`x.or`)
/// at the end of a line ends the statement, as any other name does.
#[test]
fn a_member_named_by_a_keyword_ends_a_statement() {
    for src in ["l.or;()\n", "l.and\n(1, 2)\n", "x.if\n[1]\n"] {
        let formatted = format_source(src).unwrap();
        assert_eq!(meaning(src), meaning(&formatted), "{src:?} formatted as {formatted:?}");
        assert_eq!(format_source(&formatted).unwrap(), formatted, "{src:?}");
    }
}
