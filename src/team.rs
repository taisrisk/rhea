//! Shared coordination state for the manager chat and its subagents, plus the two
//! blocking interactions that need the user (`ask_user` and `propose_plan`).
//!
//! * Subagents are workers started by the main model. Each has its own transcript,
//!   model, status and checkpoint, runs on its own thread with the same tools as the
//!   main chat, and can message the manager (`main`) and every other agent.
//! * Every agent's state lives here, so any agent can see what the others are doing
//!   (`agent_list`, and an automatic team-status note whenever it changes) and an idle
//!   agent keeps its context and can be given a new task instead of respawned.
//! * `ask_user` / `propose_plan` park the calling worker thread until the UI answers.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::chat::{ToolActivity, Turn};

pub const MAIN: &str = "main";
const MAX_AGENTS: usize = 12;
const MAX_RUNNING: usize = 6;
const MAX_NAME_CHARS: usize = 32;
const MAX_TRANSCRIPT_TURNS: usize = 60;
const MAX_INBOX: usize = 64;
const MAX_MESSAGE_CHARS: usize = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Running,
    /// Finished its task; keeps its context and can be given more work.
    Idle,
    Failed,
    Stopped,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Idle => "done",
            AgentStatus::Failed => "failed",
            AgentStatus::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub chat_id: String,
    pub task: String,
    pub model: String,
    pub status: AgentStatus,
    pub turns: Vec<Turn>,
    /// Short note of where the agent got to; survives restarts so work can resume.
    pub checkpoint: String,
    pub result: String,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub runs: u32,
    /// The agent's latest reported request usage (its context size) and its running totals.
    #[serde(default)]
    pub usage: crate::chat::Usage,
    #[serde(default)]
    pub usage_total: crate::chat::Usage,
    /// Last time the agent made visible progress; a "running" agent silent for too long is
    /// treated as stalled so a manager never waits on it forever.
    #[serde(skip)]
    pub last_update_ms: u64,
    #[serde(skip)]
    pub stop: Arc<AtomicBool>,
}

/// A running agent that has produced nothing for this long no longer holds its manager back.
const STALL_MS: u64 = 20 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Message {
    pub from: String,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct AskOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Default)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AskOption>,
    pub allow_custom: bool,
}

#[derive(Clone, Debug)]
pub struct AskRequest {
    pub id: u64,
    pub questions: Vec<AskQuestion>,
}

#[derive(Clone, Debug)]
pub struct PlanRequest {
    pub id: u64,
    pub markdown: String,
}

#[derive(Clone, Debug)]
pub struct PlanDecision {
    pub accepted: bool,
    pub feedback: String,
}

/// An update streamed from a running agent into its transcript.
pub enum Update {
    Delta(String),
    Tool(ToolActivity),
    Usage(crate::chat::Usage),
}

/// Everything a runner needs to execute one run of an agent.
pub struct RunRequest {
    pub agent_id: String,
    pub name: String,
    pub model: String,
    pub history: Vec<Turn>,
    pub prompt: String,
    pub stop: Arc<AtomicBool>,
}

pub type Runner = Arc<dyn Fn(&RunRequest, &dyn Fn(Update)) -> Result<String, String> + Send + Sync>;

#[derive(Default)]
struct State {
    agents: Vec<Agent>,
    next_agent: u64,
    // The special `main` recipient exists in every chat; include chat_id in the
    // key so a response in one conversation cannot consume another's messages.
    inboxes: HashMap<(String, String), VecDeque<Message>>,
    version: u64,
    next_interaction: u64,
    ask: Option<AskRequest>,
    ask_answer: Option<Option<Vec<(String, String)>>>,
    plan: Option<PlanRequest>,
    plan_decision: Option<PlanDecision>,
}

pub struct Hub {
    state: Mutex<State>,
    changed: Condvar,
}

pub fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub { state: Mutex::new(State::default()), changed: Condvar::new() })
}

