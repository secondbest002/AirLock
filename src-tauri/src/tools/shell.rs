use crate::jobs;
use crate::process_util::{build_shell_command, ChildTreeGuard};
use crate::security::{compress_output, validate_command_with_policy};
use crate::state::{AppState, AuditEntry};
use crate::workspace::ResolvedWorkspace;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub(crate) async fn run_shell_raw(
    root: &Path,
    command: &str,
    timeout_secs: u64,
) -> Result<(i32, String, String), String> {
    let mut cmd = build_shell_command(root, command);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Gagal menjalankan perintah '{command}': {e}"))?;

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_end(&mut buf).await;
        }
        buf
    });

    let mut guard = ChildTreeGuard::new(child);
    let wait_res = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        guard.child_mut().expect("child present").wait(),
    )
    .await;

    match wait_res {
        Ok(Ok(status)) => {
            guard.disarm();
            let out_bytes = tokio::time::timeout(Duration::from_secs(2), stdout_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let err_bytes = tokio::time::timeout(Duration::from_secs(2), stderr_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();

            let exit_code = status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out_bytes).to_string();
            let stderr = String::from_utf8_lossy(&err_bytes).to_string();
            Ok((exit_code, stdout, stderr))
        }
        Ok(Err(e)) => {
            guard.kill_tree_now();
            Err(format!("Gagal menunggu output perintah: {e}"))
        }
        Err(_) => {
            guard.kill_tree_now();
            Err(format!("Perintah timeout setelah {timeout_secs} detik."))
        }
    }
}

pub(crate) async fn exec_run_command(
    state: &AppState,
    source: &str,
    ws: &ResolvedWorkspace,
    args: &Value,
) -> Result<String, String> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Argumen 'command' wajib diisi.")?;
    let (cmd_policy, cmd_allowlist) = state.command_policy_and_allowlist().await;
    validate_command_with_policy(command, &cmd_policy, &cmd_allowlist)?;

    let max_lines = args
        .get("max_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(60)
        .clamp(12, 200) as usize;

    let explicit_promote = args
        .get("auto_promote_secs")
        .and_then(|v| v.as_u64())
        .map(|s| s.clamp(3, 20));
    let promote_secs = explicit_promote.or_else(|| if source == "WebMCP" { Some(9) } else { None });

    if let Some(p_secs) = promote_secs {
        let bg_timeout = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(300)
            .clamp(15, 1800);
        let cmd_label: String = command.chars().take(48).collect();
        let (summary, mut done_rx) = jobs::start_job_with_handle(
            state,
            &ws.canon_root,
            command,
            Some(format!("Auto: {cmd_label}")),
            bg_timeout,
            Some(ws.alias.clone()),
        )
        .await?;

        let finished_in_time =
            tokio::time::timeout(Duration::from_secs(p_secs), done_rx.wait_for(|&d| d))
                .await
                .is_ok();

        if finished_in_time {
            let (code, compressed, saved_approx) =
                if let Some((_, exit_opt, _, out, _)) =
                    jobs::get_job_snapshot_output(state, &summary.job_id, max_lines).await
                {
                    let c = exit_opt.unwrap_or(0);
                    let saved = out.len() / 2;
                    (c, out, saved)
                } else {
                    (0, String::new(), 0)
                };

            state
                .record_audit(
                    AuditEntry {
                        id: AppState::next_audit_id(),
                        timestamp: AppState::now_timestamp(),
                        source: source.to_string(),
                        tool_name: "run_command_compressed".to_string(),
                        target_path: None,
                        summary: format!("$ {command} (exit {code})"),
                        diff_text: Some(compressed.clone()),
                        can_revert: false,
                        reverted: false,
                        tokens_saved: saved_approx,
                        original_content: None,
                        original_blob_hash: None,
                        file_existed_before: false,
                        post_edit_hash: None,
                        group_id: None,
                        move_from_path: None,
                        workspace_id: Some(ws.id.clone()),
                        workspace_alias: Some(ws.alias.clone()),
                        workspace_root: Some(ws.clean_path.clone()),
                    },
                    0,
                )
                .await;

            return Ok(format!("EXIT CODE: {code}\n{compressed}"));
        }

        // Still running after promote_secs -> hand off safely to Background Job Manager
        let (elapsed, recent, total_lines) =
            if let Some((_, _, el, out, tot)) =
                jobs::get_job_snapshot_output(state, &summary.job_id, 20).await
            {
                (el, out, tot)
            } else {
                (p_secs, String::new(), 0)
            };

        state
            .record_audit(
                AuditEntry {
                    id: AppState::next_audit_id(),
                    timestamp: AppState::now_timestamp(),
                    source: source.to_string(),
                    tool_name: "run_command_compressed".to_string(),
                    target_path: None,
                    summary: format!("$ {command} (auto-promoted -> {})", summary.job_id),
                    diff_text: Some(recent.clone()),
                    can_revert: false,
                    reverted: false,
                    tokens_saved: 0,
                    original_content: None,
                    original_blob_hash: None,
                    file_existed_before: false,
                    post_edit_hash: None,
                    group_id: None,
                    move_from_path: None,
                    workspace_id: Some(ws.id.clone()),
                    workspace_alias: Some(ws.alias.clone()),
                    workspace_root: Some(ws.clean_path.clone()),
                },
                0,
            )
            .await;

        return Ok(format!(
            "AUTO_PROMOTED_TO_BACKGROUND_JOB\nJOB_ID: {}\nWORKSPACE: @{}\nSTATUS: running\nELAPSED: {}s\nNEXT_FROM_LINE: {}\nHINT: Command exceeded {}s sync window and is continuing in background. Call poll_command_job(job_id=\"{}\", from_line={}) (it automatically long-polls up to 12s; do NOT run sleep), or stop_command_job(job_id=\"{}\") to cancel.\nRECENT OUTPUT:\n{}",
            summary.job_id,
            ws.alias,
            elapsed,
            total_lines,
            p_secs,
            summary.job_id,
            total_lines,
            summary.job_id,
            if recent.is_empty() {
                "(waiting for output...)"
            } else {
                recent.as_str()
            }
        ));
    }

    let max_timeout = if source == "WebMCP" { 22 } else { 60 };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(18)
        .clamp(1, max_timeout);

    let (exit_code, stdout, stderr) = run_shell_raw(&ws.canon_root, command, timeout_secs).await?;
    let (compressed, saved_bytes) = compress_output(&stdout, &stderr, max_lines);
    let tokens_saved = saved_bytes / 4;

    state
        .record_audit(
            AuditEntry {
                id: AppState::next_audit_id(),
                timestamp: AppState::now_timestamp(),
                source: source.to_string(),
                tool_name: "run_command_compressed".to_string(),
                target_path: None,
                summary: format!("$ {command} (exit {exit_code})"),
                diff_text: Some(compressed.clone()),
                can_revert: false,
                reverted: false,
                tokens_saved,
                original_content: None,
                original_blob_hash: None,
                file_existed_before: false,
                post_edit_hash: None,
                group_id: None,
                move_from_path: None,
                workspace_id: Some(ws.id.clone()),
                workspace_alias: Some(ws.alias.clone()),
                workspace_root: Some(ws.clean_path.clone()),
            },
            0,
        )
        .await;

    Ok(format!("EXIT CODE: {exit_code}\n{compressed}"))
}
