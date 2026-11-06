//! MCP stdio and HTTP client transports plus persisted server configuration.
//!
//! The transport deliberately uses `Command::arg` rather than a shell. A configured
//! server is only launched by an explicit `connect`/`reload` call.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

const PROTOCOL_LEGACY: &str = "2025-11-25";
const PROTOCOL_MODERN: &str = "2026-07-28";
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_TOOLS: usize = 512;
const MAX_TOOL_PAGES: usize = 512;
const MAX_TOOL_DATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_QUEUED_FRAMES: usize = 8;
const MAX_ENV_VARS: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 32 * 1024;
const MAX_ENV_TOTAL_BYTES: usize = 1024 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const KEYRING_SERVICE: &str = "JARVUS-mcp";
const MAX_HTTP_BODY_BYTES: usize = MAX_RESULT_BYTES;
const MCP_HTTP_PROTOCOL: &str = "2025-11-25";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    /// Stable, user-chosen identifier used for tool namespacing and secret storage.
    pub id: String,
    /// Program to launch for a local stdio server. Empty for a remote (HTTP) server.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Endpoint of a remote server (streamable HTTP or legacy SSE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// True for the older "SSE" transport (GET event stream plus POST endpoint).
    #[serde(default, skip_serializing_if = "is_false")]
    pub sse: bool,
    /// Environment values are saved in the OS credential store, not in the config file.
    #[serde(skip)]
    pub env: BTreeMap<String, String>,
    /// Request headers (often `Authorization`) are secrets: credential store only.
    #[serde(skip)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url)
            .field("env", &"[REDACTED]")
            .field("headers", &"[REDACTED]")
            .field("timeout_secs", &self.timeout_secs)
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}
fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpTool {
    pub server_id: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct McpToolResult {
    pub content: Vec<Value>,
    pub is_error: bool,
    /// Full server result up to the transport's bounded response limit.
    pub raw: Value,
}

#[derive(Default)]
struct Connection {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    responses: Option<Receiver<Result<Value, String>>>,
    next_id: u64,
    timeout: Duration,
    protocol_version: Option<String>,
    remote: Option<RemoteTransport>,
    tools: Vec<McpTool>,
    /// Set false as soon as a stdio server closes stdout or its reader fails.
    process_alive: Option<Arc<AtomicBool>>,
}

enum RemoteTransport {
    StreamableHttp {
        client: reqwest::blocking::Client,
        endpoint: reqwest::Url,
        session_id: Option<String>,
        protocol_version: Option<String>,
        headers: BTreeMap<String, String>,
    },
    LegacySse {
        client: reqwest::blocking::Client,
        endpoint: reqwest::Url,
        receiver: Receiver<Result<SseMessage, String>>,
        headers: BTreeMap<String, String>,
    },
}

enum SseMessage {
    Endpoint(reqwest::Url),
    Json(Value),
}

impl RemoteTransport {
    fn connect_sse(
        client: reqwest::blocking::Client,
        endpoint: reqwest::Url,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
        id: &str,
    ) -> Result<Self, String> {
        let mut request = client
            .get(endpoint.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream");
        request = apply_http_headers(request, headers)?;
        let response = request
            .send()
            .map_err(|error| format!("Could not open MCP SSE stream '{id}': {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "MCP SSE endpoint '{id}' returned HTTP {}",
                response.status()
            ));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
        {
            return Err(format!(
                "MCP SSE endpoint '{id}' did not return text/event-stream"
            ));
        }
        let (tx, receiver) = mpsc::sync_channel(16);
        thread::Builder::new()
            .name(format!("mcp-{id}-sse"))
            .spawn(move || read_sse_events(response, endpoint, tx))
            .map_err(|error| format!("Could not start MCP SSE reader: {error}"))?;
        let message = receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => format!(
                    "MCP SSE endpoint '{id}' did not advertise a message endpoint before timeout"
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    format!("MCP SSE endpoint '{id}' closed before advertising a message endpoint")
                }
            })??;
        let SseMessage::Endpoint(endpoint) = message else {
            return Err(format!(
                "MCP SSE endpoint '{id}' sent a message before its endpoint event"
            ));
        };
        Ok(Self::LegacySse {
            client,
            endpoint,
            receiver,
            headers: headers.clone(),
        })
    }

    fn request(
        &mut self,
        value: Value,
        timeout: Duration,
        expect_response: bool,
    ) -> Result<Value, String> {
        match self {
            Self::StreamableHttp {
                client,
                endpoint,
                session_id,
                protocol_version,
                headers,
            } => {
                let mut request = client
                    .post(endpoint.clone())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    );
                if let Some(version) = protocol_version.as_deref() {
                    request = request.header("MCP-Protocol-Version", version);
                }
                if let Some(session) = session_id.as_deref() {
                    request = request.header("Mcp-Session-Id", session);
                }
                request = apply_http_headers(request, headers)?;
                let response = request
                    .json(&value)
                    .timeout(timeout)
                    .send()
                    .map_err(|error| format!("MCP Streamable HTTP request failed: {error}"))?;
                let status = response.status();
                let new_session = response
                    .headers()
                    .get("Mcp-Session-Id")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let selected_version = response
                    .headers()
                    .get("MCP-Protocol-Version")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                if let Some(value) = new_session {
                    *session_id = Some(value);
                }
                if let Some(value) = selected_version {
                    *protocol_version = Some(value);
                }
                if status == reqwest::StatusCode::ACCEPTED && !expect_response {
                    return Ok(json!({}));
                }
                if !status.is_success() {
                    return Err(format!(
                        "MCP Streamable HTTP endpoint returned HTTP {status}"
                    ));
                }
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let body = read_bounded_response(response)?;
                if body.is_empty() && !expect_response {
                    return Ok(json!({}));
                }
                if body.len() > MAX_HTTP_BODY_BYTES {
                    return Err("MCP HTTP response exceeded the 1 MiB limit".into());
                }
                if content_type.starts_with("text/event-stream")
                    || body.starts_with(b"event:")
                    || body.starts_with(b"data:")
                {
                    return parse_sse_response(&body, value.get("id").and_then(Value::as_u64));
                }
                serde_json::from_slice(&body)
                    .map_err(|error| format!("MCP HTTP response was not valid JSON: {error}"))
            }
            Self::LegacySse {
                client,
                endpoint,
                receiver,
                headers,
            } => {
                let mut request = client
                    .post(endpoint.clone())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    );
                request = apply_http_headers(request, headers)?;
                let response = request
                    .json(&value)
                    .timeout(timeout)
                    .send()
                    .map_err(|error| format!("MCP SSE message POST failed: {error}"))?;
                if !response.status().is_success() {
                    return Err(format!(
                        "MCP SSE message endpoint returned HTTP {}",
                        response.status()
                    ));
                }
                let body = read_bounded_response(response)?;
                if !body.is_empty() {
                    if let Ok(json) = serde_json::from_slice::<Value>(&body) {
                        return Ok(json);
                    }
                    if let Ok(json) =
                        parse_sse_response(&body, value.get("id").and_then(Value::as_u64))
                    {
                        return Ok(json);
                    }
                }
                if !expect_response {
                    return Ok(json!({}));
                }
                let wanted_id = value.get("id").and_then(Value::as_u64);
                let deadline = Instant::now() + timeout;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err("MCP SSE response timed out".into());
                    }
                    let event =
                        receiver
                            .recv_timeout(remaining)
                            .map_err(|error| match error {
                                mpsc::RecvTimeoutError::Timeout => {
                                    "MCP SSE response timed out".to_owned()
                                }
                                mpsc::RecvTimeoutError::Disconnected => {
                                    "MCP SSE connection closed".to_owned()
                                }
                            })??;
                    let SseMessage::Json(message) = event else {
                        continue;
                    };
                    if message.get("id").and_then(Value::as_u64) == wanted_id {
                        return Ok(message);
                    }
                }
            }
        }
    }
}

fn apply_http_headers(
    mut request: reqwest::blocking::RequestBuilder,
    headers: &BTreeMap<String, String>,
) -> Result<reqwest::blocking::RequestBuilder, String> {
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "origin"
                | "content-type"
                | "accept"
                | "mcp-protocol-version"
                | "mcp-session-id"
                | "connection"
                | "content-length"
        ) {
            return Err(format!(
                "MCP custom header '{name}' is reserved by the transport"
            ));
        }
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "MCP custom header name is invalid")?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| "MCP custom header value is invalid")?;
        request = request.header(name, value);
    }
    Ok(request)
}

