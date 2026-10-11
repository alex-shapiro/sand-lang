-- Ray: a recursive ray tracer (spheres on a checkered plane, two point lights, Phong shading,
-- hard shadows, reflections) that prints a pixel checksum and an ASCII rendering.
-- Stresses small table allocation (Vec3), float math, nil results, method dispatch.

local WIDTH = 320
local HEIGHT = 160
local ART_WIDTH = 40
local ART_HEIGHT = 20
local MAX_DEPTH = 3
local EPSILON = 0.0001
local AMBIENT = 0.1
local RAMP = " .:-=+*#%@"

local floor, sqrt, abs, min, max = math.floor, math.sqrt, math.abs, math.min, math.max

local Vec3 = {}
Vec3.__index = Vec3

local function vec(x, y, z)
  return setmetatable({ x = x, y = y, z = z }, Vec3)
end

function Vec3:add(o) return vec(self.x + o.x, self.y + o.y, self.z + o.z) end
function Vec3:sub(o) return vec(self.x - o.x, self.y - o.y, self.z - o.z) end
function Vec3:mul(o) return vec(self.x * o.x, self.y * o.y, self.z * o.z) end
function Vec3:scale(k) return vec(self.x * k, self.y * k, self.z * k) end
function Vec3:dot(o) return self.x * o.x + self.y * o.y + self.z * o.z end

