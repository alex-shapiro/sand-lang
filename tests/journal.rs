//! Durable execution: record a run in a journal, replay it, and resume it
//! from any prefix of the journal (SPEC §16.7).

use sandlang::journal::{Durable, Entry, EntryKind, HttpOutcome, Journal, MemoryJournal, OpKind, Suspension, operations};
use sandlang::native::{Effectful, Journaled, JournaledCall, Later, Park};
use sandlang::network::{Listener, Network, NetworkFuture, Stream};
use sandlang::platform::{Clock, ClockFuture, Instant, SystemClock, SystemTime, UNIX_EPOCH};
use sandlang::vm::Vm;
use sandlang::{Config, Engine, Error, HostError, MemorySink, NativeModule, Outcome, RulePolicy};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

// ---------------------------------------------------------------------------
// Clocks

const BASE_SECS: u64 = 1_800_000_000;

/// A clock whose time never moves and whose timers expire at once: runs
/// under it are deterministic.
struct FrozenClock(Instant);

impl Clock for FrozenClock {
    fn wall(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(BASE_SECS)
    }
    fn now(&self) -> Instant {
        self.0
    }
    fn cpu_time(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, _: Duration) -> ClockFuture {
        Box::pin(std::future::ready(()))
    }
    fn yield_now(&self) -> ClockFuture {
        Box::pin(std::future::ready(()))
    }
}

/// A clock that moves only when the test moves it. A timer is pending
/// until the time passes its expiry.
struct ManualClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl ManualClock {
    fn new() -> Arc<ManualClock> {
        Arc::new(ManualClock { base: Instant::now(), offset: Mutex::new(Duration::ZERO) })
    }
    fn advance(&self, d: Duration) {
        *self.offset.lock().unwrap() += d;
    }
    fn offset(&self) -> Duration {
        *self.offset.lock().unwrap()
    }
}

impl Clock for ManualClock {
    fn wall(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(BASE_SECS) + self.offset()
    }
    fn now(&self) -> Instant {
        self.base + self.offset()
    }
    fn cpu_time(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, d: Duration) -> ClockFuture {
        if d.is_zero() {
            return Box::pin(std::future::ready(()));
        }
        Box::pin(std::future::pending())
    }
    fn yield_now(&self) -> ClockFuture {
        Box::pin(std::future::ready(()))
    }
}

// ---------------------------------------------------------------------------
// An in-process HTTP network

/// Serves every connection in memory. The response to a request depends
/// only on the request. With `delay`, responses arrive after a short,
/// varying wait on another thread, so the point where the engine delivers
/// them varies between runs.
#[derive(Clone)]
struct FakeNet(Arc<NetShared>);

struct NetShared {
    requests: AtomicUsize,
    delay: bool,
    jitter: AtomicU64,
}

impl FakeNet {
    fn new(delay: bool) -> FakeNet {
        FakeNet(Arc::new(NetShared { requests: AtomicUsize::new(0), delay, jitter: AtomicU64::new(7) }))
    }
    fn requests(&self) -> usize {
        self.0.requests.load(Ordering::SeqCst)
    }
}

struct FakeConn {
    net: Arc<NetShared>,
    request: Vec<u8>,
    response: Option<Vec<u8>>,
    sent: usize,
    ready: Arc<Mutex<(bool, Option<Waker>)>>,
}

/// The response to the request in `req`, once it is complete.
fn respond(req: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(req);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let len = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    if body.len() < len {
        return None;
    }
    let start = head.lines().next().unwrap_or("");
    let mut parts = start.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let status = if target.starts_with("/missing") { "404 Not Found" } else { "200 OK" };
    let reply = format!("{method} {target} [{}]", &body[..len]);
    Some(format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).into_bytes())
}

