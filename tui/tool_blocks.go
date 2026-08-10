package main

// Per-tool rendering for blkTool. renderToolBlock (blocks.go) dispatches here by
// tool name; each renderer builds the shared header (icon · name · keyarg … dur
// status) via renderToolHead and a tool-specific body. The goal is information
// design: surface the one thing that matters for each call (the bash command,
// the file path, the grep pattern, the todo list) instead of a generic
// name(args) blob. Bodies stay within the flat aesthetic — a single left `│`
// rule, no boxed cards — reusing renderOutputPanel/renderDiffPanel/renderRowsPanel.
//
// Status badge (✓/✗/◷) comes from the tool_result.ok field captured in
// handlers.go; failures also tint the body's rule red so they're scannable
// while scrolling.

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"charm.land/lipgloss/v2"
)

// ---- block helpers ----------------------------------------------------------

// inFlight reports whether a tool call is still awaiting its result.
// Historical blocks (rebuilt from a session file) have a zero started time and
// dur==1, so they are NOT in-flight.
func (b *block) inFlight() bool { return b.dur == 0 && !b.started.IsZero() }

// argField reads a string field from a raw JSON args string. Returns "" if
// args isn't a JSON object or the field is absent. (args may be a bare string
// in tests/edge cases — we degrade gracefully.)
func argField(args, key string) string {
	var m map[string]json.RawMessage
	if json.Unmarshal([]byte(args), &m) != nil {
		return ""
	}
	return get(m, key)
}

// arg reads a string field from the block's args JSON.
func (b *block) arg(key string) string { return argField(b.args, key) }

// argObjArrField reads an array of objects from a raw JSON args string.
func argObjArrField(args, key string) []map[string]json.RawMessage {
	var m map[string]json.RawMessage
	if json.Unmarshal([]byte(args), &m) != nil {
		return nil
	}
	raw, ok := m[key]
	if !ok {
		return nil
	}
	var arr []map[string]json.RawMessage
	if json.Unmarshal(raw, &arr) != nil {
		return nil
	}
	return arr
}

// argObjArr reads an array of objects from the block's args JSON (todos, calls).
func (b *block) argObjArr(key string) []map[string]json.RawMessage {
	return argObjArrField(b.args, key)
}

// argStrArr reads a string array from the block's args JSON (e.g. git_add paths).
func (b *block) argStrArr(key string) []string {
	var m map[string]json.RawMessage
	if json.Unmarshal([]byte(b.args), &m) != nil {
		return nil
	}
	raw, ok := m[key]
	if !ok {
		return nil
	}
	var arr []string
	if json.Unmarshal(raw, &arr) != nil {
		return nil
	}
	return arr
}

// keyArgFor returns the single most relevant arg for a tool (the path, command,
// pattern, url, …), used by the collapsed sub-agent one-liner and the generic
// fallback head — so each surface shows the actual target instead of a raw
// JSON blob.
//
// Bash commands are summarized to one line for activity rows / sub-agent
// chatter. Approval banners intentionally do NOT go through this path — see
// approvalSummary, which keeps the full (flattened) command for consent.
// Expanded bash blocks still render the full script via renderBashBlock.
func keyArgFor(name, args string) string {
	switch name {
	case "bash":
		return summarizeBashCommand(argField(args, "command"))
	case "git_commit":
		return argField(args, "command")
	case "read_file", "list_dir", "edit", "write_file", "patch", "diagnostics", "git_diff", "git_log", "git_show", "git_status":
		return argField(args, "path")
	case "git_push", "git_pull":
		return argField(args, "remote")
	case "git_branch":
		if n := argField(args, "name"); n != "" {
			return n
		}
		return argField(args, "action")
	case "grep", "glob":
		return argField(args, "pattern")
	case "fetch":
		return argField(args, "url")
	case "memory":
		return argField(args, "name")
	}
	return ""
}

