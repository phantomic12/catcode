package main

import (
	"fmt"
	"math"
	"strings"

	"charm.land/lipgloss/v2"
	"github.com/rivo/uniseg"
)

// ---------------------------------------------------------------------------
// Color palette (Catppuccin Mocha-ish; renders well on truecolor + 256-color)
// ---------------------------------------------------------------------------

var c = struct {
	bg        string
	fg        string
	dim       string
	muted     string
	decor     string // derived non-text boundary colour (>= 3:1 against bg)
	secondary string // derived supporting-text colour (>= 4.5:1 against bg)
	accent    string
	user      string
	assist    string
	tool      string
	success   string
	warn      string
	err       string
	// --- Derived surface + soft-fill tones (the Catalyst "lift/recess/glow" language) ---
	// These are computed from the authored palette by rebuildThemeStyles, never
	// hand-picked, so every theme (light or dark) keeps its own feel while the
	// structural hierarchy stays consistent. Catalyst keeps its exact hex values.
	surface     string // card tone lifted off the page bg (user msg, composer, welcome)
	sunken      string // recessed tone below the page bg (code fences, diffs, tool out)
	railDim     string // hairline: decor blended toward bg (quiet rails + borders)
	accentSoft  string // ~14% accent blended into bg (accent pill / selected chip bg)
	successSoft string
	warnSoft    string
	errSoft     string
	inlineCode  string // inline-code chip text (web's --inline-code-text peach)
}{
	bg:          "#1e1e2e",
	fg:          "#cdd6f4",
	dim:         "#6c7086",
	muted:       "#9399b2",
	decor:       "#6c7086",
	secondary:   "#9399b2",
	accent:      "#74c7ec",
	user:        "#89b4fa",
	assist:      "#cdd6f4",
	tool:        "#f9e2af",
	success:     "#a6e3a1",
	warn:        "#fab387",
	err:         "#f38ba8",
	surface:     "#262637",
	sunken:      "#16161f",
	railDim:     "#45455c",
	accentSoft:  "#232b33",
	successSoft: "#223025",
	warnSoft:    "#2b2624",
	errSoft:     "#2b2126",
	inlineCode:  "#a9dcef",
}

var (
	baseStyle          = lipgloss.NewStyle().Foreground(lipgloss.Color(c.fg))
	boldBaseStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.fg)).Bold(true)
	dimStyle           = lipgloss.NewStyle().Foreground(lipgloss.Color(c.secondary))
	mutedStyle         = lipgloss.NewStyle().Foreground(lipgloss.Color(c.secondary))
	accentStyle        = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent)).Bold(true)
	successStyle       = lipgloss.NewStyle().Foreground(lipgloss.Color(c.success))
	errStyle           = lipgloss.NewStyle().Foreground(lipgloss.Color(c.err))
	warnStyle          = lipgloss.NewStyle().Foreground(lipgloss.Color(c.warn)).Bold(true)
	assistantStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color(c.assist))
	thinkStyle         = lipgloss.NewStyle().Foreground(lipgloss.Color(c.secondary)).Italic(true)
	toolNameStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.tool)).Bold(true)
	toolDetailStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color(c.tool))
	resultStyle        = lipgloss.NewStyle().Foreground(lipgloss.Color(c.muted))
	headerStyle        = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent)).Bold(true)
	keyHintStyle       = lipgloss.NewStyle().Foreground(lipgloss.Color(c.secondary))
	separatorStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color(c.decor))
	inputPromptStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent))
	placeholderStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color(c.secondary))
	codeTextStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.fg))
	codeInlineStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color(c.inlineCode))
	italicStyle        = lipgloss.NewStyle().Foreground(lipgloss.Color(c.fg)).Italic(true)
	linkStyle          = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent)).Underline(true)
	roleUserStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.user)).Bold(true)
	roleAssistantStyle = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent)).Bold(true)
	roleThinkStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color(c.secondary)).Italic(true)
	roleToolStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.tool)).Bold(true)
	roleResultStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color(c.success)).Bold(true)
	roleErrorStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color(c.err)).Bold(true)
	roleWarnStyle      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.warn)).Bold(true)
	roleSuccessStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color(c.success)).Bold(true)
	toolDiffAdded      = lipgloss.NewStyle().Foreground(lipgloss.Color(c.success))
	toolDiffRemoved    = lipgloss.NewStyle().Foreground(lipgloss.Color(c.err))
	toolDiffContext    = lipgloss.NewStyle().Foreground(lipgloss.Color(c.muted))
	toolDiffMeta       = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent))
	roToolNameStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent)).Bold(true) // read-only tools
	errOutStyle        = lipgloss.NewStyle().Foreground(lipgloss.Color(c.err))               // failed tool output text
	errRuleStyle       = lipgloss.NewStyle().Foreground(lipgloss.Color(c.err))               // failed tool left rule
	selectionStyle     = lipgloss.NewStyle().Foreground(lipgloss.Color(c.bg)).Background(lipgloss.Color(c.accent)).Bold(true)

	// --- Structural surface / rail styles (rebuilt per theme) ---
	surfaceStyle        = lipgloss.NewStyle().Background(lipgloss.Color(c.surface))
	sunkenStyle         = lipgloss.NewStyle().Background(lipgloss.Color(c.sunken))
	railStyle           = lipgloss.NewStyle().Foreground(lipgloss.Color(c.railDim)) // quiet hairline rail
	userRailStyle       = lipgloss.NewStyle().Foreground(lipgloss.Color(c.accent))  // accent left rail on user turns
	composerBorderStyle = lipgloss.NewStyle().Foreground(lipgloss.Color(c.railDim)) // tonal composer border

	// Shared structural primitives. Surfaces establish depth; rails establish
	// grouping. Neither introduces colors beyond the active authored palette.
	cardStyle     = lipgloss.NewStyle().BorderStyle(lipgloss.RoundedBorder()).BorderForeground(lipgloss.Color(c.railDim)).Padding(0, 1)
	recessedStyle = lipgloss.NewStyle().BorderStyle(lipgloss.NormalBorder()).BorderForeground(lipgloss.Color(c.railDim)).Padding(0, 1)
	hairlineStyle = lipgloss.NewStyle().Foreground(lipgloss.Color(c.railDim))
)

