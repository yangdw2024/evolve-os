use core_contracts::ContractError;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

const MAX_CAPTURE_BYTES: usize = 1 << 20;

pub(crate) struct ProcOutcome {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

async fn drain<R: AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    kept
}

async fn collect(task: Option<JoinHandle<Vec<u8>>>) -> String {
    let Some(task) = task else { return String::new() };
    let abort = task.abort_handle();
    match tokio::time::timeout(Duration::from_secs(2), task).await {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        _ => {
            abort.abort();
            String::new()
        }
    }
}

/// Kills the whole process tree rooted at `pid` (the candidate may spawn grandchildren).
async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

/// Runs `cmd` with a cleared environment, a fixed cwd and a hard wall-clock limit.
/// Exceeding the limit kills the process tree and yields `PolicyViolation`.
pub(crate) async fn run_limited(
    cmd: &[String],
    cwd: &Path,
    env: &[(String, String)],
    wall: Duration,
    capture_stderr: bool,
) -> Result<ProcOutcome, ContractError> {
    let (program, args) = cmd
        .split_first()
        .ok_or_else(|| ContractError::Internal("empty command".into()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr { Stdio::piped() } else { Stdio::null() })
        .kill_on_drop(true);
    for (k, v) in env {
        command.env(k, v);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| ContractError::Internal(format!("failed to spawn `{program}`: {e}")))?;
    let pid = child.id();
    let out_task = child.stdout.take().map(|s| tokio::spawn(drain(s)));
    let err_task = child.stderr.take().map(|s| tokio::spawn(drain(s)));

    let waited = tokio::time::timeout(wall, child.wait()).await;
    match waited {
        Ok(Ok(status)) => Ok(ProcOutcome {
            success: status.success(),
            code: status.code(),
            stdout: collect(out_task).await,
            stderr: collect(err_task).await,
        }),
        Ok(Err(e)) => {
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            Err(ContractError::NodeTransient(format!("waiting for `{program}` failed: {e}")))
        }
        Err(_) => {
            // Kill the tree while the root is still alive so descendants can be found.
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.kill().await;
            for t in [out_task, err_task].into_iter().flatten() {
                t.abort();
            }
            Err(ContractError::PolicyViolation(format!(
                "`{program}` exceeded max_wall_secs={}s and was killed",
                wall.as_secs()
            )))
        }
    }
}
