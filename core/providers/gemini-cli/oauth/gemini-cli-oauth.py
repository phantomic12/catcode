#!/usr/bin/env python3
"""Gemini CLI (Google) OAuth + Code Assist project discovery.

The harness sends one JSON object on stdin with an ``action`` of ``login``,
``complete``, ``token``, or ``clear``. One JSON object is written to stdout.

This file deliberately uses only the Python standard library. The endpoint
names, client id, redirect URI, refresh grant, and loadCodeAssist metadata
mirror Google's open-source ``gemini`` CLI so the upstream Code Assist
gateway provisions a real ``cloudaicompanionProject`` for us.

HTTP / PKCE / token-IO / refresh-grant / project-extraction helpers live
in ``core/providers/_shared/google_oauth.py`` — see that file for the
shared contract. Vendor constants + ``build_authorize_url`` +
``discover_project_id`` + the four action functions stay here because they
bind to gemini-cli-specific scopes and the sibling-Antigravity-token
fallback used when free-tier loadCodeAssist returns no project.

Compared to the Antigravity plugin this one uses:

* a different public OAuth client (the open-source gemini-cli client);
* a simpler scope list (no cclog / experimentsandconfigs);
* the prod Code Assist host for chat (the daily host rejects gemini-cli
  traffic more often than antigravity traffic in practice);
* the gemini-cli loadCodeAssist fingerprint (google-api-nodejs-client UA
  + X-Goog-Api-Client + Client-Metadata with the IDE/PLATFORM/PLUGIN_TYPE
  numeric enums the gemini-cli binary actually sends).
"""

import json
import os
import sys
import time
import urllib.parse
import urllib.request

# Bring the shared OAuth helpers into scope. The shared module lives at
# ``core/providers/_shared/google_oauth.py``; the import below adds its
# directory to ``sys.path`` so ``google_oauth`` resolves next to this file.
import sys as _sys, os as _os
_HERE = _os.path.dirname(_os.path.abspath(__file__))
_sys.path.insert(
    0, _os.path.abspath(_os.path.join(_HERE, "..", "..", "_shared"))
)
from google_oauth import (  # noqa: E402
    atomic_write,
    error_text,
    extract_cloudaicompanion_project,
    lock_for,
    make_pkce,
    normalize_tokens,
    post_form,
    post_json,
    read_token,
    refresh_access_token as _shared_refresh_access_token,
    token_path as _shared_token_path,
    unlock,
)


# ─── Gemini CLI public OAuth client ────────────────────────────────────────
# Public client_id / client_secret shipped in the open-source
# ``@google-gemini/gemini-cli`` npm package. Reused here unchanged.
CLIENT_ID = "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
CLIENT_SECRET = "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"

AUTH_URL = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URL = "https://oauth2.googleapis.com/token"
USERINFO_URL = "https://www.googleapis.com/oauth2/v1/userinfo"

# Google OAuth requires ``/oauth2callback`` (not arbitrary paths) for the
# gemini-cli OAuth client — only this path is registered as a loopback
# redirect URI for ``http://127.0.0.1:<port>`` in the client's Google Cloud
# console entry. Using ``/callback`` makes Google reject the request as a
# non-compliant redirect URI ("doesn't comply with Google's OAuth 2.0
# policy for keeping apps secure"). The official gemini-cli binary uses
# this exact path; we mirror it.
REDIRECT_PATH = "/oauth2callback"

# Scopes the official ``gemini`` CLI requests (no ``openid``). Including
# ``openid`` triggers Google's "unverified app" rejection for this
# public-but-unverified OAuth client — the gemini-cli project deliberately
# omits it. ``userinfo.email`` + ``userinfo.profile`` alone are sufficient
# for the loadCodeAssist user-info lookup.
SCOPES = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
]

# Gemini CLI fingerprints captured from a live ``gemini`` CLI install.
# Google fingerprints these headers + the metadata payload and silently
# refuses to provision a project if they look wrong (or if they're
# missing entirely), so the OAuth flow would technically succeed but the
# first chat request would 404 with "Project not found".
USER_AGENT = "google-api-nodejs-client/9.15.1"
X_GOOG_API_CLIENT = "google-cloud-sdk vscode_cloudshelleditor/0.1"
# Numeric enum values that match what gemini-cli actually sends. These are
# not the same as Antigravity (different ideType/pluginType).
# 9router's gemini-cli path uses Antigravity-style ClientMetadata on
# loadCodeAssist (ideType=9 / pluginType=2). Using the zeroed "unspecified"
# values makes Google refuse to provision a cloudaicompanionProject for
# free-tier individuals (UNSUPPORTED_CLIENT on free-tier).
def _platform_enum():
    import platform as _plat
    s = _plat.system().lower()
    a = _plat.machine().lower()
    if s == "darwin":
        return 2 if "arm64" in a or "aarch64" in a else 1
    if s == "linux":
        return 4 if "arm64" in a or "aarch64" in a else 3
    if s == "windows" or s == "win32":
        return 5
    return 0


