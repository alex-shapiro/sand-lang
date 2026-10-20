//! Builds the website at sandlang.com from `site/` and the standard
//! library, and serves it locally. See `site/README.md`.

mod examples;
mod highlight;
mod links;
mod markdown;
mod page;
mod reference;
mod search;
mod serve;

/// The Rust code blocks of `content/`, compiled to check them (`build.rs`).
#[cfg(test)]
mod rust_examples {
    include!(concat!(env!("OUT_DIR"), "/rust_examples.rs"));
}

use markdown::{Checks, Heading, Style};
use page::{Assets, Page, Section};
use search::Entry;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo site build [--out=DIR] [--engine=release|dev|none]
       cargo site serve [--out=DIR] [--engine=release|dev|none] [--port=PORT]

build   writes the site to DIR (default: target/site), checking every example
        and internal link; exits with 1 if any check fails
serve   builds the site, then serves it at http://127.0.0.1:PORT/ (default 8000)
        even if checks fail

--engine  how to build the playground's WebAssembly engine (default: release
          for build, dev for serve); `none` keeps the engine already in DIR
";

/// Marks a directory as the site's output, which a build may clear.
const MARKER: &str = ".sand-site";

#[derive(Clone, Copy, PartialEq, Eq)]
enum EngineBuild {
    Release,
    Dev,
    None,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("site: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((command, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let site = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = site.parent().ok_or("the site crate has no parent directory")?;
    let mut out = repo.join("target/site");
    let mut engine = if command == "serve" { EngineBuild::Dev } else { EngineBuild::Release };
    let mut port = 8000;
    for a in rest {
        if let Some(dir) = a.strip_prefix("--out=") {
            out = PathBuf::from(dir);
        } else if let Some(e) = a.strip_prefix("--engine=") {
            engine = match e {
                "release" => EngineBuild::Release,
                "dev" => EngineBuild::Dev,
                "none" => EngineBuild::None,
                _ => return Err(format!("unknown engine build `{e}`\n\n{USAGE}")),
            };
        } else if let Some(p) = a.strip_prefix("--port=").filter(|_| command == "serve") {
            port = p.parse().map_err(|_| format!("invalid port `{p}`"))?;
        } else {
            return Err(format!("unexpected argument `{a}`\n\n{USAGE}"));
        }
    }
    match command.as_str() {
        "build" => {
            let errors = build(repo, &out, engine)?;
            Ok(if report(&errors) { ExitCode::SUCCESS } else { ExitCode::from(1) })
        }
        "serve" => {
            let errors = build(repo, &out, engine)?;
            report(&errors);
            serve::serve(&out, port).map(|()| ExitCode::SUCCESS)
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

/// Prints the failed checks; true if there were none.
fn report(errors: &[String]) -> bool {
    for e in errors {
        eprintln!("error: {}\n", e.trim_end());
    }
    if !errors.is_empty() {
        eprintln!("{} check(s) failed", errors.len());
    }
    errors.is_empty()
}

/// Writes the site to `out`; returns the checks that failed.
fn build(repo: &Path, out: &Path, engine: EngineBuild) -> Result<Vec<String>, String> {
    let site = repo.join("site");
    prepare_output(out, engine)?;
    let assets = copy_static(&site.join("static"), out)?;
    if engine != EngineBuild::None {
        build_engine(repo, &out.join("play/pkg"), engine)?;
    }
    copy_playground(&site.join("playground"), &out.join("play"), &assets)?;

    let mut checks = Checks::new();
    let mut pages = Vec::new();
    let mut entries = Vec::new();

    let docs = sandlang::Engine::new(sandlang::Config::new()).docs().map_err(|e| e.render())?;
    let (reference_pages, reference_entries) = reference::Reference::new(docs).pages(&mut checks);
    pages.extend(reference_pages);

    let (guide_pages, guide_entries) = guide(&site.join("content/guide"), &mut checks)?;
    pages.extend(guide_pages);
    entries.extend(guide_entries);
    entries.extend(reference_entries);

    pages.push(home(&site.join("content/index.md"), &mut checks)?);
    pages.push(not_found());

    for p in &pages {
        let file = if p.path == "/404.html" { out.join("404.html") } else { out.join(p.path.trim_start_matches('/')).join("index.html") };
        write(&file, &page::render(p, &assets))?;
    }
    write(&out.join("search-index.json"), &search::index_json(&entries))?;
    write(&out.join("sitemap.xml"), &sitemap(&pages))?;
    write(&out.join("robots.txt"), "User-agent: *\nAllow: /\nSitemap: https://sandlang.com/sitemap.xml\n")?;

    let mut errors = checks.errors;
    errors.extend(links::check(out, repo)?);
    println!("Built {} pages and checked {} examples into {}.", pages.len() + 1, checks.examples, out.display());
    Ok(errors)
}

/// Makes `out` an empty output directory, keeping a built engine when it
/// will not be rebuilt.
fn prepare_output(out: &Path, engine: EngineBuild) -> Result<(), String> {
    if out.exists() {
        if !out.join(MARKER).exists() && std::fs::read_dir(out).map_err(|e| e.to_string())?.next().is_some() {
            return Err(format!("{} is not empty and was not made by this tool; choose another --out", out.display()));
        }
        for entry in std::fs::read_dir(out).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if engine == EngineBuild::None && path.ends_with("play") {
                for inner in std::fs::read_dir(&path).map_err(|e| e.to_string())? {
                    let inner = inner.map_err(|e| e.to_string())?.path();
                    if !inner.ends_with("pkg") {
                        remove(&inner)?;
                    }
                }
                continue;
            }
            remove(&path)?;
        }
    }
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    write(&out.join(MARKER), "Built by `cargo site build`; the next build replaces everything here.\n")
}

fn remove(path: &Path) -> Result<(), String> {
    let r = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    r.map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Copies `site/static` and returns the URLs of the stylesheet and script.
fn copy_static(from: &Path, out: &Path) -> Result<Assets, String> {
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path.file_name().unwrap_or_default();
        std::fs::copy(&path, out.join(name)).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let versioned = |name: &str| -> Result<String, String> {
        let data = std::fs::read(from.join(name)).map_err(|e| format!("{name}: {e}"))?;
        Ok(format!("/{name}?v={:08x}", fnv(&data) as u32))
    };
    Ok(Assets { css: versioned("site.css")?, js: versioned("site.js")? })
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// Builds the playground's engine into `pkg` with `wasm/build.sh`.
fn build_engine(repo: &Path, pkg: &Path, engine: EngineBuild) -> Result<(), String> {
    let mut cmd = Command::new("sh");
    cmd.arg(repo.join("wasm/build.sh")).arg("web").arg(pkg);
    if engine == EngineBuild::Dev {
        cmd.arg("--dev");
    }
    let status = cmd.status().map_err(|e| format!("cannot run wasm/build.sh: {e}"))?;
    if !status.success() {
        return Err("wasm/build.sh failed; build without the engine with --engine=none".into());
    }
    Ok(())
}

/// Copies the playground, giving its page the site's header.
fn copy_playground(from: &Path, to: &Path, assets: &Assets) -> Result<(), String> {
    for entry in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        match name.as_str() {
            "README.md" => {}
            "index.html" => {
                let html = read(&path)?
                    .replace("<!-- site-head -->", &format!(r#"<link rel="stylesheet" href="{}">"#, assets.css))
                    .replace("<!-- site-chrome -->", &page::chrome(Section::Playground));
                write(&to.join(name), &html)?;
            }
            _ => write(&to.join(name), &read(&path)?)?,
        }
    }
    Ok(())
}

/// The guide's chapters (`NN-slug.md`, in order) and its index page.
fn guide(dir: &Path, checks: &mut Checks) -> Result<(Vec<Page>, Vec<Entry>), String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    struct Chapter {
        slug: String,
        title: String,
        description: String,
        rendered: markdown::Rendered,
    }
    let mut chapters = Vec::new();
    for file in &files {
        let stem = file.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let slug = stem.split_once('-').map(|(_, s)| s.to_string()).unwrap_or(stem);
        let text = read(file)?;
        let (fields, body) = markdown::front_matter(&text);
        let source = format!("site/content/guide/{}", file.file_name().unwrap_or_default().to_string_lossy());
        let title = fields.get("title").cloned().ok_or(format!("{source}: no title"))?;
        let description = fields.get("description").cloned().unwrap_or_default();
        let rendered = markdown::render(body, &Style::page(&source, true), checks);
        chapters.push(Chapter { slug, title, description, rendered });
    }
    let links: Vec<(String, String)> = chapters.iter().map(|c| (format!("/guide/{}/", c.slug), highlight::escape(&c.title))).collect();
    let sidebar = |current: &str| format!(r#"<h2><a href="/guide/">Guide</a></h2>{}"#, page::link_list(&links, current));
    let mut pages = Vec::new();
    let mut entries = vec![Entry::new("Guide", "guide", "/guide/", "A tour of the Sand language.")];
    let mut index = String::from(
        r#"<h1>The Sand guide</h1>
<p class="lede">This guide teaches Sand chapter by chapter, from the first program to embedding the engine in Rust. The Sand compiler checks every example when the site is built, and the output below an example is what it printed.</p>
<ol class="chapters">"#,
    );
    for (i, c) in chapters.iter().enumerate() {
        let url = format!("/guide/{}/", c.slug);
        index.push_str(&format!(
            r#"<li><a href="{url}">{}</a><p>{}</p></li>"#,
            highlight::escape(&c.title),
            highlight::escape(&c.description)
        ));
        entries.push(Entry::new(&c.title, "guide", &url, &c.description));
        for h in c.rendered.headings.iter().filter(|h| h.level == 2) {
            entries.push(Entry::new(&format!("{} › {}", c.title, h.text), "guide", &format!("{url}#{}", h.id), ""));
        }
        let pager = |j: Option<usize>, rel: &str, label: &str| match j.and_then(|j| chapters.get(j)) {
            Some(o) => {
                format!(r#"<a class="{rel}" rel="{rel}" href="/guide/{}/"><span>{label}</span>{}</a>"#, o.slug, highlight::escape(&o.title))
            }
            None => "<span></span>".into(),
        };
        let body = format!(
            r#"<p class="crumbs"><a href="/guide/">Guide</a> / Chapter {}</p><h1>{}</h1>{}<nav class="pager" aria-label="Chapters">{}{}</nav>"#,
            i + 1,
            highlight::escape(&c.title),
            c.rendered.html,
            pager(i.checked_sub(1), "prev", "Previous"),
            pager(Some(i + 1), "next", "Next"),
        );
        pages.push(Page {
            path: url.clone(),
            title: c.title.clone(),
            description: c.description.clone(),
            section: Section::Guide,
            sidebar: sidebar(&url),
            toc: c.rendered.headings.iter().map(|h| Heading { level: h.level, text: h.text.clone(), id: h.id.clone() }).collect(),
            body,
        });
    }
    index.push_str("</ol>");
    pages.push(Page {
        path: "/guide/".into(),
        title: "Guide".into(),
        description: "A tour of the Sand language, from the first program to embedding.".into(),
        section: Section::Guide,
        sidebar: sidebar("/guide/"),
        toc: Vec::new(),
        body: index,
    });
    Ok((pages, entries))
}

fn home(path: &Path, checks: &mut Checks) -> Result<Page, String> {
    let text = read(path)?;
    let (fields, body) = markdown::front_matter(&text);
    let rendered = markdown::render(body, &Style::page("site/content/index.md", true), checks);
    Ok(Page {
        path: "/".into(),
        title: "Sand".into(),
        description: fields.get("description").cloned().unwrap_or_default(),
        section: Section::Home,
        sidebar: String::new(),
        toc: Vec::new(),
        body: rendered.html,
    })
}

fn not_found() -> Page {
    Page {
        path: "/404.html".into(),
        title: "Page not found".into(),
        description: "This page does not exist.".into(),
        section: Section::Other,
        sidebar: String::new(),
        toc: Vec::new(),
        body:
            r#"<h1>Page not found</h1><p>This page does not exist. Search for it above, or start from the <a href="/">home page</a>.</p>"#
                .into(),
    }
}

fn sitemap(pages: &[Page]) -> String {
    let urls: String =
        pages.iter().filter(|p| p.path.ends_with('/')).map(|p| format!("<url><loc>https://sandlang.com{}</loc></url>\n", p.path)).collect();
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n{urls}</urlset>\n")
}
