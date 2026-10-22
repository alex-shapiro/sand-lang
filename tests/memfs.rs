//! Engines whose scripts see a memory file system (`Config::memory_fs`,
//! and every engine without the `os` feature): the `fs` module, imports and
//! main files, under the policy. (The spec suite also runs every case on a
//! memory file system; these tests cover what it cannot.)

mod common;

use common::rt;
use sandlang::{Capability, Config, Decision, Engine, Error, MemFs, MemorySink, Policy, PromptAnswer, RulePolicy};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn run(config: Config, source: &str) -> (Result<sandlang::Outcome, Error>, String) {
    let out = MemorySink::new();
    let mut engine = Engine::new(config.stdout(out.clone())).unwrap();
    let r = rt().block_on(engine.run_source("main.sand", source));
    (r, out.contents())
}

fn files() -> MemFs {
    let fs = MemFs::new();
    fs.write("/data/in.txt", "input").unwrap();
    fs.write("/data/secret/key.txt", "s3cret").unwrap();
    fs.create_dir_all("/out").unwrap();
    fs
}

#[test]
fn rules_decide_on_memory_paths() {
    let fs = files();
    let rules = RulePolicy::new().allow_read("/data").deny_read("/data/secret").allow_write("/out");
    let src = r#"import fs
print(fs.read_text("/data/in.txt"))
print(fs.read_text("data/../data/in.txt").is_ok())
print(fs.read_text("/data/secret/key.txt").unwrap_err().kind)
print(fs.list_dir("/data/secret").unwrap_err().kind)
print(fs.write_text("/out/a.txt", "A").is_ok())
print(fs.read_text("/out/a.txt"))
print(fs.write_text("/data/in.txt", "x").unwrap_err().kind)
print(fs.rename("/data", "/out/moved").unwrap_err().kind)
print(fs.remove_dir("/data", recursive: true).unwrap_err().kind)
"#;
    let (r, out) = run(Config::new().policy(Policy::rules(rules)).memory_fs(fs.clone()), src);
    r.unwrap();
    assert_eq!(
        out,
        "Ok(\"input\")\ntrue\npermission_denied\npermission_denied\ntrue\nOk(\"A\")\npermission_denied\npermission_denied\npermission_denied\n"
    );
    assert_eq!(fs.read("/out/a.txt").unwrap(), b"A");
    assert_eq!(fs.read("/data/in.txt").unwrap(), b"input");
}

#[test]
fn moving_a_tree_with_protected_paths_is_denied() {
    let fs = files();
    // Writing anywhere is allowed, except under the secret directory.
    let rules = RulePolicy::new().allow_write("/").deny_write("/data/secret");
    let src = r#"import fs
print(fs.rename("/data", "/elsewhere").unwrap_err().kind)
print(fs.remove_dir("/data", recursive: true).unwrap_err().kind)
print(fs.rename("/data/in.txt", "/out/in.txt").is_ok())
"#;
    let (r, out) = run(Config::new().policy(Policy::rules(rules)).memory_fs(fs.clone()), src);
    r.unwrap();
    assert_eq!(out, "permission_denied\npermission_denied\ntrue\n");
    assert_eq!(fs.read("/data/secret/key.txt").unwrap(), b"s3cret");
    // A tree without a protected entry can be moved and removed.
    fs.write("/data/secret/id.key", "k").unwrap();
    fs.write("/out/sub/a.txt", "a").unwrap();
    let rules = RulePolicy::new().allow_write("/").deny_read("/data/secret/id.key").deny_read("/out/sub/missing.key");
    let src = r#"import fs
print(fs.rename("/data", "/elsewhere").unwrap_err().kind)
fs.rename("/out", "/out2")?
fs.remove_dir("/out2", recursive: true)?
print(fs.exists("/out2")?)
"#;
    let (r, out) = run(Config::new().policy(Policy::rules(rules)).memory_fs(fs.clone()), src);
    r.unwrap();
    assert_eq!(out, "permission_denied\nfalse\n");
}

