//! Laya, Rhea's local decision model.
//!
//! Laya (convaiinnovations/laya) is a small non-autoregressive "System 1" model: given a state and
//! a few typed questions (a choice among options, a score, a yes/no) it answers in tens of
//! milliseconds with calibrated probabilities, so quick decisions never wait for a ChatGPT round
//! trip. It speaks the `/v1/systemone` protocol.
//!
//! Rhea runs it itself, with no other tool needed (no Unsloth, no Python on the PATH):
//!   1. a private Python is downloaded once (python-build-standalone) into `~/.rhea/laya/python`,
//!   2. `pip install laya[serve]` puts the model code and PyTorch into it,
//!   3. the model weights are fetched from Hugging Face into `~/.rhea/laya/hf`,
//!   4. `python -m laya.serve` is started on a free loopback port with a random bearer token and
//!      stopped again when Rhea closes. On Windows it is also assigned to a Job Object tied to
//!      Rhea's own process, so a crash or a forced kill of Rhea takes it down too instead of
//!      leaving it running (Task Manager still lists it under its own name - "python.exe" - since
//!      Windows never nests a different executable under the process that started it).
//! Everything stays under `~/.rhea/laya`; deleting that folder removes it completely.

use std::{
    collections::VecDeque,
    env, fs,
    io::{BufRead, BufReader, Read},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::chat_store;

pub const MODEL_REPO: &str = "convaiinnovations/laya";
const PACKAGE: &str = "laya[serve]==0.3.22";
const PYTHON_RELEASES: &str = "https://api.github.com/repos/astral-sh/python-build-standalone/releases/latest";
/// What to expect, shown before installing.
pub const DOWNLOAD_ESTIMATE: &str = "about 1.5 GB (Python, PyTorch and the model)";

// ---- locations -------------------------------------------------------------------------

pub fn root() -> PathBuf {
    env::var_os("RHEA_LAYA_HOME").map(PathBuf::from).unwrap_or_else(|| chat_store::app_data_root().unwrap_or_else(env::temp_dir).join("laya"))
}

fn python_dir() -> PathBuf {
    root().join("python")
}

pub fn python_exe() -> PathBuf {
    if cfg!(windows) { python_dir().join("python.exe") } else { python_dir().join("bin").join("python3") }
}

fn hf_home() -> PathBuf {
    root().join("hf")
}

fn log_path() -> PathBuf {
    root().join("server.log")
}

fn pid_path() -> PathBuf {
    root().join("server.pid")
}

fn site_packages() -> PathBuf {
    if cfg!(windows) { python_dir().join("Lib").join("site-packages") } else { python_dir().join("lib") }
}

fn package_installed() -> bool {
    if cfg!(windows) {
        site_packages().join("laya").join("serve.py").is_file()
    } else {
        fs::read_dir(site_packages()).map(|entries| entries.flatten().any(|entry| entry.path().join("site-packages").join("laya").join("serve.py").is_file())).unwrap_or(false)
    }
}

/// `snapshots/<hash>/model.safetensors` inside the Hugging Face cache.
fn model_cached() -> bool {
    let snapshots = hf_home().join("hub").join(format!("models--{}", MODEL_REPO.replace('/', "--"))).join("snapshots");
    fs::read_dir(snapshots).map(|entries| entries.flatten().any(|entry| entry.path().join("model.safetensors").exists())).unwrap_or(false)
}

// ---- progress, for the settings page ----------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub running: bool,
    pub step: String,
    pub detail: String,
    pub fraction: Option<f32>,
    pub error: Option<String>,
    pub done: bool,
}

fn progress_cell() -> &'static Mutex<Progress> {
    static PROGRESS: OnceLock<Mutex<Progress>> = OnceLock::new();
    PROGRESS.get_or_init(|| Mutex::new(Progress::default()))
}

pub fn progress() -> Progress {
    progress_cell().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn update(change: impl FnOnce(&mut Progress)) {
    change(&mut progress_cell().lock().unwrap_or_else(|p| p.into_inner()));
}

fn step(name: &str, detail: &str) {
    update(|progress| {
        progress.step = name.to_owned();
        progress.detail = detail.to_owned();
        progress.fraction = None;
    });
}

// ---- status -----------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub python: bool,
    pub package: bool,
    pub model: bool,
    pub running: bool,
    pub port: Option<u16>,
}

