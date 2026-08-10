//! Goal mode: first-class plan-then-deploy orchestration.
//!
//! `/goal` (TUI/web) sends `start_goal`. The core owns a phase machine:
//! planning → plan_ready (optional) → deploying → running → synthesizing →
//! done|failed.
//!
//! Control Center / CEO mode (`ceo_mode=true`) extends the loop:
//! planning → reviewing (bounded revise) → deploying → running → verifying →
//! (certified→done) | (replan→planning, iteration-capped→failed).
//! The planning turn must call `goal_write_plan` with a structured plan;
//! deploy runs subagents under the user's concurrency and model/provider caps.
//! After workers finish, a parent synthesizing (single-pass) or verifying
//! (CEO) turn closes the loop without prompting the user.

use crate::protocol::{emit, Event};
use crate::tools::Outcome;
use crate::State;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    #[default]
    Idle,
    Planning,
    /// Parent CEO turn self-reviews the plan before deploy (CEO mode only).
    Reviewing,
    PlanReady,
    Deploying,
    Running,
    /// Workers finished; parent turn is summarizing results for the user.
    Synthesizing,
    /// Parent CEO turn verifies deploy artifacts against the goal (CEO mode).
    Verifying,
    /// Transitional: verify failed and a replan turn is starting (CEO mode).
    Replanning,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

impl GoalPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalPhase::Idle => "idle",
            GoalPhase::Planning => "planning",
            GoalPhase::Reviewing => "reviewing",
            GoalPhase::PlanReady => "plan_ready",
            GoalPhase::Deploying => "deploying",
            GoalPhase::Running => "running",
            GoalPhase::Synthesizing => "synthesizing",
            GoalPhase::Verifying => "verifying",
            GoalPhase::Replanning => "replanning",
            GoalPhase::Blocked => "blocked",
            GoalPhase::Done => "done",
            GoalPhase::Failed => "failed",
            GoalPhase::Cancelled => "cancelled",
        }
    }

    /// Phases where employee subagents may run without a live leader turn
    /// answering `contact_supervisor` — auto-resolve `need_decision`.
    pub fn auto_resolves_supervisor(&self) -> bool {
        matches!(
            self,
            GoalPhase::Planning
                | GoalPhase::Reviewing
                | GoalPhase::Deploying
                | GoalPhase::Running
                | GoalPhase::Verifying
                | GoalPhase::Replanning
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GoalTransitionError {
    pub code: String,
    pub from: GoalPhase,
    pub to: GoalPhase,
    pub message: String,
}

/// Validate the persisted goal state machine before mutating it. Explicit user
/// revision may reopen a terminal goal into Planning; autonomous execution may
/// otherwise only move forward or enter a terminal failure/cancellation state.
pub fn validate_transition(from: &GoalPhase, to: &GoalPhase) -> Result<(), GoalTransitionError> {
    let allowed = from == to
        || matches!(
            (from, to),
            (GoalPhase::Idle, GoalPhase::Planning)
                | (GoalPhase::Planning, GoalPhase::PlanReady)
                | (GoalPhase::PlanReady, GoalPhase::Reviewing)
                | (GoalPhase::PlanReady, GoalPhase::Deploying)
                | (GoalPhase::PlanReady, GoalPhase::Planning)
                | (GoalPhase::Reviewing, GoalPhase::PlanReady)
                | (GoalPhase::Reviewing, GoalPhase::Planning)
                | (GoalPhase::Deploying, GoalPhase::Running)
                | (GoalPhase::Running, GoalPhase::Verifying)
                | (GoalPhase::Running, GoalPhase::Synthesizing)
                | (GoalPhase::Verifying, GoalPhase::Done)
                | (GoalPhase::Verifying, GoalPhase::Replanning)
                | (GoalPhase::Replanning, GoalPhase::Planning)
                | (GoalPhase::Synthesizing, GoalPhase::Done)
                | (GoalPhase::Blocked, GoalPhase::Planning)
                | (GoalPhase::Done, GoalPhase::Planning)
                | (GoalPhase::Failed, GoalPhase::Planning)
                | (GoalPhase::Cancelled, GoalPhase::Planning)
        )
        || (matches!(
            to,
            GoalPhase::Failed | GoalPhase::Cancelled | GoalPhase::Blocked
        ) && !matches!(
            from,
            GoalPhase::Idle | GoalPhase::Done | GoalPhase::Failed | GoalPhase::Cancelled
        ));
    if allowed {
        return Ok(());
    }
    Err(GoalTransitionError {
        code: "invalid_goal_transition".into(),
        from: from.clone(),
        to: to.clone(),
        message: format!(
            "invalid goal transition: {} -> {}",
            from.as_str(),
            to.as_str()
        ),
    })
}

fn emit_transition_error(error: &GoalTransitionError) {
    emit(
        &Event::new("error")
            .with("code", json!(error.code))
            .with("from", json!(error.from.as_str()))
            .with("to", json!(error.to.as_str()))
            .with("message", json!(error.message)),
    );
}

/// Structured review / verify verdict surfaced to the UI.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GoalVerdict {
    pub ok: bool,
    pub summary: String,
    #[serde(default)]
    pub evidence_paths: Vec<String>,
    /// Unix epoch milliseconds when the verdict was recorded.
    #[serde(default)]
    pub at_ms: u64,
}

impl GoalVerdict {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "summary": self.summary,
            "evidence_paths": self.evidence_paths,
            "at": self.at_ms,
        })
    }
}

