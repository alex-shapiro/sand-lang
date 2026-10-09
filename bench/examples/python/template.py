"""A small Jinja-style template engine.

CPython port of examples/template.sand. JSON values are plain Python values
(None, bool, float, str, list, dict); like Sand's `json.parse`, every number
is parsed as a float.

Usage: python3 template.py TEMPLATE DATA.json

Supported syntax:

    {{ user.name | upper }}            output with filters
    {{ missing | default("n/a") }}     filter arguments
    {% for item in items %}...{% endfor %}   with loop.index, loop.first, loop.last
    {% if cond %}...{% elif cond %}...{% else %}...{% endif %}
    {# comments #}

Conditions may be `path`, `not cond`, or `path OP literal-or-path` with
OP one of == != < <= > >=.
"""

import json
import re
import sys
from dataclasses import dataclass


class TemplateError(Exception):
    def __init__(self, message: str, kind: str):
        super().__init__(f"{kind}: {message}")


@dataclass(slots=True)
class Filter:
    name: str
    args: list


# Operands
@dataclass(slots=True)
class Path:
    parts: list[str]


@dataclass(slots=True)
class Literal:
    value: object


@dataclass(slots=True)
class Expr:
    value: Path | Literal
    filters: list[Filter]


# Conditions
@dataclass(slots=True)
class Truthy:
    expr: Expr


@dataclass(slots=True)
class Not:
    cond: "Cond"


@dataclass(slots=True)
class Compare:
    op: str
    left: Expr
    right: Expr


Cond = Truthy | Not | Compare


# Nodes
@dataclass(slots=True)
class Text:
    text: str


@dataclass(slots=True)
class Output:
    expr: Expr


@dataclass(slots=True)
class For:
    name: str
    iterable: Expr
    body: list["Node"]


@dataclass(slots=True)
class If:
    branches: list[tuple[Cond, list["Node"]]]
    otherwise: list["Node"]


Node = Text | Output | For | If


# Tokens
@dataclass(slots=True)
class TokText:
    text: str


@dataclass(slots=True)
class TokOut:
    text: str


@dataclass(slots=True)
class TokTag:
    text: str


# ---------------------------------------------------------------------------
# Lexing


def lex(src: str) -> list:
    pattern = re.compile(r"(?s)\{\{(.*?)\}\}|\{%(.*?)%\}|\{#.*?#\}")
    toks = []
    last = 0
    for m in pattern.finditer(src):
        if m.start() > last:
            toks.append(TokText(src[last : m.start()]))
        if (out := m.group(1)) is not None:
            toks.append(TokOut(out.strip()))
        elif (tag := m.group(2)) is not None:
            toks.append(TokTag(tag.strip()))
        last = m.end()
    if last < len(src):
        toks.append(TokText(src[last:]))
    return toks


# ---------------------------------------------------------------------------
# Parsing


NUMBER_CHARS = frozenset("0123456789.+-eE")


def parse_literal(text: str) -> Literal | None:
    t = text.strip()
    if len(t) >= 2 and (t.startswith('"') and t.endswith('"') or t.startswith("'") and t.endswith("'")):
        return Literal(t[1 : len(t) - 1])
    # Like Sand's `str.parse_float`: digits, sign, point and exponent only
    # (Python's float() would also accept "inf", "nan", "1_000", ...).
    if t and NUMBER_CHARS.issuperset(t):
        try:
            return Literal(float(t))
        except ValueError:
            pass
    match t:
        case "true":
            return Literal(True)
        case "false":
            return Literal(False)
        case "none" | "null":
            return Literal(None)
        case _:
            return None


def parse_operand(text: str) -> Path | Literal:
    if (lit := parse_literal(text)) is not None:
        return lit
    parts = text.strip().split(".")
    if any(not p or not p.replace("_", "a").isalnum() for p in parts):
        raise TemplateError(f"invalid expression `{text}`", "parse")
    return Path(parts)


def split_outside_quotes(text: str, sep: str) -> list[str]:
    """Splits on `sep` outside of quotes."""
    parts = []
    current = ""
    quote = ""
    for c in text:
        if quote != "":
            if c == quote:
                quote = ""
            current += c
        elif c == '"' or c == "'":
            quote = c
            current += c
        elif c == sep:
            parts.append(current)
            current = ""
        else:
            current += c
    parts.append(current)
    return parts


