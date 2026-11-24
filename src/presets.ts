import type { DashboardSnapshot } from "./types";

export interface McpPresetOption {
  id: string;
  label: string;
}

export interface GeneratedPreset {
  title: string;
  filename: string;
  instructions: string;
  content: string;
}

export const MCP_CLIENT_PRESETS: McpPresetOption[] = [
  { id: "acp-zed", label: "Zed / JetBrains (ACP Agent --acp)" },
  { id: "gemini-web", label: "Gemini Web (Remote OAuth / URL)" },
  { id: "stdio-cli", label: "Local CLI Stdio Bridge (Claude / Cursor / Antigravity)" },
  { id: "claude-desktop", label: "Claude Desktop (mcpServers HTTP)" },
  { id: "cursor", label: "Cursor (.cursor/mcp.json)" },
  { id: "vscode", label: "VS Code Copilot (.vscode/mcp.json)" },
  { id: "windsurf", label: "Windsurf (mcp_config.json)" },
  { id: "gemini-cli", label: "Gemini CLI (~/.gemini/settings.json)" },
  { id: "antigravity", label: "Antigravity (mcp_config.json)" },
];

export function generateClientPreset(
  presetId: string,
  snap: DashboardSnapshot,
  usePublicTunnel: boolean
): GeneratedPreset {
  const localBase = `http://127.0.0.1:${snap.server_port}`;
  const tunnelBase = snap.tunnel_url ? snap.tunnel_url.replace(/\/+$/, "") : null;
  const activeBase = usePublicTunnel && tunnelBase ? tunnelBase : localBase;

  if (presetId === "acp-zed") {
    const payload = {
      agent_servers: {
        Airlock: {
          command: "D:\\tauri-mcp-hub\\src-tauri\\target\\release\\tauri-mcp-hub.exe",
          args: ["--acp"],
        },
      },
    };
    return {
      title: "Zed / JetBrains — Agent Client Protocol (--acp)",
      filename: "~/.config/zed/settings.json / ACP Agent Config",
      instructions:
        "Sesuaikan path 'command' ke binary tauri-mcp-hub.exe Anda. Mode --acp menjalankan Airlock Local Agent langsung di IDE dengan streaming session/update dan bridge HITL session/request_permission.",
      content: JSON.stringify(payload, null, 2),
    };
  }

  if (presetId === "stdio-cli") {
    const payload = {
      mcpServers: {
        airlock: {
          command: "D:\\tauri-mcp-hub\\src-tauri\\target\\release\\tauri-mcp-hub.exe",
          args: ["--stdio"],
        },
      },
    };
    return {
      title: "Local CLI Stdio Bridge Config",
      filename: "claude_desktop_config.json / mcp_config.json",
      instructions:
        "Sesuaikan path 'command' ke lokasi tauri-mcp-hub.exe Anda. Mode --stdio otomatis mem-proxy ke Airlock GUI yang aktif, atau fallback embedded bila GUI tertutup.",
      content: JSON.stringify(payload, null, 2),
    };
  }

  if (presetId === "gemini-web") {
    const remoteBase = tunnelBase ?? "https://<TUNNEL-BELUM-AKTIF>";
    const oauthUrl = `${remoteBase}/mcp`;
    const pathTokenUrl = `${remoteBase}/mcp/${snap.auth_token}`;
    return {
      title: "Gemini Web — Custom MCP Connector",
      filename: "Gemini Web Connected Apps",
      instructions: tunnelBase
        ? "Tempelkan Endpoint OAuth (direkomendasikan) di Gemini Web -> Apps -> Add Custom MCP, lalu setujui popup OAuth di Airlock. Jika klien membutuhkan token langsung di URL, gunakan URL fallback di bawah."
        : "Aktifkan Remote Tunnel (Tailscale Funnel atau Cloudflare) terlebih dahulu untuk mendapatkan URL HTTPS publik.",
      content: [
        `# Endpoint OAuth 2.1 (Direkomendasikan):`,
        oauthUrl,
        ``,
        `# Alternatif Bearer Token di Path URL (Non-OAuth):`,
        pathTokenUrl,
        ``,
        `# Bearer Header (opsional):`,
        `Authorization: Bearer ${snap.auth_token}`,
      ].join("\n"),
    };
  }

  if (presetId === "vscode") {
    const payload = {
      servers: {
        airlock: {
          type: "http",
          url: `${activeBase}/mcp`,
          headers: {
            Authorization: `Bearer ${snap.auth_token}`,
          },
        },
      },
    };
    return {
      title: "VS Code Copilot MCP Config",
      filename: ".vscode/mcp.json",
      instructions: "Simpan ke .vscode/mcp.json di proyek Anda atau User mcp.json.",
      content: JSON.stringify(payload, null, 2),
    };
  }

  if (presetId === "gemini-cli") {
    const payload = {
      mcpServers: {
        airlock: {
          httpUrl: `${activeBase}/mcp`,
          headers: {
            Authorization: `Bearer ${snap.auth_token}`,
          },
        },
      },
    };
    return {
      title: "Gemini CLI MCP Config",
      filename: "~/.gemini/settings.json",
      instructions: "Gabungkan blok mcpServers ini ke ~/.gemini/settings.json.",
      content: JSON.stringify(payload, null, 2),
    };
  }

  const labelMap: Record<string, { title: string; filename: string; instructions: string }> = {
    "claude-desktop": {
      title: "Claude Desktop MCP Config",
      filename: "claude_desktop_config.json",
      instructions:
        "Buka Settings -> Developer -> Edit Config di Claude Desktop, lalu tambahkan blok mcpServers ini.",
    },
    cursor: {
      title: "Cursor MCP Config",
      filename: ".cursor/mcp.json",
      instructions: "Simpan ke .cursor/mcp.json (workspace) atau ~/.cursor/mcp.json (global).",
    },
    windsurf: {
      title: "Windsurf MCP Config",
      filename: "~/.codeium/windsurf/mcp_config.json",
      instructions: "Tempelkan ke mcp_config.json di Windsurf MCP settings.",
    },
    antigravity: {
      title: "Antigravity MCP Config",
      filename: "mcp_config.json",
      instructions: "Tambahkan ke konfigurasi MCP server Antigravity.",
    },
  };

  const meta = labelMap[presetId] ?? {
    title: "MCP Client Config",
    filename: "mcp.json",
    instructions: "Tempelkan ke file konfigurasi klien MCP Anda.",
  };

  const payload = {
    mcpServers: {
      airlock: {
        url: `${activeBase}/mcp`,
        headers: {
          Authorization: `Bearer ${snap.auth_token}`,
        },
      },
    },
  };

  return {
    title: meta.title,
    filename: meta.filename,
    instructions: meta.instructions,
    content: JSON.stringify(payload, null, 2),
  };
}
