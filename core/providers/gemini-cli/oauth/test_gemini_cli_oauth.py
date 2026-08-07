#!/usr/bin/env python3
"""End-to-end tests for the Gemini CLI OAuth provider script.

Stdlib-only. The provider script is exercised in-process by rewriting
its URL constants to point at a local mock HTTP server and ``exec``'ing
the patched source in a namespace with ``__name__ = "__main__"`` and
``__file__`` pointing at the real script (so the relative
``../../_shared/google_oauth.py`` import still resolves).

The harness JSON contract (per the script's stdin/stdout) is:

    stdin  :: {"action": "login"|"complete"|"token"|"clear", ...}
    stdout :: action-specific JSON object (see the script docstring)

These tests cover the contract end-to-end against a mock Google auth
server so we can verify URL shape, PKCE, refresh-token rotation,
the sibling-Antigravity-token fallback for free-tier users, and the
on-disk file mode without needing real Gemini CLI / Google credentials.
"""

import base64
import contextlib
import hashlib
import http.server
import io
import json
import os
import socketserver
import stat
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse


HERE = os.path.dirname(os.path.abspath(__file__))
GEMINI_CLI_SCRIPT = os.path.abspath(os.path.join(HERE, "gemini-cli-oauth.py"))
SHARED_DIR = os.path.abspath(os.path.join(HERE, "..", "..", "_shared"))

GEMINI_CLI_CLIENT_ID = (
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
)

URL_REWRITES = {
    "https://accounts.google.com/o/oauth2/v2/auth": "http://127.0.0.1:{port}/auth",
    "https://oauth2.googleapis.com/token": "http://127.0.0.1:{port}/token",
    "https://www.googleapis.com/oauth2/v1/userinfo": "http://127.0.0.1:{port}/userinfo",
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist": (
        "http://127.0.0.1:{port}/loadCodeAssist"
    ),
    "https://cloudcode-pa.googleapis.com/v1internal:onboardUser": (
        "http://127.0.0.1:{port}/onboardUser"
    ),
}


# ─── mock HTTP server ──────────────────────────────────────────────────────


