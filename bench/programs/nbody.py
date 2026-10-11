# Benchmarks Game n-body: simulate the four Jovian planets orbiting the sun.
# Stresses float arithmetic, attribute access and nested loops.

from math import sqrt

STEPS = 500_000
PI = 3.141592653589793
SOLAR_MASS = 4.0 * PI * PI
DAYS_PER_YEAR = 365.24


class Body:
    __slots__ = ("x", "y", "z", "vx", "vy", "vz", "mass")

    def __init__(self, x: float, y: float, z: float, vx: float, vy: float, vz: float, mass: float):
        self.x = x
        self.y = y
        self.z = z
        self.vx = vx * DAYS_PER_YEAR
        self.vy = vy * DAYS_PER_YEAR
        self.vz = vz * DAYS_PER_YEAR
        self.mass = mass * SOLAR_MASS


def offset_momentum(bodies: list[Body]) -> None:
    px = py = pz = 0.0
    for b in bodies:
        px += b.vx * b.mass
        py += b.vy * b.mass
        pz += b.vz * b.mass
    sun = bodies[0]
    sun.vx = -px / SOLAR_MASS
    sun.vy = -py / SOLAR_MASS
    sun.vz = -pz / SOLAR_MASS


def energy(bodies: list[Body]) -> float:
    e = 0.0
    for i, b in enumerate(bodies):
        e += 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz)
        for b2 in bodies[i + 1 :]:
            dx = b.x - b2.x
            dy = b.y - b2.y
            dz = b.z - b2.z
            e -= b.mass * b2.mass / sqrt(dx * dx + dy * dy + dz * dz)
    return e


def advance(bodies: list[Body], dt: float) -> None:
    for i, b in enumerate(bodies):
        for b2 in bodies[i + 1 :]:
            dx = b.x - b2.x
            dy = b.y - b2.y
            dz = b.z - b2.z
            d2 = dx * dx + dy * dy + dz * dz
            mag = dt / (d2 * sqrt(d2))
            bm = b.mass * mag
            b2m = b2.mass * mag
            b.vx -= dx * b2m
            b.vy -= dy * b2m
            b.vz -= dz * b2m
            b2.vx += dx * bm
            b2.vy += dy * bm
            b2.vz += dz * bm
    for b in bodies:
        b.x += dt * b.vx
        b.y += dt * b.vy
        b.z += dt * b.vz


bodies = [
    # sun
    Body(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
    # jupiter
    Body(
        4.84143144246472090e00,
        -1.16032004402742839e00,
        -1.03622044471123109e-01,
        1.66007664274403694e-03,
        7.69901118419740425e-03,
        -6.90460016972063023e-05,
        9.54791938424326609e-04,
    ),
    # saturn
    Body(
        8.34336671824457987e00,
        4.12479856412430479e00,
        -4.03523417114321381e-01,
        -2.76742510726862411e-03,
        4.99852801234917238e-03,
        2.30417297573763929e-05,
        2.85885980666130812e-04,
    ),
    # uranus
    Body(
        1.28943695621391310e01,
        -1.51111514016986312e01,
        -2.23307578892655734e-01,
        2.96460137564761618e-03,
        2.37847173959480950e-03,
        -2.96589568540237556e-05,
        4.36624404335156298e-05,
    ),
    # neptune
    Body(
        1.53796971148509165e01,
        -2.59193146099879641e01,
        1.79258772950371181e-01,
        2.68067772490389322e-03,
        1.62824170038242295e-03,
        -9.51592254519715870e-05,
        5.15138902046611451e-05,
    ),
]

offset_momentum(bodies)
print(f"{energy(bodies):.9f}")
for _ in range(STEPS):
    advance(bodies, 0.01)
print(f"{energy(bodies):.9f}")
