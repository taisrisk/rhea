//! Skills: folders with a `SKILL.md` (YAML front matter `name` + `description`, then
//! instructions) that Rhea loads on demand. They behave like MCP servers: installed once, listed
//! to the model every run as a short catalog, loaded in full with `use_skill`, or forced with
//! `/skill-name` in the chat box. `install_skill` fetches one from GitHub.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_ARCHIVE_BYTES: u64 = 120 * 1024 * 1024;
const MAX_FILES: usize = 4_000;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 100 * 1024 * 1024;
const MAX_DEPTH: usize = 5;
const MAX_BODY_CHARS: usize = 80_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    /// Where it lives: "core" (built into the app), "rhea", "agents" or "claude".
    pub source: &'static str,
}

/// Skills shipped inside the app: always installed, always listed, never removable. Their text is
/// also part of the base system prompt, so they stay active without being loaded.
const CORE: [(&str, &str, &str); 2] = [
    (
        "systematic-debugging",
        "Find the root cause before fixing anything: reproduce, gather evidence, test one hypothesis at a time, then fix and verify. Built in and always active.",
        include_str!("skills/systematic_debugging.md"),
    ),
    (
        "brainstorming",
        "Turn an idea into a design before building: explore the context, ask one question at a time, compare approaches, get approval. Built in and always active.",
        include_str!("skills/brainstorming.md"),
    ),
];

fn core_skills() -> Vec<Skill> {
    CORE.iter().map(|(name, description, _)| Skill { name: (*name).into(), description: (*description).into(), dir: PathBuf::new(), source: "core" }).collect()
}

/// Compare names the way people type them: `systematic_debugging` finds `systematic-debugging`.
fn same_name(a: &str, b: &str) -> bool {
    a.replace('_', "-") == b.replace('_', "-")
}

fn home() -> PathBuf {
    env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(env::temp_dir)
}

/// The folder Rhea installs skills into.
pub fn install_root() -> PathBuf {
    home().join(crate::chat_store::DATA_DIR).join("skills")
}

/// Skill folders searched, first match wins: Rhea's own, then the shared agent folders.
pub fn roots() -> Vec<(PathBuf, &'static str)> {
    let home = home();
    vec![(install_root(), "rhea"), (home.join(".agents").join("skills"), "agents"), (home.join(".claude").join("skills"), "claude")]
}

/// Split `---` front matter from the body. Handles quoted values and `>` / `|` block scalars.
pub fn parse_skill(text: &str) -> (Option<String>, Option<String>, String) {
    let text = text.trim_start_matches('\u{feff}');
    let mut segments = text.split_inclusive('\n');
    let Some(first) = segments.next() else { return (None, None, String::new()) };
    if first.trim() != "---" {
        return (None, None, text.to_owned());
    }
    let mut consumed = first.len();
    let mut front: Vec<&str> = Vec::new();
    let mut closed = false;
    for segment in segments {
        consumed += segment.len();
        if segment.trim() == "---" {
            closed = true;
            break;
        }
        front.push(segment.trim_end_matches(['\r', '\n']));
    }
    if !closed {
        return (None, None, text.to_owned());
    }
    let body = text[consumed..].trim_start_matches(['\r', '\n']).to_owned();
    let field = |key: &str| -> Option<String> {
        let index = front.iter().position(|line| line.starts_with(&format!("{key}:")))?;
        let raw = front[index][key.len() + 1..].trim();
        if raw.is_empty() || raw == ">" || raw == "|" || raw == ">-" || raw == "|-" {
            let folded: Vec<&str> = front[index + 1..].iter().take_while(|l| l.starts_with(' ') || l.starts_with('\t')).map(|l| l.trim()).collect();
            let joined = folded.join(" ");
            return (!joined.is_empty()).then_some(joined);
        }
        Some(raw.trim_matches(|c| c == '"' || c == '\'').to_owned())
    };
    (field("name"), field("description"), body)
}

/// Lower-case, filesystem-safe skill name.
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    cleaned.trim_matches('-').chars().take(64).collect()
}

fn read_skill(dir: &Path, source: &'static str) -> Option<Skill> {
    let text = fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (name, description, _) = parse_skill(&text);
    let folder = dir.file_name()?.to_string_lossy().to_string();
    let name = sanitize_name(&name.unwrap_or(folder));
    (!name.is_empty()).then(|| Skill { name, description: description.unwrap_or_default(), dir: dir.to_path_buf(), source })
}

