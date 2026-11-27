use clap::{Args, Parser, Subcommand};
use core_contracts::{BudgetLedger, LlmClient};
use evolve_loop::money::{format_usd, parse_usd};
use evolve_loop::setup::{
    engine_config, load_bench, materialize_embedded_toy_bench, toy_node, DEFAULT_BUDGET_CAP_MICRO_USD,
};

const DEFAULT_BENCH: &str = "examples/toy-bench";
use evolve_loop::{EvolutionEngine, MockToyMutator};
use ledger_sqlite::SqliteLedgerStore;
use llm_router::{BudgetedLlmRouter, OpenAiCompatClient, Pricing, PromptDeduper, RouterConfig, TierBackend};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "evolve", about = "evolve-os local self-evolution loop (MVP)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a multi-island evolution on a bench directory.
    Run(RunArgs),
    /// Audit ledger/marks/grid consistency of an existing database.
    Audit(AuditArgs),
}

#[derive(Args)]
struct RunArgs {
    /// SQLite WAL database path.
    #[arg(long, default_value = "evolve-run.db")]
    db: PathBuf,
    #[arg(long, default_value_t = 2)]
    islands: u32,
    #[arg(long, default_value_t = 10)]
    iterations: u32,
    /// Offline deterministic mutator (no network). Still flows through the budget router.
    #[arg(long)]
    mock_llm: bool,
    /// Hard budget ceiling for the run (decimal USD, converted to integer micro-USD).
    #[arg(long, default_value = "0.50")]
    max_budget_usd: String,
    /// Required to set --max-budget-usd above the $0.50 safety cap.
    #[arg(long)]
    allow_budget_above_cap: bool,
    /// TOML router config (otherwise EVOLVE_FAST_* / EVOLVE_STRONG_* env vars).
    #[arg(long)]
    llm_config: Option<PathBuf>,
    #[arg(long, default_value = "examples/toy-bench")]
    bench_dir: PathBuf,
    #[arg(long, env = "EVOLVE_PYTHON", default_value = "python")]
    python: String,
    /// Per-subprocess hard wall-clock limit (PolicyGrant.max_wall_secs).
    #[arg(long, default_value_t = 3)]
    wall_secs: u64,
    /// Sandbox snapshots/work root (default: ./evolve-work).
    #[arg(long, default_value = "evolve-work")]
    work_dir: PathBuf,
    #[arg(long)]
    run_id: Option<String>,
    /// Mock mode: every N-th LLM call fails with a simulated 503 (0 disables).
    #[arg(long, default_value_t = 7)]
    mock_transient_every: u64,
    #[arg(long, default_value_t = 3)]
    stagnation_limit: u32,
    #[arg(long, default_value_t = 5)]
    migrate_every: u32,
    /// Output token budget per call (capped by the model catalog). Reasoning models need headroom:
    /// hidden reasoning tokens count against it and too-small values can cause upstream 500s.
    #[arg(long, default_value_t = 4096)]
    max_output_tokens: u32,
}

#[derive(Args)]
struct AuditArgs {
    #[arg(long, default_value = "evolve-run.db")]
    db: PathBuf,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    max_budget_usd: Option<String>,
}

