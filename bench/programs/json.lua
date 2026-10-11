-- Hand-written JSON parser and serializer: generate a document, then repeatedly parse it
-- with a character-by-character recursive-descent parser, walk the tree, encode it back
-- and check the round trip. Stresses string scanning, slicing and joining, table trees,
-- error handling, ordered object iteration and string building.

local RECORDS = 2000
local ITERATIONS = 3

local byte, sub, format, concat = string.byte, string.sub, string.format, table.concat

local NULL = setmetatable({}, { __tostring = function() return "null" end })
local Array = {}
local Object = {}

local function new_object()
  return setmetatable({ keys = {}, values = {} }, Object)
end

local Rng = {}
Rng.__index = Rng

function Rng.new(seed)
  return setmetatable({ state = seed }, Rng)
end

function Rng:next(n)
  self.state = (self.state * 1103515245 + 12345) % 2147483648
  return self.state % n
end

local FIRST_NAMES = { "Ada", "Zoë", "José", "Renée", "Linus", "Grace", "Bjørn", "Ming" }
local CITIES = { "Montréal", "São Paulo", "Zürich", "Tokyo", "Berlin", "Lagos", "Kraków" }
local TAGS = { "alpha", "beta", "gamma", "delta", "epsilon", "zeta" }

local function decimal(rng, max_cents)
  local cents = rng:next(max_cents)
  return format("%d.%02d", cents // 100, cents % 100)
end

local function record(rng, id)
  local name = FIRST_NAMES[rng:next(#FIRST_NAMES) + 1]
  local city = CITIES[rng:next(#CITIES) + 1]
  local active = rng:next(2) == 0 and "true" or "false"
  local score = decimal(rng, 100000)
  local balance = decimal(rng, 5000000)
  local rating = decimal(rng, 1000)
  local manager = rng:next(4) == 0 and "null" or tostring(rng:next(id + 1))
  local tags = {}
  for i = 1, rng:next(4) do
    tags[i] = '"' .. TAGS[rng:next(#TAGS) + 1] .. '"'
  end
  local lat = decimal(rng, 18000)
  local lon = decimal(rng, 36000)
  local history = {}
  for i = 1, rng:next(5) do
    local ts = 1700000000 + rng:next(10000000)
    local amount = decimal(rng, 100000)
    local ok = rng:next(3) == 0 and "false" or "true"
    history[i] = format('{"ts": %d, "amount": -%s, "ok": %s}', ts, amount, ok)
  end
  local street = rng:next(999) + 1
  return format('  {"id": %d, "name": "%s \\"#%d\\"", ', id, name, id)
    .. format('"email": "%s.%d@example.com", "active": %s, ', name:lower(), id, active)
    .. format('"score": %s, "balance": -%s, "rating": %se1, ', score, balance, rating)
    .. format('"tags": [%s], ', concat(tags, ", "))
    .. format('"address": {"city": "%s", "street": "%d Rue de l\\u00c9glise", ', city, street)
    .. format('"geo": [%s, -%s]}, ', lat, lon)
    .. '"notes": "line one\\nline two\\ttabbed \\\\ path\\/to caf\\u00e9 \\u2603 é", '
    .. format('"manager": %s, "history": [%s]}', manager, concat(history, ", "))
end

local function generate(count)
  local rng = Rng.new(42)
  local records = {}
  for i = 0, count - 1 do
    records[#records + 1] = record(rng, i)
  end
  return "[\n" .. concat(records, ",\n") .. "\n]\n"
end

local B_QUOTE, B_BACKSLASH = byte('"'), byte("\\")
local B_LBRACE, B_RBRACE, B_LBRACKET, B_RBRACKET = byte("{"), byte("}"), byte("["), byte("]")
local B_COMMA, B_COLON, B_MINUS = byte(","), byte(":"), byte("-")

local WHITESPACE = { [byte(" ")] = true, [byte("\t")] = true, [byte("\n")] = true, [byte("\r")] = true }
local NUMBER_CHARS = {}
for c in ("0123456789+-.eE"):gmatch(".") do
  NUMBER_CHARS[byte(c)] = true
end
local ESCAPES = { ['"'] = '"', ["\\"] = "\\", ["/"] = "/", b = "\b", f = "\f", n = "\n", r = "\r", t = "\t" }

local Parser = {}
Parser.__index = Parser

function Parser:skip_ws()
  local text, pos = self.text, self.pos
  while WHITESPACE[byte(text, pos)] do
    pos = pos + 1
  end
  self.pos = pos
end

function Parser:fail(message)
  error(format("%s at %d", message, self.pos), 0)
end

function Parser:expect(b)
  self:skip_ws()
  if byte(self.text, self.pos) ~= b then
    self:fail(format("expected `%s`", string.char(b)))
  end
  self.pos = self.pos + 1
end

function Parser:literal(word, value)
  local finish = self.pos + #word
  if sub(self.text, self.pos, finish - 1) ~= word then
    self:fail("invalid literal")
  end
  self.pos = finish
  return value
end

function Parser:number()
  local text, start = self.text, self.pos
  local pos = start
  while NUMBER_CHARS[byte(text, pos)] do
    pos = pos + 1
  end
  self.pos = pos
  local n = tonumber(sub(text, start, pos - 1))
  if not n then
    self:fail("invalid number")
  end
  return n
end

function Parser:escape()
  local text, pos = self.text, self.pos
  local c = sub(text, pos, pos)
  if c == "u" then
    local hex = sub(text, pos + 1, pos + 4)
    local n = #hex == 4 and tonumber(hex, 16)
    if not n then
      self:fail("bad \\u escape")
    end
    self.pos = pos + 5
    return utf8.char(n)
  end
  local e = ESCAPES[c]
  if not e then
    self:fail("bad escape")
  end
  self.pos = pos + 1
  return e
end

function Parser:string()
  local text = self.text
  local parts = {}
  local pos = self.pos + 1
  local start = pos
  while true do
    local b = byte(text, pos)
    if b == B_QUOTE then
      parts[#parts + 1] = sub(text, start, pos - 1)
      self.pos = pos + 1
      return concat(parts)
    elseif b == B_BACKSLASH then
      parts[#parts + 1] = sub(text, start, pos - 1)
      self.pos = pos + 1
      parts[#parts + 1] = self:escape()
      pos = self.pos
      start = pos
    elseif b == nil then
      self:fail("unterminated string")
    else
      pos = pos + 1
    end
  end
end

function Parser:array()
  self.pos = self.pos + 1
  local items = setmetatable({}, Array)
  self:skip_ws()
  if byte(self.text, self.pos) == B_RBRACKET then
    self.pos = self.pos + 1
    return items
  end
  local n = 0
  while true do
    n = n + 1
    items[n] = self:value()
    self:skip_ws()
    local b = byte(self.text, self.pos)
    self.pos = self.pos + 1
    if b == B_RBRACKET then
      return items
    elseif b ~= B_COMMA then
      self:fail("expected `,` or `]`")
    end
  end
end

function Parser:object()
  self.pos = self.pos + 1
  local obj = new_object()
  local keys, values = obj.keys, obj.values
  self:skip_ws()
  if byte(self.text, self.pos) == B_RBRACE then
    self.pos = self.pos + 1
    return obj
  end
  while true do
    self:skip_ws()
    if byte(self.text, self.pos) ~= B_QUOTE then
      self:fail("expected a key")
    end
    local key = self:string()
    self:expect(B_COLON)
    if values[key] == nil then
      keys[#keys + 1] = key
    end
    values[key] = self:value()
    self:skip_ws()
    local b = byte(self.text, self.pos)
    self.pos = self.pos + 1
    if b == B_RBRACE then
      return obj
    elseif b ~= B_COMMA then
      self:fail("expected `,` or `}`")
    end
  end
end

function Parser:value()
  self:skip_ws()
  local b = byte(self.text, self.pos)
  if b == B_LBRACE then
    return self:object()
  elseif b == B_LBRACKET then
    return self:array()
  elseif b == B_QUOTE then
    return self:string()
  elseif b == byte("t") then
    return self:literal("true", true)
  elseif b == byte("f") then
    return self:literal("false", false)
  elseif b == byte("n") then
    return self:literal("null", NULL)
  elseif b == B_MINUS or (b and b >= 48 and b <= 57) then
    return self:number()
  elseif b == nil then
    self:fail("unexpected end of input")
  end
  self:fail(format("unexpected `%s`", string.char(b)))
end

local function parse(text)
  local p = setmetatable({ text = text, pos = 1 }, Parser)
  local value = p:value()
  p:skip_ws()
  if p.pos <= #text then
    p:fail("trailing characters")
  end
  return value
end

local Stats = {}
Stats.__index = Stats

function Stats.new()
  return setmetatable({
    nulls = 0, bools = 0, nums = 0, strs = 0, arrs = 0, objs = 0,
    num_sum = 0.0, str_chars = 0, max_depth = 0,
  }, Stats)
end

function Stats:walk(value, depth)
  if depth > self.max_depth then
    self.max_depth = depth
  end
  local t = type(value)
  if t == "boolean" then
    self.bools = self.bools + 1
  elseif t == "number" then
    self.nums = self.nums + 1
    self.num_sum = self.num_sum + value
  elseif t == "string" then
    self.strs = self.strs + 1
    self.str_chars = self.str_chars + utf8.len(value)
  elseif value == NULL then
    self.nulls = self.nulls + 1
  elseif getmetatable(value) == Array then
    self.arrs = self.arrs + 1
    for _, item in ipairs(value) do
      self:walk(item, depth + 1)
    end
  else
    self.objs = self.objs + 1
    local values = value.values
    for _, key in ipairs(value.keys) do
      self.str_chars = self.str_chars + utf8.len(key)
      self:walk(values[key], depth + 1)
    end
  end
end

function Stats.of(value)
  local stats = Stats.new()
  stats:walk(value, 1)
  return stats
end

function Stats:describe()
  return format("nulls=%d bools=%d nums=%d strs=%d arrs=%d objs=%d sum=%.2f chars=%d depth=%d",
    self.nulls, self.bools, self.nums, self.strs, self.arrs, self.objs,
    self.num_sum, self.str_chars, self.max_depth)
end

local ENCODE_ESCAPES = { ['"'] = '\\"', ["\\"] = "\\\\", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }

local function escape_char(c)
  return ENCODE_ESCAPES[c] or format("\\u%04x", byte(c))
end

local function encode_str(s)
  return '"' .. s:gsub('[%c"\\]', escape_char) .. '"'
end

local function encode_num(n)
  if n == math.floor(n) and math.abs(n) < 1e15 then
    return format("%d", n)
  end
  return format("%.2f", n)
end

local function encode(value)
  local t = type(value)
  if t == "boolean" then
    return value and "true" or "false"
  elseif t == "number" then
    return encode_num(value)
  elseif t == "string" then
    return encode_str(value)
  elseif value == NULL then
    return "null"
  elseif getmetatable(value) == Array then
    local parts = {}
    for i, item in ipairs(value) do
      parts[i] = encode(item)
    end
    return "[" .. concat(parts, ",") .. "]"
  else
    local parts, values = {}, value.values
    for i, key in ipairs(value.keys) do
      parts[i] = encode_str(key) .. ":" .. encode(values[key])
    end
    return "{" .. concat(parts, ",") .. "}"
  end
end

local text = generate(RECORDS)
print(format("document: %d chars", utf8.len(text)))

local total_chars = 0
for i = 0, ITERATIONS - 1 do
  local tree = parse(text)
  local stats = Stats.of(tree)
  local out = encode(tree)
  local again = parse(out)
  assert(encode(again) == out, "round trip changed the text")
  assert(Stats.of(again):describe() == stats:describe(), "round trip changed the stats")
  total_chars = total_chars + utf8.len(out)
  if i == 0 then
    print(stats:describe())
    print(encode(tree[1]))
  end
end
print(format("encoded %d times, %d chars", ITERATIONS, total_chars))
