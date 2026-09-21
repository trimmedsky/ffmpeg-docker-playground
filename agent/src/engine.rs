use crate::{
    config::Config,
    model::{Endpoint, Event, Job, Outcome, ResponseHeader, UploadResponse},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Method};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

const LOG_LIMIT: usize = 65536;
const RESPONSE_LIMIT: usize = 65536;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Progress {
    pub phase: &'static str,
    pub log: Vec<u8>,
    pub log_truncated: bool,
    pub outcome: Outcome,
}

pub struct Active {
    pub job: Job,
    pub progress: Mutex<Progress>,
    pub sequence: AtomicU64,
}

pub struct Accepted {
    pub active: Arc<Active>,
    pub heartbeat: JoinHandle<()>,
    pub _permit: OwnedSemaphorePermit,
}

pub struct State {
    pub config: Config,
    pub client: Client,
    pub jobs: Mutex<HashSet<String>>,
    pub slots: Arc<Semaphore>,
    pub tx: mpsc::Sender<Accepted>,
    pub cancel: CancellationToken,
    pub _work_lock: std::fs::File,
}

pub fn initialize(config: Config) -> Result<(Arc<State>, Vec<JoinHandle<()>>), String> {
    std::fs::create_dir_all(&config.work_dir).map_err(|_| "cannot create work directory")?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.work_dir.join(".lock"))
        .map_err(|_| "cannot open work lock")?;
    lock.try_lock()
        .map_err(|_| "work directory already in use")?;
    // This directory belongs exclusively to one agent. Remove only its stale job directories.
    for entry in std::fs::read_dir(&config.work_dir).map_err(|_| "cannot read work directory")? {
        let entry = entry.map_err(|_| "cannot read work entry")?;
        if entry.file_name().to_string_lossy().starts_with("job-")
            && entry
                .file_type()
                .map_err(|_| "cannot inspect work entry")?
                .is_dir()
        {
            std::fs::remove_dir_all(entry.path()).map_err(|_| "cannot clean abandoned job")?;
        }
    }
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!("ffmpeg-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "cannot initialize HTTP client")?;
    let capacity = config.concurrency + config.queue;
    let (tx, rx) = mpsc::channel(capacity);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let state = Arc::new(State {
        config,
        client,
        jobs: Mutex::new(HashSet::new()),
        slots: Arc::new(Semaphore::new(capacity)),
        tx,
        cancel: CancellationToken::new(),
        _work_lock: lock,
    });
    let workers = (0..state.config.concurrency)
        .map(|_| tokio::spawn(worker(state.clone(), rx.clone())))
        .collect();
    Ok((state, workers))
}

fn request(state: &State, method: Method, endpoint: &Endpoint) -> reqwest::RequestBuilder {
    state
        .client
        .request(method, &endpoint.url)
        .headers(endpoint.header_map().expect("validated headers"))
}

pub async fn notify(state: &State, active: &Active, terminal: bool) -> bool {
    let sequence = active.sequence.fetch_add(1, Ordering::Relaxed);
    let body = {
        let progress = active.progress.lock().unwrap();
        serde_json::to_vec(&Event {
            version: 1,
            id: &active.job.id,
            sequence,
            state: progress.phase,
            terminal,
            result: terminal.then_some(&progress.outcome),
            ffmpeg_log: terminal.then(|| String::from_utf8_lossy(&progress.log).into_owned()),
            ffmpeg_log_truncated: progress.log_truncated,
        })
        .unwrap()
    };
    let attempts = if terminal { 3 } else { 1 };
    for attempt in 0..attempts {
        let result = request(state, Method::PUT, &active.job.callback)
            .header("content-type", "application/json")
            .timeout(CALLBACK_TIMEOUT)
            .body(body.clone())
            .send()
            .await;
        if result.is_ok_and(|r| r.status().is_success()) {
            return true;
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    false
}

pub fn heartbeat(state: Arc<State>, active: Arc<Active>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(state.config.heartbeat);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = state.cancel.cancelled() => break,
                _ = interval.tick() => { notify(&state, &active, false).await; }
            }
        }
    })
}

async fn worker(state: Arc<State>, rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Accepted>>>) {
    loop {
        let accepted = {
            let mut rx = rx.lock().await;
            if state.cancel.is_cancelled() {
                rx.try_recv().ok()
            } else {
                tokio::select! { biased; _ = state.cancel.cancelled() => rx.try_recv().ok(), item = rx.recv() => item }
            }
        };
        let Some(accepted) = accepted else {
            break;
        };
        let active = &accepted.active;
        let result = tokio::select! {
            biased;
            _ = state.cancel.cancelled() => Err("agent shutting down"),
            result = tokio::time::timeout(state.config.job_timeout, execute(&state, active)) => result.unwrap_or(Err("job deadline exceeded")),
        };
        accepted.heartbeat.abort();
        let _ = accepted.heartbeat.await;
        {
            let mut progress = active.progress.lock().unwrap();
            progress.phase = if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            };
            progress.outcome.error = result.err().map(str::to_owned);
        }
        if !notify(&state, active, true).await {
            eprintln!("terminal callback not acknowledged; caller must recover by timeout");
        }
        state.jobs.lock().unwrap().remove(&active.job.id);
    }
}

