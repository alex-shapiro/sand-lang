//! End-to-end tests of `sand repl`'s line editor on a pseudo-terminal.
//!
//! The REPL edits inputs with a line editor when standard input and output
//! are a terminal. These tests run it on a Unix pty through `portable-pty`,
//! play the terminal (the editor asks for the cursor position before each
//! input, `ESC [ 6 n`, and waits for the answer), type keys and check the
//! values printed and the history file. If no pseudo-terminal is available
//! they print why and pass.
#![cfg(unix)]

mod common;

use common::TempDir;
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const CURSOR_QUERY: &[u8] = b"\x1b[6n";

const LEFT: &str = "\x1b[D";
const UP: &str = "\x1b[A";
const HOME: &str = "\x1b[H";
const ALT_B: &str = "\x1bb";
const CTRL_D: &str = "\x04";

/// What the terminal received so far, and how many cursor position queries
/// were answered.
#[derive(Default)]
struct Screen {
    raw: Vec<u8>,
    queries: usize,
}

/// `sand repl` on a pseudo-terminal.
struct Repl {
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screen: Arc<Mutex<Screen>>,
}

impl Repl {
    /// Starts `sand repl` in `dir` with `history` as its history file; `None`
    /// if no pseudo-terminal is available.
    fn start(dir: &Path, history: &Path) -> Option<Repl> {
        Repl::start_with(dir, history, &[])
    }

    /// Like [`Repl::start`], with options for `sand repl`.
    fn start_with(dir: &Path, history: &Path, args: &[&str]) -> Option<Repl> {
        let pair = match native_pty_system().openpty(PtySize { rows: 50, cols: 200, pixel_width: 0, pixel_height: 0 }) {
            Ok(p) => p,
            Err(e) => {
                println!("skipped: no pseudo-terminal available: {e}");
                return None;
            }
        };
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_sand"));
        cmd.arg("repl");
        cmd.args(args);
        cmd.cwd(dir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("SAND_HISTORY", history);
        let child = pair.slave.spawn_command(cmd).expect("cannot start sand on the pseudo-terminal");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let screen = Arc::new(Mutex::new(Screen::default()));
        let (answer, sink) = (writer.clone(), screen.clone());
        std::thread::spawn(move || {
            let _master = pair.master;
            let mut buf = [0u8; 4096];
            let mut scanned = 0;
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut s = sink.lock().unwrap();
                s.raw.extend_from_slice(&buf[..n]);
                // Answer new cursor position queries (row 1, column 1 will do).
                while let Some(at) = s.raw[scanned..].windows(CURSOR_QUERY.len()).position(|w| w == CURSOR_QUERY) {
                    scanned += at + CURSOR_QUERY.len();
                    s.queries += 1;
                    let mut w = answer.lock().unwrap();
                    let _ = w.write_all(b"\x1b[1;1R");
                    let _ = w.flush();
                }
            }
        });
        Some(Repl { child, writer, screen })
    }

    /// The terminal's output without control sequences and carriage returns.
    fn text(&self) -> String {
        plain(&self.screen.lock().unwrap().raw)
    }

    fn queries(&self) -> usize {
        self.screen.lock().unwrap().queries
    }

