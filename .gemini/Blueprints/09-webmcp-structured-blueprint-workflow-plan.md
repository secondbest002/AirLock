# Blueprint 09 — Formalized Multi-Turn Workflow & Stage-Aware Dual Handoff Prompts Execution Plan

> **For agentic workers:** Execute task-by-task following `d:\tauri-mcp-hub\.gemini\Blueprints\09-webmcp-structured-blueprint-workflow-blueprint.md`.

**Goal:** Formalize the 4-stage `WebMCP` coding workflow (`inspect` -> `blueprint` -> `handoff` -> `implement`) via a built-in Airlock skill (`airlock-blueprint-workflow`), stage-aware dual handoff prompt generation (`WEBMCP_NEXT_SESSION_PROMPT` & `ANTIGRAVITY_HANDOFF_PROMPT`), redacted `.airlock/handoff/prompts.md` mirroring, Turn-End Follow-Up Choices cues, and updated protocol/harness nudges — with zero new MCP tools.

**Spec:** `d:\tauri-mcp-hub\.gemini\Blueprints\09-webmcp-structured-blueprint-workflow-blueprint.md`

**Scope & Environment Constraints:**
- Zero new MCP tools (keep Compact schema < `12 KB`).
- Preserve 100% backward compatibility with existing `SessionCheckpoint` JSON files and Blueprint 08 `.airlock/HANDOFF.md` invariants.
- On Windows, run all Cargo commands inside `d:\tauri-mcp-hub\src-tauri` (`Set-Location d:\tauri-mcp-hub\src-tauri`) so `.cargo/config.toml` applies.

---

## Pitfalls & Invariants Checklist

- **`discover_skills` purity:** Keep `discover_skills(...)` in `src-tauri/src/skills.rs` strictly filesystem-only so existing precedence/jail tests remain untouched; inject built-in skills inside `AppState::refresh_skills()` (`src-tauri/src/state.rs:869-875`).
- **Existing exact-count assertions:** Because `AppState::refresh_skills()` now includes `airlock-blueprint-workflow`, update `src-tauri/src/mcp_resources.rs:779` (`arr.len() == 6` + `skill:airlock-blueprint-workflow`) and `src-tauri/src/mcp_server/tests.rs:1462-1465` (`len == 2`, assert `rust-check` & `airlock-blueprint-workflow`).
- **Harness & Initialize contracts:** Preserve `"ANTIGRAVITY_HANDOFF_PROMPT"` in both `initialize` instructions (`rpc.rs`) and `blueprint_nudge` (`webmcp_harness.rs`), and keep `build_mcp_initialize_instructions` <= `1200` chars (`rpc.rs:112`).
- **Follow-up choices discipline:** Never pack multi-blueprint creation or full-blueprint implementation into one turn; end every active turn with 1 short follow-up question + 2–4 concise, single-line actionable choices.

---

## Execution Checklist

- [x] **Task 1: Built-in Skill `airlock-blueprint-workflow` (`src-tauri/src/skills.rs`, `src-tauri/src/state.rs`, exact-count test updates)**
  - In `src-tauri/src/skills.rs`:
    - Add `pub const BUILTIN_WORKFLOW_SKILL_NAME: &str = "airlock-blueprint-workflow";`.
    - Implement `pub fn builtin_skills(disabled_norm: &HashSet<String>) -> Vec<SkillEntry>` and `pub fn inject_builtin_skills(entries: &mut Vec<SkillEntry>, disabled_skills: &HashSet<String>)`.
    - Populate virtual `SkillEntry` with `SkillScope::Global`, `source_label: "builtin:airlock"`, `dir_path: "<builtin:airlock>"`, `canon_dir: PathBuf::new()`, `resource_files: Vec::new()`, `enabled: !disabled_norm.contains(BUILTIN_WORKFLOW_SKILL_NAME)`, `disable_model_invocation: false`, only if no workspace/plugin/global skill already has the same name.
    - Embed complete structured `body_md` covering all 4 stages (`inspect`, `blueprint` max 1 `.md`/turn in `.gemini/blueprints/`, `handoff` saving checkpoint & printing BOTH `WEBMCP_NEXT_SESSION_PROMPT` + `ANTIGRAVITY_HANDOFF_PROMPT` verbatim, `implement` self-pacing pre-flight checkup + 1 phase/turn) plus the Turn-End Follow-Up Choices Protocol (1 short follow-up question + 2–4 concise 1-line choices).
    - Update `read_skill_resource_file` (`src-tauri/src/skills.rs:867`) to reject reads on `builtin:` skills / empty `canon_dir` cleanly with an informative error before any path join.
  - In `src-tauri/src/state.rs:869-875` (`AppState::refresh_skills`):
    - Make `entries` mutable (`let mut entries = crate::skills::discover_skills(...)`) and call `crate::skills::inject_builtin_skills(&mut entries, &disabled_set);`.
  - Update existing tests expecting exact non-builtin skill/prompt counts:
    - `src-tauri/src/mcp_resources.rs:779`: expect `6` prompts including `"skill:demo-skill"` and `"skill:airlock-blueprint-workflow"`.
    - `src-tauri/src/mcp_server/tests.rs:1462-1465`: expect `2` skills and verify both `"rust-check"` (`skill://rust-check`) and `"airlock-blueprint-workflow"` (`builtin:airlock`).

