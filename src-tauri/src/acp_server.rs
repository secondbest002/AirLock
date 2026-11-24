use crate::chat_store::{ChatSession, StoredChatMessage};
use crate::config::AppConfig;
use crate::jobs;
use crate::local_agent::run_agent_turn_with_options;
use crate::providers::AgentAttachment;
use crate::state::{AppState, PendingToolApproval, ToolApprovalDecision};
use crate::workspace::{
    default_alias_for_path, ensure_unique_alias, normalize_clean_path, paths_equal,
    WorkspaceProfile,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

#[derive(Debug, Clone)]
pub enum AcpStreamEvent {
    ThoughtChunk {
        session_id: String,
        text: String,
    },
    MessageChunk {
        session_id: String,
        text: String,
    },
    ToolCallStart {
        session_id: String,
        call_id: String,
        title: String,
        kind: String,
        raw_input: Value,
    },
    ToolCallUpdate {
        session_id: String,
        call_id: String,
        status: String,
        text: String,
    },
    PlanUpdate {
        session_id: String,
        entries: Vec<AcpPlanEntry>,
    },
}

#[derive(Debug, Clone)]
pub struct AcpPlanEntry {
    pub content: String,
    pub status: String,
}

pub struct AcpPermissionRequest {
    pub session_id: String,
    pub info: PendingToolApproval,
    pub reply_tx: oneshot::Sender<ToolApprovalDecision>,
}

struct ActiveAcpIo {
    event_tx: mpsc::UnboundedSender<AcpStreamEvent>,
    permission_tx: mpsc::UnboundedSender<AcpPermissionRequest>,
    current_session_id: RwLock<Option<String>>,
}

pub struct AcpBridgeHandle {
    inner: RwLock<Option<Arc<ActiveAcpIo>>>,
}

impl Default for AcpBridgeHandle {
    fn default() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }
}

impl AcpBridgeHandle {
    pub async fn attach(
        &self,
        event_tx: mpsc::UnboundedSender<AcpStreamEvent>,
        permission_tx: mpsc::UnboundedSender<AcpPermissionRequest>,
    ) {
        let mut guard = self.inner.write().await;
        *guard = Some(Arc::new(ActiveAcpIo {
            event_tx,
            permission_tx,
            current_session_id: RwLock::new(None),
        }));
    }

    pub async fn detach(&self) {
        let mut guard = self.inner.write().await;
        *guard = None;
    }

    pub async fn set_current_session(&self, session_id: &str) {
        if let Some(io) = self.inner.read().await.as_ref() {
            *io.current_session_id.write().await = Some(session_id.to_string());
        }
    }

    pub async fn current_event_tx(&self) -> Option<mpsc::UnboundedSender<AcpStreamEvent>> {
        self.inner
            .read()
            .await
            .as_ref()
            .map(|io| io.event_tx.clone())
    }

    pub async fn request_permission(
        &self,
        info: &PendingToolApproval,
    ) -> Option<Result<ToolApprovalDecision, String>> {
        let io = self.inner.read().await.clone()?;
        let session_id = io
            .current_session_id
            .read()
            .await
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let (reply_tx, reply_rx) = oneshot::channel::<ToolApprovalDecision>();
        if io
            .permission_tx
            .send(AcpPermissionRequest {
                session_id,
                info: info.clone(),
                reply_tx,
            })
            .is_err()
        {
            return Some(Err("Channel ACP permission tidak lagi aktif.".to_string()));
        }

        match tokio::time::timeout(Duration::from_secs(120), reply_rx).await {
            Ok(Ok(decision)) => Some(Ok(decision)),
            Ok(Err(_)) | Err(_) => Some(Err(format!(
                "Timeout / channel tertutup pada ACP session/request_permission untuk '{}'.",
                info.tool_name
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct AcpSessionState {
    cwd_clean: String,
    workspace_alias: String,
    execution_mode: String,
}

pub struct AcpSessionHandler {
    state: Arc<AppState>,
    out_tx: mpsc::UnboundedSender<Value>,
    client_capabilities: RwLock<Value>,
    sessions: RwLock<HashMap<String, AcpSessionState>>,
    pending_client_requests: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_outbound_id: AtomicU64,
}

impl AcpSessionHandler {
    pub fn new(state: Arc<AppState>, out_tx: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            state,
            out_tx,
            client_capabilities: RwLock::new(json!({})),
            sessions: RwLock::new(HashMap::new()),
            pending_client_requests: Arc::new(Mutex::new(HashMap::new())),
            next_outbound_id: AtomicU64::new(1000),
        }
    }

    fn emit(&self, msg: Value) {
        let _ = self.out_tx.send(msg);
    }

    fn emit_error(&self, rpc_id: Value, code: i64, message: impl Into<String>) {
        self.emit(json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "error": {
                "code": code,
                "message": message.into()
            }
        }));
    }

    fn emit_session_update(&self, session_id: &str, update: Value) {
        self.emit(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": update
            }
        }));
    }

