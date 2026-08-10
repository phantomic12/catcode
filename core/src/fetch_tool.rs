// Native HTTP fetch tool. Unlike `bash curl`, this is a first-class tool that is
// NOT subject to the bash sandbox / `--no-network` (`unshare -n` only wraps the
// bash command), so the agent can still look up docs under the hard-security
// config. A host allowlist (`cfg.fetch_allowlist`) restricts egress; empty
// allowlist = any public http(s) host (private/loopback/link-local ranges are
// blocked by default as SSRF hardening; an explicit allowlist entry overrides).
use crate::config::Config;
use crate::tools::{smart_truncate, Outcome};
use serde_json::Value;
use std::net::IpAddr;

/// Parse an absolute http(s) URL enough to validate it and extract the host
/// (lowercased) for allowlist matching. Returns (scheme, host). No `url` crate
/// dependency — a tiny hand-rolled parse is plenty and avoids a new dep.
fn parse_http_host(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    // Strip userinfo (`user:pass@host`) before host extract. Stopping only on
    // `/ ? # :` would treat `foo@169.254.169.254` as the host string, which
    // bypasses IP/hostname private checks while reqwest still connects to the
    // real host after `@` (SSRF).
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    // IPv6 literal: [::1]:8080 — host is the bracketed content.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest.find(']')?;
        rest[..end].to_ascii_lowercase()
    } else {
        let end = host_port.find(':').unwrap_or(host_port.len());
        host_port[..end].trim_end_matches('.').to_ascii_lowercase()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();

    if host.is_empty() || host.contains('@') {
        return None;
    }
    Some((scheme, host))
}

/// Match a host against one allowlist glob pattern. A leading `*.` matches the
/// bare domain and any subdomain (`*.rust-lang.org` matches `rust-lang.org`
/// and `doc.rust-lang.org`); otherwise exact, case-insensitive.
fn host_matches(host: &str, pattern: &str) -> bool {
    let h = host.to_ascii_lowercase();
    let p = pattern.trim().to_ascii_lowercase();
    if let Some(rest) = p.strip_prefix("*.") {
        h == rest || h.ends_with(&format!(".{rest}"))
    } else {
        h == p
    }
}

/// Is `ip` in a private/loopback/link-local/unspecified range? These are
/// blocked by default (empty fetch_allowlist) to harden against SSRF — e.g.
/// cloud-metadata at 169.254.169.254, localhost services, RFC-1918 internals.
/// An explicit fetch_allowlist entry overrides this (operator opt-in).
fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127 // loopback 127.0.0.0/8
                || (o[0] == 169 && o[1] == 254) // link-local 169.254.0.0/16 (cloud-metadata)
                || o[0] == 10 // private 10.0.0.0/8
                || (o[0] == 172 && (16..=31).contains(&o[1])) // private 172.16.0.0/12
                || (o[0] == 192 && o[1] == 168) // private 192.168.0.0/16
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
                || o[0] == 0 // 0.0.0.0/8 (unspecified + "this network")
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || v6
                    .to_ipv4()
                    .map(|v4| ip_is_private(IpAddr::V4(v4)))
                    .unwrap_or(false) // ::ffff:a.b.c.d (IPv4-mapped)
        }
    }
}

/// Hostnames that resolve to a private/loopback/internal address but don't
/// parse as an IP literal, so `ip_is_private` misses them. Blocking them by
/// name closes the SSRF gap where `fetch http://localhost:…` (or a redirect
/// to `metadata.google.internal`) reaches a local/internal service under the
/// default empty allowlist. An explicit `fetch_allowlist` entry overrides this.
fn hostname_is_private(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    const BLOCKED: &[&str] = &[
        "localhost",
        "ip6-localhost",
        "ip6-loopback",
        "metadata",
        "metadata.google.internal",
        "metadata.aws.internal",
        "metadata.azure.com",
        "ip6-allnodes",
        "ip6-allrouters",
        "broadcasthost",
    ];
    const WILDCARD_DNS_SUFFIXES: &[&str] = &["nip.io", "sslip.io", "xip.io", "localtest.me"];
    BLOCKED.iter().any(|b| *b == host)
        || host == "local"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || WILDCARD_DNS_SUFFIXES
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// Is `host` a private address? IP literals and known local names are checked
/// synchronously; request execution additionally resolves and pins DNS before
/// every redirect hop.
pub(crate) fn host_is_private(host: &str) -> bool {
    let normalized = host.trim_end_matches('.');
    if hostname_is_private(normalized) {
        return true;
    }
    match normalized.parse::<IpAddr>() {
        Ok(ip) => ip_is_private(ip),
        Err(_) => false,
    }
}

/// Decide whether `host` is permitted. A non-empty allowlist means the operator
/// explicitly opted in to exactly those hosts (a listed private host is allowed
/// — explicit opt-in wins). An empty allowlist allows any PUBLIC host;
/// private/loopback/link-local ranges are blocked by default (SSRF hardening).
pub(crate) fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    if !allowlist.is_empty() {
        return allowlist.iter().any(|p| host_matches(host, p));
    }
    !host_is_private(host)
}

