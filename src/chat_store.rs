//! Per-chat local storage.
//!
//! Each conversation lives in `~/.rhea/chats/<id>/` with metadata, message
//! history, and tool-call history split into separate JSON files. The index is
//! intentionally small so the app can populate its history list without
//! loading every transcript into memory.

use std::{
    collections::HashMap,
    env, fs,
    io::{self},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
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

/// Rhea's data folder inside the home folder, and the name earlier versions used.
pub const DATA_DIR: &str = ".rhea";
pub const LEGACY_DATA_DIR: &str = ".jarvus";

/// Saved chats and hooks written by earlier versions name the Shell tools `mcp__jarvus_shell__*`.
/// They are read as `mcp__rhea_shell__*` so old chats keep their tool history and an old
/// transcript still matches the tools the model is given.
pub fn modernize_tool_names(text: &str) -> String {
    text.replace("mcp__jarvus_shell__", "mcp__rhea_shell__")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from)
}

pub fn app_data_root() -> Option<PathBuf> {
    home_dir().map(|home| home.join(DATA_DIR))
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Move one entry; a rename is instant, and when it cannot be done (another drive, a file in use)
/// the entry is copied and only then removed.
fn move_entry(from: &Path, to: &Path) -> io::Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        copy_dir(from, to)?;
        fs::remove_dir_all(from)
    } else {
        fs::copy(from, to)?;
        fs::remove_file(from)
    }
}

/// Bring a `~/.rhea` folder from an earlier version over to `~/.rhea`. Nothing already in the
/// new folder is overwritten, so it is safe to run on every start and to resume after an
/// interruption. The Chrome extension folder is copied and the old one is left in place: Chrome
/// remembers the extension by that path and would lose it if the folder vanished. Returns how
/// many entries were moved.
pub fn migrate_legacy_data_in(home: &Path) -> io::Result<usize> {
    let (old, new) = (home.join(LEGACY_DATA_DIR), home.join(DATA_DIR));
    if !old.is_dir() {
        return Ok(0);
    }
    let mut moved = 0;
    let entries: Vec<_> = fs::read_dir(&old)?.flatten().collect();
    for entry in entries {
        let name = entry.file_name();
        let target = new.join(&name);
        if target.exists() {
            continue;
        }
        fs::create_dir_all(&new)?;
        if name == "chrome-extension" {
            copy_dir(&entry.path(), &target)?;
        } else {
            move_entry(&entry.path(), &target)?;
        }
        moved += 1;
    }
    // Remove the old folder once only the extension copy is left in it... which stays. Otherwise
    // an empty one goes.
    let _ = fs::remove_dir(&old);
    Ok(moved)
}

/// Run at every start before anything reads the data folder.
pub fn migrate_legacy_data() {
    if let Some(home) = home_dir() {
        if let Err(error) = migrate_legacy_data_in(&home) {
            eprintln!("Could not move {LEGACY_DATA_DIR} to {DATA_DIR}: {error}");
        }
    }
}

/// A path as people write it: inside the home folder it becomes `~/...` (shorter, and it keeps the
/// user name out of settings pages and screenshots).
pub fn tilde(path: &Path) -> String {
    if let Some(home) = app_data_root().and_then(|root| root.parent().map(Path::to_path_buf)) {
        if let Ok(rest) = path.strip_prefix(&home) {
            let rest = rest.display().to_string().replace('\\', "/");
            return if rest.is_empty() { "~".to_owned() } else { format!("~/{rest}") };
        }
    }
    path.display().to_string()
}

/// Where screenshots and snapshot pictures are kept: `~/.rhea/captures`. (Never a path baked in at
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

/// Transcript state for chats in use, kept parsed in memory so a message does not reread and
/// reparse a large file. Every save writes the whole file to a temporary file and swaps it into
/// place (a crash or a full disk leaves the previous, intact file), and a failed save is reported
/// (`take_save_error`) instead of being swallowed. `close_chat_files` drops the cache five seconds
/// after the user leaves a chat, and before a chat folder is moved or deleted.
struct OpenState {
    state: ModelState,
}

type OpenKey = (String, String);

fn open_states() -> &'static Mutex<HashMap<OpenKey, OpenState>> {
    static OPEN: OnceLock<Mutex<HashMap<OpenKey, OpenState>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_open() -> std::sync::MutexGuard<'static, HashMap<OpenKey, OpenState>> {
    open_states().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The latest save that failed, for the UI to show once.
fn save_error_cell() -> &'static Mutex<Option<String>> {
    static ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    ERROR.get_or_init(|| Mutex::new(None))
}

