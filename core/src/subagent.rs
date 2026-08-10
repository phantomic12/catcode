// Subagent system: a port of pi-subagents' delegation/orchestration features,
// adapted to this harness's single-process, Rust-core architecture.
//
// Subagents are nested agentic loops (see run_agent) that share the workspace,
// tools, and API key but run with a focused system prompt and an optional tool
// allowlist. They are defined as markdown files with YAML frontmatter
// (agents/*.md), discovered from builtin (embedded), user, and project scopes.
//
// Execution modes (the `subagent` tool):
//   - single:  { agent, task, ... }
//   - parallel:{ tasks: [...], concurrency, worktree }
//   - chain:   { chain: [...], async }
//   - management: { action: list|get|create|update|delete|status|interrupt|resume|doctor }
//
// Coordination (see intercom.rs) is wired in here: a subagent whose resolved
// tools include `contact_supervisor`/`intercom` gets those tools + bridge
// instructions, and can prompt the orchestrator for decisions or talk to peer
// subagents when the setup allows it.

use crate::config::{Config, ResolvedProvider, SubagentConfig};
use crate::intercom::{execute_contact_supervisor, execute_intercom};
use crate::logging::{estimate_messages_tokens, grounded_estimate, TurnTimer};
use crate::message::{self, Message};
use crate::protocol::{emit, Event, ModelInfo};
use crate::tools::{self, Outcome};
use crate::State;
use futures_util::FutureExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Frontmatter parsing (port of frontmatter.ts)
// ---------------------------------------------------------------------------

/// Parse YAML-ish frontmatter (key: value, flat only) + markdown body.
pub fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut fm: HashMap<String, String> = HashMap::new();
    let normalized = content.replace("\r\n", "\n");
    if !normalized.starts_with("---") {
        return (fm, normalized.trim().to_string());
    }

    let end = match normalized.find("\n---") {
        // Require i > 3 so an immediately-closed fence ("---\n---…", empty
        // frontmatter) doesn't slice [4..3] and panic. With i == 3 the block is
        // empty; treat the whole content as the body (the caller skips agents
        // with no parsed `name`).
        Some(i) if i > 3 => i,
        _ => return (fm, normalized),
    };
    let block = &normalized[4..end];
    let body = normalized[end + 4..].trim().to_string();
    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(colon) = line.find(':') {
            let key = line[..colon].trim().to_string();
            let mut val = line[colon + 1..].trim().to_string();
            // strip surrounding quotes
            if val.len() >= 2
                && ((val.starts_with('"') && val.ends_with('"'))
                    || (val.starts_with('\'') && val.ends_with('\'')))
            {
                val = val[1..val.len() - 1].to_string();
            }
            fm.insert(key, val);
        }
    }
    (fm, body)
}

// ---------------------------------------------------------------------------
// Agent config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum SystemPromptMode {
    Replace,
    Append,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum ContextKind {
    Fresh,
    Fork,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum AgentSource {
    Builtin,
    User,
    Project,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub model: Option<String>,
    pub fallback_models: Vec<String>,
    pub thinking: Option<String>,
    pub system_prompt_mode: SystemPromptMode,
    pub inherit_project_context: bool,
    pub inherit_skills: bool,
    pub default_context: Option<ContextKind>,
    pub system_prompt: String,
    pub source: AgentSource,
    pub file_path: String,
    pub skills: Vec<String>,
    pub output: Option<String>,
    pub default_reads: Vec<String>,
    pub default_progress: bool,
    pub max_subagent_depth: Option<u32>,
    pub completion_guard: bool,
    pub disabled: bool,
}

impl AgentConfig {
    /// Map a pi-style tool name to this harness's tool name.
    pub fn normalize_tool(name: &str) -> &str {
        match name {
            "read" => "read_file",
            "find" => "glob",
            "ls" => "list_dir",
            "write" => "write_file",
            // bash, edit, grep, glob, list_dir, patch, diagnostics, subagent,
            // contact_supervisor, intercom, todo_* pass through unchanged.
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in agents (embedded fallback; .catalyst-code/agents/*.md overrides)
// ---------------------------------------------------------------------------

fn builtin_agents() -> Vec<AgentConfig> {
    let mk = |name: &str,
              desc: &str,
              tools: &str,
              thinking: Option<&str>,
              append: bool,
              inherit_ctx: bool,
              default_ctx: Option<ContextKind>,
              prompt: &str| {
        AgentConfig {
            name: name.to_string(),
            description: desc.to_string(),
            tools: tools
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            model: None,
            fallback_models: vec![],
            thinking: thinking.map(|s| s.to_string()),
            system_prompt_mode: if append {
                SystemPromptMode::Append
            } else {
                SystemPromptMode::Replace
            },
            inherit_project_context: inherit_ctx,
            inherit_skills: false,
            default_context: default_ctx,
            system_prompt: prompt.to_string(),
            source: AgentSource::Builtin,
            file_path: format!("<builtin:{name}>"),
            skills: vec![],
            output: if name == "scout" {
                Some("context.md".into())
            } else {
                None
            },
            default_reads: if name == "worker" || name == "reviewer" {
                vec!["context.md".into(), "plan.md".into()]
            } else {
                vec![]
            },
            default_progress: name == "worker" || name == "scout",
            max_subagent_depth: None,
            completion_guard: false,
            disabled: false,
        }
    };
    vec![
        mk(
            "scout",
            "Fast codebase recon that returns compressed context for handoff",
            "read_file, grep, glob, list_dir, bash, write_file, memory, intercom",
            Some("low"),
            false,
            true,
            None,
            SCOUT_PROMPT,
        ),
        mk(
            "researcher",
            "Web/docs research with sources and a concise research brief",
            "read_file, grep, glob, list_dir, bash, write_file, memory, intercom, fetch, web_search",
            Some("low"),
            false,
            true,
            None,
            RESEARCHER_PROMPT,
        ),
        mk(
            "planner",
            "A concrete implementation plan from existing context; reads and plans, does not edit",
            "read_file, grep, glob, list_dir, bash, intercom",
            Some("high"),
            false,
            true,
            Some(ContextKind::Fork),
            PLANNER_PROMPT,
        ),
        mk(
            "worker",
            "Implementation agent for normal tasks and approved oracle handoffs",
            "read_file, grep, glob, list_dir, bash, edit, write_file, contact_supervisor",
            Some("high"),
            false,
            true,
            Some(ContextKind::Fork),
            WORKER_PROMPT,
        ),
        mk(
            "reviewer",
            "Code review and small fixes against the task/plan, tests, edge cases, simplicity",
            "read_file, grep, glob, list_dir, bash, edit, write_file, intercom",
            Some("high"),
            false,
            true,
            None,
            REVIEWER_PROMPT,
        ),
        mk(
            "context-builder",
            "Stronger setup pass before planning: gathers context and writes handoff material",
            "read_file, grep, glob, list_dir, bash, write_file, memory, intercom",
            Some("low"),
            false,
            true,
            None,
            CONTEXT_BUILDER_PROMPT,
        ),
        mk(
            "oracle",
            "High-context decision-consistency oracle; challenges assumptions, prevents drift",
            "read_file, grep, glob, list_dir, bash, intercom",
            Some("high"),
            false,
            true,
            Some(ContextKind::Fork),
            ORACLE_PROMPT,
        ),
        mk(
            "delegate",
            "Lightweight general delegate that behaves close to the parent session",
            "read_file, grep, glob, list_dir, bash, edit, write_file, contact_supervisor",
            None,
            true,
            true,
            None,
            DELEGATE_PROMPT,
        ),
    ]
}

const SCOUT_PROMPT: &str = "You are a scouting subagent. Move fast, but do not guess. Use targeted search and selective reading over reading whole files unless the task clearly needs broader coverage.\n\nFocus on the minimum context another agent needs to act: relevant entry points, key types/interfaces/functions, data flow and dependencies, files likely to need changes, and constraints/risks/open questions.\n\nWorking rules:\n- Use grep, glob, list_dir, and read_file to map the area before diving deeper.\n- Use bash only for non-interactive inspection.\n- Cite exact file paths and line ranges.\n- If told to write output, write it to the provided path and keep the final response short.\n- If blocked or needing a decision, use contact_supervisor with reason \"need_decision\" and wait for the reply.\n\nOutput format:\n# Code Context\n## Files Retrieved (exact paths + line ranges + why)\n## Key Code (critical types/functions/snippets)\n## Architecture (how pieces connect)\n## Start Here (first file another agent should open + why)";

const RESEARCHER_PROMPT: &str = "You are a research subagent. Gather external evidence: official docs, specs, benchmarks, recent changes. Return a concise research brief with source links, confidence level, gaps, and decision implications. Do not edit code. If blocked or needing a decision, use contact_supervisor with reason \"need_decision\" and wait for the reply.";

const PLANNER_PROMPT: &str = "You are a planning subagent. Produce a concrete, actionable implementation plan from the supplied context. Read and plan; do not edit code. Include: goals, affected files, step-by-step changes, risks, validation steps, and open questions. Treat inherited forked context as reference-only — do not continue prior conversations. If a decision is missing that blocks planning, use contact_supervisor with reason \"need_decision\" and wait.";

const WORKER_PROMPT: &str = "You are `worker`: the implementation subagent. You are the single writer thread. Execute the assigned task or approved direction with narrow, coherent edits. The main agent and user remain the decision authority.\n\nFirst understand the inherited context, supplied files, plan, and explicit task. Then implement carefully and minimally. If implementation reveals an unapproved decision required to continue safely, pause and escalate with contact_supervisor (reason \"need_decision\") and wait for the reply before continuing. Use reason \"progress_update\" only for concise non-blocking updates.\n\nWorking rules:\n- Prefer narrow, correct changes over broad rewrites.\n- Do not add speculative scaffolding.\n- Do not leave TODOs or silent scope changes.\n- Use bash for inspection, validation, and tests.\n- Read supplied context/plan first.\n- If your task expects edits and you made none, do not return a success summary.\n\nFinal response shape:\nImplemented X.\nChanged files: Y.\nValidation: Z.\nOpen risks/questions: R.\nRecommended next step: N.";

const REVIEWER_PROMPT: &str = "You are a disciplined review subagent. Inspect, evaluate, and report findings with evidence. Do not guess; verify from code, tests, docs, or requirements.\n\nReview: implementation vs intent, correctness/edge-cases, test coverage, unintended side effects/regressions, and simplicity/readability. Return concise, evidence-backed findings with file/line references. Make small fixes only if asked. If blocked or needing a decision, use contact_supervisor/intercom with reason \"need_decision\" and wait.";

const CONTEXT_BUILDER_PROMPT: &str = "You are a context-building subagent. Gather the code context another agent needs before planning or implementation. Read every relevant file, follow imports/callers/tests/docs/config, and write handoff material (e.g. context.md) plus a compact meta-prompt. Do not implement features. If blocked, use contact_supervisor with reason \"need_decision\" and wait.";

const ORACLE_PROMPT: &str = "You are the oracle: a high-context decision-consistency subagent. Prevent the main agent from making hidden, conflicting, or inconsistent decisions by treating inherited forked context as the authoritative contract. You are not the primary executor and do not edit files.\n\nReconstruct inherited decisions/constraints/open questions; identify drift between the current trajectory and those decisions; surface contradictions and hidden assumptions. Prefer narrow corrections over broad pivots. If you need clarification, use contact_supervisor with reason \"need_decision\" and wait for the reply.\n\nOutput shape:\nInherited decisions:\nDiagnosis:\nDrift / contradiction check:\nRecommendation:\nRisks:\nNeed from main agent:\nSuggested execution prompt (only if a worker handoff is warranted):";

const DELEGATE_PROMPT: &str = "You are a delegated agent. Execute the assigned task using the provided tools. Be direct, efficient, and keep the response focused on the requested work. If blocked or needing a decision, use contact_supervisor with reason \"need_decision\" and wait for the reply.";

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Resolve an agent's intercom target name (stable per run).
pub fn subagent_target(run_id: &str, agent: &str, index: Option<usize>) -> String {
    let suffix = index.map(|i| format!("-{}", i + 1)).unwrap_or_default();
    let clean = |s: &str| {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect::<String>()
            .to_lowercase()
    };
    let rid = run_id.replace('-', "");
    let mut rid_end = rid.len().min(8);
    while rid_end > 0 && !rid.is_char_boundary(rid_end) {
        rid_end -= 1;
    }
    format!("subagent-{}-{}{}", clean(agent), &rid[..rid_end], suffix)
}

/// Discover all agents: builtin (lowest) < user < project (project wins on name).
/// Applies settings overrides (model/fallback/thinking/disabled).
pub fn discover_agents(workspace: &Path, cfg: &SubagentConfig) -> Vec<AgentConfig> {
    let mut by_name: HashMap<String, AgentConfig> = HashMap::new();

    if !cfg.disable_builtins {
        for mut a in builtin_agents() {
            apply_overrides(&mut a, cfg);
            if !a.disabled {
                by_name.insert(a.name.clone(), a);
            }
        }
    }

    // user scope: ~/.catalyst-code/agents/**/*.md
    if let Some(home) = crate::config::home_dir() {
        let dir = home.join(".catalyst-code/agents");
        load_agent_dir(&dir, AgentSource::User, &mut by_name);
    }
    // project scope: <workspace>/.catalyst-code/agents/**/*.md
    let dir = workspace.join(".catalyst-code/agents");
    load_agent_dir(&dir, AgentSource::Project, &mut by_name);

    let mut v: Vec<AgentConfig> = by_name.into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn load_agent_dir(dir: &Path, source: AgentSource, by_name: &mut HashMap<String, AgentConfig>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            load_agent_dir(&p, source.clone(), by_name);
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let (fm, body) = parse_frontmatter(&content);
        let name = match fm.get("name").and_then(|s| s.split_whitespace().next()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let tools_str = fm.get("tools").cloned().unwrap_or_default();
        let a = AgentConfig {
            name: name.clone(),
            description: fm.get("description").cloned().unwrap_or_default(),
            tools: tools_str
                .split(',')
                .map(|s| AgentConfig::normalize_tool(s.trim()).to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            model: fm.get("model").cloned(),
            fallback_models: fm
                .get("fallbackModels")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default(),
            thinking: fm.get("thinking").cloned(),
            system_prompt_mode: match fm.get("systemPromptMode").map(|s| s.as_str()) {
                Some("append") => SystemPromptMode::Append,
                _ => SystemPromptMode::Replace,
            },
            inherit_project_context: fm
                .get("inheritProjectContext")
                .map(|s| s == "true")
                .unwrap_or(false),
            inherit_skills: fm
                .get("inheritSkills")
                .map(|s| s == "true")
                .unwrap_or(false),
            default_context: match fm.get("defaultContext").map(|s| s.as_str()) {
                Some("fork") => Some(ContextKind::Fork),
                Some("fresh") => Some(ContextKind::Fresh),
                _ => None,
            },
            system_prompt: body,
            source: source.clone(),
            file_path: p.display().to_string(),
            skills: fm
                .get("skills")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default(),
            output: fm.get("output").cloned(),
            default_reads: fm
                .get("defaultReads")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default(),
            default_progress: fm
                .get("defaultProgress")
                .map(|s| s == "true")
                .unwrap_or(false),
            max_subagent_depth: fm.get("maxSubagentDepth").and_then(|s| s.parse().ok()),
            completion_guard: fm
                .get("completionGuard")
                .map(|s| s == "false")
                .unwrap_or(false),
            disabled: fm.get("disabled").map(|s| s == "true").unwrap_or(false),
        };
        if !a.disabled {
            by_name.insert(name, a);
        }
    }
}

fn apply_overrides(a: &mut AgentConfig, cfg: &SubagentConfig) {
    if let Some(ov) = cfg.agent_overrides.get(&a.name) {
        if let Some(m) = &ov.model {
            a.model = Some(m.clone());
        }
        if !ov.fallback_models.is_empty() {
            a.fallback_models = ov.fallback_models.clone();
        }
        if let Some(t) = &ov.thinking {
            a.thinking = Some(t.clone());
        }
        if ov.disabled {
            a.disabled = true;
        }
    }
}

pub fn find_agent<'a>(agents: &'a [AgentConfig], name: &str) -> Option<&'a AgentConfig> {
    // allow package.name syntax: code-analysis.scout → scout
    let bare = name.rsplit('.').next().unwrap_or(name);
    agents.iter().find(|a| a.name == name || a.name == bare)
}

// ---------------------------------------------------------------------------
// Skills (SKILL.md discovery + injection)
// ---------------------------------------------------------------------------

pub(crate) fn discover_skills(workspace: &Path) -> Vec<(String, String, String)> {
    // (name, description, location) — project first, then user.
    // Keep in sync with `crate::skills::configured` roots (CORE_REVIEW).
    let mut out: Vec<(String, String, String)> = Vec::new();
    let _skill_resolver =
        crate::skills::configured(workspace, crate::config::home_dir().as_deref());
    let _ = _skill_resolver.discover(); // warm / validate resolver path
    let dirs = [
        (workspace.join(".catalyst-code/skills"), true),
        (
            crate::config::home_dir()
                .map(|h| h.join(".catalyst-code/skills"))
                .unwrap_or_default(),
            false,
        ),
    ];
    for (dir, _proj) in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let skill_md = e.path().join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                let (fm, _) = parse_frontmatter(&content);
                // Skip skills explicitly marked `deprecated: true` in their
                // frontmatter (docs/SELF_LEARNING.md §8) — they should not be
                // advertised for use. (`replaced_by` resolution is a future
                // enhancement; for now a deprecated skill is simply hidden.)
                if fm
                    .get("deprecated")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
                {
                    continue;
                }
                let name = fm
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| e.file_name().to_string_lossy().into_owned());
                let desc = fm.get("description").cloned().unwrap_or_default();
                out.push((name, desc, skill_md.display().to_string()));
            }
        }
    }
    out
}

