use super::adapter::{
    normalize_http_error, BuiltProviderRequest, ProviderAdapter, ProviderError, ProviderProtocol,
    ProviderRequest,
};
use super::capabilities::ProviderCapabilities;
use super::streaming::{NormalizedStreamEvent, ToolCallDelta};
use crate::message::{Content, ContentPart, Message};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct GoogleCodeAssistAdapter;

/// Google's freemium Code Assist / Antigravity project id used when the user
/// has not configured a Cloud project via headers or env. Many Antigravity
/// clients share this managed project for free-tier OAuth; it is NOT a
/// personal/harness-owned project. Prefer `CODE_ASSIST_PROJECT`,
/// `GOOGLE_CLOUD_PROJECT`, or a project header when available.
const DEFAULT_CODE_ASSIST_FREEMIUM_PROJECT: &str = "rising-fact-p41fc";

impl ProviderAdapter for GoogleCodeAssistAdapter {
    fn id(&self) -> &'static str {
        "google_code_assist"
    }
    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::GoogleCodeAssist
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tools: true,
            parallel_tools: true,
            reasoning: true,
            vision: false,
            usage: true,
            model_discovery: true,
        }
    }
    fn build_request(&self, input: &ProviderRequest<'_>) -> Result<BuiltProviderRequest, String> {
        let mut notices = Vec::new();
        let project = resolve_project(input.provider, &mut notices);
        let model = resolve_model_id(input.model, input.reasoning_effort);
        let (contents, system_instruction) = messages_to_contents(input.messages);
        // Code Assist / GenAI reject empty `contents` (systemInstruction alone
        // is not enough). Fail early with a clear error instead of a 400.
        if contents.is_empty() {
            return Err("google code assist request has no user/model contents \
                 (system-only or empty transcript)"
                .into());
        }
        // Some gateways reject maxOutputTokens:0 ("generate nothing"). A missing
        // model budget used to flow through as 0; floor to 1 so the request is
        // valid without imposing a real harness generation cap.
        let max_out = input.max_tokens.max(1);
        let mut body = json!({
            "model": model, "project": project, "userAgent": "antigravity",
            "request": { "contents": contents, "generationConfig": { "maxOutputTokens": max_out } }
        });
        if let Some(system) = system_instruction {
            body["request"]["systemInstruction"] = system;
        }
        let tools = tools_to_genai(input.tools);
        if !tools.is_empty() {
            body["request"]["tools"] = json!(tools);
        }
        apply_thinking(
            &mut body,
            &model,
            input.reasoning_effort,
            input.thinking_levels,
            &mut notices,
        );
        Ok(BuiltProviderRequest {
            url: format!(
                "{}:streamGenerateContent?alt=sse",
                input.provider.base_url.trim_end_matches('/')
            ),
            body,
            notices,
        })
    }
    fn decode_stream_event(&self, value: &Value) -> Vec<NormalizedStreamEvent> {
        decode_google_chunk(value)
    }
    fn normalize_error(&self, status: Option<u16>, body: &str) -> ProviderError {
        normalize_http_error(status, body)
    }
}

fn resolve_project(
    provider: &crate::config::ResolvedProvider,
    notices: &mut Vec<String>,
) -> String {
    if let Some((_, value)) = provider.headers.iter().find(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "x-goog-user-project" | "cloudaicompanion-project" | "x-code-assist-project"
        )
    }) {
        if !value.is_empty() {
            return value.clone();
        }
    }
    if let Ok(value) = std::env::var("CODE_ASSIST_PROJECT") {
        if !value.is_empty() {
            return value;
        }
    }
    if let Ok(value) = std::env::var("GOOGLE_CLOUD_PROJECT") {
        if !value.is_empty() {
            return value;
        }
    }
    notices.push(format!(
        "no Code Assist project configured (set CODE_ASSIST_PROJECT, \
         GOOGLE_CLOUD_PROJECT, or an x-code-assist-project / \
         cloudaicompanion-project header); \
         using freemium default `{DEFAULT_CODE_ASSIST_FREEMIUM_PROJECT}`"
    ));
    DEFAULT_CODE_ASSIST_FREEMIUM_PROJECT.to_string()
}

