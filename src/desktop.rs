use std::{
    collections::HashMap,
    env, fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

pub struct SnapshotResult {
    pub json: Value,
    pub local_visualization: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct ElementHint {
    reference: Option<String>,
    role: String,
    name: String,
    bounds: Option<Bounds>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, Default)]
struct TargetActionResolution {
    status: &'static str,
    reason: &'static str,
    match_count: usize,
    actionable_candidates: Vec<ElementHint>,
    candidates_truncated: bool,
    tree_complete: bool,
    tree_retained: bool,
}

impl TargetActionResolution {
    fn unresolved(reason: &'static str) -> Self {
        Self {
            status: "unresolved",
            reason,
            ..Self::default()
        }
    }

    fn to_json(&self) -> Value {
        let candidates = self
            .actionable_candidates
            .iter()
            .take(8)
            .map(|hint| {
                json!({
                    "ref": hint.reference,
                    "role": hint.role,
                    "name": hint.name,
                })
            })
            .collect::<Vec<_>>();
        let mut result = json!({
            "status": self.status,
            "reason": self.reason,
            "match_count": self.match_count,
            "actionable_ancestor_count": self.actionable_candidates.len(),
            "candidates_truncated": self.candidates_truncated,
            "tree_complete": self.tree_complete,
            "tree_retained": self.tree_retained,
            "candidates": candidates,
        });
        if self.status == "resolved" {
            if let Some(reference) = self
                .actionable_candidates
                .first()
                .and_then(|hint| hint.reference.as_ref())
            {
                result["action_ref"] = Value::String(reference.clone());
            }
        }
        result
    }
}

#[derive(Clone, Debug)]
struct SnapshotContext {
    snapshot_id: String,
    app: Option<String>,
    window_id: Option<String>,
    surface: String,
    complete: bool,
    bounds: Option<Bounds>,
    elements: Vec<ElementHint>,
    raw_tree: Option<Arc<Value>>,
    raw_tree_bytes: usize,
    captured_at: Instant,
}

const MAX_LOCAL_RAW_TREE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SINGLE_RAW_TREE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SUMMARY_ELEMENTS: usize = 160;
pub const DEFAULT_SNAPSHOT_MAX_DEPTH: u8 = 32;
pub const DEFAULT_SNAPSHOT_TIMEOUT_MS: u32 = 30_000;
// Send sequencing needs a new same-window snapshot ID between the verified
// type and Return, but does not need a long Electron settle wait after a
// local mutation; bound only this internal refresh's deadline.
const SEND_REFRESH_TIMEOUT_MS: u32 = 3_000;
// The internal post-action verification refresh (after click/type/scroll/press)
// only needs to find the next actionable control and confirm the ref/snapshot
// is still current; it never has to hand the model a full tree. A full,
// non-interactive, non-compact walk of a Chromium/Electron accessibility tree
// (Discord, Slack, etc.) at depth 32 is the dominant cost of every tool call
// and is what made a single type-and-send round trip take 30-40+ seconds.
// Scope this refresh to interactive controls in compact form so each action's
// verification snapshot stays cheap; a model-issued `desktop_snapshot` call
// still gets the full tree by default (see toolset.json / system_prompt.md).
const ACTION_REFRESH_INTERACTIVE_ONLY: bool = true;
const ACTION_REFRESH_COMPACT: bool = true;

/// Snapshot shape used for the automatic post-action refresh, as opposed to a
/// model-requested `desktop_snapshot` call (which keeps the full/deep default).
fn action_refresh_snapshot_params() -> (bool, bool, bool, u8) {
    (
        ACTION_REFRESH_INTERACTIVE_ONLY,
        ACTION_REFRESH_COMPACT,
        false,
        DEFAULT_SNAPSHOT_MAX_DEPTH,
    )
}

static SNAPSHOTS: OnceLock<Mutex<HashMap<String, SnapshotContext>>> = OnceLock::new();
static LATEST_BY_SCOPE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn snapshots() -> &'static Mutex<HashMap<String, SnapshotContext>> {
    SNAPSHOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn latest_by_scope() -> &'static Mutex<HashMap<String, String>> {
    LATEST_BY_SCOPE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn status() -> Result<Value, String> {
    run(&["status".to_owned()])
}

pub fn list_apps() -> Result<Value, String> {
    run(&["list-apps".to_owned()])
}

pub fn list_windows(app: Option<&str>) -> Result<Value, String> {
    let mut args = vec!["list-windows".to_owned()];
    push_optional(&mut args, "--app", app)?;
    run(&args)
}

pub fn launch(app: &str) -> Result<Value, String> {
    let app = nonempty(app, "app name")?;
    run(&["launch".to_owned(), app.to_owned(), "--activate".to_owned()])
}

/// Run Agent Desktop's documented coordinate click command. This is used by
/// the macOS UIA adapter for right/middle and multi-click actions.
pub fn mouse_click_at(x: i64, y: i64, button: &str, count: u32) -> Result<Value, String> {
    if !matches!(button, "left" | "right" | "middle") {
        return Err("Mouse button must be left, right, or middle.".into());
    }
    if !(1..=100).contains(&count) {
        return Err("Mouse click count must be between 1 and 100.".into());
    }
    run(&[
        "mouse-click".into(),
        format!("--xy={x},{y}"),
        "--button".into(),
        button.into(),
        "--count".into(),
        count.to_string(),
        "--headed".into(),
    ])
}

/// Run Agent Desktop's documented drag command.
pub fn drag_at(from: (i64, i64), to: (i64, i64)) -> Result<Value, String> {
    run(&[
        "drag".into(),
        format!("--from-xy={},{}", from.0, from.1),
        format!("--to-xy={},{}", to.0, to.1),
        "--headed".into(),
    ])
}

/// Run Agent Desktop's documented mouse wheel command.
pub fn mouse_wheel_at(x: i64, y: i64, delta: i32) -> Result<Value, String> {
    run(&[
        "mouse-wheel".into(),
        "--x".into(),
        x.to_string(),
        "--y".into(),
        y.to_string(),
        "--dy".into(),
        delta.to_string(),
        "--headed".into(),
    ])
}

pub fn focus_window(
    window_id: Option<&str>,
    app: Option<&str>,
    title: Option<&str>,
) -> Result<Value, String> {
    if window_id.map(str::trim).filter(|v| !v.is_empty()).is_none()
        && app.map(str::trim).filter(|v| !v.is_empty()).is_none()
        && title.map(str::trim).filter(|v| !v.is_empty()).is_none()
    {
        return Err("Choose a window ID, app name, or title to focus.".into());
    }
    let mut args = vec!["focus-window".to_owned(), "--headed".to_owned()];
    push_optional(&mut args, "--window-id", window_id)?;
    push_optional(&mut args, "--app", app)?;
    push_optional(&mut args, "--title", title)?;
    run(&args)
}

pub fn snapshot(
    surface: &str,
    app: Option<&str>,
    include_visualization: bool,
) -> Result<SnapshotResult, String> {
    snapshot_target(surface, app, None, false, include_visualization)
}

pub fn snapshot_target(
    surface: &str,
    app: Option<&str>,
    window_id: Option<&str>,
    interactive_only: bool,
    include_visualization: bool,
) -> Result<SnapshotResult, String> {
    snapshot_target_options(
        surface,
        app,
        window_id,
        interactive_only,
        false,
        false,
        DEFAULT_SNAPSHOT_MAX_DEPTH,
        DEFAULT_SNAPSHOT_TIMEOUT_MS,
        include_visualization,
    )
}

pub fn snapshot_target_options(
    surface: &str,
    app: Option<&str>,
    window_id: Option<&str>,
    interactive_only: bool,
    compact: bool,
    skeleton: bool,
    max_depth: u8,
    timeout_ms: u32,
    include_visualization: bool,
) -> Result<SnapshotResult, String> {
    if !(1..=32).contains(&max_depth) {
        return Err("Snapshot max_depth must be between 1 and 32.".into());
    }
    validate_snapshot_timeout(timeout_ms)?;
    // agent-desktop's HTML visual-debug renderer currently accepts only a
    // window surface; ordinary tree snapshots still honor the chosen surface.
    let snapshot_surface = if include_visualization {
        "window"
    } else {
        surface
    };
    let mut args = snapshot_command_args(
        snapshot_surface,
        app,
        window_id,
        interactive_only,
        compact,
        skeleton,
        max_depth,
        timeout_ms,
    )?;

    let visualization = if include_visualization {
        let folder = crate::chat_store::captures_dir().join("agent-desktop");
        fs::create_dir_all(&folder)
            .map_err(|error| format!("Could not create local visualization folder {}: {error}", folder.display()))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();
        let path = folder.join(format!("uia-snapshot-{stamp}.html"));
        args.extend([
            "--debug".to_owned(),
            "--screenshot".to_owned(),
            path.to_string_lossy().to_string(),
        ]);
        Some(path)
    } else {
        None
    };

    let json = run(&args)?;
    remember_snapshot(&json, surface, app, window_id);
    Ok(SnapshotResult {
        json,
        local_visualization: visualization,
    })
}

fn snapshot_command_args(
    surface: &str,
    app: Option<&str>,
    window_id: Option<&str>,
    interactive_only: bool,
    compact: bool,
    skeleton: bool,
    max_depth: u8,
    timeout_ms: u32,
) -> Result<Vec<String>, String> {
    if !(1..=32).contains(&max_depth) {
        return Err("Snapshot max_depth must be between 1 and 32.".into());
    }
    validate_snapshot_timeout(timeout_ms)?;
    let mut args = vec![
        "snapshot".to_owned(),
        "--surface".to_owned(),
        surface.to_owned(),
        "--include-bounds".to_owned(),
        "--max-depth".to_owned(),
        max_depth.to_string(),
        "--timeout-ms".to_owned(),
        timeout_ms.to_string(),
        "--headed".to_owned(),
    ];
    if compact {
        args.push("--compact".to_owned());
    }
    if skeleton {
        args.push("--skeleton".to_owned());
    }
    push_optional(&mut args, "--app", app)?;
    push_optional(&mut args, "--window-id", window_id)?;
    if interactive_only {
        args.push("--interactive-only".to_owned());
    }
    Ok(args)
}

/// Return a bounded, model-facing view of the local accessibility tree. The full
/// Agent Desktop envelope is intentionally kept out of the conversation payload.
pub fn snapshot_summary(value: &Value) -> Value {
    snapshot_summary_filtered(value, "all").expect("the all filter is always valid")
}

pub fn snapshot_summary_filtered(value: &Value, filter: &str) -> Result<Value, String> {
    snapshot_summary_for_target(value, filter, None)
}

pub fn snapshot_summary_for_target(
    value: &Value,
    filter: &str,
    target_text: Option<&str>,
) -> Result<Value, String> {
    let filter = normalize_snapshot_filter(filter)?;
    let data = value.get("data").unwrap_or(value);
    let snapshot_id = find_string(value, "snapshot_id").unwrap_or_default();
    let (context, stored) = snapshots()
        .lock()
        .ok()
        .and_then(|map| {
            map.get(&snapshot_id)
                .cloned()
                .map(|context| (Some(context), true))
        })
        .unwrap_or((None, false));
    let mut elements = Vec::new();
    let mut target_match_count = 0usize;
    let mut target_resolution = target_text.map(|target| {
        context
            .as_ref()
            .map(|context| resolve_target_action(context, target))
            .unwrap_or_else(|| TargetActionResolution::unresolved("snapshot_not_retained"))
    });
    if let Some(context) = &context {
        let target = target_text
            .map(|text| text.trim().to_lowercase())
            .filter(|text| !text.is_empty());
        let mut candidates: Vec<ElementHint> = context
            .elements
            .iter()
            .filter(|hint| filter_hint(&filter, hint))
            .cloned()
            .collect();
        if let (Some(target), Some(tree)) = (&target, &context.raw_tree) {
            let mut matches = Vec::new();
            collect_matching_element_hints(tree, target, &mut matches);
            for hint in matches {
                if filter_hint(&filter, &hint)
                    && !candidates
                        .iter()
                        .any(|existing| existing.reference == hint.reference)
                {
                    candidates.push(hint);
                }
            }
        }
        if let Some(resolution) = &target_resolution {
            target_match_count = resolution.match_count;
            for hint in &resolution.actionable_candidates {
                if !candidates
                    .iter()
                    .any(|existing| existing.reference == hint.reference)
                {
                    candidates.push(hint.clone());
                }
            }
        }
        if let Some(target) = &target {
            if target_resolution.is_none() {
                target_match_count = candidates
                    .iter()
                    .filter(|hint| hint.name.to_lowercase().contains(target))
                    .count();
            }
        }
        let mut ordered: Vec<&ElementHint> = candidates.iter().collect();
        if let Some(target) = &target {
            ordered.sort_by_key(|hint| {
                let name = hint.name.to_lowercase();
                if name == *target {
                    0
                } else if name.contains(target) {
                    1
                } else {
                    2
                }
            });
        }
        for hint in ordered.into_iter().take(MAX_SUMMARY_ELEMENTS) {
            let mut item = serde_json::Map::new();
            if let Some(reference) = &hint.reference {
                item.insert("ref".into(), Value::String(reference.clone()));
            } else {
                item.insert("coordinate_only".into(), Value::Bool(true));
            }
            if !hint.role.is_empty() {
                item.insert("role".into(), Value::String(hint.role.clone()));
            }
            if !hint.name.is_empty() {
                item.insert("name".into(), Value::String(hint.name.clone()));
            }
            if let Some(bounds) = hint.bounds {
                item.insert("bounds".into(), bounds_json(bounds));
            }
            elements.push(Value::Object(item));
        }
    }
    let retained_total = context
        .as_ref()
        .map(|context| context.elements.len())
        .unwrap_or(0);
    let total = context
        .as_ref()
        .map(|context| {
            context
                .elements
                .iter()
                .filter(|hint| filter_hint(&filter, hint))
                .count()
        })
        .unwrap_or(0);
    let raw_count = data
        .get("ref_count")
        .and_then(Value::as_u64)
        .unwrap_or(retained_total as u64);
    Ok(json!({
        "kind": "desktop_snapshot",
        "target": {
            "app": context.as_ref().and_then(|c| c.app.clone()),
            "window_id": context.as_ref().and_then(|c| c.window_id.clone()),
            "surface": context.as_ref().map(|c| c.surface.clone()),
        },
        "snapshot_id": snapshot_id,
        "complete": data.get("complete").and_then(Value::as_bool),
        "ref_count": raw_count,
        "returned_actionable_count": elements.len(),
        "omitted_actionable": total.saturating_sub(elements.len()) as u64,
        "filtered_out_actionable": retained_total.saturating_sub(total) as u64,
        "local_ref_index_truncated": raw_count > retained_total as u64,
        "filter": filter,
        "window_bounds": context.as_ref().and_then(|c| c.bounds).map(bounds_json),
        "elements": elements,
        "target_text": target_text.filter(|text| !text.trim().is_empty()),
        "target_match_count": target_match_count,
        "target_resolution": target_resolution.take().map(|resolution| resolution.to_json()),
        "local_tree_retained": context.as_ref().is_some_and(|c| c.raw_tree.is_some()),
        "local_snapshot_retained": stored,
    }))
}

pub fn click_and_refresh(reference: &str) -> Result<Value, String> {
    perform_ref_action("click", reference, None, None)
}

// Used by the in-app manual control surface as well as the model tool path.
// Keep both routes on the same delivery-check and post-action observation path.
pub fn click(reference: &str) -> Result<Value, String> {
    click_and_refresh(reference)
}

pub fn find(
    app: Option<&str>,
    window_id: Option<&str>,
    text: Option<&str>,
    name: Option<&str>,
    role: Option<&str>,
) -> Result<Value, String> {
    let mut args = vec!["find".to_owned(), "--headed".to_owned()];
    push_optional(&mut args, "--app", app)?;
    push_optional(&mut args, "--window-id", window_id)?;
    push_optional(&mut args, "--text", text)?;
    push_optional(&mut args, "--name", name)?;
    push_optional(&mut args, "--role", role)?;
    let mut result = run(&args)?;
    // Agent Desktop's documented Windows role name for editable controls is
    // `textfield`; accept the familiar UIA alias `edit` and retry only when
    // the first read completed successfully with zero matches.
    if should_retry_edit_role(role, &result) {
        if let Some(index) = args.iter().position(|arg| arg == "--role") {
            args[index + 1] = "textfield".to_owned();
            result = run(&args)?;
        }
    }
    remember_snapshot(&result, "window", app, window_id);
    let mut summary = snapshot_summary(&result);
    if let Some(object) = summary.as_object_mut() {
        object.insert("kind".into(), Value::String("desktop_find".into()));
        object.insert("query".into(), json!({"text":text,"name":name,"role":role}));
        object.insert(
            "match_count".into(),
            Value::from(
                result
                    .pointer("/data/matches")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
            ),
        );
    }
    Ok(summary)
}

fn validate_snapshot_timeout(timeout_ms: u32) -> Result<(), String> {
    if !(1..=120_000).contains(&timeout_ms) {
        return Err("Snapshot timeout_ms must be between 1 and 120,000.".into());
    }
    Ok(())
}

fn should_retry_edit_role(role: Option<&str>, result: &Value) -> bool {
    role.is_some_and(|role| role.trim().eq_ignore_ascii_case("edit"))
        && result
            .pointer("/data/matches")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

pub fn scroll_and_refresh(reference: &str, direction: &str, amount: u32) -> Result<Value, String> {
    let direction = direction.trim().to_ascii_lowercase();
    if !["up", "down", "left", "right"].contains(&direction.as_str()) {
        return Err("Scroll direction must be up, down, left, or right.".into());
    }
    if !(1..=10).contains(&amount) {
        return Err("Scroll amount must be between 1 and 10.".into());
    }
    perform_ref_action("scroll", reference, None, Some((direction, amount)))
}

pub fn hover_and_refresh(reference: &str) -> Result<Value, String> {
    perform_ref_action("hover", reference, None, None)
}

pub fn get(reference: &str, property: &str) -> Result<Value, String> {
    validate_ref(reference)?;
    if !["text", "value", "title", "bounds", "role", "states"].contains(&property) {
        return Err(
            "Unsupported element property. Choose text, value, title, bounds, role, or states."
                .into(),
        );
    }
    run(&[
        "get".to_owned(),
        reference.to_owned(),
        "--property".to_owned(),
        property.to_owned(),
        "--headed".to_owned(),
    ])
}

pub fn type_and_refresh(snapshot_id: &str, reference: &str, text: &str) -> Result<Value, String> {
    type_and_refresh_with_timeout(snapshot_id, reference, text, DEFAULT_SNAPSHOT_TIMEOUT_MS)
}

fn type_and_refresh_with_timeout(
    snapshot_id: &str,
    reference: &str,
    text: &str,
    refresh_timeout_ms: u32,
) -> Result<Value, String> {
    if text.chars().count() > 2000 {
        return Err("JEV limits one typing action to 2,000 characters.".into());
    }
    validate_ref_snapshot(snapshot_id, reference)?;
    context_for_fresh_snapshot(snapshot_id)?;
    perform_ref_action_with_timeout("type", reference, Some(text), None, refresh_timeout_ms)
}

pub fn press_from_snapshot(snapshot_id: &str, combo: &str) -> Result<Value, String> {
    press_from_snapshot_with_timeout(snapshot_id, combo, DEFAULT_SNAPSHOT_TIMEOUT_MS)
}

fn press_from_snapshot_with_timeout(
    snapshot_id: &str,
    combo: &str,
    refresh_timeout_ms: u32,
) -> Result<Value, String> {
    let normalized = combo.trim().to_ascii_lowercase();
    let allowed = [
        "return",
        "escape",
        "tab",
        "shift+tab",
        "space",
        "backspace",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "ctrl+a",
        "ctrl+c",
        "ctrl+v",
        "ctrl+s",
        "ctrl+w",
        "ctrl+l",
        "ctrl+shift+tab",
        "alt+tab",
    ];
    if !allowed.contains(&normalized.as_str()) {
        return Err("That key combination is not enabled for JEV.".into());
    }
    let context = context_for_fresh_snapshot(snapshot_id)?;
    let mut args = vec!["press".to_owned(), normalized, "--headed".to_owned()];
    push_optional(&mut args, "--app", context.app.as_deref())?;
    push_optional(&mut args, "--window-id", context.window_id.as_deref())?;
    let result = run_command(&args);
    Ok(action_result_with_timeout(
        "press",
        result,
        context,
        refresh_timeout_ms,
    ))
}

/// Type into a fresh editable ref, then submit exactly once only when the
/// type command succeeded after verified focus and supplies a fresh post-type
/// view. A postcondition mismatch alone is not treated as failed execution.
/// The typed value is never copied into this result.
pub fn send_and_refresh(snapshot_id: &str, reference: &str, text: &str) -> Result<Value, String> {
    send_with_actions(
        || type_and_refresh_with_timeout(snapshot_id, reference, text, SEND_REFRESH_TIMEOUT_MS),
        |fresh_snapshot_id| {
            press_from_snapshot_with_timeout(fresh_snapshot_id, "return", SEND_REFRESH_TIMEOUT_MS)
        },
    )
}

fn send_with_actions<T, P>(mut type_action: T, mut press_action: P) -> Result<Value, String>
where
    T: FnMut() -> Result<Value, String>,
    P: FnMut(&str) -> Result<Value, String>,
{
    let typed = type_action()?;
    let type_delivery = typed
        .pointer("/result/delivery")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let type_succeeded = typed.pointer("/result/ok").and_then(Value::as_bool) == Some(true);
    let focus_verified = typed
        .pointer("/result/focus_verified_before_type")
        .and_then(Value::as_bool)
        == Some(true);
    let delivery_possible = matches!(type_delivery, "delivered_verified" | "delivered_unverified");
    if !type_succeeded || !focus_verified || !delivery_possible {
        let failure_kind =
            type_failure_kind(&typed, type_succeeded, focus_verified, delivery_possible);
        let type_result = typed.get("result").unwrap_or(&Value::Null);
        return Ok(json!({
            "kind":"desktop_send",
            "status":"not_submitted",
            "type_delivery":type_delivery,
            "type_command_succeeded":type_succeeded,
            "focus_verified_before_type":focus_verified,
            "type_failure":{
                "kind":failure_kind,
                "action_status":typed.get("status").and_then(Value::as_str),
                "error_code":type_result.get("code").and_then(Value::as_str),
                "stage":type_result.get("stage").and_then(Value::as_str),
                "agent_desktop_code":type_result.get("agent_desktop_code").and_then(Value::as_str),
                "agent_desktop_details_kind":type_result.get("agent_desktop_details_kind").and_then(Value::as_str),
                "verification_scope":type_result.get("verification_scope").and_then(Value::as_str),
                "verification_reason":type_result.get("verification_reason").and_then(Value::as_str)
            },
            "type_verification_scope":typed.pointer("/result/verification_scope").and_then(Value::as_str),
            "type_verification_reason":typed.pointer("/result/verification_reason").and_then(Value::as_str),
            "timings_ms":{
                "focus":typed.pointer("/result/stage_timings_ms/focus"),
                "focus_check":typed.pointer("/result/stage_timings_ms/focus_check"),
                "type":typed.pointer("/result/stage_timings_ms/type"),
                "post_type_observation":typed.get("observation_elapsed_ms")
            },
            "reason":"Text entry did not succeed after verified focus; submit was not attempted.",
            "fresh_observation":typed.get("fresh_observation")
        }));
    }
    let fresh_snapshot_id = typed
        .pointer("/fresh_observation/snapshot_id")
        .and_then(Value::as_str)
        .ok_or("Verified text entry did not produce a fresh snapshot; submit was not attempted.")?;
    let submitted = press_action(fresh_snapshot_id)?;
    let delivery = submitted
        .pointer("/result/delivery")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let timings = json!({
        "focus":typed.pointer("/result/stage_timings_ms/focus"),
        "focus_check":typed.pointer("/result/stage_timings_ms/focus_check"),
        "type":typed.pointer("/result/stage_timings_ms/type"),
        "post_type_observation":typed.get("observation_elapsed_ms"),
        "submit":submitted.pointer("/result/process_elapsed_ms"),
        "post_submit_observation":submitted.get("observation_elapsed_ms")
    });
    let status = if delivery == "delivered_verified" {
        "submitted_verified"
    } else if delivery == "delivered_unverified" {
        "submitted_unverified"
    } else {
        "submit_failed_or_uncertain"
    };
    Ok(json!({
        "kind":"desktop_send",
        "status":status,
        "type_delivery":type_delivery,
        "submit_delivery":delivery,
        "submit_attempts":1,
        "timings_ms":timings,
        "fresh_observation":submitted.get("fresh_observation")
    }))
}

fn type_failure_kind(
    typed: &Value,
    type_succeeded: bool,
    focus_verified: bool,
    delivery_possible: bool,
) -> &'static str {
    if typed.get("status").and_then(Value::as_str) == Some("not_performed") {
        return "stale_or_not_performed";
    }
    match typed.pointer("/result/code").and_then(Value::as_str) {
        Some("FOCUS_FAILED") => return "focus_failed",
        Some("FOCUS_NOT_CONFIRMED") => return "focus_not_confirmed",
        _ => {}
    }
    if !focus_verified {
        return "focus_not_confirmed";
    }
    if !type_succeeded {
        return "type_action_failed";
    }
    if !delivery_possible {
        return "type_delivery_unconfirmed";
    }
    "type_result_inconsistent"
}

/// One local JEV call for "message <recipient> in <app>": focus the app, reuse
/// the open conversation when its composer already names the recipient,
/// otherwise open it through the app's quick switcher (Ctrl+K), then type and
/// submit once. Each step reads a fast interactive-only snapshot (~1 s on
/// Discord) instead of costing a ChatGPT round trip (~6-7 s each). Any step
/// that cannot be confirmed stops before submitting and hands the current
/// state back to the model so it can re-plan with the lower-level tools.
pub fn message_conversation(app: &str, recipient: &str, text: &str) -> Result<Value, String> {
    let app = nonempty(app, "app")?;
    let recipient = nonempty(recipient, "recipient")?;
    let text = nonempty(text, "message text")?;
    if text.chars().count() > 2000 {
        return Err("JEV limits one message to 2,000 characters.".into());
    }
    let started = Instant::now();
    let mut timings = serde_json::Map::new();
    let mut lap = Instant::now();

    let focus = run_command(&[
        "focus-window".into(),
        "--headed".into(),
        "--app".into(),
        app.into(),
    ]);
    mark_stage(&mut timings, "focus_app", &mut lap);
    if !focus.as_ref().is_ok_and(command_ok) {
        return Ok(message_stop(
            "app_not_focused",
            "Could not bring the app to the front. Launch or focus it, then retry.",
            None,
            timings,
            started,
        ));
    }

    let mut composer = fast_snapshot(app)
        .ok()
        .and_then(|snapshot| composer_for(&snapshot, recipient));
    mark_stage(&mut timings, "check_open_conversation", &mut lap);
    let already_open = composer.is_some();
    if composer.is_none() {
        let opened = open_via_quick_switcher(app, recipient);
        mark_stage(&mut timings, "quick_switcher", &mut lap);
        if let Err(reason) = opened {
            let observation = fast_snapshot(app).ok().map(|snapshot| {
                snapshot_summary_for_target(&snapshot, "all", Some(recipient))
                    .unwrap_or(Value::Null)
            });
            return Ok(message_stop(
                "conversation_not_opened",
                &reason,
                observation,
                timings,
                started,
            ));
        }
        // The conversation view renders after the switcher closes; poll briefly.
        for _ in 0..6 {
            if let Some(found) = fast_snapshot(app)
                .ok()
                .and_then(|snapshot| composer_for(&snapshot, recipient))
            {
                composer = Some(found);
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        mark_stage(&mut timings, "wait_for_conversation", &mut lap);
    }
    let Some(composer_ref) = composer else {
        let observation = fast_snapshot(app).ok().map(|snapshot| {
            snapshot_summary_for_target(&snapshot, "all", Some(recipient)).unwrap_or(Value::Null)
        });
        return Ok(message_stop(
            "composer_not_found",
            "No message box naming the recipient was found; nothing was typed or sent.",
            observation,
            timings,
            started,
        ));
    };

    let typed = run_type_with_focus(&composer_ref, text);
    mark_stage(&mut timings, "type", &mut lap);
    let typed_ok = typed.as_ref().is_ok_and(|command| {
        command_ok(command)
            && command
                .envelope
                .pointer("/data/focus_verified_before_type")
                .and_then(Value::as_bool)
                == Some(true)
    });
    if !typed_ok {
        return Ok(message_stop(
            "not_typed",
            "Typing into the message box was not confirmed; nothing was sent.",
            Some(compact_command_result(&typed)),
            timings,
            started,
        ));
    }
    // Submit exactly once. Never retried here: a delivered-but-unverified
    // Return may already have sent the message.
    let submitted = run_command(&[
        "press".into(),
        "return".into(),
        "--headed".into(),
        "--app".into(),
        app.into(),
    ]);
    mark_stage(&mut timings, "submit", &mut lap);
    let submit = compact_command_result(&submitted);
    let delivered = submitted.as_ref().is_ok_and(command_ok);
    // A sent message clears the composer; reading it is the cheapest proof.
    let cleared = delivered
        && run_command(&[
            "get".into(),
            composer_ref.clone(),
            "--property".into(),
            "value".into(),
        ])
        .ok()
        .filter(command_ok)
        .map(|command| {
            command
                .envelope
                .pointer("/data/value")
                .or_else(|| command.envelope.pointer("/data/result"))
                .and_then(Value::as_str)
                .map(|value| {
                    value
                        .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
                        .is_empty()
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);
    mark_stage(&mut timings, "verify", &mut lap);
    let status = if cleared {
        "sent_verified"
    } else if delivered {
        "submitted_unverified"
    } else {
        "submit_failed_or_uncertain"
    };
    Ok(json!({
        "kind":"desktop_message",
        "status":status,
        "conversation_was_open":already_open,
        "submit_attempts":1,
        "submit":submit,
        "timings_ms":timings,
        "total_ms":elapsed_ms(started),
        "note": if status == "sent_verified" { "Message box cleared after Return." } else { "Do not resend; check the conversation first." }
    }))
}

fn mark_stage(timings: &mut serde_json::Map<String, Value>, name: &str, lap: &mut Instant) {
    timings.insert(name.into(), Value::from(elapsed_ms(*lap)));
    *lap = Instant::now();
}

fn message_stop(
    status: &str,
    reason: &str,
    detail: Option<Value>,
    timings: serde_json::Map<String, Value>,
    started: Instant,
) -> Value {
    json!({"kind":"desktop_message","status":status,"submit_attempts":0,"reason":reason,
        "timings_ms":timings,"total_ms":elapsed_ms(started),"state":detail,
        "next":"Nothing was sent. Re-plan from this state with desktop_snapshot / desktop_click / desktop_send."})
}

/// Interactive-only, compact window snapshot, remembered so its refs are actionable.
fn fast_snapshot(app: &str) -> Result<Value, String> {
    let (interactive_only, compact, skeleton, max_depth) = action_refresh_snapshot_params();
    snapshot_target_options(
        "window",
        Some(app),
        None,
        interactive_only,
        compact,
        skeleton,
        max_depth,
        10_000,
        false,
    )
    .map(|snapshot| snapshot.json)
}

fn open_via_quick_switcher(app: &str, recipient: &str) -> Result<(), String> {
    let opened = run_command(&[
        "press".into(),
        "ctrl+k".into(),
        "--headed".into(),
        "--app".into(),
        app.into(),
    ])?;
    if !command_ok(&opened) {
        return Err("The quick switcher shortcut (Ctrl+K) was not delivered.".into());
    }
    let mut search = None;
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(120));
        if let Some(found) = fast_snapshot(app)
            .ok()
            .and_then(|snapshot| focused_text_input(&snapshot))
        {
            search = Some(found);
            break;
        }
    }
    let search = search.ok_or("The quick switcher did not open a focused search box.")?;
    let typed = run_command(&action_args("type", &search, Some(recipient), None)?)?;
    if !command_ok(&typed) {
        return Err("Could not type the recipient into the quick switcher.".into());
    }
    std::thread::sleep(Duration::from_millis(350));
    let chosen = run_command(&[
        "press".into(),
        "return".into(),
        "--headed".into(),
        "--app".into(),
        app.into(),
    ])?;
    if !command_ok(&chosen) {
        return Err("Selecting the quick switcher result was not delivered.".into());
    }
    Ok(())
}

/// The composer is a text field whose accessible name names the recipient
/// (Discord: "Message @My goddess").
fn composer_for(snapshot: &Value, recipient: &str) -> Option<String> {
    let wanted = normalize_label(recipient);
    let tree = snapshot.pointer("/data/tree")?;
    let mut found = None;
    walk_nodes(tree, &mut |node| {
        if found.is_some() || node.get("role").and_then(Value::as_str) != Some("textfield") {
            return;
        }
        let name = normalize_label(node.get("name").and_then(Value::as_str).unwrap_or_default());
        // Exact name only: a similar nickname must never receive the message.
        if name == format!("message {wanted}") {
            found = node
                .get("ref_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    });
    found
}

fn focused_text_input(snapshot: &Value) -> Option<String> {
    let tree = snapshot.pointer("/data/tree")?;
    let mut found = None;
    walk_nodes(tree, &mut |node| {
        let role = node.get("role").and_then(Value::as_str).unwrap_or_default();
        let focused = node
            .get("states")
            .and_then(Value::as_array)
            .is_some_and(|states| states.iter().any(|state| state.as_str() == Some("focused")));
        if found.is_none() && focused && matches!(role, "textfield" | "combobox") {
            found = node
                .get("ref_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    });
    found
}

// ---- Fuzzy label matching -------------------------------------------------
// Users say "hxstile" for a server named "ネ Hxstile #solo" and "chat" for a
// channel shown as "💬│chat". Labels are compared as words: case, symbols and
// emoji are ignored, every query word must appear, and labels carrying fewer
// extra words rank higher. The best-scoring visible element wins.

/// Lowercased alphanumeric words (Unicode-aware, so "ネ" stays a word).
pub(crate) fn label_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Match tier for each query word: 3 whole word, 2 word prefix, 1 inside a word.
/// Returns 0 unless every query word is found. `whole_words_only` is used where a
/// near miss is costly (choosing a message recipient).
pub(crate) fn label_score(query: &str, label: &str, whole_words_only: bool) -> i32 {
    // Apps pack descriptors into accessible names ("unread, ︱chat (text
    // channel), Private Channel (locked)"). Score each part as well, so the part
    // that *is* the name wins over a short label that merely contains the word.
    let parts: Vec<&str> = label
        .split(|ch: char| matches!(ch, ',' | '(' | ')' | '|' | '·' | '•' | '\n' | '[' | ']'))
        .filter(|part| !label_words(part).is_empty())
        .collect();
    let whole = words_score(query, label, whole_words_only);
    if parts.len() < 2 {
        return whole;
    }
    let penalty = 10 * (parts.len() as i32 - 1);
    parts
        .iter()
        .map(|part| words_score(query, part, whole_words_only))
        .filter(|score| *score > 0)
        .map(|score| (score - penalty).max(1))
        .fold(whole, i32::max)
}

fn words_score(query: &str, label: &str, whole_words_only: bool) -> i32 {
    let query = label_words(query);
    let label = label_words(label);
    if query.is_empty() || label.is_empty() {
        return 0;
    }
    if query == label {
        return 1000;
    }
    let mut weakest = 3;
    let mut positions = Vec::with_capacity(query.len());
    for word in &query {
        let found = label
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let tier = if candidate == word {
                    3
                } else if candidate.starts_with(word.as_str()) {
                    2
                } else if candidate.contains(word.as_str()) {
                    1
                } else {
                    0
                };
                (tier, index)
            })
            .max_by_key(|(tier, index)| (*tier, std::cmp::Reverse(*index)));
        match found {
            Some((tier, index)) if tier > 0 && (!whole_words_only || tier == 3) => {
                weakest = weakest.min(tier);
                positions.push(index);
            }
            _ => return 0,
        }
    }
    let base = match weakest {
        3 => 800,
        2 => 600,
        _ => 400,
    };
    let in_order = positions.windows(2).all(|pair| pair[1] == pair[0] + 1);
    let extra = label.len().saturating_sub(query.len()) as i32;
    (base + if in_order { 50 } else { 0 } - (extra * 25).min(base - 100)).max(1)
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // `reference` is kept for diagnostics and tests.
pub struct Located {
    pub x: i32,
    pub y: i32,
    pub label: String,
    pub role: String,
    pub score: i32,
    pub reference: Option<String>,
}

fn node_rect(node: &Value) -> Option<Bounds> {
    node.get("bounds")
        .and_then(parse_bounds)
        .filter(|bounds| bounds.width >= 4.0 && bounds.height >= 4.0)
}

fn is_offscreen(node: &Value) -> bool {
    node.get("states")
        .and_then(Value::as_array)
        .is_some_and(|states| {
            states
                .iter()
                .any(|state| state.as_str() == Some("offscreen"))
        })
}

fn is_clickable(node: &Value) -> bool {
    let acts = node
        .get("available_actions")
        .and_then(Value::as_array)
        .is_some_and(|actions| {
            actions
                .iter()
                .filter_map(Value::as_str)
                .any(|action| matches!(action, "Click" | "Invoke" | "Select" | "Toggle"))
        });
    acts || matches!(
        node.get("role").and_then(Value::as_str).unwrap_or_default(),
        "button"
            | "link"
            | "menuitem"
            | "menubutton"
            | "tab"
            | "treeitem"
            | "checkbox"
            | "radiobutton"
    )
}

/// Pick the on-screen point to click for the best label match in a full
/// snapshot tree. A plain text label is mapped to the clickable element of its
/// row (nearest small ancestor), e.g. Discord server icons whose name lives in
/// a hidden tooltip label beside the icon.
pub(crate) fn locate_in_tree(
    tree: &Value,
    window: Option<Bounds>,
    query: &str,
    within: Option<&str>,
    text_input: bool,
) -> Option<Located> {
    fn visit<'a>(
        node: &'a Value,
        ancestors: &mut Vec<&'a Value>,
        out: &mut Vec<(&'a Value, Vec<&'a Value>)>,
    ) {
        out.push((node, ancestors.clone()));
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            ancestors.push(node);
            for child in children {
                visit(child, ancestors, out);
            }
            ancestors.pop();
        }
    }
    let mut nodes = Vec::new();
    visit(tree, &mut Vec::new(), &mut nodes);
    let visible = |node: &Value| -> Option<Bounds> {
        let rect = node_rect(node)?;
        if is_offscreen(node) {
            return None;
        }
        if let Some(window) = window {
            let inside = rect.x < window.x + window.width
                && rect.x + rect.width > window.x
                && rect.y < window.y + window.height
                && rect.y + rect.height > window.y;
            if !inside {
                return None;
            }
        }
        Some(rect)
    };
    let mut best: Option<(i32, usize, Located)> = None;
    for (order, (node, ancestors)) in nodes.iter().enumerate() {
        let role = node.get("role").and_then(Value::as_str).unwrap_or_default();
        if matches!(role, "window" | "document" | "application") {
            continue;
        }
        if text_input && !matches!(role, "textfield" | "combobox") {
            continue;
        }
        let name = node.get("name").and_then(Value::as_str).unwrap_or_default();
        let score = label_score(query, name, false);
        if score == 0 {
            continue;
        }
        // `within` names the area the target lives in ("servers", "channels");
        // some ancestor's accessible name must match it.
        if let Some(area) = within {
            let in_area = ancestors.iter().any(|ancestor| {
                ancestor
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|area_name| label_score(area, area_name, false) > 0)
            });
            if !in_area {
                continue;
            }
        }
        // Resolve the element to click: itself when clickable and visible, else
        // the clickable element of its row.
        let mut target = visible(node)
            .filter(|_| is_clickable(node) || text_input)
            .map(|rect| (*node, rect));
        if target.is_none() {
            for ancestor in ancestors.iter().rev() {
                let Some(rect) = ancestor.get("bounds").and_then(parse_bounds) else {
                    continue;
                };
                if rect.height > 160.0 || rect.width > 700.0 {
                    break;
                } // left the row
                if is_clickable(ancestor) {
                    if let Some(rect) = visible(ancestor) {
                        target = Some((*ancestor, rect));
                        break;
                    }
                }
                fn first_clickable<'a>(
                    node: &'a Value,
                    visible: &dyn Fn(&Value) -> Option<Bounds>,
                ) -> Option<(&'a Value, Bounds)> {
                    if is_clickable(node) {
                        if let Some(rect) = visible(node) {
                            return Some((node, rect));
                        }
                    }
                    node.get("children")
                        .and_then(Value::as_array)?
                        .iter()
                        .find_map(|child| first_clickable(child, visible))
                }
                if let Some(found) = first_clickable(ancestor, &visible) {
                    target = Some(found);
                    break;
                }
            }
        }
        if target.is_none() && node.get("ref_id").is_some() {
            target = visible(node).map(|rect| (*node, rect));
        }
        let Some((element, rect)) = target else {
            continue;
        };
        // Small preference for real controls over plain labels at equal text score.
        let ranked = score * 10 + if is_clickable(node) { 5 } else { 0 };
        if best
            .as_ref()
            .is_none_or(|(current, _, _)| ranked > *current)
        {
            best = Some((
                ranked,
                order,
                Located {
                    x: (rect.x + rect.width / 2.0).round() as i32,
                    y: (rect.y + rect.height / 2.0).round() as i32,
                    label: name.chars().take(80).collect(),
                    role: role.to_owned(),
                    score,
                    reference: element
                        .get("ref_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            ));
        }
    }
    best.map(|(_, _, located)| located)
}

/// Find the best visible match for `query` in `app`, polling briefly while the
/// UI is still rendering after the previous step.
pub(crate) fn parse_bounds_pub(value: &Value) -> Option<Bounds> {
    parse_bounds(value)
}

/// Compact view of what is on screen in `app`, for ChatGPT to re-plan from
/// when a plan step stops: visible named controls and text with their center
/// points, best matches for `focus_label` first. Read natively (one UIA call).
pub fn native_state(app: &str, focus_label: Option<&str>) -> Value {
    let tree = match crate::uia::window_tree(app) {
        Ok(tree) => tree,
        Err(_) => return plan_state(app, focus_label),
    };
    let window = tree.get("bounds").and_then(parse_bounds);
    let mut items: Vec<(i32, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk_nodes(&tree, &mut |node| {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if name.is_empty() || is_offscreen(node) {
            return;
        }
        let Some(rect) = node_rect(node) else {
            return;
        };
        if let Some(window) = window {
            if rect.x >= window.x + window.width
                || rect.x + rect.width <= window.x
                || rect.y >= window.y + window.height
                || rect.y + rect.height <= window.y
            {
                return;
            }
        }
        let role = node.get("role").and_then(Value::as_str).unwrap_or_default();
        if matches!(role, "window" | "document") {
            return;
        }
        let label: String = name.chars().take(90).collect();
        if !seen.insert((label.clone(), role.to_owned())) {
            return;
        }
        let relevance = focus_label
            .map(|focus| label_score(focus, name, false))
            .unwrap_or(0)
            + if is_clickable(node) || matches!(role, "textfield" | "combobox") {
                1
            } else {
                0
            };
        items.push((relevance, json!({"role":role,"name":label,
            "x":(rect.x + rect.width / 2.0).round() as i32,"y":(rect.y + rect.height / 2.0).round() as i32})));
    });
    items.sort_by(|a, b| b.0.cmp(&a.0));
    let total = items.len();
    json!({"window":crate::uia::foreground_title(),"visible_items":total,
        "items":items.into_iter().take(140).map(|(_, item)| item).collect::<Vec<_>>()})
}

pub fn locate(
    app: &str,
    query: &str,
    within: Option<&str>,
    text_input: bool,
) -> Result<Located, String> {
    let mut last_error = match within {
        Some(area) => {
            format!("Nothing visible matches '{query}' inside an area named like '{area}'.")
        }
        None => format!("Nothing visible matches '{query}'."),
    };
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        // Native single-request UIA read first (hundreds of ms); agent-desktop's
        // multi-second snapshot only when the native read is unavailable.
        let tree = match crate::uia::window_tree(app) {
            Ok(tree) => tree,
            Err(_) => match snapshot_target_options(
                "window",
                Some(app),
                None,
                false,
                false,
                false,
                DEFAULT_SNAPSHOT_MAX_DEPTH,
                15_000,
                false,
            ) {
                Ok(snapshot) => match snapshot.json.pointer("/data/tree") {
                    Some(tree) => tree.clone(),
                    None => continue,
                },
                Err(error) => {
                    last_error = error;
                    continue;
                }
            },
        };
        let window = tree.get("bounds").and_then(parse_bounds);
        if let Some(found) = locate_in_tree(&tree, window, query, within, text_input) {
            return Ok(found);
        }
    }
    Err(last_error)
}

// ---- Plan-step primitives -------------------------------------------------
// JEV runs a ChatGPT-authored step list locally (see jev::run_plan). Steps
// name controls by their visible label; JEV resolves the label against a fast
// snapshot itself, so no model round trip is needed per step.

/// Resolve a visible control label to one actionable ref in `app`, polling
/// briefly so a control that is still rendering after the previous step is
/// found without a model round trip. `text_input` restricts to editable fields;
/// an empty label then means "the focused text field".
pub fn resolve_control(app: &str, label: &str, text_input: bool) -> Result<String, String> {
    let wanted = normalize_label(label);
    let mut last_error = String::new();
    for attempt in 0..6 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        let snapshot = match fast_snapshot(app) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        if wanted.is_empty() {
            if let Some(found) = focused_text_input(&snapshot) {
                return Ok(found);
            }
            last_error = "No focused text field.".into();
            continue;
        }
        let Some(tree) = snapshot.pointer("/data/tree") else {
            continue;
        };
        let window = snapshot
            .pointer("/data/window/bounds")
            .or_else(|| tree.get("bounds"))
            .and_then(parse_bounds);
        let (mut exact, mut partial) = (Vec::new(), Vec::new());
        walk_nodes(tree, &mut |node| {
            let Some(reference) = node.get("ref_id").and_then(Value::as_str) else {
                return;
            };
            // Scrolled-off controls (old chat messages above the viewport) can't
            // be the user's target and only make a label look ambiguous.
            if let (Some(window), Some(bounds)) =
                (window, node.get("bounds").and_then(parse_bounds))
            {
                let visible = bounds.x < window.x + window.width
                    && bounds.x + bounds.width > window.x
                    && bounds.y < window.y + window.height
                    && bounds.y + bounds.height > window.y;
                if !visible {
                    return;
                }
            }
            let role = node.get("role").and_then(Value::as_str).unwrap_or_default();
            if text_input && !matches!(role, "textfield" | "combobox") {
                return;
            }
            if role == "window" {
                return;
            }
            let name =
                normalize_label(node.get("name").and_then(Value::as_str).unwrap_or_default());
            if name.is_empty() {
                return;
            }
            if name == wanted {
                exact.push(reference.to_owned());
            } else if name.contains(&wanted) {
                partial.push(reference.to_owned());
            }
        });
        match (exact.len(), partial.len()) {
            (1, _) => return Ok(exact.remove(0)),
            (0, 1) => return Ok(partial.remove(0)),
            (0, 0) => last_error = format!("No control named '{label}' is visible."),
            (count, _) if count > 1 => {
                return Err(format!(
                    "{count} controls are named '{label}'; the step is ambiguous."
                ));
            }
            (_, count) => {
                return Err(format!(
                    "{count} controls partly match '{label}'; use the exact label."
                ));
            }
        }
    }
    Err(last_error)
}

pub fn plan_type(reference: &str, text: &str) -> Result<(), String> {
    if text.chars().count() > 2000 {
        return Err("JEV limits one typing step to 2,000 characters.".into());
    }
    let command = run_type_with_focus(reference, text)?;
    let verified = command_ok(&command)
        && command
            .envelope
            .pointer("/data/focus_verified_before_type")
            .and_then(Value::as_bool)
            == Some(true);
    if verified {
        Ok(())
    } else {
        Err(command_error(&command, "Typing was not confirmed."))
    }
}

/// Short state handed back to ChatGPT when a plan stops, so it can re-plan.
pub fn plan_state(app: &str, focus_label: Option<&str>) -> Value {
    fast_snapshot(app)
        .and_then(|snapshot| snapshot_summary_for_target(&snapshot, "all", focus_label))
        .unwrap_or_else(|error| json!({"ok":false,"error":short_text(&error)}))
}

fn command_error(command: &CommandResult, fallback: &str) -> String {
    command
        .envelope
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(short_text)
        .unwrap_or_else(|| fallback.to_owned())
}

fn walk_nodes(node: &Value, visit: &mut impl FnMut(&Value)) {
    visit(node);
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            walk_nodes(child, visit);
        }
    }
}

pub(crate) fn normalize_label(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, '@' | '#' | '\u{feff}'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn mouse_move_from_snapshot(snapshot_id: &str, x: i64, y: i64) -> Result<Value, String> {
    let context = fresh_coordinate_context(snapshot_id, x, y)?;
    let xy = format!("--xy={x},{y}");
    let result = run_command(&["mouse-move".into(), xy, "--headed".into()]);
    Ok(action_result("mouse_move", result, context))
}

pub fn mouse_click_from_snapshot(snapshot_id: &str, x: i64, y: i64) -> Result<Value, String> {
    let context = fresh_coordinate_context(snapshot_id, x, y)?;
    let xy = format!("--xy={x},{y}");
    let args = vec![
        "mouse-click".into(),
        xy,
        "--button".into(),
        "left".into(),
        "--count".into(),
        "1".into(),
        "--headed".into(),
    ];
    let result = run_command(&args);
    Ok(action_result("mouse_click", result, context))
}

pub fn wait_for_text(snapshot_id: &str, text: &str, timeout_ms: u64) -> Result<Value, String> {
    let context = context_for_fresh_snapshot(snapshot_id)?;
    let app = context
        .app
        .as_deref()
        .ok_or("Take an app-scoped snapshot before waiting for text.")?;
    let text = nonempty(text, "wait text")?;
    if timeout_ms == 0 || timeout_ms > 120_000 {
        return Err("Wait timeout must be between 1 and 120,000 ms.".into());
    }
    let args = vec![
        "wait".into(),
        "--text".into(),
        text.into(),
        "--app".into(),
        app.into(),
        "--timeout".into(),
        timeout_ms.to_string(),
    ];
    let wait = run_command(&args);
    if wait.as_ref().is_ok_and(command_ok) {
        let observation = observe_target(&context);
        Ok(
            json!({"kind":"desktop_wait","wait":compact_command_result(&wait),"observation":observation}),
        )
    } else {
        Ok(json!({"kind":"desktop_wait","wait":compact_command_result(&wait)}))
    }
}

fn remember_snapshot(
    value: &Value,
    surface: &str,
    requested_app: Option<&str>,
    requested_window: Option<&str>,
) {
    let data = value.get("data").unwrap_or(value);
    let tree = data.get("tree").unwrap_or(data);
    let snapshot_id = find_string(value, "snapshot_id")
        .or_else(|| first_ref(tree).and_then(snapshot_id_from_ref))
        .unwrap_or_default();
    if snapshot_id.is_empty() {
        return;
    }

    let app = requested_app
        .map(str::to_owned)
        .or_else(|| find_string(data, "app"))
        .or_else(|| find_string(data, "application"));
    let window_id = requested_window
        .map(str::to_owned)
        .or_else(|| find_string(data, "window_id"))
        .or_else(|| find_string(data, "windowId"))
        .or_else(|| {
            data.pointer("/window/id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let bounds = tree
        .get("bounds")
        .and_then(parse_bounds)
        .or_else(|| {
            data.get("window")
                .and_then(|window| window.get("bounds"))
                .and_then(parse_bounds)
        })
        .or_else(|| {
            latest_context_for(app.as_deref(), window_id.as_deref())
                .and_then(|context| context.bounds)
        });
    let mut elements = Vec::new();
    if let Some(matches) = data.get("matches") {
        collect_find_hints(matches, &mut elements);
    } else {
        collect_element_hints(tree, &mut elements);
    }
    let raw_tree_bytes = serde_json::to_vec(tree)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    let raw_tree = if raw_tree_bytes <= MAX_SINGLE_RAW_TREE_BYTES {
        Some(Arc::new(tree.clone()))
    } else {
        None
    };
    let retained_raw_tree_bytes = if raw_tree.is_some() {
        raw_tree_bytes
    } else {
        0
    };
    let context = SnapshotContext {
        snapshot_id: snapshot_id.clone(),
        app,
        window_id,
        surface: surface.to_owned(),
        complete: data
            .get("complete")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        bounds,
        elements,
        raw_tree,
        raw_tree_bytes: retained_raw_tree_bytes,
        captured_at: Instant::now(),
    };
    let scope = scope_key(&context);
    if let Ok(mut map) = snapshots().lock() {
        map.insert(snapshot_id.clone(), context);
        while map.len() > 32
            || map.values().map(|ctx| ctx.raw_tree_bytes).sum::<usize>() > MAX_LOCAL_RAW_TREE_BYTES
        {
            let mut ids: Vec<_> = map
                .values()
                .filter(|ctx| ctx.snapshot_id != snapshot_id)
                .map(|ctx| (ctx.captured_at, ctx.snapshot_id.clone()))
                .collect();
            if ids.is_empty() {
                break;
            }
            ids.sort_by_key(|item| item.0);
            if let Some((_, id)) = ids.into_iter().next() {
                map.remove(&id);
            }
        }
    }
    if let Ok(mut latest) = latest_by_scope().lock() {
        latest.insert(scope, snapshot_id);
    }
}

fn collect_element_hints(node: &Value, result: &mut Vec<ElementHint>) {
    if result.len() >= 256 {
        return;
    }
    let Some(object) = node.as_object() else {
        if let Some(items) = node.as_array() {
            for item in items {
                if result.len() >= 256 {
                    break;
                }
                collect_element_hints(item, result);
            }
        }
        return;
    };
    let reference = ["ref_id", "refId", "ref", "qualified_ref", "qualifiedRef"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|reference| reference.starts_with('@') && reference.contains(":e"));
    if let Some(reference) = reference {
        let role = ["role", "control_type", "controlType"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .unwrap_or("");
        // Never forward raw `value` or static `text`: those often contain a
        // user's draft, private messages, or document contents.
        let name = [
            "name",
            "label",
            "accessible_name",
            "accessibleName",
            "title",
        ]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or("");
        let name = if name.chars().count() > 160 {
            name.chars().take(157).collect::<String>() + "…"
        } else {
            name.to_owned()
        };
        let bounds = object.get("bounds").and_then(parse_bounds);
        result.push(ElementHint {
            reference: Some(reference.to_owned()),
            role: role.to_owned(),
            name,
            bounds,
        });
    }
    for (key, value) in object {
        if result.len() >= 256 {
            break;
        }
        if key == "children" || key == "tree" || value.is_array() {
            collect_element_hints(value, result);
        } else if value.is_object() && (key == "window" || key == "root") {
            collect_element_hints(value, result);
        }
    }
}

fn collect_matching_element_hints(node: &Value, target: &str, result: &mut Vec<ElementHint>) {
    if result.len() >= 256 {
        return;
    }
    if let Some(items) = node.as_array() {
        for item in items {
            if result.len() >= 256 {
                break;
            }
            collect_matching_element_hints(item, target, result);
        }
        return;
    }
    let Some(object) = node.as_object() else {
        return;
    };
    let name = [
        "name",
        "label",
        "accessible_name",
        "accessibleName",
        "title",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(Value::as_str))
    .unwrap_or("");
    if name.to_lowercase().contains(target) {
        let reference = ["ref_id", "refId", "ref", "qualified_ref", "qualifiedRef"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .filter(|reference| reference.starts_with('@') && reference.contains(":e"));
        if let Some(reference) = reference {
            let role = ["role", "control_type", "controlType"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .unwrap_or("");
            let name = if name.chars().count() > 160 {
                name.chars().take(157).collect::<String>() + "…"
            } else {
                name.to_owned()
            };
            result.push(ElementHint {
                reference: Some(reference.to_owned()),
                role: role.to_owned(),
                name,
                bounds: object.get("bounds").and_then(parse_bounds),
            });
        }
    }
    for (key, value) in object {
        if result.len() >= 256 {
            break;
        }
        if key == "children" || key == "tree" || value.is_array() {
            collect_matching_element_hints(value, target, result);
        } else if value.is_object() && (key == "window" || key == "root") {
            collect_matching_element_hints(value, target, result);
        }
    }
}

fn resolve_target_action(context: &SnapshotContext, target_text: &str) -> TargetActionResolution {
    let mut resolution = TargetActionResolution {
        status: "unresolved",
        reason: "target_not_found",
        tree_complete: context.complete,
        tree_retained: context.raw_tree.is_some(),
        ..TargetActionResolution::default()
    };
    if !context.complete {
        resolution.reason = "snapshot_incomplete";
        return resolution;
    }
    let Some(tree) = context.raw_tree.as_deref() else {
        resolution.reason = "full_tree_not_retained";
        return resolution;
    };
    let target = target_text.trim();
    if target.is_empty() {
        resolution.reason = "empty_target";
        return resolution;
    }
    let mut ancestry = Vec::new();
    collect_target_action_candidates(
        tree,
        target,
        &context.snapshot_id,
        &mut ancestry,
        &mut resolution,
    );
    resolution.reason = if resolution.match_count == 0 {
        "target_not_found"
    } else if resolution.match_count > 1 {
        "multiple_matching_labels"
    } else if resolution.candidates_truncated {
        "actionable_candidates_truncated"
    } else if resolution.actionable_candidates.is_empty() {
        "no_actionable_ancestor"
    } else if resolution.actionable_candidates.len() > 1 {
        "multiple_actionable_ancestors"
    } else {
        resolution.status = "resolved";
        "unique_actionable_ancestor"
    };
    resolution
}

fn collect_target_action_candidates(
    node: &Value,
    target: &str,
    snapshot_id: &str,
    ancestry: &mut Vec<(ElementHint, bool)>,
    resolution: &mut TargetActionResolution,
) {
    if let Some(items) = node.as_array() {
        for item in items {
            collect_target_action_candidates(item, target, snapshot_id, ancestry, resolution);
        }
        return;
    }
    let Some(object) = node.as_object() else {
        return;
    };
    let role = ["role", "control_type", "controlType"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    let name = [
        "name",
        "label",
        "accessible_name",
        "accessibleName",
        "title",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(Value::as_str))
    .unwrap_or("");
    let reference = ["ref_id", "refId", "ref", "qualified_ref", "qualifiedRef"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|reference| reference_matches_snapshot(reference, snapshot_id));
    let hint = ElementHint {
        reference: reference.map(str::to_owned),
        role: role.to_owned(),
        name: safe_label(name),
        bounds: object.get("bounds").and_then(parse_bounds),
    };
    let actionable = hint.reference.is_some() && has_click_action(role, object);

    // Only safe accessible-name-like fields participate. `text`/`value` can
    // contain private messages, composer drafts, or document content.
    if !name.trim().is_empty() && name.trim().eq_ignore_ascii_case(target) {
        resolution.match_count = resolution.match_count.saturating_add(1);
        for (candidate, is_actionable) in ancestry
            .iter()
            .map(|(hint, actionable)| (hint, *actionable))
            .chain(std::iter::once((&hint, actionable)))
        {
            if !is_actionable {
                continue;
            }
            let Some(reference) = candidate.reference.as_ref() else {
                continue;
            };
            if !resolution
                .actionable_candidates
                .iter()
                .any(|existing| existing.reference.as_ref() == Some(reference))
            {
                if resolution.actionable_candidates.len() < 32 {
                    resolution.actionable_candidates.push(candidate.clone());
                } else {
                    resolution.candidates_truncated = true;
                }
            }
        }
    }

    ancestry.push((hint, actionable));
    for (key, child) in object {
        if key == "children"
            || key == "tree"
            || key == "window"
            || key == "root"
            || child.is_array()
        {
            collect_target_action_candidates(child, target, snapshot_id, ancestry, resolution);
        }
    }
    ancestry.pop();
}

fn reference_matches_snapshot(reference: &str, snapshot_id: &str) -> bool {
    reference.starts_with('@')
        && reference.contains(":e")
        && snapshot_id_from_ref(reference.to_owned()).as_deref() == Some(snapshot_id)
}

fn has_click_action(role: &str, node: &serde_json::Map<String, Value>) -> bool {
    let role = role
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "");
    if matches!(
        role.as_str(),
        "button"
            | "link"
            | "hyperlink"
            | "menuitem"
            | "tab"
            | "tabitem"
            | "listitem"
            | "treeitem"
            | "row"
            | "splitbutton"
    ) {
        return true;
    }
    for key in [
        "primary_action",
        "primaryAction",
        "actions",
        "available_actions",
        "availableActions",
    ] {
        if action_field_has_click(node.get(key)) {
            return true;
        }
    }
    false
}

fn action_field_has_click(value: Option<&Value>) -> bool {
    fn is_click_name(value: &str) -> bool {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "click" | "press" | "invoke" | "select"
        )
    }
    match value {
        Some(Value::String(value)) => is_click_name(value),
        Some(Value::Array(items)) => items.iter().any(|item| match item {
            Value::String(value) => is_click_name(value),
            Value::Object(action) => ["name", "action", "kind", "type"]
                .iter()
                .filter_map(|key| action.get(*key).and_then(Value::as_str))
                .any(is_click_name),
            _ => false,
        }),
        Some(Value::Object(action)) => ["name", "action", "kind", "type"]
            .iter()
            .filter_map(|key| action.get(*key).and_then(Value::as_str))
            .any(is_click_name),
        _ => false,
    }
}

fn safe_label(value: &str) -> String {
    if value.chars().count() > 160 {
        value.chars().take(157).collect::<String>() + "…"
    } else {
        value.to_owned()
    }
}

fn collect_find_hints(node: &Value, result: &mut Vec<ElementHint>) {
    if result.len() >= 256 {
        return;
    }
    if let Some(items) = node.as_array() {
        for item in items {
            if result.len() >= 256 {
                break;
            }
            collect_find_hints(item, result);
        }
        return;
    }
    let Some(object) = node.as_object() else {
        return;
    };
    let reference = ["ref_id", "refId", "ref", "qualified_ref", "qualifiedRef"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|reference| reference.starts_with('@') && reference.contains(":e"))
        .map(str::to_owned);
    let role = ["role", "control_type", "controlType"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    let name = [
        "name",
        "label",
        "accessible_name",
        "accessibleName",
        "title",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(Value::as_str))
    .unwrap_or("");
    let bounds = object.get("bounds").and_then(parse_bounds);
    if reference.is_some() || (!name.is_empty() && bounds.is_some()) {
        let name = if name.chars().count() > 160 {
            name.chars().take(157).collect::<String>() + "…"
        } else {
            name.to_owned()
        };
        result.push(ElementHint {
            reference,
            role: role.to_owned(),
            name,
            bounds,
        });
    }
}

fn parse_bounds(value: &Value) -> Option<Bounds> {
    Some(Bounds {
        x: value.get("x")?.as_f64()?,
        y: value.get("y")?.as_f64()?,
        width: value.get("width")?.as_f64()?,
        height: value.get("height")?.as_f64()?,
    })
}

fn bounds_json(bounds: Bounds) -> Value {
    json!({"x":bounds.x,"y":bounds.y,"width":bounds.width,"height":bounds.height})
}

fn normalize_snapshot_filter(filter: &str) -> Result<String, String> {
    let normalized = filter.trim().to_ascii_lowercase().replace('-', "_");
    let normalized = match normalized.as_str() {
        "all" | "buttons" | "editables" | "links" | "checkables" | "text" | "containers" | "other" => normalized,
        "fields" | "text_fields" | "textfields" => "editables".to_owned(),
        "checkboxes" | "toggles" => "checkables".to_owned(),
        _ => return Err("Snapshot filter must be all, buttons, editables, links, checkables, text, containers, or other.".into()),
    };
    Ok(normalized)
}

fn filter_hint(filter: &str, hint: &ElementHint) -> bool {
    if filter == "all" {
        return true;
    }
    let role = hint
        .role
        .to_ascii_lowercase()
        .replace(' ', "_")
        .replace('-', "_");
    let is_button = [
        "button",
        "menuitem",
        "tab",
        "toolbarbutton",
        "splitbutton",
        "hyperlink",
    ]
    .iter()
    .any(|r| role.contains(r));
    let is_editable = [
        "textfield",
        "text_field",
        "textbox",
        "text_box",
        "edit",
        "combobox",
        "searchbox",
        "document",
    ]
    .iter()
    .any(|r| role.contains(r));
    let is_link = role.contains("link");
    let is_checkable = ["checkbox", "radio", "switch", "toggle"]
        .iter()
        .any(|r| role.contains(r));
    let is_text = ["text", "label", "heading", "static"]
        .iter()
        .any(|r| role.contains(r))
        || is_editable;
    let is_container = [
        "window",
        "pane",
        "group",
        "list",
        "tree",
        "section",
        "region",
        "container",
    ]
    .iter()
    .any(|r| role.contains(r));
    match filter {
        "buttons" => is_button,
        "editables" => is_editable,
        "links" => is_link,
        "checkables" => is_checkable,
        "text" => is_text,
        "containers" => is_container,
        "other" => {
            !(is_button || is_editable || is_link || is_checkable || is_text || is_container)
        }
        _ => false,
    }
}

fn find_string(value: &Value, key: &str) -> Option<String> {
    if let Some(object) = value.as_object() {
        if let Some(found) = object.get(key).and_then(Value::as_str) {
            return Some(found.to_owned());
        }
        for child in object.values() {
            if let Some(found) = find_string(child, key) {
                return Some(found);
            }
        }
    } else if let Some(items) = value.as_array() {
        for item in items {
            if let Some(found) = find_string(item, key) {
                return Some(found);
            }
        }
    }
    None
}

fn first_ref(value: &Value) -> Option<String> {
    if let Some(object) = value.as_object() {
        for key in ["ref_id", "refId", "ref", "qualified_ref", "qualifiedRef"] {
            if let Some(reference) = object
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| v.starts_with('@') && v.contains(":e"))
            {
                return Some(reference.to_owned());
            }
        }
        for child in object.values() {
            if let Some(reference) = first_ref(child) {
                return Some(reference);
            }
        }
    } else if let Some(items) = value.as_array() {
        for item in items {
            if let Some(reference) = first_ref(item) {
                return Some(reference);
            }
        }
    }
    None
}

fn snapshot_id_from_ref(reference: String) -> Option<String> {
    reference
        .strip_prefix('@')?
        .split_once(":e")
        .map(|(id, _)| id.to_owned())
}

fn scope_key(context: &SnapshotContext) -> String {
    if let Some(window) = &context.window_id {
        format!("window:{}", window.to_ascii_lowercase())
    } else if let Some(app) = &context.app {
        format!("app:{}", app.to_ascii_lowercase())
    } else {
        format!("surface:{}", context.surface.to_ascii_lowercase())
    }
}

fn latest_context_for(app: Option<&str>, window_id: Option<&str>) -> Option<SnapshotContext> {
    let scope = if let Some(window) = window_id {
        format!("window:{}", window.to_ascii_lowercase())
    } else if let Some(app) = app {
        format!("app:{}", app.to_ascii_lowercase())
    } else {
        return None;
    };
    let id = latest_by_scope().lock().ok()?.get(&scope).cloned()?;
    snapshots().lock().ok()?.get(&id).cloned()
}

fn context_for_ref(reference: &str) -> Result<(String, SnapshotContext), String> {
    validate_ref(reference)?;
    let id =
        snapshot_id_from_ref(reference.to_owned()).ok_or("Malformed Agent Desktop reference.")?;
    let context = snapshots()
        .lock()
        .map_err(|_| "Local desktop snapshot index is unavailable.")?
        .get(&id)
        .cloned()
        .ok_or(
            "This ref is not from a recent JEV snapshot. Take a fresh snapshot before acting.",
        )?;
    Ok((id, context))
}

fn context_for_fresh_snapshot(snapshot_id: &str) -> Result<SnapshotContext, String> {
    let context = snapshots()
        .lock()
        .map_err(|_| "Local desktop snapshot index is unavailable.")?
        .get(snapshot_id)
        .cloned()
        .ok_or("This snapshot is no longer available. Take a fresh snapshot before acting.")?;
    let latest = latest_by_scope()
        .lock()
        .map_err(|_| "Local desktop snapshot index is unavailable.")?
        .get(&scope_key(&context))
        .cloned();
    if latest.as_deref() != Some(snapshot_id) || context.captured_at.elapsed().as_secs() > 45 {
        return Err(
            "The observation is stale. Take a fresh snapshot of the same app/window before acting."
                .into(),
        );
    }
    Ok(context)
}

fn fresh_coordinate_context(snapshot_id: &str, x: i64, y: i64) -> Result<SnapshotContext, String> {
    let context = context_for_fresh_snapshot(snapshot_id)?;
    let bounds = context.bounds.ok_or("The latest snapshot has no window bounds; coordinate input is unavailable for this target.")?;
    let x = x as f64;
    let y = y as f64;
    if x < bounds.x || y < bounds.y || x >= bounds.x + bounds.width || y >= bounds.y + bounds.height
    {
        return Err(
            "Coordinates must be inside the target window bounds from the latest snapshot.".into(),
        );
    }
    Ok(context)
}

fn perform_ref_action(
    action: &str,
    reference: &str,
    text: Option<&str>,
    scroll: Option<(String, u32)>,
) -> Result<Value, String> {
    perform_ref_action_with_timeout(action, reference, text, scroll, DEFAULT_SNAPSHOT_TIMEOUT_MS)
}

fn perform_ref_action_with_timeout(
    action: &str,
    reference: &str,
    text: Option<&str>,
    scroll: Option<(String, u32)>,
    observation_timeout_ms: u32,
) -> Result<Value, String> {
    let (original_snapshot_id, context) = context_for_ref(reference)?;
    let latest_snapshot = latest_by_scope()
        .lock()
        .ok()
        .and_then(|map| map.get(&scope_key(&context)).cloned());
    if !snapshot_is_current(
        &original_snapshot_id,
        latest_snapshot.as_deref(),
        context.captured_at.elapsed(),
    ) {
        let observation = observe_target_with_timeout(&context, observation_timeout_ms);
        return Ok(json!({
            "kind":"desktop_action","action":action,"status":"not_performed",
            "reason":"The ref is stale. Use a ref from the fresh observation before acting.",
            "fresh_observation":observation
        }));
    }

    let command = if action == "type" {
        run_type_with_focus(
            reference,
            text.ok_or("Text is required for a type action.")?,
        )
    } else {
        run_command(&action_args(action, reference, text, scroll.as_ref())?)
    };
    if command.as_ref().is_ok_and(stale_ref_safe_to_retry) {
        let observation = observe_target_with_timeout(&context, observation_timeout_ms);
        return Ok(json!({
            "kind":"desktop_action","action":action,"status":"not_performed",
            "reason":"Agent Desktop refused the stale ref before delivery. Use a ref from the fresh observation.",
            "result":compact_command_result(&command),"fresh_observation":observation
        }));
    }

    let action_summary = compact_command_result(&command);
    let observation_started = Instant::now();
    let observation = observe_target_with_timeout(&context, observation_timeout_ms);
    let observation_elapsed_ms = elapsed_ms(observation_started);
    let status = match action_summary.get("delivery").and_then(Value::as_str) {
        Some("delivered_verified") => "completed",
        Some("delivered_unverified") => "completed_unverified",
        _ if command.as_ref().is_ok_and(command_ok) => "completed_unverified",
        _ => "failed_or_uncertain",
    };
    Ok(json!({
        "kind":"desktop_action",
        "action":action,
        "status":status,
        "ref":reference,
        "result":action_summary,
        "fresh_observation":observation,
        "observation_elapsed_ms":observation_elapsed_ms,
    }))
}

fn run_type_with_focus(reference: &str, text: &str) -> Result<CommandResult, String> {
    run_type_with_focus_using(reference, text, run_command)
}

fn run_type_with_focus_using<F>(
    reference: &str,
    text: &str,
    mut run: F,
) -> Result<CommandResult, String>
where
    F: FnMut(&[String]) -> Result<CommandResult, String>,
{
    let focus = run(&["focus".into(), reference.into(), "--headed".into()])?;
    let focus_elapsed_ms = focus.elapsed_ms;
    if !command_ok(&focus) {
        let mut failure = focus_precondition_failure(
            "FOCUS_FAILED",
            "Agent Desktop could not focus the exact target; no text was sent.",
            &focus,
        );
        attach_stage_timings(&mut failure, json!({"focus":focus_elapsed_ms}));
        return Ok(failure);
    }
    let focused = run(&[
        "is".into(),
        reference.into(),
        "--property".into(),
        "focused".into(),
    ])?;
    let focus_check_elapsed_ms = focused.elapsed_ms;
    if !command_ok(&focused) || focused_result(&focused) != Some(true) {
        let mut failure = focus_precondition_failure(
            "FOCUS_NOT_CONFIRMED",
            "The target did not report keyboard focus; no text was sent.",
            &focused,
        );
        attach_stage_timings(
            &mut failure,
            json!({"focus":focus_elapsed_ms,"focus_check":focus_check_elapsed_ms}),
        );
        return Ok(failure);
    }
    let mut typed = run(&action_args("type", reference, Some(text), None)?)?;
    if let Some(data) = typed
        .envelope
        .get_mut("data")
        .and_then(Value::as_object_mut)
    {
        data.insert("focus_verified_before_type".into(), Value::Bool(true));
    }
    let type_elapsed_ms = typed.elapsed_ms;
    attach_stage_timings(
        &mut typed,
        json!({
            "focus":focus_elapsed_ms,
            "focus_check":focus_check_elapsed_ms,
            "type":type_elapsed_ms
        }),
    );
    Ok(typed)
}

fn attach_stage_timings(command: &mut CommandResult, timings: Value) {
    if let Some(data) = command
        .envelope
        .get_mut("data")
        .and_then(Value::as_object_mut)
    {
        data.insert("stage_timings_ms".into(), timings);
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn snapshot_is_current(
    snapshot_id: &str,
    latest_snapshot_id: Option<&str>,
    age: std::time::Duration,
) -> bool {
    latest_snapshot_id == Some(snapshot_id) && age.as_secs() <= 45
}

fn focused_result(command: &CommandResult) -> Option<bool> {
    command
        .envelope
        .pointer("/data/result")
        .and_then(Value::as_bool)
}

fn focus_precondition_failure(code: &str, message: &str, cause: &CommandResult) -> CommandResult {
    let original_error = cause.envelope.get("error");
    let mut error = json!({
        "code": code,
        "message": message,
        "stage": "focus",
        "disposition": {"delivery":"not_delivered","retry":"safe"}
    });
    if let Some(error_object) = error.as_object_mut() {
        if let Some(original) = original_error {
            if let Some(cause_code) = original.get("code").and_then(Value::as_str) {
                error_object.insert(
                    "agent_desktop_code".into(),
                    Value::String(cause_code.to_owned()),
                );
            }
            if let Some(cause_message) = original.get("message").and_then(Value::as_str) {
                error_object.insert(
                    "agent_desktop_message".into(),
                    Value::String(short_text(cause_message)),
                );
            }
            if let Some(kind) = original.pointer("/details/kind").and_then(Value::as_str) {
                error_object.insert(
                    "agent_desktop_details_kind".into(),
                    Value::String(kind.to_owned()),
                );
            }
            if let Some(checks) = original
                .pointer("/details/checks")
                .and_then(Value::as_array)
            {
                let concise =
                    checks
                        .iter()
                        .filter_map(|check| {
                            let status = check.get("status").and_then(Value::as_str)?;
                            (status != "pass").then(|| json!({
                        "check":check.get("check"),
                        "status":status,
                        "reason":check.get("reason").and_then(Value::as_str).map(short_text)
                    }))
                        })
                        .take(6)
                        .collect::<Vec<_>>();
                if !concise.is_empty() {
                    error_object
                        .insert("agent_desktop_failed_checks".into(), Value::Array(concise));
                }
            }
            if let Some(recovery) = original.get("recovery") {
                error_object.insert("agent_desktop_recovery".into(), recovery.clone());
            }
        }
    }
    CommandResult {
        process_ok: false,
        elapsed_ms: cause.elapsed_ms,
        envelope: json!({"ok":false,"error":error}),
    }
}

fn action_args(
    action: &str,
    reference: &str,
    text: Option<&str>,
    scroll: Option<&(String, u32)>,
) -> Result<Vec<String>, String> {
    let mut args = vec![action.to_owned()];
    match action {
        "click" | "hover" => args.push(reference.to_owned()),
        "type" => {
            args.push(reference.to_owned());
            args.push(
                text.ok_or("Text is required for a type action.")?
                    .to_owned(),
            );
        }
        "scroll" => {
            let (direction, amount) = scroll.ok_or("Scroll direction and amount are required.")?;
            args.push(reference.to_owned());
            args.extend([
                "--direction".into(),
                direction.clone(),
                "--amount".into(),
                amount.to_string(),
            ]);
        }
        _ => return Err("Unsupported ref action.".into()),
    }
    // Prefer Windows UI Automation for ordinary ref clicks and scrolling.
    // Physical input is reserved for actions that require it (typing, hover)
    // or the explicit coordinate tools. Forcing every click through --headed
    // needlessly foregrounds the app and can make controls miss input.
    if matches!(action, "type" | "hover") {
        args.push("--headed".into());
    }
    Ok(args)
}

fn stale_ref_safe_to_retry(command: &CommandResult) -> bool {
    let Some(error) = command.envelope.get("error") else {
        return false;
    };
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let disposition = error.get("disposition");
    code == "STALE_REF"
        && disposition
            .and_then(|d| d.get("delivery"))
            .and_then(Value::as_str)
            == Some("not_delivered")
        && disposition
            .and_then(|d| d.get("retry"))
            .and_then(Value::as_str)
            == Some("safe")
}

#[derive(Clone, Debug)]
struct CommandResult {
    envelope: Value,
    process_ok: bool,
    elapsed_ms: u64,
}

fn command_ok(command: &CommandResult) -> bool {
    command.process_ok && command.envelope.get("ok").and_then(Value::as_bool) == Some(true)
}

fn compact_command_result(command: &Result<CommandResult, String>) -> Value {
    match command {
        Ok(command) => {
            let error = command.envelope.get("error");
            let disposition = command
                .envelope
                .pointer("/data/disposition")
                .or_else(|| error.and_then(|e| e.get("disposition")));
            let verification = command
                .envelope
                .pointer("/data/verification")
                .or_else(|| command.envelope.pointer("/data/details/verification"));
            let steps =
                command
                    .envelope
                    .pointer("/data/steps")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items.iter().take(6).map(|item| json!({
                    "label":item.get("label"),"outcome":item.get("outcome"),
                    "mechanism":item.get("mechanism"),"verified":item.get("verified")
                })).collect::<Vec<_>>()
                    });
            let surfaces = command
                .envelope
                .pointer("/data/surfaces")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .take(6)
                        .filter_map(|item| {
                            item.as_str().map(str::to_owned).or_else(|| {
                                ["surface", "kind", "type"].iter().find_map(|key| {
                                    item.get(*key).and_then(Value::as_str).map(str::to_owned)
                                })
                            })
                        })
                        .collect::<Vec<_>>()
                });
            json!({
                "ok":command_ok(command),
                "code":error.and_then(|e| e.get("code")).and_then(Value::as_str),
                "message":error.and_then(|e| e.get("message")).and_then(Value::as_str).map(short_text),
                "stage":error.and_then(|e| e.get("stage")).and_then(Value::as_str),
                "agent_desktop_code":error.and_then(|e| e.get("agent_desktop_code")).and_then(Value::as_str),
                "agent_desktop_message":error.and_then(|e| e.get("agent_desktop_message")).and_then(Value::as_str).map(short_text),
                "agent_desktop_details_kind":error.and_then(|e| e.get("agent_desktop_details_kind")).and_then(Value::as_str),
                "agent_desktop_failed_checks":error.and_then(|e| e.get("agent_desktop_failed_checks")),
                "delivery":disposition.and_then(|d| d.get("delivery")).and_then(Value::as_str),
                "retry":disposition.and_then(|d| d.get("retry")).and_then(Value::as_str),
                "verification_scope":verification.and_then(|v| v.get("scope")).and_then(Value::as_str)
                    .or_else(|| command.envelope.pointer("/data/verification_scope").and_then(Value::as_str))
                    .or_else(|| error.and_then(|e| e.pointer("/details/verification_scope")).and_then(Value::as_str)),
                "verification_reason":verification.and_then(|v| v.get("reason")).and_then(Value::as_str)
                    .or_else(|| command.envelope.pointer("/data/verification_reason").and_then(Value::as_str))
                    .or_else(|| error.and_then(|e| e.pointer("/details/verification_reason")).and_then(Value::as_str)),
                "focus_verified_before_type":command.envelope.pointer("/data/focus_verified_before_type").and_then(Value::as_bool),
                "process_elapsed_ms":command.elapsed_ms,
                "stage_timings_ms":command.envelope.pointer("/data/stage_timings_ms"),
                "process_elapsed_ms":command.elapsed_ms,
                "stage_timings_ms":command.envelope.pointer("/data/stage_timings_ms"),
                "steps":steps,
                "surfaces_opened":surfaces,
            })
        }
        Err(error) => {
            json!({"ok":false,"delivery":"unknown","retry":"never_without_observation","message":short_text(error)})
        }
    }
}

fn short_text(text: &str) -> String {
    text.chars().take(240).collect()
}

fn observe_target(context: &SnapshotContext) -> Value {
    observe_target_with_timeout(context, DEFAULT_SNAPSHOT_TIMEOUT_MS)
}

fn observe_target_with_timeout(context: &SnapshotContext, timeout_ms: u32) -> Value {
    let (interactive_only, compact, skeleton, max_depth) = action_refresh_snapshot_params();
    let snapshot = |surface: &str, app: Option<&str>, window_id: Option<&str>| {
        snapshot_target_options(
            surface,
            app,
            window_id,
            interactive_only,
            compact,
            skeleton,
            max_depth,
            timeout_ms,
            false,
        )
    };
    let primary = snapshot(
        &context.surface,
        context.app.as_deref(),
        context.window_id.as_deref(),
    );
    let result = match primary {
        Ok(snapshot) => Ok(snapshot),
        Err(error) if context.surface != "window" => snapshot(
            "window",
            context.app.as_deref(),
            context.window_id.as_deref(),
        )
        .or_else(|_| {
            if context.app.is_none() && context.window_id.is_none() {
                snapshot("focused", None, None)
            } else {
                Err(error)
            }
        }),
        Err(error)
            if context.app.is_none()
                && context.window_id.is_none()
                && context.surface != "focused" =>
        {
            snapshot("focused", None, None).map_err(|_| error)
        }
        Err(error) => Err(error),
    };
    match result {
        Ok(snapshot) => snapshot_summary(&snapshot.json),
        Err(error) => {
            json!({"kind":"desktop_snapshot","ok":false,"error":short_text(&error),"previous_snapshot_id":context.snapshot_id})
        }
    }
}

fn action_result(
    action: &str,
    command: Result<CommandResult, String>,
    context: SnapshotContext,
) -> Value {
    action_result_with_timeout(action, command, context, DEFAULT_SNAPSHOT_TIMEOUT_MS)
}

fn action_result_with_timeout(
    action: &str,
    command: Result<CommandResult, String>,
    context: SnapshotContext,
    observation_timeout_ms: u32,
) -> Value {
    let result = compact_command_result(&command);
    let observation_started = Instant::now();
    let observation = observe_target_with_timeout(&context, observation_timeout_ms);
    let observation_elapsed_ms = elapsed_ms(observation_started);
    let status = match result.get("delivery").and_then(Value::as_str) {
        Some("delivered_verified") => "completed",
        Some("delivered_unverified") => "completed_unverified",
        _ if command.as_ref().is_ok_and(command_ok) => "completed_unverified",
        _ => "failed_or_uncertain",
    };
    json!({
        "kind":"desktop_action",
        "action":action,
        "status":status,
        "result":result,
        "fresh_observation":observation,
        "observation_elapsed_ms":observation_elapsed_ms,
    })
}

fn validate_ref_snapshot(snapshot_id: &str, reference: &str) -> Result<(), String> {
    let (referenced_snapshot, _element_id) = reference
        .trim()
        .strip_prefix('@')
        .and_then(|rest| rest.split_once(":e"))
        .filter(|(snapshot, element)| {
            !snapshot.is_empty()
                && !element.is_empty()
                && element.chars().all(|ch| ch.is_ascii_digit())
        })
        .ok_or("Use a complete qualified ref from a desktop snapshot.")?;
    if snapshot_id.trim() != referenced_snapshot {
        return Err("The ref does not belong to the supplied snapshot_id. Take a fresh snapshot and use its ref.".into());
    }
    Ok(())
}

pub fn screenshot(app: Option<&str>, window_id: Option<&str>) -> Result<Value, String> {
    let folder = crate::chat_store::captures_dir().join("agent-desktop");
    fs::create_dir_all(&folder)
        .map_err(|error| format!("Could not create local screenshot folder {}: {error}", folder.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    let path = folder.join(format!("screen-{stamp}.png"));
    let mut args = vec![
        "screenshot".to_owned(),
        path.to_string_lossy().to_string(),
        "--headed".to_owned(),
    ];
    push_optional(&mut args, "--app", app)?;
    push_optional(&mut args, "--window-id", window_id)?;
    let result = run(&args)?;
    Ok(serde_json::json!({"local_path":path.to_string_lossy(),"result":result}))
}

fn validate_ref(reference: &str) -> Result<(), String> {
    let reference = reference.trim();
    if !reference.starts_with('@') || !reference.contains(":e") {
        return Err(
            "Use a complete qualified ref from the latest snapshot, such as @snapshot-id:e12."
                .to_owned(),
        );
    }
    Ok(())
}

fn nonempty<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 200 {
        return Err(format!("Choose a non-empty {label} under 200 characters."));
    }
    Ok(value)
}

fn push_optional(args: &mut Vec<String>, flag: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
        if value.len() > 500 || value.contains('\0') {
            return Err("A desktop target argument was too long or invalid.".into());
        }
        args.push(flag.to_owned());
        args.push(value.to_owned());
    }
    Ok(())
}

fn run(args: &[String]) -> Result<Value, String> {
    let command = run_command(args)?;
    if !command_ok(&command) {
        let detail = command
            .envelope
            .get("error")
            .map(Value::to_string)
            .unwrap_or_else(|| "Agent Desktop rejected the command.".to_owned());
        return Err(format!(
            "agent-desktop command failed: {}",
            short_text(&detail)
        ));
    }
    Ok(command.envelope)
}

fn run_command(args: &[String]) -> Result<CommandResult, String> {
    let binary = resolve_binary()?;
    let started = Instant::now();
    let mut child = Command::new(&binary);
    child.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Agent Desktop is a separate CLI process. Keep its console hidden while
        // retaining its structured stdout/stderr pipes for JEV.
        child.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = child.output().map_err(|error| {
        format!(
            "Could not run agent-desktop at {}: {error}",
            binary.display()
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        format!(
            "agent-desktop returned invalid JSON ({error}). {}",
            stderr.trim()
        )
    })?;
    Ok(CommandResult {
        envelope: value,
        process_ok: output.status.success(),
        elapsed_ms: elapsed_ms(started),
    })
}

fn resolve_binary() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("JARVUS_AGENT_DESKTOP_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "JARVUS_AGENT_DESKTOP_BIN points to a missing file: {}",
            path.display()
        ));
    }
    // On macOS and Linux, npm/Homebrew expose the native CLI on PATH. Prefer
    // that entry point: it resolves the matching platform helper bundled next
    // to Agent Desktop, including Apple Silicon and Intel builds.
    #[cfg(not(windows))]
    if let Some(binary) = find_on_path("agent-desktop") {
        return Ok(binary);
    }
    if let Some(app_data) = env::var_os("APPDATA") {
        let binary = PathBuf::from(app_data)
            .join("npm")
            .join("node_modules")
            .join("agent-desktop")
            .join("bin")
            .join("agent-desktop-win32-x64.exe");
        if binary.is_file() {
            return Ok(binary);
        }
        let arm_binary = binary.with_file_name("agent-desktop-win32-arm64.exe");
        if arm_binary.is_file() {
            return Ok(arm_binary);
        }
    }
    Err("agent-desktop was not found. Install it with `npm install -g agent-desktop` (or `brew install agent-desktop` on macOS), or set JARVUS_AGENT_DESKTOP_BIN to its native executable.".to_owned())
}

/// Where a CLI can live on macOS/Linux even though a GUI app inherits only a bare PATH:
/// next to the app (a bundled copy), then Homebrew, npm, cargo and the system.
#[cfg(not(windows))]
fn find_on_path(name: &str) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            directories.push(dir.to_path_buf());
            // JARVUS.app/Contents/MacOS/jarvus -> JARVUS.app/Contents/Resources
            directories.push(dir.join("../Resources"));
        }
    }
    if let Some(path) = env::var_os("PATH") {
        directories.extend(env::split_paths(&path));
    }
    for fixed in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        directories.push(PathBuf::from(fixed));
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        for relative in [".npm-global/bin", ".local/bin", ".cargo/bin", ".volta/bin", "n/bin"] {
            directories.push(home.join(relative));
        }
    }
    directories.into_iter().map(|directory| directory.join(name)).find(|candidate| candidate.is_file())
}

/// Open Agent Desktop's own macOS permission guide. This is the supported
/// upstream flow for Accessibility and Screen Recording grants; JARVUS does
/// not implement a parallel accessibility engine or manipulate TCC settings.
pub fn permissions_guide() -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        return run(&["permissions".into(), "--guide".into()]);
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("The Agent Desktop permission guide is only available on macOS.".into())
    }
}

/// Ask Agent Desktop to initiate its supported macOS permission flow. The
/// upstream CLI owns the Accessibility/Screen Recording prompts and TCC flow.
pub fn request_permissions() -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        return run(&["permissions".into(), "--request".into()]);
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Agent Desktop permission requests are only available on macOS.".into())
    }
}

pub fn permission_status() -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        return run(&["permissions".into()]);
    }
    #[cfg(not(target_os = "macos"))]
    {
        status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_snapshot_defaults_are_full_uncompacted_and_deep() {
        assert_eq!(DEFAULT_SNAPSHOT_MAX_DEPTH, 32);
        assert_eq!(DEFAULT_SNAPSHOT_TIMEOUT_MS, 30_000);
        let args = snapshot_command_args(
            "window",
            Some("Discord"),
            Some("w-1"),
            false,
            false,
            false,
            DEFAULT_SNAPSHOT_MAX_DEPTH,
            DEFAULT_SNAPSHOT_TIMEOUT_MS,
        )
        .unwrap();
        assert!(args.windows(2).any(|pair| pair == ["--max-depth", "32"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--timeout-ms", "30000"])
        );
        assert!(!args.contains(&"--compact".to_owned()));
        assert!(!args.contains(&"--skeleton".to_owned()));
        assert!(!args.contains(&"--interactive-only".to_owned()));
        let narrowed =
            snapshot_command_args("window", None, None, true, true, true, 8, 3000).unwrap();
        assert!(narrowed.contains(&"--compact".to_owned()));
        assert!(narrowed.contains(&"--skeleton".to_owned()));
        assert!(narrowed.contains(&"--interactive-only".to_owned()));
    }

    #[test]
    fn snapshot_tool_schema_matches_full_tree_defaults_and_target_hint_contract() {
        let tools: Value = serde_json::from_str(include_str!("toolset.json")).unwrap();
        let snapshot = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "desktop_snapshot")
            .unwrap();
        let properties = &snapshot["parameters"]["properties"];
        assert_eq!(properties["interactive_only"]["default"], false);
        assert_eq!(properties["compact"]["default"], false);
        assert_eq!(properties["skeleton"]["default"], false);
        assert_eq!(properties["filter"]["default"], "all");
        assert_eq!(properties["max_depth"]["default"], 32);
        assert_eq!(properties["timeout_ms"]["default"], 30_000);
        assert!(properties.get("target_text").is_some());
    }

    #[test]
    fn target_text_resolves_one_same_snapshot_actionable_row_parent() {
        let tree = json!({"role":"window","children":[
            {"role":"listitem","name":"DM row","ref_id":"@label-unique:e1","children":[
                {"role":"statictext","name":"My goddess","ref_id":"@label-unique:e2","value":"private message must stay local"}
            ]}
        ]});
        let envelope = json!({"snapshot_id":"label-unique","data":{"complete":true,"ref_count":2,"tree":tree}});
        remember_snapshot(&envelope, "window", Some("Discord"), Some("window-1"));

        let summary = snapshot_summary_for_target(&envelope, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/status")
                .and_then(Value::as_str),
            Some("resolved")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/action_ref")
                .and_then(Value::as_str),
            Some("@label-unique:e1")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/actionable_ancestor_count")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert!(
            !summary
                .to_string()
                .contains("private message must stay local")
        );
    }

    #[test]
    fn target_text_reports_duplicate_matching_rows_as_ambiguous() {
        let tree = json!({"role":"window","children":[
            {"role":"listitem","name":"DM row one","ref_id":"@label-duplicate:e1","children":[{"role":"statictext","name":"My goddess"}]},
            {"role":"listitem","name":"DM row two","ref_id":"@label-duplicate:e2","children":[{"role":"statictext","name":"My goddess"}]}
        ]});
        let envelope = json!({"snapshot_id":"label-duplicate","data":{"complete":true,"ref_count":2,"tree":tree}});
        remember_snapshot(&envelope, "window", Some("Discord"), Some("window-2"));

        let summary = snapshot_summary_for_target(&envelope, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/status")
                .and_then(Value::as_str),
            Some("unresolved")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/reason")
                .and_then(Value::as_str),
            Some("multiple_matching_labels")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/actionable_ancestor_count")
                .and_then(Value::as_u64),
            Some(2)
        );
        assert!(summary.pointer("/target_resolution/action_ref").is_none());
    }

    #[test]
    fn target_text_reports_multiple_actionable_ancestors_as_ambiguous() {
        let tree = json!({"role":"window","children":[
            {"role":"listitem","name":"DM row","ref_id":"@label-nested:e1","children":[
                {"role":"button","name":"My goddess","ref_id":"@label-nested:e2"}
            ]}
        ]});
        let envelope = json!({"snapshot_id":"label-nested","data":{"complete":true,"tree":tree}});
        remember_snapshot(&envelope, "window", Some("Discord"), Some("window-2a"));

        let summary = snapshot_summary_for_target(&envelope, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/status")
                .and_then(Value::as_str),
            Some("unresolved")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/reason")
                .and_then(Value::as_str),
            Some("multiple_actionable_ancestors")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/actionable_ancestor_count")
                .and_then(Value::as_u64),
            Some(2)
        );
        assert!(summary.pointer("/target_resolution/action_ref").is_none());
    }

    #[test]
    fn target_text_without_local_snapshot_reports_missing_tree() {
        let envelope = json!({"snapshot_id":"label-not-retained","data":{"complete":true,"tree":{
            "role":"window","children":[{"role":"button","name":"My goddess","ref_id":"@label-not-retained:e1"}]
        }}});
        let summary = snapshot_summary_for_target(&envelope, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/status")
                .and_then(Value::as_str),
            Some("unresolved")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/reason")
                .and_then(Value::as_str),
            Some("snapshot_not_retained")
        );
        assert!(summary.pointer("/target_resolution/action_ref").is_none());
    }

    #[test]
    fn target_text_without_actionable_ancestor_fails_closed_with_diagnostics() {
        let tree = json!({"role":"window","children":[
            {"role":"group","name":"DM row","children":[{"role":"statictext","name":"My goddess"}]}
        ]});
        let envelope = json!({"snapshot_id":"label-inert","data":{"complete":true,"tree":tree}});
        remember_snapshot(&envelope, "window", Some("Discord"), Some("window-3"));

        let summary = snapshot_summary_for_target(&envelope, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/status")
                .and_then(Value::as_str),
            Some("unresolved")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/reason")
                .and_then(Value::as_str),
            Some("no_actionable_ancestor")
        );
        assert_eq!(
            summary
                .pointer("/target_resolution/match_count")
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn target_text_rejects_wrong_snapshot_refs_and_incomplete_trees() {
        let wrong_snapshot = json!({"role":"window","children":[
            {"role":"button","name":"My goddess","ref_id":"@other-snapshot:e1"}
        ]});
        let full =
            json!({"snapshot_id":"label-wrong-ref","data":{"complete":true,"tree":wrong_snapshot}});
        remember_snapshot(&full, "window", Some("Discord"), Some("window-4"));
        let summary = snapshot_summary_for_target(&full, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/reason")
                .and_then(Value::as_str),
            Some("no_actionable_ancestor")
        );
        assert!(summary.pointer("/target_resolution/action_ref").is_none());

        let incomplete = json!({"snapshot_id":"label-incomplete","data":{"complete":false,"tree":{
            "role":"window","children":[{"role":"button","name":"My goddess","ref_id":"@label-incomplete:e1"}]
        }}});
        remember_snapshot(&incomplete, "window", Some("Discord"), Some("window-5"));
        let summary = snapshot_summary_for_target(&incomplete, "all", Some("My goddess")).unwrap();
        assert_eq!(
            summary
                .pointer("/target_resolution/reason")
                .and_then(Value::as_str),
            Some("snapshot_incomplete")
        );
        assert!(summary.pointer("/target_resolution/action_ref").is_none());
    }

    #[test]
    fn target_text_prioritizes_visible_dm_ref_beyond_default_summary_limit_and_keeps_tree_local() {
        let mut children = (0..300)
            .map(|index| {
                json!({
                    "ref_id":format!("@target-snapshot:e{index}"),
                    "role":"list item",
                    "name":format!("Conversation {index}"),
                    "value":"private local content"
                })
            })
            .collect::<Vec<_>>();
        children.push(json!({
            "ref_id":"@target-snapshot:e300",
            "role":"list item",
            "name":"525i",
            "value":"private local content"
        }));
        let full = json!({
            "snapshot_id":"target-snapshot",
            "data":{"complete":true,"ref_count":301,"tree":{"role":"window","children":children}}
        });
        remember_snapshot(&full, "window", Some("Discord"), Some("w-target"));
        let summary = snapshot_summary_for_target(&full, "all", Some("525i")).unwrap();
        assert_eq!(summary["local_tree_retained"], true);
        assert_eq!(summary["target_match_count"], 1);
        assert_eq!(summary["elements"][0]["name"], "525i");
        assert_eq!(summary["elements"][0]["ref"], "@target-snapshot:e300");
        assert!(summary["elements"].as_array().unwrap().len() <= MAX_SUMMARY_ELEMENTS);
        assert!(!summary.to_string().contains("private local content"));
    }

    #[test]
    fn explicit_filter_is_applied_only_when_requested() {
        let full = json!({
            "snapshot_id":"explicit-filter",
            "data":{"complete":true,"ref_count":2,"tree":{"role":"window","children":[
                {"ref_id":"@explicit-filter:e1","role":"text","name":"525i"},
                {"ref_id":"@explicit-filter:e2","role":"button","name":"Open DM"}
            ]}}
        });
        remember_snapshot(&full, "window", Some("Discord"), Some("w-filter"));
        let all = snapshot_summary_for_target(&full, "all", Some("525i")).unwrap();
        let buttons = snapshot_summary_for_target(&full, "buttons", None).unwrap();
        assert!(
            all["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "525i")
        );
        assert_eq!(buttons["elements"].as_array().unwrap().len(), 1);
        assert_eq!(buttons["elements"][0]["name"], "Open DM");
    }

    #[test]
    fn type_ref_must_belong_to_the_supplied_snapshot() {
        assert!(validate_ref_snapshot("s8f3k2p9", "@s8f3k2p9:e12").is_ok());
        assert!(validate_ref_snapshot("s8f3k2p9", "@other:e12").is_err());
        assert!(validate_ref_snapshot("s8f3k2p9", "@e12").is_err());
        assert!(validate_ref_snapshot("s8f3k2p9", "@s8f3k2p9:e12x").is_err());
        assert!(validate_ref_snapshot("s8f3k2p9", "@s8f3k2p9:e12",).is_ok());
    }

    #[test]
    fn a_ref_from_an_older_snapshot_is_never_actionable() {
        assert!(snapshot_is_current(
            "fresh",
            Some("fresh"),
            std::time::Duration::from_secs(1)
        ));
        assert!(!snapshot_is_current(
            "old",
            Some("fresh"),
            std::time::Duration::from_secs(1)
        ));
        assert!(!snapshot_is_current(
            "fresh",
            Some("fresh"),
            std::time::Duration::from_secs(46)
        ));
    }

    #[test]
    fn snapshot_filters_default_to_all_and_select_role_categories() {
        let button = ElementHint {
            reference: Some("@s1:e1".into()),
            role: "button".into(),
            name: "Send".into(),
            bounds: None,
        };
        let textbox = ElementHint {
            reference: Some("@s1:e2".into()),
            role: "text field".into(),
            name: "Message".into(),
            bounds: None,
        };
        assert_eq!(normalize_snapshot_filter("all").unwrap(), "all");
        assert_eq!(normalize_snapshot_filter("fields").unwrap(), "editables");
        assert!(filter_hint("all", &button));
        assert!(filter_hint("buttons", &button));
        assert!(!filter_hint("buttons", &textbox));
        assert!(filter_hint("editables", &textbox));
        assert!(normalize_snapshot_filter("unsupported").is_err());
    }

    #[test]
    fn snapshot_timeout_is_bounded_for_slow_accessibility_trees() {
        assert!(validate_snapshot_timeout(1).is_ok());
        assert!(validate_snapshot_timeout(30_000).is_ok());
        assert!(validate_snapshot_timeout(120_000).is_ok());
        assert!(validate_snapshot_timeout(0).is_err());
        assert!(validate_snapshot_timeout(120_001).is_err());
    }

    #[test]
    fn edit_role_fallback_only_runs_after_a_successful_empty_search() {
        assert!(should_retry_edit_role(
            Some("edit"),
            &json!({"data":{"matches":[]}})
        ));
        assert!(should_retry_edit_role(
            Some(" EDIT "),
            &json!({"data":{"matches":[]}})
        ));
        assert!(!should_retry_edit_role(
            Some("edit"),
            &json!({"data":{"matches":[{}]}})
        ));
        assert!(!should_retry_edit_role(
            Some("textfield"),
            &json!({"data":{"matches":[]}})
        ));
        assert!(!should_retry_edit_role(Some("edit"), &json!({"data":{}})));
    }

    #[test]
    fn type_requires_confirmed_focus_before_sending_text() {
        let mut calls = Vec::new();
        let result = run_type_with_focus_using("@s1:e2", "safe test", |args| {
            calls.push(args[0].clone());
            let envelope = match args[0].as_str() {
                "focus" => {
                    json!({"ok":true,"data":{"disposition":{"delivery":"delivered_verified"}}})
                }
                "is" => json!({"ok":true,"data":{"result":true}}),
                "type" => {
                    json!({"ok":true,"data":{"disposition":{"delivery":"delivered_unverified"}}})
                }
                other => panic!("unexpected command {other}"),
            };
            Ok(CommandResult {
                process_ok: true,
                envelope,
                elapsed_ms: 10,
            })
        })
        .unwrap();
        assert_eq!(calls, vec!["focus", "is", "type"]);
        assert_eq!(
            result
                .envelope
                .pointer("/data/focus_verified_before_type")
                .and_then(Value::as_bool),
            Some(true)
        );

        let mut calls = Vec::new();
        let result = run_type_with_focus_using("@s1:e2", "safe test", |args| {
            calls.push(args[0].clone());
            Ok(CommandResult {
                process_ok: true,
                envelope: json!({"ok":true,"data":{"result":false}}),
                elapsed_ms: 20,
            })
        })
        .unwrap();
        assert_eq!(calls, vec!["focus", "is"]);
        assert_eq!(
            result
                .envelope
                .pointer("/error/code")
                .and_then(Value::as_str),
            Some("FOCUS_NOT_CONFIRMED")
        );
        assert_eq!(
            result
                .envelope
                .pointer("/error/disposition/delivery")
                .and_then(Value::as_str),
            Some("not_delivered")
        );
    }

    #[test]
    fn send_refresh_uses_short_deadline_and_narrowed_tree() {
        assert!(SEND_REFRESH_TIMEOUT_MS < DEFAULT_SNAPSHOT_TIMEOUT_MS);
        let (interactive_only, compact, skeleton, max_depth) = action_refresh_snapshot_params();
        let args = snapshot_command_args(
            "window",
            Some("Discord"),
            Some("w-1"),
            interactive_only,
            compact,
            skeleton,
            max_depth,
            SEND_REFRESH_TIMEOUT_MS,
        )
        .unwrap();
        assert!(args.windows(2).any(|pair| pair == ["--timeout-ms", "3000"]));
        assert!(args.contains(&"--compact".to_owned()));
        assert!(args.contains(&"--interactive-only".to_owned()));
    }

    #[test]
    fn fuzzy_label_score_prefers_fewest_extra_words_and_ignores_symbols() {
        let server = label_score("hxstile", "ネ Hxstile #solo", false);
        assert!(
            server
                > label_score(
                    "hxstile",
                    "Hxstile mod gc part 1 (group message), 9 Members, Muted",
                    false
                )
        );
        assert!(server > label_score("hxstile", "Iris's VC / ネ Hxstile #solo 16:35:35", false));
        assert!(label_score("chat", "💬│chat (text channel)", false) > 0);
        assert_eq!(label_score("my goddess", "My goddess", false), 1000);
        assert_eq!(
            label_score("goddess", "Message @My goddess2", true),
            0,
            "whole words only for recipients"
        );
        assert!(label_score("goddess", "Message @My goddess", true) > 0);
        assert_eq!(label_score("hxstile", "Ferrum", false), 0);
        assert!(
            label_score(
                "chat",
                "unread, ︱chat (text channel), Private Channel (locked)",
                false
            ) > label_score("chat", "Open Chat", false),
            "the name part beats a label that merely contains the word"
        );
    }

    #[test]
    fn plain_text_label_clicks_the_clickable_element_of_its_row() {
        // Discord server sidebar: the clickable icon has no name; the name sits in
        // a hidden 2x2 tooltip anchor beside it.
        let tree = json!({"role":"window","bounds":{"x":0.0,"y":0.0,"width":1920.0,"height":1040.0},"children":[
            {"role":"group","bounds":{"x":11.0,"y":289.0,"width":59.0,"height":37.0},"children":[
                {"role":"group","ref_id":"@s:e29","available_actions":["Click"],"bounds":{"x":22.0,"y":289.0,"width":37.0,"height":37.0}},
                {"role":"group","ref_id":"@s:e31","bounds":{"x":40.0,"y":289.0,"width":2.0,"height":2.0},"children":[
                    {"role":"statictext","name":"ネ Hxstile #solo","bounds":{"x":40.0,"y":289.0,"width":103.0,"height":19.0}}]}]},
            {"role":"link","name":"Hxstile mod gc part 1 (group message), 9 Members","available_actions":["Click"],
             "ref_id":"@s:e385","bounds":{"x":89.0,"y":565.0,"width":244.0,"height":39.0}},
            {"role":"link","name":"ネ Hxstile #solo","available_actions":["Click"],"states":["offscreen"],
             "bounds":{"x":89.0,"y":4000.0,"width":200.0,"height":30.0}}
        ]});
        let window = tree.get("bounds").and_then(parse_bounds);
        let found = locate_in_tree(&tree, window, "hxstile", None, false).unwrap();
        assert_eq!(
            (found.x, found.y),
            (41, 308),
            "center of the clickable server icon"
        );
        assert_eq!(found.reference.as_deref(), Some("@s:e29"));
    }

    #[test]
    #[ignore]
    fn live_locate_against_saved_tree() {
        // JARVUS_TREE=<agent-desktop snapshot json> cargo test --bin jarvus live_locate -- --ignored --nocapture
        let path = std::env::var("JARVUS_TREE").unwrap();
        let snapshot: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let tree = snapshot.pointer("/data/tree").unwrap();
        let window = tree.get("bounds").and_then(parse_bounds);
        for query in [
            "hxstile",
            "my goddess",
            "millionaires",
            "inbox",
            "karma",
            "strong",
            "chat",
            "welcome",
            "laws",
        ] {
            println!(
                "{query:>14} -> {:?}",
                locate_in_tree(tree, window, query, None, false)
            );
            println!(
                "{:>14} -> {:?}",
                "  in servers",
                locate_in_tree(tree, window, query, Some("servers"), false)
            );
            println!(
                "{:>14} -> {:?}",
                "  in channels",
                locate_in_tree(tree, window, query, Some("channels"), false)
            );
        }
        println!(
            "{:>14} -> {:?}",
            "text:goddess",
            locate_in_tree(tree, window, "goddess", None, true)
        );
    }

    #[test]
    fn message_composer_matches_exact_recipient_only() {
        let snapshot = json!({"data":{"tree":{"role":"window","name":"@My goddess - Discord","children":[
            {"role":"textfield","name":"Message @My goddess2","ref_id":"@s:e1"},
            {"role":"textfield","name":"Message @My goddess","ref_id":"@s:e2","states":["focused"]},
            {"role":"combobox","name":"Search","ref_id":"@s:e3"}
        ]}}});
        assert_eq!(
            composer_for(&snapshot, "my goddess").as_deref(),
            Some("@s:e2")
        );
        assert_eq!(
            composer_for(&snapshot, "  My   Goddess ").as_deref(),
            Some("@s:e2")
        );
        assert_eq!(
            composer_for(&snapshot, "goddess"),
            None,
            "partial names must not match"
        );
        assert_eq!(composer_for(&snapshot, "someone else"), None);
        assert_eq!(focused_text_input(&snapshot).as_deref(), Some("@s:e2"));
    }

    #[test]
    fn post_action_refresh_stays_interactive_only_and_compact_for_speed() {
        // Every click/type/scroll/press refresh goes through this helper. A full,
        // non-interactive walk of a Chromium/Electron accessibility tree is what
        // made each tool call take many seconds; this keeps the automatic
        // post-action refresh cheap regardless of how deep the target tree is.
        let (interactive_only, compact, skeleton, _max_depth) = action_refresh_snapshot_params();
        assert!(interactive_only);
        assert!(compact);
        assert!(!skeleton);
    }

    #[test]
    fn focus_type_pipeline_records_each_local_process_duration_without_text() {
        let result = run_type_with_focus_using("@s1:e2", "private draft", |args| {
            let elapsed_ms = match args[0].as_str() {
                "focus" => 11,
                "is" => 22,
                "type" => 33,
                _ => 0,
            };
            let envelope = match args[0].as_str() {
                "focus" => {
                    json!({"ok":true,"data":{"disposition":{"delivery":"delivered_verified"}}})
                }
                "is" => json!({"ok":true,"data":{"result":true}}),
                "type" => {
                    json!({"ok":true,"data":{"disposition":{"delivery":"delivered_verified"}}})
                }
                _ => json!({"ok":false}),
            };
            Ok(CommandResult {
                process_ok: true,
                envelope,
                elapsed_ms,
            })
        })
        .unwrap();
        assert_eq!(
            result
                .envelope
                .pointer("/data/stage_timings_ms/focus")
                .and_then(Value::as_u64),
            Some(11)
        );
        assert_eq!(
            result
                .envelope
                .pointer("/data/stage_timings_ms/focus_check")
                .and_then(Value::as_u64),
            Some(22)
        );
        assert_eq!(
            result
                .envelope
                .pointer("/data/stage_timings_ms/type")
                .and_then(Value::as_u64),
            Some(33)
        );
        assert!(!result.envelope.to_string().contains("private draft"));
    }

    #[test]
    fn compound_send_returns_local_stage_durations_without_echoing_text() {
        let result = send_with_actions(
            || Ok(json!({
                "result":{"ok":true,"delivery":"delivered_verified","focus_verified_before_type":true,
                    "stage_timings_ms":{"focus":11,"focus_check":22,"type":33},"process_elapsed_ms":33},
                "observation_elapsed_ms":44,
                "fresh_observation":{"snapshot_id":"after-type"}
            })),
            |_| Ok(json!({"result":{"delivery":"delivered_unverified","process_elapsed_ms":55},
                "observation_elapsed_ms":66,"fresh_observation":{"snapshot_id":"after-submit"}})),
        ).unwrap();
        assert_eq!(
            result.pointer("/timings_ms/focus").and_then(Value::as_u64),
            Some(11)
        );
        assert_eq!(
            result
                .pointer("/timings_ms/focus_check")
                .and_then(Value::as_u64),
            Some(22)
        );
        assert_eq!(
            result.pointer("/timings_ms/type").and_then(Value::as_u64),
            Some(33)
        );
        assert_eq!(
            result
                .pointer("/timings_ms/post_type_observation")
                .and_then(Value::as_u64),
            Some(44)
        );
        assert_eq!(
            result.pointer("/timings_ms/submit").and_then(Value::as_u64),
            Some(55)
        );
        assert_eq!(
            result
                .pointer("/timings_ms/post_submit_observation")
                .and_then(Value::as_u64),
            Some(66)
        );
        assert!(!result.to_string().contains("private draft"));
    }

    #[test]
    fn compact_action_result_keeps_delivery_without_echoing_field_content() {
        let command = Ok(CommandResult {
            process_ok: true,
            elapsed_ms: 42,
            envelope: json!({
                "ok": true,
                "data": {
                    "disposition": { "delivery": "delivered_unverified", "retry": "never" },
                    "verification_scope": "element_value",
                    "verification_reason": "postcondition_mismatch",
                    "post_state": { "value": "private message text" }
                }
            }),
        });

        let compact = compact_command_result(&command);
        assert_eq!(compact["delivery"], "delivered_unverified");
        assert_eq!(compact["verification_scope"], "element_value");
        assert_eq!(compact["verification_reason"], "postcondition_mismatch");
        assert!(!compact.to_string().contains("private message text"));
    }

    #[test]
    fn compound_send_submits_after_successful_type_with_postcondition_mismatch() {
        let mut pressed_snapshots = Vec::new();
        let result = send_with_actions(
            || Ok(json!({
                "result":{
                    "ok":true,
                    "delivery":"delivered_unverified",
                    "verification_scope":"element_value",
                    "verification_reason":"postcondition_mismatch",
                    "focus_verified_before_type":true,
                    "post_state":{"value":"private composer draft"}
                },
                "fresh_observation":{"snapshot_id":"fresh-target-window"}
            })),
            |snapshot_id| {
                pressed_snapshots.push(snapshot_id.to_owned());
                Ok(json!({"result":{"delivery":"delivered_verified"},"fresh_observation":{"snapshot_id":"after-return"}}))
            },
        ).unwrap();
        assert_eq!(pressed_snapshots, vec!["fresh-target-window"]);
        assert_eq!(result["status"], "submitted_verified");
        assert_eq!(result["submit_attempts"], 1);
        assert!(!result.to_string().contains("private composer draft"));
    }

    #[test]
    fn compound_send_does_not_press_when_type_command_failed() {
        let mut presses = 0;
        let result = send_with_actions(
            || Ok(json!({"result":{"ok":false,"delivery":"delivered_unverified","focus_verified_before_type":true},"fresh_observation":{"snapshot_id":"post-type"}})),
            |_| { presses += 1; Ok(json!({})) },
        ).unwrap();
        assert_eq!(presses, 0);
        assert_eq!(result["status"], "not_submitted");
        assert_eq!(result["type_command_succeeded"], false);
    }

    #[test]
    fn compound_send_distinguishes_stale_or_missing_ref_from_type_failure_without_echoing_text() {
        let mut presses = 0;
        let result = send_with_actions(
            || Ok(json!({
                "kind":"desktop_action",
                "action":"type",
                "status":"not_performed",
                "reason":"The ref is stale. Use a ref from the fresh observation before acting.",
                "fresh_observation":{"kind":"desktop_snapshot","snapshot_id":"fresh-after-refusal","complete":true,"local_snapshot_retained":true}
            })),
            |_| { presses += 1; Ok(json!({})) },
        ).unwrap();
        assert_eq!(presses, 0);
        assert_eq!(result["status"], "not_submitted");
        assert_eq!(result["type_failure"]["kind"], "stale_or_not_performed");
        assert_eq!(result["type_failure"]["action_status"], "not_performed");
        assert_eq!(
            result["fresh_observation"]["snapshot_id"],
            "fresh-after-refusal"
        );
    }

    #[test]
    fn compound_send_preserves_safe_focus_failure_codes_but_redacts_messages() {
        let mut presses = 0;
        let result = send_with_actions(
            || Ok(json!({
                "kind":"desktop_action",
                "action":"type",
                "status":"failed_or_uncertain",
                "result":{
                    "ok":false,
                    "code":"FOCUS_NOT_CONFIRMED",
                    "stage":"focus",
                    "agent_desktop_code":"NOT_FOCUSED",
                    "agent_desktop_details_kind":"actionability",
                    "message":"private composer text must not leak",
                    "focus_verified_before_type":false
                },
                "fresh_observation":{"kind":"desktop_snapshot","snapshot_id":"fresh-focus-failure","complete":true,"local_snapshot_retained":true}
            })),
            |_| { presses += 1; Ok(json!({})) },
        ).unwrap();
        assert_eq!(presses, 0);
        assert_eq!(result["type_failure"]["kind"], "focus_not_confirmed");
        assert_eq!(result["type_failure"]["error_code"], "FOCUS_NOT_CONFIRMED");
        assert_eq!(result["type_failure"]["stage"], "focus");
        assert_eq!(result["type_failure"]["agent_desktop_code"], "NOT_FOCUSED");
        assert!(!result.to_string().contains("private composer text"));
    }

    #[test]
    fn compound_send_does_not_press_when_focus_precondition_failed() {
        let mut presses = 0;
        let result = send_with_actions(
            || Ok(json!({"result":{"ok":false,"delivery":"not_delivered"},"fresh_observation":{"snapshot_id":"post-focus-failure"}})),
            |_| { presses += 1; Ok(json!({})) },
        ).unwrap();
        assert_eq!(presses, 0);
        assert_eq!(result["status"], "not_submitted");
        assert_eq!(result["focus_verified_before_type"], false);
    }

    #[test]
    fn compound_send_does_not_treat_an_unqualified_value_readback_as_proof() {
        let mut presses = 0;
        let result = send_with_actions(
            // Agent Desktop get(value) can return a saved snapshot fallback,
            // and its value response has no live-read indicator. Even an
            // apparent exact match must not override unverified type delivery.
            || {
                Ok(json!({
                    "result":{
                        "delivery":"delivered_unverified",
                        "verification_scope":"element_value",
                        "verification_reason":"postcondition_mismatch"
                    },
                    "value_readback":{"value":"exact intended value"},
                    "fresh_observation":{"snapshot_id":"post-type"}
                }))
            },
            |_| {
                presses += 1;
                Ok(json!({}))
            },
        )
        .unwrap();
        assert_eq!(presses, 0);
        assert_eq!(result["status"], "not_submitted");
        assert_eq!(result["type_delivery"], "delivered_unverified");
        assert_eq!(result["type_verification_scope"], "element_value");
        assert_eq!(result["type_verification_reason"], "postcondition_mismatch");
        assert!(!result.to_string().contains("exact intended value"));
    }

    #[test]
    fn compound_send_uses_fresh_snapshot_and_presses_at_most_once() {
        let mut presses = Vec::new();
        let result = send_with_actions(
            || Ok(json!({"result":{"ok":true,"delivery":"delivered_verified","focus_verified_before_type":true},"fresh_observation":{"snapshot_id":"post-type"}})),
            |snapshot_id| { presses.push(snapshot_id.to_owned()); Ok(json!({"result":{"delivery":"delivered_unverified"},"fresh_observation":{"snapshot_id":"post-submit"}})) },
        ).unwrap();
        assert_eq!(presses, vec!["post-type"]);
        assert_eq!(result["submit_attempts"], 1);
        assert_eq!(result["status"], "submitted_unverified");
        assert_eq!(result["fresh_observation"]["snapshot_id"], "post-submit");
    }
}
