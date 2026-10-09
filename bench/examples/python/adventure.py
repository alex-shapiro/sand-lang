"""A small text adventure. Reads commands from standard input.

CPython port of examples/adventure.sand.

Commands: look, go DIRECTION (or n/s/e/w), take ITEM, drop ITEM,
use ITEM, inventory (or i), help, quit.
"""

import sys
from dataclasses import dataclass, field
from enum import Enum


class Dir(Enum):
    NORTH = "north"
    SOUTH = "south"
    EAST = "east"
    WEST = "west"

    @staticmethod
    def parse(s: str) -> "Dir | None":
        match s:
            case "north" | "n":
                return Dir.NORTH
            case "south" | "s":
                return Dir.SOUTH
            case "east" | "e":
                return Dir.EAST
            case "west" | "w":
                return Dir.WEST
            case _:
                return None


@dataclass(slots=True)
class Room:
    name: str
    description: str
    exits: dict[Dir, str]
    items: set[str]
    locked: Dir | None = None


@dataclass(slots=True)
class Game:
    rooms: dict[str, Room]
    here: str
    inventory: set[str] = field(default_factory=set)
    moves: int = 0
    won: bool = False

    def room(self) -> Room:
        return self.rooms[self.here]

    def describe(self) -> str:
        r = self.room()
        exits = ", ".join(d.value for d in r.exits)
        text = f"== {r.name} ==\n{r.description}\nExits: {exits}"
        if r.items:
            text += f"\nYou see: {', '.join(sorted(r.items))}"
        return text

    def go(self, d: Dir) -> str:
        r = self.room()
        dest = r.exits.get(d)
        if dest is None:
            return f"You can't go {d.value}."
        if r.locked == d:
            return f"The door to the {d.value} is locked."
        self.here = dest
        self.moves += 1
        if self.here == "vault":
            self.won = True
            return self.describe() + f"\nYou found the treasure in {self.moves} moves. You win!"
        return self.describe()

    def take(self, item: str) -> str:
        r = self.room()
        if item not in r.items:
            return f"There is no {item} here."
        r.items.remove(item)
        self.inventory.add(item)
        return f"Taken: {item}."

    def drop(self, item: str) -> str:
        if item not in self.inventory:
            return f"You don't have {item}."
        self.inventory.remove(item)
        self.room().items.add(item)
        return f"Dropped: {item}."

    def use_item(self, item: str) -> str:
        if item not in self.inventory:
            return f"You don't have {item}."
        r = self.room()
        match (item, r.locked):
            case ("key", Dir() as d):
                r.locked = None
                return f"You unlock the door to the {d.value}."
            case _:
                return "Nothing happens."

    def command(self, line: str) -> str | None:
        words = line.strip().lower().split()
        match words:
            case []:
                return ""
            case ["quit"] | ["exit"]:
                return None
            case ["look"] | ["l"]:
                return self.describe()
            case ["inventory"] | ["i"]:
                if not self.inventory:
                    return "You are empty-handed."
                return f"You carry: {', '.join(sorted(self.inventory))}"
            case ["go", d] | [d] if Dir.parse(d) is not None:
                return self.go(Dir.parse(d))
            case ["take", item]:
                return self.take(item)
            case ["drop", item]:
                return self.drop(item)
            case ["use", item]:
                return self.use_item(item)
            case ["help"]:
                return "Commands: look, go DIR, take ITEM, drop ITEM, use ITEM, inventory, quit"
            case _:
                return "I don't understand that."


def world() -> dict[str, Room]:
    return {
        "hall": Room(
            name="Great Hall",
            description="A dusty hall with portraits on the walls.",
            exits={Dir.NORTH: "library", Dir.EAST: "kitchen"},
            items={"lamp"},
        ),
        "library": Room(
            name="Library",
            description="Shelves of crumbling books. A heavy door leads east.",
            exits={Dir.SOUTH: "hall", Dir.EAST: "vault"},
            items={"book"},
            locked=Dir.EAST,
        ),
        "kitchen": Room(
            name="Kitchen",
            description="It smells of old bread.",
            exits={Dir.WEST: "hall"},
            items={"key", "bread"},
        ),
        "vault": Room(
            name="Vault",
            description="Gold glitters everywhere.",
            exits={Dir.WEST: "library"},
            items={"treasure"},
        ),
    }


def main() -> None:
    game = Game(rooms=world(), here="hall")
    print(game.describe())
    while not game.won:
        sys.stdout.write("> ")
        line = sys.stdin.readline()
        if not line:
            break
        reply = game.command(line)
        if reply is None:
            print("Goodbye.")
            break
        if reply:
            print(reply)


main()
