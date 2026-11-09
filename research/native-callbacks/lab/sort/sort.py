import sys, time, statistics, functools
N = 200_000
seed = 1; data = []
for _ in range(N):
    seed = (seed * 1103515245 + 12345) % 2147483648; data.append(seed)
def med(f):
    ts = []
    for _ in range(5):
        c = list(data); t = time.perf_counter(); f(c); ts.append(time.perf_counter() - t)
    return statistics.median(ts) * 1000
cmp = functools.cmp_to_key(lambda a, b: -1 if a < b else (1 if b < a else 0))
print(f"E6 Python {sys.version.split()[0]} sort {N} ints: list.sort(key=cmp_to_key(py comparator)) {med(lambda c: c.sort(key=cmp)):.1f} ms; list.sort(key=lambda) {med(lambda c: c.sort(key=lambda x: x)):.1f} ms; list.sort() {med(lambda c: c.sort()):.1f} ms")
