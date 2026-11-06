use std::{
    collections::HashMap,
    env, fs,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Rect, Sense, UiBuilder, Vec2, pos2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod team_ui;
mod transcript;
use team_ui::{AskUi, SideTab, title_case};

use crate::{
    auth, browser,
    chat::{self, Model, Turn},
    chat_store,
    desktop::{self, SnapshotResult},
    hooks::{HookEvent, HookRegistry},
    mcp::{self, McpManager, McpServerConfig},
    skills, team, tools,
};

/// Every UI transition is this long at most: quick and snappy.
const ANIM: f32 = 0.15;

/// How wide a queued-message bubble may get inside the space it was given.
fn row_width_cap(ui: &egui::Ui) -> f32 {
    (ui.available_width() - 30.0).max(120.0)
}

/// One line of the `/` suggestion list.
#[derive(Clone)]
struct SlashEntry {
    /// Command, skill or server name (shown with a leading `/`).
    name: String,
    /// What it does; also the hover text.
    hint: String,
    kind: &'static str,
    /// Text the chat box gets when this entry is picked (unless it runs at once).
    insert: String,
    /// A built-in command that runs as soon as it is picked.
    runs: bool,
}

impl SlashEntry {
    fn command(name: &str, hint: &str) -> Self {
        Self { name: name.into(), hint: hint.into(), kind: "Command", insert: format!("/{name}"), runs: true }
    }
}

/// `/compact`, `/compose` and `/new` typed out in full.
fn builtin_command(prompt: &str) -> Option<&'static str> {
    match prompt.trim().to_lowercase().as_str() {
        "/compact" => Some("compact"),
        "/compose" => Some("compose"),
        "/new" => Some("new"),
        _ => None,
    }
}

// Black-and-white palette with a faint dark-green accent for hover, focus and glows.
const SHELL: Color32 = Color32::from_rgb(9, 10, 10);
const SURFACE: Color32 = Color32::from_rgb(19, 20, 20);
const RAISED: Color32 = Color32::from_rgb(27, 28, 28);
const MINT: Color32 = Color32::from_rgb(35, 37, 36);
const MINT_DEEP: Color32 = Color32::from_rgb(238, 240, 238);
const HOVER_GREEN: Color32 = Color32::from_rgb(74, 186, 118);
const HOVER_FILL: Color32 = Color32::from_rgb(24, 36, 29);
const GLOW_GREEN: [u8; 3] = [52, 170, 98];
const DANGER: Color32 = Color32::from_rgb(232, 118, 106);
const SYSTEM_PROMPT_SEGMENT: Color32 = Color32::from_rgb(74, 186, 118);
const TOOLSET_SEGMENT: Color32 = Color32::from_rgb(150, 154, 152);
const INK: Color32 = Color32::from_rgb(238, 239, 237);
const MUTED: Color32 = Color32::from_rgb(140, 144, 142);
const LINE: Color32 = Color32::from_rgb(40, 42, 41);
const TEXT: Color32 = INK;

enum CompletedOperation {
    Status(Value),
    Snapshot(SnapshotResult),
    Action(Value),
    DeviceCode(String, String, Option<String>),
    SignedIn(String),
    SignedOut(String),
    Models(Vec<Model>, u128),
    ChatDelta(String),
    ToolStarted(String),
    ToolActivity(chat::ToolActivity),
    ChatLatency(u128),
    ChatUsage(chat::Usage),
    ContextCompacted(chat::CompactionUpdate),
    /// A manual `/compact`: the new summary and how many leading turns it covers.
    Compacted(Result<(String, usize), String>),
    Apps(Value),
    Windows(Value),
    Screenshot(Value),
    BrowserSnapshot(Value),
    ChatDone,
    McpUpdated(String),
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Conversation {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    title_generated: bool,
    #[serde(default)]
    title_user_edited: bool,
    #[serde(default)]
    turns: Vec<Turn>,
    #[serde(default)]
    context_summary: String,
    #[serde(default)]
    summary_through: usize,
    /// Locally estimated tokens in this chat's current effective context (summary + live turns).
    #[serde(default)]
    context_tokens: u64,
    /// Usage reported by the most recent model response in this chat.
    #[serde(default)]
    latest_usage: Option<chat::Usage>,
    /// Sum of service-reported input/output/cache usage over this chat's requests.
    #[serde(default)]
    total_usage: chat::Usage,
    #[serde(default)]
    message_count: usize,
    #[serde(default)]
    preview: String,
    #[serde(skip)]
    loaded: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct ConversationStore {
    #[serde(default)]
    conversations: Vec<Conversation>,
    #[serde(default)]
    active_conversation: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredHistory {
    #[serde(default)]
    turns: Vec<Turn>,
    #[serde(default)]
    context_summary: String,
    #[serde(default)]
    summary_through: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredToolcalls {
    #[serde(default)]
    per_turn: Vec<Vec<chat::ToolActivity>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsSection {
    Chat,
    Usage,
    Connections,
    Mcp,
    Browser,
    Agents,
    Storage,
}

impl SettingsSection {
    const ALL: [Self; 7] = [
        Self::Chat,
        Self::Usage,
        Self::Connections,
        Self::Mcp,
        Self::Browser,
        Self::Agents,
        Self::Storage,
    ];

    fn icon(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Usage => "chart",
            Self::Connections => "link",
            Self::Mcp => "plug",
            Self::Browser => "globe",
            Self::Agents => "agents",
            Self::Storage => "storage",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Chat => "Chat & model",
            Self::Usage => "Usage",
            Self::Connections => "Connections",
            Self::Mcp => "MCP servers",
            Self::Browser => "Browser",
            Self::Agents => "Agents & hooks",
            Self::Storage => "Storage",
        }
    }
}

pub struct JarvusApp {
    target_app: String,
    target_window_id: String,
    surface: String,
    element_ref: String,
    desktop_json: String,
    desktop_status: String,
    local_visualization: Option<String>,
    pending: Option<Receiver<Result<CompletedOperation, String>>>,
    started: bool,
    signed_in: bool,
    account: String,
    device_code: Option<String>,
    verification_url: String,
    model_list: Vec<Model>,
    selected_model: String,
    reasoning_effort: String,
    context_window: u64,
    context_window_reported: bool,
    context_usage: chat::Usage,
    context_usage_estimated: bool,
    /// The local text estimate at the moment `context_usage` was reported; growth since then is added live.
    usage_estimate_base: u64,
    /// Current effective context estimate for the active chat, independent of last-request usage.
    context_tokens: u64,
    last_dynamic_tool_tokens: u64,
    context_drawer_open: bool,
    history_panel_width: f32,
    history_resize_start: f32,
    renaming_conversation: Option<usize>,
    rename_draft: String,
    conversations: Vec<Conversation>,
    inactive_unloads: HashMap<String, Instant>,
    markdown_cache: CommonMarkCache,
    active_conversation: usize,
    history_dirty: bool,
    last_history_save: Instant,
    title_pending: Vec<Receiver<(String, String)>>,
    mcp_manager: Arc<Mutex<McpManager>>,
    mcp_import_text: String,
    mcp_edit_text: String,
    mcp_editing_id: Option<String>,
    mcp_expanded: Option<String>,
    storage_breakdown: Vec<(String, u64, Color32)>,
    storage_scanned: bool,
    review_open: bool,
    review_filter_days: u32,
    review_selected: std::collections::HashSet<String>,
    pending_chat_delete: Option<usize>,
    draft: String,
    editing_turn: Option<usize>,
    edit_generation: u64,
    streaming: bool,
    active_tool: Option<String>,
    latency_ms: Option<u128>,
    jev_latency_ms: Option<u128>,
    jev_action_started: Option<Instant>,
    assistant_started_at: Option<Instant>,
    notice: String,
    scroll_to_bottom: bool,
    show_tools: bool,
    show_settings: bool,
    settings_section: SettingsSection,
    native_frame_style_attempted: bool,
    /// Updated every few seconds by a background ping of the Chrome bridge.
    chrome_alive: Arc<std::sync::atomic::AtomicBool>,
    /// User hook scripts (PreToolUse, Stop, ...), shared with every worker thread.
    hooks: Arc<RwLock<HookRegistry>>,
    hooks_draft: String,
    hooks_message: String,
    /// Compose Mode: read-only investigation and a plan the user accepts before any change.
    compose_mode: bool,
    /// Model for subagents; empty = same as the main model.
    subagent_model: String,
    /// Compact the transcript once a request would fill this share of the context window.
    compact_percent: u8,
    slash_selected: usize,
    /// Messages typed while a run is working: (chat id, text); each sends when that run ends.
    queued: Vec<(String, String)>,
    /// Whose context the context panel shows (a chat id, or an agent id); a change snaps the gauge.
    context_subject: String,
    skill_url: String,
    ask_ui: Option<AskUi>,
    interaction_t: f32,
    /// The chat the context gauge currently describes; a change clears it instantly.
    context_chat_id: String,
    /// Extra composer height (px) for rows beyond the first two; set each frame, already eased.
    composer_extra: f32,
    /// Old titles still fading out in the history list, keyed by chat id.
    title_swaps: HashMap<String, String>,
    ctx_title_swap_reset: Vec<String>,
    /// While > 0 the context numbers snap instead of animating (just after a chat switch).
    context_snap_frames: u8,
    plan_revising: bool,
    plan_feedback: String,
    plan_feedback_focused: bool,
    interaction_height: f32,
    side_tab: SideTab,
    open_agent: Option<String>,
    agent_draft: String,
    agents_loaded_for: String,
}

impl Default for JarvusApp {
    fn default() -> Self {
        let (conversations, active_conversation) = load_conversation_store();
        let mcp_path = mcp::default_config_path();
        let (mcp_manager, mcp_load_error) = match McpManager::load(mcp_path.clone()) {
            Ok(manager) => (manager, None),
            Err(error) => (McpManager::new(mcp_path), Some(error)),
        };
        Self {
            target_app: String::new(),
            target_window_id: String::new(),
            surface: "focused".to_owned(),
            element_ref: String::new(),
            desktop_json:
                "Read the Windows UI Automation tree to inspect real controls and their refs."
                    .to_owned(),
            desktop_status: "Checking local desktop bridge…".into(),
            local_visualization: None,
            pending: None,
            started: false,
            signed_in: false,
            account: String::new(),
            device_code: None,
            verification_url: auth::VERIFICATION_URL.to_owned(),
            model_list: Vec::new(),
            selected_model: load_selected_model().unwrap_or_default(),
            reasoning_effort: load_reasoning_effort().unwrap_or_else(|| "medium".into()),
            context_window: chat::DEFAULT_CONTEXT_WINDOW,
            context_window_reported: false,
            context_usage: chat::Usage::default(),
            context_usage_estimated: true,
            usage_estimate_base: 0,
            context_tokens: 0,
            last_dynamic_tool_tokens: 0,
            context_drawer_open: false,
            history_panel_width: 236.0,
            history_resize_start: 236.0,
            renaming_conversation: None,
            rename_draft: String::new(),
            conversations,
            inactive_unloads: HashMap::new(),
            markdown_cache: CommonMarkCache::default(),
            active_conversation,
            history_dirty: false,
            last_history_save: Instant::now(),
            title_pending: Vec::new(),
            mcp_manager: Arc::new(Mutex::new(mcp_manager)),
            mcp_import_text: String::new(),
            mcp_edit_text: String::new(),
            mcp_editing_id: None,
            mcp_expanded: None,
            storage_breakdown: Vec::new(),
            storage_scanned: false,
            review_open: false,
            review_filter_days: 0,
            review_selected: Default::default(),
            pending_chat_delete: None,
            draft: String::new(),
            editing_turn: None,
            edit_generation: 0,
            streaming: false,
            active_tool: None,
            latency_ms: None,
            jev_latency_ms: None,
            jev_action_started: None,
            assistant_started_at: None,
            notice: mcp_load_error.unwrap_or_else(|| {
                "JEV is running locally. Connect ChatGPT to start a conversation.".into()
            }),
            scroll_to_bottom: true,
            show_tools: false,
            show_settings: false,
            settings_section: SettingsSection::Chat,
            native_frame_style_attempted: false,
            chrome_alive: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hooks: Arc::new(RwLock::new(HookRegistry::load(&crate::hooks::default_path()).unwrap_or_default())),
            hooks_draft: String::new(),
            hooks_message: String::new(),
            compose_mode: false,
            subagent_model: load_subagent_model().unwrap_or_default(),
            compact_percent: load_compact_percent().unwrap_or(chat::DEFAULT_COMPACT_PERCENT),
            slash_selected: 0,
            queued: Vec::new(),
            context_subject: String::new(),
            skill_url: String::new(),
            ask_ui: None,
            interaction_t: 0.0,
            context_chat_id: String::new(),
            composer_extra: 0.0,
            title_swaps: HashMap::new(),
            ctx_title_swap_reset: Vec::new(),
            context_snap_frames: 0,
            plan_revising: false,
            plan_feedback: String::new(),
            plan_feedback_focused: false,
            interaction_height: 0.0,
            side_tab: SideTab::History,
            open_agent: None,
            agent_draft: String::new(),
            agents_loaded_for: String::new(),
        }
    }
}

impl JarvusApp {
    fn begin(
        &mut self,
        operation: impl FnOnce(Sender<Result<CompletedOperation, String>>) + Send + 'static,
        message: &str,
    ) {
        if self.pending.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || operation(sender));
        self.pending = Some(receiver);
        self.notice = message.to_owned();
    }

    fn initialize(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        // taste-skill and unslop come with JARVUS: fetched once in the background on first launch.
        thread::spawn(skills::ensure_bundled);
        // Chrome bridge: register the native host for this user, then keep a
        // cheap liveness ping running so the UI shows real connection health.
        let alive = Arc::clone(&self.chrome_alive);
        thread::spawn(move || {
            let _ = browser::ensure_native_host_registered();
            loop {
                alive.store(browser::extension_alive(), Ordering::Relaxed);
                thread::sleep(Duration::from_secs(5));
            }
        });
        // Bring configured MCP servers up at start and keep retrying any that are down
        // (an HTTP server may start after JARVUS). Connecting happens off the manager lock.
        let mcp_manager = Arc::clone(&self.mcp_manager);
        thread::spawn(move || loop {
            let wanted = mcp_manager.lock().map(|manager| manager.disconnected_enabled()).unwrap_or_default();
            for config in wanted {
                let outcome = McpManager::connect_detached(&config);
                if let Ok(mut manager) = mcp_manager.lock() { manager.install_connection(&config.id, outcome); }
            }
            thread::sleep(Duration::from_secs(20));
        });
        self.begin(
            |tx| {
                let _ = tx.send(desktop::status().map(CompletedOperation::Status));
                if let Ok(tokens) = auth::token() {
                    let _ = tx.send(Ok(CompletedOperation::SignedIn(
                        tokens
                            .email
                            .clone()
                            .unwrap_or_else(|| "ChatGPT account".into()),
                    )));
                    let start = Instant::now();
                    let result = chat::models(&tokens.access_token, tokens.account_id.as_deref())
                        .map(|models| {
                            CompletedOperation::Models(models, start.elapsed().as_millis())
                        });
                    let _ = tx.send(result);
                }
            },
            "JEV started · checking desktop bridge and saved ChatGPT session…",
        );
    }

    fn sign_in(&mut self) {
        self.begin(
            |tx| match auth::sign_in(|url, status, error| {
                let _ = tx.send(Ok(CompletedOperation::DeviceCode(url, status, error)));
            }) {
                Ok(account) => {
                    let _ = tx.send(Ok(CompletedOperation::SignedIn(account)));
                    let start = Instant::now();
                    let result = auth::token()
                        .and_then(|tokens| {
                            chat::models(&tokens.access_token, tokens.account_id.as_deref())
                        })
                        .map(|models| {
                            CompletedOperation::Models(models, start.elapsed().as_millis())
                        });
                    let _ = tx.send(result);
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                }
            },
            "Opening ChatGPT sign-in in your browser…",
        );
    }

    fn refresh_models(&mut self) {
        self.begin(
            |tx| {
                let start = Instant::now();
                let result = auth::token()
                    .and_then(|tokens| {
                        chat::models(&tokens.access_token, tokens.account_id.as_deref())
                    })
                    .map(|models| (models, start.elapsed().as_millis()));
                let message = result.map(|(models, ms)| CompletedOperation::Models(models, ms));
                let _ = tx.send(message);
            },
            "Checking ChatGPT and refreshing its model list…",
        );
    }

    fn sign_out(&mut self) {
        self.begin(
            |tx| {
                let _ = tx.send(
                    auth::sign_out()
                        .map(|()| CompletedOperation::SignedOut("ChatGPT disconnected.".into())),
                );
            },
            "Disconnecting ChatGPT…",
        );
    }

    fn send_chat(&mut self) {
        let prompt = self.draft.trim().to_owned();
        // Reserve the single background-operation slot before mutating the transcript.
        // In particular, model refresh/startup can still be pending after sign-in flips
        // to true; silently dropping `begin` after adding turns leaves a blank bubble.
        if !chat_send_is_allowed(
            self.pending.is_some(),
            self.streaming,
            self.signed_in,
            !self.selected_model.is_empty(),
            !prompt.is_empty(),
        ) {
            return;
        }
        if let Some(command) = builtin_command(&prompt) {
            self.draft.clear();
            self.run_builtin(command);
            return;
        }
        self.draft.clear();
        let editing_turn = self.editing_turn.take();
        let mut title_request = None;
        let mut title_model = None;
        if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
            if let Some(index) = editing_turn.filter(|index| {
                conversation
                    .turns
                    .get(*index)
                    .is_some_and(|turn| turn.role == "user")
            }) {
                conversation.turns.truncate(index);
                conversation.context_summary.clear();
                conversation.summary_through = 0;
                if index == 0 {
                    conversation.title = conversation_title(&prompt);
                }
            }
            if conversation.turns.is_empty() {
                conversation.title = conversation_title(&prompt);
                if !conversation.title_generated {
                    conversation.title_generated = true;
                    title_request = Some((conversation.id.clone(), prompt.clone()));
                }
            }
            conversation.turns.push(Turn {
                role: "user".into(),
                content: prompt.clone(),
                elapsed_ms: None,
                tool_activity: Vec::new(),
            });
            conversation.turns.push(Turn {
                role: "assistant".into(),
                content: String::new(),
                elapsed_ms: None,
                tool_activity: Vec::new(),
            });
        }
        if title_request.is_some() {
            title_model = chat::title_model_for(&self.model_list, &self.selected_model);
        }
        let (summary, history) = self
            .conversations
            .get(self.active_conversation)
            .map(|conversation| {
                let end = conversation.turns.len().saturating_sub(2);
                let start = conversation.summary_through.min(end);
                (
                    conversation.context_summary.clone(),
                    conversation.turns[start..end].to_vec(),
                )
            })
            .unwrap_or_default();
        let model = self.selected_model.clone();
        let reasoning_effort = self.reasoning_effort.clone();
        let context_window = self.context_window;
        let (extra_tools, _) = self.dynamic_tool_snapshot();
        let dynamic_tool_tokens = extra_tools
            .iter()
            .map(|tool| {
                serde_json::to_vec(tool)
                    .map(|bytes| (bytes.len() as u64 + 3) / 4)
                    .unwrap_or(0)
            })
            .sum::<u64>();
        self.context_tokens = chat::estimate_context_tokens(&summary, &history, &prompt)
            .saturating_add(toolset_tokens_estimate())
            .saturating_add(dynamic_tool_tokens);
        if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
            conversation.context_tokens = self.context_tokens;
        }
        self.last_dynamic_tool_tokens = dynamic_tool_tokens;
        self.history_dirty = true;
        self.streaming = true;
        self.assistant_started_at = Some(Instant::now());
        self.active_tool = None;
        self.scroll_to_bottom = true;
        self.flush_history(true);

        if let (Some((conversation_id, title_input)), Some(title_model)) =
            (title_request, title_model)
        {
            let (title_tx, title_rx) = mpsc::channel();
            self.title_pending.push(title_rx);
            thread::spawn(move || {
                let title = auth::token()
                    .and_then(|tokens| {
                        chat::generate_title(
                            &tokens.access_token,
                            tokens.account_id.as_deref(),
                            &title_model,
                            &title_input,
                        )
                    })
                    .unwrap_or_else(|_| chat::fallback_title(&title_input));
                let _ = title_tx.send((conversation_id, title));
            });
        }

        let chat_id = self.conversations.get(self.active_conversation).map(|c| c.id.clone()).unwrap_or_default();
        let first_turn = history.is_empty() && summary.is_empty();
        let compose = self.compose_mode;
        let tool_context = tools::ToolContext {
            mcp_manager: Arc::clone(&self.mcp_manager),
            chrome_alive: Arc::clone(&self.chrome_alive),
            hooks: Arc::clone(&self.hooks),
            chat_id: chat_id.clone(),
            agent_id: team::MAIN.into(),
            agent_name: "main".into(),
            model: model.clone(),
            effort: reasoning_effort.clone(),
            context_window,
            compose,
            subagent_model: self.subagent_model.clone(),
            compact_percent: self.compact_percent,
        };
        let compact_percent = self.compact_percent;
        self.begin(move |tx| {
            let hooks = tool_context.hooks.read().map(|registry| registry.clone()).unwrap_or_default();
            let mut extra_developer: Vec<String> = Vec::new();
            if first_turn && hooks.has_hooks_for(HookEvent::SessionStart) {
                let outcome = hooks.run(HookEvent::SessionStart, None, &serde_json::json!({"session_id": chat_id}));
                extra_developer.extend(outcome.additional_context.into_iter().map(|note| format!("[SessionStart hook] {note}")));
            }
            if hooks.has_hooks_for(HookEvent::UserPromptSubmit) {
                let outcome = hooks.run(HookEvent::UserPromptSubmit, None, &serde_json::json!({"session_id": chat_id, "prompt": prompt}));
                if let Some(reason) = outcome.block {
                    let _ = tx.send(Err(format!("A UserPromptSubmit hook blocked this message: {reason}")));
                    return;
                }
                extra_developer.extend(outcome.additional_context.into_iter().map(|note| format!("[UserPromptSubmit hook] {note}")));
            }
            if let Some((skill, _)) = skills::match_slash(&prompt) {
                if let Ok(loaded) = skills::load(&skill.name) {
                    extra_developer.push(format!(
                        "[Skill /{}] The user invoked this skill. Follow it for this request.\n{}",
                        skill.name,
                        loaded.get("instructions").and_then(Value::as_str).unwrap_or("")
                    ));
                }
            }
            extra_developer.extend(skills::catalog_prompt(&skills::list()));
            let stop_hooks = hooks.clone();
            let stop_chat = chat_id.clone();
            let team_chat = chat_id.clone();
            let inbox_chat = chat_id.clone();
            let last_status = Arc::new(Mutex::new(String::new()));
            let options = chat::RunOptions {
                compose,
                extra_developer,
                hidden_tools: Vec::new(),
                worker: false,
                hold_for_team: Some(Box::new(move || tools::hold_for_team(&team_chat))),
                compact_percent,
                session_id: Some(format!("jarvus-{chat_id}")),
                inbox: Some(Box::new(move || tools::team_inbox(team::MAIN, &inbox_chat, &last_status))),
                on_stop: Some(Box::new(move |answer| {
                    if !stop_hooks.has_hooks_for(HookEvent::Stop) {
                        return None;
                    }
                    stop_hooks
                        .run(HookEvent::Stop, None, &serde_json::json!({"session_id": stop_chat, "last_assistant_message": answer}))
                        .block
                })),
            };
            let result = auth::token().and_then(|tokens| chat::respond_with_options(
                &tokens.access_token, tokens.account_id.as_deref(), &model, &reasoning_effort, context_window,
                &summary, &history, &prompt, &extra_tools,
                |name, arguments| tools::dispatch(&tool_context, name, arguments),
                options,
                |event| {
                    if let Some(delta) = event.delta { let _ = tx.send(Ok(CompletedOperation::ChatDelta(delta))); }
                    if let Some(name) = event.tool_name { let _ = tx.send(Ok(CompletedOperation::ToolStarted(name))); }
                    if let Some(activity) = event.tool_activity { let _ = tx.send(Ok(CompletedOperation::ToolActivity(activity))); }
                    if let Some(ms) = event.latency_ms { let _ = tx.send(Ok(CompletedOperation::ChatLatency(ms))); }
                    if let Some(usage) = event.usage { let _ = tx.send(Ok(CompletedOperation::ChatUsage(usage))); }
                    if let Some(compaction) = event.compaction { let _ = tx.send(Ok(CompletedOperation::ContextCompacted(compaction))); }
                    if let Some(error) = event.error { let _ = tx.send(Err(error)); }
                }
            ));
            match result {
                Ok(_) => { let _ = tx.send(Ok(CompletedOperation::ChatDone)); }
                Err(error) => { let _ = tx.send(Err(error)); }
            }
        }, "JEV is thinking…");
    }

    /// Tools offered on every request beyond the built-in toolset: the Shell package.
    /// Chrome and MCP server tools are not sent up front; the model loads them with
    /// `get_tools_` the first time it needs that connection.
    /// Tools offered on every request beyond the built-in toolset (Shell). Chrome and MCP
    /// server tools load on demand through `get_tools_`.
    fn dynamic_tool_snapshot(&self) -> (Vec<Value>, HashMap<String, (String, String)>) {
        (tools::core_extra_tools(), HashMap::new())
    }

    fn snapshot(&mut self, with_map: bool) {
        let surface = self.surface.clone();
        let app = self.target_app.trim().to_owned();
        let window_id = self.target_window_id.trim().to_owned();
        self.begin(
            move |tx| {
                let result = desktop::snapshot_target(
                    &surface,
                    (!app.is_empty()).then_some(app.as_str()),
                    (!window_id.is_empty()).then_some(window_id.as_str()),
                    true,
                    with_map,
                )
                .map(CompletedOperation::Snapshot);
                let _ = tx.send(result);
            },
            "Reading a fresh Agent Desktop snapshot…",
        );
    }

    fn click_ref(&mut self) {
        let reference = self.element_ref.clone();
        self.begin(
            move |tx| {
                let _ = tx.send(desktop::click(&reference).map(CompletedOperation::Action));
            },
            "Sending one user-approved semantic click…",
        );
    }

    fn refresh_apps(&mut self) {
        self.begin(
            |tx| {
                let _ = tx.send(desktop::list_apps().map(CompletedOperation::Apps));
            },
            "Reading running GUI apps with Agent Desktop…",
        );
    }

    fn refresh_windows(&mut self) {
        let app = self.target_app.trim().to_owned();
        self.begin(
            move |tx| {
                let result = desktop::list_windows((!app.is_empty()).then_some(app.as_str()))
                    .map(CompletedOperation::Windows);
                let _ = tx.send(result);
            },
            "Reading visible app windows…",
        );
    }

    fn launch_target(&mut self) {
        let app = self.target_app.trim().to_owned();
        if app.is_empty() {
            self.notice = "Enter an app name or process first.".into();
            return;
        }
        self.begin(
            move |tx| {
                let _ = tx.send(crate::jev::open_app(&app).map(CompletedOperation::Action));
            },
            "Opening or focusing the selected app…",
        );
    }

    fn take_local_screenshot(&mut self) {
        let app = self.target_app.trim().to_owned();
        let window_id = self.target_window_id.trim().to_owned();
        self.begin(
            move |tx| {
                let result = desktop::screenshot(
                    (!app.is_empty()).then_some(app.as_str()),
                    (!window_id.is_empty()).then_some(window_id.as_str()),
                )
                .map(CompletedOperation::Screenshot);
                let _ = tx.send(result);
            },
            "Saving an app screenshot locally…",
        );
    }

    fn new_chat(&mut self) {
        if self.streaming {
            return;
        }
        // Already on a fresh, empty chat: do nothing instead of piling up blank chats.
        if self.conversations.get(self.active_conversation).is_some_and(|chat| chat.turns.is_empty()) {
            return;
        }
        self.flush_history(true);
        if let Some(previous) = self.conversations.get(self.active_conversation) {
            self.inactive_unloads
                .insert(previous.id.clone(), Instant::now() + Duration::from_secs(5));
        }
        self.conversations.insert(0, new_conversation());
        self.active_conversation = 0;
        self.editing_turn = None;
        self.draft.clear();
        self.notice.clear();
        self.history_dirty = true;
        self.scroll_to_bottom = true;
        self.reset_context_for_active_chat();
        if let Some(chat) = self.conversations.get(self.active_conversation) {
            self.context_chat_id = chat.id.clone();
        }
        self.flush_history(true);
    }

    /// What the main chat's own text, the system prompt and the tool list add up to locally.
    fn main_text_estimate(&self) -> u64 {
        self.conversations
            .get(self.active_conversation)
            .filter(|conversation| !conversation.turns.is_empty())
            .map(|conversation| {
                let start = conversation.summary_through.min(conversation.turns.len());
                chat::estimate_context_tokens(&conversation.context_summary, &conversation.turns[start..], "")
                    .saturating_add(toolset_tokens_estimate())
                    .saturating_add(self.last_dynamic_tool_tokens)
            })
            .unwrap_or(0)
    }

    /// Every frame: the main chat's context size is the service-reported size of its last request
    /// plus whatever text has been added since (streamed replies, tool results, a new message),
    /// or the local estimate before any request has been reported.
    fn sync_main_context(&mut self) {
        let text = self.main_text_estimate();
        let reported = if self.context_usage_estimated { 0 } else { self.context_usage.input_tokens.saturating_add(self.context_usage.output_tokens) };
        let tokens = if reported > 0 { reported.saturating_add(text.saturating_sub(self.usage_estimate_base)) } else { text };
        self.context_tokens = tokens;
        if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
            if conversation.context_tokens != tokens {
                conversation.context_tokens = tokens;
                self.history_dirty = true;
            }
        }
    }

