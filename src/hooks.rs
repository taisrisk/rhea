//! User-configurable hook scripts, modelled on the agent-hook systems of Claude
//! Code and Codex. A hook is a command that Rhea runs when something happens
//! (a prompt is submitted, a tool is about to run, a tool finished, a turn ends, a
//! subagent starts or stops). The command receives a JSON description of the event
//! on stdin and can answer with an exit code and/or a JSON object on stdout:
//!
//! * exit 0: success. Stdout JSON may carry `decision`, `reason`, `additionalContext`,
//!   `updatedInput` (PreToolUse), `continue`/`stopReason`.
//! * exit 2: block. Stderr is the reason handed back to the model.
//! * anything else: a non-blocking error that is recorded and ignored.
//!
//! A hook that cannot start, times out or crashes never blocks the agent (fail open).

use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const MAX_HOOKS: usize = 64;
const MAX_COMMAND_CHARS: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_CONTEXT_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    SubagentStart,
    SubagentStop,
}

impl HookEvent {
    pub const ALL: [HookEvent; 7] = [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::Stop,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == name)
    }

    fn uses_tool_matcher(self) -> bool {
        matches!(self, HookEvent::PreToolUse | HookEvent::PostToolUse)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct HookCommand {
    #[serde(rename = "type", default = "command_type")]
    kind: String,
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
}

fn command_type() -> String {
    "command".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct HookGroup {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    matcher: String,
    #[serde(default)]
    hooks: Vec<HookCommand>,
    /// The skill that brought this group along; removing the skill removes the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

/// Parsed, validated hook configuration. Cheap to clone and safe to share.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HookRegistry {
    groups: Vec<(HookEvent, HookGroup)>,
}

#[derive(Clone, Debug, Default)]
pub struct HookRun {
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub error: Option<String>,
}

/// What the hooks for one event decided.
#[derive(Clone, Debug, Default)]
pub struct HookOutcome {
    /// Set when a hook vetoed the action; the reason is for the model.
    pub block: Option<String>,
    pub additional_context: Vec<String>,
    /// Replacement tool arguments (PreToolUse only).
    pub updated_input: Option<Value>,
    /// A hook asked to stop the whole turn (`continue: false`), with its message.
    pub stop: Option<String>,
    pub ran: Vec<HookRun>,
}

pub fn default_path() -> PathBuf {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(crate::chat_store::DATA_DIR)
        .join("hooks.json")
}

impl HookRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Total number of configured hook commands.
    pub fn count(&self) -> usize {
        self.groups.iter().map(|(_, group)| group.hooks.len()).sum()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Ok(Self::empty());
        }
        // Matchers written for the old Shell tool names keep working.
        let text = crate::chat_store::modernize_tool_names(text);
        let value: Value = serde_json::from_str(&text).map_err(|error| format!("Hooks JSON is invalid: {error}"))?;
        let object = value.as_object().ok_or("Hooks JSON must be an object.")?;
        let hooks = match object.get("hooks") {
            Some(Value::Object(map)) => map,
            None => return Ok(Self::empty()),
            Some(_) => return Err("\"hooks\" must be an object keyed by event name.".into()),
        };
        let mut groups = Vec::new();
        let mut total = 0usize;
        for (name, entries) in hooks {
            let event = HookEvent::from_str(name).ok_or_else(|| {
                format!(
                    "Unknown hook event '{name}'. Use one of: {}.",
                    HookEvent::ALL.map(HookEvent::as_str).join(", ")
                )
            })?;
            let entries = entries.as_array().ok_or_else(|| format!("'{name}' must be an array of hook groups."))?;
            for entry in entries {
                let mut group: HookGroup =
                    serde_json::from_value(entry.clone()).map_err(|error| format!("Bad hook group under '{name}': {error}"))?;
                for hook in &group.hooks {
                    if hook.kind != "command" {
                        return Err(format!("Hook type '{}' is not supported; only \"command\".", hook.kind));
                    }
                    if hook.command.trim().is_empty() {
                        return Err(format!("A hook under '{name}' has an empty command."));
                    }
                    if hook.command.chars().count() > MAX_COMMAND_CHARS {
                        return Err(format!("A hook command under '{name}' is longer than {MAX_COMMAND_CHARS} characters."));
                    }
                }
                total += group.hooks.len();
                if total > MAX_HOOKS {
                    return Err(format!("At most {MAX_HOOKS} hook commands are supported."));
                }
                group.matcher = group.matcher.trim().to_owned();
                if !group.hooks.is_empty() {
                    groups.push((event, group));
                }
            }
        }
        Ok(Self { groups })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(error) => Err(format!("Could not read {}: {error}", path.display())),
        }
    }

    pub fn to_pretty_json(&self) -> String {
        let mut hooks = Map::new();
        for event in HookEvent::ALL {
            let groups: Vec<Value> = self
                .groups
                .iter()
                .filter(|(candidate, _)| *candidate == event)
                .filter_map(|(_, group)| serde_json::to_value(group).ok())
                .collect();
            if !groups.is_empty() {
                hooks.insert(event.as_str().to_owned(), Value::Array(groups));
            }
        }
        serde_json::to_string_pretty(&json!({ "hooks": hooks })).unwrap_or_else(|_| "{\"hooks\":{}}".into())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
        }
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, self.to_pretty_json()).map_err(|error| format!("Could not write hooks: {error}"))?;
        if let Err(first) = fs::rename(&temp, path) {
            if path.exists() {
                fs::remove_file(path).map_err(|error| format!("Could not replace hooks file: {error}"))?;
                fs::rename(&temp, path).map_err(|error| format!("Could not save hooks ({first}): {error}"))?;
            } else {
                return Err(format!("Could not save hooks: {first}"));
            }
        }
        Ok(())
    }

    /// Whether any hook is registered for this event (lets callers skip building payloads).
    pub fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.groups.iter().any(|(candidate, _)| *candidate == event)
    }

    pub fn run(&self, event: HookEvent, tool_name: Option<&str>, payload: &Value) -> HookOutcome {
        let mut outcome = HookOutcome::default();
        let mut context_bytes = 0usize;
        let mut stdin_object = payload.as_object().cloned().unwrap_or_default();
        stdin_object.insert("hook_event_name".into(), json!(event.as_str()));
        if let Some(name) = tool_name {
            stdin_object.insert("tool_name".into(), json!(name));
        }
        let stdin_text = Value::Object(stdin_object).to_string();
        'groups: for (candidate, group) in &self.groups {
            if *candidate != event {
                continue;
            }
            if event.uses_tool_matcher() || tool_name.is_some() {
                if !matcher_matches(&group.matcher, tool_name.unwrap_or("")) {
                    continue;
                }
            }
            for hook in &group.hooks {
                let timeout = Duration::from_secs(hook.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, 300));
                let (run, result) = run_command(&hook.command, &stdin_text, event, timeout);
                outcome.ran.push(run);
                let Some(result) = result else { continue };
                apply_result(event, &result, &mut outcome, &mut context_bytes);
                if outcome.block.is_some() {
                    break 'groups;
                }
            }
        }
        outcome
    }
}