#[test]
fn imports_and_main_files_come_from_the_memory_file_system() {
    let fs = MemFs::new();
    fs.write("/app/main.sand", "import \"lib/util\"\nprint(util.twice(21))\n").unwrap();
    fs.write("/app/lib/util.sand", "import \"helper\"\npub fn twice(x: int) -> int { helper.add(x, x) }\n").unwrap();
    fs.write("/app/lib/helper.sand", "pub fn add(a: int, b: int) -> int { a + b }\n").unwrap();
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().memory_fs(fs.clone()).stdout(out.clone())).unwrap();
    rt().block_on(engine.run_file("/app/main.sand")).unwrap();
    assert_eq!(out.contents(), "42\n");
    engine.check_file("/app/main.sand").unwrap();
    // Imports stay under the module root.
    fs.write("/outside.sand", "pub fn f() -> int { 1 }\n").unwrap();
    fs.write("/app/escape.sand", "import \"../outside\"\nprint(outside.f())\n").unwrap();
    match rt().block_on(engine.run_file("/app/escape.sand")) {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("outside the module root"), "{d:?}"),
        other => panic!("{other:?}"),
    }
    // `allow import` restricts them further.
    let rules = RulePolicy::new().allow_import("/app/lib");
    let mut engine = Engine::new(Config::new().policy(Policy::rules(rules)).memory_fs(fs.clone()).stdout(MemorySink::new())).unwrap();
    rt().block_on(engine.run_file("/app/main.sand")).unwrap();
    fs.write("/app/other.sand", "pub fn f() -> int { 1 }\n").unwrap();
    match rt().block_on(engine.run_source("main.sand", "import \"other\"\nprint(other.f())")) {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("permission denied"), "{d:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn prompts_ask_about_memory_paths() {
    let fs = files();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let prompter = move |cap: Capability| {
        log.lock().unwrap().push(cap.clone());
        async move { if cap == Capability::FsRead(PathBuf::from("/data/in.txt")) { PromptAnswer::AllowOnce } else { PromptAnswer::DenyOnce } }
    };
    let config = Config::new().policy(Policy::rules(RulePolicy::new().fallback(Decision::Prompt))).prompter(prompter).memory_fs(fs);
    let (r, out) = run(
        config.cwd("/data"),
        "import fs\nprint(fs.read_text(\"in.txt\").is_ok())\nprint(fs.read_text(\"/out/../data/secret/key.txt\").is_ok())",
    );
    r.unwrap();
    assert_eq!(out, "true\nfalse\n");
    assert_eq!(
        *asked.lock().unwrap(),
        [Capability::FsRead(PathBuf::from("/data/in.txt")), Capability::FsRead(PathBuf::from("/data/secret/key.txt"))]
    );
}

#[test]
fn the_capacity_limits_what_scripts_write() {
    let fs = MemFs::with_capacity(10_000);
    let src = r#"import fs
let chunk = "x".repeat(4000)
print(fs.write_text("/a", chunk).is_ok())
print(fs.write_text("/b", chunk).is_ok())
let e = fs.write_text("/c", chunk).unwrap_err()
print(e.kind)
print(fs.remove_file("/a").is_ok())
print(fs.write_text("/c", chunk).is_ok())
"#;
    let (r, out) = run(Config::new().policy(Policy::allow_all()).memory_fs(fs.clone()), src);
    r.unwrap();
    assert_eq!(out, "true\ntrue\nlimit\ntrue\ntrue\n");
    assert!(fs.used() <= fs.capacity());
}

#[test]
fn engines_can_share_a_file_system() {
    let fs = MemFs::new();
    let config = || Config::new().policy(Policy::allow_all()).memory_fs(fs.clone());
    let (r, _) = run(config(), "import fs\nfs.write_text(\"/shared.txt\", \"from the first\")?");
    r.unwrap();
    let (r, out) = run(config(), "import fs\nprint(fs.read_text(\"/shared.txt\")?)");
    r.unwrap();
    assert_eq!(out, "from the first\n");
}

#[test]
fn checks_and_lints_see_the_memory_file_system() {
    let fs = MemFs::new();
    fs.write("/lib.sand", "pub fn f() -> int { 1 }\n").unwrap();
    let engine = Engine::new(Config::new().memory_fs(fs)).unwrap();
    engine.check_source("main.sand", "import \"lib\"\nprint(lib.f())").unwrap();
    match engine.check_source("main.sand", "import \"missing\"") {
        Err(Error::Compile(d)) => assert!(d[0].message.contains("cannot load module `missing`"), "{d:?}"),
        other => panic!("{other:?}"),
    }
    let warnings = engine.lint_source("main.sand", "import \"lib\"\nlet unused = lib.f()\n").unwrap();
    assert!(warnings.iter().any(|w| w.message.contains("unused")), "{warnings:?}");
}

