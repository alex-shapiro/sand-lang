-- Generator-style streaming: stateful producers (Fibonacci mod m, primes by trial division,
-- Collatz sequences, a random stream) composed with merge, moving-sum, chunking and run-length
-- transformers into lazy pipelines that are built first and consumed later. Stateful generators
-- are coroutines, simple adapters are closure iterators. Stresses coroutine switches and closures.

local FIB_COUNT = 300000
local WINDOW = 16
local PRIME_LIMIT = 300000
local CHUNK = 8
local COLLATZ_SEEDS = 12000
local RANDOM_COUNT = 300000
local MOD = 1000000007

local wrap, yield = coroutine.wrap, coroutine.yield

-- Closure adapters: each takes and returns an iterator function that yields nil when done.

local function map(source, f)
  return function()
    local x = source()
    if x ~= nil then return f(x) end
  end
end

local function filter(source, keep)
  return function()
    for x in source do
      if keep(x) then return x end
    end
  end
end

local function take(source, n)
  return function()
    if n <= 0 then return nil end
    n = n - 1
    return source()
  end
end

local function take_while(source, keep)
  local done = false
  return function()
    if done then return nil end
    local x = source()
    if x ~= nil and keep(x) then return x end
    done = true
  end
end

local function skip(source, n)
  return function()
    while n > 0 do
      n = n - 1
      source()
    end
    return source()
  end
end

local function step_by(source, step)
  local first = true
  return function()
    if first then
      first = false
      return source()
    end
    for _ = 1, step - 1 do
      source()
    end
    return source()
  end
end

local function chain(first, second)
  return function()
    local x = first()
    if x ~= nil then return x end
    return second()
  end
end

local function zip(left, right)
  return function()
    local x, y = left(), right()
    if x ~= nil and y ~= nil then return { x, y } end
  end
end

local function flat_map(source, f)
  return wrap(function()
    for x in source do
      for y in f(x) do
        yield(y)
      end
    end
  end)
end

local function count_from(start, step)
  step = step or 1
  local n = start - step
  return function()
    n = n + step
    return n
  end
end

local function fold(source, acc, f)
  for x in source do
    acc = f(acc, x)
  end
  return acc
end

local function count(source)
  local n = 0
  for _ in source do
    n = n + 1
  end
  return n
end

-- Generators.

local function fib_mod(m)
  return wrap(function()
    local a, b = 0, 1
    while true do
      yield(a)
      a, b = b, (a + b) % m
    end
  end)
end

