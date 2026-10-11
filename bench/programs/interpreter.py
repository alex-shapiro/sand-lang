# A tree-walking interpreter for a tiny imperative language: a character-by-character
# lexer, a recursive-descent parser and an evaluator with string-keyed scope chains.
# Stresses object construction, structural pattern matching, recursion, exceptions,
# string scanning and dict lookups.

from dataclasses import dataclass
from typing import Callable

PARSE_ROUNDS = 200

SOURCE = """\
# Workload sizes.
let FIB_N = 21;
let PRIME_LIMIT = 10000;
let DIGITS = 3000;
let CALLS = 30000;

fn fib(n) {
    if n < 2 { return n; }
    return fib(n - 1) + fib(n - 2);
}

fn is_prime(n) {
    if n < 2 { return 0; }
    let d = 2;
    while d * d <= n {
        if n % d == 0 { return 0; }
        d = d + 1;
    }
    return 1;
}

fn count_primes(limit) {
    let count = 0;
    let last = 0;
    let n = 2;
    while n < limit {
        if is_prime(n) {
            count = count + 1;
            last = n;
        }
        n = n + 1;
    }
    print("primes below " + str(limit) + ": " + str(count) + ", largest " + str(last));
    return count;
}

fn digits(n) {
    let s = "";
    let i = 0;
    while i < n {
        let d = i * 7 % 10;
        if d == 0 { s = s + "-"; } else if d % 2 == 0 { s = s + str(d); } else { s = s + "."; }
        i = i + 1;
    }
    return s;
}

fn square(x) { return x * x; }

fn clamp(x, lo, hi) {
    if x < lo { return lo; }
    if x > hi { return hi; }
    return x;
}

fn mix(acc, x) { return (acc * 31 + clamp(square(x) % 1000, 100, 900)) % 1000003; }

fn hash_loop(n) {
    let acc = 0;
    let i = 0;
    while i < n {
        acc = mix(acc, i);
        i = i + 1;
    }
    return acc;
}

fn greet(name, times) {
    let out = "";
    let i = 0;
    while i < times {
        if i > 0 { out = out + ", "; }
        out = out + "hello " + name;
        i = i + 1;
    }
    return out;
}

print("fib(" + str(FIB_N) + ") = " + str(fib(FIB_N)));
count_primes(PRIME_LIMIT);
let s = digits(DIGITS);
print("digits: " + str(len(s)) + " chars, empty " + str(s == "") + ", long " + str(len(s) > 10));
print("hash: " + str(hash_loop(CALLS)));
let g = greet("sand", 3);
print(g);
print("logic: " + str(len(g) > 10 and not (fib(5) != 5) or 0) + " " + str(-7 / 2) + " " + str(-7 % 3));
"""

KEYWORDS = {"fn", "let", "if", "else", "while", "return", "and", "or", "not"}
TWO_CHAR_SYMS = {"==", "!=", "<=", ">="}
ONE_CHAR_SYMS = "+-*/%<>=(){},;"


class InterpError(Exception):
    pass


@dataclass(frozen=True, slots=True)
class Token:
    kind: str
    value: int | str | None = None


EOF_TOKEN = Token("eof")


class Lexer:
    __slots__ = ("src", "pos")

    def __init__(self, src: str):
        self.src = src
        self.pos = 0

    def tokens(self) -> list[Token]:
        src = self.src
        out = []
        while self.pos < len(src):
            c = src[self.pos]
            if c.isspace():
                self.pos += 1
            elif c == "#":
                while self.pos < len(src) and src[self.pos] != "\n":
                    self.pos += 1
            elif c.isdigit():
                out.append(Token("num", int(self.scan(str.isdigit))))
            elif c.isalpha() or c == "_":
                word = self.scan(lambda ch: ch.isalnum() or ch == "_")
                out.append(Token("kw" if word in KEYWORDS else "ident", word))
            elif c == '"':
                self.pos += 1
                text = self.scan(lambda ch: ch != '"')
                if self.pos >= len(src):
                    raise InterpError("unterminated string")
                self.pos += 1
                out.append(Token("str", text))
            else:
                two = src[self.pos : self.pos + 2]
                if two in TWO_CHAR_SYMS:
                    self.pos += 2
                    out.append(Token("sym", two))
                elif c in ONE_CHAR_SYMS:
                    self.pos += 1
                    out.append(Token("sym", c))
                else:
                    raise InterpError(f"unexpected character `{c}` at {self.pos}")
        out.append(EOF_TOKEN)
        return out

    def scan(self, accept: Callable[[str], bool]) -> str:
        start = self.pos
        while self.pos < len(self.src) and accept(self.src[self.pos]):
            self.pos += 1
        return self.src[start : self.pos]


