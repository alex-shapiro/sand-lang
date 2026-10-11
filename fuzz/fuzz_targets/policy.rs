//! Policy files: parsing, linting and checking never panic, and formatting
//! is idempotent.
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
    if let Ok(file) = PolicyFile::parse(text, base, home) {
        let policy = file.policy();
        for cap in [
            Capability::FsRead(PathBuf::from("/base/a/b")),
            Capability::FsWrite(PathBuf::from("/home/me/x")),
            Capability::Env("HOME".into()),
            Capability::Http { method: "GET".into(), scheme: "https".into(), host: "a.example.com".into(), port: 443, path: "/a/%2F/b".into() },
        ] {
            let _ = policy.check_normalized(&cap);
        }
    }
});
