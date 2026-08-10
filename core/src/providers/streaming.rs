use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NormalizedStreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart(ToolCallDelta),
    ToolCallDelta(ToolCallDelta),
    ToolCallComplete {
        index: usize,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
    },
    FinishReason(String),
    RetryableError(String),
    FatalError(String),
}

/// Read a token count from a usage field, tolerating integer, float, and
/// string encodings different OpenAI-compatible servers emit.
fn token_count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().filter(|n| *n >= 0).map(|n| n as u64))
        .or_else(|| {
            value.as_f64().and_then(|number| {
                if number.is_finite() && number >= 0.0 {
                    Some(number as u64)
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            value
                .as_str()?
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite() && *number >= 0.0)
                .map(|number| number as u64)
        })
}

/// Hard cap on streamed tool-call `index` growth. A malicious or buggy gateway
/// can emit `index: 1e9` and OOM the process if the accumulator grows unbounded
/// (`while len <= idx { push }`). Parallel tool batches stay well under this.
pub(crate) const MAX_STREAM_TOOL_CALL_INDEX: usize = 64;

/// Cap total argument bytes across all streamed tool calls in one turn.
pub(crate) const MAX_STREAM_TOOL_ARGS_BYTES: usize = 8 * 1024 * 1024;

/// Tool-call indices arrive as ints, floats (`1.0`), or quoted numbers on some
/// gateways. Falling back to `0` silently merges parallel tool streams.
fn tool_call_index(value: Option<&Value>) -> usize {
    value
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().filter(|n| *n >= 0).map(|n| n as u64))
                .or_else(|| {
                    v.as_f64().and_then(|number| {
                        if number.is_finite() && number >= 0.0 {
                            Some(number as u64)
                        } else {
                            None
                        }
                    })
                })
                .or_else(|| {
                    v.as_str()?
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|number| number.is_finite() && *number >= 0.0)
                        .map(|number| number as u64)
                })
        })
        .unwrap_or(0) as usize
}

/// Ensure `tool_calls` has a slot at `idx` without unbounded allocation (H3).
pub(crate) fn ensure_stream_tool_slot<T: Default>(
    tool_calls: &mut Vec<T>,
    idx: usize,
) -> Result<(), String> {
    if idx >= MAX_STREAM_TOOL_CALL_INDEX {
        return Err(format!(
            "stream tool_call index {idx} exceeds cap ({MAX_STREAM_TOOL_CALL_INDEX})"
        ));
    }
    while tool_calls.len() <= idx {
        tool_calls.push(T::default());
    }
    Ok(())
}

/// Append a tool-call argument fragment if total args stay under the byte cap.
pub(crate) fn append_stream_tool_args(
    total_args_bytes: &mut usize,
    acc_args: &mut String,
    fragment: &str,
) -> Result<(), String> {
    let add = fragment.len();
    let next = total_args_bytes.saturating_add(add);
    if next > MAX_STREAM_TOOL_ARGS_BYTES {
        return Err(format!(
            "stream tool_call arguments exceed cap ({MAX_STREAM_TOOL_ARGS_BYTES} bytes)"
        ));
    }
    *total_args_bytes = next;
    acc_args.push_str(fragment);
    Ok(())
}

/// Some providers stream `function.arguments` as a JSON object/array instead of
/// a string fragment. Convert those to a compact JSON string so the accumulator
/// can still assemble a valid arguments payload.
fn tool_call_arguments(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if value.is_null() {
        return None;
    }
    // Object/array/number/bool — serialize so history stays API-valid JSON text.
    Some(value.to_string())
}

fn usage_field(usage: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| usage.get(*key).and_then(token_count))
}

fn cached_tokens_from_usage(usage: &Value) -> Option<u64> {
    usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(token_count)
        .or_else(|| {
            usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(token_count)
        })
        .or_else(|| usage.get("cache_read_input_tokens").and_then(token_count))
        .or_else(|| usage.get("cached_tokens").and_then(token_count))
}