impl Status {
    pub fn installed(&self) -> bool {
        self.python && self.package && self.model
    }
}

pub fn status() -> Status {
    let server = server_cell().lock().unwrap_or_else(|p| p.into_inner());
    Status { python: python_exe().is_file(), package: package_installed(), model: model_cached(), running: server.is_some(), port: server.as_ref().map(|running| running.port) }
}

// ---- installing -------------------------------------------------------------------------

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60 * 30))
        .connect_timeout(Duration::from_secs(30))
        .user_agent(format!("Rhea/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| error.to_string())
}

/// The platform part of python-build-standalone's asset names.
fn platform_triple() -> Result<&'static str, String> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Ok("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("x86_64-unknown-linux-gnu")
    } else {
        Err("The local model is not available for this kind of computer yet.".into())
    }
}

/// The newest CPython 3.12 `install_only` build for this platform in a python-build-standalone
/// release listing. (3.12 is what PyTorch and the laya package both support.)
fn pick_python_asset(release: &Value, triple: &str) -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    for asset in release.get("assets")?.as_array()? {
        let name = asset.get("name")?.as_str()?;
        let url = asset.get("browser_download_url")?.as_str()?;
        if name.starts_with("cpython-3.12.") && name.contains(triple) && name.ends_with("-install_only.tar.gz") && best.as_ref().is_none_or(|(current, _)| name > current.as_str()) {
            best = Some((name.to_owned(), url.to_owned()));
        }
    }
    best
}

fn download_python() -> Result<(), String> {
    let triple = platform_triple()?;
    step("Python", "Looking up a private Python build…");
    let client = http()?;
    let release: Value = client.get(PYTHON_RELEASES).header("Accept", "application/vnd.github+json").send().and_then(|r| r.error_for_status()).and_then(|r| r.json()).map_err(|error| format!("Could not look up Python: {error}"))?;
    let (name, url) = pick_python_asset(&release, triple).ok_or("No suitable Python build was found for this computer.")?;
    step("Python", &format!("Downloading {name}…"));
    let mut response = client.get(&url).send().and_then(|r| r.error_for_status()).map_err(|error| format!("Python download failed: {error}"))?;
    let total = response.content_length();
    let mut bytes: Vec<u8> = Vec::with_capacity(total.unwrap_or(40_000_000) as usize);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = response.read(&mut chunk).map_err(|error| format!("Python download failed: {error}"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(total) = total {
            let fraction = bytes.len() as f32 / total as f32;
            update(|progress| progress.fraction = Some(fraction));
        }
    }
    step("Python", "Unpacking…");
    let staging = root().join(".python-staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..])).unpack(&staging).map_err(|error| format!("Could not unpack Python: {error}"))?;
    let _ = fs::remove_dir_all(python_dir());
    fs::rename(staging.join("python"), python_dir()).map_err(|error| format!("Could not install Python: {error}"))?;
    let _ = fs::remove_dir_all(&staging);
    Ok(())
}

fn quiet(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

/// A Windows Job Object that kills whatever is assigned to it the moment Rhea's own process ends,
/// crash included - Task Manager still lists Laya's Python under its own name (it is a different
/// executable, which Windows never nests under the process that started it), but it can never
/// outlive Rhea as an orphan the way a plain child process can if Rhea is killed rather than closed
/// normally.
#[cfg(windows)]
fn kill_with_rhea_job() -> windows::Win32::Foundation::HANDLE {
    use windows::Win32::{
        Foundation::HANDLE,
        System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
    };
    static JOB: OnceLock<usize> = OnceLock::new();
    let handle = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, None).unwrap_or(HANDLE(std::ptr::null_mut()));
        if !job.is_invalid() {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32);
        }
        job.0 as usize
    });
    HANDLE(handle as *mut _)
}

/// Tie `child`'s lifetime to Rhea's own: if Rhea is killed (crash, task-ended, power loss handled
/// by the next launch's `kill_orphan`) rather than exiting normally through `stop()`, Windows tears
/// this process down with it instead of leaving it running.
#[cfg(windows)]
fn tie_to_rhea(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{Foundation::HANDLE, System::JobObjects::AssignProcessToJobObject};
    let job = kill_with_rhea_job();
    if !job.is_invalid() {
        let process = HANDLE(child.as_raw_handle());
        unsafe {
            let _ = AssignProcessToJobObject(job, process);
        }
    }
}

