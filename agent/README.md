# ffmpeg-agent

A small, stateless Rust HTTP worker. It downloads one input with GET, executes
FFmpeg without a shell, uploads one output with PUT, and reports progress and the
result with PUT callbacks. It does not interpret URLs, storage response bodies,
media-library identifiers or application-specific signatures.

The Docker image installs `ffmpeg-agent` alongside `ffmpeg` and `ffprobe`.
**It starts only when explicitly invoked.** The image's default command is unchanged.
The binary has a single-threaded asynchronous runtime, streams media to disk, and
bounds its in-memory queue, logs and response bodies. FFmpeg runs as a separate
child process; its resource use is additional to the agent's own footprint.

## Build and test

```sh
cd agent
cargo build --locked --release
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 tests/integration.py target/debug/ffmpeg-agent
```

The checked-in toolchain and `Cargo.lock` fix the build dependencies. Native Linux
AMD64 and ARM64 are tested in CI. The HTTP integration suite uses a deterministic
FFmpeg substitute to exercise admission, failures, timeouts and process cleanup;
`tests/e2e.py` exercises a running agent with real FFmpeg and optional NVIDIA codecs.

From the repository root, `docker build -t ffmpeg-agent:local .` builds both the
FFmpeg image and the agent. There is no Rust compiler in the final image.

## HTTP API v1

`GET /healthz` is a public liveness check. `POST /v1/jobs` requires
`Authorization: Bearer <token>` and an `application/json` body of at most 64 KiB:

```json
{
  "id": "encode-001",
  "input": {
    "url": "https://media.example.org/input.mp4",
    "headers": {"Authorization": "Bearer input-access-token"}
  },
  "output": {
    "url": "https://storage.example.org/output.mp4?signature=example",
    "headers": {"Content-Type": "video/mp4"}
  },
  "callback": {
    "url": "https://jobs.example.org/callback",
    "headers": {"Authorization": "Bearer callback-access-token"}
  },
  "args": [
    "-hwaccel", "cuda", "-hwaccel_output_format", "cuda",
    "-i", "{input}", "-c:v", "h264_nvenc", "-preset", "p4",
    "-cq", "23", "-c:a", "copy", "{output}"
  ],
  "output_extension": "mp4"
}
```

`headers` is optional on every endpoint. Only HTTP/HTTPS URLs are accepted;
redirects are rejected so credentials are not forwarded to another destination.
TLS certificates are verified. Transport headers (`Host`, `Content-Length`,
`Transfer-Encoding`, `Connection`, `Upgrade`) are managed by the agent.

Arguments are separate process arguments, never shell text. Exactly one `-i`
must precede the standalone `{input}` placeholder. The standalone `{output}`
placeholder must occur once, at the end. Both expand to files in a unique job
directory. The extension selects the output muxer unless `-f` is specified.
FFmpeg gets `-nostdin -hide_banner -y`; the agent adds `-fs` for its output limit.
Software codecs work too: omit the CUDA input options and select `libx264`, etc.

Responses:

| Status | Meaning |
|---|---|
| `202` | Accepted; body is `{"id":"encode-001"}` |
| `401` | Missing or incorrect bearer token |
| `400` / `413` / `415` / `422` | Invalid job, oversized body, wrong content type or malformed JSON |
| `409` | That ID is already queued, running or finishing its callback |
| `429` | All execution and waiting slots are occupied; `Retry-After: 10` |
| `503` | Shutting down; not accepted |

IDs are caller-supplied, 1–80 ASCII letters/digits/`-_.`. Deduplication covers
**in-flight jobs only**. The caller owns durable scheduling, unique attempt IDs,
retry timing and idempotent handling of uploads and callbacks. A restart forgets
all jobs. There is no database, durable queue, stored-result API or automatic job
retry. A full queue is a postponement, not a failed media conversion.

### Callbacks and heartbeat

The same callback URL receives JSON PUTs while a job is queued and throughout
execution. `sequence` increases for each event within an attempt. States are
`queued`, `downloading`, `encoding`, `uploading`, `succeeded` and `failed`.
Only the last two are terminal. A heartbeat looks like:

```json
{"version":1,"id":"encode-001","sequence":0,"state":"queued","terminal":false,"ffmpeg_log_truncated":false}
```

Terminal events additionally contain `result` and `ffmpeg_log`:

```json
{
  "version": 1,
  "id": "encode-001",
  "sequence": 4,
  "state": "succeeded",
  "terminal": true,
  "result": {
    "error": null,
    "ffmpeg_exit_code": 0,
    "upload_response": {
      "status": 201,
      "headers": [{"name":"etag","value_base64":"ImV4YW1wbGUi"}],
      "body_base64": "",
      "body_truncated": false
    }
  },
  "ffmpeg_log": "...",
  "ffmpeg_log_truncated": false
}
```

