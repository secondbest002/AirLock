# Blueprint 03 — Antigravity-Grade UX: Artifacts Workflow, Parallel Subagents & Lifecycle Hooks

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent (Autonomous / Co-pilot Handoff) |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Ready for Implementation |
| **Prioritas** | P2 — Transformasi Local Agent menjadi Open, Model-Agnostic Agent Command Center |
| **Dependensi Blueprint** | Sangat disarankan **setelah Blueprint 01** (Multi-Workspace) dan **Blueprint 02** (Watch cancellation channel, shared `http_client`, parallel tool calls) |

---

## 1. Handoff Brief untuk Agent Antigravity

Dokumen ini adalah blueprint arsitektur dan implementasi lengkap untuk menghadirkan tiga pilar pengalaman inti **Google Antigravity** ke dalam Airlock **tanpa** membangun ulang full code editor:
1. **Artifacts Workflow (`Plan → Review/Comment → Approve → Execute → Walkthrough`):** Mengubah interaksi agent dari sekadar chat linear menjadi kolaborasi berbasis dokumen terstruktur (`implementation_plan.md`, `task.md`, `walkthrough.md`, serta screenshot/evidence) lengkap dengan komentar review inline.
2. **Parallel Agent Manager & Subagents (`AgentRunManager`):** Mengangkat batasan tunggal `AppState.agent_running: AtomicBool` agar Airlock dapat menjalankan beberapa agent run secara paralel sekaligus menyediakan built-in tool `spawn_subagent` untuk riset/review paralel tanpa membengkakkan konteks utama.
3. **Slash Workflows & Deterministic Lifecycle Hooks:** Memperluas `auto_inject_project_rules` (`providers.rs:57`) dan MCP `prompts` (`mcp_resources.rs:147`) menjadi perintah `/plan`, `/review`, `/debug`, `/test`, plus hook otomatis (`post_edit`, `pre_commit`, `post_turn`) dengan gerbang kepercayaan keamanan eksplisit.

### Invarian Arsitektur Airlock yang WAJIB Dipatuhi
1. **Jangan Bangun IDE/Code Editor:** Biarkan VS Code/Zed/JetBrains menangani editing & autocomplete. Airlock berfokus menjadi **Agent Command Center + Review Surface**.
2. **Zero UI Framework & XSS-Safe DOM:** Frontend tetap Vanilla TypeScript (strict) + Obsidian Studio CSS. Seluruh render Markdown artifact memakai `renderMarkdownInto` (`src/markdown.ts`, murni `createElement` + `textContent`).
3. **Workspace Hygiene:** Secara default, artifact sesi disimpan di `~/.smart-tauri-hub/artifacts/<ws_hash>/<session_id>/` agar tidak mengotori `git status` repositori pengguna, dengan tombol opsional **"Export to `.airlock/artifacts/`"**.
4. **Anti-RCE pada Workspace Hooks:** File `.airlock/hooks.json` yang berasal dari repositori (misalnya hasil `git clone`) **TIDAK BOLEH** langsung mengeksekusi perintah shell sebelum pengguna menyetujui fingerprint SHA-256 file tersebut secara eksplisit di UI.
5. **Isolasi Tulis pada Parallel Runs:** Dua agent run paralel **dilarang** melakukan mutasi file bersamaan pada workspace yang sama tanpa lock; subagent bawaan dikunci `read_only` kecuali diberi target workspace berbeda atau Git worktree terisolasi.

---

## 2. Analisis Kondisi Saat Ini (As-Is)

| Area | Lokasi Eksisting | Keterbatasan Saat Ini |
| :--- | :--- | :--- |
| **Single Agent Lock** | `src-tauri/src/state.rs:264-272`, `local_agent.rs:287` | Hanya ada 1 `agent_running: AtomicBool`, 1 `agent_cancel`, 1 `active_turn_group_id`, dan 1 history per provider (`gemini_config.history`, `openai_history`, `anthropic_history`). Mustahil menjalankan 2 tugas paralel. |
| **Direct Execution Only** | `src-tauri/src/local_agent.rs:334-346` | Agent langsung mengedit kode pada prompt pertama tanpa mode `Plan-First` / review gate. |
| **Tidak Ada Artifacts Surface** | `index.html:285-305`, `src/views/agent.ts` | `#agent-shell` hanya berisi transcript chat + drawer konfigurasi. Tidak ada panel khusus Plan, Task Checklist, Walkthrough, atau komentar review inline. |
| **Static Prompt Chips** | `index.html:308-337` | Hanya 4 tombol `.prompt-chip` hardcoded; tidak terhubung ke MCP `prompts/list` (`mcp_resources.rs:147`) dan tidak mendukung slash command `/...` atau custom workflow proyek. |
| **Tidak Ada Lifecycle Hooks** | `src-tauri/src/tools.rs:1259-1305` | Setelah `surgical_edit` / `write_file_jailed`, agent harus manual memanggil `run_command_compressed` di turn berikutnya untuk mengetahui error kompilasi (`tsc` / `cargo check`). |