pub(crate) fn decode_google_chunk(value: &Value) -> Vec<NormalizedStreamEvent> {
    let response = value.get("response").unwrap_or(value);
    let mut events = Vec::new();
    if let Some(usage) = response.get("usageMetadata") {
        events.push(NormalizedStreamEvent::Usage {
            input_tokens: usage.get("promptTokenCount").and_then(token_count),
            output_tokens: usage.get("candidatesTokenCount").and_then(token_count),
            cached_tokens: usage.get("cachedContentTokenCount").and_then(token_count),
        });
    }
    let Some(candidate) = response
        .get("candidates")
        .and_then(|candidates| candidates.get(0))
    else {
        return events;
    };
    if let Some(parts) = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        for (index, part) in parts.iter().enumerate() {
            if let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                events.push(
                    if part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        NormalizedStreamEvent::ReasoningDelta(text.to_string())
                    } else {
                        NormalizedStreamEvent::TextDelta(text.to_string())
                    },
                );
            }
            if let Some(call) = part.get("functionCall") {
                events.push(NormalizedStreamEvent::ToolCallStart(ToolCallDelta {
                    index,
                    id: None,
                    name: call.get("name").and_then(Value::as_str).map(str::to_string),
                    arguments: Some(
                        call.get("args")
                            .cloned()
                            .unwrap_or_else(|| json!({}))
                            .to_string(),
                    ),
                }));
                events.push(NormalizedStreamEvent::ToolCallComplete { index });
            }
        }
    }
    if let Some(reason) = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty() && *reason != "FINISH_REASON_UNSPECIFIED")
    {
        events.push(NormalizedStreamEvent::FinishReason(reason.to_string()));
    }
    events
}

fn resolve_model_id(model: &str, reasoning_effort: &str) -> String {
    let id = model.strip_prefix("models/").unwrap_or(model);
    let mut id = id.strip_prefix("antigravity-").unwrap_or(id).to_string();
    let lower = id.to_ascii_lowercase();
    // Gemini 3 / 3.1 Pro Antigravity ids take a -low/-high suffix. Flash does not.
    let pro = lower.starts_with("gemini-3") && lower.contains("pro") && !lower.contains("flash");
    if pro && !lower.ends_with("-low") && !lower.ends_with("-high") {
        let tier = if matches!(
            reasoning_effort.to_ascii_lowercase().as_str(),
            "low" | "minimal" | "none" | ""
        ) {
            "low"
        } else {
            "high"
        };
        id = format!("{id}-{tier}");
    }
    id
}

fn model_supports_thinking(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    // Gemini 3 uses thinkingLevel; Gemini 2.5 uses thinkingBudget.
    // Gemini 2.0 and older reject thinkingConfig entirely.
    lower.contains("gemini-3")
        || lower.contains("gemini-2.5")
        || (lower.contains("thinking") && lower.contains("gemini"))
}

fn apply_thinking(
    request: &mut Value,
    model: &str,
    reasoning_effort: &str,
    thinking_levels: &[String],
    notices: &mut Vec<String>,
) {
    if !model_supports_thinking(model) {
        return;
    }
    let lower = model.to_ascii_lowercase();
    // Clamp to model-advertised levels when the turn loop supplied them.
    let effort = if thinking_levels.is_empty() {
        reasoning_effort.to_string()
    } else {
        let resolved = crate::provider::resolve_effort(reasoning_effort, thinking_levels);
        if !resolved.eq_ignore_ascii_case(reasoning_effort) {
            notices.push(format!(
                "reasoning effort '{reasoning_effort}' not supported by model '{model}'; using '{resolved}'"
            ));
        }
        resolved
    };
    let effort_l = effort.to_ascii_lowercase();
    let off = matches!(effort_l.as_str(), "" | "none" | "off");
    if lower.contains("gemini-3") {
        // Gemini 3 Flash: minimal/low/medium/high.
        // Gemini 3 / 3.1 Pro: low/high only. Keep the level in lockstep with
        // resolve_model_id's -low/-high suffix: medium/high/max → high, else low.
        let level = if lower.contains("flash") {
            if off {
                "minimal"
            } else {
                match effort_l.as_str() {
                    "minimal" => "minimal",
                    "low" => "low",
                    "medium" => "medium",
                    "high" | "max" => "high",
                    _ => "medium",
                }
            }
        } else if off || matches!(effort_l.as_str(), "low" | "minimal") {
            "low"
        } else {
            // medium / high / max / unknown non-off → high (matches -high model id).
            if !matches!(effort_l.as_str(), "high" | "max" | "medium") {
                notices.push(format!(
                    "gemini-3 pro thinkingLevel '{effort_l}' is invalid; using 'high'"
                ));
            }
            "high"
        };
        request["request"]["generationConfig"]["thinkingConfig"] =
            json!({"thinkingLevel":level,"includeThoughts":true});
    } else if off {
        request["request"]["generationConfig"]["thinkingConfig"] = json!({"thinkingBudget":0});
    } else {
        let budget = match effort_l.as_str() {
            "low" | "minimal" => 8192,
            "high" | "max" => 32768,
            _ => 16384,
        };
        request["request"]["generationConfig"]["thinkingConfig"] =
            json!({"thinkingBudget":budget,"includeThoughts":true});
    }
}

