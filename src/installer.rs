//! Installing and updating Rhea itself.
//!
//! On Windows the downloaded `Rhea-Setup-vX.Y.Z-windows-x64.exe` is its own installer: started from
//! anywhere except the install folder it copies itself to `%LOCALAPPDATA%\Programs\Rhea`, replaces
//! the old copy (stopping it first), adds a Start-menu shortcut and an uninstall entry, then
//! starts the installed copy and exits. Chats, settings and credentials stay in `~/.rhea` and
//! the OS credential store, so a reinstall or update never touches them.
//!
//! Updates reuse the same path: the app downloads the newest release's exe from GitHub and runs
//! it, and that run replaces the installed copy.

use std::{env, fs, path::Path, time::Duration};

use serde_json::Value;

/// GitHub repository that publishes Rhea's releases.
pub const REPO: &str = "taisrisk/rhea";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub page: String,
    /// Direct download of this platform's installer, when the release has one.
    pub asset: Option<String>,
}

/// `1.2.3` or `v1.2.3` as comparable numbers.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.').map(|part| part.split(['-', '+']).next().unwrap_or("").parse::<u64>().ok());
    Some((parts.next()??, parts.next().unwrap_or(Some(0))?, parts.next().unwrap_or(Some(0))?))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

fn platform_asset_marker() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows-x64.exe"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-arm64.dmg"
    } else if cfg!(target_os = "macos") {
        "macos-x64.dmg"
    } else {
        "linux"
    }
}

fn pick_release(body: &Value) -> Option<Release> {
    let version = body.get("tag_name")?.as_str()?.trim_start_matches('v').to_owned();
    let marker = platform_asset_marker();
    let asset = body.get("assets").and_then(Value::as_array).and_then(|assets| {
        assets.iter().find_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            name.to_ascii_lowercase().ends_with(marker).then(|| asset.get("browser_download_url")?.as_str().map(str::to_owned))?
        })
    });
    Some(Release { version, page: body.get("html_url").and_then(Value::as_str).unwrap_or("").to_owned(), asset })
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(format!("Rhea/{VERSION}"))
        .build()
        .map_err(|error| error.to_string())
}

/// The newest published release when it is newer than this build.
pub fn check_for_update() -> Result<Option<Release>, String> {
    let response = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|error| format!("Could not reach GitHub: {error}"))?;
    if response.status().as_u16() == 404 {
        return Ok(None); // nothing published yet
    }
    let body: Value = response.error_for_status().map_err(|error| error.to_string())?.json().map_err(|error| error.to_string())?;
    Ok(pick_release(&body).filter(|release| is_newer(&release.version, VERSION)))
}

/// Download the release's installer and run it; the new process replaces this install, so the
/// caller should exit right after this returns `Ok`.
pub fn download_and_run(release: &Release) -> Result<(), String> {
    let url = release.asset.as_deref().ok_or("This release has no installer for this computer yet.")?;
    let bytes = client()?
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
        .map_err(|error| format!("Download failed: {error}"))?;
    let file_name = url.rsplit('/').next().unwrap_or("Rhea-update.exe");
    let folder = env::temp_dir().join(format!("rhea-update-{}", release.version));
    fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let path = folder.join(file_name);
    fs::write(&path, &bytes).map_err(|error| error.to_string())?;
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new(&path).spawn().map_err(|error| format!("Could not start the installer: {error}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        // A disk image is opened for the user to drag into Applications.
        std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
            .arg(&path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open the download: {error}"))
    }
}

/// What the program should do after the install step.
#[derive(Debug, PartialEq)]
pub enum Startup {
    /// Carry on and open the window.
    Run,
    /// The install step finished (or was an uninstall); exit now.
    Exit,
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use std::{os::windows::process::CommandExt, path::PathBuf, process::Command};

    const NO_WINDOW: u32 = 0x0800_0000;

    pub fn install_dir() -> Option<PathBuf> {
        env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Programs").join("Rhea"))
    }

    pub fn installed_exe() -> Option<PathBuf> {
        install_dir().map(|dir| dir.join("Rhea.exe"))
    }

    fn quiet(program: &str) -> Command {
        let mut command = Command::new(program);
        command.creation_flags(NO_WINDOW);
        command
    }

    /// A PowerShell single-quoted string: an apostrophe in a path (a user called O'Brien) is doubled.
    pub fn ps_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "''"))
    }

    /// The PowerShell that writes the Start-menu shortcut.
    pub fn shortcut_script(link: &Path, target: &Path, dir: &Path) -> String {
        format!(
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut({});$s.TargetPath={};$s.WorkingDirectory={};$s.IconLocation={};$s.Description='Rhea';$s.Save()",
            ps_quote(&link.display().to_string()),
            ps_quote(&target.display().to_string()),
            ps_quote(&dir.display().to_string()),
            ps_quote(&format!("{},0", target.display()))
        )
    }

    fn start_menu_shortcut() -> Option<PathBuf> {
        env::var_os("APPDATA").map(|base| PathBuf::from(base).join(r"Microsoft\Windows\Start Menu\Programs\Rhea.lnk"))
    }

    /// Running straight out of a cargo build folder is development, not an install.
    pub fn is_development_build(exe: &Path) -> bool {
        exe.components().any(|part| part.as_os_str().to_string_lossy().starts_with("target"))
    }

    pub fn install(exe: &Path) -> Result<(), String> {
        let dir = install_dir().ok_or("No LOCALAPPDATA folder.")?;
        let target = dir.join("Rhea.exe");
        fs::create_dir_all(&dir).map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
        // Stop a running installed copy (never ourselves), then move it aside: a running exe can
        // be renamed but not overwritten.
        let _ = quiet("taskkill").args(["/F", "/IM", "Rhea.exe", "/FI", &format!("PID ne {}", std::process::id())]).status();
        std::thread::sleep(Duration::from_millis(600));
        if target.exists() {
            let aside = dir.join(format!("Rhea.exe.old-{}", std::process::id()));
            if fs::rename(&target, &aside).is_err() {
                fs::remove_file(&target).map_err(|error| format!("Could not replace the installed Rhea (is it still open?): {error}"))?;
            }
        }
        fs::copy(exe, &target).map_err(|error| format!("Could not copy Rhea into {}: {error}", dir.display()))?;
        remove_old_copies(&dir);

        // The shortcut in the user's Start-menu Programs folder is what makes Rhea show up when
        // searching from the Start menu.
        if let Some(link) = start_menu_shortcut() {
            if let Some(folder) = link.parent() {
                let _ = fs::create_dir_all(folder);
            }
            let script = shortcut_script(&link, &target, &dir);
            let _ = quiet("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status();
            if !link.is_file() {
                eprintln!("Rhea could not create its Start-menu shortcut at {}.", link.display());
            }
        }
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Rhea";
        let uninstall = format!("\"{}\" --uninstall", target.display());
        for (name, kind, value) in [
            ("DisplayName", "REG_SZ", "Rhea".to_owned()),
            ("DisplayVersion", "REG_SZ", VERSION.to_owned()),
            ("Publisher", "REG_SZ", "Rhea".to_owned()),
            ("InstallLocation", "REG_SZ", dir.display().to_string()),
            ("DisplayIcon", "REG_SZ", target.display().to_string()),
            ("UninstallString", "REG_SZ", uninstall),
        ] {
            let _ = quiet("reg").args(["add", key, "/v", name, "/t", kind, "/d", &value, "/f"]).status();
        }
        Ok(())
    }

    pub fn remove_old_copies(dir: &Path) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("Rhea.exe.old") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    pub fn uninstall() {
        // An installed copy that is still open holds its exe: close it (never ourselves) first.
        let _ = quiet("taskkill").args(["/F", "/IM", "Rhea.exe", "/FI", &format!("PID ne {}", std::process::id())]).status();
        std::thread::sleep(Duration::from_millis(600));
        if let Some(link) = start_menu_shortcut() {
            let _ = fs::remove_file(link);
        }
        let _ = quiet("reg").args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Rhea", "/f"]).status();
        if let Some(dir) = install_dir() {
            // The folder still holds this running exe: remove it a moment after we exit.
            let command = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
            // raw_arg: cmd.exe parses its own quoting, Rust's escaping would break it.
            let _ = quiet("cmd").arg("/C").raw_arg(command).spawn();
        }
    }
}

