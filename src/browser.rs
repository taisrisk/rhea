//! Chrome Native Messaging host and authenticated local command relay.
//!
//! Chrome starts this process with the calling extension origin as argv[1]. The
//! extension keeps the native port open. Rhea sends commands to the host's
//! loopback relay (see `chrome-extension/README.md`); the host forwards those
//! commands over the native port and returns the extension's verified result.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Smaller than Chrome's 1 MiB host-to-extension limit; also caps input JSON.
pub const MAX_NATIVE_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_HTTP_HEADER_BYTES: usize = 8 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;
const MAX_HTTP_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const RELAY_REPLY_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_RELAY_CONNECTIONS: usize = 16;
const HOST_NAME: &str = "com.rhea.browser";
/// What earlier versions called the native host. The extension Chrome already has loaded still
/// asks for this name until it is reloaded from the new folder, so it is registered too.
const LEGACY_HOST_NAME: &str = "com.jarvus.browser";
/// ID of the bundled extension, pinned by the public "key" in
/// chrome-extension/manifest.json (so an unpacked install always gets it).
pub const BUNDLED_EXTENSION_ID: &str = "eplppddfejfcbdllfbooenaplkobcpkk";
/// Tab-level and page-level commands the bundled extension implements.
const BROWSER_METHODS: [&str; 18] = [
    "ping", "tabs", "activate", "open", "close", "navigate", "back", "forward", "reload",
    "snapshot", "click", "type", "select", "press", "scroll", "extract", "wait", "hover",
];

type PendingMap = Arc<Mutex<HashMap<String, PendingCommand>>>;
type NativeWriter = Arc<Mutex<BufWriter<io::Stdout>>>;

struct PendingCommand {
    reply: mpsc::SyncSender<Result<Value, String>>,
}

#[derive(Debug)]
pub struct BrowserHostError(pub String);

impl std::fmt::Display for BrowserHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BrowserHostError {}

impl From<io::Error> for BrowserHostError {
    fn from(value: io::Error) -> Self {
        Self(value.to_string())
    }
}

/// Persisted configuration published by the Chrome-started native host. Treat
/// the token as a local capability: never expose it to page content or logs.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRelayConfig {
    pub version: u32,
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
    pub expires_at_epoch_ms: u128,
}

/// Path shared by the Chrome-started native host and the Rhea application.
pub fn relay_config_path() -> Result<PathBuf, BrowserHostError> {
    crate::chat_store::app_data_root()
        .map(|root| root.join("browser-relay.json"))
        .ok_or_else(|| {
            BrowserHostError(
                "The user home directory is required for the authenticated browser relay".into(),
            )
        })
}

/// Load and validate the short-lived descriptor written by the live Chrome
/// native host. This does not itself prove that the extension is connected;
/// `execute_browser_command` will report a connection error if it is not.
pub fn load_relay_config() -> Result<BrowserRelayConfig, BrowserHostError> {
    let path = relay_config_path()?;
    let bytes = fs::read(&path)
        .map_err(|error| BrowserHostError(format!("browser relay is unavailable: {error}")))?;
    if bytes.len() > 4096 {
        return Err(BrowserHostError(
            "browser relay descriptor exceeds 4 KiB".into(),
        ));
    }
    let config: BrowserRelayConfig = serde_json::from_slice(&bytes)
        .map_err(|_| BrowserHostError("browser relay descriptor is invalid JSON".into()))?;
    validate_relay_config(&config)?;
    Ok(config)
}

/// Ask the active Chrome extension to run one allowlisted browser command.
/// The descriptor is only a discovery hint; the 256-bit bearer token
/// authenticates each loopback request and the native host independently
/// validates the extension's claimed origin and permission before returning.
pub fn execute_browser_command(method: &str, params: Value) -> Result<Value, BrowserHostError> {
    execute_browser_command_with_timeout(
        method,
        params,
        RELAY_REPLY_TIMEOUT + Duration::from_secs(2),
    )
}

