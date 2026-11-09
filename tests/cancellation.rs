//! Tests that a cancelled task runs each `defer` and `use` cleanup once across
//! all block exit forms and suspension points.

mod common;

use common::run;

/// Function bodies for each way to exit guarded code. `GUARD` is replaced with
/// the cleanup statement. `STOP` is replaced with the final guarded statement.
const EXITS: [(&str, &str); 7] = [
    ("tail", "fn work() {\nGUARD\nSTOP\n}"),
    ("return", "fn work() {\nGUARD\nif on {\nSTOP\nreturn\n}\nprint(\"x\")\n}"),
    ("break", "fn work() {\nwhile on {\nGUARD\nSTOP\nbreak\n}\n}"),
    ("break value", "fn work() {\nlet v = loop {\nGUARD\nSTOP\nbreak 1\n}\nif v == 2 { print(\"x\") }\n}"),
    ("continue", "fn work() {\nfor _ in 0..1 {\nGUARD\nif on {\nSTOP\ncontinue\n}\nprint(\"x\")\n}\n}"),
    ("try", "fn work() -> int? {\nGUARD\nlet missing: int? = if on { None } else { Some(1) }\nSTOP\nmissing?\nSome(1)\n}"),
    ("block value", "fn work() {\nlet v = {\nGUARD\nSTOP\n1\n}\nif v == 2 { print(\"x\") }\n}"),
];

/// Task suspension cases: name, final guarded statement, cleanup body, and
/// main task body.
const STOPS: [(&str, &str, &str, &str); 6] = [
    ("yielded in guarded code", "task.yield_now()", "print(\"d\")", "task.yield_now()\nt.cancel()"),
    ("woken in guarded code", "let _ = gate.recv()", "print(\"d\")", "task.sleep(1ms)\ngate.send(1)\nt.cancel()"),
    ("cancels itself in guarded code", "me.recv().unwrap().cancel()", "print(\"d\")", "me.send(t)"),
    ("yielded at the end of cleanup", "let s = 0", "print(\"d\")\ntask.yield_now()", "task.yield_now()\nt.cancel()"),
    ("woken at the end of cleanup", "let s = 0", "print(\"d\")\nlet _ = gate.recv()", "task.sleep(1ms)\ngate.send(1)\nt.cancel()"),
    ("waiting at the end of cleanup", "let s = 0", "print(\"d\")\nlet _ = gate.recv()", "task.sleep(1ms)\nt.cancel()"),
];

/// Cleanup forms: name and guard statement.
const GUARDS: [(&str, &str); 2] = [("defer", "defer {\nCLEANUP\n}"), ("use", "use r = Res(name: \"r\")")];

#[test]
fn cancelled_tasks_run_cleanup_exactly_once() {
    let mut failures = Vec::new();
    for (guard_name, guard) in GUARDS {
        for (exit_name, body) in EXITS {
            for (stop_name, stop, cleanup, main) in STOPS {
                let body = body.replace("GUARD", &guard.replace("CLEANUP", cleanup)).replace("STOP", stop);
                let script = format!(
                    "struct Res {{ name: str }}\n\
                     impl Close for Res {{\nfn close(self) {{\n{cleanup}\n}}\n}}\n\
                     var on = true\n\
                     let gate: Channel<int> = Channel()\n\
                     let me: Channel<Task<()>> = Channel()\n\
                     {body}\n\
                     let t = spawn {{\nlet r = work()\n}}\n\
                     {main}\n\
                     task.sleep(1ms)\n\
                     print(\"end\")\n"
                );
                let output = run(&script);
                let cleanups = output.ok_stdout().lines().filter(|line| *line == "d").count();
                if cleanups != 1 {
                    failures.push(format!("{guard_name}, {exit_name}, {stop_name}: {cleanups} cleanups"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n{}",
        failures.len(),
        GUARDS.len() * EXITS.len() * STOPS.len(),
        failures.join("\n")
    );
}
