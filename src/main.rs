use crate::{cli::Cli, config::Config, mcp::McpCoord, resolver::resolve_path};
use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use clap::Parser;
use fs_err::tokio as fs;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

mod cli;
mod config;
mod mcp;
mod resolver;

/// The one-shot CLI owns this port. The long-running MCP server uses a separate one so the two can
/// run at the same time; the Studio plugin polls both and serves whichever has a run pending.
const CLI_PORT: u16 = 28860;
const MCP_PORT: u16 = 28861;

#[derive(Clone)]
struct AppState {
    args: Arc<Cli>,
    config: Arc<Config>,
    coord: Coordinator,
}

/// Whatever drives a test run differs between the two modes, so the HTTP handlers delegate the
/// mode-specific bits (where do options come from, where does output go, what happens when a run
/// finishes) to one of these.
#[derive(Clone)]
enum Coordinator {
    /// One-shot CLI: there is exactly one run, output goes to the spinner, and the process exits
    /// when results come back.
    Cli(Arc<CliCoord>),
    /// Long-running MCP server: many runs over the process lifetime, each driven by a `run_tests`
    /// tool call.
    Mcp(Arc<McpCoord>),
}

struct CliCoord {
    spinner: Mutex<ProgressBar>,
    plugin_connected: Mutex<bool>,
    received_first_write: Mutex<bool>,
    /// We warn once when an extra plugin polls after one has already connected, then stay quiet —
    /// otherwise every duplicate poll (a second Studio window, or a stale plugin loop left behind
    /// by a hot-reload) floods the log every 0.5s for the whole run.
    duplicate_poll_warned: Mutex<bool>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    if args.mcp {
        run_mcp(args).await
    } else {
        run_cli(args).await
    }
}

/// The original one-shot behaviour: spin up the server, wait for the Studio plugin to run the
/// tests once, print the output, and exit with the test result.
async fn run_cli(args: Cli) -> anyhow::Result<()> {
    let logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .format_timestamp(None)
            .format_module_path(false)
            .build();
    let level = logger.filter();

    let multi = MultiProgress::new();

    LogWrapper::new(multi.clone(), logger).try_init().unwrap();
    log::set_max_level(level);

    let config = fs::read_to_string(args.path.join("jest-companion.toml")).await?;
    let config: Config = toml::from_str(&config).context("Failed to parse config file")?;

    let spinner = multi.add(ProgressBar::new_spinner());
    spinner.set_style(ProgressStyle::default_spinner());
    spinner.set_message("Waiting for plugin");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let coord = Arc::new(CliCoord {
        spinner: Mutex::new(spinner),
        plugin_connected: Mutex::new(false),
        received_first_write: Mutex::new(false),
        duplicate_poll_warned: Mutex::new(false),
    });

    let state = AppState {
        args: Arc::new(args),
        config: Arc::new(config),
        coord: Coordinator::Cli(coord),
    };

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CLI_PORT)).await?;

    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(state.args.server_timeout)).await;

            if let Coordinator::Cli(coord) = &state.coord {
                let spinner = coord.spinner.lock().await;
                spinner.finish_and_clear();
            }

            error!("No places have reported anything. Studio might not be open?");
            std::process::exit(1);
        })
    };

    axum::serve(listener, router(state)).await?;

    Ok(())
}

/// Run as an MCP server: keep the HTTP server alive for the whole session and let `run_tests` tool
/// calls drive individual runs through the [`McpCoord`].
async fn run_mcp(args: Cli) -> anyhow::Result<()> {
    // The MCP protocol owns stdout, so logs must go to stderr and we skip the spinner entirely.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .format_module_path(false)
        .target(env_logger::Target::Stderr)
        .init();

    // Unlike the CLI, a missing/invalid config shouldn't take down the server (which would just
    // make the client's connection fail). Start anyway; run_tests reports the problem clearly.
    let config = match fs::read_to_string(args.path.join("jest-companion.toml")).await {
        Ok(contents) => match toml::from_str::<Config>(&contents) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to parse jest-companion.toml: {e}");
                Config::default()
            }
        },
        Err(e) => {
            warn!(
                "Could not read jest-companion.toml ({e}); run_tests will fail until it exists. \
                 Looked in {}",
                args.path.display()
            );
            Config::default()
        }
    };

    let args = Arc::new(args);
    let config = Arc::new(config);

    // The HTTP server (which the Studio plugin reports to) is best-effort. If the port is already
    // taken — almost always another jest-companion MCP server that's still running — binding it
    // must NOT be fatal, or this process would exit before completing the stdio handshake and the
    // client would just see "failed to connect". Serve the stdio connection regardless; run_tests
    // reports clearly when this instance doesn't own the port.
    let http_available = match tokio::net::TcpListener::bind(("127.0.0.1", MCP_PORT)).await {
        Ok(listener) => {
            let coord = Arc::new(McpCoord::new(true));
            let state = AppState {
                args: args.clone(),
                config: config.clone(),
                coord: Coordinator::Mcp(coord.clone()),
            };
            tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, router(state)).await {
                    error!("HTTP server error: {e}");
                }
            });
            return mcp::serve(coord, args, config).await;
        }
        Err(e) => {
            warn!(
                "Could not bind 127.0.0.1:{MCP_PORT} ({e}). Another jest-companion MCP server is \
                 probably already running; this instance will connect but run_tests will be \
                 unavailable."
            );
            false
        }
    };

    let coord = Arc::new(McpCoord::new(http_available));
    // Still serve stdio so the client connects cleanly (run_tests will explain it can't run).
    mcp::serve(coord, args, config).await
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/poll", post(poll))
        .route("/write", post(write))
        .route("/results", post(results))
        .route("/run-error", post(run_error))
        .route("/fs/file/{*path}", put(fs_write))
        .route("/fs/dir/{*path}", put(fs_create_dir_all))
        .route("/fs/exists/{*path}", get(fs_exists))
        .route("/fs/file/{*path}", delete(fs_delete))
        .with_state(state)
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
}

