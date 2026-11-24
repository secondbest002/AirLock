# Airlock — Obsidian Studio Design System (`MASTER.md`)

## 1. Product Identity & Character Anchor
- **Product Name:** **Airlock** *(rebranding **Smart Tauri Hub & Commander** / `tauri-mcp-hub`)*
- **Character Anchor:** *Precision Developer Airlock* — a sealed, dark-mode-first control deck that isolates local repositories (`jailed root`) while bridging local AI agents and remote MCP clients through transparent guardrails, encrypted credentials, and 1-click snapshot rollbacks.
- **Visual Philosophy ("Quiet Chrome, Rich Content"):**
  - Calm, deep neutral graphite chrome (`#09090B`–`#121318`) with zero purple/indigo glows, zero floating ambient blobs, and zero emoji icons.
  - Richness and visual energy stay anchored in real telemetry: live HTTP payload traces, unified diffs, agent thought/step traces, and crisp monospace configuration blocks.
  - Strict Vanilla TypeScript + native HTML/CSS/WebView2 platform APIs (zero UI runtime frameworks).

---

## 2. Color, Typography & Surface Specification
All color, spacing, radius, elevation, and motion tokens live in [`tokens.css`](./tokens.css).

### Surface & Border Hierarchy
| Token | Hex / Value | Role |
| :--- | :--- | :--- |
| `--bg-canvas` | `#09090B` | Root viewport canvas (`color-scheme: dark`) |
| `--bg-rail` | `#0D0E12` | Left navigation rail & top command bar |
| `--bg-surface` | `#121318` | Primary cards, chat pane, diff inspector |
| `--bg-elevated` | `#191B22` | Inputs, buttons, active segments, dropdowns |
| `--bg-code` | `#0B0C10` | Code blocks, terminal logs, unified diff viewer |
| `--border-subtle` | `#20232D` | Structural hairlines & dividers |
| `--border-default` | `#2D3140` | Interactive borders (WCAG 1.4.11 compliant) |
| `--border-strong` | `#3E4458` | Hovered control borders |

### Dual Accent Variants
| Variant | Selector | `--accent` | `--accent-hover` | `--accent-fg` | `--on-accent` |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Electric Mint** *(Default)* | `:root` | `#10B981` | `#059669` | `#34D399` | `#031A12` |
| **Ice Cyan** *(Alternative)* | `:root[data-accent="cyan"]` | `#0EA5E9` | `#0284C7` | `#38BDF8` | `#041521` |

- Tints (`--accent-tint`, `--accent-border`, `--accent-spotlight`) are computed using `color-mix(in oklab, ...)` to stay safely in-gamut without triggering Chromium's low-chroma `oklch` neutral hue-shift bug.
- Semantic status badges (`--status-ok`, `--status-warn`, `--status-info`, `--status-danger`) always pair color with explicit icons and/or text labels (never color alone).

### Typography & Offline Bundling Plan
- **UI Sans:** `Plus Jakarta Sans` (`--font-ui`), with `"Segoe UI", system-ui, -apple-system, sans-serif` fallback.
- **Technical Mono:** `JetBrains Mono` (`--font-mono`), with `"Cascadia Code", Consolas, monospace` fallback.
- **Numeric Discipline:** `font-variant-numeric: tabular-nums` is enforced on all counters (`Tokens Saved`, `Turns Saved`), ports, timestamps, durations, and diff/audit tables.
- **Phase 2 Offline Font Plan:** Bundle `.woff2` subsets of `Plus Jakarta Sans` (400, 500, 600, 700) and `JetBrains Mono` (400, 500, 600) locally under `src/assets/fonts/` via `@font-face` (`font-display: swap`) so Tauri renders identically offline without external CDN dependency.

### Concentric Radii & Specular Elevation
- Outer Radius = Inner Radius + Padding (`12px` card with `16px` padding → `8px` inner controls; `8px` control → `6px` inner pills).
- Dark cards use `--shadow-card` (subtle `1px` top inner specular highlight `rgba(255,255,255,0.055)` + crisp contact shadow + soft ambient shadow).

---

## 3. Five-View Architecture & Left Sidebar Rail Layout

