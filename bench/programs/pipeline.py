# Closure-heavy event processing: an event bus of handler closures that mutate
# nonlocal variables, generator pipelines with itertools and higher-order
# builtins with key functions. Stresses closure calls and generators.

from collections import defaultdict
from dataclasses import dataclass
from functools import reduce
from itertools import accumulate, islice, pairwise, takewhile
from typing import Callable, Iterator

EVENTS = 300_000
KINDS = 8
USERS = 5_000
MOD = 1_000_000_007


class Rng:
    __slots__ = ("state",)

    def __init__(self, seed: int):
        self.state = seed

    def next(self, bound: int) -> int:
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state * bound // 2147483648


@dataclass(slots=True)
class Event:
    kind: int
    user: int
    amount: int


class EventBus:
    __slots__ = ("handlers",)

    def __init__(self):
        self.handlers: dict[int, list[Callable[[Event], None]]] = {}

    def on(self, kind: int, handler: Callable[[Event], None]) -> None:
        self.handlers.setdefault(kind, []).append(handler)

    def emit(self, event: Event) -> None:
        for handler in self.handlers.get(event.kind, ()):
            handler(event)


def dispatch(events: list[Event]) -> None:
    bus = EventBus()
    counts = [0] * KINDS
    purchases = revenue = refunded = largest = digest = 0
    spend_by_user: dict[int, int] = {}

    def counter(kind: int) -> Callable[[Event], None]:
        def count(_: Event) -> None:
            counts[kind] += 1
        return count

    def mix(e: Event) -> None:
        nonlocal digest
        digest = (digest * 31 + e.amount) % MOD

    def purchase(e: Event) -> None:
        nonlocal purchases, revenue
        purchases += 1
        revenue += e.amount

    def refund(e: Event) -> None:
        nonlocal refunded
        refunded += e.amount

    def track_largest(e: Event) -> None:
        nonlocal largest
        largest = max(largest, e.amount)

    def spend(e: Event) -> None:
        spend_by_user[e.user] = spend_by_user.get(e.user, 0) + e.amount

    for kind in range(KINDS):
        bus.on(kind, counter(kind))
        bus.on(kind, mix)
    bus.on(0, purchase)
    bus.on(1, refund)
    for kind in (0, 2):
        bus.on(kind, track_largest)
    bus.on(2, spend)
    for e in events:
        bus.emit(e)
    print("bus: counts " + " ".join(map(str, counts)))
    print(f"bus: purchases {purchases}, revenue {revenue}, average {revenue / purchases:.2f}")
    print(f"bus: refunded {refunded}, largest {largest}, spenders {len(spend_by_user)}, digest {digest}")


def bursts(events: list[Event]) -> Iterator[tuple[int, int]]:
    """Runs of consecutive events of the same kind, as (kind, length) pairs."""
    i = 0
    while i < len(events):
        kind = events[i].kind
        start = i
        while i < len(events) and events[i].kind == kind:
            i += 1
        yield kind, i - start


def lazy_stats(events: list[Event]) -> None:
    total = sum(e.amount for e in events)
    purchase_total = sum(e.amount for e in events if e.kind == 0)
    sampled = sum(i % 100 * e.amount for i, e in islice(enumerate(events), 0, None, 7))
    half = total // 2
    within_budget = sum(1 for _ in takewhile(lambda spent: spent < half, accumulate(e.amount for e in events)))
    rising = sum(1 for a, b in pairwise(events) if a.amount < b.amount)
    digest = reduce(lambda acc, e: (acc * 31 + e.amount) % MOD, (e for e in events if e.kind == 4), 0)
    heavy_users = len({e.user for e in events if e.amount > 990})
    whale = next(((i, e) for i, e in enumerate(events) if e.kind == 7 and e.amount == 1000), None)
    long_bursts = sum(1 for _, length in bursts(events) if length >= 4)
    burst_kind, burst_length = max(bursts(events), key=lambda b: b[1])
    print(f"lazy: total {total}, purchases {purchase_total}, sampled {sampled}, within budget {within_budget}")
    print(f"lazy: rising {rising}, digest {digest}, heavy users {heavy_users}")
    if whale is not None:
        print(f"lazy: first whale at {whale[0]}, user {whale[1].user}")
    else:
        print("lazy: no whale")
    print(f"lazy: long bursts {long_bursts}, longest kind {burst_kind} x {burst_length}")


def eager_stats(events: list[Event]) -> None:
    big = [e for e in events if e.amount > 900]
    big_total = sum(e.amount for e in big)
    by_user: defaultdict[int, list[Event]] = defaultdict(list)
    for e in events:
        by_user[e.user].append(e)
    totals = [(user, sum(e.amount for e in es)) for user, es in by_user.items()]
    top_spenders = " ".join(f"{u}={t}" for u, t in sorted(totals, key=lambda t: (-t[1], t[0]))[:3])
    ordered = sorted(events, key=lambda e: (e.user, -e.amount))
    order_digest = reduce(lambda acc, e: (acc * 31 + e.amount) % MOD, ordered, 0)
    any_whale = any(e.kind == 7 and e.amount == 1000 for e in events)
    all_positive = all(e.amount > 0 for e in events)
    peak = max(events, key=lambda e: e.amount * KINDS + e.kind)
    refunds = sum(1 for e in events if e.kind == 1)
    print(f"eager: big {len(big)} totalling {big_total}, refunds {refunds}")
    print(f"eager: top spenders {top_spenders}")
    print(f"eager: order digest {order_digest}, any whale {str(any_whale).lower()}, all positive {str(all_positive).lower()}")
    print(f"eager: peak user {peak.user}, kind {peak.kind}, amount {peak.amount}")


rng = Rng(2024)
events = [Event(rng.next(KINDS), rng.next(USERS), 1 + rng.next(1000)) for _ in range(EVENTS)]
dispatch(events)
lazy_stats(events)
eager_stats(events)
