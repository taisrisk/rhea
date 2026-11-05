//! Read-only guard for Compose Mode. While compose mode is on the model may only
//! investigate (read files, search, run non-mutating commands, browse). Anything that
//! changes the machine is refused until the user accepts the proposed plan.

use serde_json::Value;

const READ_ONLY_TOOLS: &[&str] = &[
    "list_processes",
    "desktop_list_apps",
    "desktop_list_windows",
    "desktop_snapshot",
    "desktop_find",
    "desktop_get",
    "desktop_screenshot",
    "list_mcps",
    "get_tools_",
    "use_skill",
    "ask_user",
    "propose_plan",
    "agent_spawn",
    "agent_send",
    "agent_list",
    "agent_read",
    "agent_wait",
    "agent_stop",
    "mcp__jarvus_shell__list_directory",
    "mcp__jarvus_shell__read_file",
    "mcp__jarvus_shell__web_fetch",
    "browser__tabs",
    "browser__snapshot",
    "browser__extract",
    "browser__open",
    "browser__navigate",
    "browser__back",
    "browser__scroll",
    "browser__wait",
    "browser__activate",
];

const RUN_COMMAND: &str = "mcp__jarvus_shell__run_command";
const MAX_COMMAND_CHARS: usize = 20_000;

/// Ok if `name` may be used in compose mode with these arguments.
pub fn tool_allowed(name: &str, arguments: &Value) -> Result<(), String> {
    if READ_ONLY_TOOLS.contains(&name) {
        return Ok(());
    }
    if name == RUN_COMMAND {
        if arguments.get("detach").and_then(Value::as_bool) == Some(true) {
            return Err("Compose mode cannot start background processes. Run it after the user accepts your plan.".into());
        }
        if arguments.get("shell").and_then(Value::as_str).is_some_and(|shell| !matches!(shell, "" | "auto" | "powershell")) {
            return Err("Compose mode only checks default-shell commands; use shell \"auto\" or \"powershell\" for read-only commands.".into());
        }
        let command = arguments.get("command").and_then(Value::as_str).unwrap_or("");
        return command_is_read_only(command).map_err(|reason| {
            format!("Compose mode only allows read-only commands: {reason}. Run it after the user accepts your plan.")
        });
    }
    Err(format!(
        "`{name}` is unavailable in Compose mode because it can change things. Investigate with read-only tools, then call propose_plan; it unlocks after the user accepts."
    ))
}

/// The tool schemas that may be advertised while composing.
pub fn filter_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name == RUN_COMMAND || READ_ONLY_TOOLS.contains(&name))
        })
        .cloned()
        .collect()
}

/// Conservative classifier: every pipeline segment must be an allow-listed read-only command.
pub fn command_is_read_only(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("the command is empty".into());
    }
    if command.chars().count() > MAX_COMMAND_CHARS {
        return Err("the command is too long to verify".into());
    }
    let segments = split_segments(command)?;
    if segments.is_empty() {
        return Err("the command is empty".into());
    }
    for segment in segments {
        segment_is_read_only(&segment)?;
    }
    Ok(())
}

/// Splits on | || && ; and newlines outside quotes, rejecting constructs that can hide
/// writes or code execution anywhere in the string.
fn split_segments(command: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                quote = None;
            }
            // Command substitution is live inside double quotes.
            if open == '"' && (ch == '`' || (ch == '$' && chars.get(index + 1) == Some(&'('))) {
                return Err("command substitution is not allowed".into());
            }
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                current.push(ch);
            }
            '`' => return Err("backticks are not allowed".into()),
            '$' if chars.get(index + 1) == Some(&'(') => return Err("command substitution is not allowed".into()),
            '>' => return Err("output redirection writes files".into()),
            '<' if chars.get(index + 1) == Some(&'(') => return Err("process substitution is not allowed".into()),
            '|' | ';' | '\n' | '\r' => {
                if chars.get(index + 1) == Some(&'|') && ch == '|' {
                    index += 1;
                }
                segments.push(std::mem::take(&mut current));
            }
            '&' => {
                if chars.get(index + 1) == Some(&'&') {
                    index += 1;
                    segments.push(std::mem::take(&mut current));
                } else {
                    return Err("`&` (background or call operator) is not allowed".into());
                }
            }
            '{' | '}' if current.trim().is_empty() => return Err("script blocks are not allowed".into()),
            _ => current.push(ch),
        }
        index += 1;
    }
    if quote.is_some() {
        return Err("unbalanced quotes".into());
    }
    segments.push(current);
    Ok(segments.into_iter().map(|part| part.trim().to_owned()).filter(|part| !part.is_empty()).collect())
}

fn tokens(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for ch in segment.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                has_token = true;
            }
            None if ch.is_whitespace() => {
                if has_token || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => current.push(ch),
        }
    }
    if has_token || !current.is_empty() {
        out.push(current);
    }
    out
}