const PROTOCOL_VERSION: &str = "3";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PollRequestBody {
    protocol_version: String,
    rojo_connected: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PollResponseBody {
    projects: Vec<String>,
    options: Value,
    /// Whether the plugin should actually run the tests now. In CLI mode this is always true; in
    /// MCP mode it's only true when a `run_tests` call is waiting, so the plugin can poll quietly
    /// the rest of the time.
    run: bool,
}

async fn poll(
    State(state): State<AppState>,
    Json(body): Json<PollRequestBody>,
) -> impl IntoResponse {
    if body.protocol_version != PROTOCOL_VERSION {
        warn!(
            "The plugin tried to connect with protocol version {} but we are expecting {PROTOCOL_VERSION}. Make sure your versions align.",
            body.protocol_version
        );

        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Incorrect protocol version: expected {PROTOCOL_VERSION}, got {}",
                body.protocol_version
            ),
        )
            .into_response();
    }

    match &state.coord {
        Coordinator::Cli(coord) => {
            let mut plugin_connected = coord.plugin_connected.lock().await;
            if *plugin_connected {
                // Another plugin is polling while one already owns this one-shot run — a second
                // Studio window, or a stale loop left behind by a plugin hot-reload. Tell it to
                // stay idle (run: false) instead of rejecting with a 400, which the plugin treats
                // as an error and retries loudly. Warn once so the situation is still discoverable.
                let mut warned = coord.duplicate_poll_warned.lock().await;
                if !*warned {
                    *warned = true;
                    warn!(
                        "Another plugin polled while one is already running the tests (e.g. a \
                         second Studio window, or a stale plugin from a hot-reload). Ignoring it \
                         for the rest of this run."
                    );
                }
                return (
                    StatusCode::OK,
                    Json(PollResponseBody {
                        projects: Vec::new(),
                        options: Value::Object(Default::default()),
                        run: false,
                    }),
                )
                    .into_response();
            }
            *plugin_connected = true;

            if !body.rojo_connected {
                warn!("Rojo is not connected on the running Studio instance");
            }

            let spinner = coord.spinner.lock().await;
            spinner.set_message("Waiting for test results");

            let projects: Vec<String> = state.config.projects.keys().cloned().collect();
            let options = serde_json::to_value(&state.args.options).unwrap_or(Value::Null);

            (
                StatusCode::OK,
                Json(PollResponseBody {
                    projects,
                    options,
                    run: true,
                }),
            )
                .into_response()
        }
        Coordinator::Mcp(coord) => {
            let mut active = coord.active.lock().await;
            match active.as_mut() {
                Some(run) if !run.dispatched => {
                    run.dispatched = true;

                    if !body.rojo_connected {
                        debug!("Rojo is not connected on the running Studio instance");
                    }

                    (
                        StatusCode::OK,
                        Json(PollResponseBody {
                            projects: run.projects.clone(),
                            options: run.options.clone(),
                            run: true,
                        }),
                    )
                        .into_response()
                }
                // No run pending (or one is already running): tell the plugin to stay idle.
                _ => (
                    StatusCode::OK,
                    Json(PollResponseBody {
                        projects: Vec::new(),
                        options: Value::Object(Default::default()),
                        run: false,
                    }),
                )
                    .into_response(),
            }
        }
    }
}

