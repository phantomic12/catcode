#!/usr/bin/env python3
"""End-to-end tests for the Antigravity OAuth provider script.

Stdlib-only (``unittest``, ``http.server``, ``threading``, ``tempfile``,
``json``, ``urllib``, ``contextlib``, ``base64``, ``hashlib``,
``stat``, ``io``, ``os``, ``sys``). The provider script is exercised
in-process by rewriting its URL constants to point at a local mock HTTP
server and ``exec``'ing the patched source in a namespace with
``__file__`` set so the relative ``../../_shared/google_oauth.py``
import still resolves.

The harness JSON contract (per the script's stdin/stdout) is:

    stdin  :: {"action": "login"|"complete"|"token"|"clear", ...}
    stdout :: action-specific JSON object (see the script docstring)

These tests cover the contract end-to-end against a mock Google auth
server so we can verify URL shape, PKCE, refresh-token rotation,
project-id discovery + overrides, and the on-disk file mode without
needing real Antigravity / Google credentials.
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
ANTIGRAVITY_SCRIPT = os.path.abspath(os.path.join(HERE, "antigravity-oauth.py"))
SHARED_DIR = os.path.abspath(os.path.join(HERE, "..", "..", "_shared"))

ANTIGRAVITY_CLIENT_ID = (
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
)

# Wire-level URLs the script hits. We rewrite each constant in the script
# source to point at the local mock server so urlopen() stays a 127.0.0.1
# call. The path segments are kept verbatim so the test handlers can
# distinguish /token from /loadCodeAssist etc.
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
    """Stand-in for ``accounts.google.com`` + ``*.googleapis.com`` endpoints.

    Each test registers a handler per URL path. Handlers receive the parsed
    request body (dict) and headers (dict); return ``(status, dict_body)``.
    All requests are also appended to ``request_log`` so tests can assert
    on call counts, headers, and bodies after the fact.
    """

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
        # Normalise header keys so handlers can do case-insensitive lookups.
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
                pass  # silence stderr noise

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
                # ``fetch_user_email`` uses GET on ``/userinfo`` via
                # ``urllib.request.Request(USERINFO_URL, headers=...)``.
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
    """Rewrite URL constants + zero out ONBOARD_POLL_S for fast tests."""
    for old, tmpl in URL_REWRITES.items():
        src = src.replace(old, tmpl.format(port=port))
    src = src.replace("ONBOARD_POLL_S = 2", f"ONBOARD_POLL_S = {int(poll_sleep)}")
    return src


def run_script(ctx, port=None):
    """Execute one action against the script and return its JSON stdout.

    The script's ``die()`` helper raises ``SystemExit(0)`` after emitting
    an ``{"ok": false, "error": ...}`` envelope — we swallow that so a
    scripted error doesn't abort the whole test process.
    """
    with open(ANTIGRAVITY_SCRIPT, encoding="utf-8") as handle:
        src = handle.read()
    if port is not None:
        src = _patch_source(src, port)
    saved_stdin, saved_stdout = sys.stdin, sys.stdout
    out = io.StringIO()
    try:
        sys.stdin = io.StringIO(json.dumps(ctx))
        sys.stdout = out
        # ``__name__`` must be ``"__main__"`` so the script's
        # ``if __name__ == "__main__": main()`` block dispatches the
        # action — same wiring as ``python antigravity-oauth.py``.
        ns = {"__name__": "__main__", "__file__": ANTIGRAVITY_SCRIPT}
        try:
            exec(compile(src, ANTIGRAVITY_SCRIPT, "exec"), ns)
        except SystemExit:
            pass
    finally:
        sys.stdin, sys.stdout = saved_stdin, saved_stdout
    raw = out.getvalue()
    return json.loads(raw) if raw.strip() else {}


@contextlib.contextmanager
def temp_env(**overrides):
    """Snapshot + restore os.environ for the duration of the block."""
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
    """SHA256(verifier) -> base64url-no-padding, matching ``google_oauth.make_pkce``."""
    digest = hashlib.sha256(verifier.encode("ascii")).digest()
    return base64.urlsafe_b64encode(digest).rstrip(b"=").decode("ascii")


# ─── tests ─────────────────────────────────────────────────────────────────


class AntigravityOAuthTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.token_path = os.path.join(self.tmp.name, "antigravity.json")
        self.mock = MockGoogle()

    def tearDown(self):
        self.mock.stop()
        self.tmp.cleanup()

    def test_login_pkce_s256_url_shape(self):
        redirect_uri = "http://127.0.0.1:8085/oauth2callback"
        ctx = {"action": "login", "redirect_uri": redirect_uri}
        out = run_script(ctx)

        # Login envelope
        self.assertEqual(out.get("flow"), "web")
        self.assertIn("state", out)
        self.assertIn("pending", out)
        self.assertIn("verifier", out["pending"])
        self.assertIn("url", out)

        # URL shape
        parsed = urllib.parse.urlparse(out["url"])
        params = urllib.parse.parse_qs(parsed.query)
        self.assertEqual(parsed.scheme, "https")
        self.assertEqual(parsed.netloc, "accounts.google.com")
        self.assertEqual(parsed.path, "/o/oauth2/v2/auth")

        # Client id is the public Antigravity IDE client.
        self.assertEqual(params.get("client_id", [""])[0], ANTIGRAVITY_CLIENT_ID)
        self.assertEqual(params.get("response_type", [""])[0], "code")
        self.assertEqual(params.get("redirect_uri", [""])[0], redirect_uri)
        self.assertEqual(params.get("state", [""])[0], out["state"])

        # Scopes — exactly the 5 Antigravity scopes; no openid, no
        # arbitrary extras.
        scopes = set((params.get("scope", [""])[0]).split())
        expected = {
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/userinfo.profile",
            "https://www.googleapis.com/auth/cclog",
            "https://www.googleapis.com/auth/experimentsandconfigs",
        }
        self.assertEqual(scopes, expected)
        self.assertNotIn("openid", scopes)

        # PKCE S256 with challenge = SHA256(verifier) base64url-no-padding.
        self.assertEqual(params.get("code_challenge_method", [""])[0], "S256")
        verifier = out["pending"]["verifier"]
        self.assertEqual(
            params.get("code_challenge", [""])[0], _pkce_challenge(verifier)
        )

        # Offline access required for refresh_token; no prompt=consent.
        self.assertEqual(params.get("access_type", [""])[0], "offline")
        self.assertNotIn("prompt", params)

    def test_complete_persists_token_with_project(self):
        self.mock.start()

        @self.mock.route("/token")
        def token(body, _headers):
            self.assertEqual(body.get("grant_type"), "authorization_code")
            self.assertEqual(body.get("code"), "fake-auth-code")
            self.assertEqual(body.get("code_verifier"), "test-verifier")
            self.assertEqual(body.get("redirect_uri"), "http://127.0.0.1:8085/oauth2callback")
            self.assertEqual(body.get("client_id"), ANTIGRAVITY_CLIENT_ID)
            return 200, {
                "access_token": "fake-access-token",
                "refresh_token": "fake-refresh-token",
                "expires_in": 3600,
                "scope": "cloud-platform",
                "token_type": "Bearer",
            }

        @self.mock.route("/loadCodeAssist")
        def load(body, _headers):
            return 200, {"cloudaicompanionProject": "test-project-123"}

        @self.mock.route("/userinfo")
        def userinfo(_body, headers):
            auth = headers.get("Authorization") or headers.get("authorization")
            self.assertTrue(auth and auth.startswith("Bearer "))
            return 200, {"email": "test@example.com"}

        ctx = {
            "action": "complete",
            "code": "fake-auth-code",
            "pending": {"verifier": "test-verifier"},
            "redirect_uri": "http://127.0.0.1:8085/oauth2callback",
            "token_path": self.token_path,
        }
        with temp_env(CATALYST_CODE_ANTIGRAVITY_PROJECT=""):
            out = run_script(ctx, port=self.mock.port)

        self.assertEqual(out, {"ok": True})

        # On-disk token contract.
        self.assertTrue(os.path.exists(self.token_path), "token file was not written")
        with open(self.token_path, encoding="utf-8") as handle:
            token = json.load(handle)

        self.assertEqual(token["access_token"], "fake-access-token")
        self.assertEqual(token["refresh_token"], "fake-refresh-token")
        self.assertEqual(token["project_id"], "test-project-123")
        self.assertEqual(token["email"], "test@example.com")
        # expires_at ≈ now + 3600; sanity-check ±5s slack.
        self.assertGreater(token["expires_at"], int(time.time()) + 3590)

        # File mode 0o600 — secrets at rest.
        mode = stat.S_IMODE(os.stat(self.token_path).st_mode)
        self.assertEqual(mode, 0o600)

        # loadCodeAssist was called exactly once and used the right metadata.
        load_calls = [r for r in self.mock.request_log if r["path"] == "/loadCodeAssist"]
        self.assertEqual(len(load_calls), 1)
        self.assertEqual(load_calls[0]["body"]["metadata"]["ideType"], 9)
        self.assertEqual(load_calls[0]["body"]["metadata"]["pluginType"], 2)

    def test_token_refresh_preserves_project_id_and_email(self):
        # Pre-write a near-expiry token so the script refreshes it.
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
            # Deliberately omit refresh_token in the response to verify the
            # script preserves the old one across rotations.
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

        # CRITICAL: must not regress to x-goog-user-project — that header
        # forces the Cloud Code Private API enablement check and 403s on
        # free-tier / managed projects.
        header_names = {h[0] for h in out["headers"]}
        self.assertNotIn("x-goog-user-project", header_names)

        # Token file on disk has the new access token and preserved metadata.
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

    def test_onboarding_fallback(self):
        """loadCodeAssist returns tiers-only (no project); onboardUser must
        be polled and the resulting project persisted."""
        self.mock.start()
        onboard_calls = []

        @self.mock.route("/token")
        def token(_body, _headers):
            return 200, {
                "access_token": "fake-access",
                "refresh_token": "fake-refresh",
                "expires_in": 3600,
                "token_type": "Bearer",
            }

        @self.mock.route("/loadCodeAssist")
        def load(_body, _headers):
            return 200, {
                "allowedTiers": [
                    {"id": "free-tier", "isDefault": True},
                    {"id": "legacy-tier", "isDefault": False},
                ],
                # No cloudaicompanionProject — forces the onboard fallback.
            }

        @self.mock.route("/onboardUser")
        def onboard(body, _headers):
            onboard_calls.append(body)
            if len(onboard_calls) < 3:
                return 200, {"done": False}
            return 200, {
                "done": True,
                "response": {"cloudaicompanionProject": "onboarded-proj"},
            }

        ctx = {
            "action": "complete",
            "code": "fake-code",
            "pending": {"verifier": "fake-verifier"},
            "redirect_uri": "http://127.0.0.1:8085/oauth2callback",
            "token_path": self.token_path,
        }
        with temp_env(CATALYST_CODE_ANTIGRAVITY_PROJECT=""):
            out = run_script(ctx, port=self.mock.port)

        self.assertEqual(out, {"ok": True})
        self.assertGreaterEqual(
            len(onboard_calls), 3,
            "onboardUser should have been polled until done=true",
        )
        # The script must echo the default tier id in the request body.
        sent_tiers = [c.get("tierId") for c in onboard_calls]
        self.assertTrue(all(t == "free-tier" for t in sent_tiers))

        with open(self.token_path, encoding="utf-8") as handle:
            token = json.load(handle)
        self.assertEqual(token["project_id"], "onboarded-proj")

    def test_clear_removes_token_file(self):
        # Seed both the token file and the .lock sidecar the script may have
        # left behind from a previous refresh.
        with open(self.token_path, "w", encoding="utf-8") as handle:
            json.dump({"access_token": "x"}, handle)
        with open(self.token_path + ".lock", "w", encoding="utf-8") as handle:
            handle.write("")

        out = run_script({"action": "clear", "token_path": self.token_path})
        self.assertEqual(out, {"ok": True})

        self.assertFalse(os.path.exists(self.token_path))
        self.assertFalse(os.path.exists(self.token_path + ".lock"))

    def test_env_override_project_wins(self):
        """CATALYST_CODE_ANTIGRAVITY_PROJECT wins over loadCodeAssist.

        This is the escape hatch for users whose auto-provisioned project
        is Google-managed (no Cloud Console access) — the script must
        persist the override even when loadCodeAssist returns a project.
        """
        self.mock.start()

        @self.mock.route("/token")
        def token(_body, _headers):
            return 200, {
                "access_token": "fake-access",
                "refresh_token": "fake-refresh",
                "expires_in": 3600,
                "token_type": "Bearer",
            }

        @self.mock.route("/loadCodeAssist")
        def load(_body, _headers):
            return 200, {"cloudaicompanionProject": "real-load-project"}

        @self.mock.route("/userinfo")
        def userinfo(_body, _headers):
            return 200, {"email": ""}

        ctx = {
            "action": "complete",
            "code": "fake-code",
            "pending": {"verifier": "fake-verifier"},
            "redirect_uri": "http://127.0.0.1:8085/oauth2callback",
            "token_path": self.token_path,
        }
        with temp_env(CATALYST_CODE_ANTIGRAVITY_PROJECT="override-project"):
            out = run_script(ctx, port=self.mock.port)

        self.assertEqual(out, {"ok": True})

        with open(self.token_path, encoding="utf-8") as handle:
            token = json.load(handle)
        self.assertEqual(token["project_id"], "override-project")


if __name__ == "__main__":
    unittest.main()