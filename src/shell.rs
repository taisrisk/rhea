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
const MAX_READ_LINES: usize = 4_000;
const MAX_COMMAND_OUTPUT: usize = 128 * 1024;
const MAX_FETCH_BYTES: usize = 256 * 1024;
const DEFAULT_FETCH_BYTES: usize = 32 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 500;
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
const MAX_COMMAND_TIMEOUT_MS: u64 = 900_000;

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
    let shell = args.get("shell").and_then(Value::as_str).unwrap_or("auto");
    let detach = args.get("detach").and_then(Value::as_bool).unwrap_or(false);

    let mut command = shell_command(shell, command_text)?;
    command.current_dir(working_directory).stdin(Stdio::null());
    configure_hidden_process(&mut command);
    if detach {
        // Background start (servers, GUI installers, launching programs): nothing is captured
        // and nothing is waited for; the caller gets the pid.
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let child = command
            .spawn()
            .map_err(|error| format!("Could not start the Shell command: {error}"))?;
        return Ok(json!({
            "detached": true,
            "pid": child.id(),
            "success": true,
            "note": "Started in the background. Verify it is up (for example check its port or process) before relying on it.",
            "working_directory": working_directory.display().to_string(),
        }));
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
    let mut cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if crate::jev::cancel_requested() => {
                cancelled = true;
                kill_process_tree(&mut child);
                break child.wait().ok();
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                timed_out = true;
                kill_process_tree(&mut child);
                break child.wait().ok();
            }
            Err(error) => {
                kill_process_tree(&mut child);
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
    if cancelled {
        return Err("Run cancelled.".into());
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
        "shell": shell,
    }))
}

/// Kill the shell and everything it started (a plain `kill` leaves grandchildren running).
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(target_os = "windows")]
    {
        let mut kill = Command::new("taskkill");
        kill.args(["/T", "/F", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null());
        configure_hidden_process(&mut kill);
        let _ = kill.status();
    }
    let _ = child.kill();
}

/// Build the process for a requested shell. `auto` is the platform default.
fn shell_command(shell: &str, command_text: &str) -> Result<Command, String> {
    let shell = shell.trim().to_ascii_lowercase();
    match shell.as_str() {
        "" | "auto" => Ok(platform_shell(command_text)),
        "powershell" | "pwsh" => {
            let mut command = Command::new(if cfg!(windows) { "powershell.exe" } else { "pwsh" });
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &powershell_script(command_text)]);
            Ok(command)
        }
        "cmd" => {
            if !cfg!(windows) {
                return Err("shell \"cmd\" only exists on Windows.".into());
            }
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/S", "/C", command_text]);
            Ok(command)
        }
        "bash" | "sh" | "zsh" => {
            let program = find_unix_shell(&shell).ok_or_else(|| {
                format!("`{shell}` is not installed or not on PATH (on Windows install Git for Windows for bash).")
            })?;
            let mut command = Command::new(program);
            command.args(["-lc", command_text]);
            Ok(command)
        }
        other => Err(format!("Unknown shell '{other}'. Use auto, powershell, cmd, bash, sh or zsh.")),
    }
}

fn find_unix_shell(name: &str) -> Option<std::path::PathBuf> {
    if !cfg!(windows) {
        return ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|path| path.is_file());
    }
    let mut candidates = vec![
        std::path::PathBuf::from(r"C:\Program Files\Git\bin").join(format!("{name}.exe")),
        std::path::PathBuf::from(r"C:\Program Files (x86)\Git\bin").join(format!("{name}.exe")),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join(format!("{name}.exe")));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// PowerShell's default output encoding mangles non-ASCII text; force UTF-8.
fn powershell_script(command_text: &str) -> String {
    format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; {command_text}")
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
            &powershell_script(command_text),
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
    let ranges: Vec<&str> = args.get("ranges").and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if !ranges.is_empty() {
        return read_line_ranges(path, &ranges);
    }
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

/// The whole text of a file for line-based work (bounded like every other file operation).
fn read_text_for_lines(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(format!("{} exceeds the {MAX_FILE_BYTES}-byte limit for line-based access; use run_command to slice it.", path.display()));
    }
    let bytes = fs::read(path).map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8 text.", path.display()))
}

