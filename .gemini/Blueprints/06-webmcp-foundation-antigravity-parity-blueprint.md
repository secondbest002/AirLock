# Blueprint 06 — WebMCP Foundation Overhaul & Antigravity Parity (5 Pillars)

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent (Autonomous / Co-pilot Handoff) |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Ready for Implementation |
| **Prioritas** | **P0 — Fondasi Reliabilitas, Efisiensi Turn & Latensi WebMCP (Gemini Web / Remote Clients)** |
| **Dependensi Blueprint** | Membutuhkan Blueprint 01–05 & Studi Real-World Tahap A/B (seluruhnya sudah terimplementasi di baseline `76 passed; 0 failed`) |

---

## 1. Handoff Brief & Problem Statement

### Mengapa Sesi Gemini Web + Airlock Bisa Berjam-Jam (vs. 15 Menit di Antigravity)?

Meskipun Airlock telah memiliki 28 built-in tools (AST, LSP, Artifacts, Skills, MCP Tasks, Background Jobs), pengalaman pengguna pada klien browser **Gemini Web (`WebMCP`)** mengalami degradasi parah pada sesi panjang (*long session*): banyak putaran pendek, model berhenti di tengah jalan (memaksa user mengetik *"lanjut"* berulang kali), latensi meningkat eksponensial, dan *timeout/hang*.

Akar masalahnya adalah **mismatch struktural** antara runtime *turn-based* browser (batas durasi HTTP, batas iterasi tool beruntun per turn, dan akumulasi *context window* tanpa pemadatan otomatis) dengan permukaan MCP Airlock saat ini:

| # | Lokasi Kode Saat Ini | Kelemahan Fondasi | Dampak ke Sesi WebMCP |
| :---: | :--- | :--- | :--- |
| **W-01** | `src-tauri/src/tools/schemas.rs`, `tools/mod.rs:139-148`, `mcp_server/rpc.rs:168-181` | **28 tool dipaparkan flat** ke `WebMCP` (termasuk `spawn_subagent` yang **pasti gagal** di luar Local Agent, serta tool granular tunggal yang sudah tercakup versi batch). | Membebani ~1.800 token schema tiap turn; model memilih tool granular satu-per-satu (4–6 round-trip untuk 1 inspeksi). |
| **W-02** | `src-tauri/src/tools/shell.rs:101`, `tools/fs_tools.rs:919`, `tools_extra.rs:887` | **Jebakan siklus pendek:** `auto_promote_secs = 9s` dan `poll_command_job` `wait_secs = 12s`. | Build/test 55 detik memakan **5–6 putaran tool** hanya untuk menunggu 1 perintah; cepat menghabiskan kuota loop per turn Gemini Web. |
| **W-03** | `src-tauri/src/mcp_server/rpc.rs:368`, `mcp_server/http.rs:46-68, 184-189` | **Hard timeout 24s** tanpa keepalive SSE pada `POST /mcp`, plus eksekusi batch JSON-RPC masih **sekuensial** (`for msg in &messages`). | Cold-start LSP (`rust-analyzer`) atau beberapa tool read-only dalam 1 batch rawan terputus di detik ke-24. |
| **W-04** | `src-tauri/src/tools/fs_tools.rs:1365-1456` | `multi_surgical_edit` tidak menyediakan **inline LSP diagnostics** maupun **verify command**. | Setiap selesai mengedit file selalu butuh minimal 1–2 putaran tambahan hanya untuk mengecek error compiler. |
| **W-05** | `src-tauri/src/tools/mod.rs`, `tools/fs_tools.rs` | **Tidak ada WebMCP Output Token Budget** terpusat (payload bisa mencapai 16 KB+ per tool call). | *Context poisoning* cepat terjadi setelah 12–15 turn; inferensi melambat drastis dan rawan hang. |
| **W-06** | `src-tauri/src/mcp_server/rpc.rs:67-100`, `local_agent.rs`, `agent_manager.rs` | `initialize` tidak mengirim `instructions`, belum ada jembatan **WebMCP → Local Agent** (`delegate_local_agent`), dan belum ada **server-side `session_checkpoint`**. | Model bekerja tanpa aturan efisiensi, iterasi edit-compile-fix yang panjang membebani browser, dan pindah chat baru berarti kehilangan konteks kerja. |

### Target Metrik Keberhasilan (Antigravity Parity Benchmarks)

| Metrik | Baseline Saat Ini | Target Blueprint 06 |
| :--- | :---: | :---: |
| Jumlah tool pada `tools/list` (`WebMCP compact`) | 28 tool (~7,2 KB schema) | **9–10 Super-Tools** (~2,4 KB schema, hemat >65%) |
| Putaran tool untuk *inspect + edit 3 file + verify (15s)* | 7–10 round-trip | **2 round-trip** (`batch_inspect` → `multi_surgical_edit + verify`) |
| Putaran tool untuk build/test berdurasi 60 detik | 5–6 round-trip | **2–3 round-trip** (`18s` sync + `22s` long-poll) |
| Ukuran payload rata-rata respons tool `WebMCP` | Hingga 16 KB | **≤ 6 KB terkompresi cerdas** (error-first) |
| Tugas iteratif multi-file (edit → fix error → test hijau) | 15–30 turn + manual *"lanjut"* | **2–3 interaksi WebMCP** via `delegate_local_agent` |
| Biaya pemulihan konteks saat buka Chat Baru | 4–6 turn baca ulang repo | **1 turn** via `session_checkpoint(op="load")` |

---

## 2. Global Engineering Invariants (Wajib Dipatuhi)

Seluruh implementasi Blueprint 06 **wajib** menjaga kontrak inti Airlock:
1. **100% Backward Compatibility MCP & IPC:**
   * Semua 28 nama tool eksisting (`project_overview`, `smart_search`, `read_file_range`, `surgical_edit`, `patch_and_verify`, `run_command_compressed`, `start_command_job`, `poll_command_job`, `stop_command_job`, `lsp_diagnostics`, dll.) **tetap valid dan dapat dieksekusi** di `execute_tool_core` meskipun disembunyikan dari `tools/list` mode `compact`.
   * Endpoint `/mcp`, `/mcp/:token`, `--stdio`, `--acp`, dan Tauri commands eksisting tidak boleh putus.
2. **Persistensi Aman (`#[serde(default)]`):**
   * Semua field baru di `AppConfig` (`src-tauri/src/config.rs`) wajib memiliki `#[serde(default = "...")]`.
   * Data `session_checkpoint` disimpan di `~/.smart-tauri-hub/checkpoints/` (di luar direktori workspace agar **tidak pernah mengotori `git status`**).
