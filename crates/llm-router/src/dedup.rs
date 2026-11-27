use crate::config::DedupConfig;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DedupStats {
    pub chunks_total: usize,
    pub chunks_deduped: usize,
    pub bytes_saved: usize,
    pub truncated: bool,
}

/// Lightweight SHA-256 fragment deduplication.
///
/// The user prompt is segmented into paragraphs and whole fenced code blocks; any
/// fragment (>= `min_chunk_chars`) whose normalized SHA-256 was already seen (in the
/// system prompt or earlier in the user prompt) is replaced by a short marker.
/// The first occurrence is always kept verbatim, so the parent code stays exact.
#[derive(Debug, Clone)]
pub struct PromptDeduper {
    min_chunk_chars: usize,
    max_prompt_bytes: Option<usize>,
}

impl Default for PromptDeduper {
    fn default() -> Self {
        Self::new(&DedupConfig::default())
    }
}

impl PromptDeduper {
    pub fn new(cfg: &DedupConfig) -> Self {
        Self {
            min_chunk_chars: cfg.min_chunk_chars.max(1),
            max_prompt_bytes: cfg.max_prompt_bytes,
        }
    }

    pub fn dedup(&self, system_prompt: &str, user_prompt: &str) -> (String, DedupStats) {
        let mut stats = DedupStats::default();
        let mut seen: HashSet<String> = HashSet::new();
        for chunk in segment(system_prompt) {
            let norm = normalize(&chunk);
            if norm.chars().count() >= self.min_chunk_chars {
                seen.insert(sha256_hex(&norm));
            }
        }

        let mut out: Vec<String> = Vec::new();
        for chunk in segment(user_prompt) {
            stats.chunks_total += 1;
            let norm = normalize(&chunk);
            if norm.chars().count() >= self.min_chunk_chars {
                let digest = sha256_hex(&norm);
                if !seen.insert(digest.clone()) {
                    let marker = format!("[duplicate fragment sha256:{} omitted]", &digest[..12]);
                    stats.chunks_deduped += 1;
                    stats.bytes_saved += chunk.len().saturating_sub(marker.len());
                    out.push(marker);
                    continue;
                }
            }
            out.push(chunk);
        }

        let mut joined = out.join("\n\n");
        if let Some(max) = self.max_prompt_bytes {
            if joined.len() > max {
                let mut cut = max;
                while cut > 0 && !joined.is_char_boundary(cut) {
                    cut -= 1;
                }
                stats.bytes_saved += joined.len() - cut;
                joined.truncate(cut);
                joined.push_str("\n[... prompt truncated by router ...]");
                stats.truncated = true;
            }
        }
        (joined, stats)
    }
}

fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn normalize(chunk: &str) -> String {
    chunk
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Splits into blank-line separated paragraphs, keeping each fenced block as one chunk.
fn segment(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n");
    let mut chunks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;

    let flush = |current: &mut Vec<&str>, chunks: &mut Vec<String>| {
        if current.iter().any(|l| !l.trim().is_empty()) {
            chunks.push(current.join("\n"));
        }
        current.clear();
    };

    for line in text.lines() {
        let is_fence = line.trim_start().starts_with("```");
        if in_fence {
            current.push(line);
            if is_fence {
                in_fence = false;
                flush(&mut current, &mut chunks);
            }
        } else if is_fence {
            flush(&mut current, &mut chunks);
            current.push(line);
            in_fence = true;
        } else if line.trim().is_empty() {
            flush(&mut current, &mut chunks);
        } else {
            current.push(line);
        }
    }
    flush(&mut current, &mut chunks);
    chunks
}