    /// Point the context gauge at the active chat: cleared at once (zero for an empty chat,
    /// otherwise an estimate from its own turns), never carrying numbers over from another chat.
    fn reset_context_for_active_chat(&mut self) {
        let tokens = self
            .conversations
            .get(self.active_conversation)
            .filter(|conversation| !conversation.turns.is_empty())
            .map(|conversation| {
                let start = conversation.summary_through.min(conversation.turns.len());
                chat::estimate_context_tokens(&conversation.context_summary, &conversation.turns[start..], "")
                    .saturating_add(toolset_tokens_estimate())
                    .saturating_add(self.last_dynamic_tool_tokens)
            })
            .unwrap_or(0);
        self.context_tokens = tokens;
        let latest = self
            .conversations
            .get(self.active_conversation)
            .and_then(|conversation| conversation.latest_usage.clone());
        self.context_usage = latest.clone().unwrap_or_else(|| chat::Usage {
            input_tokens: tokens,
            ..Default::default()
        });
        self.context_usage_estimated = latest.is_none();
        self.usage_estimate_base = tokens;
        self.context_snap_frames = 3;
    }

    fn activate_conversation(&mut self, index: usize) {
        if self.streaming || index >= self.conversations.len() || index == self.active_conversation
        {
            return;
        }
        self.flush_history(true);
        if let Some(previous) = self.conversations.get(self.active_conversation) {
            self.inactive_unloads
                .insert(previous.id.clone(), Instant::now() + Duration::from_secs(5));
        }
        self.active_conversation = index;
        if let Some(conversation) = self.conversations.get_mut(index) {
            self.inactive_unloads.remove(&conversation.id);
            if !conversation.loaded {
                load_conversation_body(conversation);
            }
        }
        self.reset_context_for_active_chat();
        self.editing_turn = None;
        self.scroll_to_bottom = true;
        self.history_dirty = true;
        self.flush_history(true);
    }

    fn unload_inactive_conversations(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .inactive_unloads
            .iter()
            .filter_map(|(id, deadline)| (*deadline <= now).then_some(id.clone()))
            .collect();
        for id in expired {
            if self
                .conversations
                .get(self.active_conversation)
                .is_some_and(|chat| chat.id == id)
                || self.streaming
            {
                self.inactive_unloads
                    .insert(id, now + Duration::from_secs(5));
                continue;
            }
            if let Some(conversation) = self.conversations.iter_mut().find(|chat| chat.id == id) {
                if conversation.loaded {
                    conversation.message_count = conversation.turns.len();
                    conversation.preview = conversation_preview(&conversation.turns);
                    conversation.turns.clear();
                    conversation.context_summary.clear();
                    conversation.summary_through = 0;
                    conversation.loaded = false;
                }
            }
            self.inactive_unloads.remove(&id);
        }
        if let Some(next) = self.inactive_unloads.values().min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }

    fn rename_chat(&mut self, index: usize) {
        if let Some(conversation) = self.conversations.get_mut(index) {
            let title = self
                .rename_draft
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !title.is_empty() {
                conversation.title = title.chars().take(64).collect();
                conversation.title_user_edited = true;
                conversation.title_generated = true;
            }
        }
        self.renaming_conversation = None;
        self.history_dirty = true;
        self.flush_history(true);
    }

