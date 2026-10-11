-- Benchmarks Game spectral-norm: power method on an infinite matrix A.
-- Stresses small function calls, float arithmetic and array indexing.

local N = 800

local function eval_a(i, j)
  local ij = i + j - 2
  return 1.0 / ((ij * (ij + 1)) // 2 + i)
end

local function mul_av(v, av)
  local n = #v
  for i = 1, n do
    local sum = 0.0
    for j = 1, n do
      sum = sum + eval_a(i, j) * v[j]
    end
    av[i] = sum
  end
end

local function mul_atv(v, atv)
  local n = #v
  for i = 1, n do
    local sum = 0.0
    for j = 1, n do
      sum = sum + eval_a(j, i) * v[j]
    end
    atv[i] = sum
  end
end

local function mul_atav(v, out, tmp)
  mul_av(v, tmp)
  mul_atv(tmp, out)
end

local u, v, tmp = {}, {}, {}
for i = 1, N do
  u[i], v[i], tmp[i] = 1.0, 0.0, 0.0
end
for _ = 1, 10 do
  mul_atav(u, v, tmp)
  mul_atav(v, u, tmp)
end

local vbv, vv = 0.0, 0.0
for i = 1, N do
  vbv = vbv + u[i] * v[i]
  vv = vv + v[i] * v[i]
end
print(string.format("%.9f", math.sqrt(vbv / vv)))
