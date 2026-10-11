-- Sparse graph analytics over large integer node ids, as when processing an
-- edge list exported from a database: connected components, degree histogram
-- and a topological sort. Stresses integer-keyed tables, tables as sets and
-- array-as-queue BFS.

local NODES = 200000
local EDGES = 500000
local CLUSTER = 25
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

-- Sizes of the connected components, in order of their first node in ids.
local function component_sizes(ids, adj)
  local visited = {}
  local sizes = {}
  for _, id in ipairs(ids) do
    if not visited[id] then
      visited[id] = true
      local queue = { id }
      local head = 1
      while head <= #queue do
        local u = queue[head]
        head = head + 1
        for _, v in ipairs(adj[u]) do
          if not visited[v] then
            visited[v] = true
            queue[#queue + 1] = v
          end
        end
      end
      sizes[#sizes + 1] = #queue
    end
  end
  return sizes
end

local function degree_histogram(adj)
  local hist = {}
  for _, neighbors in pairs(adj) do
    hist[#neighbors] = (hist[#neighbors] or 0) + 1
  end
  return hist
end

-- Kahn's algorithm on the DAG that orients every edge from the smaller id to
-- the larger; ties in the initial queue are broken by id.
local function topological_order(ids, edges)
  local successors = {}
  local in_degree = {}
  for _, id in ipairs(ids) do in_degree[id] = 0 end
  for _, edge in ipairs(edges) do
    local low, high = edge[1], edge[2]
    if high < low then low, high = high, low end
    local next = successors[low]
    if not next then
      next = {}
      successors[low] = next
    end
    next[#next + 1] = high
    in_degree[high] = in_degree[high] + 1
  end
  local order = {}
  for id, degree in pairs(in_degree) do
    if degree == 0 then order[#order + 1] = id end
  end
  table.sort(order)
  local head = 1
  while head <= #order do
    local u = order[head]
    head = head + 1
    local next = successors[u]
    if next then
      for _, v in ipairs(next) do
        local degree = in_degree[v] - 1
        in_degree[v] = degree
        if degree == 0 then order[#order + 1] = v end
      end
    end
  end
  return order
end

local rng = Rng.new(7)
local ids = {}
for i = 1, NODES do ids[i] = rng:next(1000000000) end
local adj = {}
local node_count = 0
for _, id in ipairs(ids) do
  if not adj[id] then node_count = node_count + 1 end
  adj[id] = {}
end
local edges = {}
for _ = 1, EDGES do
  local i = rng:next(NODES)
  local j
  if rng:next(1000) < 5 then
    j = rng:next(NODES)
  else
    j = i // CLUSTER * CLUSTER + rng:next(CLUSTER)
  end
  local a, b = ids[i + 1], ids[j + 1]
  if a ~= b then
    local na, nb = adj[a], adj[b]
    na[#na + 1] = b
    nb[#nb + 1] = a
    edges[#edges + 1] = { a, b }
  end
end
print(string.format("nodes %d, edges %d", node_count, #edges))

local sizes = component_sizes(ids, adj)
local sorted = table.move(sizes, 1, #sizes, 1, {})
table.sort(sorted, function(x, y) return x > y end)
print(string.format("components %d, largest %s", #sizes, table.concat(sorted, " ", 1, math.min(5, #sorted))))

local hist = degree_histogram(adj)
for degree = 1, 10 do
  print(string.format("degree %d: %d", degree, hist[degree] or 0))
end

local order = topological_order(ids, edges)
local checksum = 0
for position, id in ipairs(order) do
  checksum = (checksum + (position - 1) * id) % MOD
end
print(string.format("topological order %d, checksum %d", #order, checksum))
