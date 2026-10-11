-- Record validation the way a config/ETL loader is written: generate `key=value;...` records,
-- a fraction malformed in various ways, parse each with small functions that return
-- `nil, err` on bad input (checked and passed up by every caller), then aggregate valid
-- records and count errors by kind. Stresses multiple returns, small calls, splitting and tables.

local RECORDS = 150000
local FIRST = { "Ada", "Alan", "Grace", "Edsger", "Barbara", "Donald", "Margaret", "Ken", "Radia", "Niklaus" }
local LAST = { "Lovelace", "Turing", "Hopper", "Dijkstra", "Liskov", "Knuth", "Hamilton", "Thompson", "Perlman" }
local DOMAINS = { "example.com", "mail.org", "post.net", "uni.edu", "corp.io", "inbox.dev", "lab.ai" }
local TAGS = { "admin", "dev", "ops", "qa", "sales", "hr", "ml", "web", "db", "sec", "mobile" }
local FIELDS = { "id", "name", "age", "email", "tags", "score" }

local format, find, sub = string.format, string.find, string.sub

local Rng = {}
Rng.__index = Rng

function Rng.new(seed)
  return setmetatable({ state = seed }, Rng)
end

function Rng:next(n)
  self.state = (self.state * 1103515245 + 12345) % 2147483648
  return self.state // 65536 % n
end