/// A fingerprint of everything in a skill folder (file names and contents), so the same skill
/// found in two folders can be told apart from two different skills that share a name.
fn fingerprint(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut files = Vec::new();
    collect_files(dir, dir, 0, &mut files);
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for relative in files.iter().take(400) {
        relative.hash(&mut hasher);
        fs::read(dir.join(relative)).unwrap_or_default().hash(&mut hasher);
    }
    hasher.finish()
}

/// Skills from every folder, the first folder winning. A copy in a later folder (for example
/// `~/.agents/skills`) is hidden only when it is the very same skill: same name and same files.
/// A different skill that happens to share a name stays visible under `<name>-<folder>`.
pub fn list_in(roots: &[(PathBuf, &'static str)]) -> Vec<Skill> {
    let mut seen: std::collections::HashMap<String, Vec<u64>> = std::collections::HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    let mut skills = Vec::new();
    for (root, source) in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let Some(mut skill) = read_skill(&dir, source) else { continue };
            let print = fingerprint(&dir);
            match seen.get_mut(&skill.name) {
                None => {
                    seen.insert(skill.name.clone(), vec![print]);
                }
                Some(prints) if prints.contains(&print) => continue,
                Some(prints) => {
                    prints.push(print);
                    let mut renamed = format!("{}-{source}", skill.name);
                    let mut counter = 2;
                    while taken.contains(&renamed) {
                        renamed = format!("{}-{source}-{counter}", skill.name);
                        counter += 1;
                    }
                    skill.name = renamed;
                }
            }
            taken.insert(skill.name.clone());
            skills.push(skill);
        }
    }
    skills
}

/// Installed skills, re-read from disk at most every couple of seconds (the chat box asks every frame).
pub fn list() -> Vec<Skill> {
    static CACHE: Mutex<Option<(Instant, Vec<Skill>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, skills)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(2) {
            return skills.clone();
        }
    }
    let mut skills = core_skills();
    skills.extend(list_in(&roots()).into_iter().filter(|skill| !skills_has(&core_skills(), &skill.name)));
    *cache = Some((Instant::now(), skills.clone()));
    skills
}

fn skills_has(skills: &[Skill], name: &str) -> bool {
    skills.iter().any(|skill| same_name(&skill.name, name))
}

pub fn find(name: &str) -> Option<Skill> {
    let mut key = sanitize_name(name.trim_start_matches('/'));
    if key == "taste-skill" || key == "taste" {
        // The repository is called taste-skill; the skill inside calls itself design-taste-frontend.
        key = "design-taste-frontend".into();
    }
    list().into_iter().find(|skill| same_name(&skill.name, &key))
}

/// A skill's full text for the model: instructions plus the files that sit next to it.
pub fn load(name: &str) -> Result<Value, String> {
    let loaded = load_skill(name);
    if loaded.is_ok() {
        crate::usage::record_skill(name);
    }
    loaded
}

fn load_skill(name: &str) -> Result<Value, String> {
    let skill = find(name).ok_or_else(|| {
        let names: Vec<String> = list().into_iter().map(|s| s.name).collect();
        format!("No skill named '{name}'. Installed skills: {}.", if names.is_empty() { "none".into() } else { names.join(", ") })
    })?;
    let mut files = Vec::new();
    let body = if skill.source == "core" {
        CORE.iter().find(|(name, _, _)| *name == skill.name).map(|(_, _, text)| (*text).to_owned()).unwrap_or_default()
    } else {
        let text = fs::read_to_string(skill.dir.join("SKILL.md")).map_err(|e| format!("Could not read the skill: {e}"))?;
        collect_files(&skill.dir, &skill.dir, 0, &mut files);
        files.retain(|f| f != "SKILL.md");
        files.truncate(60);
        parse_skill(&text).2
    };
    Ok(json!({
        "name": skill.name,
        "description": skill.description,
        "directory": if skill.source == "core" { "built in".to_owned() } else { skill.dir.display().to_string() },
        "instructions": body.chars().take(MAX_BODY_CHARS).collect::<String>(),
        "files": files,
        "note": "Follow these instructions for the current task. Relative paths are inside `directory`; read them with read_file when the instructions point to them."
    }))
}

