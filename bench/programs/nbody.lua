-- Benchmarks Game n-body: simulate the four Jovian planets orbiting the sun.
-- Stresses float arithmetic, table field access and nested loops.

local STEPS = 500000
local PI = 3.141592653589793
local SOLAR_MASS = 4.0 * PI * PI
local DAYS_PER_YEAR = 365.24
local sqrt = math.sqrt

local function body(x, y, z, vx, vy, vz, mass)
  return {
    x = x, y = y, z = z,
    vx = vx * DAYS_PER_YEAR, vy = vy * DAYS_PER_YEAR, vz = vz * DAYS_PER_YEAR,
    mass = mass * SOLAR_MASS,
  }
end

local function offset_momentum(bodies)
  local px, py, pz = 0.0, 0.0, 0.0
  for _, b in ipairs(bodies) do
    px = px + b.vx * b.mass
    py = py + b.vy * b.mass
    pz = pz + b.vz * b.mass
  end
  local sun = bodies[1]
  sun.vx = -px / SOLAR_MASS
  sun.vy = -py / SOLAR_MASS
  sun.vz = -pz / SOLAR_MASS
end

local function energy(bodies)
  local e = 0.0
  local n = #bodies
  for i = 1, n do
    local b = bodies[i]
    e = e + 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz)
    for j = i + 1, n do
      local b2 = bodies[j]
      local dx = b.x - b2.x
      local dy = b.y - b2.y
      local dz = b.z - b2.z
      e = e - b.mass * b2.mass / sqrt(dx * dx + dy * dy + dz * dz)
    end
  end
  return e
end

local function advance(bodies, dt)
  local n = #bodies
  for i = 1, n do
    local b = bodies[i]
    for j = i + 1, n do
      local b2 = bodies[j]
      local dx = b.x - b2.x
      local dy = b.y - b2.y
      local dz = b.z - b2.z
      local d2 = dx * dx + dy * dy + dz * dz
      local mag = dt / (d2 * sqrt(d2))
      local bm = b.mass * mag
      local b2m = b2.mass * mag
      b.vx = b.vx - dx * b2m
      b.vy = b.vy - dy * b2m
      b.vz = b.vz - dz * b2m
      b2.vx = b2.vx + dx * bm
      b2.vy = b2.vy + dy * bm
      b2.vz = b2.vz + dz * bm
    end
  end
  for i = 1, n do
    local b = bodies[i]
    b.x = b.x + dt * b.vx
    b.y = b.y + dt * b.vy
    b.z = b.z + dt * b.vz
  end
end

local bodies = {
  -- sun
  body(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
  -- jupiter
  body(4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01,
       1.66007664274403694e-03, 7.69901118419740425e-03, -6.90460016972063023e-05,
       9.54791938424326609e-04),
  -- saturn
  body(8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01,
       -2.76742510726862411e-03, 4.99852801234917238e-03, 2.30417297573763929e-05,
       2.85885980666130812e-04),
  -- uranus
  body(1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01,
       2.96460137564761618e-03, 2.37847173959480950e-03, -2.96589568540237556e-05,
       4.36624404335156298e-05),
  -- neptune
  body(1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01,
       2.68067772490389322e-03, 1.62824170038242295e-03, -9.51592254519715870e-05,
       5.15138902046611451e-05),
}

offset_momentum(bodies)
print(string.format("%.9f", energy(bodies)))
for _ = 1, STEPS do
  advance(bodies, 0.01)
end
print(string.format("%.9f", energy(bodies)))