/// Default verify→replan cycles for Control Center CEO mode.
pub const DEFAULT_CEO_MAX_ITERATIONS: u32 = 3;
/// Default pre-deploy self-review revise budget for CEO mode.
pub const DEFAULT_CEO_MAX_PLAN_REVISIONS: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl DeployStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeployStatus::Pending => "pending",
            DeployStatus::Running => "running",
            DeployStatus::Done => "done",
            DeployStatus::Failed => "failed",
            DeployStatus::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalStep {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub parallel_group: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalPlan {
    pub summary: String,
    pub steps: Vec<GoalStep>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub validation: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployPrompt {
    pub step_id: String,
    pub agent: String,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
    pub status: DeployStatus,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub title: String,
}

/// Optional per-role model overrides from the Advanced section of `/goal`.
/// Empty string / None means "use step model or allowlist / parent default".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoleModels {
    #[serde(default)]
    pub planner: Option<String>,
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub reviewer: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalMode {
    pub id: String,
    pub goal: String,
    pub phase: GoalPhase,
    pub concurrency: u32,
    pub max_tasks: u32,
    pub allowed_models: Vec<String>,
    pub allowed_providers: Vec<String>,
    /// When true, deploy immediately after a valid plan. When false, stop at
    /// `plan_ready` until `approve_goal_plan`.
    pub auto_deploy: bool,
    /// When true, run the autonomous CEO loop (self-review → verify → replan).
    /// When false, classic single-pass `/goal`: plan → deploy → synthesize → Done.
    #[serde(default)]
    pub ceo_mode: bool,
    /// Verify→replan iteration counter (0 before first verify).
    #[serde(default)]
    pub iteration: u32,
    /// Cap on verify→replan cycles. 0 disables replan (single-pass verify skip
    /// when combined with `ceo_mode=false`).
    #[serde(default)]
    pub max_iterations: u32,
    /// Pre-deploy self-review revise counter.
    #[serde(default)]
    pub plan_revision: u32,
    /// Cap on plan self-review revisions before first deploy.
    #[serde(default)]
    pub max_plan_revisions: u32,
    /// Latest plan self-review verdict (CEO mode).
    #[serde(default)]
    pub review_verdict: Option<GoalVerdict>,
    /// Latest post-deploy verify verdict (CEO mode).
    #[serde(default)]
    pub verify_verdict: Option<GoalVerdict>,
    /// Gaps from the latest failed verify, fed into the next replan.
    #[serde(default)]
    pub remaining_gaps: Vec<String>,
    /// Self-review feedback (also mirrored into `revise_feedback` on revise).
    #[serde(default)]
    pub self_review_feedback: Option<String>,
    /// True once verify certifies the goal complete.
    #[serde(default)]
    pub certified: bool,
    /// Advanced: preferred model per agent role (planner / worker / reviewer).
    #[serde(default)]
    pub role_models: RoleModels,
    /// Advanced: max concurrent subagents per model id. Missing keys fall back
    /// to the global `concurrency` cap.
    #[serde(default)]
    pub model_concurrency: HashMap<String, u32>,
    pub plan: Option<GoalPlan>,
    pub prompts: Vec<DeployPrompt>,
    pub active_run_ids: Vec<String>,
    pub version: u64,
    pub error: Option<String>,
    /// Orchestrator model used for the planning turn / parent model for deploy.
    pub parent_model: String,
    #[serde(default)]
    pub reasoning_effort: String,
    /// True when a plan was accepted and deploy should run after the planning turn.
    #[serde(default)]
    pub deploy_after_turn: bool,
    /// Optional revise feedback appended on the next planning turn.
    #[serde(default)]
    pub revise_feedback: Option<String>,
    /// Speculative scout findings gathered while the planner runs.
    #[serde(default)]
    pub scout_findings: Option<String>,
}

impl Default for GoalMode {
    fn default() -> Self {
        Self {
            id: String::new(),
            goal: String::new(),
            phase: GoalPhase::Idle,
            concurrency: 4,
            max_tasks: 8,
            allowed_models: Vec::new(),
            allowed_providers: Vec::new(),
            auto_deploy: true,
            ceo_mode: false,
            iteration: 0,
            max_iterations: 0,
            plan_revision: 0,
            max_plan_revisions: 0,
            review_verdict: None,
            verify_verdict: None,
            remaining_gaps: Vec::new(),
            self_review_feedback: None,
            certified: false,
            role_models: RoleModels::default(),
            model_concurrency: HashMap::new(),
            plan: None,
            prompts: Vec::new(),
            active_run_ids: Vec::new(),
            version: 0,
            error: None,
            parent_model: String::new(),
            reasoning_effort: "medium".into(),
            deploy_after_turn: false,
            revise_feedback: None,
            scout_findings: None,
        }
    }
}

impl GoalMode {
    pub fn is_active(&self) -> bool {
        !matches!(
            self.phase,
            GoalPhase::Idle | GoalPhase::Done | GoalPhase::Failed | GoalPhase::Cancelled
        )
    }

    pub fn touch(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn to_event_value(&self) -> Value {
        json!({
            "id": self.id,
            "goal": self.goal,
            "phase": self.phase.as_str(),
            "concurrency": self.concurrency,
            "max_tasks": self.max_tasks,
            "allowed_models": self.allowed_models,
            "allowed_providers": self.allowed_providers,
            "auto_deploy": self.auto_deploy,
            "ceo_mode": self.ceo_mode,
            "mode": if self.ceo_mode { "ceo" } else { "single_pass" },
            "iteration": self.iteration,
            "max_iterations": self.max_iterations,
            "plan_revision": self.plan_revision,
            "max_plan_revisions": self.max_plan_revisions,
            "review_verdict": self.review_verdict.as_ref().map(|v| v.to_json()),
            "verify_verdict": self.verify_verdict.as_ref().map(|v| v.to_json()),
            "remaining_gaps": self.remaining_gaps,
            "self_review_feedback": self.self_review_feedback,
            "certified": self.certified,
            "role_models": {
                "planner": self.role_models.planner,
                "worker": self.role_models.worker,
                "reviewer": self.role_models.reviewer,
            },
            "model_concurrency": self.model_concurrency,
            "prompts": self.prompts.iter().map(|p| json!({
                "step_id": p.step_id,
                "agent": p.agent,
                "title": p.title,
                "task": p.task,
                "model": p.model,
                "status": p.status.as_str(),
                "run_id": p.run_id,
                "summary": p.summary,
            })).collect::<Vec<_>>(),
            "active_run_ids": self.active_run_ids,
            "version": self.version,
            "error": self.error,
            "parent_model": self.parent_model,
        })
    }

    /// Cap for a specific model: per-model override if set, else global concurrency.
    pub fn concurrency_for_model(&self, model: &str) -> u32 {
        self.model_concurrency
            .get(model)
            .copied()
            .unwrap_or(self.concurrency)
            .clamp(1, self.concurrency.max(1))
    }
}

/// Human-facing scheduling profile derived from the user's concurrency cap.
/// A high cap is treated as an instruction to shape the plan for breadth, not
/// merely as a larger semaphore for a plan that may still be entirely serial.
pub fn execution_profile(concurrency: u32) -> &'static str {
    match concurrency {
        0 | 1 => "serial",
        2..=7 => "parallel",
        _ => "ultra_parallel",
    }
}

fn planning_parallelism_guidance(mode: &GoalMode) -> String {
    let available = mode.concurrency.min(mode.max_tasks).max(1);
    match execution_profile(mode.concurrency) {
        "ultra_parallel" => {
            // Leave room for an integration/review task when the task budget
            // permits, while still asking the planner to fill almost all of a
            // large concurrency window immediately.
            let roots = if mode.max_tasks > available {
                available
            } else {
                available.saturating_sub(1).max(1)
            };
            format!(
                r#"Execution profile: ULTRA PARALLEL.
- Treat the {available} available slots as a throughput budget that should be actively used.
- Aim for about {roots} useful root steps (empty depends_on) in the first launch window when the goal has enough separable work.
- Split reconnaissance by independent area and run it concurrently; do not put one global scout in front of unrelated workers.
- Partition implementation steps by non-overlapping files/components. Add dependencies only when a step truly consumes another step's artifact.
- Reserve a final integration/review step when useful, depending only on the specific work it validates.
- A sequential chain is still correct for genuinely indivisible work, but briefly say why in the plan summary."#
            )
        }
        "parallel" => format!(
            r#"Execution profile: PARALLEL.
- Expose up to {available} independent root steps where the work naturally separates.
- Do not make unrelated work wait behind a single reconnaissance step.
- Add depends_on only for a real data, artifact, or ordering dependency."#
        ),
        _ => "Execution profile: SERIAL. Produce the shortest dependency chain that safely completes the goal.".into(),
    }
}

/// Resolve which model a step should run with, given role overrides + allowlist.
pub fn resolve_step_model(
    mode: &GoalMode,
    agent: &str,
    step_model: Option<String>,
) -> Option<String> {
    let role = match agent {
        "planner" => mode.role_models.planner.clone(),
        "worker" => mode.role_models.worker.clone(),
        "reviewer" => mode.role_models.reviewer.clone(),
        _ => None,
    };
    // Role override wins when set (Advanced section is explicit). Do NOT
    // auto-pin `allowed_models[0]` here — that forced every step through the
    // model-override path (and previously the worktree parallel wrapper) even
    // when the planner omitted step.model. Parent model + allowlist filter in
    // `resolve_model_candidates` already enforces the allowlist.
    let candidate = role.or(step_model);
    match candidate {
        Some(m)
            if !mode.allowed_models.is_empty() && !mode.allowed_models.iter().any(|a| a == &m) =>
        {
            // Role/step model outside allowlist → fall back to first allowed.
            mode.allowed_models.first().cloned()
        }
        other => other,
    }
}

/// Whether a goal deploy step should run in a git worktree when concurrency > 1.
///
/// Read-focused agents write disjoint artifacts (e.g. `review/<id>.md`) and do
/// not need full-tree isolation. Forcing `worktree:true` on every step wrapped
/// each one as a one-item parallel batch and raced concurrent `git worktree add`,
/// which aborted entire review waves (session 2026-07-15_13-46-39).
pub fn goal_step_needs_worktree(agent: &str) -> bool {
    match agent {
        "scout" | "researcher" | "planner" | "reviewer" | "context-builder" | "oracle" => false,
        // worker (and unknown/custom agents that may edit shared files)
        _ => true,
    }
}

/// Appended to every deploy step's task so workers know the deferred `browser`
/// tools (and git/web/bulk) exist and can be loaded on demand. The subagent
/// system prompt already lists the deferred groups; this reminder ties them to
/// the step so web/UI verification is not silently skipped.
const TOOL_AVAILABILITY_SECTION: &str = "

# Tool availability
Deferred tools are loadable on demand via `load_tools` (call it before first use): `git`, `web`, `bulk`, and `browser` (native WRY browser — create/navigate/snapshot/click/fill/screenshot). If this step needs to drive or verify a web page, UI, or end-to-end flow, load the `browser` group and use the `browser_*` tools instead of skipping that work.";

/// Appended (conditionally — see [`goal_step_should_validate`]) to
/// implementation/review steps so every deploy step self-validates: build,
/// run tests, confirm zero errors, and report results. This is the "ensure
/// validation after every step" guarantee — it does not depend on the planner
/// remembering to write a test clause into `step.task`.
const VALIDATION_GATE_SECTION: &str = "

# Validation gate (required for this step)
Where applicable to this step's work, finish only after:
- Building/compiling the affected code and confirming zero errors (e.g. `cargo check` / `cargo build`, `go build ./...`, `npm run build`, `tsc --noEmit`).
- Running the relevant test suite and confirming zero failures (e.g. `cargo test`, `go test ./...`, `npm test`, `pytest`). If you added or changed behavior, add or update tests for it.
- Fixing any errors or failing tests before reporting done — never leave a red build.
- Summarizing the build/test results (commands run + pass/fail counts) in your final output.";

/// Whether a goal deploy step should receive the per-step validation gate
/// (build/compile + run tests, confirm zero errors). Pure-recon agents
/// (scout / researcher / context-builder) only read and produce notes, so the
/// gate is skipped for them. Implementation, review, and custom agents must
/// validate their own work.
pub fn goal_step_should_validate(agent: &str) -> bool {
    !matches!(agent, "scout" | "researcher" | "context-builder")
}

fn should_validate_goal_after_wave(wave_idx: usize, wave_count: usize) -> bool {
    wave_count > 0 && wave_idx + 1 == wave_count
}

// ---------------------------------------------------------------------------
// Construction / validation
// ---------------------------------------------------------------------------

pub struct StartGoalArgs {
    pub goal: String,
    pub concurrency: Option<u32>,
    pub max_tasks: Option<u32>,
    pub allowed_models: Vec<String>,
    pub allowed_providers: Vec<String>,
    pub auto_deploy: Option<bool>,
    /// Enable autonomous CEO loop (Control Center). Default false = classic /goal.
    pub ceo_mode: Option<bool>,
    pub max_iterations: Option<u32>,
    pub max_plan_revisions: Option<u32>,
    pub role_models: RoleModels,
    pub model_concurrency: HashMap<String, u32>,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub default_concurrency: u32,
    pub default_max_tasks: u32,
}

fn normalize_role_model(m: Option<String>, allowed: &[String]) -> Option<String> {
    let m = m.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())?;
    if !allowed.is_empty() && !allowed.iter().any(|a| a == &m) {
        return None; // drop invalid role model vs allowlist
    }
    Some(m)
}

pub fn new_goal(args: StartGoalArgs) -> Result<GoalMode, String> {
    let goal = args.goal.trim().to_string();
    if goal.is_empty() {
        return Err("goal text must not be empty".into());
    }
    if goal.chars().count() < 4 {
        return Err("goal text is too short".into());
    }
    let concurrency = args
        .concurrency
        .unwrap_or(args.default_concurrency)
        .clamp(1, 32);
    let max_tasks = args
        .max_tasks
        .unwrap_or(args.default_max_tasks)
        .clamp(1, 64);
    if concurrency > max_tasks {
        return Err(format!(
            "concurrency ({concurrency}) cannot exceed max_tasks ({max_tasks})"
        ));
    }
    let allowed_models = args.allowed_models;
    let role_models = RoleModels {
        planner: normalize_role_model(args.role_models.planner, &allowed_models),
        worker: normalize_role_model(args.role_models.worker, &allowed_models),
        reviewer: normalize_role_model(args.role_models.reviewer, &allowed_models),
    };
    // Clamp per-model concurrency to 1..=global concurrency.
    let mut model_concurrency: HashMap<String, u32> = HashMap::new();
    for (k, v) in args.model_concurrency {
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        if !allowed_models.is_empty() && !allowed_models.iter().any(|a| a == &key) {
            continue;
        }
        model_concurrency.insert(key, v.clamp(1, concurrency));
    }
    // Planning turn prefers planner role model when set.
    let parent_model = role_models.planner.clone().unwrap_or(args.model);

    let ceo_mode = args.ceo_mode.unwrap_or(false);
    let (max_iterations, max_plan_revisions) = if ceo_mode {
        (
            args.max_iterations
                .unwrap_or(DEFAULT_CEO_MAX_ITERATIONS)
                .clamp(1, 32),
            args.max_plan_revisions
                .unwrap_or(DEFAULT_CEO_MAX_PLAN_REVISIONS)
                .clamp(0, 16),
        )
    } else {
        // Classic single-pass: no self-review / verify-replan budgets.
        (
            args.max_iterations.unwrap_or(0),
            args.max_plan_revisions.unwrap_or(0),
        )
    };

    let id = format!("goal-{}", now_ms());
    Ok(GoalMode {
        id,
        goal,
        phase: GoalPhase::Planning,
        concurrency,
        max_tasks,
        allowed_models,
        allowed_providers: args.allowed_providers,
        auto_deploy: args.auto_deploy.unwrap_or(true),
        ceo_mode,
        iteration: 0,
        max_iterations,
        plan_revision: 0,
        max_plan_revisions,
        review_verdict: None,
        verify_verdict: None,
        remaining_gaps: Vec::new(),
        self_review_feedback: None,
        certified: false,
        role_models,
        model_concurrency,
        plan: None,
        prompts: Vec::new(),
        active_run_ids: Vec::new(),
        version: 1,
        error: None,
        parent_model,
        reasoning_effort: args.reasoning_effort.unwrap_or_else(|| "medium".into()),
        deploy_after_turn: false,
        revise_feedback: None,
        scout_findings: None,
    })
}

/// Parse + validate a `goal_write_plan` payload into a GoalPlan and deploy prompts.
pub fn apply_plan(
    mode: &mut GoalMode,
    args: &Value,
    known_agents: &HashSet<String>,
) -> Result<(), String> {
    if mode.phase != GoalPhase::Planning {
        return Err(format!(
            "goal_write_plan only valid during planning (phase={})",
            mode.phase.as_str()
        ));
    }
    let summary = args
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if summary.is_empty() {
        return Err("goal_write_plan requires a non-empty 'summary'".into());
    }
    let steps_raw = args
        .get("steps")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "goal_write_plan requires a 'steps' array".to_string())?;
    if steps_raw.is_empty() {
        return Err("goal_write_plan requires at least one step".into());
    }
    if steps_raw.len() as u32 > mode.max_tasks {
        return Err(format!(
            "plan has {} steps (max_tasks={})",
            steps_raw.len(),
            mode.max_tasks
        ));
    }

    let mut steps: Vec<GoalStep> = Vec::new();
    let mut ids: HashSet<String> = HashSet::new();
    for (i, s) in steps_raw.iter().enumerate() {
        let id = s
            .get("id")
            .and_then(|v| v.as_str())
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| format!("{}", i + 1));
        if !ids.insert(id.clone()) {
            return Err(format!("duplicate step id '{id}'"));
        }
        let agent = s
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if agent.is_empty() {
            return Err(format!("step '{id}' missing agent"));
        }
        if !known_agents.is_empty() && !known_agents.contains(&agent) {
            // Soft warning: still allow unknown custom agents the registry may
            // not have listed yet; only hard-fail empty.
        }
        let title = s
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let task = s
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if task.is_empty() {
            return Err(format!("step '{id}' missing task prompt"));
        }
        let step_model = s
            .get("model")
            .and_then(|v| v.as_str())
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        // Strip models outside the allowlist (empty allowlist = unrestricted).
        let step_model = match step_model {
            Some(m)
                if !mode.allowed_models.is_empty()
                    && !mode.allowed_models.iter().any(|a| a == &m) =>
            {
                None
            }
            other => other,
        };
        // Role models (Advanced) applied later when materializing DeployPrompt.
        let depends_on: Vec<String> = s
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let parallel_group = s
            .get("parallel_group")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        steps.push(GoalStep {
            id,
            agent,
            title: if title.is_empty() {
                format!("step {}", i + 1)
            } else {
                title
            },
            task,
            model: step_model,
            depends_on,
            parallel_group,
        });
    }

    // Validate depends_on references.
    for step in &steps {
        for dep in &step.depends_on {
            if !ids.contains(dep) {
                return Err(format!(
                    "step '{}' depends on unknown id '{}'",
                    step.id, dep
                ));
            }
            if dep == &step.id {
                return Err(format!("step '{}' cannot depend on itself", step.id));
            }
        }
    }
    // Cycle check via topo waves.
    if topo_waves(&steps).is_err() {
        return Err("step depends_on graph contains a cycle".into());
    }

    let risks: Vec<String> = args
        .get("risks")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let validation: Vec<String> = args
        .get("validation")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let plan = GoalPlan {
        summary,
        steps: steps.clone(),
        risks,
        validation,
    };

    let mut prompts = Vec::new();
    let scout_block = mode
        .scout_findings
        .as_ref()
        .map(|s| format!("\n\n# Speculative scout findings\n{s}"))
        .unwrap_or_default();
    for step in &plan.steps {
        let validation_block = if plan.validation.is_empty() {
            "(none specified)".to_string()
        } else {
            plan.validation
                .iter()
                .map(|v| format!("- {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Per-step validation gate is appended only to implementation/review
        // agents (pure-recon steps only read — see goal_step_should_validate).
        let validation_gate = if goal_step_should_validate(&step.agent) {
            VALIDATION_GATE_SECTION
        } else {
            ""
        };
        let full_task = format!(
            "# Goal\n{}\n\n# Step: {}\n{}\n\n# Validation criteria for the overall goal\n{}{}{}{}",
            mode.goal,
            step.title,
            step.task,
            validation_block,
            scout_block,
            TOOL_AVAILABILITY_SECTION,
            validation_gate,
        );
        prompts.push(DeployPrompt {
            step_id: step.id.clone(),
            agent: step.agent.clone(),
            task: full_task,
            model: resolve_step_model(mode, &step.agent, step.model.clone()),
            status: DeployStatus::Pending,
            run_id: None,
            summary: None,
            title: step.title.clone(),
        });
    }

    mode.plan = Some(plan);
    mode.prompts = prompts;
    mode.error = None;
    mode.touch();
    mode.deploy_after_turn = mode.auto_deploy;
    if !transition(mode, GoalPhase::PlanReady, Some("plan ready")) {
        return Err("invalid goal phase while applying plan".into());
    }
    Ok(())
}

/// Partition steps into waves respecting depends_on. Err on cycles.
pub fn topo_waves(steps: &[GoalStep]) -> Result<Vec<Vec<String>>, String> {
    let mut remaining: HashMap<String, HashSet<String>> = HashMap::new();
    for s in steps {
        remaining.insert(s.id.clone(), s.depends_on.iter().cloned().collect());
    }
    let mut done: HashSet<String> = HashSet::new();
    let mut waves: Vec<Vec<String>> = Vec::new();
    while done.len() < steps.len() {
        let mut wave: Vec<String> = remaining
            .iter()
            .filter(|(id, deps)| !done.contains(*id) && deps.iter().all(|d| done.contains(d)))
            .map(|(id, _)| id.clone())
            .collect();
        if wave.is_empty() {
            return Err("cycle".into());
        }
        wave.sort();
        for id in &wave {
            done.insert(id.clone());
        }
        waves.push(wave);
    }
    Ok(waves)
}

/// Filter a model candidate list by goal allowlists (models + providers via registry).
pub fn filter_model_candidates(
    candidates: &[String],
    mode: &GoalMode,
    model_providers: &HashMap<String, String>,
) -> Vec<String> {
    candidates
        .iter()
        .filter(|m| {
            if !mode.allowed_models.is_empty() && !mode.allowed_models.iter().any(|a| a == *m) {
                return false;
            }
            if !mode.allowed_providers.is_empty() {
                let prov = model_providers.get(*m).map(|s| s.as_str()).unwrap_or("");
                if prov.is_empty() {
                    // Unknown provider mapping: keep candidate (may still work on active).
                    return true;
                }
                if !mode
                    .allowed_providers
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(prov))
                {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// Cap parallel concurrency for subagent calls when goal mode is active.
pub fn cap_concurrency(requested: u32, mode: &GoalMode) -> u32 {
    requested.min(mode.concurrency).max(1)
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

pub fn emit_goal_state(mode: &GoalMode) {
    let mut ev = Event::new("goal_state");
    if let Value::Object(map) = mode.to_event_value() {
        for (k, v) in map {
            ev = ev.with(&k, v);
        }
    }
    emit(&ev);
}

pub fn emit_goal_plan(mode: &GoalMode) {
    let Some(plan) = &mode.plan else {
        return;
    };
    emit(
        &Event::new("goal_plan")
            .with("id", json!(mode.id))
            .with("summary", json!(plan.summary))
            .with("steps", json!(plan.steps))
            .with("risks", json!(plan.risks))
            .with("validation", json!(plan.validation))
            .with("version", json!(mode.version)),
    );
}

pub fn emit_goal_phase(from: &GoalPhase, to: &GoalPhase, message: Option<&str>) {
    emit_goal_phase_progress(from, to, message, None, None, None);
}

/// Goal phase change with optional wave/progress counts for UI continuity.
pub fn emit_goal_phase_progress(
    from: &GoalPhase,
    to: &GoalPhase,
    message: Option<&str>,
    wave: Option<usize>,
    step_count: Option<usize>,
    done_count: Option<usize>,
) {
    let mut ev = Event::new("goal_phase")
        .with("from", json!(from.as_str()))
        .with("to", json!(to.as_str()));
    if let Some(m) = message {
        ev = ev.with("message", json!(m));
    }
    if let Some(w) = wave {
        ev = ev.with("wave", json!(w));
    }
    if let Some(n) = step_count {
        ev = ev.with("step_count", json!(n));
    }
    if let Some(n) = done_count {
        ev = ev.with("done_count", json!(n));
    }
    emit(&ev);
}

/// Ensure step summary text is never empty for UIs / wrap-up.
pub fn nonempty_step_summary(output: &str) -> String {
    let t = output.trim();
    if t.is_empty() {
        "(step finished with no written summary)".to_string()
    } else {
        truncate_str(t, 1600)
    }
}

/// One-shot lasting signal when a deploy step settles (alongside `goal_state`).
pub fn emit_goal_step_complete(
    step_id: &str,
    title: &str,
    agent: &str,
    ok: bool,
    status: &str,
    summary: &str,
    run_id: Option<&str>,
) {
    let summary = nonempty_step_summary(summary);
    let mut ev = Event::new("goal_step_complete")
        .with("step_id", json!(step_id))
        .with("title", json!(title))
        .with("agent", json!(agent))
        .with("ok", json!(ok))
        .with("status", json!(status))
        .with("summary", json!(summary));
    if let Some(rid) = run_id {
        if !rid.is_empty() {
            ev = ev.with("run_id", json!(rid));
        }
    }
    emit(&ev);
}

/// Lasting completion text when model wrap-up is skipped or as a bridge.
pub fn emit_goal_completion_summary(text: &str) {
    let text = text.trim();
    let text = if text.is_empty() {
        "Goal finished (no completion summary provided)."
    } else {
        text
    };
    emit(&Event::new("goal_completion_summary").with("text", json!(text)));
}

/// Emit iteration budget progress for Control Center UI.
pub fn emit_goal_iteration(mode: &GoalMode) {
    emit(
        &Event::new("goal_iteration")
            .with("id", json!(mode.id))
            .with("iteration", json!(mode.iteration))
            .with("max_iterations", json!(mode.max_iterations))
            .with("plan_revision", json!(mode.plan_revision))
            .with("max_plan_revisions", json!(mode.max_plan_revisions)),
    );
}

pub fn emit_goal_review_verdict(mode: &GoalMode, verdict: &GoalVerdict) {
    emit(
        &Event::new("goal_review_verdict")
            .with("id", json!(mode.id))
            .with("ok", json!(verdict.ok))
            .with("summary", json!(verdict.summary))
            .with("iteration", json!(mode.iteration))
            .with("plan_revision", json!(mode.plan_revision))
            .with("evidence_paths", json!(verdict.evidence_paths)),
    );
}

pub fn emit_goal_verify_verdict(mode: &GoalMode, verdict: &GoalVerdict, gaps: &[String]) {
    emit(
        &Event::new("goal_verify_verdict")
            .with("id", json!(mode.id))
            .with("ok", json!(verdict.ok))
            .with("summary", json!(verdict.summary))
            .with("iteration", json!(mode.iteration))
            .with("remaining_gaps", json!(gaps))
            .with("evidence_paths", json!(verdict.evidence_paths)),
    );
}

pub fn emit_goal_certified(mode: &GoalMode, summary: &str) {
    emit(
        &Event::new("goal_certified")
            .with("id", json!(mode.id))
            .with("summary", json!(summary))
            .with("iteration", json!(mode.iteration))
            .with("certified", json!(true)),
    );
}

/// Parse CEO review/verify model output for structured verdicts and optional gaps.
///
/// Accepts wire synonyms:
/// - pass/certify: `VERDICT: PASS` | `VERDICT: CERTIFY` | `VERDICT: CERTIFIED`
/// - fail/revise:  `VERDICT: FAIL` | `VERDICT: REVISE` | `VERDICT: REMAINING_GAPS`
///
/// Fail synonyms win when both appear. Missing verdict → fail (safe default).
pub fn parse_ceo_verdict(text: &str) -> (bool, String, Vec<String>) {
    let upper = text.to_ascii_uppercase();
    let fail = upper.contains("VERDICT: FAIL")
        || upper.contains("VERDICT: REVISE")
        || upper.contains("VERDICT: REMAINING_GAPS");
    let pass = upper.contains("VERDICT: PASS")
        || upper.contains("VERDICT: CERTIFY")
        || upper.contains("VERDICT: CERTIFIED");
    let ok = if fail { false } else { pass };
    let mut gaps: Vec<String> = Vec::new();
    let mut in_gaps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("gaps:")
            || lower.starts_with("remaining_gaps:")
            || lower == "## fixes"
            || lower == "## remaining gaps"
            || lower.starts_with("## fixes")
            || lower.starts_with("## remaining gaps")
        {
            in_gaps = true;
            let rest = trimmed.split_once(':').map(|(_, r)| r.trim()).unwrap_or("");
            if !rest.is_empty() {
                gaps.push(rest.to_string());
            }
            continue;
        }
        if in_gaps {
            if trimmed.is_empty() {
                in_gaps = false;
                continue;
            }
            if trimmed.starts_with("VERDICT:") || trimmed.starts_with("##") {
                in_gaps = false;
                continue;
            }
            let item = trimmed
                .trim_start_matches('-')
                .trim_start_matches('*')
                .trim();
            if !item.is_empty() {
                gaps.push(item.to_string());
            }
        }
    }
    let summary = {
        let t = text.trim();
        if t.is_empty() {
            if ok {
                "VERDICT: PASS".to_string()
            } else {
                "VERDICT: FAIL (no verdict text)".to_string()
            }
        } else {
            truncate_str(t, 2000)
        }
    };
    (ok, summary, gaps)
}

/// Workspace-relative path for the goal-scoped mission summary used by verify.
pub fn mission_summary_rel_path(goal_id: &str) -> String {
    format!(".catalyst-code/goal-ux/artifacts/{goal_id}/SUMMARY.md")
}

/// Write a goal-scoped SUMMARY.md richer than truncated prompt lines.
/// Returns the workspace-relative path on success.
pub fn write_mission_summary(workspace: &Path, mode: &GoalMode) -> Option<String> {
    if mode.id.is_empty() {
        return None;
    }
    let dir = workspace
        .join(".catalyst-code")
        .join("goal-ux")
        .join("artifacts")
        .join(&mode.id);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# Mission summary: {}", mode.goal.trim()));
    lines.push(format!("goal_id: {}", mode.id));
    lines.push(format!(
        "iteration: {}/{}",
        mode.iteration, mode.max_iterations
    ));
    if let Some(plan) = &mode.plan {
        lines.push(String::new());
        lines.push(format!("## Plan\n{}", plan.summary.trim()));
        if !plan.validation.is_empty() {
            lines.push(String::new());
            lines.push("## Validation criteria".into());
            for v in &plan.validation {
                lines.push(format!("- {v}"));
            }
        }
    }
    lines.push(String::new());
    lines.push("## Step evidence".into());
    for p in &mode.prompts {
        let title = if p.title.is_empty() {
            p.step_id.as_str()
        } else {
            p.title.as_str()
        };
        let artifact_rel = format!(
            ".catalyst-code/goal-ux/artifacts/{}/{}.md",
            mode.id, p.step_id
        );
        let artifact_abs = workspace.join(&artifact_rel);
        let body = std::fs::read_to_string(&artifact_abs).unwrap_or_else(|_| {
            p.summary
                .clone()
                .unwrap_or_else(|| "(no step output)".into())
        });
        lines.push(format!(
            "### [{status}] {title} ({agent})\npath: {artifact_rel}\n\n{body}\n",
            status = p.status.as_str(),
            title = title,
            agent = p.agent,
            artifact_rel = artifact_rel,
            body = body,
        ));
    }
    let path = dir.join("SUMMARY.md");
    if std::fs::write(&path, lines.join("\n")).is_err() {
        return None;
    }
    Some(mission_summary_rel_path(&mode.id))
}

/// Deterministic multi-line report from step results (no model call).
pub fn build_deterministic_completion_summary(mode: &GoalMode) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# Goal complete: {}", mode.goal.trim()));
    if let Some(plan) = &mode.plan {
        if !plan.summary.trim().is_empty() {
            lines.push(format!("Plan: {}", plan.summary.trim()));
        }
    }
    let failed = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Failed)
        .count();
    let skipped = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Skipped)
        .count();
    let done = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Done)
        .count();
    lines.push(format!(
        "Outcome: {done} done, {failed} failed, {skipped} skipped."
    ));
    lines.push(String::new());
    lines.push("## Step results".into());
    for p in &mode.prompts {
        let title = if p.title.is_empty() {
            p.step_id.as_str()
        } else {
            p.title.as_str()
        };
        let summary = nonempty_step_summary(p.summary.as_deref().unwrap_or(""));
        lines.push(format!(
            "- [{status}] {title} ({agent}): {summary}",
            status = p.status.as_str(),
            title = title,
            agent = p.agent,
            summary = summary,
        ));
    }
    if mode.prompts.is_empty() {
        lines.push("- (no step results)".into());
    }
    lines.join("\n")
}

/// Write full step output for wrap-up / UI drill-down. Returns workspace-relative path.
pub fn write_step_artifact(
    workspace: &Path,
    goal_id: &str,
    step_id: &str,
    output: &str,
) -> Option<String> {
    if goal_id.is_empty() || step_id.is_empty() {
        return None;
    }
    let dir = workspace
        .join(".catalyst-code")
        .join("goal-ux")
        .join("artifacts")
        .join(goal_id);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let path = dir.join(format!("{step_id}.md"));
    if std::fs::write(&path, output).is_err() {
        return None;
    }
    Some(format!(
        ".catalyst-code/goal-ux/artifacts/{goal_id}/{step_id}.md"
    ))
}

pub fn transition(mode: &mut GoalMode, to: GoalPhase, message: Option<&str>) -> bool {
    let from = mode.phase.clone();
    if from == to {
        return true;
    }
    if let Err(error) = validate_transition(&from, &to) {
        emit_transition_error(&error);
        return false;
    }
    mode.phase = to.clone();
    mode.touch();
    emit_goal_phase(&from, &to, message);
    emit_goal_state(mode);
    true
}

pub fn fail_goal(mode: &mut GoalMode, err: impl Into<String>) {
    let msg = err.into();
    mode.error = Some(msg.clone());
    mode.deploy_after_turn = false;
    mode.active_run_ids.clear();
    transition(mode, GoalPhase::Failed, Some(msg.as_str()));
}

pub fn cancel_goal(mode: &mut GoalMode, reason: impl Into<String>) {
    let message = reason.into();
    mode.error = Some(message.clone());
    mode.deploy_after_turn = false;
    mode.active_run_ids.clear();
    transition(mode, GoalPhase::Cancelled, Some(message.as_str()));
}

pub fn clear_goal(mode: &mut GoalMode) {
    *mode = GoalMode::default();
    emit(
        &Event::new("goal_state")
            .with("id", json!(""))
            .with("goal", json!(""))
            .with("phase", json!("idle"))
            .with("concurrency", json!(0))
            .with("max_tasks", json!(0))
            .with("allowed_models", json!([]))
            .with("allowed_providers", json!([]))
            .with("auto_deploy", json!(true))
            .with("prompts", json!([]))
            .with("active_run_ids", json!([]))
            .with("version", json!(0))
            .with("error", Value::Null)
            .with("parent_model", json!("")),
    );
}

// ---------------------------------------------------------------------------
// Planning prompt
// ---------------------------------------------------------------------------

pub fn planning_prompt(mode: &GoalMode) -> String {
    // Control Center CEO mode: never prompt the user; use CEO planning template.
    if mode.ceo_mode {
        return crate::goal_ceo::ceo_planning_prompt(mode);
    }
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
            parts.push(format!("planner→{m}"));
        }
        if let Some(m) = &mode.role_models.worker {
            parts.push(format!("worker→{m}"));
        }
        if let Some(m) = &mode.role_models.reviewer {
            parts.push(format!("reviewer→{m}"));
        }
        if parts.is_empty() {
            "Role models: (not pinned — omit step.model or pick from allowlist)".into()
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
        .map(|f| {
            if mode.ceo_mode {
                format!(
                    "

## Revision feedback (autonomous CEO replan / self-review)
{f}
"
                )
            } else {
                format!(
                    "

## Revision feedback from the user
{f}
"
                )
            }
        })
        .unwrap_or_default();
    let gaps_block = if mode.ceo_mode && !mode.remaining_gaps.is_empty() {
        let list = mode
            .remaining_gaps
            .iter()
            .map(|g| format!("- {g}"))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        format!(
            "

## Remaining gaps from last verify (iteration {}/{})
{list}
Focus the new plan on closing these gaps; do not re-do certified work.
",
            mode.iteration,
            mode.max_iterations,
            list = list
        )
    } else {
        String::new()
    };
    let parallelism_guidance = planning_parallelism_guidance(mode);

    let required_action = if mode.ceo_mode {
        concat!(
            "## Required action
",
            "1. Optionally use read/search tools briefly if you need repo context to plan well.
",
            "2. Do NOT call `ask` and do NOT wait for the user — you are the CEO; decide autonomously and document assumptions in the plan summary.
",
            "3. Call the `goal_write_plan` tool EXACTLY ONCE with a complete plan. Do not call it partially.",
        )
    } else {
        concat!(
            "## Required action
",
            "1. Optionally use read/search tools briefly if you need repo context to plan well.
",
            "2. Before goal_write_plan, if high-impact unknowns remain (scope boundaries, success criteria, hard constraints, irreversible/destructive actions), call `ask` ONCE with 1–3 focused questions. Skip low-stakes style/preference questions. If the goal is already clear, go straight to goal_write_plan.
",
            "3. Call the `goal_write_plan` tool EXACTLY ONCE with a complete plan. Do not call it partially.",
        )
    };

    format!(
        r#"You are in GOAL MODE. Your only job this turn is to produce a structured deployment plan for subagents — do not implement the goal yourself.

## Goal
{goal}

## Constraints
- Max concurrent subagents at deploy time: {concurrency}
- Max steps/tasks: {max_tasks}
- Allowed models: {models}
- Allowed providers: {providers}
- {role_line}
- {model_conc}
{revise}{gaps_block}
## Scheduling profile
{parallelism_guidance}

{required_action}

## Plan quality rules
- Prefer scout/context-builder before worker when the codebase area is unknown.
- Prefer independent steps that can run in parallel (same parallel_group, empty depends_on).
- Use depends_on when a step needs a prior step's output.
- Dependent steps receive prior steps' promoted files in worktrees when concurrency>1; still write durable artifacts to files; tasks stay self-contained.
- Each step.task must be a self-contained prompt the subagent can execute without this chat.
- Use agents: scout, researcher, planner, worker, reviewer, context-builder, oracle, delegate (or custom agents).
- Prefer agents planner / worker / reviewer when they match the work so role model pins apply.
- Assign step.model only from the allowed models list (or omit to use role/default).
- Each implementation/review step's `task` must name how to validate that step: the build + test commands to run and what "done" looks like (e.g. `cargo test -p foo`, `go test ./...`, `npm run build && npm test`). The harness appends a generic validation gate to these steps automatically; your `task` makes it concrete.
- Put goal-level acceptance criteria in the plan's `validation` array (checkable later with file:line / build / test evidence), not buried inside a single step.
- Browser tools are available: for goals that touch a web UI, site, or need real end-to-end verification, plan steps that use the deferred `browser` tools and tell the worker to `load_tools` group `browser` (create / navigate / snapshot / click / fill / screenshot). Do not skip browser verification just because it is harder.
- Keep steps ≤ {max_tasks}.

After goal_write_plan succeeds, briefly confirm the plan in one short paragraph. Do not start implementing."#,
        goal = mode.goal,
        concurrency = mode.concurrency,
        max_tasks = mode.max_tasks,
        models = models,
        providers = providers,
        role_line = role_line,
        model_conc = model_conc,
        revise = revise,
        gaps_block = gaps_block,
        parallelism_guidance = parallelism_guidance,
        required_action = required_action,
    )
}

/// Prompt for the CEO pre-deploy self-review turn.
/// Authored in `goal_ceo` (CERTIFY/REVISE + PASS/FAIL aliases + CEO persona).
pub fn build_self_review_prompt(mode: &GoalMode) -> String {
    crate::goal_ceo::self_review_prompt(mode)
}

/// Prompt for the CEO post-deploy verify turn (reads mission SUMMARY.md).
/// Authored in `goal_ceo` (CERTIFIED/REMAINING_GAPS + PASS/FAIL aliases + CEO persona).
pub fn build_verify_prompt(mode: &GoalMode) -> String {
    crate::goal_ceo::verify_prompt(mode)
}

/// Per-step inline budget in the wrap-up prompt (chars). Raised from 1200 so a
/// synthesis/research goal has enough inline signal without forcing the model
/// to re-read every artifact. The total is bounded by WRAPUP_TOTAL_BUDGET.
const WRAPUP_STEP_BUDGET: usize = 3_000;
/// Hard ceiling on the combined inline step text in the wrap-up prompt so a
/// large goal (many steps) cannot blow the parent's context window. Steps past
/// this point get a one-line pointer to their artifact instead of full text.
const WRAPUP_TOTAL_BUDGET: usize = 20_000;

/// Build the prior-step output context to inject into a dependent step's
/// prompt. `depends_on` was previously ordering + skip-on-failure ONLY — a
/// dependent step received its static `task` with zero output from the steps it
/// depended on, so sequential/dependency-driven goals were structurally broken.
///
/// Waves run in topological order, so by the time a dependent step's wave
/// starts its dependencies have settled and their summaries live in
/// `mode.prompts`. We inject each dependency's summary (capped per-dep and
/// total) plus a pointer to its full artifact so the step can read more if the
/// inline digest is insufficient.
///
/// Returns None when there is nothing to inject (no deps, or no dep ran).
pub(crate) fn build_dependency_context(mode: &GoalMode, deps: &[String]) -> Option<String> {
    const PER_DEP_BUDGET: usize = 2_000;
    const TOTAL_BUDGET: usize = 8_000;

    if deps.is_empty() {
        return None;
    }
    let mut sections: Vec<String> = Vec::new();
    let mut total: usize = 0;
    for dep_id in deps {
        let Some(dep) = mode.prompts.iter().find(|p| &p.step_id == dep_id) else {
            continue;
        };
        // Only inject outputs from steps that actually ran. A Pending/Skipped
        // dependency contributes nothing.
        if !matches!(dep.status, DeployStatus::Done | DeployStatus::Failed) {
            continue;
        }
        let title = if dep.title.is_empty() {
            dep.step_id.clone()
        } else {
            dep.title.clone()
        };
        let raw = dep
            .summary
            .as_deref()
            .unwrap_or("(step produced no written output)");
        let summary = truncate_str(raw, PER_DEP_BUDGET);
        let artifact = format!(
            ".catalyst-code/goal-ux/artifacts/{}/{}.md",
            mode.id, dep.step_id
        );
        let section = format!(
            "### Prior step: {title} (id: {dep_id}, status: {status})\n{summary}\n\nFull output: {artifact}",
            status = dep.status.as_str()
        );
        total += section.len();
        sections.push(section);
        if total >= TOTAL_BUDGET {
            break;
        }
    }
    if sections.is_empty() {
        return None;
    }
    let joined = sections.join("\n\n");
    Some(truncate_str(&joined, TOTAL_BUDGET))
}

/// Wrap a step's static `task` with the outputs of its `depends_on` steps.
/// No-op (returns the task unchanged) when there is nothing to inject.
pub(crate) fn inject_dependency_context(task: &str, mode: &GoalMode, deps: &[String]) -> String {
    match build_dependency_context(mode, deps) {
        Some(ctx) => format!(
            "{task}\n\n--- Prior step outputs (from steps this step depends on) ---\n{ctx}\n\n--- End prior step outputs ---\n\nThe outputs above are the results of steps this task depends on. Use them as input for your work. If the inline digest is insufficient, read the full output files referenced above."
        ),
        None => task.to_string(),
    }
}

/// Prompt for the parent wrap-up turn after deploy waves finish.
pub fn build_wrapup_prompt(mode: &GoalMode) -> String {
    let mut steps = String::new();
    let mut running_total: usize = 0;
    for p in &mode.prompts {
        let title = if p.title.is_empty() {
            p.step_id.clone()
        } else {
            p.title.clone()
        };
        let artifact = format!(
            ".catalyst-code/goal-ux/artifacts/{}/{}.md",
            mode.id, p.step_id
        );
        let summary = if running_total >= WRAPUP_TOTAL_BUDGET {
            // Past the total inline budget: list the step with a pointer only,
            // so the wrap-up still enumerates every step without blowing context.
            format!("[output omitted to save context — read full output: {artifact}]")
        } else {
            // Use the RAW summary (not nonempty_step_summary, which pre-caps at
            // 1600) so WRAPUP_STEP_BUDGET (3000) is the real inline budget.
            let raw = p.summary.as_deref().unwrap_or("");
            let raw = if raw.trim().is_empty() {
                "(step finished with no written summary)"
            } else {
                raw
            };
            let capped = truncate_str(raw, WRAPUP_STEP_BUDGET);
            running_total += capped.len();
            capped
        };
        steps.push_str(&format!(
            "- [{status}] {title} ({agent}): {summary}
  full output: {artifact}
",
            status = p.status.as_str(),
            title = title,
            agent = p.agent,
            summary = summary,
            artifact = artifact,
        ));
    }
    if steps.is_empty() {
        steps.push_str(
            "(no step results)
",
        );
    }
    let plan_summary = mode
        .plan
        .as_ref()
        .map(|p| p.summary.as_str())
        .unwrap_or("(none)");
    let failed = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Failed)
        .count();
    let skipped = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Skipped)
        .count();
    let outcome = if failed == 0 && skipped == 0 {
        "All steps completed successfully.".to_string()
    } else if failed == 0 {
        format!("All runnable steps completed; {skipped} skipped.")
    } else {
        format!("{failed} step(s) failed; {skipped} skipped.")
    };

    format!(
        r#"GOAL MODE — deployment finished. Your only job this turn is to report results to the user.

## Goal
{goal}

## Plan summary
{plan_summary}

## Deploy outcome
{outcome}

## Step results
{steps}
## Instructions
1. Write a clear completion summary for the user: what was done, what failed (if anything), and notable files/changes from the step results. Read full step artifacts under `.catalyst-code/goal-ux/artifacts/` when a truncated summary is insufficient.
2. If something failed, suggest the smallest next action — do not silently re-plan or re-deploy.
3. Do not call goal_write_plan. Do not spawn more goal workers unless the user asks.
4. Call `finish` when the summary is done."#,
        goal = mode.goal,
        plan_summary = plan_summary,
        outcome = outcome,
        steps = steps,
    )
}

