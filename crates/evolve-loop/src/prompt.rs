use core_contracts::{ArtifactPayload, CandidateSnapshot, PromptContext};
use std::collections::BTreeMap;

pub const PARENT_FILE_HEADER: &str = "### PARENT FILE: ";
pub const REPAIR_TAG: &str = "[FORMAT REPAIR REQUEST]";

pub const SYSTEM_PROMPT: &str = "You are a code mutation operator inside an evolutionary search.
Goal: maximize the verifier's primary_score for the PARENT program.
Output protocol (STRICT): respond ONLY with one or more blocks of the form

<<<<<<< SEARCH <relative/file/path>
<exact existing lines copied from the PARENT file>
=======
<replacement lines>
>>>>>>> REPLACE

Rules: SEARCH text must match the parent file exactly once. Only edit allowed files.
Keep changes small and focused. Do not explain.";

/// System prompt plus the bench's task/scoring spec (if any).
pub fn system_prompt(task_spec: Option<&str>) -> String {
    match task_spec {
        Some(spec) if !spec.trim().is_empty() => {
            format!("{SYSTEM_PROMPT}\n\n# TASK SPECIFICATION (scoring rules)\n{}", spec.trim())
        }
        _ => SYSTEM_PROMPT.to_string(),
    }
}

/// Default file name used when an artifact is a `SingleFile` payload.
pub const DEFAULT_FILE: &str = "solution.py";

pub fn payload_files(payload: &ArtifactPayload) -> BTreeMap<String, String> {
    match payload {
        ArtifactPayload::FileMap(m) => m.clone(),
        ArtifactPayload::SingleFile(s) => {
            let mut m = BTreeMap::new();
            m.insert(DEFAULT_FILE.to_string(), s.clone());
            m
        }
    }
}

fn lang(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("py") => "python",
        Some("rs") => "rust",
        Some("js") => "javascript",
        Some("md") => "markdown",
        Some("json") => "json",
        _ => "",
    }
}

fn fmt_metrics(c: &CandidateSnapshot) -> String {
    c.metrics
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_files(out: &mut String, header: &str, files: &BTreeMap<String, String>) {
    for (path, content) in files {
        out.push_str(&format!("{header}{path}\n```{}\n{}\n```\n\n", lang(path), content.trim_end()));
    }
}

/// Builds the Parent + Inspiration prompt. Only train-side data reaches here:
/// `PromptContext` is hydrated exclusively from non-holdout marks by the archive.
pub fn build_user_prompt(ctx: &PromptContext, crossover: bool) -> String {
    let p = &ctx.parent;
    let mut out = format!(
        "## PARENT candidate {} (island {}) primary_score={:.6} cell={}\nmetrics: {}\n",
        p.id,
        p.island_id,
        p.primary_score,
        p.cell_key,
        fmt_metrics(p)
    );
    if let Some(fb) = &p.last_public_feedback {
        out.push_str(&format!("verifier feedback: {fb}\n"));
    }
    out.push('\n');
    push_files(&mut out, PARENT_FILE_HEADER, &payload_files(&p.payload));

    for (i, insp) in ctx.inspirations.iter().enumerate() {
        out.push_str(&format!(
            "## INSPIRATION {} candidate {} (island {}) primary_score={:.6} cell={}\nmetrics: {}\n\n",
            i + 1,
            insp.id,
            insp.island_id,
            insp.primary_score,
            insp.cell_key,
            fmt_metrics(insp)
        ));
        push_files(&mut out, "### FILE: ", &payload_files(&insp.payload));
    }

    out.push_str("## TASK\n");
    if crossover {
        out.push_str("CROSSOVER ROUND: combine the strongest ideas of the PARENT and the INSPIRATIONS.\n");
    }
    out.push_str("Propose one improvement to the PARENT as SEARCH/REPLACE blocks.\n");
    out
}

pub fn build_repair_prompt(original_user_prompt: &str, bad_output: &str, error: &str) -> String {
    let mut bad = bad_output.to_string();
    if bad.len() > 1500 {
        let mut cut = 1500;
        while !bad.is_char_boundary(cut) {
            cut -= 1;
        }
        bad.truncate(cut);
    }
    format!(
        "{original_user_prompt}\n\n{REPAIR_TAG}\nYour previous response could not be applied: {error}\n\
         Previous response:\n{bad}\n\nRespond ONLY with corrected SEARCH/REPLACE blocks whose SEARCH text \
         matches the PARENT file exactly once."
    )
}
