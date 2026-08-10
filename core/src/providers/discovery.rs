use crate::config::{ProviderKind, ResolvedProvider};
use crate::protocol::ModelInfo;
use crate::provider::{
    ANTHROPIC_MODELS_PATH, ANTHROPIC_VERSION, CLAUDE_OAUTH_BETA, DEFAULT_THINKING_LEVELS,
    MODELS_INFO_PATH, OPENAI_MODELS_PATH,
};
use serde_json::{json, Value};
use std::time::Duration;

/// Discover models from an Anthropic-compatible endpoint (`GET /v1/models`).
/// Anthropic lists model ids but not capabilities, so each id is mapped through
/// a curated capability table; unknown ids get conservative defaults.
async fn discover_models_anthropic(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    cache_key: &str,
    force_live: bool,
) -> Vec<ModelInfo> {
    // OpenCode Go: the single /v1/models endpoint serves every model over both
    // wire protocols with no protocol field, so fetch it live and filter to
    // this provider's protocol (Anthropic /v1/messages here). See
    // opencode_go_discover_models for the family-prefix partition + caching.
    if is_opencode_go(&provider.base_url) {
        return opencode_go_discover_models(client, provider, cache_key, false, force_live).await;
    }
    if !force_live {
        if let Some(models) = read_models_cache(cache_key) {
            return models;
        }
    }
    let url = format!("{}{ANTHROPIC_MODELS_PATH}", provider.base_url);
    let mut req = client.get(&url).timeout(Duration::from_secs(8));
    if provider.oauth {
        if let Some(k) = provider.api_key.as_deref() {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        req = req.header("anthropic-beta", CLAUDE_OAUTH_BETA);
    } else if let Some(k) = provider.api_key.as_deref() {
        req = req.header("x-api-key", k);
    }
    req = req.header("anthropic-version", ANTHROPIC_VERSION);
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    let mut live = match req.send().await {
        Ok(r) if r.status().is_success() => {
            parse_anthropic_models(&r.json::<Value>().await.unwrap_or_else(|_| json!({})))
        }
        _ => read_models_cache_stale(cache_key).unwrap_or_else(anthropic_fallback_models),
    };
    // Enrich with models.dev caps for models the curated table left at
    // generic defaults (relevant for Anthropic-compatible gateways).
    if let Some(dev) = crate::models_dev::fetch_models_dev(client).await {
        crate::models_dev::enrich_models(&mut live, &dev, &provider.base_url);
    }
    write_models_cache(cache_key, &live);
    live
}

/// Parse Anthropic `GET /v1/models` -> `{data:[{id,display_name,...}]}` into
/// ModelInfo, applying curated per-id capabilities. Falls back to the static
/// list when the response has no models.
pub(crate) fn parse_anthropic_models(data: &Value) -> Vec<ModelInfo> {
    let Some(arr) = data.get("data").and_then(|d| d.as_array()) else {
        return anthropic_fallback_models();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.to_string();
            let name = m
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            Some(anthropic_model_caps(&id, &name))
        })
        .collect();
    if out.is_empty() {
        return anthropic_fallback_models();
    }
    // de-dup by id, preserve order
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

/// Curated capabilities for a Claude model id (context window, max output,
/// extended-thinking support, vision). Unknown ids get conservative defaults
/// (thinking off, vision on — Claude has had vision since 3.0).
#[allow(clippy::if_same_then_else)] // families share caps today but are kept
                                    // distinct for readability + future divergence as models gain new caps.
pub(crate) fn anthropic_model_caps(id: &str, name: &str) -> ModelInfo {
    let l = id.to_ascii_lowercase();
    let (ctx, max, thinking, vision) = if l.contains("opus-4") {
        (200_000, 32_000, true, true)
    } else if l.contains("sonnet-4") {
        (200_000, 16_000, true, true)
    } else if l.contains("haiku-4") {
        (200_000, 8_192, false, true)
    } else if l.contains("3-7-sonnet") || l.contains("3.7-sonnet") {
        (200_000, 8_192, true, true)
    } else if l.contains("3-5-sonnet") || l.contains("3.5-sonnet") {
        (200_000, 8_192, false, true)
    } else if l.contains("3-5-haiku") || l.contains("3.5-haiku") {
        (200_000, 8_192, false, true)
    } else if l.contains("3-opus") || l.contains("3.0-opus") {
        (200_000, 4_096, false, true)
    } else if l.contains("3-haiku") {
        (200_000, 4_096, false, true)
    } else {
        (200_000, 8_192, false, true)
    };
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        reasoning: thinking,
        context_window: ctx,
        max_tokens: max,
        thinking_levels: if thinking {
            DEFAULT_THINKING_LEVELS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        },
        vision,
        ..Default::default()
    }
}

/// Static Claude model list used when `/v1/models` is unreachable.
pub(crate) fn anthropic_fallback_models() -> Vec<ModelInfo> {
    let ids = [
        "claude-opus-4-1",
        "claude-sonnet-4-5",
        "claude-sonnet-4-0",
        "claude-haiku-4-5",
        "claude-3-7-sonnet-20250219",
        "claude-3-5-sonnet-20241022",
        "claude-3-5-haiku-20241022",
    ];
    ids.iter().map(|id| anthropic_model_caps(id, id)).collect()
}

pub(crate) fn fallback_models() -> Vec<ModelInfo> {
    // ponytail: GLM chat template maps any effort except 'high' to 'max', which
    // degenerates thinking output. Advertise only ["high"] so resolve_effort
    // clamps to it — replacing the old hardcoded model-name sniff.
    let std = || {
        DEFAULT_THINKING_LEVELS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    };
    vec![
        ModelInfo {
            id: "umans-coder".into(),
            name: "Umans Coder".into(),
            reasoning: true,
            context_window: 262144,
            max_tokens: 32768,
            thinking_levels: std(),
            vision: false,

            ..Default::default()
        },
        ModelInfo {
            id: "umans-kimi-k2.5".into(),
            name: "Umans Kimi K2.5".into(),
            reasoning: true,
            context_window: 262144,
            max_tokens: 32768,
            thinking_levels: std(),
            vision: false,

            ..Default::default()
        },
        ModelInfo {
            id: "umans-kimi-k2.6".into(),
            name: "Umans Kimi K2.6".into(),
            reasoning: true,
            context_window: 262144,
            max_tokens: 32768,
            thinking_levels: std(),
            vision: false,

            ..Default::default()
        },
        ModelInfo {
            id: "umans-glm-5.2".into(),
            name: "Umans GLM 5.2".into(),
            reasoning: true,
            context_window: 1_000_000,
            max_tokens: 131_072,
            thinking_levels: vec!["high".to_string(), "max".to_string()],
            vision: false,

            ..Default::default()
        },
        ModelInfo {
            id: "umans-minimax-m3".into(),
            name: "Umans MiniMax M3".into(),
            reasoning: true,
            context_window: 1_000_000,
            max_tokens: 128_000,
            thinking_levels: vec!["low".to_string(), "medium".to_string(), "high".to_string()],
            vision: true,

            ..Default::default()
        },
        ModelInfo {
            id: "umans-minimax-m2.5".into(),
            name: "Umans MiniMax M2.5".into(),
            reasoning: true,
            context_window: 204800,
            max_tokens: 8192,
            thinking_levels: std(),
            vision: false,

            ..Default::default()
        },
    ]
}

/// Discover models live from /models/info; cache to disk with an 8-hour TTL.
/// Falls back to the disk cache (even if stale) on HTTP error, then to the
/// hardcoded snapshot as a last resort.
/// Discover available models for the active provider. Branches on the
/// provider's wire protocol: OpenAI-compatible (`/models/info`) or Anthropic
/// (`/v1/models`). Results are cached to disk, keyed by base URL + kind so an
/// OpenAI and an Anthropic endpoint at the same host don't collide.
pub async fn discover_models(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
) -> Vec<ModelInfo> {
    let cache_key = provider_cache_key(provider);
    let mut models = match provider.kind {
        ProviderKind::OpenAI => discover_models_openai(client, provider, &cache_key, false).await,
        ProviderKind::Anthropic => {
            discover_models_anthropic(client, provider, &cache_key, false).await
        }
    };
    apply_context_window_override(provider, &mut models);
    apply_models_override(provider, &mut models);
    sanitize_models_caps(&mut models);
    models
}

/// Like [`discover_models`], but bypasses the fresh-cache (TTL) early return so a
/// live fetch is ALWAYS performed and the disk cache is rewritten with the
/// current model list. Used by the startup background refresh (see `main.rs`)
/// for the Umans provider so newly-added models appear without waiting out the
/// 8h TTL. Still falls back to the stale cache / curated snapshot on HTTP
/// failure, so an unreachable endpoint never degrades the caller.
pub async fn discover_models_force_refresh(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
) -> Vec<ModelInfo> {
    let cache_key = provider_cache_key(provider);
    let mut models = match provider.kind {
        ProviderKind::OpenAI => discover_models_openai(client, provider, &cache_key, true).await,
        ProviderKind::Anthropic => {
            discover_models_anthropic(client, provider, &cache_key, true).await
        }
    };
    apply_context_window_override(provider, &mut models);
    apply_models_override(provider, &mut models);
    sanitize_models_caps(&mut models);
    models
}

/// Apply a provider's optional `context_window` override to every discovered
/// model. Local OpenAI-compatible servers (e.g. LM Studio) return bare
/// `/v1/models` ids with no context field, so without this a gemma model would
/// fall to the 200k curated default and the harness would oversend past the
/// model's actual loaded context (causing context-overflow errors). An explicit
/// `context_window` on the provider config wins over discovered/curated caps.
pub(crate) fn apply_context_window_override(provider: &ResolvedProvider, models: &mut [ModelInfo]) {
    if let Some(ctx) = provider.context_window.filter(|&c| c > 0) {
        for m in models {
            m.context_window = ctx;
            // Keep max output below the (possibly reduced) context so there is
            // room for the prompt; mirrors apply_live_model_fields.
            if m.max_tokens == 0 || m.max_tokens >= ctx {
                m.max_tokens = xai_default_max_tokens(ctx);
            }
        }
    }
}