// pillStyle returns a solid-background pill chip style for header tags.
// ponytail: no rounded border on pills — solid bg reads cleaner at small size.
func pillStyle(bg string) lipgloss.Style {
	if colorsDisabled() {
		return lipgloss.NewStyle().Bold(true).Underline(true).Padding(0, 1)
	}
	return lipgloss.NewStyle().
		Foreground(lipgloss.Color(c.bg)).
		Background(lipgloss.Color(bg)).
		Bold(true).
		Padding(0, 1)
}

// softPillStyle returns a tinted-fill chip: semantic text over a low-alpha
// wash of that colour (the web's /10 fills). Calmer than the solid pill for
// inline status, and theme-safe because the wash is derived per theme.
func softPillStyle(fg, bg string) lipgloss.Style {
	if colorsDisabled() {
		return lipgloss.NewStyle().Bold(true).Padding(0, 1)
	}
	return lipgloss.NewStyle().
		Foreground(lipgloss.Color(fg)).
		Background(lipgloss.Color(bg)).
		Bold(true).
		Padding(0, 1)
}

// statusDot renders the Catalyst "status dot": a filled glyph in the state
// colour. The web pairs it with a box-shadow glow; in the terminal the colour
// itself carries the glow against the ink bg.
func statusDot(color string) string {
	return lipgloss.NewStyle().Foreground(lipgloss.Color(color)).Render("●")
}

// statusPill renders a labelled tinted chip, e.g. "● ready" over a soft wash.
func statusPill(dotColor, label string) string {
	return softPillStyle(dotColor, c.accentSoft).Render(statusDot(dotColor) + " " + label)
}

// renderFlow applies a style to text line-by-line so lipgloss's multiline
// reflow (which pads short lines to the longest) never runs.
func renderFlow(text string, st lipgloss.Style) string {
	lines := strings.Split(text, "\n")
	for i, l := range lines {
		lines[i] = st.Render(l)
	}
	return strings.Join(lines, "\n")
}

// wrapPlain is a greedy display-cell-aware word-wrap. It iterates grapheme
// clusters, so combining marks and emoji ZWJ sequences are never split and
// wide CJK characters cannot overflow bordered surfaces.
func wrapPlain(text string, w int) string {
	if w < 1 {
		w = 1
	}
	var lines []string
	for _, line := range strings.Split(text, "\n") {
		lines = append(lines, wrapLine(line, w)...)
	}
	return strings.Join(lines, "\n")
}

