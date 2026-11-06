//! Rhea's long-term memory.
//!
//! One SQLite file, `~/.rhea/memory/memory.db`, shared by every chat. It remembers what the user
//! said (preferences, standing instructions, decisions, things still to do), what Rhea actually
//! did (verified outcomes and failures taken from tool results, never from what the model claims),
//! how problems were solved, and how each turn ended, and it brings the relevant part back before
//! every message without anyone saying "remember this".
//!
//! It is part of the run loop, not an MCP tool the model might forget to call:
//! * before a run: `briefing` adds a short "Memory" note (standing instructions, facts about the
//!   user, what matches the message, unfinished work, known dead ends);
//! * after each tool result: `observe_tool` queues a verified outcome or failure, off the action
//!   path, so a click or a command never waits for a database;
//! * after a turn: `observe_turn_end` saves a summary and what was left unfinished.
//! The model can also search, add and delete memories itself (`memory_search`, `memory_remember`,
//! `memory_forget`), and Settings > Memory lets the user browse, correct and delete.
//!
//! The design draws on Palace RS (hybrid retrieval, temporal fact graph) and OmniMem
//! (experience weighting, dead ends, contradiction handling), both MIT; see `THIRD_PARTY.md`.

pub mod embed;
pub mod extract;
pub mod rank;
pub mod store;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use serde_json::{Value, json};

use crate::chat_store;
use extract::Draft;
use store::{Filter, Memory as Record, NewMemory, Stored, Store, now_ms};

pub fn db_path() -> PathBuf {
    chat_store::app_data_root().unwrap_or_else(std::env::temp_dir).join("memory").join("memory.db")
}

/// The memory, with its database behind a lock.
pub struct Memory {
    store: Mutex<Store>,
}

#[derive(Default)]
struct Trace {
    project: Option<String>,
    tools: Vec<String>,
    /// The latest unresolved failure: its text, the tool that failed and how many calls ago.
    last_failure: Option<(String, String, usize)>,
}

/// Near-identical text counts as the same memory above this similarity.
const DUPLICATE_COSINE: f32 = 0.95;
const BRIEFING_CHARS: usize = 4_000;
/// A memory this closely related to the message is brought along automatically, however many
/// there are (up to `MAX_STRONG`); weaker matches only fill the remaining room.
pub const STRONG_RELATED: f64 = 0.80;
const WEAK_RELATED: f64 = 0.45;
const MAX_STRONG: usize = 10;
const MAX_RELEVANT: usize = 10;
/// How close in time, within one chat, a memory must be to count as part of the same session.
const SESSION_WINDOW_MS: i64 = 45 * 60 * 1000;

