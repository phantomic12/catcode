//! Task-specific context pack (spec §14) — compact, bounded, scope-labeled.
//!
//! Assembles retrieval from memories (hybrid ranker), preferences, episodes,
//! rejected approaches, change coupling, and the codebase index.
//! Deterministic. Fail-open.

#![allow(dead_code)]

use std::path::Path;

use crate::change_coupling;
use crate::codebase_index;
use crate::episodes;
use crate::failure_atlas;
use crate::learning_activations::{self, RetrievalStage};
use crate::learning_retrieval;
use crate::memory::{self, MemoryStatus, Scope};
use crate::preferences::{self, PreferenceRecord};
use crate::project_identity::{self, ProjectIdentity};
use crate::rejected_approaches;
use crate::task_fingerprint::{self, FingerprintInputs, TaskFingerprint};

/// Default character budget for the pack (spec §14.3).
pub const CONTEXT_PACK_MAX_CHARS: usize = 10_000;

const MAX_PROJECT_MEMORIES: usize = 5;
const MAX_GLOBAL_PREFS: usize = 3;
const MAX_EPISODES: usize = 3;
const MAX_REJECTED: usize = 3;
const MAX_FILES: usize = 6;
const MAX_COMPANIONS: usize = 6;

/// Role for multi-agent context packs (spec §20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextRole {
    Full,
    Scout,
    Planner,
    Worker,
    Reviewer,
}

impl ContextRole {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "scout" => Self::Scout,
            "planner" => Self::Planner,
            "worker" => Self::Worker,
            "reviewer" => Self::Reviewer,
            _ => Self::Full,
        }
    }
}

/// Build a compact `[TASK CONTEXT]` pack for `prompt` (full role).
pub fn build_context_pack(workspace: &Path, prompt: &str) -> String {
    build_context_pack_for(workspace, prompt, ContextRole::Full)
}

/// Role-filtered context pack for subagents.
pub fn build_context_pack_for(workspace: &Path, prompt: &str, role: ContextRole) -> String {
    let identity = project_identity::resolve_project_identity(workspace);
    let fp = task_fingerprint::build_fingerprint(&FingerprintInputs {
        user_intent: prompt,
        files_read: &[],
        files_changed: &[],
        symbols: &[],
        tools_used: &[],
        diagnostics: &[],
        tests_run: &[],
    });

    let mut out = String::from("[TASK CONTEXT]\n\n");
    out.push_str(&format!("Task interpretation:\n- intent: {}\n", fp.intent));
    if !fp.subsystems.is_empty() {
        out.push_str(&format!("- subsystems: {}\n", fp.subsystems.join(", ")));
    }
    out.push('\n');

    out.push_str(&format!(
        "Project identity:\n- [PROJECT] {}{}\n\n",
        identity.id,
        identity
            .remote
            .as_ref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default()
    ));

    let include_prefs = matches!(
        role,
        ContextRole::Full | ContextRole::Planner | ContextRole::Worker
    );
    let include_rejected = matches!(
        role,
        ContextRole::Full | ContextRole::Planner | ContextRole::Reviewer
    );
    let include_episodes = matches!(
        role,
        ContextRole::Full | ContextRole::Scout | ContextRole::Planner
    );
    let include_files = matches!(
        role,
        ContextRole::Full | ContextRole::Scout | ContextRole::Worker
    );
    let include_companions = matches!(
        role,
        ContextRole::Full | ContextRole::Planner | ContextRole::Worker
    );
    let include_validation = matches!(
        role,
        ContextRole::Full | ContextRole::Reviewer | ContextRole::Worker
    );

    if include_prefs {
        let prefs = preferences::load_global_preferences();
        append_preferences(&mut out, &prefs);
    }
    if include_rejected {
        append_rejected(&mut out, &identity, &fp);
    }
    // Error-recovery: surface matching diagnostic signatures (spec §14.4).
    if looks_like_error_prompt(prompt) {
        append_diagnostics(&mut out, &identity.id, prompt);
    }
    if include_episodes {
        append_episodes(&mut out, &identity, &fp);
    }
    // File/companion surfacing needs the code index and only helps for
    // code-change tasks; for conversational prompts it false-matches paths and
    // wastes tokens. Gate on a cheap code-task heuristic.
    let code_task = task_fingerprint::looks_like_code_task(prompt);
    if include_files && code_task {
        append_likely_files(&mut out, &identity, prompt);
    }
    if include_companions && code_task {
        append_companions(&mut out, &identity, prompt);
    }
    if include_validation {
        append_validation_hints(&mut out, &identity, &fp);
    }

    // Ranked memories (was defined but never called — subagents rely on pack
    // alone and never saw the main-turn [RELEVANT MEMORIES] tail) (CORE_REVIEW C13).
    let include_memories = matches!(
        role,
        ContextRole::Full | ContextRole::Planner | ContextRole::Worker | ContextRole::Scout
    );
    if include_memories {
        append_ranked_memories(&mut out, workspace, prompt, &fp, true, true);
    }

    // Activation telemetry (fail-open).
    learning_activations::record_pack_activations(
        &identity.id,
        RetrievalStage::PrePlan,
        None,
        &[("context_pack", "task-context", 0, 1.0, out.len() / 4)],
    );

    if out.len() > CONTEXT_PACK_MAX_CHARS {
        out.truncate(CONTEXT_PACK_MAX_CHARS);
        out.push_str("\n…[context pack truncated]\n");
    }
    out
}

