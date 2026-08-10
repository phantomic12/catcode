package main

import (
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

func BenchmarkSmallSessionView(b *testing.B) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 40, 12
	s.authed = true
	s.models = []modelInfo{{ID: "glm-5.2", ContextWindow: 128000}}
	s.modelIdx = 0
	s.contextTokens = 12000
	for b.Loop() {
		s.View()
	}
}

func BenchmarkSmallSessionKey(b *testing.B) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 40, 12
	s.authed = true
	s.models = []modelInfo{{ID: "glm-5.2", ContextWindow: 128000}}
	s.modelIdx = 0
	s.layout()
	msg := tea.KeyPressMsg{Code: 'x', Text: "x"}
	b.ResetTimer()
	for b.Loop() {
		s.handleKey(msg)
		b.StopTimer()
		s.input.Reset()
		b.StartTimer()
	}
}

func BenchmarkSmallSessionKeyAndView(b *testing.B) {
	s := initialSession()
	s.ready = true
	s.width, s.height = 40, 12
	s.authed = true
	s.models = []modelInfo{{ID: "glm-5.2", ContextWindow: 128000}}
	s.modelIdx = 0
	s.layout()
	msg := tea.KeyPressMsg{Code: 'x', Text: "x"}
	b.ResetTimer()
	for b.Loop() {
		s.handleKey(msg)
		s.View()
		b.StopTimer()
		s.input.Reset()
		b.StartTimer()
	}
}

func liveTranscriptSession() *session {
	s := initialSession()
	s.ready = true
	s.width, s.height = 80, 24
	s.authed = true
	s.models = []modelInfo{{ID: "glm-5.2", ContextWindow: 128000}}
	s.modelIdx = 0
	s.logUser("Inspect the request flow and explain the failure mode.")
	s.cur = s.push(blkAssistant)
	s.cur.model = "glm-5.2"
	s.cur.appendText("The request enters the dispatcher, validates the payload, then waits for the provider response. ")
	s.layout()
	return s
}

func resetLivePayload(s *session, payload string) {
	s.cur.text.Reset()
	s.cur.appendText(payload)
	// The live renderer intentionally retains a snapshot below streamBatch.
	// Clear it here because this benchmark measures the expensive, scheduled
	// repaint path that runs once the threshold is reached.
	s.cur.renderStr = ""
	s.cur.renderLen = 0
	s.transcriptBase = ""
}

func BenchmarkLiveTranscriptRefresh(b *testing.B) {
	s := liveTranscriptSession()
	payloads := [2]string{strings.Repeat("x", streamBatch), strings.Repeat("y", streamBatch*2)}
	for i := 0; b.Loop(); i++ {
		b.StopTimer()
		resetLivePayload(s, payloads[i%len(payloads)])
		b.StartTimer()
		s.refresh()
	}
}

func BenchmarkLiveTranscriptRefreshAndView(b *testing.B) {
	s := liveTranscriptSession()
	payloads := [2]string{strings.Repeat("x", streamBatch), strings.Repeat("y", streamBatch*2)}
	for i := 0; b.Loop(); i++ {
		b.StopTimer()
		resetLivePayload(s, payloads[i%len(payloads)])
		b.StartTimer()
		s.refresh()
		s.View()
	}
}
