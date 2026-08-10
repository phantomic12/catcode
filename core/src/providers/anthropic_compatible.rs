use super::adapter::{
    normalize_http_error, BuiltProviderRequest, ProviderAdapter, ProviderError, ProviderProtocol,
    ProviderRequest,
};
use super::capabilities::ProviderCapabilities;
use super::streaming::{NormalizedStreamEvent, ToolCallDelta};
use serde_json::Value;

pub struct AnthropicCompatibleAdapter;

impl ProviderAdapter for AnthropicCompatibleAdapter {
    fn id(&self) -> &'static str {
        "anthropic_compatible"
    }

    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::AnthropicMessages
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            parallel_tools: true,
            reasoning: true,
            vision: true,
            usage: true,
            model_discovery: true,
        }
    }

    fn build_request(&self, input: &ProviderRequest<'_>) -> Result<BuiltProviderRequest, String> {
        let max_tokens = if input.max_tokens == 0 {
            8192
        } else {
            input.max_tokens
        };
        let mut body = crate::message::build_anthropic_request(
            input.messages,
            input.tools,
            input.reasoning_effort,
            input.thinking_levels,
            max_tokens,
        );
        body["stream"] = Value::Bool(true);
        body["model"] = Value::String(input.model.to_string());
        // MiniMax's Anthropic-compatible OpenAPI only accepts thinking
        // {type: adaptive|disabled}. The standard Anthropic enabled+budget_tokens
        // shape is rewritten here so M3/M2.x turns do not 400 — on first-party
        // MiniMax hosts and on OpenCode Go MiniMax model ids.
        let minimax_host = crate::provider::is_minimax(&input.provider.base_url);
        let minimax_model = input.model.to_ascii_lowercase().contains("minimax");
        if minimax_host || minimax_model {
            let off = input.reasoning_effort.eq_ignore_ascii_case("none")
                || input.reasoning_effort.is_empty()
                || input.thinking_levels.is_empty();
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "thinking".into(),
                    serde_json::json!({
                        "type": if off { "disabled" } else { "adaptive" }
                    }),
                );
            }
        }
        Ok(BuiltProviderRequest {
            url: format!("{}/messages", input.provider.base_url.trim_end_matches('/')),
            body,
            notices: Vec::new(),
        })
    }

    fn decode_stream_event(&self, value: &Value) -> Vec<NormalizedStreamEvent> {
        decode_anthropic_chunk(value)
    }

    fn normalize_error(&self, status: Option<u16>, body: &str) -> ProviderError {
        normalize_http_error(status, body)
    }
}

pub(crate) fn decode_anthropic_chunk(value: &Value) -> Vec<NormalizedStreamEvent> {
    let mut events = Vec::new();
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "message_start" => {
            let usage = value
                .get("message")
                .and_then(|message| message.get("usage"));
            if let Some(usage) = usage {
                events.push(NormalizedStreamEvent::Usage {
                    input_tokens: usage.get("input_tokens").and_then(token_count),
                    output_tokens: usage.get("output_tokens").and_then(token_count),
                    cached_tokens: usage.get("cache_read_input_tokens").and_then(token_count),
                });
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = value.get("content_block").unwrap_or(&Value::Null);
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                events.push(NormalizedStreamEvent::ToolCallStart(ToolCallDelta {
                    index,
                    id: block.get("id").and_then(Value::as_str).map(str::to_string),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments: None,
                }));
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                "text_delta" => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        events.push(NormalizedStreamEvent::TextDelta(text.to_string()));
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                        events.push(NormalizedStreamEvent::ReasoningDelta(text.to_string()));
                    }
                }
                "input_json_delta" => {
                    events.push(NormalizedStreamEvent::ToolCallDelta(ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments: delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }));
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            events.push(NormalizedStreamEvent::ToolCallComplete { index });
        }
        "message_delta" => {
            if let Some(reason) = value
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
            {
                events.push(NormalizedStreamEvent::FinishReason(
                    match reason {
                        "end_turn" | "stop_sequence" => "stop",
                        "tool_use" => "tool_calls",
                        "max_tokens" => "length",
                        other => other,
                    }
                    .to_string(),
                ));
            }
            if let Some(usage) = value.get("usage") {
                events.push(NormalizedStreamEvent::Usage {
                    input_tokens: usage.get("input_tokens").and_then(token_count),
                    output_tokens: usage.get("output_tokens").and_then(token_count),
                    cached_tokens: usage.get("cache_read_input_tokens").and_then(token_count),
                });
            }
        }
        "error" => {
            let error = value.get("error").unwrap_or(value);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("anthropic stream error")
                .to_string();
            let retryable = {
                use crate::providers::adapter::is_non_retryable_provider_message;
                let kind = error
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // Rate-limit / balance / auth / invalid request → fail fast.
                // Everything else (overloaded, api_error, unknown) → retry.
                !(is_non_retryable_provider_message(&message)
                    || matches!(
                        kind,
                        "rate_limit_error" | "authentication_error" | "invalid_request_error"
                    ))
            };
            events.push(if retryable {
                NormalizedStreamEvent::RetryableError(message)
            } else {
                NormalizedStreamEvent::FatalError(message)
            });
        }
        _ => {}
    }
    events
}

