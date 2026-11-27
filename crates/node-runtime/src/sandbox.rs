use crate::patch::{apply_patch, files_sha256, validate_rel_path};
use crate::process::{run_limited, ProcOutcome};
use async_trait::async_trait;
use core_contracts::{ContractError, ExecutionNode, Mark, PolicyGrant, VerifierSpec};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct LocalSandboxConfig {
    pub node_id: String,
    /// Immutable content-addressed snapshots (`<root>/<sha256>/...`).
    pub snapshot_root: PathBuf,
    /// Parent of per-evaluation temporary sandboxes.
    pub work_root: PathBuf,
    /// Optional Stage-1 build command, executed with cwd = candidate work dir.
    pub build_cmd: Option<Vec<String>>,
    pub keep_workdirs: bool,
    pub max_feedback_bytes: usize,
    pub max_artifact_bytes: u64,
}

impl LocalSandboxConfig {
    pub fn new(node_id: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            node_id: node_id.into(),
            snapshot_root: root.join("snapshots"),
            work_root: root.join("work"),
            build_cmd: None,
            keep_workdirs: false,
            max_feedback_bytes: 2_000,
            max_artifact_bytes: 16 * 1024 * 1024,
        }
    }
}

pub struct LocalSandboxNode {
    cfg: LocalSandboxConfig,
}

/// Standard verifier stdout contract (last non-empty stdout line is JSON).
#[derive(Debug, Deserialize)]
struct VerifierOutput {
    passed: bool,
    primary_score: f64,
    #[serde(default)]
    metrics: BTreeMap<String, f64>,
    #[serde(default)]
    cell_key: Option<String>,
    #[serde(default)]
    feedback: Option<String>,
    #[serde(default)]
    timeout: bool,
    #[serde(default)]
    policy_violation: Option<String>,
}

fn io_transient(ctx: &str, e: std::io::Error) -> ContractError {
    ContractError::NodeTransient(format!("{ctx}: {e}"))
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[truncated]", &text[..cut])
}

fn path_allowed(path: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| {
        let a = a.trim().trim_end_matches('/');
        a == "*" || a == "." || path == a || path.starts_with(&format!("{a}/"))
    })
}

/// Minimal environment: no inherited secrets (API keys etc.), private temp/home.
fn base_env(scratch: &Path, allow_network: bool) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for key in ["PATH", "SYSTEMROOT", "PATHEXT", "COMSPEC", "WINDIR"] {
        if let Ok(v) = std::env::var(key) {
            env.push((key.to_string(), v));
        }
    }
    let s = scratch.to_string_lossy().into_owned();
    for key in ["TEMP", "TMP", "TMPDIR", "HOME", "USERPROFILE"] {
        env.push((key.to_string(), s.clone()));
    }
    env.push(("PYTHONDONTWRITEBYTECODE".into(), "1".into()));
    env.push(("PYTHONHASHSEED".into(), "0".into()));
    if !allow_network {
        // Advisory only: steers well-behaved HTTP clients to a dead proxy. Not a kernel firewall.
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            env.push((key.to_string(), "http://127.0.0.1:9".into()));
        }
        env.push(("EVOLVE_NETWORK".into(), "deny".into()));
    }
    env
}

fn write_files(dir: &Path, files: &BTreeMap<String, String>) -> Result<(), ContractError> {
    for (rel, content) in files {
        let rel = validate_rel_path(rel)?;
        let path = dir.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_transient("create dir", e))?;
        }
        fs::write(&path, content).map_err(|e| io_transient("write file", e))?;
    }
    Ok(())
}

fn read_tree(root: &Path) -> Result<BTreeMap<String, String>, ContractError> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), ContractError> {
        for entry in fs::read_dir(dir).map_err(|e| io_transient("read snapshot dir", e))? {
            let entry = entry.map_err(|e| io_transient("read snapshot entry", e))?;
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else {
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| ContractError::Internal(format!("strip prefix: {e}")))?
                    .to_string_lossy()
                    .replace('\\', "/");
                let content = fs::read_to_string(&path).map_err(|e| io_transient("read snapshot file", e))?;
                out.insert(rel, content);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Rejects symlinks (sandbox escape) and oversized outputs after the untrusted build.
fn enforce_boundaries(dir: &Path, max_bytes: u64) -> Result<(), ContractError> {
    fn walk(dir: &Path, total: &mut u64, max: u64) -> Result<(), ContractError> {
        for entry in fs::read_dir(dir).map_err(|e| io_transient("scan sandbox", e))? {
            let entry = entry.map_err(|e| io_transient("scan sandbox entry", e))?;
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| io_transient("stat", e))?;
            if meta.file_type().is_symlink() {
                return Err(ContractError::PolicyViolation(format!(
                    "symlink `{}` created inside sandbox",
                    entry.file_name().to_string_lossy()
                )));
            }
            if meta.is_dir() {
                walk(&entry.path(), total, max)?;
            } else {
                *total += meta.len();
                if *total > max {
                    return Err(ContractError::PolicyViolation(format!(
                        "sandbox output exceeds {max} bytes"
                    )));
                }
            }
        }
        Ok(())
    }
    let mut total = 0;
    walk(dir, &mut total, max_bytes)
}

/// Copies the built artifact (minus bytecode caches) and marks every file read-only.
fn copy_tree_readonly(src: &Path, dst: &Path) -> Result<(), ContractError> {
    fs::create_dir_all(dst).map_err(|e| io_transient("create artifact dir", e))?;
    for entry in fs::read_dir(src).map_err(|e| io_transient("read work dir", e))? {
        let entry = entry.map_err(|e| io_transient("read work entry", e))?;
        let name = entry.file_name();
        if name == "__pycache__" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_tree_readonly(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| io_transient("copy artifact", e))?;
            let mut perms = fs::metadata(&to).map_err(|e| io_transient("stat artifact", e))?.permissions();
            perms.set_readonly(true);
            fs::set_permissions(&to, perms).map_err(|e| io_transient("chmod artifact", e))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dst, fs::Permissions::from_mode(0o555));
    }
    Ok(())
}

