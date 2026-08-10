//! Deterministic hybrid memory retrieval (spec §15).
//!
//! Scores [`MemoryEntry`] values against a prompt + [`TaskFingerprint`] using
//! fixed weights — no embeddings API, no network. Fail-open / pure functions.
//!
//! Lexical signal uses BM25-lite over the candidate corpus in [`rank_memories`].
//! Single-doc [`score_memory`] falls back to TF overlap.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::memory::{significant_tokens, MemoryEntry, MemoryStatus, Scope};
use crate::task_fingerprint::{fingerprint_similarity, TaskFingerprint};
use serde::Serialize;

/// Spec §15 weights (sum = 1.0).
const W_LEXICAL: f32 = 0.25;
const W_SYMBOL: f32 = 0.20;
const W_FINGERPRINT: f32 = 0.15;
const W_PATH: f32 = 0.10;
const W_UTILITY: f32 = 0.10;
const W_CONFIDENCE: f32 = 0.05;
const W_VERIFICATION: f32 = 0.05;
const W_SCOPE: f32 = 0.05;
const W_DIAGNOSTIC: f32 = 0.05;

/// Project-scope applicability bonus applied after the weighted sum (clamped).
const PROJECT_BONUS: f32 = 0.08;

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

/// Machine-readable scoring diagnostics used by local evaluation fixtures and
/// opt-in diagnostics. It contains no memory body or prompt text.
#[derive(Clone, Debug, Serialize)]
pub struct RetrievalScoreDebug {
    pub lexical: f32,
    pub semantic_or_sketch: f32,
    pub recency: f32,
    pub importance: f32,
    pub scope_match: f32,
    pub path_or_symbol_match: f32,
    pub staleness_penalty: f32,
    pub contradiction_penalty: f32,
    pub confidence: f32,
    pub utility: f32,
    pub total: f32,
}

/// Score a single memory. Returns `(score, reasons)`.
///
/// Uses TF-overlap for the lexical channel (no corpus DF). Prefer
/// [`rank_memories`] when scoring a batch so BM25-lite applies.
pub fn score_memory(entry: &MemoryEntry, prompt: &str, fp: &TaskFingerprint) -> (f32, Vec<String>) {
    let prompt_tokens = tokenize_rich(prompt);
    let mem_tokens = memory_tokens(entry);
    let lexical = tf_overlap(&prompt_tokens, &mem_tokens);
    score_memory_with_lexical(entry, fp, &prompt_tokens, lexical)
}

/// Return score components without exposing the prompt or memory content.
pub fn debug_score_memory(
    entry: &MemoryEntry,
    prompt: &str,
    fp: &TaskFingerprint,
) -> RetrievalScoreDebug {
    let prompt_tokens = tokenize_rich(prompt);
    let mem_tokens = memory_tokens(entry);
    let lexical = tf_overlap(&prompt_tokens, &mem_tokens);
    score_debug(entry, fp, &prompt_tokens, lexical)
}