fn messages_to_contents(messages: &[Message]) -> (Vec<Value>, Option<Value>) {
    let mut contents = Vec::new();
    let mut system = Vec::new();
    let mut call_names = HashMap::<String, String>::new();
    for message in messages {
        match message {
            Message::System { content, .. } => {
                let text = content_text(content);
                if !text.is_empty() {
                    system.push(json!({"text":text}));
                }
            }
            Message::User { content, .. } => {
                let text = content_text(content);
                // Empty user parts are rejected by GenAI ("must have non-empty parts").
                if !text.is_empty() {
                    contents.push(json!({"role":"user","parts":[{"text":text}]}));
                }
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                call_names.clear();
                let mut parts = Vec::new();
                if let Some(text) = content.as_ref().filter(|text| !text.is_empty()) {
                    parts.push(json!({"text":text}));
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        call_names.insert(call.id.clone(), call.function.name.clone());
                        let args = serde_json::from_str(&call.function.arguments)
                            .unwrap_or_else(|_| json!({}));
                        parts.push(json!({"functionCall":{"name":call.function.name,"args":args}}));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
            }
            Message::Tool {
                tool_call_id,
                name,
                content,
            } => {
                let name = name
                    .clone()
                    .or_else(|| call_names.get(tool_call_id).cloned())
                    .unwrap_or_else(|| "unknown".into());
                // GenAI Content roles are only `user` | `model`. functionResponse
                // parts must ride on a user turn — `role: "function"` 400s on
                // cloudcode-pa / generativelanguage.
                contents.push(json!({
                    "role":"user",
                    "parts":[{
                        "functionResponse":{
                            "name":name,
                            "response":{"result":content}
                        }
                    }]
                }));
            }
        }
    }
    let system = (!system.is_empty()).then(|| json!({"parts":system}));
    (contents, system)
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Multimodal(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn tools_to_genai(tools: &[Value]) -> Vec<Value> {
    let declarations = tools
        .iter()
        .filter_map(|tool| tool.get("function"))
        .filter_map(|function| {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            // Gemini rejects functionDeclarations with empty names.
            if name.is_empty() {
                return None;
            }
            let mut declaration = json!({
                "name": name,
                "description": function.get("description").cloned().unwrap_or(json!("")),
            });
            if let Some(parameters) = function.get("parameters") {
                declaration["parameters"] = parameters.clone();
            }
            Some(declaration)
        })
        .collect::<Vec<_>>();
    if declarations.is_empty() {
        Vec::new()
    } else {
        vec![json!({"functionDeclarations": declarations})]
    }
}

fn token_count(value: &Value) -> Option<u64> {
    // Reject negatives: `(-1.0 as u64)` wraps to u64::MAX and corrupts usage.
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    if let Some(n) = value.as_i64() {
        return (n >= 0).then_some(n as u64);
    }
    if let Some(n) = value.as_f64() {
        if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 {
            return Some(n as u64);
        }
        return None;
    }
    value.as_str()?.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderKind, ResolvedProvider};
    use crate::message::{FunctionCall, ToolCall};

    fn provider_with(headers: Vec<(String, String)>) -> ResolvedProvider {
        ResolvedProvider {
            name: "antigravity".into(),
            kind: ProviderKind::OpenAI,
            base_url: "https://cloudcode-pa.googleapis.com/v1internal".into(),
            api_key: Some("ya29.test".into()),
            headers,
            oauth: true,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }

    fn build(
        provider: &ResolvedProvider,
        model: &str,
        messages: &[Message],
        tools: &[Value],
        effort: &str,
        levels: &[String],
        max_tokens: u32,
    ) -> BuiltProviderRequest {
        GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider,
                model,
                messages,
                tools,
                reasoning_effort: effort,
                thinking_levels: levels,
                max_tokens,
            })
            .expect("build_request")
    }

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    #[test]
    fn normalizes_google_reasoning_text_tool_usage_and_finish() {
        let events = decode_google_chunk(
            &json!({"response":{"usageMetadata":{"promptTokenCount":2},"candidates":[{"content":{"parts":[{"text":"think","thought":true},{"text":"answer"},{"functionCall":{"name":"read_file","args":{"path":"a"}}}]},"finishReason":"STOP"}]}}),
        );
        assert!(events.contains(&NormalizedStreamEvent::ReasoningDelta("think".into())));
        assert!(events.contains(&NormalizedStreamEvent::TextDelta("answer".into())));
        assert!(events.iter().any(|event| matches!(event, NormalizedStreamEvent::ToolCallStart(delta) if delta.name.as_deref() == Some("read_file"))));
        assert!(events.contains(&NormalizedStreamEvent::FinishReason("STOP".into())));
    }

    #[test]
    fn max_output_tokens_zero_is_floored_to_one() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let built = build(
            &provider,
            "gemini-2.5-flash",
            &messages,
            &[],
            "none",
            &[],
            0,
        );
        assert_eq!(
            built.body["request"]["generationConfig"]["maxOutputTokens"],
            1
        );
    }

    #[test]
    fn max_output_tokens_positive_preserved() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let built = build(
            &provider,
            "gemini-2.5-flash",
            &messages,
            &[],
            "none",
            &[],
            8192,
        );
        assert_eq!(
            built.body["request"]["generationConfig"]["maxOutputTokens"],
            8192
        );
    }

    #[test]
    fn project_header_wins_over_freemium_default() {
        let provider = provider_with(vec![("X-Goog-User-Project".into(), "user-gcp-proj".into())]);
        let messages = vec![user("hi")];
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &[],
            "low",
            &[],
            1024,
        );
        assert_eq!(built.body["project"], "user-gcp-proj");
        assert!(
            built.notices.iter().all(|n| !n.contains("freemium")),
            "should not warn about freemium when header set: {:?}",
            built.notices
        );
    }

    #[test]
    fn freemium_project_fallback_emits_notice() {
        let provider = provider_with(Vec::new());
        let messages = vec![user("hi")];
        // resolve_project also reads process env; accept freemium default OR
        // whatever CODE_ASSIST_PROJECT/GOOGLE_CLOUD_PROJECT is set in the env.
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &[],
            "low",
            &[],
            1024,
        );
        assert!(
            built.body["project"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "project must be non-empty"
        );
        if built.body["project"] == DEFAULT_CODE_ASSIST_FREEMIUM_PROJECT {
            assert!(
                built.notices.iter().any(|n| n.contains("freemium default")),
                "expected freemium notice, got {:?}",
                built.notices
            );
        }
    }

    #[test]
    fn empty_contents_errors_instead_of_sending() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        // System-only transcript → no contents.
        let messages = vec![Message::system("you are helpful")];
        let err = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-3-flash",
                messages: &messages,
                tools: &[],
                reasoning_effort: "low",
                thinking_levels: &[],
                max_tokens: 1024,
            })
            .expect_err("system-only must fail");
        assert!(
            err.contains("no user/model contents"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn system_instruction_is_top_level_on_request_not_in_contents() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![Message::system("be terse"), user("hi")];
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &[],
            "low",
            &[],
            1024,
        );
        assert_eq!(
            built.body["request"]["systemInstruction"]["parts"][0]["text"],
            "be terse"
        );
        let contents = built.body["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn tool_result_uses_user_role_not_function() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![
            user("read it"),
            Message::Assistant {
                name: None,
                content: None,
                thinking: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    typ: "function".into(),
                    function: FunctionCall {
                        name: "read_file".into(),
                        arguments: r#"{"path":"a"}"#.into(),
                    },
                }]),
            },
            Message::Tool {
                tool_call_id: "call_1".into(),
                name: Some("read_file".into()),
                content: "file body".into(),
            },
        ];
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &[],
            "low",
            &[],
            1024,
        );
        let contents = built.body["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][0].get("functionCall").is_some());
        assert_eq!(
            contents[2]["role"], "user",
            "functionResponse must be user role"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
    }

    #[test]
    fn gemini3_flash_thinking_levels_include_minimal_medium() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let levels = vec![
            "minimal".into(),
            "low".into(),
            "medium".into(),
            "high".into(),
        ];
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &[],
            "medium",
            &levels,
            1024,
        );
        assert_eq!(
            built.body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
    }

    #[test]
    fn gemini3_pro_aligns_thinking_level_with_model_suffix() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        // Medium effort on bare pro → model -high + thinkingLevel high
        // (pro only accepts low|high; medium must map to high, not low).
        let built = build(
            &provider,
            "gemini-3-pro",
            &messages,
            &[],
            "medium",
            &[],
            1024,
        );
        // After resolve_model_id → gemini-3-pro-high, thinkingLevel high is valid.
        assert_eq!(built.body["model"], "gemini-3-pro-high");
        assert_eq!(
            built.body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );

        // Explicit low effort → model -low + thinkingLevel low.
        let built_low = build(&provider, "gemini-3-pro", &messages, &[], "low", &[], 1024);
        assert_eq!(built_low.body["model"], "gemini-3-pro-low");
        assert_eq!(
            built_low.body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
    }

    #[test]
    fn gemini20_does_not_send_thinking_config() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let built = build(
            &provider,
            "gemini-2.0-flash",
            &messages,
            &[],
            "high",
            &[],
            1024,
        );
        assert!(
            built.body["request"]["generationConfig"]
                .get("thinkingConfig")
                .is_none(),
            "gemini-2.0 must not receive thinkingConfig, got {:?}",
            built.body["request"]["generationConfig"]
        );
    }

    #[test]
    fn gemini25_uses_thinking_budget_not_level() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let built = build(
            &provider,
            "gemini-2.5-pro",
            &messages,
            &[],
            "high",
            &[],
            1024,
        );
        let cfg = &built.body["request"]["generationConfig"]["thinkingConfig"];
        assert_eq!(cfg["thinkingBudget"], 32768);
        assert!(cfg.get("thinkingLevel").is_none());
    }

    #[test]
    fn tools_skip_empty_names_and_wrap_function_declarations() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let tools = vec![
            json!({"type":"function","function":{"name":"","description":"bad"}}),
            json!({"type":"function","function":{"name":"read_file","description":"Read a file","parameters":{"type":"object"}}}),
        ];
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &tools,
            "low",
            &[],
            1024,
        );
        let decls = &built.body["request"]["tools"][0]["functionDeclarations"];
        assert_eq!(decls.as_array().map(|a| a.len()), Some(1));
        assert_eq!(decls[0]["name"], "read_file");
    }

    #[test]
    fn token_count_rejects_negatives() {
        assert_eq!(token_count(&json!(-1)), None);
        assert_eq!(token_count(&json!(-1.5)), None);
        assert_eq!(token_count(&json!(3)), Some(3));
        assert_eq!(token_count(&json!(4.0)), Some(4));
        assert_eq!(token_count(&json!("5")), Some(5));
    }

    #[test]
    fn url_uses_stream_generate_content_sse() {
        let provider = provider_with(vec![("x-goog-user-project".into(), "my-project".into())]);
        let messages = vec![user("hi")];
        let built = build(
            &provider,
            "gemini-3-flash",
            &messages,
            &[],
            "low",
            &[],
            1024,
        );
        assert_eq!(
            built.url,
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
    }
}

