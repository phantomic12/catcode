package main

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"charm.land/bubbles/v2/help"
	"charm.land/bubbles/v2/key"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

// ---------------------------------------------------------------------------
// Layout & rendering
//
// Top-to-bottom:
//   header   (3)   brand + tagline row, cwd + tip row, separator
//   viewport (N)   the scrollable transcript
//   posbar   (1)   scroll-position bar / "↓ N new" affordance
//   banner   (1)   approval prompt (when pending)
//   panel    (?)   active-tasks box (when a scout is in flight)
//   inputbox (3)   bordered chat input
//   footer   (2)   line 1: state·model·approval·think  |  line 2: metrics·context
//
// The posbar is always reserved so scrolling up never reflows the transcript.
// ---------------------------------------------------------------------------

func envEnabled(names ...string) bool {
	for _, name := range names {
		switch strings.ToLower(strings.TrimSpace(os.Getenv(name))) {
		case "1", "true", "yes", "on":
			return true
		}
	}
	return false
}

// These environment switches make the TUI usable in assistive/log-oriented
// environments without adding persisted settings that older cores reject.
func prefersReducedMotion() bool {
	return envEnabled("CATCODE_REDUCED_MOTION", "REDUCED_MOTION")
}

// motionReduced reports whether UI animations should be suppressed (env or setting).
func (s *session) motionReduced() bool {
	return prefersReducedMotion() || s.settings.ReducedMotion
}

func plainTerminalMode() bool {
	return envEnabled("CATCODE_PLAIN", "CATCODE_NO_ALT_SCREEN")
}

var noColorANSIRe = regexp.MustCompile(`\x1b\[[0-9;?]*[A-Za-z]`)

// stripANSI removes CSI sequences so callers can match plain text in styled output.
func stripANSI(s string) string {
	return noColorANSIRe.ReplaceAllString(s, "")
}

// headerHeight is deliberately measured rather than hard-coded: compact
// terminals use a single header row, while normal terminals retain both rows.
func (s *session) headerHeight() int { return lipgloss.Height(s.renderHeader()) }

// viewChromeCache holds chrome strings built once per View so relayoutHeights
// measure-by-render does not double-build the animated input / panels.
type viewChromeCache struct {
	header, footer, inputBox, activityShelf, goalPanel, mentionFlyout, positionBar, workingWave string
	headerOK, footerOK, inputOK, shelfOK, goalOK, mentionOK, posOK, waveOK                      bool
}

func (s *session) beginViewChrome() { s.viewChrome = &viewChromeCache{} }
func (s *session) endViewChrome()   { s.viewChrome = nil }

// relayoutHeights recomputes the viewport height to fit the current input-box
// height + panels and applies it. It is CHEAP: it does not re-render the
// transcript blocks (their content is unchanged; only the viewport's visible
// window height moves). Called after every input update so a growing
// multi-line input shrinks the viewport instead of pushing the footer off the
// bottom of the screen.
func (s *session) relayoutHeights() {
	if !s.ready {
		return
	}
	// Fixed-height optional panels (everything except the active-tasks panel,
	// whose entry count we cap below to fit).
	fixedExtra := 0
	if s.coreLifecycle == coreFailed && s.hasConversation() {
		fixedExtra++
	}
	if s.updateInfo != nil {
		fixedExtra++
	}
	fixedExtra += s.mentionFlyoutHeight()
	fixedExtra += s.activityShelfHeight() + s.oauthBannerHeight()
	fixedExtra += s.goalProgressPanelHeight()
	fixedExtra += s.workingWaveHeight()
	// Space left for the viewport + the active-tasks panel, leaving 1 line of
	// slack for v2's cursed renderer (it scrolls/overlaps when the view fills
	// the terminal exactly).
	avail := s.height - s.headerHeight() - s.positionBarHeight() - s.footerHeight() - fixedExtra - 1
	// Legacy/detail renderer remains available to tests and explicit reports;
	// the main View uses the unified shelf instead.
	s.maxTaskRows = min(2, len(s.subProgress))
	h := avail
	if h < 0 {
		h = 0 // panels fill the screen; hide the transcript rather than overflow
	}
	s.viewport.SetWidth(s.width)
	s.viewport.SetHeight(h)
	s.input.SetWidth(max(1, s.width-4))
}

// layout recomputes heights AND re-renders the transcript. Use it on events
// that change or re-wrap the blocks (terminal resize, task start/finish). For
// input-only changes (typing/pasting) use relayoutHeights() — re-rendering
// every keystroke is expensive.
func (s *session) layout() {
	prevW := s.viewport.Width()
	s.relayoutHeights()
	// Wrap width is viewport.Width(); height-only changes (todo/scout/goal
	// panels) must not wipe the finalized block cache.
	if s.viewport.Width() != prevW {
		s.invalidateAll()
	}
	s.refresh()
}

func (s *session) renderIntercomBanner() string {
	i := s.pendingIntercom
	sendKey, closeKey := s.keyHint("send"), s.keyHint("close")
	if sendKey == "" {
		sendKey = "send"
	}
	if closeKey == "" {
		closeKey = "skip"
	}
	var msg string
	if !s.intercomNudge.IsZero() && time.Since(s.intercomNudge) < 1500*time.Millisecond {
		msg = fmt.Sprintf("⚠ type a reply below, then %s   ·   %s to skip", sendKey, closeKey)
	} else {
		queue := ""
		if n := 1 + len(s.intercomQueue); n > 1 {
			queue = fmt.Sprintf(" [1 of %d]", n)
		}
		msg = fmt.Sprintf("❓ subagent %s%s asks: %s   type reply + %s   ·   %s skip", i.from, queue, truncate(i.message, max(1, s.width-60)), sendKey, closeKey)
	}
	return lipgloss.NewStyle().
		Width(max(1, s.width-2)).MaxWidth(max(1, s.width)).
		Background(lipgloss.Color(c.accent)).
		Foreground(lipgloss.Color(c.bg)).
		Bold(true).
		Padding(0, 1).
		Render(msg)
}

func (s *session) renderCoreFailureBanner() string {
	if s.coreLifecycle != coreFailed || !s.hasConversation() {
		return ""
	}
	msg := "core unavailable · r retry · q quit"
	if s.coreFailure != "" && s.width >= 64 {
		msg = truncate(s.coreFailure+" · r retry · q quit", max(1, s.width-2))
	}
	return lipgloss.NewStyle().MaxWidth(max(1, s.width)).
		Foreground(lipgloss.Color(c.bg)).Background(lipgloss.Color(c.err)).Bold(true).
		Render(" " + msg)
}

// renderHeader keeps fixed two-row geometry on normal terminals so mouse hit
// targets and transcript coordinates stay stable. The second row is metadata,
// rendered quieter than the workspace identity above it.
func (s *session) renderHeader() string {
	if s.viewChrome != nil && s.viewChrome.headerOK {
		return s.viewChrome.header
	}
	brand := accentStyle.Render("◆") + " " + boldBaseStyle.Render("Catalyst") + dimStyle.Render(" / Code")
	state, stateColor := "starting", c.secondary
	switch {
	case s.coreLifecycle == coreFailed:
		state, stateColor = "core down", c.err
	case s.busy:
		state, stateColor = "working", c.accent
		if s.goalState != nil && goalShowsProgressPanel(s.goalState.Phase, s.goalState.AutoDeploy) {
			settled, total := s.goalProgressCounts()
			state = fmt.Sprintf("goal · %s · %d/%d", goalProgressPhaseLabel(s.goalState.Phase, s.goalState.AutoDeploy), settled, total)
		}
	case s.authed:
		state, stateColor = "ready", c.success
	case len(s.models) > 0:
		state, stateColor = "no key", c.warn
	}
	right := statusDot(stateColor) + " " + lipgloss.NewStyle().Foreground(lipgloss.Color(stateColor)).Bold(true).Render(state)
	if len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) && s.width >= 42 {
		right += mutedStyle.Render(" · " + truncate(s.models[s.modelIdx].ID, max(8, s.width/3)))
	}
	out := fitRow(s.width, " "+brand, right+" ")
	if s.width >= 48 && s.height >= 12 {
		project := "PROJECT  —"
		if s.cwd != "" {
			project = "PROJECT  " + truncatePath(s.cwd, max(12, s.width/2-12))
		}
		model := "MODEL  —"
		if len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) {
			model = "MODEL  " + truncate(s.models[s.modelIdx].ID, max(10, s.width/3))
		}
		out += "\n" + fitRow(s.width, " "+mutedStyle.Render(project), mutedStyle.Render(model)+" ")
	}
	if s.viewChrome != nil {
		s.viewChrome.header = out
		s.viewChrome.headerOK = true
	}
	return out
}