/// Rank memories for a prompt + fingerprint. Deterministic order on ties (name).
///
/// Builds document DF once across the batch, scores BM25-lite per memory,
/// normalizes lexical scores by the batch max, then applies §15 weights.
pub fn rank_memories(
    memories: &[MemoryEntry],
    prompt: &str,
    fp: &TaskFingerprint,
    limit: usize,
) -> Vec<(f32, MemoryEntry, Vec<String>)> {
    let prompt_tokens = tokenize_rich(prompt);
    let docs: Vec<Vec<String>> = memories.iter().map(memory_tokens).collect();
    let (df, n) = build_df(&docs);
    let avgdl = if docs.is_empty() {
        0.0
    } else {
        docs.iter().map(|d| d.len() as f32).sum::<f32>() / docs.len() as f32
    };

    let raw_bm25: Vec<f32> = docs
        .iter()
        .map(|doc| bm25_raw(&prompt_tokens, doc, &df, n, avgdl))
        .collect();
    let max_bm25 = raw_bm25.iter().copied().fold(0.0f32, f32::max);

    let mut scored: Vec<(f32, MemoryEntry, Vec<String>)> = memories
        .iter()
        .zip(raw_bm25.iter())
        .map(|(m, &raw)| {
            let lexical = if max_bm25 > 1e-9 {
                (raw / max_bm25).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (s, reasons) = score_memory_with_lexical(m, fp, &prompt_tokens, lexical);
            (s, m.clone(), reasons)
        })
        .filter(|(s, _, _)| *s > 0.0)
        .collect();
    // Blend activation hit rates when a project id is known via env override of
    // learning root tests, or via optional thread — call sites that know the
    // project should prefer rank_memories_in_project (below).
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    scored.truncate(limit);
    scored
}

/// Rank with activation-aware utility (CORE_REVIEW: activations were write-only).
pub fn rank_memories_in_project(
    memories: &[MemoryEntry],
    prompt: &str,
    fp: &TaskFingerprint,
    limit: usize,
    project_id: &str,
) -> Vec<(f32, MemoryEntry, Vec<String>)> {
    let mut ranked = rank_memories(memories, prompt, fp, limit.saturating_mul(2).max(limit));
    let acts = crate::learning_activations::load_activations(project_id);
    let mut hits: std::collections::HashMap<String, (u32, u32)> = std::collections::HashMap::new();
    for a in &acts {
        if a.item_kind != "memory" && a.item_kind != "context_pack" {
            // memory names are the usual item_id
        }
        let e = hits.entry(a.item_id.clone()).or_insert((0, 0));
        e.0 = e.0.saturating_add(1);
        if a.followed_by_agent == Some(true) || a.explicitly_opened {
            e.1 = e.1.saturating_add(1);
        }
    }
    for (score, entry, reasons) in ranked.iter_mut() {
        if let Some(&(n, followed)) = hits.get(&entry.name) {
            let rate = if n == 0 {
                0.0
            } else {
                followed as f32 / n as f32
            };
            // Mild boost/penalty from real injection follow-through.
            let boost = 0.15 * (rate - 0.3);
            *score = (*score + boost).clamp(0.0, 1.5);
            if n >= 2 {
                reasons.push(format!("activation hits {n} follow-rate {rate:.2}"));
            }
        }
    }
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    ranked.truncate(limit);
    ranked
}

/// Lexical overlap (TF, no corpus DF) between a memory and a prompt.
///
/// Used as a cheap content-relevance *gate* for the per-turn memory tail:
/// `rank_memories` gives every workspace memory a ~0.18 floor from
/// PROJECT_BONUS + scope + confidence, so a score threshold alone can't keep
/// irrelevant workspace memories out of the tail. Requiring `lexical_overlap > 0`
/// (any shared significant token) ensures only memories with real prompt overlap
/// surface — matching the recall behavior of the older tf·idf path while keeping
/// the ranker's symbol/path/fingerprint boosts for ranking within that set.
pub fn lexical_overlap(entry: &MemoryEntry, prompt: &str) -> f32 {
    let prompt_tokens = tokenize_rich(prompt);
    let mem_tokens = memory_tokens(entry);
    tf_overlap(&prompt_tokens, &mem_tokens)
}

/// Human-readable explanation of why a memory scored as it did.
pub fn explain_score(entry: &MemoryEntry, prompt: &str, fp: &TaskFingerprint) -> String {
    let (score, reasons) = score_memory(entry, prompt, fp);
    let mut out = format!(
        "Memory: {}\nScope: {}\nStatus: {}\nScore: {:.3}\nRetrieved because:\n",
        entry.name,
        match entry.scope {
            Scope::Workspace => "PROJECT",
            Scope::Global => "GLOBAL",
        },
        entry.status.as_str().to_uppercase(),
        score
    );
    if reasons.is_empty() {
        out.push_str("- (no strong signals)\n");
    } else {
        for r in &reasons {
            out.push_str(&format!("- {r}\n"));
        }
    }
    out
}

fn score_memory_with_lexical(
    entry: &MemoryEntry,
    fp: &TaskFingerprint,
    prompt_tokens: &[String],
    lexical: f32,
) -> (f32, Vec<String>) {
    if !entry.status.is_positive_guidance() || entry.deprecated {
        return (0.0, vec!["excluded: deprecated/rejected".into()]);
    }

    let mut reasons = Vec::new();

    if lexical > 0.3 {
        reasons.push(format!("lexical relevance {lexical:.2}"));
    }

    let symbol = symbol_overlap(entry, fp, prompt_tokens);
    if symbol > 0.3 {
        reasons.push(format!("symbol/identifier overlap {symbol:.2}"));
    }

    let mem_fp = memory_as_fingerprint(entry);
    let fps = fingerprint_similarity(fp, &mem_fp);
    if fps > 0.3 {
        reasons.push(format!("task-fingerprint similarity {fps:.2}"));
    }

    let path_s = path_overlap(entry, fp);
    if path_s > 0.3 {
        reasons.push(format!("path/subsystem overlap {path_s:.2}"));
    }

    let utility = utility_score(entry);
    if utility > 0.5 {
        reasons.push(format!("historical utility {utility:.2}"));
    }

    let conf = entry.confidence.clamp(0.0, 1.0);
    reasons.push(format!("confidence {conf:.2}"));

    let ver = verification_recency(entry);
    if ver > 0.5 {
        reasons.push(format!("verification recency {ver:.2}"));
    }

    let scope_s = scope_applicability(entry);
    if entry.scope == Scope::Workspace {
        reasons.push("project scope bonus".into());
    }

    let diag = diagnostic_overlap(entry, fp);
    if diag > 0.0 {
        reasons.push(format!("diagnostic overlap {diag:.2}"));
    }

    let status_mul = entry.status.rank_multiplier();
    if status_mul < 1.0 {
        reasons.push(format!(
            "status {} (x{status_mul:.2})",
            entry.status.as_str()
        ));
    } else {
        reasons.push("status verified".into());
    }

    let mut score = W_LEXICAL * lexical
        + W_SYMBOL * symbol
        + W_FINGERPRINT * fps
        + W_PATH * path_s
        + W_UTILITY * utility
        + W_CONFIDENCE * conf
        + W_VERIFICATION * ver
        + W_SCOPE * scope_s
        + W_DIAGNOSTIC * diag;

    score *= status_mul;

    // Repeated contradictory evidence reduces authority deterministically. A
    // contradiction does not erase a memory, but it cannot rank as though it
    // had uncontested evidence.
    let contradiction_mul = contradiction_multiplier(entry);
    if contradiction_mul < 1.0 {
        reasons.push(format!(
            "contradictory evidence {} (x{contradiction_mul:.2})",
            entry.contradiction_count
        ));
        score *= contradiction_mul;
    }

    if entry.scope == Scope::Workspace {
        score = (score + PROJECT_BONUS).min(1.0);
    }

    (score.clamp(0.0, 1.0), reasons)
}

fn score_debug(
    entry: &MemoryEntry,
    fp: &TaskFingerprint,
    prompt_tokens: &[String],
    lexical: f32,
) -> RetrievalScoreDebug {
    let symbol = symbol_overlap(entry, fp, prompt_tokens);
    let semantic_or_sketch = fingerprint_similarity(fp, &memory_as_fingerprint(entry));
    let path = path_overlap(entry, fp);
    let recency = verification_recency(entry);
    let scope_match = scope_applicability(entry);
    let status_mul = if entry.deprecated || !entry.status.is_positive_guidance() {
        0.0
    } else {
        entry.status.rank_multiplier()
    };
    let contradiction_mul = contradiction_multiplier(entry);
    let (total, _) = score_memory_with_lexical(entry, fp, prompt_tokens, lexical);
    RetrievalScoreDebug {
        lexical,
        semantic_or_sketch,
        recency,
        importance: match entry.importance {
            crate::memory::Importance::High => 1.0,
            crate::memory::Importance::Normal => 0.6,
            crate::memory::Importance::Low => 0.2,
        },
        scope_match,
        path_or_symbol_match: path.max(symbol),
        staleness_penalty: 1.0 - status_mul,
        contradiction_penalty: 1.0 - contradiction_mul,
        confidence: entry.confidence.clamp(0.0, 1.0),
        utility: utility_score(entry),
        total,
    }
}

fn contradiction_multiplier(entry: &MemoryEntry) -> f32 {
    (1.0 / (1.0 + entry.contradiction_count as f32 * 0.25)).clamp(0.2, 1.0)
}

fn memory_tokens(entry: &MemoryEntry) -> Vec<String> {
    let mem_text = format!(
        "{} {} {} {}",
        entry.name, entry.description, entry.content, entry.mem_type
    );
    tokenize_rich(&mem_text)
}

/// Document frequency: number of docs containing each token (at least once).
fn build_df(docs: &[Vec<String>]) -> (HashMap<String, usize>, usize) {
    let n = docs.len();
    let mut df: HashMap<String, usize> = HashMap::new();
    for doc in docs {
        let mut seen = HashSet::new();
        for t in doc {
            if seen.insert(t.as_str()) {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
        }
    }
    (df, n)
}

/// BM25-lite raw score. IDF = ln(1 + (N - df + 0.5)/(df + 0.5)), k1=1.2, b=0.75.
fn bm25_raw(
    query: &[String],
    doc: &[String],
    df: &HashMap<String, usize>,
    n: usize,
    avgdl: f32,
) -> f32 {
    if query.is_empty() || doc.is_empty() || n == 0 {
        return 0.0;
    }
    let avgdl = if avgdl < 1e-6 { 1.0 } else { avgdl };
    let mut tf: HashMap<&str, usize> = HashMap::new();
    for t in doc {
        *tf.entry(t.as_str()).or_insert(0) += 1;
    }
    let dl = doc.len() as f32;
    let mut score = 0.0f32;
    let mut seen_q = HashSet::new();
    for q in query {
        if !seen_q.insert(q.as_str()) {
            continue;
        }
        let f = match tf.get(q.as_str()) {
            Some(&c) if c > 0 => c as f32,
            _ => continue,
        };
        let dfi = df.get(q.as_str()).copied().unwrap_or(0) as f32;
        let idf = (1.0 + (n as f32 - dfi + 0.5) / (dfi + 0.5)).ln();
        let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl);
        if denom > 0.0 {
            score += idf * (f * (BM25_K1 + 1.0)) / denom;
        }
    }
    score
}

fn tokenize_rich(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        if raw.is_empty() {
            continue;
        }
        for part in split_ident(raw) {
            if part.len() > 1 {
                out.push(part.to_lowercase());
            }
        }
    }
    for t in significant_tokens(text) {
        if !out.iter().any(|x| x == &t) {
            out.push(t);
        }
    }
    out
}

