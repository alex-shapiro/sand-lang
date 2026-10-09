"""Conway's Game of Life on a toroidal grid.

CPython port of examples/life.sand.

Usage: python3 life.py [generations]
"""

import sys


class Board:
    __slots__ = ("width", "height", "cells")

    def __init__(self, width: int, height: int, cells: list[list[bool]]):
        self.width = width
        self.height = height
        self.cells = cells

    @staticmethod
    def parse(text: str) -> "Board":
        """Parses a board where `#` is alive and anything else is dead."""
        rows = [[c == "#" for c in line.strip()] for line in text.strip().splitlines()]
        return Board(len(rows[0]), len(rows), rows)

    def alive(self, r: int, c: int) -> bool:
        # Floor modulo wraps negative coordinates around the edges.
        return self.cells[r % self.height][c % self.width]

    def neighbors(self, r: int, c: int) -> int:
        n = 0
        for dr in range(-1, 2):
            for dc in range(-1, 2):
                if (dr != 0 or dc != 0) and self.alive(r + dr, c + dc):
                    n += 1
        return n

    def next_state(self, r: int, c: int) -> bool:
        n = self.neighbors(r, c)
        return n == 3 or (n == 2 and self.cells[r][c])

    def step(self) -> None:
        self.cells = [[self.next_state(r, c) for c in range(self.width)] for r in range(self.height)]

    def population(self) -> int:
        return sum(row.count(True) for row in self.cells)

    def render(self) -> str:
        return "\n".join("".join("#" if c else "." for c in row) for row in self.cells)


GLIDER = """
    .#......
    ..#.....
    ###.....
    ........
    ........
    ........
    """


def main() -> None:
    try:
        generations = int(sys.argv[1])
    except (IndexError, ValueError):
        generations = 4
    board = Board.parse(GLIDER)
    for _ in range(generations):
        board.step()
    print(f"generation {generations}, population {board.population()}")
    print(board.render())


main()
