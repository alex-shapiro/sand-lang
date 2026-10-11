-- A small text adventure. Reads commands from standard input.
--
-- Lua port of examples/adventure.sand.
--
-- Commands: look, go DIRECTION (or n/s/e/w), take ITEM, drop ITEM,
-- use ITEM, inventory (or i), help, quit.
--
-- Usage: lua5.5 adventure.lua < COMMANDS

-- Directions are their names; this maps every spelling to the name.
local DIRECTIONS = {
  north = "north", n = "north",
  south = "south", s = "south",
  east = "east", e = "east",
  west = "west", w = "west",
}

--- A set of strings: a table of `item = true`.
local function set(items)
  local s = {}
  for _, item in ipairs(items) do s[item] = true end
  return s
end

local function sorted_list(s)
  local list = {}
  for item in pairs(s) do list[#list + 1] = item end
  table.sort(list)
  return table.concat(list, ", ")
end

--- A room's `exits` is a list of `{ dir, destination }` pairs, in display order.
local function exit_to(room, dir)
  for _, exit in ipairs(room.exits) do
    if exit[1] == dir then return exit[2] end
  end
end

local Game = {}
Game.__index = Game

function Game.new(rooms, here)
  return setmetatable({ rooms = rooms, here = here, inventory = {}, moves = 0, won = false }, Game)
end

function Game:room()
  return self.rooms[self.here]
end

function Game:describe()
  local r = self:room()
  local exits = {}
  for i, exit in ipairs(r.exits) do exits[i] = exit[1] end
  local text = "== " .. r.name .. " ==\n" .. r.description .. "\nExits: " .. table.concat(exits, ", ")
  if next(r.items) then
    text = text .. "\nYou see: " .. sorted_list(r.items)
  end
  return text
end

function Game:go(dir)
  local r = self:room()
  local dest = exit_to(r, dir)
  if not dest then
    return "You can't go " .. dir .. "."
  end
  if r.locked == dir then
    return "The door to the " .. dir .. " is locked."
  end
  self.here = dest
  self.moves = self.moves + 1
  if self.here == "vault" then
    self.won = true
    return self:describe() .. "\nYou found the treasure in " .. self.moves .. " moves. You win!"
  end
  return self:describe()
end

function Game:take(item)
  local r = self:room()
  if not r.items[item] then
    return "There is no " .. item .. " here."
  end
  r.items[item] = nil
  self.inventory[item] = true
  return "Taken: " .. item .. "."
end

function Game:drop(item)
  if not self.inventory[item] then
    return "You don't have " .. item .. "."
  end
  self.inventory[item] = nil
  self:room().items[item] = true
  return "Dropped: " .. item .. "."
end

function Game:use_item(item)
  if not self.inventory[item] then
    return "You don't have " .. item .. "."
  end
  local r = self:room()
  if item == "key" and r.locked then
    local dir = r.locked
    r.locked = nil
    return "You unlock the door to the " .. dir .. "."
  end
  return "Nothing happens."
end

--- The reply to a command line, or nil to quit.
function Game:command(line)
  local words = {}
  for word in line:lower():gmatch("%S+") do words[#words + 1] = word end
  local n, verb, object = #words, words[1], words[2]
  if n == 0 then
    return ""
  elseif n == 1 and (verb == "quit" or verb == "exit") then
    return nil
  elseif n == 1 and (verb == "look" or verb == "l") then
    return self:describe()
  elseif n == 1 and (verb == "inventory" or verb == "i") then
    if not next(self.inventory) then
      return "You are empty-handed."
    end
    return "You carry: " .. sorted_list(self.inventory)
  elseif n == 2 and verb == "go" and DIRECTIONS[object] then
    return self:go(DIRECTIONS[object])
  elseif n == 1 and DIRECTIONS[verb] then
    return self:go(DIRECTIONS[verb])
  elseif n == 2 and verb == "take" then
    return self:take(object)
  elseif n == 2 and verb == "drop" then
    return self:drop(object)
  elseif n == 2 and verb == "use" then
    return self:use_item(object)
  elseif n == 1 and verb == "help" then
    return "Commands: look, go DIR, take ITEM, drop ITEM, use ITEM, inventory, quit"
  end
  return "I don't understand that."
end

local function world()
  return {
    hall = {
      name = "Great Hall",
      description = "A dusty hall with portraits on the walls.",
      exits = { { "north", "library" }, { "east", "kitchen" } },
      items = set { "lamp" },
    },
    library = {
      name = "Library",
      description = "Shelves of crumbling books. A heavy door leads east.",
      exits = { { "south", "hall" }, { "east", "vault" } },
      items = set { "book" },
      locked = "east",
    },
    kitchen = {
      name = "Kitchen",
      description = "It smells of old bread.",
      exits = { { "west", "hall" } },
      items = set { "key", "bread" },
    },
    vault = {
      name = "Vault",
      description = "Gold glitters everywhere.",
      exits = { { "west", "library" } },
      items = set { "treasure" },
    },
  }
end

local game = Game.new(world(), "hall")
io.write(game:describe(), "\n")
while not game.won do
  io.write("> ")
  local line = io.read("l")
  if not line then break end
  local reply = game:command(line)
  if reply == nil then
    io.write("Goodbye.\n")
    break
  end
  if reply ~= "" then
    io.write(reply, "\n")
  end
end