    pub async fn emit_stream_event(&self, evt: AcpStreamEvent) {
        match evt {
            AcpStreamEvent::ThoughtChunk { session_id, text } => {
                self.emit_session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_thought_chunk",
                        "content": { "type": "text", "text": text }
                    }),
                );
            }
            AcpStreamEvent::MessageChunk { session_id, text } => {
                self.emit_session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text }
                    }),
                );
            }
            AcpStreamEvent::ToolCallStart {
                session_id,
                call_id,
                title,
                kind,
                raw_input,
            } => {
                self.emit_session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call_id,
                        "title": title,
                        "kind": kind,
                        "status": "in_progress",
                        "rawInput": raw_input
                    }),
                );
            }
            AcpStreamEvent::ToolCallUpdate {
                session_id,
                call_id,
                status,
                text,
            } => {
                self.emit_session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": call_id,
                        "status": status,
                        "content": [{
                            "type": "content",
                            "content": { "type": "text", "text": text }
                        }]
                    }),
                );
            }
            AcpStreamEvent::PlanUpdate {
                session_id,
                entries,
            } => {
                let items: Vec<Value> = entries
                    .into_iter()
                    .map(|e| {
                        json!({
                            "content": e.content,
                            "priority": "medium",
                            "status": e.status
                        })
                    })
                    .collect();
                self.emit_session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "plan",
                        "entries": items
                    }),
                );
            }
        }
    }

    pub async fn forward_permission_request(&self, req: AcpPermissionRequest) {
        let req_id = self.next_outbound_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Value>();
        {
            self.pending_client_requests.lock().await.insert(req_id, tx);
        }

        let call_id = req.info.request_id.clone();
        self.emit(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "session/request_permission",
            "params": {
                "sessionId": req.session_id,
                "toolCall": {
                    "toolCallId": call_id,
                    "title": format!("{} ({})", req.info.tool_name, req.info.summary),
                    "kind": "edit",
                    "status": "pending"
                },
                "options": [
                    { "optionId": "allow_once", "name": "Allow Once", "kind": "allow_once" },
                    { "optionId": "allow_always", "name": "Allow for Session", "kind": "allow_always" },
                    { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" }
                ]
            }
        }));

        let pending_map = Arc::clone(&self.pending_client_requests);
        tokio::spawn(async move {
            let decision = match tokio::time::timeout(Duration::from_secs(120), rx).await {
                Ok(Ok(val)) => parse_permission_outcome(&val),
                _ => {
                    pending_map.lock().await.remove(&req_id);
                    ToolApprovalDecision::Deny
                }
            };
            let _ = req.reply_tx.send(decision);
        });
    }

    async fn mount_or_resolve_cwd(&self, raw_cwd: &str) -> Result<(String, String), String> {
        let (_, clean_cwd) = normalize_clean_path(Path::new(raw_cwd.trim()))?;
        let mut cfg_changed = false;
        let alias = {
            let mut cfg = self.state.config.write().await;
            if let Some(existing) = cfg.workspaces.iter_mut().find(|w| paths_equal(&w.path, &clean_cwd)) {
                if !existing.enabled {
                    existing.enabled = true;
                    cfg_changed = true;
                }
                existing.alias.clone()
            } else {
                let alias = ensure_unique_alias(
                    &default_alias_for_path(Path::new(&clean_cwd)),
                    &cfg.workspaces,
                    None,
                );
                let id = format!("ws-{}", &AppState::generate_token()[..8]);
                cfg.workspaces.push(WorkspaceProfile {
                    id,
                    alias: alias.clone(),
                    path: clean_cwd.clone(),
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: Vec::new(),
                    pinned: false,
                });
                cfg_changed = true;
                alias
            }
        };
        if cfg_changed {
            self.state.save_config_state().await;
        }
        Ok((clean_cwd, alias))
    }

    pub async fn handle_incoming_line(self: &Arc<Self>, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                self.emit_error(Value::Null, -32700, format!("Parse error: {e}"));
                return;
            }
        };

        let method_opt = msg.get("method").and_then(|v| v.as_str()).map(str::to_string);
        let id_opt = msg.get("id").cloned();

        // Response to an outbound server->client request (e.g. session/request_permission)
        if method_opt.is_none() {
            if let Some(id_num) = id_opt.as_ref().and_then(|v| v.as_u64()) {
                if let Some(tx) = self.pending_client_requests.lock().await.remove(&id_num) {
                    let _ = tx.send(msg);
                }
            }
            return;
        }

        let method = method_opt.unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        match method.as_str() {
            "initialize" => {
                let requested_ver = params.get("protocolVersion").cloned().unwrap_or_else(|| json!(1));
                if let Some(caps) = params.get("clientCapabilities").cloned() {
                    *self.client_capabilities.write().await = caps;
                }
                if let Some(rpc_id) = id_opt {
                    self.emit(json!({
                        "jsonrpc": "2.0",
                        "id": rpc_id,
                        "result": {
                            "protocolVersion": requested_ver,
                            "agentCapabilities": {
                                "loadSession": true,
                                "promptCapabilities": {
                                    "image": true,
                                    "embeddedContext": true
                                }
                            },
                            "agentInfo": {
                                "name": "airlock",
                                "title": "Airlock Studio",
                                "version": "0.1.0"
                            }
                        }
                    }));
                }
            }
            "session/new" => {
                let Some(rpc_id) = id_opt else { return };
                let cwd_raw = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                let (cwd_clean, ws_alias) = match self.mount_or_resolve_cwd(cwd_raw).await {
                    Ok(pair) => pair,
                    Err(err) => {
                        self.emit_error(rpc_id, -32602, format!("Invalid 'cwd': {err}"));
                        return;
                    }
                };
                let session_id = format!("acp_{}", &AppState::generate_token()[..8]);
                self.sessions.write().await.insert(
                    session_id.clone(),
                    AcpSessionState {
                        cwd_clean,
                        workspace_alias: ws_alias,
                        execution_mode: "direct".to_string(),
                    },
                );
                self.state.acp_bridge.set_current_session(&session_id).await;
                self.emit(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": {
                        "sessionId": session_id,
                        "modes": acp_modes_payload("direct")
                    }
                }));
            }
            "session/load" => {
                let Some(rpc_id) = id_opt else { return };
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if session_id.is_empty() {
                    self.emit_error(rpc_id, -32602, "Parameter 'sessionId' wajib diisi.");
                    return;
                }
                let cwd_raw = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                let (cwd_clean, ws_alias) = if cwd_raw.trim().is_empty() {
                    let root = self.state.workspace_root.read().await.clone();
                    match self.mount_or_resolve_cwd(&root.display().to_string()).await {
                        Ok(pair) => pair,
                        Err(err) => {
                            self.emit_error(rpc_id, -32602, err);
                            return;
                        }
                    }
                } else {
                    match self.mount_or_resolve_cwd(cwd_raw).await {
                        Ok(pair) => pair,
                        Err(err) => {
                            self.emit_error(rpc_id, -32602, format!("Invalid 'cwd': {err}"));
                            return;
                        }
                    }
                };

                let stored = self
                    .state
                    .chat_store
                    .list_sessions(&cwd_clean)
                    .into_iter()
                    .find(|s| s.id == session_id);
                let in_mem_exists = self.sessions.read().await.contains_key(session_id);
                if stored.is_none() && !in_mem_exists {
                    self.emit_error(
                        rpc_id,
                        -32602,
                        format!("Session '{}' tidak ditemukan di ChatStore.", session_id),
                    );
                    return;
                }

                if let Some(chat) = stored {
                    for msg in chat.messages {
                        if msg.role == "user" {
                            self.emit_session_update(
                                session_id,
                                json!({
                                    "sessionUpdate": "user_message_chunk",
                                    "content": { "type": "text", "text": msg.content }
                                }),
                            );
                        } else {
                            if let Some(thought) = msg.thought.filter(|t| !t.is_empty()) {
                                self.emit_session_update(
                                    session_id,
                                    json!({
                                        "sessionUpdate": "agent_thought_chunk",
                                        "content": { "type": "text", "text": thought }
                                    }),
                                );
                            }
                            if !msg.content.is_empty() {
                                self.emit_session_update(
                                    session_id,
                                    json!({
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": msg.content }
                                    }),
                                );
                            }
                        }
                    }
                }

                let mode = self
                    .sessions
                    .read()
                    .await
                    .get(session_id)
                    .map(|s| s.execution_mode.clone())
                    .unwrap_or_else(|| "direct".to_string());
                self.sessions.write().await.insert(
                    session_id.to_string(),
                    AcpSessionState {
                        cwd_clean,
                        workspace_alias: ws_alias,
                        execution_mode: mode.clone(),
                    },
                );
                self.state.acp_bridge.set_current_session(session_id).await;
                self.emit(json!({
                    "jsonrpc": "2.0",
                    "id": rpc_id,
                    "result": {
                        "modes": acp_modes_payload(&mode)
                    }
                }));
            }
            "session/set_mode" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                let mode_id = params
                    .get("modeId")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if !matches!(mode_id, "direct" | "plan_first") {
                    if let Some(rpc_id) = id_opt {
                        self.emit_error(rpc_id, -32602, format!("Mode ACP tidak valid: '{mode_id}'"));
                    }
                    return;
                }
                let updated = {
                    let mut guard = self.sessions.write().await;
                    if let Some(sess) = guard.get_mut(session_id) {
                        sess.execution_mode = mode_id.to_string();
                        true
                    } else {
                        false
                    }
                };
                if !updated {
                    if let Some(rpc_id) = id_opt {
                        self.emit_error(rpc_id, -32602, format!("Session ACP '{session_id}' tidak ditemukan."));
                    }
                    return;
                }
                self.emit_session_update(
                    session_id,
                    json!({
                        "sessionUpdate": "current_mode_update",
                        "currentModeId": mode_id
                    }),
                );
                if let Some(rpc_id) = id_opt {
                    self.emit(json!({ "jsonrpc": "2.0", "id": rpc_id, "result": {} }));
                }
            }
            "session/prompt" => {
                let Some(rpc_id) = id_opt else { return };
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string();
                let sess_opt = self.sessions.read().await.get(&session_id).cloned();
                let Some(sess) = sess_opt else {
                    self.emit_error(rpc_id, -32602, format!("Session ACP '{session_id}' tidak ditemukan."));
                    return;
                };
                let (prompt_text, attachments) =
                    parse_acp_prompt_blocks(params.get("prompt").and_then(|v| v.as_array()));
                if prompt_text.trim().is_empty() && attachments.is_empty() {
                    self.emit_error(rpc_id, -32602, "Prompt ACP tidak boleh kosong.");
                    return;
                }

                self.state.acp_bridge.set_current_session(&session_id).await;
                let this = Arc::clone(self);
                tokio::spawn(async move {
                    let res = run_agent_turn_with_options(
                        Arc::clone(&this.state),
                        prompt_text.clone(),
                        attachments,
                        Some(session_id.clone()),
                        Some(sess.execution_mode),
                        Some(sess.workspace_alias),
                    )
                    .await;

                    match res {
                        Ok(turn_resp) => {
                            this.persist_acp_turn(&sess.cwd_clean, &session_id, &prompt_text, &turn_resp.reply, turn_resp.cancelled);
                            let stop_reason = if turn_resp.cancelled {
                                "cancelled"
                            } else {
                                "end_turn"
                            };
                            this.emit(json!({
                                "jsonrpc": "2.0",
                                "id": rpc_id,
                                "result": { "stopReason": stop_reason }
                            }));
                        }
                        Err(err) => {
                            this.emit_error(rpc_id, -32000, err);
                        }
                    }
                });
            }
            "session/cancel" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if !session_id.is_empty() {
                    let _ = self.state.agent_manager.cancel_session_runs(session_id).await;
                }
                if let Some(rpc_id) = id_opt {
                    self.emit(json!({ "jsonrpc": "2.0", "id": rpc_id, "result": {} }));
                }
            }
            other => {
                if let Some(rpc_id) = id_opt {
                    self.emit_error(rpc_id, -32601, format!("Method ACP tidak dikenal: {other}"));
                }
            }
        }
    }

    fn persist_acp_turn(
        &self,
        cwd_clean: &str,
        session_id: &str,
        prompt_text: &str,
        reply_text: &str,
        cancelled: bool,
    ) {
        let mut session = self
            .state
            .chat_store
            .list_sessions(cwd_clean)
            .into_iter()
            .find(|s| s.id == session_id)
            .unwrap_or_else(|| ChatSession {
                id: session_id.to_string(),
                title: prompt_text.chars().take(60).collect(),
                created_at: AppState::now_timestamp(),
                updated_at: AppState::now_timestamp(),
                messages: Vec::new(),
            });
        let now = AppState::now_timestamp();
        session.messages.push(StoredChatMessage {
            role: "user".to_string(),
            content: prompt_text.to_string(),
            thought: None,
            steps: Vec::new(),
            usage: None,
            cancelled: false,
            timestamp: now.clone(),
        });
        session.messages.push(StoredChatMessage {
            role: "assistant".to_string(),
            content: reply_text.to_string(),
            thought: None,
            steps: Vec::new(),
            usage: None,
            cancelled,
            timestamp: now.clone(),
        });
        session.updated_at = now;
        let _ = self.state.chat_store.save_session(cwd_clean, session);
    }
}

