# Memoized recursive dynamic programming: lattice paths around obstacles,
# edit distance and 0/1 knapsack, each memoized in a dict with tuple keys.
# Stresses tuple-keyed hashing, dict get/insert, closures and recursion.

from dataclasses import dataclass

MOD = 1_000_000_007
GRIDS = 12
GRID_SIZE = 300
PAIRS = 450
KNAPSACKS = 12
ITEMS = 100
CAPACITY = 1000
BASES = ["A", "C", "G", "T"]


class Rng:
    __slots__ = ("state",)

    def __init__(self, state: int):
        self.state = state

    def next(self, n: int) -> int:
        """A pseudo-random integer in range(n) (n at most 32768), from the high bits."""
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state // 65536 % n


@dataclass(frozen=True, slots=True)
class Item:
    weight: int
    value: int


def make_grid(rng: Rng, size: int) -> list[list[bool]]:
    """A square grid where True marks an obstacle; the corners are always open."""
    grid = [[rng.next(8) == 0 for _ in range(size)] for _ in range(size)]
    grid[0][0] = False
    grid[size - 1][size - 1] = False
    return grid


def count_paths(grid: list[list[bool]]) -> int:
    """The number of monotone (down/right) paths from the top-left to the
    bottom-right corner that avoid obstacles, modulo MOD."""
    memo: dict[tuple[int, int], int] = {}

    def paths(r: int, c: int) -> int:
        if r < 0 or c < 0 or grid[r][c]:
            return 0
        if r == 0 and c == 0:
            return 1
        if (r, c) in memo:
            return memo[r, c]
        n = (paths(r - 1, c) + paths(r, c - 1)) % MOD
        memo[r, c] = n
        return n

    return paths(len(grid) - 1, len(grid[0]) - 1)


def random_string(rng: Rng) -> str:
    return "".join(BASES[rng.next(len(BASES))] for _ in range(40 + rng.next(41)))


def edit_distance(a: str, b: str) -> int:
    memo: dict[tuple[int, int], int] = {}

    def dist(i: int, j: int) -> int:
        if i == len(a):
            return len(b) - j
        if j == len(b):
            return len(a) - i
        if (i, j) in memo:
            return memo[i, j]
        if a[i] == b[j]:
            d = dist(i + 1, j + 1)
        else:
            d = 1 + min(dist(i + 1, j), dist(i, j + 1), dist(i + 1, j + 1))
        memo[i, j] = d
        return d

    return dist(0, 0)


def knapsack(items: list[Item], capacity: int) -> int:
    memo: dict[tuple[int, int], int] = {}

    def best(i: int, room: int) -> int:
        if i == len(items):
            return 0
        if (i, room) in memo:
            return memo[i, room]
        item = items[i]
        v = best(i + 1, room)
        if item.weight <= room:
            v = max(v, item.value + best(i + 1, room - item.weight))
        memo[i, room] = v
        return v

    return best(0, capacity)


rng = Rng(42)

for g in range(GRIDS):
    print(f"grid {g}: {count_paths(make_grid(rng, GRID_SIZE))} paths")

total_distance = 0
for p in range(PAIRS):
    a = random_string(rng)
    b = random_string(rng)
    d = edit_distance(a, b)
    if p < 5:
        print(f"distance {len(a)}x{len(b)}: {d}")
    total_distance += d
print(f"total distance: {total_distance}")

for k in range(KNAPSACKS):
    items = [Item(1 + rng.next(100), 1 + rng.next(500)) for _ in range(ITEMS)]
    print(f"knapsack {k}: {knapsack(items, CAPACITY)}")