impl AsyncWrite for FakeConn {
    fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        this.request.extend_from_slice(data);
        if this.response.is_none()
            && let Some(r) = respond(&this.request)
        {
            this.net.requests.fetch_add(1, Ordering::SeqCst);
            this.response = Some(r);
            if this.request.starts_with(b"GET /slow") {
                // Never answers.
            } else if this.net.delay {
                let ready = this.ready.clone();
                let n = this.net.jitter.fetch_add(13, Ordering::SeqCst);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_micros(n * 7919 % 3000));
                    let mut r = ready.lock().unwrap();
                    r.0 = true;
                    if let Some(w) = r.1.take() {
                        w.wake();
                    }
                });
            } else {
                this.ready.lock().unwrap().0 = true;
            }
        }
        Poll::Ready(Ok(data.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for FakeConn {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        {
            let mut r = this.ready.lock().unwrap();
            if !r.0 {
                r.1 = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
        let Some(resp) = &this.response else { return Poll::Ready(Ok(())) };
        let n = (resp.len() - this.sent).min(buf.remaining());
        buf.put_slice(&resp[this.sent..this.sent + n]);
        this.sent += n;
        Poll::Ready(Ok(()))
    }
}

impl Network for FakeNet {
    fn resolve<'a>(&'a self, _: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>> {
        Box::pin(std::future::ready(Ok(vec![SocketAddr::from(([93, 184, 216, 34], port))])))
    }
    fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)> {
        let conn = FakeConn { net: self.0.clone(), request: Vec::new(), response: None, sent: 0, ready: Arc::default() };
        Box::pin(std::future::ready(Ok((Box::new(conn) as Box<dyn Stream>, addrs[0]))))
    }
    fn listen(&self, _: SocketAddr) -> NetworkFuture<'_, Box<dyn Listener>> {
        Box::pin(std::future::ready(Err(io::Error::other("no listeners"))))
    }
}

// ---------------------------------------------------------------------------
// The host

/// What host functions did, across all runs of a [`World`].
#[derive(Default)]
struct HostLog {
    /// Calls of `host.next_id` and `host.charge`.
    calls: AtomicUsize,
    /// Calls of `host.charge` made before the journal was durable.
    early: AtomicUsize,
    /// Signals for `host.wait_signal`, by name.
    signals: Mutex<HashMap<String, String>>,
    /// Calls of `host.wait_signal`.
    waits: AtomicUsize,
    /// The journal of the current run, to check what is durable.
    journal: Mutex<Option<MemoryJournal>>,
}

impl HostLog {
    fn durable(&self) -> bool {
        self.journal.lock().unwrap().as_ref().is_none_or(|j| j.durable().len() == j.entries().len())
    }
}

fn host_module(log: Arc<HostLog>) -> NativeModule {
    let mut m = NativeModule::new("host");
    let l = log.clone();
    m.function(
        "next_id",
        Journaled(move |prefix: String| -> String {
            l.calls.fetch_add(1, Ordering::SeqCst);
            format!("{prefix}-id")
        }),
    );
    let l = log.clone();
    m.async_function(
        "charge",
        Effectful(move |cents: i64| {
            let l = l.clone();
            async move {
                if !l.durable() {
                    l.early.fetch_add(1, Ordering::SeqCst);
                }
                l.calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>((cents * 2, vec![cents % 2 == 0, true]))
            }
        }),
    );
    let l = log.clone();
    m.vm_function(
        "wait_signal",
        Journaled(move |_: &mut Vm, name: String| -> Result<Later<Option<String>, HostError>, HostError> {
            l.waits.fetch_add(1, Ordering::SeqCst);
            if let Some(p) = l.signals.lock().unwrap().remove(&name) {
                return Ok(Later::ready(Ok(Some(p))));
            }
            Ok(Later::new(async move {
                std::future::pending::<()>().await;
                |_: &mut Vm| Ok::<_, HostError>(None)
            })
            .parked(Park::UntilResumed))
        }),
    );
    m
}

// ---------------------------------------------------------------------------
// Runs

/// The environment of a run: everything but the journal.
struct World {
    base: Config,
    net: FakeNet,
    log: Arc<HostLog>,
}

impl World {
    fn new(clock: Arc<dyn Clock>, delay: bool, seed: Option<u64>) -> World {
        let net = FakeNet::new(delay);
        let log = Arc::new(HostLog::default());
        let mut base = Config::new()
            .policy(RulePolicy::allow_all())
            .clock(clock)
            .network(Arc::new(net.clone()))
            .module(host_module(log.clone()))
            .env(vec![("MODE".into(), "test".into())]);
        if let Some(s) = seed {
            base = base.seed(s);
        }
        World { base, net, log }
    }

    /// A deterministic world: frozen time, immediate network.
    fn frozen(seed: Option<u64>) -> World {
        World::new(Arc::new(FrozenClock(Instant::now())), false, seed)
    }

    /// A world with real time and a network whose responses take a varying time.
    fn live() -> World {
        World::new(Arc::new(SystemClock), true, None)
    }

    /// Runs `src` without a journal.
    fn run_plain(&self, src: &str) -> Run {
        let out = MemorySink::new();
        let config = self.base.clone().stdout(out.clone()).stderr(out.clone());
        let mut engine = Engine::new(config).unwrap();
        let result = rt().block_on(engine.run_source("main.sand", src));
        Run { result, stdout: out.contents(), fuel: engine.run_stats().fuel }
    }

    fn run(&self, src: &str, journal: &MemoryJournal) -> Run {
        let out = MemorySink::new();
        *self.log.journal.lock().unwrap() = Some(journal.clone());
        let durable = Durable::new(Arc::new(journal.clone())).suspend_after(Duration::from_secs(60));
        let config = self.base.clone().stdout(out.clone()).stderr(out.clone()).durable(durable);
        let mut engine = Engine::new(config).unwrap();
        let result = rt().block_on(engine.run_source("main.sand", src));
        Run { result, stdout: out.contents(), fuel: engine.run_stats().fuel }
    }
}

#[derive(Debug)]
struct Run {
    result: Result<Outcome, Error>,
    stdout: String,
    fuel: u64,
}

impl Run {
    fn same_as(&self, other: &Run) -> bool {
        self.stdout == other.stdout && self.fuel == other.fuel && format!("{:?}", self.result) == format!("{:?}", other.result)
    }
}

// ---------------------------------------------------------------------------
// Programs

/// Tasks that interleave HTTP calls, timers, channels, races, random
/// numbers, clock readings and host calls.
const CONCURRENT: &str = r#"
import http
import random
import time
import uuid
import host

let ch: Channel<str> = Channel(2)
var tasks: [Task<int>] = []
for i in 0..4 {
    tasks.push(spawn {
        task.sleep(1ms * (4 - i))
        let r = http.get("http://svc.test/item/\(i)")
        let text = match r {
            Ok(resp) => resp.text() ?? "?"
            Err(e) => "error \(e)"
        }
        let id = host.next_id("t\(i)")
        ch.send("\(i) \(text) \(id) \(random.int(0, 1000))")
        i * 10
    })
}
let racer = task.race([
    spawn {
        task.sleep(3ms)
        "slow"
    },
    spawn {
        http.post("http://svc.test/race", b"go").map(r => r.status).unwrap_or(0)
        "http"
    },
])
print("race won by \(racer)")
var got: [str] = []
for _ in 0..4 {
    got.push(ch.recv() ?? "closed")
}
for line in got {
    print(line)
}
print(task.all(tasks))
let t = time.now()
let w = time.Stopwatch()
let (doubled, flags) = host.charge(21)?
print("charged \(doubled) \(flags.len())")
print("uuid \(uuid.v7().len()) \(t.epoch_seconds() > 0) \(w.elapsed() >= 0s)")
let missing = http.get("http://svc.test/missing")?
print("missing \(missing.status)")
"#;

/// A sequential program with a redirect-free request chain and a map.
const SEQUENTIAL: &str = r#"
import http
import random
import env

var totals: [str: int] = [:]
for name in ["a", "b", "c"] {
    let r = http.post("http://svc.test/sum/\(name)", "\(random.int(1, 9))".bytes())?
    totals[name] = r.text()?.len()
    task.yield_now()
}
print(totals)
print(env.get("MODE"))
print(random.float() < 1.0)
"#;

/// A program with many tasks that preempt each other while computing.
const PREEMPTED: &str = r#"
import http
import time

fn work(n: int) -> int {
    var acc = 0
    for i in 0..n {
        acc = (acc * 31 + i) % 1000003
    }
    acc
}

let ch: Channel<int> = Channel()
for k in 0..3 {
    spawn {
        let base = work(30000 + k * 7000)
        let r = http.get("http://svc.test/w/\(k)/\(base)")
        ch.send(base + r.map(x => x.status).unwrap_or(-1))
    }
}
var sum = 0
for _ in 0..3 {
    sum += ch.recv() ?? 0
}
print(sum)
print(time.now().epoch_seconds() > 0)
"#;

/// Cancels tasks that wait for requests and timers.
const CANCELLING: &str = r#"
import http
import time
let r = task.timeout(2ms, () => http.get("http://svc.test/slow/1"))
print(r.is_none())
let t = spawn http.get("http://svc.test/slow/2")
let sleeper = spawn {
    task.sleep(1h)
    "late"
}
task.yield_now()
t.cancel()
print(t.is_done())
print(http.get("http://svc.test/after")?.status)
let first = task.race([sleeper, spawn {
    task.sleep(1ms)
    "early"
}])
print(first)
print(time.now().epoch_seconds() > 0)
"#;

/// Many tasks that compute, fetch and yield, sending on a bounded channel.
const STRESS: &str = r#"
import http
import random
let ch: Channel<str> = Channel(3)
var tasks: [Task<int>] = []
for k in 0..8 {
    tasks.push(spawn {
        var acc = k
        for round in 0..3 {
            for i in 0..(4000 * (k % 3 + 1)) {
                acc = (acc * 17 + i) % 99991
            }
            let r = http.get("http://svc.test/s/\(k)/\(round)/\(acc)")
            acc += r.map(x => x.body.len()).unwrap_or(0)
            if random.bool() {
                task.yield_now()
            }
            ch.send("\(k).\(round)")
        }
        acc
    })
}
var order: [str] = []
for _ in 0..24 {
    order.push(ch.recv() ?? "")
}
print(order)
print(task.all(tasks))
"#;

const PROGRAMS: &[&str] = &[CONCURRENT, SEQUENTIAL, PREEMPTED, CANCELLING, STRESS];

// ---------------------------------------------------------------------------
// Tests

#[test]
fn replay_reproduces_the_recorded_run() {
    for (i, src) in PROGRAMS.iter().enumerate() {
        let world = World::live();
        let journal = MemoryJournal::new();
        let recorded = world.run(src, &journal);
        assert!(
            recorded.result.is_ok(),
            "program {i}: {:?}
{}",
            recorded.result,
            recorded.stdout
        );
        let entries = journal.entries();
        assert_eq!(journal.durable().len(), entries.len(), "program {i}: the run flushes the journal at its end");
        let (requests, calls) = (world.net.requests(), world.log.calls.load(Ordering::SeqCst));
        for _ in 0..3 {
            let again = MemoryJournal::with_entries(entries.clone());
            let replayed = world.run(src, &again);
            assert!(
                replayed.same_as(&recorded),
                "program {i}:
{recorded:?}
{replayed:?}"
            );
            assert_eq!(again.entries(), entries, "program {i}: replay records nothing");
            assert_eq!(world.net.requests(), requests, "program {i}: replay sends no requests");
            assert_eq!(world.log.calls.load(Ordering::SeqCst), calls, "program {i}: replay calls no journaled host function");
        }
    }
}

#[test]
fn plain_and_journaled_runs_behave_alike() {
    for (i, src) in PROGRAMS.iter().enumerate() {
        let world = World::frozen(Some(5));
        let plain = world.run_plain(src);
        let journaled = world.run(src, &MemoryJournal::new());
        assert!(plain.result.is_ok(), "program {i}: {:?}", plain.result);
        assert_eq!(journaled.stdout, plain.stdout, "program {i}");
        assert_eq!(format!("{:?}", journaled.result), format!("{:?}", plain.result), "program {i}");
        assert_eq!(journaled.fuel, plain.fuel, "program {i}");
    }
}

#[test]
fn a_run_resumes_from_every_prefix_of_its_journal() {
    for (i, src) in PROGRAMS.iter().enumerate() {
        for seed in [Some(5), None] {
            let world = World::frozen(seed);
            let journal = MemoryJournal::new();
            let whole = world.run(src, &journal);
            assert!(
                whole.result.is_ok(),
                "program {i}: {:?}
{}",
                whole.result,
                whole.stdout
            );
            let entries = journal.entries();
            // Without a configured seed, the first entry is the seed.
            let first = usize::from(seed.is_none());
            for k in first..=entries.len() {
                let prefix = MemoryJournal::with_entries(entries[..k].to_vec());
                let resumed = world.run(src, &prefix);
                assert!(
                    resumed.same_as(&whole),
                    "program {i}, seed {seed:?}, prefix {k} of {}:
{whole:?}
{resumed:?}",
                    entries.len()
                );
                assert_eq!(prefix.entries(), entries, "program {i}, seed {seed:?}, prefix {k}: the journal");
            }
        }
    }
}

#[test]
fn a_run_resumes_from_prefixes_with_live_timing() {
    for (i, src) in PROGRAMS.iter().enumerate() {
        let world = World::live();
        let journal = MemoryJournal::new();
        let whole = world.run(src, &journal);
        assert!(whole.result.is_ok(), "program {i}: {:?}", whole.result);
        let entries = journal.entries();
        let step = (entries.len() / 12).max(1);
        for k in (0..=entries.len()).step_by(step) {
            let prefix = MemoryJournal::with_entries(entries[..k].to_vec());
            let resumed = world.run(src, &prefix);
            assert!(resumed.result.is_ok(), "program {i}, prefix {k}: {:?}", resumed.result);
            let after = prefix.entries();
            assert_eq!(after[..k], entries[..k], "program {i}, prefix {k}: the resumed run keeps the prefix");
            // The resumed run's own journal replays it exactly.
            let replayed = world.run(src, &MemoryJournal::with_entries(after.clone()));
            assert!(
                replayed.same_as(&resumed),
                "program {i}, prefix {k}:
{resumed:?}
{replayed:?}"
            );
        }
    }
}

#[test]
fn a_changed_program_diverges() {
    let world = World::frozen(Some(1));
    let journal = MemoryJournal::new();
    let r = world.run("import http\nprint(http.get(\"http://svc.test/a\")?.status)", &journal);
    assert_eq!(
        r.stdout,
        "200
"
    );
    let other = MemoryJournal::with_entries(journal.entries());
    let r = world.run("import http\nprint(http.get(\"http://svc.test/b\")?.status)", &other);
    match r.result {
        Err(Error::Diverged { seq, message }) => {
            assert_eq!(seq, 0, "{message}");
            assert!(message.contains("GET http://svc.test/b") && message.contains("GET http://svc.test/a"), "{message}");
        }
        other => panic!("expected a divergence, got {other:?}"),
    }
    // Reading the clock where the journal records a request also diverges.
    let r = world.run("import time\nprint(time.now().epoch_seconds())", &MemoryJournal::with_entries(journal.entries()));
    assert!(matches!(r.result, Err(Error::Diverged { seq: 0, .. })), "{:?}", r.result);
    // So do different arguments to a journaled host function.
    let journal = MemoryJournal::new();
    world.run("import host\nprint(host.next_id(\"a\"))", &journal);
    let r = world.run("import host\nprint(host.next_id(\"b\"))", &MemoryJournal::with_entries(journal.entries()));
    assert!(matches!(&r.result, Err(Error::Diverged { message, .. }) if message.contains("host.next_id")), "{:?}", r.result);
}

#[test]
fn long_sleeps_suspend_the_run_until_they_are_due() {
    let clock = ManualClock::new();
    let world = World::new(clock.clone(), false, Some(3));
    let src = r#"
import time
let start = time.now()
print("before")
let short = spawn { task.sleep(0s)
 1 }
print(short.wait())
task.sleep(2h)
print("after \(time.now().since(start) >= 2h)")
"#;
    let journal = MemoryJournal::new();
    let first = world.run(src, &journal);
    let wake = UNIX_EPOCH + Duration::from_secs(BASE_SECS) + Duration::from_secs(7200);
    assert!(
        matches!(first.result, Ok(Outcome::Suspended(Suspension { wake_at: Some(t), parked: false })) if t == wake),
        "{:?}",
        first.result
    );
    assert_eq!(
        first.stdout,
        "before
1
"
    );
    assert_eq!(journal.durable().len(), journal.entries().len(), "suspending flushes the journal");
    // Resuming early suspends again at the same point.
    clock.advance(Duration::from_secs(3600));
    let early = world.run(src, &journal);
    assert!(matches!(early.result, Ok(Outcome::Suspended(Suspension { wake_at: Some(t), .. })) if t == wake), "{:?}", early.result);
    clock.advance(Duration::from_secs(3601));
    let done = world.run(src, &journal);
    assert!(done.result.is_ok(), "{:?}", done.result);
    assert_eq!(
        done.stdout,
        "before
1
after true
"
    );
    // The finished journal replays the whole run, with nothing to wait for.
    let replayed = world.run(src, &MemoryJournal::with_entries(journal.entries()));
    assert!(
        replayed.same_as(&done),
        "{done:?}
{replayed:?}"
    );
}

#[test]
fn a_parked_host_call_suspends_until_the_host_resumes_the_run() {
    let world = World::frozen(Some(9));
    let src = r#"
import host
print("waiting")
let payload = host.wait_signal("approve")
print("got \(payload)")
"#;
    let journal = MemoryJournal::new();
    let first = world.run(src, &journal);
    assert!(matches!(first.result, Ok(Outcome::Suspended(Suspension { wake_at: None, parked: true }))), "{:?}", first.result);
    assert_eq!(first.stdout, "waiting\n");
    world.log.signals.lock().unwrap().insert("approve".into(), "yes".into());
    let resumed = world.run(src, &journal);
    assert!(resumed.result.is_ok(), "{:?}", resumed.result);
    assert_eq!(resumed.stdout, "waiting\ngot Ok(Some(\"yes\"))\n");
    assert_eq!(world.log.waits.load(Ordering::SeqCst), 2);
    // Replay returns the recorded signal without calling the host again.
    let replayed = world.run(src, &MemoryJournal::with_entries(journal.entries()));
    assert!(
        replayed.same_as(&resumed),
        "{resumed:?}
{replayed:?}"
    );
    assert_eq!(world.log.waits.load(Ordering::SeqCst), 2);
}

#[test]
fn journaled_host_functions_run_once_and_effects_wait_for_a_flush() {
    let world = World::frozen(None);
    let src = r#"
import host
import random
let a = host.next_id("x")
let n = random.int(0, 100)
let (c, flags) = host.charge(n)?
print("\(a) \(c == 2 * n) \(flags)")
"#;
    let journal = MemoryJournal::new();
    let first = world.run(src, &journal);
    assert!(first.result.is_ok(), "{:?}", first.result);
    assert_eq!(world.log.calls.load(Ordering::SeqCst), 2);
    assert_eq!(world.log.early.load(Ordering::SeqCst), 0, "the effect starts after a flush");
    assert!(journal.flushes() >= 2, "a flush before the effect and one at the end");
    for _ in 0..3 {
        let again = world.run(src, &MemoryJournal::with_entries(journal.entries()));
        assert!(
            again.same_as(&first),
            "{first:?}
{again:?}"
        );
    }
    assert_eq!(world.log.calls.load(Ordering::SeqCst), 2, "each host function ran once across record and replay");
    let kinds: Vec<EntryKind> = journal.entries().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [EntryKind::Seed, EntryKind::Result, EntryKind::Start, EntryKind::Done]);
}