---

## 3. Arsitektur Target (To-Be)

### 3.1 Diagram Arsitektur 3 Pilar

```text
  +-------------------------------------------------------------------------+
  |                     Obsidian Studio — Local Agent UI                    |
  |  [Run Switcher: Main #1 | Subagent: Auth Audit | Run #2 (@web)]         |
  +-----------------------------------+-------------------------------------+
  |        Chat & Slash Composer      |       Artifacts Review Panel        |
  |  - /plan /review /debug /test     |  Tabs: [Plan] [Tasks] [Walkthrough] |
  |  - Streaming Thoughts + Steps     |  - Inline Review Comments (anchored)|
  |  - Execution Mode: Direct / Plan  |  - [Approve Plan] [Request Changes] |
  +-----------------------------------+-------------------------------------+
                     |                                     |
                     v                                     v
  +-------------------------------------------------------------------------+
  |                AgentRunManager (src-tauri/src/agent_manager.rs)         |
  |  - Map<RunId, AgentRunHandle> (maks 4 run aktif bersamaan)              |
  |  - Per-run: history, cancel_tx, turn_group_id, workspace_id, mode       |
  |  - Workspace Write Lock: Mutex per workspace_id (cegah race edit)       |
  |  - spawn_subagent() -> read_only child run dengan token budget terpisah |
  +--------------------+----------------------------------+-----------------+
                       |                                  |
                       v                                  v
  +--------------------------------------+  +-------------------------------+
  | ArtifactStore (artifacts.rs)         |  | HookEngine (hooks.rs)         |
  | - ~/.smart-tauri-hub/artifacts/...   |  | - pre_tool / post_edit /      |
  | - Plan / TaskList / Walkthrough      |  |   pre_commit / post_turn      |
  | - Plan Approval Gate (blocks edits)  |  | - SHA-256 Trust Verification  |
  | - Inline Comment Injection           |  | - Auto-append diagnostics     |
  +--------------------------------------+  +-------------------------------+
```

---

## 4. Spesifikasi Detail Setiap Pilar

---

### Pilar 1 — Artifacts Workflow & Plan Approval Gate (`src-tauri/src/artifacts.rs`)

#### 4.1.1 Model Data Rust
```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    ImplementationPlan,
    TaskList,
    Walkthrough,
    ReviewNote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Draft,
    AwaitingApproval,
    Approved,
    ChangesRequested,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactComment {
    pub id: String,              // "cmt_<8hex>"
    pub line_anchor: Option<u32>,// 1-based line number pada markdown artifact
    pub quote: Option<String>,   // Cuplikan baris yang dikomentari (maks 160 char)
    pub author: String,          // "user"
    pub body: String,
    pub resolved: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDocument {
    pub id: String,              // "art_<8hex>"
    pub session_id: String,
    pub workspace_alias: String,
    pub kind: ArtifactKind,
    pub title: String,
    pub content_md: String,
    pub status: ArtifactStatus,
    pub version: u32,
    pub updated_at: String,
    #[serde(default)]
    pub comments: Vec<ArtifactComment>,
}
```

#### 4.1.2 Persistensi & Integrasi `SnapshotStore`
* Lokasi penyimpanan: `~/.smart-tauri-hub/artifacts/<sha256_prefix_12(clean_ws)>/<session_id>/manifest.json` beserta file `.md` aktual (`implementation_plan.md`, `task.md`, `walkthrough.md`) sehingga user juga bisa membuka file Markdown tersebut di editor eksternal kapan saja.
* Setiap update artifact menaikkan `version += 1` dan memancarkan event Tauri `artifact-updated` (`ArtifactDocument`) agar panel kanan UI langsung ter-refresh tanpa menunggu akhir turn.