#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                make_writable(&path);
            } else if let Ok(meta) = fs::metadata(&path) {
                let mut perms = meta.permissions();
                perms.set_readonly(false);
                let _ = fs::set_permissions(&path, perms);
            }
        }
    }
}

/// Per-evaluation sandbox directory, removed on drop.
struct SandboxDir {
    path: PathBuf,
    keep: bool,
}

impl SandboxDir {
    fn create(root: &Path, candidate_id: &str, keep: bool) -> Result<Self, ContractError> {
        let safe: String = candidate_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .take(48)
            .collect();
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let path = root.join(format!("{safe}-{}", &suffix[..8]));
        fs::create_dir_all(&path).map_err(|e| io_transient("create sandbox", e))?;
        Ok(Self { path, keep })
    }

    fn sub(&self, name: &str) -> Result<PathBuf, ContractError> {
        let p = self.path.join(name);
        fs::create_dir_all(&p).map_err(|e| io_transient("create sandbox subdir", e))?;
        Ok(p)
    }
}

impl Drop for SandboxDir {
    fn drop(&mut self) {
        if !self.keep {
            make_writable(&self.path);
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl LocalSandboxNode {
    pub fn new(mut cfg: LocalSandboxConfig) -> Result<Self, ContractError> {
        // Child processes run with different cwds, so every path handed to them must be absolute.
        let abs = |p: &Path| std::path::absolute(p).map_err(|e| io_transient("resolve sandbox root", e));
        cfg.snapshot_root = abs(&cfg.snapshot_root)?;
        cfg.work_root = abs(&cfg.work_root)?;
        fs::create_dir_all(&cfg.snapshot_root).map_err(|e| io_transient("create snapshot root", e))?;
        fs::create_dir_all(&cfg.work_root).map_err(|e| io_transient("create work root", e))?;
        Ok(Self { cfg })
    }

    pub fn config(&self) -> &LocalSandboxConfig {
        &self.cfg
    }

    /// Stores an immutable snapshot and returns its content address (used as `base_commit_sha`).
    pub fn register_snapshot(&self, files: &BTreeMap<String, String>) -> Result<String, ContractError> {
        let mut normalized = BTreeMap::new();
        for (k, v) in files {
            normalized.insert(validate_rel_path(k)?, v.clone());
        }
        let sha = files_sha256(&normalized);
        let dir = self.cfg.snapshot_root.join(&sha);
        if dir.is_dir() {
            return Ok(sha);
        }
        let tmp = self
            .cfg
            .snapshot_root
            .join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&tmp).map_err(|e| io_transient("create snapshot tmp", e))?;
        write_files(&tmp, &normalized)?;
        if fs::rename(&tmp, &dir).is_err() {
            // Lost a race with an identical snapshot: content-addressed, so just discard ours.
            let _ = fs::remove_dir_all(&tmp);
            if !dir.is_dir() {
                return Err(ContractError::NodeTransient(format!("failed to publish snapshot {sha}")));
            }
        }
        Ok(sha)
    }

    pub fn load_snapshot(&self, sha: &str) -> Result<BTreeMap<String, String>, ContractError> {
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ContractError::NotFound(format!("invalid snapshot id `{sha}`")));
        }
        let dir = self.cfg.snapshot_root.join(sha);
        if !dir.is_dir() {
            return Err(ContractError::NotFound(format!("snapshot {sha} not registered")));
        }
        read_tree(&dir)
    }

    fn failure_mark(&self, candidate_id: &str, verifier: &VerifierSpec, sha: &str, cell: &str, feedback: String) -> Mark {
        Mark {
            candidate_id: candidate_id.to_string(),
            verifier_id: verifier.verifier_id.clone(),
            passed: false,
            primary_score: 0.0,
            metrics: BTreeMap::new(),
            cell_key: cell.to_string(),
            is_holdout: verifier.is_holdout,
            artifact_sha256: sha.to_string(),
            public_feedback: if verifier.is_holdout {
                None
            } else {
                Some(truncate(&feedback, self.cfg.max_feedback_bytes))
            },
        }
    }