@dataclass(frozen=True, slots=True)
class Num:
    value: int


@dataclass(frozen=True, slots=True)
class Str:
    value: str


@dataclass(frozen=True, slots=True)
class Var:
    name: str


@dataclass(frozen=True, slots=True)
class Neg:
    expr: "Expr"


@dataclass(frozen=True, slots=True)
class Not:
    expr: "Expr"


@dataclass(frozen=True, slots=True)
class And:
    left: "Expr"
    right: "Expr"


@dataclass(frozen=True, slots=True)
class Or:
    left: "Expr"
    right: "Expr"


@dataclass(frozen=True, slots=True)
class Bin:
    op: str
    left: "Expr"
    right: "Expr"


@dataclass(frozen=True, slots=True)
class Call:
    name: str
    args: list["Expr"]


Expr = Num | Str | Var | Neg | Not | And | Or | Bin | Call


@dataclass(frozen=True, slots=True)
class FnDef:
    name: str
    params: list[str]
    body: list["Stmt"]


@dataclass(frozen=True, slots=True)
class Let:
    name: str
    value: Expr


@dataclass(frozen=True, slots=True)
class Assign:
    name: str
    value: Expr


@dataclass(frozen=True, slots=True)
class If:
    cond: Expr
    then: list["Stmt"]
    otherwise: list["Stmt"]


@dataclass(frozen=True, slots=True)
class While:
    cond: Expr
    body: list["Stmt"]


@dataclass(frozen=True, slots=True)
class Func:
    fn: FnDef


@dataclass(frozen=True, slots=True)
class Return:
    value: Expr


@dataclass(frozen=True, slots=True)
class ExprStmt:
    expr: Expr


Stmt = Let | Assign | If | While | Func | Return | ExprStmt

COMPARISONS = {"==", "!=", "<", "<=", ">", ">="}


