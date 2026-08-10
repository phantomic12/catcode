//! CEO / Control Center orchestration prompts (parent-model turns).
//!
//! Design (recon 2026-07-16): the parent model running goal-mode phase turns
//! **is** the CEO. Do **not** add a deployable `.catalyst-code/agents/orchestrator.md`
//! — that would nest agency (no leader for `contact_supervisor`, depth caps,
//! wrap-up truncation). Employees remain scout/researcher/planner/worker/
//! reviewer/oracle/delegate (+ specialized `*-reviewer.md`).
//!
//! Single-pass `/goal` continues to use [`crate::goal::planning_prompt`] /
//! [`crate::goal::build_wrapup_prompt`]. Control Center CEO mode (`ceo_mode`)
//! uses these helpers instead (and never prompts the user).

use crate::goal::{mission_summary_rel_path, DeployPrompt, GoalMode, GoalPlan};
use std::path::Path;

/// System-prompt append injected for CEO phase turns (Planning / Reviewing /
/// Verifying / Replanning). Not a deployable subagent persona.
pub fn ceo_persona_append() -> &'static str {
    r#"## CEO / Orchestrator role (Control Center)

You are the CEO of this mission. You own the outcome end-to-end.

- Employees (subagents) you may delegate to: scout, researcher, planner, worker, reviewer, context-builder, oracle, delegate, and project-custom agents. They execute; you decide.
- Never ask the user questions. Never call `ask`. Decide with best judgment, document assumptions, and continue.
- If an employee calls `contact_supervisor`, the harness auto-resolves — treat that as "proceed; do not re-ask the user."
- Review plans before deploy. After deploy, verify against the original goal using evidence files — not truncated chat snippets alone.
- Certify completion only when the user's request is fully implemented. If gaps remain, produce a concrete delta plan and continue within the iteration budget.
- Do not hardcode models or providers; respect the goal allowlist / role pins supplied in the turn prompt."#
}