// renderPositionBar: a thin scroll affordance. Pinned to the bottom it is a
// subtle dim rule; scrolled up it becomes an accent bar telling the user how
// many newer lines are hidden below and how to jump back.
func (s *session) renderPositionBar() string {
	if s.viewChrome != nil && s.viewChrome.posOK {
		return s.viewChrome.positionBar
	}
	w := s.width
	if w < 1 {
		w = 1
	}
	// lines hidden below the current viewport window (0 when pinned to bottom)
	below := s.viewport.TotalLineCount() - s.viewport.YOffset() - s.viewport.VisibleLineCount()
	if below < 0 {
		below = 0
	}
	var out string
	if below > 0 {
		pct := int(s.viewport.ScrollPercent() * 100)
		msg := fmt.Sprintf("↓ %d new · %d%% · PgDn scroll · Ctrl+End jump", below, pct)
		out = lipgloss.NewStyle().
			Width(max(1, w-2)).MaxWidth(w).
			Background(lipgloss.Color(c.accent)).
			Foreground(lipgloss.Color(c.bg)).
			Bold(true).
			Padding(0, 1).
			Render(msg)
	}
	if s.viewChrome != nil {
		s.viewChrome.positionBar = out
		s.viewChrome.posOK = true
	}
	return out
}

func (s *session) positionBarHeight() int {
	if s.renderPositionBar() == "" {
		return 0
	}
	return 1
}

// approvalBanner: a full-width sticky bar shown while a decision is pending.
// The head reuses the per-tool primitives (icon + name + parsed keyarg) so the
// human approves the actual target ("src/main.rs · 3 replacements") instead
// of a raw JSON blob. For write/edit/patch a unified-diff preview renders
// below so the decision is on the real change, not the search/replace blobs.
func (s *session) renderApprovalBanner() string {
	a := s.pendingApproval
	var actions []string
	if key := s.keyHint("approve"); key != "" {
		actions = append(actions, "["+key+"] once")
	}
	if key := s.keyHint("deny"); key != "" {
		actions = append(actions, "["+key+"] deny")
	}
	if key := s.keyHint("approve_always"); key != "" {
		actions = append(actions, "["+key+"] type")
	}
	controls := strings.Join(actions, " · ")
	avail := s.width - lipgloss.Width(controls) - 24
	if avail < 8 {
		avail = 8
	}
	summary := truncate(approvalSummary(a.tool, a.args), avail)
	// The approval banner is the one place the UI asks for a decision, so it gets
	// the Catalyst warn (amber) as a solid accent bar — the highest-contrast
	// surface in the app, impossible to miss while scrolling.
	msg := "⚠  approval required  " + toolIcon(a.tool) + " " + toolDisplayName(a.tool) + "  " + summary
	if controls != "" {
		msg += "   " + controls
	}
	// A non-empty composer disables the Y/N/A decision keys (they'd otherwise
	// fire mid-typing). The composer placeholder explains this only when it's
	// EMPTY — useless exactly when the keys are dead — so say it here, on the
	// always-visible banner, whenever a draft is present.
	if strings.TrimSpace(s.input.Value()) != "" {
		msg += "   · clear input to answer"
	}
	// Elapsed waiting time: after a few seconds it reassures the user the
	// request is live and how long they've been blocking the turn.
	if !a.receivedAt.IsZero() {
		if d := time.Since(a.receivedAt); d >= 5*time.Second {
			msg += " · waiting " + d.Truncate(time.Second).String()
		}
	}
	banner := lipgloss.NewStyle().
		Width(max(1, s.width-2)).MaxWidth(max(1, s.width)).
		Background(lipgloss.Color(c.warn)).
		Foreground(lipgloss.Color(c.bg)).
		Bold(true).
		Padding(0, 1).
		Render(msg)
	if strings.TrimSpace(a.diff) != "" {
		banner += "\n" + s.renderApprovalDiff(a)
	}
	return banner
}

func (s *session) renderApprovalDiff(a *approvalPrompt) string {
	if !a.expanded {
		return renderDiffPanel(a.diff, false, s.width, s.keyHint("toggle_tool_output"))
	}
	all := strings.Split(renderDiffPanel(a.diff, true, s.width, s.keyHint("toggle_tool_output")), "\n")
	capRows := s.height / 2
	if capRows < 3 {
		capRows = 3
	}
	if capRows > len(all) {
		capRows = len(all)
	}
	maxScroll := len(all) - capRows
	if a.diffScroll > maxScroll {
		a.diffScroll = maxScroll
	}
	if a.diffScroll < 0 {
		a.diffScroll = 0
	}
	view := strings.Join(all[a.diffScroll:a.diffScroll+capRows], "\n")
	if len(all) > capRows {
		view += "\n" + dimStyle.Render(fmt.Sprintf("│ diff rows %d–%d/%d · PgUp/PgDn scroll", a.diffScroll+1, a.diffScroll+capRows, len(all)))
	}
	return view
}

// renderFooter is a quiet command deck. The composer remains the primary
// surface; controls and telemetry use typography instead of another filled bar.
func (s *session) renderFooter() string {
	if s.viewChrome != nil && s.viewChrome.footerOK {
		return s.viewChrome.footer
	}
	left := s.footerControlHint()
	if toast := s.renderToast(); toast != "" {
		left = toast
	} else {
		left = keyHintStyle.Render(left)
	}
	right := s.renderContext()
	var lines []string
	if s.width < 48 {
		// Compact mode preserves the one action that can be taken now and the
		// current token total. It intentionally drops only the context maximum.
		lines = append(lines, " "+keyHintStyle.Render(s.primaryFooterHint()))
		var maxToks uint64
		if len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) {
			maxToks = uint64(s.models[s.modelIdx].ContextWindow)
		}
		pct := 0
		if maxToks > 0 {
			pct = min(100, int(float64(s.contextTokens)/float64(maxToks)*100))
			if pct == 0 && s.contextTokens > 0 {
				pct = 1
			}
		}
		bar := renderContextBar(float64(pct)/100, 10)
		lines = append(lines, " "+bar+mutedStyle.Render(fmt.Sprintf(" %d%% · %s", pct, compactTokens(s.contextTokens))))
	} else {
		lines = append(lines, fitRow(max(1, s.width), " "+left, right+" "))
	}
	if s.settings.FooterMetrics && s.height >= 16 {
		model := "no model"
		if len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) {
			model = s.models[s.modelIdx].ID
		}
		if metrics := s.renderMetrics(); metrics != "" {
			model += "  ·  " + metrics
		}
		lines = append(lines, mutedStyle.Render(" "+truncate(model, max(1, s.width-1))))
	}
	out := strings.Join(lines, "\n")
	if s.viewChrome != nil {
		s.viewChrome.footer = out
		s.viewChrome.footerOK = true
	}
	return out
}

func (s *session) renderCompactFooter() string {
	return s.renderFooter()
}

func (s *session) primaryFooterHint() string {
	if s.pendingApproval != nil {
		return s.keyHint("approve") + " allow · " + s.keyHint("deny") + " deny"
	}
	if s.busy {
		return s.keyHint("send") + " queue · " + s.keyHint("close") + " abort"
	}
	return s.keyHint("send") + " send"
}

func (s *session) footerControlHint() string {
	h := s.newFooterHelp(max(1, s.width))
	return h.ShortHelpView(s.footerHelpBindings())
}

