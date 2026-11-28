import "@fontsource-variable/space-grotesk";
import "@fontsource-variable/geist";
import "@fontsource-variable/jetbrains-mono";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import {
  $,
  activateSubTab,
  copyToClipboard,
  initAccentTheme,
  nextAccentTheme,
  positionGuardrailGlider,
  setAccentTheme,
  setToast,
  switchTab,
  syncRailIndicator,
} from "./dom";
import type {
  AgentChunkV2Payload,
  AgentRunSummary,
  AgentStepV2Payload,
  AgentThoughtV2Payload,
  ArtifactDocument,
  AuditAddedPayload,
  CommandPaletteAction,
  DashboardSnapshot,
  DiagnosticReport,
  GuardrailMode,
  HooksStatusSummary,
  HttpTrafficEntry,
  JobSummary,
  PendingElicitation,
  PendingOAuthInfo,
  PendingToolApproval,
} from "./types";
import {
  appendAgentTextChunkV2,
  appendAgentThoughtChunkV2,
  getActiveSessionId,
  initAgentView,
  isAgentTurnActive,
  renderAgentView,
  renderRunsStrip,
  runProgrammaticTurn,
  setExecutionMode,
  startNewChatSession,
  updateAgentStepStatusV2,
} from "./views/agent";
import {
  handleArtifactUpdatedEvent,
  initArtifactsView,
  renderRunsTab,
  toggleArtifactsPanel,
} from "./views/artifacts";
import { appendAuditIncremental, initAuditView, renderAuditList } from "./views/audit";
import {
  appendTrafficIncremental,
  appendTunnelLogIncremental,
  initBridgeView,
  renderBridgeView,
  updateJobIncremental,
} from "./views/bridge";
import {
  enqueueMcpElicitation,
  enqueueOAuthConsent,
  enqueueToolApproval,
  handleJobUpdatedForModal,
  handleMcpElicitationResolvedEvent,
  handleOAuthResolvedEvent,
  handleToolApprovalResolvedEvent,
  initModals,
  openAddMcpModal,
  openCommandPalette,
  openDiagnosticModal,
  openHooksTrustModal,
} from "./views/modals";
import {
  initPluginsView,
  openCreateSkillModal,
  openInstallPluginModal,
  renderPluginsView,
} from "./views/plugins";
import { initWorkspaceView, renderWorkspaceView } from "./views/workspace";

let currentSnapshot: DashboardSnapshot | null = null;

function getSnap(): DashboardSnapshot | null {
  return currentSnapshot;
}

function renderStatusbarMeta(snap: DashboardSnapshot): void {
  const srv = snap.server_running ? "ONLINE" : "OFFLINE";
  const guard = snap.allow_sensitive_reads ? "OFF" : "ON";
  const builtinCount = snap.builtin_tools_count ?? 28;
  const toolsTotal = builtinCount + (snap.aggregated_tools_count ?? 0);
  $("statusbar-meta").textContent =
    `Port ${snap.server_port} · Server ${srv} · Mode ${snap.guardrail_mode} · Sensitive Guard ${guard} · ${toolsTotal} tools`;
}

