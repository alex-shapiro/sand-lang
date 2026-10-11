"""Deterministic input generators for the example benchmarks.

Everything here is a pure function of a fixed seed, so the Sand program and
its ports always see byte-identical inputs, and a run is reproducible.

`run.py` imports this module; it can also be run on its own to materialize
all inputs (useful for profiling a single workload by hand):

    python3.14 bench/gen.py OUT_DIR
"""

import json
import random
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = ROOT / "examples"
PORTS = Path(__file__).resolve().parent / "ports"

# Syllables for synthetic but word-like vocabulary.
_SYLLABLES = [
    "ka",
    "lo",
    "mi",
    "ra",
    "te",
    "su",
    "na",
    "vo",
    "ze",
    "pi",
    "do",
    "ge",
    "bu",
    "fa",
    "ri",
    "sho",
    "tan",
    "mel",
    "kor",
    "vin",
    "dra",
    "lis",
    "pon",
    "quar",
    "eth",
    "ul",
    "or",
    "an",
    "ex",
    "ist",
]


def vocabulary(rng: random.Random, n: int) -> list[str]:
    words: list[str] = []
    seen = set()
    while len(words) < n:
        w = "".join(
            rng.choice(_SYLLABLES) for _ in range(rng.choice([1, 2, 2, 3, 3, 4]))
        )
        if w not in seen:
            seen.add(w)
            words.append(w)
    return words


def zipf_weights(n: int, s: float = 1.05) -> list[float]:
    return [1.0 / (i + 1) ** s for i in range(n)]


def _substitute(path: Path, old: str, new: str) -> str:
    text = path.read_text()
    if text.count(old) != 1:
        raise SystemExit(
            f"gen.py: expected exactly one occurrence of the input block in {path}"
        )
    return text.replace(old, new)


# ---------------------------------------------------------------------------
# life: the glider board is baked into the program, so the bigger board is a
# variant with a different literal and otherwise identical code.

GLIDER_BLOCK = """\
    .#......
    ..#.....
    ###.....
    ........
    ........
    ........
"""


def life_board(
    width: int = 64, height: int = 64, density: float = 0.35, seed: int = 1
) -> str:
    rng = random.Random(seed)
    return "".join(
        "    "
        + "".join("#" if rng.random() < density else "." for _ in range(width))
        + "\n"
        for _ in range(height)
    )


def _write_variants(out: Path, name: str, block: str, data: str) -> dict[str, Path]:
    """The example `name` and its ports with `block` replaced by `data`, by implementation."""
    sources = {"sand": EXAMPLES / f"{name}.sand", "python": PORTS / "python" / f"{name}.py", "lua": PORTS / "lua" / f"{name}.lua"}
    paths = {}
    for impl, src in sources.items():
        paths[impl] = out / f"{name}_bench{src.suffix}"
        paths[impl].write_text(_substitute(src, block, data))
    return paths


def write_life_bench(out: Path) -> dict[str, Path]:
    return _write_variants(out, "life", GLIDER_BLOCK, life_board())


# ---------------------------------------------------------------------------
# ledger: same approach, a variant with a much longer baked-in ledger.

LEDGER_BLOCK = """\
    2024-01, food, groceries, 54.20
    2024-01, rent, january, 950.00
    2024-01, food, cafe, 7.5
    2024-02, food, groceries, 61.05
    2024-02, travel, train, 38.40
    2024-02, rent, february, 950.00
    2024-02, food, restaurant, 42.00
"""

_CATEGORIES = {
    "food": ["groceries", "cafe", "restaurant", "bakery", "market", "takeaway"],
    "rent": ["rent"],
    "travel": ["train", "bus", "taxi", "flight", "ferry"],
    "utilities": ["power", "water", "gas", "internet", "phone"],
    "health": ["pharmacy", "dentist", "gym", "doctor"],
    "fun": ["cinema", "concert", "books", "games", "museum"],
    "home": ["furniture", "tools", "cleaning", "garden"],
    "gifts": ["birthday", "wedding", "charity"],
    "clothes": ["shoes", "jacket", "shirts"],
    "kids": ["school", "toys", "daycare"],
    "pets": ["vet", "petfood"],
    "car": ["fuel", "parking", "repair", "insurance"],
}