    fn delete_chat(&mut self, index: usize) {
        if index >= self.conversations.len() || self.streaming {
            return;
        }
        let removed_id = self.conversations[index].id.clone();
        let active_id = self
            .conversations
            .get(self.active_conversation)
            .map(|conversation| conversation.id.clone());
        self.conversations.remove(index);
        self.inactive_unloads.remove(&removed_id);
        if let Err(error) = chat_store::delete_chat(&removed_id) {
            self.notice = format!("Could not remove chat files: {error}");
        }
        if self.conversations.is_empty() {
            let conversation = new_conversation();
            self.conversations.push(conversation);
        }
        self.active_conversation = active_id
            .and_then(|id| self.conversations.iter().position(|chat| chat.id == id))
            .unwrap_or_else(|| index.min(self.conversations.len() - 1));
        if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
            if !conversation.loaded {
                load_conversation_body(conversation);
            }
        }
        self.renaming_conversation = None;
        self.editing_turn = None;
        self.history_dirty = true;
        self.flush_history(true);
    }

    fn flush_history(&mut self, force: bool) {
        if !self.history_dirty
            || (!force && self.last_history_save.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        match save_conversation_store(&mut self.conversations, self.active_conversation) {
            Ok(()) => {
                self.history_dirty = false;
                self.last_history_save = Instant::now();
            }
            Err(error) => {
                self.notice = format!("Could not save chat history: {error}");
                self.last_history_save = Instant::now();
            }
        }
    }

    fn regenerate_response(&mut self) {
        if self.streaming {
            return;
        }
        let Some(conversation) = self.conversations.get_mut(self.active_conversation) else {
            return;
        };
        if conversation.turns.len() < 2 {
            return;
        }
        let assistant_is_last = conversation
            .turns
            .last()
            .is_some_and(|turn| turn.role == "assistant");
        let user_index = conversation
            .turns
            .iter()
            .rposition(|turn| turn.role == "user");
        let Some(user_index) = user_index else {
            return;
        };
        if !assistant_is_last {
            return;
        }
        self.draft = conversation.turns[user_index].content.clone();
        conversation.turns.truncate(user_index);
        self.send_chat();
    }

    fn edit_user_message(&mut self, index: usize) {
        if self.streaming {
            return;
        }
        let Some(turn) = self
            .conversations
            .get(self.active_conversation)
            .and_then(|conversation| conversation.turns.get(index))
        else {
            return;
        };
        if turn.role != "user" {
            return;
        }
        self.draft = turn.content.clone();
        self.editing_turn = Some(index);
        self.edit_generation = self.edit_generation.wrapping_add(1);
        self.notice = "Editing an earlier message · later turns will be replaced when sent.".into();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let mut keep_pending = false;
        let mut force_history_save = false;
        let mut title_receivers = std::mem::take(&mut self.title_pending);
        let mut remaining_titles = Vec::with_capacity(title_receivers.len());
        for receiver in title_receivers.drain(..) {
            match receiver.try_recv() {
                Ok((conversation_id, title)) => {
                    if let Some(conversation) = self
                        .conversations
                        .iter_mut()
                        .find(|item| item.id == conversation_id)
                    {
                        if !conversation.title_user_edited && !title.trim().is_empty() {
                            let new_title: String = title.chars().take(64).collect();
                            if new_title != conversation.title {
                                // The row morphs from the old name to the new one.
                                self.title_swaps.insert(conversation_id.clone(), conversation.title.clone());
                                self.ctx_title_swap_reset.push(conversation_id.clone());
                            }
                            conversation.title = new_title;
                            force_history_save = true;
                        }
                    }
                }
                Err(TryRecvError::Empty) => remaining_titles.push(receiver),
                Err(TryRecvError::Disconnected) => {}
            }
        }
        self.title_pending = remaining_titles;
        let receiver = self.pending.take();
        if let Some(receiver) = receiver.as_ref() {
            loop {
                let result = match receiver.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty) => {
                        keep_pending = true;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => break,
                };
                match result {
                    Err(error) => {
                        self.notice = error.clone();
                        if let Some(elapsed) = self.assistant_started_at.take().map(|started| {
                            started.elapsed().as_millis().min(u64::MAX as u128) as u64
                        }) {
                            if let Some(turn) = self
                                .conversations
                                .get_mut(self.active_conversation)
                                .and_then(|conversation| conversation.turns.last_mut())
                            {
                                if turn.role == "assistant" {
                                    turn.elapsed_ms = Some(elapsed);
                                }
                            }
                        }
                        self.finish_empty_assistant(format!(
                            "JEV could not finish this request: {error}"
                        ));
                        self.history_dirty = true;
                        self.streaming = false;
                        self.active_tool = None;
                        self.scroll_to_bottom = true;
                        force_history_save = true;
                    }
                    Ok(CompletedOperation::Status(value)) => {
                        self.desktop_json = pretty(&value);
                        self.desktop_status = desktop_status_summary(&value);
                    }
                    Ok(CompletedOperation::Snapshot(result)) => {
                        self.desktop_json = pretty(&result.json);
                        self.local_visualization =
                            result.local_visualization.map(|p| p.display().to_string());
                        let count = result
                            .json
                            .pointer("/data/ref_count")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        self.desktop_status = format!(
                            "Fresh UIA tree · {count} actionable refs · screenshot stays local"
                        );
                    }
                    Ok(CompletedOperation::Action(value)) => {
                        self.desktop_json = pretty(&value);
                        self.desktop_status =
                            "Click returned · inspect a fresh snapshot to verify the new state"
                                .into();
                    }
                    Ok(CompletedOperation::DeviceCode(url, code, error)) => {
                        self.verification_url = url;
                        self.device_code = Some(code);
                        self.notice = error.unwrap_or_else(|| {
                            "Complete the sign-in and allow ChatGPT plan usage in the browser."
                                .into()
                        });
                    }
                    Ok(CompletedOperation::SignedIn(account)) => {
                        self.signed_in = true;
                        self.account = account;
                        self.device_code = None;
                        self.notice = "ChatGPT connected · loading available models…".into();
                    }
                    Ok(CompletedOperation::SignedOut(message)) => {
                        self.signed_in = false;
                        self.account.clear();
                        self.model_list.clear();
                        self.notice = message;
                    }
                    Ok(CompletedOperation::Models(models, ms)) => {
                        self.model_list = models;
                        if !self
                            .model_list
                            .iter()
                            .any(|m| m.slug == self.selected_model)
                        {
                            self.selected_model = self
                                .model_list
                                .first()
                                .map(|m| m.slug.clone())
                                .unwrap_or_default();
                        }
                        if let Some(model) = self
                            .model_list
                            .iter()
                            .find(|model| model.slug == self.selected_model)
                        {
                            self.context_window_reported = model.context_window.is_some();
                            self.context_window =
                                model.context_window.unwrap_or(chat::DEFAULT_CONTEXT_WINDOW);
                        }
                        if let Some(model) = self
                            .model_list
                            .iter()
                            .find(|model| model.slug == self.selected_model)
                        {
                            if !model.supported_reasoning_levels.is_empty()
                                && !model
                                    .supported_reasoning_levels
                                    .contains(&self.reasoning_effort)
                            {
                                self.reasoning_effort = model.supported_reasoning_levels[0].clone();
                            }
                        }
                        let save_error =
                            save_selected_model(&self.selected_model, &self.reasoning_effort).err();
                        self.latency_ms = Some(ms);
                        self.signed_in = true;
                        self.notice = save_error.unwrap_or_else(|| {
                            format!(
                                "ChatGPT connected · {} models available",
                                self.model_list.len()
                            )
                        });
                    }
                    Ok(CompletedOperation::ChatDelta(delta)) => {
                        if let Some(last) = self
                            .conversations
                            .get_mut(self.active_conversation)
                            .and_then(|c| c.turns.last_mut())
                        {
                            last.content.push_str(&delta);
                        }
                        self.history_dirty = true;
                        self.active_tool = None;
                        self.scroll_to_bottom = true;
                    }
                    Ok(CompletedOperation::ToolStarted(name)) => {
                        if is_local_desktop_tool(&name) {
                            self.jev_action_started = Some(Instant::now());
                        }
                        self.active_tool = Some(name);
                        self.scroll_to_bottom = true;
                    }
                    Ok(CompletedOperation::ToolActivity(activity)) => {
                        if is_local_desktop_tool(&activity.name) {
                            if activity.state == "running" {
                                self.jev_action_started = Some(Instant::now());
                            } else if matches!(
                                activity.state.as_str(),
                                "complete" | "fresh_snapshot" | "refreshed" | "failed"
                            ) {
                                if let Some(started) = self.jev_action_started.take() {
                                    self.jev_latency_ms = Some(started.elapsed().as_millis());
                                }
                            }
                        }
                        self.active_tool = Some(activity.name.clone());
                        if let Some(turn) = self
                            .conversations
                            .get_mut(self.active_conversation)
                            .and_then(|conversation| conversation.turns.last_mut())
                        {
                            if turn.role == "assistant" {
                                chat::record_activity(&mut turn.tool_activity, activity);
                            }
                        }
                        self.history_dirty = true;
                        self.scroll_to_bottom = true;
                    }
                    Ok(CompletedOperation::ChatLatency(ms)) => {
                        self.latency_ms = Some(ms);
                    }
                    Ok(CompletedOperation::ChatUsage(usage)) => {
                        self.context_usage = usage.clone();
                        self.context_usage_estimated = false;
                        self.usage_estimate_base = self.main_text_estimate();
                        if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
                            conversation.latest_usage = Some(usage.clone());
                            conversation.total_usage.input_tokens = conversation.total_usage.input_tokens.saturating_add(usage.input_tokens);
                            conversation.total_usage.output_tokens = conversation.total_usage.output_tokens.saturating_add(usage.output_tokens);
                            conversation.total_usage.cached_tokens = conversation.total_usage.cached_tokens.saturating_add(usage.cached_tokens);
                            conversation.total_usage.cache_write_tokens = conversation.total_usage.cache_write_tokens.saturating_add(usage.cache_write_tokens);
                            conversation.total_usage.cache_reported |= usage.cache_reported;
                            conversation.total_usage.cache_write_reported |= usage.cache_write_reported;
                        }
                        self.history_dirty = true;
                    }
                    Ok(CompletedOperation::ContextCompacted(update)) => {
                        if let Some(conversation) =
                            self.conversations.get_mut(self.active_conversation)
                        {
                            conversation.context_summary = update.summary;
                            let end = conversation.turns.len().saturating_sub(2);
                            conversation.summary_through = conversation
                                .summary_through
                                .saturating_add(update.summarized_turns)
                                .min(end);
                        }
                        self.active_tool = None;
                        self.notice = "Context compacted automatically · older turns were summarized".into();
                        self.history_dirty = true;
                        force_history_save = true;
                    }
                    Ok(CompletedOperation::Compacted(result)) => match result {
                        Ok((summary, through)) => {
                            if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
                                conversation.context_summary = summary;
                                conversation.summary_through = through.min(conversation.turns.len());
                                // The next request's real size replaces the stale reported usage.
                                conversation.latest_usage = None;
                            }
                            self.reset_context_for_active_chat();
                            self.notice = "Context compacted · older turns were summarized".into();
                            self.history_dirty = true;
                            force_history_save = true;
                        }
                        Err(error) => self.notice = format!("Could not compact the context: {error}"),
                    },
                    Ok(CompletedOperation::Apps(value)) => {
                        self.desktop_json = pretty(&value);
                        self.desktop_status = "Agent Desktop app inventory refreshed".into();
                    }
                    Ok(CompletedOperation::Windows(value)) => {
                        self.desktop_json = pretty(&value);
                        self.desktop_status = "Agent Desktop window inventory refreshed".into();
                    }
                    Ok(CompletedOperation::Screenshot(value)) => {
                        self.desktop_json = pretty(&value);
                        self.desktop_status = "Screenshot saved locally on this PC".into();
                    }
                    Ok(CompletedOperation::BrowserSnapshot(value)) => {
                        self.desktop_json = pretty(&value);
                        let count = value
                            .pointer("/snapshot/controls")
                            .and_then(Value::as_array)
                            .map(Vec::len)
                            .unwrap_or(0);
                        self.desktop_status = format!(
                            "Chrome active-tab snapshot · {count} controls · page content remains local"
                        );
                        self.notice =
                            "Browser extension responded to an authorized active-tab snapshot."
                                .into();
                    }
                    Ok(CompletedOperation::ChatDone) => {
                        self.streaming = false;
                        if let Some(started) = self.jev_action_started.take() {
                            self.jev_latency_ms = Some(started.elapsed().as_millis());
                        }
                        if let Some(elapsed) = self.assistant_started_at.take().map(|started| {
                            started.elapsed().as_millis().min(u64::MAX as u128) as u64
                        }) {
                            if let Some(turn) = self
                                .conversations
                                .get_mut(self.active_conversation)
                                .and_then(|conversation| conversation.turns.last_mut())
                            {
                                if turn.role == "assistant" {
                                    turn.elapsed_ms = Some(elapsed);
                                }
                            }
                        }
                        let fallback = match self.active_tool.as_deref() {
                            Some("open_app") => "JEV opened the requested app, but ChatGPT did not return a text follow-up.".to_owned(),
                            Some("inspect_desktop") => "JEV inspected the desktop, but ChatGPT did not return a text follow-up.".to_owned(),
                            Some("list_processes") => "JEV checked the running apps, but ChatGPT did not return a text follow-up.".to_owned(),
                            Some(_) => "JEV completed a local step, but ChatGPT did not return a text follow-up.".to_owned(),
                            None => "ChatGPT completed without a visible response. Please regenerate the reply.".to_owned(),
                        };
                        self.finish_empty_assistant(fallback);
                        self.active_tool = None;
                        self.notice = "JEV replied · response completed".into();
                        self.history_dirty = true;
                        self.scroll_to_bottom = true;
                        force_history_save = true;
                    }
                    Ok(CompletedOperation::McpUpdated(message)) => {
                        self.notice = message;
                    }
                }
            }
        }
        if keep_pending {
            self.pending = receiver;
        }
        self.flush_history(force_history_save);
        self.unload_inactive_conversations(ctx);
        if self.pending.is_some() || self.streaming || !self.title_pending.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(35));
        }
    }

    /// Enter in the chat box: send now, or queue behind the run that is still working.
    fn submit_or_queue(&mut self) {
        if self.streaming || self.pending.is_some() {
            let text = self.draft.trim().to_owned();
            if text.is_empty() || self.editing_turn.is_some() {
                return;
            }
            let chat_id = self.active_chat_id();
            self.queued.push((chat_id, text));
            self.draft.clear();
            self.scroll_to_bottom = true;
        } else {
            self.send_chat();
        }
    }

    /// Once the run is over, send the oldest queued message of this chat (the box keeps what is being typed).
    fn drain_queue(&mut self) {
        if self.queued.is_empty() || self.streaming || self.pending.is_some() || !self.signed_in || self.editing_turn.is_some() || self.selected_model.is_empty() {
            return;
        }
        let chat_id = self.active_chat_id();
        let Some(index) = self.queued.iter().position(|(chat, _)| *chat == chat_id) else { return };
        let (_, text) = self.queued.remove(index);
        let typing = std::mem::take(&mut self.draft);
        self.draft = text.clone();
        self.send_chat();
        if !self.draft.is_empty() {
            // refused (for example a background operation just started): keep it queued
            self.queued.insert(index, (chat_id, text));
        }
        self.draft = typing;
    }

    /// Send a queued message into the run that is already working, as a steering note.
    fn steer_now(&mut self, chat_id: &str, text: String) {
        team::hub().post_to_main(chat_id, "user, steering the run", &text);
        if let Some(turn) = self.conversations.get_mut(self.active_conversation).and_then(|c| c.turns.last_mut()) {
            if turn.role == "assistant" {
                turn.content.push_str(&format!("\n\n> **You steered:** {}\n\n", text.replace('\n', " ")));
            }
        }
        self.history_dirty = true;
        self.scroll_to_bottom = true;
    }

    /// Queued messages, shown as dimmed bubbles at the bottom right of the transcript.
    fn queued_bubbles(&mut self, ui: &mut egui::Ui, chat_id: &str) {
        let mine: Vec<(usize, String)> = self.queued.iter().enumerate().filter(|(_, (chat, _))| chat == chat_id).map(|(i, (_, text))| (i, text.clone())).collect();
        let mut steer: Option<usize> = None;
        let mut remove: Option<usize> = None;
        for (index, text) in &mine {
            ui.add_space(10.0);
            ui.horizontal(|row| {
                row.with_layout(egui::Layout::right_to_left(egui::Align::Min), |row| {
                    egui::Frame::new().fill(Color32::from_rgb(25, 27, 25)).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(16))
                        .inner_margin(Margin { left: 15, right: 15, top: 10, bottom: 8 }).show(row, |bubble| {
                            bubble.set_max_width((row_width_cap(bubble)).min(520.0));
                            // The row is right-to-left; the bubble's own content reads normally.
                            bubble.vertical(|bubble| {
                                bubble.add(egui::Label::new(egui::RichText::new(text).color(MUTED)).wrap());
                                bubble.add_space(2.0);
                                bubble.horizontal(|line| {
                                    line.label(egui::RichText::new("Queued").size(10.0).color(MINT_DEEP));
                                    if self.streaming && line.small_button("Steer now").on_hover_text("Hand this to the running agent immediately").clicked() { steer = Some(*index); }
                                    if line.small_button("Remove").clicked() { remove = Some(*index); }
                                });
                            });
                        });
                });
            });
        }
        if let Some(index) = steer.or(remove) {
            let (chat, text) = self.queued.remove(index);
            if steer.is_some() {
                self.steer_now(&chat, text);
            }
        }
    }

    fn run_builtin(&mut self, command: &str) {
        match command {
            "compact" => self.compact_now(),
            "compose" => self.compose_mode = !self.compose_mode,
            "new" => self.new_chat(),
            _ => {}
        }
    }

    /// `/compact`: summarize everything but the last exchange into the chat's running memory.
    fn compact_now(&mut self) {
        if self.streaming || self.pending.is_some() || !self.signed_in {
            return;
        }
        let Some(conversation) = self.conversations.get(self.active_conversation) else { return };
        let start = conversation.summary_through.min(conversation.turns.len());
        let through = conversation.turns.len().saturating_sub(2).max(start);
        if through <= start {
            self.notice = "Nothing to compact yet.".into();
            return;
        }
        let turns = conversation.turns[start..through].to_vec();
        let summary = conversation.context_summary.clone();
        let session = format!("jarvus-{}", conversation.id);
        let model = self.selected_model.clone();
        self.begin(
            move |tx| {
                let result = auth::token()
                    .and_then(|tokens| chat::compact_conversation(&tokens.access_token, tokens.account_id.as_deref(), &model, &summary, Some(&session), &turns))
                    .map(|summary| (summary, through));
                let _ = tx.send(Ok(CompletedOperation::Compacted(result)));
            },
            "Compacting context…",
        );
    }

    /// Stop the running turn now: the worker is told to quit at its next safe point and the
    /// UI detaches from it so a new message can be sent immediately.
    fn cancel_run(&mut self) {
        crate::jev::request_cancel();
        self.pending = None;
        self.streaming = false;
        self.active_tool = None;
        self.jev_action_started = None;
        if let Some(elapsed) = self
            .assistant_started_at
            .take()
            .map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64)
        {
            if let Some(turn) = self
                .conversations
                .get_mut(self.active_conversation)
                .and_then(|c| c.turns.last_mut())
            {
                if turn.role == "assistant" {
                    turn.elapsed_ms = Some(elapsed);
                }
            }
        }
        for turn in self
            .conversations
            .get_mut(self.active_conversation)
            .map(|c| c.turns.iter_mut().rev().take(1))
            .into_iter()
            .flatten()
        {
            for activity in turn
                .tool_activity
                .iter_mut()
                .filter(|a| a.state == "running")
            {
                activity.state = "failed".into();
                activity.result_summary = "Stopped by you.".into();
            }
        }
        self.finish_empty_assistant("Stopped.".into());
        self.notice = "Run stopped.".into();
        self.history_dirty = true;
        self.flush_history(true);
    }

    fn finish_empty_assistant(&mut self, message: String) {
        let Some(turn) = self
            .conversations
            .get_mut(self.active_conversation)
            .and_then(|conversation| conversation.turns.last_mut())
        else {
            return;
        };
        if turn.role == "assistant" && turn.content.trim().is_empty() {
            turn.content = message;
        }
    }

    fn history_panel(&mut self, ui: &mut egui::Ui) {
        self.sync_agents_for_active_chat();
        let agents = team::hub().list(&self.active_chat_id());
        if agents.is_empty() && self.side_tab != SideTab::Subagents {
            self.open_agent = None;
        } else {
            ui.vertical(|ui| self.side_tabs(ui, &agents));
        }
        if self.side_tab == SideTab::Subagents {
            let remaining = ui.available_rect_before_wrap();
            let mut inner = ui.new_child(UiBuilder::new().max_rect(remaining));
            self.agents_panel(&mut inner, agents);
            return;
        }
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("History")
                        .size(16.0)
                        .strong()
                        .color(INK),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, "plus", "New chat", 30.0).clicked() {
                        self.new_chat();
                    }
                });
            });
            ui.label(
                egui::RichText::new(format!("{} conversations", self.conversations.len()))
                    .size(11.0)
                    .color(MUTED),
            );
            ui.add_space(10.0);
            let list_height = ui.available_height().max(90.0);
            egui::ScrollArea::vertical()
                .id_salt("conversation-history")
                .max_height(list_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut delete_index = None;
                    for index in 0..self.conversations.len() {
                        let selected = index == self.active_conversation;
                        let title = self.conversations[index].title.clone();
                        let count = self.conversations[index]
                            .message_count
                            .max(self.conversations[index].turns.len());
                        let preview = if self.conversations[index].loaded {
                            conversation_preview(&self.conversations[index].turns)
                        } else {
                            self.conversations[index].preview.clone()
                        };
                        let row_width = ui.available_width().max(80.0);
                        if self.renaming_conversation == Some(index) {
                            ui.horizontal(|ui| {
                                let edit = ui.add_sized(
                                    [row_width - 40.0, 34.0],
                                    egui::TextEdit::singleline(&mut self.rename_draft)
                                        .hint_text(egui::RichText::new("Chat name").color(MUTED)),
                                );
                                if !edit.has_focus() && !edit.lost_focus() {
                                    edit.request_focus();
                                }
                                let commit = icon_button(ui, "check", "Save name", 30.0).clicked()
                                    || (edit.lost_focus()
                                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                                if commit {
                                    self.rename_chat(index);
                                }
                                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                                    self.renaming_conversation = None;
                                }
                            });
                            ui.add_space(6.0);
                            continue;
                        }
                        let (row_rect, response) =
                            ui.allocate_exact_size(Vec2::new(row_width, 58.0), Sense::click());
                        // The action buttons sit on top of the row and take hover away from it,
                        // so hover is the pointer being anywhere inside the row rectangle.
                        let pointer_in_row = ui
                            .input(|input| input.pointer.hover_pos())
                            .is_some_and(|pos| row_rect.contains(pos));
                        let hover = ui.ctx().animate_bool_with_time(
                            ui.make_persistent_id(("history-hover", index)),
                            pointer_in_row,
                            0.15,
                        );
                        let fill = if selected {
                            MINT
                        } else {
                            lerp_color(SURFACE, RAISED, hover)
                        };
                        ui.painter()
                            .rect_filled(row_rect, CornerRadius::same(11), fill);
                        if selected {
                            let edge = Rect::from_min_size(
                                pos2(row_rect.left(), row_rect.top() + 14.0),
                                Vec2::new(3.0, row_rect.height() - 28.0),
                            );
                            ui.painter()
                                .rect_filled(edge, CornerRadius::same(2), HOVER_GREEN);
                        }
                        let text_width = row_rect.width() - 26.0;
                        let chat_key = self.conversations[index].id.clone();
                        let swap_id = ui.make_persistent_id(("title-swap", chat_key.clone()));
                        if let Some(position) = self.ctx_title_swap_reset.iter().position(|id| *id == chat_key) {
                            self.ctx_title_swap_reset.swap_remove(position);
                            ui.ctx().animate_value_with_time(swap_id, 0.0, 0.0);
                        }
                        let swap_old = self.title_swaps.get(&chat_key).cloned();
                        let swap_t = if swap_old.is_some() { ui.ctx().animate_value_with_time(swap_id, 1.0, ANIM) } else { 1.0 };
                        if swap_old.is_some() && swap_t >= 1.0 {
                            self.title_swaps.remove(&chat_key);
                        }
                        let title_pos = pos2(row_rect.left() + 13.0, row_rect.top() + 9.0);
                        let draw_title = |ui: &egui::Ui, text: &str, offset: f32, alpha: f32| {
                            let mut job = egui::text::LayoutJob::simple_singleline(
                                text.to_owned(),
                                egui::FontId::proportional(12.5),
                                INK,
                            );
                            job.wrap = egui::text::TextWrapping::truncate_at_width(text_width);
                            let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                            ui.painter().galley(title_pos + Vec2::new(0.0, offset), galley, INK.gamma_multiply(alpha));
                        };
                        match swap_old {
                            // Old name slides up and fades; the new one rises into place.
                            Some(old) if swap_t < 1.0 => {
                                draw_title(ui, &old, -7.0 * swap_t, 1.0 - swap_t);
                                draw_title(ui, &title, 7.0 * (1.0 - swap_t), swap_t);
                            }
                            _ => draw_title(ui, &title, 0.0, 1.0),
                        }
                        let subtitle = if count == 0 {
                            "Ready for a task".to_owned()
                        } else if preview.is_empty() {
                            format!("{count} messages")
                        } else {
                            format!("{count} · {preview}")
                        };
                        let mut sub_job = egui::text::LayoutJob::simple_singleline(
                            subtitle,
                            egui::FontId::proportional(10.5),
                            MUTED,
                        );
                        sub_job.wrap = egui::text::TextWrapping::truncate_at_width(text_width);
                        ui.painter().galley(
                            pos2(row_rect.left() + 13.0, row_rect.top() + 31.0),
                            ui.fonts_mut(|fonts| fonts.layout_job(sub_job)),
                            MUTED,
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::Role::Button, true, &title)
                        });
                        if response.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        // Row actions float on top of the preview, vertically centered,
                        // over a fade so the title stays legible underneath.
                        let armed = self.pending_chat_delete == Some(index);
                        let mut action_clicked = false;
                        if hover > 0.01 || armed {
                            let strip = Rect::from_min_max(
                                pos2(row_rect.right() - 96.0, row_rect.top()),
                                row_rect.max,
                            );
                            let fade = Rect::from_min_max(
                                pos2(strip.left() - 26.0, row_rect.top()),
                                pos2(strip.left(), row_rect.bottom()),
                            );
                            let base = if selected { MINT } else { RAISED };
                            let mut mesh = egui::Mesh::default();
                            let clear =
                                Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 0);
                            mesh.colored_vertex(fade.left_top(), clear);
                            mesh.colored_vertex(
                                fade.right_top(),
                                base.gamma_multiply(hover.max(armed as u8 as f32)),
                            );
                            mesh.colored_vertex(
                                fade.right_bottom(),
                                base.gamma_multiply(hover.max(armed as u8 as f32)),
                            );
                            mesh.colored_vertex(fade.left_bottom(), clear);
                            mesh.add_triangle(0, 1, 2);
                            mesh.add_triangle(0, 2, 3);
                            ui.painter().add(egui::Shape::mesh(mesh));
                            ui.painter().rect_filled(
                                strip,
                                CornerRadius {
                                    nw: 0,
                                    sw: 0,
                                    ne: 11,
                                    se: 11,
                                },
                                base.gamma_multiply(hover.max(armed as u8 as f32)),
                            );
                            let mut x = strip.right() - 10.0;
                            for (icon, hint) in [
                                (
                                    "trash",
                                    if armed {
                                        "Click again to delete this chat"
                                    } else {
                                        "Delete chat"
                                    },
                                ),
                                ("edit", "Rename chat"),
                            ] {
                                let rect = Rect::from_center_size(
                                    pos2(x - 14.0, row_rect.center().y),
                                    Vec2::splat(28.0),
                                );
                                x -= 34.0;
                                let button = ui.interact(
                                    rect,
                                    ui.make_persistent_id(("history-action", index, icon)),
                                    Sense::click(),
                                );
                                let hot = button.hovered();
                                if hot {
                                    ui.painter().rect_filled(
                                        rect,
                                        CornerRadius::same(8),
                                        if icon == "trash" && armed {
                                            Color32::from_rgb(70, 28, 24)
                                        } else {
                                            HOVER_FILL
                                        },
                                    );
                                }
                                let color = if icon == "trash" && armed {
                                    DANGER
                                } else if hot {
                                    HOVER_GREEN
                                } else {
                                    INK
                                };
                                paint_icon(
                                    ui.painter(),
                                    rect,
                                    icon,
                                    color.gamma_multiply(hover.max(armed as u8 as f32)),
                                );
                                button.widget_info(|| {
                                    egui::WidgetInfo::labeled(egui::Role::Button, true, hint)
                                });
                                let clicked = button.clicked();
                                let _ = button.on_hover_text(hint);
                                if clicked {
                                    action_clicked = true;
                                    match icon {
                                        "edit" => {
                                            self.renaming_conversation = Some(index);
                                            self.rename_draft = title.clone();
                                            self.pending_chat_delete = None;
                                        }
                                        _ if armed => {
                                            delete_index = Some(index);
                                            self.pending_chat_delete = None;
                                        }
                                        _ => {
                                            self.pending_chat_delete = Some(index);
                                        }
                                    }
                                }
                            }
                        }
                        if armed
                            && !pointer_in_row
                            && ui.input(|input| input.pointer.any_click())
                            && !action_clicked
                        {
                            self.pending_chat_delete = None;
                        }
                        if response.clicked() && !action_clicked && !self.streaming {
                            self.activate_conversation(index);
                        }
                        ui.add_space(6.0);
                    }
                    if let Some(index) = delete_index {
                        self.delete_chat(index);
                    }
                });
        });
    }

    fn settings_modal(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let available = ctx.content_rect().size();
        let modal_width = (available.x * 0.9).clamp(640.0, 1000.0);
        let modal_height = (available.y * 0.88).clamp(420.0, 700.0);
        let modal_id = egui::Id::unique("jarvus-settings-modal");
        let response = egui::Modal::new(modal_id)
            .backdrop_color(Color32::from_black_alpha(176))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0, LINE))
                    .corner_radius(CornerRadius::same(18))
                    .shadow(egui::Shadow {
                        offset: [0, 24],
                        blur: 70,
                        spread: 0,
                        color: Color32::from_black_alpha(190),
                    })
                    .inner_margin(Margin::same(0)),
            )
            .show(ctx, |ui| {
                ui.set_min_size(Vec2::new(modal_width, modal_height));
                ui.set_max_size(Vec2::new(modal_width, modal_height));
                let full = ui.max_rect();
                let header = Rect::from_min_size(full.min, Vec2::new(full.width(), 64.0));
                // Title block vertically centered in the header.
                ui.painter().text(
                    pos2(header.left() + 24.0, header.center().y - 8.0),
                    egui::Align2::LEFT_CENTER,
                    "Settings",
                    egui::FontId::proportional(17.0),
                    INK,
                );
                ui.painter().text(
                    pos2(header.left() + 24.0, header.center().y + 11.0),
                    egui::Align2::LEFT_CENTER,
                    "Account, connections, tools and local data",
                    egui::FontId::proportional(11.0),
                    MUTED,
                );
                let mut close_ui = ui.new_child(UiBuilder::new().max_rect(Rect::from_center_size(
                    pos2(header.right() - 30.0, header.center().y),
                    Vec2::splat(32.0),
                )));
                if icon_button(&mut close_ui, "cancel", "Close settings", 32.0).clicked() {
                    self.show_settings = false;
                }
                ui.painter().line_segment(
                    [
                        pos2(full.left(), header.bottom()),
                        pos2(full.right(), header.bottom()),
                    ],
                    egui::Stroke::new(1.0, LINE),
                );

                let body = Rect::from_min_max(pos2(full.left(), header.bottom()), full.max);
                let nav_rect = Rect::from_min_size(body.min, Vec2::new(206.0, body.height()));
                ui.painter().line_segment(
                    [
                        pos2(nav_rect.right(), body.top()),
                        pos2(nav_rect.right(), body.bottom()),
                    ],
                    egui::Stroke::new(1.0, LINE),
                );
                let mut nav = ui.new_child(
                    UiBuilder::new()
                        .layout(egui::Layout::top_down(egui::Align::Min))
                        .max_rect(nav_rect.shrink2(Vec2::new(12.0, 14.0))),
                );
                for section in SettingsSection::ALL {
                    let selected = self.settings_section == section;
                    let (rect, response) = nav.allocate_exact_size(
                        Vec2::new(nav.available_width(), 38.0),
                        Sense::click(),
                    );
                    let hover = nav.ctx().animate_bool_with_time(
                        nav.make_persistent_id(("settings-nav", section.title())),
                        response.hovered() || selected,
                        0.14,
                    );
                    if hover > 0.0 {
                        nav.painter().rect_filled(
                            rect,
                            CornerRadius::same(10),
                            lerp_color(SURFACE, if selected { MINT } else { RAISED }, hover),
                        );
                    }
                    if selected {
                        nav.painter().rect_filled(
                            Rect::from_min_size(
                                pos2(rect.left(), rect.top() + 10.0),
                                Vec2::new(3.0, 18.0),
                            ),
                            CornerRadius::same(2),
                            HOVER_GREEN,
                        );
                    }
                    let color = if selected {
                        INK
                    } else if response.hovered() {
                        HOVER_GREEN
                    } else {
                        MUTED
                    };
                    paint_icon(
                        nav.painter(),
                        Rect::from_center_size(
                            pos2(rect.left() + 22.0, rect.center().y),
                            Vec2::splat(18.0),
                        ),
                        section.icon(),
                        color,
                    );
                    nav.painter().text(
                        pos2(rect.left() + 40.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        section.title(),
                        egui::FontId::proportional(12.5),
                        color,
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::Role::Button,
                            true,
                            selected,
                            section.title(),
                        )
                    });
                    if response.hovered() {
                        nav.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        self.settings_section = section;
                        if section == SettingsSection::Storage {
                            self.refresh_storage_inventory();
                        }
                    }
                    nav.add_space(3.0);
                }
                let status_rect = Rect::from_min_size(
                    pos2(nav_rect.left() + 14.0, nav_rect.bottom() - 34.0),
                    Vec2::new(nav_rect.width() - 28.0, 22.0),
                );
                let (dot, label) = if self.pending.is_some() {
                    (INK, "JEV working")
                } else {
                    (HOVER_GREEN, "JEV ready")
                };
                ui.painter().circle_filled(
                    pos2(status_rect.left() + 5.0, status_rect.center().y),
                    3.5,
                    dot,
                );
                ui.painter().text(
                    pos2(status_rect.left() + 16.0, status_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(11.0),
                    MUTED,
                );

                let content_rect =
                    Rect::from_min_max(pos2(nav_rect.right() + 1.0, body.top()), body.max);
                let mut pane = ui.new_child(
                    UiBuilder::new()
                        .layout(egui::Layout::top_down(egui::Align::Min))
                        .max_rect(content_rect),
                );
                egui::ScrollArea::vertical()
                    .id_salt("settings-content")
                    .auto_shrink([false, false])
                    .show(&mut pane, |content| {
                        egui::Frame::new()
                            .inner_margin(Margin {
                                left: 26,
                                right: 26,
                                top: 20,
                                bottom: 24,
                            })
                            .show(content, |content| {
                                content.set_width(content.available_width());
                                self.settings_section_ui(content);
                                if !self.notice.trim().is_empty() {
                                    content.add_space(4.0);
                                    let lower = self.notice.to_ascii_lowercase();
                                    let is_error = [
                                        "failed",
                                        "error",
                                        "could not",
                                        "unable",
                                        "invalid",
                                        "not valid",
                                    ]
                                    .iter()
                                    .any(|word| lower.contains(word));
                                    egui::Frame::new()
                                        .fill(if is_error {
                                            Color32::from_rgb(40, 22, 20)
                                        } else {
                                            RAISED
                                        })
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            if is_error {
                                                Color32::from_rgb(90, 40, 34)
                                            } else {
                                                LINE
                                            },
                                        ))
                                        .corner_radius(CornerRadius::same(10))
                                        .inner_margin(Margin::same(10))
                                        .show(content, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&self.notice)
                                                        .size(11.0)
                                                        .color(if is_error {
                                                            DANGER
                                                        } else {
                                                            MUTED
                                                        }),
                                                )
                                                .wrap(),
                                            );
                                        });
                                }
                            });
                    });
            });
        if response.should_close() {
            self.show_settings = false;
        }
        self.review_chats_modal(ctx);
    }

    fn settings_section_ui(&mut self, ui: &mut egui::Ui) {
        let (title, description) = match self.settings_section {
            SettingsSection::Chat => (
                "Chat & model",
                "Your ChatGPT account and the model JEV plans with.",
            ),
            SettingsSection::Usage => (
                "Usage",
                "Service-reported numbers are labeled; estimates are kept separate.",
            ),
            SettingsSection::Connections => (
                "Connections",
                "Live status of ChatGPT, the desktop bridge, Chrome and MCP.",
            ),
            SettingsSection::Mcp => (
                "MCP servers",
                "Tools from connected stdio and HTTP MCP servers are offered to JEV automatically.",
            ),
            SettingsSection::Browser => (
                "Browser",
                "The Chrome extension lets JEV read and act on browser tabs.",
            ),
            SettingsSection::Agents => (
                "Agents, skills & hooks",
                "Which model workers use, the skills JEV can load, and scripts that run around tool calls.",
            ),
            SettingsSection::Storage => ("Storage", "What JARVUS keeps on this PC, and cleanup."),
        };
        ui.label(egui::RichText::new(title).size(20.0).strong().color(INK));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(description).size(11.5).color(MUTED));
        ui.add_space(18.0);
        match self.settings_section {
            SettingsSection::Chat => {
                settings_group(ui, Some("Account"), |ui| {
                    let state = if self.signed_in {
                        format!(
                            "Connected as {}",
                            if self.account.is_empty() {
                                "your ChatGPT account"
                            } else {
                                &self.account
                            }
                        )
                    } else if self.pending.is_some() {
                        "Connecting…".into()
                    } else {
                        "Not connected".into()
                    };
                    settings_row(
                        ui,
                        Some(if self.signed_in { HOVER_GREEN } else { DANGER }),
                        "ChatGPT",
                        &state,
                        |ui| {
                            if self.signed_in {
                                if ui
                                    .add_enabled_ui(self.pending.is_none(), |ui| {
                                        action_button(ui, "cancel", "Disconnect", false)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.sign_out();
                                }
                            } else if ui
                                .add_enabled_ui(self.pending.is_none(), |ui| {
                                    action_button(ui, "link", "Connect", true)
                                })
                                .inner
                                .clicked()
                            {
                                self.sign_in();
                            }
                        },
                    );
                    if let Some(code) = self.device_code.clone() {
                        row_divider(ui);
                        settings_row(ui, None, "Device sign-in code", &code, |ui| {
                            if action_button(ui, "globe", "Open page", false).clicked() {
                                if let Err(error) = auth::open_verification_page() {
                                    self.notice = error;
                                }
                            }
                            if action_button(ui, "copy", "Copy code", false).clicked() {
                                ui.ctx().copy_text(code.clone());
                                self.notice = "Device code copied.".into();
                            }
                        });
                    }
                });
                settings_group(ui, Some("Model"), |ui| {
                    let detail = self
                        .model_list
                        .iter()
                        .find(|model| model.slug == self.selected_model)
                        .map(|model| {
                            format!(
                                "{} context{}",
                                format_tokens(
                                    model.context_window.unwrap_or(chat::DEFAULT_CONTEXT_WINDOW)
                                ),
                                if model.context_window.is_some() {
                                    ""
                                } else {
                                    " (fallback)"
                                }
                            )
                        })
                        .unwrap_or_else(|| "Connect ChatGPT to load models".into());
                    settings_row(ui, None, "Model and thinking", &detail, |ui| {
                        self.model_selector(ui);
                    });
                    row_divider(ui);
                    settings_row(
                        ui,
                        None,
                        "Auto-compact context",
                        &format!("Summarize older turns once a request would use {}% of the window (15–95%). /compact does it now.", self.compact_percent),
                        |ui| {
                            let mut percent = self.compact_percent;
                            let changed = ui.add(egui::Slider::new(&mut percent, chat::MIN_COMPACT_PERCENT..=chat::MAX_COMPACT_PERCENT).suffix("%")).changed();
                            if changed {
                                self.compact_percent = chat::clamp_compact_percent(percent);
                                if let Err(error) = save_compact_percent(self.compact_percent) {
                                    self.notice = error;
                                }
                            }
                        },
                    );
                    row_divider(ui);
                    settings_row(
                        ui,
                        None,
                        "Model list",
                        &format!("{} models available", self.model_list.len()),
                        |ui| {
                            if ui
                                .add_enabled_ui(self.signed_in && self.pending.is_none(), |ui| {
                                    action_button(ui, "refresh", "Refresh", false)
                                })
                                .inner
                                .clicked()
                            {
                                self.refresh_models();
                            }
                        },
                    );
                });
            }
            SettingsSection::Usage => {
                let usage_kind = if self.context_usage_estimated {
                    "estimate"
                } else {
                    "reported"
                };
                settings_group(ui, Some("Latest request"), |ui| {
                    settings_row(
                        ui,
                        None,
                        "Input tokens",
                        &format!(
                            "{} · {usage_kind}",
                            format_tokens(self.context_usage.input_tokens)
                        ),
                        |_| {},
                    );
                    row_divider(ui);
                    settings_row(
                        ui,
                        None,
                        "Output tokens",
                        &format!(
                            "{} · {usage_kind}",
                            format_tokens(self.context_usage.output_tokens)
                        ),
                        |_| {},
                    );
                    row_divider(ui);
                    let cache = if self.context_usage_estimated {
                        "Not reported for estimates".into()
                    } else {
                        format!(
                            "cached {} · writes {}",
                            if self.context_usage.cache_reported {
                                format_tokens(self.context_usage.cached_tokens)
                            } else {
                                "n/a".into()
                            },
                            if self.context_usage.cache_write_reported {
                                format_tokens(self.context_usage.cache_write_tokens)
                            } else {
                                "n/a".into()
                            }
                        )
                    };
                    settings_row(ui, None, "Prompt cache", &cache, |_| {});
                    row_divider(ui);
                    settings_row(
                        ui,
                        None,
                        "Context window",
                        &format!(
                            "{}{}",
                            format_tokens(self.context_window),
                            if self.context_window_reported {
                                ""
                            } else {
                                " (fallback)"
                            }
                        ),
                        |_| {},
                    );
                });
                settings_group(ui, Some("Timing"), |ui| {
                    settings_row(
                        ui,
                        None,
                        "ChatGPT round trip",
                        &self
                            .latency_ms
                            .map(|ms| format!("{ms} ms"))
                            .unwrap_or_else(|| "Not measured yet".into()),
                        |_| {},
                    );
                    row_divider(ui);
                    settings_row(
                        ui,
                        Some(HOVER_GREEN),
                        "JEV local action",
                        &self
                            .jev_latency_ms
                            .map(|ms| format!("{ms} ms"))
                            .unwrap_or_else(|| "Not measured yet".into()),
                        |_| {},
                    );
                });
            }
            SettingsSection::Connections => {
                let (configs, connected) = self.mcp_snapshot();
                let chrome_alive = self.chrome_alive.load(Ordering::Relaxed);
                settings_group(ui, None, |ui| {
                    settings_row(
                        ui,
                        Some(if self.signed_in { HOVER_GREEN } else { DANGER }),
                        "ChatGPT",
                        &if self.signed_in {
                            format!("Connected · {}", self.account)
                        } else {
                            "Disconnected".into()
                        },
                        |ui| {
                            if !self.signed_in
                                && ui
                                    .add_enabled_ui(self.pending.is_none(), |ui| {
                                        action_button(ui, "link", "Connect", true)
                                    })
                                    .inner
                                    .clicked()
                            {
                                self.sign_in();
                            }
                        },
                    );
                    row_divider(ui);
                    settings_row(
                        ui,
                        Some(if self.desktop_status.starts_with("Bridge connected") {
                            HOVER_GREEN
                        } else {
                            MUTED
                        }),
                        "Desktop control",
                        &self.desktop_status.clone(),
                        |ui| {
                            if ui
                                .add_enabled_ui(self.pending.is_none(), |ui| {
                                    action_button(ui, "refresh", "Check", false)
                                })
                                .inner
                                .clicked()
                            {
                                self.begin(
                                    |tx| {
                                        let _ = tx.send(
                                            desktop::permission_status()
                                                .map(CompletedOperation::Status)
                                                .map_err(|error| error.to_string()),
                                        );
                                    },
                                    "Checking the desktop bridge…",
                                );
                            }
                            #[cfg(target_os = "macos")]
                            {
                                if ui
                                    .add_enabled_ui(self.pending.is_none(), |ui| {
                                        action_button(ui, "shield-check", "Request access", false)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.begin(
                                        |tx| {
                                            let _ = tx.send(
                                                desktop::request_permissions()
                                                    .map(CompletedOperation::Status)
                                                    .map_err(|error| error.to_string()),
                                            );
                                        },
                                        "Opening macOS accessibility permission request…",
                                    );
                                }
                                if ui
                                    .add_enabled_ui(self.pending.is_none(), |ui| {
                                        action_button(ui, "book-open", "Permission guide", false)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.begin(
                                        |tx| {
                                            let _ = tx.send(
                                                desktop::permissions_guide()
                                                    .map(CompletedOperation::Status)
                                                    .map_err(|error| error.to_string()),
                                            );
                                        },
                                        "Opening Agent Desktop’s macOS permission guide…",
                                    );
                                }
                            }
                        },
                    );
                    row_divider(ui);
                    let shell_tools = crate::shell::tool_specs().as_array().map_or(0, Vec::len);
                    settings_row(
                        ui,
                        Some(HOVER_GREEN),
                        "Shell",
                        &format!("Built-in local tools · {shell_tools} available"),
                        |_| {},
                    );
                    row_divider(ui);
                    let (dot, text) = if chrome_alive {
                        (
                            HOVER_GREEN,
                            "Extension connected · JEV can use your Chrome tabs".to_owned(),
                        )
                    } else {
                        (DANGER, "Extension not connected · open Chrome with the JARVUS extension installed".to_owned())
                    };
                    settings_row(ui, Some(dot), "Chrome extension", &text, |ui| {
                        if action_button(ui, "globe", "Browser", false).clicked() {
                            self.settings_section = SettingsSection::Browser;
                        }
                    });
                    row_divider(ui);
                    settings_row(
                        ui,
                        Some(if connected.is_empty() {
                            MUTED
                        } else {
                            HOVER_GREEN
                        }),
                        "MCP servers",
                        &format!("{} of {} connected", connected.len(), configs.len()),
                        |ui| {
                            if action_button(ui, "plug", "Manage", false).clicked() {
                                self.settings_section = SettingsSection::Mcp;
                            }
                        },
                    );
                });
            }
            SettingsSection::Mcp => self.mcp_settings_ui(ui),
            SettingsSection::Agents => self.agents_settings_ui(ui),
            SettingsSection::Browser => {
                let relay = browser::load_relay_config();
                settings_group(ui, Some("Chrome extension"), |ui| {
                    match &relay {
                        Ok(config) => settings_row(
                            ui,
                            Some(HOVER_GREEN),
                            "Connected",
                            &format!("Native host relay running · PID {}", config.pid),
                            |ui| {
                                if ui
                                    .add_enabled_ui(self.pending.is_none(), |ui| {
                                        action_button(ui, "cancel", "Disconnect", false)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.begin(
                                        |tx| {
                                            let result = browser::stop_browser_relay()
                                                .map(|()| {
                                                    CompletedOperation::McpUpdated(
                                                        "Chrome extension disconnected.".into(),
                                                    )
                                                })
                                                .map_err(|error| error.to_string());
                                            let _ = tx.send(result);
                                        },
                                        "Disconnecting the Chrome extension…",
                                    );
                                }
                                if ui
                                    .add_enabled_ui(self.pending.is_none(), |ui| {
                                        action_button(ui, "check", "Test", false)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.begin(
                                        |tx| {
                                            let result = browser::execute_browser_command(
                                                "snapshot",
                                                serde_json::json!({}),
                                            )
                                            .map(CompletedOperation::BrowserSnapshot)
                                            .map_err(|error| error.to_string());
                                            let _ = tx.send(result);
                                        },
                                        "Reading the active Chrome tab…",
                                    );
                                }
                            },
                        ),
                        Err(error) => settings_row(
                            ui,
                            Some(DANGER),
                            "Not connected",
                            &short_error(&error.to_string()),
                            |_| {},
                        ),
                    }
                    row_divider(ui);
                    settings_row(
                        ui,
                        None,
                        "Extension package",
                        &extension_package_path().display().to_string(),
                        |ui| {
                            if action_button(ui, "folder", "Open", false).clicked() {
                                self.notice = open_local_path(extension_package_path());
                            }
                        },
                    );
                });
                ui.add(egui::Label::new(egui::RichText::new("JEV acts only on tabs the extension can see, and never reads saved passwords or cookies.").size(10.5).color(MUTED)).wrap());
            }
            SettingsSection::Storage => self.storage_settings_ui(ui),
        }
    }

    fn mcp_snapshot(&self) -> (Vec<McpServerConfig>, Vec<String>) {
        self.mcp_manager
            .lock()
            .map(|manager| {
                (
                    manager.configs().to_vec(),
                    manager
                        .configs()
                        .iter()
                        .filter(|config| manager.is_connected(&config.id))
                        .map(|config| config.id.clone())
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    fn mcp_settings_ui(&mut self, ui: &mut egui::Ui) {
        settings_group(ui, Some("Add servers"), |ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Paste the MCP config an app gives you. New servers are added next to the existing ones and connect immediately.").size(11.0).color(MUTED));
            ui.add_space(8.0);
            egui::Frame::new().fill(SHELL).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(&mut self.mcp_import_text).font(egui::TextStyle::Monospace).desired_rows(6)
                    .desired_width(f32::INFINITY).frame(egui::Frame::NONE)
                    .hint_text(egui::RichText::new("{\n  \"mcpServers\": {\n    \"Name\": { \"command\": \"...\", \"args\": [\"...\"] }\n  }\n}").monospace().color(MUTED)));
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready = !self.mcp_import_text.trim().is_empty() && self.pending.is_none();
                if ui
                    .add_enabled_ui(ready, |ui| {
                        action_button(ui, "upload", "Import and connect", true)
                    })
                    .inner
                    .clicked()
                {
                    match mcp::parse_server_document(&self.mcp_import_text) {
                        Ok(configs) => {
                            self.mcp_import_text.clear();
                            self.mcp_operation(
                                move |manager| {
                                    manager
                                        .merge_configs(configs)
                                        .map(|report| report.summary())
                                },
                                "Saving and connecting MCP servers…",
                            );
                        }
                        Err(error) => self.notice = error,
                    }
                }
                if ui
                    .add_enabled_ui(self.pending.is_none(), |ui| {
                        action_button(ui, "refresh", "Reconnect all", false)
                    })
                    .inner
                    .clicked()
                {
                    self.mcp_operation(
                        |manager| {
                            manager
                                .reload()
                                .map(|()| "Reconnected enabled MCP servers.".into())
                        },
                        "Reconnecting MCP servers…",
                    );
                }
            });
            ui.add_space(8.0);
        });

        let (configs, _) = self.mcp_snapshot();
        let details: Vec<(bool, Option<String>, Vec<mcp::McpTool>, Option<String>)> = self
            .mcp_manager
            .lock()
            .map(|manager| {
                configs
                    .iter()
                    .map(|config| {
                        (
                            manager.is_connected(&config.id),
                            manager.last_error(&config.id).map(str::to_owned),
                            manager.tools_for(&config.id),
                            manager.server_document(&config.id),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        ui.label(
            egui::RichText::new("MCP servers and browser bridges")
                .size(12.0)
                .strong()
                .color(MUTED),
        );
        ui.add_space(6.0);
        // The browser bridge is a built-in integration, not a configured MCP server.
        let relay_ok = self.chrome_alive.load(Ordering::Relaxed);
        self.mcp_server_card(
            ui,
            "chrome",
            "Chrome browser bridge (built-in)",
            "JARVUS native messaging · browser tools",
            relay_ok,
            None,
            browser_tool_specs()
                .into_iter()
                .map(|(name, description, _)| (name, description))
                .collect(),
            None,
            true,
        );
        if configs.is_empty() {
            ui.label(
                egui::RichText::new(
                    "No MCP servers yet. Paste a config above, or ask JEV in chat to add one.",
                )
                .size(11.0)
                .color(MUTED),
            );
        }
        for (config, (connected, error, tools, document)) in configs.iter().zip(details) {
            let subtitle = mcp_server_subtitle(config);
            let tools = tools
                .into_iter()
                .map(|tool| (tool.name, tool.description.unwrap_or_default()))
                .collect();
            self.mcp_server_card(
                ui,
                &config.id,
                &config.id,
                &subtitle,
                connected,
                error.filter(|_| config.enabled),
                tools,
                document.map(|doc| (doc, config.enabled)),
                false,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mcp_server_card(
        &mut self,
        ui: &mut egui::Ui,
        id: &str,
        title: &str,
        subtitle: &str,
        connected: bool,
        error: Option<String>,
        tools: Vec<(String, String)>,
        editable: Option<(String, bool)>,
        builtin: bool,
    ) {
        let expanded = self.mcp_expanded.as_deref() == Some(id);
        let editing = self.mcp_editing_id.as_deref() == Some(id);
        let enabled = editable
            .as_ref()
            .map(|(_, enabled)| *enabled)
            .unwrap_or(true);
        let (dot, health) = if !enabled {
            (MUTED, "Disabled".to_owned())
        } else if connected {
            (HOVER_GREEN, format!("Connected · {} tools", tools.len()))
        } else if let Some(error) = &error {
            (DANGER, format!("Failed · {}", short_error(error)))
        } else {
            (
                Color32::from_rgb(222, 186, 92),
                if builtin {
                    "Not connected · open Chrome".into()
                } else {
                    "Not connected".into()
                },
            )
        };
        egui::Frame::new().fill(RAISED).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(12))
            .inner_margin(Margin { left: 14, right: 12, top: 10, bottom: 10 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 7.0, dot.gamma_multiply(0.22));
                    ui.painter().circle_filled(dot_rect.center(), 4.0, dot);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(title).size(12.5).strong().color(INK));
                        ui.add(egui::Label::new(egui::RichText::new(&health).size(10.5).color(if dot == DANGER { DANGER } else { MUTED })).truncate());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !builtin {
                            if icon_button(ui, "trash", "Remove server", 30.0).clicked() {
                                let id = id.to_owned();
                                self.mcp_operation(move |manager| {
                                    let configs = manager.configs().iter().filter(|config| config.id != id).cloned().collect();
                                    manager.replace_configs(configs).map(|()| format!("Removed MCP server {id}."))
                                }, "Removing MCP server…");
                            }
                            if icon_button(ui, if enabled { "cancel" } else { "check" }, if enabled { "Disable" } else { "Enable" }, 30.0).clicked() {
                                let id = id.to_owned();
                                let enable = !enabled;
                                self.mcp_operation(move |manager| {
                                    let mut updated = manager.configs().to_vec();
                                    if let Some(item) = updated.iter_mut().find(|item| item.id == id) { item.enabled = enable; }
                                    manager.replace_configs(updated).map(|()| format!("{} MCP server {id}.", if enable { "Enabled" } else { "Disabled" }))
                                }, "Updating MCP server…");
                            }
                            if icon_button(ui, "refresh", "Reconnect", 30.0).clicked() {
                                let id = id.to_owned();
                                self.mcp_operation(move |manager| { manager.disconnect(&id); manager.connect(&id).map(|()| format!("Reconnected {id}.")) }, "Reconnecting MCP server…");
                            }
                            if icon_button(ui, "edit", "Edit config", 30.0).clicked() {
                                if editing { self.mcp_editing_id = None; } else {
                                    self.mcp_editing_id = Some(id.to_owned());
                                    self.mcp_edit_text = editable.as_ref().map(|(doc, _)| doc.clone()).unwrap_or_default();
                                }
                            }
                        }
                        let tools_label = if expanded { "Hide tools" } else { "Show tools" };
                        if icon_button(ui, "chevron", tools_label, 30.0).clicked() {
                            self.mcp_expanded = if expanded { None } else { Some(id.to_owned()) };
                        }
                    });
                });
                ui.add_space(2.0);
                ui.add(egui::Label::new(egui::RichText::new(subtitle).size(10.0).monospace().color(MUTED)).truncate());
                let open = ui.ctx().animate_bool_with_time(ui.make_persistent_id(("mcp-tools-open", id)), expanded, ANIM);
                if open > 0.0 {
                    ui.add_space(8.0 * open);
                    if tools.is_empty() {
                        ui.label(egui::RichText::new(if connected { "This server reported no tools." } else { "Connect the server to list its tools." }).size(10.5).color(MUTED));
                    }
                    for (name, description) in tools.iter().take(((tools.len() as f32) * open).ceil() as usize) {
                        egui::Frame::new().fill(SURFACE).corner_radius(CornerRadius::same(8)).inner_margin(Margin { left: 10, right: 10, top: 6, bottom: 6 }).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(egui::RichText::new(name).size(11.0).monospace().color(INK));
                            if !description.trim().is_empty() {
                                ui.add(egui::Label::new(egui::RichText::new(description.trim()).size(10.0).color(MUTED)).wrap());
                            }
                        });
                        ui.add_space(3.0);
                    }
                }
                if editing {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("This server's config · secret values show as <saved> and are kept unless you replace them").size(10.0).color(MUTED));
                    egui::Frame::new().fill(SHELL).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(10)).show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut self.mcp_edit_text).font(egui::TextStyle::Monospace).desired_rows(8).desired_width(f32::INFINITY).frame(egui::Frame::NONE));
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled_ui(self.pending.is_none(), |ui| action_button(ui, "check", "Save and reconnect", true)).inner.clicked() {
                            match mcp::parse_server_document(&self.mcp_edit_text) {
                                Ok(configs) => {
                                    let old_id = id.to_owned();
                                    self.mcp_editing_id = None;
                                    self.mcp_operation(move |manager| {
                                        // A renamed server replaces the old entry.
                                        if !configs.iter().any(|config| config.id == old_id) {
                                            let kept = manager.configs().iter().filter(|config| config.id != old_id).cloned().collect();
                                            manager.replace_configs(kept)?;
                                        }
                                        manager.merge_configs(configs).map(|report| report.summary())
                                    }, "Saving MCP server…");
                                }
                                Err(error) => self.notice = error,
                            }
                        }
                        if action_button(ui, "cancel", "Cancel", false).clicked() { self.mcp_editing_id = None; }
                    });
                }
            });
        ui.add_space(8.0);
    }

    fn storage_settings_ui(&mut self, ui: &mut egui::Ui) {
        if !self.storage_scanned {
            self.refresh_storage_inventory();
        }
        let total: u64 = self
            .storage_breakdown
            .iter()
            .map(|(_, bytes, _)| *bytes)
            .sum();
        settings_group(ui, Some("This device"), |ui| {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(format!("{} used by JARVUS", format_bytes(total)))
                    .size(13.0)
                    .strong()
                    .color(INK),
            );
            ui.add_space(10.0);
            let (bar, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 12.0), Sense::hover());
            ui.painter().rect_filled(bar, CornerRadius::same(6), MINT);
            let mut x = bar.left();
            let segments: Vec<_> = self
                .storage_breakdown
                .iter()
                .filter(|(_, bytes, _)| *bytes > 0)
                .collect();
            for (index, (_, bytes, color)) in segments.iter().enumerate() {
                let width = if total == 0 {
                    0.0
                } else {
                    bar.width() * *bytes as f32 / total as f32
                };
                let width = width.max(3.0).min(bar.right() - x);
                let rounding = CornerRadius {
                    nw: if index == 0 { 6 } else { 0 },
                    sw: if index == 0 { 6 } else { 0 },
                    ne: if index + 1 == segments.len() { 6 } else { 0 },
                    se: if index + 1 == segments.len() { 6 } else { 0 },
                };
                ui.painter().rect_filled(
                    Rect::from_min_size(pos2(x, bar.top()), Vec2::new(width, bar.height())),
                    rounding,
                    *color,
                );
                x += width;
            }
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                for (label, bytes, color) in &self.storage_breakdown {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, *color);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(label).size(11.0).color(INK));
                        ui.label(
                            egui::RichText::new(format_bytes(*bytes))
                                .size(10.0)
                                .color(MUTED),
                        );
                    });
                    ui.add_space(16.0);
                }
            });
            ui.add_space(6.0);
            if let Some(root) = app_data_root() {
                ui.label(
                    egui::RichText::new(root.display().to_string())
                        .size(10.0)
                        .monospace()
                        .color(MUTED),
                );
            }
            ui.add_space(8.0);
        });
        settings_group(ui, None, |ui| {
            let chats = self
                .conversations
                .iter()
                .filter(|conversation| conversation.message_count.max(conversation.turns.len()) > 0)
                .count();
            settings_row(
                ui,
                None,
                "Clean up old chats",
                &format!("Review {chats} chats and permanently delete the ones you no longer need"),
                |ui| {
                    if action_button(ui, "chat", "Review", false).clicked() {
                        self.review_open = true;
                        self.review_selected.clear();
                    }
                },
            );
            row_divider(ui);
            settings_row(
                ui,
                None,
                "App data folder",
                "Chat history, settings and MCP config",
                |ui| {
                    if let Some(root) = app_data_root() {
                        if action_button(ui, "folder", "Open", false).clicked() {
                            self.notice = open_local_path(root);
                        }
                    }
                },
            );
            row_divider(ui);
            settings_row(
                ui,
                None,
                "Local screen captures",
                "Debug maps and screenshots JEV saved on this PC",
                |ui| {
                    if action_button(ui, "folder", "Open", false).clicked() {
                        self.notice = open_local_path(captures_directory());
                    }
                },
            );
            row_divider(ui);
            settings_row(
                ui,
                None,
                "Downloads",
                &downloads_directory()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                |ui| {
                    if let Some(path) = downloads_directory() {
                        if action_button(ui, "download", "Open", false).clicked() {
                            self.notice = open_local_path(path);
                        }
                    }
                },
            );
        });
    }

    /// Codex-style cleanup dialog: filter by age, tick chats, delete selected.
    fn review_chats_modal(&mut self, ctx: &egui::Context) {
        if !self.review_open {
            return;
        }
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let min_age_days = self.review_filter_days;
        let rows: Vec<(String, String, usize, u64)> = self
            .conversations
            .iter()
            .filter_map(|conversation| {
                let count = conversation.message_count.max(conversation.turns.len());
                if count == 0 {
                    return None;
                }
                let age_days = conversation_created_ns(&conversation.id)
                    .map(|created| ((now_ns.saturating_sub(created)) / 86_400_000_000_000) as u64)
                    .unwrap_or(0);
                (age_days >= min_age_days as u64).then(|| {
                    (
                        conversation.id.clone(),
                        conversation.title.clone(),
                        count,
                        age_days,
                    )
                })
            })
            .collect();
        let mut delete = false;
        let response = egui::Modal::new(egui::Id::unique("review-chats"))
            .backdrop_color(Color32::from_black_alpha(120))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(1.0, LINE))
                    .corner_radius(CornerRadius::same(16))
                    .shadow(egui::Shadow {
                        offset: [0, 18],
                        blur: 50,
                        spread: 0,
                        color: Color32::from_black_alpha(180),
                    })
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Review old chats")
                            .size(16.0)
                            .strong()
                            .color(INK),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, "cancel", "Close", 28.0).clicked() {
                            self.review_open = false;
                        }
                    });
                });
                ui.label(
                    egui::RichText::new("Deleting a chat removes it from this PC permanently.")
                        .size(11.0)
                        .color(MUTED),
                );
                ui.add_space(12.0);
                egui::Frame::new()
                    .fill(RAISED)
                    .stroke(egui::Stroke::new(1.0, LINE))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(12, 4))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        settings_row(
                            ui,
                            None,
                            "Last updated",
                            "Only show chats at least this old",
                            |ui| {
                                egui::ComboBox::from_id_salt("review-age")
                                    .selected_text(match self.review_filter_days {
                                        0 => "Any time",
                                        7 => "Over 7 days ago",
                                        30 => "Over 30 days ago",
                                        _ => "Over 90 days ago",
                                    })
                                    .show_ui(ui, |ui| {
                                        for (days, label) in [
                                            (0, "Any time"),
                                            (7, "Over 7 days ago"),
                                            (30, "Over 30 days ago"),
                                            (90, "Over 90 days ago"),
                                        ] {
                                            ui.selectable_value(
                                                &mut self.review_filter_days,
                                                days,
                                                label,
                                            );
                                        }
                                    });
                            },
                        );
                    });
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        // Neutral checkbox boxes (the global green bg_fill is meant for scroll/slider handles).
                        ui.visuals_mut().widgets.inactive.bg_fill = MINT;
                        ui.visuals_mut().widgets.hovered.bg_fill = HOVER_FILL;
                        ui.visuals_mut().widgets.active.bg_fill = HOVER_FILL;
                        if rows.is_empty() {
                            ui.label(
                                egui::RichText::new("No chats match these filters")
                                    .size(11.0)
                                    .color(MUTED),
                            );
                        }
                        for (id, title, turns, age) in &rows {
                            let mut checked = self.review_selected.contains(id);
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut checked, "").changed() {
                                    if checked {
                                        self.review_selected.insert(id.clone());
                                    } else {
                                        self.review_selected.remove(id);
                                    }
                                }
                                ui.vertical(|ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(title).size(12.0).color(INK),
                                        )
                                        .truncate(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{turns} messages · {}",
                                            if *age == 0 {
                                                "today".into()
                                            } else {
                                                format!("{age} days ago")
                                            }
                                        ))
                                        .size(10.0)
                                        .color(MUTED),
                                    );
                                });
                            });
                            ui.add_space(4.0);
                        }
                    });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Selected chats: {}",
                            self.review_selected.len()
                        ))
                        .size(11.0)
                        .color(MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_delete = !self.review_selected.is_empty() && !self.streaming;
                        if ui
                            .add_enabled_ui(can_delete, |ui| {
                                action_button_colored(ui, "trash", "Delete selected", DANGER)
                            })
                            .inner
                            .clicked()
                        {
                            delete = true;
                        }
                        if action_button(ui, "cancel", "Cancel", false).clicked() {
                            self.review_open = false;
                        }
                        if !rows.is_empty()
                            && action_button(ui, "check", "Select all", false).clicked()
                        {
                            self.review_selected
                                .extend(rows.iter().map(|(id, ..)| id.clone()));
                        }
                    });
                });
            });
        if response.should_close() {
            self.review_open = false;
        }
        if delete {
            let ids = std::mem::take(&mut self.review_selected);
            let active_id = self
                .conversations
                .get(self.active_conversation)
                .map(|conversation| conversation.id.clone());
            for id in &ids {
                self.inactive_unloads.remove(id);
                if let Err(error) = chat_store::delete_chat(id) {
                    self.notice = format!("Could not remove chat files: {error}");
                }
            }
            self.conversations
                .retain(|conversation| !ids.contains(&conversation.id));
            if self.conversations.is_empty() {
                self.conversations.push(new_conversation());
            }
            self.active_conversation = active_id
                .and_then(|id| {
                    self.conversations
                        .iter()
                        .position(|conversation| conversation.id == id)
                })
                .unwrap_or(0);
            if let Some(conversation) = self.conversations.get_mut(self.active_conversation) {
                if !conversation.loaded {
                    load_conversation_body(conversation);
                }
            }
            self.history_dirty = true;
            self.flush_history(true);
            self.notice = format!("Deleted {} chats.", ids.len());
            self.review_open = false;
            self.refresh_storage_inventory();
        }
    }

    fn mcp_operation(
        &mut self,
        operation: impl FnOnce(&mut McpManager) -> Result<String, String> + Send + 'static,
        message: &str,
    ) {
        let manager = Arc::clone(&self.mcp_manager);
        self.begin(
            move |tx| {
                let result = manager
                    .lock()
                    .map_err(|_| "MCP manager lock is unavailable".to_owned())
                    .and_then(|mut manager| operation(&mut manager));
                let _ = tx.send(result.map(CompletedOperation::McpUpdated));
            },
            message,
        );
    }

    fn refresh_storage_inventory(&mut self) {
        self.storage_scanned = true;
        let root = app_data_root();
        let file_size = |path: Option<PathBuf>| {
            path.and_then(|path| fs::metadata(path).ok())
                .map(|meta| meta.len())
                .unwrap_or(0)
        };
        let chats_path = chat_store::chats_root();
        let chats = chats_path.as_ref().map(|path| dir_size(path)).unwrap_or(0);
        let settings = file_size(settings_path()) + file_size(Some(mcp::default_config_path()));
        let captures = dir_size(&captures_directory());
        let app_total = root.as_ref().map(|root| dir_size(root)).unwrap_or(0);
        let other = app_total
            .saturating_sub(chats)
            .saturating_sub(file_size(settings_path()))
            .saturating_sub(file_size(Some(mcp::default_config_path())));
        self.storage_breakdown = vec![
            ("Chats".into(), chats, HOVER_GREEN),
            ("Settings & MCP".into(), settings, MINT_DEEP),
            (
                "Screen captures".into(),
                captures,
                Color32::from_rgb(120, 124, 122),
            ),
            (
                "Other app data".into(),
                other,
                Color32::from_rgb(70, 72, 71),
            ),
        ];
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let bar_rect = ui.max_rect();
        let content_rect = bar_rect.shrink2(Vec2::new(10.0, 6.0));
        // Reserve a real lane for status chips and window controls. At the old
        // 330 px fixed width, the live JEV timer could be clipped on common
        // desktop sizes once the account and settings controls were present.
        let right_width = 430.0_f32.min(content_rect.width() * 0.60);
        let right_rect = Rect::from_min_max(
            pos2(content_rect.right() - right_width, content_rect.top()),
            content_rect.max,
        );
        let left_rect = Rect::from_min_max(
            content_rect.min,
            pos2(right_rect.left(), content_rect.bottom()),
        );

        let mut left = ui.new_child(
            UiBuilder::new()
                .layout(egui::Layout::left_to_right(egui::Align::Center))
                .id_salt("titlebar-left")
                .max_rect(left_rect),
        );
        brand_mark(&mut left, 30.0);
        left.add_space(4.0);
        left.vertical(|ui| {
            ui.add_space(1.0);
            ui.label(egui::RichText::new("JARVUS").size(15.0).strong().color(INK));
            ui.label(
                egui::RichText::new("Desktop assistant")
                    .size(10.0)
                    .color(MUTED),
            );
        });
        left.add_space(20.0);

        // Segmented tabs with a pill that slides between them.
        let tabs = [("Chat", false), ("Computer", true)];
        let tab_size = Vec2::new(92.0, 30.0);
        let (track, _) = left.allocate_exact_size(
            Vec2::new(tab_size.x * tabs.len() as f32 + 6.0, tab_size.y + 6.0),
            Sense::hover(),
        );
        left.painter()
            .rect_filled(track, CornerRadius::same(18), SHELL);
        left.painter().rect_stroke(
            track,
            CornerRadius::same(18),
            egui::Stroke::new(1.0, LINE),
            egui::StrokeKind::Inside,
        );
        let selected_index = if self.show_tools { 1.0 } else { 0.0 };
        let pill_index = left.ctx().animate_value_with_time(
            left.make_persistent_id("nav-pill"),
            selected_index,
            ANIM,
        );
        let pill = Rect::from_min_size(
            pos2(
                track.left() + 3.0 + pill_index * tab_size.x,
                track.top() + 3.0,
            ),
            tab_size,
        );
        let pill_glow = egui::Shadow {
            offset: [0, 0],
            blur: 16,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(GLOW_GREEN[0], GLOW_GREEN[1], GLOW_GREEN[2], 46),
        };
        left.painter()
            .add(pill_glow.as_shape(pill, CornerRadius::same(15)));
        left.painter()
            .rect_filled(pill, CornerRadius::same(15), MINT);
        left.painter().rect_stroke(
            pill,
            CornerRadius::same(15),
            egui::Stroke::new(1.0, Color32::from_rgb(56, 60, 58)),
            egui::StrokeKind::Inside,
        );
        for (index, (label, is_tools)) in tabs.iter().enumerate() {
            let rect = Rect::from_min_size(
                pos2(
                    track.left() + 3.0 + index as f32 * tab_size.x,
                    track.top() + 3.0,
                ),
                tab_size,
            );
            let response = left.interact(
                rect,
                left.make_persistent_id(("nav-tab", index)),
                Sense::click(),
            );
            let active = self.show_tools == *is_tools;
            let color = if active {
                INK
            } else if response.hovered() {
                HOVER_GREEN
            } else {
                MUTED
            };
            left.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                *label,
                egui::FontId::proportional(12.5),
                color,
            );
            response.widget_info(|| {
                egui::WidgetInfo::selected(egui::Role::Button, true, active, *label)
            });
            if response.clicked() {
                self.show_tools = *is_tools;
            }
            if response.hovered() {
                left.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        let drag_width = left.available_width().max(0.0);
        let (_, drag) =
            left.allocate_exact_size(Vec2::new(drag_width, 34.0), Sense::click_and_drag());
        drag.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Drag window"));
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            let maximized = ui
                .ctx()
                .input(|input| input.viewport().maximized.unwrap_or(false));
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }

        let maximized = ui
            .ctx()
            .input(|input| input.viewport().maximized.unwrap_or(false));
        let mut right = ui.new_child(
            UiBuilder::new()
                .layout(egui::Layout::right_to_left(egui::Align::Center))
                .id_salt("titlebar-right")
                .max_rect(right_rect),
        );
        for (kind, command, hint) in [
            ("close", egui::ViewportCommand::Close, "Close JARVUS"),
            (
                "maximize",
                egui::ViewportCommand::Maximized(!maximized),
                if maximized { "Restore" } else { "Maximize" },
            ),
            (
                "minimize",
                egui::ViewportCommand::Minimized(true),
                "Minimize",
            ),
        ] {
            let (rect, response) = right.allocate_exact_size(Vec2::new(42.0, 32.0), Sense::click());
            let is_close = kind == "close";
            if response.hovered() {
                right.painter().rect_filled(
                    rect,
                    CornerRadius::same(8),
                    if is_close {
                        Color32::from_rgb(196, 43, 28)
                    } else {
                        MINT
                    },
                );
            }
            let center = rect.center();
            let icon_color = if response.hovered() {
                if is_close {
                    Color32::WHITE
                } else {
                    HOVER_GREEN
                }
            } else {
                INK
            };
            let stroke = egui::Stroke::new(1.5, icon_color);
            let half = 5.0;
            match kind {
                "close" => {
                    right.painter().line_segment(
                        [
                            pos2(center.x - half, center.y - half),
                            pos2(center.x + half, center.y + half),
                        ],
                        stroke,
                    );
                    right.painter().line_segment(
                        [
                            pos2(center.x + half, center.y - half),
                            pos2(center.x - half, center.y + half),
                        ],
                        stroke,
                    );
                }
                "maximize" if maximized => {
                    right.painter().rect_stroke(
                        Rect::from_center_size(center + Vec2::new(-1.5, 1.5), Vec2::splat(8.0)),
                        1.5,
                        stroke,
                        egui::StrokeKind::Middle,
                    );
                    right.painter().line_segment(
                        [
                            pos2(center.x - 2.5, center.y - 4.5),
                            pos2(center.x + 4.5, center.y - 4.5),
                        ],
                        stroke,
                    );
                    right.painter().line_segment(
                        [
                            pos2(center.x + 4.5, center.y - 4.5),
                            pos2(center.x + 4.5, center.y + 2.5),
                        ],
                        stroke,
                    );
                }
                "maximize" => {
                    right.painter().rect_stroke(
                        Rect::from_center_size(center, Vec2::splat(10.0)),
                        1.5,
                        stroke,
                        egui::StrokeKind::Middle,
                    );
                }
                _ => {
                    right.painter().line_segment(
                        [
                            pos2(center.x - half, center.y),
                            pos2(center.x + half, center.y),
                        ],
                        stroke,
                    );
                }
            }
            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, hint));
            let clicked = response.clicked();
            let _ = response.on_hover_text(hint);
            if clicked {
                ui.ctx().send_viewport_cmd(command);
            }
        }
        right.add_space(6.0);
        if icon_button(&mut right, "settings", "Settings", 34.0).clicked() {
            self.show_settings = true;
        }
        right.add_space(6.0);

        // Status chips: ChatGPT connection (dot) and JEV's own local action time.
        let (state_color, state_text) = if self.signed_in && !self.model_list.is_empty() {
            // Keep the account identifier in Settings only; the always-visible
            // titlebar state communicates connectivity without exposing the
            // signed-in email in screenshots or screen shares.
            (HOVER_GREEN, "ChatGPT connected".to_owned())
        } else if self.pending.is_some() || self.device_code.is_some() || self.signed_in {
            (
                Color32::from_rgb(222, 186, 92),
                "ChatGPT connecting…".to_owned(),
            )
        } else {
            (
                Color32::from_rgb(220, 82, 70),
                "ChatGPT not connected · open Settings to sign in".to_owned(),
            )
        };
        // Live while a local action runs, then the last finished action's time.
        let jev_label = local_action_duration_label(
            self.jev_action_started
                .map(|started| started.elapsed().as_millis())
                .or(self.jev_latency_ms),
        );
        status_chip(
            &mut right,
            "jev-chip",
            |painter, rect, color| paint_icon(painter, rect, "bolt", color),
            &jev_label,
            "JEV response time · local desktop actions only (ChatGPT time excluded)",
        );
        right.add_space(6.0);
        status_chip(
            &mut right,
            "chatgpt-chip",
            move |painter, rect, _| {
                painter.circle_filled(rect.center(), 6.0, state_color.gamma_multiply(0.25));
                painter.circle_filled(rect.center(), 3.6, state_color);
            },
            "ChatGPT",
            &state_text,
        );
    }

    fn chat_panel(&mut self, ui: &mut egui::Ui) {
        let conversation = self.conversations.get(self.active_conversation);
        let turns = conversation.map(|c| c.turns.clone()).unwrap_or_default();
        let transcript_id = conversation
            .map(|c| c.id.clone())
            .unwrap_or_else(|| "missing-conversation".into());
        let compacted_through = conversation.filter(|c| !c.context_summary.is_empty()).map(|c| c.summary_through);
        ui.vertical(|ui| {
            let panel_width = ui.available_width();
            ui.add_space(2.0);
            // Keep short conversations at the top of the transcript. Scroll to
            // the newest turn only when a new event explicitly requests it;
            // stick_to_bottom also bottom-aligns sparse chats after docking.
            egui::ScrollArea::vertical().id_salt(("chat-transcript", transcript_id.clone())).auto_shrink([false, false]).show(ui, |ui| {
                for (index, turn) in turns.iter().enumerate() {
                    if compacted_through == Some(index) && index > 0 {
                        ui.add_space(10.0);
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new("— Earlier messages were compacted into a summary —").size(10.5).color(MUTED));
                        });
                    }
                    ui.add_space(14.0);
                    if turn.role == "user" {
                        let editing = self.editing_turn == Some(index);
                        let content_max_width = (ui.available_width() - 30.0).clamp(60.0, 630.0);
                        // `with_layout` fills the available rectangle. Inside a
                        // vertical transcript that means the user row consumes
                        // the rest of the viewport and strands its actions and
                        // every later turn near the bottom. Bound the RTL layout
                        // to a content-sized horizontal row instead.
                        // The bubble hugs its text (up to a maximum), like a chat message.
                        let text_wrap = (content_max_width - 30.0).max(40.0);
                        let measured = ui.painter().layout(turn.content.clone(), egui::TextStyle::Body.resolve(ui.style()), INK, text_wrap).size().x;
                        let bubble_text_width = (measured + 2.0).min(text_wrap).max(20.0);
                        ui.horizontal(|row| {
                            row.with_layout(egui::Layout::right_to_left(egui::Align::Min), |row| {
                                egui::Frame::new().fill(Color32::from_rgb(35, 37, 34)).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(16))
                                    .inner_margin(Margin { left: 15, right: 15, top: 11, bottom: 11 }).show(row, |bubble| {
                                        if editing { bubble.set_max_width(content_max_width); } else { bubble.set_width(bubble_text_width); }
                                        if editing {
                                            let edit = bubble.add(egui::TextEdit::multiline(&mut self.draft).desired_width(f32::INFINITY).desired_rows(3)
                                                .hint_text(egui::RichText::new("Edit your message").color(MUTED)));
                                            let focus_once = bubble.make_persistent_id(("edit-focused", index, self.edit_generation));
                                            if !bubble.data(|data| data.get_temp::<bool>(focus_once).unwrap_or(false)) {
                                                edit.request_focus();
                                                bubble.data_mut(|data| data.insert_temp(focus_once, true));
                                            }
                                        } else {
                                            bubble.scope(|ui| {
                                                ui.style_mut().visuals.override_text_color = Some(INK);
                                                ui.style_mut().interaction.selectable_labels = true;
                                                CommonMarkViewer::new().show(ui, &mut self.markdown_cache, &preserve_line_breaks(&turn.content));
                                            });
                                        }
                                    });
                            });
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|row| {
                            row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |actions| {
                                if editing {
                                    let send = icon_button(actions, "send", "Send edited message", 34.0);
                                    if send.clicked() { self.send_chat(); }
                                    let cancel = icon_button(actions, "cancel", "Cancel edit", 34.0);
                                    if cancel.clicked() { self.editing_turn = None; self.draft.clear(); self.notice.clear(); }
                                } else {
                                    let copy = icon_button(actions, "copy", "Copy message", 34.0);
                                    if copy.clicked() { actions.ctx().copy_text(turn.content.clone()); self.notice = "Message copied to clipboard.".into(); }
                                    let edit = actions.add_enabled_ui(!self.streaming, |ui| icon_button(ui, "edit", "Edit and resend this message", 34.0)).inner;
                                    if edit.clicked() { self.edit_user_message(index); }
                                }
                            });
                        });
                    } else {
                        let last_turn = index + 1 == turns.len();
                        self.assistant_message(ui, index, turn, last_turn, panel_width);
                    }
                }
                self.queued_bubbles(ui, &transcript_id);
                if self.scroll_to_bottom { ui.scroll_to_cursor(Some(egui::Align::BOTTOM)); self.scroll_to_bottom = false; }
            });
        });
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let empty = self
            .conversations
            .get(self.active_conversation)
            .is_none_or(|c| c.turns.is_empty());
        if empty {
            // Suggestion chips, centered as one row.
            let chips = [("Open an app", "Open Chrome"), ("See what's running", "List running apps"), ("Inspect this screen", "Inspect the focused app")];
            let widths: Vec<f32> = chips.iter().map(|(label, _)| ui.painter().layout_no_wrap((*label).to_owned(), egui::FontId::proportional(11.0), INK).size().x + 28.0).collect();
            let total = widths.iter().sum::<f32>() + 8.0 * (chips.len() as f32 - 1.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                for ((label, prompt), width) in chips.iter().zip(widths) {
                    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
                    let hover = ui.ctx().animate_bool_with_time(response.id.with("chip-hover"), response.hovered(), ANIM);
                    ui.painter().rect_filled(rect, CornerRadius::same(16), lerp_color(Color32::from_rgb(28, 32, 29), HOVER_FILL, hover));
                    ui.painter().rect_stroke(rect, CornerRadius::same(16), egui::Stroke::new(1.0, lerp_color(LINE, HOVER_GREEN.gamma_multiply(0.6), hover)), egui::StrokeKind::Inside);
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, *label, egui::FontId::proportional(11.0), lerp_color(INK, HOVER_GREEN, hover));
                    if hover > 0.0 { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                    if response.clicked() { self.draft = (*prompt).to_owned(); }
                }
            });
            ui.add_space(8.0);
        }
        let top_radius = (18.0 * (1.0 - self.interaction_t)).round() as u8;
        let slash = self.slash_entries();
        let mut slash_pick: Option<SlashEntry> = None;
        let composer_frame = egui::Frame::new().fill(RAISED).stroke(egui::Stroke::new(1.0, Color32::from_rgb(48, 50, 49))).corner_radius(CornerRadius { nw: top_radius, ne: top_radius, sw: 18, se: 18 })
            .inner_margin(Margin { left: 14, right: 12, top: 11, bottom: 9 }).show(ui, |ui| {
                if self.editing_turn.is_some() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Editing an earlier message · later turns will be replaced when sent.").size(11.0).color(MINT_DEEP));
                        if ui.small_button("Cancel").clicked() {
                            self.editing_turn = None;
                            self.draft.clear();
                            self.notice.clear();
                        }
                    });
                    ui.add_space(4.0);
                }
                let input_id = ui.make_persistent_id("composer-input");
                if !slash.is_empty() && ui.memory(|memory| memory.focused() == Some(input_id)) {
                    self.slash_selected = self.slash_selected.min(slash.len() - 1);
                    let (down, up, accept) = ui.input_mut(|input| (
                        input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                        input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Tab) || input.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                    ));
                    if down { self.slash_selected = (self.slash_selected + 1) % slash.len(); }
                    if up { self.slash_selected = (self.slash_selected + slash.len() - 1) % slash.len(); }
                    if accept { slash_pick = slash.get(self.slash_selected).cloned(); }
                }
                let enter_to_send = ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && !i.modifiers.ctrl && !i.modifiers.alt)
                    && ui.memory(|memory| memory.focused() == Some(input_id));
                if enter_to_send { ui.input_mut(|input| { input.consume_key(egui::Modifiers::NONE, egui::Key::Enter); }); }
                // The text area is two rows tall and grows (animated) with the text; past its
                // limit the text scrolls inside it.
                let text_height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + self.composer_extra;
                let enabled = self.signed_in;
                let input = egui::ScrollArea::vertical().id_salt("composer-text").max_height(text_height).min_scrolled_height(text_height)
                    .auto_shrink([false, false]).show(ui, |ui| {
                        ui.add_enabled(enabled,
                            egui::TextEdit::multiline(&mut self.draft).id(input_id)
                                .hint_text(egui::RichText::new(if !self.signed_in { "Connect ChatGPT to start a conversation" } else if self.streaming { "Queue a message · it sends when JEV finishes" } else { "Message JEV" }).color(INK))
                                .desired_rows(2).desired_width(f32::INFINITY).font(egui::TextStyle::Body).frame(egui::Frame::NONE))
                    }).inner;
                ui.add_space(4.0);
                let mut send_clicked = false;
                let mut queue_clicked = false;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.streaming {
                        // Keep a visible animated running state, with a real stop
                        // glyph on hover so cancellation is discoverable.
                        let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
                        let hovered = response.hovered();
                        ui.painter().circle_filled(rect.center(), 20.0, if hovered { Color32::from_rgb(60, 30, 30) } else { Color32::from_rgb(18, 20, 19) });
                        if hovered {
                            paint_icon(ui.painter(), rect.shrink(9.0), "stop", DANGER);
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        } else {
                            let t = ui.input(|i| i.time) as f32;
                            let points = (0..=24).map(|i| {
                                let a = t * 6.0 + i as f32 / 24.0 * std::f32::consts::PI * 1.5;
                                pos2(rect.center().x + 11.0 * a.cos(), rect.center().y + 11.0 * a.sin())
                            }).collect::<Vec<_>>();
                            ui.painter().add(egui::Shape::line(points, egui::Stroke::new(2.5, MINT_DEEP)));
                        }
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Stop this run"));
                        if response.on_hover_text("Stop this run").clicked() { self.cancel_run(); }
                        if !self.draft.trim().is_empty() {
                            queue_clicked = ui.add(send_button("↑")).on_hover_text("Queue this message · Enter").clicked();
                        }
                    } else {
                        send_clicked = ui.add_enabled(self.signed_in && self.pending.is_none() && !self.draft.trim().is_empty(), send_button("↑"))
                            .on_hover_text("Send message · Enter · Shift+Enter for a new line").clicked();
                    }
                    let context = self.context_meter(ui);
                    if context.clicked() { self.context_drawer_open = !self.context_drawer_open; }
                    if compose_toggle(ui, self.compose_mode, !self.streaming).clicked() { self.compose_mode = !self.compose_mode; }
                    ui.add_space(4.0);
                    self.model_selector(ui);
                    if self.streaming {
                        let activity = self.active_tool.as_deref().map(tool_activity).unwrap_or("Thinking…");
                        ui.label(egui::RichText::new("Working…").size(11.0).color(MINT_DEEP)).on_hover_text(activity);
                    }
                });
                if queue_clicked || send_clicked || (enter_to_send && input.has_focus()) { self.submit_or_queue(); }
                if slash_pick.is_some() { ui.memory_mut(|memory| memory.request_focus(input_id)); }
            });
        if !slash.is_empty() {
            if let Some(index) = self.slash_popup(ui.ctx(), composer_frame.response.rect, &slash) {
                slash_pick = slash.get(index).cloned();
            }
        }
        if let Some(entry) = slash_pick {
            self.slash_selected = 0;
            if entry.runs {
                self.draft.clear();
                self.run_builtin(&entry.name);
            } else {
                self.draft = entry.insert.clone();
                let end = self.draft.chars().count();
                let id = ui.make_persistent_id("composer-input");
                let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
                state.store(ui.ctx(), id);
            }
        }
    }

    /// Commands, installed skills and MCP servers matching what follows the `/` in the chat box.
    fn slash_entries(&self) -> Vec<SlashEntry> {
        let Some(typed) = self.draft.strip_prefix('/') else { return Vec::new() };
        if typed.contains(char::is_whitespace) || self.streaming {
            return Vec::new();
        }
        let needle = typed.to_lowercase();
        let mut entries = vec![
            SlashEntry::command("compact", "Free up context by summarizing the conversation so far"),
            SlashEntry::command("compose", "Turn Compose Mode on or off: investigate and plan before anything changes"),
            SlashEntry::command("new", "Start a new chat"),
        ];
        for skill in skills::list() {
            entries.push(SlashEntry {
                insert: format!("/{} ", skill.name),
                hint: if skill.description.is_empty() { "Installed skill".into() } else { skill.description.clone() },
                name: skill.name,
                kind: "Skill",
                runs: false,
            });
        }
        if let Ok(manager) = self.mcp_manager.lock() {
            for config in manager.configs().iter().filter(|config| config.enabled) {
                entries.push(SlashEntry {
                    insert: format!("Use the {} MCP server to ", config.id),
                    hint: format!("Ask JEV to use the {} MCP server", config.id),
                    name: config.id.clone(),
                    kind: "MCP",
                    runs: false,
                });
            }
        }
        entries.retain(|entry| entry.name.to_lowercase().contains(&needle));
        entries.sort_by_key(|entry| !entry.name.to_lowercase().starts_with(&needle));
        entries
    }

    /// The suggestion list above the chat box; returns the index clicked, if any.
    fn slash_popup(&mut self, ctx: &egui::Context, anchor: Rect, entries: &[SlashEntry]) -> Option<usize> {
        let mut clicked = None;
        let width = anchor.width().clamp(280.0, 560.0);
        #[allow(deprecated)]
        let _area = egui::Area::new(egui::Id::new("slash-popup"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(pos2(anchor.left(), anchor.top() - 6.0))
            .show(ctx, |ui| {
                egui::Frame::new().fill(RAISED).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(5))
                    .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(110) })
                    .show(ui, |ui| {
                        ui.set_width(width);
                        egui::ScrollArea::vertical().max_height(280.0).auto_shrink([false, true]).show(ui, |ui| {
                            for (index, entry) in entries.iter().enumerate() {
                                let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
                                if response.hovered() { self.slash_selected = index; }
                                let selected = index == self.slash_selected;
                                if selected {
                                    ui.painter().rect_filled(rect, CornerRadius::same(8), HOVER_FILL);
                                    ui.scroll_to_rect(rect, None);
                                }
                                let name = format!("/{}", entry.name);
                                let name_width = ui.painter().layout_no_wrap(name.clone(), egui::FontId::proportional(12.0), INK).size().x;
                                ui.painter().text(rect.left_center() + Vec2::new(10.0, 0.0), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(12.0), if selected { HOVER_GREEN } else { INK });
                                let kind_width = ui.painter().layout_no_wrap(entry.kind.to_owned(), egui::FontId::proportional(9.5), MUTED).size().x;
                                ui.painter().text(rect.right_center() - Vec2::new(10.0, 0.0), egui::Align2::RIGHT_CENTER, entry.kind, egui::FontId::proportional(9.5), MUTED);
                                let hint_left = rect.left() + 10.0 + name_width + 12.0;
                                let hint_right = rect.right() - 10.0 - kind_width - 10.0;
                                if hint_right > hint_left + 30.0 {
                                    let clipped = ui.painter().with_clip_rect(Rect::from_min_max(pos2(hint_left, rect.top()), pos2(hint_right, rect.bottom())));
                                    clipped.text(pos2(hint_left, rect.center().y), egui::Align2::LEFT_CENTER, &entry.hint, egui::FontId::proportional(10.5), MUTED);
                                }
                                if response.on_hover_text(&entry.hint).clicked() { clicked = Some(index); }
                            }
                        });
                    });
            });
        clicked
    }

    fn context_meter(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let response = ui.add_sized(
            Vec2::splat(34.0),
            egui::Button::new("")
                .fill(Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE),
        );
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Context usage"));
        let rect = response.rect;
        let limit = self.context_window.max(1);
        let used = self.context_tokens.min(limit);
        let duration = if self.context_snap_frames > 0 { 0.0 } else { ANIM };
        let animated_used = ui
            .ctx()
            .animate_value_with_time(ui.make_persistent_id("context-meter-used"), used as f32, duration);
        let ratio = (animated_used / limit as f32).clamp(0.0, 1.0);
        let center = rect.center();
        let radius = 13.0;
        ui.painter().circle_stroke(
            center,
            radius,
            egui::Stroke::new(3.0, Color32::from_rgb(66, 67, 59)),
        );
        let points = (0..=64)
            .filter_map(|index| {
                let step = index as f32 / 64.0;
                if step > ratio {
                    return None;
                }
                let angle = -std::f32::consts::FRAC_PI_2 + step * std::f32::consts::TAU;
                Some(pos2(
                    center.x + radius * angle.cos(),
                    center.y + radius * angle.sin(),
                ))
            })
            .collect::<Vec<_>>();
        if points.len() > 1 {
            ui.painter()
                .add(egui::Shape::line(points, egui::Stroke::new(3.0, MINT_DEEP)));
        }
        ui.painter().text(
            center,
            egui::Align2::CENTER_CENTER,
            format!("{}", (ratio * 100.0).round() as u32),
            egui::FontId::proportional(9.0),
            INK,
        );
        response.on_hover_text(format!(
            "{} / {} used{}",
            format_tokens(used),
            format_tokens(limit),
            " · current chat context estimate"
        ))
    }

    fn model_selector(&mut self, ui: &mut egui::Ui) {
        if self.model_list.is_empty() {
            ui.label(egui::RichText::new("No model").size(11.0).color(MUTED));
            return;
        }
        let models = self.model_list.clone();
        let current_model = models
            .iter()
            .find(|model| model.slug == self.selected_model)
            .cloned();
        let selected_name = current_model
            .as_ref()
            .map(|model| model.display_name.clone())
            .unwrap_or_else(|| "Choose model".to_owned());
        let reasoning_levels = current_model
            .as_ref()
            .map(|model| model.supported_reasoning_levels.clone())
            .unwrap_or_default();

        // Trigger chip: model name + thinking level + chevron.
        let label = if reasoning_levels.is_empty() {
            selected_name.clone()
        } else {
            format!("{selected_name}  ·  {}", self.reasoning_effort)
        };
        let galley =
            ui.painter()
                .layout_no_wrap(label.clone(), egui::FontId::proportional(11.5), INK);
        // The chip keeps the width of its widest thinking level, so choosing a level never moves
        // the popup that is anchored to it.
        let chip_text_width = reasoning_levels
            .iter()
            .map(|level| ui.painter().layout_no_wrap(format!("{selected_name}  ·  {level}"), egui::FontId::proportional(11.5), INK).size().x)
            .fold(galley.size().x, f32::max);
        let (rect, button) =
            ui.allocate_exact_size(Vec2::new(chip_text_width + 36.0, 28.0), Sense::click());
        let hovered = button.hovered();
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(14),
            if hovered { HOVER_FILL } else { RAISED },
        );
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(14),
            egui::Stroke::new(1.0, LINE),
            egui::StrokeKind::Inside,
        );
        ui.painter().galley(
            pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0),
            galley,
            INK,
        );
        paint_icon(
            ui.painter(),
            Rect::from_center_size(
                pos2(rect.right() - 14.0, rect.center().y),
                Vec2::splat(14.0),
            ),
            "chevron",
            if hovered { HOVER_GREEN } else { MUTED },
        );
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        button.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Button, true, "Model and thinking")
        });

        let list_open_id = ui.make_persistent_id("model-list-open");
        let mut model_choice: Option<String> = None;
        let mut effort_changed = false;
        egui::Popup::from_toggle_button_response(&button)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            // Always open upward from the chip (the box sits at the bottom or middle of the window);
            // letting egui pick a side made it open down and then jump up when it did not fit.
            .align(egui::RectAlign::TOP_START)
            .align_alternatives(&[])
            .width(286.0)
            .gap(6.0)
            .show(|popup| {
                let mut list_open =
                    popup.data(|data| data.get_temp::<bool>(list_open_id).unwrap_or(false));
                popup.add_space(2.0);
                popup.label(egui::RichText::new("Model").size(10.5).color(MUTED));
                popup.add_space(3.0);
                // Clicking the current model expands the model list.
                let (row, row_response) = popup
                    .allocate_exact_size(Vec2::new(popup.available_width(), 34.0), Sense::click());
                popup.painter().rect_filled(
                    row,
                    CornerRadius::same(9),
                    if row_response.hovered() {
                        HOVER_FILL
                    } else {
                        MINT
                    },
                );
                popup.painter().text(
                    pos2(row.left() + 11.0, row.center().y),
                    egui::Align2::LEFT_CENTER,
                    &selected_name,
                    egui::FontId::proportional(12.5),
                    INK,
                );
                let chevron_rect = Rect::from_center_size(
                    pos2(row.right() - 16.0, row.center().y),
                    Vec2::splat(14.0),
                );
                let turn =
                    popup
                        .ctx()
                        .animate_bool_with_time(list_open_id.with("anim"), list_open, ANIM);
                if turn > 0.5 {
                    let c = chevron_rect.center();
                    popup.painter().line_segment(
                        [pos2(c.x - 3.5, c.y + 1.5), pos2(c.x, c.y - 2.0)],
                        egui::Stroke::new(1.6, MUTED),
                    );
                    popup.painter().line_segment(
                        [pos2(c.x, c.y - 2.0), pos2(c.x + 3.5, c.y + 1.5)],
                        egui::Stroke::new(1.6, MUTED),
                    );
                } else {
                    paint_icon(popup.painter(), chevron_rect, "chevron", MUTED);
                }
                if row_response.clicked() {
                    list_open = !list_open;
                }
                if turn > 0.0 {
                    popup.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .max_height(220.0 * turn)
                        .auto_shrink([false, true])
                        .show(popup, |list| {
                            for model in &models {
                                let is_current = model.slug == self.selected_model;
                                let (item, response) = list.allocate_exact_size(
                                    Vec2::new(list.available_width(), 30.0),
                                    Sense::click(),
                                );
                                if response.hovered() {
                                    list.painter().rect_filled(
                                        item,
                                        CornerRadius::same(8),
                                        HOVER_FILL,
                                    );
                                }
                                list.painter().text(
                                    pos2(item.left() + 11.0, item.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    &model.display_name,
                                    egui::FontId::proportional(12.0),
                                    if is_current {
                                        INK
                                    } else {
                                        Color32::from_rgb(196, 198, 196)
                                    },
                                );
                                if is_current {
                                    paint_icon(
                                        list.painter(),
                                        Rect::from_center_size(
                                            pos2(item.right() - 16.0, item.center().y),
                                            Vec2::splat(14.0),
                                        ),
                                        "check",
                                        HOVER_GREEN,
                                    );
                                }
                                if response.clicked() {
                                    model_choice = Some(model.slug.clone());
                                    list_open = false;
                                }
                            }
                        });
                }
                popup.data_mut(|data| data.insert_temp(list_open_id, list_open));
                popup.add_space(10.0);
                popup.horizontal(|ui| {
                    ui.label(egui::RichText::new("Thinking").size(10.5).color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(if reasoning_levels.is_empty() {
                                "fixed"
                            } else {
                                self.reasoning_effort.as_str()
                            })
                            .size(11.0)
                            .strong()
                            .color(INK),
                        );
                    });
                });
                if reasoning_levels.is_empty() {
                    popup.label(
                        egui::RichText::new("This model does not expose thinking levels.")
                            .size(10.0)
                            .color(MUTED),
                    );
                } else {
                    // Stepped slider: a track with one notch per level.
                    let steps = reasoning_levels.len();
                    let active_index = reasoning_levels
                        .iter()
                        .position(|level| level == &self.reasoning_effort)
                        .unwrap_or(0);
                    let (track_rect, track) = popup.allocate_exact_size(
                        Vec2::new(popup.available_width(), 26.0),
                        Sense::click_and_drag(),
                    );
                    let inset = 10.0;
                    let x_of = |index: usize| {
                        if steps <= 1 {
                            track_rect.center().x
                        } else {
                            track_rect.left()
                                + inset
                                + (track_rect.width() - inset * 2.0) * index as f32
                                    / (steps - 1) as f32
                        }
                    };
                    if let Some(pointer) = track.interact_pointer_pos() {
                        let nearest = (0..steps)
                            .min_by(|a, b| {
                                (x_of(*a) - pointer.x)
                                    .abs()
                                    .total_cmp(&(x_of(*b) - pointer.x).abs())
                            })
                            .unwrap_or(active_index);
                        if nearest != active_index {
                            self.reasoning_effort = reasoning_levels[nearest].clone();
                            effort_changed = true;
                        }
                    }
                    let current = reasoning_levels
                        .iter()
                        .position(|level| level == &self.reasoning_effort)
                        .unwrap_or(0);
                    let y = track_rect.center().y;
                    popup.painter().line_segment(
                        [pos2(x_of(0), y), pos2(x_of(steps - 1), y)],
                        egui::Stroke::new(3.0, MINT),
                    );
                    let knob_x = popup.ctx().animate_value_with_time(
                        popup.make_persistent_id("effort-knob"),
                        x_of(current),
                        0.15,
                    );
                    popup.painter().line_segment(
                        [pos2(x_of(0), y), pos2(knob_x, y)],
                        egui::Stroke::new(3.0, HOVER_GREEN),
                    );
                    for index in 0..steps {
                        popup.painter().circle_filled(
                            pos2(x_of(index), y),
                            2.5,
                            if index <= current {
                                HOVER_GREEN
                            } else {
                                Color32::from_rgb(70, 72, 71)
                            },
                        );
                    }
                    let glow = egui::Shadow {
                        offset: [0, 0],
                        blur: 12,
                        spread: 0,
                        color: Color32::from_rgba_unmultiplied(
                            GLOW_GREEN[0],
                            GLOW_GREEN[1],
                            GLOW_GREEN[2],
                            90,
                        ),
                    };
                    popup.painter().add(glow.as_shape(
                        Rect::from_center_size(pos2(knob_x, y), Vec2::splat(12.0)),
                        CornerRadius::same(6),
                    ));
                    popup
                        .painter()
                        .circle_filled(pos2(knob_x, y), 7.0, MINT_DEEP);
                    if track.hovered() {
                        popup.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    popup.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&reasoning_levels[0])
                                .size(10.0)
                                .color(MUTED),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(
                                    reasoning_levels.last().map(String::as_str).unwrap_or(""),
                                )
                                .size(10.0)
                                .color(MUTED),
                            );
                        });
                    });
                }
                popup.add_space(2.0);
            });
        if let Some(model) = model_choice {
            self.selected_model = model;
            if let Some(selected) = models.iter().find(|item| item.slug == self.selected_model) {
                if !selected.supported_reasoning_levels.is_empty()
                    && !selected
                        .supported_reasoning_levels
                        .contains(&self.reasoning_effort)
                {
                    self.reasoning_effort = selected.supported_reasoning_levels[0].clone();
                }
                self.context_window_reported = selected.context_window.is_some();
                self.context_window = selected
                    .context_window
                    .unwrap_or(chat::DEFAULT_CONTEXT_WINDOW);
            }
            effort_changed = true;
        }
        if effort_changed {
            if let Err(error) = save_selected_model(&self.selected_model, &self.reasoning_effort) {
                self.notice = error;
            }
        }
    }

    fn context_panel(&mut self, ui: &mut egui::Ui) {
        let dynamic_tool_tokens = self.last_dynamic_tool_tokens;
        // Inside a subagent's chat the panel shows that agent's own context; otherwise the main chat's.
        let agent = if self.side_tab == SideTab::Subagents { self.open_agent.as_deref().and_then(|id| team::hub().get(id)) } else { None };
        let subject = agent.as_ref().map(|a| a.id.clone()).unwrap_or_else(|| self.active_chat_id());
        if subject != self.context_subject {
            self.context_subject = subject;
            self.context_snap_frames = 3;
        }
        let estimate = if let Some(agent) = agent.as_ref() {
            let turns: Vec<Turn> = agent.turns.iter().filter(|turn| !(turn.role == "assistant" && turn.content.is_empty() && turn.tool_activity.is_empty())).cloned().collect();
            chat::estimate_context_tokens("", &turns, "")
                .saturating_add(toolset_tokens_estimate())
                .saturating_add(dynamic_tool_tokens)
        } else {
            self.context_tokens
        };
        let (latest_usage, total_usage) = if let Some(agent) = agent.as_ref() {
            ((agent.usage.input_tokens > 0).then(|| agent.usage.clone()), agent.usage_total.clone())
        } else {
            (
                (!self.context_usage_estimated).then(|| self.context_usage.clone()),
                self.conversations.get(self.active_conversation).map(|conversation| conversation.total_usage.clone()).unwrap_or_default(),
            )
        };
        // An agent's own request count is its context size; the main chat's is already folded
        // into `context_tokens` every frame (see `sync_main_context`).
        let reported_context = latest_usage.as_ref().map(|u| u.input_tokens.saturating_add(u.output_tokens)).unwrap_or(0);
        let estimate = if agent.is_some() && reported_context > 0 { reported_context } else { estimate };
        let limit = self.context_window.max(1);
        let used = estimate.min(limit);
        let system_target = chat::system_prompt_tokens().min(used);
        let tool_target = toolset_tokens_estimate()
            .saturating_add(dynamic_tool_tokens)
            .min(used.saturating_sub(system_target));
        // Every number and arc eases to its new value; a chat switch snaps them first.
        let duration = if self.context_snap_frames > 0 { 0.0 } else { ANIM };
        let ease = |name: &str, value: u64| {
            ui.ctx().animate_value_with_time(ui.make_persistent_id(("context-panel", name)), value as f32, duration)
        };
        let used_f = ease("used", used);
        let system_f = ease("system", system_target);
        let tool_f = ease("tools", tool_target);
        let ratio = (used_f / limit as f32).clamp(0.0, 1.0);
        let system_tokens = system_f.round() as u64;
        let tool_tokens = tool_f.round() as u64;
        let message_tokens = (used_f - system_f - tool_f).max(0.0).round() as u64;
        let animated_used = used_f.round() as u64;
        let heading = agent.as_ref().map(|a| format!("Context · {}", title_case(&a.name))).unwrap_or_else(|| "Context".into());
        let compact_percent = self.compact_percent;
        if (used_f - used as f32).abs() > 0.5 {
            ui.ctx().request_repaint();
        }
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                paint_icon(ui.painter(), icon, "context", HOVER_GREEN);
                ui.label(
                    egui::RichText::new(heading)
                        .size(13.0)
                        .strong()
                        .color(INK),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, "cancel", "Close context panel", 24.0).clicked() {
                        self.context_drawer_open = false;
                    }
                });
            });
            ui.add_space(6.0);
            // A solid scroll bar takes its own strip instead of floating over the rows.
            ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
            ui.style_mut().spacing.scroll.bar_inner_margin = 9.0;
            egui::ScrollArea::vertical()
                .id_salt("context-details")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // Ring gauge: segments for system prompt, tools and messages.
                        let (gauge, _) = ui.allocate_exact_size(Vec2::splat(74.0), Sense::hover());
                        let center = gauge.center();
                        let radius = 29.0;
                        ui.painter()
                            .circle_stroke(center, radius, egui::Stroke::new(6.0, MINT));
                        let arc = |from: f32, to: f32, color: Color32| {
                            if to - from <= 0.0005 {
                                return;
                            }
                            let points = (0..=48)
                                .map(|step| {
                                    let t = from + (to - from) * step as f32 / 48.0;
                                    let angle =
                                        -std::f32::consts::FRAC_PI_2 + t * std::f32::consts::TAU;
                                    pos2(
                                        center.x + radius * angle.cos(),
                                        center.y + radius * angle.sin(),
                                    )
                                })
                                .collect::<Vec<_>>();
                            ui.painter()
                                .add(egui::Shape::line(points, egui::Stroke::new(6.0, color)));
                        };
                        let (a, b) = (system_f / limit as f32, tool_f / limit as f32);
                        arc(0.0, a, SYSTEM_PROMPT_SEGMENT);
                        arc(a, a + b, TOOLSET_SEGMENT);
                        arc(a + b, ratio, MINT_DEEP);
                        ui.painter().text(
                            center - Vec2::new(0.0, 5.0),
                            egui::Align2::CENTER_CENTER,
                            format!("{}%", (ratio * 100.0).round() as u32),
                            egui::FontId::proportional(15.0),
                            INK,
                        );
                        ui.painter().text(
                            center + Vec2::new(0.0, 11.0),
                            egui::Align2::CENTER_CENTER,
                            "used",
                            egui::FontId::proportional(9.5),
                            MUTED,
                        );
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {}",
                                    format_tokens(animated_used),
                                    format_tokens(limit)
                                ))
                                .size(12.5)
                                .strong()
                                .color(INK),
                            );
                            ui.label(
                                egui::RichText::new(if reported_context > 0 { "Current context (service-reported)" } else { "Estimated current context" })
                                .size(10.0)
                                .color(MUTED),
                            );
                            ui.label(
                                egui::RichText::new(
                                    self.model_list
                                        .iter()
                                        .find(|model| model.slug == self.selected_model)
                                        .map(|model| model.display_name.clone())
                                        .unwrap_or_else(|| "No model".into()),
                                )
                                .size(10.0)
                                .color(MUTED),
                            );
                        });
                    });
                    ui.add_space(8.0);
                    context_segment_row(
                        ui,
                        SYSTEM_PROMPT_SEGMENT,
                        "System prompt",
                        format_tokens(system_tokens),
                    );
                    context_segment_row(
                        ui,
                        TOOLSET_SEGMENT,
                        "Tools & MCP",
                        format_tokens(tool_tokens),
                    );
                    context_segment_row(
                        ui,
                        MINT_DEEP,
                        "Messages & results",
                        format_tokens(message_tokens),
                    );
                    context_segment_row(
                        ui,
                        MINT,
                        "Free",
                        format_tokens(limit.saturating_sub(animated_used)),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);
                    let reported = |value: Option<u64>| value.map(format_tokens).unwrap_or_else(|| "—".into());
                    context_row(ui, "Latest input", reported(latest_usage.as_ref().map(|u| u.input_tokens)));
                    context_row(ui, "Latest output", reported(latest_usage.as_ref().map(|u| u.output_tokens)));
                    context_row(ui, "Cached input", reported(latest_usage.as_ref().filter(|u| u.cache_reported).map(|u| u.cached_tokens)));
                    context_row(ui, "Cache writes", reported(latest_usage.as_ref().filter(|u| u.cache_write_reported).map(|u| u.cache_write_tokens)));
                    ui.add_space(6.0);
                    context_row(ui, "Chat input total", format_tokens(total_usage.input_tokens));
                    context_row(ui, "Chat output total", format_tokens(total_usage.output_tokens));
                    context_row(ui, "Chat cached total", if total_usage.cache_reported { format_tokens(total_usage.cached_tokens) } else { "—".into() });
                    context_row(ui, "Chat cache writes", if total_usage.cache_write_reported { format_tokens(total_usage.cache_write_tokens) } else { "—".into() });
                    ui.add_space(6.0);
                    context_row(
                        ui,
                        "Window",
                        if self.context_window_reported {
                            format_tokens(limit)
                        } else {
                            format!("{} (fallback)", format_tokens(limit))
                        },
                    );
                    context_row(ui, "Auto-compact at", format!("{compact_percent}%"));
                    if self.streaming && self.active_tool.as_deref() == Some("compact_context") {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Compacting older turns…")
                                .size(11.0)
                                .color(HOVER_GREEN),
                        );
                    }
                });
        });
    }

    fn device_code_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(status) = self.device_code.clone() {
            egui::Frame::new()
                .fill(RAISED)
                .stroke(egui::Stroke::new(1.0, Color32::from_rgb(44, 92, 64)))
                .corner_radius(CornerRadius::same(14))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(120),
                })
                .inner_margin(Margin {
                    left: 16,
                    right: 14,
                    top: 10,
                    bottom: 10,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("Sign in with ChatGPT")
                                    .size(12.0)
                                    .strong()
                                    .color(TEXT),
                            );
                            ui.label(egui::RichText::new(&status).size(16.0).strong().color(INK));
                        });
                        let copy = icon_button(ui, "copy", "Copy device code", 30.0);
                        if copy.clicked() {
                            ui.ctx().copy_text(status.clone());
                            self.notice = "Device code copied.".into();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if action_button(ui, "globe", "Open sign-in page", true).clicked() {
                                if let Err(error) = auth::open_verification_page() {
                                    self.notice = error;
                                }
                            }
                        });
                    });
                    if self.notice.to_ascii_lowercase().contains("allow")
                        || self.notice.to_ascii_lowercase().contains("failed")
                        || self.notice.to_ascii_lowercase().contains("error")
                        || self.notice.to_ascii_lowercase().contains("permission")
                    {
                        ui.add_space(5.0);
                        ui.label(egui::RichText::new(&self.notice).size(11.0).color(DANGER));
                    }
                });
            ui.add_space(10.0);
        }
    }

    fn desktop_panel(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Computer").size(21.0).strong().color(INK));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if action_button(ui, "chat", "Back to chat", false).clicked() { self.show_tools = false; }
                });
            });
            ui.label(egui::RichText::new("Choose a running app or window. JEV reads its live accessibility tree before acting.").size(12.0).color(MUTED));
            ui.add_space(14.0);
            egui::ScrollArea::vertical().id_salt("computer-workspace").auto_shrink([false, false]).show(ui, |ui| {
                ui.label(egui::RichText::new("App or process").size(12.0).strong().color(INK));
                ui.add(egui::TextEdit::singleline(&mut self.target_app)
                    .hint_text(egui::RichText::new("Discord, chrome.exe, or another app").color(INK)).desired_width(f32::INFINITY));
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Window ID (optional)").size(12.0).strong().color(INK));
                ui.add(egui::TextEdit::singleline(&mut self.target_window_id)
                    .hint_text(egui::RichText::new("Leave empty to use the focused app window").color(INK)).desired_width(f32::INFINITY));
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    let enabled = self.pending.is_none();
                    if ui.add_enabled_ui(enabled, |ui| action_button(ui, "chart", "Running apps", false)).inner.clicked() { self.refresh_apps(); }
                    if ui.add_enabled_ui(enabled, |ui| action_button(ui, "folder", "Windows", false)).inner.clicked() { self.refresh_windows(); }
                    if ui.add_enabled_ui(enabled && !self.target_app.trim().is_empty(), |ui| action_button(ui, "use", "Open / focus app", true)).inner.clicked() { self.launch_target(); }
                });
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Snapshot target").size(12.0).strong().color(INK));
                egui::ComboBox::from_id_salt("surface-picker").selected_text(&self.surface).width(ui.available_width()).show_ui(ui, |ui| {
                    for surface in ["focused", "window", "menu", "menubar", "desktop", "taskbar", "start-menu"] { ui.selectable_value(&mut self.surface, surface.to_owned(), surface); }
                });
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled_ui(self.pending.is_none(), |ui| action_button(ui, "context", "Read live controls", true)).inner.clicked() { self.snapshot(false); }
                    if ui.add_enabled_ui(self.pending.is_none(), |ui| action_button(ui, "download", "Screenshot", false)).inner.clicked() { self.take_local_screenshot(); }
                    if ui.add_enabled_ui(self.pending.is_none(), |ui| action_button(ui, "globe", "Build local map", false)).inner.clicked() { self.snapshot(true); }
                });
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Fresh control reference").size(12.0).strong().color(INK));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.element_ref)
                        .hint_text(egui::RichText::new("Paste a qualified ref from the latest snapshot").color(INK)).desired_width(ui.available_width() - 110.0));
                    if ui.add_enabled_ui(self.pending.is_none(), |ui| action_button(ui, "check", "Click ref", false)).inner.clicked() { self.click_ref(); }
                });
                ui.add_space(8.0);
                ui.label(egui::RichText::new(&self.desktop_status).size(11.0).color(MUTED));
                if let Some(path) = &self.local_visualization {
                    ui.label(egui::RichText::new(format!("Local UI map: {path}")).size(10.0).color(MUTED));
                }
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Latest Agent Desktop result").size(12.0).strong().color(INK));
                egui::Frame::new().fill(SHELL).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut self.desktop_json).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY).desired_rows(10));
                });
            });
        });
    }

    fn primary_workspace(&mut self, ui: &mut egui::Ui) {
        if self.show_tools {
            // Chat stream/tool events can request an auto-scroll while the
            // transcript is not being rendered. Do not carry that stale
            // request across the Computer → Chat transition; it makes the
            // transcript jump when the Chat viewport is mounted again.
            self.scroll_to_bottom = false;
            self.desktop_panel(ui);
            return;
        }

        let available = ui.max_rect();
        let empty = self
            .conversations
            .get(self.active_conversation)
            .is_none_or(|c| c.turns.is_empty());
        let compact =
            ui.ctx()
                .animate_bool_with_time(ui.make_persistent_id("composer-docked"), !empty, ANIM);
        let compact_height = 118.0_f32.min((available.height() - 120.0).max(104.0));
        let center_height =
            (compact_height + 48.0).min((available.height() * 0.42).max(compact_height));
        let center_width = (available.width() * 0.74)
            .clamp(420.0, 760.0)
            .min(available.width());
        // New-chat page: heading + chips + text box are one block, centered on the panel.
        let heading_height = 56.0;
        let block_top = available.center().y - (heading_height + center_height) / 2.0;
        let center_top = block_top + heading_height;
        // The text box grows with its text: downward from its centered spot on the new-chat page
        // (stopping at the bottom of the panel), upward from the bottom once a chat is open (at
        // most six rows). Height changes ease over ANIM.
        let body_style = egui::TextStyle::Body;
        let row_height = ui.text_style_height(&body_style);
        let wrap_width = (center_width + (available.width() - center_width) * compact - 40.0).max(80.0);
        let rows = if self.draft.is_empty() {
            1
        } else {
            let font = body_style.resolve(ui.style());
            ui.fonts_mut(|fonts| fonts.layout(self.draft.clone(), font, INK, wrap_width)).rows.len().max(1)
        };
        let wanted_extra = rows.saturating_sub(2) as f32 * row_height;
        let center_cap = (available.bottom() - 10.0 - (center_top + center_height)).max(0.0);
        let docked_cap = 4.0 * row_height;
        let extra_cap = center_cap + (docked_cap - center_cap) * compact;
        let extra = ui.ctx().animate_value_with_time(
            ui.make_persistent_id("composer-extra"),
            wanted_extra.min(extra_cap).max(0.0),
            ANIM,
        );
        self.composer_extra = extra;
        let bottom_rect = Rect::from_min_max(
            pos2(available.left(), available.bottom() - compact_height - extra),
            available.max,
        );
        let center_rect = Rect::from_min_size(
            pos2(available.center().x - center_width / 2.0, center_top),
            Vec2::new(center_width, center_height + extra),
        );
        let composer_rect = Rect::from_min_max(
            center_rect.min + (bottom_rect.min - center_rect.min) * compact,
            center_rect.max + (bottom_rect.max - center_rect.max) * compact,
        );
        // The glow follows the same animated rectangle as the composer: broad
        // and visible on the new-chat landing state, then tighter as it docks.
        // epaint's blurred rounded-rect shadow gives one continuous halo rather
        // than several visible outline bands.
        // A broad, soft light behind the composer: wide and gentle on the
        // new-chat screen, fading to a faint halo once it docks at the bottom.
        // While an earlier message is being edited in place, the composer is
        // hidden; so are its light and shadow.
        let composer_visible = ui.ctx().animate_bool_with_time(
            ui.make_persistent_id("composer-visible"),
            self.editing_turn.is_none(),
            ANIM,
        );
        if composer_visible > 0.0 {
            let glow_painter = ui.painter().with_clip_rect(available.expand(14.0));
            let reach = Vec2::new(
                composer_rect.width() * (0.95 - compact * 0.35),
                composer_rect.height() * (2.6 - compact * 1.4),
            );
            radial_glow(
                &glow_painter,
                composer_rect.center() + Vec2::new(0.0, 10.0),
                reach,
                GLOW_GREEN,
                (0.20 - compact * 0.14) * composer_visible,
            );
            let lift = egui::Shadow {
                offset: [0, 10],
                blur: 34,
                spread: 0,
                color: Color32::from_black_alpha((130.0 * composer_visible) as u8),
            };
            ui.painter()
                .add(lift.as_shape(composer_rect, CornerRadius::same(18)));
        }
        // ask_user / propose_plan: a panel grows out of the top of the composer.
        let interaction = self.current_interaction();
        if let Some(active) = interaction.as_ref() {
            self.interaction_height = self.interaction_height(active, available.height());
        }
        let interaction_progress = ui.ctx().animate_bool_with_time(
            ui.make_persistent_id("interaction-open"),
            interaction.is_some(),
            ANIM,
        );
        let panel_height = self.interaction_height * interaction_progress;
        self.interaction_t = interaction_progress;
        let chat_rect = Rect::from_min_max(
            available.min,
            pos2(available.right(), composer_rect.top() - panel_height - 10.0),
        );
        let mut chat = ui.new_child(
            UiBuilder::new()
                .id_salt("chat-workspace")
                .max_rect(chat_rect),
        );
        self.chat_panel(&mut chat);
        if empty {
            let greeting = Rect::from_center_size(
                pos2(available.center().x, block_top + heading_height / 2.0),
                Vec2::new(available.width(), 56.0),
            );
            let mut welcome = ui.new_child(
                UiBuilder::new()
                    .layout(egui::Layout::top_down(egui::Align::Center))
                    .id_salt("empty-chat-heading")
                    .max_rect(greeting),
            );
            welcome.label(
                egui::RichText::new("Your desktop, through JEV")
                    .size(25.0)
                    .color(INK),
            );
            welcome.label(
                egui::RichText::new("Ask, explore, or hand off a task")
                    .size(12.0)
                    .color(MUTED),
            );
        }
        let mut composer = ui.new_child(
            UiBuilder::new()
                .layout(egui::Layout::top_down(egui::Align::Min))
                .id_salt("composer-workspace")
                .max_rect(composer_rect),
        );
        if self.editing_turn.is_none() {
            self.composer(&mut composer);
        }
        if panel_height > 1.0 {
            let panel_rect = Rect::from_min_max(
                pos2(composer_rect.left(), composer_rect.top() - panel_height),
                pos2(composer_rect.right(), composer_rect.top()),
            );
            self.interaction_panel(ui, panel_rect, interaction, interaction_progress);
        }
    }
}

