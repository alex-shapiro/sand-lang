# Sales report from CSV text: generate rows in memory, split and parse them, aggregate
# per (region, product) and per month into dicts of accumulators, then print padded tables.
# Stresses string building and splitting, number parsing, dict lookups with string and
# tuple keys, small object mutation and string formatting.

import sys
from collections import defaultdict
from dataclasses import dataclass

ROWS = 200000
REGIONS = ["north", "south", "east", "west", "central", "overseas"]
KINDS = ["widget", "gadget", "gizmo", "sprocket", "doohickey"]
HEADER = "date,region,product,quantity,unit_price"


class Rng:
    __slots__ = ("state",)

    def __init__(self, seed: int):
        self.state = seed

    def next(self, n: int) -> int:
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state // 65536 % n


def generate(rows: int) -> str:
    rng = Rng(7)
    lines = [HEADER]
    for _ in range(rows):
        month = rng.next(12) + 1
        day = rng.next(28) + 1
        region = REGIONS[rng.next(len(REGIONS))]
        product = f"{KINDS[rng.next(len(KINDS))]}-{rng.next(10) + 1}"
        quantity = rng.next(20) + 1
        cents = 99 + rng.next(9900)
        lines.append(f"2024-{month:02d}-{day:02d},{region},{product},{quantity},{cents // 100}.{cents % 100:02d}")
    return "\n".join(lines) + "\n"


@dataclass(slots=True)
class Sale:
    month: str
    region: str
    product: str
    quantity: int
    price: int


def parse_sale(line: str) -> Sale:
    date, region, product, qty, price = line.split(",")
    return Sale(date[:7], region, product, int(qty), round(float(price) * 100))


@dataclass(slots=True)
class Acc:
    """Totals over a group of sales; prices and revenue are in cents."""

    count: int = 0
    quantity: int = 0
    revenue: int = 0
    min_price: int = sys.maxsize
    max_price: int = 0

    def add(self, quantity: int, price: int) -> None:
        self.count += 1
        self.quantity += quantity
        self.revenue += quantity * price
        self.min_price = min(self.min_price, price)
        self.max_price = max(self.max_price, price)

    def merge(self, other: "Acc") -> None:
        self.count += other.count
        self.quantity += other.quantity
        self.revenue += other.revenue
        self.min_price = min(self.min_price, other.min_price)
        self.max_price = max(self.max_price, other.max_price)


def money(cents: int) -> str:
    return f"${cents // 100:,}.{cents % 100:02d}"


def percent(part: int, whole: int) -> str:
    basis_points = part * 10000 // whole
    return f"{basis_points // 100}.{basis_points % 100:02d}%"


text = generate(ROWS)
lines = text.splitlines()
assert lines[0] == HEADER, "unexpected header"

by_pair: defaultdict[tuple[str, str], Acc] = defaultdict(Acc)
by_month: defaultdict[str, Acc] = defaultdict(Acc)
for line in lines[1:]:
    sale = parse_sale(line)
    by_pair[sale.region, sale.product].add(sale.quantity, sale.price)
    by_month[sale.month].add(sale.quantity, sale.price)

by_product: defaultdict[str, Acc] = defaultdict(Acc)
by_region: defaultdict[str, Acc] = defaultdict(Acc)
total = Acc()
for (region, product), acc in by_pair.items():
    by_product[product].merge(acc)
    by_region[region].merge(acc)
    total.merge(acc)

print(f"== Sales report: {total.count} orders, {len(text)} bytes of CSV")
print(f"units sold {total.quantity}, revenue {money(total.revenue)}")
print()

print("== Top 15 products by revenue")
print(f" #  {'product':<14}{'orders':>8}{'units':>8}{'revenue':>16}{'avg':>9}{'min':>9}{'max':>9}")
ranked = sorted(by_product.items(), key=lambda e: (-e[1].revenue, e[0]))
for i, (product, acc) in enumerate(ranked[:15], 1):
    print(
        f"{i:>2}  {product:<14}{acc.count:>8}{acc.quantity:>8}{money(acc.revenue):>16}"
        f"{money(acc.revenue // acc.quantity):>9}{money(acc.min_price):>9}{money(acc.max_price):>9}"
    )
print()

print("== Revenue by region")
print(f"{'region':<10}{'orders':>8}{'units':>8}{'revenue':>16}{'share':>9}")
for region in sorted(by_region):
    acc = by_region[region]
    print(
        f"{region:<10}{acc.count:>8}{acc.quantity:>8}{money(acc.revenue):>16}"
        f"{percent(acc.revenue, total.revenue):>9}"
    )
print()

print("== Revenue by month")
best_month = max(acc.revenue for acc in by_month.values())
print(f"{'month':<9}{'orders':>8}{'units':>8}{'revenue':>16}{'avg order':>11}  chart")
for month in sorted(by_month):
    acc = by_month[month]
    print(
        f"{month:<9}{acc.count:>8}{acc.quantity:>8}{money(acc.revenue):>16}"
        f"{money(acc.revenue // acc.count):>11}  {'#' * (acc.revenue * 30 // best_month)}"
    )