fn acp_modes_payload(current_mode: &str) -> Value {
    json!({
        "currentModeId": current_mode,
        "availableModes": [
            {
                "id": "direct",
                "name": "Direct",
                "description": "Eksekusi agent langsung dengan guardrail aktif."
            },
            {
                "id": "plan_first",
                "name": "Plan First",
                "description": "Wajib membuat dan menyetujui implementation_plan sebelum edit."
            }
        ]
    })
}

fn parse_permission_outcome(resp: &Value) -> ToolApprovalDecision {
    let outcome = &resp["result"]["outcome"];
    if outcome["outcome"].as_str() == Some("cancelled") {
        return ToolApprovalDecision::Deny;
    }
    let opt = outcome["optionId"]
        .as_str()
        .or_else(|| resp["result"]["optionId"].as_str())
        .unwrap_or("");
    match opt {
        "allow_once" | "allow-once" => ToolApprovalDecision::ApproveOnce,
        "allow_always" | "allow-always" => ToolApprovalDecision::ApproveForSession,
        _ => ToolApprovalDecision::Deny,
    }
}

fn parse_acp_prompt_blocks(blocks_opt: Option<&Vec<Value>>) -> (String, Vec<AgentAttachment>) {
    let Some(blocks) = blocks_opt else {
        return (String::new(), Vec::new());
    };
    let mut text_parts = Vec::new();
    let mut attachments = Vec::new();

    for (idx, block) in blocks.iter().enumerate() {
        let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                if let Some(txt) = block.get("text").and_then(|v| v.as_str()) {
                    if !txt.trim().is_empty() {
                        text_parts.push(txt.to_string());
                    }
                }
            }
            "resource" => {
                let res = &block["resource"];
                let uri = res.get("uri").and_then(|v| v.as_str()).unwrap_or("resource");
                if let Some(txt) = res.get("text").and_then(|v| v.as_str()) {
                    text_parts.push(format!("<resource uri=\"{uri}\">\n{txt}\n</resource>"));
                } else {
                    text_parts.push(format!("<resource uri=\"{uri}\" />"));
                }
            }
            "resource_link" => {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("link");
                let uri = block.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                text_parts.push(format!("[Resource: {name} ({uri})]"));
            }
            "image" => {
                if let Some(data) = block.get("data").and_then(|v| v.as_str()) {
                    let mime = block
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .unwrap_or("image/png")
                        .to_string();
                    attachments.push(AgentAttachment {
                        kind: "image".to_string(),
                        name: format!("acp_image_{}.png", idx + 1),
                        workspace: None,
                        path: None,
                        mime_type: Some(mime),
                        data_base64: Some(data.to_string()),
                    });
                }
            }
            _ => {}
        }
    }

    (text_parts.join("\n\n"), attachments)
}

