---
title: ChatGPT OAuth and Runtime
type: integration-design
status: implemented-unverified
tags: [jarvus, openai, oauth]
---

# ChatGPT OAuth and Runtime

## Current code path

`src/auth.rs` uses the Codex device authorization flow. JARVUS requests a one-time device code, opens `https://auth.openai.com/codex/device`, polls for approval, exchanges the returned authorization data, and stores token material in the platform's native credential service: Windows Credential Manager or macOS Keychain Services. Do not store device codes, tokens, or browser callback values in the vault.

`src/chat.rs` currently sends model and response requests to the ChatGPT Codex backend route. This code path is account/session dependent; it is not documented here as a general public API guarantee. Preserve the user's requested device-code sign-in while tracking provider errors as runtime evidence.

## Request and tool flow

1. User completes authorization in their browser using the displayed device code.
2. JARVUS loads the saved credentials and fetches the account model catalog.
3. The selected model receives the conversation and tool schema.
4. A returned function call dispatches to JEV locally.
5. JEV returns a bounded tool result; the model can continue with another tool call or a final response.
6. JARVUS saves the conversation, tool events, selected model, and available usage metadata locally.

Each conversation has its own `metadata.json`, `history.json`, and `toolcalls.json`; transcript contents are loaded when opened and unloaded after five seconds inactive. For research and multi-step tasks, each bounded JEV run is a checkpoint. ChatGPT must inspect its evidence, continue with more tools, and test the user's criteria before claiming success. A plan statement, a local `completed` field, or one observation does not end the overall task.

The UI should distinguish service-reported context/cache counts from estimates. The app must not claim a cache hit unless the response usage metadata reports it. JEV action timing is local-only and must not include the model request duration.

## Failure handling

- Treat empty completed responses as a provider/runtime error, not a successful answer. Preserve useful response status and error details without logging access tokens.
- If the model catalog is empty, show a clear status and allow refresh/reconnect.
- If the device code is pending, keep the code copyable and explain the next step; do not treat browser launch alone as successful sign-in.
- Do not silently switch to API-key billing or another provider.
- A live response is verified only by an authorized request returning visible assistant text in the rebuilt app.

## Current verification

Source confirms the device authorization endpoints and Codex backend route. Current checks: `cargo fmt --check`, `cargo check --locked --bin jarvus`, and `git diff --check` passed; `cargo test --locked --bin jarvus` passed 86 tests with four ignored. A release build succeeded in `target-final/release/jarvus.exe` after the standard release file was found locked by a running instance. The latest app screenshot timed out twice and the second Computer Use attempt was stopped with Escape, so visual appearance, account state, current model list, and live response remain unverified.

See [[Implementation - ChatGPT and JEV]], [[Privacy and Security]], and [[Current Status and Known Issues]].
