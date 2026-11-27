"""Shared deterministic scoring logic for the toy bench.

Hidden target rule: n is "special" iff divisible by 3 XOR divisible by 5.
The candidate (`solution.py`, exposing `classify(n) -> bool`) is executed in a
separate subprocess whose cwd is the read-only artifact dir. Expected labels are
computed only inside this verifier process and never handed to the candidate.

Stdout contract (last line, JSON):
  {"passed", "primary_score", "metrics", "cell_key", "feedback"} or {"timeout": true}
"""
import json
import os
import subprocess
import sys
import time

RUNNER = r"""
import json, sys
sys.dont_write_bytecode = True
sys.path.insert(0, ".")
from solution import classify
nums = json.loads(sys.stdin.read())
print(json.dumps([bool(classify(n)) for n in nums]))
"""

LOC_PENALTY = 0.001


def target(n):
    return (n % 3 == 0) != (n % 5 == 0)


def count_loc(src):
    return sum(1 for line in src.splitlines() if line.strip() and not line.strip().startswith("#"))


def emit(obj):
    print(json.dumps(obj, sort_keys=True))
    sys.stdout.flush()
    sys.exit(0)


def fail(reason, cell, feedback_enabled):
    emit({
        "passed": False,
        "primary_score": 0.0,
        "metrics": {},
        "cell_key": cell,
        "feedback": reason if feedback_enabled else None,
    })


def evaluate(numbers, with_feedback, pass_threshold):
    art = os.environ.get("EVOLVE_ARTIFACT_DIR")
    if not art:
        fail("EVOLVE_ARTIFACT_DIR not set", "verifier_error", with_feedback)
    wall = float(os.environ.get("EVOLVE_WALL_SECS", "5"))
    cand_timeout = max(1.0, wall - 1.0)

    path = os.path.join(art, "solution.py")
    if not os.path.isfile(path):
        fail("solution.py missing from artifact", "crash", with_feedback)
    with open(path, encoding="utf-8") as fh:
        loc = count_loc(fh.read())

    env = {k: os.environ[k] for k in ("PATH", "SYSTEMROOT", "TEMP", "TMP", "HOME") if k in os.environ}
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    t0 = time.perf_counter()
    try:
        proc = subprocess.run(
            [sys.executable, "-I", "-c", RUNNER],
            cwd=art,
            input=json.dumps(numbers),
            capture_output=True,
            text=True,
            timeout=cand_timeout,
            env=env,
        )
    except subprocess.TimeoutExpired:
        emit({"passed": False, "primary_score": 0.0, "timeout": True})
    latency_ms = (time.perf_counter() - t0) * 1000.0

    if proc.returncode != 0:
        fail("candidate crashed:\n" + proc.stderr[-800:], "crash", with_feedback)
    try:
        preds = json.loads(proc.stdout.strip().splitlines()[-1])
        assert isinstance(preds, list) and len(preds) == len(numbers)
    except Exception:  # noqa: BLE001
        fail("candidate produced malformed output", "crash", with_feedback)

    misses = [n for n, p in zip(numbers, preds) if bool(p) != target(n)]
    acc = 1.0 - len(misses) / len(numbers)
    score = round(acc - LOC_PENALTY * loc, 6)
    cell = "acc_bin:%d|loc_bin:%d" % (min(int(acc * 10), 9), min(loc // 3, 5))
    feedback = None
    if with_feedback:
        feedback = "accuracy=%.4f loc=%d; misclassified (n, expected): %s" % (
            acc, loc, [(n, target(n)) for n in misses[:5]])
    emit({
        "passed": acc >= pass_threshold,
        "primary_score": score,
        "metrics": {"accuracy": round(acc, 6), "latency_ms": round(latency_ms, 3), "loc": float(loc)},
        "cell_key": cell,
        "feedback": feedback,
    })
