package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"syscall"
	"time"

	"charm.land/bubbles/v2/progress"
	"charm.land/bubbles/v2/textarea"
	"charm.land/bubbles/v2/viewport"
	tea "charm.land/bubbletea/v2"
)

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

// queuedMsg is the single buffered follow-up/steer prompt shown in a pinned
// banner while a turn runs. kind is "follow-up" (queued via Enter) or "steer"
// (Ctrl+Enter / /steer). Cleared when the queued turn starts, when it is
// dequeued via Esc, or on full abort.
type queuedMsg struct {
	kind string // "follow-up" | "steer"
	text string
	at   time.Time
}

// statusToast is a short-lived footer/composer flash for operational messages
// that should not pollute the transcript.
type statusToast struct {
	kind  toastKind
	text  string
	until time.Time
}

// transcriptPoint is a display-cell coordinate in the fully rendered
// transcript (before the viewport crops it). Mouse selection uses cell
// coordinates because terminal mouse events and Lip Gloss ranges are both
// cell-based, including wide graphemes.
type transcriptPoint struct {
	line int
	col  int
}

// mouseCoord is the single screen-coordinate normalization boundary. Bubble
// Tea v2 documents mouse X/Y as zero-based terminal cells; keeping this
// explicit prevents individual hit-test paths from growing ad-hoc +/-1 fixes.
// CATCODE_MOUSE_Y_BIAS is an opt-in compatibility override for wrappers that
// forward raw one-based rows (normal Bubble Tea events leave it unset).
func mouseCoord(x, y int) (int, int) {
	if os.Getenv("CATCODE_MOUSE_Y_BIAS") == "1" {
		return x, y - 1
	}
	return x, y
}

type transcriptSelection struct {
	active  bool // left button is currently held
	dragged bool // pointer moved away from anchor; distinguishes click from drag
	anchor  transcriptPoint
	head    transcriptPoint
}

type composerDraft struct {
	owner  string
	text   string
	cursor int
	images []string
}

type coreLifecycleState uint8

const (
	coreStarting coreLifecycleState = iota
	coreReady
	coreFailed
)

type streamRefreshMsg struct{}

// busyFrameMsg repaints time-varying chrome while a turn is in flight. Streaming
// content has its own 15 FPS clock; chrome does not need 10 FPS, and a slower
// 4 FPS cadence leaves the single-threaded event loop responsive on slow links.
type busyFrameMsg struct{}

const busyFrameInterval = time.Second / 4

// selectionFrameMsg coalesces the much faster stream of terminal drag events
// into at most one visible selection update per renderer frame.
type selectionFrameMsg struct {
	generation uint64
	modal      bool
}

// Thirty selection frames per second stays visually smooth while leaving the
// event loop enough time for large transcript/layout renders.
const selectionFrameInterval = time.Second / 30

type toastKind int

const (
	toastInfo toastKind = iota
	toastSuccess
	toastWarn
	toastError
)

// oauthBanner holds a sticky OAuth login prompt (URL copied; code if any)
// instead of dumping a hard-wrapped URL wall into the transcript.
type oauthBanner struct {
	message string
	url     string
	code    string
}

