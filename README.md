# Airlock (`tauri-mcp-hub`)

Token-efficient desktop MCP server, remote tunnel bridge, downstream MCP aggregator, and multi-provider local coding agent built with **Tauri v2**, **Axum**, and **Vanilla TypeScript** featuring the dark-first **Obsidian Studio** 5-view workspace UI (`Local Agent`, `MCP Bridge`, `Hub & Tools`, `Live Monitor`, `Diff & Audit`) and zero external UI runtime frameworks.

> **Persistence Compatibility:** Airlock stores configuration, Windows DPAPI encrypted keys, OAuth tokens, chat history, and SHA-256 rollback snapshots under `~/.smart-tauri-hub/` so existing setups carry over seamlessly.

## Core Architecture

- **Route A — Remote & Local MCP Bridge (Primary)**
  - Streamable HTTP JSON-RPC 2.0 server (`POST/GET/DELETE /mcp` and `/mcp/:token`) + health endpoint (`GET /health`).
  - Full OAuth 2.1 Dynamic Client Registration + PKCE with an in-app interactive consent popup (**Deny**, **Approve Once**, **Approve & Trust**).
  - Remote HTTPS exposure via **Cloudflare Quick Tunnel**, **Cloudflare Named Tunnel** (`TUNNEL_TOKEN` env var), or **Tailscale Funnel** with live redacted logs and Auto-Kill Idle watcher.
  - **Local CLI Stdio Bridge** (`tauri-mcp-hub.exe --stdio`): automatically proxies JSON-RPC over stdin/stdout to the running desktop Hub HTTP server, or falls back cleanly to embedded mode when the GUI is closed.
  - **True MCP Aggregator (Multiplexer)**: connects to downstream HTTP or Stdio MCP servers and exposes their tools under `namespace__tool` with unified guardrails, Token Guard output compression, and audit logging.
  - **MCP Resources, Prompts & 2026 Forward-Compat**: supports `resources/list`, `resources/templates/list`, `resources/read` (`workspace://overview`, `workspace://git/diff`, `workspace://audit/recent`, `workspace://jobs/active`, `workspace://file/{rel_path}`), `prompts/list`, `prompts/get`, `server/discover`, `_meta.cacheControl`, and `Mcp-Method` / `Mcp-Name` headers.

- **Route B — Multi-Provider Local Coding Agent (Secondary)**
  - Live SSE streaming over localhost (`Gemini` / AI Studio, `OpenAI-Compatible` for Ollama, LM Studio, OpenRouter, Groq, DeepSeek, OpenAI, and `Anthropic Claude`).
  - Windows **DPAPI** encrypted credential storage (`~/.smart-tauri-hub/secrets.dpapi`).
  - Persistent per-workspace multi-session chat history (`~/.smart-tauri-hub/chats/`).
  - Optional automatic project instruction injection (`AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `.cursorrules`, `.github/copilot-instructions.md`).
  - `@file` workspace attachments and clipboard/file image uploads.

## 14 Built-in Smart Tools & Token Guard

| Tool | Purpose | Token-Saving Behavior |
| :--- | :--- | :--- |
| `project_overview` | Directory tree + Git status | Depth-limited tree, ignores `node_modules`/`.git`/`target` |
| `smart_search` | Regex / text search | Binary/game asset exclusion + 2-line context |
| `read_file_range` | Single or batch line slices | Reads targeted line ranges with `batch_paths` |
| `surgical_edit` | Multi-block search/replace | CRLF-safe block replacement + diff preview + snapshot |
| `patch_and_verify` | Edit + verify command | Combines edit + build/test verification in 1 turn |
| `run_command_compressed` | Bounded shell execution | Enforces Command Guard, `ChildTreeGuard`, and log compression |
| `write_file_jailed` | Write/create jailed files | Snapshots existing content for 1-click revert |
| `find_files` | Glob/size file search | Compact path + byte size index |
| `code_outline` | Heuristic AST/symbol map | Extracts symbols (`fn`, `struct`, `class`, `export`) with line numbers |
| `git_ops` | Structured Git operations | Direct arg-array `git` without shell injection |
| `file_ops` | `mkdir`, `move`, `delete` | Snapshot backup + `move_from_path` support for 1-click undo |
| `start_command_job` | Async background command | Immediate `job_id` return (<50ms) to bypass WebMCP timeouts |
| `poll_command_job` | Incremental job output | Cursored compressed stdout/stderr lines + runtime status |
| `stop_command_job` | Terminate background job | Process-tree termination (`taskkill /T /F` on Windows) |

## Guardrail Modes & Persistent Rollback

- **Read-Only**: Blocks all mutative tools and shell execution across WebMCP, LocalAgent, and downstream tools.
- **Safe Edit**: Requires interactive in-app popup confirmation (**Deny**, **Approve Once**, **Approve for Session**) for shell commands, commits, deletes, `patch_and_verify`, and mutative downstream tools.
- **Autonomous**: Auto-executes inside the directory-jailed workspace while enforcing **Sensitive File Guard** and **Command Guard**.
- **SHA-256 Disk Snapshots & Turn Group Revert**: Snapshots persist under `~/.smart-tauri-hub/snapshots/`, conflict checks detect external edits (`CONFLICT:` + Force Revert option), and multi-file edits from a Local Agent turn can be undone together via **Revert Turn Group**.

## Prerequisites & Build (Windows GNU Toolchain)

1. Install **Node.js**, **Rust** (`stable-x86_64-pc-windows-gnu`), and **LLVM-MinGW** (e.g., via WinGet `MartinStorsjo.LLVM-MinGW.MSVCRT`).
2. Copy `src-tauri\.cargo\config.toml.example` to `src-tauri\.cargo\config.toml` and adjust paths if needed.
3. Run development mode:
   ```powershell
   .\run-dev.ps1
   ```
4. Build release executable (`src-tauri\target\release\tauri-mcp-hub.exe`) or full installer bundle:
   ```powershell
   .\build-release.ps1
   .\build-release.ps1 -Bundle
   ```
5. Run automated tests:
   ```powershell
   cd src-tauri
   cargo test
   ```
   ```powershell
   npm run build
   ```

## Local `--stdio` MCP Client Configuration

Configure Claude Desktop, Cursor, or Antigravity to launch the `--stdio` bridge directly:

```json
{
  "mcpServers": {
    "smart-tauri-hub": {
      "command": "D:\\tauri-mcp-hub\\src-tauri\\target\\release\\tauri-mcp-hub.exe",
      "args": ["--stdio"]
    }
  }
}
```
