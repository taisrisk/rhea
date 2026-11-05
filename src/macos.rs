//! Native macOS input and window control for JEV: CoreGraphics events for the pointer and the
//! keyboard, and System Events (AppleScript) for "which app and window is in front". Reading an
//! app's accessibility tree and acting on its elements still goes through Agent Desktop, which wraps
//! the macOS Accessibility (AX) API; the code here is what makes pointer, keyboard and focus
//! actions immediate instead of a CLI round trip each.
//!
//! Posting events needs the Accessibility permission for JARVUS (System Settings > Privacy &
//! Security > Accessibility). Without it macOS drops the events silently, so every entry point
//! checks `AXIsProcessTrusted` first and says what to enable.

use crate::uia::{Button, MOVE_PX_PER_MS};
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

const NOT_TRUSTED: &str = "macOS blocks JARVUS from controlling the keyboard and mouse until it is allowed in System Settings > Privacy & Security > Accessibility. Enable JARVUS there (add it with + if it is missing), then try again.";

pub fn accessibility_trusted() -> bool {
    // SAFETY: a plain query with no arguments.
    unsafe { AXIsProcessTrusted() }
}

fn require_trust() -> Result<(), String> {
    if accessibility_trusted() {
        return Ok(());
    }
    // Take the user to the right pane once per launch; the error text tells the model what to relay.
    static OPENED: std::sync::Once = std::sync::Once::new();
    OPENED.call_once(open_accessibility_settings);
    Err(NOT_TRUSTED.into())
}

/// Open the Accessibility pane of System Settings so the user can switch JARVUS on.
pub fn open_accessibility_settings() {
    let _ = Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn();
}

fn source() -> Result<CGEventSource, String> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| "Could not create a macOS event source.".to_owned())
}

fn post(event: CGEvent) {
    event.post(CGEventTapLocation::HID);
}

// ---- pointer ----------------------------------------------------------------------------

/// 0 = no button held, 1 = left, 2 = right, 3 = middle (so moves become drags while a button is down).
static HELD: AtomicU8 = AtomicU8::new(0);

fn kinds(button: Button) -> (CGEventType, CGEventType, CGEventType, CGMouseButton, u8) {
    match button {
        Button::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGEventType::LeftMouseDragged, CGMouseButton::Left, 1),
        Button::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGEventType::RightMouseDragged, CGMouseButton::Right, 2),
        Button::Middle => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGEventType::OtherMouseDragged, CGMouseButton::Center, 3),
    }
}

fn mouse_event(kind: CGEventType, x: f64, y: f64, button: CGMouseButton) -> Result<CGEvent, String> {
    CGEvent::new_mouse_event(source()?, kind, CGPoint::new(x, y), button).map_err(|_| "Could not create a macOS mouse event.".to_owned())
}

pub fn cursor() -> (i32, i32) {
    source()
        .ok()
        .and_then(|source| CGEvent::new(source).ok())
        .map(|event| {
            let point = event.location();
            (point.x.round() as i32, point.y.round() as i32)
        })
        .unwrap_or((0, 0))
}

fn move_to(x: f64, y: f64) -> Result<(), String> {
    let (kind, button) = match HELD.load(Ordering::SeqCst) {
        1 => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
        2 => (CGEventType::RightMouseDragged, CGMouseButton::Right),
        3 => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
        _ => (CGEventType::MouseMoved, CGMouseButton::Left),
    };
    post(mouse_event(kind, x, y, button)?);
    Ok(())
}

/// Glide to (x, y) at the same visible speed as on Windows (about 1000 px in 250 ms).
pub fn move_smooth(x: i32, y: i32) -> Result<(), String> {
    require_trust()?;
    let (from_x, from_y) = cursor();
    let (dx, dy) = ((x - from_x) as f64, (y - from_y) as f64);
    let distance = (dx * dx + dy * dy).sqrt();
    let duration_ms = (distance / MOVE_PX_PER_MS).clamp(0.0, 1500.0);
    let steps = ((duration_ms / 8.0).ceil() as u32).max(1);
    for step in 1..=steps {
        let t = f64::from(step) / f64::from(steps);
        let eased = t * t * (3.0 - 2.0 * t);
        move_to(f64::from(from_x) + dx * eased, f64::from(from_y) + dy * eased)?;
        if step < steps {
            sleep(Duration::from_millis(8));
        }
    }
    Ok(())
}

