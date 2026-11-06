use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, blocking::Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USER_CODE_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const POLL_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const SERVICE: &str = "Rhea";
/// What earlier versions called the credential store entry; a saved login under it is moved over
/// the first time it is needed.
const LEGACY_SERVICE: &str = "JARVUS";

thread_local! {
    static SERVICE_NAME: std::cell::Cell<&'static str> = const { std::cell::Cell::new(SERVICE) };
}

fn service() -> &'static str {
    SERVICE_NAME.with(std::cell::Cell::get)
}

/// Run `action` against the old entry names (this thread only).
fn with_legacy_service<T>(action: impl FnOnce() -> T) -> T {
    SERVICE_NAME.with(|name| name.set(LEGACY_SERVICE));
    let result = action();
    SERVICE_NAME.with(|name| name.set(SERVICE));
    result
}

/// A login saved by an earlier version: read it, save it under the new name and remove the old
/// entries, so the user stays signed in.
fn migrate_legacy_login() -> Option<Tokens> {
    let old = with_legacy_service(load).ok()?;
    if save(&old).is_ok() {
        let _ = with_legacy_service(delete_all_entries);
    }
    Some(old)
}
#[cfg(target_os = "macos")]
const CREDENTIAL_STORE: &str = "macOS Keychain";
#[cfg(target_os = "windows")]
const CREDENTIAL_STORE: &str = "Windows Credential Manager";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CREDENTIAL_STORE: &str = "system credential store";
const MANIFEST_ACCOUNT: &str = "chatgpt-codex-device-manifest";
const LEGACY_ACCOUNT: &str = "chatgpt-codex-device-v1";
const POLL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CREDENTIAL_CHUNK_CHARS: usize = 1000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub client_id: String,
    pub expires_at: u64,
    pub email: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Deserialize)]
struct UserCodeReply {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Option<Value>,
}

#[derive(Deserialize)]
struct DeviceTokenReply {
    authorization_code: String,
    #[serde(rename = "code_challenge")]
    _code_challenge: String,
    code_verifier: String,
}

#[derive(Default, Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<Value>,
}

/// The session lives in memory after the first read: the credential store (the macOS Keychain
/// especially) asks the user for permission on every read, and every chat run, title, subagent
/// and model refresh needs the token.
static SESSION: Mutex<Option<Tokens>> = Mutex::new(None);
/// One refresh at a time; a rotated refresh token must not be spent twice.
static REFRESH: Mutex<()> = Mutex::new(());

fn remember(tokens: Option<&Tokens>) {
    *SESSION.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = tokens.cloned();
}

pub fn token() -> Result<Tokens, String> {
    let _one_at_a_time = REFRESH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let cached = SESSION.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let mut tokens = match cached {
        Some(tokens) => tokens,
        None => {
            let loaded = match load() {
                Ok(loaded) => loaded,
                Err(error) => migrate_legacy_login().ok_or(error)?,
            };
            remember(Some(&loaded));
            loaded
        }
    };
    if tokens.expires_at > now_secs().saturating_add(60) {
        return Ok(tokens);
    }
    let http = client()?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", &tokens.refresh_token)
        .append_pair("client_id", CLIENT_ID)
        .finish();
    let response = http
        .post(TOKEN_ENDPOINT)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .map_err(|e| format!("ChatGPT token refresh failed: {e}"))?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "ChatGPT token refresh returned HTTP {status}: {}",
            concise_error(&body)
        ));
    }
    let update: TokenReply = serde_json::from_str(&body)
        .map_err(|e| format!("ChatGPT token refresh response was invalid: {e}"))?;
    if let Some(value) = update.access_token {
        tokens.access_token = value;
    }
    if let Some(value) = update.refresh_token {
        tokens.refresh_token = value;
    }
    if let Some(value) = update.id_token {
        tokens.id_token = value;
    }
    tokens.expires_at =
        now_secs().saturating_add(parse_seconds(update.expires_in.as_ref()).unwrap_or(3600));
    save(&tokens)?;
    remember(Some(&tokens));
    Ok(tokens)
}

