package main

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"
	"github.com/atotto/clipboard"
)

func navigableBlock(b *block) bool {
	if b == nil {
		return false
	}
	switch b.kind {
	case blkUser, blkAssistant, blkThinking, blkTool, blkToolResult, blkAdvisor, blkError, blkApprove:
		return true
	default:
		return false
	}
}

func (s *session) moveTranscriptFocus(delta int) {
	var idx []int
	for i, b := range s.blocks {
		if navigableBlock(b) {
			idx = append(idx, i)
		}
	}
	if len(idx) == 0 {
		s.logInfo("no transcript blocks to navigate")
		return
	}
	pos := -1
	for i, n := range idx {
		if n == s.focusedBlock {
			pos = i
			break
		}
	}
	if pos < 0 {
		if delta < 0 {
			pos = len(idx)
		} else {
			pos = -1
		}
	}
	pos = (pos + delta + len(idx)) % len(idx)
	s.focusedBlock = idx[pos]
	s.follow = false
	s.invalidateAll()
	s.refresh()
	b := s.blocks[s.focusedBlock]
	off := max(0, b.renderStart-max(1, s.viewport.Height()/3))
	s.viewport.SetYOffset(off)
	s.logInfo(fmt.Sprintf("focused %s · %d/%d", blockRoleName(b), pos+1, len(idx)))
}

func blockRoleName(b *block) string {
	switch b.kind {
	case blkUser:
		return "user message"
	case blkAssistant:
		return "assistant reply"
	case blkThinking:
		return "reasoning"
	case blkTool, blkToolResult:
		return "tool output"
	case blkAdvisor:
		return "advisor review"
	case blkApprove:
		return "approval"
	case blkError:
		return "error"
	default:
		return "block"
	}
}

func blockSearchText(b *block) string {
	return strings.Join([]string{b.text.String(), b.name, b.args, b.output, b.diff}, "\n")
}

func (s *session) findTranscript(query string) bool {
	query = strings.ToLower(strings.TrimSpace(query))
	if query == "" {
		return false
	}
	start := s.focusedBlock + 1
	for n := 0; n < len(s.blocks); n++ {
		i := (start + n) % len(s.blocks)
		if navigableBlock(s.blocks[i]) && strings.Contains(strings.ToLower(blockSearchText(s.blocks[i])), query) {
			s.focusedBlock = i
			s.follow = false
			s.invalidateAll()
			s.refresh()
			s.viewport.SetYOffset(max(0, s.blocks[i].renderStart-max(1, s.viewport.Height()/3)))
			s.logSuccess("transcript match: " + blockRoleName(s.blocks[i]))
			return true
		}
	}
	s.logWarn("no transcript match for “" + query + "”")
	return false
}

func (s *session) copyFocusedBlock() tea.Cmd {
	if s.focusedBlock < 0 || s.focusedBlock >= len(s.blocks) {
		return s.copyLastAssistant()
	}
	text := strings.TrimSpace(blockSearchText(s.blocks[s.focusedBlock]))
	if text == "" {
		s.logWarn("focused block has no copyable text")
		return nil
	}
	if err := clipboard.WriteAll(text); err == nil {
		s.logSuccess("copied focused block")
		return nil
	}
	s.logWarn("system clipboard unavailable; sent focused block via OSC 52")
	return writeOSC52Cmd(text)
}
