# Blueprint 08 — Workspace `.airlock/` Handoff Auto-Sync (WebMCP -> Antigravity)

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent / Developer |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Implemented & Verified (`cargo test` 117 passed, 0 warnings; `--benchmark-webmcp` PASS 4/4; `npm run build` OK) |
| **Prioritas** | **P1 — Zero-Config Local Handoff ke Google Antigravity** |
| **Dependensi Blueprint** | Blueprint 01 (Multi-Workspace), 03 (Artifacts & Hooks), 06 (`session_checkpoint`), 07 (`webmcp_harness`) |

---

## 1. Intent & Latar Belakang

Saat ini Airlock menyimpan `session_checkpoint` (`~/.smart-tauri-hub/checkpoints/`), `ArtifactStore` (`~/.smart-tauri-hub/artifacts/`), blob rollback (`~/.smart-tauri-hub/snapshots/`), dan riwayat audit (`~/.smart-tauri-hub/audit_history.json`) di luar folder proyek.

Ketika pengguna memakai **Gemini Web (`WebMCP`)** untuk mengerjakan coding di suatu workspace lalu membuka folder workspace tersebut di **Google Antigravity** (mis. untuk review, debugging, atau melanjutkan implementasi — termasuk tanpa menghubungkan Antigravity ke MCP Airlock terlebih dahulu), Antigravity tidak melihat jejak kerja terbaru langsung dari disk workspace.

Blueprint ini merancang mekanisme **event-driven auto-sync** ke folder `<workspace>/.airlock/` yang:
1. Selalu menjaga ringkasan progres, checkpoint, diff, status command/test terakhir, audit trail, dan artifacts **dari sesi WebMCP** tersedia langsung di disk workspace.
2. Tetap **lokal sementara** (didaftarkan otomatis di `.git/info/exclude`) agar `git status` / `git diff` tetap bersih dan tidak mengganggu file `.airlock/` yang committable (`.airlock/hooks.json`, `.airlock/workflows/`, `.airlock/skills/`).

---

## 2. Goals, Non-Goals & Anti-Goals

### Goals
1. **Ketat Berbasis `WebMCP` & Real-Time:** Hanya aktivitas **`WebMCP`** (termasuk `delegate_local_agent` dan background job yang dipicu oleh `WebMCP`, serta sinkronisasi balik saat revert/review UI atas entri `WebMCP`) yang membuat atau memperbarui `.airlock/HANDOFF.md` dan `.airlock/handoff/`.
2. **Entry Point Tunggal untuk Antigravity:** `.airlock/HANDOFF.md` memuat status ringkas (`<= 6 KB`) berisi goal, keputusan, langkah selesai, next steps, file yang disentuh, status command/verifikasi terakhir, drift Git, dan pointer ke detail di `.airlock/handoff/`.
3. **Zero Git Pollution:** Pembuatan/pembaruan file handoff otomatis mendaftarkan `/.airlock/HANDOFF.md` dan `/.airlock/handoff/` ke `.git/info/exclude` (mendukung `.git` direktori standar maupun worktree `gitdir:` file), tanpa menyentuh `.gitignore` yang terlacak Git.
4. **Zero Self-Noise:** `collect_git_state`, `project_overview`, `overview_text_for_resource`, `smart_search`, dan `find_files` tidak menganggap file handoff lokal sebagai drift atau noise pencarian (kecuali target pencarian memang diarahkan langsung ke `.airlock`).
5. **Konfigurasi & Guardrail Aman:** Menghormati guardrail `read_only`, menyaring rahasia (token/API key) sebelum menulis ke disk, dan dapat diaktifkan/nonaktifkan lewat `AppConfig.webmcp_auto_handoff_dir` + UI panel **WebMCP Optimization**.

### Non-Goals
1. Tidak memindahkan `~/.smart-tauri-hub/snapshots/*.blob` ke dalam workspace; rollback blob tetap terpusat agar tidak membengkakkan repo (cukup cantumkan `last_audit_group_id` di `HANDOFF.md`).
2. Tidak mengganti `~/.smart-tauri-hub/checkpoints/` atau `~/.smart-tauri-hub/artifacts/` sebagai backing store utama; folder di workspace adalah **proyeksi/mirror** lokal.
3. Tidak bergantung pada ekstensi browser atau scraping DOM Gemini Web untuk mendeteksi akhir turn.
4. Tidak membuat atau memperbarui `.airlock/` dari aktivitas `LocalAgent` desktop, `--acp`, `--stdio`, `Simulator`, `Test`, maupun panggilan *read-only* murni (mis. `batch_inspect`, `read_file_range`, `smart_search`, `lsp_query`, `session_checkpoint(op="load")`).
5. Tidak mengubah perilaku session-ID panel Artifacts desktop di `src/views/artifacts.ts` (di luar cakupan blueprint ini).