// session is the Bubble Tea model: it owns the core subprocess, the structured
// list of message blocks, and the viewport/input/busy-frame clock.
type session struct {
	coreCmd    *exec.Cmd
	coreIn     io.WriteCloser
	coreEvents chan *coreEvent
	stdinCh    chan []byte // P1-15: stdin writes are funneled through a writer goroutine

	// authed is true when the ACTIVE provider has a usable key (footer/status).
	// anyLoggedIn is true when ANY configured provider is logged in — multi-
	// provider sends must not be blocked just because the active/fallback
	// provider is unauthenticated (stale settings, expired OAuth, etc.).
	authed          bool
	anyLoggedIn     bool
	models          []modelInfo
	modelIdx        int
	busy            bool
	queuedNext      bool                         // a follow-up/steer turn is chained after the current one
	queued          *queuedMsg                   // the currently-queued follow-up/steer (drives the pinned banner + Esc-dequeue)
	todos           []map[string]json.RawMessage // latest todo_write state (pinned panel)
	turnCount       int
	pendingApproval *approvalPrompt
	pendingIntercom *intercomPrompt
	intercomQueue   []*intercomPrompt
	intercomNudge   time.Time // pulses a "type a reply" hint when Enter is hit on an empty intercom reply
	pendingAsk      *askPrompt
	pendingSudo     *sudoPrompt
	updateInfo      *updateInfo // non-nil when a newer release is available (drives the top banner)
	lastMetrics     json.RawMessage
	approvalModeStr string
	sessionList     []sessionEntry
	skillsList      []skillInfo // discoverable skills (drives /skill:<name> autocomplete)
	jobStatusRaw    json.RawMessage
	sessionTreeRaw  json.RawMessage
	pluginCommands  []struct {
		Name        string `json:"name"`
		Description string `json:"description"`
		Plugin      string `json:"plugin"`
	} // plugin-declared slash commands (drives /{name} palette + dispatch)
	pluginStatus        string // last plugin_status text (footer); empty = clear
	memoryList          []memoryEntry
	pendingMemoryPicker bool   // open memory picker once list_memory arrives
	pendingPluginPicker bool   // open plugin picker once plugins_list arrives
	pluginPickerMode    string // pluginModeToggle | pluginModeRemove (for plugins_list → modal)
	coreBashTimeout     int
	coreAutoCompact     bool
	// Sandbox runtime state (effective mode reported by the core via the
	// `ready` + sandbox_* events). Distinct from settings.Sandbox (the desired
	// mode): this is what the core actually resolved, surfaced in the status
	// panel so the TUI/web/CLI/model prompt all agree. pendingSandboxEnable is
	// set when the user picks Microsandbox and we are awaiting a preflight
	// reply before persisting (fail-closed: never save "none" on their behalf).
	sandboxStatus            *sandboxStatusSnap
	pendingSandboxEnable     bool
	ctxBreakdown             *contextBreakdown
	usageReport              *usageReport // last /usage reply (provider plan limits)
	usageBars                []progress.Model
	coreRestarts             int
	coreReady                bool // true once the core emitted `ready` (disarms the startup watchdog)
	coreLifecycle            coreLifecycleState
	coreFailure              string
	streamRefreshPending     bool
	splashStartedAt          time.Time          // when the current startup splash began (min-hold clock)
	coreStartGen             uint64             // bumped each startCore; lets a stale watchdog tick ignore a restart
	abortGen                 uint64             // bumped on abort arm/disarm; stale abortTimeoutMsg ignored
	visionModels             map[string]bool    // user-curated vision-capable model ids (drives /vision)
	visionModel              string             // preferred handoff target ("" = cheapest same-provider)
	visionEnabled            bool               // auto handoff (default true / recommended ON)
	pendingVisionPicker      bool               // open the vision picker once the config arrives
	pendingPluginInstallPath string             // path/URL awaiting scope pick (modalPluginInstallScope)
	pluginTrust              *pluginTrustPrompt // active trust modal state (modalPluginTrust)
	marketplaceAccepted      bool
	marketplaceResults       []marketplaceSkill
	marketplaceInstalled     []installedMarketplaceSkill

	// Active model provider (openai/anthropic endpoint). activeProvider is the
	// name the core resolved; providers is the list of configured names for the
	// settings picker; providerHasKey reflects the last provider_changed/ready
	// event (drives re-auth after a switch).
	activeProvider  string
	providerKind    string
	providers       []string
	providerHasKey  bool
	providerPresets []providerPreset
	pendingLogin    string              // preset id awaiting a pasted API key in the /login modal
	customProvider  customProviderDraft // multi-field add-custom-provider form (modalCustomProvider)

	// Goal mode: draft form for the multi-field /goal modal, plus the last
	// goal_state snapshot from the core (drives status + plan-ready review).
	goalDraft          goalDraft
	goalState          *goalStateSnap
	goalPlan           *goalPlanSnap
	goalStepLogged     map[string]string // step_id → fingerprint of last persisted completion card
	goalLastLife       string            // last persisted lifecycle line (dedupe phase+state)
	goalCompleteLogged bool              // dedupe dual goal_state+goal_phase "goal complete"
	goalPanelCollapsed bool              // collapses the pinned goal progress panel to a one-line header

	settings       *settingsStore
	keybinds       map[string]string // effective keymap (defaults + user overrides); see keybinds.go
	modal          modal
	history        []string
	histIdx        int
	recentCommands []string // most recently used slash commands, palette-first
	composerDrafts []composerDraft

	// conversation blocks + streaming/caching state
	blocks                   []*block
	cur                      *block // currently-streaming block (assistant/thinking), or nil
	thinkExpanded            bool   // global reasoning expand state (default collapsed)
	cache                    strings.Builder
	cacheIdx                 int
	cacheLines               int      // line count of s.cache (avoids cache.String() for offsets)
	transcriptBase           string   // rendered transcript without mouse-selection styling
	transcriptPlain          []string // lazily built ANSI-free lines for hit-testing/copy
	selection                transcriptSelection
	selectionMotion          tea.MouseMotionMsg // newest drag event waiting for a selection frame
	selectionPending         bool
	selectionFrameScheduled  bool
	selectionFrameGeneration uint64
	selectionLastFrame       time.Time
	reuseLastView            bool // the current event changed no paintable state
	lastView                 tea.View
	hasLastView              bool
	modalSelection           transcriptSelection // selection over the currently rendered modal canvas
	modalSelectionKind       modalKind
	modalPlain               []string // ANSI-free last-rendered modal overlay, used for hit-testing/copy
	stashedDraft             string   // idle-Esc cleared draft; a second Esc on the empty composer restores it
	modalBoxTop              int      // placed modal box origin in screen cells (mouse hit-testing)
	modalBoxLeft             int
	modalItemRow             map[int]int // custom list modals: box-relative line → filtered item index
	modalPickerFirst         int         // charm pickers: box-relative line of the first item row
	modalPickerRows          int         // charm pickers: clickable item rows on the current page
	modalPressItem           int         // item index under the mouse press (-1 = none)
	modalPressKind           modalKind

	// chromePress is the press-target registry for chrome surfaces (header,
	// banners, shelf, panels, composer, footer). Mirror of the modal press-item
	// pattern: captured on press, activated only by a stationary release on the
	// same target.
	chromePress chromePress

	// overlayPress is the press-target registry for the blocking ask/sudo
	// flyouts (submit/skip, approve/decline, question focus, select cycling).
	// Like chromePress, a stationary release on the same target activates it.
	overlayPress overlayPress

	// Placed ask/sudo flyout box origin + dimensions in screen cells, and the
	// ANSI-free box rows used for hit-testing. Mirrors modalBoxTop/Left: recorded
	// when the overlay renders so clicks map back onto the painted box.
	askBoxTop, askBoxLeft, askBoxW, askBoxH     int
	askBoxRows                                  []string
	sudoBoxTop, sudoBoxLeft, sudoBoxW, sudoBoxH int
	sudoBoxRows                                 []string

	// updating guards the update banner against double-clicks racing two
	// self-updaters (each replaces the running executable).
	updating bool

	// scroll: follow=true keeps the viewport pinned to the newest line (the
	// default). Scrolling up pauses follow so history can be read without the
	// view yanking back down on each new token; a banner offers to re-pin.
	follow            bool
	focusedBlock      int
	welcomeIdx        int                 // welcome-screen example cursor (empty conversation)
	contextTokens     uint64              // live context size from the last metrics event (drives the footer budget)
	lastCachePct      int                 // last completed turn's prefix-cache hit %; shown (with "~") while the next turn is in flight
	tokensSaved       uint64              // cumulative tokens reclaimed by digest + compaction (shown next to "cached" in the footer)
	summaryChars      int                 // character count of the current rolling compaction summary (0 until a summary is produced)
	umansConcUsed     *int64              // live Umans account-wide concurrency in use; nil => not Umans / fetch failed (hide the field)
	umansConcLimit    *int64              // Umans plan concurrency ceiling; nil => unlimited (render ∞); only meaningful when used != nil
	umansConcProvider string              // the Umans provider name the poll is tracking; conc shows only when the selected model routes here
	subProgress       []*subProgressEntry // live subagent runs (drives the progress panel)
	maxTaskRows       int                 // cap on task-panel entries (set by layout() to fit available height)
	activityExpanded  bool                // expands the compact tasks/subagents activity shelf
	activityScroll    int                 // first detail row shown in the focused activity shelf
	cwd               string              // working dir, shown in the header as ~/
	projectName       string              // launch-dir basename; shown in the terminal window title
	titleBell         bool                // attention bell in the window title until the user returns
	viewChrome        *viewChromeCache    // non-nil only during View(); dedupes measure+paint

	// Ephemeral UX state (toasts, sticky OAuth, one-shot prompts).
	toast              *statusToast
	oauth              *oauthBanner
	loginOffered       bool // auto-opened /login once when ready with no key
	ctrlCAbortArmed    bool // first Ctrl+C while busy aborted; second quits
	intentionalRestart bool // next core EOF should restart (settings apply)

	// @-mention file flyout state (see mention.go): active when an
	// unbroken @-token sits at the cursor; mentionAt is its rune index.
	mentionActive bool
	mentionItems  []mentionItem
	mentionCursor int
	mentionScroll int
	mentionAt     int

	// pendingImages are staged image attachments (absolute paths or data
	// URLs) from paste / clipboard / drag-drop. Merged into the next send via
	// withImages and cleared after the turn is dispatched. Shown as chips in
	// the input box so image paste works over SSH / VS Code (path or base64
	// arrives via bracketed paste) as well as local clipboard grab.
	pendingImages []string

	viewport        viewport.Model
	input           textarea.Model
	busyFrameActive bool // ~10 FPS re-render clock while busy/starting (stopped when idle)

	width, height int
	ready         bool
}