/// Minimum wrap-up assistant chars that count as a "rich" model summary.
/// Below this we still emit the deterministic checklist so Done is never blank.
pub const RICH_WRAPUP_MIN_CHARS: usize = 200;

/// Mark a synthesizing goal as Done (or Failed if the wrap-up was aborted).
///
/// Emits `goal_completion_summary` with a deterministic step report when wrap-up
/// was skipped or empty/short. When a rich model wrap-up already streamed
/// (`wrapup_chars >= RICH_WRAPUP_MIN_CHARS`), skips the deterministic card to
/// avoid dual Done cards (H2).
pub fn finish_synthesis(mode: &mut GoalMode, cancelled: bool) {
    finish_synthesis_with_wrapup(mode, cancelled, None);
}

/// Like [`finish_synthesis`], but `wrapup_chars` controls deterministic emit:
/// - `None` — wrap-up skipped → always emit deterministic summary
/// - `Some(n)` — emit only when `n < RICH_WRAPUP_MIN_CHARS`
pub fn finish_synthesis_with_wrapup(
    mode: &mut GoalMode,
    cancelled: bool,
    wrapup_chars: Option<usize>,
) {
    if mode.phase != GoalPhase::Synthesizing {
        return;
    }
    if cancelled {
        cancel_goal(mode, "goal wrap-up aborted");
        return;
    }
    let failed: Vec<&str> = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Failed)
        .map(|p| p.step_id.as_str())
        .collect();
    let skipped: Vec<&str> = mode
        .prompts
        .iter()
        .filter(|p| p.status == DeployStatus::Skipped)
        .map(|p| p.step_id.as_str())
        .collect();
    let msg = if failed.is_empty() {
        "goal complete".to_string()
    } else {
        format!(
            "goal complete with {} failed step(s){}",
            failed.len(),
            if skipped.is_empty() {
                String::new()
            } else {
                format!(", {} skipped", skipped.len())
            }
        )
    };
    // Emit before transition so UIs paint the lasting synthesis card as the
    // goal moves to Done (closes the dark gap after the last worker).
    let emit_deterministic = match wrapup_chars {
        None => true,
        Some(n) => n < RICH_WRAPUP_MIN_CHARS,
    };
    if emit_deterministic {
        let summary = build_deterministic_completion_summary(mode);
        emit_goal_completion_summary(&summary);
    }
    transition(mode, GoalPhase::Done, Some(&msg));
}