### Anti-Goals (Failure Modes yang Harus Dicegah)
1. Menulis ke disk pada workspace berstatus guardrail `read_only`.
2. Menyebabkan `test_session_checkpoint_save_load_and_drift_detection` atau sesi normal mengalami false positive `LIVE_DRIFT_SINCE_CHECKPOINT` gara-gara `.airlock/HANDOFF.md` muncul sebagai untracked file.
3. Memblokir panggilan MCP WebMCP melebihi deadline RPC akibat `git diff` lambat atau race condition antar-tool paralel.
4. Membocorkan isi `.env`, `auth_token`, `cloudflare_tunnel_token`, API key provider, atau entri audit milik workspace/sumber lain ke `.airlock/handoff/`.
5. Menimpa atau mengganggu path `.airlock/artifacts/` yang dipakai tombol manual **Export** (`export_artifact_to_workspace`) di UI Artifacts.

---

## 3. Pendekatan Arsitektur yang Dipilih

Dipilih **Opsi A — Event-Driven Auto-Sync di Backend (`src-tauri/src/handoff.rs`)** karena:
- Protokol MCP Streamable HTTP (`src-tauri/src/mcp_server/http.rs`) bersifat per-tool-call dan tidak menerima event "model selesai menjawab" dari Gemini Web.
- **Opsi B** (hanya menulis saat `session_checkpoint(op="save")`) gagal jika model lupa memanggil save atau jika job `cargo test` selesai di background belakangan.
- **Opsi C** (`DELETE /mcp` + idle timer) rapuh karena Gemini Web jarang mengirim `DELETE /mcp` dan membuat file terlambat tersedia saat pengguna langsung beralih ke Antigravity.

**Keputusan Desain Inti (Opsi A):**
- **Kebijakan Sumber Ketat `WebMCP`:** Sesuai keputusan pengguna (*"ketat webmcp saja"*), pemicu penciptaan `.airlock/HANDOFF.md` dan seluruh isi `session-audit.jsonl` / `progress.md` / `HANDOFF.md` dibatasi ketat hanya untuk pekerjaan asal **`WebMCP`** (termasuk `delegate_local_agent` dan background `ManagedJob` asal WebMCP). Desktop `LocalAgent`, `--acp`, `--stdio`, `Simulator`, dan `Test` tidak pernah membuat `.airlock/` maupun mengotori log handoff.
- **Jalur Proyeksi Terpadu:** Satu coordinator `WorkspaceHandoffManager` pada `AppState` (dengan inner `Arc<HandoffCoordinator>` agar dapat dipakai oleh `jobs.rs` dan task background tanpa membutuhkan `Arc<AppState>`) membangun ulang file handoff secara atomik (`*.tmp` -> `rename` dengan fallback overwrite di Windows) dan terserialisasi per `workspace_id`.
- **Sinkronisasi Deterministik:**
  - `sync_workspace_handoff` dijalankan deterministik (`await` dengan lock per workspace dan timeout `git diff` ketat **2 detik**) di akhir tool mutatif/checkpoint/artifact WebMCP, tepat **sebelum** `done_tx.send(true)` pada `jobs.rs`, serta setelah `finish_delegated_run` pada `local_agent.rs`.
  - Tidak ada race condition antara output `ANTIGRAVITY_HANDOFF_PROMPT` / selesainya job dengan ketersediaan `.airlock/HANDOFF.md` di disk.

---

## 4. Layout & Skema File di `<workspace>/.airlock/`

```text
<workspace>/
└── .airlock/
    ├── hooks.json                 # (Eksisting, committable — tidak disentuh)
    ├── workflows/                 # (Eksisting, committable — tidak disentuh)
    ├── skills/                    # (Eksisting, committable — tidak disentuh)
    ├── artifacts/                 # (Eksisting, manual UI Export — tidak disentuh)
    ├── HANDOFF.md                 # BARU: Entry point utama untuk Antigravity (<= 6 KB)
    └── handoff/                   # BARU: Detail lokal sementara (auto-excluded via .git/info/exclude)
        ├── checkpoint.json        # Mirror SessionCheckpoint JSON (dihapus saat op="clear")
        ├── progress.md            # Checklist task/checkpoint + 5 command/verify terakhir + delegated summary
        ├── latest.diff            # Bounded unified diff (staged+unstaged git diff + untracked full diff, <= 32 KB)
        ├── session-audit.jsonl    # Ring buffer maks 50 entri aksi WebMCP workspace ini (tanpa read-only noise)
        └── artifacts/             # Auto-mirror artifact WebMCP & delegated_* (merge terbaru per kind)
```

### 4.1 Kontrak Isi Tiap File

