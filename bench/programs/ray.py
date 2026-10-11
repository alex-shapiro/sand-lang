"""Ray: a recursive ray tracer (spheres on a checkered plane, two point lights, Phong shading,
hard shadows, reflections) that prints a pixel checksum and an ASCII rendering.

Stresses small object allocation (Vec3), float math, Optional results, method dispatch.
"""

from __future__ import annotations

import math

WIDTH = 320
HEIGHT = 160
ART_WIDTH = 40
ART_HEIGHT = 20
MAX_DEPTH = 3
EPSILON = 0.0001
AMBIENT = 0.1
RAMP = " .:-=+*#%@"


class Vec3:
    __slots__ = ("x", "y", "z")

    def __init__(self, x: float, y: float, z: float):
        self.x = x
        self.y = y
        self.z = z

    def __add__(self, o: Vec3) -> Vec3:
        return Vec3(self.x + o.x, self.y + o.y, self.z + o.z)

    def __sub__(self, o: Vec3) -> Vec3:
        return Vec3(self.x - o.x, self.y - o.y, self.z - o.z)

    def __mul__(self, k: float) -> Vec3:
        return Vec3(self.x * k, self.y * k, self.z * k)

    def mul(self, o: Vec3) -> Vec3:
        return Vec3(self.x * o.x, self.y * o.y, self.z * o.z)

    def dot(self, o: Vec3) -> float:
        return self.x * o.x + self.y * o.y + self.z * o.z

    def cross(self, o: Vec3) -> Vec3:
        return Vec3(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)

    def len(self) -> float:
        return math.sqrt(self.dot(self))

    def norm(self) -> Vec3:
        return self * (1.0 / self.len())


class Ray:
    __slots__ = ("origin", "dir")

    def __init__(self, origin: Vec3, dir: Vec3):
        self.origin = origin
        self.dir = dir

    def at(self, t: float) -> Vec3:
        return self.origin + self.dir * t


class Solid:
    __slots__ = ("c",)

    def __init__(self, c: Vec3):
        self.c = c

    def color_at(self, p: Vec3) -> Vec3:
        return self.c


class Checker:
    __slots__ = ("a", "b")

    def __init__(self, a: Vec3, b: Vec3):
        self.a = a
        self.b = b

    def color_at(self, p: Vec3) -> Vec3:
        return self.a if (math.floor(p.x) + math.floor(p.z)) % 2 == 0 else self.b


class Material:
    __slots__ = ("pattern", "diffuse", "specular", "shininess", "reflect")

    def __init__(self, pattern, diffuse=0.9, specular=0.5, shininess=32.0, reflect=0.0):
        self.pattern = pattern
        self.diffuse = diffuse
        self.specular = specular
        self.shininess = shininess
        self.reflect = reflect


class Hit:
    __slots__ = ("t", "point", "normal", "material")

    def __init__(self, t: float, point: Vec3, normal: Vec3, material: Material):
        self.t = t
        self.point = point
        self.normal = normal
        self.material = material


class Sphere:
    __slots__ = ("center", "radius", "material")

    def __init__(self, center: Vec3, radius: float, material: Material):
        self.center = center
        self.radius = radius
        self.material = material

    def intersect(self, ray: Ray) -> Hit | None:
        oc = ray.origin - self.center
        b = oc.dot(ray.dir)
        disc = b * b - (oc.dot(oc) - self.radius * self.radius)
        if disc < 0.0:
            return None
        sq = math.sqrt(disc)
        t = -b - sq
        if t < EPSILON:
            t = -b + sq
        if t < EPSILON:
            return None
        point = ray.at(t)
        return Hit(t, point, (point - self.center) * (1.0 / self.radius), self.material)


class Plane:
    __slots__ = ("normal", "offset", "material")

    def __init__(self, normal: Vec3, offset: float, material: Material):
        self.normal = normal
        self.offset = offset
        self.material = material

    def intersect(self, ray: Ray) -> Hit | None:
        denom = self.normal.dot(ray.dir)
        if abs(denom) < 0.000000001:
            return None
        t = (self.offset - self.normal.dot(ray.origin)) / denom
        if t < EPSILON:
            return None
        return Hit(t, ray.at(t), self.normal, self.material)


class Light:
    __slots__ = ("pos", "color")

    def __init__(self, pos: Vec3, color: Vec3):
        self.pos = pos
        self.color = color


