use super::*;
use crate::state::ToolApprovalDecision;
use serde_json::json;
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn test_surgical_edit_diff_and_revert() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_tools_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let original_code = (1..=40)
        .map(|i| {
            if i == 20 {
                "    let status = \"OLD_VALUE\";".to_string()
            } else {
                format!("    let line_{i} = {i};")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let file_path = temp_dir.join("sample.rs");
    fs::write(&file_path, &original_code).unwrap();

    let state = AppState::new(temp_dir.clone());
    let args = json!({
        "path": "sample.rs",
        "edits": [
            {
                "search": "let status = \"OLD_VALUE\";",
                "replace": "let status = \"NEW_VALUE\";"
            }
        ]
    });

    let out = execute_tool(&state, "Test", "surgical_edit", args).await;
    assert!(!out.is_error, "Surgical edit failed: {}", out.text);
    assert!(out.text.contains("-    let status = \"OLD_VALUE\";"));
    assert!(out.text.contains("+    let status = \"NEW_VALUE\";"));

    let modified = fs::read_to_string(&file_path).unwrap();
    assert!(modified.contains("NEW_VALUE"));

    let logs = state.audit_logs.read().await;
    assert_eq!(logs.len(), 1);
    assert!(logs[0].tokens_saved > 50);
    let audit_id = logs[0].id.clone();
    drop(logs);

    let revert_msg = revert_audit_entry(&state, &audit_id, false).await.unwrap();
    assert!(revert_msg.contains("sample.rs"));
    let restored = fs::read_to_string(&file_path).unwrap();
    assert_eq!(restored, original_code);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_surgical_edit_preserves_crlf_line_endings() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_crlf_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let original_crlf = "fn greet() {\r\n    let x = 1;\r\n    let y = 2;\r\n}\r\n";
    let file_path = temp_dir.join("win.rs");
    fs::write(&file_path, original_crlf).unwrap();

    let state = AppState::new(temp_dir.clone());
    let args = json!({
        "path": "win.rs",
        "edits": [
            {
                "search": "    let x = 1;\n    let y = 2;",
                "replace": "    let x = 10;\n    let y = 20;"
            }
        ]
    });

    let out = execute_tool(&state, "Test", "surgical_edit", args).await;
    assert!(!out.is_error, "CRLF surgical edit failed: {}", out.text);

    let updated = fs::read_to_string(&file_path).unwrap();
    assert!(updated.contains("\r\n"), "CRLF line endings should be preserved");
    assert!(!updated.replace("\r\n", "").contains('\n'), "No stray LF expected");
    assert!(updated.contains("let x = 10;\r\n    let y = 20;"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_revert_conflict_detection_and_force() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_revert_conflict_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let file_path = temp_dir.join("doc.txt");
    fs::write(&file_path, "v1\n").unwrap();

    let state = AppState::new(temp_dir.clone());
    let out = execute_tool(
        &state,
        "Test",
        "surgical_edit",
        json!({
            "path": "doc.txt",
            "edits": [{ "search": "v1", "replace": "v2" }]
        }),
    )
    .await;
    assert!(!out.is_error);

    let audit_id = state.audit_logs.read().await[0].id.clone();

    // Simulate external modification after edit
    fs::write(&file_path, "v3_external_edit\n").unwrap();

    let conflict_res = revert_audit_entry(&state, &audit_id, false).await;
    assert!(conflict_res.is_err());
    assert!(conflict_res.unwrap_err().starts_with("CONFLICT:"));
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "v3_external_edit\n");

    let force_res = revert_audit_entry(&state, &audit_id, true).await;
    assert!(force_res.is_ok());
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "v1\n");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_sensitive_file_blocked_in_read_and_search() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_sensitive_guard_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join(".env"), "API_SECRET=super_secret_token_123\n").unwrap();
    fs::write(
        temp_dir.join("secrets.json"),
        "{\"secret\":\"super_secret_token_123\"}\n",
    )
    .unwrap();
    fs::write(
        temp_dir.join("app.ts"),
        "const tokenRef = \"super_secret_token_123\";\n",
    )
    .unwrap();

    let state = AppState::new(temp_dir.clone());

    let read_env = execute_tool(
        &state,
        "Test",
        "read_file_range",
        json!({ "path": ".env" }),
    )
    .await;
    assert!(read_env.is_error);
    assert!(read_env.text.contains("Sensitive File Guard"));

    let read_secrets = execute_tool(
        &state,
        "Test",
        "read_file_range",
        json!({ "path": "secrets.json" }),
    )
    .await;
    assert!(read_secrets.is_error);
    assert!(read_secrets.text.contains("Sensitive File Guard"));

    let search_out = execute_tool(
        &state,
        "Test",
        "smart_search",
        json!({ "query": "super_secret_token_123" }),
    )
    .await;
    assert!(!search_out.is_error);
    assert!(search_out.text.contains("app.ts"));
    assert!(!search_out.text.contains(".env"));
    assert!(!search_out.text.contains("secrets.json"));
    assert!(search_out.text.contains("Sensitive File Guard"));

    let direct_env_search = execute_tool(
        &state,
        "Test",
        "smart_search",
        json!({ "query": "super_secret_token_123", "path": ".env" }),
    )
    .await;
    assert!(!direct_env_search.is_error);
    assert!(!direct_env_search.text.contains("API_SECRET"));
    assert!(direct_env_search.text.contains("Sensitive File Guard"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_sensitive_guard_blocks_internal_dir_even_when_allow_sensitive_reads_enabled() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_internal_guard_test");
    let _ = fs::remove_dir_all(&temp_dir);
    let hub_dir = temp_dir.join(".smart-tauri-hub");
    fs::create_dir_all(&hub_dir).unwrap();
    fs::write(hub_dir.join("auth.json"), "{\"auth_token\":\"top_secret_marker\"}").unwrap();
    fs::write(hub_dir.join("config.json"), "{\"marker\":\"top_secret_marker\"}").unwrap();
    fs::write(hub_dir.join("secrets.dpapi"), "top_secret_marker_dpapi").unwrap();
    fs::write(temp_dir.join(".env"), "ENV_VAL=allowed_when_toggled\n").unwrap();
    fs::write(temp_dir.join("src_ok.txt"), "top_secret_marker_public_note\n").unwrap();

    let state = AppState::new(temp_dir.clone());
    state.allow_sensitive_reads.store(true, Ordering::SeqCst);

    // .env should now be readable because allow_sensitive_reads = true
    let env_read = execute_tool(&state, "Test", "read_file_range", json!({ "path": ".env" })).await;
    assert!(!env_read.is_error);
    assert!(env_read.text.contains("allowed_when_toggled"));

    // All .smart-tauri-hub internal files MUST remain permanently blocked!
    for internal_rel in [
        ".smart-tauri-hub/auth.json",
        ".smart-tauri-hub/config.json",
        ".smart-tauri-hub/secrets.dpapi",
    ] {
        let out = execute_tool(
            &state,
            "Test",
            "read_file_range",
            json!({ "path": internal_rel }),
        )
        .await;
        assert!(out.is_error, "Expected permanent block for {internal_rel}");
        assert!(
            out.text.contains("ditolak permanen"),
            "Unexpected message for {internal_rel}: {}",
            out.text
        );
    }

    // smart_search & find_files must never reveal .smart-tauri-hub even when allow_sensitive_reads = true
    let search_out = execute_tool(
        &state,
        "Test",
        "smart_search",
        json!({ "query": "top_secret_marker" }),
    )
    .await;
    assert!(!search_out.is_error);
    assert!(search_out.text.contains("src_ok.txt"));
    assert!(!search_out.text.contains("auth.json"));
    assert!(!search_out.text.contains("config.json"));
    assert!(!search_out.text.contains("secrets.dpapi"));

    let find_out = execute_tool(
        &state,
        "Test",
        "find_files",
        json!({ "pattern": "*" }),
    )
    .await;
    assert!(!find_out.is_error);
    assert!(!find_out.text.contains("auth.json"));
    assert!(!find_out.text.contains("secrets.dpapi"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_smart_search_skips_binary_game_assets() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_game_assets_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(temp_dir.join("cstrike")).unwrap();
    // Fake binary assets containing matching text or NUL bytes
    fs::write(temp_dir.join("cstrike").join("player.mdl"), b"IDST\x00sv_cheats\x00").unwrap();
    fs::write(temp_dir.join("cstrike").join("sound.wav"), b"RIFFsv_cheats").unwrap();
    fs::write(temp_dir.join("cstrike").join("client.dll"), b"MZ\x00sv_cheats").unwrap();
    // Text config file
    fs::write(
        temp_dir.join("cstrike").join("server.cfg"),
        "hostname \"CS Server\"\nsv_cheats 0\nmp_freezetime 3\n",
    )
    .unwrap();

    let state = AppState::new(temp_dir.clone());
    let out = execute_tool(
        &state,
        "Test",
        "smart_search",
        json!({ "query": "sv_cheats" }),
    )
    .await;
    assert!(!out.is_error);
    assert!(out.text.contains("server.cfg"));
    assert!(!out.text.contains("player.mdl"));
    assert!(!out.text.contains("sound.wav"));
    assert!(!out.text.contains("client.dll"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_find_files_and_code_outline() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_find_outline_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(temp_dir.join("src")).unwrap();
    fs::write(
        temp_dir.join("src").join("auth.rs"),
        "pub struct AuthUser {\n    id: u64,\n}\n\npub async fn verify_token(tok: &str) -> bool {\n    !tok.is_empty()\n}\n",
    )
    .unwrap();
    fs::write(temp_dir.join("README.md"), "# Demo\n").unwrap();

    let state = AppState::new(temp_dir.clone());

    let find_out = execute_tool(
        &state,
        "Test",
        "find_files",
        json!({ "pattern": "*.rs" }),
    )
    .await;
    assert!(!find_out.is_error, "find_files failed: {}", find_out.text);
    assert!(find_out.text.contains("src/auth.rs"));
    assert!(!find_out.text.contains("README.md"));

    let outline_out = execute_tool(
        &state,
        "Test",
        "code_outline",
        json!({ "path": "src/auth.rs" }),
    )
    .await;
    assert!(!outline_out.is_error, "code_outline failed: {}", outline_out.text);
    assert!(outline_out.text.contains("pub struct AuthUser"));
    assert!(outline_out.text.contains("pub async fn verify_token"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_file_ops_move_delete_and_revert() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_file_ops_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("a.txt"), "hello move and delete\n").unwrap();

    let state = AppState::new(temp_dir.clone());

    // 1. Move a.txt -> sub/b.txt
    let mv = execute_tool(
        &state,
        "Test",
        "file_ops",
        json!({ "action": "move", "path": "a.txt", "destination": "sub/b.txt" }),
    )
    .await;
    assert!(!mv.is_error, "move failed: {}", mv.text);
    assert!(!temp_dir.join("a.txt").exists());
    assert!(temp_dir.join("sub").join("b.txt").exists());

    let move_audit_id = state.audit_logs.read().await.last().unwrap().id.clone();
    revert_audit_entry(&state, &move_audit_id, false)
        .await
        .unwrap();
    assert!(temp_dir.join("a.txt").exists());
    assert!(!temp_dir.join("sub").join("b.txt").exists());

    // 2. Delete a.txt and revert back
    let del = execute_tool(
        &state,
        "Test",
        "file_ops",
        json!({ "action": "delete", "path": "a.txt" }),
    )
    .await;
    assert!(!del.is_error, "delete failed: {}", del.text);
    assert!(!temp_dir.join("a.txt").exists());

    let del_audit_id = state.audit_logs.read().await.last().unwrap().id.clone();
    revert_audit_entry(&state, &del_audit_id, false)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(temp_dir.join("a.txt")).unwrap(),
        "hello move and delete\n"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_guardrail_read_only_and_safe_edit_hitl() {
    use std::sync::Arc;
    let temp_dir = std::env::temp_dir().join("smart_mcp_guardrail_hitl_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("notes.txt"), "initial\n").unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));

    // 1. Read-Only blocks write_file_jailed but allows read_file_range
    *state.guardrail_mode.write().await = "read_only".to_string();
    let blocked = execute_tool(
        &state,
        "WebMCP",
        "write_file_jailed",
        json!({ "path": "notes.txt", "content": "hacked" }),
    )
    .await;
    assert!(blocked.is_error);
    assert!(blocked.text.contains("Read-Only"));

    let allowed_ro = execute_tool(
        &state,
        "WebMCP",
        "read_file_range",
        json!({ "path": "notes.txt" }),
    )
    .await;
    assert!(!allowed_ro.is_error);

    // 2. Safe-Edit allows surgical_edit immediately without HITL, but prompts HITL for run_command_compressed
    *state.guardrail_mode.write().await = "safe_edit".to_string();
    let edit_ok = execute_tool(
        &state,
        "WebMCP",
        "surgical_edit",
        json!({ "path": "notes.txt", "edits": [{ "search": "initial", "replace": "updated" }] }),
    )
    .await;
    assert!(!edit_ok.is_error);

    // Deny first HITL command
    let st_deny = state.clone();
    let deny_task = tokio::spawn(async move {
        for _ in 0..80 {
            let req_id = st_deny
                .pending_tool_approvals
                .read()
                .await
                .keys()
                .next()
                .cloned();
            if let Some(id) = req_id {
                resolve_tool_approval(&st_deny, &id, ToolApprovalDecision::Deny)
                    .await
                    .unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Timed out waiting for HITL approval");
    });

    let denied_cmd = execute_tool(
        &state,
        "WebMCP",
        "run_command_compressed",
        json!({ "command": "echo should_be_denied" }),
    )
    .await;
    deny_task.await.unwrap();
    assert!(denied_cmd.is_error);
    assert!(denied_cmd.text.contains("ditolak"));

    // ApproveForSession once, then second call runs immediately without waiting
    let st_allow = state.clone();
    let allow_task = tokio::spawn(async move {
        for _ in 0..80 {
            let req_id = st_allow
                .pending_tool_approvals
                .read()
                .await
                .keys()
                .next()
                .cloned();
            if let Some(id) = req_id {
                resolve_tool_approval(&st_allow, &id, ToolApprovalDecision::ApproveForSession)
                    .await
                    .unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Timed out waiting for HITL approval");
    });

    let ok1 = execute_tool(
        &state,
        "WebMCP",
        "run_command_compressed",
        json!({ "command": "echo first_ok" }),
    )
    .await;
    allow_task.await.unwrap();
    assert!(!ok1.is_error);

    let ok2 = execute_tool(
        &state,
        "WebMCP",
        "run_command_compressed",
        json!({ "command": "echo cached_session_ok" }),
    )
    .await;
    assert!(!ok2.is_error);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_revert_audit_group_reverse_order() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_group_revert_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("counter.txt"), "step0\n").unwrap();

    let state = AppState::new(temp_dir.clone());
    *state.active_turn_group_id.write().await = Some("turn-abc1".to_string());

    let e1 = execute_tool(
        &state,
        "LocalAgent",
        "surgical_edit",
        json!({ "path": "counter.txt", "edits": [{ "search": "step0", "replace": "step1" }] }),
    )
    .await;
    assert!(!e1.is_error);

    let e2 = execute_tool(
        &state,
        "LocalAgent",
        "surgical_edit",
        json!({ "path": "counter.txt", "edits": [{ "search": "step1", "replace": "step2" }] }),
    )
    .await;
    assert!(!e2.is_error);

    *state.active_turn_group_id.write().await = None;

    // Reverting in reverse order (e2 then e1) restores step2 -> step1 -> step0 cleanly with zero conflicts!
    let msg = revert_audit_group(&state, "turn-abc1", false).await.unwrap();
    assert!(msg.contains("2 perubahan"));
    assert_eq!(fs::read_to_string(temp_dir.join("counter.txt")).unwrap(), "step0\n");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_multi_workspace_cross_repo_read_and_scoped_revert() {
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};

    let base = std::env::temp_dir().join("smart_mcp_multi_ws_revert_test");
    let _ = fs::remove_dir_all(&base);
    let dir_api = base.join("api");
    let dir_web = base.join("web");
    fs::create_dir_all(&dir_api).unwrap();
    fs::create_dir_all(&dir_web).unwrap();

    fs::write(dir_api.join("server.rs"), "pub fn port() -> u16 { 8080 }\n").unwrap();
    fs::write(dir_web.join("app.ts"), "export const ENV = \"dev\";\n").unwrap();

    let (_, clean_api) = normalize_clean_path(&dir_api).unwrap();
    let (_, clean_web) = normalize_clean_path(&dir_web).unwrap();

    let state = AppState::new(dir_api.clone());
    {
        let mut cfg = state.config.write().await;
        cfg.workspaces = vec![
            WorkspaceProfile {
                id: "ws-api".to_string(),
                alias: "api".to_string(),
                path: clean_api,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: true,
            },
            WorkspaceProfile {
                id: "ws-web".to_string(),
                alias: "web".to_string(),
                path: clean_web,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];
        cfg.default_workspace_id = Some("ws-api".to_string());
    }
    state.sync_default_workspace_root().await;

    // Cross-workspace batch read
    let read_out = execute_tool(
        &state,
        "Test",
        "read_file_range",
        json!({
            "batch_paths": [
                { "workspace": "api", "path": "server.rs" },
                { "workspace": "web", "path": "app.ts" }
            ]
        }),
    )
    .await;
    assert!(!read_out.is_error, "Cross-repo batch read failed: {}", read_out.text);
    assert!(read_out.text.contains("@api:server.rs"));
    assert!(read_out.text.contains("8080"));
    assert!(read_out.text.contains("@web:app.ts"));
    assert!(read_out.text.contains("ENV"));

    // Edit file in @web while default is still @api
    let edit_web = execute_tool(
        &state,
        "Test",
        "surgical_edit",
        json!({
            "workspace": "web",
            "path": "app.ts",
            "edits": [{ "search": "\"dev\"", "replace": "\"prod\"" }]
        }),
    )
    .await;
    assert!(!edit_web.is_error, "Edit @web failed: {}", edit_web.text);
    assert_eq!(
        fs::read_to_string(dir_web.join("app.ts")).unwrap(),
        "export const ENV = \"prod\";\n"
    );

    let audit_id = {
        let logs = state.audit_logs.read().await;
        let last = logs.last().unwrap();
        assert_eq!(last.workspace_alias.as_deref(), Some("web"));
        assert_eq!(last.workspace_id.as_deref(), Some("ws-web"));
        last.id.clone()
    };

    // Revert @web entry while default workspace remains @api — MUST revert @web without touching @api!
    let revert_msg = revert_audit_entry(&state, &audit_id, false).await.unwrap();
    assert!(revert_msg.contains("app.ts"));
    assert_eq!(
        fs::read_to_string(dir_web.join("app.ts")).unwrap(),
        "export const ENV = \"dev\";\n"
    );
    assert_eq!(
        fs::read_to_string(dir_api.join("server.rs")).unwrap(),
        "pub fn port() -> u16 { 8080 }\n"
    );

    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_per_workspace_guardrail_and_global_read_only_override() {
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};

    let base = std::env::temp_dir().join("smart_mcp_ws_guardrail_test");
    let _ = fs::remove_dir_all(&base);
    let dir_api = base.join("api");
    let dir_web = base.join("web");
    fs::create_dir_all(&dir_api).unwrap();
    fs::create_dir_all(&dir_web).unwrap();

    let (_, clean_api) = normalize_clean_path(&dir_api).unwrap();
    let (_, clean_web) = normalize_clean_path(&dir_web).unwrap();

    let state = AppState::new(dir_api.clone());
    *state.guardrail_mode.write().await = "autonomous".to_string();
    {
        let mut cfg = state.config.write().await;
        cfg.workspaces = vec![
            WorkspaceProfile {
                id: "ws-api".to_string(),
                alias: "api".to_string(),
                path: clean_api,
                enabled: true,
                guardrail_override: Some("autonomous".to_string()),
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
            WorkspaceProfile {
                id: "ws-web".to_string(),
                alias: "web".to_string(),
                path: clean_web,
                enabled: true,
                guardrail_override: Some("read_only".to_string()),
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];
        cfg.default_workspace_id = Some("ws-api".to_string());
    }
    state.sync_default_workspace_root().await;

    // @api autonomous succeeds, @web read_only is blocked
    let ok_api = execute_tool(
        &state,
        "WebMCP",
        "write_file_jailed",
        json!({ "workspace": "api", "path": "ok.txt", "content": "hello\n" }),
    )
    .await;
    assert!(!ok_api.is_error, "Autonomous @api write failed: {}", ok_api.text);

    let err_web = execute_tool(
        &state,
        "WebMCP",
        "write_file_jailed",
        json!({ "workspace": "web", "path": "blocked.txt", "content": "nope\n" }),
    )
    .await;
    assert!(err_web.is_error);
    assert!(err_web.text.contains("Read-Only"));
    assert!(err_web.text.contains("@web"));

    // Global read_only ALWAYS overrides @api's autonomous override!
    *state.guardrail_mode.write().await = "read_only".to_string();
    let err_api_global = execute_tool(
        &state,
        "WebMCP",
        "write_file_jailed",
        json!({ "workspace": "api", "path": "blocked2.txt", "content": "nope\n" }),
    )
    .await;
    assert!(err_api_global.is_error);
    assert!(err_api_global.text.contains("Read-Only"));

    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_hitl_session_approval_scoped_per_workspace() {
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};
    use std::sync::Arc;

    let base = std::env::temp_dir().join("smart_mcp_ws_hitl_scope_test");
    let _ = fs::remove_dir_all(&base);
    let dir_a = base.join("a");
    let dir_b = base.join("b");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();

    let (_, clean_a) = normalize_clean_path(&dir_a).unwrap();
    let (_, clean_b) = normalize_clean_path(&dir_b).unwrap();

    let state = Arc::new(AppState::new(dir_a.clone()));
    *state.guardrail_mode.write().await = "safe_edit".to_string();
    {
        let mut cfg = state.config.write().await;
        cfg.workspaces = vec![
            WorkspaceProfile {
                id: "ws-a".to_string(),
                alias: "alpha".to_string(),
                path: clean_a,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
            WorkspaceProfile {
                id: "ws-b".to_string(),
                alias: "beta".to_string(),
                path: clean_b,
                enabled: true,
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];
        cfg.default_workspace_id = Some("ws-a".to_string());
    }
    state.sync_default_workspace_root().await;

    // ApproveForSession on @alpha
    let st_a = state.clone();
    let t_a = tokio::spawn(async move {
        for _ in 0..80 {
            let req_id = st_a
                .pending_tool_approvals
                .read()
                .await
                .keys()
                .next()
                .cloned();
            if let Some(id) = req_id {
                resolve_tool_approval(&st_a, &id, ToolApprovalDecision::ApproveForSession)
                    .await
                    .unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Timed out waiting for HITL request on @alpha");
    });

    let res_a = execute_tool(
        &state,
        "WebMCP",
        "run_command_compressed",
        json!({ "workspace": "alpha", "command": "echo alpha_ok" }),
    )
    .await;
    t_a.await.unwrap();
    assert!(!res_a.is_error, "Alpha command failed: {}", res_a.text);

    // Calling same tool on @beta MUST still prompt HITL (Deny it to prove prompt fired!)
    let st_b = state.clone();
    let t_b = tokio::spawn(async move {
        for _ in 0..80 {
            let info_opt = st_b
                .pending_tool_approvals
                .read()
                .await
                .iter()
                .next()
                .map(|(k, (info, _))| (k.clone(), info.summary.clone()));
            if let Some((id, summary)) = info_opt {
                assert!(summary.contains("@beta"));
                resolve_tool_approval(&st_b, &id, ToolApprovalDecision::Deny)
                    .await
                    .unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Expected fresh HITL prompt for @beta");
    });

    let res_b = execute_tool(
        &state,
        "WebMCP",
        "run_command_compressed",
        json!({ "workspace": "beta", "command": "echo beta_blocked" }),
    )
    .await;
    t_b.await.unwrap();
    assert!(res_b.is_error);
    assert!(res_b.text.contains("ditolak"));

    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_batch_inspect_and_multi_surgical_edit_atomic_preflight() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_compound_tools_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(temp_dir.join("src")).unwrap();
    fs::write(
        temp_dir.join("src").join("one.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    fs::write(
        temp_dir.join("src").join("two.rs"),
        "pub fn beta() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    let state = AppState::new(temp_dir.clone());

    // 1. batch_inspect runs overview + outlines + searches concurrently
    let inspect_out = execute_tool(
        &state,
        "Test",
        "batch_inspect",
        json!({
            "include_overview": true,
            "outline_paths": ["src/one.rs", "src/two.rs"],
            "searches": [{ "query": "pub fn" }]
        }),
    )
    .await;
    assert!(!inspect_out.is_error, "batch_inspect failed: {}", inspect_out.text);
    assert!(inspect_out.text.contains("=== OVERVIEW"));
    assert!(inspect_out.text.contains("=== OUTLINES ==="));
    assert!(inspect_out.text.contains("pub fn alpha"));
    assert!(inspect_out.text.contains("pub fn beta"));
    assert!(inspect_out.text.contains("=== SEARCHES ==="));

    // 2. multi_surgical_edit atomic pre-flight: fail if 2nd file's search block doesn't match -> 1st file UNTOUCHED!
    let atomic_fail = execute_tool(
        &state,
        "Test",
        "multi_surgical_edit",
        json!({
            "files": [
                {
                    "path": "src/one.rs",
                    "edits": [{ "search": "    1", "replace": "    100" }]
                },
                {
                    "path": "src/two.rs",
                    "edits": [{ "search": "NON_EXISTENT_BLOCK", "replace": "999" }]
                }
            ]
        }),
    )
    .await;
    assert!(atomic_fail.is_error);
    assert_eq!(
        fs::read_to_string(temp_dir.join("src").join("one.rs")).unwrap(),
        "pub fn alpha() -> i32 {\n    1\n}\n",
        "Pre-flight failure must leave first file untouched!"
    );

    // 3. Valid multi_surgical_edit commits both under one group_id, and revert_audit_group reverts both cleanly!
    let atomic_ok = execute_tool(
        &state,
        "Test",
        "multi_surgical_edit",
        json!({
            "files": [
                {
                    "path": "src/one.rs",
                    "edits": [{ "search": "    1", "replace": "    10" }]
                },
                {
                    "path": "src/two.rs",
                    "edits": [{ "search": "    2", "replace": "    20" }]
                }
            ]
        }),
    )
    .await;
    assert!(!atomic_ok.is_error, "multi_surgical_edit failed: {}", atomic_ok.text);
    assert!(fs::read_to_string(temp_dir.join("src").join("one.rs"))
        .unwrap()
        .contains("10"));
    assert!(fs::read_to_string(temp_dir.join("src").join("two.rs"))
        .unwrap()
        .contains("20"));

    let gid = state
        .audit_logs
        .read()
        .await
        .last()
        .unwrap()
        .group_id
        .clone()
        .unwrap();
    let rev = revert_audit_group(&state, &gid, false).await.unwrap();
    assert!(rev.contains("2 perubahan"));
    assert_eq!(
        fs::read_to_string(temp_dir.join("src").join("one.rs")).unwrap(),
        "pub fn alpha() -> i32 {\n    1\n}\n"
    );
    assert_eq!(
        fs::read_to_string(temp_dir.join("src").join("two.rs")).unwrap(),
        "pub fn beta() -> i32 {\n    2\n}\n"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_patch_and_verify_records_rollback_even_when_verify_promoted() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_patch_promote_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("lib.rs"), "pub fn val() -> i32 { 1 }\n").unwrap();

    let state = AppState::new(temp_dir.clone());
    let slow_verify = if cfg!(target_os = "windows") {
        "ping -n 5 127.0.0.1"
    } else {
        "sleep 4"
    };

    let out = execute_tool(
        &state,
        "WebMCP",
        "patch_and_verify",
        json!({
            "path": "lib.rs",
            "edits": [{ "search": "{ 1 }", "replace": "{ 42 }" }],
            "verify_command": slow_verify,
            "auto_promote_secs": 1
        }),
    )
    .await;

    assert!(!out.is_error, "Promoted patch_and_verify should succeed: {}", out.text);
    assert!(out.text.contains("AUTO_PROMOTED_VERIFY_JOB"));
    assert!(out.text.contains("JOB_ID: job-"));
    assert_eq!(
        fs::read_to_string(temp_dir.join("lib.rs")).unwrap(),
        "pub fn val() -> i32 { 42 }\n"
    );

    // Even while verify was promoted to background, rollback entry is ALREADY recorded and works!
    let audit_id = state.audit_logs.read().await.last().unwrap().id.clone();
    revert_audit_entry(&state, &audit_id, false).await.unwrap();
    assert_eq!(
        fs::read_to_string(temp_dir.join("lib.rs")).unwrap(),
        "pub fn val() -> i32 { 1 }\n"
    );

    crate::jobs::stop_all_jobs(&state).await;
    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_plan_first_mode_blocks_mutative_tools_until_approved() {
    use crate::agent_manager::RunContext;
    use std::sync::{atomic::AtomicUsize, Arc};

    let temp_dir = std::env::temp_dir().join("smart_mcp_plan_first_gate_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("app.rs"), "fn v() -> i32 { 1 }\n").unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    let ws = resolve_workspace(&state, None, None).await.unwrap();
    let run_ctx = RunContext {
        run_id: "run_plan_test".to_string(),
        parent_run_id: None,
        session_id: "sess_plan_1".to_string(),
        workspace_id: ws.id.clone(),
        workspace_alias: ws.alias.clone(),
        workspace_clean_path: ws.clean_path.clone(),
        execution_mode: "plan_first".to_string(),
        depth: 0,
        turn_group_id: "turn_plan_1".to_string(),
        subagent_counter: Arc::new(AtomicUsize::new(0)),
        acp_event_tx: None,
    };

    // 1. Direct surgical_edit is blocked while plan_first & unapproved
    let blocked = execute_tool_with_run_context(
        state.clone(),
        "LocalAgent",
        "surgical_edit",
        json!({
            "path": "app.rs",
            "edits": [{ "search": "{ 1 }", "replace": "{ 2 }" }]
        }),
        None,
        Some(&run_ctx),
    )
    .await;
    assert!(blocked.is_error);
    assert!(blocked.text.contains("Mode Plan-First aktif"));
    assert_eq!(fs::read_to_string(temp_dir.join("app.rs")).unwrap(), "fn v() -> i32 { 1 }\n");

    // 2. Agent creates ImplementationPlan via upsert_artifact and calls request_plan_approval
    let upsert = execute_tool_with_run_context(
        state.clone(),
        "LocalAgent",
        "upsert_artifact",
        json!({
            "kind": "implementation_plan",
            "title": "Return 2",
            "content_md": "# Plan\nUpdate app.rs"
        }),
        None,
        Some(&run_ctx),
    )
    .await;
    assert!(!upsert.is_error, "upsert_artifact failed: {}", upsert.text);

    let req = execute_tool_with_run_context(
        state.clone(),
        "LocalAgent",
        "request_plan_approval",
        json!({ "summary": "Ready for review" }),
        None,
        Some(&run_ctx),
    )
    .await;
    assert!(!req.is_error, "request_plan_approval failed: {}", req.text);
    assert!(req.text.contains("AWAITING_PLAN_APPROVAL"));

    // Still blocked until user approves
    let still_blocked = execute_tool_with_run_context(
        state.clone(),
        "LocalAgent",
        "surgical_edit",
        json!({
            "path": "app.rs",
            "edits": [{ "search": "{ 1 }", "replace": "{ 2 }" }]
        }),
        None,
        Some(&run_ctx),
    )
    .await;
    assert!(still_blocked.is_error);

    // 3. User approves artifact -> surgical_edit succeeds cleanly!
    let arts = state
        .artifact_store
        .list_session_artifacts(&ws.clean_path, &run_ctx.session_id)
        .await;
    state
        .artifact_store
        .approve_artifact(&ws.clean_path, &run_ctx.session_id, &arts[0].id)
        .await
        .unwrap();

    let allowed = execute_tool_with_run_context(
        state.clone(),
        "LocalAgent",
        "surgical_edit",
        json!({
            "path": "app.rs",
            "edits": [{ "search": "{ 1 }", "replace": "{ 2 }" }]
        }),
        None,
        Some(&run_ctx),
    )
    .await;
    assert!(!allowed.is_error, "Edit should succeed after approve_artifact: {}", allowed.text);
    assert_eq!(fs::read_to_string(temp_dir.join("app.rs")).unwrap(), "fn v() -> i32 { 2 }\n");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_command_execution_policy_allowlist_and_hitl_in_autonomous_mode() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_cmd_policy_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    *state.guardrail_mode.write().await = "autonomous".to_string();

    // 1. Strict Allowlist mode blocks non-allowlisted commands & chaining even in autonomous mode
    {
        let mut cfg = state.config.write().await;
        cfg.command_execution_policy = "allowlist".to_string();
        cfg.command_allowlist = vec!["echo".to_string()];
    }

    let allowed = execute_tool(
        &state,
        "LocalAgent",
        "run_command_compressed",
        json!({ "command": "echo allowlist_ok" }),
    )
    .await;
    assert!(!allowed.is_error, "Allowlisted command should succeed: {}", allowed.text);
    assert!(allowed.text.contains("allowlist_ok"));

    let blocked_chain = execute_tool(
        &state,
        "LocalAgent",
        "run_command_compressed",
        json!({ "command": "echo ok && echo chained" }),
    )
    .await;
    assert!(blocked_chain.is_error);
    assert!(blocked_chain.text.contains("Command Policy (Allowlist)"));

    let blocked_prefix = execute_tool(
        &state,
        "LocalAgent",
        "run_command_compressed",
        json!({ "command": "whoami" }),
    )
    .await;
    assert!(blocked_prefix.is_error);
    assert!(blocked_prefix.text.contains("Command Policy (Allowlist)"));

    // 2. Always HITL mode triggers HITL prompt for shell commands even in autonomous mode
    {
        let mut cfg = state.config.write().await;
        cfg.command_execution_policy = "hitl".to_string();
    }

    let state_bg = state.clone();
    let resolver = tokio::spawn(async move {
        for _ in 0..40 {
            let req_id = state_bg
                .pending_tool_approvals
                .read()
                .await
                .keys()
                .next()
                .cloned();
            if let Some(id) = req_id {
                resolve_tool_approval(&state_bg, &id, ToolApprovalDecision::Deny)
                    .await
                    .unwrap();
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    });

    let denied = execute_tool(
        &state,
        "WebMCP",
        "run_command_compressed",
        json!({ "command": "echo hitl_enforced" }),
    )
    .await;
    let resolved = resolver.await.unwrap();
    assert!(resolved, "Expected HITL approval request in command_execution_policy=hitl");
    assert!(denied.is_error);
    assert!(denied.text.contains("ditolak oleh user"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_webmcp_poll_command_job_long_polls_to_completion() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_webmcp_poll_wait_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = AppState::new(temp_dir.clone());
    let started = execute_tool(
        &state,
        "WebMCP",
        "start_command_job",
        json!({ "command": "echo webmcp_long_poll_ok" }),
    )
    .await;
    assert!(!started.is_error, "start_command_job failed: {}", started.text);
    let job_id = started
        .text
        .lines()
        .find_map(|l| l.strip_prefix("JOB_STARTED: "))
        .expect("JOB_STARTED line missing")
        .trim()
        .to_string();

    let polled = execute_tool(
        &state,
        "WebMCP",
        "poll_command_job",
        json!({ "job_id": job_id, "from_line": 0 }),
    )
    .await;
    assert!(!polled.is_error, "poll_command_job failed: {}", polled.text);
    assert!(polled.text.contains("STATUS: completed"), "got:\n{}", polled.text);
    assert!(polled.text.contains("webmcp_long_poll_ok"), "got:\n{}", polled.text);

    let _ = fs::remove_dir_all(&temp_dir);
}
