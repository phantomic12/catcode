export interface SharedSessionTarget {
  session: string;
  workspace: string;
}

export function parseSharedSession(search: string): SharedSessionTarget | null {
  const params = new URLSearchParams(search);
  const session = params.get("session")?.trim() ?? "";
  const workspace = params.get("workspace")?.trim() ?? "";
  if (!session || !workspace || !session.endsWith(".jsonl")) return null;
  return { session, workspace };
}

export function buildSharedSessionUrl(
  origin: string,
  session: string,
  workspace: string,
): string {
  const url = new URL("/", origin);
  url.searchParams.set("session", session);
  url.searchParams.set("workspace", workspace);
  return url.toString();
}
