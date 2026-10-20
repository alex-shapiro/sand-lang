//! The library reference (`/std/`), generated from the documentation the
//! compiler extracts from the standard library (`Engine::docs`).
//!
//! Each module has a page listing its items, with its functions and values
//! in full; each type and trait has a page of its own. Names of types in
//! declarations, and inline code in doc comments that names an item, link
//! to the item.

use crate::highlight::{self, escape};
use crate::markdown::{self, Checks, Heading, Style};
use crate::page::{Page, Section};
use crate::search::Entry;
use sandlang::docs::{ItemDoc, ItemKind, MemberDoc, MemberKind, ModuleDoc, ModuleKind};
use std::collections::HashMap;

pub struct Reference {
    modules: Vec<ModuleDoc>,
    /// Type and trait names (as written in Sand) by module.
    types: HashMap<String, HashMap<String, String>>,
}

impl Reference {
    pub fn new(modules: Vec<ModuleDoc>) -> Reference {
        let mut types: HashMap<String, HashMap<String, String>> = HashMap::new();
        for m in &modules {
            let names = types.entry(m.name.clone()).or_default();
            for item in m.items.iter().filter(|i| has_page(i)) {
                let url = item_url(&m.name, item);
                names.insert(item_slug(item).to_string(), url.clone());
                names.insert(item.name.clone(), url);
            }
        }
        Reference { modules, types }
    }

    /// The URL of the type or trait `name` as code in module `module` sees
    /// it: its own, the prelude's, or the only one of that name.
    fn type_url(&self, module: &str, name: &str) -> Option<String> {
        if let Some(url) = self.types.get(module).and_then(|t| t.get(name)) {
            return Some(url.clone());
        }
        if let Some(url) = self.types.get("prelude").and_then(|t| t.get(name)) {
            return Some(url.clone());
        }
        let mut found = self.types.values().filter_map(|t| t.get(name));
        match (found.next(), found.next()) {
            (Some(url), None) => Some(url.clone()),
            _ => None,
        }
    }

    fn module(&self, name: &str) -> Option<&ModuleDoc> {
        self.modules.iter().find(|m| m.name == name)
    }