local function split(s, sep)
  local parts, start = {}, 1
  while true do
    local i = find(s, sep, start, true)
    if not i then break end
    parts[#parts + 1] = sub(s, start, i - 1)
    start = i + #sep
  end
  parts[#parts + 1] = sub(s, start)
  return parts
end

local function split_once(s, sep)
  local i = find(s, sep, 1, true)
  if i then return sub(s, 1, i - 1), sub(s, i + #sep) end
end

local function generate(rng, index)
  local first = FIRST[rng:next(#FIRST) + 1]
  local last = LAST[rng:next(#LAST) + 1]
  local id = tostring(index + 1)
  if rng:next(100) < 3 and index > 0 then
    id = tostring(1 + rng:next(index))
  end
  local age = tostring(18 + rng:next(60))
  local email = format("%s.%s@%s", first:lower(), last:lower(), DOMAINS[rng:next(#DOMAINS) + 1])
  local tags = {}
  for i = 1, 1 + rng:next(4) do
    tags[i] = TAGS[rng:next(#TAGS) + 1]
  end
  local points = rng:next(1001)
  local score = format("%d.%d", points // 10, points % 10)
  local separator = "="
  local dropped = ""
  local defect = rng:next(100)
  if defect <= 1 then
    dropped = FIELDS[rng:next(#FIELDS) + 1]
  elseif defect <= 3 then
    age = age .. "x"
  elseif defect == 4 then
    id = "#" .. id
  elseif defect <= 6 then
    age = tostring(131 + rng:next(100))
  elseif defect == 7 then
    age = "-" .. age
  elseif defect <= 9 then
    email = email:gsub("@", "")
  elseif defect == 10 then
    email = first:lower() .. "@localhost"
  elseif defect <= 12 then
    table.insert(tags, rng:next(#tags + 1) + 1, "")
  elseif defect == 13 then
    score = score .. ".5"
  elseif defect == 14 then
    score = "1" .. score
  elseif defect == 15 then
    separator = ":"
  end
  local values = { id, first .. " " .. last, age, email, table.concat(tags, ","), score }
  local pairs = {}
  for i, key in ipairs(FIELDS) do
    if key ~= dropped then
      pairs[#pairs + 1] = key .. (key == "age" and separator or "=") .. values[i]
    end
  end
  return table.concat(pairs, ";")
end

local function fail(kind, message)
  return nil, { kind = kind, message = message }
end

local function parse_fields(line)
  local fields = {}
  for _, part in ipairs(split(line, ";")) do
    local key, value = split_once(part, "=")
    if not key then
      return fail("syntax", format("expected key=value, got `%s`", part))
    end
    fields[key] = value
  end
  return fields
end

local function field(fields, key)
  local value = fields[key]
  if value == nil then
    return fail("missing", format("missing field `%s`", key))
  end
  return value
end

local function parse_int_field(fields, key)
  local text, err = field(fields, key)
  if not text then return nil, err end
  local n = text:match("^-?%d+$") and math.tointeger(tonumber(text))
  if not n then
    return fail("int", format("field `%s`: `%s` is not an integer", key, text))
  end
  return n
end

local function validate_age(age)
  if age < 0 or age > 130 then
    return fail("range", format("field `age`: %d out of range", age))
  end
  return age
end

local function validate_email(email)
  local user, domain = split_once(email, "@")
  if not user then
    return fail("email", format("bad email `%s`: no @", email))
  end
  if user == "" or not find(domain, ".", 1, true) or sub(domain, 1, 1) == "." or sub(domain, -1) == "." then
    return fail("email", format("bad email `%s`: bad domain", email))
  end
  return { user, domain }
end

local function parse_tags(text)
  local tags = split(text, ",")
  for _, tag in ipairs(tags) do
    if tag == "" then
      return fail("tag", format("empty tag in `%s`", text))
    end
  end
  return tags
end

local function parse_score(text)
  local score = text:match("^%d+%.?%d*$") and tonumber(text)
  if not score then
    return fail("score", format("field `score`: `%s` is not a number", text))
  end
  if score < 0.0 or score > 100.0 then
    return fail("range", format("field `score`: `%s` out of range", text))
  end
  return score
end

local function parse_person(line)
  local fields, err = parse_fields(line)
  if not fields then return nil, err end
  local id, name, age, text, address, tags, score
  id, err = parse_int_field(fields, "id")
  if not id then return nil, err end
  name, err = field(fields, "name")
  if not name then return nil, err end
  age, err = parse_int_field(fields, "age")
  if not age then return nil, err end
  age, err = validate_age(age)
  if not age then return nil, err end
  text, err = field(fields, "email")
  if not text then return nil, err end
  address, err = validate_email(text)
  if not address then return nil, err end
  text, err = field(fields, "tags")
  if not text then return nil, err end
  tags, err = parse_tags(text)
  if not tags then return nil, err end
  text, err = field(fields, "score")
  if not text then return nil, err end
  score, err = parse_score(text)
  if not score then return nil, err end
  return { id = id, name = name, age = age, user = address[1], domain = address[2], tags = tags, score = score }
end

local function load(line, people, order)
  local person, err = parse_person(line)
  if not person then return nil, err end
  if people[person.id] then
    return fail("duplicate", format("duplicate id %d", person.id))
  end
  people[person.id] = person
  order[#order + 1] = person
  return person
end

local function sorted_keys(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  table.sort(keys)
  return keys
end

local function top(counts, n)
  local entries = {}
  for k, v in pairs(counts) do
    entries[#entries + 1] = { k, v }
  end
  table.sort(entries, function(a, b)
    if a[2] ~= b[2] then return a[2] > b[2] end
    return a[1] < b[1]
  end)
  local out = {}
  for i = 1, math.min(n, #entries) do
    out[i] = entries[i][1] .. "=" .. entries[i][2]
  end
  return table.concat(out, " ")
end

local rng = Rng.new(1234)
local lines = {}
for i = 0, RECORDS - 1 do
  lines[i + 1] = generate(rng, i)
end

local people, order = {}, {}
local domains, tag_counts, error_counts, first_errors = {}, {}, {}, {}
local total_age, total_score = 0, 0.0
for number, line in ipairs(lines) do
  local person, err = load(line, people, order)
  if person then
    domains[person.domain] = (domains[person.domain] or 0) + 1
    for _, tag in ipairs(person.tags) do
      tag_counts[tag] = (tag_counts[tag] or 0) + 1
    end
    total_age = total_age + person.age
    total_score = total_score + person.score
  else
    error_counts[err.kind] = (error_counts[err.kind] or 0) + 1
    if #first_errors < 8 then
      first_errors[#first_errors + 1] = format("line %d: %s: %s", number, err.kind, err.message)
    end
  end
end

local valid = #order
local best = order[1]
for _, p in ipairs(order) do
  if p.score > best.score or (p.score == best.score and p.id < best.id) then best = p end
end
local error_parts = {}
for _, k in ipairs(sorted_keys(error_counts)) do
  error_parts[#error_parts + 1] = k .. "=" .. error_counts[k]
end
print(format("records %d, valid %d, rejected %d", #lines, valid, #lines - valid))
print("errors: " .. table.concat(error_parts, " "))
print("domains: " .. top(domains, 5))
print("tags: " .. top(tag_counts, 5))
print(format("average age %.2f, average score %.3f", total_age / valid, total_score / valid))
print(format("best: #%d %s <%s@%s> %.1f %s", best.id, best.name, best.user, best.domain, best.score,
  table.concat(best.tags, ",")))
for _, message in ipairs(first_errors) do
  print(message)
end
