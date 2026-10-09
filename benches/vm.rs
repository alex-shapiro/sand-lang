//! VM benchmarks. Run with `cargo bench`.
//!
//! Each benchmark is a Sand program; the harness reports the best and median
//! wall time over several runs (including compilation), the time per
//! operation for benchmarks that declare an operation count, and garbage
//! collector statistics (collections, longest and average pause).

use sandlang::{Config, Engine, GcStats, MemorySink};
use std::time::{Duration, Instant};

struct Bench {
    name: &'static str,
    source: &'static str,
    expected: &'static str,
    /// Number of operations (calls, iterations) for the ns/op column; 0 = none.
    ops: u64,
}

const BENCHES: &[Bench] = &[
    Bench {
        name: "fib(30) recursive calls",
        source: "fn fib(n: int) -> int { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }\nprint(fib(30))",
        expected: "832040\n",
        ops: 2_692_537,
    },
    Bench {
        name: "integer loop 10M",
        source: "var s = 0\nvar i = 0\nwhile i < 10000000 {\n    s += i % 7\n    i += 1\n}\nprint(s)",
        expected: "29999994\n",
        ops: 0,
    },
    Bench {
        name: "for-range float math 5M",
        source: "var x = 0.0\nfor i in 0..5000000 {\n    x = x + i.to_float() * 0.5 - x / 3.0\n}\nprint(x > 0.0)",
        expected: "true\n",
        ops: 0,
    },
    Bench {
        name: "list push/index 1M",
        source: "let xs = []\nfor i in 0..1000000 { xs.push(i) }\nvar t = 0\nfor i in 0..xs.len() { t += xs[i] }\nprint(t)",
        expected: "499999500000\n",
        ops: 0,
    },
    Bench {
        name: "map insert/lookup 200k",
        source: "let m = [:]\nfor i in 0..200000 { m[i.to_str()] = i }\nvar t = 0\nfor i in 0..200000 { t += m[i.to_str()] }\nprint(t)",
        expected: "19999900000\n",
        ops: 0,
    },
    Bench {
        name: "string building 200k",
        source: "let parts = []\nfor i in 0..200000 { parts.push(\"item{i}\") }\nlet s = \",\".join(parts)\nprint(s.len())",
        expected: "2088889\n",
        ops: 0,
    },
    Bench {
        name: "closures map/filter/fold 1M",
        source: "let xs = (0..1000000).to_list()\nlet r = xs.map(x => x * 2).filter(x => x % 3 == 0).fold(0, (a, x) => a + x)\nprint(r)",
        expected: "333333666666\n",
        ops: 0,
    },
    Bench {
        name: "sort 300k ints",
        source: "import random\nrandom.seed(1)\nlet xs = [random.int(0, 1000000) for _ in 0..300000]\nlet s = xs.sorted()\nprint(s[0] <= s[1])",
        expected: "true\n",
        ops: 0,
    },
    Bench {
        name: "struct method dispatch 2M",
        source: "struct V {\n    x: float\n    y: float\n    fn dot(self, o: V) -> float { self.x * o.x + self.y * o.y }\n}\nlet a = V { x: 1.0, y: 2.0 }\nlet b = V { x: 3.0, y: 4.0 }\nvar t = 0.0\nfor i in 0..2000000 { t += a.dot(b) }\nprint(t)",
        expected: "22000000.0\n",
        ops: 0,
    },
    Bench {
        name: "enum match tree sum",
        source: "enum T { Leaf(int), Node(T, T) }\nfn build(d: int) -> T { if d == 0 { T.Leaf(1) } else { T.Node(build(d - 1), build(d - 1)) } }\nfn sum(t: T) -> int { match t { Leaf(v) => v, Node(l, r) => sum(l) + sum(r) } }\nprint(sum(build(20)))",
        expected: "1048576\n",
        ops: 0,
    },
    Bench {
        name: "json parse/stringify 2MB",
        source: "import json\nlet items = [\"\\{\\\"id\\\": {i}, \\\"name\\\": \\\"item {i}\\\", \\\"tags\\\": [1, 2, 3]\\}\" for i in 0..30000]\nlet text = \"[\" + \",\".join(items) + \"]\"\nlet j = json.parse(text).unwrap()\nprint(json.stringify(j).len() > 0)",
        expected: "true\n",
        ops: 0,
    },
    Bench {
        name: "game of life 64x64 x 100",
        source: "let w = 64\nlet h = 64\nvar g = [[(r * 7 + c * 13) % 5 == 0 for c in 0..w] for r in 0..h]\nfor _ in 0..100 {\n    g = [[{\n        var n = 0\n        for dr in -1..=1 { for dc in -1..=1 { if (dr != 0 or dc != 0) and g[(r + dr) % h][(c + dc) % w] { n += 1 } } }\n        n == 3 or (n == 2 and g[r][c])\n    } for c in 0..w] for r in 0..h]\n}\nprint(g.len())",
        expected: "64\n",
        ops: 0,
    },
    Bench {
        name: "task spawn/wait 5k",
        source: "let ts = (0..5000).map(i => spawn i * 2)\nprint(task.all(ts).sum())",
        expected: "24995000\n",
        ops: 0,
    },
    Bench {
        name: "direct calls 10M",
        source: "fn add1(x: int) -> int { x + 1 }\nvar s = 0\nfor i in 0..10000000 { s = add1(s) }\nprint(s)",
        expected: "10000000\n",
        ops: 10_000_000,
    },
    Bench {
        name: "closure calls 10M",
        source: "let k = 3\nlet f = (x: int) => x + k\nvar s = 0\nfor i in 0..10000000 { s = f(s) }\nprint(s)",
        expected: "30000000\n",
        ops: 10_000_000,
    },
    Bench {
        name: "native method calls 10M",
        source: "let x = -1.5\nvar t = 0.0\nfor i in 0..10000000 { t += x.abs() }\nprint(t)",
        expected: "15000000.0\n",
        ops: 10_000_000,
    },
    Bench {
        name: "option map.get ?? 3M",
        source: "let m = [:]\nfor i in 0..1000 { m[i] = i }\nvar t = 0\nfor i in 0..3000000 { t += m.get(i % 2000) ?? 1 }\nprint(t)",
        expected: "750750000\n",
        ops: 3_000_000,
    },
    Bench {
        name: "option list.pop loop 3M",
        source: "let xs = (0..3000000).to_list()\nvar t = 0\nwhile let Some(x) = xs.pop() { t += x }\nprint(t)",
        expected: "4499998500000\n",
        ops: 3_000_000,
    },
    Bench {
        name: "option ? chains 3M",
        source: "fn half(x: int) -> int? { if x % 2 == 0 { Some(x / 2) } else { None } }\nfn q(x: int) -> int? {\n    let a = half(x)?\n    let b = half(a)?\n    Some(b + 1)\n}\nvar t = 0\nfor i in 0..3000000 { t += q(i) ?? 0 }\nprint(t)",
        expected: "281250375000\n",
        ops: 3_000_000,
    },
    Bench {
        name: "gc: 1M live + 10M churn",
        source: "struct P {\n    x: int\n    y: int\n}\nlet live = [P { x: i, y: i } for i in 0..1000000]\nvar t = 0\nfor i in 0..10000000 {\n    let p = P { x: i, y: 1 }\n    t += p.y\n}\nprint(t + live.len())",
        expected: "11000000\n",
        ops: 10_000_000,
    },
    Bench {
        name: "gc: 1M live, mutated + 5M churn",
        source: "struct P {\n    x: int\n    y: int\n}\nlet live = [P { x: i, y: i } for i in 0..1000000]\nvar t = 0\nfor i in 0..5000000 {\n    let p = P { x: i, y: 1 }\n    if i % 100 == 0 { live[i % 1000000] = p }\n    t += p.y\n}\nprint(t + live.len())",
        expected: "6000000\n",
        ops: 5_000_000,
    },
];

