package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"charm.land/bubbles/v2/filepicker"
	"charm.land/bubbles/v2/list"
	"charm.land/bubbles/v2/paginator"
	tea "charm.land/bubbletea/v2"
	"charm.land/huh/v2"
	"charm.land/lipgloss/v2"
)

// Extra modal kinds that lean on Charm components (kept here so modal.go's
// iota block stays stable for existing tests that match kinds by name).
const (
	modalAttachFile modalKind = 1000 + iota // bubbles/filepicker for /attach
)

// catalogItem adapts a listItem to bubbles/list's DefaultItem.
type catalogItem struct {
	title, desc, group, shortcut string
	abs                          int // original index into the unfiltered items slice
}

func (c catalogItem) FilterValue() string {
	return strings.Join([]string{c.group, c.title, c.desc, c.shortcut}, " ")
}
func (c catalogItem) Title() string { return c.title }
func (c catalogItem) Description() string {
	if c.group != "" && c.desc != "" {
		return c.group + " · " + c.desc
	}
	if c.group != "" {
		return c.group
	}
	return c.desc
}

func buildPickerList(title string, items []listItem, selected int) list.Model {
	listItems := make([]list.Item, len(items))
	for i, it := range items {
		listItems[i] = catalogItem{
			title:    it.label,
			desc:     it.desc,
			group:    it.group,
			shortcut: it.shortcut,
			abs:      i,
		}
	}
	delegate := catalystListDelegate()
	delegate.ShowDescription = true
	l := list.New(listItems, delegate, 40, 14)
	l.Title = title
	l.SetShowStatusBar(false)
	l.SetFilteringEnabled(true)
	l.SetShowHelp(true)
	// Page numbers, not the dot row: with 50+ palette entries the Dots
	// paginator renders a long row of identical bullets that reads as a
	// glitch; Arabic ("2/11") tells the user where they are.
	l.Paginator.Type = paginator.Arabic
	// "q" is a filter keystroke (type-to-filter), not quit — drop the stale
	// "q quit" hint from the list help so the footer stops lying.
	l.KeyMap.Quit.SetEnabled(false)
	l.Styles.Title = accentStyle
	l.Styles.HelpStyle = dimStyle
	l.Help.Styles = catalystHelpStyles()
	if selected >= 0 && selected < len(listItems) {
		l.Select(selected)
	}
	return l
}

// usesCharmPickerList reports whether the open modal embeds bubbles/list
// (command/models/sessions/theme). Those pickers return FilterMatchesMsg cmds
// while typing a "/" filter; session.Update must forward them or the list never shrinks.
func (s *session) usesCharmPickerList() bool {
	switch s.modal.kind {
	case modalCommand, modalModels, modalSessions, modalTheme:
		return true
	default:
		return false
	}
}

// handlePickerListMsg feeds an arbitrary msg into the embedded picker list.
// Used for FilterMatchesMsg (and similar) that arrive via the program loop
// after handlePickerListKey returns a filterItems cmd.
func (s *session) handlePickerListMsg(msg tea.Msg) tea.Cmd {
	var cmd tea.Cmd
	s.modal.pickerList, cmd = s.modal.pickerList.Update(msg)
	return cmd
}

func (s *session) handlePickerListKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	if (s.modal.loading || s.modal.loadError != "") && strings.EqualFold(msg.String(), "r") {
		s.modal.loading = true
		s.modal.loadError = ""
		s.retryAsyncPicker()
		return s, nil
	}
	// bubbles/list owns "/" filter mode: while SettingFilter, Enter applies the
	// filter and Esc cancels it. Our select/close intercepts must yield so we
	// don't select the first item (or close the modal) instead of filtering.
	filtering := s.modal.pickerList.SettingFilter()
	if s.kb(msg, "close") {
		if filtering {
			var cmd tea.Cmd
			s.modal.pickerList, cmd = s.modal.pickerList.Update(msg)
			return s, cmd
		}
		s.closeModal()
		return s, nil
	}
	if !filtering && s.modal.kind == modalSessions {
		if it, ok := s.modal.pickerList.SelectedItem().(catalogItem); ok {
			abs := it.abs
			if abs >= 0 && abs < len(s.sessionList) {
				e := &s.sessionList[abs]
				switch strings.ToLower(msg.String()) {
				case "ctrl+r":
					s.openValueEditModal(editTargetSessionRename+e.Path, "Rename Session", "session title", e.Title)
					return s, nil
				case "ctrl+p":
					if s.sendCore(map[string]any{"type": "pin_session", "path": e.Path, "pinned": !e.Pinned}) {
						s.logInfo("updating session pin…")
					}
					return s, nil
				case "ctrl+d":
					if e.Current {
						s.modal.loadError = "The active session cannot be deleted; start or load another first."
						return s, nil
					}
					if sessionLockedByAnotherProcess(e.Path) {
						s.modal.loadError = "That session is active in another terminal and cannot be deleted."
						return s, nil
					}
					s.openDestructiveConfirm("session-delete", e.Path, "permanently delete session “"+e.Title+"”")
					return s, nil
				}
			}
		}
	}
	if !filtering && (s.kb(msg, "select") || s.kb(msg, "send")) {
		if it, ok := s.modal.pickerList.SelectedItem().(catalogItem); ok {
			return s.executeListSelect(it.abs)
		}
		return s, nil
	}
	// Type-to-filter: a printable keypress while the list is not filtering
	// jumps straight into filter mode and feeds the key to the filter input —
	// the pre-Charm modals filtered on plain typing, no "/" prefix needed.
	// "/" itself is left to the list's own binding so it opens an empty filter
	// instead of inserting a literal slash.
	if !filtering && msg.Text != "" && msg.String() != "/" {
		s.modal.pickerList.SetFilterState(list.Filtering)
	}
	var cmd tea.Cmd
	s.modal.pickerList, cmd = s.modal.pickerList.Update(msg)
	s.syncPickerListFilter()
	return s, cmd
}

