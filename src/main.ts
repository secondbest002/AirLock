import "@fontsource-variable/plus-jakarta-sans";
import "@fontsource-variable/jetbrains-mono";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  $,
  copyToClipboard,
  initAccentTheme,
  positionGuardrailGlider,
  setAccentTheme,
  setToast,
  switchTab,
  syncRailIndicator,
} from "./dom";
import type {
  AuditAddedPayload,
  CommandPaletteAction,
  DashboardSnapshot,
  DiagnosticReport,
  GuardrailMode,
  HttpTrafficEntry,
  JobSummary,
  PendingOAuthInfo,
  PendingToolApproval,
} from "./types";
import {
  appendAgentTextChunk,
  appendAgentThoughtChunk,
  initAgentView,
  isAgentTurnActive,
  renderAgentView,
  startNewChatSession,
} from "./views/agent";
import { appendAuditIncremental, initAuditView, renderAuditList } from "./views/audit";
import {
  appendTrafficIncremental,
  appendTunnelLogIncremental,
  initBridgeView,
  renderBridgeView,
  updateJobIncremental,
} from "./views/bridge";
import {
  enqueueOAuthConsent,
  enqueueToolApproval,
  handleOAuthResolvedEvent,
  handleToolApprovalResolvedEvent,
  initModals,
  openAddMcpModal,
  openCommandPalette,
  openDiagnosticModal,
} from "./views/modals";
import { initWorkspaceView, renderWorkspaceView } from "./views/workspace";

let currentSnapshot: DashboardSnapshot | null = null;

function getSnap(): DashboardSnapshot | null {
  return currentSnapshot;
}

function renderStatusbarMeta(snap: DashboardSnapshot): void {
  const srv = snap.server_running ? "ONLINE" : "OFFLINE";
  const guard = snap.allow_sensitive_reads ? "OFF" : "ON";
  const toolsTotal = 14 + (snap.aggregated_tools_count ?? 0);
  $("statusbar-meta").textContent =
    `Port ${snap.server_port} · Server ${srv} · Mode ${snap.guardrail_mode} · Sensitive Guard ${guard} · ${toolsTotal} tools`;
}

function renderSnapshot(snap: DashboardSnapshot): void {
  currentSnapshot = snap;

  const track = $("guardrail-track");
  track.dataset.mode = snap.guardrail_mode;
  document.querySelectorAll<HTMLButtonElement>(".guardrail-btn").forEach((btn) => {
    const active = btn.dataset.guardrail === snap.guardrail_mode;
    btn.classList.toggle("active", active);
    btn.setAttribute("aria-pressed", active ? "true" : "false");
  });
  positionGuardrailGlider();

  $("rail-server-dot").className = `live-dot ${snap.server_running ? "online" : "offline"}`;
  $("rail-server-label").textContent = snap.server_running ? "Server Online" : "Server Offline";
  $("status-server-badge").textContent = snap.server_running ? `:${snap.server_port}` : "OFF";
  $("rail-port-pill").textContent = `:${snap.server_port}`;

  const bridgePill = $("bridge-server-pill");
  bridgePill.textContent = snap.server_running ? "ONLINE" : "OFFLINE";
  bridgePill.className = `badge ${snap.server_running ? "badge-ok" : "badge-muted"}`;

  const tunnelBadge = $("status-tunnel-badge");
  if (snap.tunnel_running) {
    tunnelBadge.textContent = snap.tunnel_url
      ? snap.tunnel_provider === "tailscale"
        ? "Tailscale"
        : "Cloudflare"
      : "Starting";
    tunnelBadge.className = `telemetry-val ${snap.tunnel_url ? "accent" : ""}`;
  } else {
    tunnelBadge.textContent = "Idle";
    tunnelBadge.className = "telemetry-val";
  }

  $("metric-tokens-saved").textContent = snap.metrics.tokens_saved_total.toLocaleString("en-US");
  $("metric-turns-saved").textContent = snap.metrics.turns_saved_total.toLocaleString("en-US");

  const aggCount = snap.aggregated_tools_count ?? 0;
  $("rail-hub-count").textContent = aggCount > 0 ? `14+${aggCount}` : "14";
  $("rail-monitor-count").textContent = String((snap.http_traffic ?? []).length);
  $("audit-count-badge").textContent = String(snap.audit_logs.length);

  renderStatusbarMeta(snap);
  renderWorkspaceView(snap, renderSnapshot);
  renderAgentView(snap);
  renderBridgeView(snap, renderSnapshot);
  renderAuditList(snap.audit_logs, getSnap);

  for (const pending of snap.pending_oauth_requests ?? []) {
    enqueueOAuthConsent(pending);
  }
  for (const pendingTool of snap.pending_tool_approvals ?? []) {
    enqueueToolApproval(pendingTool);
  }
}

