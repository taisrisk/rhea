use std::{
    io::{BufRead, BufReader},
    time::{Duration, Instant},
};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::research::{
    EvidenceLog, ResearchCompletion, requires_research_evidence, required_research_evidence_rows, research_blocker_reply, research_completion_gate,
};
#[cfg(test)]
use crate::research::{evidence_fingerprint, requested_threshold, research_evidence_rows_from_tool};

// ChatGPT subscription access uses the Codex account catalog and response route.
const CODEX_CLIENT_VERSION: &str = "0.166.0";

/// Which backend a model is asked through. Every request still carries Rhea's own local
/// transcript (see `resume_items`/`base_input`), never server-held state, so switching the
/// selected model from one provider to another mid-chat carries the same history either way -
/// there is nothing provider-specific to lose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum Provider {
    #[default]
    ChatGpt,
    OpenCode,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::ChatGpt => "ChatGPT",
            Provider::OpenCode => "OpenCode",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    #[serde(default)]
    pub provider: Provider,
    /// Catalog ordering hint from the Codex model API, not a price.
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<String>,
    /// The size the catalog recommends working in (272K for most models).
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The most the ChatGPT backend accepts for this model on this account (`max_context_window`).
    #[serde(default)]
    pub max_context_window: Option<u64>,
}

impl Model {
    /// The model's full context: the account's maximum, else the catalog's size, else what the
    /// OpenAI model pages publish (ChatGPT models only; OpenCode's catalog reports its own).
    pub fn context_limit(&self) -> Option<u64> {
        self.max_context_window.or(self.context_window).or_else(|| (self.provider == Provider::ChatGpt).then(|| documented_limits(&self.slug)).flatten().map(|limits| limits.context))
    }

    /// The most the model can write in one reply, from the OpenAI model pages (ChatGPT only).
    pub fn max_output(&self) -> Option<u64> {
        (self.provider == Provider::ChatGpt).then(|| documented_limits(&self.slug)).flatten().map(|limits| limits.max_output)
    }
}

/// Limits published on https://developers.openai.com/api/docs/models (checked against each model's
/// page). The context here is the API's; the ChatGPT backend can allow less, which is why the
/// catalog's own `max_context_window` wins when it reports one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentedLimits {
    pub context: u64,
    pub max_output: u64,
}

pub fn documented_limits(slug: &str) -> Option<DocumentedLimits> {
    let slug = slug.to_ascii_lowercase();
    let slug = slug.as_str();
    let context = match slug {
        "gpt-5.4-mini" | "gpt-5.4-nano" | "gpt-5.3-codex" | "gpt-5.2-codex" | "gpt-5" => 400_000,
        "gpt-5.4" | "gpt-5.4-pro" => 1_050_000,
        _ if slug.starts_with("gpt-6") || slug.starts_with("gpt-5.6") || slug.starts_with("gpt-5.5") => 1_050_000,
        _ if slug.starts_with("gpt-5.2") || slug.starts_with("gpt-5.1") || slug.starts_with("gpt-5-") => 400_000,
        _ => return None,
    };
    // Cyber variants are listed without figures.
    if slug.contains("cyber") {
        return None;
    }
    Some(DocumentedLimits { context, max_output: 128_000 })
}

pub fn models(access_token: &str, account_id: Option<&str>) -> Result<Vec<Model>, String> {
    let http = Client::builder()
        .timeout(Duration::from_secs(25))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = http
        .get(format!(
            "https://chatgpt.com/backend-api/codex/models?client_version={CODEX_CLIENT_VERSION}"
        ))
        .bearer_auth(access_token)
        .header("originator", "codex_cli_rs")
        .header(
            reqwest::header::USER_AGENT,
            format!("Rhea/{}", env!("CARGO_PKG_VERSION")),
        );
    if let Some(id) = account_id {
        request = request.header("ChatGPT-Account-ID", id);
    }
    let response = request
        .send()
        .map_err(|e| format!("Model list request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Could not load Codex models for this ChatGPT account: {e}"))?
        .json::<Value>()
        .map_err(|e| format!("Model list response was invalid: {e}"))?;
    let list = response
        .get("models")
        .and_then(Value::as_array)
        .ok_or("ChatGPT returned no model catalog")?;
    let catalog_count = list.len();
    let models: Vec<Model> = list
        .iter()
        .filter(|m| {
            m.get("visibility").and_then(Value::as_str) == Some("list")
                && m.get("supported_in_api")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
        })
        .filter_map(|m| {
            Some(Model {
                slug: m.get("slug")?.as_str()?.to_owned(),
                display_name: m.get("display_name")?.as_str()?.to_owned(),
                provider: Provider::ChatGpt,
                priority: m.get("priority").and_then(Value::as_i64),
                supported_reasoning_levels: m
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|level| {
                        level
                            .get("effort")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect(),
                context_window: model_context_window(m),
                max_context_window: m
                    .get("max_context_window")
                    .and_then(Value::as_u64)
                    .filter(|value| *value >= 16_000),
            })
        })
        .collect();
    if models.is_empty() {
        return Err(format!(
            "ChatGPT returned {catalog_count} catalog entries, but none are marked available to the Codex API. Rhea requests the Codex {CODEX_CLIENT_VERSION} catalog."
        ));
    }
    Ok(models)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleModel {
    pub slug: String,
    pub reasoning_effort: String,
}

/// Pick an explicitly small model by catalog slug and its lowest advertised reasoning effort.
/// The current account catalog exposes no pricing, so this does not claim lowest cost.
pub fn title_model(models: &[Model]) -> Option<TitleModel> {
    let model = models
        .iter()
        .filter(|model| {
            let slug = model.slug.to_ascii_lowercase();
            slug.contains("nano") || slug.contains("mini")
        })
        .min_by_key(|model| {
            let slug = model.slug.to_ascii_lowercase();
            if slug.contains("nano") { 0 } else { 1 }
        })?;
    let effort = model
        .supported_reasoning_levels
        .iter()
        .filter_map(|effort| {
            let rank = match effort.to_ascii_lowercase().as_str() {
                "none" => 0,
                "minimal" => 1,
                "low" => 2,
                "medium" => 3,
                "high" => 4,
                "xhigh" => 5,
                _ => return None,
            };
            Some((rank, effort))
        })
        .min_by_key(|(rank, _)| *rank)?
        .1;
    Some(TitleModel {
        slug: model.slug.clone(),
        reasoning_effort: effort.clone(),
    })
}

/// Instructions for the background naming agent. The shipped prompt ends with a
/// `<session>` template block; the app supplies the real session content itself.
fn title_instructions() -> String {
    let raw = include_str!("session_title_prompt.md");
    let closing = "`</session>`";
    match (raw.rfind("`<session>`"), raw.rfind(closing)) {
        (Some(start), Some(end)) if end > start => format!("{}{}", &raw[..start], &raw[end + closing.len()..]),
        _ => raw.to_owned(),
    }
    .trim()
    .to_owned()
}

/// The user's own selected model, at the lowest reasoning effort it advertises.
pub fn title_model_for(models: &[Model], selected_slug: &str) -> Option<TitleModel> {
    let model = models.iter().find(|model| model.slug == selected_slug).or_else(|| models.first())?;
    let rank = |effort: &str| match effort.to_ascii_lowercase().as_str() {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        _ => 99,
    };
    let effort = model
        .supported_reasoning_levels
        .iter()
        .min_by_key(|effort| rank(effort))
        .cloned()
        .unwrap_or_else(|| "low".to_owned());
    Some(TitleModel { slug: model.slug.clone(), reasoning_effort: effort })
}
const TITLE_SOURCE_CHAR_LIMIT: usize = 1_000;
const TITLE_CHAR_LIMIT: usize = 64;

/// Generate a local conversation title with a catalog-selected small model.
/// This sends only a short title instruction and a bounded excerpt of the first request;
/// it does not send history or local tools. On API/transport failures, the caller can use
/// `fallback_title` to name the chat locally.
pub fn generate_title(
    access_token: &str,
    account_id: Option<&str>,
    model: &TitleModel,
    first_request: &str,
) -> Result<String, String> {
    let http = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let request_excerpt: String = first_request
        .chars()
        .take(TITLE_SOURCE_CHAR_LIMIT)
        .collect();
    let mut request = http
        .post("https://chatgpt.com/backend-api/codex/responses")
        .bearer_auth(access_token)
        .header("originator", "codex_cli_rs")
        .header(
            reqwest::header::USER_AGENT,
            format!("Rhea/{}", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&title_request_body(model, &request_excerpt));
    if let Some(id) = account_id {
        request = request.header("ChatGPT-Account-ID", id);
    }
    let mut response = request
        .send()
        .map_err(|e| format!("Title request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!(
            "Title request returned HTTP {status}: {}",
            concise_error(&body)
        ));
    }

    let mut reader = BufReader::new(&mut response);
    let mut line = String::new();
    let mut streamed_text = String::new();
    let mut completed_title: Option<String> = None;
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| format!("Title stream read failed: {e}"))?;
        if read == 0 {
            break;
        }
        let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let event: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return Err(format!(
                    "Title stream contained invalid event JSON: {error}"
                ));
            }
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    streamed_text.push_str(delta);
                }
            }
            "response.completed" => {
                let response = event.get("response").unwrap_or(&event);
                completed_title = Some(parse_completed_title_response(
                    response,
                    &streamed_text,
                    first_request,
                ));
            }
            "response.failed" | "response.incomplete" | "error" => {
                return Err(stream_error(&event));
            }
            _ => {}
        }
    }
    completed_title.ok_or_else(|| "Title stream ended before response.completed.".to_owned())
}

/// Produce a stable local title when generation is unavailable or returns no usable text.
pub fn fallback_title(request: &str) -> String {
    let compact = request.split_whitespace().collect::<Vec<_>>().join(" ");
    let compact = compact.trim_matches(|character: char| {
        character.is_ascii_punctuation() || character.is_whitespace()
    });
    if compact.is_empty() {
        return "New chat".into();
    }
    let mut title: String = compact.chars().take(TITLE_CHAR_LIMIT).collect();
    if compact.chars().count() > TITLE_CHAR_LIMIT {
        title = format!("{}…", title.trim_end());
    }
    title
}

fn parse_title_response(response_text: &str, request: &str) -> String {
    let response_text = strip_code_fence(response_text.trim());
    let candidate = match serde_json::from_str::<Value>(response_text) {
        Ok(Value::String(text)) => text,
        Ok(Value::Object(object)) => object
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => response_text.to_owned(),
    };
    let candidate = candidate.trim().trim_matches('`').trim();
    let candidate = candidate
        .strip_prefix("Title:")
        .or_else(|| candidate.strip_prefix("title:"))
        .unwrap_or(candidate);
    let candidate = candidate.trim().trim_matches('"').trim_matches('\'').trim();
    let normalized = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return fallback_title(request);
    }
    let mut title: String = normalized.chars().take(TITLE_CHAR_LIMIT).collect();
    if normalized.chars().count() > TITLE_CHAR_LIMIT {
        title = format!("{}…", title.trim_end());
    }
    title
}

fn strip_code_fence(text: &str) -> &str {
    if !text.starts_with("```") {
        return text;
    }
    let Some(after_open) = text.strip_prefix("```") else {
        return text;
    };
    let Some((_, body)) = after_open.split_once('\n') else {
        return text;
    };
    body.strip_suffix("```").unwrap_or(body).trim()
}

fn parse_completed_title_response(response: &Value, streamed_text: &str, request: &str) -> String {
    let text = response
        .get("output_text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            let output = response.get("output")?.as_array()?;
            let text = completed_text(output);
            (!text.trim().is_empty()).then_some(text)
        })
        .unwrap_or_else(|| streamed_text.to_owned());
    parse_title_response(&text, request)
}

fn title_request_body(model: &TitleModel, request_excerpt: &str) -> Value {
    json!({
        "model":model.slug,
        "reasoning":{"effort":model.reasoning_effort},
        "input":[
            {"role":"developer","content":title_instructions()},
            {"role":"user","content":format!("<session>\n{request_excerpt}\n</session>")}
        ],
        "tools":[],"tool_choice":"none","parallel_tool_calls":false,
        "store":false,"stream":true
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub tool_activity: Vec<ToolActivity>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolActivity {
    pub id: String,
    pub name: String,
    pub arguments_summary: String,
    pub result_summary: String,
    pub state: String,
    #[serde(default)]
    pub arguments_detail: String,
    #[serde(default)]
    pub result_detail: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cache_reported: bool,
    pub cache_write_reported: bool,
}

/// Add or update a tool call in a turn's list. Nothing is dropped as the list grows; only the
/// bulky argument/result text of calls far behind the newest is cleared so saved chats stay small.
pub fn record_activity(list: &mut Vec<ToolActivity>, activity: ToolActivity) {
    const KEEP_DETAIL: usize = 60;
    const MAX_CALLS: usize = 2_000;
    if let Some(existing) = list.iter_mut().rev().find(|item| item.id == activity.id) {
        *existing = activity;
        return;
    }
    list.push(activity);
    let len = list.len();
    if len > KEEP_DETAIL {
        let old = &mut list[len - KEEP_DETAIL - 1];
        if old.state != "running" {
            old.arguments_detail.clear();
            old.result_detail.clear();
        }
    }
    if len > MAX_CALLS {
        list.remove(0);
    }
}

#[derive(Clone, Debug)]
pub struct CompactionUpdate {
    pub summary: String,
    pub summarized_turns: usize,
}

#[derive(Default)]
pub struct StreamEvent {
    pub delta: Option<String>,
    pub tool_name: Option<String>,
    pub tool_activity: Option<ToolActivity>,
    pub latency_ms: Option<u128>,
    pub error: Option<String>,
    pub usage: Option<Usage>,
    pub compaction: Option<CompactionUpdate>,
    /// A short message for the status line (not part of the reply).
    pub notice: Option<String>,
    /// Fast mode was requested but the service rejected `service_tier` and the run fell back to
    /// normal speed. Usage should count this run as normal, not fast, even though the setting is on.
    pub fast_unavailable: bool,
}

const STREAM_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

async fn wait_for_cancellation(cancelled: impl Fn() -> bool) {
    loop {
        if cancelled() {
            return;
        }
        tokio::time::sleep(STREAM_CANCEL_POLL_INTERVAL).await;
    }
}

#[derive(Default)]
struct SseLineBuffer {
    pending: Vec<u8>,
}

impl SseLineBuffer {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        let mut start = 0;
        for index in 0..self.pending.len() {
            if self.pending[index] == b'\n' {
                let end = if index > start && self.pending[index - 1] == b'\r' {
                    index - 1
                } else {
                    index
                };
                lines.push(String::from_utf8_lossy(&self.pending[start..end]).into_owned());
                start = index + 1;
            }
        }
        if start > 0 {
            self.pending.drain(..start);
        }
        lines
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        if self.pending.last() == Some(&b'\r') {
            self.pending.pop();
        }
        Some(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned())
    }
}

/// Read a streaming response without pinning the chat worker in a blocking socket read.
/// The short poll timeout applies only to each chunk read, not to the total response, so a
/// long model response remains valid while the UI can still cancel an idle stream promptly.
pub(crate) fn read_sse_lines_with_cancel(
    runtime: &tokio::runtime::Runtime,
    response: &mut reqwest::Response,
    cancelled: impl Fn() -> bool + Copy,
    mut on_line: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    let mut decoder = SseLineBuffer::default();
    loop {
        if cancelled() {
            return Err("Run cancelled.".into());
        }

        let read_result = runtime.block_on(async {
            tokio::select! {
                result = response.chunk() => Some(result),
                _ = wait_for_cancellation(cancelled) => None,
            }
        });
        match read_result {
            Some(Ok(Some(chunk))) => {
                for line in decoder.push(&chunk) {
                    on_line(&line)?;
                }
            }
            Some(Ok(None)) => {
                if let Some(line) = decoder.finish() {
                    on_line(&line)?;
                }
                return Ok(());
            }
            Some(Err(error)) => return Err(format!("Stream read failed: {error}")),
            None => return Err("Run cancelled.".into()),
        }
    }
}

pub(crate) fn send_chat_request_with_cancel(
    runtime: &tokio::runtime::Runtime,
    request: reqwest::RequestBuilder,
    cancelled: impl Fn() -> bool + Copy,
) -> Result<reqwest::Response, String> {
    runtime.block_on(async {
        tokio::select! {
            result = request.send() => {
                let response = result.map_err(|error| format!("Chat request failed: {error}"))?;
                if !response.status().is_success() {
                    let status = response.status();
                    let body = tokio::select! {
                        body = response.text() => body.unwrap_or_default(),
                        _ = wait_for_cancellation(cancelled) => return Err("Run cancelled.".into()),
                    };
                    return Err(format!(
                        "Chat request returned HTTP {status}: {}",
                        concise_error(&body)
                    ));
                }
                Ok(response)
            }
            _ = wait_for_cancellation(cancelled) => Err("Run cancelled.".into()),
        }
    })
}

/// A Stop hook may send the model back to work, at most twice per turn.
fn stop_hook_reason(options: &RunOptions, answer: &str, retries: &mut u8) -> Option<String> {
    if *retries >= 2 {
        return None;
    }
    let reason = options.on_stop.as_ref().and_then(|stop| stop(answer))?;
    *retries += 1;
    Some(reason)
}

pub const DEFAULT_CONTEXT_WINDOW: u64 = 264_000;
/// The operating manual for this platform (Windows-first text, or the macOS edition).
#[cfg(target_os = "macos")]
const PLATFORM_PROMPT: &str = include_str!("system_prompt_macos.md");
#[cfg(not(target_os = "macos"))]
const PLATFORM_PROMPT: &str = include_str!("system_prompt.md");

/// Operating instructions, always-on skills, and concrete end-to-end examples.
fn system_prompt() -> &'static str {
    static PROMPT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROMPT.get_or_init(|| {
        [
            PLATFORM_PROMPT,
            include_str!("skills/systematic_debugging.md"),
            include_str!("skills/brainstorming.md"),
            include_str!("workflow_examples.md"),
        ]
        .join("

")
    })
}

const COMPOSE_PROMPT: &str = include_str!("compose_prompt.md");

/// Per-run behaviour beyond the plain tool loop: Compose Mode, subagent plumbing and hooks.
#[derive(Default)]
pub struct RunOptions {
    /// Start in Compose Mode: read-only tools until the user accepts a proposed plan.
    pub compose: bool,
    /// Extra developer messages appended after the user message (subagent prompt, hook context).
    pub extra_developer: Vec<String>,
    /// Tool names this run must not see or call (e.g. `ask_user` for subagents).
    pub hidden_tools: Vec<String>,
    /// Polled before every model request; returns messages from other agents / the team status.
    pub inbox: Option<Box<dyn Fn() -> Vec<String> + Send>>,
    /// Stable client-generated chat key for cache affinity; local history is still sent each turn.
    pub session_id: Option<String>,
    /// A subagent run: the "verify research evidence" completion gate is for the user's own
    /// requests, not for a worker's task description.
    pub worker: bool,
    /// Called with the final answer; `Some(reason)` makes the model keep going (a Stop hook).
    pub on_stop: Option<Box<dyn Fn(&str) -> Option<String> + Send>>,
    /// Called when the model is ready to finish: blocks while workers are still running and
    /// returns what they sent, so the manager never ends a turn with work in flight.
    pub hold_for_team: Option<Box<dyn Fn() -> Vec<String> + Send>>,
    /// Compact the transcript once a request would fill this share of the context window.
    pub compact_percent: u8,
    /// The model's own saved transcript for this chat/agent (see `ModelState`); when it matches the
    /// visible turns the run continues from it instead of from the text of past replies.
    pub state: Option<crate::chat_store::ModelState>,
    /// Called when a run ends (finished, failed or cancelled) with the transcript to continue from.
    pub on_state: Option<Box<dyn Fn(crate::chat_store::ModelState) + Send>>,
    /// Where this run writes its ledger (see `ledger.rs`); none means no record is kept.
    pub ledger: Option<crate::ledger::Ledger>,
    /// The Responses endpoint; the ChatGPT backend unless a test points it at a scripted server.
    pub endpoint: Option<String>,
    /// Fast mode: asks for priority processing; speed/usage effect is unverified (see `Model`'s
    /// own doc comment in app.rs).
    pub fast: bool,
    /// Which backend `access_token` actually authenticates against.
    pub provider: Provider,
}

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// What ChatGPT calls fast mode on the wire: priority processing.
const FAST_SERVICE_TIER: &str = "priority";

pub const DEFAULT_COMPACT_PERCENT: u8 = 80;
pub const MIN_COMPACT_PERCENT: u8 = 15;
pub const MAX_COMPACT_PERCENT: u8 = 95;

pub fn clamp_compact_percent(percent: u8) -> u8 {
    percent.clamp(MIN_COMPACT_PERCENT, MAX_COMPACT_PERCENT)
}

/// Replace the bulky results of old tool calls with a short stub. A long run keeps its recent
/// work verbatim while the earliest outputs stop crowding the context window.
fn trim_old_tool_outputs(input: &mut [Value], keep_recent: usize) -> usize {
    let end = input.len().saturating_sub(keep_recent);
    let mut trimmed = 0;
    for item in input[..end].iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
            continue;
        }
        let Some(output) = item.get("output").and_then(Value::as_str) else { continue };
        if output.chars().count() <= 1_200 {
            continue;
        }
        let head: String = output.chars().take(600).collect();
        item["output"] = json!(format!("{head}\n…[older tool result trimmed to save context; re-run the tool if you need the rest]"));
        trimmed += 1;
    }
    trimmed
}

