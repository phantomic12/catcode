package main

import (
	"strings"
	"testing"
)

func newInputSession(t *testing.T, width int) *session {
	t.Helper()
	s := initialSession()
	s.ready = true
	s.width, s.height = width, 40
	s.authed = true
	s.input.Focus()
	s.layout()
	return s
}

// TestInputBoxEmptyUsesOpenComposer: an empty input renders as a rounded card
// with the prompt/placeholder row inside.
func TestInputBoxEmptyUsesOpenComposer(t *testing.T) {
	s := newInputSession(t, 80)
	box := stripANSI(s.renderInputBox())
	lines := strings.Split(box, "\n")
	if len(lines) != 3 {
		t.Fatalf("empty composer should be 3 lines, got %d:\n%s", len(lines), box)
	}
	if !strings.HasPrefix(lines[0], "╭") || !strings.Contains(lines[1], "❯ ") {
		t.Fatalf("composer missing card border or prompt:\n%s", box)
	}
	if !strings.Contains(lines[1], "Chat with the agent") {
		t.Fatalf("input box missing placeholder:\n%s", box)
	}
}

func TestAnimatedComposerOptionKeepsNewChrome(t *testing.T) {
	t.Setenv("CATCODE_ANIMATED_BORDER", "1")
	s := newInputSession(t, 80)
	s.busy = true
	box := stripANSI(s.renderInputBox())
	if !strings.Contains(box, "compose") {
		t.Fatalf("animated composer option bypassed the shared chrome:\n%s", box)
	}
}

// TestInputBoxWrapsLongMessage: a value longer than the box width soft-wraps
// onto multiple rows instead of scrolling one line — every char is still
// visible and continuation rows align beneath the prompt.
func TestInputBoxWrapsLongMessage(t *testing.T) {
	s := newInputSession(t, 40) // inner width = 36
	s.input.SetValue(strings.Repeat("a", 80))
	s.layout() // recompute now that the input grew

	box := stripANSI(s.renderInputBox())
	lines := strings.Split(box, "\n")
	if len(lines) <= 3 {
		t.Fatalf("a long message should wrap to multiple composer rows, got %d:\n%s", len(lines), box)
	}
	if !strings.HasPrefix(lines[0], "╭") || !strings.Contains(lines[1], "❯ ") {
		t.Fatalf("composer missing card border/prompt:\n%s", box)
	}
	for i := 2; i < len(lines)-1; i++ {
		if !strings.HasPrefix(lines[i], "│   ") {
			t.Fatalf("continuation row %d is not prompt-aligned: %q", i, lines[i])
		}
	}
	// wrapping must not drop any characters
	if got := strings.Count(box, "a"); got != 80 {
		t.Fatalf("wrapping dropped/duplicated chars: want 80 a's, got %d", got)
	}
	// the box grew to fit the wrapped content
	if h := s.inputBoxHeight(); h <= 2 {
		t.Fatalf("inputBoxHeight should grow past 2 for a wrapped message, got %d", h)
	}
}

// TestInputBoxCapsVeryLong: a pathologically long message is windowed so the
// box never eats the whole screen (bounded height).
func TestInputBoxCapsVeryLong(t *testing.T) {
	s := newInputSession(t, 40) // inner width = 36
	s.input.SetValue(strings.Repeat("b", 1000))
	s.layout()

	box := s.renderInputBox()
	h := lipglossHeight(box)
	// maxInputLines content + up to 2 "…" markers + top and bottom card borders.
	const ceiling = maxInputLines + 2 + 2
	if h > ceiling {
		t.Fatalf("box height %d exceeds cap %d for a 1000-char message:\n%s", h, ceiling, box)
	}
}

// TestInputBoxShrinksAfterClear: after wrapping, clearing the input returns
// the box to the single-line placeholder.
func TestInputBoxShrinksAfterClear(t *testing.T) {
	s := newInputSession(t, 40)
	s.input.SetValue(strings.Repeat("c", 80))
	s.layout()
	if s.inputBoxHeight() <= 2 {
		t.Fatalf("expected wrapped composer to be > 2 lines")
	}
	s.input.Reset()
	s.layout()
	if h := s.inputBoxHeight(); h != 3 {
		t.Fatalf("after clearing, composer should be 3 lines, got %d:\n%s", h, stripANSI(s.renderInputBox()))
	}
}

// lipglossHeight is a thin alias to avoid importing lipgloss in the test.
func lipglossHeight(s string) int {
	// count newlines + 1 of the ANSI-stripped string
	clean := stripANSI(s)
	if clean == "" {
		return 0
	}
	return strings.Count(clean, "\n") + 1
}
