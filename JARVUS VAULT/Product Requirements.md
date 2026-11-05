# Product Requirements

## Product statement

JARVUS is a Windows assistant the user can speak or type to. It should understand intent, operate desktop apps through real accessibility controls, and verify each step.

## Functional requirements

### Assistant input

- Support typed requests and later push-to-talk voice input.
- Show the recognized request before running actions when voice transcription confidence is uncertain.
- Keep conversation and workflow state local unless the selected AI provider receives data for a request.

### Desktop understanding

- Integrate Vercel `agent-desktop` for Windows UI Automation.
- Read accessible roles, names/values, measured bounds, actions, and snapshot-qualified refs.
- Let the user select an app/window/surface and inspect the current tree.
- Explain when UIA data is missing, incomplete, stale, or ambiguous.
- Offer an explicit vision fallback only when accessibility metadata cannot answer the task.

### Desktop actions

- Support launching apps, navigation, field entry, clicking, scrolling, menus, and multi-step workflows as the broker grows.
- Use only refs from the latest snapshot and refresh after the interface changes.
- Verify the result after each step; do not replay actions with uncertain delivery.
- Keep an immediate stop/cancel control available during a workflow.
- Show a reviewable plan before multi-step work and before consequential actions.

### Model and account integration

- Use a provider abstraction rather than baking one service into the action broker.
- Investigate official ChatGPT account authorization and subscription entitlements before implementation. Do not claim ChatGPT OAuth grants subscription access unless an official supported flow confirms it.
- For the usual desktop path, send structured UIA metadata rather than screenshot pixels. If a vision request is needed, show the destination and content clearly.

## Current vertical slice

- Rust/eframe desktop shell.
- agent-desktop connection check.
- UIA snapshot with app/surface filters, bounds, refs, and JSON display.
- Optional local HTML accessibility visualization.
- User-triggered click of a ref.

Voice, AI, plan review, app picker, searchable tree, and multi-step orchestration remain future work. See [[Roadmap]].
