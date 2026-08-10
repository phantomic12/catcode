package main

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
)

// ---------------------------------------------------------------------------
// Core event handling
// ---------------------------------------------------------------------------

// accumulateSaved adds a context-management event's reclaimed tokens
// (before_tokens − after_tokens) to the session's cumulative counter. Shared by
// the "digested" and "compacted" events; both carry the same before/after
// fields, and because each event reports only its own delta they add up without
// double-counting across the soft-digest and compaction tiers.
func (s *session) accumulateSaved(ev *coreEvent) {
	before, err1 := strconv.ParseUint(ev.get("before_tokens"), 10, 64)
	after, err2 := strconv.ParseUint(ev.get("after_tokens"), 10, 64)
	if err1 != nil || err2 != nil || before <= after {
		return
	}
	s.tokensSaved += before - after
}

// applyGoalState parses a goal_state event into s.goalState and opens the
// plan-ready review modal when the user asked to review before deploy.
func (s *session) applyGoalState(raw json.RawMessage) {
	var m struct {
		ID         string `json:"id"`
		Goal       string `json:"goal"`
		Phase      string `json:"phase"`
		Error      string `json:"error"`
		AutoDeploy bool   `json:"auto_deploy"`
		Version    uint64 `json:"version"`
		Prompts    []struct {
			StepID  string `json:"step_id"`
			Agent   string `json:"agent"`
			Title   string `json:"title"`
			Status  string `json:"status"`
			Summary string `json:"summary"`
		} `json:"prompts"`
	}
	if err := json.Unmarshal(raw, &m); err != nil {
		return
	}
	if m.Phase == "idle" || m.ID == "" {
		s.goalState = nil
		s.goalPlan = nil
		s.goalStepLogged = nil
		s.goalLastLife = ""
		s.goalCompleteLogged = false
		// Idle/clear must release the footer — previously we returned without
		// touching busy, so clear_goal could leave "working" stuck forever.
		s.busy = false
		s.subProgress = nil
		return
	}
	snap := &goalStateSnap{
		ID:         m.ID,
		Goal:       m.Goal,
		Phase:      m.Phase,
		Error:      m.Error,
		AutoDeploy: m.AutoDeploy,
		Version:    m.Version,
	}
	for _, p := range m.Prompts {
		snap.Prompts = append(snap.Prompts, goalPromptSnap{
			StepID:  p.StepID,
			Agent:   p.Agent,
			Title:   p.Title,
			Status:  p.Status,
			Summary: p.Summary,
		})
	}
	prevPhase := ""
	prevByID := map[string]goalPromptSnap{}
	if s.goalState != nil {
		prevPhase = s.goalState.Phase
		for _, p := range s.goalState.Prompts {
			prevByID[p.StepID] = p
		}
	}
	s.goalState = snap
	// Keep the footer busy while deploy / workers / wrap-up / CEO loops are
	// still live (the planning turn's `done` would otherwise clear busy too early).
	// Phase set mirrors web goalKeepsStreaming + applyGoalState streaming.
	switch m.Phase {
	case "planning", "reviewing", "deploying", "running", "synthesizing",
		"verifying", "replanning":
		s.busy = true
	case "plan_ready":
		if m.AutoDeploy {
			s.busy = true
		}
	case "done", "failed", "cancelled", "idle":
		// Clear the footer once the goal settles — wrap-up's `done` arrives
		// while phase is still synthesizing (goalKeepsBusy), so this is the
		// path that actually releases busy after a successful goal.
		s.busy = false
	}
	// Open plan review when we first land on plan_ready with review mode.
	if m.Phase == "plan_ready" && prevPhase != "plan_ready" && !m.AutoDeploy {
		s.openGoalPlanReview()
	}
	// Belt-and-suspenders: persist lasting step cards when prompts become
	// terminal (covers cores that have not yet emitted goal_step_complete).
	for _, p := range snap.Prompts {
		if !goalTerminalStatus(p.Status) {
			continue
		}
		prev, had := prevByID[p.StepID]
		newlyTerminal := !had || !goalTerminalStatus(prev.Status)
		summaryGrew := strings.TrimSpace(p.Summary) != "" && p.Summary != prev.Summary
		if newlyTerminal || summaryGrew {
			s.persistGoalStepComplete(p.StepID, p.Title, p.Agent, p.Status, p.Summary)
		}
	}
	if m.Phase == "failed" && m.Error != "" {
		s.logError("goal failed: " + m.Error)
		s.persistGoalLifecycle("goal failed: " + m.Error)
	}
	if m.Phase == "done" && prevPhase != "done" {
		s.announceGoalComplete()
	}
	if m.Phase == "synthesizing" && prevPhase != "synthesizing" {
		s.persistGoalLifecycle("Workers finished — writing completion summary…")
	}
	if (m.Phase == "deploying" || m.Phase == "running") && prevPhase != m.Phase {
		if prevPhase == "plan_ready" || prevPhase == "" || prevPhase == "planning" {
			s.persistGoalLifecycle(fmt.Sprintf("Goal deploy running (%s)…", m.Phase))
		}
	}
	s.layout()
}

// goalKeepsBusy mirrors the web reducer's goalKeepsStreaming: goal deploy,
// CEO verify/replan loops, and the post-deploy synthesizing turn outlive the
// planning model's `done`.
func (s *session) goalKeepsBusy() bool {
	if s.goalState == nil {
		return false
	}
	switch s.goalState.Phase {
	case "planning", "reviewing", "deploying", "running", "synthesizing",
		"verifying", "replanning":
		return true
	case "plan_ready":
		return s.goalState.AutoDeploy
	}
	return false
}

// clearBlockingPrompts drops ask/sudo/approval/intercom flyouts that must not
// outlive a turn boundary. Without this, Ctrl+C abort could clear busy while
// leaving an orphan ask flyout (scout-ask-wedge).
func (s *session) clearBlockingPrompts() {
	s.pendingAsk = nil
	s.pendingSudo = nil
	s.pendingApproval = nil
	s.pendingIntercom = nil
	s.intercomQueue = nil
	s.overlayPress = overlayPress{}
	s.askBoxRows = nil
	s.sudoBoxRows = nil
}

// disarmAbortTimeout invalidates any pending abortBusyTimeout tick so a late
// timer cannot clear busy after the core already answered (or a new turn began).
func (s *session) disarmAbortTimeout() {
	s.abortGen++
}

// armAbortTimeout starts a watchdog after the user cancels. If core never emits
// aborted/done, release busy so "working" cannot stick forever on a wedged core.
func (s *session) armAbortTimeout() tea.Cmd {
	s.abortGen++
	gen := s.abortGen
	return tea.Tick(abortBusyTimeout, func(time.Time) tea.Msg {
		return abortTimeoutMsg{gen: gen}
	})
}