    /// Waits until `done` holds for the output.
    fn wait(&mut self, what: &str, done: impl Fn(&Repl) -> bool) {
        let start = Instant::now();
        while !done(self) {
            if start.elapsed() > TIMEOUT || self.child.try_wait().unwrap().is_some() {
                let _ = self.child.kill();
                panic!("{what} did not happen; the terminal shows:\n{}", self.text());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the editor to start reading an input (it asks for the
    /// cursor position first), so that keys reach it in raw mode.
    fn wait_for_read(&mut self, reads: usize) {
        self.wait(&format!("input {reads}"), |r| r.queries() >= reads);
        // The query is sent just before the editor reads keys.
        std::thread::sleep(Duration::from_millis(100));
    }

    fn keys(&mut self, keys: &str) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(keys.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    /// How many output lines are exactly `line`.
    fn count(&self, line: &str) -> usize {
        self.text().lines().filter(|l| l.trim() == line).count()
    }

    /// Types `keys` then Enter at the next read, and waits for the output
    /// line `expect` to appear once more (if given).
    fn enter(&mut self, reads: &mut usize, keys: &str, expect: Option<&str>) {
        *reads += 1;
        self.wait_for_read(*reads);
        let before = expect.map(|e| self.count(e));
        self.keys(keys);
        self.keys("\r");
        if let (Some(expect), Some(before)) = (expect, before) {
            self.wait(&format!("output {expect:?} after {keys:?}"), |r| r.count(expect) > before);
        }
    }
}

/// Removes terminal control sequences (CSI, OSC and other escapes) and
/// carriage returns; cursor movements become line breaks.
fn plain(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            if matches!(c, 'H' | 'f' | 'A' | 'B' | 'E' | 'F') {
                                out.push('\n');
                            }
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => out.push('\n'),
            c => out.push(c),
        }
    }
    out
}

#[test]
fn the_line_editor_moves_the_cursor_recalls_and_keeps_history() {
    let dir = TempDir::new("repl-tty");
    let history = dir.path().join("history");
    let Some(mut repl) = Repl::start(dir.path(), &history) else { return };
    let mut reads = 0;

    // Left moves the cursor: `12`, Left, `+` is `1+2`.
    repl.enter(&mut reads, &format!("12{LEFT}+"), Some("3"));
    // Up recalls the previous input.
    repl.enter(&mut reads, UP, Some("3"));
    assert_eq!(repl.count("3"), 2);
    // Home goes to the start of the line, Alt-b (Option-Left) back a word.
    repl.enter(&mut reads, &format!("0 + 5{HOME}1"), Some("15"));
    repl.enter(&mut reads, &format!("2 * 30{ALT_B}1"), Some("260"));

    // Enter continues an incomplete input on the next line.
    repl.enter(&mut reads, "fn sq(x: int) -> int {\rx * x\r}", None);
    repl.enter(&mut reads, "sq(7)", Some("49"));
    // Up twice recalls the whole definition, which runs again.
    repl.enter(&mut reads, &format!("{UP}{UP}"), None);
    repl.enter(&mut reads, "sq(8)", Some("64"));
    assert!(!repl.text().contains("error"), "{}", repl.text());

    reads += 1;
    repl.wait_for_read(reads);
    repl.keys(CTRL_D);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = repl.child.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < TIMEOUT, "sand did not exit; the terminal shows:\n{}", repl.text());
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "sand failed; the terminal shows:\n{}", repl.text());

    // The history file keeps the inputs, a multi-line input as one entry.
    let saved = std::fs::read_to_string(&history).expect("history file");
    let entries: Vec<&str> = saved.lines().collect();
    let def = "fn sq(x: int) -> int {<\\n>x * x<\\n>}";
    assert_eq!(entries, ["1+2", "10 + 5", "2 * 130", def, "sq(7)", def, "sq(8)"], "{saved}");
}

#[test]
fn background_output_and_ctrl_c_leave_the_edited_input_alone() {
    let dir = TempDir::new("repl-tty-tasks");
    let history = dir.path().join("history");
    let Some(mut repl) = Repl::start(dir.path(), &history) else { return };
    let mut reads = 0;

    // A task prints while an input is being edited; the input survives.
    repl.enter(&mut reads, "let t = spawn {\rtask.sleep(300ms)\rprint(\"tick\")\r}", None);
    reads += 1;
    repl.wait_for_read(reads);
    repl.keys("40 +");
    repl.wait("the task's output", |r| r.count("tick") == 1);
    repl.keys(" 2\r");
    repl.wait("the edited input's value", |r| r.count("42") == 1);

    // Ctrl-C discards the input being edited and leaves tasks running...
    repl.enter(&mut reads, "let u = spawn { while true { task.sleep(10ms) } }", None);
    reads += 1;
    repl.wait_for_read(reads);
    repl.keys("1 + 1\x03");
    // ... and at an empty input cancels them.
    reads += 1;
    repl.wait_for_read(reads);
    assert!(!repl.text().contains("cancelled"), "{}", repl.text());
    repl.keys("\x03");
    repl.wait("task cancellation", |r| r.text().contains("cancelled 1 task"));
    assert_eq!(repl.count("2"), 0, "the discarded input ran:\n{}", repl.text());

    reads += 1;
    repl.wait_for_read(reads);
    repl.keys(CTRL_D);
    let start = Instant::now();
    while repl.child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < TIMEOUT, "sand did not exit; the terminal shows:\n{}", repl.text());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn scripts_read_lines_typed_while_an_input_runs() {
    let dir = TempDir::new("repl-tty-stdin");
    let history = dir.path().join("history");
    let Some(mut repl) = Repl::start_with(dir.path(), &history, &[]) else { return };
    let mut reads = 0;
    repl.enter(&mut reads, "import io", None);
    repl.enter(&mut reads, "io.stdin.read_line()?", None);
    std::thread::sleep(Duration::from_millis(500));
    repl.keys("typed (\r");
    repl.wait("the script's line", |r| r.count("Some(\"typed (\")") == 1);
    repl.enter(&mut reads, "io.stdin.read_line()?", None);
    std::thread::sleep(Duration::from_millis(500));
    repl.keys(CTRL_D);
    repl.wait("the end of the script's input", |r| r.text().lines().any(|l| l.trim().ends_with("None")));
    repl.enter(&mut reads, "40 + 2", Some("42"));

    reads += 1;
    repl.wait_for_read(reads);
    repl.keys(CTRL_D);
    let start = Instant::now();
    while repl.child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < TIMEOUT, "sand did not exit; the terminal shows:\n{}", repl.text());
        std::thread::sleep(Duration::from_millis(10));
    }
    let saved = std::fs::read_to_string(&history).expect("history file");
    assert_eq!(saved.lines().collect::<Vec<_>>(), ["import io", "io.stdin.read_line()?", "40 + 2"], "{saved}");
}

#[test]
fn the_repl_asks_on_the_terminal() {
    let dir = TempDir::new("repl-tty-prompt");
    dir.write("a.txt", "A");
    let history = dir.path().join("history");
    let Some(mut repl) = Repl::start(dir.path(), &history) else { return };
    let mut reads = 0;
    repl.enter(&mut reads, "import fs", None);
    repl.keys("fs.read_text(\"a.txt\").is_ok()\r");
    repl.wait("the question", |r| r.text().contains("sand: allow the script to read "));
    repl.keys("a\r");
    repl.wait("the answer", |r| r.text().lines().any(|l| l.trim().ends_with("true")));
    reads += 1;
    repl.enter(&mut reads, "fs.read_text(\"a.txt\")?", Some("\"A\""));
    assert_eq!(repl.text().matches("sand: allow the script to read ").count(), 1, "always: {}", repl.text());
    reads += 1;
    repl.wait_for_read(reads);
    repl.keys(CTRL_D);
    let start = Instant::now();
    while repl.child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < TIMEOUT, "sand did not exit; the terminal shows:\n{}", repl.text());
        std::thread::sleep(Duration::from_millis(10));
    }
}
