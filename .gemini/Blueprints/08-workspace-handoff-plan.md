# Blueprint 08 — Workspace `.airlock/` Handoff Auto-Sync Execution Plan

> **For agentic workers:** Execute task-by-task following `d:\tauri-mcp-hub\.gemini\Blueprints\08-workspace-handoff-blueprint.md`.

**Goal:** Implement event-driven, strict-`WebMCP`-only auto-sync of `<workspace>/.airlock/HANDOFF.md` and `<workspace>/.airlock/handoff/` (`checkpoint.json`, `progress.md`, `latest.diff`, `session-audit.jsonl`, `artifacts/*.md`), auto-excluded locally via `.git/info/exclude`, with scanner/drift exclusions, secret redaction, MCP resource `airlock://workspace/handoff`, and WebMCP UI toggle.

**Spec:** `d:\tauri-mcp-hub\.gemini\Blueprints\08-workspace-handoff-blueprint.md`

---

## Execution Checklist

- [x] **Task 1: Config, ArtifactStore Merge Query & State Scaffolding**
  - Add `webmcp_auto_handoff_dir: bool` (`#[serde(default = "default_true")]`) in `src-tauri/src/config.rs` + unit test assertion.
  - Add `ArtifactStore::list_workspace_latest_by_kind` in `src-tauri/src/artifacts.rs` + merge unit test.
  - Add `webmcp_auto_handoff_dir` to `DashboardSnapshot` and `handoff_manager: WorkspaceHandoffManager` to `AppState` in `src-tauri/src/state.rs`.
- [x] **Task 2: Implement Core `src-tauri/src/handoff.rs`**
  - Implement `WorkspaceHandoffManager`, `HandoffCoordinator`, `HandoffTrigger`, `is_webmcp_source`, `is_handoff_ignored_rel`, `workspace_has_handoff`.
  - Implement secret redaction, `.git/info/exclude` (standard `.git` dir + worktree `gitdir:` file), jailed atomic writes, actionable WebMCP audit filter, bounded `latest.diff` (staged + unstaged + untracked new files), `HANDOFF.md` (`<= 6144` chars), `progress.md`, `checkpoint.json`, `session-audit.jsonl`, `artifacts/*.md`, `sync_after_job_finished`, `render_handoff_preview_readonly`.
  - Register `mod handoff;` in `src-tauri/src/main.rs`.
- [x] **Task 3: Checkpoints & Scanner Exclusions**
  - Update `src-tauri/src/checkpoints.rs`: `load_checkpoint_struct`, filter `.airlock/HANDOFF.md` & `.airlock/handoff/` in `collect_git_state`, reference `.airlock/HANDOFF.md` in `build_antigravity_handoff_prompt`, sync on `save`/`clear` when `is_webmcp_source(source)`.
  - Update `src-tauri/src/tools/fs_tools.rs` & `src-tauri/src/tools_extra.rs`: ignore `.airlock/HANDOFF.md` and `.airlock/handoff/` in `scan_project_overview_blocking`, `overview_text_for_resource`, `run_smart_search_blocking`, `run_find_files_blocking` (unless search root targets `.airlock/handoff`).
- [x] **Task 4: Tool Pipeline, Background Jobs, Delegated Runs & Revert Hooks**
  - Update `src-tauri/src/tools/mod.rs`: `effective_source` (`"WebMCP:delegate"` for `delegated_*` runs) + `should_sync_webmcp_tool` auto-sync after mutative/command/artifact calls.
  - Update `src-tauri/src/jobs.rs`, `tools/shell.rs`, `tools/fs_tools.rs`, `tools_extra.rs`: track `source: Option<String>` on `ManagedJob` / `JobSummary`, call `sync_after_job_finished` before `done_tx.send(true)`.
  - Update `src-tauri/src/local_agent.rs`: `record_delegated_result` + sync on delegated completion.
  - Update `src-tauri/src/tools/revert.rs`: re-sync when reverting WebMCP entries in workspaces with existing `.airlock/HANDOFF.md`.
- [x] **Task 5: MCP Resource (`airlock://workspace/handoff`) & Tauri Commands**
  - Add `airlock://workspace/handoff` and `workspace://{alias}/handoff` in `src-tauri/src/mcp_resources.rs`.
  - Add `webmcp_auto_handoff_dir` to `update_webmcp_settings`, add `sync_workspace_handoff_now`, and add re-sync on `approve_artifact` / `request_artifact_changes` in `src-tauri/src/main.rs`.
- [x] **Task 6: Frontend WebMCP Toggle (`index.html`, `src/types.ts`, `src/views/bridge.ts`)**
  - Add `#toggle-webmcp-auto-handoff` in `index.html`, `webmcp_auto_handoff_dir` + `JobSummary.source` in `src/types.ts`, and wire rendering + handlers in `src/views/bridge.ts`.
- [x] **Task 7: Comprehensive Automated Tests & Full Verification Gate**
  - Add unit/integration test suite in `src-tauri/src/handoff.rs` (9 tests covering exclude/worktree, strict WebMCP-only isolation, staged+unstaged+untracked diff, guardrail/toggle, redaction, background job/checkpoint/revert sync, artifacts merge + read-only resource preview, scanner exclusions).
  - Run `cargo test` (117 passed, 0 failed, 0 warnings), `cargo run -- --benchmark-webmcp` (PASS 4/4), and `npm run build` (`tsc && vite build`, OK).