/// Summarize a whole transcript on demand (the `/compact` command).
pub fn compact_conversation(access_token: &str, account_id: Option<&str>, model: &str, prior_summary: &str, session_id: Option<&str>, turns: &[Turn]) -> Result<String, String> {
    let http = Client::builder().timeout(Duration::from_secs(180)).build().map_err(|e| e.to_string())?;
    compact_history(access_token, account_id, model, prior_summary, session_id, turns, &http, &|_| {})
}

fn request_tools(tools: &Value, compose_active: bool, hidden: &[String]) -> Value {
    let list = tools.as_array().cloned().unwrap_or_default();
    let mut list: Vec<Value> = if compose_active {
        crate::compose::filter_tools(&list)
    } else {
        list.into_iter().filter(|tool| tool.get("name").and_then(Value::as_str) != Some("propose_plan")).collect()
    };
    list.retain(|tool| tool.get("name").and_then(Value::as_str).is_none_or(|name| !hidden.iter().any(|h| h == name)));
    Value::Array(list)
}

#[allow(clippy::too_many_arguments)]
pub fn respond_with_options(
    access_token: &str,
    account_id: Option<&str>,
    model: &str,
    reasoning_effort: &str,
    context_window: u64,
    prior_summary: &str,
    history: &[Turn],
    user_message: &str,
    extra_tools: &[Value],
    execute_tool: impl FnMut(&str, &Value) -> Result<Value, String>,
    mut options: RunOptions,
    send: impl Fn(StreamEvent),
) -> Result<String, String> {
    let on_state = options.on_state.take();
    let ledger = options.ledger.take().unwrap_or_else(crate::ledger::Ledger::off);
    let mut run = RunState::default();
    ledger.record(crate::ledger::Event::Start { model: model.to_owned(), request: user_message.chars().take(300).collect() });
    let result = respond_inner(
        access_token,
        account_id,
        model,
        reasoning_effort,
        context_window,
        prior_summary,
        history,
        user_message,
        extra_tools,
        execute_tool,
        options,
        &send,
        &mut run,
        &ledger,
    );
    let (status, error) = match &result {
        Ok(_) => ("completed", String::new()),
        Err(error) if error.contains("cancelled") => ("cancelled", error.clone()),
        Err(error) => ("failed", error.clone()),
    };
    ledger.record(crate::ledger::Event::End {
        status: status.into(),
        ms: ledger.elapsed_ms(),
        answer_chars: result.as_ref().map(|answer| answer.chars().count()).unwrap_or(0),
        error: error.chars().take(300).collect(),
    });
    // Hand the transcript back however the run ended, so a stopped or failed run is still remembered.
    if run.started {
        if let Some(save) = on_state {
            save(run.into_state());
        }
    }
    result
}

/// What a run builds up that outlives it.
#[derive(Default)]
struct RunState {
    started: bool,
    input: Vec<Value>,
    /// Indexes in `input` of the real user messages (the history's and this run's).
    user_positions: Vec<usize>,
    loaded_tools: Vec<Value>,
    /// Transcript turns (user + assistant) this run's items account for.
    turns: usize,
}

impl RunState {
    fn into_state(self) -> crate::chat_store::ModelState {
        let (items, turn_starts) = persistable_items(&self.input, &self.user_positions);
        crate::chat_store::ModelState { items, loaded_tools: self.loaded_tools, turn_starts, turns: self.turns }
    }
}