// summarizeBashCommand collapses a shell command to a single scannable preview
// for activity rows and sub-agent one-liners (NOT the approval banner).
//
// Rules:
//   - single-line commands: whitespace collapsed, returned as-is
//   - multi-line scripts: first meaningful line (skip blanks/comments); if that
//     line is only `cd <dir>`, pair it with the next meaningful line so heredoc
//     runners still read as "cd … · python3 <<'PY' · N lines"
//   - always one physical line so compact activity cards stay one row tall
func summarizeBashCommand(cmd string) string {
	cmd = strings.TrimSpace(cmd)
	if cmd == "" {
		return ""
	}
	rawLines := strings.Split(cmd, "\n")
	totalLines := len(rawLines)

	var meaningful []string
	for _, line := range rawLines {
		t := strings.TrimSpace(line)
		if t == "" || strings.HasPrefix(t, "#") {
			continue
		}
		meaningful = append(meaningful, collapseWS(t))
	}
	if len(meaningful) == 0 {
		return collapseWS(cmd)
	}

	lead := meaningful[0]
	if totalLines == 1 {
		return lead
	}
	// A leading bare `cd dir` is setup, not the work — pair with the next line.
	if isCdOnly(lead) && len(meaningful) > 1 {
		lead = lead + " · " + meaningful[1]
	}
	return fmt.Sprintf("%s · %d lines", lead, totalLines)
}