fn read_bounded_response(response: reqwest::blocking::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_BODY_BYTES as u64)
    {
        return Err("MCP HTTP response exceeded the 1 MiB limit".into());
    }
    let mut body = Vec::new();
    response
        .take((MAX_HTTP_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("Could not read MCP HTTP response: {error}"))?;
    if body.len() > MAX_HTTP_BODY_BYTES {
        return Err("MCP HTTP response exceeded the 1 MiB limit".into());
    }
    Ok(body)
}

fn parse_sse_response(body: &[u8], wanted_id: Option<u64>) -> Result<Value, String> {
    let text = std::str::from_utf8(body).map_err(|_| "MCP SSE response was not UTF-8")?;
    let mut data = String::new();
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        } else if line.is_empty() && !data.is_empty() {
            let message: Value = serde_json::from_str(&data)
                .map_err(|error| format!("MCP SSE event contained invalid JSON: {error}"))?;
            if wanted_id.is_none() || message.get("id").and_then(Value::as_u64) == wanted_id {
                return Ok(message);
            }
            data.clear();
        }
    }
    Err("MCP SSE response did not contain the matching JSON-RPC response".into())
}

fn resolve_sse_endpoint(base_url: &reqwest::Url, advertised: &str) -> Result<reqwest::Url, String> {
    let url = base_url
        .join(advertised.trim())
        .map_err(|error| format!("MCP SSE endpoint URL is invalid: {error}"))?;
    if url.origin() != base_url.origin() || !url.username().is_empty() || url.password().is_some() {
        return Err("MCP SSE endpoint must remain on the same origin as its event stream".into());
    }
    Ok(url)
}

fn read_sse_events(
    response: reqwest::blocking::Response,
    base_url: reqwest::Url,
    tx: mpsc::SyncSender<Result<SseMessage, String>>,
) {
    let mut reader = BufReader::new(response);
    let mut event = String::new();
    let mut data = String::new();
    loop {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            match reader.read(&mut byte) {
                Ok(0) => {
                    let _ = tx.send(Err("MCP SSE connection closed".into()));
                    return;
                }
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    if bytes.len() >= MAX_HTTP_BODY_BYTES {
                        let _ = tx.send(Err("MCP SSE event exceeded size limit".into()));
                        return;
                    }
                    bytes.push(byte[0]);
                }
                Err(error) => {
                    let _ = tx.send(Err(format!("Could not read MCP SSE stream: {error}")));
                    return;
                }
            }
        }
        let line = match std::str::from_utf8(&bytes) {
            Ok(line) => line.trim_end_matches('\r'),
            Err(_) => {
                let _ = tx.send(Err("MCP SSE stream contained invalid UTF-8".into()));
                return;
            }
        };
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("data:") {
            if data.len().saturating_add(value.len()) > MAX_HTTP_BODY_BYTES {
                let _ = tx.send(Err("MCP SSE event exceeded the 1 MiB limit".into()));
                return;
            }
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        } else if line.is_empty() && !data.is_empty() {
            let message = if event == "endpoint" {
                resolve_sse_endpoint(&base_url, &data).map(SseMessage::Endpoint)
            } else {
                serde_json::from_str::<Value>(&data)
                    .map(SseMessage::Json)
                    .map_err(|error| format!("MCP SSE event contained invalid JSON: {error}"))
            };
            if tx.send(message).is_err() {
                return;
            }
            event.clear();
            data.clear();
        }
    }
}