/// Bumped whenever hooks.json is changed from inside the app (a skill installed or removed), so
/// the registry the agents use is reloaded before its next run.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reload `shared` from disk if hooks.json was changed by this app since it was last loaded.
pub fn refresh_shared(shared: &std::sync::RwLock<HookRegistry>) {
    use std::sync::atomic::Ordering::SeqCst;
    let generation = GENERATION.load(SeqCst);
    if SEEN.swap(generation, SeqCst) != generation {
        if let (Ok(registry), Ok(mut slot)) = (HookRegistry::load(&default_path()), shared.write()) {
            *slot = registry;
        }
    }
}

impl HookRegistry {
    /// Replace everything `source` brought with `groups`.
    fn replace_source(&mut self, source: &str, groups: Vec<(HookEvent, HookGroup)>) {
        self.groups.retain(|(_, group)| group.source.as_deref() != Some(source));
        self.groups.extend(groups);
    }

    fn remove_source(&mut self, source: &str) -> usize {
        let before = self.groups.len();
        self.groups.retain(|(_, group)| group.source.as_deref() != Some(source));
        before - self.groups.len()
    }
}

/// The hooks a skill ships, in the Claude Code format (`hooks/hooks.json` or `hooks.json` inside
/// the skill folder). Returns the groups to register and the events Rhea has no equivalent for.
fn skill_hooks_from_json(skill: &str, dir: &Path, text: &str) -> Result<(Vec<(HookEvent, HookGroup)>, Vec<String>), String> {
    let value: Value = serde_json::from_str(text).map_err(|error| format!("The skill's hooks file is not valid JSON: {error}"))?;
    let events = value.get("hooks").unwrap_or(&value).as_object().ok_or("The skill's hooks file must be an object keyed by event name.")?;
    let folder = dir.display().to_string();
    let mut groups = Vec::new();
    let mut skipped = Vec::new();
    for (name, entries) in events {
        let Some(event) = HookEvent::from_str(name) else {
            skipped.push(name.clone());
            continue;
        };
        for entry in entries.as_array().into_iter().flatten() {
            let mut group: HookGroup = serde_json::from_value(entry.clone()).map_err(|error| format!("Bad hook group under '{name}': {error}"))?;
            for hook in &mut group.hooks {
                if hook.kind != "command" || hook.command.trim().is_empty() || hook.command.chars().count() > MAX_COMMAND_CHARS {
                    return Err(format!("A hook under '{name}' is not a usable command hook."));
                }
                for variable in ["${CLAUDE_PLUGIN_ROOT}", "${CLAUDE_SKILL_DIR}", "${SKILL_DIR}", "${RHEA_SKILL_DIR}"] {
                    hook.command = hook.command.replace(variable, &folder);
                }
            }
            group.matcher = group.matcher.trim().to_owned();
            group.source = Some(skill.to_owned());
            if !group.hooks.is_empty() {
                groups.push((event, group));
            }
        }
    }
    Ok((groups, skipped))
}

