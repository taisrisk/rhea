//! Platform desktop layer for JEV. Windows uses UI Automation/Win32 directly;
//! macOS delegates accessibility and input to Agent Desktop's supported CLI.
//!
//! agent-desktop re-walks an app's whole accessibility tree for every ref
//! action (3-5 s per call on Chromium/Electron apps such as Discord). Here the
//! same questions are answered natively: Win32 for windows and input, and UI
//! Automation for the focused control and a whole-window tree read in a single
//! cached request.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FocusedControl {
    pub name: String,
    pub is_text_input: bool,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("left") => Ok(Self::Left),
            Some("right") => Ok(Self::Right),
            Some("middle") => Ok(Self::Middle),
            Some(other) => Err(format!("Unknown mouse button '{other}'.")),
        }
    }
}

/// Pointer speed for visible moves and drags: 1000 px per 250 ms.
pub const MOVE_PX_PER_MS: f64 = 4.0;

#[cfg(windows)]
mod imp {
    use super::{Button, FocusedControl, MOVE_PX_PER_MS};
    use serde_json::{Map, Value, json};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::Threading::{
        AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY, VkKeyScanW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, EnumWindows, GW_OWNER, GetCursorPos, GetForegroundWindow,
        GetSystemMetrics, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN, SW_RESTORE, SetCursorPos, SetForegroundWindow, ShowWindow,
    };
    use windows::core::{BOOL, PWSTR};

    fn automation() -> Result<IUIAutomation, String> {
        unsafe {
            // S_FALSE / RPC_E_CHANGED_MODE just mean COM is already set up on this thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| format!("UI Automation is unavailable: {error}"))
        }
    }

    pub fn focused_control() -> Result<FocusedControl, String> {
        let automation = automation()?;
        unsafe {
            let element = automation
                .GetFocusedElement()
                .map_err(|error| format!("No focused control: {error}"))?;
            let name = element
                .CurrentName()
                .map(|name| name.to_string())
                .unwrap_or_default();
            let kind = element.CurrentControlType().unwrap_or_default();
            let value = element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()
                .and_then(|pattern| pattern.CurrentValue().ok())
                .map(|value| value.to_string());
            Ok(FocusedControl {
                name,
                is_text_input: kind == UIA_EditControlTypeId || kind == UIA_ComboBoxControlTypeId,
                value,
            })
        }
    }

    // ---- Windows -------------------------------------------------------------

    fn title(hwnd: HWND) -> String {
        let mut buffer = [0u16; 512];
        let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        String::from_utf16_lossy(&buffer[..length.max(0) as usize])
    }

    fn process_id(hwnd: HWND) -> u32 {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        pid
    }

