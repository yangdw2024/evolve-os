# toy-bench

Deterministic local bench for the end-to-end evolve loop (Python 3 stdlib only).

- `seed/solution.py` — evolvable candidate exposing `classify(n) -> bool`.
- `train_verifier.py` — n ∈ [1, 200]; emits `accuracy`, `latency_ms`, `loc`, `cell_key` and diagnostic feedback.
- `holdout_verifier.py` — disjoint n ∈ [1000, 1400]; no feedback; pass threshold 0.75.
- Hidden rule: `(n % 3 == 0) != (n % 5 == 0)`.
- `primary_score = accuracy - 0.001 * loc` (deterministic; `latency_ms` is reported but not scored).

The verifier executes the candidate in a child process (cwd = read-only artifact dir,
stripped env, timeout = `EVOLVE_WALL_SECS - 1`); on timeout it prints `{"timeout": true}`,
which the node maps to `ContractError::PolicyViolation`.

Run manually:

```bash
EVOLVE_ARTIFACT_DIR=examples/toy-bench/seed python -I examples/toy-bench/train_verifier.py
```
