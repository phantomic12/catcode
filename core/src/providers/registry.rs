use super::adapter::{ProviderAdapter, ProviderProtocol};
use super::anthropic_compatible::AnthropicCompatibleAdapter;
use super::codex_responses::CodexResponsesAdapter;
use super::google_code_assist::GoogleCodeAssistAdapter;
use super::openai_compatible::OpenAiCompatibleAdapter;
use crate::config::{ProviderKind, ResolvedProvider};

/// Curated wire dialects accepted by provider configuration.  This is
/// intentionally closed: selecting an arbitrary Rust/plugin adapter is not a
/// supported configuration surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderDialect {
    OpenAiChat,
    AnthropicMessages,
    CodexResponses,
    GoogleCodeAssist,
}

impl ProviderDialect {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai_chat" | "openai-chat" => Some(Self::OpenAiChat),
            "anthropic" | "anthropic_messages" | "anthropic-messages" => {
                Some(Self::AnthropicMessages)
            }
            "codex" | "codex_responses" | "codex-responses" => Some(Self::CodexResponses),
            "google" | "google_code_assist" | "google-code-assist" => Some(Self::GoogleCodeAssist),
            _ => None,
        }
    }
    pub fn protocol(self) -> ProviderProtocol {
        match self {
            Self::OpenAiChat => ProviderProtocol::OpenAiChat,
            Self::AnthropicMessages => ProviderProtocol::AnthropicMessages,
            Self::CodexResponses => ProviderProtocol::CodexResponses,
            Self::GoogleCodeAssist => ProviderProtocol::GoogleCodeAssist,
        }
    }
}

/// Select an adapter from the closed declarative dialect catalog. Unknown
/// dialects are rejected by [`ProviderDialect::parse`] before this function is
/// called; there is deliberately no user-code fallback.
pub fn adapter_for_dialect(dialect: ProviderDialect) -> &'static dyn ProviderAdapter {
    match dialect {
        ProviderDialect::OpenAiChat => &OPENAI,
        ProviderDialect::AnthropicMessages => &ANTHROPIC,
        ProviderDialect::CodexResponses => &CODEX,
        ProviderDialect::GoogleCodeAssist => &GOOGLE,
    }
}
static OPENAI: OpenAiCompatibleAdapter = OpenAiCompatibleAdapter;
static ANTHROPIC: AnthropicCompatibleAdapter = AnthropicCompatibleAdapter;
static CODEX: CodexResponsesAdapter = CodexResponsesAdapter;
static GOOGLE: GoogleCodeAssistAdapter = GoogleCodeAssistAdapter;

/// Resolve the wire adapter once at the provider boundary. Endpoint-specific
/// protocols that are not OpenAI chat-completions are named explicitly so the
/// turn dispatcher does not spread URL heuristics through unrelated code.
pub fn adapter_for(provider: &ResolvedProvider) -> &'static dyn ProviderAdapter {
    if provider.kind == ProviderKind::Anthropic {
        &ANTHROPIC
    } else if crate::provider::is_code_assist_endpoint(&provider.base_url) {
        &GOOGLE
    } else if crate::provider::is_codex_endpoint(&provider.base_url) {
        &CODEX
    } else {
        &OPENAI
    }
}

pub fn protocol_for(provider: &ResolvedProvider) -> ProviderProtocol {
    if provider.kind == ProviderKind::Anthropic {
        ProviderProtocol::AnthropicMessages
    } else {
        adapter_for(provider).protocol()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(kind: ProviderKind, base_url: &str) -> ResolvedProvider {
        ResolvedProvider {
            name: "test".into(),
            kind,
            base_url: base_url.into(),
            api_key: None,
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }

    #[test]
    fn resolves_protocols_without_turn_loop_branching() {
        assert_eq!(
            protocol_for(&provider(
                ProviderKind::Anthropic,
                "https://api.anthropic.com/v1"
            )),
            ProviderProtocol::AnthropicMessages
        );
        assert_eq!(
            protocol_for(&provider(
                ProviderKind::OpenAI,
                "https://chatgpt.com/backend-api/codex"
            )),
            ProviderProtocol::CodexResponses
        );
        assert_eq!(
            protocol_for(&provider(ProviderKind::OpenAI, "https://example.com/v1")),
            ProviderProtocol::OpenAiChat
        );
    }

    #[test]
    fn parses_only_curated_dialects() {
        assert_eq!(
            ProviderDialect::parse("codex-responses"),
            Some(ProviderDialect::CodexResponses)
        );
        assert_eq!(
            ProviderDialect::parse("google_code_assist"),
            Some(ProviderDialect::GoogleCodeAssist)
        );
        assert_eq!(ProviderDialect::parse("arbitrary-rust"), None);
        assert_eq!(
            adapter_for_dialect(ProviderDialect::CodexResponses).protocol(),
            ProviderProtocol::CodexResponses
        );
    }

    #[test]
    fn code_assist_url_selects_google_adapter_not_openai() {
        // Mis-routing Antigravity / Code Assist through OpenAI chat would 404
        // every turn. Kind stays OpenAI (OAuth Gemini is OpenAI-shaped in
        // config) — the URL heuristic must win for this endpoint family.
        assert_eq!(
            protocol_for(&provider(
                ProviderKind::OpenAI,
                "https://daily-cloudcode-pa.sandbox.googleapis.com"
            )),
            ProviderProtocol::GoogleCodeAssist
        );
    }

    #[test]
    fn anthropic_kind_wins_over_codex_url_heuristic() {
        // Kind is authoritative for Anthropic wire; a mis-set base_url must
        // not flip Messages → Codex Responses.
        assert_eq!(
            protocol_for(&provider(
                ProviderKind::Anthropic,
                "https://chatgpt.com/backend-api/codex"
            )),
            ProviderProtocol::AnthropicMessages
        );
    }
}