func (s *session) handleCoreEvent(ev *coreEvent) tea.Cmd {
	switch ev.Type {
	case "ready":
		s.coreReady = true // disarm the startup watchdog
		s.coreLifecycle = coreReady
		s.coreFailure = ""
		var models []modelInfo
		var m map[string]json.RawMessage
		if err := json.Unmarshal(ev.Raw, &m); err == nil {
			if raw, ok := m["models"]; ok {
				_ = json.Unmarshal(raw, &models)
			}
			if a, ok := m["authed"]; ok {
				var b bool
				_ = json.Unmarshal(a, &b)
				s.authed = b
			}
			// anyLoggedIn unlocks multi-provider sends even when the active
			// provider itself has no key (stale settings / expired OAuth).
			if raw, ok := m["anyLoggedIn"]; ok {
				var b bool
				if err := json.Unmarshal(raw, &b); err == nil {
					s.anyLoggedIn = b
				}
			} else {
				// Older cores omit the field — fall back to active-provider auth.
				s.anyLoggedIn = s.authed
			}
			if raw, ok := m["approval"]; ok {
				var mode string
				_ = json.Unmarshal(raw, &mode)
				s.approvalModeStr = mode
			}
			if raw, ok := m["bash_timeout_secs"]; ok {
				var n int
				_ = json.Unmarshal(raw, &n)
				if n > 0 {
					s.coreBashTimeout = n
				}
			}
			if raw, ok := m["auto_compact"]; ok {
				var b bool
				_ = json.Unmarshal(raw, &b)
				s.coreAutoCompact = b
			}
			// Provider fields (openai/anthropic endpoints).
			if raw, ok := m["provider"]; ok {
				_ = json.Unmarshal(raw, &s.activeProvider)
			}
			if raw, ok := m["providerKind"]; ok {
				_ = json.Unmarshal(raw, &s.providerKind)
			}
			if raw, ok := m["providers"]; ok {
				_ = json.Unmarshal(raw, &s.providers)
			}
			if raw, ok := m["providerPresets"]; ok {
				_ = json.Unmarshal(raw, &s.providerPresets)
			}
			s.providerHasKey = s.authed // ready's authed reflects the active provider's key
			// Models already in the ready payload imply at least one logged-in
			// provider (discovery only runs for those). Prefer the explicit flag,
			// but never leave anyLoggedIn false when models arrived.
			if !s.anyLoggedIn && len(models) > 0 {
				s.anyLoggedIn = true
			}
			s.applyReadySandboxFields(m)
		}
		// Bind a pre-provider-era global key to the provider reported at startup.
		// This must happen before any later switch can change activeProvider.
		s.migrateLegacyProviderKey(s.activeProvider)
		s.applyModels(models)
		s.logInfo(fmt.Sprintf("%d model(s) discovered", len(models)))
		// Settings.json is the source of truth for approval + runtime knobs.
		// Re-apply after ready so a stale core default / config-file layer can't
		// silently diverge from what the TUI persisted (and so crash-restarts
		// keep the user's choice).
		desired := normalizeApproval(s.settings.Approval)
		s.settings.Approval = desired
		if s.approvalModeStr != desired {
			s.sendCore(map[string]any{"type": "set_approval", "mode": desired})
		}
		s.approvalModeStr = desired
		if s.settings.BashTimeoutSecs > 0 && s.coreBashTimeout != s.settings.BashTimeoutSecs {
			s.coreBashTimeout = s.settings.BashTimeoutSecs
			s.sendCore(map[string]any{"type": "set_config", "key": "bash_timeout_secs", "value": s.settings.BashTimeoutSecs})
		} else if s.settings.BashTimeoutSecs > 0 {
			s.coreBashTimeout = s.settings.BashTimeoutSecs
		}
		if s.coreAutoCompact != s.settings.AutoCompact {
			s.coreAutoCompact = s.settings.AutoCompact
			s.sendCore(map[string]any{"type": "set_config", "key": "auto_compact", "value": s.settings.AutoCompact})
		} else {
			s.coreAutoCompact = s.settings.AutoCompact
		}
		s.sendCore(map[string]any{"type": "set_config", "key": "advisor.enabled", "value": s.settings.AdvisorEnabled})
		s.sendCore(map[string]any{"type": "set_config", "key": "advisor.nudge", "value": s.settings.AdvisorNudge})
		s.sendCore(map[string]any{"type": "set_config", "key": "advisor.subagents", "value": s.settings.AdvisorSubagents})
		if s.settings.AdvisorModel != "" {
			s.sendCore(map[string]any{"type": "set_config", "key": "advisor.model", "value": s.settings.AdvisorModel})
		}
		if s.settings.AdvisorSubagentModel != "" {
			s.sendCore(map[string]any{"type": "set_config", "key": "advisor.subagent_model", "value": s.settings.AdvisorSubagentModel})
		}
		// Populate plugin slash commands for the palette (list_plugins also
		// emits plugin_commands as a companion; this covers cold start).
		s.sendCore(map[string]any{"type": "list_plugin_commands"})
		// The viewport may currently contain the pre-ready "Starting…" welcome.
		// Auth/model state changes do not add a transcript block, so explicitly
		// rebuild it now; otherwise the cached startup screen remains forever even
		// though the footer and composer have advanced to ready.
		s.layout()
		// Sync a persisted provider selection that differs from the core's
		// startup choice (e.g. switched in a previous session). The core emits
		// provider_changed + models, which re-resolves the key below.
		if s.settings.ActiveProvider != "" && s.settings.ActiveProvider != s.activeProvider &&
			s.containsProvider(s.settings.ActiveProvider) {
			s.sendCore(map[string]any{"type": "set_provider", "name": s.settings.ActiveProvider})
			// Re-arm the core-event pump: returning nil here schedules no further
			// waitForEvent, so no core event would ever be processed again — the
			// spinner keeps ticking but nothing streams (TUI deadlock). The
			// set_provider round-trip is async; provider_changed will arrive and
			// be handled normally on the next pump tick.
			return waitForEvent(s.coreEvents, s.coreStartGen)
		}
		s.reauthActiveProvider()
		// First-run auth: open /login once when NO provider is logged in so the
		// user isn't forced to burn a failed send to discover the path. Multi-
		// provider setups with a broken active provider but working secondary
		// keys must still be able to send (canSend uses anyLoggedIn).
		if !s.canSend() && !s.loginOffered && s.modal.kind == modalNone {
			s.loginOffered = true
			s.openLoginPicker()
		}

	case "provider_changed":
		var m map[string]json.RawMessage
		if err := json.Unmarshal(ev.Raw, &m); err == nil {
			s.activeProvider = get(m, "provider")
			s.providerKind = get(m, "kind")
			if hk := get(m, "has_key"); hk != "" {
				var b bool
				_ = json.Unmarshal([]byte(hk), &b)
				s.providerHasKey = b
				// OAuth login finalizes with provider_changed + has_key=true but
				// historically omitted the separate `authed` event. Without this,
				// prompt send stays blocked after SuperGrok / Claude / Gemini OAuth.
				// Mirror has_key into authed so logout/switch-without-key also clears it.
				s.authed = b
				// Gaining a key on any provider unlocks multi-provider sends.
				if b {
					s.anyLoggedIn = true
				} else if !s.containsOtherLoggedInProvider() && len(s.models) == 0 {
					// Losing the only usable provider fully signs the session out.
					s.anyLoggedIn = false
				}
				// else: keep anyLoggedIn — other owners still have models/keys.
			}
		}
		// A persisted provider may be applied asynchronously after ready. The
		// migration helper requires it to match the previously persisted owner,
		// so this cannot bind a legacy secret to an arbitrary newly-selected one.
		s.migrateLegacyProviderKey(s.activeProvider)
		// Persist the selection so the next session restores it.
		if s.activeProvider != "" && s.settings.ActiveProvider != s.activeProvider {
			s.settings.ActiveProvider = s.activeProvider
			_ = s.settings.save()
		}
		s.reauthActiveProvider()
		s.layout()

	case "sandbox_status":
		// {mode, report} — the core's preflight reply; refreshes the status
		// panel and resolves a pending enable request.
		var m map[string]json.RawMessage
		if json.Unmarshal(ev.Raw, &m) == nil {
			s.onSandboxStatusEvent(m)
		}

	case "sandbox_prepare_progress":
		// {phase} — runtime/image download progress.
		var m map[string]json.RawMessage
		if json.Unmarshal(ev.Raw, &m) == nil {
			s.onSandboxPrepareProgressEvent(m)
		}

	case "sandbox_ready":
		// {ready, report} — preparation finished (or an admin step still pending).
		var m map[string]json.RawMessage
		if json.Unmarshal(ev.Raw, &m) == nil {
			s.onSandboxReadyEvent(m)
		}

	case "sandbox_error":
		// {error} — a setup/runtime failure. Fail-closed: never fall back to host.
		var m map[string]json.RawMessage
		if json.Unmarshal(ev.Raw, &m) == nil {
			s.onSandboxErrorEvent(m)
		}

	case "authed":
		// Honor ok=false (e.g. future unauth signals); default missing ok to true.
		ok := true
		if raw := ev.get("ok"); raw != "" && raw != "null" {
			if raw == "false" || raw == "0" {
				ok = false
			}
		}
		s.authed = ok
		s.providerHasKey = ok
		if ok {
			s.anyLoggedIn = true
			s.oauth = nil
			s.logSuccess("authenticated")
		} else if len(s.models) == 0 {
			// No models left and active provider lost its key → fully signed out.
			s.anyLoggedIn = false
		}
		s.refresh()

	case "provider_presets":
		// The core advertises the first-party presets (and refreshes them after
		// add_provider so Configured/HasKey flip). Keep the picker open if it's
		// up so the list updates live.
		var presets []providerPreset
		if raw := ev.get("presets"); raw != "" {
			_ = json.Unmarshal([]byte(raw), &presets)
		} else {
			var m map[string]json.RawMessage
			if err := json.Unmarshal(ev.Raw, &m); err == nil {
				if raw, ok := m["presets"]; ok {
					_ = json.Unmarshal(raw, &presets)
				}
			}
		}
		s.providerPresets = presets
		// Always repaint so footer / next /login open shows ✓ without a restart.
		s.refresh()

	case "models":
		// The core emits this after a provider switch (and on demand). Re-apply the
		// model list + persisted selection exactly like the ready path.
		var models []modelInfo
		if raw := ev.get("models"); raw != "" {
			_ = json.Unmarshal([]byte(raw), &models)
		} else {
			var m map[string]json.RawMessage
			if err := json.Unmarshal(ev.Raw, &m); err == nil {
				if raw, ok := m["models"]; ok {
					_ = json.Unmarshal(raw, &models)
				}
			}
		}
		s.applyModels(models)
		s.logInfo(fmt.Sprintf("%d model(s) discovered", len(models)))
		s.refresh()

	case "models_refreshed":
		// Terminal event for the on-demand /refresh (core re-emitted `models`
		// first, so the list is already updated here). Just confirm to the user.
		count := ev.get("count")
		s.logInfo(fmt.Sprintf("model list refreshed (%s models)", count))
		s.refresh()

	case "provider_models_preview":
		// A discover_provider_models result (preview for the add-custom-provider
		// form). Only act when that modal is open AND still waiting on this
		// generation — ignore late results after cancel/close so the form
		// never re-locks.
		if s.modal.kind != modalCustomProvider {
			return nil
		}
		d := &s.customProvider
		if !d.discovering {
			return nil
		}
		var models []modelInfo
		if raw := ev.get("models"); raw != "" {
			_ = json.Unmarshal([]byte(raw), &models)
		} else {
			var m map[string]json.RawMessage
			if err := json.Unmarshal(ev.Raw, &m); err == nil {
				if raw, ok := m["models"]; ok {
					_ = json.Unmarshal(raw, &models)
				}
			}
		}
		d.previewModels = models
		d.discovering = false
		if len(models) == 0 {
			errMsg := ev.get("error")
			if errMsg == "" {
				errMsg = "no models discovered — check the base URL / key / kind (Add provider still works)"
			}
			s.modal.loadError = errMsg
		} else {
			cpSeedModelCaps(d)
			d.field = cpFieldModels
			d.modelCursor = 0
			d.modelScroll = 0
			d.modelField = cpCapContext
			s.modal.loadError = ""
			s.logInfo(fmt.Sprintf("%d model(s) discovered — refine caps or submit", len(models)))
		}
		s.refresh()

	case "delta":
		// Assistant text token from core (Event::new("delta").with("text",…),
		// provider.rs). Each delta appends to the live assistant block and
		// schedules a coalesced refresh — without this case the streaming
		// reply is silently dropped and only tool/thinking blocks render.
		if s.cur == nil || s.cur.kind != blkAssistant {
			s.push(blkAssistant)
		}
		if s.modelIdx >= 0 && s.modelIdx < len(s.models) {
			s.cur.model = s.models[s.modelIdx].ID
		}
		s.cur.appendText(ev.get("text"))
		return s.scheduleStreamRefresh()

	case "thinking":
		if s.cur == nil || s.cur.kind != blkThinking {
			s.push(blkThinking)
		}
		s.cur.appendText(ev.get("text"))
		return s.scheduleStreamRefresh()

	case "tool_call":
		name := ev.get("name")
		sub := strings.HasPrefix(name, "spawn:") || strings.HasPrefix(name, "subagent:") // sub-agent internal call
		if sub {
			name = strings.TrimPrefix(strings.TrimPrefix(name, "spawn:"), "subagent:")
		}
		b := s.logTool(name, ev.get("args"), sub)
		b.id = ev.get("id")
		if name == "todo_write" {
			// Capture the latest todo list so the pinned panel always reflects
			// current state (the agent rewrites the full list each call).
			s.captureTodos(b.args)
			s.layout() // todo panel may have appeared/grown
		}
		if !sub && (name == "spawn" || name == "subagent") {
			s.layout() // a scout started: make room for the active-tasks panel
		}

	case "tool_result":
		out := ev.get("output")
		id := ev.get("id")
		// Match the result to its in-flight tool block by id. spawn nests its
		// sub-agent calls below the parent scout, so the parent's result doesn't
		// land on the last block — positional matching would misattribute it.
		var match *block
		for i := len(s.blocks) - 1; i >= 0; i-- {
			b := s.blocks[i]
			if b.kind == blkTool && b.dur == 0 && (id == "" || b.id == id) {
				match = b
				break
			}
		}
		if match != nil {
			match.output = capOutput(out)
			match.diff = capStored(ev.get("diff"))
			match.ok = ev.get("ok") == "true"
			match.hasOk = true
			match.dur = time.Since(match.started)
			s.cur = nil
			wasScout := !match.sub && (match.name == "spawn" || match.name == "subagent")
			// Single rebuild path: invalidate → height adjust if scout panel
			// released → one refresh. Never refresh() then layout().
			s.invalidateAll()
			if wasScout {
				s.relayoutHeights()
			}
			s.refresh()
		} else {
			s.logToolResult(out)
		}

	case "bash_execution":
		// User-initiated `!cmd` / `!!cmd` — render like a completed bash tool card.
		// Must set dur > 0 and attach output on the tool block itself; dur==0 means
		// in-flight and the spinner runs forever (isInFlight).
		cmd := ev.get("command")
		out := ev.get("output")
		ok := ev.get("ok") == "true"
		exclude := ev.get("exclude_from_context") == "true"
		args, _ := json.Marshal(map[string]string{"command": cmd})
		b := s.logTool("bash", string(args), false)
		if exclude {
			out = "(not added to model context)\n" + out
		}
		b.output = capOutput(out)
		b.hasOk = true
		b.ok = ok
		b.dur = time.Since(b.started)
		if b.dur == 0 {
			b.dur = time.Millisecond
		}
		s.invalidateAll()
		s.refresh()

	case "done":
		s.disarmAbortTimeout()
		s.clearBlockingPrompts()
		// Do not wipe live subagent shelf while goal deploy / synthesizing is
		// still running — a planning-turn `done` must not blank the UI mid-goal.
		if !s.goalKeepsBusy() {
			s.subProgress = nil
		}
		// When every task is complete, dismiss the pinned tasks panel — a
		// finished plan shouldn't linger as a permanent fixture. Done before
		// the layout() calls below so the cleared state renders immediately.
		// A later todo_write (new work) re-shows it.
		if s.allTodosComplete() {
			s.todos = nil
		}
		if s.queuedNext {
			// A follow-up or steer turn begins right after this one; stay busy so the
			// footer keeps streaming and the input stays live.
			s.queuedNext = false
			s.queued = nil
			s.cur = nil
			s.finalizeInFlight("")
			s.layout()
			s.logInfo("continuing queued turn…")
		} else if s.goalKeepsBusy() {
			// Planning (or an earlier wave) ended, but goal deploy / synthesizing
			// is still live — keep the working footer until the goal completes.
			s.cur = nil
			s.finalizeInFlight("")
			s.layout()
		} else {
			s.busy = false
			s.turnCount++
			s.coreRestarts = 0 // P1-17: a completed turn resets the crash-restart budget
			s.cur = nil
			s.finalizeInFlight("[no result]")
			s.layout()
			s.logSuccess(fmt.Sprintf("turn %d complete", s.turnCount))
			s.input.Focus()
			// Ring the window-title bell: the request finished and the user may
			// be in another window. Any keypress silences it.
			s.titleBell = true
		}
		// Refresh the discoverable-skills list so a skill created mid-turn
		// (e.g. by /reflect or /index) shows up in the /skill:<name> autocomplete.
		s.sendCore(map[string]any{"type": "list_skills"})

	case "aborted":
		s.disarmAbortTimeout()
		s.clearBlockingPrompts()
		s.subProgress = nil
		if s.queuedNext {
			// A steer interrupted this turn; the steered turn runs next. Clear the
			// queued flag here so the steered turn's terminal `done` falls through
			// to the else branch and clears `busy` — otherwise `busy` stays true
			// forever and the TUI wedges (only Ctrl+C recovers) (P0-5).
			s.queuedNext = false
			s.queued = nil
			s.cur = nil
			s.finalizeInFlight("")
			s.layout()
		} else {
			s.busy = false
			s.cur = nil
			s.finalizeInFlight("[aborted]")
			s.layout()
			s.logWarn("aborted")
			s.input.Focus()
		}

	case "reset":
		s.disarmAbortTimeout()
		s.clearBlockingPrompts()
		s.busy = false // a reset is a conversation boundary — no turn is in flight
		s.blocks = nil
		s.cur = nil
		s.contextTokens = 0
		s.lastCachePct = 0
		s.tokensSaved = 0
		s.summaryChars = 0
		s.subProgress = nil
		s.todos = nil
		s.queued = nil
		s.queuedNext = false
		s.follow = true
		s.invalidateAll()
		s.layout()
		s.logInfo("conversation reset")

	case "cleared":
		// In-memory clear from core `/clear` (session file may still persist).
		s.disarmAbortTimeout()
		s.clearBlockingPrompts()
		s.busy = false
		s.blocks = nil
		s.cur = nil
		s.contextTokens = 0
		s.lastCachePct = 0
		s.tokensSaved = 0
		s.summaryChars = 0
		s.subProgress = nil
		s.todos = nil
		s.queued = nil
		s.queuedNext = false
		s.follow = true
		s.invalidateAll()
		s.layout()
		s.logInfo("conversation cleared")

	case "discard_partial":
		// Top-level stream discard (Codex retry path); same effect as
		// http_retry with discard_partial=true.
		s.discardPartialStreamOutput()

	case "history":
		// Loading a session / undo is a conversation boundary — clear any in-flight
		// turn/queue so a mid-turn /load or /sessions doesn't wedge the TUI with
		// busy=true and a wiped transcript.
		s.disarmAbortTimeout()
		s.clearBlockingPrompts()
		s.busy = false
		s.cur = nil
		s.queuedNext = false
		s.queued = nil
		if raw, ok := ev.rawKey("messages"); ok {
			var msgs []map[string]json.RawMessage
			if json.Unmarshal(raw, &msgs) == nil {
				s.rebuildBlocksFromHistory(msgs)
				msgs = nil // release JSON graph; blocks now own the text (D-003)
				// Show the loaded session's used context immediately instead
				// of waiting for the first turn's metrics event.
				if ti, err := strconv.ParseUint(ev.get("tokens_in"), 10, 64); err == nil {
					s.contextTokens = ti
				}
				s.follow = true
				s.invalidateAll()
				s.refresh()
			}
		}
	case "compacting":
		// Pre-compaction warning: the core is about to summarize/drop history.
		// Shown as a toast so the pause isn't a mystery (esp. on slow providers
		// where the summarize call can take several seconds).
		trigger := ev.get("trigger")
		if trigger == "" {
			trigger = "auto"
		}
		s.logInfo(fmt.Sprintf("compacting context (%s)…", trigger))

	case "compacted":
		if ev.get("scope") == "subagent" {
			break // subagent-internal compaction; don't clutter the main transcript
		}
		// The footer's context budget is driven by metrics events, which only
		// fire at turn end. Reflect the post-compaction size now so the bar
		// doesn't keep showing a stale, over-full count after a compact/digest.
		if at, err := strconv.ParseUint(ev.get("after_tokens"), 10, 64); err == nil {
			s.contextTokens = at
		}
		s.accumulateSaved(ev)
		if n, err := strconv.Atoi(ev.get("summary_chars")); err == nil && n >= 0 {
			s.summaryChars = n
		}
		s.logInfo(fmt.Sprintf("context compacted: %s → %s tokens", ev.get("before_tokens"), ev.get("after_tokens")))

	case "digested":
		if at, err := strconv.ParseUint(ev.get("after_tokens"), 10, 64); err == nil {
			s.contextTokens = at
		}
		s.accumulateSaved(ev)
		s.logInfo(fmt.Sprintf("reclaimed stale tool payload(s): %s, %s → %s tokens", ev.get("results"), ev.get("before_tokens"), ev.get("after_tokens")))

	case "reflecting":
		// The auto-reflect seam fired: instead of completing on `finish`, the core
		// injected a reflection continuation so durable facts (memory) and recurring
		// patterns (skills) get persisted without relying on the model remembering
		// to. Surfaced so the post-finish model activity isn't a mystery.
		if n := ev.get("recurrence"); n != "" && n != "0" {
			s.logInfo(fmt.Sprintf("auto-reflect: reflecting on this turn (%s recurring patterns)…", n))
		} else {
			s.logInfo("auto-reflect: reflecting on this turn…")
		}

	case "summary_required":
		// Post-reflect finish without a user-facing completion summary. Core is
		// re-prompting the model; surface so the extra stream isn't a mystery.
		attempt := ev.get("attempt")
		maxAttempts := ev.get("max_attempts")
		if attempt != "" && maxAttempts != "" {
			s.logInfo(fmt.Sprintf("auto-reflect: summary required before finish (attempt %s/%s)…", attempt, maxAttempts))
		} else {
			s.logInfo("auto-reflect: summary required before finish…")
		}

	case "approval_changed":
		// Core emits two shapes:
		//   "never"|"destructive"|"always"  — session gate mode changed
		//   "<kind>:always"                 — user hit "always allow" for one
		//                                    tool kind (escalation); NOT a mode
		//                                    change. Treating it as a mode made
		//                                    the footer flip to "destructive"
		//                                    (normalizeApproval fallback) and
		//                                    look like the user's /approval
		//                                    never preference had been wiped.
		mode := ev.get("mode")
		if strings.Contains(mode, ":") {
			s.logInfo(fmt.Sprintf("approval escalation: %s", mode))
			break
		}
		s.approvalModeStr = normalizeApproval(mode)
		s.logInfo(fmt.Sprintf("approval mode: %s", s.approvalModeStr))

	case "config_changed":
		s.logInfo(fmt.Sprintf("config: %s = %s", ev.get("key"), ev.get("value")))

	case "http_retry":
		s.logInfo(fmt.Sprintf("retry #%s %s (%s ms)", ev.get("attempt"), ev.get("status"), ev.get("backoff_ms")))
		// Mid-stream transport retries re-POST the same turn. Core already
		// cleared its own accumulators; drop any partial assistant/thinking
		// text the failed attempt streamed so a successful retry cannot
		// append a second copy. Tool calls already dispatched this turn are
		// left alone — the agent loop owns them and re-running side effects
		// is unsafe.
		if raw, ok := ev.rawKey("discard_partial"); ok && strings.TrimSpace(string(raw)) == "true" {
			s.discardPartialStreamOutput()
		}

	case "metrics":
		s.lastMetrics = ev.Raw
		// tokens_in is the live context size (prompt + output). During a turn the
		// core emits periodic estimates so the footer moves; at turn end the real
		// usage overwrites them. prompt_tokens (when present) is the prompt-only
		// count, used for the cached-fraction denominator.
		if ti, err := strconv.ParseUint(ev.get("tokens_in"), 10, 64); err == nil {
			s.contextTokens = ti
		}
		// The mid-stream metrics event omits cached_tokens; it only lands at turn
		// end. Capture the per-turn cache hit rate here (cached / prompt_tokens,
		// falling back to tokens_in) so renderMetrics can keep showing a cache %
		// while the *next* turn is in flight — carried and marked "~".
		if cached := ev.get("cached_tokens"); cached != "" && cached != "null" && cached != "0" {
			tin := ev.get("prompt_tokens")
			if tin == "" || tin == "null" || tin == "0" {
				tin = ev.get("tokens_in")
			}
			if tin != "" && tin != "null" && tin != "0" {
				if cN, err := strconv.ParseUint(cached, 10, 64); err == nil && cN > 0 {
					if tN, err := strconv.ParseUint(tin, 10, 64); err == nil && tN > 0 {
						s.lastCachePct = int(cN * 100 / tN)
					}
				}
			}
		}

	case "umans_conc":
		// Live account-wide concurrency from the Umans gateway's /v1/usage poll
		// (core background task). used=nil => not Umans / fetch failed → hide;
		// used set + limit=nil => unlimited → render ∞. `provider` is the Umans
		// provider the poll tracks; renderUmansConc only shows the field when the
		// selected model routes to it (a Gemini/OpenAI model selected → hidden).
		s.umansConcUsed = nullableInt64(ev.get("used"))
		s.umansConcLimit = nullableInt64(ev.get("limit"))
		s.umansConcProvider = ev.get("provider")

	case "approval_request":
		owner := "approval:" + ev.get("request_id")
		s.suspendComposer(owner)
		args, diff := capStored(ev.get("args")), capStored(ev.get("diff"))
		s.pendingApproval = &approvalPrompt{
			requestID:  ev.get("request_id"),
			tool:       ev.get("tool"),
			args:       args,
			diff:       diff,
			receivedAt: time.Now(),
		}
		s.logApproveDiff(ev.get("tool"), args, diff)
		s.input.Focus()
	case "ask_request":
		// The model called the `ask` tool and is blocking on the user's
		// answers. Parse the questions into a flyout and render it; the core
		// waits for `ask_reply` (sent by sendAskReply on submit/skip). rawKey
		// is required: ev.get unmarshals into a string, which fails for an
		// array and returns "" — the flyout would never open (the original
		// bug: ask.go existed but no event case ever called parseAskRequest).
		qraw, ok := ev.rawKey("questions")
		if !ok {
			qraw = json.RawMessage("[]")
		}
		if a := parseAskRequest(ev.get("request_id"), qraw); a != nil {
			s.pendingAsk = a
			s.input.Blur()
			s.logInfo(fmt.Sprintf("❓ agent asks: %d question%s — answer the prompt",
				len(a.questions), pluralS(len(a.questions))))
			s.layout()
		}
	case "sudo_request":
		// The agent wants to run a bash command that invokes `sudo`. sudo would
		// open /dev/tty to read the password, garbling the TUI — so the core
		// blocks here and surfaces it to the user. On Enter the password is fed
		// via `sudo -S` on stdin; on Esc the agent is told the request was
		// declined (command NOT run). There is intentionally no user-facing
		// timeout: password-manager and assistive workflows may take longer.
		if rid := ev.get("request_id"); rid != "" {
			s.pendingSudo = newSudoPrompt(rid, ev.get("command"))
			s.input.Blur()
			s.logInfo("🔐 sudo command requested — approve or decline")
			s.layout()
		}
		return waitForEvent(s.coreEvents, s.coreStartGen)
	case "intercom_message":
		// A subagent is prompting the orchestrator for a decision (or a progress
		// update). need_decision blocks until we reply; progress_update is a log line.
		reason := ev.get("reason")
		if reason == "progress_update" {
			s.logInfo(fmt.Sprintf("⟵ %s (progress): %s", ev.get("from"), ev.get("message")))
		} else {
			p := &intercomPrompt{
				requestID: ev.get("id"),
				from:      ev.get("from"),
				reason:    reason,
				message:   ev.get("message"),
			}
			s.enqueueIntercom(p)
			s.logWarn(fmt.Sprintf("⟵ subagent %s asks: %s", ev.get("from"), ev.get("message")))
			s.layout()
		}

	case "subagent_progress":
		runID := ev.get("run_id")
		if ev.get("phase") == "done" {
			entry := s.findSubProgress(runID)
			agent := ev.get("agent")
			if agent == "" && entry != nil {
				agent = entry.agent
			}
			ok := ev.get("ok") != "false"
			// During goal deploy, lasting finals come from goal_state /
			// goal_step_complete. Outside goal mode, persist a one-liner so
			// manual subagent runs are not toast-only.
			if agent != "" && (s.goalState == nil || !goalShowsProgressPanel(s.goalState.Phase, s.goalState.AutoDeploy)) {
				if ok {
					s.logPersist(blkSuccess, fmt.Sprintf("✓ subagent %s finished", agent))
				} else {
					s.logPersist(blkWarn, fmt.Sprintf("✗ subagent %s finished", agent))
				}
			}
			if runID != "" {
				s.removeSubProgress(runID)
			}
			// Entry removed → shelf height may shrink; do not re-wrap transcript.
			s.relayoutHeights()
			break
		}
		entry := s.findSubProgress(runID)
		if entry == nil {
			entry = &subProgressEntry{runID: runID, agent: ev.get("agent"), started: time.Now()}
			s.subProgress = append(s.subProgress, entry)
			// New shelf row → height only; View rebuilds chrome without SetContent.
			s.relayoutHeights()
		}
		if tc := ev.get("tool_count"); tc != "" {
			if n, err := strconv.Atoi(tc); err == nil {
				entry.toolCount = n
			}
		}
		if ti := ev.get("tokens_in"); ti != "" {
			if n, err := strconv.ParseUint(ti, 10, 64); err == nil {
				entry.tokensIn = n
			}
		}
		if to := ev.get("tokens_out"); to != "" {
			if n, err := strconv.ParseUint(to, 10, 64); err == nil {
				entry.tokensOut = n
			}
		}
		switch ev.get("phase") {
		case "tool":
			entry.curTool = ev.get("tool")
			entry.toolStart = time.Now()
			entry.toolRunning = true
		case "tool_end":
			entry.toolRunning = false
			entry.curTool = ev.get("tool")
		case "streaming":
			entry.toolRunning = false
		}
		// curTool/counts only change the activity shelf — Bubble Tea still
		// paints View after this Update; skip transcript renderBlocks/SetContent.

	case "advisor_note":
		detail := ev.get("finding")
		if detail == "" {
			detail = ev.get("message")
		}
		s.recordAdvisorReview(
			ev.get("scope"), ev.get("advisor"), ev.get("model"),
			"finding", detail, ev.get("severity"), 0,
		)

	case "advisor_status":
		state := ev.get("state")
		if state == "duplicate" {
			break
		}
		detail := ev.get("reason")
		var elapsed time.Duration
		if ms, err := strconv.ParseInt(ev.get("elapsed_ms"), 10, 64); err == nil && ms > 0 {
			elapsed = time.Duration(ms) * time.Millisecond
		}
		s.recordAdvisorReview(
			ev.get("scope"), ev.get("advisor"), ev.get("model"),
			state, detail, "", elapsed,
		)
	case "info":
		// Informational notices from the core (first-run staging, subagent
		// lifecycle, plugin handoffs, etc.). Surface them in the transcript.
		if msg := ev.get("message"); msg != "" {
			low := strings.ToLower(msg)
			// Goal deploy bridge lines must persist — toast-only left a dark gap.
			if strings.Contains(low, "goal deploy") ||
				strings.Contains(low, "writing completion summary") ||
				strings.Contains(low, "completion summary") {
				s.persistGoalLifecycle(msg)
			} else {
				s.logInfo(msg)
			}
			// OAuth finalize messages — belt-and-suspenders if `authed` was
			// missed so the user can type immediately after SuperGrok login.
			if strings.Contains(low, "logged into") && strings.Contains(low, "oauth") {
				s.authed = true
				s.providerHasKey = true
				s.anyLoggedIn = true
				s.logSuccess("authenticated (OAuth)")
			}
		}

	case "oauth_prompt":
		// The core needs the user to complete an interactive OAuth login (visit a
		// URL and, for the device flow, enter a code). Sticky banner + clipboard
		// instead of dumping a hard-wrapped URL wall into the transcript.
		url := ev.get("url")
		code := ev.get("code")
		message := ev.get("message")
		if message == "" {
			message = "complete the OAuth login in your browser"
		}
		clipCmd := writeOSC52Cmd(url)
		if url != "" || code != "" || message != "" {
			s.oauth = &oauthBanner{message: message, url: url, code: code}
			s.layout()
		}
		if url != "" {
			s.logInfo("OAuth URL copied — paste into a browser (or copy from the banner)")
		} else {
			s.logInfo(message)
		}
		// Progress heartbeats re-use oauth_prompt without a new URL — don't
		// re-open the browser on every "still waiting…" tick.
		if url != "" && !strings.Contains(strings.ToLower(message), "still waiting") {
			openURL(url)
		}
		// CRITICAL: always re-arm waitForEvent. Returning only clipCmd used to
		// drop the core event pump after the first oauth_prompt — so the later
		// "logged into … OAuth" / authed / models events sat unread until a TUI
		// restart. That made SuperGrok login look stuck after browser approval.
		if clipCmd != nil {
			return tea.Batch(clipCmd, waitForEvent(s.coreEvents, s.coreStartGen))
		}
		// fall through to the shared waitForEvent return below

	case "steer":
		// Core acknowledged a steer: the running turn was interrupted and the
		// steered turn is starting. The user message was already logged on send,
		// so just mark the redirect.
		s.logInfo("steering…")

	case "sessions":
		var entries []sessionEntry
		var m map[string]json.RawMessage
		if err := json.Unmarshal(ev.Raw, &m); err == nil {
			if raw, ok := m["sessions"]; ok {
				_ = json.Unmarshal(raw, &entries)
			}
		}
		s.sessionList = entries
		if s.modal.kind == modalSessions {
			s.modal.loading = false
			s.modal.loadError = ""
			s.rebuildSessionsPickerList()
		}

	case "session_changed":
		path := ev.get("path")
		if !commitSessionClaim(path) {
			s.logError("session changed, but its local ownership claim was lost")
		} else if ev.get("new") == "true" {
			s.logSuccess("new session ready")
		} else {
			s.logSuccess("session loaded")
		}

	case "session_change_failed":
		path := ev.get("path")
		cancelSessionReservation(path)
		msg := ev.get("message")
		s.logError("session switch failed: " + msg)
		if s.modal.kind == modalSessions {
			s.modal.loading = false
			s.modal.loadError = msg
		}

	case "session_renamed":
		s.logSuccess("session renamed")
		accepted := s.sendCore(map[string]any{"type": "list_sessions"})
		if s.modal.kind == modalSessions && accepted {
			s.modal.loading = true
		}

	case "session_pinned":
		s.logSuccess("session pin updated")
		accepted := s.sendCore(map[string]any{"type": "list_sessions"})
		if s.modal.kind == modalSessions && accepted {
			s.modal.loading = true
		}

	case "session_deleted":
		s.logSuccess("session deleted")
		accepted := s.sendCore(map[string]any{"type": "list_sessions"})
		if s.modal.kind == modalSessions && accepted {
			s.modal.loading = true
		}

	case "stats":
		// tokens_in is the CURRENT real context (matches the footer); tokens_out
		// is cumulative output. total_in is the cumulative prompt (billing) and
		// drives the cache ratio.
		ti := ev.get("tokens_in")
		to := ev.get("tokens_out")
		totalIn := ev.get("total_in")
		cached := ev.get("cached_tokens")
		ratio := ev.get("cache_hit_ratio")
		turns := ev.get("turns")
		msgs := ev.get("messages")
		sessionFile := ev.get("session_file")
		s.logPersist(blkInfo, fmt.Sprintf("stats: %s in / %s out · %s turns · %s msgs", ti, to, turns, msgs))
		if totalIn != "" && totalIn != "0" {
			s.logPersist(blkInfo, fmt.Sprintf("totals: %s prompt in / %s out (cumulative)", totalIn, to))
		}
		if sessionFile != "" {
			s.logPersist(blkInfo, fmt.Sprintf("session: %s", sessionFile))
		}
		if cached != "" && cached != "0" {
			if ratio != "" {
				if r, err := strconv.ParseFloat(ratio, 64); err == nil {
					s.logPersist(blkSuccess, fmt.Sprintf("cache: %s cached · %.0f%% hit", cached, r*100))
				} else {
					s.logPersist(blkSuccess, fmt.Sprintf("cache: %s cached", cached))
				}
			} else {
				s.logPersist(blkSuccess, fmt.Sprintf("cache: %s cached", cached))
			}
		}

	case "context_breakdown":
		// /context reply: parse the token-usage breakdown and open a modal so
		// the user can see where the context budget is being spent.
		var cb contextBreakdown
		if err := json.Unmarshal(ev.Raw, &cb); err != nil {
			s.logError("failed to parse context breakdown")
			break
		}
		s.ctxBreakdown = &cb
		s.modal.kind = modalContext
		s.modal.editing = false
		s.modal.fieldIdx = 0

	case "usage":
		// /usage reply: provider plan / rate-limit windows for the model the
		// user is on. Open a modal with progress bars per window.
		var ur usageReport
		if err := json.Unmarshal(ev.Raw, &ur); err != nil {
			s.logError("failed to parse usage report")
			break
		}
		s.usageReport = &ur
		s.modal.kind = modalUsage
		s.modal.editing = false
		s.modal.fieldIdx = 0
		if !ur.Available && ur.Message != "" {
			// Also log so headless/log readers see why.
			s.logInfo(ur.Message)
		}
		return tea.Batch(s.rebuildUsageBars(&ur), waitForEvent(s.coreEvents, s.coreStartGen))

	case "goal_state":
		s.applyGoalState(ev.Raw)
	case "protocol_hello":
		// Capabilities handshake — log once for operators / headless.
		if caps := ev.get("capabilities"); caps != "" {
			s.logInfo("protocol capabilities: " + caps)
		} else if ver := ev.get("version"); ver != "" {
			s.logInfo("protocol hello v" + ver)
		}
	case "file_change":
		if p := ev.get("path"); p != "" {
			s.logInfo("file_change: " + p)
		}
	case "checkpoint_created":
		s.logInfo(fmt.Sprintf("checkpoint %s created", ev.get("id")))
	case "checkpoint_restored":
		s.logInfo(fmt.Sprintf("checkpoint %s restored", ev.get("id")))
	case "checkpoints":
		// List payload — no UI yet; ignore quietly.
	case "worktree_ready":
		s.logInfo(fmt.Sprintf("worktree ready: %s", ev.get("path")))
	case "worktree_cleaned":
		// Quiet — cleanup noise.
	case "worktree_promoted":
		s.logInfo(fmt.Sprintf("worktree promoted: %s", ev.get("run_id")))
	case "audit":
		// Opt-in sidecar; quiet in TUI.
	case "cost_update":
		// Footer already shows tokens via metrics.
	case "goal_plan":
		var raw struct {
			Summary    string           `json:"summary"`
			Steps      []map[string]any `json:"steps"`
			Risks      []string         `json:"risks"`
			Validation []string         `json:"validation"`
		}
		if json.Unmarshal(ev.Raw, &raw) == nil {
			s.goalPlan = &goalPlanSnap{Summary: raw.Summary, Steps: raw.Steps, Risks: raw.Risks, Validation: raw.Validation}
		}
		if raw.Summary != "" {
			s.logInfo("plan: " + raw.Summary)
		}
	case "goal_phase":
		from, to := ev.get("from"), ev.get("to")
		msg := ev.get("message")
		// Optional progress fields (core emit_goal_phase_progress) — keep in
		// sync with web reducer so wave/counts are not toast-only gaps.
		wave := ev.get("wave")
		doneCount, stepCount := ev.get("done_count"), ev.get("step_count")
		var extras string
		if wave != "" {
			extras += " · wave " + wave
		}
		if doneCount != "" && stepCount != "" {
			extras += fmt.Sprintf(" (%s/%s)", doneCount, stepCount)
		}
		var line string
		if msg != "" {
			line = fmt.Sprintf("goal %s → %s%s: %s", from, to, extras, msg)
		} else {
			line = fmt.Sprintf("goal %s → %s%s", from, to, extras)
		}
		// Persist lifecycle transitions so deploy is never toast-only dark.
		// "done" is owned by announceGoalComplete (also called from applyGoalState)
		// so we do not persist the verbose transition line or double-toast.
		switch to {
		case "deploying", "running", "synthesizing", "failed", "cancelled",
			"planning", "reviewing", "verifying", "replanning":
			if to == "synthesizing" && msg == "" {
				line = "Workers finished — writing completion summary…"
				if extras != "" {
					line += extras
				}
			}
			s.persistGoalLifecycle(line)
			if to == "failed" || to == "cancelled" {
				s.busy = false
				// Sync local phase so goalKeepsBusy cannot retain busy after a
				// goal_phase terminal that races ahead of goal_state.
				if s.goalState != nil {
					s.goalState.Phase = to
				}
				s.logWarn(line)
				s.titleBell = true // attention: goal did not finish cleanly
			}
		case "done":
			s.busy = false
			if s.goalState != nil {
				s.goalState.Phase = "done"
			}
			s.announceGoalComplete()
		default:
			s.logInfo(line)
		}
		// Keep local phase snap current for non-terminal CEO phases so
		// goalKeepsBusy / progress panel stay correct if goal_state lags.
		if s.goalState != nil {
			switch to {
			case "planning", "reviewing", "deploying", "running", "synthesizing",
				"verifying", "replanning", "plan_ready":
				s.goalState.Phase = to
			}
		}
		if to == "plan_ready" && s.goalState != nil && !s.goalState.AutoDeploy {
			s.openGoalPlanReview()
		}
	case "goal_step_verdict":
		ok := ev.get("ok") == "true"
		out := truncateGoalSummary(ev.get("output"), goalDisplaySummaryCap)
		if ok {
			s.logPersist(blkSuccess, "goal verdict PASS\n"+out)
		} else {
			s.logPersist(blkWarn, "goal verdict FAIL\n"+out)
			s.logWarn("goal step verdict failed")
		}
	case "goal_step_complete":
		// Preferred one-shot signal from core (WP-C1). Defensive: tolerate
		// missing fields until core lands the event.
		status := ev.get("status")
		if status == "" {
			if ev.get("ok") == "false" {
				status = "failed"
			} else {
				status = "done"
			}
		}
		s.persistGoalStepComplete(
			ev.get("step_id"),
			ev.get("title"),
			ev.get("agent"),
			status,
			ev.get("summary"),
		)
	case "goal_completion_summary":
		// Emitted when wrap-up is skipped or as a deterministic Done bridge.
		text := ev.get("text")
		if text == "" {
			text = ev.get("summary")
		}
		text = strings.TrimSpace(text)
		if text == "" || text == "[no result]" {
			text = "Goal finished (no completion summary provided)."
		}
		s.logPersist(blkSuccess, "goal completion\n"+truncateGoalSummary(text, goalDisplaySummaryCap))
	case "memory_saved":
		if msg := ev.get("message"); msg != "" {
			s.logSuccess(msg)
		} else {
			s.logInfo("memory saved")
		}
	case "memory_list":
		var m map[string]json.RawMessage
		var entries []memoryEntry
		if err := json.Unmarshal(ev.Raw, &m); err == nil {
			if raw, ok := m["entries"]; ok {
				_ = json.Unmarshal(raw, &entries)
			}
		}
		s.memoryList = entries
		// Bare /memory and /forget open a pick-to-forget modal instead of
		// requiring `/forget <id>` on the command line.
		if s.pendingMemoryPicker {
			s.pendingMemoryPicker = false
			if s.modal.kind == modalMemory {
				s.modal.loading = false
				s.modal.loadError = ""
			} else {
				s.openMemoryPicker()
			}
			break
		}
		if len(entries) == 0 {
			s.logInfo("no memories saved")
			break
		}
		var rows []string
		rows = append(rows, accentStyle.Render("◆ Memories"))
		for _, e := range entries {
			text := truncateRunes(e.Text, 80)
			id := e.ID
			if id == "" {
				id = "?"
			}
			tags := ""
			if len(e.Tags) > 0 {
				tags = "  " + dimStyle.Render("["+strings.Join(e.Tags, ",")+"]")
			}
			rows = append(rows, mutedStyle.Render(id)+"  "+baseStyle.Render(text)+tags)
		}
		s.logRaw(strings.Join(rows, "\n"))
	case "job_status", "job_wait_result", "job_wait_timeout", "job_list", "job_cancel_requested":
		s.jobStatusRaw = append(s.jobStatusRaw[:0], ev.Raw...)
		s.logInfo("job: " + string(ev.Raw))
	case "session_tree":
		s.sessionTreeRaw = append(s.sessionTreeRaw[:0], ev.Raw...)
		s.logInfo("session tree: " + string(ev.Raw))
	case "session_branch":
		s.logSuccess("switched session branch: " + ev.get("entry_id"))
	case "error":
		msg := ev.get("message")
		// Capture turn-started before logError — pushing an error block finalizes
		// s.cur via push(), which would make a mid-turn error look pre-turn.
		coreDead := strings.Contains(strings.ToLower(msg), "core exited")
		turnStarted := s.cur != nil || s.pendingApproval != nil || s.pendingAsk != nil ||
			s.pendingSudo != nil || s.queuedNext || s.goalKeepsBusy()
		s.logError(msg)
		s.titleBell = true // attention: something failed
		if s.modal.loading {
			s.modal.loading = false
			s.modal.loadError = msg
			s.pendingMemoryPicker = false
			s.pendingPluginPicker = false
			s.pendingVisionPicker = false
		}
		// Core rejected a second queue while the TUI banner already flipped —
		// restore honesty: drop the optimistic queued pointer so Esc won't
		// clear a prompt the core no longer has (or thinks it has the old one).
		if strings.Contains(strings.ToLower(msg), "already queued") {
			// Keep the prior queue banner if we still have one from the first
			// accept; the optimistic overwrite already happened client-side
			// before this error. Without a core echo of the live queue text we
			// can only clear the lie and tell the user to Esc / wait.
			s.queued = nil
			s.queuedNext = false
			s.layout()
		}
		// Mirror web reducer: do NOT always clear busy (mid-turn errors are common).
		// Pre-turn failures (bad skill/model before any assistant/tool activity)
		// must release "working" so the footer cannot stick forever when core
		// emits error without a following done.
		if s.busy && !turnStarted && !coreDead {
			s.busy = false
			s.clearBlockingPrompts()
			s.layout()
			s.input.Focus()
		}
	case "plugin_installed":
		scope := ev.get("scope")
		if scope == "" {
			scope = "global"
		}
		s.logSuccess(fmt.Sprintf("plugin installed (%s): %s v%s — %s", scope, ev.get("name"), ev.get("version"), ev.get("description")))
		s.sendCore(map[string]any{"type": "list_plugin_commands"})
	case "plugin_removed":
		s.logInfo(fmt.Sprintf("plugin removed: %s", ev.get("name")))
		s.sendCore(map[string]any{"type": "list_plugin_commands"})
	case "plugin_enabled":
		s.logInfo(fmt.Sprintf("plugin enabled: %s", ev.get("name")))
		s.sendCore(map[string]any{"type": "list_plugin_commands"})
	case "plugin_disabled":
		s.logInfo(fmt.Sprintf("plugin disabled: %s", ev.get("name")))
		s.sendCore(map[string]any{"type": "list_plugin_commands"})
	case "plugin_error":
		s.logError(fmt.Sprintf("plugin error (%s): %s", ev.get("name"), ev.get("message")))
	case "plugin_trust_prompt":
		// Auto-appears once at startup when undecided plugins exist; re-opens
		// on /plugin-trust (where decided plugins are included too, so the
		// user can change their mind).
		var m map[string]json.RawMessage
		if json.Unmarshal(ev.Raw, &m) == nil {
			if raw, ok := m["plugins"]; ok {
				entries := parsePluginTrustPrompt(raw)
				if len(entries) == 0 {
					s.logInfo("no untrusted project plugins")
				} else {
					s.openPluginTrustModal(entries)
				}
			}
		}
	case "plugin_trust_applied":
		// {trusted: [names], denied: [names], loaded: n} — trusted/denied are
		// arrays, so parse them via rawKey (ev.get returns a raw string for
		// non-scalars).
		var trusted, denied []string
		if raw, ok := ev.rawKey("trusted"); ok {
			_ = json.Unmarshal(raw, &trusted)
		}
		if raw, ok := ev.rawKey("denied"); ok {
			_ = json.Unmarshal(raw, &denied)
		}
		var loaded int
		if raw := ev.get("loaded"); raw != "" && raw != "null" {
			_ = json.Unmarshal([]byte(raw), &loaded)
		}
		msg := "plugin trust updated"
		if len(trusted) > 0 {
			msg += "; trusted: " + strings.Join(trusted, ", ")
		}
		if len(denied) > 0 {
			msg += "; denied: " + strings.Join(denied, ", ")
		}
		if loaded > 0 {
			msg += fmt.Sprintf("; %d plugins loaded", loaded)
		}
		s.logSuccess(msg)
		s.sendCore(map[string]any{"type": "list_plugin_commands"})
	case "plugins_list":
		var m map[string]json.RawMessage
		if err := json.Unmarshal(ev.Raw, &m); err == nil {
			if raw, ok := m["plugins"]; ok {
				var plugins []json.RawMessage
				if json.Unmarshal(raw, &plugins) == nil && s.pendingPluginPicker {
					s.pendingPluginPicker = false
					s.openPluginPicker(plugins)
				}
			}
		}
	case "plugin_commands":
		var cmds []struct {
			Name        string `json:"name"`
			Description string `json:"description"`
			Plugin      string `json:"plugin"`
		}
		var m map[string]json.RawMessage
		if err := json.Unmarshal(ev.Raw, &m); err == nil {
			if raw, ok := m["commands"]; ok {
				_ = json.Unmarshal(raw, &cmds)
			}
		}
		s.pluginCommands = cmds
	case "plugin_status":
		text := ev.get("text")
		s.pluginStatus = text
		if text != "" {
			s.logInfo(text)
		}
		s.refresh()
	case "vision_config":
		var m map[string]json.RawMessage
		if json.Unmarshal(ev.Raw, &m) == nil {
			vm := map[string]bool{}
			if raw, ok := m["vision_models"]; ok {
				var arr []string
				if json.Unmarshal(raw, &arr) == nil {
					for _, id := range arr {
						vm[id] = true
					}
				}
			}
			s.visionModels = vm
			if raw, ok := m["vision_model"]; ok {
				var v string
				_ = json.Unmarshal(raw, &v)
				s.visionModel = v
			}
			// Missing enabled → recommended ON.
			s.visionEnabled = true
			if raw, ok := m["enabled"]; ok {
				var en bool
				if json.Unmarshal(raw, &en) == nil {
					s.visionEnabled = en
				}
			}
			if s.pendingVisionPicker {
				s.pendingVisionPicker = false
				s.openVisionPicker()
			}
		}
	case "skill_marketplace_state":
		var installed []installedMarketplaceSkill
		if raw, ok := ev.rawKey("installed"); ok {
			_ = json.Unmarshal(raw, &installed)
		}
		s.marketplaceInstalled = installed
		s.marketplaceAccepted = ev.get("disclaimer_accepted") == "true"
		if s.modal.kind == modalMarketplace {
			if s.marketplaceAccepted {
				s.openValueEditModal(editTargetMarketplaceSearch, "Search skills.sh", "at least 2 characters", "")
			} else {
				s.openValueEditModal(editTargetMarketplaceDisclaimer, "WARNING: third-party skills may be malicious", "type ACCEPT to continue at your own risk", "")
			}
		}
	case "skill_marketplace_results":
		var results []marketplaceSkill
		if raw, ok := ev.rawKey("skills"); ok {
			_ = json.Unmarshal(raw, &results)
		}
		s.marketplaceResults = results
		s.modal = newModal()
		s.modal.kind = modalMarketplace
	case "skill_marketplace_changed":
		s.logSuccess(fmt.Sprintf("skill %s: %s (%s)", ev.get("action"), ev.get("name"), ev.get("scope")))
		s.sendCore(map[string]any{"type": "list_marketplace_skills"})
	case "skill_marketplace_error":
		s.modal.loading = false
		s.logError("skill marketplace: " + ev.get("message"))
	case "skills":
		// Discoverable skills list (name + description). Populates the
		// /skill:<name> command-palette entries; the body is inlined into the
		// apply_skill prompt by the core, so the TUI must not retain Content.
		var skills []skillInfo
		if raw, ok := ev.rawKey("skills"); ok {
			_ = json.Unmarshal(raw, &skills)
		}
		s.skillsList = skills
	}
	return waitForEvent(s.coreEvents, s.coreStartGen)
}

