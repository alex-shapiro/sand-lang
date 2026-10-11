-- Collatz chain lengths below a limit, memoized in an int-keyed table, with a
-- histogram of the lengths and the longest chains. Stresses tables keyed by
-- dense small ints (the array part), recursion and integer arithmetic.

local LIMIT = 1500000
local CACHE_BELOW = 3000000

local function chain_length(cache, n)
  local l = cache[n]
  if l then return l end
  local next = (n % 2 == 0) and n // 2 or 3 * n + 1
  l = chain_length(cache, next) + 1
  if n < CACHE_BELOW then
    cache[n] = l
  end
  return l
end

local cache = { [1] = 1 }
local histogram = {}
local best_n, best_l = 0, 0
for n = 1, LIMIT - 1 do
  local l = chain_length(cache, n)
  histogram[l] = (histogram[l] or 0) + 1
  if l > best_l then
    best_n, best_l = n, l
  end
end
print(string.format("longest chain below %d: %d with %d terms", LIMIT, best_n, best_l))
local cached = 0
for _ in pairs(cache) do cached = cached + 1 end
print("cached lengths: " .. cached)
local entries = {}
for length, count in pairs(histogram) do entries[#entries + 1] = { length, count } end
table.sort(entries, function(a, b)
  if a[2] ~= b[2] then return a[2] > b[2] end
  return a[1] < b[1]
end)
for i = 1, 5 do
  print(string.format("length %d: %d starts", entries[i][1], entries[i][2]))
end
local total = 0
for n = 1, LIMIT - 1 do
  total = total + cache[n] % 7
end
print("checksum " .. total)