#### 4.1.3 Execution Mode: `direct` vs `plan_first`
Tambahkan pada run context / UI toggle di atas composer:
* **`direct` (default untuk tugas cepat):** Agent boleh langsung memanggil semua tool sesuai Guardrail Mode aktif. Agent tetap boleh membuat `task_list` atau `walkthrough` secara opsional.
* **`plan_first` (Antigravity Planning Mode):**
  1. Saat run dimulai dalam mode `plan_first`, `plan_approved` bernilai `false`.
  2. Di `enforce_guardrail_gate` (`tools.rs`), jika `run_ctx.execution_mode == "plan_first"` dan `!run_ctx.plan_approved` dan `!is_read_only` (kecuali built-in tool `upsert_artifact` dan `request_plan_approval`), **tolak eksekusi mutasi** dengan pesan jelas:
     `"Mode Plan-First aktif: buat Implementation Plan via upsert_artifact lalu panggil request_plan_approval. Tunggu persetujuan user sebelum mengedit kode."`
  3. Agent meneliti codebase (read-only), memanggil `upsert_artifact(kind="implementation_plan", ...)` dan `upsert_artifact(kind="task_list", ...)`, lalu memanggil `request_plan_approval(artifact_id, summary)`.
  4. Status artifact berubah menjadi `AwaitingApproval` dan turn berhenti dengan rapi (tidak menggantung/timeout menunggu user!).
  5. User meninjau rencana di panel kanan:
     * **Klik "Approve & Execute":** IPC `approve_artifact(session_id, artifact_id)` menandai `status = Approved`, menyetel `plan_approved = true` untuk sesi tersebut, dan otomatis memicu turn eksekusi: `"Implementation plan disetujui. Eksekusi langkah-langkah pada task list secara bertahap, perbarui task checklist, dan tutup dengan walkthrough."`
     * **Tambah Komentar Inline + Klik "Request Changes":** IPC `submit_artifact_feedback(session_id, artifact_id)` mengumpulkan seluruh `comments` berstatus `resolved == false`, memformatnya menjadi prompt terstruktur (lengkap dengan nomor baris & kutipan), dan mengirimkannya ke agent untuk merevisi rencana (`version + 1`).

#### 4.1.4 Built-in Tools Baru untuk Artifacts (Khusus LocalAgent & Opsional WebMCP)
1. `upsert_artifact`: `{ kind: "implementation_plan"|"task_list"|"walkthrough"|"review_note", title, content_md }` → menulis/memperbarui artifact sesi aktif.
2. `request_plan_approval`: `{ artifact_id?, summary }` → mengubah status plan ke `AwaitingApproval` dan memberi sinyal ke agent loop bahwa fase planning selesai di turn ini.

*(Catatan: Untuk `WebMCP`, `upsert_artifact` dapat menyimpan ke session `"webmcp-latest"` agar rencana yang dibuat Gemini Web juga tampil rapi di panel Artifacts aplikasi desktop Airlock!)*

---

### Pilar 2 — Parallel Agent Manager & Subagents (`src-tauri/src/agent_manager.rs`)

#### 4.2.1 Migrasi dari Single Run ke `AgentRunManager`
Gantikan field tunggal `agent_running` & `agent_cancel` pada `AppState` dengan `pub agent_manager: AgentRunManager`:

```rust
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Running,
    AwaitingApproval,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRunSummary {
    pub run_id: String,               // "run_<8hex>"
    pub parent_run_id: Option<String>,// Some(...) jika subagent
    pub session_id: String,
    pub workspace_alias: String,
    pub label: String,                // Judul singkat tugas
    pub provider: String,
    pub model: String,
    pub execution_mode: String,       // "direct" | "plan_first" | "read_only_subagent"
    pub status: AgentRunStatus,
    pub current_step: usize,
    pub max_steps: usize,
    pub last_activity_text: String,
    pub started_at: String,
    pub elapsed_secs: u64,
}
```

* **Batas Konkurensi:** Maksimal **4 active runs** bersamaan (`MAX_CONCURRENT_RUNS = 4`, configurable) untuk melindungi rate limit API (429) dan RAM/CPU lokal.
* **Isolasi History Per-Run/Per-Session:**
  Saat ini `AppState` menyimpan 1 `gemini_config.history`, 1 `openai_history`, 1 `anthropic_history`. Ubah menjadi `HashMap<String, Vec<Value>>` yang di-key berdasarkan `session_id` (atau `run_id` untuk subagent ephemeral), dengan batas cache LRU 12 sesi aktif di RAM!
  *(Manfaat tambahan: berpindah sesi chat di dropdown `chat-session-select` tidak lagi mencampur atau kehilangan context window LLM dari sesi lain!)*