func wrapLine(line string, w int) []string {
	type cluster struct {
		text  string
		width int
	}
	var cs []cluster
	g := uniseg.NewGraphemes(line)
	for g.Next() {
		cs = append(cs, cluster{text: g.Str(), width: g.Width()})
	}
	if len(cs) == 0 {
		return []string{""}
	}
	var out []string
	for len(cs) > 0 {
		width, end, lastSpace := 0, 0, -1
		for end < len(cs) {
			cw := cs[end].width
			if end > 0 && width+cw > w {
				break
			}
			// Always consume one cluster, even when it is wider than a
			// pathological one-column viewport, so wrapping makes progress.
			width += cw
			if cs[end].text == " " {
				lastSpace = end
			}
			end++
			if width >= w {
				break
			}
		}
		cut := end
		skip := 0
		if end < len(cs) && lastSpace > 0 {
			cut = lastSpace
			skip = 1
		}
		var b strings.Builder
		for _, c := range cs[:cut] {
			b.WriteString(c.text)
		}
		out = append(out, b.String())
		cs = cs[cut+skip:]
	}
	return out
}

// dimRule returns a dim horizontal rule of width w (used by markdown hr).
func dimRule(w int) string {
	if w < 1 {
		w = 1
	}
	return dimStyle.Render(strings.Repeat("─", w))
}

// ---------------------------------------------------------------------------
// Derived-tone colour helpers
//
// The redesign's depth comes from *derived* tones, not new authored palette
// entries — the Catalyst hex values stay fixed; we compute a lifted "surface",
// a recessed "sunken", quiet rails, and soft status fills from them. Deriving
// keeps every theme (light or dark) self-consistent and theme-switch safe.
// ---------------------------------------------------------------------------

// blendHex linearly blends hex colour a toward b by t∈[0,1] in RGB space.
// Unparseable input returns b (the safe fallback).
func blendHex(a, b string, t float64) string {
	a3, aok := parseHexColor(a)
	b3, bok := parseHexColor(b)
	if !aok || !bok {
		return b
	}
	if t < 0 {
		t = 0
	}
	if t > 1 {
		t = 1
	}
	return fmt.Sprintf("#%02x%02x%02x",
		int(math.Round((a3[0]+(b3[0]-a3[0])*t)*255)),
		int(math.Round((a3[1]+(b3[1]-a3[1])*t)*255)),
		int(math.Round((a3[2]+(b3[2]-a3[2])*t)*255)))
}

// liftTone moves a colour away from the page bg toward the fg by t, producing
// the Catalyst "surface" (card) tone: on dark themes a touch lighter, on light
// themes a touch darker — the same relational step either way.
func liftTone(base, toward string, t float64) string { return blendHex(base, toward, t) }

// softTone blends a semantic/accent colour into the page bg at a low alpha,
// giving the web's "tinted fill" for pills, chips, and status rails.
func softTone(status, bg string, t float64) string { return blendHex(status, bg, t) }

// deriveSurfaceTones fills the palette's structural tones from the authored
// colours. Called once per applyTheme so the derived values stay in sync.
func deriveSurfaceTones(t theme, isDark bool) {
	// Surface: a small lift off bg (the web ink-900 card vs ink-950 page).
	// Sunken: a small recess below bg (the web bg-code). On light themes the
	// "toward fg" direction inverts naturally.
	if isDark {
		// Calibrated so Catalyst dark lands on the web ink scale: surface ≈
		// card #242424, sunken ≈ bg-code #141414.
		c.surface = liftTone(t.bg, t.fg, 0.045)
		c.sunken = liftTone(t.bg, "#000000", 0.25)
	} else {
		c.surface = liftTone(t.bg, t.fg, 0.05)
		c.sunken = liftTone(t.bg, "#ffffff", 0.6)
	}
	// Quiet hairline rail: decor pulled partway toward bg so borders recede
	// (Catalyst dark lands near the web border #383838).
	c.railDim = blendHex(c.decor, t.bg, 0.50)
	// Soft fills: ~14% status into bg (web's /10 alpha fills).
	c.accentSoft = softTone(t.accent, t.bg, 0.16)
	c.successSoft = softTone(t.success, t.bg, 0.16)
	c.warnSoft = softTone(t.warn, t.bg, 0.18)
	c.errSoft = softTone(t.err, t.bg, 0.16)
	// Inline-code chip: the web's --inline-code-text (peach 219/178/148) is the
	// accent lightened toward fg — brighter than the accent so `code` reads on
	// both the page bg and the composer card, and clearly not the warn/tool amber.
	c.inlineCode = blendHex(t.accent, t.fg, 0.35)
}

// truncate clips a string to maxCells display columns, appending "…" if it
// was shortened. Grapheme clusters are kept intact.
func truncate(s string, maxCells int) string {
	if maxCells <= 0 {
		return s
	}
	if uniseg.StringWidth(s) <= maxCells {
		return s
	}
	if maxCells <= 1 {
		return "…"
	}
	var b strings.Builder
	used := 0
	g := uniseg.NewGraphemes(s)
	for g.Next() {
		if used+g.Width() > maxCells-1 {
			break
		}
		b.WriteString(g.Str())
		used += g.Width()
	}
	return b.String() + "…"
}