#[cfg(not(windows))]
fn tie_to_rhea(_child: &Child) {}

/// Run the private Python, showing its output lines as the step's detail.
fn run_python(arguments: &[&str], label: &str) -> Result<(), String> {
    let mut command = Command::new(python_exe());
    command.args(arguments).env("PYTHONUTF8", "1").env("PYTHONNOUSERSITE", "1").env("HF_HOME", hf_home()).env("PIP_DISABLE_PIP_VERSION_CHECK", "1").stdout(Stdio::piped()).stderr(Stdio::piped());
    quiet(&mut command);
    let mut child = command.spawn().map_err(|error| format!("{label}: could not start Python: {error}"))?;
    let tail = std::sync::Arc::new(Mutex::new(VecDeque::<String>::new()));
    let readers: Vec<_> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
        .map(|stream| {
            let tail = std::sync::Arc::clone(&tail);
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let line = line.trim().to_owned();
                    if line.is_empty() {
                        continue;
                    }
                    update(|progress| progress.detail = line.chars().take(160).collect());
                    let mut tail = tail.lock().unwrap_or_else(|p| p.into_inner());
                    tail.push_back(line);
                    if tail.len() > 12 {
                        tail.pop_front();
                    }
                }
            })
        })
        .collect();
    let status = child.wait().map_err(|error| error.to_string())?;
    for reader in readers {
        let _ = reader.join();
    }
    if status.success() {
        Ok(())
    } else {
        let tail = tail.lock().unwrap_or_else(|p| p.into_inner());
        Err(format!("{label} failed: {}", tail.iter().rev().take(4).rev().cloned().collect::<Vec<_>>().join(" | ")))
    }
}

/// Fetch the model weights into Rhea's own Hugging Face cache, with a progress bar based on the
/// size of what has arrived.
fn download_model() -> Result<(), String> {
    step("Model", "Downloading the Laya model (about 800 MB)…");
    let cache = hf_home().join("hub");
    let watching = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let watcher = {
        let watching = std::sync::Arc::clone(&watching);
        std::thread::spawn(move || {
            while watching.load(std::sync::atomic::Ordering::Relaxed) {
                let size = dir_size(&cache);
                update(|progress| {
                    progress.fraction = Some((size as f32 / 850_000_000.0).min(0.99));
                    progress.detail = format!("{} MB downloaded", size / 1_000_000);
                });
                std::thread::sleep(Duration::from_millis(700));
            }
        })
    };
    let script = format!(
        "from huggingface_hub import snapshot_download\nsnapshot_download({MODEL_REPO:?}, allow_patterns=['model.safetensors','*.json','encoder/*','tokenizer/*','rl_agent_config.json'])\nprint('model ready')"
    );
    let result = run_python(&["-c", &script], "Model download");
    watching.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = watcher.join();
    result
}

fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path).map(|entries| entries.flatten().map(|entry| match entry.metadata() {
        Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }).sum()).unwrap_or(0)
}

/// Install whatever is missing: Python, the laya package and PyTorch, the model. Safe to run again.
pub fn install() -> Result<(), String> {
    if progress().running {
        return Err("The local model is already being installed.".into());
    }
    update(|progress| *progress = Progress { running: true, step: "Starting".into(), ..Default::default() });
    let result = install_steps();
    update(|progress| {
        progress.running = false;
        progress.done = result.is_ok();
        progress.error = result.as_ref().err().cloned();
        progress.fraction = None;
        if result.is_ok() {
            progress.step = "Ready".into();
            progress.detail = "The local model is installed.".into();
        }
    });
    result
}

fn install_steps() -> Result<(), String> {
    fs::create_dir_all(root()).map_err(|error| format!("Could not create {}: {error}", root().display()))?;
    if !python_exe().is_file() {
        download_python()?;
    }
    if !package_installed() {
        step("Packages", "Installing the Laya runtime and PyTorch (this is the long step)…");
        run_python(&["-m", "pip", "install", "--no-warn-script-location", "--no-input", PACKAGE], "Package install")?;
    }
    if !model_cached() {
        download_model()?;
    }
    Ok(())
}

