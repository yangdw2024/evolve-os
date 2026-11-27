use crate::config::{Pricing, TierModelConfig};
use async_trait::async_trait;
use core_contracts::{ContractError, LlmClient, LlmRequest, LlmResponse};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Minimal OpenAI-compatible `/chat/completions` client (one per tier).
///
/// Error mapping:
/// - HTTP 429 / 5xx / timeout / connect failure -> `ContractError::NodeTransient`
/// - other non-2xx or malformed body             -> `ContractError::Internal`
pub struct OpenAiCompatClient {
    cfg: TierModelConfig,
    http: reqwest::Client,
    api_key: Option<String>,
    usage_fallbacks: AtomicU64,
}

impl OpenAiCompatClient {
    pub fn new(cfg: TierModelConfig) -> Result<Self, ContractError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
            .build()
            .map_err(|e| ContractError::Internal(format!("build http client: {e}")))?;
        let api_key = std::env::var(&cfg.api_key_env)
            .ok()
            .filter(|k| !k.trim().is_empty());
        Ok(Self { cfg, http, api_key, usage_fallbacks: AtomicU64::new(0) })
    }

    /// Number of responses whose `usage` was missing/incomplete (billed by estimate).
    pub fn usage_fallbacks(&self) -> u64 {
        self.usage_fallbacks.load(Ordering::SeqCst)
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn pricing(&self) -> Pricing {
        self.cfg.pricing()
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

fn map_transport_err(err: reqwest::Error) -> ContractError {
    if err.is_timeout() || err.is_connect() || err.is_request() || err.is_body() {
        ContractError::NodeTransient(format!("llm transport failure: {err}"))
    } else {
        ContractError::Internal(format!("llm http error: {err}"))
    }
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, ContractError> {
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let max_tokens = self
            .cfg
            .max_output_cap
            .map(|cap| req.max_output_tokens.min(cap))
            .unwrap_or(req.max_output_tokens);
        let mut body = serde_json::json!({
            "model": self.cfg.model,
            "messages": [
                {"role": "system", "content": req.system_prompt},
                {"role": "user", "content": req.user_prompt},
            ],
            "max_tokens": max_tokens,
        });
        if let Some(effort) = &self.cfg.reasoning_effort {
            body["reasoning_effort"] = serde_json::Value::String(effort.clone());
        }

        let mut builder = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder.send().await.map_err(map_transport_err)?;
        let status = resp.status();
        let text = resp.text().await.map_err(map_transport_err)?;

        if status.as_u16() == 429 || status.is_server_error() {
            return Err(ContractError::NodeTransient(format!(
                "upstream HTTP {status} from model {}",
                self.cfg.model
            )));
        }
        if !status.is_success() {
            return Err(ContractError::Internal(format!(
                "upstream HTTP {status}: {}",
                truncate(&text, 300)
            )));
        }

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| ContractError::Internal(format!("malformed completion body: {e}")))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| ContractError::Internal("completion has no choices/content".into()))?;

        let usage = parsed.usage;
        let reported_prompt = usage.as_ref().and_then(|u| u.prompt_tokens);
        let reported_completion = usage.as_ref().and_then(|u| u.completion_tokens);
        let prompt_tokens = reported_prompt.unwrap_or_else(|| {
            Pricing::estimate_tokens(req.system_prompt.len() + req.user_prompt.len())
        });
        // Missing completion usage: bill the conservative max(estimate(content), max_tokens/4)
        // because hidden reasoning tokens are invisible in `content`.
        let completion_tokens = reported_completion.unwrap_or_else(|| {
            Pricing::estimate_tokens(content.len()).max(max_tokens as u64 / 4)
        });
        if reported_prompt.is_none() || reported_completion.is_none() {
            let n = self.usage_fallbacks.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                eprintln!(
                    "warning: model `{}` returned no usage; billing with conservative token estimates",
                    self.cfg.model
                );
            }
        }

        Ok(LlmResponse {
            content,
            model_id: parsed.model.unwrap_or_else(|| self.cfg.model.clone()),
            prompt_tokens,
            completion_tokens,
            actual_cost_micro_usd: self.cfg.pricing().cost(prompt_tokens, completion_tokens),
        })
    }
}