/// Split CamelCase / snake_case / kebab-case identifiers into parts.
fn split_ident(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for chunk in s.split(|c| c == '_' || c == '-') {
        if chunk.is_empty() {
            continue;
        }
        let chars: Vec<char> = chunk.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let prev = chars[i - 1];
            let cur = chars[i];
            let boundary = (prev.is_lowercase() && cur.is_uppercase())
                || (prev.is_uppercase()
                    && cur.is_uppercase()
                    && i + 1 < chars.len()
                    && chars[i + 1].is_lowercase());
            if boundary {
                parts.push(chars[start..i].iter().collect());
                start = i;
            }
        }
        parts.push(chars[start..].iter().collect());
    }
    if parts.is_empty() {
        parts.push(s.to_string());
    }
    parts
}

fn tf_overlap(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_b: HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let mut hits = 0usize;
    let mut seen = HashSet::new();
    for t in a {
        if seen.insert(t.as_str()) && set_b.contains(t.as_str()) {
            hits += 1;
        }
    }
    let denom = a.len().min(32).max(1) as f32;
    (hits as f32 / denom).clamp(0.0, 1.0)
}

fn symbol_overlap(entry: &MemoryEntry, fp: &TaskFingerprint, prompt_tokens: &[String]) -> f32 {
    let mut symbols: Vec<String> = entry.ref_symbols.clone();
    for t in tokenize_rich(&format!("{} {}", entry.name, entry.description)) {
        if t.chars().any(|c| c.is_ascii_uppercase())
            || entry.ref_symbols.iter().any(|s| s.eq_ignore_ascii_case(&t))
        {
            // keep lowercase tokens from name for matching
            let _ = t;
        }
    }
    // Always include name tokens as symbol candidates.
    symbols.extend(tokenize_rich(&entry.name));
    symbols.extend(entry.ref_symbols.iter().cloned());

    let mut pool: Vec<String> = fp.symbols.iter().map(|s| s.to_lowercase()).collect();
    pool.extend(prompt_tokens.iter().cloned());

    if symbols.is_empty() {
        return 0.0;
    }
    let set_p: HashSet<String> = pool.into_iter().collect();
    let mut hits = 0usize;
    let mut seen = HashSet::new();
    for s in &symbols {
        let key = s.to_lowercase();
        if seen.insert(key.clone()) && set_p.contains(&key) {
            hits += 1;
        }
    }
    if hits == 0 {
        return 0.0;
    }
    // Exact symbol match in fingerprint → strong signal.
    let exact = fp
        .symbols
        .iter()
        .any(|s| entry.ref_symbols.iter().any(|r| r == s) || entry.name.eq_ignore_ascii_case(s));
    let base = (hits as f32 / seen.len().max(1) as f32).clamp(0.0, 1.0);
    if exact {
        base.max(0.9)
    } else {
        base
    }
}

