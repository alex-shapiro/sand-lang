//! The S-expression format of B8, built only on the public codec interface.
//!
//! Lists are `(1 2 3)`, maps and structs are `(:"key" value ...)`, payload-less
//! variants are bare symbols, and variants with a payload are `(Name payload)`.
//! `nil` is `None`, and `#t` and `#f` are booleans.

use super::format::{Event, FormatReader, FormatWriter, StrSpan, syntax_error};
use super::{BYTES_PER_FUEL, Result, charge, push_int};

/// The S-expression [`FormatWriter`].
#[derive(Default)]
pub struct SExprWriter {
    /// A value precedes the next item at the current nesting level.
    space: bool,
}

impl FormatWriter for SExprWriter {
    fn write(&mut self, out: &mut String, event: Event<'_>) {
        if !matches!(event, Event::CloseList | Event::CloseMap | Event::CloseVariant) && self.space {
            out.push(' ');
        }
        self.space = true;
        match event {
            Event::None => out.push_str("nil"),
            Event::Bool(b) => out.push_str(if b { "#t" } else { "#f" }),
            Event::Int(i) => push_int(out, i),
            Event::Float(f) => {
                let mut buffer = ryu::Buffer::new();
                out.push_str(if f.is_finite() { buffer.format_finite(f) } else { "nil" });
            }
            Event::Str(s) => write_quoted(out, s),
            Event::Key(k) => {
                out.push(':');
                write_quoted(out, k);
            }
            Event::Tag(name) => out.push_str(name),
            Event::OpenList(_) | Event::OpenMap(_) => {
                out.push('(');
                self.space = false;
            }
            Event::OpenVariant(name) => {
                out.push('(');
                out.push_str(name);
            }
            Event::CloseList | Event::CloseMap | Event::CloseVariant => out.push(')'),
        }
    }

    fn max_len(&self, _event: &Event<'_>, text_len: usize) -> usize {
        32 + 2 * text_len
    }
}

fn write_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The S-expression [`FormatReader`].
pub struct SExpr;

fn ws(src: &[u8], pos: &mut usize) -> Option<u8> {
    while let Some(b' ' | b'\n' | b'\t' | b'\r') = src.get(*pos) {
        *pos += 1;
    }
    src.get(*pos).copied()
}

fn atom_end(src: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while src.get(i).is_some_and(|b| !matches!(b, b' ' | b'\n' | b'\t' | b'\r' | b'(' | b')' | b'"')) {
        i += 1;
    }
    i
}

fn atom<'a>(src: &'a str, pos: &mut usize) -> &'a str {
    ws(src.as_bytes(), pos);
    let end = atom_end(src.as_bytes(), *pos);
    let text = &src[*pos..end];
    *pos = end;
    text
}

fn quoted(src: &str, pos: &mut usize, scratch: &mut String) -> Result<StrSpan> {
    let bytes = src.as_bytes();
    if ws(bytes, pos) != Some(b'"') {
        return syntax_error("sexpr", "a string", *pos);
    }
    let start = *pos + 1;
    let mut i = start;
    let mut escaped = false;
    loop {
        match bytes.get(i) {
            Some(b'"') => break,
            Some(b'\\') => {
                escaped = true;
                i += 2;
            }
            Some(_) => i += 1,
            None => return syntax_error("sexpr", "a closing quote", i),
        }
    }
    *pos = i + 1;
    if !escaped {
        return Ok(StrSpan::Range(start, i));
    }
    scratch.clear();
    let mut chars = src[start..i].chars();
    while let Some(c) = chars.next() {
        scratch.push(match (c, c == '\\') {
            (_, true) => match chars.next() {
                Some('n') => '\n',
                Some(other) => other,
                None => break,
            },
            (c, false) => c,
        });
    }
    Ok(StrSpan::Scratch)
}

