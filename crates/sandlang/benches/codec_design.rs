//! Design experiment E2 (see DESIGN_NOTES.md): the cost of a Sand-driven
//! decoder and encoder that call a native streaming reader or writer through
//! dictionary-dispatched trait methods, compared with the native walks.
//!
//! The Sand code in `e2_stream.sand` is what a compiler would synthesize for
//! the benchmark types. `cargo bench --bench codec_design`.

use sandlang::engine::{Config, Engine, HostModules, Limits, MemorySink};
use sandlang::native::convert::HostError;
use sandlang::native::ty::VmVal;
use sandlang::native::{NativeModule, NativeObject};
use sandlang::vm::{Fail, Vm};

/// A streaming JSON reader.
#[derive(sandlang::SandObject)]
#[sand(module = "codec")]
pub struct Reader {
    text: Vec<u8>,
    pos: usize,
    /// Path segments: a key, or an index.
    path: Vec<Seg>,
}

enum Seg {
    /// A key's byte range in the text.
    Key(usize, usize),
    Index(i64),
}

impl NativeObject for Reader {}

/// A table of field or variant names.
#[derive(sandlang::SandObject)]
#[sand(module = "codec")]
pub struct Names {
    names: std::sync::Arc<Vec<String>>,
}

impl NativeObject for Names {}

/// A JSON text writer.
#[derive(sandlang::SandObject)]
#[sand(module = "codec")]
pub struct Writer {
    out: String,
    /// One flag per open container: `true` before its first member.
    first: Vec<bool>,
}

impl NativeObject for Writer {}

fn err<T>(r: &Reader, what: &str) -> Result<T, HostError> {
    let mut p = String::from("$");
    for s in &r.path {
        match s {
            Seg::Key(a, b) => {
                p.push('.');
                p.push_str(std::str::from_utf8(&r.text[*a..*b]).unwrap_or("?"));
            }
            Seg::Index(i) => p.push_str(&format!("[{i}]")),
        }
    }
    Err(HostError::new("parse", format!("{p}: expected {what}")))
}

