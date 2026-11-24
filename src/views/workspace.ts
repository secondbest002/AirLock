import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { $, setToast } from "../dom";
import type { DashboardSnapshot } from "../types";

function samePath(a: string, b: string): boolean {
  return a.trim().toLowerCase() === b.trim().toLowerCase();
}

function renderWorkspaceDropdown(
  snap: DashboardSnapshot,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const dropdown = $("workspace-dropdown");
  dropdown.innerHTML = "";

  const pinned = snap.pinned_workspaces ?? [];
  const recent = (snap.recent_workspaces ?? []).filter(
    (r) => !pinned.some((p) => samePath(p, r))
  );

  if (pinned.length === 0 && recent.length === 0) {
    const empty = document.createElement("div");
    empty.className = "ws-dropdown-empty";
    empty.textContent = "Belum ada riwayat workspace.";
    dropdown.appendChild(empty);
    return;
  }

  const appendSection = (title: string, items: string[], isPinnedSection: boolean): void => {
    if (items.length === 0) return;
    const hdr = document.createElement("div");
    hdr.className = "ws-dropdown-hdr";
    hdr.textContent = title;
    dropdown.appendChild(hdr);

    for (const path of items) {
      const row = document.createElement("div");
      row.className = "ws-dropdown-row";

      const pickBtn = document.createElement("button");
      pickBtn.type = "button";
      pickBtn.className = "ws-dropdown-pick";
      pickBtn.textContent = path;
      pickBtn.title = path;
      pickBtn.addEventListener("click", async () => {
        dropdown.hidden = true;
        try {
          const next = await invoke<DashboardSnapshot>("set_workspace_root", { path });
          onSnapshot(next);
          setToast(`Workspace dikunci ke: ${next.workspace_root}`);
        } catch (err) {
          setToast(`Gagal mengubah workspace: ${String(err)}`);
        }
      });

      const pinBtn = document.createElement("button");
      pinBtn.type = "button";
      pinBtn.className = "btn btn-secondary btn-xs";
      pinBtn.textContent = isPinnedSection ? "Unpin" : "Pin";
      pinBtn.addEventListener("click", async (e) => {
        e.stopPropagation();
        try {
          const next = await invoke<DashboardSnapshot>("toggle_pin_workspace", { path });
          onSnapshot(next);
        } catch (err) {
          setToast(`Gagal mengubah pin: ${String(err)}`);
        }
      });

      row.append(pickBtn, pinBtn);
      dropdown.appendChild(row);
    }
  };

  appendSection("PINNED WORKSPACES", pinned, true);
  appendSection("RECENT WORKSPACES", recent, false);
}

export function renderWorkspaceView(
  snap: DashboardSnapshot,
  onSnapshot: (snap: DashboardSnapshot) => void
): void {
  const wsInput = $("workspace-input") as HTMLInputElement;
  if (document.activeElement !== wsInput) {
    wsInput.value = snap.workspace_root;
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

  renderWorkspaceDropdown(snap, onSnapshot);
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
        title: "Pilih Folder Proyek (Jailed Root)",
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
