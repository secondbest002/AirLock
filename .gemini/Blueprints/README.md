# Airlock (`tauri-mcp-hub`) — Master Engineering Blueprint & Antigravity Handoff Index

Dokumen ini adalah panduan induk (**Master Handoff Index**) untuk mengeksekusi peningkatan arsitektur Airlock secara bertahap, aman, dan terverifikasi oleh agent **Google Antigravity** (maupun developer manusia).

---

## 1. Daftar Dokumen Audit & Blueprint

| Fase / Urutan | Dokumen | Fokus Utama | Prioritas | Estimasi |
| :---: | :--- | :--- | :---: | :---: |
| **0 (Prasyarat)** | [`../Findings/codebase-audit-findings.md`](../Findings/codebase-audit-findings.md) | Audit 11 temuan keamanan, bug, & performa eksisting (**F-01 s.d. F-11 seluruhnya selesai**, termasuk F-06 hardening & F-10 modularitas `providers/` + `mcp_server/`). | **P0** | Selesai |
| **0B (Reliabilitas)** | [`../Findings/realworld-edge-cases-reliability-study.md`](../Findings/realworld-edge-cases-reliability-study.md) | **Studi Pemetaan Real-World & Edge Cases:** Observabilitas UI live (Tahap A), long-poll `poll_command_job` + non-interactive Windows + stall/lock detector + grace HITL (Tahap B), serta checklist uji standar (Tahap C). | **P0** | Selesai |
| **1** | [`01-multi-workspace-blueprint.md`](./01-multi-workspace-blueprint.md) | **Multi-Workspace Registry** (`workspace.rs`), alias unik, guardrail & HITL per-workspace, HTTP/header workspace pinning, scoped audit & safe revert. | **P1** | 1,5–2 hari |
| **2** | [`02-gemini-optimization-anti-timeout-blueprint.md`](./02-gemini-optimization-anti-timeout-blueprint.md) | **Gemini Anti-Timeout & Akselerasi:** Auto-promote slow command WebMCP ke `JobManager`, compound tools (`batch_inspect`, `multi_surgical_edit`), hybrid parallel read-only calls, 3-layer watchdog, `TurnCheckpoint` + Resume Turn, model-aware `thinkingConfig`, shared `reqwest::Client`. | **P1** | 1,5–2 hari |
| **3** | [`04-plugins-and-skills-ecosystem-blueprint.md`](./04-plugins-and-skills-ecosystem-blueprint.md) | **Plugins & Skills Ecosystem:** Progressive Disclosure `SKILL.md` (`skills.rs`), Agent Plugins 1.0.0 (`plugins.rs`) dengan 2-step Security Review Gate + DPAPI secret interpolation, dan Skills over MCP (`SEP-2640`). | **P2** | 2 hari |
| **4** | [`03-antigravity-artifacts-subagents-hooks-blueprint.md`](./03-antigravity-artifacts-subagents-hooks-blueprint.md) | **Antigravity-Grade UX:** Artifacts Workflow (`Plan → Comment → Approve → Execute → Walkthrough`), Parallel `AgentRunManager` (hingga 4 run) + read-only `spawn_subagent`, Slash Workflows, dan SHA-256 Trusted Lifecycle Hooks. | **P2** | 2,5–3 hari |
| **5** | [`05-mcp-2026-acp-semantic-intelligence-blueprint.md`](./05-mcp-2026-acp-semantic-intelligence-blueprint.md) | **MCP 2026, ACP & Semantic Code Intelligence:** `tasks/*`, 2-way `elicitation/create`, mode CLI `--acp` (Zed/JetBrains), Tree-sitter AST `code_outline` + `read_symbol`, dan On-Demand Lazy `LspBridge`. | **P3** | 2,5–3,5 hari |
| **6 (Fondasi WebMCP)** | [`06-webmcp-foundation-antigravity-parity-blueprint.md`](./06-webmcp-foundation-antigravity-parity-blueprint.md) | **WebMCP Foundation Overhaul & Antigravity Parity (5 Pilar):** Compact Tool Surface (28 → 9–10 Super-Tools), 1-Turn Macro Actions (`batch_inspect` + `multi_surgical_edit` inline LSP/verify + fail-fast pipeline), Transport Timing (`18s`/`22s`/`32s`) + Parallel Read-Only Batch + Output Token Guard (`≤6 KB`), Web Brain → Local Executor (`delegate_local_agent`), serta `initialize.instructions` + server-side `session_checkpoint`. | **P0** | Selesai *(uncommitted)* |
| **7 (WebMCP Harness)** | [`07-webmcp-harness-and-cli-delegation-blueprint.md`](./07-webmcp-harness-and-cli-delegation-blueprint.md) | **WebMCP Session Harness & E2E Benchmark:** Stateful Server-Side Harness (`webmcp_harness.rs`), Auto Warm-LSP Verify pada edit WebMCP, serta E2E Replay & Regression Benchmark (`webmcp_benchmark.rs` / `--benchmark-webmcp`). | **P0/P1** | Selesai *(uncommitted)* |
| **10 (UI/UX Overhaul)** | [`10-airlock-ui-ux-overhaul-blueprint.md`](./10-airlock-ui-ux-overhaul-blueprint.md) | **Obsidian Airlock v2 (Hybrid Precision Deck):** Custom Native Frameless Titlebar (`decorations: false`, `shadow: true`), 4 Signature Accents, Dual-Layer 1px Border Illumination, Focus-First Agent Studio, Interactive Signal Conduit & Photon Pulse, Live Traffic Sparkline, Diff Inspector Pro (`Unified \| Split`), dan modularisasi CSS ke `src/styles/01..05-*.css`. | **P1** | Selesai *(uncommitted)* |