// handleMouseWheel routes mouse-wheel events to the transcript viewport,
// mirroring handleScrollKey so follow mode stays consistent: scrolling up
// pauses follow (a streaming turn won't yank the view back to the bottom) and
// scrolling back to the bottom re-pins follow. Click/drag/release events are
// handled by transcript_mouse.go. Mouse tracking stays enabled for clickable
// disclosures, application-managed selection, and wheel navigation.
// applyModels sets the discovered model list and re-applies the persisted
// model selection + reasoning clamp. Shared by the `ready` and `models`
// events so a provider switch re-selects the same model id when present.
func (s *session) applyModels(models []modelInfo) {
	s.models = models
	s.modelIdx = 0
	if len(models) == 0 {
		s.modelIdx = -1 // no model: -1 is an explicit "invalid" sentinel (downstream guards accept it)
	} else if sel := s.settings.SelectedModel; sel != "" {
		for i, mm := range models {
			if mm.ID == sel || strings.Contains(mm.ID, sel) {
				s.modelIdx = i
				break
			}
		}
	} else {
		for i, mm := range models {
			if strings.Contains(mm.ID, "glm") {
				s.modelIdx = i
				break
			}
		}
	}
	if s.clampReasoning() {
		_ = s.settings.save()
		if s.modelIdx >= 0 && s.modelIdx < len(s.models) {
			s.logInfo(fmt.Sprintf("reasoning: %s (for %s)", s.settings.ReasoningEffort, s.models[s.modelIdx].ID))
		}
	}
}