fn token_count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|number| number as u64))
        .or_else(|| value.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_anthropic_text_tools_usage_finish_and_errors() {
        assert_eq!(
            decode_anthropic_chunk(
                &json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}})
            ),
            vec![NormalizedStreamEvent::TextDelta("hi".into())]
        );
        assert!(matches!(
            decode_anthropic_chunk(&json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call","name":"read_file","input":{}}})).as_slice(),
            [NormalizedStreamEvent::ToolCallStart(delta)] if delta.index == 2 && delta.id.as_deref() == Some("call")
        ));
        assert_eq!(
            decode_anthropic_chunk(
                &json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}})
            ),
            vec![
                NormalizedStreamEvent::FinishReason("tool_calls".into()),
                NormalizedStreamEvent::Usage {
                    input_tokens: None,
                    output_tokens: Some(4),
                    cached_tokens: None
                }
            ]
        );
        assert_eq!(
            decode_anthropic_chunk(
                &json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})
            ),
            vec![NormalizedStreamEvent::RetryableError("busy".into())]
        );
    }

    #[test]
    fn max_tokens_zero_defaults_to_8192_not_zero_or_one() {
        // Anthropic requires max_tokens; 0 must not reach the wire, and we
        // deliberately avoid a 1-token floor that would truncate replies.
        use crate::config::{ProviderKind, ResolvedProvider};
        use crate::message::Message;
        use crate::providers::adapter::ProviderRequest;

        let provider = ResolvedProvider {
            name: "anthropic".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: Some("sk-test".into()),
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let messages = [Message::user("hi")];
        let built = AnthropicCompatibleAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "claude-sonnet-4",
                messages: &messages,
                tools: &[],
                reasoning_effort: "none",
                thinking_levels: &[],
                max_tokens: 0,
            })
            .expect("build");
        assert_eq!(built.body["max_tokens"], 8192);
        assert_eq!(built.body["stream"], true);
        assert_eq!(built.body["model"], "claude-sonnet-4");
    }

    #[test]
    fn minimax_rewrites_thinking_to_adaptive() {
        use crate::config::{ProviderKind, ResolvedProvider};
        use crate::message::Message;
        use crate::providers::adapter::ProviderRequest;

        let provider = ResolvedProvider {
            name: "minimax".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.minimax.io/anthropic".into(),
            api_key: Some("sk-test".into()),
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let messages = [Message::user("hi")];
        let levels = ["low".to_string(), "medium".to_string(), "high".to_string()];
        let built = AnthropicCompatibleAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "MiniMax-M3",
                messages: &messages,
                tools: &[],
                reasoning_effort: "high",
                thinking_levels: &levels,
                max_tokens: 4096,
            })
            .expect("build");
        assert_eq!(built.body["thinking"]["type"], "adaptive");
        assert!(built.body["thinking"].get("budget_tokens").is_none());

        let off = AnthropicCompatibleAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "MiniMax-M3",
                messages: &messages,
                tools: &[],
                reasoning_effort: "none",
                thinking_levels: &levels,
                max_tokens: 4096,
            })
            .expect("build");
        assert_eq!(off.body["thinking"]["type"], "disabled");
    }

    #[test]
    fn thinking_delta_and_partial_json_and_cache_usage_decode() {
        assert_eq!(
            decode_anthropic_chunk(&json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "hmm"}
            })),
            vec![NormalizedStreamEvent::ReasoningDelta("hmm".into())]
        );
        assert!(matches!(
            decode_anthropic_chunk(&json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"a\""}
            })).as_slice(),
            [NormalizedStreamEvent::ToolCallDelta(d)]
                if d.index == 1 && d.arguments.as_deref() == Some("{\"a\"")
        ));
        // message_start may carry cache_read_input_tokens; surface as cached.
        assert_eq!(
            decode_anthropic_chunk(&json!({
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 40
                    }
                }
            })),
            vec![NormalizedStreamEvent::Usage {
                input_tokens: Some(100),
                output_tokens: Some(0),
                cached_tokens: Some(40),
            }]
        );
        // Non-retryable stream error stays fatal.
        assert_eq!(
            decode_anthropic_chunk(&json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "bad"}
            })),
            vec![NormalizedStreamEvent::FatalError("bad".into())]
        );
        // stop_sequence / max_tokens map to OpenAI-ish finish reasons.
        assert_eq!(
            decode_anthropic_chunk(&json!({
                "type": "message_delta",
                "delta": {"stop_reason": "max_tokens"}
            })),
            vec![NormalizedStreamEvent::FinishReason("length".into())]
        );
    }

    #[test]
    fn content_block_start_ignores_non_tool_blocks() {
        // text / thinking starts must not fabricate ToolCallStart (that would
        // poison tool_args assembly with empty placeholders).
        assert!(decode_anthropic_chunk(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        }))
        .is_empty());
        assert!(decode_anthropic_chunk(&json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "thinking", "thinking": ""}
        }))
        .is_empty());
    }
}