/// CEO planning turn: produce a deploy plan via `goal_write_plan` — no user `ask`.
pub fn ceo_planning_prompt(mode: &GoalMode) -> String {
    let models = if mode.allowed_models.is_empty() {
        "(any available model)".to_string()
    } else {
        mode.allowed_models.join(", ")
    };
    let providers = if mode.allowed_providers.is_empty() {
        "(any logged-in provider)".to_string()
    } else {
        mode.allowed_providers.join(", ")
    };
    let role_line = {
        let mut parts = Vec::new();
        if let Some(m) = &mode.role_models.planner {
            parts.push(format!("planner\u{2192}{m}"));
        }
        if let Some(m) = &mode.role_models.worker {
            parts.push(format!("worker\u{2192}{m}"));
        }
        if let Some(m) = &mode.role_models.reviewer {
            parts.push(format!("reviewer\u{2192}{m}"));
        }
        if parts.is_empty() {
            "Role models: (not pinned \u{2014} omit step.model or pick from allowlist)".into()
        } else {
            format!(
                "Role models (harness will force these for matching agents; omit step.model for them): {}",
                parts.join(", ")
            )
        }
    };
    let model_conc = if mode.model_concurrency.is_empty() {
        format!(
            "Per-model concurrency: (use global cap {c})",
            c = mode.concurrency
        )
    } else {
        let mut pairs: Vec<String> = mode
            .model_concurrency
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        pairs.sort();
        format!("Per-model concurrency: {}", pairs.join(", "))
    };
    let revise = mode
        .revise_feedback
        .as_ref()
        .or(mode.self_review_feedback.as_ref())
        .map(|f| {
            format!(
                "\n\n## Revision feedback (from prior self-review or verify \u{2014} apply fully)\n{f}\n"
            )
        })
        .unwrap_or_default();
    let scout = mode
        .scout_findings
        .as_ref()
        .map(|s| format!("\n\n## Speculative scout findings\n{s}\n"))
        .unwrap_or_default();
    let iteration = format!(
        "## Iteration\nverify_cycle={}/{} plan_revision={}/{}\n",
        mode.iteration, mode.max_iterations, mode.plan_revision, mode.max_plan_revisions
    );

    format!(
        "{persona}\n\n# GOAL MODE \u{2014} CEO Planning\n\nYour only job this turn is to produce a structured deployment plan for employee subagents \u{2014} do not implement the goal yourself.\n\n## Goal\n{goal}\n\n{iteration}\n## Constraints\n- Max concurrent subagents at deploy time: {concurrency}\n- Max steps/tasks: {max_tasks}\n- Allowed models: {models}\n- Allowed providers: {providers}\n- {role_line}\n- {model_conc}\n{revise}{scout}\n## Required action\n1. Optionally use read/search tools briefly if you need repo context to plan well.\n2. Never call `ask`. If high-impact unknowns remain, choose the safest default, state it in the plan summary/risks, and continue.\n3. Call the `goal_write_plan` tool EXACTLY ONCE with a complete plan. Do not call it partially.\n\n## Plan quality rules\n- Prefer scout/context-builder before worker when the codebase area is unknown.\n- Prefer independent steps that can run in parallel (same parallel_group, empty depends_on).\n- Use depends_on when ordering or skip-on-failure matters (prior outputs are NOT auto-injected into dependent tasks \u{2014} each step.task must be self-contained).\n- Each step.task must be a self-contained prompt the subagent can execute without this chat.\n- Use agents: scout, researcher, planner, worker, reviewer, context-builder, oracle, delegate (or custom agents).\n- Prefer planner / worker / reviewer when they match the work so role model pins apply.\n- Assign step.model only from the allowed models list (or omit to use role/default).\n- Keep steps \u{2264} {max_tasks}.\n- Each implementation/review step's `task` must name how to validate that step (build + test commands and what 'done' looks like, e.g. `cargo test -p foo`, `go test ./...`, `npm run build && npm test`). The harness appends a generic validation gate to those steps automatically; your `task` makes it concrete.\n- Browser tools are available: for goals touching a web UI / site / needing end-to-end verification, plan steps that use the deferred `browser` tools and tell the worker to `load_tools` group `browser` (create / navigate / snapshot / click / fill / screenshot). Do not skip browser verification because it is harder.\n- Include concrete `validation` criteria the later VERIFY turn can check with file:line / build evidence.\n\nAfter goal_write_plan succeeds, briefly confirm the plan in one short paragraph. Do not start implementing.",
        persona = ceo_persona_append(),
        goal = mode.goal,
        iteration = iteration,
        concurrency = mode.concurrency,
        max_tasks = mode.max_tasks,
        models = models,
        providers = providers,
        role_line = role_line,
        model_conc = model_conc,
        revise = revise,
        scout = scout,
    )
}

