use crate::{
    BudgetedLlmRouter, DedupConfig, OpenAiCompatClient, Pricing, PromptDeduper, RouterConfig,
    TierBackend, TierModelConfig,
};
use async_trait::async_trait;
use core_contracts::{
    BudgetLedger, ContractError, LlmClient, LlmRequest, LlmResponse, LlmTier, MicroUsd,
};
use ledger_sqlite::SqliteLedgerStore;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum Behavior {
    Ok { cost: MicroUsd },
    Transient,
}

struct MockBackend {
    name: &'static str,
    behavior: Behavior,
    delay_ms: u64,
    calls: AtomicUsize,
}

impl MockBackend {
    fn new(name: &'static str, behavior: Behavior, delay_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            name,
            behavior,
            delay_ms,
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmClient for MockBackend {
    async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, ContractError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        match self.behavior {
            Behavior::Ok { cost } => Ok(LlmResponse {
                content: format!("answer from {}", self.name),
                model_id: self.name.to_string(),
                prompt_tokens: 10,
                completion_tokens: 5,
                actual_cost_micro_usd: cost,
            }),
            Behavior::Transient => Err(ContractError::NodeTransient("mock 503".into())),
        }
    }
}

fn backend(client: Arc<MockBackend>) -> TierBackend {
    TierBackend {
        client,
        pricing: Pricing::FREE,
    }
}

fn req(run_id: &str, tier: LlmTier, estimate: MicroUsd) -> LlmRequest {
    LlmRequest {
        run_id: run_id.into(),
        tier,
        system_prompt: "sys".into(),
        user_prompt: "user".into(),
        max_output_tokens: 64,
        estimated_cost_micro_usd: estimate,
    }
}

fn router(
    store: &SqliteLedgerStore,
    fast: Arc<MockBackend>,
    strong: Arc<MockBackend>,
    limit: MicroUsd,
) -> BudgetedLlmRouter {
    BudgetedLlmRouter::new(
        Arc::new(store.clone()),
        backend(fast),
        backend(strong),
        limit,
        PromptDeduper::default(),
    )
}

#[tokio::test]
async fn routes_fast_and_strong_to_distinct_backends_and_settles() {
    let store = SqliteLedgerStore::open_in_memory().unwrap();
    let fast = MockBackend::new("fast-model", Behavior::Ok { cost: 100 }, 0);
    let strong = MockBackend::new("strong-model", Behavior::Ok { cost: 900 }, 0);
    let r = router(&store, fast.clone(), strong.clone(), 1_000_000);

    let a = r.complete(req("run", LlmTier::Fast, 10)).await.unwrap();
    let b = r.complete(req("run", LlmTier::Strong, 10)).await.unwrap();
    assert_eq!(a.model_id, "fast-model");
    assert_eq!(b.model_id, "strong-model");
    assert_eq!((fast.calls(), strong.calls()), (1, 1));
    assert_eq!(store.total_spent("run").await.unwrap(), 1_000);
}

#[tokio::test]
async fn budget_hard_stop_never_calls_backend() {
    let store = SqliteLedgerStore::open_in_memory().unwrap();
    let fast = MockBackend::new("fast", Behavior::Ok { cost: 600 }, 0);
    let strong = MockBackend::new("strong", Behavior::Ok { cost: 600 }, 0);
    let r = router(&store, fast.clone(), strong, 1_000);

    r.complete(req("run", LlmTier::Fast, 600)).await.unwrap();
    let err = r.complete(req("run", LlmTier::Fast, 600)).await.unwrap_err();
    assert!(
        matches!(err, ContractError::BudgetExceeded { projected: 1_200, limit: 1_000 }),
        "got {err:?}"
    );
    assert_eq!(fast.calls(), 1, "backend must not be invoked after budget exhaustion");
    assert_eq!(store.total_spent("run").await.unwrap(), 600);
}

#[tokio::test]
async fn transient_error_releases_reservation() {
    let store = SqliteLedgerStore::open_in_memory().unwrap();
    let fast = MockBackend::new("fast", Behavior::Transient, 0);
    let strong = MockBackend::new("strong", Behavior::Ok { cost: 1 }, 0);
    let r = router(&store, fast.clone(), strong, 1_000);

    for _ in 0..3 {
        let err = r.complete(req("run", LlmTier::Fast, 900)).await.unwrap_err();
        assert!(matches!(err, ContractError::NodeTransient(_)));
    }
    assert_eq!(fast.calls(), 3);
    assert_eq!(store.total_spent("run").await.unwrap(), 0);
    // If any reservation had leaked, a full-limit reservation would be rejected.
    store
        .reserve("run", "probe", 1_000, 1_000)
        .await
        .expect("all transient reservations must have been released");
}

#[tokio::test]
async fn concurrent_calls_cannot_overspend() {
    let store = SqliteLedgerStore::open_in_memory().unwrap();
    let fast = MockBackend::new("fast", Behavior::Ok { cost: 300 }, 100);
    let strong = MockBackend::new("strong", Behavior::Ok { cost: 300 }, 100);
    let r = Arc::new(router(&store, fast.clone(), strong, 1_000));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = r.clone();
        handles.push(tokio::spawn(async move {
            r.complete(req("run", LlmTier::Fast, 300)).await
        }));
    }
    let (mut ok, mut exceeded) = (0, 0);
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(ContractError::BudgetExceeded { .. }) => exceeded += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((ok, exceeded), (3, 5));
    assert_eq!(fast.calls(), 3);
    assert_eq!(store.total_spent("run").await.unwrap(), 900);
}

