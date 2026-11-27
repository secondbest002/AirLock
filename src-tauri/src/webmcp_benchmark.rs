use crate::mcp_server::build_router;
use crate::state::AppState;
use crate::workspace::{normalize_clean_path, WorkspaceProfile};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkScenarioResult {
    pub id: String,
    pub name: String,
    pub passed: bool,
    pub round_trips: usize,
    pub duration_ms: u64,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebMcpBenchmarkReport {
    pub total_passed: usize,
    pub total_failed: usize,
    pub schema_tools_count: usize,
    pub schema_bytes: usize,
    pub total_round_trips: usize,
    pub harness_nudges_observed: usize,
    pub elapsed_ms: u64,
    pub scenarios: Vec<BenchmarkScenarioResult>,
}

impl WebMcpBenchmarkReport {
    pub fn all_passed(&self) -> bool {
        self.total_failed == 0 && !self.scenarios.is_empty()
    }

    pub fn format_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# WebMCP Deterministic Benchmark Report\n\n");
        out.push_str(&format!(
            "- **Status:** {} ({} passed, {} failed)\n",
            if self.all_passed() { "PASS" } else { "FAIL" },
            self.total_passed,
            self.total_failed
        ));
        out.push_str(&format!(
            "- **Compact Schema:** {} tools ({} bytes)\n",
            self.schema_tools_count, self.schema_bytes
        ));
        out.push_str(&format!(
            "- **Total Round Trips:** {}\n",
            self.total_round_trips
        ));
        out.push_str(&format!(
            "- **Harness Nudges Observed:** {}\n",
            self.harness_nudges_observed
        ));
        out.push_str(&format!("- **Elapsed:** {} ms\n\n", self.elapsed_ms));
        out.push_str("| Scenario | Status | Round Trips | Duration (ms) | Notes |\n");
        out.push_str("|---|---|---:|---:|---|\n");
        for sc in &self.scenarios {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                sc.id,
                if sc.passed { "PASS" } else { "FAIL" },
                sc.round_trips,
                sc.duration_ms,
                sc.notes.replace('\n', " ")
            ));
        }
        out
    }
}

struct BenchServer {
    state: Arc<AppState>,
    token: String,
    client: reqwest::Client,
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for BenchServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_bench_server(root: &Path, query_suffix: &str) -> Result<BenchServer, String> {
    let state = Arc::new(AppState::new(root.to_path_buf()));
    let token = state.auth_token.read().await.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind benchmark listener: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("Failed to read local addr: {e}"))?;
    let router = build_router(Arc::clone(&state));
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(BenchServer {
        state,
        token,
        client: reqwest::Client::new(),
        base_url: format!("http://{addr}/mcp{query_suffix}"),
        handle,
    })
}

async fn rpc_post(
    srv: &BenchServer,
    session_id: Option<&str>,
    body: Value,
) -> Result<(Option<String>, Value), String> {
    let mut req = srv.client.post(&srv.base_url).bearer_auth(&srv.token);
    if let Some(sid) = session_id {
        req = req.header("mcp-session-id", sid);
    }
    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;
    let status = resp.status();
    let returned_sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {txt}"));
    }
    let json_val: Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid JSON RPC response: {e}"))?;
    Ok((returned_sid, json_val))
}

fn tool_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"].as_str().unwrap_or("")
}

fn tool_is_error(resp: &Value) -> bool {
    resp["result"]["isError"].as_bool().unwrap_or(false)
}