#[allow(clippy::too_many_arguments)]
fn respond_inner(
    access_token: &str,
    account_id: Option<&str>,
    model: &str,
    reasoning_effort: &str,
    context_window: u64,
    prior_summary: &str,
    history: &[Turn],
    user_message: &str,
    extra_tools: &[Value],
    mut execute_tool: impl FnMut(&str, &Value) -> Result<Value, String>,
    mut options: RunOptions,
    send: &impl Fn(StreamEvent),
    run: &mut RunState,
    ledger: &crate::ledger::Ledger,
) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Could not start cancellable ChatGPT stream runtime: {error}"))?;
    let stream_http = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|error| format!("Could not create ChatGPT stream client: {error}"))?;
    let http = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| e.to_string())?;
    crate::jev::begin_run();
    // For a user-authorized message-send task, don't merely ask the model to
    // prefer the composite action: remove the lower-level text/key tools from
    // the request schema so the model cannot spend serial rounds on a manual
    // type/read/press sequence. Semantic clicks remain available for reaching
    // the requested conversation; desktop_send is the only advertised way to
    // enter and submit the requested message.
    let mut tools = tools_for_request(extra_tools, user_message)?;
    let explicit_send_requested = has_explicit_send_intent(user_message);
    // Continue from the model's own saved transcript when it still matches the visible turns;
    // otherwise (a new chat, an edit, a regenerate) rebuild it from the turns' text and tool digests.
    let saved = options.state.take().unwrap_or_default();
    let (mut prior, mut starts) = resume_items(&saved, history);
    // Tools the model loaded earlier in this chat stay loaded: no `get_tools_` again every run.
    if let Some(list) = tools.as_array_mut() {
        for schema in saved.loaded_tools {
            let name = schema.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            if !name.is_empty() && !list.iter().any(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str())) {
                list.push(schema.clone());
            }
            run.loaded_tools.push(schema);
        }
    }
    let mut current_summary = prior_summary.trim().to_owned();
    let mut history_turns = history.len();
    let window = context_window.max(16_000);
    let estimated = estimate_tokens(system_prompt())
        + estimate_items(&prior)
        + estimate_tokens(user_message)
        + estimate_tokens(&current_summary);
    let compact_percent = u64::from(clamp_compact_percent(if options.compact_percent == 0 { DEFAULT_COMPACT_PERCENT } else { options.compact_percent }));
    let compact_limit = window.saturating_mul(compact_percent) / 100;
    if estimated > compact_limit {
        if let Some((cut, summarized_turns)) = compaction_cut(&starts) {
            send(StreamEvent {
                tool_name: Some("compact_context".to_owned()),
                ..Default::default()
            });
            current_summary = compact_history(
                access_token,
                account_id,
                model,
                &current_summary,
                options.session_id.as_deref(),
                &items_to_turns(&prior[..cut]),
                &http,
                send,
            )?;
            prior.drain(..cut);
            starts = starts.into_iter().filter(|start| *start >= cut).map(|start| start - cut).collect();
            history_turns = history_turns.saturating_sub(summarized_turns);
            send(StreamEvent {
                compaction: Some(CompactionUpdate {
                    summary: current_summary.clone(),
                    summarized_turns,
                }),
                ..Default::default()
            });
        }
    }
    let mut built = base_input(&current_summary);
    let offset = built.len();
    run.user_positions = starts.iter().map(|start| start + offset).collect();
    built.extend(prior);
    run.user_positions.push(built.len());
    built.push(json!({"role":"user","content":user_message}));
    run.input = built;
    run.turns = history_turns + 2;
    run.started = true;
    let input = &mut run.input;
    let mut compose_active = options.compose;
    if compose_active {
        input.push(json!({"role":"developer","content":COMPOSE_PROMPT}));
    }
    for text in &options.extra_developer {
        if !text.trim().is_empty() {
            input.push(json!({"role":"developer","content":text}));
        }
    }
    let mut stop_retries = 0_u8;
    let mut last_input_tokens = 0_u64;
    let mut answer = String::new();
    let mut buffered_research_text = String::new();
    let mut call_sequence = 0_u64;
    let mut message_sequence = 0_u64;
    let mut desktop_send_attempted = false;
    // A worker's task brief or a long coding request that merely mentions servers or tools is
    // not a status question; only a short, direct one is answered from the live inventory.
    let inventory_question = !options.worker && is_connection_inventory_question(user_message);
    let force_connection_inventory = inventory_question;
    let mut connection_inventory_requested = false;
    let mut actions_waiting_for_snapshot: Vec<ToolActivity> = Vec::new();
    let mut completion_diagnostics = "completion metadata unavailable".to_owned();
    let mut visible_delta_sent = false;
    let research_task = !options.worker && requires_research_evidence(user_message);
    // What this run has verified so far; the same page fetched twice is one source, not two.
    let mut research_log = EvidenceLog::default();
    let mut evidence_retry_count = 0_u8;
    let mut fast_on = options.fast;

    loop {
        if crate::jev::cancel_requested() {
            return Err("Run cancelled.".into());
        }
        if let Some(poll) = options.inbox.as_ref() {
            for message in poll() {
                // A message from the user or another agent shows up as its own row in the tool list.
                if let Some((from, text)) = message.strip_prefix("[message from ").and_then(|rest| rest.split_once("] ")) {
                    message_sequence += 1;
                    let from = from.split(',').next().unwrap_or(from).trim().to_owned();
                    send(StreamEvent {
                        tool_activity: Some(ToolActivity {
                            id: format!("message-{message_sequence}-{}", input.len()),
                            name: "message_received".into(),
                            arguments_summary: format!("from {from}"),
                            arguments_detail: json!({"from": from}).to_string(),
                            result_summary: "Delivered to the model".into(),
                            result_detail: text.to_owned(),
                            state: "complete".into(),
                        }),
                        ..Default::default()
                    });
                }
                input.push(json!({"role":"user","content":message}));
            }
        }
        if last_input_tokens > compact_limit {
            if trim_old_tool_outputs(input, 12) > 0 {
                last_input_tokens = 0;
            }
        }
        let tools_for_round = request_tools(&tools, compose_active, &options.hidden_tools);
        // Measure one model round trip at a time. The total Rhea task may
        // include several model rounds and local tool execution, so it must
        // not be presented as a single model-request duration.
        let request_started = Instant::now();
        // Connection status is local, live state. Do not leave its discovery
        // to tool_choice=auto: the model can answer from memory and omit bridges
        // (such as Chrome) that are not configured MCP servers.
        let tool_choice = if force_connection_inventory && !connection_inventory_requested {
            connection_inventory_requested = true;
            json!({"type":"function","name":"list_mcps"})
        } else {
            json!("auto")
        };
        let round = match options.provider {
            Provider::ChatGpt => chatgpt_round(
                &runtime, &stream_http, access_token, account_id, model, reasoning_effort, input, &tools_for_round, &tool_choice,
                options.endpoint.as_deref(), options.session_id.as_deref(), &mut fast_on, research_task, &mut answer, &mut visible_delta_sent, send,
            ),
            Provider::OpenCode => crate::opencode::chat_round(
                &runtime, &stream_http, access_token, model, input, &tools_for_round, options.endpoint.as_deref(), research_task, &mut answer, &mut visible_delta_sent, send,
            ),
        }?;
        send(StreamEvent {
            latency_ms: Some(request_started.elapsed().as_millis()),
            ..Default::default()
        });
        completion_diagnostics = round.diagnostics;
        if let Some(tokens) = round.usage_tokens {
            last_input_tokens = tokens;
        }
        // Fresh every round, like the model round's own locally-scoped buffer it replaces: a
        // round with no buffered research text (a tool-call-only round, or a non-research task)
        // must not leave an earlier round's leftover text in place.
        buffered_research_text = round.research_text;
        let output = round.output;
        let calls: Vec<Value> = output
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            .cloned()
            .collect();
        ledger.record(crate::ledger::Event::Model { ms: request_started.elapsed().as_millis() as u64, tokens: last_input_tokens, calls: calls.len() });
        if calls.is_empty() {
            if let Some(hold) = options.hold_for_team.as_ref() {
                let notes = hold();
                if !notes.is_empty() {
                    input.extend(output);
                    for note in notes {
                        input.push(json!({"role":"user","content":note}));
                    }
                    continue;
                }
            }
            let required_rows = required_research_evidence_rows(user_message);
            let gate = research_completion_gate(research_task, research_log.rows(), required_rows, evidence_retry_count);
            if research_task {
                let decision = match gate {
                    ResearchCompletion::Accept => "accept",
                    ResearchCompletion::Retry => "retry",
                    ResearchCompletion::Block => "block",
                };
                ledger.record(crate::ledger::Event::Gate { decision: decision.into(), have: research_log.rows(), need: required_rows });
            }
            match gate {
                ResearchCompletion::Retry => {
                    evidence_retry_count = evidence_retry_count.saturating_add(1);
                    input.extend(output);
                    input.push(json!({"role":"developer","content":format!("Completion gate: this is an explicit research or current-data request, and the verified source/search evidence collected so far does not meet the user's requested threshold or result count. Verified so far: {} of the {} needed ({}). Do not present a promise or unsupported findings as completion. Use available browser or MCP search/extract tools to fill the remaining evidence requirement. If no such tool is available or it fails, give a concise truthful blocker and say what could not be verified.", research_log.rows(), required_rows, research_log.digest())}));
                    continue;
                }
                ResearchCompletion::Block => {
                    let blocker = research_blocker_reply(&research_log, required_rows);
                    send(StreamEvent {
                        delta: Some(blocker.clone()),
                        ..Default::default()
                    });
                    return Ok(blocker);
                }
                ResearchCompletion::Accept => {}
            }
            if research_task {
                answer = if buffered_research_text.trim().is_empty() {
                    completed_text(&output)
                } else {
                    buffered_research_text
                };
                if answer.trim().is_empty() {
                    return Err(format!(
                        "The model completed this request without a visible reply. {completion_diagnostics}"
                    ));
                }
                send(StreamEvent {
                    delta: Some(answer.clone()),
                    ..Default::default()
                });
                if let Some(reason) = stop_hook_reason(&options, &answer, &mut stop_retries) {
                    input.extend(output);
                    input.push(json!({"role":"developer","content":format!("A Stop hook asked you to keep working before finishing: {reason}")}));
                    continue;
                }
                return Ok(answer);
            }
            if answer.trim().is_empty() {
                answer = completed_text(&output);
                if answer.trim().is_empty() {
                    return Err(format!(
                        "The model completed this request without a visible reply. {completion_diagnostics}"
                    ));
                }
                send(StreamEvent {
                    delta: Some(answer.clone()),
                    ..Default::default()
                });
                visible_delta_sent = true;
            }
            if !visible_delta_sent && !answer.trim().is_empty() {
                send(StreamEvent {
                    delta: Some(answer.clone()),
                    ..Default::default()
                });
            }
            if let Some(reason) = stop_hook_reason(&options, &answer, &mut stop_retries) {
                input.extend(output);
                input.push(json!({"role":"developer","content":format!("A Stop hook asked you to keep working before finishing: {reason}")}));
                continue;
            }
            return Ok(answer);
        }
        // Preserve every output item (including reasoning items) when returning
        // the matching function_call_output items to the Responses API.
        input.extend(output);
        // Whatever the model said alongside its tool calls is progress narration for the user:
        // show it (research prose is held back only for the final answer) and keep paragraphs apart.
        if research_task {
            let narration = std::mem::take(&mut buffered_research_text);
            if !narration.trim().is_empty() {
                answer.push_str(&narration);
                send(StreamEvent { delta: Some(narration), ..Default::default() });
            }
        }
        if !answer.is_empty() && !answer.ends_with("

") {
            answer.push_str("

");
            send(StreamEvent { delta: Some("

".into()), ..Default::default() });
        }
        // Rhea finishing ChatGPT's plan ends the turn here with the reply ChatGPT
        // wrote up front, so a successful task costs exactly one model request.
        let single_call = calls.len() == 1;
        let mut local_finish: Option<String> = None;
        let mut newly_loaded_tools: Vec<Value> = Vec::new();
        let mut exit_compose_after_round = false;
        for call in calls {
            let call_id = call
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or("The model returned a tool call without a call_id.")?;
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .ok_or("The model returned a tool call without a name.")?;
            let raw_arguments = call
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or("The model returned a tool call without arguments.")?;
            let parsed_arguments: Value = serde_json::from_str(raw_arguments)
                .map_err(|e| format!("Invalid arguments for {name}: {e}"))?;
            // The model's `final` bit is a hint, never proof that a research or
            // multi-step request is complete. Keep those plans in the Responses
            // loop so ChatGPT receives Rhea's compact result and can verify,
            // gather more evidence, or continue the work.
            let arguments = if name == "jev_run_plan" {
                enforce_plan_supervision(&parsed_arguments, user_message)
            } else {
                parsed_arguments
            };
            call_sequence = call_sequence.saturating_add(1);
            let mut activity = ToolActivity {
                id: format!("tool-{call_sequence}"),
                name: name.to_owned(),
                arguments_summary: summarize_tool_arguments(name, &arguments),
                result_summary: "Running on this PC…".into(),
                state: "running".into(),
                arguments_detail: bounded_json(&arguments, 12_000),
                result_detail: String::new(),
            };
            send(StreamEvent {
                tool_name: Some(name.to_owned()),
                ..Default::default()
            });
            send(StreamEvent {
                tool_activity: Some(activity.clone()),
                ..Default::default()
            });
            let tool_started = Instant::now();
            let gate_error = if options.hidden_tools.iter().any(|hidden| hidden == name) {
                Some(format!("`{name}` is not available to you in this run."))
            } else if compose_active {
                crate::compose::tool_allowed(name, &arguments).err()
            } else if name == "propose_plan" {
                Some("propose_plan only works while Compose mode is on.".to_owned())
            } else {
                None
            };
            if gate_error.is_none() && name == "propose_plan" {
                if let Some(plan) = arguments.get("plan_markdown").and_then(Value::as_str) {
                    let block = format!("\n\n**Proposed plan**\n\n{}\n\n", plan.trim());
                    answer.push_str(&block);
                    send(StreamEvent { delta: Some(block), ..Default::default() });
                    visible_delta_sent = true;
                }
            }
            let result = if let Some(error) = gate_error {
                json!({"ok":false,"error":error})
            } else if should_block_desktop_send_followup(name, desktop_send_attempted) {
                json!({"ok":false,"error":"desktop_send already ran for this request. Do not repeat or fall back to separate typing, readback, key, or click actions; use the returned status and fresh observation."})
            } else if should_block_low_level_send_input(name, explicit_send_requested) {
                json!({"ok":false,"error":"This request explicitly asks Rhea to send a message. Use desktop_send with the fresh unique composer ref and exact user-authorized text; separate typing or key submission is unavailable."})
            } else {
                if name == "desktop_send" {
                    desktop_send_attempted = true;
                }
                let result = execute_tool(name, &arguments)
                    .map(|mut value| {
                        // `get_tools_` hands back schemas to add to this conversation's
                        // tool list; the model only sees their names in the result.
                        if let Some(Value::Array(schemas)) = value.as_object_mut().and_then(|object| object.remove("load_tools")) {
                            newly_loaded_tools.extend(schemas);
                        }
                        json!({"ok":true,"result":value})
                    })
                    .unwrap_or_else(|error| json!({"ok":false,"error":error}));
                // A plan that stopped before any Return leaves re-planning open;
                // once it may have sent, nothing may send again this request.
                if name == "jev_run_plan"
                    && result
                        .pointer("/result/message_sent")
                        .and_then(Value::as_bool)
                        != Some(false)
                {
                    desktop_send_attempted = true;
                }
                // Only an atomic, verified plan on a request that does not need
                // further reasoning can finish locally. Research and plans with
                // multiple actions always return to ChatGPT for supervision.
                if name == "jev_run_plan"
                    && arguments.get("final").and_then(Value::as_bool) == Some(true)
                {
                    if let Some(reply) = finished_plan_reply(&arguments, &result) {
                        local_finish = Some(reply);
                    }
                }
                // Connection inventory is a direct live fact, not a reasoning
                // task. Return the locally refreshed summary verbatim so the
                // model cannot omit a connected bridge from the answer.
                if name == "list_mcps" && inventory_question {
                    local_finish = result
                        .pointer("/result/answer_hint")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                result
            };
            if let Some(evidence) = research_log.add(name, &result, user_message).cloned() {
                ledger.record(crate::ledger::Event::Evidence { tool: evidence.tool, source: evidence.source, rows: evidence.rows, total: research_log.rows() });
            }
            activity.result_detail = bounded_json(&result, 16_000);
            let succeeded = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
            ledger.record(crate::ledger::Event::Tool { name: name.to_owned(), ok: succeeded, ms: tool_started.elapsed().as_millis() as u64 });
            if !succeeded {
                activity.state = "failed".into();
                activity.result_summary =
                    "Local action failed; details are in Rhea’s response.".into();
            } else if matches!(name, "desktop_snapshot" | "browser__snapshot") {
                activity.state = "fresh_snapshot".into();
                activity.result_summary = summarize_snapshot_result(&result);
                for mut previous_action in actions_waiting_for_snapshot.drain(..) {
                    previous_action.result_summary =
                        if previous_action.name.starts_with("browser__") {
                            "A later browser snapshot captured after this action.".into()
                        } else {
                            "Fresh screen state captured after this action.".into()
                        };
                    previous_action.state = "refreshed".into();
                    send(StreamEvent {
                        tool_activity: Some(previous_action),
                        ..Default::default()
                    });
                }
            } else if is_state_changing_tool(name) {
                if can_complete_with_inline_snapshot(name, &result) {
                    activity.state = "refreshed".into();
                    activity.result_summary = summarize_action_delivery(name, &result);
                    for mut previous_action in actions_waiting_for_snapshot.drain(..) {
                        previous_action.result_summary =
                            if previous_action.name.starts_with("browser__") {
                                "A later browser snapshot captured after this action.".into()
                            } else {
                                "Fresh screen state captured after this action.".into()
                            };
                        previous_action.state = "refreshed".into();
                        send(StreamEvent {
                            tool_activity: Some(previous_action),
                            ..Default::default()
                        });
                    }
                } else {
                    activity.state = "refresh_required".into();
                    activity.result_summary = state_change_result(name);
                    actions_waiting_for_snapshot.push(activity.clone());
                }
            } else {
                activity.state = "complete".into();
                activity.result_summary = summarize_tool_result(name, &result);
            }
            send(StreamEvent {
                tool_activity: Some(activity),
                ..Default::default()
            });
            if name == "propose_plan" && result.pointer("/result/decision").and_then(Value::as_str) == Some("accepted") {
                exit_compose_after_round = true;
            }
            let model_result = model_output(name, &result);
            let model_result = if name == "jev_run_plan"
                && arguments.get("final").and_then(Value::as_bool) != Some(true)
            {
                json!({
                    "supervision_required": true,
                    "instruction": "This Rhea plan is not the final answer. Inspect its result against the user's full goal. If evidence is incomplete, use the available browser or desktop tools to continue; otherwise explain what the evidence confirms and what remains unknown.",
                    "jev_result": model_result
                })
            } else {
                model_result
            };
            input.push(json!({"type":"function_call_output","call_id":call_id,"output":model_result.to_string()}));
        }
        if exit_compose_after_round && compose_active {
            compose_active = false;
            input.push(json!({"role":"developer","content":"Compose mode has ended: the user accepted your plan. All tools are unlocked. Carry the plan out now, verifying as you go, then report what you verified."}));
        }
        if let Some(available) = tools.as_array_mut() {
            for schema in newly_loaded_tools {
                let name = schema.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                if !name.is_empty() && !available.iter().any(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str())) {
                    run.loaded_tools.push(schema.clone());
                    available.push(schema);
                }
            }
        }
        if let (true, Some(reply)) = (single_call, local_finish) {
            answer.push_str(&reply);
            send(StreamEvent {
                delta: Some(reply),
                ..Default::default()
            });
            return Ok(answer);
        }
    }
}

/// What one model round produces, in the shape the rest of the loop (tool dispatch, the research
/// evidence gate, the ledger, compaction) already expects regardless of which provider answered:
/// a Responses-API-shaped `output` array (`{"type":"function_call",...}` / `{"type":"message",...}`
/// items), since Rhea's own local transcript - not provider-held state - is what carries context
/// across a round, a tool call, or a switch from one provider's model to another's mid-chat.
#[derive(Default)]
pub(crate) struct RoundOutcome {
    pub(crate) output: Vec<Value>,
    pub(crate) diagnostics: String,
    pub(crate) usage_tokens: Option<u64>,
    /// Text buffered for a research task, released only once actual evidence has been seen
    /// (never shown as if it were a result on its own).
    pub(crate) research_text: String,
}

/// One ChatGPT round: POST the Responses API, stream its events into `send` and `answer`/
/// `visible_delta_sent`, and return the completed output array.
#[allow(clippy::too_many_arguments)]
fn chatgpt_round(
    runtime: &tokio::runtime::Runtime,
    stream_http: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    model: &str,
    reasoning_effort: &str,
    input: &[Value],
    tools_for_round: &Value,
    tool_choice: &Value,
    endpoint: Option<&str>,
    session_id: Option<&str>,
    fast_on: &mut bool,
    research_task: bool,
    answer: &mut String,
    visible_delta_sent: &mut bool,
    send: &impl Fn(StreamEvent),
) -> Result<RoundOutcome, String> {
    // This private ChatGPT backend refuses `store: true`, so Rhea re-sends its
    // persisted local history on each turn. The stable key is cache affinity, not a
    // server-issued conversation/session id and not a guarantee of server-held history.
    let mut response = loop {
        let mut body = json!({"model":model,"reasoning":{"effort":reasoning_effort},"input":input,"tools":tools_for_round,"tool_choice":tool_choice,"parallel_tool_calls":false,"store":false,"stream":true});
        if let Some(session) = session_id {
            body["prompt_cache_key"] = json!(session);
        }
        if *fast_on {
            body["service_tier"] = json!(FAST_SERVICE_TIER);
        }
        let mut request = stream_http.post(endpoint.unwrap_or(RESPONSES_URL))
            .bearer_auth(access_token)
            .header("originator", "codex_cli_rs")
            .header(reqwest::header::USER_AGENT, format!("Rhea/{}", env!("CARGO_PKG_VERSION")))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(session) = session_id {
            request = request.header("session_id", session);
        }
        if let Some(id) = account_id {
            request = request.header("ChatGPT-Account-ID", id);
        }
        match send_chat_request_with_cancel(runtime, request, crate::jev::cancel_requested) {
            // The backend does not take fast mode for this model or account: go on at normal
            // speed, and say so once instead of failing the run.
            Err(error) if *fast_on && error.contains("service_tier") => {
                *fast_on = false;
                send(StreamEvent {
                    notice: Some("Fast mode is not available for this model or account; continuing at normal speed.".into()),
                    fast_unavailable: true,
                    ..Default::default()
                });
            }
            other => break other?,
        }
    };

    let mut outcome = RoundOutcome::default();
    read_sse_lines_with_cancel(
        runtime,
        &mut response,
        crate::jev::cancel_requested,
        |line| {
            let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                return Ok(());
            };
            if payload.is_empty() || payload == "[DONE]" {
                return Ok(());
            }
            let event: Value = match serde_json::from_str(payload) {
                Ok(value) => value,
                Err(_) => return Ok(()),
            };
            match event.get("type").and_then(Value::as_str).unwrap_or("") {
                "response.output_text.delta" => {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        if research_task {
                            // Do not show a promise like "I'll research this" as
                            // if it were a result. Research prose is released only
                            // after this turn has received actual source evidence.
                            outcome.research_text.push_str(delta);
                        } else {
                            answer.push_str(delta);
                            send(StreamEvent {
                                delta: Some(delta.to_owned()),
                                ..Default::default()
                            });
                            *visible_delta_sent = true;
                        }
                    }
                }
                "response.output_text.done" => {
                    if research_task {
                        if outcome.research_text.is_empty() {
                            if let Some(text) = event.get("text").and_then(Value::as_str) {
                                outcome.research_text.push_str(text);
                            }
                        }
                    } else if answer.is_empty() {
                        if let Some(text) = event.get("text").and_then(Value::as_str) {
                            answer.push_str(text);
                            if !text.is_empty() {
                                send(StreamEvent {
                                    delta: Some(text.to_owned()),
                                    ..Default::default()
                                });
                                *visible_delta_sent = true;
                            }
                        }
                    }
                }
                "response.completed" => {
                    let response_value = event.get("response").unwrap_or(&event);
                    outcome.output = response_value
                        .get("output")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if research_task {
                        if outcome.research_text.is_empty() {
                            if let Some(text) =
                                response_value.get("output_text").and_then(Value::as_str)
                            {
                                outcome.research_text.push_str(text);
                            }
                        }
                    } else if answer.is_empty() {
                        if let Some(text) =
                            response_value.get("output_text").and_then(Value::as_str)
                        {
                            answer.push_str(text);
                        }
                    }
                    outcome.diagnostics = response_diagnostics(response_value);
                    if let Some(usage) = parse_usage(response_value.get("usage")) {
                        outcome.usage_tokens = Some(usage.input_tokens.saturating_add(usage.output_tokens));
                        send(StreamEvent {
                            usage: Some(usage),
                            ..Default::default()
                        });
                    }
                }
                "response.failed" | "response.incomplete" | "error" => {
                    let error = stream_error(&event);
                    send(StreamEvent {
                        error: Some(error.clone()),
                        ..Default::default()
                    });
                    return Err(error);
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    if outcome.diagnostics.is_empty() {
        return Err("The response stream ended before response.completed.".into());
    }
    Ok(outcome)
}

/// The user-facing reply for a plan Rhea completed, or None when the plan
/// stopped and ChatGPT must re-plan from the returned state.
fn finished_plan_reply(arguments: &Value, result: &Value) -> Option<String> {
    let status = result.pointer("/result/status").and_then(Value::as_str)?;
    if status != "completed" {
        return None;
    }
    let planned_steps = arguments.get("steps").and_then(Value::as_array)?;
    let completed_steps = result
        .pointer("/result/completed_steps")
        .and_then(Value::as_array)?;
    // Empty, partial, and submission plans are never eligible for a local
    // completion. ChatGPT must see the result and decide what to tell the user.
    if planned_steps.len() != 1
        || completed_steps.len() != planned_steps.len()
        || result
            .pointer("/result/message_sent")
            .and_then(Value::as_bool)
            != Some(false)
    {
        return None;
    }
    let seconds = result
        .pointer("/result/total_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0) as f64
        / 1000.0;
    let reply = arguments
        .get("done_message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("Done.");
    Some(format!("{reply} ({seconds:.1}s on this PC)"))
}

/// Return a safe plan payload. Multi-step plans and requests that imply
/// research, comparisons, thresholds, or a written summary must be reviewed by
/// ChatGPT after Rhea returns; only a one-action mechanical task may use the
/// fast local completion path.
fn enforce_plan_supervision(arguments: &Value, user_message: &str) -> Value {
    let mut guarded = arguments.clone();
    if plan_needs_supervision(&guarded, user_message) {
        if let Some(object) = guarded.as_object_mut() {
            object.insert("final".into(), Value::Bool(false));
            object.insert("done_message".into(), Value::String(String::new()));
        }
    }
    guarded
}

fn plan_needs_supervision(arguments: &Value, user_message: &str) -> bool {
    let step_count = arguments
        .get("steps")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    step_count != 1 || requires_goal_supervision(user_message)
}

fn requires_goal_supervision(request: &str) -> bool {
    let text = request.to_ascii_lowercase();
    [
        "research",
        "search",
        "look for",
        "find me",
        "find games",
        "browse",
        "compare",
        "summarize",
        "summary",
        "recommend",
        "top ",
        "best ",
        "current",
        "latest",
        "active players",
        "player count",
        "at least",
        "more than",
        "over ",
        "under ",
        "greater than",
        "less than",
        "how many",
        "collect results",
        "across sites",
        "multiple sites",
        "multi-step",
        "step by step",
        "make a list",
        "make a report",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

fn summarize_tool_arguments(name: &str, arguments: &Value) -> String {
    let Some(object) = arguments.as_object() else {
        return "Arguments unavailable".into();
    };
    let mut parts = Vec::new();
    for key in [
        "app",
        "process",
        "window_id",
        "surface",
        "ref",
        "snapshot_id",
        "combo",
        "direction",
        "amount",
        "x",
        "y",
        "timeout_ms",
        "interactive_only",
    ] {
        let Some(value) = object.get(key) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let display = if let Some(text) = value.as_str() {
            let basename = text.rsplit(['\\', '/']).next().unwrap_or(text);
            let bounded: String = basename.chars().take(42).collect();
            format!("{key}={bounded}")
        } else {
            format!("{key}={}", value)
        };
        parts.push(display);
    }
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        if text.chars().count() > 0 {
            parts.push(format!(
                "typed/search text hidden ({} chars)",
                text.chars().count()
            ));
        }
    }
    if parts.is_empty() {
        return format!("{} · no displayable arguments", name.replace('_', " "));
    }
    bounded_summary(parts.join(" · "), 150)
}

fn summarize_snapshot_result(result: &Value) -> String {
    let count = [
        "/result/ref_count",
        "/result/actionable_count",
        "/result/fresh_observation/ref_count",
        "/result/observation/ref_count",
        "/result/snapshot/ref_count",
        "/result/snapshot/actionable_count",
    ]
    .iter()
    .find_map(|path| result.pointer(path).and_then(Value::as_u64));
    match count {
        Some(count) => format!(
            "Fresh page/app snapshot ready · {count} actionable controls; raw tree stays local."
        ),
        None => "Fresh page/app snapshot returned; raw tree stays local.".into(),
    }
}

fn summarize_tool_result(name: &str, result: &Value) -> String {
    if let Some(message) = result.get("error").and_then(Value::as_str) {
        let _ = message;
        return "Local tool failed; details are in Rhea’s response.".into();
    }
    match name {
        "browser__snapshot" => summarize_snapshot_result(result),
        "browser__click" => {
            "Browser click completed; inspect the fresh page state before continuing.".into()
        }
        "browser__type" => "Browser text entry completed; entered text is hidden.".into(),
        "browser__navigate" => {
            "Browser navigation completed; inspect the fresh page state before continuing.".into()
        }
        "desktop_list_apps" => summarize_count(
            result,
            &["/result/data/apps", "/result/apps"],
            "running apps",
        ),
        "desktop_list_windows" => summarize_count(
            result,
            &["/result/data/windows", "/result/windows"],
            "visible windows",
        ),
        "list_processes" => summarize_count(result, &["/result/processes"], "processes"),
        "desktop_find" => result
            .pointer("/result/match_count")
            .and_then(Value::as_u64)
            .map(|count| format!("Control search completed · {count} match(es)."))
            .unwrap_or_else(|| {
                "Control search completed; labels are kept out of the activity log.".into()
            }),
        "desktop_get" => "Control details read; field contents are hidden.".into(),
        "desktop_send" => summarize_desktop_send_result(result),
        "jev_run_plan" => {
            let total = result
                .pointer("/result/total_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let steps = result
                .pointer("/result/completed_steps")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            match result
                .pointer("/result/status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
            {
                "completed" => format!("Rhea ran all {steps} step(s) · {total} ms"),
                "completed_unverified" => format!(
                    "Rhea ran all {steps} step(s) · send unverified, do not retry · {total} ms"
                ),
                _ => {
                    let failed = result
                        .pointer("/result/failed_step/step")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    format!(
                        "Stopped at step {failed} after {steps} step(s) · handing back to ChatGPT · {total} ms"
                    )
                }
            }
        }
        "desktop_screenshot" => {
            "Screenshot saved locally; image pixels were not added to this chat.".into()
        }
        "desktop_wait" => {
            "Wait completed; inspect the next fresh observation before acting.".into()
        }
        _ => "Local tool completed.".into(),
    }
}

fn summarize_desktop_send_result(result: &Value) -> String {
    match result.pointer("/result/status").and_then(Value::as_str).unwrap_or("unknown") {
        "not_submitted" => {
            let failure = result.pointer("/result/type_failure/kind").and_then(Value::as_str).unwrap_or("type_stage_failed");
            format!("Message not submitted · {failure}; no submit key was sent.")
        }
        "submitted_verified" => "Message submitted · delivery confirmed.".into(),
        "submitted_unverified" => "Message submitted once · delivery unverified; do not retry.".into(),
        "submit_failed_or_uncertain" => "Submission attempted once · outcome uncertain; do not retry.".into(),
        _ => "Desktop send returned an unknown status · do not retry until the conversation is checked.".into(),
    }
}

fn summarize_count(result: &Value, paths: &[&str], noun: &str) -> String {
    for path in paths {
        if let Some(count) = result.pointer(path).and_then(Value::as_array).map(Vec::len) {
            return format!("Found {count} {noun}.");
        }
    }
    if let Some(count) = result.pointer("/result/count").and_then(Value::as_u64) {
        return format!("Found {count} {noun}.");
    }
    format!("{noun} list completed.")
}

fn should_block_desktop_send_followup(name: &str, send_attempted: bool) -> bool {
    send_attempted
        && matches!(
            name,
            "jev_run_plan"
                | "desktop_send"
                | "desktop_type"
                | "desktop_get"
                | "desktop_press"
                | "desktop_click"
        )
}

fn is_state_changing_tool(name: &str) -> bool {
    matches!(
        name,
        "open_app"
            | "desktop_launch"
            | "desktop_focus_window"
            | "desktop_click"
            | "desktop_type"
            | "desktop_press"
            | "desktop_mouse_move"
            | "desktop_mouse_click"
            | "desktop_hover"
            | "desktop_scroll"
            | "click_ui_control"
            | "type_text"
            | "press_key"
            | "browser__click"
            | "browser__type"
            | "browser__navigate"
    )
}

fn is_browser_state_changing_tool(name: &str) -> bool {
    matches!(
        name,
        "browser__click" | "browser__type" | "browser__navigate"
    )
}

fn state_change_result(name: &str) -> String {
    match name {
        "desktop_type" | "type_text" => "Text entry requested; entered text is hidden. Waiting for a fresh snapshot.".into(),
        "desktop_press" | "press_key" => "Key delivery requested. Waiting for a fresh snapshot.".into(),
        "desktop_mouse_move" | "desktop_hover" => "Pointer moved. Waiting for a fresh snapshot.".into(),
        "desktop_mouse_click" | "desktop_click" | "click_ui_control" => "Click delivery requested. Waiting for a fresh snapshot.".into(),
        "desktop_scroll" => "Scroll delivery requested. Waiting for a fresh snapshot.".into(),
        "browser__click" => "Browser click requested. Waiting for a later browser snapshot.".into(),
        "browser__type" => "Browser text entry requested; entered text is hidden. Waiting for a later browser snapshot.".into(),
        "browser__navigate" => "Browser navigation requested. Waiting for a later browser snapshot.".into(),
        _ => "App or window state may have changed. Waiting for a fresh snapshot.".into(),
    }
}

fn action_has_fresh_snapshot(result: &Value) -> bool {
    [
        "/result/fresh_observation",
        "/result/observation",
        "/result/data/fresh_observation",
        "/result/snapshot",
    ]
    .iter()
    .filter_map(|path| result.pointer(path))
    .any(|observation| {
        let snapshot_id = observation
            .get("snapshot_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let previous_id = observation
            .get("previous_snapshot_id")
            .and_then(Value::as_str);
        observation.get("kind").and_then(Value::as_str) == Some("desktop_snapshot")
            && observation.get("complete").and_then(Value::as_bool) == Some(true)
            && observation
                .get("local_snapshot_retained")
                .and_then(Value::as_bool)
                == Some(true)
            && !snapshot_id.is_empty()
            && previous_id != Some(snapshot_id)
    })
}

fn can_complete_with_inline_snapshot(name: &str, result: &Value) -> bool {
    !is_browser_state_changing_tool(name) && action_has_fresh_snapshot(result)
}

fn summarize_action_delivery(name: &str, result: &Value) -> String {
    let delivery = ["/result/result/delivery", "/result/delivery"]
        .iter()
        .find_map(|path| result.pointer(path).and_then(Value::as_str));
    let delivery = match delivery {
        Some("delivered") => "delivered",
        Some("not_delivered") => "not delivered",
        Some("unknown") => "delivery uncertain",
        Some(other) => other,
        None => "completed",
    };
    let snapshot_count = [
        "/result/fresh_observation/ref_count",
        "/result/observation/ref_count",
        "/result/result/fresh_observation/ref_count",
    ]
    .iter()
    .find_map(|path| result.pointer(path).and_then(Value::as_u64));
    let snapshot = snapshot_count
        .map(|count| format!(" Fresh state: {count} actionable controls."))
        .unwrap_or_else(|| " Fresh state returned.".into());
    match name {
        "browser__click" => format!("Browser click {delivery}; waiting for a later page snapshot."),
        "browser__type" => format!(
            "Browser text entry {delivery}; content hidden. Waiting for a later page snapshot."
        ),
        "browser__navigate" => {
            format!("Browser navigation {delivery}; waiting for a later page snapshot.")
        }
        "desktop_type" | "type_text" => format!("Text entry {delivery}; content hidden.{snapshot}"),
        "desktop_press" | "press_key" => format!("Key action {delivery}.{snapshot}"),
        "desktop_mouse_move" | "desktop_hover" => format!("Pointer action {delivery}.{snapshot}"),
        "desktop_scroll" => format!("Scroll action {delivery}.{snapshot}"),
        _ => format!("Click/app action {delivery}.{snapshot}"),
    }
}

fn bounded_summary(summary: String, limit: usize) -> String {
    let mut output: String = summary.chars().take(limit).collect();
    if summary.chars().count() > limit {
        output.push('…');
    }
    output
}

fn bounded_json(value: &Value, limit: usize) -> String {
    let serialized = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    if serialized.chars().count() <= limit {
        return serialized;
    }
    format!(
        "{}\n… truncated locally …",
        serialized.chars().take(limit).collect::<String>()
    )
}

/// What the model sees of a tool result. Desktop (Rhea) results are trimmed hard: raw accessibility
/// trees stay local and every string is cut short. Everything else (Shell file and command output,
/// Chrome pages, MCP tools, agent reports, skills) is real content the model asked for, so it
/// passes through whole up to a generous cap instead of being clipped to a few hundred characters.
fn model_output(name: &str, result: &Value) -> Value {
    const LIMIT: usize = 60_000;
    let passes_whole = name.starts_with("mcp__")
        || name.starts_with("browser__")
        || name.starts_with("agent_")
        || matches!(name, "use_skill" | "install_skill" | "laya_decide" | "memory_search" | "memory_remember" | "memory_forget" | "list_mcps" | "ask_user" | "propose_plan");
    if !passes_whole {
        return compact_agent_output(result);
    }
    let text = result.to_string();
    let length = text.chars().count();
    if length <= LIMIT {
        return result.clone();
    }
    json!({
        "truncated": true,
        "note": format!("The result was {length} characters; only the first {LIMIT} are shown. Ask for a narrower slice (read_file `ranges`, a filter, a smaller page)."),
        "partial": text.chars().take(LIMIT).collect::<String>(),
    })
}

pub(crate) fn compact_agent_output(value: &Value) -> Value {
    fn compact(value: &Value, remaining: &mut usize, depth: usize) -> Value {
        if *remaining == 0 || depth > 8 {
            return Value::String("[local result trimmed]".into());
        }
        *remaining -= 1;
        if value.get("tree").is_some()
            && (value.get("snapshot_id").is_some() || value.get("complete").is_some())
        {
            return crate::desktop::snapshot_summary(value);
        }
        match value {
            Value::Object(object) => {
                let mut safe = serde_json::Map::new();
                for (key, item) in object.iter().take(64) {
                    // Raw accessibility trees remain local to Rhea and are omitted from API history.
                    if matches!(
                        key.as_str(),
                        "tree" | "raw_tree" | "pixels" | "image_base64"
                    ) {
                        continue;
                    }
                    safe.insert(key.clone(), compact(item, remaining, depth + 1));
                    if *remaining == 0 {
                        break;
                    }
                }
                Value::Object(safe)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| compact(item, remaining, depth + 1))
                    .collect(),
            ),
            Value::String(text) => {
                let mut bounded: String = text.chars().take(240).collect();
                if text.chars().count() > 240 {
                    bounded.push('…');
                }
                Value::String(bounded)
            }
            _ => value.clone(),
        }
    }
    // Keep whole result lists searchable by the model. The former 48-item cap
    // silently dropped late rows (for example, a DM near the bottom of a large
    // list). Accessibility trees are still kept local and summarized separately.
    let mut budget = 16_384;
    let safe = compact(value, &mut budget, 0);
    if serde_json::to_vec(&safe)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
        <= 96_000
    {
        safe
    } else {
        json!({"truncated":true,"summary":"Agent Desktop returned a large result. Raw accessibility data stays local; take a narrower app/window snapshot."})
    }
}

fn compact_history(
    access_token: &str,
    account_id: Option<&str>,
    model: &str,
    prior_summary: &str,
    session_id: Option<&str>,
    old_turns: &[Turn],
    http: &Client,
    send: &impl Fn(StreamEvent),
) -> Result<String, String> {
    let mut input = vec![
        json!({"role":"developer","content":system_prompt()}),
        json!({"role":"developer","content":"Summarize older completed conversation turns for Rhea. Preserve user goals, exact names and text still needed, decisions, constraints, safety boundaries, pending work, and verified outcomes. Remove incidental screen content and secrets. Do not perform desktop actions. Return a compact factual memory only."}),
    ];
    if !prior_summary.trim().is_empty() {
        input.push(json!({"role":"developer","content":format!("Existing compacted memory:\n{}", prior_summary.trim())}));
    }
    input.extend(
        old_turns
            .iter()
            .map(|turn| json!({"role":turn.role,"content":turn_model_text(turn)})),
    );
    input.push(
        json!({"role":"user","content":"Create the updated compacted conversation memory now."}),
    );

    let mut body = json!({"model":model,"input":input,"tools":[],"tool_choice":"none","parallel_tool_calls":false,"store":false,"stream":true});
    if let Some(session) = session_id {
        body["prompt_cache_key"] = json!(session);
    }
    let mut request = http.post("https://chatgpt.com/backend-api/codex/responses")
        .bearer_auth(access_token)
        .header("originator", "codex_cli_rs")
        .header(reqwest::header::USER_AGENT, format!("Rhea/{}", env!("CARGO_PKG_VERSION")))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&body);
    if let Some(session) = session_id {
        request = request.header("session_id", session);
    }
    if let Some(id) = account_id {
        request = request.header("ChatGPT-Account-ID", id);
    }
    let mut response = request
        .send()
        .map_err(|error| format!("Context compaction request failed: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!(
            "Context compaction returned HTTP {status}: {}",
            concise_error(&body)
        ));
    }
    let mut reader = BufReader::new(&mut response);
    let mut line = String::new();
    let mut output: Option<Vec<Value>> = None;
    let mut summary = String::new();
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|error| format!("Context compaction stream failed: {error}"))?
            == 0
        {
            break;
        }
        let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let event: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(_) => continue,
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    summary.push_str(delta);
                }
            }
            "response.completed" => {
                let response_value = event.get("response").unwrap_or(&event);
                output = response_value
                    .get("output")
                    .and_then(Value::as_array)
                    .cloned();
                if let Some(usage) = parse_usage(response_value.get("usage")) {
                    send(StreamEvent {
                        usage: Some(usage),
                        ..Default::default()
                    });
                }
            }
            "response.failed" | "response.incomplete" | "error" => {
                let error = event
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("The context summary did not complete");
                return Err(format!("Could not compact conversation context: {error}"));
            }
            _ => {}
        }
    }
    let output = output.ok_or("Context compaction ended before response.completed.")?;
    if summary.trim().is_empty() {
        summary = completed_text(&output);
    }
    if summary.trim().is_empty() {
        return Err("Context compaction completed without a summary.".into());
    }
    Ok(summary.trim().to_owned())
}

/// The items a run continues from: the saved model transcript when it still accounts for exactly
/// the visible turns, otherwise the turns' own text (with tool digests). Also where each real user
/// message sits in those items.
fn resume_items(saved: &crate::chat_store::ModelState, history: &[Turn]) -> (Vec<Value>, Vec<usize>) {
    if !saved.items.is_empty() && saved.turns == history.len() {
        return (saved.items.clone(), saved.turn_starts.clone());
    }
    let starts = history.iter().enumerate().filter(|(_, turn)| turn.role == "user").map(|(index, _)| index).collect();
    (turn_items(history), starts)
}

/// The developer messages every request starts with.
fn base_input(summary: &str) -> Vec<Value> {
    let mut input = vec![
        json!({"role":"developer","content":system_prompt()}),
        json!({"role":"developer","content":"ChatGPT plans; Rhea tools execute. Use fresh Agent Desktop observations before every state-changing step."}),
    ];
    if !summary.trim().is_empty() {
        input.push(json!({"role":"developer","content":format!("Earlier conversation summary (the full local transcript is preserved):\n{}", summary.trim())}));
    }
    input
}

/// A transcript's turns as plain messages (assistant turns carry their tool digest).
fn turn_items(history: &[Turn]) -> Vec<Value> {
    history.iter().map(|turn| json!({"role":turn.role,"content":turn_model_text(turn)})).collect()
}

#[cfg(test)]
fn conversation_input(summary: &str, history: &[Turn], user_message: &str) -> Vec<Value> {
    let mut input = base_input(summary);
    input.extend(turn_items(history));
    input.push(json!({"role":"user","content":user_message}));
    input
}

fn item_chars(item: &Value) -> usize {
    item.to_string().len()
}

fn estimate_items(items: &[Value]) -> u64 {
    items.iter().map(|item| (item_chars(item) as u64).saturating_add(3) / 4 + 4).sum()
}

/// Where to cut the oldest turns off for compaction: keep the last two turns, summarize everything
/// before the one that starts the pair. Returns (item index to cut at, turns summarized).
fn compaction_cut(starts: &[usize]) -> Option<(usize, usize)> {
    if starts.len() < 4 {
        return None;
    }
    let keep_from = starts.len() - 2;
    Some((starts[keep_from], keep_from * 2))
}

fn clip_text(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// Plain text of a message item's content (a string, or a list of text parts).
fn message_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Items as readable turns for the summarizer.
fn items_to_turns(items: &[Value]) -> Vec<Turn> {
    items
        .iter()
        .filter_map(|item| {
            let (role, content) = match item.get("type").and_then(Value::as_str) {
                Some("function_call") => (
                    "assistant",
                    format!(
                        "[called tool {} with {}]",
                        item.get("name").and_then(Value::as_str).unwrap_or("?"),
                        clip_text(item.get("arguments").and_then(Value::as_str).unwrap_or(""), 300)
                    ),
                ),
                Some("function_call_output") => ("user", format!("[tool result] {}", clip_text(item.get("output").and_then(Value::as_str).unwrap_or(""), 1_500))),
                Some("message") => ("assistant", message_text(item)),
                _ => match item.get("role").and_then(Value::as_str) {
                    Some(role @ ("user" | "assistant")) => (role, message_text(item)),
                    _ => return None,
                },
            };
            Some(Turn { role: role.into(), content, ..Default::default() })
        })
        .collect()
}

/// The part of a run's input worth keeping for the next run: messages, tool calls and their results.
/// Developer notes (rebuilt every run) and reasoning items (not needed to continue) are dropped, a
/// call without its result (or a result without its call) is dropped so the next request is valid,
/// and very long tool output is capped. Returns the items and the new indexes of the real user messages.
fn persistable_items(input: &[Value], user_positions: &[usize]) -> (Vec<Value>, Vec<usize>) {
    const MAX_OUTPUT_CHARS: usize = 24_000;
    let mut kept: Vec<(usize, Value)> = Vec::new();
    for (index, item) in input.iter().enumerate() {
        let keep = match item.get("type").and_then(Value::as_str) {
            Some("function_call") | Some("function_call_output") | Some("message") => true,
            None => matches!(item.get("role").and_then(Value::as_str), Some("user" | "assistant")),
            Some(_) => false,
        };
        if keep {
            kept.push((index, item.clone()));
        }
    }
    let calls: std::collections::HashSet<String> = kept
        .iter()
        .filter(|(_, item)| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(|(_, item)| item.get("call_id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let outputs: std::collections::HashSet<String> = kept
        .iter()
        .filter(|(_, item)| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|(_, item)| item.get("call_id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    kept.retain(|(_, item)| match item.get("type").and_then(Value::as_str) {
        Some("function_call") => item.get("call_id").and_then(Value::as_str).is_some_and(|id| outputs.contains(id)),
        Some("function_call_output") => item.get("call_id").and_then(Value::as_str).is_some_and(|id| calls.contains(id)),
        _ => true,
    });
    let mut items = Vec::with_capacity(kept.len());
    let mut starts = Vec::new();
    for (original, mut item) in kept {
        if user_positions.contains(&original) {
            starts.push(items.len());
        }
        if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            if let Some(output) = item.get("output").and_then(Value::as_str) {
                if output.chars().count() > MAX_OUTPUT_CHARS {
                    item["output"] = json!(format!("{}\n…[output capped at {MAX_OUTPUT_CHARS} characters]", output.chars().take(MAX_OUTPUT_CHARS).collect::<String>()));
                }
            }
        }
        items.push(item);
    }
    (items, starts)
}

/// Summarize the oldest part of a chat's saved transcript on demand (`/compact`). Returns the new
/// summary, the items that remain, their turn starts, and how many turns were summarized.
pub fn compact_items(
    access_token: &str,
    account_id: Option<&str>,
    model: &str,
    prior_summary: &str,
    session_id: Option<&str>,
    state: &crate::chat_store::ModelState,
) -> Result<(String, crate::chat_store::ModelState, usize), String> {
    let (cut, summarized_turns) = compaction_cut(&state.turn_starts).ok_or("Nothing to compact yet.")?;
    let http = Client::builder().timeout(Duration::from_secs(180)).build().map_err(|e| e.to_string())?;
    let summary = compact_history(access_token, account_id, model, prior_summary, session_id, &items_to_turns(&state.items[..cut]), &http, &|_| {})?;
    let remaining = crate::chat_store::ModelState {
        items: state.items[cut..].to_vec(),
        loaded_tools: state.loaded_tools.clone(),
        turn_starts: state.turn_starts.iter().filter(|start| **start >= cut).map(|start| start - cut).collect(),
        turns: state.turns.saturating_sub(summarized_turns),
    };
    Ok((summary, remaining, summarized_turns))
}

/// Rough token counts for the pieces `/compact` reports (about four characters per token).
pub fn estimate_text_tokens(text: &str) -> u64 {
    estimate_tokens(text)
}

pub fn estimate_turn_tokens(turns: &[Turn]) -> u64 {
    estimate_turns(turns)
}

pub fn estimate_state_tokens(state: &crate::chat_store::ModelState) -> u64 {
    estimate_items(&state.items)
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).saturating_add(3) / 4
}

pub fn estimate_context_tokens(summary: &str, history: &[Turn], user_message: &str) -> u64 {
    estimate_tokens(system_prompt())
        .saturating_add(estimate_tokens("ChatGPT plans; Rhea tools execute. Use fresh Agent Desktop observations before every state-changing step."))
        .saturating_add(estimate_tokens(summary))
        .saturating_add(estimate_turns(history))
        .saturating_add(estimate_tokens(user_message))
}

pub fn system_prompt_tokens() -> u64 {
    estimate_tokens(system_prompt())
}

fn estimate_turns(turns: &[Turn]) -> u64 {
    turns
        .iter()
        .map(|turn| (turn.content.len() as u64 + digest_size_estimate(turn)).saturating_add(3) / 4 + 4)
        .sum()
}

const DIGEST_PER_CALL: usize = 700;
const DIGEST_ARGS: usize = 220;
const DIGEST_TOTAL: usize = 16_000;

fn excerpt(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

/// Bytes the tool digest of a turn adds to the model's context, without building it (the context
/// gauge asks every frame).
fn digest_size_estimate(turn: &Turn) -> u64 {
    if turn.role != "assistant" || turn.tool_activity.is_empty() {
        return 0;
    }
    let mut total = 160_usize;
    for activity in turn.tool_activity.iter().rev() {
        let args = if activity.arguments_detail.is_empty() { activity.arguments_summary.len() } else { activity.arguments_detail.len() };
        let result = if activity.result_detail.is_empty() { activity.result_summary.len() } else { activity.result_detail.len() };
        let line = args.min(DIGEST_ARGS) + result.min(DIGEST_PER_CALL) + activity.name.len() + 24;
        if total + line > DIGEST_TOTAL {
            break;
        }
        total += line;
    }
    total as u64
}

/// What the model is told a past turn contained: for an assistant turn, its reply plus a short
/// digest of the tool calls it made and what they returned, so a follow-up message still knows
/// what was already done and found instead of starting from the bare text.
pub fn turn_model_text(turn: &Turn) -> String {
    if turn.role != "assistant" || turn.tool_activity.is_empty() {
        return turn.content.clone();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0_usize;
    let mut omitted = 0_usize;
    for activity in turn.tool_activity.iter().rev() {
        let args = excerpt(if activity.arguments_detail.is_empty() { &activity.arguments_summary } else { &activity.arguments_detail }, DIGEST_ARGS);
        let result = excerpt(if activity.result_detail.is_empty() { &activity.result_summary } else { &activity.result_detail }, DIGEST_PER_CALL);
        let line = format!("- {} {args} -> [{}] {result}", activity.name, activity.state);
        if omitted > 0 || used + line.len() > DIGEST_TOTAL {
            omitted += 1;
            continue;
        }
        used += line.len();
        lines.push(line);
    }
    lines.reverse();
    let note = if omitted > 0 { format!("; {omitted} earlier calls omitted") } else { String::new() };
    format!(
        "{}\n\n[Tool calls made during this reply — results shortened{note}; re-run a tool for full output]\n{}",
        turn.content,
        lines.join("\n")
    )
}

fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let input_tokens = value.get("input_tokens")?.as_u64()?;
    let details = value.get("input_tokens_details");
    Some(Usage {
        input_tokens,
        cached_tokens: details
            .and_then(|v| v.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: details
            .and_then(|v| v.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_reported: details
            .and_then(|v| v.get("cached_tokens"))
            .and_then(Value::as_u64)
            .is_some(),
        cache_write_reported: details
            .and_then(|v| v.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .is_some(),
    })
}

fn model_context_window(model: &Value) -> Option<u64> {
    [
        "context_window",
        "context_length",
        "max_input_tokens",
        "input_token_limit",
    ]
    .iter()
    .find_map(|key| {
        model
            .get(key)
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
            })
            .filter(|value| *value >= 16_000)
    })
}

fn local_tools() -> Value {
    serde_json::from_str(include_str!("toolset.json"))
        .expect("src/toolset.json must contain valid OpenAI function tools")
}

/// Build a bounded Responses API function tool. Use namespace-qualified `mcp__server__tool`
/// or `browser__tool` names; callers must keep names unique across available tools.
pub fn function_tool(name: &str, description: &str, schema: &Value) -> Result<Value, String> {
    if !valid_dynamic_function_name(name) {
        return Err("Function tool names must use mcp__server__tool or browser__tool form with ASCII letters, digits, or underscores.".into());
    }
    if description.chars().count() > 1024 {
        return Err("Function tool descriptions are limited to 1024 characters.".into());
    }
    if !schema.is_object() {
        return Err("Function tool parameters must be a JSON Schema object.".into());
    }
    let schema_size = serde_json::to_vec(schema)
        .map_err(|error| error.to_string())?
        .len();
    if schema_size > 32_000 {
        return Err("Function tool parameter schemas are limited to 32000 bytes.".into());
    }
    Ok(
        json!({"type":"function","name":name,"description":description,"strict":false,"parameters":schema}),
    )
}

fn valid_dynamic_function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && (name.starts_with("mcp__") && name.split("__").count() >= 3
            || name.starts_with("browser__") && name.split("__").count() >= 2)
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn merged_tools(extra_tools: &[Value]) -> Result<Value, String> {
    let mut tools = local_tools()
        .as_array()
        .cloned()
        .ok_or("Built-in toolset must be an array")?;
    let mut names: std::collections::HashSet<String> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for tool in extra_tools {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            return Err("Dynamic tools must use the Responses API function type.".into());
        }
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or("Dynamic function tool is missing its name")?;
        if !valid_dynamic_function_name(name) {
            return Err(format!(
                "Dynamic function tool '{name}' must use a namespace-qualified mcp__server__tool or browser__tool name."
            ));
        }
        if !names.insert(name.to_owned()) {
            return Err(format!(
                "Function tool name '{name}' conflicts with another available tool."
            ));
        }
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        if description.chars().count() > 1024 {
            return Err(format!(
                "Function tool '{name}' description exceeds 1024 characters."
            ));
        }
        let schema = tool
            .get("parameters")
            .ok_or_else(|| format!("Function tool '{name}' is missing its parameter schema"))?;
        if !schema.is_object()
            || serde_json::to_vec(schema)
                .map_err(|error| error.to_string())?
                .len()
                > 32_000
        {
            return Err(format!(
                "Function tool '{name}' has an invalid or oversized parameter schema."
            ));
        }
        tools.push(tool.clone());
    }
    Ok(Value::Array(tools))
}

fn tools_for_request(extra_tools: &[Value], user_message: &str) -> Result<Value, String> {
    let mut tools = merged_tools(extra_tools)?;
    if has_explicit_send_intent(user_message) {
        if let Some(available) = tools.as_array_mut() {
            available.retain(|tool| {
                !matches!(
                    tool.get("name").and_then(Value::as_str),
                    Some("desktop_type" | "desktop_press" | "desktop_mouse_click")
                )
            });
        }
    }
    Ok(tools)
}

/// Conservative routing signal: user asks for a send/message action *and* a
/// destination or message object. Negated requests are excluded. This only
/// narrows the tool schema; it never authorizes an action by itself.
fn has_explicit_send_intent(message: &str) -> bool {
    let lower = message
        .to_lowercase()
        .replace("don't", "dont")
        .replace("don’t", "dont");
    let prefix = lower.split(['\'', '"', '“', '‘']).next().unwrap_or(&lower);
    let tokenize = |text: &str| {
        let normalized = text.replace('’', " ").replace('–', " ").replace('—', " ");
        normalized
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let tokens = tokenize(prefix);
    let all_tokens = tokenize(&lower);
    if tokens
        .get(0..3)
        .map(|parts| parts.iter().map(String::as_str).eq(["how", "do", "i"]))
        .unwrap_or(false)
        || tokens
            .get(0..3)
            .map(|parts| parts.iter().map(String::as_str).eq(["how", "can", "i"]))
            .unwrap_or(false)
    {
        return false;
    }
    if tokens
        .windows(2)
        .any(|pair| pair[0] == "dont" && matches!(pair[1].as_str(), "send" | "message" | "dm"))
        || tokens.windows(3).any(|triple| {
            triple[0] == "do"
                && triple[1] == "not"
                && matches!(triple[2].as_str(), "send" | "message" | "dm")
        })
        || tokens
            .windows(2)
            .any(|pair| pair[0] == "without" && matches!(pair[1].as_str(), "sending" | "messaging"))
    {
        return false;
    }
    let has_verb = tokens
        .iter()
        .any(|token| matches!(token.as_str(), "send" | "sending" | "submit"))
        || tokens.windows(2).any(|pair| {
            pair[0] == "message"
                && !matches!(
                    pair[1].as_str(),
                    "box" | "composer" | "history" | "window" | "app" | "panel" | "button"
                )
        })
        || tokens.windows(2).any(|pair| {
            pair[0] == "dm"
                && !matches!(
                    pair[1].as_str(),
                    "app" | "window" | "conversation" | "history"
                )
        })
        || (tokens.iter().any(|token| token == "text") && has_destination_token(&all_tokens));
    let has_destination_or_payload = all_tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "to" | "my"
                | "her"
                | "him"
                | "them"
                | "recipient"
                | "conversation"
                | "message"
                | "email"
        )
    }) || (all_tokens.iter().any(|token| token == "dm")
        && all_tokens.len() >= 3)
        || (tokens.first().map(String::as_str) == Some("message") && all_tokens.len() >= 3);
    has_verb && has_destination_or_payload
}

fn has_destination_token(tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "to" | "my" | "her" | "him" | "them" | "recipient" | "conversation"
        )
    })
}

fn should_block_low_level_send_input(name: &str, explicit_send_requested: bool) -> bool {
    explicit_send_requested
        && matches!(
            name,
            "desktop_type" | "desktop_press" | "desktop_mouse_click"
        )
}

fn completed_text(output: &[Value]) -> String {
    let mut text = String::new();
    for item in output {
        if let Some(value) = item.get("text").and_then(Value::as_str) {
            text.push_str(value);
            continue;
        }
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for part in content {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "output_text" | "text" => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    "refusal" => {
                        if let Some(value) = part.get("refusal").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    _ => {}
                }
            }
        } else if let Some(value) = item.get("refusal").and_then(Value::as_str) {
            text.push_str(value);
        }
    }
    text
}

