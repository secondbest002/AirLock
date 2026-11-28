import { invoke } from "@tauri-apps/api/core";
import { $, setToast } from "../dom";
import type { AuditAddedPayload, AuditEntry, DashboardSnapshot } from "../types";
import { openRevertConflictModal } from "./modals";

type DiffViewMode = "unified" | "split";

interface UnifiedRow {
  kind: "hunk" | "meta" | "ctx" | "add" | "del";
  oldLn: number | null;
  newLn: number | null;
  text: string;
}

interface SplitCell {
  kind: "ctx" | "add" | "del" | "empty";
  ln: number | null;
  text: string;
}

interface SplitRow {
  kind: "hunk" | "meta" | "pair";
  bannerText?: string;
  left?: SplitCell;
  right?: SplitCell;
}

interface ParsedDiffData {
  added: number;
  removed: number;
  unified: UnifiedRow[];
  split: SplitRow[];
}

const DIFF_MODE_KEY = "airlock_diff_mode";
const narrowDiffQuery = window.matchMedia("(max-width: 1040px)");

let selectedAuditId: string | null = null;
let auditSearchQuery = "";
let auditSourceFilter = "ALL";
let auditRevertableOnly = false;
let snapGetterRef: (() => DashboardSnapshot | null) | null = null;
let diffViewMode: DiffViewMode = (() => {
  try {
    return localStorage.getItem(DIFF_MODE_KEY) === "split" ? "split" : "unified";
  } catch {
    return "unified";
  }
})();

function parseUnifiedDiffRows(diffText: string): ParsedDiffData {
  const lines = diffText.split("\n");
  const unified: UnifiedRow[] = [];
  const split: SplitRow[] = [];
  const hunkRe = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;

  let oldLine = 1;
  let newLine = 1;
  let hasSeenHunk = false;
  let added = 0;
  let removed = 0;
  let pendingDels: Array<{ ln: number; text: string }> = [];
  let pendingAdds: Array<{ ln: number; text: string }> = [];

  const flushPending = (): void => {
    const count = Math.max(pendingDels.length, pendingAdds.length);
    for (let i = 0; i < count; i++) {
      const delItem = pendingDels[i];
      const addItem = pendingAdds[i];
      split.push({
        kind: "pair",
        left: delItem
          ? { kind: "del", ln: delItem.ln, text: delItem.text }
          : { kind: "empty", ln: null, text: "" },
        right: addItem
          ? { kind: "add", ln: addItem.ln, text: addItem.text }
          : { kind: "empty", ln: null, text: "" },
      });
    }
    pendingDels = [];
    pendingAdds = [];
  };

  for (const rawLine of lines) {
    const line = rawLine.endsWith("\r") ? rawLine.slice(0, -1) : rawLine;
    const hunkMatch = hunkRe.exec(line);
    if (hunkMatch) {
      flushPending();
      hasSeenHunk = true;
      oldLine = Number.parseInt(hunkMatch[1], 10) || 1;
      newLine = Number.parseInt(hunkMatch[2], 10) || 1;
      unified.push({ kind: "hunk", oldLn: null, newLn: null, text: line });
      split.push({ kind: "hunk", bannerText: line });
      continue;
    }

    if (
      line.startsWith("diff --git") ||
      line.startsWith("index ") ||
      line.startsWith("--- ") ||
      line.startsWith("+++ ")
    ) {
      flushPending();
      unified.push({ kind: "meta", oldLn: null, newLn: null, text: line });
      split.push({ kind: "meta", bannerText: line });
      continue;
    }

    if (line.startsWith("+")) {
      added += 1;
      const ln = newLine++;
      unified.push({ kind: "add", oldLn: null, newLn: ln, text: line });
      pendingAdds.push({ ln, text: line });
      continue;
    }

    if (line.startsWith("-")) {
      removed += 1;
      const ln = oldLine++;
      unified.push({ kind: "del", oldLn: ln, newLn: null, text: line });
      pendingDels.push({ ln, text: line });
      continue;
    }

    flushPending();
    if (!hasSeenHunk && line === "\\ No newline at end of file") {
      unified.push({ kind: "meta", oldLn: null, newLn: null, text: line });
      split.push({ kind: "meta", bannerText: line });
      continue;
    }

    const oLn = oldLine++;
    const nLn = newLine++;
    unified.push({ kind: "ctx", oldLn: oLn, newLn: nLn, text: line });
    split.push({
      kind: "pair",
      left: { kind: "ctx", ln: oLn, text: line },
      right: { kind: "ctx", ln: nLn, text: line },
    });
  }

  flushPending();
  return { added, removed, unified, split };
}

