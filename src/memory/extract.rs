//! What is worth remembering, decided by plain rules so it costs nothing and needs no model.
//!
//! * From what the user writes: who they are, what they prefer, standing instructions, decisions
//!   and things they still have to do (the statement patterns follow Palace RS's general
//!   extractor, MIT).
//! * From what the tools actually did: verified outcomes (a command's exit code, a file written, a
//!   plan's status) and failures. What the model merely said it did is never stored as an outcome.
//! * From how a turn ended: a short summary, and anything left unfinished.

use serde_json::Value;

use super::store::NewMemory;

/// A memory plus the facts it implies, ready for `Memory::remember`.
#[derive(Debug, Clone)]
pub struct Draft {
    pub memory: NewMemory,
    /// `(subject, predicate, object, single_valued)`.
    pub triples: Vec<(String, String, String, bool)>,
    /// An approach that did not work, for the dead-end list: `(approach, reason)`.
    pub dead_end: Option<(String, String)>,
}

impl Draft {
    fn new(kind: &str, scope: &str, text: &str, source: &str, importance: f64) -> Self {
        let mut memory = NewMemory::new(kind, scope, text, source);
        memory.importance = importance;
        Self { memory, triples: Vec::new(), dead_end: None }
    }
}

pub fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max.saturating_sub(1)).collect::<String>().trim_end())
    }
}

