#!/usr/bin/env python3
"""Real HTTP/process contract tests; only FFmpeg is replaced by a deterministic fixture."""
import base64
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from jwt_issuer import Issuer

BINARY = str(Path(sys.argv[1]).resolve())
PAYLOAD = b"fixture media bytes\x00\xff"
RECEIPT = b'{"stored":"fixture"}\n'
EVENTS = []
UPLOADS = []
RETRIES = {}
REQUESTS = {}
LOCK = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body=b""):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def transient(self):
        with LOCK:
            key = (self.command, self.path)
            REQUESTS[key] = REQUESTS.get(key, 0) + 1
            return REQUESTS[key] < 3

    def do_POST(self):
        if self.path.startswith("/input"):
            return self.do_GET()
        return self.do_PUT()

    def do_PATCH(self):
        return self.do_PUT()

    def do_GET(self):
        if self.path == "/input-retry" and self.transient():
            return self.reply(503)
        if self.path == "/missing":
            return self.reply(404)
        if self.headers.get("X-Input-Token") != "fixture-input":
            return self.reply(403)
        if self.path == "/chunked-large":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"x" * 2048)
            return
        self.reply(200, b"x" * 2048 if self.path == "/large" else PAYLOAD)

    def do_PUT(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path.startswith("/callback"):
            event = json.loads(body)
            with LOCK:
                EVENTS.append(event)
                n = RETRIES.get(event["id"], 0)
                if event["terminal"]:
                    RETRIES[event["id"]] = n + 1
            return self.reply(503 if self.path == "/callback-retry" and event["terminal"] and n < 2 else 204)
        if self.headers.get("X-Upload-Token") != "fixture-output":
            return self.reply(403)
        if self.path == "/output-retry" and self.transient():
            assert body == PAYLOAD, "retry did not reopen complete output"
            return self.reply(503)
        if self.path == "/output-no-retry":
            self.transient()
            return self.reply(403)
        with LOCK:
            UPLOADS.append((self.path, body))
        self.send_response(500 if self.path == "/upload-fail" else 201)
        self.send_header("X-Receipt", "first")
        self.send_header("X-Receipt", "second")
        receipt = b"x" * 70000 if self.path == "/upload-large-receipt" else RECEIPT
        self.send_header("Content-Length", str(len(receipt)))
        self.end_headers()
        self.wfile.write(receipt)


SERVER = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=SERVER.serve_forever, daemon=True).start()
BASE = f"http://127.0.0.1:{SERVER.server_port}"


def wait_for(predicate, timeout=12):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("timed out waiting for expected state")


def events(job_id, terminal=None):
    with LOCK:
        return [e for e in EVENTS if e["id"] == job_id and (terminal is None or e["terminal"] == terminal)]


def request(url, data=None, token=None):
    """`token` is a JWT, or a callable (such as an Issuer) that mints a fresh one."""
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + (token() if callable(token) else token)
    req = urllib.request.Request(url, data=json.dumps(data).encode() if data is not None else None, headers=headers, method="PUT" if data is not None else "GET")
    try:
        with urllib.request.urlopen(req, timeout=3) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def raw_request(url, method, path, headers, body=b""):
    """For what urllib cannot express, such as repeated headers. Returns (status, headers)."""
    parts = urllib.parse.urlsplit(url)
    connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=3)
    try:
        connection.putrequest(method, path)
        for name, value in headers:
            connection.putheader(name, value)
        connection.putheader("Content-Length", str(len(body)))
        connection.endheaders(body)
        response = connection.getresponse()
        response.read()
        return response.status, response.headers
    finally:
        connection.close()


def job(name, mode="copy"):
    return {"id": name, "input": {"url": BASE + "/input", "headers": {"X-Input-Token": "fixture-input"}},
            "output": {"url": BASE + "/upload", "headers": {"X-Upload-Token": "fixture-output"}},
            "callback": {"url": BASE + "/callback"}, "output_extension": "bin",
            "args": ["--mode", mode, "-i", "{input}", "{output}"]}