fn segment_is_read_only(segment: &str) -> Result<(), String> {
    let parts = tokens(segment);
    let Some(first) = parts.first() else { return Err("the command is empty".into()) };
    let mut program = first.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stripped) = program.strip_suffix(suffix) {
            program = stripped.to_owned();
        }
    }
    if let Some(base) = program.rsplit(['\\', '/']).next() {
        program = base.to_owned();
    }
    let args: Vec<String> = parts[1..].iter().map(|arg| arg.to_ascii_lowercase()).collect();
    let lower = segment.to_ascii_lowercase();
    for banned in [
        "out-file", "set-content", "add-content", "tee-object", "invoke-expression", "remove-item", "move-item", "copy-item",
        "new-item", "rename-item", "start-process", "stop-process", "set-item", "clear-content", "-outfile", "-o ",
    ] {
        if lower.contains(banned) && !(banned == "-o " && matches!(program.as_str(), "rg" | "grep" | "findstr" | "git")) {
            return Err(format!("`{}` writes or runs code", banned.trim()));
        }
    }
    let plain: &[&str] = &[
        "ls", "dir", "gci", "get-childitem", "cat", "type", "get-content", "gc", "head", "tail", "rg", "grep", "findstr",
        "select-string", "sls", "where", "which", "get-command", "pwd", "get-location", "echo", "write-output", "tree", "wc",
        "measure-object", "sort", "sort-object", "select-object", "select", "where-object", "format-table", "format-list",
        "ft", "fl", "out-string", "get-item", "get-itemproperty", "test-path", "resolve-path", "split-path", "get-process",
        "get-service", "get-date", "get-filehash", "hostname", "whoami", "uname", "date", "printenv", "ver", "get-childitem",
    ];
    if plain.contains(&program.as_str()) {
        if program == "sort" && args.iter().any(|arg| arg.starts_with("/o") || arg == "-o" || arg.starts_with("--output")) {
            return Err("`sort -o` writes a file".into());
        }
        return Ok(());
    }
    let sub = args.iter().find(|arg| !arg.starts_with('-')).map(String::as_str).unwrap_or("");
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    match program.as_str() {
        "foreach-object" | "%" | "foreach" => {
            Err("foreach-object runs arbitrary script blocks".into())
        }
        "env" if args.is_empty() => Ok(()),
        "find" => {
            if args.iter().any(|arg| matches!(arg.as_str(), "-delete" | "-exec" | "-ok" | "-execdir" | "-fprint" | "-fprintf" | "-fls")) {
                Err("`find` with -delete/-exec/-fprint changes things".into())
            } else {
                Ok(())
            }
        }
        "git" => match sub {
            "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" | "describe" | "blame" | "shortlog" | "grep" => {
                if has("--output") || args.iter().any(|arg| arg.starts_with("--output=")) {
                    Err("`git --output` writes a file".into())
                } else {
                    Ok(())
                }
            }
            "branch" => {
                let rest: Vec<&String> = args.iter().skip_while(|arg| arg.starts_with('-')).skip(1).chain(args.iter().take_while(|arg| arg.starts_with('-'))).collect();
                let flag_writes = rest.iter().any(|arg| {
                    matches!(arg.as_str(), "-d" | "-m" | "-c" | "--delete" | "--move" | "--copy" | "-f" | "--force" | "--set-upstream-to" | "-u" | "--unset-upstream")
                });
                let listing = ["--list", "-l", "-a", "-r", "-v", "-vv", "--show-current", "--contains", "--merged", "--no-merged", "--all", "--remotes"]
                    .iter().any(|flag| has(flag));
                let positional = rest.iter().any(|arg| !arg.starts_with('-'));
                if flag_writes || (positional && !listing) { Err("`git branch` with a name or -d/-m changes branches".into()) } else { Ok(()) }
            }
            "remote" => {
                let positional = args.iter().skip_while(|arg| arg.starts_with('-')).skip(1).any(|arg| !arg.starts_with('-'));
                if positional { Err("`git remote` may only be listed".into()) } else { Ok(()) }
            }
            "config" => {
                if has("--get") || has("--get-all") || has("--list") || has("-l") {
                    Ok(())
                } else {
                    Err("`git config` may only read values".into())
                }
            }
            "" => Err("`git` needs a read-only subcommand".into()),
            other => Err(format!("`git {other}` is not on the read-only list")),
        },
        "cargo" => match sub {
            "check" | "metadata" | "tree" | "search" | "version" | "test" | "clippy" => Ok(()),
            "" if has("--version") || has("-v") || has("-V") => Ok(()),
            other => Err(format!("`cargo {other}` is not on the read-only list")),
        },
        "rustc" | "node" | "dotnet" if has("--version") || has("-v") || has("-version") || has("--info") || has("--list-sdks") => Ok(()),
        "npm" => match sub {
            "ls" | "list" | "view" | "outdated" => Ok(()),
            "" if has("--version") || has("-v") => Ok(()),
            other => Err(format!("`npm {other}` is not on the read-only list")),
        },
        "python" | "python3" | "py" if args.len() == 1 && (has("--version") || has("-v")) => Ok(()),
        "pip" | "pip3" => match sub {
            "list" | "show" | "freeze" => Ok(()),
            "" if has("--version") => Ok(()),
            other => Err(format!("`pip {other}` is not on the read-only list")),
        },
        "winget" => match sub {
            "list" | "search" | "show" => Ok(()),
            other => Err(format!("`winget {other}` is not on the read-only list")),
        },
        "choco" | "scoop" => match sub {
            "list" | "search" | "info" | "show" => Ok(()),
            other => Err(format!("`{program} {other}` is not on the read-only list")),
        },
        other => Err(format!("`{other}` is not on the read-only list")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(command: &str) {
        assert!(command_is_read_only(command).is_ok(), "should allow: {command} -> {:?}", command_is_read_only(command));
    }
    fn no(command: &str) {
        assert!(command_is_read_only(command).is_err(), "should block: {command}");
    }

    #[test]
    fn allows_common_read_only_commands() {
        for command in [
            "ls", "dir /b", "Get-ChildItem -Recurse src", "cat README.md", "rg \"fn main\" src", "rg \"a;b\" src", "git status",
            "git log --oneline | head -5", "git diff HEAD~1", "git branch", "git branch -a", "git branch --show-current", "git remote -v",
            "git config --get user.name", "cargo check", "cargo test", "cargo metadata", "node --version", "python --version",
            "npm ls", "winget search python", "pip list", "Get-Content a.txt | Select-String foo", "echo hello && pwd", "DIR.EXE",
            "C:\\Windows\\System32\\whoami.exe", "findstr /s foo *.rs", "find . -name '*.rs'", "git show HEAD",
        ] {
            ok(command);
        }
    }

    #[test]
    fn blocks_mutating_and_unknown_commands() {
        for command in [
            "ls; rm -rf x", "cat a > b", "cat a >> b", "git commit -m x", "git push", "git branch -D x", "git branch newname", "git checkout main",
            "git config user.name x", "git remote add o url", "Get-Content a | Set-Content b", "echo $(rm x)", "echo `whoami`", "find . -delete",
            "find . -exec rm {} ;", "cargo build", "cargo install foo", "npm install", "pip install x", "winget install x", "python -c \"print(1)\"",
            "python script.py", "node app.js", "rm file", "del file", "mv a b", "Remove-Item x", "Out-File x", "ls & calc", "ls | ForEach-Object { rm $_ }",
            "curl -o x http://a", "Invoke-Expression 'ls'", "start notepad", "powershell -c ls", "cmd /c dir", "sort -o out.txt in.txt", "tee out.txt",
            "git diff --output=x.patch", "echo a; Start-Process calc",
        ] {
            no(command);
        }
    }

    #[test]
    fn handles_edge_inputs() {
        no("");
        no("   ");
        no("echo 'unterminated");
        no("echo \"unterminated");
        ok("echo 'a > b'"); // the redirect is quoted text
        ok("echo héllo wörld ✓");
        let long = format!("echo {}", "a".repeat(100_000));
        let started = std::time::Instant::now();
        no(&long);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let many = std::iter::repeat("ls").take(2_000).collect::<Vec<_>>().join(" | ");
        let started = std::time::Instant::now();
        let _ = command_is_read_only(&many);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn tool_matrix() {
        for name in ["list_mcps", "get_tools_", "ask_user", "propose_plan", "agent_spawn", "mcp__jarvus_shell__read_file", "browser__extract", "desktop_snapshot"] {
            assert!(tool_allowed(name, &json!({})).is_ok(), "{name}");
        }
        for name in [
            "jev_run_plan", "open_app", "desktop_click", "desktop_type", "desktop_send", "desktop_press", "desktop_launch", "mcp_add_servers",
            "mcp__jarvus_shell__write_file", "mcp__jarvus_shell__edit_file", "mcp__jarvus_shell__create_directory", "browser__click", "browser__type",
            "mcp__potassium__execute_script", "totally_unknown",
        ] {
            let error = tool_allowed(name, &json!({})).unwrap_err();
            assert!(error.contains("Compose"), "{name}: {error}");
        }
        assert!(tool_allowed("mcp__jarvus_shell__run_command", &json!({"command":"git status"})).is_ok());
        assert!(tool_allowed("mcp__jarvus_shell__run_command", &json!({"command":"rm -rf x"})).is_err());
        assert!(tool_allowed("mcp__jarvus_shell__run_command", &json!({})).is_err());
    }

    #[test]
    fn filter_keeps_only_allowed_schemas() {
        let tools = vec![
            json!({"name":"list_mcps"}),
            json!({"name":"jev_run_plan"}),
            json!({"name":"mcp__jarvus_shell__run_command"}),
            json!({"name":"mcp__jarvus_shell__write_file"}),
            json!({"name":"browser__click"}),
            json!({"name":"browser__extract"}),
            json!({"no_name":true}),
        ];
        let names: Vec<_> = filter_tools(&tools).iter().filter_map(|tool| tool["name"].as_str().map(str::to_owned)).collect();
        assert_eq!(names, vec!["list_mcps", "mcp__jarvus_shell__run_command", "browser__extract"]);
    }
}