// syncPickerListFilter applies bubbles/list's async filterItems result on the
// same keystroke. list.Update returns filterItems as a tea.Cmd (often Batch'd
// with a cursor Blink tick); waiting for the program loop is correct only if
// session.Update forwards FilterMatchesMsg — and even then the View for this
// frame would still show the unfiltered rows. SetFilterText runs filterItems
// synchronously; SetFilterState(Filtering) keeps "/" edit mode active.
func (s *session) syncPickerListFilter() {
	if !s.modal.pickerList.SettingFilter() {
		return
	}
	q := s.modal.pickerList.FilterValue()
	s.modal.pickerList.SetFilterText(q)
	s.modal.pickerList.SetFilterState(list.Filtering)
}

func (s *session) renderPickerList() string {
	capWidth := 110
	if s.modal.kind == modalCommand || s.modal.kind == modalModels {
		capWidth = 84
	}
	w := s.modalWidth(capWidth)
	s.modal.pickerList.SetWidth(max(20, w-4))
	listH := s.height - 6
	if listH < 1 {
		listH = 1
	}
	if listH > 16 {
		listH = 16
	}
	if s.height <= 10 {
		s.modal.pickerList.SetShowHelp(false)
	}
	s.modal.pickerList.SetHeight(listH)
	body := s.modal.pickerList.View()
	// Record clickable row geometry for mouse hit-testing: the box adds a
	// 1-line border, the list renders a 2-line title section, then items at a
	// fixed 2-line pitch (delegate Height 2 + Spacing 0).
	if perPage := s.modal.pickerList.Paginator.PerPage; perPage > 0 {
		page := s.modal.pickerList.Paginator.Page
		rows := len(s.modal.pickerList.VisibleItems()) - page*perPage
		if rows > perPage {
			rows = perPage
		}
		if rows < 0 {
			rows = 0
		}
		s.modalPickerFirst = 3
		s.modalPickerRows = rows
	}
	if s.modal.loading || s.modal.loadError != "" {
		inner := max(1, w-4)
		var extra strings.Builder
		if s.modal.loading {
			extra.WriteString(accentStyle.Render("  ◷ Loading…") + "\n")
		}
		if s.modal.loadError != "" {
			extra.WriteString(errStyle.Render("  ✗ "+truncate(s.modal.loadError, inner-4)) + "\n")
			extra.WriteString(dimStyle.Render("  press r to retry") + "\n")
		}
		body = extra.String() + body
	}
	box := modalBox(w, body)
	if s.height > 0 {
		lines := strings.Split(box, "\n")
		if len(lines) > s.height {
			box = strings.Join(lines[:s.height], "\n")
		}
	}
	return box
}

func (s *session) openDestructiveConfirm(action, id, desc string) {
	s.modal = newModal()
	s.modal.kind = modalConfirm
	s.modal.confirm = action
	s.modal.confirmID = id
	s.modal.confirmDesc = desc
	s.modal.confirmChoice = false // Cancel / negative is the safe default
	title := "Confirm"
	if id != "" {
		title = "Confirm · " + id
	}
	form := huh.NewForm(
		huh.NewGroup(
			huh.NewConfirm().
				Title(title).
				Description(desc).
				Affirmative("Confirm").
				Negative("Cancel").
				Value(&s.modal.confirmChoice),
		),
	).WithTheme(catalystHuhTheme()).
		WithWidth(s.modalWidth(56)).
		WithShowHelp(true).
		WithShowErrors(false)
	s.modal.confirmForm = form
	_ = form.Init()
}

// openRestartConfirm asks whether to restart the core so a launch-only setting
// takes effect now. Cancel/Later is the safe default (same as destructive confirms).
func (s *session) openRestartConfirm(reason string) {
	s.modal = newModal()
	s.modal.kind = modalConfirm
	s.modal.confirm = "core-restart"
	s.modal.confirmID = reason
	s.modal.confirmChoice = false
	if reason == "" {
		reason = "settings"
	}
	form := huh.NewForm(
		huh.NewGroup(
			huh.NewConfirm().
				Title("Restart core?").
				Description("Apply " + reason + " immediately. Later keeps the change for the next launch.").
				Affirmative("Restart now").
				Negative("Later").
				Value(&s.modal.confirmChoice),
		),
	).WithTheme(catalystHuhTheme()).
		WithWidth(s.modalWidth(56)).
		WithShowHelp(true).
		WithShowErrors(false)
	s.modal.confirmForm = form
	_ = form.Init()
}

