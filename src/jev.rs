use std::{env, fs, path::PathBuf, process::Command};

use serde_json::{Value, json};

use crate::desktop;

use std::sync::atomic::{AtomicU64, Ordering};

/// Every cancel bumps this counter; a worker thread compares it with the value it
/// saw when its run began, so cancelling one run can never affect the next.
static CANCEL_GENERATION: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static RUN_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static STOP_FLAG: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>> = const { std::cell::RefCell::new(None) };
}

/// Give this worker thread its own stop switch (used for individual subagents) in
/// addition to the global cancel that stops everything.
pub fn set_run_stop_flag(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    STOP_FLAG.with(|slot| *slot.borrow_mut() = Some(flag));
}

/// Call on the worker thread when a chat run starts.
pub fn begin_run() {
    RUN_GENERATION.with(|run| run.set(CANCEL_GENERATION.load(Ordering::SeqCst)));
}
/// Ask the running chat turn and its JEV plan to stop at the next safe point.
pub fn request_cancel() {
    CANCEL_GENERATION.fetch_add(1, Ordering::SeqCst);
}
pub fn cancel_requested() -> bool {
    RUN_GENERATION.with(|run| run.get()) != CANCEL_GENERATION.load(Ordering::SeqCst)
        || STOP_FLAG.with(|slot| slot.borrow().as_ref().is_some_and(|flag| flag.load(Ordering::SeqCst)))
}

pub fn execute(name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "list_processes" => list_processes(),
        "open_app" => open_app(required_string(arguments, "app")?),
        "desktop_list_apps" => desktop::list_apps(),
        "desktop_list_windows" => desktop::list_windows(optional_string(arguments, "app")?),
        "desktop_launch" => open_app(required_string(arguments, "app")?),
        "desktop_focus_window" => focus_window_and_observe(
            optional_string(arguments, "window_id")?,
            optional_string(arguments, "app")?,
            optional_string(arguments, "title")?,
        ),
        "desktop_snapshot" => {
            let surface = optional_string(arguments, "surface")?.unwrap_or("focused");
            let interactive = arguments
                .get("interactive_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let compact = optional_bool(arguments, "compact")?.unwrap_or(false);
            let skeleton = optional_bool(arguments, "skeleton")?.unwrap_or(false);
            let max_depth = optional_u32(arguments, "max_depth")?
                .unwrap_or(desktop::DEFAULT_SNAPSHOT_MAX_DEPTH as u32);
            let filter = optional_string(arguments, "filter")?.unwrap_or("all");
            let timeout_ms = optional_u32(arguments, "timeout_ms")?
                .unwrap_or(desktop::DEFAULT_SNAPSHOT_TIMEOUT_MS);
            let target_text = optional_string(arguments, "target_text")?;
            let window_id = optional_string(arguments, "window_id")?;
            let app = optional_string(arguments, "app")?;
            let max_depth = u8::try_from(max_depth)
                .map_err(|_| "Tool argument 'max_depth' must be between 1 and 32.".to_owned())?;
            let result = desktop::snapshot_target_options(
                surface,
                app,
                window_id,
                interactive,
                compact,
                skeleton,
                max_depth,
                timeout_ms,
                false,
            )?;
            desktop::snapshot_summary_for_target(&result.json, filter, target_text)
        }
        "desktop_find" => desktop::find(
            optional_string(arguments, "app")?,
            optional_string(arguments, "window_id")?,
            optional_string(arguments, "text")?,
            optional_string(arguments, "name")?,
            optional_string(arguments, "role")?,
        ),
        "desktop_get" => desktop::get(
            required_string(arguments, "ref")?,
            optional_string(arguments, "property")?.unwrap_or("text"),
        ),
        "desktop_click" => desktop::click_and_refresh(required_string(arguments, "ref")?),
        "desktop_type" => desktop::type_and_refresh(
            required_string(arguments, "snapshot_id")?,
            required_string(arguments, "ref")?,
            required_string(arguments, "text")?,
        ),
        "jev_run_plan" => run_plan(arguments),
        "desktop_send" => desktop::send_and_refresh(
            required_string(arguments, "snapshot_id")?,
            required_string(arguments, "ref")?,
            required_string(arguments, "text")?,
        ),
        "desktop_press" => desktop::press_from_snapshot(
            required_string(arguments, "snapshot_id")?,
            required_string(arguments, "combo")?,
        ),
        "desktop_mouse_move" => desktop::mouse_move_from_snapshot(
            required_string(arguments, "snapshot_id")?,
            required_i64(arguments, "x")?,
            required_i64(arguments, "y")?,
        ),
        "desktop_mouse_click" => desktop::mouse_click_from_snapshot(
            required_string(arguments, "snapshot_id")?,
            required_i64(arguments, "x")?,
            required_i64(arguments, "y")?,
        ),
        "desktop_hover" => desktop::hover_and_refresh(required_string(arguments, "ref")?),
        "desktop_scroll" => desktop::scroll_and_refresh(
            required_string(arguments, "ref")?,
            required_string(arguments, "direction")?,
            required_u32(arguments, "amount")?,
        ),
        "desktop_wait" => desktop::wait_for_text(
            required_string(arguments, "snapshot_id")?,
            required_string(arguments, "text")?,
            required_u64(arguments, "timeout_ms")?,
        ),
        "desktop_screenshot" => desktop::screenshot(
            optional_string(arguments, "app")?,
            optional_string(arguments, "window_id")?,
        ),
        "inspect_desktop" => {
            let surface = required_string(arguments, "surface")?;
            if !["focused", "window", "desktop", "start-menu", "taskbar"].contains(&surface) {
                return Err("Unsupported desktop surface.".into());
            }
            let app = arguments
                .get("app")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let snapshot = desktop::snapshot(surface, app, false)?;
            Ok(desktop::snapshot_summary(&snapshot.json))
        }
        "click_ui_control" => {
            let reference = required_string(arguments, "ref")?;
            desktop::click_and_refresh(reference)
        }
        "type_text" => {
            let text = required_string(arguments, "text")?;
            if text.chars().count() > 2000 {
                return Err("JEV limits one typing action to 2,000 characters.".into());
            }
            type_text(text)?;
            Ok(json!({"typed_characters": text.chars().count()}))
        }
        "press_key" => {
            let key = required_string(arguments, "key")?;
            press_key(key)?;
            Ok(json!({"pressed": key}))
        }
        _ => Err(format!("JEV does not provide a tool named '{name}'.")),
    }
}

