# Blueprint 02 — Gemini AI Optimization, Anti-Timeout & Async Handoff

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent (Autonomous / Co-pilot Handoff) |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | Ready for Implementation |
| **Prioritas** | P1 — Reliabilitas & Kecepatan Eksekusi Agent (WebMCP & Local Agent) |
| **Dependensi Blueprint** | Independen (kompatibel penuh dengan Blueprint 01 Multi-Workspace) |

---

## 1. Handoff Brief untuk Agent Antigravity

Dokumen ini merancang solusi menyeluruh atas masalah **turn Gemini terlalu lama hingga dibatalkan/hang** di dua jalur eksekusi Airlock:
1. **Jalur A — WebMCP / Remote MCP Client (Gemini Web, Claude, ChatGPT):** Turn gagal atau dibatalkan oleh klien karena perintah shell/verify memakan >22–24 detik atau karena terlalu banyak round-trip tool kecil yang sekuensial.
2. **Jalur B — Local Coding Agent (`src-tauri/src/local_agent.rs`):** Turn Gemini API lambat/macet karena tool calls dijalankan sekuensial, tidak ada stream idle watchdog, tidak ada checkpoint/resume saat cancel atau timeout, batas kaku 8 step, serta belum memanfaatkan fitur native Gemini (`thinkingConfig`, deterministic prefix caching / explicit context caching, shared HTTP client).

### Invarian Arsitektur Airlock yang WAJIB Dipatuhi
1. **Preservasi `thoughtSignature` Gemini 2.5 / 3:** Fungsi `append_part_preserving_signatures` (`local_agent.rs:69-88`) menjaga field `thoughtSignature` pada model parts. Setiap perubahan pada `history`, checkpoint, atau pruning **dilarang keras** menghilangkan `thoughtSignature` atau memisahkan pasangan `model(functionCall)` dengan `user(functionResponse)`.
2. **Jangan Jalankan Mutative Tools Secara Paralel:** Hanya tool dengan `is_tool_read_only == true` (dan tidak memicu HITL) yang boleh dieksekusi paralel. Semua tool mutatif (`surgical_edit`, `write_file_jailed`, `file_ops`, `run_command_compressed`, `patch_and_verify`, `git_ops` add/commit) **wajib sekuensial** sesuai urutan dari model.
3. **Proses Cleanup Windows (`ChildTreeGuard` / `taskkill /T /F`):** Proses yang dipromosikan ke `JobManager` **tidak boleh** di-kill saat timeout promosi, tetapi wajib tetap terikat ke `JobManager` agar bisa dihentikan bersih lewat `stop_command_job` atau saat aplikasi quit.
4. **Zero UI Framework & XSS-Safe DOM:** Seluruh tambahan UI (tombol Resume Turn, dropdown Thinking Level, konfigurasi max steps/timeout) menggunakan Vanilla TypeScript (`createElement`/`textContent`) pada Obsidian Studio design system.

---

## 2. Diagnosis Akar Masalah Saat Ini (As-Is)

| # | Jalur | Lokasi Kode | Akar Masalah yang Menyebabkan Turn Lama / Dibatalkan |
| :--- | :--- | :--- | :--- |
| 1 | WebMCP | `mcp_server.rs:418`, `tools.rs:1490` | `tools/call` punya hard timeout 24s; `run_command_compressed` timeout di 22s lalu **membunuh proses** dan mengembalikan error. User/Gemini kehilangan progres build/test yang sudah berjalan 20 detik! |
| 2 | WebMCP | `tools.rs:1331` | `patch_and_verify` menjalankan `verify_command` sinkron (timeout 20s). Jika `cargo test` / `npm run build` butuh 25s, seluruh respons gagal padahal file sudah teredit! |
| 3 | WebMCP & Local | `tools.rs:41-341` | Belum ada compound/macro tool untuk inspeksi multi-file (`batch_inspect`) atau edit multi-file atomik (`multi_surgical_edit`), sehingga LLM boros 4–6 round-trip. |
| 4 | Local Agent | `local_agent.rs:621` | `for fc in step_acc.function_calls` mengeksekusi semua tool call **satu per satu** meskipun Gemini mengembalikan beberapa read-only calls sekaligus dalam 1 turn. |
| 5 | Local Agent | `local_agent.rs:566` | `resp.chunk().await` **tidak memiliki idle timeout**. Jika TCP/SSE stall, agent hang selamanya sampai user klik Stop. |
| 6 | Local Agent | `local_agent.rs:466, 684-694` | Batas step dikunci keras di `0..8`. Lebih buruk lagi, saat turn dibatalkan (`was_cancelled == true`) atau error di step ke-6, **seluruh `history` step 1–5 dibuang** — user harus mulai dari nol! |
| 7 | Local Agent | `local_agent.rs:437, 474` | `reqwest::Client::new()` dibuat baru setiap turn (TCP+TLS handshake ulang); `thinkingConfig` belum dikonfigurasi sehingga model thinking sering berpikir berlebihan (30–60s) untuk tugas sederhana. |
| 8 | Local Agent | `local_agent.rs:447`, `providers.rs` | Belum ada kontrol *turn budget* (soft warning & auto-checkpoint) dan belum memanfaatkan *Context Caching* untuk prefix system prompt + project rules + deklarasi tool. |

