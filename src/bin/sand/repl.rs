//! `sand repl`: the interactive session.
//!
//! On a terminal, inputs are edited with a line editor (reedline): the
//! cursor moves with the arrow keys and the usual Emacs and macOS bindings,
//! Up and Down recall earlier inputs (kept in a history file), an input with
//! open brackets, strings or comments continues on `...` lines and is edited
//! as a whole, and output of background tasks appears above the input being
//! edited. Otherwise (piped input) lines are read plainly, one at a time.

use crate::{exit_status, run_options, runtime};
use nu_ansi_term::{Color, Style};
use reedline::{
    EditCommand, EditMode, Emacs, ExternalPrinter, FileBackedHistory, Highlighter, KeyCode, KeyModifiers, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineRawEvent, Signal, StyledText, ValidationResult, Validator,
    default_emacs_keybindings,
};
use sandlang::syntax::highlight::{Highlight, highlight};
use sandlang::{Engine, Error, Session};
use std::borrow::Cow;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const REPL_HELP: &str = "\
Enter statements, expressions and declarations; each input runs once, in the
context of the previous ones. The value of a final expression is shown (end
the input with `;` to hide it). Input continues on `...` lines while brackets,
strings or comments are open. Tasks spawned by an input keep running, also
while the REPL waits for input. Ctrl-C interrupts a running input (and cancels
all tasks), discards a partial input, or at an empty prompt cancels the tasks
left running; Ctrl-D or :quit exits.

EDITING (on a terminal):
    Left, Right                    move the cursor
    Alt/Option-Left, Alt-Right     move by word (also Ctrl-Left/Right, Alt-b/f)
    Home, End, Ctrl-A, Ctrl-E      go to the start or end of the line
    Up, Down                       previous or next line, or input from history
    Ctrl-R                         search the history
    Ctrl-W, Ctrl-U, Ctrl-K         delete a word, to the start, to the end
History is kept in $SAND_HISTORY (default ~/.sand_history; empty: not kept).

COMMANDS:
    :help          show this help
    :type EXPR     show the type of EXPR without running it
    :test          run the `test` blocks entered so far
    :reset         forget all inputs and start over
    :quit, :q      exit
";

/// Entries kept in the history file.
const HISTORY_ENTRIES: usize = 1000;

/// The interactive session: reads inputs (possibly spanning several lines)
/// and runs each once in a persistent session.
pub(crate) fn cmd_repl(args: &[String]) -> Result<u8, String> {
    let opts = run_options(args)?;
    if let Some(f) = opts.file {
        return Err(format!("unexpected argument `{}` (`sand repl` takes options only)", f.display()));
    }
    let rt = runtime()?;
    let screen = Screen::default();
    let config =
        opts.config.stdout(ReplSink { screen: screen.clone(), stderr: false }).stderr(ReplSink { screen: screen.clone(), stderr: true });
    let mut session = Engine::new(config).session();
    let reader = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Reader::Editor(EditorReader::new(screen.clone()))
    } else {
        Reader::Plain(LineReader::new())
    };
    println!("sand {} — :help for help, :quit to exit", env!("CARGO_PKG_VERSION"));
    Ok(rt.block_on(repl(&mut session, &screen, reader)))
}

/// What the front end read.
enum Got {
    /// A line with its terminator (from the line editor: a whole input).
    Line(String),
    /// Ctrl-C; `discarded` if it discarded text being edited.
    Interrupt { discarded: bool },
    /// End of input (Ctrl-D).
    End,
}