* **Isolasi Mutasi File Lintas-Run (`WorkspaceWriteGuard`):**
  * Simpan `workspace_write_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>` di `AgentRunManager`.
  * Setiap pemanggilan tool mutatif pada `workspace_id` mengambil lock singkat workspace tersebut agar 2 run paralel tidak pernah menimpa file yang sama secara korup.
  * Lebih baik lagi: jika user mencoba memulai 2 primary run mutatif di `workspace_id` yang sama, tampilkan warning atau sarankan mode `read_only` / workspace berbeda.

#### 4.2.2 Built-in Tool `spawn_subagent`
Memungkinkan primary agent mendelegasikan tugas investigasi secara paralel (mirip subagent Antigravity / Claude Code):

```json
{
  "name": "spawn_subagent",
  "description": "Spawns a focused read-only subagent to investigate a question, audit files, or inspect another workspace in parallel without polluting the main context window. Returns a concise structured report.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "goal": { "type": "string", "description": "Specific research, audit, or verification goal" },
      "workspace": { "type": "string", "description": "Optional target workspace alias" },
      "focus_paths": { "type": "array", "items": { "type": "string" }, "description": "Optional paths to inspect first" },
      "max_steps": { "type": "integer", "description": "Max tool steps for subagent (default 6, max 10)" }
    },
    "required": ["goal"]
  },
  "annotations": {
    "readOnlyHint": true,
    "destructiveHint": false,
    "idempotentHint": true,
    "openWorldHint": false
  }
}
```

**Aturan Ketat `spawn_subagent`:**
1. **Strict Depth Limit (`depth == 1`):** Subagent **tidak diberikan** tool `spawn_subagent` di daftar deklarasi tool-nya, sehingga mustahil terjadi rekursi tak terbatas (*fork bomb*).
2. **Strict Read-Only Sandbox:** Subagent hanya menerima subset tool read-only (`project_overview`, `find_files`, `smart_search`, `code_outline`, `read_file_range`, `batch_inspect`, `git_ops` read-only, `list_workspaces`). Tidak bisa mengubah file atau menjalankan shell!
3. **Paralel Otomatis:** Karena `spawn_subagent` bertanda `readOnlyHint: true`, integrasi dengan eksekusi paralel Blueprint 02 membuat primary agent dapat memanggil 3 `spawn_subagent` sekaligus (misalnya: 1 audit backend, 1 audit frontend, 1 cek test coverage) dan ketiganya berjalan serentak!
4. **Budget Token & Waktu Terpisah:** Timeout keras 60 detik per subagent, output diringkas maksimal 6.000 karakter sebelum dikembalikan sebagai `functionResponse` ke parent run.

#### 4.2.3 Event Streaming Ber-`run_id`
Ubah payload event Tauri (tetap backward-compatible):
* `agent-thought-v2`: `{ run_id, session_id, delta }`
* `agent-chunk-v2`: `{ run_id, session_id, delta }`
* `agent-step-v2`: `{ run_id, session_id, step, max_steps, text }`
* `agent-runs-updated`: `Vec<AgentRunSummary>`

---

### Pilar 3 — Slash Workflows & Deterministic Lifecycle Hooks (`src-tauri/src/hooks.rs`)

#### 4.3.1 Slash Workflows
Satukan built-in prompts (`mcp_resources.rs:147`), custom workflow markdown per-proyek, dan composer UI:
* **Sumber Discovery:**
  1. Built-in workflows: `/plan`, `/review`, `/debug`, `/refactor`, `/architecture`, `/test`
  2. Workspace custom workflows: `<workspace>/.airlock/workflows/*.md` dan `<workspace>/.agents/workflows/*.md` (nama file tanpa `.md` menjadi `/nama-workflow`, baris pertama `# Title` atau frontmatter `description` menjadi subtitle).
* **Eksekusi:**
  * Di frontend (`src/views/agent.ts`), mengetik `/` di awal `#chat-input` memunculkan autocomplete popover ringan (navigasi panah Atas/Bawah + Enter/Tab).
  * Memilih `/plan <argumen>` otomatis menyetel `execution_mode = "plan_first"` dan melampirkan template instruksi workflow ke prompt.
  * IPC command: `list_workflows(workspace_alias?) -> Vec<WorkflowSummary>`.

