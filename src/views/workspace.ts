import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { $, copyToClipboard, setToast } from "../dom";
import type { DashboardSnapshot, GuardrailMode } from "../types";

let pendingAliasDraft = "";

function samePath(a: string, b: string): boolean {
  return a.trim().toLowerCase() === b.trim().toLowerCase();
}

function buildSectionHeader(title: string): HTMLDivElement {
  const hdr = document.createElement("div");
  hdr.className = "ws-dropdown-hdr";
  hdr.textContent = title;
  return hdr;
}

function renderWorkspaceDropdown(
  snap: DashboardSnapshot,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const dropdown = $("workspace-dropdown");
  dropdown.replaceChildren();

  const mounted = snap.workspaces ?? [];
  const enabledCount = mounted.filter((w) => w.enabled).length;

  dropdown.appendChild(buildSectionHeader(`MOUNTED WORKSPACES (${mounted.length})`));

  if (mounted.length === 0) {
    const empty = document.createElement("div");
    empty.className = "ws-dropdown-empty";
    empty.textContent = "Belum ada workspace terdaftar.";
    dropdown.appendChild(empty);
  } else {
    for (const ws of mounted) {
      const card = document.createElement("div");
      card.className = `ws-mounted-card ${ws.is_default ? "is-default" : ""} ${
        ws.enabled ? "" : "is-disabled"
      }`.trim();

      const meta = document.createElement("div");
      meta.className = "ws-mounted-meta";

      const aliasPill = document.createElement("span");
      aliasPill.className = "ws-alias-pill";
      aliasPill.textContent = `@${ws.alias}`;

      const pathSpan = document.createElement("span");
      pathSpan.className = "ws-mounted-path";
      pathSpan.textContent = ws.path;
      pathSpan.title = ws.path;

      meta.append(aliasPill, pathSpan);

      if (ws.is_default) {
        const defBadge = document.createElement("span");
        defBadge.className = "badge badge-ok";
        defBadge.textContent = "default";
        meta.appendChild(defBadge);
      } else if (!ws.exists) {
        const missBadge = document.createElement("span");
        missBadge.className = "badge badge-muted";
        missBadge.textContent = "missing";
        meta.appendChild(missBadge);
      } else if (!ws.enabled) {
        const disBadge = document.createElement("span");
        disBadge.className = "badge badge-muted";
        disBadge.textContent = "disabled";
        meta.appendChild(disBadge);
      }

      const actions = document.createElement("div");
      actions.className = "ws-mounted-actions";

      const defBtn = document.createElement("button");
      defBtn.type = "button";
      defBtn.className = "btn btn-secondary btn-xs";
      defBtn.textContent = ws.is_default ? "Default" : "Set Default";
      defBtn.disabled = ws.is_default;
      defBtn.setAttribute("aria-label", `Set @${ws.alias} as default workspace`);
      defBtn.addEventListener("click", async () => {
        try {
          const next = await invoke<DashboardSnapshot>("set_default_workspace", { id: ws.id });
          onSnapshot(next);
          setToast(`Default workspace: @${ws.alias}`);
        } catch (err) {
          setToast(`Gagal mengubah default workspace: ${String(err)}`);
        }
      });

      const guardSel = document.createElement("select");
      guardSel.className = "mono-select ws-guard-select";
      guardSel.setAttribute("aria-label", `Guardrail override for @${ws.alias}`);
      const guardOptions: Array<{ value: string; label: string }> = [
        { value: "inherit", label: `Inherit (${snap.guardrail_mode})` },
        { value: "read_only", label: "Read-Only" },
        { value: "safe_edit", label: "Safe Edit" },
        { value: "autonomous", label: "Autonomous" },
      ];
      for (const optDef of guardOptions) {
        const opt = document.createElement("option");
        opt.value = optDef.value;
        opt.textContent = optDef.label;
        guardSel.appendChild(opt);
      }
      guardSel.value = ws.guardrail_override ?? "inherit";
      guardSel.addEventListener("change", async () => {
        const chosen = guardSel.value as "inherit" | GuardrailMode;
        try {
          const next = await invoke<DashboardSnapshot>("update_workspace", {
            id: ws.id,
            alias: null,
            enabled: null,
            guardrailOverride: chosen,
            allowSensitiveOverride: null,
            pinned: null,
          });
          onSnapshot(next);
          setToast(`Guardrail @${ws.alias}: ${chosen}`);
        } catch (err) {
          setToast(`Gagal update guardrail @${ws.alias}: ${String(err)}`);
        }
      });

      const toggleBtn = document.createElement("button");
      toggleBtn.type = "button";
      toggleBtn.className = "btn btn-secondary btn-xs";
      toggleBtn.textContent = ws.enabled ? "Disable" : "Enable";
      toggleBtn.disabled = ws.enabled && enabledCount <= 1;
      toggleBtn.setAttribute(
        "aria-label",
        ws.enabled ? `Disable workspace @${ws.alias}` : `Enable workspace @${ws.alias}`
      );
      toggleBtn.addEventListener("click", async () => {
        try {
          const next = await invoke<DashboardSnapshot>("update_workspace", {
            id: ws.id,
            alias: null,
            enabled: !ws.enabled,
            guardrailOverride: null,
            allowSensitiveOverride: null,
            pinned: null,
          });
          onSnapshot(next);
          setToast(`Workspace @${ws.alias} ${!ws.enabled ? "diaktifkan" : "dinonaktifkan"}.`);
        } catch (err) {
          setToast(`Gagal toggle @${ws.alias}: ${String(err)}`);
        }
      });

      const copyUrlBtn = document.createElement("button");
      copyUrlBtn.type = "button";
      copyUrlBtn.className = "btn btn-secondary btn-xs";
      copyUrlBtn.textContent = "Copy URL";
      copyUrlBtn.setAttribute("aria-label", `Copy scoped MCP URL for @${ws.alias}`);
      copyUrlBtn.title = `Salin URL MCP terkunci ke @${ws.alias} (?workspace=${ws.alias})`;
      copyUrlBtn.addEventListener("click", async () => {
        const base = snap.tunnel_url ?? `http://127.0.0.1:${snap.server_port}`;
        const url = `${base}/mcp/${snap.auth_token}?workspace=${encodeURIComponent(ws.alias)}`;
        await copyToClipboard(url, `MCP URL (@${ws.alias})`);
      });

      const rmBtn = document.createElement("button");
      rmBtn.type = "button";
      rmBtn.className = "btn btn-danger btn-xs";
      rmBtn.textContent = "Remove";
      rmBtn.disabled = mounted.length <= 1;
      rmBtn.setAttribute("aria-label", `Remove workspace @${ws.alias}`);
      rmBtn.addEventListener("click", async () => {
        try {
          const next = await invoke<DashboardSnapshot>("remove_workspace", { id: ws.id });
          onSnapshot(next);
          setToast(`Workspace @${ws.alias} dilepas dari registry.`);
        } catch (err) {
          setToast(`Gagal melepas @${ws.alias}: ${String(err)}`);
        }
      });

      actions.append(defBtn, guardSel, toggleBtn, copyUrlBtn, rmBtn);
      card.append(meta, actions);
      dropdown.appendChild(card);
    }
  }

  // Mount new workspace section
  dropdown.appendChild(buildSectionHeader("MOUNT NEW WORKSPACE"));
  const addRow = document.createElement("div");
  addRow.className = "ws-add-row";

  const aliasLabel = document.createElement("label");
  aliasLabel.className = "sr-only";
  aliasLabel.setAttribute("for", "ws-add-alias-input");
  aliasLabel.textContent = "Alias workspace opsional";

  const aliasInput = document.createElement("input");
  aliasInput.id = "ws-add-alias-input";
  aliasInput.type = "text";
  aliasInput.className = "mono-input";
  aliasInput.placeholder = "alias opsional (misal: api, web)";
  aliasInput.setAttribute("autocomplete", "off");
  aliasInput.setAttribute("autocorrect", "off");
  aliasInput.setAttribute("autocapitalize", "none");
  aliasInput.spellcheck = false;
  aliasInput.value = pendingAliasDraft;
  aliasInput.addEventListener("input", () => {
    pendingAliasDraft = aliasInput.value;
  });

  const addBtn = document.createElement("button");
  addBtn.type = "button";
  addBtn.className = "btn btn-primary btn-xs";
  addBtn.textContent = "+ Add Folder...";
  addBtn.addEventListener("click", async () => {
    try {
      const picked = await open({
        directory: true,
        multiple: false,
        title: "Tambah Mounted Workspace Folder",
      });
      if (typeof picked === "string") {
        const aliasArg = pendingAliasDraft.trim() || null;
        const next = await invoke<DashboardSnapshot>("add_workspace", {
          path: picked,
          alias: aliasArg,
          guardrailOverride: null,
          setDefault: false,
        });
        pendingAliasDraft = "";
        onSnapshot(next);
        setToast(`Workspace ditambahkan: ${picked}`);
      }
    } catch (err) {
      setToast(`Gagal menambah workspace: ${String(err)}`);
    }
  });

  addRow.append(aliasLabel, aliasInput, addBtn);
  dropdown.appendChild(addRow);

  // Quick-mount suggestions for pinned/recent paths not yet mounted
  const unmountedPinned = (snap.pinned_workspaces ?? []).filter(
    (p) => !mounted.some((w) => samePath(w.path, p))
  );
  const unmountedRecent = (snap.recent_workspaces ?? []).filter(
    (r) =>
      !mounted.some((w) => samePath(w.path, r)) &&
      !unmountedPinned.some((p) => samePath(p, r))
  );
  const quickCandidates = [
    ...unmountedPinned.map((path) => ({ path, isPinned: true })),
    ...unmountedRecent.map((path) => ({ path, isPinned: false })),
  ].slice(0, 6);

  if (quickCandidates.length > 0) {
    dropdown.appendChild(buildSectionHeader("QUICK-MOUNT (PINNED / RECENT)"));
    for (const item of quickCandidates) {
      const row = document.createElement("div");
      row.className = "ws-dropdown-row";

      const pickBtn = document.createElement("button");
      pickBtn.type = "button";
      pickBtn.className = "ws-dropdown-pick";
      pickBtn.textContent = item.isPinned ? `[pinned] ${item.path}` : item.path;
      pickBtn.title = `Switch default ke ${item.path}`;
      pickBtn.addEventListener("click", async () => {
        dropdown.hidden = true;
        $("btn-recent-workspaces").setAttribute("aria-expanded", "false");
        try {
          const next = await invoke<DashboardSnapshot>("set_workspace_root", {
            path: item.path,
          });
          onSnapshot(next);
          setToast(`Default workspace: ${next.workspace_root}`);
        } catch (err) {
          setToast(`Gagal membuka workspace: ${String(err)}`);
        }
      });

      const mountBtn = document.createElement("button");
      mountBtn.type = "button";
      mountBtn.className = "btn btn-secondary btn-xs";
      mountBtn.textContent = "Mount";
      mountBtn.setAttribute("aria-label", `Mount ${item.path}`);
      mountBtn.addEventListener("click", async () => {
        try {
          const next = await invoke<DashboardSnapshot>("add_workspace", {
            path: item.path,
            alias: null,
            guardrailOverride: null,
            setDefault: false,
          });
          onSnapshot(next);
          setToast(`Mounted: ${item.path}`);
        } catch (err) {
          setToast(`Gagal mount workspace: ${String(err)}`);
        }
      });

      const pinBtn = document.createElement("button");
      pinBtn.type = "button";
      pinBtn.className = "btn btn-secondary btn-xs";
      pinBtn.textContent = item.isPinned ? "Unpin" : "Pin";
      pinBtn.setAttribute("aria-label", `${item.isPinned ? "Unpin" : "Pin"} ${item.path}`);
      pinBtn.addEventListener("click", async () => {
        try {
          const next = await invoke<DashboardSnapshot>("toggle_pin_workspace", {
            path: item.path,
          });
          onSnapshot(next);
        } catch (err) {
          setToast(`Gagal mengubah pin: ${String(err)}`);
        }
      });

      row.append(pickBtn, mountBtn, pinBtn);
      dropdown.appendChild(row);
    }
  }
}

