//! The JSON format: a writer and a stateless scanner over `(src, pos)`.
//!
//! Enums use the external tagging of `origin/main`: `"Active"`,
//! `{"Suspended": {"reason": "r"}}`, and `{"Banned": [1, "b"]}`. `None` is
//! `null`. Separators are lenient: the scanner accepts a missing comma.

use super::format::{Event, FormatReader, FormatWriter, StrSpan, syntax_error};
use super::{BYTES_PER_FUEL, Fail, Result, charge, push_int};

/// The JSON [`FormatWriter`].
#[derive(Default)]
pub struct JsonWriter {
    /// A value precedes the next item at the current nesting level.
    comma: bool,
}

impl FormatWriter for JsonWriter {
    #[inline(always)]
    fn write(&mut self, out: &mut String, event: Event<'_>) {
        match event {
            Event::Key(k) => {
                self.separate(out);
                write_str(out, k);
                out.push(':');
                self.comma = false;
            }
            Event::CloseList => {
                out.push(']');
                self.comma = true;
            }
            Event::CloseMap | Event::CloseVariant => {
                out.push('}');
                self.comma = true;
            }
            event => {
                self.separate(out);
                match event {
                    Event::None => out.push_str("null"),
                    Event::Bool(b) => out.push_str(if b { "true" } else { "false" }),
                    Event::Int(i) => push_int(out, i),
                    Event::Float(f) => push_float(out, f),
                    Event::Str(s) | Event::Tag(s) => write_str(out, s),
                    Event::OpenList(_) => out.push('['),
                    Event::OpenMap(_) => out.push('{'),
                    Event::OpenVariant(name) => {
                        out.push('{');
                        write_str(out, name);
                        out.push(':');
                    }
                    Event::Key(_) | Event::CloseList | Event::CloseMap | Event::CloseVariant => unreachable!(),
                }
                self.comma = !matches!(event, Event::OpenList(_) | Event::OpenMap(_) | Event::OpenVariant(_));
            }
        }
    }

    #[inline(always)]
    fn max_len(&self, _event: &Event<'_>, text_len: usize) -> usize {
        // A comma, braces, quotes, a colon, and at most 24 bytes of a
        // number, plus six bytes per escaped text byte.
        32 + 6 * text_len
    }
}

impl JsonWriter {
    #[inline]
    fn separate(&mut self, out: &mut String) {
        if self.comma {
            out.push(',');
        }
    }
}

fn push_float(out: &mut String, f: f64) {
    if f.is_finite() {
        out.push_str(ryu::Buffer::new().format_finite(f));
    } else {
        out.push_str("null");
    }
}

#[inline(always)]
fn needs_escape(b: u8) -> bool {
    b < 0x20 || b == b'"' || b == b'\\'
}

/// Returns whether any byte of `w` may need an escape. False positives are
/// possible after a byte that matches.
#[inline(always)]
fn word_needs_escape(w: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let has_zero = |x: u64| x.wrapping_sub(ONES) & !x & HIGH;
    let control = w.wrapping_sub(ONES * 0x20) & !w & HIGH;
    (control | has_zero(w ^ (ONES * b'"' as u64)) | has_zero(w ^ (ONES * b'\\' as u64))) != 0
}

/// Returns whether any byte of `bytes` needs an escape.
#[inline(always)]
fn has_escape(bytes: &[u8]) -> bool {
    let word = |chunk: &[u8; 8]| word_needs_escape(u64::from_le_bytes(*chunk));
    let (words, rest) = bytes.as_chunks::<8>();
    words.iter().any(word)
        || match bytes.last_chunk::<8>() {
            // The last chunk overlaps the words that the loop already checked.
            Some(last) => !rest.is_empty() && word(last),
            None => rest.iter().any(|&b| needs_escape(b)),
        }
}

/// Appends `s` as a quoted, escaped JSON string.
#[inline(always)]
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    if has_escape(s.as_bytes()) {
        write_escaped(out, s);
    } else {
        out.push_str(s);
    }
    out.push('"');
}

#[cold]
fn write_escaped(out: &mut String, s: &str) {
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        if !needs_escape(b) {
            continue;
        }
        out.push_str(&s[start..i]);
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
}