- [x] **Task 2: Stage-Aware Dual Handoff Prompts & Checkpoint Schema (`src-tauri/src/checkpoints.rs`, `src-tauri/src/tools/schemas.rs`)**
  - In `src-tauri/src/checkpoints.rs:16-45` (`SessionCheckpoint`):
    - Add `#[serde(default)] pub stage: Option<String>`, `#[serde(default)] pub blueprint_paths: Vec<String>`, `#[serde(default)] pub recommended_skills: Vec<String>`.
    - Implement `normalize_checkpoint_stage(raw: Option<&str>, bp_paths: &[String]) -> String` accepting `"inspect" | "blueprint" | "implement"` (auto-infer `"blueprint"` if any blueprint path is present, else `"implement"`).
    - Sync `blueprint_path` <-> `blueprint_paths` bidirectionally in `exec_session_checkpoint(op="save")` (dedup `/` paths; keep `blueprint_path` = `blueprint_paths.first().cloned()`).
  - Implement stage-aware dual prompt builders in `src-tauri/src/checkpoints.rs`:
    - `pub(crate) fn build_webmcp_next_session_prompt(cp: &SessionCheckpoint) -> String` containing workspace/checkpoint/handoff pointers, mandatory `list_skills` + `load_skill` (`airlock-blueprint-workflow` + `recommended_skills`), **PRE-FLIGHT CHECKUP** via `batch_inspect`, multi-turn guardrail (1 blueprint/turn or 1 phase/turn for 1 blueprint), Goal/Decisions/Next steps, and **STOP + Follow-Up Choices (1 question + 2–4 concise choices)**.
    - Upgrade `pub(crate) fn build_antigravity_handoff_prompt(cp: &SessionCheckpoint) -> String` retaining `.airlock/HANDOFF.md`, `airlock://workspace/checkpoint`, `blueprint_path`, Goal, Decisions, Next steps while adding Agent Skill check, **PRE-FLIGHT & FIELD AUDIT** (audit live codebase conditions vs blueprint & `.airlock/handoff/latest.diff`), and step-by-step full execution + verification.
  - Update `exec_session_checkpoint` `op="save"` (`checkpoints.rs:593-600`) to emit `stage=<stage>` in `CHECKPOINT_SAVED:` and output both fenced text blocks (`WEBMCP_NEXT_SESSION_PROMPT:` and `ANTIGRAVITY_HANDOFF_PROMPT:`).
  - Update `format_checkpoint_with_drift` (`checkpoints.rs:323-335`) to include `STAGE:`, `BLUEPRINT_PATHS:`, and `RECOMMENDED_SKILLS:` when non-empty.
  - Update `session_checkpoint` inputSchema in `src-tauri/src/tools/schemas.rs:760-778` with optional `stage`, `blueprint_paths`, and `recommended_skills` (keep descriptions concise so Compact schema stays < `12 KB`).

