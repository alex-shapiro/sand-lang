-- A small JSON (RFC 8259) decoder and encoder in pure Lua, for the ports in
-- this directory (in the spirit of rxi/json.lua).
--
-- Values map to Lua as follows:
--
--   null           json.null (a sentinel, so arrays keep their length)
--   true / false   booleans
--   numbers        floats (always, like Sand's `Json.Num(float)`)
--   strings        strings (UTF-8)
--   arrays         tables marked with json.array (elements 1..n)
--   objects        tables marked with json.object; keys keep their order
--
-- Usage:
--
--   local json = require("json")
--   local value = json.decode(text)   -- raises an error string on bad input
--   local text = json.encode(value)   -- compact, like Sand's `json.encode`

local json = {}

local byte, char, find, format, sub = string.byte, string.char, string.find, string.format, string.sub
local concat = table.concat

--- The JSON `null` value.
json.null = setmetatable({}, { __name = "json.null", __tostring = function() return "null" end })

local Array = { __name = "json.array" }
local Object = { __name = "json.object" }

-- Object key order, kept outside the objects so that `pairs` sees only fields.
local key_order = setmetatable({}, { __mode = "k" })

--- Marks the list `t` as a JSON array and returns it.
function json.array(t)
  return setmetatable(t or {}, Array)
end

--- Marks `t` as a JSON object and returns it. `keys` lists its keys in
--- order; without it, the object encodes with its keys sorted.
function json.object(t, keys)
  t = setmetatable(t or {}, Object)
  key_order[t] = keys
  return t
end

function json.is_array(v)
  return getmetatable(v) == Array
end

function json.is_object(v)
  return getmetatable(v) == Object
end

--- The keys of an object, in order.
function json.keys(obj)
  local keys = key_order[obj]
  if not keys then
    keys = {}
    for k in pairs(obj) do
      keys[#keys + 1] = k
    end
    table.sort(keys)
  end
  return keys
end

-- ---------------------------------------------------------------------------
-- Decoding

--- Raises "WHAT at line L, column C", counting columns in characters.
local function decode_error(text, pos, what)
  local line, line_start = 1, 1
  for nl in text:sub(1, pos - 1):gmatch("()\n") do
    line, line_start = line + 1, nl + 1
  end
  local col = 1 + (utf8.len(text, line_start, pos - 1) or pos - line_start)
  error(format("%s at line %d, column %d", what, line, col), 0)
end

local CHAR_ESCAPES = { ["\t"] = "\\t", ["\r"] = "\\r", ["\n"] = "\\n", ['"'] = '\\"', ["'"] = "\\'", ["\\"] = "\\\\" }

local function unexpected(text, pos)
  if pos > #text then
    decode_error(text, pos, "unexpected end of input")
  end
  local c = text:match("^[\0-\127\194-\244][\128-\191]*", pos) or sub(text, pos, pos)
  c = c:gsub("^[%c'\"\\]$", function(ch) return CHAR_ESCAPES[ch] or format("\\u{%x}", byte(ch)) end)
  decode_error(text, pos, "unexpected character `" .. c .. "`")
end

local WHITESPACE = { [32] = true, [9] = true, [10] = true, [13] = true }

local function skip_ws(text, pos)
  while WHITESPACE[byte(text, pos)] do
    pos = pos + 1
  end
  return pos
end

local ESCAPES = { ['"'] = '"', ["\\"] = "\\", ["/"] = "/", b = "\b", f = "\f", n = "\n", r = "\r", t = "\t" }

--- The four hex digits at `pos` of the escape that starts at `esc`.
local function hex4(text, pos, esc)
  local hex = text:match("^%x%x%x%x", pos)
  if not hex then
    decode_error(text, esc, "invalid unicode escape")
  end
  return tonumber(hex, 16)
end

--- Decodes the string whose opening quote is at `pos`; returns it and the
--- position after the closing quote.
local function decode_string(text, pos)
  local start = pos + 1
  local stop = find(text, '["\\\0-\31]', start)
  if stop and byte(text, stop) == 34 then
    return sub(text, start, stop - 1), stop + 1
  end
  local parts = {}
  pos = start
  while true do
    if not stop then
      unexpected(text, #text + 1)
    end
    parts[#parts + 1] = sub(text, pos, stop - 1)
    local c = byte(text, stop)
    if c == 34 then
      return concat(parts), stop + 1
    elseif c == 92 then
      local e = sub(text, stop + 1, stop + 1)
      if e == "u" then
        local code = hex4(text, stop + 2, stop)
        pos = stop + 6
        if code >= 0xD800 and code < 0xDC00 then
          if sub(text, pos, pos + 1) ~= "\\u" then
            decode_error(text, stop, "invalid unicode escape")
          end
          local low = hex4(text, pos + 2, stop)
          if low < 0xDC00 or low >= 0xE000 then
            decode_error(text, stop, "invalid unicode escape")
          end
          code = 0x10000 + (code - 0xD800) * 0x400 + (low - 0xDC00)
          pos = pos + 6
        elseif code >= 0xDC00 and code < 0xE000 then
          decode_error(text, stop, "invalid unicode escape")
        end
        parts[#parts + 1] = utf8.char(code)
      else
        local unescaped = ESCAPES[e]
        if not unescaped then
          decode_error(text, stop, "invalid escape")
        end
        parts[#parts + 1] = unescaped
        pos = stop + 2
      end
    else
      decode_error(text, stop, "control character in string")
    end
    stop = find(text, '["\\\0-\31]', pos)
  end
end

--- Returns the position after the run of digits at `pos`; raises an error
--- if there is none.
local function digits(text, pos)
  local _, stop = find(text, "^%d+", pos)
  if not stop then
    unexpected(text, pos)
  end
  return stop + 1
end

local function decode_number(text, pos)
  local stop = pos
  if byte(text, stop) == 45 then -- -
    stop = stop + 1
  end
  if byte(text, stop) == 48 then -- 0
    stop = stop + 1
  else
    stop = digits(text, stop)
  end
  if byte(text, stop) == 46 then -- .
    stop = digits(text, stop + 1)
  end
  local e = byte(text, stop)
  if e == 101 or e == 69 then -- e E
    stop = stop + 1
    local sign = byte(text, stop)
    if sign == 43 or sign == 45 then -- + -
      stop = stop + 1
    end
    stop = digits(text, stop)
  end
  local literal = sub(text, pos, stop - 1)
  local n = tonumber(literal)
  if math.type(n) == "integer" then
    -- Converting through the text keeps the sign of `-0`.
    n = tonumber(literal .. ".0")
  end
  return n, stop
end

local MAX_DEPTH = 512

local decode_value

local function decode_array(text, pos, depth)
  local items = setmetatable({}, Array)
  local n = 0
  pos = skip_ws(text, pos + 1)
  if byte(text, pos) == 93 then
    return items, pos + 1
  end
  while true do
    local value
    value, pos = decode_value(text, pos, depth)
    n = n + 1
    items[n] = value
    pos = skip_ws(text, pos)
    local c = byte(text, pos)
    if c == 93 then
      return items, pos + 1
    elseif c ~= 44 then
      unexpected(text, pos)
    end
    pos = skip_ws(text, pos + 1)
  end
end

local function decode_object(text, pos, depth)
  local obj = setmetatable({}, Object)
  local keys = {}
  key_order[obj] = keys
  pos = skip_ws(text, pos + 1)
  if byte(text, pos) == 125 then
    return obj, pos + 1
  end
  while true do
    if byte(text, pos) ~= 34 then
      unexpected(text, pos)
    end
    local key
    key, pos = decode_string(text, pos)
    pos = skip_ws(text, pos)
    if byte(text, pos) ~= 58 then
      unexpected(text, pos)
    end
    local value
    value, pos = decode_value(text, skip_ws(text, pos + 1), depth)
    if obj[key] == nil then
      keys[#keys + 1] = key
    end
    obj[key] = value
    pos = skip_ws(text, pos)
    local c = byte(text, pos)
    if c == 125 then
      return obj, pos + 1
    elseif c ~= 44 then
      unexpected(text, pos)
    end
    pos = skip_ws(text, pos + 1)
  end
end

local LITERALS = { [116] = { "true", true }, [102] = { "false", false }, [110] = { "null", json.null } }

--- Decodes the value at `pos` (after any whitespace); returns it and the
--- position after it.
function decode_value(text, pos, depth)
  local c = byte(text, pos)
  if c == 34 then -- "
    return decode_string(text, pos)
  elseif c == 123 or c == 91 then -- { [
    if depth >= MAX_DEPTH then
      decode_error(text, pos, "nesting too deep")
    end
    if c == 123 then
      return decode_object(text, pos, depth + 1)
    end
    return decode_array(text, pos, depth + 1)
  elseif c == 45 or (c and c >= 48 and c <= 57) then -- - 0-9
    return decode_number(text, pos)
  end
  local literal = LITERALS[c]
  if literal then
    local word = literal[1]
    if sub(text, pos, pos + #word - 1) == word then
      return literal[2], pos + #word
    end
    -- Like Sand, report the first character after the word's initial letter(s).
    while byte(text, pos) == c do
      pos = pos + 1
    end
  end
  unexpected(text, pos)
end

--- Parses a JSON document. Raises an error message on invalid input.
function json.decode(text)
  local value, pos = decode_value(text, skip_ws(text, 1), 0)
  pos = skip_ws(text, pos)
  if pos <= #text then
    unexpected(text, pos)
  end
  return value
end

-- ---------------------------------------------------------------------------
-- Encoding

--- The shortest round-tripping decimal form of a finite float, in exponent
--- form below 1e-4 and from 1e16 up (`1e20`, `1.5e-7`).
local function format_float(v)
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

local function encode_number(n)
  if math.type(n) == "integer" then
    return format("%d", n)
  elseif n ~= n or n == math.huge or n == -math.huge then
    return "null"
  elseif n == math.floor(n) and math.abs(n) <= 2 ^ 53 then
    return format("%d", n)
  end
  return format_float(n)
end

local STRING_ESCAPES = { ['"'] = '\\"', ["\\"] = "\\\\", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t", ["\b"] = "\\b", ["\f"] = "\\f" }

local function encode_string(s)
  return '"' .. s:gsub('[\0-\31"\\]', function(c)
    return STRING_ESCAPES[c] or format("\\u%04x", byte(c))
  end) .. '"'
end

local encode_value

local function encode_table(t, out)
  local mt = getmetatable(t)
  if mt == Array or (mt ~= Object and (next(t) == nil or t[1] ~= nil)) then
    out[#out + 1] = "["
    for i, v in ipairs(t) do
      if i > 1 then out[#out + 1] = "," end
      encode_value(v, out)
    end
    out[#out + 1] = "]"
  else
    out[#out + 1] = "{"
    for i, k in ipairs(json.keys(t)) do
      if i > 1 then out[#out + 1] = "," end
      out[#out + 1] = encode_string(k)
      out[#out + 1] = ":"
      encode_value(t[k], out)
    end
    out[#out + 1] = "}"
  end
end

function encode_value(v, out)
  local t = type(v)
  if v == nil or v == json.null then
    out[#out + 1] = "null"
  elseif t == "boolean" then
    out[#out + 1] = v and "true" or "false"
  elseif t == "number" then
    out[#out + 1] = encode_number(v)
  elseif t == "string" then
    out[#out + 1] = encode_string(v)
  elseif t == "table" then
    encode_table(v, out)
  else
    error("cannot encode a " .. t .. " as JSON", 2)
  end
end

--- Encodes a value as compact JSON. Integral floats up to 2^53 print without
--- a fraction; NaN and infinities print as `null`.
function json.encode(value)
  local out = {}
  encode_value(value, out)
  return concat(out)
end

return json