fn expect(src: &str, pos: &mut usize, byte: u8, what: &str) -> Result<()> {
    if ws(src.as_bytes(), pos) == Some(byte) {
        *pos += 1;
        Ok(())
    } else {
        syntax_error("sexpr", what, *pos)
    }
}

impl FormatReader for SExpr {
    fn int(&self, src: &str, pos: &mut usize) -> Result<i64> {
        let at = *pos;
        atom(src, pos).parse().or_else(|_| syntax_error("sexpr", "an integer", at))
    }

    fn float(&self, src: &str, pos: &mut usize) -> Result<f64> {
        let at = *pos;
        atom(src, pos).parse().or_else(|_| syntax_error("sexpr", "a number", at))
    }

    fn bool(&self, src: &str, pos: &mut usize) -> Result<bool> {
        let at = *pos;
        match atom(src, pos) {
            "#t" => Ok(true),
            "#f" => Ok(false),
            _ => syntax_error("sexpr", "a bool", at),
        }
    }

    fn null(&self, src: &str, pos: &mut usize) -> Result<bool> {
        let bytes = src.as_bytes();
        ws(bytes, pos);
        let end = atom_end(bytes, *pos);
        let null = &src[*pos..end] == "nil";
        if null {
            *pos = end;
        }
        Ok(null)
    }

    fn str(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<StrSpan> {
        quoted(src, pos, scratch)
    }

    fn open_list(&self, src: &str, pos: &mut usize) -> Result<()> {
        expect(src, pos, b'(', "`(`")
    }

    fn next_item(&self, src: &str, pos: &mut usize) -> Result<bool> {
        match ws(src.as_bytes(), pos) {
            Some(b')') => {
                *pos += 1;
                Ok(false)
            }
            Some(_) => Ok(true),
            None => syntax_error("sexpr", "`)`", *pos),
        }
    }

    fn open_map(&self, src: &str, pos: &mut usize) -> Result<()> {
        expect(src, pos, b'(', "`(`")
    }

    fn next_key(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<Option<StrSpan>> {
        match ws(src.as_bytes(), pos) {
            Some(b')') => {
                *pos += 1;
                Ok(None)
            }
            Some(b':') => {
                *pos += 1;
                quoted(src, pos, scratch).map(Some)
            }
            _ => syntax_error("sexpr", "a key", *pos),
        }
    }

    fn open_variant(&self, src: &str, pos: &mut usize, _scratch: &mut String) -> Result<(StrSpan, bool)> {
        let bytes = src.as_bytes();
        let payload = ws(bytes, pos) == Some(b'(');
        *pos += payload as usize;
        ws(bytes, pos);
        let start = *pos;
        *pos = atom_end(bytes, start);
        if *pos == start {
            return syntax_error("sexpr", "a variant name", start);
        }
        Ok((StrSpan::Range(start, *pos), payload))
    }

    fn close_variant(&self, src: &str, pos: &mut usize) -> Result<()> {
        expect(src, pos, b')', "`)`")
    }

    fn skip(&self, src: &str, pos: &mut usize, fuel: &mut u64) -> Result<()> {
        let bytes = src.as_bytes();
        let start = *pos;
        let mut depth = 0usize;
        let mut scratch = String::new();
        loop {
            match ws(bytes, pos) {
                Some(b'(') => {
                    depth += 1;
                    *pos += 1;
                    continue;
                }
                Some(b')') if depth > 0 => {
                    depth -= 1;
                    *pos += 1;
                }
                Some(b':') if depth > 0 => {
                    *pos += 1;
                    continue;
                }
                Some(b'"') => {
                    quoted(src, pos, &mut scratch)?;
                }
                Some(_) => {
                    let end = atom_end(bytes, *pos);
                    if end == *pos {
                        return syntax_error("sexpr", "a value", *pos);
                    }
                    *pos = end;
                }
                None => return syntax_error("sexpr", "a value", *pos),
            }
            if depth == 0 {
                break;
            }
        }
        charge(fuel, ((*pos - start) / BYTES_PER_FUEL) as u64)
    }
}