#[tokio::test]
async fn reservation_uses_worst_case_pricing_estimate() {
    let store = SqliteLedgerStore::open_in_memory().unwrap();
    let fast = MockBackend::new("fast", Behavior::Ok { cost: 1 }, 0);
    let r = BudgetedLlmRouter::new(
        Arc::new(store.clone()),
        TierBackend {
            client: fast.clone(),
            // 1 USD per token output => max_output_tokens=64 => 64 USD worst case.
            pricing: Pricing {
                input_micro_usd_per_mtok: 0,
                output_micro_usd_per_mtok: 1_000_000_000_000,
            },
        },
        backend(MockBackend::new("strong", Behavior::Ok { cost: 1 }, 0)),
        1_000_000,
        PromptDeduper::default(),
    );
    let err = r.complete(req("run", LlmTier::Fast, 0)).await.unwrap_err();
    assert!(matches!(err, ContractError::BudgetExceeded { .. }));
    assert_eq!(fast.calls(), 0);
}

// ---------------------------------------------------------------- dedup

#[test]
fn dedup_removes_repeated_code_blocks_but_keeps_first() {
    let code = "```python\ndef classify(n):\n    return n % 3 == 0 or n % 5 == 0\n```";
    let user = format!("PARENT\n{code}\n\nINSPIRATION 1\n{code}\n\nshort\n\nshort");
    let (out, stats) = PromptDeduper::default().dedup("system", &user);
    assert_eq!(out.matches("def classify").count(), 1);
    assert!(out.contains("[duplicate fragment sha256:"));
    assert_eq!(stats.chunks_deduped, 1, "short chunks must not be deduped");
    assert_eq!(out.matches("short").count(), 2);
}

#[test]
fn dedup_removes_fragments_already_in_system_prompt_and_truncates() {
    let rule = "Respond ONLY with SEARCH/REPLACE blocks that match the parent exactly.";
    let user = format!("{rule}\n\nPlease improve the code.");
    let dd = PromptDeduper::new(&DedupConfig {
        min_chunk_chars: 20,
        max_prompt_bytes: Some(48),
    });
    let (out, stats) = dd.dedup(rule, &user);
    assert!(!out.contains("Respond ONLY"));
    assert!(stats.truncated || out.contains("Please improve"));
    let (out2, stats2) = dd.dedup("", &"x".repeat(200));
    assert!(stats2.truncated);
    assert!(out2.len() < 120);
}

// ---------------------------------------------------------------- config

#[test]
fn config_rejects_inline_api_key_without_echoing_it() {
    let bad = r#"
[fast]
base_url = "http://localhost:1/v1"
model = "m"
api_key = "sk-SUPERSECRET"
input_micro_usd_per_mtok = 1
output_micro_usd_per_mtok = 1
[strong]
base_url = "http://localhost:1/v1"
model = "m2"
input_micro_usd_per_mtok = 1
output_micro_usd_per_mtok = 1
"#;
    let err = RouterConfig::from_toml_str(bad).unwrap_err().to_string();
    assert!(!err.contains("SUPERSECRET"), "secret leaked in error: {err}");

    let good = bad.replace("api_key = \"sk-SUPERSECRET\"\n", "api_key_env = \"MY_KEY\"\n");
    let cfg = RouterConfig::from_toml_str(&good).unwrap();
    assert_eq!(cfg.fast.api_key_env, "MY_KEY");
    assert_eq!(cfg.strong.api_key_env, "OPENAI_API_KEY");
    assert_eq!(cfg.fast.timeout_secs, 60);
}

#[test]
fn pricing_rounds_up_in_integer_micro_usd() {
    let p = Pricing {
        input_micro_usd_per_mtok: 150_000,
        output_micro_usd_per_mtok: 600_000,
    };
    assert_eq!(p.cost(1_000_000, 0), 150_000);
    assert_eq!(p.cost(1, 0), 1, "fractional micro-USD rounds up");
    assert_eq!(p.cost(0, 0), 0);
}