/// Apply explicit per-model metadata after discovery and registry enrichment.
pub(crate) fn apply_models_override(provider: &ResolvedProvider, models: &mut [ModelInfo]) {
    for ov in &provider.models_override {
        let Some(m) = models.iter_mut().find(|m| m.id == ov.id) else {
            continue;
        };
        if let Some(ctx) = ov.context_window.filter(|&c| c > 0) {
            m.context_window = ctx;
        }
        if let Some(max) = ov.max_tokens.filter(|&c| c > 0) {
            m.max_tokens = max;
        }
        sanitize_model_caps(m);
        if let Some(r) = ov.reasoning {
            m.reasoning = r;
        }
        if let Some(levels) = &ov.thinking_levels {
            m.thinking_levels = levels.clone();
            m.reasoning = !levels.is_empty();
        }
        if let Some(input) = &ov.input {
            m.input = input.clone();
            m.vision = input.iter().any(|v| v.eq_ignore_ascii_case("image"));
        }
        if let Some(output) = &ov.output {
            m.output = output.clone();
        }
        if let Some(tool_call) = ov.tool_call {
            m.tool_call = tool_call;
        }
        if let Some(structured_output) = ov.structured_output {
            m.structured_output = structured_output;
        }
    }
}

/// Repair impossible / unusable cap combinations on a single model:
/// - `context_window == 0` → generic 200k (zero context breaks compaction math)
/// - `max_tokens == 0` → sensible default under context (never stick live 0)
/// - `max_tokens >= context_window` → default under context (leave prompt room)
///
/// Call sites: live parse, disk-cache load, models.dev enrich, per-provider
/// overrides, and the final discover_models exit so no path can ship a 0-budget
/// or inverted pair into the picker / request builder.
pub(crate) fn sanitize_model_caps(info: &mut ModelInfo) {
    if info.context_window == 0 {
        info.context_window = 200_000;
    }
    if info.max_tokens == 0 || info.max_tokens >= info.context_window {
        info.max_tokens = xai_default_max_tokens(info.context_window);
    }
}

pub(crate) fn sanitize_models_caps(models: &mut [ModelInfo]) {
    for m in models.iter_mut() {
        sanitize_model_caps(m);
    }
}

/// Cache key: base URL (trailing slash normalized) + provider kind.
fn provider_cache_key(provider: &ResolvedProvider) -> String {
    format!(
        "{}|{}|{}",
        provider.base_url.trim_end_matches('/'),
        provider.kind.as_str(),
        provider
            .models_endpoint
            .as_deref()
            .unwrap_or(OPENAI_MODELS_PATH)
    )
}

async fn discover_models_openai(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    cache_key: &str,
    force_live: bool,
) -> Vec<ModelInfo> {
    // OpenCode Go: the single /v1/models endpoint serves every model over both
    // wire protocols with no protocol field, so fetch it live and filter to
    // this provider's protocol (OpenAI chat/completions here). See
    // opencode_go_discover_models for the family-prefix partition + caching.
    if is_opencode_go(&provider.base_url) {
        return opencode_go_discover_models(client, provider, cache_key, true, force_live).await;
    }
    // Kimi (Moonshot) Code: a curated capability table + live `/models` list
    // (the endpoint returns ids + `context_length` but not reasoning/vision
    // flags). See `kimi_discover_models`.
    if is_kimi(&provider.base_url) {
        return kimi_discover_models(client, provider, cache_key, force_live).await;
    }
    // DeepSeek's official `/models` endpoint is OpenAI-shaped but only
    // returns ids and ownership. Use the live catalog for automatic model
    // updates, then fill capabilities from the curated DeepSeek table and
    // models.dev instead of the generic OpenAI defaults.
    if is_deepseek(&provider.base_url) {
        return deepseek_discover_models(client, provider, cache_key, force_live).await;
    }
    // 1. Try disk cache (fresh: < 8 hours old). Skipped on a forced live refresh
    //    (the startup background check) so newly-added models are picked up every
    //    launch instead of waiting out the TTL window.
    if !force_live {
        if let Some(models) = read_models_cache(cache_key) {
            return models;
        }
    }

    // 2. Fetch live from the endpoint. Auth is optional here (Umans /models/info
    // is public; custom OpenAI-compatible endpoints may gate it). Send the key
    // only when one is configured so an unauthenticated default still works.
    //
    // `/models/info` is Umans-specific (rich capabilities). First-party and
    // other vanilla OpenAI-compatible endpoints don't serve it, so on a miss
    // we fall back to the standard OpenAI `/models` list and synthesize
    // ModelInfo with curated per-id capabilities.
    //
    // When a custom `models_endpoint` is configured, skip the Umans-specific
    // `/models/info` probe entirely and go straight to the custom endpoint
    // (step 2b) — a non-standard endpoint would 404 on `/models/info` anyway,
    // and this avoids a wasted round-trip.
    let mut live = Vec::new();
    if provider.models_endpoint.is_none() {
        let url = format!("{}{MODELS_INFO_PATH}", provider.base_url);
        let mut req = client.get(&url).timeout(Duration::from_secs(5));
        if let Some(k) = provider.api_key.as_deref() {
            req = req.bearer_auth(k);
        }
        live = match req.send().await {
            Ok(r) if r.status().is_success() => {
                parse_models_response(&match r.json::<Value>().await {
                    Ok(v) => v,
                    Err(_) => Value::Null,
                })
            }
            _ => Vec::new(),
        };
    }

    // 2b. /models/info miss (non-Umans endpoint) → standard OpenAI `/models`.
    // When a custom `models_endpoint` is configured, this is the primary path
    // (step 2 was skipped) — the configured endpoint is hit directly.
    if live.is_empty() {
        let url = if let Some(ep) = &provider.models_endpoint {
            format!("{}{ep}", provider.base_url)
        } else if is_codex_endpoint(&provider.base_url) {
            // The Codex `/models` endpoint REQUIRES `client_version` and filters
            // the catalog by each model's `minimal_client_version`: a value too
            // low (e.g. our own CARGO_PKG_VERSION "0.2.0") returns an EMPTY
            // list, so discovery falls back to a stale hardcoded list and the
            // user ends up sending a slug the backend rejects. The official
            // `codex` CLI sends its own version (>= the latest models' minimum);
            // dev builds send "0.0.0", which the backend special-cases to return
            // the FULL account catalog regardless of minimums. We are not the
            // codex CLI, so use the "0.0.0" dev sentinel — it reliably yields the
            // models this account can actually use (verified: returns 4 models
            // vs 0 for a low non-zero version).
            format!(
                "{}{OPENAI_MODELS_PATH}?client_version=0.0.0",
                provider.base_url
            )
        } else {
            format!("{}{OPENAI_MODELS_PATH}", provider.base_url)
        };
        let mut req = client.get(&url).timeout(Duration::from_secs(8));
        if let Some(k) = provider.api_key.as_deref() {
            req = req.bearer_auth(k);
        }
        for (k, v) in &provider.headers {
            req = req.header(k, v);
        }
        if let Ok(r) = req.send().await {
            if r.status().is_success() {
                if let Ok(v) = r.json::<Value>().await {
                    let listed = if is_codex_endpoint(&provider.base_url) {
                        parse_codex_models_response(&v)
                    } else if is_xai_endpoint(&provider.base_url) {
                        // xAI's `/models` includes context_length, image
                        // pricing, and non-chat media models. Parse with the
                        // xAI-aware path, then enrich vision/chat filter from
                        // `/language-models` when available.
                        let mut models = parse_xai_models_list(&v);
                        if let Some(lang) = fetch_xai_language_model_ids(client, provider).await {
                            apply_xai_language_models_enrichment(&mut models, &lang);
                        }
                        models
                    } else {
                        parse_openai_models_list(&v)
                    };
                    // Enrich with models.dev caps (context/output/reasoning/vision)
                    // for models the curated table left at generic defaults.
                    let mut listed = listed;
                    if let Some(dev) = crate::models_dev::fetch_models_dev(client).await {
                        crate::models_dev::enrich_models(&mut listed, &dev, &provider.base_url);
                    }
                    // Registry enrichment fills gaps (especially context/output
                    // limits), but the provider's live catalog is authoritative
                    // for fields it explicitly reports. Re-apply those fields
                    // after models.dev so Cursor reasoning levels/disable flags
                    // and vendor-published limits are not overwritten.
                    apply_live_model_list_fields(&v, &mut listed);
                    if !listed.is_empty() {
                        write_models_cache(cache_key, &listed);
                        return listed;
                    }
                }
            }
        }
    }

    if live.is_empty() {
        // Neither endpoint served a usable list — stale cache, else curated
        // fallbacks for *known* vendors only. Unknown custom endpoints must
        // return empty (not the Umans catalog) so the add-custom-provider
        // preview doesn't pretend a dead URL serves umans-coder/etc.
        return read_models_cache_stale(cache_key)
            .unwrap_or_else(|| openai_fallback_models(&provider.base_url));
    }

    // 3. Write fresh data to disk cache.
    write_models_cache(cache_key, &live);
    live
}

/// Model cache TTL in seconds (8 hours).
const MODELS_CACHE_TTL: u64 = 28800;

/// Cache schema version. Bumped when the parsed model shape OR the cache file
/// shape changes so a stale cache written by an older parser (e.g. one that
/// stored empty thinking_levels or wrong vision flags, or the old single-`key`
/// file shape) is treated as a miss and refreshed, instead of masking the fix
/// for up to the TTL window.
// v7: xAI models parse live `context_length` / vision from `/models` +
// `/language-models` (previously hardcoded wrong windows for Grok).
// v8: Antigravity Gemini 3 + Claude-via-Antigravity catalog.
// v10: DeepSeek/Kimi provider-specific discovery and capability metadata.
pub(crate) const MODELS_CACHE_VERSION: u64 = 10;

/// True when a parsed cache object matches the current schema version. Pure
/// (no disk) so the version gate can be unit-tested.
pub(crate) fn cache_version_ok(cache: &Value) -> bool {
    cache.get("version").and_then(|v| v.as_u64()) == Some(MODELS_CACHE_VERSION)
}

fn models_cache_path() -> Option<std::path::PathBuf> {
    let home = crate::config::home_dir()?;
    Some(home.join(".config/catalyst-code/models-cache.json"))
}

pub(crate) fn read_models_cache(cache_key: &str) -> Option<Vec<ModelInfo>> {
    let path = models_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: Value = serde_json::from_str(&content).ok()?;
    if !cache_version_ok(&cache) {
        return None;
    }
    // The cache holds a `key -> entry` map so multiple providers' caches coexist
    // (previously a single `key` field meant each provider's write clobbered the
    // file, so only the last writer ever hit on the next startup).
    let entry = cache.get("entries")?.get(cache_key)?;
    let updated = entry.get("updated_at")?.as_u64()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if now.saturating_sub(updated) > MODELS_CACHE_TTL {
        return None;
    }
    parse_cache_models(entry)
}