/// The JSON [`FormatReader`].
pub struct Json;

#[inline]
fn ws(src: &[u8], pos: &mut usize) {
    while let Some(b' ' | b'\n' | b'\r' | b'\t') = src.get(*pos) {
        *pos += 1;
    }
}

#[inline]
fn peek(src: &[u8], pos: &mut usize) -> Option<u8> {
    ws(src, pos);
    src.get(*pos).copied()
}

#[inline]
fn expect(src: &[u8], pos: &mut usize, byte: u8, what: &str) -> Result<()> {
    if peek(src, pos) == Some(byte) {
        *pos += 1;
        Ok(())
    } else {
        syntax_error("json", what, *pos)
    }
}

/// Scans a JSON string at `pos`, which must be the opening quote.
#[inline]
fn scan_str(src: &str, pos: &mut usize, scratch: &mut String) -> Result<StrSpan> {
    let bytes = src.as_bytes();
    if bytes.get(*pos) != Some(&b'"') {
        return syntax_error("json", "a string", *pos);
    }
    let start = *pos + 1;
    let mut i = start;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'"' => {
                *pos = i + 1;
                return Ok(StrSpan::Range(start, i));
            }
            b'\\' => return scan_escaped(src, pos, start, i, scratch),
            _ => i += 1,
        }
    }
    syntax_error("json", "a closing quote", i)
}

#[cold]
fn scan_escaped(src: &str, pos: &mut usize, start: usize, mut i: usize, scratch: &mut String) -> Result<StrSpan> {
    let bytes = src.as_bytes();
    scratch.clear();
    scratch.push_str(&src[start..i]);
    loop {
        let run = i;
        while let Some(&b) = bytes.get(i) {
            if b == b'"' || b == b'\\' {
                break;
            }
            i += 1;
        }
        scratch.push_str(&src[run..i]);
        match bytes.get(i) {
            Some(b'"') => {
                *pos = i + 1;
                return Ok(StrSpan::Scratch);
            }
            Some(b'\\') => {
                let c = match bytes.get(i + 1) {
                    Some(b'"') => '"',
                    Some(b'\\') => '\\',
                    Some(b'/') => '/',
                    Some(b'n') => '\n',
                    Some(b'r') => '\r',
                    Some(b't') => '\t',
                    Some(b'b') => '\u{8}',
                    Some(b'f') => '\u{c}',
                    Some(b'u') => {
                        let (c, len) = scan_unicode_escape(src, i)?;
                        scratch.push(c);
                        i += len;
                        continue;
                    }
                    _ => return syntax_error("json", "an escape", i),
                };
                scratch.push(c);
                i += 2;
            }
            _ => return syntax_error("json", "a closing quote", i),
        }
    }
}

/// Decodes `\uXXXX` (and a following low surrogate) at `i`.
fn scan_unicode_escape(src: &str, i: usize) -> Result<(char, usize)> {
    let hex = |at: usize| src.get(at..at + 4).and_then(|h| u32::from_str_radix(h, 16).ok());
    let Some(high) = hex(i + 2) else { return syntax_error("json", "four hex digits", i) };
    if (0xd800..0xdc00).contains(&high)
        && src.get(i + 6..i + 8) == Some("\\u")
        && let Some(low) = hex(i + 8)
        && (0xdc00..0xe000).contains(&low)
    {
        let c = char::from_u32(0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)).unwrap_or('\u{fffd}');
        return Ok((c, 12));
    }
    Ok((char::from_u32(high).unwrap_or('\u{fffd}'), 6))
}

/// Returns the end of the number at `pos`.
#[inline]
fn number_end(bytes: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while let Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') = bytes.get(i) {
        i += 1;
    }
    i
}