    fn exe_stem(pid: u32) -> String {
        unsafe {
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return String::new();
            };
            let mut buffer = [0u16; 1024];
            let mut size = buffer.len() as u32;
            let path = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
            .map(|()| String::from_utf16_lossy(&buffer[..size as usize]))
            .unwrap_or_default();
            let _ = CloseHandle(process);
            path.rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .trim_end_matches(".exe")
                .trim_end_matches(".EXE")
                .to_owned()
        }
    }

    /// How well a top-level window fits a user-named app: by executable name
    /// ("chrome" -> chrome.exe) or by title ("Google Chrome", "@Alex - Discord").
    fn window_score(app: &str, hwnd: HWND) -> i32 {
        let exe = exe_stem(process_id(hwnd));
        let by_exe = crate::desktop::label_score(app, &exe, false);
        let by_title = crate::desktop::label_score(app, &title(hwnd), false);
        // Executable identity is the stronger signal; title is the fallback.
        if by_exe > 0 { by_exe + 1000 } else { by_title }
    }

    fn app_window(app: &str) -> Option<HWND> {
        struct Search<'a> {
            app: &'a str,
            own_pid: u32,
            best: Option<(i32, HWND)>,
        }
        unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
            let search = unsafe { &mut *(data.0 as *mut Search) };
            unsafe {
                if !IsWindowVisible(hwnd).as_bool()
                    || GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid())
                {
                    return BOOL(1);
                }
            }
            if process_id(hwnd) == search.own_pid || title(hwnd).trim().is_empty() {
                return BOOL(1);
            }
            let score = window_score(search.app, hwnd);
            // EnumWindows walks top-to-bottom, so the first best is the frontmost.
            if score > 0 && search.best.is_none_or(|(best, _)| score > best) {
                search.best = Some((score, hwnd));
            }
            BOOL(1)
        }
        let mut search = Search {
            app,
            own_pid: unsafe { GetCurrentProcessId() },
            best: None,
        };
        unsafe {
            let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
        }
        search.best.map(|(_, hwnd)| hwnd)
    }

    pub fn foreground_title() -> String {
        title(unsafe { GetForegroundWindow() })
    }

    /// True when the foreground window belongs to `app`.
    pub fn app_is_foreground(app: &str) -> bool {
        let foreground = unsafe { GetForegroundWindow() };
        !foreground.is_invalid() && window_score(app, foreground) > 0
    }

    pub fn app_has_window(app: &str) -> bool {
        app_window(app).is_some()
    }

    /// Bring `app`'s frontmost window forward. Returns its title.
    pub fn focus_app_window(app: &str) -> Result<String, String> {
        if app.trim().is_empty() {
            return Err("No app name.".into());
        }
        if app_is_foreground(app) {
            return Ok(foreground_title());
        }
        let hwnd = app_window(app).ok_or_else(|| format!("No open {app} window."))?;
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            // Windows only lets the foreground thread hand focus away; borrow its
            // input queue for the switch so this works from JEV's worker thread.
            let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
            let current_thread = GetCurrentThreadId();
            let attached = foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            if attached {
                let _ = AttachThreadInput(current_thread, foreground_thread, false);
            }
        }
        for _ in 0..20 {
            if unsafe { GetForegroundWindow() } == hwnd {
                return Ok(title(hwnd));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(format!("Windows did not bring {app} to the front."))
    }

    // ---- UI tree ---------------------------------------------------------------

    #[allow(non_upper_case_globals)]
    fn role(kind: UIA_CONTROLTYPE_ID) -> &'static str {
        match kind {
            UIA_ButtonControlTypeId | UIA_SplitButtonControlTypeId => "button",
            UIA_HyperlinkControlTypeId => "link",
            UIA_TextControlTypeId => "statictext",
            UIA_EditControlTypeId => "textfield",
            UIA_ComboBoxControlTypeId => "combobox",
            UIA_ListItemControlTypeId | UIA_DataItemControlTypeId => "option",
            UIA_ListControlTypeId | UIA_DataGridControlTypeId => "list",
            UIA_TreeItemControlTypeId => "treeitem",
            UIA_TreeControlTypeId => "tree",
            UIA_MenuItemControlTypeId => "menuitem",
            UIA_MenuControlTypeId | UIA_MenuBarControlTypeId => "menu",
            UIA_TabItemControlTypeId => "tab",
            UIA_TabControlTypeId => "tablist",
            UIA_CheckBoxControlTypeId => "checkbox",
            UIA_RadioButtonControlTypeId => "radiobutton",
            UIA_DocumentControlTypeId => "document",
            UIA_WindowControlTypeId => "window",
            UIA_ImageControlTypeId => "image",
            UIA_SliderControlTypeId => "slider",
            UIA_ToolBarControlTypeId => "toolbar",
            _ => "group",
        }
    }

    fn cached_flag(element: &IUIAutomationElement, property: UIA_PROPERTY_ID) -> bool {
        unsafe {
            element
                .GetCachedPropertyValue(property)
                .ok()
                .and_then(|value: VARIANT| bool::try_from(&value).ok())
                .unwrap_or(false)
        }
    }

    /// `app`'s frontmost window as an agent-desktop-shaped tree (role, name,
    /// bounds, states, available_actions, children), read in one cached UIA
    /// request instead of one cross-process call per element.
    pub fn window_tree(app: &str) -> Result<Value, String> {
        let hwnd = if app_is_foreground(app) {
            unsafe { GetForegroundWindow() }
        } else {
            app_window(app).ok_or_else(|| format!("No open {app} window."))?
        };
        let automation = automation()?;
        unsafe {
            let request = automation
                .CreateCacheRequest()
                .map_err(|error| error.to_string())?;
            for property in [
                UIA_NamePropertyId,
                UIA_ControlTypePropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_IsOffscreenPropertyId,
                UIA_IsInvokePatternAvailablePropertyId,
                UIA_IsTogglePatternAvailablePropertyId,
                UIA_IsSelectionItemPatternAvailablePropertyId,
                UIA_HasKeyboardFocusPropertyId,
            ] {
                request
                    .AddProperty(property)
                    .map_err(|error| error.to_string())?;
            }
            request
                .SetTreeScope(TreeScope_Subtree)
                .map_err(|error| error.to_string())?;
            request
                .SetTreeFilter(
                    &automation
                        .ControlViewCondition()
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            let root = automation
                .ElementFromHandleBuildCache(hwnd, &request)
                .map_err(|error| format!("Could not read the {app} window: {error}"))?;
            let mut budget = 25_000usize;
            Ok(convert(&root, &mut budget))
        }
    }

    fn convert(element: &IUIAutomationElement, budget: &mut usize) -> Value {
        *budget = budget.saturating_sub(1);
        let mut node = Map::new();
        unsafe {
            node.insert(
                "role".into(),
                Value::from(role(element.CachedControlType().unwrap_or_default())),
            );
            let name = element
                .CachedName()
                .map(|name| name.to_string())
                .unwrap_or_default();
            if !name.is_empty() {
                node.insert("name".into(), Value::from(name));
            }
            if let Ok(rect) = element.CachedBoundingRectangle() {
                node.insert("bounds".into(), json!({"x":rect.left as f64,"y":rect.top as f64,
                    "width":(rect.right - rect.left) as f64,"height":(rect.bottom - rect.top) as f64}));
            }
            let mut states = Vec::new();
            if element
                .CachedIsOffscreen()
                .is_ok_and(|offscreen| offscreen.as_bool())
            {
                states.push("offscreen");
            }
            if cached_flag(element, UIA_HasKeyboardFocusPropertyId) {
                states.push("focused");
            }
            if !states.is_empty() {
                node.insert("states".into(), json!(states));
            }
            if cached_flag(element, UIA_IsInvokePatternAvailablePropertyId)
                || cached_flag(element, UIA_IsTogglePatternAvailablePropertyId)
                || cached_flag(element, UIA_IsSelectionItemPatternAvailablePropertyId)
            {
                node.insert("available_actions".into(), json!(["Click"]));
            }
            if let Ok(children) = element.GetCachedChildren() {
                let mut converted = Vec::new();
                for index in 0..children.Length().unwrap_or(0) {
                    if *budget == 0 {
                        break;
                    }
                    if let Ok(child) = children.GetElement(index) {
                        converted.push(convert(&child, budget));
                    }
                }
                if !converted.is_empty() {
                    node.insert("children".into(), Value::Array(converted));
                }
            }
        }
        Value::Object(node)
    }

    /// Focus the editable field in the foreground window with the best `score`
    /// for its accessible name (Chromium/Electron expose the placeholder, e.g.
    /// "Message @My goddess"). Clicks the field, then falls back to UIA
    /// SetFocus. Returns true once it reports keyboard focus; never types.
    pub fn focus_text_field(score: &dyn Fn(&str) -> i32) -> Result<bool, String> {
        let automation = automation()?;
        unsafe {
            let window = automation
                .ElementFromHandle(GetForegroundWindow())
                .map_err(|error| format!("Foreground window is not accessible: {error}"))?;
            let condition = automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_EditControlTypeId.0),
                )
                .map_err(|error| error.to_string())?;
            let fields = window
                .FindAll(TreeScope_Descendants, &condition)
                .map_err(|error| error.to_string())?;
            // Best-scoring field wins; an exact tie between two fields is refused.
            let mut target = None;
            let mut best = 0;
            let mut tied = false;
            for index in 0..fields.Length().unwrap_or(0) {
                let Ok(field) = fields.GetElement(index) else {
                    continue;
                };
                if field
                    .CurrentIsOffscreen()
                    .is_ok_and(|offscreen| offscreen.as_bool())
                {
                    continue;
                }
                let name = field
                    .CurrentName()
                    .map(|name| name.to_string())
                    .unwrap_or_default();
                let points = score(&name);
                if points > best {
                    best = points;
                    target = Some(field);
                    tied = false;
                } else if points > 0 && points == best {
                    tied = true;
                }
            }
            if tied {
                return Err("Two text fields match equally; refusing to guess.".into());
            }
            let Some(field) = target else {
                return Ok(false);
            };
            let is_focused = || {
                automation
                    .GetFocusedElement()
                    .ok()
                    .and_then(|focused| automation.CompareElements(&focused, &field).ok())
                    .is_some_and(|same| same.as_bool())
            };
            if is_focused() {
                return Ok(true);
            }
            if let Ok(rect) = field.CurrentBoundingRectangle() {
                if rect.right > rect.left && rect.bottom > rect.top {
                    // Click the left part of the box so attachment/emoji buttons
                    // at its edges are never hit.
                    let x = rect.left + ((rect.right - rect.left) / 4).min(120);
                    let y = rect.top + (rect.bottom - rect.top) / 2;
                    if click(x, y, Button::Left, 1, true).is_ok() {
                        for _ in 0..15 {
                            if is_focused() {
                                return Ok(true);
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            }
            let _ = field.SetFocus();
            for _ in 0..15 {
                if is_focused() {
                    return Ok(true);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(false)
        }
    }

    // ---- Mouse -----------------------------------------------------------------

    fn send(inputs: &[INPUT]) -> Result<(), String> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(format!(
                "Windows accepted {sent} of {} input events.",
                inputs.len()
            ))
        }
    }

    fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        }
    }

    /// Absolute move event in virtual-desktop coordinates, so apps see a real
    /// pointer move (drag-and-drop needs these, not just SetCursorPos).
    fn move_event(x: i32, y: i32) -> INPUT {
        unsafe {
            let (left, top) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            );
            let (width, height) = (
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            );
            let nx = ((x - left) as i64 * 65535 / (width - 1).max(1) as i64) as i32;
            let ny = ((y - top) as i64 * 65535 / (height - 1).max(1) as i64) as i32;
            mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                nx,
                ny,
                0,
            )
        }
    }

    pub fn cursor() -> (i32, i32) {
        let mut point = Default::default();
        unsafe {
            let _ = GetCursorPos(&mut point);
        }
        (point.x, point.y)
    }

    /// Glide the pointer to (x, y) at MOVE_PX_PER_MS (1000 px / 250 ms).
    pub fn move_smooth(x: i32, y: i32) -> Result<(), String> {
        let (start_x, start_y) = cursor();
        let distance = (((x - start_x).pow(2) + (y - start_y).pow(2)) as f64).sqrt();
        let duration_ms = (distance / MOVE_PX_PER_MS).max(1.0);
        let started = Instant::now();
        loop {
            let t = (started.elapsed().as_secs_f64() * 1000.0 / duration_ms).min(1.0);
            let px = start_x + ((x - start_x) as f64 * t).round() as i32;
            let py = start_y + ((y - start_y) as f64 * t).round() as i32;
            send(&[move_event(px, py)])?;
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    }

    fn button_flags(button: Button) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
        match button {
            Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }
    }

    /// Click `count` times at (x, y). `restore` puts the pointer back afterwards
    /// (used for internal focusing clicks so the user's cursor is not stolen).
    pub fn click(x: i32, y: i32, button: Button, count: u32, restore: bool) -> Result<(), String> {
        let previous = cursor();
        unsafe {
            SetCursorPos(x, y).map_err(|error| format!("Could not move the pointer: {error}"))?;
        }
        send(&[move_event(x, y)])?;
        let (down, up) = button_flags(button);
        for index in 0..count.clamp(1, 3) {
            if index > 0 {
                std::thread::sleep(Duration::from_millis(40));
            }
            send(&[mouse(down, 0, 0, 0), mouse(up, 0, 0, 0)])?;
        }
        std::thread::sleep(Duration::from_millis(30));
        if restore {
            unsafe {
                let _ = SetCursorPos(previous.0, previous.1);
            }
        }
        Ok(())
    }

    pub fn button(button: Button, down: bool) -> Result<(), String> {
        let (down_flag, up_flag) = button_flags(button);
        send(&[mouse(if down { down_flag } else { up_flag }, 0, 0, 0)])
    }

    /// Press at `from`, glide to `to`, release: drag-and-drop, sliders, selection.
    pub fn drag(from: (i32, i32), to: (i32, i32), button_kind: Button) -> Result<(), String> {
        move_smooth(from.0, from.1)?;
        button(button_kind, true)?;
        std::thread::sleep(Duration::from_millis(60));
        let result = move_smooth(to.0, to.1);
        std::thread::sleep(Duration::from_millis(60));
        let released = button(button_kind, false);
        result.and(released)
    }

    /// Wheel at (x, y); positive `notches` scrolls up.
    pub fn wheel(x: i32, y: i32, notches: i32) -> Result<(), String> {
        unsafe {
            SetCursorPos(x, y).map_err(|error| error.to_string())?;
        }
        send(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, notches.clamp(-20, 20) * 120)])
    }

    // ---- Keyboard --------------------------------------------------------------

    fn key(vk: u16, up: bool) -> INPUT {
        let extended = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C);
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if up {
            flags |= KEYEVENTF_KEYUP;
        }
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        }
    }

    fn virtual_key(name: &str) -> Option<u16> {
        let lower = name.to_ascii_lowercase();
        Some(match lower.as_str() {
            "enter" | "return" => 0x0D,
            "tab" => 0x09,
            "esc" | "escape" => 0x1B,
            "space" => 0x20,
            "backspace" => 0x08,
            "delete" | "del" => 0x2E,
            "insert" | "ins" => 0x2D,
            "up" => 0x26,
            "down" => 0x28,
            "left" => 0x25,
            "right" => 0x27,
            "home" => 0x24,
            "end" => 0x23,
            "pageup" | "pgup" => 0x21,
            "pagedown" | "pgdn" => 0x22,
            "ctrl" | "control" => 0x11,
            "shift" => 0x10,
            "alt" => 0x12,
            "win" | "meta" | "cmd" | "super" => 0x5B,
            "capslock" => 0x14,
            "printscreen" => 0x2C,
            "menu" | "apps" => 0x5D,
            _ => {
                if let Some(number) = lower
                    .strip_prefix('f')
                    .and_then(|rest| rest.parse::<u16>().ok())
                {
                    if (1..=24).contains(&number) {
                        return Some(0x6F + number);
                    }
                }
                let mut chars = name.chars();
                let (Some(ch), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                let scan = unsafe { VkKeyScanW(ch as u16) };
                if scan == -1 {
                    return None;
                }
                (scan as u16) & 0xFF
            }
        })
    }

    /// Press a combo such as "ctrl+k", "ctrl+shift+t", "alt+tab", "f5", "/".
    pub fn press_combo(combo: &str) -> Result<(), String> {
        let parts: Vec<&str> = combo
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        let (main, modifiers) = parts.split_last().ok_or("Empty key combination.")?;
        let mut keys = Vec::new();
        for part in modifiers.iter().chain(std::iter::once(main)) {
            keys.push(
                virtual_key(part).ok_or_else(|| format!("Unknown key '{part}' in '{combo}'."))?,
            );
        }
        let mut inputs: Vec<INPUT> = keys.iter().map(|vk| key(*vk, false)).collect();
        inputs.extend(keys.iter().rev().map(|vk| key(*vk, true)));
        send(&inputs)
    }
}