impl Reader {
    fn ws(&mut self) {
        while self.pos < self.text.len() && matches!(self.text[self.pos], b' ' | b'\n' | b'\r' | b'\t') {
            self.pos += 1;
        }
    }
    fn peek(&mut self) -> u8 {
        self.ws();
        self.text.get(self.pos).copied().unwrap_or(0)
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn raw_range(&mut self) -> Option<(usize, usize)> {
        if !self.eat(b'"') {
            return None;
        }
        let start = self.pos;
        while self.pos < self.text.len() && self.text[self.pos] != b'"' {
            self.pos += 1;
        }
        let end = self.pos;
        self.pos += 1;
        Some((start, end))
    }
    fn raw_str(&mut self) -> Option<&str> {
        let (a, b) = self.raw_range()?;
        std::str::from_utf8(&self.text[a..b]).ok()
    }
    fn number(&mut self) -> Option<(usize, usize)> {
        self.ws();
        let start = self.pos;
        while self.pos < self.text.len() && matches!(self.text[self.pos], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
            self.pos += 1;
        }
        (self.pos > start).then_some((start, self.pos))
    }
    fn skip(&mut self) -> Result<(), HostError> {
        match self.peek() {
            b'"' => {
                self.raw_str();
            }
            b'{' => {
                self.pos += 1;
                if !self.eat(b'}') {
                    loop {
                        self.raw_str();
                        self.eat(b':');
                        self.skip()?;
                        if !self.eat(b',') {
                            break;
                        }
                    }
                    self.eat(b'}');
                }
            }
            b'[' => {
                self.pos += 1;
                if !self.eat(b']') {
                    loop {
                        self.skip()?;
                        if !self.eat(b',') {
                            break;
                        }
                    }
                    self.eat(b']');
                }
            }
            b't' => self.pos += 4,
            b'f' => self.pos += 5,
            b'n' => self.pos += 4,
            _ => {
                self.number();
            }
        }
        Ok(())
    }
}

fn module() -> NativeModule {
    let mut m = NativeModule::new("codec");
    m.bind::<Reader>();
    m.bind::<Names>();
    m.bind::<Writer>();
    m.function("Reader.init", |vm: &mut Vm, text: VmVal<String>| -> Result<VmVal<Reader>, Fail> {
        let text = text.str(&vm.heap).as_bytes().to_vec();
        vm.new_object(Reader { text, pos: 0, path: Vec::new() })
    });
    m.function("Names.init", |vm: &mut Vm, names: Vec<String>| -> Result<VmVal<Names>, Fail> {
        vm.new_object(Names { names: std::sync::Arc::new(names) })
    });
    m.function("Reader.int", |vm: &mut Vm, r: VmVal<Reader>| -> Result<i64, HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        match r.number() {
            Some((a, b)) => std::str::from_utf8(&r.text[a..b]).ok().and_then(|s| s.parse().ok()).map_or_else(|| err(r, "int"), Ok),
            None => err(r, "int"),
        }
    });
    m.function("Reader.int_raw", |vm: &mut Vm, r: VmVal<Reader>| -> Result<i64, Fail> {
        let r = r.get_mut(&mut vm.heap)?;
        Ok(r.number().and_then(|(a, b)| std::str::from_utf8(&r.text[a..b]).ok().and_then(|s| s.parse().ok())).unwrap_or(0))
    });
    m.function("Reader.next_item_raw", |vm: &mut Vm, r: VmVal<Reader>| -> Result<bool, Fail> {
        let r = r.get_mut(&mut vm.heap)?;
        let Some(Seg::Index(i)) = r.path.last_mut() else { return Ok(false) };
        let first = *i < 0;
        *i += 1;
        if r.eat(b']') {
            r.path.pop();
            return Ok(false);
        }
        Ok(first || r.eat(b','))
    });
    m.function("Reader.float", |vm: &mut Vm, r: VmVal<Reader>| -> Result<f64, HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        match r.number() {
            Some((a, b)) => std::str::from_utf8(&r.text[a..b]).ok().and_then(|s| s.parse().ok()).map_or_else(|| err(r, "float"), Ok),
            None => err(r, "float"),
        }
    });
    m.function("Reader.bool", |vm: &mut Vm, r: VmVal<Reader>| -> Result<bool, HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        match r.peek() {
            b't' => {
                r.pos += 4;
                Ok(true)
            }
            b'f' => {
                r.pos += 5;
                Ok(false)
            }
            _ => err(r, "bool"),
        }
    });
    m.function("Reader.str", |vm: &mut Vm, r: VmVal<Reader>| -> Result<VmVal<String>, HostError> {
        let reader = r.get_mut(&mut vm.heap)?;
        let Some(s) = reader.raw_str().map(compact_str::CompactString::from) else { return err(reader, "str") };
        Ok(VmVal::from_raw(vm.new_str(s)))
    });
    m.function("Reader.null", |vm: &mut Vm, r: VmVal<Reader>| -> Result<bool, Fail> {
        let r = r.get_mut(&mut vm.heap)?;
        if r.peek() == b'n' {
            r.pos += 4;
            Ok(true)
        } else {
            Ok(false)
        }
    });
    m.function("Reader.begin_list", |vm: &mut Vm, r: VmVal<Reader>| -> Result<(), HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        if !r.eat(b'[') {
            return err(r, "list");
        }
        r.path.push(Seg::Index(-1));
        Ok(())
    });
    m.function("Reader.next_item", |vm: &mut Vm, r: VmVal<Reader>| -> Result<bool, HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        let Some(Seg::Index(i)) = r.path.last_mut() else { return err(r, "list state") };
        let first = *i < 0;
        *i += 1;
        if r.eat(b']') {
            r.path.pop();
            return Ok(false);
        }
        if !first && !r.eat(b',') {
            return err(r, "`,`");
        }
        Ok(true)
    });
    // Returns the index of the next known field, or -1 after the last field.
    m.function("Reader.begin_struct", |vm: &mut Vm, r: VmVal<Reader>| -> Result<(), HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        if !r.eat(b'{') {
            return err(r, "object");
        }
        r.path.push(Seg::Index(-1));
        Ok(())
    });
    m.function("Reader.field", |vm: &mut Vm, r: VmVal<Reader>, names: VmVal<Names>| -> Result<i64, HostError> {
        let heap = &mut vm.heap;
        let names = names.get(heap)?.names.clone();
        let r = r.get_mut(heap)?;
        loop {
            if r.eat(b'}') {
                r.path.pop();
                return Ok(-1);
            }
            if matches!(r.path.last(), Some(Seg::Key(..))) && !r.eat(b',') {
                return err(r, "`,`");
            }
            let Some((a, b)) = r.raw_range() else { return err(r, "key") };
            if !r.eat(b':') {
                return err(r, "`:`");
            }
            let key = &r.text[a..b];
            let found = names.iter().position(|n| n.as_bytes() == key);
            if let Some(top) = r.path.last_mut() {
                *top = Seg::Key(a, b);
            }
            match found {
                Some(i) => return Ok(i as i64),
                None => r.skip()?,
            }
        }
    });
    // Reads a unit variant name or the key of a `{"Name": payload}` object.
    m.function("Reader.variant", |vm: &mut Vm, r: VmVal<Reader>, names: VmVal<Names>| -> Result<i64, HostError> {
        let heap = &mut vm.heap;
        let names = names.get(heap)?.names.clone();
        let r = r.get_mut(heap)?;
        let open = r.eat(b'{');
        let Some((a, b)) = r.raw_range() else { return err(r, "variant") };
        if open && !r.eat(b':') {
            return err(r, "`:`");
        }
        match names.iter().position(|n| n.as_bytes() == &r.text[a..b]) {
            Some(i) => {
                r.path.push(Seg::Key(a, b));
                Ok(i as i64)
            }
            None => err(r, "variant name"),
        }
    });
    m.function("Reader.end_variant", |vm: &mut Vm, r: VmVal<Reader>| -> Result<(), HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        r.path.pop();
        if r.peek() == b'}' {
            r.pos += 1;
        }
        Ok(())
    });
    m.function("Reader.begin_map", |vm: &mut Vm, r: VmVal<Reader>| -> Result<(), HostError> {
        let r = r.get_mut(&mut vm.heap)?;
        if !r.eat(b'{') {
            return err(r, "map");
        }
        r.path.push(Seg::Index(-1));
        Ok(())
    });
    m.function("Reader.next_key", |vm: &mut Vm, r: VmVal<Reader>| -> Result<Option<VmVal<String>>, HostError> {
        let reader = r.get_mut(&mut vm.heap)?;
        if reader.eat(b'}') {
            reader.path.pop();
            return Ok(None);
        }
        if matches!(reader.path.last(), Some(Seg::Key(..))) && !reader.eat(b',') {
            return err(reader, "`,`");
        }
        let Some((a, b)) = reader.raw_range() else { return err(reader, "key") };
        if !reader.eat(b':') {
            return err(reader, "`:`");
        }
        if let Some(top) = reader.path.last_mut() {
            *top = Seg::Key(a, b);
        }
        let key = compact_str::CompactString::from(std::str::from_utf8(&reader.text[a..b]).unwrap_or(""));
        Ok(Some(VmVal::from_raw(vm.new_str(key))))
    });

    m.function("Writer.init", |vm: &mut Vm| -> Result<VmVal<Writer>, Fail> {
        vm.new_object(Writer { out: String::new(), first: Vec::new() })
    });
    fn sep(w: &mut Writer) {
        if let Some(f) = w.first.last_mut() {
            if !*f {
                w.out.push(',');
            }
            *f = false;
        }
    }
    m.function("Writer.int", |vm: &mut Vm, w: VmVal<Writer>, x: i64| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push_str(itoa::Buffer::new().format(x));
        Ok(())
    });
    m.function("Writer.float", |vm: &mut Vm, w: VmVal<Writer>, x: f64| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        use std::fmt::Write;
        let _ = write!(w.out, "{x:?}");
        Ok(())
    });
    m.function("Writer.bool", |vm: &mut Vm, w: VmVal<Writer>, x: bool| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push_str(if x { "true" } else { "false" });
        Ok(())
    });
    m.function("Writer.null", |vm: &mut Vm, w: VmVal<Writer>| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push_str("null");
        Ok(())
    });
    m.function("Writer.str", |vm: &mut Vm, w: VmVal<Writer>, s: VmVal<String>| -> Result<(), Fail> {
        let s = s.str(&vm.heap).to_string();
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push('"');
        w.out.push_str(&s);
        w.out.push('"');
        Ok(())
    });
    m.function("Writer.begin_list", |vm: &mut Vm, w: VmVal<Writer>| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push('[');
        w.first.push(true);
        Ok(())
    });
    m.function("Writer.end_list", |vm: &mut Vm, w: VmVal<Writer>| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        w.out.push(']');
        w.first.pop();
        Ok(())
    });
    m.function("Writer.begin_struct", |vm: &mut Vm, w: VmVal<Writer>| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push('{');
        w.first.push(true);
        Ok(())
    });
    m.function("Writer.field", |vm: &mut Vm, w: VmVal<Writer>, names: VmVal<Names>, i: i64| -> Result<(), Fail> {
        let names = names.get(&vm.heap)?.names.clone();
        let name = &names[i as usize];
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push('"');
        w.out.push_str(name);
        w.out.push_str("\":");
        if let Some(f) = w.first.last_mut() {
            *f = true;
        }
        Ok(())
    });
    m.function("Writer.key", |vm: &mut Vm, w: VmVal<Writer>, s: VmVal<String>| -> Result<(), Fail> {
        let s = s.str(&vm.heap).to_string();
        let w = w.get_mut(&mut vm.heap)?;
        sep(w);
        w.out.push('"');
        w.out.push_str(&s);
        w.out.push_str("\":");
        if let Some(f) = w.first.last_mut() {
            *f = true;
        }
        Ok(())
    });
    m.function("Writer.end_struct", |vm: &mut Vm, w: VmVal<Writer>| -> Result<(), Fail> {
        let w = w.get_mut(&mut vm.heap)?;
        w.out.push('}');
        w.first.pop();
        Ok(())
    });
    m.function("Writer.text", |vm: &mut Vm, w: VmVal<Writer>| -> Result<VmVal<String>, Fail> {
        let s = w.get(&vm.heap)?.out.clone();
        Ok(VmVal::from_raw(vm.new_str(s)))
    });
    m
}

fn main() {
    let quick = !std::env::args().any(|a| a == "--bench");
    let n = std::env::var("BENCH_USERS").ok().and_then(|n| n.parse().ok()).unwrap_or(if quick { 50 } else { 5000usize });
    let file = std::env::var("BENCH_FILE").unwrap_or_else(|_| "e2_stream.sand".into());
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../experiments/codec-design/");
    let src = std::fs::read_to_string(format!("{dir}{file}")).unwrap().replace("{n}", &n.to_string());
    let out = MemorySink::new();
    let config = Config::new().limits(Limits::default()).host_modules(HostModules::new([module()]).unwrap()).stdout(out.clone());
    let mut engine = Engine::new(config);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let r = rt.block_on(engine.run_source("e2.sand", &src));
    print!("{}", out.contents());
    r.unwrap();
}
