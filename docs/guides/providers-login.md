# Provider Setup and Login Guide

Catalyst Code supports logging into multiple model providers simultaneously.
Each turn is automatically routed to the provider that owns the selected model.

---

## Table of Contents

- [Built-in Provider Presets](#built-in-provider-presets)
- [Login Command (`/login`)](#login-command-login)
- [Logout Command (`/logout`)](#logout-command-logout)
- [Custom Providers via Config File](#custom-providers-via-config-file)
- [Multiple Simultaneous Providers](#multiple-simultaneous-providers)
- [Provider Setup from Environment Variables](#provider-setup-from-environment-variables)
- [OAuth Plugin Providers](#oauth-plugin-providers)
- [Provider Resolution Precedence](#provider-resolution-precedence)
- [Model Discovery and Per-Turn Routing](#model-discovery-and-per-turn-routing)
- [Search Keys for Web Search](#search-keys-for-web-search)

---

## Built-in Provider Presets

Four first-party provider presets ship with the harness
(`PROVIDER_PRESETS` (/core/src/config.rs), line 486). They can be activated
with one `/login` command — no manual JSON editing required.

| Preset ID | Label | Wire Protocol | Base URL | API Key Env Var |
|-----------|-------|--------------|----------|-----------------|
| `umans` | Umans (GLM-5.2) | OpenAI | `https://api.code.umans.ai/v1` | `UMANS_API_KEY` |
| `opencode-go` | OpenCode Go | OpenAI + Anthropic | `https://opencode.ai/zen/go/v1` | `OPENCODE_GO_API_KEY` |
| `openrouter` | OpenRouter | OpenAI | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `deepseek` | DeepSeek | OpenAI | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` |

### OpenCode Go Dual-Protocol Expansion

OpenCode Go serves some models over the OpenAI chat-completions protocol and
others over the Anthropic Messages API, under one subscription and one API key.
When you log into `opencode-go`, the harness creates **two** provider configs
under the hood — one of each `kind` — both sharing the same base URL and key.
This allows models on either wire protocol to be discovered and routed correctly.
The first config is the "primary" (used as the active provider and preset
identity).

Source: `preset_provider_configs()` (/core/src/config.rs), line 598.

### Listing Available Presets

The TUI sends a `list_provider_presets` command on startup and on every
`/login` picker render. The core responds with a `provider_presets` event
containing the preset list plus whether each already has a stored key.

---

## Login Command (`/login`)

**Wire protocol:** `Command::Login` (/core/src/protocol.rs), line ~120.

```json
{"type": "login", "preset": "umans", "api_key": "sk-..."}
```

**What happens:**

1. The core looks up the preset by ID via `find_preset()` (/core/src/config.rs),
   line 526. Unknown presets produce an `error` event listing available presets.
2. If `api_key` is omitted and the preset declares a non-empty `api_key_env`,
   the core emits an error prompting the user to paste a key. Environment
   variables are **not** scanned for auto-login — auth is always explicit.
3. The preset is resolved into one or more `ProviderConfig` (/core/src/config.rs)
   entries (the wire-protocol `kind`, base URL, key) and inserted into the
   in-memory provider list.
4. The runtime API key is stored in the in-memory `api_keys` map.
5. The provider list is persisted to `~/.config/catalyst-code/settings.json`
   (the `providers` array and `provider_keys` object) so the login survives
   restarts. If persistence fails, a warning is emitted but the session login
   still works.
6. Models are re-aggregated across **all** logged-in providers so the new
   provider's models join the `/models` list.
7. Events emitted:
   - `provider_changed` — with `provider`, `kind`, `base_url`, and `has_key`.
   - `authed` — `{ok: true, provider: "..."}`.
   - `info` — `{message: "logged into <label>."}`.

### Interactive Flow in the TUI

1. Press `/` to open the command bar.
2. Type `login` and press Enter — the picker shows the four built-in presets
   plus any plugin OAuth providers.
3. Select a preset.
4. You are prompted to paste an API key. For presets with a configured env var
   already set, the TUI shows an `info` event but still asks for a paste.
5. On success, the provider's models appear in the model picker.

### API Key Sources (in priority order)

1. Explicit `api_key` argument to `/login`
2. The preset's `api_key_env` env var (primary — e.g. `UMANS_API_KEY`)
3. Plugin OAuth token (for plugin-based providers)

Source: `ProviderPreset::resolved_env()` (/core/src/config.rs), line 530.

---

## Logout Command (`/logout`)

**Wire protocol:** `Command::Logout` (/core/src/protocol.rs), line ~130.

```json
{"type": "logout", "provider": "umans"}
```

**What happens:**

1. The provider's runtime key is dropped from the in-memory `api_keys` map.
2. The provider is removed from the configured providers list.
3. The change is persisted to `settings.json`.
4. Models are re-aggregated — the logged-out provider's models disappear from
   `/models`.
5. If the logged-out provider was the active provider, the harness falls back
   to the next available provider (or the legacy default).
6. Events emitted:
   - `info` — `{message: "logged out of '<provider>'"}`.
   - `provider_changed` — with the new fallback provider.
   - `authed` — `{ok: <bool>, provider: "..."}`. `ok: false` when no key remains.

Logging out of a provider that was never logged in produces an `error` event:
`"not logged into '<provider>'"`.

---

## Custom Providers via Config File

For providers not in the built-in presets, add them to the `providers` array in
any [settings file](../configuration/index.md).

### Config File Format

```json
{
  "providers": [
    {
      "name": "my-endpoint",
      "kind": "openai",
      "base_url": "https://api.example.com/v1",
      "api_key": "sk-...",
      "api_key_env": "MY_API_KEY",
      "extra_headers": {
        "HTTP-Referer": "my-app"
      }
    }
  ]
}
```

### Provider Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique provider name (used as the `provider` arg to `set_provider` and `logout`) |
| `kind` | string | yes | `"openai"` (default) or `"anthropic"` — determines wire protocol translation |
| `base_url` | string | yes | API endpoint base URL |
| `api_key` | string | no | Literal API key (store only in user-owned files like `~/.config/catalyst-code/settings.json`, never in project-scoped files) |
| `api_key_env` | string | no | Name of an env var to read the key from at request time (e.g. `"OPENAI_API_KEY"`) |
| `extra_headers` | object | no | Additional HTTP headers appended to every request |

Source: `parse_provider()` (/core/src/config.rs), line 1571.

### Provider Kind

The `kind` field selects the wire protocol used at the HTTP boundary:

| Kind | Wire Protocol | Suitable For |
|------|--------------|--------------|
| `openai` | `/v1/chat/completions` (OpenAI shape) | OpenAI, Umans, OpenRouter, Ollama, LM Studio, most proxy gateways |
| `anthropic` | `/v1/messages` (Anthropic shape) | Anthropic, Claude, Anthropic-compatible endpoints |

The internal conversation is always stored in the OpenAI chat-completions shape.
The provider abstraction translates only at the HTTP boundary — the rest of the
harness (compaction, sanitization, subagents, session persistence) is unaffected
by provider choice.

Source: `ProviderKind` (/core/src/config.rs), line 370.

### Runtime Key Switching

The command `/set-key` (protocol `set_key`) applies an API key to a named
provider at runtime, overriding both `api_key` and `api_key_env`:

```json
{"type": "set_key", "api_key": "sk-...", "provider": "my-endpoint"}
```

When `provider` is omitted, the key applies to the currently active provider
(the "default" slot, backward-compatible with the pre-provider single-endpoint
flow).

### Switching the Active Provider

```json
{"type": "set_provider", "name": "my-endpoint"}
```

This changes the default/fallback provider for operations that don't route to
a specific model's provider (e.g. compaction summarization). It re-resolves the
base URL, key, and wire protocol, then emits a `provider_changed` event.
Unknown provider names are silently ignored.

---

## Multiple Simultaneous Providers

You can be logged into **multiple providers at the same time**. Each provider's
models appear in the global model list. When you select a model for a turn, the
harness routes the request to whichever provider owns that model — each provider
can use a different base URL, API key, and wire protocol.

Example: log into Umans (for GLM-5.2), OpenCode Go (for DeepSeek/Qwen), and
a custom OpenAI-compatible local endpoint simultaneously. Each model routes to
its correct backend.

### Listing Providers

The `list_provider_presets` command returns both built-in presets and plugin
OAuth provider availability. The aggregate model list (available via the
`models` event after `init`) carries a `provider` field on each
`ModelInfo` (/core/src/protocol.rs) entry, indicating which provider owns it.

```json
{"type": "list_provider_presets"}
```

Response: `{"type": "provider_presets", "presets": [...]}`.

---

## Provider Setup from Environment Variables

### Preset-Specific Env Vars

The built-in presets look for their API key in these environment variables:

| Env Var | Preset |
|---------|--------|
| `UMANS_API_KEY` | Umans |
| `OPENCODE_GO_API_KEY` | OpenCode Go |
| `OPENROUTER_API_KEY` | OpenRouter |
| `DEEPSEEK_API_KEY` | DeepSeek |

**Important:** These env vars are **not** scanned for auto-login. A user must
still run `/login` with an explicit key paste. The env var is used as the
**fallback key source** only after login has activated the provider.

### Custom Provider Env Vars

For custom providers, use the `api_key_env` field in the provider config to
name an environment variable that holds the API key:

```json
{
  "name": "openai-direct",
  "kind": "openai",
  "base_url": "https://api.openai.com/v1",
  "api_key_env": "OPENAI_API_KEY"
}
```

This defers key resolution to request time — the env var is read on every API
call, so you can rotate keys without restarting the harness.

### UMANS_PROVIDERS Env Var

An entire provider config array can be injected at process start via:

```bash
export UMANS_PROVIDERS='[{"name":"custom","kind":"openai","base_url":"https://my-api/v1","api_key_env":"MY_API_KEY"}]'
```

This is JSON with the same schema as the `providers` array in the config file.
Entries are merged into the provider list during startup.

### Key Resolution Order

When making an API call, the effective key for a provider is resolved in this
order (first non-empty wins):

1. Runtime key (set via `/login` / `set_key` or OAuth) — highest priority
2. Config literal `api_key` field
3. Config `api_key_env` env var — read from the process environment
4. If no providers are configured: the legacy "default" runtime key from
   `cfg.base_url` + `runtime_keys["default"]`

Source: `ResolvedProvider` (/core/src/config.rs), line ~420.

---

## OAuth Plugin Providers

Built-in vendor OAuth has been removed from the core. Subscription-based OAuth
login (e.g., ChatGPT/Codex, xAI SuperGrok) is handled by **plugins** that
declare an `oauth` block. The first-party Codex bundle is staged automatically:

```text
/login codex
```

It follows the official Codex CLI device flow, polls automatically after the
device code is approved, and imports file-backed `$CODEX_HOME/auth.json` when
the Codex CLI has already authenticated. It does not require `/oauth-code`.

### Plugin OAuth Flow

1. Install a plugin that declares `oauth.provider_id` (e.g.,
   `catcode-chatgpt-provider` for ChatGPT).
2. Run `/login` and select the OAuth provider from the picker, or send:
   ```json
   {"type": "login_oauth", "preset": "chatgpt"}
   ```
3. The core emits `{"type": "oauth_prompt", "url": "...", "code": "...", "message": "..."}`.
4. Open the URL on any device and approve the authorization. Automatic device
   flows poll and finish here; manual flows continue with the next step.
5. For a manual flow, paste the code via the TUI or send:
   ```json
   {"type": "oauth_code", "code": "..."}
   ```
6. On success, the provider config is created, models are refreshed, and the
   provider appears in the model list.

If no plugin supports the requested preset, an error event is emitted:
`"'<preset>' has no plugin OAuth login — install a plugin that declares
oauth.provider_id=\"<preset>\", or paste an API key via /login"`.

Source: `Command::LoginOauth` (/core/src/protocol.rs), line ~135.

---

## Provider Resolution Precedence

The full provider resolution chain for an API call:

```
Runtime key (from /login / set_key / OAuth)
    ↓ (overrides)
Config literal api_key (from settings.json)
    ↓ (overrides)
Config api_key_env (env var read at request time)
    ↓ (overrides)
Legacy default (cfg.base_url + "default" runtime key)
```

When the provider's `api_key_env` is empty, the harness treats it as
OAuth-only (key always resolved by plugin OAuth).

Source: `ResolvedProvider` (/core/src/config.rs), line ~420.

---

## Model Discovery and Per-Turn Routing

Each provider exposes its available models via its `/models` or `/v1/models`
endpoint. The core discovers them automatically at startup and after every
login/logout event.

### Model List

Every `ModelInfo` (/core/src/protocol.rs) entry carries:

| Field | Description |
|-------|-------------|
| `id` | Model identifier (e.g. `"glm-5.2"`) |
| `name` | Human-readable name |
| `reasoning` | Whether the model supports reasoning/thinking |
| `context_window` | Context window size in tokens |
| `max_tokens` | Maximum output tokens |
| `thinking_levels` | Supported reasoning effort levels (`["low","medium","high"]`) |
| `vision` | Whether the model accepts image inputs |
| `input` | Accepted input modalities (`text`, `image`, `pdf`, etc.) |
| `output` | Emitted output modalities (`text`, etc.) |
| `tool_call` | Whether the model supports function/tool calls |
| `structured_output` | Whether the model supports structured/JSON output |
| `provider` | **Provider name** that owns this model — used for per-turn routing |

The custom-provider editor exposes these fields for every discovered or manually
entered model. Changes persist under the provider's `models_override` array in
`~/.config/catalyst-code/config.json` and are applied after live discovery and
models.dev enrichment, so explicit edits win over inferred metadata.

### Per-Turn Routing

When you send a prompt with a model ID, the harness:

1. Looks up the model in the global list.
2. Reads the `provider` field to determine which provider owns the model.
3. Resolves that provider's `ResolvedProvider` (kind, base URL, effective key).
4. Routes the request to the correct endpoint with the correct wire protocol.

This means provider A's models always go to provider A's API, even when you
are simultaneously logged into providers B and C. Each turn is independent.

### Provider Plan / Rate-Limit Usage

The `/usage` command (`Command::Usage`) queries the currently active provider's
usage stats (rate limits, concurrency, plan limits). Each provider implements
its own stats endpoint. Results are returned in a `usage` event.

---

## Search Keys for Web Search

The `web_search` tool uses dedicated search APIs (Exa, Tavily). Configure them
via the TUI's settings or the `set_search_key` command:

```json
{"type": "set_search_key", "provider": "exa", "api_key": "sk-..."}
```

Supported providers: `"exa"` and `"tavily"`.

The key is persisted to `config.json` under `search_keys` so it survives
restarts. The core also reads `EXA_API_KEY` and `TAVILY_API_KEY` env vars as
a fallback.

Setting an empty `api_key` clears the stored key. Emits a `search_key_set`
event with `{provider, has_key}`.

Source: `Command::SetSearchKey` (/core/src/protocol.rs), line ~97.
