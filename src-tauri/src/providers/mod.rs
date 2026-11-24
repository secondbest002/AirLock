mod anthropic;
mod common;
mod openai;
#[cfg(test)]
mod tests;

pub use anthropic::{
    anthropic_known_models, process_anthropic_sse_data_line, run_anthropic_turn, AnthropicBlockAcc,
    AnthropicSseAccumulator,
};
pub use common::{
    all_mcp_tools, await_with_watchdog, build_attachment_text_context, emit_stream_chunk,
    emit_stream_step, emit_stream_thought, execute_tool_calls_hybrid, format_interrupted_reply,
    load_project_rules_snippet, load_watchdog_settings, sanitize_schema_for_gemini,
    AgentAttachment, PendingToolCall, ProviderTurnOutcome, WatchdogInterrupt,
};
pub use openai::{
    fetch_openai_compat_models, process_openai_sse_data_line, run_openai_compat_turn,
    OpenAiSseAccumulator,
};
