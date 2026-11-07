//! The one place a model tool call is executed: used by the main chat and by every
//! subagent. It wraps each call with the user's hooks (PreToolUse / PostToolUse) and
//! routes it to Rhea, Shell, Chrome, an MCP server, or the team tools (`ask_user`,
//! `propose_plan`, `agent_*`).

use std::{
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};

use crate::{
    app::{browser_bridge_inventory, browser_tool_specs, mcp_status, mcp_tool_function_name, shell_connection_inventory},
    auth, browser, chat,
    hooks::{HookEvent, HookRegistry},
    jev,
    mcp::{self, McpManager},
    team::{self, AskOption, AskQuestion, Runner, Update},
};

const SUBAGENT_PROMPT: &str = include_str!("subagent_prompt.md");

/// Everything a tool call needs to know about who is calling and what is connected.
#[derive(Clone)]
pub struct ToolContext {
    pub mcp_manager: Arc<Mutex<McpManager>>,
    pub chrome_alive: Arc<AtomicBool>,
    pub hooks: Arc<RwLock<HookRegistry>>,
    pub chat_id: String,
    /// "main" for the manager chat, otherwise the subagent's id.
    pub agent_id: String,
    pub agent_name: String,
    pub model: String,
    pub effort: String,
    pub context_window: u64,
    /// Fast mode (priority processing) for the runs this context starts.
    pub fast: bool,
    /// Which backend `model` is asked through. A spawned subagent inherits the main chat's
    /// provider rather than picking its own, even when it uses a different model slug.
    pub provider: crate::chat::Provider,
    /// Compose mode was on when this run started; spawned agents inherit read-only limits.
    pub compose: bool,
    /// Model for new subagents; empty = the same model as the caller.
    pub subagent_model: String,
    /// Auto-compact threshold (percent of the context window).
    pub compact_percent: u8,
}

impl ToolContext {
    pub fn is_main(&self) -> bool {
        self.agent_id == team::MAIN
    }

    fn hooks(&self) -> HookRegistry {
        // A skill installed or removed since the last call may have changed hooks.json.
        crate::hooks::refresh_shared(&self.hooks);
        self.hooks.read().map(|hooks| hooks.clone()).unwrap_or_default()
    }
}

/// Tools sent with every request in addition to the built-in toolset: the Shell package.
/// Tokens the built-in tool list adds to every request (about four characters per token).
pub fn toolset_tokens() -> u64 {
    (include_str!("toolset.json").chars().count() as u64).saturating_add(3) / 4
}

pub fn core_extra_tools() -> Vec<Value> {
    crate::shell::tool_specs().as_array().cloned().unwrap_or_default()
}

/// Run one tool call with hooks around it.
pub fn dispatch(ctx: &ToolContext, name: &str, arguments: &Value) -> Result<Value, String> {
    let hooks = ctx.hooks();
    let mut arguments = arguments.clone();
    let mut context_notes: Vec<String> = Vec::new();
    let base = json!({"session_id": ctx.chat_id, "agent": ctx.agent_name});
    if hooks.has_hooks_for(HookEvent::PreToolUse) {
        let mut payload = base.clone();
        payload["tool_input"] = arguments.clone();
        let outcome = hooks.run(HookEvent::PreToolUse, Some(name), &payload);
        if let Some(reason) = outcome.block {
            return Err(format!("Blocked by a PreToolUse hook: {reason}"));
        }
        if let Some(updated) = outcome.updated_input {
            arguments = updated;
        }
        context_notes.extend(outcome.additional_context);
    }
    let result = dispatch_inner(ctx, name, &arguments);
    // What actually happened (not what the model said) becomes memory, off the action path.
    crate::memory::observe_tool(&ctx.chat_id, &ctx.agent_name, name, &arguments, &result);
    if hooks.has_hooks_for(HookEvent::PostToolUse) {
        let mut payload = base;
        payload["tool_input"] = arguments.clone();
        payload["tool_response"] = match &result {
            Ok(value) => value.clone(),
            Err(error) => json!({"error": error}),
        };
        let outcome = hooks.run(HookEvent::PostToolUse, Some(name), &payload);
        context_notes.extend(outcome.additional_context);
        if let Some(reason) = outcome.block {
            context_notes.push(format!("PostToolUse hook feedback: {reason}"));
        }
    }
    match result {
        Ok(mut value) if !context_notes.is_empty() => {
            match value.as_object_mut() {
                Some(object) => {
                    object.insert("hook_context".into(), json!(context_notes));
                    Ok(value)
                }
                None => Ok(json!({"result": value, "hook_context": context_notes})),
            }
        }
        other => other,
    }
}

