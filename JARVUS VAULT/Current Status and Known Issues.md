---
title: Current Status and Known Issues
type: status
status: active
tags: [jarvus, status, debugging]
---

# Current Status and Known Issues

## 2026-11-04 integration pass (multi-step work, MCP, storage, macOS, release prep)

- **Supervised multi-step tasks:** research and multi-step local plans return to ChatGPT for inspection and follow-up. A plan announcement, a single observation, or a JEV `completed` status cannot finish a task with measurable criteria. JEV supplies bounded evidence; ChatGPT continues until criteria are verified or reports the missing evidence. The chat tests cover this gate and threshold checks.
- **Per-chat persistence:** `~/.jarvus/chats/<id>/` contains `metadata.json`, `history.json`, and `toolcalls.json`. Startup loads chat metadata for the history rail, then reads a transcript only when opened. An inactive transcript is released from memory after five seconds. Existing Windows conversation/settings files are migrated by source code; this pass did not migrate or rewrite the live user's data.
- **MCP transports:** local stdio, Streamable HTTP, and legacy HTTP+SSE are implemented. Config add/edit/reload connects enabled servers immediately; credentials remain in the OS credential store and URL query/fragment data is stripped from the Settings label. Mock-server tests cover both HTTP transports; no live remote server was contacted.
- **macOS source support:** JEV uses the Agent Desktop macOS accessibility backend and its documented launch/focus/pointer commands; Chrome native-host registration/data use `~/.jarvus`. Settings exposes Agent Desktop permission status, request, and guide actions. The default keyring backend maps to macOS Keychain. This Windows checkout cannot prove a native Mac build or TCC permission/runtime flow.
- **Release preparation:** `.github/workflows/macos.yml` builds x64 and Apple Silicon artifacts and attaches them to published releases; `scripts/release.ps1` bumps the Cargo package/lock version and stages/uploads a Windows build to an existing tagged release. `.gitignore` covers build/local state. This checkout has no Git remote, so no push or release upload was attempted.
- **Latest validation:** `cargo fmt --check` passed; `cargo test --locked --bin jarvus` passed **86 tests, 0 failed, 4 ignored**; `cargo check --locked --bin jarvus` passed; `git diff --check` passed. A fresh optimized Windows binary built to the ignored `target-final/release/jarvus.exe` because the standard release executable was locked by the already-running app.
- **Visual/runtime status:** screenshot capture timed out twice; the second Computer Use attempt was stopped by the user with Escape. No fresh screenshot is available, so rendered layout, no-console launch, app-level Mac permissions, and cross-platform UI behavior remain unverified. README screenshot stays pending.

## 2026-11-04 evening pass (speed, generic control, UI overhaul, MCP, Chrome)

