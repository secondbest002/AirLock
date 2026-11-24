# Airlock (`tauri-mcp-hub`) — Walkthrough & Panduan Uji Coba MCP

Dokumen ini merangkum cara menjalankan, menguji langsung seluruh kemampuan MCP/ACP/AST/LSP **Airlock v0.1.0**, strategi & pesan `git commit`, serta pengingat audit lanjutan untuk sesi berikutnya.

---

## 1. Status Build & Verifikasi Terakhir

- **Rust Test Suite (`Set-Location d:\tauri-mcp-hub\src-tauri; cargo test`)**: `76 passed; 0 failed` (**0 compiler warnings**)
- **Frontend Check & Build (`npm run build` — `tsc && vite build`)**: lolos (`✓ built`)
- **Total Built-in MCP Tools**: `28` tool aktif
- **Catatan Windows GNU Toolchain**: Selalu jalankan `cargo` dari direktori `d:\tauri-mcp-hub\src-tauri` agar `.cargo/config.toml` terbaca (menghindari error linker `-lgcc_eh`).

---

## 2. Cara Menjalankan Airlock

### A. Mode Desktop GUI + HTTP MCP Server (Direkomendasikan untuk Mulai)

Jalankan dari root repository (`d:\tauri-mcp-hub`):

```powershell
npm run tauri dev
```

Saat aplikasi terbuka:
1. HTTP MCP Server otomatis aktif di `http://127.0.0.1:9494` (default port `9494`).
2. Buka tab **MCP Bridge** (`Ctrl+2`) untuk menyalin **Auth Token**, URL endpoint (`/mcp/<token>`), atau memakai generator **Client Presets** untuk Claude Desktop, Cursor, VS Code, Gemini CLI, Gemini Web, Antigravity, hingga Zed/JetBrains (`--acp`).
3. Buka tab **Hub & Tools** (`Ctrl+3`) untuk mengetes tool langsung via **Tool Simulator** tanpa perlu men-setup client eksternal terlebih dahulu.

### B. Build Binary Standalone untuk Mode `--stdio` & `--acp`

Jika ingin menghubungkan Claude Desktop / Cursor / Zed menggunakan executable standalone:

```powershell
cd d:\tauri-mcp-hub\src-tauri
cargo build --release
```

Binary akan tersedia di `d:\tauri-mcp-hub\src-tauri\target\release\tauri-mcp-hub.exe`.
- Mode `--stdio` otomatis mem-proxy ke instance GUI Airlock yang sedang berjalan, atau fallback ke mode embedded saat GUI ditutup.
- Mode `--acp` menjalankan **Agent Client Protocol** JSON-RPC (NDJSON via stdin/stdout) langsung untuk Zed / JetBrains.

---

## 3. Uji Coba MCP dalam 1 Menit

### Opsi 1 — Lewat UI Dashboard (**Tool Simulator**)

1. Masuk ke tab **Hub & Tools** (`Ctrl+3`).
2. Di panel **Tool Simulator**, pilih salah satu tool baru dari dropdown:
   - `code_outline` — Peta simbol AST (`tree-sitter`) untuk `.rs`, `.ts`, `.tsx`, `.js`, `.jsx`, `.py`, `.go`
   - `read_symbol` — Membaca definisi simbol spesifik (termasuk doc comment/decorator, mis. `AppState::snapshot`)
   - `lsp_diagnostics` — Mengambil error/warning compiler lewat Language Server on-demand (`rust-analyzer` sudah terdeteksi bila ada di `PATH`)
   - `lsp_definition` / `lsp_references` / `lsp_hover` — Navigasi tipe & definisi semantik lintas file di dalam jail workspace
3. Klik **Execute Tool** dan amati hasilnya langsung (serta tercatat di tab **Diff & Audit** `Ctrl+5`).

### Opsi 2 — Lewat HTTP JSON-RPC (`PowerShell`)

Ambil **Auth Token** Anda dari tab **MCP Bridge** (`Ctrl+2`), lalu jalankan di PowerShell:

```powershell
$token = "<TEMPEL_AUTH_TOKEN_DARI_TAB_MCP_BRIDGE>"
$headers = @{
  "Authorization" = "Bearer $token"
  "Content-Type"  = "application/json"
}

# 1. Health check (tanpa token)
Invoke-RestMethod "http://127.0.0.1:9494/health"

# 2. Initialize (cek kapabilitas tools, resources, prompts, tasks)
$initBody = @{
  jsonrpc = "2.0"
  id      = 1
  method  = "initialize"
  params  = @{ protocolVersion = "2026-06-01" }
} | ConvertTo-Json -Depth 5
Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:9494/mcp" -Headers $headers -Body $initBody | ConvertTo-Json -Depth 6

# 3. Panggil read_symbol (AST Tree-sitter)
$symBody = @{
  jsonrpc = "2.0"
  id      = 2
  method  = "tools/call"
  params  = @{
    name      = "read_symbol"
    arguments = @{
      path         = "src-tauri/src/semantic.rs"
      symbol       = "read_symbol_from_content"
      include_docs = $true
    }
  }
} | ConvertTo-Json -Depth 6
(Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:9494/mcp" -Headers $headers -Body $symBody).result.content[0].text
```