3. **Keamanan Jail, Guardrail & Anti-Rekursi:**
   * Semua akses file/workspace tetap melewati `resolve_workspace` + `resolve_jailed_path` + `enforce_guardrail_gate_for_workspace`.
   * `delegate_local_agent` wajib menghormati `pinned_ws`, memblokir mode tulis jika guardrail workspace adalah `read_only` atau toggle opt-in `webmcp_allow_write_delegation` nonaktif, serta **melarang rekursi** (`delegate_local_agent` tidak boleh ada di tool list milik Local Agent).
   * Mutative tool calls **tidak boleh** dijalankan paralel di JSON-RPC batch (hanya contiguous read-only calls yang paralel).
4. **Windows Path & Process Tree Safety:**
   * Tetap gunakan `build_shell_command`, `CREATE_NO_WINDOW (0x08000000)`, dan `kill_pid_tree` (`taskkill /PID <pid> /T /F`).
   * Strip prefix `\\?\` pada semua path tampilan/checkpoint.
5. **Zero-Framework XSS-Safe UI:**
   * Semua kontrol konfigurasi baru di `src/views/bridge.ts` dan `src/presets.ts` memakai Vanilla TypeScript (`createElement`/`textContent`, zero `innerHTML`).

---

## 3. Arsitektur Target Lintas 5 Pilar

```text
  +-------------------------------------------------------------------+
  |  Gemini Web / Remote Browser Client                               |
  |  (Arsitek, Perencana, Reviewer — Konteks Bersih & Ringkas)        |
  +---------------------------------+---------------------------------+
                                    |  POST /mcp?profile=compact
                                    |  (Accept: application/json[, text/event-stream])
                                    v
  +-------------------------------------------------------------------+
  |  Airlock MCP Server (mcp_server/auth.rs, http.rs, rpc.rs)         |
  |                                                                   |
  |  [Pilar 5] initialize -> result.instructions (WebMCP Playbook)    |
  |  [Pilar 1] tools/list -> Compact Profile (9-10 Super-Tools)       |
  |  [Pilar 3] Hybrid Parallel Read-Only Batch + 32s RPC Timeout      |
  |            + Optional SSE Keepalive (: ping tiap 6s)              |
  |            + Centralized WebMCP Output Token Budget (<= 6 KB)     |
  +--------+---------------------------+-------------------+----------+
           |                           |                   |
           v                           v                   v
  +------------------+      +---------------------+   +----------------------+
  | [Pilar 2]        |      | [Pilar 4]           |   | [Pilar 5]            |
  | Macro Tools      |      | Web Brain ->        |   | Session Checkpoint   |
  | - batch_inspect  |      | Local Executor      |   | - ~/.smart-tauri-hub |
  |   (+slices/syms) |      | - delegate_local_   |   |   /checkpoints/      |
  | - multi_surgical |      |   agent(start|poll  |   | - Auto git-status &  |
  |   _edit (+LSP    |      |   |stop)            |   |   audit-file enrich  |
  |   +verify+chain) |      | - Runs inside       |   | - Drift check on     |
  | - run_command    |      |   AgentRunManager   |   |   load in New Chat   |
  |   (18s/22s)      |      | - Compact Report    |   |                      |
  +------------------+      +---------------------+   +----------------------+
