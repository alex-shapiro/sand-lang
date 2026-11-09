"""Summarizes results/overhead/*.txt: per-program median time and geomean ratio vs base."""
import glob, math, re, sys
d = sys.argv[1] if len(sys.argv) > 1 else "results/overhead"
runs = {}
for f in sorted(glob.glob(f"{d}/*-round*.txt")):
    name = f.split("/")[-1].split("-round")[0]
    for line in open(f):
        m = re.match(r"(\w+)\s+(\d+)ms", line)
        if m:
            runs.setdefault(name, {}).setdefault(m[1], []).append(int(m[2]))
best = {n: {w: min(t) for w, t in ws.items()} for n, ws in runs.items()}
progs = sorted(best["base"])
print(f"{'program':<15}" + "".join(f"{n:>9}" for n in ["base", "p2", "p1"]) + "   p2/base  p1/base")
for p in progs:
    b = best["base"][p]
    print(f"{p:<15}" + "".join(f"{best[n][p]:>7}ms" for n in ["base", "p2", "p1"]) + f"   {best['p2'][p]/b:7.3f}  {best['p1'][p]/b:7.3f}")
for n in ["p2", "p1"]:
    g = math.exp(sum(math.log(best[n][p] / best["base"][p]) for p in progs) / len(progs))
    print(f"geomean {n}/base: {g:.3f} (best of the per-round medians)")

# Noise: spread of the baseline's own per-round medians.
rounds = {}
for f in sorted(glob.glob(f"{d}/base-round*.txt")):
    for line in open(f):
        m = re.match(r"(\w+)\s+(\d+)ms", line)
        if m:
            rounds.setdefault(f, {})[m[1]] = int(m[2])
fs = sorted(rounds)
if len(fs) >= 2:
    worst = max(max(rounds[a][p] for a in fs) / min(rounds[a][p] for a in fs) for p in progs)
    print(f"noise: largest base-vs-base spread for one program across rounds: {worst:.3f}")
