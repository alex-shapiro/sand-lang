import sys, time, statistics, asyncio, warnings
sys.path.insert(0, sys.argv[1])
import cbx
print("==", sys.version.split()[0])
def inc(x): return x + 1
N = 5_000_000
base, s2s, n2s = [], [], []
for _ in range(5):
    t = time.perf_counter(); acc0 = 0
    for i in range(N): acc0 += i + 1
    base.append((time.perf_counter() - t) / N * 1e9)
    t = time.perf_counter(); acc = 0
    for i in range(N): acc += inc(i)
    s2s.append((time.perf_counter() - t) / N * 1e9)
    t = time.perf_counter(); acc2 = cbx.repeat(inc, N)
    n2s.append((time.perf_counter() - t) / N * 1e9)
    assert acc == acc2
print(f"E1 loop without call: {statistics.median(base):.1f} ns/iter; script->script call loop: {statistics.median(s2s):.1f} ns/iter; native->script callback loop: {statistics.median(n2s):.1f} ns/iter")
# E2: depth of native<->script recursion vs script-only recursion (default limits)
depth = 0
def f(d):
    global depth; depth = d
    return cbx.bounce(f, d + 1)
try: f(0)
except BaseException as e: print(f"E2 native<->script max depth: {depth} | {type(e).__name__}: {e}")
sdepth = 0
def g(d):
    global sdepth; sdepth = d
    return 1 + g(d + 1)
try: g(0)
except BaseException as e: print(f"E2 script-only max depth: {sdepth} | {type(e).__name__}: {e}")
# E2b: raise the recursion limit far above the C stack: does the process survive?
# E3: a callback cannot suspend its native caller
def gen():
    r = cbx.call(lambda: (yield "suspend?"))
    yield ("callback returned", type(r).__name__)
print("E3 generator yield inside callback:", list(gen()))
async def handler():
    await asyncio.sleep(0); return "handled"
async def main():
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        r = cbx.call(handler)
        print("E3 async handler from native returns:", type(r).__name__)
        del r
        import gc; gc.collect()
        print("E3 warning:", [str(x.message) for x in w])
asyncio.run(main())
# E4: callback error propagates as an exception; the native's cleanup runs
try: cbx.with_resource(lambda: 1 / 0)
except ZeroDivisionError as e: print("E4 callback error:", repr(e), "acquired/cleaned:", cbx.counts())
# E6: comparator/key that mutates the list during sort
l = list(range(100))
try: l.sort(key=lambda x: (l.append(0), x)[1]); print("E6 sort mutation: no error")
except ValueError as e: print("E6 sort with mutating key:", repr(e))
# E8: no isolation: any callback can reach the host process through introspection
def cb(): pass
os_mod = cb.__globals__["__builtins__"].__import__("os") if isinstance(cb.__globals__["__builtins__"], dict) else cb.__globals__["__builtins__"].__import__("os")
print("E8 callback reaches os module:", os_mod.getpid() > 0)
# E8b: removing builtins does not sandbox: recover them via object graph
code = "().__class__.__base__.__subclasses__()"
r = eval(code, {"__builtins__": {}})
print("E8b subclasses reachable with empty builtins:", len(r) > 100)
