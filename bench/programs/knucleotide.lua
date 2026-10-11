-- Benchmarks Game k-nucleotide over a generated DNA sequence: count every
-- k-mer for several k with substrings as table keys, then report frequencies.
-- Stresses substring creation and string-keyed hashing.

local LENGTH = 600000
local QUERIES = { "GGT", "GGTA", "GGTATT", "GGTATTTTAATT", "GGTATTTTAATTTATAGT" }

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

-- A random base with the frequencies of the fasta benchmark's Homo sapiens
-- alphabet, rounded to thousandths.
local function random_base(rng)
  local r = rng:next(1000)
  if r <= 302 then
    return "A"
  elseif r <= 500 then
    return "C"
  elseif r <= 698 then
    return "G"
  end
  return "T"
end

local function frequencies(seq, k)
  local counts = {}
  for i = 1, #seq - k + 1 do
    local key = seq:sub(i, i + k - 1)
    counts[key] = (counts[key] or 0) + 1
  end
  return counts
end

local function print_frequencies(seq, k)
  local total = #seq - k + 1
  local entries = {}
  for key, count in pairs(frequencies(seq, k)) do
    entries[#entries + 1] = { key = key, count = count }
  end
  table.sort(entries, function(a, b)
    if a.count ~= b.count then
      return a.count > b.count
    end
    return a.key < b.key
  end)
  for _, e in ipairs(entries) do
    print(string.format("%s %.3f", e.key, 100.0 * e.count / total))
  end
  print("")
end

local rng = Rng.new(42)
local bases = {}
for i = 1, LENGTH do
  bases[i] = random_base(rng)
end
local seq = table.concat(bases)

print_frequencies(seq, 1)
print_frequencies(seq, 2)
for _, query in ipairs(QUERIES) do
  print(string.format("%d\t%s", frequencies(seq, #query)[query] or 0, query))
end