#[test]
fn sockets_and_servers_are_unsupported_in_a_journaled_run() {
    let world = World::frozen(Some(1));
    let src = r#"
import net
import http
fn kind<T>(r: Result<T>) -> str {
    match r {
        Ok(_) => "ok"
        Err(e) => e.kind
    }
}
print(kind(net.connect("svc.test", 80)))
print(kind(net.listen("127.0.0.1", 0)))
print(kind(http.serve("127.0.0.1", 0)))
"#;
    let r = world.run(src, &MemoryJournal::new());
    assert_eq!(
        r.stdout,
        "unsupported
unsupported
unsupported
",
        "{:?}",
        r.result
    );
}

#[test]
fn entries_round_trip_through_bytes() {
    let world = World::frozen(None);
    let journal = MemoryJournal::new();
    world.run(CONCURRENT, &journal);
    for e in journal.entries() {
        assert_eq!(Entry::from_bytes(&e.to_bytes()), Some(e.clone()));
        assert!(e.decode().is_some(), "{e:?}");
        assert!(!e.describe().starts_with("a malformed"), "{e:?}");
    }
    assert_eq!(Entry::from_bytes(&[1, 2]), None);
    let bad = Entry { seq: 0, kind: EntryKind::Start, data: vec![9] };
    assert_eq!(bad.decode(), None);
    assert_eq!(bad.describe(), "a malformed start entry");
}

