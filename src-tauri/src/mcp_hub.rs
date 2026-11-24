use crate::config::DownstreamMcpConfig;
use crate::security::compress_output;
use crate::state::{AppState, AuditEntry, DownstreamMcpSummary};
use crate::tools::{enforce_guardrail_gate, ToolOutput};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, RwLock};

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

    async fn handshake_and_list_tools(&mut self) -> Result<Vec<Value>, String> {
        let init_resp = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
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
    pub stdio: Arc<Mutex<Option<StdioMcpProcess>>>,
}

impl Default for DownstreamServerRuntime {
    fn default() -> Self {
        Self {
            status: "idle".to_string(),
            tools: Vec::new(),
            last_error: None,
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

async fn http_rpc_post(
    client: &reqwest::Client,
    cfg: &DownstreamMcpConfig,
    session_id: Option<&str>,
    payload: &Value,
) -> Result<(Option<String>, Value), String> {
    let mut req = client
        .post(cfg.url.trim())
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json");
    if !cfg.bearer_token.trim().is_empty() {
        req = req.bearer_auth(cfg.bearer_token.trim());
    }
    if let Some(sid) = session_id {
        req = req.header("mcp-session-id", sid);
    }

    let resp = req
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("Koneksi HTTP MCP '{}' gagal: {e}", cfg.name))?;

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
        .map_err(|e| format!("Gagal membaca respons HTTP MCP '{}': {e}", cfg.name))?;

    if !status.is_success() {
        return Err(format!(
            "HTTP MCP '{}' error ({}): {}",
            cfg.name,
            status,
            text.chars().take(200).collect::<String>()
        ));
    }

    let parsed = parse_http_mcp_body(&text)?;
    Ok((new_sid, parsed))
}

impl McpHubManager {
    pub async fn refresh_server(&self, cfg: &DownstreamMcpConfig) -> Result<usize, String> {
        if !cfg.enabled {
            let mut rt_map = self.runtimes.write().await;
            let entry = rt_map.entry(cfg.id.clone()).or_default();
            entry.status = "disabled".to_string();
            entry.tools.clear();
            entry.last_error = None;
            let stdio_arc = entry.stdio.clone();
            drop(rt_map);
            *stdio_arc.lock().await = None;
            return Ok(0);
        }

        let res = if cfg.transport.eq_ignore_ascii_case("stdio") {
            let stdio_arc = {
                let mut rt_map = self.runtimes.write().await;
                let entry = rt_map.entry(cfg.id.clone()).or_default();
                entry.stdio.clone()
            };
            let mut guard = stdio_arc.lock().await;
            *guard = None;
            let mut proc = StdioMcpProcess::spawn(cfg).await?;
            let tools = proc.handshake_and_list_tools().await?;
            *guard = Some(proc);
            Ok(tools)
        } else {
            let url = cfg.url.trim();
            if url.is_empty() {
                Err("URL HTTP MCP tidak boleh kosong.".to_string())
            } else {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(15))
                    .build()
                    .map_err(|e| format!("HTTP client error: {e}"))?;

                let (sid, init_val) = http_rpc_post(
                    &client,
                    cfg,
                    None,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {},
                            "clientInfo": { "name": "smart-tauri-hub", "version": "0.1.0" }
                        }
                    }),
                )
                .await?;
                if let Some(err) = init_val.get("error") {
                    return Err(format!("Initialize error: {err}"));
                }

                let _ = http_rpc_post(
                    &client,
                    cfg,
                    sid.as_deref(),
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    }),
                )
                .await;

                let (_, list_val) = http_rpc_post(
                    &client,
                    cfg,
                    sid.as_deref(),
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/list"
                    }),
                )
                .await?;
                if let Some(err) = list_val.get("error") {
                    return Err(format!("tools/list error: {err}"));
                }
                Ok(list_val["result"]["tools"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default())
            }
        };

        let mut rt_map = self.runtimes.write().await;
        let entry = rt_map.entry(cfg.id.clone()).or_default();
        match res {
            Ok(tools) => {
                let count = tools.len();
                entry.status = "connected".to_string();
                entry.tools = tools;
                entry.last_error = None;
                Ok(count)
            }
            Err(err) => {
                entry.status = "error".to_string();
                entry.last_error = Some(err.clone());
                Err(err)
            }
        }
    }

    pub async fn refresh_all(&self, configs: &[DownstreamMcpConfig]) {
        for cfg in configs {
            let _ = self.refresh_server(cfg).await;
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
                    has_bearer_token: !cfg.bearer_token.trim().is_empty(),
                    command: cfg.command.clone(),
                    args: cfg.args.clone(),
                    enabled: cfg.enabled,
                    status,
                    tools_count: rt.map(|r| r.tools.len()).unwrap_or(0),
                    last_error: rt.and_then(|r| r.last_error.clone()),
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

        let configs = state.config.read().await.downstream_mcps.clone();
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
            proc.request(
                "tools/call",
                json!({ "name": raw_tool, "arguments": args }),
                timeout_secs,
            )
            .await
        } else {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    return ToolOutput {
                        text: format!("HTTP client error: {e}"),
                        is_error: true,
                    };
                }
            };
            http_rpc_post(
                &client,
                &cfg,
                None,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 100,
                    "method": "tools/call",
                    "params": { "name": raw_tool, "arguments": args }
                }),
            )
            .await
            .map(|(_, val)| val)
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