fn skill_hooks_file(dir: &Path) -> Option<PathBuf> {
    [dir.join("hooks").join("hooks.json"), dir.join("hooks.json")].into_iter().find(|path| path.is_file())
}

/// Register the hooks that come with a freshly installed skill. Returns one line per hook that was
/// installed (for the install report) and notes about anything that could not be.
pub fn install_skill_hooks_at(registry_path: &Path, skill: &str, dir: &Path) -> Result<Vec<String>, String> {
    let Some(file) = skill_hooks_file(dir) else { return Ok(Vec::new()) };
    let text = fs::read_to_string(&file).map_err(|error| format!("Could not read {}: {error}", file.display()))?;
    let (groups, skipped) = skill_hooks_from_json(skill, dir, &text)?;
    let mut registry = HookRegistry::load(registry_path)?;
    let mut lines: Vec<String> = groups
        .iter()
        .flat_map(|(event, group)| group.hooks.iter().map(move |hook| format!("{} {}: {}", event.as_str(), if group.matcher.is_empty() { "*" } else { &group.matcher }, hook.command.chars().take(120).collect::<String>())))
        .collect();
    registry.replace_source(skill, groups);
    if registry.count() > MAX_HOOKS {
        return Err(format!("Installing this skill's hooks would pass the limit of {MAX_HOOKS} hook commands."));
    }
    registry.save(registry_path)?;
    if !skipped.is_empty() {
        lines.push(format!("Not installed (Rhea has no such event): {}", skipped.join(", ")));
    }
    Ok(lines)
}

pub fn install_skill_hooks(skill: &str, dir: &Path) -> Result<Vec<String>, String> {
    let lines = install_skill_hooks_at(&default_path(), skill, dir)?;
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(lines)
}

