//! Test-only reference implementation of the deep-research **evidence-ledger
//! contract** that `prompts/deep-research.md` instructs the agent to produce.
//!
//! The research *orchestration* (scoping, subagent fan-out, gap analysis) is
//! prompt-driven and only exercisable with a mock-model turn harness the repo
//! does not yet ship. The deterministic, well-defined sub-routines the prompt
//! relies on — URL canonicalization, source deduplication, citation
//! verification, coverage analysis, and stopping-rule evaluation — are encoded
//! here as pure functions and validated with no network access. This pins down
//! the exact semantics the prompt describes in prose (e.g. "canonicalize URLs",
//! "snippets are not evidence", "preserve contradictory evidence") so the
//! contract is unambiguous and implementable.
//!
//! Compiled only under `cfg(test)`; it adds no production code path.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

/// How a source's claim relates to a research proposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Supports,
    Contradicts,
    PartiallySupports,
    ContextOnly,
}

impl Support {
    pub fn as_str(&self) -> &'static str {
        match self {
            Support::Supports => "supports",
            Support::Contradicts => "contradicts",
            Support::PartiallySupports => "partially-supports",
            Support::ContextOnly => "context-only",
        }
    }
}

/// A normalized factual claim extracted from a source.
#[derive(Clone, Debug)]
pub struct Claim {
    pub claim: String,
    pub support: Support,
    pub excerpt: String,
    pub location: Option<String>,
    pub confidence: f64,
}

/// One source record in the evidence ledger (mirrors
/// `templates/evidence-record.json`).
#[derive(Clone, Debug)]
pub struct EvidenceSource {
    pub id: String,
    pub url: String,
    pub canonical_url: String,
    pub title: String,
    pub publisher: Option<String>,
    pub published_at: Option<String>, // YYYY-MM-DD or None when unverifiable
    pub accessed_at: String,
    pub source_type: SourceType,
    pub primary_source: bool,
    pub quality_score: u8, // 1..=5
    pub worker_id: String,
    pub fetched: bool, // false for a discovery snippet only
    pub claims: Vec<Claim>,
    pub notes: Option<String>,
}

/// Source-type taxonomy, ordered by the preference ranking in the prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceType {
    OriginalResearch,
    OfficialDocumentation,
    GovernmentRegulatory,
    StandardsBody,
    CourtLegislation,
    CompanyFilingAnnouncement,
    MaintainerRepositoryRelease,
    ReputableJournalism,
    TechnicalAnalysis,
    CommunitySocial,
    WorkspaceArtifact,
}

impl SourceType {
    /// Preference rank: 1 = most preferred. Lower is better.
    pub fn preference_rank(&self) -> u8 {
        match self {
            SourceType::OriginalResearch => 1,
            SourceType::OfficialDocumentation => 2,
            SourceType::GovernmentRegulatory => 3,
            SourceType::StandardsBody => 4,
            SourceType::CourtLegislation => 5,
            SourceType::CompanyFilingAnnouncement => 6,
            SourceType::MaintainerRepositoryRelease => 7,
            SourceType::ReputableJournalism => 8,
            SourceType::TechnicalAnalysis => 9,
            SourceType::CommunitySocial => 10,
            SourceType::WorkspaceArtifact => 11,
        }
    }
}

/// Canonicalize a URL: lowercase scheme + host, drop the fragment, strip common
/// tracking query params, normalize a trailing slash on the path, and drop an
/// empty query string. Syndicated/mirrored content that differs only by tracking
/// params collapses to the same canonical URL.
pub fn canonicalize_url(url: &str) -> String {
    let url = url.trim();
    // Split off the fragment.
    let url = url.split('#').next().unwrap_or(url);
    let (scheme_rest, query) = match url.split_once('?') {
        Some((rest, q)) => (rest, Some(q)),
        None => (url, None),
    };
    let (scheme, host_path) = match scheme_rest.split_once("://") {
        Some((s, hp)) => (s.to_ascii_lowercase(), hp.to_string()),
        None => (String::new(), scheme_rest.to_string()),
    };
    // Lowercase the host (up to the first '/'), leave the path case-sensitive.
    let (host, path) = match host_path.split_once('/') {
        Some((h, p)) => (h.to_ascii_lowercase(), format!("/{p}")),
        None => (host_path.to_ascii_lowercase(), String::new()),
    };
    // Normalize a trailing slash on the path (including root "/" -> "").
    let path = if path.ends_with('/') {
        path[..path.len() - 1].to_string()
    } else {
        path
    };

    let filtered_query = filter_tracking_params(query);
    let mut out = if scheme.is_empty() {
        format!("{host}{path}")
    } else {
        format!("{scheme}://{host}{path}")
    };
    if let Some(q) = filtered_query {
        if !q.is_empty() {
            out.push('?');
            out.push_str(&q);
        }
    }
    out
}

