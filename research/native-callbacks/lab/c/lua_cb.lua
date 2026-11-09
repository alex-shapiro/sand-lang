print("== " .. RELEASE)
local function inc(x) return x + 1 end
-- E1: per-call cost, script->script vs native->script. Median of 5 runs.
local N = 10000000
local function median(t) table.sort(t) return t[(#t + 1) // 2] end
local s2s, n2s, base = {}, {}, {}
for r = 1, 5 do
  local t0 = native.now(); local acc0 = 0
  for i = 0, N - 1 do acc0 = acc0 + (i + 1) end
  base[r] = (native.now() - t0) / N * 1e9
  t0 = native.now(); local acc = 0
  for i = 0, N - 1 do acc = acc + inc(i) end
  s2s[r] = (native.now() - t0) / N * 1e9
  t0 = native.now(); local acc2 = native.repeat_(inc, N)
  n2s[r] = (native.now() - t0) / N * 1e9
  assert(acc == acc2)
end
print(string.format("E1 loop without call: %.1f ns/iter; script->script call loop: %.1f ns/iter; native->script callback loop: %.1f ns/iter", median(base), median(s2s), median(n2s)))
-- E2: depth of native<->script recursion vs pure script recursion
local depth = 0
local function f(d) depth = d; return native.bounce(f, d + 1) end
local ok, err = pcall(f, 0)
print("E2 native<->script max depth: " .. depth .. " | error: " .. tostring(err))
local sdepth = 0
local function g(d) sdepth = d; return 1 + g(d + 1) end
ok, err = pcall(g, 0)
print("E2 script-only max depth: " .. sdepth .. " | error: " .. tostring(err))
-- E3: yield inside a callback
local co = coroutine.create(function() return native.call_plain(function() coroutine.yield("y") return "r" end) end)
print("E3 lua_call + yield:", coroutine.resume(co))
co = coroutine.create(function() return native.call_k(function() coroutine.yield("y") return "r" end) end)
print("E3 lua_callk + yield (first resume):", coroutine.resume(co))
print("E3 lua_callk + yield (second resume):", coroutine.resume(co))
-- E4: error in a callback unwinds through the native and skips its cleanup
ok, err = pcall(native.with_resource, function() error("boom") end)
print("E4 callback error:", ok, err, "acquired/cleaned:", native.counts())
-- E4: instruction-count hook fires inside a callback called from C
native.set_budget(1000000)
ok, err = pcall(native.repeat_, function(x) while true do end end, 1)
debug.sethook()
print("E4 hook inside callback:", ok, err)
-- E5: allocation failure inside a callback
native.set_limit(1000000)
ok, err = pcall(native.repeat_, function(x) local t = {} for i = 1, 1e7 do t[i] = i end return 0 end, 1)
native.clear_limit()
print("E5 memory limit inside callback:", ok, err)
-- E6: reentrancy hazard: comparator mutates the table during table.sort
local t = {} for i = 1, 100 do t[i] = (i * 7919) % 101 end
ok, err = pcall(table.sort, t, function(a, b) t[#t + 1] = 0; t[1] = nil; return a < b end)
print("E6 table.sort with mutating comparator:", ok, err)
local u = {} for i = 1, 200 do u[i] = i end
ok, err = pcall(table.sort, u, function(a, b) return true end)
print("E6 table.sort with inconsistent comparator:", ok, err)
-- E7: host stores a handler and calls it after the script ends
native.on_event(function(ev) print("E7 handler called later with", ev) end)