pub(crate) fn read_models_cache_stale(cache_key: &str) -> Option<Vec<ModelInfo>> {
    let path = models_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: Value = serde_json::from_str(&content).ok()?;
    if !cache_version_ok(&cache) {
        return None;
    }
    let entry = cache.get("entries")?.get(cache_key)?;
    parse_cache_models(entry)
}

pub(crate) fn write_models_cache(cache_key: &str, models: &[ModelInfo]) {
    let path = match models_cache_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let models_json: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "name": m.name,
                "reasoning": m.reasoning,
                "context_window": m.context_window,
                "max_tokens": m.max_tokens,
                "thinking_levels": m.thinking_levels,
                "vision": m.vision,
            })
        })
        .collect();
    // Load the existing entries map (if present and same schema) so this
    // provider's entry is MERGED in rather than clobbering the whole file —
    // multi-provider caches then all hit on the next startup instead of only
    // the last writer's. Written atomically (temp + fsync + rename) so a crash
    // mid-write can't truncate/corrupt the cache file.
    // Cross-process lock: the cache is a shared read-modify-write (we merge
    // this provider's entry into the existing entries map). Without a lock two
    // processes refreshing different providers concurrently would both read the
    // same base and the second rename would clobber the first's entry. Advisory
    // (flock); auto-releases on exit/crash so there are no stale locks.
    let _lock = match crate::fsutil::FileLock::acquire(&path.with_extension("lock")) {
        Ok(g) => g,
        Err(_) => return, // best-effort: never block the turn on a wedged lock
    };
    let mut entries: serde_json::Map<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .filter(cache_version_ok)
        .and_then(|c| c.get("entries").cloned())
        .and_then(|e| e.as_object().cloned())
        .unwrap_or_default();
    entries.insert(
        cache_key.to_string(),
        json!({ "updated_at": now, "models": models_json }),
    );
    let cache = json!({
        "version": MODELS_CACHE_VERSION,
        "entries": entries,
    });
    // Unique-temp atomic write (fsutil): two processes never share a temp file,
    // so a concurrent writer can't corrupt this one's write.
    let _ = crate::fsutil::atomic_write_str(
        &path,
        &serde_json::to_string(&cache).unwrap_or_else(|_| "{}".into()),
    );
}