/// A rule's path is canonicalised on the host (`/tmp` is a symlink to
/// `/private/tmp` on macOS) but matches memory paths as written.
#[cfg(all(feature = "os", target_os = "macos"))]
#[test]
fn rules_match_memory_paths_as_written() {
    let fs = MemFs::new();
    fs.write("/tmp/x.txt", "x").unwrap();
    let rules = RulePolicy::new().allow_read("/tmp");
    let (r, out) = run(Config::new().policy(Policy::rules(rules)).memory_fs(fs), "import fs\nprint(fs.read_text(\"/tmp/x.txt\").is_ok())");
    r.unwrap();
    assert_eq!(out, "true\n");
}

#[test]
fn trees_cannot_grow_deeper_than_max_path_depth() {
    // Each step uses short paths but nests the tree one level deeper.
    let src = r#"import fs
fs.create_dir("/x")?
var depth = 1
loop {
    fs.create_dir("/y")?
    match fs.rename("/x", "/y/x") {
        Ok(()) => {}
        Err(e) => {
            print("stopped at depth \(depth): \(e.kind)")
            break
        }
    }
    fs.rename("/y", "/x")?
    depth += 1
}
"#;
    let fs = MemFs::new();
    let (r, out) = run(Config::new().policy(Policy::allow_all()).memory_fs(fs.clone()), src);
    r.unwrap();
    // `/x` repeated 2048 times is the longest path (4096 bytes).
    assert_eq!(out, "stopped at depth 2048: io\n");
    assert!(fs.files().is_empty());
    // Dropping the deep tree does not recurse.
    drop(fs);
}

#[test]
fn imports_obey_read_deny_rules_and_load_only_scripts() {
    let fs = MemFs::new();
    fs.write("/data/secret/conf.sand", "pub let TOKEN = \"TOPSECRET\"\n").unwrap();
    fs.write("/data/key.txt", "SECRET\n").unwrap();
    fs.write("/data/ok.sand", "pub let X = 1\n").unwrap();
    let rules = || RulePolicy::new().allow_read("/data").deny_read("/data/secret");
    let config = || Config::new().policy(Policy::rules(rules())).memory_fs(fs.clone());
    let (r, out) = run(config(), "import \"data/ok\"\nprint(ok.X)");
    r.unwrap();
    assert_eq!(out, "1\n");
    for (src, message) in [
        ("from \"/data/secret/conf\" import TOKEN\nprint(TOKEN)", "permission denied"),
        ("import \"data/key.txt\"", "script modules are `.sand` files"),
    ] {
        match run(config(), src).0 {
            Err(Error::Compile(d)) => {
                assert!(d[0].message.contains(message), "{d:?}");
                assert!(!format!("{d:?}").contains("SECRET"), "{d:?}");
            }
            other => panic!("{src}: {other:?}"),
        }
    }
}

#[test]
fn waiting_for_prompt_answers_is_not_run_time() {
    // Every answer takes longer than the whole time limit.
    let fs = MemFs::new();
    fs.write("/lib.sand", "pub fn twice(x: int) -> int { x * 2 }\n").unwrap();
    fs.write("/a.txt", "21").unwrap();
    let slow_yes = |_: Capability| async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        PromptAnswer::AllowOnce
    };
    let limits = sandlang::Limits { max_wall_time: Some(std::time::Duration::from_millis(200)), ..Default::default() };
    let config = Config::new()
        .policy(Policy::rules(RulePolicy::new().fallback(Decision::Prompt).allow_import("/nowhere")))
        .prompter(slow_yes)
        .limits(limits)
        .memory_fs(fs);
    let src = "import \"lib\"\nimport fs\nlet n = fs.read_text(\"/a.txt\").unwrap().parse_int().unwrap()\nprint(lib.twice(n))\nprint(fs.read_text(\"/a.txt\").is_ok())";
    let (r, out) = run(config, src);
    r.unwrap();
    assert_eq!(out, "42\ntrue\n");
}
