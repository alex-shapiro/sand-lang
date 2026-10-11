# Benchmarks Game fannkuch-redux: count prefix reversals over all permutations.
# Stresses small integer lists: copying, in-place reversal and rotation.

N = 9


def flip_count(perm: list[int]) -> int:
    flips = 0
    k = perm[0]
    while k != 0:
        perm[: k + 1] = perm[k::-1]
        flips += 1
        k = perm[0]
    return flips


def fannkuch(n: int) -> tuple[int, int]:
    """Returns the checksum and the maximum number of flips over all permutations of range(n)."""
    perm = list(range(n))
    count = [0] * n
    max_flips = 0
    checksum = 0
    sign = 1
    r = n
    while True:
        while r != 1:
            count[r - 1] = r
            r -= 1
        flips = flip_count(perm.copy())
        max_flips = max(max_flips, flips)
        checksum += sign * flips
        sign = -sign
        while True:
            if r == n:
                return checksum, max_flips
            perm.insert(r, perm.pop(0))
            count[r] -= 1
            if count[r] > 0:
                break
            r += 1


checksum, max_flips = fannkuch(N)
print(checksum)
print(f"Pfannkuchen({N}) = {max_flips}")