pub fn take_save_error() -> Option<String> {
    save_error_cell().lock().unwrap_or_else(|p| p.into_inner()).take()
}

pub fn report_save_error(what: &str, error: &io::Error) {
    *save_error_cell().lock().unwrap_or_else(|p| p.into_inner()) = Some(format!("Could not save {what}: {error}. Your chat on screen is fine, but this part may not be on disk."));
}

/// `model.json` for the chat itself, `agent-<id>.json` for one of its subagents.
fn state_file_name(agent: Option<&str>) -> io::Result<String> {
    match agent {
        None => Ok("model.json".to_owned()),
        Some(agent) if is_valid_chat_id(agent) => Ok(format!("agent-{agent}.json")),
        Some(_) => Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid agent ID")),
    }
}

fn read_state(id: &str, agent: Option<&str>) -> ModelState {
    let Ok(name) = state_file_name(agent) else { return ModelState::default() };
    let key = (id.to_owned(), name.clone());
    if let Some(open) = lock_open().get(&key) {
        return open.state.clone();
    }
    let Some(path) = chat_dir(id).map(|dir| dir.join(&name)) else { return ModelState::default() };
    let Ok(bytes) = fs::read(&path) else { return ModelState::default() };
    let state: ModelState = serde_json::from_str(&modernize_tool_names(&String::from_utf8_lossy(&bytes))).unwrap_or_default();
    lock_open().insert(key, OpenState { state: state.clone() });
    state
}

fn write_state(id: &str, agent: Option<&str>, state: &ModelState) -> io::Result<()> {
    let name = state_file_name(agent)?;
    let dir = chat_dir(id).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"))?;
    fs::create_dir_all(&dir)?;
    write_json_atomic_compact(&dir.join(&name), state)?;
    lock_open().insert((id.to_owned(), name), OpenState { state: state.clone() });
    Ok(())
}

pub fn load_model_state(id: &str) -> ModelState {
    read_state(id, None)
}

pub fn save_model_state(id: &str, state: &ModelState) {
    if let Err(error) = write_state(id, None, state) {
        report_save_error("this chat's model transcript", &error);
    }
}

pub fn load_agent_state(id: &str, agent: &str) -> ModelState {
    read_state(id, Some(agent))
}

pub fn save_agent_state(id: &str, agent: &str, state: &ModelState) {
    if let Err(error) = write_state(id, Some(agent), state) {
        report_save_error("a subagent's transcript", &error);
    }
}

/// Forget the cached state of a chat (it was left alone for a while, or its folder is about to
/// be moved or deleted).
pub fn close_chat_files(id: &str) {
    lock_open().retain(|(chat, _), _| chat != id);
}

pub fn open_chat_file_count() -> usize {
    lock_open().len()
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

/// Folder inside the chats folder that holds archived chats. It has no `metadata.json` of its own,
/// so the chat scan never mistakes it for a chat.
pub const ARCHIVE_DIR: &str = "Archives";

pub fn archive_root() -> Option<PathBuf> {
    chats_root().map(|root| root.join(ARCHIVE_DIR))
}

/// Move a chat's folder into `Archives`. Nothing is deleted; `restore_chat` moves it back.
pub fn archive_chat(id: &str) -> io::Result<()> {
    close_chat_files(id);
    move_chat(id, chat_dir(id), archive_root())
}

pub fn restore_chat(id: &str) -> io::Result<()> {
    if !is_valid_chat_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"));
    }
    move_chat(id, archive_root().map(|root| root.join(id)), chats_root())
}

fn move_chat(id: &str, from: Option<PathBuf>, to_parent: Option<PathBuf>) -> io::Result<()> {
    let (Some(from), Some(to_parent)) = (from, to_parent) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"));
    };
    if !from.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "that chat has no saved files"));
    }
    fs::create_dir_all(&to_parent)?;
    let target = to_parent.join(id);
    // Never remove what is already there: a chat with this ID in the destination is a different
    // copy that must not be lost, so the move stops and says so.
    if target.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a chat with the same ID already exists there; nothing was changed"));
    }
    fs::rename(from, target)
}

/// IDs of the archived chats, newest first (chat IDs sort by creation).
pub fn archived_ids() -> Vec<String> {
    let Some(root) = archive_root() else { return Vec::new() };
    let mut ids: Vec<String> = fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().join("metadata.json").is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|id| is_valid_chat_id(id))
                .collect()
        })
        .unwrap_or_default();
    ids.sort_by(|a, b| b.cmp(a));
    ids
}

