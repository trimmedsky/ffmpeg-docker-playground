use std::{env, fs, net::SocketAddr, path::PathBuf, time::Duration};

pub struct Config {
    pub listen: SocketAddr,
    pub token: Option<Vec<u8>>,
    pub work_dir: PathBuf,
    pub ffmpeg: String,
    pub concurrency: usize,
    pub queue: usize,
    pub heartbeat: Duration,
    pub job_timeout: Duration,
    pub stall_timeout: Duration,
    pub max_input: u64,
    pub max_output: u64,
}

fn number(name: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let value = env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse::<u64>()
        .map_err(|_| format!("invalid {name}"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be in {min}..={max}"));
    }
    Ok(value)
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let token = match env::var("FFMPEG_AGENT_TOKEN_FILE") {
            Err(env::VarError::NotPresent) => None,
            Ok(path) if path.is_empty() => None,
            Err(_) => return Err("invalid FFMPEG_AGENT_TOKEN_FILE".into()),
            Ok(path) => {
                let token = fs::read_to_string(path)
                    .map_err(|_| "cannot read token file")?
                    .trim()
                    .as_bytes()
                    .to_vec();
                if token.len() < 32
                    || token.len() > 1024
                    || token.iter().any(|c| !c.is_ascii_graphic())
                {
                    return Err("token must contain 32..1024 printable ASCII characters".into());
                }
                Some(token)
            }
        };
        Ok(Self {
            listen: env::var("FFMPEG_AGENT_LISTEN")
                .unwrap_or_else(|_| "127.0.0.1:8080".into())
                .parse()
                .map_err(|_| "invalid listen address")?,
            token,
            work_dir: env::var("FFMPEG_AGENT_WORK_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| env::temp_dir().join("ffmpeg-agent")),
            ffmpeg: env::var("FFMPEG_AGENT_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
            concurrency: number("FFMPEG_AGENT_CONCURRENCY", 1, 1, 16)? as usize,
            queue: number("FFMPEG_AGENT_QUEUE_CAPACITY", 1, 1, 64)? as usize,
            heartbeat: Duration::from_secs(number("FFMPEG_AGENT_HEARTBEAT_SECS", 10, 1, 300)?),
            job_timeout: Duration::from_secs(number("FFMPEG_AGENT_JOB_TIMEOUT_SECS", 0, 0, 86400)?),
            stall_timeout: Duration::from_secs(number(
                "FFMPEG_AGENT_STALL_TIMEOUT_SECS",
                120,
                1,
                3600,
            )?),
            max_input: number("FFMPEG_AGENT_MAX_INPUT_BYTES", 16 << 30, 1, 1 << 40)?,
            max_output: number("FFMPEG_AGENT_MAX_OUTPUT_BYTES", 16 << 30, 1, 1 << 40)?,
        })
    }
}
