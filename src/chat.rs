use std::{
    io::{BufRead, BufReader},
    time::{Duration, Instant},
};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ChatGPT subscription access uses the Codex account catalog and response route.
const CODEX_CLIENT_VERSION: &str = "0.166.0";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    /// Catalog ordering hint from the Codex model API, not a price.
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
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
            format!("JARVUS/{}", env!("CARGO_PKG_VERSION")),
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
            })
        })
        .collect();
    if models.is_empty() {
        return Err(format!(
            "ChatGPT returned {catalog_count} catalog entries, but none are marked available to the Codex API. JARVUS requests the Codex {CODEX_CLIENT_VERSION} catalog."
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

const TITLE_PROMPT: &str =
    "Create a short title for this request (2-6 words). Return only the title.";
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
            format!("JARVUS/{}", env!("CARGO_PKG_VERSION")),
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
        "input":[{"role":"user","content":format!("{TITLE_PROMPT}\n\n{request_excerpt}")}],
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

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cache_reported: bool,
    pub cache_write_reported: bool,
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
fn read_sse_lines_with_cancel(
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

fn send_chat_request_with_cancel(
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

pub const DEFAULT_CONTEXT_WINDOW: u64 = 264_000;
const SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

pub fn respond(
    access_token: &str,
    account_id: Option<&str>,
    model: &str,
    context_window: u64,
    prior_summary: &str,
    history: &[Turn],
    user_message: &str,
    send: impl Fn(StreamEvent),
) -> Result<String, String> {
    respond_with_tools(
        access_token,
        account_id,
        model,
        "medium",
        context_window,
        prior_summary,
        history,
        user_message,
        &[],
        crate::jev::execute,
        send,
    )
}

pub fn respond_with_tools(
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
    send: impl Fn(StreamEvent),
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
    let mut request_history = history.to_vec();
    let mut current_summary = prior_summary.trim().to_owned();
    let window = context_window.max(16_000);
    let estimated = estimate_tokens(SYSTEM_PROMPT)
        + estimate_turns(&request_history)
        + estimate_tokens(user_message)
        + estimate_tokens(&current_summary);
    if estimated > window.saturating_mul(3) / 4 && request_history.len() > 16 {
        let summarized_turns = request_history.len() - 12;
        send(StreamEvent {
            tool_name: Some("compact_context".to_owned()),
            ..Default::default()
        });
        current_summary = compact_history(
            access_token,
            account_id,
            model,
            &current_summary,
            &request_history[..summarized_turns],
            &http,
            &send,
        )?;
        request_history.drain(..summarized_turns);
        send(StreamEvent {
            compaction: Some(CompactionUpdate {
                summary: current_summary.clone(),
                summarized_turns,
            }),
            ..Default::default()
        });
    }
    let mut input = conversation_input(&current_summary, &request_history, user_message);
    let mut answer = String::new();
    let mut tool_rounds = 0;
    let mut call_sequence = 0_u64;
    let mut desktop_send_attempted = false;
    let force_connection_inventory = is_connection_inventory_question(user_message);
    let mut connection_inventory_requested = false;
    let mut actions_waiting_for_snapshot: Vec<ToolActivity> = Vec::new();
    let mut completion_diagnostics = "completion metadata unavailable".to_owned();
    let mut visible_delta_sent = false;
    let research_task = requires_research_evidence(user_message);
    let mut research_evidence_rows = 0_usize;
    let mut evidence_retry_count = 0_u8;

    loop {
        if crate::jev::cancel_requested() {
            return Err("Run cancelled.".into());
        }
        // Measure one ChatGPT round trip at a time. The total JEV task may
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
        let mut request = stream_http.post("https://chatgpt.com/backend-api/codex/responses")
            .bearer_auth(access_token)
            .header("originator", "codex_cli_rs")
            .header(reqwest::header::USER_AGENT, format!("JARVUS/{}", env!("CARGO_PKG_VERSION")))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&json!({"model":model,"reasoning":{"effort":reasoning_effort},"input":input,"tools":tools,"tool_choice":tool_choice,"parallel_tool_calls":false,"store":false,"stream":true}));
        if let Some(id) = account_id {
            request = request.header("ChatGPT-Account-ID", id);
        }
        let mut response =
            send_chat_request_with_cancel(&runtime, request, crate::jev::cancel_requested)?;

        let mut completed_output: Option<Vec<Value>> = None;
        let mut buffered_research_text = String::new();
        read_sse_lines_with_cancel(
            &runtime,
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
                                buffered_research_text.push_str(delta);
                            } else {
                                answer.push_str(delta);
                                send(StreamEvent {
                                    delta: Some(delta.to_owned()),
                                    ..Default::default()
                                });
                                visible_delta_sent = true;
                            }
                        }
                    }
                    "response.output_text.done" => {
                        if research_task {
                            if buffered_research_text.is_empty() {
                                if let Some(text) = event.get("text").and_then(Value::as_str) {
                                    buffered_research_text.push_str(text);
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
                                    visible_delta_sent = true;
                                }
                            }
                        }
                    }
                    "response.completed" => {
                        let response_value = event.get("response").unwrap_or(&event);
                        completed_output = response_value
                            .get("output")
                            .and_then(Value::as_array)
                            .cloned();
                        if research_task {
                            if buffered_research_text.is_empty() {
                                if let Some(text) =
                                    response_value.get("output_text").and_then(Value::as_str)
                                {
                                    buffered_research_text.push_str(text);
                                }
                            }
                        } else if answer.is_empty() {
                            if let Some(text) =
                                response_value.get("output_text").and_then(Value::as_str)
                            {
                                answer.push_str(text);
                            }
                        }
                        completion_diagnostics = response_diagnostics(response_value);
                        if let Some(usage) = parse_usage(response_value.get("usage")) {
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

        send(StreamEvent {
            latency_ms: Some(request_started.elapsed().as_millis()),
            ..Default::default()
        });
        let output =
            completed_output.ok_or("The response stream ended before response.completed.")?;
        let calls: Vec<Value> = output
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            .cloned()
            .collect();
        if calls.is_empty() {
            match research_completion_gate(
                research_task,
                research_evidence_rows,
                required_research_evidence_rows(user_message),
                evidence_retry_count,
            ) {
                ResearchCompletion::Retry => {
                    evidence_retry_count = evidence_retry_count.saturating_add(1);
                    input.extend(output);
                    input.push(json!({"role":"developer","content":"Completion gate: this is an explicit research or current-data request, and the verified source/search evidence collected so far does not meet the user's requested threshold or result count. Do not present a promise or unsupported findings as completion. Use available browser or MCP search/extract tools to fill the remaining evidence requirement. If no such tool is available or it fails, give a concise truthful blocker and say what could not be verified."}));
                    continue;
                }
                ResearchCompletion::Block => {
                    let blocker = research_blocker_reply();
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
                        "ChatGPT completed this request without a visible reply. {completion_diagnostics}"
                    ));
                }
                send(StreamEvent {
                    delta: Some(answer.clone()),
                    ..Default::default()
                });
                return Ok(answer);
            }
            if answer.trim().is_empty() {
                answer = completed_text(&output);
                if answer.trim().is_empty() {
                    return Err(format!(
                        "ChatGPT completed this request without a visible reply. {completion_diagnostics}"
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
            return Ok(answer);
        }
        tool_rounds += 1;
        if tool_rounds > 48 {
            return Err("JEV stopped after 48 tool rounds to keep this task bounded. Partial work is saved; ask JEV to continue from the latest verified state.".into());
        }

        // Preserve every output item (including reasoning items) when returning
        // the matching function_call_output items to the Responses API.
        input.extend(output);
        // JEV finishing ChatGPT's plan ends the turn here with the reply ChatGPT
        // wrote up front, so a successful task costs exactly one model request.
        let single_call = calls.len() == 1;
        let mut local_finish: Option<String> = None;
        let mut newly_loaded_tools: Vec<Value> = Vec::new();
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
            // loop so ChatGPT receives JEV's compact result and can verify,
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
            let result = if should_block_desktop_send_followup(name, desktop_send_attempted) {
                json!({"ok":false,"error":"desktop_send already ran for this request. Do not repeat or fall back to separate typing, readback, key, or click actions; use the returned status and fresh observation."})
            } else if should_block_low_level_send_input(name, explicit_send_requested) {
                json!({"ok":false,"error":"This request explicitly asks JEV to send a message. Use desktop_send with the fresh unique composer ref and exact user-authorized text; separate typing or key submission is unavailable."})
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
                if name == "list_mcps" && is_connection_inventory_question(user_message) {
                    local_finish = result
                        .pointer("/result/answer_hint")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                result
            };
            research_evidence_rows = research_evidence_rows.saturating_add(
                research_evidence_rows_from_tool(name, &result, user_message),
            );
            activity.result_detail = bounded_json(&result, 16_000);
            let succeeded = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if !succeeded {
                activity.state = "failed".into();
                activity.result_summary =
                    "Local action failed; details are in JEV’s response.".into();
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
            let model_result = compact_agent_output(&result);
            let model_result = if name == "jev_run_plan"
                && arguments.get("final").and_then(Value::as_bool) != Some(true)
            {
                json!({
                    "supervision_required": true,
                    "instruction": "This JEV plan is not the final answer. Inspect its result against the user's full goal. If evidence is incomplete, use the available browser or desktop tools to continue; otherwise explain what the evidence confirms and what remains unknown.",
                    "jev_result": model_result
                })
            } else {
                model_result
            };
            input.push(json!({"type":"function_call_output","call_id":call_id,"output":model_result.to_string()}));
        }
        if let Some(available) = tools.as_array_mut() {
            for schema in newly_loaded_tools {
                let name = schema.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                if !name.is_empty() && !available.iter().any(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str())) {
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

/// The user-facing reply for a plan JEV completed, or None when the plan
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
/// ChatGPT after JEV returns; only a one-action mechanical task may use the
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

fn requires_research_evidence(request: &str) -> bool {
    let text = request.to_ascii_lowercase();
    [
        "research",
        "search",
        "look up",
        "look for",
        "find games",
        "find me",
        "browse",
        "compare",
        "summarize",
        "summary",
        "current",
        "latest",
        "active players",
        "player count",
        "more than",
        "at least",
        "over ",
        "top ",
        "sources",
        "across sites",
        "statistics",
        "1k",
        "1,000",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

/// Evidence must come from a successful browser/search/extraction tool and
/// contain substantive returned data. A plan status, navigation result, or
/// assistant promise is never research evidence.
fn research_evidence_rows_from_tool(name: &str, result: &Value, request: &str) -> usize {
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return 0;
    }
    let name = name.to_ascii_lowercase();
    let web_or_search_tool =
        (name.starts_with("browser__") || name.starts_with("web_") || name.starts_with("mcp__"))
            && ["search", "extract", "fetch", "query"]
                .iter()
                .any(|kind| name.contains(kind));
    if !web_or_search_tool {
        return 0;
    }
    let payload = compact_agent_output(result).to_string();
    let normalized = payload.to_ascii_lowercase();
    let useful_chars = payload.chars().filter(|ch| ch.is_alphanumeric()).count();
    if useful_chars < 32
        || ["no results", "0 results", "no matches", "empty result"]
            .iter()
            .any(|text| normalized.contains(text))
    {
        return 0;
    }
    let request = request.to_ascii_lowercase();
    let metric_required = [
        "active player",
        "player count",
        "more than",
        "at least",
        "1k",
        "1,000",
        "over ",
    ]
    .iter()
    .any(|phrase| request.contains(phrase));
    if metric_required {
        let Some(threshold) = requested_threshold(&request) else {
            return 0;
        };
        return qualifying_metric_count(&payload, threshold, &request);
    }
    1
}

fn required_research_evidence_rows(request: &str) -> usize {
    let text = request.to_ascii_lowercase();
    if text
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| matches!(word, "games" | "options" | "items" | "sources"))
    {
        2
    } else {
        1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResearchCompletion {
    Accept,
    Retry,
    Block,
}

fn research_completion_gate(
    research_task: bool,
    evidence_rows: usize,
    required_rows: usize,
    retry_count: u8,
) -> ResearchCompletion {
    if !research_task || evidence_rows >= required_rows {
        return ResearchCompletion::Accept;
    }
    if retry_count == 0 {
        ResearchCompletion::Retry
    } else {
        ResearchCompletion::Block
    }
}

fn requested_threshold(request: &str) -> Option<f64> {
    let tokens: Vec<&str> = request.split_whitespace().collect();
    // Prefer a number attached to the requested comparison. A prompt such as
    // "find 10 games with more than 1K players" contains two numbers; using
    // the first one would silently weaken the actual player threshold to 10.
    let mut comparisons: Vec<(bool, usize, f64)> = Vec::new();
    for phrase in [
        "more than",
        "greater than",
        "at least",
        "over",
        "above",
        "no less than",
        "at most",
        "no more than",
        "less than",
        "under",
        "below",
    ] {
        for (position, _) in request.match_indices(phrase) {
            let suffix = &request[position + phrase.len()..];
            let suffix_tokens: Vec<&str> = suffix.split_whitespace().collect();
            if let Some((number_index, value)) = suffix_tokens
                .iter()
                .enumerate()
                .find_map(|(index, token)| parse_threshold_token(token).map(|value| (index, value)))
            {
                let metric_context =
                    suffix_tokens
                        .iter()
                        .skip(number_index + 1)
                        .take(5)
                        .any(|token| {
                            let token = token.to_ascii_lowercase();
                            [
                                "player", "playing", "active", "count", "user", "download",
                                "visit", "member",
                            ]
                            .iter()
                            .any(|metric| token.contains(metric))
                        });
                comparisons.push((metric_context, position, value));
            }
        }
    }
    if let Some((_, _, value)) = comparisons
        .iter()
        .filter(|(metric, _, _)| *metric)
        .max_by_key(|(_, position, _)| *position)
    {
        return Some(*value);
    }
    if comparisons.len() == 1 {
        return comparisons.first().map(|(_, _, value)| *value);
    }
    // If there is no comparison wording, use a sole numeric value. Ambiguous
    // multi-number requests remain blocked until the model gathers clearer evidence.
    let numbers: Vec<f64> = tokens
        .iter()
        .filter_map(|token| parse_threshold_token(token))
        .collect();
    if numbers.len() == 1 {
        numbers.first().copied()
    } else {
        None
    }
}

fn parse_threshold_token(token: &str) -> Option<f64> {
    let candidate =
        token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != ',');
    parse_compact_number(candidate)
}

fn parse_compact_number(value: &str) -> Option<f64> {
    let normalized = value.trim().trim_end_matches('+').replace(',', "");
    if normalized.is_empty() {
        return None;
    }
    let (digits, scale) = match normalized.chars().last()?.to_ascii_lowercase() {
        'k' => (&normalized[..normalized.len() - 1], 1_000.0),
        'm' => (&normalized[..normalized.len() - 1], 1_000_000.0),
        'b' => (&normalized[..normalized.len() - 1], 1_000_000_000.0),
        _ => (normalized.as_str(), 1.0),
    };
    let number = digits.parse::<f64>().ok()?;
    Some(number * scale)
}

fn qualifying_metric_count(payload: &str, threshold: f64, request: &str) -> usize {
    let words: Vec<&str> = payload.split_whitespace().collect();
    let lower: Vec<String> = words.iter().map(|word| word.to_ascii_lowercase()).collect();
    let strict_more = request.contains("more than")
        || request.contains("greater than")
        || request.contains("over ")
        || request.contains('>');
    let strict_less =
        request.contains("less than") || request.contains("under ") || request.contains('<');
    let at_most = request.contains("at most") || request.contains("no more than");
    let mut qualifying = 0;
    for (index, token) in words.iter().enumerate() {
        let candidate =
            token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != ',');
        let Some(value) = parse_compact_number(candidate) else {
            continue;
        };
        let start = index.saturating_sub(4);
        let end = (index + 5).min(lower.len());
        let context = lower[start..end].join(" ");
        if !["player", "playing", "active", "count"]
            .iter()
            .any(|metric| context.contains(metric))
        {
            continue;
        }
        let meets = if strict_more {
            value > threshold
        } else if strict_less {
            value < threshold
        } else if at_most {
            value <= threshold
        } else {
            value >= threshold
        };
        if meets {
            qualifying += 1;
        }
    }
    qualifying
}

fn research_blocker_reply() -> String {
    "I couldn’t verify enough source results to complete the requested research criteria. I haven’t confirmed that the requested threshold or result count was met.".into()
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
        return "Local tool failed; details are in JEV’s response.".into();
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
                "completed" => format!("JEV ran all {steps} step(s) · {total} ms"),
                "completed_unverified" => format!(
                    "JEV ran all {steps} step(s) · send unverified, do not retry · {total} ms"
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

fn compact_agent_output(value: &Value) -> Value {
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
                    // Raw accessibility trees remain local to JEV and are omitted from API history.
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
                    .take(48)
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
    let mut budget = 1200;
    let safe = compact(value, &mut budget, 0);
    if serde_json::to_vec(&safe)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
        <= 32_000
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
    old_turns: &[Turn],
    http: &Client,
    send: &impl Fn(StreamEvent),
) -> Result<String, String> {
    let mut input = vec![
        json!({"role":"developer","content":SYSTEM_PROMPT}),
        json!({"role":"developer","content":"Summarize older completed conversation turns for JEV. Preserve user goals, exact names and text still needed, decisions, constraints, safety boundaries, pending work, and verified outcomes. Remove incidental screen content and secrets. Do not perform desktop actions. Return a compact factual memory only."}),
    ];
    if !prior_summary.trim().is_empty() {
        input.push(json!({"role":"developer","content":format!("Existing compacted memory:\n{}", prior_summary.trim())}));
    }
    input.extend(
        old_turns
            .iter()
            .map(|turn| json!({"role":turn.role,"content":turn.content})),
    );
    input.push(
        json!({"role":"user","content":"Create the updated compacted conversation memory now."}),
    );

    let mut request = http.post("https://chatgpt.com/backend-api/codex/responses")
        .bearer_auth(access_token)
        .header("originator", "codex_cli_rs")
        .header(reqwest::header::USER_AGENT, format!("JARVUS/{}", env!("CARGO_PKG_VERSION")))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .json(&json!({"model":model,"input":input,"tools":[],"tool_choice":"none","parallel_tool_calls":false,"store":false,"stream":true}));
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

fn conversation_input(summary: &str, history: &[Turn], user_message: &str) -> Vec<Value> {
    let mut input = vec![
        json!({"role":"developer","content":SYSTEM_PROMPT}),
        json!({"role":"developer","content":"ChatGPT plans; JEV tools execute. Use fresh Agent Desktop observations before every state-changing step."}),
    ];
    if !summary.trim().is_empty() {
        input.push(json!({"role":"developer","content":format!("Earlier conversation summary (the full local transcript is preserved):\n{}", summary.trim())}));
    }
    input.extend(
        history
            .iter()
            .map(|turn| json!({"role":turn.role,"content":turn.content})),
    );
    input.push(json!({"role":"user","content":user_message}));
    input
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).saturating_add(3) / 4
}

pub fn estimate_context_tokens(summary: &str, history: &[Turn], user_message: &str) -> u64 {
    estimate_tokens(SYSTEM_PROMPT)
        .saturating_add(estimate_tokens("ChatGPT plans; JEV tools execute. Use fresh Agent Desktop observations before every state-changing step."))
        .saturating_add(estimate_tokens(summary))
        .saturating_add(estimate_turns(history))
        .saturating_add(estimate_tokens(user_message))
}

pub fn system_prompt_tokens() -> u64 {
    estimate_tokens(SYSTEM_PROMPT)
}

fn estimate_turns(turns: &[Turn]) -> u64 {
    turns
        .iter()
        .map(|turn| estimate_tokens(&turn.content).saturating_add(4))
        .sum()
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
    let text = message.to_lowercase();
    let asks_for_status = [
        "connected",
        "connection",
        "online",
        "live",
        "healthy",
        "health",
        "available",
        "enabled",
        "running",
        "active",
        "status",
    ]
    .iter()
    .any(|word| text.contains(word));
    let names_integration = ["mcp", "chrome", "browser", "integration", "server", "tools"]
        .iter()
        .any(|word| text.contains(word));
    asks_for_status && names_integration
}

#[cfg(test)]
mod tests {
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
            "JEV must reject a zero-step plan before touching the desktop"
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
            research_blocker_reply()
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
        // JARVUS_PLAN='<steps json>' cargo test --bin jarvus live_plan -- --ignored --nocapture
        let steps: Value = serde_json::from_str(&std::env::var("JARVUS_PLAN").unwrap()).unwrap();
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
                priority: Some(1),
                supported_reasoning_levels: vec!["low".into(), "high".into()],
                context_window: None,
            },
            Model {
                slug: "gpt-nano".into(),
                display_name: "Nano".into(),
                priority: Some(3),
                supported_reasoning_levels: vec!["medium".into(), "minimal".into()],
                context_window: None,
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
            priority: None,
            supported_reasoning_levels: vec!["low".into()],
            context_window: None,
        }];
        let no_effort_metadata = vec![Model {
            slug: "gpt-mini".into(),
            display_name: "Mini".into(),
            priority: None,
            supported_reasoning_levels: vec![],
            context_window: None,
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
            Some(1)
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

    #[test]
    fn named_app_and_control_workflows_reuse_fresh_observations() {
        assert!(SYSTEM_PROMPT.contains("call open_app directly"));
        assert!(SYSTEM_PROMPT.contains(
            "Do not call desktop_list_apps first because open_app already performs that check."
        ));
        assert!(SYSTEM_PROMPT.contains("desktop_find only when that observation omits the target"));
        assert!(
            SYSTEM_PROMPT
                .contains("A fresh post-action observation is already the next usable snapshot")
        );
        assert!(
            SYSTEM_PROMPT
                .contains("Call desktop_find only if the needed control is omitted or ambiguous.")
        );
        assert!(SYSTEM_PROMPT.contains("compact=false"));
        assert!(SYSTEM_PROMPT.contains("target_text"));
        assert!(SYSTEM_PROMPT.contains("interactive-only mode drops static row labels"));
        assert!(SYSTEM_PROMPT.contains("Prefer the visible matching"));
    }

    #[test]
    fn exact_send_prompt_precedence_and_example_use_only_desktop_send() {
        let send_rule = SYSTEM_PROMPT
            .find("Exact send routing has priority")
            .unwrap();
        let generic_type_rule = SYSTEM_PROMPT
            .find("Use desktop_type for text-only entry")
            .unwrap();
        assert!(
            send_rule < generic_type_rule,
            "send routing must precede generic typing guidance"
        );
        assert!(SYSTEM_PROMPT.contains("Do not call `desktop_type`, `desktop_get`, `desktop_press`, or `desktop_click` as part of"));
        assert!(SYSTEM_PROMPT.contains("type_failure.kind`"));
        let example = SYSTEM_PROMPT
            .split("## Example B: send the exact message the user requested")
            .nth(1)
            .unwrap()
            .split("## Example C:")
            .next()
            .unwrap();
        assert_eq!(
            example.matches("`desktop_send`").count(),
            2,
            "example should name desktop_send for the action and its outcome"
        );
        for forbidden_action in [
            "Use `desktop_type` with",
            "Use `desktop_get` on",
            "Use `desktop_press` with",
            "Use `desktop_click` for this send sequence",
        ] {
            assert!(
                !example.contains(forbidden_action),
                "send example must not teach fallback action {forbidden_action}"
            );
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
