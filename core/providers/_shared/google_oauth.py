#!/usr/bin/env python3
"""Shared helpers for Google OAuth providers (Antigravity, Gemini CLI).

Stdlib-only — both provider scripts pull HTTP wrappers, PKCE, token I/O,
``cloudaicompanionProject`` extraction, and the refresh-grant helper from
here so the wire-level behaviour stays in lock-step.

Vendor constants (CLIENT_ID / CLIENT_SECRET / SCOPES / USER_AGENT / URLs /
CLIENT_METADATA) stay in each provider script; only the URL-shaped,
provider-neutral pieces live here. ``build_authorize_url``,
``discover_project_id``, ``exchange_code``, ``fetch_user_email``, and the
four action functions (``do_login`` / ``do_complete`` / ``do_token`` /
``do_clear``) also stay per-script because they bind to script-specific
scopes, env overrides, or sibling-token fallbacks.

Import pattern (top of each provider script)::

    import sys, os
    _HERE = os.path.dirname(os.path.abspath(__file__))
    sys.path.insert(0, os.path.abspath(os.path.join(_HERE, "..", "..", "_shared")))
    from google_oauth import (...)
"""

import base64
import hashlib
import json
import os
import secrets
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


# Outbound User-Agent used when the caller does not override via
# ``extra_headers``. Per-script wrappers (``exchange_code``,
# ``fetch_user_email``) attach their own UA via ``extra_headers`` for
# requests where Google's backend fingerprints the header (token
# endpoint, userinfo). Code-Assist calls always carry the script UA in
# ``_code_assist_headers``, so they are unaffected.
DEFAULT_USER_AGENT = "catalyst-code-google-oauth/1.0"


def now():
    return int(time.time())


# ─── HTTP helpers ──────────────────────────────────────────────────────────

def http_post(url, body, content_type, extra_headers=None, timeout=30):
    headers = {
        "Accept": "application/json",
        "Content-Type": content_type,
        "User-Agent": DEFAULT_USER_AGENT,
    }
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(url, data=body, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
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


def post_form(url, fields, extra_headers=None, timeout=30):
    return http_post(
        url,
        urllib.parse.urlencode(fields).encode("utf-8"),
        "application/x-www-form-urlencoded",
        extra_headers,
        timeout=timeout,
    )


def post_json(url, payload, extra_headers=None, timeout=30):
    return http_post(
        url,
        json.dumps(payload, separators=(",", ":")).encode("utf-8"),
        "application/json",
        extra_headers,
        timeout=timeout,
    )


def error_text(status, data):
    return (
        data.get("error_description")
        or data.get("error")
        or ("network request failed" if status == 0 else f"HTTP {status}")
    )


# ─── on-disk token file ────────────────────────────────────────────────────

def token_path(ctx, default_name):
    """Resolve the absolute path of the on-disk token file.

    The harness always passes ``token_path`` in the action context; the
    per-script ``default_name`` is only a fallback for ad-hoc invocations
    where the field is missing.
    """
    return os.path.abspath(str(ctx.get("token_path") or default_name))


def read_token(path):
    try:
        with open(path, encoding="utf-8") as handle:
            value = json.load(handle)
        return value if isinstance(value, dict) else None
    except (OSError, ValueError, TypeError):
        return None


def atomic_write(path, value, prefix=".google-oauth-"):
    """Atomic JSON write of ``value`` to ``path``.

    Uses ``mkstemp`` + ``fsync`` + ``rename`` so a crash mid-write can
    never leave a truncated token file. The ``prefix`` parameter lets
    per-script callers keep their existing temp-file marker
    (``.antigravity-oauth-`` / ``.gemini-cli-oauth-``) so staging and
    cleanup can identify which provider owns a stale temp.
    """
    path = os.path.abspath(path)
    parent = os.path.dirname(path) or "."
    os.makedirs(parent, mode=0o700, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=prefix, dir=parent)
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
    """Acquire an exclusive ``flock`` on ``path + ".lock"``.

    Returns a file handle the caller must keep alive (and pass to
    ``unlock``) to hold the lock. POSIX-only: on platforms without
    ``fcntl`` (Windows) returns ``None`` and the lock is silently
    skipped — fine for our use case since the harness only runs these
    scripts on macOS / Linux.
    """
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


# ─── PKCE ──────────────────────────────────────────────────────────────────

def make_pkce():
    """Generate ``(verifier, challenge, state)`` for S256 PKCE."""
    verifier = base64.urlsafe_b64encode(secrets.token_bytes(48)).rstrip(b"=").decode("ascii")
    challenge = base64.urlsafe_b64encode(
        hashlib.sha256(verifier.encode("ascii")).digest()
    ).rstrip(b"=").decode("ascii")
    state = base64.urlsafe_b64encode(secrets.token_bytes(24)).rstrip(b"=").decode("ascii")
    return verifier, challenge, state


# ─── token exchange ────────────────────────────────────────────────────────

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


def refresh_access_token(token_url, client_id, client_secret, refresh_token):
    """POST ``grant_type=refresh_token``; return ``(status, dict)``."""
    return post_form(
        token_url,
        {
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": client_id,
            "client_secret": client_secret,
        },
    )


# ─── Code Assist: cloudaicompanionProject extraction ───────────────────────

def extract_cloudaicompanion_project(payload):
    """Pull ``cloudaicompanionProject`` out of a Code Assist response.

    Handles both shapes Google's Code Assist gateway returns:

    * top-level: ``{"cloudaicompanionProject": "abc"}`` or
      ``{"cloudaicompanionProject": {"id": "abc"}}`` (``loadCodeAssist``)
    * nested under ``response``:
      ``{"response": {"cloudaicompanionProject": "abc"}}``
      (``onboardUser`` final ``done=true`` payload)

    Returns the project id string, or ``None`` if no project is present.
    """
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
        inner_id = nested.get("id")
        if isinstance(inner_id, str) and inner_id.strip():
            return inner_id.strip()
    return None
