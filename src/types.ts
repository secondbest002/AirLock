export type GuardrailMode = "read_only" | "safe_edit" | "autonomous";
export type AgentProvider = "gemini" | "openai_compat" | "anthropic";
export type GeminiThinkingMode = "auto" | "off" | "low" | "medium" | "high";
export type ExecutionMode = "direct" | "plan_first";
export type ArtifactKind = "implementation_plan" | "task_list" | "walkthrough" | "review_note";
export type ArtifactStatus =
  | "draft"
  | "awaiting_approval"
  | "approved"
  | "changes_requested"
  | "completed";
export type AgentRunStatus =
  | "running"
  | "awaiting_approval"
  | "completed"
  | "cancelled"
  | "failed";

export interface ArtifactComment {
  id: string;
  line_anchor: number | null;
  quote: string | null;
  author: string;
  body: string;
  resolved: boolean;
  created_at: string;
}

export interface ArtifactDocument {
  id: string;
  session_id: string;
  workspace_alias: string;
  kind: ArtifactKind;
  title: string;
  content_md: string;
  status: ArtifactStatus;
  version: number;
  updated_at: string;
  comments: ArtifactComment[];
}

export interface ArtifactFeedbackResult {
  artifact: ArtifactDocument;
  feedback_prompt: string;
}

export interface AgentRunSummary {
  run_id: string;
  parent_run_id: string | null;
  session_id: string;
  workspace_alias: string;
  label: string;
  provider: string;
  model: string;
  execution_mode: string;
  status: AgentRunStatus;
  current_step: number;
  max_steps: number;
  last_activity_text: string;
  started_at: string;
  elapsed_secs: number;
}

export interface WorkflowSummary {
  name: string;
  slash: string;
  title: string;
  description: string;
  source: string;
  execution_mode: ExecutionMode | null;
  prompt_template: string;
}

export interface HookDefinition {
  id: string;
  event: string;
  file_glob: string | null;
  command: string;
  timeout_secs: number;
  max_output_lines: number;
  on_failure: string;
  enabled: boolean;
}

export interface HooksStatusSummary {
  workspace_alias: string;
  workspace_path: string;
  hooks_file_exists: boolean;
  current_sha256: string | null;
  trusted: boolean;
  hash_changed: boolean;
  hooks: HookDefinition[];
  parse_error: string | null;
}

export interface AgentChunkV2Payload {
  run_id: string;
  session_id: string;
  delta: string;
}

export interface AgentThoughtV2Payload {
  run_id: string;
  session_id: string;
  delta: string;
}

export interface AgentStepV2Payload {
  run_id: string;
  session_id: string;
  step: number;
  max_steps: number;
  text: string;
}

export interface WorkspaceSummary {
  id: string;
  alias: string;
  path: string;
  enabled: boolean;
  is_default: boolean;
  guardrail_override: GuardrailMode | null;
  effective_guardrail: GuardrailMode;
  allow_sensitive_override: boolean | null;
  pinned: boolean;
  exists: boolean;
}

export interface TurnCheckpointInfo {
  can_resume: boolean;
  reason: string;
  completed_steps: number;
  last_tool: string | null;
}

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
  workspace_id?: string | null;
  workspace_alias?: string | null;
  workspace_root?: string | null;
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

export type ElicitationAction = "accept" | "decline" | "cancel";

export interface PendingElicitation {
  request_id: string;
  server_id: string;
  server_name: string;
  message: string;
  requested_schema: Record<string, unknown> | null;
  created_at: string;
}

export type JobStatus = "running" | "completed" | "failed" | "killed" | "timed_out";

export interface JobSummary {
  job_id: string;
  command: string;
  label: string | null;
  workspace_alias?: string | null;
  mcp_task_id?: string | null;
  status: JobStatus;
  exit_code: number | null;
  started_at: string;
  elapsed_secs: number;
  line_count: number;
}

export interface LspServerSummary {
  key: string;
  lang_id: string;
  workspace_alias: string;
  workspace_path: string;
  command: string;
  status: string;
  idle_secs: number;
  open_docs_count: number;
  diagnostics_count: number;
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
  plugin_id?: string | null;
}

export type SkillScope = "workspace" | "plugin" | "global";

export interface SkillSummary {
  name: string;
  description: string;
  scope: SkillScope;
  source_label: string;
  plugin_id: string | null;
  enabled: boolean;
  dir_path: string;
  resource_files: string[];
  allowed_tools: string[];
  catalog_tokens_est: number;
  body_tokens_est: number;
  parse_error: string | null;
}

export interface SkillDetail {
  summary: SkillSummary;
  body_md: string;
  raw_md: string;
}

export interface PluginSummary {
  id: string;
  name: string;
  version: string;
  description: string;
  source: string;
  install_dir: string;
  enabled: boolean;
  trusted: boolean;
  manifest_sha256: string;
  mcp_servers_count: number;
  skills_count: number;
  required_secrets: string[];
  missing_secrets: string[];
  configured_secrets: string[];
  installed_at: string;
}

export interface PluginManifest {
  name: string;
  version: string;
  description: string;
  author?: string | null;
  homepage?: string | null;
  tags: string[];
}

export interface PluginMcpPreviewItem {
  server_key: string;
  transport: string;
  command: string;
  args: string[];
  url: string;
  env_keys: string[];
  header_keys: string[];
  secret_placeholders: string[];
}

export interface PluginInstallPreview {
  staging_id: string;
  source_type: string;
  location: string;
  subdir: string | null;
  manifest: PluginManifest;
  manifest_sha256: string;
  mcp_servers: PluginMcpPreviewItem[];
  skills: SkillSummary[];
  required_secrets: string[];
  warnings: string[];
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
  default_workspace_id: string;
  workspaces: WorkspaceSummary[];
  builtin_tools_count: number;
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
  gemini_thinking_mode: GeminiThinkingMode;
  agent_max_steps: number;
  agent_turn_timeout_secs: number;
  stream_idle_timeout_secs: number;
  pending_checkpoint: TurnCheckpointInfo | null;
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
  pending_elicitations?: PendingElicitation[];
  background_jobs: JobSummary[];
  downstream_mcps: DownstreamMcpSummary[];
  aggregated_tools_count: number;
  skills: SkillSummary[];
  plugins: PluginSummary[];
  active_runs?: AgentRunSummary[];
  hooks_status?: HooksStatusSummary | null;
  active_lsp_servers?: LspServerSummary[];
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
  run_id?: string;
  reply: string;
  steps: ToolStepTrace[];
  usage: TokenUsage;
  cancelled: boolean;
  awaiting_approval?: boolean;
  checkpoint?: TurnCheckpointInfo | null;
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
  workspace?: string;
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
