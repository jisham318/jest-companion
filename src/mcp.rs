//! A small [Model Context Protocol](https://modelcontextprotocol.io) server that exposes
//! jest-companion over stdio, so MCP clients (like Claude) can run tests through a tool
//! call instead of shelling out to the CLI.
//!
//! The actual test running still happens exactly like the one-shot CLI: the HTTP server in
//! `main.rs` waits for the Studio plugin to poll, hands it the requested options, and
//! collects the output. The difference is that in MCP mode we keep the HTTP server alive
//! across many runs and route each run's output and result back to the awaiting tool call
//! (via [`McpCoord`]) rather than printing it and exiting the process.

use crate::{cli::Cli, config::Config};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, oneshot},
};

/// Coordinates a single in-flight test run between the MCP tool call and the HTTP handlers.
///
/// Only one run can be active at a time. The tool call inserts an [`ActiveRun`], the `/poll`
/// handler dispatches its options to the plugin, the `/write` handler appends output, and the
/// `/results` (or `/run-error`) handler signals completion through `done`.
pub struct McpCoord {
    pub active: Mutex<Option<ActiveRun>>,
    /// Whether this process managed to bind an HTTP port for the Studio plugin to report to. If it
    /// didn't (every port in the range is already taken), we still serve the stdio connection to
    /// the client, but `run_tests` can't actually drive a run, so it reports that.
    pub http_available: bool,
    /// Set once the Studio plugin has polled us at least once. Lets `run_tests` tell "Studio isn't
    /// open" (never polled) apart from "Studio is open but busy running another agent's suite"
    /// (polled before, just hasn't picked this run up yet) so we don't give up too early while
    /// queued behind another run.
    pub ever_polled: AtomicBool,
}

impl McpCoord {
    pub fn new(http_available: bool) -> Self {
        Self {
            active: Mutex::new(None),
            http_available,
            ever_polled: AtomicBool::new(false),
        }
    }
}

pub struct ActiveRun {
    /// Project names (from the config) to run.
    pub projects: Vec<String>,
    /// The merged runCLI options to hand to the plugin, as a JSON object.
    pub options: Value,
    /// Set once `/poll` has handed this run to the plugin, so we don't dispatch it twice.
    pub dispatched: bool,
    /// Accumulated `/write` output from the plugin.
    pub output: String,
    /// Resolved once the run finishes (or errors). Taken by whichever handler completes first.
    pub done: Option<oneshot::Sender<Outcome>>,
}

/// How a run finished, as reported by the plugin / HTTP handlers.
pub enum Outcome {
    /// `/results` arrived; `bool` is whether every suite passed.
    Finished(bool),
    /// `/run-error` arrived: the plugin hit an error while running.
    RunError,
    /// The plugin never picked the run up in time (Studio probably isn't open).
    NotConnected,
}

/// Run the MCP stdio loop until stdin closes (the client disconnects).
pub async fn serve(coord: Arc<McpCoord>, cli: Arc<Cli>, config: Arc<Config>) -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    debug!("MCP server ready on stdio");

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!("Ignoring invalid JSON-RPC message: {e}");
                continue;
            }
        };

        if let Some(response) = handle_message(&msg, &coord, &cli, &config).await {
            let mut out = serde_json::to_string(&response)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    debug!("stdin closed, shutting down MCP server");
    Ok(())
}

const SERVER_NAME: &str = "jest-companion";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

async fn handle_message(
    msg: &Value,
    coord: &Arc<McpCoord>,
    cli: &Arc<Cli>,
    config: &Arc<Config>,
) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    // Notifications have no `id` and never get a response.
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION)
                .to_string();

            Some(success(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            ))
        }
        "ping" => Some(success(id, json!({}))),
        "tools/list" => Some(success(id, json!({ "tools": tool_definitions() }))),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = call_tool(name, arguments, coord, cli, config).await;
            Some(success(id, result))
        }
        // We don't offer resources or prompts, but answering keeps clients that probe happy.
        "resources/list" => Some(success(id, json!({ "resources": [] }))),
        "prompts/list" => Some(success(id, json!({ "prompts": [] }))),
        // Notifications such as notifications/initialized: nothing to send back.
        _ if id.is_none() => None,
        other => Some(error(id, -32601, format!("Method not found: {other}"))),
    }
}

fn success(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn error(id: Option<Value>, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": { "code": code, "message": message },
    })
}

