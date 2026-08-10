use crate::config::ResolvedProvider;
use crate::message::Message;
use crate::protocol::ModelInfo;
use crate::providers::capabilities::ProviderCapabilities;
use crate::providers::streaming::NormalizedStreamEvent;
use crate::providers::usage::ProviderUsage;
use serde::Serialize;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub struct ProviderContext<'a> {
    pub client: &'a reqwest::Client,
    pub provider: &'a ResolvedProvider,
}

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    OpenAiChat,
    AnthropicMessages,
    CodexResponses,
    GoogleCodeAssist,
}

pub struct ProviderRequest<'a> {
    pub provider: &'a ResolvedProvider,
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [Value],
    pub reasoning_effort: &'a str,
    pub thinking_levels: &'a [String],
    pub max_tokens: u32,
}

#[derive(Clone, Debug)]
pub struct BuiltProviderRequest {
    pub url: String,
    pub body: Value,
    /// Safe, user-visible compatibility notices. Never contains credentials or
    /// complete prompts.
    pub notices: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Authentication,
    ContextLength,
    RateLimit,
    /// Billing / prepaid balance / credit exhaustion — permanent until the user tops up.
    Balance,
    Server,
    Transport,
    MalformedResponse,
    Fatal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub retryable: bool,
    pub status: Option<u16>,
    pub message: String,
}

/// Narrow, synchronous provider-wire contract. Network I/O, cancellation, and
/// retry ownership stay in the transport layer; provider-specific request and
/// response semantics live behind implementations of this trait.
pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn protocol(&self) -> ProviderProtocol;
    fn capabilities(&self) -> ProviderCapabilities;
    fn build_request(&self, input: &ProviderRequest<'_>) -> Result<BuiltProviderRequest, String>;
    fn decode_stream_event(&self, value: &Value) -> Vec<NormalizedStreamEvent>;
    fn normalize_error(&self, status: Option<u16>, body: &str) -> ProviderError;
    fn discover_models<'a>(
        &'a self,
        context: ProviderContext<'a>,
    ) -> ProviderFuture<'a, Vec<ModelInfo>> {
        Box::pin(crate::providers::discovery::discover_models(
            context.client,
            context.provider,
        ))
    }
    fn usage_status<'a>(
        &'a self,
        context: ProviderContext<'a>,
    ) -> ProviderFuture<'a, ProviderUsage> {
        Box::pin(crate::providers::usage::fetch_provider_usage(
            context.client,
            context.provider,
        ))
    }
}

pub(crate) fn normalize_http_error(status: Option<u16>, body: &str) -> ProviderError {
    let lower = body.to_ascii_lowercase();
    // Body markers for balance / rate-limit take priority over status so a 5xx
    // (or other) response that is clearly "out of credits" still fails fast.
    let kind = if is_balance_or_billing_message(&lower) {
        ProviderErrorKind::Balance
    } else if is_rate_limit_message(&lower) || matches!(status, Some(429)) {
        ProviderErrorKind::RateLimit
    } else {
        match status {
            Some(401 | 403) => ProviderErrorKind::Authentication,
            // 402 Payment Required is billing/balance, not a transient blip.
            Some(402) => ProviderErrorKind::Balance,
            // 408 Request Timeout is temporary (proxy/gateway stall) — treat as
            // retryable like a server blip, not a fatal client error.
            Some(408) => ProviderErrorKind::Server,
            Some(code) if code >= 500 => ProviderErrorKind::Server,
            _ if lower.contains("context_length")
                || lower.contains("context length")
                || lower.contains("maximum context")
                || lower.contains("prompt is too long")
                || lower.contains("input is too long") =>
            {
                ProviderErrorKind::ContextLength
            }
            None => ProviderErrorKind::Transport,
            // Unknown non-permanent status → treat as transient server blip so the
            // transport denylist (rate-limit / balance only for fail-fast) can retry.
            Some(code) if !is_permanent_client_status(code) => ProviderErrorKind::Server,
            _ => ProviderErrorKind::Fatal,
        }
    };
    // Policy: retry provider errors by default. Fail fast only on rate limits,
    // balance/billing, and permanent request/account failures (auth, context,
    // malformed, validation). Rewriting the same bad request cannot help those.
    let retryable = !matches!(
        kind,
        ProviderErrorKind::Authentication
            | ProviderErrorKind::ContextLength
            | ProviderErrorKind::RateLimit
            | ProviderErrorKind::Balance
            | ProviderErrorKind::MalformedResponse
            | ProviderErrorKind::Fatal
    );
    ProviderError {
        kind,
        retryable,
        status,
        message: sanitize_error_message(body),
    }
}

