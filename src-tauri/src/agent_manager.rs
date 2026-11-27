use crate::state::{AppState, TurnCheckpointInfo};
use chrono::Local;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex, RwLock};

pub const MAX_CONCURRENT_RUNS: usize = 4;
pub const MAX_SUBAGENTS_PER_RUN: usize = 3;
const MAX_CACHED_SESSIONS: usize = 12;
const MAX_FINISHED_RUNS_RETAINED: usize = 16;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Running,
    AwaitingApproval,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRunSummary {
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub session_id: String,
    pub workspace_alias: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub execution_mode: String,
    pub status: AgentRunStatus,
    pub current_step: usize,
    pub max_steps: usize,
    pub last_activity_text: String,
    pub started_at: String,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone)]
pub struct RunContext {
    pub run_id: String,
    #[allow(dead_code)]
    pub parent_run_id: Option<String>,
    pub session_id: String,
    #[allow(dead_code)]
    pub workspace_id: String,
    pub workspace_alias: String,
    pub workspace_clean_path: String,
    pub execution_mode: String,
    pub depth: u8,
    pub turn_group_id: String,
    pub subagent_counter: Arc<AtomicUsize>,
    pub acp_event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::acp_server::AcpStreamEvent>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DelegatedRunResult {
    pub run_id: String,
    pub workspace_alias: String,
    pub mode: String,
    pub status: AgentRunStatus,
    pub summary_text: String,
    pub touched_files: Vec<String>,
    pub turn_group_id: Option<String>,
    pub step_count: usize,
    pub elapsed_secs: u64,
    pub pending_approval_hint: Option<String>,
}

struct AgentRunHandle {
    summary: AgentRunSummary,
    started_instant: Instant,
    cancel_tx: watch::Sender<bool>,
    #[allow(dead_code)]
    subagent_counter: Arc<AtomicUsize>,
    pinned_workspace_alias: Option<String>,
    status_tx: watch::Sender<AgentRunStatus>,
    pending_approval_hint: Option<String>,
    final_result: Option<DelegatedRunResult>,
}

pub struct AgentRunManager {
    runs: RwLock<HashMap<String, AgentRunHandle>>,
    workspace_write_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    gemini_histories: RwLock<HashMap<String, Vec<Value>>>,
    openai_histories: RwLock<HashMap<String, Vec<Value>>>,
    anthropic_histories: RwLock<HashMap<String, Vec<Value>>>,
    session_lru: RwLock<Vec<String>>,
    checkpoints: RwLock<HashMap<String, TurnCheckpointInfo>>,
}

impl Default for AgentRunManager {
    fn default() -> Self {
        Self {
            runs: RwLock::new(HashMap::new()),
            workspace_write_locks: RwLock::new(HashMap::new()),
            gemini_histories: RwLock::new(HashMap::new()),
            openai_histories: RwLock::new(HashMap::new()),
            anthropic_histories: RwLock::new(HashMap::new()),
            session_lru: RwLock::new(Vec::new()),
            checkpoints: RwLock::new(HashMap::new()),
        }
    }
}

impl AgentRunManager {
    pub async fn start_run(
        &self,
        parent_run_id: Option<String>,
        session_id: String,
        workspace_alias: String,
        label: String,
        provider: String,
        model: String,
        execution_mode: String,
        max_steps: usize,
    ) -> Result<(String, watch::Receiver<bool>, Arc<AtomicUsize>), String> {
        let mut runs = self.runs.write().await;
        let running_count = runs
            .values()
            .filter(|h| h.summary.status == AgentRunStatus::Running)
            .count();
        if running_count >= MAX_CONCURRENT_RUNS {
            return Err(format!(
                "Batas maksimum {MAX_CONCURRENT_RUNS} agent run paralel tercapai. Hentikan atau tunggu salah satu run selesai."
            ));
        }

        let run_id = format!("run_{}", &AppState::generate_token()[..8]);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (status_tx, _) = watch::channel(AgentRunStatus::Running);
        let subagent_counter = Arc::new(AtomicUsize::new(0));
        let clean_label: String = label.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(72).collect();

        let summary = AgentRunSummary {
            run_id: run_id.clone(),
            parent_run_id,
            session_id,
            workspace_alias,
            label: if clean_label.is_empty() {
                "Agent Turn".to_string()
            } else {
                clean_label
            },
            provider,
            model,
            execution_mode,
            status: AgentRunStatus::Running,
            current_step: 0,
            max_steps,
            last_activity_text: "Starting...".to_string(),
            started_at: Local::now().format("%H:%M:%S").to_string(),
            elapsed_secs: 0,
        };

        runs.insert(
            run_id.clone(),
            AgentRunHandle {
                summary,
                started_instant: Instant::now(),
                cancel_tx,
                subagent_counter: subagent_counter.clone(),
                pinned_workspace_alias: None,
                status_tx,
                pending_approval_hint: None,
                final_result: None,
            },
        );
        Ok((run_id, cancel_rx, subagent_counter))
    }