impl Connection {
    fn start(&mut self, cfg: &McpServerConfig) -> Result<(), String> {
        self.stop();
        validate_config(cfg)?;
        if let Some(url) = &cfg.url {
            self.timeout = Duration::from_secs(cfg.timeout_secs);
            let endpoint = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
            let mut client_builder = reqwest::blocking::Client::builder()
                .connect_timeout(self.timeout.min(Duration::from_secs(10)))
                .redirect(reqwest::redirect::Policy::none());
            if !cfg.sse {
                client_builder = client_builder.timeout(self.timeout);
            }
            let client = client_builder
                .build()
                .map_err(|error| format!("Could not configure MCP HTTP client: {error}"))?;
            self.remote = Some(if cfg.sse {
                RemoteTransport::connect_sse(client, endpoint, &cfg.headers, self.timeout, &cfg.id)?
            } else {
                RemoteTransport::StreamableHttp {
                    client,
                    endpoint,
                    session_id: None,
                    protocol_version: None,
                    headers: cfg.headers.clone(),
                }
            });
            let mut init = None;
            let mut last_error = None;
            for version in [MCP_HTTP_PROTOCOL, "2025-06-18", "2025-03-26", "2024-11-05"] {
                match self.request(
                    "initialize",
                    json!({
                        "protocolVersion": version,
                        "capabilities": {},
                        "clientInfo": {"name":"JARVUS", "version": env!("CARGO_PKG_VERSION")}
                    }),
                    false,
                ) {
                    Ok(result) if result.get("error").is_none() => {
                        init = Some(result);
                        break;
                    }
                    Ok(result) => last_error = result.get("error").map(bounded_json),
                    Err(error) => last_error = Some(error),
                }
            }
            let init = init.ok_or_else(|| {
                format!(
                    "MCP server '{}' did not accept a supported protocol version{}",
                    cfg.id,
                    last_error
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                )
            })?;
            let version = init
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "MCP server '{}' returned an invalid initialize result",
                        cfg.id
                    )
                })?;
            if !["2024-11-05", "2025-03-26", "2025-06-18", MCP_HTTP_PROTOCOL].contains(&version) {
                return Err(format!(
                    "MCP server '{}' selected unsupported protocol version '{version}'",
                    cfg.id
                ));
            }
            self.protocol_version = Some(version.to_owned());
            if let Some(RemoteTransport::StreamableHttp {
                protocol_version, ..
            }) = &mut self.remote
            {
                *protocol_version = Some(version.to_owned());
            }
            self.notify("notifications/initialized", json!({}))?;
            self.tools = self.list_tools(cfg)?;
            return Ok(());
        }
        self.launch_process(cfg)?;

        // Modern MCP servers answer `server/discover` without initialization.
        // A legacy server may instead exit on that first request. In that case
        // the probe process cannot be reused for `initialize`; restart a fresh
        // sibling process for the legacy handshake.
        let version = match self.request("server/discover", json!({}), false) {
            Ok(result)
                if result
                    .pointer("/result/supportedVersions")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().any(|v| v.as_str() == Some(PROTOCOL_MODERN))
                    }) =>
            {
                PROTOCOL_MODERN.to_owned()
            }
            _ => {
                self.stop();
                self.launch_process(cfg)?;
                self.initialize_legacy(cfg)?
            }
        };
        self.protocol_version = Some(version);
        self.tools = self.list_tools(cfg)?;
        Ok(())
    }

    fn launch_process(&mut self, cfg: &McpServerConfig) -> Result<(), String> {
        let mut command = Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // MCP servers are background workers. In particular, shell-backed
            // servers must not create/activate a console that steals focus from
            // the application JEV is controlling.
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        for (key, value) in &cfg.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start MCP server '{}': {e}", cfg.id))?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("MCP process stdin was unavailable".into());
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("MCP process stdout was unavailable".into());
            }
        };
        // Backpressure keeps a noisy child from accumulating unbounded JSON
        // notifications while the caller is idle or waiting for another ID.
        let (tx, rx) = mpsc::sync_channel(MAX_QUEUED_FRAMES);
        let process_alive = Arc::new(AtomicBool::new(true));
        let reader_alive = Arc::clone(&process_alive);
        if let Err(error) = thread::Builder::new()
            .name(format!("mcp-{}-stdout", cfg.id))
            .spawn(move || read_frames_with_health(stdout, tx, reader_alive))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Could not start MCP response reader: {error}"));
        }
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.responses = Some(rx);
        self.process_alive = Some(process_alive);
        self.next_id = 1;
        self.timeout = Duration::from_secs(cfg.timeout_secs);
        Ok(())
    }

    fn initialize_legacy(&mut self, cfg: &McpServerConfig) -> Result<String, String> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_LEGACY,
                "capabilities": {},
                "clientInfo": {"name":"JARVUS", "version": env!("CARGO_PKG_VERSION")}
            }),
            false,
        )?;
        let version = result
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "MCP server '{}' returned an invalid initialize result",
                    cfg.id
                )
            })?;
        let supported = ["2024-11-05", "2025-03-26", "2025-06-18", PROTOCOL_LEGACY];
        if !supported.contains(&version) {
            return Err(format!(
                "MCP server '{}' selected unsupported protocol version '{version}'",
                cfg.id
            ));
        }
        self.notify("notifications/initialized", json!({}))?;
        Ok(version.to_owned())
    }

    fn list_tools(&mut self, cfg: &McpServerConfig) -> Result<Vec<McpTool>, String> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = std::collections::BTreeSet::new();
        let mut total_bytes = 0usize;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_TOOL_PAGES {
                return Err(format!(
                    "MCP server '{}' returned too many tools/list pages",
                    cfg.id
                ));
            }
            let mut params = json!({});
            if let Some(value) = &cursor {
                params["cursor"] = json!(value);
            }
            let result = self.request("tools/list", params, true)?;
            let tools = result
                .pointer("/result/tools")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    format!(
                        "MCP server '{}' returned an invalid tools/list result",
                        cfg.id
                    )
                })?;
            if all.len().saturating_add(tools.len()) > MAX_TOOLS {
                return Err(format!(
                    "MCP server '{}' exposed more than {MAX_TOOLS} tools",
                    cfg.id
                ));
            }
            for tool in tools {
                let size = serde_json::to_vec(tool)
                    .map(|bytes| bytes.len())
                    .unwrap_or(usize::MAX);
                total_bytes = total_bytes.saturating_add(size);
                if total_bytes > MAX_TOOL_DATA_BYTES {
                    return Err(format!(
                        "MCP server '{}' returned more than 8 MiB of tool metadata",
                        cfg.id
                    ));
                }
            }
            all.extend(tools.iter().cloned());
            cursor = result
                .pointer("/result/nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
            if !seen_cursors.insert(cursor.clone().unwrap()) {
                return Err(format!(
                    "MCP server '{}' repeated a tools/list pagination cursor",
                    cfg.id
                ));
            }
        }
        all.iter()
            .map(|tool| {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("MCP tool is missing its name")?;
                if name.is_empty() || name.len() > 256 {
                    return Err("MCP tool name is invalid".into());
                }
                Ok(McpTool {
                    server_id: cfg.id.clone(),
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object"})),
                })
            })
            .collect()
    }

    fn request(
        &mut self,
        method: &str,
        mut params: Value,
        include_modern_meta: bool,
    ) -> Result<Value, String> {
        if include_modern_meta && self.protocol_version.as_deref() == Some(PROTOCOL_MODERN) {
            if let Some(object) = params.as_object_mut() {
                object.insert("_meta".into(), json!({
                    "io.modelcontextprotocol/protocolVersion": PROTOCOL_MODERN,
                    "io.modelcontextprotocol/clientInfo": {"name":"JARVUS", "version":env!("CARGO_PKG_VERSION")},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }));
            }
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        if let Some(remote) = &mut self.remote {
            let response = remote.request(request, self.timeout, true)?;
            if response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
                || response.get("id").and_then(Value::as_u64) != Some(id)
            {
                return Err(format!(
                    "MCP '{method}' returned a mismatched JSON-RPC response"
                ));
            }
            if let Some(error) = response.get("error") {
                return Err(format!("MCP '{method}' failed: {}", bounded_json(error)));
            }
            let size = serde_json::to_vec(&response)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX);
            if size > MAX_RESULT_BYTES {
                return Err(format!("MCP response exceeded {MAX_RESULT_BYTES} bytes"));
            }
            return Ok(response);
        }
        self.send(request)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("MCP request '{method}' timed out"));
            }
            let value = self
                .responses
                .as_ref()
                .ok_or("MCP server is disconnected")?
                .recv_timeout(remaining)
                .map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => format!("MCP request '{method}' timed out"),
                    mpsc::RecvTimeoutError::Disconnected => {
                        "MCP server closed its output".to_owned()
                    }
                })??;
            if value.get("method").and_then(Value::as_str) == Some("ping") {
                if let Some(ping_id) = value.get("id") {
                    self.send(json!({"jsonrpc":"2.0", "id":ping_id, "result":{}}))?;
                }
                continue;
            }
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(format!("MCP '{method}' failed: {}", bounded_json(error)));
            }
            let size = serde_json::to_vec(&value)
                .map(|v| v.len())
                .unwrap_or(usize::MAX);
            if size > MAX_RESULT_BYTES {
                return Err(format!("MCP response exceeded {MAX_RESULT_BYTES} bytes"));
            }
            return Ok(value);
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(json!({"jsonrpc":"2.0", "method":method, "params":params}))
    }

    fn send(&mut self, value: Value) -> Result<(), String> {
        if let Some(remote) = &mut self.remote {
            let _ = remote.request(value, self.timeout, false)?;
            return Ok(());
        }
        let mut bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_RESULT_BYTES {
            return Err(format!("MCP request exceeded {MAX_RESULT_BYTES} bytes"));
        }
        bytes.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or("MCP server is disconnected")?;
        stdin
            .write_all(&bytes)
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("Could not write to MCP server: {e}"))
    }

    fn call(&mut self, name: &str, arguments: Value) -> Result<McpToolResult, String> {
        let response = self.request(
            "tools/call",
            json!({"name":name, "arguments":arguments}),
            true,
        )?;
        let raw = response
            .get("result")
            .cloned()
            .ok_or("MCP tools/call response did not include a result")?;
        let size = serde_json::to_vec(&raw)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > MAX_RESULT_BYTES {
            return Err(format!("MCP tool result exceeded {MAX_RESULT_BYTES} bytes"));
        }
        Ok(McpToolResult {
            content: raw
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            is_error: raw.get("isError").and_then(Value::as_bool).unwrap_or(false),
            raw,
        })
    }

    fn stop(&mut self) {
        if let Some(alive) = self.process_alive.take() {
            alive.store(false, Ordering::Release);
        }
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.responses.take();
        self.remote.take();
        self.protocol_version = None;
        self.tools.clear();
    }

    fn is_alive(&self) -> bool {
        self.process_alive
            .as_ref()
            .is_none_or(|alive| alive.load(Ordering::Acquire))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A connection made off-lock by `connect_detached`, waiting to be installed.
pub struct PendingConnection(Connection);

/// Owns configured MCP processes. `load` reads configuration only; it never launches servers.
pub struct McpManager {
    path: PathBuf,
    configs: Vec<McpServerConfig>,
    connections: BTreeMap<String, Connection>,
    /// Last connection error per server, for the health display.
    errors: BTreeMap<String, String>,
}

/// Placeholder shown instead of a saved secret in an editable server block.
/// Leaving it unchanged keeps the stored value.
pub const SAVED_SECRET: &str = "<saved>";

/// Result of merging pasted server definitions into the existing configuration.
#[derive(Debug, Default)]
pub struct MergeReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub connected: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl MergeReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("added {}", self.added.join(", ")));
        }
        if !self.updated.is_empty() {
            parts.push(format!("updated {}", self.updated.join(", ")));
        }
        if !self.unchanged.is_empty() {
            parts.push(format!("unchanged {}", self.unchanged.join(", ")));
        }
        if !self.connected.is_empty() {
            parts.push(format!("connected {}", self.connected.join(", ")));
        }
        for (id, error) in &self.failed {
            parts.push(format!("{id} failed to connect: {error}"));
        }
        if parts.is_empty() {
            "No MCP servers were found in that config.".into()
        } else {
            format!("MCP: {}.", parts.join("; "))
        }
    }
}

