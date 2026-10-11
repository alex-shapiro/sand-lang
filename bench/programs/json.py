# Hand-written JSON parser and serializer: generate a document, then repeatedly parse it
# with a character-by-character recursive-descent parser, walk the tree, encode it back
# and check the round trip. Stresses string scanning, slicing and joining, object trees,
# exceptions, dict iteration and string building.

from dataclasses import dataclass

RECORDS = 2000
ITERATIONS = 3

type Json = None | bool | float | str | list["Json"] | dict[str, "Json"]


class Rng:
    __slots__ = ("state",)

    def __init__(self, seed: int):
        self.state = seed

    def next(self, n: int) -> int:
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state % n


FIRST_NAMES = ["Ada", "Zoë", "José", "Renée", "Linus", "Grace", "Bjørn", "Ming"]
CITIES = ["Montréal", "São Paulo", "Zürich", "Tokyo", "Berlin", "Lagos", "Kraków"]
TAGS = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]


def decimal(rng: Rng, max_cents: int) -> str:
    cents = rng.next(max_cents)
    return f"{cents // 100}.{cents % 100:02d}"


def record(rng: Rng, id: int) -> str:
    name = FIRST_NAMES[rng.next(len(FIRST_NAMES))]
    city = CITIES[rng.next(len(CITIES))]
    active = "true" if rng.next(2) == 0 else "false"
    score = decimal(rng, 100000)
    balance = decimal(rng, 5000000)
    rating = decimal(rng, 1000)
    manager = "null" if rng.next(4) == 0 else str(rng.next(id + 1))
    tags = [f'"{TAGS[rng.next(len(TAGS))]}"' for _ in range(rng.next(4))]
    lat = decimal(rng, 18000)
    lon = decimal(rng, 36000)
    history = []
    for _ in range(rng.next(5)):
        ts = 1700000000 + rng.next(10000000)
        amount = decimal(rng, 100000)
        ok = "false" if rng.next(3) == 0 else "true"
        history.append(f'{{"ts": {ts}, "amount": -{amount}, "ok": {ok}}}')
    return (
        f'  {{"id": {id}, "name": "{name} \\"#{id}\\"", '
        f'"email": "{name.lower()}.{id}@example.com", "active": {active}, '
        f'"score": {score}, "balance": -{balance}, "rating": {rating}e1, '
        f'"tags": [{", ".join(tags)}], '
        f'"address": {{"city": "{city}", "street": "{rng.next(999) + 1} Rue de l\\u00c9glise", '
        f'"geo": [{lat}, -{lon}]}}, '
        '"notes": "line one\\nline two\\ttabbed \\\\ path\\/to caf\\u00e9 \\u2603 é", '
        f'"manager": {manager}, "history": [{", ".join(history)}]}}'
    )


def generate(count: int) -> str:
    rng = Rng(42)
    return "[\n" + ",\n".join(record(rng, i) for i in range(count)) + "\n]\n"


class ParseError(Exception):
    pass


ESCAPES = {'"': '"', "\\": "\\", "/": "/", "b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t"}


