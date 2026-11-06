//! Getting Rhea's Chrome extension into Chrome.
//!
//! Chrome only loads an unpacked extension when the user picks its folder in
//! `chrome://extensions` (no flag or policy can do that silently in a normal Chrome), so the
//! one-click flow does everything around that single step: download the extension from GitHub
//! (falling back to the copy built into the app) into a folder that never moves, register the
//! native host, copy the folder path to the clipboard and open Chrome's extensions page. Afterwards
//! `check()` reports each stage, so the settings page can show what still needs doing.
//!
//! The folder is `~/.rhea/chrome-extension` on every platform. Chrome remembers an unpacked
//! extension by path and drops it when that folder disappears, so the app's own install folder or
//! a build directory must never be what Chrome points at.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{browser, chat_store};

/// Files built into the app, used when GitHub cannot be reached.
const EMBEDDED: &[(&str, &[u8])] = &[
    ("manifest.json", include_bytes!("../chrome-extension/manifest.json")),
    ("background.js", include_bytes!("../chrome-extension/background.js")),
    ("content.js", include_bytes!("../chrome-extension/content.js")),
    ("action-policy.js", include_bytes!("../chrome-extension/action-policy.js")),
    ("popup.html", include_bytes!("../chrome-extension/popup.html")),
    ("popup.css", include_bytes!("../chrome-extension/popup.css")),
    ("popup.js", include_bytes!("../chrome-extension/popup.js")),
];

/// The one folder Chrome is pointed at.
pub fn folder() -> PathBuf {
    chat_store::app_data_root().unwrap_or_else(env::temp_dir).join("chrome-extension")
}

fn manifest_version(manifest: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(manifest).ok()?.get("version")?.as_str().map(str::to_owned)
}

pub fn embedded_version() -> Option<String> {
    EMBEDDED.iter().find(|(name, _)| *name == "manifest.json").and_then(|(_, bytes)| manifest_version(bytes))
}

pub fn installed_version() -> Option<String> {
    manifest_version(&fs::read(folder().join("manifest.json")).ok()?)
}

fn write_files(files: &[(String, Vec<u8>)]) -> Result<(), String> {
    let root = folder();
    for (relative, bytes) in files {
        if relative.split('/').any(|part| part.is_empty() || part == ".." || part.contains(':') || part.contains('\\')) {
            return Err(format!("Refusing the unsafe file path '{relative}'."));
        }
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
        }
        // Written in place (never delete-and-recreate): Chrome may have the folder loaded.
        fs::write(&target, bytes).map_err(|error| format!("Could not write {}: {error}", target.display()))?;
    }
    Ok(())
}

/// The extension files from the repository's `chrome-extension` folder on GitHub.
fn download() -> Result<Vec<(String, Vec<u8>)>, String> {
    let files = crate::skills::fetch_folder(crate::installer::REPO, "chrome-extension")?;
    if !files.iter().any(|(name, _)| name == "manifest.json") {
        return Err("The repository has no chrome-extension/manifest.json.".into());
    }
    // Only what Chrome needs: no tests or docs.
    Ok(files.into_iter().filter(|(name, _)| !name.ends_with(".test.cjs") && !name.ends_with(".md") && !name.ends_with(".template.json")).collect())
}

fn embedded() -> Vec<(String, Vec<u8>)> {
    EMBEDDED.iter().map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec())).collect()
}

/// Put the extension files in `folder()`; returns where they came from.
pub fn install_files() -> Result<&'static str, String> {
    let (files, source) = match download() {
        Ok(files) => (files, "GitHub"),
        Err(_) => (embedded(), "the app"),
    };
    write_files(&files)?;
    Ok(source)
}

/// Keep an already installed copy current after an app update (silently; Chrome picks the new
/// files up on its next extension reload).
pub fn refresh_if_stale() {
    if let (Some(have), Some(built_in)) = (installed_version(), embedded_version()) {
        if crate::installer::is_newer(&built_in, &have) {
            let _ = write_files(&embedded());
        }
    }
}

fn chrome_user_data() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    return env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join(r"Google\Chrome\User Data"));
    #[cfg(target_os = "macos")]
    return env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support/Google/Chrome"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    return env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/google-chrome"));
}

/// What a Chrome profile says about the extension.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub profile: String,
    pub path: Option<String>,
    pub enabled: bool,
}