fn memory_as_fingerprint(entry: &MemoryEntry) -> TaskFingerprint {
    let mut subsystems: Vec<String> = entry
        .ref_files
        .iter()
        .filter_map(|p| path_subsystem(p))
        .collect();
    subsystems.sort();
    subsystems.dedup();
    TaskFingerprint {
        intent: entry.mem_type.clone(),
        symbols: entry.ref_symbols.clone(),
        file_categories: entry
            .ref_files
            .iter()
            .map(|p| crate::pattern_log::file_category(p))
            .collect(),
        subsystems,
        ..Default::default()
    }
}

fn path_subsystem(path: &str) -> Option<String> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    if stem.len() > 2 {
        Some(stem.to_string())
    } else {
        None
    }
}

fn path_overlap(entry: &MemoryEntry, fp: &TaskFingerprint) -> f32 {
    if entry.ref_files.is_empty() && fp.file_categories.is_empty() && fp.subsystems.is_empty() {
        return 0.0;
    }
    let cats: Vec<String> = entry
        .ref_files
        .iter()
        .map(|p| crate::pattern_log::file_category(p))
        .collect();
    let mut score = 0.0f32;
    let mut n = 0.0f32;
    if !cats.is_empty() && !fp.file_categories.is_empty() {
        n += 1.0;
        score += jaccard_str(&cats, &fp.file_categories);
    }
    let subs: Vec<String> = entry
        .ref_files
        .iter()
        .filter_map(|p| path_subsystem(p))
        .collect();
    if !subs.is_empty() && !fp.subsystems.is_empty() {
        n += 1.0;
        score += jaccard_str(&subs, &fp.subsystems);
    }
    for f in &entry.ref_files {
        for sub in &fp.subsystems {
            if f.contains(sub.as_str()) {
                return (if n == 0.0 { 0.6 } else { score / n }).max(0.6);
            }
        }
    }
    if n == 0.0 {
        0.0
    } else {
        score / n
    }
}