async fn scenario_compact_inspect_edit_verify(
    root: &Path,
) -> (BenchmarkScenarioResult, usize, usize) {
    let t0 = Instant::now();
    let _ = fs::remove_dir_all(root);
    let mut round_trips = 0usize;
    let mut schema_tools_count = 0usize;
    let mut schema_bytes = 0usize;

    let res: Result<String, String> = async {
        fs::create_dir_all(root.join("src")).map_err(|e| e.to_string())?;
        let target = root.join("src").join("lib.rs");
        fs::write(&target, "pub fn add(a: i32, b: i32) -> i32 {\n    a + b + 1\n}\n")
            .map_err(|e| e.to_string())?;

        let srv = start_bench_server(root, "?profile=compact").await?;

        let (_, list_resp) = rpc_post(
            &srv,
            None,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await?;
        round_trips += 1;
        let tools_arr = list_resp["result"]["tools"]
            .as_array()
            .ok_or("Missing result.tools array")?;
        schema_tools_count = tools_arr.len();
        schema_bytes = serde_json::to_string(tools_arr)
            .map_err(|e| e.to_string())?
            .len();
        if !(9..=10).contains(&schema_tools_count) {
            return Err(format!(
                "Compact schema expected 9..=10 tools, got {schema_tools_count}"
            ));
        }

        let (sid_opt, _) = rpc_post(
            &srv,
            None,
            json!({"jsonrpc":"2.0","id":2,"method":"initialize"}),
        )
        .await?;
        round_trips += 1;
        let sid = sid_opt.ok_or("initialize did not return mcp-session-id")?;

        let (_, inspect_resp) = rpc_post(
            &srv,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "batch_inspect",
                    "arguments": {
                        "read_slices": [{ "path": "src/lib.rs", "start_line": 1, "end_line": 20 }]
                    }
                }
            }),
        )
        .await?;
        round_trips += 1;
        if tool_is_error(&inspect_resp) || !tool_text(&inspect_resp).contains("a + b + 1") {
            return Err(format!("batch_inspect unexpected: {}", tool_text(&inspect_resp)));
        }

        let (_, edit_resp) = rpc_post(
            &srv,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "multi_surgical_edit",
                    "arguments": {
                        "files": [{
                            "path": "src/lib.rs",
                            "edits": [{ "search": "a + b + 1", "replace": "a + b" }]
                        }],
                        "verify_command": "echo bench_verify_ok"
                    }
                }
            }),
        )
        .await?;
        round_trips += 1;
        let edit_txt = tool_text(&edit_resp);
        if tool_is_error(&edit_resp) || !edit_txt.contains("bench_verify_ok") {
            return Err(format!("multi_surgical_edit failed: {edit_txt}"));
        }
        let disk = fs::read_to_string(&target).map_err(|e| e.to_string())?;
        if !disk.contains("    a + b\n") {
            return Err(format!("Disk file not updated properly:\n{disk}"));
        }

        Ok(format!(
            "Compact tools={schema_tools_count} ({schema_bytes}B), edited+verified in {round_trips} HTTP round-trips"
        ))
    }
    .await;

    let _ = fs::remove_dir_all(root);
    let (passed, notes) = match res {
        Ok(msg) => (true, msg),
        Err(err) => (false, err),
    };
    (
        BenchmarkScenarioResult {
            id: "compact_inspect_edit_verify_roundtrips".to_string(),
            name: "Compact inspect -> edit -> verify round-trips".to_string(),
            passed,
            round_trips,
            duration_ms: t0.elapsed().as_millis() as u64,
            notes,
        },
        schema_tools_count,
        schema_bytes,
    )
}

async fn scenario_slow_command_auto_promote_and_output_guard(root: &Path) -> BenchmarkScenarioResult {
    let t0 = Instant::now();
    let _ = fs::remove_dir_all(root);
    let mut round_trips = 0usize;

    let res: Result<String, String> = async {
        fs::create_dir_all(root).map_err(|e| e.to_string())?;
        let big_log = root.join("big.log");
        let payload = "bench payload line 0123456789abcdef\n".repeat(340);
        fs::write(&big_log, payload).map_err(|e| e.to_string())?;

        let srv = start_bench_server(root, "?profile=compact").await?;
        {
            let mut cfg = srv.state.config.write().await;
            cfg.webmcp_poll_wait_secs = 3;
            cfg.webmcp_max_output_chars = 3000;
        }

        let slow_cmd = if cfg!(target_os = "windows") {
            "ping -n 3 127.0.0.1"
        } else {
            "sleep 2"
        };

        let (_, run_resp) = rpc_post(
            &srv,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "run_command",
                    "arguments": { "action": "run", "command": slow_cmd, "auto_promote_secs": 1 }
                }
            }),
        )
        .await?;
        round_trips += 1;
        let run_txt = tool_text(&run_resp);
        if tool_is_error(&run_resp) || !run_txt.contains("AUTO_PROMOTED_TO_BACKGROUND_JOB") {
            return Err(format!("Expected auto-promotion, got:\n{run_txt}"));
        }
        let job_id = run_txt
            .lines()
            .find_map(|l| l.trim().strip_prefix("JOB_ID:"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| format!("Missing JOB_ID in:\n{run_txt}"))?;

        let (_, poll_resp) = rpc_post(
            &srv,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "run_command",
                    "arguments": { "action": "poll", "job_id": job_id, "from_line": 0 }
                }
            }),
        )
        .await?;
        round_trips += 1;
        if tool_text(&poll_resp).contains("STATUS: running") {
            let _ = rpc_post(
                &srv,
                None,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "run_command",
                        "arguments": { "action": "stop", "job_id": job_id }
                    }
                }),
            )
            .await;
            round_trips += 1;
        }

        let (_, read_resp) = rpc_post(
            &srv,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "read_file_range",
                    "arguments": { "path": "big.log", "start_line": 1, "end_line": 340 }
                }
            }),
        )
        .await?;
        round_trips += 1;
        let capped_len = tool_text(&read_resp).chars().count();
        if capped_len > 3000 {
            return Err(format!("Output guard exceeded 3000 chars ({capped_len})"));
        }

        Ok(format!(
            "Auto-promoted slow cmd + output capped at {capped_len} chars (<= 3000)"
        ))
    }
    .await;

    let _ = fs::remove_dir_all(root);
    let (passed, notes) = match res {
        Ok(msg) => (true, msg),
        Err(err) => (false, err),
    };
    BenchmarkScenarioResult {
        id: "slow_command_auto_promote_and_output_guard".to_string(),
        name: "Slow command auto-promote & Output Guard".to_string(),
        passed,
        round_trips,
        duration_ms: t0.elapsed().as_millis() as u64,
        notes,
    }
}