fn parse_cache_models(cache: &Value) -> Option<Vec<ModelInfo>> {
    let arr = cache.get("models")?.as_array()?;
    let mut out = Vec::new();
    for m in arr {
        let id = m.get("id")?.as_str()?.to_string();
        let name = m.get("name")?.as_str()?.to_string();
        let context_window = m.get("context_window")?.as_u64()? as u32;
        let max_tokens = m.get("max_tokens")?.as_u64()? as u32;
        let vision = m.get("vision").and_then(|v| v.as_bool()).unwrap_or(false);
        let thinking_levels = m
            .get("thinking_levels")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut info = ModelInfo {
            id,
            name,
            reasoning: m.get("reasoning").and_then(|v| v.as_bool()).unwrap_or(true),
            context_window,
            max_tokens,
            thinking_levels,
            vision,

            ..Default::default()
        };
        // Disk cache can still hold pre-fix max_tokens:0 / inverted pairs
        // written by older builds; repair on read so the TTL doesn't re-ship
        // a broken budget for up to 8h.
        sanitize_model_caps(&mut info);
        out.push(info);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Parse the live /models/info response into ModelInfo vec.
pub(crate) fn parse_models_response(data: &Value) -> Vec<ModelInfo> {
    let mut out = Vec::new();
    if let Some(obj) = data.as_object() {
        for (id, info) in obj {
            let caps = info.get("capabilities");
            // Treat 0 as missing: some gateways advertise context_window/max
            // as 0 rather than omitting the field; sticking 0 breaks the
            // OpenAI wire path (omit vs send) and Anthropic (required field).
            let cw = caps
                .and_then(|c| c.get("context_window"))
                .and_then(|v| v.as_u64())
                .filter(|&c| c > 0)
                .unwrap_or(200_000) as u32;
            let mt = caps
                .and_then(|c| c.get("recommended_max_tokens"))
                .and_then(|v| v.as_u64())
                .filter(|&c| c > 0)
                .map(|c| c.min(u32::MAX as u64) as u32)
                .unwrap_or_else(|| xai_default_max_tokens(cw));
            // Vision comes from capabilities.supports_vision, which the endpoint
            // encodes as true / false / "via-handoff". Only boolean true counts
            // as native client-side vision; "via-handoff" (GLM 5.2, whose vision
            // only works on /v1/messages) falls through to false so the
            // vision-handoff plugin routes image turns to a natively-capable model.
            let vision = caps
                .and_then(|c| c.get("supports_vision"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let name = info
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            // The live /models/info endpoint nests reasoning config under
            // capabilities.reasoning: { supported, can_disable, levels,
            // default_level }. Read levels from there so each model advertises
            // the efforts it actually accepts (e.g. GLM: none/high/max, flash:
            // none/low/medium/high, kimi: []). Flat capability fields
            // (thinking_levels / reasoning_levels / reasoning_efforts) are kept
            // as a fallback for other OpenAI-compatible endpoints.
            let reasoning_caps = caps.and_then(|c| c.get("reasoning"));
            let reasoning_supported = reasoning_caps
                .and_then(|r| r.get("supported"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let thinking_levels = reasoning_caps
                .and_then(|r| r.get("levels"))
                .or_else(|| {
                    caps.and_then(|c| {
                        c.get("thinking_levels")
                            .or_else(|| c.get("reasoning_levels"))
                            .or_else(|| c.get("reasoning_efforts"))
                    })
                })
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut info = ModelInfo {
                id: id.clone(),
                name,
                reasoning: reasoning_supported,
                context_window: cw,
                max_tokens: mt,
                thinking_levels,
                vision,

                ..Default::default()
            };
            sanitize_model_caps(&mut info);
            out.push(info);
        }
    }
    if out.is_empty() {
        Vec::new()
    } else {
        out
    }
}

/// Parse the standard OpenAI `GET /models` list (`{data:[{id,...}]}`) into
/// ModelInfo, applying curated per-id capabilities for known OpenAI and Gemini
/// model families. Most OpenAI-compatible endpoints return only ids, so we
/// synthesize caps from known families. When the vendor includes richer fields
/// (`context_length`, `context_window`, image token prices), those override the
/// curated defaults — xAI does this on `/v1/models`.
fn parse_openai_models_list(data: &Value) -> Vec<ModelInfo> {
    let Some(arr) = data.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.to_string();
            let name = m
                .get("name")
                .or_else(|| m.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let mut info = openai_model_caps(&id, &name);
            apply_live_model_fields(m, &mut info);
            Some(info)
        })
        .collect();
    if out.is_empty() {
        return Vec::new();
    }
    // de-dup by id, preserve order
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

/// Overlay vendor-reported fields from a `/models` list item onto curated caps.
/// Safe no-ops when the fields are absent (vanilla OpenAI list).
pub(crate) fn apply_live_model_fields(m: &Value, info: &mut ModelInfo) {
    if let Some(reasoning) = m.get("reasoning").and_then(|v| v.as_bool()) {
        info.reasoning = reasoning;
    }
    if let Some(levels) = m
        .get("reasoning_levels")
        .or_else(|| m.get("thinking_levels"))
        .and_then(|v| v.as_array())
    {
        info.thinking_levels = levels
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .filter(|v| !v.is_empty())
            .collect();
    }
    if let Some(ctx) = m
        .get("context_length")
        .or_else(|| m.get("context_window"))
        .or_else(|| m.get("max_context_window"))
        .or_else(|| m.get("max_model_len"))
        .or_else(|| m.get("max_input_tokens"))
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
    {
        info.context_window = ctx.min(u32::MAX as u64) as u32;
    }
    if let Some(max) = m
        .get("max_tokens")
        .or_else(|| m.get("max_output_tokens"))
        .or_else(|| m.get("max_completion_tokens"))
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
    {
        info.max_tokens = max.min(u32::MAX as u64) as u32;
    }
    let modalities = |primary: &str, nested: &str| {
        m.get(primary)
            .or_else(|| m.get("modalities").and_then(|v| v.get(nested)))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
    };
    if let Some(input) = modalities("input_modalities", "input") {
        info.vision = input.iter().any(|v| v.eq_ignore_ascii_case("image"));
        info.input = input;
    }
    if let Some(output) = modalities("output_modalities", "output") {
        info.output = output;
    }
    if let Some(v) = m
        .get("tool_call")
        .or_else(|| m.get("supports_tools"))
        .and_then(|v| v.as_bool())
    {
        info.tool_call = v;
    }
    if let Some(v) = m
        .get("structured_output")
        .or_else(|| m.get("supports_structured_output"))
        .and_then(|v| v.as_bool())
    {
        info.structured_output = v;
    }
    sanitize_model_caps(info);
    if m.get("prompt_image_token_price")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0
    {
        info.vision = true;
        if !info.input.iter().any(|v| v == "image") {
            info.input.push("image".into());
        }
    }
}
/// Apply rich fields from a `/models` list after registry enrichment, preserving
/// endpoint metadata over the public registry when both provide a value.
pub(crate) fn apply_live_model_list_fields(data: &Value, models: &mut [ModelInfo]) {
    let Some(entries) = data.get("data").and_then(Value::as_array) else {
        return;
    };
    for info in models {
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(info.id.as_str()))
        {
            apply_live_model_fields(entry, info);
        }
    }
}
/// and filtering out image/video/TTS media models that cannot run the agent loop.
pub(crate) fn parse_xai_models_list(data: &Value) -> Vec<ModelInfo> {
    let Some(arr) = data.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter(|m| is_xai_chat_model_entry(m))
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.to_string();
            let name = m
                .get("name")
                .or_else(|| m.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let mut info = xai_model_caps(&id, &name);
            apply_live_model_fields(m, &mut info);
            // If context came from the API, re-derive a sensible max_tokens
            // (xAI does not publish max output on this endpoint).
            if m.get("context_length").is_some() {
                info.max_tokens = xai_default_max_tokens(info.context_window);
            }
            Some(info)
        })
        .collect();
    if out.is_empty() {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    sort_xai_models(&mut out);
    out
}

/// True when a `/models` list item is a chat/completions language model (not
/// Grok Imagine image/video or other media-only surfaces).
fn is_xai_chat_model_entry(m: &Value) -> bool {
    let id = m
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if id.is_empty() {
        return false;
    }
    // Media / non-chat surfaces.
    if id.contains("imagine")
        || id.contains("image")
        || id.contains("video")
        || id.contains("tts")
        || id.contains("speech")
        || id.contains("voice")
        || id.contains("embedding")
        || id.contains("whisper")
    {
        return false;
    }
    // Chat models advertise text completion pricing and/or a context window.
    m.get("completion_text_token_price").is_some()
        || m.get("context_length")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0
}

/// Default max output tokens when the vendor does not report one. Keeps a
/// comfortable generation budget while leaving headroom under the context window.
fn xai_default_max_tokens(context_window: u32) -> u32 {
    if context_window <= 1 {
        return 1;
    }
    // Reserve a quarter of the context for the prompt and cap the default
    // output budget for providers that do not publish one. Never return a
    // value equal to or above the context window, even for tiny local models.
    (context_window.saturating_mul(3) / 4)
        .max(1)
        .min(65_536)
        .min(context_window - 1)
}

/// Curated xAI Grok capabilities used as the base before live `/models` fields
/// overlay `context_length` / vision. Reasoning is inferred from the model id.
pub(crate) fn xai_model_caps(id: &str, name: &str) -> ModelInfo {
    let l = id.to_ascii_lowercase();
    let std_levels: Vec<String> = DEFAULT_THINKING_LEVELS
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Offline defaults aligned with live xAI catalog (May–Jul 2026). Live
    // `context_length` from `/models` always wins when discovery succeeds.
    let (ctx, reasoning, levels): (u32, bool, Vec<String>) = if l.contains("non-reasoning") {
        (1_000_000, false, Vec::new())
    } else if l.contains("grok-build") {
        (256_000, true, std_levels.clone())
    } else if l.contains("grok-4.5") {
        (500_000, true, std_levels.clone())
    } else if l.contains("grok-4.3") || l.contains("grok-4.20") || l.contains("multi-agent") {
        (1_000_000, true, std_levels.clone())
    } else if l.contains("grok") {
        (256_000, true, std_levels)
    } else {
        (200_000, true, Vec::new())
    };
    // All current Grok chat models accept image inputs (prompt_image_token_price).
    let vision = l.contains("grok");
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        reasoning,
        context_window: ctx,
        max_tokens: xai_default_max_tokens(ctx),
        thinking_levels: levels,
        vision,
        ..Default::default()
    }
}

/// Pin coding default (`grok-build-0.1`) first, then flagship reasoning models.
fn sort_xai_models(models: &mut [ModelInfo]) {
    models.sort_by(|a, b| {
        xai_model_sort_key(&a.id)
            .cmp(&xai_model_sort_key(&b.id))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn xai_model_sort_key(id: &str) -> u32 {
    let l = id.to_ascii_lowercase();
    if l == "grok-build-0.1" || l.starts_with("grok-build") {
        0
    } else if l.starts_with("grok-4.5") {
        1
    } else if l.starts_with("grok-4.3") {
        2
    } else if l.contains("reasoning") && !l.contains("non-reasoning") {
        3
    } else if l.contains("multi-agent") {
        4
    } else if l.contains("non-reasoning") {
        5
    } else {
        10
    }
}

/// Fetch xAI `GET /v1/language-models` and return a map of model id → whether
/// the model accepts image inputs. Used to drop media-only ids and set vision.
async fn fetch_xai_language_model_ids(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
) -> Option<std::collections::HashMap<String, bool>> {
    let url = format!(
        "{}/language-models",
        provider.base_url.trim_end_matches('/')
    );
    let mut req = client.get(&url).timeout(Duration::from_secs(8));
    if let Some(k) = provider.api_key.as_deref() {
        req = req.bearer_auth(k);
    }
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    let arr = v
        .get("models")
        .or_else(|| v.get("data"))
        .and_then(|d| d.as_array())?;
    let mut map = std::collections::HashMap::new();
    for m in arr {
        let Some(id) = m.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        let vision = m
            .get("input_modalities")
            .and_then(|mods| mods.as_array())
            .map(|mods| mods.iter().any(|x| x.as_str() == Some("image")))
            .unwrap_or(false)
            || m.get("prompt_image_token_price")
                .and_then(|p| p.as_u64())
                .unwrap_or(0)
                > 0;
        map.insert(id.to_string(), vision);
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// Restrict the discovered list to language-models ids (when that catalog is
/// available) and apply vision flags from `input_modalities`.
pub(crate) fn apply_xai_language_models_enrichment(
    models: &mut Vec<ModelInfo>,
    language: &std::collections::HashMap<String, bool>,
) {
    models.retain(|m| language.contains_key(&m.id));
    for m in models.iter_mut() {
        if let Some(&vision) = language.get(&m.id) {
            m.vision = vision;
        }
    }
    sort_xai_models(models);
}

/// Parse ChatGPT Codex `GET /backend-api/codex/models` (`{models:[...]}`).
/// This is the subscription catalog, so it is the source of truth for which
/// ChatGPT models the logged-in account can actually use.
pub(crate) fn parse_codex_models_response(data: &Value) -> Vec<ModelInfo> {
    let Some(arr) = data.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<(Option<u64>, ModelInfo)> = arr
        .iter()
        .filter(|m| {
            // The Codex catalog marks internal/auto models with
            // `visibility: "hide"` (e.g. `codex-auto-review`). These must
            // never be offered or picked as the default — they aren't meant
            // for direct user turns. The official codex CLI excludes them the
            // same way (only `visibility == "list"` models appear in the picker).
            m.get("supported_in_api")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
                && m.get("visibility").and_then(|v| v.as_str()) != Some("hide")
        })
        .filter_map(|m| {
            let id = m
                .get("slug")
                .or_else(|| m.get("id"))
                .and_then(|v| v.as_str())?;
            let name = m
                .get("display_name")
                .or_else(|| m.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(id);
            let mut info = openai_model_caps(id, name);
            if let Some(ctx) = m
                .get("context_window")
                .or_else(|| m.get("max_context_window"))
                .and_then(|v| v.as_u64())
            {
                info.context_window = ctx.min(u32::MAX as u64) as u32;
            }
            let levels = m
                .get("supported_reasoning_levels")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            x.get("effort")
                                .and_then(|v| v.as_str())
                                .or_else(|| x.as_str())
                                .map(String::from)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !levels.is_empty() {
                info.thinking_levels = levels;
                info.reasoning = true;
            }
            info.vision = m
                .get("supports_image_detail_original")
                .and_then(|v| v.as_bool())
                .unwrap_or(info.vision);
            let priority = m.get("priority").and_then(|v| v.as_u64());
            Some((priority, info))
        })
        .collect();
    // Sort by the catalog's `priority` ascending so the flagship (lowest
    // priority number, e.g. gpt-5.5) lands first and becomes the
    // default-selected model. The official codex CLI picks its default via a
    // separate server `is_default` flag that isn't exposed in this response;
    // priority-ascending is a faithful proxy. Models lacking a priority sort
    // last (stable).
    out.sort_by_key(|(p, _)| p.unwrap_or(u64::MAX));
    let mut out: Vec<ModelInfo> = out.into_iter().map(|(_, m)| m).collect();
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

/// Curated capabilities for an OpenAI- or Gemini-family model id. Returns
/// conservative defaults (ctx 200k, max 8k, reasoning true, vision false) for
/// unknown ids so an unrecognized model still works.
#[allow(clippy::if_same_then_else)]
pub(crate) fn openai_model_caps(id: &str, name: &str) -> ModelInfo {
    let l = id.to_ascii_lowercase();
    let std_levels: Vec<String> = DEFAULT_THINKING_LEVELS
        .iter()
        .map(|s| s.to_string())
        .collect();
    // (context_window, max_tokens, reasoning, vision, thinking_levels)
    let (ctx, max, reasoning, vision, levels): (u32, u32, bool, bool, Vec<String>) = if l
        .contains("gpt-5-codex")
    {
        (272_144, 163_840, true, true, std_levels.clone())
    } else if l.contains("gpt-5") {
        (272_144, 128_000, true, true, std_levels.clone())
    } else if l.contains("o4-mini") {
        (200_000, 100_000, true, true, std_levels.clone())
    } else if l.starts_with("o4") || l.contains("o4-") {
        (200_000, 100_000, true, true, std_levels.clone())
    } else if l.starts_with("o3") || l.contains("o3-") {
        (200_000, 100_000, true, false, std_levels.clone())
    } else if l.contains("o1") {
        (200_000, 100_000, true, false, vec!["high".to_string()])
    } else if l.contains("gpt-4.1") {
        (1_047_576, 32_768, false, true, Vec::new())
    } else if l.contains("gpt-4o") {
        (128_000, 16_384, false, true, Vec::new())
    } else if l.contains("gemini-3") && l.contains("flash") {
        // Gemini 3 Flash (Antigravity): thinkingLevel minimal/low/medium/high.
        (
            1_048_576,
            65_536,
            true,
            true,
            vec![
                "minimal".into(),
                "low".into(),
                "medium".into(),
                "high".into(),
            ],
        )
    } else if l.contains("gemini-3") {
        // Gemini 3 / 3.1 Pro (Antigravity): thinkingLevel low/high only.
        (
            1_048_576,
            65_535,
            true,
            true,
            vec!["low".into(), "high".into()],
        )
    } else if l.contains("claude-opus") && l.contains("thinking") {
        // Claude-via-Antigravity (Opus thinking) — 200k ctx.
        (200_000, 64_000, true, true, std_levels.clone())
    } else if l.contains("claude-sonnet-4") || l.contains("claude-opus-4") {
        (200_000, 64_000, false, true, Vec::new())
    } else if l.contains("gemini-2.5-pro") || (l.contains("gemini-2.5") && !l.contains("flash")) {
        (1_048_576, 65_536, true, true, std_levels.clone())
    } else if l.contains("gemini-2.5-flash") {
        (1_048_576, 65_536, true, true, std_levels.clone())
    } else if l.contains("gemini-2.0-flash") {
        (1_048_576, 8_192, false, true, Vec::new())
    } else if l.contains("gemini") {
        (1_048_576, 8_192, false, true, Vec::new())
    } else if l.contains("grok") {
        // Delegate to xai_model_caps so offline OpenAI-list parsing matches
        // the SuperGrok catalog (context_length is overlaid from live API).
        return xai_model_caps(id, name);
    } else {
        (200_000, 8_192, true, false, Vec::new())
    };
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        reasoning,
        context_window: ctx,
        max_tokens: max,
        thinking_levels: levels,
        vision,
        ..Default::default()
    }
}

/// True when the base URL points at Google's Gemini OpenAI-compatible endpoint.
pub fn is_gemini_endpoint(base_url: &str) -> bool {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "generativelanguage.googleapis.com"
}

/// True when the base URL points at a Code Assist / Antigravity gateway
/// (`cloudcode-pa.googleapis.com` or the daily/autopush sandboxes). OAuth-
/// authenticated Gemini/Claude-via-Antigravity requests are routed here —
/// `generativelanguage.googleapis.com` only accepts API keys.
pub fn is_code_assist_endpoint(base_url: &str) -> bool {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "cloudcode-pa.googleapis.com"
        || host == "daily-cloudcode-pa.sandbox.googleapis.com"
        || host == "autopush-cloudcode-pa.sandbox.googleapis.com"
        || (host.ends_with(".sandbox.googleapis.com") && host.contains("cloudcode-pa"))
}

/// True when the base URL points at ChatGPT's Codex subscription backend.
pub fn is_codex_endpoint(base_url: &str) -> bool {
    let host_path = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .trim_end_matches('/')
        .to_ascii_lowercase();
    host_path == "chatgpt.com/backend-api/codex"
        || host_path == "chat.openai.com/backend-api/codex"
        || host_path == "chatgpt-staging.com/backend-api/codex"
}

/// Default headers the official `codex` CLI attaches to every ChatGPT Codex
/// request. `originator` identifies the client; `User-Agent` avoids looking
/// like a bare bot. Idempotent — skips names already present.
pub fn inject_codex_headers(headers: &mut Vec<(String, String)>) {
    const ORIGINATOR: &str = "codex_cli_rs";
    let mut has_originator = false;
    let mut has_ua = false;
    for (k, _) in headers.iter() {
        let kl = k.to_ascii_lowercase();
        if kl == "originator" {
            has_originator = true;
        }
        if kl == "user-agent" {
            has_ua = true;
        }
    }
    if !has_originator {
        headers.push(("originator".to_string(), ORIGINATOR.to_string()));
    }
    if !has_ua {
        let os = if cfg!(target_os = "macos") {
            "macOS"
        } else if cfg!(target_os = "windows") {
            "Windows"
        } else if cfg!(target_os = "linux") {
            "Linux"
        } else {
            "Unix"
        };
        headers.push((
            "User-Agent".to_string(),
            format!(
                "codex_cli_rs/{} ({}; {})",
                env!("CARGO_PKG_VERSION"),
                os,
                std::env::consts::ARCH
            ),
        ));
    }
}

/// True if the base URL points at an OpenCode Go endpoint. OpenCode Go is a
/// single subscription that serves some models via an OpenAI-compatible
/// `/v1/chat/completions` endpoint and others via an Anthropic `/v1/messages`
/// endpoint — all under one API key at `https://opencode.ai/zen/go/v1`. The
/// harness models this as TWO provider configs (one OpenAI-kind, one
/// Anthropic-kind) sharing the base URL + key; discovery returns a curated,
/// protocol-specific model list for each (see `opencode_go_openai_models` /
/// `opencode_go_anthropic_models`).
pub fn is_opencode_go(base_url: &str) -> bool {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "opencode.ai" && base_url.to_ascii_lowercase().contains("/zen/go/")
}

pub fn is_iflow_endpoint(base_url: &str) -> bool {
    let h = endpoint_host(base_url);
    h == "apis.iflow.cn" || h == "iflow.cn" || h.ends_with(".iflow.cn")
}

fn endpoint_host(base_url: &str) -> String {
    base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn endpoint_authority(base_url: &str) -> String {
    base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// True if the base URL points at a Kimi / Moonshot endpoint
/// (`api.kimi.com` coding subscription or `api.moonshot.ai` platform API).
/// Used to route discovery through the curated `kimi_model_caps` table + live
/// `/models` list. Must stay aligned with `crate::provider::is_kimi`.
pub fn is_kimi(base_url: &str) -> bool {
    let host = endpoint_host(base_url);
    host == "api.kimi.com" || host == "api.moonshot.ai"
}

/// True if the base URL points at DeepSeek's official API. The endpoint uses
/// the standard OpenAI `/models` response shape but does not publish model
/// limits in that response.
pub fn is_deepseek(base_url: &str) -> bool {
    endpoint_host(base_url) == "api.deepseek.com"
}

/// Capabilities for an OpenCode Go model id. The OpenCode Go `/v1/models`
/// endpoint returns only ids (no context window / max tokens / reasoning /
/// vision), and does NOT indicate which wire protocol each model uses — that
/// mapping lives in the OpenCode docs, not the API — so the harness curates
/// the list. The per-model (context_window, max_tokens, vision) values come
/// from the **Models.dev** registry (`https://models.dev/models.json`) — the
/// same registry OpenCode itself uses — keyed by each model's upstream provider
/// entry (e.g. `zhipuai/glm-5.2`, `minimax/MiniMax-M3`). The OpenCode Go
/// endpoint exposes no richer endpoint of its own (`/v1/models/info` and
/// `/v1/models/{id}` both 404), so Models.dev is the authoritative source.
/// `max_tokens` is the model's max OUTPUT: for the Anthropic-served models
/// (MiniMax/Qwen) the harness sends it as the request `max_tokens` (Anthropic
/// requires the field), so an accurate value avoids truncating long replies;
/// for the OpenAI-served models `max_tokens` is metadata only (the OpenAI path
/// does not send it, so the server applies its own default). `context_window`
/// drives the harness's compaction threshold, so an accurate value keeps
/// compaction from firing far too early on the million-token models.
///
/// Reasoning: OpenCode Go is a multi-vendor proxy, not the first-party
/// MiniMax/Kimi/Zhipu hosts. The OpenAI request builder only emits vendor
/// thinking fields on those exact hosts, so OpenAI-served OpenCode Go models
/// advertise `reasoning: false` + empty levels (honest: levels would not reach
/// the wire). Anthropic-served MiniMax/Qwen still get thinking levels because
/// `build_anthropic_request` emits the standard Anthropic thinking block; the
/// MiniMax adaptive rewrite applies only when the base URL is a MiniMax host.
/// Context/max/vision come from official model docs via [`opencode_go_caps`].
fn opencode_go_model_caps(id: &str, name: &str) -> ModelInfo {
    let (context_window, max_tokens, vision) =
        opencode_go_caps(id).unwrap_or((200_000, 8_192, false));
    let (reasoning, thinking_levels) = if opencode_go_model_protocol(id) == Some(false) {
        // Anthropic-served models (MiniMax/Qwen): standard Anthropic thinking.
        (true, vec!["low".into(), "medium".into(), "high".into()])
    } else {
        (false, Vec::new())
    };
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        reasoning,
        context_window,
        max_tokens,
        thinking_levels,
        vision,
        ..Default::default()
    }
}

/// Real `(context_window, max_tokens, vision)` for each documented OpenCode Go
/// model id, sourced from Models.dev (`https://models.dev/models.json`). Values
/// are the upstream model's limits (OpenCode Go passes the upstream context
/// through, per its tiered pricing for the 256K+/1M models). `vision` is true
/// when the upstream entry's `modalities.input` includes `image`. Returns
/// `None` for ids the registry hasn't indexed; the caller then uses flat
/// defaults. Keep this in sync with [`opencode_go_known_models`] (ids + display
/// names) and [`opencode_go_model_protocol`] (family→wire-protocol routing).
fn opencode_go_caps(id: &str) -> Option<(u32, u32, bool)> {
    let l = id.to_ascii_lowercase();
    Some(match l.as_str() {
        // OpenAI-compatible /v1/chat/completions (zhipu / moonshot / deepseek / xiaomi)
        "glm-5.2" => (1_000_000, 131_072, false),
        "glm-5.1" => (200_000, 131_072, false),
        "glm-5-turbo" => (200_000, 131_072, false),
        // Official Kimi K3 is 1M/128K with low/high/max effort. Keep output under
        // context so sanitize does not rewrite equal pairs.
        "kimi-k3" => (1_048_576, 131_072, true),
        "kimi-k2.7-code" => (262_144, 32_768, true),
        "kimi-k2.7-code-highspeed" => (262_144, 32_768, true),
        "kimi-k2.6" => (262_144, 32_768, true),
        "deepseek-v4-pro" => (1_000_000, 384_000, false),
        "deepseek-v4-flash" => (1_000_000, 384_000, false),
        "mimo-v2.5" => (1_048_576, 131_072, true),
        "mimo-v2.5-pro" => (1_048_576, 131_072, false),
        // Anthropic /v1/messages (minimax / alibaba). Official MiniMax-M3 is 1M.
        "minimax-m3" => (1_000_000, 128_000, true),
        "minimax-m2.7" => (204_800, 131_072, false),
        "minimax-m2.7-highspeed" => (204_800, 131_072, false),
        "minimax-m2.5" => (204_800, 131_072, false),
        "minimax-m2.5-highspeed" => (204_800, 131_072, false),
        "qwen3.7-max" => (1_000_000, 65_536, false),
        "qwen3.7-plus" => (1_000_000, 64_000, true),
        "qwen3.6-plus" => (1_000_000, 65_536, true),
        _ => return None,
    })
}

/// All OpenCode Go model ids documented in the OpenCode Go docs endpoint
/// table, paired with their display names. The live `/v1/models` endpoint
/// returns ids without display names or a protocol field, so this table
/// supplies both: the display name (for known ids) and, via the family prefix
/// in [`opencode_go_model_protocol`], the wire protocol. It is also the
/// offline fallback when the endpoint is unreachable.
fn opencode_go_known_models() -> &'static [(&'static str, &'static str)] {
    &[
        // OpenAI-compatible /v1/chat/completions
        ("glm-5.2", "GLM-5.2"),
        ("glm-5.1", "GLM-5.1"),
        ("glm-5-turbo", "GLM-5-Turbo"),
        ("kimi-k3", "Kimi K3"),
        ("kimi-k2.7-code", "Kimi K2.7 Code"),
        ("kimi-k2.7-code-highspeed", "Kimi K2.7 Code Highspeed"),
        ("kimi-k2.6", "Kimi K2.6"),
        ("deepseek-v4-pro", "DeepSeek V4 Pro"),
        ("deepseek-v4-flash", "DeepSeek V4 Flash"),
        ("mimo-v2.5", "MiMo-V2.5"),
        ("mimo-v2.5-pro", "MiMo-V2.5-Pro"),
        // Anthropic /v1/messages
        ("minimax-m3", "MiniMax M3"),
        ("minimax-m2.7", "MiniMax M2.7"),
        ("minimax-m2.7-highspeed", "MiniMax M2.7 Highspeed"),
        ("minimax-m2.5", "MiniMax M2.5"),
        ("minimax-m2.5-highspeed", "MiniMax M2.5 Highspeed"),
        ("qwen3.7-max", "Qwen3.7 Max"),
        ("qwen3.7-plus", "Qwen3.7 Plus"),
        ("qwen3.6-plus", "Qwen3.6 Plus"),
    ]
}

/// The wire protocol an OpenCode Go model id is served over, inferred from its
/// family prefix. The `/v1/models` endpoint exposes no protocol field, but the
/// OpenCode Go docs endpoint table partitions cleanly by family:
/// `glm`/`kimi`/`deepseek`/`mimo` → OpenAI (`/v1/chat/completions`);
/// `minimax`/`qwen` → Anthropic (`/v1/messages`). Returns `None` for ids whose
/// family is unknown to the docs (e.g. `hy3-preview`) — those are dropped
/// during discovery rather than misrouted to a protocol they may not speak.
pub(crate) fn opencode_go_model_protocol(id: &str) -> Option<bool> {
    let l = id.to_ascii_lowercase();
    if l.starts_with("glm-")
        || l.starts_with("kimi-")
        || l.starts_with("deepseek-")
        || l.starts_with("mimo-")
    {
        Some(true)
    } else if l.starts_with("minimax-") || l.starts_with("qwen") {
        Some(false)
    } else {
        None
    }
}

/// Display name for an OpenCode Go model id: the curated name from the docs
/// table when known, else synthesized as `Brand <rest>` from the family prefix
/// (so newly-added ids the docs table hasn't caught up to still get a readable
/// name instead of a raw slug).
pub(crate) fn opencode_go_display_name(id: &str) -> String {
    let l = id.to_ascii_lowercase();
    if let Some((_, name)) = opencode_go_known_models().iter().find(|(k, _)| *k == l) {
        return name.to_string();
    }
    let (rest, brand) = if let Some(r) = l.strip_prefix("glm-") {
        (r, "GLM")
    } else if let Some(r) = l.strip_prefix("kimi-") {
        (r, "Kimi")
    } else if let Some(r) = l.strip_prefix("deepseek-") {
        (r, "DeepSeek")
    } else if let Some(r) = l.strip_prefix("mimo-") {
        (r, "MiMo")
    } else if let Some(r) = l.strip_prefix("minimax-") {
        (r, "MiniMax")
    } else if let Some(r) = l.strip_prefix("qwen") {
        (r, "Qwen")
    } else {
        return id.to_string();
    };
    let rest_str: String = rest
        .split('-')
        .map(|tok| {
            let mut c = tok.chars();
            match c.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if rest_str.is_empty() {
        brand.to_string()
    } else {
        format!("{brand} {rest_str}")
    }
}

/// Parse an OpenCode Go `/v1/models` response (`{data:[{id,...}]}`) and keep
/// only the ids served over the given wire protocol, mapping each to curated
/// capabilities. The endpoint lists every model with no protocol field, so we
/// partition by family prefix (see [`opencode_go_model_protocol`]); ids whose
/// family is unknown are dropped (we can't safely route them).
pub(crate) fn opencode_go_filter_models(data: &Value, openai: bool) -> Vec<ModelInfo> {
    let Some(arr) = data.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?;
            if opencode_go_model_protocol(id) != Some(openai) {
                return None;
            }
            let name = opencode_go_display_name(id);
            Some(opencode_go_model_caps(id, &name))
        })
        .collect();
    // de-dup by id, preserve order
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

/// Discover OpenCode Go models by fetching the single `/v1/models` endpoint
/// (which lists every model over both wire protocols, with no protocol field),
/// filtering to `openai`-protocol models, and caching the result. Falls back to
/// the stale disk cache, then the hardcoded curated list, when the endpoint is
/// unreachable.
///
/// OpenCode Go is modeled as TWO provider configs sharing one base URL + key
/// (OpenAI-kind + Anthropic-kind); this is called for each with `openai`
/// selecting the protocol. The cache key already encodes the kind, so the two
/// partitions never collide.
pub(crate) async fn opencode_go_discover_models(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    cache_key: &str,
    openai: bool,
    force_live: bool,
) -> Vec<ModelInfo> {
    // 1. Fresh disk cache (< 8h TTL), unless this is an explicit refresh.
    if !force_live {
        if let Some(models) = read_models_cache(cache_key) {
            return models;
        }
    }
    // 2. Fetch the live OpenAI-style /v1/models list. The endpoint serves every
    //    model here regardless of wire protocol; auth is optional (the list is
    //    public) but we send the key when configured.
    let url = format!("{}{OPENAI_MODELS_PATH}", provider.base_url);
    let mut req = client.get(&url).timeout(Duration::from_secs(8));
    if let Some(k) = provider.api_key.as_deref() {
        req = req.bearer_auth(k);
    }
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    let live = match req.send().await {
        Ok(r) if r.status().is_success() => {
            opencode_go_filter_models(&r.json::<Value>().await.unwrap_or(Value::Null), openai)
        }
        _ => Vec::new(),
    };
    if !live.is_empty() {
        write_models_cache(cache_key, &live);
        return live;
    }
    // 3. Stale cache, else the hardcoded curated list for this protocol.
    read_models_cache_stale(cache_key).unwrap_or_else(|| opencode_go_fallback_models(openai))
}

/// Hardcoded curated list for one protocol — the offline fallback when the
/// OpenCode Go `/v1/models` endpoint is unreachable. Derived from
/// [`opencode_go_known_models`] filtered to the protocol family.
fn opencode_go_fallback_models(openai: bool) -> Vec<ModelInfo> {
    opencode_go_known_models()
        .iter()
        .filter(|(id, _)| opencode_go_model_protocol(id) == Some(openai))
        .map(|(id, name)| opencode_go_model_caps(id, name))
        .collect()
}

/// OpenCode Go models served via the OpenAI-compatible `/v1/chat/completions`
/// endpoint — the offline fallback for the `opencode-go` (OpenAI-kind) provider
/// config. Derived from [`opencode_go_known_models`] filtered to the OpenAI
/// protocol family.
#[allow(dead_code)]
pub(crate) fn opencode_go_openai_models() -> Vec<ModelInfo> {
    opencode_go_fallback_models(true)
}

/// OpenCode Go models served via the Anthropic `/v1/messages` endpoint — the
/// offline fallback for the `opencode-go-anthropic` (Anthropic-kind) provider
/// config. Derived from [`opencode_go_known_models`] filtered to the Anthropic
/// protocol family.
#[allow(dead_code)]
pub(crate) fn opencode_go_anthropic_models() -> Vec<ModelInfo> {
    opencode_go_fallback_models(false)
}

/// Discover Kimi (Moonshot) Code models from the live `/models` endpoint,
/// applying curated capabilities for known ids. Mirrors the OpenCode Go
/// pattern: fresh cache → live fetch → stale cache → curated fallback. The
/// live list drives auto-update (new Kimi models appear as Moonshot ships
/// them); the curated `kimi_model_caps` guarantees correct reasoning/vision
/// for known models regardless of what `/models` reports.
pub(crate) async fn kimi_discover_models(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    cache_key: &str,
    force_live: bool,
) -> Vec<ModelInfo> {
    if !force_live {
        if let Some(models) = read_models_cache(cache_key) {
            return models;
        }
    }
    let url = format!("{}{OPENAI_MODELS_PATH}", provider.base_url);
    let mut req = client.get(&url).timeout(Duration::from_secs(8));
    if let Some(k) = provider.api_key.as_deref() {
        req = req.bearer_auth(k);
    }
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    let live = match req.send().await {
        Ok(r) if r.status().is_success() => {
            kimi_parse_models_list(&r.json::<Value>().await.unwrap_or(Value::Null))
        }
        _ => Vec::new(),
    };
    if !live.is_empty() {
        write_models_cache(cache_key, &live);
        return live;
    }
    read_models_cache_stale(cache_key).unwrap_or_else(kimi_fallback_models)
}

/// Parse Kimi's OpenAI-style `/models` list (`{data:[{id,...}]}`), mapping each
/// id through the curated `kimi_model_caps` and overlaying any vendor-reported
/// fields (e.g. `context_length`).
fn kimi_parse_models_list(data: &Value) -> Vec<ModelInfo> {
    let Some(arr) = data.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.to_string();
            let name = m
                .get("name")
                .or_else(|| m.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let mut info = kimi_model_caps(&id, &name);
            apply_live_model_fields(m, &mut info);
            Some(info)
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

/// The known Kimi Code model ids + display names. The live `/models` endpoint
/// is authoritative for the id list (auto-update); this supplies the display
/// name + offline fallback.
fn kimi_known_models() -> &'static [(&'static str, &'static str)] {
    &[
        ("kimi-k3", "Kimi K3"),
        ("kimi-k2.7-code", "Kimi K2.7 Code"),
        ("kimi-k2.7-code-highspeed", "Kimi K2.7 Code Highspeed"),
        ("kimi-k2.6", "Kimi K2.6"),
        ("kimi-for-coding", "Kimi for Coding"),
    ]
}

/// Capabilities for a Kimi (Moonshot) model id. K3 is 1M context with low/high/max
/// effort; coding models keep Preserved Thinking always-on semantics.
fn kimi_model_caps(id: &str, name: &str) -> ModelInfo {
    let l = id.to_ascii_lowercase();
    let (context_window, max_tokens, vision, thinking_levels) =
        if l == "kimi-k3" || l.starts_with("kimi-k3-") {
            (
                1_048_576,
                131_072,
                true,
                vec!["low".into(), "high".into(), "max".into()],
            )
        } else if l.contains("k2.7-code") || l.contains("for-coding") {
            (262_144, 32_768, true, Vec::new())
        } else if l.contains("k2.6") || l.contains("k2.5") {
            (
                262_144,
                32_768,
                true,
                vec!["low".into(), "medium".into(), "high".into()],
            )
        } else {
            (
                200_000,
                8_192,
                true,
                vec!["low".into(), "high".into(), "max".into()],
            )
        };
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        reasoning: true,
        context_window,
        max_tokens,
        thinking_levels,
        vision,
        ..Default::default()
    }
}

/// Hardcoded curated Kimi model list — the offline fallback when the live
/// `/models` endpoint is unreachable. Derived from `kimi_known_models`.
fn kimi_fallback_models() -> Vec<ModelInfo> {
    kimi_known_models()
        .iter()
        .map(|(id, name)| kimi_model_caps(id, name))
        .collect()
}

/// Discover DeepSeek models from the live official `/models` endpoint. The
/// endpoint is the source of truth for model ids, while the curated table plus
/// models.dev supply the capability details that DeepSeek does not return.
pub(crate) async fn deepseek_discover_models(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    cache_key: &str,
    force_live: bool,
) -> Vec<ModelInfo> {
    if !force_live {
        if let Some(models) = read_models_cache(cache_key) {
            return models;
        }
    }
    let url = format!("{}{OPENAI_MODELS_PATH}", provider.base_url);
    let mut req = client.get(&url).timeout(Duration::from_secs(8));
    if let Some(k) = provider.api_key.as_deref() {
        req = req.bearer_auth(k);
    }
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    let mut live = match req.send().await {
        Ok(r) if r.status().is_success() => {
            deepseek_parse_models_list(&r.json::<Value>().await.unwrap_or(Value::Null))
        }
        _ => Vec::new(),
    };
    if !live.is_empty() {
        // DeepSeek's list endpoint intentionally omits context/output limits.
        // Enrich unknown/new ids from the shared registry while preserving the
        // official/curated values for models we know exactly.
        if let Some(dev) = crate::models_dev::fetch_models_dev(client).await {
            crate::models_dev::enrich_models(&mut live, &dev, &provider.base_url);
        }
        write_models_cache(cache_key, &live);
        return live;
    }
    read_models_cache_stale(cache_key).unwrap_or_else(deepseek_fallback_models)
}

/// Parse DeepSeek's OpenAI-style `/models` list (`{data:[{id,owned_by}]}`).
/// DeepSeek currently returns no capability fields, so known ids use the
/// curated table and unknown ids remain eligible for models.dev enrichment.
fn deepseek_parse_models_list(data: &Value) -> Vec<ModelInfo> {
    let Some(arr) = data.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.to_string();
            let name = m
                .get("name")
                .or_else(|| m.get("display_name"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| deepseek_display_name(&id));
            let mut info = deepseek_model_caps(&id, &name);
            apply_live_model_fields(m, &mut info);
            Some(info)
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    out.retain(|m| seen.insert(m.id.clone()));
    out
}

/// The current official DeepSeek model ids used for the offline fallback.
/// The live list remains authoritative, so a newly published id appears on
/// the next refresh without a code change.
fn deepseek_known_models() -> &'static [(&'static str, &'static str)] {
    &[
        ("deepseek-v4-flash", "DeepSeek V4 Flash"),
        ("deepseek-v4-pro", "DeepSeek V4 Pro"),
    ]
}

/// Curated DeepSeek capability details. DeepSeek V4 supports both thinking and
/// non-thinking modes, has a 1M context window, and allows up to 384K output.
/// Legacy ids are retained only when a still-serving endpoint reports them;
/// they are intentionally not part of the offline fallback after retirement.
fn deepseek_model_caps(id: &str, name: &str) -> ModelInfo {
    let l = id.to_ascii_lowercase();
    let (context_window, max_tokens, reasoning, thinking_levels) = if l.contains("v4") {
        (1_000_000, 384_000, true, vec!["high".into(), "max".into()])
    } else if l.contains("reasoner") {
        (131_072, 65_536, true, vec!["high".into(), "max".into()])
    } else if l == "deepseek-chat" {
        (131_072, 8_192, false, Vec::new())
    } else {
        // Unknown ids are deliberately generic so models.dev can fill them in
        // without a curated entry masking its reasoning/context metadata.
        (200_000, 8_192, false, Vec::new())
    };
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        reasoning,
        context_window,
        max_tokens,
        thinking_levels,
        vision: false,
        ..Default::default()
    }
}

/// Display name for a model id when DeepSeek's `/models` response only gives
/// the slug. Known ids use the documented names; new ids get a readable label.
fn deepseek_display_name(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    if let Some((_, name)) = deepseek_known_models()
        .iter()
        .find(|(known, _)| *known == lower)
    {
        return name.to_string();
    }
    let suffix = lower.strip_prefix("deepseek-").unwrap_or(&lower);
    let title = suffix
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "DeepSeek".to_string()
    } else {
        format!("DeepSeek {title}")
    }
}

fn deepseek_fallback_models() -> Vec<ModelInfo> {
    deepseek_known_models()
        .iter()
        .map(|(id, name)| deepseek_model_caps(id, name))
        .collect()
}

/// Curated catalogs for local engines whose model-list endpoints are commonly
/// unavailable during startup. These entries are intentionally limited to
/// recognizable local-engine endpoints; an arbitrary dead OpenAI-compatible
/// URL still returns no models rather than pretending to serve Umans models.
pub(crate) fn local_engine_fallback_models(base_url: &str) -> Vec<ModelInfo> {
    let authority = endpoint_authority(base_url);
    let (ids, ctx, max): (&[&str], u32, u32) =
        if authority == "localhost:11434" || authority == "127.0.0.1:11434" {
            (&["llama3.1", "qwen2.5-coder", "gemma3"], 32_768, 8_192)
        } else if authority == "localhost:1234" || authority == "127.0.0.1:1234" {
            (&["local-model"], 32_768, 8_192)
        } else if authority == "localhost:8080" || authority == "127.0.0.1:8080" {
            (&["local-model"], 16_384, 4_096)
        } else {
            return Vec::new();
        };
    ids.iter()
        .map(|id| ModelInfo {
            id: (*id).to_string(),
            name: (*id).to_string(),
            reasoning: false,
            context_window: ctx,
            max_tokens: max,
            input: vec!["text".into()],
            output: vec!["text".into()],
            tool_call: true,
            provider: "local".into(),
            ..Default::default()
        })
        .collect()
}

/// Curated fallback models for an OpenAI-compatible endpoint that served no
/// list at all. Gemini host → Gemini models; xAI host → Grok models; known
/// local engines → their explicit local catalog. Unknown endpoints return no
/// models: discovery failure must not produce a misleading Umans fallback.
fn openai_fallback_models(base_url: &str) -> Vec<ModelInfo> {
    if is_codex_endpoint(base_url) {
        return codex_fallback_models();
    }
    if is_gemini_endpoint(base_url) || is_code_assist_endpoint(base_url) {
        return gemini_fallback_models();
    }
    if is_xai_endpoint(base_url) {
        return xai_fallback_models();
    }
    if is_kimi(base_url) {
        return kimi_fallback_models();
    }
    if is_deepseek(base_url) {
        return deepseek_fallback_models();
    }
    if crate::provider::is_umans(base_url) {
        return fallback_models();
    }
    local_engine_fallback_models(base_url)
}

fn codex_fallback_models() -> Vec<ModelInfo> {
    // Current ChatGPT-subscription Codex model slugs (from the official codex
    // CLI's bundled models.json). These are the source of truth when the live
    // `/backend-api/codex/models` catalog can't be reached. The OLD list
    // (gpt-5.2-codex / gpt-5.1-codex-max / gpt-5-codex) are STALE slugs the
    // backend rejects with "model is not supported when using Codex with a
    // ChatGPT account". Ordered flagship-first so the first entry is the default.
    [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
        "gpt-5.2",
    ]
    .iter()
    .map(|id| openai_model_caps(id, id))
    .collect()
}

/// Static Antigravity / Gemini model list used when live discovery is
/// unreachable. Antigravity quota models first (Gemini 3 + Claude), then the
/// older Gemini 2.5 models as a secondary set.
fn gemini_fallback_models() -> Vec<ModelInfo> {
    // (id, display_name) — Antigravity model ids match the Code Assist gateway
    // (no "models/" prefix; Gemini 3 Pro uses -low/-high tiers).
    let ids: &[(&str, &str)] = &[
        ("gemini-3.1-pro-high", "Gemini 3.1 Pro (Antigravity)"),
        ("gemini-3-pro-high", "Gemini 3 Pro (Antigravity)"),
        ("gemini-3-flash", "Gemini 3 Flash (Antigravity)"),
        (
            "claude-opus-4-6-thinking",
            "Claude Opus 4.6 Thinking (Antigravity)",
        ),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Antigravity)"),
        ("gemini-2.5-pro", "Gemini 2.5 Pro"),
        ("gemini-2.5-flash", "Gemini 2.5 Flash"),
    ];
    ids.iter()
        .map(|(id, name)| openai_model_caps(id, name))
        .collect()
}

/// Static xAI Grok model list used when `/models` is unreachable. Context
/// windows match the live SuperGrok catalog; `grok-build-0.1` is first.
fn xai_fallback_models() -> Vec<ModelInfo> {
    let ids = [
        "grok-build-0.1",
        "grok-4.5",
        "grok-4.3",
        "grok-4.20-0309-reasoning",
        "grok-4.20-0309-non-reasoning",
        "grok-4.20-multi-agent-0309",
    ];
    let mut models: Vec<ModelInfo> = ids.iter().map(|id| xai_model_caps(id, id)).collect();
    sort_xai_models(&mut models);
    models
}

/// True when the base URL points at xAI's API (`api.x.ai`).
pub fn is_xai_endpoint(base_url: &str) -> bool {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "api.x.ai" || host == "x.ai" || host.ends_with(".x.ai")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_endpoint_detection() {
        assert!(is_kimi("https://api.kimi.com/coding/v1"));
        assert!(is_kimi("https://api.kimi.com/v1"));
        assert!(is_kimi("https://api.kimi.com:443/coding/v1"));
        assert!(is_kimi("https://api.moonshot.ai/v1"));
        assert!(!is_kimi("https://api.openai.com/v1"));
        // look-alike host must NOT match
        assert!(!is_kimi("https://api.kimi.com.evil.com/v1"));
        assert!(!is_kimi("https://evil.moonshot.ai/v1"));
    }
    #[test]
    fn kimi_model_caps_known_and_unknown() {
        let m = kimi_model_caps("kimi-for-coding", "Kimi for Coding");
        assert_eq!(m.id, "kimi-for-coding");
        assert_eq!(m.context_window, 262_144);
        assert_eq!(m.max_tokens, 32_768);
        assert!(m.reasoning);
        assert!(m.vision);
        // Coding models: always-on thinking; no selectable effort levels.
        assert!(m.thinking_levels.is_empty());

        let k3 = kimi_model_caps("kimi-k3", "Kimi K3");
        assert_eq!(k3.context_window, 1_048_576);
        assert_eq!(k3.max_tokens, 131_072);
        assert_eq!(k3.thinking_levels, vec!["low", "high", "max"]);

        // Unknown id → conservative defaults (still reasoning + vision so
        // thinking/image turns aren't silently disabled on a new release).
        let u = kimi_model_caps("kimi-future-99", "Kimi Future 99");
        assert!(u.reasoning);
        assert!(u.vision);
        assert_eq!(u.thinking_levels, vec!["low", "high", "max"]);
    }

    #[test]
    fn kimi_parse_models_list_maps_ids_and_overlays_context() {
        let data = json!({
            "data": [
                {"id": "kimi-for-coding"},
                {"id": "kimi-future", "context_length": 1000000}
            ]
        });
        let models = kimi_parse_models_list(&data);
        assert_eq!(models.len(), 2);
        let known = models.iter().find(|m| m.id == "kimi-for-coding").unwrap();
        assert_eq!(known.context_window, 262_144);
        let fut = models.iter().find(|m| m.id == "kimi-future").unwrap();
        // context_length from the live list overrides the curated default.
        assert_eq!(fut.context_window, 1_000_000);
    }

    #[test]
    fn kimi_fallback_models_curated() {
        let models = kimi_fallback_models();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "kimi-k3",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k2.6",
                "kimi-for-coding",
            ]
        );
    }

    #[test]
    fn deepseek_endpoint_detection() {
        assert!(is_deepseek("https://api.deepseek.com"));
        assert!(is_deepseek("https://api.deepseek.com/v1"));
        assert!(is_deepseek("https://api.deepseek.com:443/beta"));
        assert!(!is_deepseek("https://api.deepseek.com.evil.com/v1"));
        assert!(!is_deepseek("https://deepseek.com/v1"));
    }

    #[test]
    fn deepseek_model_caps_include_current_thinking_details() {
        let flash = deepseek_model_caps("deepseek-v4-flash", "DeepSeek V4 Flash");
        assert_eq!(flash.context_window, 1_000_000);
        assert_eq!(flash.max_tokens, 384_000);
        assert!(flash.reasoning);
        assert_eq!(flash.thinking_levels, vec!["high", "max"]);
        assert!(!flash.vision);

        // Unknown ids stay generic so models.dev can supply their real caps.
        let future = deepseek_model_caps("deepseek-v5-future", "DeepSeek V5 Future");
        assert_eq!(future.context_window, 200_000);
        assert!(future.thinking_levels.is_empty());
    }

    #[test]
    fn deepseek_parse_models_list_auto_names_and_deduplicates() {
        let data = json!({
            "object": "list",
            "data": [
                {"id": "deepseek-v4-flash", "owned_by": "deepseek"},
                {"id": "deepseek-v4-flash", "owned_by": "deepseek"},
                {"id": "deepseek-v5-future", "owned_by": "deepseek"}
            ]
        });
        let models = deepseek_parse_models_list(&data);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "DeepSeek V4 Flash");
        assert_eq!(models[1].name, "DeepSeek V5 Future");
        assert_eq!(models[1].context_window, 200_000);
    }

    #[test]
    fn deepseek_fallback_models_are_current_documented_ids() {
        let models = deepseek_fallback_models();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );
        assert!(models.iter().all(|m| m.context_window == 1_000_000));
    }

    #[test]
    fn openai_shape_b_max_input_tokens_sets_context_window() {
        let mut info = openai_model_caps("gateway-model", "Gateway Model");
        apply_live_model_fields(
            &json!({
                "id": "gateway-model",
                "max_input_tokens": 512000,
                "max_output_tokens": 8192
            }),
            &mut info,
        );
        assert_eq!(info.context_window, 512_000);
        assert_eq!(info.max_tokens, 8_192);
    }

    #[test]
    fn local_engine_fallbacks_are_curated_and_unknowns_are_empty() {
        let ollama = openai_fallback_models("http://localhost:11434/v1");
        assert_eq!(
            ollama.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["llama3.1", "qwen2.5-coder", "gemma3"]
        );
        assert!(ollama
            .iter()
            .all(|m| m.provider == "local" && m.context_window > m.max_tokens));
        assert!(openai_fallback_models("http://localhost:9999/v1").is_empty());
        assert!(openai_fallback_models("https://example.invalid/v1").is_empty());
    }

    #[test]
    fn openai_fallback_unknown_custom_endpoint_is_empty() {
        // A dead arbitrary URL must NOT inherit the Umans curated catalog.
        assert!(!openai_fallback_models("http://localhost:11434/v1").is_empty());
        assert!(openai_fallback_models("http://localhost:9999/v1").is_empty());
        assert!(openai_fallback_models("https://api.example.com/v1").is_empty());
        // Known vendors still get their curated lists.
        assert!(!openai_fallback_models("https://api.code.umans.ai/v1").is_empty());
        assert!(!openai_fallback_models("https://api.deepseek.com/v1").is_empty());
    }

    #[test]
    fn sanitize_model_caps_repairs_zero_and_inversion() {
        let mut zero_max = ModelInfo {
            id: "m".into(),
            name: "m".into(),
            context_window: 128_000,
            max_tokens: 0,
            ..Default::default()
        };
        sanitize_model_caps(&mut zero_max);
        assert!(zero_max.max_tokens > 0);
        assert!(zero_max.max_tokens < zero_max.context_window);

        let mut inverted = ModelInfo {
            id: "m".into(),
            name: "m".into(),
            context_window: 32_768,
            max_tokens: 100_000,
            ..Default::default()
        };
        sanitize_model_caps(&mut inverted);
        assert!(inverted.max_tokens < inverted.context_window);

        let mut zero_ctx = ModelInfo {
            id: "m".into(),
            name: "m".into(),
            context_window: 0,
            max_tokens: 0,
            ..Default::default()
        };
        sanitize_model_caps(&mut zero_ctx);
        assert_eq!(zero_ctx.context_window, 200_000);
        assert!(zero_ctx.max_tokens > 0 && zero_ctx.max_tokens < zero_ctx.context_window);
    }

    #[test]
    fn apply_live_model_fields_ignores_zero_max_tokens() {
        // Live max_tokens:0 must NOT stick — treat as missing and keep/repair.
        let mut info = openai_model_caps("gateway-model", "Gateway Model");
        let before = info.max_tokens;
        assert!(before > 0);
        apply_live_model_fields(
            &json!({
                "id": "gateway-model",
                "context_length": 128000,
                "max_tokens": 0,
                "max_output_tokens": 0
            }),
            &mut info,
        );
        assert_eq!(info.context_window, 128_000);
        assert!(info.max_tokens > 0);
        assert!(info.max_tokens < info.context_window);
        // Zero output must not clobber the curated positive budget with 0.
        assert_ne!(info.max_tokens, 0);
    }

    #[test]
    fn apply_live_model_fields_shape_b_max_input_and_output() {
        // Shape B bare gateway ids: max_input_tokens + max_output_tokens only.
        // Start from generic defaults (unknown id path would be 200k/8k).
        let mut info = openai_model_caps("custom-flash", "Custom Flash");
        apply_live_model_fields(
            &json!({
                "id": "custom-flash",
                "max_input_tokens": 512000,
                "max_output_tokens": 8192
            }),
            &mut info,
        );
        assert_eq!(info.context_window, 512_000);
        assert_eq!(info.max_tokens, 8_192);
    }

    #[test]
    fn apply_live_model_fields_clamps_output_equal_to_context() {
        let mut info = openai_model_caps("equal-caps", "Equal");
        apply_live_model_fields(
            &json!({
                "context_length": 1000,
                "max_output_tokens": 1000
            }),
            &mut info,
        );
        assert_eq!(info.context_window, 1_000);
        assert!(info.max_tokens < 1_000);
        assert!(info.max_tokens > 0);
    }

    #[test]
    fn parse_models_response_ignores_zero_recommended_max() {
        let data = json!({
            "zero-max": {
                "display_name": "Zero Max",
                "capabilities": {
                    "context_window": 128000,
                    "recommended_max_tokens": 0,
                    "supports_vision": false,
                    "reasoning": { "supported": false, "levels": [] }
                }
            },
            "zero-ctx": {
                "display_name": "Zero Ctx",
                "capabilities": {
                    "context_window": 0,
                    "recommended_max_tokens": 4096,
                    "supports_vision": false,
                    "reasoning": { "supported": false, "levels": [] }
                }
            }
        });
        let models = parse_models_response(&data);
        let zm = models.iter().find(|m| m.id == "zero-max").unwrap();
        assert_eq!(zm.context_window, 128_000);
        assert!(zm.max_tokens > 0 && zm.max_tokens < zm.context_window);
        let zc = models.iter().find(|m| m.id == "zero-ctx").unwrap();
        assert_eq!(zc.context_window, 200_000); // 0 → generic default
        assert!(zc.max_tokens > 0 && zc.max_tokens < zc.context_window);
    }

    #[test]
    fn apply_models_override_rejects_zero_max_and_repairs_inversion() {
        use crate::config::ModelOverride;
        let provider = ResolvedProvider {
            name: "custom".into(),
            kind: ProviderKind::OpenAI,
            base_url: "http://localhost:1234/v1".into(),
            api_key: None,
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: vec![ModelOverride {
                id: "gemma".into(),
                context_window: Some(32_768),
                max_tokens: Some(0), // must be ignored (filter > 0)
                reasoning: None,
                thinking_levels: None,
                ..Default::default()
            }],
            models_endpoint: None,
        };
        let mut models = vec![ModelInfo {
            id: "gemma".into(),
            name: "Gemma".into(),
            context_window: 200_000,
            max_tokens: 8_192,
            ..Default::default()
        }];
        apply_models_override(&provider, &mut models);
        assert_eq!(models[0].context_window, 32_768);
        // max_tokens:0 override ignored; previous 8192 still < 32768 so kept,
        // or repaired if it had been inverted — never 0.
        assert!(models[0].max_tokens > 0);
        assert!(models[0].max_tokens < models[0].context_window);
    }

    #[test]
    fn parse_openai_models_list_shape_b_bare_ids() {
        let data = json!({
            "data": [{
                "id": "deepseek-v4-flash",
                "max_input_tokens": 128000,
                "max_output_tokens": 32768
            }]
        });
        let models = parse_openai_models_list(&data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "deepseek-v4-flash");
        assert_eq!(models[0].context_window, 128_000);
        assert_eq!(models[0].max_tokens, 32_768);
    }

    #[test]
    fn opencode_go_kimi_caps_keep_max_below_context() {
        let (ctx, max, _) = opencode_go_caps("kimi-k2.6").unwrap();
        assert!(max < ctx, "max={max} must be < ctx={ctx}");
        let m = opencode_go_model_caps("kimi-k2.6", "Kimi K2.6");
        assert!(m.max_tokens < m.context_window);
    }
}