/// Run before the window opens. Never blocks a normal start: if installing fails the app simply
/// runs from where it is.
pub fn run_install_step() -> Startup {
    #[cfg(target_os = "windows")]
    {
        let arguments: Vec<String> = env::args().skip(1).collect();
        if arguments.iter().any(|argument| argument == "--uninstall") {
            windows::uninstall();
            return Startup::Exit;
        }
        let Ok(exe) = env::current_exe() else { return Startup::Run };
        if env::var_os("RHEA_NO_INSTALL").is_some() || arguments.iter().any(|argument| argument == "--no-install") || windows::is_development_build(&exe) {
            return Startup::Run;
        }
        let Some(installed) = windows::installed_exe() else { return Startup::Run };
        if same_file(&exe, &installed) {
            if let Some(dir) = installed.parent() {
                windows::remove_old_copies(dir);
            }
            return Startup::Run;
        }
        match windows::install(&exe) {
            Ok(()) => match std::process::Command::new(&installed).spawn() {
                Ok(_) => Startup::Exit,
                Err(_) => Startup::Run,
            },
            Err(error) => {
                eprintln!("Rhea install skipped: {error}");
                Startup::Run
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        Startup::Run
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("not-a-version", "0.1.0"));
    }

    #[test]
    fn the_release_asset_for_this_platform_is_picked() {
        let marker = platform_asset_marker();
        let body = json!({
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/x/y/releases/tag/v0.2.0",
            "assets": [
                {"name": "Rhea-v0.2.0-other.zip", "browser_download_url": "https://example.com/other.zip"},
                {"name": format!("Rhea-v0.2.0-{marker}"), "browser_download_url": "https://example.com/mine"}
            ]
        });
        let release = pick_release(&body).unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.asset.as_deref(), Some("https://example.com/mine"));
        assert_eq!(pick_release(&json!({"tag_name": "v0.2.0", "assets": []})).unwrap().asset, None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_shortcut_script_survives_apostrophes_in_paths() {
        assert_eq!(windows::ps_quote("C:\\Users\\O'Brien"), "'C:\\Users\\O''Brien'");
        let script = windows::shortcut_script(Path::new("C:\\Users\\O'Brien\\Rhea.lnk"), Path::new("C:\\Users\\O'Brien\\Rhea.exe"), Path::new("C:\\Users\\O'Brien"));
        assert!(script.contains("TargetPath='C:\\Users\\O''Brien\\Rhea.exe'") && script.contains("IconLocation='C:\\Users\\O''Brien\\Rhea.exe,0'"), "{script}");
        assert!(!script.contains("O'B"), "no unescaped apostrophe can end the string early: {script}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cargo_build_folders_are_development_not_installs() {
        assert!(windows::is_development_build(Path::new(r"C:\code\rhea\target\release\rhea.exe")));
        assert!(windows::is_development_build(Path::new(r"D:\build\target-x\debug\rhea.exe")));
        assert!(!windows::is_development_build(Path::new(r"C:\Users\me\Downloads\Rhea-v0.2.0-windows-x64.exe")));
    }
}
