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
`resolve_project` reads the `x-goog-user-project` header that each plugin's
`token` action injects to use the user's real Code Assist project instead
of the freemium fallback.
