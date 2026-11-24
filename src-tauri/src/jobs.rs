use crate::security::{compress_output, validate_command};
use crate::state::AppState;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    Killed,
    TimedOut,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobSummary {
    pub job_id: String,
    pub command: String,
    pub label: Option<String>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub elapsed_secs: u64,
    pub line_count: usize,
}

pub struct ManagedJob {
    pub job_id: String,
    pub command: String,
    pub label: Option<String>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub started_instant: Instant,
    pub finished_elapsed_secs: Option<u64>,
    pub lines: Vec<String>,
    pub total_lines_seen: usize,
    pub pid: Option<u32>,
    pub kill_tx: Option<oneshot::Sender<()>>,
}

impl ManagedJob {
    fn to_summary(&self) -> JobSummary {
        let elapsed_secs = self
            .finished_elapsed_secs
            .unwrap_or_else(|| self.started_instant.elapsed().as_secs());
        JobSummary {
            job_id: self.job_id.clone(),
            command: self.command.clone(),
            label: self.label.clone(),
            status: self.status,
            exit_code: self.exit_code,
            started_at: self.started_at.clone(),
            elapsed_secs,
            line_count: self.total_lines_seen,
        }
    }
}

pub struct JobManager {
    pub jobs: Arc<RwLock<HashMap<String, ManagedJob>>>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

fn kill_pid_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = pid;
    }
}

fn clean_log_line(raw: &str, ansi_re: &Regex) -> Option<String> {
    let stripped = ansi_re.replace_all(raw, "");
    let final_seg = stripped.rsplit('\r').next().unwrap_or("");
    let trimmed = final_seg.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn emit_job_update(app_handle: &Option<AppHandle>, summary: &JobSummary) {
    if let Some(app) = app_handle {
        let _ = app.emit("job-updated", summary);
    }
}

pub async fn start_job(
    state: &AppState,
    root: &Path,
    command: &str,
    label: Option<String>,
    timeout_secs: u64,
) -> Result<JobSummary, String> {
    validate_command(command)?;
    let effective_timeout = timeout_secs.clamp(5, 1800);

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };

    cmd.current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Gagal memulai background job '{command}': {e}"))?;

    let pid = child.id();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let token = AppState::generate_token();
    let job_id = format!("job-{}", &token[..8]);
    let started_at = AppState::now_timestamp();
    let (kill_tx, kill_rx) = oneshot::channel::<()>();

    let managed = ManagedJob {
        job_id: job_id.clone(),
        command: command.to_string(),
        label: label.clone(),
        status: JobStatus::Running,
        exit_code: None,
        started_at,
        started_instant: Instant::now(),
        finished_elapsed_secs: None,
        lines: Vec::new(),
        total_lines_seen: 0,
        pid,
        kill_tx: Some(kill_tx),
    };

    let summary = managed.to_summary();
    let jobs_map = state.jobs.jobs.clone();
    {
        let mut guard = jobs_map.write().await;
        guard.insert(job_id.clone(), managed);
    }

    let app_handle = state.app_handle.read().await.clone();
    emit_job_update(&app_handle, &summary);

    let ansi_re = Regex::new(r"\x1B\[[0-9;?]*[ -/]*[@-~]").expect("valid ANSI regex");

    if let Some(out) = stdout_pipe {
        let map_out = jobs_map.clone();
        let jid = job_id.clone();
        let re = ansi_re.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(cleaned) = clean_log_line(&line, &re) {
                    let mut guard = map_out.write().await;
                    if let Some(job) = guard.get_mut(&jid) {
                        job.total_lines_seen += 1;
                        job.lines.push(cleaned);
                        if job.lines.len() > 2000 {
                            let excess = job.lines.len() - 2000;
                            job.lines.drain(0..excess);
                        }
                    }
                }
            }
        });
    }

    if let Some(err) = stderr_pipe {
        let map_err = jobs_map.clone();
        let jid = job_id.clone();
        let re = ansi_re;
        tokio::spawn(async move {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(cleaned) = clean_log_line(&line, &re) {
                    let mut guard = map_err.write().await;
                    if let Some(job) = guard.get_mut(&jid) {
                        job.total_lines_seen += 1;
                        job.lines.push(format!("[stderr] {cleaned}"));
                        if job.lines.len() > 2000 {
                            let excess = job.lines.len() - 2000;
                            job.lines.drain(0..excess);
                        }
                    }
                }
            }
        });
    }

    let jid_watch = job_id.clone();
    tokio::spawn(async move {
        let sleep_fut = tokio::time::sleep(Duration::from_secs(effective_timeout));
        tokio::pin!(sleep_fut);

        let (final_status, exit_code) = tokio::select! {
            wait_res = child.wait() => {
                match wait_res {
                    Ok(st) => {
                        let code = st.code().unwrap_or(-1);
                        let status = if st.success() { JobStatus::Completed } else { JobStatus::Failed };
                        (status, Some(code))
                    }
                    Err(_) => (JobStatus::Failed, Some(-1)),
                }
            }
            _ = kill_rx => {
                if let Some(p) = child.id() {
                    kill_pid_tree(p);
                }
                let _ = child.start_kill();
                (JobStatus::Killed, None)
            }
            _ = &mut sleep_fut => {
                if let Some(p) = child.id() {
                    kill_pid_tree(p);
                }
                let _ = child.start_kill();
                (JobStatus::TimedOut, None)
            }
        };

        // Brief grace period so reader tasks flush final buffered lines
        tokio::time::sleep(Duration::from_millis(80)).await;

        let updated_summary = {
            let mut guard = jobs_map.write().await;
            if let Some(job) = guard.get_mut(&jid_watch) {
                job.status = final_status;
                job.exit_code = exit_code;
                job.finished_elapsed_secs = Some(job.started_instant.elapsed().as_secs());
                job.kill_tx = None;
                Some(job.to_summary())
            } else {
                None
            }
        };
        if let Some(sum) = updated_summary {
            emit_job_update(&app_handle, &sum);
        }
    });

    Ok(summary)
}