/// Apply a self-review verdict. Returns what the caller should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// Plan accepted — caller should spawn deploy.
    Deploy,
    /// Plan rejected with budget remaining — caller should start a planning turn.
    Replan,
    /// Budget exhausted or cancelled — goal already Failed.
    Failed,
}

/// Record self-review verdict while phase == Reviewing.
pub fn finish_reviewing(
    mode: &mut GoalMode,
    cancelled: bool,
    assistant_text: &str,
) -> ReviewOutcome {
    if mode.phase != GoalPhase::Reviewing {
        return ReviewOutcome::Failed;
    }
    if cancelled {
        cancel_goal(mode, "plan self-review aborted");
        return ReviewOutcome::Failed;
    }
    let (ok, summary, gaps) = parse_ceo_verdict(assistant_text);
    let verdict = GoalVerdict {
        ok,
        summary: summary.clone(),
        evidence_paths: Vec::new(),
        at_ms: now_ms() as u64,
    };
    mode.review_verdict = Some(verdict.clone());
    mode.self_review_feedback = if ok {
        None
    } else {
        Some(if gaps.is_empty() {
            summary.clone()
        } else {
            gaps.join("\n")
        })
    };
    mode.touch();
    emit_goal_review_verdict(mode, &verdict);
    emit_goal_state(mode);
    if ok {
        transition(
            mode,
            GoalPhase::PlanReady,
            Some("self-review passed — deploying"),
        );
        ReviewOutcome::Deploy
    } else if mode.plan_revision < mode.max_plan_revisions {
        mode.plan_revision = mode.plan_revision.saturating_add(1);
        let feedback = mode
            .self_review_feedback
            .clone()
            .unwrap_or_else(|| summary.clone());
        mode.revise_feedback = Some(feedback);
        mode.plan = None;
        mode.prompts.clear();
        mode.deploy_after_turn = false;
        emit_goal_iteration(mode);
        transition(
            mode,
            GoalPhase::Planning,
            Some("self-review failed — revising plan"),
        );
        ReviewOutcome::Replan
    } else {
        fail_goal(
            mode,
            format!(
                "plan self-review budget exhausted ({}/{} revisions)",
                mode.plan_revision, mode.max_plan_revisions
            ),
        );
        ReviewOutcome::Failed
    }
}

/// Apply a verify verdict. Returns what the caller should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Goal certified — already transitioned to Done.
    Certified,
    /// Gaps remain with budget — caller should start a planning (replan) turn.
    Replan,
    /// Budget exhausted or cancelled — goal already Failed.
    Failed,
}