    fn mark_from_output(
        &self,
        candidate_id: &str,
        verifier: &VerifierSpec,
        sha: &str,
        out: ProcOutcome,
    ) -> Result<Mark, ContractError> {
        let holdout = verifier.is_holdout;
        let parsed = out
            .stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .ok_or_else(|| "verifier produced no stdout".to_string())
            .and_then(|line| {
                serde_json::from_str::<VerifierOutput>(line.trim())
                    .map_err(|e| format!("verifier stdout is not valid metrics JSON: {e}"))
            });

        let v = match parsed {
            Ok(v) => v,
            Err(why) => {
                // `out.stderr` is always empty for holdout (stderr was never captured).
                let fb = format!("{why} (exit {:?})\n{}", out.code, out.stderr);
                return Ok(self.failure_mark(candidate_id, verifier, sha, "verifier_error", fb));
            }
        };

        if v.timeout {
            return Err(ContractError::PolicyViolation(
                "candidate exceeded its wall-clock limit inside the verifier".into(),
            ));
        }
        if let Some(reason) = v.policy_violation {
            return Err(ContractError::PolicyViolation(if holdout {
                "holdout verifier reported a policy violation".into()
            } else {
                reason
            }));
        }
        if !v.primary_score.is_finite() || v.metrics.values().any(|m| !m.is_finite()) {
            return Ok(self.failure_mark(candidate_id, verifier, sha, "verifier_error", "non-finite metric".into()));
        }

        Ok(Mark {
            candidate_id: candidate_id.to_string(),
            verifier_id: verifier.verifier_id.clone(),
            passed: v.passed && out.success,
            primary_score: v.primary_score,
            metrics: v.metrics,
            cell_key: v.cell_key.unwrap_or_else(|| "default".into()),
            is_holdout: holdout,
            artifact_sha256: sha.to_string(),
            // Holdout: verifier-provided feedback is dropped unconditionally.
            public_feedback: if holdout {
                None
            } else {
                v.feedback.map(|f| truncate(&f, self.cfg.max_feedback_bytes))
            },
        })
    }
}

#[async_trait]
impl ExecutionNode for LocalSandboxNode {
    fn node_id(&self) -> &str {
        &self.cfg.node_id
    }

    async fn eval_candidate(
        &self,
        candidate_id: &str,
        base_commit_sha: &str,
        patch_payload: &str,
        verifier: &VerifierSpec,
        grant: &PolicyGrant,
    ) -> Result<Mark, ContractError> {
        if grant.max_wall_secs == 0 {
            return Err(ContractError::PolicyViolation("max_wall_secs must be > 0".into()));
        }
        let wall = Duration::from_secs(grant.max_wall_secs);
        let base = self.load_snapshot(base_commit_sha)?;
        let (files, touched) = if patch_payload.trim().is_empty() {
            (base, Vec::new())
        } else {
            apply_patch(&base, patch_payload)?
        };
        if let Some(bad) = touched.iter().find(|p| !path_allowed(p, &grant.allowed_paths)) {
            return Err(ContractError::PolicyViolation(format!(
                "patch touches `{bad}` outside allowed_paths {:?}",
                grant.allowed_paths
            )));
        }
        let artifact_sha = files_sha256(&files);
        let sandbox = SandboxDir::create(&self.cfg.work_root, candidate_id, self.cfg.keep_workdirs)?;

        // ---- Stage 1: untrusted materialize + build.
        let work = sandbox.sub("work")?;
        write_files(&work, &files)?;
        if let Some(build) = &self.cfg.build_cmd {
            let env = base_env(&sandbox.sub("build-tmp")?, grant.allow_network);
            let out = run_limited(build, &work, &env, wall, true).await?;
            if !out.success {
                let fb = format!("build failed (exit {:?}):\n{}", out.code, out.stderr);
                return Ok(self.failure_mark(candidate_id, verifier, &artifact_sha, "build_failed", fb));
            }
        }
        enforce_boundaries(&work, self.cfg.max_artifact_bytes)?;

        // ---- Stage 2: read-only artifact + isolated verifier process.
        let artifact = sandbox.path.join("artifact");
        copy_tree_readonly(&work, &artifact)?;
        let verifier_cwd = sandbox.sub("verifier")?;
        let mut env = base_env(&verifier_cwd, false);
        env.push(("EVOLVE_ARTIFACT_DIR".into(), artifact.to_string_lossy().into_owned()));
        env.push(("EVOLVE_WALL_SECS".into(), grant.max_wall_secs.to_string()));
        env.push(("EVOLVE_IS_HOLDOUT".into(), if verifier.is_holdout { "1" } else { "0" }.into()));
        let out = run_limited(&verifier.cmd, &verifier_cwd, &env, wall, !verifier.is_holdout).await?;
        self.mark_from_output(candidate_id, verifier, &artifact_sha, out)
    }
}