def parse_expr(text: str) -> Expr:
    pieces = split_outside_quotes(text, "|")
    value = parse_operand(pieces[0])
    filters = []
    for piece in pieces[1:]:
        p = piece.strip()
        name, paren, rest = p.partition("(")
        if paren:
            if not rest.endswith(")"):
                raise TemplateError(f"unclosed filter arguments in `{p}`", "parse")
            args = []
            for a in split_outside_quotes(rest[:-1], ","):
                lit = parse_literal(a)
                if lit is None:
                    raise TemplateError(f"filter arguments must be literals: `{a}`", "parse")
                args.append(lit.value)
            filters.append(Filter(name.strip(), args))
        else:
            filters.append(Filter(p, []))
    return Expr(value, filters)


def parse_cond(text: str) -> Cond:
    t = text.strip()
    if t.startswith("not "):
        return Not(parse_cond(t[4:]))
    for op in ["==", "!=", "<=", ">=", "<", ">"]:
        l, sep, r = t.partition(f" {op} ")
        if sep:
            return Compare(op, parse_expr(l), parse_expr(r))
    return Truthy(parse_expr(t))


class Parser:
    __slots__ = ("toks", "pos")

    def __init__(self, toks: list):
        self.toks = toks
        self.pos = 0

    def nodes(self, stops: list[str]) -> tuple[list[Node], str]:
        """Parses nodes until one of `stops` is reached; returns the nodes and the stop tag."""
        out = []
        while self.pos < len(self.toks):
            tok = self.toks[self.pos]
            self.pos += 1
            match tok:
                case TokText(t):
                    out.append(Text(t))
                case TokOut(e):
                    out.append(Output(parse_expr(e)))
                case TokTag(tag):
                    words = tag.split()
                    word = words[0] if words else ""
                    if word in stops:
                        return out, tag
                    out.append(self.block(tag, word))
        if not stops:
            return out, ""
        expected = "[" + ", ".join(f'"{s}"' for s in stops) + "]"
        raise TemplateError(f"unexpected end of template, expected one of {expected}", "parse")

    def block(self, tag: str, word: str) -> Node:
        match word:
            case "for":
                parts = tag.split()
                if len(parts) != 4 or parts[2] != "in":
                    raise TemplateError(f"malformed for tag `{tag}`", "parse")
                body, _ = self.nodes(["endfor"])
                return For(parts[1], parse_expr(parts[3]), body)
            case "if":
                branches = []
                cond = parse_cond(tag[2:])
                while True:
                    body, stop = self.nodes(["elif", "else", "endif"])
                    branches.append((cond, body))
                    if stop.startswith("elif"):
                        cond = parse_cond(stop[4:])
                    elif stop == "else":
                        otherwise, _ = self.nodes(["endif"])
                        return If(branches, otherwise)
                    else:
                        return If(branches, [])
            case _:
                raise TemplateError(f"unknown tag `{tag}`", "parse")


def compile_template(src: str) -> list[Node]:
    parser = Parser(lex(src))
    nodes, _ = parser.nodes([])
    return nodes


# ---------------------------------------------------------------------------
# Rendering


class Scope:
    __slots__ = ("frames",)

    def __init__(self, frames: list[dict]):
        self.frames = frames

    def lookup(self, path: list[str]):
        value = None
        for frame in reversed(self.frames):
            if path[0] in frame:
                value = frame[path[0]]
                break
        for key in path[1:]:
            match value:
                case dict():
                    value = value.get(key)
                case list():
                    try:
                        i = int(key)
                    except ValueError:
                        value = None
                    else:
                        value = value[i] if 0 <= i < len(value) else None
                case _:
                    value = None
        return value


def truthy(v) -> bool:
    # null, false, 0, "", [] and {} are falsy, as in Sand's `truthy`.
    return bool(v)


def fmt_float(v: float) -> str:
    """Formats a float the way Sand's `float.to_str()` does."""
    if v != v:
        return "NaN"
    s = repr(v)
    if "e" in s:
        mantissa, exp = s.split("e")
        s = f"{mantissa}e{int(exp)}"
    return s