`upload_response` preserves the HTTP status, all response header values (including
duplicates) and up to 64 KiB of raw response body. Header values and body use
base64, so binary responses remain intact; the agent does not parse them.
Non-2xx upload responses fail the job and are reported with their receipt.
`upload_response` or `ffmpeg_exit_code` can be null when that stage was not reached.
FFmpeg's stderr tail is capped at 64 KiB, with explicit truncation flags.

The callback must acknowledge with 2xx. Heartbeats get one attempt; terminal
events get at most three attempts, five seconds each, one second apart. Retries
reuse the **same sequence and body**. There are no durable delivery guarantees.
The caller should time out a lost job after missed heartbeats (for example three
intervals), handle duplicate events, and ignore any late heartbeat after a
terminal event. Callbacks and their acknowledgements are part of the trust boundary.

## Configuration

| Environment variable | Default / meaning |
|---|---|
| `FFMPEG_AGENT_TOKEN_FILE` | Required; file containing a random 32+ character bearer token |
| `FFMPEG_AGENT_LISTEN` | `127.0.0.1:8080`; use `0.0.0.0:8080` inside a container with loopback-only host publishing |
| `FFMPEG_AGENT_WORK_DIR` | OS temporary directory + `/ffmpeg-agent`; exclusive scratch directory |
| `FFMPEG_AGENT_FFMPEG` | `ffmpeg`; executable, not a shell command |
| `FFMPEG_AGENT_CONCURRENCY` | `1`, range 1–16 |
| `FFMPEG_AGENT_QUEUE_CAPACITY` | `1` waiting slot, range 1–64 |
| `FFMPEG_AGENT_HEARTBEAT_SECS` | `10`, range 1–300 |
| `FFMPEG_AGENT_JOB_TIMEOUT_SECS` | `3600` after execution starts, including download and upload |
| `FFMPEG_AGENT_MAX_INPUT_BYTES` | `17179869184` (16 GiB) |
| `FFMPEG_AGENT_MAX_OUTPUT_BYTES` | `17179869184` (16 GiB); outputs reaching the limit are rejected |

Media is streamed, not buffered whole in RAM. Provision scratch disk for both
input and output of every concurrent job. Container memory/CPU limits apply to
FFmpeg too; filesystem quotas can provide an additional hard disk-space boundary.
No token or URL is logged by the service. FFmpeg's own log is returned to the
specified callback and may include user-supplied arguments. Credentials are not
inherited in FFmpeg's environment.

The work directory is locked against concurrent agent instances. Normal completion,
failure and graceful cancellation delete job directories. Startup removes abandoned
`job-*` directories left by crashes. Never point this setting at shared application
data. SIGTERM/SIGINT stops admission, kills active FFmpeg children and reports
failure for active and queued jobs, subject to the service manager's stop timeout.

## Run with systemd and Docker

The example [unit](deploy/ffmpeg-agent.service) binds the host API only to
`127.0.0.1:18080`, runs as UID/GID 1000 inside the container, requests NVIDIA video
and compute capabilities, limits CPU/memory/processes, and uses a read-only root
filesystem. It never pulls an image during startup. Change the GPU flag for a
CPU-only host and adjust limits for the chosen workloads.

```sh
# Build or docker load the image first.
sudo install -d -m 0755 /etc/ffmpeg-agent
sudo install -d -o 1000 -g 1000 -m 0700 /var/lib/ffmpeg-agent
sudo install -m 0600 agent/deploy/agent.env.example /etc/ffmpeg-agent/agent.env
# Set FFMPEG_AGENT_IMAGE in that file to the image you built/loaded.
sudo sh -c 'umask 027; openssl rand -hex 32 > /etc/ffmpeg-agent/token'
sudo chown root:1000 /etc/ffmpeg-agent/token
sudo chmod 0640 /etc/ffmpeg-agent/token
sudo install -m 0644 agent/deploy/ffmpeg-agent.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ffmpeg-agent
curl http://127.0.0.1:18080/healthz
```

Rotate the token by updating the file and restarting the unit. Changing configuration
or the image also requires a restart. `systemctl stop ffmpeg-agent` stops the container;
`systemctl disable --now ffmpeg-agent` removes it from boot startup.

The API is for **trusted callers**: choosing arbitrary FFmpeg arguments and URL
destinations grants authority within the container and its reachable network.
The token is not a sandbox for hostile tenants. Do not mount host application data,
credentials unrelated to the agent, or the Docker socket into the worker. Keep
host publishing on loopback and use an SSH tunnel or an authenticated TLS gateway
when remote access is needed. Network access policy belongs to the deployment;
this project does not change host firewall rules or install a gateway.

## License

The agent code is licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option. This applies to the agent, not to FFmpeg or the other libraries in
the image; their existing license terms and redistribution constraints still apply.