---

## 3. Arsitektur Target (To-Be)

### 3.1 Diagram Alur Auto-Promote Command (Jalur WebMCP)

```text
  WebMCP: run_command_compressed("cargo test")  /  patch_and_verify(...)
                              |
                              v
      +-----------------------------------------------+
      |  jobs::spawn_managed_job(state, root, cmd)    |
      |  (returns job_id + oneshot/Notify completion) |
      +-----------------------------------------------+
                              |
          await completion with timeout(promote_secs = 9s for WebMCP)
                   /                                \
        Selesai <= 9s                           Masih berjalan > 9s
             |                                           |
             v                                           v
  Ambil output terkompresi                 JANGAN KILL PROSES!
  Tandai job internal selesai              Biarkan hidup di JobManager
  Return:                                  Emit event "job-updated" ke UI
    "EXIT CODE: 0\n<compressed>"           Return (isError: false!):
                                             "AUTO_PROMOTED_TO_BACKGROUND_JOB
                                              JOB_ID: job-xxxxxxxx
                                              STATUS: running (9s elapsed)
                                              PARTIAL OUTPUT (last 20 lines): ...
                                              NEXT ACTION: Call poll_command_job(job_id)"
```

**Mengapa `isError: false` saat auto-promote?**
Klien WebMCP (termasuk Gemini) sering menganggap `isError: true` sebagai kegagalan permanen lalu menghentikan rencana atau mengulang perintah yang sama dari awal. Mengembalikan `isError: false` dengan status terstruktur `AUTO_PROMOTED_TO_BACKGROUND_JOB` membuat model langsung memanggil `poll_command_job` pada giliran berikutnya tanpa panik!

### 3.2 Diagram Alur Local Agent Gemini yang Tahan Timeout & Resumable

```text
  User Prompt / Resume Checkpoint
               |
               v
  [Load/Prepare History + Deterministic System Prefix + ThinkingConfig]
               |
               v
  ====== Step Loop (0..max_steps, default 16, configurable 4..30) ======
  |                                                                    |
  |  1. Cek Turn Time Budget (soft_budget_secs, mis. 150s):            |
  |     - Jika melewati soft budget -> inject pesan sistem internal    |
  |       agar model merangkum progres atau simpan TurnCheckpoint.     |
  |                                                                    |
  |  2. HTTP POST (shared AppState.http_client) + TTFB Timeout (45s)   |
  |                                                                    |
  |  3. SSE Stream Loop + Per-Chunk Idle Watchdog (35s):               |
  |     - Jika idle > 35s -> trigger retry 1x atau simpan Checkpoint.  |
  |                                                                    |
  |  4. Eksekusi Function Calls (Hybrid Parallel + Sequential):        |
  |     - Kelompokkan contiguous read-only calls -> tokio::JoinSet     |
  |     - Jalankan mutative calls secara sekuensial ketat              |
  |     - Gabungkan functionResponse sesuai urutan asli                |
  |                                                                    |
  |  5. Setelah setiap step tool selesai -> Commit Checkpoint di RAM!  |
  ======================================================================
               |
       Cancel / Timeout / Max Steps tercapai?
         /                             \
       Ya                               Tidak (Selesai Normal)
       |                                        |
       v                                        v
  Commit partial history                  Clear pending checkpoint
  (hingga pasangan tool terakhir          Prune history (max 40)
   yang lengkap!) + set                   Return final reply
  TurnCheckpoint { can_resume: true }
```

