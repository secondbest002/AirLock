# Blueprint 07 — WebMCP Session Harness & E2E Benchmark

| Atribut | Detail |
| :--- | :--- |
| **Target Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Target Executor** | Google Antigravity Agent |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Status** | **Selesai (Point A + Point C.2)** |
| **Prioritas** | **P0/P1 — Penguatan Reliabilitas Gemini WebMCP & Regresi Terukur** |
| **Dependensi Blueprint** | Blueprint 01–06 + perbaikan sinkronisasi Blueprint 06 (`93 passed; 0 failed`) |

---

## 1. Problem Statement & Tujuan Arsitektur

Blueprint 06 memperbaiki fondasi permukaan MCP (`compact` profile, makro-tools, timing `18s/22s/32s`, budget `<=6 KB`, `delegate_local_agent`, dan `session_checkpoint`). Namun dua celah struktural masih membatasi sesi panjang **Gemini WebMCP**:

1. **Server MCP Masih Stateless Antar-Turn (`Point A`):**
   - Setiap request `POST /mcp` diproses terpisah tanpa melacak konteks sesi (`mcp-session-id`).
   - Model browser rentan mengalami *read-loop* (membaca file yang sama berulang tanpa edit), *error-loop* (mengulang perintah/edit yang gagal dengan kesalahan identik), *context-rot* (melampaui ~12 turn / >45 KB tanpa memanggil `session_checkpoint(op="save")`), atau lupa mengisi `post_check_lsp: true` saat mengedit.
2. **Belum Ada E2E Benchmark & Regression Harness (`Point C.2`):**
   - Unit test eksisting menguji komponen secara terisolasi, belum mengukur KPI lintas-turn secara objektif (*round-trips*, *schema bytes*, *control header survival*, *harness steering*, dan *time-to-green*).

### Target Metrik Keberhasilan

| Metrik | Baseline Blueprint 06 | Target Blueprint 07 |
| :--- | :---: | :---: |
| Deteksi *read-loop* & *error-loop* di sesi WebMCP | Tidak ada | **Otomatis pada pengulangan ke-2** via `[HARNESS_HINT:]` |
| Pengingat `session_checkpoint` saat sesi panjang | Statis di `initialize.instructions` | **Dinamis** tiap `>=12` tool call atau `>=45 KB` output tanpa save |
| Warm LSP check saat edit WebMCP (`webmcp_auto_verify_on_edit`) | Bergantung argumen model | **Otomatis (opt-in)** di server (`<=2.5s` warm-only, zero cold-start) |
| Uji regresi E2E multi-turn WebMCP | Manual | **Otomatis (`cargo test` + `--benchmark-webmcp`)** dengan laporan KPI |

---

## 2. Global Engineering Invariants (Wajib Dipatuhi)

1. **Zero Fragile Browser Scraping / DOM Coupling:** Server-Side Harness (`Point A`) beroperasi 100% di protokol MCP standar tanpa bergantung pada ekstensi browser.
2. **Backward Compatibility Config & MCP:** Semua field baru di `AppConfig` wajib memakai `#[serde(default = "...")]`. Helper `execute_tool`, `execute_tool_with_context`, dan `execute_tool_with_run_context` tetap kompatibel.
3. **Budget & Control-Line Safety:** Trailer `[HARNESS_HINT: ...]` dibatasi maksimal 2 baris (`<= 320` karakter total), disisipkan **sebelum** `compress_for_webmcp_budget`, dan masuk daftar prefix kontrol yang dipertahankan saat pemotongan.
4. **Warm-Only LSP Invariant:** `webmcp_auto_verify_on_edit` hanya menjalankan `quick_diagnostics_for_files(..., 2500ms)` bila LSP server sudah *warm* — dilarang keras melakukan *cold spawn* di dalam tool edit.
5. **Zero-Framework XSS-Safe UI:** Kontrol pengaturan baru di `index.html` dan `src/views/bridge.ts` wajib memakai Vanilla TypeScript (`createElement`/`textContent`, zero `innerHTML` dinamis).

---

## 3. Desain Detail (Point A & Point C.2)

### Pilar 1 (Point A) — Server-Side Stateful WebMCP Session Harness (`src-tauri/src/webmcp_harness.rs`)

