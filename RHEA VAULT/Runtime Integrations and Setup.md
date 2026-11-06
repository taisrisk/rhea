---
title: Runtime Integrations and Setup
type: technical-reference
status: implemented-unverified
tags: [rhea, mcp, chrome, runtime, setup]
---

# Runtime Integrations and Setup

This note describes behavior present in the current source tree. A successful build or unit test is not evidence that an external server, signed-in account, browser extension, or real desktop action works end to end.

## ChatGPT sign-in, model calls, and chat naming

- `src/auth.rs` uses OpenAI's Codex device authorization endpoints and opens `https://auth.openai.com/codex/device`. Credentials use the native platform credential store (Windows Credential Manager on Windows; Keychain Services on macOS). Never add tokens, device codes, or callback data to this vault.
- `src/chat.rs` fetches the account model catalog from `chatgpt.com/backend-api/codex/models` and streams Responses from `chatgpt.com/backend-api/codex/responses`. This is the ChatGPT Codex backend route used by this source; it is not documented here as a general public API contract or as support for every ChatGPT product operation.
- Tool calls are dispatched locally to Rhea, a connected MCP stdio process, or the Chrome bridge. Desktop snapshots and screenshots are retained locally; bounded summaries go into the model's function result. A full tool activity record is stored with the conversation and can be expanded in the chat UI.
- Conversation title generation is asynchronous and issued once on the first user turn of a new chat. Rhea chooses a model explicitly advertised as small (nano before mini) and its lowest advertised reasoning effort. The request has one short user input and no tools. If no qualifying model or title result is available, Rhea uses a local title derived from the prompt. The generated title and request-issued flag are persisted with the conversation.

## MCP server configuration and connection

Rhea implements local stdio, MCP Streamable HTTP, and legacy HTTP+SSE transports.

- Settings → MCP servers accepts standard client configs (`mcpServers`, plus common editor `servers` configs). Definitions are merged and connected immediately; editing, enable/disable, reconnect, and reload refresh the live manager.
- The active definition file is `~/.rhea/mcp-servers.json` on Windows and macOS. The app migrates the legacy Windows `%APPDATA%\Rhea\mcp-servers.json` file when present.
- Stdio uses newline-delimited JSON-RPC. Remote servers use Streamable HTTP or legacy SSE with bounded response/request sizes and timeouts; HTTPS is required except loopback HTTP. Redirects and unsafe cross-origin SSE endpoints are rejected.
- Configured environment/header secrets are omitted from JSON and stored in the OS credential service. Remote endpoint labels omit URL query/fragment values. Do not paste credentials into public notes or screenshots.
- Mock-server tests cover Streamable HTTP and legacy SSE initialization, tool listing, and calls. A live external MCP endpoint has not been tested.

## Settings and local storage inventory

- Settings has ChatGPT, Usage, Connections, MCP, Browser, and Storage sections.
- Connections can refresh local desktop bridge status, connect/disconnect configured MCP stdio servers, and inspect/revoke the current Chrome relay session. It cannot enumerate ChatGPT account scopes or Chrome's individual host grants; those remain in their respective account/browser settings.
- Each chat is stored in `~/.rhea/chats/<id>/metadata.json`, `history.json`, and `toolcalls.json`. Startup loads chat metadata for the history list; a transcript is loaded when opened and released after five seconds inactive.
- Settings shows app data and a top-level Downloads inventory, can open listed items, and requests confirmation before deleting a listed download.
- No project/file connector or generated-artifact catalog is available. Rhea cannot claim a full inventory or safe cleanup of every project, Downloads subfolder, or generated file.

## Chrome Manifest V3 and native messaging setup

The extension source is in `chrome-extension/`; the complete, current install/update/removal instructions and protocol details are in its [README](../chrome-extension/README.md).

1. Build Rhea and load `chrome-extension/` as an unpacked extension at `chrome://extensions` with Developer mode enabled.
2. Configure Chrome's exact extension ID, then restart Chrome.
3. Rhea writes a per-user native messaging manifest under the platform's user data directory (`~/.rhea`); Windows uses the Chrome registry entry and macOS uses Chrome's NativeMessagingHosts directory.
4. Grant access to each intended HTTP/HTTPS site in the extension popup and choose Connect to Rhea.

Chrome launches `rhea.exe` and supplies the caller origin. `src/main.rs` routes that origin to `browser::run_native_messaging_host()`; a wrapper may use `--native-messaging-host <origin>`. Chrome owns the native port, so Rhea must not spawn a second native host. The host validates the extension ID and creates an authenticated loopback relay. Tools expose page snapshot, click, type, and navigation; actions require a fresh snapshot identity, and sensitive submit/link actions have additional confirmation checks.

This repository contains the extension and native-host implementation, and the Rust entry point is wired. Installation in Chrome, native host registration, site permission grants, and end-to-end browser interaction have **not** been verified in a live browser on this pass.

## Agent Desktop process

Rhea invokes the installed Agent Desktop executable as a separate child process and reads its structured output. Windows uses `CREATE_NO_WINDOW`; macOS uses Agent Desktop's native AX accessibility backend and documented permission commands. Rhea does not manipulate TCC permissions or implement a duplicate accessibility engine. The third-party process remains separate from Rhea. No fresh Mac runtime check was available.

## Verification status (2026-11-04)

- `cargo fmt --check`: **passed**.
- `cargo test --locked --bin rhea`: **86 passed, 0 failed, 4 ignored**.
- `cargo check --locked --bin rhea`: **passed**; `git diff --check` passed.
- `cargo build --release --locked --bin rhea` compiled but could not replace the standard output because Windows reported `Access is denied` for a running `target/release/rhea.exe`.
- The same optimized build succeeded in the ignored `target-final/release/rhea.exe` directory. Screenshot capture of the running window timed out twice; the second Computer Use attempt was stopped by Escape, so there is no pixel-level confirmation of the latest UI.
- A macOS target check stopped in `aws-lc-sys` because this Windows host has no macOS C compiler. The macOS Actions workflow is present, but Mac CI/runtime have not run from this checkout.
- The browser/MCP implementation pass reported `node --check` passing for the extension JavaScript files. These are local implementation checks.
- No live extension install, Chrome native messaging session, authorized site workflow, signed-in ChatGPT response, remote MCP connection, or third-party desktop action was performed as part of these checks.
- Fresh accessibility inspection confirmed controls and eight resize handles, but screenshot capture timed out. This does not prove visual correctness or actual resize/copy behavior.

See [[ChatGPT OAuth and Runtime]], [[Implementation - ChatGPT and Rhea]], [[Current Status and Known Issues]], and [[Privacy and Security]].