/// Client statuses that will not succeed on an identical retry.
/// 408 is intentionally excluded (proxy stall → retry). 429 rate-limit and
/// 402 payment/balance are fail-fast by policy.
pub(crate) fn is_permanent_client_status(code: u16) -> bool {
    matches!(
        code,
        400 | 401 | 402 | 403 | 404 | 409 | 413 | 415 | 422 | 429 | 431
    )
}

/// Prepaid balance / credit / billing exhaustion markers across providers.
pub(crate) fn is_balance_or_billing_message(lower: &str) -> bool {
    (lower.contains("insufficient")
        && (lower.contains("credit")
            || lower.contains("balance")
            || lower.contains("quota")
            || lower.contains("fund")))
        || lower.contains("insufficient_quota")
        || lower.contains("insufficient_balance")
        || lower.contains("insufficient credits")
        || lower.contains("insufficient balance")
        || lower.contains("out of credits")
        || lower.contains("out of credit")
        || lower.contains("no credits")
        || lower.contains("credit balance is too low")
        || lower.contains("credit balance too low")
        || lower.contains("credit balance")
        || (lower.contains("billing")
            && (lower.contains("hard limit")
                || lower.contains("exceeded")
                || lower.contains("required")
                || lower.contains("issue")))
        || lower.contains("payment required")
        || lower.contains("payment_required")
        || (lower.contains("prepaid") && lower.contains("exhaust"))
        || lower.contains("top up")
        || lower.contains("top-up")
        || lower.contains("add credits")
        || lower.contains("purchase credits")
        || lower.contains("plan quota exhausted")
        || (lower.contains("quota exceeded") && !lower.contains("rate"))
        || lower.contains("exceeded your current quota")
}

/// Rate-limit wording (with or without HTTP 429).
pub(crate) fn is_rate_limit_message(lower: &str) -> bool {
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("ratelimit")
        || lower.contains("too many requests")
        || lower.contains("tokens per minute")
        || lower.contains("requests per minute")
        || (lower.contains("tpm") && lower.contains("limit"))
        || (lower.contains("rpm") && lower.contains("limit"))
}

/// Whether an HTTP failure should be retried at the transport layer.
/// Default is retry; only permanent account/request failures are excluded.
/// Rate limits (429) and balance/billing (402 + body markers) fail fast.
pub(crate) fn is_retryable_http_error(status: u16, body: &str) -> bool {
    if (200..300).contains(&status) {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    // Body markers win even on 5xx — a gateway that wraps billing as 503
    // still must not burn retries on an empty wallet.
    if is_balance_or_billing_message(&lower) || is_rate_limit_message(&lower) {
        return false;
    }
    // 408 + 5xx are always worth another attempt (after body denylist above).
    if status == 408 || status >= 500 {
        return true;
    }
    if is_permanent_client_status(status) {
        return false;
    }
    // Unknown / uncommon non-success statuses: retry.
    true
}

/// Message-level fail-fast for mid-stream / post-accept errors: rate limits and
/// empty-wallet billing. Everything else may still be retried by the stream loop.
pub(crate) fn is_non_retryable_provider_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_rate_limit_message(&lower) || is_balance_or_billing_message(&lower)
}

pub(crate) fn malformed_response(preview: &str) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::MalformedResponse,
        retryable: false,
        status: None,
        message: format!(
            "malformed provider stream event: {}",
            sanitize_error_message(preview)
        ),
    }
}