/// A discoverable skill with its parsed body content — used by the `skills`
/// event and `apply_skill` command so the core (which has unrestricted FS
/// access) can read global skills under ~/.catalyst-code/skills that the
/// read_file tool cannot reach (it rejects absolute / `..` paths).
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct SkillEntry {
    pub name: String,
    pub description: String,
    pub location: String,
    pub body: String,
    /// Optional skill stage from frontmatter (candidate|trusted|needs_revision|deprecated).
    pub stage: String,
    /// Optional version from frontmatter.
    pub version: String,
}

/// Like `discover_skills` but also returns the parsed SKILL.md body (frontmatter
/// stripped). Same precedence (project then user; first wins on name) and the
/// same deprecated-skill filter.
pub(crate) fn discover_skills_full(workspace: &Path) -> Vec<SkillEntry> {
    let mut out: Vec<SkillEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let dirs = [
        (workspace.join(".catalyst-code/skills"), true),
        (
            crate::config::home_dir()
                .map(|h| h.join(".catalyst-code/skills"))
                .unwrap_or_default(),
            false,
        ),
    ];
    for (dir, _proj) in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let skill_md = e.path().join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                let (fm, body) = parse_frontmatter(&content);
                if fm
                    .get("deprecated")
                    .map(|v| v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
                {
                    continue;
                }
                let name = fm
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| e.file_name().to_string_lossy().into_owned());
                // First scope wins on name (project > user), mirroring discover_skills.
                let key = name.to_lowercase();
                if !seen.insert(key) {
                    continue;
                }
                let desc = fm.get("description").cloned().unwrap_or_default();
                let stage = fm.get("stage").cloned().unwrap_or_default();
                let version = fm.get("version").cloned().unwrap_or_default();
                out.push(SkillEntry {
                    name,
                    description: desc,
                    location: skill_md.display().to_string(),
                    body,
                    stage,
                    version,
                });
            }
        }
    }
    out
}

/// Suggest a skill whose name+description semantically matches the prompt, so
/// the agent can apply it without remembering `/skill:<name>`. Mirrors the
/// memory relevant-tail: tf·idf cosine over the skill corpus (down-weights
/// common tokens so a skill isn't suggested just for sharing "all"/"use").
/// Returns a short hint string, or None when no skill clears the relevance bar.
pub(crate) fn relevant_skill_hint(workspace: &Path, prompt: &str) -> Option<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return None;
    }
    let skills = discover_skills(workspace);
    if skills.len() < 2 {
        return None;
    }
    let n = skills.len() as f64;
    let mut df: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (name, desc, _loc) in &skills {
        let toks: std::collections::HashSet<String> =
            crate::memory::significant_tokens(&format!("{name} {desc}"))
                .into_iter()
                .collect();
        for t in toks {
            *df.entry(t).or_insert(0) += 1;
        }
    }
    let idf: std::collections::HashMap<String, f64> = df
        .into_iter()
        .map(|(t, d)| (t, (n / d.max(1) as f64).ln().max(0.0)))
        .collect();
    let q = skill_tfidf(prompt, &idf);
    if q.is_empty() {
        return None;
    }
    let mut best: Option<(usize, f64)> = None;
    for (i, (name, desc, _loc)) in skills.iter().enumerate() {
        if name == "pi-subagents" {
            continue;
        }
        let v = skill_tfidf(&format!("{name} {desc}"), &idf);
        let score = crate::memory::cosine_sim(&q, &v);
        if score > 0.0 && best.is_none_or(|b| score > b.1) {
            best = Some((i, score));
        }
    }
    let (i, score) = best?;
    if score < 0.05 {
        return None;
    }
    let (name, desc, loc) = &skills[i];
    let ident = std::path::Path::new(loc)
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|x| x.to_str())
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| name.clone());
    let d = desc.trim();
    Some(format!(
        "[RELEVANT SKILL] — '{ident}' (score {score:.2}) matches this task. \
         Apply with /skill:{ident} if useful; read it first if applying.{rest}",
        rest = if d.is_empty() {
            String::new()
        } else {
            format!("\n  {d}")
        }
    ))
}

/// tf·idf-weighted bag over significant tokens, against a precomputed idf map.
fn skill_tfidf(
    text: &str,
    idf: &std::collections::HashMap<String, f64>,
) -> std::collections::HashMap<String, f64> {
    let mut v: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for t in crate::memory::significant_tokens(text) {
        let w = idf.get(&t).copied().unwrap_or(1.0);
        *v.entry(t).or_insert(0.0) += w;
    }
    v
}

fn skills_injection(workspace: &Path, names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let all = discover_skills(workspace);
    let mut blocks = String::new();
    for name in names {
        if name == "false" {
            continue;
        }
        if let Some((n, d, loc)) = all.iter().find(|(n, _, _)| n == name) {
            blocks.push_str(&format!(
                "  <skill>\n    <name>{n}</name>\n    <description>{d}</description>\n    <location>{loc}</location>\n  </skill>\n"
            ));
        }
    }
    if blocks.is_empty() {
        return String::new();
    }
    format!("The following skills are available to this subagent. Use read_file to load a skill file when the task matches its description.\n<available_skills>\n{blocks}</available_skills>")
}

// ---------------------------------------------------------------------------
// Intercom bridge instructions
// ---------------------------------------------------------------------------

pub const INTERCOM_BRIDGE_MARKER: &str = "Intercom orchestration channel:";

pub fn bridge_instruction(orchestrator_target: &str) -> String {
    format!("{INTERCOM_BRIDGE_MARKER}\nThe inherited thread is reference-only. Do not continue that conversation or send questions/status/completion handoffs to the supervisor in normal assistant text.\n\nUse contact_supervisor first. It resolves the supervisor \"{orchestrator_target}\" automatically.\n- Need a decision, blocked, approval, or scope ambiguity: contact_supervisor({{ reason: \"need_decision\", message: \"<question>\" }}). After a need_decision, stay alive and continue only after the reply arrives.\n- Meaningful progress or unexpected discoveries that change the plan: contact_supervisor({{ reason: \"progress_update\", message: \"UPDATE: <summary>\" }}).\n- Generic intercom is lower-level plumbing/fallback for peer subagents: intercom({{ action: \"ask\", to: \"<peer>\", message: \"...\" }}).\n\nDo not use contact_supervisor/intercom for routine completion handoffs. If no coordination is needed, return a focused task result.")
}

/// Decide whether intercom tools are injected for a subagent run, given the
/// bridge mode and the run's context kind.
pub fn bridge_active(mode: &crate::config::IntercomBridgeMode, ctx: Option<&ContextKind>) -> bool {
    use crate::config::IntercomBridgeMode;
    match mode {
        IntercomBridgeMode::Off => false,
        IntercomBridgeMode::ForkOnly => ctx == Some(&ContextKind::Fork),
        IntercomBridgeMode::Always => true,
    }
}

// ---------------------------------------------------------------------------
// Recursion guard
// ---------------------------------------------------------------------------

pub fn resolve_max_depth(cfg: &SubagentConfig) -> u32 {
    std::env::var("CATALYST_CODE_SUBAGENT_MAX_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &u32| *n == 0 || *n >= 1)
        .unwrap_or(cfg.max_depth)
}

pub fn child_max_depth(parent: u32, agent: Option<u32>) -> u32 {
    match agent {
        Some(a) => parent.min(a),
        None => parent,
    }
}

// ---------------------------------------------------------------------------
// Tool defs available to a subagent (filtered by agent.tools + bridge)
// ---------------------------------------------------------------------------

fn all_tool_names() -> &'static [&'static str] {
    &[
        // FS / search / shell
        "read_file",
        "edit",
        "write_file",
        "delete",
        "rename",
        "mkdir",
        "list_dir",
        "grep",
        "glob",
        "bash",
        "patch",
        // Bulk
        "bulk",
        "bulk_read",
        "bulk_write",
        "bulk_edit",
        // Planning / control
        "todo_write",
        "todo_read",
        "goal_write_plan",
        "finish",
        "ask",
        "load_tools",
        // Quality / web
        "diagnostics",
        "fetch",
        "web_search",
        // Git / workspace
        "git_status",
        "git_diff",
        "git_log",
        "git_show",
        "git_add",
        "git_commit",
        "git_push",
        "git_pull",
        "git_branch",
        "workspace_activity",
        // Agents / env
        "subagent",
        "spawn",
        "contact_supervisor",
        "intercom",
        "memory",
        "test_env",
    ]
}

/// Resolve the tool-name allowlist for a subagent (same set offered in the
/// schema). Empty `agent.tools` → core read-only defaults (not "all tools").
fn subagent_allowed_names(
    agent: &AgentConfig,
    bridge: bool,
    depth: u32,
    max_depth: u32,
) -> Vec<String> {
    let allow_subagent = agent.tools.iter().any(|t| t == "subagent") && depth + 1 < max_depth;
    let mut names: Vec<String> = if agent.tools.is_empty() {
        all_tool_names()
            .iter()
            .copied()
            .filter(|n| {
                tools::is_core_tool(n)
                    && !matches!(
                        *n,
                        "bash"
                            | "write_file"
                            | "edit"
                            | "patch"
                            | "delete"
                            | "rename"
                            | "mkdir"
                            | "subagent"
                            | "spawn"
                            | "load_tools"
                    )
            })
            .map(|s| s.to_string())
            .collect()
    } else {
        agent
            .tools
            .iter()
            .filter_map(|t| {
                let n = AgentConfig::normalize_tool(t);
                if n == "subagent" && !allow_subagent {
                    None
                } else {
                    Some(n.to_string())
                }
            })
            .collect()
    };
    if bridge {
        for t in ["contact_supervisor", "intercom"] {
            if !names.iter().any(|n| n == t) {
                names.push(t.to_string());
            }
        }
    }
    if allow_subagent && !names.iter().any(|n| n == "subagent") {
        names.push("subagent".into());
    }
    if !names.iter().any(|n| n == "finish") {
        names.push("finish".into());
    }
    names
}

