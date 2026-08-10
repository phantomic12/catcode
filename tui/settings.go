package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

var claimedSessionPath string
var claimedSessionLock string
var reservedSessionPath string
var reservedSessionLock string

// claimSession prevents two TUI processes in one workspace from appending to
// the same JSONL session. Day-old locks are treated as crash leftovers.
func claimSession(path string) bool {
	if !reserveSession(path) {
		return false
	}
	return commitSessionClaim(path)
}

// reserveSession acquires the target without releasing the currently active
// session. The old claim remains authoritative until core acknowledges that
// it switched files, preventing a failed/backpressured handoff from exposing
// the still-active JSONL to another process.
func reserveSession(path string) bool {
	if path == "" {
		return false
	}
	if path == claimedSessionPath {
		return true
	}
	if path == reservedSessionPath && reservedSessionLock != "" {
		return true
	}
	lock := path + ".lock"
	f, err := os.OpenFile(lock, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil && staleSessionLock(lock) {
		_ = os.Remove(lock)
		f, err = os.OpenFile(lock, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	}
	if err != nil {
		return false
	}
	_, _ = fmt.Fprintf(f, "pid=%d\nstarted=%s\n", os.Getpid(), time.Now().Format(time.RFC3339))
	_ = f.Close()
	if reservedSessionLock != "" && reservedSessionLock != lock {
		_ = os.Remove(reservedSessionLock)
	}
	reservedSessionPath, reservedSessionLock = path, lock
	return true
}

func staleSessionLock(lock string) bool {
	data, err := os.ReadFile(lock)
	if err != nil {
		return false
	}
	var pid int
	if _, err := fmt.Sscanf(string(data), "pid=%d", &pid); err == nil && pid > 0 {
		return !sessionLockProcessAlive(pid)
	}
	info, err := os.Stat(lock)
	return err == nil && time.Since(info.ModTime()) > 24*time.Hour
}

func commitSessionClaim(path string) bool {
	if path == claimedSessionPath {
		return true
	}
	if path != reservedSessionPath || reservedSessionLock == "" {
		return false
	}
	if claimedSessionLock != "" && claimedSessionLock != reservedSessionLock {
		_ = os.Remove(claimedSessionLock)
	}
	claimedSessionPath, claimedSessionLock = reservedSessionPath, reservedSessionLock
	reservedSessionPath, reservedSessionLock = "", ""
	return true
}

func cancelSessionReservation(path string) {
	if path != "" && path != reservedSessionPath {
		return
	}
	if reservedSessionLock != "" {
		_ = os.Remove(reservedSessionLock)
	}
	reservedSessionPath, reservedSessionLock = "", ""
}

func sessionLockedByAnotherProcess(path string) bool {
	if path == "" || path == claimedSessionPath || path == reservedSessionPath {
		return false
	}
	lock := path + ".lock"
	if staleSessionLock(lock) {
		_ = os.Remove(lock)
		return false
	}
	_, err := os.Stat(lock)
	return err == nil
}

func claimInitialSession() string {
	path := sessionPath()
	if claimSession(path) {
		return path
	}
	path = filepath.Join(sessionsDir(), newSessionFilename())
	_ = os.MkdirAll(filepath.Dir(path), 0700)
	_ = claimSession(path)
	return path
}

func releaseSessionClaim() {
	if claimedSessionLock != "" {
		_ = os.Remove(claimedSessionLock)
	}
	claimedSessionLock, claimedSessionPath = "", ""
	cancelSessionReservation("")
}

// ---------------------------------------------------------------------------
// Settings persistence (TUI-owned prefs)
//
// Stored as JSON at ~/.config/catalyst-code/settings.json with 0600 perms.
// Atomic write via temp-file + rename so a crash never corrupts the file.
// The API key is stored locally (same trust model as ~/.pi/agent/auth.json);
// the file is mode 0600 and the key is masked in the UI.
// ---------------------------------------------------------------------------

type settingsStore struct {
	path        string
	onSaveError func(error) `json:"-"`
	loadError   error       `json:"-"`
	// migratedSandbox holds the pre-migration sandbox value when a deprecated
	// backend (firejail/seatbelt/...) was found on disk and rewritten to
	// microsandbox, so the UI can surface a one-time deprecation notice. Not
	// persisted (the migrated value is written back on the next save).
	migratedSandbox string `json:"-"`
	APIKey          string `json:"api_key,omitempty"`
	SelectedModel   string `json:"model,omitempty"`
	// Approval is intentionally NOT omitempty — an empty value must not drop the
	// key on save (that looked like a "settings reset" after restart).
	Approval        string `json:"approval"`
	ReasoningEffort string `json:"reasoning_effort,omitempty"`
	Theme           string `json:"theme,omitempty"`
	// AttachDir remembers the last directory opened by the /attach file picker.
	AttachDir     string `json:"attach_dir,omitempty"`
	ThinkExpanded bool   `json:"think_expanded,omitempty"`
	// Production knobs (item 3/7): passed to the core on launch.
	Sandbox          string `json:"sandbox,omitempty"` // none | microsandbox
	NoNetwork        bool   `json:"no_network,omitempty"`
	IdleTimeout      int    `json:"idle_timeout,omitempty"`       // seconds (also written as idle_timeout_secs for core)
	MaxSessionTokens int    `json:"max_session_tokens,omitempty"` // 0=unlimited
	FooterMetrics    bool   `json:"footer_metrics"`               // default on; false hides model/performance footer row
	ReducedMotion    bool   `json:"reduced_motion"`               // disable spring animations (usage bars, etc.)
	// Runtime knobs: also applied live via set_config; persisted so restart keeps them.
	// JSON names match core apply_json so the core reads them from settings.json too.
	BashTimeoutSecs      int    `json:"bash_timeout_secs,omitempty"`
	AutoCompact          bool   `json:"auto_compact"` // not omitempty — default is true; false must survive round-trip
	AdvisorEnabled       bool   `json:"advisor_enabled,omitempty"`
	AdvisorNudge         bool   `json:"advisor_nudge,omitempty"`
	AdvisorSubagents     bool   `json:"advisor_subagents,omitempty"`
	AdvisorModel         string `json:"advisor_model,omitempty"`
	AdvisorSubagentModel string `json:"advisor_subagent_model,omitempty"`

	// Custom providers (openai/anthropic endpoints). ActiveProvider is the
	// provider name selected in the TUI; the core re-applies it on launch via
	// the `set_provider` command. ProviderKeys holds a per-provider API key
	// (keyed by provider name) so each endpoint keeps its own secret; it
	// supersedes the legacy single APIKey when set. Stored in this 0600 file.
	ActiveProvider string            `json:"active_provider,omitempty"`
	ProviderKeys   map[string]string `json:"provider_keys,omitempty"`

	// Custom keybindings (TUI-only). Maps action name → canonical key (the
	// string tea.KeyPressMsg.String() produces). Only user overrides are stored; the
	// full effective map (defaults + overrides) is computed at startup via
	// effectiveKeybinds(). See keybinds.go.
	Keybinds       map[string]string `json:"keybinds,omitempty"`
	RecentCommands []string          `json:"recent_commands,omitempty"`
}

func configDir() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "catalyst-code")
}

