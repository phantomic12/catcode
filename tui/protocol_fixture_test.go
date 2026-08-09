package main

import (
	"bufio"
	"encoding/json"
	"os"
	"testing"
)

func TestRustCommandFixturesRemainGoCompatible(t *testing.T) {
	file, err := os.Open("../protocol/fixtures/commands-v2.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()

	count := 0
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		var command map[string]any
		if err := json.Unmarshal(scanner.Bytes(), &command); err != nil {
			t.Fatalf("fixture line %d: %v", count+1, err)
		}
		if kind, ok := command["type"].(string); !ok || kind == "" {
			t.Fatalf("fixture line %d has no command type", count+1)
		}
		count++
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	if count != 73 {
		t.Fatalf("got %d command fixtures, want 73", count)
	}
}

func TestRustEventFixturesRemainGoCompatible(t *testing.T) {
	file, err := os.Open("../protocol/fixtures/events-v2.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()

	seen := map[string]bool{}
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		var event map[string]any
		if err := json.Unmarshal(scanner.Bytes(), &event); err != nil {
			t.Fatalf("event fixture line %d: %v", len(seen)+1, err)
		}
		kind, ok := event["type"].(string)
		if !ok || kind == "" {
			t.Fatalf("event fixture line %d has no event type", len(seen)+1)
		}
		if seen[kind] {
			t.Fatalf("duplicate event fixture %q", kind)
		}
		seen[kind] = true
		if version, ok := event["protocol_version"].(float64); !ok || version != 2 {
			t.Fatalf("event fixture %q has invalid protocol version", kind)
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	if len(seen) != 105 {
		t.Fatalf("got %d event fixtures, want 105", len(seen))
	}
}