impl eframe::App for JarvusApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.initialize();
        self.poll(ui.ctx());
        self.drain_queue();
        let active_chat = self.active_chat_id();
        if active_chat != self.context_chat_id {
            self.context_chat_id = active_chat;
            if !self.streaming {
                self.reset_context_for_active_chat();
            }
        }
        self.sync_main_context();
        self.context_snap_frames = self.context_snap_frames.saturating_sub(1);
        // The transparent native client area must not inherit egui's panel outline;
        // the visible shell is painted explicitly below.
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE),
            )
            .show(ui, |ui| {
                let viewport = ui.max_rect();
                let maximized = ui
                    .ctx()
                    .input(|input| input.viewport().maximized.unwrap_or(false));
                if !self.native_frame_style_attempted {
                    // This mutates native styles and can trigger a non-client redraw. Retry
                    // loops on a DWM error caused repeated frame resets on every focus repaint.
                    let _ = apply_native_corner_preference(frame);
                    self.native_frame_style_attempted = true;
                }
                let outer = if maximized { 0.0 } else { 8.0 };
                let radius = if maximized { 0 } else { 10 };
                let shell_rect = viewport.shrink(outer);
                ui.painter().rect_filled(shell_rect, radius, SHELL);
                // Every decorative effect uses this shell-bounded painter. Animated
                // panels and resized drawers can never paint into the transparent
                // native window margin or onto the desktop behind JARVUS.
                let inner = shell_rect.shrink(10.0);
                let top_height = 48.0;
                let top_rect = Rect::from_min_size(inner.min, Vec2::new(inner.width(), top_height));
                let mut top = ui.new_child(
                    UiBuilder::new()
                        .id_salt("jarvus-titlebar")
                        .max_rect(top_rect),
                );
                self.top_bar(&mut top);

                let show_code = !self.signed_in && self.device_code.is_some();
                let code_progress = ui.ctx().animate_bool_with_time(
                    ui.make_persistent_id("device-code-open"),
                    show_code,
                    ANIM,
                );
                let device_height = 78.0 * code_progress;
                let body_top = top_rect.bottom() + 10.0 + device_height;
                if device_height > 1.0 {
                    let code_rect = Rect::from_min_size(
                        pos2(inner.left(), top_rect.bottom() + 6.0),
                        Vec2::new(inner.width(), 70.0 * code_progress),
                    );
                    let mut code =
                        ui.new_child(UiBuilder::new().id_salt("device-code").max_rect(code_rect));
                    code.set_clip_rect(code_rect);
                    code.multiply_opacity(code_progress);
                    self.device_code_panel(&mut code);
                }
                let body = Rect::from_min_max(
                    pos2(inner.left(), body_top),
                    pos2(inner.right(), inner.bottom()),
                );
                let quick_width = 42.0;
                let gap = 8.0;
                let history_width = self
                    .history_panel_width
                    .clamp(190.0, (body.width() * 0.42).max(190.0));
                let context_progress = ui.ctx().animate_bool_with_time(
                    ui.make_persistent_id("context-panel-open"),
                    self.context_drawer_open,
                    ANIM,
                );
                let side_rect =
                    Rect::from_min_max(pos2(body.right() - history_width, body.top()), body.max);
                let context_height = (side_rect.height() * 0.45).max(0.0) * context_progress;
                let context_rect = Rect::from_min_max(
                    pos2(side_rect.left(), side_rect.bottom() - context_height),
                    side_rect.max,
                );
                let history_bottom = if context_height > 0.5 {
                    context_rect.top() - gap
                } else {
                    side_rect.bottom()
                };
                let history_rect =
                    Rect::from_min_max(side_rect.min, pos2(side_rect.right(), history_bottom));
                let primary_left = body.left() + quick_width + gap;
                let primary_rect = Rect::from_min_max(
                    pos2(primary_left, body.top()),
                    pos2(side_rect.left() - gap, body.bottom()),
                );

                let rail_rect =
                    Rect::from_min_size(body.min, Vec2::new(quick_width, body.height()));
                let mut rail = ui.new_child(
                    UiBuilder::new()
                        .layout(egui::Layout::top_down(egui::Align::Center))
                        .id_salt("quick-actions")
                        .max_rect(rail_rect),
                );
                let new_chat = icon_button(&mut rail, "plus", "New chat", 36.0);
                if new_chat.clicked() {
                    self.new_chat();
                }
                rail.add_space(10.0);
                let context_button = icon_button(&mut rail, "context", "Context usage", 32.0);
                if context_button.clicked() {
                    self.context_drawer_open = !self.context_drawer_open;
                }
                rail.add_space(10.0);
                let agents_button = icon_button(&mut rail, "agents", "Subagents panel", 32.0);
                if agents_button.clicked() {
                    self.side_tab = if self.side_tab == SideTab::Subagents { SideTab::History } else { SideTab::Subagents };
                    self.open_agent = None;
                }

                let empty_chat = self
                    .conversations
                    .get(self.active_conversation)
                    .is_none_or(|conversation| conversation.turns.is_empty());
                let glow_strength = ui.ctx().animate_value_with_time(
                    ui.make_persistent_id("corner-glow-strength"),
                    if empty_chat { 1.0 } else { 0.8 },
                    ANIM,
                );
                paint_inside_rounded_shell(ui, shell_rect, radius as f32, |painter| draw_panel_shadow(painter, primary_rect));
                ui.painter()
                    .rect_filled(primary_rect, CornerRadius::same(14), SURFACE);
                paint_inside_rounded_shell(ui, shell_rect, radius as f32, |painter| draw_corner_glows(painter, primary_rect, glow_strength));
                corner_glow_patches(ui, shell_rect, radius as f32, &corner_glow_specs(primary_rect, glow_strength));
                ui.painter().rect_stroke(
                    primary_rect,
                    CornerRadius::same(14),
                    egui::Stroke::new(1.0, Color32::from_rgb(34, 36, 35)),
                    egui::StrokeKind::Inside,
                );
                paint_inside_rounded_shell(ui, shell_rect, radius as f32, |painter| draw_panel_shadow(painter, history_rect));
                ui.painter()
                    .rect_filled(history_rect, CornerRadius::same(14), SURFACE);
                ui.painter().rect_stroke(
                    history_rect,
                    CornerRadius::same(14),
                    egui::Stroke::new(1.0, Color32::from_rgb(34, 36, 35)),
                    egui::StrokeKind::Inside,
                );
                if context_height > 0.5 {
                    paint_inside_rounded_shell(ui, shell_rect, radius as f32, |painter| draw_panel_shadow(painter, context_rect));
                    ui.painter()
                        .rect_filled(context_rect, CornerRadius::same(14), SURFACE);
                    ui.painter().rect_stroke(
                        context_rect,
                        CornerRadius::same(14),
                        egui::Stroke::new(1.0, Color32::from_rgb(34, 36, 35)),
                        egui::StrokeKind::Inside,
                    );
                    let mut context = ui.new_child(
                        UiBuilder::new()
                            .id_salt("context-panel")
                            .max_rect(context_rect.shrink(13.0)),
                    );
                    context.set_clip_rect(context_rect);
                    context.multiply_opacity(context_progress);
                    self.context_panel(&mut context);
                }
                let mut history = ui.new_child(
                    UiBuilder::new()
                        .id_salt("history-rail")
                        .max_rect(history_rect.shrink(13.0)),
                );
                self.history_panel(&mut history);
                let history_handle_rect = Rect::from_min_max(
                    pos2(history_rect.left() - 7.0, history_rect.top()),
                    pos2(history_rect.left() + 7.0, history_rect.bottom()),
                );
                let history_handle = ui.interact(
                    history_handle_rect,
                    ui.make_persistent_id("history-panel-resize"),
                    Sense::drag(),
                );
                history_handle.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Splitter, true, "Resize history panel")
                });
                if history_handle.hovered() || history_handle.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if history_handle.drag_started() {
                    self.history_resize_start = self.history_panel_width;
                }
                if history_handle.dragged() {
                    // drag_delta is this frame's movement, so it is applied to the current width.
                    self.history_panel_width = (self.history_panel_width
                        - history_handle.drag_delta().x)
                        .clamp(190.0, 480.0);
                }
                let mut primary = ui.new_child(
                    UiBuilder::new()
                        .id_salt("primary-workspace")
                        .max_rect(primary_rect.shrink(14.0)),
                );
                self.primary_workspace(&mut primary);

                if !maximized {
                    begin_window_resize(ui, viewport);
                }
            });
        self.settings_modal(ui.ctx());
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn settings_path() -> Option<PathBuf> {
    chat_store::app_data_root().map(|root| root.join("settings.json"))
}