// ---------------------------------------------------------------- HTTP mock server

async fn spawn_http_mock(
    status: u16,
    body: &'static str,
    delay_ms: u64,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let cap = cap.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(idx) = text.find("\r\n\r\n") {
                        let len = text[..idx]
                            .lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buf.len() >= idx + 4 + len {
                            break;
                        }
                    }
                }
                cap.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let resp = format!(
                    "HTTP/1.1 {status} MOCK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/v1"), captured)
}

fn tier_cfg(base_url: String, key_env: &str, timeout_secs: u64) -> TierModelConfig {
    TierModelConfig {
        base_url,
        model: "mock-gpt".into(),
        api_key_env: key_env.into(),
        input_micro_usd_per_mtok: 1_000_000,
        output_micro_usd_per_mtok: 2_000_000,
        timeout_secs,
        reasoning_effort: None,
        reasoning_via: crate::ReasoningVia::ReasoningEffort,
        max_output_cap: None,
    }
}

const NO_USAGE_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"0123456789ab","reasoning_content":"hidden"}}]}"#;

#[tokio::test]
async fn missing_usage_falls_back_to_conservative_estimate() {
    let (url, captured) = spawn_http_mock(200, NO_USAGE_BODY, 0).await;
    let mut cfg = tier_cfg(url, "EVOLVE_TEST_KEY_UNSET", 5);
    cfg.reasoning_effort = Some("high".into());
    cfg.max_output_cap = Some(32);
    let client = OpenAiCompatClient::new(cfg).unwrap();
    let mut r = req("run", LlmTier::Fast, 0);
    r.max_output_tokens = 400; // capped to 32 by catalog
    let resp = client.complete(r).await.unwrap();
    assert_eq!(resp.content, "0123456789ab");
    assert_eq!(resp.model_id, "mock-gpt", "falls back to configured model id");
    // prompt: ceil(7/3)+16 = 19 ; completion: max(ceil(12/3)+16=20, 32/4=8) = 20
    assert_eq!((resp.prompt_tokens, resp.completion_tokens), (19, 20));
    assert_eq!(resp.actual_cost_micro_usd, Pricing { input_micro_usd_per_mtok: 1_000_000, output_micro_usd_per_mtok: 2_000_000 }.cost(19, 20));
    assert!(resp.actual_cost_micro_usd > 0, "missing usage must never bill zero");
    assert_eq!(client.usage_fallbacks(), 1);
    let raw = captured.lock().unwrap().join("\n");
    assert!(raw.contains("\"max_tokens\":32"), "{raw}");
    assert!(raw.contains("\"reasoning_effort\":\"high\""), "{raw}");
}

#[test]
fn catalog_resolves_model_suffix_reasoning_and_validates() {
    let text = r#"
[provider]
id = "antigravity-local"
display_name = "Antigravity Tools"
protocol = "openai-chat-completions"
retry_policy = "transient_only"
max_retries = 3

[fast]
base_url = "http://127.0.0.1:8045/v1"
model = "gemini-3.8-flash"
api_key_env = "EVOLVE_API_KEY"
input_micro_usd_per_mtok = 1
output_micro_usd_per_mtok = 1
reasoning_effort = "low"
reasoning_via = "model_suffix"

[strong]
base_url = "http://127.0.0.1:8045/v1"
model = "claude-sonnet-5-5"
api_key_env = "EVOLVE_API_KEY"
input_micro_usd_per_mtok = 1
output_micro_usd_per_mtok = 1
reasoning_effort = "medium"

[[models]]
id = "gemini-3.8-flash"
display_name = "Gemini 3.8 Flash"
context_window = 1048576
max_output_tokens = 131072
input_types = ["text", "image"]
reasoning_levels = { low = "low", medium = "medium", high = "high" }

[[models]]
id = "claude-sonnet-5-5"
max_output_tokens = 64000
reasoning_levels = { medium = "medium" }
"#;
    let cfg = RouterConfig::from_toml_str(text).unwrap();
    assert_eq!(cfg.fast.model, "gemini-3.8-flash-low");
    assert_eq!(cfg.fast.reasoning_effort, None);
    assert_eq!(cfg.fast.max_output_cap, Some(131072));
    assert_eq!(cfg.strong.model, "claude-sonnet-5-5");
    assert_eq!(cfg.strong.reasoning_effort.as_deref(), Some("medium"));
    assert_eq!(cfg.max_transient_retries(), 3);

    let bad_level = text.replace("reasoning_effort = \"medium\"", "reasoning_effort = \"xhigh\"");
    assert!(RouterConfig::from_toml_str(&bad_level).is_err());
    let bad_model = text.replace("model = \"claude-sonnet-5-5\"", "model = \"unknown\"");
    assert!(RouterConfig::from_toml_str(&bad_model).is_err());
}