impl Memory {
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        Store::open(path).map(|store| Self { store: Mutex::new(store) }).map_err(|error| format!("Could not open the memory database: {error}"))
    }

    pub fn in_memory() -> Result<Self, String> {
        Store::in_memory().map(|store| Self { store: Mutex::new(store) }).map_err(|error| error.to_string())
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Remember a draft: embed it, skip it if the same thing is already known, replace older
    /// facts on the same topic, and record the facts and dead ends it carries.
    pub fn remember(&self, draft: Draft, chat_id: Option<&str>) -> Result<Stored, String> {
        let embedder = embed::active();
        let vector = embedder.embed(&draft.memory.text);
        let mut memory = draft.memory;
        memory.chat_id = chat_id.map(str::to_owned);
        let mut store = self.store();
        // The same meaning in other words (not just the same text).
        let near = store
            .vector_search(&vector, embedder.name(), &Filter { scopes: vec![memory.scope.clone()], kinds: vec![memory.kind.clone()], ..Default::default() }, 1)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|(_, similarity)| *similarity >= DUPLICATE_COSINE && memory.topic.is_none());
        let stored = match near {
            Some((id, _)) => {
                let _ = store.mark_used(&[id]);
                Stored::Duplicate(id)
            }
            None => store.insert(&memory, Some((embedder.name(), &vector))).map_err(|e| e.to_string())?,
        };
        let id = stored.id();
        for (subject, predicate, object, single) in &draft.triples {
            store.add_triple(subject, predicate, object, Some(id), *single).map_err(|e| e.to_string())?;
        }
        if let (Some((approach, reason)), Stored::New(_)) = (&draft.dead_end, &stored) {
            store.add_dead_end(&memory.scope, approach, reason, Some(id)).map_err(|e| e.to_string())?;
        }
        if let Stored::Duplicate(_) = stored {
            // Failing the same way again makes it a more important thing to know.
            if memory.kind == "failure" {
                if let Ok(Some(found)) = store.get(id) {
                    let _ = store.set_weight(id, (found.weight + 0.1).min(2.0));
                }
            }
        }
        Ok(stored)
    }

    pub fn recall(&self, query: &str, project: Option<&str>, limit: usize, kinds: &[String]) -> Vec<rank::Hit> {
        let embedder = embed::active();
        let store = self.store();
        let hits = rank::recall(&store, embedder.as_ref(), &rank::Query { text: query, project, now_ms: now_ms(), filter: Filter { kinds: kinds.to_vec(), ..Default::default() }, limit }).unwrap_or_default();
        let _ = store.mark_used(&hits.iter().map(|hit| hit.memory.id).collect::<Vec<_>>());
        hits
    }

    pub fn forget(&self, id: i64) -> bool {
        self.store().delete_forever(id).unwrap_or(false)
    }

    /// The user corrected a memory. Its text and vector change, and so do the facts it stated:
    /// the briefing reads the fact graph on its own, so an old answer left there would contradict
    /// the corrected text.
    pub fn correct(&self, id: i64, text: &str) -> bool {
        let embedder = embed::active();
        let vector = embedder.embed(text);
        let store = self.store();
        let updated = store.update_text(id, text, Some((embedder.name(), &vector))).unwrap_or(false);
        if updated {
            let _ = store.restate_facts(id, &extract::facts_in(text));
        }
        updated
    }

    pub fn list(&self, filter: &Filter, limit: usize, offset: usize) -> Vec<Record> {
        self.store().list(filter, limit, offset).unwrap_or_default()
    }

    pub fn counts(&self) -> (i64, Vec<(String, i64)>) {
        let store = self.store();
        (store.count(&Filter::default()).unwrap_or(0), store.counts_by_kind().unwrap_or_default())
    }

    pub fn clear(&self) {
        let _ = self.store().clear_all();
    }

    /// Re-make the vectors that were made by another embedder (after the model changed).
    pub fn reindex(&self, batch: usize) -> usize {
        let embedder = embed::active();
        let store = self.store();
        let pending = store.needing_embedding(embedder.name(), batch).unwrap_or_default();
        for (id, text) in &pending {
            let _ = store.set_embedding(*id, embedder.name(), &embedder.embed(text));
        }
        pending.len()
    }

    pub fn export_json(&self) -> Value {
        let all = self.store().list(&Filter { status: Some("active".into()), ..Default::default() }, 100_000, 0).unwrap_or_default();
        json!(all.iter().map(|m| json!({"id": m.id, "kind": m.kind, "scope": m.scope, "text": m.text, "source": m.source, "verified": m.verified, "created_at": m.created_at, "importance": m.importance})).collect::<Vec<_>>())
    }

    // ---- what goes in ------------------------------------------------------------------

    pub fn observe_user(&self, chat_id: &str, text: &str) -> usize {
        let mut stored = 0;
        for draft in extract::from_user_message(text) {
            if self.remember(draft, Some(chat_id)).is_ok() {
                stored += 1;
            }
        }
        stored
    }

    // ---- what comes back ---------------------------------------------------------------

    /// The "Memory" note for the start of a run, or `None` when nothing is worth saying.
    pub fn briefing(&self, query: &str, project: Option<&str>, chat_id: &str) -> Option<String> {
        let store = self.store();
        let embedder = embed::active();
        let always: Vec<Record> = {
            let mut list = store.list(&Filter { kinds: vec!["instruction".into(), "preference".into()], scopes: vec!["global".into()], ..Default::default() }, 40, 0).unwrap_or_default();
            list.sort_by(|a, b| (b.importance * b.weight + (b.uses as f64).min(10.0) * 0.02).partial_cmp(&(a.importance * a.weight + (a.uses as f64).min(10.0) * 0.02)).unwrap_or(std::cmp::Ordering::Equal));
            list.truncate(6);
            list
        };
        let facts = store.triples(Some("user"), true).unwrap_or_default();
        let hits = rank::recall(&store, embedder.as_ref(), &rank::Query { text: query, project, now_ms: now_ms(), filter: Filter::default(), limit: 24 }).unwrap_or_default();
        let always_ids: Vec<i64> = always.iter().map(|m| m.id).collect();
        let candidates: Vec<&rank::Hit> = hits.iter().filter(|hit| !always_ids.contains(&hit.memory.id) && hit.memory.kind != "task").collect();
        // Strong matches all come along; weaker ones only fill what is left.
        let mut relevant: Vec<&rank::Hit> = candidates.iter().copied().filter(|hit| hit.related >= STRONG_RELATED).take(MAX_STRONG).collect();
        let room = MAX_RELEVANT.saturating_sub(relevant.len());
        relevant.extend(candidates.iter().copied().filter(|hit| hit.related >= WEAK_RELATED && hit.related < STRONG_RELATED).take(room));
        // The full picture: what else happened in the same session as a strongly related memory.
        let mut session: Vec<Record> = Vec::new();
        let mut taken: Vec<i64> = always_ids.iter().copied().chain(relevant.iter().map(|hit| hit.memory.id)).collect();
        for hit in relevant.iter().filter(|hit| hit.related >= STRONG_RELATED) {
            let Some(chat) = hit.memory.chat_id.as_deref() else { continue };
            for near in store.neighbors(chat, hit.memory.created_at, SESSION_WINDOW_MS, &["outcome", "failure", "workflow", "decision", "summary", "task"], &taken, 2).unwrap_or_default() {
                taken.push(near.id);
                session.push(near);
            }
            if session.len() >= 6 {
                break;
            }
        }
        // Unfinished work from here: global, or from this same project, never another project's.
        let task_scopes: Vec<String> = std::iter::once("global".to_owned()).chain(project.map(|p| format!("project:{p}"))).collect();
        let unfinished: Vec<Record> = store
            .list(&Filter { kinds: vec!["task".into()], scopes: task_scopes, ..Default::default() }, 12, 0)
            .unwrap_or_default()
            .into_iter()
            .filter(|task| task.chat_id.as_deref() != Some(chat_id) && (now_ms() - task.created_at) < 30 * 86_400_000)
            .take(3)
            .collect();
        let query_words = rank::tokens(query);
        let dead_ends: Vec<_> = store
            .dead_ends(project.map(|p| format!("project:{p}")).as_deref(), 30)
            .unwrap_or_default()
            .into_iter()
            .filter(|dead| {
                let text = format!("{} {}", dead.approach, dead.reason).to_lowercase();
                query_words.iter().any(|word| word.len() > 3 && text.contains(word.as_str()))
            })
            .take(2)
            .collect();
        if always.is_empty() && facts.is_empty() && relevant.is_empty() && session.is_empty() && unfinished.is_empty() && dead_ends.is_empty() {
            return None;
        }
        let mut out = String::from("## Memory\nRecalled from earlier chats. It can be out of date: check before acting on it, and what the user says now wins over it.\n");
        if !always.is_empty() {
            out.push_str("Standing preferences and instructions:\n");
            for memory in &always {
                out.push_str(&format!("- {}\n", extract::clip(&memory.text, 220)));
            }
        }
        if !facts.is_empty() {
            let facts: Vec<String> = facts.iter().take(6).map(|t| format!("{}: {}", t.predicate.replace('_', " "), t.object)).collect();
            out.push_str(&format!("About the user: {}.\n", facts.join("; ")));
        }
        if !relevant.is_empty() {
            out.push_str("Related to this message (the percentage is how closely each one matches; 80% and up are strong matches):\n");
            for hit in &relevant {
                let memory = &hit.memory;
                let mut tags = vec![memory.kind.clone(), age_label(memory.created_at), format!("{}% related", (hit.related * 100.0).round() as i64)];
                if memory.verified {
                    tags.push("verified".into());
                }
                if let Some(project) = memory.scope.strip_prefix("project:") {
                    tags.push(project.to_owned());
                }
                out.push_str(&format!("- [{}] {}\n", tags.join(" · "), extract::clip(&memory.text, 260)));
            }
        }
        if !session.is_empty() {
            out.push_str("Around the same time in those chats:\n");
            for memory in &session {
                let verified = if memory.verified { " · verified" } else { "" };
                out.push_str(&format!("- [{} · {}{verified}] {}\n", memory.kind, age_label(memory.created_at), extract::clip(&memory.text, 240)));
            }
        }
        if !unfinished.is_empty() {
            out.push_str("Left unfinished earlier:\n");
            for task in &unfinished {
                out.push_str(&format!("- ({}) {}\n", age_label(task.created_at), extract::clip(&task.text, 200)));
            }
        }
        if !dead_ends.is_empty() {
            out.push_str("Known dead ends (do not repeat without a reason):\n");
            for dead in &dead_ends {
                out.push_str(&format!("- {}: {}\n", extract::clip(&dead.approach, 120), extract::clip(&dead.reason, 160)));
            }
        }
        let mut used: Vec<i64> = always_ids;
        used.extend(relevant.iter().map(|hit| hit.memory.id));
        used.extend(session.iter().map(|memory| memory.id));
        let _ = store.mark_used(&used);
        Some(out.chars().take(BRIEFING_CHARS).collect())
    }
}