// truncatePath front-truncates a filesystem path (keeps the last segments, the
// part that identifies the file) so a deep cwd fits the header without losing
// the current directory name.
func truncatePath(p string, maxRunes int) string {
	if maxRunes <= 3 {
		return p
	}
	if uniseg.StringWidth(p) <= maxRunes {
		return p
	}
	// Build from the end while preserving whole grapheme clusters.
	var parts []string
	used := 1
	g := uniseg.NewGraphemes(p)
	for g.Next() {
		parts = append(parts, g.Str())
	}
	start := len(parts)
	for start > 0 {
		cw := uniseg.StringWidth(parts[start-1])
		if used+cw > maxRunes {
			break
		}
		used += cw
		start--
	}
	return "…" + strings.Join(parts[start:], "")
}

// ---------------------------------------------------------------------------
// Tool-call styling primitives
//
// toolKind mirrors core/tools.rs::classify so the TUI can tint a tool's name
// (and icon) by its approval kind: read-only tools render cyan, destructive
// amber. This is display-only; the core remains the approval authority.
// ---------------------------------------------------------------------------

type toolKindT int

const (
	kindReadOnly toolKindT = iota
	kindDestructive
)

// toolKindOf returns the approval kind of a tool, mirroring the core's
// classify() so a glance at the header reveals whether the call mutates state.
func toolKindOf(name string) toolKindT {
	switch name {
	// Mirror core/src/tools.rs classify() ReadOnly set exactly.
	case "read_file", "list_dir", "grep", "glob", "bulk_read", "todo_read",
		"diagnostics", "finish", "contact_supervisor", "intercom",
		"git_status", "git_diff", "git_log", "memory",
		"web_search", "ask", "load_tools", "goal_write_plan", "workspace_activity":
		return kindReadOnly
	default:
		return kindDestructive
	}
}

// toolNameStyleFor returns the name style tinted by kind: read-only → accent
// (cyan), destructive → tool (amber).
func toolNameStyleFor(name string) lipgloss.Style {
	if toolKindOf(name) == kindReadOnly {
		return roToolNameStyle
	}
	return toolNameStyle
}

// toolIcon returns a per-family glyph for a tool, used as the header marker.
// One icon per family (not per tool) keeps the transcript scannable without
// glyph noise.
func toolIcon(name string) string {
	switch name {
	case "bash":
		return "❯"
	case "read_file", "bulk_read", "list_dir", "grep", "glob":
		return "▤"
	case "write_file", "edit", "patch", "bulk_write", "bulk_edit":
		return "✎"
	case "git_status", "git_diff", "git_log", "git_add", "git_commit":
		return "⎇"
	case "todo_write", "todo_read":
		return "☑"
	case "diagnostics":
		return "⊕"
	case "fetch", "web_search":
		return "↬"
	case "memory":
		return "❖"
	case "delete", "rename", "mkdir":
		return "✎"
	case "ask", "load_tools", "workspace_activity", "goal_write_plan", "test_env":
		return "▸"
	case "subagent", "spawn":
		return "◈"
	case "finish":
		return "✓"
	case "contact_supervisor", "intercom":
		return "✉"
	default:
		return "▸"
	}
}

// toolDisplayName maps wire names to friendlier header labels (e.g.
// git_status → "git status"). Falls back to the raw name.
func toolDisplayName(name string) string {
	switch name {
	case "git_status":
		return "git status"
	case "git_diff":
		return "git diff"
	case "git_log":
		return "git log"
	case "git_add":
		return "git add"
	case "git_commit":
		return "git commit"
	case "contact_supervisor":
		return "contact supervisor"
	}
	return name
}

// panelLines renders tool output with a left `│` rule (wrapped to fit), using
// the given rule + content styles. resultPanel selects dim/err styling by the
// `err` flag so a failed call's body reads red while scrolling.
func panelLines(output string, w int, rule, content lipgloss.Style) string {
	contentW := w - 3 // "│ " prefix + content
	if contentW < 2 {
		contentW = 2
	}
	r := rule.Render("│ ")
	wrapped := wrapPlain(output, contentW)
	var b strings.Builder
	for _, l := range strings.Split(wrapped, "\n") {
		b.WriteString(r)
		b.WriteString(content.Render(l))
		b.WriteByte('\n')
	}
	return strings.TrimRight(b.String(), "\n")
}
