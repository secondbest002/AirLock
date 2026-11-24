use crate::jobs::{self, JobStatus};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, RwLock};
use tokio::task::AbortHandle;

tokio::task_local! {
    static CURRENT_MCP_TASK_ID: String;
}

pub fn current_task_id() -> Option<String> {
    CURRENT_MCP_TASK_ID.try_with(|id| id.clone()).ok()
}

pub async fn with_task_scope<F>(task_id: String, fut: F) -> F::Output
where
    F: std::future::Future,
{
    CURRENT_MCP_TASK_ID.scope(task_id, fut).await
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

impl McpTaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTaskInfo {
    #[serde(rename = "taskId")]
    pub task_id: String,
    pub status: McpTaskStatus,
    #[serde(rename = "statusMessage", skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastUpdatedAt")]
    pub last_updated_at: String,
    pub ttl: u64,
    #[serde(rename = "pollInterval", skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
}

struct McpTaskEntry {
    info: McpTaskInfo,
    created_order: u64,
    pinned_workspace: Option<String>,
    linked_job_id: Option<String>,
    result_payload: Option<Value>,
    error_payload: Option<(i64, String)>,
    updated_instant: Instant,
    status_tx: watch::Sender<McpTaskStatus>,
    abort_handle: Option<AbortHandle>,
}

pub struct McpTaskManager {
    tasks: Arc<RwLock<HashMap<String, McpTaskEntry>>>,
    seq: AtomicU64,
}

impl Default for McpTaskManager {
    fn default() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            seq: AtomicU64::new(1),
        }
    }
}

impl McpTaskManager {
    pub async fn create_tool_task(
        &self,
        state: Arc<AppState>,
        source: String,
        tool_name: String,
        args: Value,
        pinned_ws: Option<String>,
        requested_ttl_ms: Option<u64>,
    ) -> McpTaskInfo {
        let token = AppState::generate_token();
        let task_id = format!("task-{}", &token[..12]);
        let now = AppState::now_timestamp();
        let ttl = requested_ttl_ms.unwrap_or(300_000).clamp(5_000, 3_600_000);
        let order = self.seq.fetch_add(1, Ordering::Relaxed);
        let (status_tx, _rx) = watch::channel(McpTaskStatus::Working);

        let info = McpTaskInfo {
            task_id: task_id.clone(),
            status: McpTaskStatus::Working,
            status_message: Some(format!("Running tool '{}'", tool_name)),
            created_at: now.clone(),
            last_updated_at: now,
            ttl,
            poll_interval: Some(500),
        };

        let entry = McpTaskEntry {
            info: info.clone(),
            created_order: order,
            pinned_workspace: pinned_ws.clone(),
            linked_job_id: None,
            result_payload: None,
            error_payload: None,
            updated_instant: Instant::now(),
            status_tx,
            abort_handle: None,
        };

        {
            let mut guard = self.tasks.write().await;
            guard.insert(task_id.clone(), entry);
        }

        let tid = task_id.clone();
        let state_bg = Arc::clone(&state);
        let join = tokio::spawn(async move {
            let scoped = with_task_scope(tid.clone(), async move {
                let exec_fut = async {
                    if tool_name.contains("__") {
                        state_bg
                            .mcp_hub
                            .call_downstream_tool(&state_bg, &source, &tool_name, args)
                            .await
                    } else {
                        crate::tools::execute_tool_with_context(
                            &state_bg,
                            &source,
                            &tool_name,
                            args,
                            pinned_ws.as_deref(),
                        )
                        .await
                    }
                };

                let mut output = match tokio::time::timeout(Duration::from_secs(600), exec_fut).await
                {
                    Ok(out) => out,
                    Err(_) => crate::tools::ToolOutput {
                        text: format!("MCP task timeout (600s) executing '{}'.", tool_name),
                        is_error: true,
                    },
                };

                // If start_command_job succeeded, wait for the linked background job so task status tracks real process completion.
                if tool_name == "start_command_job" && !output.is_error {
                    if let Some((job_id, mut done_rx)) =
                        jobs::subscribe_job_by_mcp_task(&state_bg, &tid).await
                    {
                        state_bg
                            .mcp_tasks
                            .attach_linked_job(&tid, job_id.clone())
                            .await;
                        state_bg
                            .mcp_tasks
                            .set_status(
                                &tid,
                                McpTaskStatus::Working,
                                Some(format!("Running command job {}", job_id)),
                            )
                            .await;

                        while !*done_rx.borrow_and_update() {
                            if done_rx.changed().await.is_err() {
                                break;
                            }
                        }

                        if let Some((status, exit_code, elapsed, compressed, total_lines)) =
                            jobs::get_job_snapshot_output(&state_bg, &job_id, 120).await
                        {
                            let status_label = match status {
                                JobStatus::Running => "running",
                                JobStatus::Completed => "completed",
                                JobStatus::Failed => "failed",
                                JobStatus::Killed => "killed",
                                JobStatus::TimedOut => "timed_out",
                            };
                            let text = format!(
                                "JOB_ID: {job_id}\nSTATUS: {status_label}\nEXIT_CODE: {}\nELAPSED_SECS: {elapsed}\nTOTAL_LINES: {total_lines}\n--- OUTPUT ---\n{compressed}",
                                exit_code
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "-".to_string())
                            );
                            let is_err = matches!(
                                status,
                                JobStatus::Failed | JobStatus::TimedOut | JobStatus::Killed
                            );
                            output = crate::tools::ToolOutput {
                                text,
                                is_error: is_err,
                            };
                        }
                    }
                }

                let final_status = if output.is_error {
                    McpTaskStatus::Failed
                } else {
                    McpTaskStatus::Completed
                };
                let result_json = json!({
                    "content": [{ "type": "text", "text": output.text }],
                    "isError": output.is_error,
                    "_meta": {
                        "io.modelcontextprotocol/related-task": {
                            "taskId": tid
                        }
                    }
                });
                state_bg
                    .mcp_tasks
                    .finish_task(&tid, final_status, result_json)
                    .await;
            });
            scoped.await;
        });

