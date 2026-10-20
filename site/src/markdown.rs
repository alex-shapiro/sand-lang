//! Markdown to HTML: headings get anchors, code blocks are checked
//! (`examples`) and highlighted, and inline code may become a link.

use crate::examples::{self, Mode, Shown};
use crate::highlight::{self, escape};
use crate::search::json_string;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::collections::BTreeMap;

/// A heading of a rendered document.
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub id: String,
}

pub struct Rendered {
    pub html: String,
    pub headings: Vec<Heading>,
}

/// What rendering checks, and where it reports problems.
pub struct Checks {
    pub runtime: tokio::runtime::Runtime,
    /// The modules (`file=NAME` blocks) of the document being rendered.
    pub files: Vec<(String, String)>,
    pub errors: Vec<String>,
    /// The number of examples run or checked.
    pub examples: usize,
}

impl Checks {
    pub fn new() -> Checks {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a Tokio runtime");
        Checks { runtime, files: Vec::new(), errors: Vec::new(), examples: 0 }
    }
}

/// How to render a document.
pub struct Style<'a> {
    /// The document's name in error messages.
    pub source: &'a str,
    /// Whether to check `sand` blocks; otherwise they are only highlighted.
    pub verify: bool,
    /// Turns the text of inline code into a link target, if it names something.
    pub link_code: &'a dyn Fn(&str) -> Option<String>,
    /// Added to the level of every heading (doc comments sit under an item's heading).
    pub heading_offset: u8,
}

impl Style<'_> {
    pub fn page(source: &str, verify: bool) -> Style<'_> {
        Style { source, verify, link_code: &|_| None, heading_offset: 0 }
    }
}

/// The `+++` front matter of a document (`key = "value"` lines) and the rest.
pub fn front_matter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut fields = BTreeMap::new();
    let Some(rest) = text.strip_prefix("+++\n") else { return (fields, text) };
    let Some(end) = rest.find("\n+++\n") else { return (fields, text) };
    for line in rest[..end].lines() {
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').replace("\\\"", "\"");
            fields.insert(key.trim().to_string(), value);
        }
    }
    (fields, &rest[end + 5..])
}

pub fn render(markdown: &str, style: &Style, checks: &mut Checks) -> Rendered {
    checks.files.clear();
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_SMART_PUNCTUATION;
    let mut events: Vec<Event> = Vec::new();
    let mut headings: Vec<Heading> = Vec::new();
    let mut heading: Option<(u8, Vec<Event>)> = None;
    let mut code: Option<(String, String)> = None;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((info, String::new()));
            }
            Event::Text(text) if code.is_some() => code.as_mut().unwrap().1.push_str(&text),
            Event::End(TagEnd::CodeBlock) => {
                let (info, text) = code.take().unwrap();
                events.push(Event::Html(code_block(&info, &text, style, checks).into()));
            }
            Event::Start(Tag::Heading { level, .. }) => heading = Some((level as u8, Vec::new())),
            Event::End(TagEnd::Heading(_)) => {
                let (level, inner) = heading.take().unwrap();
                let level = (level + style.heading_offset).min(6);
                let text: String = inner
                    .iter()
                    .filter_map(|e| match e {
                        Event::Text(t) | Event::Code(t) => Some(t.as_ref()),
                        _ => None,
                    })
                    .collect();
                let id = unique_id(&slug(&text), &headings);
                let mut html = String::new();
                pulldown_cmark::html::push_html(&mut html, inner.into_iter());
                events.push(Event::Html(
                    format!(
                        r##"<h{level} id="{id}">{html}<a class="anchor" href="#{id}" aria-label="Link to this section">#</a></h{level}>"##
                    )
                    .into(),
                ));
                headings.push(Heading { level, text, id });
            }
            Event::Code(text) => {
                let event = match (style.link_code)(&text) {
                    Some(url) => Event::Html(format!(r#"<a href="{url}"><code>{}</code></a>"#, escape(&text)).into()),
                    None => Event::Code(text),
                };
                match &mut heading {
                    Some((_, inner)) => inner.push(event),
                    None => events.push(event),
                }
            }
            e => match &mut heading {
                Some((_, inner)) => inner.push(e),
                None => events.push(e),
            },
        }
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());
    Rendered { html, headings }
}