---

## 4. Desain Detail Setiap Komponen

### 4.1 Auto-Promote Slow Command ke Background Job (`jobs.rs` & `tools.rs`)

1. **Tambahkan Completion Notifier pada `jobs.rs`:**
   * Tambahkan `pub done_notify: Arc<tokio::sync::Notify>` (atau `tokio::sync::watch::Receiver<bool>`) pada struct `ManagedJob`.
   * Buat fungsi baru `pub async fn start_job_with_handle(state, root, command, label, timeout_secs, workspace_alias) -> Result<(JobSummary, tokio::sync::watch::Receiver<bool>), String>`.
   * Saat child process selesai (atau killed/timed_out) di background task `jobs.rs`, panggil `let _ = done_tx.send(true);`.
   * Pertahankan `start_job(...)` sebagai wrapper yang mengabaikan receiver agar kode lama tetap kompatibel.
   * Tambahkan helper `pub async fn get_job_snapshot_output(state: &AppState, job_id: &str, max_lines: usize) -> Option<(JobStatus, Option<i32>, u64, String, usize)>` untuk mengambil status, exit code, elapsed secs, dan output terkompresi saat ini.

2. **Integrasikan ke `exec_run_command` (`tools.rs:1478`):**
   * Tambahkan parameter opsional `auto_promote_secs` (default: `9` untuk `WebMCP`, `None`/disabled untuk `LocalAgent` & `Simulator` kecuali diisi eksplisit — atau default `25` untuk `LocalAgent`).
   * Alur di `exec_run_command`:
     1. `validate_command(command)?;`
     2. Jika `source == "WebMCP"` (atau `auto_promote_secs` aktif):
        * Mulai via `jobs::start_job_with_handle(state, root, command, Some("auto-promoted".into()), 300, ws_alias)`.
        * `tokio::time::timeout(Duration::from_secs(promote_secs), done_rx.wait_for(|&done| done)).await`.
        * **Jika selesai dalam batas waktu:** ambil seluruh stdout/stderr job, kompres via `compress_output`, catat audit, hapus/tandai job selesai (opsional: biarkan 1 entri singkat atau hapus agar tidak memenuhi panel Jobs), kembalikan `EXIT CODE: {code}\n{compressed}`.
        * **Jika masih berjalan setelah `promote_secs`:** ambil 20 baris log terakhir, catat audit (`summary: "$ cmd (auto-promoted -> job-xxxx)"`), dan kembalikan teks terstruktur:
          ```text
          AUTO_PROMOTED_TO_BACKGROUND_JOB
          JOB_ID: job-a1b2c3d4
          COMMAND: cargo test
          STATUS: running (elapsed 9s)
          RECENT OUTPUT:
          <20 baris terakhir>

          NEXT ACTION: Perintah masih berjalan di background agar tidak timeout. Panggil `poll_command_job` dengan `job_id: "job-a1b2c3d4"` untuk memantau progres hingga selesai.
          ```
     3. Di `exec_patch_and_verify` (`tools.rs:1306`):
        * Terapkan `apply_surgical_edits_to_file` dan simpan snapshot audit **sebelum** menjalankan `verify_command` (perbaikan penting: saat ini `record_audit` baru dipanggil *setelah* `run_shell_raw` selesai di baris 1338 — jika `verify_command` timeout, file sudah berubah di disk tetapi entri rollback tidak tercatat!).
        * Jalankan `verify_command` melalui mekanisme auto-promote yang sama: jika lewat 9s pada WebMCP, kembalikan `PATCH DIFF` + `AUTO_PROMOTED_VERIFY_JOB` dengan `JOB_ID`.

### 4.2 Compound / Macro Tools Penghemat Round-Trip (`tools.rs` & `tools_extra.rs`)

Tambahkan 2 built-in tool baru (dan perbarui `is_known_builtin_tool`, `is_tool_read_only`, `execute_tool`, serta `builtin_tools_count`):

