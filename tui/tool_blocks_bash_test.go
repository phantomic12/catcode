package main

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestSummarizeBashCommand(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{
			name: "single line",
			in:   "go test ./...",
			want: "go test ./...",
		},
		{
			name: "single line with extra spaces",
			in:   "  go   test   ./...  ",
			want: "go test ./...",
		},
		{
			name: "cd plus heredoc runner",
			in: `cd /home/karutoil/glm-5.2-ai-harnesss
python3 << 'PY'
from pathlib import Path
print('hi')
PY`,
			want: "cd /home/karutoil/glm-5.2-ai-harnesss · python3 << 'PY' · 5 lines",
		},
		{
			name: "skips blank and comment lead",
			in: `# setup
# more setup

cargo test -p core`,
			want: "cargo test -p core · 4 lines",
		},
		{
			name: "comment only",
			in:   "# just a note\n# another",
			want: "# just a note # another",
		},
		{
			name: "empty",
			in:   "   \n  ",
			want: "",
		},
		{
			name: "chained cd stays lead",
			in: `cd /tmp && make all
echo done`,
			want: "cd /tmp && make all · 2 lines",
		},
		{
			name: "multi without cd",
			in: `export FOO=1
go build ./...
go test ./...`,
			want: "export FOO=1 · 3 lines",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := summarizeBashCommand(tc.in)
			if got != tc.want {
				t.Fatalf("summarizeBashCommand() = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestCompactBashActivityIsOneLine(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 100, 30
	s.layout()

	cmd := "cd /tmp/project\npython3 << 'PY'\nprint('x' * 200)\nPY"
	tb := s.logTool("bash", `{"command":`+jsonQuote(cmd)+`}`, false)
	tb.hasOk, tb.ok, tb.dur = true, true, 10
	s.invalidateAll()

	row := stripANSI(s.renderCompactToolRow(tb, 88))
	if strings.Contains(row, "\n") {
		t.Fatalf("collapsed bash row must be one physical line, got:\n%q", row)
	}
	if !strings.Contains(row, "cd /tmp/project") {
		t.Fatalf("collapsed row missing cd lead:\n%s", row)
	}
	if !strings.Contains(row, "python3") {
		t.Fatalf("collapsed row missing work command:\n%s", row)
	}
	if !strings.Contains(row, "lines") {
		t.Fatalf("collapsed row missing line count:\n%s", row)
	}
	if strings.Contains(row, "print(") {
		t.Fatalf("collapsed row leaked script body:\n%s", row)
	}

	// Expanded still shows the full command via whatLine.
	tb.expanded = true
	expanded := stripANSI(s.renderToolBlock(tb, 88))
	if !strings.Contains(expanded, "print(") {
		t.Fatalf("expanded bash should show full script body:\n%s", expanded)
	}
}

// TestApprovalBashShowsSensitivePayload ensures the approval banner does not
// hide a later sensitive line behind the activity-style one-liner (cd + first
// work line + "N lines"). Consent must surface the real script body.
func TestApprovalBashShowsSensitivePayload(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 160, 30
	s.layout()

	cmd := "cd /tmp\nrm -rf /important/production-data\necho done"
	s.pendingApproval = &approvalPrompt{
		requestID: "r-bash",
		tool:      "bash",
		args:      `{"command":` + jsonQuote(cmd) + `}`,
	}
	banner := stripANSI(s.renderApprovalBanner())
	if !strings.Contains(banner, "rm -rf /important/production-data") {
		t.Fatalf("approval banner hid sensitive payload behind summary:\n%s", banner)
	}
	// Must not be the aggressive activity summary form.
	if strings.Contains(banner, "· 3 lines") {
		t.Fatalf("approval banner used activity summarizer:\n%s", banner)
	}
	// Activity path still summarizes aggressively (cd + next work line + count).
	summary := summarizeBashCommand(cmd)
	if summary != "cd /tmp · rm -rf /important/production-data · 3 lines" {
		t.Fatalf("activity summary unexpected: %q", summary)
	}
}

func jsonQuote(s string) string {
	b, err := json.Marshal(s)
	if err != nil {
		panic(err)
	}
	return string(b)
}