fn append_ranked_memories(
    out: &mut String,
    workspace: &Path,
    prompt: &str,
    fp: &TaskFingerprint,
    project: bool,
    global: bool,
) {
    let memories = memory::scan_all_memories(workspace);
    let pid = project_identity::resolve_project_identity(workspace).id;
    let ranked = learning_retrieval::rank_memories_in_project(&memories, prompt, fp, 12, &pid);
    let mut project_n = 0usize;
    let mut global_n = 0usize;
    let mut project_section = String::new();
    let mut global_section = String::new();

    for (score, m, _reasons) in ranked {
        if score < 0.05 {
            continue;
        }
        let status = match m.status {
            MemoryStatus::Verified => "[VERIFIED]",
            MemoryStatus::Candidate => "[CANDIDATE]",
            MemoryStatus::NeedsVerification | MemoryStatus::Stale => "[STALE]",
            _ => continue,
        };
        let blurb = if m.description.is_empty() {
            m.content.lines().next().unwrap_or("").to_string()
        } else {
            m.description.clone()
        };
        match m.scope {
            Scope::Workspace if project && project_n < MAX_PROJECT_MEMORIES => {
                project_section.push_str(&format!(
                    "- [PROJECT] {status} {} — {}\n",
                    m.name,
                    truncate(&blurb, 120)
                ));
                project_n += 1;
            }
            Scope::Global if global && global_n < MAX_GLOBAL_PREFS => {
                global_section.push_str(&format!(
                    "- [GLOBAL] {status} {} — {}\n",
                    m.name,
                    truncate(&blurb, 120)
                ));
                global_n += 1;
            }
            _ => {}
        }
    }

    if !project_section.is_empty() {
        out.push_str("Project architecture and conventions:\n");
        out.push_str(&project_section);
        out.push('\n');
    }
    if !global_section.is_empty() {
        out.push_str("Global developer preferences:\n");
        out.push_str(&global_section);
        out.push('\n');
    }
}

fn append_preferences(out: &mut String, prefs: &[PreferenceRecord]) {
    if prefs.is_empty() {
        return;
    }
    out.push_str("Structured preferences:\n");
    for p in prefs.iter().take(MAX_GLOBAL_PREFS) {
        out.push_str(&format!(
            "- [GLOBAL] [VERIFIED] ({}) {}\n",
            p.category,
            truncate(&p.statement, 120)
        ));
    }
    out.push('\n');
}

