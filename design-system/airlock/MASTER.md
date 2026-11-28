# Airlock — Precision Studio v3 Design System (`MASTER.md`)

## 1. Product Identity & Character Anchor
- **Product Name:** **Airlock** *(Precision Studio v3)*
- **Character Anchor:** *Quiet Chrome, Sharp Signal* — a precision minimalist dark desktop developer airlock combining a Linear-inspired calm Local Agent studio with a collapsible Zed-style Artifacts & Plan Inspector, plus structured sub-tabbed workspaces for `MCP Bridge`, `Hub & Tools`, `Live Monitor`, and `Diff & Audit`.
- **Visual Philosophy:**
  - Calm charcoal/graphite surfaces (`#0C0D10`–`#1A1D24`) separated primarily by tonal gradation and crisp `1px` structural hairlines (`#1E212B` / `#242833`) — zero dot-grid noise, zero purple/indigo glows, zero gratuitous glassmorphism, zero cursor border-sheen scripts, and zero emoji icons.
  - Native Windows frameless window chrome (`decorations: false`, `shadow: true`) merged seamlessly into a slim `40px` Top Command Bar.
  - Strict Vanilla TypeScript + native HTML/CSS/SVG/WebView2 platform APIs (`@layer reset, tokens, shell, components, views, overlays;`, `<dialog>`, `[popover]`, View Transitions).

---

## 2. Color, Typography & Surface Specification
All tokens live in [`src/tokens.css`](../../src/tokens.css) (mirrored in [`tokens.css`](./tokens.css)).

### Surface & Border Hierarchy
| Token | Hex / Value | Role |
| :--- | :--- | :--- |
| `--bg-canvas` / `--bg-rail` | `#0C0D10` | Root window chrome, `40px` topbar, `220px` left rail, and docked inspector drawers |
| `--bg-surface` | `#13151A` | Primary workspace viewport canvas |
| `--bg-elevated` | `#1A1D24` | Unified floating composer dock, secondary controls, active rail item |
| `--bg-code` | `#0A0B0E` | Code blocks, terminal logs, split/unified diff viewer, and hairline tables |
| `--border-subtle` | `#1E212B` | Structural `1px` dividers and resting hairlines |
| `--border-default` | `#242833` | Interactive control borders |
| `--border-strong` | `#323846` | Hovered control borders |

### 4 Signature Accent Variants (`Precision Blue` Default)
| Variant | Selector | `--accent` | `--accent-hover` | `--accent-fg` | `--on-accent` |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Precision Blue** *(Default)* | `:root` / `[data-accent="blue"]` | `#3B82F6` | `#2563EB` | `#60A5FA` | `#F8FAFC` |
| **Electric Mint** | `:root[data-accent="mint"]` | `#10B981` | `#059669` | `#34D399` | `#031A12` |
| **Solar Amber** | `:root[data-accent="amber"]` | `#F59E0B` | `#D97706` | `#FBBF24` | `#1C1002` |
| **Carbon Monochrome** | `:root[data-accent="mono"]` | `#E4E4E7` | `#D4D4D8` | `#FAFAFA` | `#0C0D10` |

- Semantic status hues (`--status-ok: #34D399`, `--status-warn: #F59E0B`, `--status-info: #60A5FA`, `--status-danger: #F87171`) are strictly separated from primary interactive accents and tinted via `color-mix(in oklab, ...)`.

### Typography (100% Offline Bundled)
- **Display / Headings (`--font-display`):** `Space Grotesk Variable` (`-0.02em` tracking, weight 600).
- **UI Body (`--font-ui`):** `Geist Variable` (`13.5px` base, line-height `1.5`).
- **Technical Mono (`--font-mono`):** `JetBrains Mono Variable` with `font-variant-numeric: tabular-nums` enforced on counters, ports, timestamps, durations, and diffs.

---

## 3. Five-View Precision Architecture
1. **Top Command Bar (`40px`):** Compact `@workspace · main` switcher pill (`#btn-recent-workspaces` opening `#workspace-dropdown` with jailed path editor & multi-workspace manager), sliding Guardrail switcher (`Read` / `Safe` / `Auto`), `Commands Ctrl+K`, and native frameless window controls.
2. **Left Sidebar Rail (`220px` ↔ `58px`):** `+ New Chat`, clean 5-view navigation, integrated `Recent Sessions` list (`#rail-sessions-list`), and quiet 3-row telemetry footer (`Server`, `Tunnel`, `Tokens Saved`, `Turns Saved`).
3. **View 1 — Local Agent (`#panel-agent`):** Slim `42px` contextual sub-header, centered `780px` conversation stream, unified single-toolbar `.chat-composer` (`@File` inline popover, `Image`, `/Workflow`, `Direct | Plan`, `Run`), and docked hairline Zed-style `#artifacts-panel`.
4. **View 2 — MCP Bridge & Security (`#panel-bridge`):** Compact 1-row `#airlock-signal-pipeline` conduit + 4 underline sub-tabs (`Connection & Tunnel` 2-column split with live inline preset JSON preview, `Security & Guardrails`, `WebMCP Tuning`, `OAuth Clients`).
5. **View 3 — Hub & Tools (`#panel-hub`):** 3 underline sub-tabs (`Downstream Aggregator` borderless hairline table + split `Smart Tool Simulator & Testbench`, `Skills Catalog`, `Installed Plugins`).
6. **View 4 — Live Monitor (`#panel-monitor`):** Single horizontal KPI + 24-bar SVG histogram sparkline band, hairline `HTTP Traffic` table, and 2-column bottom split (`Background Command Jobs` + `Active LSP Servers`).
7. **View 5 — Diff & Audit (`#panel-audit`):** Full-height 2-pane hairline workbench (`300px` `Audit Trail` + `Visual Diff Inspector` with single-line non-wrapping controls and `Unified | Split` diff viewer).