impl FormatReader for Json {
    #[inline]
    fn int(&self, src: &str, pos: &mut usize) -> Result<i64> {
        let bytes = src.as_bytes();
        ws(bytes, pos);
        let mut i = *pos;
        let negative = bytes.get(i) == Some(&b'-');
        i += negative as usize;
        let digits = i;
        let mut value: i64 = 0;
        while let Some(&b @ b'0'..=b'9') = bytes.get(i) {
            let digit = (b - b'0') as i64;
            value = value.checked_mul(10).and_then(|v| if negative { v.checked_sub(digit) } else { v.checked_add(digit) }).ok_or_else(|| {
                Fail::error(format!("json: integer out of range at byte {pos}"))
            })?;
            i += 1;
        }
        if i == digits || matches!(bytes.get(i), Some(b'.' | b'e' | b'E')) {
            return syntax_error("json", "an integer", *pos);
        }
        *pos = i;
        Ok(value)
    }

    #[inline]
    fn float(&self, src: &str, pos: &mut usize) -> Result<f64> {
        let bytes = src.as_bytes();
        ws(bytes, pos);
        let end = number_end(bytes, *pos);
        let value = src[*pos..end].parse().or_else(|_| syntax_error("json", "a number", *pos))?;
        *pos = end;
        Ok(value)
    }

    #[inline]
    fn bool(&self, src: &str, pos: &mut usize) -> Result<bool> {
        let bytes = src.as_bytes();
        ws(bytes, pos);
        if bytes[*pos..].starts_with(b"true") {
            *pos += 4;
            Ok(true)
        } else if bytes[*pos..].starts_with(b"false") {
            *pos += 5;
            Ok(false)
        } else {
            syntax_error("json", "a bool", *pos)
        }
    }

    #[inline]
    fn null(&self, src: &str, pos: &mut usize) -> Result<bool> {
        let bytes = src.as_bytes();
        ws(bytes, pos);
        let null = bytes[*pos..].starts_with(b"null");
        *pos += 4 * null as usize;
        Ok(null)
    }