// sessionPath returns the JSONL conversation file the core resumes from.
// Sessions are scoped per workspace: each project gets its own directory
// (~/.config/catalyst-code/sessions/<hex(cwd)>) holding an unlimited number of
// session files. On launch we resume the most recently modified one (crash
// recovery / continuity); if none exists we start a fresh timestamped file.
// A legacy flat-layout file (sessions/<hex>.jsonl) is migrated into the dir
// once so existing histories aren't lost.
func sessionPath() string {
	dir := sessionsDir()
	_ = os.MkdirAll(dir, 0700)
	migrateLegacySession(dir)
	entries, err := os.ReadDir(dir)
	if err == nil {
		var best string
		var bestMtime time.Time
		for _, e := range entries {
			if e.IsDir() || !strings.HasSuffix(e.Name(), ".jsonl") {
				continue
			}
			if fi, err := e.Info(); err == nil {
				mt := fi.ModTime()
				if best == "" || mt.After(bestMtime) {
					best, bestMtime = e.Name(), mt
				}
			}
		}
		if best != "" {
			return filepath.Join(dir, best)
		}
	}
	return filepath.Join(dir, newSessionFilename())
}

// sessionsDir is the per-workspace directory holding that project's session
// files. Distinct projects (distinct cwd) get distinct directories.
func sessionsDir() string {
	cwd, err := os.Getwd()
	if err != nil {
		cwd = "."
	}
	if abs, err := filepath.Abs(cwd); err == nil {
		cwd = abs
	}
	return filepath.Join(configDir(), "sessions", fmt.Sprintf("%x", fnv64a(cwd)))
}

