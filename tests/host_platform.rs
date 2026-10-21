//! Tests for custom host clock and network implementations.

mod common;

use common::run_with;
use sandlang::platform::{Clock, ClockFuture, Instant, SystemTime, UNIX_EPOCH};
use sandlang::{Error, LimitKind, Limits};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct ManualClock {
    origin: Instant,
    elapsed: Mutex<Duration>,
    sleeps: Mutex<Vec<Duration>>,
    work: Mutex<u64>,
}

impl ManualClock {
    fn new() -> Arc<ManualClock> {
        Arc::new(ManualClock {
            origin: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
            sleeps: Mutex::new(Vec::new()),
            work: Mutex::new(0),
        })
    }

    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }
}

impl Clock for ManualClock {
    fn wall(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000) + self.elapsed()
    }

    fn now(&self) -> Instant {
        self.origin + self.elapsed()
    }

    fn cpu_time(&self) -> Duration {
        Duration::from_nanos(*self.work.lock().unwrap() * 10)
    }

    fn sleep(&self, duration: Duration) -> ClockFuture {
        self.sleeps.lock().unwrap().push(duration);
        *self.elapsed.lock().unwrap() += duration;
        Box::pin(std::future::ready(()))
    }

    fn yield_now(&self) -> ClockFuture {
        Box::pin(std::future::ready(()))
    }

    fn work(&self, fuel: u64) {
        *self.work.lock().unwrap() += fuel;
        *self.elapsed.lock().unwrap() += Duration::from_nanos(fuel * 10);
    }
}

#[test]
fn scripts_read_host_clock() {
    let clock = ManualClock::new();
    let src = "import time\nimport uuid\nprint(time.now())\nprint(uuid.v7()[0..8])";
    let r = run_with(src, |c| c.clock(clock.clone()));
    assert_eq!(r.ok_stdout(), "2027-01-15T08:00:00Z\n01a3185c\n");
}

#[test]
fn sleeps_wait_on_host_timers() {
    let clock = ManualClock::new();
    let src = "import task\nimport time\nlet start = time.stopwatch()\ntask.sleep(5s)\nprint(start.elapsed() >= 5s)";
    let r = run_with(src, |c| c.clock(clock.clone()));
    assert_eq!(r.ok_stdout(), "true\n");
    assert!(clock.sleeps.lock().unwrap().contains(&Duration::from_secs(5)));
}

#[test]
fn wall_time_limit_is_measured_by_host_clock() {
    let clock = ManualClock::new();
    let limits = Limits { max_wall_time: Some(Duration::from_millis(50)), ..Limits::default() };
    let r = run_with("var n = 0\nwhile true { n += 1 }", |c| c.clock(clock.clone()).limits(limits));
    assert!(matches!(r.result, Err(Error::Limit(LimitKind::WallTime))), "{:?}", r.result);
    let elapsed = clock.elapsed();
    assert!(elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(60), "{elapsed:?}");
}

#[test]
fn memfs_timestamps_are_sourced_from_host_clock() {
    let clock = ManualClock::new();
    let fs = sandlang::MemFs::with_capacity_and_clock(1 << 20, clock.clone());
    fs.write("/run/.keep", "").unwrap();
    let src = "import fs\nfs.write_text(\"/run/notes.txt\", \"hi\")?\nprint(fs.metadata(\"/run/notes.txt\")?.modified.unwrap())";
    let out = sandlang::MemorySink::new();
    let config = sandlang::Config::new().policy(sandlang::Policy::allow_all()).memory_fs(fs).cwd("/run").clock(clock).stdout(out.clone());
    let mut engine = sandlang::Engine::new(config);
    common::rt().block_on(engine.run_source("main.sand", src)).unwrap();
    assert_eq!(out.contents(), "2027-01-15T08:00:00Z\n");
}

#[cfg(feature = "os")]
mod network {
    use super::{ManualClock, run_with};
    use sandlang::network::{Listener, Network, NetworkFuture, Stream};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct MemoryNetwork {
        silent: bool,
        resolved: Mutex<Vec<String>>,
        requests: std::sync::Arc<Mutex<Vec<String>>>,
        listened: Mutex<Vec<SocketAddr>>,
    }

    /// Accepts one connection, whose peer has sent a line.
    struct MemoryListener {
        addr: SocketAddr,
        pending: Mutex<Option<tokio::io::DuplexStream>>,
    }