- **Planner/executor (earlier implementation):** ChatGPT originally sent one `jev_run_plan` call. The current multi-step and research supervision rules are recorded in the integration-pass section above; do not treat this earlier behavior as current.
- **Native control (`src/uia.rs`):** Win32 window focus (fuzzy by exe name or title, steals focus back from overlays), one-request UI Automation tree read (~1,700 nodes in ~120 ms vs ~3 s via agent-desktop), focused-field reads (~20 ms), SendInput typing, generic key combos, smooth mouse move (1000 px/250 ms), click/double/right, drag, hold, wheel.
- **Fuzzy targeting:** labels compared as words (case/symbols/emoji ignored, all query words required, fewer extra words win, packed names scored per part); off-screen items ignored; plain text labels map to the clickable element of their row; optional `within` area hint supplied by ChatGPT. No app-specific rules.
- **Messaging:** quick-switcher path waits for the typed name to land, then ~250 ms for results; recipient match is whole-word; draft present, focus stolen or unconfirmed typing all stop before Return.
- **Measured live:** Discord focus 7-39 ms, composer focus 125-315 ms, open_app 0-35 ms when running. A cold Chrome launch was not measured.
- **UI:** black/white palette with a faint green accent; smooth radial glows (no bands); layered panel shadows; segmented sliding tabs; ChatGPT status dot and JEV-only ms chip; centered window controls; model popup with expandable list and stepped thinking slider; context ring panel at 45% under History; history previews with hover actions; markdown single-newline fix; selectable text only in bubbles and results (Ctrl+C verified); green rounded scrollbars; new transparent app icon.
- **Settings:** redesigned (centered header, icon nav, grouped cards). Storage shows a usage bar and legend plus a Review old chats dialog. MCP: paste a standard `{"mcpServers":{...}}` config (also VS Code `servers`), merged without touching other servers, health dots, tool lists, per-server config editor with masked secrets. Chat tools `mcp_add_servers` / `mcp_list_servers`.
- **Chrome bridge v0.2:** pinned extension ID `eplppddfejfcbdllfbooenaplkobcpkk`, native host auto-registered on every JARVUS start, auto-connect plus reconnect, all tabs and windows, fuzzy DOM targeting, click/type (React and rich editors)/select (native and custom)/press/scroll/wait/extract. Verified in headless Chrome on a local test page. **Requires one manual Load unpacked in chrome://extensions.**
- **Known:** the top 1 px window line fix (DWM non-client rendering disabled) is built but not yet visually confirmed. The computer-use overlay covers the screen's top-right corner, so title-bar clicks there can't be tested with it.

Updated 2026-11-04 after the dropped-send and failed-observation fixes.

## Version

Source package version: `0.1.0` (Cargo metadata; not a signed release version).

## Implemented in source

- Chat history and selected model persist locally. The selected model is restored when the app starts.
- Chat message text is selectable and copy actions use egui's native clipboard integration.
- Enter sends a chat message; Shift+Enter adds a line break. The caret uses the green accent.
- The transcript reserves stable message/action rows across streaming completion. Tool activity can expand to show steps, durations, arguments, and results; failed tool calls open with selectable error details.
- The context drawer is below history in the right rail, height-constrained and scrollable. The model selector sits beside the context meter in the composer.
- Window edge/corner resize drag regions, custom titlebar controls, DWM corner preference, DWM border-color suppression, and removal of native `WS_BORDER`/`WS_DLGFRAME` while preserving `WS_THICKFRAME` are implemented.
- Main-panel corner lighting uses a radial fade and the panel has a green-tinted shadow. The old bottom-right connection footer and redundant JEV chat heading are removed.
- ChatGPT sign-in uses the Codex device-code flow at `https://auth.openai.com/codex/device`; session data is stored through Windows Credential Manager.
- JEV checks focus before typing, requires current snapshot refs, stops on stale refs, and refreshes after local actions. Snapshot output defaults to all useful elements and supports optional local category filtering.
- Sends are blocked while another background operation is pending, preventing a blank assistant turn when model/startup work occupies the worker slot. Failed, incomplete, or unretained inline desktop observations no longer count as a successful refresh.
- JEV local-action timing starts from the tool-start event and is finalized at request completion if no terminal activity event arrived.
- Main-panel green bloom now paints beyond the card edge with a broader radial fade; the surrounding glow is clipped to a small shell-safe region. The panel shadow has been strengthened in source.
- MCP stdio and remote HTTP/SSE server definitions can be added, edited, enabled, disabled, tested, removed, and reloaded. Environment secrets are stored through the OS credential service and omitted from the config JSON.
- Settings expose ChatGPT, local bridge, MCP, Chrome relay, conversation history, and top-level Downloads controls.
- The Windows release executable is built with the GUI subsystem; JEV also starts Agent Desktop with `CREATE_NO_WINDOW` while capturing its output.

## Verified in this pass