fn downloads_directory() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .map(|root| root.join("Downloads"))
}

fn open_local_path(path: PathBuf) -> String {
    #[cfg(target_os = "windows")]
    {
        match std::process::Command::new("explorer.exe")
            .arg(&path)
            .spawn()
        {
            Ok(_) => format!("Opened {}.", path.display()),
            Err(error) => format!("Could not open {}: {error}", path.display()),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(target_os = "macos")]
        let command = "open";
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let command = "xdg-open";
        match std::process::Command::new(command).arg(&path).spawn() {
            Ok(_) => format!("Opened {}.", path.display()),
            Err(error) => format!("Could not open {}: {error}", path.display()),
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        format!("{kib:.1} KB")
    } else if kib < 1024.0 * 1024.0 {
        format!("{:.1} MB", kib / 1024.0)
    } else {
        format!("{:.2} GB", kib / 1024.0 / 1024.0)
    }
}

/// The Chrome extension folder: bundled next to the app (`JARVUS.app/Contents/Resources` on a Mac,
/// beside the executable elsewhere), falling back to the source checkout it was built from.
fn extension_package_path() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from)) {
        candidates.push(dir.join("chrome-extension"));
        candidates.push(dir.join("../Resources/chrome-extension"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chrome-extension"));
    candidates
        .iter()
        .find(|path| path.join("manifest.json").is_file())
        .cloned()
        .unwrap_or_else(|| candidates.last().cloned().unwrap_or_default())
}

fn new_conversation() -> Conversation {
    Conversation {
        id: new_conversation_id(),
        title: "New chat".into(),
        loaded: true,
        ..Default::default()
    }
}

fn new_conversation_id() -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("chat-{}-{nanos:x}-{counter:x}", std::process::id())
}

fn conversation_preview(turns: &[Turn]) -> String {
    turns
        .iter()
        .rev()
        .find(|turn| !turn.content.trim().is_empty())
        .map(|turn| {
            turn.content
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
        .chars()
        .take(96)
        .collect()
}

fn load_conversation_body(conversation: &mut Conversation) {
    let history = chat_store::read_chat_file::<StoredHistory>(&conversation.id, "history.json")
        .ok()
        .flatten()
        .unwrap_or_default();
    let calls = chat_store::read_chat_file::<StoredToolcalls>(&conversation.id, "toolcalls.json")
        .ok()
        .flatten()
        .unwrap_or_default();
    conversation.turns = history.turns;
    for (index, activities) in calls.per_turn.iter().enumerate() {
        if let Some(turn) = conversation.turns.get_mut(index) {
            turn.tool_activity = activities.clone();
        }
    }
    conversation.context_summary = history.context_summary;
    conversation.summary_through = history.summary_through.min(conversation.turns.len());
    conversation.loaded = true;
    conversation.message_count = conversation.message_count.max(conversation.turns.len());
    if !conversation.turns.is_empty() {
        conversation.preview = conversation_preview(&conversation.turns);
    }
    if conversation.title.trim().is_empty() {
        conversation.title = "New chat".into();
    }
    if !conversation.turns.is_empty() {
        conversation.title_generated = true;
    }
}

fn load_conversation_store() -> (Vec<Conversation>, usize) {
    if let Ok(Some(index)) = chat_store::load_index() {
        let mut conversations: Vec<Conversation> = index
            .chat_ids
            .iter()
            .filter_map(|id| {
                chat_store::read_chat_file::<Conversation>(id, "metadata.json")
                    .ok()
                    .flatten()
            })
            .filter(|conversation| chat_store::is_valid_chat_id(&conversation.id))
            .collect();
        for conversation in &mut conversations {
            if conversation.title.trim().is_empty() {
                conversation.title = "New chat".into();
            }
            conversation.turns.clear();
            conversation.context_summary.clear();
            conversation.summary_through = 0;
            conversation.loaded = false;
        }
        if !conversations.is_empty() {
            let active = index
                .active_chat_id
                .as_ref()
                .and_then(|id| conversations.iter().position(|chat| &chat.id == id))
                .unwrap_or(0);
            load_conversation_body(&mut conversations[active]);
            return (conversations, active);
        }
    }

    // Import the legacy all-in-one file once, retaining it as a backup until
    // the split files and index have all been persisted successfully.
    let legacy = chat_store::legacy_conversations_path()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<ConversationStore>(&bytes).ok());
    if let Some(mut store) = legacy {
        if !store.conversations.is_empty() {
            store.active_conversation =
                store.active_conversation.min(store.conversations.len() - 1);
            for conversation in &mut store.conversations {
                if !chat_store::is_valid_chat_id(&conversation.id) {
                    conversation.id = new_conversation_id();
                }
                if conversation.title.trim().is_empty() {
                    conversation.title = "New chat".into();
                }
                conversation.loaded = true;
                conversation.message_count = conversation.turns.len();
                conversation.preview = conversation_preview(&conversation.turns);
                if !conversation.turns.is_empty() {
                    conversation.title_generated = true;
                }
                conversation.summary_through =
                    conversation.summary_through.min(conversation.turns.len());
            }
            if save_conversation_store(&mut store.conversations, store.active_conversation).is_ok()
            {
                for (index, conversation) in store.conversations.iter_mut().enumerate() {
                    if index != store.active_conversation {
                        conversation.turns.clear();
                        conversation.context_summary.clear();
                        conversation.summary_through = 0;
                        conversation.loaded = false;
                    }
                }
            }
            return (store.conversations, store.active_conversation);
        }
    }
    (vec![new_conversation()], 0)
}

fn save_conversation_store(
    conversations: &mut [Conversation],
    active_conversation: usize,
) -> Result<(), String> {
    let mut chat_ids = Vec::with_capacity(conversations.len());
    for conversation in conversations.iter_mut() {
        if !chat_store::is_valid_chat_id(&conversation.id) {
            return Err("A chat has an invalid local ID and was not saved.".into());
        }
        if conversation.loaded {
            conversation.message_count = conversation.turns.len();
            conversation.preview = conversation_preview(&conversation.turns);
            let mut turns = conversation.turns.clone();
            let per_turn = turns
                .iter()
                .map(|turn| turn.tool_activity.clone())
                .collect();
            for turn in &mut turns {
                turn.tool_activity.clear();
            }
            let history = StoredHistory {
                turns,
                context_summary: conversation.context_summary.clone(),
                summary_through: conversation.summary_through.min(conversation.turns.len()),
            };
            chat_store::write_chat_file(&conversation.id, "history.json", &history)
                .map_err(|error| format!("Could not write chat history: {error}"))?;
            chat_store::write_chat_file(
                &conversation.id,
                "toolcalls.json",
                &StoredToolcalls { per_turn },
            )
            .map_err(|error| format!("Could not write chat tool calls: {error}"))?;
        }
        let mut metadata = conversation.clone();
        metadata.turns.clear();
        metadata.context_summary.clear();
        metadata.summary_through = 0;
        metadata.loaded = false;
        chat_store::write_chat_file(&conversation.id, "metadata.json", &metadata)
            .map_err(|error| format!("Could not write chat metadata: {error}"))?;
        chat_ids.push(conversation.id.clone());
    }
    let active_chat_id = conversations
        .get(active_conversation)
        .map(|conversation| conversation.id.clone());
    chat_store::save_index(&chat_store::ChatIndex {
        chat_ids,
        active_chat_id,
    })
    .map_err(|error| format!("Could not update chat index: {error}"))
}

fn load_selected_model() -> Option<String> {
    settings_values()?
        .get("selected_model")?
        .as_str()
        .map(str::to_owned)
}

fn load_subagent_model() -> Option<String> {
    settings_values()?
        .get("subagent_model")?
        .as_str()
        .map(str::to_owned)
        .filter(|model| !model.is_empty())
}

fn save_subagent_model(model: &str) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "Could not locate your home folder.".to_owned())?;
    let mut settings = settings_values().filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    settings["subagent_model"] = Value::String(model.to_owned());
    chat_store::write_json_atomic(&path, &settings).map_err(|error| format!("Could not save the subagent model: {error}"))
}