/// Record verify verdict while phase == Verifying.
pub fn finish_verifying(
    mode: &mut GoalMode,
    cancelled: bool,
    assistant_text: &str,
) -> VerifyOutcome {
    if mode.phase != GoalPhase::Verifying {
        return VerifyOutcome::Failed;
    }
    if cancelled {
        cancel_goal(mode, "goal verify aborted");
        return VerifyOutcome::Failed;
    }
    let (ok, summary, mut gaps) = parse_ceo_verdict(assistant_text);
    // Never leave Verifying without either certifying or a non-empty gap list
    // (empty FAIL would replan with blank revise_feedback and look "stuck").
    if !ok && gaps.is_empty() {
        let fallback = summary.trim();
        gaps.push(if fallback.is_empty() {
            "verify failed without enumerated gaps".to_string()
        } else {
            truncate_str(fallback, 500)
        });
    }
    let evidence = vec![mission_summary_rel_path(&mode.id)];
    let verdict = GoalVerdict {
        ok,
        summary: summary.clone(),
        evidence_paths: evidence,
        at_ms: now_ms() as u64,
    };
    mode.verify_verdict = Some(verdict.clone());
    mode.remaining_gaps = gaps.clone();
    mode.touch();
    emit_goal_verify_verdict(mode, &verdict, &gaps);
    emit_goal_state(mode);
    if ok {
        mode.certified = true;
        let completion = if summary.trim().is_empty() {
            build_deterministic_completion_summary(mode)
        } else {
            summary.clone()
        };
        emit_goal_certified(mode, &completion);
        emit_goal_completion_summary(&completion);
        transition(mode, GoalPhase::Done, Some("goal certified"));
        VerifyOutcome::Certified
    } else if mode.iteration < mode.max_iterations {
        mode.iteration = mode.iteration.saturating_add(1);
        let feedback = format!(
            "Verify failed (iteration {}/{}). Remaining gaps:\n{}",
            mode.iteration,
            mode.max_iterations,
            gaps.iter()
                .map(|g| format!("- {g}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        mode.revise_feedback = Some(feedback);
        mode.plan = None;
        mode.prompts.clear();
        mode.deploy_after_turn = false;
        mode.certified = false;
        emit_goal_iteration(mode);
        transition(
            mode,
            GoalPhase::Replanning,
            Some("verify failed — replanning gaps"),
        );
        transition(
            mode,
            GoalPhase::Planning,
            Some("replan: writing delta plan"),
        );
        VerifyOutcome::Replan
    } else {
        fail_goal(
            mode,
            format!(
                "verify budget exhausted after {} iteration(s); last gaps: {}",
                mode.max_iterations,
                gaps.join("; ")
            ),
        );
        VerifyOutcome::Failed
    }
}

// ---------------------------------------------------------------------------
// Deploy (via subagent::execute parallel waves)
// ---------------------------------------------------------------------------

async fn persist_goal_activity(
    st: &State,
    goal_id: &str,
    state: crate::session::RunState,
    detail: Option<&str>,
) {
    let path = st.cfg.read().await.session_file.clone();
    if let Some(path) = path {
        crate::session::append_activity_state(
            &path,
            st.runtime.session_id().as_str(),
            goal_id,
            "goal",
            crate::runtime::current_run_id().as_deref(),
            None,
            state,
            detail,
        );
    }
}

async fn await_goal_worker<F>(
    future: F,
    cancellation: &CancellationToken,
    timeout: std::time::Duration,
) -> Outcome
where
    F: std::future::Future<Output = Outcome>,
{
    tokio::pin!(future);
    match tokio::time::timeout(timeout, &mut future).await {
        Ok(outcome) => outcome,
        Err(_) => {
            cancellation.cancel();
            // Give the owned subagent loop a bounded grace period to finalize
            // its run record, mailbox, resources, and worktree after cancellation.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut future).await;
            Outcome::err(format!(
                "goal worker timed out after {}s and was cancelled",
                timeout.as_secs()
            ))
        }
    }
}

fn accepts_goal_worker_result(mode: &GoalMode, goal_id: &str, cancelled: bool) -> bool {
    !cancelled && mode.id == goal_id && mode.is_active()
}

/// Run deploy for the current plan. Caller must hold no goal lock across await.
///
/// On success, leaves the goal in [`GoalPhase::Synthesizing`] (classic) or
/// [`GoalPhase::Verifying`] (CEO mode) so the caller can start the follow-up
/// parent turn. Returns `true` when that follow-up should run.
pub async fn deploy_goal(
    st: Arc<State>,
    client: reqwest::Client,
    cancel: CancellationToken,
) -> bool {
    // Snapshot config for deploy.
    let (parent_model, global_conc, model_conc_map, prompts_snapshot, goal_id, workspace, ceo_mode) = {
        let mode = st.goal.lock().await;
        if mode.plan.is_none() || mode.prompts.is_empty() {
            return false;
        }
        let workspace = st.cfg.read().await.workspace.clone();
        (
            mode.parent_model.clone(),
            mode.concurrency,
            mode.model_concurrency.clone(),
            mode.prompts.clone(),
            mode.id.clone(),
            workspace,
            mode.ceo_mode,
        )
    };

    {
        let mut mode = st.goal.lock().await;
        if mode.id != goal_id {
            return false;
        }
        mode.deploy_after_turn = false;
        transition(
            &mut mode,
            GoalPhase::Deploying,
            Some("deploying plan steps"),
        );
    }
    persist_goal_activity(
        &st,
        &goal_id,
        crate::session::RunState::Started,
        Some("deploying"),
    )
    .await;

    // Build step dependency map from plan.
    let steps: Vec<GoalStep> = {
        let mode = st.goal.lock().await;
        mode.plan
            .as_ref()
            .map(|p| p.steps.clone())
            .unwrap_or_default()
    };
    let waves = match topo_waves(&steps) {
        Ok(w) => w,
        Err(e) => {
            let mut mode = st.goal.lock().await;
            fail_goal(&mut mode, format!("deploy ordering failed: {e}"));
            drop(mode);
            persist_goal_activity(
                &st,
                &goal_id,
                crate::session::RunState::Failed,
                Some("deploy ordering failed"),
            )
            .await;
            return false;
        }
    };

    let prompt_by_id: HashMap<String, DeployPrompt> = prompts_snapshot
        .into_iter()
        .map(|p| (p.step_id.clone(), p))
        .collect();

    // Global + per-model semaphores for concurrent deploy slots.
    use std::sync::Arc as StdArc;
    use tokio::sync::Semaphore;
    let global_sem = StdArc::new(Semaphore::new(global_conc.max(1) as usize));
    let mut model_sems: HashMap<String, StdArc<Semaphore>> = HashMap::new();
    for (m, cap) in &model_conc_map {
        let n = (*cap).clamp(1, global_conc.max(1)) as usize;
        model_sems.insert(m.clone(), StdArc::new(Semaphore::new(n)));
    }

    let mut any_failed = false;
    // Track failed step IDs so we can skip their dependents in later waves
    // instead of fail-fast aborting the entire deploy on a single failure.
    let mut failed_steps: HashSet<String> = HashSet::new();
    // Map step_id → depends_on for skip-on-failure logic.
    let deps_by_id: HashMap<String, Vec<String>> = steps
        .iter()
        .map(|s| (s.id.clone(), s.depends_on.clone()))
        .collect();

    let wave_count = waves.len();
    for (wave_idx, wave) in waves.into_iter().enumerate() {
        if cancel.is_cancelled() {
            let mut mode = st.goal.lock().await;
            cancel_goal(&mut mode, "goal cancelled");
            return false;
        }

        // Skip cascade first while steps are still Pending (never flash
        // Running→Skipped). Transitive: if B was skipped because A failed,
        // anything depending on B must also be skipped.
        let mut wave_prompts: Vec<DeployPrompt> = Vec::new();
        for id in &wave {
            if let Some(deps) = deps_by_id.get(id) {
                if deps.iter().any(|d| failed_steps.contains(d)) {
                    let mut mode = st.goal.lock().await;
                    let skip_summary =
                        "skipped: a dependency step failed or was skipped".to_string();
                    let (title, agent, run_id) =
                        if let Some(p) = mode.prompts.iter_mut().find(|p| &p.step_id == id) {
                            p.status = DeployStatus::Skipped;
                            p.summary = Some(skip_summary.clone());
                            let title = if p.title.is_empty() {
                                p.step_id.clone()
                            } else {
                                p.title.clone()
                            };
                            (title, p.agent.clone(), p.run_id.clone())
                        } else {
                            (id.clone(), String::new(), None)
                        };
                    mode.touch();
                    emit_goal_state(&mode);
                    emit_goal_step_complete(
                        id,
                        &title,
                        &agent,
                        false,
                        "skipped",
                        &skip_summary,
                        run_id.as_deref(),
                    );
                    sync_work_state_from_prompts(&st, &mode).await;
                    // Propagate: mark this step as unavailable so its own
                    // dependents are skipped in later waves too.
                    failed_steps.insert(id.clone());
                    continue;
                }
            }
            if let Some(p) = prompt_by_id.get(id) {
                wave_prompts.push(p.clone());
            }
        }

        // Emit phase Running / wave progress after skip handling. Steps stay
        // Pending until they acquire a concurrency slot (marked Running then).
        {
            let mut mode = st.goal.lock().await;
            let from = mode.phase.clone();
            let step_count = mode.prompts.len();
            let done_count = mode
                .prompts
                .iter()
                .filter(|p| {
                    matches!(
                        p.status,
                        DeployStatus::Done | DeployStatus::Failed | DeployStatus::Skipped
                    )
                })
                .count();
            if from != GoalPhase::Running && validate_transition(&from, &GoalPhase::Running).is_ok()
            {
                mode.phase = GoalPhase::Running;
            }
            mode.touch();
            let msg = format!("wave {} — dispatching steps", wave_idx + 1);
            emit_goal_phase_progress(
                &from,
                &GoalPhase::Running,
                Some(&msg),
                Some(wave_idx + 1),
                Some(step_count),
                Some(done_count),
            );
            emit_goal_state(&mode);
            sync_work_state_from_prompts(&st, &mode).await;
        }

        if wave_prompts.is_empty() {
            continue;
        }

        // Run each step with global + per-model concurrency caps.
        // Track step_id alongside each handle so a JoinError (panic) can still
        // mark the right step as failed.
        let mut handles: Vec<(
            String,
            tokio::task::JoinHandle<(String, bool, String, Option<String>)>,
        )> = Vec::new();
        for p in wave_prompts {
            if cancel.is_cancelled() {
                break;
            }
            let model_key = p.model.clone().unwrap_or_else(|| parent_model.clone());
            let g_sem = global_sem.clone();
            let m_sem = model_sems
                .entry(model_key.clone())
                .or_insert_with(|| StdArc::new(Semaphore::new(global_conc.max(1) as usize)))
                .clone();
            let st_c = st.clone();
            let client_c = client.clone();
            let cancel_c = cancel.clone();
            let parent = parent_model.clone();
            let step_id = p.step_id.clone();
            let step_id_outer = step_id.clone();
            // Pre-allocate run_id so goal_state / step cards can deep-link before
            // execute returns (C2).
            let run_id = crate::subagent::allocate_run_id();
            {
                let mut mode = st.goal.lock().await;
                if let Some(prompt) = mode.prompts.iter_mut().find(|x| x.step_id == step_id) {
                    prompt.run_id = Some(run_id.clone());
                }
                if !mode.active_run_ids.iter().any(|r| r == &run_id) {
                    mode.active_run_ids.push(run_id.clone());
                }
                mode.touch();
                emit_goal_state(&mode);
            }
            let run_id_task = run_id.clone();
            let parent_goal_id = goal_id.clone();
            // Inject prior-step outputs into this step's prompt so a dependent
            // step receives its dependencies' results (depends_on was
            // previously ordering + skip-on-failure ONLY). Computed here, before
            // the spawn, because the dependency summaries live in mode.prompts
            // and the topo wave guarantees deps have settled by now.
            let augmented_task = {
                let mode = st.goal.lock().await;
                let deps = deps_by_id.get(&p.step_id).cloned().unwrap_or_default();
                inject_dependency_context(&p.task, &mode, &deps)
            };
            handles.push((
                step_id_outer,
                tokio::spawn(async move {
                    let _g = match g_sem.acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => {
                            return (
                                step_id,
                                false,
                                "global concurrency semaphore closed".into(),
                                Some(run_id_task),
                            );
                        }
                    };
                    let _m = match m_sem.acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => {
                            return (
                                step_id,
                                false,
                                "model concurrency semaphore closed".into(),
                                Some(run_id_task),
                            );
                        }
                    };
                    if cancel_c.is_cancelled() {
                        return (step_id, false, "cancelled".into(), Some(run_id_task));
                    }
                    // Mark Running only after acquiring a concurrency slot so
                    // queued steps stay Pending (no Running→Skipped flash).
                    {
                        let mut mode = st_c.goal.lock().await;
                        if !accepts_goal_worker_result(&mode, &parent_goal_id, false) {
                            st_c.runtime.note_stale_result();
                            return (
                                step_id,
                                false,
                                "stale goal worker discarded before execution".into(),
                                Some(run_id_task),
                            );
                        }
                        if let Some(prompt) = mode.prompts.iter_mut().find(|x| x.step_id == step_id)
                        {
                            prompt.status = DeployStatus::Running;
                        }
                        mode.touch();
                        emit_goal_state(&mode);
                        sync_work_state_from_prompts(&st_c, &mode).await;
                    }
                    let mut args = json!({
                        "agent": p.agent,
                        "task": augmented_task,
                        "context": "fresh",
                        "run_id": run_id_task,
                        "_parent_run_id": parent_goal_id,
                    });
                    if let Some(m) = &p.model {
                        args["model"] = json!(m);
                    }
                    // Isolate only mutating agents when concurrency > 1. Read-
                    // focused reviewers/scouts share the main tree (disjoint
                    // artifact paths) — avoiding concurrent git worktree races.
                    let workspace = st_c.cfg.read().await.workspace.clone();
                    if global_conc > 1
                        && goal_step_needs_worktree(&p.agent)
                        && crate::worktree::is_git_repo(&workspace)
                    {
                        args["worktree"] = json!(true);
                    }
                    let provider = st_c
                        .resolve_provider_for_model(p.model.as_deref().unwrap_or(&parent))
                        .await;
                    let worker_timeout = {
                        let configured = st_c.cfg.read().await.idle_timeout_secs;
                        std::time::Duration::from_secs(configured.saturating_mul(5).max(300))
                    };
                    let worker_cancel = cancel_c.child_token();
                    // Single-agent execute (worktree handled inside execute when set).
                    // Previously wrapped every step as a one-item parallel batch
                    // whenever concurrency > 1, which raced git worktree add.
                    let outcome = await_goal_worker(
                        crate::subagent::execute(
                            st_c,
                            client_c,
                            provider,
                            parent,
                            args,
                            worker_cancel.clone(),
                            0,
                        ),
                        &worker_cancel,
                        worker_timeout,
                    )
                    .await;
                    (step_id, outcome.ok, outcome.output, Some(run_id_task))
                }),
            ));
        }

        let mut wave_failed = false;
        for (step_id, h) in handles {
            match h.await {
                Ok((_id, ok, output, joined_run_id)) => {
                    let mut mode = st.goal.lock().await;
                    if !accepts_goal_worker_result(&mode, &goal_id, cancel.is_cancelled()) {
                        st.runtime.note_stale_result();
                        continue;
                    }
                    let summary = nonempty_step_summary(&output);
                    let _ = write_step_artifact(&workspace, &goal_id, &step_id, &output);
                    let (title, agent, run_id, status, clear_rid) =
                        if let Some(p) = mode.prompts.iter_mut().find(|p| p.step_id == step_id) {
                            if ok {
                                p.status = DeployStatus::Done;
                            } else {
                                p.status = DeployStatus::Failed;
                                wave_failed = true;
                                any_failed = true;
                                failed_steps.insert(step_id.clone());
                            }
                            p.summary = Some(summary.clone());
                            if p.run_id.is_none() {
                                p.run_id = joined_run_id.clone();
                            }
                            let clear_rid = p.run_id.clone();
                            let title = if p.title.is_empty() {
                                p.step_id.clone()
                            } else {
                                p.title.clone()
                            };
                            let agent = p.agent.clone();
                            let status = p.status.as_str();
                            let run_id = p.run_id.clone();
                            (title, agent, run_id, status, clear_rid)
                        } else {
                            if !ok {
                                wave_failed = true;
                                any_failed = true;
                                failed_steps.insert(step_id.clone());
                            }
                            (
                                step_id.clone(),
                                String::new(),
                                joined_run_id.clone(),
                                if ok { "done" } else { "failed" },
                                joined_run_id,
                            )
                        };
                    if let Some(ref rid) = clear_rid {
                        mode.active_run_ids.retain(|r| r != rid);
                    }
                    mode.touch();
                    emit_goal_state(&mode);
                    emit_goal_step_complete(
                        &step_id,
                        &title,
                        &agent,
                        ok,
                        status,
                        &summary,
                        run_id.as_deref(),
                    );
                    sync_work_state_from_prompts(&st, &mode).await;
                }
                Err(e) => {
                    any_failed = true;
                    wave_failed = true;
                    failed_steps.insert(step_id.clone());
                    let mut mode = st.goal.lock().await;
                    if !accepts_goal_worker_result(&mode, &goal_id, cancel.is_cancelled()) {
                        st.runtime.note_stale_result();
                        continue;
                    }
                    let summary = format!("task join error: {e}");
                    let _ = write_step_artifact(&workspace, &goal_id, &step_id, &summary);
                    let (title, agent, run_id, clear_rid) =
                        if let Some(p) = mode.prompts.iter_mut().find(|p| p.step_id == step_id) {
                            p.status = DeployStatus::Failed;
                            p.summary = Some(nonempty_step_summary(&summary));
                            let clear_rid = p.run_id.clone();
                            let title = if p.title.is_empty() {
                                p.step_id.clone()
                            } else {
                                p.title.clone()
                            };
                            let agent = p.agent.clone();
                            let run_id = p.run_id.clone();
                            (title, agent, run_id, clear_rid)
                        } else {
                            (step_id.clone(), String::new(), None, None)
                        };
                    if let Some(ref rid) = clear_rid {
                        mode.active_run_ids.retain(|r| r != rid);
                    }
                    mode.touch();
                    emit_goal_state(&mode);
                    emit_goal_step_complete(
                        &step_id,
                        &title,
                        &agent,
                        false,
                        "failed",
                        &summary,
                        run_id.as_deref(),
                    );
                    sync_work_state_from_prompts(&st, &mode).await;
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!("goal deploy task join error: {e}")),
                    ));
                }
            }
        }

        // Do NOT fail-fast: continue to the next wave so independent steps
        // still run. Steps whose depends_on includes a failed step are
        // skipped at the top of the next wave iteration.

        // The plan's validation criteria describe the completed goal, not an
        // intermediate dependency wave. Running them after (for example)
        // Phase A can only produce a false failure for work intentionally
        // scheduled in later phases, which then poisons the dependency graph.
        // Validate once, after the final wave has run.
        // CEO mode: skip terminal-wave reviewer — Verifying phase is the
        // authoritative certify step (avoids double-fail / double-emit).
        if !ceo_mode
            && !cancel.is_cancelled()
            && should_validate_goal_after_wave(wave_idx, wave_count)
        {
            let (validation, validation_parent_id) = {
                let mode = st.goal.lock().await;
                (
                    mode.plan
                        .as_ref()
                        .map(|p| p.validation.clone())
                        .unwrap_or_default(),
                    mode.id.clone(),
                )
            };
            if !validation.is_empty() && !wave_failed {
                let checks = validation.join("\n- ");
                let provider = st.resolve_provider_for_model(&parent_model).await;
                let args = json!({
                    "agent": "reviewer",
                    "task": format!(
                        "Verify the completed goal against these checks:\n- {checks}\n\nRun diagnostics if helpful. Reply VERDICT: PASS or VERDICT: FAIL with reasons."
                    ),
                    "context": "fresh",
                    "_parent_run_id": validation_parent_id,
                });
                let outcome = crate::subagent::execute(
                    st.clone(),
                    client.clone(),
                    provider,
                    parent_model.clone(),
                    args,
                    cancel.clone(),
                    0,
                )
                .await;
                let pass = outcome
                    .output
                    .to_ascii_uppercase()
                    .contains("VERDICT: PASS");
                emit(
                    &Event::new("goal_step_verdict")
                        .with("ok", json!(outcome.ok && pass))
                        .with("provider_ok", json!(outcome.ok))
                        .with("output", json!(truncate_str(&outcome.output, 800))),
                );
                if !outcome.ok {
                    // A provider/transport failure says nothing about whether
                    // the completed work passed validation. Surface it for the
                    // wrap-up without rewriting successful step state.
                    any_failed = true;
                    emit(&Event::new("error").with(
                        "message",
                        json!(format!(
                            "goal validation could not run: {}",
                            truncate_str(&outcome.output, 800)
                        )),
                    ));
                } else if !pass {
                    // An explicit completed-goal validation failure belongs to
                    // the terminal wave. There are no later dependencies to
                    // cascade-skip at this point.
                    let mut mode = st.goal.lock().await;
                    let mut completed: Vec<(String, String, String, Option<String>, String)> =
                        Vec::new();
                    for id in &wave {
                        if let Some(p) = mode.prompts.iter_mut().find(|p| &p.step_id == id) {
                            if p.status == DeployStatus::Done {
                                p.status = DeployStatus::Failed;
                                let summary = nonempty_step_summary(&format!(
                                    "verifier failed: {}",
                                    truncate_str(&outcome.output, 800)
                                ));
                                p.summary = Some(summary.clone());
                                failed_steps.insert(id.clone());
                                any_failed = true;
                                let title = if p.title.is_empty() {
                                    p.step_id.clone()
                                } else {
                                    p.title.clone()
                                };
                                completed.push((
                                    id.clone(),
                                    title,
                                    p.agent.clone(),
                                    p.run_id.clone(),
                                    summary,
                                ));
                            }
                        }
                    }
                    mode.touch();
                    emit_goal_state(&mode);
                    for (sid, title, agent, run_id, summary) in completed {
                        emit_goal_step_complete(
                            &sid,
                            &title,
                            &agent,
                            false,
                            "failed",
                            &summary,
                            run_id.as_deref(),
                        );
                    }
                    sync_work_state_from_prompts(&st, &mode).await;
                }
            }
        }
        let _ = wave_failed;
    }

    {
        let mut mode = st.goal.lock().await;
        if mode.id != goal_id {
            return false;
        }
        if cancel.is_cancelled() {
            cancel_goal(&mut mode, "goal cancelled");
            sync_work_state_from_prompts(&st, &mode).await;
            drop(mode);
            persist_goal_activity(
                &st,
                &goal_id,
                crate::session::RunState::Cancelled,
                Some("goal cancelled during deploy"),
            )
            .await;
            return false;
        }
        // All waves processed. Even if some steps failed (and their
        // dependents were skipped), the deploy ran to completion.
        // CEO mode → Verifying (artifact-rich); classic → Synthesizing wrap-up.
        let failed_n = mode
            .prompts
            .iter()
            .filter(|p| p.status == DeployStatus::Failed)
            .count();
        let skipped_n = mode
            .prompts
            .iter()
            .filter(|p| p.status == DeployStatus::Skipped)
            .count();
        let suffix = if failed_n == 0 {
            String::new()
        } else {
            format!(
                " with {failed_n} failed step(s){}",
                if skipped_n == 0 {
                    String::new()
                } else {
                    format!(", {skipped_n} skipped")
                }
            )
        };
        if mode.ceo_mode {
            let summary_path = write_mission_summary(&workspace, &mode);
            let msg = format!(
                "workers finished{suffix} — verifying against goal{}",
                summary_path
                    .as_ref()
                    .map(|p| format!(" (evidence: {p})"))
                    .unwrap_or_default()
            );
            transition(&mut mode, GoalPhase::Verifying, Some(&msg));
        } else {
            let msg = format!("workers finished{suffix} — summarizing");
            transition(&mut mode, GoalPhase::Synthesizing, Some(&msg));
        }
        sync_work_state_from_prompts(&st, &mode).await;
    }

    emit(&Event::new("info").with(
        "message",
        json!(if any_failed {
            "Goal deploy complete — some steps failed; continuing to follow-up turn…"
        } else {
            "Goal deploy complete — starting follow-up turn…"
        }),
    ));
    persist_goal_activity(
        &st,
        &goal_id,
        crate::session::RunState::Completed,
        Some("deploy workers finished"),
    )
    .await;
    true
}

