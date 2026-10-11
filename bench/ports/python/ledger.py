"""An expense report built from two small "traits".

CPython port of examples/ledger.sand. Sand's `Monoid` and `Render` traits
(implemented for structs and for builtin types such as `int`, `str`, tuples
and lists) become `functools.singledispatch` generic functions. Sand passes
the identity `V.empty()` implicitly through the trait dictionary; here it is
passed explicitly.

Usage: python3 ledger.py [CATEGORY]
"""

import sys
from dataclasses import dataclass
from functools import reduce, singledispatch

# ---------------------------------------------------------------------------
# Monoid: values with an identity and an associative way to combine them.


@singledispatch
def combine(a, b):
    raise TypeError(f"not a monoid: {type(a).__name__}")


def concat(xs, empty):
    """Combines all values of a list (the identity if it is empty)."""
    return reduce(combine, xs, empty)


# ---------------------------------------------------------------------------
# Render: values that can be shown in the report.


@singledispatch
def render(x) -> str:
    raise TypeError(f"cannot render {type(x).__name__}")


def cell(x, width: int) -> str:
    """The rendering, padded to `width` characters."""
    return render(x).ljust(width)


@dataclass(frozen=True, slots=True, order=True)
class Money:
    """An amount of money in cents."""

    cents: int


@combine.register
def _(a: Money, b: Money) -> Money:
    return Money(a.cents + b.cents)


@render.register
def _(m: Money) -> str:
    return f"${m.cents // 100}.{m.cents % 100:02}"


@dataclass(frozen=True, slots=True)
class Max:
    """The largest value combined so far, if any."""

    value: object = None


@combine.register
def _(a: Max, b: Max) -> Max:
    match (a.value, b.value):
        case (None, v) | (v, None):
            return Max(v)
        case (x, y):
            return Max(x if x >= y else y)


@render.register
def _(m: Max) -> str:
    return "-" if m.value is None else "max " + render(m.value)


@combine.register
def _(a: int, b: int) -> int:
    return a + b


@render.register
def _(x: int) -> str:
    return f"{x}"


@render.register
def _(x: str) -> str:
    return x


@combine.register
def _(a: tuple, b: tuple) -> tuple:
    return (combine(a[0], b[0]), combine(a[1], b[1]))


@render.register
def _(x: tuple) -> str:
    return cell(x[0], 12) + render(x[1])


@render.register
def _(xs: list) -> str:
    return ", ".join(render(x) for x in xs)


# ---------------------------------------------------------------------------


@dataclass(slots=True)
class Expense:
    month: str
    category: str
    what: str
    amount: Money


# Per expense: (count, (total, largest)).
STATS_EMPTY = (0, (Money(0), Max()))


def stats(e: Expense):
    return (1, (e.amount, Max(e.amount)))


def summarize(pairs, empty) -> dict:
    """Groups values by key, combining each group."""
    out = {}
    for k, v in pairs:
        out[k] = combine(out.get(k, empty), v)
    return out


def section(title: str, rows: dict) -> None:
    print(f"== {title}")
    for k in sorted(rows):
        print("  " + cell(k, 10) + render(rows[k]))


def parse(line: str) -> Expense | None:
    parts = [p.strip() for p in line.split(",")]
    if len(parts) != 4:
        return None
    month, category, what, amount = parts
    whole, frac = amount.split(".", 1) if "." in amount else (amount, "0")
    try:
        cents = int(whole) * 100 + int(frac.ljust(2, "0")[:2])
    except ValueError:
        return None
    return Expense(month, category, what, Money(cents))


DATA = """
    2024-01, food, groceries, 54.20
    2024-01, rent, january, 950.00
    2024-01, food, cafe, 7.5
    2024-02, food, groceries, 61.05
    2024-02, travel, train, 38.40
    2024-02, rent, february, 950.00
    2024-02, food, restaurant, 42.00
    """


def main() -> None:
    expenses = [e for line in DATA.splitlines() if (e := parse(line)) is not None]

    section("by category: count, total, largest", summarize([(e.category, stats(e)) for e in expenses], STATS_EMPTY))
    section("by month: total", summarize([(e.month, e.amount) for e in expenses], Money(0)))
    overall = concat([stats(e) for e in expenses], STATS_EMPTY)
    print(f"== overall: {render(overall)}")

    # An optional argument picks one category.
    if len(sys.argv) > 1:
        wanted = sys.argv[1]
        items = [e.what for e in expenses if e.category == wanted]
        print(f"== {wanted}: " + ("none" if not items else render(items)))


main()