/// Parse the server document apps hand out: `{"mcpServers": {"Name": {"command":
/// ..., "args": [...], "env": {...}}}}` or a remote `{"url": ..., "headers": {...}}`. Also accepts VS Code's `"servers"`, a
/// bare `{"Name": {...}}` map, or a single server object with an `"id"`/`"name"`.
pub fn parse_server_document(text: &str) -> Result<Vec<McpServerConfig>, String> {
    let text = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("That is not valid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or("The MCP config must be a JSON object.")?;
    let servers = object
        .get("mcpServers")
        .or_else(|| object.get("servers"))
        .and_then(Value::as_object);
    let entries: Vec<(String, &Value)> = match servers {
        Some(map) => map
            .iter()
            .map(|(name, server)| (name.clone(), server))
            .collect(),
        None if object.contains_key("command") => {
            let name = object
                .get("id")
                .or_else(|| object.get("name"))
                .and_then(Value::as_str)
                .ok_or("A single server needs an \"id\" or \"name\".")?;
            vec![(name.to_owned(), &value)]
        }
        None => object
            .iter()
            .filter(|(_, server)| server.get("command").is_some() || server.get("url").is_some())
            .map(|(name, server)| (name.clone(), server))
            .collect(),
    };
    if entries.is_empty() {
        return Err(
            "No MCP servers were found. Expected {\"mcpServers\": {\"name\": {\"command\": ...}}}."
                .into(),
        );
    }
    let mut configs = Vec::new();
    for (name, server) in entries {
        let id: String = name
            .trim()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        let url = server
            .get("url")
            .or_else(|| server.get("serverUrl"))
            .or_else(|| server.get("httpUrl"))
            .and_then(Value::as_str)
            .map(|u| u.trim().to_owned());
        let kind = server
            .get("type")
            .or_else(|| server.get("transport"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let command = if url.is_some() && server.get("command").is_none() {
            String::new()
        } else {
            server
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("'{name}' has no \"command\" or \"url\"."))?
                .to_owned()
        };
        let sse = url.as_deref().is_some_and(|u| {
            matches!(kind.as_str(), "sse" | "legacy-sse")
                || (kind.is_empty() && u.trim_end_matches('/').ends_with("/sse"))
        });
        let mut headers = BTreeMap::new();
        if let Some(map) = server.get("headers").and_then(Value::as_object) {
            for (key, value) in map {
                headers.insert(
                    key.clone(),
                    match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    },
                );
            }
        }
        let args = match server.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) => Ok(text.clone()),
                    Value::Number(number) => Ok(number.to_string()),
                    Value::Bool(flag) => Ok(flag.to_string()),
                    _ => Err(format!("'{name}' args must be strings.")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(format!("'{name}' args must be an array.")),
        };
        let mut env = BTreeMap::new();
        if let Some(map) = server.get("env").and_then(Value::as_object) {
            for (key, value) in map {
                let value = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                env.insert(key.clone(), value);
            }
        }
        let timeout_secs = server
            .get("timeout")
            .or_else(|| server.get("timeout_secs"))
            .and_then(Value::as_u64)
            .map(|value| if value > 1000 { value / 1000 } else { value })
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, 300);
        let enabled = !server
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && server
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
        let config = McpServerConfig {
            id,
            command,
            args,
            url,
            sse,
            env,
            headers,
            timeout_secs,
            enabled,
        };
        let unmasked = |map: &BTreeMap<String, String>| {
            map.iter()
                .filter(|(_, v)| v.as_str() != SAVED_SECRET)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        validate_config(&McpServerConfig {
            env: unmasked(&config.env),
            headers: unmasked(&config.headers),
            ..config.clone()
        })?;
        configs.push(config);
    }
    Ok(configs)
}