fn filter_tracking_params(query: Option<&str>) -> Option<String> {
    let q = query?;
    let drop: &[&str] = &[
        "utm_source",
        "utm_medium",
        "utm_campaign",
        "utm_term",
        "utm_content",
        "gclid",
        "fbclid",
        "mc_cid",
        "mc_eid",
        "ref",
        "source",
        "igshid",
    ];
    let drop_set: HashSet<&str> = drop.iter().copied().collect();
    let kept: Vec<&str> = q
        .split('&')
        .filter(|kv| {
            let key = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
            !drop_set.contains(key.as_str())
        })
        .collect();
    Some(kept.join("&"))
}

/// Deduplicate sources by canonical URL, retaining the strongest record.
/// Fetched sources always beat discovery snippets; quality and primary-source
/// status break ties. Claims from every duplicate are merged so contradictions
/// remain visible.
pub fn dedup_sources(sources: &[EvidenceSource]) -> Vec<EvidenceSource> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<EvidenceSource> = Vec::new();
    for s in sources {
        let key = if s.canonical_url.is_empty() {
            canonicalize_url(&s.url)
        } else {
            s.canonical_url.clone()
        };
        if let Some(&idx) = seen.get(&key) {
            let kept = &mut out[idx];
            let stronger = (
                s.fetched,
                s.quality_score,
                s.primary_source,
                std::cmp::Reverse(s.source_type.preference_rank()),
            ) > (
                kept.fetched,
                kept.quality_score,
                kept.primary_source,
                std::cmp::Reverse(kept.source_type.preference_rank()),
            );
            if stronger {
                let mut replacement = s.clone();
                replacement.claims = kept.claims.clone();
                *kept = replacement;
            }
            for c in &s.claims {
                if !kept
                    .claims
                    .iter()
                    .any(|k| k.claim == c.claim && k.support == c.support)
                {
                    kept.claims.push(c.clone());
                }
            }
            continue;
        }
        seen.insert(key, out.len());
        out.push(s.clone());
    }
    out
}

/// The result of verifying a single citation against the ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationResult {
    /// The cited source exists, was fetched, and supports the claim.
    Verified,
    /// The cited source id does not exist in the ledger (a fabricated citation).
    SourceMissing,
    /// The source exists but was never fetched (snippet-only) — not evidence.
    NotFetched,
    /// The source exists and was fetched but has no claim supporting the
    /// proposition (citation stretched across an unrelated claim).
    NotSupported,
}

/// Verify that a citation is backed by a fetched source that actually supports
/// the proposition. `support_needed` is typically `Support::Supports`; pass
/// `Support::Contradicts` to verify a counter-citation.
pub fn verify_citation(
    ledger: &[EvidenceSource],
    cited_source_id: &str,
    support_needed: Support,
) -> VerificationResult {
    let Some(src) = ledger.iter().find(|s| s.id == cited_source_id) else {
        return VerificationResult::SourceMissing;
    };
    if !src.fetched {
        return VerificationResult::NotFetched;
    }
    let ok = src.claims.iter().any(|c| {
        c.support == support_needed
            || (support_needed == Support::Supports && c.support == Support::PartiallySupports)
    });
    if ok {
        VerificationResult::Verified
    } else {
        VerificationResult::NotSupported
    }
}

/// Coverage state for one research question.
#[derive(Clone, Debug, Default)]
pub struct QuestionCoverage {
    pub question: String,
    pub answered: bool,
    pub partial: bool,
    pub supporting_sources: usize,
    pub contradicting_sources: usize,
    pub weak_only: bool, // supported by only one weak source
}