```
+-------------------------------------------------------------------------+
| [Brand / Rail] | Workspace Capsule (Root + git + Pin + Recent) | Guard  | Ctrl+K |
+----------------+--------------------------------------------------------+
|  RAIL (216px   |                                                        |
|   <-> 64px)    |   ACTIVE VIEW CANVAS (100% height, scroll-isolated)    |
|                |                                                        |
|  > Local Agent |   Ctrl+1  panel-agent   Chat + Collapsible Config Drawer|
|    MCP Bridge  |   Ctrl+2  panel-bridge  Server + Tunnel + Guardrails   |
|    Hub & Tools |   Ctrl+3  panel-hub     Downstream Aggregator + Sim    |
|    Monitor     |   Ctrl+4  panel-monitor HTTP Traffic + Background Jobs |
|    Diff & Audit|   Ctrl+5  panel-audit   Audit List + Visual Diff       |
|                |                                                        |
|  [Telemetry]   |                                                        |
|  Srv :9494     |                                                        |
|  Tunnel Active |                                                        |
|  Tokens / Turns|                                                        |
+----------------+--------------------------------------------------------+
| Statusbar: Ready / Meta / Shortcut Hints                                |
+-------------------------------------------------------------------------+
```

### Region Breakdown
1. **Top Command Bar (`48px`):**
   - **Workspace Capsule:** Displays active jailed root (`#workspace-input`), branch pill (`#workspace-git-branch`), Pin toggle (`#btn-pin-workspace`), Apply (`#btn-apply-workspace`), Browse (`#btn-pick-workspace`), and Recent/Pinned trigger (`#btn-recent-workspaces` -> `#workspace-dropdown`).
   - **Sliding Guardrail Switcher:** 3-state segmented control (`Read-Only`, `Safe Edit`, `Auto`) with a smooth GPU-composited sliding pill indicator.
   - **Command Palette Trigger:** `#btn-open-palette` (`Ctrl+K`).
2. **Collapsible Left Sidebar Rail (`216px` expanded ↔ `64px` icon rail; auto-compacts below `1060px` viewport width):**
   - **Navigation (`role="tablist"`):**
     1. **Local Agent** (`Ctrl+1`, `#panel-agent`) — Chat transcript, session switcher, prompt templates, `@file` / image attachments, and a **collapsible Config & Smart Tools drawer** on the right (keeps the chat canvas spacious by default).
     2. **MCP Bridge** (`Ctrl+2`, `#panel-bridge`) — Embedded MCP Server (Route A), Remote Tunnel & Auto-Kill Idle, Security & Desktop Guardrails, Authorized OAuth Clients.
     3. **Hub & Tools** (`Ctrl+3`, `#panel-hub`) — Downstream MCP Aggregator (`namespace__tool`) and Built-in Smart Tool Simulator & Self-Test (split out from the overcrowded old Bridge tab).
     4. **Live Monitor** (`Ctrl+4`, `#panel-monitor`) — Live HTTP Traffic Monitor (clickable rows -> Payload Inspector) and Background Command Jobs (Async).
     5. **Diff & Audit** (`Ctrl+5`, `#panel-audit`) — Filterable Live Audit Trail + Visual Diff Inspector with VS Code / Explorer launch, `Revert Turn Group`, and `1-Click Revert File`.
   - **Rail Telemetry Footer:** Live Server status (`#status-server-badge`), Tunnel status (`#status-tunnel-badge`), `Tokens Saved` (`#metric-tokens-saved`), and `Turns Saved` (`#metric-turns-saved`).

---

## 4. Phase 2 DOM-ID Preservation & Safe Rebranding Map

All existing element IDs from `index.html` are preserved verbatim across the 5 views so `src/views/*.ts`, `src/main.ts`, and Tauri IPC commands stay intact without breaking logic:

| View / Region | Preserved DOM IDs |
| :--- | :--- |
| **Top Bar & Workspace** | `#workspace-input`, `#workspace-git-branch`, `#workspace-dropdown`, `#btn-recent-workspaces`, `#btn-pin-workspace`, `#btn-apply-workspace`, `#btn-pick-workspace`, `#btn-open-palette`, `.guardrail-btn[data-guardrail]` |
| **Rail Navigation & Telemetry** | `#tab-btn-agent`, `#tab-btn-bridge`, `#tab-btn-audit` *(plus new `#tab-btn-hub`, `#tab-btn-monitor`)*, `#audit-count-badge`, `#status-server-badge`, `#status-tunnel-badge`, `#metric-tokens-saved`, `#metric-turns-saved` |
| **View 1: Local Agent (`#panel-agent`)** | `#agent-card-title`, `#agent-provider`, `#openai-compat-fields`, `#agent-base-url`, `#agent-api-key-label`, `#agent-api-key`, `#btn-clear-api-key`, `#agent-model`, `#btn-fetch-models`, `#agent-custom-model`, `#toggle-auto-rules`, `#btn-save-agent`, `#btn-clear-chat`, `#active-tools-heading`, `#chat-session-select`, `#btn-new-session`, `#btn-delete-session`, `#chat-messages`, `#chat-form`, `#attachment-chips`, `#attach-file-path`, `#btn-attach-file`, `#attach-image-input`, `#btn-attach-image`, `#chat-input`, `#agent-run-status`, `#btn-stop-agent`, `#btn-send-chat` |
| **View 2: MCP Bridge (`#panel-bridge`)** | `#btn-self-diag`, `#btn-toggle-server`, `#local-mcp-url`, `#server-port-input`, `#btn-apply-port`, `#mcp-auth-token`, `#btn-toggle-secrets`, `#btn-rotate-token`, `#mcp-preset-select`, `#btn-copy-preset`, `#btn-preview-preset`, `#btn-copy-mcp-url`, `#btn-toggle-tunnel`, `#tunnel-provider-select`, `#cf-named-fields`, `#cf-named-token`, `#cf-named-url`, `#btn-save-cf-named`, `#btn-clear-cf-named`, `#public-tunnel-url`, `#idle-timeout-select`, `#tunnel-logs-drawer`, `#btn-clear-tunnel-logs`, `#tunnel-logs-pre`, `#toggle-sensitive-guard`, `#toggle-close-to-tray`, `#btn-revoke-all-oauth`, `#oauth-clients-body` |
| **View 3: Hub & Tools (`#panel-hub`)** | `#btn-add-mcp`, `#mcp-hub-body`, `#sim-tool-select`, `#btn-copy-sim-rpc`, `#btn-run-sim`, `#sim-args-input`, `#sim-output` *(relocated cleanly out of `#panel-bridge` without altering any ID)* |
| **View 4: Live Monitor (`#panel-monitor`)** | `#traffic-filter-input`, `#traffic-status-filter`, `#btn-clear-traffic`, `#traffic-count-label`, `#http-traffic-body`, `#btn-clear-finished-jobs`, `#jobs-body` *(relocated cleanly out of `#panel-bridge` without altering any ID)* |
| **View 5: Diff & Audit (`#panel-audit`)** | `#btn-refresh-audit`, `#audit-search-input`, `#audit-source-filter`, `#audit-revertable-only`, `#audit-list`, `#diff-title`, `#diff-meta`, `#btn-open-vscode`, `#btn-open-explorer`, `#btn-revert-group`, `#btn-revert-selected`, `#diff-viewer` |
| **Statusbar & Modals** | `#toast-region`, `#statusbar-meta`, `#oauth-consent-dialog` (`#oauth-dialog-title`, `#oauth-req-client`, `#oauth-req-redirect`, `#oauth-req-scope`, `#btn-oauth-deny`, `#btn-oauth-once`, `#btn-oauth-trust`), `#revert-conflict-dialog` (`#revert-dialog-title`, `#revert-conflict-message`, `#btn-revert-cancel`, `#btn-revert-force`), `#traffic-inspector-dialog` (`#traffic-insp-title`, `#traffic-insp-meta`, `#btn-copy-insp-req`, `#traffic-insp-req`, `#btn-copy-insp-res`, `#traffic-insp-res`, `#btn-close-traffic-insp`), `#preset-preview-dialog` (`#preset-modal-title`, `#preset-modal-desc`, `#preset-modal-file`, `#preset-modal-use-tunnel`, `#preset-modal-code`, `#btn-close-preset-modal`, `#btn-preset-modal-copy`), `#diagnostic-dialog` (`#diag-modal-title`, `#diagnostic-checks-grid`, `#diagnostic-notes-list`, `#btn-close-diag`), `#command-palette-dialog` (`#palette-title`, `#palette-search-input`, `#btn-close-palette`, `#palette-results`), `#tool-approval-dialog` (`#tool-approval-title`, `#tool-req-summary`, `#tool-req-source`, `#tool-req-name`, `#tool-req-args`, `#btn-tool-deny`, `#btn-tool-once`, `#btn-tool-session`), `#job-logs-dialog` (`#job-logs-title`, `#job-logs-meta`, `#job-logs-pre`, `#btn-refresh-job-logs`, `#btn-copy-job-logs`, `#btn-close-job-logs`), `#add-mcp-dialog` (`#add-mcp-form`, `#add-mcp-title`, `#mcp-name`, `#mcp-namespace`, `#mcp-transport`, `#mcp-http-fields`, `#mcp-url`, `#mcp-bearer`, `#mcp-stdio-fields`, `#mcp-command`, `#mcp-args`, `#btn-cancel-add-mcp`, `#btn-save-add-mcp`) |