impl McpManager {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            configs: Vec::new(),
            connections: BTreeMap::new(),
            errors: BTreeMap::new(),
        }
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let mut manager = Self::new(path);
        let mut source_path = manager.path.clone();
        if !source_path.exists() && manager.path == default_config_path() {
            if let Some(legacy) = legacy_config_path().filter(|path| path.exists()) {
                source_path = legacy;
            }
        }
        if source_path.exists() {
            let raw = fs::read(&source_path)
                .map_err(|e| format!("Could not read MCP configuration: {e}"))?;
            if raw.len() > MAX_RESULT_BYTES {
                return Err("MCP configuration file is too large".into());
            }
            manager.configs = serde_json::from_slice(&raw)
                .map_err(|e| format!("MCP configuration is invalid: {e}"))?;
            for config in &mut manager.configs {
                validate_config(config)?;
                let (env, headers) = load_credentials(&config.id)?;
                config.env = env;
                config.headers = headers;
                validate_config(config)?;
            }
            if source_path != manager.path {
                save_configs(&manager.path, &manager.configs)?;
            }
        }
        Ok(manager)
    }

    pub fn configs(&self) -> &[McpServerConfig] {
        &self.configs
    }
    pub fn tools(&self) -> Vec<McpTool> {
        self.connections
            .values()
            .filter(|connection| connection.is_alive())
            .flat_map(|c| c.tools.clone())
            .collect()
    }
    pub fn is_connected(&self, id: &str) -> bool {
        self.connections.get(id).is_some_and(Connection::is_alive)
    }

    pub fn tools_for(&self, id: &str) -> Vec<McpTool> {
        self.connections
            .get(id)
            .filter(|connection| connection.is_alive())
            .map(|connection| connection.tools.clone())
            .unwrap_or_default()
    }

    pub fn last_error(&self, id: &str) -> Option<&str> {
        if self
            .connections
            .get(id)
            .is_some_and(|connection| !connection.is_alive())
        {
            return Some("MCP server process exited or closed its output");
        }
        self.errors.get(id).map(String::as_str)
    }

    /// Remove stdio servers whose response reader has observed EOF or an I/O
    /// failure. Tool schemas are filtered immediately by `tools()` as well, so
    /// a dead server is never offered to the model while this refresh runs.
    pub fn refresh_health(&mut self) {
        let dead: Vec<String> = self
            .connections
            .iter()
            .filter(|(_, connection)| !connection.is_alive())
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            self.connections.remove(&id);
            self.errors
                .insert(id, "MCP server process exited or closed its output".into());
        }
    }

    /// This server's definition as a standalone `{"mcpServers": {...}}` block,
    /// with secret environment and header values masked as SAVED_SECRET.
    pub fn server_document(&self, id: &str) -> Option<String> {
        let config = self.configs.iter().find(|config| config.id == id)?;
        let env: serde_json::Map<String, Value> = config
            .env
            .keys()
            .map(|key| (key.clone(), Value::from(SAVED_SECRET)))
            .collect();
        let headers: serde_json::Map<String, Value> = config
            .headers
            .keys()
            .map(|key| (key.clone(), Value::from(SAVED_SECRET)))
            .collect();
        let mut server = if let Some(url) = &config.url {
            json!({"url": url, "transport": if config.sse { "sse" } else { "streamable-http" }})
        } else {
            json!({"command": config.command, "args": config.args})
        };
        if !env.is_empty() {
            server["env"] = Value::Object(env);
        }
        if !headers.is_empty() {
            server["headers"] = Value::Object(headers);
        }
        if config.timeout_secs != DEFAULT_TIMEOUT_SECS {
            server["timeout"] = Value::from(config.timeout_secs);
        }
        if !config.enabled {
            server["disabled"] = Value::Bool(true);
        }
        serde_json::to_string_pretty(&json!({"mcpServers": {config.id.clone(): server}})).ok()
    }

    /// Add or replace only the given servers (matched by id), keeping every
    /// other configured server untouched and running. Masked secrets keep their
    /// saved values. Only new or changed enabled servers are (re)connected.
    pub fn merge_configs(&mut self, incoming: Vec<McpServerConfig>) -> Result<MergeReport, String> {
        let mut report = MergeReport::default();
        let mut configs = self.configs.clone();
        let mut touched = Vec::new();
        for mut config in incoming {
            let existing = configs.iter().position(|item| item.id == config.id);
            if let Some(index) = existing {
                let saved_env = &configs[index].env;
                config.env = config
                    .env
                    .into_iter()
                    .filter_map(|(key, value)| {
                        if value == SAVED_SECRET {
                            saved_env.get(&key).map(|old| (key, old.clone()))
                        } else {
                            Some((key, value))
                        }
                    })
                    .collect();
                let saved_headers = &configs[index].headers;
                config.headers = config
                    .headers
                    .into_iter()
                    .filter_map(|(key, value)| {
                        if value == SAVED_SECRET {
                            saved_headers.get(&key).map(|old| (key, old.clone()))
                        } else {
                            Some((key, value))
                        }
                    })
                    .collect();
                if configs[index] == config && configs[index].env == config.env {
                    report.unchanged.push(config.id.clone());
                    if config.enabled && !self.connections.contains_key(&config.id) {
                        touched.push(config.id.clone());
                    }
                    continue;
                }
                report.updated.push(config.id.clone());
                touched.push(config.id.clone());
                configs[index] = config;
            } else {
                config.env.retain(|_, value| value != SAVED_SECRET);
                config.headers.retain(|_, value| value != SAVED_SECRET);
                report.added.push(config.id.clone());
                touched.push(config.id.clone());
                configs.push(config);
            }
        }
        for config in &configs {
            validate_config(config)?;
        }
        persist_configs_and_credentials(&self.path, &self.configs, &configs, save_credentials)?;
        self.configs = configs;
        for id in touched {
            self.connections.remove(&id);
            let enabled = self
                .configs
                .iter()
                .any(|config| config.id == id && config.enabled);
            if !enabled {
                continue;
            }
            match self.connect(&id) {
                Ok(()) => report.connected.push(id),
                Err(error) => report.failed.push((id, error)),
            }
        }
        Ok(report)
    }

    /// Replace all stored definitions and reload enabled processes immediately.
    pub fn replace_configs(&mut self, configs: Vec<McpServerConfig>) -> Result<(), String> {
        let mut ids = std::collections::BTreeSet::new();
        for config in &configs {
            validate_config(config)?;
            if !ids.insert(config.id.clone()) {
                return Err("MCP server IDs must be unique".into());
            }
        }
        self.refresh_health();
        let previous = self.configs.clone();
        persist_configs_and_credentials(&self.path, &previous, &configs, save_credentials)?;

        let changed: std::collections::BTreeSet<String> = previous
            .iter()
            .filter(|old| configs.iter().find(|new| new.id == old.id) != Some(*old))
            .map(|config| config.id.clone())
            .chain(
                configs
                    .iter()
                    .filter(|new| previous.iter().find(|old| old.id == new.id) != Some(*new))
                    .map(|config| config.id.clone()),
            )
            .collect();
        self.configs = configs;
        self.connections.retain(|id, _| !changed.contains(id));
        self.errors
            .retain(|id, _| self.configs.iter().any(|config| &config.id == id));
        for id in &changed {
            self.errors.remove(id);
        }
        let mut errors = Vec::new();
        let enabled: Vec<String> = self
            .configs
            .iter()
            .filter(|config| config.enabled && !self.connections.contains_key(&config.id))
            .map(|config| config.id.clone())
            .collect();
        for id in enabled {
            if let Err(e) = self.connect(&id) {
                errors.push(e);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    pub fn connect(&mut self, id: &str) -> Result<(), String> {
        let config = self
            .configs
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| format!("Unknown MCP server '{id}'"))?
            .clone();
        if !config.enabled {
            return Err(format!("MCP server '{id}' is disabled"));
        }
        let mut connection = Connection::default();
        if let Err(error) = connection.start(&config) {
            self.errors.insert(id.to_owned(), error.clone());
            return Err(error);
        }
        self.errors.remove(id);
        self.connections.insert(id.to_owned(), connection);
        Ok(())
    }

    pub fn disconnect(&mut self, id: &str) {
        self.connections.remove(id);
    }

    /// Enabled servers that are not connected, so a background task can bring
    /// them up (or back up) without holding the manager lock while it connects.
    pub fn disconnected_enabled(&self) -> Vec<McpServerConfig> {
        self.configs
            .iter()
            .filter(|config| config.enabled && !self.connections.contains_key(&config.id))
            .cloned()
            .collect()
    }

    /// Connect outside the manager lock (a remote server can take seconds).
    pub fn connect_detached(config: &McpServerConfig) -> Result<PendingConnection, String> {
        let mut connection = Connection::default();
        connection.start(config)?;
        Ok(PendingConnection(connection))
    }

    /// Hand a finished detached connection (or its error) back to the manager.
    /// Ignored when the server was removed, disabled, or connected meanwhile.
    pub fn install_connection(&mut self, id: &str, outcome: Result<PendingConnection, String>) {
        let still_wanted = self.configs.iter().any(|config| config.id == id && config.enabled)
            && !self.connections.contains_key(id);
        if !still_wanted {
            return;
        }
        match outcome {
            Ok(PendingConnection(connection)) => {
                self.errors.remove(id);
                self.connections.insert(id.to_owned(), connection);
            }
            Err(error) => {
                self.errors.insert(id.to_owned(), error);
            }
        }
    }

    pub fn reload(&mut self) -> Result<(), String> {
        let ids: Vec<String> = self.connections.keys().cloned().collect();
        for id in &ids {
            self.disconnect(id);
        }
        let enabled: Vec<String> = self
            .configs
            .iter()
            .filter(|c| c.enabled)
            .map(|c| c.id.clone())
            .collect();
        let errors: Vec<String> = enabled
            .iter()
            .filter_map(|id| self.connect(id).err())
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    pub fn call_tool(
        &mut self,
        server_id: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolResult, String> {
        self.refresh_health();
        let connection = self
            .connections
            .get_mut(server_id)
            .ok_or_else(|| format!("MCP server '{server_id}' is not connected"))?;
        if !connection.tools.iter().any(|t| t.name == tool_name) {
            return Err(format!(
                "Unknown tool '{tool_name}' on MCP server '{server_id}'"
            ));
        }
        connection.call(tool_name, arguments)
    }
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".jarvus")
        .join("mcp-servers.json")
}

fn legacy_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .map(|path| path.join("JARVUS").join("mcp-servers.json"))
}

fn validate_config(cfg: &McpServerConfig) -> Result<(), String> {
    if cfg.id.is_empty()
        || cfg.id.len() > 64
        || !cfg
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("MCP server ID must be 1-64 ASCII letters, digits, '-' or '_'".into());
    }
    match &cfg.url {
        Some(url) => {
            let parsed = reqwest::Url::parse(url)
                .map_err(|e| format!("MCP server '{}' url is invalid: {e}", cfg.id))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!(
                    "MCP server '{}' url must start with http:// or https://",
                    cfg.id
                ));
            }
            if parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(format!(
                    "MCP server '{}' URL cannot contain credentials or a fragment",
                    cfg.id
                ));
            }
            if parsed.scheme() == "http" {
                let host = parsed.host_str().unwrap_or_default();
                let loopback = host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback());
                if !loopback {
                    return Err(format!(
                        "MCP server '{}' must use HTTPS for non-local HTTP endpoints",
                        cfg.id
                    ));
                }
            }
            if cfg.headers.len() > MAX_ENV_VARS
                || cfg.headers.iter().any(|(k, v)| {
                    k.is_empty()
                        || k.len() > 256
                        || v.len() > MAX_ENV_VALUE_BYTES
                        || k.bytes().any(|b| b == b'\r' || b == b'\n')
                        || v.bytes().any(|b| b == b'\r' || b == b'\n')
                })
            {
                return Err(format!("MCP server '{}' has an invalid header", cfg.id));
            }
        }
        None if cfg.command.trim().is_empty() || cfg.command.len() > 2048 => {
            return Err(format!("MCP server '{}' command is invalid", cfg.id));
        }
        None => {}
    }
    if cfg.args.len() > 128 || cfg.args.iter().any(|a| a.len() > 8192) {
        return Err(format!("MCP server '{}' arguments exceed limits", cfg.id));
    }
    if !(1..=300).contains(&cfg.timeout_secs) {
        return Err(format!(
            "MCP server '{}' timeout must be from 1 to 300 seconds",
            cfg.id
        ));
    }
    if cfg.env.len() > MAX_ENV_VARS {
        return Err(format!(
            "MCP server '{}' has more than {MAX_ENV_VARS} environment variables",
            cfg.id
        ));
    }
    let mut env_bytes = 0usize;
    for (key, value) in &cfg.env {
        if key.is_empty() || key.len() > 256 || key.contains('=') || key.contains('\0') {
            return Err("MCP environment variable name is invalid".into());
        }
        if value.len() > MAX_ENV_VALUE_BYTES || value.contains('\0') {
            return Err(format!(
                "MCP server '{}' has an invalid or oversized environment value",
                cfg.id
            ));
        }
        env_bytes = env_bytes
            .saturating_add(key.len())
            .saturating_add(value.len());
        if env_bytes > MAX_ENV_TOTAL_BYTES {
            return Err(format!("MCP server '{}' environment exceeds 1 MiB", cfg.id));
        }
    }
    Ok(())
}

fn save_configs(path: &Path, configs: &[McpServerConfig]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create MCP configuration directory: {e}"))?;
    }
    let raw = serde_json::to_vec_pretty(configs).map_err(|e| e.to_string())?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, raw).map_err(|e| format!("Could not write MCP configuration: {e}"))?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(first_error) if path.exists() => {
            // Windows does not consistently replace an existing destination
            // through std::fs::rename. Move the old file to a same-directory
            // backup first, and restore it if committing the staged file fails.
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let backup = path.with_extension(format!("json.backup-{}-{nonce}", std::process::id()));
            fs::rename(path, &backup)
                .map_err(|e| format!("Could not preserve current MCP configuration: {e}"))?;
            match fs::rename(&temp, path) {
                Ok(()) => {
                    let _ = fs::remove_file(backup);
                    Ok(())
                }
                Err(commit_error) => match fs::rename(&backup, path) {
                    Ok(()) => Err(format!(
                        "Could not commit MCP configuration ({first_error}; {commit_error}); previous configuration was restored"
                    )),
                    Err(restore_error) => Err(format!(
                        "Could not commit MCP configuration ({first_error}; {commit_error}) or restore it ({restore_error}); previous file remains at {}",
                        backup.display()
                    )),
                },
            }
        }
        Err(error) => Err(format!("Could not commit MCP configuration: {error}")),
    }
}

