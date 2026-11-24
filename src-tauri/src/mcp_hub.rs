use crate::config::DownstreamMcpConfig;
use crate::mcp_tasks::McpTaskStatus;
use crate::process_util::kill_pid_tree;
use crate::security::compress_output;
use crate::state::{
    AppState, AuditEntry, DownstreamMcpSummary, ElicitationAction, ElicitationResponse,
    PendingElicitation,
};
use crate::tools::{enforce_guardrail_gate, ToolOutput};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tauri::{Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex, RwLock};

pub fn sanitize_namespace(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else if ch == '-' || ch == ' ' {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "ext".to_string()
    } else {
        trimmed
    }
}

pub struct StdioMcpProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout_lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Drop for StdioMcpProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            kill_pid_tree(pid);
        }
        let _ = self.child.start_kill();
    }
}

impl StdioMcpProcess {
    async fn spawn(cfg: &DownstreamMcpConfig) -> Result<Self, String> {
        let cmd_str = cfg.command.trim();
        if cmd_str.is_empty() {
            return Err("Command stdio MCP tidak boleh kosong.".to_string());
        }

        let mut cmd = if cfg!(target_os = "windows")
            && (cmd_str.eq_ignore_ascii_case("npx") || cmd_str.eq_ignore_ascii_case("uvx"))
        {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(cmd_str).args(&cfg.args);
            c
        } else {
            let mut c = Command::new(cmd_str);
            c.args(&cfg.args);
            c
        };

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &cfg.env {
            if !k.trim().is_empty() {
                cmd.env(k, v);
            }
        }
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Gagal menjalankan stdio MCP '{}': {e}", cfg.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or("Gagal membuka stdin proses stdio MCP.")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Gagal membuka stdout proses stdio MCP.")?;

        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout_lines: BufReader::new(stdout).lines(),
            next_id: 1,
        })
    }

    async fn send_line(&mut self, msg: &Value) -> Result<(), String> {
        let line = serde_json::to_string(msg)
            .map_err(|e| format!("Gagal serialisasi JSON-RPC stdio: {e}"))?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("Gagal menulis stdin stdio MCP: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("Gagal menulis newline stdio MCP: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("Gagal flush stdin stdio MCP: {e}"))?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value, timeout_secs: u64) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_line(&payload).await?;

        tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            while let Ok(Some(line)) = self.stdout_lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    if val.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return Ok(val);
                    }
                }
            }
            Err("Stdio MCP menutup stdout sebelum merespons.".to_string())
        })
        .await
        .map_err(|_| format!("Timeout ({timeout_secs}s) menunggu respons stdio MCP untuk '{method}'."))?
    }

    async fn request_with_elicitation(
        &mut self,
        state: &AppState,
        server_id: &str,
        server_name: &str,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_line(&payload).await?;

        loop {
            let line_res = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                self.stdout_lines.next_line(),
            )
            .await
            .map_err(|_| {
                format!("Timeout ({timeout_secs}s) menunggu respons stdio MCP untuk '{method}'.")
            })?;

            let line = match line_res {
                Ok(Some(l)) => l,
                Ok(None) => return Err("Stdio MCP menutup stdout sebelum merespons.".to_string()),
                Err(e) => return Err(format!("Gagal membaca stdout stdio MCP: {e}")),
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };

            if val.get("id").and_then(|v| v.as_u64()) == Some(id) && val.get("method").is_none() {
                return Ok(val);
            }

            if let Some(req_method) = val.get("method").and_then(|v| v.as_str()) {
                let Some(req_id) = val.get("id").cloned() else {
                    // Server notification (e.g. progress) — ignore without replying.
                    continue;
                };
                if req_method == "elicitation/create" {
                    let req_params = val.get("params").cloned().unwrap_or_else(|| json!({}));
                    let mode = req_params.get("mode").and_then(|v| v.as_str()).unwrap_or("form");
                    let reply = if mode != "form" {
                        json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "error": {
                                "code": -32602,
                                "message": format!("Unsupported elicitation mode '{mode}'; Airlock only supports form elicitation.")
                            }
                        })
                    } else {
                        let elic = request_user_elicitation(state, server_id, server_name, &req_params).await;
                        json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": elic
                        })
                    };
                    self.send_line(&reply).await?;
                } else if req_method == "ping" {
                    self.send_line(&json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {}
                    }))
                    .await?;
                } else {
                    self.send_line(&json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {
                            "code": -32601,
                            "message": format!("Client method not supported by Airlock: {req_method}")
                        }
                    }))
                    .await?;
                }
            }
        }
    }

    async fn handshake_and_list_tools(&mut self) -> Result<Vec<Value>, String> {
        let init_resp = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {
                        "elicitation": {}
                    },
                    "clientInfo": { "name": "smart-tauri-hub", "version": "0.1.0" }
                }),
                15,
            )
            .await?;
        if let Some(err) = init_resp.get("error") {
            return Err(format!("Initialize error: {err}"));
        }
        let _ = self
            .send_line(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await;

        let list_resp = self.request("tools/list", json!({}), 15).await?;
        if let Some(err) = list_resp.get("error") {
            return Err(format!("tools/list error: {err}"));
        }
        Ok(list_resp["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }
}

pub struct DownstreamServerRuntime {
    pub status: String,
    pub tools: Vec<Value>,
    pub last_error: Option<String>,
    pub http_session_id: Option<String>,
    pub stdio: Arc<Mutex<Option<StdioMcpProcess>>>,
}

impl Default for DownstreamServerRuntime {
    fn default() -> Self {
        Self {
            status: "idle".to_string(),
            tools: Vec::new(),
            last_error: None,
            http_session_id: None,
            stdio: Arc::new(Mutex::new(None)),
        }
    }
}

pub struct McpHubManager {
    pub runtimes: RwLock<HashMap<String, DownstreamServerRuntime>>,
}

impl Default for McpHubManager {
    fn default() -> Self {
        Self {
            runtimes: RwLock::new(HashMap::new()),
        }
    }
}

fn parse_http_mcp_body(raw_text: &str) -> Result<Value, String> {
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    // Handle SSE response frames ("data: {...}")
    let mut last_json: Option<Value> = None;
    for line in trimmed.lines() {
        if let Some(data) = line.trim().strip_prefix("data:") {
            let payload = data.trim();
            if !payload.is_empty() && payload != "[DONE]" {
                if let Ok(v) = serde_json::from_str::<Value>(payload) {
                    last_json = Some(v);
                }
            }
        }
    }
    last_json.ok_or_else(|| {
        format!(
            "Gagal mem-parsing respons HTTP MCP: {}",
            trimmed.chars().take(200).collect::<String>()
        )
    })
}

enum HttpRpcError {
    SessionExpired(String),
    Other(String),
}

impl HttpRpcError {
    fn into_message(self) -> String {
        match self {
            HttpRpcError::SessionExpired(s) | HttpRpcError::Other(s) => s,
        }
    }
}

async fn http_rpc_post(
    client: &reqwest::Client,
    cfg: &DownstreamMcpConfig,
    session_id: Option<&str>,
    timeout_secs: u64,
    payload: &Value,
) -> Result<(Option<String>, Value), HttpRpcError> {
    let mut req = client
        .post(cfg.url.trim())
        .timeout(Duration::from_secs(timeout_secs))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json");
    let mut has_custom_auth = false;
    for (k, v) in &cfg.headers {
        let key_trim = k.trim();
        if key_trim.is_empty() {
            continue;
        }
        if key_trim.eq_ignore_ascii_case("authorization") {
            has_custom_auth = true;
        }
        req = req.header(key_trim, v);
    }
    if !has_custom_auth && !cfg.bearer_token.trim().is_empty() {
        req = req.bearer_auth(cfg.bearer_token.trim());
    }
    if let Some(sid) = session_id {
        req = req.header("mcp-session-id", sid);
    }

    let resp = req
        .json(payload)
        .send()
        .await
        .map_err(|e| HttpRpcError::Other(format!("Koneksi HTTP MCP '{}' gagal: {e}", cfg.name)))?;

    let status = resp.status();
    let new_sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| session_id.map(|s| s.to_string()));

    let text = resp
        .text()
        .await
        .map_err(|e| HttpRpcError::Other(format!("Gagal membaca respons HTTP MCP '{}': {e}", cfg.name)))?;

    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(HttpRpcError::SessionExpired(format!(
            "HTTP MCP '{}' session expired (404): {}",
            cfg.name,
            text.chars().take(200).collect::<String>()
        )));
    }
    if !status.is_success() {
        return Err(HttpRpcError::Other(format!(
            "HTTP MCP '{}' error ({}): {}",
            cfg.name,
            status,
            text.chars().take(200).collect::<String>()
        )));
    }

    let parsed = parse_http_mcp_body(&text).map_err(HttpRpcError::Other)?;
    Ok((new_sid, parsed))
}