export function renderWorkspaceView(
  snap: DashboardSnapshot,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const wsInput = $("workspace-input") as HTMLInputElement;
  if (document.activeElement !== wsInput) {
    wsInput.value = snap.workspace_root;
  }

  const mounted = snap.workspaces ?? [];
  const enabledWs = mounted.filter((w) => w.enabled);
  const defWs = mounted.find((w) => w.is_default) ?? enabledWs[0] ?? null;

  $("workspace-alias-pill").textContent = defWs ? `@${defWs.alias}` : "@workspace";

  const mountedBadge = $("workspace-mounted-badge");
  if (enabledWs.length > 1) {
    mountedBadge.hidden = false;
    mountedBadge.textContent = `+${enabledWs.length - 1}`;
    mountedBadge.title = `Mounted: ${enabledWs.map((w) => `@${w.alias}`).join(", ")}`;
  } else {
    mountedBadge.hidden = true;
    mountedBadge.textContent = "+0";
  }

  const branchPill = $("workspace-git-branch");
  if (snap.git_branch) {
    branchPill.hidden = false;
    branchPill.textContent = `git:${snap.git_branch}`;
  } else {
    branchPill.hidden = true;
    branchPill.textContent = "";
  }

  const isPinned = (snap.pinned_workspaces ?? []).some((p) =>
    samePath(p, snap.workspace_root)
  );
  const pinBtn = $("btn-pin-workspace");
  pinBtn.setAttribute("aria-pressed", isPinned ? "true" : "false");
  pinBtn.title = isPinned ? "Pinned — klik untuk melepas" : "Pin Workspace";
  const starSvg = pinBtn.querySelector("svg");
  if (starSvg) {
    starSvg.setAttribute("fill", isPinned ? "currentColor" : "none");
  }

  if (
    !$("workspace-dropdown").contains(document.activeElement) ||
    document.activeElement?.tagName !== "SELECT"
  ) {
    renderWorkspaceDropdown(snap, onSnapshot);
  }
}

