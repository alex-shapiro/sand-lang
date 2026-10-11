# Record validation the way a config/ETL loader is written: generate `key=value;...` records,
# a fraction malformed in various ways, parse each with small functions that raise on bad
# input, then aggregate valid records and count errors by kind.
# Stresses exceptions, small calls, string splitting and dict updates.

from dataclasses import dataclass

RECORDS = 150_000
FIRST = ["Ada", "Alan", "Grace", "Edsger", "Barbara", "Donald", "Margaret", "Ken", "Radia", "Niklaus"]
LAST = ["Lovelace", "Turing", "Hopper", "Dijkstra", "Liskov", "Knuth", "Hamilton", "Thompson", "Perlman"]
DOMAINS = ["example.com", "mail.org", "post.net", "uni.edu", "corp.io", "inbox.dev", "lab.ai"]
TAGS = ["admin", "dev", "ops", "qa", "sales", "hr", "ml", "web", "db", "sec", "mobile"]
FIELDS = ["id", "name", "age", "email", "tags", "score"]


class Rng:
    __slots__ = ("state",)

    def __init__(self, seed: int):
        self.state = seed

    def next(self, n: int) -> int:
        self.state = (self.state * 1103515245 + 12345) % 2147483648
        return self.state // 65536 % n


def generate(rng: Rng, index: int) -> str:
    first = FIRST[rng.next(len(FIRST))]
    last = LAST[rng.next(len(LAST))]
    id = str(index + 1)
    if rng.next(100) < 3 and index > 0:
        id = str(1 + rng.next(index))
    age = str(18 + rng.next(60))
    email = f"{first.lower()}.{last.lower()}@{DOMAINS[rng.next(len(DOMAINS))]}"
    tags = [TAGS[rng.next(len(TAGS))] for _ in range(1 + rng.next(4))]
    points = rng.next(1001)
    score = f"{points // 10}.{points % 10}"
    separator = "="
    dropped = ""
    match rng.next(100):
        case 0 | 1:
            dropped = FIELDS[rng.next(len(FIELDS))]
        case 2 | 3:
            age = f"{age}x"
        case 4:
            id = f"#{id}"
        case 5 | 6:
            age = str(131 + rng.next(100))
        case 7:
            age = f"-{age}"
        case 8 | 9:
            email = email.replace("@", "")
        case 10:
            email = f"{first.lower()}@localhost"
        case 11 | 12:
            tags.insert(rng.next(len(tags) + 1), "")
        case 13:
            score = f"{score}.5"
        case 14:
            score = f"1{score}"
        case 15:
            separator = ":"
    values = [id, f"{first} {last}", age, email, ",".join(tags), score]
    pairs = []
    for key, value in zip(FIELDS, values):
        if key == dropped:
            continue
        pairs.append(f"{key}{separator}{value}" if key == "age" else f"{key}={value}")
    return ";".join(pairs)


class RecordError(Exception):
    def __init__(self, message: str, kind: str):
        super().__init__(message)
        self.kind = kind

    def __str__(self) -> str:
        return f"{self.kind}: {self.args[0]}"


@dataclass(slots=True)
class Person:
    id: int
    name: str
    age: int
    user: str
    domain: str
    tags: list[str]
    score: float


def parse_fields(line: str) -> dict[str, str]:
    fields = {}
    for part in line.split(";"):
        key, sep, value = part.partition("=")
        if not sep:
            raise RecordError(f"expected key=value, got `{part}`", "syntax")
        fields[key] = value
    return fields


def field(fields: dict[str, str], key: str) -> str:
    try:
        return fields[key]
    except KeyError:
        raise RecordError(f"missing field `{key}`", "missing") from None


def parse_int_field(fields: dict[str, str], key: str) -> int:
    text = field(fields, key)
    try:
        return int(text)
    except ValueError:
        raise RecordError(f"field `{key}`: `{text}` is not an integer", "int") from None


def validate_age(age: int) -> int:
    if age < 0 or age > 130:
        raise RecordError(f"field `age`: {age} out of range", "range")
    return age


def validate_email(email: str) -> tuple[str, str]:
    user, at, domain = email.partition("@")
    if not at:
        raise RecordError(f"bad email `{email}`: no @", "email")
    if not user or "." not in domain or domain.startswith(".") or domain.endswith("."):
        raise RecordError(f"bad email `{email}`: bad domain", "email")
    return user, domain


def parse_tags(text: str) -> list[str]:
    tags = text.split(",")
    if any(not t for t in tags):
        raise RecordError(f"empty tag in `{text}`", "tag")
    return tags


def parse_score(text: str) -> float:
    try:
        score = float(text)
    except ValueError:
        raise RecordError(f"field `score`: `{text}` is not a number", "score") from None
    if score < 0.0 or score > 100.0:
        raise RecordError(f"field `score`: `{text}` out of range", "range")
    return score


def parse_person(line: str) -> Person:
    fields = parse_fields(line)
    id = parse_int_field(fields, "id")
    name = field(fields, "name")
    age = validate_age(parse_int_field(fields, "age"))
    user, domain = validate_email(field(fields, "email"))
    tags = parse_tags(field(fields, "tags"))
    score = parse_score(field(fields, "score"))
    return Person(id, name, age, user, domain, tags, score)


def load(line: str, people: dict[int, Person]) -> Person:
    person = parse_person(line)
    if person.id in people:
        raise RecordError(f"duplicate id {person.id}", "duplicate")
    people[person.id] = person
    return person


def top(counts: dict[str, int], n: int) -> str:
    ranked = sorted(counts.items(), key=lambda e: (-e[1], e[0]))
    return " ".join(f"{k}={v}" for k, v in ranked[:n])


rng = Rng(1234)
lines = [generate(rng, i) for i in range(RECORDS)]

people: dict[int, Person] = {}
domains: dict[str, int] = {}
tag_counts: dict[str, int] = {}
error_counts: dict[str, int] = {}
first_errors: list[str] = []
total_age = 0
total_score = 0.0
for number, line in enumerate(lines):
    try:
        person = load(line, people)
    except RecordError as e:
        error_counts[e.kind] = error_counts.get(e.kind, 0) + 1
        if len(first_errors) < 8:
            first_errors.append(f"line {number + 1}: {e}")
        continue
    domains[person.domain] = domains.get(person.domain, 0) + 1
    for tag in person.tags:
        tag_counts[tag] = tag_counts.get(tag, 0) + 1
    total_age += person.age
    total_score += person.score

valid = len(people)
best = max(people.values(), key=lambda p: (p.score, -p.id))
print(f"records {len(lines)}, valid {valid}, rejected {len(lines) - valid}")
print("errors: " + " ".join(f"{k}={error_counts[k]}" for k in sorted(error_counts)))
print(f"domains: {top(domains, 5)}")
print(f"tags: {top(tag_counts, 5)}")
print(f"average age {total_age / valid:.2f}, average score {total_score / valid:.3f}")
print(f"best: #{best.id} {best.name} <{best.user}@{best.domain}> {best.score:.1f} {','.join(best.tags)}")
for message in first_errors:
    print(message)