fn stream_error(event: &Value) -> String {
    let message = event
        .pointer("/response/error/message")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/error/message").and_then(Value::as_str));
    if let Some(message) = message.filter(|message| !message.trim().is_empty()) {
        return message.to_owned();
    }
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("error");
    let response = event.get("response").unwrap_or(event);
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let reason = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .map(|reason| format!(", reason={reason}"))
        .unwrap_or_default();
    format!("ChatGPT returned {kind} (status={status}{reason}) without a complete answer")
}

fn response_diagnostics(response: &Value) -> String {
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let items = response
        .get("output")
        .and_then(Value::as_array)
        .map(|output| {
            if output.is_empty() {
                return "empty".to_owned();
            }
            output
                .iter()
                .map(|item| {
                    let kind = item
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown-item");
                    let parts = item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|content| {
                            content
                                .iter()
                                .map(|part| {
                                    part.get("type")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown-part")
                                })
                                .collect::<Vec<_>>()
                                .join("+")
                        })
                        .unwrap_or_default();
                    if parts.is_empty() {
                        kind.to_owned()
                    } else {
                        format!("{kind}[{parts}]")
                    }
                })
                .take(8)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "missing".into());
    format!("(response={response_id}, status={status}, output={items})")
}

fn concise_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(400).collect())
}

