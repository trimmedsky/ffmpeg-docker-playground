#!/usr/bin/env python3
"""Real HTTP/process contract tests; only FFmpeg is replaced by a deterministic fixture."""
import base64
import contextlib
import http.server
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

BINARY = str(Path(sys.argv[1]).resolve())
PAYLOAD = b"fixture media bytes\x00\xff"
RECEIPT = b'{"stored":"fixture"}\n'
EVENTS = []
UPLOADS = []
RETRIES = {}
LOCK = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body=b""):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
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
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(url, data=json.dumps(data).encode() if data is not None else None, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=3) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def job(name, mode="copy"):
    return {"id": name, "input": {"url": BASE + "/input", "headers": {"X-Input-Token": "fixture-input"}},
            "output": {"url": BASE + "/upload", "headers": {"X-Upload-Token": "fixture-output"}},
            "callback": {"url": BASE + "/callback"}, "output_extension": "bin",
            "args": ["--mode", mode, "-i", "{input}", "{output}"]}


@contextlib.contextmanager
def agent(**overrides):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        token = secrets.token_hex(32)
        (root / "token").write_text(token)
        fake = root / "ffmpeg"
        fake.write_text('''#!/usr/bin/env python3
import os, pathlib, sys, time
assert "FFMPEG_AGENT_TOKEN_FILE" not in os.environ
mode = sys.argv[sys.argv.index("--mode") + 1]
print("fixture pid=" + str(os.getpid()), file=sys.stderr, flush=True)
if mode == "slow": time.sleep(2)
if mode == "timeout": time.sleep(30)
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
        env = dict(os.environ, FFMPEG_AGENT_TOKEN_FILE=str(root / "token"), FFMPEG_AGENT_LISTEN=f"127.0.0.1:{port}",
                   FFMPEG_AGENT_WORK_DIR=str(root / "work"), FFMPEG_AGENT_FFMPEG=str(fake),
                   FFMPEG_AGENT_HEARTBEAT_SECS="1", FFMPEG_AGENT_JOB_TIMEOUT_SECS="10")
        env.update(overrides)
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
                    return request(url + "/healthz")[0] == 200
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
                assert token not in log.read(), "credential leaked into service log"


def submit(url, token, spec, status=202):
    actual, body = request(url + "/v1/jobs", spec, token)
    assert actual == status, (actual, body)


def terminal(name):
    return wait_for(lambda: events(name, True))[0]


with agent() as (_, url, token, root):
    submit(url, None, job("unauthorized"), 401)
    malformed = urllib.request.Request(url + "/v1/jobs", data=b"not-json", headers={"Content-Type": "application/json"})
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
    assert log["ffmpeg_log_truncated"] and len(log["ffmpeg_log"].encode()) <= 65536 and "LOG-END" in log["ffmpeg_log"]
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
SERVER.shutdown()