class MockGoogle:
    """Stand-in for ``accounts.google.com`` + ``*.googleapis.com`` endpoints."""

    def __init__(self):
        self.handlers = {}
        self.request_log = []
        self._server = None
        self._thread = None
        self.port = None

    def route(self, path):
        def deco(fn):
            self.handlers[path] = fn
            return fn
        return deco

    def _parse_body(self, raw, headers):
        ct = ""
        for k, v in headers.items():
            if k.lower() == "content-type":
                ct = v or ""
                break
        if not raw:
            return {}
        if "json" in ct.lower():
            try:
                value = json.loads(raw)
                return value if isinstance(value, dict) else {}
            except Exception:
                return {}
        try:
            return dict(urllib.parse.parse_qsl(raw, keep_blank_values=True))
        except Exception:
            return {}

    def _dispatch(self, path, raw, headers):
        body = self._parse_body(raw, headers)
        normalised = {}
        for k, v in headers.items():
            normalised[k] = v
            normalised[k.lower()] = v
        self.request_log.append({"path": path, "body": body, "headers": normalised})
        handler = self.handlers.get(path)
        if handler is None:
            return 404, {"error": "not_found", "path": path}
        return handler(body, normalised)

    def start(self):
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args, **kwargs):
                pass

            def _handle(self):
                length = int(self.headers.get("Content-Length", "0") or "0")
                raw = self.rfile.read(length).decode("utf-8") if length else ""
                status, payload = outer._dispatch(self.path, raw, dict(self.headers))
                body = json.dumps(payload).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                self._handle()

            def do_GET(self):
                # ``fetch_user_email`` GETs ``/userinfo``.
                self._handle()

        class TCPServer(socketserver.TCPServer):
            allow_reuse_address = True

        self._server = TCPServer(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(
            target=self._server.serve_forever, daemon=True
        )
        self._thread.start()

    def stop(self):
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
            self._thread = None


# ─── script runner ─────────────────────────────────────────────────────────


def _patch_source(src, port, poll_sleep=0):
    for old, tmpl in URL_REWRITES.items():
        src = src.replace(old, tmpl.format(port=port))
    src = src.replace("ONBOARD_POLL_S = 2", f"ONBOARD_POLL_S = {int(poll_sleep)}")
    return src


def run_script(ctx, port=None):
    with open(GEMINI_CLI_SCRIPT, encoding="utf-8") as handle:
        src = handle.read()
    if port is not None:
        src = _patch_source(src, port)
    saved_stdin, saved_stdout = sys.stdin, sys.stdout
    out = io.StringIO()
    try:
        sys.stdin = io.StringIO(json.dumps(ctx))
        sys.stdout = out
        ns = {"__name__": "__main__", "__file__": GEMINI_CLI_SCRIPT}
        try:
            exec(compile(src, GEMINI_CLI_SCRIPT, "exec"), ns)
        except SystemExit:
            pass
    finally:
        sys.stdin, sys.stdout = saved_stdin, saved_stdout
    raw = out.getvalue()
    return json.loads(raw) if raw.strip() else {}


@contextlib.contextmanager
def temp_env(**overrides):
    saved = {}
    for key, value in overrides.items():
        saved[key] = os.environ.get(key)
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = value
    try:
        yield
    finally:
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def _pkce_challenge(verifier):
    digest = hashlib.sha256(verifier.encode("ascii")).digest()
    return base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")


# ─── tests ─────────────────────────────────────────────────────────────────


class GeminiCliOAuthTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.token_path = os.path.join(self.tmp.name, "gemini-cli.json")
        self.mock = MockGoogle()

    def tearDown(self):
        self.mock.stop()
        self.tmp.cleanup()

    # ── login ────────────────────────────────────────────────────────────

    def test_login_pkce_s256_url_shape(self):
        redirect_uri = "http://127.0.0.1:8085/oauth2callback"
        ctx = {"action": "login", "redirect_uri": redirect_uri}
        out = run_script(ctx)

        self.assertEqual(out.get("flow"), "web")
        self.assertIn("state", out)
        self.assertIn("pending", out)
        self.assertIn("verifier", out["pending"])
        self.assertIn("url", out)

        parsed = urllib.parse.urlparse(out["url"])
        params = urllib.parse.parse_qs(parsed.query)
        self.assertEqual(parsed.scheme, "https")
        self.assertEqual(parsed.netloc, "accounts.google.com")
        self.assertEqual(parsed.path, "/o/oauth2/v2/auth")

        self.assertEqual(params.get("client_id", [""])[0], GEMINI_CLI_CLIENT_ID)
        self.assertEqual(params.get("response_type", [""])[0], "code")
        self.assertEqual(params.get("redirect_uri", [""])[0], redirect_uri)
        self.assertEqual(params.get("state", [""])[0], out["state"])

        # gemini-cli scopes are exactly the 3 cloud-platform ones — no
        # openid, no cclog, no experimentsandconfigs.
        scopes = set((params.get("scope", [""])[0]).split())
        expected = {
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/userinfo.profile",
        }
        self.assertEqual(scopes, expected)

        # PKCE S256.
        self.assertEqual(params.get("code_challenge_method", [""])[0], "S256")
        verifier = out["pending"]["verifier"]
        self.assertEqual(
            params.get("code_challenge", [""])[0], _pkce_challenge(verifier)
        )

        self.assertEqual(params.get("access_type", [""])[0], "offline")
        # Mirror the official ``gemini`` CLI: no ``prompt=consent``, no
        # ``include_granted_scopes=true`` — both can confuse refresh-token
        # issuance for this public-but-unverified OAuth client.
        self.assertNotIn("prompt", params)
        self.assertNotIn("include_granted_scopes", params)

    def test_no_openid_in_scope(self):
        """Regression: scope MUST NOT contain ``openid``.

        Including ``openid`` triggers Google's "unverified app" rejection
        for this public-but-unverified OAuth client. The gemini-cli project
        deliberately omits it; ``userinfo.email`` + ``userinfo.profile``
        are sufficient for the loadCodeAssist user-info lookup.
        """
        out = run_script(
            {"action": "login", "redirect_uri": "http://127.0.0.1:8085/oauth2callback"}
        )
        parsed = urllib.parse.urlparse(out["url"])
        params = urllib.parse.parse_qs(parsed.query)
        scopes = set((params.get("scope", [""])[0]).split())
        self.assertNotIn("openid", scopes)
        self.assertNotIn(
            "https://www.googleapis.com/auth/openid", scopes
        )
        # And the gemini-cli-specific scopes must not appear (those belong
        # to Antigravity, not gemini-cli).
        self.assertNotIn(
            "https://www.googleapis.com/auth/cclog", scopes
        )
        self.assertNotIn(
            "https://www.googleapis.com/auth/experimentsandconfigs", scopes
        )

    # ── complete ─────────────────────────────────────────────────────────

    def test_complete_persists_token_with_sibling_project_fallback(self):
        """Free-tier loadCodeAssist returns UNSUPPORTED_CLIENT (no project).

        The script must fall back to reading the sibling antigravity.json
        file (same user, different OAuth client, often already has a
        working managed project) and persist its ``project_id``.
        """
        # Place the sibling antigravity token in a tempdir under HOME so
        # the script's default ``~/.config/catalyst-code/oauth/antigravity.json``
        # lookup (via ``os.path.expanduser``) resolves without polluting
        # the real home directory.
        oauth_dir = os.path.join(self.tmp.name, ".config", "catalyst-code", "oauth")
        os.makedirs(oauth_dir, exist_ok=True)
        sibling_path = os.path.join(oauth_dir, "antigravity.json")
        with open(sibling_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "access_token": "sibling-access",
                    "refresh_token": "sibling-refresh",
                    "project_id": "sibling-project",
                    "email": "sibling@example.com",
                },
                handle,
            )
        os.chmod(sibling_path, 0o600)

        # Override HOME so the ``~/.config/...`` expansion lands in
        # our tempdir.
        home = self.tmp.name
        with temp_env(HOME=home, USERPROFILE=home, CATALYST_CODE_OAUTH_DIR=""):
            self.mock.start()

            @self.mock.route("/token")
            def token(body, _headers):
                return 200, {
                    "access_token": "gemini-access",
                    "refresh_token": "gemini-refresh",
                    "expires_in": 3600,
                    "token_type": "Bearer",
                }

            @self.mock.route("/loadCodeAssist")
            def load(_body, _headers):
                # Free-tier: no allowedTiers, no cloudaicompanionProject —
                # the script must treat this as UNSUPPORTED_CLIENT and
                # proceed to onboard (also returns nothing) and then to
                # the sibling lookup.
                return 200, {"error": {"code": 400, "message": "UNSUPPORTED_CLIENT"}}

            @self.mock.route("/onboardUser")
            def onboard(_body, _headers):
                return 200, {"done": False}

            @self.mock.route("/userinfo")
            def userinfo(_body, _headers):
                return 200, {"email": ""}

            token_path = os.path.join(oauth_dir, "gemini-cli.json")
            ctx = {
                "action": "complete",
                "code": "fake-code",
                "pending": {"verifier": "fake-verifier"},
                "redirect_uri": "http://127.0.0.1:8085/oauth2callback",
                "token_path": token_path,
            }
            out = run_script(ctx, port=self.mock.port)

        self.assertEqual(out, {"ok": True})

        with open(token_path, encoding="utf-8") as handle:
            token = json.load(handle)
        self.assertEqual(token["access_token"], "gemini-access")
        self.assertEqual(token["project_id"], "sibling-project")
        self.assertEqual(
            stat.S_IMODE(os.stat(token_path).st_mode), 0o600
        )

    def test_complete_falls_back_to_sibling_via_env_dir(self):
        """``CATALYST_CODE_OAUTH_DIR`` overrides the sibling lookup dir.

        Same fallback as above, but via the env var instead of relying on
        ``$HOME`` — useful for sandboxed installs where ``~/.config/`` is
        read-only or points somewhere weird.
        """
        oauth_dir = os.path.join(self.tmp.name, "custom", "oauth")
        os.makedirs(oauth_dir, exist_ok=True)
        sibling_path = os.path.join(oauth_dir, "antigravity.json")
        with open(sibling_path, "w", encoding="utf-8") as handle:
            json.dump(
                {"access_token": "x", "project_id": "env-dir-proj"},
                handle,
            )

        self.mock.start()

        @self.mock.route("/token")
        def token(_body, _headers):
            return 200, {
                "access_token": "env-access",
                "refresh_token": "env-refresh",
                "expires_in": 3600,
                "token_type": "Bearer",
            }

        @self.mock.route("/loadCodeAssist")
        def load(_body, _headers):
            return 200, {}

        @self.mock.route("/onboardUser")
        def onboard(_body, _headers):
            return 200, {"done": False}

        @self.mock.route("/userinfo")
        def userinfo(_body, _headers):
            return 200, {"email": ""}

        token_path = os.path.join(oauth_dir, "gemini-cli.json")
        ctx = {
            "action": "complete",
            "code": "fake-code",
            "pending": {"verifier": "fake-verifier"},
            "redirect_uri": "http://127.0.0.1:8085/oauth2callback",
            "token_path": token_path,
        }
        with temp_env(CATALYST_CODE_OAUTH_DIR=oauth_dir):
            out = run_script(ctx, port=self.mock.port)

        self.assertEqual(out, {"ok": True})

        with open(token_path, encoding="utf-8") as handle:
            token = json.load(handle)
        self.assertEqual(token["project_id"], "env-dir-proj")

    # ── token ────────────────────────────────────────────────────────────

    def test_token_refresh_preserves_project_id_and_email(self):
        now = int(time.time())
        seed = {
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_in": 60,
            "expires_at": now + 60,
            "scope": "",
            "token_type": "Bearer",
            "project_id": "preserved-project",
            "email": "preserved@example.com",
        }
        with open(self.token_path, "w", encoding="utf-8") as handle:
            json.dump(seed, handle)
        os.chmod(self.token_path, 0o600)

        self.mock.start()

        @self.mock.route("/token")
        def token(body, _headers):
            self.assertEqual(body.get("grant_type"), "refresh_token")
            self.assertEqual(body.get("refresh_token"), "old-refresh")
            return 200, {
                "access_token": "new-access",
                "expires_in": 3600,
                "token_type": "Bearer",
            }

        out = run_script(
            {"action": "token", "token_path": self.token_path},
            port=self.mock.port,
        )

        self.assertEqual(out["access_token"], "new-access")
        self.assertEqual(out["expires_at"], int(time.time()) + 3600)
        self.assertEqual(
            out["headers"],
            [["x-code-assist-project", "preserved-project"]],
        )

        # Must not regress to x-goog-user-project.
        header_names = {h[0] for h in out["headers"]}
        self.assertNotIn("x-goog-user-project", header_names)

        with open(self.token_path, encoding="utf-8") as handle:
            rotated = json.load(handle)
        self.assertEqual(rotated["access_token"], "new-access")
        self.assertEqual(rotated["refresh_token"], "old-refresh")
        self.assertEqual(rotated["project_id"], "preserved-project")
        self.assertEqual(rotated["email"], "preserved@example.com")
        self.assertEqual(
            stat.S_IMODE(os.stat(self.token_path).st_mode), 0o600
        )

    def test_token_returns_null_when_no_file(self):
        out = run_script({"action": "token", "token_path": self.token_path})
        self.assertEqual(out, {"access_token": None})

    # ── clear ────────────────────────────────────────────────────────────

    def test_clear_removes_token_file(self):
        with open(self.token_path, "w", encoding="utf-8") as handle:
            json.dump({"access_token": "x"}, handle)
        with open(self.token_path + ".lock", "w", encoding="utf-8") as handle:
            handle.write("")

        out = run_script({"action": "clear", "token_path": self.token_path})
        self.assertEqual(out, {"ok": True})

        self.assertFalse(os.path.exists(self.token_path))
        self.assertFalse(os.path.exists(self.token_path + ".lock"))


if __name__ == "__main__":
    unittest.main()