fn entry_in(preferences: &Value, profile: &str) -> Option<Entry> {
    let record = preferences.get("extensions")?.get("settings")?.get(browser::BUNDLED_EXTENSION_ID)?;
    let state = record.get("state").and_then(Value::as_i64).unwrap_or(1);
    let disabled = record.get("disable_reasons").and_then(Value::as_array).is_some_and(|reasons| !reasons.is_empty())
        || record.get("disable_reasons").and_then(Value::as_i64).is_some_and(|bits| bits != 0);
    Some(Entry {
        profile: profile.to_owned(),
        path: record.get("path").and_then(Value::as_str).map(str::to_owned),
        enabled: state == 1 && !disabled,
    })
}

/// The extension as Chrome has it recorded in any profile, if at all.
pub fn chrome_entry() -> Option<Entry> {
    let root = chrome_user_data()?;
    for profile in fs::read_dir(&root).ok()?.flatten() {
        let name = profile.file_name().to_string_lossy().into_owned();
        if name != "Default" && !name.starts_with("Profile ") {
            continue;
        }
        for file in ["Secure Preferences", "Preferences"] {
            if let Some(preferences) = fs::read(profile.path().join(file)).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
                if let Some(found) = entry_in(&preferences, &name) {
                    return Some(found);
                }
            }
        }
    }
    None
}

pub fn chrome_executable() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let candidates = [("PROGRAMFILES", r"Google\Chrome\Application\chrome.exe"), ("PROGRAMFILES(X86)", r"Google\Chrome\Application\chrome.exe"), ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe")];
        candidates.iter().filter_map(|(var, rest)| env::var_os(var).map(|base| PathBuf::from(base).join(rest))).find(|path| path.is_file())
    }
    #[cfg(target_os = "macos")]
    {
        let app = PathBuf::from("/Applications/Google Chrome.app");
        app.is_dir().then_some(app)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable"].iter().map(PathBuf::from).find(|path| path.is_file())
    }
}

/// Where things stand, for the settings page.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub files_ready: bool,
    pub version: Option<String>,
    pub chrome_found: bool,
    pub entry: Option<Entry>,
    pub connected: bool,
}

impl Status {
    /// Chrome has the extension but from a different folder than ours (for example the old copy in
    /// a build folder), or from a folder that no longer exists.
    pub fn points_elsewhere(&self) -> bool {
        match self.entry.as_ref().and_then(|entry| entry.path.as_deref()) {
            Some(path) => !same_folder(Path::new(path), &folder()),
            None => false,
        }
    }

    pub fn in_chrome(&self) -> bool {
        self.entry.is_some()
    }

    pub fn working(&self) -> bool {
        self.connected && self.in_chrome()
    }
}

fn same_folder(a: &Path, b: &Path) -> bool {
    let normal = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normal(a) == normal(b)
}

pub fn check() -> Status {
    Status {
        files_ready: folder().join("manifest.json").is_file(),
        version: installed_version(),
        chrome_found: chrome_executable().is_some(),
        entry: chrome_entry(),
        connected: browser::extension_alive(),
    }
}

/// The profile Chrome should open: the one that already has the extension, else the one used last.
/// Naming it keeps Chrome from stopping at its "Who's using Chrome?" picker when a URL is passed.
fn profile_to_open() -> Option<String> {
    if let Some(entry) = chrome_entry() {
        return Some(entry.profile);
    }
    let state: Value = serde_json::from_slice(&fs::read(chrome_user_data()?.join("Local State")).ok()?).ok()?;
    last_used_profile(&state)
}

/// `profile.last_used` from Chrome's `Local State`, when that profile still exists.
fn last_used_profile(local_state: &Value) -> Option<String> {
    let profile = local_state.get("profile")?;
    let last = profile.get("last_used").and_then(Value::as_str)?;
    let known = profile.get("info_cache").and_then(Value::as_object).is_none_or(|cache| cache.contains_key(last));
    (known && !last.is_empty() && !last.contains(['/', '\\', '"'])).then(|| last.to_owned())
}

/// Arguments that open `url` in a named profile (no profile picker).
fn open_args(url: &str, profile: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(profile) = profile {
        args.push(format!("--profile-directory={profile}"));
    }
    args.push(url.to_owned());
    args
}