    #[inline]
    fn str(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<StrSpan> {
        ws(src.as_bytes(), pos);
        scan_str(src, pos, scratch)
    }

    #[inline]
    fn open_list(&self, src: &str, pos: &mut usize) -> Result<()> {
        expect(src.as_bytes(), pos, b'[', "`[`")
    }

    #[inline]
    fn next_item(&self, src: &str, pos: &mut usize) -> Result<bool> {
        let bytes = src.as_bytes();
        match peek(bytes, pos) {
            Some(b']') => {
                *pos += 1;
                Ok(false)
            }
            Some(b',') => {
                *pos += 1;
                Ok(true)
            }
            Some(_) => Ok(true),
            None => syntax_error("json", "`]`", *pos),
        }
    }

    #[inline]
    fn open_map(&self, src: &str, pos: &mut usize) -> Result<()> {
        expect(src.as_bytes(), pos, b'{', "`{`")
    }

    #[inline]
    fn next_key(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<Option<StrSpan>> {
        let bytes = src.as_bytes();
        match peek(bytes, pos) {
            Some(b'}') => {
                *pos += 1;
                return Ok(None);
            }
            Some(b',') => {
                *pos += 1;
                ws(bytes, pos);
            }
            _ => {}
        }
        let key = scan_str(src, pos, scratch)?;
        expect(bytes, pos, b':', "`:`")?;
        Ok(Some(key))
    }

    fn open_variant(&self, src: &str, pos: &mut usize, scratch: &mut String) -> Result<(StrSpan, bool)> {
        let bytes = src.as_bytes();
        match peek(bytes, pos) {
            Some(b'"') => Ok((scan_str(src, pos, scratch)?, false)),
            Some(b'{') => {
                *pos += 1;
                ws(bytes, pos);
                let name = scan_str(src, pos, scratch)?;
                expect(bytes, pos, b':', "`:`")?;
                Ok((name, true))
            }
            _ => syntax_error("json", "a variant", *pos),
        }
    }

    fn close_variant(&self, src: &str, pos: &mut usize) -> Result<()> {
        expect(src.as_bytes(), pos, b'}', "`}`")
    }

    fn skip(&self, src: &str, pos: &mut usize, fuel: &mut u64) -> Result<()> {
        let bytes = src.as_bytes();
        let start = *pos;
        let mut charged = start;
        let mut depth = 0usize;
        let mut scratch = String::new();
        loop {
            if *pos - charged >= 1024 {
                charge(fuel, ((*pos - charged) / BYTES_PER_FUEL) as u64)?;
                charged = *pos;
            }
            match peek(bytes, pos) {
                Some(b'[' | b'{') => {
                    depth += 1;
                    *pos += 1;
                    continue;
                }
                Some(b']' | b'}') => {
                    depth = depth.checked_sub(1).ok_or_else(|| Fail::error(format!("json: unexpected close at byte {pos}")))?;
                    *pos += 1;
                }
                Some(b',' | b':') if depth > 0 => {
                    *pos += 1;
                    continue;
                }
                Some(b'"') => {
                    scan_str(src, pos, &mut scratch)?;
                }
                Some(b't' | b'f' | b'n') => {
                    let word = if bytes[*pos] == b'f' { 5 } else { 4 };
                    if !matches!(&bytes[*pos..(*pos + word).min(bytes.len())], b"true" | b"false" | b"null") {
                        return syntax_error("json", "a value", *pos);
                    }
                    *pos += word;
                }
                Some(b'0'..=b'9' | b'-') => *pos = number_end(bytes, *pos),
                _ => return syntax_error("json", "a value", *pos),
            }
            if depth == 0 {
                break;
            }
        }
        charge(fuel, ((*pos - charged) / BYTES_PER_FUEL) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(events: &[Event]) -> String {
        let mut w = JsonWriter::default();
        let mut out = String::new();
        for e in events {
            w.write(&mut out, *e);
        }
        out
    }

    #[test]
    fn writer_separates_items_and_variants() {
        let out = encode(&[
            Event::OpenMap(2),
            Event::Key("a"),
            Event::OpenList(2),
            Event::Int(1),
            Event::Str("x\"y"),
            Event::CloseList,
            Event::Key("s"),
            Event::OpenVariant("Banned"),
            Event::OpenList(2),
            Event::Float(2.5),
            Event::None,
            Event::CloseList,
            Event::CloseVariant,
            Event::CloseMap,
        ]);
        assert_eq!(out, r#"{"a":[1,"x\"y"],"s":{"Banned":[2.5,null]}}"#);
    }

    #[test]
    fn scanner_reads_values_and_skips() {
        let src = r#" {"a": [1, -2], "b\n": "xé😀", "skip": {"x": [true, null, 1e3, "s"]}, "c": 2.5} "#;
        let j = Json;
        let mut pos = 0;
        let mut scratch = String::new();
        j.open_map(src, &mut pos).unwrap();
        let k = j.next_key(src, &mut pos, &mut scratch).unwrap().unwrap();
        assert_eq!(k.resolve(src, &scratch), "a");
        j.open_list(src, &mut pos).unwrap();
        assert!(j.next_item(src, &mut pos).unwrap());
        assert_eq!(j.int(src, &mut pos).unwrap(), 1);
        assert!(j.next_item(src, &mut pos).unwrap());
        assert_eq!(j.int(src, &mut pos).unwrap(), -2);
        assert!(!j.next_item(src, &mut pos).unwrap());
        let k = j.next_key(src, &mut pos, &mut scratch).unwrap().unwrap();
        assert_eq!(k, StrSpan::Scratch);
        assert_eq!(scratch, "b\n");
        let v = j.str(src, &mut pos, &mut scratch).unwrap();
        assert_eq!(v.resolve(src, &scratch), "xé😀");
        j.next_key(src, &mut pos, &mut scratch).unwrap().unwrap();
        let mut fuel = 100;
        j.skip(src, &mut pos, &mut fuel).unwrap();
        let k = j.next_key(src, &mut pos, &mut scratch).unwrap().unwrap();
        assert_eq!(k.resolve(src, &scratch), "c");
        assert_eq!(j.float(src, &mut pos).unwrap(), 2.5);
        assert_eq!(j.next_key(src, &mut pos, &mut scratch).unwrap(), None);
    }

    #[test]
    fn skip_stops_at_the_fuel_limit() {
        let src = format!("[{}0]", "1,".repeat(100_000));
        let mut fuel = 1000;
        assert_eq!(Json.skip(&src, &mut 0, &mut fuel), Err(Fail::OutOfFuel));
    }
}