async fn repl(session: &mut Session, screen: &Screen, mut reader: Reader) -> u8 {
    let mut input = String::new();
    loop {
        let prompt = if input.is_empty() { "> " } else { "... " };
        // Background tasks run while the REPL waits for an input (or Ctrl-C).
        let next = async {
            tokio::select! {
                got = reader.next(screen, prompt) => got,
                Ok(()) = tokio::signal::ctrl_c() => Got::Interrupt { discarded: false },
            }
        };
        let on_error = |e: Error| {
            let mut text = repl_error_text(&e, "");
            text.push_str("(in a background task; all tasks were cancelled)\n");
            let _ = screen.write(true, text.as_bytes());
        };
        let got = session.run_tasks_until(next, on_error).await;
        screen.hide();
        let Some(got) = got else {
            return exit_status(session.exit_code().unwrap_or(0));
        };
        let line = match got {
            Got::Line(line) => line,
            Got::Interrupt { discarded } => {
                if reader.echoes_interrupt() {
                    println!();
                }
                if input.is_empty() && !discarded {
                    match session.cancel_tasks() {
                        0 => {}
                        1 => eprintln!("cancelled 1 task"),
                        n => eprintln!("cancelled {n} tasks"),
                    }
                }
                input.clear();
                continue;
            }
            Got::End => {
                // End of input: run what was entered.
                if !input.trim().is_empty() {
                    println!();
                    eval(session, &input).await;
                }
                break;
            }
        };
        if input.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(command) = trimmed.strip_prefix(':') {
                let (name, arg) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
                match name {
                    "quit" | "q" => break,
                    "help" | "h" => print!("{REPL_HELP}"),
                    "reset" => session.reset(),
                    "type" | "t" => match session.type_of(arg.trim()) {
                        Ok(t) => println!("{t}"),
                        Err(e) => eprint!("{}", repl_error_text(&e, arg)),
                    },
                    "test" => match interruptible(session.interrupt_handle(), session.run_tests()).await {
                        Ok(report) => print!("{}", report.render()),
                        Err(e) => eprint!("{}", repl_error_text(&e, "")),
                    },
                    _ => eprintln!("unknown command `:{name}` (:help lists the commands)"),
                }
                continue;
            }
        }
        input.push_str(&line);
        if !Session::is_complete(&input) {
            continue;
        }
        eval(session, &std::mem::take(&mut input)).await;
        if let Some(code) = session.exit_code() {
            return exit_status(code);
        }
    }
    0
}

/// Runs one input, printing its value or error.
async fn eval(session: &mut Session, input: &str) {
    let handle = session.interrupt_handle();
    match interruptible(handle, session.eval(input)).await {
        Ok(Some(value)) => println!("{value}"),
        Ok(None) => {}
        Err(e) => eprint!("{}", repl_error_text(&e, input)),
    }
}

/// Awaits `fut`, interrupting the session it runs on when Ctrl-C is pressed.
async fn interruptible<T>(handle: sandlang::InterruptHandle, fut: impl std::future::Future<Output = T>) -> T {
    let mut fut = std::pin::pin!(fut);
    loop {
        tokio::select! {
            r = &mut fut => return r,
            Ok(()) = tokio::signal::ctrl_c() => handle.interrupt(),
        }
    }
}

/// The report of an error of an input (whose text is `input`).
fn repl_error_text(e: &Error, input: &str) -> String {
    match e {
        Error::Compile(diags) => diags
            .iter()
            .map(|d| {
                // Diagnostics are in the input, or in a module it imports.
                let source = if d.file.starts_with("<input ") || d.file == "<type>" {
                    Some(input.to_string())
                } else {
                    std::fs::read_to_string(&d.file).ok()
                };
                d.render(source.as_deref())
            })
            .collect(),
        _ => format!("{e}\n"),
    }
}

// ----------------------------------------------------------------------
// Output

/// The terminal shared by the REPL and its scripts' output, which may
/// arrive from background tasks while the REPL waits for an input.
///
/// While the plain reader waits, the prompt ends the output: script output
/// starts on a new line and the prompt is printed again after it. While the
/// line editor reads, complete lines of output go to the editor, which
/// prints them above the input being edited.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<ScreenState>>);

#[derive(Default)]
struct ScreenState {
    /// The plain reader's prompt, while it waits for input.
    prompt: Option<&'static str>,
    /// Whether the prompt ends the output so far.
    visible: bool,
    /// The line editor's printer, while it reads an input.
    printer: Option<ExternalPrinter<String>>,
    /// Output received while the editor reads that does not end a line yet.
    partial: Vec<u8>,
}

impl Screen {
    fn state(&self) -> MutexGuard<'_, ScreenState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Prints `prompt` and waits for input (plain reader).
    fn show(&self, prompt: &'static str) {
        let mut st = self.state();
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(prompt.as_bytes());
        let _ = out.flush();
        st.prompt = Some(prompt);
        st.visible = true;
    }

    /// Stops waiting for input.
    fn hide(&self) {
        let mut st = self.state();
        st.prompt = None;
        st.visible = false;
    }

    /// The line editor starts reading an input: output goes to `printer`.
    fn start_editing(&self, printer: ExternalPrinter<String>) {
        self.state().printer = Some(printer);
    }

