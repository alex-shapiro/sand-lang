#!/usr/bin/env python3.14
"""Benchmarks every example program against its CPython port.

    cargo build --release
    python3.14 bench/examples/run.py [--only NAME[,NAME...]] [--runs N]

For each workload the inputs are generated into a temporary directory, then
the Sand program (`target/release/sand run ...`) and the Python port
(`bench/examples/python/...`) are each run once untimed (warm-up and output
check) and then N times, alternating. Every run's output must be identical
between the two implementations; the exit status is 1 if any differ.
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

BASELINE = (3, 14)
if sys.version_info[:2] != BASELINE:
    sys.exit(f"run.py runs on Python {BASELINE[0]}.{BASELINE[1]} (python{BASELINE[0]}.{BASELINE[1]} bench/examples/run.py)")

import gen
import servers

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
EXAMPLES = ROOT / "examples"
PORTS = HERE / "python"

IMPLS = ("sand", "python")


@dataclass
class Step:
    """One process invocation."""

    sand: Path  # Sand program
    python: Path  # its Python port
    args: list[str] = field(default_factory=list)
    flags: list[str] = field(default_factory=list)  # Sand sandbox flags
    stdin: Path | None = None


@dataclass
class Workload:
    name: str
    steps: Callable[[str], list[Step]]  # impl -> steps (run in sequence, timed together)
    cwd: Path | None = None
    env: dict[str, str] | None = None
    setup: Callable[[str], None] | None = None  # untimed, before every run
    check: Callable[[str], bytes] | None = None  # untimed, after every run: extra output to compare
    custom: Callable[[str], tuple[float, bytes]] | None = None  # replaces the default runner


class Bench:
    def __init__(self, sand: Path, python: str, tmp: Path):
        self.sand = sand
        self.python = python
        self.tmp = tmp
        self.cleanups: list[Callable[[], None]] = []

    def command(self, impl: str, step: Step) -> list[str]:
        if impl == "sand":
            return [str(self.sand), "run", str(step.sand), "--max-memory=8G", *step.flags, "--", *step.args]
        return [self.python, str(step.python), *step.args]

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
            for step in w.steps(impl):
                cmd = self.command(impl, step)
                stdin = open(step.stdin, "rb") if step.stdin else subprocess.DEVNULL
                try:
                    start = time.perf_counter()
                    p = subprocess.run(cmd, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=w.cwd, env=env)
                    elapsed += time.perf_counter() - start
                finally:
                    if step.stdin:
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


def workloads(b: Bench) -> list[Workload]:
    tmp = b.tmp
    ws: list[Workload] = []

    def simple(name, sand, python, args=(), flags=(), stdin=None, **kw):
        step = Step(sand, python, list(args), list(flags), stdin)
        ws.append(Workload(name, lambda impl: [step], **kw))

    # life ---------------------------------------------------------------
    simple("life", EXAMPLES / "life.sand", PORTS / "life.py", ["12000"])
    life_sand, life_py = gen.write_life_bench(tmp)
    simple("life_big", life_sand, life_py, ["150"])

    # calc / adventure (stdin) ---------------------------------------------
    calc_in = tmp / "calc.txt"
    calc_in.write_text(gen.calc_script())
    simple("calc", EXAMPLES / "calc.sand", PORTS / "calc.py", flags=["--allow-stdin"], stdin=calc_in)

    adv_in = tmp / "adventure.txt"
    adv_in.write_text(gen.adventure_script())
    simple("adventure", EXAMPLES / "adventure.sand", PORTS / "adventure.py", flags=["--allow-stdin"], stdin=adv_in)

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
            EXAMPLES / "template.sand",
            PORTS / "template.py",
            [str(tdir / f"{kind}.tmpl"), str(tdir / f"{kind}.json")],
            [f"--allow-read={tdir}"],
        )

    # ledger -------------------------------------------------------------------
    ledger_sand, ledger_py = gen.write_ledger_bench(tmp)
    simple("ledger", ledger_sand, ledger_py, ["food"])

    # search_index ---------------------------------------------------------------
    # `sand run` resolves relative paths against the script's directory, so
    # every path passed to the programs is absolute.
    sdir = tmp / "search"
    sdir.mkdir()
    corpus = gen.search_corpus(sdir)
    sflags = [f"--allow-read={sdir}", f"--allow-write={sdir}"]
    sprog = (EXAMPLES / "search_index.sand", PORTS / "search_index.py")

    def idx(impl: str, kind: str = "base") -> Path:
        return sdir / f"{impl}.{kind}.json"

    def search_step(impl: str, *args: str) -> list[Step]:
        return [Step(*sprog, list(args), sflags)]

    def remove_index(impl: str):
        idx(impl).unlink(missing_ok=True)

    ws.append(
        Workload(
            "search_index_index",
            lambda impl: search_step(impl, "index", str(idx(impl)), str(corpus)),
            cwd=sdir,
            setup=remove_index,
            # Both implementations must write the same index file.
            check=lambda impl: b"index sha256 " + sha(idx(impl)) + b"\n",
        )
    )
    # Each search is a fresh process that loads the whole index, as in real use.
    queries = gen.search_queries()
    ws.append(
        Workload(
            "search_index_search",
            lambda impl: [s for q in queries for s in search_step(impl, "search", str(idx(impl)), *q)],
            cwd=sdir,
        )
    )
    add_files = [str(sdir / f) for f in gen.search_add_files(corpus)]

    def copy_index(impl: str):
        shutil.copyfile(idx(impl), idx(impl, "work"))

    ws.append(
        Workload(
            "search_index_add",
            lambda impl: search_step(impl, "add", str(idx(impl, "work")), *add_files),
            cwd=sdir,
            setup=copy_index,
            check=lambda impl: b"index sha256 " + sha(idx(impl, "work")) + b"\n",
        )
    )
    ws.append(
        Workload("search_index_stats", lambda impl: search_step(impl, "stats", str(idx(impl))) * 4, cwd=sdir)
    )

    # http_fetch -------------------------------------------------------------------
    pages = {f"/page/{i}": gen.html_page(i).encode() for i in range(48)}
    page_server = servers.PageServer(pages)
    b.cleanups.append(page_server.close)
    base = f"http://{page_server.addr}"
    urls = [f"{base}/old"] + [f"{base}/page/{i}" for i in range(1, 48)] + [f"{base}/missing", f"{base}/data.json"]
    simple(
        "http_fetch",
        EXAMPLES / "http_fetch.sand",
        PORTS / "http_fetch.py",
        urls,
        [f"--allow-net={page_server.addr}"],
    )

    # chat_server ---------------------------------------------------------------------
    def chat(impl: str) -> tuple[float, bytes]:
        step = Step(EXAMPLES / "chat_server.sand", PORTS / "chat_server.py", ["127.0.0.1:0"], ["--allow-listen=127.0.0.1:0"])
        cmd = b.command(impl, step)
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

    ws.append(Workload("chat_server", lambda impl: [], custom=chat))

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
    cli = (EXAMPLES / "s3" / "cli.sand", PORTS / "s3" / "cli.py")

    def s3cmd(*args: str) -> Step:
        return Step(*cli, list(args), s3flags)

    blob = random.Random(8).randbytes(S3_BLOB_MB << 20)
    (s3dir / "blob.bin").write_bytes(blob)

    def s3_workload(name, steps, setup_objects=None, check=None):
        ws.append(
            Workload(
                name,
                steps,
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
        for impl in IMPLS:
            if not (sdir / f"{impl}.base.json").exists():
                step = Step(EXAMPLES / "search_index.sand", PORTS / "search_index.py", ["index", str(sdir / f"{impl}.base.json"), str(sdir / "corpus")],
                            [f"--allow-read={sdir}", f"--allow-write={sdir}"])
                subprocess.run(b.command(impl, step), cwd=sdir, check=True, stdout=subprocess.DEVNULL, env=b.env(w))


def first_difference(a: bytes, b: bytes) -> str:
    la, lb = a.splitlines(), b.splitlines()
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            return f"line {i + 1}:\n  sand:   {x[:200]!r}\n  python: {y[:200]!r}"
    return f"lengths differ: sand {len(la)} lines / {len(a)} bytes, python {len(lb)} lines / {len(b)} bytes"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--only", help="comma-separated workload names or prefixes (e.g. life,search_index)")
    parser.add_argument("--runs", type=int, default=5, help="timed runs per implementation (default 5)")
    parser.add_argument("--sand", default=str(ROOT / "target" / "release" / "sand"), help="sand binary")
    parser.add_argument(
        "--python",
        default=sys.executable,
        help="interpreter for the ports (default: the one running this script)",
    )
    parser.add_argument(
        "--any-python",
        action="store_true",
        help=f"allow a Python other than {BASELINE[0]}.{BASELINE[1]} (the ratios are then not comparable)",
    )
    parser.add_argument("--keep", action="store_true", help="keep the generated inputs and print where they are")
    args = parser.parse_args()

    sand = Path(args.sand)
    if not sand.exists():
        print(f"{sand} not found: run `cargo build --release` first", file=sys.stderr)
        return 2
    python = shutil.which(args.python)
    if not python:
        print(f"{args.python} not found (use --python)", file=sys.stderr)
        return 2
    probe = "import sys; print(sys.version_info[0], sys.version_info[1]); print(sys.version.replace(chr(10), ' '))"
    lines = subprocess.run([python, "-c", probe], capture_output=True, text=True, check=True).stdout.splitlines()
    version, description = tuple(int(x) for x in lines[0].split()), lines[1]
    if version != BASELINE and not args.any_python:
        want = f"{BASELINE[0]}.{BASELINE[1]}"
        print(f"{python} is Python {description}; the baseline is {want} (use --python, or --any-python)", file=sys.stderr)
        return 2
    print(f"python: {python} ({description})")

    tmp = Path(tempfile.mkdtemp(prefix="sand-bench-")).resolve()
    b = Bench(sand, python, tmp)
    failed = False
    rows = []
    try:
        print(f"generating inputs in {tmp} ...", file=sys.stderr)
        all_ws = workloads(b)
        if args.only:
            wanted = [s.strip() for s in args.only.split(",") if s.strip()]
            selected = [w for w in all_ws if any(w.name == s or w.name.startswith(s) for s in wanted)]
            if not selected:
                print(f"no workload matches {args.only}; available: {', '.join(w.name for w in all_ws)}", file=sys.stderr)
                return 2
        else:
            selected = all_ws

        header = f"{'workload':<22}{'sand best':>11}{'median':>9}{'python best':>13}{'median':>9}{'ratio':>8}{'(best)':>8}"
        print(header)
        print("-" * len(header))
        for w in selected:
            prepare(b, w)
            times: dict[str, list[float]] = {impl: [] for impl in IMPLS}
            outputs: dict[str, bytes] = {}
            mismatch = None
            try:
                for i in range(args.runs + 1):  # run 0 is the untimed warm-up
                    for impl in IMPLS:
                        elapsed, out = b.run_once(w, impl)
                        if impl not in outputs:
                            outputs[impl] = out
                        elif out != outputs[impl]:
                            mismatch = f"{impl} output changed between runs: " + first_difference(outputs[impl], out)
                        if i > 0:
                            times[impl].append(elapsed)
                    if i == 0 and outputs["sand"] != outputs["python"]:
                        mismatch = "outputs differ, " + first_difference(outputs["sand"], outputs["python"])
                    if mismatch:
                        break
            except Exception as e:  # noqa: BLE001 - report and continue with the next workload
                failed = True
                print(f"{w.name:<22}ERROR: {e}")
                continue
            if mismatch:
                failed = True
                for impl in IMPLS:
                    (tmp / f"{w.name}.{impl}.out").write_bytes(outputs.get(impl, b""))
                print(f"{w.name:<22}MISMATCH: {mismatch}\n{'':<22}(outputs saved to {tmp}/{w.name}.*.out)")
                args.keep = True
                continue
            s, p = times["sand"], times["python"]
            ratio = statistics.median(s) / statistics.median(p)
            best_ratio = min(s) / min(p)
            rows.append((w.name, ratio, best_ratio))
            print(
                f"{w.name:<22}{min(s) * 1e3:>9.1f}ms{statistics.median(s) * 1e3:>7.1f}ms"
                f"{min(p) * 1e3:>11.1f}ms{statistics.median(p) * 1e3:>7.1f}ms{ratio:>8.2f}{best_ratio:>8.2f}",
                flush=True,
            )
        if rows:
            geo = statistics.geometric_mean([r[1] for r in rows])
            geo_best = statistics.geometric_mean([r[2] for r in rows])
            print("-" * len(header))
            print(f"{'geometric mean':<22}{'':>52}{geo:>8.2f}{geo_best:>8.2f}")
            print("ratio = sand median / python median, (best) = sand best / python best; > 1 means Sand is slower")
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