- `cargo test --bin jarvus`: **39/39 passed**, including pending-send gating and failed/incomplete snapshot regressions.
- `cargo build --release --bin jarvus`: passed after gracefully closing the test instance. The final updated release is running from the standard `target/release/jarvus.exe` path.
- `git diff --check`: passed.
- `src/toolset.json` parsed successfully and contains 18 tools.
- `chrome-extension/manifest.json` parsed; the extension scripts passed `node --check` in the browser/MCP implementation pass.
- The release PE subsystem is GUI (`2`). This verifies the executable type, not whether any external child process can create a console.
- Live accessibility inspection found the titlebar, chat input, model picker, context, history actions, expandable tool activity, and eight resize handles. This is accessibility-tree evidence only.
- Fresh Agent Desktop Chat → Computer → Chat clicks were delivered, and a new complete tree after returning still exposed the composer and saved GPT-6-Luna selection. This confirms control presence/navigation semantics, not rendered layout.
- The updated standard release launched at 1440×920; GPT-6-Luna persisted across restart. The temporary isolated build was only used while the normal executable was locked.
- After the native border-style cleanup, Agent Desktop resized the rebuilt standard release to 1120×760 and back to 1440×920 with verified window-size results. This still does not prove pointer edge-drag behavior.
- A smoke prompt was not sent: Agent Desktop returned `AMBIGUOUS_TARGET` for the duplicate “New chat” controls. No existing private conversation was modified.
- Screenshot capture timed out twice in the fresh window state. No pixel-level visual check or input/resize/copy interaction test was completed.
- A harmless request in the already-empty chat completed through the saved ChatGPT session and JEV's local running-app count tool. The transcript showed a completed assistant response and `Worked for 7s`; the process list content was not inspected. No third-party desktop action was sent.

## Open issues and acceptance checks

1. **Top hairline:** not visually resolved. Source uses a fill-only shell/titlebar and requests DWM `COLOR_NONE`; without a fresh screenshot it is not known whether the line is from DWM or a client boundary.
2. **Live layout behavior:** verify user/assistant bubble positions and action rows after a completed response. The semantic tab flow passed, but no pixel proof is available.
3. **Window behavior:** verify edge/corner drag resizing and restored/maximized corner changes. The supported Agent Desktop resize command was verified at 1120×760 and restored to 1440×920; this does not verify edge dragging.
4. **Copy and keyboard:** verify Ctrl+C selection, copy-message controls, Enter send, and Shift+Enter newline in the rebuilt window.
5. **Live AI:** the saved session returned a visible response and completed the local process-count tool loop. Device-code reauthentication itself was not repeated; unit tests cover empty/tool-only response parsing, but not all provider event shapes for this account.
6. **Desktop action:** verify a harmless, user-authorized local app launch and a fresh Agent Desktop snapshot/action/snapshot cycle. Do not send social messages during a smoke test.
7. **Chrome:** extension/native-host source exists, but installation, host registration, browser permission grant, and active-tab interaction have not been verified. JARVUS cannot enumerate per-site Chrome host grants.
8. **MCP:** stdio, Streamable HTTP, and legacy HTTP+SSE are implemented and mock-tested. A real remote service connection has not been verified.
9. **Storage scope:** each chat has separate metadata/history/tool-call files and inactive transcripts unload after five seconds. Settings inventories conversation history and only the top level of Downloads. No project/file connector or owned generated-artifact catalog is available, so JARVUS cannot list or safely remove all project/generated files.
10. **Console process:** JARVUS is configured for the Windows GUI subsystem and Agent Desktop uses a no-console child-process flag. Fresh pixel verification remains unavailable; Agent Desktop remains a separate process.
11. **macOS:** source paths, browser-host registration, Keychain use, and Agent Desktop permission actions are implemented. The macOS GitHub Actions build and a real Mac permission/accessibility run have not been executed from this Windows checkout.
12. **GitHub publication:** no Git remote is configured; the release workflow and Windows staging script are present, but nothing has been pushed or uploaded.

## Evidence rule

Source edits, unit tests, GUI subsystem metadata, and accessibility trees are separate evidence types. Do not call them a successful visual, OAuth, browser, or desktop-action end-to-end test. Record live proof only after a fresh app window or authorized action produces observable evidence. Never write tokens or one-time device codes to this vault.