local function random_stream(seed)
  return wrap(function()
    local state = seed
    while true do
      state = (state * 1103515245 + 12345) % 2147483648
      yield(state // 65536)
    end
  end)
end

-- Primes in increasing order, each tested by trial division against the primes found so far.
local function primes()
  return wrap(function()
    local found = {}
    local n = 1
    while true do
      n = n + 1
      local is_prime = true
      for i = 1, #found do
        local p = found[i]
        if p * p > n then break end
        if n % p == 0 then
          is_prime = false
          break
        end
      end
      if is_prime then
        found[#found + 1] = n
        yield(n)
      end
    end
  end)
end

local function collatz(seed)
  return wrap(function()
    local n = seed
    yield(n)
    while n ~= 1 do
      if n % 2 == 0 then
        n = n // 2
      else
        n = 3 * n + 1
      end
      yield(n)
    end
  end)
end

-- Merges two ascending streams into one ascending stream.
local function merge(left, right)
  return wrap(function()
    local a, b = left(), right()
    while a ~= nil or b ~= nil do
      if b == nil or (a ~= nil and a <= b) then
        yield(a)
        a = left()
      else
        yield(b)
        b = right()
      end
    end
  end)
end

-- Sums of the last `width` elements (fewer at the start).
local function moving_sum(source, width)
  return wrap(function()
    local window = {}
    for i = 1, width do
      window[i] = 0
    end
    local i, total = 1, 0
    for x in source do
      total = total + x - window[i]
      window[i] = x
      i = i % width + 1
      yield(total)
    end
  end)
end

local function chunks(source, size)
  return wrap(function()
    while true do
      local chunk = {}
      for x in take(source, size) do
        chunk[#chunk + 1] = x
      end
      if #chunk == 0 then return end
      yield(chunk)
    end
  end)
end

-- Runs of equal consecutive elements as `value, length` pairs.
local function run_lengths(source)
  return wrap(function()
    local current = source()
    while current ~= nil do
      local value, length = current, 1
      while true do
        current = source()
        if current ~= value then break end
        length = length + 1
      end
      yield(value, length)
    end
  end)
end

local function digest(acc, x)
  return (acc * 31 + x) % MOD
end

local function fibonacci_windows()
  local sums = moving_sum(take(fib_mod(1000), FIB_COUNT), WINDOW)
  local odd = filter(sums, function(s) return s % 2 == 1 end)
  local checksum = fold(odd, 0, digest)
  local peak, peak_at, index = nil, nil, 0
  for s in moving_sum(take(fib_mod(1000), FIB_COUNT), WINDOW) do
    if peak == nil or s > peak then peak, peak_at = s, index end
    index = index + 1
  end
  local digits = map(random_stream(3), function(x) return x % 1000 end)
  local calm = count(take_while(skip(moving_sum(digits, WINDOW), WINDOW), function(s) return s >= 3300 end))
  print(string.format("fib: checksum %d, peak %d at %d, %d random sums above 3300", checksum, peak, peak_at, calm))
end

local function prime_streams()
  local squares = map(count_from(1), function(n) return n * n end)
  local merged = take_while(merge(primes(), squares), function(x) return x < PRIME_LIMIT end)
  local groups = chunks(merged, CHUNK)
  local group_sums = map(groups, function(g)
    local total = 0
    for _, x in ipairs(g) do
      total = total + x
    end
    return total
  end)
  local checksum = fold(group_sums, 0, digest)
  local pairs = take_while(zip(primes(), skip(primes(), 1)), function(p) return p[2] < PRIME_LIMIT end)
  local twins = count(filter(pairs, function(p) return p[2] - p[1] == 2 end))
  local gaps = map(
    step_by(take_while(primes(), function(p) return p < PRIME_LIMIT end), 100),
    function(p) return p % 1000 end
  )
  local sampled = {}
  for p in take(gaps, 12) do
    sampled[#sampled + 1] = p
  end
  print(string.format("primes: merged checksum %d, twins %d", checksum, twins))
  print("primes: sampled [" .. table.concat(sampled, ", ") .. "]")
end

local function collatz_streams()
  local steps, seed = 0, 0
  for s in take(count_from(1), COLLATZ_SEEDS) do
    local n = count(collatz(s))
    if n >= steps then steps, seed = n, s end
  end
  local all_values = flat_map(take(count_from(1), COLLATZ_SEEDS), collatz)
  local checksum, index = 0, 0
  for x in all_values do
    if index % 7 == 0 then checksum = digest(checksum, x) end
    index = index + 1
  end
  local peak = 0
  for x in flat_map(take(count_from(1, 3), COLLATZ_SEEDS // 3), collatz) do
    if x > peak then peak = x end
  end
  print(string.format("collatz: longest %d from %d, peak %d, checksum %d", steps, seed, peak, checksum))
end

local function random_runs()
  local symbols = take(map(random_stream(42), function(x) return x % 3 end), RANDOM_COUNT)
  local tail = take(map(random_stream(7), function(x) return x % 2 end), RANDOM_COUNT // 10)
  local runs = run_lengths(chain(symbols, tail))
  local histogram = {}
  for i = 1, 9 do
    histogram[i] = 0
  end
  local n, longest_value, longest = 0, 0, 0
  for value, length in runs do
    local bucket = math.min(length, 9)
    histogram[bucket] = histogram[bucket] + 1
    n = n + 1
    if length > longest then longest_value, longest = value, length end
  end
  print(string.format("runs: %d runs, longest %d of %d, histogram [%s]", n, longest, longest_value,
    table.concat(histogram, ", ")))
end

fibonacci_windows()
prime_streams()
collatz_streams()
random_runs()