/// True when Chrome is running the bridge and it answers within ~1.5 s.
pub fn extension_alive() -> bool {
    execute_browser_command_with_timeout("ping", json!({}), Duration::from_millis(1500)).is_ok()
}

fn execute_browser_command_with_timeout(
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, BrowserHostError> {
    if !allowed_browser_method(method) {
        return Err(BrowserHostError("browser method is not allowed".into()));
    }
    if !params.is_object() {
        return Err(BrowserHostError(
            "browser command params must be an object".into(),
        ));
    }
    let body = serde_json::to_vec(&json!({"method":method,"params":params}))
        .map_err(|e| BrowserHostError(e.to_string()))?;
    if body.len() > MAX_HTTP_BODY_BYTES {
        return Err(BrowserHostError(
            "browser command exceeds the 64 KiB limit".into(),
        ));
    }
    let config = load_relay_config()?;
    let endpoint = url::Url::parse(&config.endpoint)
        .map_err(|_| BrowserHostError("browser relay endpoint is invalid".into()))?;
    let port = endpoint
        .port()
        .ok_or_else(|| BrowserHostError("browser relay endpoint has no port".into()))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| {
        BrowserHostError(format!(
            "could not connect to Chrome browser relay: {error}"
        ))
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST /v1/command HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        config.token,
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream
        .take((MAX_HTTP_HEADER_BYTES + MAX_HTTP_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)?;
    if response.len() > MAX_HTTP_HEADER_BYTES + MAX_HTTP_RESPONSE_BYTES {
        return Err(BrowserHostError(
            "browser relay response exceeds size limit".into(),
        ));
    }
    let (header_end, body_start) = find_bytes(&response, b"\r\n\r\n")
        .map(|position| (position, position + 4))
        .ok_or_else(|| BrowserHostError("browser relay returned malformed HTTP".into()))?;
    let headers = std::str::from_utf8(&response[..header_end])
        .map_err(|_| BrowserHostError("browser relay returned invalid HTTP headers".into()))?;
    let status_line = headers.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let value: Value = serde_json::from_slice(&response[body_start..])
        .map_err(|_| BrowserHostError("browser relay returned invalid JSON".into()))?;
    if status != 200 || value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(BrowserHostError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("browser command failed")
                .to_string(),
        ));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// Ask the currently connected extension to close its Native Messaging port.
/// Starting still requires the visible popup action because only Chrome can
/// create the extension-owned host process.
pub fn stop_browser_relay() -> Result<(), BrowserHostError> {
    let config = load_relay_config()?;
    let endpoint = url::Url::parse(&config.endpoint)
        .map_err(|_| BrowserHostError("browser relay endpoint is invalid".into()))?;
    let port = endpoint
        .port()
        .ok_or_else(|| BrowserHostError("browser relay endpoint has no port".into()))?;
    let body = b"{}";
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| {
        BrowserHostError(format!(
            "could not connect to Chrome browser relay: {error}"
        ))
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST /v1/stop HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        config.token,
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream
        .take((MAX_HTTP_HEADER_BYTES + 4096) as u64)
        .read_to_end(&mut response)?;
    let (header_end, body_start) = find_bytes(&response, b"\r\n\r\n")
        .map(|position| (position, position + 4))
        .ok_or_else(|| BrowserHostError("browser relay returned malformed HTTP".into()))?;
    let headers = std::str::from_utf8(&response[..header_end])
        .map_err(|_| BrowserHostError("browser relay returned invalid HTTP headers".into()))?;
    let status = headers
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let value: Value = serde_json::from_slice(&response[body_start..])
        .map_err(|_| BrowserHostError("browser relay returned invalid JSON".into()))?;
    if status != 200 || value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(BrowserHostError(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("could not stop browser relay")
                .to_string(),
        ));
    }
    Ok(())
}

/// Run when `rhea.exe` is invoked as Chrome's native messaging host.
///
/// `RHEA_CHROME_EXTENSION_ID` must contain the exact 32-character unpacked or
/// published Chrome extension ID registered in Chrome's native-host manifest.
pub fn run_native_messaging_host() -> Result<(), BrowserHostError> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let caller_origin = if args.first().map(String::as_str) == Some("--native-messaging-host") {
        args.get(1)
    } else {
        args.first()
    }
    .ok_or_else(|| BrowserHostError("Chrome extension origin is missing from argv[1]".into()))?
    .to_string();
    let expected_id = env::var("RHEA_CHROME_EXTENSION_ID")
        .ok()
        .filter(|id| valid_extension_id(id))
        .unwrap_or_else(|| BUNDLED_EXTENSION_ID.to_owned());
    let extension_id = validate_extension_origin(&caller_origin, &expected_id)?;
    let canonical_origin = format!("chrome-extension://{extension_id}");

    let token = secure_token()?;
    let (session_path, descriptor) = publish_relay_session()?;
    let _session_file = SessionFileGuard(session_path);

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let writer: NativeWriter = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    start_relay_listener(
        &token,
        descriptor,
        canonical_origin.clone(),
        pending.clone(),
        writer.clone(),
    )?;

    // Native messaging is binary framed even on Windows. std::io uses raw OS
    // handles here, so stdout must remain reserved for frames (diagnostics use
    // stderr only).
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        let message = match read_native_frame(&mut reader)? {
            Some(message) => message,
            None => break,
        };
        if let Some(response) = handle_extension_message(message, &canonical_origin, &pending) {
            send_native_frame(&writer, &response)?;
        }
    }

    // Any relay requests still waiting on a result are failed promptly.
    if let Ok(mut waiting) = pending.lock() {
        for (_, command) in waiting.drain() {
            let _ = command
                .reply
                .send(Err("Chrome extension disconnected".into()));
        }
    }
    Ok(())
}

/// Read a single Chrome Native Messaging frame, returning `None` only on a
/// clean EOF before the first length byte. Length is native-endian per Chrome.
pub fn read_native_frame<R: Read>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut length = [0u8; 4];
    let first = reader.read(&mut length[..1])?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length[1..])?;
    let size = u32::from_ne_bytes(length) as usize;
    if size == 0 || size > MAX_NATIVE_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("native message length {size} is outside allowed bounds"),
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid JSON frame: {e}"),
        )
    })
}

