#!/usr/bin/env python3
"""Antigravity (Google IDE) OAuth + Code Assist project discovery.

The harness sends one JSON object on stdin with an ``action`` of ``login``,
``complete``, ``token``, or ``clear``. One JSON object is written to stdout.

This file deliberately uses only the Python standard library. The endpoint
names, client id, redirect URI, refresh grant, and loadCodeAssist metadata
mirror the public Antigravity IDE (2.1.1, darwin/arm64) so the upstream
Code Assist gateway provisions a real ``cloudaicompanionProject`` for us.

Flow
----
login    PKCE + Authorization Code → harness binds loopback → opens browser
         → captures ``code`` → we exchange + run ``loadCodeAssist`` →
         write ``token.json`` containing access + refresh + project_id.
token    Return a fresh ``access_token`` (refresh if near expiry) and a
         ``x-goog-user-project`` header carrying the cached ``project_id``
         so the harness's Google Code Assist adapter routes to the user's
         real Antigravity project (not the freemium shared one).
clear    Delete the on-disk token file.
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


# ─── Antigravity IDE public OAuth client ────────────────────────────────────
# Public client_id / client_secret shipped in the open-source Antigravity IDE.
# Both values are intentionally public — every Antigravity IDE install carries
# the same pair — and are reused here unchanged.
CLIENT_ID = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
CLIENT_SECRET = "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"

AUTH_URL = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URL = "https://oauth2.googleapis.com/token"
USERINFO_URL = "https://www.googleapis.com/oauth2/v1/userinfo"

# Scopes the Antigravity IDE requests. ``cclog`` + ``experimentsandconfigs``
# are Antigravity-specific and are required for Code Assist provisioning.
# Google OAuth requires ``/oauth2callback`` (not arbitrary paths) for the
# Antigravity OAuth client — only this path is registered as a loopback
# redirect URI for ``http://127.0.0.1:<port>`` in the client's Google Cloud
# console entry. Using ``/callback`` makes Google reject the request as a
# non-compliant redirect URI ("doesn't comply with Google's OAuth 2.0
# policy for keeping apps secure"). We mirror the path the Antigravity IDE
# binary uses.
REDIRECT_PATH = "/oauth2callback"

# Scopes the Antigravity IDE requests. ``openid`` is intentionally omitted
# (same reason as gemini-cli — it triggers Google's unverified-app gate on
# ``cclog`` / ``experimentsandconfigs`` requests). ``userinfo.email`` +
# ``userinfo.profile`` are sufficient for the loadCodeAssist user lookup.
SCOPES = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
]

# Env var that pins the Antigravity ``cloudaicompanionProject`` to a
# specific GCP project the user owns and can enable Cloud Code Private
# API on. Use this when the auto-provisioned project (e.g.
# ``synthetic-expanse-sxhhm``) is unusable — e.g. the user is not a
# member of the Google-managed project so they cannot enable the API
# from the Cloud Console. When unset, the script uses whatever
# ``loadCodeAssist`` / ``onboardUser`` returns.
ANTIGRAVITY_PROJECT_ENV = "CATALYST_CODE_ANTIGRAVITY_PROJECT"

# Antigravity IDE fingerprints (must match what the IDE actually sends —
# Google's backend fingerprints these headers and silently refuses to
# provision a project if they look wrong).
USER_AGENT = "antigravity/ide/2.1.1 darwin/arm64"

# Project discovery stays on PROD — the daily host rejects loadCodeAssist /
# onboardUser calls. Only chat traffic uses the daily host (via base_url in
# plugin.json).
LOAD_CODE_ASSIST_URL = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
ONBOARD_USER_URL = "https://cloudcode-pa.googleapis.com/v1internal:onboardUser"

# Numeric enum values that the Code Assist backend fingerprints. Values
# captured from a real Antigravity IDE 2.1.1 / darwin-arm64 install. Anything
# else triggers silent provisioning failure (no cloudaicompanionProject in
# the response, and onboardUser's poll never reaches ``done=true``).
#   IDE_TYPE_ANTIGRAVITY = 9
#   PLATFORM_DARWIN_ARM64 = 2
#   PLUGIN_TYPE_GEMINI = 2
CLIENT_METADATA = {"ideType": 9, "platform": 2, "pluginType": 2}

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
    return os.path.abspath(str(ctx.get("token_path") or "antigravity.json"))


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
    fd, tmp = tempfile.mkstemp(prefix=".antigravity-oauth-", dir=parent)
    try:
        try:
            os.fchmod(fd, 0o600)
        except AttributeError:
            pass  # Windows has no POSIX mode bits
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


def build_authorize_url(redirect_uri, state, challenge, extra=None):
    # The Antigravity IDE binary does not include ``prompt=consent`` or
    # ``include_granted_scopes=true``; including either can confuse Google's
    # refresh-token issuance for the Antigravity OAuth client. Keep the
    # request minimal: redirect + scope + PKCE + state + offline.
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
    if extra:
        params.update(extra)
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
    }


def _code_assist_body(include_tier=False, tier_id=None):
    body = {"metadata": dict(CLIENT_METADATA)}
    if include_tier:
        body["tierId"] = tier_id or "legacy-tier"
    return body



def load_code_assist(access_token):
    """POST :loadCodeAssist, return ``cloudaicompanionProject`` id or ``None``."""
    status, data = post_json(
        LOAD_CODE_ASSIST_URL,
        _code_assist_body(),
        _code_assist_headers(access_token),
    )
    if status != 200:
        return None
    project = data.get("cloudaicompanionProject")
    if isinstance(project, str) and project.strip():
        return project.strip()
    if isinstance(project, dict):
        nested = project.get("id")
        if isinstance(nested, str) and nested.strip():
            return nested.strip()
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
            response = data.get("response") or {}
            project = response.get("cloudaicompanionProject")
            if isinstance(project, str) and project.strip():
                return project.strip()
            if isinstance(project, dict):
                nested = project.get("id")
                if isinstance(nested, str) and nested.strip():
                    return nested.strip()
            return None
        if attempt < ONBOARD_MAX_ATTEMPTS:
            time.sleep(ONBOARD_POLL_S)
    return None


def discover_project_id(access_token):
    """Resolve the Antigravity ``cloudaicompanionProject``.

    Priority:
      1. ``CATALYST_CODE_ANTIGRAVITY_PROJECT`` env override (escape hatch
         when the auto-provisioned project is a Google-managed one the
         user does not own and so cannot enable Cloud Code Private API on).
      2. ``loadCodeAssist`` — returns the existing project if the user is
         already onboarded, otherwise ``onboardUser`` polls until done.
    """
    override = os.environ.get(ANTIGRAVITY_PROJECT_ENV, "").strip()
    if override:
        return override
    status, data = post_json(
        LOAD_CODE_ASSIST_URL,
        _code_assist_body(),
        _code_assist_headers(access_token),
    )
    if status != 200:
        return None
    project = data.get("cloudaicompanionProject")
    if isinstance(project, str) and project.strip():
        return project.strip()
    if isinstance(project, dict):
        nested = project.get("id")
        if isinstance(nested, str) and nested.strip():
            return nested.strip()
    tier = _pick_default_tier(data)
    return onboard_user(access_token, tier)


# ─── actions ───────────────────────────────────────────────────────────────

def do_login(ctx):
    """Build the authorize URL. The harness binds the loopback + opens the
    browser; we receive the ``code`` back in ``complete``."""
    verifier, challenge, state = make_pkce()
    redirect_uri = str(ctx.get("redirect_uri") or "").strip()
    if not redirect_uri:
        die("Antigravity OAuth requires a loopback redirect_uri (catcode binds this)")

    url = build_authorize_url(redirect_uri, state, challenge)
    emit(
        {
            "url": url,
            "flow": "web",
            "state": state,
            "pending": {"verifier": verifier},
            "message": (
                "Open the URL to authorize Antigravity. The token is then "
                "auto-discovered via loadCodeAssist and a real Cloud project "
                "is provisioned if needed."
            ),
        }
    )


def do_complete(ctx):
    code = str(ctx.get("code") or "").strip()
    if not code:
        die("no authorization code received from Antigravity")
    pending = ctx.get("pending") or {}
    verifier = str(pending.get("verifier") or "").strip()
    if not verifier:
        die("missing PKCE verifier in pending state; restart /login antigravity")
    redirect_uri = str(ctx.get("redirect_uri") or "").strip()
    if not redirect_uri:
        die("missing redirect_uri in complete context; restart /login antigravity")

    status, tokens = exchange_code(code, redirect_uri, verifier)
    if status != 200 or not tokens.get("access_token"):
        die("Antigravity token exchange failed: " + error_text(status, tokens))

    normalized = normalize_tokens(tokens)
    if not normalized:
        die("Antigravity token exchange returned no usable tokens")

    project_id = discover_project_id(normalized["access_token"])
    if project_id:
        normalized["project_id"] = project_id
    # Re-check the env override after discover_project_id — if set, the
    # auto-provisioned project would otherwise be persisted and chat would
    # be stuck on SERVICE_DISABLED.
    override = os.environ.get(ANTIGRAVITY_PROJECT_ENV, "").strip()
    if override:
        normalized["project_id"] = override

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
            # Put the project in the request body only (via the harness
            # adapter reading x-code-assist-project). Verified: body.project
            # alone works for gemini-cli; x-goog-user-project → 403.
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
        die("Antigravity OAuth provider error: " + str(exc))


if __name__ == "__main__":
    main()
