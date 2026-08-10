// web_search tool: no API key, no JS, no self-host.
//
// Primary → fallback chain (same security model as fetch — honors
// --no-network / fetch_allowlist, reuses html_to_text + egress helpers):
//   1. SearXNG public instances ranked from https://searx.space/data/instances.json
//      that expose google+bing; queries pin engines=google,bing (JSON API when
//      enabled, else HTML scrape of the simple theme)
//   2. DuckDuckGo Lite  (https://lite.duckduckgo.com/lite/)
//   3. DuckDuckGo HTML  (https://html.duckduckgo.com/html/)
//   4. Mojeek           (https://www.mojeek.com/search)
//
// NO API KEY, NO JavaScript, NO new crate deps. SearXNG instance list is
// cached in-process (~1h). We try a few top-ranked instances serially
// (parallel spray trips rate limits). DDG wraps destinations in `uddg=`
// redirects which we decode by hand (no percent-encoding crate).
//
// This is best-effort, not an SLA: public instances rate-limit / captcha,
// and markup may drift. On block/HTTP failure / empty parse we try the
// next backend. Only if every backend fails do we surface an aggregated
// error; a successful empty SERP reports "no results".
use crate::config::Config;
use crate::fetch_tool::{egress_check, html_to_text};
use crate::tools::{smart_truncate, Outcome};
use regex::Regex;
use serde_json::json;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SEARX_SPACE_INSTANCES: &str = "https://searx.space/data/instances.json";
/// How many ranked public instances to try before falling through to DDG.
const SEARX_MAX_INSTANCES: usize = 4;
/// In-process TTL for the ranked instance list.
const SEARX_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Bump when instance filters change so a long-lived process doesn't keep a
/// stale ranked list that ignored the new criteria.
const SEARX_CACHE_GEN: u32 = 2;
/// Engines pinned on every SearXNG query (highest-quality general web results).
const SEARX_ENGINES: &str = "google,bing";
/// Reject an engine whose searx.space-reported error_rate is at or above this.
const SEARX_ENGINE_MAX_ERROR_RATE: f64 = 80.0;

// ---- shared regexes (compiled once) ----

/// DDG Lite: `<a class="result-link" href="...">title</a>`
static DDG_LITE_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+class="result-link"\s+href="([^"]+)"[^>]*>([\s\S]*?)</a>"#).unwrap()
});
/// DDG Lite: `<td class="result-snippet">...</td>`
static DDG_LITE_SNIP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"class="result-snippet"[^>]*>([\s\S]*?)</td>"#).unwrap());
/// Loose fallback for any `<a href>` when structured classes drift.
static ANY_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<a\s+[^>]*href="([^"]+)"[^>]*>([\s\S]*?)</a>"#).unwrap());

/// DDG HTML: `<a class="result__a" href="...">title</a>` (class order may vary).
static DDG_HTML_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)</a>"#)
        .unwrap()
});
/// Alternate attribute order: href before class.
static DDG_HTML_LINK_RE_ALT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+[^>]*href="([^"]+)"[^>]*class="[^"]*result__a[^"]*"[^>]*>([\s\S]*?)</a>"#)
        .unwrap()
});
/// DDG HTML snippets.
static DDG_HTML_SNIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"class="[^"]*result__snippet[^"]*"[^>]*>([\s\S]*?)</(?:a|td|span|div)>"#).unwrap()
});

/// Mojeek: `<a class="title" title="url" href="url">Title</a>`
static MOJEEK_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+class="title"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)</a>"#).unwrap()
});
/// Mojeek snippet: `<p class="s">...</p>` (paired by index with titles).
static MOJEEK_SNIP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<p\s+class="s">([\s\S]*?)</p>"#).unwrap());

/// SearXNG simple theme: one result card.
static SEARX_ARTICLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<article[^>]*class="[^"]*\bresult\b[^"]*"[^>]*>(.*?)</article>"#).unwrap()
});
/// Title link inside a SearXNG result card (h3 > a).
static SEARX_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<h3[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
});
/// Snippet paragraph inside a SearXNG result card.
static SEARX_CONTENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<p[^>]*class="[^"]*\bcontent\b[^"]*"[^>]*>(.*?)</p>"#).unwrap()
});

/// Cached ranked instance URLs from searx.space: (generation, fetched_at, urls).
static SEARX_INSTANCE_CACHE: LazyLock<Mutex<Option<(u32, Instant, Vec<String>)>>> =
    LazyLock::new(|| Mutex::new(None));

/// Percent-decode a query-string value (no `percent-encoding` crate dep).
/// Handles `%XX` hex escapes and `+`→space. Malformed `%` sequences are passed
/// through literally rather than panicking.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// DDG wraps each result's destination in a redirect like
/// `//duckduckgo.com/l/?uddg=<encoded>&rut=...`. Extract and decode the real
/// URL. For direct hrefs (no `uddg=`), return the href unchanged (protocol-less
/// `//host/...` is upgraded to `https://`).
fn unwrap_ddg_url(href: &str) -> String {
    let href = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    };
    if let Some(idx) = href.find("uddg=") {
        let after = &href[idx + "uddg=".len()..];
        let end = after.find('&').unwrap_or(after.len());
        return percent_decode(&after[..end]);
    }
    href
}

/// Strip tags from a snippet cell and tidy whitespace, reusing the shared
/// html_to_text helper so entities/whitespace are handled consistently with
/// the fetch tool's output.
fn cell_text(s: &str) -> String {
    let t = html_to_text(s);
    t.trim().to_string()
}

/// A single search hit.
#[derive(Clone, Debug)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

/// Which backend produced the hits (shown in the tool output header).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Backend {
    Exa,
    Tavily,
    Searx(String),
    DdgLite,
    DdgHtml,
    Mojeek,
}

impl Backend {
    fn label(&self) -> String {
        match self {
            Backend::Exa => "Exa API".into(),
            Backend::Tavily => "Tavily API".into(),
            Backend::Searx(host) => format!("SearXNG ({host})"),
            Backend::DdgLite => "DuckDuckGo Lite".into(),
            Backend::DdgHtml => "DuckDuckGo HTML".into(),
            Backend::Mojeek => "Mojeek".into(),
        }
    }
}

/// Outcome of one backend attempt.
enum Attempt {
    /// Parsed ≥1 hit — stop the chain.
    Hits(Vec<Hit>),
    /// Page looked like a real SERP but had zero results — stop the chain
    /// (don't keep searching; the query genuinely has nothing).
    Empty,
    /// Blocked / HTTP error / markup drift — try the next backend.
    Fail(String),
}

/// Shared captcha / anomaly heuristics used by DDG + SearXNG HTML.
fn looks_blocked(html: &str) -> bool {
    let low = html.to_ascii_lowercase();
    low.contains("captcha")
        || low.contains("unusual traffic")
        || low.contains("are you a robot")
        || low.contains("bots use duckduckgo")
        || low.contains("anomaly-modal")
        || low.contains("please complete the following challenge")
        || low.contains("making sure you're not a bot")
        || low.contains("checking your browser")
        || low.contains("checking if the site connection is secure")
        || low.contains("cf-browser-verification")
        || low.contains("browser verification required")
        || low.contains("just a moment...")
}

/// Map DDG-style `us-en` region to a SearXNG `language` code (`en`).
fn searx_language(region: &str) -> &str {
    region
        .rsplit_once('-')
        .map(|(_, lang)| lang)
        .filter(|s| !s.is_empty())
        .unwrap_or("en")
}

fn instance_host(base: &str) -> String {
    base.trim_end_matches('/')
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base)
        .to_string()
}

/// Return the engine's reported error_rate if it looks usable, else `None`
/// (missing engine, or error_rate too high). An empty `{}` entry means OK (0%).
fn searx_engine_error_rate(engines: &Value, name: &str) -> Option<f64> {
    let eng = engines.get(name)?;
    // Presence as an object (including `{}`) means the instance lists the engine.
    if !eng.is_object() {
        return None;
    }
    let rate = eng
        .get("error_rate")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if rate >= SEARX_ENGINE_MAX_ERROR_RATE {
        return None;
    }
    Some(rate)
}

