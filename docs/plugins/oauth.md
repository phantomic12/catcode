# Plugin OAuth Providers

A plugin can add a **subscription OAuth provider** to the harness — no
recompile, no API key, the same `/login` + `/models` flow as a built-in
provider. The plugin declares an `oauth` block in `plugin.json`; the harness
owns the loopback redirect server, polling, and the per-turn token refresh
loop. The plugin supplies **one** script (or per-action overrides) that owns
the on-disk token format and any provider-specific quirks.

This page is the wire-level spec for the `oauth` block and the harness ↔
script contract. The terse overview lives in
[`.catalyst-code/skills/plugin-authoring/SKILL.md`](../../.catalyst-code/skills/plugin-authoring/SKILL.md)
("Declaring an OAuth provider"); the bundle catalog (which providers are
shipped with the core) lives in
[`core/providers/README.md`](../../core/providers/README.md).

---

## Table of contents

- [Full manifest schema](#full-manifest-schema)
  - [Field reference](#field-reference)
  - [`redirect_path`: matching the provider's registered redirect URI](#redirect_path-matching-the-providers-registered-redirect-uri)
  - [`env_passthrough`: plugin-specific config knobs that survive env scrubbing](#env_passthrough-plugin-specific-config-knobs-that-survive-env-scrubbing)
- [Harness ↔ script contract](#harness--script-contract)
  - [Base context (every action)](#base-context-every-action)
  - [`login`](#login)
  - [`complete`](#complete)
  - [`token`](#token)
  - [`clear`](#clear)
- [Wire-format examples](#wire-format-examples)
  - [Web flow (browser on the local machine)](#web-flow-browser-on-the-local-machine)
  - [Manual / headless flow (paste a code)](#manual--headless-flow-paste-a-code)
  - [Automatic device-code flow](#automatic-device-code-flow)
- [How it fits into the harness](#how-it-fits-into-the-harness)
- [Reference implementations](#reference-implementations)

---

## Full manifest schema

The full `OauthManifestEntry` (mirrors `core/src/plugins.rs::OauthManifestEntry`,
the `#[derive(Deserialize)]` the harness actually parses):

```json
{
  "name": "my-provider",
  "version": "0.1.0",
  "oauth": {
    "provider_id": "my-provider",
    "label": "My Provider (subscription)",
    "kind": "openai",
    "base_url": "https://api.example.com/v1",
    "description": "Used in the /login picker",
    "headers": [
      ["User-Agent", "my-plugin/0.1"]
    ],
    "token_path": "my-provider.json",
    "detect_path": null,
    "script": "oauth/my-provider-oauth.py",
    "login_script": "oauth/login.py",
    "complete_script": "oauth/complete.py",
    "token_script": "oauth/token.py",
    "login_timeout_ms": 180000,
    "token_timeout_ms": 30000,
    "redirect_path": "/oauth2callback",
    "env_passthrough": [
      "MY_PROVIDER_HOST",
      "CATALYST_CODE_MYPROVIDER_PROJECT"
    ]
  }
}
```

`plugin.json` must also declare the capabilities the `oauth` block implies —
`execute_subprocess`, `register_providers`, `access_network`, `access_secrets`.
The harness infers them when `capabilities` is omitted.

### Field reference

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `provider_id` | string | **yes** | — | Stable provider identity. `/login`, `/oauth-code`, `/logout`, and the created `~/.config/catalyst-code/config.json` entry all use this name. The plugin's `name` and `provider_id` are independent. |
| `label` | string | no | `provider_id` | Human-readable name shown in the `/login` picker. |
| `kind` | string | no | `"openai"` | Wire protocol. `"openai"` → `/chat/completions` + `Authorization: Bearer`. `"anthropic"` → `/v1/messages` + `x-api-key`. The harness uses this to pick the adapter; model discovery, request building, and SSE decoding all follow it. |
| `base_url` | string | **yes** | — | Provider endpoint, including any path prefix the API expects (`/v1`, `/v1internal`, …). Paths are appended directly. |
| `description` | string | no | `""` | Shown alongside the label in the `/login` picker. |
| `headers` | array of `[name, value]` | no | `[]` | Extra HTTP headers on every request for this provider. Persisted into the `config.json` provider entry. Plugin wins on name conflicts with any header the `token` action also returns. |
| `token_path` | string | no | `<provider_id>.json` | Token-file name, resolved against `~/.config/catalyst-code/oauth/`. The harness passes the **absolute** path to every script invocation; the plugin owns the on-disk format. |
| `detect_path` | string | no | — | External credential file the harness can probe for cheap "already-logged-in" detection (no schema parsing). Supported patterns: `$CODEX_HOME/auth.json` and `~/.codex/auth.json`. Other paths are resolved against `$HOME` and rejected if they escape it or are absolute. The provider script remains responsible for importing the format. |
| `script` | string | conditional | — | Script handling **all** four actions, dispatched by the `action` field on stdin. Required unless every action has an explicit override. |
| `login_script` | string | no | falls back to `script` | Per-action override for `login`. |
| `complete_script` | string | no | falls back to `script` | Per-action override for `complete`. |
| `token_script` | string | no | falls back to `script` | Per-action override for `token`. **Token resolution is mandatory** — without a script for `token` (or a shared `script`), the harness rejects the manifest at load time. |
| `login_timeout_ms` | number | no | `120000` | Per-call timeout for `login` and `complete`. |
| `token_timeout_ms` | number | no | `30000` | Per-call timeout for `token` and `clear`. `token` runs on the per-turn hot path, so keep it short. |
| `redirect_path` | string | no | `"/callback"` | The path the harness binds on its loopback server for the web flow. Must match the redirect URI registered with the provider's OAuth client. See [below](#redirect_path-matching-the-providers-registered-redirect-uri). |
| `env_passthrough` | array of string | no | `[]` | Non-secret env var names the harness forwards from its own process env to the plugin's scripts. Names must be `[A-Za-z_][A-Za-z0-9_]*` and **must not** contain `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, or `CREDENTIAL` (case-insensitive) — passthrough must never defeat env scrubbing. See [below](#env_passthrough-plugin-specific-config-knobs-that-survive-env-scrubbing). |

### `redirect_path`: matching the provider's registered redirect URI

The harness binds a loopback server (`http://localhost:<port>/<redirect_path>`)
on demand and embeds that exact URL in the authorize request the script
builds. **The path component is not arbitrary** — it must be one of the
redirect URIs registered with the provider's OAuth client, or the provider
will reject the request (Google, for example, returns a
`redirect_uri_mismatch` error and the spec calls this out as a hard
non-compliance).

| Provider / client type | Required path | Why |
|------------------------|---------------|-----|
| Most OAuth clients (the default) | `/callback` | The conventional path; the harness ships this as the default so a simple `oauth` block "just works". |
| Google installed-app OAuth clients (Antigravity IDE, Gemini CLI) | `/oauth2callback` | Google only accepts this exact path for installed-app / desktop clients; `/callback` is rejected as non-compliant. |
| Self-hosted / custom IdPs | Whatever the IdP expects | E.g. a corporate IdP may require `/auth/callback` or `/oauth/callback`. |

When to set it:

- **Always set it for Google OAuth clients** (the Antigravity and Gemini CLI
  bundles do). Verified live: omitting it on the Antigravity OAuth client
  returns `redirect_uri_mismatch` from `accounts.google.com`.
- **Always set it when the provider's registered redirect URI is not
  `/callback`**. Read the provider's OAuth docs.
- **Default is fine for most other providers** (ChatGPT Codex, Grok xAI,
  generic OAuth/OIDC, GitHub Apps with a localhost callback, etc.).

Implementation note: the harness prefixes a `/` if the value does not start
with one, so `redirect_path: "oauth2callback"` and
`redirect_path: "/oauth2callback"` are equivalent. Absolute paths and paths
with a scheme/host are rejected.

### `env_passthrough`: plugin-specific config knobs that survive env scrubbing

Plugin scripts are spawned with a **scrubbed** environment: the harness
clears the child's env and re-injects only a small allowlist (`PATH`,
`HOME`, `TMPDIR`, `USER`, plus the Windows baseline on Windows, plus a
handful of memory-provider keys). This is the defense against a plugin
script accidentally seeing — or exfiltrating — a `*_API_KEY` / `*_TOKEN`
the user exported. The cost: plugin scripts **cannot** see any env var by
default.

`env_passthrough` is the explicit opt-in. Declare the names (not values) of
the env vars your scripts need, and the harness reads the values from its
own process env at call time and injects them into the script's child env.

**Conventions**

- **Plugin-specific project overrides** should follow the
  `CATALYST_CODE_<NAME>_PROJECT` pattern so they're namespaced and easy to
  grep for. Examples already in the catalog:
  - `CATALYST_CODE_ANTIGRAVITY_PROJECT` — overrides the Antigravity Code
    Assist `cloudaicompanionProject` (bypasses the `loadCodeAssist`
    auto-discovery round-trip in tests / CI).
  - `CATALYST_CODE_GEMINICLI_PROJECT` — same for the Gemini CLI bundle.
- **Self-hosted IdP overrides** typically use a `<PRODUCT>_HOST` /
  `<PRODUCT>_API_URL` / `<PRODUCT>_TENANT` shape. Example:
  `["ACME_OAUTH_HOST", "ACME_TENANT"]`.
- **Never** put a secret in passthrough. Names containing `KEY`, `TOKEN`,
  `SECRET`, `PASSWORD`, or `CREDENTIAL` (case-insensitive) are rejected at
  manifest load time. The `value` of a passthrough var lives in the
  harness's env and is whatever the user exported — the harness does not
  inspect or redact it, so do not use passthrough as a back door to leak
  `OPENAI_API_KEY` to a plugin. (The harness already has the value; if a
  plugin needs to know the API key, the user must pass it explicitly via
  `api_key` on a `login` command, not via env.)
- **Validation**: the name must match `[A-Za-z_][A-Za-z0-9_]*`. Names that
  are empty, contain punctuation, or start with a digit are rejected at
  load. This blocks shell-injection attempts in any naive
  `env("USER_SUPPLIED_$X")` plumbing.

**Why not just allow `*`?** The whole point of env scrubbing is that a
plugin script cannot reach the user's `*_API_KEY` exports. An allowlist
keeps the trust model auditable: every env var a plugin can see is declared
in its `plugin.json`.

---

## Harness ↔ script contract

Every script invocation has the same shape:

1. The harness writes **one JSON object** to the script's stdin.
2. The script processes it.
3. The script writes **one JSON object** to stdout (terminated by EOF or
   close). Stderr is captured for error reporting.
4. The harness enforces the timeout (`login_timeout_ms` for `login`/
   `complete`; `token_timeout_ms` for `token`/`clear`), validates that
   the exit was zero, parses the JSON, and either uses the response or
   surfaces an error event.

JSON input is bounded to 1 MiB and stdout/stderr to 1 MiB per invocation.
Timeouts, non-zero exits, and parse failures are surfaced as `error` events
— they never crash the core.

### Base context (every action)

The harness always injects these fields; each action adds its own.

```json
{
  "action": "login",
  "provider_id": "my-provider",
  "token_path": "/home/user/.config/catalyst-code/oauth/my-provider.json",
  "workspace": "/abs/path/to/workspace",
  "timestamp": 1719000000
}
```

`action` is the discriminator (`"login"`, `"complete"`, `"token"`,
`"clear"`). `token_path` is the **absolute** path the harness expects the
script to read/write; the script owns the file's format.

### `login`

**Input** (additions to base context):

| Field | When | Description |
|-------|------|-------------|
| `headless` | always | `true` if the harness detected no display / no browser support; `false` otherwise. Honor it when choosing between web and manual. |
| `redirect_uri` | non-headless only | The `http://localhost:<port>/<redirect_path>` the harness already bound. Embed it **verbatim** in the authorize URL the script builds. |

**Output** (any subset):

```json
{
  "url": "https://auth.example.com/oauth/authorize?...",
  "code": "ABCD-EFGH",
  "message": "Open the URL and enter the code",
  "flow": "web",
  "state": "<csrf-token>",
  "pending": { "verifier": "<pkce>", "device_id": "<id>" }
}
```

- `url` (required, except for `flow: "already_authenticated"`): the
  authorize/verify URL the user should open.
- `code` (optional): user-code to display for manual / device flows.
- `message` (optional, defaults to a generic prompt): UI message shown
  alongside the URL.
- `flow` (optional, defaults inferred from `headless`):
  - `"web"` — the harness will wait for the loopback redirect at
    `redirect_uri`.
  - `"manual"` — the harness stashes the `pending` blob and waits for
    `/oauth-code <code>` from the user.
  - `"poll"` or `"auto"` — the harness immediately calls `complete` and
    waits for the script to drive the device-code polling loop.
  - `"already_authenticated"` — the script imported an existing
    credential store and no browser flow is needed; the harness skips
    straight to `finalize_oauth`.
- `state` (web flow): the CSRF state you put in the authorize URL, so the
  harness can verify the redirect.
- `pending`: an opaque JSON blob to carry to `complete` (PKCE verifier,
  device-auth id, anything else). Passed back verbatim.

### `complete`

**Input** (additions to base context):

| Field | When | Description |
|-------|------|-------------|
| `code` | web + paste flows | The authorization code the provider returned (from the redirect query string or the user's paste). |
| `redirect_uri` | web flow | The same loopback URI from `login` — re-sent so the script can re-validate the code. |
| `pending` | always | The opaque `pending` blob from `login`, if the script returned one. |

**Output**:

```json
{ "ok": true }
{ "ok": false, "error": "expired code" }
```

On `ok: true` the script **must** have written the token to `token_path`
(or sidecar files of its own design). On `ok: false` the harness surfaces
`error` as an `error` event and restores the pending state so the user can
retry with `/oauth-code`.

### `token`

**Input**: base context only. `action` is `"token"`.

**Output**:

```json
{
  "access_token": "<bearer>",
  "expires_at": 1719003600,
  "headers": [
    ["chatgpt-account-id", "<uuid>"],
    ["x-code-assist-project", "my-project"]
  ]
}
```

- `access_token` (required, non-empty): the bearer to use. The harness
  injects it as `Authorization: Bearer <access_token>` for `kind: "openai"`
  or `x-api-key: <access_token>` for `kind: "anthropic"`.
- `expires_at` (optional, unix seconds): when the harness should re-run
  `token` to refresh. `0` or absent = cache for ~5 minutes.
- `headers` (optional): extra HTTP headers to merge onto the provider's
  request headers for **this turn and every subsequent turn** (cached with
  the token). Plugin wins on name conflicts. Common uses:
  - `chatgpt-account-id` for ChatGPT multi-account.
  - `x-code-assist-project` for Antigravity / Gemini CLI bundles
    (overrides the freemium `rising-fact-p41fc` default — see
    [OAuth gotchas](../../core/providers/README.md#oauth-gotchas)).
  - `anthropic-beta` for Anthropic features gated on headers.

This runs on the per-turn hot path. **Concurrency note:** several harness
processes (TUI, web service, a second TUI) can invoke `token` at the same
time, and providers commonly rotate refresh tokens. Write `token_path`
**atomically** (temp file + rename) and serialize the refresh (e.g.
`flock` on a sidecar lock, then re-check freshness before refreshing) — a
truncated read or a lost refresh-token rotation surfaces to the user as an
unexplained "run /login" prompt.

### `clear`

**Input**: base context only.

**Output**:

```json
{ "ok": true }
```

The harness **also** deletes `token_path`, so this action is optional.
Use it to clean up sidecar files the script manages (a refresh-token
mirror, a state file, etc.).

---

## Wire-format examples

### Web flow (browser on the local machine)

1. The user runs `/login my-provider`.
2. The harness binds a loopback server, e.g. `http://localhost:51234/oauth2callback`.
3. The harness calls `login` with stdin:
   ```json
   {
     "action": "login", "provider_id": "my-provider",
     "token_path": "/home/user/.config/catalyst-code/oauth/my-provider.json",
     "workspace": "/abs/path/to/workspace", "timestamp": 1719000000,
     "headless": false,
     "redirect_uri": "http://localhost:51234/oauth2callback"
   }
   ```
4. The script returns:
   ```json
   {
     "url": "https://auth.example.com/oauth/authorize?client_id=...&redirect_uri=http%3A%2F%2Flocalhost%3A51234%2Foauth2callback&state=csrf&...&code_challenge=...&code_challenge_method=S256",
     "flow": "web",
     "state": "csrf",
     "pending": { "verifier": "<pkce-verifier>" }
   }
   ```
5. The harness emits an `oauth_prompt` event (URL + message) and opens
   the browser.
6. The user approves; the browser hits
   `http://localhost:51234/oauth2callback?code=...&state=csrf`.
7. The harness verifies `state`, calls `complete` with stdin:
   ```json
   {
     "action": "complete", "provider_id": "my-provider",
     "token_path": "...", "workspace": "...", "timestamp": 1719000050,
     "code": "<the-code>", "redirect_uri": "http://localhost:51234/oauth2callback",
     "pending": { "verifier": "<pkce-verifier>" }
   }
   ```
8. The script exchanges the code, writes the token, returns `{"ok": true}`.
9. The harness calls `finalize_oauth`: creates the provider config, sets
   it active, refreshes models, emits `authed` + `provider_changed`.

### Manual / headless flow (paste a code)

Same as web flow, but step 5 returns `flow: "manual"`. The harness emits
`oauth_prompt` and **does not** open a browser. The user pastes the code
via `/oauth-code <code>` (or the `oauth_code` protocol command), which
drives step 7.

This is the right flow for SSH/headless sessions, and the recommended
flow for CI / first-party smoke tests.

### Automatic device-code flow

Step 5 returns `flow: "poll"` (or `"auto"`, or
`auto_complete: true`). The harness immediately calls `complete` with an
empty `code`; the script owns the polling loop. The user still sees the
URL + user-code via `oauth_prompt`, but no `/oauth-code` is needed.

---

## How it fits into the harness

- `/login <provider_id>` → harness runs `login` → emits `oauth_prompt` →
  waits for the redirect (web), invokes `complete` immediately (auto
  poll), or stashes `pending` for `/oauth-code` (manual). On success it
  creates the provider config (name = `provider_id`, your
  `base_url`/`kind`/`headers`, no `api_key`) and refreshes `/models`.
- Every turn → harness runs `token` (cached), injects the access token as
  `Authorization: Bearer`, merges any returned `headers`, and routes the
  turn to your `base_url` over your declared `kind`.
- `/logout <provider_id>` → deletes `token_path` + runs `clear` + drops
  the provider config.

The plugin's token format is entirely its own — the harness never parses
the contents of `token_path`.

---

## Reference implementations

Bundled in `core/providers/<name>/`:

- `codex/` — ChatGPT (Codex) CLI device-code OAuth with automatic polling
  and `auth.json` import.
- `antigravity/` — Google Antigravity IDE Authorization Code + PKCE with
  the `loadCodeAssist` project discovery. Uses
  `redirect_path: "/oauth2callback"`.
- `gemini-cli/` — Google Gemini CLI Authorization Code + PKCE with
  `loadCodeAssist` project discovery. Uses
  `redirect_path: "/oauth2callback"`.
- `kimi/` — Kimi Code (Moonshot) device-code OAuth.
- `deepseek/` — **not OAuth** — this is an API-key bundle, shown here only
  as the side-by-side catalog entry.

External template: `docs/examples/plugins/grok-oauth/`.

The wire-level spec is mirrored in
`.catalyst-code/skills/plugin-authoring/SKILL.md` ("Declaring an OAuth
provider"). Update both when adding new fields.