#[test]
fn operations_pair_starts_with_their_ends() {
    let world = World::frozen(Some(4));
    let journal = MemoryJournal::new();
    let src = "import http\nprint(http.get(\"http://svc.test/a\")?.status)\nprint(http.get(\"http://svc.test/missing\")?.status)";
    let r = world.run(src, &journal);
    assert!(r.result.is_ok(), "{:?}", r.result);
    let ops = operations(&journal.entries()).unwrap();
    let labels: Vec<&str> = ops.iter().map(|o| o.start.label.as_str()).collect();
    assert_eq!(labels, ["GET http://svc.test/a", "GET http://svc.test/missing"]);
    let statuses: Vec<i64> = ops
        .iter()
        .map(|o| match o.http() {
            Some(HttpOutcome::Response { status, .. }) => status,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(statuses, [200, 404]);
    assert!(ops.iter().all(|o| o.start.kind == OpKind::Http && !o.cancelled()));
    let mut entries = journal.entries();
    let done = entries.iter().position(|e| e.kind == EntryKind::Done).unwrap();
    entries.push(Entry { seq: entries.len() as u64, ..entries[done].clone() });
    assert!(operations(&entries).unwrap_err().contains("a second time"));
}

#[test]
fn fed_input_and_the_process_environment_are_journaled() {
    let base = Config::new().policy(RulePolicy::allow_all()).clock(Arc::new(FrozenClock(Instant::now()))).seed(2);
    let src = r#"
import io
import env
print(io.stdin.read_line()?)
print(io.stdin.read_all()?.len())
print(env.get("PATH").is_some())
print(env.vars().len() > 0)
"#;
    let fed = || {
        let feed = sandlang::StdinFeed::new();
        feed.send("hello\nrest");
        feed.end();
        feed
    };
    let plain = MemorySink::new();
    let r = rt().block_on(Engine::new(base.clone().stdout(plain.clone()).stdin_feed(&fed())).unwrap().run_source("main.sand", src));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(plain.contents(), "Some(\"hello\")\n4\ntrue\ntrue\n");
    let journal = MemoryJournal::new();
    let out = MemorySink::new();
    let config = base.clone().stdout(out.clone()).stdin_feed(&fed()).durable(Durable::new(Arc::new(journal.clone())));
    let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out.contents(), plain.contents());
    // A line that is available is a synchronous input; a read that waits is an operation.
    let kinds: Vec<EntryKind> = journal.entries().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [EntryKind::Result, EntryKind::Start, EntryKind::Done, EntryKind::Result, EntryKind::Result]);
    // Replay does not read the feed, which has nothing now.
    let empty = sandlang::StdinFeed::new();
    empty.end();
    let again = MemorySink::new();
    let config = base
        .clone()
        .stdout(again.clone())
        .stdin_feed(&empty)
        .durable(Durable::new(Arc::new(MemoryJournal::with_entries(journal.entries()))));
    let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(again.contents(), out.contents());
}