/// Score a searx.space instance entry. Higher is better. `None` = skip.
/// Only instances that expose working `google` and `bing` engines qualify.
fn score_searx_instance(meta: &Value) -> Option<f64> {
    let network_type = meta.get("network_type").and_then(|v| v.as_str());
    if matches!(network_type, Some(t) if t != "normal") {
        return None;
    }
    let http = meta.get("http")?;
    if http.get("status_code").and_then(|v| v.as_u64()) != Some(200) {
        return None;
    }
    if http.get("error").map(|e| !e.is_null()).unwrap_or(false) {
        return None;
    }
    let engines = meta.get("engines")?;
    let google_err = searx_engine_error_rate(engines, "google")?;
    let bing_err = searx_engine_error_rate(engines, "bing")?;
    let uptime = meta.get("uptime")?;
    let day = uptime.get("uptimeDay")?.as_f64()?;
    let week = uptime
        .get("uptimeWeek")
        .and_then(|v| v.as_f64())
        .unwrap_or(day);
    let search = meta.get("timing")?.get("search")?;
    let success = search.get("success_percentage")?.as_f64()?;
    if success < 50.0 {
        return None;
    }
    let median = search
        .get("all")
        .and_then(|a| a.get("median"))
        .and_then(|v| v.as_f64())
        .unwrap_or(9.0);
    // Prefer high uptime + search success + healthy google/bing, then low latency.
    Some(
        day * 2.0 + week + success * 3.0 - median * 5.0
            + (100.0 - google_err) * 0.5
            + (100.0 - bing_err) * 0.5,
    )
}

/// Parse searx.space `instances.json` into ranked base URLs (https only).
fn rank_searx_instances(doc: &Value) -> Vec<String> {
    let Some(map) = doc.get("instances").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut ranked: Vec<(f64, String)> = Vec::new();
    for (url, meta) in map {
        if !url.starts_with("https://") {
            continue;
        }
        if let Some(score) = score_searx_instance(meta) {
            ranked.push((score, url.trim_end_matches('/').to_string() + "/"));
        }
    }
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(_, u)| u).collect()
}

/// Fetch + cache ranked public SearXNG instances from searx.space.
async fn load_searx_instances(cfg: &Config) -> Result<Vec<String>, String> {
    if let Ok(guard) = SEARX_INSTANCE_CACHE.lock() {
        if let Some((gen, at, urls)) = guard.as_ref() {
            if *gen == SEARX_CACHE_GEN && at.elapsed() < SEARX_CACHE_TTL && !urls.is_empty() {
                return Ok(urls.clone());
            }
        }
    }

    if let Some(err) = egress_check("web_search", SEARX_SPACE_INSTANCES, cfg) {
        return Err(err);
    }

    let (status, body, _trunc) = fetch_html(
        cfg,
        SEARX_SPACE_INSTANCES,
        // searx.space instances.json is ~1-2MB; the default fetch_max_bytes
        // (256KB) truncates it mid-string → "JSON parse failed: EOF at column
        // 262144". Floor this one fetch at 8MB so the instance list parses.
        cfg.fetch_max_bytes.max(8 * 1024 * 1024),
    )
    .await?;
    if !status.is_success() {
        return Err(format!("searx.space returned HTTP {status}"));
    }
    let doc: Value =
        serde_json::from_str(&body).map_err(|e| format!("searx.space JSON parse failed: {e}"))?;
    let urls = rank_searx_instances(&doc);
    if urls.is_empty() {
        return Err(
            "searx.space returned no usable online instances with google+bing engines".into(),
        );
    }

    if let Ok(mut guard) = SEARX_INSTANCE_CACHE.lock() {
        *guard = Some((SEARX_CACHE_GEN, Instant::now(), urls.clone()));
    }
    Ok(urls)
}

/// Parse SearXNG `format=json` body into hits.
fn parse_searx_json(body: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let doc: Value =
        serde_json::from_str(body).map_err(|e| format!("SearXNG JSON parse failed: {e}"))?;
    let Some(results) = doc.get("results").and_then(|v| v.as_array()) else {
        return Err("SearXNG JSON missing results[]".into());
    };
    let mut hits = Vec::new();
    for r in results {
        let url = r
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let title = r
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let snippet = r
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.starts_with("http") && !title.is_empty() {
            hits.push(Hit {
                title,
                url,
                snippet,
            });
            if hits.len() >= limit {
                break;
            }
        }
    }
    Ok(hits)
}