function renderSnapshot(snap: DashboardSnapshot): void {
  currentSnapshot = snap;

  document.documentElement.dataset.guardrail = snap.guardrail_mode;
  const sealLabel = document.getElementById("airlock-seal-label");
  if (sealLabel) {
    sealLabel.textContent =
      snap.guardrail_mode === "read_only"
        ? "LOCKED · READ"
        : snap.guardrail_mode === "safe_edit"
          ? "GATED · SAFE"
          : "SEALED · AUTO";
  }

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

  const builtinCount = snap.builtin_tools_count ?? 28;
  const aggCount = snap.aggregated_tools_count ?? 0;
  $("rail-hub-count").textContent = aggCount > 0 ? `${builtinCount}+${aggCount}` : String(builtinCount);
  $("rail-monitor-count").textContent = String((snap.http_traffic ?? []).length);
  $("audit-count-badge").textContent = String(snap.audit_logs.length);

  renderStatusbarMeta(snap);
  renderWorkspaceView(snap, renderSnapshot);
  renderAgentView(snap);
  renderBridgeView(snap, renderSnapshot);
  renderPluginsView(snap, renderSnapshot);
  renderAuditList(snap.audit_logs, getSnap);

  const runs = snap.active_runs ?? [];
  renderRunsStrip(runs);
  renderRunsTab(runs);

  const hooksBtn = $("btn-hooks-trust") as HTMLButtonElement;
  const hs = snap.hooks_status;
  if (hs && hs.hooks_file_exists && !hs.trusted) {
    hooksBtn.hidden = false;
    hooksBtn.textContent = hs.hash_changed ? "Hooks: Changed" : "Hooks: Untrusted";
  } else {
    hooksBtn.hidden = true;
  }

  for (const pending of snap.pending_oauth_requests ?? []) {
    enqueueOAuthConsent(pending);
  }
  for (const pendingTool of snap.pending_tool_approvals ?? []) {
    enqueueToolApproval(pendingTool);
  }
  for (const pendingElic of snap.pending_elicitations ?? []) {
    enqueueMcpElicitation(pendingElic);
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
      id: "accent-blue",
      label: "Accent Theme: Precision Blue",
      hint: "Precision Studio",
      run: () => setAccentTheme("blue"),
    },
    {
      id: "accent-mint",
      label: "Accent Theme: Electric Mint",
      hint: "Precision Studio",
      run: () => setAccentTheme("mint"),
    },
    {
      id: "accent-amber",
      label: "Accent Theme: Solar Amber",
      hint: "Precision Studio",
      run: () => setAccentTheme("amber"),
    },
    {
      id: "accent-mono",
      label: "Accent Theme: Carbon Monochrome",
      hint: "Precision Studio",
      run: () => setAccentTheme("mono"),
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
        activateSubTab("hub", "aggregator");
        openAddMcpModal();
      },
    },
    {
      id: "install-agent-plugin",
      label: "Install Agent Plugin (1.0.0)",
      hint: "Hub & Plugins",
      run: () => {
        switchTab("hub");
        activateSubTab("hub", "plugins");
        openInstallPluginModal();
      },
    },
    {
      id: "rescan-agent-skills",
      label: "Rescan Agent Skills (SKILL.md)",
      hint: "Skills Catalog",
      run: async () => {
        const next = await invoke<DashboardSnapshot>("rescan_skills");
        renderSnapshot(next);
        switchTab("hub");
        activateSubTab("hub", "skills");
        setToast(`Skills dipindai ulang (${next.skills.length} ditemukan).`);
      },
    },
    {
      id: "new-global-skill",
      label: "Buat Global Skill Baru (SKILL.md)",
      hint: "Skills Catalog",
      run: () => {
        switchTab("hub");
        activateSubTab("hub", "skills");
        openCreateSkillModal();
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
    {
      id: "toggle-artifacts-panel",
      label: "Toggle Artifacts & Runs Panel",
      hint: "Local Agent",
      run: () => {
        switchTab("agent");
        toggleArtifactsPanel();
      },
    },
    {
      id: "exec-mode-plan-first",
      label: "Switch Execution Mode -> Plan First",
      hint: "Artifacts Gate",
      run: () => {
        switchTab("agent");
        setExecutionMode("plan_first");
        setToast("Execution Mode: Plan First (butuh approval sebelum edit)");
      },
    },
    {
      id: "exec-mode-direct",
      label: "Switch Execution Mode -> Direct",
      hint: "Fast Coding",
      run: () => {
        switchTab("agent");
        setExecutionMode("direct");
        setToast("Execution Mode: Direct");
      },
    },
    {
      id: "inspect-workspace-hooks",
      label: "Inspect Workspace Lifecycle Hooks (.airlock/hooks.json)",
      hint: "Trust Gate",
      run: async () => {
        const status = await invoke<HooksStatusSummary>("get_workspace_hooks_status");
        openHooksTrustModal(status, renderSnapshot);
      },
    },
  ];

  actions.push({
    id: "add-mounted-workspace",
    label: "Add Mounted Workspace Folder...",
    hint: "Multi-Workspace",
    run: async () => {
      const picked = await open({
        directory: true,
        multiple: false,
        title: "Tambah Mounted Workspace Folder",
      });
      if (typeof picked === "string") {
        const next = await invoke<DashboardSnapshot>("add_workspace", {
          path: picked,
          alias: null,
          guardrailOverride: null,
          setDefault: false,
        });
        renderSnapshot(next);
        setToast(`Mounted: ${picked}`);
      }
    },
  });

  for (const ws of snap?.workspaces ?? []) {
    if (ws.enabled && !ws.is_default) {
      actions.push({
        id: `switch-default-ws-${ws.id}`,
        label: `Switch Default Workspace -> @${ws.alias}`,
        hint: ws.path,
        run: async () => {
          const next = await invoke<DashboardSnapshot>("set_default_workspace", { id: ws.id });
          renderSnapshot(next);
          setToast(`Default workspace: @${ws.alias}`);
        },
      });
    }
  }

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

function buildWindowMaxIcon(maximized: boolean): SVGSVGElement {
  const svgNs = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(svgNs, "svg");
  svg.setAttribute("width", "11");
  svg.setAttribute("height", "11");
  svg.setAttribute("viewBox", "0 0 12 12");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.2");
  svg.setAttribute("aria-hidden", "true");

  if (maximized) {
    const front = document.createElementNS(svgNs, "rect");
    front.setAttribute("x", "2");
    front.setAttribute("y", "4");
    front.setAttribute("width", "6");
    front.setAttribute("height", "6");
    front.setAttribute("rx", "0.8");
    const back = document.createElementNS(svgNs, "path");
    back.setAttribute("d", "M4 4V2.5a.5.5 0 0 1 .5-.5H9.5a.5.5 0 0 1 .5.5V7.5a.5.5 0 0 1-.5.5H8");
    svg.append(back, front);
  } else {
    const rect = document.createElementNS(svgNs, "rect");
    rect.setAttribute("x", "2");
    rect.setAttribute("y", "2");
    rect.setAttribute("width", "8");
    rect.setAttribute("height", "8");
    rect.setAttribute("rx", "1");
    svg.appendChild(rect);
  }
  return svg;
}

function initFramelessWindowControls(): void {
  const controls = document.getElementById("window-controls");
  const isTauri = Boolean(
    (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
  );
  if (!controls || !isTauri) return;

  controls.hidden = false;
  const win = getCurrentWindow();
  const minBtn = $("win-btn-min");
  const maxBtn = $("win-btn-max");
  const closeBtn = $("win-btn-close");
  const spacer = document.querySelector<HTMLElement>(".cmdbar-spacer");

  const syncMaxState = async (): Promise<void> => {
    try {
      const max = await win.isMaximized();
      maxBtn.setAttribute("aria-label", max ? "Restore window" : "Maximize window");
      maxBtn.title = max ? "Restore" : "Maximize";
      maxBtn.replaceChildren(buildWindowMaxIcon(max));
    } catch {
      // ignore outside native window context
    }
  };

  minBtn.addEventListener("click", () => {
    void win.minimize().catch(() => {});
  });
  maxBtn.addEventListener("click", () => {
    void win.toggleMaximize().then(() => syncMaxState()).catch(() => {});
  });
  closeBtn.addEventListener("click", () => {
    void win.close().catch(() => {});
  });
  spacer?.addEventListener("dblclick", () => {
    void win.toggleMaximize().then(() => syncMaxState()).catch(() => {});
  });

  void syncMaxState();
  void win.onResized(() => {
    void syncMaxState();
  });
}

async function init(): Promise<void> {
  initAccentTheme();
  initFramelessWindowControls();

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
    setAccentTheme(nextAccentTheme(document.documentElement.dataset.accent));
  });

  $("statusbar-jobs-pill").addEventListener("click", () => {
    switchTab("monitor");
    document
      .getElementById("jobs-monitor-card")
      ?.scrollIntoView({ behavior: "smooth", block: "nearest" });
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

  const railNewChat = document.getElementById("btn-rail-new-chat");
  railNewChat?.addEventListener("click", async () => {
    switchTab("agent");
    await startNewChatSession();
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

  $("btn-hooks-trust").addEventListener("click", async () => {
    try {
      const status =
        currentSnapshot?.hooks_status ??
        (await invoke<HooksStatusSummary>("get_workspace_hooks_status"));
      openHooksTrustModal(status, renderSnapshot);
    } catch (err) {
      setToast(`Gagal membaca status hooks: ${String(err)}`, "warn");
    }
  });

  initModals(renderSnapshot, buildPaletteActions);
  initWorkspaceView(getSnap, renderSnapshot);
  initBridgeView(getSnap, renderSnapshot, refreshDashboard);
  initPluginsView(getSnap, renderSnapshot);
  initAuditView(getSnap, refreshDashboard);
  initArtifactsView({
    getSessionId: getActiveSessionId,
    onRunPrompt: runProgrammaticTurn,
  });
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
          const toggleBtn = $("btn-recent-workspaces");
          toggleBtn.setAttribute("aria-expanded", "false");
          toggleBtn.focus();
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

  const isTauri = Boolean(
    (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
  );

  if (isTauri) {
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

    await listen<PendingElicitation>("mcp-elicitation-request", (event) => {
      enqueueMcpElicitation(event.payload);
    });

    await listen<string>("mcp-elicitation-resolved", (event) => {
      handleMcpElicitationResolvedEvent(event.payload);
    });

    await listen<JobSummary>("job-updated", (event) => {
      updateJobIncremental(currentSnapshot, event.payload, renderSnapshot);
      handleJobUpdatedForModal(event.payload);
    });

    await listen<AgentStepV2Payload>("agent-step-v2", (event) => {
      updateAgentStepStatusV2(event.payload);
    });

    await listen<AgentThoughtV2Payload>("agent-thought-v2", (event) => {
      appendAgentThoughtChunkV2(event.payload);
    });

    await listen<AgentChunkV2Payload>("agent-chunk-v2", (event) => {
      appendAgentTextChunkV2(event.payload);
    });

    await listen<ArtifactDocument>("artifact-updated", (event) => {
      handleArtifactUpdatedEvent(event.payload);
    });

    await listen<AgentRunSummary[]>("agent-runs-updated", (event) => {
      renderRunsStrip(event.payload);
      renderRunsTab(event.payload);
    });

    await listen<HooksStatusSummary>("hooks-trust-required", (event) => {
      const hooksBtn = $("btn-hooks-trust") as HTMLButtonElement;
      hooksBtn.hidden = false;
      hooksBtn.textContent = event.payload.hash_changed ? "Hooks: Changed" : "Hooks: Untrusted";
      setToast(
        `Hooks @${event.payload.workspace_alias} belum di-trust. Klik tombol Hooks untuk inspeksi.`,
        "warn"
      );
    });

    await refreshDashboard();
  }
  requestAnimationFrame(() => {
    positionGuardrailGlider();
    syncRailIndicator();
  });
}

void init();