/// Browser navigation policy: honor `--no-network` / `fetch_allowlist` the same
/// way `fetch` does, and always block private/link-local metadata hosts unless
/// explicitly allowlisted (CORE_REVIEW browser SSRF).
pub(crate) fn browser_navigation_allowed(
    url: &str,
    no_network: bool,
    allowlist: &[String],
) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if lower == "about:blank" || lower.starts_with("about:blank#") {
        return Ok(());
    }
    let Some((scheme, host)) = parse_http_host(url) else {
        return Err("navigation blocked: could not parse http(s) URL host".into());
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!("only http(s) navigation is allowed (got {scheme})"));
    }
    if no_network && allowlist.is_empty() {
        return Err("navigation blocked: --no-network is set and fetch_allowlist is empty".into());
    }
    if !host_allowed(&host, allowlist) {
        return Err(format!(
            "navigation blocked: host '{host}' is not permitted by network policy"
        ));
    }
    Ok(())
}

/// A redirect policy that re-checks the fetch_allowlist on EVERY redirect hop,
/// not just the original URL. `Policy::limited(5)` would follow a
/// docs.rs → 169.254.169.254 (cloud-metadata) or → localhost redirect straight
/// to an internal host, defeating the `--no-network` + `fetch_allowlist` opt-in
/// security model (the whole point: bash stays offline, fetch reaches only
/// listed hosts). A redirect whose target host isn't allowed is stopped — the
/// 3xx response is returned without following, and the disallowed host is
/// never contacted. With an empty allowlist, private/loopback/link-local
/// redirect targets are also stopped (SSRF hardening — see `host_allowed`).
/// Shared by `fetch` and `web_search` so both honor the same redirect policy.
pub(crate) fn allowlist_redirect_policy(allowlist: Vec<String>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        let host = attempt.url().host_str().unwrap_or("").to_ascii_lowercase();
        if host_allowed(&host, &allowlist) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

fn validate_resolved_addresses(
    host: &str,
    allowlist: &[String],
    addresses: &[std::net::SocketAddr],
) -> Result<(), String> {
    if addresses.is_empty() {
        return Err(format!("DNS resolution returned no addresses for '{host}'"));
    }
    if allowlist.is_empty() && addresses.iter().any(|addr| ip_is_private(addr.ip())) {
        return Err(format!(
            "host '{host}' resolves to a private/loopback/link-local address"
        ));
    }
    Ok(())
}

async fn resolved_target(
    url: &reqwest::Url,
    allowlist: &[String],
) -> Result<(String, std::net::SocketAddr), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !host_allowed(&host, allowlist) {
        return Err(format!("host '{host}' is not permitted"));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no port".to_string())?;
    let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?
        .collect();
    validate_resolved_addresses(&host, allowlist, &addresses)?;
    Ok((host, addresses[0]))
}

pub(crate) async fn send_resolved(
    mut url: reqwest::Url,
    cfg: &Config,
) -> Result<reqwest::Response, String> {
    for redirects in 0..=5 {
        let (host, target) = resolved_target(&url, &cfg.fetch_allowlist).await?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                cfg.fetch_timeout_secs.max(1),
            ))
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .resolve(&host, target)
            .user_agent("catalyst-code-fetch/0.1")
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if redirects == 5 {
            return Err("redirect limit exceeded".into());
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "redirect response has no valid Location header".to_string())?;
        url = url
            .join(location)
            .map_err(|e| format!("invalid redirect URL: {e}"))?;
    }
    unreachable!()
}

pub(crate) async fn post_resolved_json(
    url: &str,
    cfg: &Config,
    auth_header: (&str, &str),
    body: &Value,
) -> Result<reqwest::Response, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let (host, target) = resolved_target(&parsed, &cfg.fetch_allowlist).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            cfg.fetch_timeout_secs.max(1),
        ))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, target)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    client
        .post(parsed)
        .header(auth_header.0, auth_header.1)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))
}

