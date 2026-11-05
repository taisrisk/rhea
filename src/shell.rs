//! Local Shell connection package for command, file, and HTTP work.
//!
//! Commands run as the current user, without an elevated prompt or a visible
//! console window. Outputs and file reads are bounded before they are returned
//! to the model.

use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_COMMAND_OUTPUT: usize = 128 * 1024;
const MAX_FETCH_BYTES: usize = 256 * 1024;
const DEFAULT_FETCH_BYTES: usize = 32 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 500;
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
const MAX_COMMAND_TIMEOUT_MS: u64 = 180_000;

#[derive(Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

pub fn tool_specs() -> Value {
    serde_json::from_str(include_str!("shell_toolset.json"))
        .expect("src/shell_toolset.json must contain valid OpenAI function tools")
}

pub fn execute(name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "mcp__jarvus_shell__run_command" => run_command(args),
        "mcp__jarvus_shell__list_directory" => list_directory(args),
        "mcp__jarvus_shell__read_file" => read_file(args),
        "mcp__jarvus_shell__write_file" => write_file(args),
        "mcp__jarvus_shell__edit_file" => edit_file(args),
        "mcp__jarvus_shell__create_directory" => create_directory(args),
        "mcp__jarvus_shell__web_fetch" => web_fetch(args),
        _ => Err(format!("Unknown Shell tool '{name}'.")),
    }
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("'{key}' is required."))
}

fn run_command(args: &Value) -> Result<Value, String> {
    let command_text = required_string(args, "command")?;
    let working_directory = Path::new(required_string(args, "working_directory")?);
    if !working_directory.is_dir() {
        return Err(format!(
            "Working directory does not exist or is not a directory: {}",
            working_directory.display()
        ));
    }
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS)
        .clamp(1_000, MAX_COMMAND_TIMEOUT_MS);

    let mut command = platform_shell(command_text);
    command
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_hidden_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start the Shell command: {error}"))?;

    let stdout_capture = Arc::new(Mutex::new(CapturedOutput::default()));
    let stderr_capture = Arc::new(Mutex::new(CapturedOutput::default()));
    let stdout_reader = child
        .stdout
        .take()
        .map(|stdout| drain_capped(stdout, Arc::clone(&stdout_capture), MAX_COMMAND_OUTPUT));
    let stderr_reader = child
        .stderr
        .take()
        .map(|stderr| drain_capped(stderr, Arc::clone(&stderr_capture), MAX_COMMAND_OUTPUT));

    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Failed while waiting for Shell command: {error}"));
            }
        }
    };

    // Readers drain pipes in the background. Do not let a detached descendant
    // that inherited a pipe hold up the UI or the next model turn indefinitely.
    let drain_deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < drain_deadline
        && (stdout_reader
            .as_ref()
            .is_some_and(|reader| !reader.is_finished())
            || stderr_reader
                .as_ref()
                .is_some_and(|reader| !reader.is_finished()))
    {
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(reader) = stdout_reader.filter(|reader| reader.is_finished()) {
        let _ = reader.join();
    }
    if let Some(reader) = stderr_reader.filter(|reader| reader.is_finished()) {
        let _ = reader.join();
    }

    let stdout = snapshot_capture(&stdout_capture);
    let stderr = snapshot_capture(&stderr_capture);
    let success = status.as_ref().is_some_and(|status| status.success()) && !timed_out;
    let exit_code = status.as_ref().and_then(|status| status.code());
    Ok(json!({
        "exit_code": exit_code,
        "success": success,
        "timed_out": timed_out,
        "duration_ms": started.elapsed().as_millis(),
        "stdout": String::from_utf8_lossy(&stdout.bytes),
        "stderr": String::from_utf8_lossy(&stderr.bytes),
        "stdout_truncated": stdout.truncated,
        "stderr_truncated": stderr.truncated,
        "working_directory": working_directory.display().to_string(),
    }))
}

fn platform_shell(command_text: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command_text,
        ]);
        command
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/bin/zsh");
        command.args(["-lc", command_text]);
        command
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let mut command = Command::new("/bin/sh");
        command.args(["-lc", command_text]);
        command
    }
}

#[cfg(target_os = "windows")]
fn configure_hidden_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn configure_hidden_process(_command: &mut Command) {}