pub fn read_archived_file<T: DeserializeOwned>(id: &str, name: &str) -> io::Result<Option<T>> {
    if !is_valid_chat_id(id) || !matches!(name, "metadata.json" | "history.json") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported archived file"));
    }
    let Some(path) = archive_root().map(|root| root.join(id).join(name)) else { return Ok(None) };
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn delete_archived_chat(id: &str) -> io::Result<()> {
    if !is_valid_chat_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"));
    }
    match archive_root().map(|root| fs::remove_dir_all(root.join(id))) {
        Some(Err(error)) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
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
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            serde_json::from_str(&modernize_tool_names(&text))
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
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
    close_chat_files(id);
    let dir = chat_dir(id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid chat ID"))?;
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_bytes_atomic(path, &bytes)
}

/// Like `write_json_atomic` without the indentation (large transcripts).
pub fn write_json_atomic_compact<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_bytes_atomic(path, &bytes)
}

/// Write to a temporary file, flush it to disk, then swap it into place.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(parent)?;
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
        file.write_all(bytes)?;
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
    fn an_old_data_folder_moves_to_the_new_name_once_and_keeps_the_extension_where_chrome_has_it() {
        let home = std::env::temp_dir().join(format!("rhea-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let old = home.join(super::LEGACY_DATA_DIR);
        std::fs::create_dir_all(old.join("chats/chat-1")).unwrap();
        std::fs::write(old.join("chats/chat-1/history.json"), "{}").unwrap();
        std::fs::write(old.join("settings.json"), "{\"a\":1}").unwrap();
        std::fs::create_dir_all(old.join("chrome-extension")).unwrap();
        std::fs::write(old.join("chrome-extension/manifest.json"), "{}").unwrap();

        assert_eq!(super::migrate_legacy_data_in(&home).unwrap(), 3);
        let new = home.join(super::DATA_DIR);
        assert!(new.join("chats/chat-1/history.json").is_file() && new.join("settings.json").is_file());
        assert!(new.join("chrome-extension/manifest.json").is_file());
        assert!(old.join("chrome-extension/manifest.json").is_file(), "Chrome still loads the old extension folder");
        assert!(!old.join("chats").exists() && !old.join("settings.json").exists());

        // running again changes nothing, and never overwrites what is already in the new folder
        std::fs::write(old.join("settings.json"), "{\"a\":2}").unwrap();
        assert_eq!(super::migrate_legacy_data_in(&home).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "{\"a\":1}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn archiving_or_restoring_never_replaces_a_chat_that_is_already_there() {
        let root = std::env::temp_dir().join(format!("rhea-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (from, to_parent) = (root.join("a").join("chat-1"), root.join("b"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("history.json"), "moving").unwrap();
        std::fs::create_dir_all(to_parent.join("chat-1")).unwrap();
        std::fs::write(to_parent.join("chat-1").join("history.json"), "already here").unwrap();
        let error = super::move_chat("chat-1", Some(from.clone()), Some(to_parent.clone())).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(to_parent.join("chat-1").join("history.json")).unwrap(), "already here");
        assert_eq!(std::fs::read_to_string(from.join("history.json")).unwrap(), "moving", "the source is untouched too");
        std::fs::remove_dir_all(to_parent.join("chat-1")).unwrap();
        super::move_chat("chat-1", Some(from), Some(to_parent.clone())).unwrap();
        assert!(to_parent.join("chat-1").join("history.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_and_reports_the_error() {
        let root = std::env::temp_dir().join(format!("rhea-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("model.json");
        super::write_bytes_atomic(&path, b"{\"turns\":1}").unwrap();
        // a directory sits where the new file must go, so the swap fails
        let blocked = root.join("blocked.json");
        std::fs::create_dir_all(blocked.join("inner")).unwrap();
        assert!(super::write_bytes_atomic(&blocked, b"x").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"turns\":1}");
        assert_eq!(std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).count(), 0, "no temporary file is left behind");
        super::report_save_error("test", &std::io::Error::other("disk full"));
        assert!(super::take_save_error().unwrap().contains("disk full"));
        assert!(super::take_save_error().is_none(), "reported once");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tool_names_from_older_saved_chats_are_read_under_their_new_name() {
        assert_eq!(super::modernize_tool_names(r#"{"name":"mcp__jarvus_shell__run_command"}"#), r#"{"name":"mcp__rhea_shell__run_command"}"#);
        assert_eq!(super::modernize_tool_names("browser__click"), "browser__click");
    }

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