/// Remove every case-insensitive `<open ...>...</open>` block from `s`
/// (used to drop `<script>`/`<style>` so their text doesn't leak into the
/// readable output). Bounded bodies keep this affordable despite the
/// repeated lowercase scans.
fn cut_blocks(s: &str, open: &str, close: &str) -> String {
    let open_l = open.to_ascii_lowercase();
    let close_l = close.to_ascii_lowercase();
    let close_len = close_l.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let lower = s[i..].to_ascii_lowercase();
        match lower.find(&open_l) {
            Some(p) => {
                out.push_str(&s[i..i + p]);
                let after = i + p;
                let rest_l = s[after..].to_ascii_lowercase();
                match rest_l.find(&close_l) {
                    Some(c) => i = after + c + close_len,
                    None => break, // no closer: drop the rest
                }
            }
            None => {
                out.push_str(&s[i..]);
                break;
            }
        }
    }
    out
}

/// Collapse runs of whitespace, preserving newlines (a doc page stays
/// paragraph-shaped instead of one giant line). Uses `char::from(10)` for the
/// newline to keep this file free of backslash escapes.
fn collapse_ws(s: &str) -> String {
    let nl = char::from(10);
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    let mut prev_nl = false;
    for c in s.chars() {
        if c == nl {
            if !prev_nl {
                out.push(nl);
            }
            prev_nl = true;
            prev_ws = false;
            continue;
        }
        prev_nl = false;
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Decode the handful of HTML entities that show up in doc text. `&amp;` is
/// decoded last so `&amp;lt;` becomes `&lt;`, not `<`. Uses `char::from(n)` for
/// the quote chars to avoid backslash/quote escaping in source.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", &char::from(34).to_string())
        .replace("&#39;", &char::from(39).to_string())
        .replace("&apos;", &char::from(39).to_string())
        .replace("&amp;", "&")
}

/// Shared network egress policy for HTTP tools (fetch, web_search) and the
/// documented relationship with bash `--no-network` / sandbox.
///
/// Bash isolation (`unshare -n` / firejail) is separate: it only wraps the
/// shell. This policy is what HTTP tools consult so `--no-network` cannot be
/// bypassed via `fetch` unless `fetch_allowlist` is explicitly populated.
#[derive(Clone, Debug)]
pub struct NetworkPolicy {
    pub no_network: bool,
    pub allowlist: Vec<String>,
}

impl NetworkPolicy {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            no_network: cfg.no_network,
            allowlist: cfg.fetch_allowlist.clone(),
        }
    }

    pub fn check(&self, label: &str, url: &str) -> Option<String> {
        // Reuse the same rules as egress_check without requiring a full Config.
        let (_scheme, host) = match parse_http_host(url) {
            Some(v) => v,
            None => return Some(format!("{label}: url must be an absolute http(s) URL")),
        };
        if self.no_network && self.allowlist.is_empty() {
            return Some(format!(
                "{label}: network egress is disabled (--no-network) and no fetch_allowlist is configured; populate fetch_allowlist to opt specific hosts in for {label} while keeping bash offline"
            ));
        }
        if !host_allowed(&host, &self.allowlist) {
            if self.allowlist.is_empty() {
                return Some(format!(
                    "{label}: host '{host}' is a private/loopback/link-local address and is blocked by default (empty fetch_allowlist); add it to fetch_allowlist to explicitly opt in"
                ));
            }
            return Some(format!(
                "{label}: host '{host}' is not in the allowlist ({} pattern(s) configured); add it to fetch_allowlist to permit it",
                self.allowlist.len()
            ));
        }
        None
    }
}

/// Shared egress decision for HTTP tools (fetch, web_search). Returns
/// Some(err_msg) if the request must be denied per --no-network /
/// fetch_allowlist, else None. Reused by web_search so it honors the SAME
/// security model as fetch (no surprise bypass of --no-network).
pub(crate) fn egress_check(label: &str, url: &str, cfg: &Config) -> Option<String> {
    NetworkPolicy::from_config(cfg).check(label, url)
}

/// Very light HTML-to-text: drop script/style blocks, strip tags, collapse
/// whitespace, decode common entities. Not a real parser — just enough that a
/// fetched doc page is readable instead of a wall of markup.
pub(crate) fn html_to_text(html: &str) -> String {
    let s = cut_blocks(html, "<script", "</script>");
    let s = cut_blocks(&s, "<style", "</style>");
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&collapse_ws(&out))
}