/// Serialize and write one Chrome Native Messaging frame.
pub fn write_native_frame<W: Write>(writer: &mut W, message: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(message)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if bytes.is_empty() || bytes.len() > MAX_NATIVE_MESSAGE_BYTES || bytes.len() > u32::MAX as usize
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native message exceeds the configured size limit",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_ne_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

/// Confirm both the Chrome Native Messaging caller shape and the exact
/// extension ID configured by the installer. The host manifest's exact
/// `allowed_origins` entry is a second, independent gate.
pub fn validate_extension_origin(
    origin: &str,
    expected_id: &str,
) -> Result<String, BrowserHostError> {
    if !valid_extension_id(expected_id) {
        return Err(BrowserHostError(
            "configured Chrome extension ID must be 32 lowercase a-p characters".into(),
        ));
    }
    let stripped = origin.strip_suffix('/').unwrap_or(origin);
    let Some(id) = stripped.strip_prefix("chrome-extension://") else {
        return Err(BrowserHostError(
            "caller is not a Chrome extension origin".into(),
        ));
    };
    if !valid_extension_id(id) || id != expected_id {
        return Err(BrowserHostError(
            "caller extension origin is not allowlisted".into(),
        ));
    }
    Ok(id.to_string())
}

fn valid_extension_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

fn handle_extension_message(
    message: Value,
    extension_origin: &str,
    pending: &PendingMap,
) -> Option<Value> {
    let id = message.get("id").and_then(Value::as_str).unwrap_or("");
    if id.len() > 128 {
        return Some(json!({"type":"error", "error":"message id is too long"}));
    }
    match message.get("type").and_then(Value::as_str) {
        Some("ready") => Some(json!({
            "type":"ready_ack",
            "protocol":1,
            "host":HOST_NAME,
            "extensionOrigin":extension_origin
        })),
        Some("heartbeat") => Some(json!({"type":"heartbeat_ack"})),
        Some("command_result") => {
            if id.is_empty() {
                return Some(json!({"type":"error", "error":"command_result needs an id"}));
            }
            let is_ok = message.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let result = message.get("result").cloned().unwrap_or(Value::Null);
            let error = message
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("extension action failed")
                .to_string();
            let command = pending.lock().ok().and_then(|mut map| map.remove(id));
            let Some(command) = command else {
                return Some(
                    json!({"type":"error", "id":id, "error":"unknown or expired command id"}),
                );
            };
            let response = if is_ok { Ok(result) } else { Err(error) };
            let _ = command.reply.send(response);
            None
        }
        Some("extension_error") => Some(json!({
            "type":"error",
            "id":id,
            "error":message.get("error").and_then(Value::as_str).unwrap_or("extension error")
        })),
        _ => Some(json!({"type":"error", "id":id, "error":"unsupported extension message"})),
    }
}

fn send_native_frame(writer: &NativeWriter, message: &Value) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("native messaging writer lock poisoned"))?;
    write_native_frame(&mut *writer, message)
}