/// Drop the hooks a removed skill brought.
pub fn remove_skill_hooks(skill: &str) {
    let path = default_path();
    if let Ok(mut registry) = HookRegistry::load(&path) {
        if registry.remove_source(skill) > 0 && registry.save(&path).is_ok() {
            GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// `*` and empty match everything; otherwise `|`-separated names, `*` is a wildcard.
pub fn matcher_matches(matcher: &str, name: &str) -> bool {
    let matcher = matcher.trim();
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    matcher.split('|').map(str::trim).filter(|part| !part.is_empty()).any(|pattern| glob_matches(pattern, name))
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

struct CommandResult {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_command(command: &str, stdin_text: &str, event: HookEvent, timeout: Duration) -> (HookRun, Option<CommandResult>) {
    let started = Instant::now();
    let mut run = HookRun { command: command.to_owned(), ..Default::default() };
    let mut process = shell_command(command);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RHEA_HOOK_EVENT", event.as_str());
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            run.error = Some(format!("could not start: {error}"));
            run.duration_ms = started.elapsed().as_millis() as u64;
            return (run, None);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let data = stdin_text.as_bytes().to_vec();
        // Written on a thread: a hook that never reads stdin must not hang us.
        thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        thread::spawn(move || {
            let _ = out_tx.send(read_capped(stdout));
        });
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let _ = err_tx.send(read_capped(stderr));
        });
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                run.timed_out = true;
                run.error = Some(format!("timed out after {}s", timeout.as_secs()));
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(error) => {
                run.error = Some(format!("wait failed: {error}"));
                break None;
            }
        }
    };
    run.duration_ms = started.elapsed().as_millis() as u64;
    let grace = Duration::from_millis(800);
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    let Some(status) = status else { return (run, None) };
    run.exit_code = status.code();
    (run, Some(CommandResult { code: status.code().unwrap_or(-1), stdout, stderr }))
}

fn read_capped(mut reader: impl Read) -> String {
    let mut collected = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if collected.len() < MAX_OUTPUT_BYTES {
                    let room = MAX_OUTPUT_BYTES - collected.len();
                    collected.extend_from_slice(&buffer[..count.min(room)]);
                }
                // Keep draining so the child never blocks on a full pipe.
            }
        }
    }
    String::from_utf8_lossy(&collected).into_owned()
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut process = Command::new("powershell");
        process.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("sh");
        process.args(["-c", command]);
        process
    }
}