/// Build an MCP tool result with a single text block.
fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error,
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "run_tests",
            "description": "Run jest-lua tests in the connected Roblox Studio instance and return \
                the test output. Roblox Studio must be open with the jest-companion plugin \
                installed. All arguments are optional; any provided runCLI option overrides the \
                jest-companion CLI default.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "testNamePattern": {
                        "type": "string",
                        "description": "Run only tests whose full name (test name plus surrounding describe blocks) matches this regex."
                    },
                    "testPathPattern": {
                        "type": "string",
                        "description": "Run only tests whose path matches this regex."
                    },
                    "testPathIgnorePatterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Skip tests whose path matches any of these regexes."
                    },
                    "testMatch": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Glob patterns Jest uses to detect test files."
                    },
                    "testTimeout": {
                        "type": "integer",
                        "description": "Default timeout of a test in milliseconds."
                    },
                    "verbose": {
                        "type": "boolean",
                        "description": "Display individual test results with the test suite hierarchy."
                    },
                    "updateSnapshot": {
                        "type": "boolean",
                        "description": "Re-record every snapshot that fails during this run."
                    },
                    "expand": {
                        "type": "boolean",
                        "description": "Show full diffs and errors instead of a patch."
                    },
                    "noStackTrace": {
                        "type": "boolean",
                        "description": "Disable stack traces in the test results output."
                    },
                    "clearMocks": {
                        "type": "boolean",
                        "description": "Clear mock calls, instances, contexts and results before every test."
                    },
                    "resetMocks": {
                        "type": "boolean",
                        "description": "Reset mock state before every test."
                    },
                    "oldFunctionSpying": {
                        "type": "boolean",
                        "description": "Use the old jest.spyOn() behaviour (overwrite the spied method with a mock object)."
                    },
                    "passWithNoTests": {
                        "type": "boolean",
                        "description": "Allow the test suite to pass when no test files are found."
                    },
                    "projects": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subset of configured project names to run. Omit to run every configured project. Use list_projects to see the names."
                    }
                }
            }
        },
        {
            "name": "list_projects",
            "description": "List the project names configured in jest-companion.toml that can be passed to run_tests.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

async fn call_tool(
    name: &str,
    arguments: Value,
    coord: &Arc<McpCoord>,
    cli: &Arc<Cli>,
    config: &Arc<Config>,
) -> Value {
    match name {
        "run_tests" => run_tests_tool(arguments, coord, cli, config).await,
        "list_projects" => list_projects_tool(config),
        other => text_result(format!("Unknown tool: {other}"), true),
    }
}

fn list_projects_tool(config: &Config) -> Value {
    if config.projects.is_empty() {
        return text_result(
            "No projects are configured. Add a [projects] table to jest-companion.toml.",
            false,
        );
    }

    let mut lines: Vec<String> = config
        .projects
        .iter()
        .map(|(name, path)| format!("- {name} -> {}", path.display()))
        .collect();
    lines.sort();

    text_result(format!("Configured projects:\n{}", lines.join("\n")), false)
}

/// runCLI options accepted by the `run_tests` tool. Mirrors [`crate::cli::JestOptions`] but with
/// plain optional fields so it both deserializes cleanly from tool arguments and serializes to a
/// JSON object containing only the keys the caller provided (`skip_serializing_if`).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    clear_mocks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expand: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_stack_trace: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_function_spying: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_with_no_tests: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_mocks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_match: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_name_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_path_ignore_patterns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_path_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verbose: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_snapshot: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunTestsParams {
    #[serde(flatten)]
    options: ToolOptions,
    #[serde(default)]
    projects: Option<Vec<String>>,
}

async fn run_tests_tool(
    arguments: Value,
    coord: &Arc<McpCoord>,
    cli: &Arc<Cli>,
    config: &Arc<Config>,
) -> Value {
    let params: RunTestsParams = match serde_json::from_value(arguments) {
        Ok(params) => params,
        Err(e) => return text_result(format!("Invalid arguments for run_tests: {e}"), true),
    };

    match run_tests(params, coord, cli, config).await {
        Ok((output, success)) => {
            let header = if success {
                "Test run PASSED"
            } else {
                "Test run FAILED"
            };
            let body = if output.trim().is_empty() {
                "(the plugin reported no output)".to_string()
            } else {
                output
            };
            // A completed run is not a tool error, even when tests fail; the header makes the
            // outcome obvious and `isError` is reserved for "couldn't run at all".
            text_result(format!("{header}\n\n{body}"), false)
        }
        Err(e) => text_result(format!("{e}"), true),
    }
}

