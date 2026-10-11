# Benchmarks Game spectral-norm: power method on an infinite matrix A.
# Stresses small function calls, float arithmetic and list indexing.

from math import sqrt

N = 800


def eval_a(i: int, j: int) -> float:
    return 1.0 / ((i + j) * (i + j + 1) // 2 + i + 1)


def mul_av(v: list[float], av: list[float]) -> None:
    n = len(v)
    for i in range(n):
        total = 0.0
        for j in range(n):
            total += eval_a(i, j) * v[j]
        av[i] = total


def mul_atv(v: list[float], atv: list[float]) -> None:
    n = len(v)
    for i in range(n):
        total = 0.0
        for j in range(n):
            total += eval_a(j, i) * v[j]
        atv[i] = total


def mul_atav(v: list[float], out: list[float], tmp: list[float]) -> None:
    mul_av(v, tmp)
    mul_atv(tmp, out)


u = [1.0] * N
v = [0.0] * N
tmp = [0.0] * N
for _ in range(10):
    mul_atav(u, v, tmp)
    mul_atav(v, u, tmp)

vbv = 0.0
vv = 0.0
for i in range(N):
    vbv += u[i] * v[i]
    vv += v[i] * v[i]
print(f"{sqrt(vbv / vv):.9f}")