#[test]
fn monotonic_time_keeps_moving_forward_after_replay() {
    let src = r#"
import time
import host
let w = time.Stopwatch()
host.tick()
let a = w.elapsed()
host.wait_signal("go")
let b = w.elapsed()
print("\(a) \(b >= a)")
"#;
    let journal = MemoryJournal::new();
    let run = |clock: Arc<ManualClock>, log: Arc<HostLog>| {
        let mut m = host_module(log);
        let c = clock.clone();
        m.function("tick", Journaled(move || c.advance(Duration::from_secs(50))));
        let out = MemorySink::new();
        let config = Config::new()
            .policy(RulePolicy::allow_all())
            .clock(clock)
            .module(m)
            .seed(1)
            .stdout(out.clone())
            .durable(Durable::new(Arc::new(journal.clone())).suspend_after(Duration::from_secs(60)));
        let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
        (r, out.contents())
    };
    let log = Arc::new(HostLog::default());
    let (r, _) = run(ManualClock::new(), log.clone());
    assert!(matches!(r, Ok(Outcome::Suspended(Suspension { wake_at: None, .. }))), "{r:?}");
    // The resumed run's process started later: its monotonic clock reads less.
    log.signals.lock().unwrap().insert("go".into(), "now".into());
    let (r, out) = run(ManualClock::new(), log);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "50s true\n");
}

#[test]
fn a_parked_call_with_a_deadline_suspends_until_the_deadline() {
    let clock = ManualClock::new();
    let log = Arc::new(HostLog::default());
    let mut m = NativeModule::new("signals");
    let l = log.clone();
    let c = clock.clone();
    m.vm_function(
        "wait_until",
        Journaled(
            move |_: &mut Vm, name: String, deadline: sandlang::jiff::Timestamp| -> Result<Later<Option<String>, HostError>, HostError> {
                l.waits.fetch_add(1, Ordering::SeqCst);
                if let Some(p) = l.signals.lock().unwrap().remove(&name) {
                    return Ok(Later::ready(Ok(Some(p))));
                }
                let due = UNIX_EPOCH + Duration::from_nanos(deadline.as_nanosecond() as u64);
                if c.wall() >= due {
                    return Ok(Later::ready(Ok(None)));
                }
                Ok(Later::new(async move {
                    std::future::pending::<()>().await;
                    |_: &mut Vm| Ok::<_, HostError>(None)
                })
                .parked(Park::Until(due)))
            },
        ),
    );
    m.source(
        r#"
import time
/// Waits for signal `name` for at most `timeout`.
pub fn wait(name: str, timeout: duration) -> Result<str?> {
    wait_until(name, time.now().add(timeout))
}
"#,
    );
    let src = "import signals\nprint(signals.wait(\"go\", 1d))";
    let journal = MemoryJournal::new();
    let base = Config::new().policy(RulePolicy::allow_all()).clock(clock.clone()).module(m).seed(1);
    let run = || {
        let out = MemorySink::new();
        let config =
            base.clone().stdout(out.clone()).durable(Durable::new(Arc::new(journal.clone())).suspend_after(Duration::from_secs(60)));
        let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
        (r, out.contents())
    };
    let due = UNIX_EPOCH + Duration::from_secs(BASE_SECS + 86_400);
    let (r, _) = run();
    assert!(matches!(r, Ok(Outcome::Suspended(Suspension { wake_at: Some(t), .. })) if t == due), "{r:?}");
    clock.advance(Duration::from_secs(86_400));
    let (r, out) = run();
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "Ok(None)\n");
    assert_eq!(log.waits.load(Ordering::SeqCst), 2);
}

