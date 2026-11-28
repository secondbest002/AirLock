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

#[tokio::test]
async fn test_webmcp_output_budget_preserves_control_headers_and_errors() {
    // 1. Unit test for compress_for_webmcp_budget with 20,000-char payload + control headers + middle error
    let mut big = String::from(
        "AUTO_PROMOTED_TO_BACKGROUND_JOB\nJOB_ID: job-123\nWORKSPACE: @repo\nSTATUS: running\nNEXT_FROM_LINE: 42\n",
    );
    for i in 1..=320 {
        if i == 160 {
            big.push_str("error[E0308]: mismatched types expected `u32`, found `String`\n");
        } else {
            big.push_str(&format!(
                "Compiling crate_step_{i:03} with verbose build metadata and artifact checksum 0123456789abcdef\n"
            ));
        }
    }
    assert!(big.chars().count() > 20_000);

    let compressed = compress_for_webmcp_budget("run_command_compressed", &big, 6000);
    assert!(
        compressed.chars().count() <= 6000,
        "Expected <=6000 chars, got {}",
        compressed.chars().count()
    );
    assert!(compressed.contains("JOB_ID: job-123"));
    assert!(compressed.contains("STATUS: running"));
    assert!(compressed.contains("NEXT_FROM_LINE: 42"));
    assert!(compressed.contains("error[E0308]: mismatched types"));
    assert!(compressed.contains("WebMCP Output Guard:"));

    // 2. Single giant line (minified JS/JSON) still stays strictly within budget
    let giant_line = "x".repeat(20_000);
    let clipped_line = compress_for_webmcp_budget("read_file_range", &giant_line, 6000);
    assert!(clipped_line.chars().count() <= 6000);
    assert!(clipped_line.contains("WebMCP Output Guard:"));

    // 3. End-to-end execute_tool applies output guard ONLY for source == "WebMCP", not "LocalAgent"
    let temp_dir = std::env::temp_dir().join("smart_mcp_webmcp_budget_e2e_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let long_file = (1..=250)
        .map(|i| format!("pub fn fn_{i:03}() -> usize {{ /* padding_for_webmcp_budget_test_{i:03} */ {i} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(temp_dir.join("big.rs"), &long_file).unwrap();

    let state = AppState::new(temp_dir.clone());
    let web_out = execute_tool(
        &state,
        "WebMCP",
        "read_file_range",
        json!({ "path": "big.rs", "start_line": 1, "end_line": 250 }),
    )
    .await;
    assert!(!web_out.is_error);
    assert!(web_out.text.chars().count() <= 6000);
    assert!(web_out.text.contains("WebMCP Output Guard:"));

    let local_out = execute_tool(
        &state,
        "LocalAgent",
        "read_file_range",
        json!({ "path": "big.rs", "start_line": 1, "end_line": 250 }),
    )
    .await;
    assert!(!local_out.is_error);
    assert!(local_out.text.chars().count() > 6000);
    assert!(!local_out.text.contains("WebMCP Output Guard:"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_batch_inspect_supports_read_slices_and_symbol_reads() {
    use crate::workspace::{normalize_clean_path, WorkspaceProfile};

    let base = std::env::temp_dir().join("smart_mcp_batch_inspect_slices_sym_test");
    let _ = fs::remove_dir_all(&base);
    let alpha_dir = base.join("alpha");
    let beta_dir = base.join("beta");
    fs::create_dir_all(alpha_dir.join("src")).unwrap();
    fs::create_dir_all(&beta_dir).unwrap();

    let mut lib_lines = vec![
        "/// Target helper doc".to_string(),
        "pub fn target_helper(x: i32) -> i32 {".to_string(),
        "    x + 42".to_string(),
        "}".to_string(),
    ];
    for i in 5..=240 {
        lib_lines.push(format!("pub fn filler_{i}() -> usize {{ {i} }}"));
    }
    fs::write(alpha_dir.join("src").join("lib.rs"), lib_lines.join("\n")).unwrap();
    fs::write(beta_dir.join("secret.txt"), "beta_only_content").unwrap();

    let (_, clean_alpha) = normalize_clean_path(&alpha_dir).unwrap();
    let (_, clean_beta) = normalize_clean_path(&beta_dir).unwrap();

    let state = Arc::new(AppState::new(alpha_dir.clone()));
    {
        let mut cfg = state.config.write().await;
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
                guardrail_override: None,
                allow_sensitive_override: None,
                extra_sensitive_patterns: vec![],
                pinned: false,
            },
        ];
        cfg.default_workspace_id = Some("ws-alpha".to_string());
    }
    state.sync_default_workspace_root().await;

    // 1. Concurrent overview + find_patterns + outline_paths + searches + read_slices (WebMCP 160-line cap) + symbol_reads
    let res = execute_tool_with_run_context(
        Arc::clone(&state),
        "WebMCP",
        "batch_inspect",
        json!({
            "include_overview": true,
            "find_patterns": [{ "pattern": "*.rs" }],
            "outline_paths": ["src/lib.rs"],
            "searches": [{ "query": "x + 42" }],
            "read_slices": [{ "path": "src/lib.rs", "start_line": 1, "end_line": 230 }],
            "symbol_reads": [{ "path": "src/lib.rs", "query": "target_helper" }]
        }),
        Some("alpha"),
        None,
    )
    .await;
    assert!(!res.is_error, "batch_inspect failed: {}", res.text);
    assert!(res.text.contains("=== OVERVIEW (@alpha) ==="));
    assert!(res.text.contains("=== FILES ==="));
    assert!(res.text.contains("src/lib.rs"));
    assert!(res.text.contains("=== OUTLINES ==="));
    assert!(res.text.contains("=== SEARCHES ==="));
    assert!(res.text.contains("=== FILE SLICES ==="));
    assert!(res.text.contains("WebMCP slice capped at 160 lines"));
    assert!(res.text.contains("=== SYMBOLS ==="));
    assert!(res.text.contains("pub fn target_helper"));

    // 2. read_slices strictly enforces pinned workspace isolation
    let cross_pin = execute_tool_with_run_context(
        Arc::clone(&state),
        "WebMCP",
        "batch_inspect",
        json!({
            "read_slices": [{ "workspace": "beta", "path": "secret.txt" }]
        }),
        Some("alpha"),
        None,
    )
    .await;
    assert!(cross_pin.is_error);
    assert!(cross_pin.text.contains("di-pin ke workspace '@alpha'"));

    let _ = fs::remove_dir_all(&base);
}

#[tokio::test]
async fn test_multi_surgical_edit_preflight_blocks_disallowed_verify_command() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_multi_edit_preflight_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(temp_dir.join("src")).unwrap();
    let target = temp_dir.join("src").join("mod_a.rs");
    fs::write(&target, "pub const VALUE: i32 = 1;\n").unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));

    // 1. Disallowed verify_command MUST block before writing anything to disk
    let blocked = execute_tool(
        &state,
        "WebMCP",
        "multi_surgical_edit",
        json!({
            "files": [{
                "path": "src/mod_a.rs",
                "edits": [{ "search": "VALUE: i32 = 1;", "replace": "VALUE: i32 = 2;" }]
            }],
            "verify_command": "rm -rf /"
        }),
    )
    .await;
    assert!(blocked.is_error);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "pub const VALUE: i32 = 1;\n",
        "File MUST NOT be modified when verify_command fails pre-flight policy check"
    );

    // 2. HITL denial on verify_command in command_execution_policy='hitl' MUST also abort before disk write
    state.config.write().await.command_execution_policy = "hitl".to_string();
    let state_bg = Arc::clone(&state);
    let denier = tokio::spawn(async move {
        for _ in 0..40 {
            let req_id: Option<String> = state_bg
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
        "multi_surgical_edit",
        json!({
            "files": [{
                "path": "src/mod_a.rs",
                "edits": [{ "search": "VALUE: i32 = 1;", "replace": "VALUE: i32 = 2;" }]
            }],
            "verify_command": "echo should_not_run"
        }),
    )
    .await;
    assert!(denier.await.unwrap());
    assert!(denied.is_error);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "pub const VALUE: i32 = 1;\n",
        "File MUST remain untouched when verify_command HITL is denied"
    );

    // 3. Passing verify_command + cold LSP post_check_lsp succeeds cleanly
    state.config.write().await.command_execution_policy = "normal".to_string();
    let ok = execute_tool(
        &state,
        "WebMCP",
        "multi_surgical_edit",
        json!({
            "files": [{
                "path": "src/mod_a.rs",
                "edits": [{ "search": "VALUE: i32 = 1;", "replace": "VALUE: i32 = 2;" }]
            }],
            "post_check_lsp": true,
            "verify_command": "echo multi_edit_verified"
        }),
    )
    .await;
    assert!(!ok.is_error, "multi_surgical_edit failed: {}", ok.text);
    assert!(ok.text.contains("language server cold"), "got:\n{}", ok.text);
    assert!(ok.text.contains("multi_edit_verified"), "got:\n{}", ok.text);
    assert!(fs::read_to_string(&target).unwrap().contains("VALUE: i32 = 2;"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_run_command_pipeline_stops_on_first_failure() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_run_command_pipeline_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = AppState::new(temp_dir.clone());
    let fail_step = if cfg!(target_os = "windows") {
        "cmd /C exit 7"
    } else {
        "exit 7"
    };

    let out = execute_tool(
        &state,
        "WebMCP",
        "run_command",
        json!({
            "action": "run",
            "commands": [
                "echo pipeline_step_1_ok",
                fail_step,
                "echo pipeline_step_3_must_not_run"
            ]
        }),
    )
    .await;
    assert!(!out.is_error, "Pipeline execution returned hard Err: {}", out.text);
    assert!(out.text.contains("EXIT CODE: 7"), "got:\n{}", out.text);
    assert!(out.text.contains("[STEP 1/3]"), "got:\n{}", out.text);
    assert!(out.text.contains("pipeline_step_1_ok"), "got:\n{}", out.text);
    assert!(out.text.contains("[STEP 2/3]"), "got:\n{}", out.text);
    assert!(
        !out.text.contains("pipeline_step_3_must_not_run"),
        "Step 3 must NOT run after Step 2 failed:\n{}",
        out.text
    );

    // Unified lsp_query supports diagnostics cleanly (unknown op fails cleanly)
    let bad_lsp = execute_tool(
        &state,
        "WebMCP",
        "lsp_query",
        json!({ "op": "bogus_op" }),
    )
    .await;
    assert!(bad_lsp.is_error);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_case_insensitive_action_and_op_routing() {
    use super::schemas::requires_hitl_in_safe_edit;

    assert!(is_tool_read_only("run_command", &json!({ "action": "POLL" })));
    assert!(is_tool_read_only("run_command", &json!({ "action": " Poll " })));
    assert!(!is_tool_read_only("run_command", &json!({ "action": "RUN" })));
    assert!(is_tool_read_only("delegate_local_agent", &json!({ "action": "POLL" })));
    assert!(is_tool_read_only(
        "delegate_local_agent",
        &json!({ "action": "START", "mode": "READ_ONLY" })
    ));
    assert!(!is_tool_read_only(
        "delegate_local_agent",
        &json!({ "action": "START", "mode": "EXECUTE" })
    ));
    assert!(is_tool_read_only("session_checkpoint", &json!({ "op": "LOAD" })));
    assert!(is_tool_read_only("session_checkpoint", &json!({ "op": "" })));
    assert!(is_tool_read_only("session_checkpoint", &json!({})));
    assert!(is_tool_read_only("git_ops", &json!({ "op": "STATUS" })));
    assert!(!requires_hitl_in_safe_edit(
        "run_command",
        &json!({ "action": "POLL" }),
        true
    ));
    assert!(!requires_hitl_in_safe_edit(
        "run_command",
        &json!({ "action": "STOP" }),
        false
    ));

    let temp_dir = std::env::temp_dir().join("smart_mcp_case_action_routing_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let state = AppState::new(temp_dir.clone());
    *state.guardrail_mode.write().await = "safe_edit".to_string();
    state.config.write().await.command_execution_policy = "hitl".to_string();

    let (summary, _rx) = crate::jobs::start_job_with_handle(
        &state,
        &temp_dir,
        "echo case_routing_ok",
        None,
        30,
        None,
    )
    .await
    .unwrap();

    // Calling run_command(action="POLL") under safe_edit + hitl must route to poll without HITL
    let polled = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        execute_tool(
            &state,
            "WebMCP",
            "run_command",
            json!({ "action": "POLL", "job_id": summary.job_id, "from_line": 0 }),
        ),
    )
    .await
    .expect("run_command(action=POLL) must not trigger HITL wait");
    assert!(!polled.is_error, "Upper-case POLL failed: {}", polled.text);
    assert!(polled.text.contains("case_routing_ok"), "got:\n{}", polled.text);

    // Upper-case STOP on a slow job under safe_edit + hitl must route to stop without HITL
    let slow_cmd = if cfg!(target_os = "windows") {
        "ping -n 10 127.0.0.1"
    } else {
        "sleep 10"
    };
    let (slow_sum, _rx2) =
        crate::jobs::start_job_with_handle(&state, &temp_dir, slow_cmd, None, 30, None)
            .await
            .unwrap();
    let stopped = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        execute_tool(
            &state,
            "WebMCP",
            "run_command",
            json!({ "action": "STOP", "job_id": slow_sum.job_id }),
        ),
    )
    .await
    .expect("run_command(action=STOP) must not trigger HITL wait");
    assert!(!stopped.is_error, "Upper-case STOP failed: {}", stopped.text);
    assert!(stopped.text.contains(&slow_sum.job_id));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_webmcp_auto_verify_on_edit_checks_warm_lsp_or_skips_cold() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_auto_verify_edit_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("main.rs"), "fn main() { let a = 1; }\n").unwrap();

    let state = AppState::new(temp_dir.clone());
    state.config.write().await.webmcp_auto_verify_on_edit = true;

    let web_out = execute_tool(
        &state,
        "WebMCP",
        "surgical_edit",
        json!({
            "path": "main.rs",
            "edits": [{ "search": "let a = 1;", "replace": "let a = 2;" }]
        }),
    )
    .await;
    assert!(!web_out.is_error, "WebMCP edit failed: {}", web_out.text);
    assert!(
        web_out.text.contains("LSP post-check skipped"),
        "Expected auto warm-LSP check note on WebMCP edit, got:\n{}",
        web_out.text
    );

    let local_out = execute_tool(
        &state,
        "LocalAgent",
        "surgical_edit",
        json!({
            "path": "main.rs",
            "edits": [{ "search": "let a = 2;", "replace": "let a = 3;" }]
        }),
    )
    .await;
    assert!(!local_out.is_error);
    assert!(!local_out.text.contains("LSP post-check skipped"));

    // Also verify Output Guard preserves [HARNESS_HINT: control lines
    let mut big_with_hint = "line padding 0123456789abcdef\n".repeat(400);
    big_with_hint.push_str("[HARNESS_HINT: Keep this hint intact under budget compression.]\n");
    let compressed = compress_for_webmcp_budget("run_command", &big_with_hint, 6000);
    assert!(compressed.chars().count() <= 6000);
    assert!(
        compressed.contains("[HARNESS_HINT: Keep this hint intact"),
        "Output guard dropped [HARNESS_HINT:]:\n{compressed}"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_concurrent_slow_run_command_does_not_block_concurrent_write_or_edit_verify() {
    use std::time::Instant;

    let temp_dir = std::env::temp_dir().join("smart_mcp_concurrent_ws_lock_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    fs::write(temp_dir.join("file_a.txt"), "alpha=1\n").unwrap();
    fs::write(temp_dir.join("file_b.txt"), "beta=1\n").unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));

    let slow_cmd = if cfg!(target_os = "windows") {
        "ping -n 6 127.0.0.1 >NUL && echo slow_done"
    } else {
        "sleep 5 && echo slow_done"
    };

    // 1. Slow run_command on workspace (4s sync wait) MUST NOT block another session's file edit
    let st_cmd = Arc::clone(&state);
    let slow_handle = tokio::spawn(async move {
        execute_tool(
            &st_cmd,
            "WebMCP",
            "run_command",
            json!({
                "action": "run",
                "command": slow_cmd,
                "auto_promote_secs": 4
            }),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(250)).await;
    let edit_started = Instant::now();
    let edit_out = tokio::time::timeout(
        Duration::from_millis(1800),
        execute_tool(
            &state,
            "WebMCP",
            "surgical_edit",
            json!({
                "path": "file_b.txt",
                "edits": [{ "search": "beta=1", "replace": "beta=2" }]
            }),
        ),
    )
    .await
    .expect("surgical_edit must NOT be starved by concurrent slow run_command");
    assert!(!edit_out.is_error, "surgical_edit failed: {}", edit_out.text);
    assert!(
        edit_started.elapsed() < Duration::from_millis(1800),
        "surgical_edit waited too long: {:?}",
        edit_started.elapsed()
    );

    let cmd_out = slow_handle.await.unwrap();
    assert!(!cmd_out.is_error, "slow run_command failed: {}", cmd_out.text);
    assert!(cmd_out.text.contains("AUTO_PROMOTED_TO_BACKGROUND_JOB"));

    // 2. multi_surgical_edit with slow verify_command (4s) MUST release write lock before verify wait
    let st_verify = Arc::clone(&state);
    let verify_handle = tokio::spawn(async move {
        execute_tool(
            &st_verify,
            "WebMCP",
            "multi_surgical_edit",
            json!({
                "files": [{
                    "path": "file_a.txt",
                    "edits": [{ "search": "alpha=1", "replace": "alpha=2" }]
                }],
                "verify_command": slow_cmd,
                "auto_promote_secs": 4
            }),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(350)).await;
    let concurrent_write = tokio::time::timeout(
        Duration::from_millis(1800),
        execute_tool(
            &state,
            "WebMCP",
            "surgical_edit",
            json!({
                "path": "file_b.txt",
                "edits": [{ "search": "beta=2", "replace": "beta=3" }]
            }),
        ),
    )
    .await
    .expect("surgical_edit must NOT be starved during multi_surgical_edit verify phase");
    assert!(
        !concurrent_write.is_error,
        "concurrent surgical_edit failed: {}",
        concurrent_write.text
    );

    let verify_out = verify_handle.await.unwrap();
    assert!(!verify_out.is_error, "multi_surgical_edit verify failed: {}", verify_out.text);
    assert!(verify_out.text.contains("AUTO_PROMOTED_VERIFY_JOB"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn test_webmcp_output_guard_applies_task_mode_floor() {
    let temp_dir = std::env::temp_dir().join("smart_mcp_webmcp_task_output_floor_test");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let payload: String = (0..150)
        .map(|i| format!("line_{i:03}_0123456789abcdefghijklmnopqrstuvwxyz0123456789"))
        .collect::<Vec<_>>()
        .join("\n");
    let payload_len = payload.chars().count();
    assert!(payload_len > 7500 && payload_len < 10_000, "payload_len={payload_len}");
    fs::write(temp_dir.join("big.txt"), &payload).unwrap();

    let state = Arc::new(AppState::new(temp_dir.clone()));
    assert_eq!(state.config.read().await.effective_webmcp_max_output_chars(), 6000);

    let chat_out = execute_tool_with_webmcp_context(
        Arc::clone(&state),
        "WebMCP",
        "read_file_range",
        json!({ "path": "big.txt", "start_line": 1, "end_line": 200 }),
        None,
        None,
        None,
        crate::config::WebMcpExecutionMode::Chat,
    )
    .await;
    assert!(!chat_out.is_error);
    assert!(
        chat_out.text.contains("WebMCP Output Guard"),
        "Chat mode should compress >6000 chars"
    );

    let task_out = execute_tool_with_webmcp_context(
        Arc::clone(&state),
        "WebMCP",
        "read_file_range",
        json!({ "path": "big.txt", "start_line": 1, "end_line": 200 }),
        None,
        None,
        None,
        crate::config::WebMcpExecutionMode::Task,
    )
    .await;
    assert!(!task_out.is_error);
    assert!(
        !task_out.text.contains("WebMCP Output Guard"),
        "Task mode floor (10000 chars) should preserve 8.5k output intact, got len={}",
        task_out.text.chars().count()
    );

    let _ = fs::remove_dir_all(&temp_dir);
}