// containsProvider reports whether name is in the core's configured provider list.
func (s *session) containsProvider(name string) bool {
	for _, p := range s.providers {
		if p == name {
			return true
		}
	}
	return false
}

// providerKey returns the persisted API key for a provider, preferring the
// per-provider key map over the legacy single APIKey (which applies to the
// default/active provider). Empty when nothing is stored.
// selectedProvider returns the owning provider of the currently selected model
// (models[modelIdx]). Sent with send/steer so an explicit /model pick routes to
// THAT provider even when several providers serve the same model id — selecting
// a model uses its owning provider, no /login + provider switch needed. Empty
// when there is no selection or the entry carries no provider tag.
func (s *session) selectedProvider() string {
	if s.modelIdx >= 0 && s.modelIdx < len(s.models) {
		return s.models[s.modelIdx].Provider
	}
	return ""
}

func (s *session) providerKey(name string) string {
	if k, ok := s.settings.ProviderKeys[name]; ok && k != "" {
		return k
	}
	// The legacy key predates named providers. It is safe only for the provider
	// that was active when the old settings were loaded; once a provider has
	// been selected/persisted, never treat it as a universal fallback.
	if s.settings.APIKey != "" && s.settings.ActiveProvider == "" && name == s.activeProvider {
		return s.settings.APIKey
	}
	return ""
}

