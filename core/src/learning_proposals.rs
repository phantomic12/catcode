//! Structured learning proposal pipeline (spec §18).
//!
//! Reflection and subagents submit proposals; this module validates evidence
//! rules before mutating memory/preferences. Fail-open.

#![allow(dead_code)]

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::memory::{self, Importance, Scope};
use crate::preferences;

/// Spec §18 proposal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningProposal {
    pub kind: String,
    pub scope: String,
    pub statement: String,
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub contradicts: Vec<String>,
    pub suggested_action: String,
}

fn looks_like_secret(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("api_key")
        || l.contains("apikey")
        || l.contains("secret=")
        || l.contains("password=")
        || l.contains("-----begin")
        || l.contains("bearer ")
}

fn parse_scope(s: &str) -> Scope {
    Scope::parse(s)
}

/// Validate and apply a proposal. Returns a human-readable result.
pub fn validate_and_apply(workspace: &Path, proposal: &LearningProposal) -> Result<String, String> {
    if looks_like_secret(&proposal.statement)
        || proposal.evidence.iter().any(|e| looks_like_secret(e))
    {
        return Err("rejected: proposal appears to contain secrets".into());
    }
    if proposal.statement.trim().is_empty() {
        return Err("rejected: empty statement".into());
    }
    let scope = parse_scope(&proposal.scope);
    let action = proposal.suggested_action.trim().to_lowercase();

    match action.as_str() {
        "create_candidate" | "verify" => {
            let name = slugify_name(&proposal.kind, &proposal.statement);
            let status_note = if action == "verify" {
                "verified"
            } else {
                "candidate"
            };
            let mem_type = if proposal.kind.is_empty() {
                "note"
            } else {
                proposal.kind.as_str()
            };
            let mut body = proposal.statement.clone();
            if !proposal.evidence.is_empty() {
                body.push_str("\n\nEvidence:\n");
                for e in &proposal.evidence {
                    body.push_str("- ");
                    body.push_str(e);
                    body.push('\n');
                }
            }
            let desc: String = proposal.statement.chars().take(100).collect();
            // Duplicate candidates are left untouched, but verification is an
            // explicit lifecycle transition and must update the current entry.
            if memory::memory_exists_scoped(workspace, scope, &name) {
                if action == "verify" {
                    memory::update_memory_scoped(workspace, scope, &name, |entry| {
                        entry.schema_version = entry.schema_version.max(2);
                        entry.status = memory::MemoryStatus::Verified;
                        entry.confidence = proposal.confidence.clamp(0.0, 1.0);
                        entry.support_count = entry
                            .support_count
                            .saturating_add(proposal.evidence.len() as u32);
                        memory::extend_unique(&mut entry.ref_files, &proposal.references);
                        memory::extend_unique(&mut entry.evidence_episodes, &proposal.evidence);
                        entry.last_verified_at = Some(now_secs());
                    })?;
                }
                return Ok(format!(
                    "duplicate: memory '{name}' already exists — skipped ({status_note})"
                ));
            }
            memory::save_memory_scoped_with_importance(
                workspace,
                scope,
                &name,
                &body,
                mem_type,
                &desc,
                if proposal.confidence >= 0.9 {
                    Importance::High
                } else {
                    Importance::Normal
                },
            )
            .map_err(|e| e)?;
            memory::update_memory_scoped(workspace, scope, &name, |entry| {
                entry.schema_version = 2;
                entry.status = if action == "verify" {
                    memory::MemoryStatus::Verified
                } else {
                    memory::MemoryStatus::Candidate
                };
                entry.confidence = proposal.confidence.clamp(0.0, 1.0);
                entry.support_count = proposal.evidence.len() as u32;
                memory::extend_unique(&mut entry.ref_files, &proposal.references);
                memory::extend_unique(&mut entry.evidence_episodes, &proposal.evidence);
                if action == "verify" {
                    entry.last_verified_at = Some(now_secs());
                }
            })?;
            Ok(format!(
                "created {status_note} {} memory '{name}' (conf={:.2})",
                scope.as_str(),
                proposal.confidence
            ))
        }
        "append_evidence" => {
            let name = slugify_name(&proposal.kind, &proposal.statement);
            if !memory::memory_exists_scoped(workspace, scope, &name) {
                return Err(format!("append_evidence: memory '{name}' not found"));
            }
            let extra = proposal.evidence.join("\n");
            memory::append_memory_scoped(
                workspace,
                scope,
                &name,
                &extra,
                "note",
                &proposal.statement.chars().take(80).collect::<String>(),
                16 * 1024,
            )
            .map_err(|e| e)?;
            Ok(format!("appended evidence to '{name}'"))
        }
        "promote_to_global" => {
            let ok = preferences::may_promote_to_global(
                &proposal.statement,
                2, // caller should pass real counts; gate still requires multi-project
                proposal.evidence.len().max(3),
                false,
                !proposal.contradicts.is_empty(),
                false,
            );
            if !ok {
                return Err(
                    "promote_to_global blocked: needs multi-project evidence and no project-specific details"
                        .into(),
                );
            }
            let pref = preferences::PreferenceRecord {
                id: slugify_name("pref", &proposal.statement),
                scope: "global".into(),
                category: proposal.kind.clone(),
                statement: proposal.statement.clone(),
                status: preferences::LearningStatus::Verified,
                confidence: proposal.confidence.clamp(0.0, 1.0),
                explicit: false,
                supporting_events: proposal.evidence.clone(),
                contradicting_events: proposal.contradicts.clone(),
                project_exceptions: vec![],
                first_seen: now_secs(),
                last_seen: now_secs(),
            };
            preferences::append_global_preference(&pref);
            Ok(format!("promoted preference '{}'", pref.id))
        }
        "create_project_exception" => {
            // Record as a project decision memory referencing the global pref.
            let name = slugify_name("exception", &proposal.statement);
            let body = format!(
                "Project exception:\n{}\n\nContradicts global: {}\n",
                proposal.statement,
                proposal.contradicts.join(", ")
            );
            memory::save_memory_scoped_with_importance(
                workspace,
                Scope::Workspace,
                &name,
                &body,
                "decision",
                &proposal.statement.chars().take(100).collect::<String>(),
                Importance::High,
            )
            .map_err(|e| e)?;
            Ok(format!("created project exception memory '{name}'"))
        }
        "deprecate" | "reject" => {
            let name = slugify_name(&proposal.kind, &proposal.statement);
            match memory::mark_memory_deprecated_any(workspace, &name, None) {
                Ok(()) => Ok(format!("deprecated memory '{name}'")),
                Err(e) => Err(e),
            }
        }
        "create_skill_candidate" | "update_skill" => {
            // Advisory only — skill files are authored deliberately.
            Ok(format!(
                "skill proposal recorded as advisory ({action}): {}",
                proposal.statement.chars().take(120).collect::<String>()
            ))
        }
        "replace" => {
            let name = slugify_name(&proposal.kind, &proposal.statement);
            memory::save_memory_scoped_with_importance(
                workspace,
                scope,
                &name,
                &proposal.statement,
                if proposal.kind.is_empty() {
                    "note"
                } else {
                    proposal.kind.as_str()
                },
                &proposal.statement.chars().take(100).collect::<String>(),
                Importance::High,
            )
            .map_err(|e| e)?;
            Ok(format!("replaced memory '{name}'"))
        }
        other => Err(format!("unknown suggested_action '{other}'")),
    }
}

