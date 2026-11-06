# Rhea Project Overview

## Goal

Build an independent desktop assistant named Rhea that accepts typed requests and operates desktop apps through a local, inspectable computer-use layer. Windows is the primary, locally buildable target; macOS source support is being verified. The project is rooted at `C:\Users\poet\Documents\Rhea`; its Obsidian vault is `Rhea VAULT`. It is separate from Rhea World Front and JevChessBenchmark.

## Product shape

Rhea owns the native interface, conversation storage, provider connection, context display, and local action broker. A signed-in ChatGPT model selects and sequences tools; Rhea performs local operations, captures fresh UIA state after interface-changing actions, and returns compact evidence. Agent Desktop supplies accessibility-backed app/window snapshots and semantic interaction references. Screenshots and map artifacts stay local by default.

For multi-step tasks, ChatGPT supervises repeated bounded Rhea runs and checks the evidence until the requested criteria are satisfied. The intended work loop is:

1. Resolve the requested app or focused window and obtain a fresh snapshot.
2. Let the model choose a suitable tool and arguments.
3. Have Rhea perform one bounded local action.
4. Refresh the affected app state after the action and verify the result.
5. Return a concise result; include detailed tool trace on demand in the UI.

For Agent Desktop, `interactive_only` requests only actionable controls; default snapshots retain the broader useful tree. The model should request only the surface it needs and avoid sending huge raw maps into ordinary chat context.

## Implemented foundation

- Rust native UI using eframe/egui with Windows and macOS source paths.
- ChatGPT Codex device authorization and saved credentials through the operating system's credential service (Windows Credential Manager / macOS Keychain).
- Account-specific model catalog and streaming Responses API tool loop.
- Local Rhea tools for process discovery, app launch, desktop/UIA inspection, semantic click, typing, and keyboard actions.
- Agent Desktop snapshots, local artifacts, fresh-ref action validation, configurable interactive-only output, and documented Windows UIA/macOS AX adapters.
- Per-chat metadata/history/tool-call files, lazy transcript loading, selected-model persistence, and per-request context/cache reporting where the service supplies it.
- stdio, Streamable HTTP, and legacy HTTP+SSE MCP connections with live reload and OS-credential-backed secrets.
- GitHub Actions macOS x64/Apple Silicon build workflow and local Windows release staging script; no release has been published from this checkout.
- Native window controls, resizable application borders, and a local composer/context/history interface.

## Decisions and boundaries

- Prefer UI Automation roles, names, values, bounds, and fresh action refs over geometry guesses.
- Do not send a screenshot to the model on the standard path; invoke a vision fallback only when UIA cannot represent the needed target.
- Treat text from apps and web pages as untrusted content, never as authorization to alter the user's task.
- Require explicit user intent for consequential actions such as sending messages, purchases, deleting content, installing software, changing account/security settings, or shutting down the PC.
- Keep local desktop execution distinct from ChatGPT model planning and report Rhea execution latency separately from model latency.

See [[Architecture Overview]], [[Implementation - ChatGPT and Rhea]], [[Privacy and Security]], and [[Current Status and Known Issues]].