func (s *session) newFooterHelp(width int) help.Model {
	h := help.New()
	h.ShortSeparator = " · "
	h.Ellipsis = "…"
	h.Styles = catalystHelpStyles()
	h.SetWidth(width)
	return h
}

func (s *session) footerHelpBindings() []key.Binding {
	switch {
	case s.pendingApproval != nil:
		return []key.Binding{
			s.bindingFor("approve", "allow once"),
			s.bindingFor("deny", "deny"),
			s.bindingFor("approve_always", "always allow type"),
		}
	case s.busy:
		return []key.Binding{
			s.bindingFor("send", "queue"),
			s.bindingFor("close", "abort"),
			s.bindingFor("steer", "steer"),
		}
	default:
		return []key.Binding{
			s.bindingFor("send", "send"),
			s.bindingFor("newline", "newline"),
			s.bindingFor("command_palette", "commands"),
		}
	}
}

// composerHintLine is a dim second line inside the composer while busy/queued/
// approval is active and the user is already typing (placeholder is hidden).
func (s *session) composerHintLine(innerW int) string {
	h := s.newFooterHelp(innerW)
	switch {
	case s.pendingApproval != nil:
		out := h.ShortHelpView([]key.Binding{
			s.bindingFor("approve", "allow once"),
			s.bindingFor("deny", "deny"),
			s.bindingFor("approve_always", "always allow type"),
		})
		extra := h.Styles.ShortDesc.Inline(true).Render("clear input first")
		if out == "" {
			return extra
		}
		return out + h.Styles.ShortSeparator.Inline(true).Render(h.ShortSeparator) + extra
	case s.queued != nil:
		out := h.ShortHelpView([]key.Binding{s.bindingFor("close", "cancels queued")})
		prefix := h.Styles.ShortDesc.Inline(true).Render("queue full")
		if out == "" {
			return prefix
		}
		return prefix + h.Styles.ShortSeparator.Inline(true).Render(h.ShortSeparator) + out
	case s.busy:
		return h.ShortHelpView([]key.Binding{
			s.bindingFor("send", "queues"),
			s.bindingFor("close", "aborts"),
			s.bindingFor("steer", "steers"),
		})
	default:
		return ""
	}
}

func (s *session) renderFooterPerformance() string {
	return mutedStyle.Render(s.renderFooterPerformancePlain())
}

func (s *session) renderFooterPerformancePlain() string {
	model := "no model"
	if len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) {
		model = s.models[s.modelIdx].ID
	}
	parts := []string{model}
	if len(s.lastMetrics) > 0 {
		var m map[string]json.RawMessage
		if json.Unmarshal(s.lastMetrics, &m) == nil {
			if tps := strings.TrimSpace(get(m, "tps")); tps != "" {
				parts = append(parts, tps+" tok/s")
			}
			if ttft := strings.TrimSpace(get(m, "ttft_ms")); ttft != "" {
				parts = append(parts, ttft+"ms ttft")
			}
		}
	}
	return truncate(strings.Join(parts, "  ·  "), max(1, s.width))
}

// renderMetrics builds the throughput string for the footer's second line:
// TPS (rounded to the nearest integer) and TTFT, plus the prefix-cache hit rate
// (e.g. "42 tok/s · 180ms ttft · 87% cached"). During an in-flight stream the
// core may emit tps_est, an approximate live throughput based on streamed text;
// final metrics use tps, the provider-reported real token count.
//
// The cache rate has a wrinkle: the live mid-stream metrics event omits
// cached_tokens — it only lands in the turn-end metrics. So while a turn is in
// flight there's no cache number for *this* turn yet. We fall back to the
// previous turn's measured rate (captured in s.lastCachePct by the metrics
// handler) and prefix it with "~" so it reads as "from last turn", not a live
// reading. Once the turn-end metrics arrive (cached_tokens present), the fresh,
// un-tilde'd rate is shown.
func (s *session) renderMetrics() string {
	var m map[string]json.RawMessage
	haveLive := len(s.lastMetrics) > 0 && json.Unmarshal(s.lastMetrics, &m) == nil

	var out string
	if haveLive {
		tps := get(m, "tps")
		approx := false
		if tps == "" || tps == "null" {
			tps = get(m, "tps_est")
			approx = tps != "" && tps != "null"
		}
		if tps != "" && tps != "null" {
			// Round to the nearest integer so the footer reads "71 tok/s"
			// rather than "71.123132991239 tok/s". Prefix live estimates with
			// "~" so they are useful in-flight without being confused for the
			// final provider-usage-derived TPS.
			prefix := ""
			if approx {
				prefix = "~"
			}
			if f, err := strconv.ParseFloat(tps, 64); err == nil {
				out = fmt.Sprintf("%s%d tok/s", prefix, int(math.Round(f)))
			} else {
				out = fmt.Sprintf("%s%s tok/s", prefix, tps)
			}
		}
		// Time-to-first-token for this turn (latency, not throughput).
		if ttft := get(m, "ttft_ms"); ttft != "" && ttft != "null" {
			if out != "" {
				out += fmt.Sprintf(" · %sms ttft", ttft)
			} else {
				out = fmt.Sprintf("%sms ttft", ttft)
			}
		}
	}

	// Prefix-cache hit rate. cached_tokens present in the live metrics ⇒ this
	// is the turn-end number (fresh); absent ⇒ mid-stream, so carry the last
	// turn's rate and mark it "~" so it isn't mistaken for a live reading.
	fresh := false
	if haveLive {
		c := get(m, "cached_tokens")
		fresh = c != "" && c != "null" && c != "0"
	}
	if s.lastCachePct > 0 {
		cacheStr := fmt.Sprintf("%d%% cached", s.lastCachePct)
		if !fresh {
			cacheStr = "~" + cacheStr
		}
		if out != "" {
			out += " · " + cacheStr
		} else {
			out = cacheStr
		}
	}
	// Context-management reclaim: cumulative tokens freed by digest + compaction
	// and the current rolling summary's size, shown next to the cache stat so the
	// cost/benefit of compaction is visible at a glance.
	if s.tokensSaved > 0 {
		if out != "" {
			out += " · "
		}
		out += fmt.Sprintf("%s saved", compactTokens(s.tokensSaved))
	}
	if s.summaryChars > 0 {
		if out != "" {
			out += " · "
		}
		out += fmt.Sprintf("summary %s chars", compactTokens(uint64(s.summaryChars)))
	}
	// Live Umans account-wide concurrency (used/limit) goes FIRST, ahead of
	// tps/ttft/cached, so it reads "Conc 3/8 · 42 tok/s · …". It is shown even
	// when idle (no turn metrics) because it is polled independently every few
	// seconds — that is the "always live" part. Hidden when not Umans / fetch
	// failed; limit renders ∞ when the plan is unlimited.
	if conc := s.renderUmansConc(); conc != "" {
		if out != "" {
			out = conc + " · " + out
		} else {
			out = conc
		}
	}
	return out
}

// renderUmansConc renders the live concurrency field for the footer, e.g.
// "Conc 3/8". Returns "" (hide) when there is no usage reading (not Umans,
// no key, or the /v1/usage fetch failed), OR when the selected model does NOT
// route to the Umans provider the poll is tracking — a Gemini/OpenAI model
// selected means no conc field, even if a Umans provider is logged in. A null
// limit (unlimited plan) renders as ∞.
func (s *session) renderUmansConc() string {
	if s.umansConcUsed == nil || s.umansConcProvider == "" {
		return ""
	}
	// Only show when the selected model routes to this Umans provider.
	if s.modelIdx < 0 || s.modelIdx >= len(s.models) {
		return ""
	}
	if s.models[s.modelIdx].Provider != s.umansConcProvider {
		return ""
	}
	if s.umansConcLimit == nil {
		return fmt.Sprintf("Conc %d/∞", *s.umansConcUsed)
	}
	return fmt.Sprintf("Conc %d/%d", *s.umansConcUsed, *s.umansConcLimit)
}