async fn scenario_session_harness_steering(root: &Path) -> (BenchmarkScenarioResult, usize) {
    let t0 = Instant::now();
    let _ = fs::remove_dir_all(root);
    let mut round_trips = 0usize;
    let mut nudges = 0usize;

    let res: Result<String, String> = async {
        fs::create_dir_all(root).map_err(|e| e.to_string())?;
        fs::write(root.join("notes.txt"), "alpha\nbeta\n").map_err(|e| e.to_string())?;
        let srv = start_bench_server(root, "?profile=compact").await?;

        let ws = crate::workspace::resolve_workspace(&srv.state, None, None)
            .await
            .map_err(|e| e)?;
        crate::checkpoints::exec_session_checkpoint(
            &srv.state,
            "WebMCP",
            &ws,
            &json!({
                "op": "save",
                "goal": "Bench checkpoint flow",
                "next_steps": ["Verify bench anchor"]
            }),
        )
        .await?;

        let (sid_opt, _) = rpc_post(
            &srv,
            None,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        )
        .await?;
        round_trips += 1;
        let sid = sid_opt.ok_or("initialize did not return mcp-session-id")?;

        // 1. Anchor on first call
        let (_, anchor_resp) = rpc_post(
            &srv,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "list_workspaces", "arguments": {} }
            }),
        )
        .await?;
        round_trips += 1;
        if !tool_text(&anchor_resp).contains("[HARNESS_HINT:")
            || !tool_text(&anchor_resp).contains("Verify bench anchor")
        {
            return Err(format!("Missing anchor hint:\n{}", tool_text(&anchor_resp)));
        }
        nudges += 1;

        // 2. Read-loop nudge on 2nd consecutive identical read
        for id in [3, 4] {
            let (_, r) = rpc_post(
                &srv,
                Some(&sid),
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": { "name": "read_file_range", "arguments": { "path": "notes.txt" } }
                }),
            )
            .await?;
            round_trips += 1;
            if id == 4 {
                let txt = tool_text(&r);
                if !txt.contains("[HARNESS_HINT:") || !txt.contains("notes.txt") {
                    return Err(format!("Missing read-loop hint:\n{txt}"));
                }
                nudges += 1;
            }
        }

        // 3. Error-loop nudge on 2nd consecutive identical command error
        let fail_cmd = if cfg!(target_os = "windows") {
            "cmd /C exit 9"
        } else {
            "exit 9"
        };
        for id in [5, 6] {
            let (_, r) = rpc_post(
                &srv,
                Some(&sid),
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": { "name": "run_command", "arguments": { "action": "run", "command": fail_cmd } }
                }),
            )
            .await?;
            round_trips += 1;
            if id == 6 {
                let txt = tool_text(&r);
                if !txt.contains("[HARNESS_HINT:") || !txt.contains("Repeated failure") {
                    return Err(format!("Missing error-loop hint:\n{txt}"));
                }
                nudges += 1;
            }
        }

        // 4. Context-rot checkpoint nudge by call 12 and reset on save
        let mut saw_rot = false;
        for id in 7..=13 {
            let (_, r) = rpc_post(
                &srv,
                Some(&sid),
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": { "name": "list_workspaces", "arguments": {} }
                }),
            )
            .await?;
            round_trips += 1;
            let txt = tool_text(&r);
            if txt.contains("[HARNESS_HINT:") && txt.contains("session_checkpoint(op=\"save\")") {
                saw_rot = true;
                nudges += 1;
                break;
            }
        }
        if !saw_rot {
            return Err("Expected context-rot checkpoint nudge by call 12".to_string());
        }

        let (_, save_resp) = rpc_post(
            &srv,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "tools/call",
                "params": { "name": "session_checkpoint", "arguments": { "op": "save", "goal": "bench done" } }
            }),
        )
        .await?;
        round_trips += 1;
        if tool_is_error(&save_resp) {
            return Err(format!("session_checkpoint save failed: {}", tool_text(&save_resp)));
        }

        let (_, after_save) = rpc_post(
            &srv,
            Some(&sid),
            json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": { "name": "list_workspaces", "arguments": {} }
            }),
        )
        .await?;
        round_trips += 1;
        if tool_text(&after_save).contains("session_checkpoint(op=\"save\")") {
            return Err("Checkpoint nudge persisted after save reset".to_string());
        }

        Ok(format!(
            "Observed {nudges} harness nudges (anchor, read-loop, error-loop, context-rot + reset)"
        ))
    }
    .await;

    let _ = fs::remove_dir_all(root);
    let (passed, notes) = match res {
        Ok(msg) => (true, msg),
        Err(err) => (false, err),
    };
    (
        BenchmarkScenarioResult {
            id: "session_harness_steering_and_checkpoint_rollover".to_string(),
            name: "Session harness steering & checkpoint rollover".to_string(),
            passed,
            round_trips,
            duration_ms: t0.elapsed().as_millis() as u64,
            notes,
        },
        nudges,
    )
}

