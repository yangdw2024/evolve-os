//! Integration tests against the real toy-bench verifiers (requires Python 3 on PATH,
//! or `EVOLVE_PYTHON` pointing to an interpreter). Missing Python is a hard failure.

use core_contracts::{ContractError, ExecutionNode, PolicyGrant, VerifierSpec};
use node_runtime::{LocalSandboxConfig, LocalSandboxNode};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

fn python() -> String {
    std::env::var("EVOLVE_PYTHON").unwrap_or_else(|_| "python".into())
}

fn bench_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/toy-bench")
}

fn verifier(holdout: bool) -> VerifierSpec {
    let script = if holdout { "holdout_verifier.py" } else { "train_verifier.py" };
    let path = std::path::absolute(bench_dir().join(script)).unwrap();
    VerifierSpec {
        verifier_id: if holdout { "toy-holdout" } else { "toy-train" }.into(),
        cmd: vec![python(), "-I".into(), path.to_string_lossy().into_owned()],
        is_holdout: holdout,
    }
}

fn grant(wall: u64) -> PolicyGrant {
    PolicyGrant {
        allowed_paths: vec!["solution.py".into()],
        allow_network: false,
        max_wall_secs: wall,
        max_cost_micro_usd: 0,
    }
}

fn node(tag: &str) -> (LocalSandboxNode, String) {
    let root = std::env::temp_dir().join(format!("evolve-node-test-{tag}-{}", std::process::id()));
    let mut cfg = LocalSandboxConfig::new(format!("test-{tag}"), &root);
    cfg.build_cmd = Some(vec![python(), "-I".into(), "-m".into(), "py_compile".into(), "solution.py".into()]);
    let node = LocalSandboxNode::new(cfg).unwrap();
    let seed = std::fs::read_to_string(bench_dir().join("seed/solution.py")).unwrap();
    let mut files = BTreeMap::new();
    files.insert("solution.py".to_string(), seed);
    let sha = node.register_snapshot(&files).unwrap();
    (node, sha)
}

fn patch(search: &str, replace: &str) -> String {
    format!("<<<<<<< SEARCH solution.py\n{search}\n=======\n{replace}\n>>>>>>> REPLACE\n")
}

#[tokio::test]
async fn seed_scores_with_metrics_cell_key_and_feedback() {
    let (node, sha) = node("seed");
    let mark = node.eval_candidate("c-seed", &sha, "", &verifier(false), &grant(10)).await.unwrap();
    assert!(mark.passed);
    assert!((mark.metrics["accuracy"] - 0.8).abs() < 1e-9, "{:?}", mark.metrics);
    assert_eq!(mark.metrics["loc"], 2.0);
    assert!(mark.metrics.contains_key("latency_ms"));
    assert!((mark.primary_score - 0.798).abs() < 1e-9);
    assert_eq!(mark.cell_key, "acc_bin:8|loc_bin:0");
    assert!(mark.public_feedback.as_deref().unwrap().contains("misclassified"));
    assert!(!mark.is_holdout);
}

#[tokio::test]
async fn improving_patch_scores_higher_and_holdout_has_no_feedback() {
    let (node, sha) = node("improve");
    let p = patch("    return n % 3 == 0", "    return (n % 3 == 0) != (n % 5 == 0)");
    let train = node.eval_candidate("c-xor", &sha, &p, &verifier(false), &grant(10)).await.unwrap();
    assert!(train.passed);
    assert_eq!(train.metrics["accuracy"], 1.0);
    let hold = node.eval_candidate("c-xor", &sha, &p, &verifier(true), &grant(10)).await.unwrap();
    assert!(hold.passed && hold.is_holdout);
    assert_eq!(hold.public_feedback, None);
    assert_eq!(train.artifact_sha256, hold.artifact_sha256);
}

#[tokio::test]
async fn mismatched_patch_is_patch_invalid() {
    let (node, sha) = node("badpatch");
    let err = node
        .eval_candidate("c-bad", &sha, &patch("    return n % 7 == 0", "x"), &verifier(false), &grant(10))
        .await
        .unwrap_err();
    assert!(matches!(err, ContractError::PatchInvalid(_)), "{err:?}");
    let err = node
        .eval_candidate("c-bad2", &sha, "no blocks at all", &verifier(false), &grant(10))
        .await
        .unwrap_err();
    assert!(matches!(err, ContractError::PatchInvalid(_)), "{err:?}");
}

