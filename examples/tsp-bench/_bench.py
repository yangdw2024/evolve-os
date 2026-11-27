"""Deterministic TSP heuristic bench.

The candidate `solution.py` exposes `solve(points) -> list[int]`, a visiting order
(permutation of range(len(points))) of a closed tour. Instances are generated from
fixed seeds inside this verifier process; the candidate runs in a child process whose
cwd is the read-only artifact dir.

Score per instance = nearest_neighbour_length / candidate_length (higher is better,
1.0 == greedy nearest-neighbour baseline). primary_score = mean ratio - 0.0002 * loc.
"""
import json
import math
import os
import random
import subprocess
import sys
import time

RUNNER = r"""
import json, sys
sys.dont_write_bytecode = True
sys.path.insert(0, ".")
from solution import solve
instances = json.loads(sys.stdin.read())
print(json.dumps([list(solve([tuple(p) for p in pts])) for pts in instances]))
"""

LOC_PENALTY = 0.0002


def make_instance(seed):
    rng = random.Random(seed)
    n = rng.randint(25, 60)
    return [(round(rng.uniform(0, 1000), 2), round(rng.uniform(0, 1000), 2)) for _ in range(n)]


def tour_length(points, order):
    return sum(math.dist(points[order[i]], points[order[(i + 1) % len(order)]]) for i in range(len(order)))


def nearest_neighbour(points):
    left = set(range(1, len(points)))
    order = [0]
    while left:
        last = points[order[-1]]
        nxt = min(left, key=lambda j: (math.dist(last, points[j]), j))
        order.append(nxt)
        left.remove(nxt)
    return order


def count_loc(src):
    return sum(1 for line in src.splitlines() if line.strip() and not line.strip().startswith("#"))


def emit(obj):
    print(json.dumps(obj, sort_keys=True))
    sys.stdout.flush()
    sys.exit(0)


def fail(reason, cell, with_feedback):
    emit({"passed": False, "primary_score": 0.0, "metrics": {}, "cell_key": cell,
          "feedback": reason if with_feedback else None})


def evaluate(seeds, with_feedback, pass_ratio):
    art = os.environ.get("EVOLVE_ARTIFACT_DIR")
    if not art:
        fail("EVOLVE_ARTIFACT_DIR not set", "verifier_error", with_feedback)
    wall = float(os.environ.get("EVOLVE_WALL_SECS", "10"))
    cand_timeout = max(1.0, wall - 1.5)
    path = os.path.join(art, "solution.py")
    if not os.path.isfile(path):
        fail("solution.py missing from artifact", "crash", with_feedback)
    with open(path, encoding="utf-8") as fh:
        loc = count_loc(fh.read())

    instances = [make_instance(s) for s in seeds]
    env = {k: os.environ[k] for k in ("PATH", "SYSTEMROOT", "TEMP", "TMP", "HOME") if k in os.environ}
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    t0 = time.perf_counter()
    try:
        proc = subprocess.run([sys.executable, "-I", "-c", RUNNER], cwd=art, input=json.dumps(instances),
                              capture_output=True, text=True, timeout=cand_timeout, env=env)
    except subprocess.TimeoutExpired:
        emit({"passed": False, "primary_score": 0.0, "timeout": True})
    latency_ms = (time.perf_counter() - t0) * 1000.0
    if proc.returncode != 0:
        fail("candidate crashed:\n" + proc.stderr[-800:], "crash", with_feedback)
    try:
        orders = json.loads(proc.stdout.strip().splitlines()[-1])
        assert isinstance(orders, list) and len(orders) == len(instances)
    except Exception:  # noqa: BLE001
        fail("candidate produced malformed output", "crash", with_feedback)

    ratios = []
    for idx, (pts, order) in enumerate(zip(instances, orders)):
        if sorted(order) != list(range(len(pts))):
            fail("instance %d: order is not a permutation of range(%d)" % (idx, len(pts)), "invalid", with_feedback)
        ratios.append(tour_length(pts, nearest_neighbour(pts)) / max(tour_length(pts, order), 1e-9))
    mean_ratio = sum(ratios) / len(ratios)
    score = round(mean_ratio - LOC_PENALTY * loc, 6)
    cell = "ratio_bin:%d|loc_bin:%d" % (min(int(mean_ratio * 10), 15), min(loc // 10, 5))
    feedback = None
    if with_feedback:
        worst = sorted(range(len(ratios)), key=lambda i: ratios[i])[:3]
        feedback = "mean_ratio_vs_nearest_neighbour=%.4f (1.0 = greedy NN baseline) loc=%d; worst instances: %s" % (
            mean_ratio, loc, ", ".join("#%d n=%d ratio=%.3f" % (i, len(instances[i]), ratios[i]) for i in worst))
    emit({
        "passed": mean_ratio >= pass_ratio,
        "primary_score": score,
        "metrics": {"mean_ratio": round(mean_ratio, 6), "min_ratio": round(min(ratios), 6),
                    "latency_ms": round(latency_ms, 3), "loc": float(loc)},
        "cell_key": cell,
        "feedback": feedback,
    })
