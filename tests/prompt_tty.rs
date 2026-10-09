//! End-to-end tests of `sand run --prompt` on a real terminal.
//!
//! `--prompt` asks on the controlling terminal (`/dev/tty` on Unix, the
//! console's `CONIN$`/`CONOUT$` on Windows), never on the script's standard
//! streams. These tests run `sand` on a pseudo-terminal (a Unix pty made the
//! child's controlling terminal, or a Windows ConPTY pseudo console) through
//! `portable-pty`, wait for each question to appear on it, type the answer
//! and check what the script was allowed to do. If the platform cannot
//! provide a pseudo-terminal at all (e.g. a container without `/dev/ptmx`),
//! they print why and pass.

mod common;

use common::TempDir;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const QUESTION: &str = "sand: allow the script to read ";
const MARKER: &str = "---- sand: permission request (the script is paused) ----";

/// What the terminal showed, and how `sand` exited.
struct Transcript {
    code: u32,
    /// The terminal's output without control sequences and carriage returns.
    text: String,
}

impl Transcript {
    /// The lines the script printed (`true` or `false`).
    fn results(&self) -> Vec<&str> {
        self.text.lines().map(str::trim).filter(|l| *l == "true" || *l == "false").collect()
    }
}

/// Removes terminal control sequences (CSI, OSC and other escapes) and
/// carriage returns. Cursor positioning becomes a line break: a Windows
/// pseudo console may move to the next line rather than print one.
fn plain(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // Parameters and intermediates, then a final byte.
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            if c == 'H' || c == 'f' {
                                out.push('\n');
                            }
                            break;
                        }
                    }
                }
                Some(']') => {
                    // Until BEL or ST (ESC \).
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Runs `sand ARGS` in `dir` on a pseudo-terminal, typing one of `answers`
/// each time a permission question appears. `None` if no pseudo-terminal is
/// available.
fn run_on_terminal(dir: &Path, args: &[&str], answers: &[&str]) -> Option<Transcript> {
    let pty = native_pty_system();
    // Wide enough that no question wraps.
    let pair = match pty.openpty(PtySize { rows: 50, cols: 400, pixel_width: 0, pixel_height: 0 }) {
        Ok(p) => p,
        Err(e) => {
            println!("skipped: no pseudo-terminal available: {e}");
            return None;
        }
    };
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_sand"));
    cmd.args(args);
    cmd.cwd(dir);
    cmd.env("TERM", "dumb");
    let mut child = pair.slave.spawn_command(cmd).expect("cannot start sand on the pseudo-terminal");
    // Only the child holds the terminal's other end, so reads end when it exits.
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let sink = output.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
        let _ = done_tx.send(());
    });
    let shown = || plain(&output.lock().unwrap());
    let start = Instant::now();
    for (i, answer) in answers.iter().enumerate() {
        // Wait for question i + 1.
        while shown().matches(QUESTION).count() <= i {
            if start.elapsed() > TIMEOUT || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                panic!("question {} did not appear; the terminal shows:\n{}", i + 1, shown());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        write!(writer, "{answer}\r").unwrap();
        writer.flush().unwrap();
    }
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("sand did not finish; the terminal shows:\n{}", shown());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Collect the rest of the output (a pseudo console only ends it when
    // closed).
    if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        drop(writer);
        drop(pair.master);
        let _ = done_rx.recv_timeout(Duration::from_secs(5));
    }
    Some(Transcript { code: status.exit_code(), text: shown() })
}

/// A script that tries to read a file three times.
fn reader_script() -> TempDir {
    let dir = TempDir::new("tty-prompt");
    dir.write("a.txt", "A");
    dir.write("main.sand", "import fs\nfor i in 0..3 {\n    print(fs.read_text(\"a.txt\").is_ok())\n}\n");
    dir
}

#[test]
fn prompt_asks_on_the_terminal() {
    let dir = reader_script();
    let cases: [(&[&str], [&str; 3], usize); 4] = [
        (&["y", "y", "n"], ["true", "true", "false"], 3),
        (&["a"], ["true", "true", "true"], 1),
        (&["N"], ["false", "false", "false"], 1),
        // An unrecognised answer asks again; an empty one denies.
        (&["n", "maybe", "y", ""], ["false", "true", "false"], 4),
    ];
    for (answers, want, questions) in cases {
        let Some(t) = run_on_terminal(dir.path(), &["run", "main.sand", "--prompt"], answers) else { return };
        assert_eq!(t.code, 0, "answers {answers:?}:\n{}", t.text);
        assert_eq!(t.results(), want, "answers {answers:?}:\n{}", t.text);
        assert_eq!(t.text.matches(QUESTION).count(), questions, "answers {answers:?}:\n{}", t.text);
        // Each question follows the marker and names the file.
        assert_eq!(t.text.matches(MARKER).count(), questions - usize::from(answers.contains(&"maybe")), "{}", t.text);
        assert!(t.text.contains("a.txt?"), "{}", t.text);
    }
    // Rules still apply first: allowed reads are not asked about.
    let Some(t) = run_on_terminal(dir.path(), &["run", "main.sand", "--prompt", "--allow-read=."], &[]) else { return };
    assert_eq!((t.code, t.results()), (0, vec!["true"; 3]), "{}", t.text);
    assert!(!t.text.contains(QUESTION), "{}", t.text);
}
