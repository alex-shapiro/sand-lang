-- Benchmarks Game fannkuch-redux: count prefix reversals over all permutations.
-- Stresses small integer arrays: copying, in-place reversal and rotation.

local N = 9

local function flip_count(perm)
  local flips = 0
  local k = perm[1]
  while k ~= 0 do
    local i, j = 1, k + 1
    while i < j do
      perm[i], perm[j] = perm[j], perm[i]
      i = i + 1
      j = j - 1
    end
    flips = flips + 1
    k = perm[1]
  end
  return flips
end

-- Returns the checksum and the maximum number of flips over all permutations of 0..n-1.
local function fannkuch(n)
  local perm, count = {}, {}
  for i = 1, n do
    perm[i] = i - 1
    count[i] = 0
  end
  local max_flips, checksum, sign = 0, 0, 1
  local r = n
  while true do
    while r ~= 1 do
      count[r] = r
      r = r - 1
    end
    local flips = flip_count(table.move(perm, 1, n, 1, {}))
    max_flips = math.max(max_flips, flips)
    checksum = checksum + sign * flips
    sign = -sign
    while true do
      if r == n then
        return checksum, max_flips
      end
      table.insert(perm, r + 1, table.remove(perm, 1))
      count[r + 1] = count[r + 1] - 1
      if count[r + 1] > 0 then
        break
      end
      r = r + 1
    end
  end
end

local checksum, max_flips = fannkuch(N)
print(checksum)
print(string.format("Pfannkuchen(%d) = %d", N, max_flips))
