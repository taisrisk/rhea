//! Per-chat local storage.
//!
//! Each conversation lives in `~/.jarvus/chats/<id>/` with metadata, message
//! history, and tool-call history split into separate JSON files. The index is
//! intentionally small so the app can populate its history list without
//! loading every transcript into memory.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Serialize, de::DeserializeOwned};

const INDEX_FILE: &str = "index.json";

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ChatIndex {
    #[serde(default)]
    pub chat_ids: Vec<String>,
    #[serde(default)]
    pub active_chat_id: Option<String>,
}

pub fn app_data_root() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .map(|home| home.join(".jarvus"))
}

/// Where screenshots and snapshot pictures are kept: `~/.jarvus/captures`. (Never a path baked in at
/// build time: that is the build machine's folder, which does not exist or is not writable on the
/// machine the app runs on.)
pub fn captures_dir() -> PathBuf {
    app_data_root().unwrap_or_else(env::temp_dir).join("captures")
}

/// What the model itself saw in a chat: the Responses API items (user messages, assistant messages,
/// tool calls and their results), the tool schemas it loaded with `get_tools_`, and where the real
/// user turns start. The ChatGPT Codex backend does not store conversations (`store:true` is
/// refused), so continuing a chat means sending this transcript again, exactly as Codex does. It is
/// kept per chat in `model.json`.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct ModelState {
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
    #[serde(default)]
    pub loaded_tools: Vec<serde_json::Value>,
    /// Index in `items` of every real user message (not teammate messages).
    #[serde(default)]
    pub turn_starts: Vec<usize>,
    /// How many transcript turns (user + assistant) these items account for; a mismatch with the
    /// visible transcript (an edit, a regenerate) makes the state stale and it is rebuilt from the turns.
    #[serde(default)]
    pub turns: usize,
}

pub fn load_model_state(id: &str) -> ModelState {
    read_chat_file::<ModelState>(id, "model.json").ok().flatten().unwrap_or_default()
}

pub fn save_model_state(id: &str, state: &ModelState) {
    let _ = write_chat_file(id, "model.json", state);
}

pub fn chats_root() -> Option<PathBuf> {
    app_data_root().map(|root| root.join("chats"))
}

pub fn index_path() -> Option<PathBuf> {
    chats_root().map(|root| root.join(INDEX_FILE))
}

pub fn chat_dir(id: &str) -> Option<PathBuf> {
    if !is_valid_chat_id(id) {
        return None;
    }
    chats_root().map(|root| root.join(id))
}

pub fn is_valid_chat_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub fn load_index() -> io::Result<Option<ChatIndex>> {
    let Some(path) = index_path() else {
        return Ok(None);
    };
    match fs::read(path) {
        Ok(bytes) => {
            let mut index: ChatIndex = serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            index.chat_ids.retain(|id| is_valid_chat_id(id));
            index.chat_ids.dedup();
            if index
                .active_chat_id
                .as_deref()
                .is_some_and(|id| !is_valid_chat_id(id))
            {
                index.active_chat_id = None;
            }
            Ok(Some(index))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // During the one-time migration the legacy file remains the
            // source of truth until the split files and index are complete.
            // Avoid adopting a partially written directory set after a crash.
            if legacy_conversations_path().is_some_and(|path| path.is_file()) {
                return Ok(None);
            }
            let Some(root) = chats_root() else {
                return Ok(None);
            };
            let mut ids = Vec::new();
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        continue;
                    }
                    let id = entry.file_name().to_string_lossy().into_owned();
                    if is_valid_chat_id(&id) && entry.path().join("metadata.json").is_file() {
                        ids.push(id);
                    }
                }
            }
            ids.sort();
            Ok((!ids.is_empty()).then_some(ChatIndex {
                chat_ids: ids,
                active_chat_id: None,
            }))
        }
        Err(error) => Err(error),
    }
}

pub fn save_index(index: &ChatIndex) -> io::Result<()> {
    let path = index_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "user home directory is unavailable",
        )
    })?;
    write_json_atomic(&path, index)
}

pub fn read_chat_file<T: DeserializeOwned>(id: &str, name: &str) -> io::Result<Option<T>> {
    if !matches!(name, "metadata.json" | "history.json" | "toolcalls.json" | "model.json") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported chat file",
        ));
    }
    let Some(path) = chat_dir(id).map(|dir| dir.join(name)) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid chat ID",
        ));
    };
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_chat_file<T: Serialize>(id: &str, name: &str, value: &T) -> io::Result<()> {
    if !matches!(name, "metadata.json" | "history.json" | "toolcalls.json") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported chat file",
        ));
    }
    let dir = chat_dir(id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"))?;
    fs::create_dir_all(&dir)?;
    write_json_atomic(&dir.join(name), value)
}

pub fn delete_chat(id: &str) -> io::Result<()> {
    let dir = chat_dir(id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"))?;
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut temporary_name = file_name.to_os_string();
    temporary_name.push(format!(".{}.{}.tmp", std::process::id(), nonce));
    let temporary_path = parent.join(temporary_name);
    let write_result = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        replace_file(&temporary_path, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
    let from = wide(from.as_os_str());
    let to = wide(to.as_os_str());
    // SAFETY: both paths are NUL-terminated UTF-16 buffers valid for the call.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn legacy_conversations_path() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("APPDATA"))
        .map(PathBuf::from)
        .map(|root| root.join("JARVUS").join("conversations.json"))
}

pub fn legacy_settings_path() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("APPDATA"))
        .map(PathBuf::from)
        .map(|root| root.join("JARVUS").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::is_valid_chat_id;

    #[test]
    fn chat_ids_cannot_escape_the_chats_directory() {
        assert!(is_valid_chat_id("chat-123-abcd-2"));
        assert!(is_valid_chat_id("chat_123"));
        assert!(!is_valid_chat_id(""));
        assert!(!is_valid_chat_id("../outside"));
        assert!(!is_valid_chat_id("chat/subdir"));
        assert!(!is_valid_chat_id("chat\\outside"));
    }
}
