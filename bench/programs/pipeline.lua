-- Closure-heavy event processing: an event bus of handler closures that mutate
-- upvalues, hand-written iterator closures and loops standing in for lazy
-- pipelines, and table.sort with comparators. Stresses closure calls and
-- upvalue access.

local EVENTS = 300000
local KINDS = 8
local USERS = 5000
local MOD = 1000000007

local Rng = {}
Rng.__index = Rng

function Rng.new(seed)
  return setmetatable({ state = seed }, Rng)
end

function Rng:next(bound)
  self.state = (self.state * 1103515245 + 12345) % 2147483648
  return self.state * bound // 2147483648
end

local EventBus = {}
EventBus.__index = EventBus

function EventBus.new()
  return setmetatable({ handlers = {} }, EventBus)
end

function EventBus:on(kind, handler)
  local list = self.handlers[kind]
  if not list then
    list = {}
    self.handlers[kind] = list
  end
  list[#list + 1] = handler
end

function EventBus:emit(event)
  local list = self.handlers[event.kind]
  if not list then return end
  for _, handler in ipairs(list) do
    handler(event)
  end
end

local function dispatch(events)
  local bus = EventBus.new()
  local counts = {}
  local purchases, revenue, refunded, largest, digest = 0, 0, 0, 0, 0
  local spend_by_user, spenders = {}, 0
  for kind = 0, KINDS - 1 do
    counts[kind + 1] = 0
    bus:on(kind, function() counts[kind + 1] = counts[kind + 1] + 1 end)
    bus:on(kind, function(e) digest = (digest * 31 + e.amount) % MOD end)
  end
  bus:on(0, function(e)
    purchases = purchases + 1
    revenue = revenue + e.amount
  end)
  bus:on(1, function(e) refunded = refunded + e.amount end)
  for _, kind in ipairs({ 0, 2 }) do
    bus:on(kind, function(e) largest = math.max(largest, e.amount) end)
  end
  bus:on(2, function(e)
    local spent = spend_by_user[e.user]
    if not spent then spenders = spenders + 1 end
    spend_by_user[e.user] = (spent or 0) + e.amount
  end)
  for _, e in ipairs(events) do
    bus:emit(e)
  end
  print("bus: counts " .. table.concat(counts, " "))
  print(string.format("bus: purchases %d, revenue %d, average %.2f", purchases, revenue, revenue / purchases))
  print(string.format("bus: refunded %d, largest %d, spenders %d, digest %d", refunded, largest, spenders, digest))
end

-- Runs of consecutive events of the same kind, yielding kind, length.
local function bursts(events)
  local i = 1
  return function()
    if i > #events then return nil end
    local kind = events[i].kind
    local start = i
    while i <= #events and events[i].kind == kind do
      i = i + 1
    end
    return kind, i - start
  end
end

local function lazy_stats(events)
  local total = 0
  for _, e in ipairs(events) do total = total + e.amount end
  local purchase_total = 0
  for _, e in ipairs(events) do
    if e.kind == 0 then purchase_total = purchase_total + e.amount end
  end
  local sampled = 0
  for i = 1, #events, 7 do sampled = sampled + (i - 1) % 100 * events[i].amount end
  local budget, within_budget = total // 2, 0
  for _, e in ipairs(events) do
    budget = budget - e.amount
    if budget <= 0 then break end
    within_budget = within_budget + 1
  end
  local rising = 0
  for i = 2, #events do
    if events[i - 1].amount < events[i].amount then rising = rising + 1 end
  end
  local digest = 0
  for _, e in ipairs(events) do
    if e.kind == 4 then digest = (digest * 31 + e.amount) % MOD end
  end
  local heavy, heavy_users = {}, 0
  for _, e in ipairs(events) do
    if e.amount > 990 and not heavy[e.user] then
      heavy[e.user] = true
      heavy_users = heavy_users + 1
    end
  end
  local whale_index, whale
  for i, e in ipairs(events) do
    if e.kind == 7 and e.amount == 1000 then
      whale_index, whale = i - 1, e
      break
    end
  end
  local long_bursts = 0
  for _, length in bursts(events) do
    if length >= 4 then long_bursts = long_bursts + 1 end
  end
  local burst_kind, burst_length = nil, -1
  for kind, length in bursts(events) do
    if length > burst_length then burst_kind, burst_length = kind, length end
  end
  print(string.format("lazy: total %d, purchases %d, sampled %d, within budget %d", total, purchase_total, sampled,
    within_budget))
  print(string.format("lazy: rising %d, digest %d, heavy users %d", rising, digest, heavy_users))
  if whale then
    print(string.format("lazy: first whale at %d, user %d", whale_index, whale.user))
  else
    print("lazy: no whale")
  end
  print(string.format("lazy: long bursts %d, longest kind %d x %d", long_bursts, burst_kind, burst_length))
end

local function eager_stats(events)
  local big, big_total = {}, 0
  for _, e in ipairs(events) do
    if e.amount > 900 then big[#big + 1] = e end
  end
  for _, e in ipairs(big) do big_total = big_total + e.amount end
  local by_user = {}
  for _, e in ipairs(events) do
    local list = by_user[e.user]
    if not list then
      list = {}
      by_user[e.user] = list
    end
    list[#list + 1] = e
  end
  local totals = {}
  for user, list in pairs(by_user) do
    local sum = 0
    for _, e in ipairs(list) do sum = sum + e.amount end
    totals[#totals + 1] = { user, sum }
  end
  table.sort(totals, function(a, b)
    if a[2] ~= b[2] then return a[2] > b[2] end
    return a[1] < b[1]
  end)
  local top = {}
  for i = 1, math.min(3, #totals) do top[i] = totals[i][1] .. "=" .. totals[i][2] end
  local ordered = table.move(events, 1, #events, 1, {})
  table.sort(ordered, function(a, b)
    if a.user ~= b.user then return a.user < b.user end
    return a.amount > b.amount
  end)
  local order_digest = 0
  for _, e in ipairs(ordered) do order_digest = (order_digest * 31 + e.amount) % MOD end
  local any_whale, all_positive = false, true
  for _, e in ipairs(events) do
    if e.kind == 7 and e.amount == 1000 then
      any_whale = true
      break
    end
  end
  for _, e in ipairs(events) do
    if e.amount <= 0 then
      all_positive = false
      break
    end
  end
  local peak, peak_key = nil, nil
  for _, e in ipairs(events) do
    local key = e.amount * KINDS + e.kind
    if not peak or key > peak_key then peak, peak_key = e, key end
  end
  local refunds = 0
  for _, e in ipairs(events) do
    if e.kind == 1 then refunds = refunds + 1 end
  end
  print(string.format("eager: big %d totalling %d, refunds %d", #big, big_total, refunds))
  print("eager: top spenders " .. table.concat(top, " "))
  print(string.format("eager: order digest %d, any whale %s, all positive %s", order_digest, any_whale, all_positive))
  print(string.format("eager: peak user %d, kind %d, amount %d", peak.user, peak.kind, peak.amount))
end

local rng = Rng.new(2024)
local events = {}
for i = 1, EVENTS do
  local kind = rng:next(KINDS)
  local user = rng:next(USERS)
  events[i] = { kind = kind, user = user, amount = 1 + rng:next(1000) }
end
dispatch(events)
lazy_stats(events)
eager_stats(events)