See [[Implementation - ChatGPT and JEV]], [[Runtime Integrations and Setup]], and [[Roadmap]].

## 2026-11-05 Claude pass
- Core tools now: JEV, Shell (`mcp__jarvus_shell__*`), `list_mcps` (live status: connected/disconnected/unauthorized/error/disabled) and `get_tools_{server}`. Chrome (`browser__*`) and MCP server tools load on demand: `get_tools_` returns `load_tools` schemas which `chat.rs` appends to the live tool list.
- Tool rows show what they did (Reading file, Opening Discord, Running cmd) via `tool_title` in `app.rs`.
- Stop button: send button becomes a spinning ring, hover shows a stop square; `jev::request_cancel` bumps a generation counter checked by the chat loop and `run_plan`.
- JEV ms chip is live while an action runs. Persona: system prompt section 0 names the assistant JEV.
- Phosphor icon font (`egui-pop-phosphor`) replaces hand-drawn icons; primary buttons are deep green with white text; history row hover covers the whole row so rename/delete stay clickable.
- New app icon (`assets/jarvus-mark.png`, `jarvus.ico` embedded via `build.rs`/winresource).
- Not verified: computer-use access to the app was denied, so only a PrintWindow capture (`docs/screenshot.png`) was checked; stop-button hover, `get_tools_` against a live model and the mac build are untested.

### 2026-11-05 live verification (Claude, release build in `target-claude`)
- Verified live: persona answers "I'm JEV"; Shell file listing in 6 s; multi-step Chrome research loop with `Checking connections` / `Loading chrome tools` / `Opening … in Chrome` rows; stop button (hover shows stop square, click ends the run with "Stopped."); history hover keeps rename/delete clickable; Potassium (Streamable HTTP) connects with 5 tools; no top hairline.
- MCP servers now auto-connect at start and retry every 20 s off the manager lock (`McpManager::disconnected_enabled` / `connect_detached` / `install_connection`).
- Live header text and tool rows come from `tool_label` (args-aware); stray `…` and wide bubble fixed.
- Still open: ChatGPT research quality (the Roblox player-count task looped on search pages; prompt now has research discipline), macOS runtime check on a real Mac, native macOS AX API (macOS currently goes through Agent Desktop, which wraps AX).

## 2026-11-05 Compose mode, ask_user, subagents, hooks (Claude pass 2)
- **New modules:** `src/hooks.rs` (hook registry, runs commands with JSON on stdin, exit 2 = block), `src/compose.rs` (read-only command classifier + compose-mode tool allowlist), `src/team.rs` (agent hub: spawn/send/wait/stop/resume, inboxes, team status, checkpoints saved to `~/.jarvus/agents/<chat>.json`, plus the blocking `ask_user` / `propose_plan` interactions), `src/tools.rs` (the single tool dispatcher used by the main chat and every subagent; wraps calls with PreToolUse/PostToolUse hooks), `src/app/team_ui.rs` (question/plan panel, History|Subagents tabs, subagent chat view, Agents & hooks settings).
- **chat.rs:** `respond_with_options` + `RunOptions {compose, extra_developer, hidden_tools, inbox, worker, on_stop}`. Compose mode filters tools per request, gates each call with `compose::tool_allowed`, and leaves compose when `propose_plan` returns `accepted`. The research evidence gate no longer fires on local/code requests or on worker runs.
- **Prompts:** `system_prompt.md` rewritten (about 450 lines), plus `compose_prompt.md` (plan-mode and debug workflow) and `subagent_prompt.md` (worker role).
- **UI:** every transition is 150 ms max (`ANIM`); question/plan panel grows out of the composer; compose toggle left of the context ring.
- **Verified live (release build):** ask_user options and custom answer; plan Accept and Revise then Accept (executed and verified a tiny project); subagents run in parallel, report to main, view and message from the Subagents tab; a PreToolUse hook blocked `write_file` and the model reported the reason.
- **Not verified live:** compose-mode read-only block against a real model (unit-tested), stop button while a question is open (unit-tested wait/cancel), agent persistence across an app restart, agent-to-agent messaging between two workers, macOS.

