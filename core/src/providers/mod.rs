pub(crate) mod adapter;
pub(crate) mod anthropic_compatible;
pub(crate) mod capabilities;
pub(crate) mod codex_responses;
pub(crate) mod discovery;
pub(crate) mod google_code_assist;
pub(crate) mod openai_compatible;
pub(crate) mod registry;
pub(crate) mod sse;
pub(crate) mod streaming;
pub(crate) mod usage;

#[cfg(test)]
mod fixture_tests {
    use super::adapter::{
        normalize_http_error, ProviderAdapter, ProviderErrorKind, ProviderRequest,
    };
    use super::anthropic_compatible::AnthropicCompatibleAdapter;
    use super::codex_responses::CodexResponsesAdapter;
    use super::google_code_assist::GoogleCodeAssistAdapter;
    use super::openai_compatible::OpenAiCompatibleAdapter;
    use super::sse::{SseDecoder, SseFrame};
    use super::streaming::NormalizedStreamEvent;
    use crate::config::{ProviderKind, ResolvedProvider};
    use crate::message::Message;
    use serde_json::json;
    use serde_json::Value;

    fn jsonl_events(adapter: &dyn ProviderAdapter, fixture: &str) -> Vec<NormalizedStreamEvent> {
        fixture
            .lines()
            .filter(|line| !line.trim().is_empty())
            .flat_map(|line| {
                let value: Value = serde_json::from_str(line).expect("valid provider fixture");
                adapter.decode_stream_event(&value)
            })
            .collect()
    }

    #[test]
    fn protocol_fixtures_cover_reasoning_tools_usage_and_finish() {
        let cases: [(&dyn ProviderAdapter, &str); 3] = [
            (
                &AnthropicCompatibleAdapter,
                include_str!("../../tests/fixtures/providers/anthropic_stream.jsonl"),
            ),
            (
                &CodexResponsesAdapter,
                include_str!("../../tests/fixtures/providers/codex_stream.jsonl"),
            ),
            (
                &GoogleCodeAssistAdapter,
                include_str!("../../tests/fixtures/providers/google_stream.jsonl"),
            ),
        ];
        for (adapter, fixture) in cases {
            let events = jsonl_events(adapter, fixture);
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, NormalizedStreamEvent::ReasoningDelta(_))),
                "{} reasoning",
                adapter.id()
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, NormalizedStreamEvent::TextDelta(_))),
                "{} text",
                adapter.id()
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, NormalizedStreamEvent::ToolCallStart(_))),
                "{} tool",
                adapter.id()
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, NormalizedStreamEvent::Usage { .. })),
                "{} usage",
                adapter.id()
            );
        }
    }

    #[test]
    fn fragmented_sse_is_transport_chunk_independent_and_reports_disconnect() {
        let fixture = include_bytes!("../../tests/fixtures/providers/fragmented.sse");
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        for chunk in fixture.chunks(3) {
            frames.extend(decoder.push(chunk));
        }
        frames.extend(decoder.finish());
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(frame, SseFrame::Json { .. }))
                .count(),
            2
        );
        assert!(frames.iter().any(|frame| matches!(frame, SseFrame::Done)));

        let mut disconnected = SseDecoder::default();
        assert!(disconnected.push(b"data: {\"choices\": [").is_empty());
        assert!(matches!(
            disconnected.finish().as_slice(),
            [SseFrame::Malformed { .. }]
        ));
    }

    #[test]
    fn error_fixture_classifies_retryability_context_and_secret_redaction() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/providers/provider_errors.json"
        ))
        .unwrap();
        let cases = [
            ("authentication", ProviderErrorKind::Authentication, false),
            ("rate_limit", ProviderErrorKind::RateLimit, false),
            ("server", ProviderErrorKind::Server, true),
            ("context", ProviderErrorKind::ContextLength, false),
        ];
        for (name, expected, retryable) in cases {
            let case = &fixture[name];
            let error = normalize_http_error(
                case["status"].as_u64().map(|v| v as u16),
                case["body"].as_str().unwrap(),
            );
            assert_eq!(error.kind, expected);
            assert_eq!(error.retryable, retryable);
            assert!(!error.message.contains("top-secret-token"));
        }
    }

    #[test]
    fn openai_request_preserves_inline_vision_payload() {
        let provider = ResolvedProvider {
            name: "fixture".into(),
            kind: ProviderKind::OpenAI,
            base_url: "https://example.test/v1".into(),
            api_key: None,
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let messages: Vec<Message> = serde_json::from_value(json!([{
            "role": "user",
            "content": [
                {"type":"text", "text":"inspect"},
                {"type":"image_url", "image_url":{"url":"data:image/png;base64,AA==", "detail":"low"}}
            ]
        }])).unwrap();
        let built = OpenAiCompatibleAdapter
            .build_request(&ProviderRequest {
                provider: &provider,
                model: "vision-model",
                messages: &messages,
                tools: &[],
                reasoning_effort: "none",
                thinking_levels: &[],
                max_tokens: 32,
            })
            .unwrap();
        assert_eq!(
            built.body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA=="
        );
    }
}