fn load_compact_percent() -> Option<u8> {
    let value = settings_values()?.get("compact_percent")?.as_u64()?;
    Some(chat::clamp_compact_percent(value.min(255) as u8))
}

fn save_compact_percent(percent: u8) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "Could not locate your home folder.".to_owned())?;
    let mut settings = settings_values().filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    settings["compact_percent"] = Value::from(percent);
    chat_store::write_json_atomic(&path, &settings).map_err(|error| format!("Could not save the auto-compact setting: {error}"))
}

fn load_reasoning_effort() -> Option<String> {
    settings_values()?
        .get("reasoning_effort")?
        .as_str()
        .map(str::to_owned)
}

fn settings_values() -> Option<Value> {
    let current_path = settings_path()?;
    if let Ok(bytes) = fs::read(&current_path) {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Some(value);
        }
    }
    let legacy = chat_store::legacy_settings_path()?;
    let bytes = fs::read(legacy).ok()?;
    let value = serde_json::from_slice::<Value>(&bytes).ok()?;
    let _ = chat_store::write_json_atomic(&current_path, &value);
    Some(value)
}

fn save_selected_model(model: &str, reasoning_effort: &str) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| {
        "Could not locate your home folder to save the selected model.".to_owned()
    })?;
    let mut settings = settings_values()
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    settings["selected_model"] = Value::String(model.to_owned());
    settings["reasoning_effort"] = Value::String(reasoning_effort.to_owned());
    chat_store::write_json_atomic(&path, &settings)
        .map_err(|error| format!("Could not save selected model: {error}"))
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{}K", tokens / 1_000)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}m {remaining_seconds}s")
    } else {
        format!("{remaining_seconds}s")
    }
}