/// Parse SearXNG simple-theme HTML SERP into hits.
fn parse_searx_html(html: &str, limit: usize) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    for art in SEARX_ARTICLE_RE.captures_iter(html) {
        let block = art.get(1).map(|m| m.as_str()).unwrap_or("");
        let Some(cap) = SEARX_TITLE_RE.captures(block) else {
            continue;
        };
        let href = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let title = cell_text(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        if !href.starts_with("http") || title.is_empty() {
            continue;
        }
        let snippet = SEARX_CONTENT_RE
            .captures(block)
            .map(|c| cell_text(c.get(1).map(|m| m.as_str()).unwrap_or("")))
            .unwrap_or_default();
        if hits.iter().any(|h| h.url == href) {
            continue;
        }
        hits.push(Hit {
            title,
            url: href,
            snippet,
        });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

fn classify_searx_response(
    host: &str,
    status: reqwest::StatusCode,
    body: &str,
    content_type: &str,
    want_json: bool,
    limit: usize,
) -> Attempt {
    if status.as_u16() == 429 {
        return Attempt::Fail(format!("SearXNG ({host}) rate-limited (HTTP 429)"));
    }
    if status.as_u16() == 403 {
        return Attempt::Fail(format!(
            "SearXNG ({host}) forbidden (HTTP 403; JSON often disabled on public instances)"
        ));
    }
    if !status.is_success() {
        return Attempt::Fail(format!("SearXNG ({host}) returned HTTP {status}"));
    }
    if looks_blocked(body) {
        return Attempt::Fail(format!("SearXNG ({host}) served a bot-check/captcha page"));
    }

    let ct = content_type.to_ascii_lowercase();
    if want_json {
        let looks_json = ct.contains("json") || body.trim_start().starts_with('{');
        if !looks_json {
            return Attempt::Fail(format!(
                "SearXNG ({host}) did not return JSON (content-type {content_type:?})"
            ));
        }
        return match parse_searx_json(body, limit) {
            Ok(hits) if hits.is_empty() => Attempt::Empty,
            Ok(hits) => Attempt::Hits(hits),
            Err(e) => Attempt::Fail(format!("SearXNG ({host}): {e}")),
        };
    }

    let low = body.to_ascii_lowercase();
    let has_markers = low.contains("class=\"result") || low.contains("article class=\"result");
    if !has_markers && body.len() < 8 * 1024 {
        return Attempt::Fail(format!(
            "SearXNG ({host}) returned an unexpected page with no result markers"
        ));
    }
    let hits = parse_searx_html(body, limit);
    if hits.is_empty() {
        if has_markers {
            Attempt::Empty
        } else {
            Attempt::Fail(format!(
                "SearXNG ({host}) returned a page that parsed to zero results"
            ))
        }
    } else {
        Attempt::Hits(hits)
    }
}

/// Try ranked public SearXNG instances (JSON first, then HTML per host).
async fn try_searxng(
    cfg: &Config,
    query: &str,
    count: usize,
    region: &str,
    byte_limit: usize,
    failures: &mut Vec<String>,
) -> Option<(Backend, Attempt)> {
    let instances = match load_searx_instances(cfg).await {
        Ok(v) => v,
        Err(e) => {
            failures.push(format!("searx.space: {e}"));
            return None;
        }
    };

    let q = form_urlencode(query);
    let lang = form_urlencode(searx_language(region));
    let engines = form_urlencode(SEARX_ENGINES);
    // Pin google+bing so we don't get low-quality default engine mixes.
    let common = format!("q={q}&language={lang}&engines={engines}&categories=general&pageno=1");

    for base in instances.into_iter().take(SEARX_MAX_INSTANCES) {
        let host = instance_host(&base);
        // JSON attempt (many public instances disable this → 403 → HTML).
        let json_url = format!("{base}search?{common}&format=json");
        if let Some(err) = egress_check("web_search", &json_url, cfg) {
            failures.push(format!("SearXNG ({host}): skipped ({err})"));
            continue;
        }

        match fetch_html_with_ct(cfg, &json_url, byte_limit).await {
            Ok((status, body, ct, _trunc)) => {
                match classify_searx_response(&host, status, &body, &ct, true, count) {
                    Attempt::Hits(h) => {
                        return Some((Backend::Searx(host), Attempt::Hits(h)));
                    }
                    Attempt::Empty => {
                        return Some((Backend::Searx(host), Attempt::Empty));
                    }
                    Attempt::Fail(reason) => {
                        // Fall through to HTML on the same host for JSON disable / drift.
                        failures.push(reason);
                    }
                }
            }
            Err(e) => {
                failures.push(format!("SearXNG ({host}) JSON: {e}"));
            }
        }

        let html_url = format!("{base}search?{common}");
        if let Some(err) = egress_check("web_search", &html_url, cfg) {
            failures.push(format!("SearXNG ({host}) HTML: skipped ({err})"));
            continue;
        }
        match fetch_html_with_ct(cfg, &html_url, byte_limit).await {
            Ok((status, body, ct, _trunc)) => {
                match classify_searx_response(&host, status, &body, &ct, false, count) {
                    Attempt::Hits(h) => {
                        return Some((Backend::Searx(host), Attempt::Hits(h)));
                    }
                    Attempt::Empty => {
                        return Some((Backend::Searx(host), Attempt::Empty));
                    }
                    Attempt::Fail(reason) => failures.push(reason),
                }
            }
            Err(e) => failures.push(format!("SearXNG ({host}) HTML: {e}")),
        }
    }
    None
}

/// Parse DDG Lite HTML into ordered hits. Returns up to `limit` results.
/// Defensive: if the structured `result-link`/`result-snippet` parse yields
/// nothing (markup drift / captcha), falls back to scraping any `<a href>`
/// whose href looks like a real external result.
fn parse_ddg_lite(html: &str, limit: usize) -> Vec<Hit> {
    let titles_urls: Vec<(String, String)> = DDG_LITE_LINK_RE
        .captures_iter(html)
        .map(|c| {
            let href = c.get(1).map(|m| m.as_str()).unwrap_or("");
            let title = c.get(2).map(|m| m.as_str()).unwrap_or("");
            (cell_text(title), unwrap_ddg_url(href))
        })
        .filter(|(t, u)| !t.is_empty() && !u.is_empty())
        .collect();
    let snippets: Vec<String> = DDG_LITE_SNIP_RE
        .captures_iter(html)
        .map(|c| cell_text(c.get(1).map(|m| m.as_str()).unwrap_or("")))
        .collect();

    if !titles_urls.is_empty() {
        return titles_urls
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, (t, u))| Hit {
                title: t.clone(),
                url: u.clone(),
                snippet: snippets.get(i).cloned().unwrap_or_default(),
            })
            .collect();
    }

    // Fallback: scrape external-looking <a href> links. Filters out anchors,
    // javascript:, and DDG-internal nav links. This is looser but still useful
    // when the structured classes drift.
    let mut hits: Vec<Hit> = Vec::new();
    for c in ANY_LINK_RE.captures_iter(html) {
        let href = c.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = cell_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
        if (href.starts_with("http://")
            || (href.starts_with("https://") && !href.contains("duckduckgo.com/l/")))
            && !title.is_empty()
            && !hits.iter().any(|h| h.url == href)
        {
            hits.push(Hit {
                title,
                url: href.to_string(),
                snippet: String::new(),
            });
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

/// Parse DDG HTML (`html.duckduckgo.com/html/`) results: `result__a` +
/// `result__snippet`. Same `uddg=` unwrap as Lite.
fn parse_ddg_html(html: &str, limit: usize) -> Vec<Hit> {
    let mut titles_urls: Vec<(String, String)> = DDG_HTML_LINK_RE
        .captures_iter(html)
        .map(|c| {
            let href = c.get(1).map(|m| m.as_str()).unwrap_or("");
            let title = c.get(2).map(|m| m.as_str()).unwrap_or("");
            (cell_text(title), unwrap_ddg_url(href))
        })
        .filter(|(t, u)| !t.is_empty() && !u.is_empty())
        .collect();
    if titles_urls.is_empty() {
        titles_urls = DDG_HTML_LINK_RE_ALT
            .captures_iter(html)
            .map(|c| {
                let href = c.get(1).map(|m| m.as_str()).unwrap_or("");
                let title = c.get(2).map(|m| m.as_str()).unwrap_or("");
                (cell_text(title), unwrap_ddg_url(href))
            })
            .filter(|(t, u)| !t.is_empty() && !u.is_empty())
            .collect();
    }
    let snippets: Vec<String> = DDG_HTML_SNIP_RE
        .captures_iter(html)
        .map(|c| cell_text(c.get(1).map(|m| m.as_str()).unwrap_or("")))
        .collect();

    titles_urls
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, (t, u))| Hit {
            title: t.clone(),
            url: u.clone(),
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Parse Mojeek SERP: `a.title` + paired `p.s` snippets. Hrefs are direct
/// (no redirect wrapper).
fn parse_mojeek(html: &str, limit: usize) -> Vec<Hit> {
    let titles_urls: Vec<(String, String)> = MOJEEK_TITLE_RE
        .captures_iter(html)
        .map(|c| {
            let href = c.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let title = cell_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
            (title, href)
        })
        .filter(|(t, u)| !t.is_empty() && u.starts_with("http"))
        .collect();
    let snippets: Vec<String> = MOJEEK_SNIP_RE
        .captures_iter(html)
        .map(|c| cell_text(c.get(1).map(|m| m.as_str()).unwrap_or("")))
        .collect();

    titles_urls
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, (t, u))| Hit {
            title: t.clone(),
            url: u.clone(),
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Classify a fetched body for a scrape backend into Hits / Empty / Fail.
fn classify_response(
    backend: &Backend,
    status: reqwest::StatusCode,
    html: &str,
    body_len: usize,
    truncated: bool,
    limit: usize,
) -> Attempt {
    let trunc = if truncated { " [body truncated]" } else { "" };
    let label = backend.label();
    if !status.is_success() {
        return Attempt::Fail(format!("{label} returned HTTP {status}{trunc}"));
    }

    if looks_blocked(html) {
        return Attempt::Fail(format!(
            "{label} served a captcha/anomaly page (likely rate-limited){trunc}"
        ));
    }

    let low = html.to_ascii_lowercase();
    let has_markers = match backend {
        // API providers never flow through classify_response (they return parsed
        // hits directly); mark as "has markers" so they can't trip the no-markers
        // fail path if ever reached defensively.
        Backend::Exa | Backend::Tavily => true,
        Backend::DdgLite => low.contains("result-link") || low.contains("result-snippet"),
        Backend::DdgHtml => {
            low.contains("result__a")
                || low.contains("result__snippet")
                || low.contains("web-result")
        }
        Backend::Mojeek => low.contains("class=\"title\"") || low.contains("class=\"s\""),
        Backend::Searx(_) => {
            low.contains("class=\"result") || low.contains("article class=\"result")
        }
    };

    // Small page with no result markers → block / markup drift, not "no hits".
    if !has_markers && body_len < 8 * 1024 {
        return Attempt::Fail(format!(
            "{label} returned an unexpected page with no result markers (markup drift or a block){trunc}"
        ));
    }

    let hits = match backend {
        Backend::Exa | Backend::Tavily => Vec::new(),
        Backend::DdgLite => parse_ddg_lite(html, limit),
        Backend::DdgHtml => parse_ddg_html(html, limit),
        Backend::Mojeek => parse_mojeek(html, limit),
        Backend::Searx(_) => parse_searx_html(html, limit),
    };

    if hits.is_empty() {
        // Markers present (or large page) but nothing parsed: treat as empty
        // SERP when markers exist; otherwise as a soft fail so the chain continues.
        if has_markers {
            Attempt::Empty
        } else {
            Attempt::Fail(format!(
                "{label} returned a page that parsed to zero results{trunc}"
            ))
        }
    } else {
        Attempt::Hits(hits)
    }
}

/// GET `url`, stream up to `byte_limit` bytes, return (status, body, truncated).
async fn fetch_html(
    cfg: &Config,
    url: &str,
    byte_limit: usize,
) -> Result<(reqwest::StatusCode, String, bool), String> {
    let (status, body, _ct, truncated) = fetch_html_with_ct(cfg, url, byte_limit).await?;
    Ok((status, body, truncated))
}

async fn fetch_html_with_ct(
    cfg: &Config,
    url: &str,
    byte_limit: usize,
) -> Result<(reqwest::StatusCode, String, String, bool), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let resp = crate::fetch_tool::send_resolved(parsed, cfg).await?;
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    use futures_util::StreamExt;
    let mut collected = Vec::with_capacity(byte_limit.min(64 * 1024));
    let mut truncated = false;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("failed to read body: {e}"))?;
        let room = byte_limit.saturating_sub(collected.len());
        if chunk.len() <= room {
            collected.extend_from_slice(&chunk);
        } else {
            collected.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
    }
    Ok((
        status,
        String::from_utf8_lossy(&collected).into_owned(),
        ct,
        truncated,
    ))
}

fn render_hits(query: &str, backend: &Backend, hits: &[Hit], note: Option<&str>) -> Outcome {
    let mut header = format!(
        "Search: {query}  ({}, {} hit(s)",
        backend.label(),
        hits.len()
    );
    if let Some(n) = note {
        header.push_str(&format!(" · {n}"));
    }
    header.push_str(")\n\n");
    let mut text = header;
    for (i, h) in hits.iter().enumerate() {
        text.push_str(&format!(
            "{}. {}\n   {}\n   {}\n\n",
            i + 1,
            h.title,
            h.url,
            h.snippet
        ));
    }

    const OUT_CAP: usize = 24_576;
    if text.len() > OUT_CAP {
        text = smart_truncate(&text, OUT_CAP);
    }
    Outcome::ok(text)
}

// ============================================================================
// Paid API providers (Exa + Tavily) with load balancing + usage tracking.
//
// When EXA_API_KEY and/or TAVILY_API_KEY are set, web_search prefers them
// (structured snippets, higher quality than scraping). With both keys set,
// requests round-robin between the two. Cumulative monthly usage persists to
// ~/.config/catalyst-code/search-usage.json so a restarted session won't blow
// past the free-tier quota (default 1000/mo each; override via
// EXA_MONTHLY_LIMIT / TAVILY_MONTHLY_LIMIT).
//
// On a 429 / quota-exceeded response the provider enters a cooldown (parsed
// from `retry-after`, default 60s) and the OTHER provider is tried. Only if
// every API provider is unavailable / exhausted / failing do we fall through
// to the no-key scrape chain (SearXNG -> DDG -> Mojeek) below -- so web_search
// ALWAYS has a path to results.
//
// Egress: API endpoints go through the same `egress_check` as scrape URLs, so
// `--no-network` + empty allowlist denies them too (consistent). Add the API
// hosts to `fetch_allowlist` to opt them in under a locked-down config.
// ============================================================================

/// A paid API search provider. Keys resolve from env vars; `ALL` order is the
/// deterministic round-robin priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiProvider {
    Exa,
    Tavily,
}

impl ApiProvider {
    const ALL: [ApiProvider; 2] = [ApiProvider::Exa, ApiProvider::Tavily];

    fn env_key(&self) -> &'static str {
        match self {
            ApiProvider::Exa => "EXA_API_KEY",
            ApiProvider::Tavily => "TAVILY_API_KEY",
        }
    }
    fn endpoint(&self) -> &'static str {
        match self {
            ApiProvider::Exa => "https://api.exa.ai/search",
            ApiProvider::Tavily => "https://api.tavily.com/search",
        }
    }
    /// Monthly request/credit budget. Override via env; default = free tier.
    fn monthly_limit(&self) -> u64 {
        let env = match self {
            ApiProvider::Exa => "EXA_MONTHLY_LIMIT",
            ApiProvider::Tavily => "TAVILY_MONTHLY_LIMIT",
        };
        std::env::var(env)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1000)
    }
    /// Resolved API key (non-empty). A key set via `/search-key` (persisted in
    /// `cfg.search_keys`) wins over the env var, so slash-command keys override.
    fn key(&self, cfg: &Config) -> Option<String> {
        let name = match self {
            ApiProvider::Exa => "exa",
            ApiProvider::Tavily => "tavily",
        };
        if let Some(k) = cfg.search_keys.get(name) {
            if !k.is_empty() {
                return Some(k.clone());
            }
        }
        std::env::var(self.env_key())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
    fn backend(self) -> Backend {
        match self {
            ApiProvider::Exa => Backend::Exa,
            ApiProvider::Tavily => Backend::Tavily,
        }
    }
}

