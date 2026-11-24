import { copyToClipboard } from "./dom";

function appendInlineFormatted(parent: HTMLElement, text: string): void {
  const tokenRe = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*]+\*)/g;
  let lastIdx = 0;
  let match: RegExpExecArray | null;

  while ((match = tokenRe.exec(text)) !== null) {
    if (match.index > lastIdx) {
      parent.appendChild(document.createTextNode(text.slice(lastIdx, match.index)));
    }
    const token = match[0];
    if (token.startsWith("`") && token.endsWith("`")) {
      const code = document.createElement("code");
      code.className = "md-inline-code";
      code.textContent = token.slice(1, -1);
      parent.appendChild(code);
    } else if (token.startsWith("**") && token.endsWith("**")) {
      const strong = document.createElement("strong");
      strong.textContent = token.slice(2, -2);
      parent.appendChild(strong);
    } else if (token.startsWith("*") && token.endsWith("*")) {
      const em = document.createElement("em");
      em.textContent = token.slice(1, -1);
      parent.appendChild(em);
    }
    lastIdx = match.index + token.length;
  }

  if (lastIdx < text.length) {
    parent.appendChild(document.createTextNode(text.slice(lastIdx)));
  }
}

function renderProseBlock(container: HTMLElement, blockText: string): void {
  const lines = blockText.split("\n");
  let listEl: HTMLUListElement | HTMLOListElement | null = null;
  let listKind: "ul" | "ol" | null = null;

  const flushList = (): void => {
    if (listEl) {
      container.appendChild(listEl);
      listEl = null;
      listKind = null;
    }
  };

  for (const rawLine of lines) {
    const line = rawLine.trimEnd();
    if (!line.trim()) {
      flushList();
      continue;
    }

    const headingMatch = /^(#{1,3})\s+(.+)$/.exec(line);
    if (headingMatch) {
      flushList();
      const level = headingMatch[1].length;
      const tag = level === 1 ? "h3" : level === 2 ? "h4" : "h5";
      const h = document.createElement(tag);
      h.className = "md-heading";
      appendInlineFormatted(h, headingMatch[2]);
      container.appendChild(h);
      continue;
    }

    if (line.startsWith("> ")) {
      flushList();
      const bq = document.createElement("blockquote");
      bq.className = "md-quote";
      appendInlineFormatted(bq, line.slice(2));
      container.appendChild(bq);
      continue;
    }

    const ulMatch = /^\s*[-*]\s+(.+)$/.exec(line);
    if (ulMatch) {
      if (listKind !== "ul") {
        flushList();
        listEl = document.createElement("ul");
        listEl.className = "md-list";
        listKind = "ul";
      }
      const li = document.createElement("li");
      appendInlineFormatted(li, ulMatch[1]);
      listEl!.appendChild(li);
      continue;
    }

    const olMatch = /^\s*\d+\.\s+(.+)$/.exec(line);
    if (olMatch) {
      if (listKind !== "ol") {
        flushList();
        listEl = document.createElement("ol");
        listEl.className = "md-list";
        listKind = "ol";
      }
      const li = document.createElement("li");
      appendInlineFormatted(li, olMatch[1]);
      listEl!.appendChild(li);
      continue;
    }

    flushList();
    const p = document.createElement("p");
    p.className = "md-para";
    appendInlineFormatted(p, line);
    container.appendChild(p);
  }

  flushList();
}

function buildCodeBlock(lang: string, codeText: string): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "md-code-block";

  const hdr = document.createElement("div");
  hdr.className = "md-code-hdr";

  const langLabel = document.createElement("span");
  langLabel.textContent = lang || "text";

  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "btn btn-secondary btn-xs";
  copyBtn.textContent = "Copy";
  copyBtn.addEventListener("click", () => {
    void copyToClipboard(codeText, "Code block");
  });

  hdr.append(langLabel, copyBtn);

  const pre = document.createElement("pre");
  const code = document.createElement("code");
  code.textContent = codeText;
  pre.appendChild(code);

  wrap.append(hdr, pre);
  return wrap;
}

export function renderMarkdownInto(container: HTMLElement, raw: string): void {
  container.innerHTML = "";
  const normalized = raw.replace(/\r\n/g, "\n");
  const lines = normalized.split("\n");

  let inFence = false;
  let fenceLang = "";
  let fenceLines: string[] = [];
  let proseLines: string[] = [];

  const flushProse = (): void => {
    if (proseLines.length > 0) {
      renderProseBlock(container, proseLines.join("\n"));
      proseLines = [];
    }
  };

  for (const line of lines) {
    const trimmed = line.trim();
    if (!inFence && trimmed.startsWith("```")) {
      flushProse();
      inFence = true;
      fenceLang = trimmed.slice(3).trim();
      fenceLines = [];
    } else if (inFence && trimmed === "```") {
      container.appendChild(buildCodeBlock(fenceLang, fenceLines.join("\n")));
      inFence = false;
      fenceLang = "";
      fenceLines = [];
    } else if (inFence) {
      fenceLines.push(line);
    } else {
      proseLines.push(line);
    }
  }

  if (inFence) {
    container.appendChild(buildCodeBlock(fenceLang, fenceLines.join("\n")));
  } else {
    flushProse();
  }
}
