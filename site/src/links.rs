//! Verification of internal links, heading anchors, and GitHub repository links.

use crate::markdown::github_slug;
use crate::page::REPO;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Checks all `href` and `src` attributes in HTML files under `out` and returns
/// any broken links or missing anchors.
pub fn check(out: &Path, repo: &Path) -> Result<Vec<String>, String> {
    let mut pages = Vec::new();
    collect(out, &mut pages)?;
    let mut ids: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut errors = Vec::new();
    for page in &pages {
        let html = std::fs::read_to_string(page).map_err(|e| format!("{}: {e}", page.display()))?;
        let from = format!("/{}", page.strip_prefix(out).unwrap_or(page).display()).replace("index.html", "");
        for link in attributes(&html, "href").into_iter().chain(attributes(&html, "src")) {
            let (path, fragment) = match link.split_once('#') {
                Some((p, f)) => (p, Some(f)),
                None => (link.as_str(), None),
            };
            let path = path.split('?').next().unwrap_or("");
            let in_repo = repo_file(path);
            let target = if let Some(file) = in_repo {
                repo.join(file)
            } else if path.starts_with("http") || path.starts_with("mailto:") || path.starts_with("data:") {
                continue;
            } else if path.is_empty() {
                page.clone()
            } else if let Some(p) = path.strip_prefix('/') {
                let t = out.join(p);
                if p.is_empty() || p.ends_with('/') { t.join("index.html") } else { t }
            } else {
                page.parent().unwrap_or(out).join(path)
            };
            if !(target.is_file() || in_repo.is_some() && target.exists()) {
                errors.push(format!("{from}: broken link `{link}`"));
                continue;
            }
            if let Some(fragment) = fragment.filter(|f| !f.is_empty()) {
                let known = ids.entry(target.clone()).or_insert_with(|| {
                    let text = std::fs::read_to_string(&target).unwrap_or_default();
                    if target.extension().is_some_and(|e| e == "md") {
                        github_anchors(&text)
                    } else {
                        attributes(&text, "id").into_iter().collect()
                    }
                });
                if !known.contains(fragment) {
                    errors.push(format!("{from}: link `{link}` has an unknown anchor"));
                }
            }
        }
    }
    Ok(errors)
}

fn repo_file(link: &str) -> Option<&str> {
    let rest = link.strip_prefix(REPO)?;
    rest.strip_prefix("/blob/main/").or_else(|| rest.strip_prefix("/tree/main/"))
}

fn github_anchors(markdown: &str) -> HashSet<String> {
    let mut anchors = HashSet::new();
    let mut text: Option<String> = None;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Heading { .. }) => text = Some(String::new()),
            Event::Text(t) | Event::Code(t) if text.is_some() => text.as_mut().unwrap().push_str(&t),
            Event::End(TagEnd::Heading(_)) => {
                let base = github_slug(&text.take().unwrap_or_default());
                let mut anchor = base.clone();
                let mut n = 1;
                while anchors.contains(&anchor) {
                    anchor = format!("{base}-{n}");
                    n += 1;
                }
                anchors.insert(anchor);
            }
            _ => {}
        }
    }
    anchors
}

fn collect(dir: &Path, pages: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            collect(&path, pages)?;
        } else if path.extension().is_some_and(|e| e == "html") {
            pages.push(path);
        }
    }
    Ok(())
}

fn attributes(html: &str, name: &str) -> Vec<String> {
    let needle = format!(" {name}=\"");
    let mut values = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(&needle) {
        rest = &rest[at + needle.len()..];
        let end = rest.find('"').unwrap_or(rest.len());
        values.push(rest[..end].replace("&amp;", "&").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">"));
        rest = &rest[end..];
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_links_and_fragments_are_reported() {
        let dir = std::env::temp_dir().join(format!("sand-site-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("index.html"), r##"<a href="/a/#x">a</a> <a href="/a/#y">a</a> <a href="/b/">b</a> <a href="#top">t</a>"##)
            .unwrap();
        std::fs::write(dir.join("a/index.html"), r#"<h2 id="x">X</h2>"#).unwrap();
        let errors = check(&dir, &dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].contains("`/a/#y` has an unknown anchor"), "{errors:?}");
        assert!(errors[1].contains("broken link `/b/`"), "{errors:?}");
        assert!(errors[2].contains("`#top`"), "{errors:?}");
    }

    #[test]
    fn links_to_the_repository_are_checked() {
        let dir = std::env::temp_dir().join(format!("sand-site-repo-links-{}", std::process::id()));
        let (out, repo) = (dir.join("out"), dir.join("repo"));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/A.md"), "## 1.2 The `a` module\n\n## `?` here\n\n## Notes\n\n## Notes\n").unwrap();
        let links: String =
            ["blob/main/docs/A.md#12-the-a-module", "blob/main/docs/A.md#-here", "blob/main/docs/A.md#notes-1", "tree/main/docs"]
                .iter()
                .chain(&["blob/main/docs/A.md#nope", "blob/main/docs/B.md"])
                .map(|l| format!(r#"<a href="{REPO}/{l}">x</a>"#))
                .collect();
        std::fs::write(out.join("index.html"), links).unwrap();
        let errors = check(&out, &repo).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("A.md#nope` has an unknown anchor"), "{errors:?}");
        assert!(errors[1].contains("broken link") && errors[1].contains("B.md"), "{errors:?}");
    }
}
