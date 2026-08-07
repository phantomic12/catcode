# Gemini CLI — Google OAuth

This first-party bundle connects the harness to the **Gemini CLI**
(`@google-gemini/gemini-cli`) subscription tier via Google's **Code
Assist / `cloudcode-pa` gateway**. It uses Google's standard OAuth 2.0
Authorization Code flow with PKCE against the public gemini-cli client,
then runs `:loadCodeAssist` to fetch a real `cloudaicompanionProject`
for the authenticated user.

Use `/login` and choose **Gemini CLI (Google)**, or run:

```text
/login gemini-cli
```

The harness binds a loopback redirect, opens the browser to Google's
authorization page, captures the code, exchanges it for tokens, runs
`loadCodeAssist` (with the gemini-cli fingerprint headers — `X-Goog-Api-Client`
+ `Client-Metadata`), and persists everything to
`~/.config/catalyst-code/oauth/gemini-cli.json`. On every subsequent
turn the harness refreshes the access token when needed and injects an
`x-goog-user-project` header carrying the discovered project id, so
requests route to the user's real Cloud project — not the shared
freemium project the adapter ships as a fallback.

If `loadCodeAssist` returns no project (new Google account with no
Code Assist history yet), the harness also calls `:onboardUser` and polls
until provisioning finishes (`done=true`), so the first request never
fails with `project not found`.

## Models

Gemini CLI exposes the Gemini 3 / 3.1 Pro + Flash previews plus the
2.5 family. Model IDs map 1:1 to upstream Code Assist slugs — no
aliasing:

```text
gemini-3.1-pro-preview
gemini-3-pro-preview
gemini-3-flash-preview
gemini-3.1-flash-lite-preview
gemini-2.5-pro
gemini-2.5-flash
gemini-2.5-flash-lite
```

## Endpoints

| Purpose                     | URL                                                                |
|-----------------------------|--------------------------------------------------------------------|
| Authorization               | `https://accounts.google.com/o/oauth2/v2/auth`                     |
| Token exchange / refresh    | `https://oauth2.googleapis.com/token`                              |
| Userinfo (email)            | `https://www.googleapis.com/oauth2/v1/userinfo`                    |
| Project discovery           | `https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`    |
| User onboarding (fallback)  | `https://cloudcode-pa.googleapis.com/v1internal:onboardUser`       |
| Chat (streamGenerateContent)| `https://cloudcode-pa.googleapis.com/v1internal`                   |

## Client identity

The script uses the public Gemini CLI OAuth client:

| Field            | Value                                                                            |
|------------------|----------------------------------------------------------------------------------|
| `client_id`      | `681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com`       |
| `client_secret`  | `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`                                            |
| `User-Agent`     | `google-api-nodejs-client/9.15.1`                                                |
| `X-Goog-Api-Client` | `google-cloud-sdk vscode_cloudshelleditor/0.1`                                 |
| `Client-Metadata`| `{ ideType: 0, platform: 0, pluginType: 0 }`                                     |

These are intentional — the gemini-cli npm package ships the same public
client and fingerprints. Google's backend uses them to differentiate
gemini-cli traffic from Antigravity / 3rd-party clients; including the
wrong pair (or omitting the `X-Goog-Api-Client` / `Client-Metadata`
headers) makes OAuth succeed but `loadCodeAssist` returns no project
and the first chat request fails with "project not found".

## Wire

After OAuth + project discovery, every chat turn is a POST to
`{base_url}:streamGenerateContent?alt=sse` with body shape:

```json
{
  "model": "gemini-3.1-pro-preview",
  "project": "<discovered cloudaicompanionProject>",
  "userAgent": "google-api-nodejs-client/9.15.1",
  "request": {
    "contents": [...],
    "systemInstruction": {...},
    "tools": [...],
    "generationConfig": {"maxOutputTokens": N}
  }
}
```

The `project` field comes from the harness's `x-code-assist-project` header
(merged from the OAuth plugin's per-request headers); see
`core/src/providers/google_code_assist.rs`.

## References

- Gemini CLI source fingerprint: captured from a live `@google-gemini/gemini-cli`
  install.
- Code Assist wire format + project discovery: Google's open-source
  Gemini CLI source.

## Working models (verified 2026-08)

With a free-tier Google account the gemini-cli OAuth client is marked
`UNSUPPORTED_CLIENT` for free-tier project *provisioning*, but chat still
works when `body.project` is set to a managed project the same account
already owns (e.g. one provisioned by the sibling Antigravity login). Do
**not** send `x-goog-user-project` — that header forces a Cloud Code
Private API consumer check and returns `SERVICE_DISABLED`. The plugin
emits `x-code-assist-project` instead so the harness adapter only puts
the id into `body.project`.

Verified working (HTTP 200, real text):

```text
gemini-2.5-pro
gemini-2.5-flash
gemini-2.5-flash-lite
gemini-3.1-flash-lite-preview
```

404 / not available on free-tier gemini-cli:

```text
gemini-3-pro-preview
gemini-3-flash-preview
gemini-3.1-pro-preview
gemini-3.1-pro-high          # Antigravity-only slug
claude-*                     # Antigravity-only
```

## Gotchas

1. **Never send `x-goog-user-project`.** It is a Google consumer-project
   header and trips `SERVICE_DISABLED` on free-tier managed projects.
   Project goes in the JSON body only (`{"project": "...", "model": "...",
   "request": {...}}`). The plugin uses `x-code-assist-project` which the
   harness adapter translates into `body.project` without the consumer
   gate.
2. **User-Agent for chat** should look like the official CLI:
   `GeminiCLI/0.34.0/<model> (linux; x64; terminal)` plus
   `X-Goog-Api-Client: google-genai-sdk/1.41.0 gl-node/v22.19.0`. The
   harness currently leaves User-Agent as whatever `plugin.json` sets;
   body-level `userAgent: "antigravity"` (set by the shared adapter) is
   tolerated by the gateway.
3. **Project discovery** may return nothing for free-tier gemini-cli
   accounts. The script then falls back to
   `CATALYST_CODE_GEMINI_CLI_PROJECT` or the sibling Antigravity token's
   `project_id` under `~/.config/catalyst-code/oauth/antigravity.json`.