#[test]
fn a_changed_instant_argument_of_a_journaled_call_diverges() {
    // A `time.Instant` has no journal encoding; the call's identity still covers it.
    let mut m = NativeModule::new("deadline");
    m.function("seconds_until", Journaled(|t: sandlang::jiff::Timestamp| -> i64 { t.as_second() }));
    let base = Config::new().policy(RulePolicy::allow_all()).module(m).seed(1);
    let run = |src: &str, journal: &MemoryJournal| {
        let out = MemorySink::new();
        let config = base.clone().stdout(out.clone()).durable(Durable::new(Arc::new(journal.clone())));
        let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
        (r, out.contents())
    };
    let src = |s: i64| format!("import deadline\nimport time\nprint(deadline.seconds_until(time.Instant.from_epoch_seconds({s})))");
    let journal = MemoryJournal::new();
    let (r, out) = run(&src(100), &journal);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "100\n");
    let (r, out) = run(&src(100), &MemoryJournal::with_entries(journal.entries()));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "100\n");
    let (r, _) = run(&src(200), &MemoryJournal::with_entries(journal.entries()));
    assert!(matches!(&r, Err(Error::Diverged { message, .. }) if message.contains("deadline.seconds_until")), "{r:?}");
    // An asynchronous call whose arguments changed says so.
    let mut m = NativeModule::new("later");
    m.async_function("echo", Journaled(|t: sandlang::jiff::Timestamp| async move { Ok::<_, String>(t.as_second()) }));
    let base = Config::new().policy(RulePolicy::allow_all()).module(m).seed(1);
    let run = |src: &str, journal: &MemoryJournal| {
        let config = base.clone().stdout(MemorySink::new()).durable(Durable::new(Arc::new(journal.clone())));
        rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src))
    };
    let src = |s: i64| format!("import later\nimport time\nprint(later.echo(time.Instant.from_epoch_seconds({s}))?)");
    let journal = MemoryJournal::new();
    assert!(run(&src(1), &journal).is_ok());
    let r = run(&src(2), &MemoryJournal::with_entries(journal.entries()));
    assert!(matches!(&r, Err(Error::Diverged { message, .. }) if message.contains("with other arguments")), "{r:?}");
}

#[test]
fn wall_time_does_not_go_back_on_a_host_whose_clock_is_behind() {
    let src = r#"
import time
import host
let a = time.now()
task.sleep(10s)
host.tick()
let b = time.now()
host.wait_signal("go")
let c = time.now()
print("\(b.since(a) >= 10s) \(c.since(b) >= 0s)")
"#;
    let journal = MemoryJournal::new();
    let run = |clock: Arc<ManualClock>, log: Arc<HostLog>| {
        let mut m = host_module(log);
        let c = clock.clone();
        m.function("tick", Journaled(move || c.advance(Duration::from_secs(50))));
        let out = MemorySink::new();
        let config = Config::new()
            .policy(RulePolicy::allow_all())
            .clock(clock.clone())
            .module(m)
            .seed(1)
            .stdout(out.clone())
            .durable(Durable::new(Arc::new(journal.clone())).suspend_after(Duration::ZERO));
        let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
        (r, out.contents())
    };
    let log = Arc::new(HostLog::default());
    let first = ManualClock::new();
    let (r, _) = run(first.clone(), log.clone());
    let wake = UNIX_EPOCH + Duration::from_secs(BASE_SECS + 10);
    assert!(matches!(r, Ok(Outcome::Suspended(Suspension { wake_at: Some(t), .. })) if t == wake), "{r:?}");
    // The next host's clock is a minute behind the first one's.
    first.advance(Duration::from_secs(10));
    let (r, _) = run(first, log.clone());
    assert!(matches!(r, Ok(Outcome::Suspended(Suspension { wake_at: None, .. }))), "{r:?}");
    log.signals.lock().unwrap().insert("go".into(), "now".into());
    let (r, out) = run(ManualClock::new(), log);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "true true\n");
}

/// A durable call that suspends returns its value when the host calls the function again
/// with the same journal.
#[test]
fn a_suspended_call_resumes_and_returns() {
    let fs = sandlang::MemFs::new();
    fs.write("/main.sand", "import task\nfn main(n: int) -> int {\n    print(\"start\")\n    task.sleep(10s)\n    n * 2\n}").unwrap();
    let base = Config::new().memory_fs(fs).seed(1);
    let program = rt().block_on(Engine::new(base.clone()).unwrap().compile_file("/main.sand")).unwrap();
    let main = program.function("main").unwrap();
    let journal = MemoryJournal::new();
    let clock = ManualClock::new();
    let call = || {
        let out = MemorySink::new();
        let durable = Durable::new(Arc::new(journal.clone())).suspend_after(Duration::ZERO);
        let config = base.clone().clock(clock.clone()).stdout(out.clone()).durable(durable);
        (rt().block_on(Engine::new(config).unwrap().call(&main, &["21"])), out.contents())
    };
    let (r, out) = call();
    let wake = UNIX_EPOCH + Duration::from_secs(BASE_SECS + 10);
    assert!(matches!(r, Ok(Outcome::Suspended(Suspension { wake_at: Some(t), .. })) if t == wake), "{r:?}");
    assert_eq!(out, "start\n");
    clock.advance(Duration::from_secs(10));
    let (r, _) = call();
    assert_eq!(r.unwrap(), Outcome::Returned(Ok("42".into())));
}