func initialSession() *session {
	s := &session{}

	s.settings = loadSettings()
	s.settings.onSaveError = func(err error) {
		s.logError("setting applied for this session but could not be saved: " + err.Error())
	}
	if s.settings.loadError != nil {
		s.logError(s.settings.loadError.Error())
	}
	if v := s.settings.migratedSandbox; v != "" {
		s.logInfo(fmt.Sprintf("sandbox setting %q migrated to microsandbox (firejail/seatbelt backends were removed)", v))
	}
	s.keybinds = effectiveKeybinds(s.settings.Keybinds)
	s.recentCommands = append([]string(nil), s.settings.RecentCommands...)
	// Seed the status-line approval from settings immediately so we don't flash
	// the "destructive" default before the core's ready event arrives (and so a
	// pre-ready /approval picker doesn't overwrite the saved mode).
	s.approvalModeStr = normalizeApproval(s.settings.Approval)
	if s.settings.Theme != "" {
		setTheme(s.settings.Theme)
	}
	s.thinkExpanded = s.settings.ThinkExpanded
	s.follow = true // pin viewport to newest line until the user scrolls up
	s.focusedBlock = -1
	s.cwd = cwdDisplay()
	s.projectName = projectName()
	s.maxTaskRows = 4 // cap on task-panel entries; layout() shrinks it to fit available height
	// Seed runtime knobs from settings so the UI shows persisted values before
	// the core's ready event (and so a pre-ready edit doesn't fight defaults).
	s.coreBashTimeout = s.settings.BashTimeoutSecs
	if s.coreBashTimeout <= 0 {
		s.coreBashTimeout = 30
	}
	s.coreAutoCompact = s.settings.AutoCompact
	s.visionModels = map[string]bool{}
	s.visionEnabled = true

	s.input = newComposer()

	s.viewport = viewport.New(viewport.WithWidth(80), viewport.WithHeight(20))
	s.viewport.SetContent("")

	return s
}

func busyFrameTick() tea.Cmd {
	return tea.Tick(busyFrameInterval, func(time.Time) tea.Msg { return busyFrameMsg{} })
}

// ---------------------------------------------------------------------------
// Core subprocess lifecycle
// ---------------------------------------------------------------------------

// coreExeSuffix returns the platform executable suffix (".exe" on Windows,
// "" elsewhere) so the installed Windows layout (catcode.exe + catcode-core.exe)
// is discovered correctly.
func coreExeSuffix() string {
	if runtime.GOOS == "windows" {
		return ".exe"
	}
	return ""
}

// coreBinaryPath resolves the core subprocess binary. Search order:
//  1. $CATCODE_CORE — explicit override (used as-is if it exists)
//  2. <dir of this exe>/catcode-core[.exe] — installed layout (beside the TUI)
//  3. Development-only CWD/repository fallbacks (when coreVersion == "dev")
//  4. catcode-core on PATH
//
// On Windows ".exe" is appended to every candidate so the install layout
// (catcode.exe next to catcode-core.exe) is found from any CWD.
func coreBinaryPath() string {
	if env := os.Getenv("CATCODE_CORE"); env != "" {
		// An explicit override is authoritative even when invalid; falling back
		// could launch a different installed core and hide the configuration
		// mistake. startCore will surface the exact path on its recovery screen.
		if abs, err := filepath.Abs(env); err == nil {
			return abs
		}
		return env
	}
	if p := embeddedCorePath(); p != "" {
		return p
	}
	sfx := coreExeSuffix()
	coreName := "catcode-core" + sfx // installed beside the TUI
	devName := "core" + sfx          // cargo's bin name in the dev build
	var candidates []string
	if coreVersion == "dev" {
		// Prefer a repository build over an older globally-installed core when
		// launching the development TUI. An explicit CATCODE_CORE above still
		// wins, so production/service configurations remain authoritative.
		candidates = append(candidates,
			"core/target/release/"+devName,
			"../core/target/release/"+devName,
		)
		if exe, err := os.Executable(); err == nil {
			dir := filepath.Dir(exe)
			candidates = append(candidates,
				filepath.Join(dir, devName),
				filepath.Join(dir, "../core/target/release/"+devName),
			)
		}
	}
	if exe, err := os.Executable(); err == nil {
		dir := filepath.Dir(exe)
		installed := filepath.Join(dir, coreName)
		if _, err := os.Stat(installed); err == nil {
			return installed
		}
	}
	for _, c := range candidates {
		if _, err := os.Stat(c); err == nil {
			if abs, err := filepath.Abs(c); err == nil {
				return abs
			}
			return c
		}
	}
	if p, err := exec.LookPath(coreName); err == nil {
		return p
	}
	// Let exec.Command report a useful PATH error on release builds. Local dev
	// builds retain the historical repo-relative error path for diagnostics.
	if coreVersion == "dev" {
		return "core/target/release/" + devName
	}
	return coreName
}

