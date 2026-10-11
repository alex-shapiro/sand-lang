# Collatz chain lengths below a limit, memoized in an int-keyed dict, with a
# histogram of the lengths and the longest chains. Stresses dicts keyed by
# dense small ints, recursion and integer arithmetic.

import sys

LIMIT = 1_500_000
CACHE_BELOW = 3_000_000

sys.setrecursionlimit(10_000)


def chain_length(cache: dict[int, int], n: int) -> int:
    l = cache.get(n)
    if l is not None:
        return l
    nxt = n // 2 if n % 2 == 0 else 3 * n + 1
    l = chain_length(cache, nxt) + 1
    if n < CACHE_BELOW:
        cache[n] = l
    return l


cache = {1: 1}
histogram: dict[int, int] = {}
longest = (0, 0)
for n in range(1, LIMIT):
    l = chain_length(cache, n)
    histogram[l] = histogram.get(l, 0) + 1
    if l > longest[1]:
        longest = (n, l)
print(f"longest chain below {LIMIT}: {longest[0]} with {longest[1]} terms")
print(f"cached lengths: {len(cache)}")
common = sorted(histogram.items(), key=lambda e: (-e[1], e[0]))[:5]
for length, count in common:
    print(f"length {length}: {count} starts")
total = 0
for n in range(1, LIMIT):
    total += cache[n] % 7
print(f"checksum {total}")