#[test]
fn time_spent_in_flushes_does_not_count_against_the_wall_time_limit() {
    struct Slow(MemoryJournal, Arc<ManualClock>);
    impl Journal for Slow {
        fn replay(&self) -> Vec<Entry> {
            self.0.replay()
        }
        fn record(&self, entry: Entry) {
            self.0.record(entry)
        }
        fn flush(&self) -> sandlang::journal::FlushFuture {
            self.1.advance(Duration::from_secs(2));
            self.0.flush()
        }
    }
    let clock = ManualClock::new();
    let world = World::new(clock.clone(), false, Some(1));
    let limits = sandlang::Limits { max_wall_time: Some(Duration::from_secs(1)), ..sandlang::Limits::default() };
    let journal = Slow(MemoryJournal::new(), clock);
    let config = world.base.clone().limits(limits).stdout(MemorySink::new()).durable(Durable::new(Arc::new(journal)));
    let src = "import http\nfor i in 0..3 {\n    print(http.get(\"http://svc.test/\\(i)\")?.status)\n}";
    let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src));
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn effects_have_the_same_sequence_number_in_every_execution() {
    let seqs: Arc<Mutex<Vec<(String, Option<u64>)>>> = Arc::default();
    let mut m = NativeModule::new("fx");
    let s = seqs.clone();
    m.vm_function(
        "send",
        Effectful(move |_: &mut Vm, call: JournaledCall, name: String| -> Result<Later<i64, HostError>, HostError> {
            s.lock().unwrap().push((name.clone(), call.seq()));
            if name == "parked" {
                return Ok(Later::new(async move {
                    std::future::pending::<()>().await;
                    |_: &mut Vm| Ok::<_, HostError>(0)
                })
                .parked(Park::UntilResumed));
            }
            Ok(Later::ready(Ok(1)))
        }),
    );
    let filtered: Arc<Mutex<Vec<Option<u64>>>> = Arc::default();
    let f = filtered.clone();
    let net = FakeNet::new(false);
    let base = Config::new()
        .policy(RulePolicy::allow_all())
        .clock(Arc::new(FrozenClock(Instant::now())))
        .network(Arc::new(net))
        .module(m)
        .seed(1)
        .request_filter(move |t: &sandlang::RequestTarget, _: &mut Vec<(String, String)>| {
            f.lock().unwrap().push(t.journal_seq);
            Ok(())
        });
    let src = "import fx\nimport http\nprint(fx.send(\"a\")?)\nprint(http.get(\"http://svc.test/x\")?.status)\nprint(fx.send(\"parked\")?)";
    let journal = MemoryJournal::new();
    let run = || {
        let config = base.clone().stdout(MemorySink::new()).durable(Durable::new(Arc::new(journal.clone())).suspend_after(Duration::ZERO));
        rt().block_on(Engine::new(config).unwrap().run_source("main.sand", src))
    };
    let r = run();
    assert!(matches!(r, Ok(Outcome::Suspended(Suspension { wake_at: None, .. }))), "{r:?}");
    let first = seqs.lock().unwrap().clone();
    assert_eq!(first.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["a", "parked"]);
    let entries = journal.entries();
    for (_, seq) in &first {
        let seq = seq.expect("a journaled call has a sequence number") as usize;
        assert!(matches!(entries[seq].kind, EntryKind::Result | EntryKind::Start), "{:?}", entries[seq]);
    }
    let requests = filtered.lock().unwrap().clone();
    let http = operations(&entries).unwrap().into_iter().find(|o| o.start.kind == OpKind::Http).unwrap();
    assert_eq!(requests, [Some(http.seq)]);
    // The resumed run replays the first call and the request, and calls the parked function
    // again: it gets the same number. The filter sees only requests that are sent.
    let _ = run();
    assert_eq!(seqs.lock().unwrap()[2..], first[1..]);
    assert_eq!(filtered.lock().unwrap()[..], requests[..]);
}

#[test]
fn a_journaled_call_that_runs_again_after_a_collection_replays_its_fuel() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut m = NativeModule::new("mem");
    let c = calls.clone();
    m.vm_function(
        "grow",
        Journaled(move |vm: &mut Vm, n: i64| -> Result<i64, sandlang::vm::Ctl> {
            c.fetch_add(1, Ordering::SeqCst);
            vm.charge(50)?;
            vm.check_memory(n as usize)?;
            Ok(n)
        }),
    );
    let base = Config::new().policy(RulePolicy::allow_all()).module(m).seed(1).retry_stress(true);
    let src = "import mem\nvar total = 0\nfor i in 0..3 {\n    total += mem.grow(i * 10)\n}\nprint(total)";
    let run = |journal: &MemoryJournal| {
        let out = MemorySink::new();
        let config = base.clone().stdout(out.clone()).durable(Durable::new(Arc::new(journal.clone())));
        let mut engine = Engine::new(config).unwrap();
        let r = rt().block_on(engine.run_source("main.sand", src));
        (r, out.contents(), engine.run_stats().fuel)
    };
    let journal = MemoryJournal::new();
    let (r, out, fuel) = run(&journal);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "30\n");
    assert_eq!(calls.load(Ordering::SeqCst), 6, "each call runs again after the forced collection");
    let (r, again, replayed_fuel) = run(&MemoryJournal::with_entries(journal.entries()));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!((again, replayed_fuel), (out, fuel), "replay charges the fuel of the attempt that completes");
    assert_eq!(calls.load(Ordering::SeqCst), 6);
}

#[test]
fn a_native_that_runs_again_after_a_collection_costs_the_fuel_of_one_call() {
    let mut m = NativeModule::new("mem");
    m.vm_function("grow", |vm: &mut Vm, n: i64| -> Result<i64, sandlang::vm::Ctl> {
        vm.charge(50)?;
        vm.check_memory(n as usize)?;
        Ok(n)
    });
    let src = "import mem\nvar total = 0\nfor i in 0..3 {\n    total += mem.grow(i * 10)\n}\nprint(total)";
    let fuel = |stress: bool| {
        let config = Config::new().policy(RulePolicy::allow_all()).module(m.clone()).retry_stress(stress).stdout(MemorySink::new());
        let mut engine = Engine::new(config).unwrap();
        let r = rt().block_on(engine.run_source("main.sand", src));
        assert!(r.is_ok(), "{r:?}");
        engine.run_stats().fuel
    };
    assert_eq!(fuel(true), fuel(false));
}