function syncDiffModeButtons(): void {
  const uniBtn = document.getElementById("btn-diff-unified");
  const splitBtn = document.getElementById("btn-diff-split");
  const isSplit = diffViewMode === "split";
  if (uniBtn) {
    uniBtn.classList.toggle("active", !isSplit);
    uniBtn.setAttribute("aria-pressed", isSplit ? "false" : "true");
  }
  if (splitBtn) {
    splitBtn.classList.toggle("active", isSplit);
    splitBtn.setAttribute("aria-pressed", isSplit ? "true" : "false");
  }
}

function renderDiffStats(added: number, removed: number): void {
  const statsEl = document.getElementById("diff-stats");
  if (!statsEl) return;
  if (added === 0 && removed === 0) {
    statsEl.hidden = true;
    statsEl.replaceChildren();
    return;
  }

  statsEl.hidden = false;
  const addSpan = document.createElement("span");
  addSpan.className = "diffstat-add";
  addSpan.textContent = `+${added}`;

  const delSpan = document.createElement("span");
  delSpan.className = "diffstat-del";
  delSpan.textContent = `-${removed}`;

  const blocksWrap = document.createElement("span");
  blocksWrap.className = "diffstat-blocks";
  blocksWrap.setAttribute("aria-hidden", "true");

  const total = added + removed;
  const addBlocks = total > 0 ? Math.round((added / total) * 5) : 0;
  const clampedAdd = added > 0 && addBlocks === 0 ? 1 : added === 0 ? 0 : Math.min(5, addBlocks);
  const delBlocks = removed > 0 ? Math.max(1, Math.min(5 - clampedAdd, 5)) : 0;

  for (let i = 0; i < 5; i++) {
    const blk = document.createElement("span");
    blk.className =
      i < clampedAdd
        ? "diffstat-block is-add"
        : i < clampedAdd + delBlocks
          ? "diffstat-block is-del"
          : "diffstat-block is-neutral";
    blocksWrap.appendChild(blk);
  }

  statsEl.replaceChildren(addSpan, delSpan, blocksWrap);
}