fn drain_capped<R>(
    mut reader: R,
    capture: Arc<Mutex<CapturedOutput>>,
    cap: usize,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let Ok(mut capture) = capture.lock() else {
                break;
            };
            let available = cap.saturating_sub(capture.bytes.len());
            let kept = read.min(available);
            capture.bytes.extend_from_slice(&chunk[..kept]);
            if kept < read {
                capture.truncated = true;
            }
        }
    })
}

fn snapshot_capture(capture: &Arc<Mutex<CapturedOutput>>) -> CapturedOutput {
    capture
        .lock()
        .map(|capture| CapturedOutput {
            bytes: capture.bytes.clone(),
            truncated: capture.truncated,
        })
        .unwrap_or_default()
}

fn list_directory(args: &Value) -> Result<Value, String> {
    let path = Path::new(required_string(args, "path")?);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, MAX_DIRECTORY_ENTRIES as u64) as usize;
    let entries = fs::read_dir(path)
        .map_err(|error| format!("Could not list {}: {error}", path.display()))?;
    let mut items = Vec::new();
    let mut truncated = false;
    for entry in entries {
        if items.len() == limit {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|error| format!("Could not read a directory entry: {error}"))?;
        let metadata = entry
            .metadata()
            .map_err(|error| format!("Could not inspect {}: {error}", entry.path().display()))?;
        items.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "path": entry.path().display().to_string(),
            "directory": metadata.is_dir(),
            "file": metadata.is_file(),
            "size_bytes": metadata.len(),
        }));
    }
    items.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(json!({"path":path.display().to_string(),"entries":items,"truncated":truncated}))
}

fn read_file(args: &Value) -> Result<Value, String> {
    let path = Path::new(required_string(args, "path")?);
    let limit = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(64 * 1024)
        .clamp(1, MAX_FILE_BYTES as u64) as usize;
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    fs::File::open(path)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?
        .take((limit.saturating_add(1)) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let content = std::str::from_utf8(&bytes[..bytes.len().min(limit)])
        .map_err(|_| format!("{} is not valid UTF-8 text.", path.display()))?;
    Ok(
        json!({"path":path.display().to_string(),"content":content,"bytes_read":content.len(),"truncated":truncated}),
    )
}

fn write_file(args: &Value) -> Result<Value, String> {
    let path = Path::new(required_string(args, "path")?);
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or("'content' is required.")?;
    if content.len() > MAX_FILE_BYTES {
        return Err(format!(
            "Content exceeds the {MAX_FILE_BYTES}-byte file write limit."
        ));
    }
    let overwrite = args
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let existed = path.exists();
    if existed && !overwrite {
        return Err(format!(
            "{} already exists; set overwrite=true only when replacing it is intended.",
            path.display()
        ));
    }
    create_parent(path)?;
    fs::write(path, content)
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
    Ok(json!({"path":path.display().to_string(),"bytes_written":content.len(),"created":!existed}))
}

fn edit_file(args: &Value) -> Result<Value, String> {
    let path = Path::new(required_string(args, "path")?);
    let old = args
        .get("find")
        .and_then(Value::as_str)
        .ok_or("'find' is required.")?;
    let replacement = args
        .get("replace")
        .and_then(Value::as_str)
        .ok_or("'replace' is required.")?;
    if old.is_empty() {
        return Err("'find' must contain at least one character.".into());
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "{} exceeds the {MAX_FILE_BYTES}-byte edit limit.",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize + 1);
    fs::File::open(path)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_FILE_BYTES}-byte edit limit.",
            path.display()
        ));
    }
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} is not valid UTF-8 text.", path.display()))?;
    let occurrences = content.match_indices(old).count();
    if occurrences != 1 {
        return Err(format!(
            "Expected exactly one match in {}; found {occurrences}. No changes were written.",
            path.display()
        ));
    }
    let updated = content.replacen(old, replacement, 1);
    fs::write(path, &updated)
        .map_err(|error| format!("Could not update {}: {error}", path.display()))?;
    Ok(json!({"path":path.display().to_string(),"replacements":1,"bytes_written":updated.len()}))
}

fn create_directory(args: &Value) -> Result<Value, String> {
    let path = Path::new(required_string(args, "path")?);
    let created = !path.exists();
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create {}: {error}", path.display()))?;
    Ok(json!({"path":path.display().to_string(),"created":created}))
}

fn create_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    Ok(())
}