/// Persist the new server document and its OS-protected credentials as one
/// recoverable operation. The OS credential store cannot join a filesystem
/// transaction, so each attempted credential write is rolled back from the
/// previous in-memory config if any later stage fails. Callers publish the new
/// config/connection state only after this function succeeds.
fn persist_configs_and_credentials<F>(
    path: &Path,
    previous: &[McpServerConfig],
    next: &[McpServerConfig],
    mut save_credential: F,
) -> Result<(), String>
where
    F: FnMut(&str, &BTreeMap<String, String>, &BTreeMap<String, String>) -> Result<(), String>,
{
    let previous_by_id: BTreeMap<&str, &McpServerConfig> = previous
        .iter()
        .map(|config| (config.id.as_str(), config))
        .collect();
    let next_by_id: BTreeMap<&str, &McpServerConfig> = next
        .iter()
        .map(|config| (config.id.as_str(), config))
        .collect();
    let ids: std::collections::BTreeSet<&str> = previous_by_id
        .keys()
        .chain(next_by_id.keys())
        .copied()
        .collect();
    let mut changes = Vec::new();
    for id in ids {
        let old = previous_by_id.get(id).copied();
        let new = next_by_id.get(id).copied();
        let old_secrets = old.map(|config| (&config.env, &config.headers));
        let new_secrets = new.map(|config| (&config.env, &config.headers));
        if old_secrets == new_secrets {
            continue;
        }
        let (env, headers) = new
            .map(|config| (config.env.clone(), config.headers.clone()))
            .unwrap_or_default();
        changes.push((id.to_owned(), env, headers));
    }

    let mut attempted = Vec::new();
    for (id, env, headers) in &changes {
        // Include the failing write in the rollback list: a credential backend
        // may report failure after partially changing its underlying record.
        attempted.push(id.clone());
        if let Err(error) = save_credential(id, env, headers) {
            let rollback = restore_credentials(&attempted, &previous_by_id, &mut save_credential);
            return Err(with_rollback_detail(
                format!("Could not save credentials for MCP server '{id}': {error}"),
                rollback,
            ));
        }
    }

    if let Err(error) = save_configs(path, next) {
        let rollback = restore_credentials(&attempted, &previous_by_id, &mut save_credential);
        return Err(with_rollback_detail(error, rollback));
    }
    Ok(())
}

fn restore_credentials<F>(
    ids: &[String],
    previous: &BTreeMap<&str, &McpServerConfig>,
    save_credential: &mut F,
) -> Vec<String>
where
    F: FnMut(&str, &BTreeMap<String, String>, &BTreeMap<String, String>) -> Result<(), String>,
{
    let mut errors = Vec::new();
    for id in ids.iter().rev() {
        let (env, headers) = previous
            .get(id.as_str())
            .map(|config| (&config.env, &config.headers))
            .unwrap_or((&EMPTY_SECRETS, &EMPTY_SECRETS));
        if let Err(error) = save_credential(id, env, headers) {
            errors.push(format!("{id}: {error}"));
        }
    }
    errors
}

static EMPTY_SECRETS: BTreeMap<String, String> = BTreeMap::new();

fn with_rollback_detail(mut error: String, rollback_errors: Vec<String>) -> String {
    if !rollback_errors.is_empty() {
        error.push_str("; credential rollback also failed for ");
        error.push_str(&rollback_errors.join(", "));
    }
    error
}

fn keyring_account(id: &str) -> String {
    format!("server-secrets-{id}")
}
fn legacy_keyring_account(id: &str) -> String {
    format!("server-env-{id}")
}

#[derive(Default, Serialize, Deserialize)]
struct McpSecrets {
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

fn save_credentials(
    id: &str,
    env: &BTreeMap<String, String>,
    headers: &BTreeMap<String, String>,
) -> Result<(), String> {
    credential_cache().lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &keyring_account(id)).map_err(|e| e.to_string())?;
    if env.is_empty() && headers.is_empty() {
        delete_keyring_credential(&entry)
            .map_err(|e| format!("Could not clear MCP credentials: {e}"))?;
        let legacy = keyring::Entry::new(KEYRING_SERVICE, &legacy_keyring_account(id))
            .map_err(|e| e.to_string())?;
        return delete_keyring_credential(&legacy)
            .map_err(|e| format!("Could not clear legacy MCP credentials: {e}"));
    }
    let raw = serde_json::to_string(&McpSecrets {
        env: env.clone(),
        headers: headers.clone(),
    })
    .map_err(|e| e.to_string())?;
    entry
        .set_password(&raw)
        .map_err(|e| format!("Could not save MCP credentials to the OS keyring: {e}"))?;
    let legacy = keyring::Entry::new(KEYRING_SERVICE, &legacy_keyring_account(id))
        .map_err(|e| e.to_string())?;
    delete_keyring_credential(&legacy)
        .map_err(|e| format!("Could not clear legacy MCP credentials: {e}"))
}

fn delete_keyring_credential(entry: &keyring::Entry) -> Result<(), keyring::Error> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error),
    }
}
type Credentials = (BTreeMap<String, String>, BTreeMap<String, String>);

/// Credentials already read this session. The OS credential store asks the user's permission on
/// every read (the macOS Keychain especially), and a server that is down is retried regularly.
fn credential_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Credentials>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Credentials>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn load_credentials(id: &str) -> Result<Credentials, String> {
    if let Some(found) = credential_cache().lock().unwrap_or_else(|p| p.into_inner()).get(id) {
        return Ok(found.clone());
    }
    let loaded = load_credentials_uncached(id)?;
    credential_cache().lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_owned(), loaded.clone());
    Ok(loaded)
}

fn load_credentials_uncached(
    id: &str,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>), String> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &keyring_account(id)).map_err(|e| e.to_string())?;
    let raw = match entry.get_password() {
        Ok(raw) => raw,
        Err(keyring::Error::NoEntry) => {
            let legacy = keyring::Entry::new(KEYRING_SERVICE, &legacy_keyring_account(id))
                .map_err(|e| e.to_string())?;
            match legacy.get_password() {
                Ok(raw) => raw,
                Err(keyring::Error::NoEntry) => return Ok((BTreeMap::new(), BTreeMap::new())),
                Err(error) => return Err(format!("Could not read saved MCP credentials: {error}")),
            }
        }
        Err(error) => return Err(format!("Could not read saved MCP credentials: {error}")),
    };
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("Saved MCP credentials are invalid: {error}"))?;
    if parsed.get("env").is_some() || parsed.get("headers").is_some() {
        let secrets: McpSecrets = serde_json::from_value(parsed)
            .map_err(|error| format!("Saved MCP credentials are invalid: {error}"))?;
        Ok((secrets.env, secrets.headers))
    } else {
        // Previous versions stored environment variables directly as a JSON map.
        let env: BTreeMap<String, String> = serde_json::from_value(parsed)
            .map_err(|error| format!("Saved MCP credentials are invalid: {error}"))?;
        Ok((env, BTreeMap::new()))
    }
}

fn read_frames(mut stdout: impl Read, tx: mpsc::SyncSender<Result<Value, String>>) {
    let mut reader = BufReader::new(&mut stdout);
    loop {
        let mut frame = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            match reader.read_exact(&mut byte) {
                Ok(()) if byte[0] == b'\n' => break,
                Ok(()) => {
                    frame.push(byte[0]);
                    if frame.len() > MAX_FRAME_BYTES {
                        let _ = tx.send(Err("MCP frame exceeded the 4 MiB limit".into()));
                        return;
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::UnexpectedEof && frame.is_empty() =>
                {
                    let _ = tx.send(Err("MCP server closed its output".into()));
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Err(format!("Could not read MCP server output: {error}")));
                    return;
                }
            }
        }
        match serde_json::from_slice::<Value>(&frame) {
            Ok(value) => {
                if tx.send(Ok(value)).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(format!("MCP server sent invalid JSON: {error}")));
                return;
            }
        }
    }
}

fn read_frames_with_health(
    stdout: impl Read,
    tx: mpsc::SyncSender<Result<Value, String>>,
    alive: Arc<AtomicBool>,
) {
    struct HealthGuard(Arc<AtomicBool>);
    impl Drop for HealthGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    let _health_guard = HealthGuard(alive);
    read_frames(stdout, tx);
}

