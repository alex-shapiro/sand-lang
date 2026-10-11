-- Dijkstra's shortest paths from several sources over a random weighted digraph.
-- Stresses small table allocation, array indexing, small functions and a
-- hand-written binary heap.

local NODES = 40000
local MAX_OUT_DEGREE = 9
local SOURCES = 8
local INF = math.maxinteger

local Rng = {}
Rng.__index = Rng

function Rng.new(seed)
  return setmetatable({ state = seed }, Rng)
end

function Rng:next(bound)
  self.state = (self.state * 1103515245 + 12345) % 2147483648
  return self.state * bound // 2147483648
end

local function less(a, b)
  return a[1] < b[1] or (a[1] == b[1] and a[2] < b[2])
end

-- A binary min-heap of {dist, node} pairs.
local Heap = {}
Heap.__index = Heap

function Heap.new()
  return setmetatable({ items = {}, n = 0 }, Heap)
end

function Heap:push(item)
  local items = self.items
  local n = self.n + 1
  self.n = n
  items[n] = item
  local i = n
  while i > 1 do
    local parent = i // 2
    if not less(item, items[parent]) then break end
    items[i], items[parent] = items[parent], items[i]
    i = parent
  end
end

function Heap:pop()
  local n = self.n
  if n == 0 then return nil end
  local items = self.items
  local top = items[1]
  items[1] = items[n]
  items[n] = nil
  n = n - 1
  self.n = n
  local i = 1
  while true do
    local left = 2 * i
    if left > n then break end
    local right = left + 1
    local child = left
    if right <= n and less(items[right], items[left]) then child = right end
    if not less(items[child], items[i]) then break end
    items[i], items[child] = items[child], items[i]
    i = child
  end
  return top
end

local function random_graph(rng)
  local graph = {}
  for u = 1, NODES do
    local edges = {}
    for k = 1, 1 + rng:next(MAX_OUT_DEGREE) do
      local target = rng:next(NODES)
      edges[k] = { target, 1 + rng:next(100) }
    end
    graph[u] = edges
  end
  return graph
end

-- Nodes are numbered from 0 as in the other ports; graph[u + 1] holds u's edges.
local function shortest_paths(graph, source)
  local dist = {}
  for i = 1, #graph do dist[i] = INF end
  dist[source + 1] = 0
  local heap = Heap.new()
  heap:push({ 0, source })
  while true do
    local entry = heap:pop()
    if not entry then break end
    local d, u = entry[1], entry[2]
    if d <= dist[u + 1] then
      for _, edge in ipairs(graph[u + 1]) do
        local v = edge[1]
        local nd = d + edge[2]
        if nd < dist[v + 1] then
          dist[v + 1] = nd
          heap:push({ nd, v })
        end
      end
    end
  end
  return dist
end

local rng = Rng.new(42)
local graph = random_graph(rng)
for _ = 1, SOURCES do
  local source = rng:next(NODES)
  local reached, max, sum = 0, 0, 0
  for _, d in ipairs(shortest_paths(graph, source)) do
    if d < INF then
      reached = reached + 1
      if d > max then max = d end
      sum = sum + d
    end
  end
  print(string.format("source %d: reached %d, max %d, sum %d", source, reached, max, sum))
end