def ledger_lines(n: int = 120_000, seed: int = 2) -> str:
    rng = random.Random(seed)
    cats = list(_CATEGORIES)
    lines = []
    for i in range(n):
        year = 2015 + (i * 10) // n
        month = f"{year}-{rng.randint(1, 12):02}"
        cat = rng.choice(cats)
        what = rng.choice(_CATEGORIES[cat])
        if cat == "rent":
            what = f"rent{rng.randint(1, 12)}"
        cents = int(rng.lognormvariate(3.2, 1.1) * 100) + 1
        match rng.randrange(4):
            case 0:
                amount = f"{cents // 100}.{cents % 100:02}"
            case 1:
                amount = f"{cents // 100}.{(cents % 100) // 10}"
            case 2:
                amount = f"{cents // 100}"
            case _:
                amount = f"{cents // 100}.{cents % 100:02}"
        lines.append(f"    {month}, {cat}, {what}, {amount}\n")
        if i % 997 == 0:
            lines.append("    this line is not an expense\n")
    return "".join(lines)


def write_ledger_bench(out: Path) -> dict[str, Path]:
    return _write_variants(out, "ledger", LEDGER_BLOCK, ledger_lines())


# ---------------------------------------------------------------------------
# calc: a long script of statements, with a sprinkling of every error path.


def calc_script(n: int = 40_000, seed: int = 3) -> str:
    rng = random.Random(seed)
    defined: list[str] = []

    def number() -> str:
        match rng.randrange(4):
            case 0:
                return str(rng.randint(0, 9))
            case 1:
                return str(rng.randint(10, 999))
            case 2:
                return f"{rng.randint(0, 99)}.{rng.randint(0, 99)}"
            case _:
                return f"{rng.randint(1, 9)}.{rng.randint(0, 9)}"

    def small() -> str:
        # Bounded operands for `^`, so nothing overflows to infinity.
        if defined and rng.random() < 0.3:
            return rng.choice(defined)
        return str(rng.randint(1, 9))

    def expr(depth: int) -> str:
        if depth <= 0 or rng.random() < 0.25:
            if defined and rng.random() < 0.4:
                return rng.choice(defined + ["pi", "e"])
            return number()
        match rng.randrange(10):
            case 0 | 1 | 2:
                op = rng.choice(["+", "-", "+", "-", "*", "*", "/"])
                return f"{expr(depth - 1)} {op} {expr(depth - 1)}"
            case 3:
                return f"({expr(depth - 1)})"
            case 4:
                return f"-{expr(depth - 1)}"
            case 5:
                return f"{small()} ^ {rng.randint(0, 3)}"
            case 6:
                return f"{expr(depth - 1)} % {rng.randint(1, 9)}"
            case 7:
                return f"sqrt(abs({expr(depth - 1)}))"
            case 8:
                f = rng.choice(["max", "min"])
                return f"{f}({', '.join(expr(depth - 1) for _ in range(rng.randint(1, 4)))})"
            case _:
                return f"abs({expr(depth - 1)}) * {number()}"

    lines = []
    for i in range(n):
        r = rng.random()
        if r < 0.15:
            name = f"v{rng.randint(0, 40)}"
            # Variables stay small so `^` can't overflow.
            lines.append(
                f"let {name} = {rng.randint(0, 20)} + {rng.randint(0, 9)} / {rng.randint(1, 9)}"
            )
            if name not in defined:
                defined.append(name)
        elif r < 0.97:
            lines.append(expr(4))
        else:
            lines.append(
                rng.choice(
                    [
                        "undefined_var + 1",
                        "1 +",
                        "sqrt(1, 2)",
                        "foo(1)",
                        "3 $ 4",
                        "(1, 2)",
                        "1 2",
                        "let = 3",
                        "let x 3",
                        "1.2.3",
                        "max()",
                        "10 / 0",
                        "(1 + 2",
                        "abs()",
                        "",
                    ]
                )
            )
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# adventure: wander around (every command, including the unknown and the
# failing ones) for a long time, then win.

_ADVENTURE_LOOP = [
    "look",
    "take lamp",
    "inventory",
    "go north",
    "look",
    "e",
    "take book",
    "drop book",
    "use lamp",
    "s",
    "drop lamp",
    "i",
    "east",
    "take bread",
    "take key",
    "look",
    "drop bread",
    "w",
    "drop key",
    "go west",
    "dance",
    "help",
    "take nothing",
    "drop nothing",
    "use nothing",
    "",
    "LOOK",
    "  go   north  ",
    "south",
    "l",
]
_ADVENTURE_WIN = [
    "take lamp",
    "take key",
    "n",
    "use key",
    "east",
]  # the key was dropped in the hall


def adventure_script(loops: int = 6_000) -> str:
    return "\n".join(_ADVENTURE_LOOP * loops + _ADVENTURE_WIN) + "\n"


# ---------------------------------------------------------------------------
# template: a big data set rendered through a template that uses every
# feature, and a huge template compiled against a small data set.

