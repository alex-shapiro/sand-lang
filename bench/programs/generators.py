# Generator-style streaming: stateful producers (Fibonacci mod m, primes by trial division,
# Collatz sequences, a random stream) composed with merge, moving-sum, chunking and run-length
# transformers into lazy pipelines that are built first and consumed later. Stresses generator
# functions, itertools and lambdas.

from functools import reduce
from itertools import batched, chain, count, groupby, islice, takewhile
from operator import itemgetter
from typing import Iterator

FIB_COUNT = 300_000
WINDOW = 16
PRIME_LIMIT = 300_000
CHUNK = 8
COLLATZ_SEEDS = 12_000
RANDOM_COUNT = 300_000
MOD = 1_000_000_007


def fib_mod(m: int) -> Iterator[int]:
    a, b = 0, 1
    while True:
        yield a
        a, b = b, (a + b) % m


def random_stream(seed: int) -> Iterator[int]:
    state = seed
    while True:
        state = (state * 1103515245 + 12345) % 2147483648
        yield state // 65536


def primes() -> Iterator[int]:
    """Primes in increasing order, each tested by trial division against the primes found so far."""
    found: list[int] = []
    for n in count(2):
        if all(n % p != 0 for p in takewhile(lambda p: p * p <= n, found)):
            found.append(n)
            yield n


def collatz(seed: int) -> Iterator[int]:
    n = seed
    yield n
    while n != 1:
        n = n // 2 if n % 2 == 0 else 3 * n + 1
        yield n


def merge(left: Iterator[int], right: Iterator[int]) -> Iterator[int]:
    """Merges two ascending streams into one ascending stream."""
    a, b = next(left, None), next(right, None)
    while a is not None or b is not None:
        if b is None or (a is not None and a <= b):
            yield a
            a = next(left, None)
        else:
            yield b
            b = next(right, None)


def moving_sum(source: Iterator[int], width: int) -> Iterator[int]:
    """Sums of the last `width` elements (fewer at the start)."""
    window = [0] * width
    i = total = 0
    for x in source:
        total += x - window[i]
        window[i] = x
        i = (i + 1) % width
        yield total


def run_lengths(source: Iterator[int]) -> Iterator[tuple[int, int]]:
    """Runs of equal consecutive elements as `(value, length)` pairs."""
    for value, run in groupby(source):
        yield value, sum(1 for _ in run)


def digest(acc: int, x: int) -> int:
    return (acc * 31 + x) % MOD


def fibonacci_windows() -> None:
    sums = moving_sum(islice(fib_mod(1000), FIB_COUNT), WINDOW)
    odd = (s for s in sums if s % 2 == 1)
    checksum = reduce(digest, odd, 0)
    peak_at, peak = max(enumerate(moving_sum(islice(fib_mod(1000), FIB_COUNT), WINDOW)), key=itemgetter(1))
    digits = (x % 1000 for x in random_stream(3))
    calm = sum(1 for _ in takewhile(lambda s: s >= 3300, islice(moving_sum(digits, WINDOW), WINDOW, None)))
    print(f"fib: checksum {checksum}, peak {peak} at {peak_at}, {calm} random sums above 3300")


def prime_streams() -> None:
    squares = (n * n for n in count(1))
    merged = takewhile(lambda x: x < PRIME_LIMIT, merge(primes(), squares))
    groups = batched(merged, CHUNK)
    group_sums = map(sum, groups)
    checksum = reduce(digest, group_sums, 0)
    pairs = takewhile(lambda p: p[1] < PRIME_LIMIT, zip(primes(), islice(primes(), 1, None)))
    twins = sum(1 for p, q in pairs if q - p == 2)
    gaps = (p % 1000 for p in islice(takewhile(lambda p: p < PRIME_LIMIT, primes()), 0, None, 100))
    print(f"primes: merged checksum {checksum}, twins {twins}")
    print(f"primes: sampled {list(islice(gaps, 12))}")


def collatz_streams() -> None:
    lengths = ((sum(1 for _ in collatz(seed)), seed) for seed in islice(count(1), COLLATZ_SEEDS))
    steps, seed = max(lengths)
    all_values = chain.from_iterable(map(collatz, islice(count(1), COLLATZ_SEEDS)))
    checksum = reduce(digest, (x for i, x in enumerate(all_values) if i % 7 == 0), 0)
    peak = max(chain.from_iterable(map(collatz, islice(count(1, 3), COLLATZ_SEEDS // 3))))
    print(f"collatz: longest {steps} from {seed}, peak {peak}, checksum {checksum}")


def random_runs() -> None:
    symbols = islice((x % 3 for x in random_stream(42)), RANDOM_COUNT)
    tail = islice((x % 2 for x in random_stream(7)), RANDOM_COUNT // 10)
    runs = run_lengths(chain(symbols, tail))
    histogram = [0] * 10
    n = 0
    longest = (0, 0)
    for value, length in runs:
        histogram[min(length, 9)] += 1
        n += 1
        if length > longest[1]:
            longest = (value, length)
    print(f"runs: {n} runs, longest {longest[1]} of {longest[0]}, histogram {histogram[1:]}")


fibonacci_windows()
prime_streams()
collatz_streams()
random_runs()