// migrateLegacyProviderKey binds the old global key to the provider it
// actually belonged to. Call only after the core's initial ready event has
// identified that provider, never after an arbitrary provider switch.
func (s *session) migrateLegacyProviderKey(name string) {
	if name == "" || s.settings.APIKey == "" ||
		(s.settings.ActiveProvider != "" && s.settings.ActiveProvider != name) {
		return
	}
	if s.settings.ProviderKeys == nil {
		s.settings.ProviderKeys = map[string]string{}
	}
	if s.settings.ProviderKeys[name] == "" {
		s.settings.ProviderKeys[name] = s.settings.APIKey
	}
	s.settings.APIKey = ""
	_ = s.settings.save()
}

// deleteProviderKey drops a provider's persisted key from the per-provider map
// (and the legacy single APIKey when it was the active/default provider). Used
// by /logout so the TUI side and the core agree the provider is logged out.
func (s *session) deleteProviderKey(name string) {
	if s.settings.ProviderKeys != nil {
		delete(s.settings.ProviderKeys, name)
	}
	if s.settings.APIKey != "" && (name == s.activeProvider || name == "default") {
		s.settings.APIKey = ""
	}
}

// sendProviderKey sends `set_key` for a named provider (or the active one when
// name is empty). Only sent when a key is actually available.
func (s *session) sendProviderKey(name string) bool {
	if name == "" {
		name = s.activeProvider
	}
	key := s.providerKey(name)
	if key == "" {
		return false
	}
	s.sendCore(map[string]any{"type": "set_key", "provider": name, "api_key": key})
	return true
}

// reauthActiveProvider re-sends the active provider's persisted key when the
// core reports it isn't authenticated yet (e.g. after launch or a switch to a
// provider whose key isn't in the config file/env).
func (s *session) reauthActiveProvider() {
	if s.providerHasKey {
		return // already authed for this provider
	}
	if s.sendProviderKey(s.activeProvider) {
		s.logInfo("sending key…")
	} else if !s.canSend() {
		// Only nag when NO provider can serve a turn. Multi-provider setups with a
		// broken active provider but working secondary keys must still send.
		s.logWarn("not authenticated — run /login")
	}
}

// canSend reports whether the user may dispatch a model turn. True when the
// active provider is keyed OR any configured provider is logged in (models
// route per-id to their owning provider). This unblocks Linux/WSL users who
// had a stale/broken active provider while other providers still worked — the
// old global `authed` gate silently dropped every send before HTTP left core.
func (s *session) canSend() bool {
	return s.authed || s.anyLoggedIn || s.providerHasKey || len(s.models) > 0
}

// containsOtherLoggedInProvider is a cheap heuristic used when the active
// provider loses its key: if models remain tagged to a non-active owner, some
// other provider is still usable for sends.
func (s *session) containsOtherLoggedInProvider() bool {
	for _, m := range s.models {
		if m.Provider != "" && m.Provider != s.activeProvider {
			return true
		}
	}
	for _, p := range s.providerPresets {
		if p.LoggedIn && p.ID != s.activeProvider {
			return true
		}
	}
	return false
}

// In v2 mouse mode is a declarative View field. It remains in cell-motion mode
// for selection, clicks, and wheel scrolling.
func (s *session) handleMouseWheel(msg tea.MouseWheelMsg) tea.Cmd {
	// Modal overlays own the whole screen; route the wheel to their existing
	// keyboard navigation instead of scrolling the hidden transcript.
	if s.modal.kind != modalNone {
		return s.handleModalMouseWheel(msg)
	}
	// Blocking ask/sudo flyouts also own the whole screen: swallow the wheel so
	// it cannot scroll the hidden transcript behind the password/answer field.
	if s.pendingAsk != nil || s.pendingSudo != nil {
		return nil
	}
	switch msg.Button {
	case tea.MouseWheelUp:
		s.follow = false
		s.viewport.ScrollUp(s.viewport.MouseWheelDelta)
	case tea.MouseWheelDown:
		s.viewport.ScrollDown(s.viewport.MouseWheelDelta)
		if s.viewport.AtBottom() {
			s.follow = true
		}
	}
	return nil
}

// sendSteer dispatches a steer command to the core: interrupt the running
// turn (if any) and redirect it with prompt. Marks a chained turn so the TUI
// keeps streaming across the interrupt. Used by both Ctrl+Enter and /steer.
// sendDelegation sends a prompt that instructs the orchestrator to invoke the
// subagent tool (the model applies the pi-subagents skill). cmdName is shown
// to the user as the originating slash command.
func (s *session) sendDelegation(prompt, cmdName string) tea.Cmd {
	if !s.canSend() {
		s.logError("not authenticated — run /login first")
		return nil
	}
	if len(s.models) == 0 {
		s.logError("no models loaded yet")
		return nil
	}
	model := s.models[s.modelIdx].ID
	payload := s.withImages(map[string]any{
		"type": "send", "prompt": prompt, "model": model, "provider": s.selectedProvider(),
		"reasoning_effort": s.settings.ReasoningEffort,
	}, prompt)
	if !s.sendCore(payload) {
		return nil
	}
	s.follow = true
	s.logUser(prompt + "  ↳ " + cmdName)
	s.pushHistory(prompt)
	s.clearPendingImages()
	s.busy = true
	return nil
}

// runSubagentCommand parses a /run, /parallel, or /chain slash command and
// delegates to the subagent tool via a structured prompt. Supported forms:
//
//	/run <agent> "<task>"            (single)
//	/parallel <a1> "<t1>" | <a2> "<t2>"   (parallel)
//	/chain <a1> "<t1>" -> <a2> "<t2>"      (chain, {previous} flows)
//
// Bare commands (no remainder) open a value-edit modal instead of printing usage.
func (s *session) runSubagentCommand(parts []string, mode string) tea.Cmd {
	if len(parts) < 2 {
		switch mode {
		case "single":
			s.openRunModal()
		case "parallel":
			s.openParallelModal()
		case "chain":
			s.openChainModal()
		}
		return nil
	}
	return s.runSubagentRest(strings.TrimSpace(strings.Join(parts[1:], " ")), mode)
}

// runSubagentRest applies a free-form remainder (from the slash line or a modal)
// for /run, /parallel, or /chain.
func (s *session) runSubagentRest(rest, mode string) tea.Cmd {
	rest = strings.TrimSpace(rest)
	if rest == "" {
		s.logError("empty subagent task")
		return nil
	}
	var prompt string
	switch mode {
	case "single":
		agent, task := splitAgentTask(rest)
		if agent == "" {
			s.logError(`need: agent "task description"`)
			return nil
		}
		prompt = fmt.Sprintf("Run the subagent tool: agent=%q, task=%q. Return its result.", agent, task)
	case "parallel":
		tasks := splitParallel(rest)
		if strings.TrimSpace(tasks) == "" {
			s.logError(`need: a1 "task1" | a2 "task2"`)
			return nil
		}
		prompt = "Run the subagent tool in parallel mode with these tasks:\n" + tasks
	case "chain":
		steps := splitChain(rest)
		if strings.TrimSpace(steps) == "" {
			s.logError(`need: a1 "task1" -> a2 "task2"`)
			return nil
		}
		prompt = "Run the subagent tool as a chain with these steps (use {previous} to pass the prior step's output):\n" + steps
	default:
		return nil
	}
	return s.sendDelegation(prompt, "/"+mode)
}

// splitAgentTask splits "agent \"task text\"" (or agent task...) into (agent, task).
func splitAgentTask(s string) (string, string) {
	s = strings.TrimSpace(s)
	if s == "" {
		return "", ""
	}
	// quoted task: agent "task"
	if idx := strings.IndexAny(s, "\""); idx >= 0 {
		agent := strings.TrimSpace(s[:idx])
		task := strings.Trim(s[idx:], "\"")
		return unquoteFirst(agent), strings.TrimSpace(task)
	}
	// bare: first token is agent, rest is task
	parts := strings.Fields(s)
	if len(parts) == 0 {
		return "", ""
	}
	return parts[0], strings.Join(parts[1:], " ")
}

func unquoteFirst(s string) string {
	s = strings.TrimSpace(s)
	if len(s) >= 2 && (s[0] == '"' && s[len(s)-1] == '"' || s[0] == '\'' && s[len(s)-1] == '\'') {
		return s[1 : len(s)-1]
	}
	return s
}

// splitParallel renders a parallel tasks list from "a1 \"t1\" | a2 \"t2\"".
func splitParallel(s string) string {
	var lines []string
	for i, step := range strings.Split(s, "|") {
		agent, task := splitAgentTask(step)
		if agent == "" {
			continue
		}
		lines = append(lines, fmt.Sprintf("  %d. agent=%q, task=%q", i+1, agent, task))
	}
	return strings.Join(lines, "\n")
}

// splitChain renders a chain steps list from "a1 \"t1\" -> a2 \"t2\"".
func splitChain(s string) string {
	var lines []string
	for i, step := range strings.Split(s, "->") {
		agent, task := splitAgentTask(step)
		if agent == "" {
			continue
		}
		if task == "" {
			lines = append(lines, fmt.Sprintf("  %d. agent=%q (task inherits {previous})", i+1, agent))
		} else {
			lines = append(lines, fmt.Sprintf("  %d. agent=%q, task=%q", i+1, agent, task))
		}
	}
	return strings.Join(lines, "\n")
}

func (s *session) sendSteer(prompt string) tea.Cmd {
	if !s.canSend() {
		s.logError("not authenticated — run /login first")
		return nil
	}
	if len(s.models) == 0 {
		s.logError("no models loaded yet")
		return nil
	}
	model := s.models[s.modelIdx].ID
	if !s.sendCore(map[string]any{
		"type": "steer", "prompt": prompt, "model": model, "provider": s.selectedProvider(),
		"reasoning_effort": s.settings.ReasoningEffort,
	}) {
		return nil
	}
	s.follow = true
	s.logUser(prompt + "  ↳ steer")
	s.pushHistory(prompt)
	s.queuedNext = true
	s.queued = &queuedMsg{kind: "steer", text: prompt, at: time.Now()}
	s.layout()
	// Steer currently has no images field in the core protocol; path tokens in
	// the prompt still help for context, but pending image bytes are not
	// forwarded. Warn before clearing so the user knows attachments were dropped.
	if n := len(s.pendingImages); n > 0 {
		s.logWarn(fmt.Sprintf("steer dropped %d attached image%s (not supported on steer — use Enter to queue a follow-up with images)", n, pluralS(n)))
	}
	s.clearPendingImages()
	return nil
}

// steerFromInput sends the current input as a steer (Ctrl+Enter).
func (s *session) steerFromInput() tea.Cmd {
	text := strings.TrimSpace(s.input.Value())
	if text == "" && len(s.pendingImages) == 0 {
		return nil
	}
	if text == "" {
		text = "Describe this image."
	}
	cmd := s.sendSteer(text)
	if s.queued != nil && s.queued.kind == "steer" {
		s.input.Reset()
	}
	return cmd
}