TEMPLATE_RENDER = """\
<h1>{{ title | upper }}</h1>
{# a report of every user #}
<table>
{% for u in users %}<tr class="{% if loop.first %}first{% elif loop.last %}last{% else %}row{% endif %}">
  <td>{{ loop.index }}</td><td>{{ u.name | title }}</td><td>{{ u.email | lower | escape }}</td>
  <td>{% if u.admin %}admin{% elif u.age >= 65 %}senior{% elif not u.active %}inactive{% else %}user{% endif %}</td>
  <td>{{ u.tags | join(", ") }} ({{ u.tags | length }})</td><td>{{ u.tags.0 | default("-") }}</td>
  <td>{{ u.address.city | default("unknown") }} {{ u.address.zip }}</td>
  <td>{% for t in u.tags %}<span>{{ t | upper }}</span>{% if not loop.last %}, {% endif %}{% endfor %}</td>
  <td>{% if u.score > 50 %}high{% else %}low{% endif %} {{ u.score }}</td>
  <td>{% if u.name == "ann lee" %}founder{% endif %}{% if u.team != team %}external{% endif %}</td>
  <td>{{ u.bio | trim | escape }}</td>
</tr>
{% endfor %}</table>
{{ users | length }} users, {{ missing | default("n/a") }}, {{ "<done>" | escape }} {{ count }}
"""


def template_data(n: int = 12_000, seed: int = 4) -> dict:
    rng = random.Random(seed)
    first = [
        "ann",
        "bob",
        "cara",
        "dan",
        "eve",
        "finn",
        "gus",
        "hana",
        "ivo",
        "jo",
        "kim",
        "lea",
    ]
    last = [
        "lee",
        "smith",
        "o'brien",
        "nguyen",
        "garcia",
        "müller",
        "kowalski",
        "de la cruz",
    ]
    cities = ["Oslo", "Lima", "Kyoto", "Accra", "Perth", "Quito", "Riga"]
    tags = ["ops", "dev", "qa", "design", "sales", "<lead>", "remote", "oncall"]
    users = []
    for i in range(n):
        u: dict = {
            "name": f"{rng.choice(first)} {rng.choice(last)}" if i else "ann lee",
            "email": f"User{i}@Example.COM",
            "admin": rng.random() < 0.05,
            "age": rng.randint(18, 90),
            "active": rng.random() < 0.8,
            "tags": rng.sample(tags, rng.randint(0, 4)),
            "score": rng.choice([rng.randint(0, 100), round(rng.uniform(0, 100), 2)]),
            "team": rng.choice(["core", "core", "web"]),
            "bio": "  "
            + " ".join(
                rng.choice(["likes", "<b>tea</b>", "&", "maps", '"quotes"'])
                for _ in range(5)
            )
            + " ",
        }
        if rng.random() < 0.9:
            u["address"] = {"city": rng.choice(cities), "zip": rng.randint(1000, 99999)}
        users.append(u)
    return {"title": "Team report", "users": users, "count": n, "team": "core"}


def template_compile_source(sections: int = 12_000, seed: int = 5) -> str:
    rng = random.Random(seed)
    parts = ["<html>{# generated #}\n"]
    for i in range(sections):
        match rng.randrange(5):
            case 0:
                parts.append(
                    f'<p id="s{i}">{{{{ title | upper }}}} {{{{ \'x|y\' | default("a, b") }}}}</p>\n'
                )
            case 1:
                parts.append(
                    "{% if count > 3 %}<b>{{ count }}</b>{% elif count == 1 %}one{% else %}{{ missing | default('none') }}{% endif %}\n"
                )
            case 2:
                parts.append(
                    "<ul>{% for t in tags %}<li>{{ loop.index }}. {{ t | title | escape }}</li>{% endfor %}</ul>\n"
                )
            case 3:
                parts.append(
                    f"{{# comment {i} #}}<span>{{{{ user.name | lower }}}} / {{{{ user.age }}}}</span>\n"
                )
            case _:
                parts.append(
                    "{% if not user.admin %}{% if user.age >= 18 %}adult{% endif %}{% else %}admin{% endif %} "
                    '{{ tags | join(" | ") }} {{ tags | length }}\n'
                )
    parts.append("</html>\n")
    return "".join(parts)


TEMPLATE_COMPILE_DATA = {
    "title": "Small",
    "count": 5,
    "tags": ["red", "green", "blue"],
    "user": {"name": "Ann LEE", "age": 41, "admin": False},
}


# ---------------------------------------------------------------------------
# search_index: a corpus of documents in nested directories.


