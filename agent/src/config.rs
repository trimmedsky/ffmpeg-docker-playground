use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

pub struct Config {
    pub listen: SocketAddr,
    /// `None` only with the explicit `FFMPEG_AGENT_INSECURE_NO_AUTH=1` (local tests).
    pub backend: Option<hng_auth::client::BackendVerifier>,
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

/// Fail closed: without HNG_BACKEND_JWKS / HNG_SERVICE_ID the agent refuses to
/// start. Serving unauthenticated needs FFMPEG_AGENT_INSECURE_NO_AUTH=1, meant
/// only for local tests in an isolated network namespace.
fn backend_verifier(
    verifier: Option<hng_auth::client::BackendVerifier>,
    insecure: Option<&std::ffi::OsStr>,
) -> Result<Option<hng_auth::client::BackendVerifier>, String> {
    match (verifier, insecure) {
        (Some(_), Some(_)) => Err(
            "FFMPEG_AGENT_INSECURE_NO_AUTH cannot be combined with HNG_BACKEND_JWKS".into(),
        ),
        (Some(verifier), None) => Ok(Some(verifier)),
        (None, Some(flag)) if flag == "1" => Ok(None),
        (None, _) => Err(
            "HNG backend authentication is required: set HNG_BACKEND_JWKS and HNG_SERVICE_ID (FFMPEG_AGENT_INSECURE_NO_AUTH=1 only for local tests)".into(),
        ),
    }
}

impl Config {
    pub fn load() -> Result<Self, String> {
        if env::var_os("FFMPEG_AGENT_TOKEN_FILE").is_some() {
            return Err(
                "FFMPEG_AGENT_TOKEN_FILE is retired; the agent authenticates the host connector's backend JWT (HNG_BACKEND_JWKS)".into(),
            );
        }
        let backend = backend_verifier(
            hng_auth::client::BackendVerifier::from_env().map_err(|e| e.to_string())?,
            env::var_os("FFMPEG_AGENT_INSECURE_NO_AUTH").as_deref(),
        )?;
        Ok(Self {
            listen: env::var("FFMPEG_AGENT_LISTEN")
                .unwrap_or_else(|_| "127.0.0.1:8080".into())
                .parse()
                .map_err(|_| "invalid listen address")?,
            backend,
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

#[cfg(test)]
mod tests {
    use super::backend_verifier;
    use std::ffi::OsStr;

    fn verifier() -> hng_auth::client::BackendVerifier {
        hng_auth::client::BackendVerifier::new("/nonexistent/jwks.json", "ffmpeg-1".into())
    }

    #[test]
    fn refuses_to_start_without_hng() {
        assert!(backend_verifier(None, None).is_err());
        for flag in ["0", "true", "yes", ""] {
            assert!(
                backend_verifier(None, Some(OsStr::new(flag))).is_err(),
                "{flag}"
            );
        }
        assert!(backend_verifier(Some(verifier()), Some(OsStr::new("1"))).is_err());
    }

    #[test]
    fn hng_or_explicit_insecure_flag() {
        assert!(backend_verifier(Some(verifier()), None).unwrap().is_some());
        assert!(
            backend_verifier(None, Some(OsStr::new("1")))
                .unwrap()
                .is_none()
        );
    }
}