/// What a briefing costs the context: its size in tokens (about four characters each) and how many
/// memories it carries (one bullet each).
pub fn note_stats(note: &str) -> (u64, usize) {
    let tokens = (note.chars().count() as u64).saturating_add(3) / 4;
    let items = note.lines().filter(|line| line.starts_with("- ")).count();
    (tokens, items)
}

pub fn age_label(created_ms: i64) -> String {
    let days = (now_ms() - created_ms).max(0) / 86_400_000;
    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        2..=13 => format!("{days}d ago"),
        14..=59 => format!("{}w ago", days / 7),
        _ => format!("{}mo ago", days / 30),
    }
}

// ---- the process-wide memory --------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn enabled() -> bool {
    !cfg!(test) && ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

fn shared() -> Option<&'static Arc<Memory>> {
    static SHARED: OnceLock<Option<Arc<Memory>>> = OnceLock::new();
    SHARED
        .get_or_init(|| match Memory::open(&db_path()) {
            Ok(memory) => Some(Arc::new(memory)),
            Err(error) => {
                eprintln!("{error}");
                None
            }
        })
        .as_ref()
}

/// The shared memory, whether or not saving is switched on (Settings reads it either way).
pub fn handle() -> Option<Arc<Memory>> {
    shared().map(Arc::clone)
}