/// "5", "2-9", "40-" (to the end) -> 1-based inclusive (start, end); `end` is None for "to the end".
fn parse_line_range(text: &str) -> Result<(usize, Option<usize>), String> {
    let text = text.trim();
    let bad = || format!("'{text}' is not a line range; use \"5\", \"2-9\" or \"40-\" (1-based, inclusive).");
    let (start, end) = match text.split_once('-') {
        Some((start, end)) => (start.trim(), end.trim()),
        None => (text, text),
    };
    let start: usize = start.parse().map_err(|_| bad())?;
    if start == 0 {
        return Err(bad());
    }
    let end = if end.is_empty() { None } else { Some(end.parse::<usize>().map_err(|_| bad())?) };
    Ok((start, end))
}

fn read_line_ranges(path: &Path, ranges: &[&str]) -> Result<Value, String> {
    let content = read_text_for_lines(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let mut out = Vec::new();
    let mut emitted = 0_usize;
    let mut truncated = false;
    for spec in ranges {
        let (start, end) = parse_line_range(spec)?;
        let end = end.unwrap_or(total).min(total);
        if start > total.max(1) || end < start {
            out.push(json!({"range": spec, "error": format!("The file has {total} lines; nothing in {spec}."), "content": ""}));
            continue;
        }
        let mut text = String::new();
        for number in start..=end {
            if emitted >= MAX_READ_LINES {
                truncated = true;
                break;
            }
            text.push_str(&format!("{number}| {}\n", lines[number - 1]));
            emitted += 1;
        }
        out.push(json!({"range": spec, "start": start, "end": end, "content": text}));
    }
    Ok(json!({"path": path.display().to_string(), "total_lines": total, "ranges": out, "truncated": truncated,
        "note": "Each line is shown as `number| text`; the number and bar are not part of the file."}))
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
    let edits = args.get("edits").and_then(Value::as_array).filter(|items| !items.is_empty());
    let has_text_mode = args.get("find").and_then(Value::as_str).is_some_and(|text| !text.is_empty());
    match (edits, has_text_mode) {
        (Some(edits), false) => edit_by_lines(Path::new(required_string(args, "path")?), edits),
        (None, true) => edit_by_text(args),
        (Some(_), true) => Err("Give either `edits` (line ranges) or `find`/`replace`, not both.".into()),
        (None, false) => Err("edit_file needs `edits` (a list of {lines, replacement}) or `find`/`replace`.".into()),
    }
}

struct LineEdit {
    /// 1-based first line replaced; `end` is the last (start - 1 means a pure insert before `start`).
    start: usize,
    end: usize,
    text: String,
    spec: String,
}

/// Apply several line-range replacements to one file in a single atomic write. Every range refers
/// to the file as it was when read, so the edits never shift each other.
fn edit_by_lines(path: &Path, edits: &[Value]) -> Result<Value, String> {
    let content = read_text_for_lines(path)?;
    let eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    let last_unterminated = lines.last().is_some_and(|line| !line.ends_with('\n'));
    let mut parsed: Vec<LineEdit> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let spec = edit.get("lines").and_then(Value::as_str).ok_or_else(|| format!("edits[{index}] needs `lines` (e.g. \"2-5\")."))?;
        let text = edit.get("replacement").and_then(Value::as_str).ok_or_else(|| format!("edits[{index}] needs `replacement` (use \"\" to delete the lines)."))?;
        let (start, end) = parse_line_range(spec).map_err(|e| format!("edits[{index}]: {e}"))?;
        let end = end.ok_or_else(|| format!("edits[{index}]: give an explicit end line (\"{start}-{}\"); to insert before line {start} use \"{start}-{}\".", total.max(start), start - 1))?;
        if start > total + 1 || end > total || end + 1 < start {
            return Err(format!("edits[{index}] '{spec}' is outside the file ({total} lines). Nothing was written."));
        }
        parsed.push(LineEdit { start, end, text: text.to_owned(), spec: spec.trim().to_owned() });
    }
    parsed.sort_by_key(|edit| (edit.start, edit.end));
    for pair in parsed.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // Half-open [start, end + 1): overlapping ranges, or an insert strictly inside another range.
        if b.start < a.end + 1 {
            return Err(format!("Edits '{}' and '{}' overlap. Nothing was written.", a.spec, b.spec));
        }
    }
    let mut output: Vec<String> = Vec::with_capacity(total + 8);
    let mut cursor = 1_usize;
    let mut report = Vec::new();
    let mut new_line = 1_usize;
    for edit in &parsed {
        while cursor < edit.start {
            output.push(lines[cursor - 1].to_owned());
            cursor += 1;
            new_line += 1;
        }
        let body = edit.text.strip_suffix("\r\n").or_else(|| edit.text.strip_suffix('\n')).unwrap_or(&edit.text);
        let new_lines: Vec<&str> = if edit.text.is_empty() { Vec::new() } else { body.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect() };
        let reaches_unterminated_end = last_unterminated && edit.end == total && total > 0;
        for (i, line) in new_lines.iter().enumerate() {
            let terminated = !(reaches_unterminated_end && i + 1 == new_lines.len());
            output.push(format!("{line}{}", if terminated { eol } else { "" }));
        }
        let removed = edit.end + 1 - edit.start;
        let first = new_line;
        new_line += new_lines.len();
        report.push(json!({"lines": edit.spec, "replaced_lines": removed, "new_lines": new_lines.len(),
            "now_at": if new_lines.is_empty() { Value::Null } else { json!(format!("{}-{}", first, new_line - 1)) },
            "preview": new_lines.iter().enumerate().take(30).map(|(i, line)| format!("{}| {line}", first + i)).collect::<Vec<_>>().join("\n")}));
        cursor = edit.end + 1;
    }
    while cursor <= total {
        output.push(lines[cursor - 1].to_owned());
        cursor += 1;
    }
    lines.clear();
    let updated: String = output.concat();
    if updated.len() > MAX_FILE_BYTES {
        return Err(format!("The edited file would exceed the {MAX_FILE_BYTES}-byte limit. Nothing was written."));
    }
    let temp = path.with_extension(format!("{}.jarvus-edit", path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")));
    fs::write(&temp, &updated).map_err(|error| format!("Could not stage the edit: {error}"))?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("Could not update {}: {error}", path.display()));
    }
    Ok(json!({"path": path.display().to_string(), "edits_applied": parsed.len(), "lines_before": total,
        "lines_after": updated.lines().count(), "bytes_written": updated.len(), "edits": report}))
}