class Parser:
    __slots__ = ("tokens", "pos")

    def __init__(self, tokens: list[Token]):
        self.tokens = tokens
        self.pos = 0

    def peek(self) -> Token:
        return self.tokens[self.pos]

    def advance(self) -> Token:
        t = self.tokens[self.pos]
        if t != EOF_TOKEN:
            self.pos += 1
        return t

    def eat(self, want: Token) -> bool:
        if self.peek() == want:
            self.pos += 1
            return True
        return False

    def expect(self, want: Token) -> None:
        t = self.advance()
        if t != want:
            raise InterpError(f"expected {want}, found {t}")

    def ident(self) -> str:
        t = self.advance()
        if t.kind != "ident":
            raise InterpError(f"expected a name, found {t}")
        return t.value

    def program(self) -> list[Stmt]:
        stmts = []
        while self.peek() != EOF_TOKEN:
            stmts.append(self.statement())
        return stmts

    def block(self) -> list[Stmt]:
        self.expect(Token("sym", "{"))
        stmts = []
        while not self.eat(Token("sym", "}")):
            stmts.append(self.statement())
        return stmts

    def statement(self) -> Stmt:
        match self.peek():
            case Token("kw", "fn"):
                self.pos += 1
                name = self.ident()
                self.expect(Token("sym", "("))
                params = []
                if not self.eat(Token("sym", ")")):
                    while True:
                        params.append(self.ident())
                        if self.eat(Token("sym", ")")):
                            break
                        self.expect(Token("sym", ","))
                return Func(FnDef(name, params, self.block()))
            case Token("kw", "let"):
                self.pos += 1
                name = self.ident()
                self.expect(Token("sym", "="))
                value = self.expr()
                self.expect(Token("sym", ";"))
                return Let(name, value)
            case Token("kw", "if"):
                return self.if_statement()
            case Token("kw", "while"):
                self.pos += 1
                cond = self.expr()
                return While(cond, self.block())
            case Token("kw", "return"):
                self.pos += 1
                value = self.expr()
                self.expect(Token("sym", ";"))
                return Return(value)
            case Token("ident", name) if self.tokens[self.pos + 1] == Token("sym", "="):
                self.pos += 2
                value = self.expr()
                self.expect(Token("sym", ";"))
                return Assign(name, value)
            case _:
                e = self.expr()
                self.expect(Token("sym", ";"))
                return ExprStmt(e)

    def if_statement(self) -> Stmt:
        self.pos += 1
        cond = self.expr()
        then = self.block()
        if not self.eat(Token("kw", "else")):
            otherwise = []
        elif self.peek() == Token("kw", "if"):
            otherwise = [self.if_statement()]
        else:
            otherwise = self.block()
        return If(cond, then, otherwise)

    def expr(self) -> Expr:
        left = self.and_expr()
        while self.eat(Token("kw", "or")):
            left = Or(left, self.and_expr())
        return left

    def and_expr(self) -> Expr:
        left = self.comparison()
        while self.eat(Token("kw", "and")):
            left = And(left, self.comparison())
        return left

    def comparison(self) -> Expr:
        left = self.additive()
        t = self.peek()
        if t.kind == "sym" and t.value in COMPARISONS:
            self.pos += 1
            return Bin(t.value, left, self.additive())
        return left

    def additive(self) -> Expr:
        left = self.term()
        while (t := self.peek()).kind == "sym" and t.value in ("+", "-"):
            self.pos += 1
            left = Bin(t.value, left, self.term())
        return left

    def term(self) -> Expr:
        left = self.unary()
        while (t := self.peek()).kind == "sym" and t.value in ("*", "/", "%"):
            self.pos += 1
            left = Bin(t.value, left, self.unary())
        return left

    def unary(self) -> Expr:
        if self.eat(Token("sym", "-")):
            return Neg(self.unary())
        if self.eat(Token("kw", "not")):
            return Not(self.unary())
        return self.primary()

    def primary(self) -> Expr:
        match self.advance():
            case Token("num", n):
                return Num(n)
            case Token("str", s):
                return Str(s)
            case Token("ident", name):
                if not self.eat(Token("sym", "(")):
                    return Var(name)
                args = []
                if not self.eat(Token("sym", ")")):
                    while True:
                        args.append(self.expr())
                        if self.eat(Token("sym", ")")):
                            break
                        self.expect(Token("sym", ","))
                return Call(name, args)
            case Token("sym", "("):
                e = self.expr()
                self.expect(Token("sym", ")"))
                return e
            case t:
                raise InterpError(f"unexpected {t}")


Value = int | str | FnDef


def truthy(v: Value) -> bool:
    return v != 0 and v != ""


def display(v: Value) -> str:
    if isinstance(v, FnDef):
        return f"<fn {v.name}>"
    return str(v)


def binary(op: str, a: Value, b: Value) -> Value:
    match (op, a, b):
        case ("+", int(), int()) | ("+", str(), str()):
            return a + b
        case ("-", int(), int()):
            return a - b
        case ("*", int(), int()):
            return a * b
        case ("/" | "%", int(), 0):
            raise InterpError("division by zero")
        case ("/", int(), int()):
            return a // b
        case ("%", int(), int()):
            return a % b
        case ("==", _, _):
            return int(a == b)
        case ("!=", _, _):
            return int(a != b)
        case ("<", int(), int()):
            return int(a < b)
        case ("<=", int(), int()):
            return int(a <= b)
        case (">", int(), int()):
            return int(a > b)
        case (">=", int(), int()):
            return int(a >= b)
    raise InterpError(f"unsupported operands for {op}: {display(a)}, {display(b)}")


