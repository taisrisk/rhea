//! OpenCode as a second model provider, alongside ChatGPT.
//!
//! OpenCode Console (`opencode.ai`) fronts several model families behind one gateway. Auth here is
//! a plain API key (no OAuth device flow like ChatGPT's) pasted once in Settings and kept in the
//! OS credential store, same as the ChatGPT login. The catalog and the request/response shapes
//! below are read from OpenCode's own published docs (https://opencode.ai/docs/zen/, which the
//! product itself now calls "Console"), not verified against a live account or API key - say so
//! plainly wherever it matters, the same way Rhea already does for fast mode.
//!
//! Every text model works through the OpenAI-compatible chat-completions endpoint; OpenCode's own
//! docs say Console translates to and from each model's native API on its end, so Rhea only needs
//! to speak one wire format here regardless of which underlying model (GPT, Claude, Gemini, ...) is
//! selected.

use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::Value;

use crate::chat::{self, Model, Provider, RoundOutcome, StreamEvent};

const SERVICE: &str = "Rhea";
const ACCOUNT: &str = "opencode-api-key";
const MODELS_URL: &str = "https://opencode.ai/inference/v1/models";
pub const CHAT_URL: &str = "https://opencode.ai/inference/openai/v1/chat/completions";

pub fn save_api_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return clear_api_key();
    }
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?.set_password(key).map_err(|e| e.to_string())
}

pub fn load_api_key() -> Option<String> {
    keyring::Entry::new(SERVICE, ACCOUNT).ok()?.get_password().ok()
}

pub fn clear_api_key() -> Result<(), String> {
    match keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn connected() -> bool {
    load_api_key().is_some()
}

/// A masked form of the key for display ("sk-...ab12"), never the whole thing.
pub fn masked_api_key() -> Option<String> {
    let key = load_api_key()?;
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    Some(if key.chars().count() <= 8 { "•".repeat(key.chars().count()) } else { format!("{}...{}", &key[..4.min(key.len())], tail) })
}

fn model_from(entry: &Value) -> Option<Model> {
    let id = entry.get("id").or_else(|| entry.get("slug")).and_then(Value::as_str)?;
    let name = entry.get("name").or_else(|| entry.get("display_name")).and_then(Value::as_str).unwrap_or(id);
    let context_window = entry
        .get("context_window")
        .or_else(|| entry.get("context_length"))
        .or_else(|| entry.get("limit").and_then(|limit| limit.get("context")))
        .and_then(Value::as_u64);
    let max_context_window = entry.get("max_tokens").or_else(|| entry.get("limit").and_then(|limit| limit.get("output"))).and_then(Value::as_u64);
    Some(Model {
        slug: id.to_owned(),
        display_name: name.to_owned(),
        provider: Provider::OpenCode,
        priority: None,
        supported_reasoning_levels: Vec::new(),
        context_window,
        max_context_window,
    })
}

/// The model catalog OpenCode reports for this API key. Unverified against a live key - the exact
/// response shape (`{"models": [...]}`? a bare array? which keys each entry actually has) is read
/// from OpenCode's docs, which do not show the schema; this parses several plausible shapes and
/// reports plainly if none match, rather than silently returning an empty catalog.
pub fn models(api_key: &str) -> Result<Vec<Model>, String> {
    let http = Client::builder().timeout(Duration::from_secs(25)).build().map_err(|e| e.to_string())?;
    let response = http
        .get(MODELS_URL)
        .bearer_auth(api_key)
        .send()
        .map_err(|e| format!("OpenCode model list request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Could not load the OpenCode model catalog (check the API key): {e}"))?
        .json::<Value>()
        .map_err(|e| format!("OpenCode's model list response was not valid JSON: {e}"))?;
    models_from_response(&response)
}

fn models_from_response(response: &Value) -> Result<Vec<Model>, String> {
    let list = response
        .as_array()
        .or_else(|| response.get("models").and_then(Value::as_array))
        .or_else(|| response.get("data").and_then(Value::as_array))
        .ok_or("OpenCode's model list response had no recognizable list of models")?;
    let models: Vec<Model> = list.iter().filter_map(model_from).collect();
    if models.is_empty() {
        return Err("OpenCode returned a model list, but Rhea could not read any entries from it (its id/name fields may differ from what Rhea expects).".into());
    }
    Ok(models)
}

// ---- translating Rhea's local transcript to and from OpenAI-compatible chat completions -------
//
// Rhea's own `input` array (Responses-API item shapes) is the one source of truth for a chat's
// context - never provider-held state - so switching the selected model from ChatGPT to OpenCode
// mid-chat, or back, carries the same history either way: this just has to translate it each time.

/// One `input` item (`{"role":...,"content":...}`, `{"type":"function_call",...}`,
/// `{"type":"function_call_output",...}`, a ChatGPT-only `{"type":"reasoning",...}` trace, ...)
/// into Chat Completions messages. Consecutive `function_call` items (one assistant turn calling
/// several tools at once) merge into one assistant message with several `tool_calls`, which is how
/// Chat Completions represents that turn.
fn to_messages(input: &[Value]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut pending_calls: Vec<Value> = Vec::new();
    let flush = |messages: &mut Vec<Value>, pending_calls: &mut Vec<Value>| {
        if !pending_calls.is_empty() {
            messages.push(serde_json::json!({"role": "assistant", "content": Value::Null, "tool_calls": std::mem::take(pending_calls)}));
        }
    };
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let (Some(id), Some(name)) = (item.get("call_id").and_then(Value::as_str), item.get("name").and_then(Value::as_str)) else { continue };
                let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                pending_calls.push(serde_json::json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}}));
            }
            Some("function_call_output") => {
                flush(&mut messages, &mut pending_calls);
                let Some(id) = item.get("call_id").and_then(Value::as_str) else { continue };
                let output = item.get("output").cloned().unwrap_or(Value::Null);
                let content = output.as_str().map(str::to_owned).unwrap_or_else(|| output.to_string());
                messages.push(serde_json::json!({"role": "tool", "tool_call_id": id, "content": content}));
            }
            Some("message") => {
                flush(&mut messages, &mut pending_calls);
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""))
                    .unwrap_or_default();
                if !text.is_empty() {
                    messages.push(serde_json::json!({"role": "assistant", "content": text}));
                }
            }
            // A ChatGPT reasoning trace, or anything else this translation does not recognize:
            // no Chat Completions equivalent, and safe to drop rather than fail the request.
            Some(_) => {}
            None => {
                flush(&mut messages, &mut pending_calls);
                let Some(role) = item.get("role").and_then(Value::as_str) else { continue };
                let Some(content) = item.get("content").and_then(Value::as_str) else { continue };
                // "developer" (ChatGPT's system-prompt role) becomes "system", understood everywhere.
                let role = if role == "developer" { "system" } else { role };
                messages.push(serde_json::json!({"role": role, "content": content}));
            }
        }
    }
    flush(&mut messages, &mut pending_calls);
    messages
}