impl McpHubManager {
    pub async fn refresh_server(
        &self,
        client: &reqwest::Client,
        cfg: &DownstreamMcpConfig,
    ) -> Result<usize, String> {
        if !cfg.enabled {
            let mut rt_map = self.runtimes.write().await;
            let entry = rt_map.entry(cfg.id.clone()).or_default();
            entry.status = "disabled".to_string();
            entry.tools.clear();
            entry.last_error = None;
            entry.http_session_id = None;
            let stdio_arc = entry.stdio.clone();
            drop(rt_map);
            *stdio_arc.lock().await = None;
            return Ok(0);
        }

        let res: Result<(Vec<Value>, Option<String>), String> =
            if cfg.transport.eq_ignore_ascii_case("stdio") {
                let stdio_arc = {
                    let mut rt_map = self.runtimes.write().await;
                    let entry = rt_map.entry(cfg.id.clone()).or_default();
                    entry.stdio.clone()
                };
                let mut guard = stdio_arc.lock().await;
                *guard = None;
                match StdioMcpProcess::spawn(cfg).await {
                    Ok(mut proc) => match proc.handshake_and_list_tools().await {
                        Ok(tools) => {
                            *guard = Some(proc);
                            Ok((tools, None))
                        }
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                }
            } else {
                let url = cfg.url.trim();
                if url.is_empty() {
                    Err("URL HTTP MCP tidak boleh kosong.".to_string())
                } else {
                    async {
                        let (mut current_sid, init_val) = http_rpc_post(
                            client,
                            cfg,
                            None,
                            15,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "method": "initialize",
                                "params": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {
                                        "elicitation": {}
                                    },
                                    "clientInfo": { "name": "smart-tauri-hub", "version": "0.1.0" }
                                }
                            }),
                        )
                        .await
                        .map_err(HttpRpcError::into_message)?;
                        if let Some(err) = init_val.get("error") {
                            return Err(format!("Initialize error: {err}"));
                        }

                        if let Ok((next_sid, _)) = http_rpc_post(
                            client,
                            cfg,
                            current_sid.as_deref(),
                            15,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/initialized"
                            }),
                        )
                        .await
                        {
                            if next_sid.is_some() {
                                current_sid = next_sid;
                            }
                        }