// queueFollowUp sends prompt as a follow-up: the core buffers it (one-deep)
// and runs it after the current turn. Marks a chained turn so the TUI stays
// busy across the hand-off instead of flashing "ready".
func (s *session) queueFollowUp(text string) tea.Cmd {
	if !s.canSend() {
		s.logError("not authenticated — run /login first")
		return nil
	}
	if len(s.models) == 0 {
		s.logError("no models loaded yet")
		return nil
	}
	if s.queued != nil {
		s.logWarn("queue full — Esc to cancel the queued message, then send")
		return nil
	}
	model := s.models[s.modelIdx].ID
	payload := s.withImages(map[string]any{
		"type": "send", "prompt": text, "model": model, "provider": s.selectedProvider(),
		"reasoning_effort": s.settings.ReasoningEffort,
	}, text)
	if !s.sendCore(payload) {
		return nil
	}
	s.follow = true
	disp := text + "  ↳ queued"
	if n := len(s.pendingImages); n > 0 {
		disp = text + fmt.Sprintf("  [%d image%s]  ↳ queued", n, pluralS(n))
	}
	s.logUser(disp)
	s.pushHistory(text)
	s.queuedNext = true
	s.queued = &queuedMsg{kind: "follow-up", text: text, at: time.Now()}
	s.layout()
	s.clearPendingImages()
	return nil
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

func (s *session) handleKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	// The user is back at the keyboard — silence the window-title attention
	// bell (turn-complete / error notifications have been seen).
	s.titleBell = false
	// global: the quit key (default Ctrl+C). While a turn is running, the first
	// press aborts (or peels the queue) — matching CLI muscle memory — and a
	// second press within a short window quits. Idle Ctrl+C still quits.
	if s.kb(msg, "quit") && s.modal.kind == modalNone {
		if s.busy {
			if s.ctrlCAbortArmed {
				s.ctrlCAbortArmed = false
				return s, s.quit()
			}
			s.ctrlCAbortArmed = true
			if s.queued != nil {
				kind := s.queued.kind
				s.queued = nil
				s.queuedNext = false
				s.sendCore(map[string]any{"type": "clear_queue"})
				s.layout()
				if kind == "steer" {
					s.logInfo("steer cancelled — Ctrl+C again to quit")
				} else {
					s.logInfo("queued follow-up cancelled — Ctrl+C again to quit")
				}
				return s, nil
			}
			s.queuedNext = false
			s.sendCore(map[string]any{"type": "abort"})
			s.logWarn("aborting… (Ctrl+C again to quit)")
			return s, s.armAbortTimeout()
		}
		return s, s.quit()
	}
	s.ctrlCAbortArmed = false
	// A startup failure is a persistent recovery screen. Keep its two actions
	// deliberately simple so they remain usable even when the normal composer
	// and provider state never initialized.
	if s.coreLifecycle == coreFailed && s.modal.kind == modalNone {
		switch strings.ToLower(msg.String()) {
		case "r":
			s.resetCoreUIState()
			return s, s.startCore()
		case "q":
			return s, s.quit()
		default:
			return s, nil
		}
	}
	// modal intercept: when a modal is active it owns all keys.
	if s.modal.kind != modalNone {
		return s.handleModalKey(msg)
	}
	// Blocking prompts own the keyboard before any global composer shortcuts.
	// In particular, Shift+Enter and clipboard-image bindings must never mutate
	// the hidden chat draft while a password or answer field is focused.
	if s.pendingAsk != nil {
		return s.handleAskKey(msg)
	}
	if s.pendingSudo != nil {
		return s.handleSudoKey(msg)
	}
	// Shift+Enter inserts a line break so the user can compose multi-line
	// messages. This works while idle AND while a turn runs (so a follow-up can
	// be drafted mid-flight), as long as no modal owns the keys. Bubble Tea v2
	// delivers modified Enter as a real KeyPressMsg with modifier bits, so we
	// match it through the keybind system like any other key — no SS3/CSI
	// buffering needed.
	if s.kb(msg, "newline") {
		s.insertNewline()
		return s, nil
	}
	// Paste image from the local clipboard (ctrl+shift+v by default). Works on
	// a local GUI session; over SSH prefer bracketed paste of a path / data
	// URL (handled in Update on tea.PasteMsg).
	if s.kb(msg, "paste_image") {
		return s, readClipboardImageCmd()
	}
	// Remove the last staged image attachment.
	if s.kb(msg, "detach_image") {
		if s.popPendingImage() {
			s.logInfo("detached last image")
		}
		return s, nil
	}
	// Expanded activity is a lightweight focus mode. Subagents and tasks can
	// outnumber the available rows, so navigation scrolls that shelf before the
	// transcript; Esc or the toggle key returns focus to the composer.
	if s.activityExpanded && s.pendingApproval == nil && s.pendingIntercom == nil && s.queued == nil &&
		(len(s.subProgress) > 0 || len(s.todos) > 0) {
		switch {
		case s.kb(msg, "toggle_activity") || s.kb(msg, "close"):
			s.activityExpanded = false
			s.activityScroll = 0
			s.layout()
			return s, nil
		case s.kbAny(msg, "nav_up", "nav_up_alt", "scroll_line_up"):
			s.activityScroll = max(0, s.activityScroll-1)
			s.layout()
			return s, nil
		case s.kb(msg, "scroll_page_up"):
			s.activityScroll = max(0, s.activityScroll-5)
			s.layout()
			return s, nil
		case s.kbAny(msg, "nav_down", "nav_down_alt", "scroll_line_down"):
			s.activityScroll++
			s.layout()
			return s, nil
		case s.kb(msg, "scroll_page_down"):
			s.activityScroll += 5
			s.layout()
			return s, nil
		}
	}
	// transcript scrolling works in every state (idle/busy/approval) so the
	// user can read history while a turn runs or a decision is pending.
	if s.handleApprovalDiffScroll(msg) {
		return s, nil
	}
	if s.handleScrollKey(msg) {
		return s, nil
	}
	if s.kb(msg, "transcript_prev") {
		s.moveTranscriptFocus(-1)
		return s, nil
	}
	if s.kb(msg, "transcript_next") {
		s.moveTranscriptFocus(1)
		return s, nil
	}
	if s.kb(msg, "transcript_find") {
		s.openValueEditModal(editTargetTranscriptFind, "Find in Transcript", "search messages and tool output", "")
		return s, nil
	}
	if s.kb(msg, "copy_focused") {
		return s, s.copyFocusedBlock()
	}
	// global: toggle reasoning-block collapse/expand
	if s.kb(msg, "toggle_reasoning") {
		s.thinkExpanded = !s.thinkExpanded
		s.settings.ThinkExpanded = s.thinkExpanded
		_ = s.settings.save()
		for _, b := range s.blocks {
			if b.kind == blkThinking {
				b.collapsed = !s.thinkExpanded
				// collapsed state changed: the cached render (pill ↔ full markdown)
				// is stale, so drop it for a fresh render. Other finalized blocks
				// keep their cached render across the rebuild.
				b.renderStr = ""
				b.renderLen = 0
			}
		}
		s.invalidateAll()
		s.refresh()
		return s, nil
	}
	// global: toggle full output for the tool nearest the viewport (not always
	// the chronologically last one — scrolling up to inspect an older truncated
	// tool should expand that one).
	if s.kb(msg, "toggle_tool_output") {
		if s.pendingApproval != nil && strings.TrimSpace(s.pendingApproval.diff) != "" {
			s.pendingApproval.expanded = !s.pendingApproval.expanded
			if !s.pendingApproval.expanded {
				s.pendingApproval.diffScroll = 0
			}
			s.layout()
			return s, nil
		}
		if b := s.nearestToolOutputBlock(); b != nil {
			b.expanded = !b.expanded
			s.invalidateAll()
			s.refresh()
		}
		return s, nil
	}
	// global: open the command palette (default ctrl+p / ctrl+k).
	if s.kbAny(msg, "command_palette", "command_palette_alt") {
		s.openCommandPalette()
		return s, nil
	}
	// global: open the reasoning-effort picker for the active model.
	if s.kb(msg, "reasoning_picker") {
		s.openReasoningPicker()
		return s, nil
	}
	if s.kb(msg, "toggle_activity") {
		s.activityExpanded = !s.activityExpanded
		s.activityScroll = 0
		s.layout()
		return s, nil
	}
	// global: collapse / expand the pinned goal progress panel (big goals
	// otherwise eat a lot of vertical space above the composer).
	if s.kb(msg, "toggle_goal_panel") {
		if s.goalState != nil && goalShowsProgressPanel(s.goalState.Phase, s.goalState.AutoDeploy) {
			s.goalPanelCollapsed = !s.goalPanelCollapsed
			s.layout()
		}
		return s, nil
	}
	// "/" opens the palette when the input is empty — works while idle and
	// in-flight, mirroring the @-mention flyout (which also opens mid-turn).
	if msg.String() == "/" && s.input.Value() == "" {
		s.openCommandPalette()
		return s, nil
	}
	// "?" opens help when the input is empty (matches the header tip).
	if msg.String() == "?" && s.input.Value() == "" {
		return s, s.handleUserLine("/help")
	}
	// @-mention flyout: when open it owns arrow/tab/enter/esc; printable
	// and editing keys fall through to the input and re-evaluate the token.
	if s.mentionActive && s.handleMentionNav(msg) {
		return s, nil
	}

	if s.pendingIntercom != nil {
		// A subagent asked the orchestrator a blocking question. Enter (the send
		// key) replies; Esc unblocks the child with a best-judgment nudge so it
		// isn't stuck.
		if s.kb(msg, "close") {
			s.sendCore(map[string]any{"type": "intercom_reply", "request_id": s.pendingIntercom.requestID, "reply": "[no reply — proceed with your best judgment]"})
			s.advanceIntercom()
			s.layout()
			return s, nil
		}
		if s.kb(msg, "send") {
			reply := strings.TrimSpace(s.input.Value())
			if reply == "" {
				// Empty intercom replies are never sent (Esc sends the
				// "[no reply]" nudge). Pulse the banner hint instead of a
				// silent no-op so the user knows to type — fixes
				// "Enter does not reply to the subagent".
				s.intercomNudge = time.Now()
				s.input.Focus()
				return s, nil
			}
			s.sendCore(map[string]any{"type": "intercom_reply", "request_id": s.pendingIntercom.requestID, "reply": reply})
			s.logSuccess(fmt.Sprintf("↦ reply to %s sent", s.pendingIntercom.from))
			s.advanceIntercom()
			s.layout()
			return s, nil
		}
		var cmd tea.Cmd
		s.input, cmd = s.input.Update(msg)
		return s, cmd
	}
	if s.pendingApproval != nil {
		inputEmpty := strings.TrimSpace(s.input.Value()) == ""
		owner := "approval:" + s.pendingApproval.requestID
		switch {
		case inputEmpty && s.kb(msg, "approve"):
			s.sendCore(map[string]any{"type": "approve", "request_id": s.pendingApproval.requestID, "decision": "yes"})
			s.resolveLatestApproval("approved once")
			s.pendingApproval = nil
		case inputEmpty && s.kb(msg, "approve_always"):
			s.sendCore(map[string]any{"type": "approve", "request_id": s.pendingApproval.requestID, "decision": "always"})
			s.resolveLatestApproval("always allowed")
			s.pendingApproval = nil
		case inputEmpty && s.kbAny(msg, "deny", "close"):
			s.sendCore(map[string]any{"type": "approve", "request_id": s.pendingApproval.requestID, "decision": "no"})
			s.resolveLatestApproval("denied")
			s.logError("denied")
			s.pendingApproval = nil
		default:
			// Decision keys only fire when the composer is empty so drafting a
			// follow-up (typing "yes" / "y…") can't accidentally approve.
			var cmd tea.Cmd
			s.input, cmd = s.input.Update(msg)
			return s, cmd
		}
		s.restoreComposer(owner)
		s.layout() // banner released, grow viewport back
		return s, nil
	}
	if s.busy {
		// While a turn runs the input stays live: type a follow-up (Enter),
		// steer the model (Ctrl+Enter), run a slash command, or abort (Esc).
		// Scrolling + ctrl+t/o/p above still work; a lone "/" with an empty
		// input opens the command palette mid-turn too (like the @ flyout).
		switch {
		case s.kb(msg, "close"):
			// Esc peels off layers: if a follow-up/steer is queued, first Esc just
			// drops the queued message (the in-flight turn keeps running); a
			// second Esc then aborts the running turn. This matches the user's
			// mental model of "cancel the queued thing" without nuking the
			// whole in-flight chat.
			if s.queued != nil {
				kind := s.queued.kind
				s.queued = nil
				s.queuedNext = false
				s.sendCore(map[string]any{"type": "clear_queue"})
				s.layout() // release the queue banner
				if kind == "steer" {
					s.logInfo("steer cancelled (the running turn was already interrupted)")
				} else {
					s.logInfo("queued follow-up cancelled — turn continues")
				}
				return s, nil
			}
			s.queuedNext = false
			s.sendCore(map[string]any{"type": "abort"})
			s.logWarn("aborting…")
			return s, s.armAbortTimeout()
		case s.kb(msg, "steer"):
			return s, s.steerFromInput()
		case s.kb(msg, "send"):
			text := strings.TrimSpace(s.input.Value())
			if text == "" && len(s.pendingImages) == 0 {
				return s, nil
			}
			if text == "?" || text == "/?" {
				s.input.Reset()
				return s, s.handleUserLine("/help")
			}
			// Image-only follow-up: default caption so the core still gets a prompt.
			if text == "" {
				text = "Describe this image."
			}
			// Queue is one-deep — refuse a second follow-up instead of lying
			// with a banner that overwrites the real queued prompt.
			if s.queued != nil && !(len(s.pendingImages) == 0 && (strings.HasPrefix(text, "/") || isBangCommand(text))) {
				s.logWarn("queue full — Esc to cancel the queued message, then send")
				return s, nil
			}
			// Slash commands and bang bash (`!` / `!!`) run immediately even
			// while a turn is in flight — same as PI interactive mode.
			if len(s.pendingImages) == 0 && (strings.HasPrefix(text, "/") || isBangCommand(text)) {
				s.input.Reset()
				s.evalMention()
				return s, s.handleUserLine(text)
			}
			cmd := s.queueFollowUp(text)
			if s.queued != nil && s.queued.text == text {
				s.input.Reset()
				s.evalMention()
			}
			return s, cmd
		case s.kb(msg, "history_prev") && len(s.history) > 0 && s.historyRecallAllowed(-1):
			val := s.recallHistory(-1)
			s.input.SetValue(val)
			return s, s.evalMention()
		case s.kb(msg, "history_next") && len(s.history) > 0 && s.historyRecallAllowed(+1):
			val := s.recallHistory(+1)
			s.input.SetValue(val)
			return s, s.evalMention()
		}
		// Backspace on empty input pops the last attached image (same affordance
		// as removing a chip in the web composer).
		if (msg.Code == tea.KeyBackspace || msg.String() == "backspace" || msg.String() == "ctrl+h") &&
			s.input.Value() == "" && len(s.pendingImages) > 0 {
			s.popPendingImage()
			return s, nil
		}
		var cmd tea.Cmd
		s.input, cmd = s.input.Update(msg)
		return s, tea.Batch(cmd, s.evalMention())
	}

	// welcome-screen navigation: when the conversation is empty, ↑/↓ move the
	// example cursor; enter drops the selected example into the (editable)
	// input. Only arrow keys are used so typing letters/digits is unaffected.
	if !s.hasConversation() && s.pendingApproval == nil && strings.TrimSpace(s.input.Value()) == "" {
		if !s.canSend() {
			if s.kb(msg, "select") && strings.TrimSpace(s.input.Value()) == "" {
				s.openLoginPicker()
				return s, nil
			}
		} else {
			switch {
			case s.kb(msg, "nav_up"):
				s.welcomeIdx = (s.welcomeIdx - 1 + len(welcomeExamples)) % len(welcomeExamples)
				return s, nil
			case s.kb(msg, "nav_down"):
				s.welcomeIdx = (s.welcomeIdx + 1) % len(welcomeExamples)
				return s, nil
			}
			if s.kb(msg, "select") && strings.TrimSpace(s.input.Value()) == "" {
				s.input.SetValue(welcomeExamples[s.welcomeIdx])
				s.evalMention()
				s.input.Focus()
				return s, nil
			}
		}
	}

	// history recall: only when the draft is empty, or the cursor is already on
	// the first (Up) / last (Down) line — otherwise Up/Down navigate inside the
	// multi-line composition (shell/IDE pattern).
	if s.kb(msg, "history_prev") && len(s.history) > 0 && s.historyRecallAllowed(-1) {
		val := s.recallHistory(-1)
		s.input.SetValue(val)
		return s, s.evalMention()
	}
	if s.kb(msg, "history_next") && len(s.history) > 0 && s.historyRecallAllowed(+1) {
		val := s.recallHistory(+1)
		s.input.SetValue(val)
		return s, s.evalMention()
	}

	if s.kb(msg, "send") {
		text := strings.TrimSpace(s.input.Value())
		if text == "" && len(s.pendingImages) == 0 {
			return s, nil
		}
		if text == "?" || text == "/?" {
			s.input.Reset()
			return s, s.handleUserLine("/help")
		}
		// Image-only send: give the model a default caption prompt.
		if text == "" {
			text = "Describe this image."
		}
		if !strings.HasPrefix(text, "/") && !isBangCommand(text) {
			if !s.canSend() {
				s.logError("not authenticated — run /login first")
				return s, nil
			}
			if len(s.models) == 0 {
				s.logError("no models loaded yet")
				return s, nil
			}
		}
		s.input.Reset()
		s.evalMention()
		s.histIdx = len(s.history)
		s.stashedDraft = "" // sending commits the draft; no undo-restore after
		return s, s.handleUserLine(text)
	}

	// Backspace on empty input removes the last attached image.
	if (msg.Code == tea.KeyBackspace || msg.String() == "backspace" || msg.String() == "ctrl+h") &&
		s.input.Value() == "" && len(s.pendingImages) > 0 {
		s.popPendingImage()
		return s, nil
	}

	// Idle Esc was a dead key, which is safe but useless. Give it the chat-UI
	// affordance without the trap: first Esc clears the draft but stashes it,
	// a second Esc on the empty composer restores it. Modal/approval/busy
	// owners of "close" returned long before this point.
	if s.kb(msg, "close") {
		if v := s.input.Value(); strings.TrimSpace(v) != "" {
			s.stashedDraft = v
			s.input.Reset()
			s.evalMention()
			s.logInfo("draft cleared — Esc again to restore")
			return s, nil
		}
		if s.stashedDraft != "" {
			s.input.SetValue(s.stashedDraft)
			s.stashedDraft = ""
			s.evalMention()
			s.logInfo("draft restored")
			return s, nil
		}
	}

	var cmd tea.Cmd
	s.input, cmd = s.input.Update(msg)
	return s, tea.Batch(cmd, s.evalMention())
}

func (s *session) handleApprovalDiffScroll(msg tea.KeyPressMsg) bool {
	a := s.pendingApproval
	if a == nil || !a.expanded || strings.TrimSpace(a.diff) == "" {
		return false
	}
	step := max(1, s.height/3)
	switch {
	case s.kb(msg, "scroll_page_up"):
		a.diffScroll -= step
	case s.kb(msg, "scroll_page_down"):
		a.diffScroll += step
	case s.kb(msg, "scroll_line_up"):
		a.diffScroll--
	case s.kb(msg, "scroll_line_down"):
		a.diffScroll++
	default:
		return false
	}
	if a.diffScroll < 0 {
		a.diffScroll = 0
	}
	s.layout()
	return true
}

// handleScrollKey moves the transcript viewport and manages follow mode.
// Returns true when it consumed the key. Scroll-up motions pause follow (so the
// view isn't yanked to the bottom on the next token); scroll-down re-pins
// follow once the bottom is reached.
func (s *session) handleScrollKey(msg tea.KeyPressMsg) bool {
	switch {
	case s.kb(msg, "scroll_page_up"):
		s.follow = false
		s.viewport.PageUp()
		return true
	case s.kb(msg, "scroll_page_down"):
		s.viewport.PageDown()
		if s.viewport.AtBottom() {
			s.follow = true
		}
		return true
	case s.kb(msg, "scroll_line_up"):
		s.follow = false
		s.viewport.ScrollUp(1)
		return true
	case s.kb(msg, "scroll_line_down"):
		s.viewport.ScrollDown(1)
		if s.viewport.AtBottom() {
			s.follow = true
		}
		return true
	case s.kb(msg, "scroll_top"):
		s.follow = false
		s.viewport.GotoTop()
		return true
	case s.kb(msg, "scroll_bottom"):
		s.follow = true
		s.viewport.GotoBottom()
		return true
	}
	return false
}

// ---------------------------------------------------------------------------
// User line / slash commands
// ---------------------------------------------------------------------------

// quit performs a clean app teardown: it marks quitting so the core's stdout
// EOF doesn't trigger an auto-restart, kills the core process (the
// stdout-reader goroutine reaps it via cmd.Wait() on EOF — we never Wait()
// here, which would race the reader's Wait on the same Cmd), and returns the
// tea.Quit command. Shared by the quit key and the /exit · /quit commands.
func (s *session) quit() tea.Cmd {
	quitting.Store(true)
	s.clearPendingImages()
	if s.coreCmd != nil && s.coreCmd.Process != nil {
		_ = s.coreCmd.Process.Kill()
	}
	return tea.Quit
}

