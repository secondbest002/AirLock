export type GuardrailMode = "read_only" | "safe_edit" | "autonomous";
export type AgentProvider = "gemini" | "openai_compat" | "anthropic";

export interface AuditEntry {
  id: string;
  timestamp: string;
  source: string;
  tool_name: string;
  target_path: string | null;
  summary: string;
  diff_text: string | null;
  can_revert: boolean;
  reverted: boolean;
  tokens_saved: number;
  original_blob_hash?: string | null;
  group_id?: string | null;
  move_from_path?: string | null;
}

export interface TokenMetrics {
  tokens_saved_total: number;
  turns_saved_total: number;
  tool_calls_total: number;
}

export interface HttpTrafficEntry {
  timestamp: string;
  method: string;
  path: string;
  rpc_method: string | null;
  status: number;
  duration_ms: number;
  note: string;
  request_preview?: string | null;
  response_preview?: string | null;
}

export interface OAuthClientSummary {
  client_id: string;
  client_name: string | null;
  trusted: boolean;
  redirect_uris: string[];
  active_access_tokens: number;
  active_refresh_tokens: number;
  created_at_epoch: number;
  last_used_epoch: number | null;
}

export interface PendingOAuthInfo {
  request_id: string;
  client_id: string;
  client_name: string | null;
  redirect_uri: string;
  scope: string;
  created_at: string;
}

export interface PendingToolApproval {
  request_id: string;
  source: string;
  tool_name: string;
  summary: string;
  args_preview: string;
  created_at: string;
}

export type JobStatus = "running" | "completed" | "failed" | "killed" | "timed_out";

export interface JobSummary {
  job_id: string;
  command: string;
  label: string | null;
  status: JobStatus;
  exit_code: number | null;
  started_at: string;
  elapsed_secs: number;
  line_count: number;
}

export interface DownstreamMcpSummary {
  id: string;
  name: string;
  namespace: string;
  transport: string;
  url: string;
  has_bearer_token: boolean;
  command: string;
  args: string[];
  enabled: boolean;
  status: string;
  tools_count: number;
  last_error: string | null;
}

export interface DownstreamMcpConfigInput {
  id: string;
  name: string;
  namespace: string;
  transport: "http" | "stdio";
  url: string;
  bearer_token: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  enabled: boolean;
}

export interface DashboardSnapshot {
  workspace_root: string;
  server_running: boolean;
  server_port: number;
  auth_token: string;
  tunnel_running: boolean;
  tunnel_url: string | null;
  tunnel_status: string;
  tunnel_provider: string;
  idle_timeout_mins: number;
  close_to_tray: boolean;
  allow_sensitive_reads: boolean;
  guardrail_mode: GuardrailMode;
  recent_workspaces: string[];
  pinned_workspaces: string[];
  git_branch: string | null;
  has_cloudflare_token: boolean;
  cloudflare_public_url: string;
  tunnel_logs: string[];
  agent_running: boolean;
  agent_provider: AgentProvider;
  openai_base_url: string;
  openai_model: string;
  anthropic_model: string;
  auto_inject_project_rules: boolean;
  gemini_model: string;
  has_api_key: boolean;
  has_gemini_api_key: boolean;
  has_openai_api_key: boolean;
  has_anthropic_api_key: boolean;
  metrics: TokenMetrics;
  audit_logs: AuditEntry[];
  http_traffic: HttpTrafficEntry[];
  oauth_clients: OAuthClientSummary[];
  pending_oauth_requests: PendingOAuthInfo[];
  pending_tool_approvals: PendingToolApproval[];
  background_jobs: JobSummary[];
  downstream_mcps: DownstreamMcpSummary[];
  aggregated_tools_count: number;
}

export interface ToolStepTrace {
  tool_name: string;
  summary: string;
  tokens_saved: number;
}

export interface TokenUsage {
  prompt_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  total_tokens: number;
}

export interface GeminiModelInfo {
  id: string;
  display_name: string;
  description: string;
  input_token_limit: number;
}

export interface AgentTurnResponse {
  reply: string;
  steps: ToolStepTrace[];
  usage: TokenUsage;
  cancelled: boolean;
  snapshot: DashboardSnapshot;
}

export interface AuditAddedPayload {
  entry: AuditEntry;
  metrics: TokenMetrics;
}

export interface DiagnosticReport {
  server_running: boolean;
  port: number;
  health_ok: boolean;
  mcp_initialize_ok: boolean;
  tools_count: number;
  latency_ms: number;
  tunnel_active: boolean;
  tunnel_url: string | null;
  oauth_clients_count: number;
  notes: string[];
}

export interface ChatMessage {
  role: "user" | "assistant";
  content: string;
  thought?: string;
  steps?: ToolStepTrace[];
  usage?: TokenUsage;
  cancelled?: boolean;
  timestamp: string;
}

export interface AgentAttachment {
  kind: "file" | "image";
  name: string;
  path?: string;
  mime_type?: string;
  data_base64?: string;
}

export interface StoredChatStep {
  tool_name: string;
  summary: string;
  tokens_saved: number;
}

export interface StoredChatUsage {
  prompt_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  total_tokens: number;
}

export interface StoredChatMessage {
  role: "user" | "assistant";
  content: string;
  thought: string | null;
  steps: StoredChatStep[];
  usage: StoredChatUsage | null;
  cancelled: boolean;
  timestamp: string;
}

export interface ChatSession {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  messages: StoredChatMessage[];
}

export interface CommandPaletteAction {
  id: string;
  label: string;
  hint: string;
  run: () => void | Promise<void>;
}