#[test]
fn an_asynchronous_host_function_that_is_not_journaled_runs_again_in_replay() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut m = NativeModule::new("local");
    let c = calls.clone();
    m.async_function("double", move |n: i64| {
        c.fetch_add(1, Ordering::SeqCst);
        async move { Ok::<_, String>(n * 2) }
    });
    let base = Config::new().policy(RulePolicy::allow_all()).module(m).seed(1);
    let run = |journal: &MemoryJournal| {
        let out = MemorySink::new();
        let config = base.clone().stdout(out.clone()).durable(Durable::new(Arc::new(journal.clone())));
        let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", "import local\nprint(local.double(21)?)"));
        (r, out.contents())
    };
    let journal = MemoryJournal::new();
    let (r, out) = run(&journal);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "42\n");
    assert!(journal.entries().is_empty(), "{:?}", journal.entries());
    let (r, again) = run(&MemoryJournal::with_entries(journal.entries()));
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(again, out);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_host_abort_ends_the_run_with_its_payload_and_is_not_journaled() {
    #[derive(Debug, PartialEq)]
    struct Outage(u32);
    impl std::fmt::Display for Outage {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "outage {}", self.0)
        }
    }
    impl std::error::Error for Outage {}
    let calls = Arc::new(AtomicUsize::new(0));
    let mut m = NativeModule::new("store");
    let c = calls.clone();
    m.vm_function(
        "read",
        Journaled(move |_: &mut Vm, key: String| -> Result<Later<String, HostError>, HostError> {
            let first = c.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(Later::new(async move {
                move |_: &mut Vm| {
                    if first {
                        return Err(sandlang::vm::Ctl::abort(Outage(7)).into());
                    }
                    Ok::<_, HostError>(format!("value of {key}"))
                }
            }))
        }),
    );
    let base = Config::new().policy(RulePolicy::allow_all()).module(m).seed(1);
    let journal = MemoryJournal::new();
    let run = || {
        let out = MemorySink::new();
        let config = base.clone().stdout(out.clone()).durable(Durable::new(Arc::new(journal.clone())));
        let r = rt().block_on(Engine::new(config).unwrap().run_source("main.sand", "import store\nprint(store.read(\"k\")?)"));
        (r, out.contents())
    };
    let (r, _) = run();
    match &r {
        Err(Error::Aborted(a)) => assert_eq!(a.downcast_ref::<Outage>(), Some(&Outage(7))),
        other => panic!("expected an abort, got {other:?}"),
    }
    assert!(journal.entries().iter().all(|e| e.kind != EntryKind::Done), "{:?}", journal.entries());
    // The resumed run calls the function again.
    let (r, out) = run();
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(out, "value of k\n");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_journal_that_fails_to_flush_aborts_the_run_with_its_error() {
    #[derive(Debug)]
    struct Fenced;
    impl std::fmt::Display for Fenced {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("another node owns the run")
        }
    }
    impl std::error::Error for Fenced {}
    struct Lost(MemoryJournal);
    impl Journal for Lost {
        fn replay(&self) -> Vec<Entry> {
            self.0.replay()
        }
        fn record(&self, entry: Entry) {
            self.0.record(entry)
        }
        fn flush(&self) -> sandlang::journal::FlushFuture {
            Box::pin(std::future::ready(Err(sandlang::HostAbort::new(Fenced))))
        }
    }
    let world = World::frozen(Some(1));
    let config = world.base.clone().durable(Durable::new(Arc::new(Lost(MemoryJournal::new()))));
    let r =
        rt().block_on(Engine::new(config).unwrap().run_source("main.sand", "import http\nprint(http.get(\"http://svc.test/x\")?.status)"));
    match &r {
        Err(Error::Aborted(e)) => assert!(e.downcast_ref::<Fenced>().is_some(), "{e:?}"),
        other => panic!("expected an abort, got {other:?}"),
    }
    assert_eq!(world.net.requests(), 0, "the request waits for the flush");
}

/// A host module whose `wait_gate` waits, in a local operation, until
/// another task calls `open_gate`.
fn gate_module() -> NativeModule {
    let (tx, _) = tokio::sync::watch::channel(false);
    let tx = Arc::new(tx);
    let mut m = NativeModule::new("gate");
    let t = tx.clone();
    m.async_function("wait_gate", move || {
        let mut rx = t.subscribe();
        async move {
            let _ = rx.wait_for(|open| *open).await;
            Ok::<_, String>(7i64)
        }
    });
    m.function("open_gate", move || {
        tx.send_replace(true);
    });
    m
}

#[test]
fn local_operations_wait_without_blocking_other_tasks() {
    let src = r#"
import gate
let waiter = spawn {
    let n = gate.wait_gate().unwrap_or(0)
    print("opened \(n)")
    n
}
task.yield_now()
print("opening")
gate.open_gate()
print(waiter.wait())
"#;
    let run = |journal: Option<&MemoryJournal>| {
        let out = MemorySink::new();
        let mut config = Config::new().policy(RulePolicy::allow_all()).module(gate_module()).seed(1).stdout(out.clone());
        if let Some(j) = journal {
            config = config.durable(Durable::new(Arc::new(j.clone())));
        }
        let mut engine = Engine::new(config).unwrap();
        let r = rt().block_on(engine.run_source("main.sand", src));
        Run { result: r, stdout: out.contents(), fuel: engine.run_stats().fuel }
    };
    let plain = run(None);
    assert!(plain.result.is_ok(), "{:?}", plain.result);
    assert_eq!(plain.stdout, "opening\nopened 7\n7\n");
    let journal = MemoryJournal::new();
    let recorded = run(Some(&journal));
    assert!(recorded.same_as(&plain), "{plain:?}\n{recorded:?}");
    let entries = journal.entries();
    let ops = operations(&entries).unwrap();
    assert!(ops.len() == 1 && ops[0].start.kind == OpKind::Local && ops[0].done.is_some(), "{ops:?}");
    for k in 0..=entries.len() {
        let prefix = MemoryJournal::with_entries(entries[..k].to_vec());
        let resumed = run(Some(&prefix));
        assert!(resumed.same_as(&plain), "prefix {k}:\n{plain:?}\n{resumed:?}");
        assert_eq!(prefix.entries(), entries, "prefix {k}");
    }
}
