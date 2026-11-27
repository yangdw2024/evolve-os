use core_contracts::{ContractError, MicroUsd};
use serde::Deserialize;
use std::path::Path;

/// Integer pricing in micro-USD per one million tokens (no floating point accounting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    pub input_micro_usd_per_mtok: MicroUsd,
    pub output_micro_usd_per_mtok: MicroUsd,
}

impl Pricing {
    pub const FREE: Pricing = Pricing {
        input_micro_usd_per_mtok: 0,
        output_micro_usd_per_mtok: 0,
    };

    /// Exact integer cost, rounded up to the next micro-USD.
    pub fn cost(&self, prompt_tokens: u64, completion_tokens: u64) -> MicroUsd {
        let total = (prompt_tokens as i128) * (self.input_micro_usd_per_mtok.max(0) as i128)
            + (completion_tokens as i128) * (self.output_micro_usd_per_mtok.max(0) as i128);
        let micro = (total + 999_999) / 1_000_000;
        micro.clamp(0, i64::MAX as i128) as i64
    }

    /// Conservative token estimate (~3 bytes per token, plus chat framing overhead).
    pub fn estimate_tokens(text_bytes: usize) -> u64 {
        (text_bytes as u64).div_ceil(3) + 16
    }

    /// Worst-case pre-call estimate used for the reservation.
    pub fn estimate(&self, prompt_bytes: usize, max_output_tokens: u32) -> MicroUsd {
        self.cost(Self::estimate_tokens(prompt_bytes), max_output_tokens as u64)
    }
}

fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_min_chunk_chars() -> usize {
    40
}

/// One model endpoint. API keys are NEVER stored here: only the *name* of the env var.
/// `deny_unknown_fields` makes an accidental `api_key = "..."` entry a hard config error.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierModelConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    pub input_micro_usd_per_mtok: MicroUsd,
    pub output_micro_usd_per_mtok: MicroUsd,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Optional reasoning level label (must be a level listed in the model catalog).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// How the reasoning level is sent: `reasoning_effort` body param or `<model>-<value>` suffix.
    #[serde(default)]
    pub reasoning_via: ReasoningVia,
    /// Output-token cap resolved from the model catalog (not user-settable).
    #[serde(skip)]
    pub max_output_cap: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningVia {
    #[default]
    ReasoningEffort,
    ModelSuffix,
}

impl TierModelConfig {
    pub fn pricing(&self) -> Pricing {
        Pricing {
            input_micro_usd_per_mtok: self.input_micro_usd_per_mtok,
            output_micro_usd_per_mtok: self.output_micro_usd_per_mtok,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedupConfig {
    #[serde(default = "default_min_chunk_chars")]
    pub min_chunk_chars: usize,
    #[serde(default)]
    pub max_prompt_bytes: Option<usize>,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            min_chunk_chars: default_min_chunk_chars(),
            max_prompt_bytes: None,
        }
    }
}

/// Retry policy for model calls (mirrors "retry only on transient errors" UIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetryPolicy {
    /// Retry only `NodeTransient` (429 / 5xx / timeout / connect).
    #[default]
    TransientOnly,
    /// Never retry.
    None,
}

/// Wire protocol. Only OpenAI Chat Completions is implemented in Mode A.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApiProtocol {
    #[default]
    OpenaiChatCompletions,
}

/// Descriptive provider block (display metadata + retry behaviour).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub protocol: ApiProtocol,
    #[serde(default)]
    pub retry_policy: RetryPolicy,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_max_retries() -> u32 {
    2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    Text,
    Image,
}

/// One entry of the model catalog (context window, output cap, inputs, reasoning levels).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalogEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub input_types: Vec<InputType>,
    /// Reasoning level label -> API parameter value (e.g. "high" -> "high").
    #[serde(default)]
    pub reasoning_levels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    #[serde(default)]
    pub provider: Option<ProviderConfig>,
    pub fast: TierModelConfig,
    pub strong: TierModelConfig,
    #[serde(default)]
    pub dedup: DedupConfig,
    #[serde(default)]
    pub models: Vec<ModelCatalogEntry>,
}