fn start_relay_listener(
    token: &str,
    session: RelaySession,
    extension_origin: String,
    pending: PendingMap,
    writer: NativeWriter,
) -> Result<(), BrowserHostError> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    let endpoint = format!(
        "http://127.0.0.1:{}/v1/command",
        listener.local_addr()?.port()
    );
    session.write_descriptor(&endpoint, token)?;
    eprintln!(
        "Rhea browser relay listening on loopback (session file: {})",
        session.path.display()
    );

    let expected_token = token.to_string();
    let active = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("rhea-browser-relay".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let current = active.fetch_add(1, Ordering::AcqRel);
                        if current >= MAX_RELAY_CONNECTIONS {
                            active.fetch_sub(1, Ordering::AcqRel);
                            let mut stream = stream;
                            let _ = write_http_response(
                                &mut stream,
                                429,
                                &json!({"ok":false,"error":"too many relay connections"}),
                            );
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        let expected_token = expected_token.clone();
                        let extension_origin = extension_origin.clone();
                        let pending = pending.clone();
                        let writer = writer.clone();
                        let active_worker = active.clone();
                        let worker = thread::Builder::new()
                            .name("rhea-browser-relay-request".into())
                            .spawn(move || {
                                let mut stream = stream;
                                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                                let response = process_relay_request(
                                    &mut stream,
                                    &expected_token,
                                    &extension_origin,
                                    &pending,
                                    &writer,
                                );
                                let (status, body) = match response {
                                    Ok(body) => (200, body),
                                    Err((status, reason)) => {
                                        (status, json!({"ok":false,"error":reason}))
                                    }
                                };
                                let _ = write_http_response(&mut stream, status, &body);
                                let _ = stream.shutdown(Shutdown::Both);
                                active_worker.fetch_sub(1, Ordering::AcqRel);
                            });
                        if worker.is_err() {
                            active.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    Err(error) => eprintln!("browser relay accept failed: {error}"),
                }
            }
        })?;
    Ok(())
}