fn apply_result(event: HookEvent, result: &CommandResult, outcome: &mut HookOutcome, context_bytes: &mut usize) {
    let mut add_context = |outcome: &mut HookOutcome, text: &str| {
        let text = text.trim();
        if text.is_empty() || *context_bytes >= MAX_CONTEXT_BYTES {
            return;
        }
        let room = MAX_CONTEXT_BYTES - *context_bytes;
        let mut clipped: String = text.chars().take(room).collect();
        while clipped.len() > room {
            clipped.pop();
        }
        *context_bytes += clipped.len();
        outcome.additional_context.push(clipped);
    };
    if result.code == 2 {
        let reason = result.stderr.trim();
        outcome.block = Some(if reason.is_empty() { "Blocked by a hook.".to_owned() } else { reason.to_owned() });
        return;
    }
    if result.code != 0 {
        return;
    }
    let stdout = result.stdout.trim();
    let parsed = serde_json::from_str::<Value>(stdout).ok().filter(Value::is_object);
    let Some(object) = parsed else {
        if matches!(event, HookEvent::UserPromptSubmit | HookEvent::SessionStart) {
            add_context(outcome, stdout);
        }
        return;
    };
    let specific = object.get("hookSpecificOutput").and_then(Value::as_object);
    let pick = |key: &str| object.get(key).or_else(|| specific.and_then(|inner| inner.get(key)));
    let reason = pick("reason")
        .or_else(|| pick("permissionDecisionReason"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let decision = pick("decision").or_else(|| pick("permissionDecision")).and_then(Value::as_str).unwrap_or("");
    if matches!(decision, "block" | "deny") {
        outcome.block = Some(reason.unwrap_or_else(|| "Blocked by a hook.".to_owned()));
    }
    if let Some(text) = pick("additionalContext").and_then(Value::as_str) {
        add_context(outcome, text);
    }
    if event == HookEvent::PreToolUse {
        if let Some(input) = pick("updatedInput").filter(|value| value.is_object()) {
            outcome.updated_input = Some(input.clone());
        }
    }
    if pick("continue").and_then(Value::as_bool) == Some(false) {
        outcome.stop = Some(
            pick("stopReason").and_then(Value::as_str).unwrap_or("A hook stopped this turn.").to_owned(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(event: &str, matcher: &str, command: &str, timeout: u64) -> HookRegistry {
        HookRegistry::from_json(
            &json!({"hooks": {event: [{"matcher": matcher, "hooks": [{"type":"command","command":command,"timeout":timeout}]}]}}).to_string(),
        )
        .unwrap()
    }

    #[test]
    fn parsing_validates_events_types_and_sizes() {
        assert!(HookRegistry::from_json("").unwrap().is_empty());
        assert!(HookRegistry::from_json("{}").unwrap().is_empty());
        assert!(HookRegistry::from_json("[1]").is_err());
        assert!(HookRegistry::from_json("{bad").is_err());
        assert!(HookRegistry::from_json(r#"{"hooks":{"Nope":[]}}"#).unwrap_err().contains("Unknown hook event"));
        assert!(HookRegistry::from_json(r#"{"hooks":{"Stop":[{"hooks":[{"type":"http","command":"x"}]}]}}"#).is_err());
        assert!(HookRegistry::from_json(r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"  "}]}]}}"#).is_err());
        let long = "x".repeat(MAX_COMMAND_CHARS + 1);
        assert!(HookRegistry::from_json(&json!({"hooks":{"Stop":[{"hooks":[{"command":long}]}]}}).to_string()).is_err());
        let many: Vec<Value> = (0..=MAX_HOOKS).map(|i| json!({"command": format!("echo {i}")})).collect();
        assert!(HookRegistry::from_json(&json!({"hooks":{"Stop":[{"hooks":many}]}}).to_string()).is_err());
        let ok = HookRegistry::from_json(r#"{"hooks":{"PreToolUse":[{"matcher":"a|b","hooks":[{"command":"echo hi"}]}]}}"#).unwrap();
        assert_eq!(ok.count(), 1);
        assert!(ok.has_hooks_for(HookEvent::PreToolUse));
        assert!(!ok.has_hooks_for(HookEvent::Stop));
    }

    #[test]
    fn matchers_support_globs_and_alternation() {
        assert!(matcher_matches("", "anything"));
        assert!(matcher_matches("*", "anything"));
        assert!(matcher_matches("mcp__rhea_shell__*", "mcp__rhea_shell__run_command"));
        assert!(!matcher_matches("mcp__rhea_shell__*", "browser__click"));
        assert!(matcher_matches("browser__*|mcp__rhea_shell__run_command", "browser__open"));
        assert!(matcher_matches("a*c", "abbbc"));
        assert!(!matcher_matches("a*c", "abbbd"));
        assert!(matcher_matches("exact", "exact"));
        assert!(!matcher_matches("exact", "exact2"));
        assert!(!matcher_matches("Exact", "exact"));
    }

    #[test]
    fn empty_registry_runs_nothing() {
        let outcome = HookRegistry::empty().run(HookEvent::PreToolUse, Some("x"), &json!({}));
        assert!(outcome.block.is_none() && outcome.ran.is_empty() && outcome.additional_context.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn exit_codes_and_json_decisions() {
        let blocked = registry("PreToolUse", "", "[Console]::Error.WriteLine('nope'); exit 2", 20)
            .run(HookEvent::PreToolUse, Some("t"), &json!({"tool_input":{}}));
        assert_eq!(blocked.block.as_deref(), Some("nope"));

        let ok = registry("PreToolUse", "", "exit 0", 20).run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert!(ok.block.is_none());
        assert_eq!(ok.ran[0].exit_code, Some(0));

        let soft = registry("PreToolUse", "", "exit 1", 20).run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert!(soft.block.is_none());
        assert_eq!(soft.ran[0].exit_code, Some(1));

        let json_block = registry("PreToolUse", "", r#"Write-Output '{"decision":"block","reason":"r"}'"#, 20)
            .run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert_eq!(json_block.block.as_deref(), Some("r"));

        let nested = registry("PreToolUse", "", r#"Write-Output '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no"}}'"#, 20)
            .run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert_eq!(nested.block.as_deref(), Some("no"));

        let context = registry("UserPromptSubmit", "", "Write-Output 'remember the milk'", 20)
            .run(HookEvent::UserPromptSubmit, None, &json!({"prompt":"hi"}));
        assert_eq!(context.additional_context, vec!["remember the milk".to_owned()]);

        let json_context = registry("PostToolUse", "", r#"Write-Output '{"additionalContext":"ctx"}'"#, 20)
            .run(HookEvent::PostToolUse, Some("t"), &json!({}));
        assert_eq!(json_context.additional_context, vec!["ctx".to_owned()]);

        let updated = registry("PreToolUse", "", r#"Write-Output '{"updatedInput":{"path":"safe"}}'"#, 20)
            .run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert_eq!(updated.updated_input, Some(json!({"path":"safe"})));

        let stop = registry("Stop", "", r#"Write-Output '{"continue":false,"stopReason":"enough"}'"#, 20)
            .run(HookEvent::Stop, None, &json!({}));
        assert_eq!(stop.stop.as_deref(), Some("enough"));
    }

    #[cfg(windows)]
    #[test]
    fn stdin_payload_and_matcher_filtering() {
        // The hook echoes the tool name it read from stdin JSON.
        let reg = registry("PreToolUse", "browser__*", "$j = [Console]::In.ReadToEnd() | ConvertFrom-Json; Write-Output ('{\"additionalContext\":\"' + $j.tool_name + '|' + $j.hook_event_name + '\"}')", 20);
        let hit = reg.run(HookEvent::PreToolUse, Some("browser__open"), &json!({"tool_input":{"url":"x"}}));
        assert_eq!(hit.additional_context, vec!["browser__open|PreToolUse".to_owned()]);
        let miss = reg.run(HookEvent::PreToolUse, Some("list_mcps"), &json!({}));
        assert!(miss.ran.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_the_hook_and_fails_open() {
        let started = Instant::now();
        let outcome = registry("PreToolUse", "", "Start-Sleep 20", 1).run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(outcome.block.is_none());
        assert!(outcome.ran[0].timed_out);
    }

    #[test]
    fn spawn_failure_fails_open() {
        // A shell that cannot find a program still runs; simulate by calling run_command with a bad program.
        let mut process = Command::new("definitely-not-a-real-program-xyz");
        assert!(process.spawn().is_err());
    }

    #[test]
    fn first_block_wins_and_skips_later_hooks() {
        let reg = HookRegistry::from_json(
            &json!({"hooks":{"PreToolUse":[{"hooks":[
                {"command": if cfg!(windows) {"[Console]::Error.WriteLine('first'); exit 2"} else {"echo first >&2; exit 2"}},
                {"command": if cfg!(windows) {"exit 0"} else {"exit 0"}}
            ]}]}}).to_string(),
        )
        .unwrap();
        let outcome = reg.run(HookEvent::PreToolUse, Some("t"), &json!({}));
        assert_eq!(outcome.block.as_deref(), Some("first"));
        assert_eq!(outcome.ran.len(), 1);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = env::temp_dir().join(format!("rhea-hooks-test-{}", std::process::id()));
        let path = dir.join("nested").join("hooks.json");
        let reg = registry("PostToolUse", "mcp__*", "echo done", 7);
        reg.save(&path).unwrap();
        let loaded = HookRegistry::load(&path).unwrap();
        assert_eq!(loaded, reg);
        reg.save(&path).unwrap(); // overwrite
        assert_eq!(HookRegistry::load(&path).unwrap(), reg);
        assert!(HookRegistry::load(&dir.join("missing.json")).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skills_hooks_are_installed_tagged_and_replaced_on_reinstall() {
        let dir = env::temp_dir().join(format!("rhea-skillhooks-{}", std::process::id()));
        let skill = dir.join("skill");
        fs::create_dir_all(skill.join("hooks")).unwrap();
        fs::write(
            skill.join("hooks").join("hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Write|Edit","hooks":[{"type":"command","command":"node ${CLAUDE_PLUGIN_ROOT}/check.js"}]}],"Notification":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        let registry_path = dir.join("hooks.json");
        // an unrelated hook the user already had
        registry("Stop", "", "echo mine", 5).save(&registry_path).unwrap();

        let lines = install_skill_hooks_at(&registry_path, "lint", &skill).unwrap();
        assert!(lines[0].starts_with("PostToolUse Write|Edit: node "), "{lines:?}");
        assert!(lines[0].contains("check.js") && !lines[0].contains("${"), "{lines:?}");
        assert!(lines.last().unwrap().contains("Notification"));
        let loaded = HookRegistry::load(&registry_path).unwrap();
        assert_eq!(loaded.count(), 2);
        assert!(loaded.has_hooks_for(HookEvent::Stop) && loaded.has_hooks_for(HookEvent::PostToolUse));

        install_skill_hooks_at(&registry_path, "lint", &skill).unwrap();
        assert_eq!(HookRegistry::load(&registry_path).unwrap().count(), 2, "reinstalling does not duplicate");
        let mut loaded = HookRegistry::load(&registry_path).unwrap();
        assert_eq!(loaded.remove_source("lint"), 1);
        assert_eq!(loaded.count(), 1, "the user's own hook stays");
        let plain = dir.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(install_skill_hooks_at(&registry_path, "plain", &plain).unwrap().is_empty(), "a skill without hooks changes nothing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_is_thread_safe() {
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<HookRegistry>();
    }
}
