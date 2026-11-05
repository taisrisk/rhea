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
