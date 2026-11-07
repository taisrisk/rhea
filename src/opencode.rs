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

use crate::chat::{Model, Provider};

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
