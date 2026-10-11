-- Sales report from CSV text: generate rows in memory, split and parse them, aggregate
-- per (region, product) and per month into tables of accumulators, then print padded tables.
-- Stresses string building and pattern matching, number parsing, table lookups with string
-- keys, small table mutation and string formatting.

local ROWS = 200000
local REGIONS = { "north", "south", "east", "west", "central", "overseas" }
local KINDS = { "widget", "gadget", "gizmo", "sprocket", "doohickey" }
local HEADER = "date,region,product,quantity,unit_price"

local format = string.format

local Rng = {}
Rng.__index = Rng

function Rng.new(seed)
  return setmetatable({ state = seed }, Rng)
end

function Rng:next(n)
  self.state = (self.state * 1103515245 + 12345) % 2147483648
  return self.state // 65536 % n
end

local function generate(rows)
  local rng = Rng.new(7)
  local lines = { HEADER }
  for i = 1, rows do
    local month = rng:next(12) + 1
    local day = rng:next(28) + 1
    local region = REGIONS[rng:next(#REGIONS) + 1]
    local kind = KINDS[rng:next(#KINDS) + 1]
    local product = format("%s-%d", kind, rng:next(10) + 1)
    local quantity = rng:next(20) + 1
    local cents = 99 + rng:next(9900)
    lines[i + 1] = format("2024-%02d-%02d,%s,%s,%d,%d.%02d",
      month, day, region, product, quantity, cents // 100, cents % 100)
  end
  return table.concat(lines, "\n") .. "\n"
end

local function parse_sale(line)
  local date, region, product, qty, price = line:match("^([^,]*),([^,]*),([^,]*),([^,]*),([^,]*)$")
  if not date then
    error(format("expected 5 fields: `%s`", line))
  end
  local quantity = math.tointeger(tonumber(qty))
  if not quantity then
    error(format("bad quantity `%s`", qty))
  end
  local unit_price = tonumber(price)
  if not unit_price then
    error(format("bad price `%s`", price))
  end
  return {
    month = date:sub(1, 7),
    region = region,
    product = product,
    quantity = quantity,
    price = math.floor(unit_price * 100 + 0.5),
  }
end

-- Totals over a group of sales; prices and revenue are in cents.
local Acc = {}
Acc.__index = Acc

function Acc.new()
  return setmetatable({ count = 0, quantity = 0, revenue = 0, min_price = math.maxinteger, max_price = 0 }, Acc)
end

function Acc:add(quantity, price)
  self.count = self.count + 1
  self.quantity = self.quantity + quantity
  self.revenue = self.revenue + quantity * price
  self.min_price = math.min(self.min_price, price)
  self.max_price = math.max(self.max_price, price)
end

function Acc:merge(other)
  self.count = self.count + other.count
  self.quantity = self.quantity + other.quantity
  self.revenue = self.revenue + other.revenue
  self.min_price = math.min(self.min_price, other.min_price)
  self.max_price = math.max(self.max_price, other.max_price)
end

local function get_acc(t, key)
  local acc = t[key]
  if not acc then
    acc = Acc.new()
    t[key] = acc
  end
  return acc
end

local function money(cents)
  local grouped = tostring(cents // 100):reverse():gsub("(%d%d%d)", "%1,"):reverse()
  return format("$%s.%02d", grouped:gsub("^,", ""), cents % 100)
end

local function percent(part, whole)
  local basis_points = part * 10000 // whole
  return format("%d.%02d%%", basis_points // 100, basis_points % 100)
end

local function sorted_keys(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  table.sort(keys)
  return keys
end

local text = generate(ROWS)

local by_pair, by_month = {}, {}
local first = true
for line in text:gmatch("([^\n]*)\n") do
  if first then
    assert(line == HEADER, "unexpected header")
    first = false
  else
    local sale = parse_sale(line)
    local products = by_pair[sale.region]
    if not products then
      products = {}
      by_pair[sale.region] = products
    end
    get_acc(products, sale.product):add(sale.quantity, sale.price)
    get_acc(by_month, sale.month):add(sale.quantity, sale.price)
  end
end

local by_product, by_region = {}, {}
local total = Acc.new()
for region, products in pairs(by_pair) do
  for product, acc in pairs(products) do
    get_acc(by_product, product):merge(acc)
    get_acc(by_region, region):merge(acc)
    total:merge(acc)
  end
end

print(format("== Sales report: %d orders, %d bytes of CSV", total.count, #text))
print(format("units sold %d, revenue %s", total.quantity, money(total.revenue)))
print("")

print("== Top 15 products by revenue")
print(format(" #  %-14s%8s%8s%16s%9s%9s%9s", "product", "orders", "units", "revenue", "avg", "min", "max"))
local ranked = {}
for product, acc in pairs(by_product) do
  ranked[#ranked + 1] = { product = product, acc = acc }
end
table.sort(ranked, function(a, b)
  if a.acc.revenue ~= b.acc.revenue then
    return a.acc.revenue > b.acc.revenue
  end
  return a.product < b.product
end)
for i = 1, math.min(15, #ranked) do
  local product, acc = ranked[i].product, ranked[i].acc
  print(format("%2d  %-14s%8d%8d%16s%9s%9s%9s", i, product, acc.count, acc.quantity,
    money(acc.revenue), money(acc.revenue // acc.quantity), money(acc.min_price), money(acc.max_price)))
end
print("")

print("== Revenue by region")
print(format("%-10s%8s%8s%16s%9s", "region", "orders", "units", "revenue", "share"))
for _, region in ipairs(sorted_keys(by_region)) do
  local acc = by_region[region]
  print(format("%-10s%8d%8d%16s%9s", region, acc.count, acc.quantity,
    money(acc.revenue), percent(acc.revenue, total.revenue)))
end
print("")

print("== Revenue by month")
local best_month = 1
for _, acc in pairs(by_month) do
  best_month = math.max(best_month, acc.revenue)
end
print(format("%-9s%8s%8s%16s%11s  chart", "month", "orders", "units", "revenue", "avg order"))
for _, month in ipairs(sorted_keys(by_month)) do
  local acc = by_month[month]
  print(format("%-9s%8d%8d%16s%11s  %s", month, acc.count, acc.quantity, money(acc.revenue),
    money(acc.revenue // acc.count), ("#"):rep(acc.revenue * 30 // best_month)))
end