pub async fn execute_fetch(args: &Value, cfg: &Config) -> Outcome {
    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);
    if url.is_empty() {
        return Outcome::err("fetch requires a 'url'");
    }
    let (_scheme, host) = match parse_http_host(url) {
        Some(v) => v,
        None => return Outcome::err("fetch: url must be an absolute http(s) URL"),
    };
    // --no-network blocks bash egress; honor the operator's intent for fetch
    // too, UNLESS they explicitly configured a fetch_allowlist (opting specific
    // hosts in for doc lookups while keeping bash offline). Empty allowlist +
    // --no-network = deny (no surprise bypass of the egress block).
    if cfg.no_network && cfg.fetch_allowlist.is_empty() {
        return Outcome::err(
            "fetch: network egress is disabled (--no-network) and no fetch_allowlist is configured; populate fetch_allowlist to opt specific hosts in for fetch while keeping bash offline",
        );
    }
    if !host_allowed(&host, &cfg.fetch_allowlist) {
        if cfg.fetch_allowlist.is_empty() {
            return Outcome::err(format!(
                "fetch: host '{host}' is a private/loopback/link-local address and is blocked by default (empty fetch_allowlist); add it to fetch_allowlist to explicitly opt in"
            ));
        }
        return Outcome::err(format!(
            "fetch: host '{host}' is not in the allowlist ({} pattern(s) configured); add it to fetch_allowlist to permit it",
            cfg.fetch_allowlist.len()
        ));
    }

    let parsed_url = match reqwest::Url::parse(url) {
        Ok(value) => value,
        Err(e) => return Outcome::err(format!("fetch: invalid URL: {e}")),
    };
    let resp = match send_resolved(parsed_url, cfg).await {
        Ok(response) => response,
        Err(e) => return Outcome::err(format!("fetch: {e}")),
    };
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Stream the body, bounding memory to `limit + 1` so a giant response can't
    // OOM the agent. The +1 lets us detect truncation.
    let limit = cfg.fetch_max_bytes.max(1024);
    use futures_util::StreamExt;
    let mut collected: Vec<u8> = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Outcome::err(format!("fetch: failed to read body: {e}")),
        };
        let room = limit - collected.len();
        if chunk.len() <= room {
            collected.extend_from_slice(&chunk);
        } else {
            collected.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
    }

    let is_html = !raw && (ctype.contains("text/html") || ctype.starts_with("application/xhtml"));
    let text = if is_html {
        html_to_text(&String::from_utf8_lossy(&collected))
    } else {
        String::from_utf8_lossy(&collected).to_string()
    };

    let mut out = format!("HTTP {status}  {ctype}\n");
    if truncated {
        out.push_str(&format!("...[body truncated at {limit} bytes]...\n"));
    }
    out.push_str(&text);

    // Cap the final text the model sees so a big doc doesn't blow context.
    const OUT_CAP: usize = 24_576;
    if out.len() > OUT_CAP {
        out = smart_truncate(&out, OUT_CAP);
    }
    Outcome {
        ok: status.is_success(),
        output: out,
        diff: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_basic() {
        assert_eq!(
            parse_http_host("https://Doc.Rust-Lang.org/std/"),
            Some(("https".into(), "doc.rust-lang.org".into()))
        );
        assert_eq!(
            parse_http_host("http://example.com:8080/x"),
            Some(("http".into(), "example.com".into()))
        );
        assert_eq!(parse_http_host("file:///etc/passwd"), None);
        assert_eq!(parse_http_host("not a url"), None);
        assert_eq!(
            parse_http_host("http://[::1]:8080/x"),
            Some(("http".into(), "::1".into()))
        );
        // userinfo must not become the "host" (SSRF bypass via foo@169.254…)
        assert_eq!(
            parse_http_host("http://foo@169.254.169.254/"),
            Some(("http".into(), "169.254.169.254".into()))
        );
        assert_eq!(
            parse_http_host("https://user:pass@example.com:443/path"),
            Some(("https".into(), "example.com".into()))
        );
    }

    #[test]
    fn userinfo_cannot_bypass_private_block() {
        assert!(!host_allowed("169.254.169.254", &[]));
        let (_s, host) = parse_http_host("http://evil@169.254.169.254/latest").unwrap();
        assert!(!host_allowed(&host, &[]));
        assert!(!host_allowed("100.64.0.1", &[])); // CGNAT
        assert!(!host_allowed("100.127.255.255", &[]));
        assert!(host_allowed("100.128.0.1", &[])); // just outside CGNAT
    }

    #[test]
    fn hostname_aliases_blocked() {
        // H6: hostnames that resolve to loopback/internal but don't parse as
        // IP literals must be blocked, else `fetch http://localhost:…` (or a
        // redirect to metadata.google.internal) reaches a local/internal
        // service under the default empty allowlist.
        for h in [
            "localhost",
            "LOCALHOST",
            "ip6-localhost",
            "ip6-loopback",
            "metadata.google.internal",
            "metadata",
        ] {
            assert!(hostname_is_private(h), "{} should be private", h);
        }
        // a real public host is not private
        assert!(!hostname_is_private("example.com"));
    }

    #[test]
    fn trailing_dot_and_wildcard_dns_hosts_are_private() {
        for host in [
            "localhost.",
            "127.0.0.1.",
            "169.254.169.254.nip.io",
            "10.0.0.1.sslip.io",
        ] {
            assert!(!host_allowed(host, &[]), "{host} should be denied");
        }
    }

    #[test]
    fn host_allowlist_matching() {
        assert!(host_matches("doc.rust-lang.org", "*.rust-lang.org"));
        assert!(host_matches("rust-lang.org", "*.rust-lang.org"));
        assert!(!host_matches("evilrust-lang.org", "*.rust-lang.org"));
        assert!(host_matches("docs.rs", "docs.rs"));
        assert!(!host_matches("docs.rs", "crates.io"));
        // empty allowlist = allow any public host (private ranges blocked)
        assert!(host_allowed("anything.example", &[]));
        let list = vec!["*.rust-lang.org".into(), "docs.rs".into()];
        assert!(host_allowed("doc.rust-lang.org", &list));
        assert!(host_allowed("docs.rs", &list));
        assert!(!host_allowed("evil.com", &list));
    }

    #[test]
    fn private_ranges_blocked_by_default() {
        // empty allowlist: private/loopback/link-local blocked; public allowed.
        assert!(!host_allowed("169.254.169.254", &[])); // cloud-metadata
        assert!(!host_allowed("127.0.0.1", &[])); // loopback
        assert!(!host_allowed("127.255.255.255", &[])); // loopback edge
        assert!(!host_allowed("10.0.0.5", &[])); // private 10/8
        assert!(!host_allowed("192.168.1.1", &[])); // private 192.168/16
        assert!(!host_allowed("172.16.0.1", &[])); // private 172.16/12 start
        assert!(!host_allowed("172.31.255.255", &[])); // private 172.16/12 end
        assert!(host_allowed("172.32.0.1", &[])); // just outside → public
        assert!(host_allowed("8.8.8.8", &[])); // public
        assert!(host_allowed("1.1.1.1", &[])); // public
        assert!(host_allowed("example.com", &[])); // hostname → allowed (no DNS)
                                                   // IPv6
        assert!(!host_allowed("::1", &[])); // v6 loopback
        assert!(!host_allowed("fe80::1", &[])); // v6 link-local
        assert!(!host_allowed("fc00::1", &[])); // v6 unique-local
        assert!(!host_allowed("fd00::1", &[])); // v6 unique-local
        assert!(!host_allowed("::ffff:169.254.169.254", &[])); // v4-mapped metadata
        assert!(host_allowed("2606:4700:4700::1111", &[])); // public v6
    }

    #[test]
    fn explicit_allowlist_overrides_private_block() {
        // operator explicitly allowlists a private host → allowed (opt-in wins).
        let list = vec!["127.0.0.1".into(), "localhost".into()];
        assert!(host_allowed("127.0.0.1", &list));
        assert!(host_allowed("localhost", &list));
        assert!(!host_allowed("169.254.169.254", &list)); // not listed → denied
    }

    #[test]
    fn html_strips_tags_and_scripts() {
        let html = "<html><head><style>body{color:red}</style></head><body><script>alert(1)</script><h1>Title</h1><p>Hi &amp; bye &lt;3</p></body></html>";
        let text = html_to_text(html);
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(text.contains("Title"));
        assert!(text.contains("Hi & bye <3"));
    }

    // ---- HTTP integration: execute_fetch against a one-shot mock server ----
    fn find_header_end(b: &[u8]) -> Option<usize> {
        b.windows(4).position(|w| w == b"\r\n\r\n")
    }

    async fn mock_http(body: String, ctype: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/page");
        let ctype = ctype.to_string();
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
            // drain request body if present
            let header_end = find_header_end(&buf).unwrap_or(buf.len());
            let header_str = String::from_utf8_lossy(&buf[..header_end]);
            let clen = header_str
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = header_end + 4;
            let mut have = buf.len().saturating_sub(body_start);
            while have < clen {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                have += n;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
                ctype,
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        (url, h)
    }

    #[tokio::test]
    async fn fetch_strips_html_over_http() {
        let html = String::from(
            "<html><body><script>evil()</script><h1>Rust Docs</h1><p>Use Vec for grow.</p></body></html>",
        );
        let (url, _h) = mock_http(html, "text/html; charset=utf-8").await;
        let cfg = crate::config::Config {
            // 127.0.0.1 is loopback → blocked by default; allowlist the mock host.
            fetch_allowlist: vec!["127.0.0.1".into(), "localhost".into()],
            fetch_timeout_secs: 10,
            fetch_max_bytes: 1 << 20,
            ..crate::config::Config::default()
        };
        let out = execute_fetch(&serde_json::json!({ "url": url }), &cfg).await;
        assert!(out.ok, "{}", out.output);
        assert!(out.output.contains("Rust Docs"));
        assert!(out.output.contains("Use Vec for grow."));
        assert!(!out.output.contains("evil()"));
        assert!(out.output.contains("text/html"));
    }

    #[tokio::test]
    async fn fetch_allowlist_denies_other_hosts() {
        let (_url, _h) = mock_http("hi".into(), "text/plain").await; // unused: denied before connect
        let cfg = crate::config::Config {
            fetch_allowlist: vec!["docs.rs".into(), "*.rust-lang.org".into()],
            ..crate::config::Config::default()
        };
        let out = execute_fetch(
            &serde_json::json!({ "url": "https://evil.example.com/x" }),
            &cfg,
        )
        .await;
        assert!(!out.ok);
        assert!(out.output.contains("not in the allowlist"));
    }

    #[tokio::test]
    async fn fetch_no_network_denies_without_allowlist() {
        // --no-network + empty allowlist: fetch must not bypass the egress block.
        let cfg = crate::config::Config {
            no_network: true,
            fetch_allowlist: Vec::new(),
            ..crate::config::Config::default()
        };
        let out = execute_fetch(&serde_json::json!({ "url": "https://example.com/x" }), &cfg).await;
        assert!(!out.ok);
        assert!(out.output.contains("--no-network"));
        assert!(out.output.contains("fetch_allowlist"));
    }

    #[tokio::test]
    async fn fetch_no_network_allows_when_allowlist_opts_in() {
        let html = String::from("<html><body><p>docs ok</p></body></html>");
        let (url, _h) = mock_http(html, "text/html").await;
        let cfg = crate::config::Config {
            no_network: true,
            fetch_allowlist: vec!["127.0.0.1".into(), "localhost".into()],
            fetch_timeout_secs: 10,
            fetch_max_bytes: 1 << 20,
            ..crate::config::Config::default()
        };
        let out = execute_fetch(&serde_json::json!({ "url": url }), &cfg).await;
        assert!(out.ok, "{}", out.output);
        assert!(out.output.contains("docs ok"));
    }

    #[test]
    fn dns_validation_rejects_public_name_resolving_to_loopback() {
        let addresses = [std::net::SocketAddr::from(([127, 0, 0, 1], 80))];
        let err =
            validate_resolved_addresses("public-looking.example", &[], &addresses).unwrap_err();
        assert!(err.contains("private/loopback/link-local"), "{err}");
    }

    #[tokio::test]
    async fn resolved_transport_rechecks_real_redirect_target() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://localhost:9/private\r\nContent-Length: 0\r\n\r\n",
            ).await.unwrap();
        });
        let cfg = crate::config::Config {
            fetch_allowlist: vec!["127.0.0.1".into()],
            ..crate::config::Config::default()
        };
        let url = reqwest::Url::parse(&format!("http://{addr}/redirect")).unwrap();
        let err = send_resolved(url, &cfg).await.unwrap_err();
        server.await.unwrap();
        assert!(
            err.contains("localhost") && err.contains("not permitted"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn resolved_transport_rejects_loopback_dns_target_without_allowlist() {
        let cfg = crate::config::Config {
            fetch_allowlist: Vec::new(),
            ..crate::config::Config::default()
        };
        let url = reqwest::Url::parse("http://localhost./private").unwrap();
        let err = send_resolved(url, &cfg).await.unwrap_err();
        assert!(
            err.contains("private") || err.contains("not permitted"),
            "{err}"
        );
    }
}