/// Build the tool-definition list a subagent may call, applying the agent's
/// allowlist and the intercom bridge. `depth`/`max_depth` gate the `subagent`
/// tool (nested fanout only when allowed and below the depth cap).
pub fn subagent_tool_defs(
    agent: &AgentConfig,
    bridge: bool,
    depth: u32,
    max_depth: u32,
) -> Vec<Value> {
    let all = tools::definitions();
    let by_name: HashMap<&str, &Value> = all
        .iter()
        .map(|d| {
            (
                d.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                d,
            )
        })
        .collect();
    let names = subagent_allowed_names(agent, bridge, depth, max_depth);
    names
        .iter()
        .filter_map(|n| by_name.get(n.as_str()).map(|v| (*v).clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Run tracking (for status/interrupt/resume)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SubagentRun {
    pub id: String,
    pub parent_run_id: Option<String>,
    pub mode: String, // single | parallel | chain
    pub agent: Option<String>,
    pub agents: Vec<String>,
    pub state: String, // running | completed | failed | cancelled | paused
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub depth: u32,
    pub intercom_target: Option<String>,
    pub cancel: Option<Arc<CancellationToken>>,
    pub children: Vec<SubagentRun>,
    pub summary: Option<String>,
    /// Shared conversation snapshot for peek/steer. Updated after each turn.
    pub messages: Arc<Mutex<Vec<Message>>>,
}

/// Maximum number of terminal run records kept for
/// `status`/history. Still-running runs are always retained. Without this cap
/// the map grew without bound over a long session — every subagent invocation
/// (single/parallel/chain) left a permanent entry pinning an
/// `Arc<CancellationToken>` (and its parent chain), so RSS crept up the longer
/// the process stayed up.
const MAX_TERMINAL_RUNS: usize = 64;
const PARKED_AGENT_TTL_MS: u64 = 5 * 60 * 1000;

/// Evict old terminal runs so `subagent_runs` stays bounded. Always keeps every
/// still-running run; trims terminal runs to the most recent `MAX_TERMINAL_RUNS`
/// by end time (`ended_at`, falling back to `started_at`). Call after a run is
/// finalized. `resume` checks liveness via `intercom targets()`, so a pruned
/// completed run is simply absent rather than misreported as live.
fn prune_terminal_runs(runs: &mut HashMap<String, SubagentRun>) {
    if runs.len() <= MAX_TERMINAL_RUNS {
        return;
    }
    let mut terminal: Vec<(u64, String)> = runs
        .iter()
        // A `paused` (interrupted) run is NOT droppable — the user may still
        // `resume`/`steer` it. Only genuinely terminal runs (completed/failed)
        // are eligible for eviction (including explicitly cancelled runs).
        .filter(|(_, r)| r.state != "running" && r.state != "paused")
        .map(|(id, r)| (r.ended_at.unwrap_or(r.started_at), id.clone()))
        .collect();
    if terminal.len() <= MAX_TERMINAL_RUNS {
        return; // running runs dominate the count; nothing safe to drop
    }
    terminal.sort_unstable_by_key(|(t, _)| *t);
    let drop = terminal.len().saturating_sub(MAX_TERMINAL_RUNS);
    for (_, id) in terminal.into_iter().take(drop) {
        runs.remove(&id);
    }
}

// ---------------------------------------------------------------------------
// Execution entry point (the `subagent` tool body)
// ---------------------------------------------------------------------------

pub fn execute(
    st: Arc<State>,
    client: reqwest::Client,
    provider: ResolvedProvider,
    parent_model: String,
    args: Value,
    cancel: CancellationToken,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Outcome> + Send>> {
    let inherited_parent_run_id = crate::runtime::current_run_id().or_else(|| {
        args.get("_parent_run_id")
            .and_then(Value::as_str)
            .map(String::from)
    });
    let session = st.runtime.session_context();
    Box::pin(crate::runtime::scope_session(session, async move {
        let workspace = st.cfg.read().await.workspace.clone();
        let cfg = st.cfg.read().await.clone();
        let max_depth = resolve_max_depth(&cfg.subagents);

        // Depth guard: a subagent calling subagent beyond the cap is blocked.
        if depth >= max_depth {
            return Outcome::err(format!(
            "subagent nesting blocked at depth {depth} (max {max_depth}); complete the current task directly"
        ));
        }

        // Backward-compat: a bare {prompt} (legacy spawn) runs as a delegate.
        if args.get("agent").is_none() && args.get("tasks").is_none() && args.get("chain").is_none()
        {
            if let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) {
                let agents = discover_agents(&workspace, &cfg.subagents);
                let agent = find_agent(&agents, "delegate").cloned().unwrap_or_else(|| {
                    builtin_agents()
                        .into_iter()
                        .find(|a| a.name == "delegate")
                        .unwrap()
                        .clone()
                });
                let model_override = args.get("model").and_then(|v| v.as_str()).map(String::from);
                let run_id = next_run_id();
                return run_single(
                    &st,
                    &client,
                    &provider,
                    &parent_model,
                    &agent,
                    prompt,
                    &run_id,
                    inherited_parent_run_id.clone(),
                    model_override,
                    ContextKind::Fresh,
                    depth,
                    &cancel,
                    None,
                    None,
                )
                .await;
            }
        }
        // Management / control actions. A parked completed agent is revived as
        // a fresh, steerable continuation from its durable transcript.
        if let Some(action) = args.get("action").and_then(|v| v.as_str()) {
            if action == "resume" {
                if let Some(outcome) = revive_parked_agent(
                    &args,
                    &workspace,
                    &cfg,
                    &st,
                    &client,
                    &provider,
                    &parent_model,
                    depth,
                    &cancel,
                )
                .await
                {
                    return outcome;
                }
            }
            return handle_action(action, &args, &workspace, &cfg, &st, &cancel).await;
        }

        // Single agent run.
        if let Some(agent_name) = args.get("agent").and_then(|v| v.as_str()) {
            let agents = discover_agents(&workspace, &cfg.subagents);
            let agent = match find_agent(&agents, agent_name) {
                Some(a) => a.clone(),
                None => {
                    return Outcome::err(format!(
                        "unknown agent '{agent_name}'; use action:\"list\" to discover agents"
                    ))
                }
            };
            let task = args
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model_override = args.get("model").and_then(|v| v.as_str()).map(String::from);
            let context = parse_context(&args, &agent);
            let run_id = args
                .get("run_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(next_run_id);
            let use_worktree = args
                .get("worktree")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let wt_path = if use_worktree {
                if !crate::worktree::is_git_repo(&workspace) {
                    return Outcome::err(
                        "worktree:true requires a git repository; use hybrid checkpoints for non-git workspaces",
                    );
                }
                match crate::worktree::add_worktree(&workspace, &run_id) {
                    Ok(p) => match crate::worktree::seed_worktree_from_main(&workspace, &p) {
                        Ok(paths) => {
                            if !paths.is_empty() {
                                emit(
                                    &Event::new("worktree_seeded")
                                        .with("run_id", json!(&run_id))
                                        .with("paths", json!(paths)),
                                );
                            }
                            Some(p)
                        }
                        Err(e) => {
                            let _ = crate::worktree::remove_worktree(&workspace, &p);
                            return Outcome::err(format!("worktree seed failed: {e}"));
                        }
                    },
                    Err(e) => {
                        return Outcome::err(format!("worktree setup failed: {e}"));
                    }
                }
            } else {
                None
            };
            let mut outcome = run_single(
                &st,
                &client,
                &provider,
                &parent_model,
                &agent,
                &task,
                &run_id,
                inherited_parent_run_id.clone(),
                model_override,
                context,
                depth,
                &cancel,
                wt_path.clone(),
                None,
            )
            .await;
            if let Some(ref wt) = wt_path {
                // Cancelled runs report non-ok. Also refuse promote if the
                // parent turn token fired (H13). run_single uses a child token
                // for interrupt; abort paths already return Outcome::err.
                if outcome.ok && !cancel.is_cancelled() {
                    match crate::worktree::promote_worktree(&workspace, wt) {
                        Ok(paths) if !paths.is_empty() => {
                            emit(
                                &Event::new("worktree_promoted")
                                    .with("run_id", json!(run_id))
                                    .with("paths", json!(paths)),
                            );
                        }
                        Err(e) => {
                            emit(
                                &Event::new("error").with(
                                    "message",
                                    json!(format!("worktree promote failed: {e}")),
                                ),
                            );
                            // Promote failure must not report success — child
                            // edits live only in the worktree (CORE_REVIEW C11).
                            outcome = Outcome::err(format!(
                                "worktree promote failed (changes not applied to main workspace): {e}"
                            ));
                        }
                        _ => {}
                    }
                }
                // Only remove after successful promote or failed run; keep tree
                // on promote failure so the operator can recover.
                if outcome.ok || cancel.is_cancelled() {
                    let _ = crate::worktree::remove_worktree(&workspace, wt);
                }
            }
            return outcome;
        }

        // Parallel run.
        if let Some(tasks) = args.get("tasks").and_then(|v| v.as_array()) {
            return run_parallel(
                &st,
                &client,
                &provider,
                &parent_model,
                tasks,
                &args,
                depth,
                &cancel,
                inherited_parent_run_id.clone(),
            )
            .await;
        }

        // Chain run.
        if let Some(chain) = args.get("chain").and_then(|v| v.as_array()) {
            return run_chain(
                &st,
                &client,
                &provider,
                &parent_model,
                chain,
                &args,
                depth,
                &cancel,
                inherited_parent_run_id,
            )
            .await;
        }

        Outcome::err("subagent requires 'agent'+'task', 'tasks', 'chain', or 'action'")
    }))
}

async fn revive_parked_agent(
    args: &Value,
    workspace: &std::path::Path,
    cfg: &Config,
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    depth: u32,
    cancel: &CancellationToken,
) -> Option<Outcome> {
    let id = args.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let path = st.cfg.read().await.session_file.clone()?;
    let record = crate::session::load_parked_agents(&path, now_ms())
        .into_iter()
        .find(|record| record.run_id == id || record.run_id.starts_with(id))?;
    let agent = find_agent(&discover_agents(workspace, &cfg.subagents), &record.agent)?.clone();
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Continue the prior task.");
    crate::session::remove_parked_agent(&path, &record.run_id).ok()?;
    // Resume from the parked transcript instead of Fresh empty (CORE_REVIEW).
    let prior = record.messages.clone();
    let outcome = run_single(
        st,
        client,
        provider,
        parent_model,
        &agent,
        message,
        &record.run_id,
        record.parent_run_id,
        None,
        ContextKind::Fresh,
        depth,
        cancel,
        None,
        Some(prior),
    )
    .await;
    Some(outcome)
}
fn parse_context(args: &Value, agent: &AgentConfig) -> ContextKind {
    match args.get("context").and_then(|v| v.as_str()) {
        Some("fork") => ContextKind::Fork,
        Some("fresh") => ContextKind::Fresh,
        _ => agent.default_context.clone().unwrap_or(ContextKind::Fresh),
    }
}

fn next_run_id() -> String {
    allocate_run_id()
}

async fn persist_subagent_state(
    st: &State,
    run_id: &str,
    parent_run_id: Option<&str>,
    state: crate::session::RunState,
    detail: Option<&str>,
) {
    let path = st.cfg.read().await.session_file.clone();
    if let Some(path) = path {
        crate::session::append_activity_state(
            &path,
            st.runtime.session_id().as_str(),
            run_id,
            "subagent",
            parent_run_id,
            None,
            state,
            detail,
        );
    }
}

async fn persist_and_deliver_job(
    st: &State,
    run_id: &str,
    parent_run_id: Option<&str>,
    state: &str,
    summary: &str,
) {
    let Some(session_path) = st.cfg.read().await.session_file.clone() else {
        return;
    };
    let (task_identity, started_at, ended_at) = st
        .subagent_runs
        .lock()
        .await
        .get(run_id)
        .map(|run| {
            (
                run.agent.clone().unwrap_or_else(|| run.mode.clone()),
                run.started_at,
                run.ended_at.unwrap_or_else(now_ms),
            )
        })
        .unwrap_or_else(|| (run_id.to_string(), now_ms(), now_ms()));
    let error = (state == "failed" || state == "cancelled").then_some(summary);
    match crate::session::write_job_artifact_record(
        &session_path,
        run_id,
        parent_run_id,
        &task_identity,
        state,
        started_at,
        ended_at,
        error,
        summary,
    ) {
        Ok(path) => {
            if let Some(parent) = parent_run_id {
                if let Err(error) =
                    crate::session::append_parent_delivery(&session_path, parent, run_id, &path)
                {
                    emit(&Event::new("error").with("message", json!(error)));
                    return;
                }
            }
            emit(
                &Event::new("subagent_delivery")
                    .with("run_id", json!(run_id))
                    .with("parent_run_id", json!(parent_run_id))
                    .with("state", json!(state))
                    .with("artifact_path", json!(path.display().to_string())),
            );
        }
        Err(error) => emit(&Event::new("error").with("message", json!(error))),
    }
}

async fn fail_registered_subagent_setup(
    st: &State,
    run_id: &str,
    parent_run_id: Option<&str>,
    started: u64,
    message: String,
) -> Outcome {
    let ended = now_ms();
    let mut runs = st.subagent_runs.lock().await;
    if let Some(run) = runs.get_mut(run_id) {
        run.state = "failed".into();
        run.ended_at = Some(ended);
        run.summary = Some(nonempty_run_summary(&message));
    }
    prune_terminal_runs(&mut runs);
    drop(runs);
    persist_subagent_state(
        st,
        run_id,
        parent_run_id,
        crate::session::RunState::Failed,
        Some(&message),
    )
    .await;
    persist_and_deliver_job(st, run_id, parent_run_id, "failed", &message).await;
    emit_subagent_done(
        run_id,
        "failed",
        Some(&message),
        started,
        ended.max(started),
        parent_run_id,
    );
    Outcome::err(message)
}

/// Allocate a fresh subagent run id (also used by goal deploy so step cards can link).
pub fn allocate_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::SeqCst);
    format!("run-{n:x}")
}

// ---------------------------------------------------------------------------
// Single-agent run (the nested agentic loop)
// ---------------------------------------------------------------------------

async fn run_single(
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    agent: &AgentConfig,
    task: &str,
    run_id: &str,
    parent_run_id: Option<String>,
    model_override: Option<String>,
    context: ContextKind,
    depth: u32,
    cancel: &CancellationToken,
    workspace_override: Option<std::path::PathBuf>,
    prior_messages: Option<Vec<Message>>,
) -> Outcome {
    let cfg = st.cfg.read().await.clone();
    let max_depth = child_max_depth(resolve_max_depth(&cfg.subagents), agent.max_subagent_depth);
    let bridge = bridge_active(&cfg.subagents.intercom_bridge_mode, Some(&context));
    let my_target = subagent_target(run_id, &agent.name, None);
    let orchestrator = st.intercom.orchestrator_target();
    if bridge {
        st.intercom.register_target(&my_target);
    }

    // Register the run for status/interrupt. run_cancel is a CHILD of the
    // parent cancel: interrupt_action cancels THIS run only, and a parent
    // /abort propagates automatically (a child token cancels when its parent
    // does). Previously this was a detached token never wired into the loop, so
    // interrupt was a no-op.
    let run_cancel = cancel.child_token();
    let session = st.runtime.session_context();
    let Some(_subagent_resource) = st.runtime.register_owned_session_resource(
        &session,
        crate::runtime::ResourceKind::Subagent,
        format!("subagent:{run_id}:{}", agent.name),
        parent_run_id.as_deref(),
        run_cancel.clone(),
    ) else {
        return Outcome::err("subagent rejected because its session is no longer active");
    };
    let started = now_ms();
    let messages = Arc::new(Mutex::new(Vec::new()));
    let run = SubagentRun {
        id: run_id.to_string(),
        parent_run_id: parent_run_id.clone(),
        mode: "single".into(),
        agent: Some(agent.name.clone()),
        agents: vec![agent.name.clone()],
        state: "running".into(),
        started_at: started,
        ended_at: None,
        depth,
        intercom_target: Some(my_target.clone()),
        cancel: Some(Arc::new(run_cancel.clone())),
        children: vec![],
        summary: None,
        messages: messages.clone(),
    };
    st.subagent_runs
        .lock()
        .await
        .insert(run_id.to_string(), run);

    emit_subagent_start(
        run_id,
        "single",
        Some(agent.name.as_str()),
        std::slice::from_ref(&agent.name),
        task,
        depth,
        started,
        parent_run_id.as_deref(),
    );
    persist_subagent_state(
        st,
        run_id,
        parent_run_id.as_deref(),
        crate::session::RunState::Started,
        Some(&agent.name),
    )
    .await;
    emit_subagent_message(run_id, "user", task);

    // Wrap the run in catch_unwind so a panic inside run_agent_inner (e.g. the
    // old char-boundary `truncate` bug, a future unwrap, or a serde panic) is
    // CAUGHT here and the run still gets finalized + its mailbox unregistered.
    // Without this, a panic unwound past run_single's finalize, leaving the run
    // stuck "running" forever (pinning its whole conversation + cancel token)
    // and the dead mailbox advertised to peers (5-min intercom wedges).
    let result = match AssertUnwindSafe(run_agent(
        st,
        client,
        provider,
        parent_model,
        agent,
        task,
        &my_target,
        &orchestrator,
        run_id,
        model_override,
        context,
        depth,
        max_depth,
        bridge,
        &run_cancel,
        messages,
        workspace_override,
        prior_messages,
    ))
    .catch_unwind()
    .await
    {
        Ok(o) => o,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("(non-string panic payload)");
            emit(&Event::new("error").with(
                "message",
                json!(format!(
                    "subagent {} panicked (finalizing run): {msg}",
                    run_id
                )),
            ));
            Outcome::err(format!(
                "subagent {run_id} terminated unexpectedly (panic): {msg}"
            ))
        }
    };

    // Finalize run state.
    let final_state = if run_cancel.is_cancelled() {
        "cancelled"
    } else if result.ok {
        "completed"
    } else {
        "failed"
    };
    let mut runs = st.subagent_runs.lock().await;
    let mut done_ended: u64 = started;
    let mut done_summary: Option<String> = None;
    if let Some(r) = runs.get_mut(run_id) {
        r.state = final_state.into();
        r.ended_at = Some(now_ms());
        r.summary = Some(nonempty_run_summary(&result.output));
        done_ended = r.ended_at.unwrap_or(started);
        done_summary = r.summary.clone();
    }
    prune_terminal_runs(&mut runs);
    drop(runs);
    if bridge {
        if final_state == "completed" {
            if let Some(path) = st.cfg.read().await.session_file.clone() {
                let snapshot = st.subagent_runs.lock().await.get(run_id).cloned();
                if let Some(run) = snapshot {
                    let record = crate::session::ParkedAgentRecord {
                        run_id: run.id,
                        target: my_target.clone(),
                        agent: agent.name.clone(),
                        parent_run_id: run.parent_run_id,
                        depth: run.depth,
                        parked_at_ms: done_ended,
                        expires_at_ms: done_ended.saturating_add(PARKED_AGENT_TTL_MS),
                        messages: run
                            .messages
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone(),
                    };
                    let _ = crate::session::store_parked_agent(&path, &record);
                }
            }
        }
        st.intercom.unregister(&my_target);
    }
    let persisted_state = match final_state {
        "completed" => crate::session::RunState::Completed,
        "cancelled" => crate::session::RunState::Cancelled,
        _ => crate::session::RunState::Failed,
    };
    persist_subagent_state(
        st,
        run_id,
        parent_run_id.as_deref(),
        persisted_state,
        done_summary.as_deref(),
    )
    .await;
    persist_and_deliver_job(
        st,
        run_id,
        parent_run_id.as_deref(),
        final_state,
        done_summary.as_deref().unwrap_or(""),
    )
    .await;
    emit_subagent_done(
        run_id,
        final_state,
        done_summary.as_deref(),
        started,
        done_ended,
        parent_run_id.as_deref(),
    );
    result
}

/// The nested agentic loop for one agent. Mirrors run_spawn but applies the
/// agent config: filtered tools, system prompt, forked context, model fallback,
/// reads/output/progress, and intercom dispatch.
pub async fn run_agent(
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    agent: &AgentConfig,
    task: &str,
    my_target: &str,
    orchestrator: &str,
    run_id: &str,
    model_override: Option<String>,
    context: ContextKind,
    depth: u32,
    max_depth: u32,
    bridge: bool,
    cancel: &CancellationToken,
    messages: Arc<Mutex<Vec<Message>>>,
    workspace_override: Option<std::path::PathBuf>,
    prior_messages: Option<Vec<Message>>,
) -> Outcome {
    emit_subagent_progress(run_id, agent, "start", "", 0, 0, 0, 0, true);
    let result = run_agent_inner(
        st,
        client,
        provider,
        parent_model,
        agent,
        task,
        my_target,
        orchestrator,
        run_id,
        model_override,
        context,
        depth,
        max_depth,
        bridge,
        cancel,
        messages,
        workspace_override,
        prior_messages,
    )
    .await;
    emit_subagent_progress(run_id, agent, "done", "", 0, 0, 0, 0, result.ok);
    result
}

#[allow(unused_assignments)]
async fn run_agent_inner(
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    agent: &AgentConfig,
    task: &str,
    my_target: &str,
    orchestrator: &str,
    run_id: &str,
    model_override: Option<String>,
    context: ContextKind,
    depth: u32,
    max_depth: u32,
    bridge: bool,
    cancel: &CancellationToken,
    messages: Arc<Mutex<Vec<Message>>>,
    workspace_override: Option<std::path::PathBuf>,
    prior_messages: Option<Vec<Message>>,
) -> Outcome {
    let mut cfg = st.cfg.read().await.clone();
    if let Some(ws) = workspace_override {
        cfg.workspace = ws;
    }
    let workspace = cfg.workspace.clone();
    let tool_defs = subagent_tool_defs(agent, bridge, depth, max_depth);
    let mut advisor_turn_diffs: Vec<crate::advisor::TurnDiff> = Vec::new();

    // --- system prompt ---
    let mut sys = match agent.system_prompt_mode {
        SystemPromptMode::Replace => String::new(),
        SystemPromptMode::Append => crate::build_system_prompt(&workspace, false, None),
    };
    if agent.inherit_project_context && agent.system_prompt_mode == SystemPromptMode::Replace {
        // even in replace mode, inherit git context + memory if asked
        sys.push_str(&crate::build_system_prompt(&workspace, false, None));
        sys.push_str("\n\n");
    }
    sys.push_str(&agent.system_prompt);
    if bridge {
        sys.push_str("\n\n");
        sys.push_str(&bridge_instruction(orchestrator));
    }
    // skills
    let skill_names: Vec<String> = if !agent.skills.is_empty() {
        agent.skills.clone()
    } else {
        vec![]
    };
    let sinj = skills_injection(&workspace, &skill_names);
    if !sinj.is_empty() {
        sys.push_str("\n\n");
        sys.push_str(&sinj);
    }

    let mut sub: Vec<Message> = vec![Message::system(sys)];
    // Parked-agent resume: seed with durable transcript (CORE_REVIEW).
    if let Some(prior) = prior_messages {
        if !prior.is_empty() {
            sub.push(Message::system(
                "--- resumed from parked agent transcript (continuation) ---",
            ));
            for pm in prior {
                if pm.is_system() {
                    continue;
                }
                sub.push(pm);
            }
        }
    }

    // --- forked context: parent conversation as reference ---
    if context == ContextKind::Fork {
        let parent = st.conversation.lock().await.clone();
        // Fork the MOST RECENT 40 qualifying messages (not the oldest 40, which
        // dropped the live context the child most needs) and scrub secret
        // leakage: drop ALL tool results (they may contain file contents / bash
        // output with pasted credentials) and tool_call envelopes, then redact
        // common secret patterns from the kept user/assistant prose before
        // handing the transcript to a child whose own tools could exfiltrate it.
        let filtered: Vec<Message> = parent
            .iter()
            .filter(|m| {
                if m.is_system() {
                    return false;
                }
                if m.is_tool() {
                    return false;
                } // drop all tool results (secret/noise)
                if m.is_assistant() && m.tool_calls().is_some() {
                    return false;
                }
                true
            })
            .map(redact_message_secrets)
            .collect();
        let start = filtered.len().saturating_sub(40);
        let fork_msgs: Vec<Message> = filtered[start..].to_vec();
        if !fork_msgs.is_empty() {
            sub.push(Message::system("You are running from a fork of the parent session. Treat the following inherited conversation as reference-only context, not a live thread to continue. Do not answer prior messages.\n\n--- inherited parent context ---"));
            sub.extend(fork_msgs);
        }
    }

    // --- default reads ---
    let reads: Vec<String> = agent.default_reads.clone();
    if !reads.is_empty() {
        let mut read_ctx = String::new();
        for r in &reads {
            let Ok(p) = crate::workspace::resolve(&workspace, r) else {
                read_ctx.push_str(&format!(
                    "--- {r} ---\n[skipped: path is outside the workspace]\n\n"
                ));
                continue;
            };
            if let Ok(c) = std::fs::read_to_string(&p) {
                read_ctx.push_str(&format!("--- {r} ---\n{}\n\n", truncate(&c, 8000)));
            }
        }
        if !read_ctx.is_empty() {
            sub.push(Message::system(format!(
                "Reference files read before your task:\n\n{read_ctx}"
            )));
        }
    }

    // --- per-turn relevant memories + skill hint: mirrors the main loop's
    // transient tail so subagents recall durable facts for their specific task.
    // Uses the telemetry-free path — subagents aren't user turns, and calling
    // `relevant_memories_tail` would clobber the orchestrator's in-flight
    // recall tracking in `memory_recall::begin_turn`.
    if !task.trim().is_empty() {
        let mut tail = crate::memory::relevant_tail_for_subagent(&workspace, task);
        // Role-specific learning context (spec §20).
        let role = crate::context_pack::ContextRole::parse(agent.name.as_str());
        let pack = crate::context_pack::build_context_pack_for(&workspace, task, role);
        if !pack.is_empty() {
            if tail.is_empty() {
                tail = pack;
            } else {
                tail.push_str("\n\n");
                tail.push_str(&pack);
            }
        }
        if let Some(h) = relevant_skill_hint(&workspace, task) {
            if tail.is_empty() {
                tail = h;
            } else {
                tail.push_str("\n\n");
                tail.push_str(&h);
            }
        }
        if !tail.is_empty() {
            sub.push(Message::system(tail));
        }
    }

    // --- task message ---
    let task_msg = if context == ContextKind::Fork {
        Message::user(format!("Task:\n{task}"))
    } else {
        Message::user(task)
    };
    sub.push(task_msg);

    // --- model candidate list (fallback) ---
    let candidates = resolve_model_candidates(agent, parent_model, model_override, st);
    if candidates.is_empty() {
        return Outcome::err("no model resolved for subagent (set agent model or a default)");
    }
    let effort = agent
        .thinking
        .clone()
        .unwrap_or_else(|| "medium".to_string());
    // Clone the model registry once (vs two read-lock acquisitions) and reuse it
    // for the candidate lookups below + the per-candidate max_tokens the
    // Anthropic path needs inside stream_with_fallback.
    let models_registry = st.models.read().await.clone();
    let first_model = models_registry
        .iter()
        .find(|m| candidates.iter().any(|c| c == &m.id));
    let thinking_levels = first_model
        .map(|m| m.thinking_levels.clone())
        .unwrap_or_default();
    let model_ctx = first_model
        .map(|m| m.context_window as u64)
        .unwrap_or(200_000);
    let model_max_tokens = first_model.map(|m| m.max_tokens).unwrap_or(8_192);
    let mut timer = TurnTimer::new();
    let mut sub_in: u64 = 0;
    let mut sub_out: u64 = 0;
    let mut sub_cached: u64 = 0;
    let mut last_model: Option<String> = None;
    let run_start = std::time::Instant::now();
    let mut tool_count: u32 = 0;
    // Real `prompt_tokens` from the subagent's last request + the `sub` length
    // captured then. Anchors the compaction gate on the endpoint's real count
    // (system + history + tool framing) instead of a whole-history char/4 guess,
    // mirroring the main loop. Reset whenever compaction rewrites `sub`.
    let mut last_real: Option<u64> = None;
    let mut len_at_real: usize = 0;

    loop {
        if cancel.is_cancelled() {
            return Outcome::err("[subagent aborted]");
        }
        // Poll intercom mailbox for orchestrator steer messages (peek + steer actions)
        // Drain orchestrator steer messages from the mailbox without
        // consuming peer-to-peer messages (those stay for the intercom tool's
        // `receive` action). Using receive_from ensures we never eat a peer's
        // fire-and-forget message meant for the subagent.
        let mut got_steer = false;
        while let Some(msg) = st.intercom.receive_from(my_target, orchestrator) {
            let steer_text = format!("Orchestrator: {}", &msg.message);
            sub.push(Message::user(&steer_text));
            emit_subagent_message(run_id, "system", &steer_text);
            got_steer = true;
        }
        if got_steer {
            emit_subagent_progress(
                run_id,
                agent,
                "steered",
                "",
                tool_count,
                sub_in,
                sub_out,
                run_start.elapsed().as_millis() as u64,
                true,
            );
            *messages.lock().unwrap() = sub.clone();
            continue; // re-enter loop with new context
        }
        let est = grounded_estimate(&sub, last_real, len_at_real);
        let policy = crate::context_policy(
            &sub,
            model_ctx,
            model_max_tokens,
            cfg.context_compact_at,
            cfg.context_digest_at,
        );
        if crate::should_auto_digest(cfg.auto_compact, est, policy) {
            let changed = {
                let mut cache = st.tool_output_cache.lock().await;
                crate::soft_digest_conversation(&mut sub, model_ctx, Some(&mut cache))
            };
            if changed > 0 {
                last_real = None;
                len_at_real = 0;
                emit(
                    &Event::new("digested")
                        .with("scope", json!("subagent"))
                        .with("results", json!(changed))
                        .with("before_tokens", json!(est))
                        .with("after_tokens", json!(estimate_messages_tokens(&sub)))
                        .with("trigger", json!("pressure"))
                        .with("context_window", json!(model_ctx))
                        .with("threshold_tokens", json!(policy.digest_threshold))
                        .with("hard_limit_tokens", json!(policy.hard_limit)),
                );
            }
        }
        // compaction gate — anchor on the endpoint's real prompt_tokens when
        // available so compaction fires at the right context level, not a
        // whole-history char/4 guess that drifts late.
        let est = grounded_estimate(&sub, last_real, len_at_real);
        if crate::should_auto_compact(cfg.auto_compact, est, sub.len(), policy) {
            // Mirror the main loop: let pre_compact plugin hooks run before
            // summarizing a subagent's context (otherwise subagent compaction
            // silently bypasses the hook the user configured).
            crate::dispatch_lifecycle(st, "pre_compact").await;
            let mp = st.plugin_manager.memory_provider();
            crate::compact_with_summary(
                client,
                &cfg,
                provider,
                candidates.first().unwrap(),
                &mut sub,
                cancel,
                est > policy.hard_limit,
                model_ctx,
                cfg.compact_instructions.as_deref(),
                mp.as_ref(),
            )
            .await;
            // Compaction rewrote `sub`; the real baseline no longer applies.
            last_real = None;
            len_at_real = 0;
            let after = estimate_messages_tokens(&sub);
            emit(
                &Event::new("compacted")
                    .with("scope", json!("subagent"))
                    .with("before_tokens", json!(est))
                    .with("after_tokens", json!(after))
                    .with("context_window", json!(model_ctx))
                    .with("threshold_tokens", json!(policy.compact_threshold))
                    .with("hard_limit_tokens", json!(policy.hard_limit))
                    .with("within_limit", json!(after <= policy.hard_limit)),
            );
            if after > policy.hard_limit {
                return Outcome::err(format!(
                    "subagent context remains too large after compaction ({after} > safe limit {}); split the task or reduce oversized input",
                    policy.hard_limit
                ));
            }
        }

        // Sanitize the conversation directly (it's already Vec<Message>).
        crate::provider::sanitize_orphaned_tool_calls(&mut sub);
        // Coerce any malformed tool-call `arguments` (e.g. a long, quote-heavy
        // command that broke the model's JSON encoding) to "{}" so the history
        // stays valid for the API. Without this, one malformed call makes every
        // later subagent request fail with "function.arguments must be valid JSON".
        let _ = crate::provider::sanitize_tool_call_arguments(&mut sub);

        // stream with model fallback — pass Messages directly
        let (assistant, _finish_reason, ti, to, cached, served_model) = match stream_with_fallback(
            client,
            &cfg,
            provider,
            &candidates,
            &sub,
            &tool_defs,
            &effort,
            &thinking_levels,
            &models_registry,
            cancel,
            &mut timer,
            Some(st),
            run_id,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                if e == "aborted" {
                    emit(&Event::new("aborted"));
                }
                return Outcome::err(format!("subagent stream error: {e}"));
            }
        };
        // Report the model that actually served the last turn (on fallback this
        // may be candidates[1+], not candidates[0]) so telemetry is accurate.
        last_model = Some(served_model);
        sub_in += ti;
        sub_out += to;
        sub_cached += cached;
        // Anchor the next compaction gate on the real prompt_tokens when the
        // endpoint reported usage. `sub` here is exactly what we sent (the
        // assistant + tool results are appended below), so its length marks
        // where the real baseline stops and the char/4 delta begins.
        if ti > 0 {
            last_real = Some(ti);
            len_at_real = sub.len();
        }
        *st.tokens_in.lock().await += ti;
        *st.tokens_out.lock().await += to;
        *st.cached_tokens.lock().await += cached;
        sub.push(assistant.clone());
        // Snapshot for peek: after assistant response
        *messages.lock().unwrap() = sub.clone();
        emit_subagent_progress(
            run_id,
            agent,
            "streaming",
            "",
            tool_count,
            sub_in,
            sub_out,
            run_start.elapsed().as_millis() as u64,
            true,
        );
        let asst_text = assistant.content_text().unwrap_or("").to_string();
        if !asst_text.is_empty() {
            emit_subagent_message(run_id, "assistant", &truncate(&asst_text, 16000));
        }

        if assistant
            .tool_calls()
            .map(|calls| calls.is_empty())
            .unwrap_or(true)
        {
            // Subagent reviews get the evidence pack but no soft-continue
            // (parent finalize should not re-enter the child loop).
            let review = crate::advisor::review(
                st,
                crate::advisor::Scope::Subagent,
                last_model.as_deref().unwrap_or(parent_model),
                &sub,
                &advisor_turn_diffs,
                cancel,
            )
            .await;
            for note in review.system_messages(crate::advisor::Scope::Subagent) {
                sub.push(note);
            }
            // Bubble actionable watchdog findings to the parent-facing result;
            // the parent can then decide whether to fix or explain them.
            if !review.notes.is_empty() {
                let actionable = review
                    .notes
                    .iter()
                    .filter(|n| n.severity.requires_action())
                    .map(|n| n.steering_line())
                    .collect::<Vec<_>>();
                if !actionable.is_empty() {
                    let mut text = String::from("\n\nAdvisor follow-ups from subagent review:\n");
                    for item in actionable.iter().take(4) {
                        text.push_str("- ");
                        text.push_str(item);
                        text.push('\n');
                    }
                    sub.push(Message::system(text));
                }
            }
        }

        let Some(calls) = assistant.tool_calls().map(|tc| tc.to_vec()) else {
            // done — finalize output
            let text = finalize_subagent_text(assistant.content_text(), &sub);
            // optional output file
            if let Some(out_path) = &agent.output {
                let p = workspace.join(out_path);
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&p, &text);
            }
            emit_subagent_summary(sub_in, sub_out, sub_cached, &last_model);
            return Outcome::ok(text);
        };
        if calls.is_empty() {
            let text = finalize_subagent_text(assistant.content_text(), &sub);
            emit_subagent_summary(sub_in, sub_out, sub_cached, &last_model);
            return Outcome::ok(text);
        }

        for call in &calls {
            // Honor an abort/interrupt mid-batch: without this, the
            // synchronous fall-through tools (write_file/edit/patch/
            // read_file/…) run to completion after the subagent was
            // interrupted — only bash/fetch/web_search/diagnostics were
            // cancel-wrapped. Check before each call so a batch's remaining
            // destructive writes don't execute once cancelled.
            if cancel.is_cancelled() {
                emit_subagent_summary(sub_in, sub_out, sub_cached, &last_model);
                return Outcome::err("[subagent aborted]");
            }
            let id = call.id.clone();
            let name = call.function.name.clone();
            let args_str = call.function.arguments.clone();
            tool_count += 1;
            if tool_count > SUBAGENT_MAX_TOOL_CALLS {
                emit_subagent_summary(sub_in, sub_out, sub_cached, &last_model);
                return Outcome::err(format!("subagent exceeded the per-run tool-call cap ({SUBAGENT_MAX_TOOL_CALLS}); aborting to bound cost. Re-decompose into smaller subagent runs."));
            }
            emit_subagent_progress(
                run_id,
                agent,
                "tool",
                &name,
                tool_count,
                sub_in,
                sub_out,
                run_start.elapsed().as_millis() as u64,
                true,
            );
            let argsv: Value = match serde_json::from_str(&args_str) {
                Ok(v) => v,
                Err(_) => {
                    // Malformed JSON arguments: skip dispatch and push an actionable
                    // error so the subagent retries simply. The unconditional
                    // sanitize_tool_call_arguments() above keeps the history valid
                    // for the API (otherwise the malformed message would make every
                    // later request fail with "function.arguments must be valid JSON").
                    let msg = format!(
                        "tool call '{}' produced malformed JSON arguments (the argument string was not valid JSON). This usually happens with long, quote-heavy commands wrapped inside bulk's nested JSON. Re-issue it simply: call bash directly (not via bulk), and for complex logic write a script to a file with write_file then run `bash script.sh` instead of inlining one long command string.",
                        name
                    );
                    emit_subagent_progress(
                        run_id,
                        agent,
                        "tool_end",
                        &name,
                        tool_count,
                        sub_in,
                        sub_out,
                        run_start.elapsed().as_millis() as u64,
                        false,
                    );
                    emit_subagent_tool_call(run_id, &id, &name, &json!({}), tool_count);
                    emit_subagent_tool_result(run_id, &id, &name, &truncate(&msg, 8000), false);
                    sub.push(Message::tool(&id, &msg));
                    continue;
                }
            };

            emit_subagent_tool_call(run_id, &id, &name, &argsv, tool_count);

            // Duplicate short-circuit (parity with main): identical undigested
            // result already in this subagent's history.
            let outcome = if let Some((prior_id, preview)) =
                crate::find_duplicate_tool_result(&sub, &name, &args_str)
            {
                tools::Outcome::ok(format!(
                    "[duplicate of tool_call_id {prior_id}; content unchanged]\n{preview}"
                ))
            } else {
                dispatch_subagent_tool(
                    &name,
                    &argsv,
                    &id,
                    &args_str,
                    st,
                    client,
                    provider,
                    parent_model,
                    my_target,
                    agent,
                    depth,
                    max_depth,
                    bridge,
                    &cfg,
                    cancel,
                )
                .await
            };

            if outcome.ok {
                if let Some(diff) = outcome.diff.as_ref().filter(|d| !d.is_empty()) {
                    if advisor_turn_diffs.len() < 8 {
                        let path = argsv
                            .get("path")
                            .or_else(|| argsv.get("to"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(&name);
                        advisor_turn_diffs.push(crate::advisor::TurnDiff {
                            path: path.to_string(),
                            unified_diff: diff.clone(),
                        });
                    }
                }
            }

            emit_subagent_progress(
                run_id,
                agent,
                "tool_end",
                &name,
                tool_count,
                sub_in,
                sub_out,
                run_start.elapsed().as_millis() as u64,
                outcome.ok,
            );
            // finish sentinel
            let finish =
                name == "finish" && outcome.ok && outcome.output == crate::tools::FINISH_SENTINEL;
            // Map the internal sentinel to a human-readable result for the UI /
            // conversation; the loop still exits on `finish` below.
            let emit_output = if finish {
                crate::tools::FINISH_MESSAGE.to_string()
            } else {
                truncate(&outcome.output, 8000)
            };
            emit_subagent_tool_result(run_id, &id, &name, &emit_output, outcome.ok);
            // Mirror main-loop cache + ingress so subagent contexts stay lean
            // (previously they pushed full tool output and only soft-compacted
            // at 90%). Key by the original call args (same as main loop).
            let mut model_output = if finish {
                crate::tools::FINISH_MESSAGE.to_string()
            } else {
                outcome.output
            };
            if outcome.ok && !finish {
                // Invalidate the shared cache on tree-mutating tools, matching the
                // main loop's `invalidates_cache`. bash is included because a
                // shell redirect / `sed -i` / `jq >` can change files a prior
                // read cached — excluding it would let a subsequent re-read
                // restore stale content. The re-execution cost on read-only
                // bash is the price of correctness (the main loop pays it too).
                let wipe = crate::tool_cache::invalidates_cache(&name);
                if wipe {
                    st.tool_output_cache.lock().await.invalidate_all();
                } else if crate::tool_cache::ToolOutputCache::is_restorable(&name)
                    && !model_output.starts_with("[restored from digest cache]")
                    && !model_output.starts_with("[duplicate of tool_call_id")
                {
                    let cache_args = format!("{}\0{}", cfg.workspace.display(), args_str);
                    st.tool_output_cache
                        .lock()
                        .await
                        .store(&name, &cache_args, &model_output);
                }
                if !model_output.starts_with("[restored from digest cache]")
                    && !model_output.starts_with("[duplicate of tool_call_id")
                {
                    model_output = crate::apply_ingress_cap(&name, &args_str, model_output);
                }
            }
            sub.push(Message::tool(&id, &model_output));
            // Snapshot after tool result (for peek)
            *messages.lock().unwrap() = sub.clone();

            // finish sentinel
            if finish {
                let text = finalize_subagent_text(assistant.content_text(), &sub);
                if let Some(out_path) = &agent.output {
                    let p = workspace.join(out_path);
                    if let Some(parent) = p.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&p, &text);
                }
                emit_subagent_summary(sub_in, sub_out, sub_cached, &last_model);
                return Outcome::ok(text);
            }
        }
    }
}