## 2026-11-05 pass 3 (prompt, skills, shell, window frame, sessions)
- **System prompt:** `src/system_prompt.md` is now about 1,130 lines: per-tool reference with examples, Shell cookbook, Shell/JEV/Chrome/MCP/subagent hand-off patterns (including the `shell` plan step), browser cookbook, error-recovery table, coding standards, final-answer examples. Core skills `src/skills/systematic_debugging.md` and `src/skills/brainstorming.md` (adapted from obra/superpowers, Visual Companion removed) are concatenated onto it at compile time (`SYSTEM_PROMPT` in `chat.rs`).
- **Shell:** `run_command` takes `shell` (auto/powershell/cmd/bash/sh/zsh), `detach` and `timeout_ms` up to 900000; kills the whole process tree on timeout or cancel; forces UTF-8 output. Compose mode only allows the default shell and never `detach`. `jev_run_plan` has a `shell` step so command line and GUI work share one plan.
- **Window frame:** `native_frame` in `app.rs` subclasses the window (comctl32 chain, same as winit) and swallows `WM_NCACTIVATE` repaints, `WM_NCPAINT`, and uses the whole window as client area, which removes the top line and the classic caption buttons that flashed on focus changes. Verified by capturing frames during focus toggles: old build showed a classic title bar, new build showed none. Test harness note: a maximized Claude window on top swallows synthetic clicks; make JARVUS TOPMOST first.
- **Context gauge:** clears at once on a chat switch (zero for an empty chat, an estimate from its own turns otherwise) and every number/arc eases to its new value.
- **Sessions:** the ChatGPT Codex backend rejects `store: true` ("Store must be set to false"), so server-side continuation by response id is impossible. Instead each chat sends a stable `session_id` header and `prompt_cache_key` (`jarvus-<chat id>`), so resumed chats reuse the cached prefix; the full history stays local.
- **Title agent:** first message starts a background request with `src/session_title_prompt.md`, the user's selected model at its lowest advertised effort; the history row morphs old name to new name in 150 ms.
- **Avatars:** generated 5x5 mirrored pixel avatars, two complementary colors, rounded 10 px, deterministic from the agent id and name (microseconds).

### 2026-11-05 continuation — tool rounds, chat context, and worker routing

- Removed the explicit 48-tool-round stop. The model/JEV loop continues until completion, cancellation, a tool/provider error, or a per-request timeout. The 48-item array truncation was also removed; a regression test confirms row 301 (`525i`) remains available to the model. Very large single results still have bounded node/byte budgets.
- Context meter state is per conversation: effective context estimate, latest reported request usage, and cumulative reported usage persist in chat metadata. New chats clear the meter immediately; switching back restores that chat's state. System-prompt compaction requests reuse the chat's stable cache-affinity key.
- The stable `jarvus-<chat id>` key is locally generated cache affinity, not an API-issued conversation ID. The OAuth-backed private endpoint rejects server-side response storage, so JARVUS continues from its locally persisted transcript rather than claiming ChatGPT-hosted history. The public Conversations API is a separate integration.
- Subagent inboxes are isolated by chat and recipient. Workers can route messages to the main agent and each other within the same chat; a regression test covers cross-chat isolation.
- The system prompt composes the core JARVUS instructions, adapted systematic-debugging and brainstorming skills, and a new workflow examples file for desktop, shell, browser, MCP, Compose, and subagent work.
- Windows non-client frame setup is attempted once and suppresses frame/activation paints in restored and maximized states to address the top edge and classic-caption flash. Visual verification is still pending.
- Checks: `cargo check --bin jarvus` passed; `cargo test --bin jarvus` passed **149, 0 failed, 5 ignored**; `git diff --check` passed. `cargo fmt --check` reports broad formatting differences across the dirty source tree. Apple Silicon cross-check could not run because this Windows host lacks the Apple-target C compiler required by aws-lc-sys.