fn traces() -> &'static Mutex<HashMap<String, Trace>> {
    static TRACES: OnceLock<Mutex<HashMap<String, Trace>>> = OnceLock::new();
    TRACES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn trace_of<T>(chat_id: &str, action: impl FnOnce(&mut Trace) -> T) -> T {
    let mut traces = traces().lock().unwrap_or_else(|p| p.into_inner());
    action(traces.entry(chat_id.to_owned()).or_default())
}

enum Job {
    Remember(Draft, String),
}

/// Writes happen on one background thread so recording never delays a tool call.
fn queue() -> &'static mpsc::Sender<Job> {
    static QUEUE: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("rhea-memory".into())
            .spawn(move || {
                for job in receiver {
                    let Some(memory) = handle() else { continue };
                    match job {
                        Job::Remember(draft, chat_id) => {
                            let _ = memory.remember(draft, Some(&chat_id));
                        }
                    }
                }
            })
            .expect("memory thread");
        sender
    })
}

fn submit(draft: Draft, chat_id: &str) {
    let _ = queue().send(Job::Remember(draft, chat_id.to_owned()));
}

/// What the user wrote in a message.
pub fn observe_user(chat_id: &str, text: &str) {
    if !enabled() {
        return;
    }
    for draft in extract::from_user_message(text) {
        submit(draft, chat_id);
    }
}

