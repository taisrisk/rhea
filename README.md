# JARVUS

JARVUS is a native desktop assistant for Windows, with macOS support in progress. **JEV** is its local executor: it observes app controls, carries out bounded actions, and returns fresh results. The connected ChatGPT model plans the work, reviews those results, and decides what to do next.

> Project status: active development. macOS support is being implemented and is not yet verified end to end. macOS builds are experimental until they pass an interactive runtime check on a Mac.

<p align="center"><img src="assets/jarvus-icon-1024.png" alt="JARVUS icon" width="96"></p>

## Screenshot

![JARVUS main window](docs/screenshot.png)

Chat with tool history on the left, conversation history on the right, model picker and context meter in the composer.

## What it does

- Streams conversations with the model selected from the signed-in account's available catalog.
- Routes local desktop and browser tasks through JEV and returns tool results to the model for follow-up reasoning.
- Uses Agent Desktop accessibility controls for desktop observation and actions; it does not send screenshots to the model by default.
- Provides browser and MCP connections over local stdio, Streamable HTTP, and legacy HTTP+SSE transports.
- Stores chat metadata, message history, and tool traces per conversation under `~/.jarvus/chats/`; settings live in `~/.jarvus/`.
- Keeps authentication credentials in the operating system's credential store rather than the chat files.

Desktop automation is permissioned by the operating system. On macOS, JARVUS requires Agent Desktop's Accessibility permission; screenshot capture also requires Screen Recording permission. Review the permission prompt before enabling it.

## Core skills, Shell and sessions