/// Split on whitespace, but keep a quoted value together even when it holds spaces, so a secret
/// such as `--password "correct horse battery"` is one piece. A quote only opens at the start of a
/// piece, after punctuation (`=`, `:`, `{`) or glued to a flag (`-p'a b'`), never inside a word:
/// an apostrophe in "don't" does not swallow what follows it.
fn pieces(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut open: Option<char> = None;
    for c in text.chars() {
        match open {
            Some(quote) => {
                current.push(c);
                if c == quote {
                    open = None;
                }
            }
            None if (c == '"' || c == '\'') && (current.is_empty() || current.starts_with('-') || current.chars().last().is_some_and(|last| !last.is_alphanumeric())) => {
                open = Some(c);
                current.push(c);
            }
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    pieces.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// A piece wrapped in one kind of quote: the quote and what is inside it.
fn quoted(piece: &str) -> Option<(char, &str)> {
    let quote = piece.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    (piece.len() >= 2 && piece.ends_with(quote)).then(|| (quote, &piece[1..piece.len() - 1]))
}

/// Names whose value, in the piece after them, is a secret.
const SECRET_NAMES: [&str; 9] = ["password", "passwd", "token", "secret", "api_key", "apikey", "api-key", "authorization", "bearer"];

/// Take obvious secrets out of text before it is stored. A quoted value with spaces counts as one
/// value and is hidden whole.
pub fn redact(text: &str) -> String {
    let mut out = Vec::new();
    let mut hide_next = false;
    let all = pieces(text);
    for (position, word) in all.iter().enumerate() {
        let word = word.as_str();
        let lower = word.to_lowercase();
        let looks_like_key = word.len() >= 32 && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=')) && word.chars().any(|c| c.is_ascii_digit()) && word.chars().any(|c| c.is_ascii_alphabetic());
        let secret_pair = ["password=", "passwd=", "token=", "secret=", "api_key=", "apikey=", "key=", "authorization="].iter().find(|p| lower.contains(*p));
        let glued_flag = word.starts_with("-p") && !word.starts_with("--") && word[2..].starts_with(['"', '\'']);
        let name = lower.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
        if hide_next || looks_like_key || word.starts_with("sk-") || word.starts_with("ghp_") || word.starts_with("xox") {
            out.push("[hidden]".to_owned());
            hide_next = false;
        } else if let Some((quote, inner)) = quoted(word).filter(|_| secret_pair.is_none()) {
            // A quoted argument (a JSON body, a shell string) is read for secrets inside it too.
            out.push(format!("{quote}{}{quote}", redact(inner)));
        } else if glued_flag {
            out.push("-p[hidden]".to_owned());
        } else if let Some(pair) = secret_pair {
            let at = lower.find(pair).unwrap_or(0) + pair.len();
            out.push(format!("{}[hidden]", &word[..at.min(word.len())]));
        } else {
            // `--password x`, `-p x`, `bearer x`, and JSON-style `"password": "x y"`.
            // `Authorization: Bearer x` hides x (the scheme word is not the secret); `Authorization: x` hides x.
            let scheme_follows = all.get(position + 1).is_some_and(|next| matches!(next.to_lowercase().as_str(), "bearer" | "basic" | "token"));
            hide_next = matches!(lower.as_str(), "-p" | "--password" | "--token" | "--secret" | "--api-key")
                || name == "bearer"
                || (matches!(name, "basic" | "token") && position > 0 && all[position - 1].to_lowercase().trim_end_matches(':') == "authorization")
                || (SECRET_NAMES.contains(&name) && lower.ends_with([':', '=']) && !(name == "authorization" && scheme_follows));
            out.push(word.to_owned());
        }
    }
    out.join(" ")
}

/// Facts a memory's text states, for when the user corrects it: "The user's name is Alex." gives
/// `(user, name, Alex, single-valued)`. Third-person wording is read as the user's own words.
pub fn facts_in(text: &str) -> Vec<(String, String, String, bool)> {
    let lower = text.to_lowercase();
    let mut spoken = text.to_owned();
    if lower.len() == text.len() {
        for (third, first) in [
            ("the user's name is ", "my name is "),
            ("user's name is ", "my name is "),
            ("the user is called ", "i'm called "),
            ("the user lives in ", "i live in "),
            ("the user works at ", "i work at "),
            ("the user works for ", "i work for "),
            ("the user's time zone is ", "my time zone is "),
            ("the user's timezone is ", "my timezone is "),
        ] {
            if let Some(at) = lower.find(third) {
                spoken.replace_range(at..at + third.len(), first);
                break;
            }
        }
    }
    from_user_message(&spoken).into_iter().flat_map(|draft| draft.triples).collect()
}

pub fn sentences(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        current.push(*c);
        let ends = matches!(c, '.' | '!' | '?' | '\n') && chars.get(index + 1).is_none_or(|next| next.is_whitespace());
        if ends {
            let sentence = current.trim().to_owned();
            if !sentence.is_empty() {
                found.push(sentence);
            }
            current.clear();
        }
    }
    let rest = current.trim();
    if !rest.is_empty() {
        found.push(rest.to_owned());
    }
    found
}

fn after<'a>(lower: &'a str, original: &'a str, marker: &str) -> Option<&'a str> {
    let at = lower.find(marker)?;
    original.get(at + marker.len()..)
}

fn clean_value(value: &str) -> String {
    let cut = value.split(['.', ',', ';', '\n']).next().unwrap_or(value);
    cut.trim().trim_matches(|c: char| matches!(c, '"' | '\'' | '!' | '?')).trim().to_owned()
}

const INSTRUCTION_STARTS: [&str; 12] = ["always ", "never ", "from now on", "from here on", "remember that", "remember to", "remember:", "please always", "please never", "don't ever", "do not ever", "keep in mind"];
const PREFERENCE_MARKERS: [&str; 14] = ["i prefer", "i like ", "i love ", "i hate ", "i don't like", "i do not like", "i dislike", "my favorite", "my favourite", "i usually", "i always", "i never", "i'd rather", "i would rather"];
const DECISION_MARKERS: [&str; 9] = ["we decided", "i decided", "let's go with", "lets go with", "we'll go with", "we will use", "we're going with", "we chose", "decision:"];
const TASK_MARKERS: [&str; 6] = ["remind me", "i need to", "i have to", "don't forget", "todo:", "i still need"];

/// Memories in what the user wrote. Questions, code and very long lines are not statements.
pub fn from_user_message(text: &str) -> Vec<Draft> {
    let mut drafts: Vec<Draft> = Vec::new();
    for sentence in sentences(text) {
        let flat = clip(&sentence, 300);
        let lower = flat.to_lowercase();
        if flat.chars().count() < 8 || flat.ends_with('?') || flat.contains("```") || lower.starts_with('/') {
            continue;
        }
        let original = flat.as_str();
        let mut draft: Option<Draft> = None;

        let identity = |marker: &str| after(&lower, original, marker).map(clean_value).filter(|v| !v.is_empty() && v.chars().count() <= 60);
        if let Some(name) = identity("my name is ").or_else(|| identity("call me ")).or_else(|| identity("i'm called ")) {
            let mut d = Draft::new("fact", "global", &format!("The user's name is {name}."), "user", 0.9);
            d.memory.topic = Some("name".into());
            d.triples.push(("user".into(), "name".into(), name, true));
            draft = Some(d);
        } else if let Some(place) = identity("i live in ") {
            let mut d = Draft::new("fact", "global", &format!("The user lives in {place}."), "user", 0.7);
            d.memory.topic = Some("residence".into());
            d.triples.push(("user".into(), "lives_in".into(), place, true));
            draft = Some(d);
        } else if let Some(employer) = identity("i work at ").or_else(|| identity("i work for ")) {
            let mut d = Draft::new("fact", "global", &format!("The user works at {employer}."), "user", 0.7);
            d.memory.topic = Some("employer".into());
            d.triples.push(("user".into(), "works_at".into(), employer, true));
            draft = Some(d);
        } else if let Some(zone) = identity("my timezone is ").or_else(|| identity("my time zone is ")) {
            let mut d = Draft::new("fact", "global", &format!("The user's time zone is {zone}."), "user", 0.6);
            d.memory.topic = Some("timezone".into());
            d.triples.push(("user".into(), "timezone".into(), zone, true));
            draft = Some(d);
        } else if INSTRUCTION_STARTS.iter().any(|start| lower.starts_with(start)) || lower.contains(" from now on") {
            draft = Some(Draft::new("instruction", "global", original, "user", 0.85));
        } else if DECISION_MARKERS.iter().any(|marker| lower.contains(marker)) {
            draft = Some(Draft::new("decision", "global", original, "user", 0.7));
        } else if PREFERENCE_MARKERS.iter().any(|marker| lower.contains(marker)) {
            draft = Some(Draft::new("preference", "global", original, "user", 0.7));
        } else if TASK_MARKERS.iter().any(|marker| lower.contains(marker)) && flat.chars().count() <= 160 {
            draft = Some(Draft::new("task", "global", original, "user", 0.5));
        }
        if let Some(mut draft) = draft {
            draft.memory.text = redact(&draft.memory.text);
            if !drafts.iter().any(|existing| existing.memory.text == draft.memory.text) {
                drafts.push(draft);
            }
        }
    }
    drafts.truncate(6);
    drafts
}

// ---- tools ------------------------------------------------------------------------------

const TRIVIAL_COMMANDS: [&str; 26] = [
    "ls", "dir", "cat", "type", "echo", "pwd", "cd", "where", "which", "head", "tail", "grep", "rg", "find", "get-childitem", "get-content", "select-string", "test-path", "whoami", "date", "hostname", "cls", "clear", "get-location", "set-location", "wc",
];

fn first_word(command: &str) -> String {
    command.trim_start().split_whitespace().next().unwrap_or("").to_lowercase()
}

fn is_trivial_command(command: &str) -> bool {
    let word = first_word(command);
    if TRIVIAL_COMMANDS.contains(&word.as_str()) {
        return true;
    }
    if word == "git" {
        let sub = command.split_whitespace().nth(1).unwrap_or("").to_lowercase();
        return matches!(sub.as_str(), "status" | "diff" | "log" | "show" | "branch" | "remote" | "rev-parse" | "ls-files" | "blame");
    }
    false
}

/// The folder name of the project a path belongs to: the nearest parent holding a repository or a
/// build manifest.
pub fn project_of(path: &str) -> Option<String> {
    let mut current = std::path::Path::new(path);
    if current.is_file() || current.extension().is_some() {
        current = current.parent()?;
    }
    // The home folder itself is never a project, even when it holds a stray package.json.
    let home = crate::chat_store::app_data_root().and_then(|root| root.parent().map(std::path::Path::to_path_buf));
    for _ in 0..12 {
        if home.as_deref() == Some(current) {
            return None;
        }
        if [".git", "Cargo.toml", "package.json", "pyproject.toml", "go.mod"].iter().any(|marker| current.join(marker).exists()) {
            return current.file_name().map(|name| name.to_string_lossy().to_lowercase());
        }
        current = current.parent()?;
    }
    None
}

fn last_error_line(text: &str) -> String {
    let line = text.lines().rev().map(str::trim).find(|line| !line.is_empty() && !line.starts_with("at ") && line.len() > 3).unwrap_or("");
    clip(&redact(line), 200)
}

fn scope_for(project: Option<&str>) -> String {
    project.map(|name| format!("project:{name}")).unwrap_or_else(|| "global".to_owned())
}

/// A verified outcome or failure from one tool call, if it is worth keeping. `project` is the
/// project the call worked in, when its arguments say so.
pub fn from_tool(name: &str, arguments: &Value, outcome: &Result<Value, String>) -> Option<(Draft, Option<String>)> {
    let text = |key: &str| arguments.get(key).and_then(Value::as_str).unwrap_or("");
    match name {
        "mcp__rhea_shell__run_command" => {
            let command = clip(&redact(text("command")), 220);
            let project = project_of(text("working_directory"));
            let scope = scope_for(project.as_deref());
            match outcome {
                Ok(result) if result.get("detached").and_then(Value::as_bool) == Some(true) => {
                    let mut draft = Draft::new("outcome", &scope, &format!("Started in the background: `{command}`."), "tool", 0.4);
                    draft.memory.verified = true;
                    Some((draft, project))
                }
                Ok(result) => {
                    let success = result.get("success").and_then(Value::as_bool).unwrap_or(false);
                    let code = result.get("exit_code").and_then(Value::as_i64);
                    if success {
                        if is_trivial_command(text("command")) {
                            return None;
                        }
                        let mut draft = Draft::new("outcome", &scope, &format!("Ran `{command}` successfully."), "tool", 0.45);
                        draft.memory.verified = true;
                        Some((draft, project))
                    } else {
                        let timed_out = result.get("timed_out").and_then(Value::as_bool).unwrap_or(false);
                        let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
                        let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
                        let why = if timed_out { "it timed out".to_owned() } else { last_error_line(if stderr.trim().is_empty() { stdout } else { stderr }) };
                        let code = code.map(|c| format!(" (exit {c})")).unwrap_or_default();
                        let mut draft = Draft::new("failure", &scope, &format!("`{command}` failed{code}: {why}"), "tool", 0.6);
                        draft.memory.verified = true;
                        draft.dead_end = Some((command, why));
                        Some((draft, project))
                    }
                }
                Err(error) => {
                    let mut draft = Draft::new("failure", &scope, &format!("`{command}` could not run: {}", clip(&redact(error), 200)), "tool", 0.5);
                    draft.memory.verified = true;
                    Some((draft, project))
                }
            }
        }
        "mcp__rhea_shell__write_file" | "mcp__rhea_shell__edit_file" | "mcp__rhea_shell__create_directory" if outcome.is_ok() => {
            let path = text("path");
            let project = project_of(path);
            let verb = match name {
                "mcp__rhea_shell__write_file" => "Wrote",
                "mcp__rhea_shell__edit_file" => "Edited",
                _ => "Created folder",
            };
            let mut draft = Draft::new("outcome", &scope_for(project.as_deref()), &format!("{verb} {path}."), "tool", 0.4);
            draft.memory.verified = true;
            draft.memory.meta = serde_json::json!({"path": path});
            Some((draft, project))
        }
        "open_app" | "desktop_launch" if outcome.is_ok() => {
            let mut draft = Draft::new("outcome", "global", &format!("Opened the app {}.", clip(text("app"), 80)), "tool", 0.3);
            draft.memory.verified = true;
            Some((draft, None))
        }
        "jev_run_plan" => {
            let steps = arguments.get("steps").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            let app = clip(text("app"), 60);
            let place = if app.is_empty() { String::new() } else { format!(" in {app}") };
            match outcome {
                Ok(result) => {
                    let status = result.get("status").and_then(Value::as_str).unwrap_or("finished");
                    let done = status.starts_with("completed");
                    let reason = result.get("reason").or_else(|| result.get("error")).and_then(Value::as_str).unwrap_or("");
                    let mut draft = if done {
                        Draft::new("outcome", "global", &format!("Desktop plan{place}: {steps} steps {status}."), "tool", 0.45)
                    } else {
                        Draft::new("failure", "global", &format!("Desktop plan{place} ended as {status}{}", if reason.is_empty() { ".".to_owned() } else { format!(": {}", clip(reason, 160)) }), "tool", 0.55)
                    };
                    draft.memory.verified = true;
                    Some((draft, None))
                }
                Err(error) => {
                    let mut draft = Draft::new("failure", "global", &format!("Desktop plan{place} failed: {}", clip(error, 180)), "tool", 0.55);
                    draft.memory.verified = true;
                    Some((draft, None))
                }
            }
        }
        "browser__navigate" | "browser__open" if outcome.is_ok() => {
            let url = clip(text("url"), 160);
            if url.is_empty() {
                return None;
            }
            let mut draft = Draft::new("outcome", "global", &format!("Opened {url} in Chrome."), "tool", 0.25);
            draft.memory.verified = true;
            Some((draft, None))
        }
        "install_skill" if outcome.is_ok() => {
            let installed = outcome.as_ref().ok().and_then(|r| r.get("installed")).and_then(Value::as_str).unwrap_or("a skill");
            let mut draft = Draft::new("outcome", "global", &format!("Installed the skill {installed} from {}.", clip(text("source"), 120)), "tool", 0.45);
            draft.memory.verified = true;
            Some((draft, None))
        }
        "agent_spawn" if outcome.is_ok() => {
            let mut draft = Draft::new("outcome", "global", &format!("Started the subagent {}: {}", clip(text("name"), 40), clip(&redact(text("task")), 160)), "tool", 0.3);
            draft.memory.verified = true;
            Some((draft, None))
        }
        _ => match outcome {
            // Any other state-changing tool that failed is worth a note; reads and questions are not.
            Err(error) if is_state_changing(name) => {
                let mut draft = Draft::new("failure", "global", &format!("{name} failed: {}", clip(&redact(error), 200)), "tool", 0.4);
                draft.memory.verified = true;
                Some((draft, None))
            }
            _ => None,
        },
    }
}

fn is_state_changing(name: &str) -> bool {
    name.starts_with("desktop_") && !matches!(name, "desktop_snapshot" | "desktop_find" | "desktop_get" | "desktop_list_apps" | "desktop_list_windows" | "desktop_screenshot" | "desktop_wait")
        || name.starts_with("browser__") && !matches!(name, "browser__snapshot" | "browser__extract" | "browser__tabs")
        || matches!(name, "mcp_add_servers" | "install_skill")
}

// ---- how a turn ended -------------------------------------------------------------------

const UNFINISHED_MARKERS: [&str; 10] = ["next step", "still need", "remaining", "not yet", "todo", "left to do", "could not", "couldn't", "unable to", "failed to"];

/// A summary of the turn and anything it left unfinished.
pub fn from_turn_end(chat_id: &str, user: &str, assistant: &str, tools: &[String], project: Option<&str>) -> Vec<Draft> {
    let mut drafts = Vec::new();
    let scope = scope_for(project);
    let user = clip(&redact(user), 160);
    if user.chars().count() < 12 || (tools.is_empty() && assistant.chars().count() < 200) {
        return drafts;
    }
    let outcome: String = sentences(assistant).into_iter().take(2).collect::<Vec<_>>().join(" ");
    let mut counts: Vec<(String, usize)> = Vec::new();
    for tool in tools {
        let short = tool.rsplit("__").next().unwrap_or(tool).to_owned();
        match counts.iter_mut().find(|(name, _)| *name == short) {
            Some((_, count)) => *count += 1,
            None => counts.push((short, 1)),
        }
    }
    let used = counts.iter().take(6).map(|(name, count)| if *count > 1 { format!("{name} x{count}") } else { name.clone() }).collect::<Vec<_>>().join(", ");
    let mut text = format!("Asked: {user} Result: {}", clip(&redact(&outcome), 320));
    if !used.is_empty() {
        text.push_str(&format!(" Tools: {used}."));
    }
    let mut summary = Draft::new("summary", &scope, &text, "assistant", 0.45);
    // One summary per turn, however many times that turn is seen (live, then again when chats are indexed).
    summary.memory.key = Some(format!("{chat_id}:{}", super::store::text_hash(&user)));
    drafts.push(summary);
    for sentence in sentences(assistant) {
        let lower = sentence.to_lowercase();
        if UNFINISHED_MARKERS.iter().any(|marker| lower.contains(marker)) && sentence.chars().count() <= 240 && !sentence.ends_with('?') {
            drafts.push(Draft::new("task", &scope, &format!("Unfinished: {}", clip(&redact(&sentence), 220)), "assistant", 0.55));
            if drafts.len() >= 3 {
                break;
            }
        }
    }
    drafts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kinds(text: &str) -> Vec<(String, String)> {
        from_user_message(text).into_iter().map(|d| (d.memory.kind, d.memory.text)).collect()
    }

    #[test]
    fn statements_about_the_user_become_memories_and_questions_do_not() {
        let found = kinds("My name is Dana. I prefer dark themes in every app. Always reply in short sentences. What should I prefer?");
        assert_eq!(found[0], ("fact".into(), "The user's name is Dana.".into()));
        assert!(found.iter().any(|(kind, text)| kind == "preference" && text.contains("dark themes")));
        assert!(found.iter().any(|(kind, text)| kind == "instruction" && text.starts_with("Always reply")));
        assert_eq!(found.len(), 3, "{found:?}");
    }

    #[test]
    fn identity_facts_carry_a_topic_and_graph_triples() {
        let drafts = from_user_message("I live in Lisbon.");
        assert_eq!(drafts[0].memory.topic.as_deref(), Some("residence"));
        assert_eq!(drafts[0].triples, [("user".to_owned(), "lives_in".to_owned(), "Lisbon".to_owned(), true)]);
    }

    #[test]
    fn a_request_is_not_a_standing_preference() {
        let found = kinds("I want you to read the code that you run on and find bugs.");
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(kinds("I always want short answers.")[0].0, "preference");
    }

    #[test]
    fn decisions_tasks_and_code_are_told_apart() {
        let found = kinds("We decided to use SQLite for storage. Remind me to renew the domain. ```let i prefer = 1;```");
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].0, "decision");
        assert_eq!(found[1].0, "task");
    }

    #[test]
    fn secrets_are_hidden_before_anything_is_stored() {
        assert_eq!(redact("curl -H Authorization: Bearer abc123 http://x"), "curl -H Authorization: Bearer [hidden] http://x");
        assert!(redact("run with --password hunter2 now").contains("[hidden]") && !redact("run with --password hunter2 now").contains("hunter2"));
        assert!(redact("TOKEN=abcdef123").contains("TOKEN=[hidden]"));
        let key = "ghp_ABCDEF0123456789abcdef0123456789abcd";
        assert!(!redact(&format!("use {key} here")).contains(key));
        let found = kinds("I always use the key sk-ABCDEF0123456789abcdef0123456789 for this.");
        assert!(!found[0].1.contains("sk-ABC"), "{found:?}");
    }

    #[test]
    fn a_quoted_secret_with_spaces_is_hidden_whole() {
        for (command, secret) in [
            (r#"deploy --password "correct horse battery staple" --env prod"#, ["horse", "battery"]),
            (r#"deploy --token 'two words here' now"#, ["two", "words"]),
            (r#"PASSWORD="my long pass phrase" ./run.sh"#, ["long", "phrase"]),
            (r#"mysql -p'my secret pass' db"#, ["secret", "my s"]),
            (r#"curl -d '{"password": "two words", "user": "x"}' http://x"#, ["two", "words"]),
            (r#"curl -H "Authorization: Bearer abc123xyz" http://x"#, ["abc123", "xyz"]),
        ] {
            let hidden = redact(command);
            for leaked in secret {
                assert!(!hidden.contains(leaked), "{command} -> {hidden} (leaked {leaked})");
            }
            assert!(hidden.contains("[hidden]"), "{command} -> {hidden}");
        }
    }

    #[test]
    fn ordinary_sentences_with_apostrophes_and_quotes_are_untouched() {
        let text = "I don't think it's basic, and the token budget isn't the problem; run it again";
        assert_eq!(redact(text), text);
        assert_eq!(redact("echo \"hello world\" and move on"), "echo \"hello world\" and move on");
    }

    #[test]
    fn a_corrected_memory_is_read_for_its_facts() {
        let facts = |text: &str| facts_in(text);
        assert_eq!(facts("The user's name is Alex."), vec![("user".to_owned(), "name".to_owned(), "Alex".to_owned(), true)]);
        assert_eq!(facts("My name is Sam")[0].2, "Sam");
        assert_eq!(facts("The user lives in Oslo.")[0].1, "lives_in");
        assert_eq!(facts("The user works at Initech.")[0].2, "Initech");
        assert!(facts("I prefer dark themes.").is_empty());
    }

    fn shell(command: &str, dir: &str) -> Value {
        json!({"command": command, "working_directory": dir})
    }

    #[test]
    fn a_failed_command_is_a_verified_failure_with_its_error_and_a_dead_end() {
        let result = Ok(json!({"success": false, "exit_code": 1, "stderr": "error: linker `link.exe` not found\n", "stdout": ""}));
        let (draft, _) = from_tool("mcp__rhea_shell__run_command", &shell("cargo build --release", "C:/nowhere"), &result).unwrap();
        assert_eq!(draft.memory.kind, "failure");
        assert!(draft.memory.verified);
        assert!(draft.memory.text.contains("cargo build --release") && draft.memory.text.contains("linker `link.exe` not found") && draft.memory.text.contains("exit 1"));
        assert!(draft.dead_end.is_some());
    }

    #[test]
    fn routine_reads_are_not_remembered_but_real_work_is() {
        let ok = Ok(json!({"success": true, "exit_code": 0}));
        assert!(from_tool("mcp__rhea_shell__run_command", &shell("git status", "."), &ok).is_none());
        assert!(from_tool("mcp__rhea_shell__run_command", &shell("ls -la", "."), &ok).is_none());
        let (draft, _) = from_tool("mcp__rhea_shell__run_command", &shell("cargo test", "."), &ok).unwrap();
        assert_eq!(draft.memory.kind, "outcome");
        assert!(from_tool("mcp__rhea_shell__read_file", &json!({"path": "a.txt"}), &Ok(json!({}))).is_none());
        assert!(from_tool("desktop_snapshot", &json!({}), &Err("x".into())).is_none());
        let (written, _) = from_tool("mcp__rhea_shell__write_file", &json!({"path": "C:/x/notes.md"}), &Ok(json!({}))).unwrap();
        assert_eq!(written.memory.text, "Wrote C:/x/notes.md.");
        assert!(from_tool("mcp__rhea_shell__write_file", &json!({"path": "C:/x/notes.md"}), &Err("denied".into())).is_none());
    }

    #[test]
    fn a_turn_leaves_a_summary_and_its_unfinished_business() {
        let drafts = from_turn_end(
            "chat-1",
            "Set up the build script for the release",
            "I added build.ps1 and ran it. The Mac signing step is not yet configured. Next step: add the certificate secrets.",
            &["mcp__rhea_shell__run_command".into(), "mcp__rhea_shell__run_command".into(), "mcp__rhea_shell__write_file".into()],
            Some("rhea"),
        );
        assert_eq!(drafts[0].memory.kind, "summary");
        assert_eq!(drafts[0].memory.scope, "project:rhea");
        assert!(drafts[0].memory.text.contains("run_command x2, write_file"));
        assert!(drafts.iter().filter(|d| d.memory.kind == "task").count() >= 1);
        assert!(from_turn_end("c", "hi", "hello there", &[], None).is_empty(), "chit-chat is not remembered");
    }

    #[test]
    fn a_project_is_the_nearest_folder_with_a_manifest() {
        let root = std::env::temp_dir().join(format!("rhea-proj-{}", std::process::id()));
        let inner = root.join("MyApp").join("src");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("MyApp").join("Cargo.toml"), "").unwrap();
        assert_eq!(project_of(&inner.display().to_string()).as_deref(), Some("myapp"));
        assert_eq!(project_of(&inner.join("main.rs").display().to_string()).as_deref(), Some("myapp"));
        // the temp folder sits under the home folder, which is never a project
        assert_eq!(project_of(&std::env::temp_dir().join("definitely-not-a-project-xyz").display().to_string()), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