class Scene:
    __slots__ = ("objects", "lights")

    def __init__(self, objects: list, lights: list[Light]):
        self.objects = objects
        self.lights = lights

    def closest(self, ray: Ray) -> Hit | None:
        hits = [h for o in self.objects if (h := o.intersect(ray)) is not None]
        return min(hits, key=lambda h: h.t, default=None)

    def occluded(self, ray: Ray, max_t: float) -> bool:
        return any((h := o.intersect(ray)) is not None and h.t < max_t for o in self.objects)

    def trace(self, ray: Ray, depth: int) -> Vec3:
        hit = self.closest(ray)
        if hit is None:
            t = 0.5 * (ray.dir.y + 1.0)
            return Vec3(0.3, 0.35, 0.45) * (1.0 - t) + Vec3(0.05, 0.1, 0.3) * t
        m = hit.material
        base = m.pattern.color_at(hit.point)
        over = hit.point + hit.normal * EPSILON
        color = base * AMBIENT
        for light in self.lights:
            to_light = light.pos - hit.point
            dist = to_light.len()
            l = to_light * (1.0 / dist)
            ndotl = hit.normal.dot(l)
            if ndotl <= 0.0 or self.occluded(Ray(over, l), dist):
                continue
            color = color + base.mul(light.color) * (ndotl * m.diffuse)
            rv = -(hit.normal * (2.0 * ndotl) - l).dot(ray.dir)
            if rv > 0.0:
                color = color + light.color * (m.specular * rv**m.shininess)
        if m.reflect > 0.0 and depth < MAX_DEPTH:
            dir = ray.dir - hit.normal * (2.0 * ray.dir.dot(hit.normal))
            reflected = self.trace(Ray(over, dir), depth + 1)
            color = color * (1.0 - m.reflect) + reflected * m.reflect
        return color


def sphere(x: float, y: float, z: float, radius: float, material: Material) -> Sphere:
    return Sphere(Vec3(x, y, z), radius, material)


def solid(r: float, g: float, b: float, reflect: float = 0.0) -> Material:
    return Material(Solid(Vec3(r, g, b)), reflect=reflect)


def quantize(c: float) -> int:
    return math.floor(min(max(c, 0.0), 1.0) * 255.0)


scene = Scene(
    [
        Plane(
            Vec3(0.0, 1.0, 0.0),
            0.0,
            Material(Checker(Vec3(0.9, 0.9, 0.9), Vec3(0.15, 0.15, 0.15)), specular=0.0, reflect=0.25),
        ),
        sphere(0.0, 1.0, 0.0, 1.0, solid(0.9, 0.2, 0.2, reflect=0.3)),
        sphere(-2.2, 0.7, 0.8, 0.7, solid(0.2, 0.8, 0.3)),
        sphere(2.1, 0.8, 0.5, 0.8, solid(0.2, 0.3, 0.9, reflect=0.5)),
        sphere(-0.9, 0.35, -1.6, 0.35, solid(0.95, 0.85, 0.2)),
        sphere(1.0, 0.3, -1.8, 0.3, solid(0.8, 0.3, 0.9, reflect=0.2)),
        sphere(0.3, 2.6, 1.5, 0.6, solid(0.9, 0.9, 0.9, reflect=0.8)),
        sphere(-3.5, 1.5, 3.5, 1.5, solid(0.3, 0.9, 0.9, reflect=0.1)),
        sphere(3.8, 1.2, 3.0, 1.2, solid(0.95, 0.5, 0.1)),
    ],
    [
        Light(Vec3(-5.0, 8.0, -6.0), Vec3(0.8, 0.8, 0.8)),
        Light(Vec3(6.0, 6.0, -3.0), Vec3(0.5, 0.45, 0.4)),
    ],
)

eye = Vec3(0.0, 2.2, -6.5)
forward = (Vec3(0.0, 1.0, 0.0) - eye).norm()
right = Vec3(0.0, 1.0, 0.0).cross(forward).norm()
up = forward.cross(right)
aspect = WIDTH / HEIGHT

checksum = 0
lum = [0.0] * (WIDTH * HEIGHT)
for py in range(HEIGHT):
    for px in range(WIDTH):
        u = (2.0 * (px + 0.5) / WIDTH - 1.0) * aspect * 0.4
        v = (1.0 - 2.0 * (py + 0.5) / HEIGHT) * 0.4
        dir = (forward + right * u + up * v).norm()
        c = scene.trace(Ray(eye, dir), 0)
        r, g, b = quantize(c.x), quantize(c.y), quantize(c.z)
        checksum += r + g + b
        lum[py * WIDTH + px] = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0

block_w = WIDTH // ART_WIDTH
block_h = HEIGHT // ART_HEIGHT
for cy in range(ART_HEIGHT):
    line = []
    for cx in range(ART_WIDTH):
        total = 0.0
        for y in range(cy * block_h, (cy + 1) * block_h):
            for x in range(cx * block_w, (cx + 1) * block_w):
                total += lum[y * WIDTH + x]
        avg = total / (block_w * block_h)
        line.append(RAMP[min(math.floor(avg * 10.0), 9)])
    print("".join(line))
print(f"size: {WIDTH}x{HEIGHT}")
print(f"checksum: {checksum}")