1. **`batch_inspect` (Read-Only):**
   * **Tujuan:** Menggabungkan 1x `project_overview` (opsional) + beberapa `code_outline` + beberapa `smart_search` ringan dalam **1 round-trip**.
   * **Input Schema:**
     ```json
     {
       "type": "object",
       "properties": {
         "workspace": { "type": "string" },
         "include_overview": { "type": "boolean", "description": "Include project tree + git status (default false)" },
         "overview_depth": { "type": "integer", "description": "Max depth if include_overview=true (default 2, max 4)" },
         "outline_paths": { "type": "array", "items": { "type": "string" }, "description": "Max 8 relative file paths to extract symbols from" },
         "searches": {
           "type": "array",
           "description": "Max 4 quick search queries",
           "items": {
             "type": "object",
             "properties": {
               "query": { "type": "string" },
               "path": { "type": "string" },
               "max_results": { "type": "integer" }
             },
             "required": ["query"]
           }
         }
       }
     }
     ```
   * **Eksekusi Internal:** Jalankan semua sub-operasi secara paralel via `tokio::join!` / `JoinSet`, enforce `resolve_jailed_path` & `check_sensitive_guard` untuk setiap path, lalu gabungkan output dengan batas total 16.000 karakter.

2. **`multi_surgical_edit` (Mutative, 1 Turn Group Rollback):**
   * **Tujuan:** Mengedit 2–8 file berbeda dalam **1 round-trip** dengan jaminan validasi pre-flight: semua `search` block pada semua file diverifikasi *sebelum* satu pun file ditulis ke disk (*all-or-nothing pre-check*).
   * **Input Schema:**
     ```json
     {
       "type": "object",
       "properties": {
         "workspace": { "type": "string" },
         "files": {
           "type": "array",
           "description": "Array of { path, edits } (max 8 files)",
           "items": {
             "type": "object",
             "properties": {
               "path": { "type": "string" },
               "edits": {
                 "type": "array",
                 "items": {
                   "type": "object",
                   "properties": {
                     "search": { "type": "string" },
                     "replace": { "type": "string" },
                     "replace_all": { "type": "boolean" }
                   },
                   "required": ["search", "replace"]
                 }
               }
             },
             "required": ["path", "edits"]
           }
         }
       },
       "required": ["files"]
     }
     ```
   * **Rollback Safety:** Buat satu `group_id` otomatis (atau pakai `current_turn_group_id`) untuk seluruh file yang dimodifikasi, sehingga 1 klik **Revert Turn Group** mengembalikan seluruh batch edit sekaligus.

### 4.3 Parallel Read-Only Tool Execution di Local Agent (`local_agent.rs` & `providers.rs`)

Saat model mengembalikan `function_calls` berisi `[A, B, C, D]`:
* Klasifikasikan tiap call apakah aman diparalelkan:
  ```rust
  async fn is_safe_parallel_call(state: &AppState, name: &str, args: &Value) -> bool {
      if name.contains("__") {
          let cfgs = state.config.read().await.downstream_mcps.clone();
          state.mcp_hub.is_tool_read_only(&cfgs, name).await
      } else {
          crate::tools::is_tool_read_only(name, args)
      }
  }
  ```
* Kelompokkan panggilan berurutan (*contiguous runs*):
  * Contoh: `[read_file_range(1), smart_search(2), code_outline(3), surgical_edit(4), read_file_range(5)]`
  * Run 1 (Parallel, maks koncurrency 6): `[1, 2, 3]` → jalankan bersamaan dengan `tokio::task::JoinSet`, tunggu dengan `tokio::select!` terhadap `state.agent_cancel.notified()`, lalu urutkan kembali hasilnya berdasarkan indeks asli `(0, 1, 2)`.
  * Run 2 (Sequential): `[4]` → jalankan sendiri.
  * Run 3 (Parallel): `[5]` → jalankan.
* Terapkan helper eksekusi hybrid ini pada **ketiga provider**: `local_agent.rs` (Gemini), `providers.rs` (OpenAI-Compat), dan `providers.rs` (Anthropic).

### 4.4 Watchdog Streaming, Turn Budget & Checkpoint/Resume (`local_agent.rs` & `state.rs`)

