//! Sandbox hardening of I/O: resolved network addresses, file system races
//! and symlinks, subtree deny rules, and memory accounting of pending I/O.

mod common;

use common::{HttpResponse, TempDir, http_server, run_with};
use sandlang::policy::is_private_address;
use sandlang::{Capability, Decision, Limits, Policy, PromptAnswer, RulePolicy};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Network: patterns, private ranges and resolved addresses
// ---------------------------------------------------------------------------

fn net(host: &str, port: u16) -> Capability {
    Capability::Net { host: host.into(), port }
}

fn net_addr(host: &str, addr: &str) -> Capability {
    Capability::NetAddr { host: host.into(), addr: addr.parse().unwrap() }
}

#[test]
fn net_patterns_match_ip_literals_and_ranges() {
    let p = RulePolicy::new()
        .allow_net("10.0.0.0/8")
        .allow_net("192.168.1.5:8080")
        .allow_net("[::1]:9000")
        .allow_net("fd00::/8")
        .allow_net("example.com:443")
        .deny_net("10.9.0.0/16");
    assert_eq!(p.check(&net("10.1.2.3", 80)), Decision::Allow);
    assert_eq!(p.check(&net("10.9.2.3", 80)), Decision::Deny);
    assert_eq!(p.check(&net("11.0.0.1", 80)), Decision::Deny);
    assert_eq!(p.check(&net("192.168.1.5", 8080)), Decision::Allow);
    assert_eq!(p.check(&net("192.168.1.5", 8081)), Decision::Deny);
    assert_eq!(p.check(&net("::1", 9000)), Decision::Allow);
    assert_eq!(p.check(&net("[::1]", 9000)), Decision::Allow);
    assert_eq!(p.check(&net("::1", 9001)), Decision::Deny);
    assert_eq!(p.check(&net("fd12::1", 1)), Decision::Allow);
    // IPv4-mapped IPv6 literals are the IPv4 address.
    assert_eq!(p.check(&net("::ffff:10.1.2.3", 80)), Decision::Allow);
    assert_eq!(p.check(&net("::ffff:10.9.2.3", 80)), Decision::Deny);
    assert_eq!(p.check(&net("EXAMPLE.com", 443)), Decision::Allow);
    // Range patterns never match names.
    assert_eq!(p.check(&net("10.example", 80)), Decision::Deny);
    // Without `deny_private_networks`, resolved addresses are only checked against deny ranges.
    assert_eq!(p.check(&net_addr("example.com", "127.0.0.1:443")), Decision::Allow);
    assert_eq!(p.check(&net_addr("example.com", "10.9.0.1:443")), Decision::Deny);
    assert_eq!(p.check(&net_addr("example.com", "[::ffff:10.9.0.1]:443")), Decision::Deny);
}

#[test]
fn deny_ranges_catch_other_spellings_of_their_addresses() {
    // `0.0.0.0` and `::` connect to the local host.
    let p = RulePolicy::new().allow_net("*").deny_net("127.0.0.0/8").deny_net("10.0.0.0/8").deny_net("[::1]");
    for host in [
        "0.0.0.0",
        "::",
        "[::]",
        "::ffff:0.0.0.0",
        "::ffff:127.0.0.1",
        "::7f00:1",
        "64:ff9b::7f00:1",
        "2002:7f00:1::",
        "64:ff9b::a00:1",
        "::ffff:0:a00:1",
    ] {
        assert_eq!(p.check(&net(host, 8080)), Decision::Deny, "{host}");
        let addr = SocketAddr::new(host.trim_matches(['[', ']']).parse::<IpAddr>().unwrap(), 8080);
        assert_eq!(p.check(&Capability::NetAddr { host: "example.com".into(), addr }), Decision::Deny, "{host}");
    }
    for host in ["8.8.8.8", "2001:4860::8888", "64:ff9b::808:808", "2002:808:808::"] {
        assert_eq!(p.check(&net(host, 8080)), Decision::Allow, "{host}");
    }
    // Only deny rules are widened.
    let p = RulePolicy::new().allow_net("127.0.0.1");
    assert_eq!(p.check(&net("0.0.0.0", 80)), Decision::Deny);
}

#[test]
fn net_pattern_edge_cases() {
    let p = RulePolicy::new()
        .allow_net("*.example.com")
        .allow_net("[fc00::]/7")
        .allow_net("[2001:db8::/32]:443")
        .allow_net("::ffff:10.0.0.0/104")
        .deny_net("evil.com")
        .deny_net("*.bad.org");
    // Non-ASCII hosts never panic and never match ASCII patterns.
    assert_eq!(p.check(&net("ab\u{e9}xample.com", 80)), Decision::Deny);
    assert_eq!(p.check(&net("\u{e9}.example.com", 80)), Decision::Allow);
    // A trailing dot names the same host.
    assert_eq!(p.check(&net("api.example.com.", 80)), Decision::Allow);
    assert_eq!(RulePolicy::new().allow_net("*").deny_net("evil.com").check(&net("EVIL.com.", 80)), Decision::Deny);
    assert_eq!(RulePolicy::new().allow_net("*").deny_net("*.bad.org").check(&net("x.bad.org.", 80)), Decision::Deny);
    // Bracketed IPv6 ranges, with and without a port, and IPv4-mapped ranges.
    assert_eq!(p.check(&net("fd00::1", 1)), Decision::Allow);
    assert_eq!(p.check(&net("2001:db8::5", 443)), Decision::Allow);
    assert_eq!(p.check(&net("2001:db8::5", 80)), Decision::Deny);
    assert_eq!(p.check(&net("10.2.3.4", 80)), Decision::Allow);
    // Validation.
    use sandlang::policy::is_valid_net_pattern;
    for ok in [
        "*",
        "*:443",
        "*.example.com",
        "example.com",
        "example.com.",
        "host:8080",
        "10.0.0.0/8",
        "10.0.0.0/8:443",
        "::1",
        "[::1]:80",
        "[fc00::]/7",
        "[fc00::/7]:443",
        "fc00::/7",
    ] {
        assert!(is_valid_net_pattern(ok), "{ok}");
    }
    for bad in [
        "",
        "host:99999",
        "host:x",
        "10.0.0.0/33",
        "fc00::/129",
        "[::1",
        "[::1]x",
        "ex ample.com",
        "\u{e9}.com",
        "*.",
        "[example.com]",
        "a/b",
    ] {
        assert!(!is_valid_net_pattern(bad), "{bad}");
    }
}