1. **`.airlock/HANDOFF.md` (Target `<= 6.144` chars)**
   Urutan bagian tetap (deterministik agar mudah di-parse manusia & agent):
   - **Header Metadata:** Workspace `@alias`, clean root path (tanpa `\\?\`), branch aktif + short `HEAD`, timestamp update terakhir, trigger terakhir (`WebMCP tool`, `Job finished`, `Delegated run`, `Checkpoint saved/cleared`, `Artifact updated`, `Audit reverted`, `Manual sync`), serta `Last Audit Group ID` (jika ada, untuk revert di UI Airlock).
   - **Current Goal & Status:** Diambil dari `SessionCheckpoint` aktif jika ada; jika belum ada `session_checkpoint(op="save")`, tampilkan status sintetis: `"Active WebMCP session (no explicit session_checkpoint saved yet)"` beserta ringkasan aktivitas terakhir.
   - **Decisions / Completed / Next Steps / Open Issues / Notes:** Diambil dari `SessionCheckpoint` (masing-masing dipotong rapi agar total file tetap `<= 6 KB`).
   - **Recent Touched Files:** Gabungan terdeduplikasi (maks 15 file) dari `SessionCheckpoint.recent_touched_files` dan entri audit `WebMCP` (termasuk `delegated_*`) milik workspace ini (`can_revert == true` atau file ops), menandai status `[reverted]` bila sudah di-revert.
   - **Latest Verification & Commands:** Ringkasan 3 eksekusi command/job asal `WebMCP` terbaru pada workspace ini (status `completed`/`failed`/`running`/`timed_out`, exit code, durasi, plus maks 12 baris ekor output yang sudah disaring rahasianya).
   - **Live Git Working Tree:** Output singkat `git status --short` (maks 20 baris, sudah memfilter `.airlock/HANDOFF.md` & `.airlock/handoff/`) atau `(clean working tree)`.
   - **Artifacts & Supporting Files:** Daftar file yang tersedia di `.airlock/handoff/` (`progress.md`, `latest.diff`, `session-audit.jsonl`, `checkpoint.json`, `artifacts/*.md`) serta pengingat MCP resource `airlock://workspace/checkpoint` / `airlock://workspace/handoff`.

2. **`.airlock/handoff/checkpoint.json`**
   - Salinan persis `SessionCheckpoint` (`serde_json::to_string_pretty`) dari `~/.smart-tauri-hub/checkpoints/<ws_id>.json` bila ada.
   - Saat `session_checkpoint(op="clear")` dipanggil oleh `WebMCP`: hapus `.airlock/handoff/checkpoint.json` bila ada, lalu render ulang `.airlock/HANDOFF.md` dan `.airlock/handoff/progress.md` dengan catatan `"Checkpoint cleared at <timestamp>"` sambil tetap menampilkan diff/audit aktif bila masih ada.

3. **`.airlock/handoff/progress.md`**
   - Memuat bagian `Completed` & `Next Steps` dari `SessionCheckpoint` (bila ada), plus isi `task.md` jika artifact `task_list` tersedia di `.airlock/handoff/artifacts/`.
   - Memuat **Recent Command & Job Runs** (maks 5 job/command asal `WebMCP` terakhir untuk workspace ini: command, status, exit code, elapsed, dan hingga 30 baris tail log terkompresi yang sudah diredaksi).
   - Memuat **Latest Delegated Local Agent Summary** (jika ada `DelegatedRunResult` terakhir untuk workspace ini: `run_id`, `mode`, `status`, `touched_files`, dan ringkasan hasil).

4. **`.airlock/handoff/latest.diff` (Batas `<= 32 KB`)** *(keputusan desain implementasi: mencakup staged + unstaged + file baru untracked)*
   - **Bagian A (Tracked Changes — Staged & Unstaged):**
     - Cek apakah `HEAD` ada (`git rev-parse --verify HEAD`, timeout 2s).
     - Jika `HEAD` ada: jalankan `git diff HEAD --no-color -- . :(exclude).airlock/HANDOFF.md :(exclude).airlock/handoff` (sehingga `git_ops(op="add")` tetap memperlihatkan seluruh diff yang sudah di-stage!).
     - Jika belum ada commit `HEAD` (repo baru `git init`): gabungkan `git diff --cached --no-color -- . :(exclude).airlock/HANDOFF.md :(exclude).airlock/handoff` dan `git diff --no-color -- . :(exclude).airlock/HANDOFF.md :(exclude).airlock/handoff`.
   - **Bagian B (Untracked New Files):**
     - Jalankan `git status --porcelain --untracked-files=all` (atau filter dari status yang sudah diambil) untuk mencari file berawalan `?? ` di luar `.airlock/HANDOFF.md` & `.airlock/handoff/`.
     - Untuk file untracked yang disentuh oleh WebMCP (atau file teks non-sensitif `<= 16 KB` yang tercatat di audit WebMCP belum di-revert), render unified diff sintetis penuh terhadap `/dev/null` (`--- /dev/null`, `+++ b/<rel>`, `@@ -0,0 +1,<N> @@`, baris `+...`) langsung dari isi file di disk — sehingga jika file baru dibuat via `write_file_jailed` lalu diedit lagi dengan `surgical_edit`, Antigravity tetap melihat isi lengkap file baru tersebut, dan tidak duplikat setelah `git_ops(op="add")`.
   - Jika total gabungan melebihi 32 KB, potong bagian tengah/ekor dengan marker eksplisit `... [truncated at 32 KB — run git diff or inspect files directly] ...`.

5. **`.airlock/handoff/session-audit.jsonl` (Maks 50 baris terbaru)** *(keputusan desain implementasi: strict WebMCP + filter out read-only noise)*
   - Setiap baris adalah JSON ringkas `HandoffAuditRecord`:
     `{ id, timestamp, source, tool_name, target_path, summary, can_revert, reverted, group_id, post_edit_hash }`
   - **Filter Ketat (3 syarat wajib):**
     1. **Milik workspace ini:** `workspace_id == ws.id` (atau fallback saat `workspaces` registry kosong: `workspace_root` cocok `ws.clean_path` — **jangan** memasukkan entri dengan `workspace_root == None` seperti `list_workspaces`!).
     2. **Asal WebMCP:** `source == "WebMCP"` atau `source == "WebMCP:delegate"` / `is_webmcp_origin == true` (mengabaikan `LocalAgent` desktop, `Stdio`, `Test`, `Simulator`).
     3. **Bukan read-only noise:** hanya entri tool mutatif file/git, eksekusi command/job, `delegate_local_agent`, `upsert_artifact`/`request_plan_approval`, dan `session_checkpoint(op="save"|"clear")`. Tolak `batch_inspect`, `read_file_range`, `smart_search`, `project_overview`, `find_files`, `code_outline`, `read_symbol`, `lsp_*`, `list_workspaces`, `list_skills`, `load_skill`, `read_skill_resource`, `session_checkpoint(op="load")`, dan `poll_command_job`.
   - Entri yang menyentuh path sensitif (saat `!ws.effective_allow_sensitive`) tidak menulis isi/diff ke file handoff.

6. **`.airlock/handoff/artifacts/*.md`** *(keputusan desain implementasi: merge newest per `ArtifactKind`)*
   - Mengambil semua `ArtifactDocument` milik workspace ini pada sesi `"webmcp-latest"` maupun seluruh sesi `"delegated_*"` lewat metode baru `ArtifactStore::list_workspace_latest_by_kind(clean_ws, &["webmcp-latest", "delegated_"])`.
   - Untuk setiap `ArtifactKind` (`implementation_plan`, `task_list`, `walkthrough`, `review_note`), pilih **1 dokumen terbaru** berdasarkan `updated_at` (tie-breaker: utamakan `"webmcp-latest"` lalu `version` lebih tinggi). Dengan cara ini, jika `delegate_local_agent` menulis `review_note.md`, file `implementation_plan.md` dan `task.md` dari `"webmcp-latest"` **tidak terhapus**.
   - Tulis `doc.content_md` murni ke `.airlock/handoff/artifacts/<filename>` dan hapus file `.md` kind lama di `.airlock/handoff/artifacts/` bila tidak ada lagi di hasil merge.

---

## 5. Manajemen `.git/info/exclude` (Zero Git Pollution)

Fungsi `ensure_git_info_exclude(ws_canon_root: &Path)` di `src-tauri/src/handoff.rs`:
1. Cek apakah `ws_canon_root.join(".git")` ada:
   - Jika `.git` adalah direktori -> target = `ws_canon_root.join(".git/info/exclude")`.
   - Jika `.git` adalah file (worktree/submodule dengan `gitdir: ...`) -> jalankan `git rev-parse --git-path info/exclude` di `ws_canon_root` (timeout 2s, `CREATE_NO_WINDOW`). Jika relatif, gabungkan dengan `ws_canon_root`.
   - Jika bukan repo Git -> return `Ok(())` (tidak ada yang perlu dikecualikan).
2. Pastikan direktori induk (`info/`) ada (`create_dir_all`), baca isi `exclude` jika sudah ada.
3. Cek apakah `/.airlock/HANDOFF.md` dan `/.airlock/handoff/` sudah ada di baris terpisah (setelah trim). Jika sudah lengkap, tandai `exclude_ensured` di cache memori dan return `Ok(())`.
4. Jika belum ada, tambahkan blok:
   ```gitignore
   # >>> Airlock local handoff (auto-managed)
   /.airlock/HANDOFF.md
   /.airlock/handoff/
   # <<< Airlock local handoff
   ```
   Tulis secara atomik (`.tmp` + `rename` fallback).

---

## 6. Rincian Perubahan per Modul

### 6.1 Modul Baru: `src-tauri/src/handoff.rs`
- **Structs & Types:**
  - `HandoffTrigger`: enum (`ToolMutation { tool_name }`, `CommandFinished { tool_name }`, `JobFinished { job_id }`, `DelegatedFinished { run_id }`, `CheckpointSaved`, `CheckpointCleared`, `ArtifactUpdated { session_id }`, `AuditReverted`, `ManualSync`).
  - `HandoffCoordinator` (dibungkus `Arc` di `WorkspaceHandoffManager`):
    - `ws_locks: tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>`
    - `exclude_ensured: tokio::sync::RwLock<HashSet<String>>`
    - `latest_delegated: tokio::sync::RwLock<HashMap<String, DelegatedRunResult>>` (key `ws.id`)
    - `cached_snapshots: tokio::sync::RwLock<HashMap<String, CachedHandoffContext>>` (menyimpan snapshot konfigurasi ringan, daftar rahasia teredaksi, checkpoint terbaru, dan ringkasan audit/artifact WebMCP terakhir agar `jobs.rs` dapat me-refresh `progress.md` & `HANDOFF.md` saat background job selesai tanpa butuh `Arc<AppState>`!).
- **Redaksi Rahasia (`redact_secrets`):**
  - Mengumpulkan nilai non-kosong dengan panjang `>= 6` karakter dari `state.auth_token` dan `state.provider_secrets` (`cloudflare_tunnel_token`, `gemini_api_key`, `openai_api_key`, `anthropic_api_key`, `downstream_bearer_tokens`, `downstream_envs`, `plugin_secrets`).
  - Mengganti kemunculan nilai tersebut di teks output/summary/diff sebelum ditulis ke `.airlock/`.
- **Jailed Atomic Writer:**
  - Semua target di dalam `.airlock/` divalidasi lewat `crate::security::resolve_jailed_path(&ws.canon_root, rel_path)` sehingga symlink keluar workspace ditolak.
  - Penulisan file memakai temp file sibling + `fs::rename` (dengan fallback `fs::write` + hapus `.tmp` di Windows).
- **Skip & Guard Rules:**
  - Pada trigger otomatis (`!force_manual`): jika `!cfg.webmcp_auto_handoff_dir` -> no-op.
  - Jika `ws.effective_guardrail == "read_only"` -> selalu tolak/no-op (pada trigger otomatis: no-op; pada `sync_workspace_handoff_now`: return `Err(...)`).
  - Helper `pub(crate) fn workspace_has_handoff(ws: &ResolvedWorkspace) -> bool` memeriksa keberadaan `.airlock/HANDOFF.md` (untuk gating re-sync saat revert/review UI agar tidak pernah membuat folder baru secara tak terduga).

### 6.2 `src-tauri/src/config.rs`, `state.rs`, `main.rs`
1. `config.rs`: Tambah field `#[serde(default = "default_true")] pub webmcp_auto_handoff_dir: bool` pada `AppConfig` dan inisialisasi di `Default::default()`. Update unit test `test_legacy_config_without_webmcp_fields_loads_defaults` untuk memverifikasi default `true`.
2. `state.rs`:
   - Tambah `#[serde(default)] pub is_webmcp_origin: bool` pada `AuditEntry` (backward-compatible dengan `audit_history.json` lama; diisi `true` bila `source == "WebMCP"` atau berasal dari run `delegated_*` di `tools/mod.rs` / `local_agent.rs`).
   - Tambah `pub handoff_manager: crate::handoff::WorkspaceHandoffManager` pada `AppState`.
   - Tambah `pub webmcp_auto_handoff_dir: bool` pada `DashboardSnapshot` dan isi di `AppState::snapshot()`.
3. `main.rs`:
   - Daftarkan `mod handoff;`.
   - Tambah parameter opsional `webmcp_auto_handoff_dir: Option<bool>` di Tauri command `update_webmcp_settings`.
   - Tambah Tauri command `sync_workspace_handoff_now(workspace_alias: Option<String>) -> Result<String, String>` *(keputusan desain implementasi: backend/diagnostic command tanpa tombol UI tambahan; mem-bypass `webmcp_auto_handoff_dir == false`, tetapi mengembalikan `Err(...)` jika workspace berstatus `read_only`)*.
   - Di `approve_artifact` dan `request_artifact_changes`: jika `doc.session_id == "webmcp-latest"` atau `doc.session_id.starts_with("delegated_")` **dan** `handoff::workspace_has_handoff(&ws)`, panggil `handoff::sync_from_state(&state, &ws, HandoffTrigger::ArtifactUpdated { session_id }).await`.

### 6.3 `src-tauri/src/tools/mod.rs` & `src-tauri/src/tools/revert.rs`
1. **Tracking Origin WebMCP pada `AuditEntry` & Tool Hooks (`tools/mod.rs`):**
   - Tentukan `is_webmcp_origin = source == "WebMCP" || run_ctx.map(|c| c.execution_mode.starts_with("delegated_")).unwrap_or(false)`.
   - Saat `is_webmcp_origin == true`, pastikan `AuditEntry` terbaru yang baru dicatat pada workspace tersebut (oleh `execute_tool_core_inner`) ditandai `is_webmcp_origin = true` (atau gunakan helper pencatat/penanda di akhir `execute_tool_core` sebelum `record_audit` flush, plus pastikan `group_id` pada `delegated_*` mengisi `run_ctx.turn_group_id` bila `current_turn_group_id` kosong).
   - Evaluasi `should_sync_webmcp_tool(is_webmcp_origin, tool_name, &args, &out)`:
     - Wajib `is_webmcp_origin == true` (tidak pernah aktif untuk `LocalAgent` non-delegated, `Stdio`, `Simulator`, `Test`).
     - Tool termasuk kelompok mutatif/eksekusi/artifact:
       - `surgical_edit`, `multi_surgical_edit`, `patch_and_verify`, `write_file_jailed`, `file_ops` (bila `!out.is_error` atau bila `patch_and_verify`/`multi_surgical_edit` sudah menyentuh file lalu gagal pada step verify!).
       - `run_command`, `run_command_compressed`, `start_command_job` (sinkronkan baik exit 0 maupun exit non-zero agar error gagal build/test ikut tercatat untuk debugging Antigravity; abaikan bila gagal validasi guardrail/command policy sebelum eksekusi).
       - `poll_command_job` atau `run_command(action="poll")` jika status job berubah selesai pada panggilan tersebut (`STATUS: completed|failed|killed|timed_out`).
       - `git_ops` bila `op` adalah `"add"` atau `"commit"`.
       - `upsert_artifact`, `request_plan_approval`.
     - Tolak sinkronisasi jika `out.is_error` berasal dari blokir guardrail/validasi awal (`out.text.starts_with("Diblokir oleh ")`, `"Akses ditolak"`, `"Command Policy"`, atau `"Argumen "`/`"Format "`).
2. **Re-sync saat Revert (`tools/revert.rs`):**
   - Setelah `revert_audit_entry` (dan sekali di akhir `revert_audit_group`, bukan per-item di loop grup) sukses mengubah `entry.reverted = true`: jika `entry.is_webmcp_origin || entry.source == "WebMCP"` dan workspace asal masih valid serta `.airlock/HANDOFF.md` sudah ada di workspace tersebut (`workspace_has_handoff`), panggil `handoff::sync_from_state(state, &ws, HandoffTrigger::AuditReverted).await`.

### 6.4 `src-tauri/src/checkpoints.rs`
1. `exec_session_checkpoint`:
   - Terima `source: &str` yang sudah ada di parameter fungsi: hanya bila `source == "WebMCP"` maka:
     - Pada `op == "save"`: setelah `save_checkpoint_atomic`, panggil `crate::handoff::sync_from_state(state, ws, HandoffTrigger::CheckpointSaved).await` (non-fatal jika gagal tulis workspace; tambahkan baris info `HANDOFF_PATH: .airlock/HANDOFF.md` saat sukses atau `HANDOFF_SYNC_WARN:` saat gagal).
     - Pada `op == "clear"`: setelah menghapus checkpoint sentral, jika `handoff::workspace_has_handoff(ws)`, panggil `crate::handoff::sync_from_state(state, ws, HandoffTrigger::CheckpointCleared).await`.
   - Panggilan `session_checkpoint` dari `"Stdio"`, `"LocalAgent"`, `"Test"`, atau `"Simulator"` **tidak** menyentuh `.airlock/`.
2. `build_antigravity_handoff_prompt`:
   - Tetap menyertakan `airlock://workspace/checkpoint` dan `session_checkpoint(op="load")`, dan tambahkan referensi `.airlock/HANDOFF.md` (mis. `read '.airlock/HANDOFF.md' (or load airlock://workspace/checkpoint)`).
3. `collect_git_state`:
   - Filter baris `git status --short` agar baris untuk `.airlock/HANDOFF.md` atau `.airlock/handoff/` tidak pernah dianggap sebagai drift.

### 6.5 `src-tauri/src/jobs.rs`, `local_agent.rs`, `artifacts.rs`, `tools/fs_tools.rs`, `tools_extra.rs`
1. **Background Job Completion (`jobs.rs`, `tools/shell.rs`, `tools/fs_tools.rs`, `tools_extra.rs`):**
   - Tambah `pub source: Option<String>` pada `ManagedJob` dan `JobSummary` (`#[serde(default)]`).
   - Tambah fungsi `start_job_for_source` & `start_pipeline_job_for_source` (dengan `source: Option<String>`), sementara `start_job_with_handle` & `start_pipeline_job_with_handle` tetap menjadi wrapper `source = None` agar signature eksisting 100% kompatibel.
   - `tools/shell.rs`, `tools/fs_tools.rs` (`patch_and_verify` / `multi_surgical_edit`), dan `tools_extra.rs` (`start_command_job`) memanggil varian `*_for_source(..., Some(source.to_string()))`.
   - Di watcher selesai `jobs.rs`, jika `job_source.as_deref() == Some("WebMCP")` (atau `job_source.as_deref() == Some("WebMCP:delegate")`) dan `handoff_ctx` tersedia, panggil `handoff::sync_after_job_finished(...)` **sebelum** `done_tx.send(true)`.
2. **Delegated Local Agent (`local_agent.rs`):**
   - Di `exec_delegate_local_agent_start` (saat `finish_delegated_run` dipanggil): simpan `final_res` ke `state_bg.handoff_manager` untuk `ws_bg.id`, lalu jika `mode_bg == "execute"` atau `touched_files` tidak kosong atau `handoff::workspace_has_handoff(&ws_bg)`, panggil `crate::handoff::sync_from_state(&state_bg, &ws_bg, HandoffTrigger::DelegatedFinished { run_id: run_id_bg.clone() }).await`.
3. **Artifact Query Merge (`artifacts.rs`):**
   - Tambah `pub async fn list_workspace_latest_by_kind(&self, clean_ws: &str, session_prefixes: &[&str]) -> Vec<ArtifactDocument>` yang memindai cache memori + subdirektori `<artifacts_root>/<ws_hash>/*/manifest.json` untuk sesi yang cocok `"webmcp-latest"` atau prefix `"delegated_"`, lalu mengembalikan maksimal 1 dokumen terbaru per `ArtifactKind`.
4. **Scanner Exclusions (`tools/fs_tools.rs` & `tools_extra.rs`):**
   - Tambahkan helper bersama `crate::handoff::is_handoff_ignored_rel(rel: &str) -> bool` yang mengenali `.airlock/handoff`, `.airlock/handoff/...`, dan `.airlock/handoff.md` (case-insensitive).
   - Terapkan pada `scan_project_overview_blocking`, `overview_text_for_resource`, `run_smart_search_blocking` (kecuali `search_root` sudah berada di dalam `.airlock/handoff`), dan `run_find_files_blocking` (kecuali `search_root` berada di dalam `.airlock/handoff`).

### 6.6 `src-tauri/src/mcp_resources.rs`
1. Tambah resource `airlock://workspace/handoff` (`name: "Workspace Handoff Summary"`, `mimeType: "text/markdown"`) di `resources_list()` dan `read_resource_with_context()` serta template `workspace://{alias}/handoff`:
   - Jika `.airlock/HANDOFF.md` ada di workspace, kembalikan isinya; jika belum ada, render preview handoff on-the-fly tanpa menulis ke disk (read-only safe).

### 6.7 Frontend UI (`index.html`, `src/types.ts`, `src/views/bridge.ts`)
1. `index.html`: Di dalam panel **WebMCP Optimization** (setelah `#toggle-webmcp-auto-verify-edit`), tambah 1 toggle:
   - Checkbox `#toggle-webmcp-auto-handoff` ("Auto-Export Workspace `.airlock/HANDOFF.md`") dengan hint jelas bahwa folder lokal dikecualikan otomatis via `.git/info/exclude`.
2. `src/types.ts`: Tambah `webmcp_auto_handoff_dir: boolean;` di `DashboardSnapshot`.
3. `src/views/bridge.ts`: Render nilai `#toggle-webmcp-auto-handoff` pada `renderSnapshot`, ikat `change` listener memanggil `saveWebmcpSettings({ webmcpAutoHandoffDir: enabled }, ...)`, serta sertakan di tombol `#btn-save-webmcp`.

---

## 7. Rencana Pengujian & Verifikasi Otomatis

### Unit & Integration Tests Baru (di `src-tauri/src/handoff.rs`, `artifacts.rs`, `tools/revert.rs`, & `webmcp_benchmark.rs`)
1. **`test_auto_handoff_creates_files_and_updates_git_info_exclude`:**
   - Init repo Git di temp dir (`git init`), jalankan `surgical_edit` & `run_command` dari `source = "WebMCP"`.
   - Verifikasi `.airlock/HANDOFF.md`, `.airlock/handoff/progress.md`, `.airlock/handoff/latest.diff`, `.airlock/handoff/session-audit.jsonl` terbentuk, dan `.git/info/exclude` berisi blok Airlock secara idempoten.
   - Verifikasi `git status --short` **tidak** menampilkan `.airlock/HANDOFF.md` atau `.airlock/handoff/`.
2. **`test_auto_handoff_strict_webmcp_only_ignores_other_sources`:**
   - Jalankan `surgical_edit`, `run_command`, dan `session_checkpoint(op="save")` dengan `source = "LocalAgent"`, `"Stdio"`, `"Test"` -> verifikasi `.airlock/HANDOFF.md` dan `.airlock/handoff/` **tidak dibuat** dan entri mereka **tidak muncul** di `session-audit.jsonl` setelah WebMCP bekerja.
3. **`test_latest_diff_includes_staged_and_full_untracked_new_files`:**
   - Buat file baru dengan `write_file_jailed` lalu edit lagi dengan `surgical_edit` -> verifikasi `latest.diff` memuat diff penuh file baru terhadap `/dev/null`; lalu jalankan `git_ops(op="add")` -> verifikasi diff tetap muncul di `latest.diff` (via `git diff --cached` / `git diff HEAD`) tanpa duplikasi.
4. **`test_artifacts_merge_newest_per_kind_across_webmcp_and_delegated`:**
   - Simpan `implementation_plan` di `"webmcp-latest"` dan `review_note` di `"delegated_abc"` -> verifikasi `.airlock/handoff/artifacts/` menyimpan **keduanya** (`implementation_plan.md` tidak terhapus).
5. **`test_auto_handoff_handles_git_worktree_gitdir_file`:**
   - Simulasikan worktree (`.git` sebagai file `gitdir: ...`) dan verifikasi `info/exclude` tetap ditulis ke gitdir yang benar tanpa error.
6. **`test_auto_handoff_respects_read_only_guardrail_and_config_toggle`:**
   - Set guardrail `read_only` -> mutasi ditolak & `.airlock/` tidak dibuat; `sync_workspace_handoff_now` mengembalikan `Err`.
   - Set `webmcp_auto_handoff_dir = false` -> edit WebMCP berhasil tapi `.airlock/HANDOFF.md` tidak dibuat otomatis; `sync_workspace_handoff_now` tetap bisa memaksa tulis manual.
7. **`test_auto_handoff_redacts_secrets_and_filters_readonly_audit`:**
   - Pastikan `auth_token`/secret yang tercetak di output command terganti `<redacted>` di `HANDOFF.md`/`progress.md`, audit Workspace A tidak bocor ke Workspace B, dan 50+ panggilan `read_file_range` tidak menggeser edit WebMCP keluar dari `session-audit.jsonl`.
8. **`test_auto_handoff_updates_after_background_job_checkpoint_and_revert`:**
   - Jalankan job via `start_command_job` (`source = "WebMCP"`) -> tunggu `done_rx` -> verifikasi exit code & tail output sudah masuk ke `.airlock/handoff/progress.md` & `HANDOFF.md` tanpa polling manual.
   - Panggil `session_checkpoint(op="save")` & `op="clear"`, lalu uji `revert_audit_entry` -> verifikasi `checkpoint.json` terhapus saat clear dan `[reverted]` muncul setelah revert.
9. **`test_scanners_ignore_handoff_dir_while_direct_read_works`:**
   - Verifikasi `project_overview`, `smart_search`, dan `find_files` tidak mengembalikan `.airlock/HANDOFF.md` atau `.airlock/handoff/*`, sementara `.airlock/hooks.json` tetap terindeks dan `read_file_range(path=".airlock/HANDOFF.md")` tetap berhasil.
10. **Benchmark/E2E (`webmcp_benchmark.rs`):**
    - Pastikan seluruh skenario `--benchmark-webmcp` tetap PASS.

### Gate Verifikasi Akhir (Wajib)
```powershell
Set-Location D:\tauri-mcp-hub\src-tauri
cargo test
cargo run -- --benchmark-webmcp
Set-Location D:\tauri-mcp-hub
npm run build
```

---

## 8. Batasan yang Diserahkan ke Implementer
- Detail formatting Markdown spesifik di dalam `HANDOFF.md` dan `progress.md` (selama menjaga seluruh section wajib dan batas karakter `<= 6 KB` / `<= 32 KB`).
- Pembantu internal (*helper functions*) dan organisasi sub-fungsi di `src-tauri/src/handoff.rs`.
- Copy toast/label UI persis di `src/views/bridge.ts` dan `index.html` mengikuti pola Obsidian Studio yang sudah ada.