```

---

## 4. Desain Detail 5 Pilar

### Pilar 1 — WebMCP Compact Surface & Profile Routing

#### 1.1 Resolusi Profil Koneksi (`mcp_server/auth.rs` & `config.rs`)
Tambahkan enum `McpToolProfile` (`Compact` | `Full`):
1. Pada `TokenQuery` (`src-tauri/src/mcp_server/auth.rs:11`), tambah:
   ```rust
   pub(super) profile: Option<String>,
   ```
2. Tambah helper `extract_tool_profile(headers: &HeaderMap, query: &TokenQuery, default_profile: &str) -> McpToolProfile`:
   * Prioritas 1: Query `?profile=compact` atau `?profile=full`.
   * Prioritas 2: Header `X-Airlock-Profile: compact | full` (daftarkan header ini di `CorsLayer::allow_headers` di `mcp_server/mod.rs:97`).
   * Prioritas 3: `AppConfig.webmcp_default_profile` (default `"compact"` untuk jalur HTTP `WebMCP`, sedangkan `--stdio` dan Local Agent tetap memakai `"full"`).
3. Propagasikan `tool_profile: McpToolProfile` ke `handle_single_rpc_with_context` (buat wrapper/struct `RpcRequestContext { pinned_ws: Option<String>, tool_profile: McpToolProfile, source: &'static str }`).

#### 1.2 Peta Konsolidasi: 28 Tool → 10 Compact Super-Tools (`tools/schemas.rs`)
Tambahkan fungsi `pub fn mcp_tools_list_for_profile(profile: McpToolProfile, enable_delegation: bool) -> Value`:

| # | Compact Super-Tool | Menggantikan / Menyerap Tool Granular | Catatan Desain |
| :---: | :--- | :--- | :--- |
| 1 | `list_workspaces` | `list_workspaces` | Tetap (ringan, 0 argumen). |
| 2 | `batch_inspect` | `project_overview`, `find_files`, `smart_search`, `code_outline`, `read_file_range`, `read_symbol` | Diperluas dengan `read_slices[]`, `symbol_reads[]`, dan `find_pattern` (detail di Pilar 2.1). |
| 3 | `multi_surgical_edit` | `surgical_edit`, `multi_surgical_edit`, `patch_and_verify` | Diperluas dengan `post_check_lsp` dan `verify_command` / `verify_commands[]` (detail di Pilar 2.2). |
| 4 | `write_file_jailed` | `write_file_jailed` | Tetap untuk pembuatan file baru atau rewrite penuh. |
| 5 | `file_ops` | `file_ops` | Tetap (`mkdir`, `move`, `delete`). |
| 6 | `run_command` | `run_command_compressed`, `start_command_job`, `poll_command_job`, `stop_command_job` | Unified shell & async job manager via `action: "run" \| "start" \| "poll" \| "stop"` + `commands[]` fail-fast pipeline (detail di Pilar 2.3 & 3.1). |
| 7 | `git_ops` | `git_ops` | Tetap (`status`, `diff`, `diff_staged`, `log`, `branch`, `show`, `blame`, `stash_list`, `add`, `commit`). |
| 8 | `lsp_query` | `lsp_diagnostics`, `lsp_definition`, `lsp_references`, `lsp_hover` | Unified LSP tool via `op: "diagnostics" \| "definition" \| "references" \| "hover"`. |
| 9 | `session_checkpoint` | *(baru — Pilar 5)* | `op: "save" \| "load" \| "clear"` untuk transisi *New Chat* tanpa hilang konteks. |
| 10 | `delegate_local_agent` | *(baru — Pilar 4)* | `action: "start" \| "poll" \| "stop"`. Hanya muncul di `tools/list` jika `enable_delegation == true` & kredensial Local Agent tersedia. |

**Tool yang secara eksplisit DIKELUARKAN dari profil `compact`:**
* `spawn_subagent` — **bug fix wajib**: pasti error di WebMCP (`tools/mod.rs:141`). Tidak boleh lagi muncul di `WebMCP` baik `compact` maupun `full`!
* `upsert_artifact`, `request_plan_approval` — hanya bermakna untuk loop Local Agent / UI Artifacts Review; di Gemini Web membuang turn tanpa efek pause-loop native.
* `list_skills`, `load_skill`, `read_skill_resource` — dihilangkan dari default `compact` kecuali `AppConfig.webmcp_expose_skills == true` (atau diakses lewat `resources/read` `skill://...`).
* Downstream MCP tools (`mcp_hub`): tetap diagregasi jika user secara sadar mengaktifkan downstream server.

#### 1.3 Preset Gemini Web (`src/presets.ts`)
Perbarui preset `"gemini-web"` di `src/presets.ts:72-93` agar menampilkan URL dengan `?profile=compact` (dan mencantumkan opsi `?profile=full` sebagai alternatif lanjutan), serta menyinkronkan badge profil aktif di tab **MCP Bridge**.

---

### Pilar 2 — Macro-Actions ("1-Turn Inspect" & "Edit + Auto-Diagnostics + Verify")

#### 2.1 Upgrade `batch_inspect` (`src-tauri/src/tools/fs_tools.rs:1154`)
Saat ini `batch_inspect` hanya mendukung `include_overview`, `outline_paths`, dan `searches`. Tambahkan 3 kapabilitas opsional agar fase investigasi selesai dalam **1 kali panggil**:
1. `find_patterns: Option<Vec<{ pattern: String, path?: String, max_results?: u64 }>>` (maks 3 pola, memakai walker yang sama dengan `exec_find_files`).
2. `read_slices: Option<Vec<{ workspace?: String, path: String, start_line?: usize, end_line?: usize }>>` (maks 6 slice, default `end_line = start_line + 100`, batas maksimum 160 baris per slice di mode `WebMCP`).
3. `symbol_reads: Option<Vec<{ path: String, symbol: String, include_docs?: bool }>>` (maks 6 simbol, memanggil `crate::semantic::read_symbol_from_content` secara paralel via `JoinSet`).

**Aturan Eksekusi:** Seluruh sub-operasi (`overview`, `find_patterns`, `outlines`, `searches`, `read_slices`, `symbol_reads`) dijalankan **konkuren** dalam satu `JoinSet`, lalu dirangkai dengan *fair-share char budget* per seksi agar 1 seksi yang panjang tidak memotong seksi lain.

#### 2.2 Upgrade `multi_surgical_edit` dengan `post_check_lsp` & `verify_command` (`src-tauri/src/tools/fs_tools.rs:1365`)
Tambahkan parameter opsional pada schema & implementasi `exec_multi_surgical_edit`:
* `post_check_lsp: Option<bool>` — default `false` untuk kompatibilitas backward, atau direkomendasikan `true` lewat `initialize.instructions` di WebMCP.
  * Setelah Phase 2 (semua file ditulis & post-edit hooks selesai), jalankan pengecekan diagnostik cepat terhadap `edited_paths`:
  * **Fast-Path Non-Blocking Guard (Kritis):** Tambah helper `lsp_bridge::quick_diagnostics_for_files(state, ws, &edited_paths, Duration::from_millis(2500))`.
    * Jika LSP server untuk bahasa terkait **sudah aktif (warm)** di `LspManager`, sinkronkan dokumen (`didChange`/`didSave`) dan ambil diagnostik `severity <= 2` (Error & Warning) dengan timeout ketat **2,5 detik**.
    * Jika LSP server **belum berjalan (cold)**, **JANGAN spawn blocking** di dalam `multi_surgical_edit`; lakukan background warmup opsional atau kembalikan satu baris ringkas `LSP_POST_CHECK: skipped (server cold — call lsp_query(op="diagnostics") if needed)`. Ini mencegah cold-start `rust-analyzer` (15–30s) memicu timeout edit!
* `verify_command: Option<String>` / `verify_commands: Option<Vec<String>>` + `auto_promote_secs: Option<u64>`:
  * Validasi seluruh perintah dengan `validate_command_with_policy` **sebelum** menjalankan (dan jika mode guardrail memerlukan HITL untuk shell, pastikan gate shell dipanggil *sebelum* menulis file atau ditangani eksplisit di `tools/mod.rs` / `guardrail.rs`).
  * Jalankan sekuensial fail-fast via `jobs::start_job_with_handle` (untuk `verify_commands` lebih dari 1, gabungkan lewat runner internal atau eksekusi berurutan dalam satu managed pipeline job sehingga satu `job_id` memantau seluruh rantai!).
  * Tunggu hingga `promote_secs` (default **`18s`** untuk `WebMCP`). Jika selesai, kembalikan `DIFF + LSP_POST_CHECK + VERIFY EXIT CODE + COMPRESSED OUTPUT` dalam **1 respons tunggal**. Jika melebihi 18s, promosikan ke background job dengan `isError: false` dan kembalikan `AUTO_PROMOTED_VERIFY_JOB` + `JOB_ID`.

#### 2.3 Unified `run_command` + Fail-Fast Pipeline & Unified `lsp_query` (`tools_extra.rs`, `tools/shell.rs`, `lsp_bridge.rs`)
1. **`run_command` (wrapper terpadu):**
   ```json
   {
     "action": "run | start | poll | stop",
     "command": "string (untuk run/start tunggal)",
     "commands": ["cmd1", "cmd2"],
     "job_id": "string (untuk poll/stop)",
     "from_line": 0,
     "wait_secs": 22,
     "auto_promote_secs": 18,
     "timeout_secs": 300,
     "max_lines": 60,
     "workspace": "@alias"
   }
   ```
   * Jika `commands` array diisi (maks 5 langkah), jalankan sebagai **deterministic fail-fast pipeline** di `jobs.rs` (jalankan langkah 1, cek `exit_code == 0`, lanjut langkah 2, berhenti pada error pertama, log jelas `[STEP 1/2] $ ...`). Tidak perlu shell chaining `&&` yang sering diblokir allowlist F-06 — tiap perintah divalidasi terpisah terhadap `validate_command_with_policy`!
2. **`lsp_query` (wrapper terpadu):**
   * Parameter: `op: "diagnostics" | "definition" | "references" | "hover"`, `path`, `line`, `character`, `severity`, `max_results`, `workspace`.
   * Mendelegasikan langsung ke `exec_lsp_diagnostics`, `exec_lsp_definition`, `exec_lsp_references`, `exec_lsp_hover` yang sudah ada.

---

### Pilar 3 — Transport Timing, Streamable Keepalive, Parallel Batch & Token Guard

#### 3.1 Tuning Jendela Waktu WebMCP (`config.rs`, `rpc.rs`, `shell.rs`, `fs_tools.rs`, `tools_extra.rs`)
Hindari angka ajaib (*magic numbers*) tersebar. Definisikan konstanta terpusat & field konfigurasi di `AppConfig` (`src-tauri/src/config.rs`):

| Parameter `AppConfig` | Default Lama | Default Baru | Clamp Aman | Invarian Kritis |
| :--- | :---: | :---: | :---: | :--- |
| `webmcp_rpc_timeout_secs` | `24s` (hardcoded) | **`32s`** | `20..=60` | Selalu lebih besar minimal `+6s` dari `poll_wait_secs` |
| `webmcp_auto_promote_secs` | `9s` (hardcoded) | **`18s`** | `5..=25` | Memungkinkan build inkremental selesai dalam 1 call |
| `webmcp_poll_wait_secs` | `12s` (hardcoded) | **`22s`** | `0..=26` | Memangkas jumlah poll ~2x lipat; bangun instan saat `done_rx` berubah |
| `webmcp_max_output_chars` | tidak ada (hingga 16 KB) | **`6000`** | `3000..=16000` | Mencegah *context poisoning* di sesi panjang browser |
| `webmcp_sse_keepalive` | tidak ada | **`false`** *(opt-in)* | `bool` | Aktifkan stream SSE keepalive pada `POST /mcp` untuk proxy ketat |

**Update pesan HINT:** Perbarui seluruh teks hint di `shell.rs:205`, `fs_tools.rs:960`, dan `tools_extra.rs:843` agar mencetak nilai dinamis `wait_secs` (mis. `22s`) dan menyinggung `run_command(action="poll", ...)` atau `poll_command_job(...)` sesuai tool yang dipanggil.

#### 3.2 Hybrid Parallel Execution untuk Batch JSON-RPC (`src-tauri/src/mcp_server/http.rs:184-189`)
Saat ini `process_mcp_post` mengeksekusi array JSON-RPC secara sekuensial. Ubah dengan pola **contiguous read-only chunking**:
1. Buat helper `is_rpc_message_read_only(msg: &Value) -> bool`:
   * `true` untuk `ping`, `tools/list`, `resources/list`, `resources/read`, `prompts/list`, `skills/list`, `tasks/get`, `tasks/list`, serta `tools/call` di mana `!name.contains("__")` dan `is_tool_read_only(name, args) == true`.
   * `false` untuk semua tool mutatif, downstream MCP, `tasks/cancel`, dan `tools/call` dengan `params.task`.
2. Kelompokkan `messages` menjadi segmen berurutan:
   * Segmen read-only berisi ≥2 item → eksekusi paralel via `futures::future::join_all` (atau `tokio::task::JoinSet` dengan preservasi indeks asli).
   * Segmen mutatif → eksekusi sekuensial ketat satu per satu.
3. Hasil `outs` tetap mempertahankan **urutan persis** sesuai `messages` input.

#### 3.3 Optional SSE Keepalive untuk `POST /mcp` Panjang (`src-tauri/src/mcp_server/http.rs`)
Sesuai spesifikasi MCP Streamable HTTP (`2025-03-26` / `2026-06-01`):
* Jika `cfg.webmcp_sse_keepalive == true` **DAN** header request `Accept` mengandung `text/event-stream` **DAN** request berisi `tools/call` (bukan `initialize` / `tools/list`):
  * Jalankan eksekusi RPC pada background task yang mengirim hasil ke `oneshot::channel`.
  * Tunggu fast-path `3s`: jika selesai dalam ≤3 detik, kembalikan `application/json` biasa (zero overhead untuk tool cepat!).
  * Jika >3 detik, upgrade respons menjadi `axum::response::Sse` (`Content-Type: text/event-stream`, header `mcp-session-id` tetap dipasang), kirim SSE comment keepalive `: keepalive\n\n` setiap **6 detik** hingga `oneshot` selesai, lalu kirim satu event `Event::default().event("message").data(json_string)` dan tutup stream.
* Catatan: `GET /mcp` tetap `405` (stateless mode) agar tidak membebani koneksi idle permanen di browser.

#### 3.4 Centralized WebMCP Output Token Guard (`src-tauri/src/tools/mod.rs`)
Tambahkan modul helper `compress_for_webmcp_budget(tool_name: &str, text: &str, max_chars: usize) -> String` yang dipanggil tepat sebelum return di `execute_tool_core` **khusus bila `source == "WebMCP"`**:
1. Jika `text.chars().count() <= max_chars` → return apa adanya.
2. Jika melebihi `max_chars`:
   * **Header & Control Lines Preservation:** Pertahankan baris pembuka yang berisi metadata operasional (`JOB_ID:`, `RUN_ID:`, `GROUP_ID:`, `EXIT CODE:`, `STATUS:`, `NEXT_FROM_LINE:`, `AUTO_PROMOTED`, `STALL_WARNING:`, `LSP_POST_CHECK:`).
   * **Error-Priority Extraction:** Untuk output command/verify/LSP, ekstrak maksimal 25 baris yang mengandung pola error (`error[`, `error:`, `FAILED`, `panicked at`, `TS2`, `Traceback`, `SyntaxError`) terlebih dahulu.
   * **Head + Tail Split:** Alokasikan sisa budget ~55% untuk head (konteks awal) dan ~45% untuk tail (hasil akhir/ringkasan), sisipkan penanda eksplisit:
     ```text
     ... [WebMCP Output Guard: 8,420 chars omitted to protect browser context. Narrow 'path'/'start_line' or call with targeted slice/from_line.] ...
     ```
3. Catat estimasi token yang dihemat ke `AuditEntry` / telemetry jika dipotong.

---

### Pilar 4 — Arsitektur "Web Brain, Local Executor" (`delegate_local_agent`)

#### 4.1 Konsep & Pembagian Peran
Alih-alih memaksa Gemini Web menjalankan 15–20 putaran baca→edit→compile→fix error→test lewat tunnel HTTP, Gemini Web mendelegasikan fase implementasi/investigasi iteratif ke **Local Agent AirLock** yang berjalan langsung di mesin lokal:
* **Gemini Web (Web Brain):** Diskusi dengan user, arsitektur, review hasil, keputusan produk.
* **AirLock Local Agent (Local Executor):** Membaca banyak file, melakukan edit berulang, menjalankan `cargo check`/`npm test`, memperbaiki error compiler secara mandiri hingga selesai, lalu menyetor **1 laporan ringkas (≤ 4,5 KB)** ke Gemini Web.

#### 4.2 Schema & Kontrak Tool `delegate_local_agent`
```json
{
  "name": "delegate_local_agent",
  "description": "Delegates an iterative multi-step coding, refactoring, verification, or deep research task to Airlock's Local Agent so it executes locally without consuming WebMCP turns. Use action='start' to launch (waits up to 18s; returns RUN_ID if still working), action='poll' to long-poll up to 22s for completion, or action='stop' to cancel.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "action": { "type": "string", "enum": ["start", "poll", "stop"], "description": "Default: 'start'" },
      "goal": { "type": "string", "description": "Complete task specification, constraints, and acceptance criteria (required for action='start')" },
      "workspace": { "type": "string", "description": "Optional mounted workspace @alias" },
      "mode": { "type": "string", "enum": ["read_only", "execute"], "description": "'read_only' for deep investigation/audit (default); 'execute' allows edits & verification commands if permitted by workspace guardrail and Airlock settings" },
      "focus_paths": { "type": "array", "items": { "type": "string" }, "description": "Optional starting relative paths" },
      "verify_command": { "type": "string", "description": "Optional verification command the local agent must run and report before finishing" },
      "max_steps": { "type": "integer", "description": "Max local agent steps (default 12, range 1..20)" },
      "run_id": { "type": "string", "description": "Delegated run ID (required for action='poll' or 'stop')" },
      "wait_secs": { "type": "integer", "description": "Long-poll wait seconds for action='poll' (default 22, range 0..26)" }
    }
  }
}
```

#### 4.3 Manajemen State & Laporan Ringkas (`agent_manager.rs` & `local_agent.rs`)
1. Tambahkan struct `DelegatedRunResult` di `src-tauri/src/agent_manager.rs`:
   ```rust
   #[derive(Debug, Clone, Serialize)]
   pub struct DelegatedRunResult {
       pub run_id: String,
       pub workspace_alias: String,
       pub mode: String,
       pub status: AgentRunStatus,
       pub steps_taken: usize,
       pub max_steps: usize,
       pub elapsed_secs: u64,
       pub turn_group_id: String,
       pub touched_files: Vec<String>,
       pub reply_summary: String,
       pub error_message: Option<String>,
       pub awaiting_approval_hint: Option<String>,
   }
   ```
2. Pada `AgentRunHandle` (`agent_manager.rs:61`), tambahkan:
   * `status_tx: watch::Sender<AgentRunStatus>` dan `status_rx: watch::Receiver<AgentRunStatus>` (untuk event-driven long-poll tanpa spin-wait).
   * `final_result: Option<DelegatedRunResult>`.
   * `pinned_workspace_alias: String` (untuk verifikasi `pinned_ws` saat `poll`/`stop`).
3. Tambahkan fungsi `local_agent::start_delegated_webmcp_run(state: Arc<AppState>, ws: ResolvedWorkspace, req: DelegationRequest) -> Result<(String, watch::Receiver<AgentRunStatus>), String>`:
   * **Validasi Keamanan Berlapis:**
     1. Cek kredensial provider aktif di `SecretsStore` / `AppState` (jika belum dikonfigurasi, return error informatif: `"Local Agent provider belum memiliki API key di Airlock. Konfigurasikan di tab Local Agent atau jalankan tool langsung."`).
     2. Jika `mode == "execute"`:
        * Tolak jika `ws.effective_guardrail == "read_only"`.
        * Tolak jika `!cfg.webmcp_allow_write_delegation` (default `false` — user mengaktifkan 1x di tab MCP Bridge untuk keamanan eksplisit).
     3. Pastikan `delegate_local_agent` **disaring keluar** dari daftar deklarasi tool yang dikirim ke Local Agent (mencegah loop delegasi rekursif).
   * **Prompt Pembungkus Deterministik:** Suntikkan instruksi agar Local Agent bekerja tuntas, menjalankan `verify_command` (jika diberikan), dan menutup dengan ringkasan terstruktur (perubahan file, hasil test, risiko tersisa) maksimal 300 kata.
   * **Async Handoff Identik dengan `run_command`:**
     * `action="start"` menunggu hingga `webmcp_auto_promote_secs` (`18s`). Jika selesai cepat → kembalikan laporan akhir langsung! Jika masih berjalan → kembalikan `DELEGATION_RUNNING`, `RUN_ID`, `CURRENT_STEP`, `LAST_ACTIVITY`, dan instruksi memanggil `delegate_local_agent(action="poll", run_id=..., wait_secs=22)`.
     * `action="poll"` menunggu pada `status_rx.wait_for(|s| *s != AgentRunStatus::Running)` selama `wait_secs` (`22s`). Begitu status menjadi `Completed`, `Failed`, `Cancelled`, atau `AwaitingApproval`, langsung bangun!
     * Jika status `AwaitingApproval`: kembalikan status jelas agar Gemini Web memberi tahu user untuk menyetujui popup HITL di jendela desktop Airlock, lalu melanjutkan poll.
     * Saat `Completed`: rangkum `touched_files` (dari audit entries dengan `group_id == turn_group_id`), `GROUP_ID` (untuk 1-click group revert di UI), jumlah step, durasi, dan `reply_summary` (dipotong aman ≤ 4.500 karakter).

#### 4.4 Sinkronisasi `execute_tool_core` untuk `Arc<AppState>` (`src-tauri/src/tools/mod.rs` & `mcp_server/rpc.rs`)
Perhatikan bahwa `rpc.rs:357` saat ini memanggil `execute_tool_with_context(state, ...)` yang mengirim `&AppState` (sehingga `state_arc` adalah `None`).
* Ubah pemanggilan di `rpc.rs:357` menjadi `execute_tool_with_run_context(Arc::clone(state), "WebMCP", tool_name, args, pinned_ws, None).await` (atau tambah `execute_tool_arc_with_context`).
* Dengan demikian `delegate_local_agent` menerima `Arc<AppState>` yang dibutuhkan untuk `tokio::spawn` background agent run secara bersih.

---

### Pilar 5 — Protocol Discipline (`initialize.instructions`) & Server-Side `session_checkpoint`

#### 5.1 `initialize.instructions` Dinamis (`src-tauri/src/mcp_server/rpc.rs:67-100`)
Spesifikasi MCP (`InitializeResult`) menyediakan field standar `instructions?: string` yang disuntikkan klien ke konteks sistem model. Tambahkan generator `build_mcp_initialize_instructions(state: &AppState, profile: McpToolProfile, pinned_ws: Option<&str>) -> String`:

```text
AIRLOCK WEBMCP OPERATING PROTOCOL (Profile: compact):
1. MINIMIZE ROUND-TRIPS: Start exploration with a single `batch_inspect` call (combine overview + searches + outlines + read_slices/symbol_reads). Issue independent read-only tool calls in parallel in the same turn.
2. ATOMIC EDIT + VERIFY: Prefer `multi_surgical_edit` (even for 1 file) with `post_check_lsp: true` and `verify_command` when validating changes. Avoid re-reading files you just edited.
3. LONG COMMANDS: Use `run_command` (auto-promotes after 18s; `action="poll"` long-polls 22s automatically). NEVER run `sleep` or spin-poll.
4. DELEGATE HEAVY LOOPS: For iterative multi-file implementation/refactor/fix-until-tests-pass loops (>3 files or repeated compile fixes), delegate via `delegate_local_agent` instead of burning WebMCP turns.
5. LONG-SESSION HYGIENE: Every ~12-15 turns or before context grows heavy, call `session_checkpoint(op="save")` so the user can open a fresh chat and resume in 1 turn via `session_checkpoint(op="load")`.
Active workspace(s): @tauri-mcp-hub (guardrail: autonomous).
```

* Catatan: Jaga panjang `instructions` tetap **≤ 1.200 karakter (~280 token)** agar tidak memboroskan context window, dan sesuaikan item #4 (hilangkan baris `delegate_local_agent` jika fitur delegasi tidak aktif).

#### 5.2 Modul Baru `src-tauri/src/checkpoints.rs` & Tool `session_checkpoint`
Buat modul khusus `src-tauri/src/checkpoints.rs` untuk menyimpan state kerja persisten per workspace di `~/.smart-tauri-hub/checkpoints/<workspace_id>.json`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCheckpoint {
    pub workspace_id: String,
    pub workspace_alias: String,
    pub updated_at: String,
    pub goal: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub open_issues: Vec<String>,
    #[serde(default)]
    pub model_notes: String,
    // Auto-enriched oleh server saat save:
    #[serde(default)]
    pub git_head_short: Option<String>,
    #[serde(default)]
    pub git_status_short: Vec<String>,
    #[serde(default)]
    pub recent_touched_files: Vec<String>,
    #[serde(default)]
    pub running_job_ids: Vec<String>,
    #[serde(default)]
    pub last_audit_group_id: Option<String>,
}
```

**Operasi `session_checkpoint`:**
* `op = "save"`:
  * Validasi ukuran teks input (batasi total JSON ≤ 8 KB agar ringkas dan bebas sampah).
  * **Auto-Enrichment sisi server:** Secara otomatis ambil `git rev-parse --short HEAD`, `git status --short` (maks 20 baris), 10 `target_path` mutasi terakhir dari `state.audit_log` untuk `ws.id`, `group_id` terakhir, dan `job_id` yang masih `Running` di `JobManager`.
  * Tulis atomik (`.tmp` + rename fallback Windows seperti `AppConfig::save`).
* `op = "load"`:
  * Muat checkpoint terakhir untuk workspace terkait.
  * **Live Drift Detection:** Bandingkan `git_status_short` & `git_head_short` saat disimpan dengan kondisi `git` saat ini. Jika ada perubahan baru sejak checkpoint disimpan, tampilkan blok `LIVE_DRIFT_SINCE_CHECKPOINT:` sehingga sesi Chat Baru langsung akurat tanpa harus re-scan repositori!
  * Kembalikan format teks ringkas (~350–500 token) yang langsung siap dipakai untuk melompat ke `next_steps[0]`.
* `op = "clear"`: Menghapus file checkpoint workspace terkait.
* **Bonus Integrasi MCP Resource:** Daftarkan `airlock://workspace/checkpoint` pada `src-tauri/src/mcp_resources.rs` sehingga klien MCP yang mendukung resources juga dapat membacanya langsung.

---

## 5. Rencana Perubahan File (File-by-File Map)

| File | Jenis | Perubahan Inti |
| :--- | :---: | :--- |
| `src-tauri/src/checkpoints.rs` | **Baru** | `SessionCheckpoint`, atomic save/load/clear di `~/.smart-tauri-hub/checkpoints/`, server auto-enrichment (git + audit + running jobs), live drift check. |
| `src-tauri/src/config.rs` | Edit | Tambah 8 field `#[serde(default)]`: `webmcp_default_profile`, `webmcp_rpc_timeout_secs`, `webmcp_auto_promote_secs`, `webmcp_poll_wait_secs`, `webmcp_max_output_chars`, `webmcp_sse_keepalive`, `webmcp_allow_write_delegation`, `webmcp_expose_skills`. |
| `src-tauri/src/mcp_server/auth.rs` | Edit | Tambah `profile: Option<String>` di `TokenQuery`, enum `McpToolProfile`, `extract_tool_profile()`. |
| `src-tauri/src/mcp_server/mod.rs` | Edit | Izinkan header `x-airlock-profile` pada `CorsLayer`. |
| `src-tauri/src/mcp_server/http.rs` | Edit | Ekstrak `tool_profile`; hybrid parallel read-only JSON-RPC batch; optional SSE keepalive (`: keepalive\n\n`) untuk `POST /mcp` >3s. |
| `src-tauri/src/mcp_server/rpc.rs` | Edit | `initialize.instructions` dinamis; `tools/list` & `server/discover` sadar `McpToolProfile`; buang `spawn_subagent` dari WebMCP; teruskan `Arc<AppState>` ke `tools/call`; timeout dinamis `webmcp_rpc_timeout_secs` + grace HITL `+70s`. |
| `src-tauri/src/tools/schemas.rs` | Edit | Tambah `mcp_tools_list_for_profile()`, definisi compact `run_command`, `lsp_query`, `session_checkpoint`, `delegate_local_agent`, serta perluasan schema `batch_inspect` & `multi_surgical_edit`; update `is_known_builtin_tool` & `is_tool_read_only`. |
| `src-tauri/src/tools/mod.rs` | Edit | Routing 4 tool baru (`run_command`, `lsp_query`, `session_checkpoint`, `delegate_local_agent`); post-processor `compress_for_webmcp_budget()` khusus `source == "WebMCP"`. |
| `src-tauri/src/tools/fs_tools.rs` | Edit | Perluas `exec_batch_inspect` (`find_patterns`, `read_slices`, `symbol_reads`); perluas `exec_multi_surgical_edit` (`post_check_lsp`, `verify_command`/`verify_commands`, `auto_promote_secs`); gunakan `webmcp_auto_promote_secs` dari config. |
| `src-tauri/src/tools/shell.rs` | Edit | Gunakan `webmcp_auto_promote_secs` dari config; tambah handler `exec_run_command_unified` (`action: run/start/poll/stop` + `commands[]` fail-fast pipeline). |
| `src-tauri/src/jobs.rs` | Edit | Tambah `start_pipeline_job_with_handle` untuk eksekusi berurutan fail-fast `Vec<String>` dalam 1 `job_id` terkelola. |
| `src-tauri/src/lsp_bridge.rs` | Edit | Tambah `quick_diagnostics_for_files` (warm-only fast-path ≤2,5s) dan `exec_lsp_query` wrapper. |
| `src-tauri/src/agent_manager.rs` | Edit | Tambah `DelegatedRunResult`, `status_tx`/`status_rx` watch channel pada `AgentRunHandle`, dan `poll_delegated_run(run_id, wait_secs, pinned_ws)`. |
| `src-tauri/src/local_agent.rs` | Edit | Tambah `start_delegated_webmcp_run` (strip `delegate_local_agent` anti-rekursi, hormati guardrail & `webmcp_allow_write_delegation`, bangun laporan akhir terkompresi). |
| `src-tauri/src/mcp_resources.rs` | Edit | Tambah resource `airlock://workspace/checkpoint`. |
| `src-tauri/src/state.rs` & `main.rs` | Edit | Ekspos setting WebMCP baru di `DashboardSnapshot` & IPC command `update_webmcp_settings`. |
| `src/presets.ts`, `src/types.ts`, `src/views/bridge.ts`, `index.html` | Edit | Update URL preset Gemini Web (`?profile=compact`), tambah panel pengaturan ringkas **"WebMCP Optimization"** di tab MCP Bridge (profile default, output budget, toggle write delegation, toggle SSE keepalive) dengan Vanilla TS XSS-safe. |

---

## 6. Strategi Implementasi Bertahap (3 Fase Eksekusi)

Kerjakan berurutan dengan commit/verifikasi lulus di akhir setiap fase agar risiko regresi mendekati nol:

### Fase 1 — Quick Win Fondasi (Pilar 1 + 3.1 + 3.4 + 5.1) — *Estimasi: 0,5–1 hari*
1. Tambah config WebMCP di `config.rs` (`#[serde(default)]`).
2. Implementasikan `McpToolProfile` (`auth.rs`, `mod.rs`, `http.rs`, `rpc.rs`).
3. Tambahkan `mcp_tools_list_for_profile` (`schemas.rs`) & pastikan `spawn_subagent` tidak pernah dipaparkan di `WebMCP`.
4. Tambahkan `initialize.instructions` dinamis (`rpc.rs`).
5. Naikkan default timing (`32s` RPC, `18s` auto-promote, `22s` long-poll) dan pasang `compress_for_webmcp_budget` (`6.000` chars) di `tools/mod.rs`.
6. Update preset `"gemini-web"` di `src/presets.ts`.
* **Gate Verifikasi Fase 1:** `cargo test` + `npm run build` lulus; `tools/list` pada `/mcp?profile=compact` mengembalikan ≤10 tool tanpa `spawn_subagent`; `initialize` menyertakan `instructions`.

### Fase 2 — Compound Macro-Tools, Parallel Batch & Checkpoint (Pilar 2 + 3.2 + 3.3 + 5.2) — *Estimasi: 1 hari*
1. Perluas `exec_batch_inspect` (`find_patterns`, `read_slices`, `symbol_reads`).
2. Implementasikan `lsp_bridge::quick_diagnostics_for_files` (warm-only ≤2,5s) & `exec_lsp_query`.
3. Perluas `exec_multi_surgical_edit` dengan `post_check_lsp` + `verify_command(s)` + `start_pipeline_job_with_handle` (`jobs.rs`).
4. Implementasikan `exec_run_command_unified` (`run_command`).
5. Implementasikan hybrid parallel read-only JSON-RPC batch & optional SSE keepalive di `mcp_server/http.rs`.
6. Buat `src-tauri/src/checkpoints.rs` + tool `session_checkpoint` + resource `airlock://workspace/checkpoint`.
* **Gate Verifikasi Fase 2:** Uji `multi_surgical_edit` dengan `verify_commands: ["echo step1", "echo step2"]` selesai dalam 1 call; uji `session_checkpoint(save → load)` mendeteksi drift file baru; uji batch JSON-RPC read-only berjalan paralel.

### Fase 3 — Web Brain, Local Executor & UI Controls (Pilar 4 + UI) — *Estimasi: 1 hari*
1. Upgrade `AgentRunManager` (`status_tx/rx`, `DelegatedRunResult`, `poll_delegated_run`).
2. Implementasikan `local_agent::start_delegated_webmcp_run` dan routing tool `delegate_local_agent` dengan `Arc<AppState>`.
3. Tambahkan kontrol UI **WebMCP Optimization** di tab MCP Bridge (`index.html`, `src/types.ts`, `src/views/bridge.ts`, `src-tauri/src/main.rs`).
* **Gate Verifikasi Fase 3:** Uji delegasi `read_only` & penolakan `execute` saat `webmcp_allow_write_delegation == false` atau workspace `read_only`; uji anti-rekursi; uji `cargo test` & `npm run build` bersih dengan 0 compiler warnings.

---

## 7. Pitfalls, Edge Cases & Mitigasi Wajib

| Edge Case / Risiko | Mengapa Berbahaya | Mitigasi Spesifik |
| :--- | :--- | :--- |
| **Cold-start LSP di dalam `multi_surgical_edit`** | Jika `rust-analyzer` belum jalan, spawn + indexing bisa 20–40 detik dan memutus panggilan edit. | `quick_diagnostics_for_files` **hanya** query LSP server yang sudah ada di map aktif dengan timeout `2,5s`. Jika cold, skip instan tanpa error. |
| **Bypass Guardrail/HITL pada `multi_surgical_edit` + `verify_command`** | `multi_surgical_edit` adalah edit file, tetapi `verify_command` menjalankan shell. Jika `command_execution_policy == "allowlist"` atau `"hitl"`, perintah liar bisa lolos jika hanya mengecek gate file. | Validasi `validate_command_with_policy` pada **semua** `verify_command(s)` di Phase 1 (pre-flight, sebelum menulis file apa pun), dan jalankan `enforce_guardrail_gate_for_workspace` dengan virtual tool `"run_command_compressed"` bila `verify_command` diisi. |
| **Klien WebMCP menolak `text/event-stream` pada `POST /mcp`** | Beberapa klien MCP web mengharapkan respons `application/json` murni untuk `POST`. | Jadikan `webmcp_sse_keepalive` default **`false`** (opt-in) dan hanya aktif jika header `Accept` klien secara eksplisit mencantumkan `text/event-stream` **dan** durasi call >3s. |
| **Pemotongan Output Budget merusak `JOB_ID` / `RUN_ID`** | Jika pemotong karakter memotong bagian atas atau bawah secara buta, model kehilangan `JOB_ID` dan mengulang perintah dari awal. | `compress_for_webmcp_budget` wajib mengekstrak & mempertahankan semua baris kontrol terstruktur (`JOB_ID:`, `RUN_ID:`, `GROUP_ID:`, `EXIT CODE:`, `STATUS:`, `NEXT_FROM_LINE:`, `STALL_WARNING:`) sebelum memotong body log. |
| **Local Agent terhenti pada HITL saat didelegasikan WebMCP** | Jika `delegate_local_agent(mode="execute")` mengenai popup HITL di mode `safe_edit`, menunggu diam di HTTP akan memutus koneksi tunnel. | `status_rx` langsung memancarkan `AgentRunStatus::AwaitingApproval`; `poll` langsung bangun dan mengembalikan status `AWAITING_APPROVAL` agar Gemini Web meminta user klik *Approve* di desktop Airlock. |
| **Self-lock Cargo saat uji coba di `D:\tauri-mcp-hub`** | `npm run tauri dev` menahan lock pada `src-tauri/target/debug/.cargo-lock`. | Tetap gunakan binary release saat menguji repo sendiri atau arahkan `cargo test --target-dir <temp>` (lihat Studi Real-World bagian 3.1). |

---

## 8. Rencana Automated Testing (Wajib Ditambahkan)

Tambahkan unit & integration tests di `src-tauri/src/mcp_server/tests.rs`, `src-tauri/src/tools/tests.rs`, `src-tauri/src/checkpoints.rs`, dan `src-tauri/src/local_agent.rs`:

1. `test_webmcp_compact_profile_hides_granular_and_dead_tools`:
   * Verifikasi `POST /mcp?profile=compact` (`tools/list`) mengembalikan ≤10 tool, tidak mengandung `spawn_subagent` / `upsert_artifact` / `request_plan_approval`, sedangkan panggilan langsung `tools/call` untuk tool lama (`read_file_range`, `surgical_edit`) tetap sukses (backward compatible).
2. `test_initialize_returns_profile_aware_instructions`:
   * Verifikasi respons `initialize` mengandung field `result.instructions` yang tidak kosong dan menyertakan nama workspace aktif.
3. `test_webmcp_output_budget_preserves_control_headers_and_errors`:
   * Uji output 20.000 karakter berisi `JOB_ID: job-123` + baris `error[E0308]` di tengah; pastikan hasil ≤6.000 karakter dan `JOB_ID` serta baris error tetap utuh.
4. `test_http_batch_executes_read_only_in_parallel_and_mutative_in_order`:
   * Kirim batch JSON-RPC berisi 3 read-only calls + 2 mutative calls; pastikan urutan array respons persis sesuai input dan mutasi dieksekusi berurutan.
5. `test_batch_inspect_supports_read_slices_and_symbol_reads`:
   * Verifikasi 1 panggilan `batch_inspect` dengan `include_overview + read_slices + symbol_reads + find_patterns` mengembalikan semua seksi dalam 1 turn.
6. `test_multi_surgical_edit_preflight_blocks_disallowed_verify_command`:
   * Pastikan jika `verify_command` ditolak oleh `allowlist` policy, **0 file ditulis ke disk** (atomicity terjaga).
7. `test_run_command_pipeline_stops_on_first_failure`:
   * Jalankan `commands: ["echo ok1", "exit 1", "echo should_not_run"]`; pastikan step ke-3 tidak pernah tereksekusi dan output memuat `[STEP 2/3]` exit code `1`.
8. `test_session_checkpoint_save_load_and_drift_detection`:
   * Simpan checkpoint di tempdir git repo, ubah 1 file, panggil `load`, pastikan output menyertakan `LIVE_DRIFT_SINCE_CHECKPOINT` dan file tersimpan di direktori checkpoint terpisah (bukan di dalam workspace).
9. `test_delegate_local_agent_enforces_write_opt_in_and_workspace_pin`:
   * Pastikan `mode="execute"` ditolak saat `webmcp_allow_write_delegation == false` atau saat `ws.effective_guardrail == "read_only"`; pastikan `poll`/`stop` lintas `pinned_ws` ditolak; pastikan `delegate_local_agent` tidak ada di tool list internal Local Agent.

### Verifikasi Akhir Wajib (Windows GNU Toolchain)

```powershell
Set-Location D:\tauri-mcp-hub\src-tauri
cargo test
Set-Location D:\tauri-mcp-hub
npm run build
```

Target: **0 errors, 0 compiler warnings, seluruh test lama (`76`) + 9 test baru Blueprint 06 lulus (`>= 85 passed; 0 failed`)**.
