package main

import (
	"strings"
	"testing"
	"time"

	"charm.land/lipgloss/v2"
)

func busyWaveSession(t *testing.T) *session {
	t.Helper()
	s := initialSession()
	s.ready = true
	s.width, s.height = 80, 24
	s.models = []modelInfo{{ID: "m1", ContextWindow: 8192}}
	s.modelIdx = 0
	s.authed = true
	return s
}

// TestWorkingWaveWhileBusy proves the pulse line renders while the agent is
// working and claims exactly one row of layout height.
func TestWorkingWaveWhileBusy(t *testing.T) {
	s := busyWaveSession(t)
	s.busy = true

	out := s.renderWorkingWave()
	if out == "" {
		t.Fatal("working wave should render while busy")
	}
	plain := stripANSI(out)
	if !strings.ContainsAny(plain, "▁▂▃▄▅▆▇█") {
		t.Fatalf("working wave should contain sparkline runes; got %q", plain)
	}
	if w := lipgloss.Width(out); w != s.width {
		t.Fatalf("working wave should span the input card width %d; got %d", s.width, w)
	}
	if h := s.workingWaveHeight(); h != 1 {
		t.Fatalf("working wave height while busy should be 1; got %d", h)
	}
}

// TestWorkingWaveHiddenWhenIdle proves the indicator disappears (and claims no
// height) once the turn finishes, so the footer keeps its place.
func TestWorkingWaveHiddenWhenIdle(t *testing.T) {
	s := busyWaveSession(t)
	s.busy = false

	if out := s.renderWorkingWave(); out != "" {
		t.Fatalf("working wave should be empty while idle; got %q", out)
	}
	if h := s.workingWaveHeight(); h != 0 {
		t.Fatalf("working wave height while idle should be 0; got %d", h)
	}
}

// TestWorkingWaveReducedMotion proves reduced motion renders a static line:
// identical output at two different times (no time-based phase).
func TestWorkingWaveReducedMotion(t *testing.T) {
	s := busyWaveSession(t)
	s.busy = true
	s.settings.ReducedMotion = true

	first := s.renderWorkingWaveUncached()
	time.Sleep(150 * time.Millisecond) // cross at least one busy-frame boundary
	second := s.renderWorkingWaveUncached()
	if first != second {
		t.Fatal("reduced-motion working wave should be static across frames")
	}
	if !strings.Contains(stripANSI(first), "▄") {
		t.Fatalf("reduced-motion wave should be a mid-level dim line; got %q", stripANSI(first))
	}
}

func TestStreamRefreshIntervalScalesWithTranscriptSize(t *testing.T) {
	cases := []struct {
		blocks int
		want   time.Duration
	}{
		{blocks: 0, want: 66 * time.Millisecond},
		{blocks: 100, want: 66 * time.Millisecond},
		{blocks: 101, want: 100 * time.Millisecond},
		{blocks: 300, want: 100 * time.Millisecond},
		{blocks: 301, want: 150 * time.Millisecond},
	}
	for _, tc := range cases {
		if got := streamRefreshInterval(tc.blocks); got != tc.want {
			t.Errorf("streamRefreshInterval(%d) = %v, want %v", tc.blocks, got, tc.want)
		}
	}
}