@contextlib.contextmanager
def agent(**overrides):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        # The test issuer mints what the configured policy asks for.
        token = Issuer(root / "issuer", issuer=overrides.get("FFMPEG_AGENT_AUTH_ISSUER"),
                       typ=overrides.get("FFMPEG_AGENT_AUTH_TYP"))
        jwks = token.write_jwks()
        fake = root / "ffmpeg"
        fake.write_text('''#!/usr/bin/env python3
import os, pathlib, sys, time
assert not [name for name in os.environ if name.startswith("FFMPEG_AGENT")], "agent settings leaked to FFmpeg"
mode = sys.argv[sys.argv.index("--mode") + 1]
print("fixture pid=" + str(os.getpid()), file=sys.stderr, flush=True)
if mode == "slow": time.sleep(2)
if mode == "timeout": time.sleep(30)
if mode in ("growth", "progress", "no-progress"):
    for n in range(10):
        if mode == "growth":
            path = pathlib.Path("nested")
            path.mkdir(exist_ok=True)
            with (path / "temporary-data").open("ab") as file: file.write(b"x")
        if mode == "progress": print("frame=" + str(n + 1), flush=True)
        if mode == "no-progress":
            print("frame=0", flush=True)
            print("still waiting", file=sys.stderr, flush=True)
        time.sleep(0.3)
if mode == "fail":
    print("fixture encode error", file=sys.stderr)
    sys.exit(7)
if mode == "log": print("x" * 100000 + "LOG-END", file=sys.stderr)
source = pathlib.Path(sys.argv[sys.argv.index("-i") + 1]).read_bytes()
pathlib.Path(sys.argv[-1]).write_bytes(source)
''')
        fake.chmod(0o755)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env = dict(os.environ, FFMPEG_AGENT_AUTH_JWKS_FILE=str(jwks), FFMPEG_AGENT_AUTH_AUDIENCE="ffmpeg-agent",
                   FFMPEG_AGENT_LISTEN=f"127.0.0.1:{port}",
                   FFMPEG_AGENT_WORK_DIR=str(root / "work"), FFMPEG_AGENT_FFMPEG=str(fake),
                   FFMPEG_AGENT_HEARTBEAT_SECS="1", FFMPEG_AGENT_JOB_TIMEOUT_SECS="10")
        env.update(overrides)
        env = {key: value for key, value in env.items() if value is not None}
        abandoned = root / "work" / "job-abandoned"
        abandoned.mkdir(parents=True)
        (abandoned / "partial").write_bytes(b"abandoned fixture")
        with (root / "agent.log").open("w+") as log:
            process = subprocess.Popen([BINARY], env=env, stdout=log, stderr=log)
            url = f"http://127.0.0.1:{port}"
            def ready():
                if process.poll() is not None:
                    log.seek(0)
                    raise AssertionError(log.read())
                try:
                    return request(url + "/healthz", token=token if "FFMPEG_AGENT_AUTH_JWKS_FILE" in env else None)[0] == 200
                except urllib.error.URLError:
                    return False
            try:
                wait_for(ready)
                assert not abandoned.exists(), "abandoned scratch was not cleaned"
                other_env = dict(env, FFMPEG_AGENT_LISTEN="127.0.0.1:0")
                other = subprocess.run([BINARY], env=other_env, capture_output=True, timeout=5)
                assert other.returncode == 1 and b"work directory already in use" in other.stderr
                yield process, url, token, root
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                process.wait(timeout=20)
                assert process.returncode == 0
                assert not list((root / "work").glob("job-*")), "temporary files leaked"
                log.seek(0)
                logged = log.read()
                assert not [t for t in token.issued if t in logged], "credential leaked into service log"
                assert token.key.read_text() not in logged


def submit(url, token, spec, status=202):
    spec = dict(spec)
    name = spec.pop("id")
    actual, body = request(url + "/v1/jobs/" + name, spec, token)
    assert actual == status, (actual, body)


def terminal(name):
    return wait_for(lambda: events(name, True))[0]


def start_fails(message, **overrides):
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ, FFMPEG_AGENT_LISTEN="127.0.0.1:0", FFMPEG_AGENT_WORK_DIR=str(Path(tmp) / "work"))
        env = {k: v for k, v in env.items() if not k.startswith("FFMPEG_AGENT_AUTH") and k != "FFMPEG_AGENT_INSECURE_NO_AUTH"}
        env.update({k: str(v) for k, v in overrides.items()})
        result = subprocess.run([BINARY], env=env, capture_output=True, timeout=10)
        assert result.returncode == 1 and message in result.stderr, (overrides, result.returncode, result.stderr)


