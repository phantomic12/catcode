package main

import (
	"testing"

	tea "charm.land/bubbletea/v2"
)

func TestSessionTreeIsDiscoverableAndDispatches(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 80, 24
	found := false
	for _, item := range s.commandItems() {
		if item.label == "/tree" {
			found = true
			break
		}
	}
	if !found {
		t.Fatal("/tree should appear in the command palette")
	}
	writer := &captureWriter{}
	s.coreIn = writer
	s.handleUserLine("/tree")
	if len(writer.lines) != 1 || writer.lines[0] != `{"type":"session_tree"}` {
		t.Fatalf("/tree should send session_tree, got %#v", writer.lines)
	}
}

func TestSkillPaletteOpensTaskModal(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 80, 24
	s.skillsList = []skillInfo{{Name: "frontend-design", Description: "design skill"}}
	s.openCommandPalette()
	idx := -1
	for i, it := range s.commandItems() {
		if it.label == "/skill:frontend-design" {
			idx = i
			break
		}
	}
	if idx < 0 {
		t.Fatal("skill entry should appear in the command palette")
	}
	s.runCommandByIndex(idx)
	if s.modal.kind != modalValueEdit || s.modal.editTarget != editTargetSkill+"frontend-design" {
		t.Fatalf("skill palette should open task modal; kind=%v target=%q", s.modal.kind, s.modal.editTarget)
	}
}

// TestEnterSelectsEvenWhenSelectUnbound: the list modals (command palette,
// models, theme, …) must keep a hardcoded "enter" fallback for selecting, so
// clearing the "select" binding via /keybinds can never trap the user out of
// the palette. Mirrors the guarantee the keybinds modal already makes.
func TestEnterSelectsEvenWhenSelectUnbound(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.authed = true
	s.width, s.height = 80, 24
	s.models = []modelInfo{{ID: "m1", Name: "Model 1"}}
	s.modelIdx = 0
	s.openCommandPalette()

	// Disable the select binding entirely (as /keybinds Delete would).
	s.keybinds["select"] = ""

	// Enter must still fire the select. The first palette entry is /keybinds,
	// which opens the keybinds modal — so the assertion is that the palette
	// dispatched the selection (modal left modalCommand), not that it closed.
	before := s.modal.kind
	if before != modalCommand {
		t.Fatalf("precondition: palette should be open; kind=%v", before)
	}
	s.handleModalKey(tea.KeyPressMsg{Code: tea.KeyEnter})
	if s.modal.kind == modalCommand {
		t.Fatal("enter should select in the palette even with select unbound; palette stayed open")
	}
}

func TestSkillCommandOpensTaskModal(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.skillsList = []skillInfo{{Name: "frontend-design", Description: "design skill"}}
	s.handleUserLine("/skill:frontend-design")
	if s.modal.kind != modalValueEdit || s.modal.editTarget != editTargetSkill+"frontend-design" {
		t.Fatalf("skill command should open task modal; kind=%v target=%q", s.modal.kind, s.modal.editTarget)
	}
}