fn normalize_finish_reason(reason: &str) -> String {
    match reason {
        // Older OpenAI wire name; harness + Anthropic path use `tool_calls`.
        "function_call" => "tool_calls".into(),
        other => other.to_string(),
    }
}

fn stream_error_detail(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| error.as_str().map(str::to_string))
        .unwrap_or_else(|| error.to_string())
}

fn is_retryable_stream_error(error: &Value, detail: &str) -> bool {
    use crate::providers::adapter::is_non_retryable_provider_message;

    let code = error
        .get("code")
        .and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| code.as_i64().map(|n| n.to_string()))
                .or_else(|| code.as_u64().map(|n| n.to_string()))
        })
        .unwrap_or_default();
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code_l = code.to_ascii_lowercase();
    let kind_l = kind.to_ascii_lowercase();
    let detail_l = detail.to_ascii_lowercase();

    // Rate limits and balance/billing fail fast — check type/code/message.
    if is_non_retryable_provider_message(detail)
        || matches!(
            code_l.as_str(),
            "rate_limit"
                | "rate_limit_exceeded"
                | "rate_limit_error"
                | "insufficient_quota"
                | "insufficient_balance"
                | "billing_not_active"
                | "429"
                | "402"
        )
        || matches!(
            kind_l.as_str(),
            "rate_limit" | "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota"
        )
    {
        return false;
    }

    // Permanent account/request failures stay fatal even if not rate/balance.
    // Message phrases catch vendors that invent types (e.g. cursor_sdk_error
    // with "authentication failed"). Keep matches specific — bare tokens like
    // "forbidden"/"authentication" alone false-positive rate-limit copy.
    if matches!(
        code_l.as_str(),
        "invalid_api_key"
            | "invalid_request"
            | "invalid_request_error"
            | "authentication_error"
            | "permission_error"
            | "not_found_error"
            | "context_length_exceeded"
            | "401"
            | "403"
            | "404"
            | "422"
    ) || matches!(
        kind_l.as_str(),
        "invalid_request_error" | "authentication_error" | "permission_error" | "not_found_error"
    ) || detail_l.contains("authentication failed")
        || detail_l.contains("authentication error")
        || detail_l.contains("unauthorized")
        || detail_l.contains("invalid_api_key")
        || detail_l.contains("invalid api key")
        || detail_l.contains("invalid credentials")
        || detail_l.contains("access denied")
        || detail_l.contains("permission denied")
        || detail_l.contains("context_length")
        || detail_l.contains("context length")
        || detail_l.contains("maximum context")
        || detail_l.contains("prompt is too long")
        || detail_l.contains("input is too long")
    {
        return false;
    }

    // Explicit transient codes/kinds.
    if matches!(
        code_l.as_str(),
        "server_error"
            | "overloaded"
            | "overloaded_error"
            | "timeout"
            | "500"
            | "502"
            | "503"
            | "504"
    ) || matches!(
        kind_l.as_str(),
        "server_error" | "overloaded" | "overloaded_error" | "api_error" | "timeout"
    ) || detail_l.contains("overloaded")
        || detail_l.contains("temporarily unavailable")
        || detail_l.contains("server_error")
    {
        return true;
    }

    // Default: retry any other provider stream error (user policy — everything
    // except rate-limit / balance). Permanent codes already returned false.
    true
}