fn process_relay_request(
    stream: &mut TcpStream,
    expected_token: &str,
    _extension_origin: &str,
    pending: &PendingMap,
    writer: &NativeWriter,
) -> Result<Value, (u16, String)> {
    if !stream
        .peer_addr()
        .map_err(|e| (400, e.to_string()))?
        .ip()
        .is_loopback()
    {
        return Err((403, "loopback clients only".into()));
    }
    let (headers, body) = read_http_request(stream).map_err(|e| (400, e))?;
    let request_line = headers
        .get("request-line")
        .map(String::as_str)
        .unwrap_or("");
    if !matches!(
        request_line,
        "POST /v1/command HTTP/1.1" | "POST /v1/stop HTTP/1.1"
    ) {
        return Err((404, "use POST /v1/command or POST /v1/stop".into()));
    }
    let authorization = headers
        .get("authorization")
        .map(String::as_str)
        .unwrap_or("");
    let supplied_token = authorization.strip_prefix("Bearer ").unwrap_or("");
    if !constant_time_eq(supplied_token.as_bytes(), expected_token.as_bytes()) {
        return Err((401, "invalid relay session token".into()));
    }
    if headers.get("transfer-encoding").is_some() {
        return Err((400, "chunked requests are not supported".into()));
    }
    let request: Value =
        serde_json::from_slice(&body).map_err(|_| (400, "request body must be JSON".into()))?;
    if request_line == "POST /v1/stop HTTP/1.1" {
        send_native_frame(writer, &json!({"type":"shutdown"}))
            .map_err(|_| (503, "Chrome extension native port is unavailable".into()))?;
        return Ok(json!({"ok":true,"stopping":true}));
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    if !allowed_browser_method(method) {
        return Err((400, "method is not allowed".into()));
    }
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return Err((400, "params must be an object".into()));
    }
    validate_command_params(method, &params).map_err(|reason| (400, reason))?;

    let id = next_command_id();
    let (tx, rx) = mpsc::sync_channel(1);
    {
        let mut map = pending
            .lock()
            .map_err(|_| (500, "command queue unavailable".into()))?;
        if map.len() >= 32 {
            return Err((429, "too many pending browser commands".into()));
        }
        map.insert(id.clone(), PendingCommand { reply: tx });
    }
    let command = json!({"type":"command","id":id,"method":method,"params":params});
    if send_native_frame(writer, &command).is_err() {
        if let Ok(mut map) = pending.lock() {
            map.remove(&id);
        }
        return Err((503, "Chrome extension native port is unavailable".into()));
    }
    match rx.recv_timeout(RELAY_REPLY_TIMEOUT) {
        Ok(Ok(result)) => Ok(json!({"ok":true,"id":id,"result":result})),
        Ok(Err(reason)) => Err((409, reason)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Ok(mut map) = pending.lock() {
                map.remove(&id);
            }
            Err((
                504,
                "Chrome extension did not finish the command in time".into(),
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err((503, "command result channel closed".into()))
        }
    }
}

fn allowed_browser_method(method: &str) -> bool {
    BROWSER_METHODS.contains(&method)
}

fn validate_command_params(method: &str, params: &Value) -> Result<(), String> {
    for key in [
        "target",
        "within",
        "selector",
        "controlId",
        "tab",
        "keys",
        "option",
    ] {
        if params
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| value.len() > 2048)
        {
            return Err(format!("{key} is too long"));
        }
    }
    if let Some(text) = params.get("text").and_then(Value::as_str) {
        if text.len() > 20_000 {
            return Err("text exceeds the 20,000-character limit".into());
        }
    }
    if matches!(method, "open" | "navigate") {
        let destination = params
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{method} needs a url"))?;
        if destination.trim().is_empty() || destination.len() > 4096 {
            return Err("url is missing or longer than 4096 characters".into());
        }
        let lower = destination.trim().to_ascii_lowercase();
        if [
            "javascript:",
            "data:",
            "file:",
            "chrome:",
            "about:",
            "vbscript:",
        ]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
        {
            return Err("only web pages (http/https) can be opened".into());
        }
    }
    Ok(())
}

/// Register the bundled extension's native messaging host for this Windows
/// user: write the host manifest next to the app data and point Chrome's
/// per-user registry key at it. Idempotent; runs at every Rhea start so the
/// path follows the current executable.
pub fn ensure_native_host_registered() -> Result<String, BrowserHostError> {
    let _ = register_host(LEGACY_HOST_NAME);
    register_host(HOST_NAME)
}

fn register_host(host_name: &str) -> Result<String, BrowserHostError> {
    let exe = env::current_exe()?;
    let directory = crate::chat_store::app_data_root()
        .ok_or_else(|| BrowserHostError("The user home directory is not set".into()))?;
    fs::create_dir_all(&directory)?;
    let manifest_path = directory.join(format!("{host_name}.json"));
    let mut origins = vec![format!("chrome-extension://{BUNDLED_EXTENSION_ID}/")];
    if let Some(custom) = env::var("RHEA_CHROME_EXTENSION_ID")
        .ok()
        .filter(|id| valid_extension_id(id) && id != BUNDLED_EXTENSION_ID)
    {
        origins.push(format!("chrome-extension://{custom}/"));
    }
    let manifest = json!({
        "name": host_name,
        "description": "Rhea local browser-control bridge",
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": origins,
    });
    let bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|e| BrowserHostError(e.to_string()))?;
    write_if_changed(&manifest_path, &bytes)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let key = format!(r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{host_name}");
        let target = manifest_path.to_string_lossy().to_string();
        let current = std::process::Command::new("reg")
            .args(["query", &key, "/ve"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if !String::from_utf8_lossy(&current.stdout).contains(&target) {
            let status = std::process::Command::new("reg")
                .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", &target, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .status()?;
            if !status.success() {
                return Err(BrowserHostError(
                    "Could not register the Chrome native host for this user".into(),
                ));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = directory.parent().ok_or_else(|| {
            BrowserHostError("Rhea data directory has no home directory".into())
        })?;
        let chrome_hosts = home
            .join("Library")
            .join("Application Support")
            .join("Google")
            .join("Chrome")
            .join("NativeMessagingHosts");
        fs::create_dir_all(&chrome_hosts)?;
        let chrome_manifest = chrome_hosts.join(format!("{host_name}.json"));
        write_if_changed(&chrome_manifest, &bytes)?;
    }
    Ok(manifest_path.display().to_string())
}

fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> Result<(), BrowserHostError> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    let temporary = path.with_extension("json.tmp");
    write_private_file(&temporary, bytes)?;
    if let Err(error) = replace_path(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(BrowserHostError(error.to_string()));
    }
    Ok(())
}

#[cfg(windows)]
fn replace_path(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    if to.exists() {
        fs::remove_file(to)?;
    }
    fs::rename(from, to)
}

#[cfg(not(windows))]
fn replace_path(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    fs::rename(from, to)
}

fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), BrowserHostError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_http_request(stream: &mut TcpStream) -> Result<(HashMap<String, String>, Vec<u8>), String> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
        if bytes.len() >= MAX_HTTP_HEADER_BYTES {
            return Err("HTTP headers exceed the size limit".into());
        }
        let count = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("request ended before headers completed".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "HTTP headers are not UTF-8".to_string())?;
    let mut headers = HashMap::new();
    for (index, line) in header_text.split("\r\n").enumerate() {
        if line.is_empty() {
            continue;
        }
        if index == 0 {
            headers.insert("request-line".into(), line.to_string());
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| "malformed HTTP header".to_string())?;
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() || headers.contains_key(&key) {
            return Err("duplicate or empty HTTP header".into());
        }
        headers.insert(key, value.trim().to_string());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| "Content-Length is required".to_string())?
        .parse::<usize>()
        .map_err(|_| "invalid Content-Length".to_string())?;
    if length > MAX_HTTP_BODY_BYTES {
        return Err("request body exceeds the size limit".into());
    }
    let mut body = bytes[header_end..].to_vec();
    if body.len() > length {
        return Err("extra bytes after request body are not accepted".into());
    }
    while body.len() < length {
        let remaining = length - body.len();
        let read_len = remaining.min(chunk.len());
        let count = stream
            .read(&mut chunk[..read_len])
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("request body ended early".into());
        }
        body.extend_from_slice(&chunk[..count]);
    }
    Ok((headers, body))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_http_response(stream: &mut TcpStream, status: u16, body: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    if bytes.len() > MAX_HTTP_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "browser relay response exceeds size limit",
        ));
    }
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        bytes.len()
    )?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        difference |= (left.get(index).copied().unwrap_or(0)
            ^ right.get(index).copied().unwrap_or(0)) as usize;
    }
    difference == 0
}

