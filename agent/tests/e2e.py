#!/usr/bin/env python3
"""Exercise a running agent with real FFmpeg. Run in the same network namespace."""
import argparse
import base64
import http.server
import json
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument("--url", default="http://127.0.0.1:8080")
parser.add_argument("--token-file")
parser.add_argument("--gpu", action="store_true")
args = parser.parse_args()
token = Path(args.token_file).read_text().strip() if args.token_file else None
receipt = b'{"fixture":"uploaded"}\n'
events = []
lock = threading.Lock()

with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    source = root / "source.mp4"
    subprocess.run(["ffmpeg", "-hide_banner", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30",
                    "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "1", "-c:v", "libx264",
                    "-preset", "fast", "-crf", "18", "-profile:v", "high", "-pix_fmt", "yuv420p", "-c:a", "aac", str(source)], check=True)

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            assert self.path == "/input"
            assert self.headers.get("X-Source-Token") == "fixture-source"
            self.send_response(200)
            self.send_header("Content-Length", str(source.stat().st_size))
            self.end_headers()
            with source.open("rb") as file:
                while chunk := file.read(65536):
                    self.wfile.write(chunk)

        def do_PUT(self):
            size = int(self.headers["Content-Length"])
            if self.path == "/callback":
                assert self.headers.get("X-Callback-Token") == "fixture-callback"
                body = self.rfile.read(size)
                with lock:
                    events.append(json.loads(body))
                self.send_response(204)
                self.end_headers()
                return
            assert self.path in ("/h264", "/hevc", "/av1")
            assert self.headers.get("X-Upload-Token") == "fixture-output"
            with (root / (self.path[1:] + ".mp4")).open("wb") as file:
                while size:
                    chunk = self.rfile.read(min(65536, size))
                    if not chunk:
                        raise AssertionError("truncated upload")
                    file.write(chunk)
                    size -= len(chunk)
            self.send_response(201)
            self.send_header("X-Receipt", "opaque-metadata")
            self.send_header("Content-Length", str(len(receipt)))
            self.end_headers()
            self.wfile.write(receipt)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        for codec in (["h264", "hevc", "av1"] if args.gpu else ["h264"]):
            job_id = "fixture-" + codec + "-" + str(time.time_ns())
            encoder = codec + "_nvenc" if args.gpu else "libx264"
            ffmpeg_args = (["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"] if args.gpu else [])
            ffmpeg_args += ["-i", "{input}", "-c:v", encoder]
            if args.gpu:
                ffmpeg_args += ["-vf", "scale_cuda=640:360", "-pix_fmt", "cuda", "-preset", "p4", "-cq", "23"]
            ffmpeg_args += ["-c:a", "copy", "{output}"]
            spec = {"input": {"url": base + "/input", "headers": {"X-Source-Token": "fixture-source"}},
                    "output": {"url": base + "/" + codec, "headers": {"X-Upload-Token": "fixture-output", "Content-Type": "video/mp4"}},
                    "callback": {"url": base + "/callback", "headers": {"X-Callback-Token": "fixture-callback"}},
                    "args": ffmpeg_args, "output_extension": "mp4"}
            req = urllib.request.Request(args.url + "/v1/jobs/" + job_id, method="PUT", data=json.dumps(spec).encode(),
                                         headers={"Content-Type": "application/json", **({"Authorization": "Bearer " + token} if token else {})})
            with urllib.request.urlopen(req, timeout=10) as response:
                assert response.status == 202
            deadline = time.monotonic() + 90
            terminal = None
            while time.monotonic() < deadline:
                with lock:
                    terminal = next((e for e in events if e["id"] == job_id and e["terminal"]), None)
                if terminal:
                    break
                time.sleep(0.1)
            assert terminal is not None, "missing terminal callback"
            assert terminal["state"] == "succeeded", terminal
            assert terminal["result"]["ffmpeg_exit_code"] == 0
            upload = terminal["result"]["upload_response"]
            assert upload["status"] == 201 and base64.b64decode(upload["body_base64"]) == receipt
            assert any(h["name"] == "x-receipt" and base64.b64decode(h["value_base64"]) == b"opaque-metadata" for h in upload["headers"])
            probe = json.loads(subprocess.check_output(["ffprobe", "-v", "error", "-count_frames", "-show_streams", "-of", "json", str(root / (codec + ".mp4"))]))
            video = next(s for s in probe["streams"] if s["codec_type"] == "video")
            audio = next(s for s in probe["streams"] if s["codec_type"] == "audio")
            assert (video["codec_name"], video["width"], video["height"], video["nb_read_frames"]) == (codec, 640 if args.gpu else 1280, 360 if args.gpu else 720, "30")
            assert audio["codec_name"] == "aac"
            print(f"PASS: GET -> {encoder} -> PUT -> callback; 30 frames, AAC, opaque upload receipt preserved", flush=True)
    finally:
        server.shutdown()