#### 4.3.2 Deterministic Lifecycle Hooks (`src-tauri/src/hooks.rs`)

Format file konfigurasi (global di `AppConfig.hooks` atau per-proyek di `<workspace>/.airlock/hooks.json`):

```json
{
  "version": 1,
  "hooks": [
    {
      "id": "rust-check-after-edit",
      "event": "post_edit",
      "file_glob": "*.rs",
      "command": "cargo check --manifest-path src-tauri/Cargo.toml --message-format=short",
      "timeout_secs": 15,
      "max_output_lines": 30,
      "on_failure": "append_to_tool_output",
      "enabled": true
    },
    {
      "id": "ts-typecheck-after-edit",
      "event": "post_edit",
      "file_glob": "*.ts",
      "command": "npx tsc --noEmit",
      "timeout_secs": 15,
      "max_output_lines": 25,
      "on_failure": "append_to_tool_output",
      "enabled": true
    }
  ]
}
```

**Event Hook yang Didukung:**
* `post_edit`: dipicu tepat setelah `surgical_edit`, `multi_surgical_edit`, atau `write_file_jailed` berhasil pada file yang cocok dengan `file_glob` (pakai `security::wildcard_match`).
  * Jika `exit_code != 0` dan `on_failure == "append_to_tool_output"`, tempelkan blok `--- HOOK DIAGNOSTICS (rust-check-after-edit, exit 101) ---\n<compressed>` ke `ToolOutput.text`! Agent langsung melihat compiler error di turn yang sama tanpa membuang 1 round-trip tambahan.
* `pre_commit`: dipicu sebelum `git_ops(op="commit")`; jika `on_failure == "block"` dan `exit_code != 0`, batalkan commit dengan pesan error terkompresi.
* `post_turn`: dipicu setelah turn agent selesai (misalnya formatter atau ringkasan test).

**Gerbang Keamanan Anti-RCE untuk Workspace Hooks (KRITIS):**
1. Setiap perintah hook **wajib** melewati `security::validate_command(&hook.command)`.
2. Hook **tidak pernah dijalankan** saat effective `guardrail_mode == "read_only"`.
3. Untuk `.airlock/hooks.json` yang berasal dari dalam workspace:
   * Hitung `sha256_hex` isi file `.airlock/hooks.json`.
   * Bandingkan dengan `AppConfig.trusted_hook_hashes: HashMap<String, String>` (key = clean workspace path, value = approved SHA-256).
   * Jika belum dipercaya atau isi file berubah setelah `git pull`, **JANGAN eksekusi hook**; tampilkan banner/toast di UI: `"Workspace ini mendefinisikan .airlock/hooks.json baru/berubah. Tinjau & klik Trust Hooks untuk mengaktifkan."`
4. **Loop Prevention:** Hook dijalankan dengan `run_shell_raw` langsung (bukan lewat `execute_tool`), dibatasi timeout maks 20 detik, dan tidak boleh memicu hook lain.

---

## 5. Spesifikasi UI / UX (Obsidian Studio — Vanilla TS)

1. **Agent View Split Layout (`index.html` + `src/styles.css` + `src/views/artifacts.ts`):**
   * Di `#panel-agent`, tambahkan toggle button **`Artifacts`** di header (di sebelah `#btn-toggle-drawer`) dengan badge jumlah artifact/komentar terbuka.
   * Saat terbuka, `#agent-shell` bertransisi mulus menjadi 2 kolom (Chat Surface kiri ~58%, Artifacts Panel kanan ~42%, responsif menyembunyikan drawer konfigurasi saat sempit).
   * Di dalam **Artifacts Panel**:
     * Tab pill: `Plan` | `Tasks` | `Walkthrough` | `Runs (N)`.
     * Status badge (`Draft` / `Awaiting Approval` / `Approved` / `Changes Requested`) + tombol `Export to Repo` + `Copy Markdown`.
     * Baris/blok markdown interaktif: tombol kecil `+ Comment` per section/heading atau kotak komentar ber-anchor baris.
     * Footer aksi sticky saat `AwaitingApproval`: tombol primer **`Approve & Execute`** dan sekunder **`Request Changes`**.