#[cfg(windows)]
pub use imp::{
    app_has_window, app_is_foreground, button, click, cursor, drag, focus_app_window,
    focus_text_field, focused_control, foreground_title, move_smooth, press_combo, wheel,
    window_tree,
};

#[cfg(target_os = "macos")]
mod macos {
    use super::{Button, FocusedControl};
    use serde_json::Value;

    fn app_snapshot(app: &str) -> Result<Value, String> {
        crate::desktop::snapshot_target("window", Some(app), None, false, false)
            .map(|snapshot| snapshot.json)
    }

    fn snapshot_id(snapshot: &Value) -> Result<&str, String> {
        snapshot
            .get("snapshot_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Agent Desktop returned a snapshot without an ID.".to_owned())
    }

    fn action_succeeded(result: &Value) -> bool {
        result
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "completed" | "completed_unverified"))
    }

    fn action_error(result: &Value, fallback: &str) -> String {
        result
            .pointer("/result/message")
            .and_then(Value::as_str)
            .or_else(|| {
                result
                    .pointer("/result/error/message")
                    .and_then(Value::as_str)
            })
            .unwrap_or(fallback)
            .to_owned()
    }

    fn windows(app: Option<&str>) -> Option<Vec<Value>> {
        let result = crate::desktop::list_windows(app).ok()?;
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        // Agent Desktop's documented list-windows data is the array itself.
        result.get("data")?.as_array().cloned()
    }

    fn focused_window() -> Option<Value> {
        windows(None)?
            .into_iter()
            .find(|window| window.get("is_focused").and_then(Value::as_bool) == Some(true))
    }

    pub fn focused_control() -> Result<FocusedControl, String> {
        Err("On macOS, JEV uses Agent Desktop accessibility refs instead of native focused-control reads.".into())
    }

    pub fn foreground_title() -> String {
        focused_window()
            .and_then(|window| {
                window
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    }

    pub fn app_is_foreground(app: &str) -> bool {
        focused_window().is_some_and(|window| {
            ["app_name", "title"].iter().any(|key| {
                window
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|found| crate::desktop::label_score(app, found, false) > 0)
            })
        })
    }

    pub fn app_has_window(app: &str) -> bool {
        windows(Some(app)).is_some_and(|items| !items.is_empty())
    }

    pub fn focus_app_window(app: &str) -> Result<String, String> {
        if app.trim().is_empty() {
            return Err("No app name.".into());
        }
        let result = crate::desktop::focus_window(None, Some(app), None)?;
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(result
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Agent Desktop could not focus the requested app.")
                .to_owned());
        }
        // focus-window returns the focused window in its documented envelope.
        Ok(result
            .pointer("/data/focused/title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .unwrap_or(app)
            .to_owned())
    }

    pub fn window_tree(app: &str) -> Result<Value, String> {
        let snapshot = app_snapshot(app)?;
        snapshot.pointer("/data/tree").cloned().ok_or_else(|| {
            "Agent Desktop did not return an accessibility tree for this app.".into()
        })
    }

    pub fn focus_text_field(_score: &dyn Fn(&str) -> i32) -> Result<bool, String> {
        // JEV's caller falls through to an Agent Desktop ref action when this
        // platform-specific shortcut is unavailable.
        Ok(false)
    }

    pub fn cursor() -> (i32, i32) {
        (0, 0)
    }

    pub fn move_smooth(x: i32, y: i32) -> Result<(), String> {
        let snapshot = crate::desktop::snapshot_target("focused", None, None, false, false)?.json;
        let result =
            crate::desktop::mouse_move_from_snapshot(snapshot_id(&snapshot)?, x as i64, y as i64)?;
        if action_succeeded(&result) {
            Ok(())
        } else {
            Err(action_error(
                &result,
                "Agent Desktop could not move the pointer.",
            ))
        }
    }

    pub fn click(x: i32, y: i32, button: Button, count: u32, restore: bool) -> Result<(), String> {
        if restore {
            return Err("Agent Desktop cannot restore the pointer position after a click.".into());
        }
        // Require a fresh observation before coordinate-based input, so stale
        // screen coordinates are never blindly replayed.
        let _snapshot = crate::desktop::snapshot_target("focused", None, None, false, false)?;
        let button = match button {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        };
        let result = crate::desktop::mouse_click_at(x as i64, y as i64, button, count.clamp(1, 3))?;
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(result
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Agent Desktop could not click the requested position.")
                .to_owned())
        }
    }

    pub fn button(_button: Button, _down: bool) -> Result<(), String> {
        Err("Agent Desktop does not expose standalone mouse-down/up actions on macOS.".into())
    }

    pub fn drag(from: (i32, i32), to: (i32, i32), _button: Button) -> Result<(), String> {
        let _snapshot = crate::desktop::snapshot_target("focused", None, None, false, false)?;
        let result =
            crate::desktop::drag_at((from.0 as i64, from.1 as i64), (to.0 as i64, to.1 as i64))?;
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(result
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Agent Desktop could not drag the requested position.")
                .to_owned())
        }
    }

    pub fn wheel(x: i32, y: i32, notches: i32) -> Result<(), String> {
        if (x, y) == (0, 0) {
            return Err(
                "A pointer position is required for Agent Desktop mouse-wheel input.".into(),
            );
        }
        let _snapshot = crate::desktop::snapshot_target("focused", None, None, false, false)?;
        let result = crate::desktop::mouse_wheel_at(x as i64, y as i64, notches)?;
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(result
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Agent Desktop could not scroll the requested position.")
                .to_owned())
        }
    }

    pub fn press_combo(combo: &str) -> Result<(), String> {
        let combo = combo
            .split('+')
            .map(str::trim)
            .map(|part| {
                if part.eq_ignore_ascii_case("ctrl")
                    || part.eq_ignore_ascii_case("control")
                    || part.eq_ignore_ascii_case("win")
                    || part.eq_ignore_ascii_case("meta")
                    || part.eq_ignore_ascii_case("super")
                {
                    "cmd"
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join("+");
        let snapshot = crate::desktop::snapshot_target("focused", None, None, false, false)?.json;
        let result = crate::desktop::press_from_snapshot(snapshot_id(&snapshot)?, &combo)?;
        if action_succeeded(&result) {
            Ok(())
        } else {
            Err(action_error(
                &result,
                "Agent Desktop could not deliver the key combination.",
            ))
        }
    }
}
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(windows, target_os = "macos")))]
mod fallback {
    use super::{Button, FocusedControl};
    use serde_json::Value;
    const NO: &str = "Native desktop control is Windows-only.";
    pub fn focused_control() -> Result<FocusedControl, String> {
        Err(NO.into())
    }
    pub fn foreground_title() -> String {
        String::new()
    }
    pub fn app_is_foreground(_app: &str) -> bool {
        false
    }
    pub fn app_has_window(_app: &str) -> bool {
        false
    }
    pub fn focus_app_window(_app: &str) -> Result<String, String> {
        Err(NO.into())
    }
    pub fn window_tree(_app: &str) -> Result<Value, String> {
        Err(NO.into())
    }
    pub fn focus_text_field(_score: &dyn Fn(&str) -> i32) -> Result<bool, String> {
        Err(NO.into())
    }
    pub fn cursor() -> (i32, i32) {
        (0, 0)
    }
    pub fn move_smooth(_x: i32, _y: i32) -> Result<(), String> {
        Err(NO.into())
    }
    pub fn click(
        _x: i32,
        _y: i32,
        _button: Button,
        _count: u32,
        _restore: bool,
    ) -> Result<(), String> {
        Err(NO.into())
    }
    pub fn button(_button: Button, _down: bool) -> Result<(), String> {
        Err(NO.into())
    }
    pub fn drag(_from: (i32, i32), _to: (i32, i32), _button: Button) -> Result<(), String> {
        Err(NO.into())
    }
    pub fn wheel(_x: i32, _y: i32, _notches: i32) -> Result<(), String> {
        Err(NO.into())
    }
    pub fn press_combo(_combo: &str) -> Result<(), String> {
        Err(NO.into())
    }
}
#[cfg(not(windows))]
pub use fallback::*;