// collapseWS flattens internal whitespace so a command preview stays one line.
func collapseWS(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

// isCdOnly reports whether line is a bare directory change (no &&/;/| chain).
func isCdOnly(line string) bool {
	fields := strings.Fields(line)
	if len(fields) == 0 || fields[0] != "cd" {
		return false
	}
	if strings.ContainsAny(line, "|&;") || strings.Contains(line, "&&") {
		return false
	}
	// `cd` alone or `cd <path>` (optionally with flags like -P/-L).
	return true
}

// toolKeyArg returns keyArgFor for a block.
func toolKeyArg(b *block) string { return keyArgFor(b.name, b.args) }

// approvalSummary builds the plain detail string for an approval prompt
// (keyarg + an optional count), so the sticky banner reads
// "approve ✎ edit src/main.rs · 3 replacements" instead of a JSON blob.
//
// Bash is special: consent must surface the real script body, not the
// aggressive activity one-liner (which can hide a sensitive later line behind
// a benign leading `cd`). We flatten newlines so the banner stays one row,
// then let renderApprovalBanner's truncate clip to available width — the
// start of the flattened script still includes later payload lines.
func approvalSummary(name, args string) string {
	var segs []string
	switch name {
	case "bash":
		if cmd := argField(args, "command"); cmd != "" {
			segs = append(segs, collapseWS(cmd))
		}
	case "edit":
		if ka := keyArgFor(name, args); ka != "" {
			segs = append(segs, ka)
		}
		if n := len(argObjArrField(args, "edits")); n > 0 {
			segs = append(segs, fmt.Sprintf("· %d replacement%s", n, pluralS(n)))
		}
	case "write_file":
		if ka := keyArgFor(name, args); ka != "" {
			segs = append(segs, ka)
		}
		if content := argField(args, "content"); content != "" {
			nl := strings.Count(content, "\n") + 1
			segs = append(segs, fmt.Sprintf("· %d line%s", nl, pluralS(nl)))
		}
	default:
		if ka := keyArgFor(name, args); ka != "" {
			segs = append(segs, ka)
		}
	}
	return strings.Join(segs, "  ")
}

// ---- shared header + status -------------------------------------------------

// toolStatus returns the duration string, a status badge, and whether the call
// failed, for the header's right side. In-flight → accent spinner + live
// elapsed; finalized-with-ok → ✓/✗; historical (no ok) → duration only.
func toolStatus(b *block) (dur, badge string, failed bool) {
	switch {
	case b.inFlight():
		dur = fmt.Sprintf("%.1fs", time.Since(b.started).Seconds())
		badge = roToolNameStyle.Render("◷")
	case b.hasOk:
		dur = fmt.Sprintf("%.1fs", b.dur.Seconds())
		if b.ok {
			badge = successStyle.Render("✓")
		} else {
			badge = errStyle.Render("✗")
			failed = true
		}
	default:
		if b.dur > 0 && !b.started.IsZero() {
			dur = fmt.Sprintf("%.1fs", b.dur.Seconds())
		}
	}
	return
}

// renderToolHead builds the consistent header row:
//
//	ICON  name  keyarg  ·  subinfo            dur  STATUS
//
// name (and icon) are tinted by kind (read-only cyan / destructive amber); the
// right side (duration + badge) is right-flushed via fitRow. leftExtra carries
// the pre-styled keyarg + subinfo. Returns the head and whether the call failed
// (so the body can tint itself red).
func renderToolHead(b *block, w int, leftExtra string) (string, bool) {
	dur, badge, failed := toolStatus(b)
	nameStyle := toolNameStyleFor(b.name)
	left := nameStyle.Render(toolIcon(b.name) + " " + toolDisplayName(b.name))
	if leftExtra != "" {
		left += "  " + leftExtra
	}
	right := strings.TrimSpace(dur + " " + badge)
	if right == "" {
		return left, failed
	}
	return fitRow(w, left, right), failed
}

// joinSegs joins pre-styled header segments (e.g. path, "· 3 replacements")
// with two spaces, skipping empty ones. Empty segments must be the literal ""
// (callers should not pass styled-empty strings).
func joinSegs(segs ...string) string {
	var out string
	for _, s := range segs {
		if s == "" {
			continue
		}
		if out != "" {
			out += "  "
		}
		out += s
	}
	return out
}

// whatLine renders an indented "what" line under the header: a styled sigil
// (e.g. "$ ", "◆ ") followed by text wrapped to fit. Continuation lines align
// under the text (sigil is assumed 2 cells wide).
func whatLine(sigilStyled, text string, contentStyle lipgloss.Style, w int) string {
	const indent = "  "
	avail := w - len(indent) - 2
	if avail < 2 {
		avail = 2
	}
	lines := strings.Split(wrapPlain(text, avail), "\n")
	var b strings.Builder
	for i, l := range lines {
		b.WriteString(indent)
		if i == 0 {
			b.WriteString(sigilStyled)
		} else {
			b.WriteString("  ")
		}
		b.WriteString(contentStyle.Render(l))
		b.WriteByte('\n')
	}
	return strings.TrimRight(b.String(), "\n")
}

// renderToolBody is the shared body chooser for tools without a bespoke layout:
// diff panel when the core attached a diff, else the output panel (tinted red on
// failure), else "(no output)".
func renderToolBody(b *block, w int, unit string, failed bool) string {
	switch {
	case strings.TrimSpace(b.diff) != "":
		return renderDiffPanel(b.diff, b.expanded, w)
	case strings.TrimSpace(b.output) != "":
		return renderOutputPanel(strings.TrimSpace(b.output), b.expanded, w, unit, failed)
	default:
		if failed {
			return errRuleStyle.Render("│ ") + errOutStyle.Italic(true).Render("(no output)")
		}
		return dimStyle.Italic(true).Render("│ (no output)")
	}
}

// ---- shared structured-body panel -------------------------------------------

// renderRowsPanel renders a list of pre-styled, pre-truncated rows under a left
// `│` rule, truncating to the first 3 rows unless expanded (ctrl+o). `unit`
// labels the hidden remainder ("entries"/"matches"/"commits"/…); `err` tints
// the rule red. Callers must truncate each row's plain text to w-3 before
// styling (rows aren't wrapped here — wrapped styled text would break ANSI).
func renderRowsPanel(rows []string, expanded bool, w int, unit string, err bool) string {
	if len(rows) == 0 {
		if err {
			return errRuleStyle.Render("│ ") + errOutStyle.Italic(true).Render("(no output)")
		}
		return dimStyle.Italic(true).Render("│ (no output)")
	}
	const headLines = 3
	rule := dimStyle
	if err {
		rule = errRuleStyle
	}
	if len(rows) > headLines && !expanded {
		more := len(rows) - headLines
		panel := prefixRows(rows[:headLines], rule)
		hint := dimStyle.Italic(true).Render(
			fmt.Sprintf("│ … +%d %s  (ctrl+o expand)", more, unit))
		return panel + "\n" + hint
	}
	panel := prefixRows(rows, rule)
	if len(rows) > headLines && expanded {
		panel += "\n" + dimStyle.Italic(true).Render("│ (ctrl+o collapse)")
	}
	return panel
}

func prefixRows(rows []string, rule lipgloss.Style) string {
	r := rule.Render("│ ")
	var b strings.Builder
	for _, row := range rows {
		b.WriteString(r)
		b.WriteString(row)
		b.WriteByte('\n')
	}
	return strings.TrimRight(b.String(), "\n")
}

// ---- small text helpers -----------------------------------------------------

func splitNonEmpty(s, sep string) []string {
	var out []string
	for _, l := range strings.Split(s, sep) {
		l = strings.TrimSpace(l)
		if l != "" {
			out = append(out, l)
		}
	}
	return out
}

func pluralS(n int) string {
	if n == 1 {
		return ""
	}
	return "s"
}

func atoiSafe(s string) int {
	n := 0
	for _, r := range s {
		if r < '0' || r > '9' {
			return 0
		}
		n = n*10 + int(r-'0')
	}
	return n
}

func panelCW(w int) int {
	cw := w - 3
	if cw < 2 {
		cw = 2
	}
	return cw
}

// ---- renderers --------------------------------------------------------------

// renderSubToolLine collapses a sub-agent's internal call to a dim one-liner
// (no body), keeping a scout's chatter tidy. The key arg is extracted so the
// one line still says what the call touched.
func renderSubToolLine(b *block, w int) string {
	head := dimStyle.Render("  ┊ " + b.name)
	if ka := toolKeyArg(b); ka != "" {
		head += dimStyle.Render("  " + truncate(ka, w-12))
	} else if b.args != "" {
		head += dimStyle.Render("(" + truncate(b.args, w-12) + ")")
	}
	if b.dur > 0 && !b.started.IsZero() {
		head += dimStyle.Render(fmt.Sprintf(" · %.1fs", b.dur.Seconds()))
	}
	return head
}

func renderBashBlock(b *block, w int) string {
	head, failed := renderToolHead(b, w, "")
	var out strings.Builder
	out.WriteString(head)
	cmd := b.arg("command")
	if cmd == "" && b.args != "" {
		// args isn't a JSON object (e.g. a bare command string): show it raw.
		var probe map[string]json.RawMessage
		if json.Unmarshal([]byte(b.args), &probe) != nil {
			cmd = b.args
		}
	}
	if cmd != "" {
		out.WriteString("\n" + whatLine(roToolNameStyle.Render("$ "), cmd, baseStyle, w))
	}
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", failed))
	}
	return out.String()
}

