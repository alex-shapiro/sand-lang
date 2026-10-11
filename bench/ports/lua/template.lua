-- A small Jinja-style template engine.
--
-- Lua port of examples/template.sand. JSON values come from json.lua next to
-- this script: json.null, booleans, floats (every JSON number, as in Sand),
-- strings, and tables marked as arrays or objects. Like Lua's string library,
-- the `upper`, `lower` and `title` filters change only ASCII letters.
--
-- Usage: lua5.5 template.lua TEMPLATE DATA.json
--
-- Supported syntax:
--
--     {{ user.name | upper }}            output with filters
--     {{ missing | default("n/a") }}     filter arguments
--     {% for item in items %}...{% endfor %}   with loop.index, loop.first, loop.last
--     {% if cond %}...{% elif cond %}...{% else %}...{% endif %}
--     {# comments #}
--
-- Conditions may be `path`, `not cond`, or `path OP literal-or-path` with
-- OP one of == != < <= > >=.

package.path = (arg[0]:match("^(.*)[/\\]") or ".") .. "/?.lua;" .. package.path
local json = require("json")

local null = json.null
local find, sub, byte, format = string.find, string.sub, string.byte, string.format

--- Errors reported to the user as `error: MESSAGE`.
local Error = {}
Error.__index = Error
Error.__tostring = function(e) return e.message end

local function fail(message, kind)
  error(setmetatable({ message = kind and kind .. ": " .. message or message }, Error), 0)
end

local function trim(s)
  return s:match("^%s*(.-)%s*$")
end

--- Splits `s` on every occurrence of the plain string `sep`.
local function split(s, sep)
  local parts = {}
  local start = 1
  while true do
    local i, j = find(s, sep, start, true)
    if not i then break end
    parts[#parts + 1] = sub(s, start, i - 1)
    start = j + 1
  end
  parts[#parts + 1] = sub(s, start)
  return parts
end

local function words(s)
  local out = {}
  for w in s:gmatch("%S+") do out[#out + 1] = w end
  return out
end

--- Parses a decimal literal as a float, or returns nil.
local function parse_float(text)
  local n = tonumber(text)
  if math.type(n) == "integer" then
    return tonumber(text .. ".0")
  end
  return n
end

-- ---------------------------------------------------------------------------
-- Lexing: `{ kind = "text" | "out" | "tag", text = ... }` tokens.

local CLOSERS = { ["{"] = "}}", ["%"] = "%}", ["#"] = "#}" }
local TOKEN_KINDS = { ["{"] = "out", ["%"] = "tag" }

local function lex(src)
  local toks = {}
  local last = 1
  local pos = 1
  while true do
    local start = find(src, "{[{%%#]", pos)
    if not start then break end
    local opener = sub(src, start + 1, start + 1)
    local close = find(src, CLOSERS[opener], start + 2, true)
    if close then
      if start > last then
        toks[#toks + 1] = { kind = "text", text = sub(src, last, start - 1) }
      end
      local kind = TOKEN_KINDS[opener]
      if kind then
        toks[#toks + 1] = { kind = kind, text = trim(sub(src, start + 2, close - 1)) }
      end
      last = close + 2
      pos = last
    else
      pos = start + 1
    end
  end
  if last <= #src then
    toks[#toks + 1] = { kind = "text", text = sub(src, last) }
  end
  return toks
end

-- ---------------------------------------------------------------------------
-- Parsing

--- The value of a literal, or nil if `text` is not one.
local function parse_literal(text)
  local t = trim(text)
  local q = sub(t, 1, 1)
  if #t >= 2 and (q == '"' or q == "'") and sub(t, -1) == q then
    return sub(t, 2, -2)
  end
  -- Like Sand's `str.parse_float`: digits, sign, point and exponent only.
  if t ~= "" and not find(t, "[^%d.+%-eE]") then
    local n = parse_float(t)
    if n then return n end
  end
  if t == "true" then
    return true
  elseif t == "false" then
    return false
  elseif t == "none" or t == "null" then
    return null
  end
  return nil
end

--- `{ kind = "literal", value = ... }` or `{ kind = "path", parts = { ... } }`.
local function parse_operand(text)
  local lit = parse_literal(text)
  if lit ~= nil then
    return { kind = "literal", value = lit }
  end
  local parts = split(trim(text), ".")
  for _, p in ipairs(parts) do
    if not find(p, "^[%w_\128-\255]+$") then
      fail("invalid expression `" .. text .. "`", "parse")
    end
  end
  return { kind = "path", parts = parts }
end

--- Splits on `sep` outside of quotes.
local function split_outside_quotes(text, sep)
  local parts = {}
  local start = 1
  local quote = nil
  sep = byte(sep)
  for i = 1, #text do
    local c = byte(text, i)
    if quote then
      if c == quote then quote = nil end
    elseif c == 34 or c == 39 then
      quote = c
    elseif c == sep then
      parts[#parts + 1] = sub(text, start, i - 1)
      start = i + 1
    end
  end
  parts[#parts + 1] = sub(text, start)
  return parts
end

--- `{ value = operand, filters = { { name = ..., args = { ... } }, ... } }`.
local function parse_expr(text)
  local pieces = split_outside_quotes(text, "|")
  local value = parse_operand(pieces[1])
  local filters = {}
  for i = 2, #pieces do
    local p = trim(pieces[i])
    local paren = find(p, "(", 1, true)
    if paren then
      local rest = sub(p, paren + 1)
      if sub(rest, -1) ~= ")" then
        fail("unclosed filter arguments in `" .. p .. "`", "parse")
      end
      local args = {}
      for _, a in ipairs(split_outside_quotes(sub(rest, 1, -2), ",")) do
        local lit = parse_literal(a)
        if lit == nil then
          fail("filter arguments must be literals: `" .. a .. "`", "parse")
        end
        args[#args + 1] = lit
      end
      filters[#filters + 1] = { name = trim(sub(p, 1, paren - 1)), args = args }
    else
      filters[#filters + 1] = { name = p, args = {} }
    end
  end
  return { value = value, filters = filters }
end

local COMPARISONS = { "==", "!=", "<=", ">=", "<", ">" }

--- `{ kind = "truthy", expr = ... }`, `{ kind = "not", cond = ... }` or
--- `{ kind = "compare", op = ..., left = ..., right = ... }`.
local function parse_cond(text)
  local t = trim(text)
  if sub(t, 1, 4) == "not " then
    return { kind = "not", cond = parse_cond(sub(t, 5)) }
  end
  for _, op in ipairs(COMPARISONS) do
    local i, j = find(t, " " .. op .. " ", 1, true)
    if i then
      return { kind = "compare", op = op, left = parse_expr(sub(t, 1, i - 1)), right = parse_expr(sub(t, j + 1)) }
    end
  end
  return { kind = "truthy", expr = parse_expr(t) }
end

-- Nodes: `{ kind = "text", text = ... }`, `{ kind = "output", expr = ... }`,
-- `{ kind = "for", name = ..., iterable = ..., body = ... }` and
-- `{ kind = "if", branches = { { cond, body }, ... }, otherwise = ... }`.

local Parser = {}
Parser.__index = Parser

function Parser.new(toks)
  return setmetatable({ toks = toks, pos = 1 }, Parser)
end

local function contains(list, x)
  for _, y in ipairs(list) do
    if y == x then return true end
  end
  return false
end

--- Parses nodes until one of `stops` is reached; returns the nodes and the stop tag.
function Parser:nodes(stops)
  local out = {}
  local toks = self.toks
  while self.pos <= #toks do
    local tok = toks[self.pos]
    self.pos = self.pos + 1
    if tok.kind == "text" then
      out[#out + 1] = { kind = "text", text = tok.text }
    elseif tok.kind == "out" then
      out[#out + 1] = { kind = "output", expr = parse_expr(tok.text) }
    else
      local tag = tok.text
      local word = tag:match("%S+") or ""
      if contains(stops, word) then
        return out, tag
      end
      out[#out + 1] = self:block(tag, word)
    end
  end
  if #stops == 0 then
    return out, ""
  end
  local expected = {}
  for i, s in ipairs(stops) do expected[i] = '"' .. s .. '"' end
  fail("unexpected end of template, expected one of [" .. table.concat(expected, ", ") .. "]", "parse")
end

function Parser:block(tag, word)
  if word == "for" then
    local parts = words(tag)
    if #parts ~= 4 or parts[3] ~= "in" then
      fail("malformed for tag `" .. tag .. "`", "parse")
    end
    local body = self:nodes({ "endfor" })
    return { kind = "for", name = parts[2], iterable = parse_expr(parts[4]), body = body }
  elseif word == "if" then
    local branches = {}
    local cond = parse_cond(sub(tag, 3))
    while true do
      local body, stop = self:nodes({ "elif", "else", "endif" })
      branches[#branches + 1] = { cond, body }
      if sub(stop, 1, 4) == "elif" then
        cond = parse_cond(sub(stop, 5))
      elseif stop == "else" then
        return { kind = "if", branches = branches, otherwise = (self:nodes({ "endif" })) }
      else
        return { kind = "if", branches = branches, otherwise = {} }
      end
    end
  end
  fail("unknown tag `" .. tag .. "`", "parse")
end

local function compile(src)
  return (Parser.new(lex(src)):nodes({}))
end

-- ---------------------------------------------------------------------------
-- Rendering. A scope is a stack of frames (tables of names), searched from
-- the innermost one.

local function lookup(frames, path)
  local value = null
  local name = path[1]
  for i = #frames, 1, -1 do
    local v = frames[i][name]
    if v ~= nil then
      value = v
      break
    end
  end
  for i = 2, #path do
    local key = path[i]
    if json.is_object(value) then
      value = value[key]
    elseif json.is_array(value) and find(key, "^%d+$") then
      value = value[tonumber(key) + 1]
    else
      value = nil
    end
    if value == nil then value = null end
  end
  return value
end

--- null, false, 0, "", [] and {} are falsy, as in Sand's `truthy`.
local function truthy(v)
  local t = type(v)
  if t == "boolean" then
    return v
  elseif t == "number" then
    return v ~= 0
  elseif t == "string" then
    return v ~= ""
  elseif json.is_array(v) then
    return #v > 0
  elseif json.is_object(v) then
    return #json.keys(v) > 0
  end
  return false
end

--- Formats a float the way Sand's `float.to_str()` does: the shortest
--- round-tripping digits, in exponent form below 1e-4 and from 1e16 up.
local function fmt_float(v)
  if v ~= v then return "NaN" end
  if v == math.huge then return "inf" end
  if v == -math.huge then return "-inf" end
  local s
  for precision = 14, 16 do
    s = format("%." .. precision .. "e", v)
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

local function fmt_num(n)
  if n == math.floor(n) and math.abs(n) < 1e15 then
    return format("%d", n)
  end
  return fmt_float(n)
end

local function display(v)
  local t = type(v)
  if t == "string" then
    return v
  elseif t == "number" then
    return fmt_num(v)
  elseif t == "boolean" then
    return v and "true" or "false"
  elseif v == null then
    return ""
  end
  return json.encode(v)
end

local HTML_ESCAPES = { ["&"] = "&amp;", ["<"] = "&lt;", [">"] = "&gt;", ['"'] = "&quot;" }

local function escape_html(s)
  return (s:gsub('[&<>"]', HTML_ESCAPES))
end

local function capitalize(w)
  return sub(w, 1, 1):upper() .. sub(w, 2)
end

local EMPTY = json.array({})

local function apply_filter(v, f)
  local name = f.name
  if name == "upper" then
    return display(v):upper()
  elseif name == "lower" then
    return display(v):lower()
  elseif name == "title" then
    return (display(v):gsub("[^ ]+", capitalize))
  elseif name == "length" then
    if type(v) == "string" then
      return (utf8.len(v) or #v) + 0.0
    elseif json.is_array(v) then
      return #v + 0.0
    elseif json.is_object(v) then
      return #json.keys(v) + 0.0
    end
    return 0.0
  elseif name == "default" then
    if truthy(v) then
      return v
    elseif #f.args > 0 then
      return f.args[1]
    end
    return ""
  elseif name == "join" then
    local sep = type(f.args[1]) == "string" and f.args[1] or ""
    local items = json.is_array(v) and v or EMPTY
    local parts = {}
    for i, x in ipairs(items) do
      parts[i] = display(x)
    end
    return table.concat(parts, sep)
  elseif name == "escape" then
    return escape_html(display(v))
  elseif name == "trim" then
    return trim(display(v))
  end
  fail("unknown filter `" .. name .. "`", "invalid")
end

local function eval_expr(e, frames)
  local operand = e.value
  local v
  if operand.kind == "path" then
    v = lookup(frames, operand.parts)
  else
    v = operand.value
  end
  for _, f in ipairs(e.filters) do
    v = apply_filter(v, f)
  end
  return v
end

local function compare(op, a, b)
  local x, y = a, b
  if type(a) ~= "number" or type(b) ~= "number" then
    x, y = display(a), display(b)
  end
  if op == "==" then
    return x == y
  elseif op == "!=" then
    return x ~= y
  elseif op == "<" then
    return x < y
  elseif op == "<=" then
    return x <= y
  elseif op == ">" then
    return x > y
  end
  return x >= y
end

local function eval_cond(c, frames)
  if c.kind == "truthy" then
    return truthy(eval_expr(c.expr, frames))
  elseif c.kind == "not" then
    return not eval_cond(c.cond, frames)
  end
  return compare(c.op, eval_expr(c.left, frames), eval_expr(c.right, frames))
end

local LOOP_KEYS = { "index", "first", "last" }

local function render_nodes(nodes, frames, out)
  for _, node in ipairs(nodes) do
    local kind = node.kind
    if kind == "text" then
      out[#out + 1] = node.text
    elseif kind == "output" then
      out[#out + 1] = display(eval_expr(node.expr, frames))
    elseif kind == "for" then
      local items = eval_expr(node.iterable, frames)
      if not json.is_array(items) then
        items = EMPTY
      end
      local n = #items
      for i = 1, n do
        local loop_info = json.object({ index = i + 0.0, first = i == 1, last = i == n }, LOOP_KEYS)
        local frame = {}
        frame[node.name] = items[i]
        frame.loop = loop_info
        frames[#frames + 1] = frame
        render_nodes(node.body, frames, out)
        frames[#frames] = nil
      end
    else
      local done = false
      for _, branch in ipairs(node.branches) do
        if eval_cond(branch[1], frames) then
          render_nodes(branch[2], frames, out)
          done = true
          break
        end
      end
      if not done then
        render_nodes(node.otherwise, frames, out)
      end
    end
  end
end

local function render(template, data)
  local nodes = compile(template)
  local root = json.is_object(data) and data or {}
  local out = {}
  render_nodes(nodes, { root }, out)
  return table.concat(out)
end

-- Sand's error kinds for the errno values of failed file reads.
local IO_ERROR_KINDS = { [2] = "not_found", [13] = "permission_denied", [21] = "invalid" }

local function read_file(path)
  local f, err, code = io.open(path, "rb")
  if f then
    local text
    text, err, code = f:read("a")
    f:close()
    if text then
      return text
    end
    err = path .. ": " .. err
  end
  fail(format("%s (os error %d)", err, code), IO_ERROR_KINDS[code] or "io")
end

local function main()
  if #arg ~= 2 then
    io.stderr:write("usage: template.lua TEMPLATE DATA.json\n")
    os.exit(2)
  end
  local template = read_file(arg[1])
  local ok, data = pcall(json.decode, read_file(arg[2]))
  if not ok then
    fail(data, "parse")
  end
  io.write(render(template, data))
end

local ok, err = xpcall(main, function(e)
  return getmetatable(e) == Error and e or debug.traceback(e, 2)
end)
if not ok then
  io.stderr:write(getmetatable(err) == Error and "error: " or "", tostring(err), "\n")
  os.exit(1)
end
