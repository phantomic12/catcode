//! Models.dev runtime enrichment.
//!
//! `https://models.dev/models.json` is the authoritative cross-provider
//! model-capabilities registry (context window, max output, reasoning,
//! vision). OpenCode itself uses it. We fetch it once (cached 24h on disk)
//! and use it to enrich model capabilities for providers whose `/v1/models`
//! endpoint returns only bare `{id}` lists — which is the common case for
//! OpenAI-compatible gateways (DeepSeek, Groq, Mistral, OpenRouter, etc.).
//!
//! Enrichment is an OVERLAY: it only fills in fields that the curated table
//! and the live API left at generic defaults. Values from the live API
//! (`apply_live_model_fields`) and the curated table always take priority.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const MODELS_DEV_URL: &str = "https://models.dev/models.json";
/// 24h TTL — model caps don't change often, and this avoids hitting the
/// registry on every discovery call.
const MODELS_DEV_CACHE_TTL: u64 = 86400;

/// A single model entry from models.dev (only the fields we use).
#[derive(Clone, Debug)]
pub struct ModelsDevEntry {
    #[allow(dead_code)]
    pub name: String,
    pub context: u32,
    pub output: u32,
    pub reasoning: bool,
    pub vision: bool,
    pub input: Vec<String>,
    pub output_modalities: Vec<String>,
    pub tool_call: bool,
    pub structured_output: bool,
}

/// Parse the models.dev JSON into a flat lookup keyed by `provider/model-id`.
fn parse_registry(data: &Value) -> HashMap<String, ModelsDevEntry> {
    let Some(obj) = data.as_object() else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (key, val) in obj {
        let context = val
            .get("limit")
            .and_then(|l| l.get("context"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as u32;
        let output = val
            .get("limit")
            .and_then(|l| l.get("output"))
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as u32;
        let reasoning = val
            .get("reasoning")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        let modalities = |kind: &str| {
            val.get("modalities")
                .and_then(|m| m.get(kind))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let input = modalities("input");
        let output_modalities = modalities("output");
        let vision = input.iter().any(|v| v == "image");
        let name = val
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(
            key.to_lowercase(),
            ModelsDevEntry {
                name,
                context,
                output,
                reasoning,
                vision,
                input,
                output_modalities,
                tool_call: val
                    .get("tool_call")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                structured_output: val
                    .get("structured_output")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
        );
    }
    out
}

/// Fetch the models.dev registry, using a 24h disk cache. Returns `None` on
/// any failure (non-fatal — discovery proceeds with curated/live defaults).
pub async fn fetch_models_dev(client: &reqwest::Client) -> Option<HashMap<String, ModelsDevEntry>> {
    // 1. Disk cache (fresh: < 24h).
    if let Some(cached) = read_disk_cache() {
        return Some(cached);
    }
    // 2. Live fetch (10s timeout; non-fatal on failure).
    let resp = client
        .get(MODELS_DEV_URL)
        .timeout(Duration::from_secs(10))
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        // Fall back to stale cache if the network failed mid-TTL.
        return read_disk_cache_stale();
    }
    let raw = resp.text().await.ok()?;
    let data: Value = serde_json::from_str(&raw).ok()?;
    let map = parse_registry(&data);
    if map.is_empty() {
        return read_disk_cache_stale();
    }
    // 3. Write disk cache.
    write_disk_cache(&raw);
    Some(map)
}

/// Map a provider's base_url host to its models.dev provider slug.
fn provider_slug(base_url: &str) -> Option<&'static str> {
    let host = base_host(base_url);
    Some(match host.as_str() {
        "api.deepseek.com" => "deepseek",
        "api.moonshot.ai" | "api.kimi.com" => "moonshotai",
        "api.mistral.ai" => "mistral",
        "api.x.ai" => "xai",
        "api.z.ai" | "open.bigmodel.cn" => "zhipuai",
        "api.minimax.io" | "api.minimaxi.com" => "minimax",
        "api.cohere.ai" => "cohere",
        "integrate.api.nvidia.com" => "nvidia",
        "api.perplexity.ai" => "perplexity",
        "api.cerebras.ai" => "cerebras",
        "api.fireworks.ai" => "fireworks",
        "api.openai.com" => "openai",
        "api.anthropic.com" => "anthropic",
        "portal.qwen.ai" => "alibaba",
        "copilot.tencent.com" => "tencent",
        "api.xiaomimimo.com" | "token-plan-sgp.xiaomimimo.com" => "xiaomi",
        // OpenRouter / Together / Groq / Hyperbolic / SiliconFlow serve
        // third-party models — no single slug; rely on direct or suffix match.
        _ => return None,
    })
}

/// Look up a model in the models.dev registry, trying multiple key forms:
/// 1. Direct match on the full model id (OpenRouter-style `provider/model-id`)
/// 2. Provider-prefixed match (`slug/model-id`) based on the base_url host
/// 3. Suffix match: any key whose model-id part (after `/`) equals the id
pub fn lookup_entry<'a>(
    map: &'a HashMap<String, ModelsDevEntry>,
    id: &str,
    base_url: &str,
) -> Option<&'a ModelsDevEntry> {
    let id_lower = id.to_lowercase();
    // 1. Direct match (handles OpenRouter-style prefixed IDs).
    if let Some(e) = map.get(&id_lower) {
        return Some(e);
    }
    // 2. Provider-prefixed match.
    if let Some(slug) = provider_slug(base_url) {
        let key = format!("{slug}/{id_lower}");
        if let Some(e) = map.get(&key) {
            return Some(e);
        }
        // Also try without version suffixes: some providers append
        // "-latest" or date stamps that models.dev doesn't have.
        if let Some(stripped) = strip_common_suffixes(&id_lower) {
            let key = format!("{slug}/{stripped}");
            if let Some(e) = map.get(&key) {
                return Some(e);
            }
        }
    }
    // 3. Suffix match: find any key whose part after `/` equals the id.
    //    Handles providers like Groq/Together/Fireworks that serve
    //    third-party models under bare IDs.
    if let Some(stripped) = strip_common_suffixes(&id_lower) {
        if let Some(e) = suffix_match(map, &stripped) {
            return Some(e);
        }
    }
    suffix_match(map, &id_lower)
}