1. **Konfigurasi Baru di `AppConfig` (`src-tauri/src/config.rs`):**
   ```rust
   #[serde(default = "default_gemini_thinking_mode")]
   pub gemini_thinking_mode: String,          // "auto" | "off" | "low" | "medium" | "high"
   #[serde(default = "default_agent_max_steps")]
   pub agent_max_steps: usize,                // default: 16 (clamp 4..30)
   #[serde(default = "default_agent_turn_timeout_secs")]
   pub agent_turn_timeout_secs: u64,          // default: 180 (clamp 30..600)
   #[serde(default = "default_stream_idle_timeout_secs")]
   pub stream_idle_timeout_secs: u64,         // default: 35 (clamp 10..120)
   ```

2. **Tiga Lapis Watchdog di `run_gemini_turn` (dan provider lain):**
   * **Layer 1 — TTFB Timeout (45s):** Bungkus `client.post(&url)...send()` dengan `tokio::time::timeout(Duration::from_secs(45), ...)`. Jika timeout, perlakukan seperti retryable error (maks 2x retry dengan backoff singkat) sambil emit status `"Menunggu respons awal Gemini (retry 1/2)..."`.
   * **Layer 2 — SSE Stream Idle Watchdog (`stream_idle_timeout_secs`, default 35s):**
     Bungkus `resp.chunk()` di dalam `tokio::select!` dengan `tokio::time::sleep(Duration::from_secs(idle_secs))`:
     ```rust
     tokio::select! {
         _ = state.agent_cancel.notified() => {
             cancel_reason = Some(CancelReason::UserCancelled);
             break;
         }
         _ = tokio::time::sleep(Duration::from_secs(idle_timeout_secs)) => {
             cancel_reason = Some(CancelReason::StreamStalled);
             break;
         }
         chunk_res = resp.chunk() => { ... }
     }
     ```
   * **Layer 3 — Total Turn Budget (`agent_turn_timeout_secs`, default 180s):**
     Catat `let turn_started = Instant::now();` di awal turn.
     * Ketika `elapsed >= soft_budget` (mis. 80% dari `agent_turn_timeout_secs`, atau 144s) dan masih ada tool call berikutnya: **suntikkan catatan singkat ke `functionResponse` terakhir**: `"[AIRLOCK BUDGET NOTICE]: Sisa waktu turn tinggal ~30 detik. Selesaikan analisis atau berikan ringkasan progres & langkah lanjutan agar turn tidak terputus."`
     * Ketika `elapsed >= hard_budget` (100%): hentikan loop secara bersih dengan `CancelReason::TurnTimeout`, simpan checkpoint!

3. **Mekanisme Checkpoint & Resume (`TurnCheckpoint`):**
   ```rust
   #[derive(Debug, Clone, Serialize)]
   pub struct TurnCheckpointInfo {
       pub can_resume: bool,
       pub reason: String,           // "user_cancelled" | "stream_stalled" | "turn_timeout" | "max_steps"
       pub completed_steps: usize,
       pub last_tool: Option<String>,
   }
   ```
   * **Aturan Emas Penyimpanan History saat Cancel/Timeout:**
     * Pertahankan variabel `last_clean_history: Vec<Value>` yang diperbarui **hanya** pada titik aman:
       1. Tepat setelah user prompt awal masuk (sebelum step 0), dan
       2. Tepat setelah pasangan lengkap `model(functionCalls)` + `user(functionResponses)` berhasil diselesaikan!
     * Jika cancel/timeout terjadi di tengah streaming teks atau di tengah eksekusi tool: simpan `last_clean_history` ke `state.gemini_config.write().await.history` (bukan membuangnya seperti saat ini!).
     * Set `state.pending_checkpoint` ke `Some(TurnCheckpointInfo { ... })`.
     * Tambahkan field `pub checkpoint: Option<TurnCheckpointInfo>` di `AgentTurnResponse` dan `DashboardSnapshot`.
   * **IPC Command Baru `resume_local_agent_turn`:**
     Memanggil `run_agent_turn` dengan prompt lanjutan internal: `"Lanjutkan tugas sebelumnya tepat dari hasil tool terakhir tanpa mengulang langkah yang sudah selesai."` (dan menghapus `pending_checkpoint`).

### 4.5 Optimasi Native Gemini API (`local_agent.rs` & `state.rs`)