/// Mirror deploy prompt statuses into WorkState done/in_progress/next.
pub async fn sync_work_state_from_prompts(st: &Arc<State>, mode: &GoalMode) {
    let mut done = Vec::new();
    let mut in_progress = Vec::new();
    let mut next = Vec::new();
    for p in &mode.prompts {
        let label = if p.title.is_empty() {
            format!("{} ({})", p.step_id, p.agent)
        } else {
            format!("{} — {}", p.title, p.agent)
        };
        match p.status {
            DeployStatus::Done => done.push(label),
            DeployStatus::Running => in_progress.push(label),
            DeployStatus::Failed => done.push(format!("FAILED: {label}")),
            DeployStatus::Skipped => done.push(format!("skipped: {label}")),
            DeployStatus::Pending => next.push(label),
        }
    }
    let mut ws = st.work_state.lock().await;
    if !mode.goal.is_empty() {
        ws.goal = truncate_str(&mode.goal, 240);
    }
    ws.done = done;
    ws.in_progress = in_progress;
    ws.next = next;
    ws.last_activity = format!("goal:{}", mode.phase.as_str());
    ws.touch();
    let version = ws.version;
    let goal = ws.goal.clone();
    let done = ws.done.clone();
    let in_progress = ws.in_progress.clone();
    let next = ws.next.clone();
    let recent_files = ws.recent_files.clone();
    let last_activity = ws.last_activity.clone();
    drop(ws);
    emit(
        &Event::new("work_state")
            .with("version", json!(version))
            .with("goal", json!(goal))
            .with("done", json!(done))
            .with("in_progress", json!(in_progress))
            .with("next", json!(next))
            .with("recent_files", json!(recent_files))
            .with("last_activity", json!(last_activity)),
    );
}