// coreProcess holds the running core's *os.Process so a signal handler can
// kill it on SIGHUP/SIGTERM — otherwise closing the terminal (SIGHUP) or `kill`
// (SIGTERM) kills the TUI but orphans catcode-core, which keeps running.
// Set in startCore after cmd.Start() (UI thread); read from the signal-handler
// goroutine. An atomic.Pointer is used because the field is shared across
// goroutines — a plain var would be a data race.
var coreProcess atomic.Pointer[os.Process]

// quitting is set by the signal handler / quit key before killing the core, so
// the coreEOFMsg auto-restart path doesn't spawn a fresh core while the TUI is
// tearing down (a killed core's stdout EOF would otherwise look like a crash).
var quitting atomic.Bool

func (s *session) startCore() tea.Cmd {
	// Reset startup-tracking state and arm a fresh watchdog generation so a stale
	// watchdog tick from a previous (crashed) core is ignored once `ready` lands.
	s.coreReady = false
	s.coreLifecycle = coreStarting
	s.coreFailure = ""
	s.splashStartedAt = time.Now()
	s.coreStartGen++
	gen := s.coreStartGen
	bin := coreBinaryPath()
	approval := normalizeApproval(s.settings.Approval)
	s.settings.Approval = approval
	debugLog := filepath.Join(configDir(), "debug.jsonl")
	sessionFile := claimedSessionPath
	if sessionFile == "" {
		sessionFile = sessionPath()
	}
	args := []string{
		"--workspace", ".",
		"--approval", approval,
		"--session", sessionFile,
		"--debug-log", debugLog,
		"--idle-timeout", fmt.Sprintf("%d", s.settings.IdleTimeout),
	}
	// `catcode --debug` (or CATALYST_CODE_DEBUG=1) asks the core for full
	// verbosity: HTTP bodies, tool args/outputs, protocol command/event mirror.
	if debugMode || os.Getenv("CATALYST_CODE_DEBUG") == "1" || strings.EqualFold(os.Getenv("CATALYST_CODE_DEBUG"), "true") {
		args = append(args, "--debug")
	}
	if s.settings.BashTimeoutSecs > 0 {
		args = append(args, "--bash-timeout", fmt.Sprintf("%d", s.settings.BashTimeoutSecs))
	}
	if s.settings.Sandbox != "" && s.settings.Sandbox != "none" {
		args = append(args, "--sandbox", s.settings.Sandbox)
	}
	if s.settings.NoNetwork {
		args = append(args, "--no-network")
	}
	if s.settings.MaxSessionTokens > 0 {
		args = append(args, "--max-session-tokens", fmt.Sprintf("%d", s.settings.MaxSessionTokens))
	}
	cmd := exec.Command(bin, args...)
	cmd.Dir, _ = os.Getwd()
	// P1-14: don't mutate UI state (logError) from this cmd goroutine on the
	// error paths — return a message and let Update log it on the UI thread.
	in, err := cmd.StdinPipe()
	if err != nil {
		return func() tea.Msg {
			return coreStartErrorMsg{err: fmt.Errorf("failed to open core stdin: %s", err), gen: gen}
		}
	}
	out, err := cmd.StdoutPipe()
	if err != nil {
		return func() tea.Msg {
			return coreStartErrorMsg{err: fmt.Errorf("failed to open core stdout: %s", err), gen: gen}
		}
	}
	// P2: route the core's stderr (panic backtraces, unexpected warnings) to the
	// debug log instead of the terminal — under the alt-screen TUI, raw stderr is
	// buffered/lost and garbles the screen. The core appends its structured logs
	// to the same file, so this is safe (append, no truncate race).
	if f, ferr := os.OpenFile(debugLog, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600); ferr == nil {
		cmd.Stderr = f
		defer f.Close() // child inherits the fd after Start; close our copy
	} else {
		cmd.Stderr = os.Stderr
	}
	if err := cmd.Start(); err != nil {
		return func() tea.Msg {
			return coreStartErrorMsg{err: fmt.Errorf("failed to start core (%s): %s", bin, err), gen: gen}
		}
	}
	s.coreCmd = cmd
	coreProcess.Store(cmd.Process) // expose to the signal handler (M8): kill on SIGHUP/SIGTERM
	s.coreIn = in
	eventCh := make(chan *coreEvent, 256)
	s.coreEvents = eventCh
	s.stdinCh = make(chan []byte, 256)

	// P1-15: a dedicated stdin-writer goroutine funnels commands to the core. A
	// blocking pipe write (core not draining) happens here, off the UI thread,
	// so a wedged core can never freeze the Bubble Tea Update loop. The locals
	// (in/ch) are captured once so the writer never reads the shared s.coreIn /
	// s.stdinCh fields, which the restart path nils concurrently — avoids a data
	// race / nil-deref when the core is restarted mid-write.
	ch := s.stdinCh
	go func() {
		for b := range ch {
			if _, err := in.Write(b); err != nil {
				return // core died; the stdout EOF will trigger restart
			}
		}
	}()

	// P1-11: bufio.Reader.ReadString grows with the line (no 4 MiB cap like
	// bufio.Scanner), so a large tool_result JSON line doesn't silently kill the
	// stream. P1-10: the send is BLOCKING (backpressure) so a `done` or
	// `approval_request` is never silently dropped — the core's stdout pipe fills
	// and naturally throttles the core instead.
	go func() {
		r := bufio.NewReaderSize(out, 64*1024)
		for {
			line, err := r.ReadString('\n')
			line = strings.TrimSpace(line)
			if line != "" {
				// Parse once into fields; keep a copy of the line as Raw for
				// handlers that still Unmarshal Raw (D-004).
				var ev coreEvent
				rawCopy := append(json.RawMessage(nil), line...)
				var m map[string]json.RawMessage
				if json.Unmarshal(rawCopy, &m) == nil {
					ev.Raw = rawCopy
					ev.fields = m
					if t, ok := m["type"]; ok {
						var typ string
						if json.Unmarshal(t, &typ) == nil {
							ev.Type = typ
						} else {
							ev.Type = strings.TrimSpace(string(t))
						}
					}
				}
				// Blocking send preserves backpressure (a `done` or
				// approval_request is never silently dropped). But when the read
				// also returned an error (io.EOF at core exit), the UI may have
				// stopped draining — a blocking send here would wedge the reader
				// forever, skipping cmd.Wait()/close() and leaking the core as a
				// zombie. Drop the final line non-blockingly on error so we reach
				// the break and reap the child.
				if err != nil {
					select {
					case eventCh <- &ev:
					default:
					}
				} else {
					eventCh <- &ev
				}
			}
			if err != nil {
				if err != io.EOF {
					// Surface a real read error instead of a silent clean-looking EOF.
					ev := &coreEvent{Type: "error"}
					msg, _ := json.Marshal(map[string]string{"message": "core stdout read error: " + err.Error()})
					ev.Raw = msg
					select {
					case eventCh <- ev:
					default:
					}
				}
				break
			}
		}
		// Reap the core child process exactly once, after the stdout pipe has
		// been fully read. Per os/exec: Wait must not be called before the pipe
		// drain completes — the reader just hit EOF, so the process has exited
		// and Wait only collects its status. This prevents the core from lingering
		// as a zombie across the crash auto-restart (which spawns a fresh core
		// each time) and on Ctrl+C quit. Uses the local cmd (not s.coreCmd, which
		// is nilled during restart) to avoid racing the recovery handler.
		_ = cmd.Wait()
		close(eventCh)
	}()

	s.sendCore(map[string]any{
		"type":             "init",
		"protocol_version": 2,
		"client": map[string]any{
			"name":         "catcode-tui",
			"version":      coreVersion,
			"capabilities": []string{"run_ids", "session_ids", "event_sequence"},
		},
	})
	// Hold the branded splash for at least splashMinHold so a fast `ready` does
	// not skip the animation entirely (local rebuilds often finish in <100ms).
	hold := s.splashMinHoldDuration()
	var splashHoldCmd tea.Cmd
	if hold > 0 {
		splashHoldCmd = tea.Tick(hold, func(time.Time) tea.Msg {
			return splashHoldDoneMsg{gen: gen}
		})
	}
	// Arm a startup watchdog: if the core starts but never emits `ready` within
	// coreStartupTimeout (e.g. a bad UMANS_CORE path or a config that panics),
	// surface a clear error instead of spinning "starting core…" forever. The
	// tick carries the generation captured above so a tick from a previous
	// (crashed+restarted) core is ignored once `ready` disarms it.
	cmds := []tea.Cmd{
		waitForEvent(eventCh, gen),
		tea.Tick(coreStartupTimeout, func(time.Time) tea.Msg { return readyTimeoutMsg{gen: gen} }),
	}
	if splashHoldCmd != nil {
		cmds = append(cmds, splashHoldCmd)
	}
	return tea.Batch(cmds...)
}