fn next_command_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "rhea-{}-{}-{}",
        std::process::id(),
        time,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

struct RelaySession {
    path: PathBuf,
}

impl RelaySession {
    fn write_descriptor(&self, endpoint: &str, token: &str) -> Result<(), BrowserHostError> {
        let descriptor = json!({
            "version":1,
            "endpoint":endpoint,
            "token":token,
            "pid":std::process::id(),
            "expiresAtEpochMs":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() + 24 * 60 * 60 * 1000
        });
        let temporary = self.path.with_extension("json.tmp");
        write_private_file(
            &temporary,
            &serde_json::to_vec(&descriptor).map_err(|e| BrowserHostError(e.to_string()))?,
        )?;
        replace_path(&temporary, &self.path)?;
        Ok(())
    }
}

struct SessionFileGuard(PathBuf);
impl Drop for SessionFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn publish_relay_session() -> Result<(PathBuf, RelaySession), BrowserHostError> {
    let directory = crate::chat_store::app_data_root().ok_or_else(|| {
        BrowserHostError(
            "The user home directory is required for the authenticated browser relay".into(),
        )
    })?;
    fs::create_dir_all(&directory)?;
    let path = directory.join("browser-relay.json");
    let session = RelaySession { path: path.clone() };
    Ok((path, session))
}