#[test]
fn private_address_classification() {
    let private = [
        "0.0.0.0",
        "10.1.2.3",
        "100.64.0.1",
        "100.127.255.255",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.8",
        "192.0.2.1",
        "192.168.0.1",
        "198.18.0.1",
        "203.0.113.9",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::127.0.0.1",
        "::ffff:127.0.0.1",
        "::ffff:192.168.1.1",
        "::ffff:0:127.0.0.1",
        "64:ff9b::7f00:1",
        "2002:7f00:1::",
        "2001:db8::1",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "fec0::1",
        "ff02::1",
    ];
    let public = [
        "1.1.1.1",
        "8.8.8.8",
        "100.63.255.255",
        "100.128.0.0",
        "172.32.0.1",
        "93.184.215.14",
        "2606:4700::1111",
        "64:ff9b::808:808",
        "2002:808:808::",
    ];
    for ip in private {
        assert!(is_private_address(ip.parse::<IpAddr>().unwrap()), "{ip} should be private");
    }
    for ip in public {
        assert!(!is_private_address(ip.parse::<IpAddr>().unwrap()), "{ip} should be public");
    }
}

#[test]
fn deny_private_networks_rules() {
    let p = RulePolicy::new().allow_net("*").deny_private_networks();
    // Names are allowed by the wildcard; their addresses are checked after resolution.
    assert_eq!(p.check(&net("example.com", 80)), Decision::Allow);
    assert_eq!(p.check(&net_addr("example.com", "93.184.215.14:80")), Decision::Allow);
    assert_eq!(p.check(&net_addr("example.com", "127.0.0.1:80")), Decision::Deny);
    assert_eq!(p.check(&net_addr("example.com", "[::ffff:169.254.169.254]:80")), Decision::Deny);
    assert_eq!(p.check(&net_addr("example.com", "[fd00::1]:80")), Decision::Deny);
    // Internal IP literals and localhost need an explicit rule: the wildcard is not enough.
    assert_eq!(p.check(&net("127.0.0.1", 80)), Decision::Deny);
    assert_eq!(p.check(&net("::ffff:127.0.0.1", 80)), Decision::Deny);
    assert_eq!(p.check(&net("localhost", 80)), Decision::Deny);
    assert_eq!(p.check(&net("8.8.8.8", 53)), Decision::Allow);

    let p = RulePolicy::new().allow_net("127.0.0.1:8080").allow_net("localhost").allow_net("10.0.0.0/8").deny_private_networks();
    assert_eq!(p.check(&net("127.0.0.1", 8080)), Decision::Allow);
    assert_eq!(p.check(&net("127.0.0.1", 8081)), Decision::Deny);
    assert_eq!(p.check(&net_addr("127.0.0.1", "127.0.0.1:8080")), Decision::Allow);
    assert_eq!(p.check(&net("localhost", 1234)), Decision::Allow);
    assert_eq!(p.check(&net_addr("localhost", "[::1]:1234")), Decision::Allow);
    assert_eq!(p.check(&net_addr("localhost", "127.0.0.1:1234")), Decision::Allow);
    // `localhost` must resolve to loopback.
    assert_eq!(p.check(&net_addr("localhost", "192.168.0.1:1234")), Decision::Deny);
    // Range allow rules exempt resolved addresses.
    assert_eq!(p.check(&net_addr("db.internal", "10.1.1.1:5432")), Decision::Allow);
    assert_eq!(p.check(&net_addr("db.internal", "192.168.1.1:5432")), Decision::Deny);

    // With a prompting fallback, an internal literal is asked about rather than allowed by `*`.
    let p = RulePolicy::new().allow_net("*").deny_private_networks().fallback(Decision::Prompt);
    assert_eq!(p.check(&net("127.0.0.1", 80)), Decision::Prompt);
    assert_eq!(p.check(&net_addr("example.com", "127.0.0.1:80")), Decision::Deny);
}

#[cfg(unix)]
fn private_policy() -> Policy {
    Policy::rules(RulePolicy::new().allow_net("*").deny_private_networks())
}