// ---- the server -------------------------------------------------------------------------

struct Running {
    child: Child,
    port: u16,
    token: String,
}

fn server_cell() -> &'static Mutex<Option<Running>> {
    static SERVER: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    SERVER.get_or_init(|| Mutex::new(None))
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0").and_then(|listener| listener.local_addr()).map(|addr| addr.port()).map_err(|error| format!("No free port: {error}"))
}

fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    // Four independently seeded hashes: unguessable enough for a loopback-only server and needs no
    // extra dependency.
    (0..4).map(|_| format!("{:016x}", std::collections::hash_map::RandomState::new().build_hasher().finish())).collect()
}

/// The environment the server runs with.
fn server_env(port: u16, token: &str, device: &str) -> Vec<(String, String)> {
    let mut vars = vec![
        ("LAYA_HOST".to_owned(), "127.0.0.1".to_owned()),
        ("LAYA_PORT".to_owned(), port.to_string()),
        ("LAYA_MODELS".to_owned(), "english".to_owned()),
        ("LAYA_PRELOAD".to_owned(), "1".to_owned()),
        ("LAYA_API_KEY".to_owned(), token.to_owned()),
        ("LAYA_LOG_LEVEL".to_owned(), "warning".to_owned()),
        ("HF_HOME".to_owned(), hf_home().display().to_string()),
        // The weights are already on disk; never reach for the network at start-up.
        ("HF_HUB_OFFLINE".to_owned(), "1".to_owned()),
        ("TOKENIZERS_PARALLELISM".to_owned(), "false".to_owned()),
        ("PYTHONUTF8".to_owned(), "1".to_owned()),
        ("PYTHONNOUSERSITE".to_owned(), "1".to_owned()),
    ];
    if device != "auto" {
        vars.push(("LAYA_DEVICE".to_owned(), device.to_owned()));
    }
    vars
}

fn health(port: u16, token: &str) -> bool {
    http()
        .and_then(|client| client.get(format!("http://127.0.0.1:{port}/health")).bearer_auth(token).timeout(Duration::from_secs(3)).send().map_err(|error| error.to_string()))
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

/// Start the server (loading the model takes several seconds) unless it already runs.
pub fn start() -> Result<u16, String> {
    {
        let mut server = server_cell().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(running) = server.as_mut() {
            if running.child.try_wait().ok().flatten().is_none() {
                return Ok(running.port);
            }
            *server = None;
        }
    }
    if !status().installed() {
        return Err("The local model is not installed yet. Install it in Settings > Local model.".into());
    }
    kill_orphan();
    let (port, token) = (free_port()?, random_token());
    let log = fs::File::create(log_path()).map_err(|error| format!("Could not open the log: {error}"))?;
    let log_err = log.try_clone().map_err(|error| error.to_string())?;
    let mut command = Command::new(python_exe());
    command.args(["-m", "laya.serve"]).envs(server_env(port, &token, "auto")).current_dir(root()).stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(log_err));
    quiet(&mut command);
    let child = command.spawn().map_err(|error| format!("Could not start the local model: {error}"))?;
    tie_to_rhea(&child);
    let _ = fs::write(pid_path(), child.id().to_string());
    *server_cell().lock().unwrap_or_else(|p| p.into_inner()) = Some(Running { child, port, token: token.clone() });
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(180) {
        std::thread::sleep(Duration::from_millis(500));
        let exited = server_cell().lock().unwrap_or_else(|p| p.into_inner()).as_mut().is_some_and(|running| running.child.try_wait().ok().flatten().is_some());
        if exited {
            *server_cell().lock().unwrap_or_else(|p| p.into_inner()) = None;
            return Err(format!("The local model stopped while starting: {}", last_log_lines()));
        }
        if health(port, &token) {
            return Ok(port);
        }
    }
    stop();
    Err(format!("The local model did not become ready in time: {}", last_log_lines()))
}