/// The note to give the model before it answers `text`.
pub fn briefing(chat_id: &str, text: &str) -> Option<String> {
    if !enabled() {
        return None;
    }
    let project = trace_of(chat_id, |trace| trace.project.clone());
    handle()?.briefing(text, project.as_deref(), chat_id)
}

/// One finished tool call. A failure followed by a different step that worked becomes a
/// "workflow": the problem and what fixed it.
pub fn observe_tool(chat_id: &str, agent: &str, name: &str, arguments: &Value, outcome: &Result<Value, String>) {
    if !enabled() {
        return;
    }
    if agent == "main" {
        trace_of(chat_id, |trace| trace.tools.push(name.to_owned()));
    }
    let Some((mut draft, project)) = extract::from_tool(name, arguments, outcome) else { return };
    let resolved = trace_of(chat_id, |trace| {
        if project.is_some() {
            trace.project = project.clone();
        }
        trace.last_failure = trace.last_failure.take().map(|(text, tool, calls)| (text, tool, calls + 1)).filter(|(_, _, calls)| *calls <= 8);
        if draft.memory.kind == "failure" {
            trace.last_failure = Some((draft.memory.text.clone(), name.to_owned(), 0));
            None
        } else if draft.memory.kind == "outcome" && trace.last_failure.as_ref().is_some_and(|(_, tool, _)| tool == name) {
            trace.last_failure.take().map(|(failure, _, _)| failure)
        } else {
            None
        }
    });
    if agent != "main" {
        draft.memory.meta = json!({"agent": agent});
    }
    if let Some(failure) = resolved {
        let mut workflow = Draft { memory: NewMemory::new("workflow", &draft.memory.scope, &format!("Problem: {failure} Fixed by: {}", draft.memory.text), "tool"), triples: Vec::new(), dead_end: None };
        workflow.memory.importance = 0.75;
        workflow.memory.verified = true;
        submit(workflow, chat_id);
    }
    submit(draft, chat_id);
}

/// The end of a turn: its summary and what it left unfinished.
pub fn observe_turn_end(chat_id: &str, user: &str, assistant: &str) {
    if !enabled() {
        return;
    }
    let (tools, project) = trace_of(chat_id, |trace| (std::mem::take(&mut trace.tools), trace.project.clone()));
    for draft in extract::from_turn_end(chat_id, user, assistant, &tools, project.as_deref()) {
        submit(draft, chat_id);
    }
}

// ---- tools the model can call --------------------------------------------------------------

pub const TOOL_NAMES: [&str; 3] = ["memory_search", "memory_remember", "memory_forget"];

fn describe(hit: &rank::Hit) -> Value {
    let memory = &hit.memory;
    json!({
        "id": memory.id, "kind": memory.kind, "scope": memory.scope, "text": memory.text, "verified": memory.verified,
        "age": age_label(memory.created_at),
        "related_percent": (hit.related * 100.0).round() as i64,
        "importance_percent": (hit.importance() * 100.0).round() as i64,
    })
}

