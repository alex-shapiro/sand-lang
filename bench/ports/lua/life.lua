-- Conway's Game of Life on a toroidal grid.
--
-- Lua port of examples/life.sand.
--
-- Usage: lua5.5 life.lua [generations]

local Board = {}
Board.__index = Board

--- Parses a board where `#` is alive and anything else is dead.
function Board.parse(text)
  local rows = {}
  for line in (text:match("^%s*(.-)%s*$") .. "\n"):gmatch("(.-)\n") do
    local row = {}
    for c in line:match("^%s*(.-)%s*$"):gmatch(".") do
      row[#row + 1] = c == "#"
    end
    rows[#rows + 1] = row
  end
  return setmetatable({ width = #rows[1], height = #rows, cells = rows }, Board)
end

-- Coordinates are 0-based; floor modulo wraps negative ones around the edges.
function Board:alive(r, c)
  return self.cells[r % self.height + 1][c % self.width + 1]
end

function Board:neighbors(r, c)
  local n = 0
  for dr = -1, 1 do
    for dc = -1, 1 do
      if (dr ~= 0 or dc ~= 0) and self:alive(r + dr, c + dc) then
        n = n + 1
      end
    end
  end
  return n
end

function Board:next_state(r, c)
  local n = self:neighbors(r, c)
  return n == 3 or (n == 2 and self.cells[r + 1][c + 1])
end

function Board:step()
  local cells = {}
  for r = 0, self.height - 1 do
    local row = {}
    for c = 0, self.width - 1 do
      row[c + 1] = self:next_state(r, c)
    end
    cells[r + 1] = row
  end
  self.cells = cells
end

function Board:population()
  local n = 0
  for _, row in ipairs(self.cells) do
    for _, alive in ipairs(row) do
      if alive then n = n + 1 end
    end
  end
  return n
end

function Board:render()
  local lines = {}
  for r, row in ipairs(self.cells) do
    local chars = {}
    for c, alive in ipairs(row) do
      chars[c] = alive and "#" or "."
    end
    lines[r] = table.concat(chars)
  end
  return table.concat(lines, "\n")
end

local GLIDER = [[
    .#......
    ..#.....
    ###.....
    ........
    ........
    ........
    ]]

local generations = arg[1] and arg[1]:find("^%s*[+-]?%d+%s*$") and math.tointeger(tonumber(arg[1])) or 4
local board = Board.parse(GLIDER)
for _ = 1, generations do
  board:step()
end
print(string.format("generation %d, population %d", generations, board:population()))
print(board:render())