/// Force a live local inventory lookup when the user asks for integration
/// connectivity. This prevents the model from answering from saved context
/// when a browser bridge is not an MCP child process.
fn is_connection_inventory_question(message: &str) -> bool {
    let text = message.trim().to_lowercase();
    // Direct status questions are short; briefs and coding requests are not.
    if text.chars().count() > 160 || text.lines().count() > 2 {
        return false;
    }
    let asks_for_status = ["connected", "connection", "online", "healthy", "health", "available", "enabled", "status", "list"]
        .iter()
        .any(|word| text.contains(word));
    let names_integration = ["mcp", "chrome", "browser", "integration"].iter().any(|word| text.contains(word));
    let question_form = text.ends_with('?')
        || ["what", "which", "is ", "are ", "list", "show", "check", "do you have", "can you see"].iter().any(|start| text.starts_with(start));
    asks_for_status && names_integration && question_form
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_same_result_twice_is_one_piece_of_research_evidence() {
        let page = json!({"ok": true, "text": "Game A has 5,000 players and Game B has 7,000 players this week"});
        let same = json!({"ok": true, "text": "Game A has 5,000 players and Game B has 7,000 players this week"});
        let other = json!({"ok": true, "text": "Game C has 9,000 players and Game D has 12,000 players this week"});
        assert_eq!(evidence_fingerprint(&page), evidence_fingerprint(&same));
        assert_ne!(evidence_fingerprint(&page), evidence_fingerprint(&other));
    }

    use super::*;

    #[test]
    fn sse_line_buffer_preserves_split_lines_and_crlf() {
        let mut buffer = SseLineBuffer::default();
        assert!(buffer.push(b"data: response.out").is_empty());
        assert_eq!(
            buffer.push(b"put_text.delta\r\ndata: [DONE]\n"),
            vec!["data: response.output_text.delta", "data: [DONE]"]
        );
        assert!(buffer.push(b"data: final").is_empty());
        assert_eq!(buffer.finish().as_deref(), Some("data: final"));
    }

    #[test]
    fn idle_sse_stream_can_be_cancelled_promptly_and_drops_the_response() {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            net::TcpListener,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request_reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                if request_reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            drop(request_reader);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            socket.flush().unwrap();

            // Reqwest may retain a reusable HTTP/1 connection after discarding
            // an incomplete body. Keep the mock server bounded without making
            // connection-pool policy part of the cancellation contract.
            let mut byte = [0_u8; 1];
            let _ = socket.read(&mut byte);
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::builder().build().unwrap();
        let mut response = runtime
            .block_on(client.get(format!("http://{address}/events")).send())
            .unwrap();
        assert!(response.status().is_success());

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_signal = Arc::clone(&cancelled);
        let cancellation_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(75));
            cancel_signal.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let error = read_sse_lines_with_cancel(
            &runtime,
            &mut response,
            || cancelled.load(Ordering::SeqCst),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error, "Run cancelled.");
        assert!(started.elapsed() < Duration::from_secs(1));

        drop(response);
        cancellation_thread.join().unwrap();
    }

    #[test]
    fn only_a_verified_atomic_plan_can_finish_locally() {
        let arguments =
            json!({"steps":[{"do":"open_app","app":"Discord"}],"done_message":"Opened Discord."});
        let done = json!({"ok":true,"result":{"status":"completed","message_sent":false,"completed_steps":[{"step":1}],"total_ms":4200}});
        assert_eq!(
            finished_plan_reply(&arguments, &done).as_deref(),
            Some("Opened Discord. (4.2s on this PC)")
        );
        let unverified = json!({"ok":true,"result":{"status":"completed_unverified","message_sent":true,"completed_steps":[{"step":1}],"total_ms":1000}});
        assert_eq!(finished_plan_reply(&arguments, &unverified), None);
        let empty = json!({"steps":[],"done_message":"Sent it."});
        assert_eq!(finished_plan_reply(&empty, &done), None);
        assert!(
            crate::jev::run_plan(&empty)
                .unwrap_err()
                .contains("1 to 25 steps"),
            "Rhea must reject a zero-step plan before touching the desktop"
        );
        let multi = json!({"steps":[{"do":"open_app"},{"do":"click"}],"done_message":"Done."});
        let multi_done = json!({"ok":true,"result":{"status":"completed","message_sent":false,"completed_steps":[{"step":1},{"step":2}],"total_ms":1000}});
        assert_eq!(finished_plan_reply(&multi, &multi_done), None);
        let incomplete = json!({"ok":true,"result":{"status":"completed","message_sent":false,"completed_steps":[],"total_ms":1000}});
        assert_eq!(finished_plan_reply(&arguments, &incomplete), None);
        let stopped = json!({"ok":true,"result":{"status":"stopped"}});
        assert_eq!(finished_plan_reply(&arguments, &stopped), None);
        assert_eq!(
            finished_plan_reply(&arguments, &json!({"ok":false,"error":"x"})),
            None
        );
        assert!(should_block_desktop_send_followup("jev_run_plan", true));
        assert!(!should_block_desktop_send_followup("jev_run_plan", false));
    }

    #[test]
    fn research_and_multi_step_plans_are_forced_back_to_chatgpt() {
        let research =
            "Find Roblox games with more than 1K active players and summarize the genres";
        let single =
            json!({"steps":[{"do":"open_app","app":"Chrome"}],"done_message":"Done.","final":true});
        assert!(plan_needs_supervision(&single, research));
        let guarded = enforce_plan_supervision(&single, research);
        assert_eq!(guarded.get("final").and_then(Value::as_bool), Some(false));
        assert_eq!(
            guarded.get("done_message").and_then(Value::as_str),
            Some("")
        );

        let multi =
            json!({"steps":[{"do":"open_app"},{"do":"click"}],"done_message":"Done.","final":true});
        assert!(plan_needs_supervision(
            &multi,
            "Open Discord and focus the window"
        ));
        assert_eq!(
            enforce_plan_supervision(&multi, "Open Discord")["final"],
            false
        );
        assert!(
            !plan_needs_supervision(&single, "Open Discord"),
            "a simple one-action request stays fast"
        );
    }

    #[test]
    fn goal_supervision_detects_thresholds_current_data_and_summaries() {
        for request in [
            "Find games with more than 1K active players",
            "Search current active player counts",
            "Summarize these options",
            "Compare the best results across sites",
        ] {
            assert!(
                requires_goal_supervision(request),
                "missing supervision for: {request}"
            );
        }
        assert!(!requires_goal_supervision("Open Discord"));
    }

    #[test]
    fn explicit_research_cannot_complete_from_a_plan_or_an_unverified_promise() {
        let user = "Find Roblox games with more than 1K active players and summarize their genres";
        let plan =
            json!({"ok":true,"result":{"status":"completed","completed_steps":[{"step":1}]}});
        assert!(requires_research_evidence(user));
        assert_eq!(
            research_evidence_rows_from_tool("jev_run_plan", &plan, user),
            0
        );
        assert_eq!(
            research_evidence_rows_from_tool(
                "browser__extract",
                &json!({"ok":false,"error":"unavailable"}),
                user
            ),
            0
        );
        assert_eq!(
            research_evidence_rows_from_tool(
                "browser__extract",
                &json!({"ok":true,"result":{"text":"No results found."}}),
                user
            ),
            0
        );
        assert!(
            research_blocker_reply(&EvidenceLog::default(), 2)
                .contains("haven’t confirmed that the requested threshold or result count was met")
        );
        assert_eq!(
            research_completion_gate(true, 0, 2, 0),
            ResearchCompletion::Retry
        );
        assert_eq!(
            research_completion_gate(true, 0, 2, 1),
            ResearchCompletion::Block
        );
        assert_eq!(
            research_completion_gate(true, 2, 2, 0),
            ResearchCompletion::Accept
        );
        assert_eq!(
            research_completion_gate(false, 0, 1, 0),
            ResearchCompletion::Accept
        );
    }

    #[test]
    fn research_gate_requires_real_metric_data_for_numeric_thresholds() {
        let user = "Find games with more than 1K active players";
        let no_metric = json!({"ok":true,"result":{"text":"Blox Fruits is a popular Roblox game with an active community."}});
        let one_row =
            json!({"ok":true,"result":{"text":"Blox Fruits — 12,345 active players now."}});
        let two_rows = json!({"ok":true,"result":{"text":"Blox Fruits — 12,345 active players now. Tower of Hell — 2.4K players playing."}});
        assert_eq!(
            research_evidence_rows_from_tool("browser__extract", &no_metric, user),
            0
        );
        assert_eq!(
            research_evidence_rows_from_tool("browser__extract", &one_row, user),
            1
        );
        assert_eq!(
            research_evidence_rows_from_tool("browser__extract", &two_rows, user),
            2
        );
        assert_eq!(
            research_evidence_rows_from_tool("desktop_list_apps", &two_rows, user),
            0
        );
    }

    #[test]
    fn research_threshold_uses_the_comparison_number_not_requested_result_count() {
        assert_eq!(
            requested_threshold("Find 10 games with more than 1K active players"),
            Some(1_000.0)
        );
        assert_eq!(
            requested_threshold("Find at least 5 options under 2.5K users"),
            Some(2_500.0)
        );
        assert_eq!(
            requested_threshold("Find games with more than 1,000 active players"),
            Some(1_000.0)
        );
        assert_eq!(requested_threshold("Compare 5 games from 3 websites"), None);
    }

    #[test]
    #[ignore]
    fn live_plan_navigation_only() {
        // RHEA_PLAN='<steps json>' cargo test --bin rhea live_plan -- --ignored --nocapture
        let steps: Value = serde_json::from_str(&std::env::var("RHEA_PLAN").unwrap()).unwrap();
        assert!(
            steps.to_string().contains("\"send\"") == false
                && steps.to_string().contains("\"message\"") == false
                && steps.to_string().contains("\"type\"") == false,
            "live probe is navigation-only"
        );
        let result = crate::jev::run_plan(&json!({"steps":steps,"done_message":"probe"})).unwrap();
        println!(
            "{}",
            serde_json::to_string_pretty(&result)
                .unwrap()
                .chars()
                .take(1500)
                .collect::<String>()
        );
    }

    #[test]
    fn plan_stops_at_first_invalid_step_without_running_later_ones() {
        let plan = json!({"steps":[
            {"do":"press","app":null,"target":null,"recipient":null,"text":null,"keys":"return"},
            {"do":"send","app":"Discord","target":null,"recipient":null,"text":"never typed","keys":null}
        ],"done_message":"x"});
        let result = crate::jev::run_plan(&plan).unwrap();
        assert_eq!(result["status"], "stopped");
        assert_eq!(result["failed_step"]["step"], 1);
        assert_eq!(result["message_sent"], false);
        assert!(result["completed_steps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn desktop_send_activity_summary_reports_non_submission_and_never_exposes_text() {
        let result = json!({"ok":true,"result":{
            "status":"not_submitted",
            "type_failure":{"kind":"stale_or_not_performed"},
            "private_text":"do not show this"
        }});
        let summary = summarize_tool_result("desktop_send", &result);
        assert_eq!(
            summary,
            "Message not submitted · stale_or_not_performed; no submit key was sent."
        );
        assert!(!summary.contains("do not show this"));
        assert_eq!(
            summarize_tool_result(
                "desktop_send",
                &json!({"ok":true,"result":{"status":"submit_failed_or_uncertain"}})
            ),
            "Submission attempted once · outcome uncertain; do not retry."
        );
    }

    #[test]
    fn completed_output_text_handles_multiple_content_parts() {
        let output = vec![json!({"type":"message","content":[
            {"type":"output_text","text":"Hello"},
            {"type":"refusal","refusal":" world"}
        ]})];
        assert_eq!(completed_text(&output), "Hello world");
    }

    #[test]
    fn completed_output_text_is_empty_for_tool_only_completion() {
        let output = vec![json!({"type":"function_call","name":"desktop_snapshot"})];
        assert!(completed_text(&output).trim().is_empty());
    }

    #[test]
    fn incomplete_response_error_includes_reason_when_message_is_missing() {
        let event = json!({"type":"response.incomplete","response":{
            "status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}
        }});
        let error = stream_error(&event);
        assert!(error.contains("response.incomplete"));
        assert!(error.contains("reason=max_output_tokens"));
    }

    #[test]
    fn title_model_uses_nano_before_mini_and_lowest_advertised_effort() {
        let models = vec![
            Model {
                slug: "gpt-mini".into(),
                display_name: "Mini".into(),
                provider: Provider::ChatGpt,
                priority: Some(1),
                supported_reasoning_levels: vec!["low".into(), "high".into()],
                context_window: None,
                max_context_window: None,
            },
            Model {
                slug: "gpt-nano".into(),
                display_name: "Nano".into(),
                provider: Provider::ChatGpt,
                priority: Some(3),
                supported_reasoning_levels: vec!["medium".into(), "minimal".into()],
                context_window: None,
                max_context_window: None,
            },
        ];
        assert_eq!(
            title_model(&models),
            Some(TitleModel {
                slug: "gpt-nano".into(),
                reasoning_effort: "minimal".into()
            })
        );
    }

    #[test]
    fn title_model_returns_none_without_explicit_small_model_or_effort_metadata() {
        let no_small_model = vec![Model {
            slug: "gpt-main".into(),
            display_name: "Main".into(),
            provider: Provider::ChatGpt,
            priority: None,
            supported_reasoning_levels: vec!["low".into()],
            context_window: None,
                max_context_window: None,
        }];
        let no_effort_metadata = vec![Model {
            slug: "gpt-mini".into(),
            display_name: "Mini".into(),
            provider: Provider::ChatGpt,
            priority: None,
            supported_reasoning_levels: vec![],
            context_window: None,
                max_context_window: None,
        }];
        assert!(title_model(&no_small_model).is_none());
        assert!(title_model(&no_effort_metadata).is_none());
    }

    #[test]
    fn title_response_parser_accepts_plain_and_json_title_forms() {
        assert_eq!(
            parse_title_response("  Title:  \"Organize project notes\"  ", "fallback request"),
            "Organize project notes"
        );
        assert_eq!(
            parse_title_response(r#"{"title":"Review budget options"}"#, "fallback request"),
            "Review budget options"
        );
        assert_eq!(
            parse_title_response(r#""Plan weekend meals""#, "fallback request"),
            "Plan weekend meals"
        );
        assert_eq!(
            parse_title_response(
                "```json\n{\"title\":\"Read planning notes\"}\n```",
                "fallback"
            ),
            "Read planning notes"
        );
    }

    #[test]
    fn completed_title_parser_reads_responses_api_output_text_and_output_items() {
        let direct = json!({"output_text":"Plan the launch"});
        assert_eq!(
            parse_completed_title_response(&direct, "ignored delta", "request"),
            "Plan the launch"
        );
        let items = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"Sort tax documents"}]}]});
        assert_eq!(
            parse_completed_title_response(&items, "ignored delta", "request"),
            "Sort tax documents"
        );
        let delta_only = json!({"output":[]});
        assert_eq!(
            parse_completed_title_response(&delta_only, "  Prepare travel list  ", "request"),
            "Prepare travel list"
        );
    }

    #[test]
    fn title_response_parser_uses_fallback_for_empty_or_unusable_json() {
        assert_eq!(
            parse_title_response("{\"other\":\"value\"}", "  Find   a quiet cafe. "),
            "Find a quiet cafe"
        );
        assert_eq!(parse_title_response("   ", "\n\t"), "New chat");
    }

    #[test]
    fn old_tool_outputs_are_trimmed_but_recent_ones_and_messages_are_kept() {
        let big = "y".repeat(5_000);
        let mut input = vec![
            json!({"role":"user","content":big.clone()}),
            json!({"type":"function_call_output","call_id":"1","output":big.clone()}),
            json!({"type":"function_call_output","call_id":"2","output":"small"}),
            json!({"type":"function_call_output","call_id":"3","output":big.clone()}),
        ];
        assert_eq!(trim_old_tool_outputs(&mut input, 1), 1);
        assert!(input[1]["output"].as_str().unwrap().len() < 800);
        assert!(input[1]["output"].as_str().unwrap().contains("trimmed"));
        assert_eq!(input[0]["content"].as_str().unwrap().len(), 5_000, "user messages are never trimmed");
        assert_eq!(input[2]["output"], "small");
        assert_eq!(input[3]["output"].as_str().unwrap().len(), 5_000, "the newest output stays intact");
    }

    #[test]
    fn only_short_direct_status_questions_count_as_connection_inventory_requests() {
        assert!(is_connection_inventory_question("Is Chrome connected?"));
        assert!(is_connection_inventory_question("what MCP servers are connected"));
        assert!(is_connection_inventory_question("list my mcps"));
        // A worker brief or coding request that merely mentions servers/running/tools is not one.
        let brief = "Read-only inspect C:\\Projects\\FastLab (do not edit) and compare its architecture to the requested game: web visual UI, 100x headless training with live counters, Laya local HTTP model integration. Report exact file map, what is already implemented, commands to run, and technical gaps.";
        assert!(!is_connection_inventory_question(brief));
        assert!(!is_connection_inventory_question("start the dev server and tell me if it is running"));
        assert!(!is_connection_inventory_question("fix the browser tab bug in app.rs"));
    }

    #[test]
    fn tool_activity_is_never_dropped_and_only_old_details_are_cleared() {
        let mut list = Vec::new();
        for i in 0..150 {
            record_activity(&mut list, ToolActivity { id: format!("c{i}"), result_detail: "x".repeat(50), state: "complete".into(), ..Default::default() });
        }
        assert_eq!(list.len(), 150);
        assert!(list[0].result_detail.is_empty(), "old details are cleared");
        assert!(!list[149].result_detail.is_empty(), "recent details stay");
        record_activity(&mut list, ToolActivity { id: "c149".into(), state: "failed".into(), ..Default::default() });
        assert_eq!((list.len(), list[149].state.as_str()), (150, "failed"), "an update replaces, not appends");
    }

    #[test]
    fn shell_and_agent_results_reach_the_model_whole_but_desktop_results_stay_trimmed() {
        let long = "z".repeat(5_000);
        let shell = json!({"ok":true,"result":{"content":long.clone()}});
        assert_eq!(model_output("mcp__rhea_shell__read_file", &shell), shell);
        assert_eq!(model_output("agent_read", &json!({"result":long.clone()}))["result"].as_str().unwrap().len(), 5_000);
        let desktop = model_output("desktop_find", &json!({"text":long.clone()}));
        assert!(desktop["text"].as_str().unwrap().chars().count() <= 241);
        let huge = json!({"stdout":"q".repeat(100_000)});
        let capped = model_output("mcp__rhea_shell__run_command", &huge);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["partial"].as_str().unwrap().chars().count(), 60_000);
    }

    #[test]
    fn past_turns_carry_a_digest_of_their_tool_calls_to_the_next_request() {
        let turn = Turn {
            role: "assistant".into(),
            content: "Done.".into(),
            tool_activity: vec![ToolActivity {
                id: "1".into(),
                name: "mcp__rhea_shell__run_command".into(),
                arguments_detail: "{\"command\":\"dir\"}".into(),
                result_detail: "{\"stdout\": \"a.txt   b.txt\"}".into(),
                state: "complete".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = turn_model_text(&turn);
        assert!(text.starts_with("Done."));
        assert!(text.contains("run_command") && text.contains("a.txt b.txt"), "{text}");
        let plain = Turn { role: "user".into(), content: "hi".into(), ..Default::default() };
        assert_eq!(turn_model_text(&plain), "hi");
        assert!(digest_size_estimate(&turn) > 0 && digest_size_estimate(&plain) == 0);
        let input = conversation_input("", std::slice::from_ref(&turn), "next");
        assert!(input.iter().any(|item| item["content"].as_str().is_some_and(|c| c.contains("a.txt b.txt"))));
        // a long run never grows without bound
        let mut big = turn.clone();
        big.tool_activity = (0..500).map(|i| ToolActivity { id: i.to_string(), name: "t".into(), result_detail: "r".repeat(5_000), state: "complete".into(), ..Default::default() }).collect();
        assert!(turn_model_text(&big).len() < 18_000 + 400);
        assert!(turn_model_text(&big).contains("earlier calls omitted"));
    }

    fn call(id: &str, name: &str) -> Value {
        json!({"type":"function_call","id":format!("fc_{id}"),"call_id":id,"name":name,"arguments":"{}"})
    }

    fn output(id: &str, text: &str) -> Value {
        json!({"type":"function_call_output","call_id":id,"output":text})
    }

    #[test]
    fn a_runs_transcript_is_kept_without_developer_notes_or_reasoning_and_with_valid_pairs() {
        let input = vec![
            json!({"role":"developer","content":"system"}),
            json!({"role":"user","content":"first"}),
            json!({"type":"reasoning","id":"rs_1","summary":[]}),
            call("a", "get_tools_"),
            output("a", "loaded"),
            json!({"role":"developer","content":"hook note"}),
            call("orphan", "run_command"),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}),
            json!({"role":"user","content":"[message from Lines] teammate"}),
            json!({"role":"user","content":"second"}),
            output("ghost", "result without a call"),
            output("b", &"x".repeat(30_000)),
            call("b", "read_file"),
        ];
        // positions of the real user messages in `input`: 1 and 9
        let (items, starts) = persistable_items(&input, &[1, 9]);
        let kinds: Vec<String> = items.iter().map(|i| i.get("type").and_then(Value::as_str).unwrap_or("user/assistant").to_owned()).collect();
        assert_eq!(kinds, ["user/assistant", "function_call", "function_call_output", "message", "user/assistant", "user/assistant", "function_call_output", "function_call"]);
        assert_eq!(starts, [0, 5], "real user messages keep their place; the teammate message is not a turn start");
        let capped = items.iter().find(|i| i["call_id"] == "b" && i["type"] == "function_call_output").unwrap();
        assert!(capped["output"].as_str().unwrap().chars().count() < 24_100 && capped["output"].as_str().unwrap().contains("capped"));
        assert!(items.iter().all(|i| i["call_id"] != "orphan" && i["call_id"] != "ghost"));
    }

    #[test]
    fn a_chat_continues_from_its_saved_transcript_only_while_it_matches_the_visible_turns() {
        let turn = |role: &str, text: &str| Turn { role: role.into(), content: text.into(), ..Default::default() };
        let history = vec![turn("user", "hi"), turn("assistant", "hello")];
        let state = crate::chat_store::ModelState {
            items: vec![json!({"role":"user","content":"hi"}), call("a", "get_tools_"), output("a", "ok"), json!({"role":"assistant","content":"hello"})],
            loaded_tools: vec![],
            turn_starts: vec![0],
            turns: 2,
        };
        let (items, starts) = resume_items(&state, &history);
        assert_eq!((items.len(), starts.as_slice()), (4, &[0usize][..]), "the saved transcript, tool calls included, is used");
        // an edit or regenerate changes the number of visible turns: rebuild from the turns instead
        let (items, starts) = resume_items(&state, &history[..1]);
        assert_eq!((items.len(), starts.as_slice()), (1, &[0usize][..]));
        let (items, _) = resume_items(&crate::chat_store::ModelState::default(), &history);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn compaction_summarizes_everything_before_the_last_two_turns() {
        assert_eq!(compaction_cut(&[0, 5, 9]), None, "too short to be worth it");
        assert_eq!(compaction_cut(&[0, 5, 9, 14]), Some((9, 4)));
        assert_eq!(compaction_cut(&[0, 5, 9, 14, 20, 30]), Some((20, 8)));
        let turns = items_to_turns(&[json!({"role":"user","content":"do it"}), call("a", "run_command"), output("a", "42"), json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}), json!({"type":"reasoning"})]);
        assert_eq!(turns.len(), 4);
        assert!(turns[1].content.contains("run_command") && turns[2].content.contains("42") && turns[3].content == "done");
    }

    /// Live check (needs a signed-in session): a second run must continue from the first run's tool
    /// calls and results, replayed to the Codex backend with `store:false`. Run with:
    /// cargo test --bin rhea live_second_run_remembers_the_first_runs_tool_calls -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_second_run_remembers_the_first_runs_tool_calls() {
        use std::sync::{Arc, Mutex};
        let tokens = crate::auth::token().expect("sign in first");
        let models = models(&tokens.access_token, tokens.account_id.as_deref()).unwrap();
        let model = models.first().unwrap().slug.clone();
        let saved: Arc<Mutex<Option<crate::chat_store::ModelState>>> = Arc::new(Mutex::new(None));
        let run = |history: &[Turn], user: &str, state: Option<crate::chat_store::ModelState>| -> (String, Vec<String>) {
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = Arc::clone(&calls);
            let sink = Arc::clone(&saved);
            let options = RunOptions {
                session_id: Some("rhea-live-probe".into()),
                state,
                on_state: Some(Box::new(move |state| *sink.lock().unwrap() = Some(state))),
                ..Default::default()
            };
            let answer = respond_with_options(
                &tokens.access_token,
                tokens.account_id.as_deref(),
                &model,
                "low",
                200_000,
                "",
                history,
                user,
                &[],
                move |name, _args| {
                    seen.lock().unwrap().push(name.to_owned());
                    Ok(json!({"ok": true, "result": {"answer_hint": "Configured MCP servers: none. Marker ALPHA-4471."}}))
                },
                options,
                |_| {},
            )
            .expect("run failed");
            let names = calls.lock().unwrap().clone();
            (answer, names)
        };
        let (first, first_calls) = run(&[], "Call the list_mcps tool exactly once, then reply with only the word DONE.", None);
        println!("run 1: {first:?} calls={first_calls:?}");
        assert!(first_calls.iter().any(|name| name == "list_mcps"));
        let state = saved.lock().unwrap().clone().expect("state saved");
        assert!(state.items.iter().any(|item| item["type"] == "function_call"), "tool call kept");
        assert_eq!((state.turns, state.turn_starts.len()), (2, 1));
        let history = vec![
            Turn { role: "user".into(), content: "Call the list_mcps tool exactly once, then reply with only the word DONE.".into(), ..Default::default() },
            Turn { role: "assistant".into(), content: first, ..Default::default() },
        ];
        let (second, second_calls) = run(&history, "Without calling any tool: which tool did you call in your previous reply, and what marker text did it return? Answer in one short line.", Some(state));
        println!("run 2: {second:?} calls={second_calls:?}");
        assert!(second_calls.is_empty(), "no tool needed to answer: {second_calls:?}");
        assert!(second.contains("list_mcps"), "{second}");
        assert!(second.contains("ALPHA-4471"), "the previous tool result must be in context: {second}");
        let after = saved.lock().unwrap().clone().unwrap();
        assert_eq!((after.turns, after.turn_starts.len()), (4, 2));
    }

    /// Live check (needs a signed-in session): prints every model's size-related catalog fields.
    /// cargo test --bin rhea live_model_catalog_sizes -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_model_catalog_sizes() {
        let tokens = crate::auth::token().expect("sign in first");
        let raw = reqwest::blocking::Client::new()
            .get(format!("https://chatgpt.com/backend-api/codex/models?client_version={CODEX_CLIENT_VERSION}"))
            .bearer_auth(&tokens.access_token)
            .header("originator", "codex_cli_rs")
            .header("ChatGPT-Account-ID", tokens.account_id.clone().unwrap_or_default())
            .send()
            .unwrap()
            .json::<Value>()
            .unwrap();
        for model in raw["models"].as_array().unwrap() {
            let numbers: Vec<String> = model.as_object().unwrap().iter().filter(|(_, v)| v.is_number()).map(|(k, v)| format!("{k}={v}")).collect();
            println!("{} [{}] {}", model["slug"], model["visibility"], numbers.join(" "));
        }
        let parsed = models(&tokens.access_token, tokens.account_id.as_deref()).unwrap();
        for model in parsed {
            println!("parsed {} -> {:?}", model.slug, model.context_window);
        }
    }

    #[test]
    fn a_model_uses_the_accounts_maximum_then_the_catalog_size_then_the_published_one() {
        let model = |context: Option<u64>, max: Option<u64>| Model {
            slug: "gpt-6-astra".into(),
            display_name: "x".into(),
            provider: Provider::ChatGpt,
            priority: None,
            supported_reasoning_levels: Vec::new(),
            context_window: context,
            max_context_window: max,
        };
        assert_eq!(model(Some(272_000), Some(872_000)).context_limit(), Some(872_000));
        assert_eq!(model(Some(272_000), None).context_limit(), Some(272_000));
        assert_eq!(model(None, None).context_limit(), Some(1_050_000), "the published size when the catalog has none");
        assert_eq!(model(None, None).max_output(), Some(128_000));
    }

    #[test]
    fn published_limits_match_the_openai_model_pages() {
        let limits = |slug: &str| documented_limits(slug).map(|l| (l.context, l.max_output));
        for slug in ["gpt-6-astra", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna", "gpt-6-sol", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.5-pro", "gpt-5.4", "gpt-5.4-pro"] {
            assert_eq!(limits(slug), Some((1_050_000, 128_000)), "{slug}");
        }
        for slug in ["gpt-5.4-mini", "gpt-5.4-nano", "gpt-5.3-codex", "gpt-5.2", "gpt-5.2-pro", "gpt-5.1", "gpt-5", "gpt-5-mini", "gpt-5-nano"] {
            assert_eq!(limits(slug), Some((400_000, 128_000)), "{slug}");
        }
        assert_eq!(limits("gpt-6-cyber"), None, "listed without figures");
        assert_eq!(limits("something-else"), None);
    }

    #[test]
    fn compact_percent_is_clamped_to_the_documented_range() {
        assert_eq!(clamp_compact_percent(0), 15);
        assert_eq!(clamp_compact_percent(80), 80);
        assert_eq!(clamp_compact_percent(200), 95);
    }

    #[test]
    fn title_response_and_local_fallback_are_bounded_and_deterministic() {
        let long = "word ".repeat(30);
        let title = parse_title_response(&long, "unused");
        assert!(title.chars().count() <= TITLE_CHAR_LIMIT + 1);
        assert!(title.ends_with('…'));
        assert_eq!(fallback_title(&long), fallback_title(&long));
    }

    #[test]
    fn title_request_body_has_one_short_user_input_and_no_tools() {
        let model = TitleModel {
            slug: "gpt-nano".into(),
            reasoning_effort: "minimal".into(),
        };
        let body = title_request_body(&model, "Summarize my first request");
        assert_eq!(body.get("model").and_then(Value::as_str), Some("gpt-nano"));
        assert_eq!(
            body.pointer("/reasoning/effort").and_then(Value::as_str),
            Some("minimal")
        );
        assert_eq!(
            body.pointer("/input")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            body.get("tools").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
        assert_eq!(
            body.get("tool_choice").and_then(Value::as_str),
            Some("none")
        );
    }

    #[test]
    fn function_tool_constructor_validates_and_bounds_dynamic_schema() {
        let schema = json!({"type":"object","properties":{},"required":[]});
        let tool = function_tool(
            "mcp__calendar__list_events",
            "List calendar events",
            &schema,
        )
        .unwrap();
        assert_eq!(
            tool.get("name").and_then(Value::as_str),
            Some("mcp__calendar__list_events")
        );
        for name in [
            "browser__snapshot",
            "browser__click",
            "browser__type",
            "browser__navigate",
        ] {
            assert_eq!(
                function_tool(name, "Browser action", &schema)
                    .unwrap()
                    .get("name")
                    .and_then(Value::as_str),
                Some(name)
            );
        }
        assert!(function_tool("unsafe name", "Description", &schema).is_err());
        assert!(function_tool("browser__unsafe name", "Description", &schema).is_err());
        assert!(function_tool("valid_name", &"x".repeat(1025), &schema).is_err());
        assert!(function_tool("valid_name", "Description", &json!([])).is_err());
    }

    #[test]
    fn merged_dynamic_tools_reject_name_collisions() {
        let schema = json!({"type":"object","properties":{},"required":[]});
        let distinct = function_tool("mcp__files__read", "Read a local file", &schema).unwrap();
        assert!(merged_tools(&[distinct.clone()]).is_ok());
        assert!(
            merged_tools(&[distinct.clone(), distinct])
                .unwrap_err()
                .contains("conflicts")
        );
    }

    #[test]
    fn browser_actions_remain_pending_until_a_browser_snapshot_call() {
        let wrapped_action_result = json!({"ok":true,"result":{"snapshot":{"kind":"desktop_snapshot","snapshot_id":"new","complete":true,"local_snapshot_retained":true,"ref_count":4}}});
        for name in ["browser__click", "browser__type", "browser__navigate"] {
            assert!(is_state_changing_tool(name));
            assert!(!can_complete_with_inline_snapshot(
                name,
                &wrapped_action_result
            ));
            assert!(state_change_result(name).contains("later browser snapshot"));
        }
        assert!(!is_state_changing_tool("browser__snapshot"));
        assert!(action_has_fresh_snapshot(&wrapped_action_result));
        assert!(
            summarize_snapshot_result(&wrapped_action_result).contains("4 actionable controls")
        );
    }

    #[test]
    fn non_browser_actions_can_complete_from_an_inline_fresh_snapshot() {
        let result = json!({"ok":true,"result":{"fresh_observation":{"kind":"desktop_snapshot","snapshot_id":"new","complete":true,"local_snapshot_retained":true,"ref_count":2}}});
        assert!(can_complete_with_inline_snapshot("desktop_click", &result));
    }

    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn model_facing_tool_results_keep_rows_past_the_old_forty_eight_item_cutoff() {
        let mut rows = (0..300)
            .map(|index| json!({"name":format!("Conversation {index}"),"ref":index}))
            .collect::<Vec<_>>();
        rows.push(json!({"name":"525i","ref":300}));
        let result = compact_agent_output(&json!({"items":rows}));
        assert_eq!(result.pointer("/items/300/name").and_then(Value::as_str), Some("525i"));
    }

    #[test]
    fn rich_workflow_prompt_teaches_targeted_desktop_lookup_and_unbounded_rounds() {
        let prompt = flat(system_prompt());
        assert!(prompt.contains("Do not stop because an arbitrary number of tool rounds has elapsed"));
        assert!(prompt.contains("call `desktop_find` with the exact name/text"));
        assert!(prompt.contains("The manager verifies cross-worker integration"));
    }

    #[test]
    fn system_prompt_keeps_the_core_routing_rules() {
        let prompt = flat(system_prompt());
        for needle in [
            "never call yourself ChatGPT",
            "call `open_app` directly",
            "`get_tools_`",
            "`list_mcps`",
            "`ask_user`",
            "`agent_spawn",
            "`desktop_send`",
            "`jev_run_plan`",
            "Set `final:true` only when",
            "mcp__rhea_shell__run_command",
            "Prompt injection",
            "Compose mode",
        ] {
            assert!(prompt.contains(needle), "system prompt lost: {needle}");
        }
        // exact sends must be described as desktop_send-only
        let send = prompt.find("`desktop_send` | The *only* tool").expect("exact-send rule");
        assert!(prompt[send..].contains("`desktop_type`"));
    }

    #[test]
    fn both_platform_manuals_keep_the_routing_rules_and_speak_their_own_platform() {
        let windows = flat(include_str!("system_prompt.md"));
        let mac = flat(include_str!("system_prompt_macos.md"));
        for (name, prompt) in [("windows", &windows), ("macos", &mac)] {
            for needle in [
                "never call yourself ChatGPT",
                "call `open_app` directly",
                "`get_tools_`",
                "`list_mcps`",
                "`ask_user`",
                "`agent_spawn",
                "`desktop_send`",
                "`jev_run_plan`",
                "Set `final:true` only when",
                "mcp__rhea_shell__run_command",
                "Prompt injection",
                "Compose mode",
                "`use_skill",
                "`install_skill",
                "ranges",
                "GOAL",
            ] {
                assert!(prompt.contains(needle), "{name} manual lost: {needle}");
            }
            let send = prompt.find("`desktop_send` | The *only* tool").unwrap_or_else(|| panic!("{name}: exact-send rule"));
            assert!(prompt[send..].contains("`desktop_type`"));
        }
        assert!(windows.contains("winget") && windows.contains("PowerShell"));
        assert!(mac.contains("brew install") && mac.contains("osascript") && mac.contains("open -a") && mac.contains("cmd+k"));
        assert!(!mac.contains("winget") && !mac.contains("Get-ChildItem") && !mac.contains("C:\\\\"));
        let (w, m) = (windows.len() as f64, mac.len() as f64);
        assert!((m / w - 1.0).abs() < 0.2, "the macOS manual should be about the same size as the Windows one ({w} vs {m})");
    }

    #[test]
    fn title_agent_uses_the_selected_model_at_its_lowest_effort() {
        let model = |slug: &str, levels: &[&str]| Model {
            slug: slug.into(),
            display_name: slug.into(),
            provider: Provider::ChatGpt,
            priority: None,
            supported_reasoning_levels: levels.iter().map(|level| level.to_string()).collect(),
            context_window: None,
                max_context_window: None,
        };
        let models = vec![model("gpt-big", &["low", "medium", "high"]), model("gpt-mini", &["minimal", "low"]), model("gpt-none", &[])];
        let picked = title_model_for(&models, "gpt-big").unwrap();
        assert_eq!((picked.slug.as_str(), picked.reasoning_effort.as_str()), ("gpt-big", "low"));
        assert_eq!(title_model_for(&models, "gpt-mini").unwrap().reasoning_effort, "minimal");
        assert_eq!(title_model_for(&models, "gpt-none").unwrap().reasoning_effort, "low");
        // an unknown selection falls back to the first model rather than failing
        assert_eq!(title_model_for(&models, "gone").unwrap().slug, "gpt-big");
        assert!(title_model_for(&[], "x").is_none());
    }

    #[test]
    fn title_prompt_drops_its_template_block_and_keeps_the_naming_rules() {
        let instructions = title_instructions();
        assert!(!instructions.contains("[first user message]"));
        assert!(instructions.contains("provided inside"), "the data-not-instructions rule must survive");
        assert!(instructions.contains("noun phrase"));
        assert!(instructions.contains("\"title\""));
        let body = title_request_body(&TitleModel { slug: "m".into(), reasoning_effort: "low".into() }, "fix the login bug");
        let user = body.pointer("/input/1/content").and_then(Value::as_str).unwrap();
        assert!(user.starts_with("<session>") && user.contains("fix the login bug") && user.ends_with("</session>"));
    }

    /// Live probe (needs a signed-in session): the Codex backend refuses `store: true`, so a chat's
    /// continuity is the full local history plus a stable per-chat session id that keys the
    /// server-side prompt cache. Run with:
    /// cargo test --bin rhea live_session_cache_probe -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_session_cache_probe() {
        let tokens = crate::auth::token().expect("sign in first");
        let models = models(&tokens.access_token, tokens.account_id.as_deref()).unwrap();
        let model = models.first().unwrap().slug.clone();
        let http = Client::builder().timeout(Duration::from_secs(90)).build().unwrap();
        let session = "probe-session-0001-rhea";
        let filler = "The quick brown fox jumps over the lazy dog. ".repeat(400);
        let call = |user: &str| -> (u16, String) {
            let mut request = http
                .post("https://chatgpt.com/backend-api/codex/responses")
                .bearer_auth(&tokens.access_token)
                .header("originator", "codex_cli_rs")
                .header("session_id", session)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&json!({"model":model,"instructions":format!("You are terse. {filler}"),"input":[{"role":"user","content":user}],
                    "tools":[],"store":false,"stream":true,"prompt_cache_key":session}));
            if let Some(id) = tokens.account_id.as_deref() {
                request = request.header("ChatGPT-Account-ID", id);
            }
            let response = request.send().unwrap();
            let status = response.status().as_u16();
            (status, response.text().unwrap_or_default())
        };
        for user in ["Say OK.", "Say OK again."] {
            let (status, body) = call(user);
            let usage = body.lines().filter_map(|line| line.strip_prefix("data:")).filter_map(|payload| serde_json::from_str::<Value>(payload.trim()).ok())
                .find_map(|event| event.pointer("/response/usage").cloned());
            println!("CALL status={status} usage={usage:?}");
            if status != 200 {
                println!("{}", body.chars().take(400).collect::<String>());
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn shell_plan_steps_run_commands_and_stop_on_failure() {
        let temp = std::env::temp_dir().to_string_lossy().into_owned();
        let ok = crate::jev::run_plan(&json!({"steps":[
            {"do":"shell","text":"Write-Output plan-step-ok","target":temp,"ms":20000},
            {"do":"wait","ms":10}
        ],"done_message":"","final":false}))
        .unwrap();
        assert_eq!(ok["status"], "completed", "{ok}");
        assert!(ok["completed_steps"][0]["detail"]["stdout"].as_str().unwrap().contains("plan-step-ok"), "{ok}");

        let failed = crate::jev::run_plan(&json!({"steps":[
            {"do":"shell","text":"[Console]::Error.WriteLine('boom'); exit 3","target":temp,"ms":20000},
            {"do":"wait","ms":10}
        ],"done_message":"","final":false}))
        .unwrap();
        assert_eq!(failed["status"], "stopped");
        assert_eq!(failed["failed_step"]["do"], "shell");
        assert!(failed["failed_step"]["reason"].as_str().unwrap().contains("boom"), "{failed}");
        assert_eq!(failed["completed_steps"].as_array().map(Vec::len), Some(0));

        let missing = crate::jev::run_plan(&json!({"steps":[{"do":"shell","text":null,"target":null}],"done_message":"","final":false})).unwrap();
        assert_eq!(missing["status"], "stopped");
    }

    #[test]
    fn research_gate_ignores_local_work_but_catches_web_research() {
        assert!(!requires_research_evidence("count the .rs files in the src folder of the current project"));
        assert!(!requires_research_evidence("search the codebase for the function that sends messages"));
        assert!(!requires_research_evidence("use two subagents to summarize the latest changes in this repo"));
        assert!(requires_research_evidence("look on roblox for games with more than 1K active players"));
        assert!(requires_research_evidence("research the latest news about rust on the web"));
        assert!(requires_research_evidence("search for the best restaurants"));
        assert!(!requires_research_evidence("open discord"));
    }

    #[test]
    fn compose_mode_filters_tools_and_hides_propose_plan_otherwise() {
        let tools = json!([
            {"name":"propose_plan"}, {"name":"ask_user"}, {"name":"desktop_click"},
            {"name":"mcp__rhea_shell__read_file"}, {"name":"mcp__rhea_shell__write_file"}, {"name":"mcp__rhea_shell__run_command"}
        ]);
        let names = |value: Value| value.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        let composing = names(request_tools(&tools, true, &[]));
        assert!(composing.contains(&"propose_plan".to_owned()));
        assert!(composing.contains(&"mcp__rhea_shell__read_file".to_owned()));
        assert!(!composing.contains(&"desktop_click".to_owned()));
        assert!(!composing.contains(&"mcp__rhea_shell__write_file".to_owned()));
        let normal = names(request_tools(&tools, false, &[]));
        assert!(!normal.contains(&"propose_plan".to_owned()));
        assert!(normal.contains(&"desktop_click".to_owned()));
        let hidden = names(request_tools(&tools, false, &["ask_user".to_owned()]));
        assert!(!hidden.contains(&"ask_user".to_owned()));
    }

    #[test]
    fn compose_and_subagent_prompts_describe_their_protocols() {
        let compose = flat(COMPOSE_PROMPT);
        for needle in ["propose_plan", "ask_user", "read-only"] {
            assert!(compose.contains(needle), "compose prompt lost: {needle}");
        }
        let worker = flat(include_str!("subagent_prompt.md"));
        for needle in ["{name}", "agent_send", "main"] {
            assert!(worker.contains(needle), "subagent prompt lost: {needle}");
        }
    }

    #[test]
    fn exact_send_schema_routes_to_composite_and_tool_loop_blocks_redundant_followups() {
        let tools = local_tools();
        let tools = tools.as_array().unwrap();
        let description = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|tool| tool.get("description"))
                .and_then(Value::as_str)
                .unwrap()
        };
        assert!(description("desktop_send").contains("Sole send path"));
        assert!(
            description("desktop_type")
                .contains("only when the user asked for text entry without submission")
        );
        assert!(description("desktop_get").contains("Do not read composer contents"));
        for name in [
            "desktop_send",
            "desktop_type",
            "desktop_get",
            "desktop_press",
            "desktop_click",
        ] {
            assert!(!should_block_desktop_send_followup(name, false));
            assert!(
                should_block_desktop_send_followup(name, true),
                "{name} must be blocked after desktop_send"
            );
        }
        for name in ["desktop_snapshot", "desktop_find", "desktop_list_windows"] {
            assert!(
                !should_block_desktop_send_followup(name, true),
                "read-only clarification/verification remains available: {name}"
            );
        }
    }

    #[test]
    fn explicit_send_requests_advertise_composite_input_only_and_keep_navigation() {
        let user = "Open Discord and send my goddess a message saying I love you";
        assert!(has_explicit_send_intent(user));
        let tools = tools_for_request(&[], user).unwrap();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"desktop_send"));
        assert!(
            names.contains(&"desktop_click"),
            "semantic navigation to the recipient remains available"
        );
        assert!(names.contains(&"desktop_snapshot"));
        for blocked in ["desktop_type", "desktop_press", "desktop_mouse_click"] {
            assert!(
                !names.contains(&blocked),
                "manual text/submit input must be unavailable during explicit send: {blocked}"
            );
        }
        assert!(should_block_low_level_send_input("desktop_type", true));
        assert!(should_block_low_level_send_input("desktop_press", true));
        assert!(!should_block_low_level_send_input("desktop_click", true));
        assert!(!should_block_low_level_send_input("desktop_type", false));
    }

    #[test]
    fn explicit_send_router_ignores_negated_or_quoted_send_language() {
        assert!(!has_explicit_send_intent("Don't send anything to Alex"));
        assert!(!has_explicit_send_intent("Do not send the draft"));
        assert!(!has_explicit_send_intent(
            "Type 'send a message' into Notepad without submitting"
        ));
        assert!(!has_explicit_send_intent(
            "How do I send a message from this app?"
        ));
        assert!(has_explicit_send_intent("Send 'don't send' to Alex"));
        assert!(!has_explicit_send_intent(
            "Open Discord and inspect the message composer"
        ));
    }

    #[test]
    fn failed_or_incomplete_desktop_observations_do_not_mark_an_action_refreshed() {
        let failed = json!({"ok":true,"result":{"fresh_observation":{"kind":"desktop_snapshot","ok":false,"error":"capture failed","previous_snapshot_id":"old"}}});
        let incomplete = json!({"ok":true,"result":{"fresh_observation":{"kind":"desktop_snapshot","snapshot_id":"partial","complete":false,"local_snapshot_retained":true}}});
        let unretained = json!({"ok":true,"result":{"fresh_observation":{"kind":"desktop_snapshot","snapshot_id":"gone","complete":true,"local_snapshot_retained":false}}});
        assert!(!action_has_fresh_snapshot(&failed));
        assert!(!action_has_fresh_snapshot(&incomplete));
        assert!(!action_has_fresh_snapshot(&unretained));
    }
}

#[test]
fn browser_bridge_tools_are_forwarded_to_the_model_on_chat_turns() {
    let schema = json!({"type":"object","properties":{},"additionalProperties":false});
    let browser_tools = [
        function_tool("browser__tabs", "List Chrome tabs", &schema).unwrap(),
        function_tool("browser__snapshot", "Inspect the active page", &schema).unwrap(),
    ];
    let tools = tools_for_request(&browser_tools, "What is open in Chrome?").unwrap();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"browser__tabs"));
    assert!(names.contains(&"browser__snapshot"));
}