func (s *session) handleUserLine(text string) tea.Cmd {
	// PI-compatible bang bash: `!cmd` runs and adds output to model context;
	// `!!cmd` runs without adding to context. Empty `!` / `!!` fall through.
	if cmd, exclude, ok := parseBangCommand(text); ok {
		s.pushHistory(text)
		s.sendCore(map[string]any{
			"type":                 "user_bash",
			"command":              cmd,
			"exclude_from_context": exclude,
		})
		return nil
	}
	if strings.HasPrefix(text, "/") {
		parts := strings.Fields(text)
		s.recordRecentCommand(parts[0])
		// The TUI is modal-first: text after a slash command is never interpreted
		// as command arguments. Every configurable command opens its form/picker,
		// where validation, defaults, and all options remain visible.
		parts = parts[:1]
		// /skill:<name> [optional task] — invoke a discoverable skill. Handled
		// before the switch because the command token is dynamic (/skill:<x>
		// has no fixed case). The core reads the SKILL.md and runs the turn.
		if strings.HasPrefix(parts[0], "/skill:") {
			name := strings.TrimPrefix(parts[0], "/skill:")
			s.openValueEditModal(editTargetSkill+name, "Apply Skill: "+name,
				"optional task (blank = apply without a task)", "")
			return nil
		}
		switch parts[0] {
		case "/login", "/provider":
			s.openLoginPicker()
			return nil
		case "/search-key":
			// /search-key                       -> picker (Exa/Tavily → paste key)
			// /search-key <exa|tavily>          -> paste modal for that provider
			// /search-key <exa|tavily> <key>    -> set inline
			// /search-key <exa|tavily> --clear  -> remove the stored key
			if len(parts) < 2 {
				s.openSearchKeyPicker()
				return nil
			}
			name := strings.ToLower(parts[1])
			if name != "exa" && name != "tavily" {
				s.logError("/search-key: provider must be 'exa' or 'tavily' (got '" + parts[1] + "')")
				return nil
			}
			if len(parts) >= 3 {
				arg := strings.Join(parts[2:], " ")
				if arg == "--clear" || arg == "clear" || arg == "off" {
					s.sendCore(map[string]any{"type": "set_search_key", "provider": name, "api_key": ""})
					s.logInfo("clearing " + name + " search key…")
				} else {
					s.sendCore(map[string]any{"type": "set_search_key", "provider": name, "api_key": arg})
					s.logInfo("saving " + name + " search key…")
				}
				return nil
			}
			label := "Exa"
			if name == "tavily" {
				label = "Tavily"
			}
			s.openValueEditModal(editTargetSearchKey+":"+name, label+" API Key",
				"paste your key, then Enter (blank to clear, Esc to cancel)", "")
			return nil
		case "/logout":
			if len(parts) >= 2 {
				// /logout <provider> — direct logout without the picker.
				name := parts[1]
				s.sendCore(map[string]any{"type": "logout", "provider": name})
				s.deleteProviderKey(name)
				if s.settings.ActiveProvider == name {
					s.settings.ActiveProvider = ""
				}
				_ = s.settings.save()
				s.logInfo("logged out of " + name)
				return nil
			}
			s.openLogoutPicker()
			return nil
		case "/oauth-code":
			// /oauth-code [code] completes a pending no-browser OAuth login (the
			// SSH/headless Google flow). With an inline code it sends immediately;
			// with no argument it opens a modal to paste the code into — the long
			// Google code is awkward to paste inline after the command (the command
			// input mangles/truncates it).
			if len(parts) >= 2 {
				code := strings.Join(parts[1:], " ")
				s.sendCore(map[string]any{"type": "oauth_code", "code": code})
				s.logInfo("submitting OAuth code…")
				return nil
			}
			s.openOauthCodeModal()
			return nil
		case "/model", "/models":
			if len(parts) < 2 {
				s.openModelPicker()
				return nil
			}
			idx := -1
			if n, _ := fmt.Sscanf(parts[1], "%d", &idx); n == 1 && idx >= 0 && idx < len(s.models) {
			} else {
				idx = -1
				for i, mm := range s.models {
					if strings.Contains(mm.ID, parts[1]) {
						idx = i
						break
					}
				}
			}
			if idx < 0 {
				s.logError("no model matches '" + parts[1] + "'")
				return nil
			}
			s.modelIdx = idx
			s.settings.SelectedModel = s.models[idx].ID
			_ = s.settings.save()
			if s.clampReasoning() {
				_ = s.settings.save()
				s.logInfo(fmt.Sprintf("reasoning: %s (for %s)", s.settings.ReasoningEffort, s.models[idx].ID))
			}
			s.logInfo(fmt.Sprintf("model: %s", s.models[idx].ID))
			return nil
		case "/jobs":
			s.sendCore(map[string]any{"type": "job_list"})
			return nil
		case "/tree":
			s.sendCore(map[string]any{"type": "session_tree"})
			return nil
		case "/branch":
			if len(parts) != 2 {
				s.logError("usage: /branch <entry-id>")
				return nil
			}
			s.sendCore(map[string]any{"type": "session_branch", "entry_id": parts[1]})
			return nil
		case "/job":
			if len(parts) != 2 {
				s.logError("usage: /job <run-id>")
				return nil
			}
			s.sendCore(map[string]any{"type": "job_status", "run_id": parts[1]})
			return nil
		case "/wait-job":
			if len(parts) != 2 {
				s.logError("usage: /wait-job <run-id>")
				return nil
			}
			s.sendCore(map[string]any{"type": "job_wait", "run_id": parts[1]})
			return nil
		case "/cancel-job":
			if len(parts) != 2 {
				s.logError("usage: /cancel-job <run-id>")
				return nil
			}
			s.sendCore(map[string]any{"type": "job_cancel", "run_id": parts[1]})
			return nil
		case "/reset":
			s.openDestructiveConfirm("reset", "", "wipe the conversation and its session file")
			return nil
		case "/abort", "/stop":
			s.queuedNext = false
			s.queued = nil
			s.sendCore(map[string]any{"type": "abort"})
			s.logWarn("aborting…")
			return s.armAbortTimeout()
		case "/exit", "/quit":
			// Quit the app (alias: /quit). Same clean teardown as the quit key.
			return s.quit()
		case "/steer":
			// Steer via command: works on every terminal (Ctrl+Enter is only detected
			// on terminals that send a distinct CSI for it). Bare /steer opens a
			// modal so the user does not have to type the message on the slash line.
			if len(parts) < 2 {
				s.openSteerModal()
				return nil
			}
			return s.sendSteer(strings.Join(parts[1:], " "))
		case "/approval", "/approvals":
			// Bare /approval opens the dedicated picker; with an arg, set directly.
			if len(parts) < 2 {
				s.openApprovalPicker()
				return nil
			}
			mode := parts[1]
			switch mode {
			case "never", "destructive", "always":
				s.applyApprovalMode(mode)
			default:
				s.logError("usage: /approval never|destructive|always")
			}
			return nil
		case "/advisor":
			s.openAdvisorModal()
			return nil
		case "/reasoning", "/thinking":
			if len(parts) >= 2 {
				level := parts[1]
				levels := s.thinkingLevels()
				ok := false
				for _, l := range levels {
					if strings.EqualFold(l, level) {
						s.settings.ReasoningEffort = l
						_ = s.settings.save()
						s.logInfo(fmt.Sprintf("reasoning: %s", l))
						ok = true
						break
					}
				}
				if !ok {
					s.logError("usage: /reasoning [" + strings.Join(levels, "|") + "]")
				}
				return nil
			}
			s.openReasoningPicker()
			return nil
		case "/bash-timeout":
			if len(parts) >= 2 {
				var n int
				if _, err := fmt.Sscanf(parts[1], "%d", &n); err == nil && n > 0 {
					s.coreBashTimeout = n
					s.settings.BashTimeoutSecs = n
					_ = s.settings.save()
					s.sendCore(map[string]any{"type": "set_config", "key": "bash_timeout_secs", "value": n})
					s.logInfo(fmt.Sprintf("bash timeout: %ds", n))
				} else {
					s.logError("usage: /bash-timeout <seconds>")
				}
				return nil
			}
			s.openBashTimeoutModal()
			return nil
		case "/auto-compact":
			if len(parts) >= 2 {
				switch strings.ToLower(parts[1]) {
				case "on", "true", "1":
					s.coreAutoCompact = true
					s.settings.AutoCompact = true
					_ = s.settings.save()
					s.sendCore(map[string]any{"type": "set_config", "key": "auto_compact", "value": true})
					s.logInfo("auto-compact: on")
				case "off", "false", "0":
					s.coreAutoCompact = false
					s.settings.AutoCompact = false
					_ = s.settings.save()
					s.sendCore(map[string]any{"type": "set_config", "key": "auto_compact", "value": false})
					s.logInfo("auto-compact: off")
				default:
					s.logError("usage: /auto-compact [on|off]")
				}
				return nil
			}
			s.openAutoCompactPicker()
			return nil
		case "/sandbox":
			if len(parts) >= 2 {
				sub := strings.ToLower(parts[1])
				switch sub {
				case "status":
					s.requestSandboxStatus()
					return nil
				case "enable", "on":
					s.requestSandboxEnable()
					return nil
				case "disable", "off":
					s.setSandboxNone()
					return nil
				case "setup", "prepare":
					s.requestSandboxPrepare()
					return nil
				case "recheck", "check":
					s.requestSandboxStatus()
					return nil
				case "reset":
					s.requestSandboxReset()
					return nil
				}
				// Otherwise treat the argument as a sandbox value (none | microsandbox
				// | a deprecated backend alias). Deprecated backends are migrated to
				// microsandbox so the user's intent to enable sandboxing is preserved.
				mode, deprecated := normalizeSandboxValue(parts[1])
				if mode == "" {
					s.logError("usage: /sandbox [status|enable|disable|setup|recheck|reset] | none | microsandbox")
					return nil
				}
				if deprecated {
					s.logInfo(fmt.Sprintf("sandbox %q is deprecated; migrating to microsandbox", parts[1]))
				}
				if mode == "none" {
					s.setSandboxNone()
				} else {
					s.requestSandboxEnable()
				}
				return nil
			}
			s.openSandboxPicker()
			return nil
		case "/no-network":
			if len(parts) >= 2 {
				switch strings.ToLower(parts[1]) {
				case "on", "true", "1":
					s.settings.NoNetwork = true
					_ = s.settings.save()
					s.logInfo("no-network: on")
					s.offerCoreRestart("no-network")
				case "off", "false", "0":
					s.settings.NoNetwork = false
					_ = s.settings.save()
					s.logInfo("no-network: off")
					s.offerCoreRestart("no-network")
				default:
					s.logError("usage: /no-network [on|off]")
				}
				return nil
			}
			s.openNoNetworkPicker()
			return nil
		case "/mouse-wheel":
			// Backward-compatible no-op for old scripts/settings muscle memory.
			s.logInfo("mouse scrolling, clicking, and drag-copy are always on")
			return nil
		case "/footer-metrics":
			if len(parts) >= 2 {
				switch strings.ToLower(parts[1]) {
				case "on", "true", "1":
					s.settings.FooterMetrics = true
				case "off", "false", "0":
					s.settings.FooterMetrics = false
				default:
					s.logError("usage: /footer-metrics [on|off]")
					return nil
				}
				_ = s.settings.save()
				s.logInfo("footer metrics: " + boolStr(s.settings.FooterMetrics))
				s.layout()
				return nil
			}
			s.openFooterMetricsPicker()
			return nil
		case "/reduced-motion":
			if len(parts) >= 2 {
				switch strings.ToLower(parts[1]) {
				case "on", "true", "1":
					s.settings.ReducedMotion = true
				case "off", "false", "0":
					s.settings.ReducedMotion = false
				default:
					s.logError("usage: /reduced-motion [on|off]")
					return nil
				}
				_ = s.settings.save()
				s.logInfo("reduced motion: " + boolStr(s.settings.ReducedMotion))
				if s.usageReport != nil {
					_ = s.rebuildUsageBars(s.usageReport)
				}
				return nil
			}
			s.openReducedMotionPicker()
			return nil
		case "/idle-timeout":
			if len(parts) >= 2 {
				var n int
				if _, err := fmt.Sscanf(parts[1], "%d", &n); err == nil && n >= 10 {
					s.settings.IdleTimeout = n
					_ = s.settings.save()
					s.logInfo(fmt.Sprintf("idle timeout: %ds", n))
					s.offerCoreRestart("idle timeout")
				} else {
					s.logError("usage: /idle-timeout <seconds≥10>")
				}
				return nil
			}
			s.openIdleTimeoutModal()
			return nil
		case "/max-session-tokens":
			if len(parts) >= 2 {
				var n int
				if _, err := fmt.Sscanf(parts[1], "%d", &n); err == nil && n >= 0 {
					s.settings.MaxSessionTokens = n
					_ = s.settings.save()
					s.logInfo(fmt.Sprintf("max session tokens: %d", n))
					s.offerCoreRestart("max session tokens")
				} else {
					s.logError("usage: /max-session-tokens <n≥0>  (0=unlimited)")
				}
				return nil
			}
			s.openMaxSessionTokensModal()
			return nil
		case "/help", "/?":
			s.openHelp()
			return nil
		case "/settings":
			s.openSettings()
			return nil
		case "/keybinds":
			s.openKeybindsModal()
			return nil
		case "/theme":
			s.openThemePicker()
			return nil
		case "/copy":
			return s.copyLastAssistant()
		case "/attach":
			// Bare /attach opens a file picker; with args it sends immediately.
			if len(parts) < 2 {
				return s.openAttachModal()
			}
			promptText := ""
			if len(parts) > 2 {
				promptText = strings.Join(parts[2:], " ")
			}
			return s.sendAttach(parts[1], promptText)
		case "/clear":
			s.sendCore(map[string]any{"type": "clear"})
			s.blocks = nil
			s.cur = nil
			s.contextTokens = 0
			s.follow = true
			s.invalidateAll()
			s.transcriptBase = ""
			s.transcriptPlain = nil
			s.viewport.SetContent("")
			s.logInfo("in-memory conversation cleared (session file kept)")
			return nil
		case "/undo":
			// Core emits history with the trimmed conversation; do not wipe the
			// transcript optimistically (that looked like total data loss).
			s.sendCore(map[string]any{"type": "undo"})
			s.logInfo("dropping last turn…")
			return nil
		case "/compact":
			// Bare /compact opens a modal for optional preserve-instructions;
			// with args it still compacts immediately.
			rest := strings.TrimSpace(strings.Join(parts[1:], " "))
			if rest == "" {
				s.openCompactModal()
				return nil
			}
			s.sendCore(map[string]any{"type": "compact", "instructions": rest})
			s.logInfo("forcing context compaction…")
			return nil
		case "/refresh":
			// On-demand model-cache refresh: core force-fetches /models live from
			// every logged-in provider (bypassing the 8h disk-cache TTL), then
			// re-emits `models` + `provider_presets` and a final `models_refreshed`.
			s.sendCore(map[string]any{"type": "refresh_models"})
			s.logInfo("refreshing model list…")
			return nil
		case "/context":
			s.sendCore(map[string]any{"type": "context"})
			return nil
		case "/usage":
			// Provider plan limits for the currently selected model. Core
			// resolves model → provider and fetches the appropriate endpoint.
			cmd := map[string]any{"type": "usage"}
			if s.modelIdx >= 0 && s.modelIdx < len(s.models) {
				cmd["model"] = s.models[s.modelIdx].ID
			}
			s.usageReport = nil // show "loading…" until the event lands
			s.usageBars = nil
			s.modal.kind = modalUsage
			s.modal.editing = false
			s.modal.fieldIdx = 0
			s.sendCore(cmd)
			return nil
		case "/remember":
			rest := strings.TrimSpace(strings.Join(parts[1:], " "))
			if rest == "" {
				s.openRememberModal()
				return nil
			}
			s.sendCore(map[string]any{"type": "save_memory", "text": rest})
			s.sendCore(map[string]any{"type": "refresh_memory"})
			s.logSuccess("memory saved")
			return nil
		case "/memory", "/memories":
			// Open the memory picker (enter forgets); falls back to a log line
			// only when the list is empty.
			s.requestMemoryPicker()
			return nil
		case "/forget":
			if len(parts) < 2 {
				s.requestMemoryPicker()
				return nil
			}
			s.openDestructiveConfirm("memory-forget", parts[1], "permanently forget memory "+parts[1])
			return nil
		case "/index":
			s.openIndexModal()
			return nil
		case "/reflect":
			// Deliberate end-of-task learning pass: critique the recent work in this
			// session and persist durable takeaways via the memory tool. Pure
			// delegation — no core command needed.
			task := "Reflect on the work done in this session so far. Identify: (1) any convention, architecture fact, decision, or gotcha worth persisting so future sessions don't rediscover it, and (2) any repetitive pattern you performed more than once that should become a reusable skill under `.catalyst-code/skills/`. Use the `memory` tool (action: append if a topic memory exists, else save) to persist durable facts only — skip transient task state. If you wrote a skill, name it. Finish with a two-line summary: what you learned and what you persisted."
			return s.sendDelegation(task, "/reflect")
		case "/sessions", "/resume":
			s.openSessionsPicker()
			s.modal.loading = true
			if !s.sendCore(map[string]any{"type": "list_sessions"}) {
				s.modal.loading = false
				s.modal.loadError = "The core is busy; press r to retry."
			}
			return nil
		case "/new":
			path := filepath.Join(sessionsDir(), newSessionFilename())
			if !reserveSession(path) {
				s.logError("could not reserve a new session file")
				return nil
			}
			if !s.sendCore(map[string]any{"type": "new_session", "path": path}) {
				cancelSessionReservation(path)
				s.logError("the core is busy; the current session was left unchanged")
				return nil
			}
			s.logInfo("starting a new session…")
			return nil
		case "/stats":
			s.sendCore(map[string]any{"type": "stats"})
			return nil
		case "/status":
			model := "no model"
			if s.modelIdx >= 0 && s.modelIdx < len(s.models) {
				model = s.models[s.modelIdx].ID
			}
			provider := s.activeProvider
			if provider == "" {
				provider = "not selected"
			}
			effort := s.settings.ReasoningEffort
			if effort == "" {
				effort = s.preferredLevel(s.thinkingLevels())
			}
			lines := []string{
				"Status",
				"model: " + model + " · provider: " + provider,
				"approval: " + approvalModeLabel(s.approvalMode()) + " · reasoning: " + effort,
				"sandbox: " + s.sandboxEffectiveLabel(),
				"theme: " + activeTheme.name + " · mouse: on",
			}
			if metrics := s.renderMetrics(); metrics != "" {
				lines = append(lines, "performance: "+metrics)
			}
			lines = append(lines, "context: "+noColorANSIRe.ReplaceAllString(s.renderContext(), ""))
			s.logPersist(blkInfo, strings.Join(lines, "\n"))
			return nil
		case "/find":
			query := strings.TrimSpace(strings.TrimPrefix(text, parts[0]))
			if query == "" {
				s.openValueEditModal(editTargetTranscriptFind, "Find in Transcript", "search messages and tool output", "")
				return nil
			}
			s.findTranscript(query)
			return nil
		case "/skills":
			s.modal = newModal()
			s.modal.kind = modalMarketplace
			s.marketplaceResults = nil
			s.sendCore(map[string]any{"type": "list_marketplace_skills"})
			return nil
		case "/plugin-install":
			if len(parts) < 2 {
				s.openPluginInstallModal()
				return nil
			}
			path, scope, err := parsePluginInstallArgs(parts[1:])
			if err != nil {
				s.logError(err.Error())
				return nil
			}
			if scope == "" {
				s.openPluginInstallScopeModal(path)
				return nil
			}
			s.sendPluginInstall(path, scope)
			return nil
		case "/plugin-list", "/plugin-config", "/plugin-enable", "/plugin-disable":
			// Bare enable/disable open the toggle picker (same as /plugin-config);
			// with a name they still act immediately.
			if (parts[0] == "/plugin-enable" || parts[0] == "/plugin-disable") && len(parts) >= 2 {
				if parts[0] == "/plugin-enable" {
					s.sendCore(map[string]any{"type": "enable_plugin", "name": parts[1]})
				} else {
					s.sendCore(map[string]any{"type": "disable_plugin", "name": parts[1]})
				}
				return nil
			}
			s.requestPluginPicker(pluginModeToggle)
			return nil
		case "/vision":
			s.requestVisionPicker()
			return nil
		case "/plugin-remove", "/plugin-uninstall":
			if len(parts) >= 2 {
				s.openDestructiveConfirm("plugin-remove", parts[1], "uninstall plugin "+parts[1]+" and remove its files")
				return nil
			}
			s.requestPluginPicker(pluginModeRemove)
			return nil
		case "/plugin-reload":
			s.sendCore(map[string]any{"type": "reload_plugins"})
			s.logInfo("reloading plugins…")
			return nil
		case "/plugin-trust":
			// Re-open the plugin trust modal (also opens automatically once on
			// startup when undecided plugins exist).
			s.sendCore(map[string]any{"type": "plugin_trust_prompt"})
			s.logInfo("checking plugin trust…")
			return nil
		case "/control":
			prefill := ""
			if len(parts) >= 2 {
				prefill = strings.TrimSpace(strings.TrimPrefix(text, parts[0]))
			}
			s.openGoalModal(prefill)
			s.goalDraft.ceoMode = true
			s.goalDraft.reviewBeforeDeploy = false
			return nil
		case "/goal":
			prefill := ""
			if len(parts) >= 2 {
				// Keep everything after "/goal " as the goal text.
				prefill = strings.TrimSpace(strings.TrimPrefix(text, parts[0]))
			}
			s.openGoalModal(prefill)
			return nil
		case "/cancel-goal":
			s.sendCore(map[string]any{"type": "cancel_goal"})
			s.logInfo("cancelling goal…")
			return nil
		case "/run":
			return s.runSubagentCommand(parts, "single")
		case "/parallel":
			return s.runSubagentCommand(parts, "parallel")
		case "/chain":
			return s.runSubagentCommand(parts, "chain")
		case "/subagents", "/subagents-list":
			return s.sendDelegation(`Run subagent({ action: "list" }) and show the available agents.`, "/subagents")
		case "/subagents-doctor":
			return s.sendDelegation(`Run subagent({ action: "doctor" }) and show the setup diagnostics.`, "/subagents-doctor")
		case "/subagents-status":
			return s.sendDelegation(`Run subagent({ action: "status" }) and show the active subagent runs.`, "/subagents-status")
		case "/subagents-models":
			return s.sendDelegation(`Run subagent({ action: "models" }) and show the runtime model mapping for the builtin agents.`, "/subagents-models")
		default:
			cmdName := strings.TrimPrefix(parts[0], "/")
			for _, pc := range s.pluginCommands {
				if strings.EqualFold(pc.Name, cmdName) {
					s.openValueEditModal(editTargetPluginCommand+pc.Name, "Plugin Command: /"+pc.Name,
						"optional command input", "")
					return nil
				}
			}
			s.logError("unknown command: " + parts[0])
			return nil
		}
	}

	if !s.canSend() {
		s.logError("not authenticated — run /login first")
		return nil
	}
	if len(s.models) == 0 {
		s.logError("no models loaded yet")
		return nil
	}
	model := s.models[s.modelIdx].ID
	payload := s.withImages(map[string]any{
		"type": "send", "prompt": text, "model": model, "provider": s.selectedProvider(),
		"reasoning_effort": s.settings.ReasoningEffort,
	}, text)
	if !s.sendCore(payload) {
		// handleKey may already have transferred the draft to this function;
		// restore it so backpressure never turns into user-visible data loss.
		s.input.SetValue(text)
		s.input.MoveToEnd()
		return nil
	}
	s.follow = true // jump to bottom so the user sees their turn + the response
	disp := text
	if n := len(s.pendingImages); n > 0 {
		disp = text + fmt.Sprintf("  [%d image%s]", n, pluralS(n))
	}
	s.logUser(disp)
	s.pushHistory(text)
	s.clearPendingImages()
	s.busy = true
	return nil
}