async function refreshDashboard(): Promise<void> {
  const snap = await invoke<DashboardSnapshot>("get_dashboard_state");
  renderSnapshot(snap);
}

function toggleRail(): void {
  const shell = $("app-shell");
  const collapsed = shell.classList.toggle("rail-collapsed");
  $("btn-toggle-rail").setAttribute("aria-expanded", collapsed ? "false" : "true");
  try {
    localStorage.setItem("airlock_rail_collapsed", collapsed ? "1" : "0");
  } catch {
    // ignore storage errors
  }
  requestAnimationFrame(syncRailIndicator);
}

function buildPaletteActions(): CommandPaletteAction[] {
  const snap = currentSnapshot;
  const actions: CommandPaletteAction[] = [
    {
      id: "tab-agent",
      label: "Buka View: Local Agent",
      hint: "Ctrl+1",
      run: () => switchTab("agent"),
    },
    {
      id: "tab-bridge",
      label: "Buka View: MCP Bridge & Security",
      hint: "Ctrl+2",
      run: () => switchTab("bridge"),
    },
    {
      id: "tab-hub",
      label: "Buka View: Hub & Smart Tools",
      hint: "Ctrl+3",
      run: () => switchTab("hub"),
    },
    {
      id: "tab-monitor",
      label: "Buka View: Live Monitor",
      hint: "Ctrl+4",
      run: () => switchTab("monitor"),
    },
    {
      id: "tab-audit",
      label: "Buka View: Diff & Audit Log",
      hint: "Ctrl+5",
      run: () => switchTab("audit"),
    },
    {
      id: "toggle-rail",
      label: "Toggle Left Sidebar Rail",
      hint: "[",
      run: () => toggleRail(),
    },
    {
      id: "accent-mint",
      label: "Accent Theme: Electric Mint",
      hint: "Obsidian",
      run: () => setAccentTheme("mint"),
    },
    {
      id: "accent-cyan",
      label: "Accent Theme: Ice Cyan",
      hint: "Obsidian",
      run: () => setAccentTheme("cyan"),
    },
    {
      id: "toggle-server",
      label: snap?.server_running ? "Stop MCP Server" : "Start MCP Server",
      hint: `Port ${snap?.server_port ?? 9494}`,
      run: async () => {
        const next = await invoke<DashboardSnapshot>(
          snap?.server_running ? "stop_mcp_server" : "start_mcp_server"
        );
        renderSnapshot(next);
      },
    },
    {
      id: "toggle-tunnel",
      label: snap?.tunnel_running ? "Stop Remote Tunnel" : "Start Remote Tunnel",
      hint: snap?.tunnel_provider ?? "cloudflare",
      run: async () => {
        const provider = ($("tunnel-provider-select") as HTMLSelectElement).value;
        const next = await invoke<DashboardSnapshot>(
          snap?.tunnel_running ? "stop_tunnel" : "start_tunnel",
          snap?.tunnel_running ? {} : { provider }
        );
        renderSnapshot(next);
      },
    },
    {
      id: "self-diag",
      label: "Jalankan MCP Self-Diagnostic",
      hint: "Health + Initialize + Tools",
      run: async () => {
        const report = await invoke<DiagnosticReport>("run_self_diagnostic");
        openDiagnosticModal(report);
      },
    },
    {
      id: "copy-mcp-url",
      label: "Copy MCP Endpoint + Token",
      hint: "Clipboard",
      run: async () => {
        if (!currentSnapshot) return;
        const base =
          currentSnapshot.tunnel_url ?? `http://127.0.0.1:${currentSnapshot.server_port}`;
        await copyToClipboard(`${base}/mcp/${currentSnapshot.auth_token}`, "MCP Endpoint + Token");
      },
    },
    {
      id: "open-ws-vscode",
      label: "Buka Workspace Aktif di VS Code",
      hint: currentSnapshot?.workspace_root ?? "",
      run: async () => {
        const msg = await invoke<string>("open_in_editor", {
          relPath: null,
          mode: "vscode",
        });
        setToast(msg);
      },
    },
    {
      id: "open-ws-explorer",
      label: "Buka Workspace Aktif di Explorer",
      hint: "File Manager",
      run: async () => {
        const msg = await invoke<string>("open_in_editor", {
          relPath: null,
          mode: "explorer",
        });
        setToast(msg);
      },
    },
    {
      id: "guardrail-read-only",
      label: "Guardrail Mode: Read-Only",
      hint: "Blokir semua mutasi",
      run: async () => {
        const next = await invoke<DashboardSnapshot>("set_guardrail_mode", {
          mode: "read_only",
        });
        renderSnapshot(next);
        setToast("Guardrail Mode: Read-Only.");
      },
    },
    {
      id: "guardrail-safe-edit",
      label: "Guardrail Mode: Safe Edit (HITL)",
      hint: "Popup konfirmasi shell/delete/commit",
      run: async () => {
        const next = await invoke<DashboardSnapshot>("set_guardrail_mode", {
          mode: "safe_edit",
        });
        renderSnapshot(next);
        setToast("Guardrail Mode: Safe Edit.", "warn");
      },
    },
    {
      id: "guardrail-autonomous",
      label: "Guardrail Mode: Autonomous",
      hint: "Eksekusi otomatis di jailed workspace",
      run: async () => {
        const next = await invoke<DashboardSnapshot>("set_guardrail_mode", {
          mode: "autonomous",
        });
        renderSnapshot(next);
        setToast("Guardrail Mode: Autonomous.");
      },
    },
    {
      id: "add-downstream-mcp",
      label: "Tambah Downstream MCP Server",
      hint: "Hub & Tools",
      run: () => {
        switchTab("hub");
        openAddMcpModal();
      },
    },
    {
      id: "new-chat-session",
      label: "Mulai Sesi Chat Local Agent Baru",
      hint: "Local Agent",
      run: async () => {
        switchTab("agent");
        await startNewChatSession();
      },
    },
  ];

  for (const ws of snap?.pinned_workspaces ?? []) {
    actions.push({
      id: `switch-pin-${ws}`,
      label: `Switch Workspace (Pinned): ${ws}`,
      hint: "Workspace",
      run: async () => {
        const next = await invoke<DashboardSnapshot>("set_workspace_root", { path: ws });
        renderSnapshot(next);
      },
    });
  }

  return actions;
}