impl RouterConfig {
    pub fn from_toml_str(text: &str) -> Result<Self, ContractError> {
        // Only `message()` is surfaced (not the source span) so a misplaced secret is never echoed.
        let mut cfg: RouterConfig = toml::from_str(text)
            .map_err(|e| ContractError::Internal(format!("invalid llm router config: {}", e.message())))?;
        cfg.resolve_catalog()?;
        Ok(cfg)
    }

    pub fn model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Loop-level retry count implied by the provider retry policy.
    pub fn max_transient_retries(&self) -> u32 {
        match &self.provider {
            Some(p) if p.retry_policy == RetryPolicy::None => 0,
            Some(p) => p.max_retries,
            None => default_max_retries(),
        }
    }

    /// Validates tiers against the catalog and maps reasoning labels to API values.
    fn resolve_catalog(&mut self) -> Result<(), ContractError> {
        if self.models.is_empty() {
            return Ok(());
        }
        let models = self.models.clone();
        for (name, tier) in [("fast", &mut self.fast), ("strong", &mut self.strong)] {
            let entry = models.iter().find(|m| m.id == tier.model).ok_or_else(|| {
                ContractError::Internal(format!(
                    "[{name}].model `{}` is not listed in [[models]]",
                    tier.model
                ))
            })?;
            if let Some(level) = tier.reasoning_effort.take() {
                let value = entry.reasoning_levels.get(&level).ok_or_else(|| {
                    ContractError::Internal(format!(
                        "[{name}].reasoning_effort `{level}` not in reasoning_levels of `{}`",
                        entry.id
                    ))
                })?;
                match tier.reasoning_via {
                    ReasoningVia::ReasoningEffort => tier.reasoning_effort = Some(value.clone()),
                    ReasoningVia::ModelSuffix => tier.model = format!("{}-{value}", entry.id),
                }
            }
            tier.max_output_cap = entry.max_output_tokens;
        }
        Ok(())
    }

    pub fn from_toml_file(path: impl AsRef<Path>) -> Result<Self, ContractError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| {
            ContractError::Internal(format!("read llm config {}: {e}", path.display()))
        })?;
        Self::from_toml_str(&text)
    }

    /// Reads `EVOLVE_{FAST,STRONG}_{BASE_URL,MODEL,API_KEY_ENV,INPUT_PRICE,OUTPUT_PRICE,TIMEOUT_SECS}`.
    pub fn from_env() -> Result<Self, ContractError> {
        Ok(Self {
            provider: None,
            fast: tier_from_env("FAST")?,
            strong: tier_from_env("STRONG")?,
            dedup: DedupConfig::default(),
            models: Vec::new(),
        })
    }
}

fn tier_from_env(tier: &str) -> Result<TierModelConfig, ContractError> {
    let get = |suffix: &str| std::env::var(format!("EVOLVE_{tier}_{suffix}")).ok();
    let required = |suffix: &str| {
        get(suffix).ok_or_else(|| {
            ContractError::Internal(format!("missing env var EVOLVE_{tier}_{suffix}"))
        })
    };
    let parse_i64 = |suffix: &str| -> Result<i64, ContractError> {
        required(suffix)?.trim().parse::<i64>().map_err(|e| {
            ContractError::Internal(format!("EVOLVE_{tier}_{suffix} must be integer micro-USD/Mtok: {e}"))
        })
    };
    Ok(TierModelConfig {
        base_url: get("BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
        model: required("MODEL")?,
        api_key_env: get("API_KEY_ENV").unwrap_or_else(default_api_key_env),
        input_micro_usd_per_mtok: parse_i64("INPUT_PRICE")?,
        output_micro_usd_per_mtok: parse_i64("OUTPUT_PRICE")?,
        timeout_secs: get("TIMEOUT_SECS")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(default_timeout_secs),
        reasoning_effort: get("REASONING_EFFORT"),
        reasoning_via: ReasoningVia::ReasoningEffort,
        max_output_cap: None,
    })
}