// Relies on the resolver accepting `127.1` for 127.0.0.1 (glibc, macOS).
#[cfg(unix)]
#[test]
fn names_resolving_to_private_addresses_are_denied() {
    let server = http_server(|_| HttpResponse::new(200, "internal"));
    let port = server.addr.port();
    // `127.1` and `2130706433` are not IP literals, but the resolver maps them to 127.0.0.1.
    let src = format!(
        r#"
import http
import net
for host in ["127.1", "2130706433", "localhost", "127.0.0.1"] {{
    let e = http.get("http://{{host}}:{port}/").unwrap_err()
    print("{{e.kind}}: {{e.message.contains("127.0.0.1") or e.message.contains("::1")}}")
}}
print(net.connect("127.1:{port}").unwrap_err().kind)
"#
    );
    let r = run_with(&src, |c| c.policy(private_policy()));
    assert_eq!(
        r.ok_stdout(),
        "permission_denied: true\npermission_denied: true\npermission_denied: false\npermission_denied: true\npermission_denied\n"
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

// Relies on the resolver accepting `127.1` for 127.0.0.1 (glibc, macOS).
#[cfg(unix)]
#[test]
fn explicitly_allowed_internal_hosts_still_work() {
    let server = http_server(|_| HttpResponse::new(200, "hi"));
    let port = server.addr.port();
    let src = format!(
        r#"
import http
import net
print(http.get("http://127.0.0.1:{port}/").map(r => r.text().unwrap()))
print(http.get("http://localhost:{port}/").map(r => r.text().unwrap()))
print(http.get("http://127.1:{port}/").map(r => r.text().unwrap()))
print(net.connect("127.0.0.1:{port}").is_ok())
"#
    );
    // `127.1` is allowed by name only through `*`; its address by the explicit IP rule.
    let policy = RulePolicy::new().allow_net("*").allow_net(&format!("127.0.0.1:{port}")).allow_net("localhost").deny_private_networks();
    let r = run_with(&src, |c| c.policy(Policy::rules(policy)));
    assert_eq!(r.ok_stdout(), "Ok(\"hi\")\nOk(\"hi\")\nOk(\"hi\")\ntrue\n");
}

// Relies on the resolver accepting `127.1` for 127.0.0.1 (glibc, macOS).
#[cfg(unix)]
#[test]
fn redirects_to_private_addresses_are_denied() {
    let target = http_server(|_| HttpResponse::new(200, "secret"));
    let tport = target.addr.port();
    let origin = http_server(move |req| match req.path() {
        "/name" => HttpResponse::new(302, "").header("location", &format!("http://127.1:{tport}/")),
        _ => HttpResponse::new(302, "").header("location", &format!("http://localhost:{tport}/")),
    });
    // The origin is allowed explicitly; the redirect targets only by the wildcard.
    let policy = RulePolicy::new().allow_net("*").allow_net(&origin.addr.to_string()).deny_private_networks();
    let src = format!(
        "import http\nprint(http.get(\"http://{0}/name\").unwrap_err().kind)\nprint(http.get(\"http://{0}/local\").unwrap_err().kind)",
        origin.addr
    );
    let r = run_with(&src, |c| c.policy(Policy::rules(policy)));
    assert_eq!(r.ok_stdout(), "permission_denied\npermission_denied\n");
    assert_eq!(origin.requests.lock().unwrap().len(), 2);
    assert!(target.requests.lock().unwrap().is_empty());
}

#[test]
fn custom_policies_see_resolved_addresses_without_extra_prompts() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let prompts = Arc::new(Mutex::new(Vec::<String>::new()));
    let (seen2, prompts2) = (seen.clone(), prompts.clone());
    // Asks about everything; denies one resolved address.
    let policy = Policy::custom(move |cap: &Capability| {
        seen2.lock().unwrap().push(cap.to_string());
        match cap {
            Capability::NetAddr { addr, .. } if addr.ip().is_ipv6() => Decision::Deny,
            Capability::Stdout | Capability::Stderr => Decision::Allow,
            _ => Decision::Prompt,
        }
    });
    let src =
        format!("import net\nprint(net.connect(\"127.0.0.1:{port}\").is_ok())\nprint(net.connect(\"[::1]:{port}\").unwrap_err().message)");
    let r = run_with(&src, |c| {
        c.policy(policy).prompter(move |cap: Capability| {
            prompts2.lock().unwrap().push(cap.to_string());
            async { PromptAnswer::AllowOnce }
        })
    });
    assert_eq!(
        r.ok_stdout(),
        format!("true\npermission denied: connect to ::1:{port}: address ::1 (private or internal) is not allowed\n")
    );
    // One prompt per connection: the `Prompt` answer for the resolved address counts as `Allow`.
    assert_eq!(*prompts.lock().unwrap(), vec![format!("connect to 127.0.0.1:{port}"), format!("connect to ::1:{port}")]);
    let seen = seen.lock().unwrap();
    assert!(seen.contains(&format!("connect to 127.0.0.1:{port}")), "{seen:?}");
    assert!(seen.contains(&format!("connect to [::1]:{port}")), "{seen:?}");
}

#[test]
fn net_addr_capability_display() {
    let addr: SocketAddr = "93.184.215.14:443".parse().unwrap();
    assert_eq!(Capability::NetAddr { host: "example.com".into(), addr }.to_string(), "connect to example.com at 93.184.215.14:443");
    assert_eq!(Capability::NetAddr { host: "93.184.215.14".into(), addr }.to_string(), "connect to 93.184.215.14:443");
}

// Relies on the resolver accepting `127.1` for 127.0.0.1 (glibc, macOS).
#[cfg(unix)]
#[test]
fn cli_denies_private_networks_by_default() {
    let server = http_server(|_| HttpResponse::new(200, "hi"));
    let dir = TempDir::new("cli-net");
    let script = dir
        .write("main.sand", &format!("import http\nprint(http.get(\"http://127.1:{}/\").map(r => r.text().unwrap()))", server.addr.port()));
    let run = |flags: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_sand")).arg("run").arg(&script).args(flags).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(run(&["--allow-net=*"]).starts_with("Err(Error { kind: \"permission_denied\""));
    assert_eq!(run(&["--allow-net=*", "--allow-private-net"]), "Ok(\"hi\")\n");
    assert_eq!(run(&["--allow-net=*,127.0.0.0/8"]), "Ok(\"hi\")\n");
    assert!(
        run(&["--allow-net=*", "--allow-private-net", "--deny-net=127.0.0.0/8"]).starts_with("Err(Error { kind: \"permission_denied\"")
    );
}

// ---------------------------------------------------------------------------
// File system: symlinks, check-then-use races and subtree deny rules
// ---------------------------------------------------------------------------

fn rules(dir: &TempDir, policy: RulePolicy) -> impl FnOnce(sandlang::Config) -> sandlang::Config {
    let cwd = dir.path().to_path_buf();
    move |c| c.policy(Policy::rules(policy)).cwd(cwd)
}

#[cfg(unix)]
#[test]
fn entry_checks_do_not_resolve_a_final_symlink() {
    let dir = TempDir::new("entry-policy");
    dir.write("outside/target.txt", "t");
    std::fs::create_dir_all(dir.path().join("allowed")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside/target.txt"), dir.path().join("allowed/link")).unwrap();
    let link = Capability::FsWrite(dir.path().join("allowed/link"));
    let p = Policy::rules(RulePolicy::new().allow_write(dir.path().join("allowed")));
    assert_eq!(p.check(&link), Decision::Deny);
    assert_eq!(p.check_entry(&link), Decision::Allow);
    // Symlinks in the directories leading to the entry are still resolved.
    std::os::unix::fs::symlink(dir.path().join("outside"), dir.path().join("allowed/dir_link")).unwrap();
    assert_eq!(p.check_entry(&Capability::FsWrite(dir.path().join("allowed/dir_link/target.txt"))), Decision::Deny);
    assert_eq!(Policy::allow_all().check_entry(&link), Decision::Allow);
}

#[test]
fn denies_under_finds_deny_rules_inside_a_tree() {
    let dir = TempDir::new("denies-under");
    let d = dir.path();
    let p = Policy::rules(RulePolicy::new().allow_write(d).deny_read(d.join("a/b/secret")).deny_write(d.join("c/locked")));
    assert!(p.denies_under(d));
    assert!(p.denies_under(&d.join("a")));
    assert!(p.denies_under(&d.join("a/b/secret")));
    assert!(p.denies_under(&d.join("c")));
    assert!(!p.denies_under(&d.join("a/other")));
    assert!(!p.denies_under(&d.join("ab")));
    assert!(!Policy::allow_all().denies_under(d));
    assert!(!Policy::custom(|_: &Capability| Decision::Deny).denies_under(d));
}

#[cfg(unix)]
#[test]
fn dangling_symlinks_cannot_create_files_outside() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("dangling");
    std::fs::create_dir_all(dir.path().join("allowed")).unwrap();
    std::fs::create_dir_all(dir.path().join("outside")).unwrap();
    symlink(dir.path().join("outside/new.txt"), dir.path().join("allowed/dangling")).unwrap();
    symlink(dir.path().join("outside/sub"), dir.path().join("allowed/dangling_dir")).unwrap();
    dir.write("allowed/real.txt", "real");
    let src = r#"
import fs
print(fs.write_text("allowed/dangling", "x").unwrap_err().kind)
print(fs.append_text("allowed/dangling", "x").unwrap_err().kind)
print(fs.write_bytes("allowed/dangling", b"x").unwrap_err().message.contains("symbolic link"))
print(fs.copy("allowed/real.txt", "allowed/dangling").unwrap_err().kind)
print(fs.create_dir("allowed/dangling_dir/a", recursive: true).unwrap_err().kind)
print(fs.read_text("allowed/dangling").unwrap_err().kind)
print(fs.exists("allowed/dangling"))
print(fs.metadata("allowed/dangling").unwrap_err().kind)
fs.remove_file("allowed/dangling")?
print(fs.list_dir("allowed")?)
"#;
    let r = run_with(src, rules(&dir, RulePolicy::new().allow_write(dir.path().join("allowed"))));
    assert_eq!(
        r.ok_stdout(),
        "permission_denied\npermission_denied\ntrue\npermission_denied\npermission_denied\npermission_denied\nOk(false)\npermission_denied\n[\"dangling_dir\", \"real.txt\"]\n"
    );
    assert_eq!(std::fs::read_dir(dir.path().join("outside")).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn symlink_entries_are_checked_as_themselves() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("entries");
    dir.write("outside/target.txt", "target");
    dir.write("outside/dir/keep.txt", "keep");
    dir.write("allowed/tree/file.txt", "f");
    symlink(dir.path().join("outside/target.txt"), dir.path().join("allowed/link.txt")).unwrap();
    symlink(dir.path().join("outside/dir"), dir.path().join("allowed/tree/dir_link")).unwrap();
    let src = r#"
import fs
// Following the link needs permission for the target...
print(fs.read_text("allowed/link.txt").unwrap_err().kind)
print(fs.write_text("allowed/link.txt", "x").unwrap_err().kind)
print(fs.list_dir("allowed/tree/dir_link").unwrap_err().kind)
// ...but the link itself is an entry of the allowed directory.
fs.rename("allowed/link.txt", "allowed/renamed.txt")?
fs.remove_file("allowed/renamed.txt")?
fs.remove_dir("allowed/tree", recursive: true)?
print(fs.list_dir("allowed")?)
"#;
    let r = run_with(src, rules(&dir, RulePolicy::new().allow_write(dir.path().join("allowed"))));
    assert_eq!(r.ok_stdout(), "permission_denied\npermission_denied\npermission_denied\n[]\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("outside/target.txt")).unwrap(), "target");
    assert!(dir.path().join("outside/dir/keep.txt").exists());
}

#[test]
fn renaming_or_removing_trees_respects_deny_rules_inside() {
    let dir = TempDir::new("subtree");
    dir.write("data/a/secret/key.txt", "key");
    dir.write("data/a/public.txt", "pub");
    dir.write("data/b/notes.txt", "notes");
    dir.write("data/c/x.txt", "x");
    let data = dir.path().join("data");
    let policy = RulePolicy::new().allow_write(&data).deny_read(data.join("a/secret")).deny_write(data.join("locked/inner"));
    let src = r#"
import fs
print(fs.read_text("data/a/secret/key.txt").unwrap_err().kind)
// Moving `a` would move the protected `a/secret` out from under its rule.
print(fs.rename("data/a", "data/moved").unwrap_err().message)
print(fs.rename("data", "elsewhere").unwrap_err().kind)
// Removing `a` recursively fails before removing anything.
print(fs.remove_dir("data/a", recursive: true).unwrap_err().kind)
print(fs.exists("data/a/public.txt")?)
// A destination with protected paths below it cannot be replaced either.
print(fs.rename("data/b", "data/locked").unwrap_err().kind)
// Unrelated trees are fine.
fs.rename("data/b", "data/b2")?
fs.remove_dir("data/c", recursive: true)?
print(fs.list_dir("data")?)
"#;
    let r = run_with(src, rules(&dir, policy));
    assert_eq!(
        r.ok_stdout(),
        "permission_denied\npermission denied: data/a contains paths protected by the policy\npermission_denied\npermission_denied\ntrue\npermission_denied\n[\"a\", \"b2\"]\n"
    );
    assert!(dir.path().join("data/a/secret/key.txt").exists());

    // Custom policies decide on the top-level paths alone.
    let custom = Policy::custom(|_: &Capability| Decision::Allow);
    let cwd = dir.path().to_path_buf();
    let src = "import fs\nfs.rename(\"data/a\", \"data/moved\")?\nprint(fs.exists(\"data/moved/secret/key.txt\")?)";
    let r = run_with(src, move |c| c.policy(custom).cwd(cwd));
    assert_eq!(r.ok_stdout(), "true\n");
}

#[cfg(unix)]
#[test]
fn special_files_are_not_read_or_written() {
    let dir = TempDir::new("fifo");
    let status = std::process::Command::new("mkfifo").arg(dir.path().join("pipe")).status().unwrap();
    assert!(status.success());
    dir.write("sub/file.txt", "x");
    let src = r#"
import fs
print(fs.read_text("pipe").unwrap_err().kind)
print(fs.write_text("pipe", "x").unwrap_err().kind)
print(fs.read_bytes("sub").unwrap_err().kind)
print(fs.metadata("pipe")?.is_file)
"#;
    let r = run_with(src, rules(&dir, RulePolicy::new().allow_write(dir.path())));
    // Opening a FIFO without a reader for writing fails at once (ENXIO).
    assert_eq!(r.ok_stdout(), "invalid\nio\ninvalid\nfalse\n");
}

/// Junctions (which, unlike symlinks, need no privilege to create on
/// Windows) are resolved by the policy check and never followed by the
/// operation; entry operations act on the junction itself.
#[cfg(windows)]
#[test]
fn junctions_are_checked_like_symlinks() {
    let dir = TempDir::new("junctions");
    dir.write("outside/dir/keep.txt", "keep");
    dir.write("allowed/real.txt", "real");
    let (link, target) = (dir.path().join("allowed").join("link"), dir.path().join("outside").join("dir"));
    junction::create(&target, &link).unwrap();
    let src = r#"
import fs
print(fs.read_text("allowed/link/keep.txt").unwrap_err().kind)
print(fs.list_dir("allowed/link").unwrap_err().kind)
print(fs.write_text("allowed/link/new.txt", "x").unwrap_err().kind)
fs.rename("allowed/link", "allowed/moved")?
fs.remove_file("allowed/moved")?
print(fs.list_dir("allowed")?)
"#;
    let r = run_with(src, rules(&dir, RulePolicy::new().allow_write(dir.path().join("allowed"))));
    assert_eq!(r.ok_stdout(), "permission_denied\npermission_denied\npermission_denied\n[\"real.txt\"]\n");
    assert_eq!(std::fs::read_to_string(target.join("keep.txt")).unwrap(), "keep");
    assert!(!target.join("new.txt").exists());
}

/// A script swaps a directory with a symlink to a forbidden directory while
/// other tasks read through it: whatever the interleaving, no read may see
/// the forbidden file (it is refused by the policy check or by the no-follow
/// open). With path-based opens, this leaks the file dozens of times.
#[cfg(unix)]
#[test]
fn symlink_swaps_cannot_bypass_the_policy() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("swap");
    dir.write("allowed/x/file.txt", "public");
    dir.write("outside/file.txt", "SECRET");
    symlink(dir.path().join("outside"), dir.path().join("allowed/link")).unwrap();
    let src = r#"
import fs
let swapper = spawn {
    for _ in 0..100 {
        fs.rename("x", "tmp").unwrap()
        fs.rename("link", "x").unwrap()
        task.yield_now()
        fs.rename("x", "link").unwrap()
        fs.rename("tmp", "x").unwrap()
    }
}
let readers = (0..4).map(_ => spawn {
    var seen: [str: int] = [:]
    while not swapper.is_done() {
        let key = match fs.read_text("x/file.txt") {
            Ok(text) => text
            Err(e) => e.kind
        }
        seen[key] = (seen.get(key) ?? 0) + 1
        // Policy denials do not suspend; let the swapper run.
        task.yield_now()
    }
    seen
})
var secret = 0
var public = 0
for seen in task.all(readers) {
    secret += seen.get("SECRET") ?? 0
    public += seen.get("public") ?? 0
}
print("secret reads: {secret}")
print(public > 0)
"#;
    let cwd = dir.path().join("allowed");
    let policy = RulePolicy::new().allow_write(&cwd);
    let r = run_with(src, move |c| c.policy(Policy::rules(policy)).cwd(cwd));
    assert_eq!(r.ok_stdout(), "secret reads: 0\ntrue\n");
}

// ---------------------------------------------------------------------------
// Memory accounting of pending I/O
// ---------------------------------------------------------------------------

fn limited(bytes: usize) -> impl FnOnce(sandlang::Config) -> sandlang::Config {
    // The wall time limit turns a regression (buffers pending forever) into a failure.
    let limits = Limits { max_memory: Some(bytes), max_wall_time: Some(Duration::from_secs(10)), ..Limits::default() };
    move |c| c.policy(Policy::allow_all()).limits(limits)
}

/// An HTTP server whose responses send their head and the first `sent`
/// bytes of a `len`-byte body, then stall for `stall` before the rest.
/// With `content_length: false` the body is delimited by closing the
/// connection.
fn stalling_server(len: usize, sent: usize, stall: Duration, content_length: bool) -> SocketAddr {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let head = if content_length {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n")
                } else {
                    "HTTP/1.1 200 OK\r\n\r\n".into()
                };
                let body = vec![b'x'; len];
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body[..sent]);
                std::thread::sleep(stall);
                let _ = stream.write_all(&body[sent..]);
            });
        }
    });
    addr
}