1. **Shared `reqwest::Client` di `AppState`:**
   * Tambahkan `pub http_client: reqwest::Client` pada `AppState` (dibangun sekali dengan `pool_idle_timeout(Duration::from_secs(90))` dan `tcp_keepalive(Duration::from_secs(30))`).
   * Hapus `reqwest::Client::new()` dari `local_agent.rs:437`, `providers.rs:513, 1008`, dan `mcp_hub.rs:334, 590`.
2. **Model-Aware `thinkingConfig` (`local_agent.rs`):**
   **PENTING — Perbedaan Skema API antar Generasi Model Gemini:**
   * Model `gemini-2.5-*` (Flash / Pro / Flash-Lite): gunakan `thinkingConfig.thinkingBudget` (integer):
     * `"off"` → `0` *(kecuali `gemini-2.5-pro` yang minimumnya `128`, jangan kirim `0` ke Pro agar tidak HTTP 400!)*
     * `"low"` → `1024`
     * `"medium"` → `4096`
     * `"high"` → `12288`
     * `"auto"` → `-1` (dynamic thinking)
     * Sertakan `"includeThoughts": true` agar panel Thought di UI tetap menerima streaming pemikiran.
   * Model `gemini-3*` (`gemini-3-flash-preview`, `gemini-3-pro*`): gunakan `thinkingConfig.thinkingLevel`:
     * `"low"` / `"medium"` / `"high"` (untuk `"off"` pada flash gunakan `"minimal"`, untuk `"auto"` omit field agar memakai default model).
   * Model lama (`gemini-2.0-*`, `gemini-1.5-*`): **jangan kirim `thinkingConfig` sama sekali** (menghindari HTTP 400).
   * **Fallback Otomatis:** Jika Gemini mengembalikan HTTP `400` yang pesan error-nya menyebut `thinkingConfig` / `thinkingBudget` / `thinkingLevel`, otomatis retry 1x **tanpa** `thinkingConfig` agar tidak pernah memblokir model kustom baru!
3. **Deterministic Prefix Caching & Optional Explicit Context Caching:**
   * **Implicit Caching (Gratis & Otomatis di Gemini 2.5/3):** Pastikan `systemInstruction` dan urutan array `functionDeclarations` **100% deterministik byte-for-byte** antar step dan antar turn (sortir `downstream_mcps` berdasarkan `namespace` dan nama tool secara alfabetis; jangan taruh timestamp dinamis di dalam `system_prompt`!).
   * **Explicit Context Caching (Opsional/Fase Lanjut):** Jika estimasi panjang `system_prompt + tools_decl` > 16.000 karakter (~4.000 token), siapkan modul kecil `gemini_cache.rs` yang membuat `POST /v1beta/cachedContents` dengan `ttl: "600s"` (kunci cache = SHA-256 dari `model + system_prompt + tools_json`), lalu kirim `"cachedContent": name` pada request. Jika API menolak (mis. kuota / model tidak mendukung / di bawah minimum token), fallback diam-diam ke mode uncached tanpa mengganggu user.

### 4.6 Perbaikan Kecil `mcp_server.rs` (Streaming & Timeout Selaras)

* Pada `mcp_server.rs:418`: timeout `tools/call` 24s tetap dipertahankan sebagai safety net paling luar (anti-hang), sementara `run_command_compressed` dan `patch_and_verify` melakukan auto-promote pada **9 detik** — sehingga masih ada margin **15 detik** sebelum batas 24s tersentuh!
* Pada `mcp_server.rs:418`: pastikan jika `tool_name` adalah `run_command_compressed` atau `patch_and_verify`, pesan error timeout juga mengingatkan tentang `start_command_job` / `poll_command_job`.

---

## 5. Spesifikasi UI / UX (`src/views/agent.ts`, `index.html`, `src/types.ts`)

1. **Kontrol Gemini Thinking & Turn Budget di Agent Drawer (`index.html` + `src/views/agent.ts`):**
   * Di dalam drawer konfigurasi agent (tampil saat provider = `gemini`), tambahkan:
     * `<select id="gemini-thinking-mode">`: `Auto (Dynamic)`, `Off / Minimal (Tercepat)`, `Low (1K)`, `Medium (4K)`, `High (12K)`.
     * Input numerik ringkas: `Max Steps` (`4–30`, default `16`) dan `Turn Budget (s)` (`30–600`, default `180`).
   * Tampilkan elapsed timer live saat turn aktif (`Step 3/16 · 14s · 2 parallel tools`) pada status bar stream card!