/// Handle goal_write_plan tool call against live GoalMode.
pub async fn handle_goal_write_plan(st: &Arc<State>, args: &Value) -> Outcome {
    let workspace = st.cfg.read().await.workspace.clone();
    let cfg = st.cfg.read().await.clone();
    let agents = crate::subagent::discover_agents(&workspace, &cfg.subagents);
    let known: HashSet<String> = agents.into_iter().map(|a| a.name).collect();

    let (msg, titles) = {
        let mut mode = st.goal.lock().await;
        if mode.phase != GoalPhase::Planning {
            return Outcome::err(format!(
                "goal_write_plan only valid during planning (phase={})",
                mode.phase.as_str()
            ));
        }
        match apply_plan(&mut mode, args, &known) {
            Ok(()) => {
                emit_goal_plan(&mode);
                emit_goal_state(&mode);
                let msg = if mode.auto_deploy {
                    format!(
                        "Plan accepted ({} steps). Deploy will start after this planning turn.",
                        mode.prompts.len()
                    )
                } else {
                    format!(
                        "Plan accepted ({} steps). Waiting for approve_goal_plan.",
                        mode.prompts.len()
                    )
                };
                let titles: Vec<String> = mode
                    .prompts
                    .iter()
                    .map(|p| {
                        if p.title.is_empty() {
                            p.step_id.clone()
                        } else {
                            p.title.clone()
                        }
                    })
                    .collect();
                (msg, titles)
            }
            Err(e) => return Outcome::err(e),
        }
    };
    // Refresh work-state next from plan titles (goal lock released).
    {
        let mut ws = st.work_state.lock().await;
        ws.next = titles;
        ws.in_progress.clear();
        ws.done.clear();
        ws.last_activity = "goal:plan_ready".into();
        ws.touch();
        let version = ws.version;
        let goal = ws.goal.clone();
        let recent_files = ws.recent_files.clone();
        let last_activity = ws.last_activity.clone();
        let next = ws.next.clone();
        drop(ws);
        emit(
            &Event::new("work_state")
                .with("version", json!(version))
                .with("goal", json!(goal))
                .with("done", json!([]))
                .with("in_progress", json!([]))
                .with("next", json!(next))
                .with("recent_files", json!(recent_files))
                .with("last_activity", json!(last_activity)),
        );
    }
    Outcome::ok(msg)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn truncate_str(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{t}…")
    } else {
        t
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_mode() -> GoalMode {
        GoalMode {
            id: "g1".into(),
            goal: "ship feature X".into(),
            phase: GoalPhase::Planning,
            concurrency: 2,
            max_tasks: 4,
            allowed_models: vec!["m1".into()],
            allowed_providers: vec![],
            auto_deploy: true,
            parent_model: "m1".into(),
            reasoning_effort: "medium".into(),
            ..GoalMode::default()
        }
    }

    #[test]
    fn is_active_during_planning_not_done() {
        let mut m = base_mode();
        assert!(m.is_active()); // Planning
        m.phase = GoalPhase::PlanReady;
        assert!(m.is_active());
        m.phase = GoalPhase::Deploying;
        assert!(m.is_active());
        m.phase = GoalPhase::Running;
        assert!(m.is_active());
        m.phase = GoalPhase::Synthesizing;
        assert!(m.is_active());
        m.phase = GoalPhase::Reviewing;
        assert!(m.is_active());
        m.phase = GoalPhase::Verifying;
        assert!(m.is_active());
        m.phase = GoalPhase::Replanning;
        assert!(m.is_active());
        m.phase = GoalPhase::Done;
        assert!(!m.is_active());
        m.phase = GoalPhase::Failed;
        assert!(!m.is_active());
        m.phase = GoalPhase::Cancelled;
        assert!(!m.is_active());
        m.phase = GoalPhase::Idle;
        assert!(!m.is_active());
    }

    #[test]
    fn goal_transition_table_rejects_invalid_jump_with_structured_error() {
        let mut mode = base_mode();
        mode.phase = GoalPhase::Planning;
        let error = validate_transition(&mode.phase, &GoalPhase::Done).unwrap_err();
        assert_eq!(error.code, "invalid_goal_transition");
        assert_eq!(error.from, GoalPhase::Planning);
        assert_eq!(error.to, GoalPhase::Done);

        crate::protocol::begin_emit_capture();
        assert!(!transition(
            &mut mode,
            GoalPhase::Done,
            Some("invalid test jump")
        ));
        let captured = crate::protocol::end_emit_capture();
        assert_eq!(
            mode.phase,
            GoalPhase::Planning,
            "invalid jump mutated state"
        );
        let event = captured
            .iter()
            .find(|(kind, _)| kind == "error")
            .map(|(_, data)| data)
            .expect("structured error event");
        assert_eq!(event["code"], "invalid_goal_transition");
        assert_eq!(event["from"], "planning");
        assert_eq!(event["to"], "done");
    }

    #[test]
    fn goal_transition_table_allows_forward_cancel_and_explicit_reopen() {
        assert!(validate_transition(&GoalPhase::Planning, &GoalPhase::PlanReady).is_ok());
        assert!(validate_transition(&GoalPhase::Running, &GoalPhase::Cancelled).is_ok());
        assert!(validate_transition(&GoalPhase::Done, &GoalPhase::Planning).is_ok());
        assert!(validate_transition(&GoalPhase::Failed, &GoalPhase::Done).is_err());
    }

    #[test]
    fn wrapup_prompt_includes_step_results() {
        let mut m = base_mode();
        m.goal = "ship feature X".into();
        m.plan = Some(GoalPlan {
            summary: "two steps".into(),
            steps: vec![],
            risks: vec![],
            validation: vec![],
        });
        m.prompts = vec![
            DeployPrompt {
                step_id: "a".into(),
                agent: "scout".into(),
                task: "map".into(),
                model: None,
                status: DeployStatus::Done,
                run_id: None,
                summary: Some("found auth.rs".into()),
                title: "recon".into(),
            },
            DeployPrompt {
                step_id: "b".into(),
                agent: "worker".into(),
                task: "impl".into(),
                model: None,
                status: DeployStatus::Failed,
                run_id: None,
                summary: Some("tests failed".into()),
                title: "implement".into(),
            },
        ];
        let p = build_wrapup_prompt(&m);
        assert!(p.contains("ship feature X"));
        assert!(p.contains("[done] recon (scout): found auth.rs"));
        assert!(p.contains("[failed] implement (worker): tests failed"));
        assert!(p.contains("Call `finish`"));
        assert!(p.contains("1 step(s) failed"));
        assert!(p.contains(".catalyst-code/goal-ux/artifacts/"));
    }

    #[test]
    fn nonempty_step_summary_never_blank() {
        assert_eq!(
            nonempty_step_summary(""),
            "(step finished with no written summary)"
        );
        assert_eq!(
            nonempty_step_summary("   "),
            "(step finished with no written summary)"
        );
        assert_eq!(nonempty_step_summary("hello"), "hello");
        let long = "x".repeat(2000);
        let s = nonempty_step_summary(&long);
        assert!(s.chars().count() <= 1601); // 1600 + ellipsis
        assert!(s.ends_with('…'));
    }

    #[test]
    fn deterministic_completion_summary_lists_steps() {
        let mut m = base_mode();
        m.goal = "ship feature X".into();
        m.plan = Some(GoalPlan {
            summary: "two steps".into(),
            steps: vec![],
            risks: vec![],
            validation: vec![],
        });
        m.prompts = vec![
            DeployPrompt {
                step_id: "a".into(),
                agent: "scout".into(),
                task: "map".into(),
                model: None,
                status: DeployStatus::Done,
                run_id: None,
                summary: Some("found auth.rs".into()),
                title: "recon".into(),
            },
            DeployPrompt {
                step_id: "b".into(),
                agent: "worker".into(),
                task: "impl".into(),
                model: None,
                status: DeployStatus::Failed,
                run_id: None,
                summary: None, // empty -> stub
                title: "implement".into(),
            },
        ];
        let s = build_deterministic_completion_summary(&m);
        assert!(s.contains("ship feature X"));
        assert!(s.contains("1 done, 1 failed, 0 skipped"));
        assert!(s.contains("[done] recon (scout): found auth.rs"));
        assert!(s.contains("(step finished with no written summary)"));
        assert!(!s.contains("(no summary)"));
    }

    #[test]
    fn wrapup_prompt_keeps_step_budget_and_total_cap() {
        let mut m = base_mode();
        m.goal = "g".into();
        let big = "y".repeat(5000);
        m.prompts = vec![DeployPrompt {
            step_id: "a".into(),
            agent: "worker".into(),
            task: "t".into(),
            model: None,
            status: DeployStatus::Done,
            run_id: None,
            summary: Some(big.clone()),
            title: "do".into(),
        }];
        let p = build_wrapup_prompt(&m);
        // Summary in wrap-up is truncated to WRAPUP_STEP_BUDGET (3000), not the
        // full 5000.
        assert!(!p.contains(&big));
        assert!(p.contains(&("y".repeat(3_000) + "…")));
    }

    #[test]
    fn wrapup_prompt_omits_steps_past_total_budget() {
        let mut m = base_mode();
        m.goal = "g".into();
        // 15 steps × 3000-char summaries = 45k > WRAPUP_TOTAL_BUDGET (20k).
        let big = "z".repeat(4_000);
        let mut prompts = Vec::new();
        for i in 0..15 {
            prompts.push(DeployPrompt {
                step_id: format!("s{i}"),
                agent: "worker".into(),
                task: "t".into(),
                model: None,
                status: DeployStatus::Done,
                run_id: None,
                summary: Some(big.clone()),
                title: format!("step {i}"),
            });
        }
        m.prompts = prompts;
        let p = build_wrapup_prompt(&m);
        // Later steps should have been omitted with a pointer instead of full text.
        assert!(p.contains("[output omitted to save context"));
    }

    #[test]
    fn dependency_context_injects_prior_step_output() {
        let mut m = base_mode();
        m.goal = "build module".into();
        m.prompts = vec![
            DeployPrompt {
                step_id: "design".into(),
                agent: "planner".into(),
                task: "design the module".into(),
                model: None,
                status: DeployStatus::Done,
                run_id: None,
                summary: Some("The module should expose foo() and bar().".into()),
                title: "design phase".into(),
            },
            DeployPrompt {
                step_id: "implement".into(),
                agent: "worker".into(),
                task: "implement the module".into(),
                model: None,
                status: DeployStatus::Pending,
                run_id: None,
                summary: None,
                title: "impl phase".into(),
            },
        ];
        // The implement step depends on design.
        let deps = vec!["design".to_string()];
        let task = inject_dependency_context("implement the module", &m, &deps);
        // The prior step's summary must be present in the augmented task.
        assert!(task.contains("The module should expose foo() and bar()."));
        assert!(task.contains("Prior step outputs"));
        assert!(task.contains("design phase"));
        // The original task text is preserved.
        assert!(task.starts_with("implement the module"));
    }

    #[test]
    fn dependency_context_noop_without_deps() {
        let mut m = base_mode();
        m.prompts = vec![DeployPrompt {
            step_id: "a".into(),
            agent: "worker".into(),
            task: "t".into(),
            model: None,
            status: DeployStatus::Done,
            run_id: None,
            summary: Some("done".into()),
            title: "a".into(),
        }];
        // No depends_on → task returned unchanged.
        let task = inject_dependency_context("do work", &m, &[]);
        assert_eq!(task, "do work");
    }

    #[test]
    fn dependency_context_skips_unran_deps() {
        let mut m = base_mode();
        m.prompts = vec![
            DeployPrompt {
                step_id: "a".into(),
                agent: "worker".into(),
                task: "t".into(),
                model: None,
                status: DeployStatus::Pending, // hasn't run yet
                run_id: None,
                summary: None,
                title: "a".into(),
            },
            DeployPrompt {
                step_id: "b".into(),
                agent: "worker".into(),
                task: "t".into(),
                model: None,
                status: DeployStatus::Skipped, // skipped
                run_id: None,
                summary: Some("skipped".into()),
                title: "b".into(),
            },
        ];
        // Neither dep ran → no injection, task unchanged.
        let task = inject_dependency_context("do work", &m, &["a".into(), "b".into()]);
        assert_eq!(task, "do work");
    }

    #[test]
    fn write_step_artifact_roundtrip() {
        let dir = tempfile_dir();
        let rel =
            write_step_artifact(&dir, "gid", "step1", "full output here").expect("artifact path");
        assert_eq!(rel, ".catalyst-code/goal-ux/artifacts/gid/step1.md");
        let body = std::fs::read_to_string(dir.join(&rel)).unwrap();
        assert_eq!(body, "full output here");
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("catcode-goal-artifact-{}", now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finish_synthesis_marks_done() {
        let mut m = base_mode();
        m.phase = GoalPhase::Synthesizing;
        m.prompts = vec![DeployPrompt {
            step_id: "a".into(),
            agent: "worker".into(),
            task: "t".into(),
            model: None,
            status: DeployStatus::Done,
            run_id: None,
            summary: Some("ok".into()),
            title: "do".into(),
        }];
        crate::protocol::begin_emit_capture();
        finish_synthesis(&mut m, false);
        let captured = crate::protocol::end_emit_capture();
        assert_eq!(m.phase, GoalPhase::Done);
        assert!(
            captured.iter().any(|(k, d)| {
                k == "goal_completion_summary"
                    && d.get("text")
                        .and_then(|v| v.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
            }),
            "finish_synthesis must emit non-empty goal_completion_summary; got {captured:?}"
        );
        finish_synthesis(&mut m, false); // no-op once Done
        assert_eq!(m.phase, GoalPhase::Done);

        m.phase = GoalPhase::Synthesizing;
        finish_synthesis(&mut m, true);
        assert_eq!(m.phase, GoalPhase::Cancelled);
    }

    #[test]
    fn finish_synthesis_skips_deterministic_when_wrapup_rich() {
        let mut m = base_mode();
        m.phase = GoalPhase::Synthesizing;
        m.prompts = vec![DeployPrompt {
            step_id: "a".into(),
            agent: "worker".into(),
            task: "t".into(),
            model: None,
            status: DeployStatus::Done,
            run_id: Some("run-1".into()),
            summary: Some("ok".into()),
            title: "do".into(),
        }];
        crate::protocol::begin_emit_capture();
        finish_synthesis_with_wrapup(&mut m, false, Some(RICH_WRAPUP_MIN_CHARS + 50));
        let captured = crate::protocol::end_emit_capture();
        assert_eq!(m.phase, GoalPhase::Done);
        assert!(
            !captured.iter().any(|(k, _)| k == "goal_completion_summary"),
            "rich wrap-up should skip deterministic completion card; got {captured:?}"
        );
    }

    #[test]
    fn emit_goal_step_complete_includes_nonempty_summary_and_run_id() {
        crate::protocol::begin_emit_capture();
        emit_goal_step_complete(
            "s1",
            "Implement X",
            "worker",
            true,
            "done",
            "Implemented X cleanly.",
            Some("run-abc"),
        );
        let captured = crate::protocol::end_emit_capture();
        let ev = captured
            .iter()
            .find(|(k, _)| k == "goal_step_complete")
            .expect("goal_step_complete emitted");
        let summary = ev.1.get("summary").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!summary.is_empty());
        assert_eq!(ev.1.get("run_id").and_then(|v| v.as_str()), Some("run-abc"));
        assert_eq!(ev.1.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    fn start_args(goal: &str) -> StartGoalArgs {
        StartGoalArgs {
            goal: goal.into(),
            concurrency: None,
            max_tasks: None,
            allowed_models: vec![],
            allowed_providers: vec![],
            auto_deploy: None,
            ceo_mode: None,
            max_iterations: None,
            max_plan_revisions: None,
            role_models: RoleModels::default(),
            model_concurrency: HashMap::new(),
            model: "m".into(),
            reasoning_effort: None,
            default_concurrency: 4,
            default_max_tasks: 8,
        }
    }

    #[test]
    fn new_goal_rejects_empty() {
        let err = new_goal(start_args("  ")).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn role_models_and_model_concurrency_normalized() {
        let mut args = start_args("ship the feature now");
        args.concurrency = Some(3);
        args.allowed_models = vec!["m1".into(), "m2".into()];
        args.role_models = RoleModels {
            planner: Some("m1".into()),
            worker: Some("not-allowed".into()), // dropped
            reviewer: Some("m2".into()),
        };
        args.model_concurrency.insert("m1".into(), 99); // clamp to 3
        args.model_concurrency.insert("ghost".into(), 2); // dropped vs allowlist
        let mode = new_goal(args).unwrap();
        assert_eq!(mode.role_models.planner.as_deref(), Some("m1"));
        assert!(mode.role_models.worker.is_none());
        assert_eq!(mode.role_models.reviewer.as_deref(), Some("m2"));
        assert_eq!(mode.model_concurrency.get("m1"), Some(&3));
        assert!(!mode.model_concurrency.contains_key("ghost"));
        assert_eq!(mode.parent_model, "m1"); // planner pin for planning turn
    }

    #[test]
    fn resolve_step_model_prefers_role() {
        let mut mode = base_mode();
        mode.role_models.worker = Some("m1".into());
        mode.allowed_models = vec!["m1".into()];
        let m = resolve_step_model(&mode, "worker", Some("other".into()));
        assert_eq!(m.as_deref(), Some("m1"));
        let m2 = resolve_step_model(&mode, "scout", Some("m1".into()));
        assert_eq!(m2.as_deref(), Some("m1"));
    }

    #[test]
    fn resolve_step_model_omits_when_unpinned() {
        let mut mode = base_mode();
        mode.allowed_models = vec!["m1".into(), "m2".into()];
        // Allowlist alone must not force a model pin (that previously pushed
        // every reviewer through the worktree parallel wrapper).
        assert!(resolve_step_model(&mode, "reviewer", None).is_none());
        assert!(resolve_step_model(&mode, "scout", None).is_none());
    }

    #[test]
    fn goal_step_needs_worktree_skips_read_focused_agents() {
        assert!(!goal_step_needs_worktree("reviewer"));
        assert!(!goal_step_needs_worktree("scout"));
        assert!(!goal_step_needs_worktree("researcher"));
        assert!(!goal_step_needs_worktree("planner"));
        assert!(!goal_step_needs_worktree("oracle"));
        assert!(!goal_step_needs_worktree("context-builder"));
        assert!(goal_step_needs_worktree("worker"));
        assert!(goal_step_needs_worktree("delegate"));
        assert!(goal_step_needs_worktree("custom-writer"));
    }

    #[test]
    fn whole_goal_validation_runs_only_after_the_terminal_wave() {
        assert!(!should_validate_goal_after_wave(0, 4));
        assert!(!should_validate_goal_after_wave(1, 4));
        assert!(!should_validate_goal_after_wave(2, 4));
        assert!(should_validate_goal_after_wave(3, 4));
        assert!(!should_validate_goal_after_wave(0, 0));
    }

    #[test]
    fn apply_plan_happy_path_auto_deploy() {
        let mut mode = base_mode();
        let args = json!({
            "summary": "two-step",
            "steps": [
                {"id": "a", "agent": "scout", "title": "recon", "task": "map auth"},
                {"id": "b", "agent": "worker", "title": "impl", "task": "implement", "depends_on": ["a"]}
            ],
            "risks": ["r1"],
            "validation": ["tests pass"]
        });
        let known = HashSet::new();
        apply_plan(&mut mode, &args, &known).unwrap();
        assert_eq!(mode.phase, GoalPhase::PlanReady);
        assert!(mode.deploy_after_turn);
        assert_eq!(mode.prompts.len(), 2);
        assert!(mode.prompts[0].task.contains("ship feature X"));
    }

    #[test]
    fn apply_plan_appends_validation_gate_and_browser_note_to_worker_steps() {
        let mut mode = base_mode();
        let args = json!({
            "summary": "two-step",
            "steps": [
                {"id": "a", "agent": "scout", "title": "recon", "task": "map auth"},
                {"id": "b", "agent": "worker", "title": "impl", "task": "implement auth"}
            ],
            "validation": ["cargo test green"]
        });
        apply_plan(&mut mode, &args, &HashSet::new()).unwrap();
        let scout = mode.prompts.iter().find(|p| p.agent == "scout").unwrap();
        let worker = mode.prompts.iter().find(|p| p.agent == "worker").unwrap();

        // Tool availability (browser group) is appended to every step so workers
        // know they can drive a real browser instead of skipping web work.
        assert!(scout.task.contains("# Tool availability"));
        assert!(scout.task.contains("`browser`"));
        assert!(worker.task.contains("# Tool availability"));
        assert!(worker.task.contains("`browser`"));

        // Goal-level validation criteria are folded into every step's task.
        assert!(worker.task.contains("cargo test green"));

        // The per-step validation gate (build + run tests, zero errors) is
        // enforced on implementation/review agents but skipped for pure-recon.
        assert!(worker
            .task
            .contains("# Validation gate (required for this step)"));
        assert!(worker.task.contains("Running the relevant test suite"));
        assert!(!scout
            .task
            .contains("# Validation gate (required for this step)"));
    }

    #[test]
    fn goal_step_should_validate_skips_only_pure_recon() {
        assert!(!goal_step_should_validate("scout"));
        assert!(!goal_step_should_validate("researcher"));
        assert!(!goal_step_should_validate("context-builder"));
        assert!(goal_step_should_validate("worker"));
        assert!(goal_step_should_validate("reviewer"));
        assert!(goal_step_should_validate("delegate"));
        assert!(goal_step_should_validate("oracle"));
        assert!(goal_step_should_validate("custom-agent"));
    }

    #[test]
    fn apply_plan_review_mode_no_auto() {
        let mut mode = base_mode();
        mode.auto_deploy = false;
        let args = json!({
            "summary": "one",
            "steps": [{"id": "1", "agent": "worker", "title": "do", "task": "work"}]
        });
        apply_plan(&mut mode, &args, &HashSet::new()).unwrap();
        assert!(!mode.deploy_after_turn);
        assert_eq!(mode.phase, GoalPhase::PlanReady);
    }

    #[test]
    fn apply_plan_rejects_too_many_steps() {
        let mut mode = base_mode();
        mode.max_tasks = 1;
        let args = json!({
            "summary": "x",
            "steps": [
                {"id": "1", "agent": "worker", "task": "a"},
                {"id": "2", "agent": "worker", "task": "b"}
            ]
        });
        assert!(apply_plan(&mut mode, &args, &HashSet::new()).is_err());
    }

    #[test]
    fn apply_plan_rejects_cycle() {
        let mut mode = base_mode();
        let args = json!({
            "summary": "x",
            "steps": [
                {"id": "a", "agent": "worker", "task": "a", "depends_on": ["b"]},
                {"id": "b", "agent": "worker", "task": "b", "depends_on": ["a"]}
            ]
        });
        assert!(apply_plan(&mut mode, &args, &HashSet::new()).is_err());
    }

    #[test]
    fn topo_waves_order() {
        let steps = vec![
            GoalStep {
                id: "a".into(),
                agent: "scout".into(),
                title: "a".into(),
                task: "a".into(),
                model: None,
                depends_on: vec![],
                parallel_group: None,
            },
            GoalStep {
                id: "b".into(),
                agent: "worker".into(),
                title: "b".into(),
                task: "b".into(),
                model: None,
                depends_on: vec!["a".into()],
                parallel_group: None,
            },
            GoalStep {
                id: "c".into(),
                agent: "worker".into(),
                title: "c".into(),
                task: "c".into(),
                model: None,
                depends_on: vec!["a".into()],
                parallel_group: Some("impl".into()),
            },
        ];
        let waves = topo_waves(&steps).unwrap();
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0], vec!["a".to_string()]);
        assert!(waves[1].contains(&"b".to_string()) && waves[1].contains(&"c".to_string()));
    }

    #[test]
    fn filter_models_by_allowlist() {
        let mode = GoalMode {
            allowed_models: vec!["m1".into()],
            allowed_providers: vec!["openai".into()],
            ..GoalMode::default()
        };
        let mut map = HashMap::new();
        map.insert("m1".into(), "openai".into());
        map.insert("m2".into(), "anthropic".into());
        map.insert("m3".into(), "openai".into());
        let out = filter_model_candidates(&["m1".into(), "m2".into(), "m3".into()], &mode, &map);
        assert_eq!(out, vec!["m1".to_string()]);
    }

    #[test]
    fn cap_concurrency_respects_goal() {
        let mode = GoalMode {
            concurrency: 2,
            ..GoalMode::default()
        };
        assert_eq!(cap_concurrency(8, &mode), 2);
        assert_eq!(cap_concurrency(1, &mode), 1);
    }

    #[test]
    fn high_concurrency_selects_ultra_parallel_profile() {
        assert_eq!(execution_profile(1), "serial");
        assert_eq!(execution_profile(4), "parallel");
        assert_eq!(execution_profile(8), "ultra_parallel");
        assert_eq!(execution_profile(32), "ultra_parallel");
    }

    #[test]
    fn ultra_planning_prompt_demands_broad_root_fanout() {
        let mut mode = base_mode();
        mode.concurrency = 12;
        mode.max_tasks = 16;
        let prompt = planning_prompt(&mode);
        assert!(prompt.contains("Execution profile: ULTRA PARALLEL"));
        assert!(prompt.contains("12 available slots"));
        assert!(prompt.contains("12 useful root steps"));
        assert!(prompt.contains("do not put one global scout"));
    }

    #[test]
    fn planning_prompt_asks_before_write_for_high_impact_unknowns() {
        let mode = base_mode();
        let prompt = planning_prompt(&mode);
        assert!(prompt.contains("call `ask` ONCE"));
        assert!(prompt.contains("high-impact unknowns"));
        assert!(prompt.contains("Skip low-stakes style/preference"));
        assert!(prompt.contains("promoted files in worktrees"));
        // goal_write_plan remains the terminal planning action.
        assert!(prompt.contains("goal_write_plan` tool EXACTLY ONCE"));
    }

    #[test]
    fn ceo_planning_prompt_forbids_ask() {
        let mut mode = base_mode();
        mode.ceo_mode = true;
        mode.max_iterations = 3;
        mode.max_plan_revisions = 2;
        let prompt = planning_prompt(&mode);
        assert!(prompt.contains("Never call `ask`") || prompt.contains("Do NOT call `ask`"));
        assert!(!prompt.contains("call `ask` ONCE"));
    }

    #[test]
    fn parse_ceo_verdict_pass_fail_and_gaps() {
        let (ok, _, gaps) = parse_ceo_verdict(
            "looks good
VERDICT: PASS
",
        );
        assert!(ok);
        assert!(gaps.is_empty());
        let (ok, _, gaps) = parse_ceo_verdict(
            "missing tests
VERDICT: FAIL
GAPS:
- add unit tests
- wire UI
",
        );
        assert!(!ok);
        assert_eq!(gaps.len(), 2);
        assert!(gaps[0].contains("unit tests"));
    }

    #[test]
    fn finish_reviewing_deploys_or_replans() {
        let mut m = base_mode();
        m.ceo_mode = true;
        m.max_plan_revisions = 2;
        m.phase = GoalPhase::Reviewing;
        m.plan = Some(GoalPlan {
            summary: "s".into(),
            steps: vec![],
            risks: vec![],
            validation: vec![],
        });
        m.prompts = vec![DeployPrompt {
            step_id: "1".into(),
            agent: "worker".into(),
            task: "t".into(),
            model: None,
            status: DeployStatus::Pending,
            run_id: None,
            summary: None,
            title: "t".into(),
        }];
        assert_eq!(
            finish_reviewing(&mut m, false, "VERDICT: PASS"),
            ReviewOutcome::Deploy
        );
        assert_eq!(m.phase, GoalPhase::PlanReady);

        m.phase = GoalPhase::Reviewing;
        m.plan_revision = 0;
        assert_eq!(
            finish_reviewing(
                &mut m,
                false,
                "VERDICT: FAIL
GAPS:
- fix deps"
            ),
            ReviewOutcome::Replan
        );
        assert_eq!(m.phase, GoalPhase::Planning);
        assert_eq!(m.plan_revision, 1);
        assert!(m.plan.is_none());
    }

    #[test]
    fn finish_verifying_certifies_or_replans() {
        let mut m = base_mode();
        m.ceo_mode = true;
        m.max_iterations = 2;
        m.phase = GoalPhase::Verifying;
        assert_eq!(
            finish_verifying(
                &mut m,
                false,
                "evidence ok
VERDICT: PASS"
            ),
            VerifyOutcome::Certified
        );
        assert!(m.certified);
        assert_eq!(m.phase, GoalPhase::Done);

        m.phase = GoalPhase::Verifying;
        m.certified = false;
        m.iteration = 0;
        assert_eq!(
            finish_verifying(
                &mut m,
                false,
                "VERDICT: FAIL
GAPS:
- still broken"
            ),
            VerifyOutcome::Replan
        );
        assert_eq!(m.phase, GoalPhase::Planning);
        assert_eq!(m.iteration, 1);
        assert!(!m.remaining_gaps.is_empty());
    }

    #[test]
    fn verify_failure_evidence_becomes_remaining_gap() {
        let mut m = base_mode();
        m.ceo_mode = true;
        m.max_iterations = 2;
        m.phase = GoalPhase::Verifying;
        assert_eq!(
            finish_verifying(
                &mut m,
                false,
                "VERDICT: FAIL\nGAPS:\n- evidence validation failed: required summary unavailable"
            ),
            VerifyOutcome::Replan
        );
        assert!(!m.certified);
        assert!(m.remaining_gaps[0].contains("evidence validation failed"));
    }

    #[test]
    fn finish_verifying_fail_without_gaps_synthesizes_remaining_gaps() {
        let mut m = base_mode();
        m.ceo_mode = true;
        m.max_iterations = 2;
        m.phase = GoalPhase::Verifying;
        assert_eq!(
            finish_verifying(&mut m, false, "VERDICT: FAIL\nsomething went wrong"),
            VerifyOutcome::Replan
        );
        assert!(!m.remaining_gaps.is_empty());
        assert!(m
            .revise_feedback
            .as_ref()
            .unwrap()
            .contains("Remaining gaps"));
    }

    #[test]
    fn finish_verifying_iteration_cap_fails() {
        let mut m = base_mode();
        m.ceo_mode = true;
        m.max_iterations = 1;
        m.iteration = 1; // already used the budget
        m.phase = GoalPhase::Verifying;
        assert_eq!(
            finish_verifying(
                &mut m,
                false,
                "VERDICT: REMAINING_GAPS\n## Remaining gaps\n- still open"
            ),
            VerifyOutcome::Failed
        );
        assert_eq!(m.phase, GoalPhase::Failed);
        assert!(!m.certified);
    }

    #[tokio::test]
    async fn worker_timeout_cancels_child_and_returns_terminal_failure() {
        let cancellation = CancellationToken::new();
        let child_observer = cancellation.clone();
        let worker = async move {
            child_observer.cancelled().await;
            Outcome::err("worker observed cancellation")
        };
        let outcome =
            await_goal_worker(worker, &cancellation, std::time::Duration::from_millis(5)).await;
        assert!(!outcome.ok);
        assert!(outcome.output.contains("timed out"));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn stale_worker_result_is_rejected_after_goal_replacement_or_cancellation() {
        let mut mode = base_mode();
        mode.id = "goal-current".into();
        mode.phase = GoalPhase::Running;
        assert!(accepts_goal_worker_result(&mode, "goal-current", false));
        assert!(!accepts_goal_worker_result(&mode, "goal-old", false));
        assert!(!accepts_goal_worker_result(&mode, "goal-current", true));
        mode.phase = GoalPhase::Cancelled;
        assert!(!accepts_goal_worker_result(&mode, "goal-current", false));
    }

    #[test]
    fn terminal_goal_transitions_clear_active_worker_ids() {
        let mut failed = base_mode();
        failed.phase = GoalPhase::Running;
        failed.active_run_ids = vec!["worker-a".into(), "worker-b".into()];
        fail_goal(&mut failed, "worker failure");
        assert_eq!(failed.phase, GoalPhase::Failed);
        assert!(failed.active_run_ids.is_empty());

        let mut cancelled = base_mode();
        cancelled.phase = GoalPhase::Running;
        cancelled.active_run_ids = vec!["worker-c".into()];
        cancel_goal(&mut cancelled, "parent cancelled");
        assert_eq!(cancelled.phase, GoalPhase::Cancelled);
        assert!(cancelled.active_run_ids.is_empty());
    }

    #[test]
    fn new_goal_ceo_defaults() {
        let mut args = start_args("ship the control center feature");
        args.ceo_mode = Some(true);
        let mode = new_goal(args).unwrap();
        assert!(mode.ceo_mode);
        assert_eq!(mode.max_iterations, DEFAULT_CEO_MAX_ITERATIONS);
        assert_eq!(mode.max_plan_revisions, DEFAULT_CEO_MAX_PLAN_REVISIONS);
        let v = mode.to_event_value();
        assert_eq!(v.get("mode").and_then(|x| x.as_str()), Some("ceo"));
    }

    #[test]
    fn ultra_prompt_reserves_integration_slot_when_budget_equals_cap() {
        let mut mode = base_mode();
        mode.concurrency = 8;
        mode.max_tasks = 8;
        let prompt = planning_prompt(&mode);
        assert!(prompt.contains("7 useful root steps"));
    }

    #[test]
    fn strips_disallowed_step_model() {
        let mut mode = base_mode();
        mode.allowed_models = vec!["m1".into()];
        let args = json!({
            "summary": "x",
            "steps": [{
                "id": "1", "agent": "worker", "task": "t", "model": "other-model"
            }]
        });
        apply_plan(&mut mode, &args, &HashSet::new()).unwrap();
        // Disallowed step.model is stripped; no auto-pin to allowlist[0] —
        // parent model + allowlist filter apply at execute time.
        assert!(mode.prompts[0].model.is_none());
    }

    /// Verify the no-fail-fast skip logic: when a step fails, its dependents
    /// are skipped but independent steps in later waves still run. This mirrors
    /// the `deps_by_id` + `failed_steps` logic in `deploy_goal`.
    #[test]
    fn deploy_skips_dependents_of_failed_steps_not_independent_ones() {
        let steps = vec![
            GoalStep {
                id: "recon".into(),
                agent: "scout".into(),
                title: "recon".into(),
                task: "recon".into(),
                model: None,
                depends_on: vec![],
                parallel_group: None,
            },
            GoalStep {
                id: "plan".into(),
                agent: "planner".into(),
                title: "plan".into(),
                task: "plan".into(),
                model: None,
                depends_on: vec!["recon".into()],
                parallel_group: None,
            },
            GoalStep {
                id: "independent".into(),
                agent: "worker".into(),
                title: "independent".into(),
                task: "independent".into(),
                model: None,
                depends_on: vec![],
                parallel_group: None,
            },
        ];
        let waves = topo_waves(&steps).unwrap();
        // Wave 0: recon + independent (parallel). Wave 1: plan (depends on recon).
        assert_eq!(waves.len(), 2);

        // Simulate: recon FAILED, independent SUCCEEDED.
        let failed_steps: HashSet<String> = ["recon".to_string()].into_iter().collect();
        let deps_by_id: HashMap<String, Vec<String>> = steps
            .iter()
            .map(|s| (s.id.clone(), s.depends_on.clone()))
            .collect();

        // In wave 1, "plan" depends on failed "recon" → should be skipped.
        let plan_deps = deps_by_id.get("plan").unwrap();
        assert!(
            plan_deps.iter().any(|d| failed_steps.contains(d)),
            "plan depends on failed recon → should be skipped"
        );

        // "independent" has no deps → should NOT be skipped.
        let indep_deps = deps_by_id.get("independent").unwrap();
        assert!(
            !indep_deps.iter().any(|d| failed_steps.contains(d)),
            "independent has no failed deps → should still run"
        );
    }
}
