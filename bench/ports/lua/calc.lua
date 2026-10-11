-- A tiny interpreter for a calculator language.
--
-- Lua port of examples/calc.sand. Reads one statement per line from
-- standard input:
--
--     let r = 2
--     pi * r ^ 2
--     max(1, sqrt(16)) + -3
--
-- Supports + - * / % ^, parentheses, unary minus, variables and built-in
-- functions. Errors are raised as `CalcError` tables and caught per line.
--
-- Usage: lua5.5 calc.lua < SCRIPT

local CalcError = {}
CalcError.__tostring = function(e) return e.message end

local function fail(message)
  error(setmetatable({ message = message }, CalcError), 0)
end

--- Formats a float the way Sand's `float.to_str()` does: the shortest
--- round-tripping digits, in exponent form below 1e-4 and from 1e16 up.
local function fmt_float(v)
  if v ~= v then return "NaN" end
  if v == math.huge then return "inf" end
  if v == -math.huge then return "-inf" end
  local s
  for precision = 14, 16 do
    s = string.format("%." .. precision .. "e", v)
    if tonumber(s) == v then break end
  end
  local sign, first, rest, exp = s:match("^(-?)(%d)%.?(%d*)e([-+]%d+)$")
  local digits = (first .. rest):gsub("0+$", "")
  if digits == "" then digits = "0" end
  exp = tonumber(exp)
  if exp < -4 or exp >= 16 then
    local mantissa = #digits > 1 and digits:sub(1, 1) .. "." .. digits:sub(2) or digits
    return sign .. mantissa .. "e" .. exp
  elseif exp < 0 then
    return sign .. "0." .. string.rep("0", -exp - 1) .. digits
  elseif exp + 1 >= #digits then
    return sign .. digits .. string.rep("0", exp + 1 - #digits) .. ".0"
  end
  return sign .. digits:sub(1, exp + 1) .. "." .. digits:sub(exp + 2)
end

local function quote(s)
  return '"' .. s:gsub('[\\"]', "\\%0") .. '"'
end

-- ---------------------------------------------------------------------------
-- Tokens: `{ kind = "num", value = ... }`, `{ kind = "ident", name = ... }`,
-- `{ kind = "op", op = ... }`, or one of the punctuation singletons.

local function token_str(t)
  if t.kind == "num" then return "Num(" .. fmt_float(t.value) .. ")" end
  if t.kind == "ident" then return "Ident(" .. quote(t.name) .. ")" end
  if t.kind == "op" then return "Op(" .. quote(t.op) .. ")" end
  return t.kind
end

local LPAREN = { kind = "LParen" }
local RPAREN = { kind = "RParen" }
local COMMA = { kind = "Comma" }
local ASSIGN = { kind = "Assign" }
local LET = { kind = "Let" }

local PUNCTUATION = { ["("] = LPAREN, [")"] = RPAREN, [","] = COMMA, ["="] = ASSIGN }
local OPERATORS = { ["+"] = true, ["-"] = true, ["*"] = true, ["/"] = true, ["%"] = true, ["^"] = true }

--- Parses a decimal literal as a float, or returns nil.
local function parse_float(text)
  local n = tonumber(text)
  if math.type(n) == "integer" then
    return tonumber(text .. ".0")
  end
  return n
end

--- Splits a line into tokens. Non-ASCII characters count as letters.
local function tokens(src)
  local out = {}
  local pos = 1
  local len = #src
  while pos <= len do
    local c = src:sub(pos, pos)
    if c:find("^%s") then
      pos = pos + 1
    elseif c:find("^[%d.]") then
      local stop = select(2, src:find("^[%d.]+", pos))
      local text = src:sub(pos, stop)
      local value = parse_float(text)
      if not value then fail("bad number `" .. text .. "`") end
      out[#out + 1] = { kind = "num", value = value }
      pos = stop + 1
    elseif c:find("^[%a_\128-\255]") then
      local stop = select(2, src:find("^[%w_\128-\255]+", pos))
      local word = src:sub(pos, stop)
      out[#out + 1] = word == "let" and LET or { kind = "ident", name = word }
      pos = stop + 1
    else
      local t = PUNCTUATION[c] or (OPERATORS[c] and { kind = "op", op = c })
      if not t then fail("unexpected character `" .. c .. "` at " .. utf8.len(src, 1, pos)) end
      out[#out + 1] = t
      pos = pos + 1
    end
  end
  return out
end

-- ---------------------------------------------------------------------------
-- Syntax tree: `{ kind = "num" | "var" | "neg" | "bin" | "call", ... }`.

local function precedence(op)
  if op == "+" or op == "-" then return 1 end
  if op == "*" or op == "/" or op == "%" then return 2 end
  return 3
end

local Parser = {}
Parser.__index = Parser

function Parser.new(toks)
  return setmetatable({ tokens = toks, pos = 1 }, Parser)
end

function Parser:peek()
  return self.tokens[self.pos]
end

function Parser:advance()
  local t = self.tokens[self.pos]
  self.pos = self.pos + 1
  return t
end

function Parser:expect(want, what)
  local t = self:advance()
  if t == nil then fail("expected " .. what .. ", found end of input") end
  if t ~= want then fail("expected " .. what .. ", found " .. token_str(t)) end
end

function Parser:statement()
  local stmt
  if self:peek() == LET then
    self.pos = self.pos + 1
    local name = self:advance()
    if name == nil or name.kind ~= "ident" then fail("expected a name after `let`") end
    self:expect(ASSIGN, "`=`")
    stmt = { kind = "let", name = name.name, expr = self:expr(0) }
  else
    stmt = { kind = "eval", expr = self:expr(0) }
  end
  local t = self:peek()
  if t ~= nil then fail("unexpected " .. token_str(t)) end
  return stmt
end

--- Precedence climbing.
function Parser:expr(min_prec)
  local left = self:unary()
  while true do
    local t = self:peek()
    if t == nil or t.kind ~= "op" then break end
    local op = t.op
    local prec = precedence(op)
    if prec < min_prec then break end
    self.pos = self.pos + 1
    -- `^` is right associative, everything else left associative.
    local right = self:expr(op == "^" and prec or prec + 1)
    left = { kind = "bin", op = op, left = left, right = right }
  end
  return left
end

function Parser:unary()
  local t = self:peek()
  if t ~= nil and t.kind == "op" and t.op == "-" then
    self.pos = self.pos + 1
    return { kind = "neg", inner = self:unary() }
  end
  return self:primary()
end

function Parser:primary()
  local t = self:advance()
  if t == nil then fail("unexpected end of input") end
  if t.kind == "num" then
    return { kind = "num", value = t.value }
  elseif t.kind == "ident" then
    if self:peek() ~= LPAREN then
      return { kind = "var", name = t.name }
    end
    self.pos = self.pos + 1
    local args = {}
    if self:peek() ~= RPAREN then
      while true do
        args[#args + 1] = self:expr(0)
        if self:peek() == COMMA then
          self.pos = self.pos + 1
        else
          break
        end
      end
    end
    self:expect(RPAREN, "`)`")
    return { kind = "call", name = t.name, args = args }
  elseif t == LPAREN then
    local e = self:expr(0)
    self:expect(RPAREN, "`)`")
    return e
  end
  fail("unexpected " .. token_str(t))
end

-- ---------------------------------------------------------------------------
-- Evaluation. Lua's float arithmetic matches Sand's: IEEE 754 results
-- (NaN and infinities rather than errors) and floor modulo.

local function call(name, args)
  local n = #args
  if name == "sqrt" and n == 1 then
    return math.sqrt(args[1])
  elseif name == "abs" and n == 1 then
    return math.abs(args[1])
  elseif name == "max" and n >= 1 then
    return math.max(table.unpack(args))
  elseif name == "min" and n >= 1 then
    return math.min(table.unpack(args))
  elseif name == "sqrt" or name == "abs" then
    fail("`" .. name .. "` takes 1 argument")
  end
  fail("unknown function `" .. name .. "`")
end

local Interpreter = {}
Interpreter.__index = Interpreter

function Interpreter.new()
  return setmetatable({ vars = { pi = math.pi, e = 2.718281828459045 } }, Interpreter)
end

function Interpreter:eval(e)
  local kind = e.kind
  if kind == "num" then
    return e.value
  elseif kind == "var" then
    local v = self.vars[e.name]
    if v == nil then fail("unknown variable `" .. e.name .. "`") end
    return v
  elseif kind == "neg" then
    return -self:eval(e.inner)
  elseif kind == "bin" then
    local a = self:eval(e.left)
    local b = self:eval(e.right)
    local op = e.op
    if op == "+" then return a + b
    elseif op == "-" then return a - b
    elseif op == "*" then return a * b
    elseif op == "/" then
      if b == 0.0 then fail("division by zero") end
      return a / b
    elseif op == "%" then return a % b
    end
    return a ^ b
  end
  local vals = {}
  for i, arg in ipairs(e.args) do
    vals[i] = self:eval(arg)
  end
  return call(e.name, vals)
end

local function fmt_num(v)
  if v == math.floor(v) and math.abs(v) < 1e15 then
    return string.format("%d", v)
  end
  return fmt_float(v)
end

function Interpreter:run(line)
  local stmt = Parser.new(tokens(line)):statement()
  if stmt.kind == "let" then
    local v = self:eval(stmt.expr)
    self.vars[stmt.name] = v
    return stmt.name .. " = " .. fmt_num(v)
  end
  return fmt_num(self:eval(stmt.expr))
end

local interp = Interpreter.new()
for line in io.lines() do
  if line:find("%S") then
    local ok, result = pcall(interp.run, interp, line)
    if ok then
      io.write(result, "\n")
    elseif getmetatable(result) == CalcError then
      io.write("error: ", result.message, "\n")
    else
      error(result, 0)
    end
  end
end