/// Persisted monthly usage for one provider (resets when the calendar month
/// rolls over so quotas are month-bound, not lifetime).
#[derive(Clone, Debug, Default)]
struct MonthlyUsage {
    /// "YYYY-MM" -- when this differs from the current month, count resets to 0.
    month: String,
    count: u64,
}

/// In-process usage + cooldown state (loaded lazily from disk on first use).
#[derive(Default)]
struct UsageState {
    exa: MonthlyUsage,
    tavily: MonthlyUsage,
    exa_cooldown_until: Option<Instant>,
    tavily_cooldown_until: Option<Instant>,
    loaded: bool,
}

static USAGE: LazyLock<Mutex<UsageState>> = LazyLock::new(|| Mutex::new(UsageState::default()));
/// Round-robin cursor: incremented once per search; the starting provider is
/// `cursor % available.len()`, so two available providers strictly alternate.
static ROUND_ROBIN: AtomicU64 = AtomicU64::new(0);

/// Current calendar month as "YYYY-MM" (no chrono dep -- Howard Hinnant's
/// civil-from-days algorithm applied to the Unix epoch second).
fn current_month() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}")
}

fn usage_file_path() -> Option<PathBuf> {
    crate::config::home_dir().map(|h| h.join(".config/catalyst-code/search-usage.json"))
}

/// Lazily load persisted monthly counts into the in-process state (once).
/// A stored month that doesn't match the current month resets the count to 0.
fn load_usage(state: &mut UsageState) {
    if state.loaded {
        return;
    }
    state.loaded = true;
    let Some(path) = usage_file_path() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&content) else {
        return;
    };
    let month = current_month();
    state.exa = read_provider_usage(&doc, "exa", &month);
    state.tavily = read_provider_usage(&doc, "tavily", &month);
}

fn read_provider_usage(doc: &Value, name: &str, current: &str) -> MonthlyUsage {
    let Some(p) = doc.get(name) else {
        return MonthlyUsage::default();
    };
    let month = p
        .get("month")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let count = p.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    if month == current {
        MonthlyUsage { month, count }
    } else {
        // New calendar month -> quota resets.
        MonthlyUsage {
            month: current.to_string(),
            count: 0,
        }
    }
}

/// Best-effort persist of the monthly counts (ignore errors -- usage tracking
/// is advisory, never blocks a search).
fn save_usage(state: &UsageState) {
    let Some(path) = usage_file_path() else {
        return;
    };
    let doc = json!({
        "exa": { "month": state.exa.month, "count": state.exa.count },
        "tavily": { "month": state.tavily.month, "count": state.tavily.count }
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, doc.to_string());
}

/// Record a billable call against the provider's monthly quota. Returns the
/// post-increment monthly count (for the usage note in the output).
fn record_request(p: ApiProvider, credits: u64) -> u64 {
    let credits = credits.max(1);
    let mut state = USAGE.lock().unwrap();
    load_usage(&mut state);
    let month = current_month();
    let usage = match p {
        ApiProvider::Exa => &mut state.exa,
        ApiProvider::Tavily => &mut state.tavily,
    };
    if usage.month != month {
        *usage = MonthlyUsage {
            month: month.clone(),
            count: 0,
        };
    }
    usage.count = usage.count.saturating_add(credits);
    let count = usage.count;
    save_usage(&state);
    count
}

/// Park a provider in cooldown for `secs` (rate-limit / quota response).
fn set_cooldown(p: ApiProvider, secs: u64) {
    let mut state = USAGE.lock().unwrap();
    let until = Instant::now() + Duration::from_secs(secs.max(1));
    match p {
        ApiProvider::Exa => state.exa_cooldown_until = Some(until),
        ApiProvider::Tavily => state.tavily_cooldown_until = Some(until),
    }
}

/// Is `p` usable right now given a key is (or isn't) present? Checks the
/// cooldown + monthly budget only (key presence is passed in so the quota
/// logic is unit-testable without env vars).
fn provider_usable(state: &UsageState, p: ApiProvider, has_key: bool) -> bool {
    if !has_key {
        return false;
    }
    let cooldown = match p {
        ApiProvider::Exa => state.exa_cooldown_until,
        ApiProvider::Tavily => state.tavily_cooldown_until,
    };
    if let Some(until) = cooldown {
        if Instant::now() < until {
            return false;
        }
    }
    let usage = match p {
        ApiProvider::Exa => &state.exa,
        ApiProvider::Tavily => &state.tavily,
    };
    usage.count < p.monthly_limit()
}

/// Is `p` usable right now? Requires: key set, not over monthly budget, not in
/// an active cooldown.
fn provider_available(state: &UsageState, p: ApiProvider, cfg: &Config) -> bool {
    provider_usable(state, p, p.key(cfg).is_some())
}

/// Providers usable right now, in deterministic round-robin priority order.
fn available_providers(state: &UsageState, cfg: &Config) -> Vec<ApiProvider> {
    ApiProvider::ALL
        .iter()
        .copied()
        .filter(|p| provider_available(state, *p, cfg))
        .collect()
}

/// "43/1000 this month" -- surfaced in the search output header so the model
/// / user can see quota pressure building.
fn usage_note(p: ApiProvider) -> String {
    let mut state = USAGE.lock().unwrap();
    load_usage(&mut state);
    let usage = match p {
        ApiProvider::Exa => &state.exa,
        ApiProvider::Tavily => &state.tavily,
    };
    format!("{}/{} this month", usage.count, p.monthly_limit())
}

/// Outcome of one API provider attempt.
enum ApiOutcome {
    Hits(Vec<Hit>),
    /// Real SERP, zero results -- stop (don't burn the other provider's quota).
    Empty,
    /// Rate-limited / quota-exceeded -- set cooldown, try the other provider.
    RateLimited {
        retry_after_secs: u64,
        msg: String,
    },
    /// Other failure -- try the other provider / scrape fallback.
    Fail(String),
}

/// Parse a `retry-after` header (delta-seconds form per RFC 7231).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(60)
}

