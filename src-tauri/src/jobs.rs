use crate::process_util::{build_shell_command, kill_pid_tree};
use crate::security::{ansi_regex, compress_output, validate_command_with_policy};
use crate::state::AppState;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, watch, RwLock};

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
    #[serde(default)]
    pub workspace_alias: Option<String>,
    #[serde(default)]
    pub mcp_task_id: Option<String>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub elapsed_secs: u64,
    #[serde(default)]
    pub timeout_secs: u64,
    pub line_count: usize,
    #[serde(default)]
    pub idle_secs: u64,
    #[serde(default)]
    pub last_line: Option<String>,
    #[serde(default)]
    pub stall_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobLogsDelta {
    pub summary: JobSummary,
    pub lines: Vec<String>,
    pub from_line: usize,
    pub next_from_line: usize,
    pub truncated_earlier: bool,
}

pub struct ManagedJob {
    pub job_id: String,
    pub command: String,
    pub label: Option<String>,
    pub workspace_alias: Option<String>,
    pub mcp_task_id: Option<String>,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub started_instant: Instant,
    pub last_output_instant: Instant,
    pub timeout_secs: u64,
    pub finished_elapsed_secs: Option<u64>,
    pub lines: Vec<String>,
    pub total_lines_seen: usize,
    pub pid: Option<u32>,
    pub kill_tx: Option<oneshot::Sender<()>>,
    pub done_rx: watch::Receiver<bool>,
}