fn last_log_lines() -> String {
    fs::read_to_string(log_path()).map(|text| text.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ")).unwrap_or_default()
}

/// A server left behind by a crash of an earlier Rhea.
fn kill_orphan() {
    if let Some(pid) = fs::read_to_string(pid_path()).ok().and_then(|text| text.trim().parse::<u32>().ok()) {
        #[cfg(windows)]
        {
            let mut command = Command::new("taskkill");
            command.args(["/F", "/T", "/PID", &pid.to_string()]);
            let _ = quiet(&mut command).output();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("kill").arg(pid.to_string()).output();
        }
    }
    let _ = fs::remove_file(pid_path());
}

pub fn stop() {
    if let Some(mut running) = server_cell().lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = running.child.kill();
        let _ = running.child.wait();
    }
    let _ = fs::remove_file(pid_path());
}

// ---- asking it --------------------------------------------------------------------------

/// Ask Laya one or more typed questions about `state`. Starts the server first when needed.
/// `questions` maps a name to `{type: "choice"|"score"|"noul", instructions, criteria}`; the answer
/// holds one entry per name with the chosen option and its probabilities.
pub fn decide(state: &Value, questions: &Value) -> Result<Value, String> {
    if !questions.as_object().is_some_and(|map| !map.is_empty()) {
        return Err("`questions` must be an object with at least one question.".into());
    }
    let port = start()?;
    let token = server_cell().lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|running| running.token.clone()).ok_or("The local model is not running.")?;
    let body = json!({"model": "laya", "state": state, "questions": questions});
    let started = Instant::now();
    let response = http()?
        .post(format!("http://127.0.0.1:{port}/v1/systemone"))
        .bearer_auth(&token)
        .timeout(Duration::from_secs(60))
        .json(&body)
        .send()
        .map_err(|error| format!("The local model did not answer: {error}"))?;
    let status = response.status();
    let text = response.text().map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("The local model refused the request (HTTP {status}): {}", text.chars().take(300).collect::<String>()));
    }
    let mut answer: Value = serde_json::from_str(&text).map_err(|error| format!("The local model's answer was not JSON: {error}"))?;
    if let Some(object) = answer.as_object_mut() {
        object.insert("latency_ms".into(), json!(started.elapsed().as_millis() as u64));
    }
    Ok(answer)
}

/// The last thing a settings-page action wanted to tell the user.
fn note_cell() -> &'static Mutex<String> {
    static NOTE: OnceLock<Mutex<String>> = OnceLock::new();
    NOTE.get_or_init(|| Mutex::new(String::new()))
}

