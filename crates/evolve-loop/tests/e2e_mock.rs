//! End-to-end loop tests with the offline mock mutator (requires Python 3).

use core_contracts::{BudgetLedger, LlmClient};
use evolve_loop::setup::{engine_config, load_toy_bench, toy_node};
use evolve_loop::{EvolutionEngine, MockToyMutator};
use ledger_sqlite::SqliteLedgerStore;
use llm_router::{BudgetedLlmRouter, Pricing, PromptDeduper, TierBackend};
use std::path::PathBuf;
use std::sync::Arc;

fn python() -> String {
    std::env::var("EVOLVE_PYTHON").unwrap_or_else(|_| "python".into())
}

fn bench_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/toy-bench")
}

const FAST: Pricing = Pricing { input_micro_usd_per_mtok: 150_000, output_micro_usd_per_mtok: 600_000 };
const STRONG: Pricing = Pricing { input_micro_usd_per_mtok: 2_500_000, output_micro_usd_per_mtok: 10_000_000 };

fn build(tag: &str, budget: i64, iterations: u32) -> (Arc<EvolutionEngine>, SqliteLedgerStore, String, evolve_loop::setup::ToyBench) {
    let root = std::env::temp_dir().join(format!("evolve-loop-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let store = SqliteLedgerStore::open(root.join("run.db")).unwrap();
    let bench = load_toy_bench(&bench_dir(), &python()).unwrap();
    let node = Arc::new(toy_node(&root.join("work"), &python()).unwrap());
    let mock = Arc::new(MockToyMutator::new(FAST, STRONG, 7));
    let llm: Arc<dyn LlmClient> = Arc::new(BudgetedLlmRouter::new(
        Arc::new(store.clone()),
        TierBackend { client: mock.clone(), pricing: FAST },
        TierBackend { client: mock, pricing: STRONG },
        budget,
        PromptDeduper::default(),
    ));
    let run_id = format!("test-{tag}");
    let cfg = engine_config(run_id.clone(), 2, iterations, budget, 3, &bench);
    (EvolutionEngine::new(cfg, store.clone(), node, llm), store, run_id, bench)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_islands_evolve_blacklist_overfit_and_audit_clean() {
    let (engine, store, run_id, bench) = build("e2e", 500_000, 10);
    engine.seed(bench.seed_files).await.unwrap();
    let report = engine.run().await.unwrap();
    let s = &report.stats;

    assert!(!s.budget_stop);
    assert!(s.candidates_recorded >= 10, "{s:?}");
    assert!(s.patch_invalid >= 1 && s.patch_repaired >= 1, "{s:?}");
    assert!(s.policy_refusals >= 1, "infinite loop must be refused: {s:?}");
    assert!(s.llm_transient_retries >= 1, "{s:?}");
    assert!(s.blacklisted >= 1, "overfit lookup must be blacklisted by holdout: {s:?}");
    assert!(s.migrations >= 1, "{s:?}");
    assert!(s.llm_strong_requests >= 1, "{s:?}");

    let champ = report.champion.expect("champion");
    assert!(champ.holdout_passed);
    assert!(champ.train_score > 0.9, "{champ:?}");
    assert_eq!(store.candidate_status(&champ.candidate_id).await.unwrap().as_deref(), Some("active"));

    let audit = store.audit(&run_id, Some(500_000)).await.unwrap();
    assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    assert!(audit.marks_holdout >= 2);
    assert_eq!(audit.holdout_marks_with_feedback, 0);
    assert_eq!(audit.event_counts.get("refusal").copied().unwrap_or(0) as u64, s.policy_refusals);
    assert!(audit.released_reservations >= 1, "transient calls must release");
    assert_eq!(audit.llm_spent_micro_usd, store.total_spent(&run_id).await.unwrap());
    assert!(audit.llm_spent_micro_usd > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_budget_halts_gracefully_without_overspend() {
    let budget = 3_000; // $0.003: enough for only a handful of mock calls
    let (engine, store, run_id, bench) = build("budget", budget, 50);
    engine.seed(bench.seed_files).await.unwrap();
    let report = engine.run().await.unwrap();
    assert!(report.stats.budget_stop, "{:?}", report.stats);
    assert!(report.spent_micro_usd <= budget);
    let audit = store.audit(&run_id, Some(budget)).await.unwrap();
    assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    assert_eq!(audit.event_counts.get("budget_stop"), Some(&1));
}

#[test]
fn embedded_toy_bench_matches_examples() {
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/toy-bench");
    for (rel, content) in evolve_loop::setup::EMBEDDED_TOY_BENCH {
        let on_disk = std::fs::read_to_string(examples.join(rel)).unwrap().replace("\r\n", "\n");
        assert_eq!(on_disk, content.replace("\r\n", "\n"), "assets/toy-bench/{rel} drifted from examples/");
    }
}
