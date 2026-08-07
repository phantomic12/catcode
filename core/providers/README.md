# core/providers/ — First-Party Provider Catalog

This directory holds the **official, shipped** provider bundles for the harness.
Each subdirectory is one provider: a self-contained bundle of its identity,
API-key or OAuth/login flow (if any), and metadata. Bundles are embedded into the binary
at compile time (`include_str!` in `core/src/staging.rs`) and staged into every
install at first run under `~/.catalyst-code/plugins/<name>/`, so they are
globally available with no user install and no `--trust-project-plugins`.

## Transport vs catalog (two "providers" directories)

There are two directories named `providers` under `core/` — they are different
layers and must not be confused:

| Directory | Concern | Contents |
|-----------|---------|----------|
| `core/src/providers/` | **Wire-protocol transport** | *How* the harness speaks to any provider — the OpenAI/Anthropic/Codex/Google adapters, SSE decoding, model discovery, usage parsing. Provider-agnostic. |
| `core/providers/` (this dir) | **Shipped provider catalog** | *Which* providers we officially ship — each as a self-contained bundle (`plugin.json`, optional OAuth scripts, and README). Provider-specific. |

A provider bundle declares *who* the provider is and *how to log in*; the
transport layer in `src/providers/` handles *how to talk to it*. Per-provider
Rust specifics that cannot be data (a wire gate like `is_kimi()`, or a curated
capability table like `kimi_model_caps()`) live in `src/` next to the transport
code that uses them.

## Adding a provider

1. Create `core/providers/<name>/` with:
   - `plugin.json` — provider identity/catalog metadata and, when needed, an
     `oauth` block for subscription OAuth. See the `plugin-authoring` skill for
     the full `oauth` schema + script contract (`login` / `complete` / `token` /
     `clear`).
   - `oauth/<name>-oauth.py` — the login/token script (stdlib-only; python3)
     for OAuth providers. API-key providers do not need this file.
   - `README.md` — endpoints, constants, usage.
2. Register it in `core/src/staging.rs`: add `include_str!` entries for each
   file (source path `../../providers/<name>/...`) and add executable scripts to
   `executable_rel_paths()`.
3. Add per-provider Rust specifics in `src/`:
   - `core/src/provider.rs`: a `pub fn is_<name>(base_url)` wire gate (mirror
     `is_umans` / `is_opencode_go`) if the provider needs non-standard request
     fields (e.g. reasoning injection).
   - `core/src/providers/discovery.rs`: an `is_<name>()` + `<name>_model_caps()`
     curated capability table (mirror `opencode_go_model_caps`) so known models
     advertise correct reasoning/vision/context even before the live
     `/models` endpoint is reached; a dedicated `<name>_discover_models` that
     reads the live list (so new models auto-appear) with a curated fallback.

OAuth scripts **must** be on disk to execute, so staging writes them to
`~/.catalyst-code/plugins/<name>/` at runtime; the bundle here is the
source-of-truth embedded into the binary.

## Current providers

- `kimi/` — Kimi Code (Moonshot), device-code OAuth subscription.
- `codex/` — ChatGPT (Codex), official Codex CLI device-code OAuth with automatic polling.
- `deepseek/` — DeepSeek API, official OpenAI-compatible API-key provider.
- `antigravity/` — Google Antigravity IDE, OAuth + Code Assist `loadCodeAssist` project discovery (Authorization Code + PKCE).
- `gemini-cli/` — Google Gemini CLI, OAuth + Code Assist `loadCodeAssist` project discovery (Authorization Code + PKCE).

Both Google bundles reuse the existing `core/src/providers/google_code_assist.rs`
adapter — `is_code_assist_endpoint` already routes the `cloudcode-pa` /
daily-cloudcode-pa hosts to the right wire format, and the adapter's
`resolve_project` reads the `x-code-assist-project` header that each plugin's
`token` action injects to use the user's real Code Assist project instead
of the freemium fallback.

## OAuth gotchas

These are the wire-level footguns the `google_code_assist` adapter exists
to handle and the `wire_shape_contract` test module in
`core/src/providers/google_code_assist.rs` line 800 is the authoritative
spec for. Anything in this section will break the live Antigravity IDE /
Gemini CLI flow with HTTP 403 (`SERVICE_DISABLED`) or
`redirect_uri_mismatch` if violated.

### 1. Project header: `x-code-assist-project`, NOT `x-goog-user-project`

