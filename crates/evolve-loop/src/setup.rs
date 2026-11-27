//! Wiring helpers shared by the CLI and integration tests.

use crate::engine::EngineConfig;
use core_contracts::{ContractError, MicroUsd, PolicyGrant, VerifierSpec};
use node_runtime::{LocalSandboxConfig, LocalSandboxNode};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Hard safety cap for a run unless explicitly overridden: $0.50.
pub const DEFAULT_BUDGET_CAP_MICRO_USD: MicroUsd = 500_000;

/// A local bench directory:
/// `seed/` (evolvable files), `train_verifier.py`, `holdout_verifier.py`, optional `TASK.md`.
pub struct Bench {
    pub seed_files: BTreeMap<String, String>,
    pub train: VerifierSpec,
    pub holdout: VerifierSpec,
    pub task_spec: Option<String>,
}

/// Backwards-compatible alias.
pub type ToyBench = Bench;

fn read_seed(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), ContractError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| ContractError::Internal(format!("read {}: {e}", dir.display())))?;
    for entry in entries {
        let path = entry.map_err(|e| ContractError::Internal(e.to_string()))?.path();
        if path.file_name().is_some_and(|n| n == "__pycache__") {
            continue;
        }
        if path.is_dir() {
            read_seed(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| ContractError::Internal(e.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ContractError::Internal(format!("read {}: {e}", path.display())))?;
            out.insert(rel, text);
        }
    }
    Ok(())
}

pub fn load_bench(bench_dir: &Path, python: &str) -> Result<Bench, ContractError> {
    let bench = std::path::absolute(bench_dir)
        .map_err(|e| ContractError::Internal(format!("resolve bench dir: {e}")))?;
    let seed_dir = bench.join("seed");
    let mut seed_files = BTreeMap::new();
    read_seed(&seed_dir, &seed_dir, &mut seed_files)?;
    if seed_files.is_empty() {
        return Err(ContractError::Internal(format!("{} has no seed files", seed_dir.display())));
    }
    let spec = |id: &str, script: &str, holdout: bool| -> Result<VerifierSpec, ContractError> {
        let path = bench.join(script);
        if !path.is_file() {
            return Err(ContractError::Internal(format!("missing {}", path.display())));
        }
        Ok(VerifierSpec {
            verifier_id: id.into(),
            cmd: vec![python.into(), "-I".into(), path.to_string_lossy().into_owned()],
            is_holdout: holdout,
        })
    };
    let name = bench.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "bench".into());
    Ok(Bench {
        seed_files,
        train: spec(&format!("{name}-train"), "train_verifier.py", false)?,
        holdout: spec(&format!("{name}-holdout"), "holdout_verifier.py", true)?,
        task_spec: std::fs::read_to_string(bench.join("TASK.md")).ok(),
    })
}

/// Backwards-compatible name.
pub fn load_toy_bench(bench_dir: &Path, python: &str) -> Result<Bench, ContractError> {
    load_bench(bench_dir, python)
}

/// The toy bench compiled into the binary (used when installed via pip/npm without `examples/`).
pub const EMBEDDED_TOY_BENCH: &[(&str, &str)] = &[
    ("_bench.py", include_str!("../assets/toy-bench/_bench.py")),
    ("train_verifier.py", include_str!("../assets/toy-bench/train_verifier.py")),
    ("holdout_verifier.py", include_str!("../assets/toy-bench/holdout_verifier.py")),
    ("TASK.md", include_str!("../assets/toy-bench/TASK.md")),
    ("seed/solution.py", include_str!("../assets/toy-bench/seed/solution.py")),
];

/// Writes the embedded toy bench under `dir` (idempotent) and returns its path.
pub fn materialize_embedded_toy_bench(dir: &Path) -> Result<PathBuf, ContractError> {
    let root = dir.join("toy-bench");
    for (rel, content) in EMBEDDED_TOY_BENCH {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ContractError::Internal(format!("create {}: {e}", parent.display())))?;
        }
        std::fs::write(&path, content)
            .map_err(|e| ContractError::Internal(format!("write {}: {e}", path.display())))?;
    }
    Ok(root)
}

/// Sandbox node whose Stage-1 build byte-compiles every Python seed file (syntax gate).
pub fn toy_node(work_dir: &Path, python: &str) -> Result<LocalSandboxNode, ContractError> {
    let mut cfg = LocalSandboxConfig::new("local-sandbox-0", PathBuf::from(work_dir));
    cfg.build_cmd = Some(vec![
        python.into(),
        "-I".into(),
        "-m".into(),
        "compileall".into(),
        "-q".into(),
        ".".into(),
    ]);
    LocalSandboxNode::new(cfg)
}

pub fn engine_config(
    run_id: String,
    islands: u32,
    iterations: u32,
    budget: MicroUsd,
    wall_secs: u64,
    bench: &Bench,
) -> EngineConfig {
    EngineConfig {
        run_id,
        islands,
        iterations,
        inspiration_k: 2,
        stagnation_limit: 3,
        cross_every: 4,
        migrate_every: 5,
        holdout_regression_threshold: 0.05,
        max_transient_retries: 2,
        max_output_tokens: 1024,
        grant: PolicyGrant {
            allowed_paths: bench.seed_files.keys().cloned().collect(),
            allow_network: false,
            max_wall_secs: wall_secs,
            max_cost_micro_usd: budget,
        },
        train_verifier: bench.train.clone(),
        holdout_verifier: bench.holdout.clone(),
        task_spec: bench.task_spec.clone(),
    }
}