/// Starts the official Codex device authorization flow. Calls `show_code` as soon as
/// OpenAI returns the one-time code, then waits for the user to approve it in the browser.
pub fn sign_in(
    mut show_code: impl FnMut(String, String, Option<String>),
) -> Result<String, String> {
    let http = client()?;
    let response = http
        .post(USER_CODE_ENDPOINT)
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .map_err(|e| format!("Could not request a ChatGPT device code: {e}"))?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "OpenAI device-code request returned HTTP {status}: {}",
            concise_error(&body)
        ));
    }
    let device: UserCodeReply = serde_json::from_str(&body)
        .map_err(|e| format!("OpenAI returned an invalid device-code response: {e}"))?;
    let poll_seconds = parse_seconds(device.interval.as_ref())
        .unwrap_or(5)
        .clamp(3, 30);
    show_code(VERIFICATION_URL.to_owned(), device.user_code.clone(), None);
    if let Err(error) = open_verification_page() {
        show_code(
            VERIFICATION_URL.to_owned(),
            device.user_code.clone(),
            Some(error),
        );
    }

    let deadline = Instant::now() + POLL_TIMEOUT;
    let device_tokens = loop {
        if Instant::now() >= deadline {
            return Err(
                "ChatGPT device authorization expired. Start sign-in again for a new code.".into(),
            );
        }
        let response = http
            .post(POLL_ENDPOINT)
            .json(&json!({"device_auth_id": device.device_auth_id, "user_code": device.user_code}))
            .send()
            .map_err(|e| format!("Could not check ChatGPT device authorization: {e}"))?;
        let status = response.status();
        if status.is_success() {
            break response
                .json::<DeviceTokenReply>()
                .map_err(|e| format!("OpenAI returned an invalid device authorization: {e}"))?;
        }
        if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
            std::thread::sleep(
                Duration::from_secs(poll_seconds)
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
            continue;
        }
        let body = response.text().unwrap_or_default();
        return Err(format!(
            "ChatGPT device authorization returned HTTP {status}: {}",
            concise_error(&body)
        ));
    };

    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &device_tokens.authorization_code)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code_verifier", &device_tokens.code_verifier)
        .finish();
    let response = http
        .post(TOKEN_ENDPOINT)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .map_err(|e| format!("ChatGPT authorization-code exchange failed: {e}"))?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "ChatGPT token exchange returned HTTP {status}: {}",
            concise_error(&body)
        ));
    }
    let token_reply: TokenReply = serde_json::from_str(&body)
        .map_err(|e| format!("ChatGPT token response was invalid: {e}"))?;
    let access_token = token_reply
        .access_token
        .ok_or("ChatGPT token response did not include an access token")?;
    let refresh_token = token_reply
        .refresh_token
        .ok_or("ChatGPT token response did not include a refresh token")?;
    let id_token = token_reply.id_token.unwrap_or_default();
    let claims = decode_token_claims(&id_token).unwrap_or_default();
    let auth_claims = claims.pointer("/https://api.openai.com/auth");
    let tokens = Tokens {
        access_token,
        refresh_token,
        id_token,
        client_id: CLIENT_ID.to_owned(),
        expires_at: now_secs()
            .saturating_add(parse_seconds(token_reply.expires_in.as_ref()).unwrap_or(3600)),
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        account_id: auth_claims
            .and_then(|v| v.get("chatgpt_account_id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    save(&tokens)?;
    Ok(tokens
        .email
        .clone()
        .unwrap_or_else(|| "ChatGPT account".into()))
}

pub fn sign_out() -> Result<(), String> {
    remember(None);
    // Also forget a login an earlier version left under its old name.
    let _ = with_legacy_service(delete_all_entries);
    delete_all_entries()
}

fn delete_all_entries() -> Result<(), String> {
    let single = keyring::Entry::new(service(), SINGLE_ACCOUNT).map_err(|e| e.to_string())?;
    match single.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(format!("Could not clear the saved ChatGPT session: {e}")),
    }
    let old = read_manifest().ok();
    if let Some((generation, count)) = old {
        delete_chunks(&generation, count)?;
    }
    for account in [MANIFEST_ACCOUNT, LEGACY_ACCOUNT] {
        let entry = keyring::Entry::new(service(), account).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(format!("Could not clear the saved ChatGPT session: {e}")),
        }
    }
    Ok(())
}

pub fn open_verification_page() -> Result<(), String> {
    #[cfg(windows)]
    {
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
        let verb: Vec<u16> = std::ffi::OsStr::new("open")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let url: Vec<u16> = std::ffi::OsStr::new(VERIFICATION_URL)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                verb.as_ptr(),
                url.as_ptr(),
                null_mut(),
                null_mut(),
                1,
            )
        };
        if (result as isize) <= 32 {
            return Err(format!(
                "Windows could not open the default browser (ShellExecuteW returned {}).",
                result as isize
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        webbrowser::open(VERIFICATION_URL)
            .map(|_| ())
            .map_err(|e| format!("Could not open the verification page: {e}"))
    }
}

/// Windows Credential Manager caps one credential at about 2.5 KB, so there the session is split
/// into pieces (see `save_chunked`). Everywhere else it is a single credential, one read.
const SINGLE_ACCOUNT: &str = "chatgpt-codex-device-v2";

#[cfg(windows)]
fn load() -> Result<Tokens, String> {
    load_chunked()
}

#[cfg(windows)]
fn save(tokens: &Tokens) -> Result<(), String> {
    save_chunked(tokens)
}

#[cfg(not(windows))]
fn load() -> Result<Tokens, String> {
    let entry = keyring::Entry::new(service(), SINGLE_ACCOUNT).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(raw) => return serde_json::from_str(&raw).map_err(|e| format!("Saved ChatGPT session is unreadable: {e}")),
        Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(format!("Could not read the saved ChatGPT session from {CREDENTIAL_STORE}: {e}")),
    }
    // An earlier build stored the session in many small credentials (a prompt for each one);
    // move it into the single credential once and remove the pieces.
    let tokens = load_chunked()?;
    if save(&tokens).is_ok() {
        if let Ok((generation, count)) = read_manifest() {
            let _ = delete_chunks(&generation, count);
        }
        if let Ok(manifest) = keyring::Entry::new(service(), MANIFEST_ACCOUNT) {
            let _ = manifest.delete_credential();
        }
    }
    Ok(tokens)
}