### 2026-11-05 — skills, slash menu, auto-compact, team flow

- **Skills:** `src/skills.rs`. Folders with `SKILL.md` from `~/.jarvus/skills`, `~/.agents/skills`, `~/.claude/skills`; catalog appended to every run (main and workers); `use_skill` / `install_skill` tools (GitHub folder, SKILL.md or repo link, size/path-safe); Settings → Agents, skills & hooks lists, removes (JARVUS-installed only) and installs.
- **Slash menu:** typing `/` shows `/compact`, `/compose`, `/new`, skills and MCP servers; arrows/Tab/Enter/click; hover shows what each does. `/skill-name` injects that skill for the message.
- **Compaction:** setting 15–95% (default 80%); also trims bulky old tool results mid-run once a request passes the threshold; `/compact` summarizes on demand; the transcript marks where.
- **Context panel:** latest-request and cache rows no longer vanish while a request is sent; cumulative cache totals added; solid scroll bar gutter; shows the opened subagent's own context (agents now record usage).
- **Team:** the manager is no longer told to block; worker reports arrive in full (limits raised to 60k chars); a manager turn holds while workers run instead of ending with work in flight. Prompts updated (system section 11, 11b skills, worker prompt).
- Live verified: slash menu, /compose via menu, /compact on the long Discord chat (marker + context drop), two workers with follow-up message from main, per-agent context panel. Unverified: skill install from GitHub on a real link, auto-compact trigger at threshold, mid-run trimming, 15% minimum.
- Checks: `cargo test --bin jarvus` 160 passed, 0 failed, 5 ignored.

### 2026-11-05 (later) — why subagents were useless, queue/steer, line-range file tools

- **Root causes of useless workers (found by reading the JevSnakeBenchmark worker transcripts):** (1) any message that merely contained words like *server*, *running*, *tools* or *MCP* was treated as a connection-status question: `list_mcps` was forced as the first call and its inventory became the worker's whole answer. Now only short, direct questions count, and never for workers. (2) Every tool result was trimmed to 240 characters per string, so file reads and command output reached the model almost empty. Shell, Chrome, MCP, agent and skill results now pass whole up to 60,000 characters; only desktop (JEV) results stay trimmed.
- **Why runs seemed to stop after many tools:** the transcript silently dropped tool calls after 100 (200 for workers), so nothing new appeared. The list is now unbounded in practice (old argument/result text is cleared after 60 calls to keep saved chats small), and a manager no longer waits on a worker that has been silent for 20 minutes.
- **Context gauge** is the service-reported size of the last request plus text added since, updated every frame, so it moves live and differs per agent.
- **File tools:** `read_file` takes `ranges` ("1-40", "300-") and returns numbered lines; `edit_file` takes ordered `edits` of {lines, replacement}, applied atomically against the original numbering (insert with "N-(N-1)", delete with an empty replacement, overlaps rejected, CRLF kept). Legacy find/replace remains.
- **Queue and steer:** typing while a run works queues the message as a dimmed bubble at the bottom right; it sends when the run ends, or **Steer now** hands it to the running agent immediately (shown inline as a note).
- **Transcript:** one-line turn summary ("Ran 4 commands and edited 2 files"; three or more kinds are a comma list), "Working for 1m 12s" while running, commentary between tool rounds is kept with paragraph breaks.
- **Model chip:** keeps the width of its widest thinking level and its popup always opens upward, so changing thinking no longer shifts or flips it.
- **Briefs:** system prompt and `agent_spawn` now require GOAL / CONTEXT / SCOPE / STEPS / CHECKS / DELIVERABLE / LIMITS with a worked example; workers are told to start on the task, not inventory tools.
- Live verified: line-range edit result, queued message auto-send, the new summary and timer, popup stability, workers returning complete function maps. Not reproduced: the transcript overflowing past the window bottom (clipping looked correct in a long chat with expanded tool rows). `cargo test --bin jarvus` 169 passed.