async fn scenario_hybrid_batch_and_guardrail_isolation(root: &Path) -> BenchmarkScenarioResult {
    let t0 = Instant::now();
    let _ = fs::remove_dir_all(root);
    let mut round_trips = 0usize;

    let res: Result<String, String> = async {
        let dir_alpha = root.join("alpha");
        let dir_beta = root.join("beta");
        fs::create_dir_all(&dir_alpha).map_err(|e| e.to_string())?;
        fs::create_dir_all(&dir_beta).map_err(|e| e.to_string())?;
        fs::write(dir_alpha.join("notes.txt"), "before_batch\n").map_err(|e| e.to_string())?;
        fs::write(dir_alpha.join(".env"), "SECRET_KEY=bench_secret\n").map_err(|e| e.to_string())?;
        fs::write(dir_beta.join("locked.txt"), "immutable\n").map_err(|e| e.to_string())?;

        let (_, clean_alpha) = normalize_clean_path(&dir_alpha)?;
        let (_, clean_beta) = normalize_clean_path(&dir_beta)?;
        let srv = start_bench_server(root, "?profile=compact").await?;
        {
            let mut cfg = srv.state.config.write().await;
            cfg.workspaces = vec![
                WorkspaceProfile {
                    id: "ws-alpha".to_string(),
                    alias: "alpha".to_string(),
                    path: clean_alpha,
                    enabled: true,
                    guardrail_override: None,
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: vec![],
                    pinned: false,
                },
                WorkspaceProfile {
                    id: "ws-beta".to_string(),
                    alias: "beta".to_string(),
                    path: clean_beta,
                    enabled: true,
                    guardrail_override: Some("read_only".to_string()),
                    allow_sensitive_override: None,
                    extra_sensitive_patterns: vec![],
                    pinned: false,
                },
            ];
            cfg.default_workspace_id = Some("ws-alpha".to_string());
        }
        srv.state.sync_default_workspace_root().await;

        let (_, batch_resp) = rpc_post(
            &srv,
            None,
            json!([
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": "read_file_range", "arguments": { "path": "notes.txt" } }
                },
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": { "name": "list_workspaces", "arguments": {} }
                },
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "surgical_edit",
                        "arguments": {
                            "path": "notes.txt",
                            "edits": [{ "search": "before_batch", "replace": "after_batch" }]
                        }
                    }
                }
            ]),
        )
        .await?;
        round_trips += 1;
        let batch_arr = batch_resp
            .as_array()
            .ok_or("Expected array response for JSON-RPC batch")?;
        if batch_arr.len() != 3 || batch_arr.iter().any(tool_is_error) {
            return Err(format!("Hybrid batch failed: {batch_resp}"));
        }
        if !fs::read_to_string(dir_alpha.join("notes.txt"))
            .map_err(|e| e.to_string())?
            .contains("after_batch")
        {
            return Err("Batch write step did not persist edit".to_string());
        }

        let (_, sens_resp) = rpc_post(
            &srv,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "name": "read_file_range", "arguments": { "path": ".env" } }
            }),
        )
        .await?;
        round_trips += 1;
        if !tool_is_error(&sens_resp) {
            return Err("Sensitive .env read was NOT blocked".to_string());
        }

        let (_, ro_resp) = rpc_post(
            &srv,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "write_file_jailed",
                    "arguments": { "workspace": "beta", "path": "locked.txt", "content": "hijack" }
                }
            }),
        )
        .await?;
        round_trips += 1;
        if !tool_is_error(&ro_resp) {
            return Err("Write to read_only workspace @beta was NOT blocked".to_string());
        }

        Ok("Hybrid batch ordered OK; sensitive .env and read_only @beta writes blocked".to_string())
    }
    .await;

    let _ = fs::remove_dir_all(root);
    let (passed, notes) = match res {
        Ok(msg) => (true, msg),
        Err(err) => (false, err),
    };
    BenchmarkScenarioResult {
        id: "hybrid_batch_and_guardrail_isolation".to_string(),
        name: "Hybrid batch ordering & guardrail isolation".to_string(),
        passed,
        round_trips,
        duration_ms: t0.elapsed().as_millis() as u64,
        notes,
    }
}