    pub async fn set_run_pinned_workspace(&self, run_id: &str, alias: Option<String>) {
        let mut runs = self.runs.write().await;
        if let Some(handle) = runs.get_mut(run_id) {
            handle.pinned_workspace_alias = alias;
        }
    }

    pub async fn get_run_max_steps(&self, run_id: &str) -> Option<usize> {
        let runs = self.runs.read().await;
        runs.get(run_id).map(|h| h.summary.max_steps)
    }

    pub async fn notify_run_status(
        &self,
        run_id: &str,
        status: AgentRunStatus,
        activity: String,
        pending_approval_hint: Option<String>,
    ) {
        let mut runs = self.runs.write().await;
        if let Some(handle) = runs.get_mut(run_id) {
            handle.summary.status = status.clone();
            handle.summary.last_activity_text = activity;
            handle.summary.elapsed_secs = handle.started_instant.elapsed().as_secs();
            handle.pending_approval_hint = pending_approval_hint;
            let _ = handle.status_tx.send(status);
        }
    }

    pub async fn update_run_step(
        &self,
        run_id: &str,
        step: usize,
        max_steps: usize,
        activity: String,
    ) {
        let mut runs = self.runs.write().await;
        if let Some(handle) = runs.get_mut(run_id) {
            handle.summary.current_step = step;
            handle.summary.max_steps = max_steps;
            handle.summary.last_activity_text = activity;
            handle.summary.elapsed_secs = handle.started_instant.elapsed().as_secs();
        }
    }

    fn trim_excess_finished_runs(runs: &mut HashMap<String, AgentRunHandle>) {
        if runs.len() > MAX_FINISHED_RUNS_RETAINED {
            let mut finished: Vec<(String, Instant)> = runs
                .iter()
                .filter(|(_, h)| h.summary.status != AgentRunStatus::Running)
                .map(|(k, h)| (k.clone(), h.started_instant))
                .collect();
            finished.sort_by_key(|(_, inst)| *inst);
            let excess = runs.len().saturating_sub(MAX_FINISHED_RUNS_RETAINED);
            for (k, _) in finished.into_iter().take(excess) {
                runs.remove(&k);
            }
        }
    }

    pub async fn finish_run(&self, run_id: &str, status: AgentRunStatus, activity: String) {
        let mut runs = self.runs.write().await;
        if let Some(handle) = runs.get_mut(run_id) {
            handle.summary.status = status.clone();
            handle.summary.last_activity_text = activity;
            handle.summary.elapsed_secs = handle.started_instant.elapsed().as_secs();
            let _ = handle.status_tx.send(status);
        }
        Self::trim_excess_finished_runs(&mut runs);
    }

    pub async fn finish_delegated_run(&self, run_id: &str, result: DelegatedRunResult) {
        let mut runs = self.runs.write().await;
        if let Some(handle) = runs.get_mut(run_id) {
            handle.summary.status = result.status.clone();
            handle.summary.last_activity_text = result.summary_text.chars().take(120).collect();
            handle.summary.elapsed_secs = handle.started_instant.elapsed().as_secs();
            handle.pending_approval_hint = result.pending_approval_hint.clone();
            handle.final_result = Some(result.clone());
            let _ = handle.status_tx.send(result.status);
        }
        Self::trim_excess_finished_runs(&mut runs);
    }

    pub async fn get_run_workspace_pin_info(&self, run_id: &str) -> Option<(String, Option<String>)> {
        let runs = self.runs.read().await;
        runs.get(run_id).map(|h| {
            (
                h.summary.workspace_alias.clone(),
                h.pinned_workspace_alias.clone(),
            )
        })
    }

    fn synthesize_delegated_result(handle: &AgentRunHandle) -> DelegatedRunResult {
        if let Some(ref res) = handle.final_result {
            return res.clone();
        }
        DelegatedRunResult {
            run_id: handle.summary.run_id.clone(),
            workspace_alias: handle.summary.workspace_alias.clone(),
            mode: if handle.summary.execution_mode.contains("execute") {
                "execute".to_string()
            } else {
                "read_only".to_string()
            },
            status: handle.summary.status.clone(),
            summary_text: handle.summary.last_activity_text.clone(),
            touched_files: Vec::new(),
            turn_group_id: None,
            step_count: handle.summary.current_step,
            elapsed_secs: handle.started_instant.elapsed().as_secs(),
            pending_approval_hint: handle.pending_approval_hint.clone(),
        }
    }