const OK_BODY: &str = r#"{"model":"mock-gpt-0613","choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":1000,"completion_tokens":500}}"#;

#[tokio::test]
async fn http_200_parses_content_usage_and_sends_bearer_from_env() {
    std::env::set_var("EVOLVE_TEST_KEY_HTTP200", "test-secret-123");
    let (url, captured) = spawn_http_mock(200, OK_BODY, 0).await;
    let client = OpenAiCompatClient::new(tier_cfg(url, "EVOLVE_TEST_KEY_HTTP200", 5)).unwrap();
    let resp = client.complete(req("run", LlmTier::Fast, 0)).await.unwrap();
    assert_eq!(resp.content, "hello");
    assert_eq!(resp.model_id, "mock-gpt-0613");
    assert_eq!((resp.prompt_tokens, resp.completion_tokens), (1000, 500));
    // 1000 * 1 USD/Mtok + 500 * 2 USD/Mtok = 0.002 USD = 2000 micro-USD
    assert_eq!(resp.actual_cost_micro_usd, 2_000);
    let raw = captured.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(raw.starts_with("post /v1/chat/completions"));
    assert!(raw.contains("authorization: bearer test-secret-123"));
    assert!(raw.contains("\"model\":\"mock-gpt\""));
}

#[tokio::test]
async fn http_429_and_5xx_map_to_transient_and_4xx_to_internal() {
    for (status, transient) in [(429u16, true), (503, true), (500, true), (400, false), (401, false)] {
        let (url, _) = spawn_http_mock(status, r#"{"error":"x"}"#, 0).await;
        let client = OpenAiCompatClient::new(tier_cfg(url, "EVOLVE_TEST_KEY_UNSET", 5)).unwrap();
        let err = client.complete(req("run", LlmTier::Fast, 0)).await.unwrap_err();
        if transient {
            assert!(matches!(err, ContractError::NodeTransient(_)), "{status}: {err:?}");
        } else {
            assert!(matches!(err, ContractError::Internal(_)), "{status}: {err:?}");
        }
    }
}

#[tokio::test]
async fn http_timeout_and_connection_refused_map_to_transient() {
    let (url, _) = spawn_http_mock(200, OK_BODY, 3_000).await;
    let client = OpenAiCompatClient::new(tier_cfg(url, "EVOLVE_TEST_KEY_UNSET", 1)).unwrap();
    let err = client.complete(req("run", LlmTier::Fast, 0)).await.unwrap_err();
    assert!(matches!(err, ContractError::NodeTransient(_)), "{err:?}");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}/v1", listener.local_addr().unwrap());
    drop(listener);
    let client = OpenAiCompatClient::new(tier_cfg(dead, "EVOLVE_TEST_KEY_UNSET", 2)).unwrap();
    let err = client.complete(req("run", LlmTier::Fast, 0)).await.unwrap_err();
    assert!(matches!(err, ContractError::NodeTransient(_)), "{err:?}");
}

#[tokio::test]
async fn router_over_http_settles_on_200_and_releases_on_429() {
    let store = SqliteLedgerStore::open_in_memory().unwrap();
    let (ok_url, _) = spawn_http_mock(200, OK_BODY, 0).await;
    let (busy_url, _) = spawn_http_mock(429, r#"{"error":"rate"}"#, 0).await;
    let cfg = RouterConfig {
        provider: None,
        fast: tier_cfg(busy_url, "EVOLVE_TEST_KEY_UNSET", 5),
        strong: tier_cfg(ok_url, "EVOLVE_TEST_KEY_UNSET", 5),
        dedup: DedupConfig::default(),
        models: Vec::new(),
    };
    let r = BudgetedLlmRouter::from_config(&cfg, Arc::new(store.clone()), 50_000).unwrap();

    let err = r.complete(req("run", LlmTier::Fast, 0)).await.unwrap_err();
    assert!(matches!(err, ContractError::NodeTransient(_)));
    assert_eq!(store.total_spent("run").await.unwrap(), 0);

    let resp = r.complete(req("run", LlmTier::Strong, 0)).await.unwrap();
    assert_eq!(resp.actual_cost_micro_usd, 2_000);
    assert_eq!(store.total_spent("run").await.unwrap(), 2_000);
    // Remaining headroom = 48_000 -> must be fully reservable (no leaked pending reservation).
    store.reserve("run", "probe", 48_000, 50_000).await.unwrap();
}
