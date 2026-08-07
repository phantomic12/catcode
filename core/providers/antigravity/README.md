# Antigravity — Google IDE OAuth

This first-party bundle connects the harness to the Google Antigravity IDE
subscription via the **Code Assist / `cloudcode-pa` gateway**. It uses
Google's standard OAuth 2.0 Authorization Code flow with PKCE against the
public Antigravity IDE client, then runs `:loadCodeAssist` to fetch a real
`cloudaicompanionProject` for the authenticated user.

Use `/login` and choose **Antigravity (Google IDE)**, or run:

```text
/login antigravity
```

The harness binds a loopback redirect, opens the browser to Google's
authorization page, captures the code, exchanges it for tokens, runs
`loadCodeAssist` (with the Antigravity IDE 2.1.1 fingerprint headers), and
persists everything to `~/.config/catalyst-code/oauth/antigravity.json`.
On every subsequent turn the harness refreshes the access token when needed
and injects an `x-goog-user-project` header carrying the discovered project
id, so requests route to the user's real Antigravity project — not the
shared freemium project the adapter ships as a fallback.

If `loadCodeAssist` returns no project (new Google account with no
Code Assist history yet), the harness also calls `:onboardUser` and polls
until provisioning finishes (`done=true`), so the first request never fails
with `project not found`.

## Models

Antigravity exposes Gemini 3 / 3.1 Pro and Flash (with tiered -high / -low
for Pro), Claude Sonnet 4.6 and Opus 4.6 Thinking, plus GPT-OSS 120B.
Model IDs map 1:1 to upstream Code Assist slugs — no aliasing:

```text
gemini-3.1-pro-high
gemini-3.1-pro-low
gemini-3-pro-high
gemini-3-pro-low
gemini-3-flash
gemini-2.5-pro
gemini-2.5-flash
claude-opus-4-6-thinking
claude-sonnet-4-6
gpt-oss-120b-medium
```

## Endpoints

| Purpose                     | URL                                                                |
|-----------------------------|--------------------------------------------------------------------|
| Authorization               | `https://accounts.google.com/o/oauth2/v2/auth`                     |
| Token exchange / refresh    | `https://oauth2.googleapis.com/token`                              |
| Userinfo (email)            | `https://www.googleapis.com/oauth2/v1/userinfo`                    |
| Project discovery           | `https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`    |
| User onboarding (fallback)  | `https://cloudcode-pa.googleapis.com/v1internal:onboardUser`       |
| Chat (streamGenerateContent)| `https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal`     |

Project discovery + onboarding use the **prod** Code Assist host — the
daily/sandbox host rejects `loadCodeAssist` and `onboardUser`. Only chat
traffic uses the daily host (to bypass prod-side 429 rate limits).

## Client identity

The script uses the public Antigravity IDE OAuth client:

| Field         | Value                                                                             |
|---------------|-----------------------------------------------------------------------------------|
| `client_id`   | `1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com`       |
| `client_secret` | `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`                                          |
| User-Agent    | `antigravity/ide/2.1.1 darwin/arm64`                                              |
| Metadata      | `{ ideType: 9, platform: 2, pluginType: 2 }` (ANTIGRAVITY, DARWIN_ARM64, GEMINI) |

These are intentional — every Antigravity IDE install carries the same
public client and the same fingerprints. Google's backend uses the
fingerprint to detect non-IDE clients and silently refuses to provision a
project if it looks wrong, so matching them is what lets the first
request succeed.

## Wire

After OAuth + project discovery, every chat turn is a POST to
`{base_url}:streamGenerateContent?alt=sse` with body shape:

```json
{
  "model": "gemini-3.1-pro-high",
  "project": "<discovered cloudaicompanionProject>",
  "userAgent": "antigravity",
  "request": {
    "contents": [...],
    "systemInstruction": {...},
    "tools": [...],
    "generationConfig": {"maxOutputTokens": N}
  }
}
```

The `project` field comes from the harness's `x-goog-user-project` header
(merged from the OAuth plugin's per-request headers); see
`core/src/providers/google_code_assist.rs`.

## References

- Antigravity IDE source fingerprint: captured from a real 2.1.1 install.
- Code Assist wire format + project discovery: Google's open-source
  Gemini CLI + Antigravity IDE.