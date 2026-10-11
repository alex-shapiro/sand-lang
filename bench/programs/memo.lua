-- Memoized recursive dynamic programming: lattice paths around obstacles,
-- edit distance and 0/1 knapsack, each memoized in a table keyed by an
-- integer computed from the two indices.
-- Stresses table get/insert, closures and recursion.

local MOD = 1000000007
local GRIDS = 12
local GRID_SIZE = 300
local PAIRS = 450
local KNAPSACKS = 12
local ITEMS = 100
local CAPACITY = 1000
local BASES = { "A", "C", "G", "T" }

local Rng = {}
Rng.__index = Rng

function Rng.new(state)
  return setmetatable({ state = state }, Rng)
end

-- A pseudo-random integer in [0, n) (n at most 32768), from the high bits.
function Rng:next(n)
  self.state = (self.state * 1103515245 + 12345) % 2147483648
  return self.state // 65536 % n
end

-- A square grid (1-based) where true marks an obstacle; the corners are always open.
local function make_grid(rng, size)
  local grid = {}
  for r = 1, size do
    local row = {}
    for c = 1, size do
      row[c] = rng:next(8) == 0
    end
    grid[r] = row
  end
  grid[1][1] = false
  grid[size][size] = false
  return grid
end

-- The number of monotone (down/right) paths from the top-left to the
-- bottom-right corner that avoid obstacles, modulo MOD.
local function count_paths(grid)
  local width = #grid[1]
  local memo = {}
  local function paths(r, c)
    if r < 1 or c < 1 or grid[r][c] then
      return 0
    end
    if r == 1 and c == 1 then
      return 1
    end
    local key = r * width + c
    local n = memo[key]
    if n then
      return n
    end
    n = (paths(r - 1, c) + paths(r, c - 1)) % MOD
    memo[key] = n
    return n
  end
  return paths(#grid, width)
end

local function random_string(rng)
  local n = 40 + rng:next(41)
  local chars = {}
  for i = 1, n do
    chars[i] = BASES[rng:next(#BASES) + 1]
  end
  return table.concat(chars)
end

local function edit_distance(a, b)
  local la, lb = #a, #b
  local memo = {}
  local function dist(i, j)
    if i > la then
      return lb - j + 1
    end
    if j > lb then
      return la - i + 1
    end
    local key = i * (lb + 1) + j
    local d = memo[key]
    if d then
      return d
    end
    if a:byte(i) == b:byte(j) then
      d = dist(i + 1, j + 1)
    else
      d = 1 + math.min(dist(i + 1, j), dist(i, j + 1), dist(i + 1, j + 1))
    end
    memo[key] = d
    return d
  end
  return dist(1, 1)
end

local function knapsack(items, capacity)
  local memo = {}
  local function best(i, room)
    if i > #items then
      return 0
    end
    local key = i * (capacity + 1) + room
    local v = memo[key]
    if v then
      return v
    end
    local item = items[i]
    v = best(i + 1, room)
    if item.weight <= room then
      v = math.max(v, item.value + best(i + 1, room - item.weight))
    end
    memo[key] = v
    return v
  end
  return best(1, capacity)
end

local rng = Rng.new(42)

for g = 0, GRIDS - 1 do
  print(string.format("grid %d: %d paths", g, count_paths(make_grid(rng, GRID_SIZE))))
end

local total_distance = 0
for p = 0, PAIRS - 1 do
  local a = random_string(rng)
  local b = random_string(rng)
  local d = edit_distance(a, b)
  if p < 5 then
    print(string.format("distance %dx%d: %d", #a, #b, d))
  end
  total_distance = total_distance + d
end
print(string.format("total distance: %d", total_distance))

for k = 0, KNAPSACKS - 1 do
  local items = {}
  for i = 1, ITEMS do
    local weight = 1 + rng:next(100)
    items[i] = { weight = weight, value = 1 + rng:next(500) }
  end
  print(string.format("knapsack %d: %d", k, knapsack(items, CAPACITY)))
end