fn append_rejected(out: &mut String, identity: &ProjectIdentity, fp: &TaskFingerprint) {
    let mut hits = rejected_approaches::match_rejected(fp, Some(&identity.id), 0.2, MAX_REJECTED);
    let global = rejected_approaches::match_rejected(fp, None, 0.4, MAX_REJECTED);
    hits.extend(global);
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(MAX_REJECTED);
    if hits.is_empty() {
        return;
    }
    out.push_str("Known rejected approaches and failure modes:\n");
    for (sim, r) in hits {
        let scope = if r.scope == "global" {
            "[GLOBAL]"
        } else {
            "[PROJECT]"
        };
        out.push_str(&format!(
            "- {scope} [REJECTED APPROACH] (sim={sim:.2}) {} — {}{}\n",
            truncate(&r.approach, 80),
            truncate(&r.rejection_reason, 100),
            r.preferred_alternative
                .as_ref()
                .map(|a| format!(" → prefer: {}", truncate(a, 60)))
                .unwrap_or_default()
        ));
    }
    out.push('\n');
}

fn append_episodes(out: &mut String, identity: &ProjectIdentity, fp: &TaskFingerprint) {
    let similar = episodes::similar_episodes(&identity.id, fp, 0.35, MAX_EPISODES);
    if similar.is_empty() {
        return;
    }
    out.push_str("Similar previous coding episodes:\n");
    for (sim, ep) in similar {
        out.push_str(&format!(
            "- [PROJECT] {} (sim={sim:.2}, outcome={}) — {}\n",
            ep.id,
            ep.outcome.as_str(),
            truncate(&ep.user_intent, 100)
        ));
    }
    out.push('\n');
}

fn append_likely_files(out: &mut String, identity: &ProjectIdentity, prompt: &str) {
    let files = codebase_index::list_files(&identity.id);
    if files.is_empty() {
        return;
    }
    let prompt_l = prompt.to_lowercase();
    let mut hits: Vec<&codebase_index::FileRecord> = files
        .iter()
        .filter(|f| {
            let p = f.path.to_lowercase();
            prompt_l
                .split_whitespace()
                .any(|t| t.len() > 3 && p.contains(t))
        })
        .collect();
    hits.truncate(MAX_FILES);
    if hits.is_empty() {
        return;
    }
    out.push_str("Likely files and symbols:\n");
    for f in hits {
        out.push_str(&format!("- [PROJECT] {} ({})\n", f.path, f.language));
    }
    out.push('\n');
}

fn append_companions(out: &mut String, identity: &ProjectIdentity, prompt: &str) {
    let files = codebase_index::list_files(&identity.id);
    let prompt_l = prompt.to_lowercase();
    let triggers: Vec<&str> = files
        .iter()
        .filter(|f| {
            let p = f.path.to_lowercase();
            prompt_l
                .split_whitespace()
                .any(|t| t.len() > 3 && p.contains(t))
        })
        .map(|f| f.path.as_str())
        .take(3)
        .collect();
    let mut lines = Vec::new();
    for t in triggers {
        for edge in change_coupling::companions_for(&identity.id, t, 3) {
            lines.push(format!(
                "- [PROJECT] {} → {} (conf={:.2}, n={})",
                edge.trigger, edge.companion, edge.confidence, edge.supporting_commits
            ));
            if lines.len() >= MAX_COMPANIONS {
                break;
            }
        }
        if lines.len() >= MAX_COMPANIONS {
            break;
        }
    }
    if lines.is_empty() {
        return;
    }
    out.push_str("Likely companion changes:\n");
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }
    out.push('\n');
}

fn append_validation_hints(out: &mut String, identity: &ProjectIdentity, fp: &TaskFingerprint) {
    let eps = episodes::similar_episodes(&identity.id, fp, 0.4, 5);
    let mut cmds: Vec<String> = Vec::new();
    for (_, ep) in eps {
        for t in ep.tests_run {
            if t.ok {
                let c = t.command.clone();
                if !cmds.iter().any(|x| x == &c) {
                    cmds.push(c);
                }
            }
        }
    }
    for v in &fp.validation_classes {
        let guess = if v.starts_with("cargo-test-") {
            format!("cargo test {}", v.trim_start_matches("cargo-test-"))
        } else if v == "cargo-build" {
            "cargo build".into()
        } else {
            continue;
        };
        if !cmds.iter().any(|x| x == &guess) {
            cmds.push(guess);
        }
    }
    cmds.truncate(5);
    if cmds.is_empty() {
        return;
    }
    out.push_str("Recommended validation:\n");
    for c in cmds {
        out.push_str(&format!("- [PROJECT] {c}\n"));
    }
    out.push('\n');
}

