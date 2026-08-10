import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// hub-layout-store reads homedir() — point HOME at a temp dir for isolation.
const tmp = mkdtempSync(join(tmpdir(), "hub-layout-"));
const prevHome = process.env.HOME;
process.env.HOME = tmp;

const {
  defaultHubState,
  loadHubLayout,
  sanitizeHubState,
  saveHubLayout,
} = await import("../server/hub-layout-store");

afterAll(() => {
  process.env.HOME = prevHome;
  try {
    rmSync(tmp, { recursive: true, force: true });
  } catch {
    /* ignore */
  }
});

describe("hub layout store (chat hub v3)", () => {
  test("default state is empty", () => {
    const s = defaultHubState();
    expect(s.version).toBe(3);
    expect(s.tabPaths).toEqual([]);
    expect(s.active).toBeNull();
    expect(s.sessions).toEqual({});
    expect(s.gitOpen).toBe(true);
    expect(s.gitWidth).toBe(280);
    expect(s.previewOpen).toBe(false);
    expect(s.previewWidth).toBe(420);
    expect(s.terminalOpen).toBe(false);
    expect(s.terminalHeight).toBe(260);
  });

  test("sanitize migrates older layouts and clamps panel sizes", () => {
    const s = sanitizeHubState({
      version: 1,
      tabPaths: ["/tmp/proj"],
      names: { "/tmp/proj": "proj" },
      layouts: { "/tmp/proj": { kind: "nope" } },
      active: "/tmp/proj",
      focused: { "/tmp/proj": "pane_1" },
      sessions: { "/tmp/proj": "/home/x/.config/catalyst-code/sessions/abc/chat.jsonl" },
      gitOpen: false,
      gitWidth: 9999,
      previewOpen: true,
      previewWidth: 50,
      terminalOpen: true,
      terminalHeight: 9999,
    });
    expect(s.version).toBe(3);
    expect(s.tabPaths).toEqual(["/tmp/proj"]);
    expect(s.sessions["/tmp/proj"]).toContain("chat.jsonl");
    // Terminal pane fields are dropped.
    expect((s as { layouts?: unknown }).layouts).toBeUndefined();
    expect(s.gitWidth).toBe(480); // clamped max
    expect(s.gitOpen).toBe(false);
    expect(s.previewOpen).toBe(true);
    expect(s.previewWidth).toBe(280); // clamped min
    expect(s.terminalOpen).toBe(true);
    expect(s.terminalHeight).toBe(560); // clamped max
  });

  test("sanitize drops non-jsonl session paths", () => {
    const s = sanitizeHubState({
      tabPaths: ["/ws"],
      sessions: {
        "/ws": "not-a-session",
        "/other": "/tmp/real.jsonl",
      },
    });
    expect(s.sessions["/ws"]).toBeUndefined();
    expect(s.sessions["/other"]).toBe("/tmp/real.jsonl");
  });

  test("save + load round-trips session paths (cross-device reattach key)", () => {
    expect(loadHubLayout()).toBeNull();
    const state = sanitizeHubState({
      version: 3,
      tabPaths: ["/ws/a"],
      names: { "/ws/a": "a" },
      sessions: {
        "/ws/a": "/home/u/.config/catalyst-code/sessions/deadbeef/2026-01-01.jsonl",
      },
      active: "/ws/a",
      gitOpen: true,
      gitWidth: 300,
      previewOpen: true,
      previewWidth: 500,
      terminalOpen: true,
      terminalHeight: 220,
    });
    const saved = saveHubLayout("user-1", state);
    expect(saved.userId).toBe("user-1");
    expect(saved.updatedAt).toBeGreaterThan(0);

    const loaded = loadHubLayout();
    expect(loaded).not.toBeNull();
    expect(loaded!.state.tabPaths).toEqual(["/ws/a"]);
    expect(loaded!.state.sessions["/ws/a"]).toBe(
      "/home/u/.config/catalyst-code/sessions/deadbeef/2026-01-01.jsonl",
    );
    expect(loaded!.state.version).toBe(3);
    expect(loaded!.state.previewWidth).toBe(500);
    expect(loaded!.state.terminalHeight).toBe(220);

    const raw = JSON.parse(
      readFileSync(join(tmp, ".config", "catalyst-code", "hub-layout.json"), "utf8"),
    );
    expect(raw.userId).toBe("user-1");
    expect(raw.state.sessions["/ws/a"]).toContain("2026-01-01.jsonl");
    expect(raw.state.previewWidth).toBe(500);
  });
});