fn dispatch_inner(ctx: &ToolContext, name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "mcp_add_servers" => mcp_add_servers(ctx, arguments),
        "list_mcps" | "mcp_list_servers" => list_mcps(ctx),
        "get_tools_" => get_tools(ctx, arguments),
        "ask_user" => ask_user(ctx, arguments),
        "propose_plan" => propose_plan(ctx, arguments),
        "agent_spawn" => agent_spawn(ctx, arguments),
        "agent_send" => agent_send(ctx, arguments),
        "agent_list" => agent_list(ctx),
        "agent_read" => agent_read(ctx, arguments),
        "agent_wait" => agent_wait(ctx, arguments),
        "agent_stop" => agent_stop(ctx, arguments),
        "laya_decide" => crate::laya::decide_tool(arguments),
        "memory_search" | "memory_remember" | "memory_forget" => crate::memory::tool(&ctx.chat_id, name, arguments),
        "use_skill" => crate::skills::load(arguments.get("name").and_then(Value::as_str).ok_or("`name` is required")?),
        "install_skill" => crate::skills::install(
            arguments.get("source").and_then(Value::as_str).ok_or("`source` is required (a GitHub link)")?,
            arguments.get("name").and_then(Value::as_str),
        ),
        _ if name.starts_with("browser__") => {
            browser::execute_browser_command(&name["browser__".len()..], arguments.clone()).map_err(|error| error.to_string())
        }
        _ if name.starts_with("mcp__rhea_shell__") => crate::shell::execute(name, arguments),
        _ if name.starts_with("mcp__") => call_mcp_tool(ctx, name, arguments),
        _ => jev::execute(name, arguments),
    }
}

fn lock_manager(ctx: &ToolContext) -> Result<std::sync::MutexGuard<'_, McpManager>, String> {
    ctx.mcp_manager.lock().map_err(|_| "MCP manager lock is unavailable".to_owned())
}

fn mcp_add_servers(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    let text = arguments.get("config_json").and_then(Value::as_str).ok_or("config_json is required")?;
    let configs = mcp::parse_server_document(text)?;
    let report = lock_manager(ctx)?.merge_configs(configs)?;
    Ok(json!({"summary": report.summary(), "added": report.added, "updated": report.updated,
        "unchanged": report.unchanged, "connected": report.connected,
        "failed": report.failed.iter().map(|(id, error)| json!({"id": id, "error": error})).collect::<Vec<_>>()}))
}