#[test]
fn pending_http_bodies_count_against_the_memory_limit() {
    for content_length in [true, false] {
        // Ten 3 MB downloads that all stay pending for 5 s cannot fit in 16 MB:
        // some fail with kind "limit" while the bodies are still arriving.
        let addr = stalling_server(3 << 20, 2 << 20, Duration::from_secs(5), content_length);
        let src = format!(
            r#"
import http
let results: Channel<str> = task.channel()
for _ in 0..10 {{
    let _ = spawn {{
        let r = http.get("http://{addr}/", timeout: 20s)
        results.send(match r {{
            Ok(_) => "ok"
            Err(e) => e.kind
        }})
    }}
}}
print(results.recv())
"#
        );
        let start = Instant::now();
        let r = run_with(&src, limited(16 << 20));
        assert_eq!(r.ok_stdout(), "Some(\"limit\")\n", "content_length: {content_length}");
        assert!(start.elapsed() < Duration::from_secs(4), "content_length: {content_length}, took {:?}", start.elapsed());
    }
}

#[test]
fn pending_socket_buffers_count_against_the_memory_limit() {
    // Each pending read holds its buffer (up to 1 MiB); 64 of them exceed 32 MB.
    // (The connections are opened first: their own buffers count too.)
    let src = r#"
import net
let listener = net.listen("127.0.0.1:0")?
let addr = listener.addr()
var conns = []
for _ in 0..64 {
    conns.push((net.connect(addr)?, listener.accept()?))
}
let readers = conns.map(p => spawn p.0.read(max: 1048576))
task.sleep(100ms)
// Reads whose buffer did not fit are done (failed); the others still wait.
let failed = readers.filter(t => t.is_done()).map(t => t.wait().unwrap_err().kind)
print(failed.len() > 16 and failed.len() < 64)
print(failed.all(k => k == "limit"))
"#;
    assert_eq!(run_with(src, limited(32 << 20)).ok_stdout(), "true\ntrue\n");

    // Copies of data being written are held until the peer takes them.
    let src = r#"
import net
let listener = net.listen("127.0.0.1:0")?
let c = net.connect(listener.addr())?
let peer = listener.accept()?
let data = "x".repeat(2000000).bytes()
let writers = (0..20).map(_ => spawn c.write(data))
task.sleep(100ms)
// The socket may take a whole write; the others hold their copies.
let done = writers.filter(t => t.is_done()).map(t => t.wait())
let failed = done.filter(r => r.is_err()).map(r => r.unwrap_err().kind)
print(failed.len() > 5)
print(failed.all(k => k == "limit"))
"#;
    assert_eq!(run_with(src, limited(24 << 20)).ok_stdout(), "true\ntrue\n");
}

