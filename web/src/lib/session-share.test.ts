import { describe, expect, test } from "bun:test";
import { buildSharedSessionUrl, parseSharedSession } from "./session-share";

describe("session share links", () => {
  test("round-trips encoded workspace and session paths", () => {
    const url = buildSharedSessionUrl(
      "https://catcode.example",
      "/home/user/.config/catalyst-code/sessions/a b.jsonl",
      "/workspaces/project with spaces",
    );
    expect(parseSharedSession(new URL(url).search)).toEqual({
      session: "/home/user/.config/catalyst-code/sessions/a b.jsonl",
      workspace: "/workspaces/project with spaces",
    });
  });

  test("rejects incomplete or non-session targets", () => {
    expect(parseSharedSession("?session=%2Ftmp%2Fsession.jsonl")).toBeNull();
    expect(parseSharedSession("?workspace=%2Ftmp&session=%2Ftmp%2Fsession.txt")).toBeNull();
    expect(parseSharedSession("?workspace=%2Ftmp&session=%2Ftmp%2Fsession.jsonl")).toEqual({
      session: "/tmp/session.jsonl",
      workspace: "/tmp",
    });
  });
});