fn list_mcps(ctx: &ToolContext) -> Result<Value, String> {
    let mut manager = lock_manager(ctx)?;
    manager.refresh_health();
    let servers = manager
        .configs()
        .iter()
        .map(|config| {
            json!({
                "id": config.id, "enabled": config.enabled, "connected": manager.is_connected(&config.id),
                "status": mcp_status(config.enabled, manager.is_connected(&config.id), manager.last_error(&config.id)),
                "tool_count": manager.tools_for(&config.id).len(),
                "error": manager.last_error(&config.id), "command": config.command,
                "tools": manager.tools_for(&config.id).iter().map(|tool| tool.name.clone()).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    drop(manager);
    // Ping the bridge now so the answer is current, not the five-second UI cache.
    let chrome_connected = browser::extension_alive();
    ctx.chrome_alive.store(chrome_connected, Ordering::Relaxed);
    let browser_bridge = browser_bridge_inventory(chrome_connected);
    let configured = servers
        .iter()
        .map(|server| {
            let id = server["id"].as_str().unwrap_or("unknown");
            format!("{id}: {}", server["status"].as_str().unwrap_or("unknown"))
        })
        .collect::<Vec<_>>();
    let browser_tools = browser_bridge["tools"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
    let answer_hint = format!(
        "Live connection inventory (refreshed for this request). Configured MCP servers: {}. Chrome browser bridge (not a configured MCP server): {}. Its tools (load with get_tools_ chrome): {}. Rhea Shell (built-in local connection): connected.",
        if configured.is_empty() { "none".to_owned() } else { configured.join(", ") },
        if chrome_connected { "connected" } else { "disconnected" },
        browser_tools.join(", ")
    );
    Ok(json!({"answer_hint": answer_hint, "mcp_servers": servers, "browser_bridges": [browser_bridge],
        "local_connections": [shell_connection_inventory()]}))
}

fn get_tools(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    let wanted = arguments.get("server").and_then(Value::as_str).unwrap_or("");
    let norm = |value: &str| value.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    let key = norm(wanted);
    if key.is_empty() {
        return Err("get_tools_ needs a server id from list_mcps.".into());
    }
    if key.contains("chrome") || key.contains("browser") || key == "extension" {
        let mut schemas = Vec::new();
        let mut names = Vec::new();
        for (tool_name, description, schema) in browser_tool_specs() {
            if let Ok(function) = chat::function_tool(&tool_name, &description, &schema) {
                schemas.push(function);
                names.push(tool_name);
            }
        }
        return Ok(json!({"server": "chrome", "tools": names, "load_tools": schemas,
            "note": "Chrome tools are now callable. Page elements are matched by visible label."}));
    }
    if key.contains("shell") {
        return Ok(json!({"server": "shell", "note": "Shell tools are always loaded: mcp__rhea_shell__*"}));
    }
    let manager = lock_manager(ctx)?;
    let config = manager
        .configs()
        .iter()
        .find(|config| norm(&config.id) == key)
        .or_else(|| manager.configs().iter().find(|config| norm(&config.id).contains(&key) || key.contains(&norm(&config.id))))
        .ok_or_else(|| format!("No connection named '{wanted}'. Call list_mcps to see the ids."))?;
    let id = config.id.clone();
    let tools = manager.tools_for(&id);
    if tools.is_empty() {
        return Err(format!(
            "'{id}' has no tools right now: status {}. {}",
            mcp_status(config.enabled, manager.is_connected(&id), manager.last_error(&id)),
            manager.last_error(&id).unwrap_or("It may be disabled or disconnected; check list_mcps.")
        ));
    }
    let mut schemas = Vec::new();
    let mut names = Vec::new();
    for tool in tools {
        let function_name = mcp_tool_function_name(&tool.server_id, &tool.name);
        if function_name.is_empty() {
            continue;
        }
        let description = tool.description.as_deref().unwrap_or("MCP server tool");
        if let Ok(schema) = chat::function_tool(&function_name, description, &tool.input_schema) {
            schemas.push(schema);
            names.push(function_name);
        }
    }
    Ok(json!({"server": id, "tools": names, "load_tools": schemas, "note": "These tools are now callable."}))
}

fn call_mcp_tool(ctx: &ToolContext, name: &str, arguments: &Value) -> Result<Value, String> {
    let mut manager = lock_manager(ctx)?;
    let binding = manager
        .tools()
        .into_iter()
        .find(|tool| mcp_tool_function_name(&tool.server_id, &tool.name) == name)
        .map(|tool| (tool.server_id, tool.name))
        .ok_or_else(|| format!("Unknown tool '{name}'. Call get_tools_ for its server first."))?;
    manager
        .call_tool(&binding.0, &binding.1, arguments.clone())
        .map(|result| json!({"content": result.content, "is_error": result.is_error}))
}

// ---- ask_user / propose_plan ----------------------------------------------------------

fn ask_user(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    if !ctx.is_main() {
        return Err("Workers cannot ask the user. Send the question to \"main\" with agent_send.".into());
    }
    let list = arguments.get("questions").and_then(Value::as_array).ok_or("questions must be an array")?;
    let mut questions = Vec::new();
    for (index, item) in list.iter().take(6).enumerate() {
        let question = item.get("question").and_then(Value::as_str).map(str::trim).filter(|q| !q.is_empty()).ok_or("each question needs text")?;
        let id = item.get("id").and_then(Value::as_str).map(str::trim).filter(|i| !i.is_empty()).map(str::to_owned).unwrap_or_else(|| format!("q{}", index + 1));
        let options = item
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| {
                        let label = option.get("label").and_then(Value::as_str)?.trim().to_owned();
                        if label.is_empty() {
                            return None;
                        }
                        Some(AskOption { label, description: option.get("description").and_then(Value::as_str).unwrap_or("").trim().to_owned() })
                    })
                    .take(6)
                    .collect()
            })
            .unwrap_or_default();
        let allow_custom = item.get("allow_custom").and_then(Value::as_bool).unwrap_or(true);
        questions.push(AskQuestion { id, question: question.to_owned(), options, allow_custom });
    }
    match team::hub().ask(questions, &jev::cancel_requested)? {
        Some(answers) => Ok(json!({"answers": answers.into_iter().map(|(id, answer)| json!({"id": id, "answer": answer})).collect::<Vec<_>>()})),
        None => Ok(json!({"skipped": true, "note": "The user skipped the questions. Proceed with your best default and state the assumption."})),
    }
}

fn propose_plan(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    if !ctx.is_main() {
        return Err("Only the main agent can propose a plan.".into());
    }
    let markdown = arguments.get("plan_markdown").and_then(Value::as_str).ok_or("plan_markdown is required")?;
    let decision = team::hub().propose_plan(markdown, &jev::cancel_requested)?;
    if decision.accepted {
        Ok(json!({"decision": "accepted", "note": "The user accepted the plan. Compose mode is over and all tools are unlocked; carry it out."}))
    } else {
        Ok(json!({"decision": "revise", "feedback": decision.feedback,
            "note": "The user wants changes. Update the plan and call propose_plan again with a complete replacement."}))
    }
}

// ---- subagents ------------------------------------------------------------------------

/// Builds the function that runs one agent turn loop on the agent's own thread.
fn agent_runner(ctx: &ToolContext) -> Runner {
    let base = ctx.clone();
    Arc::new(move |request: &team::RunRequest, update: &dyn Fn(Update)| {
        jev::set_run_stop_flag(request.stop.clone());
        let (access_token, account_id) = match base.provider {
            chat::Provider::ChatGpt => auth::token().map(|tokens| (tokens.access_token, tokens.account_id))?,
            chat::Provider::OpenCode => (crate::opencode::load_api_key().ok_or("OpenCode is not connected. Add an API key in Settings > Connections.")?, None),
        };
        let mut agent_ctx = base.clone();
        agent_ctx.agent_id = request.agent_id.clone();
        agent_ctx.agent_name = request.name.clone();
        agent_ctx.model = request.model.clone();

        let hooks = agent_ctx.hooks();
        let mut prompt = request.prompt.clone();
        if hooks.has_hooks_for(HookEvent::SubagentStart) {
            let outcome = hooks.run(HookEvent::SubagentStart, None, &json!({"session_id": base.chat_id, "agent": request.name, "task": request.prompt}));
            if let Some(reason) = outcome.block {
                return Err(format!("A SubagentStart hook refused this agent: {reason}"));
            }
            for note in outcome.additional_context {
                prompt.push_str(&format!("\n\n[hook context] {note}"));
            }
        }

        let inbox_id = request.agent_id.clone();
        let chat_id = base.chat_id.clone();
        let last_status = Arc::new(Mutex::new(String::new()));
        let options = chat::RunOptions {
            compose: base.compose,
            extra_developer: std::iter::once(SUBAGENT_PROMPT.replace("{name}", &request.name)).chain(crate::skills::catalog_prompt(&crate::skills::list())).collect(),
            hidden_tools: vec!["ask_user".into(), "propose_plan".into(), "agent_spawn".into()],
            inbox: Some(Box::new(move || team_inbox(&inbox_id, &chat_id, &last_status))),
            worker: true,
            session_id: Some(format!("rhea-{}-{}", base.chat_id, request.agent_id)),
            on_stop: None,
            hold_for_team: None,
            compact_percent: base.compact_percent,
            state: Some(team::hub().agent_state(&request.agent_id)),
            on_state: Some(Box::new({
                let agent_id = request.agent_id.clone();
                move |state| team::hub().set_agent_state(&agent_id, state)
            })),
            ledger: Some(crate::ledger::Ledger::for_chat(&base.chat_id, &request.agent_id)),
            endpoint: None,
            fast: base.fast,
            provider: base.provider,
        };
        let outcome = chat::respond_with_options(
            &access_token,
            account_id.as_deref(),
            &request.model,
            &base.effort,
            base.context_window,
            "",
            &request.history,
            &prompt,
            &core_extra_tools(),
            |tool, args| dispatch(&agent_ctx, tool, args),
            options,
            |event| {
                if let Some(delta) = event.delta {
                    update(Update::Delta(delta));
                }
                if let Some(activity) = event.tool_activity {
                    update(Update::Tool(activity));
                }
                if let Some(usage) = event.usage {
                    // Subagents' tokens count toward the lifetime total too (the system prompt and
                    // tool list are what every request carries; memory is not sent to agents).
                    let parts = crate::usage::Parts::of_request(usage.input_tokens, usage.output_tokens, chat::system_prompt_tokens(), crate::tools::toolset_tokens(), 0);
                    crate::usage::record_request(usage.input_tokens, usage.output_tokens, parts);
                    update(Update::Usage(usage));
                }
            },
        );
        if hooks.has_hooks_for(HookEvent::SubagentStop) {
            let text = outcome.as_deref().unwrap_or("");
            hooks.run(HookEvent::SubagentStop, None, &json!({"session_id": base.chat_id, "agent": request.name, "result": text, "ok": outcome.is_ok()}));
        }
        outcome
    })
}

/// The manager is ready to finish: while workers are still running, wait for what they send
/// (up to the user cancelling) so a turn never ends with work in flight or results unread.
pub fn hold_for_team(chat_id: &str) -> Vec<String> {
    let hub = team::hub();
    loop {
        let messages = hub.drain_inbox(chat_id, team::MAIN);
        if !messages.is_empty() || hub.active_count(chat_id) == 0 || jev::cancel_requested() {
            return messages;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Messages for an agent plus a fresh team-status note whenever the team changed.
pub fn team_inbox(recipient: &str, chat_id: &str, last_status: &Mutex<String>) -> Vec<String> {
    let hub = team::hub();
    let mut messages = hub.drain_inbox(chat_id, recipient);
    let status = hub.team_status(chat_id);
    if !status.is_empty() {
        if let Ok(mut last) = last_status.lock() {
            if *last != status {
                *last = status.clone();
                messages.push(status);
            }
        }
    }
    messages
}

fn agent_spawn(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    if !ctx.is_main() {
        return Err("Workers cannot start other workers. Ask \"main\" with agent_send.".into());
    }
    let name = arguments.get("name").and_then(Value::as_str).unwrap_or("");
    let task = arguments.get("task").and_then(Value::as_str).unwrap_or("");
    let model = arguments
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .or_else(|| Some(ctx.subagent_model.clone()).filter(|m| !m.is_empty()))
        .unwrap_or_else(|| ctx.model.clone());
    team::hub().spawn(&ctx.chat_id, name, task, &model, agent_runner(ctx))
}

fn agent_send(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    let to = arguments.get("to").and_then(Value::as_str).ok_or("`to` is required")?;
    let message = arguments.get("message").and_then(Value::as_str).ok_or("`message` is required")?;
    team::hub().send(&ctx.chat_id, &ctx.agent_name, to, message, agent_runner(ctx))
}

/// A message typed by the user in the subagent panel.
pub fn message_agent(ctx: &ToolContext, who: &str, text: &str) -> Result<Value, String> {
    team::hub().send(&ctx.chat_id, "user", who, text, agent_runner(ctx))
}

fn agent_list(ctx: &ToolContext) -> Result<Value, String> {
    let agents = team::hub().list(&ctx.chat_id);
    Ok(json!({"agents": agents.iter().map(|a| json!({
        "id": a.id, "name": a.name, "status": a.status.label(), "model": a.model, "runs": a.runs,
        "task": a.task.chars().take(200).collect::<String>(), "checkpoint": a.checkpoint
    })).collect::<Vec<_>>()}))
}

fn agent_read(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    let who = arguments.get("id").and_then(Value::as_str).ok_or("`id` is required")?;
    let agent = team::hub().find(&ctx.chat_id, who).ok_or_else(|| format!("No agent named '{who}'."))?;
    let tail: String = agent
        .turns
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(|turn| format!("{}: {}", turn.role, turn.content.chars().take(20_000).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");
    let tools_used: usize = agent.turns.iter().map(|turn| turn.tool_activity.len()).sum();
    Ok(json!({"id": agent.id, "name": agent.name, "status": agent.status.label(), "result": agent.result.chars().take(60_000).collect::<String>(),
        "checkpoint": agent.checkpoint, "tools_used": tools_used, "transcript_tail": tail}))
}

fn agent_wait(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    let ids: Vec<String> = arguments
        .get("ids")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let timeout = arguments.get("timeout_ms").and_then(Value::as_u64).unwrap_or(120_000).clamp(100, 600_000);
    team::hub().wait(&ctx.chat_id, &ids, &ctx.agent_id, Duration::from_millis(timeout), &jev::cancel_requested)
}

fn agent_stop(ctx: &ToolContext, arguments: &Value) -> Result<Value, String> {
    let who = arguments.get("id").and_then(Value::as_str).ok_or("`id` is required")?;
    team::hub().stop(&ctx.chat_id, who)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(hooks: HookRegistry) -> ToolContext {
        ToolContext {
            mcp_manager: Arc::new(Mutex::new(McpManager::new(std::env::temp_dir().join("rhea-tools-test-mcp.json")))),
            chrome_alive: Arc::new(AtomicBool::new(false)),
            hooks: Arc::new(RwLock::new(hooks)),
            chat_id: "chat-test-tools".into(),
            agent_id: team::MAIN.into(),
            agent_name: "main".into(),
            model: "test".into(),
            effort: "low".into(),
            context_window: 100_000,
            fast: false,
            provider: chat::Provider::ChatGpt,
            compose: false,
            subagent_model: String::new(),
            compact_percent: 80,
        }
    }

    #[test]
    fn workers_cannot_ask_the_user_or_spawn() {
        let mut ctx = context(HookRegistry::empty());
        ctx.agent_id = "a1".into();
        ctx.agent_name = "worker".into();
        assert!(dispatch(&ctx, "ask_user", &json!({"questions":[{"id":"q","question":"?","options":[],"allow_custom":true}]})).unwrap_err().contains("main"));
        assert!(dispatch(&ctx, "agent_spawn", &json!({"name":"x","task":"y","model":null})).is_err());
        assert!(dispatch(&ctx, "propose_plan", &json!({"plan_markdown":"p"})).is_err());
    }

    #[test]
    fn list_and_get_tools_report_unknown_servers() {
        let ctx = context(HookRegistry::empty());
        let listing = dispatch(&ctx, "list_mcps", &json!({})).unwrap();
        assert_eq!(listing["local_connections"][0]["id"], "shell");
        assert!(dispatch(&ctx, "get_tools_", &json!({"server":"nope"})).unwrap_err().contains("list_mcps"));
        assert!(dispatch(&ctx, "get_tools_", &json!({"server":""})).is_err());
        let chrome = dispatch(&ctx, "get_tools_", &json!({"server":"Chrome"})).unwrap();
        assert!(chrome["load_tools"].as_array().unwrap().len() >= 10);
    }

    #[test]
    fn hold_for_team_returns_at_once_without_workers_and_waits_for_a_running_one() {
        let _serial = team::test_serial();
        let chat = "chat-test-hold";
        assert!(hold_for_team(chat).is_empty());
        let runner: team::Runner = Arc::new(|_request: &team::RunRequest, _update: &dyn Fn(Update)| {
            std::thread::sleep(Duration::from_millis(250));
            Ok("the full report".to_owned())
        });
        team::hub().spawn(chat, "Slow", "work", "m", runner).unwrap();
        let started = std::time::Instant::now();
        let notes = hold_for_team(chat);
        assert!(started.elapsed() >= Duration::from_millis(150), "returned before the worker finished");
        assert!(notes.iter().any(|note| note.contains("the full report")));
        team::hub().forget_chat(chat);
    }

    #[test]
    fn skill_tools_report_unknown_skills_and_validate_arguments() {
        let ctx = context(HookRegistry::empty());
        assert!(dispatch(&ctx, "use_skill", &json!({})).is_err());
        assert!(dispatch(&ctx, "use_skill", &json!({"name":"definitely-not-installed-zz"})).unwrap_err().contains("No skill named"));
        assert!(dispatch(&ctx, "install_skill", &json!({"source":"not a link"})).is_err());
    }

    #[test]
    fn agent_tools_validate_arguments() {
        let mut ctx = context(HookRegistry::empty());
        // not "main": the hub's main inbox is shared with the team tests
        ctx.agent_id = "tester".into();
        ctx.agent_name = "tester".into();
        assert!(dispatch(&ctx, "agent_send", &json!({"to":"ghost","message":"hi"})).is_err());
        assert!(dispatch(&ctx, "agent_read", &json!({"id":"ghost"})).is_err());
        assert!(dispatch(&ctx, "agent_stop", &json!({"id":"ghost"})).is_err());
        let listing = dispatch(&ctx, "agent_list", &json!({})).unwrap();
        assert_eq!(listing["agents"].as_array().unwrap().len(), 0);
        let waited = dispatch(&ctx, "agent_wait", &json!({"ids":null,"timeout_ms":100})).unwrap();
        assert!(waited["agents"].as_array().unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn pre_hook_can_block_and_post_hook_adds_context() {
        let blocking = HookRegistry::from_json(
            &json!({"hooks":{"PreToolUse":[{"matcher":"list_mcps","hooks":[{"command":"[Console]::Error.WriteLine('policy says no'); exit 2"}]}]}}).to_string(),
        )
        .unwrap();
        let error = dispatch(&context(blocking), "list_mcps", &json!({})).unwrap_err();
        assert!(error.contains("policy says no"), "{error}");

        let noting = HookRegistry::from_json(
            &json!({"hooks":{"PostToolUse":[{"matcher":"agent_list","hooks":[{"command":"Write-Output '{\"additionalContext\":\"audited\"}'"}]}]}}).to_string(),
        )
        .unwrap();
        let value = dispatch(&context(noting), "agent_list", &json!({})).unwrap();
        assert_eq!(value["hook_context"][0], "audited");
    }
}