func renderReadFileBlock(b *block, w int) string {
	path := b.arg("path")
	offset := atoiSafe(b.arg("offset"))
	limit := atoiSafe(b.arg("limit"))
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	if offset > 0 || limit > 0 {
		lo := offset
		if lo == 0 {
			lo = 1
		}
		if limit > 0 {
			segs = append(segs, dimStyle.Render(fmt.Sprintf("· L%d–%d", lo, lo+limit-1)))
		} else {
			segs = append(segs, dimStyle.Render(fmt.Sprintf("· L%d–", lo)))
		}
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		startLine := offset
		if startLine == 0 {
			startLine = 1
		}
		out.WriteString("\n" + renderNumberedOutput(b.output, startLine, b.expanded, w, false))
	}
	return out.String()
}

func renderWriteFileBlock(b *block, w int) string {
	path := b.arg("path")
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	if content := b.arg("content"); content != "" {
		nl := strings.Count(content, "\n") + 1
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d line%s", nl, pluralS(nl))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", !b.ok && b.hasOk))
	}
	return out.String()
}

func renderEditBlock(b *block, w int) string {
	path := b.arg("path")
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	if edits := b.argObjArr("edits"); len(edits) > 0 {
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d replacement%s", len(edits), pluralS(len(edits)))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", !b.ok && b.hasOk))
	}
	return out.String()
}

