# Benchmarks Game binary-trees: build and walk many perfect binary trees.
# Stresses allocation of small objects, recursion and the GC.

MIN_DEPTH = 4
MAX_DEPTH = 15


class Node:
    __slots__ = ("left", "right")

    def __init__(self, left: "Node | None", right: "Node | None"):
        self.left = left
        self.right = right

    @staticmethod
    def bottom_up(depth: int) -> "Node":
        if depth == 0:
            return Node(None, None)
        return Node(Node.bottom_up(depth - 1), Node.bottom_up(depth - 1))

    def check(self) -> int:
        if self.left is not None:
            return 1 + self.left.check() + self.right.check()
        return 1


stretch_depth = MAX_DEPTH + 1
print(f"stretch tree of depth {stretch_depth}\t check: {Node.bottom_up(stretch_depth).check()}")

long_lived = Node.bottom_up(MAX_DEPTH)

for depth in range(MIN_DEPTH, MAX_DEPTH + 1, 2):
    iterations = 1 << (MAX_DEPTH - depth + MIN_DEPTH)
    check = sum(Node.bottom_up(depth).check() for _ in range(iterations))
    print(f"{iterations}\t trees of depth {depth}\t check: {check}")

print(f"long lived tree of depth {MAX_DEPTH}\t check: {long_lived.check()}")