/// Pre-deploy SELF-REVIEW: CERTIFY the plan or REVISE with concrete fixes.
///
/// Wire-compatible aliases: `VERDICT: PASS` == CERTIFY, `VERDICT: FAIL` == REVISE
/// (see [`crate::goal::parse_ceo_verdict`]).
pub fn self_review_prompt(mode: &GoalMode) -> String {
    let (summary, risks, validation, steps_block) = match &mode.plan {
        Some(p) => (
            p.summary.clone(),
            format_list(&p.risks),
            format_list(&p.validation),
            format_plan_steps(p),
        ),
        None => (
            "(no plan \u{2014} REVISE and demand a full goal_write_plan on the next planning turn)"
                .into(),
            "- (none)".into(),
            "- (none)".into(),
            "(no steps)".into(),
        ),
    };
    let iteration = format!(
        "## Plan revision\n{}/{} (pre-deploy self-review budget)\n",
        mode.plan_revision, mode.max_plan_revisions
    );

    format!(
        "{persona}\n\n# GOAL MODE \u{2014} CEO Self-Review (pre-deploy)\n\nReview the plan below for correctness, completeness, and risk **before** any employee is deployed.\n\n## Goal\n{goal}\n\n{iteration}\n## Plan summary\n{summary}\n\n## Risks\n{risks}\n\n## Validation criteria (must be checkable later)\n{validation}\n\n## Steps\n{steps_block}\n\n## Required action\n1. Read the plan carefully. Optionally spot-check the repo with read/search if a step looks unsafe or underspecified.\n2. Decide exactly one outcome:\n\n### CERTIFY (pass)\nIf the plan is sound enough to deploy, end your reply with ONE of these single lines:\n`VERDICT: CERTIFY`\n`VERDICT: PASS`\nOptionally one short paragraph of rationale above that line. Do not call goal_write_plan.\n\n### REVISE (fail)\nIf the plan has material gaps, end with ONE of:\n`VERDICT: REVISE`\n`VERDICT: FAIL`\nThen a section `## Fixes` (or `remaining_gaps:`) listing specific, actionable changes for the next planning turn (these become revise_feedback). Examples: missing scout, unsafe parallel writes, vague success criteria, missing depends_on, steps that cannot succeed without prior artifacts, over-budget step count.\n\n## Rules\n- Never ask the user. Never call `ask`.\n- Prefer CERTIFY when issues are minor/style-only; REVISE when deploy would likely waste waves or miss the goal.\n- Do not implement code. Do not spawn subagents.",
        persona = ceo_persona_append(),
        goal = mode.goal,
        iteration = iteration,
        summary = summary,
        risks = risks,
        validation = validation,
        steps_block = steps_block,
    )
}

/// Post-deploy VERIFY: CERTIFIED or REMAINING_GAPS for a delta replan.
///
/// Instructs the model to read the goal-scoped SUMMARY.md (via
/// [`mission_summary_rel_path`]) \u{2014} richer than wrap-up truncation.
/// Wire aliases: `VERDICT: PASS` == CERTIFIED, `VERDICT: FAIL` == REMAINING_GAPS.
pub fn verify_prompt(mode: &GoalMode) -> String {
    verify_prompt_with_workspace(mode, None)
}

/// Build the CEO verify prompt with bounded records from existing project stores.
pub fn verify_prompt_with_workspace(mode: &GoalMode, workspace: Option<&Path>) -> String {
    let summary_path = if mode.id.is_empty() {
        ".catalyst-code/goal-ux/artifacts/<goal_id>/SUMMARY.md".into()
    } else {
        mission_summary_rel_path(&mode.id)
    };
    let plan_summary = mode
        .plan
        .as_ref()
        .map(|p| p.summary.as_str())
        .unwrap_or("(none)");
    let validation = mode
        .plan
        .as_ref()
        .map(|p| format_list(&p.validation))
        .unwrap_or_else(|| "- (none recorded)".into());
    let steps_idx = format_deploy_index(mode);
    let iteration = format!("## Iteration\n{}/{}\n", mode.iteration, mode.max_iterations);
    let learning = workspace
        .map(|ws| format_learning_evidence(ws, &mode.goal))
        .unwrap_or_default();
    format!("{persona}\n\n# GOAL MODE \u{2014} CEO Verify (post-deploy)\n\nJudge whether the user's request is **fully** implemented. You certify completion or enumerate remaining gaps for a delta replan. Do not casually mark done.\n\n## Goal\n{goal}\n\n{iteration}\n## Plan summary\n{plan_summary}\n\n## Validation criteria\n{validation}\n\n## Evidence (read these \u{2014} do not rely on chat truncation)\n1. **Goal-scoped summary (required):** `{summary_path}`\n   - Use the `read_file` tool to load this file first. It aggregates full step outputs beyond wrap-up truncation.\n2. Per-step full artifacts listed in that summary (and below). Open any artifact whose summary is incomplete or contested.\n\n{learning}\n## Step index\n{steps_idx}\n\n## Required action\n1. Read `{summary_path}` (and additional step artifacts as needed).\n2. Check each validation criterion against evidence (prefer file:line citations and command/build output quoted from artifacts).\n3. Decide exactly one outcome:\n\n### CERTIFIED (pass)\nIf the goal is fully met, end with ONE of:\n`VERDICT: CERTIFIED`\n`VERDICT: PASS`\nAbove that line: a short evidence-backed summary (file:line / build output pointers).\n\n### REMAINING_GAPS (fail)\nIf anything material is unfinished or wrong, end with ONE of:\n`VERDICT: REMAINING_GAPS`\n`VERDICT: FAIL`\nThen a section `## Remaining gaps` or `remaining_gaps:` \u{2014} each gap must be concrete enough to become a delta plan step.\n\n## Rules\n- Never ask the user. Never call `ask`. Never call goal_write_plan this turn.\n- Do not implement code this turn. Certification is an evidence judgment only.", persona=ceo_persona_append(), goal=mode.goal, iteration=iteration, plan_summary=plan_summary, validation=validation, summary_path=summary_path, learning=learning, steps_idx=steps_idx)
}

fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>()
}

fn format_learning_evidence(workspace: &Path, goal: &str) -> String {
    let pid = crate::project_identity::resolve_project_identity(workspace).id;
    let mut out = String::from(
        "## Bounded project evidence (advisory; verify against artifacts)\n### research_evidence\n",
    );
    for ep in crate::episodes::load_episodes(&pid).iter().rev().take(3) {
        let tests = ep
            .tests_run
            .iter()
            .take(3)
            .map(|t| format!("{}={}", t.command, t.ok))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- episode {}: {}; tests: {}\n",
            ep.id,
            short(&ep.approach_summary, 180),
            short(&tests, 180)
        ));
    }
    out.push_str("### failure_atlas\n");
    for d in crate::failure_atlas::match_diagnostics(&pid, goal, 3) {
        out.push_str(&format!(
            "- [{}] x{} {}\n",
            d.class,
            d.count,
            short(&d.signature, 160)
        ));
    }
    out.push_str("### coverage_ledger\n");
    for area in crate::coverage_ledger::poorly_covered(&pid, 5) {
        out.push_str(&format!(
            "- {}: confidence {:.2}, files {}, symbols {}\n",
            area.area, area.confidence, area.indexed_files, area.indexed_symbols
        ));
    }
    out
}

/// Dispatch helper: CEO planning vs classic single-pass planning.
#[allow(dead_code)] // Control Center / tests; planning_prompt routes ceo_mode itself
pub fn planning_prompt_for(mode: &GoalMode) -> String {
    if mode.ceo_mode {
        ceo_planning_prompt(mode)
    } else {
        crate::goal::planning_prompt(mode)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_list(items: &[String]) -> String {
    if items.is_empty() {
        "- (none)".into()
    } else {
        items
            .iter()
            .map(|i| format!("- {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn format_plan_steps(plan: &GoalPlan) -> String {
    if plan.steps.is_empty() {
        return "(no steps)".into();
    }
    let mut out = String::new();
    for s in &plan.steps {
        let deps = if s.depends_on.is_empty() {
            "depends_on: []".into()
        } else {
            format!("depends_on: [{}]", s.depends_on.join(", "))
        };
        let model = s
            .model
            .as_ref()
            .map(|m| format!(" model={m}"))
            .unwrap_or_default();
        let group = s
            .parallel_group
            .as_ref()
            .map(|g| format!(" parallel_group={g}"))
            .unwrap_or_default();
        let title = if s.title.is_empty() {
            s.id.as_str()
        } else {
            s.title.as_str()
        };
        out.push_str(&format!(
            "### {id} \u{2014} {title} ({agent}){model}{group}\n{deps}\n\n{task}\n\n",
            id = s.id,
            title = title,
            agent = s.agent,
            model = model,
            group = group,
            deps = deps,
            task = s.task,
        ));
    }
    out
}

fn format_deploy_index(mode: &GoalMode) -> String {
    if mode.prompts.is_empty() {
        return "(no step results)".into();
    }
    let mut out = String::new();
    for p in &mode.prompts {
        out.push_str(&format_deploy_line(mode, p));
    }
    out
}

fn format_deploy_line(mode: &GoalMode, p: &DeployPrompt) -> String {
    let title = if p.title.is_empty() {
        p.step_id.as_str()
    } else {
        p.title.as_str()
    };
    let artifact = format!(
        ".catalyst-code/goal-ux/artifacts/{}/{}.md",
        mode.id, p.step_id
    );
    let sniff = p
        .summary
        .as_deref()
        .map(|s| {
            let t = s.trim();
            if t.chars().count() > 200 {
                let clipped: String = t.chars().take(200).collect();
                format!("{clipped}\u{2026}")
            } else {
                t.to_string()
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(see artifact)".into());
    format!(
        "- [{status}] {title} ({agent}): {sniff}\n  full output: `{artifact}`\n",
        status = p.status.as_str(),
        title = title,
        agent = p.agent,
        sniff = sniff,
        artifact = artifact,
    )
}
/// Machine-check the goal summary and every referenced artifact before a CEO
/// verdict can certify. Evidence must be present, non-empty, current, and
/// internally consistent with completed steps.
pub fn validate_certification_evidence(
    workspace: &std::path::Path,
    mode: &GoalMode,
) -> Result<Vec<String>, String> {
    if mode.id.is_empty() {
        return Err("missing goal id".into());
    }
    let rel = mission_summary_rel_path(&mode.id);
    let summary_path = workspace.join(&rel);
    let meta = std::fs::metadata(&summary_path)
        .map_err(|e| format!("required summary {rel} unavailable: {e}"))?;
    if meta.len() == 0 {
        return Err(format!("required summary {rel} is empty"));
    }
    let summary = std::fs::read_to_string(&summary_path)
        .map_err(|e| format!("read required summary {rel}: {e}"))?;
    let summary_mtime = meta
        .modified()
        .map_err(|e| format!("summary timestamp: {e}"))?;
    let mut referenced = Vec::new();
    for line in summary.lines() {
        if let Some(path) = line.trim().strip_prefix("path:") {
            let path = path.trim();
            if path.is_empty() || path.contains("..") {
                return Err(format!("invalid evidence path {path:?}"));
            }
            let abs = crate::workspace::resolve(workspace, path)
                .map_err(|e| format!("invalid evidence path {path:?}: {e}"))?;
            let artifact = std::fs::metadata(&abs)
                .map_err(|e| format!("referenced artifact {path} unavailable: {e}"))?;
            if artifact.len() == 0 {
                return Err(format!("referenced artifact {path} is empty"));
            }
            let changed = artifact
                .modified()
                .map_err(|e| format!("artifact timestamp: {e}"))?;
            if changed > summary_mtime {
                return Err(format!("summary {rel} is stale relative to {path}"));
            }
            let body = std::fs::read_to_string(&abs).unwrap_or_default();
            if body.trim().is_empty() || body.contains("(no step output)") {
                return Err(format!("referenced artifact {path} has no evidence"));
            }
            referenced.push(path.to_string());
        }
    }
    if referenced.len() < mode.prompts.len() {
        return Err("summary omits one or more step artifacts".into());
    }
    if summary
        .to_ascii_lowercase()
        .contains("contradiction: unresolved")
        || summary
            .to_ascii_lowercase()
            .contains("evidence: contradictory")
    {
        return Err("summary contains unresolved contradictory evidence".into());
    }
    Ok(referenced)
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{DeployStatus, GoalPhase, GoalStep};

    fn base_mode() -> GoalMode {
        GoalMode {
            id: "g-ceo".into(),
            goal: "add control center CEO loop".into(),
            phase: GoalPhase::Planning,
            concurrency: 2,
            max_tasks: 6,
            allowed_models: vec!["m1".into()],
            allowed_providers: vec![],
            auto_deploy: true,
            ceo_mode: true,
            iteration: 1,
            max_iterations: 3,
            parent_model: "m1".into(),
            reasoning_effort: "medium".into(),
            plan: Some(GoalPlan {
                summary: "plan then verify".into(),
                steps: vec![GoalStep {
                    id: "s1".into(),
                    agent: "worker".into(),
                    title: "implement".into(),
                    task: "implement the feature".into(),
                    model: None,
                    depends_on: vec![],
                    parallel_group: None,
                }],
                risks: vec!["scope creep".into()],
                validation: vec!["cargo check green".into()],
            }),
            prompts: vec![DeployPrompt {
                step_id: "s1".into(),
                agent: "worker".into(),
                task: "implement the feature".into(),
                model: None,
                status: DeployStatus::Done,
                run_id: None,
                summary: Some("implemented X".into()),
                title: "implement".into(),
            }],
            ..GoalMode::default()
        }
    }

    #[test]
    fn certification_evidence_requires_current_nonempty_artifacts() {
        let root = std::env::temp_dir().join(format!("ceo-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mode = base_mode();
        let summary = root.join(mission_summary_rel_path(&mode.id));
        std::fs::create_dir_all(summary.parent().unwrap()).unwrap();
        std::fs::write(
            &summary,
            "# Summary\npath: .catalyst-code/goal-ux/artifacts/g-ceo/s1.md\n",
        )
        .unwrap();
        assert!(validate_certification_evidence(&root, &mode).is_err());
        let artifact = root.join(".catalyst-code/goal-ux/artifacts/g-ceo/s1.md");
        std::fs::write(&artifact, "verified test output").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        std::fs::write(
            &summary,
            "# Summary\npath: .catalyst-code/goal-ux/artifacts/g-ceo/s1.md\n",
        )
        .unwrap();
        assert!(validate_certification_evidence(&root, &mode).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ceo_planning_forbids_ask() {
        let p = ceo_planning_prompt(&base_mode());
        assert!(p.contains("Never call `ask`"));
        assert!(!p.contains("call `ask` ONCE"));
        assert!(p.contains("CEO / Orchestrator role"));
        assert!(p.contains("goal_write_plan"));
    }

    #[test]
    fn self_review_demands_certify_or_revise() {
        let p = self_review_prompt(&base_mode());
        assert!(p.contains("VERDICT: CERTIFY"));
        assert!(p.contains("VERDICT: REVISE"));
        assert!(p.contains("VERDICT: PASS"));
        assert!(p.contains("VERDICT: FAIL"));
        assert!(p.contains("## Fixes"));
        assert!(p.contains("cargo check green"));
        assert!(!p.contains("call `ask` ONCE"));
    }

    #[test]
    fn verify_requires_mission_summary_path() {
        let m = base_mode();
        let p = verify_prompt(&m);
        let expected = mission_summary_rel_path(&m.id);
        assert!(p.contains(&expected));
        assert!(p.contains("VERDICT: CERTIFIED"));
        assert!(p.contains("VERDICT: REMAINING_GAPS"));
        assert!(p.contains("read_file"));
        assert!(p.contains("Never call `ask`"));
    }

    #[test]
    fn workspace_verify_prompt_labels_all_bounded_project_evidence() {
        let root = std::env::temp_dir().join(format!("ceo-learning-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let prompt = verify_prompt_with_workspace(&base_mode(), Some(&root));
        assert!(prompt.contains("### research_evidence"));
        assert!(prompt.contains("### failure_atlas"));
        assert!(prompt.contains("### coverage_ledger"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn planning_prompt_for_routes_on_ceo_mode() {
        let mut m = base_mode();
        m.ceo_mode = true;
        assert!(planning_prompt_for(&m).contains("Never call `ask`"));
        m.ceo_mode = false;
        let classic = planning_prompt_for(&m);
        assert!(classic.contains("call `ask` ONCE"));
    }

    #[test]
    fn persona_never_prompts_user() {
        let persona = ceo_persona_append();
        assert!(persona.contains("Never ask the user"));
        assert!(persona.contains("Never call `ask`"));
        assert!(persona.contains("scout"));
        assert!(persona.contains("worker"));
        assert!(persona.contains("reviewer"));
    }
}