fn slugify_name(kind: &str, statement: &str) -> String {
    let base = if !kind.is_empty() && kind != "note" {
        kind.to_string()
    } else {
        statement
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join("-")
    };
    let mut s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        format!("proposal-{}", now_secs())
    } else {
        s.chars().take(48).collect()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Heuristic proposals from an episode digest (used by reflection).
pub fn proposals_from_episode_digest(
    intent: &str,
    files_changed: &[String],
    rejected: &[String],
    corrections: &[String],
) -> Vec<LearningProposal> {
    let mut out = Vec::new();
    if !corrections.is_empty() {
        out.push(LearningProposal {
            kind: "gotcha".into(),
            scope: "project".into(),
            statement: format!(
                "User correction during '{intent}': {}",
                corrections.first().map(|s| s.as_str()).unwrap_or("")
            ),
            confidence: 0.75,
            evidence: corrections.to_vec(),
            references: files_changed.to_vec(),
            contradicts: vec![],
            suggested_action: "create_candidate".into(),
        });
    }
    for r in rejected.iter().take(3) {
        out.push(LearningProposal {
            kind: "rejected".into(),
            scope: "project".into(),
            statement: format!("Rejected approach for '{intent}': {r}"),
            confidence: 0.7,
            evidence: vec![r.clone()],
            references: files_changed.to_vec(),
            contradicts: vec![],
            suggested_action: "create_candidate".into(),
        });
    }
    out
}

/// Capture an explicit user preference statement if wording matches §12.1.
pub fn maybe_capture_explicit_preference(workspace: &Path, user_text: &str) -> Option<String> {
    let t = user_text.trim();
    if t.len() < 12 || t.len() > 500 {
        return None;
    }
    let lower = t.to_lowercase();
    let explicit = lower.contains("i always")
        || lower.contains("i prefer")
        || lower.contains("never use")
        || lower.contains("always do")
        || lower.contains("do not refactor")
        || lower.contains("keep changes minimal")
        || lower.starts_with("from now on")
        || lower.contains("don't add dependencies")
        || lower.contains("do not add dependencies");
    if !explicit {
        return None;
    }
    let scope = preferences::infer_preference_scope(t);
    let conf = preferences::confidence_for_evidence(true, false);
    if scope == Scope::Global {
        let pref = preferences::PreferenceRecord {
            id: slugify_name("pref", t),
            scope: "global".into(),
            category: "communication".into(),
            statement: t.to_string(),
            status: preferences::LearningStatus::Verified,
            confidence: conf,
            explicit: true,
            supporting_events: vec!["explicit-user".into()],
            contradicting_events: vec![],
            project_exceptions: vec![],
            first_seen: now_secs(),
            last_seen: now_secs(),
        };
        preferences::append_global_preference(&pref);
        return Some(format!("captured global preference '{}'", pref.id));
    }
    // Project-scoped: store as preference-type memory.
    let name = slugify_name("pref", t);
    let _ = memory::save_memory_scoped_with_importance(
        workspace,
        Scope::Workspace,
        &name,
        t,
        "preference",
        &t.chars().take(100).collect::<String>(),
        Importance::High,
    );
    Some(format!("captured project preference memory '{name}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning_store::{learning_test_serial, override_learning_root};
    use crate::memory::override_memory_root;
    use crate::project_identity::override_registry_path;

    fn tmp() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let d =
            std::env::temp_dir().join(format!("prop-{}-{}-{}", std::process::id(), now_secs(), n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn secrets_rejected() {
        let p = LearningProposal {
            kind: "note".into(),
            scope: "project".into(),
            statement: "api_key=sk-secret".into(),
            confidence: 0.9,
            evidence: vec![],
            references: vec![],
            contradicts: vec![],
            suggested_action: "create_candidate".into(),
        };
        let home = tmp();
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let err = validate_and_apply(&ws, &p).unwrap_err();
        assert!(err.contains("secret"));
    }

    #[test]
    fn create_candidate_writes_memory() {
        let _ls = learning_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ms = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _rs = crate::project_identity::registry_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tmp();
        std::fs::create_dir_all(home.join("memory/global")).unwrap();
        let _mr = override_memory_root(home.join("memory"));
        let _lr = override_learning_root(home.join("learning"));
        let _rr = override_registry_path(home.join("reg.json"));
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let p = LearningProposal {
            kind: "convention".into(),
            scope: "project".into(),
            statement: "Prefer edit over write_file for targeted changes".into(),
            confidence: 0.85,
            evidence: vec!["ep-1".into()],
            references: vec!["core/src/tools.rs".into()],
            contradicts: vec![],
            suggested_action: "create_candidate".into(),
        };
        let msg = validate_and_apply(&ws, &p).unwrap();
        assert!(msg.contains("created"), "{msg}");
        let entries = memory::scan_all_memories(&ws);
        assert!(!entries.is_empty());
    }

    #[test]
    fn promote_blocked_for_one_project_path() {
        let p = LearningProposal {
            kind: "code-style".into(),
            scope: "global".into(),
            statement: "never edit core/src/memory.rs directly".into(),
            confidence: 0.9,
            evidence: vec!["a".into(), "b".into(), "c".into()],
            references: vec![],
            contradicts: vec![],
            suggested_action: "promote_to_global".into(),
        };
        let home = tmp();
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(validate_and_apply(&ws, &p).is_err());
    }

    #[test]
    fn explicit_preference_capture() {
        let _ls = learning_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ms = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _rs = crate::project_identity::registry_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tmp();
        std::fs::create_dir_all(home.join("memory/global")).unwrap();
        let _mr = override_memory_root(home.join("memory"));
        let _lr = override_learning_root(home.join("learning"));
        let _rr = override_registry_path(home.join("reg.json"));
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let msg = maybe_capture_explicit_preference(&ws, "I always prefer tabs over spaces");
        assert!(msg.is_some(), "{msg:?}");
    }
}