fn looks_like_error_prompt(prompt: &str) -> bool {
    let p = prompt.to_lowercase();
    p.contains("error[")
        || p.contains("failed")
        || p.contains("panic")
        || p.contains("does not compile")
        || p.contains("test failed")
}

fn append_diagnostics(out: &mut String, project_id: &str, prompt: &str) {
    let hits = failure_atlas::match_diagnostics(project_id, prompt, 3);
    if hits.is_empty() {
        // Also try a coarse class token.
        let hits = failure_atlas::match_diagnostics(project_id, "cargo", 3);
        if hits.is_empty() {
            return;
        }
        out.push_str("Prior matching failures:\n");
        for d in hits {
            out.push_str(&format!(
                "- [PROJECT] [{}] x{} {}\n",
                d.class,
                d.count,
                truncate(&d.signature, 100)
            ));
        }
        out.push('\n');
        return;
    }
    out.push_str("Prior matching failures:\n");
    for d in hits {
        out.push_str(&format!(
            "- [PROJECT] [{}] x{} {}\n",
            d.class,
            d.count,
            truncate(&d.signature, 100)
        ));
    }
    out.push('\n');
}

fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning_store::override_learning_root;
    use crate::memory::override_memory_root;
    use crate::preferences::LearningStatus;
    use crate::project_identity::override_registry_path;
    use crate::rejected_approaches::{append_rejected, RejectedApproach};

    fn tmp_home(label: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pack_is_bounded_and_labeled() {
        let home = tmp_home("ctx-pack");
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lserial = crate::learning_store::learning_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _mr = override_memory_root(home.join("memory"));
        let _lr = override_learning_root(home.join("learning"));
        let _rserial = crate::project_identity::registry_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _rr = override_registry_path(home.join("registry.json"));
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("main.rs"), "fn main() {}").unwrap();
        let _ = codebase_index::ensure_index(&ws);
        let pack = build_context_pack(&ws, "extend the memory tool schema");
        assert!(pack.contains("[TASK CONTEXT]"));
        assert!(pack.contains("Project identity"));
        assert!(pack.len() <= CONTEXT_PACK_MAX_CHARS + 40);
    }

    #[test]
    fn rejected_approaches_surface_as_warnings() {
        let home = tmp_home("ctx-rej");
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lserial = crate::learning_store::learning_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _mr = override_memory_root(home.join("memory"));
        let _lr = override_learning_root(home.join("learning"));
        let _rserial = crate::project_identity::registry_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _rr = override_registry_path(home.join("registry.json"));
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let id = project_identity::resolve_project_identity(&ws);
        let mut fp = TaskFingerprint::default();
        fp.intent = "extend-memory-tool".into();
        fp.subsystems = vec!["memory".into(), "tools".into()];
        append_rejected(
            Some(&id.id),
            &RejectedApproach {
                id: "rej-1".into(),
                scope: "project".into(),
                task_fingerprint: fp.clone(),
                approach: "add tool action without schema".into(),
                rejection_reason: "dispatch/schema drift".into(),
                preferred_alternative: Some("update schema + dispatch together".into()),
                evidence: vec!["ep-1".into()],
                confidence: 0.9,
                status: LearningStatus::Verified,
            },
        );
        let matched = rejected_approaches::match_rejected(&fp, Some(&id.id), 0.2, 3);
        assert!(
            !matched.is_empty(),
            "stored rejection must match fingerprint"
        );
        let pack = build_context_pack(&ws, "Extend the memory tool with a new action");
        assert!(
            pack.contains("[REJECTED APPROACH]"),
            "pack should warn about rejected approaches: {pack}"
        );
    }

    #[test]
    fn scout_role_omits_rejected_section() {
        let home = tmp_home("ctx-role");
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _lserial = crate::learning_store::learning_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _mr = override_memory_root(home.join("memory"));
        let _lr = override_learning_root(home.join("learning"));
        let _rserial = crate::project_identity::registry_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _rr = override_registry_path(home.join("registry.json"));
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let scout = build_context_pack_for(&ws, "find memory modules", ContextRole::Scout);
        assert!(!scout.contains("[REJECTED APPROACH]"));
        assert!(scout.contains("[TASK CONTEXT]"));
    }
}