fn edit_by_text(args: &Value) -> Result<Value, String> {
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
    fn run_command_schema_exposes_shell_detach_and_long_timeouts() {
        let tools = tool_specs();
        let run = tools.as_array().unwrap().iter().find(|tool| tool["name"] == "mcp__jarvus_shell__run_command").unwrap();
        let properties = run["parameters"]["properties"].as_object().unwrap();
        for key in ["command", "working_directory", "timeout_ms", "shell", "detach"] {
            assert!(properties.contains_key(key), "missing {key}");
        }
        assert_eq!(run["parameters"]["required"].as_array().unwrap().len(), properties.len(), "strict schemas list every property");
    }

    #[test]
    fn unknown_or_unavailable_shells_are_rejected_cleanly() {
        let directory = temporary_directory("badshell");
        let error = execute("mcp__jarvus_shell__run_command", &json!({"command":"echo hi","working_directory":directory.to_str().unwrap(),"shell":"fish","detach":null,"timeout_ms":null})).unwrap_err();
        assert!(error.contains("Unknown shell"), "{error}");
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(windows)]
    #[test]
    fn windows_shells_cmd_powershell_and_utf8_output() {
        let directory = temporary_directory("shells");
        let dir = directory.to_str().unwrap();
        let cmd = execute("mcp__jarvus_shell__run_command", &json!({"command":"echo from-cmd","working_directory":dir,"shell":"cmd","detach":null,"timeout_ms":null})).unwrap();
        assert_eq!(cmd["success"], true);
        assert!(cmd["stdout"].as_str().unwrap().contains("from-cmd"));
        let ps = execute("mcp__jarvus_shell__run_command", &json!({"command":"Write-Output 'héllo ✓'","working_directory":dir,"shell":"powershell","detach":null,"timeout_ms":null})).unwrap();
        assert!(ps["stdout"].as_str().unwrap().contains("héllo ✓"), "{ps}");
        let failing = execute("mcp__jarvus_shell__run_command", &json!({"command":"exit 7","working_directory":dir,"shell":null,"detach":null,"timeout_ms":null})).unwrap();
        assert_eq!(failing["success"], false);
        assert_eq!(failing["exit_code"], 7);
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(windows)]
    #[test]
    fn detached_commands_return_a_pid_without_waiting() {
        let directory = temporary_directory("detach");
        let started = Instant::now();
        let result = execute("mcp__jarvus_shell__run_command", &json!({"command":"Start-Sleep 20","working_directory":directory.to_str().unwrap(),"shell":null,"detach":true,"timeout_ms":null})).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(result["detached"], true);
        let pid = result["pid"].as_u64().unwrap();
        assert!(pid > 0);
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).output();
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(windows)]
    #[test]
    fn timeouts_kill_the_whole_process_tree() {
        let directory = temporary_directory("tree");
        let started = Instant::now();
        let result = execute("mcp__jarvus_shell__run_command", &json!({"command":"Start-Sleep 30","working_directory":directory.to_str().unwrap(),"shell":null,"detach":null,"timeout_ms":1500})).unwrap();
        assert_eq!(result["timed_out"], true);
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = fs::remove_dir_all(directory);
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
    fn read_file_returns_only_the_requested_numbered_line_ranges() {
        let directory = temporary_directory("read-ranges");
        let file = directory.join("f.txt");
        fs::write(&file, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let result = read_file(&json!({"path":file,"max_bytes":null,"ranges":["2-3","5-"]})).unwrap();
        assert_eq!(result["total_lines"], 5);
        assert_eq!(result["ranges"][0]["content"], "2| two\n3| three\n");
        assert_eq!(result["ranges"][1]["content"], "5| five\n");
        let past = read_file(&json!({"path":file,"ranges":["9-12"]})).unwrap();
        assert!(past["ranges"][0]["error"].as_str().unwrap().contains("5 lines"));
        assert!(read_file(&json!({"path":file,"ranges":["0-2"]})).is_err());
        assert!(read_file(&json!({"path":file,"ranges":["x"]})).is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn edit_file_applies_several_line_ranges_atomically_against_the_original_numbering() {
        let directory = temporary_directory("edit-lines");
        let file = directory.join("f.txt");
        fs::write(&file, "a\nb\nc\nd\ne\nf\n").unwrap();
        let result = edit_file(&json!({"path":file,"edits":[
            {"lines":"5-6","replacement":"E\nF\nG\n"},
            {"lines":"2-3","replacement":"B"},
            {"lines":"4-3","replacement":"inserted"},
        ],"find":null,"replace":null})).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\nB\ninserted\nd\nE\nF\nG\n");
        assert_eq!(result["edits_applied"], 3);
        assert_eq!(result["lines_after"], 7);
        assert_eq!(result["edits"][2]["now_at"], "5-7");
        edit_file(&json!({"path":file,"edits":[{"lines":"1","replacement":""}]})).unwrap();
        assert!(fs::read_to_string(&file).unwrap().starts_with("B\n"));
        let before = fs::read_to_string(&file).unwrap();
        assert!(edit_file(&json!({"path":file,"edits":[{"lines":"1-3","replacement":"x"},{"lines":"3-4","replacement":"y"}]})).unwrap_err().contains("overlap"));
        assert!(edit_file(&json!({"path":file,"edits":[{"lines":"2-99","replacement":"x"}]})).unwrap_err().contains("outside"));
        assert!(edit_file(&json!({"path":file,"edits":[{"lines":"2-3","replacement":"x"}],"find":"B","replace":"b"})).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), before, "a rejected edit writes nothing");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn line_edits_keep_crlf_endings_and_a_missing_final_newline() {
        let directory = temporary_directory("edit-crlf");
        let file = directory.join("f.txt");
        fs::write(&file, "a\r\nb\r\nc").unwrap();
        edit_file(&json!({"path":file,"edits":[{"lines":"2-3","replacement":"x\ny"}]})).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\r\nx\r\ny");
        edit_file(&json!({"path":file,"edits":[{"lines":"2-1","replacement":"mid"}]})).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\r\nmid\r\nx\r\ny");
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