/// `memory_search`, `memory_remember`, `memory_forget`.
pub fn tool(chat_id: &str, name: &str, arguments: &Value) -> Result<Value, String> {
    if !enabled() && name != "memory_forget" {
        return Err("Memory is switched off in Settings > Memory.".into());
    }
    let memory = handle().ok_or("The memory database is not available.")?;
    let text = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    match name {
        "memory_search" => {
            let query = text("query");
            if query.is_empty() {
                return Err("`query` is required.".into());
            }
            let kinds: Vec<String> = arguments.get("kinds").and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
            let limit = arguments.get("limit").and_then(Value::as_u64).unwrap_or(6).clamp(1, 20) as usize;
            let project = trace_of(chat_id, |trace| trace.project.clone());
            let hits = memory.recall(&query, project.as_deref(), limit, &kinds);
            Ok(json!({"count": hits.len(), "memories": hits.iter().map(describe).collect::<Vec<_>>()}))
        }
        "memory_remember" => {
            let body = text("text");
            if body.chars().count() < 4 {
                return Err("`text` is required.".into());
            }
            let kind = match text("kind").as_str() {
                kind @ ("preference" | "instruction" | "fact" | "decision" | "task" | "note") => kind.to_owned(),
                _ => "note".to_owned(),
            };
            let scope = match text("scope").as_str() {
                "global" | "" => "global".to_owned(),
                other => format!("project:{}", other.trim_start_matches("project:").to_lowercase()),
            };
            let mut draft = Draft { memory: NewMemory::new(&kind, &scope, &extract::redact(&body), "assistant"), triples: Vec::new(), dead_end: None };
            draft.memory.importance = 0.7;
            let stored = memory.remember(draft, Some(chat_id))?;
            Ok(json!({"id": stored.id(), "stored": !matches!(stored, Stored::Duplicate(_)), "note": "Saved. The user can review or delete it in Settings > Memory."}))
        }
        "memory_forget" => {
            let id = arguments.get("id").and_then(Value::as_i64).ok_or("`id` is required (from memory_search).")?;
            Ok(json!({"id": id, "forgotten": memory.forget(id)}))
        }
        _ => Err(format!("Unknown memory tool '{name}'.")),
    }
}

/// Memories from the chats that already exist, so a new install of this feature starts with
/// history instead of empty. Each chat is read once (it is marked as imported).
pub fn import_existing_chats(progress: impl Fn(usize, usize)) -> usize {
    let (Some(memory), Some(root)) = (handle(), chat_store::chats_root()) else { return 0 };
    let ids: Vec<String> = std::fs::read_dir(&root).map(|entries| entries.flatten().filter(|e| e.path().join("history.json").is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|id| chat_store::is_valid_chat_id(id)).collect()).unwrap_or_default();
    let mut added = 0;
    for (index, id) in ids.iter().enumerate() {
        progress(index, ids.len());
        let marker = format!("imported:{id}");
        if memory.store().meta_get(&marker).is_some() {
            continue;
        }
        let turns: Vec<Value> = std::fs::read(root.join(id).join("history.json")).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).and_then(|v| v.get("turns").and_then(Value::as_array).cloned()).unwrap_or_default();
        let mut last_user = String::new();
        for turn in &turns {
            let content = turn.get("content").and_then(Value::as_str).unwrap_or("");
            match turn.get("role").and_then(Value::as_str) {
                Some("user") => {
                    added += memory.observe_user(id, content);
                    last_user = content.to_owned();
                }
                Some("assistant") if !last_user.is_empty() => {
                    for draft in extract::from_turn_end(id, &last_user, content, &[], None) {
                        if draft.memory.kind == "summary" && memory.remember(draft, Some(id)).is_ok() {
                            added += 1;
                        }
                    }
                    last_user.clear();
                }
                _ => {}
            }
        }
        memory.store().meta_set(&marker, "1");
    }
    progress(ids.len(), ids.len());
    added
}

#[cfg(test)]
mod note_tests {
    use super::*;