func renderPatchBlock(b *block, w int) string {
	path := b.arg("path")
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", !b.ok && b.hasOk))
	}
	return out.String()
}

func renderListDirBlock(b *block, w int) string {
	path := b.arg("path")
	entries := splitNonEmpty(strings.TrimSpace(b.output), "\n")
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	if len(entries) > 0 {
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d entries", len(entries))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		if len(entries) == 0 {
			out.WriteString("\n" + renderToolBody(b, w, "lines", false))
			return out.String()
		}
		cw := panelCW(w)
		rows := make([]string, 0, len(entries))
		for _, e := range entries {
			if strings.HasSuffix(e, "/") {
				rows = append(rows, roToolNameStyle.Render("▸ ")+roToolNameStyle.Render(truncate(e, cw-2)))
			} else {
				rows = append(rows, resultStyle.Render("  "+truncate(e, cw-2)))
			}
		}
		out.WriteString("\n" + renderRowsPanel(rows, b.expanded, w, "entries", false))
	}
	return out.String()
}

func renderGrepBlock(b *block, w int) string {
	pattern := b.arg("pattern")
	gpath := b.arg("path")
	matches := splitNonEmpty(strings.TrimSpace(b.output), "\n")
	var segs []string
	if pattern != "" {
		segs = append(segs, codeInlineStyle.Render(`"`+pattern+`"`))
	}
	if gpath != "" {
		segs = append(segs, dimStyle.Render("in "+gpath))
	}
	if len(matches) > 0 {
		word := "matches"
		if len(matches) == 1 {
			word = "match"
		}
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d %s", len(matches), word)))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "matches", false))
	}
	return out.String()
}

func renderGlobBlock(b *block, w int) string {
	pattern := b.arg("pattern")
	files := splitNonEmpty(strings.TrimSpace(b.output), "\n")
	var segs []string
	if pattern != "" {
		segs = append(segs, codeInlineStyle.Render(`"`+pattern+`"`))
	}
	if len(files) > 0 {
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d file%s", len(files), pluralS(len(files)))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "files", false))
	}
	return out.String()
}

func renderGitStatusBlock(b *block, w int) string {
	path := b.arg("path")
	lines := splitNonEmpty(strings.TrimSpace(b.output), "\n")
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		if len(lines) == 0 {
			out.WriteString("\n" + renderToolBody(b, w, "lines", false))
			return out.String()
		}
		cw := panelCW(w)
		rows := make([]string, 0, len(lines))
		for _, l := range lines {
			if strings.HasPrefix(l, "## ") {
				rows = append(rows, dimStyle.Render(truncate(strings.TrimPrefix(l, "## "), cw)))
				continue
			}
			if len(l) < 3 {
				rows = append(rows, resultStyle.Render(truncate(l, cw)))
				continue
			}
			code, file := l[:2], strings.TrimSpace(l[2:])
			rows = append(rows, gitStatusCodeStyle(code).Render(code)+" "+resultStyle.Render(truncate(file, cw-3)))
		}
		out.WriteString("\n" + renderRowsPanel(rows, b.expanded, w, "entries", false))
	}
	return out.String()
}

// gitStatusCodeStyle tints a 2-char git status code: M amber, A green, D/?? red,
// R cyan, else dim.
func gitStatusCodeStyle(code string) lipgloss.Style {
	switch {
	case strings.HasPrefix(code, "??"):
		return lipgloss.NewStyle().Foreground(lipgloss.Color(c.err))
	case strings.Contains(code, "D"):
		return lipgloss.NewStyle().Foreground(lipgloss.Color(c.err))
	case strings.Contains(code, "A"):
		return lipgloss.NewStyle().Foreground(lipgloss.Color(c.success))
	case strings.Contains(code, "M"):
		return lipgloss.NewStyle().Foreground(lipgloss.Color(c.warn))
	case strings.Contains(code, "R"):
		return lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent))
	default:
		return dimStyle
	}
}

