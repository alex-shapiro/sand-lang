//! Acceptance tests for a host's own clock and network.

mod common;

use common::run_with;
use sandlang::platform::{Clock, ClockFuture, Instant, SystemTime, UNIX_EPOCH};
use sandlang::{Error, LimitKind, Limits};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A clock that only moves when the engine works or sleeps.
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
fn scripts_read_the_time_of_the_hosts_clock() {
    let clock = ManualClock::new();
    let src = "import time\nimport uuid\nprint(time.now().to_iso())\nprint(uuid.v7()[0..8])";
    let r = run_with(src, |c| c.clock(clock.clone()));
    assert_eq!(r.ok_stdout(), "2027-01-15T08:00:00Z\n01a3185c\n");
}

#[test]
fn sleeps_wait_on_the_hosts_timers() {
    let clock = ManualClock::new();
    let src = "import task\nimport time\nlet start = time.monotonic()\ntask.sleep(5s)\nprint(start.elapsed() >= 5s)";
    let r = run_with(src, |c| c.clock(clock.clone()));
    assert_eq!(r.ok_stdout(), "true\n");
    assert!(clock.sleeps.lock().unwrap().contains(&Duration::from_secs(5)));
}

#[test]
fn the_time_limit_is_measured_on_the_hosts_clock() {
    let clock = ManualClock::new();
    let limits = Limits { max_wall_time: Some(Duration::from_millis(50)), ..Limits::default() };
    let r = run_with("var n = 0\nwhile true { n += 1 }", |c| c.clock(clock.clone()).limits(limits));
    assert!(matches!(r.result, Err(Error::Limit(LimitKind::WallTime))), "{:?}", r.result);
    let elapsed = clock.elapsed();
    assert!(elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(60), "{elapsed:?}");
}

#[cfg(feature = "os")]
mod network {
    use super::run_with;
    use sandlang::network::{Network, NetworkFuture, Stream};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers every connection with one HTTP response from memory.
    #[derive(Default)]
    struct MemoryNetwork {
        resolved: Mutex<Vec<String>>,
        requests: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl Network for MemoryNetwork {
        fn resolve<'a>(&'a self, host: &'a str, port: u16) -> NetworkFuture<'a, Vec<SocketAddr>> {
            self.resolved.lock().unwrap().push(host.to_string());
            Box::pin(async move { Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), port)]) })
        }

        fn connect<'a>(&'a self, addrs: &'a [SocketAddr]) -> NetworkFuture<'a, (Box<dyn Stream>, SocketAddr)> {
            let (client, mut server) = tokio::io::duplex(1 << 16);
            let requests = self.requests.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && server.read(&mut byte).await.unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                requests.lock().unwrap().push(String::from_utf8_lossy(&head).into_owned());
                let body = "from memory";
                let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = server.write_all(response.as_bytes()).await;
            });
            let peer = addrs[0];
            Box::pin(async move { Ok((Box::new(client) as Box<dyn Stream>, peer)) })
        }
    }

    #[test]
    fn requests_go_through_the_hosts_network() {
        let network = std::sync::Arc::new(MemoryNetwork::default());
        let src = "import http\nprint(http.get(\"http://example.test/page\")?.text()?)";
        let r = run_with(src, |c| c.network(network.clone()));
        assert_eq!(r.ok_stdout(), "from memory\n");
        assert_eq!(*network.resolved.lock().unwrap(), ["example.test"]);
        let requests = network.requests.lock().unwrap();
        assert!(requests[0].starts_with("GET /page HTTP/1.1\r\n"), "{requests:?}");
    }
}
