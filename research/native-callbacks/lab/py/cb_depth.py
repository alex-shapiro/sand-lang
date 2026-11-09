import sys
sys.path.insert(0, sys.argv[1]); import cbx
sys.setrecursionlimit(1_000_000)
print("==", sys.version.split()[0], "recursionlimit=1e6")
depth = 0
def f(d):
    global depth; depth = d
    return cbx.bounce(f, d + 1)
try: f(0)
except BaseException as e: print(f"E2b native<->script max depth: {depth} | {type(e).__name__}: {e}")
sdepth = 0
def g(d):
    global sdepth; sdepth = d
    return 1 + g(d + 1)
try: g(0)
except BaseException as e: print(f"E2b script-only max depth: {sdepth} | {type(e).__name__}: {e}")