#[cfg(not(windows))]
fn save(tokens: &Tokens) -> Result<(), String> {
    let raw = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
    let entry = keyring::Entry::new(service(), SINGLE_ACCOUNT).map_err(|e| e.to_string())?;
    entry
        .set_password(&raw)
        .map_err(|e| format!("Could not save ChatGPT credentials to {CREDENTIAL_STORE}: {e}"))
}

fn load_chunked() -> Result<Tokens, String> {
    let (generation, count) = match read_manifest() {
        Ok(value) => value,
        Err(_) => {
            let legacy = keyring::Entry::new(service(), LEGACY_ACCOUNT).map_err(|e| e.to_string())?;
            let raw = legacy
                .get_password()
                .map_err(|e| format!("No saved ChatGPT session: {e}"))?;
            return serde_json::from_str(&raw)
                .map_err(|e| format!("Saved ChatGPT session is unreadable: {e}"));
        }
    };
    let mut raw = String::new();
    for index in 0..count {
        let entry = keyring::Entry::new(service(), &chunk_account(&generation, index))
            .map_err(|e| e.to_string())?;
        raw.push_str(
            &entry
                .get_password()
                .map_err(|e| format!("Could not read saved ChatGPT token chunk {index}: {e}"))?,
        );
    }
    serde_json::from_str(&raw).map_err(|e| format!("Saved ChatGPT session is unreadable: {e}"))
}

#[cfg(windows)]
fn save_chunked(tokens: &Tokens) -> Result<(), String> {
    let raw = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
    let old = read_manifest().ok();
    let generation = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let chunks: Vec<String> = raw
        .chars()
        .collect::<Vec<_>>()
        .chunks(CREDENTIAL_CHUNK_CHARS)
        .map(|chunk| chunk.iter().collect())
        .collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let entry = keyring::Entry::new(service(), &chunk_account(&generation, index))
            .map_err(|e| e.to_string())?;
        if let Err(error) = entry.set_password(chunk) {
            let _ = delete_chunks(&generation, index + 1);
            return Err(format!(
                "Could not save ChatGPT credentials to {CREDENTIAL_STORE}: {error}"
            ));
        }
    }
    let manifest = keyring::Entry::new(service(), MANIFEST_ACCOUNT).map_err(|e| e.to_string())?;
    if let Err(error) = manifest.set_password(&format!("{generation}:{}", chunks.len())) {
        let _ = delete_chunks(&generation, chunks.len());
        return Err(format!(
            "Could not finish saving ChatGPT credentials to {CREDENTIAL_STORE}: {error}"
        ));
    }
    if let Some((old_generation, old_count)) = old {
        let _ = delete_chunks(&old_generation, old_count);
    }
    Ok(())
}

fn read_manifest() -> Result<(String, usize), String> {
    let entry = keyring::Entry::new(service(), MANIFEST_ACCOUNT).map_err(|e| e.to_string())?;
    let value = entry.get_password().map_err(|e| e.to_string())?;
    let (generation, count) = value
        .split_once(':')
        .ok_or("Credential manifest is invalid")?;
    let count = count.parse::<usize>().map_err(|e| e.to_string())?;
    if generation.is_empty() || count == 0 || count > 100 {
        return Err("Credential manifest values are out of range".into());
    }
    Ok((generation.to_owned(), count))
}

fn chunk_account(generation: &str, index: usize) -> String {
    format!("chatgpt-codex-device-v1-{generation}-{index}")
}

fn delete_chunks(generation: &str, count: usize) -> Result<(), String> {
    let mut first_error = None;
    for index in 0..count {
        let entry = keyring::Entry::new(service(), &chunk_account(generation, index))
            .map_err(|e| e.to_string())?;
        if let Err(error) = entry.delete_credential() {
            if !matches!(error, keyring::Error::NoEntry) && first_error.is_none() {
                first_error = Some(error.to_string());
            }
        }
    }
    first_error.map_or(Ok(()), |e| {
        Err(format!("Could not clear saved ChatGPT token chunks: {e}"))
    })
}

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("Rhea/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

fn decode_token_claims(token: &str) -> Result<Value, String> {
    let payload = token.split('.').nth(1).ok_or("ID token is malformed")?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn parse_seconds(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn concise_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(400).collect())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