fn collect_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files(root, &path, depth + 1, out);
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// The short catalog appended to every run so the model knows what it can load.
pub fn catalog_prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut text = String::from("## Installed skills\nSkills are reusable instructions. When a request matches one, call `use_skill {name}` BEFORE starting and follow it. The user can also force one by typing `/skill-name`. To add more, call `install_skill` with a GitHub link.\n");
    for skill in skills.iter().take(60) {
        let description: String = skill.description.chars().take(220).collect();
        text.push_str(&format!("- `{}` — {}\n", skill.name, if description.is_empty() { "(no description)" } else { &description }));
    }
    Some(text)
}

/// `/name rest of the message` -> the installed skill and the user's remaining text.
pub fn match_slash(prompt: &str) -> Option<(Skill, String)> {
    let trimmed = prompt.trim_start();
    let command = trimmed.strip_prefix('/')?;
    let (token, rest) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    find(token).map(|skill| (skill, rest.trim().to_owned()))
}

pub fn remove(name: &str) -> Result<(), String> {
    let key = sanitize_name(name);
    let dir = install_root().join(&key);
    if key.is_empty() || !dir.join("SKILL.md").is_file() {
        return Err(format!("'{name}' is not a skill installed by Rhea."));
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("Could not remove the skill: {e}"))?;
    crate::hooks::remove_skill_hooks(&key);
    Ok(())
}

// ---- installing from GitHub ------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct GithubSource {
    pub owner: String,
    pub repo: String,
    pub git_ref: Option<String>,
    /// Directory inside the repo ("" = the root). A link to `SKILL.md` points at its folder.
    pub dir: String,
}

pub fn parse_github(source: &str) -> Result<GithubSource, String> {
    let source = source.trim().trim_end_matches('/');
    let rest = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("http://github.com/"))
        .or_else(|| source.strip_prefix("https://raw.githubusercontent.com/"))
        .or_else(|| source.strip_prefix("github.com/"))
        .unwrap_or(source);
    let raw = source.contains("raw.githubusercontent.com");
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 || parts.iter().any(|p| *p == ".." || p.contains(':')) {
        return Err("Give a GitHub link like https://github.com/owner/repo/tree/main/skills/name (or owner/repo).".into());
    }
    let (owner, repo) = (parts[0].to_owned(), parts[1].trim_end_matches(".git").to_owned());
    let (git_ref, path_parts): (Option<String>, &[&str]) = if raw {
        (parts.get(2).map(|s| (*s).to_owned()), parts.get(3..).unwrap_or(&[]))
    } else if matches!(parts.get(2), Some(&"tree") | Some(&"blob")) {
        (parts.get(3).map(|s| (*s).to_owned()), parts.get(4..).unwrap_or(&[]))
    } else {
        (None, parts.get(2..).unwrap_or(&[]))
    };
    let mut path: Vec<&str> = path_parts.to_vec();
    if path.last().is_some_and(|last| last.eq_ignore_ascii_case("SKILL.md")) {
        path.pop();
    }
    Ok(GithubSource { owner, repo, git_ref, dir: path.join("/") })
}

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .user_agent(format!("Rhea/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

/// The whole repository as one gzip tarball: a single request (no per-file API rate limits).
fn fetch_archive(source: &GithubSource) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let git_ref = source.git_ref.as_deref().unwrap_or("HEAD");
    let url = format!("https://codeload.github.com/{}/{}/tar.gz/{git_ref}", source.owner, source.repo);
    let response = http()?.get(&url).send().map_err(|e| format!("GitHub request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub returned HTTP {} for {url}. Check the link (the repository must be public).", response.status()));
    }
    let mut bytes = Vec::new();
    response.take(MAX_ARCHIVE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| format!("Could not download the repository: {e}"))?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err("The repository archive is larger than 120 MB; link a smaller repository.".into());
    }
    Ok(bytes)
}

/// Path of a tar entry without its leading `repo-ref/` folder, using `/`.
fn entry_relative(path: &Path) -> Option<String> {
    let parts: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    (parts.len() > 1).then(|| parts[1..].join("/"))
}

fn open_archive(bytes: &[u8]) -> tar::Archive<flate2::read::GzDecoder<&[u8]>> {
    tar::Archive::new(flate2::read::GzDecoder::new(bytes))
}