async function init(): Promise<void> {
  initAccentTheme();

  try {
    if (localStorage.getItem("airlock_rail_collapsed") === "1") {
      $("app-shell").classList.add("rail-collapsed");
      $("btn-toggle-rail").setAttribute("aria-expanded", "false");
    }
  } catch {
    // ignore storage errors
  }

  $("btn-toggle-rail").addEventListener("click", toggleRail);

  $("btn-toggle-accent").addEventListener("click", () => {
    const cur = document.documentElement.dataset.accent === "cyan" ? "cyan" : "mint";
    setAccentTheme(cur === "cyan" ? "mint" : "cyan");
  });

  $("rail-server-toggle").addEventListener("click", async () => {
    try {
      const running = currentSnapshot?.server_running ?? false;
      const next = await invoke<DashboardSnapshot>(
        running ? "stop_mcp_server" : "start_mcp_server"
      );
      renderSnapshot(next);
      setToast(
        next.server_running
          ? `MCP Server aktif di 127.0.0.1:${next.server_port}`
          : "MCP Server dimatikan.",
        next.server_running ? "info" : "offline"
      );
    } catch (err) {
      setToast(`Error server: ${String(err)}`, "warn");
    }
  });

  // Delegated card spotlight sheen
  document.addEventListener("pointermove", (e) => {
    const card = e.target instanceof Element ? e.target.closest<HTMLElement>(".card") : null;
    if (!card) return;
    const rect = card.getBoundingClientRect();
    card.style.setProperty("--spot-x", `${e.clientX - rect.left}px`);
    card.style.setProperty("--spot-y", `${e.clientY - rect.top}px`);
  });

  document.querySelectorAll<HTMLButtonElement>(".rail-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      if (btn.dataset.tab) switchTab(btn.dataset.tab);
    });
  });

  document.querySelectorAll<HTMLButtonElement>(".guardrail-btn").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const mode = btn.dataset.guardrail as GuardrailMode | undefined;
      if (!mode) return;
      try {
        const next = await invoke<DashboardSnapshot>("set_guardrail_mode", { mode });
        renderSnapshot(next);
        setToast(`Guardrail Mode: ${mode}`, mode === "safe_edit" ? "warn" : "info");
      } catch (err) {
        setToast(`Gagal mengubah guardrail mode: ${String(err)}`, "warn");
      }
    });
  });

  $("btn-open-palette").addEventListener("click", () => openCommandPalette());

  initModals(renderSnapshot, buildPaletteActions);
  initWorkspaceView(getSnap, renderSnapshot);
  initBridgeView(getSnap, renderSnapshot, refreshDashboard);
  initAuditView(getSnap, refreshDashboard);
  await initAgentView(renderSnapshot);

  document.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      openCommandPalette();
      return;
    }
    const tabMap: Record<string, string> = {
      "1": "agent",
      "2": "bridge",
      "3": "hub",
      "4": "monitor",
      "5": "audit",
    };
    if ((e.ctrlKey || e.metaKey) && tabMap[e.key]) {
      e.preventDefault();
      switchTab(tabMap[e.key]);
      return;
    }
    const activeTag = document.activeElement?.tagName;
    if (
      e.key === "[" &&
      activeTag !== "INPUT" &&
      activeTag !== "TEXTAREA" &&
      activeTag !== "SELECT"
    ) {
      toggleRail();
      return;
    }
    if (e.key === "Escape") {
      const anyDialogOpen = Array.from(document.querySelectorAll("dialog")).some((d) => d.open);
      if (!anyDialogOpen) {
        const wsDropdown = $("workspace-dropdown");
        if (!wsDropdown.hidden) {
          wsDropdown.hidden = true;
          $("btn-recent-workspaces").setAttribute("aria-expanded", "false");
          return;
        }
        if (isAgentTurnActive()) {
          void invoke("cancel_local_agent").catch(() => {});
        }
      }
    }
  });

  window.addEventListener("resize", () => {
    positionGuardrailGlider();
    syncRailIndicator();
  });

  await listen<DashboardSnapshot>("dashboard-updated", (event) => {
    renderSnapshot(event.payload);
  });

  await listen<HttpTrafficEntry>("http-traffic-added", (event) => {
    appendTrafficIncremental(currentSnapshot, event.payload);
  });

  await listen<AuditAddedPayload>("audit-added", (event) => {
    appendAuditIncremental(currentSnapshot, event.payload, getSnap);
  });

  await listen<string>("tunnel-log-added", (event) => {
    appendTunnelLogIncremental(currentSnapshot, event.payload);
  });

  await listen<PendingOAuthInfo>("oauth-consent-request", (event) => {
    enqueueOAuthConsent(event.payload);
  });

  await listen<string>("oauth-consent-resolved", (event) => {
    handleOAuthResolvedEvent(event.payload);
  });

  await listen<PendingToolApproval>("tool-approval-request", (event) => {
    enqueueToolApproval(event.payload);
  });

  await listen<string>("tool-approval-resolved", (event) => {
    handleToolApprovalResolvedEvent(event.payload);
  });

  await listen<JobSummary>("job-updated", (event) => {
    updateJobIncremental(currentSnapshot, event.payload, renderSnapshot);
  });

  await listen<string>("agent-step", (event) => {
    $("agent-run-status").textContent = event.payload;
  });

  await listen<string>("agent-thought-chunk", (event) => {
    appendAgentThoughtChunk(event.payload);
  });

  await listen<string>("agent-text-chunk", (event) => {
    appendAgentTextChunk(event.payload);
  });

  await refreshDashboard();
  requestAnimationFrame(() => {
    positionGuardrailGlider();
    syncRailIndicator();
  });
}

void init();