#### 3.1 Model State Sesi (`WebMcpHarnessManager`)
Disimpan pada `AppState` (`pub webmcp_harness: WebMcpHarnessManager`) dengan key `mcp-session-id`:
* **Batas Memori:** Maksimal **16 sesi** aktif (LRU eviction) + auto-expire sesi idle > **60 menit**.
* **Lock Safety:** Konfigurasi & checkpoint next-step dibaca sebelum mengambil `std::sync::RwLock` sinkron agar tidak pernah menahan lock di lintas `.await`.

#### 3.2 Propagasi `session_id` & Urutan Eksekusi
1. [`process_mcp_post`](file:///D:/tauri-mcp-hub/src-tauri/src/mcp_server/http.rs) meneruskan `session_id` ke `execute_rpc_batch_hybrid` dan `RpcRequestContext { pinned_ws, tool_profile, source, session_id }`.
2. [`tools/call`](file:///D:/tauri-mcp-hub/src-tauri/src/mcp_server/rpc.rs) memanggil `execute_tool_with_session_context(Arc::clone(state), ctx.source, tool_name, args, pinned_ws, None, ctx.session_id)`.
3. Di [`tools/mod.rs`](file:///D:/tauri-mcp-hub/src-tauri/src/tools/mod.rs), tepat setelah `execute_tool_core_inner`:
   * Jika `source == "WebMCP"` dan `cfg.webmcp_session_harness == true`: jalankan `state.webmcp_harness.after_tool_call(state, session_id, ws_opt, tool_name, &args, &mut out).await`.
   * Setelah itu baru jalankan `apply_webmcp_output_guard` (`compress_for_webmcp_budget`), di mana `[HARNESS_HINT:` ditambahkan ke `is_webmcp_control_line` agar petunjuk tidak ikut terpotong.

#### 3.3 Empat Aturan Steering Deterministik
1. **Task-Contract Anchor:** Pada panggilan tool pertama sesi (selain `session_checkpoint`), jika workspace memiliki checkpoint tersimpan dengan `next_steps` (atau `goal` aktif), [`peek_checkpoint_next_step`](file:///D:/tauri-mcp-hub/src-tauri/src/checkpoints.rs) menyisipkan 1 baris `[HARNESS_HINT: Active checkpoint (@<alias>) next: ...]` tanpa spawn `git`.
2. **Read-Loop Guard:** Untuk inspeksi (`read_file_range`, `code_outline`, `read_symbol`, `smart_search`, `project_overview`, `find_files`, `batch_inspect`) beruntun pada target identik `>= 2` kali tanpa edit/perintah di antaranya, sisipkan `[HARNESS_HINT: Inspected '<target>' <N>x with no edit. ...]`.
3. **Error-Loop Guard:** Jika `out.is_error` atau output shell berisi non-zero `EXIT CODE:` / `EXIT_CODE:` yang sama `>= 2` kali berturut-turut, sisipkan `[HARNESS_HINT: Repeated failure (<N>x) on '<tool>'. ...]`.
4. **Context-Rot & Checkpoint Nudge:** Saat `session_checkpoint(op="save"|"load")` sukses, reset counter panggilan/byte dan hint nudge. Jika `calls_since_checkpoint >= 12 || bytes_since_checkpoint >= 45_000`, sisipkan `[HARNESS_HINT: Long session without checkpoint; call session_checkpoint(op="save") to preserve progress.]`.

#### 3.4 Auto-Verify Edit (`webmcp_auto_verify_on_edit`)
* Helper bersama [`append_edit_post_checks`](file:///D:/tauri-mcp-hub/src-tauri/src/tools/fs_tools.rs) dipakai oleh `exec_surgical_edit`, `exec_write_file`, dan `exec_multi_surgical_edit`:
  * Menjalankan `hooks::run_post_edit_hooks`.
  * Menjalankan `lsp_bridge::quick_diagnostics_for_files(state, ws, edited_paths, Duration::from_millis(2500))` bila `post_check_lsp == Some(true)` **atau** `(post_check_lsp.is_none() && source == "WebMCP" && cfg.webmcp_auto_verify_on_edit)`.

#### 3.5 Konfigurasi & UI (`config.rs`, `state.rs`, `main.rs`, `index.html`, `src/types.ts`, `src/views/bridge.ts`)
Tambah 2 field `#[serde(default)]` di `AppConfig` & `DashboardSnapshot`:
* `webmcp_session_harness: bool` (default **`true`**)
* `webmcp_auto_verify_on_edit: bool` (default **`false`**)
Ekspos di `update_webmcp_settings` dan panel **WebMCP Optimization** pada tab MCP Bridge.

---

### Pilar 2 (Point C.2) — WebMCP E2E Benchmark & Regression Harness (`src-tauri/src/webmcp_benchmark.rs`)

#### 3.6 Suite Replay Deterministik & CLI Flag `--benchmark-webmcp`
1. Menjalankan server Axum [`build_router`](file:///D:/tauri-mcp-hub/src-tauri/src/mcp_server/mod.rs) di `127.0.0.1:0` melawan **4 skenario E2E** lewat `POST /mcp?profile=compact`:
   * **S1 — `compact_inspect_edit_verify_roundtrips`:** Verifikasi schema Compact (`9–10` tools), `initialize`, `batch_inspect`, dan `multi_surgical_edit` + `verify_command`.
   * **S2 — `slow_command_auto_promote_and_output_guard`:** Uji `run_command` melampaui window sinkron -> `AUTO_PROMOTED_TO_BACKGROUND_JOB` + `poll`/`stop`, serta pemadatan output besar `<= max_output_chars`.
   * **S3 — `session_harness_steering_and_checkpoint_rollover`:** Uji *checkpoint anchor*, *read-loop*, *error-loop*, *context-rot nudge* pada panggilan ke-12, serta reset setelah `session_checkpoint(op="save")`.
   * **S4 — `hybrid_batch_and_guardrail_isolation`:** JSON-RPC batch (read paralel + mutatif berurutan), blokir file sensitif (`.env`), dan isolasi workspace `read_only`.
2. **CLI Flag `--benchmark-webmcp` ([`main.rs`](file:///D:/tauri-mcp-hub/src-tauri/src/main.rs)):**
   * `tauri-mcp-hub.exe --benchmark-webmcp [--json]` mencetak laporan KPI Markdown/JSON dan mengembalikan exit code `0` jika seluruh skenario lulus.

---

## 4. Ringkasan File Terimplementasi

| File | Jenis | Perubahan Inti |
| :--- | :---: | :--- |
| `src-tauri/src/webmcp_harness.rs` | **Baru** | `WebMcpHarnessManager`, `SessionHarnessState`, LRU/TTL cleanup, 4 aturan steering, unit test. |
| `src-tauri/src/webmcp_benchmark.rs` | **Baru** | `WebMcpBenchmarkReport`, 4 skenario E2E replay via `build_router`, runner CLI `--benchmark-webmcp`. |
| `src-tauri/src/config.rs` & `state.rs` | Edit | `webmcp_session_harness` (`true`) & `webmcp_auto_verify_on_edit` (`false`), `AppState.webmcp_harness`, `DashboardSnapshot`. |
| `src-tauri/src/mcp_server/http.rs` & `rpc.rs` | Edit | Propagasi `session_id: Option<&str>` di `RpcRequestContext` menuju eksekusi `tools/call`. |
| `src-tauri/src/checkpoints.rs` | Edit | `peek_checkpoint_next_step` (tanpa spawn `git`). |
| `src-tauri/src/tools/mod.rs` & `fs_tools.rs` | Edit | `execute_tool_with_session_context`, integrasi `webmcp_harness.after_tool_call`, preservasi `[HARNESS_HINT:]`, `append_edit_post_checks`. |
| `src-tauri/src/main.rs` | Edit | Daftarkan modul baru, flag CLI `--benchmark-webmcp`, dan parameter baru di `update_webmcp_settings`. |
| `index.html`, `src/types.ts`, `src/views/bridge.ts` | Edit | Tambah 2 toggle WebMCP Harness & Auto Warm-LSP Verify di panel **WebMCP Optimization** (Vanilla TS XSS-safe). |

---

## 5. Gate Verifikasi

```powershell
Set-Location D:\tauri-mcp-hub\src-tauri
cargo test
cargo run -- --benchmark-webmcp
Set-Location D:\tauri-mcp-hub
npm run build
```

Hasil terukur: **0 compiler warnings/errors, `97/97` unit & E2E tests lulus, `4/4` skenario `--benchmark-webmcp` PASS, dan `npm run build` bersih.**