    /// The line editor has stopped reading: prints the output it did not
    /// print yet (with an unfinished last line), and writes directly again.
    fn stop_editing(&self) {
        let mut st = self.state();
        let Some(printer) = st.printer.take() else { return };
        let mut out = std::io::stdout().lock();
        while let Some(lines) = printer.get_line() {
            let _ = writeln!(out, "{lines}");
        }
        let _ = out.write_all(&std::mem::take(&mut st.partial));
        let _ = out.flush();
    }

    /// Writes script output (`stderr` for standard error).
    fn write(&self, stderr: bool, data: &[u8]) -> std::io::Result<()> {
        let mut st = self.state();
        if st.printer.is_some() {
            st.partial.extend_from_slice(data);
            let end = match st.partial.iter().rposition(|&b| b == b'\n') {
                Some(end) => end + 1,
                // A long unfinished line is shown in pieces rather than kept.
                None if st.partial.len() >= MAX_PARTIAL_LINE => st.partial.len(),
                None => return Ok(()),
            };
            let rest = st.partial.split_off(end);
            let lines = std::mem::replace(&mut st.partial, rest);
            // The editor prints messages line by line (on standard output:
            // standard error is not told apart while an input is edited)
            // and drops an empty message.
            let mut message = String::from_utf8_lossy(lines.strip_suffix(b"\n").unwrap_or(&lines)).into_owned();
            if message.is_empty() {
                message.push(' ');
            }
            loop {
                let Some(printer) = &st.printer else {
                    return direct(stderr, message.as_bytes()).and_then(|()| direct(stderr, b"\n"));
                };
                // Never block here: the editor drains its printer only while
                // it reads, and it may have just stopped.
                match printer.sender().try_send(message) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.is_full() => {
                        message = e.into_inner();
                        drop(st);
                        std::thread::sleep(Duration::from_millis(5));
                        st = self.state();
                    }
                    Err(e) => {
                        let message = e.into_inner();
                        return direct(stderr, message.as_bytes()).and_then(|()| direct(stderr, b"\n"));
                    }
                }
            }
        }
        if st.visible {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            st.visible = false;
        }
        let r = direct(stderr, data);
        if let Some(prompt) = st.prompt
            && data.ends_with(b"\n")
        {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(prompt.as_bytes());
            let _ = out.flush();
            st.visible = true;
        }
        r
    }
}

/// Output without a line end that is held while an input is edited, at most.
const MAX_PARTIAL_LINE: usize = 4096;

/// Writes to standard output (flushed) or error.
fn direct(stderr: bool, data: &[u8]) -> std::io::Result<()> {
    if stderr {
        std::io::stderr().lock().write_all(data)
    } else {
        let mut out = std::io::stdout().lock();
        out.write_all(data)?;
        out.flush()
    }
}

/// Standard output or error of the REPL's scripts (see [`Screen`]).
struct ReplSink {
    screen: Screen,
    stderr: bool,
}

impl sandlang::OutputSink for ReplSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.screen.write(self.stderr, data)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.stderr { std::io::stderr().flush() } else { std::io::stdout().flush() }
    }
}

// ----------------------------------------------------------------------
// Input

/// Where inputs come from.
enum Reader {
    Plain(LineReader),
    Editor(EditorReader),
}

impl Reader {
    /// The next line or input, after showing `prompt`. Cancel-safe.
    async fn next(&mut self, screen: &Screen, prompt: &'static str) -> Got {
        match self {
            Reader::Plain(lines) => {
                screen.show(prompt);
                match lines.next().await {
                    Some(line) => Got::Line(line),
                    None => Got::End,
                }
            }
            Reader::Editor(editor) => editor.next(prompt).await,
        }
    }

    /// Whether the terminal shows Ctrl-C without ending the line (so the
    /// REPL ends it); the line editor ends it itself.
    fn echoes_interrupt(&self) -> bool {
        matches!(self, Reader::Plain(_))
    }
}

/// Reads lines of standard input on a thread, one per request, so that the
/// REPL can wait for a line and for Ctrl-C at once. Nothing is read between
/// requests, so a prompter can read answers from standard input meanwhile.
struct LineReader {
    requests: std::sync::mpsc::Sender<()>,
    lines: tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    pending: bool,
}

