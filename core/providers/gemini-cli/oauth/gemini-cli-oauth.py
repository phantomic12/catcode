#!/usr/bin/env python3
"""Gemini CLI (Google) OAuth + Code Assist project discovery.

The harness sends one JSON object on stdin with an ``action`` of ``login``,
``complete``, ``token``, or ``clear``. One JSON object is written to stdout.

This file deliberately uses only the Python standard library. The endpoint
names, client id, redirect URI, refresh grant, and loadCodeAssist metadata
mirror Google's open-source ``gemini`` CLI so the upstream Code Assist
gateway provisions a real ``cloudaicompanionProject`` for us.

Compared to the Antigravity plugin this one uses:

* a different public OAuth client (the open-source gemini-cli client);
* a simpler scope list (no cclog / experimentsandconfigs);
* the prod Code Assist host for chat (the daily host rejects gemini-cli
  traffic more often than antigravity traffic in practice);
* the gemini-cli loadCodeAssist fingerprint (google-api-nodejs-client UA
  + X-Goog-Api-Client + Client-Metadata with the IDE/PLATFORM/PLUGIN_TYPE
  numeric enums the gemini-cli binary actually sends).
"""

import base64
import hashlib
import json
import os
import secrets
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


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


# ─── harness I/O ───────────────────────────────────────────────────────────

def emit(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")))
    sys.stdout.flush()


def die(message):
    emit({"ok": False, "error": str(message)})
    raise SystemExit(0)


def now():
    return int(time.time())


# ─── HTTP helpers ──────────────────────────────────────────────────────────

def http_post(url, body, content_type, extra_headers=None):
    headers = {
        "Accept": "application/json",
        "Content-Type": content_type,
        "User-Agent": USER_AGENT,
    }
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(url, data=body, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as response:
            raw = response.read().decode("utf-8", "replace")
            return response.status, parse_json(raw)
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", "replace")
        return exc.code, parse_json(raw)
    except Exception as exc:
        return 0, {"error": "request_failed", "error_description": str(exc)}


def parse_json(raw):
    try:
        value = json.loads(raw) if raw.strip() else {}
        return value if isinstance(value, dict) else {}
    except Exception:
        return {"error": "invalid_json", "error_description": raw[:500]}


def post_form(url, fields, extra_headers=None):
    return http_post(
        url,
        urllib.parse.urlencode(fields).encode("utf-8"),
        "application/x-www-form-urlencoded",
        extra_headers,
    )


def post_json(url, payload, extra_headers=None):
    return http_post(
        url,
        json.dumps(payload, separators=(",", ":")).encode("utf-8"),
        "application/json",
        extra_headers,
    )


def error_text(status, data):
    return (
        data.get("error_description")
        or data.get("error")
        or ("network request failed" if status == 0 else f"HTTP {status}")
    )


# ─── on-disk token file ────────────────────────────────────────────────────

def token_path(ctx):
    return os.path.abspath(str(ctx.get("token_path") or "gemini-cli.json"))


def read_token(path):
    try:
        with open(path, encoding="utf-8") as handle:
            value = json.load(handle)
        return value if isinstance(value, dict) else None
    except (OSError, ValueError, TypeError):
        return None


def atomic_write(path, value):
    path = os.path.abspath(path)
    parent = os.path.dirname(path) or "."
    os.makedirs(parent, mode=0o700, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".gemini-cli-oauth-", dir=parent)
    try:
        try:
            os.fchmod(fd, 0o600)
        except AttributeError:
            pass
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(value, handle, separators=(",", ":"))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def lock_for(path):
    try:
        import fcntl
    except ImportError:
        return None
    handle = open(path + ".lock", "a+", encoding="utf-8")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
    except OSError:
        pass
    return handle


def unlock(handle):
    if handle is not None:
        try:
            handle.close()
        except OSError:
            pass


# ─── PKCE + auth URL ───────────────────────────────────────────────────────

def make_pkce():
    """Generate (verifier, challenge, state) for S256 PKCE."""
    verifier = base64.urlsafe_b64encode(secrets.token_bytes(48)).rstrip(b"=").decode("ascii")
    challenge = base64.urlsafe_b64encode(
        hashlib.sha256(verifier.encode("ascii")).digest()
    ).rstrip(b"=").decode("ascii")
    state = base64.urlsafe_b64encode(secrets.token_bytes(24)).rstrip(b"=").decode("ascii")
    return verifier, challenge, state


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
    status, data = post_form(
        TOKEN_URL,
        {
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
        },
    )
    return status, data


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


def normalize_tokens(tokens):
    """Coerce the raw OAuth response into the persistent shape on disk."""
    access = tokens.get("access_token") or ""
    refresh = tokens.get("refresh_token") or ""
    if not access and not refresh:
        return None
    expires_in = int(tokens.get("expires_in") or 0)
    return {
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": expires_in,
        "expires_at": now() + max(expires_in, 60),
        "scope": tokens.get("scope", ""),
        "token_type": tokens.get("token_type", "Bearer"),
    }


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


def _extract_project(payload):
    """Pull ``cloudaicompanionProject`` out of a loadCodeAssist / onboardUser response."""
    project = payload.get("cloudaicompanionProject")
    if isinstance(project, str) and project.strip():
        return project.strip()
    if isinstance(project, dict):
        nested = project.get("id")
        if isinstance(nested, str) and nested.strip():
            return nested.strip()
    nested = (payload.get("response") or {}).get("cloudaicompanionProject")
    if isinstance(nested, str) and nested.strip():
        return nested.strip()
    if isinstance(nested, dict):
        id_ = nested.get("id")
        if isinstance(id_, str) and id_.strip():
            return id_.strip()
    return None


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
            return _extract_project(data)
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
        project = _extract_project(payload)
        if project:
            return project
        tier = _pick_default_tier(payload)
        project = onboard_user(access_token, tier)
        if project:
            return project
    # Sibling Antigravity token (same user, different OAuth client) often
    # already holds a working managed project. Read it if present.
    try:
        sibling = os.path.join(os.path.dirname(os.path.abspath(
            # token_path is not in scope here; reconstruct from common layout.
            os.path.expanduser("~/.config/catalyst-code/oauth/antigravity.json")
        )), "antigravity.json") if False else os.path.expanduser(
            "~/.config/catalyst-code/oauth/antigravity.json"
        )
        sib = read_token(sibling)
        if sib:
            pid = str(sib.get("project_id") or "").strip()
            if pid:
                return pid
    except Exception:
        pass
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

    atomic_write(token_path(ctx), normalized)
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
                atomic_write(path, rotated)
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