async fn run_tests(
    params: RunTestsParams,
    coord: &Arc<McpCoord>,
    cli: &Arc<Cli>,
    config: &Arc<Config>,
) -> anyhow::Result<(String, bool)> {
    if !coord.http_available {
        anyhow::bail!(
            "This jest-companion MCP server could not bind its port, so it can't reach the Studio \
             plugin. Another jest-companion MCP server is most likely already running — use that \
             one, or stop it and restart this server."
        );
    }

    let all_projects: Vec<String> = config.projects.keys().cloned().collect();
    if all_projects.is_empty() {
        anyhow::bail!(
            "No projects are configured. Add a [projects] table to jest-companion.toml and restart the MCP server."
        );
    }

    let projects = match &params.projects {
        Some(requested) => {
            if requested.is_empty() {
                anyhow::bail!("`projects` was provided but empty; omit it to run every project.");
            }
            for name in requested {
                if !all_projects.contains(name) {
                    anyhow::bail!(
                        "Unknown project '{name}'. Configured projects: {}.",
                        all_projects.join(", ")
                    );
                }
            }
            requested.clone()
        }
        None => all_projects,
    };

    // Merge: start from the CLI defaults, then overlay only the keys the tool call provided.
    let mut options = serde_json::to_value(&cli.options)?;
    let overlay = serde_json::to_value(&params.options)?;
    if let (Value::Object(base), Value::Object(over)) = (&mut options, overlay) {
        for (key, value) in over {
            base.insert(key, value);
        }
    }

    let (tx, rx) = oneshot::channel();
    {
        let mut active = coord.active.lock().await;
        if active.is_some() {
            anyhow::bail!("A test run is already in progress; wait for it to finish.");
        }
        *active = Some(ActiveRun {
            projects,
            options,
            dispatched: false,
            output: String::new(),
            done: Some(tx),
        });
    }

    // If Studio never even polls us, give up after the connect timeout so the tool call doesn't
    // hang. But if Studio HAS polled us before, it's open — it just hasn't picked this run up yet
    // (likely busy running another agent's suite), so keep waiting up to the full run timeout.
    let watchdog = {
        let coord = coord.clone();
        let connect_timeout = Duration::from_secs(cli.server_timeout);
        tokio::spawn(async move {
            tokio::time::sleep(connect_timeout).await;
            if coord.ever_polled.load(Ordering::Relaxed) {
                return;
            }
            let mut active = coord.active.lock().await;
            if let Some(run) = active.as_mut()
                && !run.dispatched
                && let Some(tx) = run.done.take()
            {
                let _ = tx.send(Outcome::NotConnected);
            }
        })
    };

    let outcome = tokio::time::timeout(Duration::from_secs(cli.run_timeout), rx).await;
    watchdog.abort();

    // Take the run regardless of how it ended so the next call starts clean.
    let output = {
        let mut active = coord.active.lock().await;
        active
            .take()
            .map(|run| strip_ansi(&run.output))
            .unwrap_or_default()
    };

    match outcome {
        Ok(Ok(Outcome::Finished(success))) => Ok((output, success)),
        Ok(Ok(Outcome::NotConnected)) => anyhow::bail!(
            "Roblox Studio did not pick up the test run within {}s. Make sure Studio is open with a place that has the jest-companion plugin installed.",
            cli.server_timeout
        ),
        Ok(Ok(Outcome::RunError)) => {
            if output.trim().is_empty() {
                anyhow::bail!(
                    "The test runner reported an error. Check the Studio output for details."
                )
            } else {
                anyhow::bail!("The test runner reported an error. Captured output:\n\n{output}")
            }
        }
        // Sender dropped without sending (shouldn't normally happen).
        Ok(Err(_)) => anyhow::bail!("The test run ended unexpectedly."),
        Err(_) => anyhow::bail!(
            "Timed out after {}s waiting for test results.",
            cli.run_timeout
        ),
    }
}

/// Remove ANSI escape sequences (Jest's output is colored via Chalk) so the text returned to the
/// MCP client is clean.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\u{1B}' {
            out.push(c);
            continue;
        }

        if chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            // Consume parameter/intermediate bytes until the final byte (0x40..=0x7E).
            while let Some(&next) = chars.peek() {
                chars.next();
                if ('\u{40}'..='\u{7E}').contains(&next) {
                    break;
                }
            }
        } else {
            // Some other escape; drop it and the following byte.
            chars.next();
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_color_codes() {
        let colored = "\u{1B}[31mFAIL\u{1B}[39m \u{1B}[1msuite\u{1B}[22m";
        assert_eq!(strip_ansi(colored), "FAIL suite");
    }

    #[test]
    fn strips_256_color_codes_and_keeps_unicode() {
        let colored = "\u{1B}[38;5;123m✓ passed — ok\u{1B}[39m";
        assert_eq!(strip_ansi(colored), "✓ passed — ok");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
    }

    #[test]
    fn tool_options_only_serialize_provided_fields() {
        let opts = ToolOptions {
            test_name_pattern: Some("auth".to_string()),
            verbose: Some(true),
            ..Default::default()
        };
        let value = serde_json::to_value(&opts).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert_eq!(obj.get("testNamePattern").unwrap(), "auth");
        assert_eq!(obj.get("verbose").unwrap(), true);
    }

    #[test]
    fn run_tests_params_split_options_from_projects() {
        let params: RunTestsParams = serde_json::from_value(json!({
            "testNamePattern": "auth",
            "projects": ["Foo", "Bar"],
        }))
        .unwrap();
        assert_eq!(
            params.projects,
            Some(vec!["Foo".to_string(), "Bar".to_string()])
        );
        assert_eq!(params.options.test_name_pattern.as_deref(), Some("auth"));
    }
}
