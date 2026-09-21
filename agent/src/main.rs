mod config;
mod engine;
mod model;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use engine::{Accepted, Active, Progress};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicU64};
use subtle::ConstantTimeEq;

async fn authenticate(
    State(state): State<Arc<engine::State>>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(token.as_bytes().ct_eq(&state.config.token)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response();
    }
    next.run(request).await
}

async fn submit(State(state): State<Arc<engine::State>>, Json(job): Json<model::Job>) -> Response {
    if let Err(message) = job.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":message}))).into_response();
    }
    if state.cancel.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let mut jobs = state.jobs.lock().unwrap();
    if jobs.contains(&job.id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"id already in flight"})),
        )
            .into_response();
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "10")],
            Json(json!({"error":"busy"})),
        )
            .into_response();
    };
    jobs.insert(job.id.clone());
    let active = Arc::new(Active {
        job,
        progress: std::sync::Mutex::new(Progress {
            phase: "queued",
            ..Default::default()
        }),
        sequence: AtomicU64::new(0),
    });
    let id = active.job.id.clone();
    let heartbeat = engine::heartbeat(state.clone(), active.clone());
    let accepted = Accepted {
        active,
        heartbeat,
        _permit: permit,
    };
    if let Err(error) = state.tx.try_send(accepted) {
        error.into_inner().heartbeat.abort();
        jobs.remove(&id);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    (StatusCode::ACCEPTED, Json(json!({"id":id}))).into_response()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [arg] if arg == "--version" => {
            println!("ffmpeg-agent {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        [arg] if arg == "--help" => {
            println!(
                "ffmpeg-agent: stateless HTTP FFmpeg worker\nConfigure with FFMPEG_AGENT_* environment variables. FFMPEG_AGENT_TOKEN_FILE is required.\nSee agent/README.md for the API, limits and deployment."
            );
            return;
        }
        [] => {}
        _ => {
            eprintln!("unexpected arguments; use --help");
            std::process::exit(2);
        }
    }
    if let Err(error) = run().await {
        eprintln!("ffmpeg-agent: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = config::Config::load()?;
    let address = config.listen;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|_| "cannot bind listen address")?;
    let (state, workers) = engine::initialize(config)?;
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await.expect("signal handler");
        cancel.cancel();
    });
    let app = Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")})) }),
        )
        .route(
            "/v1/jobs",
            post(submit).route_layer(middleware::from_fn_with_state(state.clone(), authenticate)),
        )
        .layer(DefaultBodyLimit::max(65536))
        .with_state(state.clone());
    eprintln!(
        "ffmpeg-agent {} listening on {address}",
        env!("CARGO_PKG_VERSION")
    );
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(state.cancel.clone().cancelled_owned())
        .await;
    state.cancel.cancel();
    for worker in workers {
        worker.await.map_err(|_| "worker terminated unexpectedly")?;
    }
    result.map_err(|_| "HTTP server failed".into())
}