JEV always follows two built-in skills adapted from [obra/superpowers](https://github.com/obra/superpowers) (MIT): **systematic debugging** (root cause before any fix) and **brainstorming** (understand what the user wants before building). The Shell package's `run_command` runs anything a terminal can (PowerShell, cmd, bash, zsh; up to 15 minutes; `detach` for servers and installers), and a `jev_run_plan` can mix `shell` steps with GUI steps so "install it, open it, click through it" is a single plan. Every chat has a stable session id that keys the ChatGPT prompt cache (the Codex backend does not store conversations server-side, so the full local history is always sent), and the first message also starts a background naming agent that renames the chat with the selected model at its lowest reasoning effort.

## Compose mode, subagents and hooks

**Compose mode** (the toggle next to the context ring) makes JEV investigate first: it can read files, search, run read-only commands and browse, but nothing that changes your machine. It asks questions with `ask_user` (a panel that grows out of the chat box), then proposes a plan you **Accept** or **Revise**. Accepting unlocks every tool and JEV carries the plan out. Bug reports get a debugging section: reproduction, hypotheses, root cause, fix and tests.

![Compose mode plan panel](docs/compose.png)

**Subagents** let the main model act as a manager. `agent_spawn` starts a worker with its own context and model (default: the main model, changeable in Settings); workers use the same tools, message the manager and each other with `agent_send`, and every agent sees a live team status. A finished worker keeps its context and can be given a new task. Each worker gets a generated pixel avatar (a mirrored 5x5 pattern in two complementary colors). The right panel gains a **History | Subagents** switcher; click a worker to read its work and message it.

![Subagent panel](docs/subagents.png)

**Hooks** run your scripts around the agent, in the style of Claude Code and Codex hooks. Configure them in Settings -> Agents & hooks or in `~/.jarvus/hooks.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "mcp__jarvus_shell__*",
        "hooks": [{ "type": "command", "command": "python check.py", "timeout": 10 }] }
    ]
  }
}
```

Events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `SubagentStart`, `SubagentStop`. A hook gets the event as JSON on stdin; exit code 2 blocks (stderr is the reason shown to the model), and JSON on stdout can add `additionalContext`, replace tool input with `updatedInput`, or stop the turn. A hook that fails or times out never blocks the agent.

## Requirements

### Windows development

- Windows 10 version 1809 or newer
- Rust stable toolchain with Cargo
- Node.js 18 or newer
- Agent Desktop installed for the current user:

```powershell
npm install --global agent-desktop
agent-desktop version
```

### macOS support in progress

- macOS 13 or newer (Intel or Apple Silicon)
- Xcode Command Line Tools (`xcode-select --install`)
- Rust stable toolchain with Cargo
- Node.js 18 or newer and Agent Desktop:

```sh
npm install --global agent-desktop
agent-desktop status
agent-desktop permissions --guide
```

Agent Desktop uses macOS Accessibility APIs. Grant Accessibility access to the process that launches JARVUS when prompted. Grant Screen Recording only if using screenshot capture. macOS builds are experimental until a Mac runtime pass verifies launch, permissions, browser connection, and desktop actions.

## Build and run

From the repository root:

```sh
cargo run --locked --bin jarvus
```

Build an optimized Windows executable on Windows:

```powershell
cargo build --release --locked --bin jarvus
```

The executable is `target/release/jarvus.exe`. An experimental native macOS binary can be built on a Mac:

```sh
cargo build --release --locked --bin jarvus
```

GitHub Actions builds macOS x64 and Apple Silicon artifacts on macOS runners. The workflow is a build pipeline; successful compilation does not replace interactive macOS accessibility and permission verification.

## Sign in

Use **Sign in with ChatGPT** in JARVUS. The app starts the Codex device authorization flow, opens the official device page in the default browser, and displays the one-time code to enter there. Tokens are stored with the operating system credential service. JARVUS sends prompts and the tool results required for the answer to the connected ChatGPT service. Local screenshots, full accessibility trees, and unrelated local files stay on the PC unless the user shares their contents.

## Data and privacy

JARVUS keeps application data in the current user's `.jarvus` directory:

```text
~/.jarvus/
  settings.json
  mcp-servers.json
  chats/
    <chat-id>/
      metadata.json
      history.json
      toolcalls.json
```

On Windows, `~` is the user's profile directory, for example `C:\Users\Alice\.jarvus`. The app migrates legacy Windows conversation and settings files when they are found. Credential values use the operating system credential store. Do not attach `~/.jarvus` to bug reports; it may contain private chat content or connection configuration.

Tool calls are visible in the conversation. A successful sequence of local steps is not, by itself, proof that a longer user goal is complete: research and multi-step work must be checked against the requested outcome before JEV reports completion. Sending messages and other consequential actions should only occur when the user's request clearly authorizes them.

## MCP servers

Open **Settings → MCP servers** to import or manage servers. JARVUS supports local stdio, Streamable HTTP, and legacy HTTP+SSE MCP transports. Use HTTPS for remote servers and keep bearer tokens in the app's credential storage. Never paste secrets into screenshots or public issue reports.

The built-in Chrome extension is a **browser bridge**, not an MCP server. The built-in **Shell** connection supplies bounded local command, file, directory, and HTTP(S) fetch tools. When asked what is connected, JEV queries live inventory and reports configured MCP servers, browser bridges, and local connections as separate groups.

## GitHub builds and releases

The macOS workflow runs on pushes, pull requests, and manual dispatch. It creates separate macOS x64 and Apple Silicon executable archives; a published GitHub Release also receives the matching archives. These are unsigned executable archives, not a signed or notarized `.app` installer.

Build and stage the Windows executable locally:

```powershell
.\scripts\release.ps1 -Bump patch
```

After reviewing, committing, and tagging that version, upload the Windows executable to an existing GitHub Release:

```powershell
.\scripts\release.ps1 -Upload
```

The upload option requires GitHub CLI authentication, a configured `origin`, a matching version tag, and an existing release. It never pushes source changes. A repository remote must be configured before uploads can work.

## Project map

| Path | Purpose |
| --- | --- |
| `src/app.rs` | Native workspace, settings, chat history and tool activity UI |
| `src/chat.rs` | Model catalog, streamed responses and tool-call loop |
| `src/jev.rs` | Local tool routing and bounded desktop plans |
| `src/desktop.rs` | Agent Desktop process adapter, snapshots and actions |
| `src/mcp.rs` | MCP configuration, server connections and tool dispatch |
| `src/browser.rs` | Chrome extension bridge |
| `src/chat_store.rs` | Per-chat files, migration and persistence helpers |
| `src/system_prompt.md` | JEV operating rules and workflow examples |
| `src/toolset.json` | Model-facing function schemas |
| `chrome-extension/` | Browser extension and native messaging manifest |
| `JARVUS VAULT/` | Obsidian project notes |
| `.planning/` | Requirements, roadmap, debug notes and project state |

## Current limits

- macOS releases need an interactive Mac verification pass for permissions, app launch, browser connection, and desktop actions.
- Agent Desktop can only act on controls its accessibility provider exposes. When a target is missing or ambiguous, JEV should stop and report the limitation instead of guessing.
- A successful build does not prove that a ChatGPT account, MCP server, browser extension, or third-party desktop action works end to end.

## Skills, slash commands and context

**Skills** are folders with a `SKILL.md`. JARVUS lists the ones in `~/.jarvus/skills`, `~/.agents/skills` and `~/.claude/skills` to the model on every run; the model loads one with `use_skill` when a request matches, and `install_skill` (or Settings → Agents, skills & hooks) installs one from a GitHub link. Type `/` in the chat box for the command menu: `/compact`, `/compose`, `/new`, every installed skill (`/skill-name` loads it for that message) and every MCP server. Hover an entry to see what it does.

**Context.** Each chat keeps its own gauge, latest request usage, cache totals and cumulative totals, and they survive new messages and restarts. A request that would fill more than the **auto-compact** share of the window (Settings → Chat & model, 15–95%, default 80%) summarizes older turns first; `/compact` does it on demand and the transcript shows where. Inside a subagent's chat the context panel shows that agent's own context.

**Team work.** The manager keeps working while its subagents run, receives each worker's full report as a message, and a turn does not end while workers are still running.
