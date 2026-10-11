# Dijkstra's shortest paths from several sources over a random weighted digraph.
# Stresses tuple allocation and unpacking, list indexing and heapq.

import heapq

NODES = 40_000
MAX_OUT_DEGREE = 9
SOURCES = 8
INF = float("inf")


class Rng:
    __slots__ = ("state",)

    def __init__(self, seed: int):
        self.state = seed

    def next(self, bound: int) -> int:
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state * bound // 2147483648


def random_graph(rng: Rng) -> list[list[tuple[int, int]]]:
    return [
        [(rng.next(NODES), 1 + rng.next(100)) for _ in range(1 + rng.next(MAX_OUT_DEGREE))]
        for _ in range(NODES)
    ]


def shortest_paths(graph: list[list[tuple[int, int]]], source: int) -> list[float]:
    dist = [INF] * len(graph)
    dist[source] = 0
    heap = [(0, source)]
    while heap:
        d, u = heapq.heappop(heap)
        if d > dist[u]:
            continue
        for v, w in graph[u]:
            nd = d + w
            if nd < dist[v]:
                dist[v] = nd
                heapq.heappush(heap, (nd, v))
    return dist


rng = Rng(42)
graph = random_graph(rng)
for _ in range(SOURCES):
    source = rng.next(NODES)
    reached = [d for d in shortest_paths(graph, source) if d < INF]
    print(f"source {source}: reached {len(reached)}, max {max(reached)}, sum {sum(reached)}")
