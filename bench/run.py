#!/usr/bin/env python3.14
"""Benchmarks Sand against Lua 5.5 and CPython 3.14.

    cargo build --release
    python3.14 bench/run.py [--suite programs|examples] [--only NAME[,NAME...]] [--runs N]

Two suites:

- `programs`: the programs in `programs/`, each written in Sand, Lua and
  Python (`NAME.sand`, `NAME.lua`, `NAME.py`), generating their own inputs.
- `examples`: the programs in `examples/` on scaled-up generated inputs,
  against their ports in `ports/python/` and (where Lua's standard library
  allows: no sockets, crypto or directory listing) `ports/lua/`.

Each workload runs once untimed per implementation (warm-up and output
check), then N times, alternating implementations. Every implementation's
output must be identical to Sand's; the exit status is 1 if any differ.
"""

import argparse
import hashlib
import os
import random
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

PYTHON_BASELINE = (3, 14)
LUA_BASELINE = "Lua 5.5"
if sys.version_info[:2] != PYTHON_BASELINE:
    sys.exit("run.py runs on Python 3.14 (python3.14 bench/run.py)")

import gen
import servers

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
EXAMPLES = ROOT / "examples"
PY = HERE / "ports" / "python"
LUA = HERE / "ports" / "lua"
PROGRAMS = HERE / "programs"

IMPLS = ("sand", "lua", "python")


@dataclass
class Step:
    """One process invocation: the program for each implementation that has one."""

    progs: dict[str, Path]
    args: list[str] = field(default_factory=list)
    flags: list[str] = field(default_factory=list)  # Sand sandbox flags
    stdin: Path | None = None


def step(sand: Path, py: Path | None = None, lua: Path | None = None, args=(), flags=(), stdin=None) -> Step:
    progs = {"sand": sand}
    if py:
        progs["python"] = py
    if lua:
        progs["lua"] = lua
    return Step(progs, list(args), list(flags), stdin)


@dataclass
class Workload:
    name: str
    suite: str
    steps: Callable[[str], list[Step]]  # impl -> steps (run in sequence, timed together)
    impls: tuple[str, ...] = IMPLS  # the implementations with ports
    cwd: Path | None = None
    env: dict[str, str] | None = None
    setup: Callable[[str], None] | None = None  # untimed, before every run
    check: Callable[[str], bytes] | None = None  # untimed, after every run: extra output to compare
    custom: Callable[[str], tuple[float, bytes]] | None = None  # replaces the default runner


class Bench:
    def __init__(self, exe: dict[str, str], tmp: Path):
        self.exe = exe
        self.tmp = tmp
        self.cleanups: list[Callable[[], None]] = []

    def command(self, impl: str, s: Step) -> list[str]:
        prog = str(s.progs[impl])
        if impl == "sand":
            return [self.exe["sand"], "run", prog, "--max-memory=8G", *s.flags, "--", *s.args]
        return [self.exe[impl], prog, *s.args]

    def env(self, w: Workload | None = None) -> dict[str, str]:
        env = dict(os.environ)
        env["PYTHONUTF8"] = "1"
        env.pop("PYTHONDONTWRITEBYTECODE", None)
        env.update((w and w.env) or {})
        return env

    def run_once(self, w: Workload, impl: str) -> tuple[float, bytes]:
        if w.setup:
            w.setup(impl)
        if w.custom:
            elapsed, out = w.custom(impl)
        else:
            out_parts = []
            env = self.env(w)
            elapsed = 0.0
            for s in w.steps(impl):
                cmd = self.command(impl, s)
                stdin = open(s.stdin, "rb") if s.stdin else subprocess.DEVNULL
                try:
                    start = time.perf_counter()
                    p = subprocess.run(cmd, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=w.cwd, env=env)
                    elapsed += time.perf_counter() - start
                finally:
                    if s.stdin:
                        stdin.close()
                if p.returncode != 0:
                    raise RuntimeError(
                        f"{w.name} ({impl}) exited with {p.returncode}\n$ {' '.join(cmd)}\n{p.stderr.decode(errors='replace')[-2000:]}"
                    )
                out_parts.append(p.stdout)
            out = b"".join(out_parts)
        if w.check:
            out += w.check(impl)
        return elapsed, out