/// Open a page in Chrome (in the right profile) from the default install location.
fn open_in_chrome(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").args(["-a", "Google Chrome"]).arg(url).spawn();
    #[cfg(not(target_os = "macos"))]
    let result = match chrome_executable() {
        Some(chrome) => std::process::Command::new(chrome).args(open_args(url, profile_to_open().as_deref())).spawn(),
        None => return Err("Google Chrome was not found in its default location. Install Chrome first.".into()),
    };
    result.map(|_| ()).map_err(|error| format!("Could not open Chrome: {error}"))
}

/// Open `chrome://extensions` in Chrome.
pub fn open_extensions_page() -> Result<(), String> {
    open_in_chrome("chrome://extensions")
}

/// Where the extension's source lives on GitHub.
pub fn github_page() -> String {
    format!("https://github.com/{}/tree/main/chrome-extension", crate::installer::REPO)
}

/// Open the extension's GitHub page in the default browser.
pub fn open_github_page() -> Result<(), String> {
    webbrowser::open(&github_page()).map_err(|error| format!("Could not open {}: {error}", github_page()))
}

/// The whole one-click install: files, native host, then Chrome's extensions page. The caller puts
/// `folder()` on the clipboard.
pub fn install() -> Result<String, String> {
    let source = install_files()?;
    browser::ensure_native_host_registered().map_err(|error| error.to_string())?;
    let opened = open_extensions_page();
    Ok(match opened {
        Ok(()) => format!("Extension downloaded from {source}."),
        Err(error) => format!("Extension downloaded from {source}. {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_embedded_extension_is_complete() {
        assert!(embedded_version().is_some());
        for needed in ["manifest.json", "background.js", "content.js"] {
            assert!(EMBEDDED.iter().any(|(name, bytes)| *name == needed && !bytes.is_empty()), "{needed}");
        }
    }

    /// Network check: the extension really can be fetched from the repository on GitHub.
    #[test]
    #[ignore = "network"]
    fn live_the_extension_downloads_from_github() {
        let files = download().expect("download");
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        println!("{names:?}");
        assert!(names.contains(&"manifest.json") && names.contains(&"background.js"));
        assert!(!names.iter().any(|name| name.ends_with(".md") || name.ends_with(".test.cjs")));
    }

    #[test]
    fn chrome_preferences_are_read_for_the_extension() {
        let present = json!({"extensions": {"settings": {browser::BUNDLED_EXTENSION_ID: {"state": 1, "path": "C:\\Users\\me\\.rhea\\chrome-extension"}}}});
        let found = entry_in(&present, "Default").unwrap();
        assert!(found.enabled);
        assert_eq!(found.path.as_deref(), Some("C:\\Users\\me\\.rhea\\chrome-extension"));
        let disabled = json!({"extensions": {"settings": {browser::BUNDLED_EXTENSION_ID: {"state": 0}}}});
        assert!(!entry_in(&disabled, "Default").unwrap().enabled);
        assert!(entry_in(&json!({"extensions": {"settings": {}}}), "Default").is_none());
    }

    #[test]
    fn chrome_is_opened_in_a_named_profile_so_no_picker_appears() {
        assert_eq!(open_args("chrome://extensions", Some("Profile 2")), ["--profile-directory=Profile 2", "chrome://extensions"]);
        assert_eq!(open_args("chrome://extensions", None), ["chrome://extensions"]);
        let state = json!({"profile": {"last_used": "Profile 1", "info_cache": {"Default": {}, "Profile 1": {}}}});
        assert_eq!(last_used_profile(&state).as_deref(), Some("Profile 1"));
        let gone = json!({"profile": {"last_used": "Profile 9", "info_cache": {"Default": {}}}});
        assert_eq!(last_used_profile(&gone), None, "a profile that no longer exists is not used");
        assert_eq!(last_used_profile(&json!({"profile": {"last_used": "../evil"}})), None);
        assert_eq!(last_used_profile(&json!({})), None);
    }

    #[test]
    fn the_github_page_is_the_extension_folder_of_the_repo() {
        assert_eq!(github_page(), "https://github.com/taisrisk/rhea/tree/main/chrome-extension");
    }

    #[test]
    fn a_chrome_entry_from_another_folder_is_reported_as_elsewhere() {
        let status = Status { entry: Some(Entry { profile: "Default".into(), path: Some("Z:/old/build/chrome-extension".into()), enabled: true }), ..Default::default() };
        assert!(status.points_elsewhere());
        let ours = Status { entry: Some(Entry { profile: "Default".into(), path: Some(folder().display().to_string()), enabled: true }), ..Default::default() };
        assert!(!ours.points_elsewhere());
    }
}