### Safe Rebranding & Data Preservation Contract
When rebranding UI labels, window title, tray tooltip, and `tauri.conf.json` (`productName: "Airlock"`):
- **MUST preserve `~/.smart-tauri-hub/` persistence directory** (or read/migrate cleanly before creating new paths):
  - `~/.smart-tauri-hub/config.json` (workspace pins, server port, tunnel & model config)
  - `~/.smart-tauri-hub/secrets.dpapi` (Windows DPAPI encrypted provider API keys)
  - `~/.smart-tauri-hub/auth.json` (MCP bearer token & OAuth clients/tokens — protected by Sensitive File Guard in `security.rs` and `tools.rs`)
  - `~/.smart-tauri-hub/chats/` (per-workspace multi-session chat history)
  - `~/.smart-tauri-hub/snapshots/` & `~/.smart-tauri-hub/audit_history.json` (SHA-256 disk rollback snapshots)
- Avoid breaking existing MCP client configurations (`smart-tauri-hub` serverInfo fallback or configurable alias if external clients already pinned it).

---

## 5. Lightweight GPU Effects & Accessibility Contract
1. **Same-Document View Transitions (`document.startViewTransition`):**
   - Only the active `.view-panel.active` carries `view-transition-name: airlock-view` (ensuring at most one snapshot before and after).
   - Fallback executes immediate DOM update when `startViewTransition` is unavailable.
   - Programmatic focus moves to the active view's heading (`tabindex="-1"`) once `transition.finished` settles.
2. **Top-Layer `<dialog>` Entry & Exit Animations:**
   - Uses `transition-property: opacity, transform, display, overlay;` with separate `transition-behavior: allow-discrete;` and `@starting-style` nested inside `dialog[open]`.
   - Implements `closeTopLayer()` fallback (`data-closing` + `getAnimations()`) for engines lacking `overlay` or discrete `display` transitions.
   - Informational / navigation dialogs (`#command-palette-dialog`, `#traffic-inspector-dialog`, `#add-mcp-dialog`, `#preset-preview-dialog`, `#diagnostic-dialog`, `#job-logs-dialog`) use `closedby="any"` + click-outside fallback.
   - Critical security / destructive confirmation dialogs (`#oauth-consent-dialog`, `#tool-approval-dialog`, `#revert-conflict-dialog`) intentionally keep `closedby="none"` (or require explicit button / `Esc` deny) so users cannot dismiss an authorization decision by misclicking the backdrop.
3. **Sliding Indicator Pills & Card Border Sheen:**
   - Guardrail segmented control and Left Rail active indicator animate purely via `transform` (`translateX` / `translateY` + `scale`) on the compositor thread.
   - Cards use a single delegated `pointermove` handler updating CSS `--spot-x` / `--spot-y` on `.card` only inside `@media (prefers-reduced-motion: no-preference)`.
4. **Floating Toast Stack:**
   - Replaces static statusbar text updates with stacked dismissible bottom-right notifications (`aria-live="polite"`) while keeping `#toast-region` / `#statusbar-meta` synchronized.
5. **Reduced Motion & Contrast Discipline:**
   - Spatial transforms, spotlights, and breathing status dots are gated behind `@media (prefers-reduced-motion: no-preference)`.
   - `@media (prefers-reduced-motion: reduce)` preserves instant clarity and gentle color/opacity feedback (never blanket `* { transition: none !important; }` or `0.01ms` hacks).
   - Minimum `4.5:1` contrast for body/metadata text, `3:1` for UI boundaries, visible `:focus-visible` rings, and `@media (forced-colors: active)` support.