CLIENT_METADATA = {"ideType": 9, "platform": _platform_enum(), "pluginType": 2}

LOAD_CODE_ASSIST_URL = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
ONBOARD_USER_URL = "https://cloudcode-pa.googleapis.com/v1internal:onboardUser"

REFRESH_LEAD_S = 300
ONBOARD_MAX_ATTEMPTS = 5
ONBOARD_POLL_S = 2
HTTP_TIMEOUT_S = 30

# Per-script defaults for shared helpers.
_TOKEN_FILENAME = "gemini-cli.json"
_ATOMIC_WRITE_PREFIX = ".gemini-cli-oauth-"


# ─── harness I/O ───────────────────────────────────────────────────────────

def emit(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")))
    sys.stdout.flush()


def die(message):
    emit({"ok": False, "error": str(message)})
    raise SystemExit(0)


def token_path(ctx):
    """Absolute path of the on-disk token file (gemini-cli-specific default)."""
    return _shared_token_path(ctx, _TOKEN_FILENAME)


# ─── PKCE + auth URL ───────────────────────────────────────────────────────

def build_authorize_url(redirect_uri, state, challenge):
    # The official ``gemini`` CLI does NOT send ``prompt=consent`` or
    # ``include_granted_scopes=true``; including them can confuse Google's
    # refresh-token issuance logic for the public-but-unverified gemini-cli
    # OAuth client. Keep the request minimal: redirect + scope + PKCE +
    # state + offline access_type (required for a refresh_token).
    params = {
        "client_id": CLIENT_ID,
        "response_type": "code",
        "redirect_uri": redirect_uri,
        "scope": " ".join(SCOPES),
        "state": state,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "access_type": "offline",
    }
    return AUTH_URL + "?" + urllib.parse.urlencode(params)


# ─── token exchange ────────────────────────────────────────────────────────

def exchange_code(code, redirect_uri, verifier):
    status, data = post_form(
        TOKEN_URL,
        {
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
            "code_verifier": verifier,
        },
    )
    return status, data


def refresh_access_token(refresh_token):
    return _shared_refresh_access_token(
        TOKEN_URL, CLIENT_ID, CLIENT_SECRET, refresh_token
    )


def fetch_user_email(access_token):
    req = urllib.request.Request(
        USERINFO_URL,
        headers={"Authorization": f"Bearer {access_token}", "User-Agent": USER_AGENT},
    )
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as response:
            data = parse_json(response.read().decode("utf-8", "replace"))
            return data.get("email") or ""
    except Exception:
        return ""


def parse_json(raw):
    # Local shim so ``fetch_user_email`` can keep its old call site; the
    # canonical implementation now lives in ``google_oauth``. The behaviour
    # is identical (raw -> dict-or-fallback-error shape).
    try:
        value = json.loads(raw) if raw.strip() else {}
        return value if isinstance(value, dict) else {}
    except Exception:
        return {"error": "invalid_json", "error_description": raw[:500]}


# ─── Code Assist: loadCodeAssist + onboardUser ─────────────────────────────

def _code_assist_headers(access_token):
    return {
        "Authorization": f"Bearer {access_token}",
        "Content-Type": "application/json",
        "User-Agent": USER_AGENT,
        "X-Goog-Api-Client": X_GOOG_API_CLIENT,
        "Client-Metadata": json.dumps(CLIENT_METADATA, separators=(",", ":")),
    }


def _code_assist_body(include_tier=False, tier_id=None, mode=1):
    # mode=1 is the Code Assist mode 9router always sends; without it the
    # free-tier gemini-cli OAuth client often gets no project back.
    body = {"metadata": dict(CLIENT_METADATA), "mode": mode}
    if include_tier:
        body["tierId"] = tier_id or "legacy-tier"
    return body


def _pick_default_tier(payload):
    tiers = payload.get("allowedTiers")
    if isinstance(tiers, list):
        for tier in tiers:
            if isinstance(tier, dict) and tier.get("isDefault") is True:
                tid = tier.get("id")
                if isinstance(tid, str) and tid.strip():
                    return tid.strip()
    return "legacy-tier"


def load_code_assist_payload(access_token):
    """POST :loadCodeAssist and return the raw payload (or ``None`` on failure)."""
    status, data = post_json(
        LOAD_CODE_ASSIST_URL,
        _code_assist_body(),
        _code_assist_headers(access_token),
    )
    return data if status == 200 else None


def onboard_user(access_token, tier_id):
    """POST :onboardUser, polling until ``done=true``; return project_id."""
    for attempt in range(1, ONBOARD_MAX_ATTEMPTS + 1):
        status, data = post_json(
            ONBOARD_USER_URL,
            _code_assist_body(include_tier=True, tier_id=tier_id),
            _code_assist_headers(access_token),
        )
        if status != 200:
            return None
        if data.get("done") is True:
            return extract_cloudaicompanion_project(data)
        if attempt < ONBOARD_MAX_ATTEMPTS:
            time.sleep(ONBOARD_POLL_S)
    return None


def discover_project_id(access_token):
    """Try loadCodeAssist; on failure, fall back to onboardUser polling.

    Free-tier gemini-cli OAuth often returns no project (Google now marks
    free-tier as UNSUPPORTED_CLIENT for this OAuth client). Fallbacks, in
    order:
      1. ``CATALYST_CODE_GEMINI_CLI_PROJECT`` env override.
      2. Sibling Antigravity token file's ``project_id`` (same Google
         account often already has a working managed project via the
         Antigravity OAuth flow — verified: body.project alone works).
    """
    override = (os.environ.get("CATALYST_CODE_GEMINI_CLI_PROJECT") or "").strip()
    if override:
        return override
    payload = load_code_assist_payload(access_token)
    if payload is not None:
        project = extract_cloudaicompanion_project(payload)
        if project:
            return project
        tier = _pick_default_tier(payload)
        project = onboard_user(access_token, tier)
        if project:
            return project
    # Sibling Antigravity token (same user, different OAuth client) often
    # already holds a working managed project. Resolve the sibling path
    # from ``CATALYST_CODE_ANTIGRAVITY_PROJECT`` / the gemini-cli token
    # directory first, then fall back to the default global location. The
    # configured ``token_path`` is not in scope here (discover_project_id
    # is called from do_complete, which has ctx); callers pass it via the
    # GEMINI_CLI_PROJECT_DIR env var when they need a non-default layout.
    sibling_candidates = []
    project_dir = os.environ.get("CATALYST_CODE_OAUTH_DIR", "").strip()
    if project_dir:
        sibling_candidates.append(os.path.join(project_dir, "antigravity.json"))
    sibling_candidates.append(os.path.expanduser(
        "~/.config/catalyst-code/oauth/antigravity.json"
    ))
    for sibling in sibling_candidates:
        try:
            sib = read_token(sibling)
        except Exception:
            continue
        if sib:
            pid = str(sib.get("project_id") or "").strip()
            if pid:
                return pid
    return None


# ─── actions ───────────────────────────────────────────────────────────────

def do_login(ctx):
    """Build the authorize URL. The harness binds the loopback + opens the
    browser; we receive the ``code`` back in ``complete``."""
    verifier, challenge, state = make_pkce()
    redirect_uri = str(ctx.get("redirect_uri") or "").strip()
    if not redirect_uri:
        die("Gemini CLI OAuth requires a loopback redirect_uri (catcode binds this)")

    url = build_authorize_url(redirect_uri, state, challenge)
    emit(
        {
            "url": url,
            "flow": "web",
            "state": state,
            "pending": {"verifier": verifier},
            "message": (
                "Open the URL to authorize Gemini CLI. The token is then "
                "auto-discovered via loadCodeAssist and a real Cloud project "
                "is provisioned if needed."
            ),
        }
    )


def do_complete(ctx):
    code = str(ctx.get("code") or "").strip()
    if not code:
        die("no authorization code received from Gemini CLI")
    pending = ctx.get("pending") or {}
    verifier = str(pending.get("verifier") or "").strip()
    if not verifier:
        die("missing PKCE verifier in pending state; restart /login gemini-cli")
    redirect_uri = str(ctx.get("redirect_uri") or "").strip()
    if not redirect_uri:
        die("missing redirect_uri in complete context; restart /login gemini-cli")

    status, tokens = exchange_code(code, redirect_uri, verifier)
    if status != 200 or not tokens.get("access_token"):
        die("Gemini CLI token exchange failed: " + error_text(status, tokens))

    normalized = normalize_tokens(tokens)
    if not normalized:
        die("Gemini CLI token exchange returned no usable tokens")

    project_id = discover_project_id(normalized["access_token"])
    if project_id:
        normalized["project_id"] = project_id

    email = fetch_user_email(normalized["access_token"])
    if email:
        normalized["email"] = email

    atomic_write(token_path(ctx), normalized, prefix=_ATOMIC_WRITE_PREFIX)
    emit({"ok": True})


def do_token(ctx):
    path = token_path(ctx)
    handle = lock_for(path)
    try:
        token = read_token(path)
        if not token:
            emit({"access_token": None})
            return

        current = now()
        expires_at = int(token.get("expires_at") or 0)
        needs_refresh = (not token.get("access_token")) or (
            expires_at > 0 and expires_at - current <= REFRESH_LEAD_S
        )

        if needs_refresh and token.get("refresh_token"):
            # Another harness process may have refreshed while we waited for
            # the flock; always re-read before making a network request.
            current_token = read_token(path) or token
            current_exp = int(current_token.get("expires_at") or 0)
            if current_token.get("access_token") and (
                current_exp == 0 or current_exp - now() > REFRESH_LEAD_S
            ):
                token = current_token
            else:
                status, data = refresh_access_token(current_token["refresh_token"])
                if status != 200 or not data.get("access_token"):
                    emit({"access_token": None})
                    return
                rotated = normalize_tokens(
                    {
                        "access_token": data.get("access_token", ""),
                        "refresh_token": data.get("refresh_token")
                        or current_token.get("refresh_token", ""),
                        "expires_in": int(data.get("expires_in") or 0),
                        "scope": data.get("scope", current_token.get("scope", "")),
                        "token_type": data.get("token_type", "Bearer"),
                    }
                )
                if not rotated:
                    emit({"access_token": None})
                    return
                # Preserve project_id + email across rotations — those were
                # discovered once and remain valid for the lifetime of the
                # OAuth grant.
                rotated["project_id"] = current_token.get("project_id", "")
                rotated["email"] = current_token.get("email", "")
                atomic_write(path, rotated, prefix=_ATOMIC_WRITE_PREFIX)
                token = rotated

        access = token.get("access_token") or ""
        if not access:
            emit({"access_token": None})
            return

        headers = []
        project_id = str(token.get("project_id") or "").strip()
        if project_id:
            # CRITICAL: do NOT use x-goog-user-project. That Google consumer
            # header forces a Cloud Code Private API enablement check and
            # returns SERVICE_DISABLED on free-tier / managed projects.
            # 9router's gemini-cli executor never sends it — project goes in
            # the request body only. The harness adapter also accepts
            # x-code-assist-project / cloudaicompanion-project, which only
            # affect body.project resolution and do not trip the consumer
            # API gate. Verified end-to-end: body.project alone works;
            # x-goog-user-project → 403 SERVICE_DISABLED.
            headers.append(["x-code-assist-project", project_id])
        emit(
            {
                "access_token": access,
                "expires_at": int(token.get("expires_at") or 0),
                "headers": headers,
            }
        )
    finally:
        unlock(handle)


def do_clear(ctx):
    path = token_path(ctx)
    for candidate in (path, path + ".lock"):
        try:
            os.remove(candidate)
        except OSError:
            pass
    emit({"ok": True})


def now():
    # Local shim — action functions and ``do_token`` already use ``now()``
    # directly. Same implementation as ``google_oauth.now()``; kept local
    # so the per-script code reads naturally without a shared-module call.
    return int(time.time())


def main():
    try:
        raw = sys.stdin.read()
        ctx = json.loads(raw) if raw.strip() else {}
        if not isinstance(ctx, dict):
            die("OAuth context must be a JSON object")
        action = ctx.get("action", "")
        if action == "login":
            do_login(ctx)
        elif action == "complete":
            do_complete(ctx)
        elif action == "token":
            do_token(ctx)
        elif action == "clear":
            do_clear(ctx)
        else:
            die("unknown action: %r" % action)
    except SystemExit:
        raise
    except Exception as exc:
        die("Gemini CLI OAuth provider error: " + str(exc))


if __name__ == "__main__":
    main()
