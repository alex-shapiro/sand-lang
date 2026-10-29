//! Fuzz target for policy parsing, linting, formatting, and round-trip serialization.
#![no_main]
use libfuzzer_sys::fuzz_target;
use sandlang::Capability;
use sandlang::policy::{PolicyFile, format_policy, lint_policy};
use std::path::{Path, PathBuf};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let (base, home) = (Path::new("/base"), Some(Path::new("/home/me")));
    if let Ok(once) = format_policy(text) {
        assert_eq!(format_policy(&once).expect("formatted policies lex"), once);
    }
    let _ = lint_policy(text, base, home);
    let Ok(file) = PolicyFile::parse(text, base, home) else { return };
    let policy = file.rules;
    let shown = policy.to_string();
    let again = PolicyFile::parse(&shown, base, home).unwrap_or_else(|e| panic!("{shown:?}: {}", e[0].message)).rules;
    let get = Capability::Http { method: "GET".into(), scheme: "http".into(), host: "a.example.com".into(), port: 80, path: "/a%2Fb/c".into() };
    for cap in [
        Capability::FsRead(PathBuf::from("/base/a/b")),
        Capability::FsWrite(PathBuf::from("/home/me/x")),
        Capability::Env("HOME".into()),
        Capability::Env("*".into()),
        Capability::Net { host: "10.0.0.1".into(), port: 80, tls: false },
        get.clone(),
    ] {
        assert_eq!(policy.check_normalized(&cap), again.check_normalized(&cap), "{shown:?}: {cap}");
    }
    for addr in ["10.0.0.1:80", "127.0.0.1:80", "[::ffff:a00:1]:80"] {
        let addr = addr.parse().unwrap();
        assert_eq!(policy.check_resolved(&get, addr), again.check_resolved(&get, addr), "{shown:?}: {get} at {addr}");
    }
});