#[test]
fn hostile_peers_cannot_end_a_server_run() {
    // A request that claims a huge body, and a line that never ends, fail
    // the operation (kind "limit") without ending the run.
    let src = r#"
import http
import net
let server = http.serve("127.0.0.1:0")?
let raw = net.listen("127.0.0.1:0")?
print("ready {server.addr()} {raw.addr()}")
let req = server.accept()?
req.respond(200, b"served after the hostile request")?
let line = raw.accept()?.read_line()
print(line.unwrap_err().kind)
print("still running")
"#;
    let out = sandlang::MemorySink::new();
    let limits = Limits { max_memory: Some(8 << 20), max_wall_time: Some(Duration::from_secs(20)), ..Limits::default() };
    let config = sandlang::Config::new().policy(Policy::allow_all()).limits(limits).stdout(out.clone());
    let mut engine = sandlang::Engine::new(config);
    let handle = std::thread::spawn(move || common::rt().block_on(engine.run_source("server.sand", src)));
    let start = Instant::now();
    let (http_addr, raw_addr) = loop {
        if let Some(l) = out.contents().lines().next() {
            let mut parts = l.split(' ').skip(1);
            break (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    };
    use std::io::{Read, Write};
    // Claims 60 MB and sends more than fits: the request fails (and is
    // answered with 400, which may be lost to a reset since the server
    // closes the connection with input unread).
    let mut hostile = std::net::TcpStream::connect(&http_addr).unwrap();
    write!(hostile, "POST / HTTP/1.1\r\nContent-Length: 60000000\r\n\r\n").unwrap();
    let _ = hostile.write_all(&vec![b'a'; 12 << 20]);
    let _ = hostile.read_to_end(&mut Vec::new());
    let mut ok = std::net::TcpStream::connect(&http_addr).unwrap();
    write!(ok, "GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    ok.read_to_string(&mut resp).unwrap();
    assert!(resp.ends_with("served after the hostile request"), "{resp}");
    // A 10 MB line without a newline.
    let mut liner = std::net::TcpStream::connect(&raw_addr).unwrap();
    let _ = liner.write_all(&vec![b'b'; 10 << 20]);
    handle.join().unwrap().unwrap();
    assert!(out.contents().ends_with("limit\nstill running\n"), "{}", out.contents());
}

#[test]
fn completed_io_releases_its_reservations() {
    let server = http_server(|_| HttpResponse::new(200, vec![b'y'; 1 << 20]));
    let dir = TempDir::new("release");
    dir.write("big.txt", &"z".repeat(1 << 20));
    let src = format!(
        r#"
import fs
import http
import net
var total = 0
for _ in 0..60 {{
    total += fs.read_bytes("big.txt")?.len()
    total += http.get("http://{}/")?.body.len()
    fs.write_text("copy.txt", fs.read_text("big.txt")?)?
}}
let listener = net.listen("127.0.0.1:0")?
let c = net.connect(listener.addr())?
let peer = listener.accept()?
for _ in 0..60 {{
    peer.write("x".repeat(100000).bytes())?
    var got = 0
    while got < 100000 {{
        got += c.read(max: 1048576)?.len()
    }}
    total += got
}}
print(total)
"#,
        server.addr
    );
    // 60 MB pass through I/O buffers; leaked reservations would exceed 16 MB.
    let cwd = dir.path().to_path_buf();
    let r = run_with(&src, move |c| limited(16 << 20)(c).cwd(cwd));
    assert_eq!(r.ok_stdout(), format!("{}\n", 120 * (1 << 20) + 60 * 100000));
}

#[test]
fn oversized_files_and_input_fail_before_being_read() {
    let dir = TempDir::new("bigfile");
    dir.write("big.txt", &"z".repeat(10 << 20));
    let cwd = dir.path().to_path_buf();
    let src = "import fs\nlet e = fs.read_text(\"big.txt\").unwrap_err()\nprint(e.kind)\nprint(e.message)";
    let r = run_with(src, move |c| limited(8 << 20)(c).cwd(cwd));
    assert_eq!(r.ok_stdout(), "limit\nbig.txt: the data does not fit in the memory limit\n");
    let r = run_with("import io\nprint(io.read_all().unwrap_err().kind)", |c| limited(8 << 20)(c).stdin_bytes(vec![b'a'; 10 << 20]));
    assert_eq!(r.ok_stdout(), "limit\n");
    let r = run_with("import io\nprint(io.read_line().unwrap_err().kind)", |c| limited(8 << 20)(c).stdin_bytes(vec![b'a'; 10 << 20]));
    assert_eq!(r.ok_stdout(), "limit\n");
    let r =
        run_with("import io\nprint(io.read_line()?)\nprint(io.read_all()?)", |c| limited(8 << 20)(c).stdin_bytes(b"one\r\ntwo\n".to_vec()));
    assert_eq!(r.ok_stdout(), "Some(\"one\")\ntwo\n\n");
}

/// File systems may be case insensitive: a deny rule for a path that does
/// not exist yet must not be bypassed by spelling it differently (on every
/// platform, since Linux has case-insensitive directories and mounts too).
#[test]
fn deny_rules_ignore_case() {
    let dir = TempDir::new("deny-case");
    let d = dir.path();
    let p = Policy::rules(RulePolicy::new().allow_write(d).deny_read(d.join("future/secret")));
    assert_eq!(p.check(&Capability::FsRead(d.join("future/Secret/x"))), Decision::Deny);
    assert_eq!(p.check(&Capability::FsWrite(d.join("FUTURE/secret/x"))), Decision::Deny);
    assert_eq!(p.check(&Capability::FsWrite(d.join("future/other"))), Decision::Allow);
    assert!(p.denies_under(&d.join("Future")));
}

/// Unicode spellings a case- and normalisation-insensitive file system
/// (APFS) treats as the same name: `ſ`/`s`, `ς`/`σ`, NFC/NFD.
#[test]
fn deny_rules_match_unicode_equivalent_spellings() {
    let dir = TempDir::new("deny-unicode");
    let d = dir.path();
    let p = Policy::rules(
        RulePolicy::new().allow_write(d).deny_read(d.join("future/secrets")).deny_write(d.join("café.json")).deny_write(d.join("λόγος")),
    );
    for denied in ["future/ſecrets/x", "FUTURE/SECRETS", "cafe\u{301}.json", "CAFÉ.JSON", "λόγοσ", "future/se\u{200b}crets"] {
        assert_eq!(p.check(&Capability::FsWrite(d.join(denied))), Decision::Deny, "{denied}");
        assert_eq!(p.check_entry(&Capability::FsWrite(d.join(denied))), Decision::Deny, "{denied}");
    }
    assert!(p.denies_under(&d.join("futuſe").with_file_name("future")));
    assert!(p.denies_under(&d.join("FUTURE")));
    assert_eq!(p.check(&Capability::FsWrite(d.join("cafe.json"))), Decision::Allow);
    assert_eq!(p.check(&Capability::FsWrite(d.join("future/secret"))), Decision::Allow);
}

/// The reported bypasses on APFS: renaming or removing protected entries,
/// and squatting protected paths before they exist, through alternative
/// spellings.
#[cfg(target_os = "macos")]
#[test]
fn unicode_spellings_cannot_bypass_deny_rules_on_apfs() {
    let dir = TempDir::new("apfs-deny");
    dir.write("data/secrets/key.txt", "key");
    dir.write("data/config.json", "{}");
    let data = dir.path().join("data");
    let policy = || {
        RulePolicy::new()
            .allow_write(&data)
            .deny_read(data.join("secrets"))
            .deny_write(data.join("config.json"))
            .deny_write(data.join("future"))
            .deny_write(data.join("future.json"))
            .deny_write(data.join("café.txt"))
    };
    let src = r#"
import fs
print(fs.rename("data/ſecrets", "data/stolen").unwrap_err().kind)
print(fs.read_text("data/ſecrets/key.txt").unwrap_err().kind)
print(fs.remove_file("data/config.jſon").unwrap_err().kind)
print(fs.rename("data/config.jſon", "data/x.json").unwrap_err().kind)
print(fs.remove_dir("data/SECRETS", recursive: true).unwrap_err().kind)
print(fs.write_text("data/future.jſon", "evil").unwrap_err().kind)
print(fs.create_dir("data/FUTURE").unwrap_err().kind)
print(fs.create_dir("data/ſuture".replace("ſ", "f")).unwrap_err().kind)
print(fs.write_text("data/cafe\u{301}.txt", "evil").unwrap_err().kind)
print(fs.list_dir("data")?)
"#;
    let r = run_with(src, rules(&dir, policy()));
    assert_eq!(r.ok_stdout(), format!("{}[\"config.json\", \"secrets\"]\n", "permission_denied\n".repeat(9)));
    assert!(dir.path().join("data/secrets/key.txt").exists() && dir.path().join("data/config.json").exists());

    // Existing entries are named as stored: the policy sees `secrets`.
    let p = Policy::rules(policy());
    let stored = sandlang::policy::normalize_entry_path(&data.join("ſecrets"));
    assert_eq!(stored, data.join("secrets"));
    assert_eq!(p.check_entry(&Capability::FsWrite(data.join("CONFIG.JSON"))), Decision::Deny);
}

/// A symlink's own name (which cannot be canonicalised without following
/// the link) is named as stored too, so allow rules, which compare exactly,
/// see the same path for every spelling the file system accepts.
#[cfg(target_os = "macos")]
#[test]
fn symlink_entries_are_named_as_stored() {
    use sandlang::policy::normalize_entry_path;
    use std::os::unix::fs::symlink;
    let dir = TempDir::new("link-names");
    dir.write("outside/target.txt", "target");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    symlink(dir.path().join("outside/target.txt"), data.join("Link.txt")).unwrap();
    symlink(dir.path().join("outside"), data.join("caf\u{e9}-link")).unwrap();
    for (given, stored) in [
        ("link.TXT", "Link.txt"),
        ("Link.txt", "Link.txt"),
        ("cafe\u{301}-link", "caf\u{e9}-link"),
        ("CAF\u{c9}-LINK", "caf\u{e9}-link"),
        ("missing.TXT", "missing.TXT"),
    ] {
        assert_eq!(normalize_entry_path(&data.join(given)), data.join(stored), "{given}");
    }
    // A rule naming a path that later becomes a symlink.
    let p = Policy::rules(RulePolicy::new().allow_write(data.join("Later")));
    symlink(dir.path().join("outside/target.txt"), data.join("Later")).unwrap();
    assert_eq!(p.check_entry(&Capability::FsWrite(data.join("LATER"))), Decision::Allow);

    // Capabilities (seen by custom policies and prompts, and remembered by
    // "always" answers) name the link as stored. Only the link and the new
    // name may be written here.
    let allowed = [
        Capability::Stdout,
        Capability::FsRead(data.clone()),
        Capability::FsWrite(data.join("Link.txt")),
        Capability::FsWrite(data.join("renamed.txt")),
    ];
    let policy = Policy::custom(move |cap: &Capability| if allowed.contains(cap) { Decision::Allow } else { Decision::Deny });
    let src = r#"
import fs
fs.rename("data/LINK.TXT", "data/renamed.txt")?
print(fs.list_dir("data")?)
print(fs.remove_file("data/CAFE\u{301}-LINK").unwrap_err().kind)
"#;
    let cwd = dir.path().to_path_buf();
    let r = run_with(src, move |c| c.policy(policy).cwd(cwd));
    assert_eq!(r.ok_stdout(), "[\"Later\", \"caf\u{e9}-link\", \"renamed.txt\"]\npermission_denied\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("outside/target.txt")).unwrap(), "target");
}

/// On a case-sensitive file system, differently cased symlinks are distinct
/// entries and keep their names.
#[cfg(target_os = "linux")]
#[test]
fn symlink_entries_keep_their_exact_names() {
    use sandlang::policy::normalize_entry_path;
    let dir = TempDir::new("link-cases");
    for name in ["link", "LINK"] {
        std::os::unix::fs::symlink("/nowhere", dir.path().join(name)).unwrap();
        assert_eq!(normalize_entry_path(&dir.path().join(name)), dir.path().join(name));
    }
    assert_eq!(normalize_entry_path(&dir.path().join("Link")), dir.path().join("Link"));
}

#[test]
fn denial_messages_use_the_scripts_spelling() {
    let dir = TempDir::new("deny-message");
    dir.write("data/private/key.txt", "key");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("data/private"), dir.path().join("data/link")).unwrap();
    let policy = RulePolicy::new().allow_read(dir.path().join("data/public"));
    let src = r#"
import fs
print(fs.read_text("data/../data/private/key.txt").unwrap_err().message)
print(fs.write_text("data/x.txt", "x").unwrap_err().message)
print(fs.remove_file("data/x.txt").unwrap_err().message)
"#;
    let r = run_with(src, rules(&dir, policy.clone()));
    assert_eq!(
        r.ok_stdout(),
        "permission denied: read data/../data/private/key.txt\npermission denied: write data/x.txt\npermission denied: write data/x.txt\n"
    );
    // A symlink's target is not revealed.
    #[cfg(unix)]
    {
        let r = run_with("import fs\nprint(fs.read_text(\"data/link/key.txt\").unwrap_err().message)", rules(&dir, policy));
        assert_eq!(r.ok_stdout(), "permission denied: read data/link/key.txt\n");
    }
}

#[test]
fn overlong_paths_are_refused_quickly() {
    let dir = TempDir::new("long-paths");
    let src = r#"
import fs
let p = "a/".repeat(400000) + "b"
print(fs.exists(p).unwrap_err().kind)
print(fs.remove_file(p).unwrap_err().kind)
print(fs.read_text("a/".repeat(200) + "b").unwrap_err().kind)
print(fs.exists("x\u{0}y").unwrap_err().kind)
"#;
    let start = Instant::now();
    let r = run_with(src, rules(&dir, RulePolicy::new().allow_write(dir.path())));
    assert_eq!(r.ok_stdout(), "invalid\ninvalid\nnot_found\ninvalid\n");
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
}

#[test]
fn recursive_create_dir_checks_every_created_directory() {
    let dir = TempDir::new("mkdir-parents");
    // Only `out/data` may be written, and `out` does not exist yet.
    let allowed = dir.path().join("out/data");
    let src = r#"
import fs
print(fs.create_dir("out/data/x", recursive: true).unwrap_err().kind)
print(fs.exists("out")?)
"#;
    let policy = RulePolicy::new().allow_write(&allowed).allow_read(dir.path());
    let r = run_with(src, rules(&dir, policy.clone()));
    assert_eq!(r.ok_stdout(), "permission_denied\nfalse\n");
    std::fs::create_dir(dir.path().join("out")).unwrap();
    let r =
        run_with("import fs\nfs.create_dir(\"out/data/x/y\", recursive: true)?\nprint(fs.is_dir(\"out/data/x/y\")?)", rules(&dir, policy));
    assert_eq!(r.ok_stdout(), "true\n");
}

#[cfg(unix)]
#[test]
fn copies_do_not_keep_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("copy-mode");
    let tool = dir.write("tool.sh", "#!/bin/sh\n");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o4755)).unwrap();
    let r = run_with(
        "import fs\nfs.copy(\"tool.sh\", \"copy.sh\")?\nprint(fs.read_text(\"copy.sh\")?)",
        rules(&dir, RulePolicy::new().allow_write(dir.path())),
    );
    assert_eq!(r.ok_stdout(), "#!/bin/sh\n\n");
    let mode = std::fs::metadata(dir.path().join("copy.sh")).unwrap().permissions().mode();
    assert_eq!(mode & 0o7111, 0, "{mode:o}");
}

#[test]
fn path_helpers_are_metered() {
    let src = "import fs\nlet s = \"x\".repeat(1 << 20)\nlet parts = (0..2000).map(_ => s)\nprint(fs.join(parts).len())";
    let r = run_with(src, |c| c.limits(Limits { max_memory: Some(64 << 20), ..Limits::default() }));
    assert!(matches!(r.result, Err(sandlang::Error::Limit(sandlang::LimitKind::Memory))), "{:?}", r.result);
    let src = "import fs\nlet s = \"x/\".repeat(1 << 20)\nvar n = 0\nfor i in 0..100000 { n += fs.basename(s).len() + fs.dirname(s).len() }\nprint(n)";
    let r = run_with(src, |c| c.limits(Limits { max_instructions: Some(5_000_000), ..Limits::default() }));
    assert!(matches!(r.result, Err(sandlang::Error::Limit(sandlang::LimitKind::Instructions))), "{:?}", r.result);
}
