# Sparse graph analytics over large integer node ids, as when processing an
# edge list exported from a database: connected components, degree histogram
# and a topological sort. Stresses int-keyed dicts, sets and list-as-queue BFS.

NODES = 200_000
EDGES = 500_000
CLUSTER = 25
MOD = 1_000_000_007


class Rng:
    __slots__ = ("state",)

    def __init__(self, seed: int):
        self.state = seed

    def next(self, bound: int) -> int:
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state * bound // 2147483648


def component_sizes(ids: list[int], adj: dict[int, list[int]]) -> list[int]:
    """Sizes of the connected components, in order of their first node in ids."""
    visited: set[int] = set()
    sizes = []
    for id in ids:
        if id in visited:
            continue
        visited.add(id)
        queue = [id]
        head = 0
        while head < len(queue):
            u = queue[head]
            head += 1
            for v in adj[u]:
                if v not in visited:
                    visited.add(v)
                    queue.append(v)
        sizes.append(len(queue))
    return sizes


def degree_histogram(adj: dict[int, list[int]]) -> dict[int, int]:
    hist: dict[int, int] = {}
    for neighbors in adj.values():
        hist[len(neighbors)] = hist.get(len(neighbors), 0) + 1
    return hist


def topological_order(ids: list[int], edges: list[tuple[int, int]]) -> list[int]:
    """Kahn's algorithm on the DAG that orients every edge from the smaller id
    to the larger; ties in the initial queue are broken by id."""
    successors: dict[int, list[int]] = {}
    in_degree = {id: 0 for id in ids}
    for a, b in edges:
        low, high = (a, b) if a < b else (b, a)
        successors.setdefault(low, []).append(high)
        in_degree[high] += 1
    order = sorted(id for id, degree in in_degree.items() if degree == 0)
    head = 0
    while head < len(order):
        u = order[head]
        head += 1
        for v in successors.get(u, ()):
            in_degree[v] -= 1
            if in_degree[v] == 0:
                order.append(v)
    return order


rng = Rng(7)
ids = [rng.next(1_000_000_000) for _ in range(NODES)]
adj: dict[int, list[int]] = {id: [] for id in ids}
edges: list[tuple[int, int]] = []
for _ in range(EDGES):
    i = rng.next(NODES)
    j = rng.next(NODES) if rng.next(1000) < 5 else i // CLUSTER * CLUSTER + rng.next(CLUSTER)
    a, b = ids[i], ids[j]
    if a == b:
        continue
    adj[a].append(b)
    adj[b].append(a)
    edges.append((a, b))
print(f"nodes {len(adj)}, edges {len(edges)}")

sizes = component_sizes(ids, adj)
largest = " ".join(str(s) for s in sorted(sizes, reverse=True)[:5])
print(f"components {len(sizes)}, largest {largest}")

hist = degree_histogram(adj)
for degree in range(1, 11):
    print(f"degree {degree}: {hist.get(degree, 0)}")

order = topological_order(ids, edges)
checksum = 0
for position, id in enumerate(order):
    checksum = (checksum + position * id) % MOD
print(f"topological order {len(order)}, checksum {checksum}")