    #[test]
    fn a_briefing_is_measured_in_tokens_and_memories() {
        let note = "## Memory\nRecalled.\nStanding preferences:\n- Prefers dark themes\n- Replies short\nAbout the user: name: Dana.\n- [fact] one more\n";
        let (tokens, items) = note_stats(note);
        assert_eq!(items, 3);
        assert_eq!(tokens, (note.chars().count() as u64 + 3) / 4);
        assert_eq!(note_stats(""), (0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Memory {
        Memory::in_memory().unwrap()
    }

    #[test]
    fn what_the_user_says_comes_back_in_a_later_briefing_without_being_asked() {
        let memory = memory();
        memory.observe_user("chat-1", "My name is Dana. Always reply in short sentences. I prefer dark themes.");
        let note = memory.briefing("can you tidy my settings page?", None, "chat-2").unwrap();
        assert!(note.contains("## Memory") && note.contains("Always reply in short sentences"), "{note}");
        assert!(note.contains("name: Dana"), "{note}");
        assert!(note.chars().count() <= BRIEFING_CHARS);
        assert!(memory.briefing("anything", None, "chat-2").is_some());
        assert!(Memory::in_memory().unwrap().briefing("anything", None, "c").is_none(), "an empty memory says nothing");
    }

    #[test]
    fn a_changed_fact_replaces_the_old_one_everywhere() {
        let memory = memory();
        memory.observe_user("c1", "I live in Lisbon.");
        memory.observe_user("c2", "I live in Berlin.");
        let note = memory.briefing("where should the package ship?", None, "c3").unwrap();
        assert!(note.contains("Berlin") && !note.contains("Lisbon"), "{note}");
        let active = memory.list(&Filter { kinds: vec!["fact".into()], ..Default::default() }, 10, 0);
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn verified_outcomes_and_failures_are_recalled_for_the_project() {
        let memory = memory();
        let ok = Ok(json!({"success": true, "exit_code": 0}));
        let failed = Ok(json!({"success": false, "exit_code": 1, "stderr": "error: linker not found"}));
        let args = json!({"command": "cargo build --release", "working_directory": "."});
        for (outcome, chat) in [(&failed, "c1"), (&ok, "c2")] {
            let (draft, _) = extract::from_tool("mcp__rhea_shell__run_command", &args, outcome).unwrap();
            memory.remember(draft, Some(chat)).unwrap();
        }
        let hits = memory.recall("why did cargo build fail", None, 5, &[]);
        assert!(hits.iter().any(|hit| hit.memory.kind == "failure" && hit.memory.text.contains("linker not found")), "{hits:?}");
        let note = memory.briefing("cargo build is failing again", None, "c3").unwrap();
        assert!(note.contains("Known dead ends") && note.contains("linker not found"), "{note}");
    }

    #[test]
    fn unfinished_work_from_another_chat_is_brought_back() {
        let memory = memory();
        for draft in extract::from_turn_end("chat-old", "Set up the signing for the Mac release build", "I added the workflow. The certificate secrets are not yet configured, so it still signs ad hoc.", &["mcp__rhea_shell__write_file".into()], Some("rhea")) {
            memory.remember(draft, Some("chat-old")).unwrap();
        }
        let note = memory.briefing("let's continue with the release", Some("rhea"), "chat-new").unwrap();
        assert!(note.contains("Left unfinished earlier") && note.contains("not yet configured"), "{note}");
        let same_chat = memory.briefing("continue", Some("rhea"), "chat-old").unwrap_or_default();
        assert!(!same_chat.contains("Left unfinished"), "a chat is not reminded of its own pending work");
    }

    #[test]
    fn repeating_a_memory_does_not_duplicate_it_and_failures_gain_weight() {
        let memory = memory();
        let failed = Ok(json!({"success": false, "exit_code": 2, "stderr": "boom"}));
        let args = json!({"command": "make all", "working_directory": "."});
        for _ in 0..3 {
            let (draft, _) = extract::from_tool("mcp__rhea_shell__run_command", &args, &failed).unwrap();
            memory.remember(draft, Some("c")).unwrap();
        }
        let all = memory.list(&Filter { kinds: vec!["failure".into()], ..Default::default() }, 10, 0);
        assert_eq!(all.len(), 1);
        assert!(all[0].weight > 1.1, "{}", all[0].weight);
    }

    #[test]
    fn the_user_can_correct_and_delete() {
        let memory = memory();
        memory.observe_user("c1", "I prefer tabs over spaces.");
        let id = memory.list(&Filter::default(), 5, 0)[0].id;
        assert!(memory.correct(id, "I prefer spaces over tabs."));
        assert!(memory.recall("tabs or spaces", None, 3, &[])[0].memory.text.contains("spaces over tabs"));
        assert!(memory.forget(id));
        assert!(memory.recall("tabs or spaces", None, 3, &[]).is_empty());
        assert_eq!(memory.counts().0, 0);
    }

    fn task(scope: &str, text: &str) -> Draft {
        Draft { memory: NewMemory::new("task", scope, text, "assistant"), triples: Vec::new(), dead_end: None }
    }

    #[test]
    fn correcting_a_fact_corrects_what_the_briefing_says_about_the_user() {
        let memory = memory();
        memory.observe_user("c1", "My name is Dana.");
        let id = memory.list(&Filter { kinds: vec!["fact".into()], ..Default::default() }, 5, 0)[0].id;
        assert!(memory.correct(id, "The user's name is Alex."));
        let note = memory.briefing("what should I call you?", None, "c2").unwrap();
        assert!(note.contains("name: Alex") && !note.contains("Dana"), "{note}");
        let current = memory.store().triples(Some("user"), true).unwrap();
        assert_eq!(current.iter().filter(|t| t.predicate == "name").count(), 1, "one current answer, not two");
    }

    #[test]
    fn correcting_a_fact_into_something_else_leaves_no_old_fact_behind() {
        let memory = memory();
        memory.observe_user("c1", "I live in Lisbon.");
        let id = memory.list(&Filter { kinds: vec!["fact".into()], ..Default::default() }, 5, 0)[0].id;
        assert!(memory.correct(id, "Lisbon was a long time ago."));
        assert!(memory.store().triples(Some("user"), true).unwrap().is_empty());
    }

    #[test]
    fn unfinished_work_from_another_project_is_not_shown() {
        let memory = memory();
        memory.remember(task("project:alpha", "Unfinished: ship the alpha installer."), Some("old-chat")).unwrap();
        memory.remember(task("global", "Unfinished: renew the passport."), Some("old-chat")).unwrap();
        let in_beta = memory.briefing("continue", Some("beta"), "now").unwrap();
        assert!(!in_beta.contains("alpha installer") && in_beta.contains("renew the passport"), "{in_beta}");
        let in_alpha = memory.briefing("continue", Some("alpha"), "now").unwrap();
        assert!(in_alpha.contains("alpha installer"), "{in_alpha}");
        let nowhere = memory.briefing("continue", None, "now").unwrap();
        assert!(!nowhere.contains("alpha installer"), "{nowhere}");
    }

    #[test]
    fn dead_ends_from_another_project_never_leak_in() {
        let memory = memory();
        {
            let store = memory.store();
            store.add_dead_end("project:alpha", "cargo build with the gnu target", "linker not found", None).unwrap();
            store.add_dead_end("global", "restart the router twice", "it did not help", None).unwrap();
        }
        let store = memory.store();
        let none = store.dead_ends(None, 10).unwrap();
        assert_eq!(none.len(), 1, "with no project only the global one: {none:?}");
        assert_eq!(none[0].scope, "global");
        assert_eq!(store.dead_ends(Some("project:alpha"), 10).unwrap().len(), 2);
        assert_eq!(store.dead_ends(Some("project:beta"), 10).unwrap().len(), 1);
    }

    #[test]
    fn exporting_lists_what_is_remembered() {
        let memory = memory();
        memory.observe_user("c1", "Never use emoji in commit messages.");
        let exported = memory.export_json();
        assert_eq!(exported.as_array().unwrap().len(), 1);
        assert_eq!(exported[0]["kind"], "instruction");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age_label(now_ms()), "today");
        assert_eq!(age_label(now_ms() - 86_400_000), "yesterday");
        assert_eq!(age_label(now_ms() - 5 * 86_400_000), "5d ago");
        assert_eq!(age_label(now_ms() - 90 * 86_400_000), "3mo ago");
    }
}