fn code_block(info: &str, text: &str, style: &Style, checks: &mut Checks) -> String {
    let words: Vec<&str> = info.split([' ', ',']).filter(|w| !w.is_empty()).collect();
    let lang = words.first().copied().unwrap_or("");
    let mut fail = |message: String| {
        let first = text.lines().next().unwrap_or("");
        checks.errors.push(format!("{}: code block starting `{first}`: {message}", style.source));
    };
    match lang {
        "sand" => {
            let file = words.iter().find_map(|w| w.strip_prefix("file=")).map(str::to_string);
            let rest: Vec<&str> = words[1..].iter().copied().filter(|w| !w.starts_with("file=")).collect();
            let mode = Mode::parse(&rest).unwrap_or_else(|e| {
                fail(e);
                Mode::Ignore
            });
            let mode = match (style.verify, &file) {
                (false, _) => Mode::Ignore,
                (true, Some(_)) => Mode::NoRun,
                (true, None) => mode,
            };
            if mode != Mode::Ignore {
                checks.examples += 1;
            }
            let html = match examples::check_sand(&checks.runtime, text, mode, &checks.files) {
                Ok(example) => {
                    let play = if mode == Mode::Ignore || file.is_some() { "" } else { &example.program };
                    let files = if play.is_empty() { &[][..] } else { &checks.files[..] };
                    sand_example(&example.shown, play, files, file.as_deref(), example.result.as_ref())
                }
                Err(message) => {
                    fail(message);
                    sand_example(text, "", &[], file.as_deref(), None)
                }
            };
            if let Some(name) = file {
                checks.files.push((name, text.to_string()));
            }
            html
        }
        "policy" => {
            if style.verify {
                checks.examples += 1;
                if let Err(e) = examples::check_policy(text) {
                    fail(e);
                }
            }
            figure("policy", &highlight::policy(text.trim_end()), None, "", &[])
        }
        "" if style.verify => {
            fail("code blocks need a language (sand, policy, rust, js, sh or text)".into());
            figure("text", &escape(text.trim_end()), None, "", &[])
        }
        "rust" => {
            let shown: String = text.lines().filter(|l| !l.starts_with("# ")).map(|l| format!("{l}\n")).collect();
            figure(lang, &highlight::clike(shown.trim_end(), &highlight::RUST), None, "", &[])
        }
        "js" => figure(lang, &highlight::clike(text.trim_end(), &highlight::JS), None, "", &[]),
        _ => figure(lang, &escape(text.trim_end()), None, "", &[]),
    }
}

/// `play` is the program the playground opens (none if empty), with `files`.
fn sand_example(shown: &str, play: &str, files: &[(String, String)], caption: Option<&str>, result: Option<&Shown>) -> String {
    let output = match result {
        None => None,
        Some(Shown::Output(o)) => Some(("output", "Output", escape(o.trim_end()))),
        Some(Shown::CompileError(e)) => Some(("error", "Compile error", escape(e.trim_end()))),
        Some(Shown::RuntimeError { output, error }) => {
            Some(("error", "Runtime error", format!("{}{}", escape(output), escape(error.trim_end()))))
        }
    };
    let mut figure = figure("sand", &highlight::sand(shown.trim_end()), output, play, files);
    if let Some(name) = caption {
        figure =
            figure.replacen("<div class=\"code-box\">", &format!("<figcaption>{}</figcaption><div class=\"code-box\">", escape(name)), 1);
    }
    figure
}

/// A code block with its actions and, below it, what it printed.
fn figure(lang: &str, code: &str, output: Option<(&str, &str, String)>, play: &str, files: &[(String, String)]) -> String {
    let play = if play.is_empty() {
        String::new()
    } else {
        let files: Vec<String> = files.iter().map(|(n, t)| format!("{}:{}", json_string(n), json_string(t))).collect();
        format!(
            r#"<button type="button" class="play" data-program="{}" data-files="{}">Open in playground</button>"#,
            escape(play),
            escape(&format!("{{{}}}", files.join(",")))
        )
    };
    let output = match output {
        Some((class, label, text)) => format!(r#"<div class="{class}"><span class="label">{label}</span><pre>{text}</pre></div>"#),
        None => String::new(),
    };
    format!(
        r#"<figure class="code" data-lang="{lang}"><div class="code-box"><pre><code>{code}</code></pre><div class="code-actions"><button type="button" class="copy">Copy</button>{play}</div></div>{output}</figure>"#
    )
}

/// The anchor GitHub gives a heading, without hyphens at its ends:
/// `13.4 The http module` -> `134-the-http-module`, `` `?` at the top`` -> `at-the-top`.
pub fn slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    slug.trim_matches('-').to_string()
}

fn unique_id(base: &str, headings: &[Heading]) -> String {
    let base = if base.is_empty() { "section" } else { base };
    let mut id = base.to_string();
    let mut n = 2;
    while headings.iter().any(|h| h.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_read() {
        let (fields, rest) = front_matter("+++\ntitle = \"Basics\"\n+++\n\nText\n");
        assert_eq!(fields["title"], "Basics");
        assert_eq!(rest, "\nText\n");
    }

    #[test]
    fn headings_get_unique_anchors() {
        let mut checks = Checks::new();
        let r = render("## A `b`\n\n## A b\n", &Style::page("t", true), &mut checks);
        assert_eq!(r.headings.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), ["a-b", "a-b-2"]);
        assert!(r.html.contains(r##"<h2 id="a-b">A <code>b</code><a class="anchor" href="#a-b""##), "{}", r.html);
    }

    #[test]
    fn examples_show_output_and_failures_are_reported() {
        let mut checks = Checks::new();
        let r = render("```sand\nprint(1 + 1)\n```\n\n```sand\nprint(1 + 1.0)\n```\n", &Style::page("t", true), &mut checks);
        assert!(r.html.contains(r#"<span class="label">Output</span><pre>2</pre>"#), "{}", r.html);
        assert_eq!(checks.errors.len(), 1, "{:?}", checks.errors);
        assert!(checks.errors[0].starts_with("t: code block starting `print(1 + 1.0)`"), "{:?}", checks.errors);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("11. Concurrency and asynchronous I/O"), "11-concurrency-and-asynchronous-io");
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("? at the top level"), "at-the-top-level");
    }
}