    /// The URL of what the inline code `text` names, seen from `module`:
    /// `Json`, `parse`, `json.parse`, `str.split`, `json.Json.get`, `print()`.
    pub fn resolve(&self, module: &str, text: &str) -> Option<String> {
        let text = text.strip_suffix("()").unwrap_or(text);
        if !text.split('.').all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            return None;
        }
        let parts: Vec<&str> = text.split('.').collect();
        match parts.as_slice() {
            [name] => self.type_url(module, name).or_else(|| self.item_anchor(module, name)).or_else(|| self.item_anchor("prelude", name)),
            [first, second] => match self.module(first) {
                Some(_) if *first != module || self.type_url(module, first).is_none() => {
                    self.types.get(*first).and_then(|t| t.get(*second)).cloned().or_else(|| self.item_anchor(first, second))
                }
                _ => self.member_anchor(module, first, second),
            },
            [m, ty, member] if self.module(m).is_some() => self.member_anchor(m, ty, member),
            _ => None,
        }
    }

    /// The anchor of a function or value of a module.
    fn item_anchor(&self, module: &str, name: &str) -> Option<String> {
        let m = self.module(module)?;
        let item = m.items.iter().find(|i| i.name == name && !has_page(i))?;
        Some(format!("{}#{}", module_url(&m.name), item.name))
    }

    fn member_anchor(&self, module: &str, ty: &str, member: &str) -> Option<String> {
        let url = self.type_url(module, ty)?;
        let mut segments = url.trim_matches('/').split('/').skip(1);
        let (mname, slug) = (segments.next()?, segments.next()?);
        let m = self.module(mname)?;
        let item = m.items.iter().find(|i| item_slug(i) == slug)?;
        let anchors = member_anchors(&item.members);
        let i = item.members.iter().position(|x| x.name == member)?;
        Some(format!("{url}#{}", anchors[i]))
    }

    /// The pages of the reference and their search entries.
    pub fn pages(&self, checks: &mut Checks) -> (Vec<Page>, Vec<Entry>) {
        let mut pages = vec![self.index_page()];
        let mut entries = Vec::new();
        for m in &self.modules {
            pages.push(self.module_page(m, checks, &mut entries));
            for item in m.items.iter().filter(|i| has_page(i)) {
                pages.push(self.item_page(m, item, checks, &mut entries));
            }
        }
        (pages, entries)
    }

    fn index_page(&self) -> Page {
        let mut body = String::from(
            r#"<h1>Library reference</h1>
<p class="lede">The prelude and the standard modules, generated from their source. Scripts use the prelude without an import, and import each module by name: <code>import json</code>.</p>"#,
        );
        for (title, kind) in [("Prelude", ModuleKind::Prelude), ("Standard modules", ModuleKind::Std), ("Host modules", ModuleKind::Host)] {
            let rows: String = self
                .modules
                .iter()
                .filter(|m| m.kind == kind)
                .map(|m| {
                    let summary = m.doc.as_deref().map(first_sentence).unwrap_or_default();
                    format!(
                        r#"<tr><th scope="row"><a href="{}"><code>{}</code></a></th><td>{}</td></tr>"#,
                        module_url(&m.name),
                        escape(&m.name),
                        inline_markdown(&summary)
                    )
                })
                .collect();
            if !rows.is_empty() {
                body.push_str(&format!(r#"<h2 id="{}">{title}</h2><table class="modules">{rows}</table>"#, markdown::slug(title)));
            }
        }
        Page {
            path: "/std/".into(),
            title: "Library reference".into(),
            description: "Reference documentation of the Sand prelude and standard library.".into(),
            section: Section::Library,
            sidebar: self.sidebar(None, ""),
            toc: Vec::new(),
            body,
        }
    }

    fn module_page(&self, m: &ModuleDoc, checks: &mut Checks, entries: &mut Vec<Entry>) -> Page {
        let url = module_url(&m.name);
        let title = if m.kind == ModuleKind::Prelude { "Prelude".to_string() } else { format!("module {}", m.name) };
        let summary = m.doc.as_deref().map(first_sentence).unwrap_or_default();
        entries.push(Entry::new(&m.name, "module", &url, &summary));
        let mut body = format!(r#"<p class="crumbs"><a href="/std/">Library</a></p><h1>{}</h1>"#, escape(&title));
        if m.kind != ModuleKind::Prelude {
            body.push_str(&format!(r#"<pre class="import"><code>{}</code></pre>"#, highlight::sand(&format!("import {}", m.name))));
        }
        if let Some(doc) = &m.doc {
            body.push_str(&self.doc_html(&m.name, doc, &format!("module {}", m.name), checks));
        }
        let mut toc = Vec::new();
        type Belongs = fn(&ItemDoc) -> bool;
        let groups: [(&str, Belongs); 5] = [
            ("Built-in types", |i| i.kind == ItemKind::Type && i.decl.starts_with("type ") && is_builtin(i)),
            ("Types", |i| i.kind == ItemKind::Type && !is_builtin(i)),
            ("Traits", |i| i.kind == ItemKind::Trait),
            ("Functions", |i| i.kind == ItemKind::Function),
            ("Values", |i| i.kind == ItemKind::Value),
        ];
        for (heading, belongs) in groups {
            let items: Vec<&ItemDoc> = m.items.iter().filter(|i| belongs(i)).collect();
            if items.is_empty() {
                continue;
            }
            let id = markdown::slug(heading);
            body.push_str(&format!(
                r##"<h2 id="{id}">{heading}<a class="anchor" href="#{id}" aria-label="Link to this section">#</a></h2>"##
            ));
            toc.push(Heading { level: 2, text: heading.into(), id });
            if has_page(items[0]) {
                body.push_str(r#"<dl class="item-list">"#);
                for item in items {
                    let url = item_url(&m.name, item);
                    let summary = item.doc.as_deref().map(first_sentence).unwrap_or_default();
                    entries.push(Entry::new(&qualified(m, item_slug(item)), kind_name(item), &url, &summary));
                    body.push_str(&format!(
                        r#"<dt><a href="{url}"><code>{}</code></a></dt><dd>{}</dd>"#,
                        escape(&item.name),
                        inline_markdown(&summary)
                    ));
                }
                body.push_str("</dl>");
            } else {
                for item in items {
                    let anchor = item.name.clone();
                    let summary = item.doc.as_deref().map(first_sentence).unwrap_or_default();
                    entries.push(Entry::new(&qualified(m, &item.name), kind_name(item), &format!("{url}#{anchor}"), &summary));
                    toc.push(Heading { level: 3, text: item.name.clone(), id: anchor.clone() });
                    body.push_str(&self.entry_html(&m.name, &anchor, &item.decl, item.doc.as_deref(), &url, checks));
                }
            }
        }
        Page {
            path: url.clone(),
            title: if m.kind == ModuleKind::Prelude { "Prelude".into() } else { m.name.clone() },
            description: if summary.is_empty() { format!("The {} module of the Sand standard library.", m.name) } else { plain(&summary) },
            section: Section::Library,
            sidebar: self.sidebar(Some(m), &url),
            toc,
            body,
        }
    }

    fn item_page(&self, m: &ModuleDoc, item: &ItemDoc, checks: &mut Checks, entries: &mut Vec<Entry>) -> Page {
        let url = item_url(&m.name, item);
        let module_link = if m.kind == ModuleKind::Prelude { "Prelude".to_string() } else { m.name.clone() };
        let mut body = format!(
            r#"<p class="crumbs"><a href="/std/">Library</a> / <a href="{}">{}</a></p><h1>{}</h1>"#,
            module_url(&m.name),
            escape(&module_link),
            escape(&item.name)
        );
        body.push_str(&format!(r#"<pre class="decl"><code>{}</code></pre>"#, highlight::sand(&item.decl)));
        if let Some(doc) = &item.doc {
            body.push_str(&self.doc_html(&m.name, doc, &format!("{} {}", m.name, item.name), checks));
        }
        let mut toc = Vec::new();
        if !item.related.is_empty() {
            let (heading, id) =
                if item.kind == ItemKind::Trait { ("Implemented for", "implementors") } else { ("Implements", "implements") };
            let list: String = item
                .related
                .iter()
                .map(|r| format!("<li><code>{}</code></li>", highlight::sand_linked(r, &|w| self.type_url(&m.name, w))))
                .collect();
            body.push_str(&format!(r#"<p class="related" id="{id}"><span>{heading}:</span></p><ul class="related-list">{list}</ul>"#));
        }
        let anchors = member_anchors(&item.members);
        for (title, kind) in [("Variants", MemberKind::Variant), ("Fields", MemberKind::Field), ("Methods", MemberKind::Method)] {
            let members: Vec<(usize, &MemberDoc)> = item.members.iter().enumerate().filter(|(_, x)| x.kind == kind).collect();
            if members.is_empty() {
                continue;
            }
            let id = markdown::slug(title);
            body.push_str(&format!(
                r##"<h2 id="{id}">{title}<a class="anchor" href="#{id}" aria-label="Link to this section">#</a></h2>"##
            ));
            toc.push(Heading { level: 2, text: title.into(), id });
            for (i, member) in members {
                let anchor = &anchors[i];
                let summary = member.doc.as_deref().map(first_sentence).unwrap_or_default();
                let name = format!("{}.{}", qualified(m, item_slug(item)), member.name);
                entries.push(Entry::new(&name, kind_label(kind), &format!("{url}#{anchor}"), &summary));
                toc.push(Heading { level: 3, text: member.name.clone(), id: anchor.clone() });
                body.push_str(&self.entry_html(&m.name, anchor, &member.decl, member.doc.as_deref(), &url, checks));
            }
        }
        Page {
            path: url.clone(),
            title: qualified(m, &item.name),
            description: item
                .doc
                .as_deref()
                .map(first_sentence)
                .map(|s| plain(&s))
                .unwrap_or_else(|| format!("{} in the Sand library.", item.name)),
            section: Section::Library,
            sidebar: self.sidebar(Some(m), &url),
            toc,
            body,
        }
    }

    /// A declaration with an anchor, and its documentation.
    fn entry_html(&self, module: &str, anchor: &str, decl: &str, doc: Option<&str>, page: &str, checks: &mut Checks) -> String {
        let self_url = |w: &str| self.type_url(module, w).filter(|u| u != page);
        let decl = highlight::sand_linked(decl, &self_url);
        let doc = doc.map(|d| self.doc_html(module, d, &format!("{module} {anchor}"), checks)).unwrap_or_default();
        format!(
            r##"<section class="entry" id="{anchor}"><h3 class="decl"><a class="anchor" href="#{anchor}" aria-label="Link to this item">#</a><code>{decl}</code></h3>{doc}</section>"##
        )
    }

    fn doc_html(&self, module: &str, doc: &str, source: &str, checks: &mut Checks) -> String {
        let link = |text: &str| self.resolve(module, text);
        let style = Style { source, verify: false, link_code: &link, heading_offset: 2 };
        let doc = with_sand_fences(doc);
        format!(r#"<div class="doc">{}</div>"#, markdown::render(&doc, &style, checks).html)
    }

    fn sidebar(&self, current: Option<&ModuleDoc>, url: &str) -> String {
        let mut html = String::from(r#"<h2><a href="/std/">Library</a></h2><ul>"#);
        for m in &self.modules {
            let href = module_url(&m.name);
            let mark = if href == url { r#" aria-current="page""# } else { "" };
            let label = if m.kind == ModuleKind::Prelude { "Prelude" } else { &m.name };
            html.push_str(&format!(r#"<li><a href="{href}"{mark}>{label}</a>"#));
            if current.is_some_and(|c| c.name == m.name) {
                let items: Vec<(String, String)> = m
                    .items
                    .iter()
                    .filter(|i| has_page(i))
                    .map(|i| (item_url(&m.name, i), format!("<code>{}</code>", escape(&i.name))))
                    .collect();
                if !items.is_empty() {
                    html.push_str(&crate::page::link_list(&items, url));
                }
            }
            html.push_str("</li>");
        }
        html.push_str("</ul>");
        html
    }
}

/// Doc comments write examples without a language; they are Sand.
fn with_sand_fences(doc: &str) -> String {
    let mut out = String::new();
    let mut open = false;
    for line in doc.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if !open && trimmed == "```" {
                out.push_str(&line.replace("```", "```sand"));
            } else {
                out.push_str(line);
            }
            open = !open;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn has_page(item: &ItemDoc) -> bool {
    matches!(item.kind, ItemKind::Type | ItemKind::Trait) && !item.decl.contains(" = ")
}

fn is_builtin(item: &ItemDoc) -> bool {
    item.decl == format!("type {}", item.name)
        && (item.name.contains('<') || item.name == "Range" || item.name.starts_with(|c: char| c.is_lowercase()))
}

pub fn module_url(name: &str) -> String {
    format!("/std/{name}/")
}

/// The name without type parameters: `List` for the built-in `List<T>`.
fn item_slug(item: &ItemDoc) -> &str {
    item.name.split('<').next().unwrap_or(&item.name)
}

fn item_url(module: &str, item: &ItemDoc) -> String {
    format!("/std/{module}/{}/", item_slug(item))
}

/// `json.Json`, but `str` and `Option` for the prelude's items.
fn qualified(m: &ModuleDoc, name: &str) -> String {
    if m.kind == ModuleKind::Prelude { name.to_string() } else { format!("{}.{name}", m.name) }
}

/// Anchors of members, unique within their type: `method.sum`, `method.sum-2`.
fn member_anchors(members: &[MemberDoc]) -> Vec<String> {
    let mut anchors: Vec<String> = Vec::new();
    for m in members {
        let base = format!("{}.{}", kind_label(m.kind), m.name);
        let mut anchor = base.clone();
        let mut n = 2;
        while anchors.contains(&anchor) {
            anchor = format!("{base}-{n}");
            n += 1;
        }
        anchors.push(anchor);
    }
    anchors
}

fn kind_label(kind: MemberKind) -> &'static str {
    match kind {
        MemberKind::Variant => "variant",
        MemberKind::Field => "field",
        MemberKind::Method => "method",
    }
}

fn kind_name(item: &ItemDoc) -> &'static str {
    match item.kind {
        ItemKind::Type => "type",
        ItemKind::Trait => "trait",
        ItemKind::Function => "fn",
        ItemKind::Value => "value",
    }
}

/// The first sentence of the first paragraph of a doc comment.
pub fn first_sentence(doc: &str) -> String {
    let para: Vec<&str> = doc.lines().map(str::trim).skip_while(|l| l.is_empty()).take_while(|l| !l.is_empty()).collect();
    let text = para.join(" ");
    let end = text
        .match_indices(". ")
        .map(|(i, _)| i)
        .find(|&i| {
            let word = text[..i].rsplit(' ').next().unwrap_or("").trim_start_matches(|c: char| !c.is_alphanumeric());
            !matches!(word, "e.g" | "i.e" | "etc" | "vs")
        })
        .map_or(text.len(), |i| i + 1);
    text[..end].to_string()
}

/// Inline Markdown (code spans) of a summary as HTML.
fn inline_markdown(text: &str) -> String {
    let mut out = String::new();
    for (i, part) in text.split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str(&format!("<code>{}</code>", escape(part)));
        } else {
            out.push_str(&escape(part));
        }
    }
    out
}

/// A summary without Markdown, for meta descriptions.
fn plain(text: &str) -> String {
    text.replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> Reference {
        Reference::new(sandlang::Engine::new(sandlang::Config::new()).docs().unwrap())
    }

    #[test]
    fn inline_code_resolves_to_items() {
        let r = reference();
        assert_eq!(r.resolve("regex", "Regex").as_deref(), Some("/std/regex/Regex/"));
        assert_eq!(r.resolve("regex", "regex.new").as_deref(), Some("/std/regex/#new"));
        assert_eq!(r.resolve("json", "Json.get").as_deref(), Some("/std/json/Json/#method.get"));
        assert_eq!(r.resolve("prelude", "str.split").as_deref(), Some("/std/prelude/str/#method.split"));
        assert_eq!(r.resolve("json", "Option").as_deref(), Some("/std/prelude/Option/"));
        assert_eq!(r.resolve("json", "print()").as_deref(), Some("/std/prelude/#print"));
        assert_eq!(r.resolve("time", "json.Json.get").as_deref(), Some("/std/json/Json/#method.get"));
        assert_eq!(r.resolve("json", "x + 1"), None);
    }

    #[test]
    fn builtin_types_have_pages() {
        let r = reference();
        assert_eq!(r.type_url("json", "List").as_deref(), Some("/std/prelude/List/"));
        assert_eq!(r.type_url("json", "Map").as_deref(), Some("/std/prelude/Map/"));
        assert_eq!(r.type_url("json", "str").as_deref(), Some("/std/prelude/str/"));
    }

    #[test]
    fn summaries_are_first_sentences() {
        assert_eq!(first_sentence("Parses `text`. Fails on errors."), "Parses `text`.");
        assert_eq!(first_sentence("Splits, e.g. on commas. More."), "Splits, e.g. on commas.");
    }
}