// newSessionFilename returns a unique, human-readable timestamped name for a
// new session file (date_time + nanosecond suffix for uniqueness).
func newSessionFilename() string {
	t := time.Now()
	return fmt.Sprintf("%s_%09d.jsonl", t.Format("2006-01-02_15-04-05"), t.Nanosecond())
}

// migrateLegacySession moves a legacy flat-layout file (sessions/<hex>.jsonl)
// into the per-project directory, named by its original mtime. Runs once:
// after the move the legacy path no longer exists.
func migrateLegacySession(dir string) {
	legacy := filepath.Join(filepath.Dir(dir), filepath.Base(dir)+".jsonl")
	info, err := os.Stat(legacy)
	if err != nil || info.IsDir() {
		return
	}
	name := info.ModTime().Format("2006-01-02_15-04-05") + ".jsonl"
	dst := filepath.Join(dir, name)
	if _, err := os.Stat(dst); err == nil {
		dst = filepath.Join(dir, newSessionFilename())
	}
	_ = os.Rename(legacy, dst)
}

// settingsPath is the TUI prefs file (api key, model, theme, ...).
func settingsPath() string {
	return filepath.Join(configDir(), "settings.json")
}

// fnv64a: 64-bit FNV-1a, same algorithm the core uses for line hashes.
func fnv64a(s string) uint64 {
	const offset uint64 = 0xcbf29ce484222325
	const prime uint64 = 0x100000001b3
	h := offset
	for i := 0; i < len(s); i++ {
		h ^= uint64(s[i])
		h *= prime
	}
	return h
}

// loadSettings reads the persisted prefs, returning a zero-value store on any
// error (missing file is not an error — first run).
func loadSettings() *settingsStore {
	return loadSettingsFrom(settingsPath())
}