/// Kept for callers that only need a quick read of a value.
#[allow(dead_code)]
pub fn tree_summary(tree: &Value) -> usize {
    fn count(node: &Value) -> usize {
        1 + node
            .get("children")
            .and_then(Value::as_array)
            .map(|children| children.iter().map(count).sum())
            .unwrap_or(0)
    }
    count(tree)
}

#[cfg(all(test, windows))]
mod live {
    /// Live probes: `cargo test --bin jarvus uia::live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn probe_quick_switcher() {
        // Opens Discord's quick switcher, samples the focused control, closes it.
        println!("focus={:?}", super::focus_app_window("Discord"));
        let started = std::time::Instant::now();
        super::press_combo("ctrl+k").unwrap();
        for _ in 0..15 {
            println!(
                "{:>5} ms {:?}",
                started.elapsed().as_millis(),
                super::focused_control()
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        super::press_combo("esc").unwrap();
    }

    #[test]
    #[ignore]
    fn probe_discord_focus() {
        let started = std::time::Instant::now();
        let title = super::focus_app_window("Discord");
        let focus_ms = started.elapsed().as_millis();
        let read = std::time::Instant::now();
        let control = super::focused_control();
        println!(
            "focus={title:?} in {focus_ms} ms; focused={control:?} in {} ms",
            read.elapsed().as_millis()
        );
        let tree = std::time::Instant::now();
        let window = super::window_tree("Discord");
        println!(
            "window_tree: {} nodes in {} ms",
            window.as_ref().map(super::tree_summary).unwrap_or(0),
            tree.elapsed().as_millis()
        );
        if let Ok(window) = window {
            let bounds = window
                .get("bounds")
                .and_then(crate::desktop::parse_bounds_pub);
            for query in ["hxstile", "my goddess", "inbox"] {
                println!(
                    "  {query} -> {:?}",
                    crate::desktop::locate_in_tree(&window, bounds, query, Some("servers"), false)
                        .or_else(|| crate::desktop::locate_in_tree(
                            &window, bounds, query, None, false
                        ))
                );
            }
        }
        for app in ["chrome", "discord", "obsidian", "spotify"] {
            println!("  app {app}: has_window={}", super::app_has_window(app));
        }
    }
}
