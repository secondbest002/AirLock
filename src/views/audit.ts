import { invoke } from "@tauri-apps/api/core";
import { $, setToast } from "../dom";
import type { AuditAddedPayload, AuditEntry, DashboardSnapshot } from "../types";
import { openRevertConflictModal } from "./modals";

let selectedAuditId: string | null = null;
let auditSearchQuery = "";
let auditSourceFilter = "ALL";
let auditRevertableOnly = false;
let snapGetterRef: (() => DashboardSnapshot | null) | null = null;

function matchesAuditFilter(entry: AuditEntry): boolean {
  if (auditSourceFilter !== "ALL" && entry.source !== auditSourceFilter) {
    return false;
  }
  if (auditRevertableOnly && !(entry.can_revert && !entry.reverted)) {
    return false;
  }
  const q = auditSearchQuery.trim().toLowerCase();
  if (!q) return true;
  const hay = `${entry.tool_name} ${entry.source} ${entry.target_path ?? ""} ${entry.summary}`.toLowerCase();
  return hay.includes(q);
}

function buildAuditItem(
  entry: AuditEntry,
  getSnap: () => DashboardSnapshot | null
): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.dataset.auditId = entry.id;
  btn.className = `audit-item ${entry.id === selectedAuditId ? "selected" : ""}`;
  btn.addEventListener("click", () => {
    selectedAuditId = entry.id;
    document.querySelectorAll<HTMLButtonElement>("#audit-list .audit-item").forEach((el) => {
      el.classList.toggle("selected", el.dataset.auditId === entry.id);
    });
    const found =
      getSnap()?.audit_logs.find((e) => e.id === entry.id) ?? entry;
    renderDiffInspector(found);
  });

  const top = document.createElement("div");
  top.className = "audit-item-top";

  const nameSpan = document.createElement("span");
  nameSpan.className = "audit-tool-name";
  nameSpan.textContent = `${entry.tool_name} [${entry.source}]`;

  const timeSpan = document.createElement("span");
  timeSpan.textContent = entry.reverted ? "REVERTED" : entry.timestamp;

  top.append(nameSpan, timeSpan);

  const summary = document.createElement("div");
  summary.className = "audit-summary";
  summary.textContent = entry.target_path ? `${entry.target_path} — ${entry.summary}` : entry.summary;

  btn.append(top, summary);
  if (entry.group_id) {
    const pill = document.createElement("span");
    pill.className = "group-pill";
    pill.textContent = entry.group_id;
    btn.appendChild(pill);
  }
  return btn;
}