fn bounded_json(value: &Value) -> String {
    let raw = serde_json::to_string(value).unwrap_or_else(|_| "unreadable error".into());
    raw.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Cursor};

    fn test_config(id: &str, token: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            command: "local-server".into(),
            args: Vec::new(),
            url: None,
            sse: false,
            env: if token.is_empty() {
                BTreeMap::new()
            } else {
                BTreeMap::from([("TOKEN".into(), token.into())])
            },
            headers: BTreeMap::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            enabled: true,
        }
    }

    fn test_path(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "jarvus-mcp-{label}-{}-{nonce}.json",
            std::process::id()
        ))
    }

    #[test]
    fn dead_stdio_reader_is_not_reported_connected_or_advertised() {
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::sync_channel(1);
        read_frames_with_health(Cursor::new(b""), tx, Arc::clone(&alive));
        assert!(rx.recv().unwrap().is_err());
        assert!(!alive.load(Ordering::Acquire));

        let mut manager = McpManager::new(test_path("health"));
        manager.configs.push(test_config("mock", ""));
        let mut connection = Connection::default();
        connection.process_alive = Some(alive);
        connection.tools = vec![McpTool {
            server_id: "mock".into(),
            name: "echo".into(),
            description: None,
            input_schema: json!({"type":"object"}),
        }];
        manager.connections.insert("mock".into(), connection);

        assert!(!manager.is_connected("mock"));
        assert!(manager.tools().is_empty());
        assert!(manager.tools_for("mock").is_empty());
        assert!(
            manager
                .last_error("mock")
                .unwrap_or_default()
                .contains("exited")
        );

        manager.refresh_health();
        assert!(!manager.connections.contains_key("mock"));
        assert!(manager.last_error("mock").is_some());
    }

    #[test]
    fn credential_failure_rolls_back_secret_and_keeps_old_config_file() {
        let path = test_path("credential-rollback");
        let old = vec![test_config("server", "old-token")];
        let next = vec![test_config("server", "new-token")];
        save_configs(&path, &old).unwrap();
        let old_file = fs::read(&path).unwrap();
        let mut stored = "old-token".to_owned();
        let mut first_write = true;

        let result = persist_configs_and_credentials(&path, &old, &next, |_, env, _| {
            let token = env.get("TOKEN").cloned().unwrap_or_default();
            if first_write {
                first_write = false;
                stored = token;
                Err("simulated keyring write failed after mutation".into())
            } else {
                stored = token;
                Ok(())
            }
        });

        assert!(
            result
                .unwrap_err()
                .contains("simulated keyring write failed")
        );
        assert_eq!(stored, "old-token");
        assert_eq!(fs::read(&path).unwrap(), old_file);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn config_write_failure_rolls_back_staged_secrets() {
        let dir_as_file = test_path("not-a-directory");
        fs::write(&dir_as_file, b"occupied").unwrap();
        let path = dir_as_file.join("servers.json");
        let old = vec![test_config("server", "old-token")];
        let next = vec![test_config("server", "new-token")];
        let mut stored = "old-token".to_owned();

        let result = persist_configs_and_credentials(&path, &old, &next, |_, env, _| {
            stored = env.get("TOKEN").cloned().unwrap_or_default();
            Ok(())
        });

        assert!(result.is_err());
        assert_eq!(stored, "old-token");
        let _ = fs::remove_file(dir_as_file);
    }

    #[test]
    fn replace_config_persistence_failure_preserves_config_and_live_connection() {
        let parent_as_file = test_path("replace-parent-file");
        fs::write(&parent_as_file, b"occupied").unwrap();
        let old = test_config("server", "");
        let mut next = old.clone();
        next.command = "replacement-server".into();
        let mut manager = McpManager::new(parent_as_file.join("servers.json"));
        manager.configs.push(old.clone());

        let mut connection = Connection::default();
        connection.tools.push(McpTool {
            server_id: "server".into(),
            name: "echo".into(),
            description: None,
            input_schema: json!({"type":"object"}),
        });
        manager.connections.insert("server".into(), connection);

        assert!(manager.replace_configs(vec![next]).is_err());
        assert_eq!(manager.configs, vec![old]);
        assert!(manager.is_connected("server"));
        assert_eq!(manager.tools_for("server").len(), 1);
        let _ = fs::remove_file(parent_as_file);
    }

    #[test]
    fn replacing_config_keeps_a_valid_committed_file() {
        let path = test_path("replace-config");
        save_configs(&path, &[test_config("server", "old")]).unwrap();
        save_configs(&path, &[test_config("server", "new")]).unwrap();
        let raw: Vec<McpServerConfig> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].id, "server");
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_the_mcp_servers_document_apps_hand_out() {
        let document = r#"{
  "mcpServers": {
    "Roblox_Studio": {
      "command": "cmd.exe",
      "args": [
        "/c",
        "cd /d %LOCALAPPDATA%\\Roblox && .\\mcp.bat"
      ]
    }
  }
}"#;
        let configs = parse_server_document(document).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id, "Roblox_Studio");
        assert_eq!(configs[0].command, "cmd.exe");
        assert_eq!(
            configs[0].args,
            vec![
                "/c".to_owned(),
                r"cd /d %LOCALAPPDATA%\Roblox && .\mcp.bat".to_owned()
            ]
        );
        assert!(configs[0].enabled);
        assert_eq!(configs[0].timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn parses_other_common_shapes_and_rejects_unsupported_ones() {
        let vscode = parse_server_document(r#"{"servers":{"files":{"command":"npx","args":["-y","@x/fs"],"env":{"TOKEN":"t"},"disabled":true}}}"#).unwrap();
        assert_eq!(vscode[0].id, "files");
        assert_eq!(vscode[0].env.get("TOKEN").map(String::as_str), Some("t"));
        assert!(!vscode[0].enabled);
        let bare =
            parse_server_document(r#"{"my server":{"command":"node","args":["s.js"]}}"#).unwrap();
        assert_eq!(bare[0].id, "my_server", "ids are sanitized");
        let fenced =
            parse_server_document("```json\n{\"mcpServers\":{\"a\":{\"command\":\"x\"}}}\n```")
                .unwrap();
        assert_eq!(fenced[0].id, "a");
        let remote = parse_server_document(r#"{"mcpServers":{"remote":{"type":"streamable-http","url":"https://example.com/mcp","headers":{"Authorization":"Bearer token"}}}}"#).unwrap();
        assert_eq!(remote[0].url.as_deref(), Some("https://example.com/mcp"));
        assert!(!remote[0].sse);
        assert_eq!(
            remote[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer token")
        );
        let sse = parse_server_document(
            r#"{"mcpServers":{"events":{"type":"sse","url":"https://example.com/sse"}}}"#,
        )
        .unwrap();
        assert!(sse[0].sse);
        assert!(parse_server_document("not json").is_err());
        assert!(parse_server_document(r#"{"mcpServers":{}}"#).is_err());
    }

    #[test]
    fn reads_one_newline_delimited_json_message() {
        let (tx, rx) = mpsc::sync_channel(MAX_QUEUED_FRAMES);
        read_frames(
            Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n"),
            tx,
        );
        assert_eq!(rx.recv().unwrap().unwrap()["id"], 1);
    }

    #[test]
    fn validates_remote_transport_urls_and_sse_origin() {
        let mut cfg = McpServerConfig {
            id: "remote".into(),
            command: String::new(),
            args: vec![],
            url: Some("https://example.com/mcp".into()),
            sse: false,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout_secs: 5,
            enabled: true,
        };
        assert!(validate_config(&cfg).is_ok());
        cfg.url = Some("http://127.0.0.1:8080/mcp".into());
        assert!(validate_config(&cfg).is_ok());
        cfg.url = Some("http://example.com/mcp".into());
        assert!(validate_config(&cfg).is_err());
        cfg.url = Some("https://user:pass@example.com/mcp".into());
        assert!(validate_config(&cfg).is_err());
        let base = reqwest::Url::parse("https://example.com/sse").unwrap();
        assert_eq!(
            resolve_sse_endpoint(&base, "/messages").unwrap().as_str(),
            "https://example.com/messages"
        );
        assert!(resolve_sse_endpoint(&base, "https://attacker.example/collect").is_err());
    }

    #[test]
    fn streamable_http_initializes_lists_and_calls_a_tool() {
        use std::net::{TcpListener, TcpStream};

        fn receive(stream: TcpStream) -> (TcpStream, String, Value) {
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            let json = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            };
            (reader.into_inner(), headers, json)
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for _ in 0..4 {
                let (stream, _) = listener.accept().unwrap();
                let (mut stream, headers, request) = receive(stream);
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                let response = match method {
                    "initialize" => {
                        json!({"jsonrpc":"2.0", "id":request["id"], "result":{"protocolVersion":MCP_HTTP_PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"}}})
                    }
                    "tools/list" => {
                        json!({"jsonrpc":"2.0", "id":request["id"], "result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}})
                    }
                    "tools/call" => {
                        json!({"jsonrpc":"2.0", "id":request["id"], "result":{"content":[{"type":"text","text":"hello"}]}})
                    }
                    _ => Value::Null,
                };
                if method != "initialize" && method != "notifications/initialized" {
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains("mcp-protocol-version: 2025-11-25")
                    );
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains("mcp-session-id: mock-session")
                    );
                }
                if method == "notifications/initialized" {
                    stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    let bytes = serde_json::to_vec(&response).unwrap();
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Protocol-Version: {MCP_HTTP_PROTOCOL}\r\nMcp-Session-Id: mock-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    stream.write_all(header.as_bytes()).unwrap();
                    stream.write_all(&bytes).unwrap();
                }
            }
        });
        let cfg = McpServerConfig {
            id: "http-mock".into(),
            command: String::new(),
            args: vec![],
            url: Some(url),
            sse: false,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout_secs: 5,
            enabled: true,
        };
        let mut connection = Connection::default();
        connection.start(&cfg).unwrap();
        assert_eq!(
            connection.call("echo", json!({})).unwrap().content[0]["text"],
            "hello"
        );
        connection.stop();
        server.join().unwrap();
    }

    #[test]
    fn legacy_sse_initializes_and_uses_the_advertised_message_endpoint() {
        use std::net::{TcpListener, TcpStream};

        fn receive(stream: TcpStream) -> (TcpStream, Value) {
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            (
                reader.into_inner(),
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            )
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/sse", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (mut stream, request) = receive(stream);
            assert!(
                request.is_null(),
                "first request opens the SSE event stream"
            );
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\nevent: endpoint\ndata: /messages?sessionId=test\n\n").unwrap();
            stream.flush().unwrap();
            let shared_stream = std::sync::Arc::new(std::sync::Mutex::new(stream));
            for _ in 0..4 {
                let (mut post, request) = receive(listener.accept().unwrap().0);
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                post.write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
                if method != "notifications/initialized" {
                    let result = match method {
                        "initialize" => {
                            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"sse-mock","version":"1"}})
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
                        }
                        "tools/call" => json!({"content":[{"type":"text","text":"sse-hello"}]}),
                        _ => Value::Null,
                    };
                    let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
                    let event = format!(
                        "event: message\ndata: {}\n\n",
                        serde_json::to_string(&response).unwrap()
                    );
                    let mut stream = shared_stream.lock().unwrap();
                    stream.write_all(event.as_bytes()).unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        let cfg = McpServerConfig {
            id: "sse-mock".into(),
            command: String::new(),
            args: vec![],
            url: Some(url),
            sse: true,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout_secs: 5,
            enabled: true,
        };
        let mut connection = Connection::default();
        connection.start(&cfg).unwrap();
        assert_eq!(
            connection.call("echo", json!({})).unwrap().content[0]["text"],
            "sse-hello"
        );
        connection.stop();
        server.join().unwrap();
    }

    #[test]
    fn config_serialization_does_not_write_environment_secrets() {
        let cfg = McpServerConfig {
            id: "safe".into(),
            command: "local-server".into(),
            args: vec![],
            url: None,
            sse: false,
            env: BTreeMap::from([("TOKEN".into(), "secret".into())]),
            headers: BTreeMap::from([("Authorization".into(), "secret-header".into())]),
            timeout_secs: 30,
            enabled: true,
        };
        let serialized = serde_json::to_string(&cfg).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("TOKEN"));
        assert!(!serialized.contains("secret-header"));
    }

    #[test]
    fn config_rejects_oversized_environment_values() {
        let mut cfg = McpServerConfig {
            id: "bounded".into(),
            command: "local-server".into(),
            args: vec![],
            url: None,
            sse: false,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout_secs: 30,
            enabled: true,
        };
        assert!(validate_config(&cfg).is_ok());
        cfg.env
            .insert("TOKEN".into(), "x".repeat(MAX_ENV_VALUE_BYTES + 1));
        assert!(validate_config(&cfg).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn local_child_runs_real_stdio_lifecycle_and_tool_call() {
        // Launch a fully specified local mock over the same stdio pipes as a configured server.
        let script = r#"while ($line = [Console]::In.ReadLine()) { $m = ConvertFrom-Json $line; $result = switch ($m.method) { 'server/discover' { @{ supportedVersions=@('2026-07-28'); capabilities=@{ tools=@{} } } } 'tools/list' { @{ tools=@(@{ name='echo'; inputSchema=@{ type='object' } }) } } 'tools/call' { @{ content=@(@{ type='text'; text='hello' }) } } default { @{} } }; if ($null -ne $m.id) { [Console]::Out.WriteLine((ConvertTo-Json -InputObject @{ jsonrpc='2.0'; id=$m.id; result=$result } -Compress -Depth 20)); [Console]::Out.Flush() } }"#;
        let cfg = McpServerConfig {
            id: "mock".into(),
            command: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            url: None,
            sse: false,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout_secs: 5,
            enabled: true,
        };
        let mut connection = Connection::default();
        connection.start(&cfg).unwrap();
        let result = connection.call("echo", json!({"text":"hello"})).unwrap();
        assert_eq!(result.content[0]["text"], "hello");
    }

    #[cfg(windows)]
    #[test]
    fn legacy_server_that_exits_on_discovery_is_restarted_for_initialize() {
        let marker = std::env::temp_dir().join(format!(
            "jarvus-mcp-discovery-{}-{}.marker",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let script = r#"
$firstProcess = -not (Test-Path $env:JARVUS_MCP_DISCOVERY_MARKER)
if ($firstProcess) { Set-Content -NoNewline -Path $env:JARVUS_MCP_DISCOVERY_MARKER -Value 'probed' }
while ($line = [Console]::In.ReadLine()) {
  $message = ConvertFrom-Json $line
  if ($firstProcess -and $message.method -eq 'server/discover') { break }
  $result = switch ($message.method) {
    'initialize' { @{ protocolVersion='2025-11-25'; capabilities=@{ tools=@{} }; serverInfo=@{ name='legacy'; version='1' } } }
    'tools/list' { @{ tools=@(@{ name='echo'; inputSchema=@{ type='object' } }) } }
    'tools/call' { @{ content=@(@{ type='text'; text='restarted' }) } }
    default { @{} }
  }
  if ($null -ne $message.id) {
    [Console]::Out.WriteLine((ConvertTo-Json -InputObject @{ jsonrpc='2.0'; id=$message.id; result=$result } -Compress -Depth 20))
    [Console]::Out.Flush()
  }
}
"#;
        let cfg = McpServerConfig {
            id: "legacy-restart".into(),
            command: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            url: None,
            sse: false,
            env: BTreeMap::from([(
                "JARVUS_MCP_DISCOVERY_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            )]),
            headers: BTreeMap::new(),
            timeout_secs: 5,
            enabled: true,
        };
        let mut connection = Connection::default();
        let started = connection.start(&cfg);
        let result = started
            .and_then(|()| connection.call("echo", json!({})))
            .map(|result| result.content[0]["text"].as_str().unwrap_or("").to_owned());
        connection.stop();
        let _ = fs::remove_file(marker);
        assert_eq!(result.unwrap(), "restarted");
    }

    #[cfg(windows)]
    #[test]
    fn manager_removes_stdio_server_that_exits_after_tool_discovery() {
        let script = r#"
while ($line = [Console]::In.ReadLine()) {
  $message = ConvertFrom-Json $line
  $result = switch ($message.method) {
    'server/discover' { @{ supportedVersions=@('2026-07-28'); capabilities=@{ tools=@{} } } }
    'tools/list' { @{ tools=@(@{ name='echo'; inputSchema=@{ type='object' } }) } }
    default { @{} }
  }
  if ($null -ne $message.id) {
    [Console]::Out.WriteLine((ConvertTo-Json -InputObject @{ jsonrpc='2.0'; id=$message.id; result=$result } -Compress -Depth 20))
    [Console]::Out.Flush()
  }
  if ($message.method -eq 'tools/list') { break }
}
"#;
        let cfg = McpServerConfig {
            id: "exit-after-discovery".into(),
            command: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            url: None,
            sse: false,
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            timeout_secs: 5,
            enabled: true,
        };
        let id = cfg.id.clone();
        let mut manager = McpManager::new(test_path("child-exit"));
        manager.configs.push(cfg);
        manager.connect(&id).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while manager.is_connected(&id) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!manager.is_connected(&id));
        assert!(manager.tools_for(&id).is_empty());
        manager.refresh_health();
        assert!(manager.last_error(&id).is_some());
    }
}