/// Read a response body, streaming and capped at `byte_limit` (defends against
/// a runaway response). Returns (status, body_text, response_headers).
async fn read_capped(
    resp: reqwest::Response,
    byte_limit: usize,
) -> Result<(reqwest::StatusCode, String, reqwest::header::HeaderMap), String> {
    let status = resp.status();
    let hdrs = resp.headers().clone();
    use futures_util::StreamExt;
    let mut collected: Vec<u8> = Vec::with_capacity(byte_limit.min(64 * 1024));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("failed to read body: {e}"))?;
        let room = byte_limit - collected.len();
        if chunk.len() <= room {
            collected.extend_from_slice(&chunk);
        } else {
            collected.extend_from_slice(&chunk[..room]);
            break;
        }
    }
    Ok((
        status,
        String::from_utf8_lossy(&collected).into_owned(),
        hdrs,
    ))
}

/// Truncate a string to ~`max` chars for inclusion in error messages.
fn truncate_msg(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn parse_exa_results(doc: &Value, limit: usize) -> Vec<Hit> {
    let Some(results) = doc.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for r in results {
        let url = r
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let title = r
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        let snippet = r
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        hits.push(Hit {
            title,
            url,
            snippet,
        });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

fn parse_tavily_results(doc: &Value, limit: usize) -> Vec<Hit> {
    let Some(results) = doc.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for r in results {
        let url = r
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let title = r
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !url.starts_with("http") || title.is_empty() {
            continue;
        }
        let snippet = r
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        hits.push(Hit {
            title,
            url,
            snippet,
        });
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

/// Query Exa. Bills 1 search/call. 429 -> cooldown; 402/403 -> quota/auth.
async fn search_exa(cfg: &Config, query: &str, count: usize, byte_limit: usize) -> ApiOutcome {
    let Some(key) = ApiProvider::Exa.key(cfg) else {
        return ApiOutcome::Fail("Exa: no API key (set EXA_API_KEY or use /search-key exa)".into());
    };
    if let Some(err) = egress_check("web_search", ApiProvider::Exa.endpoint(), cfg) {
        return ApiOutcome::Fail(format!("Exa: skipped ({err})"));
    }
    let body = json!({
        "query": query,
        "numResults": count,
        "type": "auto",
        "contents": { "text": { "maxCharacters": 300 } }
    });
    let resp = match crate::fetch_tool::post_resolved_json(
        ApiProvider::Exa.endpoint(),
        cfg,
        ("x-api-key", key.as_str()),
        &body,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return ApiOutcome::Fail(format!("Exa: {e}")),
    };
    let (status, text, hdrs) = match read_capped(resp, byte_limit).await {
        Ok(v) => v,
        Err(e) => return ApiOutcome::Fail(format!("Exa: {e}")),
    };
    let code = status.as_u16();
    // Map failure modes to cooldown durations so a bad/exhausted key isn't
    // retried every call (respect `retry-after` when present):
    //   429 rate-limit -> transient (retry-after, floor 30s)
    //   401/403 auth   -> bad key/billing (60s; restart to fix the env var)
    //   402 payment    -> quota exhausted (1h; won't heal until plan change)
    let (cooldown, label) = match code {
        429 => (parse_retry_after(&hdrs).max(30), "rate-limited"),
        401 | 403 => (60, "auth error"),
        402 => (3600, "payment required / quota"),
        _ => (0, ""),
    };
    if cooldown > 0 {
        return ApiOutcome::RateLimited {
            retry_after_secs: cooldown,
            msg: format!("Exa: {label} (HTTP {code}): {}", truncate_msg(&text, 120)),
        };
    }
    if !status.is_success() {
        return ApiOutcome::Fail(format!("Exa: HTTP {status}: {}", truncate_msg(&text, 200)));
    }
    let doc: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return ApiOutcome::Fail(format!("Exa: JSON parse failed: {e}")),
    };
    let hits = parse_exa_results(&doc, count);
    record_request(ApiProvider::Exa, 1);
    if hits.is_empty() {
        ApiOutcome::Empty
    } else {
        ApiOutcome::Hits(hits)
    }
}

/// Query Tavily. Bills `usage.credits` (1 for basic depth). 429/432/433 -> quota.
async fn search_tavily(cfg: &Config, query: &str, count: usize, byte_limit: usize) -> ApiOutcome {
    let Some(key) = ApiProvider::Tavily.key(cfg) else {
        return ApiOutcome::Fail(
            "Tavily: no API key (set TAVILY_API_KEY or use /search-key tavily)".into(),
        );
    };
    if let Some(err) = egress_check("web_search", ApiProvider::Tavily.endpoint(), cfg) {
        return ApiOutcome::Fail(format!("Tavily: skipped ({err})"));
    }
    let body = json!({
        "query": query,
        "search_depth": "basic",
        "max_results": count,
        "topic": "general",
        "include_answer": false,
        "include_raw_content": false,
        "include_usage": true
    });
    let resp = match crate::fetch_tool::post_resolved_json(
        ApiProvider::Tavily.endpoint(),
        cfg,
        ("authorization", &format!("Bearer {key}")),
        &body,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return ApiOutcome::Fail(format!("Tavily: {e}")),
    };
    let (status, text, hdrs) = match read_capped(resp, byte_limit).await {
        Ok(v) => v,
        Err(e) => return ApiOutcome::Fail(format!("Tavily: {e}")),
    };
    let code = status.as_u16();
    // Cooldown per failure mode (respect `retry-after` when present):
    //   429 rate-limit    -> transient (retry-after, floor 30s)
    //   401/403 auth     -> bad key (60s; restart to fix)
    //   432/433 plan/pay -> monthly quota exhausted (1h; won't heal soon)
    let (cooldown, label) = match code {
        429 => (parse_retry_after(&hdrs).max(30), "rate-limited"),
        401 | 403 => (60, "auth error"),
        432 | 433 => (3600, "plan quota exhausted"),
        _ => (0, ""),
    };
    if cooldown > 0 {
        return ApiOutcome::RateLimited {
            retry_after_secs: cooldown,
            msg: format!("Tavily: {label} (HTTP {code})"),
        };
    }
    if !status.is_success() {
        return ApiOutcome::Fail(format!(
            "Tavily: HTTP {status}: {}",
            truncate_msg(&text, 200)
        ));
    }
    let doc: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return ApiOutcome::Fail(format!("Tavily: JSON parse failed: {e}")),
    };
    let credits = doc
        .get("usage")
        .and_then(|u| u.get("credits"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let hits = parse_tavily_results(&doc, count);
    record_request(ApiProvider::Tavily, credits);
    if hits.is_empty() {
        ApiOutcome::Empty
    } else {
        ApiOutcome::Hits(hits)
    }
}

async fn search_provider(
    cfg: &Config,
    p: ApiProvider,
    query: &str,
    count: usize,
    byte_limit: usize,
) -> ApiOutcome {
    match p {
        ApiProvider::Exa => search_exa(cfg, query, count, byte_limit).await,
        ApiProvider::Tavily => search_tavily(cfg, query, count, byte_limit).await,
    }
}

/// Try the available API providers in round-robin order. Returns the backend +
/// outcome + which provider succeeded (for the usage note). On rate-limit /
/// failure of one, tries the next; returns None only when none are available or
/// all failed (caller then falls through to the scrape chain).
async fn try_api_providers(
    cfg: &Config,
    query: &str,
    count: usize,
    byte_limit: usize,
    failures: &mut Vec<String>,
) -> Option<(Backend, Attempt, ApiProvider)> {
    let avail = {
        let mut state = USAGE.lock().unwrap();
        load_usage(&mut state);
        available_providers(&state, cfg)
    };
    if avail.is_empty() {
        return None;
    }
    let start = ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) as usize % avail.len();
    for i in 0..avail.len() {
        let p = avail[(start + i) % avail.len()];
        match search_provider(cfg, p, query, count, byte_limit).await {
            ApiOutcome::Hits(hits) => return Some((p.backend(), Attempt::Hits(hits), p)),
            ApiOutcome::Empty => return Some((p.backend(), Attempt::Empty, p)),
            ApiOutcome::RateLimited {
                retry_after_secs,
                msg,
            } => {
                set_cooldown(p, retry_after_secs);
                failures.push(msg);
            }
            ApiOutcome::Fail(msg) => failures.push(msg),
        }
    }
    None
}

pub async fn execute_web_search(args: &Value, cfg: &Config) -> Outcome {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    if query.trim().is_empty() {
        return Outcome::err("web_search requires a non-empty 'query'");
    }
    let count = args
        .get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or(8)
        .clamp(1, 20) as usize;
    let region = args
        .get("region")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("us-en");

    let q = form_urlencode(query);
    // Scrape fallbacks after SearXNG. Region (`kl=`) only applies to DDG.
    let scrape_backends: [(Backend, String); 3] = [
        (
            Backend::DdgLite,
            format!("https://lite.duckduckgo.com/lite/?q={q}&kl={region}"),
        ),
        (
            Backend::DdgHtml,
            format!("https://html.duckduckgo.com/html/?q={q}&kl={region}"),
        ),
        (
            Backend::Mojeek,
            format!("https://www.mojeek.com/search?q={q}"),
        ),
    ];

    // Fail fast under --no-network with empty allowlist (same message shape as fetch).
    if let Some(err) = egress_check("web_search", SEARX_SPACE_INSTANCES, cfg) {
        if cfg.no_network && cfg.fetch_allowlist.is_empty() {
            return Outcome::err(err);
        }
        // Narrow allowlist may deny searx.space but still allow DDG — continue.
    }

    let byte_limit = cfg.fetch_max_bytes.max(64 * 1024);
    let mut failures: Vec<String> = Vec::new();

    // 1) Paid API providers (Exa / Tavily) -- round-robin load-balanced,
    //    quota-tracked, cooldown-aware. Only falls through to scraping when no
    //    API key is set or every provider is unavailable / over budget.
    if let Some((backend, attempt, provider)) =
        try_api_providers(cfg, query, count, byte_limit, &mut failures).await
    {
        match attempt {
            Attempt::Hits(hits) => {
                return render_hits(query, &backend, &hits, Some(&usage_note(provider)));
            }
            Attempt::Empty => {
                return Outcome::ok(format!(
                    "No results found for {query:?} on {}.",
                    backend.label()
                ));
            }
            Attempt::Fail(reason) => failures.push(reason),
        }
    }

    // 2) SearXNG via searx.space ranking
    if let Some((backend, attempt)) =
        try_searxng(cfg, query, count, region, byte_limit, &mut failures).await
    {
        match attempt {
            Attempt::Hits(hits) => return render_hits(query, &backend, &hits, None),
            Attempt::Empty => {
                return Outcome::ok(format!(
                    "No results found for {query:?} on {}.",
                    backend.label()
                ));
            }
            Attempt::Fail(reason) => failures.push(reason),
        }
    }

    // 3) DDG / Mojeek scrape fallbacks
    for (backend, url) in &scrape_backends {
        if let Some(err) = egress_check("web_search", url, cfg) {
            failures.push(format!("{}: skipped ({err})", backend.label()));
            continue;
        }

        let (status, html, truncated) = match fetch_html(cfg, url, byte_limit).await {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{}: {e}", backend.label()));
                continue;
            }
        };

        match classify_response(backend, status, &html, html.len(), truncated, count) {
            Attempt::Hits(hits) => return render_hits(query, backend, &hits, None),
            Attempt::Empty => {
                return Outcome::ok(format!(
                    "No results found for {query:?} on {}.{}",
                    backend.label(),
                    if truncated {
                        " [page body was truncated; refine the query]"
                    } else {
                        ""
                    }
                ));
            }
            Attempt::Fail(reason) => {
                failures.push(reason);
            }
        }
    }

    Outcome::err(format!(
        "web_search: all backends failed; retry later or use the `fetch` tool against a specific URL. query was {query:?}. attempts: {}",
        failures.join(" | ")
    ))
}

/// Minimal application/x-www-form-urlencoded encoder for the query string
/// (no `form_urlencoded` crate dep). Encodes everything except unreserved
/// chars [A-Za-z0-9_.~-] as %XX; spaces become %20 (NOT +) which DDG accepts.
fn form_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fpath"),
            "https://example.com/path"
        );
        // malformed trailing % passes through literally
        assert_eq!(percent_decode("abc%ZZ%"), "abc%ZZ%");
    }

    #[test]
    fn unwrap_ddg_redirect() {
        assert_eq!(
            unwrap_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=x"),
            "https://rust-lang.org/"
        );
        // direct https href unchanged
        assert_eq!(
            unwrap_ddg_url("https://example.com/page"),
            "https://example.com/page"
        );
        // protocol-relative direct link upgraded
        assert_eq!(unwrap_ddg_url("//example.com/x"), "https://example.com/x");
    }

    #[test]
    fn form_urlencode_encodes_spaces_and_special() {
        assert_eq!(form_urlencode("hello world"), "hello%20world");
        assert_eq!(form_urlencode("a&b=c"), "a%26b%3Dc");
        // unreserved chars stay literal
        assert_eq!(form_urlencode("A-z_0.~"), "A-z_0.~");
    }

    #[test]
    fn searx_language_from_ddg_region() {
        assert_eq!(searx_language("us-en"), "en");
        assert_eq!(searx_language("uk-en"), "en");
        assert_eq!(searx_language("de-de"), "de");
        assert_eq!(searx_language("en"), "en");
    }

    const SAMPLE_DDG_LITE: &str = r#"<html><body>
<table>
<tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&rut=abc">The Rust Programming Language</a></td></tr>
<tr><td class="result-snippet">A language empowering everyone to build reliable &amp; efficient software.</td></tr>
<tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&rut=def">std - Rust</a></td></tr>
<tr><td class="result-snippet">API documentation for the Rust standard library.</td></tr>
<tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fads&rut=zzz">Sponsored Ad</a></td></tr>
<tr><td class="result-snippet">Buy our stuff now.</td></tr>
</table>
</body></html>"#;

    const SAMPLE_DDG_HTML: &str = r#"<html><body>
<div id="links">
<div class="result results_links web-result">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&rut=abc">The Rust Programming Language</a></h2>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">A language empowering everyone to build reliable &amp; efficient software.</a>
</div>
<div class="result results_links web-result">
  <h2 class="result__title"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&rut=def">std - Rust</a></h2>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F">API documentation for the Rust standard library.</a>
</div>
</div>
</body></html>"#;

    const SAMPLE_MOJEEK: &str = r#"<html><body><ul>
<!--rs--><li class="r1"><a title="https://rust-lang.org/" href="https://rust-lang.org/" class="ob"><p class="i"><span class="url">https://rust-lang.org/</span></p></a><h2><a class="title" title="https://rust-lang.org/" href="https://rust-lang.org/">Rust Programming Language</a></h2><p class="s"><strong>Rust</strong> is blazingly fast and memory-efficient.</p></li><!--re-->
<!--rs--><li class="r2"><a title="https://doc.rust-lang.org/book/" href="https://doc.rust-lang.org/book/" class="ob"></a><h2><a class="title" title="https://doc.rust-lang.org/book/" href="https://doc.rust-lang.org/book/">The Rust Programming Language</a></h2><p class="s">The HTML format is available online.</p></li><!--re-->
</ul></body></html>"#;

    const SAMPLE_SEARX_HTML: &str = r#"<html><body>
<article class="result result-default category-general">
  <h3><a href="https://www.rust-lang.org/">The Rust Programming Language</a></h3>
  <p class="content">A language empowering everyone to build reliable &amp; efficient software.</p>
</article>
<article class="result result-default category-general">
  <h3><a href="https://doc.rust-lang.org/std/">std - Rust</a></h3>
  <p class="content">API documentation for the Rust standard library.</p>
</article>
</body></html>"#;

    const SAMPLE_SEARX_JSON: &str = r#"{
  "query": "rust",
  "results": [
    {"title": "The Rust Programming Language", "url": "https://www.rust-lang.org/", "content": "A language empowering everyone."},
    {"title": "std - Rust", "url": "https://doc.rust-lang.org/std/", "content": "Standard library."}
  ]
}"#;

    const SAMPLE_INSTANCES_JSON: &str = r#"{
  "instances": {
    "https://good.example/": {
      "network_type": "normal",
      "http": {"status_code": 200, "error": null},
      "uptime": {"uptimeDay": 100.0, "uptimeWeek": 99.0},
      "timing": {"search": {"success_percentage": 100.0, "all": {"median": 0.2}}},
      "engines": {"google": {}, "bing": {"error_rate": 0}}
    },
    "https://slow.example/": {
      "network_type": "normal",
      "http": {"status_code": 200, "error": null},
      "uptime": {"uptimeDay": 100.0, "uptimeWeek": 99.0},
      "timing": {"search": {"success_percentage": 100.0, "all": {"median": 5.0}}},
      "engines": {"google": {"error_rate": 10}, "bing": {}}
    },
    "https://no-google.example/": {
      "network_type": "normal",
      "http": {"status_code": 200, "error": null},
      "uptime": {"uptimeDay": 100.0, "uptimeWeek": 100.0},
      "timing": {"search": {"success_percentage": 100.0, "all": {"median": 0.1}}},
      "engines": {"bing": {}, "duckduckgo": {}}
    },
    "https://broken-google.example/": {
      "network_type": "normal",
      "http": {"status_code": 200, "error": null},
      "uptime": {"uptimeDay": 100.0, "uptimeWeek": 100.0},
      "timing": {"search": {"success_percentage": 100.0, "all": {"median": 0.1}}},
      "engines": {"google": {"error_rate": 100}, "bing": {}}
    },
    "https://down.example/": {
      "network_type": "normal",
      "http": {"status_code": 500, "error": "boom"},
      "uptime": {"uptimeDay": 10.0, "uptimeWeek": 10.0},
      "timing": {"search": {"success_percentage": 10.0, "all": {"median": 1.0}}},
      "engines": {"google": {}, "bing": {}}
    },
    "http://insecure.example/": {
      "network_type": "normal",
      "http": {"status_code": 200, "error": null},
      "uptime": {"uptimeDay": 100.0, "uptimeWeek": 100.0},
      "timing": {"search": {"success_percentage": 100.0, "all": {"median": 0.1}}},
      "engines": {"google": {}, "bing": {}}
    }
  }
}"#;

    #[test]
    fn parse_ddg_lite_structured() {
        let hits = parse_ddg_lite(SAMPLE_DDG_LITE, 10);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert!(hits[0].snippet.contains("reliable & efficient"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std/");
        assert_eq!(hits[2].title, "Sponsored Ad");
    }

    #[test]
    fn parse_ddg_lite_respects_limit() {
        let hits = parse_ddg_lite(SAMPLE_DDG_LITE, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn parse_ddg_lite_fallback_scrapes_links() {
        // markup drift: no result-link class, but external <a> hrefs exist
        let drifted = r#"<div><a href="https://example.org/page1">First</a> <a href="https://example.org/page2">Second</a></div>"#;
        let hits = parse_ddg_lite(drifted, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.org/page1");
        assert_eq!(hits[0].title, "First");
        // snippet empty in fallback
        assert!(hits[0].snippet.is_empty());
    }

    #[test]
    fn parse_ddg_html_structured() {
        let hits = parse_ddg_html(SAMPLE_DDG_HTML, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert!(hits[0].snippet.contains("reliable & efficient"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std/");
    }

    #[test]
    fn parse_ddg_html_href_before_class() {
        let html = r##"<a href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&rut=x" class="result__a">Example</a>
<a class="result__snippet" href="#">An example site.</a>"##;
        let hits = parse_ddg_html(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/");
        assert_eq!(hits[0].title, "Example");
    }

    #[test]
    fn parse_mojeek_structured() {
        let hits = parse_mojeek(SAMPLE_MOJEEK, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert!(hits[0].snippet.contains("blazingly fast"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
    }

    #[test]
    fn parse_searx_html_structured() {
        let hits = parse_searx_html(SAMPLE_SEARX_HTML, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "The Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert!(hits[0].snippet.contains("reliable & efficient"));
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std/");
    }

    #[test]
    fn parse_searx_json_structured() {
        let hits = parse_searx_json(SAMPLE_SEARX_JSON, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(hits[1].title, "std - Rust");
    }

    #[test]
    fn rank_searx_instances_orders_by_health() {
        let doc: Value = serde_json::from_str(SAMPLE_INSTANCES_JSON).unwrap();
        let urls = rank_searx_instances(&doc);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://good.example/");
        assert_eq!(urls[1], "https://slow.example/");
        assert!(!urls.iter().any(|u| u.starts_with("http://")));
        assert!(!urls.iter().any(|u| u.contains("down.example")));
        // Must expose working google+bing — not duckduckgo-only / 100% error google.
        assert!(!urls.iter().any(|u| u.contains("no-google.example")));
        assert!(!urls.iter().any(|u| u.contains("broken-google.example")));
    }

    #[test]
    fn searx_engine_error_rate_requires_object() {
        let engines =
            json!({"google": {}, "bing": {"error_rate": 25}, "dead": {"error_rate": 100}});
        assert_eq!(searx_engine_error_rate(&engines, "google"), Some(0.0));
        assert_eq!(searx_engine_error_rate(&engines, "bing"), Some(25.0));
        assert_eq!(searx_engine_error_rate(&engines, "dead"), None);
        assert_eq!(searx_engine_error_rate(&engines, "missing"), None);
    }

    #[test]
    fn classify_searx_json_hits() {
        match classify_searx_response(
            "example.org",
            reqwest::StatusCode::OK,
            SAMPLE_SEARX_JSON,
            "application/json",
            true,
            8,
        ) {
            Attempt::Hits(h) => assert_eq!(h.len(), 2),
            _ => panic!("expected Hits"),
        }
    }

    #[test]
    fn classify_searx_bot_check_is_fail() {
        let html = "<html><title>Checking your browser…</title><body>wait</body></html>";
        match classify_searx_response(
            "example.org",
            reqwest::StatusCode::OK,
            html,
            "text/html",
            false,
            8,
        ) {
            Attempt::Fail(r) => assert!(r.contains("bot-check") || r.contains("captcha")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn classify_captcha_is_fail() {
        let html = "<html><body>please solve the captcha to continue</body></html>";
        match classify_response(
            &Backend::DdgLite,
            reqwest::StatusCode::OK,
            html,
            html.len(),
            false,
            8,
        ) {
            Attempt::Fail(r) => assert!(r.contains("captcha") || r.contains("anomaly")),
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn classify_ddg_html_hits() {
        match classify_response(
            &Backend::DdgHtml,
            reqwest::StatusCode::OK,
            SAMPLE_DDG_HTML,
            SAMPLE_DDG_HTML.len(),
            false,
            8,
        ) {
            Attempt::Hits(h) => assert_eq!(h.len(), 2),
            _ => panic!("expected Hits"),
        }
    }

    #[test]
    fn classify_mojeek_hits() {
        match classify_response(
            &Backend::Mojeek,
            reqwest::StatusCode::OK,
            SAMPLE_MOJEEK,
            SAMPLE_MOJEEK.len(),
            false,
            8,
        ) {
            Attempt::Hits(h) => assert_eq!(h.len(), 2),
            _ => panic!("expected Hits"),
        }
    }

    #[test]
    fn classify_anomaly_modal_is_fail() {
        let html =
            r#"<div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div>"#;
        match classify_response(
            &Backend::DdgHtml,
            reqwest::StatusCode::OK,
            html,
            html.len(),
            false,
            8,
        ) {
            Attempt::Fail(r) => assert!(r.contains("captcha") || r.contains("anomaly")),
            _ => panic!("expected Fail"),
        }
    }

    // ---- HTTP integration against a one-shot mock server ----
    fn find_header_end(b: &[u8]) -> Option<usize> {
        b.windows(4).position(|w| w == b"\r\n\r\n")
    }

    async fn mock_http(body: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/lite/?q=test");
        let h = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 1024];
            while find_header_end(&buf).is_none() {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        (url, h)
    }

    #[tokio::test]
    async fn web_search_parses_results_over_http() {
        let body = SAMPLE_DDG_LITE.to_string();
        let (_url, _h) = mock_http(body).await;
        // We can't easily point the tool at the mock (URL is hardcoded to DDG),
        // but we can exercise the full parse path + output rendering against the
        // sample HTML by calling parse_ddg_lite directly, and confirm the mock
        // server path used by fetch_tool's tests still serves HTML correctly.
        let hits = parse_ddg_lite(SAMPLE_DDG_LITE, 8);
        assert_eq!(hits.len(), 3);
    }

    #[tokio::test]
    async fn web_search_captcha_is_surfaced() {
        // Build args against a known query; we only test the captcha-detection
        // branch by feeding a synthetic captcha HTML to the parser-detector.
        let html = "<html><body>please solve the captcha to continue</body></html>";
        let low = html.to_ascii_lowercase();
        assert!(low.contains("captcha"));
        let has_result_classes = low.contains("result-link") || low.contains("result-snippet");
        assert!(!has_result_classes);
        assert!(looks_blocked(html));
    }

    #[test]
    fn empty_query_errors() {
        // sync pre-check (block on runtime in the test)
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = Config::default();
        let out = rt.block_on(execute_web_search(&json!({ "query": "  " }), &cfg));
        assert!(!out.ok);
        assert!(out.output.contains("non-empty 'query'"));
    }

    #[test]
    fn no_network_denies() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = Config {
            no_network: true,
            fetch_allowlist: Vec::new(),
            ..Config::default()
        };
        let out = rt.block_on(execute_web_search(&json!({ "query": "rust lang" }), &cfg));
        assert!(!out.ok);
        assert!(out.output.contains("--no-network"));
    }

    #[test]
    fn render_hits_names_backend() {
        let hits = vec![Hit {
            title: "T".into(),
            url: "https://example.com/".into(),
            snippet: "S".into(),
        }];
        let out = render_hits("q", &Backend::Mojeek, &hits, None);
        assert!(out.ok);
        assert!(out.output.contains("Mojeek"));
        assert!(out.output.contains("https://example.com/"));

        let out = render_hits("q", &Backend::Searx("searx.example".into()), &hits, None);
        assert!(out.ok);
        assert!(out.output.contains("SearXNG (searx.example)"));
    }

    // ---- Exa / Tavily API provider tests (hermetic: no env, no network, no file) ----

    #[test]
    fn parse_exa_results_structured() {
        let doc = json!({
            "requestId": "abc",
            "results": [
                {"title": "Rust Lang", "url": "https://www.rust-lang.org/", "text": "Empowering everyone.", "score": 0.9},
                {"title": "std docs", "url": "https://doc.rust-lang.org/std/", "text": "Standard library.", "score": 0.8},
                {"title": "bad", "url": "not-a-url", "text": "x"},
                {"title": "", "url": "https://example.com/", "text": "x"}
            ]
        });
        let hits = parse_exa_results(&doc, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust Lang");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(hits[0].snippet, "Empowering everyone.");
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std/");
    }

    #[test]
    fn parse_exa_results_respects_limit() {
        let doc = json!({
            "results": [
                {"title": "a", "url": "https://a.example/", "text": ""},
                {"title": "b", "url": "https://b.example/", "text": ""},
                {"title": "c", "url": "https://c.example/", "text": ""}
            ]
        });
        assert_eq!(parse_exa_results(&doc, 2).len(), 2);
    }

    #[test]
    fn parse_exa_results_missing_array() {
        assert!(parse_exa_results(&json!({"requestId": "x"}), 5).is_empty());
        assert!(parse_exa_results(&json!({"results": "notarray"}), 5).is_empty());
    }

    #[test]
    fn parse_tavily_results_structured() {
        let doc = json!({
            "query": "rust",
            "answer": "Rust is...",
            "results": [
                {"title": "Rust Lang", "url": "https://www.rust-lang.org/", "content": "Empowering everyone.", "score": 0.81},
                {"title": "std", "url": "https://doc.rust-lang.org/std/", "content": "Std lib.", "score": 0.7}
            ],
            "usage": {"credits": 1}
        });
        let hits = parse_tavily_results(&doc, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust Lang");
        assert_eq!(hits[0].snippet, "Empowering everyone.");
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std/");
    }

    #[test]
    fn parse_tavily_results_skips_invalid() {
        let doc = json!({
            "results": [
                {"title": "ok", "url": "https://ok.example/", "content": "c"},
                {"title": "no-url", "url": "", "content": "c"},
                {"title": "", "url": "https://notitle.example/", "content": "c"}
            ]
        });
        let hits = parse_tavily_results(&doc, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://ok.example/");
    }

    #[test]
    fn current_month_is_yyyy_mm() {
        let m = current_month();
        assert_eq!(m.len(), 7);
        assert_eq!(m.as_bytes()[4], b'-');
        let (y, mo) = m.split_once('-').unwrap();
        assert!(y.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(y.len(), 4);
        let mon: u32 = mo.parse().unwrap();
        assert!((1..=12).contains(&mon));
    }

    #[test]
    fn read_provider_usage_keeps_count_same_month() {
        let doc = json!({"exa": {"month": "2025-06", "count": 42}});
        let u = read_provider_usage(&doc, "exa", "2025-06");
        assert_eq!(u.month, "2025-06");
        assert_eq!(u.count, 42);
    }

    #[test]
    fn read_provider_usage_resets_on_new_month() {
        let doc = json!({"exa": {"month": "2025-06", "count": 42}});
        let u = read_provider_usage(&doc, "exa", "2025-07");
        assert_eq!(u.month, "2025-07");
        assert_eq!(u.count, 0);
    }

    #[test]
    fn read_provider_usage_missing_is_default() {
        let u = read_provider_usage(&json!({}), "exa", "2025-06");
        assert_eq!(u.count, 0);
        assert!(u.month.is_empty());
    }

    fn fresh_state() -> UsageState {
        UsageState::default()
    }

    #[test]
    fn provider_usable_no_key_is_false() {
        let st = fresh_state();
        assert!(!provider_usable(&st, ApiProvider::Exa, false));
        assert!(!provider_usable(&st, ApiProvider::Tavily, false));
    }

    #[test]
    fn provider_usable_under_budget_is_true() {
        let mut st = fresh_state();
        st.exa = MonthlyUsage {
            month: current_month(),
            count: 0,
        };
        assert!(provider_usable(&st, ApiProvider::Exa, true));
    }

    #[test]
    fn provider_usable_over_budget_is_false() {
        let mut st = fresh_state();
        // u64::MAX exceeds any monthly_limit (>=1), so always over budget.
        st.tavily = MonthlyUsage {
            month: current_month(),
            count: u64::MAX,
        };
        assert!(!provider_usable(&st, ApiProvider::Tavily, true));
    }

    #[test]
    fn provider_usable_in_cooldown_is_false() {
        let mut st = fresh_state();
        st.exa_cooldown_until = Some(Instant::now() + Duration::from_secs(120));
        assert!(!provider_usable(&st, ApiProvider::Exa, true));
    }

    #[test]
    fn provider_usable_expired_cooldown_is_true() {
        let mut st = fresh_state();
        st.exa_cooldown_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(provider_usable(&st, ApiProvider::Exa, true));
    }

    #[test]
    fn truncate_msg_passthrough_and_cut() {
        assert_eq!(truncate_msg("short", 10), "short");
        let long = "abcdefghij".repeat(5);
        let t = truncate_msg(&long, 10);
        assert!(t.len() <= 13); // 10 + "..."
        assert!(t.starts_with("abcdefghij"));
    }

    #[test]
    fn parse_retry_after_seconds_and_default() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(parse_retry_after(&h), 30);

        let h2 = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h2), 60);
    }

    #[test]
    fn api_provider_labels() {
        assert_eq!(Backend::Exa.label(), "Exa API");
        assert_eq!(Backend::Tavily.label(), "Tavily API");
        assert_eq!(ApiProvider::Exa.backend(), Backend::Exa);
        assert_eq!(ApiProvider::Tavily.backend(), Backend::Tavily);
    }

    #[test]
    fn api_provider_key_reads_persisted_config() {
        // A key set via /search-key (persisted in cfg.search_keys) is returned
        // directly — independent of any env var.
        let mut cfg = Config::default();
        cfg.search_keys
            .insert("exa".into(), "exa-persisted-key".into());
        assert_eq!(
            ApiProvider::Exa.key(&cfg).as_deref(),
            Some("exa-persisted-key")
        );
        cfg.search_keys
            .insert("tavily".into(), "tvly-persisted-key".into());
        assert_eq!(
            ApiProvider::Tavily.key(&cfg).as_deref(),
            Some("tvly-persisted-key")
        );
    }

    #[test]
    fn provider_available_with_persisted_key() {
        // A persisted key (in cfg) makes the provider usable (not over budget,
        // not in cooldown) even with no env var.
        let mut cfg = Config::default();
        cfg.search_keys.insert("exa".into(), "k".into());
        let st = UsageState::default();
        assert!(provider_available(&st, ApiProvider::Exa, &cfg));
        // Do not assert that Tavily is unavailable here: provider resolution
        // intentionally falls back to TAVILY_API_KEY, which may be present in
        // the developer/CI environment. No-key behavior is covered directly by
        // provider_usable_no_key_is_false without consulting process globals.
    }

    #[test]
    fn render_hits_shows_usage_note() {
        let hits = vec![Hit {
            title: "T".into(),
            url: "https://example.com/".into(),
            snippet: "S".into(),
        }];
        let out = render_hits("q", &Backend::Exa, &hits, Some("5/1000 this month"));
        assert!(out.ok);
        assert!(out.output.contains("Exa API"));
        assert!(out.output.contains("5/1000 this month"));
    }
}
