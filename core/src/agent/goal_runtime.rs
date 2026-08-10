use crate::*;

#[derive(Clone, Default)]
pub struct WorkState {
    pub goal: String,
    pub done: Vec<String>,
    pub in_progress: Vec<String>,
    pub next: Vec<String>,
    pub recent_files: Vec<String>,
    pub last_activity: String,
    pub version: u64,
}

impl WorkState {
    /// Render as a compact, model-facing system block. Empty sections are
    /// omitted so the block stays minimal; each list is capped so a runaway
    /// plan can't bloat every request.
    pub fn render(&self) -> String {
        const MAX_LIST: usize = 6;
        const MAX_FILES: usize = 8;
        let mut out = String::from(
            "[Work state — ambient status the harness keeps current via todo_write \
             and file edits. Use it as context; respond to the user's latest message, \
             not to this block. Keep it accurate by updating todos as you work.]",
        );
        out.push_str("\nGoal: ");
        let goal = if self.goal.is_empty() {
            "(not yet stated)".to_string()
        } else {
            truncate_str(self.goal.as_str(), 240)
        };
        out.push_str(&goal);
        {
            let mut section = |label: &str, items: &[String]| {
                if items.is_empty() {
                    return;
                }
                out.push('\n');
                out.push_str(label);
                for it in items.iter().take(MAX_LIST) {
                    out.push_str("\n- ");
                    out.push_str(&truncate_str(it, 160));
                }
                if items.len() > MAX_LIST {
                    out.push_str(&format!("\n- … +{} more", items.len() - MAX_LIST));
                }
            };
            section("Done:", &self.done);
            section("In progress:", &self.in_progress);
            section("Next:", &self.next);
        }
        if !self.recent_files.is_empty() {
            out.push_str("\nRecently touched: ");
            let files: Vec<String> = self
                .recent_files
                .iter()
                .take(MAX_FILES)
                .map(|s| truncate_str(s, 120))
                .collect();
            out.push_str(&files.join(", "));
        }
        if !self.last_activity.is_empty() {
            out.push_str("\nLast: ");
            out.push_str(&truncate_str(&self.last_activity, 160));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.goal.is_empty()
            && self.done.is_empty()
            && self.in_progress.is_empty()
            && self.next.is_empty()
            && self.recent_files.is_empty()
            && self.last_activity.is_empty()
    }

    pub(crate) fn touch(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Partition a `todo_write` payload into done/in-progress/next. Pure logic
    /// (no locking/emit) so it is unit-testable; the async wrapper adds those.
    pub fn sync_from_todos(&mut self, todos: &[Value]) {
        let mut done = Vec::new();
        let mut in_progress = Vec::new();
        let mut next = Vec::new();
        for t in todos {
            let subject = t.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            if subject.is_empty() {
                continue;
            }
            match t.get("status").and_then(|v| v.as_str()).unwrap_or("") {
                "completed" => done.push(subject.to_string()),
                "in_progress" => in_progress.push(subject.to_string()),
                _ => next.push(subject.to_string()),
            }
        }
        self.done = done;
        self.in_progress = in_progress;
        self.next = next;
        self.touch();
    }

    /// Record file paths touched (most-recent-first, deduped, capped) and a
    /// short last-activity note. Pure logic; the async wrapper extracts paths.
    pub fn record_files(&mut self, tool: &str, paths: &[String]) {
        if paths.is_empty() {
            return;
        }
        // Iterate in reverse so the FIRST-listed (primary) path lands at the
        // front of the most-recent-first list — "Recently touched: a.rs, b.rs"
        // reads naturally when a.rs was the edit's primary target.
        for p in paths.iter().rev() {
            if let Some(pos) = self.recent_files.iter().position(|x| x == p) {
                self.recent_files.remove(pos);
            }
            self.recent_files.insert(0, p.clone());
        }
        self.recent_files.truncate(8);
        let act = format!("{} {}", tool, paths.join(", "));
        self.last_activity = truncate_str(&act, 160);
        self.touch();
    }
}

/// Emit a `work_state` event with the current rolling summary so the TUI/web
/// can render a live status panel alongside the conversation.
pub(crate) async fn emit_work_state(st: &State) {
    let ws = st.work_state.lock().await.clone();
    emit(
        &Event::new("work_state")
            .with("version", json!(ws.version))
            .with("goal", json!(ws.goal))
            .with("done", json!(ws.done))
            .with("in_progress", json!(ws.in_progress))
            .with("next", json!(ws.next))
            .with("recent_files", json!(ws.recent_files))
            .with("last_activity", json!(ws.last_activity)),
    );
}

/// Cancel an in-flight goal deploy task (if any).
pub(crate) async fn cancel_goal_deploy(st: &State) {
    if let Some(tok) = st.goal_deploy_cancel.lock().await.take() {
        tok.cancel();
    }
}

/// Spawn the deterministic goal deploy loop on a child cancel token.
/// After workers finish, starts a parent synthesizing turn so the user gets a
/// completion follow-up (the planning turn already called `finish`).
///
/// Non-async on purpose: callers inside parent-turn helpers must not `.await` this
/// (breaks a recursive `Future: Send` cycle with `start_goal_parent_turn`).
pub(crate) fn spawn_goal_deploy(st: Arc<State>, client: reqwest::Client) {
    let session = st.runtime.session_context();
    let Some(resource) =
        st.runtime
            .register_session_resource(&session, ResourceKind::Goal, "goal_deploy")
    else {
        {
            emit(&Event::new("error").with(
                "message",
                json!("goal deploy could not register session resource — session inactive"),
            ));
            return;
        }
    };
    tokio::spawn(runtime::scope_session(session, async move {
        cancel_goal_deploy(&st).await;
        // Emit before the (possibly slow) workspace snapshot so UIs leave the
        // plan_ready dark gap immediately.
        emit(&Event::new("info").with("message", json!("Goal deploy: snapshotting workspace…")));
        let tok = resource.cancellation().clone();
        *st.goal_deploy_cancel.lock().await = Some(tok.clone());
        let checkpoint_result = {
            let cfg = st.cfg.read().await;
            checkpoint::create(
                &cfg.workspace,
                cfg.session_file.as_deref(),
                "auto-before-goal-deploy",
                &[],
                true,
            )
        };
        if let Err(error) = checkpoint_result {
            let mut g = st.goal.lock().await;
            goal::fail_goal(
                &mut g,
                format!("goal deploy blocked: checkpoint failed: {error}"),
            );
            emit(&Event::new("error").with(
                "message",
                json!(format!(
                    "Goal deploy blocked: automatic checkpoint failed: {error}"
                )),
            ));
            return;
        }
        let need_followup = goal::deploy_goal(st.clone(), client.clone(), tok.clone()).await;
        // Clear cancel slot only if we still own it. A newer spawn cancels
        // this token first, so a cancelled token means the slot was replaced.
        if !tok.is_cancelled() {
            *st.goal_deploy_cancel.lock().await = None;
        }
        if !need_followup || tok.is_cancelled() {
            return;
        }
        let phase = {
            let g = st.goal.lock().await;
            g.phase.clone()
        };
        match phase {
            goal::GoalPhase::Verifying => {
                spawn_goal_verify(st.clone(), client.clone()).await;
            }
            goal::GoalPhase::Synthesizing => {
                let wrap = {
                    let g = st.goal.lock().await;
                    (
                        goal::build_wrapup_prompt(&g),
                        g.parent_model.clone(),
                        g.reasoning_effort.clone(),
                        g.id.clone(),
                    )
                };
                let (prompt, model, effort, goal_id) = wrap;
                start_goal_parent_turn(
                    st.clone(),
                    client.clone(),
                    prompt,
                    model,
                    effort,
                    goal_id,
                    "wrapup",
                )
                .await;
            }
            _ => {}
        }
    }));
}

/// After a planning turn ends: fail if no plan, or kick review/deploy.
pub(crate) async fn maybe_finish_goal_planning(
    st: &Arc<State>,
    client: &reqwest::Client,
    cancelled: bool,
) {
    enum Next {
        Deploy,
        Review,
        None,
    }
    let next = {
        let mut g = st.goal.lock().await;
        // Deploy path: plan was written this turn (or earlier) and auto-deploy is armed.
        if g.deploy_after_turn && g.plan.is_some() && !g.prompts.is_empty() {
            g.deploy_after_turn = false;
            if g.ceo_mode && g.max_plan_revisions > 0 {
                Next::Review
            } else {
                Next::Deploy
            }
        } else if g.phase == goal::GoalPhase::Planning {
            if cancelled {
                goal::cancel_goal(&mut g, "planning aborted");
            } else if g.plan.is_none() {
                goal::fail_goal(
                    &mut g,
                    "planning turn ended without goal_write_plan — re-run /goal",
                );
            }
            Next::None
        } else {
            Next::None
        }
    };
    match next {
        Next::Deploy => spawn_goal_deploy(st.clone(), client.clone()),
        // Fire-and-forget like deploy: planning finishers still hold st.current,
        // so awaiting review here always hit the "session busy" path and skipped
        // self-review (never deploying). Spawn so current can clear first.
        Next::Review => spawn_goal_review(st.clone(), client.clone()),
        Next::None => {}
    }
}

/// Start the CEO pre-deploy self-review parent turn.
///
/// Non-async on purpose (same pattern as [`spawn_goal_deploy`]): callers inside
/// parent-turn finishers must not `.await` this while `st.current` is still held.
pub(crate) fn spawn_goal_review(st: Arc<State>, client: reqwest::Client) {
    let session = st.runtime.session_context();
    let Some(_resource) =
        st.runtime
            .register_session_resource(&session, ResourceKind::Goal, "goal_review")
    else {
        {
            emit(&Event::new("error").with(
                "message",
                json!("goal review could not register session resource — session inactive"),
            ));
            return;
        }
    };
    tokio::spawn(runtime::scope_session(session, async move {
        // Wait for the planning turn to release the session slot.
        // Wait up to ~2 minutes for the parent turn slot (was ~3s and hard-failed
        // healthy missions on slow tool drains) (CORE_REVIEW).
        for _ in 0..2400 {
            if st.current.lock().await.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let wrap = {
            let mut g = st.goal.lock().await;
            if g.plan.is_none() || g.prompts.is_empty() {
                return;
            }
            // Only start review from plan_ready (fresh plan) or reviewing (retry).
            if g.phase != goal::GoalPhase::PlanReady && g.phase != goal::GoalPhase::Reviewing {
                return;
            }
            goal::transition(
                &mut g,
                goal::GoalPhase::Reviewing,
                Some("CEO self-reviewing plan"),
            );
            (
                goal::build_self_review_prompt(&g),
                g.parent_model.clone(),
                g.reasoning_effort.clone(),
                g.id.clone(),
            )
        };
        let (prompt, model, effort, goal_id) = wrap;
        start_goal_parent_turn(st, client, prompt, model, effort, goal_id, "review").await;
    }));
}

/// Start the CEO post-deploy verify parent turn.
pub(crate) async fn spawn_goal_verify(st: Arc<State>, client: reqwest::Client) {
    let workspace = st.cfg.read().await.workspace.clone();
    let wrap = {
        let g = st.goal.lock().await;
        if g.phase != goal::GoalPhase::Verifying {
            return;
        }
        (
            crate::goal_ceo::verify_prompt_with_workspace(&g, Some(&workspace)),
            g.parent_model.clone(),
            g.reasoning_effort.clone(),
            g.id.clone(),
        )
    };
    let (prompt, model, effort, goal_id) = wrap;
    start_goal_parent_turn(st, client, prompt, model, effort, goal_id, "verify").await;
}

/// Shared helper: start a parent orchestrator turn for review/verify/wrap-up.
pub(crate) async fn start_goal_parent_turn(
    st: Arc<State>,
    client: reqwest::Client,
    prompt: String,
    model: String,
    effort: String,
    goal_id: String,
    kind: &str,
) {
    let models = st.models.read().await;
    let turn_model = if models.iter().any(|m| m.id == model) {
        Some(model)
    } else {
        models.first().map(|m| m.id.clone())
    };
    drop(models);

    let Some(turn_model) = turn_model else {
        let mut g = st.goal.lock().await;
        if g.id == goal_id {
            match kind {
                "review" if g.phase == goal::GoalPhase::Reviewing => {
                    goal::fail_goal(&mut g, "plan self-review skipped: no models");
                }
                "verify" if g.phase == goal::GoalPhase::Verifying => {
                    goal::fail_goal(&mut g, "goal verify skipped: no models");
                }
                "wrapup" if g.phase == goal::GoalPhase::Synthesizing => {
                    goal::finish_synthesis(&mut g, false);
                    goal::sync_work_state_from_prompts(&st, &g).await;
                }
                _ => {}
            }
        }
        emit(
            &Event::new("error").with("message", json!(format!("goal {kind} skipped: no models"))),
        );
        return;
    };

    // Wait for the session slot. Review/verify/wrap-up are spawned from finishers
    // or deploy tasks that may still briefly hold `st.current`; never skip CEO
    // self-review by silently accepting the plan (that left missions stuck in
    // plan_ready with deploy_after_turn armed and nobody consuming it).
    // Wait up to ~2 minutes for the parent turn slot (was ~3s) (CORE_REVIEW).
    for _ in 0..2400 {
        if st.current.lock().await.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    enum BusyAction {
        FailVerify,
        FailReview,
        FinishWrapup,
        Abort,
    }
    let busy = {
        let mut cur = st.current.lock().await;
        if cur.is_some() {
            drop(cur);
            let mut g = st.goal.lock().await;
            if g.id != goal_id {
                Some(BusyAction::Abort)
            } else {
                match kind {
                    "review" if g.phase == goal::GoalPhase::Reviewing => {
                        goal::fail_goal(
                            &mut g,
                            "plan self-review skipped: session still busy after wait",
                        );
                        Some(BusyAction::FailReview)
                    }
                    "verify" if g.phase == goal::GoalPhase::Verifying => {
                        goal::fail_goal(&mut g, "goal verify skipped: another turn is running");
                        Some(BusyAction::FailVerify)
                    }
                    "wrapup" if g.phase == goal::GoalPhase::Synthesizing => {
                        goal::finish_synthesis(&mut g, false);
                        Some(BusyAction::FinishWrapup)
                    }
                    _ => Some(BusyAction::Abort),
                }
            }
        } else {
            let run = st.runtime.start_run();
            *cur = Some(run.clone());
            drop(cur);
            st.goal_wrapup_active
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let handle = tokio::spawn(run_turn_and_drain(
                st.clone(),
                client.clone(),
                turn_model,
                None,
                prompt,
                effort,
                None,
                run,
            ));
            *st.handle.lock().await = Some(handle);
            None
        }
    };

    match busy {
        Some(BusyAction::FailVerify)
        | Some(BusyAction::FailReview)
        | Some(BusyAction::FinishWrapup) => {
            // Sync work state after releasing the goal lock (busy block above).
            {
                let g = st.goal.lock().await;
                if g.id == goal_id {
                    goal::sync_work_state_from_prompts(&st, &g).await;
                }
            }
            emit(&Event::new("info").with(
                "message",
                json!(format!(
                    "Goal {kind} finished without parent turn (another turn is running)"
                )),
            ));
        }
        Some(BusyAction::Abort) | None => {}
    }
}

/// After the post-deploy synthesizing turn ends, mark the goal Done.
pub(crate) async fn maybe_finish_goal_synthesis(st: &Arc<State>, cancelled: bool) {
    // Measure wrap-up assistant text so finish_synthesis can skip a redundant
    // deterministic card when the model already streamed a rich summary.
    let wrapup_chars = {
        let conv = st.conversation.lock().await;
        conv.iter()
            .rev()
            .find(|m| m.role() == "assistant")
            .and_then(|m| m.content_text())
            .map(|t| t.trim().chars().count())
            .unwrap_or(0)
    };
    let mut g = st.goal.lock().await;
    if g.phase != goal::GoalPhase::Synthesizing {
        return;
    }
    goal::finish_synthesis_with_wrapup(&mut g, cancelled, Some(wrapup_chars));
    goal::sync_work_state_from_prompts(st, &g).await;
}

/// After a CEO self-review turn ends: deploy or re-enter planning.
pub(crate) async fn maybe_finish_goal_reviewing(
    st: &Arc<State>,
    client: &reqwest::Client,
    cancelled: bool,
) {
    let assistant = {
        let conv = st.conversation.lock().await;
        conv.iter()
            .rev()
            .find(|m| m.role() == "assistant")
            .and_then(|m| m.content_text())
            .map(|t| t.to_string())
            .unwrap_or_default()
    };
    let (outcome, prompt, model, effort) = {
        let mut g = st.goal.lock().await;
        if g.phase != goal::GoalPhase::Reviewing {
            return;
        }
        let outcome = goal::finish_reviewing(&mut g, cancelled, &assistant);
        let next = match outcome {
            goal::ReviewOutcome::Deploy => (outcome, None, String::new(), String::new()),
            goal::ReviewOutcome::Replan => (
                outcome,
                Some(goal::planning_prompt(&g)),
                g.parent_model.clone(),
                g.reasoning_effort.clone(),
            ),
            goal::ReviewOutcome::Failed => (outcome, None, String::new(), String::new()),
        };
        // Snapshot for work-state sync after releasing the goal lock.
        let mode_snapshot = g.clone();
        drop(g);
        goal::sync_work_state_from_prompts(st, &mode_snapshot).await;
        next
    };
    match outcome {
        goal::ReviewOutcome::Deploy => {
            spawn_goal_deploy(st.clone(), client.clone());
        }
        goal::ReviewOutcome::Replan => {
            if let Some(prompt) = prompt {
                start_turn(st, client, model, None, prompt, effort, None).await;
            }
        }
        goal::ReviewOutcome::Failed => {}
    }
}

/// After a CEO verify turn ends: certify Done or replan / fail.
pub(crate) async fn maybe_finish_goal_verifying(
    st: &Arc<State>,
    client: &reqwest::Client,
    cancelled: bool,
) {
    let assistant = {
        let conv = st.conversation.lock().await;
        conv.iter()
            .rev()
            .find(|m| m.role() == "assistant")
            .and_then(|m| m.content_text())
            .map(|t| t.to_string())
            .unwrap_or_default()
    };
    let workspace = st.cfg.read().await.workspace.clone();
    let (outcome, prompt, model, effort) = {
        let mut g = st.goal.lock().await;
        if g.phase != goal::GoalPhase::Verifying {
            return;
        }
        let upper = assistant.to_ascii_uppercase();
        let claims_pass = upper.contains("VERDICT: PASS")
            || upper.contains("VERDICT: CERTIFY")
            || upper.contains("VERDICT: CERTIFIED");
        let candidate = if g.ceo_mode && claims_pass {
            match crate::goal_ceo::validate_certification_evidence(&workspace, &g) {
                Ok(_) => assistant.clone(),
                Err(error) => {
                    format!("VERDICT: FAIL\nGAPS:\n- evidence validation failed: {error}")
                }
            }
        } else {
            assistant.clone()
        };
        let outcome = goal::finish_verifying(&mut g, cancelled, &candidate);
        let next = match outcome {
            goal::VerifyOutcome::Replan => (
                outcome,
                Some(goal::planning_prompt(&g)),
                g.parent_model.clone(),
                g.reasoning_effort.clone(),
            ),
            other => (other, None, String::new(), String::new()),
        };
        let mode_snapshot = g.clone();
        drop(g);
        goal::sync_work_state_from_prompts(st, &mode_snapshot).await;
        next
    };
    if matches!(outcome, goal::VerifyOutcome::Replan) {
        if let Some(prompt) = prompt {
            start_turn(st, client, model, None, prompt, effort, None).await;
        }
    }
}

/// Finalize goal parent turns that may end via `finish` (includes planning).
pub(crate) async fn maybe_finish_goal_orchestrator_turn(
    st: &Arc<State>,
    client: &reqwest::Client,
    cancelled: bool,
) {
    maybe_finish_goal_planning(st, client, cancelled).await;
    maybe_finish_goal_followup_turn(st, client, cancelled).await;
}

/// Finalize review / verify / wrap-up follow-ups (safe from drain; never planning).
pub(crate) async fn maybe_finish_goal_followup_turn(
    st: &Arc<State>,
    client: &reqwest::Client,
    cancelled: bool,
) {
    maybe_finish_goal_reviewing(st, client, cancelled).await;
    maybe_finish_goal_verifying(st, client, cancelled).await;
    maybe_finish_goal_synthesis(st, cancelled).await;
}

/// Seed the work-state goal from a user prompt (the first substantive message).
/// Subsequent calls are no-ops once a goal is set, so the goal reflects the
/// session's original intent rather than every follow-up. Slash commands and
/// trivially short prompts are ignored so they don't pin the goal.
pub(crate) async fn maybe_seed_work_state_goal(st: &State, prompt: &str) {
    let p = prompt.trim();
    if p.is_empty() || p.starts_with('/') || p.chars().count() < 8 {
        return;
    }
    let mut ws = st.work_state.lock().await;
    if !ws.goal.is_empty() {
        return;
    }
    ws.goal = truncate_str(p.lines().next().unwrap_or(p), 240);
    ws.touch();
    drop(ws);
    emit_work_state(st).await;
}
