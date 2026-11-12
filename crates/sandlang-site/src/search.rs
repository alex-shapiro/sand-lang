//! Client-side search index serializer (`/search-index.json`).

pub struct Entry {
    pub name: String,
    pub kind: String,
    pub url: String,
    pub summary: String,
}

impl Entry {
    pub fn new(name: &str, kind: &str, url: &str, summary: &str) -> Entry {
        Entry { name: name.into(), kind: kind.into(), url: url.into(), summary: summary.into() }
    }
}

/// Serializes `entries` as a JSON array of `[name, kind, url, summary]` tuples.
pub fn index_json(entries: &[Entry]) -> String {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| format!("[{},{},{},{}]", json_string(&e.name), json_string(&e.kind), json_string(&e.url), json_string(&e.summary)))
        .collect();
    format!("[\n{}\n]\n", rows.join(",\n"))
}

pub fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_json() {
        let json = index_json(&[Entry::new("a\"b", "fn", "/x/", "Line\nbreak \\")]);
        assert_eq!(json, "[\n[\"a\\\"b\",\"fn\",\"/x/\",\"Line\\nbreak \\\\\"]\n]\n");
    }
}