# ---------------------------------------------------------------------------
# Workloads


def sha(path: Path) -> bytes:
    return hashlib.sha256(path.read_bytes()).hexdigest().encode()


def programs() -> list[Workload]:
    ws = []
    for sand in sorted(PROGRAMS.glob("*.sand")):
        s = step(sand, sand.with_suffix(".py"), sand.with_suffix(".lua"))
        impls = tuple(i for i in IMPLS if i in s.progs and s.progs[i].exists())
        ws.append(Workload(sand.stem, "programs", lambda impl, s=s: [s], impls))
    return ws


def examples(b: Bench) -> list[Workload]:
    tmp = b.tmp
    ws: list[Workload] = []

    def simple(name, s: Step, **kw):
        impls = tuple(i for i in IMPLS if i in s.progs)
        ws.append(Workload(name, "examples", lambda impl: [s], impls, **kw))

    def ported(name: str) -> tuple[Path, Path, Path | None]:
        lua = LUA / f"{name}.lua"
        return EXAMPLES / f"{name}.sand", PY / f"{name}.py", lua if lua.exists() else None

    # life ---------------------------------------------------------------
    simple("life", step(*ported("life"), args=["12000"]))
    life = gen.write_life_bench(tmp)
    simple("life_big", step(life["sand"], life["python"], life["lua"], args=["150"]))

    # calc / adventure (stdin) ---------------------------------------------
    calc_in = tmp / "calc.txt"
    calc_in.write_text(gen.calc_script())
    simple("calc", step(*ported("calc"), flags=["--allow-stdin"], stdin=calc_in))

    adv_in = tmp / "adventure.txt"
    adv_in.write_text(gen.adventure_script())
    simple("adventure", step(*ported("adventure"), flags=["--allow-stdin"], stdin=adv_in))

    # template ---------------------------------------------------------------
    tdir = tmp / "template"
    tdir.mkdir()
    import json

    (tdir / "render.tmpl").write_text(gen.TEMPLATE_RENDER)
    (tdir / "render.json").write_text(json.dumps(gen.template_data(), ensure_ascii=False))
    (tdir / "compile.tmpl").write_text(gen.template_compile_source())
    (tdir / "compile.json").write_text(json.dumps(gen.TEMPLATE_COMPILE_DATA))
    for kind in ("render", "compile"):
        simple(
            f"template_{kind}",
            step(*ported("template"), args=[str(tdir / f"{kind}.tmpl"), str(tdir / f"{kind}.json")], flags=[f"--allow-read={tdir}"]),
        )

    # ledger -------------------------------------------------------------------
    ledger = gen.write_ledger_bench(tmp)
    simple("ledger", step(ledger["sand"], ledger["python"], ledger["lua"], args=["food"]))

    # search_index ---------------------------------------------------------------
    # `sand run` resolves relative paths against the script's directory, so
    # every path passed to the programs is absolute.
    sdir = tmp / "search"
    sdir.mkdir()
    corpus = gen.search_corpus(sdir)
    sflags = [f"--allow-read={sdir}", f"--allow-write={sdir}"]
    sprog = (EXAMPLES / "search_index.sand", PY / "search_index.py")

    def idx(impl: str, kind: str = "base") -> Path:
        return sdir / f"{impl}.{kind}.json"

    def search_step(*args: str) -> list[Step]:
        return [step(*sprog, args=args, flags=sflags)]

    def remove_index(impl: str):
        idx(impl).unlink(missing_ok=True)

    def search(name, steps, **kw):
        ws.append(Workload(name, "examples", steps, ("sand", "python"), cwd=sdir, **kw))

    search(
        "search_index_index",
        lambda impl: search_step("index", str(idx(impl)), str(corpus)),
        setup=remove_index,
        # Both implementations must write the same index file.
        check=lambda impl: b"index sha256 " + sha(idx(impl)) + b"\n",
    )
    # Each search is a fresh process that loads the whole index, as in real use.
    queries = gen.search_queries()
    search("search_index_search", lambda impl: [s for q in queries for s in search_step("search", str(idx(impl)), *q)])
    add_files = [str(sdir / f) for f in gen.search_add_files(corpus)]

    def copy_index(impl: str):
        shutil.copyfile(idx(impl), idx(impl, "work"))

    search(
        "search_index_add",
        lambda impl: search_step("add", str(idx(impl, "work")), *add_files),
        setup=copy_index,
        check=lambda impl: b"index sha256 " + sha(idx(impl, "work")) + b"\n",
    )
    search("search_index_stats", lambda impl: search_step("stats", str(idx(impl))) * 4)

    # http_fetch -------------------------------------------------------------------
    pages = {f"/page/{i}": gen.html_page(i).encode() for i in range(48)}
    page_server = servers.PageServer(pages)
    b.cleanups.append(page_server.close)
    base = f"http://{page_server.addr}"
    urls = [f"{base}/old"] + [f"{base}/page/{i}" for i in range(1, 48)] + [f"{base}/missing", f"{base}/data.json"]
    simple("http_fetch", step(EXAMPLES / "http_fetch.sand", PY / "http_fetch.py", args=urls, flags=[f"--allow-net={page_server.addr}"]))

    # chat_server ---------------------------------------------------------------------
    def chat(impl: str) -> tuple[float, bytes]:
        s = step(EXAMPLES / "chat_server.sand", PY / "chat_server.py", args=["0"], flags=["--allow-listen=127.0.0.1:0"])
        cmd = b.command(impl, s)
        start = time.perf_counter()
        p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=b.env())
        try:
            line = servers.wait_for_line(p.stdout, b"listening on ", 10, lambda: f"chat server ({impl}) did not start")
            addr = line.decode().split()[-1]
            out = servers.chat_session(addr, clients=CHAT_CLIENTS, messages=CHAT_MESSAGES)
            elapsed = time.perf_counter() - start
        finally:
            p.send_signal(signal.SIGINT)
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
        return elapsed, out

    ws.append(Workload("chat_server", "examples", lambda impl: [], ("sand", "python"), custom=chat))

    # s3 -------------------------------------------------------------------------------
    s3 = servers.FakeS3()
    b.cleanups.append(s3.close)
    s3dir = tmp / "s3"
    s3dir.mkdir()
    s3env = {
        "AWS_ACCESS_KEY_ID": "TESTKEY",
        "AWS_SECRET_ACCESS_KEY": "secret",
        "S3_ENDPOINT": f"http://{s3.addr}",
        "AWS_REGION": "us-east-1",
    }
    s3flags = [
        f"--allow-net={s3.addr}",
        f"--allow-read={s3dir}",
        f"--allow-write={s3dir}",
        "--allow-env=AWS_ACCESS_KEY_ID,AWS_SECRET_ACCESS_KEY,AWS_SESSION_TOKEN,AWS_REGION,S3_ENDPOINT",
    ]
    os.environ.pop("AWS_SESSION_TOKEN", None)
    cli = (EXAMPLES / "s3" / "cli.sand", PY / "s3" / "cli.py")

    def s3cmd(*args: str) -> Step:
        return step(*cli, args=args, flags=s3flags)

    blob = random.Random(8).randbytes(S3_BLOB_MB << 20)
    (s3dir / "blob.bin").write_bytes(blob)

    def s3_workload(name, steps, setup_objects=None, check=None):
        ws.append(
            Workload(
                name,
                "examples",
                steps,
                ("sand", "python"),
                cwd=s3dir,
                env=s3env,
                setup=lambda impl: s3.reset(setup_objects() if setup_objects else None),
                check=check,
            )
        )

    s3_workload("s3_put", lambda impl: [s3cmd("put", "bucket", "big/blob.bin", str(s3dir / "blob.bin"))])
    s3_workload(
        "s3_get",
        lambda impl: [s3cmd("get", "bucket", "big/blob.bin", str(s3dir / "down.bin"))] * 4,
        setup_objects=lambda: {"bucket/big/blob.bin": blob},
        check=lambda impl: b"download sha256 " + sha(s3dir / "down.bin") + b"\n",
    )
    rng = random.Random(9)
    listing = {
        f"bucket/logs/{2020 + i % 5}/{i % 12 + 1:02}/app-{i:06}{'&x' if i % 50 == 0 else ''}.log": b"x" * rng.randint(0, 3000)
        for i in range(S3_LIST_KEYS)
    }
    listing["bucket/other/file.txt"] = b"other"
    s3_workload("s3_ls", lambda impl: [s3cmd("ls", "bucket", "logs/")], setup_objects=lambda: listing)

    small = s3dir / "small.txt"
    small.write_text("hello from the benchmark\n")

    def small_ops(impl: str) -> list[Step]:
        steps = []
        for i in range(6):
            steps.append(s3cmd("put", "bucket", f"notes/n{i}.txt", str(small)))
            steps.append(s3cmd("stat", "bucket", f"notes/n{i}.txt"))
            steps.append(s3cmd("cat", "bucket", f"notes/n{i}.txt"))
        steps.append(s3cmd("ls", "bucket", "notes/"))
        for i in range(6):
            steps.append(s3cmd("rm", "bucket", f"notes/n{i}.txt"))
        steps.append(s3cmd("stat", "bucket", "notes/n0.txt"))
        return steps

    s3_workload("s3_small_ops", small_ops)
    return ws