2. **Banner Checkpoint & Tombol "Resume Turn" (`src/views/agent.ts`):**
   * Jika `AgentTurnResponse.checkpoint?.can_resume === true` (karena cancel, stream stall, timeout, atau max steps), tampilkan banner aksi halus di bawah kartu jawaban terakhir:
     * Teks: `"Turn berhenti di step 6/16 (alasan: stream stalled). Progres tool tersimpan."`
     * Tombol: **`Resume Turn`** → memanggil `invoke("resume_local_agent_turn")`.
     * Tombol: **`Discard Checkpoint`** → menghapus banner.
3. **Live Tool Execution Indicator:**
   * Tambahkan event Tauri `agent-parallel-tools` (atau format jelas pada `agent-step`: `"Menjalankan 3 tool paralel: project_overview, smart_search, code_outline..."`) agar user langsung melihat akselerasi eksekusi.

---

## 6. Pitfalls & Gotchas Kritis untuk Antigravity

1. **Jangan Rusak `thoughtSignature` & Pasangan `functionCall`/`functionResponse`:**
   * Jika turn dibatalkan saat `step_acc.function_calls` sedang dieksekusi di item ke-2 dari 3, **JANGAN** push `model(functionCalls)` ke `history` karena responsnya belum lengkap. Gunakan `last_clean_history`! Jika sebuah `model(functionCall)` tidak diikuti `user(functionResponse)` yang cocok jumlah namanya, Gemini API akan mengembalikan HTTP `400 INVALID_ARGUMENT` pada turn berikutnya.
2. **Urutan `functionResponse` pada Eksekusi Paralel:**
   * Saat memakai `tokio::task::JoinSet`, task selesai tidak berurutan! Wajib menyertakan indeks `usize` pada setiap future (`async move { (idx, out) }`), kumpulkan dalam `Vec<(usize, ToolOutput)>`, lalu `sort_by_key(|(idx, _)| *idx)` sebelum membangun `function_response_parts`.
3. **Race Condition `agent_cancel` (`tokio::sync::Notify`):**
   * `Notify::notify_waiters()` hanya membangunkan waiter yang *sedang* menunggu, sedangkan `Notify::notify_one()` menyimpan 1 permit. Periksa `main.rs:430` (`cancel_local_agent`): pastikan saat `run_agent_turn` baru dimulai, permit sisa dari pembatalan sebelumnya tidak langsung membatalkan turn baru! **Solusi yang lebih bersih:** gunakan `tokio::sync::watch::channel(false)` (atau `CancellationToken` pattern berbasis `watch::Sender<bool>`) yang di-reset ke `false` setiap awal turn dan diset ke `true` saat `cancel_local_agent` dipanggil. Ini 100% bebas dari race condition permit lama!
4. **Auto-Promote & `patch_and_verify` Audit Ordering:**
   * Di `tools.rs:1326-1359`, edit file sudah ditulis ke disk pada baris 1327, tetapi `record_audit` baru terjadi di baris 1338 setelah `run_shell_raw`. Pindahkan pencatatan audit edit (termasuk `original_blob_hash`, `post_edit_hash`, `can_revert: true`) agar **pasti** tercatat meskipun `verify_command` timeout atau dipromosikan ke background job!
5. **`gemini-2.5-pro` Menolak `thinkingBudget: 0`:**
   * Jika user memilih `"off"` namun model mengandung `"pro"`, clamp `thinkingBudget` ke `128` (minimum Pro) atau omit field, dan selalu sediakan auto-retry 1x tanpa `thinkingConfig` bila server mengembalikan `400`.
6. **Non-Deterministic System Prompt Merusak Cache:**
   * Pastikan daftar tool dari `mcp_hub.list_aggregated_tools` diurutkan deterministik (`sort_by(|a, b| a["name"]...)`) sebelum dikirim ke Gemini agar implicit prompt caching selalu hit!

---

## 7. Roadmap Implementasi Bertahap

