---
title: Local Action Broker
type: technical-design
status: proposed
tags: [rhea, actions, safety]
---

# Local Action Broker

## Purpose

The broker is the only component allowed to turn assistant proposals into local PC input. The model can request a typed action; the broker validates, authorizes, executes, and verifies it.

## Initial action vocabulary

- `launch_app`: open an app from a user-managed allowlist.
- `focus_window`: focus a currently detected window by ID.
- `click_element`: click a current screen-map element ID; coordinate click is a guarded fallback.
- `type_text`: type text into a user-approved target; show the text before submission when it may be sent outside the PC.
- `press_key`: invoke a short allowlisted keyboard action (for example Enter, Escape, or Tab).
- `scroll_region`: scroll a current region by a bounded amount.
- `wait_for_change`: pause briefly and request a fresh screen map.
- `stop_task`: cancel the current plan.

Do not expose unrestricted shell, arbitrary PowerShell, or arbitrary code execution as a desktop action.

## Validation

For each proposal, verify:

1. Action name is in the registered schema.
2. Required fields are present and have expected types.
3. Target ID belongs to the newest valid screen map and is not expired.
4. Coordinates are within the current display and not a stale frame.
5. The action has an explicit maximum duration/amount where applicable.
6. The action is permitted by the current safety rule and user settings.

## Confirmation tiers

- **Low impact:** opening an allowlisted app, focusing a window, or navigating a visible UI may execute within the current user task.
- **Review required:** pasting text into a form, changing application settings, or triggering an external workflow should show the exact target and proposed content first.
- **Explicit confirmation:** sending messages, publishing, purchases, deletion, security changes, account changes, or other consequential actions.
- **Blocked:** actions outside configured scope, stale targets, unknown destinations, or requests that require a prohibited live-game automation context.

The first three tiers are policy proposals and should be confirmed against actual use before coding.

## Execution cycle

`Plan -> validate -> (confirm if needed) -> execute one action -> capture -> detect -> compare expected state -> continue or stop`

The broker must stop on unexpected windows, low detector confidence, failed capture, or user cancellation. It must not silently retry high-impact actions.

## Audit record

Keep a local task log with timestamp, action name, target ID, confirmation state, and observed result. Avoid storing screenshots or sensitive typed text unless the user explicitly enables a debugging capture.

## Related notes

- [[Architecture Overview]]
- [[Custom Screen Pipeline]]
- [[Screen Map JSON Schema]]
- [[Privacy and Security]]