/// The hub is one process-wide value, so every test that uses it takes this lock first.
#[cfg(test)]
pub fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn normalize(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump(&self, state: &mut State) {
        state.version = state.version.wrapping_add(1);
        self.changed.notify_all();
    }

    // ---- agents -------------------------------------------------------------------

    /// Create an agent and start its first run. Returns its id and name.
    pub fn spawn(&self, chat_id: &str, name: &str, task: &str, model: &str, runner: Runner) -> Result<Value, String> {
        let task = task.trim();
        if task.is_empty() {
            return Err("agent_spawn needs a task.".into());
        }
        let name = clip(name.trim(), MAX_NAME_CHARS);
        if name.is_empty() {
            return Err("agent_spawn needs a name.".into());
        }
        let id = {
            let mut state = self.lock();
            let mine: Vec<&Agent> = state.agents.iter().filter(|a| a.chat_id == chat_id).collect();
            if mine.len() >= MAX_AGENTS {
                return Err(format!("This chat already has {MAX_AGENTS} agents; reuse an idle one with agent_send."));
            }
            if mine.iter().filter(|a| a.status == AgentStatus::Running).count() >= MAX_RUNNING {
                return Err(format!("{MAX_RUNNING} agents are already running; wait for one to finish."));
            }
            if mine.iter().any(|a| normalize(&a.name) == normalize(&name) || normalize(&a.name) == MAIN) || normalize(&name) == MAIN {
                return Err(format!("An agent named '{name}' already exists (or the name is reserved). Pick another or message the existing one."));
            }
            state.next_agent += 1;
            let id = format!("a{}", state.next_agent);
            state.agents.push(Agent {
                id: id.clone(),
                name: name.clone(),
                chat_id: chat_id.to_owned(),
                task: task.to_owned(),
                model: model.to_owned(),
                status: AgentStatus::Running,
                turns: Vec::new(),
                checkpoint: String::new(),
                result: String::new(),
                started_ms: now_ms(),
                finished_ms: None,
                runs: 0,
                usage: Default::default(),
                usage_total: Default::default(),
                last_update_ms: now_ms(),
                stop: Arc::new(AtomicBool::new(false)),
            });
            self.bump(&mut state);
            id
        };
        self.start_run(&id, task.to_owned(), runner);
        Ok(json!({"id": id, "name": name, "status": "running", "model": model}))
    }

    /// Begin a run for an agent that is not currently running (first run or resume).
    fn start_run(&self, id: &str, prompt: String, runner: Runner) {
        let request = {
            let mut state = self.lock();
            let Some(agent) = state.agents.iter_mut().find(|a| a.id == id) else { return };
            agent.status = AgentStatus::Running;
            agent.last_update_ms = now_ms();
            agent.finished_ms = None;
            agent.runs += 1;
            agent.stop = Arc::new(AtomicBool::new(false));
            let history = agent.turns.clone();
            agent.turns.push(Turn { role: "user".into(), content: prompt.clone(), ..Default::default() });
            agent.turns.push(Turn { role: "assistant".into(), ..Default::default() });
            let request = RunRequest {
                agent_id: agent.id.clone(),
                name: agent.name.clone(),
                model: agent.model.clone(),
                history,
                prompt,
                stop: agent.stop.clone(),
            };
            self.bump(&mut state);
            request
        };
        let agent_id = request.agent_id.clone();
        thread::Builder::new()
            .name(format!("agent-{}", request.name))
            .spawn(move || {
                let hub = hub();
                let update = |item: Update| hub.apply_update(&request.agent_id, item);
                let outcome = runner(&request, &update);
                hub.finish_run(&request.agent_id, &request.stop, outcome);
            })
            .unwrap_or_else(|error| {
                self.finish_run(&agent_id, &Arc::new(AtomicBool::new(false)), Err(format!("Could not start agent thread: {error}")));
                unreachable_handle()
            });
    }

    fn apply_update(&self, id: &str, update: Update) {
        let mut state = self.lock();
        let Some(agent) = state.agents.iter_mut().find(|a| a.id == id) else { return };
        agent.last_update_ms = now_ms();
        if let Update::Usage(usage) = update {
            let total = &mut agent.usage_total;
            total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
            total.cached_tokens = total.cached_tokens.saturating_add(usage.cached_tokens);
            total.cache_write_tokens = total.cache_write_tokens.saturating_add(usage.cache_write_tokens);
            total.cache_reported |= usage.cache_reported;
            total.cache_write_reported |= usage.cache_write_reported;
            agent.usage = usage;
            self.bump(&mut state);
            return;
        }
        let Some(turn) = agent.turns.last_mut().filter(|t| t.role == "assistant") else { return };
        match update {
            Update::Delta(text) => turn.content.push_str(&text),
            Update::Tool(activity) => crate::chat::record_activity(&mut turn.tool_activity, activity),
            Update::Usage(_) => {}
        }
        self.bump(&mut state);
    }

    fn finish_run(&self, id: &str, stop: &Arc<AtomicBool>, outcome: Result<String, String>) {
        let mut notify: Option<(String, String, String)> = None;
        {
            let mut state = self.lock();
            let Some(agent) = state.agents.iter_mut().find(|a| a.id == id) else { return };
            if !Arc::ptr_eq(&agent.stop, stop) {
                return; // a newer run owns this agent
            }
            agent.finished_ms = Some(now_ms());
            let stopped = stop.load(Ordering::SeqCst);
            match outcome {
                Ok(text) if !stopped => {
                    agent.status = AgentStatus::Idle;
                    agent.result = text.clone();
                    agent.checkpoint = clip(&text, 600);
                    if let Some(turn) = agent.turns.last_mut().filter(|t| t.role == "assistant" && t.content.trim().is_empty()) {
                        turn.content = text.clone();
                    }
                    notify = Some((agent.chat_id.clone(), agent.name.clone(), format!("finished. Full result:
{text}")));
                }
                Ok(_) | Err(_) if stopped => {
                    agent.status = AgentStatus::Stopped;
                    agent.checkpoint = format!("stopped after {} run(s)", agent.runs);
                    notify = Some((agent.chat_id.clone(), agent.name.clone(), "was stopped".into()));
                }
                Err(error) => {
                    agent.status = AgentStatus::Failed;
                    agent.checkpoint = clip(&error, 600);
                    agent.result = error.clone();
                    if let Some(turn) = agent.turns.last_mut().filter(|t| t.role == "assistant" && t.content.trim().is_empty()) {
                        turn.content = format!("This agent stopped: {error}");
                    }
                    notify = Some((agent.chat_id.clone(), agent.name.clone(), format!("failed: {error}")));
                }
                Ok(_) => {}
            }
            if agent.turns.len() > MAX_TRANSCRIPT_TURNS {
                let excess = agent.turns.len() - MAX_TRANSCRIPT_TURNS;
                agent.turns.drain(..excess);
            }
            let chat_id = agent.chat_id.clone();
            let snapshot = agent.clone();
            self.bump(&mut state);
            drop(state);
            save_agent(&chat_id, &snapshot);
        }
        if let Some((chat_id, name, text)) = notify {
            self.post(&chat_id, &name, MAIN, &text);
        }
    }

    /// Find an agent by id or (fuzzy) name inside a chat.
    pub fn find(&self, chat_id: &str, who: &str) -> Option<Agent> {
        let state = self.lock();
        let key = normalize(who);
        state
            .agents
            .iter()
            .filter(|a| a.chat_id == chat_id)
            .find(|a| a.id == who || normalize(&a.name) == key)
            .or_else(|| state.agents.iter().filter(|a| a.chat_id == chat_id).find(|a| !key.is_empty() && normalize(&a.name).contains(&key)))
            .cloned()
    }

    /// Deliver `text` to `to` ("main" or an agent). A finished agent is resumed with the
    /// message as its new task so it keeps its context.
    pub fn send(&self, chat_id: &str, from: &str, to: &str, text: &str, runner: Runner) -> Result<Value, String> {
        let text = clip(text.trim(), MAX_MESSAGE_CHARS);
        if text.is_empty() {
            return Err("The message is empty.".into());
        }
        if normalize(to) == MAIN {
            self.post(chat_id, from, MAIN, &text);
            return Ok(json!({"delivered": true, "to": MAIN}));
        }
        let agent = self.find(chat_id, to).ok_or_else(|| format!("No agent named '{to}'. Use agent_list."))?;
        if normalize(&agent.name) == normalize(from) || agent.id == from {
            return Err("An agent cannot message itself.".into());
        }
        if agent.status == AgentStatus::Running {
            self.post(chat_id, from, &agent.id, &text);
            return Ok(json!({"delivered": true, "to": agent.name, "status": "running"}));
        }
        {
            let state = self.lock();
            let running = state.agents.iter().filter(|a| a.chat_id == chat_id && a.status == AgentStatus::Running).count();
            if running >= MAX_RUNNING {
                return Err(format!("{MAX_RUNNING} agents are already running; wait for one to finish."));
            }
        }
        self.start_run(&agent.id, format!("[message from {from}] {text}"), runner);
        Ok(json!({"delivered": true, "to": agent.name, "status": "resumed"}))
    }

    fn post(&self, chat_id: &str, from: &str, to: &str, text: &str) {
        let mut state = self.lock();
        let inbox = state.inboxes.entry((chat_id.to_owned(), to.to_owned())).or_default();
        if inbox.len() >= MAX_INBOX {
            inbox.pop_front();
        }
        inbox.push_back(Message { from: from.to_owned(), text: clip(text, MAX_MESSAGE_CHARS) });
        self.bump(&mut state);
    }

    /// Hand `text` to the manager of `chat_id` from outside the team (the user steering a run).
    pub fn post_to_main(&self, chat_id: &str, from: &str, text: &str) {
        self.post(chat_id, from, MAIN, text);
    }

    /// Messages waiting for `recipient` (an agent id or "main"), formatted for the model.
    pub fn drain_inbox(&self, chat_id: &str, recipient: &str) -> Vec<String> {
        let mut state = self.lock();
        state
            .inboxes
            .get_mut(&(chat_id.to_owned(), recipient.to_owned()))
            .map(|inbox| inbox.drain(..).map(|m| format!("[message from {}] {}", m.from, m.text)).collect())
            .unwrap_or_default()
    }

    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// One-line-per-agent status block so every agent knows what the others are doing.
    pub fn team_status(&self, chat_id: &str) -> String {
        let state = self.lock();
        let lines: Vec<String> = state
            .agents
            .iter()
            .filter(|a| a.chat_id == chat_id)
            .map(|a| format!("- {} ({}): {} — {}", a.name, a.id, a.status.label(), clip(&a.checkpoint, 140)))
            .collect();
        if lines.is_empty() { String::new() } else { format!("[team status]\n{}", lines.join("\n")) }
    }

    pub fn list(&self, chat_id: &str) -> Vec<Agent> {
        self.lock().agents.iter().filter(|a| a.chat_id == chat_id).cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Agent> {
        self.lock().agents.iter().find(|a| a.id == id).cloned()
    }

    pub fn running_count(&self, chat_id: &str) -> usize {
        self.lock().agents.iter().filter(|a| a.chat_id == chat_id && a.status == AgentStatus::Running).count()
    }

    /// Running agents that are still making progress (not silent past the stall limit).
    pub fn active_count(&self, chat_id: &str) -> usize {
        let now = now_ms();
        self.lock()
            .agents
            .iter()
            .filter(|a| a.chat_id == chat_id && a.status == AgentStatus::Running && now.saturating_sub(a.last_update_ms) < STALL_MS)
            .count()
    }

    pub fn stop(&self, chat_id: &str, who: &str) -> Result<Value, String> {
        let agent = self.find(chat_id, who).ok_or_else(|| format!("No agent named '{who}'."))?;
        agent.stop.store(true, Ordering::SeqCst);
        {
            let mut state = self.lock();
            if let Some(record) = state.agents.iter_mut().find(|a| a.id == agent.id) {
                if record.status == AgentStatus::Running {
                    // The worker thread finalizes the status when it notices the flag.
                    record.checkpoint = "stop requested".into();
                }
            }
            self.bump(&mut state);
        }
        Ok(json!({"id": agent.id, "name": agent.name, "stopping": true}))
    }

    /// Stop every running agent of a chat (used by the global cancel button).
    pub fn stop_all(&self, chat_id: &str) {
        for agent in self.list(chat_id) {
            agent.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Block until the named agents (or every running agent) are no longer running, a
    /// message for `recipient` arrives, `timeout` passes, or `cancelled` is true.
    pub fn wait(&self, chat_id: &str, ids: &[String], recipient: &str, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value, String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            let watched: Vec<&Agent> = state
                .agents
                .iter()
                .filter(|a| a.chat_id == chat_id && (ids.is_empty() || ids.iter().any(|w| a.id == *w || normalize(&a.name) == normalize(w))))
                .collect();
            let still_running = watched.iter().any(|a| a.status == AgentStatus::Running);
            let inbox_key = (chat_id.to_owned(), recipient.to_owned());
            let has_message = state.inboxes.get(&inbox_key).is_some_and(|inbox| !inbox.is_empty());
            let timed_out = std::time::Instant::now() >= deadline;
            if !still_running || has_message || timed_out || cancelled() {
                let report: Vec<Value> = watched
                    .iter()
                    .map(|a| json!({"id": a.id, "name": a.name, "status": a.status.label(), "result": clip(&a.result, 60_000), "checkpoint": clip(&a.checkpoint, 300)}))
                    .collect();
                let timed = timed_out && still_running;
                drop(state);
                if cancelled() {
                    return Err("Run cancelled.".into());
                }
                return Ok(json!({"agents": report, "messages": self.drain_inbox(chat_id, recipient), "timed_out": timed}));
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    // ---- ask_user -----------------------------------------------------------------

    /// Show questions to the user and block for the answer. `Ok(None)` = skipped.
    pub fn ask(&self, questions: Vec<AskQuestion>, cancelled: &dyn Fn() -> bool) -> Result<Option<Vec<(String, String)>>, String> {
        if questions.is_empty() {
            return Err("ask_user needs at least one question.".into());
        }
        let id = {
            let mut state = self.lock();
            if state.ask.is_some() || state.plan.is_some() {
                return Err("The user is already answering another prompt; wait for it.".into());
            }
            state.next_interaction += 1;
            let id = state.next_interaction;
            state.ask = Some(AskRequest { id, questions });
            state.ask_answer = None;
            self.bump(&mut state);
            id
        };
        let mut state = self.lock();
        loop {
            if let Some(answer) = state.ask_answer.take() {
                state.ask = None;
                self.bump(&mut state);
                return Ok(answer);
            }
            if cancelled() {
                if state.ask.as_ref().is_some_and(|ask| ask.id == id) {
                    state.ask = None;
                }
                self.bump(&mut state);
                return Err("Run cancelled.".into());
            }
            let (guard, _) = self.changed.wait_timeout(state, Duration::from_millis(100)).unwrap_or_else(|p| p.into_inner());
            state = guard;
        }
    }

    pub fn pending_ask(&self) -> Option<AskRequest> {
        self.lock().ask.clone()
    }

    pub fn answer_ask(&self, id: u64, answers: Option<Vec<(String, String)>>) {
        let mut state = self.lock();
        if state.ask.as_ref().is_some_and(|ask| ask.id == id) {
            state.ask_answer = Some(answers);
            self.bump(&mut state);
        }
    }

    // ---- propose_plan -------------------------------------------------------------

    pub fn propose_plan(&self, markdown: &str, cancelled: &dyn Fn() -> bool) -> Result<PlanDecision, String> {
        let markdown = markdown.trim();
        if markdown.is_empty() {
            return Err("propose_plan needs plan_markdown.".into());
        }
        let id = {
            let mut state = self.lock();
            if state.ask.is_some() || state.plan.is_some() {
                return Err("The user is already answering another prompt; wait for it.".into());
            }
            state.next_interaction += 1;
            let id = state.next_interaction;
            state.plan = Some(PlanRequest { id, markdown: markdown.to_owned() });
            state.plan_decision = None;
            self.bump(&mut state);
            id
        };
        let mut state = self.lock();
        loop {
            if let Some(decision) = state.plan_decision.take() {
                state.plan = None;
                self.bump(&mut state);
                return Ok(decision);
            }
            if cancelled() {
                if state.plan.as_ref().is_some_and(|plan| plan.id == id) {
                    state.plan = None;
                }
                self.bump(&mut state);
                return Err("Run cancelled.".into());
            }
            let (guard, _) = self.changed.wait_timeout(state, Duration::from_millis(100)).unwrap_or_else(|p| p.into_inner());
            state = guard;
        }
    }

    pub fn pending_plan(&self) -> Option<PlanRequest> {
        self.lock().plan.clone()
    }

    pub fn decide_plan(&self, id: u64, accepted: bool, feedback: &str) {
        let mut state = self.lock();
        if state.plan.as_ref().is_some_and(|plan| plan.id == id) {
            state.plan_decision = Some(PlanDecision { accepted, feedback: feedback.trim().to_owned() });
            self.bump(&mut state);
        }
    }

    // ---- persistence --------------------------------------------------------------

    /// Load a chat's saved agents (anything that was running becomes idle/"interrupted").
    pub fn load_chat(&self, chat_id: &str) {
        let Some(path) = agents_path(chat_id) else { return };
        let Ok(text) = fs::read_to_string(path) else { return };
        let Ok(saved) = serde_json::from_str::<Vec<Agent>>(&text) else { return };
        let mut state = self.lock();
        for mut agent in saved {
            if state.agents.iter().any(|a| a.id == agent.id) {
                continue;
            }
            if agent.status == AgentStatus::Running {
                agent.status = AgentStatus::Stopped;
                agent.checkpoint = format!("interrupted: {}", clip(&agent.checkpoint, 400));
            }
            agent.stop = Arc::new(AtomicBool::new(false));
            let number = agent.id.trim_start_matches('a').parse::<u64>().unwrap_or(0);
            state.next_agent = state.next_agent.max(number);
            state.agents.push(agent);
        }
        self.bump(&mut state);
    }

    pub fn forget_chat(&self, chat_id: &str) {
        self.stop_all(chat_id);
        let mut state = self.lock();
        state.agents.retain(|a| a.chat_id != chat_id);
        state.inboxes.retain(|(inbox_chat, _), _| inbox_chat != chat_id);
        self.bump(&mut state);
        if let Some(path) = agents_path(chat_id) {
            let _ = fs::remove_file(path);
        }
    }
}

fn unreachable_handle() -> thread::JoinHandle<()> {
    thread::spawn(|| {})
}

fn agents_path(chat_id: &str) -> Option<PathBuf> {
    // Unit tests use the process-wide hub; never let them write into a real profile.
    if cfg!(test) || !crate::chat_store::is_valid_chat_id(chat_id) {
        return None;
    }
    crate::chat_store::app_data_root().map(|root| root.join("agents").join(format!("{chat_id}.json")))
}

fn save_agent(chat_id: &str, agent: &Agent) {
    let Some(path) = agents_path(chat_id) else { return };
    let mut all: Vec<Agent> = fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    all.retain(|a| a.id != agent.id);
    all.push(agent.clone());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = crate::chat_store::write_json_atomic(&path, &all);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn hb() -> &'static Hub {
        hub()
    }

    /// The hub is a process-wide singleton, so tests that use it run one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        super::test_serial()
    }

    fn runner(reply: &'static str) -> Runner {
        Arc::new(move |request: &RunRequest, update: &dyn Fn(Update)| {
            update(Update::Delta(format!("working on {}", request.prompt)));
            thread::sleep(Duration::from_millis(30));
            Ok(reply.to_owned())
        })
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(5), "timed out waiting");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn spawn_runs_to_idle_and_notifies_main() {
        let _serial = serial();
        let chat = "chat-test-spawn";
        let hub = hb();
        let made = hub.spawn(chat, "Scout", "look around", "gpt-test", runner("found it")).unwrap();
        let id = made["id"].as_str().unwrap().to_owned();
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Idle));
        let agent = hub.get(&id).unwrap();
        assert_eq!(agent.result, "found it");
        assert_eq!(agent.turns.len(), 2);
        assert!(agent.turns[1].content.contains("found it") || agent.turns[1].content.contains("working on"));
        // the completion note is posted just after the status flips to idle
        let mut mail = Vec::new();
        wait_until(|| {
            mail.extend(hub.drain_inbox(chat, MAIN));
            mail.iter().any(|m| m.contains("Scout") && m.contains("found it"))
        });
        hub.forget_chat(chat);
    }

    #[test]
    fn long_reports_reach_main_untruncated_and_usage_is_tracked() {
        let _serial = serial();
        let chat = "chat-test-long";
        let hub = hb();
        let long: &'static str = Box::leak("x".repeat(30_000).into_boxed_str());
        let runner: Runner = Arc::new(move |_request: &RunRequest, update: &dyn Fn(Update)| {
            update(Update::Usage(crate::chat::Usage { input_tokens: 100, cached_tokens: 60, cache_reported: true, ..Default::default() }));
            update(Update::Usage(crate::chat::Usage { input_tokens: 150, cached_tokens: 90, cache_reported: true, ..Default::default() }));
            Ok(long.to_owned())
        });
        let made = hub.spawn(chat, "Big", "write a lot", "m", runner).unwrap();
        let id = made["id"].as_str().unwrap().to_owned();
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Idle));
        let agent = hub.get(&id).unwrap();
        assert_eq!(agent.result.len(), 30_000);
        assert_eq!((agent.usage.input_tokens, agent.usage_total.input_tokens, agent.usage_total.cached_tokens), (150, 250, 150));
        let mut mail = Vec::new();
        wait_until(|| {
            mail.extend(hub.drain_inbox(chat, MAIN));
            mail.iter().any(|m| m.len() > 30_000)
        });
        hub.forget_chat(chat);
    }

    #[test]
    fn duplicate_and_reserved_names_and_limits_are_rejected() {
        let _serial = serial();
        let chat = "chat-test-names";
        let hub = hb();
        hub.spawn(chat, "Alpha", "t", "m", runner("ok")).unwrap();
        assert!(hub.spawn(chat, "alpha", "t", "m", runner("ok")).is_err());
        assert!(hub.spawn(chat, "main", "t", "m", runner("ok")).is_err());
        assert!(hub.spawn(chat, "  ", "t", "m", runner("ok")).is_err());
        assert!(hub.spawn(chat, "Beta", "  ", "m", runner("ok")).is_err());
        hub.forget_chat(chat);
    }

    #[test]
    fn idle_agent_is_resumed_with_its_context() {
        let _serial = serial();
        let chat = "chat-test-resume";
        let hub = hb();
        let id = hub.spawn(chat, "Worker", "first task", "m", runner("one")).unwrap()["id"].as_str().unwrap().to_owned();
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Idle));
        let seen = Arc::new(Mutex::new(0usize));
        let seen_inner = seen.clone();
        let resumer: Runner = Arc::new(move |request: &RunRequest, _| {
            *seen_inner.lock().unwrap() = request.history.len();
            Ok("two".into())
        });
        let outcome = hub.send(chat, MAIN, "worker", "second task", resumer).unwrap();
        assert_eq!(outcome["status"], "resumed");
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Idle && a.result == "two"));
        assert_eq!(*seen.lock().unwrap(), 2, "history from the first run must be reused");
        assert_eq!(hub.get(&id).unwrap().runs, 2);
        hub.forget_chat(chat);
    }

    #[test]
    fn messages_reach_running_agents_main_and_teammates() {
        let _serial = serial();
        let chat = "chat-test-msg";
        let hub = hb();
        let slow: Runner = Arc::new(|_, _| {
            thread::sleep(Duration::from_millis(400));
            Ok("slow done".into())
        });
        let id = hub.spawn(chat, "Slow", "wait", "m", slow.clone()).unwrap()["id"].as_str().unwrap().to_owned();
        hub.send(chat, MAIN, "slow", "hello worker", slow.clone()).unwrap();
        assert_eq!(hub.drain_inbox(chat, &id), vec!["[message from main] hello worker".to_owned()]);
        hub.send(chat, "Slow", "main", "found a bug", slow.clone()).unwrap();
        assert!(hub.drain_inbox(chat, MAIN).iter().any(|m| m.contains("found a bug")));
        assert!(hub.send(chat, MAIN, "ghost", "x", slow.clone()).is_err());
        assert!(hub.send(chat, "Slow", "slow", "x", slow).is_err());
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Idle));
        hub.forget_chat(chat);
    }

    #[test]
    fn inboxes_are_isolated_between_chats_including_main_notifications() {
        let _serial = serial();
        let hub = hb();
        let chat_a = "chat-test-inbox-a";
        let chat_b = "chat-test-inbox-b";

        // `main` is shared by all conversations as a recipient label. Its
        // messages must nevertheless remain scoped to the originating chat.
        hub.send(chat_a, "ScoutA", MAIN, "result from A", runner("unused")).unwrap();
        hub.send(chat_b, "ScoutB", MAIN, "result from B", runner("unused")).unwrap();
        assert_eq!(hub.drain_inbox(chat_a, MAIN), vec!["[message from ScoutA] result from A"]);
        assert_eq!(hub.drain_inbox(chat_b, MAIN), vec!["[message from ScoutB] result from B"]);
        assert!(hub.drain_inbox("chat-test-inbox-c", MAIN).is_empty());

        // Automatic completion notes use the same chat-scoped main inbox.
        let id_a = hub.spawn(chat_a, "WorkerA", "task", "m", runner("done A")).unwrap()["id"].as_str().unwrap().to_owned();
        let id_b = hub.spawn(chat_b, "WorkerB", "task", "m", runner("done B")).unwrap()["id"].as_str().unwrap().to_owned();
        wait_until(|| hub.get(&id_a).is_some_and(|a| a.status == AgentStatus::Idle));
        wait_until(|| hub.get(&id_b).is_some_and(|a| a.status == AgentStatus::Idle));
        let a_notes = hub.drain_inbox(chat_a, MAIN);
        let b_notes = hub.drain_inbox(chat_b, MAIN);
        assert!(a_notes.iter().any(|note| note.contains("WorkerA") && note.contains("done A")), "{a_notes:?}");
        assert!(!a_notes.iter().any(|note| note.contains("WorkerB")), "{a_notes:?}");
        assert!(b_notes.iter().any(|note| note.contains("WorkerB") && note.contains("done B")), "{b_notes:?}");
        assert!(!b_notes.iter().any(|note| note.contains("WorkerA")), "{b_notes:?}");

        hub.forget_chat(chat_a);
        hub.forget_chat(chat_b);
    }

    #[test]
    fn wait_returns_when_agents_finish_and_on_timeout() {
        let _serial = serial();
        let chat = "chat-test-wait";
        let hub = hb();
        let id = hub.spawn(chat, "W", "t", "m", runner("done")).unwrap()["id"].as_str().unwrap().to_owned();
        let report = hub.wait(chat, &[id.clone()], "tester", Duration::from_secs(5), &|| false).unwrap();
        assert_eq!(report["agents"][0]["status"], "done");
        let blocker: Runner = Arc::new(|request: &RunRequest, _| {
            while !request.stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
            }
            Ok("late".into())
        });
        let bid = hub.spawn(chat, "B", "t", "m", blocker).unwrap()["id"].as_str().unwrap().to_owned();
        let started = Instant::now();
        let timed = hub.wait(chat, &[bid.clone()], "tester", Duration::from_millis(250), &|| false).unwrap();
        assert!(timed["timed_out"].as_bool().unwrap());
        assert!(started.elapsed() < Duration::from_secs(3));
        hub.stop(chat, "b").unwrap();
        wait_until(|| hub.get(&bid).is_some_and(|a| a.status == AgentStatus::Stopped));
        assert!(hub.wait(chat, &[bid], "tester", Duration::from_secs(1), &|| true).is_err());
        hub.forget_chat(chat);
    }

    #[test]
    fn failed_run_is_recorded() {
        let _serial = serial();
        let chat = "chat-test-fail";
        let hub = hb();
        let bad: Runner = Arc::new(|_, _| Err("boom".into()));
        let id = hub.spawn(chat, "Bad", "t", "m", bad).unwrap()["id"].as_str().unwrap().to_owned();
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Failed));
        assert!(hub.get(&id).unwrap().checkpoint.contains("boom"));
        let mut mail = Vec::new();
        wait_until(|| {
            mail.extend(hub.drain_inbox(chat, MAIN));
            mail.iter().any(|m: &String| m.contains("failed"))
        });
        hub.forget_chat(chat);
    }

    #[test]
    fn team_status_lists_every_agent() {
        let _serial = serial();
        let chat = "chat-test-status";
        let hub = hb();
        let id = hub.spawn(chat, "One", "t", "m", runner("a")).unwrap()["id"].as_str().unwrap().to_owned();
        wait_until(|| hub.get(&id).is_some_and(|a| a.status == AgentStatus::Idle));
        let status = hub.team_status(chat);
        assert!(status.contains("One") && status.contains("done"), "{status}");
        assert!(hub.team_status("chat-test-empty").is_empty());
        hub.forget_chat(chat);
    }

    #[test]
    fn ask_blocks_until_answered_and_supports_skip_and_cancel() {
        let _serial = serial();
        let hub = hb();
        let question = || vec![AskQuestion { id: "q".into(), question: "which?".into(), options: vec![AskOption { label: "A".into(), description: String::new() }], allow_custom: true }];
        let answerer = thread::spawn(|| {
            let hub = hb();
            wait_until(|| hub.pending_ask().is_some());
            let id = hub.pending_ask().unwrap().id;
            hub.answer_ask(id, Some(vec![("q".into(), "A".into())]));
        });
        let answers = hub.ask(question(), &|| false).unwrap().unwrap();
        assert_eq!(answers, vec![("q".to_owned(), "A".to_owned())]);
        answerer.join().unwrap();
        assert!(hub.pending_ask().is_none());

        let skipper = thread::spawn(|| {
            let hub = hb();
            wait_until(|| hub.pending_ask().is_some());
            hub.answer_ask(hub.pending_ask().unwrap().id, None);
        });
        assert_eq!(hub.ask(question(), &|| false).unwrap(), None);
        skipper.join().unwrap();

        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            flag2.store(true, Ordering::SeqCst);
        });
        assert!(hub.ask(question(), &|| flag.load(Ordering::SeqCst)).is_err());
        canceller.join().unwrap();
        assert!(hub.pending_ask().is_none());
        assert!(hub.ask(vec![], &|| false).is_err());
    }

    #[test]
    fn plan_accept_and_revise_round_trip() {
        let _serial = serial();
        let hub = hb();
        let reviser = thread::spawn(|| {
            let hub = hb();
            wait_until(|| hub.pending_plan().is_some());
            let plan = hub.pending_plan().unwrap();
            assert!(plan.markdown.contains("Step one"));
            hub.decide_plan(plan.id, false, "add tests");
        });
        let decision = hub.propose_plan("# Plan\nStep one", &|| false).unwrap();
        assert!(!decision.accepted);
        assert_eq!(decision.feedback, "add tests");
        reviser.join().unwrap();

        let acceptor = thread::spawn(|| {
            let hub = hb();
            wait_until(|| hub.pending_plan().is_some());
            hub.decide_plan(hub.pending_plan().unwrap().id, true, "");
        });
        assert!(hub.propose_plan("# Plan 2", &|| false).unwrap().accepted);
        acceptor.join().unwrap();
        assert!(hub.propose_plan("  ", &|| false).is_err());
        // a stale id cannot decide a newer plan
        hub.decide_plan(999_999, true, "");
        assert!(hub.pending_plan().is_none());
    }
}