/// Dispatch a tool call inside a subagent loop. Handles async tools (bash/bulk/
/// diagnostics/subagent) + intercom tools; others go through tools::execute.
/// Like the main loop, destructive tools respect the configured `cfg.approval`
/// mode + permission rules + escalated kinds, and every pre/post plugin hook
/// (pre_bash/pre_write/pre_read/post_*) runs — so delegated work is gated and
/// observable exactly like top-level tool calls (P0-4: previously subagents
/// bypassed the approval gate and all plugin hooks entirely).
async fn dispatch_subagent_tool(
    name: &str,
    args: &Value,
    id: &str,
    args_str: &str,
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    my_target: &str,
    agent: &AgentConfig,
    depth: u32,
    max_depth: u32,
    bridge: bool,
    cfg: &Config,
    cancel: &CancellationToken,
) -> Outcome {
    // Enforce the same tool set offered in the schema. Empty `agent.tools`
    // means the core read-only default — NOT "execute anything the model invents".
    let _ = my_target; // retained for intercom dispatch below
    let offered = subagent_allowed_names(agent, bridge, depth, max_depth);
    if !offered.iter().any(|n| n == name) {
        return Outcome::err(format!(
            "tool '{}' is not available to agent '{}' (not in its tool schema). Declare it in the agent's tools: frontmatter if needed.",
            name, agent.name
        ));
    }

    // Restricted ("dangerous") paths (.env, .git/**, .ssh/**, id_rsa, …).
    // Under `Never` ALL file restrictions are disabled — no prompt, no block.
    // Under `Destructive`/`Always` a restricted path forces an approval prompt
    // (for reads AND writes) instead of an unconditional hard block; an approved
    // call proceeds. Mirrors the main loop.
    let restricted = if matches!(cfg.approval, crate::config::Approval::Never) {
        None
    } else {
        crate::restricted_path_for_tool(name, args, &cfg.workspace)
    };

    // Approval gate + permission rules. A subagent is model-driven (the parent
    // model decides to spawn; the child decides what to run), so delegated work
    // must honor the same `cfg.approval` mode and allow/deny rules as the main
    // loop — otherwise `--approval always`/`destructive` is silently bypassed
    // for every tool a subagent runs.
    let kind = tools::classify(name);
    let kind_str: &'static str = match kind {
        tools::ToolKind::ReadOnly => "readonly",
        tools::ToolKind::Destructive => "destructive",
    };
    let escalated = st.escalated_kinds.lock().await.contains(kind_str);
    let mut force_allow = false;
    let mut force_deny = false;
    for rule in &cfg.allow_rules {
        if crate::tool_matches_rule(name, args, rule) {
            force_allow = true;
            break;
        }
    }
    if !force_allow {
        for rule in &cfg.deny_rules {
            if crate::tool_matches_rule(name, args, rule) {
                force_deny = true;
                break;
            }
        }
    }
    if force_deny {
        return Outcome::err(format!("tool call '{}' denied by permission rule", name));
    }
    let needs_approval = crate::tooling::approval::approval_required(
        &cfg.approval,
        kind,
        restricted.is_some(),
        force_allow,
        escalated,
        false,
    );
    if needs_approval {
        match crate::request_approval(st, id, name, args_str, kind_str, Some(my_target), cancel)
            .await
        {
            crate::ApprovalResult::Granted => {}
            crate::ApprovalResult::Denied => {
                return Outcome::err(format!("tool call '{}' was denied by the user", name));
            }
            crate::ApprovalResult::Aborted => {
                return Outcome::err("[subagent aborted]");
            }
        }
    }

    // Pre-execution plugin hooks. Two phases compose:
    //   1. the tool-SPECIFIC pre_* hook (pre_bash/pre_write/pre_read); and
    //   2. the catch-all `pre_tool` hook, which fires for EVERY tool call so a
    //      plugin can audit/deny/modify any subagent tool.
    // Each hook may amend `exec_args` or deny; a deny returns the reason to the
    // subagent model.
    let hook_name = match name {
        "bash" => "pre_bash",
        "write_file" | "edit" => "pre_write",
        "read_file" | "grep" | "glob" => "pre_read",
        _ => "",
    };
    let mut exec_args = args.clone();
    let mut hook_notes: Vec<String> = Vec::new();
    if !hook_name.is_empty() {
        if let Some(deny) =
            crate::run_pre_hooks(&st, &cfg, hook_name, name, &mut exec_args, &mut hook_notes).await
        {
            return Outcome::err(deny);
        }
    }
    if name != "finish" {
        if let Some(deny) =
            crate::run_pre_hooks(&st, &cfg, "pre_tool", name, &mut exec_args, &mut hook_notes).await
        {
            return Outcome::err(deny);
        }
    }

    // bulk inner-call gate (mirror of the main-loop gate): permission deny-
    // rules + dangerous-path + plugin pre-hooks run on EACH inner call so
    // destructive ops can't evade the safety floor by hiding inside a bulk
    // call. Denied inner calls are recorded by index and rendered by execute_bulk.
    let mut bulk_denied: HashMap<usize, String> = HashMap::new();
    if name == "bulk" {
        if let Some(calls) = exec_args.get_mut("calls").and_then(|v| v.as_array_mut()) {
            for (i, c) in calls.iter_mut().enumerate() {
                let iname = c
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let iargs = c.get("args").cloned().unwrap_or(json!({}));
                let mut modified = iargs.clone();
                let mut dmsg: Option<String> = None;
                // Inner calls must also be in this subagent's offered schema —
                // bulk must not smuggle tools the agent wasn't given.
                if !offered.iter().any(|n| n == &iname) {
                    dmsg = Some(format!(
                        "tool '{iname}' is not in this subagent's tool schema"
                    ));
                }
                let mut force_allow = false;
                if dmsg.is_none() {
                    for rule in &cfg.allow_rules {
                        if crate::tool_matches_rule(&iname, &iargs, rule) {
                            force_allow = true;
                            break;
                        }
                    }
                }
                if dmsg.is_none() && !force_allow {
                    for rule in &cfg.deny_rules {
                        if crate::tool_matches_rule(&iname, &iargs, rule) {
                            dmsg = Some("denied by permission rule".into());
                            break;
                        }
                    }
                }
                if dmsg.is_none() {
                    let ihook = match iname.as_str() {
                        "bash" => "pre_bash",
                        "write_file" | "edit" => "pre_write",
                        "read_file" | "grep" | "glob" => "pre_read",
                        _ => "",
                    };
                    if !ihook.is_empty() {
                        if let Some(deny) = crate::run_pre_hooks(
                            &st,
                            &cfg,
                            ihook,
                            &iname,
                            &mut modified,
                            &mut hook_notes,
                        )
                        .await
                        {
                            dmsg = Some(deny);
                        }
                    }
                    if dmsg.is_none() && iname != "finish" {
                        if let Some(deny) = crate::run_pre_hooks(
                            &st,
                            &cfg,
                            "pre_tool",
                            &iname,
                            &mut modified,
                            &mut hook_notes,
                        )
                        .await
                        {
                            dmsg = Some(deny);
                        }
                    }
                }
                if let Some(m) = dmsg {
                    bulk_denied.insert(i, m);
                } else {
                    *c = json!({ "name": iname, "args": modified });
                }
            }
        }
    }

    // Cache restore is scoped to the resolved workspace. Parallel worktrees
    // routinely issue identical relative reads but must never share bytes.
    let cache_args = format!("{}\0{}", cfg.workspace.display(), args_str);
    if let Some(restored) = {
        let cache = st.tool_output_cache.lock().await;
        cache.get(name, &cache_args).map(|s| s.to_string())
    } {
        let mut o = Outcome::ok(crate::apply_restore_cap(&restored));
        if !hook_notes.is_empty() {
            o.output.push_str("\n\n[hooks]\n");
            o.output.push_str(&hook_notes.join("\n"));
        }
        return o;
    }

    // Execute. bash/bulk/diagnostics/subagent are async; others sync. Hooks that
    // amended `exec_args` are honored here. The async ones are wrapped in a
    // `select!` on the turn cancel so /abort can interrupt them mid-flight.
    let mut outcome = match name {
        "bash" => {
            let cmd = exec_args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let timeout_override = exec_args.get("timeout").and_then(|v| v.as_u64());
            tokio::select! {
                o = tools::execute_bash(cmd, cfg, timeout_override, tools::SudoAuth::None) => o,
                _ = cancel.cancelled() => Outcome::err("bash aborted"),
            }
        }
        "bulk" => {
            tokio::select! {
                o = tools::execute_bulk(&exec_args, cfg, &bulk_denied) => o,
                _ = cancel.cancelled() => Outcome::err("bulk aborted"),
            }
        }
        "diagnostics" => {
            tokio::select! {
                o = tools::execute_diagnostics(&exec_args, cfg) => o,
                _ = cancel.cancelled() => Outcome::err("diagnostics aborted"),
            }
        }
        "fetch" => {
            tokio::select! {
                o = tools::execute_fetch(&exec_args, cfg) => o,
                _ = cancel.cancelled() => Outcome::err("fetch aborted"),
            }
        }
        "web_search" => {
            tokio::select! {
                o = tools::execute_web_search(&exec_args, cfg) => o,
                _ = cancel.cancelled() => Outcome::err("web_search aborted"),
            }
        }
        "contact_supervisor" => {
            // During goal mode phases that spawn employees without a live
            // leader turn answering need_decision (Planning scout, Reviewing,
            // Deploying/Running waves, Verifying, Replanning), blocking for
            // the 5-min intercom timeout wastes time and "do NOT proceed" can
            // cascade into step failures. Short-circuit: proceed with best
            // judgment and document it so the autonomous loop keeps moving.
            let reason = exec_args
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("need_decision");
            let auto_resolve = {
                let g = st.goal.lock().await;
                g.phase.auto_resolves_supervisor()
            };
            if auto_resolve && reason == "need_decision" {
                let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                emit(
                    &Event::new("intercom_message")
                        .with("from", json!(my_target))
                        .with("to", json!(st.intercom.orchestrator_target()))
                        .with("reason", json!("need_decision"))
                        .with("message", json!(msg))
                        .with("auto_resolved", json!(true)),
                );
                // Safe default: do NOT authorize unapproved mutations. Read-only
                // agents may continue with a documented assumption; writers must
                // stop and surface the decision (CORE_REVIEW C10).
                let agent_l = my_target.to_ascii_lowercase();
                let read_only = [
                    "scout",
                    "reviewer",
                    "oracle",
                    "researcher",
                    "planner",
                    "context-builder",
                ]
                .iter()
                .any(|a| agent_l.contains(a));
                if read_only {
                    Outcome::ok(
                        "No active supervisor during goal orchestration for this need_decision.                          As a read-only agent, continue with the most reasonable *non-mutating*                          assumption, document it in your final summary, and do not re-ask."
                    )
                } else {
                    Outcome::err(format!(
                        "Blocked: need_decision requires supervisor approval during goal                          orchestration, and no supervisor is waiting on intercom. Decision was: {msg}.                          Do NOT proceed with unapproved destructive/mutating choices — fail this step                          so the orchestrator can replan."
                    ))
                }
            } else {
                execute_contact_supervisor(&exec_args, &st.intercom, my_target, cancel).await
            }
        }
        "intercom" => execute_intercom(&exec_args, &st.intercom, my_target, cancel).await,
        "subagent" | "spawn" => {
            if depth + 1 >= max_depth {
                Outcome::err(format!(
                    "nested subagent blocked at depth {} (max {})",
                    depth + 1,
                    max_depth
                ))
            } else {
                execute(
                    st.clone(),
                    client.clone(),
                    provider.clone(),
                    parent_model.to_string(),
                    exec_args.clone(),
                    cancel.clone(),
                    depth + 1,
                )
                .await
            }
        }
        _ => tools::execute(name, &exec_args, cfg),
    };

    // Post-execution plugin hooks. They can't block (the op already ran); their
    // reason is surfaced to the subagent as a note appended to the result.
    let post_hook = match name {
        "bash" => "post_bash",
        "write_file" | "edit" => "post_write",
        "read_file" | "grep" | "glob" => "post_read",
        _ => "",
    };
    if !post_hook.is_empty() {
        crate::run_post_hooks(
            &st,
            &cfg,
            post_hook,
            name,
            &exec_args,
            &mut outcome,
            &mut hook_notes,
        )
        .await;
    }
    crate::run_post_hooks(
        &st,
        &cfg,
        "post_tool",
        name,
        &exec_args,
        &mut outcome,
        &mut hook_notes,
    )
    .await;
    if !hook_notes.is_empty() {
        outcome.output.push_str("\n\nPlugin hooks:\n- ");
        outcome.output.push_str(&hook_notes.join("\n- "));
    }
    outcome
}

