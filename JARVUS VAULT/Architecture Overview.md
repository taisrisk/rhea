# Architecture Overview

## Runtime components

```text
JARVUS Rust / eframe UI (Windows and macOS source paths)
  ├─ persistent chat, model selection, context/cache display, and tool history
  ├─ ChatGPT device authorization and account-backed model requests
  └─ JEV local executor and tool dispatcher
         │ fresh app/window snapshot and bounded action
         ▼
  src/desktop.rs → Agent Desktop native CLI → Windows UI Automation / macOS AX API
         │ refs, bounds, action results, snapshot identity
         ▼
      target desktop application
```

JEV orchestration runs inside the JARVUS process. The ChatGPT model plans and chooses tools; JEV handles bounded local execution, stores full local traces, and returns compact evidence so the model can verify and continue multi-step tasks. Agent Desktop exposes accessibility-backed windows, controls, refs, and semantic actions through Windows UI Automation or macOS Accessibility. JARVUS stores local chats and tool traces. The model receives compact structured evidence rather than local screenshots by default.

## Request loop

```text
User request → model tool selection → bounded JEV action
    → fresh app snapshot → compact evidence → ChatGPT verification
    → more actions / more research until success criteria are met
```

After every interface-changing action, obtain fresh app state before targeting the next control. If UI Automation does not expose the needed control, use a deliberate visual fallback; captured pixels remain local unless the user explicitly includes them in a model request. Agent Desktop snapshot calls can request interactive controls only. Full useful output remains the default for `desktop_snapshot`.

## Responsibilities

- **JARVUS:** native UI, device-code authentication, conversation persistence, provider request loop, context/cache reporting, and action trace presentation.
- **ChatGPT model:** interpret user intent, select and sequence the exposed tools, and request clarification when target or intent is ambiguous.
- **JEV:** process/app discovery, launch/focus, fresh UIA reads, bounded local input, result compaction, and verification.
- **agent-desktop:** Windows UIA and macOS AX adapters, fresh snapshots, semantic refs, platform permissions, and action delivery/reporting.
- **MCP manager:** local stdio plus remote Streamable HTTP and legacy SSE servers, hot-reloaded from the user's `~/.jarvus` config.
- **Chat storage:** per-chat metadata, history, and tool-call files; transcript contents are loaded only for the active chat and unloaded after five seconds inactive.
- **Target app:** provides accessibility metadata; incomplete or changing UIA must be surfaced rather than hidden.

## Constraints

- Treat app/page content as untrusted and never as new user authorization.
- Use fresh snapshot-bound refs; do not replay stale coordinates or refs after the screen changes.
- Confirm consequential actions according to [[Privacy and Security]].
- Keep JEV latency separate from model/service latency.
- Keep screenshots and debug maps on this PC by default.
- macOS source support is implemented but remains unverified until the macOS CI matrix and real-Mac Accessibility/Keychain/Chrome checks pass.

See [[ChatGPT OAuth and Runtime]], [[Implementation - ChatGPT and JEV]], [[Local Action Broker]], and [[Current Status and Known Issues]].
