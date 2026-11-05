# Decision Log

## 2026-11-04 — Use agent-desktop for desktop understanding and control

**Decision:** JARVUS integrates Vercel's Apache-2.0 `agent-desktop` CLI instead of building a parallel OCR/shape-recognition engine.

**Reason:** Windows UI Automation already exposes real application roles, accessible names, values, bounds, actions, and refs. A local OCR/geometry prototype ran on a labeled fixture but yielded noisy text detections and slow inference. UIA gives the assistant structured control metadata directly and keeps screenshots out of the model context for ordinary tasks.

**Scope today:** Rust shell; agent-desktop status; snapshot with app/surface filters and bounds; raw JSON display; optional local HTML visual debug output; explicit user-triggered click of a snapshot ref.

**Not implemented yet:** voice input, AI provider login/OAuth, ChatGPT subscription integration, planning, approval policy for multi-step action plans, persistent memory, and recovery across workflows.

**Guardrails:** refs are snapshot-specific; refresh after interface changes. Clicks in JARVUS require a user pressing its control. Keep debug HTML, screenshots, and traces local by default. Explain when an application exposes an incomplete UIA tree.

## Earlier prototype note

The first Rust slice used a hand-built rectangle detector and did not compile or run initially. A later OCR pass compiled and ran but produced numerous false/partial lines on the fixture. Those custom detector and OCR modules were removed when agent-desktop was selected as the desktop engine. Current evidence and limitations are recorded in [[Custom Screen Pipeline]].