impl QuestionCoverage {
    pub fn status(&self) -> &'static str {
        if self.answered {
            "answered"
        } else if self.partial {
            "partial"
        } else {
            "unanswered"
        }
    }
}

/// Detect overrepresented domains in the ledger (one domain dominating).
pub fn overrepresented_domains(sources: &[EvidenceSource], threshold: f64) -> Vec<(String, f64)> {
    let total = sources.len().max(1);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in sources {
        if let Some(host) = host_of(&s.canonical_url) {
            *counts.entry(host).or_insert(0) += 1;
        }
    }
    let mut out: Vec<(String, f64)> = counts
        .into_iter()
        .map(|(h, c)| (h, c as f64 / total as f64))
        .filter(|(_, frac)| *frac >= threshold)
        .collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Inputs to the stopping-rule evaluation.
#[derive(Clone, Debug)]
pub struct StopInputs {
    pub all_must_answer_answered: bool,
    pub critical_claims_supported: bool,
    pub contradictions_investigated: bool,
    /// New sources found in each of the last two iterations.
    pub new_sources_last_two_iters: [usize; 2],
    pub source_count: usize,
    pub max_sources: usize,
    pub iteration: usize,
    pub max_iterations: usize,
    pub budget_exhausted: bool,
}

/// The reason research stopped (or should continue).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopDecision {
    Continue,
    AllAnswered,
    CriticalClaimsSupported,
    ContradictionsInvestigated,
    NoveltyExhausted,
    SourceBudgetReached,
    IterationBudgetReached,
    BudgetExhausted,
}