/// Every folder in the archive that holds a `SKILL.md` ("" = the repo root).
fn skill_dirs(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut dirs = Vec::new();
    for entry in open_archive(bytes).entries().map_err(|e| format!("The archive is unreadable: {e}"))? {
        let entry = entry.map_err(|e| format!("The archive is unreadable: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Ok(path) = entry.path() else { continue };
        let Some(relative) = entry_relative(&path) else { continue };
        if relative == "SKILL.md" {
            dirs.push(String::new());
        } else if let Some(dir) = relative.strip_suffix("/SKILL.md") {
            dirs.push(dir.to_owned());
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

struct Downloaded {
    files: Vec<(String, Vec<u8>)>,
    total: usize,
}

/// The regular files under `dir` (relative to it), within the size limits.
fn extract_dir(bytes: &[u8], dir: &str) -> Result<Downloaded, String> {
    use std::io::Read;
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut out = Downloaded { files: Vec::new(), total: 0 };
    for entry in open_archive(bytes).entries().map_err(|e| format!("The archive is unreadable: {e}"))? {
        let mut entry = entry.map_err(|e| format!("The archive is unreadable: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let relative = match entry.path().ok().and_then(|p| entry_relative(&p)) {
            Some(relative) => relative,
            None => continue,
        };
        let Some(inside) = relative.strip_prefix(&prefix) else { continue };
        if inside.is_empty() || inside.split('/').any(|part| part == ".git" || part == "node_modules") {
            continue;
        }
        if out.files.len() >= MAX_FILES {
            return Err(format!("This skill has more than {MAX_FILES} files; link a smaller folder."));
        }
        let size = entry.header().size().unwrap_or(0) as usize;
        if size > MAX_FILE_BYTES {
            continue;
        }
        let mut data = Vec::with_capacity(size);
        entry.read_to_end(&mut data).map_err(|e| format!("Could not read {inside}: {e}"))?;
        out.total += data.len();
        if out.total > MAX_TOTAL_BYTES {
            return Err("This skill is larger than 100 MB; link a smaller folder.".into());
        }
        out.files.push((inside.to_owned(), data));
    }
    Ok(out)
}

/// The files of one folder of a public GitHub repository (default branch), relative to it.
pub fn fetch_folder(repo: &str, dir: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let source = parse_github(repo)?;
    let archive = fetch_archive(&source)?;
    Ok(extract_dir(&archive, dir)?.files)
}

/// Write downloaded files as `<root>/<name>/`, replacing an older copy only once everything arrived.
fn write_skill(root: &Path, name: &str, files: &[(String, Vec<u8>)]) -> Result<PathBuf, String> {
    let staging = root.join(format!(".installing-{name}"));
    let _ = fs::remove_dir_all(&staging);
    for (relative, bytes) in files {
        if relative.split('/').any(|part| part.is_empty() || part == ".." || part.contains(':') || part.contains('\\')) {
            return Err(format!("Refusing the unsafe file path '{relative}'."));
        }
        let target = staging.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Could not create the skill folder: {e}"))?;
        }
        fs::write(&target, bytes).map_err(|e| format!("Could not write {relative}: {e}"))?;
    }
    let destination = root.join(name);
    let _ = fs::remove_dir_all(&destination);
    fs::rename(&staging, &destination).map_err(|e| format!("Could not finish the install: {e}"))?;
    Ok(destination)
}

/// Install a skill from a GitHub link into `~/.rhea/skills`.
pub fn install(source: &str, name_override: Option<&str>) -> Result<Value, String> {
    let github = parse_github(source)?;
    let archive = fetch_archive(&github)?;
    install_from_archive(&github, &archive, name_override, &install_root())
}

fn install_from_archive(github: &GithubSource, archive: &[u8], name_override: Option<&str>, root: &Path) -> Result<Value, String> {
    let dirs = skill_dirs(archive)?;
    let prefix = format!("{}/", github.dir);
    let chosen = if dirs.iter().any(|d| *d == github.dir) {
        github.dir.clone()
    } else {
        let candidates: Vec<String> = dirs.iter().filter(|d| github.dir.is_empty() || d.starts_with(&prefix)).cloned().collect();
        match candidates.as_slice() {
            [only] => only.clone(),
            [] => return Err(format!("No SKILL.md found{}.", if github.dir.is_empty() { " in that repository".to_owned() } else { format!(" under {}", github.dir) })),
            many => {
                return Err(format!(
                    "That link holds {} skills: {}. Install one by linking its folder, e.g. https://github.com/{}/{}/tree/{}/{}",
                    many.len(),
                    many.iter().take(40).cloned().collect::<Vec<_>>().join(", "),
                    github.owner,
                    github.repo,
                    github.git_ref.as_deref().unwrap_or("main"),
                    many[0]
                ))
            }
        }
    };
    let downloaded = extract_dir(archive, &chosen)?;
    let skill_md = downloaded.files.iter().find(|(p, _)| p.eq_ignore_ascii_case("SKILL.md")).ok_or("SKILL.md was not found in the archive.")?;
    let (front_name, description, _) = parse_skill(&String::from_utf8_lossy(&skill_md.1));
    let fallback = chosen.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&github.repo).to_owned();
    let name = sanitize_name(name_override.filter(|n| !n.trim().is_empty()).or(front_name.as_deref()).unwrap_or(&fallback));
    if name.is_empty() {
        return Err("Could not work out a name for this skill; pass `name`.".into());
    }
    fs::create_dir_all(root).map_err(|e| format!("Could not create {}: {e}", root.display()))?;
    let destination = write_skill(root, &name, &downloaded.files)?;
    // A skill can ship hooks (Claude Code format): register them so they run without the user
    // having to wire anything. Only done for the real install folder, never for test folders.
    let (hooks, hook_error) = if root == install_root() {
        match crate::hooks::install_skill_hooks(&name, &destination) {
            Ok(lines) => (lines, None),
            Err(error) => (Vec::new(), Some(error)),
        }
    } else {
        (Vec::new(), None)
    };
    let mut note = "Installed. Call use_skill with this name to read it, or the user can type /name.".to_owned();
    if !hooks.is_empty() {
        note.push_str(" Its hooks were registered too (see Settings > Agents, skills & hooks).");
    }
    if let Some(error) = &hook_error {
        note.push_str(&format!(" Its hooks could not be installed: {error}"));
    }
    Ok(json!({
        "installed": name,
        "description": description.unwrap_or_default(),
        "files": downloaded.files.len(),
        "directory": destination.display().to_string(),
        "hooks_installed": hooks,
        "note": note
    }))
}

/// Skills every Rhea install starts with (fetched once, in the background, on first launch).
pub const BUNDLED: [&str; 2] = ["https://github.com/Leonxlnx/taste-skill", "https://github.com/heydamir/unslop"];

fn bundled_marker() -> PathBuf {
    install_root().join(".bundled.json")
}

/// Install the bundled skills that have not been installed yet. A skill the user later removes
/// stays removed (it is remembered in `.bundled.json`); a failed download is retried next launch.
pub fn ensure_bundled() {
    let marker = bundled_marker();
    let mut done: Vec<String> = fs::read_to_string(&marker).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    let mut changed = false;
    for url in BUNDLED {
        if done.iter().any(|d| d == url) {
            continue;
        }
        match install(url, None) {
            Ok(_) => {
                done.push(url.to_owned());
                changed = true;
            }
            Err(_) => continue,
        }
    }
    if changed {
        let _ = fs::create_dir_all(install_root());
        let _ = fs::write(&marker, serde_json::to_string_pretty(&done).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("rhea-skills-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn front_matter_handles_plain_quoted_and_block_values() {
        let (name, description, body) = parse_skill("---\nname: \"Debug-It\"\ndescription: >\n  find root\n  causes first\n---\n# Body\ntext");
        assert_eq!(name.as_deref(), Some("Debug-It"));
        assert_eq!(description.as_deref(), Some("find root causes first"));
        assert_eq!(body, "# Body\ntext");
        let (name, _, body) = parse_skill("no front matter");
        assert!(name.is_none());
        assert_eq!(body, "no front matter");
        assert_eq!(parse_skill("---\nname: x\nno close").2, "---\nname: x\nno close");
    }

    #[test]
    fn the_two_core_skills_are_always_installed_and_found_by_either_spelling() {
        let names: Vec<String> = list().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"systematic-debugging".to_owned()) && names.contains(&"brainstorming".to_owned()));
        for spelling in ["systematic-debugging", "systematic_debugging", "/Systematic_Debugging"] {
            let loaded = load(spelling).unwrap();
            assert_eq!(loaded["name"], "systematic-debugging");
            assert!(loaded["instructions"].as_str().unwrap().contains("root cause"));
            assert_eq!(loaded["directory"], "built in");
        }
        assert!(load("brainstorming").unwrap()["instructions"].as_str().unwrap().len() > 1000);
        assert!(!load("brainstorming").unwrap()["instructions"].as_str().unwrap().contains("Visual Companion"));
        assert!(remove("brainstorming").is_err(), "core skills cannot be removed");
    }

    #[test]
    fn names_are_filesystem_safe() {
        assert_eq!(sanitize_name("  My Skill!! v2 "), "my-skill---v2");
        assert_eq!(sanitize_name("../../etc"), "etc");
        assert_eq!(sanitize_name("///"), "");
    }

    #[test]
    fn listing_prefers_the_first_root_and_skips_folders_without_skill_md() {
        let a = temp("list-a");
        let b = temp("list-b");
        // the very same skill in both folders: only the first is listed
        for root in [&a, &b] {
            let dir = root.join("alpha");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("SKILL.md"), "---\nname: alpha\ndescription: first\n---\nbody").unwrap();
        }
        fs::create_dir_all(b.join("empty")).unwrap();
        fs::create_dir_all(b.join("beta")).unwrap();
        fs::write(b.join("beta").join("SKILL.md"), "plain body without front matter").unwrap();
        let skills = list_in(&[(a.clone(), "rhea"), (b.clone(), "agents")]);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(skills[0].description, "first");
        let catalog = catalog_prompt(&skills).unwrap();
        assert!(catalog.contains("`alpha` — first") && catalog.contains("use_skill"));
        assert!(catalog_prompt(&[]).is_none());
    }

    #[test]
    fn github_links_resolve_to_a_skill_folder() {
        let tree = parse_github("https://github.com/obra/superpowers/tree/main/skills/systematic-debugging").unwrap();
        assert_eq!((tree.owner.as_str(), tree.repo.as_str(), tree.git_ref.as_deref(), tree.dir.as_str()), ("obra", "superpowers", Some("main"), "skills/systematic-debugging"));
        let blob = parse_github("https://github.com/obra/superpowers/blob/main/skills/brainstorming/SKILL.md").unwrap();
        assert_eq!(blob.dir, "skills/brainstorming");
        let raw = parse_github("https://raw.githubusercontent.com/obra/superpowers/main/skills/x/SKILL.md").unwrap();
        assert_eq!((raw.git_ref.as_deref(), raw.dir.as_str()), (Some("main"), "skills/x"));
        let short = parse_github("obra/superpowers").unwrap();
        assert_eq!((short.git_ref, short.dir.as_str()), (None, ""));
        assert!(parse_github("https://github.com/only-owner").is_err());
        assert!(parse_github("https://github.com/a/b/tree/main/../x").is_err());
    }

    #[test]
    #[ignore = "network: installs a real skill from GitHub into a temp HOME"]
    fn live_install_bundled_skills_into_a_temp_home() {
        let home = temp("live-home");
        // SAFETY: single-threaded ignored test; points the install root at a throwaway folder.
        unsafe { env::set_var("USERPROFILE", &home) };
        ensure_bundled();
        let names: Vec<String> = list_in(&[(install_root(), "rhea")]).into_iter().map(|s| s.name).collect();
        println!("installed: {names:?}");
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(home.join(".rhea/skills/.bundled.json").is_file());
        let taste = names.iter().find(|n| n.contains("taste")).unwrap();
        assert!(load(taste).unwrap()["instructions"].as_str().unwrap().len() > 5_000);
        let unslop = load("unslop").unwrap();
        assert!(unslop["files"].as_array().unwrap().len() > 3);
    }

    fn put_skill(root: &Path, folder: &str, text: &str, extra: Option<(&str, &str)>) {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), text).unwrap();
        if let Some((name, body)) = extra {
            fs::write(dir.join(name), body).unwrap();
        }
    }

    #[test]
    fn the_same_skill_in_two_folders_is_listed_once_and_the_own_copy_wins() {
        let own = temp("dedupe-own");
        let shared = temp("dedupe-agents");
        let text = "---\nname: taste\ndescription: d\n---\nbody";
        put_skill(&own, "taste", text, Some(("rules.md", "same")));
        put_skill(&shared, "taste", text, Some(("rules.md", "same")));
        let found = list_in(&[(own.clone(), "rhea"), (shared.clone(), "agents")]);
        assert_eq!(found.len(), 1, "{:?}", found.iter().map(|s| (&s.name, s.source)).collect::<Vec<_>>());
        assert_eq!(found[0].source, "rhea");
    }

    #[test]
    fn a_different_skill_with_the_same_name_stays_visible_under_its_own_name() {
        let own = temp("differ-own");
        let shared = temp("differ-agents");
        put_skill(&own, "taste", "---\nname: taste\n---\nmine", None);
        put_skill(&shared, "taste", "---\nname: taste\n---\nsomeone else's version", None);
        put_skill(&shared, "extra", "---\nname: extra\n---\nonly here", None);
        let names: Vec<(String, &str)> = list_in(&[(own, "rhea"), (shared, "agents")]).into_iter().map(|s| (s.name, s.source)).collect();
        assert_eq!(names, [("taste".to_owned(), "rhea"), ("extra".to_owned(), "agents"), ("taste-agents".to_owned(), "agents")]);
    }

    fn archive_with(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, text) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, format!("repo-main/{path}"), text.as_bytes()).unwrap();
        }
        let tarred = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tarred).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn a_repo_with_one_skill_installs_without_naming_its_folder() {
        let root = temp("archive-one");
        let archive = archive_with(&[
            ("README.md", "readme"),
            ("skills/pretty/SKILL.md", "---\nname: pretty-ui\ndescription: makes it pretty\n---\nbody"),
            ("skills/pretty/references/a.md", "ref"),
            ("skills/pretty/node_modules/x.js", "skip me"),
        ]);
        assert_eq!(skill_dirs(&archive).unwrap(), ["skills/pretty"]);
        let github = parse_github("https://github.com/o/r").unwrap();
        let done = install_from_archive(&github, &archive, None, &root).unwrap();
        assert_eq!(done["installed"], "pretty-ui");
        assert_eq!(done["files"], 2);
        assert!(root.join("pretty-ui/references/a.md").is_file());
        assert!(!root.join("pretty-ui/node_modules").exists());
        let found = list_in(&[(root.clone(), "rhea")]);
        assert_eq!(found[0].name, "pretty-ui");
    }

    #[test]
    fn a_repo_with_several_skills_lists_them_and_a_folder_link_picks_one() {
        let root = temp("archive-many");
        let archive = archive_with(&[("skills/a/SKILL.md", "---\nname: a\n---\nA"), ("skills/b/SKILL.md", "---\nname: b\n---\nB")]);
        let repo = parse_github("https://github.com/o/r").unwrap();
        let error = install_from_archive(&repo, &archive, None, &root).unwrap_err();
        assert!(error.contains("skills/a") && error.contains("skills/b"), "{error}");
        let one = parse_github("https://github.com/o/r/tree/main/skills/b").unwrap();
        assert_eq!(install_from_archive(&one, &archive, None, &root).unwrap()["installed"], "b");
        let none = parse_github("https://github.com/o/r/tree/main/docs").unwrap();
        assert!(install_from_archive(&none, &archive, None, &root).unwrap_err().contains("No SKILL.md"));
    }

    #[test]
    fn write_skill_replaces_an_old_copy_and_rejects_traversal() {
        let root = temp("write");
        let files = vec![("SKILL.md".to_owned(), b"---\nname: t\n---\nv1".to_vec()), ("refs/a.md".to_owned(), b"a".to_vec())];
        let dir = write_skill(&root, "t", &files).unwrap();
        assert_eq!(fs::read_to_string(dir.join("refs").join("a.md")).unwrap(), "a");
        let again = vec![("SKILL.md".to_owned(), b"v2".to_vec())];
        write_skill(&root, "t", &again).unwrap();
        assert_eq!(fs::read_to_string(root.join("t").join("SKILL.md")).unwrap(), "v2");
        assert!(!root.join("t").join("refs").exists());
        assert!(write_skill(&root, "bad", &[("../evil.md".to_owned(), b"x".to_vec())]).is_err());
        assert!(write_skill(&root, "bad", &[("C:/evil.md".to_owned(), b"x".to_vec())]).is_err());
        assert!(!root.join("evil.md").exists());
    }
}
