def solve(points):
    """Return a visiting order for a closed tour over `points` (list of (x, y) tuples).

    Must return a permutation of range(len(points)). Shorter tours score higher.
    """
    return list(range(len(points)))