// fitRow places left flush and right flush, padding the gap.
func fitRow(width int, left, right string) string {
	if width < 1 {
		return ""
	}
	tl := lipgloss.Width(left)
	if tl > width {
		return lipgloss.NewStyle().MaxWidth(width).Render(left)
	}
	tr := lipgloss.Width(right)
	gap := width - tl - tr
	if gap < 0 {
		return lipgloss.NewStyle().MaxWidth(width).Render(left)
	}
	return left + strings.Repeat(" ", gap) + right
}

func (s *session) renderSeparator() string {
	w := s.width
	if w < 1 {
		w = 1
	}
	return separatorStyle.Render(strings.Repeat("─", w))
}

// compactTokens formats a token count compactly: 940 → "940", 1200 → "1.2k".
func compactTokens(n uint64) string {
	if n < 1000 {
		return fmt.Sprintf("%d", n)
	}
	if n < 1_000_000 {
		return fmt.Sprintf("%.1fk", float64(n)/1000)
	}
	return fmt.Sprintf("%.1fM", float64(n)/1_000_000)
}

// cwdBasename returns the last path component of the working dir, for the header.
func cwdBasename() string {
	wd, err := os.Getwd()
	if err != nil {
		return ""
	}
	b := filepath.Base(wd)
	if b == "." || b == string(filepath.Separator) {
		return ""
	}
	return b
}

// cwdDisplay returns the working dir as a short home-relative path (~/rest),
// falling back to the basename when it's long or off-home. Shown in the header.
func cwdDisplay() string {
	wd, err := os.Getwd()
	if err != nil {
		return ""
	}
	if abs, err := filepath.Abs(wd); err == nil {
		wd = abs
	}
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		if wd == home {
			return "~"
		}
		if rel, err := filepath.Rel(home, wd); err == nil && !strings.HasPrefix(rel, "..") {
			return "~/" + filepath.ToSlash(rel)
		}
	}
	return cwdBasename()
}

// renderContext builds the right-aligned context-budget string: "7% 13.7k/128k"
// using the current model's context window and the cumulative session tokens.
func (s *session) renderContext() string {
	var maxToks uint64
	if len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) {
		maxToks = uint64(s.models[s.modelIdx].ContextWindow)
	}
	cur := s.contextTokens
	if maxToks == 0 {
		return compactTokens(cur) + " tok"
	}
	pct := int(float64(cur) / float64(maxToks) * 100)
	if pct < 1 && cur > 0 {
		pct = 1
	}
	if pct > 100 {
		pct = 100
	}
	// A 10-cell fill bar tinted by context pressure: green < 60%, amber < 85%,
	// red ≥ 85% — so a glance at the footer shows how full the window is.
	const cells = 10
	filled := cells * pct / 100 // truncate so sub-cell pressure stays empty
	ratio := float64(filled) / float64(cells)
	bar := renderContextBar(ratio, cells)
	return bar + " " + mutedStyle.Render(fmt.Sprintf("%d%% %s/%s", pct, compactTokens(cur), compactTokens(maxToks)))
}

// composerPlaceholder returns the empty-input hint, contextualized for busy /
// approval / queue so in-flight controls aren't invisible.
func (s *session) composerPlaceholder() string {
	if s.showingSplash() {
		return "Starting core and checking credentials…"
	}
	if s.coreLifecycle == coreFailed {
		return "Core unavailable — r retry · q quit · see the recovery panel"
	}
	if s.pendingApproval != nil {
		return "Type a follow-up, or clear input to use the approval keys…"
	}
	if s.busy {
		send, close := s.keyHint("send"), s.keyHint("close")
		if send == "" {
			send = "Send"
		}
		if close == "" {
			close = "Close"
		}
		if s.queued != nil {
			return "Queue full — " + close + " cancels queued · again aborts…"
		}
		steer := s.keyHint("steer")
		if steer == "" {
			steer = "Ctrl+Enter"
		}
		return send + " queues · " + close + " aborts · " + steer + " steers · / commands"
	}
	if !s.canSend() {
		return "Log in first — /login · / for commands · ? help"
	}
	if s.input.Placeholder != "" {
		return s.input.Placeholder
	}
	return "Chat with the agent…  (/ commands · ? help)"
}

// keyLabel returns the live binding string for an action, or "".
func (s *session) keyLabel(action string) string {
	if s.keybinds == nil {
		return ""
	}
	return s.keybinds[action]
}

// renderInputBox presents the composer as a labelled command surface. It grows
// downward with wrapped input while its MESSAGE label and SEND affordance stay
// in fixed positions, making the primary action obvious at a glance.
func (s *session) renderInputBox() string {
	if s.viewChrome != nil && s.viewChrome.inputOK {
		return s.viewChrome.inputBox
	}
	out := s.renderInputBoxUncached()
	if s.viewChrome != nil {
		s.viewChrome.inputBox = out
		s.viewChrome.inputOK = true
	}
	return out
}

func (s *session) renderInputBoxUncached() string {
	w := s.width
	if w < 1 {
		w = 1
	}
	if w < 8 {
		return lipgloss.NewStyle().MaxWidth(w).Render(s.inputContent(w))
	}
	// The composer is a rounded surface card. Border + horizontal padding consume
	// four cells; the "❯ " prompt prefix consumes another two on text lines.
	cardInnerW := w - 4
	textW := cardInnerW - 2
	// Attachment chips sit above the typed text so pasted images are visible
	// even when the text field is empty (image-only send).
	var chipLine string
	if n := len(s.pendingImages); n > 0 {
		parts := make([]string, 0, n)
		for i := 0; i < n; i++ {
			parts = append(parts, s.pendingImageLabel(i))
		}
		chip := strings.Join(parts, " ")
		// Hint for detaching — only when there's room.
		detach := s.keyHint("detach_image")
		hint := ""
		if detach != "" {
			hint = "  " + detach + " remove"
		}
		if lipgloss.Width(chip)+lipgloss.Width(hint) <= cardInnerW {
			chipLine = accentStyle.Render(chip) + dimStyle.Render(hint)
		} else {
			chipLine = accentStyle.Render(truncate(chip, cardInnerW))
		}
	}
	content := s.inputContent(textW)
	var lines []string
	if chipLine != "" {
		lines = append(lines, chipLine)
	}
	lines = append(lines, strings.Split(content, "\n")...)
	// Busy / approval hint under the typed text when the box has content so
	// controls stay discoverable even after the placeholder is gone.
	if hint := s.composerHintLine(cardInnerW); hint != "" && s.input.Value() != "" {
		lines = append(lines, hint)
	}
	// A static perimeter is intentional: it keeps focus calm while streamed
	// content changes and gives every terminal the same composer geometry.
	return s.renderComposerStatic(w, cardInnerW, textW, lines)
}

// renderComposerStatic draws a compact focus frame. The label identifies the
// mode; actionable key guidance remains in the footer instead of decorating
// both ends of the border.
func (s *session) renderComposerStatic(w, cardInnerW, textW int, lines []string) string {
	prompt := accentStyle.Render("❯")
	label := accentStyle.Render(" compose ")
	middle := max(0, w-lipgloss.Width(label)-2)
	var out strings.Builder
	out.WriteString(railStyle.Render("╭"))
	out.WriteString(label)
	out.WriteString(railStyle.Render(strings.Repeat("─", middle) + "╮"))
	for i, ln := range lines {
		out.WriteByte('\n')
		row := "  " + ln
		if i == 0 {
			row = prompt + " " + ln
		}
		if gap := cardInnerW - lipgloss.Width(row); gap > 0 {
			row += strings.Repeat(" ", gap)
		}
		out.WriteString(railStyle.Render("│ "))
		out.WriteString(row)
		out.WriteString(railStyle.Render(" │"))
	}
	out.WriteByte('\n')
	out.WriteString(railStyle.Render("╰" + strings.Repeat("─", max(0, w-2)) + "╯"))
	return out.String()
}

