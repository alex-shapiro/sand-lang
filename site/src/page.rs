//! Shared HTML page layout, `<head>` metadata, navigation header, and footer.

use crate::highlight::escape;
use crate::markdown::Heading;

pub const REPO: &str = "https://github.com/alex-shapiro/sand-lang";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Home,
    Guide,
    Library,
    Spec,
    Playground,
    Other,
}

pub struct Page {
    pub path: String,
    pub title: String,
    pub description: String,
    pub section: Section,
    pub sidebar: String,
    pub toc: Vec<Heading>,
    pub body: String,
}

/// Versioned asset URLs for the site stylesheet and script.
pub struct Assets {
    pub css: String,
    pub js: String,
}

const NAV: &[(&str, &str, Section)] = &[
    ("Guide", "/guide/", Section::Guide),
    ("Library", "/std/", Section::Library),
    ("Spec", "/spec/", Section::Spec),
    ("Playground", "/play/", Section::Playground),
];

pub fn render(page: &Page, assets: &Assets) -> String {
    let title = if page.section == Section::Home { "Sand".to_string() } else { format!("{} – Sand", page.title) };
    let layout = match (page.section, page.sidebar.is_empty()) {
        (Section::Home, _) => "home",
        (_, true) => "plain",
        _ => "docs",
    };
    let main = match layout {
        "docs" => format!(
            r#"<div class="docs">
<nav class="sidebar" aria-label="{section}"><details class="sidebar-menu" open><summary>Contents</summary>{sidebar}</details></nav>
<main id="main" class="content">{body}</main>
{toc}
</div>"#,
            section = escape(&page.title),
            sidebar = page.sidebar,
            body = page.body,
            toc = toc(&page.toc),
        ),
        _ => format!(r#"<main id="main" class="{layout}">{}</main>"#, page.body),
    };
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
{head}
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
{chrome}
{main}
{footer}
</body>
</html>
"##,
        head = head(&title, &page.description, assets),
        chrome = chrome(page.section),
        footer = FOOTER,
    )
}

pub fn head(title: &str, description: &str, assets: &Assets) -> String {
    let (title, description) = (escape(title), escape(description));
    format!(
        r##"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{description}">
<meta property="og:title" content="{title}">
<meta property="og:description" content="{description}">
<meta property="og:image" content="https://sandlang.com/og.png">
<meta name="theme-color" content="#f8f6f0" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#0e1624" media="(prefers-color-scheme: dark)">
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<link rel="stylesheet" href="{css}">
<script type="module" src="{js}"></script>"##,
        css = assets.css,
        js = assets.js,
    )
}

pub fn chrome(section: Section) -> String {
    let nav: String = NAV
        .iter()
        .map(|&(label, href, s)| {
            let current = if s == section { r#" aria-current="page""# } else { "" };
            format!(r#"<a href="{href}"{current}>{label}</a>"#)
        })
        .collect();
    format!(
        r#"<div class="alpha" role="note"><strong>Alpha software.</strong> Sand changes without notice. Do not use it in production.</div>
<header class="site-header">
<a class="brand" href="/" aria-label="Sand home"><img src="/logo.svg" alt="" width="28" height="32"><span>Sand</span></a>
<nav class="site-nav" aria-label="Site">{nav}</nav>
<div class="search" role="search">
<input id="search" type="search" role="combobox" placeholder="Search" aria-label="Search the guide, library and specification" autocomplete="off" spellcheck="false" aria-autocomplete="list" aria-controls="search-results" aria-expanded="false">
<kbd aria-hidden="true">/</kbd>
<ul id="search-results" class="search-results" role="listbox" hidden></ul>
</div>
<a class="repo" href="{REPO}" aria-label="Sand on GitHub">{GITHUB}</a>
</header>"#
    )
}

fn toc(headings: &[Heading]) -> String {
    let items: String = headings
        .iter()
        .filter(|h| h.level == 2 || h.level == 3)
        .map(|h| format!(r##"<li class="toc-{}"><a href="#{}">{}</a></li>"##, h.level, h.id, escape(&h.text)))
        .collect();
    if items.is_empty() {
        return String::new();
    }
    format!(r#"<aside class="toc" aria-label="On this page"><h2>On this page</h2><ul>{items}</ul></aside>"#)
}

pub fn link_list(links: &[(String, String)], current: &str) -> String {
    let items: String = links
        .iter()
        .map(|(href, label)| {
            let mark = if href == current { r#" aria-current="page""# } else { "" };
            format!(r#"<li><a href="{href}"{mark}>{label}</a></li>"#)
        })
        .collect();
    format!("<ul>{items}</ul>")
}

const FOOTER: &str = r#"<footer class="site-footer">
<p>If you are interested in using or contributing to Sand, reach out on <a href="https://github.com/alex-shapiro/sand-lang">GitHub</a>.</p>
</footer>"#;

const GITHUB: &str = r#"<svg viewBox="0 0 16 16" width="20" height="20" aria-hidden="true"><path fill="currentColor" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>"#;
