# Studi Pemetaan Masalah Dunia Nyata, Edge Cases & Standar Reliabilitas Eksekusi — Airlock (`tauri-mcp-hub`)

| Atribut | Detail |
| :--- | :--- |
| **Proyek** | Airlock (`tauri-mcp-hub`) v0.1.0 |
| **Workspace** | `D:\tauri-mcp-hub` |
| **Tanggal Studi** | 25 November 2026 |
| **Cakupan** | Siklus eksekusi `WebMCP` (Gemini Web) ↔ `mcp_server.rs` ↔ `jobs.rs` / `tools/shell.rs` ↔ Dashboard UI |
| **Status Implementasi** | **Tahap A (Observabilitas) & Tahap B (Hardening Eksekusi)** telah diimplementasikan & diverifikasi (`76 passed; 0 failed`, `npm run build` OK) |

---

## 1. Latar Belakang & Gejala di Dunia Nyata

Saat Airlock digunakan bersama klien **Gemini Web** lewat bridge MCP untuk menjalankan build/test berdurasi panjang (`start_command_job` dengan `timeout > 300s`), ditemukan dua hambatan utama yang membuat proses sulit dipantau dan rawan *hang* bahkan pada tahap awal:

1. **Observabilitas UI Blind (`running · 0s`)**: Job tampil macet pada status `running` dan `0s · 0 lines` hingga proses selesai/mati. Modal *Job Output* tidak auto-refresh saat terbuka, sehingga operator tidak bisa membedakan apakah proses sedang kompilasi normal, menunggu lock file, atau hang.
2. **Mismatch Turn-Based Gemini Web & Jebakan Proses Windows**:
   * Klien LLM berbasis browser tidak memiliki loop polling otomatis yang ramah latensi; memanggil `poll_command_job` yang langsung return 0ms memicu *spin-polling* puluhan putaran, mendorong model menjalankan `sleep`, atau terkena cutoff turn.
   * Subprocess Windows tanpa environment non-interaktif rentan menggantung senyap akibat prompt kredensial Git, pager (`less`/`more`), buffering stdout Python, atau path UNC `\\?\`.
   * Popup konfirmasi HITL (`guardrail.rs` timeout `90s`) berpotensi diputus lebih awal oleh batas anti-hang `24s` di `mcp_server.rs`.

---

## 2. Peta Kasus Dunia Nyata, Edge Cases & Penanganannya

### A. Observabilitas & Live UI (Diselesaikan di Tahap A)

| ID | Kasus / Edge Case | Akar Penyebab Awal | Solusi Terimplementasi | Lokasi Kode |
| :--- | :--- | :--- | :--- | :--- |
| **RW-01** | Status & durasi job membeku di `running (0s)` | `emit_job_update` hanya dipanggil saat spawn (0s) dan selesai; UI tanpa ticker | Ticker backend `1s` terikat `done_rx` memancarkan `job-updated` berisi `elapsed_secs`, `timeout_secs`, `idle_secs`, `last_line`, `stall_hint` | `src-tauri/src/jobs.rs` |
| **RW-02** | Modal *Logs* statis & kehilangan scroll saat refresh | `openJobLogsModal` hanya memanggil `get_background_job_logs` 1x; light-dismiss (`closedby="any"`) tidak me-reset `activeLogJobId` | Command inkremental `get_background_job_logs_delta`, auto-stream saat `job-updated`, checkbox *Auto-scroll*, tombol *Stop Job* di modal, cleanup di event `close` | `src-tauri/src/jobs.rs`, `src-tauri/src/main.rs`, `src/views/modals.ts` |
| **RW-03** | Tombol *Stop* / *Logs* gagal diklik saat tick `1s` | `renderBackgroundJobs` menghapus dan membangun ulang seluruh `<tbody>` setiap update | Reconcile baris in-place (`patchJobRow`) berdasarkan `data-job-id`; node tombol tidak di-detach selama job tidak berubah urutan | `src/views/bridge.ts` |
| **RW-04** | User berada di tab lain saat job Gemini berjalan | Tabel job hanya ada di tab *Live Monitor* | Pill global `#statusbar-jobs-pill` di footer statusbar (muncul saat ada job `running`, klik untuk lompat ke `#jobs-monitor-card`) | `index.html`, `src/views/bridge.ts`, `src/main.ts` |

### B. Eksekusi Perintah, Gemini Web & Windows Traps (Diselesaikan di Tahap B)