#[cfg(test)]
mod wire_shape_contract {
    //! Wire-shape lock-in tests.
    //!
    //! These guard the exact URL + body shape + identity headers the OAuth
    //! plugins (antigravity, gemini-cli) and downstream clients depend on.
    //! If any of these break, both the harness and the real Antigravity /
    //! Gemini CLI web clients will silently fail with HTTP 403. Reviewed
    //! against the live Google Code Assist gateway.
    use super::*;
    use crate::config::{ProviderKind, ResolvedProvider};

    fn project_provider(base_url: &str, project: &str) -> ResolvedProvider {
        ResolvedProvider {
            name: "code-assist".into(),
            kind: ProviderKind::OpenAI,
            base_url: base_url.into(),
            api_key: Some("ya29.fake".into()),
            headers: vec![
                ("x-goog-user-project".into(), project.into()),
                ("x-code-assist-project".into(), project.into()),
                ("cloudaicompanion-project".into(), project.into()),
            ],
            oauth: true,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }

    #[test]
    fn chat_targets_daily_cloudcode_pa_for_antigravity() {
        // Antigravity OAuth plugin's base_url; verified live: the daily
        // host serves chat for Antigravity IDE traffic. The prod host
        // rejects Antigravity-issued tokens with HTTP 403 on free-tier.
        let provider = project_provider(
            "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal",
            "synthetic-expanse-sxhhm",
        );
        let built = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-3.1-pro-high",
                messages: &[Message::user("hi")],
                tools: &[],
                reasoning_effort: "high",
                thinking_levels: &["high".into()],
                max_tokens: 64,
            })
            .expect("build_request");
        assert_eq!(
            built.url,
            "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn chat_targets_prod_cloudcode_pa_for_gemini_cli() {
        // gemini-cli OAuth plugin's base_url. Verified live: the prod host
        // serves chat for gemini-cli clients; daily is rejected with 403.
        let provider = project_provider(
            "https://cloudcode-pa.googleapis.com/v1internal",
            "synthetic-expanse-sxhhm",
        );
        let built = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-2.5-flash",
                messages: &[Message::user("hi")],
                tools: &[],
                reasoning_effort: "low",
                thinking_levels: &[],
                max_tokens: 64,
            })
            .expect("build_request");
        assert_eq!(
            built.url,
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn body_uses_antigravity_user_agent_and_body_project() {
        // The body shape is the Google GenAI Cloud Code Assist envelope.
        // The Antigravity IDE binary sends body.userAgent="antigravity" +
        // body.project=<cloudaicompanionProject>. Verified live; the project
        // field MUST come from body (NOT x-goog-user-project header, which
        // trips the consumer API gate and returns SERVICE_DISABLED).
        let provider = project_provider(
            "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal",
            "synthetic-expanse-sxhhm",
        );
        let built = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-3.1-pro-high",
                messages: &[Message::user("hi")],
                tools: &[],
                reasoning_effort: "high",
                thinking_levels: &["high".into()],
                max_tokens: 64,
            })
            .expect("build_request");
        assert_eq!(built.body["model"], "gemini-3.1-pro-high");
        assert_eq!(built.body["project"], "synthetic-expanse-sxhhm");
        assert_eq!(built.body["userAgent"], "antigravity");
        assert!(built.body["request"]["contents"].is_array());
        assert!(built.body["request"]["generationConfig"]["maxOutputTokens"].is_number());
    }

    #[test]
    fn resolve_project_picks_first_matching_header_in_iteration_order() {
        // resolve_project reads the first matching header from the headers
        // vec, matching any of the three names case-insensitively. Plugin
        // authors must therefore inject ONLY x-code-assist-project — the
        // other two names trigger Google's consumer API gate on the chat
        // endpoint (HTTP 403 SERVICE_DISABLED). Verified live.
        // Test 1: with x-goog-user-project first, it wins.
        let provider = ResolvedProvider {
            name: "code-assist".into(),
            kind: ProviderKind::OpenAI,
            base_url: "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal".into(),
            api_key: Some("ya29.fake".into()),
            headers: vec![("x-goog-user-project".into(), "from-x-goog".into())],
            oauth: true,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let built = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-3.1-pro-high",
                messages: &[Message::user("hi")],
                tools: &[],
                reasoning_effort: "high",
                thinking_levels: &["high".into()],
                max_tokens: 64,
            })
            .expect("build_request");
        assert_eq!(built.body["project"], "from-x-goog");
        // Test 2: with only x-code-assist-project, it wins.
        let provider = ResolvedProvider {
            headers: vec![("x-code-assist-project".into(), "from-x-code-assist".into())],
            ..provider.clone()
        };
        let built = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-3.1-pro-high",
                messages: &[Message::user("hi")],
                tools: &[],
                reasoning_effort: "high",
                thinking_levels: &["high".into()],
                max_tokens: 64,
            })
            .expect("build_request");
        assert_eq!(built.body["project"], "from-x-code-assist");
    }

    fn freemium_fallback_emitted_when_no_project_header_present() {
        // When the plugin doesn't inject any project header, the adapter
        // falls back to the freemium default `rising-fact-p41fc` and emits
        // a notice so the user can fix their config.
        let provider = ResolvedProvider {
            name: "code-assist".into(),
            kind: ProviderKind::OpenAI,
            base_url: "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal".into(),
            api_key: Some("ya29.fake".into()),
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let built = GoogleCodeAssistAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "gemini-3-flash",
                messages: &[Message::user("hi")],
                tools: &[],
                reasoning_effort: "low",
                thinking_levels: &[],
                max_tokens: 64,
            })
            .expect("build_request");
        assert_eq!(
            built
                .notices
                .iter()
                .filter(|n| n.contains("rising-fact-p41fc"))
                .count(),
            1,
            "expected exactly one notice mentioning the freemium default project"
        );
    }
}