fn phase(active: &Active, phase: &'static str) {
    active.progress.lock().unwrap().phase = phase;
}

async fn execute(state: &State, active: &Active) -> Result<(), &'static str> {
    let dir = tempfile::Builder::new()
        .prefix("job-")
        .tempdir_in(&state.config.work_dir)
        .map_err(|_| "cannot create job directory")?;
    let input = dir.path().join("input");
    let output = dir
        .path()
        .join(format!("output.{}", active.job.output_extension));
    phase(active, "downloading");
    let mut response = request(state, Method::GET, &active.job.input)
        .send()
        .await
        .map_err(|_| "input GET failed")?;
    if !response.status().is_success() {
        return Err("input GET returned non-success status");
    }
    if response
        .content_length()
        .is_some_and(|n| n > state.config.max_input)
    {
        return Err("input exceeds byte limit");
    }
    let mut file = tokio::fs::File::create(&input)
        .await
        .map_err(|_| "cannot create input file")?;
    let mut count = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "input download interrupted")?
    {
        count = count
            .checked_add(chunk.len() as u64)
            .ok_or("input exceeds byte limit")?;
        if count > state.config.max_input {
            return Err("input exceeds byte limit");
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| "cannot write input file")?;
    }
    file.flush().await.map_err(|_| "cannot flush input file")?;
    drop(file);
    phase(active, "encoding");
    let mut command = Command::new(&state.config.ffmpeg);
    command
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("TMPDIR", dir.path())
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .args(["-nostdin", "-hide_banner", "-y"]);
    for arg in &active.job.args {
        match arg.as_str() {
            "{input}" => {
                command.arg(&input);
            }
            "{output}" => {
                command
                    .args(["-fs", &state.config.max_output.to_string()])
                    .arg(&output);
            }
            _ => {
                command.arg(arg);
            }
        }
    }
    let mut child = command.spawn().map_err(|_| "cannot start FFmpeg")?;
    let mut stderr = child.stderr.take().ok_or("cannot capture FFmpeg log")?;
    let read_log = async {
        let mut buffer = [0; 8192];
        loop {
            let n = stderr.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            let mut progress = active.progress.lock().unwrap();
            progress.log.extend_from_slice(&buffer[..n]);
            if progress.log.len() > LOG_LIMIT {
                let excess = progress.log.len() - LOG_LIMIT;
                progress.log.drain(..excess);
                progress.log_truncated = true;
            }
        }
        Ok::<_, std::io::Error>(())
    };
    let (status, _) =
        tokio::try_join!(child.wait(), read_log).map_err(|_| "FFmpeg process I/O failed")?;
    active.progress.lock().unwrap().outcome.ffmpeg_exit_code = status.code();
    if !status.success() {
        return Err("FFmpeg exited unsuccessfully");
    }
    let size = tokio::fs::metadata(&output)
        .await
        .map_err(|_| "FFmpeg produced no output")?
        .len();
    // -fs may stop an encode with exit code 0, so do not upload a truncated result.
    if size == 0 || size >= state.config.max_output {
        return Err("output is empty or reached byte limit");
    }
    phase(active, "uploading");
    let file = tokio::fs::File::open(&output)
        .await
        .map_err(|_| "cannot open output file")?;
    let mut response = request(state, Method::PUT, &active.job.output)
        .header("content-length", size)
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await
        .map_err(|_| "output PUT failed")?;
    let mut receipt = UploadResponse {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(name, value)| ResponseHeader {
                name: name.to_string(),
                value_base64: STANDARD.encode(value.as_bytes()),
            })
            .collect(),
        ..Default::default()
    };
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "output PUT response interrupted")?
    {
        let n = chunk.len().min(RESPONSE_LIMIT - body.len());
        body.extend_from_slice(&chunk[..n]);
        if n < chunk.len() {
            receipt.body_truncated = true;
            break;
        }
    }
    receipt.body_base64 = STANDARD.encode(body);
    let success = (200..300).contains(&receipt.status);
    active.progress.lock().unwrap().outcome.upload_response = Some(receipt);
    if !success {
        return Err("output PUT returned non-success status");
    }
    Ok(())
}