func renderGitLogBlock(b *block, w int) string {
	path := b.arg("path")
	commits := splitNonEmpty(strings.TrimSpace(b.output), "\n")
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	if len(commits) > 0 {
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d commit%s", len(commits), pluralS(len(commits)))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		if len(commits) == 0 {
			out.WriteString("\n" + renderToolBody(b, w, "lines", false))
			return out.String()
		}
		cw := panelCW(w)
		rows := make([]string, 0, len(commits))
		for _, cm := range commits {
			parts := strings.SplitN(cm, " ", 2)
			hash := parts[0]
			subj := ""
			if len(parts) > 1 {
				subj = parts[1]
			}
			subjW := cw - 9 // hash(7) + 2 spaces
			if subjW < 2 {
				subjW = 2
			}
			rows = append(rows, roToolNameStyle.Render(truncate(hash, 7))+"  "+resultStyle.Render(truncate(subj, subjW)))
		}
		out.WriteString("\n" + renderRowsPanel(rows, b.expanded, w, "commits", false))
	}
	return out.String()
}

func renderGitDiffBlock(b *block, w int) string {
	path := b.arg("path")
	staged := b.arg("staged") == "true"
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	} else if staged {
		segs = append(segs, dimStyle.Render("staged"))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", false))
	}
	return out.String()
}

func renderGitAddBlock(b *block, w int) string {
	paths := b.argStrArr("paths")
	var segs []string
	if n := len(paths); n > 0 {
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d file%s", n, pluralS(n))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		if len(paths) == 0 {
			out.WriteString("\n" + renderToolBody(b, w, "lines", false))
			return out.String()
		}
		cw := panelCW(w)
		rows := make([]string, 0, len(paths))
		for _, p := range paths {
			rows = append(rows, successStyle.Render("+ ")+resultStyle.Render(truncate(p, cw-2)))
		}
		out.WriteString("\n" + renderRowsPanel(rows, b.expanded, w, "files", false))
	}
	return out.String()
}

func renderGitCommitBlock(b *block, w int) string {
	msg := b.arg("message")
	head, failed := renderToolHead(b, w, "")
	var out strings.Builder
	out.WriteString(head)
	if msg != "" {
		out.WriteString("\n" + whatLine(roToolNameStyle.Render("$ "), `git commit -m "`+msg+`"`, baseStyle, w))
	}
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", failed))
	}
	return out.String()
}

// todoCheckbox returns the checkbox glyph + its style for a todo status.
func todoCheckbox(status string) (string, lipgloss.Style) {
	switch status {
	case "completed":
		return "[✓]", successStyle
	case "in_progress":
		return "[•]", roToolNameStyle
	default:
		return "[○]", dimStyle
	}
}

func countTodoStatuses(todos []map[string]json.RawMessage) (done, pend, run int) {
	for _, t := range todos {
		switch get(t, "status") {
		case "completed":
			done++
		case "in_progress":
			run++
		default:
			pend++
		}
	}
	return
}

// renderTodoRows builds checklist rows from a parsed todo list.
func renderTodoRows(todos []map[string]json.RawMessage, cw int) []string {
	rows := make([]string, 0, len(todos))
	for _, t := range todos {
		subj := get(t, "subject")
		ck, stStyle := todoCheckbox(get(t, "status"))
		rows = append(rows, stStyle.Render(ck+" ")+baseStyle.Render(truncate(subj, cw-4)))
	}
	return rows
}

func renderTodoWriteBlock(b *block, w int) string {
	todos := b.argObjArr("todos")
	var segs []string
	if n := len(todos); n > 0 {
		done, pend, run := countTodoStatuses(todos)
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d items", n)))
		segs = append(segs, dimStyle.Render(fmt.Sprintf("(%d✓ %d○ %d•)", done, run, pend)))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if len(todos) > 0 {
		out.WriteString("\n" + renderRowsPanel(renderTodoRows(todos, panelCW(w)), b.expanded, w, "items", false))
	} else if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", false))
	}
	return out.String()
}