    pub async fn get_delegated_run_snapshot(&self, run_id: &str) -> Result<DelegatedRunResult, String> {
        let runs = self.runs.read().await;
        let handle = runs
            .get(run_id)
            .ok_or_else(|| format!("Delegated run '{run_id}' tidak ditemukan."))?;
        Ok(Self::synthesize_delegated_result(handle))
    }

    pub async fn poll_delegated_run(
        &self,
        run_id: &str,
        wait_secs: u64,
    ) -> Result<DelegatedRunResult, String> {
        let mut status_rx = {
            let runs = self.runs.read().await;
            let handle = runs
                .get(run_id)
                .ok_or_else(|| format!("Delegated run '{run_id}' tidak ditemukan."))?;
            if handle.summary.status != AgentRunStatus::Running || wait_secs == 0 {
                return Ok(Self::synthesize_delegated_result(handle));
            }
            handle.status_tx.subscribe()
        };

        let clamped_wait = wait_secs.min(26);
        let _ = tokio::time::timeout(
            Duration::from_secs(clamped_wait),
            status_rx.wait_for(|s| *s != AgentRunStatus::Running),
        )
        .await;

        self.get_delegated_run_snapshot(run_id).await
    }

    pub async fn cancel_run(&self, run_id: Option<&str>) -> usize {
        let runs = self.runs.read().await;
        let mut cancelled = 0usize;
        match run_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(target) => {
                if let Some(handle) = runs.get(target) {
                    if matches!(
                        handle.summary.status,
                        AgentRunStatus::Running | AgentRunStatus::AwaitingApproval
                    ) {
                        let _ = handle.cancel_tx.send(true);
                        cancelled = 1;
                    }
                }
            }
            None => {
                for handle in runs.values() {
                    if matches!(
                        handle.summary.status,
                        AgentRunStatus::Running | AgentRunStatus::AwaitingApproval
                    ) {
                        let _ = handle.cancel_tx.send(true);
                        cancelled += 1;
                    }
                }
            }
        }
        cancelled
    }

    pub async fn cancel_session_runs(&self, session_id: &str) -> usize {
        let runs = self.runs.read().await;
        let mut cancelled = 0usize;
        for handle in runs.values() {
            if handle.summary.status == AgentRunStatus::Running
                && handle.summary.session_id == session_id
            {
                let _ = handle.cancel_tx.send(true);
                cancelled += 1;
            }
        }
        cancelled
    }

    pub async fn has_active_runs(&self) -> bool {
        self.runs
            .read()
            .await
            .values()
            .any(|h| h.summary.status == AgentRunStatus::Running)
    }

    pub async fn list_runs(&self) -> Vec<AgentRunSummary> {
        let runs = self.runs.read().await;
        let mut items: Vec<(bool, Instant, AgentRunSummary)> = runs
            .values()
            .map(|h| {
                let mut sum = h.summary.clone();
                if sum.status == AgentRunStatus::Running {
                    sum.elapsed_secs = h.started_instant.elapsed().as_secs();
                }
                (
                    sum.status == AgentRunStatus::Running,
                    h.started_instant,
                    sum,
                )
            })
            .collect();
        items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        items.into_iter().map(|(_, _, sum)| sum).collect()
    }