        {
            let mut guard = self.tasks.write().await;
            if let Some(item) = guard.get_mut(&task_id) {
                if !item.info.status.is_terminal() {
                    item.abort_handle = Some(join.abort_handle());
                } else {
                    join.abort();
                }
            }
        }

        info
    }

    pub async fn attach_linked_job(&self, task_id: &str, job_id: String) {
        let mut guard = self.tasks.write().await;
        if let Some(entry) = guard.get_mut(task_id) {
            entry.linked_job_id = Some(job_id);
        }
    }

    pub async fn set_status(
        &self,
        task_id: &str,
        status: McpTaskStatus,
        message: Option<String>,
    ) {
        let mut guard = self.tasks.write().await;
        if let Some(entry) = guard.get_mut(task_id) {
            if entry.info.status.is_terminal() {
                return;
            }
            entry.info.status = status;
            if message.is_some() {
                entry.info.status_message = message;
            }
            entry.info.last_updated_at = AppState::now_timestamp();
            entry.updated_instant = Instant::now();
            let _ = entry.status_tx.send(status);
        }
    }

    async fn finish_task(&self, task_id: &str, status: McpTaskStatus, result_payload: Value) {
        let mut guard = self.tasks.write().await;
        if let Some(entry) = guard.get_mut(task_id) {
            if entry.info.status.is_terminal() {
                return;
            }
            entry.info.status = status;
            entry.info.status_message = Some(match status {
                McpTaskStatus::Completed => "Completed".to_string(),
                McpTaskStatus::Failed => "Execution finished with error".to_string(),
                _ => status.as_str().to_string(),
            });
            entry.info.last_updated_at = AppState::now_timestamp();
            entry.updated_instant = Instant::now();
            entry.result_payload = Some(result_payload);
            entry.abort_handle = None;
            let _ = entry.status_tx.send(status);
        }
    }

    fn verify_scope(entry: &McpTaskEntry, pinned_ws: Option<&str>) -> Result<(), (i64, String)> {
        if let Some(req_pin) = pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
            let task_pin = entry.pinned_workspace.as_deref().unwrap_or("");
            if !task_pin.eq_ignore_ascii_case(req_pin) {
                return Err((
                    -32602,
                    format!("Task '{}' tidak termasuk dalam workspace pin '{}'.", entry.info.task_id, req_pin),
                ));
            }
        }
        Ok(())
    }

    pub async fn get_task(
        &self,
        task_id: &str,
        pinned_ws: Option<&str>,
    ) -> Result<McpTaskInfo, (i64, String)> {
        let guard = self.tasks.read().await;
        let entry = guard
            .get(task_id)
            .ok_or_else(|| (-32602, format!("Task '{}' tidak ditemukan.", task_id)))?;
        Self::verify_scope(entry, pinned_ws)?;
        Ok(entry.info.clone())
    }

    pub async fn list_tasks(
        &self,
        cursor: Option<&str>,
        pinned_ws: Option<&str>,
    ) -> Result<Value, (i64, String)> {
        let offset = match cursor.map(str::trim).filter(|s| !s.is_empty()) {
            Some(c) => c
                .parse::<usize>()
                .map_err(|_| (-32602, format!("Cursor pagination tidak valid: '{c}'.")))?,
            None => 0,
        };

        let guard = self.tasks.read().await;
        let mut items: Vec<&McpTaskEntry> = guard
            .values()
            .filter(|entry| match pinned_ws.map(str::trim).filter(|s| !s.is_empty()) {
                Some(pin) => entry
                    .pinned_workspace
                    .as_deref()
                    .map(|p| p.eq_ignore_ascii_case(pin))
                    .unwrap_or(false),
                None => true,
            })
            .collect();

        items.sort_by(|a, b| b.created_order.cmp(&a.created_order));

        let page_size = 50usize;
        let total = items.len();
        let page: Vec<McpTaskInfo> = items
            .into_iter()
            .skip(offset)
            .take(page_size)
            .map(|e| e.info.clone())
            .collect();
        let next_cursor = if offset + page.len() < total {
            Some((offset + page.len()).to_string())
        } else {
            None
        };

        let mut res = json!({ "tasks": page });
        if let Some(nc) = next_cursor {
            res.as_object_mut()
                .expect("object")
                .insert("nextCursor".to_string(), json!(nc));
        }
        Ok(res)
    }

    pub async fn cancel_task(
        &self,
        state: &AppState,
        task_id: &str,
        pinned_ws: Option<&str>,
    ) -> Result<McpTaskInfo, (i64, String)> {
        let (abort_opt, linked_job_opt, updated_info) = {
            let mut guard = self.tasks.write().await;
            let entry = guard
                .get_mut(task_id)
                .ok_or_else(|| (-32602, format!("Task '{}' tidak ditemukan.", task_id)))?;
            Self::verify_scope(entry, pinned_ws)?;
            if entry.info.status.is_terminal() {
                return Err((
                    -32602,
                    format!(
                        "Cannot cancel task '{}' in terminal status '{}'.",
                        task_id,
                        entry.info.status.as_str()
                    ),
                ));
            }
            entry.info.status = McpTaskStatus::Cancelled;
            entry.info.status_message = Some("Cancelled by client".to_string());
            entry.info.last_updated_at = AppState::now_timestamp();
            entry.updated_instant = Instant::now();
            entry.error_payload = Some((-32000, "Task cancelled by client.".to_string()));
            let _ = entry.status_tx.send(McpTaskStatus::Cancelled);
            let abort_opt = entry.abort_handle.take();
            let linked_job = entry.linked_job_id.clone();
            (abort_opt, linked_job, entry.info.clone())
        };

        if let Some(handle) = abort_opt {
            handle.abort();
        }
        if let Some(jid) = linked_job_opt {
            let _ = jobs::stop_job(state, &jid).await;
        } else if let Some(jid) = jobs::find_job_id_by_mcp_task(state, task_id).await {
            let _ = jobs::stop_job(state, &jid).await;
        }

        Ok(updated_info)
    }

    pub async fn get_task_result(
        &self,
        task_id: &str,
        pinned_ws: Option<&str>,
        wait_timeout_secs: u64,
    ) -> Result<Value, (i64, String)> {
        let mut status_rx = {
            let guard = self.tasks.read().await;
            let entry = guard
                .get(task_id)
                .ok_or_else(|| (-32602, format!("Task '{}' tidak ditemukan.", task_id)))?;
            Self::verify_scope(entry, pinned_ws)?;
            if entry.info.status.is_terminal() {
                return Self::extract_terminal_outcome(entry);
            }
            entry.status_tx.subscribe()
        };

        let deadline = Instant::now() + Duration::from_secs(wait_timeout_secs.max(1));
        loop {
            let current = *status_rx.borrow_and_update();
            if current.is_terminal() {
                let guard = self.tasks.read().await;
                let entry = guard
                    .get(task_id)
                    .ok_or_else(|| (-32602, format!("Task '{}' tidak ditemukan.", task_id)))?;
                return Self::extract_terminal_outcome(entry);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err((
                    -32001,
                    format!(
                        "Task '{}' masih berjalan ({}); poll tasks/get sebelum memanggil tasks/result lagi.",
                        task_id,
                        current.as_str()
                    ),
                ));
            }
            match tokio::time::timeout(remaining, status_rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) | Err(_) => {
                    return Err((
                        -32001,
                        format!(
                            "Task '{}' masih berjalan; poll tasks/get sebelum memanggil tasks/result lagi.",
                            task_id
                        ),
                    ));
                }
            }
        }
    }

    fn extract_terminal_outcome(entry: &McpTaskEntry) -> Result<Value, (i64, String)> {
        if entry.info.status == McpTaskStatus::Cancelled {
            return Err(entry
                .error_payload
                .clone()
                .unwrap_or((-32000, "Task cancelled.".to_string())));
        }
        if let Some(payload) = &entry.result_payload {
            return Ok(payload.clone());
        }
        if let Some((code, msg)) = &entry.error_payload {
            return Err((*code, msg.clone()));
        }
        Err((
            -32603,
            format!("Task '{}' selesai tanpa payload hasil.", entry.info.task_id),
        ))
    }

    pub async fn gc_expired_tasks(&self) {
        let mut guard = self.tasks.write().await;
        let now = Instant::now();
        guard.retain(|_, entry| {
            if !entry.info.status.is_terminal() {
                return true;
            }
            let age_ms = now.duration_since(entry.updated_instant).as_millis() as u64;
            age_ms < entry.info.ttl
        });

        // Hard cap terminal history to 200 items oldest-first
        let terminal_count = guard.values().filter(|e| e.info.status.is_terminal()).count();
        if terminal_count > 200 {
            let mut expired_keys: Vec<(u64, String)> = guard
                .iter()
                .filter(|(_, e)| e.info.status.is_terminal())
                .map(|(k, e)| (e.created_order, k.clone()))
                .collect();
            expired_keys.sort_by_key(|(ord, _)| *ord);
            let remove_n = terminal_count - 200;
            for (_, key) in expired_keys.into_iter().take(remove_n) {
                guard.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_server::handle_single_rpc;

    #[tokio::test]
    async fn test_mcp_tasks_lifecycle_create_get_result_cancel() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_tasks_lifecycle_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::write(
            temp_dir.join("sample.rs"),
            "pub fn hello_task() -> &'static str { \"ok\" }\n",
        )
        .unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));

        // 1. Augmented tools/call returns CreateTaskResult immediately
        let create_resp = handle_single_rpc(
            &state,
            &json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": {
                    "name": "read_symbol",
                    "arguments": {
                        "path": "sample.rs",
                        "symbol": "hello_task"
                    },
                    "task": {
                        "ttl": 60000
                    }
                }
            }),
        )
        .await
        .expect("tools/call with task should respond");

        let task_id = create_resp["result"]["task"]["taskId"]
            .as_str()
            .expect("taskId must be returned")
            .to_string();
        assert_eq!(create_resp["result"]["task"]["status"], "working");

        // 2. tasks/result awaits completion and returns the tool output
        let result_resp = handle_single_rpc(
            &state,
            &json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tasks/result",
                "params": { "taskId": task_id }
            }),
        )
        .await
        .expect("tasks/result should respond");
        assert_eq!(result_resp["result"]["isError"], false);
        let text = result_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(text.contains("hello_task"), "unexpected result: {text}");
        assert_eq!(
            result_resp["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"],
            task_id
        );

        // 3. tasks/get reports completed
        let get_resp = handle_single_rpc(
            &state,
            &json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "tasks/get",
                "params": { "taskId": task_id }
            }),
        )
        .await
        .expect("tasks/get should respond");
        assert_eq!(get_resp["result"]["status"], "completed");

        // 4. tasks/list lists the created task
        let list_resp = handle_single_rpc(
            &state,
            &json!({
                "jsonrpc": "2.0",
                "id": 13,
                "method": "tasks/list",
                "params": {}
            }),
        )
        .await
        .expect("tasks/list should respond");
        assert!(list_resp["result"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["taskId"] == task_id));

        // 5. Long task cancellation via tasks/cancel
        let sleep_cmd = if cfg!(target_os = "windows") {
            "ping -n 15 127.0.0.1 >nul"
        } else {
            "sleep 15"
        };
        let bg_create = handle_single_rpc(
            &state,
            &json!({
                "jsonrpc": "2.0",
                "id": 14,
                "method": "tools/call",
                "params": {
                    "name": "start_command_job",
                    "arguments": {
                        "command": sleep_cmd,
                        "label": "cancel-test"
                    },
                    "task": {}
                }
            }),
        )
        .await
        .expect("start_command_job task should respond");
        let bg_task_id = bg_create["result"]["task"]["taskId"]
            .as_str()
            .unwrap()
            .to_string();

        tokio::time::sleep(Duration::from_millis(200)).await;

        let cancel_resp = handle_single_rpc(
            &state,
            &json!({
                "jsonrpc": "2.0",
                "id": 15,
                "method": "tasks/cancel",
                "params": { "taskId": bg_task_id }
            }),
        )
        .await
        .expect("tasks/cancel should respond");
        assert_eq!(cancel_resp["result"]["status"], "cancelled");

        jobs::stop_all_jobs(&state).await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