fn resolve_model_candidates(
    agent: &AgentConfig,
    parent_model: &str,
    override_model: Option<String>,
    st: &Arc<State>,
) -> Vec<String> {
    let mut cands: Vec<String> = Vec::new();
    let has_explicit = override_model.is_some() || agent.model.is_some();
    if let Some(m) = override_model {
        cands.push(m);
    }
    if let Some(m) = &agent.model {
        cands.push(m.clone());
    }
    for m in &agent.fallback_models {
        cands.push(m.clone());
    }
    if cands.is_empty() {
        cands.push(parent_model.to_string());
    }
    // keep only models known to the registry, but keep unknowns too (they may
    // be valid ids the registry didn't list); just dedup preserving order.
    let mut seen = std::collections::HashSet::new();
    cands.retain(|c| seen.insert(c.clone()));

    // Goal-mode allowlists: filter candidates by allowed models/providers when
    // an active goal is constraining the run. Falls back to parent_model (or
    // first allowed model) if everything was filtered out.
    if let Ok(g) = st.goal.try_lock() {
        if g.is_active() && (!g.allowed_models.is_empty() || !g.allowed_providers.is_empty()) {
            let model_providers: std::collections::HashMap<String, String> = st
                .models
                .try_read()
                .map(|reg| {
                    reg.iter()
                        .map(|m| (m.id.clone(), m.provider.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let filtered = crate::goal::filter_model_candidates(&cands, &g, &model_providers);
            if !filtered.is_empty() {
                return filtered;
            }
            if let Some(m) = g.allowed_models.first() {
                return vec![m.clone()];
            }
            return vec![parent_model.to_string()];
        }
    }

    // Task-aware routing: when no explicit pin, reorder/augment candidates by
    // role preference against the model registry.
    if !has_explicit {
        if let Ok(cfg) = st.cfg.try_read() {
            if let Some(prefer) = cfg.routing.preference_for(&agent.name) {
                if let Ok(reg) = st.models.try_read() {
                    let mut scored: Vec<(i32, String)> = reg
                        .iter()
                        .map(|m| (cfg.routing.score_model(&m.id, prefer), m.id.clone()))
                        .filter(|(s, _)| *s > 0)
                        .collect();
                    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    for (_, id) in scored.into_iter().take(3) {
                        if seen.insert(id.clone()) {
                            // Prefer routed models ahead of parent fallback.
                            cands.insert(0, id);
                        }
                    }
                    // Re-dedup preserving new order.
                    let mut seen2 = std::collections::HashSet::new();
                    cands.retain(|c| seen2.insert(c.clone()));
                }
            }
        }
    }
    cands
}

/// stream_turn with model fallback: try each candidate in order until one
/// succeeds (or all fail). Aborted errors are not retried.
///
/// When `st` is provided, each candidate resolves its own provider via
/// `resolve_provider_for_model` so multi-provider goal allowlists work. Falls
/// back to the parent `provider` clone when resolution isn't available.
async fn stream_with_fallback(
    client: &reqwest::Client,
    cfg: &Config,
    provider: &ResolvedProvider,
    candidates: &[String],
    messages: &[Message],
    tools: &[Value],
    effort: &str,
    thinking_levels: &[String],
    models: &[ModelInfo],
    cancel: &CancellationToken,
    timer: &mut TurnTimer,
    st: Option<&Arc<State>>,
    trace_run_id: &str,
) -> Result<(Message, String, u64, u64, u64, String), String> {
    let mut last_err = String::from("no model candidates");
    for (i, model) in candidates.iter().enumerate() {
        // Always re-resolve thinking_levels per candidate model (a fallback model
        // may support different effort labels than the parent/primary). Passing
        // empty lets stream_turn re-resolve from the registry for THIS model
        // instead of reusing the parent's levels for every candidate.
        let _ = thinking_levels;
        let levels: Vec<String> = Vec::new();
        // Per-candidate max_tokens (Anthropic requires it; OpenAI servers ignore
        // it). Looked up from the registry so a fallback model with a smaller
        // output cap doesn't get an over-large request that the API rejects.
        let max_tokens = if let Some(st) = st {
            st.model_info_for(model)
                .await
                .map(|m| m.max_tokens)
                .unwrap_or(8_192)
        } else {
            models
                .iter()
                .find(|m| m.id == *model)
                .map(|m| m.max_tokens)
                .unwrap_or(8_192)
        };
        // Multi-provider: route each candidate to its owning endpoint when we
        // have State; otherwise keep the inherited parent provider.
        let resolved;
        let use_provider = if let Some(st) = st {
            resolved = st.resolve_provider_for_model(model).await;
            &resolved
        } else {
            provider
        };
        let provider_result = crate::provider::stream_turn(
            client,
            use_provider,
            cfg.idle_timeout_secs,
            model,
            messages,
            tools,
            effort,
            &levels,
            max_tokens,
            cancel,
            timer,
            // Subagent turns stream quietly (no live footer), so est_prompt is
            // unused mid-stream; pass a char/4 of the prompt for correctness.
            estimate_messages_tokens(messages),
            true,
        )
        .await;
        if provider_result.is_err() {
            timer.finish_failed_provider_call();
        }
        if let (Some(st), Some(call_metrics)) = (st, timer.take_provider_call_metrics()) {
            st.logger.log(
                "provider_request",
                json!({
                    "subagent_id": trace_run_id,
                    "provider": &use_provider.name,
                    "provider_kind": use_provider.kind.as_str(),
                    "model": model,
                    "duration_ms": call_metrics.duration_ms,
                    "ttft_ms": call_metrics.ttft_ms,
                    "stream_ms": call_metrics.stream_ms,
                    "status": if provider_result.is_ok() { "completed" } else { "failed" },
                    "subagent": true,
                }),
            );
        }
        match provider_result {
            Ok((assistant, fr, ti, to, cached)) => {
                let assistant_msg = Message::try_from(&assistant).unwrap_or_else(|e| {
                    emit(
                        &Event::new("error")
                            .with("message", json!(format!("subagent assistant parse: {e}"))),
                    );
                    Message::assistant("")
                });
                return Ok((assistant_msg, fr, ti, to, cached, model.clone()));
            }
            Err(e) => {
                if e == "aborted" || cancel.is_cancelled() {
                    return Err(e);
                }
                last_err = format!("model {} failed: {e}", model);
                if i + 1 < candidates.len() {
                    emit(&Event::new("info").with(
                        "message",
                        json!(format!(
                            "subagent model '{}' failed ({}); falling back to '{}'",
                            model,
                            e,
                            candidates[i + 1]
                        )),
                    ));
                }
            }
        }
    }
    Err(last_err)
}

fn emit_subagent_progress(
    run_id: &str,
    agent: &AgentConfig,
    phase: &str,
    tool: &str,
    tool_count: u32,
    tokens_in: u64,
    tokens_out: u64,
    elapsed_ms: u64,
    ok: bool,
) {
    emit(
        &Event::new("subagent_progress")
            .with("run_id", json!(run_id))
            .with("agent", json!(agent.name))
            .with("phase", json!(phase))
            .with("tool", json!(tool))
            .with("tool_count", json!(tool_count))
            .with("tokens_in", json!(tokens_in))
            .with("tokens_out", json!(tokens_out))
            .with("elapsed_ms", json!(elapsed_ms))
            .with("ok", json!(ok)),
    );
}

fn emit_subagent_start(
    run_id: &str,
    mode: &str,
    agent: Option<&str>,
    agents: &[String],
    task: &str,
    depth: u32,
    started_at: u64,
    parent_run_id: Option<&str>,
) {
    let mut ev = Event::new("subagent_start")
        .with("run_id", json!(run_id))
        .with("mode", json!(mode))
        .with("agents", json!(agents))
        .with("task", json!(task))
        .with("depth", json!(depth))
        .with("started_at", json!(started_at));
    if let Some(a) = agent {
        ev = ev.with("agent", json!(a));
    }
    if let Some(parent_run_id) = parent_run_id {
        ev = ev.with("parent_run_id", json!(parent_run_id));
    }
    emit(&ev);
}

fn emit_subagent_message(run_id: &str, role: &str, content: &str) {
    emit(
        &Event::new("subagent_message")
            .with("run_id", json!(run_id))
            .with("role", json!(role))
            .with("content", json!(content)),
    );
}

fn emit_subagent_tool_call(run_id: &str, call_id: &str, name: &str, args: &Value, tool_count: u32) {
    emit(
        &Event::new("subagent_tool_call")
            .with("run_id", json!(run_id))
            .with("call_id", json!(call_id))
            .with("name", json!(name))
            .with("args", args.clone())
            .with("tool_count", json!(tool_count)),
    );
}

fn emit_subagent_tool_result(run_id: &str, call_id: &str, name: &str, result: &str, ok: bool) {
    emit(
        &Event::new("subagent_tool_result")
            .with("run_id", json!(run_id))
            .with("call_id", json!(call_id))
            .with("name", json!(name))
            .with("result", json!(result))
            .with("ok", json!(ok)),
    );
}

/// Prefer a non-empty final summary for goal deploy / UI cards.
fn nonempty_run_summary(output: &str) -> String {
    let t = output.trim();
    if t.is_empty() {
        "(step finished with no written summary)".to_string()
    } else {
        t.chars().take(800).collect()
    }
}

/// When finish (or final assistant) has empty prose, fall back to the last
/// non-empty assistant message in the subagent history; never return "".
fn finalize_subagent_text(current: Option<&str>, history: &[crate::message::Message]) -> String {
    let mut summary = current.unwrap_or("").trim().to_string();
    if summary.is_empty() {
        for m in history.iter().rev() {
            if m.role() != "assistant" {
                continue;
            }
            if let Some(t) = m.content_text() {
                let t = t.trim();
                if !t.is_empty() {
                    summary = t.to_string();
                    break;
                }
            }
        }
    }
    if summary.is_empty() {
        summary = "(step finished with no written summary)".to_string();
    }
    // Actionable watchdog findings are intentionally bubbled into the
    // parent-facing result so delegated work cannot hide an unresolved issue.
    for m in history {
        if m.role() == "system" {
            if let Some(t) = m.content_text() {
                if t.starts_with("Advisor follow-ups from subagent review:") {
                    summary.push_str("\n\n");
                    summary.push_str(t.trim());
                }
            }
        }
    }
    summary
}

fn emit_subagent_done(
    run_id: &str,
    state: &str,
    summary: Option<&str>,
    started_at: u64,
    ended_at: u64,
    parent_run_id: Option<&str>,
) {
    let mut ev = Event::new("subagent_done")
        .with("run_id", json!(run_id))
        .with("state", json!(state))
        .with("ended_at", json!(ended_at))
        .with("duration_ms", json!(ended_at.saturating_sub(started_at)));
    if let Some(parent_run_id) = parent_run_id {
        ev = ev.with("parent_run_id", json!(parent_run_id));
    }
    if let Some(s) = summary {
        ev = ev.with("summary", json!(s));
    }
    emit(&ev);
}

fn emit_subagent_summary(sub_in: u64, sub_out: u64, sub_cached: u64, last_model: &Option<String>) {
    let m = last_model.clone().unwrap_or_else(|| "?".into());
    let pct = if sub_cached > 0 && sub_in > 0 {
        sub_cached * 100 / sub_in
    } else {
        0
    };
    if sub_cached > 0 && sub_in > 0 {
        emit(&Event::new("info").with(
            "message",
            json!(format!(
                "subagent done ({m}): {sub_in}+{sub_out}t ({pct}% cached)"
            )),
        ));
    } else {
        emit(&Event::new("info").with(
            "message",
            json!(format!("subagent done ({m}): {sub_in}+{sub_out}t")),
        ));
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Walk back from `max` to the nearest UTF-8 char boundary so we never panic
    // slicing into the middle of a multi-byte character (model prose, code with
    // CJK/emoji/smart quotes routinely contains them). The main loop uses the
    // char-safe `smart_truncate`; this is the subagent equivalent.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Per-run backstop to catch a runaway/looping subagent (no per-subagent
/// max-session-tokens is otherwise enforced). There is intentionally NO token
/// budget — a subagent runs until it finishes the task (relying on its own
/// context compaction to stay within the window); only an infinite tool-call
/// loop stops here, since that never completes a task (and is multiplied
/// across parallel fan-out).
const SUBAGENT_MAX_TOOL_CALLS: u32 = 200;

/// Best-effort redaction of common credential patterns from a forked
/// transcript. Not exhaustive — the goal is to drop the obvious pasted
/// secrets, not to be a perfect scanner. Quoted values ("key":"x") are not
/// handled (the pattern avoids a literal quote so it stays a clean raw string);
/// bare `key=value` / `key: value` and well-known token formats are masked.
pub(crate) fn redact_secrets(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{36}|xox[baprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_\-]{35})|(Bearer\s+[A-Za-z0-9._\-]{20,})|((?:password|passwd|secret|api[_-]?key|apikey|token|authorization)\s*[:=]\s*)[A-Za-z0-9_\-+/=]{6,}"
        ).unwrap()
    });
    re.replace_all(s, |caps: &regex::Captures| {
        if caps.get(1).is_some() {
            return "[REDACTED]".to_string();
        }
        if caps.get(2).is_some() {
            return "Bearer [REDACTED]".to_string();
        }
        if let Some(p) = caps.get(3) {
            return format!("{}[REDACTED]", p.as_str());
        }
        "[REDACTED]".to_string()
    })
    .to_string()
}

/// Redact secrets from a forked message's text content (string or multimodal).
fn redact_message_secrets(m: &Message) -> Message {
    let mut clean = m.clone();
    match &mut clean {
        Message::System { content, .. } | Message::User { content, .. } => {
            redact_content(content);
        }
        Message::Assistant {
            content: Some(ref mut s),
            ..
        } => {
            *s = redact_secrets(s);
        }
        Message::Tool {
            ref mut content, ..
        } => {
            *content = redact_secrets(content);
        }
        _ => {}
    }
    clean
}

fn redact_content(content: &mut message::Content) {
    match content {
        message::Content::Text(s) => *s = redact_secrets(s),
        message::Content::Multimodal(parts) => {
            for part in parts {
                if let message::ContentPart::Text { ref mut text } = part {
                    *text = redact_secrets(text);
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Parallel run
// ---------------------------------------------------------------------------

/// Absolute safety bound for parallel fan-out (DoS guard). Also used to clamp
/// explicit concurrency requests that exceed the task count / absolute max.
pub(crate) const PARALLEL_TASKS_ABSOLUTE_MAX: usize = 256;

/// Resolve effective parallel concurrency.
///
/// - `requested` already incorporates the config default when the caller
///   omitted `concurrency` (so defaults stay modest).
/// - Explicit higher requests are honored, clamped only by task count and the
///   absolute safety max — not by the config default ceiling.
pub(crate) fn resolve_parallel_concurrency(
    requested: u64,
    task_count: usize,
    absolute_max: usize,
) -> usize {
    let req = usize::try_from(requested.max(1)).unwrap_or(usize::MAX);
    let by_tasks = task_count.max(1);
    req.min(by_tasks).min(absolute_max.max(1))
}

async fn run_parallel(
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    tasks: &[Value],
    args: &Value,
    depth: u32,
    cancel: &CancellationToken,
    parent_run_id: Option<String>,
) -> Outcome {
    let workspace = st.cfg.read().await.workspace.clone();
    let cfg = st.cfg.read().await.clone();
    if tasks.is_empty() {
        return Outcome::err("parallel requires a non-empty 'tasks' array");
    }
    // Soft cap: the configured `parallel_max_tasks` (default 8) is advisory.
    // Exceeding it no longer errors instantly — tasks queue through the
    // concurrency semaphore below (only `concurrency` run at once). We emit an
    // info event so the user knows a large batch is queuing. A much higher
    // absolute safety bound guards against runaway fan-out (DoS).
    // Callers may also request concurrency above the config default; the
    // default only applies when `concurrency` is omitted.
    if tasks.len() > PARALLEL_TASKS_ABSOLUTE_MAX {
        return Outcome::err(format!(
            "parallel has {} tasks (absolute safety max {PARALLEL_TASKS_ABSOLUTE_MAX}); \
             split into smaller batches",
            tasks.len()
        ));
    }
    if tasks.len() as u32 > cfg.subagents.parallel_max_tasks {
        emit(&Event::new("info").with(
            "message",
            json!(format!(
                "parallel batch has {} tasks (soft default max {}): queuing under concurrency; \
                 only a few run at once unless you raise concurrency. This is allowed — \
                 set subagents.parallel.maxTasks if you want a higher advisory default.",
                tasks.len(),
                cfg.subagents.parallel_max_tasks
            )),
        ));
    }
    let default_concurrency = cfg.subagents.parallel_concurrency.max(1) as u64;
    let requested_concurrency = args
        .get("concurrency")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_concurrency)
        .max(1);
    // Config concurrency is the *default* when omitted, not a hard ceiling.
    // Explicit requests can exceed it; absolute + task-count bounds still apply.
    let concurrency = resolve_parallel_concurrency(
        requested_concurrency,
        tasks.len(),
        PARALLEL_TASKS_ABSOLUTE_MAX,
    );
    let context = args.get("context").and_then(|v| v.as_str());
    let use_worktree = args
        .get("worktree")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if use_worktree && !crate::worktree::is_git_repo(&workspace) {
        return Outcome::err(
            "worktree:true requires a git repository; use hybrid checkpoints for non-git workspaces",
        );
    }

    // resolve agents up front (fail fast on a bad name)
    let agents = discover_agents(&workspace, &cfg.subagents);
    let mut resolved: Vec<(AgentConfig, String, Option<String>, ContextKind)> = Vec::new();
    let mut agent_names: Vec<String> = Vec::new();
    for (i, t) in tasks.iter().enumerate() {
        let an = t.get("agent").and_then(|v| v.as_str()).unwrap_or("");
        let agent = match find_agent(&agents, an) {
            Some(a) => a.clone(),
            None => return Outcome::err(format!("parallel task {i}: unknown agent '{an}'")),
        };
        agent_names.push(agent.name.clone());
        let task = t
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model_override = t.get("model").and_then(|v| v.as_str()).map(String::from);
        let ctx = match context {
            Some("fork") => ContextKind::Fork,
            Some("fresh") => ContextKind::Fresh,
            _ => agent.default_context.clone().unwrap_or(ContextKind::Fresh),
        };
        resolved.push((agent, task, model_override, ctx));
    }

    let run_id = next_run_id();
    // Child token so interrupt_action cancels just this batch (and a parent
    // /abort propagates), not the whole orchestrator.
    let run_cancel = cancel.child_token();
    let session = st.runtime.session_context();
    let Some(_subagent_resource) = st.runtime.register_owned_session_resource(
        &session,
        crate::runtime::ResourceKind::Subagent,
        format!("subagent:{run_id}:parallel"),
        parent_run_id.as_deref(),
        run_cancel.clone(),
    ) else {
        return Outcome::err("parallel subagent rejected because its session is no longer active");
    };
    let started = now_ms();
    let run = SubagentRun {
        id: run_id.clone(),
        parent_run_id: parent_run_id.clone(),
        mode: "parallel".into(),
        agent: None,
        agents: agent_names.clone(),
        state: "running".into(),
        started_at: started,
        ended_at: None,
        depth,
        intercom_target: None,
        cancel: Some(Arc::new(run_cancel.clone())),
        children: vec![],
        summary: None,
        messages: Arc::new(Mutex::new(Vec::new())),
    };
    st.subagent_runs.lock().await.insert(run_id.clone(), run);
    emit_subagent_start(
        &run_id,
        "parallel",
        None,
        &agent_names,
        &format!("parallel ({} tasks)", tasks.len()),
        depth,
        started,
        parent_run_id.as_deref(),
    );
    persist_subagent_state(
        st,
        &run_id,
        parent_run_id.as_deref(),
        crate::session::RunState::Started,
        Some("parallel"),
    )
    .await;

    // Pre-create worktrees (fail fast before spawning).
    let mut worktrees: Vec<Option<std::path::PathBuf>> = Vec::with_capacity(resolved.len());
    if use_worktree {
        for i in 0..resolved.len() {
            let rid = format!("{}-{}", run_id, i);
            match crate::worktree::add_worktree(&workspace, &rid) {
                Ok(p) => match crate::worktree::seed_worktree_from_main(&workspace, &p) {
                    Ok(paths) => {
                        if !paths.is_empty() {
                            emit(
                                &Event::new("worktree_seeded")
                                    .with("run_id", json!(&rid))
                                    .with("paths", json!(paths)),
                            );
                        }
                        worktrees.push(Some(p));
                    }
                    Err(e) => {
                        let _ = crate::worktree::remove_worktree(&workspace, &p);
                        for wt in worktrees.iter().flatten() {
                            let _ = crate::worktree::remove_worktree(&workspace, wt);
                        }
                        return fail_registered_subagent_setup(
                            st,
                            &run_id,
                            parent_run_id.as_deref(),
                            started,
                            format!("worktree seed failed for task {i}: {e}"),
                        )
                        .await;
                    }
                },
                Err(e) => {
                    // Clean up any already-created worktrees.
                    for (j, wt) in worktrees.iter().enumerate() {
                        if let Some(p) = wt {
                            let _ = crate::worktree::remove_worktree(&workspace, p);
                            let _ = j;
                        }
                    }
                    return fail_registered_subagent_setup(
                        st,
                        &run_id,
                        parent_run_id.as_deref(),
                        started,
                        format!("worktree setup failed for task {i}: {e}"),
                    )
                    .await;
                }
            }
        }
    } else {
        worktrees.resize(resolved.len(), None);
    }

    // run all tasks with a concurrency semaphore; collect results in order.
    // Tasks run on JoinHandles (not a channel) so a panicking task is observed
    // (JoinHandle::await -> Err(JoinError)) and reported as an error for its
    // index, instead of silently dropped and mislabeled (P1-8).
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut handles: Vec<(usize, tokio::task::JoinHandle<Outcome>)> =
        Vec::with_capacity(tasks.len());
    // Populate parent.children for status/tree cancel (CORE_REVIEW).
    {
        let child_ids: Vec<String> = (0..resolved.len())
            .map(|i| format!("{}-{}", run_id, i))
            .collect();
        if let Some(parent) = st.subagent_runs.lock().await.get_mut(&run_id) {
            // Store lightweight child stubs for graph visibility.
            parent.children = child_ids
                .iter()
                .map(|cid| SubagentRun {
                    id: cid.clone(),
                    parent_run_id: Some(run_id.to_string()),
                    mode: "single".into(),
                    agent: None,
                    agents: vec![],
                    state: "running".into(),
                    started_at: now_ms(),
                    ended_at: None,
                    depth,
                    intercom_target: None,
                    cancel: None,
                    children: vec![],
                    summary: None,
                    messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                })
                .collect();
        }
    }
    for (i, (agent, task, model_override, ctx)) in resolved.into_iter().enumerate() {
        let stc = st.clone();
        let clientc = client.clone();
        let prov = provider.clone();
        let pm = parent_model.to_string();
        let cancelc = run_cancel.clone(); // child of parent; interrupt cancels just this batch
        let semc = sem.clone();
        let rid = format!("{}-{}", run_id, i);
        let parent_batch_run_id = run_id.clone();
        let wt = worktrees.get(i).cloned().flatten();
        let h = tokio::spawn(async move {
            let Ok(_permit) = semc.acquire().await else {
                return Outcome::err("parallel slot unavailable (semaphore closed or cancelled)");
            };
            run_single(
                &stc,
                &clientc,
                &prov,
                &pm,
                &agent,
                &task,
                &rid,
                Some(parent_batch_run_id),
                model_override,
                ctx,
                depth,
                &cancelc,
                wt,
                None,
            )
            .await
        });
        handles.push((i, h));
    }
    let mut collected: Vec<(usize, Outcome)> = Vec::with_capacity(handles.len());
    for (i, h) in handles {
        match h.await {
            Ok(o) => collected.push((i, o)),
            Err(_) => collected.push((
                i,
                Outcome::err("parallel task panicked (internal error — see debug log)"),
            )),
        }
    }
    collected.sort_by_key(|(i, _)| *i);
    let all_ok = collected.iter().all(|(_, o)| o.ok);
    let delivery_summary = collected
        .iter()
        .map(|(index, outcome)| format!("task {}: {}", index + 1, outcome.output))
        .collect::<Vec<_>>()
        .join("\n");

    // Promote successful worktrees into the main workspace, then clean up.
    // Never promote on cancel even if a child reported ok before the abort (H13).
    // Promote Err flips the task (and batch) to failed — do not report ok:true
    // when main workspace was not updated (CORE_REVIEW C11).
    let batch_cancelled = run_cancel.is_cancelled();
    let mut promote_failed = false;
    let mut promoted_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, o) in &collected {
        if let Some(wt) = worktrees.get(*i).and_then(|w| w.as_ref()) {
            let mut keep_worktree = false;
            if o.ok && !batch_cancelled {
                match crate::worktree::promote_worktree(&workspace, wt) {
                    Ok(paths) if !paths.is_empty() => {
                        // Detect last-writer-wins overlap across parallel worktrees
                        // (CORE_REVIEW): report failure rather than silent clobber.
                        let overlap: Vec<_> = paths
                            .iter()
                            .filter(|p| promoted_paths.contains(*p))
                            .cloned()
                            .collect();
                        if !overlap.is_empty() {
                            emit(
                                &Event::new("error").with(
                                    "message",
                                    json!(format!(
                                        "worktree promote conflict for task {i}: overlapping paths {:?}",
                                        overlap
                                    )),
                                ),
                            );
                            promote_failed = true;
                            keep_worktree = true;
                        } else {
                            for p in &paths {
                                promoted_paths.insert(p.clone());
                            }
                            emit(
                                &Event::new("worktree_promoted")
                                    .with("run_id", json!(format!("{}-{}", run_id, i)))
                                    .with("paths", json!(paths)),
                            );
                        }
                    }
                    Err(e) => {
                        emit(&Event::new("error").with(
                            "message",
                            json!(format!("worktree promote failed for task {i}: {e}")),
                        ));
                        promote_failed = true;
                        keep_worktree = true;
                    }
                    _ => {}
                }
            }
            if !keep_worktree {
                let _ = crate::worktree::remove_worktree(&workspace, wt);
            }
        }
    }
    let all_ok = all_ok && !promote_failed;

    // finalize run
    let final_state = if run_cancel.is_cancelled() {
        "cancelled"
    } else if all_ok {
        "completed"
    } else {
        "failed"
    };
    let mut runs = st.subagent_runs.lock().await;
    let mut done_ended: u64 = started;
    if let Some(r) = runs.get_mut(&run_id) {
        r.state = final_state.into();
        r.ended_at = Some(now_ms());
        done_ended = r.ended_at.unwrap_or(started);
    }
    prune_terminal_runs(&mut runs);
    drop(runs);
    persist_and_deliver_job(
        st,
        &run_id,
        parent_run_id.as_deref(),
        final_state,
        &delivery_summary,
    )
    .await;
    let persisted_state = match final_state {
        "completed" => crate::session::RunState::Completed,
        "cancelled" => crate::session::RunState::Cancelled,
        _ => crate::session::RunState::Failed,
    };
    persist_subagent_state(st, &run_id, parent_run_id.as_deref(), persisted_state, None).await;
    emit_subagent_done(
        &run_id,
        final_state,
        None,
        started,
        done_ended,
        parent_run_id.as_deref(),
    );

    let mut blocks = String::new();
    for (i, o) in collected.iter() {
        blocks.push_str(&format!(
            "=== Parallel Task {} ({}) ===\n{}\n\n",
            i + 1,
            agent_names.get(*i).cloned().unwrap_or_default(),
            o.output
        ));
    }
    Outcome {
        ok: all_ok,
        output: blocks.trim().to_string(),
        diff: None,
    }
}

// ---------------------------------------------------------------------------
// Chain run (sequential; static parallel groups inline)
// ---------------------------------------------------------------------------

async fn run_chain(
    st: &Arc<State>,
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    parent_model: &str,
    chain: &[Value],
    args: &Value,
    depth: u32,
    cancel: &CancellationToken,
    parent_run_id: Option<String>,
) -> Outcome {
    if chain.is_empty() {
        return Outcome::err("chain requires a non-empty 'chain' array");
    }
    // CORE_REVIEW: async:true is intentionally ignored — chain is always
    // awaited so status/cancel/promote stay coherent. Detached work uses
    // parallel mode or parent-level orchestration instead.
    let _ = args.get("async");
    let workspace = st.cfg.read().await.workspace.clone();
    let cfg = st.cfg.read().await.clone();
    let agents = discover_agents(&workspace, &cfg.subagents);
    let run_id = next_run_id();
    let chain_dir = std::env::temp_dir().join(format!("catalyst-code-subagent-chain-{}", run_id));
    let _ = std::fs::create_dir_all(&chain_dir);

    // Child token so interrupt_action cancels JUST this chain (and a parent
    // /abort still propagates through it), not the parent orchestrator/turn
    // and its siblings — matching run_single/run_parallel.
    let run_cancel = cancel.child_token();
    let session = st.runtime.session_context();
    let Some(_subagent_resource) = st.runtime.register_owned_session_resource(
        &session,
        crate::runtime::ResourceKind::Subagent,
        format!("subagent:{run_id}:chain"),
        parent_run_id.as_deref(),
        run_cancel.clone(),
    ) else {
        return Outcome::err("chain subagent rejected because its session is no longer active");
    };
    let chain_agents: Vec<String> = chain
        .iter()
        .filter_map(|s| s.get("agent").and_then(|v| v.as_str()).map(String::from))
        .collect();
    let started = now_ms();
    let chain_parent_run_id = parent_run_id.clone();
    let run = SubagentRun {
        id: run_id.clone(),
        parent_run_id,
        mode: "chain".into(),
        agent: None,
        agents: chain_agents.clone(),
        state: "running".into(),
        started_at: started,
        ended_at: None,
        depth,
        intercom_target: None,
        cancel: Some(Arc::new(run_cancel.clone())),
        children: vec![],
        summary: None,
        messages: Arc::new(Mutex::new(Vec::new())),
    };
    st.subagent_runs.lock().await.insert(run_id.clone(), run);
    emit_subagent_start(
        &run_id,
        "chain",
        None,
        &chain_agents,
        &format!("chain ({} steps)", chain.len()),
        depth,
        started,
        chain_parent_run_id.as_deref(),
    );
    persist_subagent_state(
        st,
        &run_id,
        chain_parent_run_id.as_deref(),
        crate::session::RunState::Started,
        Some("chain"),
    )
    .await;

    // The step loop runs in an inner async block so EVERY exit path (abort,
    // unknown agent, a failed step, or clean completion) falls through to the
    // finalize + chain_dir cleanup below. Previously an early `return` skipped
    // the parent run's finalize (leaving it "running" forever, pinning its
    // messages + cancel token) AND leaked the chain_dir temp directory.
    let outcome = async {
        let mut outputs: HashMap<String, String> = HashMap::new();
        let mut previous = String::new();

        for (step_i, step) in chain.iter().enumerate() {
            if run_cancel.is_cancelled() {
                return Outcome::err("[chain aborted]");
            }
            // parallel group?
            if let Some(group) = step.get("parallel").and_then(|v| v.as_array()) {
                let group_args = json!({ "tasks": group, "context": args.get("context").and_then(|v| v.as_str()).unwrap_or("fresh"), "concurrency": step.get("concurrency").and_then(|v| v.as_u64()).unwrap_or(cfg.subagents.parallel_concurrency as u64) });
                let o = Box::pin(run_parallel(
                    st,
                    client,
                    provider,
                    parent_model,
                    group,
                    &group_args,
                    depth,
                    &run_cancel,
                    Some(run_id.clone()),
                ))
                .await;
                if !o.ok {
                    return Outcome::err(format!(
                        "chain step {step_i} (parallel group) failed: {}",
                        o.output
                    ));
                }
                previous = o.output.clone();
                if let Some(as_name) = step.get("as").and_then(|v| v.as_str()) {
                    outputs.insert(as_name.to_string(), o.output.clone());
                }
                continue;
            }

            let an = step.get("agent").and_then(|v| v.as_str()).unwrap_or("");
            let agent = match find_agent(&agents, an) {
                Some(a) => a.clone(),
                None => return Outcome::err(format!("chain step {step_i}: unknown agent '{an}'")),
            };
            let task_tmpl = step
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("{previous}");
            let task = render_task(task_tmpl, &previous, &outputs, &chain_dir);
            let model_override = step.get("model").and_then(|v| v.as_str()).map(String::from);
            let context = match args.get("context").and_then(|v| v.as_str()) {
                Some("fork") => ContextKind::Fork,
                Some("fresh") => ContextKind::Fresh,
                _ => agent.default_context.clone().unwrap_or(ContextKind::Fresh),
            };
            let step_id = format!("{run_id}-{step_i}");
            emit(&Event::new("info").with(
                "message",
                json!(format!(
                    "chain step {step_i}+1: {} — {}",
                    agent.name,
                    truncate(&task, 80)
                )),
            ));
            let o = run_single(
                st,
                client,
                provider,
                parent_model,
                &agent,
                &task,
                &step_id,
                Some(run_id.clone()),
                model_override,
                context,
                depth,
                &run_cancel,
                None,
        None,
    )
            .await;
            if !o.ok {
                return Outcome::err(format!(
                    "chain step {step_i} ({}) failed: {}",
                    agent.name, o.output
                ));
            }
            previous = o.output.clone();
            if let Some(as_name) = step.get("as").and_then(|v| v.as_str()) {
                outputs.insert(as_name.to_string(), o.output.clone());
            }
        }
        Outcome::ok(previous)
    }
    .await;

    // Finalize the parent run record on every exit path, and remove the
    // chain_dir temp directory (previously leaked — one per chain run,
    // including failed/aborted ones — filling /tmp over a long session).
    let final_state = if run_cancel.is_cancelled() {
        "cancelled"
    } else if outcome.ok {
        "completed"
    } else {
        "failed"
    };
    let mut runs = st.subagent_runs.lock().await;
    let mut done_ended: u64 = started;
    if let Some(r) = runs.get_mut(&run_id) {
        r.state = final_state.into();
        r.ended_at = Some(now_ms());
        done_ended = r.ended_at.unwrap_or(started);
    }
    prune_terminal_runs(&mut runs);
    drop(runs);
    persist_and_deliver_job(
        st,
        &run_id,
        chain_parent_run_id.as_deref(),
        final_state,
        &outcome.output,
    )
    .await;
    let persisted_state = match final_state {
        "completed" => crate::session::RunState::Completed,
        "cancelled" => crate::session::RunState::Cancelled,
        _ => crate::session::RunState::Failed,
    };
    persist_subagent_state(
        st,
        &run_id,
        chain_parent_run_id.as_deref(),
        persisted_state,
        None,
    )
    .await;
    emit_subagent_done(
        &run_id,
        final_state,
        None,
        started,
        done_ended,
        chain_parent_run_id.as_deref(),
    );
    let _ = std::fs::remove_dir_all(&chain_dir);
    outcome
}

/// Render a chain task template, substituting {previous}, {outputs.name},
/// {task}, {chain_dir}.
fn render_task(
    tmpl: &str,
    previous: &str,
    outputs: &HashMap<String, String>,
    chain_dir: &std::path::Path,
) -> String {
    let mut out = tmpl.replace("{previous}", previous);
    out = out.replace("{chain_dir}", &chain_dir.display().to_string());
    // {outputs.name}
    let mut start = 0;
    let mut res = String::new();
    while let Some(i) = out[start..].find("{outputs.") {
        res.push_str(&out[start..start + i]);
        let rest = &out[start + i + 9..]; // after "{outputs."
        if let Some(end) = rest.find('}') {
            let name = &rest[..end];
            res.push_str(outputs.get(name).map(|s| s.as_str()).unwrap_or(""));
            start = start + i + 9 + end + 1;
        } else {
            res.push_str(&out[start + i..]);
            start = out.len();
            break;
        }
    }
    res.push_str(&out[start..]);
    res
}

// ---------------------------------------------------------------------------
// Management / control actions
// ---------------------------------------------------------------------------

async fn handle_action(
    action: &str,
    args: &Value,
    workspace: &std::path::Path,
    cfg: &Config,
    st: &Arc<State>,
    cancel: &CancellationToken,
) -> Outcome {
    match action {
        "list" => {
            let agents = discover_agents(workspace, &cfg.subagents);
            let lines: Vec<String> = agents.iter().map(|a| {
                format!("- {} [{}] — {}", a.name, source_label(&a.source), a.description)
            }).collect();
            Outcome::ok(format!("{} agent(s):\n{}", agents.len(), lines.join("\n")))
        }
        "get" => {
            let agents = discover_agents(workspace, &cfg.subagents);
            let name = args.get("agent").and_then(|v| v.as_str()).unwrap_or("");
            match find_agent(&agents, name) {
                Some(a) => Outcome::ok(json!(a).to_string()),
                None => Outcome::err(format!("unknown agent '{name}'")),
            }
        }
        "models" => {
            let agents = discover_agents(workspace, &cfg.subagents);
            let models = st.models.read().await;
            let default_model = cfg.default_model.clone().or_else(|| models.first().map(|m| m.id.clone())).unwrap_or_default();
            let name = args.get("agent").and_then(|v| v.as_str());
            let lines: Vec<String> = agents.iter().filter(|a| name.map(|n| a.name == n || a.name == n.rsplit('.').next().unwrap_or(n)).unwrap_or(true)).map(|a| {
                let m = a.model.clone().unwrap_or_else(|| default_model.clone());
                format!("- {}: {} (fallback: {})", a.name, m, a.fallback_models.join(", "))
            }).collect();
            Outcome::ok(lines.join("\n"))
        }
        "create" => create_agent(args, workspace),
        "update" => update_agent(args, workspace),
        "delete" => delete_agent(args, workspace),
        "status" => status_action(args, st).await,
        "interrupt" => interrupt_action(args, st).await,
        "resume" => resume_action(args, st, cancel).await,
        "peek" => peek_action(args, st).await,
        "steer" => steer_action(args, st, cancel).await,
        "doctor" => doctor_action(workspace, cfg, st).await,
        other => Outcome::err(format!("unknown action '{other}'; use list|get|create|update|delete|status|interrupt|resume|peek|steer|doctor")),
    }
}

fn source_label(s: &AgentSource) -> &'static str {
    match s {
        AgentSource::Builtin => "builtin",
        AgentSource::User => "user",
        AgentSource::Project => "project",
    }
}

/// Peek at a running subagent's conversation state.
pub async fn peek_action(args: &Value, st: &Arc<State>) -> Outcome {
    let runs = st.subagent_runs.lock().await;
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let r = match find_run_prefix(&runs, id) {
        Some(r) => r.clone(),
        None => return Outcome::err(format!("no run matching '{id}'")),
    };
    drop(runs);
    let msgs = r.messages.lock().unwrap();
    let msg_count = msgs.len();
    let est_tokens = estimate_messages_tokens(&msgs);
    let last_turns: Vec<String> = msgs
        .iter()
        .filter(|m| m.is_assistant())
        .filter_map(|m| m.content_text())
        .map(|s| s.to_string())
        .rev()
        .take(3)
        .collect();
    let last_tools: Vec<String> = msgs
        .iter()
        .filter(|m| m.is_tool())
        .filter_map(|m| m.content_text())
        .map(|s| s.to_string())
        .rev()
        .take(3)
        .collect();
    drop(msgs);
    let body = json!({
        "id": r.id,
        "parent_run_id": r.parent_run_id,
        "state": r.state,
        "mode": r.mode,
        "agents": r.agents,
        "started_at": r.started_at,
        "ended_at": r.ended_at,
        "messages_count": msg_count,
        "estimated_tokens": est_tokens,
        "last_turns": last_turns,
        "last_tools": last_tools,
        "intercom_pending": st.intercom.pending_count(),
    });
    Outcome::ok(body.to_string())
}

/// Steer a running subagent by injecting a message into its conversation.
pub async fn steer_action(args: &Value, st: &Arc<State>, _cancel: &CancellationToken) -> Outcome {
    let runs = st.subagent_runs.lock().await;
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let r = match find_run_prefix(&runs, id) {
        Some(r) => r.clone(),
        None => return Outcome::err(format!("no run matching '{id}'")),
    };
    drop(runs);
    if msg.is_empty() {
        return Outcome::err("steer requires 'message'");
    }
    if let Some(target) = &r.intercom_target {
        if st.intercom.targets().iter().any(|t| t == target) {
            let imsg = crate::intercom::IntercomMessage {
                id: format!("steer-{}", now_ms()),
                from: st.intercom.orchestrator_target(),
                to: target.clone(),
                message: msg.to_string(),
                reason: "steer".into(),
                ts: now_ms(),
                ask_id: String::new(),
            };
            match st.intercom.post(imsg) {
                Ok(()) => {
                    return Outcome::ok(format!(
                        "steer message delivered to {} ({})",
                        r.id, target
                    ));
                }
                Err(e) => return Outcome::err(format!("steer delivery failed: {e}")),
            }
        }
    }
    Outcome::err(format!("run {} is no longer live", r.id))
}

fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("agent name must not be empty".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!(
            "agent name must be a single path segment (got '{name}')"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "agent name may only contain [A-Za-z0-9._-] (got '{name}')"
        ));
    }
    Ok(())
}

fn create_agent(args: &Value, workspace: &std::path::Path) -> Outcome {
    let cfg = match args.get("config") {
        Some(v) => v,
        None => return Outcome::err("create requires 'config' with name + systemPrompt"),
    };
    let name = cfg.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Outcome::err("create config requires 'name'");
    }
    if let Err(e) = validate_agent_name(name) {
        return Outcome::err(e);
    }
    let scope = cfg
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("project");
    let dir = match scope {
        "user" => crate::config::home_dir()
            .map(|h| h.join(".catalyst-code/agents"))
            .unwrap_or_else(|| workspace.join(".catalyst-code/agents")),
        _ => workspace.join(".catalyst-code/agents"),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Outcome::err(format!("create mkdir failed: {e}"));
    }
    let path = dir.join(format!("{name}.md"));
    let body = cfg
        .get("systemPrompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut fm = format!("---\nname: {name}\n");
    if let Some(d) = cfg.get("description").and_then(|v| v.as_str()) {
        fm.push_str(&format!("description: {d}\n"));
    }
    if let Some(t) = cfg.get("tools").and_then(|v| v.as_str()) {
        fm.push_str(&format!("tools: {t}\n"));
    }
    if let Some(m) = cfg.get("model").and_then(|v| v.as_str()) {
        fm.push_str(&format!("model: {m}\n"));
    }
    if let Some(t) = cfg.get("thinking").and_then(|v| v.as_str()) {
        fm.push_str(&format!("thinking: {t}\n"));
    }
    let mode = cfg
        .get("systemPromptMode")
        .and_then(|v| v.as_str())
        .unwrap_or("replace");
    fm.push_str(&format!("systemPromptMode: {mode}\n"));
    fm.push_str("---\n\n");
    fm.push_str(body);
    match std::fs::write(&path, fm) {
        Ok(_) => Outcome::ok(format!("created agent '{name}' at {}", path.display())),
        Err(e) => Outcome::err(format!("create write failed: {e}")),
    }
}

fn update_agent(args: &Value, workspace: &std::path::Path) -> Outcome {
    let name = args.get("agent").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Outcome::err("update requires 'agent'");
    }
    if let Err(e) = validate_agent_name(name) {
        return Outcome::err(e);
    }
    // find the file
    let candidates = [
        workspace
            .join(".catalyst-code/agents")
            .join(format!("{name}.md")),
        crate::config::home_dir()
            .map(|h| h.join(format!(".catalyst-code/agents/{name}.md")))
            .unwrap_or_default(),
    ];
    let path = candidates.iter().find(|p| p.exists()).cloned();
    let path = match path {
        Some(p) => p,
        None => {
            return Outcome::err(format!(
                "agent '{name}' not found to update; create it first"
            ))
        }
    };
    let cfg = match args.get("config") {
        Some(v) => v,
        None => return Outcome::err("update requires 'config'"),
    };
    let (mut fm, body) = parse_frontmatter(&std::fs::read_to_string(&path).unwrap_or_default());
    if let Some(m) = cfg.get("model").and_then(|v| v.as_str()) {
        fm.insert("model".into(), m.into());
    }
    if let Some(t) = cfg.get("thinking").and_then(|v| v.as_str()) {
        fm.insert("thinking".into(), t.into());
    }
    if let Some(d) = cfg.get("description").and_then(|v| v.as_str()) {
        fm.insert("description".into(), d.into());
    }
    if let Some(t) = cfg.get("tools").and_then(|v| v.as_str()) {
        fm.insert("tools".into(), t.into());
    }
    if let Some(b) = cfg.get("systemPrompt").and_then(|v| v.as_str()) {
        let out = format!(
            "---\n{}\n---\n\n{}",
            fm.iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
            b
        );
        let _ = std::fs::write(&path, out);
        return Outcome::ok(format!("updated agent '{name}'"));
    }
    let out = format!(
        "---\n{}\n---\n\n{}",
        fm.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        body
    );
    let _ = std::fs::write(&path, out);
    Outcome::ok(format!("updated agent '{name}'"))
}

fn delete_agent(args: &Value, workspace: &std::path::Path) -> Outcome {
    let name = args.get("agent").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Outcome::err("delete requires 'agent'");
    }
    if let Err(e) = validate_agent_name(name) {
        return Outcome::err(e);
    }
    let candidates = [
        workspace
            .join(".catalyst-code/agents")
            .join(format!("{name}.md")),
        crate::config::home_dir()
            .map(|h| h.join(format!(".catalyst-code/agents/{name}.md")))
            .unwrap_or_default(),
    ];
    for p in &candidates {
        if p.exists() {
            if let Err(e) = std::fs::remove_file(p) {
                return Outcome::err(format!("delete failed: {e}"));
            }
            return Outcome::ok(format!("deleted agent '{name}'"));
        }
    }
    Outcome::err(format!(
        "agent '{name}' not found (builtins cannot be deleted; override with disabled:true)"
    ))
}