fn print_json<T: serde::Serialize>(label: &str, value: &T) {
    println!("{label}:\n{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

async fn run(args: RunArgs) -> Result<bool, String> {
    let budget = parse_usd(&args.max_budget_usd)?;
    if budget > DEFAULT_BUDGET_CAP_MICRO_USD && !args.allow_budget_above_cap {
        return Err(format!(
            "--max-budget-usd {} exceeds the {} safety cap; pass --allow-budget-above-cap to override",
            format_usd(budget),
            format_usd(DEFAULT_BUDGET_CAP_MICRO_USD)
        ));
    }
    if args.islands == 0 {
        return Err("--islands must be >= 1".into());
    }
    let run_id = args
        .run_id
        .unwrap_or_else(|| format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    if let Some(parent) = args.db.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let store = SqliteLedgerStore::open(&args.db).map_err(|e| e.to_string())?;
    let bench_dir = if args.bench_dir.is_dir() {
        args.bench_dir.clone()
    } else if args.bench_dir == std::path::Path::new(DEFAULT_BENCH) {
        let dir = materialize_embedded_toy_bench(&args.work_dir).map_err(|e| e.to_string())?;
        println!("bench dir `{DEFAULT_BENCH}` not found; using embedded toy bench at {}", dir.display());
        dir
    } else {
        return Err(format!("bench dir {} does not exist", args.bench_dir.display()));
    };
    let bench = load_bench(&bench_dir, &args.python).map_err(|e| e.to_string())?;
    let node = Arc::new(toy_node(&args.work_dir, &args.python).map_err(|e| e.to_string())?);
    let ledger: Arc<dyn BudgetLedger> = Arc::new(store.clone());

    let mut retries_override = None;
    let mut http_clients: Vec<Arc<OpenAiCompatClient>> = Vec::new();
    let llm: Arc<dyn LlmClient> = if args.mock_llm {
        // Representative small/large model list prices (integer micro-USD per Mtok).
        let fast = Pricing { input_micro_usd_per_mtok: 150_000, output_micro_usd_per_mtok: 600_000 };
        let strong = Pricing { input_micro_usd_per_mtok: 2_500_000, output_micro_usd_per_mtok: 10_000_000 };
        let mock = Arc::new(MockToyMutator::new(fast, strong, args.mock_transient_every));
        Arc::new(
            BudgetedLlmRouter::new(
                ledger,
                TierBackend { client: mock.clone(), pricing: fast },
                TierBackend { client: mock, pricing: strong },
                budget,
                PromptDeduper::default(),
            )
            .with_node_id("llm-router-mock"),
        )
    } else {
        let cfg = match &args.llm_config {
            Some(p) => RouterConfig::from_toml_file(p),
            None => RouterConfig::from_env(),
        }
        .map_err(|e| e.to_string())?;
        for (tier, t) in [("fast", &cfg.fast), ("strong", &cfg.strong)] {
            if std::env::var(&t.api_key_env).map(|v| v.trim().is_empty()).unwrap_or(true) {
                eprintln!("warning: {tier} tier api key env `{}` is not set; requests will be unauthenticated", t.api_key_env);
            }
            println!("tier {tier}: model={} base_url={} reasoning_effort={:?} max_output_cap={:?}",
                t.model, t.base_url, t.reasoning_effort, t.max_output_cap);
        }
        retries_override = Some(cfg.max_transient_retries());
        let fast = Arc::new(OpenAiCompatClient::new(cfg.fast.clone()).map_err(|e| e.to_string())?);
        let strong = Arc::new(OpenAiCompatClient::new(cfg.strong.clone()).map_err(|e| e.to_string())?);
        http_clients.push(fast.clone());
        http_clients.push(strong.clone());
        Arc::new(BudgetedLlmRouter::new(
            ledger,
            TierBackend { pricing: cfg.fast.pricing(), client: fast },
            TierBackend { pricing: cfg.strong.pricing(), client: strong },
            budget,
            PromptDeduper::new(&cfg.dedup),
        ))
    };

    let mut cfg = engine_config(run_id.clone(), args.islands, args.iterations, budget, args.wall_secs, &bench);
    cfg.stagnation_limit = args.stagnation_limit;
    cfg.migrate_every = args.migrate_every;
    cfg.max_output_tokens = args.max_output_tokens;
    if let Some(r) = retries_override {
        cfg.max_transient_retries = r;
    }
    println!(
        "run_id={run_id} islands={} iterations={} budget={} mode={}",
        args.islands,
        args.iterations,
        format_usd(budget),
        if args.mock_llm { "mock-llm" } else { "real-llm" }
    );

    let engine = EvolutionEngine::new(cfg, store.clone(), node, llm);
    engine.seed(bench.seed_files).await.map_err(|e| format!("seed failed: {e}"))?;
    let report = engine.run().await.map_err(|e| format!("run aborted: {e}"))?;
    print_json("run_report", &report);
    println!("spent: {} of {}", format_usd(report.spent_micro_usd), format_usd(budget));
    for c in &http_clients {
        println!("model {}: responses billed by usage estimate (no `usage` field) = {}", c.model(), c.usage_fallbacks());
    }

    let audit = store.audit(&run_id, Some(budget)).await.map_err(|e| e.to_string())?;
    print_json("audit", &audit);
    Ok(audit.violations.is_empty())
}

async fn audit(args: AuditArgs) -> Result<bool, String> {
    let limit = args.max_budget_usd.as_deref().map(parse_usd).transpose()?;
    let store = SqliteLedgerStore::open(&args.db).map_err(|e| e.to_string())?;
    let report = store.audit(&args.run_id, limit).await.map_err(|e| e.to_string())?;
    print_json("audit", &report);
    Ok(report.violations.is_empty())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run(a) => run(a).await,
        Cmd::Audit(a) => audit(a).await,
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("audit found invariant violations");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
