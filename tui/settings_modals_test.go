package main

import (
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

// TestApprovalCommandOpensPicker: bare /approval (and /approvals) must open the
// dedicated approval modal, not the old multi-field settings editor.
func TestApprovalCommandOpensPicker(t *testing.T) {
	for _, cmd := range []string{"/approval", "/approvals"} {
		s := initialSession()
		s.ready = true
		s.width, s.height = 100, 40
		s.approvalModeStr = "destructive"
		s.handleUserLine(cmd)
		if s.modal.kind != modalApproval {
			t.Fatalf("%s: modal kind = %v, want modalApproval", cmd, s.modal.kind)
		}
		items := s.approvalItems()
		if len(items) != 3 {
			t.Fatalf("%s: approvalItems len=%d, want 3", cmd, len(items))
		}
		for i, mode := range []string{"never", "destructive", "always"} {
			if items[i].meta != mode {
				t.Errorf("%s: items[%d] mode=%q, want %q", cmd, i, items[i].meta, mode)
			}
		}
		body := stripANSI(s.renderModalBody())
		if !strings.Contains(body, "Approval Mode") {
			t.Errorf("%s: modal body missing title:\n%s", cmd, body)
		}
		// Selected mode should appear (cursor starts on current = destructive).
		if !strings.Contains(body, "Ask for destructive tools") {
			t.Errorf("%s: modal body missing current mode:\n%s", cmd, body)
		}
	}
}

func TestApprovalArgumentsStillOpenPicker(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.handleUserLine("/approval always")
	if s.modal.kind != modalApproval {
		t.Fatalf("approval arguments must not bypass picker; got %v", s.modal.kind)
	}
}
func TestAdvisorCommandOpensConfigurationModal(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.models = []modelInfo{{ID: "main"}, {ID: "reviewer"}}
	s.handleUserLine("/advisor on")
	if s.modal.kind != modalAdvisor {
		t.Fatalf("advisor arguments must open modal; got %v", s.modal.kind)
	}
	before := s.settings.AdvisorEnabled
	s.executeListSelect(0)
	if s.settings.AdvisorEnabled == before {
		t.Fatal("advisor toggle did not change state")
	}
	beforeNudge := s.settings.AdvisorNudge
	s.executeListSelect(1)
	if s.settings.AdvisorNudge == beforeNudge {
		t.Fatal("advisor checkpoint toggle did not change state")
	}
	s.executeListSelect(2)
	if s.modal.kind != modalAdvisorModels {
		t.Fatalf("main model should open picker; got %v", s.modal.kind)
	}
	s.executeListSelect(2)
	if s.settings.AdvisorModel != "reviewer" {
		t.Fatalf("advisor model=%q, want reviewer", s.settings.AdvisorModel)
	}
}

func TestAdvisorStatusIsVisibleInTranscript(t *testing.T) {
	s := initialSession()
	s.ready = true
	raw, err := json.Marshal(map[string]any{
		"type": "advisor_status", "scope": "main", "state": "reviewing",
		"advisor": "Correctness", "model": "ck-grok-4.5",
	})
	if err != nil {
		t.Fatal(err)
	}
	s.handleCoreEvent(&coreEvent{Type: "advisor_status", Raw: raw})
	if s.toast != nil {
		t.Fatal("advisor lifecycle status should be a transcript card, not a toast")
	}
	if len(s.blocks) != 1 || s.blocks[0].kind != blkAdvisor {
		t.Fatalf("reviewing status must create advisor block, blocks=%+v", s.blocks)
	}
	if !strings.Contains(stripANSI(s.renderBlocks()), "reviewing") {
		t.Fatal("reviewing status must be rendered in the transcript")
	}

	raw, err = json.Marshal(map[string]any{
		"type": "advisor_status", "scope": "main", "state": "failed",
		"advisor": "Correctness", "model": "ck-grok-4.5",
	})
	if err != nil {
		t.Fatal(err)
	}
	s.handleCoreEvent(&coreEvent{Type: "advisor_status", Raw: raw})
	if len(s.blocks) != 1 || s.blocks[0].advisorState != "failed" {
		t.Fatalf("terminal status should update existing advisor block, blocks=%+v", s.blocks)
	}
	if !strings.Contains(stripANSI(s.renderBlocks()), "executor continued") {
		t.Fatal("failed review must explain fail-open continuation")
	}
}

func TestAdvisorModalArrowKeysNavigate(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.keybinds = defaultKeybinds()
	s.openAdvisorModal()
	if s.modal.kind != modalAdvisor {
		t.Fatalf("expected advisor modal, got %v", s.modal.kind)
	}
	if s.modal.cursor != 0 {
		t.Fatalf("cursor=%d, want 0", s.modal.cursor)
	}
	_, _ = s.handleListKey(keyMsg("down"))
	if s.modal.cursor != 1 {
		t.Fatalf("down from 0: cursor=%d, want 1", s.modal.cursor)
	}
	_, _ = s.handleListKey(keyMsg("down"))
	if s.modal.cursor != 2 {
		t.Fatalf("down from 1: cursor=%d, want 2", s.modal.cursor)
	}
	_, _ = s.handleListKey(keyMsg("up"))
	if s.modal.cursor != 1 {
		t.Fatalf("up from 2: cursor=%d, want 1", s.modal.cursor)
	}
	// j/k alt binds should also move.
	_, _ = s.handleListKey(keyMsg("j"))
	if s.modal.cursor != 2 {
		t.Fatalf("j from 1: cursor=%d, want 2", s.modal.cursor)
	}
}

// TestApprovalEscalationDoesNotResetMode: core's "<kind>:always" event must not
// flip the footer/settings to "destructive" (normalizeApproval's fallback).
func TestApprovalEscalationDoesNotResetMode(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.settings.path = filepath.Join(t.TempDir(), "settings.json")
	s.applyApprovalMode("never")
	if s.approvalMode() != "never" {
		t.Fatalf("precondition: approvalMode=%q", s.approvalMode())
	}

	raw, _ := json.Marshal(map[string]any{
		"type": "approval_changed",
		"mode": "destructive:always",
	})
	s.handleCoreEvent(&coreEvent{Type: "approval_changed", Raw: raw})

	if s.approvalMode() != "never" {
		t.Fatalf("after escalation: approvalMode=%q, want never", s.approvalMode())
	}
	if s.settings.Approval != "never" {
		t.Fatalf("settings.Approval=%q, want never", s.settings.Approval)
	}
	// Stale escalation string must not leak into the display path.
	s.approvalModeStr = "destructive:always"
	if s.approvalMode() != "never" {
		t.Fatalf("approvalMode with leaked escalation=%q, want never (from settings)", s.approvalMode())
	}
}

// TestSettingsHubOpensDedicatedModals: /settings is a hub; selecting each
// entry opens the corresponding dedicated modal (not a field editor).
func TestSettingsHubOpensDedicatedModals(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 100, 40
	s.settings.path = filepath.Join(t.TempDir(), "settings.json")
	s.approvalModeStr = "destructive"
	s.coreBashTimeout = 30
	s.settings.IdleTimeout = 120

	s.handleUserLine("/settings")
	if s.modal.kind != modalSettings {
		t.Fatalf("modal kind = %v, want modalSettings", s.modal.kind)
	}
	body := stripANSI(s.renderModalBody())
	for _, want := range []string{"/approval", "/reasoning", "/theme", "/bash-timeout", "/sandbox"} {
		if !strings.Contains(body, want) {
			t.Errorf("settings hub missing %s:\n%s", want, body)
		}
	}
	if strings.Contains(body, "/mouse-wheel") {
		t.Errorf("always-on mouse interaction should not have an opt-in setting:\n%s", body)
	}
	// Must not show the old field-editor chrome.
	if strings.Contains(body, "enter edit/apply") || strings.Contains(body, "←→ cycle") {
		t.Errorf("settings hub still looks like the old field editor:\n%s", body)
	}

	// Select /approval from the hub.
	items := s.settingsHubItems()
	approvalIdx := -1
	for i, it := range items {
		if it.label == "/approval" {
			approvalIdx = i
			break
		}
	}
	if approvalIdx < 0 {
		t.Fatal("/approval not in settings hub")
	}
	s.modal.cursor = approvalIdx
	s.handleModalKey(tea.KeyPressMsg{Code: tea.KeyEnter})
	if s.modal.kind != modalApproval {
		t.Fatalf("after select /approval: kind = %v, want modalApproval", s.modal.kind)
	}
}

// TestDedicatedSettingCommandsOpenModals covers bare slash commands for each
// former settings field.
func TestDedicatedSettingCommandsOpenModals(t *testing.T) {
	cases := []struct {
		cmd  string
		kind modalKind
	}{
		{"/reasoning", modalReasoning},
		{"/theme", modalTheme},
		{"/bash-timeout", modalValueEdit},
		{"/auto-compact", modalAutoCompact},
		{"/sandbox", modalSandbox},
		{"/no-network", modalNoNetwork},
		{"/idle-timeout", modalValueEdit},
		{"/max-session-tokens", modalValueEdit},
		{"/settings", modalSettings},
	}
	for _, tc := range cases {
		s := initialSession()
		s.ready = true
		s.settings.path = filepath.Join(t.TempDir(), "settings.json")
		s.handleUserLine(tc.cmd)
		if s.modal.kind != tc.kind {
			t.Errorf("%s: kind = %v, want %v", tc.cmd, s.modal.kind, tc.kind)
		}
	}
}

func TestConfigurableCommandsIgnoreInlineArguments(t *testing.T) {
	cases := []struct {
		command string
		kind    modalKind
	}{
		{"/sandbox enable", modalSandbox},
		{"/auto-compact off", modalAutoCompact},
		{"/bash-timeout 90", modalValueEdit},
		{"/footer-metrics off", modalFooterMetrics},
	}
	for _, tc := range cases {
		s := initialSession()
		s.ready = true
		s.handleUserLine(tc.command)
		if s.modal.kind != tc.kind {
			t.Errorf("%s opened %v, want %v", tc.command, s.modal.kind, tc.kind)
		}
	}
}

// TestApprovalPickerSelectAppliesMode: enter on a mode in the approval modal
// persists it and closes the modal.
func TestApprovalPickerSelectAppliesMode(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.settings.path = filepath.Join(t.TempDir(), "settings.json")
	s.approvalModeStr = "destructive"
	s.openApprovalPicker()

	// Move to "always" (index 2) and select.
	s.modal.cursor = 2
	s.handleModalKey(tea.KeyPressMsg{Code: tea.KeyEnter})
	if s.modal.kind != modalNone {
		t.Fatalf("modal should close after select, kind=%v", s.modal.kind)
	}
	if s.settings.Approval != "always" {
		t.Errorf("settings.Approval = %q, want always", s.settings.Approval)
	}
}

// TestCommandPaletteApprovalOpensPicker: palette entry for /approval must not
// fall back to the old settings field editor.
func TestCommandPaletteApprovalOpensPicker(t *testing.T) {
	s := initialSession()
	s.ready = true
	s.openCommandPalette()
	// Find /approval in the palette and select it.
	items := s.commandItems()
	idx := -1
	for i, it := range items {
		if it.label == "/approval" {
			idx = i
			break
		}
	}
	if idx < 0 {
		t.Fatal("/approval missing from command palette")
	}
	// runCommandByIndex expects absolute index into commandItems (not filtered).
	s.closeModal()
	cmd := s.runCommandByIndex(idx)
	_ = cmd
	if s.modal.kind != modalApproval {
		t.Fatalf("palette /approval → kind=%v, want modalApproval", s.modal.kind)
	}
}