- [x] **Task 3: Mirror Redacted Dual Prompts to `.airlock/handoff/prompts.md` (`src-tauri/src/handoff.rs`)**
  - In `write_handoff_bundle` (`src-tauri/src/handoff.rs:1015+`):
    - When `checkpoint` is `Some(cp)`, render markdown containing both `build_webmcp_next_session_prompt(cp)` and `build_antigravity_handoff_prompt(cp)` (plus header with workspace alias, updated_at, and stage), redact with `redact_secrets(secrets, ...)`, and write atomically to `{HANDOFF_DIR_REL}/prompts.md`.
    - When `checkpoint` is `None`, remove `{HANDOFF_DIR_REL}/prompts.md` if it exists.
  - In `render_handoff_md` (`src-tauri/src/handoff.rs:885-896`):
    - When `checkpoint.is_some()`, append `- Next-session prompts (WebMCP / Antigravity): .airlock/handoff/prompts.md` under `## Supporting Files & MCP Resources` without breaching `MAX_HANDOFF_MD_CHARS = 6144`.

- [x] **Task 4: Protocol `initialize`, Harness Nudge & Output Guard (`src-tauri/src/mcp_server/rpc.rs`, `src-tauri/src/webmcp_harness.rs`, `src-tauri/src/tools/mod.rs`)**
  - In `src-tauri/src/mcp_server/rpc.rs:92-110`:
    - Update `handoff_clause` / Compact & Full `initialize` instructions to instruct loading `airlock-blueprint-workflow`, saving `session_checkpoint(op="save", stage=...)` with both `WEBMCP_NEXT_SESSION_PROMPT` & `ANTIGRAVITY_HANDOFF_PROMPT` copied verbatim, and ending active workflow turns with 1 short follow-up question + 2–4 choices — while staying under `1200` chars.
  - In `src-tauri/src/webmcp_harness.rs:435-444`:
    - Update `blueprint_nudge` so it reminds max 1 blueprint/turn, calling `session_checkpoint(op="save", stage="blueprint")`, and outputting BOTH `WEBMCP_NEXT_SESSION_PROMPT` & `ANTIGRAVITY_HANDOFF_PROMPT` + 2–4 follow-up choices, staying within `MAX_TOTAL_HINT_CHARS = 320`.
  - In `src-tauri/src/tools/mod.rs:160-179`:
    - Add `"WEBMCP_NEXT_SESSION_PROMPT:"` and `"CHECKPOINT_STAGE:"` to `is_webmcp_control_line`.

- [x] **Task 5: Add Comprehensive Automated Tests**
  - `src-tauri/src/skills.rs`: `test_builtin_workflow_skill_injection_override_and_toggle` (verify injection, override precedence, `disabled_skills` toggle, and `read_skill_resource_file` rejection on `builtin:airlock`).
  - `src-tauri/src/checkpoints.rs`: `test_session_checkpoint_stage_aware_dual_prompts` (verify `inspect`, `blueprint`, `implement`, auto-infer, `blueprint_paths` <-> `blueprint_path` sync, backward compatibility with legacy checkpoint JSON, and prompt contents for skills/pre-flight/multi-turn/choices/field-audit).
  - `src-tauri/src/handoff.rs`: `test_handoff_writes_redacted_prompts_md_and_clears_on_reset` (verify `prompts.md` created + secret redacted + linked in `HANDOFF.md` on `save`, and removed on `clear`).
  - `src-tauri/src/webmcp_harness.rs`: verify `blueprint_nudge` includes `session_checkpoint`, `WEBMCP_NEXT_SESSION_PROMPT`, `ANTIGRAVITY_HANDOFF_PROMPT`, and stays <= `320` chars.

- [x] **Task 6: Full Verification Gate & Docs Update**
  - Run `Set-Location d:\tauri-mcp-hub\src-tauri; cargo test` -> verify 0 warnings and all tests pass.
  - Run `Set-Location d:\tauri-mcp-hub\src-tauri; cargo run -- --benchmark-webmcp` -> verify all benchmark scenarios PASS and Compact schema bytes remain < `12 KB`.
  - Run `Set-Location d:\tauri-mcp-hub; npm run build` -> verify `tsc && vite build` succeeds cleanly.
  - Mark all checkboxes in `09-webmcp-structured-blueprint-workflow-plan.md` and update status in `09-webmcp-structured-blueprint-workflow-blueprint.md` to Implemented & Verified.