fn truncate_preview_line(raw: &str, max_chars: usize) -> String {
    let trimmed = raw.trim();
    let mut out = String::new();
    let mut chars = trimmed.chars();
    for _ in 0..max_chars {
        match chars.next() {
            Some(c) => out.push(c),
            None => return out,
        }
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

pub(crate) fn detect_stall_or_lock_hint(lines: &[String], idle_secs: u64) -> Option<String> {
    let tail_joined = lines
        .iter()
        .rev()
        .take(6)
        .map(|s| s.to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    const LOCK_PATTERNS: &[&str] = &[
        "blocking waiting for file lock",
        "waiting for lock",
        "could not get lock",
        "being used by another process",
        "ebusy:",
    ];
    if LOCK_PATTERNS.iter().any(|p| tail_joined.contains(p)) {
        return Some(
            "Terdeteksi file/build lock contention (kemungkinan proses lain atau `tauri dev` sedang memakai folder target/)."
                .to_string(),
        );
    }

    const PROMPT_PATTERNS: &[&str] = &[
        "enter passphrase",
        "password for",
        "username for",
        "press any key",
        "[y/n]",
        "(y/n)",
        "are you sure you want to continue",
    ];
    if PROMPT_PATTERNS.iter().any(|p| tail_joined.contains(p)) {
        return Some(
            "Terdeteksi indikasi prompt interaktif meskipun stdin ditutup; pertimbangkan stop_command_job dan jalankan dengan flag non-interaktif."
                .to_string(),
        );
    }

    if idle_secs >= 25 {
        return Some(format!(
            "Tidak ada output baru selama {idle_secs}s (kemungkinan kompilasi berat, lock contention, atau proses macet)."
        ));
    }

    None
}

impl ManagedJob {
    fn to_summary(&self) -> JobSummary {
        let elapsed_secs = self
            .finished_elapsed_secs
            .unwrap_or_else(|| self.started_instant.elapsed().as_secs());
        let idle_secs = if self.status == JobStatus::Running {
            self.last_output_instant.elapsed().as_secs()
        } else {
            0
        };
        let last_line = self
            .lines
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| truncate_preview_line(l, 140));
        let stall_hint = if self.status == JobStatus::Running {
            detect_stall_or_lock_hint(&self.lines, idle_secs)
        } else {
            None
        };
        JobSummary {
            job_id: self.job_id.clone(),
            command: self.command.clone(),
            label: self.label.clone(),
            workspace_alias: self.workspace_alias.clone(),
            mcp_task_id: self.mcp_task_id.clone(),
            status: self.status,
            exit_code: self.exit_code,
            started_at: self.started_at.clone(),
            elapsed_secs,
            timeout_secs: self.timeout_secs,
            line_count: self.total_lines_seen,
            idle_secs,
            last_line,
            stall_hint,
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

fn spawn_pipe_reader<R>(
    pipe: Option<R>,
    jobs_map: Arc<RwLock<HashMap<String, ManagedJob>>>,
    job_id: String,
    ansi_re: &'static Regex,
    prefix: &'static str,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(stream) = pipe else {
        return;
    };
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(cleaned) = clean_log_line(&line, ansi_re) {
                let entry = if prefix.is_empty() {
                    cleaned
                } else {
                    format!("{prefix}{cleaned}")
                };
                let mut guard = jobs_map.write().await;
                if let Some(job) = guard.get_mut(&job_id) {
                    job.last_output_instant = Instant::now();
                    job.total_lines_seen += 1;
                    job.lines.push(entry);
                    if job.lines.len() > 2000 {
                        let excess = job.lines.len() - 2000;
                        job.lines.drain(0..excess);
                    }
                }
            }
        }
    });
}

pub async fn start_job(
    state: &AppState,
    root: &Path,
    command: &str,
    label: Option<String>,
    timeout_secs: u64,
) -> Result<JobSummary, String> {
    let (summary, _rx) =
        start_job_with_handle(state, root, command, label, timeout_secs, None).await?;
    Ok(summary)
}

pub async fn start_job_with_handle(
    state: &AppState,
    root: &Path,
    command: &str,
    label: Option<String>,
    timeout_secs: u64,
    workspace_alias: Option<String>,
) -> Result<(JobSummary, watch::Receiver<bool>), String> {
    let (cmd_policy, cmd_allowlist) = state.command_policy_and_allowlist().await;
    validate_command_with_policy(command, &cmd_policy, &cmd_allowlist)?;
    let effective_timeout = timeout_secs.clamp(5, 1800);

    let mut cmd = build_shell_command(root, command);

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
    let (done_tx, done_rx) = watch::channel(false);
    let mcp_task_id = crate::mcp_tasks::current_task_id();

    let started_instant = Instant::now();
    let managed = ManagedJob {
        job_id: job_id.clone(),
        command: command.to_string(),
        label: label.clone(),
        workspace_alias,
        mcp_task_id,
        status: JobStatus::Running,
        exit_code: None,
        started_at,
        started_instant,
        last_output_instant: started_instant,
        timeout_secs: effective_timeout,
        finished_elapsed_secs: None,
        lines: Vec::new(),
        total_lines_seen: 0,
        pid,
        kill_tx: Some(kill_tx),
        done_rx: done_rx.clone(),
    };

    let summary = managed.to_summary();
    let jobs_map = state.jobs.jobs.clone();
    {
        let mut guard = jobs_map.write().await;
        guard.insert(job_id.clone(), managed);
    }

    let app_handle = state.app_handle.read().await.clone();
    emit_job_update(&app_handle, &summary);

    if let Some(app_for_tick) = app_handle.clone() {
        let map_tick = jobs_map.clone();
        let jid_tick = job_id.clone();
        let mut done_tick = done_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let sum_opt = {
                            let guard = map_tick.read().await;
                            guard
                                .get(&jid_tick)
                                .filter(|j| j.status == JobStatus::Running)
                                .map(ManagedJob::to_summary)
                        };
                        if let Some(sum) = sum_opt {
                            let _ = app_for_tick.emit("job-updated", &sum);
                        } else {
                            break;
                        }
                    }
                    res = done_tick.changed() => {
                        if res.is_err() || *done_tick.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

    let ansi_re = ansi_regex();
    spawn_pipe_reader(stdout_pipe, jobs_map.clone(), job_id.clone(), ansi_re, "");
    spawn_pipe_reader(stderr_pipe, jobs_map.clone(), job_id.clone(), ansi_re, "[stderr] ");

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
        let _ = done_tx.send(true);
    });

    Ok((summary, done_rx))
}

pub async fn get_job_snapshot_output(
    state: &AppState,
    job_id: &str,
    max_lines: usize,
) -> Option<(JobStatus, Option<i32>, u64, String, usize)> {
    let effective_max = max_lines.clamp(12, 250);
    let guard = state.jobs.jobs.read().await;
    let job = guard.get(job_id)?;
    let raw = job.lines.join("\n");
    let (compressed, _) = compress_output(&raw, "", effective_max);
    let elapsed_secs = job
        .finished_elapsed_secs
        .unwrap_or_else(|| job.started_instant.elapsed().as_secs());
    Some((
        job.status,
        job.exit_code,
        elapsed_secs,
        compressed,
        job.total_lines_seen,
    ))
}

pub async fn get_job_workspace_alias(state: &AppState, job_id: &str) -> Option<String> {
    let guard = state.jobs.jobs.read().await;
    guard
        .get(job_id)
        .and_then(|j| j.workspace_alias.clone())
}

pub async fn subscribe_job_by_mcp_task(
    state: &AppState,
    task_id: &str,
) -> Option<(String, watch::Receiver<bool>)> {
    let guard = state.jobs.jobs.read().await;
    guard
        .values()
        .find(|j| j.mcp_task_id.as_deref() == Some(task_id))
        .map(|j| (j.job_id.clone(), j.done_rx.clone()))
}