pub async fn run_webmcp_deterministic_benchmark() -> WebMcpBenchmarkReport {
    let t_total = Instant::now();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = std::env::temp_dir().join(format!("airlock_webmcp_bench_{nonce}"));

    let (sc1, schema_tools_count, schema_bytes) =
        scenario_compact_inspect_edit_verify(&base.join("s1")).await;
    let sc2 = scenario_slow_command_auto_promote_and_output_guard(&base.join("s2")).await;
    let (sc3, harness_nudges_observed) =
        scenario_session_harness_steering(&base.join("s3")).await;
    let sc4 = scenario_hybrid_batch_and_guardrail_isolation(&base.join("s4")).await;

    let _ = fs::remove_dir_all(&base);
    let scenarios = vec![sc1, sc2, sc3, sc4];
    let total_passed = scenarios.iter().filter(|s| s.passed).count();
    let total_failed = scenarios.len().saturating_sub(total_passed);
    let total_round_trips = scenarios.iter().map(|s| s.round_trips).sum();

    WebMcpBenchmarkReport {
        total_passed,
        total_failed,
        schema_tools_count,
        schema_bytes,
        total_round_trips,
        harness_nudges_observed,
        elapsed_ms: t_total.elapsed().as_millis() as u64,
        scenarios,
    }
}

pub fn run_benchmark_cli(json_output: bool) -> i32 {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to initialize Tokio runtime: {e}");
            return 1;
        }
    };
    let report = rt.block_on(run_webmcp_deterministic_benchmark());
    if json_output {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("Failed to serialize benchmark JSON: {e}");
                return 1;
            }
        }
    } else {
        println!("{}", report.format_markdown());
    }
    if report.all_passed() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_webmcp_e2e_benchmark_suite_passes_all_scenarios() {
        let report = run_webmcp_deterministic_benchmark().await;
        assert!(report.all_passed(), "Benchmark failed: {:?}", report);
        assert_eq!(report.scenarios.len(), 4);
        assert_eq!(report.total_passed, 4);
        assert_eq!(report.total_failed, 0);
        assert!(report.schema_tools_count <= 10 && report.schema_tools_count >= 9);
        assert!(report.schema_bytes > 0);
        assert!(report.harness_nudges_observed >= 1);
    }
}
