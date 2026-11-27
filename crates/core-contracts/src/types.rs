use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Integer micro-USD accounting (1 USD = 1_000_000 MicroUsd) to eliminate f64 drift.
pub type MicroUsd = i64;

/// Declarative execution boundary enforced outside the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyGrant {
    pub allowed_paths: Vec<String>,
    pub allow_network: bool,
    pub max_wall_secs: u64,
    pub max_cost_micro_usd: MicroUsd,
}

/// What kind of evolvable artifact a candidate represents.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    CodeDiff,
    SkillBundle,
    PromptPolicy,
}

impl ArtifactType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactType::CodeDiff => "code_diff",
            ArtifactType::SkillBundle => "skill_bundle",
            ArtifactType::PromptPolicy => "prompt_policy",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "code_diff" => Some(ArtifactType::CodeDiff),
            "skill_bundle" => Some(ArtifactType::SkillBundle),
            "prompt_policy" => Some(ArtifactType::PromptPolicy),
            _ => None,
        }
    }
}

/// Materialized candidate content supporting both Stage 1 single-file evolution
/// and Stage 2 multi-file Skill Bundles without breaking the contract layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ArtifactPayload {
    SingleFile(String),
    FileMap(BTreeMap<String, String>),
}

impl ArtifactPayload {
    pub fn media_type(&self) -> &'static str {
        match self {
            ArtifactPayload::SingleFile(_) => "text/plain",
            ArtifactPayload::FileMap(_) => "application/vnd.evolve.filemap+json",
        }
    }
}

/// Deterministic verifier specification.
/// When `is_holdout == true`, execution MUST run in an isolated read-only context
/// inaccessible to candidate code, and diagnostic output MUST NOT leak to prompts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifierSpec {
    pub verifier_id: String,
    pub cmd: Vec<String>,
    pub is_holdout: bool,
}

/// Proof emitted exclusively by an external deterministic verifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mark {
    pub candidate_id: String,
    pub verifier_id: String,
    pub passed: bool,
    /// Normalized primary fitness (strictly higher is better) used by MAP-Elites.
    pub primary_score: f64,
    /// Raw multi-objective scalar metrics (e.g., accuracy, latency_ms, loc).
    pub metrics: BTreeMap<String, f64>,
    /// Deterministic discretized behavioral coordinate (e.g., "lat_bin:2|loc_bin:1").
    pub cell_key: String,
    pub is_holdout: bool,
    pub artifact_sha256: String,
    /// Truncated compile/test diagnostics. MUST be `None` when `is_holdout == true`.
    pub public_feedback: Option<String>,
}

/// Materialized snapshot surfaced to the outer mutation loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateSnapshot {
    pub id: String,
    pub island_id: u32,
    pub base_commit_sha: String,
    pub artifact_type: ArtifactType,
    pub payload: ArtifactPayload,
    pub primary_score: f64,
    pub metrics: BTreeMap<String, f64>,
    pub cell_key: String,
    pub last_public_feedback: Option<String>,
}

/// AlphaEvolve-style Parent + Inspiration prompt context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptContext {
    pub parent: CandidateSnapshot,
    pub inspirations: Vec<CandidateSnapshot>,
}

/// Two-phase atomic budget reservation handle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetReservation {
    pub reservation_id: String,
    pub run_id: String,
    pub reserved_micro_usd: MicroUsd,
}

/// Cost-aware model routing tier for Mode-A in-process LLM clients.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmTier {
    Fast,
    Strong,
}

impl LlmTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmTier::Fast => "fast",
            LlmTier::Strong => "strong",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub run_id: String,
    pub tier: LlmTier,
    pub system_prompt: String,
    pub user_prompt: String,
    pub max_output_tokens: u32,
    pub estimated_cost_micro_usd: MicroUsd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    pub model_id: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub actual_cost_micro_usd: MicroUsd,
}