### Fase 1 — Anti-Timeout WebMCP & Compound Tools (Dampak Instan untuk Gemini Web)
- [ ] Tambahkan `done_notify` / `watch::Receiver<bool>` dan `start_job_with_handle` + `get_job_snapshot_output` di `src-tauri/src/jobs.rs`.
- [ ] Implementasikan Auto-Promote (9s default pada WebMCP) pada `exec_run_command` dan `exec_patch_and_verify` di `src-tauri/src/tools.rs`, serta perbaiki urutan pencatatan audit rollback `patch_and_verify`.
- [ ] Implementasikan 2 compound tools: `batch_inspect` dan `multi_surgical_edit` (dengan pre-flight validation all-or-nothing & shared `group_id`).

### Fase 2 — Akselerasi & Reliabilitas Local Agent (`local_agent.rs` & `providers.rs`)
- [ ] Tambahkan shared `http_client: reqwest::Client` di `AppState` dan hilangkan `Client::new()` berulang.
- [ ] Ganti `agent_cancel: Arc<Notify>` menjadi `watch` cancellation channel agar bebas permit race condition.
- [ ] Implementasikan hybrid parallel execution untuk read-only tool calls (dengan preservasi urutan respons) pada Gemini, OpenAI-Compat, dan Anthropic.
- [ ] Implementasikan 3-layer watchdog: TTFB timeout (45s), SSE Stream Idle timeout (35s), dan Total Turn Budget (soft warning di 80%, hard stop di 100%).
- [ ] Implementasikan `last_clean_history` + `TurnCheckpointInfo` + IPC command `resume_local_agent_turn`.

### Fase 3 — Optimasi Native Gemini API & UI
- [ ] Tambahkan `gemini_thinking_mode`, `agent_max_steps`, `agent_turn_timeout_secs`, dan `stream_idle_timeout_secs` ke `AppConfig`, `DashboardSnapshot`, dan `configure_agent_provider`.
- [ ] Implementasikan model-aware `thinkingConfig` builder + auto-fallback bila HTTP 400 + deterministic tool sorting untuk implicit prompt caching.
- [ ] Perbarui UI `index.html`, `src/types.ts`, `src/views/agent.ts` (kontrol thinking/budget, live step timer, banner & tombol **Resume Turn**).

---

## 8. Rencana Pengujian & Definition of Done

### Automated Tests Wajib (`cargo test`)
1. `jobs::tests::test_auto_promote_fast_command_returns_sync` (perintah <1s selesai sinkron dengan exit code & output).
2. `jobs::tests::test_auto_promote_slow_command_hands_off_to_job_manager` (perintah sleep >2s dengan `promote_secs = 1` mengembalikan `AUTO_PROMOTED_TO_BACKGROUND_JOB` + `job_id`, proses tetap hidup, dan `poll_command_job` berhasil membaca hingga `completed`).
3. `tools::tests::test_batch_inspect_and_multi_surgical_edit_atomic_preflight` (uji jika 1 dari 3 file gagal match `search`, 0 file termodifikasi; jika sukses, seluruh 3 file teredit dan bisa di-revert sekaligus via `revert_audit_group`).
4. `tools::tests::test_patch_and_verify_records_rollback_even_when_verify_promoted` (audit entry `can_revert: true` tercatat walau verify dipromosikan).
5. `local_agent::tests::test_parallel_read_only_preserves_order_and_keeps_mutative_sequential`.
6. `local_agent::tests::test_cancel_saves_clean_checkpoint_without_dangling_function_call` (verifikasi `history` tersimpan tanpa `functionCall` yatim dan `thoughtSignature` utuh).
7. `local_agent::tests::test_thinking_config_builder_per_model_family` (`gemini-2.5-flash`, `gemini-2.5-pro`, `gemini-3-flash-preview`, `gemini-2.0-flash`).

### Verifikasi Build & Runtime
```powershell
cd src-tauri
cargo test
cd ..
npm run build
```
**Definition of Done:**
* Perintah lambat (seperti `cargo test`) lewat WebMCP tidak lagi mati di detik ke-24, melainkan otomatis berlanjut di background job dan dapat dipantau lewat `poll_command_job`.
* Local Agent menjalankan beberapa read-only tools secara paralel, tidak pernah hang saat SSE stall, dan menyediakan tombol **Resume Turn** yang melanjutkan progres tanpa mengulang langkah sebelumnya.
* Seluruh `cargo test` dan `npm run build` lulus 100% bersih.