function matchesAuditFilter(entry: AuditEntry): boolean {
  if (auditSourceFilter !== "ALL" && entry.source !== auditSourceFilter) {
    return false;
  }
  if (auditRevertableOnly && !(entry.can_revert && !entry.reverted)) {
    return false;
  }
  const q = auditSearchQuery.trim().toLowerCase();
  if (!q) return true;
  const hay = `${entry.tool_name} ${entry.source} ${entry.workspace_alias ?? ""} ${entry.target_path ?? ""} ${entry.summary}`.toLowerCase();
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

  const dot = document.createElement("span");
  dot.className = `audit-timeline-dot ${
    entry.reverted ? "is-reverted" : entry.can_revert ? "is-revertable" : "is-info"
  }`;
  dot.setAttribute("aria-hidden", "true");
  btn.appendChild(dot);

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
  if (entry.workspace_alias || entry.group_id || entry.tokens_saved > 0) {
    const pillsRow = document.createElement("div");
    pillsRow.className = "audit-pills-row";
    if (entry.workspace_alias) {
      const wsPill = document.createElement("span");
      wsPill.className = "audit-ws-pill";
      wsPill.textContent = `@${entry.workspace_alias}`;
      pillsRow.appendChild(wsPill);
    }
    if (entry.group_id) {
      const pill = document.createElement("span");
      pill.className = "group-pill";
      pill.textContent = entry.group_id;
      pillsRow.appendChild(pill);
    }
    if (entry.tokens_saved > 0) {
      const tokPill = document.createElement("span");
      tokPill.className = "audit-tok-pill";
      tokPill.textContent = `-${entry.tokens_saved} tok`;
      pillsRow.appendChild(tokPill);
    }
    btn.appendChild(pillsRow);
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
    renderDiffStats(0, 0);
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

  const wsPrefix = entry.workspace_alias ? `@${entry.workspace_alias} · ` : "";
  titleEl.textContent = entry.target_path
    ? `${entry.tool_name}: ${entry.target_path}`
    : entry.tool_name;
  metaEl.textContent = `${wsPrefix}${entry.timestamp} · ${entry.summary} · Hemat ~${entry.tokens_saved} token`;
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
    renderDiffStats(0, 0);
    const pre = document.createElement("div");
    pre.className = "diff-line";
    pre.textContent = entry.summary;
    viewerEl.appendChild(pre);
    return;
  }

  const parsed = parseUnifiedDiffRows(entry.diff_text);
  renderDiffStats(parsed.added, parsed.removed);

  const effectiveMode: DiffViewMode =
    diffViewMode === "split" && !narrowDiffQuery.matches ? "split" : "unified";

  if (effectiveMode === "split") {
    const grid = document.createElement("div");
    grid.className = "diff-split-grid";
    for (const row of parsed.split) {
      if (row.kind !== "pair") {
        const banner = document.createElement("div");
        banner.className = `diff-split-banner ${row.kind === "hunk" ? "diff-hunk" : "diff-meta-line"}`;
        banner.textContent = row.bannerText ?? "";
        grid.appendChild(banner);
        continue;
      }

      const makeCell = (cell: SplitCell | undefined, sideClass: string): HTMLDivElement => {
        const el = document.createElement("div");
        el.className = `diff-split-cell ${sideClass} ${
          cell?.kind === "add"
            ? "diff-add"
            : cell?.kind === "del"
              ? "diff-del"
              : cell?.kind === "empty"
                ? "diff-cell-empty"
                : ""
        }`.trim();
        const ln = document.createElement("span");
        ln.className = "diff-ln";
        ln.textContent = cell?.ln !== null && cell?.ln !== undefined ? String(cell.ln) : "";
        const code = document.createElement("span");
        code.className = "diff-code";
        code.textContent = cell?.text ?? "";
        el.append(ln, code);
        return el;
      };

      grid.append(makeCell(row.left, "diff-split-left"), makeCell(row.right, "diff-split-right"));
    }
    viewerEl.appendChild(grid);
    return;
  }

  for (const row of parsed.unified) {
    const div = document.createElement("div");
    div.className = `diff-line diff-unified-row ${
      row.kind === "add"
        ? "diff-add"
        : row.kind === "del"
          ? "diff-del"
          : row.kind === "hunk"
            ? "diff-hunk"
            : row.kind === "meta"
              ? "diff-meta-line"
              : ""
    }`.trim();

    const oldLn = document.createElement("span");
    oldLn.className = "diff-ln";
    oldLn.textContent =
      row.kind === "hunk" ? "..." : row.oldLn !== null ? String(row.oldLn) : "";

    const newLn = document.createElement("span");
    newLn.className = "diff-ln";
    newLn.textContent =
      row.kind === "hunk" ? "..." : row.newLn !== null ? String(row.newLn) : "";

    const code = document.createElement("span");
    code.className = "diff-code";
    code.textContent = row.text;

    div.append(oldLn, newLn, code);
    viewerEl.appendChild(div);
  }
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
  syncDiffModeButtons();

  const setMode = (mode: DiffViewMode): void => {
    diffViewMode = mode;
    try {
      localStorage.setItem(DIFF_MODE_KEY, mode);
    } catch {
      // ignore storage errors
    }
    syncDiffModeButtons();
    const snap = getSnap();
    const selected = snap?.audit_logs.find((e) => e.id === selectedAuditId) ?? null;
    if (selected) {
      renderDiffInspector(selected);
    }
  };

  $("btn-diff-unified").addEventListener("click", () => setMode("unified"));
  $("btn-diff-split").addEventListener("click", () => setMode("split"));
  narrowDiffQuery.addEventListener("change", () => {
    const snap = getSnap();
    const selected = snap?.audit_logs.find((e) => e.id === selectedAuditId) ?? null;
    if (selected && diffViewMode === "split") {
      renderDiffInspector(selected);
    }
  });

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
    const workspaceAlias = selected?.workspace_alias ?? null;
    const workspaceRoot = selected?.workspace_root ?? null;
    try {
      const msg = await invoke<string>("open_in_editor", {
        relPath,
        mode,
        workspaceAlias,
        workspaceRoot,
      });
      setToast(msg);
    } catch (err) {
      setToast(String(err));
    }
  };

  $("btn-open-vscode").addEventListener("click", () => void openSelectedIn("vscode"));
  $("btn-open-explorer").addEventListener("click", () => void openSelectedIn("explorer"));
}