def search_corpus(out: Path, docs: int = 600, seed: int = 6) -> Path:
    rng = random.Random(seed)
    vocab = vocabulary(rng, 6_000)
    weights = zipf_weights(len(vocab))
    stop = ["the", "a", "of", "to", "and", "in", "is", "it", "on", "for", "an"]
    root = out / "corpus"
    for i in range(docs):
        sub = root / f"part{i % 7}" / (f"sec{i % 3}" if i % 2 else "")
        sub.mkdir(parents=True, exist_ok=True)
        ext = ["txt", "txt", "md", "log"][i % 4]
        n = rng.randint(80, 900)
        body = rng.choices(vocab, weights, k=n)
        words = []
        for j, w in enumerate(body):
            if rng.random() < 0.25:
                words.append(rng.choice(stop))
            if rng.random() < 0.1:
                w = w.capitalize()
            words.append(w + rng.choice(["", "", "", "", ",", ".", ";", "!"]))
            if j % 97 == 0:
                words.append(str(rng.randint(0, 2024)))
        (sub / f"doc{i:04}.{ext}").write_text(" ".join(words) + "\n")
    return root


def search_queries(n: int = 6) -> list[list[str]]:
    """`n` queries of a few terms each, mixing common, rare, stop and unknown words."""
    rng = random.Random(6)
    vocab = vocabulary(rng, 6_000)
    queries = []
    for i in range(n):
        q = [
            vocab[i],
            vocab[3 + i].upper(),
            vocab[17 * (i + 1)],
            vocab[150 + i],
            vocab[999 - i],
            vocab[4000 + i],
        ]
        q += ["the", "zzzz"] if i % 2 else [vocab[42 + i] + "!"]
        queries.append(q)
    queries.append(["zebra"])
    return queries


def search_add_files(corpus: Path) -> list[str]:
    """Already-indexed documents, so each `add` re-indexes (remove + add)."""
    files = sorted(p for p in corpus.rglob("*") if p.suffix in (".txt", ".md"))
    return [str(p.relative_to(corpus.parent)) for p in files[::15]]


# ---------------------------------------------------------------------------
# http_fetch: big HTML pages for the local server.


def html_page(i: int, seed: int = 7) -> str:
    rng = random.Random(seed * 1000 + i)
    vocab = vocabulary(rng, 400)
    out = [
        f"<!doctype html>\n<html><head><meta charset=utf-8><title>Page {i} &amp; <b>friends</b></title></head>\n<body>\n"
    ]
    for s in range(60):
        level = rng.randint(1, 3)
        out.append(
            f'<h{level} class="h">Section {s} <em>{rng.choice(vocab)}</em> &lt;{rng.choice(vocab)}&gt;</h{level}>\n'
        )
        for _ in range(rng.randint(5, 15)):
            text = " ".join(rng.choices(vocab, k=rng.randint(10, 40)))
            out.append(f'<p>{text}\n  <span class="x">{rng.choice(vocab)}</span></p>\n')
            r = rng.randrange(8)
            href = [
                f"/page/{rng.randint(0, 500)}",
                f"rel/{rng.choice(vocab)}.html",
                f"../up/{rng.choice(vocab)}?q={rng.randint(0, 9)}&amp;x=1",
                f"https://other{rng.randint(0, 20)}.example/{rng.choice(vocab)}",
                f"#frag{rng.randint(0, 9)}",
                "mailto:someone@example.com",
                f"//cdn.example/{rng.choice(vocab)}.js",
                f"?page={rng.randint(0, 30)}",
            ][r]
            quote = rng.choice(['"', "'"])
            out.append(
                f'<a class="l" href={quote}{href}{quote} title="t">{rng.choice(vocab)}</a>\n'
            )
    out.append("</body></html>\n")
    return "".join(out)


# ---------------------------------------------------------------------------


def write_all(out: Path) -> None:
    """Writes every generated input to `out` (for manual runs and profiling)."""
    out.mkdir(parents=True, exist_ok=True)
    write_life_bench(out)
    write_ledger_bench(out)
    (out / "calc.txt").write_text(calc_script())
    (out / "adventure.txt").write_text(adventure_script())
    (out / "render.tmpl").write_text(TEMPLATE_RENDER)
    (out / "render.json").write_text(json.dumps(template_data(), ensure_ascii=False))
    (out / "compile.tmpl").write_text(template_compile_source())
    (out / "compile.json").write_text(json.dumps(TEMPLATE_COMPILE_DATA))
    search_corpus(out)
    pages = out / "pages"
    pages.mkdir(exist_ok=True)
    for i in range(4):
        (pages / f"page{i}.html").write_text(html_page(i))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: gen.py OUT_DIR")
    write_all(Path(sys.argv[1]))
    print(f"wrote inputs to {sys.argv[1]}")