with tempfile.TemporaryDirectory() as tmp:
    issuer = Issuer(Path(tmp) / "issuer")
    jwks = issuer.write_jwks()
    broken = Path(tmp) / "broken.json"
    broken.write_text(json.dumps({"keys": [issuer.jwk(), issuer.jwk()]}))
    empty = Path(tmp) / "empty.json"
    empty.write_text(json.dumps({"keys": []}))
    start_fails(b"authentication is required")
    for flag in ("0", "true", ""):
        start_fails(b"FFMPEG_AGENT_INSECURE_NO_AUTH", FFMPEG_AGENT_INSECURE_NO_AUTH=flag)
    start_fails(b"FFMPEG_AGENT_TOKEN_FILE is no longer supported", FFMPEG_AGENT_TOKEN_FILE="/dev/null")
    start_fails(b"FFMPEG_AGENT_AUTH_AUDIENCE is required", FFMPEG_AGENT_AUTH_JWKS_FILE=jwks)
    start_fails(b"set without", FFMPEG_AGENT_AUTH_AUDIENCE="ffmpeg-agent")
    start_fails(b"cannot be combined", FFMPEG_AGENT_AUTH_JWKS_FILE=jwks, FFMPEG_AGENT_AUTH_AUDIENCE="a",
                FFMPEG_AGENT_INSECURE_NO_AUTH="1")
    for path, message in ((Path(tmp) / "missing.json", b"cannot open"), (broken, b"duplicate kid"), (empty, b"no keys")):
        start_fails(message, FFMPEG_AGENT_AUTH_JWKS_FILE=path, FFMPEG_AGENT_AUTH_AUDIENCE="ffmpeg-agent")
print("PASS: refuses to start without, with incomplete or with contradictory authentication settings")