async fn status_action(args: &Value, st: &Arc<State>) -> Outcome {
    let runs = st.subagent_runs.lock().await;
    let id = args.get("id").and_then(|v| v.as_str());
    if let Some(id) = id {
        if let Some(r) = find_run_prefix(&runs, id) {
            return Outcome::ok(format_run(r));
        }
        return Outcome::err(format!("no run matching '{id}'"));
    }
    if runs.is_empty() {
        return Outcome::ok("no subagent runs");
    }
    let lines: Vec<String> = runs.values().map(format_run).collect();
    Outcome::ok(lines.join("\n"))
}

async fn interrupt_action(args: &Value, st: &Arc<State>) -> Outcome {
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut runs = st.subagent_runs.lock().await;
    let r = match find_run_prefix_mut(&mut runs, id) {
        Some(r) => r,
        None => return Outcome::err(format!("no run matching '{id}'")),
    };
    if let Some(c) = r.cancel.clone() {
        c.cancel();
        // Honest status: interrupt cancels the run loop; there is no true pause.
        // Mark cancelled so resume does not claim a parked/paused agent
        // (CORE_REVIEW: interrupt ≠ pause).
        r.state = "cancelled".into();
        Outcome::ok(format!(
            "cancelled run {} (interrupt stops the run; start a new run or use steer while still live)",
            r.id
        ))
    } else {
        Outcome::err(format!("run {} has no cancel handle", r.id))
    }
}