fn run_once(src: &str) -> (Duration, String, GcStats) {
    let out = MemorySink::new();
    let mut engine = Engine::new(Config::new().stdout(out.clone()));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let start = Instant::now();
    rt.block_on(engine.run_source("bench.sand", src)).unwrap();
    (start.elapsed(), out.contents(), engine.gc_stats())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let quick = std::env::var("BENCH_QUICK").is_ok();
    println!(
        "{:<34} {:>10} {:>10} {:>8} {:>5} {:>6} {:>9} {:>9} {:>9}",
        "benchmark", "best", "median", "ns/op", "gcs", "majors", "max-pause", "max-minor", "avg-pause"
    );
    for b in BENCHES {
        if let Some(f) = &filter
            && !b.name.contains(f.as_str())
        {
            continue;
        }
        let runs = if quick { 1 } else { std::env::var("BENCH_RUNS").ok().and_then(|n| n.parse().ok()).unwrap_or(5usize).max(1) };
        let mut times = Vec::new();
        let mut gc = GcStats::default();
        for _ in 0..runs {
            let (t, out, s) = run_once(b.source);
            assert_eq!(out, b.expected, "{}", b.name);
            times.push(t);
            gc.collections += s.collections;
            gc.total_pause += s.total_pause;
            gc.max_pause = gc.max_pause.max(s.max_pause);
            gc.majors += s.majors;
            gc.max_minor_pause = gc.max_minor_pause.max(s.max_minor_pause);
        }
        times.sort();
        let ns_op = if b.ops > 0 { format!("{:.1}", times[0].as_secs_f64() * 1e9 / b.ops as f64) } else { "-".into() };
        let avg = if gc.collections > 0 { gc.total_pause / gc.collections as u32 } else { Duration::ZERO };
        println!(
            "{:<34} {:>8.1}ms {:>8.1}ms {:>8} {:>5} {:>6} {:>7.2}ms {:>7.2}ms {:>7.2}ms",
            b.name,
            ms(times[0]),
            ms(times[times.len() / 2]),
            ns_op,
            gc.collections / runs as u64,
            gc.majors / runs as u64,
            ms(gc.max_pause),
            ms(gc.max_minor_pause),
            ms(avg)
        );
    }
}