func (s *session) handleConfirmFormKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	if s.modal.confirmForm == nil {
		s.closeModal()
		return s, nil
	}
	if s.kb(msg, "close") {
		s.closeModal()
		return s, nil
	}
	s.pumpHuhConfirm(msg)
	switch s.modal.confirmForm.State {
	case huh.StateCompleted:
		ok := s.modal.confirmChoice
		action, id := s.modal.confirm, s.modal.confirmID
		s.closeModal()
		if action == "core-restart" {
			if ok {
				return s, s.requestCoreRestart()
			}
			s.logInfo("restart skipped — change applies on next launch")
			return s, nil
		}
		if ok {
			s.executeDestructive(action, id)
		}
	case huh.StateAborted:
		s.closeModal()
	}
	return s, nil
}

// pumpHuhConfirm feeds msg into the confirm form and drains NextField /
// NextGroup cmds. Embedded overlays don't run a tea.Program, so without this
// Accept/Submit never advance StateCompleted.
func (s *session) pumpHuhConfirm(msg tea.Msg) {
	if s.modal.confirmForm == nil {
		return
	}
	m, cmd := s.modal.confirmForm.Update(msg)
	if f, ok := m.(*huh.Form); ok {
		s.modal.confirmForm = f
	}
	for i := 0; i < 8 && cmd != nil; i++ {
		next := cmd()
		if next == nil {
			return
		}
		m, cmd = s.modal.confirmForm.Update(next)
		if f, ok := m.(*huh.Form); ok {
			s.modal.confirmForm = f
		}
	}
}

func (s *session) renderConfirmForm() string {
	if s.modal.confirmForm == nil {
		return ""
	}
	w := s.modalWidth(56)
	body := s.modal.confirmForm.View()
	inner := max(1, w-4)
	return lipgloss.NewStyle().
		BorderStyle(lipgloss.RoundedBorder()).
		BorderForeground(lipgloss.Color(c.accent)).
		Padding(0, 1).
		Width(inner + 2).
		Render(body)
}

func (s *session) openAttachModal() tea.Cmd {
	s.modal = newModal()
	s.modal.kind = modalAttachFile
	fp := filepicker.New()
	fp.AllowedTypes = []string{".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"}
	fp.DirAllowed = false
	fp.FileAllowed = true
	fp.ShowPermissions = false
	fp.ShowSize = true
	fp.AutoHeight = false
	h := min(18, max(8, s.height-10))
	fp.SetHeight(h)
	startDir := strings.TrimSpace(s.settings.AttachDir)
	if startDir == "" {
		if wd, err := os.Getwd(); err == nil {
			startDir = wd
		}
	} else if st, err := os.Stat(startDir); err != nil || !st.IsDir() {
		if wd, err := os.Getwd(); err == nil {
			startDir = wd
		}
	}
	fp.CurrentDirectory = startDir
	fp.Styles = catalystFilePickerStyles()
	s.modal.filePicker = fp
	return s.modal.filePicker.Init()
}

func (s *session) handleAttachFileKey(msg tea.KeyPressMsg) (tea.Model, tea.Cmd) {
	if s.kb(msg, "close") {
		s.closeModal()
		return s, nil
	}
	var cmd tea.Cmd
	s.modal.filePicker, cmd = s.modal.filePicker.Update(msg)
	if ok, path := s.modal.filePicker.DidSelectFile(msg); ok {
		dir := s.modal.filePicker.CurrentDirectory
		s.closeModal()
		if path != "" {
			if dir == "" {
				dir = filepath.Dir(path)
			}
			if dir != "" && dir != s.settings.AttachDir {
				s.settings.AttachDir = dir
				_ = s.settings.save()
			}
			if s.addPendingImage(path) {
				s.logSuccess(fmt.Sprintf("attached image → %s", filepath.Base(path)))
			}
		}
		return s, nil
	}
	return s, cmd
}

func (s *session) renderAttachFilePicker() string {
	w := s.modalWidth(72)
	inner := max(1, w-4)
	body := s.modal.filePicker.View()
	header := accentStyle.Render("◆ Attach Image") + "\n" +
		dimStyle.Render("  "+s.modal.filePicker.CurrentDirectory) + "\n" +
		dimStyle.Render("  enter select · esc cancel")
	panel := header + "\n" + body
	return lipgloss.NewStyle().
		BorderStyle(lipgloss.RoundedBorder()).
		BorderForeground(lipgloss.Color(c.accent)).
		Padding(0, 1).
		Width(inner + 2).
		Render(panel)
}

func (s *session) openThemePicker() {
	s.modal = newModal()
	s.modal.kind = modalTheme
	items := s.themeItems()
	sel := 0
	for i, th := range themes {
		if strings.EqualFold(th.name, activeTheme.name) {
			sel = i
			break
		}
	}
	s.modal.pickerList = buildPickerList("Theme", items, sel)
}