fn context_row(ui: &mut egui::Ui, label: &str, value: String) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(11.0).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).size(11.0).color(INK));
        });
    });
}

/// A rounded card holding a column of settings rows.
fn settings_group(ui: &mut egui::Ui, title: Option<&str>, add: impl FnOnce(&mut egui::Ui)) {
    if let Some(title) = title {
        ui.label(egui::RichText::new(title).size(12.0).strong().color(MUTED));
        ui.add_space(6.0);
    }
    egui::Frame::new()
        .fill(RAISED)
        .stroke(egui::Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin {
            left: 14,
            right: 12,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(16.0);
}

/// One settings row: optional status dot, title and detail on the left,
/// controls right-aligned.
fn settings_row(
    ui: &mut egui::Ui,
    dot: Option<Color32>,
    title: &str,
    detail: &str,
    controls: impl FnOnce(&mut egui::Ui),
) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(width, 52.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_height(52.0);
            if let Some(color) = dot {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 6.0, color.gamma_multiply(0.22));
                ui.painter().circle_filled(rect.center(), 3.5, color);
                ui.add_space(4.0);
            }
            let text_width = (ui.available_width() * 0.58).max(160.0);
            ui.allocate_ui_with_layout(
                Vec2::new(text_width, 44.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new(title).size(12.5).color(INK));
                    ui.add(
                        egui::Label::new(egui::RichText::new(detail).size(10.5).color(MUTED))
                            .truncate(),
                    );
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                controls(ui)
            });
        },
    );
}

fn row_divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(1.0, LINE),
    );
}

/// Icon on the left, text on the right. `primary` = deep green fill, white text.
fn action_button(ui: &mut egui::Ui, icon: &str, label: &str, primary: bool) -> egui::Response {
    action_button_styled(
        ui,
        icon,
        label,
        if primary {
            Some((Color32::from_rgb(33, 78, 55), INK))
        } else {
            None
        },
    )
}

fn action_button_colored(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    color: Color32,
) -> egui::Response {
    action_button_styled(ui, icon, label, Some((color.gamma_multiply(0.22), color)))
}

fn action_button_styled(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    colors: Option<(Color32, Color32)>,
) -> egui::Response {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), egui::FontId::proportional(11.5), INK);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 40.0, 32.0), Sense::click());
    let enabled = ui.is_enabled();
    let hovered = response.hovered() && enabled;
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id.with("hover"), hovered, 0.12);
    let (fill, text) = match colors {
        Some((fill, text)) => (
            if hovered {
                fill.gamma_multiply(1.15)
            } else {
                fill
            },
            text,
        ),
        None => (
            lerp_color(MINT, HOVER_FILL, hover),
            lerp_color(INK, HOVER_GREEN, hover),
        ),
    };
    let alpha = if enabled { 1.0 } else { 0.4 };
    if hover > 0.0 && colors.is_none() {
        let glow = egui::Shadow {
            offset: [0, 0],
            blur: 14,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(
                GLOW_GREEN[0],
                GLOW_GREEN[1],
                GLOW_GREEN[2],
                (40.0 * hover) as u8,
            ),
        };
        ui.painter()
            .add(glow.as_shape(rect, CornerRadius::same(10)));
    }
    ui.painter()
        .rect_filled(rect, CornerRadius::same(10), fill.gamma_multiply(alpha));
    if colors.is_none() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(10),
            egui::Stroke::new(1.0, LINE),
            egui::StrokeKind::Inside,
        );
    }
    paint_icon(
        ui.painter(),
        Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), Vec2::splat(16.0)),
        icon,
        text.gamma_multiply(alpha),
    );
    ui.painter().galley(
        pos2(rect.left() + 29.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text.gamma_multiply(alpha),
    );
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    response
}

fn short_error(error: &str) -> String {
    let first = error.lines().next().unwrap_or(error).trim();
    if first.chars().count() > 110 {
        format!("{}…", first.chars().take(110).collect::<String>())
    } else {
        first.to_owned()
    }
}

fn app_data_root() -> Option<PathBuf> {
    chat_store::app_data_root()
}

fn captures_directory() -> PathBuf {
    chat_store::captures_dir()
}

fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 50_000 {
                return total;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Chat ids embed their creation time: `chat-{pid}-{nanos:x}-{counter:x}`.
fn conversation_created_ns(id: &str) -> Option<u128> {
    u128::from_str_radix(id.split('-').nth(2)?, 16).ok()
}

fn is_local_desktop_tool(name: &str) -> bool {
    matches!(
        name,
        "open_app"
            | "list_processes"
            | "inspect_desktop"
            | "click_ui_control"
            | "type_text"
            | "press_key"
            | "jev_run_plan"
    ) || name.starts_with("desktop_")
        || name.starts_with("browser__")
}

fn local_action_duration_label(milliseconds: Option<u128>) -> String {
    milliseconds
        .map(|ms| format!("{ms} ms"))
        .unwrap_or_else(|| "-- ms".to_owned())
}

fn chat_send_is_allowed(
    operation_pending: bool,
    streaming: bool,
    signed_in: bool,
    has_model: bool,
    has_prompt: bool,
) -> bool {
    !operation_pending && !streaming && signed_in && has_model && has_prompt
}

/// The "Compose Mode" switch in the chat box: read-only investigation and a plan to accept.
fn compose_toggle(ui: &mut egui::Ui, on: bool, enabled: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(response.id.with("compose-hover"), response.hovered() && enabled, ANIM);
    let state = ui.ctx().animate_bool_with_time(response.id.with("compose-on"), on, ANIM);
    let accent = lerp_color(MUTED, HOVER_GREEN, state.max(hover * 0.7));
    // The circle and the brain fade into the accent green together when switched on.
    ui.painter().circle_filled(rect.center(), 15.0, lerp_color(Color32::TRANSPARENT, HOVER_FILL, state.max(hover * 0.4)));
    ui.painter().circle_stroke(rect.center(), 14.5 - 0.5 * state, egui::Stroke::new(1.0 + state, lerp_color(LINE, HOVER_GREEN, state)));
    if state > 0.01 {
        let glow = egui::Shadow { offset: [0, 0], blur: 10, spread: 0, color: Color32::from_rgba_unmultiplied(GLOW_GREEN[0], GLOW_GREEN[1], GLOW_GREEN[2], (46.0 * state) as u8) };
        ui.painter().add(glow.as_shape(rect.shrink(4.0), CornerRadius::same(11)));
    }
    paint_icon(ui.painter(), rect.shrink(4.0 - 1.0 * state), "brain", accent);
    if hover > 0.0 {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, enabled, on, "Compose mode"));
    response.on_hover_text(if on {
        "Compose mode is ON: JEV investigates read-only, asks questions and proposes a plan. Nothing changes until you accept it. Click to turn off."
    } else {
        "Compose mode: JEV investigates read-only, asks questions and proposes a plan before changing anything. Click to turn on."
    })
}

fn context_segment_row(ui: &mut egui::Ui, color: Color32, label: &str, value: String) {
    ui.horizontal(|ui| {
        let (swatch, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(swatch.center(), 3.5, color);
        ui.label(egui::RichText::new(label).size(11.0).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).size(11.0).color(INK));
        });
    });
}

fn toolset_tokens_estimate() -> u64 {
    ((include_str!("toolset.json").chars().count() as u64).saturating_add(3)) / 4
}

fn begin_window_resize(ui: &mut egui::Ui, rect: Rect) {
    let edge = 14.0;
    let corners = [
        (
            "nw",
            Rect::from_min_size(rect.min, Vec2::splat(edge)),
            egui::ResizeDirection::NorthWest,
        ),
        (
            "ne",
            Rect::from_min_size(pos2(rect.right() - edge, rect.top()), Vec2::splat(edge)),
            egui::ResizeDirection::NorthEast,
        ),
        (
            "sw",
            Rect::from_min_size(pos2(rect.left(), rect.bottom() - edge), Vec2::splat(edge)),
            egui::ResizeDirection::SouthWest,
        ),
        (
            "se",
            Rect::from_min_size(
                pos2(rect.right() - edge, rect.bottom() - edge),
                Vec2::splat(edge),
            ),
            egui::ResizeDirection::SouthEast,
        ),
    ];
    let sides = [
        (
            "n",
            Rect::from_min_max(
                pos2(rect.left() + edge, rect.top()),
                pos2(rect.right() - edge, rect.top() + edge),
            ),
            egui::ResizeDirection::North,
        ),
        (
            "s",
            Rect::from_min_max(
                pos2(rect.left() + edge, rect.bottom() - edge),
                pos2(rect.right() - edge, rect.bottom()),
            ),
            egui::ResizeDirection::South,
        ),
        (
            "w",
            Rect::from_min_max(
                pos2(rect.left(), rect.top() + edge),
                pos2(rect.left() + edge, rect.bottom() - edge),
            ),
            egui::ResizeDirection::West,
        ),
        (
            "e",
            Rect::from_min_max(
                pos2(rect.right() - edge, rect.top() + edge),
                pos2(rect.right(), rect.bottom() - edge),
            ),
            egui::ResizeDirection::East,
        ),
    ];
    for (name, handle, direction) in corners.into_iter().chain(sides) {
        let response = ui.interact(
            handle,
            ui.make_persistent_id(("window-resize", name)),
            Sense::drag(),
        );
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Splitter, true, "Resize window"));
        if response.hovered() || response.dragged() {
            let cursor = match name {
                "nw" | "se" => egui::CursorIcon::ResizeNwSe,
                "ne" | "sw" => egui::CursorIcon::ResizeNeSw,
                "n" | "s" => egui::CursorIcon::ResizeVertical,
                "e" | "w" => egui::CursorIcon::ResizeHorizontal,
                _ => egui::CursorIcon::Default,
            };
            ui.ctx().set_cursor_icon(cursor);
        }
        if response.drag_started() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

/// Windows repaints the frame and the classic caption buttons whenever the window is
/// activated or deactivated, and keeps a 1 px edge on some builds. JARVUS paints its
/// own shell, so the native frame is never allowed to draw: activation repaints and
/// non-client painting are swallowed, and (when not maximized) the client area is the
/// whole window.
#[cfg(target_os = "windows")]
mod native_frame {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

    const WM_NCCALCSIZE: u32 = 0x0083;
    const WM_NCPAINT: u32 = 0x0085;
    const WM_NCACTIVATE: u32 = 0x0086;
    const SUBCLASS_ID: usize = 0x4a41_5256; // "JARV"

    static SUBCLASSED_HWND: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn frame_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _subclass_id: usize,
        _reference: usize,
    ) -> LRESULT {
        match message {
            // lParam = -1 tells the default handler not to repaint the frame.
            WM_NCACTIVATE => return unsafe { DefSubclassProc(hwnd, message, wparam, -1) },
            WM_NCPAINT => return 0,
            // The whole native window is client area in both restored and maximized states.
            // JARVUS paints the titlebar and shell; this prevents Windows from flashing its
            // caption/buttons or a one-pixel frame during activation transitions.
            // Both forms must be handled. With wParam == FALSE, lParam points to a
            // RECT; returning zero keeps its full area in the client region too.
            WM_NCCALCSIZE => return 0,
            _ => {}
        }
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    /// Install once per window, through the common-controls subclass chain that winit itself
    /// uses, so neither side can break the other's message routing.
    pub fn install(hwnd: HWND) {
        if SUBCLASSED_HWND.load(Ordering::SeqCst) == hwnd as isize {
            return;
        }
        if unsafe { SetWindowSubclass(hwnd, Some(frame_proc), SUBCLASS_ID, 0) } != 0 {
            SUBCLASSED_HWND.store(hwnd as isize, Ordering::SeqCst);
        }
    }
}

#[cfg(target_os = "windows")]
fn apply_native_corner_preference(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_STYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SetWindowLongPtrW, SetWindowPos, WS_BORDER, WS_DLGFRAME,
    };

    let Ok(window_handle) = frame.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(handle) = window_handle.as_raw() else {
        return false;
    };
    let hwnd = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    native_frame::install(hwnd);
    // DWMWA_WINDOW_CORNER_PREFERENCE: 1 = do not round, 2 = round.
    let preference: u32 = 1; // DWMWCP_DONOTROUND: the shell paints its own rounded corners.
    // DWMWA_BORDER_COLOR = 34; DWMWA_COLOR_NONE removes the native hairline.
    let border_color: u32 = 0xffff_fffe;
    // A few Windows builds still draw a one-pixel top edge for transparent,
    // undecorated windows even after the DWM color is cleared. Remove only the
    // visible border styles; preserve WS_THICKFRAME for the resize hit targets.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    let borderless_style = style & !((WS_BORDER | WS_DLGFRAME) as isize);
    let style_applied = if borderless_style != style {
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_STYLE, borderless_style);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            ) != 0
        }
    } else {
        true
    };
    unsafe {
        let corner_result = DwmSetWindowAttribute(
            hwnd,
            33,
            &preference as *const u32 as *const core::ffi::c_void,
            core::mem::size_of::<u32>() as u32,
        );
        let border_result = DwmSetWindowAttribute(
            hwnd,
            34,
            &border_color as *const u32 as *const core::ffi::c_void,
            core::mem::size_of::<u32>() as u32,
        );
        // DWMWA_NCRENDERING_POLICY (2) = DWMNCRP_DISABLED (1): Windows 10 still
        // paints a 1 px light line along the top of borderless WS_THICKFRAME
        // windows. JARVUS paints its own shell and shadow, so turn the
        // non-client rendering off entirely (resize hit-testing is unaffected).
        let nc_policy: u32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            2,
            &nc_policy as *const u32 as *const core::ffi::c_void,
            core::mem::size_of::<u32>() as u32,
        );
        corner_result == 0 && border_result == 0 && style_applied
    }
}

#[cfg(not(target_os = "windows"))]
fn apply_native_corner_preference(_frame: &eframe::Frame) -> bool {
    true
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn tool_activity(name: &str) -> &str {
    match name {
        "compact_context" => "Compacting conversation…",
        "ask_user" => "Waiting for your answer…",
        "propose_plan" => "Waiting for your decision on the plan…",
        "agent_spawn" => "Starting an agent…",
        "agent_send" => "Messaging an agent…",
        "agent_wait" => "Waiting for agents…",
        "agent_list" | "agent_read" => "Checking the team…",
        "agent_stop" => "Stopping an agent…",
        "list_mcps" => "Checking connections…",
        "get_tools_" => "Loading tools…",
        "desktop_list_apps" => "Finding open apps…",
        "desktop_list_windows" => "Finding app windows…",
        "desktop_launch" => "Opening the requested app…",
        "desktop_focus_window" => "Focusing the selected window…",
        "desktop_snapshot" => "Reading a fresh screen state…",
        "desktop_find" => "Finding a control…",
        "desktop_get" => "Reading a control…",
        "desktop_click" => "Clicking the selected control…",
        "desktop_type" => "Typing into the selected control…",
        "desktop_press" => "Sending a key…",
        "desktop_screenshot" => "Saving a local screenshot…",
        "inspect_desktop" => "Inspecting desktop…",
        "click_ui_control" => "Using desktop control…",
        "list_processes" => "Checking running apps…",
        "open_app" => "Opening app…",
        "press_key" => "Using keyboard…",
        "type_text" => "Typing in app…",
        _ => "Working on this PC…",
    }
}

/// Human title for a tool row: what it actually did ("Reading main.rs",
/// "Opening Discord", "Running cargo build"), built from the call's arguments.
fn tool_title(activity: &chat::ToolActivity) -> String {
    format!("{} · {}", tool_label(activity), tool_state_label(&activity.state))
}

fn tool_label(activity: &chat::ToolActivity) -> String {
    let args: Value = serde_json::from_str(&activity.arguments_detail).unwrap_or(Value::Null);
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let short = |value: &str, max: usize| {
        let flat = value.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() > max { format!("{}…", flat.chars().take(max).collect::<String>()) } else { flat }
    };
    let file = |key: &str| text(key).map(|path| path.rsplit(['\\', '/']).find(|part| !part.is_empty()).unwrap_or(path).to_owned());
    let host = |key: &str| text(key).map(|url| url.split("://").last().unwrap_or(url).split('/').next().unwrap_or(url).to_owned());
    let name = activity.name.rsplit("__").next().unwrap_or(&activity.name);
    let described = match activity.name.as_str() {
        "jev_run_plan" => {
            let steps = args.get("steps").and_then(Value::as_array).map(|steps| {
                steps.iter().take(3).filter_map(|step| {
                    let action = step.get("do").and_then(Value::as_str)?;
                    let detail = ["recipient", "app", "target", "text"].iter().find_map(|key| step.get(*key).and_then(Value::as_str).filter(|v| !v.is_empty()));
                    Some(match (action, detail) {
                        ("type" | "send" | "web_type" | "web_send", _) => action.replace('_', " "),
                        (_, Some(detail)) => format!("{} {}", action.replace('_', " "), short(detail, 24)),
                        _ => action.replace('_', " "),
                    })
                }).collect::<Vec<_>>().join(" → ")
            }).unwrap_or_default();
            let count = args.get("steps").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            if steps.is_empty() { None } else { Some(format!("JEV · {steps}{}", if count > 3 { format!(" +{} more", count - 3) } else { String::new() })) }
        }
        "open_app" | "desktop_launch" => text("app").map(|app| format!("Opening {app}")),
        "message_received" => Some(format!("Message from {}", text("from").unwrap_or("an agent"))),
        "mcp__jarvus_shell__run_command" => Some("Running command".to_owned()),
        "mcp__jarvus_shell__read_file" => file("path").map(|f| format!("Reading {f}")),
        "mcp__jarvus_shell__write_file" => file("path").map(|f| format!("Writing {f}")),
        "mcp__jarvus_shell__edit_file" => file("path").map(|f| format!("Editing {f}")),
        "mcp__jarvus_shell__list_directory" => file("path").map(|f| format!("Listing {f}")),
        "mcp__jarvus_shell__create_directory" => file("path").map(|f| format!("Creating folder {f}")),
        "mcp__jarvus_shell__web_fetch" => host("url").map(|h| format!("Fetching {h}")),
        "browser__open" => host("url").map(|h| format!("Opening {h} in Chrome")),
        "browser__navigate" => host("url").map(|h| format!("Going to {h}")),
        "browser__click" => text("target").map(|t| format!("Clicking “{}” in Chrome", short(t, 30))),
        "browser__type" => text("target").map(|t| format!("Typing into “{}” in Chrome", short(t, 30))),
        "browser__select" => text("option").map(|t| format!("Choosing “{}” in Chrome", short(t, 30))),
        "browser__extract" => Some(format!("Reading {} from the page", text("kind").unwrap_or("text"))),
        "browser__snapshot" => Some("Reading the Chrome page".to_owned()),
        "browser__tabs" => Some("Listing Chrome tabs".to_owned()),
        "browser__scroll" => Some("Scrolling the page".to_owned()),
        "browser__press" => text("keys").map(|k| format!("Pressing {k} in Chrome")),
        "browser__wait" => Some("Waiting for the page".to_owned()),
        "ask_user" => Some("Asking you a question".to_owned()),
        "propose_plan" => Some("Writing the plan".to_owned()),
        "agent_spawn" => Some(format!("Starting agent {}", text("name").unwrap_or("worker"))),
        "agent_send" => Some(format!("Messaging {}", text("to").unwrap_or("an agent"))),
        "agent_list" => Some("Checking the team".to_owned()),
        "agent_read" => Some(format!("Reading {}", text("id").unwrap_or("an agent"))),
        "agent_wait" => Some("Waiting for agents".to_owned()),
        "agent_stop" => Some(format!("Stopping {}", text("id").unwrap_or("an agent"))),
        "mcp_add_servers" => Some("Adding MCP servers".to_owned()),
        "list_mcps" => Some("Checking connections".to_owned()),
        "get_tools_" => Some(format!("Loading {} tools", text("server").unwrap_or("MCP"))),
        _ => None,
    };
    let base = described.unwrap_or_else(|| {
        let fallback = tool_activity(&activity.name);
        if fallback == "Working on this PC…" { name.replace('_', " ") } else { fallback.trim_end_matches('…').to_owned() }
    });
    base
}

fn tool_state_label(state: &str) -> &str {
    match state {
        "running" => "Running",
        "complete" => "Complete",
        "fresh_snapshot" => "Fresh screen state",
        "refresh_required" => "Refresh needed",
        "refreshed" => "Screen refreshed",
        "failed" => "Failed",
        _ => "Updated",
    }
}

fn brand_mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let id = egui::Id::unique("jarvus-brand-icon");
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(id));
    let texture = texture.unwrap_or_else(|| {
        let image = image::load_from_memory(include_bytes!("../assets/jarvus-mark.png"))
            .expect("embedded JARVUS icon PNG")
            .into_rgba8();
        let size_px = [image.width() as usize, image.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size_px, image.as_raw());
        let texture = ui.ctx().load_texture(
            "jarvus-brand-icon",
            color_image,
            egui::TextureOptions::LINEAR,
        );
        ui.ctx()
            .data_mut(|data| data.insert_temp(id, texture.clone()));
        texture
    });
    ui.put(
        rect,
        egui::Image::from_texture(&texture).fit_to_exact_size(rect.size()),
    );
}

