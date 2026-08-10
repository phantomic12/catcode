// Bash collapsed-preview helpers — run with `bun test`.
import { describe, expect, test } from "bun:test";
import { summarizeBashCommand, toolArgPreview } from "./format";

describe("summarizeBashCommand", () => {
  test("keeps single-line commands", () => {
    expect(summarizeBashCommand("go test ./...")).toBe("go test ./...");
  });

  test("pairs bare cd with next work line and appends count", () => {
    const cmd = `cd /home/karutoil/glm-5.2-ai-harnesss
python3 << 'PY'
from pathlib import Path
print('hi')
PY`;
    expect(summarizeBashCommand(cmd)).toBe(
      "cd /home/karutoil/glm-5.2-ai-harnesss · python3 << 'PY' · 5 lines",
    );
  });

  test("skips blank and comment leads", () => {
    expect(summarizeBashCommand("# setup\n\ncargo test -p core")).toBe(
      "cargo test -p core · 3 lines",
    );
  });
});

describe("toolArgPreview", () => {
  test("summarizes bash command instead of raw JSON", () => {
    const args = {
      command: "cd /tmp\npython3 << 'PY'\nprint(1)\nPY",
    };
    const preview = toolArgPreview("bash", args, JSON.stringify(args), 72);
    expect(preview).toContain("cd /tmp");
    expect(preview).toContain("python3");
    expect(preview).not.toContain("print(1)");
  });
});
