package main

import (
	"encoding/json"
	"fmt"
	"html"
	"path/filepath"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

// ---------------------------------------------------------------------------
// Block model
//
// The conversation is a list of typed blocks rather than a raw string buffer.
// Streaming blocks (assistant, thinking) accumulate into .text; the rest are
// set once. Finalized blocks are rendered once and cached; the live streaming
// block is re-rendered per token, so per-token cost stays O(1) amortized.
// ---------------------------------------------------------------------------

type blockKind int

const (
	blkUser blockKind = iota
	blkAssistant
	blkThinking
	blkTool
	blkToolResult
	blkAdvisor // durable watchdog review record; finalized in place from advisor_status/note
	blkInfo
	blkSuccess
	blkWarn
	blkError
	blkApprove
	blkTrimmed // synthetic marker showing that older in-memory history was evicted
	blkRaw     // pre-styled string rendered verbatim (e.g. the /model list)
)

type approvalBlockState uint8

const (
	approvalPending approvalBlockState = iota
	approvalApproved
	approvalAlways
	approvalDenied
)

type block struct {
	kind         blockKind
	text         strings.Builder // streamed content (assistant / thinking) or raw
	name         string          // tool name (blkTool / blkApprove)
	args         string          // tool args
	output       string          // blkToolResult
	diff         string          // blkTool: unified-diff text (tool_result only); shown instead of output when present
	started      time.Time       // blkTool: when the call began
	dur          time.Duration   // blkTool: elapsed until result
	collapsed    bool            // blkThinking only
	model        string          // blkAssistant: model id (for the role line)
	sub          bool            // blkTool: a spawn:* sub-agent internal call (collapsed)
	id           string          // blkTool: tool-call id, to match results out of order
	expanded     bool            // blkTool / blkToolResult: full output shown (ctrl+o)
	ok           bool            // blkTool: outcome.ok from tool_result (false on error/deny)
	hasOk        bool            // blkTool: true once a result landed (distinguishes in-flight)
	advisorState string          // blkAdvisor: reviewing/clear/finding/failed/etc.
	renderW      int             // P1-12: width the streaming block was last rendered at
	renderLen    int             // P1-12: text length at last render (throttle)
	renderStr    string          // cached render (pre-decoration) keyed by renderW + renderTheme
	renderTheme  string          // activeTheme.name when renderStr was produced (theme invalidation)
	renderStart  int             // first rendered transcript line (zero based)
	renderEnd    int             // last rendered transcript line (inclusive)
	approval     approvalBlockState
	trimmed      int // blkTrimmed: number of evicted blocks
}

// push appends a block and updates the streaming cursor. Streaming kinds
// (assistant, thinking) become s.cur; everything else finalizes s.cur=nil so
// the previous streaming block gets cached.
// maxBlocks caps the in-memory transcript so a long session doesn't grow
// `blocks` + the pre-rendered `cache` (~3x the transcript) without bound (P1-16).
const maxBlocks = 400

func (s *session) push(kind blockKind) *block {
	// A new block closes the previous streaming block. Clear its throttled
	// render snapshot so a final short delta cannot be lost when that block is
	// moved into the immutable prefix cache.
	if s.cur != nil && (s.cur.kind == blkAssistant || s.cur.kind == blkThinking) {
		s.cur.renderStr = ""
		s.cur.renderLen = 0
	}
	b := &block{kind: kind}
	if kind == blkThinking {
		b.collapsed = !s.thinkExpanded
	}
	s.blocks = append(s.blocks, b)
	if len(s.blocks) > maxBlocks {
		// Drop the oldest finalized blocks and reset the render cache (its prefix
		// no longer matches the shifted indices). `s.cur` is always the newest
		// block, so it's never trimmed.
		trim := len(s.blocks) - (maxBlocks - 1)
		trimmed := trim
		if len(s.blocks) > 0 && s.blocks[0] != nil && s.blocks[0].kind == blkTrimmed {
			trim = len(s.blocks) - maxBlocks
			trimmed = trim + s.blocks[0].trimmed
			trim++ // discard the old marker as well
		}
		// Copy into a fresh backing slice instead of slicing (s.blocks[trim:]).
		// Slicing alone keeps the old backing array — and the *block pointers in
		// its dropped prefix — alive for the whole session, pinning the trimmed
		// blocks' rendered strings and slowly creeping RSS. A fresh copy lets the
		// GC reclaim the old array and its stale prefix.
		kept := make([]*block, 0, maxBlocks+8)
		kept = append(kept, &block{kind: blkTrimmed, trimmed: trimmed})
		kept = append(kept, s.blocks[trim:]...)
		s.blocks = kept
		if s.focusedBlock >= 0 {
			s.focusedBlock = s.focusedBlock - trim + 1 // new marker occupies index 0
			if s.focusedBlock < 0 {
				s.focusedBlock = -1
			}
		}
		s.invalidateAll()
	}
	if kind == blkAssistant || kind == blkThinking {
		s.cur = b
	} else {
		s.cur = nil
	}
	return b
}

// invalidateAll drops the prefix render cache so the transcript rebuilds from
// block 0 on the next renderBlocks. Per-block cached renders (renderStr, keyed
// by width + theme) are deliberately preserved: immutable post-finalize blocks
// (assistant/thinking/user) skip Glamour+lipgloss on rebuild, so toggles, focus
// moves, and tool results stay cheap even with a large expanded reasoning
// block. Width changes invalidate via the renderW key; theme changes via
// renderTheme; content changes (toggles) clear the affected block's renderStr
// explicitly at the mutation site.
func (s *session) invalidateAll() {
	s.cache.Reset()
	s.cacheIdx = 0
	s.cacheLines = 0
	for _, b := range s.blocks {
		if b != nil {
			b.renderStart, b.renderEnd = 0, -1
		}
	}
}

// renderBlocks returns the full viewport content. Finalized blocks are cached
// in s.cache and only extended; the live streaming block (s.cur) and any
// in-flight tool blocks (awaiting their result) are re-rendered each call so
// the active-tasks panel's elapsed time stays live.
// ponytail: full re-render (invalidateAll) only on resize/toggle; very long
// histories re-wrap the current streaming block per token (upgrade: cache
// wrapped lines per finalized block keyed by width).
// hasConversation reports whether the transcript contains real chat turns
// (user/assistant/tools). Status/error-only blocks do not count, so the
// welcome screen survives startup chatter and failed auth probes.
func (s *session) hasConversation() bool {
	for _, b := range s.blocks {
		if b == nil {
			continue
		}
		switch b.kind {
		case blkUser, blkAssistant, blkThinking, blkTool, blkToolResult, blkAdvisor, blkApprove:
			return true
		}
	}
	return false
}

func (s *session) renderBlocks() string {
	if s.coreLifecycle == coreFailed && !s.hasConversation() {
		return s.renderCoreFailure()
	}
	if !s.hasConversation() {
		return s.renderWelcome()
	}
	w := s.viewport.Width()
	for s.cacheIdx < len(s.blocks) {
		blk := s.blocks[s.cacheIdx]
		if blk == s.cur || isInFlight(blk) {
			break // don't cache the live streaming block or in-flight tools
		}
		if isToolActivityBlock(blk) {
			end := toolActivityRunEnd(s.blocks, s.cacheIdx)
			// Keep the trailing run live. A model often emits several calls in
			// succession; waiting for the next conversation block lets them merge
			// into one activity group instead of freezing several tiny groups in
			// the prefix cache.
			if end == len(s.blocks) || toolActivityRunInFlight(s.blocks[s.cacheIdx:end]) {
				break
			}
			start := s.cacheLines
			rendered := s.renderToolActivity(s.cacheIdx, end, w, start)
			h := lipgloss.Height(rendered)
			s.cache.WriteString(rendered)
			s.cache.WriteString("\n\n")
			s.cacheLines += h + 1
			for _, toolBlock := range s.blocks[s.cacheIdx:end] {
				releaseAfterCache(toolBlock)
			}
			s.cacheIdx = end
			continue
		}
		start := s.cacheLines
		rendered := s.renderBlock(blk, w)
		h := lipgloss.Height(rendered)
		blk.renderStart, blk.renderEnd = start, start+h-1
		s.cache.WriteString(rendered)
		s.cache.WriteString("\n\n") // breathing room between blocks
		// Two newline bytes after the last content row create one blank row,
		// so the next block starts h+1 lines after this one.
		s.cacheLines += h + 1
		// Drop streaming residue and shrink tool payloads once the card is in
		// the prefix cache — display comes from s.cache until invalidate.
		releaseAfterCache(blk)
		s.cacheIdx++
	}
	var b strings.Builder
	b.WriteString(s.cache.String())
	lineBase := s.cacheLines
	// Everything after the cached prefix is rendered in order. In practice this
	// is a live assistant/reasoning block, a trailing activity run, or both.
	// Keeping this as an ordered walk avoids tool calls visually jumping ahead
	// of the reasoning/commentary that introduced them.
	for i := s.cacheIdx; i < len(s.blocks); {
		blk := s.blocks[i]
		if isToolActivityBlock(blk) {
			end := toolActivityRunEnd(s.blocks, i)
			rendered := s.renderToolActivity(i, end, w, lineBase)
			if rendered != "" {
				h := lipgloss.Height(rendered)
				b.WriteString(rendered)
				b.WriteString("\n\n")
				lineBase += h + 1
			}
			i = end
			continue
		}
		rendered := s.renderBlock(blk, w)
		h := lipgloss.Height(rendered)
		blk.renderStart, blk.renderEnd = lineBase, lineBase+h-1
		b.WriteString(rendered)
		b.WriteString("\n\n")
		lineBase += h + 1
		i++
	}
	return strings.TrimRight(b.String(), "\n")
}

// isToolActivityBlock identifies calls/results that belong in the compact
// activity treatment. Approval decisions remain standalone because they are
// user interaction, not background agent work.
func isToolActivityBlock(b *block) bool {
	return b != nil && (b.kind == blkTool || b.kind == blkToolResult)
}

func toolActivityRunEnd(blocks []*block, start int) int {
	end := start
	for end < len(blocks) && isToolActivityBlock(blocks[end]) {
		end++
	}
	return end
}

func toolActivityRunInFlight(blocks []*block) bool {
	for _, b := range blocks {
		if isInFlight(b) {
			return true
		}
	}
	return false
}

// renderToolActivity turns a noisy run of tool cards into a single visual
// unit. Collapsed calls are one-line summaries; Ctrl+O on the nearest/focused
// call swaps only that row for the existing full, tool-specific renderer.
// renderStart/renderEnd stay per-call so transcript navigation and nearest-
// output discovery continue to target the right call inside a group.
func (s *session) renderToolActivity(start, end, w, lineBase int) string {
	if start < 0 || end > len(s.blocks) || start >= end {
		return ""
	}
	visible := make([]*block, 0, end-start)
	failed, running := 0, 0
	for _, b := range s.blocks[start:end] {
		// A live scout already has a richer pinned progress surface. Its nested
		// operations can still join this run, and the completed spawn call returns
		// to the transcript as a normal summary row.
		if b.kind == blkTool && b.inFlight() && (b.name == "spawn" || b.name == "subagent") {
			b.renderStart, b.renderEnd = 0, -1
			continue
		}
		visible = append(visible, b)
		if b.kind == blkTool {
			if b.inFlight() {
				running++
			} else if b.hasOk && !b.ok {
				failed++
			}
		}
	}
	if len(visible) == 0 {
		return ""
	}

	label := fmt.Sprintf("%d call%s", len(visible), pluralS(len(visible)))
	var state string
	switch {
	case failed > 0 && running > 0:
		state = fmt.Sprintf(" · %d failed · %d running", failed, running)
	case failed > 0:
		state = fmt.Sprintf(" · %d failed", failed)
	case running > 0:
		state = fmt.Sprintf(" · %d running", running)
	}
	allExpanded, inspectable := true, 0
	for _, b := range visible {
		if b == nil || b.sub {
			continue
		}
		inspectable++
		allExpanded = allExpanded && b.expanded
	}
	if inspectable == 0 {
		allExpanded = false
	}
	disclosure := "▸"
	if allExpanded {
		disclosure = "▾"
	}
	innerW := max(8, w-4) // card border + padding
	heading := roleToolStyle.Render(disclosure+" activity") + dimStyle.Render(" · "+label+state)
	if key := s.keyHint("toggle_tool_output"); key != "" {
		heading = fitRow(innerW, heading, keyHintStyle.Render("click rows · "+key+" details"))
	}

	var out strings.Builder
	out.WriteString(heading)
	line := lineBase + 1 // heading occupies the first line
	for _, b := range visible {
		out.WriteByte('\n')
		var rendered string
		if b.expanded {
			if b.kind == blkToolResult {
				rendered = indentToolDetail(s.renderKeyHints(s.renderBlockFull(b, innerW)))
			} else {
				rendered = indentToolDetail(s.renderKeyHints(s.renderToolBlock(b, innerW)))
			}
		} else {
			rendered = s.renderCompactToolRow(b, innerW)
		}
		h := lipgloss.Height(rendered)
		b.renderStart, b.renderEnd = line, line+h-1
		out.WriteString(rendered)
		line += h
	}
	return cardStyle.Width(w).Render(out.String())
}

func indentToolDetail(detail string) string {
	lines := strings.Split(detail, "\n")
	for i := range lines {
		lines[i] = "  " + lines[i]
	}
	return strings.Join(lines, "\n")
}

func (s *session) renderCompactToolRow(b *block, w int) string {
	prefix := ""
	if s.focusedBlock >= 0 && s.focusedBlock < len(s.blocks) && s.blocks[s.focusedBlock] == b {
		prefix = accentStyle.Render("▸ ")
	}
	if b.kind == blkToolResult {
		lines := outputLineCount(b.output)
		detail := "result"
		if lines > 0 {
			detail += fmt.Sprintf(" · %d line%s", lines, pluralS(lines))
		}
		return prefix + successStyle.Render("✓") + " " + resultStyle.Render(truncate(detail, max(4, w-4)))
	}

	dur, _, failed := toolStatus(b)
	status, statusStyle := "·", mutedStyle
	switch {
	case b.inFlight():
		status, statusStyle = "◷", roToolNameStyle
	case failed:
		status, statusStyle = "✗", errStyle
	case b.hasOk && b.ok:
		status, statusStyle = "✓", successStyle
	}
	nameStyle := toolNameStyleFor(b.name)
	left := prefix + statusStyle.Render(status) + " " + nameStyle.Render(toolDisplayName(b.name))
	if detail := compactToolDetail(b); detail != "" {
		left += dimStyle.Render("  " + detail)
	}
	if dur == "" {
		return truncateStyledRow(left, w)
	}
	return fitRow(w, left, dimStyle.Render(dur))
}

func truncateStyledRow(row string, w int) string {
	if lipgloss.Width(row) <= w {
		return row
	}
	return lipgloss.NewStyle().MaxWidth(max(1, w)).Render(row)
}

func outputLineCount(output string) int {
	output = strings.TrimSpace(output)
	if output == "" {
		return 0
	}
	return strings.Count(output, "\n") + 1
}

// compactToolDetail keeps the most decision-useful part of each call in the
// timeline without leaking raw JSON into the chat. Full args/output/diffs stay
// one Ctrl+O away through the existing bespoke renderers.
func compactToolDetail(b *block) string {
	var parts []string
	add := func(v string) {
		v = strings.TrimSpace(v)
		if v != "" {
			parts = append(parts, v)
		}
	}
	switch b.name {
	case "bash":
		command := b.arg("command")
		if command == "" {
			command = bareToolArg(b.args)
		}
		add(command)
	case "git_commit":
		if message := b.arg("message"); message != "" {
			add(`"` + message + `"`)
		}
	case "git_add":
		if n := len(b.argStrArr("paths")); n > 0 {
			add(fmt.Sprintf("%d file%s", n, pluralS(n)))
		}
	case "grep":
		if pattern := b.arg("pattern"); pattern != "" {
			add(`"` + pattern + `"`)
		}
		if path := b.arg("path"); path != "" {
			add("in " + path)
		}
		if n := outputLineCount(b.output); n > 0 && !b.inFlight() {
			add(fmt.Sprintf("%d match%s", n, pluralS(n)))
		}
	case "glob":
		add(b.arg("pattern"))
		if n := outputLineCount(b.output); n > 0 && !b.inFlight() {
			add(fmt.Sprintf("%d file%s", n, pluralS(n)))
		}
	case "list_dir":
		add(b.arg("path"))
		if n := outputLineCount(b.output); n > 0 && !b.inFlight() {
			add(fmt.Sprintf("%d entries", n))
		}
	case "edit":
		add(b.arg("path"))
		if n := len(b.argObjArr("edits")); n > 0 {
			add(fmt.Sprintf("%d replacement%s", n, pluralS(n)))
		}
	case "write_file":
		add(b.arg("path"))
		if content := b.arg("content"); content != "" {
			n := strings.Count(content, "\n") + 1
			add(fmt.Sprintf("%d line%s", n, pluralS(n)))
		}
	case "todo_write":
		if n := len(b.argObjArr("todos")); n > 0 {
			add(fmt.Sprintf("%d item%s", n, pluralS(n)))
		}
	case "todo_read":
		var todos []map[string]json.RawMessage
		if json.Unmarshal([]byte(strings.TrimSpace(b.output)), &todos) == nil && len(todos) > 0 {
			add(fmt.Sprintf("%d item%s", len(todos), pluralS(len(todos))))
		}
	case "diagnostics":
		add(b.arg("path"))
		errors, warnings := countDiag(splitNonEmpty(strings.TrimSpace(b.output), "\n"))
		if errors > 0 {
			add(fmt.Sprintf("%d error%s", errors, pluralS(errors)))
		}
		if warnings > 0 {
			add(fmt.Sprintf("%d warning%s", warnings, pluralS(warnings)))
		}
	case "memory":
		add(b.arg("action"))
		add(b.arg("name"))
	case "spawn", "subagent":
		add(b.arg("agent"))
		add(b.arg("task"))
	case "bulk":
		if n := len(b.argObjArr("calls")); n > 0 {
			add(fmt.Sprintf("%d nested call%s", n, pluralS(n)))
		}
	case "web_search":
		add(b.arg("query"))
	case "finish":
		if !b.inFlight() {
			add(strings.TrimSpace(b.output))
		}
	default:
		add(toolKeyArg(b))
	}
	return strings.Join(parts, " · ")
}

// bareToolArg recovers legacy/test calls whose args are a command string
// rather than the normal JSON object shape.
func bareToolArg(args string) string {
	args = strings.TrimSpace(args)
	if args == "" {
		return ""
	}
	var value string
	if json.Unmarshal([]byte(args), &value) == nil {
		return value
	}
	var object map[string]json.RawMessage
	if json.Unmarshal([]byte(args), &object) != nil {
		return args
	}
	return ""
}

// scheduleStreamRefresh coalesces token deltas into bounded terminal repaint
// work. The core event pump remains armed, so tool/approval latency is not
// coupled to visual refresh. Terminal output is materially costlier than an
// in-memory UI frame, especially over multiplexers and SSH: cap short sessions
// at 15 FPS and progressively reduce repaint frequency for larger transcripts.
func (s *session) scheduleStreamRefresh() tea.Cmd {
	wait := waitForEvent(s.coreEvents, s.coreStartGen)
	if s.streamRefreshPending {
		return wait
	}
	s.streamRefreshPending = true
	return tea.Batch(wait, tea.Tick(streamRefreshInterval(len(s.blocks)), func(time.Time) tea.Msg {
		return streamRefreshMsg{}
	}))
}

func streamRefreshInterval(blocks int) time.Duration {
	switch {
	case blocks > 300:
		return 150 * time.Millisecond
	case blocks > 100:
		return 100 * time.Millisecond
	default:
		return 66 * time.Millisecond
	}
}

func (s *session) renderCoreFailure() string {
	w, h := s.viewport.Width(), s.viewport.Height()
	msg := strings.TrimSpace(s.coreFailure)
	if msg == "" {
		msg = "The core failed to start."
	}
	rows := []string{
		errStyle.Render("Core unavailable"), "", baseStyle.Render(msg), "",
		accentStyle.Render("r") + baseStyle.Render(" retry   ") + accentStyle.Render("q") + baseStyle.Render(" quit"),
		"", dimStyle.Render("Debug log: " + filepath.Join(configDir(), "debug.jsonl")),
	}
	panelW := min(68, max(20, w-4))
	panel := lipgloss.NewStyle().BorderStyle(lipgloss.RoundedBorder()).
		BorderForeground(lipgloss.Color(c.err)).Padding(0, 1).Width(panelW).
		Render(strings.Join(rows, "\n"))
	return lipgloss.Place(w, h, lipgloss.Center, lipgloss.Center, panel)
}

// refresh re-renders the transcript into the viewport. When follow mode is on
// (default) the view pins to the newest line; when the user has scrolled up,
// follow is off and the current offset is preserved so reading isn't yanked.
func (s *session) refresh() {
	base := s.renderBlocks()
	if base != s.transcriptBase {
		s.transcriptBase = base
		// Mouse hit-testing is opt-in. Do not eagerly build a second, ANSI-free
		// copy of the entire transcript on every stream frame; the first mouse
		// event will populate it through transcriptPlainLines().
		s.transcriptPlain = nil
		// SetContent re-splits the entire transcript (strings.Split + line
		// normalization in bubbles/viewport). Skip it when content is byte-
		// identical — e.g. spinner/stream ticks that left the live block
		// unchanged, or repeated layout()/refresh calls.
		s.viewport.SetContent(base)
	}
	// Freeze auto-follow while a drag selection is in flight: new deltas
	// arriving mid-drag would scroll the content out from under the anchor
	// coordinates, so copy-on-release would capture the wrong text.
	if s.follow && !(s.selection.active && s.selection.dragged) {
		s.viewport.GotoBottom()
	}
}

// discardPartialStreamOutput drops the live assistant/thinking stream blocks
// produced by a failed provider attempt so a mid-stream transport retry can
// re-emit cleanly without duplicating partial text. Finalized history and any
// tool activity already started this turn are left intact.
func (s *session) discardPartialStreamOutput() {
	if s == nil || len(s.blocks) == 0 {
		return
	}
	// Prefer dropping the live stream cursor; if s.cur was already finalized
	// by an intervening push (info toast, etc.), strip a trailing
	// assistant/thinking run at the end of the transcript instead.
	dropFrom := -1
	if s.cur != nil && (s.cur.kind == blkAssistant || s.cur.kind == blkThinking) {
		for i := len(s.blocks) - 1; i >= 0; i-- {
			if s.blocks[i] == s.cur {
				dropFrom = i
				break
			}
		}
		// Also drop any immediately preceding stream blocks of the same turn
		// (thinking then assistant, or consecutive assistant chunks).
		for dropFrom > 0 {
			prev := s.blocks[dropFrom-1]
			if prev != nil && (prev.kind == blkAssistant || prev.kind == blkThinking) {
				dropFrom--
				continue
			}
			break
		}
	} else {
		i := len(s.blocks)
		for i > 0 {
			b := s.blocks[i-1]
			if b != nil && (b.kind == blkAssistant || b.kind == blkThinking) {
				i--
				continue
			}
			break
		}
		if i < len(s.blocks) {
			dropFrom = i
		}
	}
	if dropFrom < 0 || dropFrom >= len(s.blocks) {
		return
	}
	// Fresh slice so GC can reclaim the dropped block strings.
	out := make([]*block, dropFrom)
	copy(out, s.blocks[:dropFrom])
	s.blocks = out
	s.cur = nil
	s.invalidateAll()
	s.refresh()
}

// ---------------------------------------------------------------------------
// log helpers: push a block, then refresh
// ---------------------------------------------------------------------------

func (s *session) logUser(text string) {
	b := s.push(blkUser)
	b.appendText(text)
	s.refresh()
}

// logInfo / logSuccess / logWarn go to the ephemeral toast rail so operational
// chatter does not pollute the transcript or kill the welcome screen.
func (s *session) logInfo(text string) {
	s.setToast(toastInfo, text)
}

func (s *session) logSuccess(text string) {
	s.setToast(toastSuccess, text)
}

func (s *session) logWarn(text string) {
	s.setToast(toastWarn, text)
}

// logError stays in the transcript — real failures should remain reviewable —
// and also flashes a toast so the user notices without scrolling.
func (s *session) logError(text string) {
	b := s.push(blkError)
	b.appendText(text)
	s.refresh()
	s.setToast(toastError, text)
}

// logPersist writes a lasting info/success line into the transcript (stats,
// multi-line reports). Prefer this over logInfo when the user needs to scroll
// back to the content.
func (s *session) logPersist(kind blockKind, text string) {
	b := s.push(kind)
	b.appendText(text)
	s.refresh()
}

func (s *session) logTool(name, args string, sub bool) *block {
	b := s.push(blkTool)
	b.name = name
	b.args = capStored(args)
	b.sub = sub
	b.started = time.Now()
	s.refresh()
	return b
}

// recordAdvisorReview adds or updates one durable reviewer record. Status and
// note events share a stable id, so a live review is one transcript entry.
func (s *session) recordAdvisorReview(scope, advisor, model, state, detail, severity string, elapsed time.Duration) {
	if advisor == "" {
		advisor = "default"
	}
	key := scope + ":" + advisor + ":" + model
	var b *block
	if state != "reviewing" {
		for i := len(s.blocks) - 1; i >= 0; i-- {
			candidate := s.blocks[i]
			if candidate != nil && candidate.kind == blkAdvisor && candidate.id == key {
				b = candidate
				break
			}
		}
	}
	if b == nil {
		b = s.push(blkAdvisor)
		b.id, b.name, b.model = key, advisor, model
	}
	b.advisorState = state
	if detail != "" {
		b.output = capOutput(detail)
	}
	if severity != "" {
		b.args = severity
	}
	if elapsed > 0 {
		b.dur = elapsed
	}
	b.renderStr = ""
	s.invalidateAll()
	s.refresh()
}

func renderAdvisorBlock(b *block, w int) string {
	state := b.advisorState
	if state == "" {
		state = "finding"
	}
	severity := strings.ToLower(strings.TrimSpace(b.args))
	label, dot, tone := "reviewing", "◷", roToolNameStyle
	failure := false
	switch state {
	case "clear":
		label, dot, tone = "clear", "✓", successStyle
	case "finding":
		label = severity
		if label == "" {
			label = "finding"
		}
		if severity == "concern" || severity == "blocker" {
			dot, tone = "!", warnStyle
		} else {
			dot, tone = "·", mutedStyle
		}
	case "failed", "no_key", "invalid_response":
		label, dot, tone, failure = state, "✗", errStyle, true
	case "duplicate":
		label, dot, tone = "duplicate", "·", mutedStyle
	}
	left := tone.Render(dot+" advisor") + " " + toolNameStyleFor("read_file").Render(b.name)
	if b.model != "" {
		left += dimStyle.Render("  " + b.model)
	}
	right := tone.Render(label)
	if b.dur > 0 {
		right = dimStyle.Render(fmt.Sprintf("%.1fs", b.dur.Seconds())) + " " + right
	}
	head := fitRow(w, left, right)
	body := strings.TrimSpace(b.output)
	if body == "" && state == "reviewing" {
		body = "Checking the completed work against the request…"
	} else if body == "" && state == "clear" {
		body = "No actionable finding."
	} else if body == "" && failure {
		body = "Reviewer was unavailable; the executor continued."
	}
	if body == "" {
		return head
	}
	style := mutedStyle
	if failure {
		style = errOutStyle
	} else if severity == "concern" || severity == "blocker" {
		style = warnStyle
	}
	return head + "\n" + style.Render(renderMarkdown(body, max(12, w-2)))
}

func parseHistoricalAdvisory(raw string) (advisor, severity, finding string, ok bool) {
	raw = strings.TrimSpace(raw)
	if !strings.HasPrefix(raw, "<advisory ") {
		return "", "", "", false
	}
	close, end := strings.Index(raw, ">"), strings.LastIndex(raw, "</advisory>")
	if close < 0 || end <= close {
		return "", "", "", false
	}
	attrs := raw[:close]
	attr := func(name string) string {
		prefix := name + `="`
		start := strings.Index(attrs, prefix)
		if start < 0 {
			return ""
		}
		start += len(prefix)
		finish := strings.Index(attrs[start:], `"`)
		if finish < 0 {
			return ""
		}
		return html.UnescapeString(attrs[start : start+finish])
	}
	advisor, severity = attr("advisor"), attr("severity")
	finding = strings.TrimSpace(html.UnescapeString(raw[close+1 : end]))
	return advisor, severity, finding, advisor != "" && finding != ""
}

// captureTodos parses a todo_write args blob and stores the latest todo list in
// s.todos so the pinned panel always shows current state. The agent rewrites
// the full list on every todo_write, so the latest call wins.
func (s *session) captureTodos(args string) {
	todos := argObjArrField(args, "todos")
	if todos == nil {
		return
	}
	// Stash a copy so later block mutations can't alias it.
	cp := make([]map[string]json.RawMessage, len(todos))
	copy(cp, todos)
	s.todos = cp
}

// allTodosComplete reports whether every captured todo is marked completed.
// Used to auto-dismiss the pinned tasks panel once the whole plan is done — a
// finished plan shouldn't linger as a permanent fixture. A later todo_write
// (new work) re-shows it.
func (s *session) allTodosComplete() bool {
	if len(s.todos) == 0 {
		return false
	}
	done, pend, run := countTodoStatuses(s.todos)
	return pend == 0 && run == 0 && done == len(s.todos)
}

// maxStoredOutput bounds text retained on a block (tool output, args, diff).
// A multi-MB write/edit payload is stored capped though only a short preview
// ever renders; this keeps session RSS from pinning megabytes per card.
// 64 KiB × ~3 fields × maxBlocks is a bounded worst case (~75 MiB), down from
// the prior 256 KiB ceiling.
const maxStoredOutput = 64 * 1024 // 64 KiB

// maxStoredText soft-caps assistant/thinking/user streamed text (D-002).
// Higher than tool payloads so long replies stay readable, but still bounded.
const maxStoredText = 256 * 1024 // 256 KiB

const storedTruncMarker = "\n…[truncated]"

// maxCachedToolArgs is the args/diff size kept after a tool/approve card has
// been rendered into the prefix cache (key fields still fit; expand uses output).
const maxCachedToolArgs = 4 * 1024 // 4 KiB

// capStored truncates retained block strings to maxStoredOutput bytes,
// appending a marker when it cut content.
func capStored(s string) string {
	if len(s) <= maxStoredOutput {
		return s
	}
	return s[:maxStoredOutput] + storedTruncMarker
}

// capTo truncates s to n bytes with the same marker used elsewhere.
func capTo(s string, n int) string {
	if n <= 0 || len(s) <= n {
		return s
	}
	return s[:n] + storedTruncMarker
}

// capOutput is the historical name for capping tool-result text.
func capOutput(s string) string { return capStored(s) }

// appendText soft-caps streamed / user text onto the block builder (D-002).
func (b *block) appendText(s string) {
	if b == nil || s == "" {
		return
	}
	if b.text.Len() >= maxStoredText {
		return
	}
	remain := maxStoredText - b.text.Len()
	if len(s) <= remain {
		b.text.WriteString(s)
		return
	}
	// Need room for the truncation marker; if we can't fit it, stop as-is.
	if remain <= len(storedTruncMarker) {
		return
	}
	cut := remain - len(storedTruncMarker)
	b.text.WriteString(s[:cut])
	b.text.WriteString(storedTruncMarker)
}

// releaseAfterCache drops per-block duplicates once the rendered card lives in
// s.cache: clear streaming renderStr, and shrink tool/approve args+diff so the
// session does not keep full payloads alongside the cached card (multi-copy).
func releaseAfterCache(b *block) {
	if b == nil {
		return
	}
	// Preserve the cached render for immutable blocks (assistant/thinking/user)
	// so a later invalidateAll rebuild skips Glamour+lipgloss. Tool/approve cards
	// mutate with results/approval state and never cache, so drop any stale copy.
	if !blockRenderCacheable(b) {
		b.renderStr = ""
		b.renderLen = 0
	}
	switch b.kind {
	case blkTool, blkApprove:
		if len(b.args) > maxCachedToolArgs {
			b.args = capTo(b.args, maxCachedToolArgs)
		}
		if len(b.diff) > maxCachedToolArgs {
			b.diff = capTo(b.diff, maxCachedToolArgs)
		}
	case blkAssistant, blkThinking, blkUser:
		// Keep b.text for copy/find/resize re-wrap; soft-cap already bounds it.
		// Re-pack the Builder so excess capacity from a long stream can be GC'd.
		if b.text.Len() > 0 && b.text.Cap() > b.text.Len()*2 && b.text.Cap()-b.text.Len() > 64*1024 {
			s := b.text.String()
			b.text.Reset()
			b.text.Grow(len(s))
			b.text.WriteString(s)
		}
	}
}

func (s *session) logToolResult(output string) {
	b := s.push(blkToolResult)
	b.output = capOutput(output)
	s.refresh()
}

func (s *session) logApproveDiff(tool, args, diff string) {
	b := s.push(blkApprove)
	b.name, b.args, b.diff = tool, capStored(args), capStored(diff)
	s.refresh()
}

// resolveLatestApproval makes the transcript truthful after the sticky
// approval prompt is dismissed. Call it with the core decision ("yes",
// "always", or "no") or its equivalent UI label before clearing the prompt.
func (s *session) resolveLatestApproval(decision string) {
	for i := len(s.blocks) - 1; i >= 0; i-- {
		b := s.blocks[i]
		if b == nil || b.kind != blkApprove || b.approval != approvalPending {
			continue
		}
		switch decision {
		case "yes", "approved once":
			b.approval = approvalApproved
		case "always", "always allowed":
			b.approval = approvalAlways
		case "no", "denied":
			b.approval = approvalDenied
		default:
			return
		}
		// Decision is what matters in the transcript; drop bulky args/diff.
		b.diff = ""
		if len(b.args) > maxCachedToolArgs {
			b.args = capTo(b.args, maxCachedToolArgs)
		}
		s.invalidateAll()
		s.refresh()
		return
	}
}

// logRaw pushes a pre-styled string verbatim (no further wrapping/styling).
func (s *session) logRaw(styled string) {
	b := s.push(blkRaw)
	b.appendText(styled)
	s.refresh()
}

// ---------------------------------------------------------------------------
// Block rendering — flat, modern, gutter-free
//
// Conversation turns render as a coloured role glyph + bold role label on its
// own line, then full-width markdown content. Transient status lines
// (info/success/warn/error) collapse to a single glyph-prefixed line — no
// boxed card around every message. Tool output gets a left `│` rule panel.
// ---------------------------------------------------------------------------

// renderBlock renders a live streaming block only after enough new content has
// accumulated to justify a complete markdown/layout pass. The stream refresh
// clock bounds visual latency; this byte threshold avoids repeated full renders
// for tiny deltas between clocks.
//
// Finalized blocks additionally cache their full render (Glamour + lipgloss
// output, PRE-decoration) keyed by (width, theme). invalidateAll drops only the
// prefix cache, not these per-block renders, so a rebuild after a toggle, focus
// move, or tool result skips Glamour+lipgloss entirely and re-applies only the
// cheap focus/key-hint decoration (which depends on transient focus state, not
// block content). This keeps a large expanded reasoning block (hundreds of
// lines) from being re-styled on every rebuild. Only immutable post-finalize
// kinds are cached; tool/approve cards mutate with in-flight elapsed time,
// results, and approval state, so they always re-render.
func (s *session) renderBlock(b *block, w int) string {
	if b == s.cur && (b.kind == blkAssistant || b.kind == blkThinking) {
		text := b.text.String()
		force := w != b.renderW || strings.HasSuffix(text, "\n") ||
			b.renderStr == "" || b.renderTheme != activeTheme.name
		if !force && len(text)-b.renderLen < streamBatch {
			return s.decorateFocusedBlock(b, s.renderKeyHints(b.renderStr))
		}
		out := s.renderBlockFull(b, w)
		b.renderW = w
		b.renderStr = out
		b.renderLen = len(text)
		b.renderTheme = activeTheme.name
		return s.decorateFocusedBlock(b, s.renderKeyHints(out))
	}
	if blockRenderCacheable(b) && b.renderStr != "" &&
		b.renderW == w && b.renderTheme == activeTheme.name &&
		b.renderLen == b.text.Len() {
		return s.decorateFocusedBlock(b, s.renderKeyHints(b.renderStr))
	}
	out := s.renderBlockFull(b, w)
	if blockRenderCacheable(b) {
		b.renderW = w
		b.renderStr = out
		b.renderLen = len(b.text.String())
		b.renderTheme = activeTheme.name
	}
	return s.decorateFocusedBlock(b, s.renderKeyHints(out))
}

// blockRenderCacheable reports whether a finalized block's rendered output is a
// pure function of (content, width, theme) and never mutates after finalize —
// so it can be cached across invalidateAll rebuilds. Tool/approve blocks are
// excluded: their cards change with in-flight elapsed time, tool results, and
// approval decisions, so they must re-render each visit.
func blockRenderCacheable(b *block) bool {
	switch b.kind {
	case blkAssistant, blkThinking, blkUser:
		return true
	}
	return false
}

func (s *session) decorateFocusedBlock(b *block, out string) string {
	if s.focusedBlock >= 0 && s.focusedBlock < len(s.blocks) && s.blocks[s.focusedBlock] == b {
		parts := strings.SplitN(out, "\n", 2)
		parts[0] = accentStyle.Render("▸ ") + parts[0]
		return strings.Join(parts, "\n")
	}
	return out
}

func (s *session) renderKeyHints(out string) string {
	if key := s.keyLabel("toggle_tool_output"); key != "" {
		out = strings.ReplaceAll(out, "ctrl+o", key)
	}
	if key := s.keyLabel("toggle_reasoning"); key != "" {
		out = strings.ReplaceAll(out, "ctrl+t", key)
	}
	return out
}

const streamBatch = 128 // bytes of display latency before a live block re-renders

func (s *session) renderBlockFull(b *block, w int) string {
	switch b.kind {
	case blkUser:
		// The user's turn is a right-aligned surface bubble: a rounded card that
		// caps its width so long conversations stay readable and the transcript
		// keeps a clear visual rhythm.
		return s.renderUserBubble(b.text.String(), w)
	case blkAssistant:
		// The assistant is the app's ambient voice: a quiet model tag above
		// flush-left prose with a thin accent rail. No heavy ledger header.
		meta := b.model
		if meta == "" && len(s.models) > 0 && s.modelIdx >= 0 && s.modelIdx < len(s.models) {
			meta = s.models[s.modelIdx].ID
		}
		markdown := renderMarkdown
		if b == s.cur {
			markdown = renderMarkdownUncached
		}
		return s.renderAssistantTurn(meta, b.text.String(), w, markdown)
	case blkThinking:
		if b.collapsed {
			n := strings.Count(b.text.String(), "\n") + 1
			if b.text.Len() == 0 {
				n = 0
			}
			label := fmt.Sprintf("▸ reasoning · %d line%s", n, pluralS(n))
			if key := s.keyLabel("toggle_reasoning"); key != "" {
				label += " · " + key + " expand"
			}
			return softPillStyle(c.secondary, c.surface).Render(label)
		}
		markdown := renderMarkdown
		if b == s.cur {
			markdown = renderMarkdownUncached
		}
		content := thinkStyle.Render(markdown(b.text.String(), w-4))
		return recessedStyle.Width(max(1, w-2)).Render(content)
	case blkTool:
		return s.renderToolBlock(b, w)
	case blkToolResult:
		return renderOutputPanel(strings.TrimSpace(b.output), b.expanded, w, "lines", false)
	case blkAdvisor:
		return renderAdvisorBlock(b, w)
	case blkSuccess:
		return renderStatusLine("✓ ", successStyle, baseStyle, b.text.String(), w)
	case blkWarn:
		return renderStatusLine("! ", warnStyle, baseStyle, b.text.String(), w)
	case blkError:
		// Multi-line errors (core crashes, stack traces) get a red rule panel so
		// real failures stand out from the success/info toasts around them.
		text := strings.TrimSpace(b.text.String())
		if strings.Contains(text, "\n") {
			parts := strings.SplitN(text, "\n", 2)
			head := renderStatusLine("✗ ", errStyle, baseStyle, parts[0], w)
			if len(parts) > 1 {
				head += "\n" + panelLines(parts[1], w, errRuleStyle, errOutStyle)
			}
			return head
		}
		return renderStatusLine("✗ ", errStyle, baseStyle, text, w)
	case blkInfo:
		return renderStatusLine("· ", mutedStyle, mutedStyle, b.text.String(), w)
	case blkApprove:
		// compact history marker; the live decision is the sticky banner. Reuses
		// the per-tool head grammar so the marker names the target, not a JSON blob.
		prefix, style := "? awaiting approval ", warnStyle
		switch b.approval {
		case approvalApproved:
			prefix, style = "✓ approved ", successStyle
		case approvalAlways:
			prefix, style = "✓ approved always ", successStyle
		case approvalDenied:
			prefix, style = "✗ denied ", errStyle
		}
		head := style.Render(prefix) +
			toolNameStyleFor(b.name).Render(toolIcon(b.name)+" "+toolDisplayName(b.name))
		if ka := keyArgFor(b.name, b.args); ka != "" {
			head += toolDetailStyle.Render("  " + truncate(ka, 60))
		}
		if b.approval == approvalPending {
			var controls []string
			if key := s.keyHint("approve"); key != "" {
				controls = append(controls, "["+key+"] approve")
			}
			if key := s.keyHint("deny"); key != "" {
				controls = append(controls, "["+key+"] deny")
			}
			if key := s.keyHint("approve_always"); key != "" {
				controls = append(controls, "["+key+"] always")
			}
			if len(controls) > 0 {
				head += dimStyle.Render("   " + strings.Join(controls, "  "))
			}
		}
		if strings.TrimSpace(b.diff) != "" {
			head += "\n" + renderDiffPanel(b.diff, b.expanded, s.width, s.keyHint("toggle_tool_output"))
		}
		return head
	case blkTrimmed:
		return dimStyle.Italic(true).Render(fmt.Sprintf("⋯ %d older transcript blocks hidden · reopen the session to view full history", b.trimmed))
	case blkRaw:
		return b.text.String()
	}
	return ""
}

// renderStatusLine renders a toast-style status line (info/success/warn/error)
// with a 2-cell prefix, word-wrapping the body to the terminal width so long
// plugin descriptions / error messages aren't clipped mid-sentence.
func renderStatusLine(prefix string, prefixStyle, bodyStyle lipgloss.Style, text string, w int) string {
	const prefixCells = 2 // "✓ ", "· ", "! ", "✗ "
	avail := w - prefixCells
	if avail < 8 {
		avail = 8
	}
	text = strings.TrimRight(text, "\n")
	if text == "" {
		return prefixStyle.Render(prefix)
	}
	lines := strings.Split(wrapPlain(text, avail), "\n")
	var b strings.Builder
	pad := strings.Repeat(" ", prefixCells)
	for i, ln := range lines {
		if i == 0 {
			b.WriteString(prefixStyle.Render(prefix))
		} else {
			b.WriteString(pad)
		}
		b.WriteString(bodyStyle.Render(ln))
		if i < len(lines)-1 {
			b.WriteByte('\n')
		}
	}
	return b.String()
}

// renderToolBlock dispatches to a per-tool renderer so each call gets a layout
// that surfaces its most relevant info (the bash command, the file path, the
// grep pattern…) instead of a generic name(args) head. Sub-agent internal
// calls (spawn:*) collapse to a dim one-liner so a scout's chatter stays tidy.
// The shared header grammar (icon · name · keyarg … dur status) and the
// status badge live in tool_blocks.go; this method just picks the renderer.
func (s *session) renderToolBlock(b *block, w int) string {
	if b.sub {
		return renderSubToolLine(b, w)
	}
	switch b.name {
	case "bash":
		return renderBashBlock(b, w)
	case "read_file":
		return renderReadFileBlock(b, w)
	case "write_file":
		return renderWriteFileBlock(b, w)
	case "edit":
		return renderEditBlock(b, w)
	case "patch":
		return renderPatchBlock(b, w)
	case "list_dir":
		return renderListDirBlock(b, w)
	case "grep":
		return renderGrepBlock(b, w)
	case "glob":
		return renderGlobBlock(b, w)
	case "git_status":
		return renderGitStatusBlock(b, w)
	case "git_log":
		return renderGitLogBlock(b, w)
	case "git_diff":
		return renderGitDiffBlock(b, w)
	case "git_add":
		return renderGitAddBlock(b, w)
	case "git_commit":
		return renderGitCommitBlock(b, w)
	case "todo_write":
		return renderTodoWriteBlock(b, w)
	case "todo_read":
		return renderTodoReadBlock(b, w)
	case "diagnostics":
		return renderDiagnosticsBlock(b, w)
	case "fetch":
		return renderFetchBlock(b, w)
	case "memory":
		return renderMemoryBlock(b, w)
	case "spawn", "subagent":
		return renderSubagentBlock(b, w)
	case "bulk":
		return renderBulkBlock(b, w)
	case "finish":
		return renderFinishBlock(b, w)
	default:
		return renderGenericToolBlock(b, w)
	}
}

// renderOutputPanel wraps tool output, truncating to the first 3 lines
// unless the block is expanded (ctrl+o). A dim hint line shows the toggle.
// `unit` labels the hidden remainder ("lines"/"matches"/"entries"/…) and `err`
// tints the panel red for failed calls. ctrl+o is per-block: users expand one
// call, not all. Upgrade path: a viewport pager if outputs grow huge.
func renderOutputPanel(output string, expanded bool, w int, unit string, err bool) string {
	if unit == "" {
		unit = "lines"
	}
	const headLines = 3
	lines := strings.Split(output, "\n")
	if len(lines) > headLines && !expanded {
		more := len(lines) - headLines
		shown := strings.Join(lines[:headLines], "\n")
		hint := dimStyle.Italic(true).Render(
			fmt.Sprintf("│ … +%d %s  (ctrl+o expand)", more, unit))
		return resultPanel(shown, w, err) + "\n" + hint
	}
	panel := resultPanel(output, w, err)
	if len(lines) > headLines && expanded {
		panel += "\n" + dimStyle.Italic(true).Render("│ (ctrl+o collapse)")
	}
	return panel
}

// resultPanel renders tool/command output with a left `│` rule, wrapped to fit.
// The rule + content are tinted red when err is set so failures are scannable.
func resultPanel(output string, w int, err bool) string {
	if err {
		return panelLines(output, w, errRuleStyle, errOutStyle)
	}
	return panelLines(output, w, dimStyle, resultStyle)
}

// lastToolOutputBlock returns the most recent inspectable tool call/result.
// The historical name is retained for callers, but calls without textual
// output are included because their args/diff still have useful details.
func (s *session) lastToolOutputBlock() *block {
	for i := len(s.blocks) - 1; i >= 0; i-- {
		b := s.blocks[i]
		if b == nil {
			continue
		}
		if b.kind == blkToolResult {
			return b
		}
		if b.kind == blkTool && !b.sub && b.dur > 0 {
			return b
		}
	}
	return nil
}

// nearestToolOutputBlock picks the tool output nearest the visible viewport.
// renderBlocks records exact line ranges, avoiding the old scroll-percentage
// approximation (which selected the wrong card whenever block heights varied).
func (s *session) nearestToolOutputBlock() *block {
	var candidates []*block
	for _, b := range s.blocks {
		if b == nil {
			continue
		}
		if b.kind == blkToolResult {
			candidates = append(candidates, b)
			continue
		}
		if b.kind == blkApprove && strings.TrimSpace(b.diff) != "" {
			candidates = append(candidates, b)
			continue
		}
		if b.kind == blkTool && !b.sub && b.dur > 0 {
			candidates = append(candidates, b)
		}
	}
	if len(candidates) == 0 {
		return nil
	}
	if s.follow || s.viewport.AtBottom() {
		return candidates[len(candidates)-1]
	}
	top := s.viewport.YOffset()
	bottom := top + max(1, s.viewport.VisibleLineCount()) - 1
	center := (top + bottom) / 2
	best, bestDistance := candidates[0], int(^uint(0)>>1)
	for _, candidate := range candidates {
		distance := 0
		switch {
		case candidate.renderEnd < top:
			distance = top - candidate.renderEnd
		case candidate.renderStart > bottom:
			distance = candidate.renderStart - bottom
		default:
			blockCenter := (candidate.renderStart + candidate.renderEnd) / 2
			distance = blockCenter - center
			if distance < 0 {
				distance = -distance
			}
		}
		if distance < bestDistance {
			best, bestDistance = candidate, distance
		}
	}
	return best
}

// findSubProgress returns the live progress entry for runID, or nil.
func (s *session) findSubProgress(runID string) *subProgressEntry {
	for _, e := range s.subProgress {
		if e.runID == runID {
			return e
		}
	}
	return nil
}

// removeSubProgress drops the progress entry for runID (subagent finished).
func (s *session) removeSubProgress(runID string) {
	for i, e := range s.subProgress {
		if e.runID == runID {
			s.subProgress = append(s.subProgress[:i], s.subProgress[i+1:]...)
			return
		}
	}
}

// ---- active-task (scout) helpers ----

// isInFlight reports a top-level tool block still awaiting its result.
func isInFlight(b *block) bool {
	return b != nil && b.kind == blkTool && !b.sub && b.dur == 0
}

// hasLiveContent reports whether the transcript has content that changes over
// time (a streaming block or an in-flight tool). Used to gate the per-tick
// refresh so an idle session doesn't re-render the viewport every second.
func (s *session) hasLiveContent() bool {
	if s.cur != nil {
		return true
	}
	for _, b := range s.blocks {
		if b != nil && b.kind == blkTool && b.inFlight() {
			return true
		}
	}
	return false
}

// blockingInputOpen reports whether a keyboard-intercepting overlay is
// currently open: a HITL flyout that blocks the agent on user input (ask /
// sudo / approval) OR any modal (command palette, settings edit, OAuth code
// box, value-edit box, …). While one is open the main input doesn't own the
// keyboard, so the transcript re-render churn is paused — the busy-frame
// storm (10×/s View) and the tickMsg / streamRefreshMsg renderBlocks rebuilds
// (see those handlers in main.go). Without this the churn saturates
// bubbletea's single-threaded loop and makes typing in the overlay lag by
// seconds per keystroke on long transcripts (the same root cause as the
// ask-flyout lag, just on the modal path — modals were left out of the
// original guard).
//
// For ask/sudo/approval the agent is paused so the transcript truly can't
// change. For a modal the agent may keep streaming, but the modal overlays
// the transcript, so a frozen view is harmless and catches up within ~1s of
// the modal closing (tickMsg re-arms the busy-frame and refreshes; a
// streaming turn also refreshes on the next streamRefreshMsg).
func (s *session) blockingInputOpen() bool {
	return s.pendingAsk != nil || s.pendingSudo != nil || s.pendingApproval != nil || s.modal.kind != modalNone
}

// finalizeInFlight marks any still-running tool blocks as done so the
// active-tasks panel doesn't linger after an abort or turn end.
func (s *session) finalizeInFlight(note string) {
	changed := false
	for _, b := range s.blocks {
		if isInFlight(b) || (b != nil && b.kind == blkTool && b.sub && b.dur == 0) {
			b.dur = time.Since(b.started)
			if strings.TrimSpace(b.output) == "" {
				if b.name == "finish" {
					b.output = "This turn has finished"
					b.hasOk = true
					b.ok = true
				} else {
					b.output = note
				}
			}
			changed = true
		}
	}
	if changed {
		s.invalidateAll()
	}
}

// taskLabel extracts a human-readable description for an in-flight tool: the
// scout's prompt for a spawn, else the tool name + args.
func taskLabel(b *block, w int) string {
	if b.name == "spawn" {
		if p := jsonStringField(b.args, "prompt"); p != "" {
			return truncate(p, w-6)
		}
	}
	return b.name + "(" + truncate(b.args, w-len(b.name)-8) + ")"
}

// taskRole labels the agent kind for the active-tasks panel: a spawn is a scout.
func taskRole(b *block) string {
	if b.name == "spawn" {
		return "scout"
	}
	return b.name
}

// taskModel returns the model a scout runs on (parsed from spawn args), if any.
func taskModel(b *block) string {
	if b.name == "spawn" {
		return jsonStringField(b.args, "model")
	}
	return ""
}

// formatDur renders a duration as M:SS.
func formatDur(d time.Duration) string {
	m := int(d.Minutes())
	s := int(d.Seconds()) % 60
	return fmt.Sprintf("%d:%02d", m, s)
}

// jsonStringField reads a string field from a JSON object string.
func jsonStringField(s, key string) string {
	var m map[string]json.RawMessage
	if json.Unmarshal([]byte(s), &m) != nil {
		return ""
	}
	return get(m, key)
}

// roleLine renders a coloured glyph + bold role label + dim metadata.
//
//	● you   · 14:32
func roleLine(glyph, role, meta, color string) string {
	// Glowing role marker: a solid dot in the role colour, then a strong label —
	// the Catalyst status-dot idiom applied to conversation turns.
	out := lipgloss.NewStyle().Foreground(lipgloss.Color(color)).Render(glyph)
	out += " " + lipgloss.NewStyle().Foreground(lipgloss.Color(color)).Bold(true).Render(role)
	if meta != "" {
		out += mutedStyle.Render("  " + truncate(meta, 40))
	}
	return out
}

// turnRail wraps a conversation turn's body in the Catalyst hairline rail: a
// single left │ in the given colour (peach for the user, quiet for the
// assistant) with a small left inset, so turns are visually grouped and
// scannable. The rail colour — not a box — carries the structure, matching the
// web's hairline dividers. Empty lines keep the rail so the group reads whole.
func turnRail(body string, rail lipgloss.Style) string {
	bar := rail.Render("│")
	lines := strings.Split(body, "\n")
	for i, l := range lines {
		lines[i] = bar + " " + l
	}
	return strings.Join(lines, "\n")
}

// heavyRail is the user-turn treatment: a thick accent bar (blockquote-style,
// reads as "you typed this") that stays visually distinct from the thin `│`
// tool rails. bodyIndent is a quiet left inset for the assistant body.
func heavyRail(body string, rail lipgloss.Style) string {
	bar := rail.Render("▌")
	lines := strings.Split(body, "\n")
	for i, l := range lines {
		lines[i] = bar + " " + l
	}
	return strings.Join(lines, "\n")
}

func bodyIndent(body string) string {
	lines := strings.Split(body, "\n")
	for i, l := range lines {
		lines[i] = "  " + l
	}
	return strings.Join(lines, "\n")
}

// Turns use compact labeled rails. The small header carries role identity while
// preserving stable transcript rows for navigation and text selection.
func (s *session) renderUserBubble(text string, w int) string {
	return turnHeader("YOU", c.user, w) + "\n" + heavyRail(renderMarkdown(text, max(1, w-2)), userRailStyle)
}

func (s *session) renderAssistantTurn(meta, text string, w int, markdown func(string, int) string) string {
	label := "CATALYST"
	if meta != "" {
		label += "  " + truncate(meta, max(8, w/2))
	}
	return turnHeader(label, c.accent, w) + "\n" + bodyIndent(markdown(text, max(1, w-2)))
}

// turnHeader renders a ledger section header: the role tag on the left, then a
// full-width hairline rule filling the rest of the row. Every user turn anchors
// one, so the transcript reads as a ruled ledger with clear section breaks —
// the Catalyst hairline divider idiom applied to conversation structure.
func turnHeader(tag, color string, w int) string {
	label := lipgloss.NewStyle().Foreground(lipgloss.Color(color)).Render("●") +
		" " + lipgloss.NewStyle().Foreground(lipgloss.Color(color)).Bold(true).Render(tag)
	used := lipgloss.Width(label) + 1
	if w-used < 1 {
		return label
	}
	return label + " " + railStyle.Render(strings.Repeat("─", w-used))
}

// ---------------------------------------------------------------------------
// Welcome screen — shown when the conversation is empty.
//
// A centred brand + tagline + a selectable list of starter prompts. Arrow keys
// (or number keys) pick one; enter fills the input so it can be edited before
// sending.
// ---------------------------------------------------------------------------

var welcomeExamples = []string{
	"Understand this repository",
	"Find and fix a bug",
	"Add or improve tests",
	"Review recent changes",
}

// surfacePanel is the shared quiet focus surface for empty and blocking states.
// It uses theme-derived depth and hairline rails, never a new authored color.
func surfacePanel(width int) lipgloss.Style {
	return lipgloss.NewStyle().
		BorderStyle(lipgloss.NormalBorder()).
		BorderForeground(lipgloss.Color(c.railDim)).
		Background(lipgloss.Color(c.surface)).
		Padding(1, 2).
		Width(width)
}

func (s *session) renderWelcome() string {
	w := s.viewport.Width()
	h := s.viewport.Height()
	if s.showingSplash() {
		return s.renderSplashScreen(w, h)
	}
	if !s.canSend() {
		panelW := min(58, max(1, w-4))
		rows := []string{
			accentStyle.Render("◆  CONNECT CATALYST"),
			dimStyle.Render("Authentication required before the workspace can run."),
			"",
			baseStyle.Render("No API key yet — log in to start chatting."),
			keyHintStyle.Render("Enter opens /login · / commands · ? help"),
		}
		panel := surfacePanel(panelW).Render(strings.Join(rows, "\n"))
		return lipgloss.Place(w, h, lipgloss.Center, lipgloss.Center, panel)
	}
	idx := min(max(0, s.welcomeIdx), len(welcomeExamples)-1)
	if h < 10 || w < 32 {
		lines := []string{
			boldBaseStyle.Render("START A SESSION"),
			accentStyle.Render(fmt.Sprintf("▸ %d  %s", idx+1, welcomeExamples[idx])),
			keyHintStyle.Render("↑↓ choose  ·  enter use  ·  / commands"),
		}
		return lipgloss.Place(w, h, lipgloss.Left, lipgloss.Center, strings.Join(lines, "\n"))
	}
	panelW := min(58, max(1, w-4))
	rows := []string{
		accentStyle.Render("◆  CATALYST CODE"),
		boldBaseStyle.Render("What would you like to build?"),
		dimStyle.Render("Choose a starting point or type below."),
		"",
	}
	for i, ex := range welcomeExamples {
		marker := "  "
		text := baseStyle.Render(ex)
		if i == idx {
			marker = accentStyle.Render("▸ ")
			text = accentStyle.Render(ex)
		}
		rows = append(rows, marker+dimStyle.Render(fmt.Sprintf("%d", i+1))+"  "+text)
	}
	rows = append(rows, "", keyHintStyle.Render("↑↓ select   Enter use   / commands"))
	panel := surfacePanel(panelW).Render(strings.Join(rows, "\n"))
	return lipgloss.Place(w, h, lipgloss.Center, lipgloss.Center, panel)
}

// rebuildBlocksFromHistory reconstructs the transcript blocks from a loaded
// session's message list (role / content / reasoning_content / tool_calls /
// tool results) so a resumed or switched session shows its prior turns instead
// of an empty view. Timing for historical tool calls is unknown, so they render
// as finalized cards without a duration line (gated on started.IsZero()).
func (s *session) rebuildBlocksFromHistory(msgs []map[string]json.RawMessage) {
	s.blocks = nil
	s.cur = nil
	s.cacheIdx = 0
	s.cacheLines = 0
	pending := map[string]*block{} // tool_call_id -> block awaiting its result
	for i := range msgs {
		msg := msgs[i]
		msgs[i] = nil // release each message as we go so JSON+blocks don't peak 2× (D-003)
		if msg == nil {
			continue
		}
		switch get(msg, "role") {
		case "user":
			text := contentText(msg["content"])
			msg["content"] = nil
			if strings.TrimSpace(text) == "" {
				continue
			}
			b := s.push(blkUser)
			b.appendText(text)
		case "assistant":
			content := contentText(msg["content"])
			reasoning := contentText(msg["reasoning_content"])
			// MiniMax proxy sessions embed thinking in content as <think> tags.
			if strings.TrimSpace(reasoning) == "" {
				if thought, visible, ok := peelThinkTags(content); ok {
					reasoning = thought
					content = visible
				}
			}
			if strings.TrimSpace(reasoning) != "" {
				b := s.push(blkThinking)
				b.appendText(reasoning)
			}
			msg["reasoning_content"] = nil
			if strings.TrimSpace(content) != "" {
				b := s.push(blkAssistant)
				b.appendText(content)
				if s.modelIdx >= 0 && s.modelIdx < len(s.models) {
					b.model = s.models[s.modelIdx].ID
				}
			}
			msg["content"] = nil
			if raw, ok := msg["tool_calls"]; ok {
				var calls []map[string]json.RawMessage
				if json.Unmarshal(raw, &calls) == nil {
					for _, tc := range calls {
						name, args, id := decodeToolCall(tc)
						if name == "" {
							continue
						}
						sub := strings.HasPrefix(name, "spawn:")
						disp := name
						if sub {
							disp = strings.TrimPrefix(name, "spawn:")
						}
						b := s.push(blkTool)
						b.name = disp
						b.args = capStored(args)
						b.sub = sub
						b.id = id
						b.started = time.Time{} // historical: no timing
						b.dur = 1               // >0 => finalized, not in-flight
						if disp == "todo_write" {
							s.captureTodos(args) // last todo_write wins
						}
						if id != "" {
							pending[id] = b
						}
					}
				}
				msg["tool_calls"] = nil
			}
		case "system":
			if advisor, severity, finding, ok := parseHistoricalAdvisory(contentText(msg["content"])); ok {
				b := s.push(blkAdvisor)
				b.name = advisor
				b.advisorState = "finding"
				b.output = finding
				b.args = severity
				b.dur = 1 // historical entry: finalized
			}
			msg["content"] = nil
		case "tool":
			out := contentText(msg["content"])
			msg["content"] = nil
			id := get(msg, "tool_call_id")
			if b, ok := pending[id]; ok && id != "" {
				b.output = capOutput(out)
				delete(pending, id)
			} else {
				b := s.push(blkToolResult)
				b.output = capOutput(out)
			}
		}
	}
	s.cur = nil
}

// peelThinkTags extracts a leading <think>…</think> block from MiniMax-style
// assistant content. ok is false when no complete tag pair is present.
func peelThinkTags(content string) (thought, visible string, ok bool) {
	trimmed := strings.TrimLeft(content, " \t\r\n")
	const open = "<think>"
	const close = "</think>"
	if !strings.HasPrefix(trimmed, open) {
		return "", content, false
	}
	rest := trimmed[len(open):]
	idx := strings.Index(rest, close)
	if idx < 0 {
		return "", content, false
	}
	thought = strings.TrimSpace(rest[:idx])
	visible = rest[idx+len(close):]
	// MiniMax proxy streams sometimes append bare extra closing tags.
	for {
		v := strings.TrimLeft(visible, " \t\r\n")
		if !strings.HasPrefix(v, close) {
			visible = v
			break
		}
		visible = v[len(close):]
	}
	visible = strings.TrimLeft(visible, " \t\r\n")
	return thought, visible, true
}

// contentText extracts displayable text from a message content field, which may
// be a plain string, null, or a multimodal array of text/image parts.
func contentText(raw json.RawMessage) string {
	if len(raw) == 0 || string(raw) == "null" {
		return ""
	}
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var parts []map[string]json.RawMessage
	if json.Unmarshal(raw, &parts) == nil {
		var b strings.Builder
		imgs := 0
		for _, p := range parts {
			switch get(p, "type") {
			case "text":
				if b.Len() > 0 {
					b.WriteByte('\n')
				}
				b.WriteString(get(p, "text"))
			case "image_url":
				imgs++
			}
		}
		if imgs > 0 {
			if b.Len() > 0 {
				b.WriteByte('\n')
			}
			fmt.Fprintf(&b, "[image \u00d7%d]", imgs)
		}
		return b.String()
	}
	return ""
}

// decodeToolCall pulls (name, arguments, id) from a tool_calls entry.
func decodeToolCall(tc map[string]json.RawMessage) (name, args, id string) {
	id = get(tc, "id")
	var fn map[string]json.RawMessage
	if raw, ok := tc["function"]; ok {
		if json.Unmarshal(raw, &fn) == nil {
			name = get(fn, "name")
			args = get(fn, "arguments")
		}
	}
	return name, args, id
}