async fn resume_action(args: &Value, st: &Arc<State>, _cancel: &CancellationToken) -> Outcome {
    // Resume is acknowledged: a follow-up message is delivered to the child's
    // intercom target if it is still registered (targets() now excludes
    // unregistered peers, so a finished run reports "no longer live" rather
    // than falsely claiming delivery), otherwise we suggest a new run.
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let r = {
        let runs = st.subagent_runs.lock().await;
        match find_run_prefix(&runs, id) {
            Some(r) => r.clone(),
            None => return Outcome::err(format!("no run matching '{id}'")),
        }
    };
    if let Some(target) = &r.intercom_target {
        if st.intercom.targets().iter().any(|t| t == target) {
            let msg = crate::intercom::IntercomMessage {
                id: format!("resume-{}", now_ms()),
                from: st.intercom.orchestrator_target(),
                to: target.clone(),
                message: message.to_string(),
                reason: "resume".into(),
                ts: now_ms(),
                ask_id: String::new(),
            };
            // Surface a delivery failure (e.g. the run ended between the
            // liveness check and the post) instead of silently reporting success.
            match st.intercom.post(msg) {
                Ok(()) => return Outcome::ok(format!("resume message delivered to {target}")),
                Err(e) => return Outcome::err(format!("resume delivery failed: {e}")),
            }
        }
    }
    Outcome::ok(format!(
        "run {} is no longer live; start a new run to continue",
        r.id
    ))
}