export function renderDiffInspector(entry: AuditEntry | null): void {
  const titleEl = $("diff-title");
  const metaEl = $("diff-meta");
  const viewerEl = $("diff-viewer");
  const revertBtn = $("btn-revert-selected") as HTMLButtonElement;
  const groupBtn = $("btn-revert-group") as HTMLButtonElement;
  const vscodeBtn = $("btn-open-vscode") as HTMLButtonElement;
  const explorerBtn = $("btn-open-explorer") as HTMLButtonElement;

  viewerEl.innerHTML = "";

  if (!entry) {
    titleEl.textContent = "Visual Diff Inspector";
    metaEl.textContent =
      "Pilih entri log di sebelah kiri untuk melihat detail atau melakukan 1-Click Revert.";
    revertBtn.disabled = true;
    groupBtn.hidden = true;
    groupBtn.disabled = true;
    vscodeBtn.disabled = true;
    explorerBtn.disabled = true;
    const empty = document.createElement("div");
    empty.className = "chat-empty";
    empty.textContent = "Tidak ada diff yang dipilih.";
    viewerEl.appendChild(empty);
    return;
  }

  titleEl.textContent = entry.target_path
    ? `${entry.tool_name}: ${entry.target_path}`
    : entry.tool_name;
  metaEl.textContent = `${entry.timestamp} · ${entry.summary} · Hemat ~${entry.tokens_saved} token`;
  revertBtn.disabled = !(entry.can_revert && !entry.reverted);

  if (entry.group_id) {
    const logs = snapGetterRef?.()?.audit_logs ?? [];
    const revertableCount = logs.filter(
      (e) => e.group_id === entry.group_id && e.can_revert && !e.reverted
    ).length;
    groupBtn.hidden = false;
    groupBtn.disabled = revertableCount === 0;
    groupBtn.textContent =
      revertableCount > 0 ? `Revert Turn Group (${revertableCount})` : "Group Reverted";
  } else {
    groupBtn.hidden = true;
    groupBtn.disabled = true;
  }

  const hasTargetPath = Boolean(entry.target_path);
  vscodeBtn.disabled = !hasTargetPath;
  explorerBtn.disabled = !hasTargetPath;

  if (!entry.diff_text) {
    const pre = document.createElement("div");
    pre.className = "diff-line";
    pre.textContent = entry.summary;
    viewerEl.appendChild(pre);
    return;
  }

  const lines = entry.diff_text.split("\n");
  lines.forEach((line, idx) => {
    const div = document.createElement("div");
    div.className = "diff-line";
    if (line.startsWith("+") && !line.startsWith("+++")) {
      div.classList.add("diff-add");
    } else if (line.startsWith("-") && !line.startsWith("---")) {
      div.classList.add("diff-del");
    } else if (line.startsWith("@@")) {
      div.classList.add("diff-hunk");
    }
    const ln = document.createElement("span");
    ln.className = "diff-ln";
    ln.textContent = line.startsWith("@@") ? "..." : String(idx + 1);
    const code = document.createElement("span");
    code.textContent = line;
    div.append(ln, code);
    viewerEl.appendChild(div);
  });
}

export function renderAuditList(
  logs: AuditEntry[],
  getSnap: () => DashboardSnapshot | null
): void {
  snapGetterRef = getSnap;
  const container = $("audit-list");
  container.innerHTML = "";

  const filtered = [...logs].reverse().filter(matchesAuditFilter);

  if (filtered.length === 0) {
    const empty = document.createElement("div");
    empty.className = "chat-empty";
    empty.textContent =
      logs.length === 0
        ? "Belum ada aktivitas tool pada sesi ini."
        : "Tidak ada entri audit yang cocok dengan filter.";
    container.appendChild(empty);
    const currentSelected = logs.find((e) => e.id === selectedAuditId) ?? null;
    renderDiffInspector(currentSelected);
    return;
  }

  if (!selectedAuditId || !logs.some((e) => e.id === selectedAuditId)) {
    selectedAuditId = filtered[0].id;
  }

  for (const entry of filtered) {
    container.appendChild(buildAuditItem(entry, getSnap));
  }

  const selected = logs.find((e) => e.id === selectedAuditId) ?? null;
  renderDiffInspector(selected);
}

export function appendAuditIncremental(
  snap: DashboardSnapshot | null,
  payload: AuditAddedPayload,
  getSnap: () => DashboardSnapshot | null
): void {
  const { entry, metrics } = payload;

  $("metric-tokens-saved").textContent = metrics.tokens_saved_total.toLocaleString("en-US");
  $("metric-turns-saved").textContent = metrics.turns_saved_total.toLocaleString("en-US");

  if (snap) {
    snap.metrics = metrics;
    snap.audit_logs.push(entry);
    if (snap.audit_logs.length > 200) {
      snap.audit_logs.splice(0, snap.audit_logs.length - 200);
    }
    $("audit-count-badge").textContent = String(snap.audit_logs.length);
  }

  if (!matchesAuditFilter(entry)) {
    return;
  }

  const container = $("audit-list");
  const emptyPlaceholder = container.querySelector(".chat-empty");
  if (emptyPlaceholder) {
    emptyPlaceholder.remove();
  }

  const wasUnselected = !selectedAuditId;
  if (wasUnselected) {
    selectedAuditId = entry.id;
  }

  container.prepend(buildAuditItem(entry, getSnap));

  const items = container.querySelectorAll(".audit-item");
  for (let i = 200; i < items.length; i++) {
    items[i].remove();
  }

  if (wasUnselected) {
    renderDiffInspector(entry);
  }
}