/// Small rounded chip in the title bar: icon on the left, short text on the right.
fn lerp_color(from: Color32, to: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

fn status_chip(
    ui: &mut egui::Ui,
    id: &str,
    icon: impl FnOnce(&egui::Painter, Rect, Color32),
    text: &str,
    hint: &str,
) -> egui::Response {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), egui::FontId::proportional(11.0), INK);
    let size = Vec2::new(galley.size().x + 34.0, 28.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let hovered = response.hovered();
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(14),
        if hovered { HOVER_FILL } else { RAISED },
    );
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(14),
        egui::Stroke::new(1.0, LINE),
        egui::StrokeKind::Inside,
    );
    let icon_rect =
        Rect::from_center_size(pos2(rect.left() + 15.0, rect.center().y), Vec2::splat(16.0));
    icon(
        ui.painter(),
        icon_rect,
        if hovered { HOVER_GREEN } else { INK },
    );
    ui.painter().galley(
        pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
        galley,
        INK,
    );
    let _ = id;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, hint));
    response.on_hover_text(hint)
}

fn icon_button(ui: &mut egui::Ui, icon: &str, hint: &str, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let hovered = response.hovered() || response.has_focus();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), HOVER_FILL);
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_icon(
        ui.painter(),
        rect,
        icon,
        if hovered { HOVER_GREEN } else { INK },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, hint));
    response.on_hover_text(hint)
}

/// Draws a named icon from the bundled Phosphor icon font.
fn paint_icon(painter: &egui::Painter, rect: Rect, icon: &str, color: Color32) {
    use egui_phosphor::regular as ph;
    let glyph = match icon {
        "plus" => ph::PLUS,
        "edit" => ph::PENCIL_SIMPLE,
        "copy" => ph::COPY,
        "use" | "send" => ph::ARROW_RIGHT,
        "refresh" => ph::ARROW_CLOCKWISE,
        "context" => ph::GAUGE,
        "more" => ph::DOTS_THREE_VERTICAL,
        "bolt" => ph::LIGHTNING,
        "settings" => ph::GEAR,
        "trash" => ph::TRASH,
        "chat" => ph::CHAT_CIRCLE,
        "chart" => ph::CHART_BAR,
        "link" => ph::LINK,
        "plug" => ph::PLUGS_CONNECTED,
        "globe" => ph::GLOBE,
        "storage" => ph::DATABASE,
        "download" => ph::DOWNLOAD_SIMPLE,
        "folder" => ph::FOLDER,
        "check" => ph::CHECK,
        "chevron" => ph::CARET_DOWN,
        "chevron_right" => ph::CARET_RIGHT,
        "brain" => ph::BRAIN,
        "upload" => ph::UPLOAD_SIMPLE,
        "cancel" => ph::X,
        "question" => ph::QUESTION,
        "compose" => ph::NOTE_PENCIL,
        "back" => ph::ARROW_LEFT,
        "agents" => ph::USERS_THREE,
        "stop" => ph::STOP,
        _ => ph::CIRCLE,
    };
    let size = rect.width().min(rect.height()) * 0.62;
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(size.max(12.0)),
        color,
    );
}

/// Depth under a panel: a wide, faint ambient shadow plus a tighter contact
/// shadow, both blurred so no edge is visible.
/// Run `paint` clipped to the shell but without its four rounded-corner squares, so soft
/// shadows and glows never tint the cut-outs outside the rounded corners (which would show
/// as a second, square window border over the desktop).
fn paint_inside_rounded_shell(ui: &egui::Ui, shell_rect: Rect, radius: f32, paint: impl Fn(&egui::Painter)) {
    if radius <= 0.0 {
        paint(&ui.painter().with_clip_rect(shell_rect));
        return;
    }
    let r = radius;
    let strips = [
        Rect::from_min_max(pos2(shell_rect.left() + r, shell_rect.top()), pos2(shell_rect.right() - r, shell_rect.bottom())),
        Rect::from_min_max(pos2(shell_rect.left(), shell_rect.top() + r), pos2(shell_rect.left() + r, shell_rect.bottom() - r)),
        Rect::from_min_max(pos2(shell_rect.right() - r, shell_rect.top() + r), pos2(shell_rect.right(), shell_rect.bottom() - r)),
    ];
    for strip in strips {
        paint(&ui.painter().with_clip_rect(strip));
    }
}

fn draw_panel_shadow(painter: &egui::Painter, rect: Rect) {
    let ambient = egui::Shadow {
        offset: [0, 18],
        blur: 60,
        spread: 0,
        color: Color32::from_black_alpha(150),
    };
    let contact = egui::Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    painter.add(ambient.as_shape(rect, CornerRadius::same(14)));
    painter.add(contact.as_shape(rect, CornerRadius::same(14)));
}

/// One continuous radial light: alpha falls off smoothly (smootherstep) from
/// the center to the edge of an ellipse, drawn as a fine mesh so there are no
/// visible rings or layers.
fn radial_glow(painter: &egui::Painter, center: Pos2, radius: Vec2, rgb: [u8; 3], peak_alpha: f32) {
    if peak_alpha <= 0.0 || radius.x < 1.0 || radius.y < 1.0 {
        return;
    }
    const RINGS: usize = 48;
    const SEGMENTS: usize = 72;
    let alpha_at = |t: f32| {
        let t = t.clamp(0.0, 1.0);
        let fade = 1.0 - t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
        (peak_alpha * fade * 255.0).round().clamp(0.0, 255.0) as u8
    };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(
        center,
        Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha_at(0.0)),
    );
    for ring in 1..=RINGS {
        let t = ring as f32 / RINGS as f32;
        let color = Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha_at(t));
        for segment in 0..SEGMENTS {
            let angle = segment as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            mesh.colored_vertex(
                pos2(
                    center.x + angle.cos() * radius.x * t,
                    center.y + angle.sin() * radius.y * t,
                ),
                color,
            );
        }
    }
    let ring_start = |ring: usize| (1 + (ring - 1) * SEGMENTS) as u32;
    for segment in 0..SEGMENTS as u32 {
        let next = (segment + 1) % SEGMENTS as u32;
        mesh.add_triangle(0, ring_start(1) + segment, ring_start(1) + next);
    }
    for ring in 1..RINGS {
        let (inner, outer) = (ring_start(ring), ring_start(ring + 1));
        for segment in 0..SEGMENTS as u32 {
            let next = (segment + 1) % SEGMENTS as u32;
            mesh.add_triangle(inner + segment, outer + segment, outer + next);
            mesh.add_triangle(inner + segment, outer + next, inner + next);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// (center, ellipse radius, rgb, peak alpha) of the two soft green corner lights of the main panel.
/// `strength` scales them (stronger on the new-chat screen).
fn corner_glow_specs(rect: Rect, strength: f32) -> [(Pos2, Vec2, [u8; 3], f32); 2] {
    let strength = strength.clamp(0.0, 1.0);
    let radius = Vec2::new((rect.width() * 0.55).clamp(260.0, 620.0), (rect.height() * 0.6).clamp(220.0, 520.0));
    [
        (rect.right_top(), radius, GLOW_GREEN, 0.24 * strength),
        (rect.left_bottom(), radius, GLOW_GREEN, 0.20 * strength),
    ]
}

/// Alpha (0..1) of one radial glow at a point; the same curve `radial_glow` draws.
fn glow_alpha(center: Pos2, radius: Vec2, peak: f32, point: Pos2) -> f32 {
    let offset = Vec2::new((point.x - center.x) / radius.x, (point.y - center.y) / radius.y);
    let t = offset.length().clamp(0.0, 1.0);
    peak * (1.0 - t * t * t * (t * (t * 6.0 - 15.0) + 10.0))
}

fn draw_corner_glows(painter: &egui::Painter, rect: Rect, strength: f32) {
    for (center, radius, rgb, peak) in corner_glow_specs(rect, strength) {
        radial_glow(painter, center, radius, rgb, peak);
    }
}

/// The glows are drawn everywhere except the four corner squares of the rounded shell, so they
/// never tint the cut-outs outside the rounded corners. Each corner square is then filled here
/// with the same glow, masked to the inside of the arc at 1 px resolution, so there is neither a
/// tinted square outside the arc nor a dark notch inside it.
fn corner_glow_patches(ui: &egui::Ui, shell_rect: Rect, radius: f32, glows: &[(Pos2, Vec2, [u8; 3], f32)]) {
    if radius <= 0.0 {
        return;
    }
    let cells = radius.ceil() as usize;
    let step = radius / cells as f32;
    let corners = [
        (shell_rect.left_top(), 1.0_f32, 1.0_f32),
        (shell_rect.right_top(), -1.0, 1.0),
        (shell_rect.left_bottom(), 1.0, -1.0),
        (shell_rect.right_bottom(), -1.0, -1.0),
    ];
    for (corner, sx, sy) in corners {
        let arc_center = corner + Vec2::new(sx * radius, sy * radius);
        for &(center, glow_radius, rgb, peak) in glows {
            let mut mesh = egui::Mesh::default();
            for j in 0..=cells {
                for i in 0..=cells {
                    let point = corner + Vec2::new(sx * i as f32 * step, sy * j as f32 * step);
                    let inside = (radius - (point - arc_center).length() + 0.5).clamp(0.0, 1.0);
                    let alpha = (glow_alpha(center, glow_radius, peak, point) * inside * 255.0).round().clamp(0.0, 255.0) as u8;
                    mesh.colored_vertex(point, Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha));
                }
            }
            let row = (cells + 1) as u32;
            for j in 0..cells as u32 {
                for i in 0..cells as u32 {
                    let a = j * row + i;
                    mesh.add_triangle(a, a + 1, a + row);
                    mesh.add_triangle(a + 1, a + row + 1, a + row);
                }
            }
            ui.painter().add(egui::Shape::mesh(mesh));
        }
    }
}

/// Markdown treats a single newline as a space, so a model reply written one
/// item per line would collapse into one paragraph. Turn single newlines into
/// hard breaks outside fenced code blocks; blank lines still separate paragraphs.
fn preserve_line_breaks(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 32);
    let mut in_fence = false;
    let lines: Vec<&str> = text.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        output.push_str(line);
        if index + 1 < lines.len() {
            let next = lines[index + 1].trim();
            let needs_break = !in_fence
                && !line.trim().is_empty()
                && !next.is_empty()
                && !line.ends_with("  ")
                && !line.trim_start().starts_with('|');
            if needs_break {
                output.push_str("  ");
            }
            output.push('\n');
        }
    }
    output
}

pub(crate) fn browser_tool_specs() -> Vec<(String, String, Value)> {
    use serde_json::json;
    // Every page command accepts an optional tab: a numeric tabId, or `tab` (a
    // fuzzy title/URL match). Without one it uses the focused tab. Elements are
    // targeted by visible label (fuzzy, best visible match), a controlId from
    // browser__snapshot, or a CSS selector.
    let tab = json!({"tabId":{"type":"integer"},"tab":{"type":"string","description":"Tab title or URL words, e.g. 'youtube'"}});
    let target = json!({"target":{"type":"string","description":"Visible label/text of the element, as a person would say it"},
        "within":{"type":"string","description":"Area words, e.g. 'navigation', 'search', 'dialog'"},
        "controlId":{"type":"string"},"selector":{"type":"string","description":"CSS selector (only when a label cannot identify it)"}});
    let merge = |extra: serde_json::Value| {
        let mut properties = tab.as_object().unwrap().clone();
        for source in [target.as_object().unwrap(), extra.as_object().unwrap()] {
            for (k, v) in source {
                properties.insert(k.clone(), v.clone());
            }
        }
        json!({"type":"object","properties":properties,"additionalProperties":false})
    };
    let tab_only = |extra: serde_json::Value| {
        let mut properties = tab.as_object().unwrap().clone();
        for (k, v) in extra.as_object().unwrap() {
            properties.insert(k.clone(), v.clone());
        }
        json!({"type":"object","properties":properties,"additionalProperties":false})
    };
    vec![
        ("browser__tabs".into(), "List every open Chrome tab in every window (tabId, title, url, active).".into(), json!({"type":"object","properties":{},"additionalProperties":false})),
        ("browser__activate".into(), "Switch to a Chrome tab (by tabId or fuzzy tab title/URL) and bring its window forward.".into(), tab_only(json!({}))),
        ("browser__open".into(), "Open a URL in a new tab and wait for it to load. Bare domains work ('youtube.com'); plain words become a Google search.".into(),
            json!({"type":"object","properties":{"url":{"type":"string"},"background":{"type":"boolean"}},"required":["url"],"additionalProperties":false})),
        ("browser__navigate".into(), "Load a URL in an existing tab (default: focused tab) and wait for it.".into(), tab_only(json!({"url":{"type":"string"}}))),
        ("browser__back".into(), "Go back in a tab's history.".into(), tab_only(json!({}))),
        ("browser__snapshot".into(), "Read a tab fast: url, title, visible text excerpt, and interactive controls with ids. Pass target to rank controls matching it first.".into(),
            tab_only(json!({"target":{"type":"string"},"limit":{"type":"integer"},"max_chars":{"type":"integer"}}))),
        ("browser__click".into(), "Click an element (real mouse-event sequence). JARVUS independently asks the user to approve consequential controls such as Send, Delete, or Purchase; wait for that approval and never try to bypass it. Set double or right for those clicks.".into(),
            merge(json!({"double":{"type":"boolean"},"right":{"type":"boolean"}}))),
        ("browser__type".into(), "Type into a text field (works with React and rich editors). clear defaults to true. submit=true presses Enter / submits the form; consequential submissions may pause for the user's JARVUS approval.".into(),
            merge(json!({"text":{"type":"string"},"clear":{"type":"boolean"},"submit":{"type":"boolean"}}))),
        ("browser__select".into(), "Choose an option in a dropdown (native <select> or custom). option is matched fuzzily.".into(),
            merge(json!({"option":{"type":"string"}}))),
        ("browser__press".into(), "Press keys in the page (enter, escape, tab, arrows, pageDown, ctrl+a ...). Optionally focus a target first. Enter may require user approval when it submits a consequential form.".into(),
            merge(json!({"keys":{"type":"string"}}))),
        ("browser__scroll".into(), "Scroll the page (direction up/down, amount in thirds of a screen) or scroll an element into view.".into(),
            merge(json!({"direction":{"type":"string","enum":["up","down"]},"amount":{"type":"integer"}}))),
        ("browser__extract".into(), "Scrape quickly: kind = text | links | tables | headings | images | forms | elements (with match = CSS). Optional selector limits the area.".into(),
            tab_only(json!({"kind":{"type":"string"},"selector":{"type":"string"},"match":{"type":"string"},"limit":{"type":"integer"},"max_chars":{"type":"integer"}}))),
        ("browser__wait".into(), "Wait until text or a selector appears (or disappears with gone=true). timeout_ms up to 30000.".into(),
            tab_only(json!({"text":{"type":"string"},"selector":{"type":"string"},"gone":{"type":"boolean"},"timeout_ms":{"type":"integer"}}))),
    ]
}

/// Live health word for a configured MCP server.
pub(crate) fn mcp_status(enabled: bool, connected: bool, error: Option<&str>) -> &'static str {
    if !enabled { return "disabled"; }
    if connected { return "connected"; }
    match error {
        Some(text) if { let lower = text.to_ascii_lowercase(); lower.contains("401") || lower.contains("403") || lower.contains("unauthor") || lower.contains("forbidden") } => "unauthorized",
        Some(_) => "error",
        None => "disconnected",
    }
}

pub(crate) fn browser_bridge_inventory(connected: bool) -> Value {
    let tools = browser_tool_specs()
        .into_iter()
        .map(|(name, _, _)| name)
        .collect::<Vec<_>>();
    serde_json::json!({
        "id": "chrome",
        "status": if connected { "connected" } else { "disconnected" },
        "name": "JARVUS Chrome browser bridge",
        "kind": "browser_bridge",
        "enabled": true,
        "connected": connected,
        "error": if connected { Value::Null } else { serde_json::json!("Chrome extension is not connected") },
        "tools": tools
    })
}

pub(crate) fn shell_connection_inventory() -> Value {
    let tools = crate::shell::tool_specs()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>();
    serde_json::json!({
        "id": "shell",
        "status": "connected",
        "name": "JARVUS Shell",
        "kind": "local_connection",
        "enabled": true,
        "connected": true,
        "error": Value::Null,
        "tools": tools
    })
}

fn tool_name_component(value: &str, max_len: usize) -> String {
    let mut output = String::new();
    let mut previous_separator = false;
    for character in value.chars() {
        if output.len() >= max_len {
            break;
        }
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator && !output.is_empty() {
            output.push('_');
            previous_separator = true;
        }
    }
    output.trim_matches('_').to_owned()
}

pub(crate) fn mcp_tool_function_name(server_id: &str, tool_name: &str) -> String {
    let server = tool_name_component(server_id, 14);
    let action = tool_name_component(tool_name, 25);
    if server.is_empty() || action.is_empty() {
        return String::new();
    }
    let mut hash = 0x811c9dc5_u32;
    for byte in server_id.bytes().chain([0]).chain(tool_name.bytes()) {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("mcp__{server}__{action}_{hash:08x}")
}

fn mcp_server_subtitle(config: &McpServerConfig) -> String {
    if let Some(endpoint) = config.url.as_deref() {
        let endpoint = reqwest::Url::parse(endpoint)
            .map(|mut url| {
                // Query strings and fragments can contain deployment-specific
                // credentials; they are never needed to identify the endpoint in
                // the settings list.
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            })
            .unwrap_or_else(|_| "Invalid endpoint".to_owned());
        let transport = if config.sse {
            "Legacy HTTP+SSE"
        } else {
            "Streamable HTTP"
        };
        format!("{transport} · {endpoint}")
    } else {
        format!("stdio · {} {}", config.command, config.args.join(" "))
            .trim()
            .to_owned()
    }
}

fn desktop_status_summary(value: &Value) -> String {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let detail = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/error/code").and_then(Value::as_str))
            .unwrap_or("Agent Desktop reported an error");
        return format!(
            "Agent Desktop · {}",
            detail.chars().take(110).collect::<String>()
        );
    }
    if let Some(state) = value
        .pointer("/data/permissions/accessibility/state")
        .and_then(Value::as_str)
    {
        return format!("Bridge connected · UIA access {state}");
    }
    let command = value.get("command").and_then(Value::as_str).unwrap_or("");
    if command.starts_with("permissions") {
        "Agent Desktop permissions checked · details in Computer".into()
    } else {
        "Bridge connected · Agent Desktop ready".into()
    }
}

fn conversation_title(prompt: &str) -> String {
    let compact = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = compact.chars().take(34).collect();
    if compact.chars().count() > 34 {
        title.push('…');
    }
    if title.is_empty() {
        "New chat".into()
    } else {
        title
    }
}

fn send_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(label)
            .size(12.0)
            .strong()
            .color(Color32::WHITE),
    )
    .fill(Color32::from_rgb(18, 20, 19))
    .corner_radius(CornerRadius::same(20))
    .min_size(Vec2::new(40.0, 40.0))
}

#[cfg(test)]
mod ui_state_tests {
    use super::{
        SettingsSection, browser_bridge_inventory, builtin_command, chat_send_is_allowed, conversation_created_ns,
        desktop_status_summary, is_local_desktop_tool, local_action_duration_label,
        mcp_server_subtitle, preserve_line_breaks, shell_connection_inventory,
    };

    #[test]
    fn browser_bridge_inventory_is_separate_and_reports_live_health_and_tools() {
        let connected = browser_bridge_inventory(true);
        assert_eq!(connected["kind"], "browser_bridge");
        assert_eq!(connected["connected"], true);
        assert_eq!(connected["tools"][0], "browser__tabs");
        assert!(
            connected["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool == "browser__click")
        );

        let disconnected = browser_bridge_inventory(false);
        assert_eq!(disconnected["connected"], false);
        assert!(
            disconnected["error"]
                .as_str()
                .unwrap()
                .contains("not connected")
        );
    }

    #[test]
    fn shell_is_reported_as_a_live_local_connection_with_its_tools() {
        let shell = shell_connection_inventory();
        assert_eq!(shell["kind"], "local_connection");
        assert_eq!(shell["connected"], true);
        assert_eq!(shell["tools"][0], "mcp__jarvus_shell__run_command");
        assert!(shell["tools"].as_array().unwrap().len() >= 7);
    }

    #[test]
    fn local_action_duration_is_never_fabricated() {
        assert_eq!(local_action_duration_label(None), "-- ms");
        assert_eq!(local_action_duration_label(Some(42)), "42 ms");
    }

    #[test]
    fn desktop_status_handles_legacy_and_agent_desktop_permission_envelopes() {
        assert_eq!(
            desktop_status_summary(
                &serde_json::json!({"data":{"permissions":{"accessibility":{"state":"granted"}}}})
            ),
            "Bridge connected · UIA access granted"
        );
        assert_eq!(
            desktop_status_summary(
                &serde_json::json!({"version":1,"ok":true,"command":"permissions","data":{"accessibility":"granted"}})
            ),
            "Agent Desktop permissions checked · details in Computer"
        );
        assert!(
            desktop_status_summary(
                &serde_json::json!({"ok":false,"error":{"message":"permission denied"}})
            )
            .contains("permission denied")
        );
    }

    #[test]
    fn chat_send_is_blocked_until_any_existing_background_operation_finishes() {
        assert!(!chat_send_is_allowed(true, false, true, true, true));
        assert!(!chat_send_is_allowed(false, true, true, true, true));
        assert!(!chat_send_is_allowed(false, false, true, true, false));
        assert!(chat_send_is_allowed(false, false, true, true, true));
    }

    #[test]
    fn only_local_desktop_tools_feed_the_jev_duration() {
        assert!(is_local_desktop_tool("open_app"));
        assert!(is_local_desktop_tool("desktop_click"));
        assert!(is_local_desktop_tool("list_processes"));
        assert!(is_local_desktop_tool("inspect_desktop"));
        assert!(is_local_desktop_tool("click_ui_control"));
        assert!(is_local_desktop_tool("type_text"));
        assert!(is_local_desktop_tool("press_key"));
        assert!(!is_local_desktop_tool("mcp_search"));
        assert!(!is_local_desktop_tool("web_search"));
    }

    #[test]
    fn builtin_slash_commands_match_whole_commands_only() {
        assert_eq!(builtin_command("/compact"), Some("compact"));
        assert_eq!(builtin_command("  /Compose "), Some("compose"));
        assert_eq!(builtin_command("/new"), Some("new"));
        assert_eq!(builtin_command("/compact now"), None);
        assert_eq!(builtin_command("compact"), None);
    }

    #[test]
    fn settings_nav_has_distinct_destination_names() {
        let names = SettingsSection::ALL.map(SettingsSection::title);
        let distinct = names.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(distinct.len(), names.len());
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn mcp_server_subtitles_show_remote_transport_without_query_credentials() {
        let mut config = crate::mcp::McpServerConfig {
            id: "remote".into(),
            command: String::new(),
            args: vec![],
            url: Some("https://mcp.example/tools?token=private#section".into()),
            sse: false,
            env: Default::default(),
            headers: Default::default(),
            timeout_secs: 30,
            enabled: true,
        };
        assert_eq!(
            mcp_server_subtitle(&config),
            "Streamable HTTP · https://mcp.example/tools"
        );
        config.sse = true;
        assert!(mcp_server_subtitle(&config).starts_with("Legacy HTTP+SSE · "));
        assert!(!mcp_server_subtitle(&config).contains("private"));
    }

    #[test]
    fn single_newlines_become_hard_breaks_outside_code() {
        assert_eq!(preserve_line_breaks("Apple\nBanana"), "Apple  \nBanana");
        assert_eq!(
            preserve_line_breaks("one\n\ntwo"),
            "one\n\ntwo",
            "paragraph breaks are untouched"
        );
        assert_eq!(
            preserve_line_breaks("```\na\nb\n```"),
            "```\na\nb\n```",
            "code blocks are untouched"
        );
    }

    #[test]
    fn chat_age_comes_from_the_id_timestamp() {
        assert_eq!(conversation_created_ns("chat-123-ff-1"), Some(255));
        assert_eq!(conversation_created_ns("legacy"), None);
    }
}