pub async fn find_job_id_by_mcp_task(state: &AppState, task_id: &str) -> Option<String> {
    let guard = state.jobs.jobs.read().await;
    guard
        .values()
        .find(|j| j.mcp_task_id.as_deref() == Some(task_id))
        .map(|j| j.job_id.clone())
}

pub async fn poll_job(
    state: &AppState,
    job_id: &str,
    from_line: usize,
    max_lines: usize,
) -> Result<String, String> {
    poll_job_with_wait(state, job_id, from_line, max_lines, 0).await
}

pub async fn poll_job_with_wait(
    state: &AppState,
    job_id: &str,
    from_line: usize,
    max_lines: usize,
    wait_secs: u64,
) -> Result<String, String> {
    let effective_wait = wait_secs.min(18);
    if effective_wait > 0 {
        let wait_target = {
            let guard = state.jobs.jobs.read().await;
            let job = guard
                .get(job_id)
                .ok_or_else(|| format!("Background job '{job_id}' tidak ditemukan."))?;
            if job.status == JobStatus::Running {
                Some(job.done_rx.clone())
            } else {
                None
            }
        };
        if let Some(mut done_rx) = wait_target {
            let _ = tokio::time::timeout(
                Duration::from_secs(effective_wait),
                done_rx.wait_for(|&d| d),
            )
            .await;
        }
    }

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

    let ws_line = summary
        .workspace_alias
        .as_deref()
        .map(|a| format!("\nWORKSPACE: @{a}"))
        .unwrap_or_default();
    let stall_line = summary
        .stall_hint
        .as_deref()
        .map(|w| format!("\nSTALL_WARNING: {w}"))
        .unwrap_or_default();
    let next_hint = if summary.status == JobStatus::Running {
        format!(
            "\nHINT: Still running. Call poll_command_job(job_id=\"{}\", from_line={next_from_line}) directly (waits up to 12s automatically; do NOT run sleep) or stop_command_job to cancel.",
            summary.job_id
        )
    } else {
        String::new()
    };

    Ok(format!(
        "JOB_ID: {}{ws_line}\nCOMMAND: {}\nSTATUS: {}\nEXIT_CODE: {}\nELAPSED: {}s / {}s\nIDLE_SECS: {}s\nNEXT_FROM_LINE: {}{}{}\nOUTPUT:\n{}",
        summary.job_id,
        summary.command,
        status_str,
        summary
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string()),
        summary.elapsed_secs,
        summary.timeout_secs,
        summary.idle_secs,
        next_from_line,
        stall_line,
        next_hint,
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

pub async fn get_job_logs_delta(
    state: &AppState,
    job_id: &str,
    from_line: usize,
) -> Result<JobLogsDelta, String> {
    let guard = state.jobs.jobs.read().await;
    let job = guard
        .get(job_id)
        .ok_or_else(|| format!("Background job '{job_id}' tidak ditemukan."))?;
    let evicted = job.total_lines_seen.saturating_sub(job.lines.len());
    let truncated_earlier = from_line < evicted;
    let effective_from = from_line.max(evicted);
    let local_start = effective_from.saturating_sub(evicted).min(job.lines.len());
    Ok(JobLogsDelta {
        summary: job.to_summary(),
        lines: job.lines[local_start..].to_vec(),
        from_line: effective_from,
        next_from_line: job.total_lines_seen,
        truncated_earlier,
    })
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

    #[tokio::test]
    async fn test_auto_promote_fast_command_returns_sync() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_job_fast_sync_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let (summary, mut done_rx) = start_job_with_handle(
            &state,
            &temp_dir,
            "echo fast_sync_ok",
            Some("Fast Command".to_string()),
            60,
            Some("api".to_string()),
        )
        .await
        .unwrap();

        assert_eq!(summary.workspace_alias.as_deref(), Some("api"));
        let wait_res = tokio::time::timeout(Duration::from_secs(5), done_rx.wait_for(|&d| d)).await;
        assert!(wait_res.is_ok(), "Fast command should notify done_rx quickly");

        let snap = get_job_snapshot_output(&state, &summary.job_id, 60)
            .await
            .unwrap();
        assert_eq!(snap.0, JobStatus::Completed);
        assert_eq!(snap.1, Some(0));
        assert!(snap.3.contains("fast_sync_ok"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_auto_promote_slow_command_hands_off_to_job_manager() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_job_slow_promote_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let slow_cmd = if cfg!(target_os = "windows") {
            "ping -n 5 127.0.0.1"
        } else {
            "sleep 4"
        };

        let (summary, mut done_rx) = start_job_with_handle(
            &state,
            &temp_dir,
            slow_cmd,
            Some("Slow Build".to_string()),
            60,
            Some("web".to_string()),
        )
        .await
        .unwrap();

        // Waiting 300ms must time out without killing the job
        let wait_res =
            tokio::time::timeout(Duration::from_millis(300), done_rx.wait_for(|&d| d)).await;
        assert!(wait_res.is_err(), "Slow command should exceed short promote window");

        let snap = get_job_snapshot_output(&state, &summary.job_id, 20)
            .await
            .unwrap();
        assert_eq!(snap.0, JobStatus::Running);
        assert_eq!(
            get_job_workspace_alias(&state, &summary.job_id).await.as_deref(),
            Some("web")
        );

        let stopped = stop_job(&state, &summary.job_id).await.unwrap();
        assert_eq!(stopped.status, JobStatus::Killed);
        stop_all_jobs(&state).await;

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_job_summary_preview_and_logs_delta() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_job_delta_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let cmd = if cfg!(target_os = "windows") {
            "echo line_one & echo line_two"
        } else {
            "echo line_one; echo line_two"
        };
        let summary = start_job(&state, &temp_dir, cmd, Some("Delta Test".to_string()), 120)
            .await
            .unwrap();
        assert_eq!(summary.timeout_secs, 120);

        let mut completed = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let list = list_jobs(&state).await;
            if let Some(item) = list.iter().find(|j| j.job_id == summary.job_id) {
                if item.status == JobStatus::Completed {
                    completed = true;
                    assert_eq!(item.timeout_secs, 120);
                    assert_eq!(item.idle_secs, 0);
                    assert!(
                        item.last_line
                            .as_deref()
                            .unwrap_or_default()
                            .contains("line_two"),
                        "Expected last_line preview to include line_two, got {:?}",
                        item.last_line
                    );
                    break;
                }
            }
        }
        assert!(completed, "Job did not finish in time");

        let d1 = get_job_logs_delta(&state, &summary.job_id, 0).await.unwrap();
        assert!(!d1.truncated_earlier);
        assert!(d1.next_from_line >= 2);
        assert!(d1.lines.iter().any(|l| l.contains("line_one")));
        assert!(d1.lines.iter().any(|l| l.contains("line_two")));

        let d2 = get_job_logs_delta(&state, &summary.job_id, d1.next_from_line)
            .await
            .unwrap();
        assert!(d2.lines.is_empty());
        assert_eq!(d2.next_from_line, d1.next_from_line);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_poll_job_with_wait_and_non_interactive_env() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_job_wait_env_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let state = AppState::new(temp_dir.clone());
        let env_cmd = if cfg!(target_os = "windows") {
            "echo CI=%CI% GIT=%GIT_TERMINAL_PROMPT% PY=%PYTHONUNBUFFERED%"
        } else {
            "echo CI=$CI GIT=$GIT_TERMINAL_PROMPT PY=$PYTHONUNBUFFERED"
        };
        let summary = start_job(&state, &temp_dir, env_cmd, Some("Env Check".to_string()), 60)
            .await
            .unwrap();

        let started = Instant::now();
        let polled = poll_job_with_wait(&state, &summary.job_id, 0, 80, 12)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "Long-poll should wake immediately when job completes"
        );
        assert!(polled.contains("STATUS: completed"), "got:\n{polled}");
        assert!(polled.contains("CI=true"), "got:\n{polled}");
        assert!(polled.contains("GIT=0"), "got:\n{polled}");
        assert!(polled.contains("PY=1"), "got:\n{polled}");
        assert!(polled.contains("IDLE_SECS: 0s"), "got:\n{polled}");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_detect_stall_or_lock_hint() {
        let lock_lines = vec![
            "    Blocking waiting for file lock on build directory".to_string(),
        ];
        let lock_hint = detect_stall_or_lock_hint(&lock_lines, 3).unwrap();
        assert!(lock_hint.contains("lock contention"));

        let prompt_lines = vec!["Enter passphrase for key '/home/user/.ssh/id_ed25519':".to_string()];
        let prompt_hint = detect_stall_or_lock_hint(&prompt_lines, 1).unwrap();
        assert!(prompt_hint.contains("prompt interaktif"));

        let quiet_lines = vec!["Compiling serde v1.0.229".to_string()];
        assert!(detect_stall_or_lock_hint(&quiet_lines, 10).is_none());
        let idle_hint = detect_stall_or_lock_hint(&quiet_lines, 30).unwrap();
        assert!(idle_hint.contains("30s"));
    }
}