class Parser:
    __slots__ = ("text", "pos")

    def __init__(self, text: str):
        self.text = text
        self.pos = 0

    @staticmethod
    def parse(text: str) -> Json:
        p = Parser(text)
        value = p.value()
        p.skip_ws()
        if p.pos < len(text):
            raise ParseError(f"trailing characters at {p.pos}")
        return value

    def skip_ws(self) -> None:
        text, pos = self.text, self.pos
        while pos < len(text) and text[pos] in " \t\n\r":
            pos += 1
        self.pos = pos

    def next_char(self) -> str:
        if self.pos >= len(self.text):
            return ""
        self.pos += 1
        return self.text[self.pos - 1]

    def expect(self, c: str) -> None:
        self.skip_ws()
        got = self.next_char()
        if got != c:
            raise ParseError(f"expected `{c}` at {self.pos}, got `{got}`")

    def value(self) -> Json:
        self.skip_ws()
        if self.pos >= len(self.text):
            raise ParseError("unexpected end of input")
        c = self.text[self.pos]
        if c == "{":
            return self.object()
        if c == "[":
            return self.array()
        if c == '"':
            return self.string()
        if c == "t":
            return self.literal("true", True)
        if c == "f":
            return self.literal("false", False)
        if c == "n":
            return self.literal("null", None)
        if c == "-" or c.isdigit():
            return self.number()
        raise ParseError(f"unexpected `{c}` at {self.pos}")

    def literal(self, word: str, value: Json) -> Json:
        end = self.pos + len(word)
        if self.text[self.pos:end] != word:
            raise ParseError(f"invalid literal at {self.pos}")
        self.pos = end
        return value

    def number(self) -> float:
        text, start = self.text, self.pos
        pos = start
        while pos < len(text) and text[pos] in "0123456789+-.eE":
            pos += 1
        self.pos = pos
        try:
            return float(text[start:pos])
        except ValueError:
            raise ParseError(f"invalid number `{text[start:pos]}`") from None

    def string(self) -> str:
        text = self.text
        parts = []
        pos = start = self.pos + 1
        while pos < len(text):
            c = text[pos]
            if c == '"':
                parts.append(text[start:pos])
                self.pos = pos + 1
                return "".join(parts)
            if c == "\\":
                parts.append(text[start:pos])
                self.pos = pos + 1
                parts.append(self.escape())
                pos = start = self.pos
            else:
                pos += 1
        raise ParseError("unterminated string")

    def escape(self) -> str:
        c = self.next_char()
        if c == "u":
            hex = self.text[self.pos : self.pos + 4]
            try:
                n = int(hex, 16)
            except ValueError:
                raise ParseError(f"bad \\u escape `{hex}`") from None
            self.pos += 4
            return chr(n)
        if c not in ESCAPES:
            raise ParseError(f"bad escape `\\{c}` at {self.pos}")
        return ESCAPES[c]

    def array(self) -> list[Json]:
        self.pos += 1
        items = []
        self.skip_ws()
        if self.pos < len(self.text) and self.text[self.pos] == "]":
            self.pos += 1
            return items
        while True:
            items.append(self.value())
            self.skip_ws()
            c = self.next_char()
            if c == "]":
                return items
            if c != ",":
                raise ParseError(f"expected `,` or `]` at {self.pos}, got `{c}`")

    def object(self) -> dict[str, Json]:
        self.pos += 1
        fields = {}
        self.skip_ws()
        if self.pos < len(self.text) and self.text[self.pos] == "}":
            self.pos += 1
            return fields
        while True:
            self.skip_ws()
            if self.pos >= len(self.text) or self.text[self.pos] != '"':
                raise ParseError(f"expected a key at {self.pos}")
            key = self.string()
            self.expect(":")
            fields[key] = self.value()
            self.skip_ws()
            c = self.next_char()
            if c == "}":
                return fields
            if c != ",":
                raise ParseError(f"expected `,` or `}}` at {self.pos}, got `{c}`")


@dataclass(slots=True)
class Stats:
    nulls: int = 0
    bools: int = 0
    nums: int = 0
    strs: int = 0
    arrs: int = 0
    objs: int = 0
    num_sum: float = 0.0
    str_chars: int = 0
    max_depth: int = 0

    @staticmethod
    def of(value: Json) -> "Stats":
        stats = Stats()
        stats.walk(value, 1)
        return stats

    def walk(self, value: Json, depth: int) -> None:
        self.max_depth = max(self.max_depth, depth)
        if value is None:
            self.nulls += 1
        elif isinstance(value, bool):
            self.bools += 1
        elif isinstance(value, float):
            self.nums += 1
            self.num_sum += value
        elif isinstance(value, str):
            self.strs += 1
            self.str_chars += len(value)
        elif isinstance(value, list):
            self.arrs += 1
            for item in value:
                self.walk(item, depth + 1)
        else:
            self.objs += 1
            for key, item in value.items():
                self.str_chars += len(key)
                self.walk(item, depth + 1)

    def describe(self) -> str:
        return (
            f"nulls={self.nulls} bools={self.bools} nums={self.nums} strs={self.strs} "
            f"arrs={self.arrs} objs={self.objs} sum={self.num_sum:.2f} "
            f"chars={self.str_chars} depth={self.max_depth}"
        )


ENCODE_ESCAPES = {'"': '\\"', "\\": "\\\\", "\n": "\\n", "\r": "\\r", "\t": "\\t"}
ENCODE_ESCAPES.update({chr(i): f"\\u{i:04x}" for i in range(0x20) if chr(i) not in ENCODE_ESCAPES})


def encode(value: Json) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, float):
        return encode_num(value)
    if isinstance(value, str):
        return encode_str(value)
    if isinstance(value, list):
        return "[" + ",".join(map(encode, value)) + "]"
    return "{" + ",".join(encode_str(k) + ":" + encode(v) for k, v in value.items()) + "}"


def encode_num(n: float) -> str:
    if n.is_integer() and abs(n) < 1e15:
        return str(int(n))
    return f"{n:.2f}"


def encode_str(s: str) -> str:
    return '"' + "".join(ENCODE_ESCAPES.get(c, c) for c in s) + '"'


text = generate(RECORDS)
print(f"document: {len(text)} chars")

total_chars = 0
for i in range(ITERATIONS):
    tree = Parser.parse(text)
    stats = Stats.of(tree)
    out = encode(tree)
    again = Parser.parse(out)
    assert encode(again) == out, "round trip changed the text"
    assert Stats.of(again) == stats, "round trip changed the stats"
    total_chars += len(out)
    if i == 0:
        print(stats.describe())
        print(encode(tree[0]))
print(f"encoded {ITERATIONS} times, {total_chars} chars")