// sendAttach validates imgPath and sends a vision turn. promptText may be empty
// (falls back to the composer value, then a default caption prompt). Shared by
// `/attach <path> [prompt]` and the attach value-edit modal.
func (s *session) sendAttach(imgPath, promptText string) tea.Cmd {
	// P2-12: validate the image like the main send paths do (via withImages),
	// so /attach can't base64-encode a non-image or a >20MiB file.
	abs, err := validateImage(imgPath)
	if err != nil {
		s.logError(err.Error())
		return nil
	}
	if strings.TrimSpace(promptText) == "" {
		promptText = s.input.Value()
	}
	if strings.TrimSpace(promptText) == "" {
		promptText = "Describe this image."
	}
	if !s.canSend() {
		s.logError("not authenticated — run /login first")
		return nil
	}
	if len(s.models) == 0 {
		s.logError("no models loaded yet")
		return nil
	}
	model := s.models[s.modelIdx].ID
	s.follow = true
	payload := s.withImages(map[string]any{
		"type":             "send",
		"prompt":           promptText,
		"model":            model,
		"provider":         s.selectedProvider(),
		"reasoning_effort": s.settings.ReasoningEffort,
		"images":           []string{abs},
	}, promptText)
	if !s.sendCore(payload) {
		return nil
	}
	s.logUser(promptText + " [image: " + imgPath + "]")
	s.clearPendingImages()
	s.busy = true
	s.input.Reset()
	return nil
}

// handleSkillCommand dispatches "/skill:<name> [optional task]". It resolves the
// skill from the cached skills list and sends an apply_skill command to the
// core, which reads the SKILL.md (resolving project > user scope, bypassing
// read_file's path restriction so global skills work too) and runs a turn that
// applies it. The displayed user line is the concise "/skill:<name> [task]";
// the full skill body is injected by the core, not shown in the transcript.
// Skills are the exception to the "bare slash → modal" rule: they stay
// argument-oriented so the user can append a task after the skill token.
func (s *session) handleSkillCommand(parts []string) tea.Cmd {
	token := parts[0] // "/skill:<name>"
	name := strings.TrimPrefix(token, "/skill:")
	if name == "" {
		s.logError("usage: /skill:<name> [optional task]")
		return nil
	}
	var found *skillInfo
	for i := range s.skillsList {
		if strings.EqualFold(s.skillsList[i].Name, name) {
			found = &s.skillsList[i]
			break
		}
	}
	if found == nil {
		s.logError("unknown skill: " + name)
		return nil
	}
	if !s.canSend() {
		s.logError("not authenticated — run /login first")
		return nil
	}
	if len(s.models) == 0 {
		s.logError("no models loaded yet")
		return nil
	}
	model := s.models[s.modelIdx].ID
	task := strings.TrimSpace(strings.Join(parts[1:], " "))
	display := token
	if task != "" {
		display = token + " " + task
	}
	cmd := map[string]any{
		"type":             "apply_skill",
		"name":             found.Name,
		"model":            model,
		"reasoning_effort": s.settings.ReasoningEffort,
	}
	if task != "" {
		cmd["task"] = task
	}
	if !s.sendCore(cmd) {
		return nil
	}
	s.follow = true
	s.logUser(display)
	s.pushHistory(display)
	s.busy = true
	return nil
}

func (s *session) runIndex(incremental bool) tea.Cmd {
	if incremental {
		return s.sendDelegation("Run an incremental knowledge index of this repository. Inspect changed files and update relevant durable memories without duplicates. End by listing what changed.", "/index")
	}
	return s.sendDelegation("Run a full knowledge index of this repository. Inspect architecture, conventions, APIs, build steps, tests, and gotchas; persist durable named memories and list them.", "/index")
}

// openURL best-effort opens a URL in the OS default browser (used to surface an
// OAuth login URL). Errors are ignored — the URL is also shown in the transcript.
func openURL(url string) {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.Command("open", url)
	case "windows":
		cmd = exec.Command("cmd", "/C", "start", "", url)
	default:
		cmd = exec.Command("xdg-open", url)
	}
	cmd.Stdin = nil
	cmd.Stdout = nil
	cmd.Stderr = nil
	// Run (Start+Wait) in a goroutine so the opener is reaped instead of
	// leaving a zombie; fire-and-forget Start() never collects the child.
	go func() { _ = cmd.Run() }()
}

// writeOSC52Cmd returns a tea.Cmd that writes the OSC 52 escape sequence to set
// the LOCAL terminal's clipboard to text. Over SSH the sequence passes through
// to the user's local terminal, which writes its clipboard — so the user can
// paste (Ctrl/Cmd+V) into their local browser without copying from the
// (wrapped, hard-to-select) transcript. Best-effort: terminals that don't
// support OSC 52 ignore it. The sequence is invisible (no cursor move / no
// text). tea.Raw routes the bytes through Bubble Tea's renderer, avoiding an
// asynchronous os.Stdout race with frame output.
func writeOSC52Cmd(text string) tea.Cmd {
	if text == "" {
		return nil
	}
	// OSC 52: ESC ] 52 ; <selection> ; <base64> BEL.  'c' = the CLIPBOARD
	// selection (the Ctrl/Cmd+V paste buffer).
	const maxOSC52Bytes = 100_000
	if len(text) > maxOSC52Bytes {
		text = text[:maxOSC52Bytes]
	}
	enc := base64.StdEncoding.EncodeToString([]byte(text))
	seq := "\x1b]52;c;" + enc + "\x07"
	return tea.Raw(seq)
}

// parsePluginInstallArgs extracts the plugin source and install scope from
// `/plugin-install` args (or the install modal value fields).
// Scope is empty when omitted — callers should prompt (global vs workspace).
// Recognizes global|workspace and --global/--workspace/-g/-w.
func parsePluginInstallArgs(args []string) (path string, scope string, err error) {
	var pathParts []string
	for _, a := range args {
		switch strings.ToLower(strings.TrimSpace(a)) {
		case "global", "--global", "-g", "user":
			scope = "global"
		case "workspace", "--workspace", "-w", "project", "local":
			scope = "workspace"
		case "":
			continue
		default:
			pathParts = append(pathParts, a)
		}
	}
	if len(pathParts) == 0 {
		return "", "", fmt.Errorf("usage: /plugin-install <path|url> [global|workspace]")
	}
	if len(pathParts) > 1 {
		return "", "", fmt.Errorf("unexpected extra args after plugin source: %s", strings.Join(pathParts[1:], " "))
	}
	return pathParts[0], scope, nil
}

// isBangCommand reports whether text is a PI-style bang bash invocation with a
// non-empty command (`!cmd` or `!!cmd`). Bare `!` / `!!` are not bang commands.
func isBangCommand(text string) bool {
	_, _, ok := parseBangCommand(text)
	return ok
}

// parseBangCommand extracts a PI-compatible bang bash command.
//
//	!cmd  → command, excludeFromContext=false
//	!!cmd → command, excludeFromContext=true
//
// Returns ok=false when the text is not a bang command or the command is empty
// (so bare `!` falls through as a normal prompt, matching PI).
func parseBangCommand(text string) (command string, excludeFromContext bool, ok bool) {
	if !strings.HasPrefix(text, "!") {
		return "", false, false
	}
	excludeFromContext = strings.HasPrefix(text, "!!")
	if excludeFromContext {
		command = strings.TrimSpace(text[2:])
	} else {
		command = strings.TrimSpace(text[1:])
	}
	if command == "" {
		return "", false, false
	}
	return command, excludeFromContext, true
}