- **Continuity:** a past reply now reaches the model with a digest of the tool calls it made and what they returned (700 chars per call, 16k per reply), so the context no longer drops back to the bare baseline on every new message; the gauge counts the digest.
- **Messages as tool rows:** every message a run receives from the user or another agent shows as "Message from X" in the main and worker tool lists.
- **Core skills installed:** systematic-debugging and brainstorming are built into the app, listed as installed (source core), found by either spelling (`systematic_debugging` too) and shown in the / menu; their text also stays in the base prompt. 171 tests pass.

### 2026-11-05 (night) — macOS completion, bundled skills

- **Why the CI failed:** two compile errors on macOS only: `uia.rs` re-exported a `fallback` module that does not exist on macOS (`cfg(not(windows))` should have been `not(any(windows, macos))`), and `browser.rs` used `PermissionsExt::from_mode` without importing the trait. A local macOS type-check (a copy of the project with reqwest on `rustls-no-provider`, target `aarch64-apple-darwin`, `cargo check --tests`) now reproduces and verifies this without a Mac; it passes.
- **Native macOS layer (`src/macos.rs`):** CoreGraphics events for click, drag, wheel, key combos (ctrl/win/meta/super = Command) and Unicode typing; System Events for front app/window; `open -a` for launching; `AXIsProcessTrusted` check with a clear error and a one-time jump to the Accessibility pane; PATH extended from a login shell at startup (Finder-launched apps get a bare PATH). Accessibility trees and element actions still go through Agent Desktop (AX). `list_processes` uses `ps` on macOS.
- **macOS manual:** `src/system_prompt_macos.md` (about the same size as the Windows-first one) is compiled in on macOS; a test keeps every routing rule in both files and checks each speaks its own platform.
- **Packaging:** `packaging/macos/package.sh` builds `JARVUS.app` (Info.plist, .icns from the 1024 px icon, bundled Agent Desktop CLI + helper, ad hoc or Developer ID signature), a `.zip` and a `.dmg`. The workflow (`.github/workflows/macos.yml`) builds both architectures, runs the tests, uploads artifacts, and attaches the files to a published release (or a tag entered on manual dispatch).
- **Skills:** `install_skill` now downloads the repository tarball (no per-file API rate limit). `taste-skill` (design-taste-frontend) and `unslop` are installed in the background on first launch and stay removed if the user removes them. Skills load from `~/.agents/skills`, `~/.claude/skills` and JARVUS's own `~/.jarvus/skills` (all installs go there).
- **Unverified:** the macOS app has never been run on a Mac (permissions, CoreGraphics input, System Events, bundled Agent Desktop, DMG layout); signing/notarization needs the repository secrets listed in the README.

### 2026-11-06 — chats now continue from the model's own transcript

- **Problem:** every run rebuilt the model's context from the text of past replies (plus a short tool digest), and the `get_tools_` schemas were forgotten, so each new message re-loaded the same MCP tools and the context fell back to the baseline.
- **Fix (the way Codex does it with `store:false`):** each chat saves the Responses API items the model saw (user messages, assistant messages, tool calls and their results, capped at 24k characters per result) plus the tool schemas it loaded, in `~/.jarvus/chats/<id>/model.json`; each subagent keeps its own in memory. The next run sends that transcript again (the prompt cache key keeps the prefix cached) and starts with the loaded tools already available. Reasoning items and per-run developer notes are not kept; calls without results are dropped so the next request is valid. The saved state is only used while it accounts for exactly the visible turns, so an edit or regenerate rebuilds it from the turns; chats from before this change start from the turn text and then continue normally.
- **Compaction** (automatic and `/compact`) cuts the saved transcript at a turn boundary, keeping the last two turns.
- Verified against the live backend: a second run answered a question about the first run's tool call and result without calling any tool (`live_second_run_remembers_the_first_runs_tool_calls`, ignored by default).
