-- An expense report built from two small "traits".
--
-- Lua port of examples/ledger.sand. Sand's `Monoid` and `Render` traits
-- (implemented for structs and for builtin types such as `int`, `str`, tuples
-- and lists) become `combine` and `render` functions that dispatch on the
-- value: numbers and strings directly, everything else through its
-- metatable's methods. Sand passes the identity `V.empty()` implicitly through
-- the trait dictionary; here it is passed explicitly.
--
-- Usage: lua5.5 ledger.lua [CATEGORY]

-- ---------------------------------------------------------------------------
-- Monoid: values with an identity and an associative way to combine them.

local function combine(a, b)
  if type(a) == "number" then
    return a + b
  end
  return a:combine(b)
end

--- Combines all values of a list (the identity if it is empty).
local function concat(xs, empty)
  local acc = empty
  for _, x in ipairs(xs) do
    acc = combine(acc, x)
  end
  return acc
end

-- ---------------------------------------------------------------------------
-- Render: values that can be shown in the report.

local function render(x)
  local t = type(x)
  if t == "string" then
    return x
  elseif t == "number" then
    return string.format("%d", x)
  end
  return x:render()
end

--- The rendering, padded to `width` characters.
local function cell(x, width)
  local s = render(x)
  return s .. string.rep(" ", width - #s)
end

--- An amount of money in cents.
local Money = {}
Money.__index = Money
Money.__lt = function(a, b) return a.cents < b.cents end
Money.__le = function(a, b) return a.cents <= b.cents end

function Money.new(cents)
  return setmetatable({ cents = cents }, Money)
end

function Money:combine(other)
  return Money.new(self.cents + other.cents)
end

function Money:render()
  return string.format("$%d.%02d", self.cents // 100, self.cents % 100)
end

--- The largest value combined so far, if any.
local Max = {}
Max.__index = Max

function Max.new(value)
  return setmetatable({ value = value }, Max)
end

function Max:combine(other)
  local a, b = self.value, other.value
  if a == nil then return Max.new(b) end
  if b == nil then return Max.new(a) end
  return Max.new(a >= b and a or b)
end

function Max:render()
  if self.value == nil then return "-" end
  return "max " .. render(self.value)
end

--- A pair of values; it is a monoid or renderable when both halves are.
local Pair = {}
Pair.__index = Pair

function Pair.new(first, second)
  return setmetatable({ first, second }, Pair)
end

function Pair:combine(other)
  return Pair.new(combine(self[1], other[1]), combine(self[2], other[2]))
end

function Pair:render()
  return cell(self[1], 12) .. render(self[2])
end

local List = {}
List.__index = List

function List:render()
  local parts = {}
  for i, x in ipairs(self) do
    parts[i] = render(x)
  end
  return table.concat(parts, ", ")
end

-- ---------------------------------------------------------------------------

-- Per expense: (count, (total, largest)).
local STATS_EMPTY = Pair.new(0, Pair.new(Money.new(0), Max.new(nil)))

local function stats(e)
  return Pair.new(1, Pair.new(e.amount, Max.new(e.amount)))
end

--- Groups `{ key, value }` pairs by key, combining each group.
local function summarize(pairs, empty)
  local out = {}
  for _, kv in ipairs(pairs) do
    local k = kv[1]
    out[k] = combine(out[k] or empty, kv[2])
  end
  return out
end

local function section(title, rows)
  io.write("== ", title, "\n")
  local keys = {}
  for k in pairs(rows) do
    keys[#keys + 1] = k
  end
  table.sort(keys)
  for _, k in ipairs(keys) do
    io.write("  ", cell(k, 10), render(rows[k]), "\n")
  end
end

local function trim(s)
  return s:match("^%s*(.-)%s*$")
end

local function parse_int(s)
  return s:find("^[+-]?%d+$") and math.tointeger(tonumber(s))
end

local function parse(line)
  local parts = {}
  for part in (line .. ","):gmatch("([^,]*),") do
    parts[#parts + 1] = trim(part)
  end
  if #parts ~= 4 then
    return nil
  end
  local month, category, what, amount = table.unpack(parts)
  local whole, frac = amount:match("^([^.]*)%.(.*)$")
  if not whole then
    whole, frac = amount, "0"
  end
  whole, frac = parse_int(whole), parse_int((frac .. "00"):sub(1, 2))
  if not whole or not frac then
    return nil
  end
  return { month = month, category = category, what = what, amount = Money.new(whole * 100 + frac) }
end

local DATA = [[
    2024-01, food, groceries, 54.20
    2024-01, rent, january, 950.00
    2024-01, food, cafe, 7.5
    2024-02, food, groceries, 61.05
    2024-02, travel, train, 38.40
    2024-02, rent, february, 950.00
    2024-02, food, restaurant, 42.00
    ]]

local expenses = {}
for line in DATA:gmatch("[^\n]+") do
  local e = parse(line)
  if e then
    expenses[#expenses + 1] = e
  end
end

local by_category, by_month, all_stats = {}, {}, {}
for i, e in ipairs(expenses) do
  by_category[i] = { e.category, stats(e) }
  by_month[i] = { e.month, e.amount }
  all_stats[i] = stats(e)
end
section("by category: count, total, largest", summarize(by_category, STATS_EMPTY))
section("by month: total", summarize(by_month, Money.new(0)))
io.write("== overall: ", render(concat(all_stats, STATS_EMPTY)), "\n")

-- An optional argument picks one category.
local wanted = arg[1]
if wanted then
  local items = setmetatable({}, List)
  for _, e in ipairs(expenses) do
    if e.category == wanted then
      items[#items + 1] = e.what
    end
  end
  io.write("== ", wanted, ": ", #items == 0 and "none" or render(items), "\n")
end