pub fn click(x: i32, y: i32, button: Button, count: u32, restore: bool) -> Result<(), String> {
    require_trust()?;
    let before = cursor();
    let (down, up, _, mouse_button, _) = kinds(button);
    move_to(f64::from(x), f64::from(y))?;
    sleep(Duration::from_millis(20));
    for index in 1..=count.clamp(1, 3) {
        for kind in [down, up] {
            let event = mouse_event(kind, f64::from(x), f64::from(y), mouse_button)?;
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, i64::from(index));
            post(event);
            sleep(Duration::from_millis(12));
        }
    }
    if restore {
        move_to(f64::from(before.0), f64::from(before.1))?;
    }
    Ok(())
}

pub fn button(button: Button, press: bool) -> Result<(), String> {
    require_trust()?;
    let (down, up, _, mouse_button, code) = kinds(button);
    let (x, y) = cursor();
    let event = mouse_event(if press { down } else { up }, f64::from(x), f64::from(y), mouse_button)?;
    event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, 1);
    post(event);
    HELD.store(if press { code } else { 0 }, Ordering::SeqCst);
    Ok(())
}

pub fn drag(from: (i32, i32), to: (i32, i32), which: Button) -> Result<(), String> {
    require_trust()?;
    move_smooth(from.0, from.1)?;
    button(which, true)?;
    sleep(Duration::from_millis(40));
    let moved = move_smooth(to.0, to.1);
    sleep(Duration::from_millis(40));
    let released = button(which, false);
    moved.and(released)
}

/// Positive `notches` scroll up, negative down; three lines per notch.
pub fn wheel(x: i32, y: i32, notches: i32) -> Result<(), String> {
    require_trust()?;
    if (x, y) != (0, 0) {
        move_to(f64::from(x), f64::from(y))?;
        sleep(Duration::from_millis(20));
    }
    // core-graphics has no scroll-event constructor: turn a blank event into a line-based scroll wheel event.
    let event = CGEvent::new(source()?).map_err(|_| "Could not create a macOS scroll event.".to_owned())?;
    event.set_type(CGEventType::ScrollWheel);
    event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, i64::from(notches.saturating_mul(3)));
    post(event);
    Ok(())
}

// ---- keyboard ---------------------------------------------------------------------------

/// Virtual key codes of the ANSI layout (Carbon `kVK_*`).
fn keycode(name: &str) -> Option<u16> {
    let name = name.trim().to_lowercase();
    Some(match name.as_str() {
        "a" => 0, "s" => 1, "d" => 2, "f" => 3, "h" => 4, "g" => 5, "z" => 6, "x" => 7, "c" => 8, "v" => 9,
        "b" => 11, "q" => 12, "w" => 13, "e" => 14, "r" => 15, "y" => 16, "t" => 17,
        "1" => 18, "2" => 19, "3" => 20, "4" => 21, "6" => 22, "5" => 23, "=" | "plus" | "equals" => 24,
        "9" => 25, "7" => 26, "-" | "minus" => 27, "8" => 28, "0" => 29, "]" => 30, "o" => 31, "u" => 32,
        "[" => 33, "i" => 34, "p" => 35, "enter" | "return" => 36, "l" => 37, "j" => 38, "'" => 39, "k" => 40,
        ";" => 41, "\\" => 42, "," => 43, "/" => 44, "n" => 45, "m" => 46, "." => 47,
        "tab" => 48, "space" | " " => 49, "`" => 50, "backspace" => 51, "esc" | "escape" => 53,
        "f5" => 96, "f6" => 97, "f7" => 98, "f3" => 99, "f8" => 100, "f9" => 101, "f11" => 103,
        "f10" => 109, "f12" => 111, "home" => 115, "pageup" | "pgup" => 116, "delete" | "del" => 117,
        "f4" => 118, "end" => 119, "f2" => 120, "pagedown" | "pgdn" => 121, "f1" => 122,
        "left" => 123, "right" => 124, "down" => 125, "up" => 126,
        _ => return None,
    })
}

/// Split "ctrl+shift+k" into modifier flags and a key code. `ctrl`/`win`/`meta`/`super` mean the
/// Command key, so a combo written for Windows ("ctrl+c") does the same thing here; `control`
/// stays Control only via `ctrl_literal`.
fn parse_combo(combo: &str) -> Result<(CGEventFlags, u16), String> {
    let mut flags = CGEventFlags::empty();
    let mut key: Option<u16> = None;
    for part in combo.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_lowercase().as_str() {
            "cmd" | "command" | "ctrl" | "control" | "win" | "meta" | "super" => flags |= CGEventFlags::CGEventFlagCommand,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "alt" | "option" | "opt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl_literal" | "rawctrl" => flags |= CGEventFlags::CGEventFlagControl,
            other => {
                if key.is_some() {
                    return Err(format!("'{combo}' names more than one key."));
                }
                key = Some(keycode(other).ok_or_else(|| format!("Unknown key '{part}' in '{combo}'."))?);
            }
        }
    }
    Ok((flags, key.ok_or_else(|| format!("'{combo}' has no key, only modifiers."))?))
}