---

## 2. Grafik Dependensi & Urutan Eksekusi yang Disarankan

```text
  [0] Audit Fase 1 (Security Quick Wins + Inisialisasi Git)
         |
         +---------------------------+
         v                           v
  [1] Blueprint 01              [2] Blueprint 02
  Multi-Workspace               Gemini Optimization & Anti-Timeout
  (workspace.rs,                (watch cancel, shared http_client,
   scoped AuditEntry,            auto-promote jobs, parallel read-only,
   per-ws guardrail)             TurnCheckpoint, thinkingConfig)
         |                           |
         +-------------+-------------+
                       |
          +------------+------------+
          v                         v
  [3] Blueprint 04            [4] Blueprint 03
  Plugins 1.0.0 & SKILL.md    Antigravity UX (Artifacts, Parallel Runs,
  (skills.rs, plugins.rs,     Subagents & Trusted Hooks)
   SEP-2640, DPAPI secrets)         |
          |                         |
          +------------+------------+
                       v
                [5] Blueprint 05
                MCP 2026 (Tasks/Elicitation),
                ACP (--acp), Tree-sitter & Lazy LSP
```

### Mengapa Urutan Ini Optimal?
1. **Audit Fase 1 + `git init` duluan** mengamankan baseline (menutup bypass `auth.json`, bug `mcp-session-id`, dan memberi safety net Git commit per blueprint).
2. **Blueprint 01 & 02 dapat dikerjakan berurutan (01 → 02) atau paralel dengan koordinasi minimal:**
   * Blueprint 01 menetapkan signature `execute_tool_with_context` dan field `workspace_*` pada `AuditEntry` & `JobSummary`.
   * Blueprint 02 menetapkan `watch` cancellation channel, `AppState.http_client`, dan `start_job_with_handle` — semua dibutuhkan langsung oleh Blueprint 03 dan 05.
3. **Blueprint 04 disarankan sebelum atau paralel dengan Blueprint 03:** keduanya menambahkan built-in tools & fitur UI di tab terpisah (Blueprint 04 di View 3 `#panel-hub`, Blueprint 03 di View 1 `#panel-agent`).
4. **Blueprint 05 paling akhir** karena mode `--acp` dan `MCP Tasks` langsung membungkus seluruh mesin yang sudah matang dari Blueprint 01–04 (`WorkspaceResolver`, `JobManager`, `AgentRunManager`, `ArtifactStore`, `SkillRegistry`).

---

## 3. Global Engineering Invariants (Wajib Dibaca Setiap Sesi Antigravity)

Seluruh perubahan di 5 blueprint **wajib** mematuhi kontrak inti codebase `D:\tauri-mcp-hub`:

1. **Stack & Zero-Framework UI:**
   * Backend: **Rust 2021 + Tauri v2 + Axum 0.7 + Tokio 1**.
   * Frontend: **Vanilla TypeScript (`strict: true`, `noUnusedLocals`, `noUnusedParameters`) + Vite 6 + Obsidian Studio CSS**. Jangan memasang React/Vue/Tailwind.
2. **Keamanan XSS & CSP:**
   * Dilarang memasukkan string dinamis ke `innerHTML`. Gunakan DOM API (`createElement`, `textContent`, `setAttribute`).
   * Pastikan semua fitur UI baru berjalan bersih dengan CSP ketat (`default-src 'self'`, lihat Audit F-04).
3. **Kompatibilitas Persistensi `~/.smart-tauri-hub/`:**
   * Semua field baru di `AppConfig`, `ProviderSecrets`, `AuditEntry`, dan `PersistedAuth` **wajib** memiliki `#[serde(default)]` agar file konfigurasi user lama tidak pernah gagal dimuat.
   * **Rahasia / Secret** (API key, tunnel token, plugin `${SECRET:...}`) hanya boleh disimpan di `SecretsStore` (Windows DPAPI `secrets.dpapi`), **tidak pernah** plaintext di `config.json`.