# Scale knobs for the network workloads.
CHAT_CLIENTS = 24
CHAT_MESSAGES = 600
S3_BLOB_MB = 64
S3_LIST_KEYS = 8000


def prepare(b: Bench, w: Workload):
    """One-off, untimed preparation."""
    if w.name in ("search_index_search", "search_index_add", "search_index_stats"):
        # These need each implementation's own index of the corpus.
        sdir = b.tmp / "search"
        for impl in w.impls:
            if not (sdir / f"{impl}.base.json").exists():
                s = step(
                    EXAMPLES / "search_index.sand",
                    PY / "search_index.py",
                    args=["index", str(sdir / f"{impl}.base.json"), str(sdir / "corpus")],
                    flags=[f"--allow-read={sdir}", f"--allow-write={sdir}"],
                )
                subprocess.run(b.command(impl, s), cwd=sdir, check=True, stdout=subprocess.DEVNULL, env=b.env(w))


def first_difference(impl: str, a: bytes, b: bytes) -> str:
    la, lb = a.splitlines(), b.splitlines()
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            return f"line {i + 1}:\n  sand: {x[:200]!r}\n  {impl}: {y[:200]!r}"
    return f"lengths differ: sand {len(la)} lines / {len(a)} bytes, {impl} {len(lb)} lines / {len(b)} bytes"


