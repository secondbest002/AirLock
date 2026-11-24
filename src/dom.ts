import { writeText } from "@tauri-apps/plugin-clipboard-manager";

export type ToastKind = "info" | "warn" | "offline";
export type AccentTheme = "mint" | "cyan";

const ACCENT_STORAGE_KEY = "airlock_accent";

export function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`Missing element #${id}`);
  return el;
}

export function setToast(msg: string, kind: ToastKind = "info"): void {
  $("toast-region").textContent = msg;
  const stack = document.getElementById("toast-stack");
  if (!stack || !msg.trim()) return;

  while (stack.children.length >= 4 && stack.firstElementChild) {
    stack.firstElementChild.remove();
  }

  const item = document.createElement("div");
  item.className = "toast-item";

  const dot = document.createElement("span");
  dot.className = `live-dot ${kind === "warn" ? "warn" : kind === "offline" ? "offline" : "online"}`;
  dot.setAttribute("aria-hidden", "true");

  const text = document.createElement("span");
  text.textContent = msg;

  item.append(dot, text);
  stack.appendChild(item);

  window.setTimeout(() => {
    item.remove();
  }, 3400);
}

export async function copyToClipboard(text: string, label: string): Promise<void> {
  try {
    await writeText(text);
    setToast(`${label} disalin ke clipboard.`);
  } catch {
    await navigator.clipboard.writeText(text);
    setToast(`${label} disalin ke clipboard.`);
  }
}

export function maskToken(token: string): string {
  if (!token) return "---";
  if (token.length <= 6) return "••••••";
  return `${"•".repeat(12)}${token.slice(-4)}`;
}

export function escapeHtml(raw: string): string {
  return raw
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

export function syncRailIndicator(): void {
  const activeBtn = document.querySelector<HTMLElement>(".rail-btn.active");
  const indicator = document.getElementById("rail-indicator");
  if (!activeBtn || !indicator) return;
  const y = activeBtn.offsetTop + 8;
  indicator.style.transform = `translateY(${y}px)`;
}

export function positionGuardrailGlider(): void {
  const track = document.getElementById("guardrail-track");
  const glider = document.getElementById("guardrail-glider");
  const activeBtn = track?.querySelector<HTMLElement>(".guardrail-btn.active");
  if (!track || !glider || !activeBtn) return;
  const trackW = track.clientWidth;
  if (!trackW) return;
  const x = activeBtn.offsetLeft;
  const sx = activeBtn.offsetWidth / trackW;
  glider.style.transform = `translateX(${x}px) scaleX(${sx})`;
}

export function switchTab(tabName: string): void {
  const currentActive = document.querySelector<HTMLElement>(".view-panel.active");
  if (currentActive?.id === `panel-${tabName}`) {
    return;
  }

  const applySwitch = (): void => {
    document.querySelectorAll<HTMLButtonElement>(".rail-btn").forEach((btn) => {
      const active = btn.dataset.tab === tabName;
      btn.classList.toggle("active", active);
      btn.setAttribute("aria-selected", active ? "true" : "false");
    });
    document.querySelectorAll<HTMLElement>(".view-panel").forEach((panel) => {
      panel.classList.toggle("active", panel.id === `panel-${tabName}`);
    });
    syncRailIndicator();
  };

  const targetHeading = document.getElementById(`heading-${tabName}`);
  const doc = document as Document & {
    startViewTransition?: (cb: () => void) => { finished: Promise<void> };
  };

  if (!doc.startViewTransition) {
    applySwitch();
    targetHeading?.focus({ preventScroll: true });
    return;
  }

  const transition = doc.startViewTransition(applySwitch);
  void transition.finished.finally(() => {
    targetHeading?.focus({ preventScroll: true });
  });
}

let supportsDisplayTransition: boolean | undefined;

function canTransitionDisplay(): boolean {
  if (supportsDisplayTransition !== undefined) return supportsDisplayTransition;
  if (!window.CSS?.supports?.("transition-behavior", "allow-discrete") || !document.body) {
    supportsDisplayTransition = false;
    return false;
  }
  const probe = document.createElement("div");
  probe.style.cssText = "transition: display 1s allow-discrete !important; display: block;";
  document.body.appendChild(probe);
  getComputedStyle(probe).display;
  probe.style.display = "none";
  supportsDisplayTransition = getComputedStyle(probe).display === "block";
  probe.remove();
  return supportsDisplayTransition;
}

function supportsTopLayerExit(): boolean {
  return Boolean(window.CSS?.supports?.("overlay", "auto") && canTransitionDisplay());
}

export async function closeTopLayer(dialog: HTMLDialogElement): Promise<void> {
  if (!dialog.open) return;
  if (!supportsTopLayerExit()) {
    dialog.setAttribute("data-closing", "");
    const animations = dialog.getAnimations({ subtree: true });
    if (animations.length > 0) {
      await Promise.race([
        Promise.allSettled(animations.map((a) => a.finished)),
        new Promise<void>((resolve) => window.setTimeout(resolve, 600)),
      ]);
    }
    if (!dialog.hasAttribute("data-closing")) return;
    dialog.removeAttribute("data-closing");
  }
  dialog.close();
}

export function setupDialogLightDismiss(dialog: HTMLDialogElement): void {
  const closedBy = dialog.getAttribute("closedby");
  if (closedBy === "any") {
    dialog.addEventListener("cancel", (event) => {
      if (!supportsTopLayerExit()) {
        event.preventDefault();
        void closeTopLayer(dialog);
      }
    });

    if (!("closedBy" in HTMLDialogElement.prototype)) {
      dialog.addEventListener("click", (event) => {
        if (event.target !== dialog) return;
        const rect = dialog.getBoundingClientRect();
        const inside =
          rect.top <= event.clientY &&
          event.clientY <= rect.top + rect.height &&
          rect.left <= event.clientX &&
          event.clientX <= rect.left + rect.width;
        if (!inside) {
          void closeTopLayer(dialog);
        }
      });
    }
  }
}

export function setAccentTheme(accent: AccentTheme, announce = true): void {
  document.documentElement.dataset.accent = accent;
  try {
    localStorage.setItem(ACCENT_STORAGE_KEY, accent);
  } catch {
    // ignore storage errors
  }
  const label = document.getElementById("accent-name-label");
  if (label) {
    label.textContent = accent === "cyan" ? "Ice Cyan" : "Electric Mint";
  }
  if (announce) {
    setToast(`Accent Obsidian Studio: ${accent === "cyan" ? "Ice Cyan" : "Electric Mint"}`);
  }
}

export function initAccentTheme(): AccentTheme {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(ACCENT_STORAGE_KEY);
  } catch {
    stored = null;
  }
  const accent: AccentTheme = stored === "cyan" ? "cyan" : "mint";
  setAccentTheme(accent, false);
  return accent;
}