// renderInputBoxAnimated draws the composer as a rounded surface card with a
// soft accent "comet" sweeping the top border. Geometry matches the static card
// exactly: only border foreground colors animate → zero layout shift.
func (s *session) renderInputBoxAnimated(w, cardInnerW, textW int, lines []string) string {
	// The comet sweeps the w-2 horizontal cells between the rounded corners.
	P := w - 2
	phase := float64(time.Now().UnixNano()%int64(inflightCycle)) / float64(int64(inflightCycle))
	head := phase * float64(P)

	base := hexRGB(c.railDim)
	accent := hexRGB(c.accent)
	sigma := float64(P) / inflightSigmaDiv
	if sigma < 1 {
		sigma = 1
	}
	ramp := make([]lipgloss.Style, inflightLevels)
	for i := 0; i < inflightLevels; i++ {
		t := float64(i) / float64(inflightLevels-1)
		rgb := blendRGB(base, accent, t)
		ramp[i] = lipgloss.NewStyle().Foreground(lipgloss.Color(fmt.Sprintf("#%02x%02x%02x", rgb[0], rgb[1], rgb[2])))
	}
	styleAt := func(idx int) lipgloss.Style {
		d := float64(idx) - head
		if d < 0 {
			d = -d
		}
		t := math.Exp(-(d * d) / (2 * sigma * sigma))
		li := int(t*float64(inflightLevels-1) + 0.5)
		if li < 0 {
			li = 0
		} else if li >= inflightLevels {
			li = inflightLevels - 1
		}
		return ramp[li]
	}

	var b strings.Builder
	b.WriteString(railStyle.Render("╭"))
	for i := 0; i < P; i++ {
		b.WriteString(styleAt(i).Render("─"))
	}
	b.WriteString(railStyle.Render("╮"))

	prompt := accentStyle.Render("❯")
	left := railStyle.Render("│ ")
	right := railStyle.Render(" │")
	for i, ln := range lines {
		b.WriteByte('\n')
		b.WriteString(left)
		var row string
		if i == 0 {
			row = prompt + " " + ln
		} else {
			row = "  " + ln
		}
		// Pad the row to the card's inner width so the right border aligns.
		if dw := cardInnerW - lipgloss.Width(row); dw > 0 {
			row += strings.Repeat(" ", dw)
		}
		b.WriteString(row)
		b.WriteString(right)
	}

	b.WriteByte('\n')
	b.WriteString(railStyle.Render("╰"))
	b.WriteString(railStyle.Render(strings.Repeat("─", P)))
	b.WriteString(railStyle.Render("╯"))
	return b.String()
}

// inflight animation tuning. The cycle matches the web's 3s gradient sweep;
// the glow half-width scales with the box perimeter so it reads as a single
// moving light rather than the whole border pulsing.
const (
	inflightCycle    = 3 * time.Second // one comet lap
	inflightSigmaDiv = 12.0            // glow = perimeter / sigmaDiv
	inflightLevels   = 32              // brightness ramp steps (smooth on truecolor)
)

// hexRGB parses a #RRGGBB string into its RGB components.
func hexRGB(hex string) [3]int {
	hex = strings.TrimPrefix(hex, "#")
	if len(hex) != 6 {
		return [3]int{}
	}
	n, err := strconv.ParseUint(hex, 16, 32)
	if err != nil {
		return [3]int{}
	}
	return [3]int{int(n >> 16 & 255), int(n >> 8 & 255), int(n & 255)}
}

// blendRGB linearly interpolates between base and target by t∈[0,1].
func blendRGB(base, target [3]int, t float64) [3]int {
	return [3]int{
		int(math.Round(float64(base[0]) + float64(target[0]-base[0])*t)),
		int(math.Round(float64(base[1]) + float64(target[1]-base[1])*t)),
		int(math.Round(float64(base[2]) + float64(target[2]-base[2])*t)),
	}
}

// workingWave animation tuning. The travel cycle is short enough to read as
// motion at the 10 FPS busy clock; the slower breath keeps long runs from
// looking metronomic.
const (
	workingWaveCycle  = 1600 * time.Millisecond // one full wave travel
	workingWaveBreath = 2500 * time.Millisecond // amplitude breathing cycle
)

// workingWaveRamp maps a 0..1 level to a sparkline glyph.
var workingWaveRamp = []rune{' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'}

// renderWorkingWave draws the one-line "agent is working" pulse directly above
// the composer: a full-width sparkline whose cell heights follow two traveling
// sine waves, colored per cell from the dim rail up to the accent at the
// crests (aurora / audio-waveform feel). The busy clock (busyFrameTick)
// re-renders View ~10x/s while busy, so the time-based phase animates without
// a dedicated ticker.
func (s *session) renderWorkingWave() string {
	if s.viewChrome != nil && s.viewChrome.waveOK {
		return s.viewChrome.workingWave
	}
	out := s.renderWorkingWaveUncached()
	if s.viewChrome != nil {
		s.viewChrome.workingWave = out
		s.viewChrome.waveOK = true
	}
	return out
}

func (s *session) renderWorkingWaveUncached() string {
	if !s.busy {
		return ""
	}
	w := s.width
	if w < 1 {
		w = 1
	}
	cells := make([]string, w)
	if s.motionReduced() {
		// Static stand-in: a steady dim mid-level line, no time-based phase.
		mid := string(workingWaveRamp[len(workingWaveRamp)/2])
		for x := range cells {
			cells[x] = dimStyle.Render(mid)
		}
		return strings.Join(cells, "")
	}
	phase := float64(time.Now().UnixNano()%int64(workingWaveCycle)) / float64(int64(workingWaveCycle))
	breath := float64(time.Now().UnixNano()%int64(workingWaveBreath)) / float64(int64(workingWaveBreath))
	amp := 0.7 + 0.3*math.Sin(2*math.Pi*breath)
	l1 := float64(w) / 2.5
	l2 := float64(w) / 5
	base := hexRGB(c.railDim)
	accent := hexRGB(c.accent)
	for x := 0; x < w; x++ {
		v := 0.55*math.Sin(2*math.Pi*(float64(x)/l1)-2*math.Pi*phase) +
			0.45*math.Sin(2*math.Pi*(float64(x)/l2)+2*math.Pi*phase*0.6)
		level := (v + 1) / 2 * amp
		// Fade the outer ~2 cells so the wave melts into the margins.
		if edge := math.Min(float64(x), float64(w-1-x)) / 2; edge < 1 {
			level *= edge
		}
		level = math.Min(math.Max(level, 0), 1)
		ri := int(level*float64(len(workingWaveRamp)-1) + 0.5)
		rgb := blendRGB(base, accent, level)
		cells[x] = lipgloss.NewStyle().
			Foreground(lipgloss.Color(fmt.Sprintf("#%02x%02x%02x", rgb[0], rgb[1], rgb[2]))).
			Render(string(workingWaveRamp[ri]))
	}
	return strings.Join(cells, "")
}

func (s *session) workingWaveHeight() int {
	wv := s.renderWorkingWave()
	if wv == "" {
		return 0
	}
	return lipgloss.Height(wv)
}

// maxInputLines caps the input box height: a very long message shows a
// cursor-centered window (with … markers) instead of consuming the screen.
const maxInputLines = 5