pub fn extract_plan_entries_from_markdown(md: &str) -> Vec<AcpPlanEntry> {
    let mut out = Vec::new();
    for raw in md.lines() {
        let trimmed = raw.trim();
        let (status, rest) = if let Some(r) = trimmed.strip_prefix("- [x] ").or_else(|| trimmed.strip_prefix("- [X] ")) {
            ("completed", r)
        } else if let Some(r) = trimmed.strip_prefix("- [/] ") {
            ("in_progress", r)
        } else if let Some(r) = trimmed.strip_prefix("- [ ] ") {
            ("pending", r)
        } else {
            continue;
        };
        let clean = rest.trim();
        if !clean.is_empty() {
            out.push(AcpPlanEntry {
                content: clean.chars().take(160).collect(),
                status: status.to_string(),
            });
            if out.len() >= 25 {
                break;
            }
        }
    }
    out
}

pub fn run_acp_server() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[airlock --acp] Gagal inisialisasi Tokio runtime: {e}");
            return 1;
        }
    };

    rt.block_on(async move {
        let cfg_path = AppConfig::default_path();
        let auth_path = AppState::default_auth_file_path();
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let state = Arc::new(AppState::new_persistent(ws, auth_path, cfg_path));

        let downstream = state.resolved_downstream_mcps().await;
        if !downstream.is_empty() {
            state.mcp_hub.refresh_all(&state.http_client, &downstream).await;
        }

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let writer_handle = tokio::task::spawn_blocking(move || {
            let stdout = std::io::stdout();
            while let Some(msg) = out_rx.blocking_recv() {
                if let Ok(line) = serde_json::to_string(&msg) {
                    let mut out = stdout.lock();
                    let _ = writeln!(out, "{line}");
                    let _ = out.flush();
                }
            }
        });

        let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<AcpStreamEvent>();
        let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<AcpPermissionRequest>();
        state.acp_bridge.attach(evt_tx, perm_tx).await;

        let handler = Arc::new(AcpSessionHandler::new(Arc::clone(&state), out_tx.clone()));
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let locked = stdin.lock();
            for line_res in locked.lines() {
                match line_res {
                    Ok(line) => {
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        loop {
            tokio::select! {
                maybe_line = line_rx.recv() => {
                    match maybe_line {
                        Some(l) => {
                            handler.handle_incoming_line(&l).await;
                        }
                        None => break,
                    }
                }
                maybe_evt = evt_rx.recv() => {
                    if let Some(evt) = maybe_evt {
                        handler.emit_stream_event(evt).await;
                    }
                }
                maybe_perm = perm_rx.recv() => {
                    if let Some(req) = maybe_perm {
                        handler.forward_permission_request(req).await;
                    }
                }
            }
        }

        state.acp_bridge.detach().await;
        jobs::stop_all_jobs(&state).await;
        state.mcp_hub.shutdown_all().await;
        state.lsp_manager.shutdown_all().await;
        drop(out_tx);
        let _ = writer_handle.await;
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_acp_initialize_session_new_and_cancel_protocol() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_acp_protocol_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let handler = Arc::new(AcpSessionHandler::new(Arc::clone(&state), out_tx));

        // 1. initialize
        handler
            .handle_incoming_line(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": 1,
                        "clientCapabilities": { "fs": { "readTextFile": true }, "terminal": true }
                    }
                })
                .to_string(),
            )
            .await;

        let init_msg = out_rx.recv().await.expect("initialize reply");
        assert_eq!(init_msg["id"], 1);
        assert_eq!(init_msg["result"]["agentInfo"]["name"], "airlock");
        assert_eq!(init_msg["result"]["agentCapabilities"]["loadSession"], true);

        // 2. session/new with real cwd
        handler
            .handle_incoming_line(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session/new",
                    "params": { "cwd": temp_dir.to_string_lossy().to_string() }
                })
                .to_string(),
            )
            .await;

        let new_msg = out_rx.recv().await.expect("session/new reply");
        let session_id = new_msg["result"]["sessionId"]
            .as_str()
            .expect("sessionId present")
            .to_string();
        assert!(session_id.starts_with("acp_"));
        assert_eq!(new_msg["result"]["modes"]["currentModeId"], "direct");

        // 3. session/set_mode -> emits update + ok response
        handler
            .handle_incoming_line(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "session/set_mode",
                    "params": { "sessionId": session_id, "modeId": "plan_first" }
                })
                .to_string(),
            )
            .await;
        let mode_update = out_rx.recv().await.expect("mode update notification");
        assert_eq!(mode_update["method"], "session/update");
        assert_eq!(mode_update["params"]["update"]["currentModeId"], "plan_first");
        let mode_ok = out_rx.recv().await.expect("set_mode reply");
        assert_eq!(mode_ok["id"], 3);

        // 4. session/cancel notification + parse error framing check (must be single-line NDJSON)
        handler
            .handle_incoming_line(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": { "sessionId": session_id }
                })
                .to_string(),
            )
            .await;
        handler.handle_incoming_line("{invalid-json").await;
        let parse_err = out_rx.recv().await.expect("parse error");
        assert_eq!(parse_err["error"]["code"], -32700);

        let serialized_ndjson = serde_json::to_string(&init_msg).unwrap();
        assert!(!serialized_ndjson.contains('\n') && !serialized_ndjson.contains('\r'));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