fn sanitize_error_message(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "provider request failed".into();
    }
    let mut out = trimmed.chars().take(2_000).collect::<String>();
    let lower = out.to_ascii_lowercase();
    // Whole-body redaction when the payload looks credential-bearing. Prefer
    // over-redacting: these strings often appear next to live secrets in 401
    // bodies, proxy dumps, and misconfigured gateway responses.
    for marker in [
        "authorization",
        "bearer ",
        "api_key",
        "api-key",
        "api key",
        "x-api-key",
        "access_token",
        "access-token",
        "refresh_token",
        "refresh-token",
        "client_secret",
        "client-secret",
        "id_token",
        "oauth_token",
        "sk-ant-",
        "sk-or-",
        "sk-proj-",
        "sk-",
    ] {
        if lower.contains(marker) {
            return "provider returned a redacted authentication/error response".into();
        }
    }
    if trimmed.chars().count() > 2_000 {
        out.push_str("…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_normalization_classifies_and_redacts() {
        let auth = normalize_http_error(Some(401), r#"{"api_key":"secret"}"#);
        assert_eq!(auth.kind, ProviderErrorKind::Authentication);
        assert!(!auth.retryable);
        assert!(!auth.message.contains("secret"));

        // Rate limits fail fast — do not burn retries on long cooldowns.
        let rate = normalize_http_error(Some(429), "busy");
        assert_eq!(rate.kind, ProviderErrorKind::RateLimit);
        assert!(!rate.retryable);

        let context = normalize_http_error(Some(400), "maximum context length exceeded");
        assert_eq!(context.kind, ProviderErrorKind::ContextLength);
        assert!(!context.retryable);
    }

    #[test]
    fn balance_and_billing_errors_are_not_retryable() {
        for (status, body) in [
            (Some(402), "payment required"),
            (Some(403), "insufficient credits — top up your balance"),
            (Some(400), "exceeded your current quota"),
            (None, "insufficient_quota: you have no credits left"),
            (Some(500), "credit balance is too low"), // body wins even on 5xx
        ] {
            let err = normalize_http_error(status, body);
            assert_eq!(err.kind, ProviderErrorKind::Balance, "{body}");
            assert!(!err.retryable, "{body}");
        }
        assert!(!is_retryable_http_error(402, "payment required"));
        assert!(!is_retryable_http_error(429, "rate limit exceeded"));
        assert!(is_retryable_http_error(503, "temporarily unavailable"));
        assert!(is_retryable_http_error(408, "gateway timed out"));
        assert!(!is_retryable_http_error(401, "invalid api key"));
        // Body denylist wins over 5xx status.
        assert!(!is_retryable_http_error(503, "credit balance is too low"));
        assert!(!is_retryable_http_error(500, "rate limit exceeded"));
    }

    #[test]
    fn anthropic_prompt_too_long_is_context_length() {
        for body in ["prompt is too long for this model", "input is too long"] {
            let err = normalize_http_error(Some(400), body);
            assert_eq!(err.kind, ProviderErrorKind::ContextLength, "{body}");
            assert!(!err.retryable, "{body}");
        }
    }

    #[test]
    fn request_timeout_408_is_retryable_server() {
        let err = normalize_http_error(Some(408), "gateway timed out waiting for upstream");
        assert_eq!(err.kind, ProviderErrorKind::Server);
        assert!(err.retryable);
        assert_eq!(err.status, Some(408));
        assert!(err.message.contains("gateway timed out"));
    }

    #[test]
    fn empty_error_body_gets_generic_message() {
        let err = normalize_http_error(Some(500), "   ");
        assert_eq!(err.kind, ProviderErrorKind::Server);
        assert!(err.retryable);
        assert_eq!(err.message, "provider request failed");
    }

    #[test]
    fn sanitize_redacts_common_key_prefixes_and_header_names() {
        for body in [
            "invalid key sk-ant-api03-ABCDEF",
            "OpenRouter rejected sk-or-v1-deadbeef",
            "x-api-key header missing",
            "client_secret leaked in proxy log",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc",
        ] {
            let msg = sanitize_error_message(body);
            assert_eq!(
                msg, "provider returned a redacted authentication/error response",
                "body should redact: {body}"
            );
            assert!(!msg.contains("sk-"), "leaked material in: {msg}");
            assert!(!msg.contains("eyJ"), "leaked jwt in: {msg}");
        }
        // Ordinary provider errors must stay readable.
        let plain = sanitize_error_message("model not found: foo-bar");
        assert_eq!(plain, "model not found: foo-bar");
    }
}