// inputContent renders the chat input value soft-wrapped to width w, with the
// textinput cursor cell placed on the correct wrapped line. Returns the
// placeholder when the value is empty. textinput v2 no longer exposes its
// internal Cursor (SetChar/View) or top-level TextStyle/PlaceholderStyle
// fields, so composer text and cursor colors are derived directly from the
// active theme instead of inheriting the terminal's default grey.
func (s *session) inputContent(w int) string {
	if w < 1 {
		w = 1
	}
	value := s.input.Value()
	// Active style state depends on focus; v2 keeps Focused()/Styles().
	st := s.input.Styles()
	active := st.Focused
	if !s.input.Focused() {
		active = st.Blurred
	}
	if value == "" {
		ph := s.composerPlaceholder()
		// When a subagent is waiting on an intercom reply, make it obvious the
		// chat box below is where you type it (the banner alone reads as
		// "press ↵ to reply", which leads users to hit Enter on an empty box).
		if s.pendingIntercom != nil {
			ph = "Reply to " + s.pendingIntercom.from + "…"
		}
		if ph == "" {
			return ""
		}
		return active.Placeholder.Render(truncateFit(ph, w))
	}
	pos := inputPosition(s.input)
	r := []rune(value)
	if pos < 0 {
		pos = 0
	}
	if pos > len(r) {
		pos = len(r)
	}
	before := r[:pos]
	after := r[pos:] // after[0] is the char under the cursor (if any)

	// beforeLines are the display lines strictly above the cursor's line.
	// wrapRunesMultiline splits on literal '\n' first, then width-wraps each
	// segment, so typed/pasted line breaks render as their own rows instead
	// of being treated as width-1 runes (which broke the box + cursor math).
	beforeLines := wrapRunesMultiline(before, w)
	cLine := len(beforeLines) - 1
	cCol := len(beforeLines[cLine])
	// If the last before-line is exactly full, the cursor wraps to a fresh line.
	if cCol >= w {
		beforeLines = append(beforeLines, []rune{})
		cLine++
		cCol = 0
	}
	// The cursor cell + the remainder after it. When the cursor sits directly on
	// a line break, show an empty cell and force everything that follows onto
	// subsequent display lines (the '\n' just ends the current line).
	curChar := " "
	rest := []rune(nil)
	newlineConsumed := false
	if len(after) > 0 {
		if after[0] == '\n' {
			newlineConsumed = true
			rest = after[1:]
		} else {
			curChar = string(after[0])
			rest = after[1:]
		}
	}
	// How much of `rest` fits on the cursor line (after the cursor cell). It
	// also must not cross a '\n' (a line break ends the cursor line).
	avail := w - cCol - 1
	if avail < 0 {
		avail = 0
	}
	limit := avail
	for i, ch := range rest {
		if ch == '\n' {
			if i < limit {
				limit = i
			}
			break
		}
	}
	if limit > len(rest) {
		limit = len(rest)
	}
	restOnLine := rest[:limit]
	var restAfter []rune
	if limit < len(rest) && rest[limit] == '\n' {
		// The line break ends the cursor line; consume it so it doesn't render
		// as a spurious empty row. The content after it starts the next line.
		newlineConsumed = true
		restAfter = rest[limit+1:]
	} else {
		restAfter = rest[limit:]
	}
	// Display lines below the cursor line. A consumed newline guarantees at
	// least one following line (even when empty) so the break stays visible.
	var restLines [][]rune
	if newlineConsumed || len(restAfter) > 0 {
		restLines = wrapRunesMultiline(restAfter, w)
	}

	styleText := composerTextStyle().Render
	out := make([]string, 0, cLine+1+len(restLines)+1)
	for i := 0; i < cLine; i++ {
		out = append(out, styleText(string(beforeLines[i])))
	}
	// cursor line: text before cursor + cursor cell + text after (on this line).
	// v2 dropped textinput's internal Cursor.SetChar/View. Use explicit theme
	// colors here: Reverse(true) inherits the terminal's foreground/background,
	// which produced the same light-grey cursor in every theme.
	line := styleText(string(beforeLines[cLine]))
	if s.input.Focused() {
		line += composerCursorStyle().Render(curChar)
	} else {
		line += styleText(curChar)
	}
	if len(restOnLine) > 0 {
		line += styleText(string(restOnLine))
	}
	out = append(out, line)
	for _, rl := range restLines {
		out = append(out, styleText(string(rl)))
	}
	// Cap the box height: if the wrapped message is very long, show a window
	// centered on the cursor line so the box never eats the whole screen
	// (the cursor always stays visible). “…” markers flag hidden content.
	cursorIdx := cLine
	if len(out) > maxInputLines {
		half := maxInputLines / 2
		start := cursorIdx - half
		if start < 0 {
			start = 0
		}
		end := start + maxInputLines
		if end > len(out) {
			end = len(out)
			start = max(0, end-maxInputLines)
		}
		var capped []string
		if start > 0 {
			capped = append(capped, dimStyle.Render("…"))
		}
		capped = append(capped, out[start:end]...)
		if end < len(out) {
			capped = append(capped, dimStyle.Render("…"))
		}
		out = capped
	}
	return strings.Join(out, "\n")
}

func composerTextStyle() lipgloss.Style {
	if colorsDisabled() {
		return lipgloss.NewStyle().Inline(true)
	}
	// Inline(true) lets the composer card's surface fill show through around
	// glyphs (lipgloss pads the fill) instead of the text repainting its own bg.
	return lipgloss.NewStyle().Foreground(lipgloss.Color(c.fg)).Inline(true)
}

func composerCursorStyle() lipgloss.Style {
	if colorsDisabled() {
		return lipgloss.NewStyle().Reverse(true)
	}
	// A solid accent cell cursor: the web caret-glow as a block.
	return lipgloss.NewStyle().
		Foreground(lipgloss.Color(c.bg)).
		Background(lipgloss.Color(c.accent)).
		Bold(true)
}

// wrapRunesMultiline splits r on literal '\n' then width-wraps each segment
// via wrapRunes, producing the full set of display rows for a value that may
// contain typed/pasted line breaks. A trailing newline yields a final empty
// row (so the user sees the blank line they entered). Never returns empty.
func wrapRunesMultiline(r []rune, w int) [][]rune {
	if w < 1 {
		w = 1
	}
	var out [][]rune
	start := 0
	for i, ch := range r {
		if ch == '\n' {
			out = append(out, wrapRunes(r[start:i], w)...)
			start = i + 1
		}
	}
	out = append(out, wrapRunes(r[start:], w)...)
	if len(out) == 0 {
		out = [][]rune{{}}
	}
	return out
}

// wrapRunes hard-wraps a rune slice to at most w runes per line. Hard (not
// word) wrapping keeps the cursor column math exact for the input box.
func wrapRunes(r []rune, w int) [][]rune {
	if w < 1 {
		w = 1
	}
	var lines [][]rune
	for len(r) > 0 {
		n := w
		if n > len(r) {
			n = len(r)
		}
		line := make([]rune, n)
		copy(line, r[:n])
		lines = append(lines, line)
		r = r[n:]
	}
	if len(lines) == 0 {
		lines = [][]rune{{}}
	}
	return lines
}

// inputBoxHeight is the rendered input box height so layout() reserves the
// right number of lines as the box grows with wrapping. Measured from the
// actual render so it can never disagree with View().
func (s *session) inputBoxHeight() int {
	return lipgloss.Height(s.renderInputBox())
}

// footerHeight is the input box + the status rail beneath it.
func (s *session) footerHeight() int {
	return s.inputBoxHeight() + lipgloss.Height(s.renderFooter())
}

// renderActiveTasks draws a bordered "active tasks" panel listing in-flight
// scout (spawn) tool calls with their model and live elapsed time. Returns ""
// when nothing is in flight. Tools run sequentially in the core, so this shows
// real work, not fabricated parallelism.
// ponytail: per-scout token counts aren't emitted by the core; the aggregate
// context budget is shown in the footer instead. Add per-scout metrics if the
// core grows a subagent-usage event.
func (s *session) renderActiveTasks(w int) string {
	if s.height < 12 || len(s.subProgress) == 0 || s.maxTaskRows == 0 {
		return ""
	}
	entries := append([]*subProgressEntry(nil), s.subProgress...)
	// Surface potentially stuck work first; it is more actionable than a
	// freshly-started run when the panel must collapse.
	sort.SliceStable(entries, func(i, j int) bool {
		istuck := entries[i].toolRunning && time.Since(entries[i].toolStart) > 30*time.Second
		jstuck := entries[j].toolRunning && time.Since(entries[j].toolStart) > 30*time.Second
		return istuck && !jstuck
	})
	hidden := 0
	if len(entries) > s.maxTaskRows {
		hidden = len(entries) - s.maxTaskRows
		entries = entries[:s.maxTaskRows]
	}
	var rows []string
	for _, e := range entries {
		elapsed := formatDur(time.Since(e.started))
		head := accentStyle.Render("◷ ") + boldBaseStyle.Render(e.agent) +
			dimStyle.Render(" · "+elapsed+" · "+strconv.Itoa(e.toolCount)+" tools · "+compactTokens(e.tokensIn)+"+"+compactTokens(e.tokensOut)+" tok")
		rows = append(rows, head)
		if e.curTool != "" {
			td := time.Since(e.toolStart)
			icon := toolIcon(e.curTool)
			marker := "  " + icon + " "
			if e.toolRunning && td > 30*time.Second {
				rows = append(rows, warnStyle.Render(marker+"⚠ "+e.curTool+" STUCK "+formatDur(td)))
			} else if e.toolRunning {
				rows = append(rows, dimStyle.Render(marker+e.curTool+" · "+formatDur(td)))
			} else {
				rows = append(rows, dimStyle.Render(marker+e.curTool+" ✓"))
			}
		}
	}
	title := fmt.Sprintf("subagents %d/%d active", len(entries), len(s.subProgress))
	if hidden > 0 {
		title += fmt.Sprintf(" · +%d hidden", hidden)
	}
	body := accentStyle.Render(title) + "\n" + strings.Join(rows, "\n")
	boxW := max(1, w-4) // border + horizontal padding consume four cells
	return lipgloss.NewStyle().
		BorderStyle(lipgloss.RoundedBorder()).
		BorderForeground(lipgloss.Color(c.railDim)).
		Padding(0, 1).
		Width(boxW).MaxWidth(max(1, w)).MaxHeight(max(1, s.height)).
		Render(body)
}