with agent() as (_, url, token, root):
    submit(url, None, job("unauthorized"), 401)
    submit(url, "a" * 64, job("static-bearer"), 401)
    assert request(url + "/healthz")[0] == 401, "health requires a token"
    assert request(url + "/no-such-route")[0] == 401, "unknown routes are not revealed before authentication"
    status, headers = raw_request(url, "GET", "/healthz", [("X-User-Id", "example-caller")])
    assert status == 401 and headers["WWW-Authenticate"] == "Bearer", "identity headers do not authenticate"
    assert raw_request(url, "GET", "/healthz", [("Authorization", "Bearer " + token())])[0] == 200
    assert raw_request(url, "GET", "/healthz", [("Authorization", "bearer " + token())])[0] == 200
    duplicate = [("Authorization", "Bearer " + token()), ("Authorization", "Bearer " + token())]
    assert raw_request(url, "GET", "/healthz", duplicate)[0] == 401, "repeated credentials are refused"
    assert request(url + "/no-such-route", token=token)[0] == 404
    assert request(url + "/healthz", token=token.token(aud="another-service"))[0] == 401
    assert request(url + "/healthz", token=token.token(lifetime=300))[0] == 200
    assert request(url + "/healthz", token=token.token(lifetime=301))[0] == 401
    assert request(url + "/healthz", token=token.token(iat=int(time.time()) - 120, exp=int(time.time()) - 60))[0] == 401, "expired"
    assert request(url + "/healthz", token=token.token(header={"alg": "none"}))[0] == 401
    assert request(url + "/healthz", token=token.token(header={"jwk": token.jwk()}))[0] == 401
    assert request(url + "/healthz", token=token.token(sub=None))[0] == 401
    stranger = Issuer(root / "stranger")
    assert request(url + "/healthz", token=stranger.token())[0] == 401, "same kid, unknown key"
    # Rotation: publish the next key alongside the current one, switch, then retire the old key.
    successor = Issuer(root / "successor", kid="next-key")
    jwks = root / "issuer" / "jwks.json"
    assert request(url + "/healthz", token=successor)[0] == 401
    token.write_jwks(jwks, token.jwk(), successor.jwk())
    assert request(url + "/healthz", token=successor)[0] == 200, "a published key is usable at once"
    assert request(url + "/healthz", token=token)[0] == 200
    token.write_jwks(jwks, successor.jwk())
    assert request(url + "/healthz", token=token)[0] == 401, "a retired key is refused at once"
    jwks.write_text("{")
    assert request(url + "/healthz", token=successor)[0] == 401, "a broken JWKS fails closed"
    token.write_jwks(jwks)
    assert request(url + "/healthz", token=token)[0] == 200
    token.issued += successor.issued + stranger.issued
    malformed = urllib.request.Request(url + "/v1/jobs/invalid", method="PUT", data=b"not-json", headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(malformed, timeout=3)
        raise AssertionError("unauthenticated request accepted")
    except urllib.error.HTTPError as error:
        assert error.code == 401, "body parsed before authentication"
    invalid = job("invalid")
    invalid["input"]["url"] = "file:///etc/passwd"
    submit(url, token, invalid, 400)
    invalid = job("path-traversal")
    invalid["output_extension"] = "../bad"
    submit(url, token, invalid, 400)
    submit(url, token, job("first", "slow"))
    submit(url, token, job("first", "slow"), 409)
    submit(url, token, job("second", "slow"))
    submit(url, token, job("busy"), 429)
    first = terminal("first")
    second = terminal("second")
    assert first["state"] == second["state"] == "succeeded"
    assert any(e["state"] == "queued" for e in events("second", False))
    receipt = first["result"]["upload_response"]
    assert receipt["status"] == 201 and base64.b64decode(receipt["body_base64"]) == RECEIPT
    assert [base64.b64decode(h["value_base64"]) for h in receipt["headers"] if h["name"] == "x-receipt"] == [b"first", b"second"]
    assert all(data == PAYLOAD for _, data in UPLOADS)
    assert not receipt["body_truncated"]
    wait_for(lambda: not list((root / "work").glob("job-*")))
    submit(url, token, job("failed", "fail"))
    failure = terminal("failed")
    assert failure["state"] == "failed" and failure["result"]["ffmpeg_exit_code"] == 7
    assert "fixture encode error" in failure["ffmpeg_log"]
    missing = job("missing")
    missing["input"]["url"] = BASE + "/missing"
    submit(url, token, missing)
    assert terminal("missing")["result"]["error"] == "input GET returned non-success status"
    bad_upload = job("bad-upload")
    bad_upload["output"]["url"] = BASE + "/upload-fail"
    submit(url, token, bad_upload)
    assert terminal("bad-upload")["result"]["upload_response"]["status"] == 500
    submit(url, token, job("bounded-log", "log"))
    log = terminal("bounded-log")
    assert len(log["ffmpeg_log"].encode()) == 65536 and len(log["ffmpeg_log_tail"].encode()) <= 65536 and "LOG-END" in log["ffmpeg_log_tail"]
    assert "ffmpeg_log_tail" not in first and "ffmpeg_log_truncated" not in log
    oversized_receipt = job("large-receipt")
    oversized_receipt["output"]["url"] = BASE + "/upload-large-receipt"
    submit(url, token, oversized_receipt)
    receipt = terminal("large-receipt")["result"]["upload_response"]
    assert receipt["body_truncated"] and len(base64.b64decode(receipt["body_base64"])) == 65536
    retry = job("callback-retry")
    retry["callback"]["url"] = BASE + "/callback-retry"
    submit(url, token, retry)
    wait_for(lambda: len(events("callback-retry", True)) == 3)
    assert len({e["sequence"] for e in events("callback-retry", True)}) == 1
print("PASS: authentication, validation, admission, deduplication, queued heartbeats, streaming, receipts, errors, bounded logs, callback retries")

with agent(FFMPEG_AGENT_JOB_TIMEOUT_SECS="1", FFMPEG_AGENT_MAX_INPUT_BYTES="1024") as (_, url, token, root):
    oversized = job("oversized")
    oversized["input"]["url"] = BASE + "/large"
    submit(url, token, oversized)
    assert terminal("oversized")["result"]["error"] == "input exceeds byte limit"
    oversized = job("chunked-oversized")
    oversized["input"]["url"] = BASE + "/chunked-large"
    submit(url, token, oversized)
    assert terminal("chunked-oversized")["result"]["error"] == "input exceeds byte limit"
    submit(url, token, job("timeout", "timeout"))
    result = terminal("timeout")
    assert result["result"]["error"] == "job deadline exceeded"
    pid = int(result["ffmpeg_log"].split("pid=")[1].split()[0])
    def dead():
        try:
            os.kill(pid, 0)
            return False
        except ProcessLookupError:
            return True
    wait_for(dead)
print("PASS: input bounds, deadlines, child termination and file cleanup")

with agent(FFMPEG_AGENT_MAX_OUTPUT_BYTES="10") as (_, url, token, root):
    submit(url, token, job("output-limit"))
    assert terminal("output-limit")["result"]["error"] == "output is empty or reached byte limit"
print("PASS: output byte limit rejects a truncated or oversized result")

with agent() as (process, url, token, root):
    submit(url, token, job("shutdown-active", "timeout"))
    submit(url, token, job("shutdown-queued", "timeout"))
    wait_for(lambda: any(e["state"] == "encoding" for e in events("shutdown-active")))
    process.send_signal(signal.SIGTERM)
    assert terminal("shutdown-active")["result"]["error"] == "agent shutting down"
    assert terminal("shutdown-queued")["result"]["error"] == "agent shutting down"
print("PASS: graceful shutdown reports both active and queued jobs")
with agent() as (_, url, token, root):
    spec = job("methods-and-retries")
    spec["input"].update(url=BASE + "/input-retry", method="POST", retry={"max_attempts": 3, "delay_ms": 10})
    spec["output"].update(url=BASE + "/output-retry", method="PATCH", retry={"max_attempts": 3, "delay_ms": 10})
    spec["callback"].update(url=BASE + "/callback-retry", method="POST", retry={"max_attempts": 3, "delay_ms": 10})
    submit(url, token, spec)
    wait_for(lambda: len(events(spec["id"], True)) == 3)
    assert terminal(spec["id"])["state"] == "succeeded"
    assert REQUESTS[("POST", "/input-retry")] == REQUESTS[("PATCH", "/output-retry")] == 3
    spec = job("non-retryable")
    spec["output"].update(url=BASE + "/output-no-retry", retry={"max_attempts": 3, "delay_ms": 0})
    submit(url, token, spec)
    assert terminal(spec["id"])["state"] == "failed"
    assert REQUESTS[("PUT", "/output-no-retry")] == 1
    for field in ("input", "output", "callback"):
        spec = job("invalid-retry-" + field)
        spec[field]["retry"] = {"max_attempts": 0}
        submit(url, token, spec, 400)
        spec[field].pop("retry")
        spec[field]["method"] = "CONNECT"
        submit(url, token, spec, 400)
print("PASS: per-endpoint HTTP methods, bounded retries, output replay and permanent failures")

# Issuer, JOSE type, subject allow-list and a shorter lifetime bound, as a deployment
# behind a token-minting gateway or sidecar might configure them.
with agent(FFMPEG_AGENT_AUTH_ISSUER="example-issuer", FFMPEG_AGENT_AUTH_TYP="example+jwt",
           FFMPEG_AGENT_AUTH_MAX_LIFETIME_SECS="60", FFMPEG_AGENT_AUTH_SUBJECTS="example-caller,other-caller") as (_, url, token, root):
    # Unknown claims (here a display name and a scope) are accepted and ignored.
    submit(url, token.token(name="Example caller", scope=["jobs"]), job("strict-policy"))
    assert terminal("strict-policy")["state"] == "succeeded"
    assert request(url + "/healthz", token=token.token(sub="other-caller", aud=["ffmpeg-agent", "x"]))[0] == 200
    assert request(url + "/healthz", token=token.token(iss=None))[0] == 401, "issuer required"
    assert request(url + "/healthz", token=token.token(iss="other-issuer"))[0] == 401
    assert request(url + "/healthz", token=token.token(header={"typ": None}))[0] == 401, "typ required"
    assert request(url + "/healthz", token=token.token(header={"typ": "JWT"}))[0] == 401
    assert request(url + "/healthz", token=token.token(sub="someone-else"))[0] == 401
    assert request(url + "/healthz", token=token.token(lifetime=60))[0] == 200
    assert request(url + "/healthz", token=token.token(lifetime=61))[0] == 401
print("PASS: issuer, typ, subject allow-list and lifetime bound")

# Isolated local tests only: the explicit flag is the one way to run without a token.
with agent(FFMPEG_AGENT_AUTH_JWKS_FILE=None, FFMPEG_AGENT_AUTH_AUDIENCE=None, FFMPEG_AGENT_INSECURE_NO_AUTH="1") as (_, url, token, root):
    submit(url, None, job("insecure-local"))
    assert terminal("insecure-local")["state"] == "succeeded"
print("PASS: unauthenticated only with the explicit local-test flag")

with agent(FFMPEG_AGENT_STALL_TIMEOUT_SECS="1", FFMPEG_AGENT_JOB_TIMEOUT_SECS="0") as (_, url, token, root):
    for mode in ("growth", "progress", "timeout", "no-progress"):
        name = "stall-" + mode
        submit(url, token, job(name, mode))
        result = terminal(name)
        if mode in ("growth", "progress"):
            assert result["state"] == "succeeded", result
        else:
            assert result["result"]["error"] == "job progress stalled", result
            pid = int(result["ffmpeg_log"].split("pid=")[1].split()[0])
            def dead():
                try: os.kill(pid, 0); return False
                except ProcessLookupError: return True
            wait_for(dead)
    submit(url, token, job("long-active", "growth"))
    submit(url, token, job("long-queued", "progress"))
    assert terminal("long-active")["state"] == "succeeded"
    assert terminal("long-queued")["state"] == "succeeded"
print("PASS: nested file growth, encoder progress, stall detection, queued time excluded and child cleanup")
SERVER.shutdown()