// loadSettingsFrom is the testable core of loadSettings (path-injectable).
func loadSettingsFrom(path string) *settingsStore {
	s := &settingsStore{
		path:            path,
		ReasoningEffort: "high",
		Approval:        "destructive",
		Sandbox:         "none",
		IdleTimeout:     120,
		BashTimeoutSecs: 30,
		AutoCompact:     true,
		FooterMetrics:   true,
	}
	data, err := os.ReadFile(s.path)
	if err != nil {
		if !os.IsNotExist(err) {
			s.loadError = fmt.Errorf("could not read settings: %w", err)
		}
		return s
	}
	// Keep defaults for fields absent from the file.
	var onDisk settingsStore
	if err := json.Unmarshal(data, &onDisk); err != nil {
		s.loadError = fmt.Errorf("settings file is invalid JSON; defaults are active: %w", err)
		return s
	}
	// Raw map for keys whose Go zero-value collides with a non-zero default
	// (auto_compact defaults true; missing must not become false).
	var raw map[string]any
	_ = json.Unmarshal(data, &raw)
	if onDisk.APIKey != "" {
		s.APIKey = onDisk.APIKey
	}
	if onDisk.SelectedModel != "" {
		s.SelectedModel = onDisk.SelectedModel
	}
	if onDisk.Approval != "" {
		s.Approval = onDisk.Approval
	}
	if onDisk.ReasoningEffort != "" {
		s.ReasoningEffort = onDisk.ReasoningEffort
	}
	if onDisk.Theme != "" {
		s.Theme = onDisk.Theme
	}
	if onDisk.AttachDir != "" {
		s.AttachDir = onDisk.AttachDir
	}
	s.ThinkExpanded = onDisk.ThinkExpanded
	if onDisk.Sandbox != "" {
		norm, deprecated := normalizeSandboxValue(onDisk.Sandbox)
		if norm == "" {
			norm = "none"
		}
		s.Sandbox = norm
		if deprecated {
			s.migratedSandbox = onDisk.Sandbox
		}
	}
	s.NoNetwork = onDisk.NoNetwork
	if onDisk.IdleTimeout > 0 {
		s.IdleTimeout = onDisk.IdleTimeout
	} else if n, ok := jsonNumber(raw["idle_timeout_secs"]); ok && n > 0 {
		// Core-compatible alias written by save(); accept on load for round-trip.
		s.IdleTimeout = n
	}
	s.MaxSessionTokens = onDisk.MaxSessionTokens
	if v, ok := raw["footer_metrics"].(bool); ok {
		s.FooterMetrics = v
	}
	if v, ok := raw["reduced_motion"].(bool); ok {
		s.ReducedMotion = v
	}
	if onDisk.BashTimeoutSecs > 0 {
		s.BashTimeoutSecs = onDisk.BashTimeoutSecs
	}
	if v, ok := raw["auto_compact"].(bool); ok {
		s.AutoCompact = v
	}
	if v, ok := raw["advisor_enabled"].(bool); ok {
		s.AdvisorEnabled = v
	}
	if v, ok := raw["advisor_nudge"].(bool); ok {
		s.AdvisorNudge = v
	}
	if v, ok := raw["advisor_subagents"].(bool); ok {
		s.AdvisorSubagents = v
	}
	s.AdvisorModel = onDisk.AdvisorModel
	s.AdvisorSubagentModel = onDisk.AdvisorSubagentModel
	if onDisk.ActiveProvider != "" {
		s.ActiveProvider = onDisk.ActiveProvider
	}
	s.ProviderKeys = map[string]string{}
	for k, v := range onDisk.ProviderKeys {
		if v != "" {
			s.ProviderKeys[k] = v
		}
	}
	// Custom keybinds: store only the user overrides (non-default bindings).
	// Empty string is a valid override meaning "disabled" (kb returns false),
	// so we keep it rather than falling back to the default.
	// The full effective map is computed in initialSession via effectiveKeybinds().
	if onDisk.Keybinds != nil {
		s.Keybinds = map[string]string{}
		for k, v := range onDisk.Keybinds {
			s.Keybinds[k] = v // keep empty (disabled) values
		}
	}
	if len(onDisk.RecentCommands) > 0 {
		s.RecentCommands = append([]string(nil), onDisk.RecentCommands...)
	}
	return s
}

// jsonNumber coerces a JSON number (float64 from encoding/json) or int-like
// value into an int. Used when reading aliased keys from the raw settings map.
func jsonNumber(v any) (int, bool) {
	switch n := v.(type) {
	case float64:
		return int(n), true
	case int:
		return n, true
	case int64:
		return int(n), true
	case json.Number:
		i, err := n.Int64()
		return int(i), err == nil
	default:
		return 0, false
	}
}

// normalizeApproval returns a valid approval mode, defaulting blank/unknown to
// destructive (same as the core's Approval::parse fallback).
func normalizeApproval(mode string) string {
	switch strings.ToLower(strings.TrimSpace(mode)) {
	case "never", "destructive", "always":
		return strings.ToLower(strings.TrimSpace(mode))
	default:
		return "destructive"
	}
}