                        let (list_sid, list_val) = http_rpc_post(
                            client,
                            cfg,
                            current_sid.as_deref(),
                            15,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": 2,
                                "method": "tools/list"
                            }),
                        )
                        .await
                        .map_err(HttpRpcError::into_message)?;
                        if list_sid.is_some() {
                            current_sid = list_sid;
                        }
                        if let Some(err) = list_val.get("error") {
                            return Err(format!("tools/list error: {err}"));
                        }
                        let tools = list_val["result"]["tools"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        Ok((tools, current_sid))
                    }
                    .await
                }
            };

        let mut rt_map = self.runtimes.write().await;
        let entry = rt_map.entry(cfg.id.clone()).or_default();
        match res {
            Ok((tools, session_id)) => {
                let count = tools.len();
                entry.status = "connected".to_string();
                entry.tools = tools;
                entry.last_error = None;
                entry.http_session_id = session_id;
                Ok(count)
            }
            Err(err) => {
                entry.status = "error".to_string();
                entry.last_error = Some(err.clone());
                entry.http_session_id = None;
                Err(err)
            }
        }
    }

    pub async fn refresh_all(&self, client: &reqwest::Client, configs: &[DownstreamMcpConfig]) {
        for cfg in configs {
            let _ = self.refresh_server(client, cfg).await;
        }
    }

    pub async fn remove_server(&self, id: &str) {
        let stdio_opt = {
            let mut rt_map = self.runtimes.write().await;
            rt_map.remove(id).map(|rt| rt.stdio)
        };
        if let Some(stdio_arc) = stdio_opt {
            *stdio_arc.lock().await = None;
        }
    }

    pub async fn shutdown_all(&self) {
        let stdios: Vec<Arc<Mutex<Option<StdioMcpProcess>>>> = {
            let mut rt_map = self.runtimes.write().await;
            rt_map.drain().map(|(_, rt)| rt.stdio).collect()
        };
        for stdio_arc in stdios {
            *stdio_arc.lock().await = None;
        }
    }

    pub async fn list_aggregated_tools(&self, configs: &[DownstreamMcpConfig]) -> Vec<Value> {
        let rt_map = self.runtimes.read().await;
        let mut out = Vec::new();
        for cfg in configs {
            if !cfg.enabled {
                continue;
            }
            let Some(rt) = rt_map.get(&cfg.id) else {
                continue;
            };
            let ns = sanitize_namespace(&cfg.namespace);
            for tool in &rt.tools {
                let Some(orig_name) = tool.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                let orig_desc = tool
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut cloned = tool.clone();
                cloned["name"] = Value::String(format!("{ns}__{orig_name}"));
                cloned["description"] = Value::String(format!("[{}] {}", cfg.name, orig_desc));
                out.push(cloned);
            }
        }
        out.sort_by(|a, b| {
            let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            na.cmp(nb)
        });
        out
    }

    pub async fn list_summaries(&self, configs: &[DownstreamMcpConfig]) -> Vec<DownstreamMcpSummary> {
        let rt_map = self.runtimes.read().await;
        configs
            .iter()
            .map(|cfg| {
                let rt = rt_map.get(&cfg.id);
                let status = if !cfg.enabled {
                    "disabled".to_string()
                } else {
                    rt.map(|r| r.status.clone())
                        .unwrap_or_else(|| "idle".to_string())
                };
                DownstreamMcpSummary {
                    id: cfg.id.clone(),
                    name: cfg.name.clone(),
                    namespace: sanitize_namespace(&cfg.namespace),
                    transport: cfg.transport.clone(),
                    url: cfg.url.clone(),
                    has_bearer_token: !cfg.bearer_token.trim().is_empty()
                        || cfg
                            .headers
                            .keys()
                            .any(|k| k.eq_ignore_ascii_case("authorization")),
                    command: cfg.command.clone(),
                    args: cfg.args.clone(),
                    enabled: cfg.enabled,
                    status,
                    tools_count: rt.map(|r| r.tools.len()).unwrap_or(0),
                    last_error: rt.and_then(|r| r.last_error.clone()),
                    plugin_id: cfg.plugin_id.clone(),
                }
            })
            .collect()
    }

    pub async fn is_tool_read_only(
        &self,
        configs: &[DownstreamMcpConfig],
        namespaced_name: &str,
    ) -> bool {
        let Some((ns, raw_tool)) = namespaced_name.split_once("__") else {
            return false;
        };
        let rt_map = self.runtimes.read().await;
        for cfg in configs {
            if cfg.enabled && sanitize_namespace(&cfg.namespace) == ns {
                if let Some(rt) = rt_map.get(&cfg.id) {
                    if let Some(tool) = rt
                        .tools
                        .iter()
                        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(raw_tool))
                    {
                        return tool["annotations"]["readOnlyHint"]
                            .as_bool()
                            .unwrap_or(false);
                    }
                }
            }
        }
        false
    }

    pub async fn call_downstream_tool(
        &self,
        state: &AppState,
        source: &str,
        namespaced_name: &str,
        args: Value,
    ) -> ToolOutput {
        let Some((ns, raw_tool)) = namespaced_name.split_once("__") else {
            return ToolOutput {
                text: format!("Format tool aggregator tidak valid: {namespaced_name}"),
                is_error: true,
            };
        };

        let configs = state.resolved_downstream_mcps().await;
        let Some(cfg) = configs
            .iter()
            .find(|c| c.enabled && sanitize_namespace(&c.namespace) == ns)
            .cloned()
        else {
            return ToolOutput {
                text: format!("Downstream MCP server untuk namespace '{ns}' tidak ditemukan atau nonaktif."),
                is_error: true,
            };
        };

        let is_ro = self.is_tool_read_only(&configs, namespaced_name).await;
        if let Err(err) = enforce_guardrail_gate(state, source, namespaced_name, &args, is_ro).await
        {
            return ToolOutput {
                text: err,
                is_error: true,
            };
        }

        let timeout_secs = if source == "WebMCP" { 20 } else { 45 };
        let rpc_res = if cfg.transport.eq_ignore_ascii_case("stdio") {
            let stdio_arc = {
                let mut rt_map = self.runtimes.write().await;
                let entry = rt_map.entry(cfg.id.clone()).or_default();
                entry.stdio.clone()
            };
            let mut guard = stdio_arc.lock().await;
            if guard.is_none() {
                match StdioMcpProcess::spawn(&cfg).await {
                    Ok(mut p) => {
                        if let Err(e) = p.handshake_and_list_tools().await {
                            return ToolOutput {
                                text: e,
                                is_error: true,
                            };
                        }
                        *guard = Some(p);
                    }
                    Err(e) => {
                        return ToolOutput {
                            text: e,
                            is_error: true,
                        };
                    }
                }
            }
            let proc = guard.as_mut().expect("stdio proc initialized");
            proc.request_with_elicitation(
                state,
                &cfg.id,
                &cfg.name,
                "tools/call",
                json!({ "name": raw_tool, "arguments": args }),
                timeout_secs,
            )
            .await
        } else {
            let call_payload = json!({
                "jsonrpc": "2.0",
                "id": 100,
                "method": "tools/call",
                "params": { "name": raw_tool, "arguments": args }
            });
            let current_sid = {
                let rt_map = self.runtimes.read().await;
                rt_map
                    .get(&cfg.id)
                    .and_then(|rt| rt.http_session_id.clone())
            };

            match http_rpc_post(
                &state.http_client,
                &cfg,
                current_sid.as_deref(),
                timeout_secs,
                &call_payload,
            )
            .await
            {
                Ok((new_sid, val)) => {
                    if new_sid.is_some() && new_sid != current_sid {
                        let mut rt_map = self.runtimes.write().await;
                        let entry = rt_map.entry(cfg.id.clone()).or_default();
                        entry.http_session_id = new_sid;
                    }
                    Ok(val)
                }
                Err(HttpRpcError::SessionExpired(_)) => {
                    if let Err(refresh_err) = self.refresh_server(&state.http_client, &cfg).await {
                        Err(refresh_err)
                    } else {
                        let fresh_sid = {
                            let rt_map = self.runtimes.read().await;
                            rt_map
                                .get(&cfg.id)
                                .and_then(|rt| rt.http_session_id.clone())
                        };
                        match http_rpc_post(
                            &state.http_client,
                            &cfg,
                            fresh_sid.as_deref(),
                            timeout_secs,
                            &call_payload,
                        )
                        .await
                        {
                            Ok((new_sid, val)) => {
                                if new_sid.is_some() && new_sid != fresh_sid {
                                    let mut rt_map = self.runtimes.write().await;
                                    let entry = rt_map.entry(cfg.id.clone()).or_default();
                                    entry.http_session_id = new_sid;
                                }
                                Ok(val)
                            }
                            Err(e) => Err(e.into_message()),
                        }
                    }
                }
                Err(e) => Err(e.into_message()),
            }
        };

        let rpc_val = match rpc_res {
            Ok(v) => v,
            Err(err) => {
                return ToolOutput {
                    text: err,
                    is_error: true,
                };
            }
        };

        if let Some(err) = rpc_val.get("error") {
            return ToolOutput {
                text: format!("Downstream error: {err}"),
                is_error: true,
            };
        }

        let is_error = rpc_val["result"]["isError"].as_bool().unwrap_or(false);
        let raw_text = if let Some(arr) = rpc_val["result"]["content"].as_array() {
            let chunks: Vec<String> = arr
                .iter()
                .filter_map(|item| {
                    if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
                        Some(txt.to_string())
                    } else {
                        serde_json::to_string(item).ok()
                    }
                })
                .collect();
            if chunks.is_empty() {
                "(no output)".to_string()
            } else {
                chunks.join("\n")
            }
        } else {
            serde_json::to_string_pretty(&rpc_val["result"]).unwrap_or_default()
        };

        let line_count = raw_text.lines().count();
        let (final_text, tokens_saved) = if line_count > 160 {
            let (compressed, saved_bytes) = compress_output(&raw_text, "", 120);
            (compressed, saved_bytes / 4)
        } else {
            (raw_text, 0)
        };

        state
            .record_audit(
                AuditEntry {
                    id: AppState::next_audit_id(),
                    timestamp: AppState::now_timestamp(),
                    source: source.to_string(),
                    tool_name: namespaced_name.to_string(),
                    target_path: None,
                    summary: format!("[{}] {}", cfg.name, raw_tool),
                    diff_text: Some(final_text.chars().take(1500).collect()),
                    can_revert: false,
                    reverted: false,
                    tokens_saved,
                    original_content: None,
                    original_blob_hash: None,
                    file_existed_before: false,
                    post_edit_hash: None,
                    group_id: None,
                    move_from_path: None,
                    workspace_id: None,
                    workspace_alias: None,
                    workspace_root: None,
                },
                0,
            )
            .await;

        ToolOutput {
            text: final_text,
            is_error,
        }
    }
}