def fmt_num(n: float) -> str:
    if n.is_integer() and abs(n) < 1e15:
        return str(int(n))
    return fmt_float(n)


def to_json(v):
    """Integral floats print without a fraction, as in Sand's `json.stringify`."""
    match v:
        case float() if v.is_integer() and abs(v) <= 2**53:
            return int(v)
        case list():
            return [to_json(x) for x in v]
        case dict():
            return {k: to_json(x) for k, x in v.items()}
        case _:
            return v


def display(v) -> str:
    match v:
        case None:
            return ""
        case bool():
            return "true" if v else "false"
        case float():
            return fmt_num(v)
        case str():
            return v
        case _:
            return json.dumps(to_json(v), separators=(",", ":"), ensure_ascii=False)


def escape_html(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def apply_filter(v, f: Filter):
    match f.name:
        case "upper":
            return display(v).upper()
        case "lower":
            return display(v).lower()
        case "title":
            return " ".join(w[:1].upper() + w[1:] for w in display(v).split(" "))
        case "length":
            match v:
                case str() | list() | dict():
                    return float(len(v))
                case _:
                    return 0.0
        case "default":
            if truthy(v):
                return v
            return f.args[0] if f.args else ""
        case "join":
            sep = f.args[0] if f.args and isinstance(f.args[0], str) else ""
            items = v if isinstance(v, list) else []
            return sep.join(display(x) for x in items)
        case "escape":
            return escape_html(display(v))
        case "trim":
            return display(v).strip()
        case _:
            raise TemplateError(f"unknown filter `{f.name}`", "invalid")


def eval_expr(e: Expr, scope: Scope):
    match e.value:
        case Path(p):
            v = scope.lookup(p)
        case Literal(j):
            v = j
    for f in e.filters:
        v = apply_filter(v, f)
    return v


def compare(op: str, a, b) -> bool:
    if isinstance(a, float) and isinstance(b, float):
        x, y = a, b
    else:
        x, y = display(a), display(b)
    match op:
        case "==":
            return x == y
        case "!=":
            return x != y
        case "<":
            return x < y
        case "<=":
            return x <= y
        case ">":
            return x > y
        case _:
            return x >= y


def eval_cond(c: Cond, scope: Scope) -> bool:
    match c:
        case Truthy(e):
            return truthy(eval_expr(e, scope))
        case Not(inner):
            return not eval_cond(inner, scope)
        case Compare(op, l, r):
            return compare(op, eval_expr(l, scope), eval_expr(r, scope))


def render_nodes(nodes: list[Node], scope: Scope, out: list[str]) -> None:
    for node in nodes:
        match node:
            case Text(t):
                out.append(t)
            case Output(e):
                out.append(display(eval_expr(e, scope)))
            case For(name, iterable, body):
                items = eval_expr(iterable, scope)
                if not isinstance(items, list):
                    items = []
                for i, item in enumerate(items):
                    loop_info = {
                        "index": float(i + 1),
                        "first": i == 0,
                        "last": i == len(items) - 1,
                    }
                    scope.frames.append({name: item, "loop": loop_info})
                    render_nodes(body, scope, out)
                    scope.frames.pop()
            case If(branches, otherwise):
                for cond, body in branches:
                    if eval_cond(cond, scope):
                        render_nodes(body, scope, out)
                        break
                else:
                    render_nodes(otherwise, scope, out)


def render(template: str, data) -> str:
    nodes = compile_template(template)
    root = data if isinstance(data, dict) else {}
    out = []
    render_nodes(nodes, Scope([root]), out)
    return "".join(out)


def main() -> None:
    args = sys.argv[1:]
    if len(args) != 2:
        print("usage: template.py TEMPLATE DATA.json", file=sys.stderr)
        sys.exit(2)
    try:
        with open(args[0], encoding="utf-8") as f:
            template = f.read()
        with open(args[1], encoding="utf-8") as f:
            data = json.loads(f.read(), parse_int=float)
        sys.stdout.write(render(template, data))
    except (TemplateError, OSError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)


main()