// coreStartErrorMsg reports a core subprocess start failure (P1-14: logged on
// the UI thread, not from the startCore goroutine).
type coreStartErrorMsg struct {
	err error
	gen uint64
}

// coreStartupTimeout is how long startCore's watchdog waits for a `ready` event
// before declaring the core failed to start.
const coreStartupTimeout = 30 * time.Second

// readyTimeoutMsg is delivered by the startup watchdog when the core has not
// emitted `ready` within coreStartupTimeout. gen ties it to a specific start so
// a tick from a previous (restarted) core is ignored.
type readyTimeoutMsg struct{ gen uint64 }

// abortBusyTimeout is how long we wait after the user aborts before locally
// clearing busy if core never emits aborted/done (wedged provider / dead pipe).
const abortBusyTimeout = 15 * time.Second

// abortTimeoutMsg fires when an abort watchdog expires without a terminal event.
type abortTimeoutMsg struct{ gen uint64 }

// sigtermMsg is sent by the SIGHUP/SIGTERM handler so Bubble Tea restores the
// terminal (alt-screen / raw-mode) via its normal tea.Quit path instead of a
// raw os.Exit that would leave the terminal broken.
type sigtermMsg struct{}

func waitForEvent(ch <-chan *coreEvent, gen uint64) tea.Cmd {
	return func() tea.Msg {
		ev, ok := <-ch
		if !ok {
			return coreEOFMsg{gen: gen}
		}
		return coreEventMsg{event: ev, gen: gen}
	}
}

// sendCore enqueues a command and reports whether the UI may commit its
// optimistic state transition. Callers that clear user input or enter busy
// state must check the return value.
func (s *session) sendCore(m map[string]any) bool {
	if s.coreIn == nil {
		// Pre-start/test models historically use a nil writer. Real failed-core
		// input is intercepted by handleKey before dispatch; treating this as an
		// accepted no-op keeps pure state-machine tests deterministic.
		return s.coreLifecycle != coreFailed
	}
	b, _ := json.Marshal(m)
	b = append(b, '\n')
	// P1-15: hand the bytes to the stdin-writer goroutine instead of a direct
	// (possibly blocking) pipe write from the UI thread. Non-blocking send with a
	// drop+log on overflow so a wedged core can never freeze Update; in practice
	// the buffer never fills (commands are human-paced and the writer drains to
	// the pipe as fast as the core accepts).
	if s.stdinCh != nil {
		select {
		case s.stdinCh <- b:
			return true
		default:
			s.logError("core not accepting input (backpressure); command dropped")
			return false
		}
	}
	_, err := s.coreIn.Write(b) // legacy path before startCore wires stdinCh
	if err != nil {
		s.logError("core write failed: " + err.Error())
		return false
	}
	return true
}

// ---------------------------------------------------------------------------
// Tea Model methods
// ---------------------------------------------------------------------------

func (s *session) Init() tea.Cmd {
	// Arm the busy-frame clock immediately so the startup splash animates from
	// the first paint (before tickMsg's 1s re-arm path can notice needsBusyFrames).
	s.busyFrameActive = true
	// Fresh launch: no stale attention bell from a previous session state.
	s.titleBell = false
	return tea.Batch(s.startCore(), tick(), busyFrameTick())
}

func tick() tea.Cmd {
	return tea.Tick(time.Second, func(t time.Time) tea.Msg { return tickMsg{t} })
}