export function initAuditView(
  getSnap: () => DashboardSnapshot | null,
  onRefresh: () => Promise<void>
): void {
  snapGetterRef = getSnap;
  $("btn-refresh-audit").addEventListener("click", () => void onRefresh());

  $("audit-search-input").addEventListener("input", (e) => {
    auditSearchQuery = (e.target as HTMLInputElement).value;
    const snap = getSnap();
    if (snap) renderAuditList(snap.audit_logs, getSnap);
  });

  $("audit-source-filter").addEventListener("change", (e) => {
    auditSourceFilter = (e.target as HTMLSelectElement).value;
    const snap = getSnap();
    if (snap) renderAuditList(snap.audit_logs, getSnap);
  });

  $("audit-revertable-only").addEventListener("change", (e) => {
    auditRevertableOnly = (e.target as HTMLInputElement).checked;
    const snap = getSnap();
    if (snap) renderAuditList(snap.audit_logs, getSnap);
  });

  const executeForceRevert = async (targetId: string): Promise<void> => {
    try {
      const msg = await invoke<string>("revert_audit_entry", {
        auditId: targetId,
        force: true,
      });
      setToast(msg);
      await onRefresh();
    } catch (err) {
      setToast(`Gagal force revert: ${String(err)}`);
    }
  };

  $("btn-revert-selected").addEventListener("click", async () => {
    if (!selectedAuditId) return;
    const targetAuditId = selectedAuditId;
    try {
      const msg = await invoke<string>("revert_audit_entry", {
        auditId: targetAuditId,
        force: false,
      });
      setToast(msg);
      await onRefresh();
    } catch (err) {
      const errStr = String(err);
      if (errStr.includes("CONFLICT:")) {
        openRevertConflictModal(targetAuditId, errStr, executeForceRevert);
      } else {
        setToast(`Gagal revert: ${errStr}`);
      }
    }
  });

  const executeForceGroupRevert = async (targetGroupId: string): Promise<void> => {
    try {
      const msg = await invoke<string>("revert_audit_group", {
        groupId: targetGroupId,
        force: true,
      });
      setToast(msg);
      await onRefresh();
    } catch (err) {
      setToast(`Gagal force revert group: ${String(err)}`);
    }
  };

  $("btn-revert-group").addEventListener("click", async () => {
    const snap = getSnap();
    const selected = snap?.audit_logs.find((e) => e.id === selectedAuditId) ?? null;
    const targetGroupId = selected?.group_id;
    if (!targetGroupId) return;
    try {
      const msg = await invoke<string>("revert_audit_group", {
        groupId: targetGroupId,
        force: false,
      });
      setToast(msg);
      await onRefresh();
    } catch (err) {
      const errStr = String(err);
      if (errStr.includes("CONFLICT:")) {
        openRevertConflictModal(targetGroupId, errStr, executeForceGroupRevert);
      } else {
        setToast(`Gagal revert turn group: ${errStr}`);
      }
    }
  });

  const openSelectedIn = async (mode: "vscode" | "explorer"): Promise<void> => {
    const snap = getSnap();
    const selected = snap?.audit_logs.find((e) => e.id === selectedAuditId) ?? null;
    const relPath = selected?.target_path ?? null;
    try {
      const msg = await invoke<string>("open_in_editor", { relPath, mode });
      setToast(msg);
    } catch (err) {
      setToast(String(err));
    }
  };

  $("btn-open-vscode").addEventListener("click", () => void openSelectedIn("vscode"));
  $("btn-open-explorer").addEventListener("click", () => void openSelectedIn("explorer"));
}