2. **Execution Mode Switcher & Slash Popover (`src/views/agent.ts`):**
   * Segmented pill kecil di atas textarea composer: **`Direct`** | **`Plan First`**.
   * Popover autocomplete `/workflow` muncul di atas textarea saat mengetik `/` di karakter pertama.
3. **Parallel Runs Strip (`src/views/agent.ts`):**
   * Bar ringkas di bawah `.chat-sessions-bar` (hanya muncul bila ada >= 1 active/subagent run) menampilkan chip tiap run (`#1 Main · Step 4/16`, `Subagent: Auth Audit · Running`) lengkap dengan tombol Stop individual (`cancel_agent_run(run_id)`).
4. **Hooks Trust & Manager Modal (`src/views/modals.ts`):**
   * Tampilkan daftar perintah hook aktual dalam `<pre>` monospace sebelum user mengklik **`Trust & Enable Workspace Hooks`**.

---

## 6. Tauri IPC Commands & Kontrak TypeScript Baru

### Backend IPC (`src-tauri/src/main.rs`)
| Command | Parameter | Return |
| :--- | :--- | :--- |
| `run_local_agent_turn` *(upgrade)* | Tambah `session_id: Option<String>`, `execution_mode: Option<String>`, `workspace_alias: Option<String>` | `Result<AgentTurnResponse, String>` (tambah `run_id`, `awaiting_approval`) |
| `cancel_agent_run` | `run_id: Option<String>` (None = cancel semua active runs) | `Result<(), String>` |
| `list_agent_runs` | — | `Result<Vec<AgentRunSummary>, String>` |
| `list_session_artifacts` | `session_id: String` | `Result<Vec<ArtifactDocument>, String>` |
| `add_artifact_comment` | `session_id: String`, `artifact_id: String`, `line_anchor: Option<u32>`, `quote: Option<String>`, `body: String` | `Result<ArtifactDocument, String>` |
| `resolve_artifact_comment` | `session_id: String`, `artifact_id: String`, `comment_id: String`, `resolved: bool` | `Result<ArtifactDocument, String>` |
| `approve_artifact` | `session_id: String`, `artifact_id: String` | `Result<ArtifactDocument, String>` |
| `request_artifact_changes` | `session_id: String`, `artifact_id: String` | `Result<String, String>` (returns formatted feedback prompt) |
| `export_artifact_to_workspace` | `session_id: String`, `artifact_id: String` | `Result<String, String>` |
| `list_workflows` | `workspace_alias: Option<String>` | `Result<Vec<WorkflowSummary>, String>` |
| `get_workspace_hooks_status` | `workspace_alias: Option<String>` | `Result<HooksStatusSummary, String>` |
| `trust_workspace_hooks` | `workspace_alias: Option<String>`, `expected_sha256: String` | `Result<DashboardSnapshot, String>` |

---

## 7. Pitfalls & Gotchas Kritis untuk Antigravity

1. **Jangan Memblokir HTTP/Tauri Thread saat `AwaitingApproval`:**
   Berbeda dari HITL tool approval (yang menunggu `oneshot` 90 detik), `request_plan_approval` **tidak boleh** memblokir task agent dengan `oneshot`! Membaca rencana bisa memakan waktu 5–10 menit bagi manusia. Akhiri turn saat ini secara bersih dengan status `AwaitingApproval`, simpan history & artifact, lalu mulai turn baru saat user mengklik **Approve & Execute**.
2. **Per-Session History Isolation (Cegah Kebocoran Konteks Antar Sesi):**
   Pastikan `clear_agent_history(session_id?)` dan pergantian dropdown `#chat-session-select` tidak menghapus history sesi lain yang mungkin sedang berjalan di background!
3. **Subagent Cost & Rate Limit Control:**
   Batasi maksimal 3 subagent per parent run dan gunakan model cepat (`gemini-2.5-flash` / model aktif) dengan thinking `low` agar tidak memicu HTTP `429 Too Many Requests` dari Gemini/Anthropic API.
4. **Hook `post_edit` pada `multi_surgical_edit`:**
   Jika `multi_surgical_edit` mengubah 4 file `.rs` sekaligus, **deduplikasi** eksekusi hook `post_edit` berdasarkan `hook.id` sehingga `cargo check` hanya berjalan **1 kali** setelah seluruh batch selesai, bukan 4 kali berturut-turut!
