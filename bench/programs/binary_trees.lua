-- Benchmarks Game binary-trees: build and walk many perfect binary trees.
-- Stresses allocation of small tables, recursion and the GC.

local MIN_DEPTH = 4
local MAX_DEPTH = 15

local function bottom_up(depth)
  if depth == 0 then
    return {}
  end
  depth = depth - 1
  return { left = bottom_up(depth), right = bottom_up(depth) }
end

local function check(node)
  if node.left then
    return 1 + check(node.left) + check(node.right)
  end
  return 1
end

local stretch_depth = MAX_DEPTH + 1
print(string.format("stretch tree of depth %d\t check: %d", stretch_depth, check(bottom_up(stretch_depth))))

local long_lived = bottom_up(MAX_DEPTH)

for depth = MIN_DEPTH, MAX_DEPTH, 2 do
  local iterations = 1 << (MAX_DEPTH - depth + MIN_DEPTH)
  local total = 0
  for _ = 1, iterations do
    total = total + check(bottom_up(depth))
  end
  print(string.format("%d\t trees of depth %d\t check: %d", iterations, depth, total))
end

print(string.format("long lived tree of depth %d\t check: %d", MAX_DEPTH, check(long_lived)))