impl LineReader {
    fn new() -> LineReader {
        let (requests, wanted) = std::sync::mpsc::channel::<()>();
        let (send, lines) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for () in wanted {
                let mut line = String::new();
                let got = match std::io::stdin().lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line),
                };
                let end = got.is_none();
                if send.send(got).is_err() || end {
                    break;
                }
            }
        });
        LineReader { requests, lines, pending: false }
    }

    /// The next line, with its terminator; `None` at the end of input.
    /// Cancel-safe: if the future is dropped, the next call gets the line.
    async fn next(&mut self) -> Option<String> {
        if !self.pending {
            if self.requests.send(()).is_err() {
                return None;
            }
            self.pending = true;
        }
        let line = self.lines.recv().await.flatten();
        self.pending = false;
        line
    }
}

/// Reads inputs with the line editor on a thread (it blocks), one per
/// request, like [`LineReader`]: the terminal is left alone between
/// requests, for permission prompts and running inputs.
struct EditorReader {
    requests: std::sync::mpsc::Sender<&'static str>,
    inputs: tokio::sync::mpsc::UnboundedReceiver<Got>,
    pending: bool,
}

impl EditorReader {
    fn new(screen: Screen) -> EditorReader {
        let (requests, wanted) = std::sync::mpsc::channel::<&'static str>();
        let (send, inputs) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let printer = ExternalPrinter::new(256);
            let buffer = Arc::new(BufferState::default());
            let mut editor = line_editor(printer.clone(), buffer.clone());
            for prompt in wanted {
                screen.start_editing(printer.clone());
                let signal = editor.read_line(&ReplPrompt(prompt));
                screen.stop_editing();
                let _ = editor.sync_history();
                let got = match signal {
                    Ok(Signal::Success(input)) => Got::Line(input + "\n"),
                    Ok(Signal::CtrlC) => Got::Interrupt { discarded: !buffer.surely_empty() },
                    Ok(Signal::CtrlD) => Got::End,
                    Ok(_) => continue,
                    Err(e) => {
                        eprintln!("sand: cannot read the terminal: {e}");
                        Got::End
                    }
                };
                let end = matches!(got, Got::End);
                if send.send(got).is_err() || end {
                    break;
                }
            }
        });
        EditorReader { requests, inputs, pending: false }
    }

    /// The next input (after `prompt`). Cancel-safe: if the future is
    /// dropped, the next call gets the input.
    async fn next(&mut self, prompt: &'static str) -> Got {
        if !self.pending {
            if self.requests.send(prompt).is_err() {
                return Got::End;
            }
            self.pending = true;
        }
        let got = self.inputs.recv().await.unwrap_or(Got::End);
        self.pending = false;
        got
    }
}

/// Whether the input being edited is empty, for Ctrl-C (which cancels
/// tasks only at an empty input). The editor clears the input before it
/// reports Ctrl-C, so this is tracked from outside: the highlighter sees
/// the input at each repaint, and keys read since may have changed it (a
/// key and Ctrl-C can arrive together, before a repaint).
#[derive(Default)]
struct BufferState {
    /// Whether the input was empty when last painted.
    empty: AtomicBool,
    /// Whether keys were read since.
    keys_since_paint: AtomicBool,
}

impl BufferState {
    fn surely_empty(&self) -> bool {
        self.empty.load(Ordering::Relaxed) && !self.keys_since_paint.load(Ordering::Relaxed)
    }
}

/// Emacs-style editing that records keys in [`BufferState`].
struct TrackedEmacs {
    inner: Emacs,
    buffer: Arc<BufferState>,
}

