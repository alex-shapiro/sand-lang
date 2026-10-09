"""A tiny interpreter for a calculator language.

CPython port of examples/calc.sand. Reads one statement per line from
standard input:

    let r = 2
    pi * r ^ 2
    max(1, sqrt(16)) + -3

Supports + - * / % ^, parentheses, unary minus, variables and built-in
functions.
"""

import math
import sys
from dataclasses import dataclass


class CalcError(Exception):
    pass


def fmt_float(v: float) -> str:
    """Formats a float the way Sand's `float.to_str()` does."""
    if v != v:
        return "NaN"
    s = repr(v)
    if "e" in s:
        mantissa, exp = s.split("e")
        s = f"{mantissa}e{int(exp)}"
    return s


def quote(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


# ---------------------------------------------------------------------------
# Tokens


@dataclass(frozen=True, slots=True)
class Num:
    value: float

    def __str__(self) -> str:
        return f"Num({fmt_float(self.value)})"


@dataclass(frozen=True, slots=True)
class Ident:
    name: str

    def __str__(self) -> str:
        return f"Ident({quote(self.name)})"


@dataclass(frozen=True, slots=True)
class Op:
    op: str

    def __str__(self) -> str:
        return f"Op({quote(self.op)})"


@dataclass(frozen=True, slots=True)
class LParen:
    def __str__(self) -> str:
        return "LParen"


@dataclass(frozen=True, slots=True)
class RParen:
    def __str__(self) -> str:
        return "RParen"


@dataclass(frozen=True, slots=True)
class Comma:
    def __str__(self) -> str:
        return "Comma"


@dataclass(frozen=True, slots=True)
class Assign:
    def __str__(self) -> str:
        return "Assign"


@dataclass(frozen=True, slots=True)
class Let:
    def __str__(self) -> str:
        return "Let"


LPAREN, RPAREN, COMMA, ASSIGN, LET = LParen(), RParen(), Comma(), Assign(), Let()
Token = Num | Ident | Op | LParen | RParen | Comma | Assign | Let


DIGITS = "0123456789"  # Sand's `is_digit` is ASCII-only, unlike str.isdigit


class Lexer:
    __slots__ = ("src", "pos")

    def __init__(self, src: list[str]):
        self.src = src
        self.pos = 0

    def tokens(self) -> list[Token]:
        out = []
        while self.pos < len(self.src):
            c = self.src[self.pos]
            if c.isspace():
                self.pos += 1
            elif c in DIGITS or c == ".":
                out.append(Num(self.number()))
            elif c.isalpha() or c == "_":
                word = self.word()
                out.append(LET if word == "let" else Ident(word))
            else:
                self.pos += 1
                match c:
                    case "(":
                        out.append(LPAREN)
                    case ")":
                        out.append(RPAREN)
                    case ",":
                        out.append(COMMA)
                    case "=":
                        out.append(ASSIGN)
                    case "+" | "-" | "*" | "/" | "%" | "^":
                        out.append(Op(c))
                    case _:
                        raise CalcError(f"unexpected character `{c}` at {self.pos}")
        return out

    def number(self) -> float:
        start = self.pos
        while self.pos < len(self.src) and (self.src[self.pos] in DIGITS or self.src[self.pos] == "."):
            self.pos += 1
        text = "".join(self.src[start : self.pos])
        try:
            return float(text)
        except ValueError:
            raise CalcError(f"bad number `{text}`") from None

    def word(self) -> str:
        start = self.pos
        while self.pos < len(self.src) and (self.src[self.pos].isalnum() or self.src[self.pos] == "_"):
            self.pos += 1
        return "".join(self.src[start : self.pos])


# ---------------------------------------------------------------------------
# Syntax tree


@dataclass(slots=True)
class NumExpr:
    value: float


@dataclass(slots=True)
class Var:
    name: str


@dataclass(slots=True)
class Neg:
    inner: "Expr"


@dataclass(slots=True)
class Bin:
    op: str
    left: "Expr"
    right: "Expr"


@dataclass(slots=True)
class Call:
    name: str
    args: list["Expr"]


Expr = NumExpr | Var | Neg | Bin | Call


@dataclass(slots=True)
class LetStmt:
    name: str
    expr: Expr


@dataclass(slots=True)
class EvalStmt:
    expr: Expr


class Parser:
    __slots__ = ("tokens", "pos")

    def __init__(self, tokens: list[Token]):
        self.tokens = tokens
        self.pos = 0

    def peek(self) -> Token | None:
        return self.tokens[self.pos] if self.pos < len(self.tokens) else None

    def advance(self) -> Token | None:
        t = self.peek()
        self.pos += 1
        return t

    def expect(self, want: Token, what: str) -> None:
        t = self.advance()
        if t is None:
            raise CalcError(f"expected {what}, found end of input")
        if t != want:
            raise CalcError(f"expected {what}, found {t}")

    def statement(self) -> LetStmt | EvalStmt:
        match self.peek():
            case Let():
                self.pos += 1
                match self.advance():
                    case Ident(name):
                        pass
                    case _:
                        raise CalcError("expected a name after `let`")
                self.expect(ASSIGN, "`=`")
                stmt = LetStmt(name, self.expr(0))
            case _:
                stmt = EvalStmt(self.expr(0))
        t = self.peek()
        if t is not None:
            raise CalcError(f"unexpected {t}")
        return stmt

    def expr(self, min_prec: int) -> Expr:
        """Precedence climbing."""
        left = self.unary()
        while True:
            match self.peek():
                case Op(op):
                    pass
                case _:
                    break
            prec = precedence(op)
            if prec < min_prec:
                break
            self.pos += 1
            # `^` is right associative, everything else left associative.
            next_prec = prec if op == "^" else prec + 1
            right = self.expr(next_prec)
            left = Bin(op, left, right)
        return left

    def unary(self) -> Expr:
        match self.peek():
            case Op("-"):
                self.pos += 1
                return Neg(self.unary())
            case _:
                return self.primary()

    def primary(self) -> Expr:
        match self.advance():
            case Num(n):
                return NumExpr(n)
            case Ident(name):
                if self.peek() != LPAREN:
                    return Var(name)
                self.pos += 1
                args = []
                if self.peek() != RPAREN:
                    while True:
                        args.append(self.expr(0))
                        if self.peek() == COMMA:
                            self.pos += 1
                        else:
                            break
                self.expect(RPAREN, "`)`")
                return Call(name, args)
            case LParen():
                e = self.expr(0)
                self.expect(RPAREN, "`)`")
                return e
            case None:
                raise CalcError("unexpected end of input")
            case t:
                raise CalcError(f"unexpected {t}")


def precedence(op: str) -> int:
    match op:
        case "+" | "-":
            return 1
        case "*" | "/" | "%":
            return 2
        case _:
            return 3


# ---------------------------------------------------------------------------
# Evaluation. Sand floats follow IEEE 754 (NaN/inf instead of exceptions), so
# the few operations where Python raises are wrapped.


def fmod(a: float, b: float) -> float:
    return a % b if b != 0.0 else math.nan


def power(a: float, b: float) -> float:
    try:
        return math.pow(a, b)
    except ValueError:
        return math.inf if a == 0.0 else math.nan
    except OverflowError:
        return -math.inf if a < 0.0 and b % 2.0 == 1.0 else math.inf


class Interpreter:
    __slots__ = ("vars",)

    def __init__(self):
        self.vars: dict[str, float] = {"pi": math.pi, "e": math.e}

    def eval(self, e: Expr) -> float:
        match e:
            case NumExpr(n):
                return n
            case Var(name):
                try:
                    return self.vars[name]
                except KeyError:
                    raise CalcError(f"unknown variable `{name}`") from None
            case Neg(inner):
                return -self.eval(inner)
            case Bin(op, l, r):
                a = self.eval(l)
                b = self.eval(r)
                match op:
                    case "+":
                        return a + b
                    case "-":
                        return a - b
                    case "*":
                        return a * b
                    case "/":
                        if b == 0.0:
                            raise CalcError("division by zero")
                        return a / b
                    case "%":
                        return fmod(a, b)
                    case _:
                        return power(a, b)
            case Call(name, args):
                vals = []
                for a in args:
                    vals.append(self.eval(a))
                return call(name, vals)

    def run(self, line: str) -> str:
        tokens = Lexer(list(line)).tokens()
        parser = Parser(tokens)
        match parser.statement():
            case LetStmt(name, e):
                v = self.eval(e)
                self.vars[name] = v
                return f"{name} = {fmt_num(v)}"
            case EvalStmt(e):
                return fmt_num(self.eval(e))


def call(name: str, args: list[float]) -> float:
    match (name, args):
        case ("sqrt", [x]):
            return math.sqrt(x) if x >= 0.0 else math.nan
        case ("abs", [x]):
            return abs(x)
        case ("max", [_, *_]):
            return max(args)
        case ("min", [_, *_]):
            return min(args)
        case ("sqrt", _) | ("abs", _):
            raise CalcError(f"`{name}` takes 1 argument")
        case _:
            raise CalcError(f"unknown function `{name}`")


def fmt_num(v: float) -> str:
    if v.is_integer() and abs(v) < 1e15:
        return str(int(v))
    return fmt_float(v)


def main() -> None:
    interp = Interpreter()
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            print(interp.run(line))
        except CalcError as e:
            print(f"error: {e}")


main()