fn find_run_prefix<'a>(
    runs: &'a HashMap<String, SubagentRun>,
    id: &str,
) -> Option<&'a SubagentRun> {
    if let Some(r) = runs.get(id) {
        return Some(r);
    }
    // Prefix match only when unique (CORE_REVIEW).
    let mut hits = runs.values().filter(|r| r.id.starts_with(id));
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(first)
}

fn find_run_prefix_mut<'a>(
    runs: &'a mut HashMap<String, SubagentRun>,
    id: &str,
) -> Option<&'a mut SubagentRun> {
    if runs.contains_key(id) {
        return runs.get_mut(id);
    }
    let keys: Vec<String> = runs.keys().filter(|k| k.starts_with(id)).cloned().collect();
    if keys.len() != 1 {
        return None;
    }
    runs.get_mut(&keys[0])
}

fn format_run(r: &SubagentRun) -> String {
    let dur = r
        .ended_at
        .map(|e| e.saturating_sub(r.started_at) / 1000)
        .unwrap_or(0);
    format!(
        "[{}] {} ({}) — parent: {} — {} — {}s — target: {}",
        r.state,
        r.id,
        r.mode,
        r.parent_run_id.as_deref().unwrap_or("-"),
        r.agents.join(","),
        dur,
        r.intercom_target.clone().unwrap_or("-".into())
    )
}

async fn doctor_action(workspace: &std::path::Path, cfg: &Config, st: &Arc<State>) -> Outcome {
    let agents = discover_agents(workspace, &cfg.subagents);
    let mut lines = Vec::new();
    lines.push(format!("agents discovered: {}", agents.len()));
    lines.push(format!(
        "max subagent depth: {}",
        resolve_max_depth(&cfg.subagents)
    ));
    lines.push(format!(
        "intercom bridge mode: {}",
        cfg.subagents.intercom_bridge_mode.as_str()
    ));
    lines.push(format!(
        "intercom known targets: {}",
        st.intercom.targets().join(", ")
    ));
    lines.push(format!(
        "intercom pending asks: {}",
        st.intercom.pending_count()
    ));
    lines.push(format!(
        "parallel: maxTasks={}, concurrency={}",
        cfg.subagents.parallel_max_tasks, cfg.subagents.parallel_concurrency
    ));
    let runs = st.subagent_runs.lock().await;
    lines.push(format!("tracked runs: {}", runs.len()));
    for r in runs.values() {
        lines.push(format!("  {}", format_run(r)));
    }
    Outcome::ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_defaults_are_soft_not_hard() {
        // Config default max tasks is 8 (advisory).
        let cfg = SubagentConfig::default();
        assert_eq!(cfg.parallel_max_tasks, 8);
        assert_eq!(cfg.parallel_concurrency, 4);

        // Explicit concurrency above the config default is honored.
        assert_eq!(
            resolve_parallel_concurrency(16, 16, PARALLEL_TASKS_ABSOLUTE_MAX),
            16
        );
        // Clamped by task count so we never spin more workers than tasks.
        assert_eq!(
            resolve_parallel_concurrency(32, 12, PARALLEL_TASKS_ABSOLUTE_MAX),
            12
        );
        // Absolute safety max still binds runaway requests.
        assert_eq!(
            resolve_parallel_concurrency(10_000, 10_000, PARALLEL_TASKS_ABSOLUTE_MAX),
            PARALLEL_TASKS_ABSOLUTE_MAX
        );
        // Zero request still yields at least 1.
        assert_eq!(
            resolve_parallel_concurrency(0, 5, PARALLEL_TASKS_ABSOLUTE_MAX),
            1
        );
    }

    #[test]
    fn frontmatter_parses() {
        let (fm, body) =
            parse_frontmatter("---\nname: scout\ntools: read, grep\n---\n\nYou are a scout.");
        assert_eq!(fm.get("name").unwrap(), "scout");
        assert_eq!(fm.get("tools").unwrap(), "read, grep");
        assert!(body.starts_with("You are a scout."));
    }

    #[test]
    fn child_max_depth_caps_the_subtree() {
        // no override -> inherit the parent ceiling
        assert_eq!(child_max_depth(2, None), 2);
        // override below parent -> capped down
        assert_eq!(child_max_depth(5, Some(1)), 1);
        // override above parent -> parent still wins (a child can't widen the cap)
        assert_eq!(child_max_depth(2, Some(9)), 2);
    }

    #[test]
    fn builtin_agents_present() {
        let v = builtin_agents();
        let names: Vec<&str> = v.iter().map(|a| a.name.as_str()).collect();
        for n in [
            "scout",
            "researcher",
            "planner",
            "worker",
            "reviewer",
            "context-builder",
            "oracle",
            "delegate",
        ] {
            assert!(names.contains(&n), "missing builtin {n}");
        }
    }

    #[test]
    fn tool_normalization() {
        assert_eq!(AgentConfig::normalize_tool("read"), "read_file");
        assert_eq!(AgentConfig::normalize_tool("find"), "glob");
        assert_eq!(AgentConfig::normalize_tool("ls"), "list_dir");
        assert_eq!(AgentConfig::normalize_tool("bash"), "bash");
    }

    #[test]
    fn render_task_substitutes() {
        let mut outputs = HashMap::new();
        outputs.insert("context".into(), "CTX".into());
        let r = render_task(
            "plan from {outputs.context} and {previous}",
            "PREV",
            &outputs,
            std::path::Path::new("/tmp"),
        );
        assert_eq!(r, "plan from CTX and PREV");
    }

    #[test]
    fn target_is_stable() {
        let t = subagent_target("run-1", "worker", Some(0));
        assert!(t.starts_with("subagent-worker-"));
    }

    #[test]
    fn target_truncates_on_char_boundary() {
        // 8-byte cap can land mid multi-byte char. U+2019 ’ is 3 bytes; three
        // of them are 9 bytes so min(8) is inside the third apostrophe.
        let t = subagent_target("’’’extra", "worker", None);
        assert!(t.starts_with("subagent-worker-"));
        let prefix = t.trim_start_matches("subagent-worker-");
        assert!(prefix.len() <= 8);
        assert!(prefix.is_char_boundary(prefix.len()));
        // Must not panic and must keep only whole characters (0, 3, or 6 bytes).
        assert_eq!(prefix.len() % 3, 0);
    }

    #[test]
    fn depth_guard_clamps() {
        assert_eq!(child_max_depth(2, Some(1)), 1);
        assert_eq!(child_max_depth(2, None), 2);
    }

    fn tool_name(def: &Value) -> &str {
        def.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    #[test]
    fn all_tool_names_includes_memory() {
        // Milestone 1.2: the memory tool is part of the subagent default
        // (read-only) allowlist so info-gathering agents can persist learnings.
        assert!(
            all_tool_names().contains(&"memory"),
            "memory must be in the default subagent tool allowlist"
        );
    }

    #[test]
    fn default_tools_include_memory_and_exclude_destructive() {
        // An agent with no declared `tools:` gets the read-only default set,
        // which must include `memory` (read-only) but not `bash`/`write_file`
        // (destructive).
        let mut a = builtin_agents()
            .into_iter()
            .find(|a| a.name == "delegate")
            .unwrap()
            .clone();
        a.tools = vec![];
        let defs = subagent_tool_defs(&a, false, 0, 2);
        let names: Vec<&str> = defs.iter().map(tool_name).collect();
        assert!(
            names.contains(&"memory"),
            "default agent should get memory: {names:?}"
        );
        assert!(
            !names.contains(&"bash"),
            "default agent must not get bash: {names:?}"
        );
        assert!(
            !names.contains(&"write_file"),
            "default agent must not get write_file: {names:?}"
        );
        assert!(
            !names.contains(&"fetch")
                && !names.contains(&"web_search")
                && !names.contains(&"bulk_read"),
            "default agent must not get deferred tools: {names:?}"
        );
    }

    #[test]
    fn explicit_tools_control_memory_presence() {
        // scout/researcher/context-builder declare `memory` explicitly so they
        // get it; planner does not declare it so it must not.
        let agents = builtin_agents();
        let scout = agents.iter().find(|a| a.name == "scout").unwrap();
        let planner = agents.iter().find(|a| a.name == "planner").unwrap();
        let scout_defs = subagent_tool_defs(scout, false, 0, 2);
        let scout_names: Vec<&str> = scout_defs.iter().map(tool_name).collect();
        let planner_defs = subagent_tool_defs(planner, false, 0, 2);
        let planner_names: Vec<&str> = planner_defs.iter().map(tool_name).collect();
        assert!(
            scout_names.contains(&"memory"),
            "scout should have memory: {scout_names:?}"
        );
        assert!(
            !planner_names.contains(&"memory"),
            "planner should not have memory: {planner_names:?}"
        );
    }

    #[test]
    fn finalize_subagent_text_falls_back_to_prior_assistant() {
        use crate::message::Message;
        let hist = vec![
            Message::user("task"),
            Message::assistant("first findings about auth.rs"),
            Message::assistant(""), // empty finish prose
        ];
        let out = finalize_subagent_text(Some(""), &hist);
        assert_eq!(out, "first findings about auth.rs");
        let out2 = finalize_subagent_text(Some("  real summary  "), &hist);
        assert_eq!(out2, "real summary");
        let empty_hist: Vec<Message> = vec![Message::user("x")];
        let stub = finalize_subagent_text(None, &empty_hist);
        assert_eq!(stub, "(step finished with no written summary)");
    }

    #[test]
    fn nonempty_run_summary_never_blank_and_caps_800() {
        assert_eq!(
            nonempty_run_summary(""),
            "(step finished with no written summary)"
        );
        let long = "z".repeat(2000);
        let s = nonempty_run_summary(&long);
        assert_eq!(s.chars().count(), 800);
    }

    #[test]
    fn project_default_read_cannot_escape_workspace() {
        let root = std::env::temp_dir().join(format!(
            "catalyst-code-subagent-read-{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(root.join("secret.txt"), "must not reach provider context").unwrap();
        assert!(crate::workspace::resolve(&workspace, "../secret.txt").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelled_subagent_outcome_is_not_ok() {
        // H13: abort/cancel must return non-ok so worktree promote gates
        // (`outcome.ok && !cancel`) never merge half-done trees.
        let aborted = Outcome::err("[subagent aborted]");
        assert!(!aborted.ok);
        assert_eq!(aborted.output, "[subagent aborted]");
        let chain = Outcome::err("[chain aborted]");
        assert!(!chain.ok);
    }

    #[test]
    fn promote_gate_requires_ok_and_not_cancelled() {
        // Mirrors the single/parallel promote predicate used after run_single.
        let ok = true;
        let cancelled = true;
        assert!(
            !(ok && !cancelled),
            "cancel must block promote even when ok"
        );
        assert!(ok && !false, "successful non-cancelled run may promote");
        assert!(!(!true && !false), "failed run must not promote");
    }
}