// resetCoreUIState clears turn/queue/modal state that must not survive a core
// restart (crash or intentional). Also closes the old stdin writer channel.
func (s *session) resetCoreUIState() {
	// P1-13: reset stale turn/UI state so the restarted core isn't shown as
	// "working" with a dead request_id the user could accidentally approve.
	s.disarmAbortTimeout()
	s.busy = false
	s.cur = nil
	s.queuedNext = false
	s.pendingApproval = nil
	s.pendingIntercom = nil
	s.intercomQueue = nil
	s.subProgress = nil
	s.queued = nil
	s.todos = nil
	s.pendingAsk = nil
	s.pendingSudo = nil
	s.lastMetrics = nil
	s.lastCachePct = 0
	s.tokensSaved = 0
	s.umansConcUsed = nil
	s.umansConcLimit = nil
	s.oauth = nil
	s.chromePress = chromePress{}
	s.overlayPress = overlayPress{}
	s.askBoxRows = nil
	s.sudoBoxRows = nil
	s.ctrlCAbortArmed = false
	s.titleBell = false
	s.restoreAllComposerDrafts()
	if s.modal.kind != modalNone {
		s.closeModal()
	}
	// Stop the old stdin writer (its range loop exits on close) and drop the
	// dead pipes before respawning.
	if s.stdinCh != nil {
		close(s.stdinCh)
		s.stdinCh = nil
	}
	s.coreCmd = nil
	s.coreIn = nil
}

// setToast shows a short-lived status flash above the composer.
func (s *session) setToast(kind toastKind, text string) {
	text = strings.TrimSpace(text)
	if text == "" {
		return
	}
	// Cap toast length so a multi-line dump doesn't blow the layout.
	if len(text) > 240 {
		text = text[:237] + "…"
	}
	// Priority guard (kinds are iota-ordered info<success<warn<error): while a
	// warn/error toast is still fresh, lower-priority chatter ("draft
	// restored", "copied") must not erase the error the user needs to see.
	// Same-or-higher priority always replaces; anything replaces an expired one.
	if s.toast != nil && time.Now().Before(s.toast.until) && kind < s.toast.kind {
		return
	}
	s.toast = &statusToast{kind: kind, text: text, until: time.Now().Add(4 * time.Second)}
}

// largeDraftWarnThreshold is the composer size at which a paste stops being
// silent: pasting a whole log/base64 blob and hitting Enter would otherwise
// send it all to the model with zero signal.
const largeDraftWarnThreshold = 100 * 1024

// warnIfLargeDraft flashes an actionable warning when a paste just pushed the
// composer past largeDraftWarnThreshold. Enter still sends (no blocking) —
// the point is the user knows what they're about to send, and the
// Esc-clears-draft affordance is the way out.
func (s *session) warnIfLargeDraft() {
	n := len(s.input.Value())
	if n < largeDraftWarnThreshold {
		return
	}
	s.logWarn("large draft (" + humanByteCount(n) + ") — Enter sends it all · Esc clears")
}

// humanByteCount formats a byte count compactly ("100 KB", "1.4 MB").
func humanByteCount(n int) string {
	if n >= 1<<20 {
		return fmt.Sprintf("%.1f MB", float64(n)/(1<<20))
	}
	return fmt.Sprintf("%d KB", n/1024)
}

// requestCoreRestart kills the core so coreEOFMsg respawns it with new
// launch-only settings (sandbox, idle-timeout, …).
func (s *session) requestCoreRestart() tea.Cmd {
	s.intentionalRestart = true
	s.closeModal()
	s.logInfo("restarting core to apply settings…")
	if s.coreCmd != nil && s.coreCmd.Process != nil {
		_ = s.coreCmd.Process.Kill()
	} else if p := coreProcess.Load(); p != nil {
		_ = p.Kill()
	}
	return nil
}