impl EditMode for TrackedEmacs {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let event = self.inner.parse_event(event);
        if !matches!(event, ReedlineEvent::CtrlC | ReedlineEvent::None) {
            self.buffer.keys_since_paint.store(true, Ordering::Relaxed);
        }
        event
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

/// The line editor: Emacs-style bindings, the history file, inputs
/// continued while incomplete, syntax highlighting and `printer` for
/// output arriving while an input is edited.
fn line_editor(printer: ExternalPrinter<String>, buffer: Arc<BufferState>) -> Reedline {
    let editor = Reedline::create()
        .with_edit_mode(Box::new(TrackedEmacs { inner: Emacs::new(keybindings()), buffer: buffer.clone() }))
        .with_validator(Box::new(InputValidator))
        .with_highlighter(Box::new(SandHighlighter { buffer }))
        .with_external_printer(printer)
        .with_poll_interval(Duration::from_millis(30))
        .use_bracketed_paste(true);
    match history_file() {
        None => editor.with_history(Box::new(FileBackedHistory::new(HISTORY_ENTRIES).expect("history capacity is valid"))),
        Some(path) => match FileBackedHistory::with_file(HISTORY_ENTRIES, path.clone()) {
            Ok(h) => editor.with_history(Box::new(h)),
            Err(e) => {
                eprintln!("sand: history is not kept: cannot use {}: {e}", path.display());
                editor.with_history(Box::new(FileBackedHistory::new(HISTORY_ENTRIES).expect("history capacity is valid")))
            }
        },
    }
}

/// The history file: `$SAND_HISTORY` (empty for none), else `~/.sand_history`.
fn history_file() -> Option<PathBuf> {
    match std::env::var_os("SAND_HISTORY") {
        Some(p) if p.is_empty() => None,
        Some(p) => Some(PathBuf::from(p)),
        None => std::env::home_dir().map(|h| h.join(".sand_history")),
    }
}

/// Reedline's Emacs bindings, plus moves that macOS terminals send or that
/// terminals reporting the Command key (as Super) send, and without the
/// binding that opens an external editor (a subprocess).
fn keybindings() -> reedline::Keybindings {
    let mut kb = default_emacs_keybindings();
    let edit = |c: EditCommand| ReedlineEvent::Edit(vec![c]);
    // Option-Left/Right: Alt-Left/Right or Alt-b/f, all bound by default.
    // Command-Left/Right: Home/End or Ctrl-A/E in most terminals (also bound).
    kb.add_binding(KeyModifiers::SUPER, KeyCode::Left, edit(EditCommand::MoveToLineStart { select: false }));
    kb.add_binding(KeyModifiers::SUPER, KeyCode::Right, edit(EditCommand::MoveToLineEnd { select: false }));
    kb.add_binding(KeyModifiers::SUPER, KeyCode::Backspace, edit(EditCommand::CutFromLineStart));
    kb.add_binding(KeyModifiers::CONTROL, KeyCode::Home, edit(EditCommand::MoveToStart { select: false }));
    kb.add_binding(KeyModifiers::CONTROL, KeyCode::End, edit(EditCommand::MoveToEnd { select: false }));
    kb.remove_binding(KeyModifiers::CONTROL, KeyCode::Char('o'));
    kb
}

/// The REPL's prompt: `> `, and `... ` on continuation lines.
struct ReplPrompt(&'static str);

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.0)
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(history search: {}) ", search.term))
    }
    fn get_prompt_color(&self) -> reedline::Color {
        reedline::Color::Default
    }
    fn get_prompt_multiline_color(&self) -> reedline::Color {
        reedline::Color::Default
    }
}

/// Enter continues an input that is not complete (open brackets, strings or
/// comments, a trailing operator) on a new line, like the plain reader.
struct InputValidator;

impl Validator for InputValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        // Commands are single lines.
        if line.trim_start().starts_with(':') || Session::is_complete(line) {
            ValidationResult::Complete
        } else {
            ValidationResult::Incomplete
        }
    }
}

/// Colors keywords, literals and comments. Also records whether the input
/// is empty (see [`BufferState`]).
struct SandHighlighter {
    buffer: Arc<BufferState>,
}

impl Highlighter for SandHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        self.buffer.empty.store(line.is_empty(), Ordering::Relaxed);
        self.buffer.keys_since_paint.store(false, Ordering::Relaxed);
        let mut styled = StyledText::new();
        let mut at = 0;
        for (lo, hi, kind) in highlight(line) {
            if lo > at {
                styled.push((Style::new(), line[at..lo].to_string()));
            }
            styled.push((style(kind), line[lo..hi].to_string()));
            at = hi;
        }
        if at < line.len() {
            styled.push((Style::new(), line[at..].to_string()));
        }
        styled
    }
}

/// The terminal style of highlighted text.
fn style(kind: Highlight) -> Style {
    let color = match kind {
        Highlight::Keyword => Color::Magenta,
        Highlight::Constant | Highlight::Number => Color::Yellow,
        Highlight::String => Color::Green,
        Highlight::Interpolation => Color::Cyan,
        Highlight::Comment | Highlight::DocComment => Color::DarkGray,
        Highlight::Type => Color::Blue,
        Highlight::Call => return Style::new(),
    };
    Style::new().fg(color)
}