5. **Re-verifikasi SHA-256 `hooks.json` Sebelum Eksekusi:**
   Selalu hitung ulang hash file `.airlock/hooks.json` tepat sebelum menjalankan perintah hook (bukan hanya saat startup), untuk menutup celah TOCTOU apabila agent atau `git pull` memodifikasi `.airlock/hooks.json` di tengah sesi!
6. **Sinkronisasi `open_artifact` dengan `src/markdown.ts`:**
   Pastikan `src/markdown.ts` mendukung checkbox task list (`- [ ]` / `- [x]`) dan tabel Markdown (lihat temuan Audit F-11) agar `implementation_plan.md` dan `task.md` tampil tajam di panel Artifacts.

---

## 8. Roadmap Implementasi Bertahap

### Fase 1 — Artifacts Store & Plan-First Gate
- [ ] Buat `src-tauri/src/artifacts.rs` (`ArtifactDocument`, `ArtifactComment`, persistensi disk, approval & feedback formatter).
- [ ] Tambahkan tool `upsert_artifact` dan `request_plan_approval` ke `tools.rs` serta penegakan gerbang `plan_first` di `enforce_guardrail_gate`.
- [ ] Tambahkan dukungan checkbox (`- [ ]` / `- [x]`) & tabel Markdown pada `src/markdown.ts`.
- [ ] Bangun `src/views/artifacts.ts` + layout split-pane pada `index.html` & `src/styles.css`.

### Fase 2 — Parallel `AgentRunManager` & `spawn_subagent`
- [ ] Buat `src-tauri/src/agent_manager.rs` dengan `HashMap<RunId, AgentRunHandle>`, per-session history map, workspace write lock, dan limit 4 concurrent runs.
- [ ] Migrasikan `local_agent.rs` dan `providers.rs` untuk menerima `RunContext` (`run_id`, `session_id`, `workspace_id`, `execution_mode`, `cancel_rx`, `depth`).
- [ ] Implementasikan built-in tool `spawn_subagent` (read-only, `depth == 1`, timeout 60s).
- [ ] Tambahkan Run Switcher strip & listener event `agent-*-v2` di `src/views/agent.ts` & `src/main.ts`.

### Fase 3 — Slash Workflows & Trusted Lifecycle Hooks
- [ ] Buat `src-tauri/src/hooks.rs` (parser `.airlock/hooks.json`, SHA-256 trust verification, deduplicated `post_edit`/`pre_commit`/`post_turn` runner, workflow loader `.airlock/workflows/*.md`).
- [ ] Integrasikan hook `post_edit` ke `surgical_edit`, `multi_surgical_edit`, `write_file_jailed` dan `pre_commit` ke `git_ops`.
- [ ] Tambahkan Slash Command autocomplete popover di `#chat-input` dan modal Trust Workspace Hooks di `src/views/modals.ts`.

---

## 9. Rencana Pengujian & Definition of Done

### Automated Tests Wajib (`cargo test`)
1. `artifacts::tests::test_artifact_crud_comments_and_feedback_formatting`
2. `tools::tests::test_plan_first_mode_blocks_mutative_tools_until_approved` (verifikasi `surgical_edit` ditolak sebelum `approve_artifact`, lalu berhasil setelah disetujui).
3. `agent_manager::tests::test_concurrent_runs_isolation_and_workspace_write_lock` (2 run paralel dengan `session_id` berbeda tidak saling mencampur history maupun mengkorupsi file).
4. `local_agent::tests::test_subagent_is_strictly_read_only_and_cannot_recurse` (pastikan deklarasi tool subagent tidak memuat `spawn_subagent` atau tool mutatif).
5. `hooks::tests::test_untrusted_workspace_hooks_never_execute_and_hash_change_revokes_trust` (modifikasi 1 byte pada `.airlock/hooks.json` otomatis membatalkan trust dan memblokir eksekusi).
6. `hooks::tests::test_post_edit_hook_appends_compressed_diagnostics_and_dedups_batch` (verifikasi output compiler gagal masuk ke `ToolOutput.text`).

### Verifikasi Build & Runtime
```powershell
cd src-tauri
cargo test
cd ..
npm run build
```
**Definition of Done:** User dapat menyalakan mode `Plan First`, menerima `Implementation Plan` di panel kanan, memberi komentar inline pada baris tertentu, menyetujui rencana, melihat agent menjalankan subagent riset secara paralel, dan mendapat diagnostik `post_edit` otomatis — seluruhnya pada model apa pun (Gemini, Claude, atau OpenAI-Compat/Ollama).