export function initWorkspaceView(
  getSnap: () => DashboardSnapshot | null,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  $("btn-apply-workspace").addEventListener("click", async () => {
    const path = ($("workspace-input") as HTMLInputElement).value.trim();
    if (!path) return;
    try {
      const snap = await invoke<DashboardSnapshot>("set_workspace_root", { path });
      onSnapshot(snap);
      setToast(`Workspace dikunci ke: ${snap.workspace_root}`);
    } catch (err) {
      setToast(`Gagal mengubah workspace: ${String(err)}`);
    }
  });

  ($("workspace-input") as HTMLInputElement).addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      $("btn-apply-workspace").click();
    }
  });

  $("btn-pick-workspace").addEventListener("click", async () => {
    try {
      const picked = await open({
        directory: true,
        multiple: false,
        title: "Pilih Folder Default Workspace",
      });
      if (typeof picked === "string") {
        const snap = await invoke<DashboardSnapshot>("set_workspace_root", { path: picked });
        onSnapshot(snap);
        setToast(`Workspace dikunci ke: ${snap.workspace_root}`);
      }
    } catch (err) {
      setToast(`Gagal membuka dialog folder: ${String(err)}`);
    }
  });

  $("btn-pin-workspace").addEventListener("click", async () => {
    const snap = getSnap();
    if (!snap) return;
    try {
      const next = await invoke<DashboardSnapshot>("toggle_pin_workspace", {
        path: snap.workspace_root,
      });
      onSnapshot(next);
      setToast("Daftar pin workspace diperbarui.");
    } catch (err) {
      setToast(`Gagal pin workspace: ${String(err)}`);
    }
  });

  const dropdown = $("workspace-dropdown");
  dropdown.addEventListener("click", (e) => {
    e.stopPropagation();
  });

  const toggleBtn = $("btn-recent-workspaces");
  toggleBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    const snap = getSnap();
    if (snap) renderWorkspaceDropdown(snap, onSnapshot);
    dropdown.hidden = !dropdown.hidden;
    toggleBtn.setAttribute("aria-expanded", dropdown.hidden ? "false" : "true");
  });

  document.addEventListener("click", (e) => {
    if (
      !dropdown.hidden &&
      !dropdown.contains(e.target as Node) &&
      e.target !== toggleBtn
    ) {
      dropdown.hidden = true;
      toggleBtn.setAttribute("aria-expanded", "false");
    }
  });
}