    pub async fn workspace_write_lock(&self, workspace_id: &str) -> Arc<Mutex<()>> {
        let key = workspace_id.trim().to_string();
        {
            let guard = self.workspace_write_locks.read().await;
            if let Some(lock) = guard.get(&key) {
                return lock.clone();
            }
        }
        let mut guard = self.workspace_write_locks.write().await;
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn touch_session_lru(&self, session_id: &str) {
        let sid = session_id.to_string();
        let evicted = {
            let mut lru = self.session_lru.write().await;
            lru.retain(|x| x != &sid);
            lru.push(sid);
            if lru.len() > MAX_CACHED_SESSIONS {
                Some(lru.remove(0))
            } else {
                None
            }
        };
        if let Some(old_sid) = evicted {
            self.gemini_histories.write().await.remove(&old_sid);
            self.openai_histories.write().await.remove(&old_sid);
            self.anthropic_histories.write().await.remove(&old_sid);
            self.checkpoints.write().await.remove(&old_sid);
        }
    }

    pub async fn get_history(&self, provider: &str, session_id: &str) -> Vec<Value> {
        let sid = session_id.trim();
        match provider.trim() {
            "openai_compat" => self
                .openai_histories
                .read()
                .await
                .get(sid)
                .cloned()
                .unwrap_or_default(),
            "anthropic" => self
                .anthropic_histories
                .read()
                .await
                .get(sid)
                .cloned()
                .unwrap_or_default(),
            _ => self
                .gemini_histories
                .read()
                .await
                .get(sid)
                .cloned()
                .unwrap_or_default(),
        }
    }

    pub async fn set_history(&self, provider: &str, session_id: &str, history: Vec<Value>) {
        let sid = session_id.trim().to_string();
        if sid.is_empty() {
            return;
        }
        match provider.trim() {
            "openai_compat" => {
                self.openai_histories.write().await.insert(sid.clone(), history);
            }
            "anthropic" => {
                self.anthropic_histories
                    .write()
                    .await
                    .insert(sid.clone(), history);
            }
            _ => {
                self.gemini_histories.write().await.insert(sid.clone(), history);
            }
        }
        self.touch_session_lru(&sid).await;
    }

    pub async fn clear_history(&self, session_id: Option<&str>) {
        match session_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sid) => {
                self.gemini_histories.write().await.remove(sid);
                self.openai_histories.write().await.remove(sid);
                self.anthropic_histories.write().await.remove(sid);
                self.checkpoints.write().await.remove(sid);
                self.session_lru.write().await.retain(|x| x != sid);
            }
            None => {
                self.gemini_histories.write().await.clear();
                self.openai_histories.write().await.clear();
                self.anthropic_histories.write().await.clear();
                self.checkpoints.write().await.clear();
                self.session_lru.write().await.clear();
            }
        }
    }

    pub async fn get_checkpoint(&self, session_id: &str) -> Option<TurnCheckpointInfo> {
        self.checkpoints
            .read()
            .await
            .get(session_id.trim())
            .cloned()
    }

    pub async fn set_checkpoint(&self, session_id: &str, cp: Option<TurnCheckpointInfo>) {
        let sid = session_id.trim().to_string();
        let mut guard = self.checkpoints.write().await;
        match cp {
            Some(info) => {
                guard.insert(sid, info);
            }
            None => {
                guard.remove(&sid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_concurrent_runs_isolation_and_workspace_write_lock() {
        let mgr = Arc::new(AgentRunManager::default());

        // 1. Per-session histories remain strictly isolated
        mgr.set_history("gemini", "sess_a", vec![json!({"role":"user","parts":[{"text":"A1"}]})])
            .await;
        mgr.set_history("gemini", "sess_b", vec![json!({"role":"user","parts":[{"text":"B1"}]})])
            .await;
        assert_eq!(mgr.get_history("gemini", "sess_a").await[0]["parts"][0]["text"], "A1");
        assert_eq!(mgr.get_history("gemini", "sess_b").await[0]["parts"][0]["text"], "B1");

        mgr.clear_history(Some("sess_a")).await;
        assert!(mgr.get_history("gemini", "sess_a").await.is_empty());
        assert_eq!(mgr.get_history("gemini", "sess_b").await.len(), 1);

        // 2. Max concurrent runs limit (4) enforced cleanly
        let mut ids = Vec::new();
        for i in 0..MAX_CONCURRENT_RUNS {
            let (rid, _, _) = mgr
                .start_run(
                    None,
                    format!("s-{i}"),
                    "ws".to_string(),
                    format!("Task {i}"),
                    "gemini".to_string(),
                    "gemini-2.5-flash".to_string(),
                    "direct".to_string(),
                    16,
                )
                .await
                .unwrap();
            ids.push(rid);
        }
        assert!(mgr
            .start_run(
                None,
                "s-overflow".to_string(),
                "ws".to_string(),
                "Overflow".to_string(),
                "gemini".to_string(),
                "gemini-2.5-flash".to_string(),
                "direct".to_string(),
                16,
            )
            .await
            .is_err());

        // Finish one run -> allows spawning another
        mgr.finish_run(&ids[0], AgentRunStatus::Completed, "done".to_string())
            .await;
        let (rid5, _, _) = mgr
            .start_run(
                None,
                "s-5".to_string(),
                "ws".to_string(),
                "Task 5".to_string(),
                "gemini".to_string(),
                "gemini-2.5-flash".to_string(),
                "direct".to_string(),
                16,
            )
            .await
            .unwrap();
        assert!(!rid5.is_empty());

        // 3. Per-workspace write lock serializes concurrent mutations cleanly (no overlapping writes)
        let overlap_counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent_writers = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let m = mgr.clone();
            let ov = overlap_counter.clone();
            let mx = max_concurrent_writers.clone();
            handles.push(tokio::spawn(async move {
                let lock = m.workspace_write_lock("ws-1").await;
                let _guard = lock.lock().await;
                let active = ov.fetch_add(1, Ordering::SeqCst) + 1;
                mx.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                ov.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(max_concurrent_writers.load(Ordering::SeqCst), 1);
    }
}