func (s *session) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		s.width = msg.Width
		s.height = msg.Height
		s.ready = true
		s.layout()
		return s, nil

	case tickMsg:
		// Refresh the transcript while there's live content (a streaming block or
		// an in-flight tool) so the in-flight badge `◷` and its elapsed timer tick.
		// Finalized blocks are cached, so this is O(live) when idle it's a no-op.
		// While a blocking input flyout (ask/sudo/approval) is open the agent is
		// paused on the user, so the transcript cannot change — skip the per-tick
		// rebuild (renderBlocks is expensive on long sessions). Without this the
		// 1Hz refresh churn starves typing in the flyout.
		if s.hasLiveContent() && !s.blockingInputOpen() {
			s.refresh()
		}
		// Drop expired toasts so the rail clears without waiting for another key.
		if s.toast != nil && time.Now().After(s.toast.until) {
			s.toast = nil
		}
		cmds := []tea.Cmd{tick()}
		// (Re)start the busy-frame clock if a turn is in flight but the cycle
		// has stopped. The cycle stops when idle (see busyFrameMsg) to avoid a
		// ~10x/sec re-render storm that disrupts mouse text selection (copy);
		// tickMsg (every 500ms) catches the busy transition and restarts it.
		// Don't re-arm the busy-frame clock while a blocking input flyout owns
		// the keyboard (see busyFrameMsg): the agent is paused on the user, so
		// the ~10×/s re-render only starves typing. Resumes within ~1s of close.
		if !s.blockingInputOpen() && s.needsBusyFrames() && !s.busyFrameActive {
			s.busyFrameActive = true
			cmds = append(cmds, busyFrameTick())
		}
		return s, tea.Batch(cmds...)

	case busyFrameMsg:
		// Only keep framing while a turn runs, the core is still starting (splash
		// animation), or we're pre-layout. When idle, stop so the renderer isn't
		// driven ~10x/sec — that constant re-render makes mouse text selection
		// (copy) impossible. tickMsg restarts the cycle when activity resumes.
		// While a blocking input flyout (ask/sudo/approval) is open the agent is
		// paused on the user; pause the ~10Hz re-render too, otherwise it saturates
		// the single-threaded loop and typing in the flyout lags badly (seconds
		// per keystroke on long transcripts). tickMsg restarts the cycle within
		// ~1s after the flyout closes (s.busy && !busyFrameActive).
		if s.blockingInputOpen() {
			s.busyFrameActive = false
			return s, nil
		}
		if s.needsBusyFrames() {
			s.busyFrameActive = true
			// The startup splash lives in the viewport (not chrome), so each frame
			// must refresh transcript content for the time-based phase to advance.
			if s.splashAnimatesInViewport() {
				s.refresh()
			}
			return s, busyFrameTick()
		}
		s.busyFrameActive = false
		return s, nil

	case progress.FrameMsg:
		if s.modal.kind != modalUsage || len(s.usageBars) == 0 {
			return s, nil
		}
		var cmds []tea.Cmd
		for i := range s.usageBars {
			var cmd tea.Cmd
			s.usageBars[i], cmd = s.usageBars[i].Update(msg)
			if cmd != nil {
				cmds = append(cmds, cmd)
			}
		}
		if len(cmds) == 0 {
			return s, nil
		}
		return s, tea.Batch(cmds...)

	case coreStartErrorMsg:
		if msg.gen != s.coreStartGen {
			return s, nil
		}
		// P1-14: a core start failure is logged on the UI thread (startCore ran in
		// a goroutine and must not touch UI state itself).
		s.coreLifecycle = coreFailed
		s.coreFailure = msg.err.Error()
		s.busy = false
		s.clearBlockingPrompts()
		s.logError(s.coreFailure)
		return s, nil

	case streamRefreshMsg:
		s.streamRefreshPending = false
		// Skip the transcript rebuild while a keyboard-intercepting overlay
		// (ask/sudo/approval flyout or any modal) is open: it overlays the
		// transcript, so rebuilding it only starves the typing loop. The next
		// tickMsg (or the next delta after the overlay closes) refreshes.
		if !s.blockingInputOpen() {
			s.refresh()
		}
		return s, nil

	case mentionSearchMsg:
		// A background repository walk completed. Re-evaluate the token and
		// repaint immediately instead of waiting for another keystroke.
		return s, s.handleMentionSearchMsg(msg)

	case updateAvailableMsg:
		// A newer GitHub release was found by the launch-time check. Store it so
		// renderUpdateBanner shows a one-line notice; re-layout to claim the line.
		s.updateInfo = &msg.info
		s.layout()
		return s, nil
	case updateExecMsg:
		s.updating = false
		if msg.err != nil {
			s.setToast(toastError, "update failed: "+msg.err.Error())
		} else {
			s.setToast(toastSuccess, "update complete; restart catcode to use it")
			s.updateInfo = nil
			s.layout()
		}
		return s, nil

	case splashHoldDoneMsg:
		// Minimum splash hold elapsed. If the core is already ready the viewport
		// may still be showing the brand panel — rebuild so the real welcome
		// (or resumed transcript) takes over. Ignore stale gens from restarts.
		if msg.gen != s.coreStartGen {
			return s, nil
		}
		if s.coreLifecycle == coreReady && !s.hasConversation() {
			s.layout()
		}
		return s, nil

	case readyTimeoutMsg:
		// The startup watchdog fired. Ignore if `ready` already arrived or this
		// tick belongs to a previous (restarted) core; otherwise the core never
		// came up — surface a clear error instead of spinning forever.
		if s.coreReady || msg.gen != s.coreStartGen {
			return s, nil
		}
		s.coreLifecycle = coreFailed
		s.coreFailure = "core did not start within 30s — check CATCODE_CORE or the debug log"
		s.busy = false
		s.clearBlockingPrompts()
		s.logError(s.coreFailure)
		// A process that never completes its handshake is not useful and must not
		// be left orphaned while the recovery screen waits for Retry.
		if s.coreCmd != nil && s.coreCmd.Process != nil {
			_ = s.coreCmd.Process.Kill()
		}
		return s, nil

	case abortTimeoutMsg:
		// User aborted but core never emitted aborted/done. Release the footer so
		// "working" cannot stick forever on a wedged provider or dead event pump.
		if msg.gen != s.abortGen || !s.busy {
			return s, nil
		}
		s.busy = false
		s.queuedNext = false
		s.queued = nil
		s.clearBlockingPrompts()
		s.cur = nil
		s.finalizeInFlight("[aborted — no response from core]")
		s.layout()
		s.logWarn("abort timed out — cleared working state (core may still be wedged)")
		s.input.Focus()
		return s, nil

	case sudoTimeoutMsg:
		// The 30s auto-close timer fired. If the sudo flyout is still open for
		// the same request, auto-decline so the agent isn't blocked forever.
		if s.pendingSudo != nil && s.pendingSudo.requestID == msg.requestID {
			s.sendSudoReply(s.pendingSudo, false)
			s.logWarn("⊘ sudo request auto-declined (30s timeout)")
		}
		return s, nil

	case sigtermMsg:
		// SIGHUP/SIGTERM: restore the terminal via the normal quit path (the
		// signal goroutine already killed the core; the reader reaps it).
		s.clearPendingImages()
		return s, tea.Quit

	case coreEOFMsg:
		if msg.gen != s.coreStartGen {
			return s, nil
		}
		// A signal-driven teardown (SIGHUP/SIGTERM) or the quit key killed the
		// core; the reader then reports EOF. Don't auto-restart — we're quitting.
		if quitting.Load() {
			return s, tea.Quit
		}
		if s.coreLifecycle == coreFailed {
			return s, nil
		}
		// Intentional restart (sandbox / idle-timeout / etc. apply on next launch):
		// restart without burning the crash-retry budget.
		if s.intentionalRestart {
			s.intentionalRestart = false
			s.resetCoreUIState()
			s.logInfo("core restarted — new settings applied")
			return s, s.startCore()
		}
		// Core crashed or exited unexpectedly. Auto-restart once so the user
		// isn't stranded, and re-auth with the persisted key if we had one.
		// P1-17: coreRestarts is reset to 0 after every successful turn (see the
		// `done` handler), so this budget is per-incident, not lifetime — an early
		// crash doesn't burn the only retry forever.
		if s.coreRestarts >= 1 {
			s.resetCoreUIState()
			s.coreLifecycle = coreFailed
			s.coreFailure = "core exited again after automatic restart"
			s.logError(s.coreFailure + " — press r to retry or q to quit")
			s.layout()
			return s, nil
		}
		s.coreRestarts++
		s.logWarn("core exited; restarting…")
		s.resetCoreUIState()
		return s, s.startCore()

	case coreEventMsg:
		if msg.gen != s.coreStartGen {
			return s, nil
		}
		return s, s.handleCoreEvent(msg.event)

	case tea.KeyPressMsg:
		model, cmd := s.handleKey(msg)
		return model, cmd

	case tea.MouseWheelMsg:
		return s, s.handleMouseWheel(msg)

	case tea.MouseClickMsg:
		return s, s.handleTranscriptMouseClick(msg)

	case tea.MouseMotionMsg:
		return s, s.handleTranscriptMouseMotion(msg)

	case tea.MouseReleaseMsg:
		return s, s.handleTranscriptMouseRelease(msg)

	case selectionFrameMsg:
		if msg.modal {
			return s, s.handleModalSelectionFrame(msg)
		}
		return s, s.handleTranscriptSelectionFrame(msg)

	case tea.FocusMsg:
		// The terminal window regained focus (ReportFocus in View) — the user is
		// looking at catcode again, so silence the window-title attention bell.
		s.titleBell = false
		return s, nil

	case tea.BlurMsg:
		// Focus-loss carries no UI state change; consume it so the default case
		// can't forward it into the ask flyout / picker list.
		return s, nil

	case tea.PasteMsg:
		// v2 enables bracketed-paste by default, so every paste arrives as a
		// PasteMsg (v1 delivered paste differently and this case was absent, which
		// silently dropped all pastes after the migration). Route the pasted text
		// to whichever input owns the keys right now — mirroring the key dispatch
		// in handleKey — so paste works in the chat box, the ask flyout, and modal
		// edit fields (e.g. pasting an API key). textinput v2 inserts the text.
		//
		// Chat-box paste also inspects the payload for images (path / file:// /
		// data URL / raw base64 / binary magic). Over SSH and VS Code Remote,
		// clipboard image tools on the remote host are empty — the only reliable
		// path is bracketed-paste of a remote file path or image data injected by
		// the local terminal / a VS Code extension. When we detect an image we
		// stage it as a pending attachment instead of dumping base64 into the box.
		switch {
		case s.modal.kind != modalNone:
			// A modal always owns input. Filter/edit-capable modals accept the paste;
			// other dialogs consume it so nothing leaks into the hidden composer.
			_ = s.appendModalPaste(msg.Content)
			return s, nil
		case s.pendingSudo != nil:
			s.pendingSudo.input, _ = s.pendingSudo.input.Update(msg)
		case s.pendingAsk != nil:
			s.pumpHuhAsk(msg)
		default:
			res := s.handlePasteContent(msg.Content)
			if res.consumed {
				// Image-only paste: chips update the input box; nothing to insert.
				return s, nil
			}
			if len(res.attached) > 0 && res.text != "" {
				// Mixed paste: images staged, residual text still goes in.
				s.input, _ = s.input.Update(tea.PasteMsg{Content: res.text})
				s.warnIfLargeDraft()
				return s, nil
			}
			s.input, _ = s.input.Update(msg)
			s.warnIfLargeDraft()
		}
		return s, nil

	case clipboardImageMsg:
		// Async result of paste_image keybind (local clipboard grab).
		if msg.err != nil {
			s.logError(msg.err.Error())
			return s, nil
		}
		if msg.path != "" && s.addPendingImage(msg.path) {
			s.logSuccess(fmt.Sprintf("attached image → %s", filepath.Base(msg.path)))
		}
		return s, nil

	default:
		// While the ask flyout is open, forward unhandled msgs (e.g. the cursor
		// BlinkMsg resolved from the cmd handleAskKey returned) back into the ask
		// form so the blink chain re-arms; the form's resulting cmd is returned
		// for the runtime to run asynchronously. Without this the cursor blinks
		// once then freezes.
		if s.pendingAsk != nil {
			return s, s.pumpHuhAsk(msg)
		}
		// Charm picker lists (command/models/sessions/theme) filter asynchronously:
		// typing while "/"-filtering returns a filterItems cmd → list.FilterMatchesMsg.
		// Forward those (and other list-owned msgs) into pickerList; dropping them
		// here leaves filteredItems stale so the visible list never shrinks.
		if s.usesCharmPickerList() {
			return s, s.handlePickerListMsg(msg)
		}
		// In v2, enhanced keyboard protocols (Kitty progressive keyboard + xterm
		// modifyOtherKeys) are auto-enabled by the renderer and every modified key
		// — Shift/Ctrl+Enter, Esc, Ctrl+letter — arrives as a real KeyPressMsg
		// dispatched by the case above.
	}
	return s, nil
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

