use std::time::Instant;

/// Median of `runs` timings of `f`, in nanoseconds per iteration.
pub fn median_ns(runs: usize, iters: u64, mut f: impl FnMut()) -> f64 {
    let mut v: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as f64 / iters as f64
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

pub const N: u64 = 5_000_000;