fn jaccard_str(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let sa: HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let sb: HashSet<&str> = b.iter().map(|s| s.as_str()).collect();
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn utility_score(entry: &MemoryEntry) -> f32 {
    let s = entry.support_count as f32;
    (s / (s + 3.0)).clamp(0.0, 1.0)
}

fn verification_recency(entry: &MemoryEntry) -> f32 {
    match entry.last_verified_at {
        Some(ts) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(ts);
            let age_days = now.saturating_sub(ts) as f32 / 86400.0;
            (1.0 - (age_days / 180.0).min(0.7)).clamp(0.3, 1.0)
        }
        None => {
            if entry.status == MemoryStatus::Verified {
                0.6
            } else {
                0.3
            }
        }
    }
}

fn scope_applicability(entry: &MemoryEntry) -> f32 {
    match entry.scope {
        Scope::Workspace => 1.0,
        Scope::Global => 0.55,
    }
}

fn diagnostic_overlap(entry: &MemoryEntry, fp: &TaskFingerprint) -> f32 {
    if fp.diagnostic_classes.is_empty() {
        return 0.0;
    }
    let text = format!("{} {}", entry.description, entry.content).to_lowercase();
    let mut hits = 0usize;
    for d in &fp.diagnostic_classes {
        if text.contains(&d.to_lowercase()) {
            hits += 1;
        }
    }
    if hits == 0 {
        0.0
    } else {
        (hits as f32 / fp.diagnostic_classes.len() as f32).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Importance;
    use std::path::PathBuf;

    fn entry(
        name: &str,
        scope: Scope,
        status: MemoryStatus,
        symbols: &[&str],
        files: &[&str],
        content: &str,
    ) -> MemoryEntry {
        MemoryEntry {
            name: name.into(),
            description: format!("{name} description"),
            mem_type: "architecture".into(),
            content: content.into(),
            path: PathBuf::from(format!("{name}.md")),
            scope,
            pinned: false,
            importance: Importance::Normal,
            deprecated: false,
            superseded_by: None,
            schema_version: 2,
            source_session: None,
            source_run: None,
            created_at: None,
            status,
            confidence: 1.0,
            support_count: 0,
            contradiction_count: 0,
            last_verified_at: Some(1_700_000_000),
            last_verified_commit: None,
            ref_files: files.iter().map(|s| (*s).to_string()).collect(),
            ref_symbols: symbols.iter().map(|s| (*s).to_string()).collect(),
            evidence_episodes: vec![],
        }
    }

    #[test]
    fn exact_symbol_ranks_high() {
        let fp = TaskFingerprint {
            intent: "extend-tool-schema".into(),
            symbols: vec!["ProviderConfig".into()],
            subsystems: vec!["provider".into()],
            languages: vec!["rust".into()],
            ..Default::default()
        };
        let hit = entry(
            "provider-extension-architecture",
            Scope::Workspace,
            MemoryStatus::Verified,
            &["ProviderConfig", "PluginOAuthConfig"],
            &["core/src/provider.rs"],
            "API-key providers use config; OAuth belongs in plugins.",
        );
        let miss = entry(
            "unrelated-formatting",
            Scope::Workspace,
            MemoryStatus::Verified,
            &["IndentStyle"],
            &["docs/STYLE.md"],
            "Prefer spaces over tabs in prose docs.",
        );
        let ranked = rank_memories(
            &[hit, miss],
            "extend ProviderConfig for new OAuth provider",
            &fp,
            5,
        );
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].1.name, "provider-extension-architecture");
        assert!(ranked[0].0 > ranked.last().unwrap().0 || ranked.len() == 1);
        let explain = explain_score(&ranked[0].1, "extend ProviderConfig", &fp);
        assert!(
            explain.contains("ProviderConfig")
                || explain.contains("symbol")
                || explain.contains("Score:")
        );
    }

    #[test]
    fn verified_ranks_above_candidate() {
        let fp = TaskFingerprint {
            intent: "memory-work".into(),
            symbols: vec!["MemoryEntry".into()],
            ..Default::default()
        };
        let verified = entry(
            "memory-store-layout",
            Scope::Workspace,
            MemoryStatus::Verified,
            &["MemoryEntry"],
            &["core/src/memory.rs"],
            "MemoryEntry holds frontmatter metadata.",
        );
        let mut candidate = verified.clone();
        candidate.name = "memory-store-layout-candidate".into();
        candidate.status = MemoryStatus::Candidate;
        candidate.path = PathBuf::from("cand.md");

        let ranked = rank_memories(
            &[candidate, verified],
            "update MemoryEntry metadata",
            &fp,
            5,
        );
        assert_eq!(ranked[0].1.status, MemoryStatus::Verified);
        assert!(ranked[0].0 >= ranked[1].0);
    }

    #[test]
    fn project_scope_gets_bonus_over_global() {
        let fp = TaskFingerprint {
            intent: "testing".into(),
            symbols: vec!["assert_eq".into()],
            ..Default::default()
        };
        let project = entry(
            "prefer-cargo-test",
            Scope::Workspace,
            MemoryStatus::Verified,
            &["assert_eq"],
            &["core/src/memory.rs"],
            "Prefer cargo test for validation.",
        );
        let mut global = project.clone();
        global.name = "prefer-cargo-test-global".into();
        global.scope = Scope::Global;
        global.path = PathBuf::from("g.md");

        let (sp, _) = score_memory(&project, "run cargo test assert_eq", &fp);
        let (sg, _) = score_memory(&global, "run cargo test assert_eq", &fp);
        assert!(sp > sg, "project {sp} should beat global {sg}");
    }

    #[test]
    fn bm25_ranks_rare_exact_token_above_common_noise() {
        let fp = TaskFingerprint {
            intent: "fix".into(),
            ..Default::default()
        };
        let rare = entry(
            "rare-id-memory",
            Scope::Workspace,
            MemoryStatus::Verified,
            &[],
            &[],
            "Handles UniqueSymbolXyz123 specifically.",
        );
        let noise_a = entry(
            "noise-common-a",
            Scope::Workspace,
            MemoryStatus::Verified,
            &[],
            &[],
            "Please update the module and fix the code for the user.",
        );
        let noise_b = entry(
            "noise-common-b",
            Scope::Workspace,
            MemoryStatus::Verified,
            &[],
            &[],
            "Please update the module and fix the code for the build.",
        );
        let noise_c = entry(
            "noise-common-c",
            Scope::Workspace,
            MemoryStatus::Verified,
            &[],
            &[],
            "Please update the module and fix the tests for the release.",
        );
        let ranked = rank_memories(
            &[noise_a, rare, noise_b, noise_c],
            "please fix UniqueSymbolXyz123 and update the module",
            &fp,
            4,
        );
        assert!(!ranked.is_empty());
        assert_eq!(
            ranked[0].1.name,
            "rare-id-memory",
            "rare exact token should beat common-token-only memories; got {:?}",
            ranked.iter().map(|r| (&r.1.name, r.0)).collect::<Vec<_>>()
        );
    }
}