4. **Keamanan Windows Path & Process Tree:**
   * Selalu strip prefix `r"\\?\"` untuk path yang ditampilkan/disimpan; selalu panggil `security::resolve_jailed_path` sebelum baca/tulis disk.
   * Setiap child process eksternal (shell, background job, downstream stdio MCP, LSP server) wajib memakai `creation_flags(0x08000000)` (`CREATE_NO_WINDOW`) di Windows dan dibersihkan via `kill_pid_tree` (`taskkill /PID <pid> /T /F`) saat timeout/drop/quit.
5. **Backward Compatibility MCP & IPC:**
   * Jangan merusak signature endpoint `/mcp`, `/mcp/:token`, `--stdio`, atau Tauri command eksisting (`set_workspace_root`, `run_local_agent_turn`, dll.). Tambah parameter sebagai `Option<T>` dengan default yang kompatibel.
6. **Sinkronisasi Jumlah Built-in Tools:**
   * Hapus angka hardcode `14` di `src/main.ts:63` dan `index.html:686` — selalu hitung dinamis dari `mcp_tools_list().len()` (`DashboardSnapshot.builtin_tools_count`).
7. **Toolchain Build & Verifikasi Wajib (Windows GNU):**
   Setiap selesai menyelesaikan satu fase/blueprint, jalankan verifikasi penuh:
   ```powershell
   cd D:\tauri-mcp-hub\src-tauri
   cargo test
   cd D:\tauri-mcp-hub
   npm run build
   ```

---

## 4. Peta Modul Baru & Perubahan File Lintas Blueprint

| Modul / File | Blueprint Terkait | Peran |
| :--- | :---: | :--- |
| `src-tauri/src/workspace.rs` *(baru)* | 01 | Multi-Workspace Registry, alias sanitizer, resolver, per-workspace policy |
| `src-tauri/src/skills.rs` *(baru)* | 04 | Parser `SKILL.md`, 3-tier discovery, progressive disclosure, jailed resource reader |
| `src-tauri/src/plugins.rs` *(baru)* | 04 | Agent Plugins 1.0.0 inspector, 2-step trust install, `${SECRET:...}` runtime interpolator |
| `src-tauri/src/artifacts.rs` *(baru)* | 03 | Artifact store (`Plan`, `TaskList`, `Walkthrough`), inline comments, approval state |
| `src-tauri/src/agent_manager.rs` *(baru)* | 03 | Parallel run manager (maks 4 run), per-session history map, workspace write locks |
| `src-tauri/src/hooks.rs` *(baru)* | 03 | Lifecycle hooks (`post_edit`/`pre_commit`/`post_turn`) + SHA-256 trust verification |
| `src-tauri/src/mcp_tasks.rs` *(baru)* | 05 | State machine resmi MCP 2026 `tasks/*` membungkus `JobManager` & async tools |
| `src-tauri/src/acp_server.rs` *(baru)* | 05 | CLI `--acp` server untuk integrasi native Zed / JetBrains IDE |
| `src-tauri/src/semantic.rs` *(baru)* | 05 | Tree-sitter AST `code_outline` & `read_symbol` + regex fallback |
| `src-tauri/src/lsp_bridge.rs` *(baru)* | 05 | Lazy-spawn LSP client (`rust-analyzer`, `tsserver`, `pyright`) + 5m idle reaper |
| `src-tauri/src/checkpoints.rs` *(baru)* | 06 | Persistent server-side `session_checkpoint` di `~/.smart-tauri-hub/checkpoints/` + auto git/audit enrichment & live drift check |
| `src-tauri/src/webmcp_harness.rs` *(baru)* | 07 | Stateful server-side WebMCP session harness (anti read/error loop, context-rot nudge, checkpoint anchor) |
| `src-tauri/src/webmcp_benchmark.rs` *(baru)* | 07 | Deterministic WebMCP E2E replay & KPI regression benchmark (`--benchmark-webmcp`) |
| `src/views/plugins.ts` *(baru)* | 04 | UI Installed Plugins (2-step security review) & Skills Catalog |
| `src/views/artifacts.ts` *(baru)* | 03 | Split-pane Artifacts Review panel + inline comments + Approve/Request Changes |

---

## 5. Template Prompt Handoff untuk Antigravity

Saat menyerahkan satu blueprint ke Antigravity, cukup gunakan prompt singkat berikut (ganti nomor blueprint sesuai tahapan):

> Baca `.gemini/Blueprints/README.md` dan `.gemini/Blueprints/01-multi-workspace-blueprint.md` secara menyeluruh. Implementasikan seluruh fase dalam blueprint tersebut secara bertahap dengan mematuhi seluruh Global Engineering Invariants, Pitfalls & Gotchas, serta tuliskan seluruh automated tests yang disyaratkan. Verifikasi akhir wajib lulus `cargo test` di `src-tauri` dan `npm run build` di root tanpa error maupun warning.
