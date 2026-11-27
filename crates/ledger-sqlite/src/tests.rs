use crate::SqliteLedgerStore;
use core_contracts::{
    ArtifactPayload, ArtifactType, BudgetLedger, ContractError, EvolutionArchive, Mark,
};
use std::collections::BTreeMap;

fn sample_mark(
    candidate_id: &str,
    score: f64,
    cell_key: &str,
    is_holdout: bool,
    public_feedback: Option<&str>,
) -> Mark {
    let mut metrics = BTreeMap::new();
    metrics.insert("accuracy".to_string(), score);
    Mark {
        candidate_id: candidate_id.to_string(),
        verifier_id: if is_holdout {
            "holdout-v1".into()
        } else {
            "train-v1".into()
        },
        passed: true,
        primary_score: score,
        metrics,
        cell_key: cell_key.to_string(),
        is_holdout,
        artifact_sha256: "computed-by-store".into(),
        public_feedback: public_feedback.map(str::to_string),
    }
}

#[tokio::test]
async fn multi_island_identical_blob_convergence_succeeds() {
    let store = SqliteLedgerStore::open_in_memory().expect("open in-memory db");
    let identical_payload = ArtifactPayload::SingleFile("fn solve() -> i32 { 42 }".into());

    store
        .record_candidate(
            1,
            None,
            "abc123",
            ArtifactType::CodeDiff,
            "@@ diff island 1 @@",
            &identical_payload,
            &sample_mark("cand-i1", 0.90, "lat:1|loc:1", false, Some("ok")),
        )
        .await
        .expect("island 1 insert");

    // Island 3 independently converges to the exact same materialized code.
    store
        .record_candidate(
            3,
            None,
            "abc123",
            ArtifactType::CodeDiff,
            "@@ diff island 3 @@",
            &identical_payload,
            &sample_mark("cand-i3", 0.90, "lat:1|loc:1", false, Some("ok")),
        )
        .await
        .expect("island 3 insert should NOT fail on duplicate blob sha256");

    let ctx1 = store.sample_prompt_context(1, 2).await.expect("sample island 1");
    assert_eq!(ctx1.parent.id, "cand-i1");
    assert_eq!(ctx1.parent.payload, identical_payload);
    assert_eq!(ctx1.inspirations.len(), 1);
    assert_eq!(ctx1.inspirations[0].id, "cand-i3");
}

#[tokio::test]
async fn holdout_mark_never_updates_grid_and_rejects_public_feedback() {
    let store = SqliteLedgerStore::open_in_memory().expect("open in-memory db");
    let payload = ArtifactPayload::SingleFile("fn fast() {}".into());

    // 1. Holdout mark with public_feedback MUST be rejected.
    let leaky = sample_mark("cand-leak", 0.99, "lat:1", true, Some("secret test failed"));
    let err = store
        .record_candidate(
            0,
            None,
            "deadbeef",
            ArtifactType::CodeDiff,
            "diff",
            &payload,
            &leaky,
        )
        .await
        .expect_err("must reject holdout public_feedback");
    assert!(matches!(err, ContractError::HoldoutLeakPrevented(_)));

    // 2. Clean holdout mark succeeds in marks table, but MUST NOT populate map_elites_grid.
    let clean_holdout = sample_mark("cand-holdout-only", 0.99, "lat:1", true, None);
    store
        .record_candidate(
            0,
            None,
            "deadbeef",
            ArtifactType::CodeDiff,
            "diff",
            &payload,
            &clean_holdout,
        )
        .await
        .expect("clean holdout mark records");

    let sample_err = store
        .sample_prompt_context(0, 1)
        .await
        .expect_err("grid must stay empty when only holdout marks exist");
    assert!(matches!(sample_err, ContractError::NotFound(_)));
}

