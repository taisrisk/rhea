# Implementation: ChatGPT and JEV

## Current app

JARVUS is a native Rust/eframe desktop app with Windows and macOS source paths. Its main view contains the conversation, composer, context/model controls, persistent history, and a computer-tools view. The UI uses egui/AccessKit for accessible controls. Windows native window chrome is customized and the process is configured as a GUI subsystem so launching the app should not open a console window. macOS builds and runtime permissions still need Mac CI and an interactive Mac check.

## Authentication and model service

- Sign in with the Codex device authorization flow: request a one-time code, open `https://auth.openai.com/codex/device`, poll for approval, exchange the returned authorization code and verifier, then store tokens in the platform's native credential store (Windows Credential Manager or macOS Keychain).
- Store large token sets as bounded credential entries and switch the manifest after the chunks have been written.
- Fetch the account's available model catalog; persist the selected model across restarts.
- Send prompts through the ChatGPT Codex backend route with streaming. Context and cache-read/cache-write numbers are shown only when returned by the service; estimates are labeled separately. The vault does not represent this route as a general public API guarantee.
- The model's function calls are dispatched locally by JEV. Tool results are returned to the model for continued reasoning. Detailed tool events are stored with the conversation and can be expanded in the UI.

## Local desktop tools

Current tools cover process discovery, app launch, fresh desktop/window snapshots, semantic actions against snapshot-bound references, typing, keyboard navigation, and mouse/pointer interaction. Windows uses UI Automation and macOS uses Agent Desktop's AX API. The standard interaction uses accessibility metadata and fresh refs. Agent Desktop snapshot output can be limited to interactive controls with `interactive_only`; `false` (the default) retains the full useful snapshot. Captures and debug maps remain local unless the user deliberately includes content in a prompt.

After an action that can change the interface, JEV should capture a fresh state before acting on an old target. Snapshot and tool results should be summarized to the smallest useful evidence. User-visible tool history retains the full local call/response details for inspection.

## Safety boundaries

The model may plan and request local tools; JEV is the executor. Multi-step plans and measurable research return to ChatGPT after bounded local actions; the model must inspect evidence and continue until the success criteria pass or a blocker is reported. A fresh observation is required before semantic actions. Ask before messaging/posting, deleting, purchases, installs/removals, security/account changes, or shutdown/restart unless the user explicitly asked for that specific operation. Stop if capture or action verification fails rather than guessing.

## Current UI repair record

The active repair addresses: stable transcript geometry between streaming and completed states; copy support through egui's clipboard integration; a green caret and Enter-to-send/Shift+Enter newline; full-window resize hit-testing; scrollable context details; context/model control placement; a smooth radial green glow and visible panel shadow; removal of duplicate main-chat heading and bottom-right connection text; and hidden console startup. The current integrated release build succeeds and its executable is configured as a GUI subsystem. Live-window appearance, edge/corner resizing, copy behavior, and keyboard interaction still need direct runtime verification. The JEV response-time field reports local action time only and should show a neutral readiness state until a local action has been measured.

## MCP, Chrome, and asynchronous titles

- Local stdio, Streamable HTTP, and legacy HTTP+SSE MCP transports, live connect/reload, initialization/tool discovery, and dynamic tool dispatch are implemented in `src/mcp.rs` and Settings. Definitions live in `~/.jarvus/mcp-servers.json`; environment/header secrets use the OS credential service. Mock tests exercise both remote protocols; no external MCP server has been live-connected.
- The Chrome MV3 extension and native messaging host are in `chrome-extension/` and `src/browser.rs`. `src/main.rs` recognizes Chrome's extension-origin invocation. Setup requires an unpacked Chrome install, `JARVUS_CHROME_EXTENSION_ID`, a user-level native host manifest/registry entry, per-site permission grants, and popup connection. See [[Runtime Integrations and Setup]]. These steps and live browser control remain unverified.
- First-turn chat naming runs asynchronously, once per new conversation. JARVUS selects a catalog-listed small model and lowest advertised effort, sends one short prompt with no tools, and falls back to a local title. Title-request state and the result are persisted. This is source/test-verified, not live-service-verified.
- Model tool responses receive compact summaries; expanded per-call activity is retained locally with the chat. Raw desktop snapshots and screenshot pixels are not included in those ordinary summaries.

## Validation record

- Latest `cargo fmt --check`, `cargo check --locked --bin jarvus`, and `git diff --check` passed. `cargo test --locked --bin jarvus` passed 86 tests; four are ignored. The optimized binary built in `target-final/release/jarvus.exe` because the standard release executable was locked by a running instance.
- Source-level and build verification do not establish runtime UI appearance, resizing, keyboard/copy interaction, extension installation, browser control, or a live authorized ChatGPT request.
- Live UI inspection, resizing, keyboard behavior, copy behavior, and a real authorized ChatGPT request are separate checks. Do not infer them from a successful compile.
- No credentials, access tokens, or one-time device codes belong in the vault.
- The macOS cross-check could not compile the application on this Windows host because `aws-lc-sys` needs a Mac C toolchain. macOS CI and actual Mac Accessibility/Keychain/browser workflows remain pending.
- A fresh screenshot of the latest UI could not be obtained: capture timed out twice and the second Computer Use attempt was stopped with Escape. This does not prove rendered layout, console visibility, or live chat/desktop interaction.

## References

- [OpenAI Codex device-code implementation](https://github.com/openai/codex/blob/main/codex-rs/login/src/device_code_auth.rs)
- [OpenAI Codex device-login command](https://github.com/openai/codex/blob/main/codex-rs/cli/src/login.rs)
- [[ChatGPT OAuth and Runtime]]
- [[Current Status and Known Issues]]