async fn write(State(state): State<AppState>, data: String) -> impl IntoResponse {
    match &state.coord {
        Coordinator::Cli(coord) => {
            let spinner = coord.spinner.lock().await;
            let mut received_first_write = coord.received_first_write.lock().await;
            if !*received_first_write {
                *received_first_write = true;
                spinner.set_message("Receiving results");
            }
            spinner.println(&data);
        }
        Coordinator::Mcp(coord) => {
            let mut active = coord.active.lock().await;
            if let Some(run) = active.as_mut() {
                run.output.push_str(&data);
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct Results {
    success: bool,
}

async fn results(State(state): State<AppState>, Json(results): Json<Results>) -> impl IntoResponse {
    match &state.coord {
        Coordinator::Cli(coord) => {
            let spinner = coord.spinner.lock().await;
            spinner.finish_and_clear();

            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                std::process::exit(if results.success { 0 } else { 1 });
            });
        }
        Coordinator::Mcp(coord) => {
            let coord = coord.clone();
            tokio::spawn(async move {
                // The plugin sends writes fire-and-forget, so wait a beat for trailing output
                // before we declare the run done (mirrors the CLI's pre-exit grace period).
                tokio::time::sleep(Duration::from_millis(150)).await;
                let mut active = coord.active.lock().await;
                if let Some(run) = active.as_mut()
                    && let Some(tx) = run.done.take()
                {
                    let _ = tx.send(mcp::Outcome::Finished(results.success));
                }
            });
        }
    }

    (StatusCode::OK, ())
}

async fn run_error(State(state): State<AppState>) -> impl IntoResponse {
    match &state.coord {
        Coordinator::Cli(coord) => {
            let spinner = coord.spinner.lock().await;
            spinner.finish_and_clear();

            error!("The test runner encountered an error. See the Studio output for more details.");

            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                std::process::exit(1);
            });
        }
        Coordinator::Mcp(coord) => {
            let coord = coord.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let mut active = coord.active.lock().await;
                if let Some(run) = active.as_mut()
                    && let Some(tx) = run.done.take()
                {
                    let _ = tx.send(mcp::Outcome::RunError);
                }
            });
        }
    }

    (StatusCode::OK, ())
}

async fn fs_write(
    State(state): State<AppState>,
    AxumPath(virtual_path): AxumPath<String>,
    body: String,
) -> impl IntoResponse {
    match resolve_path(&state.config, &virtual_path, &state.args.path) {
        Some(real_path) => {
            if let Some(parent) = real_path.parent()
                && let Err(e) = fs::create_dir_all(parent).await
            {
                return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
            }
            match fs::write(&real_path, body).await {
                Ok(_) => {
                    debug!("File written: {}", real_path.display());
                    (StatusCode::OK, ()).into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        None => (StatusCode::NOT_FOUND, "Could not resolve path").into_response(),
    }
}

async fn fs_create_dir_all(
    State(state): State<AppState>,
    AxumPath(virtual_path): AxumPath<String>,
) -> impl IntoResponse {
    match resolve_path(&state.config, &virtual_path, &state.args.path) {
        Some(real_path) => match fs::create_dir_all(&real_path).await {
            Ok(_) => {
                debug!("Directory created: {}", real_path.display());
                (StatusCode::OK, ()).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        None => (StatusCode::NOT_FOUND, "Could not resolve path").into_response(),
    }
}

async fn fs_exists(
    State(state): State<AppState>,
    AxumPath(virtual_path): AxumPath<String>,
) -> impl IntoResponse {
    match resolve_path(&state.config, &virtual_path, &state.args.path) {
        Some(real_path) => match fs::metadata(&real_path).await {
            Ok(_) => (StatusCode::OK, ()).into_response(),
            Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
        },
        None => (StatusCode::NOT_FOUND, "Could not resolve path").into_response(),
    }
}

async fn fs_delete(
    State(state): State<AppState>,
    AxumPath(virtual_path): AxumPath<String>,
) -> impl IntoResponse {
    match resolve_path(&state.config, &virtual_path, &state.args.path) {
        Some(real_path) => match fs::remove_file(&real_path).await {
            Ok(_) => {
                debug!("File deleted: {}", real_path.display());
                (StatusCode::OK, ()).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        None => (StatusCode::NOT_FOUND, "Could not resolve path").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    fn cli_app() -> Router {
        let coord = Arc::new(CliCoord {
            spinner: Mutex::new(ProgressBar::hidden()),
            plugin_connected: Mutex::new(false),
            received_first_write: Mutex::new(false),
            duplicate_poll_warned: Mutex::new(false),
        });
        let state = AppState {
            args: Arc::new(Cli::parse_from(["jest-companion", "/tmp"])),
            config: Arc::new(Config::default()),
            coord: Coordinator::Cli(coord),
        };
        router(state)
    }

    async fn post_poll(app: &Router, protocol_version: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/poll")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"protocolVersion":"{protocol_version}","rojoConnected":true}}"#
            )))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    // The bug fix: in one-shot CLI mode the first poll runs the tests, but every later poll (a
    // second Studio window, or a stale plugin loop after a hot-reload) used to get a 400 that the
    // plugin retried loudly, flooding the log. Now duplicates get a quiet 200 {run:false}.
    #[tokio::test]
    async fn cli_first_poll_runs_then_duplicates_idle() {
        let app = cli_app();

        let (status, body) = post_poll(&app, PROTOCOL_VERSION).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["run"], Value::Bool(true));

        for _ in 0..3 {
            let (status, body) = post_poll(&app, PROTOCOL_VERSION).await;
            assert_eq!(status, StatusCode::OK, "duplicate polls must not 400");
            assert_eq!(
                body["run"],
                Value::Bool(false),
                "duplicate polls must be told to stay idle"
            );
        }
    }

    #[tokio::test]
    async fn cli_rejects_wrong_protocol_version() {
        let app = cli_app();
        let (status, _) = post_poll(&app, "999").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