### Opsi 3 — Uji Siklus **MCP 2026 Tasks** (`tasks/get`, `tasks/result`, `tasks/list`, `tasks/cancel`)

```powershell
# 1. Jalankan tools/call dengan augmentasi params.task
$taskCall = @{
  jsonrpc = "2.0"
  id      = 10
  method  = "tools/call"
  params  = @{
    name      = "start_command_job"
    arguments = @{
      command = "cargo check"
      label   = "Cargo Check Task"
    }
    task      = @{ ttl = 120000 }
  }
} | ConvertTo-Json -Depth 6

$created = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:9494/mcp" -Headers $headers -Body $taskCall
$taskId = $created.result.task.taskId
Write-Host "Task ID: $taskId (status: $($created.result.task.status))"

# 2. Cek tab Live Monitor -> tabel Background Command Jobs akan menampilkan badge "MCP Task"

# 3. Poll status via tasks/get
$getBody = @{ jsonrpc = "2.0"; id = 11; method = "tasks/get"; params = @{ taskId = $taskId } } | ConvertTo-Json -Depth 5
Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:9494/mcp" -Headers $headers -Body $getBody | ConvertTo-Json -Depth 5

# 4. Ambil hasil akhir (long-poll hingga selesai) via tasks/result
$resBody = @{ jsonrpc = "2.0"; id = 12; method = "tasks/result"; params = @{ taskId = $taskId } } | ConvertTo-Json -Depth 5
(Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:9494/mcp" -Headers $headers -Body $resBody).result.content[0].text
```

> **Tips Workspace Pinning:** Untuk mengunci koneksi ke workspace tertentu (mis. `@api`), tambahkan query `?workspace=api` pada URL endpoint atau kirim HTTP header `x-airlock-workspace: api`.

---

## 4. Uji Coba Fitur Lanjutan

1. **Downstream MCP Elicitation UI**:
   - Di tab **Hub & Tools** (`Ctrl+3`) -> klik **Add MCP Server** untuk mendaftarkan server downstream (HTTP atau Stdio).
   - Saat inisialisasi downstream, Airlock otomatis mengiklankan kapabilitas `"elicitation": {}`.
   - Jika tool downstream mengirim `elicitation/create`, jendela Airlock akan memunculkan modal `#mcp-elicitation-dialog` yang merender form dinamis (checkbox, radio button 2–5 opsi, select >5 opsi, number/integer, text/email/url/date) dengan tombol **Submit**, **Decline**, dan **Cancel**.
2. **On-Demand LSP Bridge & Monitor**:
   - Memanggil tool `lsp_diagnostics` pada file `.rs` akan men-spawn `rust-analyzer` secara lazy (maksimum **2** server aktif bersamaan, LRU eviction, serta auto-shutdown setelah idle **5 menit**).
   - Pantau dan hentikan proses LSP kapan saja dari tab **Live Monitor** (`Ctrl+4`) pada kartu **Active LSP Servers (On-Demand)** menggunakan tombol **Stop LSP** atau **Stop All LSP**.
3. **Agent Client Protocol (`--acp`) di Zed / JetBrains**:
   - Pilih preset **Zed / JetBrains (ACP Agent --acp)** pada **Client Presets** di tab **MCP Bridge** (`Ctrl+2`).
   - Tempelkan konfigurasi `agent_servers.Airlock` (`args: ["--acp"]`) ke settings IDE. Mode `--acp` mendukung `initialize`, `session/new`, `session/load`, `session/set_mode` (`direct` / `plan_first`), streaming `session/update`, serta jembatan HITL `session/request_permission`.
4. **Guardrail, Workspace Hooks & Plan-First Artifacts**:
   - Mode **Safe-Edit** / **Read-Only** / **Autonomous** dapat diatur global maupun per workspace.
   - Project dengan `.airlock/hooks.json` diverifikasi dengan SHA-256 trust sebelum hook post-edit dijalankan.

---

## 5. Strategi & Pesan `git commit`