class Env:
    """A chain of scopes, innermost last; the first scope holds the globals."""

    __slots__ = ("scopes",)

    def __init__(self, scopes: list[dict[str, Value]]):
        self.scopes = scopes

    def lookup(self, name: str) -> Value:
        for scope in reversed(self.scopes):
            if name in scope:
                return scope[name]
        raise InterpError(f"undefined variable `{name}`")

    def assign(self, name: str, v: Value) -> None:
        for scope in reversed(self.scopes):
            if name in scope:
                scope[name] = v
                return
        raise InterpError(f"assignment to undefined variable `{name}`")

    def define(self, name: str, v: Value) -> None:
        self.scopes[-1][name] = v

    def run(self, stmts: list[Stmt]) -> Value | None:
        """Executes statements; returns the value of a `return`, or None."""
        for stmt in stmts:
            ret = self.exec(stmt)
            if ret is not None:
                return ret
        return None

    def run_scoped(self, stmts: list[Stmt]) -> Value | None:
        self.scopes.append({})
        ret = self.run(stmts)
        self.scopes.pop()
        return ret

    def exec(self, stmt: Stmt) -> Value | None:
        match stmt:
            case Let(name, e):
                self.define(name, self.eval(e))
            case Assign(name, e):
                self.assign(name, self.eval(e))
            case If(cond, then, otherwise):
                return self.run_scoped(then if truthy(self.eval(cond)) else otherwise)
            case While(cond, body):
                while truthy(self.eval(cond)):
                    ret = self.run_scoped(body)
                    if ret is not None:
                        return ret
            case Func(f):
                self.define(f.name, f)
            case Return(e):
                return self.eval(e)
            case ExprStmt(e):
                self.eval(e)
        return None

    def eval(self, e: Expr) -> Value:
        match e:
            case Num(n):
                return n
            case Str(s):
                return s
            case Var(name):
                return self.lookup(name)
            case Neg(inner):
                v = self.eval(inner)
                if not isinstance(v, int):
                    raise InterpError(f"cannot negate {display(v)}")
                return -v
            case Not(inner):
                return int(not truthy(self.eval(inner)))
            case And(l, r):
                return int(truthy(self.eval(l)) and truthy(self.eval(r)))
            case Or(l, r):
                return int(truthy(self.eval(l)) or truthy(self.eval(r)))
            case Bin(op, l, r):
                return binary(op, self.eval(l), self.eval(r))
            case Call(name, arg_exprs):
                return self.call(name, [self.eval(a) for a in arg_exprs])

    def call(self, name: str, args: list[Value]) -> Value:
        match (name, args):
            case ("print", [v]):
                print(display(v))
                return 0
            case ("len", [str() as s]):
                return len(s)
            case ("str", [v]):
                return display(v)
        f = self.lookup(name)
        if not isinstance(f, FnDef):
            raise InterpError(f"`{name}` is not a function")
        if len(f.params) != len(args):
            raise InterpError(f"`{name}` takes {len(f.params)} arguments, got {len(args)}")
        frame = dict(zip(f.params, args))
        ret = Env([self.scopes[0], frame]).run(f.body)
        return 0 if ret is None else ret


def parse(source: str) -> tuple[list[Token], list[Stmt]]:
    tokens = Lexer(source).tokens()
    return tokens, Parser(tokens).program()


token_total = 0
stmt_total = 0
for _ in range(PARSE_ROUNDS):
    tokens, program = parse(SOURCE)
    token_total += len(tokens)
    stmt_total += len(program)
print(f"parsed {PARSE_ROUNDS} times: {token_total} tokens, {stmt_total} statements")

_, program = parse(SOURCE)
Env([{}]).run(program)