pub async fn poll_job(
    state: &AppState,
    job_id: &str,
    from_line: usize,
    max_lines: usize,
) -> Result<String, String> {
    let effective_max = max_lines.clamp(12, 250);
    let (summary, slice_text, next_from_line) = {
        let guard = state.jobs.jobs.read().await;
        let job = guard
            .get(job_id)
            .ok_or_else(|| format!("Background job '{job_id}' tidak ditemukan."))?;

        let evicted = job.total_lines_seen.saturating_sub(job.lines.len());
        let local_start = from_line.saturating_sub(evicted).min(job.lines.len());
        let raw_slice = job.lines[local_start..].join("\n");
        let (compressed, _) = compress_output(&raw_slice, "", effective_max);
        (job.to_summary(), compressed, job.total_lines_seen)
    };

    let status_str = match summary.status {
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Killed => "killed",
        JobStatus::TimedOut => "timed_out",
    };

    Ok(format!(
        "JOB_ID: {}\nCOMMAND: {}\nSTATUS: {}\nEXIT_CODE: {}\nELAPSED: {}s\nNEXT_FROM_LINE: {}\nOUTPUT:\n{}",
        summary.job_id,
        summary.command,
        status_str,
        summary
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string()),
        summary.elapsed_secs,
        next_from_line,
        slice_text
    ))
}

pub async fn stop_job(state: &AppState, job_id: &str) -> Result<JobSummary, String> {
    let (pid_opt, kill_tx, summary_opt) = {
        let mut guard = state.jobs.jobs.write().await;
        let job = guard
            .get_mut(job_id)
            .ok_or_else(|| format!("Background job '{job_id}' tidak ditemukan."))?;
        let pid = job.pid;
        let tx = job.kill_tx.take();
        if job.status == JobStatus::Running {
            job.status = JobStatus::Killed;
            job.finished_elapsed_secs = Some(job.started_instant.elapsed().as_secs());
        }
        (pid, tx, Some(job.to_summary()))
    };

    if let Some(tx) = kill_tx {
        let _ = tx.send(());
    }
    if let Some(pid) = pid_opt {
        kill_pid_tree(pid);
    }

    let summary = summary_opt.expect("job summary present");
    let app_handle = state.app_handle.read().await.clone();
    emit_job_update(&app_handle, &summary);
    Ok(summary)
}

pub async fn list_jobs(state: &AppState) -> Vec<JobSummary> {
    let guard = state.jobs.jobs.read().await;
    let mut items: Vec<JobSummary> = guard.values().map(ManagedJob::to_summary).collect();
    items.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| a.job_id.cmp(&b.job_id)));
    items
}

pub async fn get_job_logs(state: &AppState, job_id: &str) -> Result<String, String> {
    let guard = state.jobs.jobs.read().await;
    let job = guard
        .get(job_id)
        .ok_or_else(|| format!("Background job '{job_id}' tidak ditemukan."))?;
    if job.lines.is_empty() {
        Ok("(belum ada output log)".to_string())
    } else {
        Ok(job.lines.join("\n"))
    }
}

pub async fn clear_finished_jobs(state: &AppState) {
    let mut guard = state.jobs.jobs.write().await;
    guard.retain(|_, j| j.status == JobStatus::Running);
}

pub async fn stop_all_jobs(state: &AppState) {
    let running_ids: Vec<String> = {
        let guard = state.jobs.jobs.read().await;
        guard
            .iter()
            .filter(|(_, j)| j.status == JobStatus::Running)
            .map(|(k, _)| k.clone())
            .collect()
    };
    for id in running_ids {
        let _ = stop_job(state, &id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn test_async_job_start_poll_and_complete() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_async_job_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let summary = start_job(
            &state,
            &temp_dir,
            "echo hello_async_job",
            Some("Echo Test".to_string()),
            30,
        )
        .await
        .unwrap();
        assert_eq!(summary.status, JobStatus::Running);

        let mut completed = false;
        let mut last_poll = String::new();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            last_poll = poll_job(&state, &summary.job_id, 0, 80).await.unwrap();
            if last_poll.contains("STATUS: completed") {
                completed = true;
                break;
            }
        }

        assert!(completed, "Job did not complete cleanly: {last_poll}");
        assert!(last_poll.contains("EXIT_CODE: 0"));
        assert!(last_poll.contains("hello_async_job"));

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