pub async fn resolve_mcp_elicitation(
    state: &AppState,
    request_id: &str,
    action: ElicitationAction,
    content: Option<Value>,
) -> Result<(), String> {
    let entry = {
        let mut pending = state.pending_elicitations.write().await;
        pending.remove(request_id)
    };
    let Some((_, tx)) = entry else {
        return Err("Permintaan MCP Elicitation sudah kadaluarsa atau tidak ditemukan.".to_string());
    };
    let normalized_content = if action == ElicitationAction::Accept {
        Some(content.unwrap_or_else(|| json!({})))
    } else {
        None
    };
    let _ = tx.send(ElicitationResponse {
        action,
        content: normalized_content,
    });
    Ok(())
}

async fn request_user_elicitation(
    state: &AppState,
    server_id: &str,
    server_name: &str,
    params: &Value,
) -> ElicitationResponse {
    let request_id = format!("elic_{}", &AppState::generate_token()[..12]);
    let message = params
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Downstream MCP server meminta input tambahan.")
        .to_string();
    let requested_schema = params.get("requestedSchema").cloned();

    let info = PendingElicitation {
        request_id: request_id.clone(),
        server_id: server_id.to_string(),
        server_name: server_name.to_string(),
        message: message.clone(),
        requested_schema,
        created_at: AppState::now_timestamp(),
    };

    let active_task = crate::mcp_tasks::current_task_id();
    if let Some(ref tid) = active_task {
        state
            .mcp_tasks
            .set_status(
                tid,
                McpTaskStatus::InputRequired,
                Some(format!("Menunggu input elicitation dari {}: {}", server_name, message)),
            )
            .await;
    }

    let (tx, rx) = oneshot::channel::<ElicitationResponse>();
    {
        let mut pending = state.pending_elicitations.write().await;
        pending.insert(request_id.clone(), (info.clone(), tx));
    }

    if let Some(app) = state.app_handle.read().await.clone() {
        if let Some(win) = app.get_webview_window("main") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
        }
        let _ = app.emit("mcp-elicitation-request", &info);
    }
    state.emit_dashboard().await;

    let wait_secs = if cfg!(test) { 15 } else { 120 };
    let res = tokio::time::timeout(Duration::from_secs(wait_secs), rx).await;

    {
        let mut pending = state.pending_elicitations.write().await;
        pending.remove(&request_id);
    }
    if let Some(app) = state.app_handle.read().await.clone() {
        let _ = app.emit("mcp-elicitation-resolved", &request_id);
    }
    state.emit_dashboard().await;

    if let Some(ref tid) = active_task {
        state
            .mcp_tasks
            .set_status(tid, McpTaskStatus::Working, Some("Melanjutkan eksekusi tool...".to_string()))
            .await;
    }

    match res {
        Ok(Ok(reply)) => reply,
        _ => ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_downstream_elicitation_request_roundtrip() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_elicitation_roundtrip_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mock_script = temp_dir.join("mock_elic_server.cjs");
        let script_src = r#"
const readline = require("readline");
const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
let pendingToolId = null;
rl.on("line", (line) => {
  if (!line.trim()) return;
  const msg = JSON.parse(line);
  if (msg.method === "initialize") {
    process.stdout.write(JSON.stringify({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        protocolVersion: "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "mock-elic", version: "1.0.0" }
      }
    }) + "\n");
  } else if (msg.method === "tools/list") {
    process.stdout.write(JSON.stringify({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        tools: [{
          name: "ask_env",
          description: "Requests user environment choice",
          inputSchema: { type: "object", properties: {} }
        }]
      }
    }) + "\n");
  } else if (msg.method === "tools/call") {
    pendingToolId = msg.id;
    process.stdout.write(JSON.stringify({
      jsonrpc: "2.0",
      id: "elic-req-1",
      method: "elicitation/create",
      params: {
        message: "Select target environment",
        requestedSchema: {
          type: "object",
          properties: {
            env: { type: "string", enum: ["staging", "production"] },
            confirm: { type: "boolean" }
          },
          required: ["env"]
        }
      }
    }) + "\n");
  } else if (msg.id === "elic-req-1") {
    const action = msg.result?.action || "none";
    const env = msg.result?.content?.env || "";
    const confirm = msg.result?.content?.confirm === true;
    process.stdout.write(JSON.stringify({
      jsonrpc: "2.0",
      id: pendingToolId,
      result: {
        content: [{ type: "text", text: `ELIC_ACTION=${action};ENV=${env};CONFIRM=${confirm}` }],
        isError: false
      }
    }) + "\n");
  }
});
"#;
        std::fs::write(&mock_script, script_src).unwrap();

        let state = Arc::new(AppState::new(temp_dir.clone()));
        let cfg = DownstreamMcpConfig {
            id: "mock-elic-id".to_string(),
            name: "Mock Elicitation".to_string(),
            namespace: "mockelic".to_string(),
            transport: "stdio".to_string(),
            url: String::new(),
            bearer_token: String::new(),
            command: "node".to_string(),
            args: vec![mock_script.to_string_lossy().to_string()],
            env: HashMap::new(),
            headers: HashMap::new(),
            enabled: true,
            plugin_id: None,
        };
        {
            let mut app_cfg = state.config.write().await;
            app_cfg.downstream_mcps.push(cfg.clone());
        }

        let state_bg = Arc::clone(&state);
        let call_handle = tokio::spawn(async move {
            state_bg
                .mcp_hub
                .call_downstream_tool(&state_bg, "Agent", "mockelic__ask_env", json!({}))
                .await
        });

        let mut captured_req: Option<PendingElicitation> = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let snap = state.snapshot().await;
            if let Some(first) = snap.pending_elicitations.first().cloned() {
                captured_req = Some(first);
                break;
            }
        }

        let req = captured_req.expect("Downstream MCP elicitation should appear in snapshot");
        assert_eq!(req.message, "Select target environment");
        assert!(req.requested_schema.is_some());

        resolve_mcp_elicitation(
            &state,
            &req.request_id,
            ElicitationAction::Accept,
            Some(json!({ "env": "staging", "confirm": true })),
        )
        .await
        .expect("resolve_mcp_elicitation should succeed");

        let out = tokio::time::timeout(Duration::from_secs(10), call_handle)
            .await
            .expect("tool call should complete")
            .expect("task should not panic");
        assert!(!out.is_error, "unexpected downstream error: {}", out.text);
        assert!(
            out.text.contains("ELIC_ACTION=accept;ENV=staging;CONFIRM=true"),
            "unexpected tool output: {}",
            out.text
        );

        state.mcp_hub.shutdown_all().await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