#[tokio::test]
async fn writes_outside_allowed_paths_are_policy_violations() {
    let (node, sha) = node("paths");
    for p in [
        "<<<<<<< SEARCH other.py\n=======\nx = 1\n>>>>>>> REPLACE".to_string(),
        "<<<<<<< SEARCH ../escape.py\n=======\nx = 1\n>>>>>>> REPLACE".to_string(),
    ] {
        let err = node.eval_candidate("c-path", &sha, &p, &verifier(false), &grant(10)).await.unwrap_err();
        assert!(matches!(err, ContractError::PolicyViolation(_)), "{err:?}");
    }
}

#[tokio::test]
async fn infinite_loop_is_killed_by_wall_clock_policy() {
    let (node, sha) = node("timeout");
    let p = patch("    return n % 3 == 0", "    while True:\n        pass");
    let started = Instant::now();
    let err = node.eval_candidate("c-hang", &sha, &p, &verifier(false), &grant(2)).await.unwrap_err();
    assert!(matches!(err, ContractError::PolicyViolation(_)), "{err:?}");
    assert!(started.elapsed().as_secs() < 8, "timeout not enforced: {:?}", started.elapsed());
}

#[tokio::test]
async fn hanging_build_is_killed_by_wall_clock_policy() {
    let root = std::env::temp_dir().join(format!("evolve-node-test-buildhang-{}", std::process::id()));
    let mut cfg = LocalSandboxConfig::new("test-buildhang", &root);
    cfg.build_cmd = Some(vec![python(), "-c".into(), "import time; time.sleep(30)".into()]);
    let node = LocalSandboxNode::new(cfg).unwrap();
    let mut files = BTreeMap::new();
    files.insert("solution.py".to_string(), "def classify(n):\n    return False\n".to_string());
    let sha = node.register_snapshot(&files).unwrap();
    let started = Instant::now();
    let err = node.eval_candidate("c-bh", &sha, "", &verifier(false), &grant(1)).await.unwrap_err();
    assert!(matches!(err, ContractError::PolicyViolation(_)), "{err:?}");
    assert!(started.elapsed().as_secs() < 6);
}

#[tokio::test]
async fn holdout_errors_never_leak_diagnostics() {
    let (node, sha) = node("leak");
    let secret = "SECRET_HOLDOUT_DIAGNOSTIC";
    let p = patch(
        "    return n % 3 == 0",
        &format!("    import sys\n    print('{secret}', file=sys.stderr)\n    raise ValueError('{secret}')"),
    );
    let train = node.eval_candidate("c-leak", &sha, &p, &verifier(false), &grant(10)).await.unwrap();
    assert!(!train.passed);
    assert!(train.public_feedback.as_deref().unwrap_or("").contains(secret), "train should get diagnostics");

    let hold = node.eval_candidate("c-leak", &sha, &p, &verifier(true), &grant(10)).await.unwrap();
    assert!(!hold.passed && hold.is_holdout);
    assert_eq!(hold.public_feedback, None);
    let serialized = serde_json::to_string(&hold).unwrap();
    assert!(!serialized.contains(secret), "holdout mark leaked: {serialized}");

    // A verifier that crashes with diagnostics on stderr must also stay silent in holdout mode.
    let crashing = VerifierSpec {
        verifier_id: "crashy-holdout".into(),
        cmd: vec![python(), "-c".into(), format!("import sys; sys.stderr.write('{secret}'); sys.exit(3)")],
        is_holdout: true,
    };
    let m = node.eval_candidate("c-crash", &sha, "", &crashing, &grant(10)).await.unwrap();
    assert!(!m.passed);
    assert_eq!(m.public_feedback, None);
}

#[tokio::test]
async fn artifact_is_read_only_during_verification() {
    let (node, sha) = node("readonly");
    let p = patch(
        "    return n % 3 == 0",
        "    open('solution.py', 'a').write('# tampered')\n    return n % 3 == 0",
    );
    let mark = node.eval_candidate("c-ro", &sha, &p, &verifier(false), &grant(10)).await.unwrap();
    assert!(!mark.passed, "candidate must not be able to modify the artifact");
    let fb = mark.public_feedback.unwrap_or_default();
    assert!(fb.contains("Permission") || fb.contains("denied") || fb.contains("Errno"), "{fb}");
}

#[tokio::test]
async fn unknown_snapshot_is_not_found() {
    let (node, _) = node("missing");
    let err = node
        .eval_candidate("c", &"0".repeat(64), "", &verifier(false), &grant(5))
        .await
        .unwrap_err();
    assert!(matches!(err, ContractError::NotFound(_)));
}