/// Match any models.dev key whose model-id part (after `/`) equals `id`.
fn suffix_match<'a>(
    map: &'a HashMap<String, ModelsDevEntry>,
    id: &str,
) -> Option<&'a ModelsDevEntry> {
    // Build a reverse index on first call per lookup? No — just scan.
    // The registry has ~300 entries; a linear scan is fine for a one-time
    // enrichment pass per discovery.
    let needle = id.to_lowercase();
    for (key, entry) in map {
        if let Some(model_part) = key.split('/').nth(1) {
            if model_part == needle {
                return Some(entry);
            }
        }
    }
    None
}

/// Strip common version/date suffixes that providers append to model IDs but
/// models.dev doesn't have (e.g. `mistral-large-latest` → `mistral-large`).
fn strip_common_suffixes(id: &str) -> Option<String> {
    for suffix in ["-latest", ":latest", "-2407", "-2411"] {
        if let Some(stripped) = id.strip_suffix(suffix) {
            return Some(stripped.to_string());
        }
    }
    // Strip date patterns like `-2025-01-01` or `-20250101`.
    if let Some(pos) = id.rfind("-20") {
        let tail = &id[pos + 1..];
        if tail.len() >= 8 && tail.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Some(id[..pos].to_string());
        }
    }
    None
}

/// The generic default from `openai_model_caps`'s else branch.
/// Used to detect models that fell through the curated table.
const GENERIC_DEFAULT_CTX: u32 = 200_000;
const GENERIC_DEFAULT_MAX: u32 = 8_192;