/// Normalize one parsed OpenAI-compatible SSE data object. Transport framing,
/// retry decisions, accumulation, and user-visible emission remain outside
/// this pure decoder.
fn reasoning_delta_text(delta: &Value) -> Option<String> {
    if let Some(text) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }
    // MiniMax OpenAI wire with reasoning_split=true may stream reasoning_details
    // as a string or as [{text|content|type:reasoning_text}]. Official streaming
    // examples re-send the full cumulative text each chunk; de-cumulation happens
    // in append_stream_fragment at the accumulate sites, not here.
    let details = delta.get("reasoning_details")?;
    if let Some(text) = details.as_str().filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    let arr = details.as_array()?;
    let mut out = String::new();
    for item in arr {
        if let Some(text) = item
            .as_str()
            .or_else(|| item.get("text").and_then(Value::as_str))
            .or_else(|| item.get("content").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
        {
            out.push_str(text);
            continue;
        }
        if item.get("type").and_then(Value::as_str) == Some("reasoning_text") {
            if let Some(text) = item
                .get("text")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                out.push_str(text);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub(crate) fn decode_openai_chunk(object: &Value) -> Vec<NormalizedStreamEvent> {
    let mut events = Vec::new();
    if let Some(error) = object.get("error") {
        let detail = stream_error_detail(error);
        events.push(if is_retryable_stream_error(error, &detail) {
            NormalizedStreamEvent::RetryableError(detail)
        } else {
            NormalizedStreamEvent::FatalError(detail)
        });
        // Never treat an error frame as a successful text/tool completion, even
        // when a partial `choices` payload is also present.
        return events;
    }
    if let Some(usage) = object.get("usage") {
        // Accept both OpenAI (`prompt_tokens`/`completion_tokens`) and the
        // Anthropic-ish aliases some OpenAI-compatible proxies emit.
        events.push(NormalizedStreamEvent::Usage {
            input_tokens: usage_field(usage, &["prompt_tokens", "input_tokens"]),
            output_tokens: usage_field(usage, &["completion_tokens", "output_tokens"]),
            cached_tokens: cached_tokens_from_usage(usage),
        });
    }
    let Some(choice) = object.get("choices").and_then(|choices| choices.get(0)) else {
        return events;
    };
    if let Some(delta) = choice.get("delta") {
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            events.push(NormalizedStreamEvent::TextDelta(text.to_string()));
        }
        if let Some(reasoning) = reasoning_delta_text(delta) {
            events.push(NormalizedStreamEvent::ReasoningDelta(reasoning));
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            events.extend(tool_calls.iter().map(|tool_call| {
                let function = tool_call.get("function");
                NormalizedStreamEvent::ToolCallDelta(ToolCallDelta {
                    index: tool_call_index(tool_call.get("index")),
                    id: tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name: function
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    arguments: tool_call_arguments(
                        function.and_then(|value| value.get("arguments")),
                    ),
                })
            }));
        }
    }
    if let Some(reason) = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
    {
        events.push(NormalizedStreamEvent::FinishReason(
            normalize_finish_reason(reason),
        ));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let text = match name {
            "text" => include_str!("../../tests/fixtures/providers/openai_text.json"),
            "tools" => include_str!("../../tests/fixtures/providers/openai_tools.json"),
            "error" => include_str!("../../tests/fixtures/providers/openai_error.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn normalizes_text_reasoning_usage_and_finish() {
        assert_eq!(
            decode_openai_chunk(&fixture("text")),
            vec![
                NormalizedStreamEvent::Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(2),
                    cached_tokens: Some(4),
                },
                NormalizedStreamEvent::TextDelta("hello".into()),
                NormalizedStreamEvent::ReasoningDelta("think".into()),
                NormalizedStreamEvent::FinishReason("stop".into()),
            ]
        );
    }

    #[test]
    fn normalizes_multiple_fragmented_tool_calls() {
        let events = decode_openai_chunk(&fixture("tools"));
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            NormalizedStreamEvent::ToolCallDelta(delta)
                if delta.index == 0 && delta.name.as_deref() == Some("read_file")
        ));
        assert!(matches!(
            &events[1],
            NormalizedStreamEvent::ToolCallDelta(delta)
                if delta.index == 1 && delta.arguments.as_deref() == Some("{\"path\":")
        ));
    }

    #[test]
    fn normalizes_provider_error_without_panicking() {
        // Unclassified provider stream errors default to retryable.
        assert_eq!(
            decode_openai_chunk(&fixture("error")),
            vec![NormalizedStreamEvent::RetryableError("busy".into())]
        );
    }

    #[test]
    fn rate_limit_stream_errors_are_fatal() {
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": { "type": "rate_limit_error", "message": "slow down" }
            })),
            vec![NormalizedStreamEvent::FatalError("slow down".into())]
        );
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": { "code": "rate_limit_exceeded", "message": "too many requests" }
            })),
            vec![NormalizedStreamEvent::FatalError(
                "too many requests".into()
            )]
        );
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": { "message": "insufficient credits — top up" }
            })),
            vec![NormalizedStreamEvent::FatalError(
                "insufficient credits — top up".into()
            )]
        );
        // Vendor-specific type still fatal when message is clearly auth.
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": {
                    "type": "cursor_sdk_error",
                    "message": "Cursor SDK authentication failed"
                }
            })),
            vec![NormalizedStreamEvent::FatalError(
                "Cursor SDK authentication failed".into()
            )]
        );
        // Context-length message path (no standard code/type).
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": {
                    "type": "provider_error",
                    "message": "prompt is too long for this model"
                }
            })),
            vec![NormalizedStreamEvent::FatalError(
                "prompt is too long for this model".into()
            )]
        );
        // Rate-limit payload that merely mentions "forbidden" stays Retryable
        // only when type/code are not rate-limit; with rate_limit type it is Fatal.
        // Bare incidental words must NOT flip an otherwise-retryable server blip.
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": {
                    "type": "server_error",
                    "message": "upstream said forbidden briefly; retry"
                }
            })),
            vec![NormalizedStreamEvent::RetryableError(
                "upstream said forbidden briefly; retry".into()
            )]
        );
    }

    #[test]
    fn retryable_error_from_type_and_string_error() {
        assert_eq!(
            decode_openai_chunk(&json!({
                "error": { "type": "server_error", "message": "blip" }
            })),
            vec![NormalizedStreamEvent::RetryableError("blip".into())]
        );
        assert_eq!(
            decode_openai_chunk(&json!({ "error": "overloaded right now" })),
            vec![NormalizedStreamEvent::RetryableError(
                "overloaded right now".into()
            )]
        );
    }

    #[test]
    fn token_count_accepts_int_float_and_string() {
        assert_eq!(token_count(&json!(12)), Some(12));
        assert_eq!(token_count(&json!(12.0)), Some(12));
        assert_eq!(token_count(&json!("34")), Some(34));
        assert_eq!(token_count(&json!(" 56.0 ")), Some(56));
        assert_eq!(token_count(&json!(-1)), None);
        assert_eq!(token_count(&json!("n/a")), None);
    }

    #[test]
    fn usage_only_frame_is_not_dropped() {
        // Final OpenAI chunk often has empty choices + usage only.
        let events = decode_openai_chunk(&json!({
            "choices": [],
            "usage": {
                "prompt_tokens": "11",
                "completion_tokens": 3.0,
                "prompt_tokens_details": { "cached_tokens": "2" }
            }
        }));
        assert_eq!(
            events,
            vec![NormalizedStreamEvent::Usage {
                input_tokens: Some(11),
                output_tokens: Some(3),
                cached_tokens: Some(2),
            }]
        );
    }

    #[test]
    fn usage_accepts_anthropic_style_aliases() {
        let events = decode_openai_chunk(&json!({
            "usage": {
                "input_tokens": "9",
                "output_tokens": 4,
                "cache_read_input_tokens": "1"
            }
        }));
        assert_eq!(
            events,
            vec![NormalizedStreamEvent::Usage {
                input_tokens: Some(9),
                output_tokens: Some(4),
                cached_tokens: Some(1),
            }]
        );
    }

    #[test]
    fn error_frame_is_never_success_even_with_choices() {
        let events = decode_openai_chunk(&json!({
            "error": { "message": "nope", "code": "invalid_request" },
            "choices": [{ "delta": { "content": "should not emit" }, "finish_reason": "stop" }]
        }));
        assert_eq!(
            events,
            vec![NormalizedStreamEvent::FatalError("nope".into())]
        );
    }

    #[test]
    fn tool_call_index_and_object_arguments_assemble() {
        let events = decode_openai_chunk(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": "1",
                        "id": "call-1",
                        "function": {
                            "name": "bash",
                            "arguments": {"command": "ls"}
                        }
                    }]
                }
            }]
        }));
        assert_eq!(events.len(), 1);
        match &events[0] {
            NormalizedStreamEvent::ToolCallDelta(delta) => {
                assert_eq!(delta.index, 1);
                assert_eq!(delta.id.as_deref(), Some("call-1"));
                assert_eq!(delta.name.as_deref(), Some("bash"));
                assert_eq!(delta.arguments.as_deref(), Some(r#"{"command":"ls"}"#));
            }
            other => panic!("expected tool delta, got {other:?}"),
        }
    }

    #[test]
    fn ensure_stream_tool_slot_rejects_malicious_high_index() {
        let mut slots: Vec<u8> = Vec::new();
        assert!(ensure_stream_tool_slot(&mut slots, 0).is_ok());
        assert_eq!(slots.len(), 1);
        assert!(ensure_stream_tool_slot(&mut slots, MAX_STREAM_TOOL_CALL_INDEX - 1).is_ok());
        assert_eq!(slots.len(), MAX_STREAM_TOOL_CALL_INDEX);
        let err = ensure_stream_tool_slot(&mut slots, MAX_STREAM_TOOL_CALL_INDEX).unwrap_err();
        assert!(err.contains("exceeds cap"), "{err}");
        // Must not allocate multi-GB for a huge index.
        let before = slots.len();
        let _ = ensure_stream_tool_slot(&mut slots, usize::MAX / 2);
        assert_eq!(slots.len(), before);
    }

    #[test]
    fn append_stream_tool_args_caps_total_bytes() {
        let mut total = 0usize;
        let mut acc = String::new();
        append_stream_tool_args(&mut total, &mut acc, "abc").unwrap();
        assert_eq!(total, 3);
        assert_eq!(acc, "abc");
        let big = "x".repeat(MAX_STREAM_TOOL_ARGS_BYTES);
        let err = append_stream_tool_args(&mut total, &mut acc, &big).unwrap_err();
        assert!(err.contains("exceed cap"), "{err}");
        assert_eq!(acc, "abc");
    }

    #[test]
    fn function_call_finish_reason_maps_to_tool_calls() {
        let events = decode_openai_chunk(&json!({
            "choices": [{ "delta": {}, "finish_reason": "function_call" }]
        }));
        assert_eq!(
            events,
            vec![NormalizedStreamEvent::FinishReason("tool_calls".into())]
        );
    }

    #[test]
    fn decodes_minimax_reasoning_details_string_and_array() {
        let string_events = decode_openai_chunk(&json!({
            "choices": [{
                "delta": { "reasoning_details": "step one" }
            }]
        }));
        assert_eq!(
            string_events,
            vec![NormalizedStreamEvent::ReasoningDelta("step one".into())]
        );

        let array_events = decode_openai_chunk(&json!({
            "choices": [{
                "delta": {
                    "reasoning_details": [
                        { "type": "reasoning_text", "text": "alpha" },
                        { "text": " beta" }
                    ]
                }
            }]
        }));
        assert_eq!(
            array_events,
            vec![NormalizedStreamEvent::ReasoningDelta("alpha beta".into())]
        );

        // Prefer explicit reasoning_content over reasoning_details when both present.
        let preferred = decode_openai_chunk(&json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "primary",
                    "reasoning_details": [{ "text": "ignored" }]
                }
            }]
        }));
        assert_eq!(
            preferred,
            vec![NormalizedStreamEvent::ReasoningDelta("primary".into())]
        );
    }
}
