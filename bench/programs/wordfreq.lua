-- Word-frequency analysis over a generated text of ~300k words.
-- Stresses string splitting, lowercasing, slicing, concatenation,
-- string-keyed table updates and sorting by composite keys.

local VOCAB_SIZE = 5000
local WORD_COUNT = 300000
local SYLLABLES = {
  "ka", "to", "ri", "men", "sa", "lo", "ve", "qu", "an", "del",
  "pho", "ny", "ex", "ur", "bi", "ston", "gle", "ad", "ter", "op",
}
local PUNCTUATION = { ",", ".", ";", "!" }

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

local function make_word(rng)
  local parts = {}
  for i = 1, 2 + rng:next(3) do
    parts[i] = SYLLABLES[rng:next(#SYLLABLES) + 1]
  end
  return table.concat(parts)
end

-- A vocabulary word as it appears in running text: skewed towards
-- frequent words, sometimes capitalized, sometimes quoted or punctuated.
local function make_token(rng, vocabulary)
  local n = #vocabulary
  local a = rng:next(n)
  local b = rng:next(n)
  local word = vocabulary[a * b // n + 1]
  local token
  local case = rng:next(10)
  if case == 0 then
    token = word:sub(1, 1):upper() .. word:sub(2)
  elseif case == 1 then
    token = word:upper()
  else
    token = word
  end
  if rng:next(25) == 0 then
    token = '"' .. token .. '"'
  end
  local p = rng:next(8)
  if p < #PUNCTUATION then
    token = token .. PUNCTUATION[p + 1]
  end
  return token
end

local function make_text(rng, vocabulary)
  local lines = {}
  local written = 0
  while written < WORD_COUNT do
    local n = math.min(8 + rng:next(9), WORD_COUNT - written)
    local tokens = {}
    for i = 1, n do
      tokens[i] = make_token(rng, vocabulary)
    end
    lines[#lines + 1] = table.concat(tokens, " ")
    written = written + n
  end
  return table.concat(lines, "\n")
end

local function top(counts, n)
  local entries = {}
  for key, count in pairs(counts) do
    entries[#entries + 1] = { key = key, count = count }
  end
  table.sort(entries, function(a, b)
    if a.count ~= b.count then
      return a.count > b.count
    end
    return a.key < b.key
  end)
  local result = {}
  for i = 1, math.min(n, #entries) do
    result[i] = entries[i]
  end
  return result
end

local rng = Rng.new(42)
local vocabulary = {}
for i = 1, VOCAB_SIZE do
  vocabulary[i] = make_word(rng)
end
local text = make_text(rng, vocabulary)

local words, bigrams = {}, {}
local distinct_words, distinct_bigrams = 0, 0
local total = 0
for line in text:gmatch("[^\n]+") do
  local previous = nil
  for raw in line:gmatch("%S+") do
    local word = raw:lower():match('^[,.;!"]*(.-)[,.;!"]*$')
    if word ~= "" then
      total = total + 1
      local count = words[word]
      if not count then
        distinct_words = distinct_words + 1
      end
      words[word] = (count or 0) + 1
      if previous then
        local bigram = previous .. " " .. word
        local bcount = bigrams[bigram]
        if not bcount then
          distinct_bigrams = distinct_bigrams + 1
        end
        bigrams[bigram] = (bcount or 0) + 1
      end
      previous = word
    end
  end
end

print(string.format("total words: %d", total))
print(string.format("distinct words: %d", distinct_words))
print(string.format("distinct bigrams: %d", distinct_bigrams))
print("top words:")
for _, e in ipairs(top(words, 20)) do
  print(string.format("  %d %s", e.count, e.key))
end
print("top bigrams:")
for _, e in ipairs(top(bigrams, 10)) do
  print(string.format("  %d %s", e.count, e.key))
end