fn validate_relay_config(config: &BrowserRelayConfig) -> Result<(), BrowserHostError> {
    if config.version != 1 || config.pid == 0 {
        return Err(BrowserHostError(
            "browser relay descriptor version or process ID is invalid".into(),
        ));
    }
    if config.token.len() != 64 || !config.token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BrowserHostError("browser relay token is invalid".into()));
    }
    let endpoint = url::Url::parse(&config.endpoint)
        .map_err(|_| BrowserHostError("browser relay endpoint is invalid".into()))?;
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port().is_none()
        || endpoint.path() != "/v1/command"
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(BrowserHostError(
            "browser relay endpoint must be an exact 127.0.0.1 command URL".into(),
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if config.expires_at_epoch_ms < now
        || config.expires_at_epoch_ms > now + 24 * 60 * 60 * 1000 + 10_000
    {
        return Err(BrowserHostError(
            "browser relay descriptor has expired or an invalid lifetime".into(),
        ));
    }
    Ok(())
}

fn secure_token() -> Result<String, BrowserHostError> {
    let mut bytes = [0u8; 32];
    secure_random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(windows)]
fn secure_random_bytes(output: &mut [u8]) -> Result<(), BrowserHostError> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut std::ffi::c_void,
            buffer: *mut u8,
            length: u32,
            flags: u32,
        ) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x00000002;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            output.as_mut_ptr(),
            output.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(BrowserHostError(format!(
            "Windows cryptographic RNG failed: 0x{:08x}",
            status as u32
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn secure_random_bytes(output: &mut [u8]) -> Result<(), BrowserHostError> {
    File::open("/dev/urandom")?.read_exact(output)?;
    Ok(())
}

#[cfg(not(any(windows, unix)))]
fn secure_random_bytes(_output: &mut [u8]) -> Result<(), BrowserHostError> {
    Err(BrowserHostError(
        "secure random tokens are unsupported on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn native_frames_round_trip_unicode_json() {
        let value = json!({"type":"ready", "text":"café 🌐"});
        let mut encoded = Vec::new();
        write_native_frame(&mut encoded, &value).unwrap();
        let mut reader = Cursor::new(encoded);
        assert_eq!(read_native_frame(&mut reader).unwrap(), Some(value));
        assert_eq!(read_native_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn native_frame_rejects_oversize_length_and_truncation() {
        let oversized = (MAX_NATIVE_MESSAGE_BYTES as u32 + 1).to_ne_bytes();
        assert_eq!(
            read_native_frame(&mut Cursor::new(oversized))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let truncated = [5u32.to_ne_bytes().as_slice(), b"{}".as_slice()].concat();
        assert_eq!(
            read_native_frame(&mut Cursor::new(truncated))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn extension_origin_must_match_the_exact_configured_id() {
        let id = "abcdefghijklmnopabcdefghijklmnop";
        assert_eq!(
            validate_extension_origin(&format!("chrome-extension://{id}"), id).unwrap(),
            id
        );
        assert!(
            validate_extension_origin("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", id)
                .is_err()
        );
        assert!(validate_extension_origin("https://example.com", id).is_err());
        assert!(validate_extension_origin("chrome-extension://bad", id).is_err());
    }

    #[test]
    fn extension_results_are_relayed_to_the_waiting_command() {
        let (tx, rx) = mpsc::sync_channel(1);
        let pending = Arc::new(Mutex::new(HashMap::from([(
            "c1".into(),
            PendingCommand { reply: tx },
        )])));
        let result = json!({"tabs":[{"tabId":1,"title":"Example"}]});
        handle_extension_message(
            json!({"type":"command_result","id":"c1","ok":true,"result":result}),
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
            &pending,
        );
        assert_eq!(rx.recv().unwrap().unwrap(), result);
        let (tx, rx) = mpsc::sync_channel(1);
        let pending = Arc::new(Mutex::new(HashMap::from([(
            "c2".into(),
            PendingCommand { reply: tx },
        )])));
        handle_extension_message(
            json!({"type":"command_result","id":"c2","ok":false,"error":"Nothing visible matches 'x'"}),
            "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
            &pending,
        );
        assert_eq!(
            rx.recv().unwrap().unwrap_err(),
            "Nothing visible matches 'x'"
        );
    }

    #[test]
    fn only_bridge_methods_are_dispatchable() {
        for method in [
            "snapshot", "click", "type", "navigate", "tabs", "open", "select", "extract", "wait",
            "ping",
        ] {
            assert!(allowed_browser_method(method));
        }
        for method in ["executeScript", "cookies", "download", "eval", ""] {
            assert!(!allowed_browser_method(method));
        }
    }

    #[test]
    fn command_params_reject_non_web_urls_and_oversized_values() {
        assert!(validate_command_params("snapshot", &json!({})).is_ok());
        assert!(validate_command_params("click", &json!({"target":"Sign in"})).is_ok());
        assert!(validate_command_params("open", &json!({"url":"youtube.com"})).is_ok());
        assert!(
            validate_command_params("navigate", &json!({"url":"javascript:alert(1)"})).is_err()
        );
        assert!(validate_command_params("open", &json!({"url":"file:///C:/secret"})).is_err());
        assert!(validate_command_params("open", &json!({})).is_err());
        assert!(validate_command_params("type", &json!({"text":"x".repeat(20_001)})).is_err());
    }

    #[test]
    fn bundled_extension_id_is_well_formed() {
        assert!(valid_extension_id(BUNDLED_EXTENSION_ID));
    }

    #[test]
    fn relay_config_only_accepts_loopback_and_a_short_lifetime() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let base = BrowserRelayConfig {
            version: 1,
            endpoint: "http://127.0.0.1:43127/v1/command".into(),
            token: "a".repeat(64),
            pid: 42,
            expires_at_epoch_ms: now + 30_000,
        };
        assert!(validate_relay_config(&base).is_ok());
        let mut remote = base.clone();
        remote.endpoint = "http://192.168.1.2:43127/v1/command".into();
        assert!(validate_relay_config(&remote).is_err());
        let mut expired = base;
        expired.expires_at_epoch_ms = now.saturating_sub(1);
        assert!(validate_relay_config(&expired).is_err());
    }
}