    impl Listener for MemoryListener {
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            Ok(self.addr)
        }

        fn accept(&self) -> NetworkFuture<'_, (Box<dyn Stream>, SocketAddr)> {
            let pending = self.pending.lock().unwrap().take();
            Box::pin(async move {
                match pending {
                    Some(s) => Ok((Box::new(s) as Box<dyn Stream>, "10.9.9.9:5555".parse().unwrap())),
                    None => std::future::pending().await,
                }
            })
        }
    }

    impl Network for MemoryNetwork {
        fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>> {
            self.resolved.lock().unwrap().push(host.to_string());
            Box::pin(async move { Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), port)]) })
        }

        fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)> {
            let (client, mut server) = tokio::io::duplex(1 << 16);
            let requests = self.requests.clone();
            let silent = self.silent;
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && server.read(&mut byte).await.unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                requests.lock().unwrap().push(String::from_utf8_lossy(&head).into_owned());
                if silent {
                    return std::future::pending::<()>().await;
                }
                let body = "from memory";
                let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = server.write_all(response.as_bytes()).await;
            });
            let peer = addrs[0];
            Box::pin(async move { Ok((Box::new(client) as Box<dyn Stream>, peer)) })
        }

        fn listen(&self, addr: SocketAddr) -> NetworkFuture<'_, Box<dyn Listener>> {
            self.listened.lock().unwrap().push(addr);
            let (server, mut client) = tokio::io::duplex(1 << 16);
            Box::pin(async move {
                client.write_all(b"from memory listener\n").await?;
                Ok(Box::new(MemoryListener { addr, pending: Mutex::new(Some(server)) }) as Box<dyn Listener>)
            })
        }
    }

    #[test]
    fn listeners_are_opened_through_host_network() {
        let network = std::sync::Arc::new(MemoryNetwork::default());
        let src = "import net\nlet l = net.listen(\"10.1.2.3\", 80)?\nprint(l.addr())\nlet c = l.accept()?\nprint(c.peer_addr())\nprint(c.read_line()?)";
        let r = run_with(src, |c| c.network(network.clone()));
        assert_eq!(r.ok_stdout(), "10.1.2.3:80\n10.9.9.9:5555\nSome(\"from memory listener\")\n");
        assert_eq!(*network.listened.lock().unwrap(), ["10.1.2.3:80".parse::<SocketAddr>().unwrap()]);
    }

    #[test]
    fn http_requests_are_routed_through_host_network() {
        let network = std::sync::Arc::new(MemoryNetwork::default());
        let src = "import http\nprint(http.get(\"http://example.test/page\")?.text()?)";
        let r = run_with(src, |c| c.network(network.clone()));
        assert_eq!(r.ok_stdout(), "from memory\n");
        assert_eq!(*network.resolved.lock().unwrap(), ["example.test"]);
        let requests = network.requests.lock().unwrap();
        assert!(requests[0].starts_with("GET /page HTTP/1.1\r\n"), "{requests:?}");
    }

    #[test]
    fn tcp_connections_are_routed_through_host_network() {
        let network = std::sync::Arc::new(MemoryNetwork::default());
        let src = "import net\nlet c = net.connect(\"example.test\", 80)?\nc.write_str(\"GET / HTTP/1.1\\r\\n\\r\\n\")?\nprint(c.peer_addr())\nprint(c.read_all()?.len())";
        let r = run_with(src, |c| c.network(network.clone()));
        assert_eq!(r.ok_stdout(), "93.184.216.34:80\n69\n");
        assert_eq!(*network.resolved.lock().unwrap(), ["example.test"]);
    }

    #[test]
    fn http_timeouts_are_measured_by_host_clock() {
        let network = std::sync::Arc::new(MemoryNetwork { silent: true, ..MemoryNetwork::default() });
        let clock = ManualClock::new();
        let src = "import http\nprint(http.get(\"http://example.test/\", timeout: 30s).map_err(e => e.kind))";
        let started = std::time::Instant::now();
        let r = run_with(src, |c| c.network(network.clone()).clock(clock.clone()));
        assert_eq!(r.ok_stdout(), "Err(\"timeout\")\n");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(clock.sleeps.lock().unwrap().contains(&Duration::from_secs(30)));
    }
}