Saat ini seluruh hasil **Gelombang 1–5B** masih berada di working tree (uncommitted). Folder `.gemini/` berisi dokumen audit & blueprint lokal—**jangan ikut di-stage** saat commit.

### Opsi A (Direkomendasikan): 3 Commit Terstruktur & Buildable

Memisahkan core backend Rust, frontend UI TypeScript/CSS/HTML, dan dokumentasi walkthrough agar riwayat Git mudah di-review dan setiap layer jelas ruang lingkupnya:

#### Commit 1 — Core Backend Rust (Waves 1–5B)
```powershell
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json src-tauri/src/
git commit -m "feat(core): harden security and add multi-workspace, MCP 2026 tasks/elicitation, ACP & LSP bridge" -m "- Harden security: constant-time tokens, DPAPI secret migration, CSP, Safe-Edit HITL, and Windows job-tree cleanup
- Add multi-workspace registry, workspace pinning, AgentRunManager concurrent execution, and Plan-First artifacts
- Add SKILL.md discovery, Plugin 1.0.0 installer/secrets, and SHA-256 trusted .airlock/hooks.json pipeline
- Implement Tree-sitter AST engine (code_outline + read_symbol), MCP 2026 Tasks state machine, and downstream Elicitation bridge
- Add --acp server mode and on-demand LRU-capped LspBridge (lsp_diagnostics, lsp_definition, lsp_references, lsp_hover)"
```

#### Commit 2 — Frontend UI Dashboard & Modals
```powershell
git add index.html src/
git commit -m "feat(ui): upgrade Airlock dashboard for multi-workspace, artifacts, plugins, elicitation & LSP monitor" -m "- Upgrade Agent, Bridge, Hub, Monitor, and Audit views with pure DOM rendering (zero dynamic innerHTML)
- Add Multi-Workspace switcher, Parallel Runs strip, Plan-First Artifacts drawer, Plugins view, and Hooks trust modal
- Add dynamic MCP Elicitation schema modal, MCP Task job badge, Active LSP Servers monitor strip, and --acp preset"
```

#### Commit 3 — Dokumentasi Walkthrough
```powershell
git add walkthrough.md
git commit -m "docs: add hands-on Airlock MCP, ACP & LSP walkthrough"
```

---

### Opsi B (Alternatif Cepat): Single Commit
Jika Anda ingin menggabungkan seluruh perubahan code + UI + docs dalam 1 commit:

```powershell
git add index.html walkthrough.md src/ src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json src-tauri/src/
git commit -m "feat: complete Airlock Waves 1-5B (security hardening, multi-workspace, MCP 2026, ACP & LSP)"
```

> Pastikan `.gemini/` tetap tidak di-stage (cek dengan `git status --short` sebelum push).

---

## 6. Pembaruan Audit, Reliabilitas & Sisa Backlog

Status tindak lanjut pasca-Gelombang 1–5B:

1. **F-06 (Command Execution Policy Hardening)** — **Selesai**: Mode `guarded`, `allowlist` (blokir chaining/metakarakter & validasi prefix), dan `hitl` (Always HITL di semua mode), beserta deobfuscation di [security.rs](src-tauri/src/security.rs) dan kontrol UI di tab **MCP Bridge**.
2. **Studi & Perbaikan Reliabilitas Eksekusi (Tahap A–C)** — **Selesai**:
   - **Live Job Observability**: Tick `1s` backend (`job-updated`), in-place row patching di Live Monitor, indikator global `#statusbar-jobs-pill`, preview baris terakhir (`↳ ...`), dan modal *Live Logs* berbasis `get_background_job_logs_delta` dengan *Auto-scroll* dan tombol *Stop Job*.
   - **Gemini Web & Windows Hardening**: Environment non-interaktif bersama (`build_shell_command`), adaptive long-poll `wait_secs` (default `12s` untuk `WebMCP`) di `poll_command_job`, detektor file lock / prompt / hening `>= 25s` (`STALL_WARNING:`), serta grace window +70s untuk popup HITL di `mcp_server.rs`.
   - **Dokumentasi Studi & Checklist Uji**: Tersimpan di `.gemini/Findings/realworld-edge-cases-reliability-study.md` (jangan ikut di-stage ke Git).
3. **F-10 Sisa Refactoring Modularitas** — **Selesai**: Memecah dua modul Rust berukuran besar yang tersisa menjadi submodul terfokus seperti pola `src-tauri/src/tools/`:
   - `src-tauri/src/providers/` (`mod.rs`, `common.rs`, `openai.rs`, `anthropic.rs`, `tests.rs`)
   - `src-tauri/src/mcp_server/` (`mod.rs`, `auth.rs`, `rpc.rs`, `http.rs`, `tests.rs`)