#[tokio::test]
async fn concurrent_budget_reservations_prevent_overspend() {
    let store = SqliteLedgerStore::open_in_memory().expect("open in-memory db");
    let limit_micro_usd = 1_000_000; // $1.00
    let per_call_estimate = 400_000; // $0.40 -> at most 2 concurrent calls allowed

    let mut handles = Vec::new();
    for i in 0..5 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            s.reserve("run-1", &format!("node-{i}"), per_call_estimate, limit_micro_usd)
                .await
        }));
    }

    let mut reserved = Vec::new();
    let mut rejected = 0usize;
    for h in handles {
        match h.await.expect("join") {
            Ok(res) => reserved.push(res),
            Err(ContractError::BudgetExceeded { .. }) => rejected += 1,
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    assert_eq!(reserved.len(), 2, "exactly two $0.40 reservations fit in $1.00");
    assert_eq!(rejected, 3);

    // Settle first reservation for $0.25 (refunds $0.15 headroom) and release second.
    store
        .settle(&reserved[0], "node-0", Some("fast"), 1000, 500, 250_000, "{}")
        .await
        .expect("settle");
    store.release(&reserved[1]).await.expect("release");

    assert_eq!(store.total_spent("run-1").await.expect("spent"), 250_000);

    // Now $0.75 remains, so a $0.70 reservation succeeds.
    store
        .reserve("run-1", "node-next", 700_000, limit_micro_usd)
        .await
        .expect("should fit after settle + release");
}

#[tokio::test]
async fn cross_island_migration_and_filemap_skill_bundle() {
    let store = SqliteLedgerStore::open_in_memory().expect("open in-memory db");

    let mut bundle = BTreeMap::new();
    bundle.insert("manifest.json".to_string(), r#"{"name":"opt-skill"}"#.into());
    bundle.insert("run.py".to_string(), "print('v2')".into());
    let skill_payload = ArtifactPayload::FileMap(bundle.clone());

    store
        .record_candidate(
            0,
            None,
            "base0",
            ArtifactType::SkillBundle,
            "skill-diff-0",
            &skill_payload,
            &sample_mark("skill-strong", 0.95, "cost:1|acc:5", false, Some("pass")),
        )
        .await
        .expect("island 0 elite");

    store
        .record_candidate(
            1,
            None,
            "base0",
            ArtifactType::SkillBundle,
            "skill-diff-1",
            &ArtifactPayload::SingleFile("legacy".into()),
            &sample_mark("skill-weak", 0.40, "cost:1|acc:5", false, Some("pass")),
        )
        .await
        .expect("island 1 weak elite");

    let migrated = store.migrate_elites().await.expect("migrate");
    assert_eq!(migrated, 1, "island 0 strong elite should replace island 1 weak elite");

    let ctx = store.sample_prompt_context(1, 0).await.expect("sample island 1");
    assert_eq!(ctx.parent.id, "skill-strong");
    assert_eq!(ctx.parent.payload, ArtifactPayload::FileMap(bundle));
}

#[tokio::test]
async fn blacklist_removes_elite_restores_previous_and_audits_clean() {
    let store = SqliteLedgerStore::open_in_memory().expect("open in-memory db");
    let payload_a = ArtifactPayload::SingleFile("a".into());
    let payload_b = ArtifactPayload::SingleFile("b".into());
    store
        .record_candidate(0, None, "base", ArtifactType::CodeDiff, "d0", &payload_a,
            &sample_mark("good", 0.70, "cell:1", false, Some("ok")))
        .await
        .unwrap();
    store
        .record_candidate(0, Some("good"), "base", ArtifactType::CodeDiff, "d1", &payload_b,
            &sample_mark("overfit", 0.99, "cell:1", false, Some("ok")))
        .await
        .unwrap();
    store
        .record_candidate(0, Some("good"), "base", ArtifactType::CodeDiff, "d1", &payload_b,
            &sample_mark("overfit", 0.10, "cell:1", true, None))
        .await
        .unwrap();
    // Simulate migration copying the overfit elite to island 1.
    store
        .record_candidate(1, None, "base", ArtifactType::CodeDiff, "d2", &payload_a,
            &sample_mark("weak1", 0.20, "cell:1", false, None))
        .await
        .unwrap();
    assert_eq!(store.migrate_elites().await.unwrap(), 1);

    let touched = store.blacklist_candidate("run", "overfit", "holdout regression").await.unwrap();
    assert_eq!(touched, 2, "native cell + migrated copy");
    assert_eq!(store.candidate_status("overfit").await.unwrap().as_deref(), Some("blacklisted"));

    let ctx0 = store.sample_prompt_context(0, 5).await.unwrap();
    assert_eq!(ctx0.parent.id, "good", "previous elite restored");
    assert!(ctx0.inspirations.iter().all(|c| c.id != "overfit"));
    let ctx1 = store.sample_prompt_context(1, 5).await.unwrap();
    assert_eq!(ctx1.parent.id, "weak1");

    let report = store.audit("run", Some(0)).await.unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert_eq!(report.candidates_blacklisted, 1);
    assert_eq!(report.marks_holdout, 1);
    assert_eq!(report.event_counts.get("rollback"), Some(&1));
}