`resolve_project` reads the **first** header in the provider's headers vec
that matches any of:

- `x-goog-user-project`
- `cloudaicompanion-project`
- `x-code-assist-project`

(iteration order, case-insensitive). The Google Code Assist chat gateway
treats these as **three different signals** with **different routing**:

| Header | What the gateway does | What to do |
|--------|-----------------------|------------|
| `x-goog-user-project` | Routes to the **consumer** Generative Language API (GenAI) gate. The Antigravity / Gemini CLI OAuth token does **not** have access; the gateway returns `403 SERVICE_DISABLED`. | **Do not inject.** |
| `cloudaicompanion-project` | Routes to the consumer gate same as `x-goog-user-project`. | **Do not inject.** |
| `x-code-assist-project` | Routes to the **Code Assist** gate. The OAuth token is authorized here. The body also carries the same value in `body.project`. | **Inject this one.** |

**The plugin's `token` action MUST return `x-code-assist-project` in its
`headers` array** (not `x-goog-user-project`, not
`cloudaicompanion-project`). The bundled `antigravity/` and `gemini-cli/`
bundles both do this. Verified live against the
`daily-cloudcode-pa.sandbox.googleapis.com` and
`cloudcode-pa.googleapis.com` hosts — swapping the header name surfaces
as `403 SERVICE_DISABLED` on the very first chat request, with no helpful
error message from the gateway.

The `wire_shape_contract::resolve_project_picks_first_matching_header_in_iteration_order`
test (line 882) pins this behavior.

### 2. Code Assist body envelope shape

The Code Assist / GenAI chat endpoint does not use the OpenAI
`{messages, …}` body. The adapter wraps the user messages into the
GenAI streaming envelope:

```json
{
  "model": "<resolved-model-id>",
  "project": "<from x-code-assist-project>",
  "userAgent": "antigravity",
  "request": {
    "contents": [ {"role": "user", "parts": [{"text": "…"}]}, … ],
    "generationConfig": { "maxOutputTokens": <n> },
    "systemInstruction": {"parts": [{"text": "…"}]},
    "tools": [{"functionDeclarations": […]}],
    "thinkingConfig": {"thinkingLevel": "low|medium|high", "includeThoughts": true}
  }
}
```

Pinned by the
`wire_shape_contract::body_uses_antigravity_user_agent_and_body_project`
test (line 837). Key constraints:

- `userAgent` is the **string** `"antigravity"` for Antigravity IDE traffic
  and `"gemini-cli"` for Gemini CLI traffic. The gateway distinguishes
  clients by this field.
- `project` is the value the plugin's `token` action injected as
  `x-code-assist-project`. The header and the body field must agree.
- `contents[].role` is **only** `user` or `model`. `functionResponse`
  parts must ride on a `user` turn (using role `function` 400s on
  `cloudcode-pa` / `generativelanguage`).
- `maxOutputTokens: 0` is rejected ("generate nothing"); the adapter
  floors to `1`.
- Empty `contents` (system-only) is rejected; the adapter errors before
  sending instead of letting the gateway 400.
- Gemini 3 uses `thinkingLevel` (`minimal` / `low` / `medium` / `high` /
  `auto`); Gemini 2.5 uses `thinkingBudget` (numeric); Gemini 2.0
  rejects `thinkingConfig` entirely. The adapter picks the right shape
  per model id (`model_supports_thinking`).

### 3. Redirect path: `/oauth2callback` for Google

The Antigravity and Gemini CLI bundles both declare
`redirect_path: "/oauth2callback"`. Google's installed-app OAuth clients
only accept this exact path; using the harness's default `/callback`
makes `accounts.google.com` reject the request as a non-compliant
redirect URI (error: `redirect_uri_mismatch`, hard non-compliance
per Google's OAuth 2.0 policy for installed apps). The plugin is
expected to embed the harness-provided `redirect_uri` **verbatim** in
the authorize URL — including the port and path.

### 4. Token refresh on the hot path

The `token` action runs on **every turn** (cached for ~5 min, then
re-run). Two consequences:

- Keep `token` cheap. Refresh only when the cached token is near
  expiry; do not call out to the IdP on every chat turn.
- The `headers` returned by `token` are **cached with the token** and
  merged onto the provider's request headers. If `x-code-assist-project`
  changes between calls (e.g. the user's `loadCodeAssist` rotation
  swapped the project), the new value reaches the gateway on the very
  next turn without a `/login` cycle.
