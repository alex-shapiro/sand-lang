# Word-frequency analysis over a generated text of ~300k words.
# Stresses string splitting, lowercasing, slicing, concatenation,
# string-keyed dict updates and sorting by composite keys.

VOCAB_SIZE = 5000
WORD_COUNT = 300_000
SYLLABLES = [
    "ka", "to", "ri", "men", "sa", "lo", "ve", "qu", "an", "del",
    "pho", "ny", "ex", "ur", "bi", "ston", "gle", "ad", "ter", "op",
]
PUNCTUATION = [",", ".", ";", "!"]


class Rng:
    __slots__ = ("state",)

    def __init__(self, state: int):
        self.state = state

    def next(self, n: int) -> int:
        """A pseudo-random integer in range(n) (n at most 32768), from the high bits."""
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state // 65536 % n


def make_word(rng: Rng) -> str:
    return "".join(SYLLABLES[rng.next(len(SYLLABLES))] for _ in range(2 + rng.next(3)))


def make_token(rng: Rng, vocabulary: list[str]) -> str:
    """A vocabulary word as it appears in running text: skewed towards
    frequent words, sometimes capitalized, sometimes quoted or punctuated."""
    n = len(vocabulary)
    word = vocabulary[rng.next(n) * rng.next(n) // n]
    match rng.next(10):
        case 0:
            token = word[0].upper() + word[1:]
        case 1:
            token = word.upper()
        case _:
            token = word
    if rng.next(25) == 0:
        token = '"' + token + '"'
    p = rng.next(8)
    if p < len(PUNCTUATION):
        token += PUNCTUATION[p]
    return token


def make_text(rng: Rng, vocabulary: list[str]) -> str:
    lines = []
    written = 0
    while written < WORD_COUNT:
        n = min(8 + rng.next(9), WORD_COUNT - written)
        lines.append(" ".join([make_token(rng, vocabulary) for _ in range(n)]))
        written += n
    return "\n".join(lines)


def top(counts: dict[str, int], n: int) -> list[tuple[str, int]]:
    return sorted(counts.items(), key=lambda e: (-e[1], e[0]))[:n]


rng = Rng(42)
vocabulary = [make_word(rng) for _ in range(VOCAB_SIZE)]
text = make_text(rng, vocabulary)

words: dict[str, int] = {}
bigrams: dict[str, int] = {}
total = 0
for line in text.splitlines():
    previous = None
    for raw in line.split():
        word = raw.lower().strip(',.;!"')
        if not word:
            continue
        total += 1
        words[word] = words.get(word, 0) + 1
        if previous is not None:
            bigram = previous + " " + word
            bigrams[bigram] = bigrams.get(bigram, 0) + 1
        previous = word

print(f"total words: {total}")
print(f"distinct words: {len(words)}")
print(f"distinct bigrams: {len(bigrams)}")
print("top words:")
for word, count in top(words, 20):
    print(f"  {count} {word}")
print("top bigrams:")
for bigram, count in top(bigrams, 10):
    print(f"  {count} {bigram}")