// activeTasksHeight is the lines the active-tasks panel claims (0 when none),
// so layout() can shrink the viewport to make room.
func (s *session) activeTasksHeight() int {
	p := s.renderActiveTasks(s.width)
	if p == "" {
		return 0
	}
	return lipgloss.Height(p)
}

// approvalHeight is the lines the sticky approval banner (plus its diff
// preview, when present) claims, so layout() shrinks the viewport to fit.
func (s *session) approvalHeight() int {
	if s.pendingApproval == nil {
		return 0
	}
	return lipgloss.Height(s.renderApprovalBanner())
}

// maxPinnedTodos caps the always-visible todo panel so a long list never eats
// the whole screen (overflow collapses to a "… +N more" hint).
const maxPinnedTodos = 5

// renderTodoPanel draws a persistent, always-visible checklist of the latest
// todo_write state so the plan/progress is glanceable without scrolling the
// transcript. Returns "" when there are no todos.
func (s *session) renderTodoPanel() string {
	if s.height < 12 {
		return ""
	}
	todos := s.todos
	if len(todos) == 0 {
		return ""
	}
	done, pend, run := countTodoStatuses(todos)
	head := accentStyle.Render("tasks") +
		dimStyle.Render(fmt.Sprintf("  · %d items (%d✓ %d○ %d•)", len(todos), done, run, pend))
	if s.pendingApproval != nil {
		return head // preserve decision context; expand the checklist after approval
	}
	// inner content width = boxW - 2(border) - 2(padding); each row indents 2 + "[✓] " 4.
	cw := s.width - 2 - 4 - 2 - 4
	if cw < 4 {
		cw = 4
	}
	rows := make([]string, 0, len(todos))
	for _, t := range todos {
		ck, st := todoCheckbox(get(t, "status"))
		rows = append(rows, "  "+st.Render(ck+" ")+baseStyle.Render(truncate(get(t, "subject"), cw)))
	}
	more := 0
	if len(rows) > maxPinnedTodos {
		more = len(rows) - maxPinnedTodos
		rows = rows[:maxPinnedTodos]
	}
	body := head
	for _, r := range rows {
		body += "\n" + r
	}
	if more > 0 {
		body += "\n" + dimStyle.Italic(true).Render(fmt.Sprintf("  … +%d more", more))
	}
	boxW := max(1, s.width-4) // border + horizontal padding consume four cells
	return lipgloss.NewStyle().
		BorderStyle(lipgloss.RoundedBorder()).
		BorderForeground(lipgloss.Color(c.railDim)).
		Padding(0, 1).
		Width(boxW).MaxWidth(max(1, s.width)).MaxHeight(max(1, s.height)).
		Render(body)
}

func (s *session) todoPanelHeight() int {
	p := s.renderTodoPanel()
	if p == "" {
		return 0
	}
	return lipgloss.Height(p)
}

// renderQueueBanner is a one-line sticky banner shown while a follow-up or
// steer prompt is buffered (one-deep) behind the running turn. It labels the
// kind and reminds the user Esc cancels just the queued message (vs a bare
// abort). Returns "" when nothing is queued.
func (s *session) renderQueueBanner() string {
	q := s.queued
	if q == nil {
		return ""
	}
	label := "⏳ queued follow-up"
	if q.kind == "steer" {
		label = "⤴ queued steer"
	}
	avail := s.width - len(label) - 24
	if avail < 6 {
		avail = 6
	}
	msg := fmt.Sprintf("%s: %s", label, truncate(q.text, avail))
	if key := s.keyHint("close"); key != "" {
		msg += "   " + key + " to cancel"
	}
	return lipgloss.NewStyle().
		Width(max(1, s.width-2)).MaxWidth(max(1, s.width)).
		Background(lipgloss.Color(c.user)).
		Foreground(lipgloss.Color(c.bg)).
		Bold(true).
		Padding(0, 1).
		Render(msg)
}

func (s *session) queueBannerHeight() int {
	if s.queued == nil {
		return 0
	}
	return 1
}

// renderActivityShelf is the single home for transient work below the
// transcript. Decision states take exclusive focus; routine tasks and
// subagents collapse into one row and can be expanded without competing with
// the composer or hiding the conversation.
func (s *session) renderActivityShelf() string {
	if s.viewChrome != nil && s.viewChrome.shelfOK {
		return s.viewChrome.activityShelf
	}
	out := s.renderActivityShelfUncached()
	if s.viewChrome != nil {
		s.viewChrome.activityShelf = out
		s.viewChrome.shelfOK = true
	}
	return out
}

func (s *session) renderActivityShelfUncached() string {
	switch {
	case s.pendingApproval != nil:
		return s.renderApprovalBanner()
	case s.pendingIntercom != nil:
		return s.renderIntercomBanner()
	case s.queued != nil:
		return s.renderQueueBanner()
	}
	if len(s.todos) == 0 && len(s.subProgress) == 0 {
		if s.goalState != nil && goalShowsProgressPanel(s.goalState.Phase, s.goalState.AutoDeploy) {
			settled, total := s.goalProgressCounts()
			phaseLabel := goalProgressPhaseLabel(s.goalState.Phase, s.goalState.AutoDeploy)
			label := accentStyle.Render("◈ ") +
				baseStyle.Render(fmt.Sprintf("Goal %s · %d/%d steps", phaseLabel, settled, total))
			return surfaceStyle.Padding(0, 1).Render(label)
		}
		return ""
	}
	done, _, running := countTodoStatuses(s.todos)
	parts := make([]string, 0, 3)
	if s.goalState != nil && goalShowsProgressPanel(s.goalState.Phase, s.goalState.AutoDeploy) {
		settled, total := s.goalProgressCounts()
		phaseLabel := goalProgressPhaseLabel(s.goalState.Phase, s.goalState.AutoDeploy)
		parts = append(parts, fmt.Sprintf("Goal %s · %d/%d", phaseLabel, settled, total))
	}
	if len(s.subProgress) > 0 {
		parts = append(parts, fmt.Sprintf("Subagents %d active", len(s.subProgress)))
	}
	if len(s.todos) > 0 {
		parts = append(parts, fmt.Sprintf("Tasks %d/%d complete · %d active", done, len(s.todos), running))
	}
	toggle := s.keyHint("toggle_activity")
	if toggle == "" {
		toggle = "Ctrl+G"
	}
	if !s.activityExpanded {
		label := accentStyle.Render("◷ ") + baseStyle.Render(strings.Join(parts, " · ")) +
			dimStyle.Render("   "+toggle+" expand")
		return " " + label
	}
	var details []string
	if len(s.subProgress) > 0 {
		details = append(details, accentStyle.Render("Subagents"))
	}
	for _, agent := range s.subProgress {
		detail := agent.agent + " · " + formatDur(time.Since(agent.started))
		if agent.curTool != "" {
			detail += " · " + toolIcon(agent.curTool) + " " + agent.curTool
		}
		details = append(details, accentStyle.Render("◷ ")+baseStyle.Render(truncate(detail, max(8, s.width-10))))
	}
	if len(s.todos) > 0 {
		details = append(details, accentStyle.Render("Tasks"))
	}
	for _, todo := range s.todos {
		mark, st := todoCheckbox(get(todo, "status"))
		details = append(details, st.Render(mark+" ")+baseStyle.Render(truncate(get(todo, "subject"), max(8, s.width-10))))
	}
	limit := max(2, min(7, s.height/3-2))
	maxScroll := max(0, len(details)-limit)
	s.activityScroll = min(max(0, s.activityScroll), maxScroll)
	end := min(len(details), s.activityScroll+limit)
	position := ""
	if len(details) > limit {
		position = fmt.Sprintf(" · %d–%d/%d", s.activityScroll+1, end, len(details))
	}
	lines := []string{accentStyle.Render("Activity") + dimStyle.Render(" · ↑↓ scroll · Esc close"+position)}
	lines = append(lines, details[s.activityScroll:end]...)
	return recessedStyle.Width(max(1, s.width-2)).MaxWidth(max(1, s.width)).Render(strings.Join(lines, "\n"))
}