pub fn press_combo(combo: &str) -> Result<(), String> {
    require_trust()?;
    let (flags, key) = parse_combo(combo)?;
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(source()?, key, down).map_err(|_| "Could not create a macOS key event.".to_owned())?;
        event.set_flags(flags);
        post(event);
        sleep(Duration::from_millis(8));
    }
    Ok(())
}

/// Type text exactly as given, any language, by posting Unicode strings in small chunks.
pub fn type_text(text: &str) -> Result<(), String> {
    require_trust()?;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            press_combo("enter")?;
        }
        let units: Vec<u16> = line.trim_end_matches('\r').encode_utf16().collect();
        for chunk in units.chunks(16) {
            let piece = String::from_utf16_lossy(chunk);
            for down in [true, false] {
                let event = CGEvent::new_keyboard_event(source()?, 0, down).map_err(|_| "Could not create a macOS key event.".to_owned())?;
                event.set_string(&piece);
                post(event);
            }
            sleep(Duration::from_millis(3));
        }
    }
    Ok(())
}

// ---- apps and windows (System Events) ---------------------------------------------------

/// Run AppleScript lines with arguments passed safely through `argv` (never spliced into the script).
fn osascript(lines: &[&str], args: &[&str]) -> Result<String, String> {
    if args.iter().any(|arg| arg.starts_with('-')) {
        return Err("Argument looks like an option.".into());
    }
    let mut command = Command::new("osascript");
    for line in lines {
        command.arg("-e").arg(line);
    }
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| format!("Could not run osascript: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(6) => {
                let _ = child.kill();
                return Err("osascript timed out (the app may be busy or waiting on a permission prompt).".into());
            }
            Ok(None) => sleep(Duration::from_millis(15)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Name of the frontmost application, or empty when it cannot be read.
pub fn front_app_name() -> String {
    osascript(&["tell application \"System Events\" to get name of first application process whose frontmost is true"], &[]).unwrap_or_default()
}

/// Title of the frontmost app's front window, or empty.
pub fn front_window_title() -> String {
    osascript(
        &[
            "tell application \"System Events\"",
            "set p to first application process whose frontmost is true",
            "try",
            "return name of front window of p",
            "on error",
            "return \"\"",
            "end try",
            "end tell",
        ],
        &[],
    )
    .unwrap_or_default()
}

pub fn app_has_window(app: &str) -> bool {
    osascript(
        &[
            "on run argv",
            "tell application \"System Events\"",
            "if not (exists process (item 1 of argv)) then return \"0\"",
            "return (count of windows of process (item 1 of argv)) as string",
            "end tell",
            "end run",
        ],
        &[app],
    )
    .ok()
    .and_then(|count| count.trim().parse::<u32>().ok())
    .is_some_and(|count| count > 0)
}

/// Bring `app` to the front (launching it when needed) and return the title of its window.
pub fn activate(app: &str) -> Result<String, String> {
    if app.trim().is_empty() || app.starts_with('-') {
        return Err("No app name.".into());
    }
    let status = Command::new("open").arg("-a").arg(app).status().map_err(|e| format!("Could not run open: {e}"))?;
    if !status.success() {
        return Err(format!("macOS could not find an app called '{app}'."));
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if crate::desktop::label_score(app, &front_app_name(), false) > 0 {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    let title = front_window_title();
    Ok(if title.is_empty() { app.to_owned() } else { title })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_split_into_modifiers_and_one_key() {
        let (flags, key) = parse_combo("ctrl+shift+K").unwrap();
        assert!(flags.contains(CGEventFlags::CGEventFlagCommand) && flags.contains(CGEventFlags::CGEventFlagShift));
        assert_eq!(key, 40);
        assert_eq!(parse_combo("enter").unwrap().1, 36);
        assert_eq!(parse_combo("alt+f4").unwrap().1, 118);
        assert!(parse_combo("cmd+").is_err());
        assert!(parse_combo("a+b").is_err());
        assert!(parse_combo("cmd+nonsense").is_err());
    }

    #[test]
    fn arguments_that_look_like_options_are_refused() {
        assert!(osascript(&["return 1"], &["-e"]).is_err());
        assert!(activate("-a").is_err());
    }
}