// save writes the store atomically with 0600 perms.
//
// Uses a read-merge-write against the on-disk JSON object so we never drop
// keys that this process doesn't know about (forward-compat) and never blank
// out approval if memory somehow lost it mid-session.
func (s *settingsStore) save() (err error) {
	defer func() {
		if err != nil && s.onSaveError != nil {
			s.onSaveError(err)
		}
	}()
	dir := filepath.Dir(s.path)
	if err := os.MkdirAll(dir, 0700); err != nil {
		return err
	}

	// Start from on-disk document (preserve unknown keys), then overlay ours.
	merged := map[string]any{}
	if existing, err := os.ReadFile(s.path); err == nil && len(existing) > 0 {
		_ = json.Unmarshal(existing, &merged)
		if merged == nil {
			merged = map[string]any{}
		}
		// Guard: never let a blank in-memory approval erase a persisted one.
		if strings.TrimSpace(s.Approval) == "" {
			if prev, ok := merged["approval"].(string); ok && strings.TrimSpace(prev) != "" {
				s.Approval = prev
			}
		}
	}
	s.Approval = normalizeApproval(s.Approval)

	// Do not build the overlay by marshaling settingsStore: omitempty would
	// leave the old on-disk value behind precisely when a user clears a string,
	// disables a bool, resets a number, logs out, or removes the last map entry.
	// Keep this list explicit so every known key has replace (not merge) semantics
	// while unknown/newer-core keys above remain untouched.
	overlay := map[string]any{
		"api_key": s.APIKey, "model": s.SelectedModel,
		"approval": s.Approval, "reasoning_effort": s.ReasoningEffort,
		"theme": s.Theme, "think_expanded": s.ThinkExpanded,
		"sandbox": s.Sandbox, "no_network": s.NoNetwork,
		"idle_timeout": s.IdleTimeout, "max_session_tokens": s.MaxSessionTokens,
		"footer_metrics": s.FooterMetrics, "reduced_motion": s.ReducedMotion, "bash_timeout_secs": s.BashTimeoutSecs,
		"advisor_enabled": s.AdvisorEnabled, "advisor_nudge": s.AdvisorNudge, "advisor_subagents": s.AdvisorSubagents,
		"advisor_model": s.AdvisorModel, "advisor_subagent_model": s.AdvisorSubagentModel,
		"auto_compact": s.AutoCompact, "active_provider": s.ActiveProvider,
		"provider_keys":   nonNilStringMap(s.ProviderKeys),
		"keybinds":        nonNilStringMap(s.Keybinds),
		"recent_commands": append([]string(nil), s.RecentCommands...),
	}
	for k, v := range overlay {
		merged[k] = v
	}
	// Mouse interaction is always enabled. Remove the retired opt-in on the
	// next save instead of preserving a setting that no longer has an effect.
	delete(merged, "mouse_wheel")
	// Always persist approval / auto_compact explicitly (bool defaults + omitempty
	// must not drop a deliberate false, and blank approval must not wipe disk).
	merged["approval"] = s.Approval
	merged["auto_compact"] = s.AutoCompact
	merged["footer_metrics"] = s.FooterMetrics
	merged["reduced_motion"] = s.ReducedMotion
	// Core apply_json reads idle_timeout_secs; keep the TUI's idle_timeout too.
	merged["idle_timeout"] = s.IdleTimeout
	merged["idle_timeout_secs"] = s.IdleTimeout
	merged["bash_timeout_secs"] = s.BashTimeoutSecs

	data, err := json.MarshalIndent(merged, "", "  ")
	if err != nil {
		return err
	}
	// Unique temp file (random suffix via os.CreateTemp) in the SAME directory
	// as the target, so two processes saving settings concurrently never
	// share a temp file — a shared temp would interleave writes and rename a
	// corrupted file over settings.json. Atomic rename within one filesystem
	// is preserved (same dir).
	base := filepath.Base(s.path)
	f, err := os.CreateTemp(dir, "."+base+".*.tmp")
	if err != nil {
		return err
	}
	tmp := f.Name()
	if _, err := f.Write(data); err != nil {
		f.Close()
		os.Remove(tmp)
		return err
	}
	if err := f.Sync(); err != nil {
		f.Close()
		os.Remove(tmp)
		return err
	}
	f.Close()
	// 0600: settings.json may hold API keys. CreateTemp already uses 0600 on
	// Unix, but set it explicitly for parity with the original WriteFile path.
	if err := os.Chmod(tmp, 0600); err != nil {
		os.Remove(tmp)
		return err
	}
	if err := os.Rename(tmp, s.path); err != nil {
		os.Remove(tmp)
		return err
	}
	return nil
}

func nonNilStringMap(m map[string]string) map[string]string {
	if m == nil {
		return map[string]string{}
	}
	return m
}