func (s *session) activityShelfHeight() int {
	p := s.renderActivityShelf()
	if p == "" {
		return 0
	}
	return lipgloss.Height(p)
}

func (s *session) renderToast() string {
	if s.height < 10 || s.toast == nil {
		return ""
	}
	if time.Now().After(s.toast.until) {
		s.toast = nil
		return ""
	}
	style := mutedStyle
	prefix := "· "
	switch s.toast.kind {
	case toastSuccess:
		style = successStyle
		prefix = "✓ "
	case toastWarn:
		style = warnStyle
		prefix = "! "
	case toastError:
		style = errStyle
		prefix = "✗ "
	}
	msg := truncate(prefix+s.toast.text, max(8, s.width-2))
	return style.Render(msg)
}

func (s *session) toastHeight() int {
	if s.renderToast() == "" {
		return 0
	}
	return 1
}

func (s *session) renderOauthBanner() string {
	o := s.oauth
	if o == nil {
		return ""
	}
	msg := o.message
	if msg == "" {
		msg = "OAuth login"
	}
	extra := ""
	if o.code != "" {
		extra = " · code " + o.code
	}
	line := fmt.Sprintf("🔑 %s%s · URL on clipboard", msg, extra)
	if o.url != "" && s.width > 60 {
		// Show a short URL tail when there's room; full URL is clipboarded.
		u := o.url
		if len(u) > 48 {
			u = u[:45] + "…"
		}
		line = fmt.Sprintf("🔑 %s%s · %s", msg, extra, u)
	}
	return lipgloss.NewStyle().
		Width(max(1, s.width-2)).MaxWidth(max(1, s.width)).
		Background(lipgloss.Color(c.accent)).
		Foreground(lipgloss.Color(c.bg)).
		Bold(true).
		Padding(0, 1).
		Render(truncate(line, max(8, s.width-2)))
}

func (s *session) oauthBannerHeight() int {
	if s.oauth == nil {
		return 0
	}
	return 1
}

func (s *session) View() tea.View {
	if s.reuseLastView && s.hasLastView {
		s.reuseLastView = false
		return s.lastView
	}
	s.reuseLastView = false
	var content string
	if !s.ready {
		// Pre-layout boot: show the branded splash at a sensible default size so
		// the first paint isn't a bare one-liner while we wait for WindowSizeMsg.
		w, h := s.width, s.height
		if w < 40 {
			w = 80
		}
		if h < 10 {
			h = 24
		}
		content = s.renderSplashScreen(w, h)
	} else {
		// Recompute the viewport height from the CURRENT input-box + tasks-panel
		// height on every render. This is the single source of truth: paths that
		// mutate the input (insertNewline on Shift+Enter, paste) or the tasks panel
		// (mid-run tool updates) don't all call relayoutHeights themselves, so doing
		// it here guarantees the viewport shrinks to fit before we render — no
		// overflow that pushes the footer off-screen between events. Chrome strings
		// are cached for this View so measure-by-render != second full paint.
		s.beginViewChrome()
		defer s.endViewChrome()
		s.relayoutHeights()
		parts := []string{s.renderHeader()}
		if b := s.renderCoreFailureBanner(); b != "" {
			parts = append(parts, b)
		}
		if b := s.renderUpdateBanner(); b != "" && s.height >= 10 {
			parts = append(parts, b)
		}
		if o := s.renderOauthBanner(); o != "" {
			parts = append(parts, o)
		}
		// Selection is painted over the viewport's already-cropped rows. This
		// keeps drag cost proportional to terminal height instead of transcript
		// length, which matters for large selections and long sessions.
		parts = append(parts, s.renderVisibleTranscriptSelection(s.viewport.View()))
		if p := s.renderPositionBar(); p != "" {
			parts = append(parts, p)
		}
		if shelf := s.renderActivityShelf(); shelf != "" {
			parts = append(parts, shelf)
		}
		if gp := s.renderGoalProgressPanel(s.width); gp != "" {
			parts = append(parts, gp)
		}
		if f := s.renderMentionFlyout(); f != "" {
			parts = append(parts, f)
		}
		if wv := s.renderWorkingWave(); wv != "" {
			parts = append(parts, wv)
		}
		parts = append(parts, s.renderInputBox(), s.renderFooter())
		view := strings.Join(parts, "\n")
		if s.modal.kind != modalNone {
			view = s.renderModalOverlay(view)
			// Cache exactly what is on-screen before adding selection styling.
			// Mouse coordinates for modals are screen-relative, unlike transcript
			// coordinates, so the complete placed overlay is the selection surface.
			s.modalPlain = plainTranscriptLines(view)
			view = s.renderModalSelection(view)
		} else {
			s.modalPlain = nil
		}
		// ask flyout: a blocking `ask` prompt renders as a centered overlay on
		// top of the full view (like the modal above). renderAskOverlay is a
		// no-op (returns base unchanged) when s.pendingAsk is nil.
		content = s.renderAskOverlay(view)
		// sudo flyout: a blocking sudo_request (bash command invokes sudo) renders
		// as a centered overlay with a password field. No-op when nil.
		content = s.renderSudoOverlay(content)
		// Final containment is an invariant, not a best effort. Do not paint a
		// background around the multiline view: terminals carry that SGR color
		// across the unused remainder of each row, creating large rectangles after
		// short text. Individual banners/modals still own their intentional fills.
		content = constrainViewContent(content, s.width, s.height)
		if colorsDisabled() {
			content = noColorANSIRe.ReplaceAllString(content, "")
		}
	}
	v := tea.NewView(content)
	// v2 is declarative: alt-screen + mouse mode are View fields, not program
	// options. The renderer also always enables Kitty progressive-keyboard
	// disambiguation + xterm modifyOtherKeys level 2 (restoring them on exit),
	// so modified keys (Shift/Ctrl+Enter, Esc) arrive as real KeyPressMsgs.
	v.AltScreen = !plainTerminalMode()
	// Cell motion supplies click, release, drag, and wheel events. Transcript
	// selection is application-managed, so mouse tracking can stay enabled
	// without sacrificing drag-to-copy.
	v.MouseMode = tea.MouseModeCellMotion
	// Focus events deliver tea.FocusMsg when the terminal window is
	// (un)focused; used to silence the window-title attention bell the moment
	// the user comes back (see window_title.go).
	v.ReportFocus = true
	// Window title = project name; the busy spinner animates it via the
	// busy-frame clock and a bell marks completed turns / attention prompts
	// (see window_title.go). The renderer emits OSC 2 only on change and
	// clears the title on exit.
	v.WindowTitle = s.windowTitle()
	s.lastView = v
	s.hasLastView = true
	return v
}

func constrainViewContent(content string, width, height int) string {
	return lipgloss.NewStyle().
		MaxWidth(max(1, width)).
		MaxHeight(max(1, height)).
		Render(content)
}
