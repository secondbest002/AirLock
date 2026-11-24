use super::*;
use serde_json::json;

#[test]
fn test_sanitize_schema_for_gemini() {
    let raw = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Demo",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": ["string", "null"],
                "default": "abc",
                "description": "Query string"
            }
        }
    });
    let cleaned = sanitize_schema_for_gemini(&raw);
    assert!(cleaned.get("$schema").is_none());
    assert!(cleaned.get("additionalProperties").is_none());
    assert!(cleaned.get("title").is_none());
    assert_eq!(cleaned["properties"]["query"]["type"], "string");
    assert!(cleaned["properties"]["query"].get("default").is_none());
}

#[test]
fn test_openai_sse_delta_accumulation() {
    let mut acc = OpenAiSseAccumulator::default();
    let c1 = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Check ","content":""}}]}"#;
    let (t1, th1) = process_openai_sse_data_line(&mut acc, c1).unwrap();
    assert_eq!(t1, "");
    assert_eq!(th1, "Check ");

    let c2 = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_99","function":{"name":"smart_","arguments":"{\"query\":"}}]}}]}"#;
    process_openai_sse_data_line(&mut acc, c2).unwrap();

    let c3 = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"search","arguments":"\"main\"}"}}]}}],"usage":{"prompt_tokens":90,"completion_tokens":15,"total_tokens":105}}"#;
    process_openai_sse_data_line(&mut acc, c3).unwrap();

    assert_eq!(acc.thought_buf, "Check ");
    assert_eq!(acc.tool_calls.len(), 1);
    assert_eq!(acc.tool_calls[&0].id, "call_99");
    assert_eq!(acc.tool_calls[&0].name, "smart_search");
    assert_eq!(acc.tool_calls[&0].arguments, r#"{"query":"main"}"#);
    assert_eq!(acc.usage.total_tokens, 105);
}

#[test]
fn test_anthropic_sse_block_parsing() {
    let mut acc = AnthropicSseAccumulator::default();
    process_anthropic_sse_data_line(
        &mut acc,
        r#"{"type":"message_start","message":{"usage":{"input_tokens":140,"cache_read_input_tokens":40}}}"#,
    )
    .unwrap();
    process_anthropic_sse_data_line(
        &mut acc,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
    )
    .unwrap();
    let (_, th) = process_anthropic_sse_data_line(
        &mut acc,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan..."}}"#,
    )
    .unwrap();
    assert_eq!(th, "Plan...");

    process_anthropic_sse_data_line(
        &mut acc,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file_range","input":{}}}"#,
    )
    .unwrap();
    process_anthropic_sse_data_line(
        &mut acc,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/main.rs\"}"}}"#,
    )
    .unwrap();
    process_anthropic_sse_data_line(
        &mut acc,
        r#"{"type":"message_delta","usage":{"output_tokens":32}}"#,
    )
    .unwrap();

    assert_eq!(acc.thought_buf, "Plan...");
    assert_eq!(acc.usage.prompt_tokens, 140);
    assert_eq!(acc.usage.cached_tokens, 40);
    assert_eq!(acc.usage.output_tokens, 32);
    assert_eq!(acc.usage.total_tokens, 172);
    match acc.blocks.get(&1).unwrap() {
        AnthropicBlockAcc::ToolUse {
            id,
            name,
            input_json,
        } => {
            assert_eq!(id, "toolu_1");
            assert_eq!(name, "read_file_range");
            assert_eq!(input_json, r#"{"path":"src/main.rs"}"#);
        }
        _ => panic!("Expected ToolUse block"),
    }
}