| ID | Kasus / Edge Case | Akar Penyebab Awal | Solusi Terimplementasi | Lokasi Kode |
| :--- | :--- | :--- | :--- | :--- |
| **RW-05** | Spin-polling `poll_command_job` / loop `sleep` di Gemini Web | `poll_job` return instan 0ms meski status masih `running` | `poll_job_with_wait` dengan `wait_secs` (`0..=18`, default **`12s`** untuk `WebMCP`, `0s` lokal) yang menunggu `done_rx` secara event-driven dan bangun seketika saat selesai | `src-tauri/src/jobs.rs`, `src-tauri/src/tools_extra.rs`, `src-tauri/src/tools/schemas.rs` |
| **RW-06** | Prompt Git/NPM/Pip tersembunyi, pager, buffering Python & `\\?\` CWD | `cmd.exe /C` di `jobs.rs` dan `shell.rs` terduplikasi; `jobs.rs` mengirim raw `\\?\` root ke `current_dir` | Shared helper `build_shell_command` memakai `clean_path_string` + env `CI=true`, `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`, `GIT_PAGER=cat`, `PAGER=cat`, `NO_COLOR=1`, `TERM=dumb`, `NPM_CONFIG_YES=true`, `PIP_NO_INPUT=1`, `PYTHONUNBUFFERED=1`, `CARGO_TERM_PROGRESS_WHEN=never` | `src-tauri/src/process_util.rs`, `src-tauri/src/jobs.rs`, `src-tauri/src/tools/shell.rs` |
| **RW-07** | Deadlock `cargo` lock (`target/`) atau proses macet tanpa tanda | Output `Blocking waiting for file lock` / prompt tidak ditandai sebagai peringatan | `detect_stall_or_lock_hint` memindai 6 baris terakhir + `idle_secs >= 25`, memancarkan `STALL_WARNING:` di `poll_job_with_wait` dan badge `lock/prompt!` / `silent Xs` di UI | `src-tauri/src/jobs.rs`, `src/views/bridge.ts`, `src/views/modals.ts` |
| **RW-08** | Safe-Edit / Always-HITL terpotong timeout `24s` saat user meninjau popup | `mcp_server.rs` memutus semua `tools/call` non-task pada `24s` padahal `guardrail.rs` menunggu konfirmasi user s.d. `90s` | `tokio::pin!(fut)` di `mcp_server.rs`: jika `24s` tercapai saat `pending_tool_approvals` tidak kosong, berikan perpanjangan `+70s` | `src-tauri/src/mcp_server.rs` |

---

## 3. Batasan Nyata & Edge Cases yang Masih Perlu Diperhatikan (Operational Guardrails)

Studi ini juga mencatat batasan lingkungan yang **tidak bisa dihilangkan sepenuhnya dari sisi kode** dan wajib menjadi prosedur uji standar:

1. **Self-Lock Cargo saat `npm run tauri dev` Aktif di Repo yang Sama**:
   * Jika Airlock dijalankan via `npm run tauri dev` dari `D:\tauri-mcp-hub` dan pada saat yang sama Gemini Web menjalankan `cargo test` di workspace yang sama, Cargo menahan lock pada `src-tauri/target/debug/.cargo-lock`.
   * **Mitigasi:** Gunakan binary release (`tauri-mcp-hub.exe`) saat menguji/mengedit repo Airlock sendiri, atau jalankan `cargo test --target-dir <temp_dir>` jika harus mengetes sambil `tauri dev` berjalan. Bila lock terjadi, `STALL_WARNING:` dan indikator `lock/prompt!` kini langsung muncul.
2. **Output Progress Tanpa Newline (`\r`-only PTY Spinner)**:
   * Pembaca `BufReader::lines()` memecah output per `\n`. Sebagian besar tool sudah ditekan oleh `CI=true` dan `CARGO_TERM_PROGRESS_WHEN=never`, namun tool CLI pihak ketiga yang memaksa *carriage-return* `\r` tanpa `\n` tidak akan memperbarui `last_line` hingga muncul `\n` atau proses selesai.
3. **Timeout Jaringan Tunnel / Ekstensi Browser Saat HITL**:
   * Meskipun `mcp_server.rs` kini memberi grace hingga `94s` untuk popup HITL, reverse proxy eksternal (seperti Cloudflare Tunnel atau fetch ekstensi browser) tetap memiliki batas timeout koneksi HTTP (~60–100s). Untuk task panjang tanpa pengawasan terus-menerus, gunakan kombinasi mode `Autonomous` + `Strict Allowlist` (F-06).
4. **Working Directory Cargo pada Toolchain Windows GNU**:
   * Menjalankan `cargo test --manifest-path src-tauri/Cargo.toml` dari root `D:\tauri-mcp-hub` melewatkan `src-tauri/.cargo/config.toml`, menyebabkan error linker `-lgcc_eh`. Selalu jalankan perintah Cargo dari dalam `D:\tauri-mcp-hub\src-tauri`.

---

## 4. Checklist Standar Uji Coba Sebelum Merilis / Melanjutkan Fitur Besar

Sebelum mengaktifkan atau menguji fitur lanjutan (Blueprint 03–05 / F-10), pastikan skenario berikut lulus:

- [ ] **Rust & Frontend Baseline:** Dari `D:\tauri-mcp-hub\src-tauri` jalankan `cargo test` (`76 passed; 0 failed`), lalu dari root jalankan `npm run build` (`tsc && vite build` lulus tanpa error).
- [ ] **Verifikasi Live Progress UI (Tahap A):** Jalankan job >15 detik (mis. lewat Simulator `start_command_job` dengan `ping -n 16 127.0.0.1`). Pastikan pill `#statusbar-jobs-pill` muncul, durasi naik tiap detik (`1s`, `2s`, ...), kolom preview `↳ ...` terisi, dan modal *Live Logs* menambah baris otomatis tanpa memutus klik tombol `Stop`.
- [ ] **Verifikasi Long-Poll & Non-Interactive (Tahap B):** Panggil `start_command_job` lalu `poll_command_job` via endpoint `/mcp` (source `WebMCP`) pada perintah berdurasi 3–5 detik; pastikan `poll_command_job` selesai dalam **1 kali panggil** (`wait_secs=12` bangun instan) dan environment `CI=true`, `GIT_TERMINAL_PROMPT=0`, `PYTHONUNBUFFERED=1` aktif.
- [ ] **Verifikasi Stall / Lock Detection:** Uji perintah dengan jeda sunyi >=25 detik atau keluaran `Blocking waiting for file lock`; pastikan `STALL_WARNING:` muncul pada respons `poll_command_job` dan UI menampilkan indikator `silent` / `lock/prompt!`.
- [ ] **Verifikasi Shell Policy (F-06):** Uji mode `allowlist` (blokir `&&` / `;` dan prefix di luar daftar) dan mode `hitl` (popup muncul di mode `Autonomous` dan tidak diputus di detik ke-24).