pub fn note() -> String {
    note_cell().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub fn set_note(text: String) {
    *note_cell().lock().unwrap_or_else(|p| p.into_inner()) = text;
}

/// The Test button: one real question, reported with how long it took.
pub fn self_test() -> String {
    let started = Instant::now();
    match decide(
        &json!("Hi, we were billed twice for March. Please refund the duplicate today or we will cancel our plan."),
        &json!({"department": {"type": "choice", "instructions": "Which department should handle this?", "criteria": {"billing": "invoices, payments, refunds", "technical": "bugs, outages, system errors", "other": "everything else"}}}),
    ) {
        Ok(answer) => {
            let choice = answer.pointer("/answers/department/choice").and_then(Value::as_str).unwrap_or("?");
            format!("Test passed: Laya chose “{choice}” in {} ms.", started.elapsed().as_millis())
        }
        Err(error) => format!("Test failed: {error}"),
    }
}

pub fn is_running() -> bool {
    server_cell().lock().unwrap_or_else(|p| p.into_inner()).as_mut().is_some_and(|running| running.child.try_wait().ok().flatten().is_none())
}

/// Below this probability a choice is not trusted to act on without the user's model checking.
const TRUSTED_CHOICE: f64 = 0.85;

/// Several controls match a step's label: ask the local model (only when it is already running,
/// so a step never waits for it to start) which one is meant. Returns the chosen reference only
/// when the model is confident.
pub fn pick_control(label: &str, options: &[(String, String)]) -> Option<String> {
    if options.len() < 2 || options.len() > 12 || !is_running() {
        return None;
    }
    let criteria: serde_json::Map<String, Value> = options.iter().map(|(reference, text)| (reference.clone(), Value::String(text.clone()))).collect();
    let answer = decide(
        &json!(format!("A step in a desktop plan says: click or fill the control named \"{label}\". Several visible controls carry that name.")),
        &json!({"pick": {"type": "choice", "instructions": "Which control does the step most likely mean? Prefer an enabled, primary control in the main content area over toolbars, tabs and duplicates.", "criteria": criteria}}),
    )
    .ok()?;
    let pick = answer.pointer("/answers/pick")?;
    let choice = pick.get("choice")?.as_str()?;
    let confidence = pick.pointer(&format!("/probabilities/{choice}")).and_then(Value::as_f64).or_else(|| pick.get("confidence").and_then(Value::as_f64))?;
    (confidence >= TRUSTED_CHOICE && options.iter().any(|(reference, _)| reference == choice)).then(|| choice.to_owned())
}

/// The `laya_decide` tool.
pub fn decide_tool(arguments: &Value) -> Result<Value, String> {
    let state = arguments.get("state").ok_or("`state` is required: the situation to decide about, as text or an object.")?;
    let questions = arguments.get("questions").ok_or("`questions` is required.")?;
    decide(state, questions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_matching_python_build_is_chosen() {
        let release = json!({"assets": [
            {"name": "cpython-3.12.11+20250712-x86_64-pc-windows-msvc-install_only.tar.gz", "browser_download_url": "https://x/old"},
            {"name": "cpython-3.12.12+20251209-x86_64-pc-windows-msvc-install_only.tar.gz", "browser_download_url": "https://x/new"},
            {"name": "cpython-3.12.12+20251209-x86_64-pc-windows-msvc-install_only_stripped.tar.gz", "browser_download_url": "https://x/stripped"},
            {"name": "cpython-3.13.1+20251209-x86_64-pc-windows-msvc-install_only.tar.gz", "browser_download_url": "https://x/313"},
            {"name": "cpython-3.12.12+20251209-aarch64-apple-darwin-install_only.tar.gz", "browser_download_url": "https://x/mac"}
        ]});
        assert_eq!(pick_python_asset(&release, "x86_64-pc-windows-msvc").unwrap().1, "https://x/new");
        assert_eq!(pick_python_asset(&release, "aarch64-apple-darwin").unwrap().1, "https://x/mac");
        assert!(pick_python_asset(&release, "x86_64-unknown-linux-gnu").is_none());
    }

    #[test]
    fn the_server_listens_only_on_loopback_with_a_token_and_never_touches_the_network() {
        let vars: std::collections::HashMap<_, _> = server_env(4242, "secret", "auto").into_iter().collect();
        assert_eq!(vars["LAYA_HOST"], "127.0.0.1");
        assert_eq!(vars["LAYA_PORT"], "4242");
        assert_eq!(vars["LAYA_API_KEY"], "secret");
        assert_eq!(vars["HF_HUB_OFFLINE"], "1");
        assert!(!vars.contains_key("LAYA_DEVICE"));
        assert_eq!(server_env(1, "t", "cuda").into_iter().find(|(k, _)| k == "LAYA_DEVICE").unwrap().1, "cuda");
    }

    #[test]
    fn tokens_are_long_and_different_every_time() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn questions_are_required() {
        assert!(decide(&json!("state"), &json!({})).unwrap_err().contains("at least one question"));
        assert!(decide_tool(&json!({"questions": {"a": {}}})).unwrap_err().contains("`state`"));
    }

    /// Live end-to-end check on this machine: installs the runtime into a throwaway folder (a
    /// large download), starts the server and asks it a question. Run with:
    /// RHEA_LAYA_HOME=<empty folder> cargo test --bin rhea live_laya_install_and_decide -- --ignored --nocapture
    #[test]
    #[ignore = "downloads about 1.5 GB"]
    fn live_laya_install_and_decide() {
        install().expect("install");
        assert!(status().installed());
        let answer = decide(
            &json!("Hi, we were billed twice for March. Please refund the duplicate today or we will cancel our plan."),
            &json!({"department": {"type": "choice", "instructions": "Which department should handle this?", "criteria": {"billing": "invoices, payments, refunds", "technical": "bugs and outages", "other": "everything else"}}}),
        )
        .expect("decide");
        println!("{answer}");
        assert_eq!(answer["answers"]["department"]["choice"], "billing");
        stop();
    }
}