#[cfg(windows)]
pub(crate) fn type_text(text: &str) -> Result<(), String> {
    use std::mem::size_of;
    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyboardInput {
        vk: u16,
        scan: u16,
        flags: u32,
        time: u32,
        extra_info: usize,
    }
    #[repr(C)]
    union InputData {
        keyboard: KeyboardInput,
        padding: [usize; 4],
    }
    #[repr(C)]
    struct Input {
        kind: u32,
        data: InputData,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    }

    let units: Vec<u16> = text.encode_utf16().collect();
    let mut inputs = Vec::with_capacity(units.len() * 2);
    for unit in units {
        inputs.push(Input {
            kind: INPUT_KEYBOARD,
            data: InputData {
                keyboard: KeyboardInput {
                    vk: 0,
                    scan: unit,
                    flags: KEYEVENTF_UNICODE,
                    time: 0,
                    extra_info: 0,
                },
            },
        });
        inputs.push(Input {
            kind: INPUT_KEYBOARD,
            data: InputData {
                keyboard: KeyboardInput {
                    vk: 0,
                    scan: unit,
                    flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    extra_info: 0,
                },
            },
        });
    }
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<Input>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        return Err(format!(
            "Windows accepted {sent} of {} keyboard events.",
            inputs.len()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn type_text(text: &str) -> Result<(), String> {
    crate::macos::type_text(text)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn type_text(_text: &str) -> Result<(), String> {
    Err("JEV keyboard typing is available on Windows and macOS only.".into())
}

/// Press a key combination ("ENTER", "ctrl+k", "ctrl+shift+tab", "f5").
pub(crate) fn press_key(key: &str) -> Result<(), String> {
    crate::uia::press_combo(key)
}

#[cfg(not(windows))]
fn list_processes() -> Result<Value, String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,rss=,comm="])
        .output()
        .map_err(|e| format!("Could not list processes: {e}"))?;
    if !output.status.success() {
        return Err(format!("ps failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let processes: Vec<Value> = String::from_utf8_lossy(&output.stdout).lines().filter_map(parse_ps_row).take(300).collect();
    Ok(json!({ "count": processes.len(), "processes": processes }))
}

/// One `ps -axo pid=,rss=,comm=` line: "  123 45678 /Applications/Foo.app/Contents/MacOS/Foo".
#[cfg(not(windows))]
fn parse_ps_row(line: &str) -> Option<Value> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let rss_kb = parts.next()?.parse::<u64>().ok()?;
    let path = parts.collect::<Vec<_>>().join(" ");
    if path.is_empty() {
        return None;
    }
    let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    Some(json!({"image_name": name, "pid": pid, "memory": format!("{rss_kb} K"), "path": path}))
}

#[cfg(windows)]
fn list_processes() -> Result<Value, String> {
    let mut command = Command::new("tasklist.exe");
    command.args(["/FO", "CSV", "/NH"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command
        .output()
        .map_err(|e| format!("Could not list Windows processes: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tasklist failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let processes: Vec<Value> = stdout
        .lines()
        .filter_map(parse_tasklist_row)
        .take(200)
        .collect();
    Ok(json!({ "count": processes.len(), "processes": processes }))
}

fn parse_tasklist_row(line: &str) -> Option<Value> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.trim().chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    if !current.is_empty() || !fields.is_empty() {
        fields.push(current);
    }
    if fields.len() < 5 {
        return None;
    }
    Some(json!({
        "image_name": fields[0],
        "pid": fields[1].parse::<u32>().ok(),
        "session_name": fields[2],
        "session_id": fields[3].parse::<u32>().ok(),
        "memory": fields[4],
    }))
}

pub fn open_app(requested: &str) -> Result<Value, String> {
    let app = requested.trim();
    if app.is_empty() || app.chars().count() > 200 {
        return Err("Choose an app name under 200 characters.".into());
    }
    let running = desktop::list_apps()?;
    let (already_running, result) = if json_contains_app(&running, app) {
        (true, desktop::focus_window(None, Some(app), None)?)
    } else {
        let launch_result = match desktop::launch(app) {
            Ok(result) => result,
            Err(error) if error.contains("APP_NOT_FOUND") => {
                #[cfg(windows)]
                {
                    let shortcut = find_start_menu_shortcut(app)?
                .ok_or_else(|| format!("Agent Desktop could not find '{app}', and no matching Start menu shortcut was found."))?;
                    shell_open(&shortcut.to_string_lossy())?;
                    json!({"shortcut":shortcut,"launch_method":"Start menu shortcut"})
                }
                #[cfg(target_os = "macos")]
                {
                    // Agent Desktop does not know the app; let macOS look it up by name.
                    let opened = Command::new("open").arg("-a").arg(app).status();
                    if !opened.is_ok_and(|status| status.success()) {
                        return Err(format!("macOS could not find an app called '{app}'."));
                    }
                    json!({"launch_method": "open -a"})
                }
                #[cfg(not(any(windows, target_os = "macos")))]
                {
                    return Err(format!(
                        "Agent Desktop could not find or launch '{app}': {}",
                        error.replace("APP_NOT_FOUND", "app not found")
                    ));
                }
            }
            Err(error) => return Err(error),
        };
        (false, launch_result)
    };
    let observation = desktop::snapshot_target("window", Some(app), None, false, false)
        .map(|snapshot| desktop::snapshot_summary(&snapshot.json))
        .unwrap_or_else(|error| json!({"kind":"desktop_snapshot","ok":false,"error":error}));
    Ok(
        json!({"app":app,"already_running":already_running,"result":result,"fresh_observation":observation}),
    )
}

/// Execute a ChatGPT-authored plan end to end on this PC. ChatGPT decides the
/// steps and the exact text once; JEV resolves controls by label, acts, and
/// moves on without a model round trip. The first step that cannot be
/// confirmed stops the plan and returns the current screen so ChatGPT can
/// re-plan from there. Nothing after a failed step runs.
pub fn run_plan(arguments: &Value) -> Result<Value, String> {
    use crate::uia::{self, Button};
    let steps = arguments
        .get("steps")
        .and_then(Value::as_array)
        .ok_or("A plan needs a 'steps' array.")?;
    if steps.is_empty() || steps.len() > 25 {
        return Err("A plan must have 1 to 25 steps.".into());
    }
    if arguments.get("final").and_then(Value::as_bool) == Some(true) && steps.len() != 1 {
        return Err("A multi-step plan must return its results to ChatGPT for supervision; set final=false.".into());
    }
    let started = std::time::Instant::now();
    let mut app: Option<String> = None;
    let mut done = Vec::new();
    let mut sent = false;
    let mut unverified_send = false;
    for (index, step) in steps.iter().enumerate() {
        if cancel_requested() {
            return Err("Run cancelled.".into());
        }
        let step_started = std::time::Instant::now();
        let mut step_detail: Option<Value> = None;
        let action = step.get("do").and_then(Value::as_str).unwrap_or_default();
        let field = |key: &str| {
            step.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let int = |key: &str| step.get(key).and_then(Value::as_i64);
        if let Some(step_app) = field("app") {
            app = Some(step_app.to_owned());
        }
        let outcome: Result<(), String> = (|| {
            let current_app = || {
                app.clone()
                    .ok_or_else(|| "Name the app in this step or an earlier one.".to_owned())
            };
            // A screen point from x/y, or from a fuzzy-matched visible label.
            let mut point = |target: Option<&str>,
                             within: Option<&str>,
                             x: Option<i64>,
                             y: Option<i64>|
             -> Result<(i32, i32), String> {
                if let (Some(x), Some(y)) = (x, y) {
                    return Ok((x as i32, y as i32));
                }
                let target = target.ok_or("Give a target label or x and y.")?;
                let found = desktop::locate(&current_app()?, target, within, false)?;
                let entry = json!({"matched":found.label,"role":found.role,"score":found.score,"x":found.x,"y":found.y});
                step_detail = Some(match step_detail.take() {
                    Some(Value::Object(mut map)) if map.contains_key("matched") => {
                        map.insert("to".into(), entry);
                        Value::Object(map)
                    }
                    _ => entry,
                });
                Ok((found.x, found.y))
            };
            match action {
                // A command-line step inside a plan: Shell and the GUI share one plan, so
                // "install it, open it, click through it" costs a single model round trip.
                "shell" => {
                    let command = field("text").ok_or("shell needs text = the command to run")?;
                    let directory = field("target")
                        .map(PathBuf::from)
                        .or_else(|| env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from))
                        .ok_or("shell needs a working directory in target")?;
                    let detach = field("keys") == Some("detach");
                    let timeout = int("ms").filter(|ms| *ms > 0).unwrap_or(120_000);
                    let result = crate::shell::execute(
                        "mcp__jarvus_shell__run_command",
                        &json!({"command": command, "working_directory": directory.to_string_lossy(),
                                "timeout_ms": timeout, "shell": null, "detach": detach}),
                    )?;
                    let clip = |value: &Value| value.as_str().unwrap_or("").chars().take(1500).collect::<String>();
                    step_detail = Some(json!({
                        "exit_code": result["exit_code"], "duration_ms": result["duration_ms"], "pid": result["pid"],
                        "stdout": clip(&result["stdout"]), "stderr": clip(&result["stderr"]),
                    }));
                    if result["success"].as_bool().unwrap_or(false) {
                        Ok(())
                    } else {
                        Err(format!("command failed (exit {}): {}", result["exit_code"], clip(&result["stderr"])))
                    }
                }
                "open_app" => ensure_app_focused(&current_app()?),
                "click" | "double_click" | "right_click" => {
                    bring_forward(&current_app()?)?;
                    let button = if action == "right_click" {
                        Button::Right
                    } else {
                        Button::parse(field("button"))?
                    };
                    let count = if action == "double_click" {
                        2
                    } else {
                        int("count").unwrap_or(1).clamp(1, 3) as u32
                    };
                    let (x, y) = point(field("target"), field("within"), int("x"), int("y"))?;
                    uia::click(x, y, button, count, false)
                }
                "move" => {
                    bring_forward(&current_app()?)?;
                    let (x, y) = point(field("target"), field("within"), int("x"), int("y"))?;
                    uia::move_smooth(x, y)
                }
                "drag" => {
                    bring_forward(&current_app()?)?;
                    let from = point(field("target"), field("within"), int("x"), int("y"))?;
                    let to = point(field("to"), field("within"), int("to_x"), int("to_y"))?;
                    uia::drag(from, to, Button::parse(field("button"))?)
                }
                "hold" => {
                    bring_forward(&current_app()?)?;
                    let (x, y) = point(field("target"), field("within"), int("x"), int("y"))?;
                    let button = Button::parse(field("button"))?;
                    uia::move_smooth(x, y)?;
                    uia::button(button, true)?;
                    std::thread::sleep(std::time::Duration::from_millis(
                        int("ms").unwrap_or(800).clamp(50, 10_000) as u64,
                    ));
                    uia::button(button, false)
                }
                "scroll" => {
                    bring_forward(&current_app()?)?;
                    let (x, y) = match (field("target"), int("x"), int("y")) {
                        (None, None, None) => uia::cursor(),
                        (target, x, y) => point(target, field("within"), x, y)?,
                    };
                    uia::wheel(x, y, int("amount").unwrap_or(-3) as i32)
                }
                "type" | "send" => {
                    let app_name = current_app()?;
                    let text = step
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("type needs text")?;
                    match native_type(&app_name, field("target"), text, action == "send") {
                        NativeType::Done { submitted } => {
                            if submitted {
                                sent = true;
                            }
                            return Ok(());
                        }
                        NativeType::Failed { reason, submitted } => {
                            if submitted {
                                sent = true;
                            }
                            return Err(reason);
                        }
                        NativeType::Fallback => {}
                    }
                    let target =
                        desktop::resolve_control(&app_name, field("target").unwrap_or(""), true)?;
                    desktop::plan_type(&target, text)?;
                    if action == "send" {
                        sent = true; // Return is attempted at most once from here on.
                        uia::press_combo("enter")?;
                    }
                    Ok(())
                }
                "press" => {
                    bring_forward(&current_app()?)?;
                    uia::press_combo(field("keys").ok_or("press needs keys")?)
                }
                "wait" => match field("text").or(field("target")) {
                    Some(label) => wait_for_label(
                        &current_app()?,
                        label,
                        field("within"),
                        int("ms").unwrap_or(8_000),
                    ),
                    None => {
                        std::thread::sleep(std::time::Duration::from_millis(
                            int("ms").unwrap_or(250).clamp(0, 10_000) as u64,
                        ));
                        Ok(())
                    }
                },
                "message" => {
                    let app_name = current_app()?;
                    let recipient = field("recipient").ok_or("message needs a recipient")?;
                    let text = step
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("message needs text")?;
                    if text.trim().is_empty() || text.chars().count() > 2000 {
                        return Err("Message text must be 1 to 2,000 characters.".into());
                    }
                    // Native path first (milliseconds); agent-desktop only when the
                    // native path could not start and nothing was typed.
                    let result = match message_native(&app_name, recipient, text) {
                        NativeMessage::Done(result) => result,
                        NativeMessage::Fallback(first_reason) => {
                            ensure_app_focused(&app_name)?;
                            match message_native(&app_name, recipient, text) {
                                NativeMessage::Done(result) => result,
                                NativeMessage::Fallback(reason) => {
                                    let mut result =
                                        desktop::message_conversation(&app_name, recipient, text)?;
                                    if let Some(object) = result.as_object_mut() {
                                        object.insert(
                                            "native_fallback_reason".into(),
                                            json!([first_reason, reason]),
                                        );
                                    }
                                    result
                                }
                            }
                        }
                    };
                    step_detail = Some(result.clone());
                    let status = result
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    if result.get("submit_attempts").and_then(Value::as_u64) != Some(0) {
                        sent = true;
                    }
                    match status {
                        "sent_verified" => Ok(()),
                        "submitted_unverified" => {
                            unverified_send = true;
                            Ok(())
                        }
                        _ => Err(result
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or(status)
                            .to_owned()),
                    }
                }
                web if web.starts_with("web_") => {
                    // Chrome via the JARVUS extension: same plan, no model round trips.
                    let mut params = serde_json::Map::new();
                    for (key, value) in [
                        ("target", field("target")),
                        ("within", field("within")),
                        ("keys", field("keys")),
                    ] {
                        if let Some(value) = value {
                            params.insert(key.into(), Value::from(value));
                        }
                    }
                    if let Some(tab) = field("app").filter(|app| {
                        !app.eq_ignore_ascii_case("chrome")
                            && !app.eq_ignore_ascii_case("google chrome")
                    }) {
                        params.insert("tab".into(), Value::from(tab));
                    }
                    let text = step.get("text").and_then(Value::as_str).map(str::to_owned);
                    let (method, extra): (&str, Vec<(&str, Value)>) = match web {
                        "web_open" => (
                            "open",
                            vec![(
                                "url",
                                Value::from(
                                    text.clone()
                                        .or(field("target").map(str::to_owned))
                                        .ok_or("web_open needs text = url")?,
                                ),
                            )],
                        ),
                        "web_tab" => (
                            "activate",
                            vec![(
                                "tab",
                                Value::from(
                                    field("target")
                                        .ok_or("web_tab needs target = tab title/url words")?,
                                ),
                            )],
                        ),
                        "web_click" => ("click", vec![]),
                        "web_type" => (
                            "type",
                            vec![(
                                "text",
                                Value::from(text.clone().ok_or("web_type needs text")?),
                            )],
                        ),
                        "web_send" => (
                            "type",
                            vec![
                                (
                                    "text",
                                    Value::from(text.clone().ok_or("web_send needs text")?),
                                ),
                                ("submit", Value::Bool(true)),
                            ],
                        ),
                        "web_select" => (
                            "select",
                            vec![(
                                "option",
                                Value::from(text.clone().ok_or("web_select needs text = option")?),
                            )],
                        ),
                        "web_press" => ("press", vec![]),
                        "web_wait" => (
                            "wait",
                            vec![
                                ("text", text.clone().map(Value::from).unwrap_or(Value::Null)),
                                ("timeout_ms", Value::from(int("ms").unwrap_or(8_000))),
                            ],
                        ),
                        "web_scroll" => (
                            "scroll",
                            vec![
                                (
                                    "amount",
                                    Value::from(int("amount").unwrap_or(-3).unsigned_abs()),
                                ),
                                (
                                    "direction",
                                    Value::from(if int("amount").unwrap_or(-3) > 0 {
                                        "up"
                                    } else {
                                        "down"
                                    }),
                                ),
                            ],
                        ),
                        other => return Err(format!("Unknown web step '{other}'.")),
                    };
                    for (key, value) in extra {
                        if !value.is_null() {
                            params.insert(key.into(), value);
                        }
                    }
                    if method == "activate" {
                        params.remove("target");
                    }
                    if method == "open" {
                        params.remove("target");
                        params.remove("tab");
                    }
                    if web == "web_send" {
                        sent = true;
                    }
                    let result =
                        crate::browser::execute_browser_command(method, Value::Object(params))
                            .map_err(|error| error.to_string())?;
                    step_detail = Some(result);
                    Ok(())
                }
                other => Err(format!("Unknown step '{other}'.")),
            }
        })();
        let ms = step_started.elapsed().as_millis() as u64;
        match outcome {
            Ok(()) => done.push(json!({"step":index + 1,"do":action,"ms":ms,"detail":step_detail})),
            Err(reason) => {
                let state = app
                    .as_deref()
                    .map(|app| desktop::native_state(app, field("target").or(field("recipient"))));
                return Ok(json!({
                    "kind":"jev_plan","status":"stopped","completed_steps":done,
                    "failed_step":{"step":index + 1,"do":action,"reason":reason,"ms":ms,"detail":step_detail},
                    "message_sent":sent,"total_ms":started.elapsed().as_millis() as u64,"state":state,
                    "next": if sent { "A message may already have been sent; do not send it again. Report the status." }
                            else { "Nothing was sent. Send a corrected plan that continues from this state." }
                }));
            }
        }
    }
    Ok(
        json!({"kind":"jev_plan","status": if unverified_send { "completed_unverified" } else { "completed" },
        "completed_steps":done,"message_sent":sent,"total_ms":started.elapsed().as_millis() as u64}),
    )
}

/// Make `app` the foreground window before pointer/keyboard input; another
/// app (e.g. an overlay) may have taken focus since the previous step.
fn bring_forward(app: &str) -> Result<(), String> {
    use crate::uia;
    if uia::app_is_foreground(app) || uia::focus_app_window(app).is_ok() {
        return Ok(());
    }
    let _ = desktop::focus_window(None, Some(app), None);
    if uia::app_is_foreground(app) {
        Ok(())
    } else {
        Err(format!("Could not bring {app} to the front."))
    }
}

/// Wait until a label matching `label` is visible in `app` (native tree polls).
fn wait_for_label(
    app: &str,
    label: &str,
    within: Option<&str>,
    timeout_ms: i64,
) -> Result<(), String> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(timeout_ms.clamp(100, 30_000) as u64);
    loop {
        if let Ok(tree) = crate::uia::window_tree(app) {
            let window = tree.get("bounds").and_then(desktop::parse_bounds_pub);
            if desktop::locate_in_tree(&tree, window, label, within, false).is_some() {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("'{label}' did not appear in time."));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

enum NativeType {
    Done { submitted: bool },
    Failed { reason: String, submitted: bool },
    Fallback,
}

/// Native type/send step: bring the app forward, focus the text field named
/// `target` (placeholder/label, or the place it belongs to: "chat" matches
/// "Message #chat"; best fuzzy match wins) by clicking it, or use the focused
/// field when no target is given; type with SendInput; confirm the text landed;
/// for send, press Return once. `Fallback` means nothing was typed.
fn native_type(app: &str, target: Option<&str>, text: &str, submit: bool) -> NativeType {
    use crate::uia;
    use std::time::Duration;
    if text.chars().count() > 2000 {
        return NativeType::Failed {
            reason: "JEV limits one typing step to 2,000 characters.".into(),
            submitted: false,
        };
    }
    if bring_forward(app).is_err() {
        return NativeType::Fallback;
    }
    let clean = |value: &str| {
        value
            .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
            .to_owned()
    };
    let focus_field = || match target.filter(|wanted| !desktop::label_words(wanted).is_empty()) {
        Some(wanted) => {
            uia::focused_control().is_ok_and(|control| {
                control.is_text_input && desktop::label_score(wanted, &control.name, false) > 0
            }) || uia::focus_text_field(&|name: &str| desktop::label_score(wanted, name, false))
                .unwrap_or(false)
        }
        None => {
            uia::focused_control().is_ok_and(|control| control.is_text_input)
                || uia::focus_text_field(&|_: &str| 1).unwrap_or(false)
        }
    };
    if !focus_field() {
        return NativeType::Fallback;
    }
    let field_value = || {
        uia::focused_control()
            .ok()
            .filter(|control| control.is_text_input)
            .and_then(|control| control.value)
    };
    if submit && field_value().is_some_and(|value| !clean(&value).is_empty()) {
        return NativeType::Failed {
            reason: "The text field already contains a draft; nothing was typed or sent.".into(),
            submitted: false,
        };
    }
    // Focus can be stolen between checks; confirm once more right before typing.
    if !uia::app_is_foreground(app) && (bring_forward(app).is_err() || !focus_field()) {
        return NativeType::Fallback;
    }
    if let Err(reason) = type_text(text) {
        return NativeType::Failed {
            reason,
            submitted: false,
        };
    }
    let expected = desktop::normalize_label(text);
    let mut typed = false;
    for _ in 0..30 {
        if field_value().is_some_and(|value| desktop::normalize_label(&value).contains(&expected)) {
            typed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    if !typed {
        return NativeType::Failed {
            reason: "The typed text was not confirmed in the field; Return was not pressed.".into(),
            submitted: false,
        };
    }
    if !submit {
        return NativeType::Done { submitted: false };
    }
    if !uia::app_is_foreground(app) {
        return NativeType::Failed {
            reason: "Another window took focus after typing; Return was not pressed.".into(),
            submitted: false,
        };
    }
    match uia::press_combo("enter") {
        Ok(()) => NativeType::Done { submitted: true },
        Err(reason) => NativeType::Failed {
            reason,
            submitted: true,
        },
    }
}

enum NativeMessage {
    Done(Value),
    Fallback(String),
}

/// Native fast path for a message step: open the conversation (reuse the open
/// one, else the app's quick switcher), confirm its text field names the
/// recipient, type, confirm, press Return once, confirm the field cleared.
/// Every keystroke burst is preceded by a foreground check, because another
/// window can take focus at any time. `Fallback` only before anything was typed.
fn message_native(app: &str, recipient: &str, text: &str) -> NativeMessage {
    use crate::uia;
    use std::time::{Duration, Instant};
    let started = Instant::now();
    let mut timings = serde_json::Map::new();
    let mut lap = Instant::now();
    let mut stage = |timings: &mut serde_json::Map<String, Value>, name: &str| {
        timings.insert(name.into(), Value::from(lap.elapsed().as_millis() as u64));
        lap = Instant::now();
    };
    let until = |timeout_ms: u64, every_ms: u64, check: &mut dyn FnMut() -> bool| -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if check() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(every_ms));
        }
    };
    // Whole-word fuzzy match on the conversation's text field ("Message @My
    // goddess"): case/symbols ignored, but a different person never matches.
    let recipient_score = |name: &str| desktop::label_score(recipient, name, true);
    let is_composer =
        |control: &uia::FocusedControl| control.is_text_input && recipient_score(&control.name) > 0;
    let composer_focused = || uia::focused_control().is_ok_and(|control| is_composer(&control));
    let focus_composer = || {
        if !uia::app_is_foreground(app) && bring_forward(app).is_err() {
            return false;
        }
        composer_focused() || uia::focus_text_field(&recipient_score).unwrap_or(false)
    };
    let clean = |value: &str| {
        value
            .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
            .to_owned()
    };
    let stop = |status: &str, reason: &str, timings: serde_json::Map<String, Value>| {
        NativeMessage::Done(json!({
        "kind":"desktop_message","status":status,"reason":reason,"submit_attempts":0,
        "timings_ms":timings,"total_ms":started.elapsed().as_millis() as u64,"path":"native"}))
    };

    if let Err(error) = bring_forward(app) {
        return NativeMessage::Fallback(error);
    }
    stage(&mut timings, "focus_app");
    let was_open = focus_composer();
    stage(&mut timings, "check_open_conversation");
    if !was_open {
        // Open it with the quick switcher: Ctrl+K, type the name, wait for the
        // results to load, pick the top one. Only the name is typed here.
        if uia::press_combo("ctrl+k").is_err() {
            return NativeMessage::Fallback("Ctrl+K was not delivered.".into());
        }
        let switcher = until(1_500, 40, &mut || {
            if !uia::app_is_foreground(app) {
                let _ = bring_forward(app);
            }
            uia::focused_control()
                .is_ok_and(|control| control.is_text_input && !is_composer(&control))
        });
        if !switcher {
            let _ = uia::press_combo("esc");
            return NativeMessage::Fallback(
                "The quick switcher did not open a focused search box.".into(),
            );
        }
        if type_text(recipient).is_err() {
            let _ = uia::press_combo("esc");
            return NativeMessage::Fallback("Could not type into the quick switcher.".into());
        }
        let wanted = desktop::normalize_label(recipient);
        let typed = until(1_000, 30, &mut || {
            uia::focused_control()
                .ok()
                .and_then(|control| control.value)
                .is_some_and(|value| desktop::normalize_label(&value).contains(&wanted))
        });
        if !typed {
            let _ = uia::press_combo("esc");
            return NativeMessage::Fallback("The quick switcher did not receive the name.".into());
        }
        std::thread::sleep(Duration::from_millis(250)); // let the results list load
        if !uia::app_is_foreground(app) {
            let _ = uia::press_combo("esc");
            return NativeMessage::Fallback("Focus moved away from the quick switcher.".into());
        }
        if uia::press_combo("enter").is_err() {
            return NativeMessage::Fallback("Could not choose the quick switcher result.".into());
        }
        let opened = until(3_000, 100, &mut || focus_composer());
        stage(&mut timings, "quick_switcher");
        if !opened {
            return stop(
                "conversation_not_opened",
                &format!(
                    "No conversation matching '{recipient}' opened (window: '{}'). Nothing was typed or sent.",
                    uia::foreground_title()
                ),
                timings,
            );
        }
    }
    let draft = uia::focused_control()
        .ok()
        .and_then(|control| control.value)
        .map(|value| clean(&value))
        .unwrap_or_default();
    if !draft.is_empty() {
        return stop(
            "draft_present",
            "The message box already contains a draft; nothing was typed or sent.",
            timings,
        );
    }
    if !focus_composer() {
        return stop(
            "composer_lost_focus",
            "The message box lost focus before typing; nothing was typed or sent.",
            timings,
        );
    }
    if let Err(error) = type_text(text) {
        return stop("not_typed", &error, timings);
    }
    let expected = desktop::normalize_label(text);
    let typed = until(900, 30, &mut || {
        uia::focused_control()
            .ok()
            .filter(|control| is_composer(control))
            .and_then(|control| control.value)
            .is_some_and(|value| desktop::normalize_label(&value).contains(&expected))
    });
    stage(&mut timings, "type");
    if !typed {
        return stop(
            "not_typed",
            "The typed text was not confirmed in the message box; Return was not pressed.",
            timings,
        );
    }
    if !uia::app_is_foreground(app) {
        return stop(
            "focus_stolen",
            "Another window took focus after typing; Return was not pressed.",
            timings,
        );
    }
    if let Err(error) = uia::press_combo("enter") {
        return stop("submit_failed_or_uncertain", &error, timings);
    }
    let cleared = until(1_600, 40, &mut || {
        uia::focused_control()
            .ok()
            .filter(|control| is_composer(control))
            .and_then(|control| control.value)
            .is_some_and(|value| clean(&value).is_empty())
    });
    stage(&mut timings, "submit_and_verify");
    NativeMessage::Done(json!({
        "kind":"desktop_message",
        "status": if cleared { "sent_verified" } else { "submitted_unverified" },
        "submit_attempts":1,"path":"native","conversation_was_open":was_open,
        "timings_ms":timings,"total_ms":started.elapsed().as_millis() as u64
    }))
}

/// Focus `app`, launching it first when it has no window. All checks are
/// native, and a new window is picked up as soon as it appears.
fn ensure_app_focused(app: &str) -> Result<(), String> {
    use crate::uia;
    if uia::focus_app_window(app).is_ok() {
        return Ok(());
    }
    if !uia::app_has_window(app) {
        match find_start_menu_shortcut(app)? {
            Some(shortcut) => shell_open(&shortcut.to_string_lossy())?,
            None => {
                desktop::launch(app)?;
            }
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    while std::time::Instant::now() < deadline {
        if uia::focus_app_window(app).is_ok() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("{app} did not open a window within 12 seconds."))
}

fn focus_window_and_observe(
    window_id: Option<&str>,
    app: Option<&str>,
    title: Option<&str>,
) -> Result<Value, String> {
    let focus = desktop::focus_window(window_id, app, title)?;
    let observation = desktop::snapshot_target("window", app, window_id, false, false)
        .or_else(|_| desktop::snapshot_target("focused", None, None, false, false))
        .map(|snapshot| desktop::snapshot_summary(&snapshot.json))
        .unwrap_or_else(|error| json!({"kind":"desktop_snapshot","ok":false,"error":error}));
    Ok(
        json!({"action":"focus_window","focus_confirmed":focus.get("ok").and_then(Value::as_bool).unwrap_or(false),"fresh_observation":observation}),
    )
}

fn json_contains_app(value: &Value, expected: &str) -> bool {
    let expected = expected.trim().to_ascii_lowercase();
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            if ["name", "app", "process", "process_name", "title"].contains(&key.as_str()) {
                value
                    .as_str()
                    .is_some_and(|found| found.to_ascii_lowercase().contains(&expected))
            } else {
                json_contains_app(value, &expected)
            }
        }),
        Value::Array(items) => items.iter().any(|item| json_contains_app(item, &expected)),
        _ => false,
    }
}

fn find_start_menu_shortcut(requested: &str) -> Result<Option<PathBuf>, String> {
    let mut roots = Vec::new();
    if let Some(app_data) = env::var_os("APPDATA") {
        roots.push(PathBuf::from(app_data).join("Microsoft/Windows/Start Menu/Programs"));
    }
    if let Some(program_data) = env::var_os("PROGRAMDATA") {
        roots.push(PathBuf::from(program_data).join("Microsoft/Windows/Start Menu/Programs"));
    }
    let wanted = requested.trim().to_ascii_lowercase();
    let mut exact = Vec::new();
    let mut partial = Vec::new();
    for root in roots.into_iter().filter(|root| root.is_dir()) {
        let mut pending = vec![root];
        while let Some(folder) = pending.pop() {
            let entries = fs::read_dir(&folder).map_err(|error| {
                format!(
                    "Could not search Start menu folder {}: {error}",
                    folder.display()
                )
            })?;
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    pending.push(path);
                    continue;
                }
                if !path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
                {
                    continue;
                }
                let display = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if display == wanted {
                    exact.push(path);
                } else if display.contains(&wanted) || wanted.contains(&display) {
                    partial.push(path);
                }
            }
        }
    }
    exact.sort();
    exact.dedup();
    partial.sort();
    partial.dedup();
    let candidates = if exact.is_empty() { partial } else { exact };
    match candidates.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        many => {
            let names = many
                .iter()
                .filter_map(|path| path.file_stem().and_then(|s| s.to_str()))
                .take(8)
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "More than one Start menu shortcut matches '{requested}': {names}. Use a more specific app name."
            ))
        }
    }
}

#[cfg(windows)]
fn shell_open(target: &str) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr::null_mut};
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> *mut c_void;
    }
    let verb: Vec<u16> = OsStr::new("open").encode_wide().chain(Some(0)).collect();
    let file: Vec<u16> = OsStr::new(target).encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            null_mut(),
            null_mut(),
            1,
        )
    };
    if (result as isize) <= 32 {
        return Err(format!(
            "Windows could not open {target} (ShellExecuteW returned {}).",
            result as isize
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn shell_open(target: &str) -> Result<(), String> {
    Command::new("open")
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open {target}: {e}"))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn shell_open(target: &str) -> Result<(), String> {
    Command::new(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open {target}: {e}"))
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Tool argument '{key}' must be a string."))
}

fn optional_string<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("Tool argument '{key}' must be a string or null.")),
    }
}

fn optional_bool(value: &Value, key: &str) -> Result<Option<bool>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("Tool argument '{key}' must be a boolean or null.")),
    }
}

fn optional_u32(value: &Value, key: &str) -> Result<Option<u32>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| {
                format!("Tool argument '{key}' must be a non-negative integer or null.")
            }),
    }
}

fn required_i64(value: &Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Tool argument '{key}' must be an integer."))
}

fn required_u32(value: &Value, key: &str) -> Result<u32, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| format!("Tool argument '{key}' must be a non-negative integer."))
}

fn required_u64(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("Tool argument '{key}' must be a non-negative integer."))
}
