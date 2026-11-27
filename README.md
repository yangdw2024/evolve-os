# evolve-os

A local, contract-first **self-evolving code loop** in Rust (AlphaEvolve-style):
multiple islands of candidates evolve through LLM-proposed `SEARCH/REPLACE` patches,
are scored by external deterministic verifiers in a two-stage sandbox, kept in a
MAP-Elites archive, and promoted only after passing a hidden **holdout** gate.

```
sample Parent + Inspirations ─▶ LLM (Fast / Strong, budget-guarded) ─▶ SEARCH/REPLACE patch
        ▲                                                                    │
        │                                                                    ▼
 MAP-Elites grid ◀── train Mark ◀── verifier (read-only artifact) ◀── sandbox build (time-limited)
        │
        └─ new global best ─▶ holdout verifier (no feedback) ─▶ accept champion | rollback + blacklist
```

## Highlights

- **Hard budget guard** – every model call is `reserve → complete → settle` (or `release` on failure)
  against an atomic SQLite ledger; concurrent islands cannot overspend. Integer micro-USD only.
- **Holdout zero-leak** – holdout marks never carry feedback, never enter the MAP-Elites grid or prompts
  (enforced in the node, the loop and by a DB `CHECK` constraint).
- **Two-stage sandbox** – patch + build in a fresh temp dir with a hard wall-clock limit (process-tree kill);
  verifiers read a read-only artifact copy from an isolated cwd with a scrubbed environment.
- **Typed recovery** – `PatchInvalid` → one format-repair retry; `PolicyViolation` → refusal ledger;
  `NodeTransient` → release + retry without penalty; `BudgetExceeded` → graceful stop of all islands.
- **OpenAI-compatible** – any `base_url`/model, Fast/Strong tiers, model catalog, reasoning levels,
  conservative billing when a server omits `usage`.
- **Auditable** – `evolve audit` cross-checks ledger events, reservations, marks and the grid, and prints
  the score curve and patch-repair statistics.

## Install

| Channel | Command |
|---|---|
| Prebuilt binaries | [GitHub Releases](https://github.com/yangdw2024/evolve-os/releases) |
| Cargo | `cargo install evolve-os` |
| PyPI (ships the `evolve` binary) | `pip install evolve-os` |
| npm (downloads the release binary) | `npx evolve-os --help` |

Benches run their verifiers with Python 3 (standard library only); set `EVOLVE_PYTHON` if `python` is not on `PATH`.

## Quick start

```bash
git clone https://github.com/yangdw2024/evolve-os && cd evolve-os

# Offline end-to-end smoke test (deterministic mock LLM, still goes through the budget ledger)
cargo run -p evolve-os --bin evolve -- run --mock-llm --islands 2 --iterations 10

# Real OpenAI-compatible model (see llm.toml; the key is read from an env var, never from the file)
export EVOLVE_API_KEY=...            # PowerShell: $env:EVOLVE_API_KEY = "..."
cargo run -p evolve-os --bin evolve -- run --llm-config llm.toml \
    --bench-dir examples/tsp-bench --islands 2 --iterations 5 --wall-secs 10 --run-id tsp-1

# Audit the ledger, budget settlement, repair rate and score curve
cargo run -p evolve-os --bin evolve -- audit --run-id tsp-1 --max-budget-usd 0.50
```

The default hard budget is **$0.50** per run; going above it requires `--allow-budget-above-cap`.

## Benches

A bench is a directory with `seed/` (evolvable files), `train_verifier.py`, `holdout_verifier.py`
and an optional `TASK.md` (task + scoring rules, injected into the system prompt).
Verifiers read `EVOLVE_ARTIFACT_DIR` and print one JSON line:
`{"passed", "primary_score", "metrics", "cell_key", "feedback"}` (or `{"timeout": true}`).

- `examples/toy-bench` – tiny integer classifier (sanity check; saturates quickly).
- `examples/tsp-bench` – Euclidean TSP heuristic scored against greedy nearest neighbour; open-ended.

## Workspace

| Crate (crates.io name) | Purpose |
|---|---|
| `evolveos-core-contracts` | frozen errors, types and traits (`ExecutionNode`, `EvolutionArchive`, `BudgetLedger`, `LlmClient`) |
| `evolveos-ledger-sqlite` | SQLite WAL ledger, blobs, lineage, budget reservations, MAP-Elites islands, audit |
| `evolveos-llm-router` | `BudgetedLlmRouter` + OpenAI-compatible client, prompt dedup |
| `evolveos-node-runtime` | `LocalSandboxNode`, SEARCH/REPLACE patch protocol |
| `evolve-os` | multi-island engine, mock mutator, `evolve` CLI |

## Security notes

The sandbox is process-level (temp dirs, scrubbed env, read-only artifacts, wall-clock kill), **not**
a kernel/container boundary; network denial is advisory. Run untrusted model output on machines you
are comfortable with, or wrap the node in a container.

## License

MIT