def version(cmd: list[str]) -> str:
    p = subprocess.run(cmd, capture_output=True, text=True)
    return (p.stdout + p.stderr).strip().splitlines()[0]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--suite", choices=("all", "programs", "examples"), default="all")
    parser.add_argument("--only", help="comma-separated workload names or prefixes (e.g. life,search_index)")
    parser.add_argument("--runs", type=int, default=5, help="timed runs per implementation (default 5)")
    parser.add_argument("--impls", default=",".join(IMPLS), help="implementations to run (default sand,lua,python)")
    parser.add_argument("--sand", default=str(ROOT / "target" / "release" / "sand"), help="sand binary")
    parser.add_argument("--lua", default="lua5.5", help="Lua 5.5 interpreter")
    parser.add_argument("--python", default=sys.executable, help="Python 3.14 interpreter (default: the one running this script)")
    parser.add_argument("--any-version", action="store_true", help="allow other Lua and Python versions (ratios are then not comparable)")
    parser.add_argument("--keep", action="store_true", help="keep the generated inputs and print where they are")
    args = parser.parse_args()

    impls = tuple(i for i in IMPLS if i in args.impls.split(","))
    if "sand" not in impls:
        print("--impls must include sand (outputs are checked against it)", file=sys.stderr)
        return 2
    exe = {"sand": args.sand}
    if not Path(args.sand).exists():
        print(f"{args.sand} not found: run `cargo build --release` first", file=sys.stderr)
        return 2
    for impl, flag, probe, want in (("lua", args.lua, ["-v"], LUA_BASELINE), ("python", args.python, ["--version"], "Python 3.14")):
        if impl not in impls:
            continue
        path = shutil.which(flag)
        if not path:
            print(f"{flag} not found (use --{impl})", file=sys.stderr)
            return 2
        v = version([path, *probe])
        if not v.startswith(want) and not args.any_version:
            print(f"{path} is {v}; the baseline is {want} (use --{impl}, or --any-version)", file=sys.stderr)
            return 2
        exe[impl] = path
        print(f"{impl}: {path} ({v})")

    tmp = Path(tempfile.mkdtemp(prefix="sand-bench-")).resolve()
    b = Bench(exe, tmp)
    failed = False
    rows = []
    try:
        all_ws = []
        if args.suite in ("all", "programs"):
            all_ws += programs()
        if args.suite in ("all", "examples"):
            print(f"generating inputs in {tmp} ...", file=sys.stderr)
            all_ws += examples(b)
        if args.only:
            wanted = [s.strip() for s in args.only.split(",") if s.strip()]
            selected = [w for w in all_ws if any(w.name == s or w.name.startswith(s) for s in wanted)]
            if not selected:
                print(f"no workload matches {args.only}; available: {', '.join(w.name for w in all_ws)}", file=sys.stderr)
                return 2
        else:
            selected = all_ws

        others = [i for i in impls if i != "sand"]
        header = f"{'workload':<22}{'sand':>10}" + "".join(f"{i:>10}" for i in others) + "".join(f"{'/' + i:>9}" for i in others)
        print(header)
        print("-" * len(header))
        for w in selected:
            prepare(b, w)
            run_impls = [i for i in impls if i in w.impls]
            times: dict[str, list[float]] = {impl: [] for impl in run_impls}
            outputs: dict[str, bytes] = {}
            mismatch = None
            try:
                for i in range(args.runs + 1):  # run 0 is the untimed warm-up
                    for impl in run_impls:
                        elapsed, out = b.run_once(w, impl)
                        if impl not in outputs:
                            outputs[impl] = out
                        elif out != outputs[impl]:
                            mismatch = f"{impl} output changed between runs: " + first_difference(impl, outputs[impl], out)
                        if i > 0:
                            times[impl].append(elapsed)
                    if i == 0:
                        for impl in run_impls[1:]:
                            if outputs[impl] != outputs["sand"]:
                                mismatch = "outputs differ, " + first_difference(impl, outputs["sand"], outputs[impl])
                    if mismatch:
                        break
            except Exception as e:  # noqa: BLE001 - report and continue with the next workload
                failed = True
                print(f"{w.name:<22}ERROR: {e}")
                continue
            if mismatch:
                failed = True
                for impl in run_impls:
                    (tmp / f"{w.name}.{impl}.out").write_bytes(outputs.get(impl, b""))
                print(f"{w.name:<22}MISMATCH: {mismatch}\n{'':<22}(outputs saved to {tmp}/{w.name}.*.out)")
                args.keep = True
                continue
            med = {impl: statistics.median(t) for impl, t in times.items()}
            ratios = {impl: med["sand"] / med[impl] for impl in others if impl in med}
            rows.append((w, ratios))
            cells = "".join(f"{med[i] * 1e3:>8.0f}ms" if i in med else f"{'-':>10}" for i in others)
            rcells = "".join(f"{ratios[i]:>9.2f}" if i in ratios else f"{'-':>9}" for i in others)
            print(f"{w.name:<22}{med['sand'] * 1e3:>8.0f}ms{cells}{rcells}", flush=True)
        if rows:
            print("-" * len(header))
            for suite in ("programs", "examples"):
                srows = [r for w, r in rows if w.suite == suite]
                if not srows:
                    continue
                means = ""
                for impl in others:
                    rs = [r[impl] for r in srows if impl in r]
                    means += f"{statistics.geometric_mean(rs):>9.2f}" if rs else f"{'-':>9}"
                print(f"{'geomean ' + suite:<22}{'':>{10 * len(others) + 10}}{means}")
            print("times are medians; /lua and /python are sand time / that time (< 1: Sand is faster)")
    finally:
        for c in b.cleanups:
            c()
        if args.keep:
            print(f"inputs kept in {tmp}", file=sys.stderr)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    if failed:
        print("FAILED: some workloads errored or produced different output", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