func renderTodoReadBlock(b *block, w int) string {
	var todos []map[string]json.RawMessage
	_ = json.Unmarshal([]byte(strings.TrimSpace(b.output)), &todos)
	var segs []string
	if n := len(todos); n > 0 {
		done, pend, run := countTodoStatuses(todos)
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d items", n)))
		segs = append(segs, dimStyle.Render(fmt.Sprintf("(%d✓ %d○ %d•)", done, run, pend)))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		if len(todos) > 0 {
			out.WriteString("\n" + renderRowsPanel(renderTodoRows(todos, panelCW(w)), b.expanded, w, "items", false))
		} else {
			out.WriteString("\n" + renderToolBody(b, w, "lines", false))
		}
	}
	return out.String()
}

func countDiag(lines []string) (errors, warnings int) {
	for _, l := range lines {
		lc := strings.ToLower(l)
		switch {
		case strings.Contains(lc, "error"):
			errors++
		case strings.Contains(lc, "warning"):
			warnings++
		}
	}
	return
}

func renderDiagnosticsBlock(b *block, w int) string {
	path := b.arg("path")
	raw := strings.TrimSpace(b.output)
	lines := splitNonEmpty(raw, "\n")
	ne, nw := countDiag(lines)
	var segs []string
	if path != "" {
		segs = append(segs, baseStyle.Render(path))
	}
	var parts []string
	if ne > 0 {
		parts = append(parts, fmt.Sprintf("%d error%s", ne, pluralS(ne)))
	}
	if nw > 0 {
		parts = append(parts, fmt.Sprintf("%d warning%s", nw, pluralS(nw)))
	}
	if len(parts) > 0 {
		segs = append(segs, dimStyle.Render("· "+strings.Join(parts, " · ")))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "issues", !b.ok && b.hasOk))
	}
	return out.String()
}