/// Responses-API tool schemas (`{"type":"function","name":...,"description":...,"parameters":...}`,
/// flat) into Chat Completions tool schemas (the same fields nested under `"function"`).
fn to_tools(tools_for_round: &Value) -> Value {
    let tools: Vec<Value> = tools_for_round
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            Some(serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
                    "parameters": tool.get("parameters").cloned().unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
                }
            }))
        })
        .collect();
    Value::Array(tools)
}

/// One streamed tool call, accumulated across however many chunks its `index` appears in: the id
/// and name usually arrive once, the arguments string arrives in fragments to concatenate in order.
#[derive(Default, Clone)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// One OpenCode round: translate `input`/`tools_for_round` into a Chat Completions request,
/// stream it, and translate the result back into the same Responses-API-shaped output
/// `chatgpt_round` produces, so the rest of the loop never needs to know which provider answered.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_round(
    runtime: &tokio::runtime::Runtime,
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
    input: &[Value],
    tools_for_round: &Value,
    endpoint: Option<&str>,
    research_task: bool,
    answer: &mut String,
    visible_delta_sent: &mut bool,
    send: &impl Fn(StreamEvent),
) -> Result<RoundOutcome, String> {
    let mut body = serde_json::json!({
        "model": model,
        "messages": to_messages(input),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let tools = to_tools(tools_for_round);
    if let Some(list) = tools.as_array() {
        if !list.is_empty() {
            body["tools"] = tools;
        }
    }
    let request = http.post(endpoint.unwrap_or(CHAT_URL)).bearer_auth(api_key).header(reqwest::header::ACCEPT, "text/event-stream").json(&body);
    let mut response = chat::send_chat_request_with_cancel(runtime, request, crate::jev::cancel_requested)?;

    let mut outcome = RoundOutcome::default();
    let mut calls: Vec<PendingCall> = Vec::new();
    let mut finished = false;
    // Whatever text this round streamed, kept separately from the cumulative `answer` (which can
    // already hold an earlier round's preamble): only this round's own text belongs in the
    // "message" item synthesized below for Rhea's own transcript continuity.
    let mut round_text = String::new();
    chat::read_sse_lines_with_cancel(
        runtime,
        &mut response,
        crate::jev::cancel_requested,
        |line| {
            let Some(payload) = line.strip_prefix("data:").map(str::trim) else { return Ok(()) };
            if payload.is_empty() {
                return Ok(());
            }
            if payload == "[DONE]" {
                finished = true;
                return Ok(());
            }
            let event: Value = match serde_json::from_str(payload) {
                Ok(value) => value,
                Err(_) => return Ok(()),
            };
            if let Some(error) = event.get("error") {
                let message = error.get("message").and_then(Value::as_str).unwrap_or("OpenCode returned an error").to_owned();
                send(StreamEvent { error: Some(message.clone()), ..Default::default() });
                return Err(message);
            }
            if let Some(usage) = event.get("usage") {
                let input_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
                let output_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
                outcome.usage_tokens = Some(input_tokens.saturating_add(output_tokens));
                send(StreamEvent { usage: Some(chat::Usage { input_tokens, output_tokens, ..Default::default() }), ..Default::default() });
            }
            let Some(choice) = event.get("choices").and_then(Value::as_array).and_then(|list| list.first()) else { return Ok(()) };
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    round_text.push_str(text);
                    if research_task {
                        outcome.research_text.push_str(text);
                    } else {
                        answer.push_str(text);
                        if !text.is_empty() {
                            send(StreamEvent { delta: Some(text.to_owned()), ..Default::default() });
                            *visible_delta_sent = true;
                        }
                    }
                }
                for fragment in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while calls.len() <= index {
                        calls.push(PendingCall::default());
                    }
                    let call = &mut calls[index];
                    if let Some(id) = fragment.get("id").and_then(Value::as_str) {
                        call.id = id.to_owned();
                    }
                    if let Some(name) = fragment.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                        call.name = name.to_owned();
                    }
                    if let Some(arguments) = fragment.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                        call.arguments.push_str(arguments);
                    }
                }
            }
            outcome.diagnostics = format!("opencode {model}, finish_reason {}", choice.get("finish_reason").and_then(Value::as_str).unwrap_or("none"));
            Ok(())
        },
    )?;
    if !finished {
        return Err("The OpenCode response stream ended before [DONE].".into());
    }
    outcome.output = calls
        .into_iter()
        .filter(|call| !call.id.is_empty() && !call.name.is_empty())
        .map(|call| serde_json::json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": call.arguments}))
        .collect();
    if outcome.output.is_empty() && !round_text.is_empty() {
        outcome.output.push(serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": round_text}]}));
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_model_list_wrapped_in_models_or_data_or_bare_is_read() {
        for wrapped in [
            json!({"models": [{"id": "claude-5", "name": "Claude 5", "context_window": 200_000}]}),
            json!({"data": [{"id": "claude-5", "name": "Claude 5", "context_window": 200_000}]}),
            json!([{"id": "claude-5", "name": "Claude 5", "context_window": 200_000}]),
        ] {
            let models = models_from_response(&wrapped).unwrap();
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].slug, "claude-5");
            assert_eq!(models[0].display_name, "Claude 5");
            assert_eq!(models[0].provider, Provider::OpenCode);
            assert_eq!(models[0].context_limit(), Some(200_000));
        }
    }

    #[test]
    fn an_unrecognizable_response_is_a_clear_error_not_an_empty_catalog() {
        assert!(models_from_response(&json!({"nonsense": true})).is_err());
        assert!(models_from_response(&json!({"models": [{"nonsense": true}]})).is_err());
    }

    #[test]
    fn a_masked_key_never_shows_the_whole_thing() {
        let masked = |key: &str| {
            let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            if key.chars().count() <= 8 { "•".repeat(key.chars().count()) } else { format!("{}...{}", &key[..4.min(key.len())], tail) }
        };
        assert_eq!(masked("sk-abcdefghijklmnop"), "sk-a...mnop");
        assert!(!masked("sk-abcdefghijklmnop").contains("efghijkl"));
    }
}