fn web_fetch(args: &Value) -> Result<Value, String> {
    let url = required_string(args, "url")?;
    let max_bytes = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_FETCH_BYTES as u64)
        .clamp(1024, MAX_FETCH_BYTES as u64) as usize;
    let parsed = url::Url::parse(url).map_err(|error| format!("Invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Shell web fetch only supports http and https URLs.".into());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(format!("JARVUS/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("Could not create HTTP client: {error}"))?;
    let mut response = client
        .get(parsed.clone())
        .send()
        .map_err(|error| format!("HTTP fetch failed: {error}"))?;
    let status = response.status();
    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read HTTP response: {error}"))?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    Ok(json!({
        "requested_url":parsed.as_str(),
        "final_url":final_url.as_str(),
        "status":status.as_u16(),
        "success":status.is_success(),
        "content_type":content_type,
        "body":String::from_utf8_lossy(&bytes),
        "truncated":truncated,
        "untrusted_content":true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, time::SystemTime};

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("jarvus-shell-{label}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn shell_toolset_is_valid_and_namespaced() {
        let tools = tool_specs();
        let tools = tools.as_array().unwrap();
        assert!(tools.len() >= 7);
        assert!(tools.iter().all(|tool| tool["type"] == "function"));
        assert!(tools.iter().all(|tool| {
            tool["name"]
                .as_str()
                .unwrap()
                .starts_with("mcp__jarvus_shell__")
        }));
    }

    #[test]
    fn write_refuses_existing_file_without_explicit_overwrite() {
        let directory = temporary_directory("overwrite");
        let file = directory.join("note.txt");
        fs::write(&file, "keep").unwrap();
        let error =
            write_file(&json!({"path":file,"content":"replace","overwrite":false})).unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn edit_requires_one_exact_match() {
        let directory = temporary_directory("edit");
        let file = directory.join("code.txt");
        fs::write(&file, "alpha\nbeta\n").unwrap();
        edit_file(&json!({"path":file,"find":"beta","replace":"gamma"})).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "alpha\ngamma\n");
        let error = edit_file(&json!({"path":file,"find":"missing","replace":"x"})).unwrap_err();
        assert!(error.contains("found 0"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn read_file_stops_at_the_requested_limit() {
        let directory = temporary_directory("read-limit");
        let file = directory.join("large.txt");
        fs::write(&file, "x".repeat(1024 * 1024)).unwrap();
        let result = read_file(&json!({"path":file,"max_bytes":32})).unwrap();
        assert_eq!(result["bytes_read"], 32);
        assert_eq!(result["content"].as_str().unwrap().len(), 32);
        assert_eq!(result["truncated"], true);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn command_runs_in_the_requested_directory_and_captures_both_streams() {
        let directory = temporary_directory("command-output");
        #[cfg(windows)]
        let command_text =
            "Write-Output 'shell-stdout'; [Console]::Error.WriteLine('shell-stderr')";
        #[cfg(not(windows))]
        let command_text = "printf 'shell-stdout'; printf 'shell-stderr' >&2";
        let result = run_command(
            &json!({"command":command_text,"working_directory":directory,"timeout_ms":5000}),
        )
        .unwrap();
        assert_eq!(result["success"], true);
        assert!(result["stdout"].as_str().unwrap().contains("shell-stdout"));
        assert!(result["stderr"].as_str().unwrap().contains("shell-stderr"));
        assert_eq!(result["working_directory"], directory.display().to_string());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn list_directory_is_bounded_and_reports_entry_types() {
        let directory = temporary_directory("list");
        fs::write(directory.join("file.txt"), "value").unwrap();
        fs::create_dir(directory.join("child")).unwrap();
        let result = list_directory(&json!({"path":directory,"limit":1})).unwrap();
        assert_eq!(result["entries"].as_array().unwrap().len(), 1);
        assert_eq!(result["truncated"], true);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn command_requires_an_existing_working_directory_and_times_out() {
        let error = run_command(
            &json!({"command":"echo hi","working_directory":"Z:\\missing","timeout_ms":1000}),
        )
        .unwrap_err();
        assert!(error.contains("Working directory"));

        let directory = temporary_directory("timeout");
        #[cfg(windows)]
        let command_text = "Start-Sleep -Seconds 5";
        #[cfg(not(windows))]
        let command_text = "sleep 5";
        let result = run_command(
            &json!({"command":command_text,"working_directory":directory,"timeout_ms":1000}),
        )
        .unwrap();
        assert_eq!(result["timed_out"], true);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn web_fetch_rejects_non_http_schemes_before_network_access() {
        let error = web_fetch(&json!({"url":"file:///etc/passwd"})).unwrap_err();
        assert!(error.contains("http and https"));
    }
}