func renderFetchBlock(b *block, w int) string {
	url := b.arg("url")
	var segs []string
	if url != "" {
		segs = append(segs, roToolNameStyle.Render(truncate(url, max(2, w-18))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", !b.ok && b.hasOk))
	}
	return out.String()
}

func renderMemoryBlock(b *block, w int) string {
	action := b.arg("action")
	name := b.arg("name")
	mtype := b.arg("type")
	var segs []string
	if action != "" {
		segs = append(segs, baseStyle.Render(action))
	}
	if name != "" {
		segs = append(segs, codeInlineStyle.Render(`"`+truncate(name, w-24)+`"`))
	}
	if mtype != "" {
		segs = append(segs, dimStyle.Render("· "+mtype))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", !b.ok && b.hasOk))
	}
	return out.String()
}

// renderFinishBlock is a one-liner: the finish tool has no args worth showing,
// so put the completion message in the header instead of an empty ({}) body.
func renderFinishBlock(b *block, w int) string {
	msg := strings.TrimSpace(b.output)
	if msg == "" || msg == "[no result]" {
		msg = "This turn has finished"
	}
	extra := ""
	if !b.inFlight() {
		extra = dimStyle.Render(truncate(msg, max(2, w-20)))
	}
	head, _ := renderToolHead(b, w, extra)
	return head
}

func renderSubagentBlock(b *block, w int) string {
	agent := b.arg("agent")
	task := b.arg("task")
	var segs []string
	if agent != "" {
		segs = append(segs, baseStyle.Render(agent))
	}
	if task != "" {
		segs = append(segs, dimStyle.Render(`"`+truncate(task, w-28)+`"`))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", !b.ok && b.hasOk))
	}
	return out.String()
}

func renderBulkBlock(b *block, w int) string {
	calls := b.argObjArr("calls")
	var segs []string
	if n := len(calls); n > 0 {
		segs = append(segs, dimStyle.Render(fmt.Sprintf("· %d call%s", n, pluralS(n))))
	}
	head, _ := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if len(calls) > 0 {
		cw := panelCW(w)
		rows := make([]string, 0, len(calls))
		for i, c := range calls {
			cn := get(c, "name")
			var ia map[string]json.RawMessage
			if raw, ok := c["args"]; ok {
				_ = json.Unmarshal(raw, &ia)
			}
			detail := get(ia, "path")
			if detail == "" {
				detail = get(ia, "command")
			}
			if detail == "" {
				detail = get(ia, "pattern")
			}
			if detail == "" {
				detail = get(ia, "url")
			}
			seg := dimStyle.Render(fmt.Sprintf("[%d] ", i+1)) +
				toolNameStyleFor(cn).Render(toolIcon(cn)+" "+toolDisplayName(cn))
			if detail != "" {
				seg += "  " + resultStyle.Render(truncate(detail, max(2, cw-len(cn)-10)))
			}
			rows = append(rows, seg)
		}
		out.WriteString("\n" + renderRowsPanel(rows, b.expanded, w, "results", false))
	} else if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", false))
	}
	return out.String()
}

func renderGenericToolBlock(b *block, w int) string {
	ka := toolKeyArg(b)
	var segs []string
	if ka != "" {
		segs = append(segs, baseStyle.Render(truncate(ka, w-16)))
	} else if b.args != "" {
		segs = append(segs, dimStyle.Render("("+truncate(b.args, w-16)+")"))
	}
	head, failed := renderToolHead(b, w, joinSegs(segs...))
	var out strings.Builder
	out.WriteString(head)
	if !b.inFlight() {
		out.WriteString("\n" + renderToolBody(b, w, "lines", failed))
	}
	return out.String()
}

// ---- numbered output (read_file) -------------------------------------------

// renderNumberedOutput is renderOutputPanel with a right-aligned line-number
// gutter, so a read_file call reads like an editor snippet. startLine is the
// number of the first output line (the read offset, or 1).
func renderNumberedOutput(output string, startLine int, expanded bool, w int, err bool) string {
	output = strings.TrimSpace(output)
	if output == "" {
		return dimStyle.Italic(true).Render("│ (no output)")
	}
	const headLines = 3
	lines := strings.Split(output, "\n")
	if len(lines) > headLines && !expanded {
		more := len(lines) - headLines
		shown := numberedLines(lines[:headLines], startLine, w, err)
		hint := dimStyle.Italic(true).Render(fmt.Sprintf("│ … +%d lines  (ctrl+o expand)", more))
		return shown + "\n" + hint
	}
	panel := numberedLines(lines, startLine, w, err)
	if len(lines) > headLines && expanded {
		panel += "\n" + dimStyle.Italic(true).Render("│ (ctrl+o collapse)")
	}
	return panel
}

func numberedLines(lines []string, startLine, w int, err bool) string {
	contentW := panelCW(w)
	rule := dimStyle
	numStyle := dimStyle
	textStyle := resultStyle
	if err {
		rule = errRuleStyle
		numStyle = errRuleStyle
		textStyle = errOutStyle
	}
	last := startLine + len(lines) - 1
	gw := len(fmt.Sprintf("%d", last))
	if gw < 3 {
		gw = 3
	}
	avail := contentW - gw - 1 // num + trailing space
	if avail < 2 {
		avail = 2
	}
	r := rule.Render("│ ")
	var b strings.Builder
	for i, l := range lines {
		num := fmt.Sprintf("%*d", gw, startLine+i)
		blank := strings.Repeat(" ", gw)
		pieces := wrapLine(l, avail)
		for j, piece := range pieces {
			b.WriteString(r)
			if j == 0 {
				b.WriteString(numStyle.Render(num + " "))
			} else {
				b.WriteString(numStyle.Render(blank + " "))
			}
			b.WriteString(textStyle.Render(piece))
			b.WriteByte('\n')
		}
	}
	return strings.TrimRight(b.String(), "\n")
}
