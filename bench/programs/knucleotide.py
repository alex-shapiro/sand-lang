# Benchmarks Game k-nucleotide over a generated DNA sequence: count every
# k-mer for several k with substrings as dict keys, then report frequencies.
# Stresses substring creation and string-keyed hashing.

LENGTH = 600_000
QUERIES = ["GGT", "GGTA", "GGTATT", "GGTATTTTAATT", "GGTATTTTAATTTATAGT"]


class Rng:
    __slots__ = ("state",)

    def __init__(self, state: int):
        self.state = state

    def next(self, n: int) -> int:
        """A pseudo-random integer in range(n) (n at most 32768), from the high bits."""
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state // 65536 % n


def random_base(rng: Rng) -> str:
    """A random base with the frequencies of the fasta benchmark's Homo sapiens
    alphabet, rounded to thousandths."""
    r = rng.next(1000)
    if r <= 302:
        return "A"
    if r <= 500:
        return "C"
    if r <= 698:
        return "G"
    return "T"


def frequencies(seq: str, k: int) -> dict[str, int]:
    counts: dict[str, int] = {}
    for i in range(len(seq) - k + 1):
        key = seq[i : i + k]
        counts[key] = counts.get(key, 0) + 1
    return counts


def print_frequencies(seq: str, k: int) -> None:
    total = float(len(seq) - k + 1)
    for key, count in sorted(frequencies(seq, k).items(), key=lambda e: (-e[1], e[0])):
        print(f"{key} {100.0 * count / total:.3f}")
    print()


rng = Rng(42)
seq = "".join([random_base(rng) for _ in range(LENGTH)])

print_frequencies(seq, 1)
print_frequencies(seq, 2)
for query in QUERIES:
    print(f"{frequencies(seq, len(query)).get(query, 0)}\t{query}")
