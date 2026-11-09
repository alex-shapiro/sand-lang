local N = 200000
local seed = 1
local data = {}
for i = 1, N do seed = (seed * 1103515245 + 12345) % 2147483648; data[i] = seed end
local function copy(t) local c = {} for i = 1, #t do c[i] = t[i] end return c end
local function median(f) local ts = {} for r = 1, 5 do local c = copy(data) local t0 = os.clock() f(c) ts[r] = os.clock() - t0 end table.sort(ts) return ts[3] * 1000 end
print(string.format("E6 %s sort %d ints: table.sort with Lua comparator %.1f ms; table.sort default %.1f ms", _VERSION, N,
  median(function(c) table.sort(c, function(a, b) return a < b end) end), median(function(c) table.sort(c) end)))
