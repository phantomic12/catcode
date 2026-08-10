package main

import (
	"encoding/json"
	"regexp"
	"strings"
	"testing"
	"time"
)

var goldenRuleRE = regexp.MustCompile(`[─━]{3,}`)

// canonicalVisual keeps hierarchy, wording, borders, and ordering while
// discarding terminal-centering whitespace and theme ANSI. It gives the UI a
// stable golden contract without making snapshots depend on a particular
// renderer's padding implementation.
func canonicalVisual(view string) string {
	view = stripANSI(view)
	var lines []string
	for _, line := range strings.Split(view, "\n") {
		line = strings.Join(strings.Fields(line), " ")
		line = goldenRuleRE.ReplaceAllString(line, "─")
		if line != "" {
			lines = append(lines, line)
		}
	}
	return strings.Join(lines, "\n")
}

func visualSession(w, h int) *session {
	s := initialSession()
	s.ready = true
	s.coreLifecycle = coreReady
	s.width, s.height = w, h
	s.authed = true
	s.models = []modelInfo{{ID: "glm-5.2", Provider: "umans", ContextWindow: 128000, MaxTokens: 8192}}
	s.modelIdx = 0
	s.contextTokens = 12000
	s.lastMetrics = json.RawMessage(`{"tps":"42.1","ttft_ms":"180"}`)
	s.settings.FooterMetrics = true
	s.cwd = "~/project"
	s.toast = nil
	s.layout()
	return s
}

func TestVisualGoldens(t *testing.T) {
	t.Run("40x12 idle", func(t *testing.T) {
		s := visualSession(40, 12)
		got := canonicalVisual(s.View().Content)
		want := `◆ Catalyst / Code ● ready
START A SESSION
▸ 1 Understand this repository
↑↓ choose · enter use · / commands
╭ compose ─╮
│ ❯ Chat with the agent… (/ commands… │
╰─╯
Enter send
▰▱▱▱▱▱▱▱▱▱ 9% · 12.0k`
		if got != want {
			t.Fatalf("visual golden changed:\n--- got ---\n%s\n--- want ---\n%s", got, want)
		}
	})

	t.Run("80x24 approval", func(t *testing.T) {
		s := visualSession(80, 24)
		s.logUser("Delete generated build artifacts")
		s.pendingApproval = &approvalPrompt{requestID: "r1", tool: "bash", args: `{"command":"rm -rf dist"}`}
		s.layout()
		got := canonicalVisual(s.View().Content)
		want := `◆ Catalyst / Code ● ready · glm-5.2
PROJECT ~/project MODEL glm-5.2
● YOU ─
▌ Delete generated build artifacts
⚠ approval required ❯ bash rm -rf dist [Y] once · [N] deny · [A] type
╭ compose ─╮
│ ❯ Type a follow-up, or clear input to use the approval keys… │
╰─╯
Y allow once · N deny · A always allow type ▱▱▱▱▱▱▱▱▱▱ 9% 12.0k/128.0k
glm-5.2 · 42 tok/s · 180ms ttft`
		if got != want {
			t.Fatalf("visual golden changed:\n--- got ---\n%s\n--- want ---\n%s", got, want)
		}
	})

	t.Run("120x40 activity", func(t *testing.T) {
		s := visualSession(120, 40)
		s.logUser("Refactor the parser and verify it")
		s.todos = []map[string]json.RawMessage{
			{"subject": json.RawMessage(`"Refactor parser"`), "status": json.RawMessage(`"in_progress"`)},
			{"subject": json.RawMessage(`"Run tests"`), "status": json.RawMessage(`"pending"`)},
		}
		s.subProgress = []*subProgressEntry{{agent: "reviewer", started: time.Now(), curTool: "read_file", toolStart: time.Now(), toolRunning: true}}
		s.activityExpanded = true
		s.layout()
		got := canonicalVisual(s.View().Content)
		want := `◆ Catalyst / Code ● ready · glm-5.2
PROJECT ~/project MODEL glm-5.2
● YOU ─
▌ Refactor the parser and verify it
╭─╮
│ Activity · ↑↓ scroll · Esc close │
│ Subagents │
│ ◷ reviewer · 0:00 · ▤ read_file │
│ Tasks │
│ [•] Refactor parser │
│ [○] Run tests │
╰─╯
╭ compose ─╮
│ ❯ Chat with the agent… (/ commands · ? help) │
╰─╯
Enter send · Shift+Enter newline · Ctrl+P commands ▱▱▱▱▱▱▱▱▱▱ 9% 12.0k/128.0k
glm-5.2 · 42 tok/s · 180ms ttft`
		if got != want {
			t.Fatalf("visual golden changed:\n--- got ---\n%s\n--- want ---\n%s", got, want)
		}
	})
}