func main() {
	// CLI flags (--update / --check-update / --version / --help) are handled
	// before the TUI starts so they run in a plain terminal (no alt-screen) and
	// can exit cleanly.
	if code, handled := handleCLIArgs(os.Args[1:]); handled {
		os.Exit(code)
	}

	// v2 is declarative: alt-screen, mouse mode, and enhanced keyboard protocols
	// are set as fields on the View returned by View() rather than program options,
	// so NewProgram takes only the model. The renderer auto-enables the Kitty
	// progressive-keyboard + xterm modifyOtherKeys protocols (and restores the
	// terminal on exit), so the hand-rolled enable/disable sequences are gone.
	// Windows self-update leaves <exe>.old beside the binary because the
	// running image can't be deleted mid-process. Clean leftovers on launch.
	cleanupStaleSelfUpdate()

	claimInitialSession()
	defer releaseSessionClaim()
	prog := tea.NewProgram(initialSession())

	// Background, non-blocking check for a newer release. On a fresh cache it
	// answers instantly (no network); otherwise it fetches asynchronously and
	// sends updateAvailableMsg when one is found. Silent on any failure.
	launchUpdateCheck(prog)

	// Kill the core child on SIGHUP (terminal closed) / SIGTERM (kill) so it
	// isn't orphaned and left running after the TUI exits. Best-effort: a missing
	// handle (core not yet started) just sends the quit msg. Instead of os.Exit
	// we send a sigtermMsg so Bubble Tea restores the terminal (alt-screen /
	// raw-mode) via its normal quit path — os.Exit would leave the terminal
	// broken. `quitting` is set first so the core's stdout EOF (from the kill)
	// doesn't trigger an auto-restart; the reader goroutine reaps the process.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGHUP)
	go func() {
		<-sigCh
		quitting.Store(true)
		if p := coreProcess.Load(); p != nil {
			_ = p.Kill()
		}
		prog.Send(sigtermMsg{})
	}()

	_, err := prog.Run()
	if err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}