/// Evaluate the stopping rules from §7 of the protocol. Returns the first
/// matching stop reason, or `Continue`.
pub fn evaluate_stopping(i: &StopInputs) -> StopDecision {
    if i.all_must_answer_answered {
        return StopDecision::AllAnswered;
    }
    if i.critical_claims_supported && i.contradictions_investigated {
        return StopDecision::CriticalClaimsSupported;
    }
    // Novelty-based: two consecutive low-yield iterations.
    if i.new_sources_last_two_iters[0] <= 1 && i.new_sources_last_two_iters[1] <= 1 {
        return StopDecision::NoveltyExhausted;
    }
    if i.budget_exhausted {
        return StopDecision::BudgetExhausted;
    }
    if i.source_count >= i.max_sources {
        return StopDecision::SourceBudgetReached;
    }
    if i.iteration >= i.max_iterations {
        return StopDecision::IterationBudgetReached;
    }
    StopDecision::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, url: &str, fetched: bool, claims: Vec<Claim>) -> EvidenceSource {
        EvidenceSource {
            id: id.to_string(),
            canonical_url: canonicalize_url(url),
            url: url.to_string(),
            title: format!("Source {id}"),
            publisher: None,
            published_at: Some("2026-07-01".to_string()),
            accessed_at: "2026-07-24T20:00:00-04:00".to_string(),
            source_type: SourceType::OfficialDocumentation,
            primary_source: true,
            quality_score: 4,
            worker_id: "researcher-1".to_string(),
            fetched,
            claims,
            notes: None,
        }
    }

    fn claim(text: &str, support: Support) -> Claim {
        Claim {
            claim: text.to_string(),
            support,
            excerpt: format!("...{text}..."),
            location: None,
            confidence: 0.9,
        }
    }

    // ---- canonicalization ----

    #[test]
    fn canonicalize_strips_tracking_params_and_fragment() {
        let a = canonicalize_url("https://Example.com/vLLM?utm_source=x&keep=1#sec");
        let b = canonicalize_url("https://example.com/vLLM?keep=1");
        assert_eq!(a, b);
        assert_eq!(a, "https://example.com/vLLM?keep=1");
    }

    #[test]
    fn canonicalize_normalizes_scheme_host_and_trailing_slash() {
        assert_eq!(
            canonicalize_url("HTTPS://Docs.VLLM.AI/Path/"),
            "https://docs.vllm.ai/Path"
        );
        assert_eq!(canonicalize_url("https://x.com/"), "https://x.com");
    }

    // ---- deduplication (duplicate + syndicated sources) ----

    #[test]
    fn dedup_collapses_duplicate_canonical_urls() {
        let s1 = src(
            "source-001",
            "https://x.com/a?utm_campaign=c",
            true,
            vec![claim("vLLM is fast", Support::Supports)],
        );
        let s2 = src(
            "source-002",
            "https://x.com/a",
            true,
            vec![claim("vLLM is fast", Support::Supports)],
        );
        let out = dedup_sources(&[s1, s2]);
        assert_eq!(out.len(), 1, "syndicated duplicate collapsed");
    }

    #[test]
    fn dedup_merges_claims_without_dropping_contradictions() {
        let s1 = src(
            "source-001",
            "https://x.com/a",
            true,
            vec![claim("X is fast", Support::Supports)],
        );
        let s2 = src(
            "source-002",
            "https://x.com/a",
            true,
            vec![claim("X is slow", Support::Contradicts)],
        );
        let out = dedup_sources(&[s1, s2]);
        assert_eq!(out.len(), 1);
        let supports = out[0]
            .claims
            .iter()
            .filter(|c| c.support == Support::Supports)
            .count();
        let contradicts = out[0]
            .claims
            .iter()
            .filter(|c| c.support == Support::Contradicts)
            .count();
        assert_eq!(supports, 1);
        assert_eq!(contradicts, 1, "contradictory evidence preserved");
    }

    #[test]
    fn dedup_retains_fetched_high_quality_source() {
        let snippet = src("snippet", "https://x.com/q", false, vec![]);
        let mut fetched = src(
            "fetched",
            "https://x.com/q",
            true,
            vec![claim("p", Support::Supports)],
        );
        fetched.quality_score = 5;
        let out = dedup_sources(&[snippet, fetched]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "fetched");
        assert!(out[0].fetched);
    }

    // ---- citation verification ----

    #[test]
    fn verify_rejects_fabricated_citation() {
        let ledger = [src(
            "source-001",
            "https://x.com/a",
            true,
            vec![claim("p", Support::Supports)],
        )];
        assert_eq!(
            verify_citation(&ledger, "source-999", Support::Supports),
            VerificationResult::SourceMissing
        );
    }

    #[test]
    fn verify_rejects_snippet_only_source() {
        // A discovery snippet that was never fetched is not evidence.
        let ledger = [src(
            "source-001",
            "https://x.com/a",
            false,
            vec![claim("p", Support::Supports)],
        )];
        assert_eq!(
            verify_citation(&ledger, "source-001", Support::Supports),
            VerificationResult::NotFetched
        );
    }

    #[test]
    fn verify_rejects_citation_stretched_across_unrelated_claim() {
        let ledger = [src(
            "source-001",
            "https://x.com/a",
            true,
            vec![claim("about topic A", Support::ContextOnly)],
        )];
        assert_eq!(
            verify_citation(&ledger, "source-001", Support::Supports),
            VerificationResult::NotSupported
        );
    }

    #[test]
    fn verify_accepts_well_supported_citation() {
        let ledger = [src(
            "source-001",
            "https://x.com/a",
            true,
            vec![claim("p", Support::Supports)],
        )];
        assert_eq!(
            verify_citation(&ledger, "source-001", Support::Supports),
            VerificationResult::Verified
        );
    }

    #[test]
    fn verify_accepts_partial_support_for_supports_need() {
        let ledger = [src(
            "source-001",
            "https://x.com/a",
            true,
            vec![claim("p", Support::PartiallySupports)],
        )];
        assert_eq!(
            verify_citation(&ledger, "source-001", Support::Supports),
            VerificationResult::Verified
        );
    }

    // ---- coverage / overrepresentation ----

    #[test]
    fn coverage_status_reflects_answered_partial_unanswered() {
        let mut q = QuestionCoverage {
            question: "q".into(),
            answered: false,
            partial: false,
            supporting_sources: 0,
            contradicting_sources: 0,
            weak_only: false,
        };
        assert_eq!(q.status(), "unanswered");
        q.partial = true;
        assert_eq!(q.status(), "partial");
        q.answered = true;
        assert_eq!(q.status(), "answered");
    }

    #[test]
    fn overrepresentation_detects_dominant_domain() {
        let mut sources = vec![];
        for i in 0..8 {
            sources.push(src(
                &format!("source-{i:03}"),
                &format!("https://medium.com/p{i}"),
                true,
                vec![],
            ));
        }
        for i in 0..2 {
            sources.push(src(
                &format!("source-{i:03}b"),
                &format!("https://other.com/p{i}"),
                true,
                vec![],
            ));
        }
        let dom = overrepresented_domains(&sources, 0.5);
        assert_eq!(dom.len(), 1);
        assert_eq!(dom[0].0, "medium.com");
        assert!(dom[0].1 >= 0.5);
    }

    // ---- source quality ranking ----

    #[test]
    fn source_type_preference_rank_orders_primary_first() {
        assert!(
            SourceType::OriginalResearch.preference_rank()
                < SourceType::OfficialDocumentation.preference_rank()
        );
        assert!(
            SourceType::OfficialDocumentation.preference_rank()
                < SourceType::ReputableJournalism.preference_rank()
        );
        assert!(
            SourceType::ReputableJournalism.preference_rank()
                < SourceType::CommunitySocial.preference_rank()
        );
    }

    // ---- stopping rules ----

    #[test]
    fn stop_when_all_must_answer_answered() {
        let i = StopInputs {
            all_must_answer_answered: true,
            critical_claims_supported: false,
            contradictions_investigated: false,
            new_sources_last_two_iters: [5, 5],
            source_count: 1,
            max_sources: 100,
            iteration: 1,
            max_iterations: 5,
            budget_exhausted: false,
        };
        assert_eq!(evaluate_stopping(&i), StopDecision::AllAnswered);
    }

    #[test]
    fn stop_on_novelty_exhaustion() {
        // Two consecutive low-yield iterations -> stop even if not all answered.
        let i = StopInputs {
            all_must_answer_answered: false,
            critical_claims_supported: false,
            contradictions_investigated: false,
            new_sources_last_two_iters: [0, 1],
            source_count: 10,
            max_sources: 100,
            iteration: 2,
            max_iterations: 5,
            budget_exhausted: false,
        };
        assert_eq!(evaluate_stopping(&i), StopDecision::NoveltyExhausted);
    }

    #[test]
    fn stop_on_budget_exhaustion() {
        let i = StopInputs {
            all_must_answer_answered: false,
            critical_claims_supported: false,
            contradictions_investigated: false,
            new_sources_last_two_iters: [5, 5],
            source_count: 5,
            max_sources: 100,
            iteration: 1,
            max_iterations: 5,
            budget_exhausted: true,
        };
        assert_eq!(evaluate_stopping(&i), StopDecision::BudgetExhausted);
    }

    #[test]
    fn stop_on_source_budget_reached() {
        let i = StopInputs {
            all_must_answer_answered: false,
            critical_claims_supported: false,
            contradictions_investigated: false,
            new_sources_last_two_iters: [5, 5],
            source_count: 40,
            max_sources: 40,
            iteration: 1,
            max_iterations: 5,
            budget_exhausted: false,
        };
        assert_eq!(evaluate_stopping(&i), StopDecision::SourceBudgetReached);
    }

    #[test]
    fn continue_when_budgets_remain_and_novelty_high() {
        let i = StopInputs {
            all_must_answer_answered: false,
            critical_claims_supported: false,
            contradictions_investigated: false,
            new_sources_last_two_iters: [8, 6],
            source_count: 20,
            max_sources: 40,
            iteration: 2,
            max_iterations: 5,
            budget_exhausted: false,
        };
        assert_eq!(evaluate_stopping(&i), StopDecision::Continue);
    }

    // ---- source without a publication date ----

    #[test]
    fn source_with_unverifiable_publication_date_is_handled() {
        let mut s = src("source-001", "https://x.com/a", true, vec![]);
        s.published_at = None; // could not be verified
        s.notes = Some("publication date not stated on the page".into());
        assert!(s.published_at.is_none());
        assert!(s.notes.as_deref().unwrap().contains("publication date"));
    }
}
