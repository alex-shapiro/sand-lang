-- A tree-walking interpreter for a tiny imperative language: a character-by-character
-- lexer, a recursive-descent parser and an evaluator with string-keyed scope chains.
-- Stresses table construction, string dispatch, recursion, string scanning and
-- table lookups.

local PARSE_ROUNDS = 200

local SOURCE = [[
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
]]

local KEYWORDS = {
  ["fn"] = true, ["let"] = true, ["if"] = true, ["else"] = true, ["while"] = true,
  ["return"] = true, ["and"] = true, ["or"] = true, ["not"] = true,
}
local TWO_CHAR_SYMS = { ["=="] = true, ["!="] = true, ["<="] = true, [">="] = true }
local ONE_CHAR_SYMS = "+-*/%<>=(){},;"

local function tokenize(src)
  local out = {}
  local pos = 1
  local n = #src

  local function scan(accept)
    local start = pos
    while pos <= n and accept(src:sub(pos, pos)) do
      pos = pos + 1
    end
    return src:sub(start, pos - 1)
  end

  while pos <= n do
    local c = src:sub(pos, pos)
    if c:match("%s") then
      pos = pos + 1
    elseif c == "#" then
      while pos <= n and src:sub(pos, pos) ~= "\n" do
        pos = pos + 1
      end
    elseif c:match("%d") then
      local text = scan(function(ch) return ch:match("%d") end)
      out[#out + 1] = { kind = "num", value = math.tointeger(tonumber(text)) }
    elseif c:match("[%a_]") then
      local word = scan(function(ch) return ch:match("[%w_]") end)
      out[#out + 1] = { kind = KEYWORDS[word] and "kw" or "ident", value = word }
    elseif c == '"' then
      pos = pos + 1
      local text = scan(function(ch) return ch ~= '"' end)
      if pos > n then error("unterminated string") end
      pos = pos + 1
      out[#out + 1] = { kind = "str", value = text }
    else
      local two = src:sub(pos, pos + 1)
      if TWO_CHAR_SYMS[two] then
        pos = pos + 2
        out[#out + 1] = { kind = "sym", value = two }
      elseif ONE_CHAR_SYMS:find(c, 1, true) then
        pos = pos + 1
        out[#out + 1] = { kind = "sym", value = c }
      else
        error("unexpected character `" .. c .. "` at " .. pos)
      end
    end
  end
  out[#out + 1] = { kind = "eof" }
  return out
end

local Parser = {}
Parser.__index = Parser

function Parser.new(tokens)
  return setmetatable({ tokens = tokens, pos = 1 }, Parser)
end

function Parser:peek()
  return self.tokens[self.pos]
end

function Parser:advance()
  local t = self.tokens[self.pos]
  if t.kind ~= "eof" then
    self.pos = self.pos + 1
  end
  return t
end

local function show_token(t)
  return t.kind .. "(" .. tostring(t.value) .. ")"
end

function Parser:eat(kind, value)
  local t = self.tokens[self.pos]
  if t.kind == kind and t.value == value then
    self.pos = self.pos + 1
    return true
  end
  return false
end

function Parser:expect(kind, value)
  local t = self:advance()
  if t.kind ~= kind or t.value ~= value then
    error("expected " .. value .. ", found " .. show_token(t))
  end
end

function Parser:ident()
  local t = self:advance()
  if t.kind ~= "ident" then
    error("expected a name, found " .. show_token(t))
  end
  return t.value
end

function Parser:program()
  local stmts = {}
  while self:peek().kind ~= "eof" do
    stmts[#stmts + 1] = self:statement()
  end
  return stmts
end

function Parser:block()
  self:expect("sym", "{")
  local stmts = {}
  while not self:eat("sym", "}") do
    stmts[#stmts + 1] = self:statement()
  end
  return stmts
end

function Parser:statement()
  local t = self:peek()
  if t.kind == "kw" and t.value == "fn" then
    self.pos = self.pos + 1
    local name = self:ident()
    self:expect("sym", "(")
    local params = {}
    if not self:eat("sym", ")") then
      while true do
        params[#params + 1] = self:ident()
        if self:eat("sym", ")") then break end
        self:expect("sym", ",")
      end
    end
    return { tag = "func", def = { name = name, params = params, body = self:block() } }
  elseif t.kind == "kw" and t.value == "let" then
    self.pos = self.pos + 1
    local name = self:ident()
    self:expect("sym", "=")
    local value = self:expr()
    self:expect("sym", ";")
    return { tag = "let", name = name, value = value }
  elseif t.kind == "kw" and t.value == "if" then
    return self:if_statement()
  elseif t.kind == "kw" and t.value == "while" then
    self.pos = self.pos + 1
    local cond = self:expr()
    return { tag = "while", cond = cond, body = self:block() }
  elseif t.kind == "kw" and t.value == "return" then
    self.pos = self.pos + 1
    local value = self:expr()
    self:expect("sym", ";")
    return { tag = "return", value = value }
  end
  local next_token = self.tokens[self.pos + 1]
  if t.kind == "ident" and next_token.kind == "sym" and next_token.value == "=" then
    self.pos = self.pos + 2
    local value = self:expr()
    self:expect("sym", ";")
    return { tag = "assign", name = t.value, value = value }
  end
  local e = self:expr()
  self:expect("sym", ";")
  return { tag = "expr", expr = e }
end

function Parser:if_statement()
  self.pos = self.pos + 1
  local cond = self:expr()
  local thenb = self:block()
  local otherwise
  if not self:eat("kw", "else") then
    otherwise = {}
  else
    local t = self:peek()
    if t.kind == "kw" and t.value == "if" then
      otherwise = { self:if_statement() }
    else
      otherwise = self:block()
    end
  end
  return { tag = "if", cond = cond, thenb = thenb, otherwise = otherwise }
end

function Parser:expr()
  local left = self:and_expr()
  while self:eat("kw", "or") do
    left = { tag = "or", left = left, right = self:and_expr() }
  end
  return left
end

function Parser:and_expr()
  local left = self:comparison()
  while self:eat("kw", "and") do
    left = { tag = "and", left = left, right = self:comparison() }
  end
  return left
end

local COMPARISONS = { ["=="] = true, ["!="] = true, ["<"] = true, ["<="] = true, [">"] = true, [">="] = true }

function Parser:comparison()
  local left = self:additive()
  local t = self:peek()
  if t.kind == "sym" and COMPARISONS[t.value] then
    self.pos = self.pos + 1
    return { tag = "bin", op = t.value, left = left, right = self:additive() }
  end
  return left
end

function Parser:additive()
  local left = self:term()
  while true do
    local t = self:peek()
    if t.kind ~= "sym" or (t.value ~= "+" and t.value ~= "-") then break end
    self.pos = self.pos + 1
    left = { tag = "bin", op = t.value, left = left, right = self:term() }
  end
  return left
end

function Parser:term()
  local left = self:unary()
  while true do
    local t = self:peek()
    if t.kind ~= "sym" or (t.value ~= "*" and t.value ~= "/" and t.value ~= "%") then break end
    self.pos = self.pos + 1
    left = { tag = "bin", op = t.value, left = left, right = self:unary() }
  end
  return left
end

function Parser:unary()
  if self:eat("sym", "-") then
    return { tag = "neg", expr = self:unary() }
  elseif self:eat("kw", "not") then
    return { tag = "not", expr = self:unary() }
  end
  return self:primary()
end

function Parser:primary()
  local t = self:advance()
  if t.kind == "num" then
    return { tag = "num", value = t.value }
  elseif t.kind == "str" then
    return { tag = "str", value = t.value }
  elseif t.kind == "ident" then
    if not self:eat("sym", "(") then
      return { tag = "var", name = t.value }
    end
    local args = {}
    if not self:eat("sym", ")") then
      while true do
        args[#args + 1] = self:expr()
        if self:eat("sym", ")") then break end
        self:expect("sym", ",")
      end
    end
    return { tag = "call", name = t.value, args = args }
  elseif t.kind == "sym" and t.value == "(" then
    local e = self:expr()
    self:expect("sym", ")")
    return e
  end
  error("unexpected " .. show_token(t))
end

local function truthy(v)
  return v ~= 0 and v ~= ""
end

local function display(v)
  if type(v) == "table" then
    return "<fn " .. v.name .. ">"
  end
  return tostring(v)
end

local function of_bool(b)
  return b and 1 or 0
end

local function binary(op, a, b)
  local ints = math.type(a) == "integer" and math.type(b) == "integer"
  if op == "+" then
    if ints then return a + b end
    if type(a) == "string" and type(b) == "string" then return a .. b end
  elseif op == "-" then
    if ints then return a - b end
  elseif op == "*" then
    if ints then return a * b end
  elseif op == "/" or op == "%" then
    if ints then
      if b == 0 then error("division by zero") end
      if op == "/" then return a // b end
      return a % b
    end
  elseif op == "==" then
    return of_bool(a == b)
  elseif op == "!=" then
    return of_bool(a ~= b)
  elseif ints then
    if op == "<" then return of_bool(a < b) end
    if op == "<=" then return of_bool(a <= b) end
    if op == ">" then return of_bool(a > b) end
    if op == ">=" then return of_bool(a >= b) end
  end
  error("unsupported operands for " .. op .. ": " .. display(a) .. ", " .. display(b))
end

-- A chain of scopes, innermost last; the first scope holds the globals.
local Env = {}
Env.__index = Env

function Env.new(scopes)
  return setmetatable({ scopes = scopes }, Env)
end

function Env:lookup(name)
  local scopes = self.scopes
  for i = #scopes, 1, -1 do
    local v = scopes[i][name]
    if v ~= nil then return v end
  end
  error("undefined variable `" .. name .. "`")
end

function Env:assign(name, v)
  local scopes = self.scopes
  for i = #scopes, 1, -1 do
    if scopes[i][name] ~= nil then
      scopes[i][name] = v
      return
    end
  end
  error("assignment to undefined variable `" .. name .. "`")
end

function Env:define(name, v)
  self.scopes[#self.scopes][name] = v
end

-- Executes statements; returns the value of a `return`, or nil.
function Env:run(stmts)
  for i = 1, #stmts do
    local ret = self:exec(stmts[i])
    if ret ~= nil then return ret end
  end
  return nil
end

function Env:run_scoped(stmts)
  local scopes = self.scopes
  scopes[#scopes + 1] = {}
  local ret = self:run(stmts)
  scopes[#scopes] = nil
  return ret
end

function Env:exec(stmt)
  local tag = stmt.tag
  if tag == "let" then
    self:define(stmt.name, self:eval(stmt.value))
  elseif tag == "assign" then
    self:assign(stmt.name, self:eval(stmt.value))
  elseif tag == "if" then
    if truthy(self:eval(stmt.cond)) then
      return self:run_scoped(stmt.thenb)
    end
    return self:run_scoped(stmt.otherwise)
  elseif tag == "while" then
    while truthy(self:eval(stmt.cond)) do
      local ret = self:run_scoped(stmt.body)
      if ret ~= nil then return ret end
    end
  elseif tag == "func" then
    self:define(stmt.def.name, stmt.def)
  elseif tag == "return" then
    return self:eval(stmt.value)
  elseif tag == "expr" then
    self:eval(stmt.expr)
  end
  return nil
end

function Env:eval(e)
  local tag = e.tag
  if tag == "num" or tag == "str" then
    return e.value
  elseif tag == "var" then
    return self:lookup(e.name)
  elseif tag == "neg" then
    local v = self:eval(e.expr)
    if math.type(v) ~= "integer" then error("cannot negate " .. display(v)) end
    return -v
  elseif tag == "not" then
    return of_bool(not truthy(self:eval(e.expr)))
  elseif tag == "and" then
    return of_bool(truthy(self:eval(e.left)) and truthy(self:eval(e.right)))
  elseif tag == "or" then
    return of_bool(truthy(self:eval(e.left)) or truthy(self:eval(e.right)))
  elseif tag == "bin" then
    return binary(e.op, self:eval(e.left), self:eval(e.right))
  elseif tag == "call" then
    local args = {}
    for i, a in ipairs(e.args) do
      args[i] = self:eval(a)
    end
    return self:call(e.name, args)
  end
  error("unknown expression " .. tostring(tag))
end

function Env:call(name, args)
  if name == "print" and #args == 1 then
    print(display(args[1]))
    return 0
  elseif name == "len" and #args == 1 and type(args[1]) == "string" then
    return #args[1]
  elseif name == "str" and #args == 1 then
    return display(args[1])
  end
  local f = self:lookup(name)
  if type(f) ~= "table" then
    error("`" .. name .. "` is not a function")
  end
  if #f.params ~= #args then
    error("`" .. name .. "` takes " .. #f.params .. " arguments, got " .. #args)
  end
  local frame = {}
  for i, param in ipairs(f.params) do
    frame[param] = args[i]
  end
  local ret = Env.new({ self.scopes[1], frame }):run(f.body)
  if ret == nil then return 0 end
  return ret
end

local function parse(source)
  local tokens = tokenize(source)
  return tokens, Parser.new(tokens):program()
end

local token_total, stmt_total = 0, 0
for _ = 1, PARSE_ROUNDS do
  local tokens, program = parse(SOURCE)
  token_total = token_total + #tokens
  stmt_total = stmt_total + #program
end
print(string.format("parsed %d times: %d tokens, %d statements", PARSE_ROUNDS, token_total, stmt_total))

local _, program = parse(SOURCE)
Env.new({ {} }):run(program)