function Vec3:cross(o)
  return vec(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
end

function Vec3:len() return sqrt(self:dot(self)) end
function Vec3:norm() return self:scale(1.0 / self:len()) end

local function ray_at(ray, t) return ray.origin:add(ray.dir:scale(t)) end

local Solid = {}
Solid.__index = Solid
function Solid.new(c) return setmetatable({ c = c }, Solid) end
function Solid:color_at(p) return self.c end

local Checker = {}
Checker.__index = Checker
function Checker.new(a, b) return setmetatable({ a = a, b = b }, Checker) end

function Checker:color_at(p)
  if (floor(p.x) + floor(p.z)) % 2 == 0 then return self.a else return self.b end
end

local function material(m)
  return {
    pattern = m.pattern,
    diffuse = m.diffuse or 0.9,
    specular = m.specular or 0.5,
    shininess = m.shininess or 32.0,
    reflect = m.reflect or 0.0,
  }
end

local Sphere = {}
Sphere.__index = Sphere
function Sphere.new(center, radius, mat)
  return setmetatable({ center = center, radius = radius, material = mat }, Sphere)
end

function Sphere:intersect(ray)
  local oc = ray.origin:sub(self.center)
  local b = oc:dot(ray.dir)
  local disc = b * b - (oc:dot(oc) - self.radius * self.radius)
  if disc < 0.0 then return nil end
  local sq = sqrt(disc)
  local t = -b - sq
  if t < EPSILON then t = -b + sq end
  if t < EPSILON then return nil end
  local point = ray_at(ray, t)
  return { t = t, point = point, normal = point:sub(self.center):scale(1.0 / self.radius), material = self.material }
end

local Plane = {}
Plane.__index = Plane
function Plane.new(normal, offset, mat)
  return setmetatable({ normal = normal, offset = offset, material = mat }, Plane)
end

function Plane:intersect(ray)
  local denom = self.normal:dot(ray.dir)
  if abs(denom) < 0.000000001 then return nil end
  local t = (self.offset - self.normal:dot(ray.origin)) / denom
  if t < EPSILON then return nil end
  return { t = t, point = ray_at(ray, t), normal = self.normal, material = self.material }
end

local Scene = {}
Scene.__index = Scene
function Scene.new(objects, lights)
  return setmetatable({ objects = objects, lights = lights }, Scene)
end

function Scene:closest(ray)
  local best = nil
  for _, o in ipairs(self.objects) do
    local h = o:intersect(ray)
    if h and (best == nil or h.t < best.t) then best = h end
  end
  return best
end

function Scene:occluded(ray, max_t)
  for _, o in ipairs(self.objects) do
    local h = o:intersect(ray)
    if h and h.t < max_t then return true end
  end
  return false
end

function Scene:trace(ray, depth)
  local hit = self:closest(ray)
  if hit == nil then
    local t = 0.5 * (ray.dir.y + 1.0)
    return vec(0.3, 0.35, 0.45):scale(1.0 - t):add(vec(0.05, 0.1, 0.3):scale(t))
  end
  local m = hit.material
  local base = m.pattern:color_at(hit.point)
  local over = hit.point:add(hit.normal:scale(EPSILON))
  local color = base:scale(AMBIENT)
  for _, light in ipairs(self.lights) do
    local to_light = light.pos:sub(hit.point)
    local dist = to_light:len()
    local l = to_light:scale(1.0 / dist)
    local ndotl = hit.normal:dot(l)
    if ndotl > 0.0 and not self:occluded({ origin = over, dir = l }, dist) then
      color = color:add(base:mul(light.color):scale(ndotl * m.diffuse))
      local rv = -hit.normal:scale(2.0 * ndotl):sub(l):dot(ray.dir)
      if rv > 0.0 then
        color = color:add(light.color:scale(m.specular * rv ^ m.shininess))
      end
    end
  end
  if m.reflect > 0.0 and depth < MAX_DEPTH then
    local dir = ray.dir:sub(hit.normal:scale(2.0 * ray.dir:dot(hit.normal)))
    local reflected = self:trace({ origin = over, dir = dir }, depth + 1)
    color = color:scale(1.0 - m.reflect):add(reflected:scale(m.reflect))
  end
  return color
end

local function sphere(x, y, z, radius, mat) return Sphere.new(vec(x, y, z), radius, mat) end

local function solid(r, g, b, reflect)
  return material({ pattern = Solid.new(vec(r, g, b)), reflect = reflect })
end

local function quantize(c) return floor(max(0.0, min(1.0, c)) * 255.0) end

local scene = Scene.new({
  Plane.new(vec(0.0, 1.0, 0.0), 0.0, material({
    pattern = Checker.new(vec(0.9, 0.9, 0.9), vec(0.15, 0.15, 0.15)),
    specular = 0.0,
    reflect = 0.25,
  })),
  sphere(0.0, 1.0, 0.0, 1.0, solid(0.9, 0.2, 0.2, 0.3)),
  sphere(-2.2, 0.7, 0.8, 0.7, solid(0.2, 0.8, 0.3)),
  sphere(2.1, 0.8, 0.5, 0.8, solid(0.2, 0.3, 0.9, 0.5)),
  sphere(-0.9, 0.35, -1.6, 0.35, solid(0.95, 0.85, 0.2)),
  sphere(1.0, 0.3, -1.8, 0.3, solid(0.8, 0.3, 0.9, 0.2)),
  sphere(0.3, 2.6, 1.5, 0.6, solid(0.9, 0.9, 0.9, 0.8)),
  sphere(-3.5, 1.5, 3.5, 1.5, solid(0.3, 0.9, 0.9, 0.1)),
  sphere(3.8, 1.2, 3.0, 1.2, solid(0.95, 0.5, 0.1)),
}, {
  { pos = vec(-5.0, 8.0, -6.0), color = vec(0.8, 0.8, 0.8) },
  { pos = vec(6.0, 6.0, -3.0), color = vec(0.5, 0.45, 0.4) },
})

local eye = vec(0.0, 2.2, -6.5)
local forward = vec(0.0, 1.0, 0.0):sub(eye):norm()
local right = vec(0.0, 1.0, 0.0):cross(forward):norm()
local up = forward:cross(right)
local aspect = WIDTH / HEIGHT

local checksum = 0
local lum = {}
for py = 0, HEIGHT - 1 do
  for px = 0, WIDTH - 1 do
    local u = (2.0 * (px + 0.5) / WIDTH - 1.0) * aspect * 0.4
    local v = (1.0 - 2.0 * (py + 0.5) / HEIGHT) * 0.4
    local dir = forward:add(right:scale(u)):add(up:scale(v)):norm()
    local c = scene:trace({ origin = eye, dir = dir }, 0)
    local r, g, b = quantize(c.x), quantize(c.y), quantize(c.z)
    checksum = checksum + r + g + b
    lum[py * WIDTH + px] = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
  end
end

local block_w = WIDTH // ART_WIDTH
local block_h = HEIGHT // ART_HEIGHT
for cy = 0, ART_HEIGHT - 1 do
  local line = {}
  for cx = 0, ART_WIDTH - 1 do
    local sum = 0.0
    for y = cy * block_h, (cy + 1) * block_h - 1 do
      for x = cx * block_w, (cx + 1) * block_w - 1 do
        sum = sum + lum[y * WIDTH + x]
      end
    end
    local avg = sum / (block_w * block_h)
    local i = min(floor(avg * 10.0), 9) + 1
    line[#line + 1] = RAMP:sub(i, i)
  end
  print(table.concat(line))
end
print(string.format("size: %dx%d", WIDTH, HEIGHT))
print("checksum: " .. checksum)