/// Enrich a list of ModelInfos with models.dev capabilities. Overlays context
/// window, max output, reasoning, and vision ONLY for models that the curated
/// table left at generic defaults (detected by context_window == 200_000 AND
/// thinking_levels is empty). Live-API fields (from `apply_live_model_fields`)
/// always take priority since those were already applied.
pub fn enrich_models(
    models: &mut [crate::protocol::ModelInfo],
    map: &HashMap<String, ModelsDevEntry>,
    base_url: &str,
) {
    for m in models.iter_mut() {
        // Only enrich models that fell through to the generic default
        // (curated table missed + no live API context_length override).
        let is_generic = m.context_window == GENERIC_DEFAULT_CTX
            && m.max_tokens == GENERIC_DEFAULT_MAX
            && m.thinking_levels.is_empty();
        if !is_generic {
            // Even for curated models, check if models.dev has vision info
            // that the curated table missed (the curated else-branch sets
            // vision=false as a generic default).
            if m.vision {
                continue; // Already has vision from curated table or live API.
            }
            if let Some(entry) = lookup_entry(map, &m.id, base_url) {
                if entry.vision {
                    m.vision = true;
                }
            }
            continue;
        }
        let Some(entry) = lookup_entry(map, &m.id, base_url) else {
            continue;
        };
        // Overlay from models.dev (authoritative for this model).
        // Skip zeros: an empty/broken registry entry must not pin max_tokens:0
        // or wipe a usable curated/default budget.
        if entry.context > 0 {
            m.context_window = entry.context;
        }
        if entry.output > 0 {
            m.max_tokens = entry.output;
        }
        // models.dev occasionally lists output >= context (or equal). Keep a
        // generation budget strictly below the context window so compaction /
        // request builders always leave prompt room.
        if m.max_tokens == 0 || m.max_tokens >= m.context_window {
            if m.context_window > 1 {
                m.max_tokens = (m.context_window.saturating_mul(3) / 4)
                    .max(1)
                    .min(65_536)
                    .min(m.context_window - 1);
            } else {
                m.max_tokens = 1;
            }
        }
        m.reasoning = entry.reasoning;
        m.vision = entry.vision;
        m.input = entry.input.clone();
        m.output = entry.output_modalities.clone();
        m.tool_call = entry.tool_call;
        m.structured_output = entry.structured_output;
        if entry.reasoning && m.thinking_levels.is_empty() {
            m.thinking_levels = crate::provider::DEFAULT_THINKING_LEVELS
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
    }
}

// --- disk cache ---

fn disk_cache_path() -> Option<std::path::PathBuf> {
    let home = crate::config::home_dir()?;
    Some(home.join(".config/catalyst-code/models-dev-cache.json"))
}

fn read_disk_cache() -> Option<HashMap<String, ModelsDevEntry>> {
    let path = disk_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: DiskCache = serde_json::from_str(&content).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if now.saturating_sub(cache.fetched_at) > MODELS_DEV_CACHE_TTL {
        return None;
    }
    Some(parse_registry(&cache.data))
}

fn read_disk_cache_stale() -> Option<HashMap<String, ModelsDevEntry>> {
    let path = disk_cache_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: DiskCache = serde_json::from_str(&content).ok()?;
    Some(parse_registry(&cache.data))
}

fn write_disk_cache(raw: &str) {
    let path = match disk_cache_path() {
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
    let cache = DiskCache {
        fetched_at: now,
        data: serde_json::from_str(raw).unwrap_or(Value::Null),
    };
    if let Ok(text) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(&path, text);
    }
}

#[derive(Deserialize, serde::Serialize)]
struct DiskCache {
    fetched_at: u64,
    data: Value,
}

fn base_host(base_url: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(context: u32, output: u32, reasoning: bool, vision: bool) -> ModelsDevEntry {
        ModelsDevEntry {
            name: "test".into(),
            context,
            output,
            reasoning,
            vision,
            input: if vision {
                vec!["text".into(), "image".into()]
            } else {
                vec!["text".into()]
            },
            output_modalities: vec!["text".into()],
            tool_call: true,
            structured_output: false,
        }
    }
    #[test]
    fn parse_registry_extracts_caps() {
        let json = serde_json::json!({
            "deepseek/deepseek-chat": {
                "name": "DeepSeek Chat",
                "reasoning": false,
                "attachment": true,
                "tool_call": true,
                "limit": {"context": 1000000, "output": 384000},
                "modalities": {"input": ["text"], "output": ["text"]}
            },
            "openai/gpt-4o": {
                "name": "GPT-4o",
                "reasoning": false,
                "limit": {"context": 128000, "output": 16384},
                "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]}
            }
        });
        let map = parse_registry(&json);
        assert_eq!(map.len(), 2);
        let ds = &map["deepseek/deepseek-chat"];
        assert_eq!(ds.context, 1000000);
        assert_eq!(ds.output, 384000);
        assert!(!ds.reasoning);
        assert!(!ds.vision);
        let gpt4o = &map["openai/gpt-4o"];
        assert_eq!(gpt4o.context, 128000);
        assert_eq!(gpt4o.output, 16384);
        assert!(gpt4o.vision);
    }

    #[test]
    fn lookup_direct_match_works_for_openrouter_style_ids() {
        let mut map = HashMap::new();
        map.insert(
            "openai/gpt-4o".into(),
            make_entry(128000, 16384, false, true),
        );
        let e = lookup_entry(&map, "openai/gpt-4o", "https://openrouter.ai/api/v1");
        assert!(e.is_some());
        assert_eq!(e.unwrap().context, 128000);
    }

    #[test]
    fn lookup_provider_prefixed_match_works_for_bare_ids() {
        let mut map = HashMap::new();
        map.insert(
            "deepseek/deepseek-chat".into(),
            make_entry(1000000, 384000, false, false),
        );
        let e = lookup_entry(&map, "deepseek-chat", "https://api.deepseek.com");
        assert!(e.is_some());
        assert_eq!(e.unwrap().context, 1000000);
    }

    #[test]
    fn lookup_suffix_match_works_for_third_party_gateways() {
        let mut map = HashMap::new();
        map.insert(
            "meta/llama-3.3-70b-instruct".into(),
            make_entry(128000, 4096, false, false),
        );
        // Groq serves this under a bare id.
        let e = lookup_entry(
            &map,
            "llama-3.3-70b-instruct",
            "https://api.groq.com/openai/v1",
        );
        assert!(e.is_some());
        assert_eq!(e.unwrap().context, 128000);
    }

    #[test]
    fn lookup_strips_latest_suffix() {
        let mut map = HashMap::new();
        map.insert(
            "mistral/mistral-large".into(),
            make_entry(128000, 8192, false, false),
        );
        let e = lookup_entry(&map, "mistral-large-latest", "https://api.mistral.ai/v1");
        assert!(e.is_some());
    }

    #[test]
    fn enrich_overlays_generic_defaults() {
        let mut map = HashMap::new();
        map.insert(
            "deepseek/deepseek-chat".into(),
            make_entry(1000000, 384000, false, false),
        );
        let mut models = vec![crate::protocol::ModelInfo {
            id: "deepseek-chat".into(),
            name: "DeepSeek Chat".into(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8_192,
            thinking_levels: Vec::new(),
            vision: false,
            provider: String::new(),
            ..Default::default()
        }];
        enrich_models(&mut models, &map, "https://api.deepseek.com");
        assert_eq!(models[0].context_window, 1000000);
        assert_eq!(models[0].max_tokens, 384000);
        assert!(!models[0].reasoning); // deepseek-chat is not a reasoning model
    }

    #[test]
    fn enrich_preserves_curated_values() {
        // A model that matched the curated table (non-default context + thinking_levels)
        // should NOT have its context/max overwritten by models.dev.
        let mut map = HashMap::new();
        map.insert(
            "openai/gpt-4o".into(),
            make_entry(128000, 16384, false, true),
        );
        let mut models = vec![crate::protocol::ModelInfo {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            reasoning: false,
            context_window: 128000,
            max_tokens: 16384,
            thinking_levels: vec!["low".into()],
            vision: false,
            provider: String::new(),
            ..Default::default()
        }];
        enrich_models(&mut models, &map, "https://api.openai.com/v1");
        // Context and max preserved (not generic default).
        assert_eq!(models[0].context_window, 128000);
        assert_eq!(models[0].max_tokens, 16384);
        // But vision should be overlaid since it was false and models.dev says true.
        assert!(models[0].vision);
    }

    #[test]
    fn enrich_adds_thinking_levels_for_reasoning_models() {
        let mut map = HashMap::new();
        map.insert(
            "deepseek/deepseek-reasoner".into(),
            make_entry(1000000, 384000, true, false),
        );
        let mut models = vec![crate::protocol::ModelInfo {
            id: "deepseek-reasoner".into(),
            name: "DeepSeek Reasoner".into(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8_192,
            thinking_levels: Vec::new(),
            vision: false,
            provider: String::new(),
            ..Default::default()
        }];
        enrich_models(&mut models, &map, "https://api.deepseek.com");
        assert!(models[0].reasoning);
        assert!(!models[0].thinking_levels.is_empty());
    }

    #[test]
    fn enrich_clamps_output_that_meets_or_exceeds_context() {
        let mut map = HashMap::new();
        map.insert(
            "vendor/equal-caps".into(),
            make_entry(1000, 1000, false, false),
        );
        map.insert(
            "vendor/over-caps".into(),
            make_entry(1000, 5000, false, false),
        );
        let mut models = vec![
            crate::protocol::ModelInfo {
                id: "equal-caps".into(),
                name: "Equal".into(),
                reasoning: false,
                context_window: 200_000,
                max_tokens: 8_192,
                thinking_levels: Vec::new(),
                vision: false,
                provider: String::new(),
                ..Default::default()
            },
            crate::protocol::ModelInfo {
                id: "over-caps".into(),
                name: "Over".into(),
                reasoning: false,
                context_window: 200_000,
                max_tokens: 8_192,
                thinking_levels: Vec::new(),
                vision: false,
                provider: String::new(),
                ..Default::default()
            },
        ];
        enrich_models(&mut models, &map, "https://api.example.com/v1");
        assert_eq!(models[0].context_window, 1000);
        assert!(models[0].max_tokens < 1000 && models[0].max_tokens > 0);
        assert_eq!(models[1].context_window, 1000);
        assert!(models[1].max_tokens < 1000 && models[1].max_tokens > 0);
    }

    #[test]
    fn enrich_skips_zero_output_from_registry() {
        let mut map = HashMap::new();
        map.insert("vendor/broken".into(), make_entry(128000, 0, false, false));
        let mut models = vec![crate::protocol::ModelInfo {
            id: "broken".into(),
            name: "Broken".into(),
            reasoning: false,
            context_window: 200_000,
            max_tokens: 8_192,
            thinking_levels: Vec::new(),
            vision: false,
            provider: String::new(),
            ..Default::default()
        }];
        enrich_models(&mut models, &map, "https://api.example.com/v1");
        assert_eq!(models[0].context_window, 128000);
        assert!(models[0].max_tokens > 0);
        assert!(models[0].max_tokens < models[0].context_window);
    }

    #[test]
    fn provider_slug_maps_known_hosts() {
        assert_eq!(provider_slug("https://api.deepseek.com"), Some("deepseek"));
        assert_eq!(
            provider_slug("https://api.moonshot.ai/v1"),
            Some("moonshotai")
        );
        assert_eq!(provider_slug("https://api.mistral.ai/v1"), Some("mistral"));
        assert_eq!(
            provider_slug("https://api.z.ai/api/coding/paas/v4"),
            Some("zhipuai")
        );
        assert_eq!(provider_slug("https://openrouter.ai/api/v1"), None);
        assert_eq!(provider_slug("https://api.groq.com/openai/v1"), None);
    }

    #[test]
    fn strip_suffixes() {
        assert_eq!(
            strip_common_suffixes("mistral-large-latest"),
            Some("mistral-large".into())
        );
        assert_eq!(
            strip_common_suffixes("model-2025-01-01"),
            Some("model".into())
        );
        assert_eq!(
            strip_common_suffixes("model-20250101"),
            Some("model".into())
        );
        assert_eq!(strip_common_suffixes("deepseek-chat"), None);
    }
}
